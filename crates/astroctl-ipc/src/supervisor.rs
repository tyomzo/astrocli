//! Spawn, handshake, health, restart and the job queue (SDD §5.12.2, §5.12.3).
//!
//! ```text
//! submit() ─► queue ─► spawn(python_interpreter, compute_worker.py) ─► handshake ─► ready
//!                          │                                                        │
//!                          ├─ Ping every `workers.health_ping_seconds`              │
//!                          │     3 consecutive missed Pongs ──► SIGKILL ────────────┤
//!                          ├─ child exit (any cause) ──────────────────────────────►┤
//!                          └─ job past `workers.job_timeout_seconds` ─► Cancel ─────┘
//!                                                                       then kill
//!                                                                       ▼
//!                                       restart, capped exponential backoff, 60 s ceiling
//! ```
//!
//! # Two decisions this module makes that SDD §5.12.3 does not state
//!
//! **Workers start on demand, not at boot.** `astroctl-stack`'s startup sequence says so
//! explicitly and it is the same principle as the field node not connecting hardware at
//! startup: a stacking server with no session running should not be holding a Python process
//! and a GPU context. [`spawn`] therefore starts nothing; the first [`WorkerHandle::submit`]
//! brings a worker up, and it then stays up.
//!
//! **A job that timed out is not retried; a job whose worker died is.** §5.12.3's retry-once
//! rule is written for a worker that dies mid-job, where the work is simply lost. A job that
//! ran past `job_timeout_seconds` is not lost — it is too slow, and running it again buys
//! another `job_timeout_seconds` of an observing night for the same answer.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use astroctl_core::bus::EventBus;
use astroctl_core::config::WorkersConfig;
use astroctl_core::error::ErrorCode;
use astroctl_core::event::{Alert, WorkerState};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, sleep, sleep_until, timeout, timeout_at};
use tokio::time::{Duration, Instant, MissedTickBehavior};

use crate::protocol::{
    decode_from_worker_bytes, FromWorker, JobId, JobKind, Nonce, ProtocolError, ToWorker,
    WorkerCaps, WorkerError, MAX_LINE_BYTES, PROTO_VERSION,
};

/// The `worker` field SDD §5.12.1 wants on every line of captured worker output.
const WORKER: &str = "compute";

/// A worker that produces no `hello` within this is killed and treated as a failed start
/// (SDD §5.12.2).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Consecutive unanswered pings before SIGKILL (SDD §5.12.3).
pub const MISSED_PONGS_BEFORE_KILL: u32 = 3;

/// Ceiling of the capped exponential restart backoff (SDD §5.12.3).
pub const RESTART_BACKOFF_CEILING: Duration = Duration::from_secs(60);

/// How long a worker gets to honour a `cancel` before it is killed instead.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// How long a `shutdown` plus EOF gets before the worker is killed instead.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long we wait to reap a killed child before giving up and logging it.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Floor on the write budget, for deployments configuring a one-second ping.
const MIN_WRITE_BUDGET: Duration = Duration::from_secs(5);

/// Queued submissions before [`WorkerHandle::submit`] starts applying backpressure.
const JOB_QUEUE_DEPTH: usize = 32;

/// Decoded frames buffered between the reader task and the supervisor loop.
const FRAME_QUEUE_DEPTH: usize = 64;

/// Attempts a single job gets across worker deaths: the original and one retry (SDD §5.12.3).
const MAX_JOB_ATTEMPTS: u32 = 2;

/// `alert` code for a worker built against a different protocol version (SDD §5.12.2).
pub const ALERT_PROTO_MISMATCH: &str = "WORKER_PROTO_MISMATCH";

/// `alert` code for a worker that cannot be started at all — bad interpreter or script path.
pub const ALERT_UNAVAILABLE: &str = "WORKER_UNAVAILABLE";

/// `alert` code for a restart. Warning, not critical: one restart is survivable, and it is the
/// *count* in [`WorkerStatus::restarts`] that reveals the failure §5.12.3 cares about.
pub const ALERT_RESTARTED: &str = "WORKER_RESTARTED";

/// `alert` code for a job that exhausted [`MAX_JOB_ATTEMPTS`].
pub const ALERT_JOB_FAILED: &str = "WORKER_JOB_FAILED";

/// Decoded frames on their way from the reader task to the supervisor loop.
type Frames = mpsc::Receiver<Result<FromWorker, ProtocolError>>;

// ---------------------------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------------------------

/// What the supervisor knows about its worker, for `/api/system/health` and `stack.status`.
///
/// `restarts` is the number §5.12.3 singles out: "a worker quietly restarting every few minutes
/// is the failure mode most likely to go unnoticed".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerStatus {
    /// [`WorkerState::Stopped`] until the first job brings a worker up — see the module docs on
    /// on-demand start. The variant was added to the event schema for exactly this field
    /// (2026-07-30): the earlier `Option<WorkerState>` carried "not started yet" as `None`,
    /// which collided with `StackStatus`'s use of `null` for "stack unreachable, unknown" —
    /// two different absences sharing one spelling.
    pub state: WorkerState,
    /// Worker restarts since this supervisor began.
    pub restarts: u32,
    /// Jobs that returned a successful result.
    pub jobs_completed: u64,
    /// Jobs that failed, for any reason including exhausted retries.
    pub jobs_failed: u64,
}

/// Why a submitted job did not produce an answer.
#[derive(Debug, thiserror::Error)]
pub enum JobFailure {
    /// The worker ran the job and reported a failure. The only variant carrying the worker's
    /// own diagnosis.
    #[error("{0}")]
    Worker(WorkerError),

    /// No worker could be started, or the one that existed will not be restarted.
    #[error("the compute worker is unavailable: {0}")]
    Unavailable(String),

    /// The job outlived `workers.job_timeout_seconds`.
    #[error("the compute job exceeded workers.job_timeout_seconds ({0:?})")]
    TimedOut(Duration),

    /// The worker died running this job, on every attempt it was given.
    #[error("the compute worker died on {attempts} attempts at this job: {reason}")]
    Crashed {
        /// How many worker processes this job took down.
        attempts: u32,
        /// How the last one went.
        reason: String,
    },

    /// The supervisor is gone — the process is shutting down.
    #[error("the worker supervisor has stopped")]
    Stopped,
}

impl JobFailure {
    /// The closed §4.2 code this failure reaches the API as.
    ///
    /// §4.2 gained the worker vocabulary for exactly this mapping (change note 2026-07-30):
    /// before that, everything the supervisor produced collapsed onto `INTERNAL`, and "the
    /// stacking software has a bug" and "the Python worker crashed and is restarting" send the
    /// operator to entirely different places. [`JobFailure::Worker`] still carries the worker's
    /// own code, because only the worker knows whether it was handed a missing file or a full
    /// disk. `Stopped` is a cancellation, not a failure: the job was surrendered to shutdown.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            JobFailure::Worker(error) => error.code,
            JobFailure::Unavailable(_) => ErrorCode::WorkerUnavailable,
            JobFailure::TimedOut(_) => ErrorCode::WorkerTimeout,
            JobFailure::Crashed { .. } => ErrorCode::WorkerCrashed,
            JobFailure::Stopped => ErrorCode::Cancelled,
        }
    }
}

/// The backbone's end of the worker channel: submit a job, await its result, read the health
/// counters. Cheap to clone; the supervisor stops when the last clone is dropped.
#[derive(Debug, Clone)]
pub struct WorkerHandle {
    jobs: mpsc::Sender<Envelope>,
    status: watch::Receiver<WorkerStatus>,
}

impl WorkerHandle {
    /// Queue a job and wait for its result.
    ///
    /// The first call is what starts a worker process. `paths` names frames on the stacking
    /// server's own filesystem — this channel never carries pixels.
    ///
    /// # Errors
    /// [`JobFailure`], which distinguishes a failure the worker diagnosed from one the
    /// supervisor imposed.
    pub async fn submit(
        &self,
        kind: JobKind,
        params: Value,
        paths: Vec<PathBuf>,
    ) -> Result<Value, JobFailure> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Envelope {
                kind,
                params,
                paths,
                reply,
            })
            .await
            .map_err(|_| JobFailure::Stopped)?;
        answer.await.map_err(|_| JobFailure::Stopped)?
    }

    /// Current worker state and counters.
    #[must_use]
    pub fn status(&self) -> WorkerStatus {
        *self.status.borrow()
    }
}

/// Start supervising the compute worker described by `config`.
///
/// Returns immediately and spawns **no process**: SDD §5.12.3 supervises workers as on-demand
/// children, so the first [`WorkerHandle::submit`] is what brings one up. Alerts are published
/// on `bus` (ADD §5.6 rule 2 — this crate reports, it does not reach into the binary).
///
/// # Panics
/// If called outside a Tokio runtime, like any other `tokio::spawn`.
#[must_use]
pub fn spawn(config: &WorkersConfig, bus: &EventBus) -> WorkerHandle {
    let (jobs_tx, jobs_rx) = mpsc::channel(JOB_QUEUE_DEPTH);
    let (status_tx, status_rx) = watch::channel(WorkerStatus::default());
    let settings = Settings::from_config(config);
    let bus = bus.clone();

    tokio::spawn(async move {
        supervise(settings, bus, jobs_rx, status_tx).await;
    });

    WorkerHandle {
        jobs: jobs_tx,
        status: status_rx,
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// One submitted job and the caller waiting on it.
#[derive(Debug)]
struct Envelope {
    kind: JobKind,
    params: Value,
    paths: Vec<PathBuf>,
    reply: oneshot::Sender<Result<Value, JobFailure>>,
}

/// A job in the supervisor's hands, carrying the attempt count the retry-once rule needs.
#[derive(Debug)]
struct Pending {
    envelope: Envelope,
    attempts: u32,
}

impl Pending {
    fn new(envelope: Envelope) -> Self {
        Self {
            envelope,
            attempts: 0,
        }
    }

    /// Answer the caller. The `let _` is deliberate everywhere: a caller that gave up and
    /// dropped its future is not an error the supervisor should react to.
    fn succeed(self, data: Value) {
        let _ = self.envelope.reply.send(Ok(data));
    }

    fn fail(self, failure: JobFailure) {
        let _ = self.envelope.reply.send(Err(failure));
    }
}

/// The job currently on the worker.
#[derive(Debug)]
struct Live {
    pending: Pending,
    id: JobId,
    /// When to act: the job timeout first, then the cancel grace.
    deadline: Instant,
    /// Whether `cancel` has already been sent for this job.
    cancelled: bool,
}

/// The parts of `workers:` this module needs, resolved once.
#[derive(Debug, Clone)]
struct Settings {
    interpreter: PathBuf,
    script: PathBuf,
    ping: Duration,
    backoff_base: Duration,
    job_timeout: Duration,
}

impl Settings {
    fn from_config(config: &WorkersConfig) -> Self {
        Self {
            interpreter: config.python_interpreter.clone(),
            // PRD §8.2 ships `compute_worker` as a relative path, so it resolves against the
            // working directory. Resolving it here means a wrong path is reported absolute,
            // which is the difference between a one-line fix and an argument about which
            // directory the service was started from.
            script: absolutize(&config.compute_worker),
            // `interval()` panics on a zero period. The config loader's range check makes zero
            // unreachable through YAML, but a `WorkersConfig` built in code has no such gate,
            // and the failure mode is the whole stacking server panicking on a restart.
            ping: Duration::from_secs(config.health_ping_seconds.max(1)),
            backoff_base: Duration::from_secs(config.restart_backoff_seconds.max(1)),
            job_timeout: Duration::from_secs(config.job_timeout_seconds.max(1)),
        }
    }

    /// How long a write to the worker's stdin may take before the worker counts as wedged.
    fn write_budget(&self) -> Duration {
        self.ping.max(MIN_WRITE_BUDGET)
    }
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}

/// Shared, mutable supervision context. Bundled so the loop's helpers stay under the argument
/// count clippy tolerates, and so `jobs` can be borrowed independently inside `select!`.
struct Ctx<'a> {
    settings: &'a Settings,
    bus: &'a EventBus,
    status: &'a watch::Sender<WorkerStatus>,
    stats: &'a mut WorkerStatus,
}

impl Ctx<'_> {
    fn set_state(&mut self, state: WorkerState) {
        self.stats.state = state;
        // `send` fails only when nobody is listening, which is normal.
        let _ = self.status.send(*self.stats);
    }

    fn completed(&mut self) {
        self.stats.jobs_completed = self.stats.jobs_completed.saturating_add(1);
        let _ = self.status.send(*self.stats);
    }

    fn failed(&mut self) {
        self.stats.jobs_failed = self.stats.jobs_failed.saturating_add(1);
        let _ = self.status.send(*self.stats);
    }

    /// Give a job whose worker died one more worker, or fail it (SDD §5.12.3).
    fn retry_or_fail(&mut self, mut pending: Pending, reason: &str) -> Option<Pending> {
        pending.attempts += 1;
        let attempts = pending.attempts;
        if attempts < MAX_JOB_ATTEMPTS {
            tracing::warn!(
                worker = WORKER,
                attempt = attempts,
                %reason,
                "retrying the in-flight compute job on a fresh worker"
            );
            return Some(pending);
        }
        // "A job that reliably kills its worker must not become a restart loop": the job is
        // failed here, and because workers start on demand the dead one is not replaced until
        // some *other* job arrives.
        tracing::error!(worker = WORKER, attempts, %reason, "compute job failed after its retry");
        self.bus.publish(Alert::critical(
            ALERT_JOB_FAILED,
            format!("A compute job failed after {attempts} attempts: {reason}."),
        ));
        self.failed();
        pending.fail(JobFailure::Crashed {
            attempts,
            reason: reason.to_owned(),
        });
        None
    }
}

/// How a worker session ended.
enum Outcome {
    /// Every [`WorkerHandle`] was dropped: stop supervising.
    Closed,
    /// This worker will never work: do not restart it.
    Fatal { reason: String },
    /// The worker died; restart after a backoff, carrying a retryable job if there is one.
    Died {
        reason: String,
        carry: Option<Pending>,
    },
}

async fn supervise(
    settings: Settings,
    bus: EventBus,
    mut jobs: mpsc::Receiver<Envelope>,
    status: watch::Sender<WorkerStatus>,
) {
    let mut stats = WorkerStatus::default();
    let mut backoff = settings.backoff_base;
    let mut carry: Option<Pending> = None;

    loop {
        // On-demand start: with nothing to compute there is no reason to hold a Python process.
        let pending = match carry.take() {
            Some(pending) => pending,
            None => match jobs.recv().await {
                Some(envelope) => Pending::new(envelope),
                None => break,
            },
        };

        let mut ctx = Ctx {
            settings: &settings,
            bus: &bus,
            status: &status,
            stats: &mut stats,
        };
        ctx.set_state(WorkerState::Starting);

        let started = Instant::now();
        let outcome = session(&mut ctx, &mut jobs, pending).await;

        match outcome {
            Outcome::Closed => break,
            Outcome::Fatal { reason } => {
                ctx.set_state(WorkerState::Failed);
                // Keep answering submissions with the reason rather than dropping the channel:
                // "the worker speaks protocol v2, this backbone speaks v1" is actionable and
                // "the supervisor has stopped" is not.
                refuse_forever(&mut jobs, &reason).await;
                break;
            }
            Outcome::Died {
                reason,
                carry: next,
            } => {
                ctx.stats.restarts = ctx.stats.restarts.saturating_add(1);
                let restarts = ctx.stats.restarts;
                ctx.set_state(WorkerState::Restarting);
                tracing::warn!(worker = WORKER, restarts, %reason, "compute worker died");
                ctx.bus.publish(Alert::warning(
                    ALERT_RESTARTED,
                    format!(
                        "The compute worker restarted ({reason}). Restarts so far: {restarts}."
                    ),
                ));

                // A worker that ran longer than the ceiling was not crash-looping, so the next
                // failure starts from the base again. Without this the backoff ratchets to 60 s
                // permanently after one bad night and never comes back.
                if started.elapsed() >= RESTART_BACKOFF_CEILING {
                    backoff = settings.backoff_base;
                }
                carry = next;
                sleep(backoff).await;
                backoff = next_backoff(backoff);
            }
        }
    }
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(RESTART_BACKOFF_CEILING)
}

async fn refuse_forever(jobs: &mut mpsc::Receiver<Envelope>, reason: &str) {
    while let Some(envelope) = jobs.recv().await {
        let _ = envelope
            .reply
            .send(Err(JobFailure::Unavailable(reason.to_owned())));
    }
}

/// One worker process, from spawn to death.
async fn session(
    ctx: &mut Ctx<'_>,
    jobs: &mut mpsc::Receiver<Envelope>,
    first: Pending,
) -> Outcome {
    let mut child = match spawn_child(ctx.settings) {
        Ok(child) => child,
        Err(SpawnFailure::Permanent(reason)) => {
            tracing::error!(
                worker = WORKER,
                interpreter = %ctx.settings.interpreter.display(),
                script = %ctx.settings.script.display(),
                %reason,
                "the compute worker cannot be started"
            );
            ctx.bus.publish(Alert::critical(
                ALERT_UNAVAILABLE,
                format!(
                    "The compute worker cannot be started: {reason}. Check \
                     `workers.python_interpreter` and `workers.compute_worker`."
                ),
            ));
            ctx.failed();
            first.fail(JobFailure::Unavailable(reason.clone()));
            return Outcome::Fatal { reason };
        }
        Err(SpawnFailure::Transient(reason)) => {
            let carry = ctx.retry_or_fail(first, &reason);
            return Outcome::Died { reason, carry };
        }
    };

    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        // Unreachable with three `Stdio::piped()` above; an `expect` here would take the whole
        // stacking server down for a case the type system cannot rule out.
        let reason = "the worker process exposed no stdio pipes".to_owned();
        kill(&mut child).await;
        let carry = ctx.retry_or_fail(first, &reason);
        return Outcome::Died { reason, carry };
    };

    let (frames_tx, frames) = mpsc::channel(FRAME_QUEUE_DEPTH);
    tokio::spawn(read_frames(stdout, frames_tx));
    tokio::spawn(log_stderr(stderr));

    serve(ctx, jobs, child, stdin, frames, first).await
}

enum SpawnFailure {
    /// Will fail identically on every retry — a configuration error, not a transient one.
    Permanent(String),
    Transient(String),
}

fn spawn_child(settings: &Settings) -> Result<Child, SpawnFailure> {
    let mut command = Command::new(&settings.interpreter);
    command
        // `-u`: CPython block-buffers stdout when it is a pipe, so without this a worker's
        // frames sit in libc until 8 KiB of them accumulate — the handshake times out and the
        // cause is invisible. The worker flushes explicitly too; this is the belt to that
        // braces, and it makes tracebacks on stderr arrive while they are still relevant.
        .arg("-u")
        .arg(&settings.script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Without this an aborting backbone leaves a Python process holding the GPU context,
        // and the replacement finds the device busy.
        .kill_on_drop(true);

    command.spawn().map_err(|error| {
        let reason = format!(
            "cannot run `{} -u {}`: {error}",
            settings.interpreter.display(),
            settings.script.display()
        );
        match error.kind() {
            // A missing or unexecutable interpreter fails identically forever. Retrying it on a
            // backoff produces one log line a minute and no progress; the config is what has to
            // change, so say that once and stop.
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => {
                SpawnFailure::Permanent(reason)
            }
            _ => SpawnFailure::Transient(reason),
        }
    })
}

/// Read framed messages off the worker's stdout until it closes.
async fn read_frames(stdout: ChildStdout, sink: mpsc::Sender<Result<FromWorker, ProtocolError>>) {
    let mut reader = BufReader::new(stdout);
    let mut frame = Vec::new();
    loop {
        match read_frame(&mut reader, &mut frame).await {
            Ok(true) => {
                if sink.send(decode_from_worker_bytes(&frame)).await.is_err() {
                    break;
                }
            }
            // EOF. A worker SIGKILLed halfway through writing its result leaves a newline-less
            // partial frame in `frame`, and dropping it is the entire point: a truncated JSON
            // object must never have a chance of parsing as a complete one.
            Ok(false) => break,
            Err(error) => {
                tracing::warn!(worker = WORKER, %error, "the worker's output stream failed");
                break;
            }
        }
    }
}

/// Read one newline-terminated frame. `Ok(false)` means EOF.
async fn read_frame(reader: &mut BufReader<ChildStdout>, frame: &mut Vec<u8>) -> io::Result<bool> {
    frame.clear();
    loop {
        let (complete, consumed) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(false);
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(at) => {
                    frame.extend_from_slice(&available[..at]);
                    (true, at + 1)
                }
                None => {
                    frame.extend_from_slice(available);
                    (false, available.len())
                }
            }
        };
        reader.consume(consumed);
        if complete {
            return Ok(true);
        }
        // Bounded here rather than after the fact: `read_until` would grow this buffer to the
        // size of whatever the worker is emitting before anyone got the chance to object.
        if frame.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the worker emitted over {MAX_LINE_BYTES} bytes with no frame boundary"),
            ));
        }
    }
}

/// SDD §5.12.1: stderr is captured into tracing with a `worker` field and is *not* the protocol.
/// A worker with something to say uses a `log` frame, so anything arriving here is a traceback
/// or a stray `print` — both worth a warning.
async fn log_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::warn!(worker = WORKER, "{line}");
    }
}

enum Handshake {
    Ready(WorkerCaps),
    Mismatch(u16),
    Failed(String),
}

async fn handshake(stdin: &mut ChildStdin, frames: &mut Frames, budget: Duration) -> Handshake {
    if let Err(reason) = write_frame(
        stdin,
        &ToWorker::Hello {
            proto_version: PROTO_VERSION,
        },
        budget,
    )
    .await
    {
        return Handshake::Failed(reason);
    }

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let frame = match timeout_at(deadline, frames.recv()).await {
            Err(_) => {
                return Handshake::Failed(format!("no hello within {HANDSHAKE_TIMEOUT:?}"));
            }
            Ok(None) => {
                return Handshake::Failed("the worker exited before the handshake".to_owned());
            }
            Ok(Some(frame)) => frame,
        };
        match frame {
            Ok(FromWorker::Hello {
                proto_version,
                capabilities,
            }) => {
                return if proto_version == PROTO_VERSION {
                    Handshake::Ready(capabilities)
                } else {
                    Handshake::Mismatch(proto_version)
                };
            }
            // A worker is allowed to say something before it says hello; a log line is not a
            // protocol violation and must not be mistaken for a failed start.
            Ok(FromWorker::Log { level, message }) => emit_worker_log(&level, &message),
            Ok(other) => {
                return Handshake::Failed(format!(
                    "expected hello, the worker sent `{}`",
                    frame_name(&other)
                ));
            }
            Err(error) => {
                return Handshake::Failed(format!("unreadable handshake: {error}"));
            }
        }
    }
}

/// The handshake, then jobs and pings until something ends the session.
async fn serve(
    ctx: &mut Ctx<'_>,
    jobs: &mut mpsc::Receiver<Envelope>,
    mut child: Child,
    mut stdin: ChildStdin,
    mut frames: Frames,
    first: Pending,
) -> Outcome {
    let budget = ctx.settings.write_budget();

    match handshake(&mut stdin, &mut frames, budget).await {
        Handshake::Ready(caps) => {
            tracing::info!(
                worker = WORKER,
                proto_version = PROTO_VERSION,
                gpu = caps.gpu,
                vram_mb = ?caps.vram_mb,
                libs = %render_libs(&caps),
                "compute worker ready"
            );
        }
        Handshake::Mismatch(worker_version) => {
            let reason = format!(
                "the worker speaks protocol v{worker_version}, this backbone speaks \
                 v{PROTO_VERSION}"
            );
            tracing::error!(
                worker = WORKER,
                backbone_proto_version = PROTO_VERSION,
                worker_proto_version = worker_version,
                script = %ctx.settings.script.display(),
                "refusing the compute worker: protocol version mismatch"
            );
            ctx.bus.publish(Alert::critical(
                ALERT_PROTO_MISMATCH,
                format!(
                    "{reason}. `{}` does not match this build; deploy the two together.",
                    ctx.settings.script.display()
                ),
            ));
            kill(&mut child).await;
            ctx.failed();
            first.fail(JobFailure::Unavailable(reason.clone()));
            // §5.12.2: the supervisor does not retry a version mismatch. Retrying a
            // deterministic failure is a crash loop with extra steps.
            return Outcome::Fatal { reason };
        }
        Handshake::Failed(reason) => {
            kill(&mut child).await;
            let carry = ctx.retry_or_fail(first, &reason);
            return Outcome::Died { reason, carry };
        }
    }

    let mut next_id: JobId = 1;
    let mut nonce: Nonce = 0;
    let mut unanswered_pings: u32 = 0;

    let mut ping = interval(ctx.settings.ping);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // `interval` yields its first tick immediately; consume it so the first ping is one full
    // period after the handshake rather than on top of it.
    ping.tick().await;

    let mut inflight = match dispatch(ctx, &mut stdin, &mut next_id, first, budget).await {
        Ok(live) => Some(live),
        Err(DispatchFailure { reason, pending }) => {
            kill(&mut child).await;
            let carry = ctx.retry_or_fail(pending, &reason);
            return Outcome::Died { reason, carry };
        }
    };

    loop {
        let job_deadline = inflight.as_ref().map(|live| live.deadline);

        tokio::select! {
            frame = frames.recv() => match frame {
                None => {
                    let reason = exit_reason(&mut child).await;
                    let carry = match inflight.take() {
                        Some(live) => ctx.retry_or_fail(live.pending, &reason),
                        None => None,
                    };
                    return Outcome::Died { reason, carry };
                }
                Some(Err(error)) => {
                    // One unreadable frame does not justify killing a worker mid-job. If what
                    // was lost was the result, the job timeout is the backstop.
                    tracing::warn!(worker = WORKER, %error, "discarding an unreadable worker frame");
                }
                Some(Ok(FromWorker::Pong { .. })) => unanswered_pings = 0,
                Some(Ok(FromWorker::Log { level, message })) => emit_worker_log(&level, &message),
                Some(Ok(FromWorker::Progress { id, pct })) => {
                    tracing::debug!(worker = WORKER, job = id, pct = u32::from(pct), "compute job progress");
                }
                Some(Ok(FromWorker::Hello { proto_version, .. })) => {
                    tracing::warn!(worker = WORKER, proto_version, "ignoring a second hello mid-session");
                }
                Some(Ok(FromWorker::Result { id, ok, data, error })) => {
                    match inflight.take() {
                        Some(live) if live.id == id => {
                            settle(ctx, live, ok, data, error);
                            ctx.set_state(WorkerState::Ready);
                        }
                        // A result for a job we are not running: a straggler from a process
                        // that already died, or a worker inventing job ids. Never deliverable.
                        other => {
                            tracing::warn!(worker = WORKER, job = id, "ignoring a result for an unknown job");
                            inflight = other;
                        }
                    }
                }
            },

            _ = ping.tick() => {
                if unanswered_pings >= MISSED_PONGS_BEFORE_KILL {
                    let reason = format!("no answer to {unanswered_pings} consecutive pings");
                    kill(&mut child).await;
                    let carry = match inflight.take() {
                        Some(live) => ctx.retry_or_fail(live.pending, &reason),
                        None => None,
                    };
                    return Outcome::Died { reason, carry };
                }
                nonce += 1;
                unanswered_pings += 1;
                if let Err(reason) = write_frame(&mut stdin, &ToWorker::Ping { nonce }, budget).await {
                    kill(&mut child).await;
                    let carry = match inflight.take() {
                        Some(live) => ctx.retry_or_fail(live.pending, &reason),
                        None => None,
                    };
                    return Outcome::Died { reason, carry };
                }
            },

            () = sleep_until(job_deadline.unwrap_or_else(Instant::now)), if job_deadline.is_some() => {
                let Some((id, already_cancelled)) =
                    inflight.as_ref().map(|live| (live.id, live.cancelled))
                else {
                    continue;
                };

                if already_cancelled {
                    let reason = format!("job {id} ignored `cancel` for {CANCEL_GRACE:?}");
                    kill(&mut child).await;
                    if let Some(live) = inflight.take() {
                        ctx.failed();
                        live.pending.fail(JobFailure::TimedOut(ctx.settings.job_timeout));
                    }
                    // Not retried — see the module docs.
                    return Outcome::Died { reason, carry: None };
                }

                tracing::warn!(
                    worker = WORKER,
                    job = id,
                    timeout = ?ctx.settings.job_timeout,
                    "cancelling a compute job that exceeded workers.job_timeout_seconds"
                );
                if let Some(live) = inflight.as_mut() {
                    live.cancelled = true;
                    live.deadline = Instant::now() + CANCEL_GRACE;
                }
                if let Err(reason) = write_frame(&mut stdin, &ToWorker::Cancel { id }, budget).await {
                    kill(&mut child).await;
                    if let Some(live) = inflight.take() {
                        ctx.failed();
                        live.pending.fail(JobFailure::TimedOut(ctx.settings.job_timeout));
                    }
                    return Outcome::Died { reason, carry: None };
                }
            },

            envelope = jobs.recv(), if inflight.is_none() => {
                let Some(envelope) = envelope else {
                    shutdown(&mut child, stdin).await;
                    return Outcome::Closed;
                };
                match dispatch(ctx, &mut stdin, &mut next_id, Pending::new(envelope), budget).await {
                    Ok(live) => inflight = Some(live),
                    Err(DispatchFailure { reason, pending }) => {
                        kill(&mut child).await;
                        let carry = ctx.retry_or_fail(pending, &reason);
                        return Outcome::Died { reason, carry };
                    }
                }
            },
        }
    }
}

/// Deliver a terminal result to whoever is waiting on it.
fn settle(
    ctx: &mut Ctx<'_>,
    live: Live,
    ok: bool,
    data: Option<Value>,
    error: Option<WorkerError>,
) {
    if live.cancelled {
        // The caller is still waiting, but the answer is already decided: the job outlived its
        // budget, and a late success is not the thing that was asked for.
        ctx.failed();
        live.pending
            .fail(JobFailure::TimedOut(ctx.settings.job_timeout));
        return;
    }
    if ok {
        ctx.completed();
        live.pending.succeed(data.unwrap_or(Value::Null));
    } else {
        ctx.failed();
        // `FromWorker::decode` already rejects `ok: false` with no error, so this default is
        // unreachable through the pipe; it exists so the failure path has no `expect`.
        let error = error.unwrap_or_else(|| WorkerError {
            code: ErrorCode::Internal,
            message: "the worker reported a failure with no detail".to_owned(),
        });
        live.pending.fail(JobFailure::Worker(error));
    }
}

struct DispatchFailure {
    reason: String,
    pending: Pending,
}

async fn dispatch(
    ctx: &mut Ctx<'_>,
    stdin: &mut ChildStdin,
    next_id: &mut JobId,
    pending: Pending,
    budget: Duration,
) -> Result<Live, DispatchFailure> {
    let id = *next_id;
    *next_id += 1;
    let message = ToWorker::Job {
        id,
        kind: pending.envelope.kind,
        // Cloned rather than moved because a worker death entitles this job to one more
        // attempt, and the retry needs the same parameters.
        params: pending.envelope.params.clone(),
        paths: pending.envelope.paths.clone(),
    };
    match write_frame(stdin, &message, budget).await {
        Ok(()) => {
            ctx.set_state(WorkerState::Busy);
            Ok(Live {
                pending,
                id,
                deadline: Instant::now() + ctx.settings.job_timeout,
                cancelled: false,
            })
        }
        Err(reason) => Err(DispatchFailure { reason, pending }),
    }
}

async fn write_frame(
    stdin: &mut ChildStdin,
    message: &ToWorker,
    budget: Duration,
) -> Result<(), String> {
    let frame = message.encode().map_err(|error| error.to_string())?;
    // A worker that has stopped reading its stdin fills the pipe, and an unbounded write would
    // then park the whole supervision loop — including the ping timer that exists to notice
    // exactly this. The wedge has to be detectable by something other than the wedged path.
    let write = async {
        stdin.write_all(frame.as_bytes()).await?;
        stdin.flush().await
    };
    match timeout(budget, write).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("writing to the worker failed: {error}")),
        Err(_) => Err(format!(
            "the worker did not read its stdin within {budget:?}"
        )),
    }
}

async fn shutdown(child: &mut Child, mut stdin: ChildStdin) {
    let _ = write_frame(&mut stdin, &ToWorker::Shutdown, SHUTDOWN_GRACE).await;
    // Closing stdin is the other half of the ask: a worker that ignores `shutdown` still sees
    // EOF on its input and has nothing left to wait for.
    drop(stdin);
    if timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
        kill(child).await;
    }
}

async fn kill(child: &mut Child) {
    if let Err(error) = child.start_kill() {
        tracing::warn!(worker = WORKER, %error, "could not signal the compute worker");
    }
    // Reap it, or a killed worker lingers as a zombie holding its pid.
    if timeout(REAP_TIMEOUT, child.wait()).await.is_err() {
        tracing::warn!(
            worker = WORKER,
            "the compute worker did not exit after SIGKILL"
        );
    }
}

/// Why the worker's output stopped, phrased for a log line. On Linux this is where `signal: 9
/// (SIGKILL)` appears, which is the difference between "it crashed" and "the OOM killer took it".
async fn exit_reason(child: &mut Child) -> String {
    match timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => format!("the worker exited ({status})"),
        Ok(Err(error)) => format!("the worker exited and could not be reaped: {error}"),
        Err(_) => {
            kill(child).await;
            "the worker closed its output but did not exit".to_owned()
        }
    }
}

fn emit_worker_log(level: &str, message: &str) {
    match level.to_ascii_lowercase().as_str() {
        "trace" => tracing::trace!(worker = WORKER, "{message}"),
        "debug" => tracing::debug!(worker = WORKER, "{message}"),
        "warn" | "warning" => tracing::warn!(worker = WORKER, "{message}"),
        "error" | "critical" | "fatal" => tracing::error!(worker = WORKER, "{message}"),
        _ => tracing::info!(worker = WORKER, "{message}"),
    }
}

fn frame_name(message: &FromWorker) -> &'static str {
    match message {
        FromWorker::Hello { .. } => "hello",
        FromWorker::Progress { .. } => "progress",
        FromWorker::Result { .. } => "result",
        FromWorker::Pong { .. } => "pong",
        FromWorker::Log { .. } => "log",
    }
}

fn render_libs(caps: &WorkerCaps) -> String {
    if caps.libs.is_empty() {
        return "none".to_owned();
    }
    caps.libs
        .iter()
        .map(|(name, version)| format!("{name} {version}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WorkersConfig {
        // Not parsed from YAML: this asserts the behaviour of `Settings`, and the YAML path is
        // already covered by astroctl-core's config tests.
        serde_json::from_value(serde_json::json!({
            "python_interpreter": "/usr/bin/python3",
            "compute_worker": "workers/compute_worker.py",
            "ml_worker": "workers/ml_worker.py",
            "health_ping_seconds": 5,
            "restart_backoff_seconds": 2,
            "job_timeout_seconds": 300,
        }))
        .expect("the workers block deserializes")
    }

    #[test]
    fn the_backoff_doubles_and_stops_at_the_ceiling() {
        let mut backoff = Duration::from_secs(2);
        let mut seen = vec![backoff];
        for _ in 0..8 {
            backoff = next_backoff(backoff);
            seen.push(backoff);
        }
        assert_eq!(seen[1], Duration::from_secs(4));
        assert_eq!(seen[2], Duration::from_secs(8));
        assert_eq!(*seen.last().expect("non-empty"), RESTART_BACKOFF_CEILING);
    }

    #[test]
    fn a_relative_worker_script_is_resolved_before_it_is_spawned() {
        let settings = Settings::from_config(&config());
        assert!(
            settings.script.is_absolute(),
            "script was {}",
            settings.script.display()
        );
        assert!(settings.script.ends_with("workers/compute_worker.py"));
    }

    #[test]
    fn a_zero_ping_cannot_reach_tokios_interval() {
        let mut raw = config();
        raw.health_ping_seconds = 0;
        raw.restart_backoff_seconds = 0;
        let settings = Settings::from_config(&raw);
        assert!(!settings.ping.is_zero());
        assert!(!settings.backoff_base.is_zero());
    }

    #[test]
    fn the_write_budget_never_drops_below_the_floor() {
        let mut raw = config();
        raw.health_ping_seconds = 1;
        assert_eq!(Settings::from_config(&raw).write_budget(), MIN_WRITE_BUDGET);
        raw.health_ping_seconds = 30;
        assert_eq!(
            Settings::from_config(&raw).write_budget(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn every_failure_shape_carries_its_own_error_code() {
        // The worker's own diagnosis passes through untouched — only it knows whether the
        // problem was a missing file or a full disk.
        assert_eq!(
            JobFailure::Worker(WorkerError {
                code: ErrorCode::NotFound,
                message: "no such frame".to_owned(),
            })
            .code(),
            ErrorCode::NotFound
        );
        // Supervisor-produced failures each name themselves (§4.2 worker vocabulary,
        // 2026-07-30). Before that they collapsed onto INTERNAL, which told the operator
        // "the stacking software has a bug" when the truth was "the worker crashed and is
        // being restarted".
        assert_eq!(
            JobFailure::TimedOut(Duration::from_secs(1)).code(),
            ErrorCode::WorkerTimeout
        );
        assert_eq!(
            JobFailure::Crashed {
                attempts: 1,
                reason: "boom".to_owned(),
            }
            .code(),
            ErrorCode::WorkerCrashed
        );
        assert_eq!(
            JobFailure::Unavailable("spawn failed".to_owned()).code(),
            ErrorCode::WorkerUnavailable
        );
        // Stopped is a cancellation, not a failure: the job was surrendered to shutdown.
        assert_eq!(JobFailure::Stopped.code(), ErrorCode::Cancelled);
    }
}
