//! The camera thread, the command channel, the operation-class timeouts and the wedge protocol —
//! SDD §5.3.1.
//!
//! # Why a thread and not a task
//!
//! libgphoto2 blocks for seconds and its `Context` is not safe to share between threads. Those
//! are two separate reasons and each alone is sufficient:
//!
//! * A blocking call on a tokio worker occupies it. The field node sizes its runtime at
//!   `min(2, cores-2)` workers (SDD §7), so a 2.08 s capture on a worker is half the node's
//!   concurrency gone for two seconds — mount polling, the event bus and the API all stall.
//!   T-ISO-1 (SDD §9) is the regression test that exists to catch exactly that creeping back.
//! * Even `spawn_blocking` would be wrong: it hands the work to *a* pool thread, not *the* same
//!   one every time, and the context has to live on one thread for the device's lifetime.
//!
//! So: one dedicated OS thread, named [`CAMERA_THREAD_NAME`], owning the context from open to
//! close. `std::sync::mpsc` inbound because the receiving end is not async, tokio `oneshot`
//! outbound because the waiting end is.
//!
//! # One context means one queue
//!
//! Commands are serviced strictly in order, because there is exactly one context. Live view and
//! capture therefore contend and cannot be made not to (SDD §5.3.1). That is surfaced rather than
//! hidden — the facade publishes `capture.progress` around the blocking region so the UI can say
//! *why* the preview stopped — but the queueing itself happens here, and nothing above this
//! module can change it.
//!
//! # The wedge protocol is a stub in M2-T02
//!
//! A command that exceeds its budget means the thread is stuck inside a libgphoto2 call. It
//! cannot be killed — there is no safe way to interrupt a C call mid-flight — so the design
//! abandons it: drop the channel, let the thread exit when its current call finally returns, and
//! surface `DeviceError::Timeout`. **What M2-T02 implements is that much and no more.** The
//! respawn (fresh thread, fresh context, USB reset, `camera.status` reconnecting event) is M2-T04
//! and is marked `TODO(M2-T04)` at the one place it attaches.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use astroctl_core::config::CameraTimeouts;
use astroctl_core::error::DeviceError;
use astroctl_core::types::{BatteryStatus, StorageInfo};
use tokio::sync::oneshot;

use super::download::{self, LandedFile};
use super::gvfs;
use super::ops::{
    AbortSignal, CamOps, CamOpsFactory, CfgKey, RawCapture, RawChoices, RawFileRef, RawIdentity,
    RawSettings,
};

/// The OS thread name every blocking libgphoto2 call runs under.
///
/// Load-bearing, not decorative. It is what the tracing span reports, so a log line proves which
/// thread a call was on; and it is what `a_blocking_call_never_runs_on_the_callers_thread`
/// asserts, so the claim is checked rather than described. If this name ever appears on two
/// threads, or a `CamOps` call reports any other name, the isolation SDD §5.3.1 promises has
/// been lost.
pub(crate) const CAMERA_THREAD_NAME: &str = "astroctl-camera";

/// How long an operation may take before the thread is considered wedged (SDD §5.3.1).
///
/// The classes are the operation's *shape*, not its name: everything that is one config-tree
/// touch shares a budget because they share a cost profile, and a new command joins an existing
/// class rather than bringing its own number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpClass {
    /// Autodetect, open, read the abilities and walk the config tree.
    ///
    /// Shares `config_seconds` with [`Config`](Self::Config) rather than inventing a number.
    /// **Measured**: connect took 190–210 ms and the full 91-entry tree 222 ms (M2-T01), so the
    /// default 5 s is roughly ten times the observed cost. It is also enough because the failure
    /// modes here return *fast* — a camera that is absent or claimed answers immediately with an
    /// error rather than hanging, which the spike established by pulling the cable.
    Connect,
    /// One setting read or write, or one status query.
    ///
    /// **Measured**: 0.4–10 ms per read, 11 ms per write. The budget exists for the pathological
    /// case, not the normal one.
    Config,
    /// One exposure of the carried duration, plus the trigger and the wire.
    ///
    /// The budget is `exposure + capture_extra_seconds` because the exposure is the one part of
    /// the cost that is *known in advance and unbounded*: a 300 s bulb frame is not a wedged
    /// camera, and a fixed budget would either abandon the thread mid-integration or be so large
    /// it never fires. The allowance on top covers everything that is not the exposure —
    /// M2-T01 measured 2.08 s of it for a timed capture, against a 30 s default.
    ///
    /// **The exposure is part of the class, not of the command**, so the budget cannot drift from
    /// the exposure it is meant to cover: there is one place that knows both.
    Capture(Duration),
    /// One file off the camera and onto the disk.
    ///
    /// `download_seconds`, defaulting to 120. Generous against the measured cost because the
    /// measured cost is not the interesting case: with `capturetarget=Internal RAM` the frame is
    /// already in libgphoto2's memory and the write took 2.67 ms (M2-T01), but a body set to
    /// `Memory card` transfers 32 MB over USB inside this call, and that is the case the number
    /// is for.
    Download,
}

impl OpClass {
    /// The budget for this class, from configuration.
    pub(crate) fn budget(self, timeouts: &CameraTimeouts) -> Duration {
        match self {
            Self::Connect | Self::Config => Duration::from_secs(timeouts.config_seconds),
            Self::Capture(exposure) => {
                exposure.saturating_add(Duration::from_secs(timeouts.capture_extra_seconds))
            }
            Self::Download => Duration::from_secs(timeouts.download_seconds),
        }
    }
}

/// A reply channel. The camera thread owns the sending half for exactly one command.
type Reply<T> = oneshot::Sender<Result<T, DeviceError>>;

/// One unit of work for the camera thread (SDD §5.3.1's `CamCmd`).
///
/// Each variant carries its own reply channel rather than the enum having one, because the reply
/// types differ and a single `Reply<CamReply>` would need an enum on the way back and a match at
/// every call site that can only fail one way.
pub(crate) enum CamCmd {
    /// Build the ops object *on this thread*, then open the camera.
    Open(Reply<RawIdentity>),
    /// Close the camera but keep the thread — a reconnect then costs no respawn.
    Close(Reply<()>),
    /// Read the settings in force.
    GetSettings(Reply<RawSettings>),
    /// Re-enumerate what the body accepts.
    GetChoices(Reply<RawChoices>),
    /// Write one setting.
    SetSetting {
        /// Which key.
        key: CfgKey,
        /// The camera token to write; already checked against the body's own choices.
        value: String,
        /// Where the outcome goes.
        reply: Reply<()>,
    },
    /// Battery charge and external-power state.
    GetBattery(Reply<BatteryStatus>),
    /// Card space.
    GetStorage(Reply<StorageInfo>),
    /// Take one exposure at the settings in force.
    Capture(Reply<RawCapture>),
    /// Hold the shutter open for `duration`.
    CaptureBulb {
        /// How long to hold it.
        duration: Duration,
        /// The abort generation this exposure started at — movement away from it ends the hold.
        since: u64,
        /// Where the outcome goes.
        reply: Reply<RawCapture>,
    },
    /// Land one camera-side file in `dir` under `stem`, durably.
    Download {
        /// The file the body is holding.
        file: RawFileRef,
        /// The session directory the frame store created.
        dir: PathBuf,
        /// The filename stem, without extension.
        stem: String,
        /// Where the outcome goes.
        reply: Reply<LandedFile>,
    },
    /// Release the shutter if the body might still be holding it.
    ///
    /// The *queued* half of aborting, and deliberately not the part that does the work: a command
    /// sent while a capture is running would be serviced after it, so promptness comes from
    /// [`AbortSignal`] instead. This is the safety net that runs once the thread is free again.
    Abort(Reply<()>),
    /// Remove the temporaries an abandoned capture would have written.
    ///
    /// On the camera thread rather than the caller's because it must not race the download it is
    /// cleaning up after, and the queue is what guarantees that.
    Sweep {
        /// The session directory.
        dir: PathBuf,
        /// The filename stem.
        stem: String,
        /// The files whose temporaries to remove.
        files: Vec<RawFileRef>,
        /// Where the outcome goes.
        reply: Reply<()>,
    },
    // M2-T04: LiveViewStart, LiveViewStop.
}

impl CamCmd {
    /// The name that appears in the span, and in the timeout message the operator reads.
    fn name(&self) -> &'static str {
        match self {
            Self::Open(_) => "open",
            Self::Close(_) => "close",
            Self::GetSettings(_) => "get_settings",
            Self::GetChoices(_) => "get_choices",
            Self::SetSetting { .. } => "set_setting",
            Self::GetBattery(_) => "get_battery",
            Self::GetStorage(_) => "get_storage",
            Self::Capture(_) => "capture",
            Self::CaptureBulb { .. } => "capture_bulb",
            Self::Download { .. } => "download",
            Self::Abort(_) => "abort",
            Self::Sweep { .. } => "sweep",
        }
    }
}

/// The facade's end of the camera thread.
///
/// Holds the *only* [`Sender`], which is what makes the wedge protocol work: dropping it closes
/// the channel, and the thread's blocking `recv` is then the thing that tells it to stop. A
/// clone escaping this struct would keep the channel alive and an abandoned thread would sit
/// there holding a USB claim for the rest of the process's life.
#[derive(Debug)]
pub(crate) struct CameraLink {
    /// `None` once the link has been shut down or wedged.
    sender: Mutex<Option<Sender<CamCmd>>>,
    /// Kept so a graceful shutdown can wait for the context to actually be released. A wedged
    /// thread is never joined — that is the definition of abandoning it.
    handle: Mutex<Option<JoinHandle<()>>>,
    /// Per-operation-class budgets, from config.
    timeouts: CameraTimeouts,
    /// The out-of-band route to a busy thread. See [`AbortSignal`] for why a command cannot be
    /// one.
    abort: Arc<AbortSignal>,
    /// Whether an exposure is in flight.
    ///
    /// Lives here, beside the sender, so that
    /// [`request_unless_capturing`](Self::request_unless_capturing) can test it under the same
    /// lock that sends. A flag held one layer up would be readable but not *atomic with the
    /// send*, which is the property that matters.
    capturing: AtomicBool,
}

/// Holds the imaging path for one exposure, and releases it however the exposure ends.
///
/// RAII rather than a matched pair of stores because a capture has six ways out — refused,
/// aborted, timed out, failed to download, wedged, or successful — and the fifth one to forget
/// the release is the one that leaves the camera permanently `Busy` with nothing running.
pub(crate) struct CaptureClaim<'a>(&'a AtomicBool);

impl Drop for CaptureClaim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl CameraLink {
    /// Spawns the camera thread.
    ///
    /// The factory crosses the thread boundary; the `CamOps` it builds never does. No I/O
    /// happens here — the thread starts idle and the camera is opened by the first
    /// [`CamCmd::Open`], so constructing a driver stays free of side effects as SDD §8.1
    /// requires of registry construction.
    ///
    /// # Errors
    /// `Transport` if the OS refuses to start a thread.
    pub(crate) fn spawn(
        factory: Arc<dyn CamOpsFactory>,
        timeouts: CameraTimeouts,
    ) -> Result<Self, DeviceError> {
        let (sender, receiver) = mpsc::channel();
        let abort = Arc::new(AbortSignal::default());
        let thread_abort = Arc::clone(&abort);
        let handle = std::thread::Builder::new()
            .name(CAMERA_THREAD_NAME.to_owned())
            .spawn(move || run(&*factory, &receiver, &thread_abort))
            .map_err(|error| {
                DeviceError::Transport(format!("could not start the camera thread: {error}"))
            })?;

        Ok(Self {
            sender: Mutex::new(Some(sender)),
            handle: Mutex::new(Some(handle)),
            timeouts,
            abort,
            capturing: AtomicBool::new(false),
        })
    }

    /// The abort generation a capture should record before it starts.
    pub(crate) fn abort_generation(&self) -> u64 {
        self.abort.generation()
    }

    /// Raises an abort, reaching the thread even while it is mid-exposure.
    pub(crate) fn raise_abort(&self) {
        self.abort.raise();
    }

    /// Whether an abort has been raised since `since`.
    pub(crate) fn aborted_since(&self, since: u64) -> bool {
        self.abort.raised_since(since)
    }

    /// Claims the imaging path for one exposure, or reports what is using it.
    ///
    /// Held by the link rather than by the driver because
    /// [`request_unless_capturing`](Self::request_unless_capturing) has to consult it *under the
    /// sender lock* — see there for why a check anywhere else is a race with a wedge on the
    /// losing side.
    pub(crate) fn claim_capture(&self) -> Result<CaptureClaim<'_>, DeviceError> {
        self.capturing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| CaptureClaim(&self.capturing))
            .map_err(|_| DeviceError::Busy("the camera is already exposing"))
    }

    /// Sends a command and waits for its reply, bounded by the class budget.
    ///
    /// # Errors
    /// Whatever the camera returned; `NotConnected` if the link is already down; `Timeout` if the
    /// budget expired, in which case the thread has been abandoned.
    pub(crate) async fn request<T, F>(&self, class: OpClass, build: F) -> Result<T, DeviceError>
    where
        F: FnOnce(Reply<T>) -> CamCmd,
    {
        self.send_and_wait(class, build, false).await
    }

    /// As [`request`](Self::request), but refuses rather than queueing behind an exposure.
    ///
    /// **This is what stops a routine status poll from destroying a capture**, and the failure it
    /// prevents is not exotic. There is one queue, so a `get_battery` issued during a 300 s bulb
    /// exposure is serviced 300 seconds later — but its budget is `config_seconds`, five. It
    /// therefore times out, and a timed-out command is by definition a wedged thread (SDD
    /// §5.3.1): the link drops its sender and abandons the thread, and the *running* exposure
    /// then fails to download with `NotConnected`. One health tick would cost the frame and the
    /// camera.
    ///
    /// Before M2-T03 nothing could occupy the thread for longer than a config operation, so the
    /// shared budget was safe. Capture and bulb are what made this reachable, so this is where it
    /// is closed: every operation that is *not* part of an exposure asks through here and gets an
    /// immediate `Busy` instead of a place in a queue it cannot afford to wait in.
    ///
    /// # Errors
    /// `Busy` while an exposure is in flight, otherwise as [`request`](Self::request).
    pub(crate) async fn request_unless_capturing<T, F>(
        &self,
        class: OpClass,
        build: F,
    ) -> Result<T, DeviceError>
    where
        F: FnOnce(Reply<T>) -> CamCmd,
    {
        self.send_and_wait(class, build, true).await
    }

    /// The body of both request forms.
    ///
    /// `refuse_while_capturing` is tested *inside* the sender lock, which is what makes the gate
    /// airtight rather than advisory: checking the flag before calling would leave a window in
    /// which a capture starts between the check and the send, putting the command on the queue
    /// after all — and the consequence of losing that race is a wedged camera, not a slow reply.
    async fn send_and_wait<T, F>(
        &self,
        class: OpClass,
        build: F,
        refuse_while_capturing: bool,
    ) -> Result<T, DeviceError>
    where
        F: FnOnce(Reply<T>) -> CamCmd,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = build(reply_tx);
        let operation = command.name();
        let budget = class.budget(&self.timeouts);

        // The guard is scoped so it provably does not span the await below. The workspace denies
        // `await_holding_lock` (SDD §2) and this is the one function in the driver that could
        // trip it: holding this lock across the await would serialise every camera command
        // behind the slowest one *and* park the guard on a runtime worker.
        {
            let sender = self
                .sender
                .lock()
                .expect("the camera link's sender is never poisoned");
            // Under the lock, so a capture cannot start between this and the send.
            if refuse_while_capturing && self.capturing.load(Ordering::SeqCst) {
                return Err(DeviceError::Busy("the camera is exposing"));
            }
            let Some(sender) = sender.as_ref() else {
                return Err(DeviceError::NotConnected);
            };
            if sender.send(command).is_err() {
                // The thread exited on its own — it panicked, or it is finishing a call after a
                // previous wedge. Either way the camera is not reachable through it.
                return Err(DeviceError::NotConnected);
            }
        }

        match tokio::time::timeout(budget, reply_rx).await {
            Ok(Ok(result)) => result,
            // The reply channel was dropped without an answer: the thread died between accepting
            // the command and replying. Reported as transport rather than timeout because the
            // caller waited no time at all and retrying is pointless until a reconnect.
            Ok(Err(_dropped)) => Err(DeviceError::Transport(format!(
                "the camera thread stopped while `{operation}` was in flight"
            ))),
            Err(_elapsed) => {
                self.wedge(operation, budget);
                Err(DeviceError::Timeout(budget))
            }
        }
    }

    /// Abandons the camera thread after a timeout — SDD §5.3.1's wedge protocol.
    ///
    /// The thread is *not* joined and *not* killed. It is stuck inside a libgphoto2 call and
    /// there is no safe way to interrupt one; what can be done is to stop sending it work and let
    /// it discover, when the call eventually returns, that its channel is gone. It then exits and
    /// drops the context — releasing the USB claim on the thread that took it, which is the only
    /// thread allowed to.
    fn wedge(&self, operation: &str, budget: Duration) {
        // Dropping the sole sender is the signal. Taking the handle too makes the abandonment
        // explicit: nothing will ever join this thread.
        let dropped = self
            .sender
            .lock()
            .expect("the camera link's sender is never poisoned")
            .take();
        let abandoned = self
            .handle
            .lock()
            .expect("the camera link's handle is never poisoned")
            .take();
        drop(dropped);

        tracing::error!(
            operation,
            budget_s = budget.as_secs_f64(),
            thread = CAMERA_THREAD_NAME,
            abandoned = abandoned.is_some(),
            "the camera did not answer within its budget; abandoning the camera thread"
        );
        // TODO(M2-T04): this is where recovery attaches. SDD §5.3.1 specifies a fresh thread and
        // context plus an attempted USB reset, surfaced as a `camera.status` reconnecting event;
        // M2-T01 measured that a fresh context reconnects in 108 ms while the *old* handle never
        // recovers (five retries, all failed), so respawning is the only path and there is no
        // cheaper one to try first. Until that lands, the driver reports itself disconnected and
        // the operator reconnects.
    }

    /// Stops the camera thread and waits for the context to be released.
    ///
    /// Called from `disconnect` and from `Drop`. Unlike [`wedge`](Self::wedge) this *joins*: a
    /// caller that has asked for a clean shutdown is entitled to know the USB claim is gone
    /// before it returns, or the next `connect` races the previous session's context.
    pub(crate) fn shutdown(&self) {
        let sender = self
            .sender
            .lock()
            .expect("the camera link's sender is never poisoned")
            .take();
        drop(sender);

        let handle = self
            .handle
            .lock()
            .expect("the camera link's handle is never poisoned")
            .take();
        if let Some(handle) = handle {
            // The thread may be mid-call; this waits for that call to finish, which is the
            // point. SDD §7's shutdown order would rather wait than leave a claim behind.
            let _ = handle.join();
        }
    }

    /// Whether the link still has a thread to talk to.
    pub(crate) fn is_up(&self) -> bool {
        self.sender
            .lock()
            .expect("the camera link's sender is never poisoned")
            .is_some()
    }
}

impl Drop for CameraLink {
    fn drop(&mut self) {
        // A driver dropped without `disconnect` still has to release the camera — otherwise the
        // next process to want it meets "Could not claim the USB device" and goes hunting for a
        // gvfs mount that is not there.
        self.shutdown();
    }
}

/// The camera thread's body.
///
/// Owns the `CamOps` for the whole life of the thread and drops it here, on this thread, when the
/// channel closes. That last part is the reason the ops object is built inside this function
/// rather than passed in: construction *and* destruction of the gphoto2 context both happen on
/// the thread that uses it, with no window in which another thread holds it.
fn run(factory: &dyn CamOpsFactory, receiver: &Receiver<CamCmd>, abort: &AbortSignal) {
    let mut ops: Option<Box<dyn CamOps>> = None;

    // `recv` blocks until a command arrives or every sender is gone. The second case is the exit:
    // a graceful shutdown and an abandonment look identical from in here, which is deliberate —
    // the thread cannot tell whether it was abandoned, and does not need to.
    while let Ok(command) = receiver.recv() {
        let operation = command.name();
        // Carries the thread name so a log line is evidence of where the call ran, not a claim
        // about it. `astroctl-camera` in this field on any line is the isolation invariant
        // holding; anything else is T-ISO-1's failure, visible without a debugger.
        let span = tracing::info_span!(
            "camera_op",
            thread = std::thread::current().name().unwrap_or("<unnamed>"),
            operation,
            // Empty until a `SetSetting` fills it in — a log of a settings session is then
            // readable as a list of keys rather than a row of identical `set_setting` lines.
            key = tracing::field::Empty,
        );
        if let CamCmd::SetSetting { key, .. } = &command {
            span.record("key", key.as_str());
        }
        let _entered = span.enter();

        dispatch(factory, &mut ops, command, abort);
    }

    // Channel closed. Dropping `ops` runs the gphoto2 teardown on this thread and releases the
    // USB claim.
    drop(ops);
    tracing::debug!(
        thread = CAMERA_THREAD_NAME,
        "the camera thread has released the camera and is exiting"
    );
}

/// Runs one command against the ops object, creating it on first use.
///
/// A dropped reply channel is not an error: the caller timed out and stopped listening, which is
/// exactly the wedge case. The work still had to finish — it was already in flight — and the
/// result is discarded rather than logged as a failure, because it is not one.
fn dispatch(
    factory: &dyn CamOpsFactory,
    ops: &mut Option<Box<dyn CamOps>>,
    command: CamCmd,
    abort: &AbortSignal,
) {
    /// Runs `$call` against the open camera, or answers `NotConnected`, and replies.
    macro_rules! answer {
        ($reply:expr, |$ops:ident| $call:expr) => {{
            let result = match ops.as_mut() {
                Some($ops) => $call,
                None => Err(DeviceError::NotConnected),
            };
            let _ = $reply.send(result);
        }};
    }

    match command {
        CamCmd::Open(reply) => {
            let _ = reply.send(open(factory, ops));
        }
        CamCmd::Close(reply) => {
            // Taking it first means a failing `close` still drops the context: a teardown that
            // errors is not a reason to keep holding the USB claim.
            let result = match ops.take() {
                Some(mut ops) => ops.close(),
                None => Ok(()),
            };
            let _ = reply.send(result);
        }
        CamCmd::GetSettings(reply) => answer!(reply, |ops| ops.read_settings()),
        CamCmd::GetChoices(reply) => answer!(reply, |ops| ops.read_choices()),
        CamCmd::SetSetting { key, value, reply } => {
            answer!(reply, |ops| ops.write_setting(key, &value));
        }
        CamCmd::GetBattery(reply) => answer!(reply, |ops| ops.battery()),
        CamCmd::GetStorage(reply) => answer!(reply, |ops| ops.storage()),
        CamCmd::Capture(reply) => answer!(reply, |ops| ops.capture()),
        CamCmd::CaptureBulb {
            duration,
            since,
            reply,
        } => answer!(reply, |ops| ops.capture_bulb(duration, abort, since)),
        CamCmd::Download {
            file,
            dir,
            stem,
            reply,
        } => answer!(reply, |ops| download::land(
            ops.as_mut(),
            &file,
            &dir,
            &stem
        )),
        CamCmd::Abort(reply) => {
            // Not `answer!`: aborting a camera that is not open is `Ok(())`, never
            // `NotConnected`. A stopping command may not fail for want of something to stop
            // (SDD §5.8.1), and "there is no camera" is the strongest possible guarantee that
            // the shutter is closed.
            let result = match ops.as_mut() {
                Some(ops) => ops.abort(),
                None => Ok(()),
            };
            let _ = reply.send(result);
        }
        CamCmd::Sweep {
            dir,
            stem,
            files,
            reply,
        } => {
            // Filesystem only, so it needs no camera and must work without one — the capture
            // whose debris this is may have ended by losing the camera.
            download::sweep(&dir, &stem, &files);
            let _ = reply.send(Ok(()));
        }
    }
}

/// Builds the ops object if needed, opens the camera, and explains a failed claim.
///
/// Reconnecting an already-open camera is `Ok` with the identity re-read, which is what the
/// `Camera` trait means by "connecting an already-connected camera is `Ok(())`" — and re-reading
/// rather than replaying a cached identity matters, because the mode dial may have moved.
///
/// **The claim diagnosis lives here, not in the backend.** Every `CamOps` implementation hits the
/// same exclusive-claim failure for the same reason — it is a property of USB, not of how the
/// driver talks to libgphoto2 — so the CLI fallback of SDD §5.3.3 gets the diagnosis for free,
/// and CI can test it without either backend.
fn open(
    factory: &dyn CamOpsFactory,
    ops: &mut Option<Box<dyn CamOps>>,
) -> Result<RawIdentity, DeviceError> {
    if ops.is_none() {
        *ops = Some(factory.build()?);
    }
    let opened = ops.as_mut().expect("just built").open();
    if opened.is_err() {
        // A failed open leaves nothing usable behind. Dropping it here means the next attempt
        // gets a *fresh* context — which M2-T01 proved is the only thing that recovers, since the
        // stale handle never does however many times it is retried.
        *ops = None;
    }
    opened.map_err(diagnose)
}

/// Replaces a bare claim failure with one an operator can act on.
///
/// Only `Transport` errors whose text names a claim are touched: everything else already says
/// what happened, and rewriting a message that is fine is how the original cause gets lost.
fn diagnose(error: DeviceError) -> DeviceError {
    match error {
        DeviceError::Transport(detail) if gvfs::is_claim_failure(&detail) => {
            DeviceError::Transport(gvfs::explain_claim_failure(&detail, &gvfs::gvfs_root()))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use astroctl_core::config::CameraTimeouts;
    use astroctl_core::error::DeviceError;

    use super::super::mock::MockState;
    use super::{CamCmd, CameraLink, OpClass, CAMERA_THREAD_NAME};

    /// The example config's budgets (`config/field-node.example.yaml`).
    fn timeouts() -> CameraTimeouts {
        CameraTimeouts {
            config_seconds: 5,
            capture_extra_seconds: 30,
            download_seconds: 120,
        }
    }

    #[tokio::test]
    async fn every_blocking_call_runs_on_the_camera_thread_and_not_the_callers() {
        // The acceptance criterion, asserted rather than described: "all blocking gphoto2 calls
        // verifiably on the camera thread". The mock records `std::thread::current()` inside
        // each `CamOps` method, so this checks where the call *actually ran* — a span field
        // alone would only prove what the driver believes.
        let (state, factory) = MockState::new();
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");

        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect("opens");
        link.request(OpClass::Config, CamCmd::GetSettings)
            .await
            .expect("reads settings");
        link.request(OpClass::Config, CamCmd::GetBattery)
            .await
            .expect("reads battery");

        let calls = state.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        for call in &calls {
            assert_eq!(
                call.thread, CAMERA_THREAD_NAME,
                "`{}` ran on `{}`, not the camera thread",
                call.op, call.thread
            );
        }

        // And the negative half: the test's own thread is a different one, so the assertion
        // above is not trivially true of every thread in the process.
        let caller = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_owned();
        assert_ne!(caller, CAMERA_THREAD_NAME);
    }

    #[tokio::test]
    async fn one_context_means_one_queue_serviced_in_order() {
        let (state, factory) = MockState::new();
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");

        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect("opens");
        link.request(OpClass::Config, CamCmd::GetChoices)
            .await
            .expect("reads choices");
        link.request(OpClass::Config, CamCmd::GetStorage)
            .await
            .expect("reads storage");

        let ops: Vec<&str> = state.calls().iter().map(|call| call.op).collect();
        assert_eq!(ops, ["open", "read_choices", "storage"]);
    }

    #[tokio::test]
    async fn a_command_before_open_is_not_connected_rather_than_a_panic() {
        let (_state, factory) = MockState::new();
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");

        let result = link.request(OpClass::Config, CamCmd::GetSettings).await;
        assert!(
            matches!(result, Err(DeviceError::NotConnected)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn the_budget_that_bounds_an_operation_comes_from_configuration() {
        // One second is `config_seconds`' validated minimum, so this is the cheapest possible
        // demonstration that the number is read from config rather than compiled in.
        let one_second = CameraTimeouts {
            config_seconds: 1,
            ..timeouts()
        };
        assert_eq!(
            OpClass::Config.budget(&one_second),
            Duration::from_secs(1),
            "the budget must track `camera.timeouts.config_seconds`"
        );
        assert_eq!(OpClass::Connect.budget(&timeouts()), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn an_operation_over_budget_wedges_the_thread_and_the_link_goes_down() {
        // Real time, not `tokio::time::pause`: the reply travels from a plain OS thread the
        // runtime knows nothing about, so a paused clock would auto-advance past *every* budget
        // the moment the runtime went idle — including the ones that are supposed to be met.
        // One second is the price of testing this honestly.
        let (state, factory) = MockState::new();
        let one_second = CameraTimeouts {
            config_seconds: 1,
            ..timeouts()
        };
        // Longer than the budget, so `open` is still inside the C call when the budget expires —
        // which is exactly the state the wedge protocol exists for.
        state.block_calls_for(Duration::from_secs(3));

        let link = CameraLink::spawn(factory, one_second).expect("spawns");
        let result = link.request(OpClass::Connect, CamCmd::Open).await;

        match result {
            Err(DeviceError::Timeout(budget)) => assert_eq!(budget, Duration::from_secs(1)),
            other => panic!("expected a timeout at the configured budget, got {other:?}"),
        }

        // SDD §5.3.1: the facade drops the channel and abandons the thread. From the outside
        // that is a link which is no longer up...
        assert!(!link.is_up(), "a wedged link must not report itself usable");
        // ...and every subsequent command is refused immediately rather than queueing behind a
        // thread that is never coming back.
        let after = link.request(OpClass::Config, CamCmd::GetSettings).await;
        assert!(matches!(after, Err(DeviceError::NotConnected)), "{after:?}");
    }

    #[tokio::test]
    async fn a_clean_shutdown_releases_the_camera_on_the_camera_thread() {
        // The reason the ops object is built *inside* the thread: it must also be destroyed
        // there. A context dropped on another thread is libgphoto2 being used the one way it
        // forbids, and it would show up as an occasional crash rather than a test failure.
        let (state, factory) = MockState::new();
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");
        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect("opens");

        link.shutdown();

        assert_eq!(
            state.drop_threads(),
            vec![CAMERA_THREAD_NAME.to_owned()],
            "the camera context must be dropped on the camera thread"
        );
        assert!(!link.is_up());
    }

    #[tokio::test]
    async fn dropping_the_link_releases_the_camera_even_without_a_disconnect() {
        let (state, factory) = MockState::new();
        {
            let link = CameraLink::spawn(factory, timeouts()).expect("spawns");
            link.request(OpClass::Connect, CamCmd::Open)
                .await
                .expect("opens");
        }
        // Otherwise the next process to want the camera meets "Could not claim the USB device"
        // and goes looking for a gvfs mount that is not there.
        assert_eq!(state.drop_threads(), vec![CAMERA_THREAD_NAME.to_owned()]);
    }

    #[tokio::test]
    async fn a_failed_open_is_diagnosed_before_the_caller_ever_sees_it() {
        let (state, factory) = MockState::new();
        state.fail_open_with("Could not claim the USB device");
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");

        let error = link
            .request(OpClass::Connect, CamCmd::Open)
            .await
            .expect_err("the claim failed");

        let DeviceError::Transport(message) = error else {
            panic!("a claim failure is a transport error");
        };
        // libgphoto2's own text survives...
        assert!(
            message.contains("Could not claim the USB device"),
            "{message}"
        );
        // ...and the diagnosis has been attached. Which branch fires depends on whether this
        // machine happens to have a camera gvfs-mounted, so assert the property both branches
        // share: the operator is told what to do next, not just what failed.
        assert!(
            message.contains("gio mount -u") || message.contains("switched off"),
            "the message names no next step: {message}"
        );
    }

    #[tokio::test]
    async fn a_missing_camera_keeps_its_own_message_rather_than_a_gvfs_theory() {
        // The distinction M2-T01 established by pulling the cable. A driver that answered
        // "check for a gvfs mount" to an unplugged camera would send the operator to the wrong
        // place at the worst time.
        let (state, factory) = MockState::new();
        state.fail_open_with("Could not find the requested device on the USB port");
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");

        let error = link
            .request(OpClass::Connect, CamCmd::Open)
            .await
            .expect_err("no camera");
        let DeviceError::Transport(message) = error else {
            panic!("expected a transport error");
        };
        assert_eq!(
            message,
            "Could not find the requested device on the USB port"
        );
    }

    #[tokio::test]
    async fn a_failed_open_drops_the_context_so_the_retry_gets_a_fresh_one() {
        // M2-T01 retried a stale handle five times after a replug; all five failed with the same
        // error, and a *fresh* context connected in 108 ms. So a failed open must not leave the
        // old context in place to be retried.
        let (state, factory) = MockState::new();
        state.fail_open_with("Could not claim the USB device");
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");

        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect_err("first attempt fails");
        state.succeed_open();
        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect("the retry gets a fresh context and succeeds");

        assert_eq!(
            state.builds(),
            2,
            "the retry must build a new context, not reuse the failed one"
        );
    }

    #[tokio::test]
    async fn the_link_survives_a_close_and_can_be_opened_again() {
        // `Close` releases the camera but keeps the thread, so a reconnect costs no respawn.
        let (state, factory) = MockState::new();
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");

        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect("opens");
        link.request(OpClass::Config, CamCmd::Close)
            .await
            .expect("closes");
        assert!(link.is_up(), "closing the camera must not end the thread");

        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect("opens again");
        assert_eq!(state.builds(), 2);
        // Still one thread throughout — the whole point of keeping it.
        for call in state.calls() {
            assert_eq!(call.thread, CAMERA_THREAD_NAME);
        }
    }

    #[tokio::test]
    async fn closing_a_camera_that_is_not_open_is_ok() {
        let (_state, factory) = MockState::new();
        let link = CameraLink::spawn(factory, timeouts()).expect("spawns");
        link.request(OpClass::Config, CamCmd::Close)
            .await
            .expect("closing nothing is not an error");
    }

    #[tokio::test]
    async fn the_camera_thread_is_shared_by_every_command_from_every_task() {
        // PRF-04's structural claim: concurrent callers do not get concurrent contexts. Four
        // tasks, one thread, four calls.
        let (state, factory) = MockState::new();
        let link = Arc::new(CameraLink::spawn(factory, timeouts()).expect("spawns"));
        link.request(OpClass::Connect, CamCmd::Open)
            .await
            .expect("opens");

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let link = Arc::clone(&link);
            tasks.push(tokio::spawn(async move {
                link.request(OpClass::Config, CamCmd::GetBattery).await
            }));
        }
        for task in tasks {
            task.await.expect("task").expect("battery");
        }

        let threads: std::collections::BTreeSet<String> =
            state.calls().into_iter().map(|call| call.thread).collect();
        assert_eq!(
            threads,
            [CAMERA_THREAD_NAME.to_owned()].into_iter().collect(),
            "every call, from every task, on one thread"
        );
    }
}
