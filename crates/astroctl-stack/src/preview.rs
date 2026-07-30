//! Ingested frame → stub worker → `/ws/preview` — SDD §5.11.1, §5.12.4, §8.3(5).
//!
//! This is the stacking server's half of the loop the M1 demo is built on. A frame lands via
//! `/api/ingest`, the compute worker asinh-stretches it into a JPEG beside the raw (§5.12.4), and
//! the bytes go out on a binary socket the field node proxies to the operator's browser (ADR-07).
//!
//! # Why a second socket here too
//!
//! §5.11.1 gives this node `/ws` for JSON status and `/ws/preview` for "binary JPEG previews
//! only", and says in the same row that it mirrors the field node's `/ws/liveview` split. The
//! reason is §8.3(5)'s and it is about TCP, not code structure: two streams multiplexed onto one
//! connection share one retransmit queue, so a 500 KB JPEG that needs resending would hold
//! everything behind it. `/ws` on this node is **not** built here — nothing subscribes to it. The
//! field node republishes `stack.status` from `/api/system/health` and `/api/stacking/stats`
//! (§4.3, USB-06), so a JSON socket on this side would be a socket that never sends, which is
//! precisely what `api.rs` declined to declare before there was something to push.
//!
//! # Authentication is the bearer token, and there is no ticket store
//!
//! §4.5 invents single-use tickets for one reason: the browser `WebSocket` constructor cannot set
//! an `Authorization` header. That reason does not exist on this node. ADR-07 makes the field node
//! the operator's only origin, so the sole client of this socket is the field node's proxy — not a
//! browser — and §4.5 says so directly: "The field node connecting to the stack node's WebSocket
//! (M1-T14's preview proxy) is not a browser and uses the ordinary `Authorization` header; it has
//! no need of a ticket." So `/ws/preview` sits under the same bearer middleware as every other
//! route on this node and this module has no auth code at all.
//!
//! # The queue is depth 1 with replace semantics
//!
//! §5.8.3's rule for image sockets, applied to the job side: a burst of ingests previews the
//! newest frame and skips the rest. Both halves are deliberate. Previews are ephemeral and
//! regenerable; the *frames* are what matter, and they are durable and journalled before this
//! module ever hears about them (§5.11.2, and the ack contract that makes REL-13 safe). A queue
//! that instead ran every job would put an unbounded backlog of stretches between the operator and
//! the frame they are actually waiting for — the newest one.
//!
//! # A failed preview must never fail an ingest
//!
//! §5.12.3: "Capture on the field node is unaffected by any of this by construction: the worker
//! sits behind ingest, and ingest acks on durability, not on processing." [`PreviewQueue::offer`]
//! is therefore infallible and non-blocking, called after the journal row is written, and returns
//! `()`. The handler cannot await a worker, cannot see it fail, and has nothing to roll back.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use astroctl_core::bus::EventBus;
use astroctl_core::error::ErrorCode;
use astroctl_core::event::{Alert, WorkerState};
use astroctl_core::image_frame::{encode, FrameKind};
use astroctl_ipc::protocol::JobKind;
use astroctl_ipc::supervisor::{JobFailure, WorkerHandle, WorkerStatus};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use chrono::{DateTime, Utc};
use futures_util::stream::StreamExt;
use futures_util::SinkExt;
use tokio::sync::{watch, Notify};

use crate::api::AppState;

/// `alert` code for previews that are failing on a stack node that is otherwise healthy.
///
/// Distinct from the supervisor's own `WORKER_*` codes: those say the worker process is in
/// trouble, this says the worker is fine and rejecting the work — a frame it cannot read, a
/// dependency missing from `workers/requirements.txt`. The operator's next move differs.
pub const ALERT_PREVIEW_FAILED: &str = "STACK_PREVIEW_FAILED";

/// The largest preview JPEG this node will read back off disk and fan out.
///
/// The stub worker's own `max_dimension` default keeps a preview well under a megabyte, so this
/// is a guard against a worker returning a path to something else entirely rather than a tuning
/// knob — hence no config key for it (PRD §8.2 has none, and inventing one would be a silent
/// schema extension).
const MAX_PREVIEW_BYTES: u64 = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// The fan-out
// ---------------------------------------------------------------------------------------------

/// The node's single preview fan-out.
///
/// A `watch` channel, for the reason §5.8.3 gives about the field node's twin: the depth-1 replace
/// queue *is* the channel rather than a policy layered over one. Every subscriber is an
/// independent cursor over a single slot, so a client stalled on a write falls behind in time
/// rather than in queue depth, and the publisher can never block.
///
/// Holds no [`EventBus`] handle, and must not acquire one. A bus handle is a broadcast *sender*,
/// so one held by a connection task that outlives `drop(bus)` keeps the session log's subscriber
/// open and costs the flush its whole timeout — the constraint the field node's `ws` and
/// `liveview` modules both document at length.
///
/// `astroctl-hal`'s `FrameStream` is the same construct and is deliberately *not* used: the HAL is
/// the hardware abstraction, and a stacking server that has no hardware should not depend on one
/// to get a channel.
#[derive(Debug)]
pub struct PreviewHub {
    tx: watch::Sender<Option<Arc<[u8]>>>,
    /// When the last preview was published, for `/api/stacking/stats` (§5.11.1) and the
    /// `stack.status` the field node republishes from it (§4.3, USB-06).
    last_preview_ts: Mutex<Option<DateTime<Utc>>>,
}

impl PreviewHub {
    /// An idle hub with nothing in the slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tx: watch::channel(None).0,
            last_preview_ts: Mutex::new(None),
        }
    }

    /// A cursor for one client, positioned at the present.
    fn subscribe(&self) -> watch::Receiver<Option<Arc<[u8]>>> {
        let mut rx = self.tx.subscribe();
        rx.mark_unchanged();
        rx
    }

    /// The frame currently in the slot, if any.
    ///
    /// Sent the moment a client connects. A panel that has just reconnected — through a proxy that
    /// itself just reconnected (REL-10) — shows the last preview immediately rather than an empty
    /// rectangle until the next frame is ingested, which on a 5-minute sub could be five minutes.
    fn latest(&self) -> Option<Arc<[u8]>> {
        self.tx.borrow().clone()
    }

    /// Publish a preview to every attached client.
    pub fn publish(&self, jpeg: &[u8], frame_id: &str) {
        let now = Utc::now();
        // Encoded once, not once per client — the whole reason the envelope's encoder takes bytes
        // and returns a shared `Arc<[u8]>`.
        let frame = encode(FrameKind::Preview, jpeg, now, Some(frame_id));
        *self.locked() = Some(now);
        drop(self.tx.send_replace(Some(frame)));
    }

    /// When the last preview was produced; `None` on a node that has produced none.
    #[must_use]
    pub fn last_preview_ts(&self) -> Option<DateTime<Utc>> {
        *self.locked()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Option<DateTime<Utc>>> {
        self.last_preview_ts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for PreviewHub {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------
// The queue
// ---------------------------------------------------------------------------------------------

/// One frame to preview.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewJob {
    session_id: String,
    frame_id: String,
    /// Absolute path on *this* node's filesystem. ADR-13's channel passes frames by path and
    /// never carries pixels, so this is what the worker receives.
    path: PathBuf,
}

/// The depth-1 replace slot between the ingest handler and the worker.
///
/// Structurally the field node's `DecodeQueue`: a slot, a `Notify`, and a producer that never
/// blocks. The producer here is an *HTTP handler on the ack path*, which makes "never blocks"
/// load-bearing rather than merely tidy — a queue that applied backpressure would make ingest
/// latency depend on how long a stretch takes, and ingest is what REL-13 lets the field node
/// delete its only copy on the strength of.
#[derive(Debug)]
pub struct PreviewQueue {
    pending: Mutex<Option<PreviewJob>>,
    notify: Notify,
    closed: Mutex<bool>,
}

impl PreviewQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            closed: Mutex::new(false),
        }
    }

    /// Offer a frame, replacing any job that has not started.
    ///
    /// Infallible and non-blocking — see the module docs on why an ingest cannot be failed by
    /// this. The displaced id is logged rather than returned: a preview that never appears for a
    /// frame the operator watched arrive is otherwise a silent absence, and §5.8.3 chose it.
    pub fn offer(&self, session_id: &str, frame_id: &str, path: PathBuf) {
        let job = PreviewJob {
            session_id: session_id.to_owned(),
            frame_id: frame_id.to_owned(),
            path,
        };
        let displaced = self.lock().replace(job).map(|previous| previous.frame_id);
        self.notify.notify_one();

        if let Some(displaced) = displaced {
            // Not a warning: it is the designed behaviour under a burst, and the displaced frame
            // is on disk, journalled and acked either way.
            tracing::info!(
                displaced = %displaced,
                replaced_by = %frame_id,
                "a newer frame replaced an unstarted preview job (SDD §5.8.3 depth-1 replace)"
            );
        }
    }

    /// Take the next job, waiting for one. `None` once closed and empty.
    async fn take(&self) -> Option<PreviewJob> {
        loop {
            // The future is created before the slot is inspected. The other order loses a
            // notification delivered in between, and the worker then sleeps until the *next*
            // ingest, having skipped one it was told about.
            let notified = self.notify.notified();
            {
                if let Some(job) = self.lock().take() {
                    return Some(job);
                }
                if *self.closed_lock() {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn close(&self) {
        *self.closed_lock() = true;
        self.notify.notify_one();
    }

    /// Take whatever is in the slot without waiting — the path a job would be dispatched at.
    ///
    /// Test-only, and it exists because the alternative is worse. Asserting that ingest queues a
    /// preview otherwise means running a real Python worker inside a handler test, which would
    /// make the ingest suite depend on an interpreter and on `workers/requirements.txt`. This
    /// reads the seam the handler actually writes to, which is the thing under test.
    #[cfg(test)]
    pub fn take_for_test(&self) -> Option<PathBuf> {
        self.lock().take().map(|job| job.path)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<PreviewJob>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn closed_lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for PreviewQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------------------------

/// The task that turns ingested frames into previews, and the handle shutdown stops it with.
///
/// **Owns the only [`WorkerHandle`]**, which is what makes shutdown work: the supervisor loop ends
/// when its last handle is dropped (`astroctl-ipc`'s own contract), and the supervisor task holds
/// an `EventBus` clone for its alerts. Dropping the handle inside [`PreviewPipeline::abort`] —
/// which `main` calls before `drop(bus)` — is therefore the step that lets the event log flush
/// rather than wait out its timeout.
#[derive(Debug)]
pub struct PreviewPipeline {
    worker: tokio::task::JoinHandle<()>,
    queue: Arc<PreviewQueue>,
}

impl PreviewPipeline {
    /// Stop the pipeline and release the worker supervisor.
    ///
    /// The task holds an [`EventBus`] handle — it publishes [`ALERT_PREVIEW_FAILED`] — so it must
    /// end before `drop(bus)` for the same reason every other publishing task on both nodes does.
    pub async fn abort(self) {
        self.queue.close();
        self.worker.abort();
        // Joining is what drops the task's `EventBus` and its `WorkerHandle`. Awaiting an aborted
        // task returns promptly with a cancellation error; the `let _` is that error.
        let _ = self.worker.await;
    }
}

/// Start the preview pipeline.
///
/// One task, not the field node's two: there is no event subscription on this side. The producer
/// is the ingest handler calling [`PreviewQueue::offer`] directly, so there is no bus reader that
/// a slow job could make lag.
#[must_use]
pub fn spawn(
    queue: Arc<PreviewQueue>,
    hub: Arc<PreviewHub>,
    workers: WorkerHandle,
    bus: EventBus,
) -> PreviewPipeline {
    let worker = tokio::spawn(run(Arc::clone(&queue), hub, workers, bus));
    PreviewPipeline { worker, queue }
}

async fn run(
    queue: Arc<PreviewQueue>,
    hub: Arc<PreviewHub>,
    workers: WorkerHandle,
    bus: EventBus,
) {
    // Edge-triggered, exactly as ingest's REL-12 refusal is: a field node uploading all night
    // against a worker whose dependencies are missing would otherwise produce one alert per frame,
    // and an alert per frame is an alert the operator learns to ignore (SDD §5.10.4).
    let failing = AtomicBool::new(false);

    while let Some(job) = queue.take().await {
        render_one(&job, &hub, &workers, &bus, &failing).await;
    }
}

/// One frame's preview, from the worker's answer to the operator's screen.
async fn render_one(
    job: &PreviewJob,
    hub: &PreviewHub,
    workers: &WorkerHandle,
    bus: &EventBus,
    failing: &AtomicBool,
) {
    let started = std::time::Instant::now();

    // Params left empty: §5.9's M1 row is "**No knobs** — the stub does no stacking, so there is
    // nothing to tune", so the worker's own defaults are the whole configuration. Sending an empty
    // object rather than omitting the field keeps the frame shape stable for 2b, which is where
    // IPP-07's method/rejection/stretch arrive.
    let answer = workers
        .submit(
            JobKind::Preview,
            serde_json::json!({}),
            vec![job.path.clone()],
        )
        .await;

    let data = match answer {
        Ok(data) => data,
        Err(failure) => {
            report_failure(job, &failure, bus, failing);
            return;
        }
    };

    let Some(preview_path) = data.get("preview_path").and_then(|v| v.as_str()) else {
        tracing::error!(
            frame_id = %job.frame_id,
            "the worker reported a preview with no `preview_path`"
        );
        return;
    };

    let jpeg = match read_preview(std::path::Path::new(preview_path)).await {
        Ok(jpeg) => jpeg,
        Err(error) => {
            // The worker says it wrote a file this node cannot read. Loud, and not fatal: the
            // frame itself is durable and the next ingest has every chance of working.
            tracing::error!(
                frame_id = %job.frame_id,
                path = %preview_path,
                %error,
                "the preview the worker reported could not be read back"
            );
            return;
        }
    };

    hub.publish(&jpeg, &job.frame_id);
    recovered(bus, failing);

    tracing::info!(
        session_id = %job.session_id,
        frame_id = %job.frame_id,
        width = data.get("width").and_then(serde_json::Value::as_u64),
        height = data.get("height").and_then(serde_json::Value::as_u64),
        bytes = jpeg.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "preview ready"
    );
}

/// Read the worker's output, refusing anything implausibly large before it is in memory.
async fn read_preview(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let size = tokio::fs::metadata(path).await?.len();
    if size > MAX_PREVIEW_BYTES {
        return Err(std::io::Error::other(format!(
            "{} is {size} bytes, over the {MAX_PREVIEW_BYTES}-byte preview ceiling",
            path.display()
        )));
    }
    tokio::fs::read(path).await
}

/// Announce, once per transition, that previews are failing.
fn report_failure(job: &PreviewJob, failure: &JobFailure, bus: &EventBus, failing: &AtomicBool) {
    // `JobFailure::code()` is M1-T13's mapping onto the closed §4.2 vocabulary, so the operator
    // sees "the worker crashed" or "the worker is unavailable" rather than everything collapsing
    // onto INTERNAL. `Cancelled` is shutdown surrendering the job, not a failure to report.
    let code = failure.code();
    if code == ErrorCode::Cancelled {
        tracing::debug!(frame_id = %job.frame_id, "a preview job was surrendered to shutdown");
        return;
    }

    tracing::error!(
        session_id = %job.session_id,
        frame_id = %job.frame_id,
        code = code.as_str(),
        %failure,
        "the frame could not be previewed"
    );

    if !failing.swap(true, Ordering::Relaxed) {
        bus.publish(Alert::warning(
            ALERT_PREVIEW_FAILED,
            format!(
                "The stacking server received frame {} but could not preview it: {failure}. \
                 Frames are still being stored — this affects the preview only.",
                job.frame_id
            ),
        ));
    }
}

/// Announce, once, that previews are working again.
fn recovered(bus: &EventBus, failing: &AtomicBool) {
    if failing.swap(false, Ordering::Relaxed) {
        bus.publish(Alert::info(
            ALERT_PREVIEW_FAILED,
            "The stacking server is producing previews again.".to_owned(),
        ));
    }
}

// ---------------------------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------------------------

/// `GET /ws/preview` — SDD §5.11.1, authenticated by the bearer middleware like every other route
/// on this node (see the module docs on why there is no ticket here).
pub async fn upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    // Subscribe *before* the upgrade completes, and hand the connection nothing but the cursor and
    // the frame already in the slot — never the `AppState` they came from. `AppState` holds an
    // `EventBus`, and a connection holding one keeps the session log's subscriber open past
    // `drop(bus)`. This is the one thing about this handler a refactor must not lose.
    let frames = state.previews.subscribe();
    let latest = state.previews.latest();

    upgrade.on_upgrade(move |socket| serve(socket, frames, latest))
}

/// Drive one client for the life of its connection.
async fn serve(
    socket: WebSocket,
    frames: watch::Receiver<Option<Arc<[u8]>>>,
    latest: Option<Arc<[u8]>>,
) {
    let (sink, stream) = socket.split();
    let writer = tokio::spawn(write(frames, latest, sink));
    let reader = tokio::spawn(read(stream));

    // The reader finishing is the one event that ends a connection from this side.
    let _ = reader.await;
    // Aborted rather than joined: the writer may be parked inside a `send` to a peer that has
    // stopped reading — on this socket the normal way a bad link fails — and waiting for that to
    // time out would leak a task per disconnect for as long as the TCP stack takes to give up.
    writer.abort();
}

/// Hub → socket. The only task that ever waits on a write.
async fn write(
    mut frames: watch::Receiver<Option<Arc<[u8]>>>,
    latest: Option<Arc<[u8]>>,
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
) {
    if let Some(frame) = latest {
        if sink
            .send(Message::Binary(frame.to_vec().into()))
            .await
            .is_err()
        {
            return;
        }
    }

    // `changed()` resolves `Err` only when every sender is gone, which for this hub means the
    // node is shutting down. Every frame missed while a send was in flight has already been
    // replaced in the slot — that is the depth-1 replace, and it is why a client on a saturated
    // link falls behind in time rather than in queue depth.
    while frames.changed().await.is_ok() {
        let frame = frames.borrow_and_update().clone();
        let Some(frame) = frame else { continue };
        if sink
            .send(Message::Binary(frame.to_vec().into()))
            .await
            .is_err()
        {
            break;
        }
    }
    let _ = sink.close().await;
}

/// Socket → nothing. This socket is one-way; the reader is how the node learns the client left.
async fn read(mut stream: futures_util::stream::SplitStream<WebSocket>) {
    while let Some(message) = stream.next().await {
        let Ok(message) = message else { break };
        match message {
            Message::Close(_) => break,
            // Ignored rather than fatal: a client newer than this node is normal after an upgrade,
            // and dropping the operator's image link over a frame we do not read would turn a
            // forward-compatible client into a reconnect loop.
            Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------------------------

/// The `worker` object of SDD §5.11.1, from the supervisor's counters.
///
/// [`WorkerState::Stopped`] is the honest idle state on a node whose workers start on demand
/// (§5.12.3) — never `Ready`, which would claim a process that is not running.
#[must_use]
pub fn worker_health(workers: Option<&WorkerHandle>) -> Option<WorkerStatus> {
    workers.map(WorkerHandle::status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(frame_id: &str) -> PreviewJob {
        PreviewJob {
            session_id: "2026-07-29_m31".to_owned(),
            frame_id: frame_id.to_owned(),
            path: PathBuf::from(format!("/frames/{frame_id}.fits")),
        }
    }

    /// §5.8.3's replace semantics: a burst previews the newest frame, and the ones it displaced
    /// are simply not previewed. They are on disk regardless — that is what makes this safe.
    #[tokio::test]
    async fn a_burst_of_ingests_previews_only_the_newest() {
        let queue = PreviewQueue::new();
        for id in ["light_00001", "light_00002", "light_00003"] {
            queue.offer("2026-07-29_m31", id, PathBuf::from(format!("/frames/{id}.fits")));
        }

        assert_eq!(queue.take().await, Some(job("light_00003")));

        // And nothing is left behind it: the queue is a slot, not a backlog.
        queue.close();
        assert_eq!(queue.take().await, None);
    }

    /// A job offered while the worker is between jobs must not be lost to the gap between
    /// "notified" and "inspect the slot".
    #[tokio::test]
    async fn a_job_offered_before_the_worker_waits_is_still_taken() {
        let queue = Arc::new(PreviewQueue::new());
        queue.offer("s", "light_00001", PathBuf::from("/frames/a.fits"));

        let taken = tokio::time::timeout(std::time::Duration::from_secs(1), queue.take())
            .await
            .expect("take does not block on an already-offered job");
        assert_eq!(taken.map(|j| j.frame_id), Some("light_00001".to_owned()));
    }

    /// The hub's slot is what a late subscriber finds, which is what makes a reconnecting panel
    /// show the last preview instead of an empty rectangle.
    #[test]
    fn a_client_connecting_late_finds_the_last_preview_in_the_slot() {
        let hub = PreviewHub::new();
        assert!(hub.latest().is_none());
        assert!(hub.last_preview_ts().is_none());

        hub.publish(b"\xff\xd8jpeg", "light_00042");

        let frame = hub.latest().expect("the slot holds the published frame");
        assert_eq!(&frame[..4], astroctl_core::image_frame::MAGIC);
        assert_eq!(frame[5], FrameKind::Preview.as_byte());
        assert!(hub.last_preview_ts().is_some());
    }

    /// A subscriber is positioned at the present: it waits for the *next* frame rather than
    /// replaying the one already in the slot, which the connect-time `latest()` has just sent.
    /// Without `mark_unchanged` every client would receive the same preview twice.
    #[tokio::test]
    async fn a_subscriber_does_not_replay_the_frame_it_was_already_handed() {
        let hub = PreviewHub::new();
        hub.publish(b"first", "light_00001");

        let mut cursor = hub.subscribe();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), cursor.changed())
                .await
                .is_err(),
            "a fresh cursor must not see the frame already in the slot"
        );

        hub.publish(b"second", "light_00002");
        assert!(cursor.changed().await.is_ok());
    }

    /// A worker that ran the job and rejected it is reported under its own §4.2 code, and the
    /// alert fires once rather than once per frame — a field node draining a backlog against a
    /// worker missing a dependency would otherwise produce one alert per upload.
    #[tokio::test]
    async fn a_failing_worker_alerts_once_and_recovers_once() {
        use astroctl_core::bus::{EventBus, Recv};
        use astroctl_core::event::Topic;
        use astroctl_ipc::protocol::WorkerError;

        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let failing = AtomicBool::new(false);
        let failure = JobFailure::Worker(WorkerError {
            code: ErrorCode::NotFound,
            message: "no such frame".to_owned(),
        });

        report_failure(&job("light_00001"), &failure, &bus, &failing);
        report_failure(&job("light_00002"), &failure, &bus, &failing);
        recovered(&bus, &failing);
        recovered(&bus, &failing);

        let mut alerts = Vec::new();
        while let Ok(Recv::Event(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), events.recv()).await
        {
            if event.topic == Topic::Alert {
                alerts.push(event.data["severity"].as_str().unwrap_or_default().to_owned());
            }
        }
        assert_eq!(
            alerts,
            vec!["warning", "info"],
            "one alert per transition, not per frame (SDD §5.10.4)"
        );
    }

    /// Shutdown surrendering a job is a cancellation, not a fault: it must not put a warning in
    /// the operator's alert strip on the way down.
    #[tokio::test]
    async fn a_job_surrendered_to_shutdown_is_not_an_alert() {
        use astroctl_core::bus::{EventBus, Recv};

        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let failing = AtomicBool::new(false);

        report_failure(&job("light_00001"), &JobFailure::Stopped, &bus, &failing);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                .await
                .map_or(true, |r| !matches!(r, Recv::Event(_))),
            "shutdown must not alert"
        );
        assert!(!failing.load(Ordering::Relaxed));
    }

    /// The idle state of a node whose workers start on demand is `Stopped`, and health must say
    /// so rather than claiming a `Ready` process that does not exist (§5.12.3).
    #[test]
    fn a_node_with_no_worker_started_reports_stopped_not_ready() {
        assert_eq!(WorkerStatus::default().state, WorkerState::Stopped);
        assert_ne!(
            WorkerStatus::default().state,
            WorkerState::Ready,
            "an on-demand supervisor with no worker up must never claim `ready`"
        );
        // And a node with no supervisor at all reports nothing, which is a different fact again.
        assert!(worker_health(None).is_none());
    }
}

/// `/ws/preview` over a real socket.
///
/// Everything here needs a *completed* upgrade, which `tower`'s `oneshot` cannot produce — it
/// drives a `Service` and never hands the connection back. These bind an ephemeral port and drive
/// the assembled node with `tokio-tungstenite` as the client, which is also what makes the
/// handshake a real test rather than one library agreeing with itself.
#[cfg(test)]
mod e2e {
    use std::net::SocketAddr;
    use std::time::Duration;

    use astroctl_core::image_frame::{HEADER_LEN, MAGIC};
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    use super::*;
    use crate::test_support::{app_for, TestNode};

    const TOKEN: &str = "s3cret";

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    struct Node {
        addr: SocketAddr,
        state: AppState,
        _dir: TestNode,
    }

    impl Node {
        async fn start() -> Self {
            let dir = TestNode::authenticated(TOKEN);
            let app = app_for(&dir).await;
            let state = app.state.clone();

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("binds an ephemeral port");
            let addr = listener.local_addr().expect("has a local address");
            let router = app.router;
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Self {
                addr,
                state,
                _dir: dir,
            }
        }

        /// Open `/ws/preview` with a bearer header — the field node's posture, not a browser's.
        async fn open(&self, token: Option<&str>) -> Result<Socket, String> {
            let mut request = format!("ws://{}/ws/preview", self.addr)
                .into_client_request()
                .expect("the url is a valid upgrade request");
            if let Some(token) = token {
                request.headers_mut().insert(
                    axum::http::header::AUTHORIZATION,
                    format!("Bearer {token}")
                        .parse()
                        .expect("a bearer header value"),
                );
            }
            match tokio_tungstenite::connect_async(request).await {
                Ok((socket, _)) => Ok(socket),
                Err(error) => Err(error.to_string()),
            }
        }
    }

    /// Read one binary frame and return its `frame_id` and JPEG bytes.
    async fn next_frame(socket: &mut Socket) -> (String, Vec<u8>) {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("a frame arrives within the timeout")
                .expect("the socket is open")
                .expect("the frame is readable");
            match message {
                ClientMessage::Binary(bytes) => {
                    assert_eq!(&bytes[..4], MAGIC, "every frame carries the envelope magic");
                    assert_eq!(bytes[5], FrameKind::Preview.as_byte(), "previews only");
                    let meta_len = usize::from(u16::from_be_bytes([bytes[6], bytes[7]]));
                    let meta: serde_json::Value =
                        serde_json::from_slice(&bytes[HEADER_LEN..HEADER_LEN + meta_len])
                            .expect("meta JSON");
                    return (
                        meta["frame_id"].as_str().unwrap_or_default().to_owned(),
                        bytes[HEADER_LEN + meta_len..].to_vec(),
                    );
                }
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => {}
                other => panic!("unexpected frame on the preview socket: {other:?}"),
            }
        }
    }

    /// §4.5's posture on this node: the bearer token authenticates the upgrade, and there is no
    /// ticket to present. A socket that accepted an unauthenticated upgrade would be a hole the
    /// rest of the node's middleware does not have.
    #[tokio::test]
    async fn the_upgrade_is_authenticated_by_the_bearer_token() {
        let node = Node::start().await;

        let refused = node.open(None).await.expect_err("no token is refused");
        assert!(
            refused.contains("401"),
            "an unauthenticated upgrade must be refused: {refused}"
        );

        let wrong = node
            .open(Some("wrong"))
            .await
            .expect_err("a wrong token is refused");
        assert!(wrong.contains("401"), "{wrong}");

        node.open(Some(TOKEN))
            .await
            .expect("the operator's token opens the socket");
    }

    /// A client that connects after a preview was produced gets it immediately. Without this a
    /// panel that reconnected through a restarted proxy would show an empty rectangle until the
    /// *next* frame is ingested — which on a five-minute sub is five minutes of looking at
    /// nothing while the node has the picture in hand.
    #[tokio::test]
    async fn a_client_connecting_late_is_sent_the_frame_already_in_the_slot() {
        let node = Node::start().await;
        node.state.previews.publish(b"\xff\xd8already", "light_00007");

        let mut socket = node.open(Some(TOKEN)).await.expect("the socket opens");
        let (frame_id, jpeg) = next_frame(&mut socket).await;
        assert_eq!(frame_id, "light_00007");
        assert_eq!(jpeg, b"\xff\xd8already");
    }

    /// Two clients, one publish, two deliveries — and neither is the connect-time frame twice.
    #[tokio::test]
    async fn every_attached_client_receives_each_new_preview_once() {
        let node = Node::start().await;
        let mut first = node.open(Some(TOKEN)).await.expect("the first socket opens");
        let mut second = node.open(Some(TOKEN)).await.expect("the second socket opens");

        node.state.previews.publish(b"\xff\xd8one", "light_00001");
        assert_eq!(next_frame(&mut first).await.0, "light_00001");
        assert_eq!(next_frame(&mut second).await.0, "light_00001");

        node.state.previews.publish(b"\xff\xd8two", "light_00002");
        assert_eq!(next_frame(&mut first).await.0, "light_00002");
        assert_eq!(next_frame(&mut second).await.0, "light_00002");
    }

    /// The depth-1 replace of §5.8.3, end to end: a client that was not reading through a burst
    /// wakes to the newest frame, not to a backlog of stale ones. This is what stops a phone on a
    /// bad link from accumulating previews it will never catch up on.
    #[tokio::test]
    async fn a_client_that_missed_a_burst_wakes_to_the_newest_frame() {
        let node = Node::start().await;
        let mut socket = node.open(Some(TOKEN)).await.expect("the socket opens");

        for id in ["light_00001", "light_00002", "light_00003"] {
            node.state.previews.publish(id.as_bytes(), id);
        }

        let (frame_id, _) = next_frame(&mut socket).await;
        assert_eq!(
            frame_id, "light_00003",
            "the slot holds only the newest; the ones it replaced are simply not shown"
        );
    }
}
