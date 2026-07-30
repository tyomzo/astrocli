//! The `/ws/liveview` socket and the preview pipeline — SDD §5.7, §5.8.3, §8.3(5).
//!
//! The second socket exists for one reason: **a 500 KB JPEG must never head-of-line-block a stop
//! command or a position update** (§8.3(5)). That is a statement about TCP, not about code
//! structure — two streams multiplexed onto one connection share one retransmit queue, and no
//! amount of care on this side changes it. So image bytes get their own connection, and the only
//! thing this module owes the rest of the node is never to make control traffic wait on it.
//!
//! # Depth-1 replace is the channel, not a policy on top of one
//!
//! §5.8.3 asks for "a depth-1 replace queue — only the newest frame is ever in flight" per client.
//! That is precisely [`astroctl_hal::stream::FrameStream`]: a `watch` channel where every
//! subscriber is an independent cursor over a single slot. A client stalled on a write does not
//! accumulate a backlog, it simply misses the frames that were replaced while it was away, and
//! wakes to the newest one. Writing a bounded queue with eviction on top would be a second
//! mechanism doing the same job with more places to be wrong — and, unlike `/ws`'s [`Outbox`],
//! there is nothing here that must *not* be dropped. Every frame on this socket is
//! self-superseding; that is what makes it the socket that may drop things.
//!
//! [`Outbox`]: crate::ws
//!
//! # Two tasks per client, not three
//!
//! `/ws` needs a pump between the bus and the socket because the bus is shared and a client that
//! stops reading it makes *everyone* lag (see `crate::ws`). The `watch` channel here has no such
//! failure: a slow cursor costs the publisher nothing. So there is a writer and a reader, and the
//! reader exists only to notice the client leaving.
//!
//! # The frame envelope, and why the socket is not raw JPEG
//!
//! §5.8.1 calls this socket "binary JPEG frames only (live view + previews)". Two *kinds* of
//! image therefore share one socket, and a client that cannot tell them apart cannot use either:
//! a preview the operator asked to keep looking at would be overwritten by the next live-view
//! frame 200 ms later. Correlating with `capture.progress: preview_ready` on `/ws` does not fix
//! it — the two sockets have no ordering between them, which is the entire point of separating
//! them.
//!
//! Each binary frame therefore carries a fixed 8-byte header and a small JSON metadata block
//! before the JPEG. The layout, and the encoder, are [`astroctl_core::image_frame`] — M1-T14 gave
//! the stacking server's `/ws/preview` the same envelope, and one wire format the PWA decodes
//! with one function must not be two encoders that can drift.
//!
//! # Expected gaps are not stalls (§5.7, §5.3.1)
//!
//! The camera's live-view stream pauses for the whole of every exposure — the simulator's loop
//! `try_acquire`s the sensor permit and skips rather than queueing, and a real body has no
//! choice. Nothing in this module treats that as a fault: no reconnect, no alert, no teardown.
//! The distinction between "idle because the camera is busy" and "idle because the camera
//! stopped answering" is not available *here* at all — it is `capture.progress` on `/ws`, and
//! the panel is where the two are joined. Inventing a stream-level heartbeat to answer it would
//! produce exactly the spurious alert on every capture that §5.7 warns against.

use std::sync::{Arc, Mutex};

use astroctl_core::bus::{EventBus, EventSubscriber, Recv};
use astroctl_core::error::{ApiError, ErrorCode};
use astroctl_core::event::CaptureProgress;
use astroctl_core::image_frame::{encode, FrameKind};
use astroctl_hal::stream::{FrameSink, FrameStream};
use astroctl_pipeline::PreviewParams;
use astroctl_session::FrameId;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use futures_util::stream::StreamExt;
use futures_util::SinkExt;
use tokio::sync::Notify;

use crate::api::{ApiFailure, AppState};
use crate::auth::unauthorized;
use crate::ticket::TicketRejection;

// ---------------------------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------------------------

/// The node's single live-view fan-out, plus the camera stream feeding it.
///
/// Holds no [`EventBus`] handle. That is the same shutdown requirement `crate::ws` documents at
/// length: a bus handle is a broadcast *sender*, and one held by a task that outlives `drop(bus)`
/// stalls the session log's flush for its whole timeout. The forwarding task here carries an
/// `Arc<LiveViewHub>` and a device stream, neither of which can publish.
#[derive(Debug)]
pub struct LiveViewHub {
    sink: FrameSink<Arc<[u8]>>,
    /// The task pumping the camera's stream into [`Self::sink`], when live view is running.
    ///
    /// A `std` mutex holding a `JoinHandle`: nothing is awaited under it (`await_holding_lock` is
    /// denied workspace-wide), the handle is taken out before anything is awaited on it.
    stream: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LiveViewHub {
    /// An idle hub — no camera stream, no frames.
    #[must_use]
    pub fn new() -> Self {
        let (sink, _stream) = FrameStream::channel();
        Self {
            sink,
            stream: Mutex::new(None),
        }
    }

    /// A cursor for one client, positioned at the present.
    #[must_use]
    pub fn subscribe(&self) -> FrameStream<Arc<[u8]>> {
        self.sink.subscribe()
    }

    /// The frame currently in the slot, if any.
    ///
    /// Sent to a client the moment it connects, which is this socket's equivalent of `/ws`'s
    /// connect snapshot (§5.8.3): a panel that has just reconnected shows the last preview
    /// immediately rather than an empty rectangle until the next capture — which, with live view
    /// stopped, could be the rest of the night.
    #[must_use]
    pub fn latest(&self) -> Option<Arc<[u8]>> {
        self.sink.subscribe().latest()
    }

    /// Publish a preview (CAM-06).
    pub fn publish_preview(&self, jpeg: &[u8], frame_id: &str) {
        self.sink
            .publish(encode(FrameKind::Preview, jpeg, Utc::now(), Some(frame_id)));
    }

    /// Publish a live-view frame (CAM-05).
    fn publish_live(&self, jpeg: &[u8], captured_at: DateTime<Utc>) {
        self.sink
            .publish(encode(FrameKind::Live, jpeg, captured_at, None));
    }

    /// Whether the camera stream is running.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.locked()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    /// Start forwarding the camera's live-view stream.
    ///
    /// Idempotent: a second start while one is running is success, not a second stream. The
    /// driver's own `live_view_stream` is idempotent the same way (two cursors on one stream,
    /// M1-T06), and two operators on two phones tapping *start* must not cost two sensor loops.
    ///
    /// # Errors
    ///
    /// Whatever the camera says — `NOT_CONNECTED` most often, which is the honest answer to
    /// starting live view on a camera nobody has connected.
    pub async fn start(
        self: &Arc<Self>,
        camera: &crate::camera::CameraFacade,
    ) -> Result<(), ApiError> {
        if self.is_streaming() {
            return Ok(());
        }
        let stream = camera.live_view_stream().await?;
        let task = tokio::spawn(forward(Arc::clone(self), stream));
        // Replacing rather than assuming the slot is empty: a task that ended on its own — the
        // camera disconnected, the driver's sink dropped — leaves a finished handle behind, and
        // `is_streaming` already treats that as not streaming.
        if let Some(previous) = self.locked().replace(task) {
            previous.abort();
        }
        Ok(())
    }

    /// Stop the stream, server-side.
    ///
    /// Both halves matter and the order does too. Aborting the forwarding task alone would leave
    /// the *driver* generating frames into a sink nobody reads — the orphaned work the M1-T09
    /// acceptance criterion names — and on a real body that is a USB transfer per frame for the
    /// rest of the night. Telling the driver alone would leave a task parked on a stream that
    /// ends only when the sink drops, which is a leak of a different shape.
    ///
    /// # Errors
    ///
    /// The camera's refusal, if it has one. The task is aborted either way: a stop that reported
    /// an error *and* left the stream running would be the worst of both.
    pub async fn stop(&self, camera: &crate::camera::CameraFacade) -> Result<(), ApiError> {
        let task = self.locked().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        camera.stop_live_view().await
    }

    /// Stop the forwarding task without touching the camera — the shutdown path.
    ///
    /// SDD §7 step 2 ("abort live view"). Separate from [`Self::stop`] because shutdown must not
    /// depend on the camera answering: a wedged body that never returns from `stop_live_view`
    /// would hold the whole shutdown sequence, and §7's later steps include flushing the night's
    /// event log.
    pub async fn abort(&self) {
        let task = self.locked().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Option<tokio::task::JoinHandle<()>>> {
        self.stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for LiveViewHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Camera stream → hub. Ends when the driver drops its sink (`stop_live_view`, disconnect).
async fn forward(
    hub: Arc<LiveViewHub>,
    mut stream: FrameStream<astroctl_hal::camera::LiveViewFrame>,
) {
    while let Some(frame) = stream.next_frame().await {
        hub.publish_live(&frame.jpeg, frame.captured_at);
    }
    // Not a warning. The stream ending is what `stop_live_view` and `disconnect` *are* — see the
    // driver's own note that a dropped sink is how it says "live view stopped" without a second
    // channel.
    tracing::debug!("the camera's live-view stream ended");
}

// ---------------------------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------------------------

/// `GET /ws/liveview` — SDD §5.8.1, ticket-authenticated exactly as `/ws` is (§4.5).
///
/// The same [`crate::ticket::TicketStore`] and the same single-use rule: a ticket buys one
/// upgrade, so a PWA opening both sockets fetches two. Sharing one would work on the first
/// connection and fail on every reconnect, which is the worst way for it to be wrong.
pub async fn upgrade(
    State(state): State<AppState>,
    Query(params): Query<crate::ws::UpgradeParams>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(rejection) = state.tickets.consume(params.ticket()) {
        // Logged, never returned — telling a caller which of "unknown", "expired" and "already
        // spent" their guess was is an oracle for guessing the next one.
        tracing::info!(reason = rejection.reason(), "liveview upgrade refused");
        return unauthorized(&ApiError::new(ErrorCode::Auth, TicketRejection::MESSAGE));
    }

    // Subscribe *before* the upgrade completes and hand the connection nothing but the cursor and
    // the frame already in the slot — never the `AppState` they came from. `AppState` holds an
    // `EventBus`, which is a broadcast sender, and a connection holding one keeps the session
    // log's subscriber open past `drop(bus)`; one attached phone would then cost the flush its
    // whole timeout and the tail of the night's log. `crate::ws::upgrade` documents the same
    // constraint, and it is the one thing about this handler that a refactor must not lose.
    let frames = state.liveview.subscribe();
    let latest = state.liveview.latest();

    upgrade.on_upgrade(move |socket| serve(socket, frames, latest))
}

/// Drive one client for the life of its connection.
async fn serve(socket: WebSocket, frames: FrameStream<Arc<[u8]>>, latest: Option<Arc<[u8]>>) {
    let (sink, stream) = socket.split();
    let writer = tokio::spawn(write(frames, latest, sink));
    let reader = tokio::spawn(read(stream));

    // As on `/ws`: the reader finishing is the one event that ends a connection from this side.
    let _ = reader.await;
    // Aborted rather than joined. The writer may be parked inside a `send` to a peer that has
    // stopped reading — which on this socket is the *normal* way a bad link fails — and waiting
    // for that send to time out would leak a task per disconnect for as long as the TCP stack
    // takes to give up.
    writer.abort();
}

/// Hub → socket. The only task that ever waits on a write.
async fn write(
    mut frames: FrameStream<Arc<[u8]>>,
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
    while let Some(frame) = frames.next_frame().await {
        // Every frame missed while this send was in flight has already been replaced in the
        // watch slot — that is the depth-1 replace of §5.8.3, and it is why a client on a
        // saturated link falls behind in *time* rather than in queue depth.
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
            // Control and subscription messages belong on `/ws`. Ignored rather than fatal, for
            // the reason that module gives: a client newer than this node is normal after an
            // upgrade, and dropping an operator's image link over a frame we do not read would
            // turn a forward-compatible client into a reconnect loop.
            Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The preview pipeline
// ---------------------------------------------------------------------------------------------

/// The decode queue of SDD §5.7: **depth 1, replace semantics**.
///
/// "If frames arrive faster than decode, only the newest is previewed (previews are ephemeral;
/// raw frames are what matters)." Both halves of that sentence are load-bearing. Only the newest
/// is *previewed* — a burst of five captures renders one preview, the fifth. And all five frames
/// stay durable regardless, because nothing in this file touches the frame store's write path;
/// the preview is derived from a frame that was durable before `frame.saved` was published at all
/// (SDD §5.5 note 3).
///
/// Structurally this is `crate::ws`'s `Outbox` with a queue depth of one and no discrete lane:
/// a slot, a `Notify`, and a producer that never blocks. The producer is the bus listener and it
/// must never block, because it is also the task reading the event bus — a decode that took long
/// enough to stall it would make this subscriber lag and start losing *events*.
#[derive(Debug)]
struct DecodeQueue {
    pending: Mutex<Option<PreviewJob>>,
    notify: Notify,
    /// Set once so the worker can exit when the bus closes.
    closed: Mutex<bool>,
}

/// One frame to render.
#[derive(Debug, Clone)]
struct PreviewJob {
    frame_id: String,
    path: std::path::PathBuf,
}

impl DecodeQueue {
    fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            closed: Mutex::new(false),
        }
    }

    /// Offer a job, replacing any that has not started.
    ///
    /// Returns the id that was displaced, purely so the caller can say so in a log line: a
    /// preview that never appears for a frame the operator watched being taken is otherwise a
    /// silent absence, and §5.7 chose it deliberately.
    fn offer(&self, job: PreviewJob) -> Option<String> {
        let displaced = self
            .lock()
            .replace(job)
            .map(|previous| previous.frame_id.clone());
        self.notify.notify_one();
        displaced
    }

    /// Take the next job, waiting for one. `None` once the queue is closed and empty.
    async fn take(&self) -> Option<PreviewJob> {
        loop {
            // The future is created before the slot is inspected — the other order loses a
            // notification delivered in between, and the worker then sleeps until the *next*
            // capture, having skipped one it was told about. `crate::ws`'s `Outbox::recv` has
            // the same comment for the same reason.
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

/// The two tasks that turn saved frames into previews, and the handle shutdown stops them with.
#[derive(Debug)]
pub struct PreviewPipeline {
    listener: tokio::task::JoinHandle<()>,
    worker: tokio::task::JoinHandle<()>,
}

impl PreviewPipeline {
    /// Stop both tasks.
    ///
    /// Called from SDD §7 step 2, and it must be, because the **worker holds an [`EventBus`]
    /// handle** — it publishes `capture.progress: preview_ready`. A bus handle is a broadcast
    /// sender, so a worker still alive at `drop(bus)` keeps the session log's subscriber open and
    /// costs the flush its whole timeout. The listener holds only a subscriber and would end on
    /// its own; it is aborted here anyway so there is one rule rather than two.
    pub async fn abort(self) {
        self.listener.abort();
        self.worker.abort();
        let _ = self.listener.await;
        let _ = self.worker.await;
    }
}

/// Start the preview pipeline.
///
/// Two tasks, split exactly where `crate::ws` splits its pump from its writer: the one that reads
/// the bus never does slow work, and the one that does slow work never reads the bus.
#[must_use]
pub fn spawn_previews(
    hub: Arc<LiveViewHub>,
    camera: Arc<crate::camera::CameraFacade>,
    bus: EventBus,
    events: EventSubscriber,
    params: PreviewParams,
) -> PreviewPipeline {
    let queue = Arc::new(DecodeQueue::new());
    let listener = tokio::spawn(listen(Arc::clone(&queue), events));
    let worker = tokio::spawn(render_loop(queue, hub, camera, bus, params));
    PreviewPipeline { listener, worker }
}

/// Bus → decode queue. Holds a subscriber and nothing that can publish.
async fn listen(queue: Arc<DecodeQueue>, mut events: EventSubscriber) {
    loop {
        match events.recv().await {
            Recv::Event(event) => {
                if event.topic != astroctl_core::event::Topic::FrameSaved {
                    continue;
                }
                let (Some(frame_id), Some(path)) = (
                    event.data.get("frame_id").and_then(|v| v.as_str()),
                    event.data.get("path").and_then(|v| v.as_str()),
                ) else {
                    tracing::warn!("a frame.saved event carried no frame_id or path");
                    continue;
                };
                let job = PreviewJob {
                    frame_id: frame_id.to_owned(),
                    path: std::path::PathBuf::from(path),
                };
                if let Some(displaced) = queue.offer(job) {
                    // §5.7's replace semantics, made visible. Not a warning: it is the designed
                    // behaviour under a burst, and the displaced frame is on disk either way.
                    tracing::info!(
                        displaced = %displaced,
                        replaced_by = %frame_id,
                        "a newer frame replaced an unstarted preview job (SDD §5.7 depth-1 replace)"
                    );
                }
            }
            // The snapshot store's reasoning applies: lagging means an intermediate `frame.saved`
            // was missed, and the preview of a frame two captures ago is not what the operator is
            // waiting for anyway. Nothing to resync.
            Recv::Lagged { skipped } => {
                tracing::debug!(skipped, "the preview listener fell behind the bus");
            }
            Recv::Closed => {
                queue.close();
                return;
            }
        }
    }
}

/// Decode queue → preview file, socket push and `preview_ready`.
async fn render_loop(
    queue: Arc<DecodeQueue>,
    hub: Arc<LiveViewHub>,
    camera: Arc<crate::camera::CameraFacade>,
    bus: EventBus,
    params: PreviewParams,
) {
    while let Some(job) = queue.take().await {
        render_one(&job, &hub, &camera, &bus, params).await;
    }
}

/// One frame's preview, from bytes on disk to the operator's screen.
async fn render_one(
    job: &PreviewJob,
    hub: &LiveViewHub,
    camera: &crate::camera::CameraFacade,
    bus: &EventBus,
    params: PreviewParams,
) {
    let started = std::time::Instant::now();

    let Some(id) = FrameId::parse(&job.frame_id) else {
        tracing::warn!(frame = %job.frame_id, "a frame.saved id this node cannot parse");
        return;
    };
    let Ok(session) = camera.session() else {
        // Only reachable if the session vanished between the capture and here.
        tracing::warn!(frame = %job.frame_id, "no open session; preview skipped");
        return;
    };
    let target = session.preview_path(&id);
    let source = job.path.clone();

    // Read *and* decode on the blocking pool, in one hop. SDD §5.7 puts the decode there; the
    // read belongs with it because a 48 MB frame off an SD card is exactly the kind of I/O that
    // stalls a runtime worker, and the node runs `min(2, cores-2)` of them (§7). Two hops would
    // also mean the 48 MB crossing a task boundary for no reason.
    let rendered = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&source)?; // check-async: allow runs on the blocking pool
        astroctl_pipeline::render_preview(&bytes, &params)
            .map_err(|error| std::io::Error::other(error.to_string()))
    })
    .await;

    let preview = match rendered {
        Ok(Ok(preview)) => preview,
        Ok(Err(error)) => {
            // Loud, and not a fault. The frame is durable and the next capture has every chance
            // of working; holding the session over an image the operator can regenerate by
            // capturing again would cost more than the preview is worth.
            tracing::error!(frame = %job.frame_id, %error, "the frame could not be previewed");
            return;
        }
        Err(error) => {
            tracing::error!(frame = %job.frame_id, %error, "the preview task did not join");
            return;
        }
    };

    // Cached before it is announced. `preview_ready` is what sends the PWA to
    // `/api/session/frames/{id}/preview.jpg`, so announcing first would race the operator's own
    // client to the file and answer a `404` to an event this node published.
    if let Err(error) = write_preview(&target, &preview.jpeg).await {
        tracing::error!(
            frame = %job.frame_id,
            path = %target.display(),
            %error,
            "the preview could not be cached"
        );
        return;
    }

    // Pushed once (§5.7), so the panel has the image before it could have fetched it.
    hub.publish_preview(&preview.jpeg, &job.frame_id);

    // `elapsed_s` here is **how long the render took**, not how long the exposure was.
    //
    // §4.3 does not define the field per state and the capture flow already uses it for two
    // different quantities — seconds of open shutter on `exposing`, the exposure's length on
    // `saved` — so it is "elapsed, for whatever this state is". This task has no access to the
    // third meaning even if it wanted it: `frame.saved` carries `{frame_id, path, size_bytes,
    // sha256}` and no duration, and the preview pipeline is deliberately decoupled from the
    // capture task so it can be replaced without touching it. The render time is the one number
    // this step actually knows, and it is the one worth having: it is what says whether previews
    // are keeping up on a Pi.
    bus.publish(CaptureProgress::preview_ready(
        &job.frame_id,
        started.elapsed().as_secs_f64(),
    ));
    tracing::info!(
        frame = %job.frame_id,
        width = preview.width,
        height = preview.height,
        bytes = preview.jpeg.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "preview ready"
    );
}

/// Write the preview through a temporary and rename it into place.
///
/// The same rule the frame store and the stack's ingest both follow, and the Python worker with
/// them: a node killed mid-write must not leave a half-written JPEG at the path the API is about
/// to serve as a finished preview. The temporary is named so `sweep_temporaries` reclaims it.
async fn write_preview(target: &std::path::Path, jpeg: &[u8]) -> std::io::Result<()> {
    let directory = target
        .parent()
        .ok_or_else(|| std::io::Error::other("the preview path has no directory"))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("the preview path has no file name"))?;
    let scratch = directory.join(format!(".tmp_{name}"));

    tokio::fs::write(&scratch, jpeg).await?;
    // Not fsynced. §6 calls a preview "ephemeral, regenerable": paying an fsync per capture on a
    // Pi's SD card to protect a file the next capture would rebuild is the wrong trade, and it is
    // the trade the *frame* store makes precisely because a frame is not regenerable.
    tokio::fs::rename(&scratch, target).await
}

// ---------------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------------

/// `POST /api/camera/liveview/start` — begin streaming (CAM-05).
pub async fn start(State(state): State<AppState>) -> Result<StatusCode, ApiFailure> {
    state
        .liveview
        .start(&state.camera)
        .await
        .map(|()| StatusCode::ACCEPTED)
        .map_err(ApiFailure)
}

/// `POST /api/camera/liveview/stop` — stop it, server-side and at the driver.
pub async fn stop(State(state): State<AppState>) -> Result<StatusCode, ApiFailure> {
    state
        .liveview
        .stop(&state.camera)
        .await
        .map(|()| StatusCode::ACCEPTED)
        .map_err(ApiFailure)
}

/// `GET /api/session/frames/{id}/preview.jpg` — SDD §5.8.1's "cached preview image".
///
/// Serves the cache and never renders on demand. A route that decoded a 48 MB frame because
/// someone opened a URL would put an unbounded amount of blocking work behind a `read`-tier GET,
/// which is the shape of request an LLM tier is allowed to make freely (§8.2).
pub async fn preview(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiFailure> {
    // Parsed, not joined. `{id}` arrives over HTTP, and `FrameId::parse` is the only constructor
    // — so `../../etc/shadow` fails here rather than reaching a path join.
    let Some(frame_id) = FrameId::parse(&id) else {
        return Err(ApiFailure(ApiError::new(
            ErrorCode::Validation,
            format!("`{id}` is not a frame id"),
        )));
    };
    let session = state.camera.session().map_err(ApiFailure)?;
    let path = session.preview_path(&frame_id);

    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        // `NOT_FOUND` for a frame whose preview has not been rendered — which is a normal state,
        // not a failure: previews are derived asynchronously and the burst case deliberately
        // skips some (§5.7). The client's answer is to wait for `preview_ready`, and saying
        // "not found" is what tells it that rather than "this node is broken".
        ApiFailure(ApiError::new(
            ErrorCode::NotFound,
            format!("no cached preview for {frame_id}: {error}"),
        ))
    })?;

    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            // The bytes are immutable for the life of the frame id — a preview is rendered once
            // — but `no-store` is what the rest of this API answers with and a preview the
            // operator is comparing against a *new* capture must not come from a cache. The id
            // in the path is what makes re-fetching cheap enough not to need one.
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    // The envelope constants moved to `astroctl-core` with the encoder (M1-T14). These tests
    // decode by hand rather than through a decoder, which is the point: they assert the bytes
    // the PWA is written against, not that two functions in this file agree.
    use astroctl_core::image_frame::{HEADER_LEN, MAGIC, PROTOCOL_VERSION};

    /// Connect the camera through the same route an operator uses, so a test cannot pass against
    /// a device the binary would have left disconnected.
    async fn connect_camera(state: &AppState) {
        let _status = crate::camera::connect(State(state.clone()), None)
            .await
            .map_err(|failure| failure.0)
            .expect("the simulator must connect");
    }

    fn decode_envelope(frame: &[u8]) -> (FrameKind, serde_json::Value, Vec<u8>) {
        assert_eq!(&frame[..4], MAGIC, "magic");
        assert_eq!(frame[4], PROTOCOL_VERSION, "version");
        let kind = match frame[5] {
            0 => FrameKind::Live,
            1 => FrameKind::Preview,
            other => panic!("unknown kind byte {other}"),
        };
        let meta_len = u16::from_be_bytes([frame[6], frame[7]]) as usize;
        let meta: serde_json::Value =
            serde_json::from_slice(&frame[HEADER_LEN..HEADER_LEN + meta_len]).expect("meta JSON");
        (kind, meta, frame[HEADER_LEN + meta_len..].to_vec())
    }

    #[test]
    fn a_preview_frame_is_told_apart_from_a_live_frame_and_names_its_frame() {
        // The whole reason the envelope exists: without it a preview the operator asked to look
        // at is overwritten by the next live-view frame 200 ms later, and nothing on the wire
        // could have told the panel not to.
        let jpeg = [0xFF, 0xD8, 0x00, 0xFF, 0xD9];
        let live = encode(FrameKind::Live, &jpeg, Utc::now(), None);
        let preview = encode(FrameKind::Preview, &jpeg, Utc::now(), Some("light_00042"));

        let (kind, meta, payload) = decode_envelope(&live);
        assert_eq!(kind, FrameKind::Live);
        assert!(
            meta.get("frame_id").is_none(),
            "a live frame is of nothing in particular"
        );
        assert!(meta["ts"].is_string());
        assert_eq!(payload, jpeg);

        let (kind, meta, payload) = decode_envelope(&preview);
        assert_eq!(kind, FrameKind::Preview);
        assert_eq!(meta["frame_id"], "light_00042");
        assert_eq!(payload, jpeg);
    }

    #[test]
    fn the_jpeg_survives_the_envelope_byte_for_byte() {
        // A framing bug that shifted the payload by one byte would still decode as "a JPEG" in
        // some viewers and as garbage in others.
        let jpeg: Vec<u8> = (0..=255_u8).cycle().take(5000).collect();
        let frame = encode(FrameKind::Preview, &jpeg, Utc::now(), Some("light_00001"));
        let (_, _, payload) = decode_envelope(&frame);
        assert_eq!(payload, jpeg);
    }

    #[tokio::test]
    async fn a_burst_of_captures_leaves_only_the_newest_job_queued() {
        // SDD §5.7's replace semantics, at the queue. Five captures land while nothing is
        // decoding; the worker must find the fifth, not a backlog of five.
        let queue = DecodeQueue::new();
        for i in 1..=5 {
            queue.offer(PreviewJob {
                frame_id: format!("light_{i:05}"),
                path: std::path::PathBuf::from(format!("/frames/light_{i:05}.fits")),
            });
        }

        let job = queue.take().await.expect("a job");
        assert_eq!(
            job.frame_id, "light_00005",
            "only the newest frame is previewed"
        );

        // ...and nothing behind it.
        queue.close();
        assert!(
            queue.take().await.is_none(),
            "the replaced jobs must not still be queued"
        );
    }

    #[tokio::test]
    async fn offering_reports_what_it_displaced_and_nothing_when_it_displaced_nothing() {
        let queue = DecodeQueue::new();
        let job = |id: &str| PreviewJob {
            frame_id: id.to_owned(),
            path: std::path::PathBuf::from("/frames/x.fits"),
        };
        assert_eq!(queue.offer(job("light_00001")), None);
        assert_eq!(
            queue.offer(job("light_00002")).as_deref(),
            Some("light_00001")
        );
    }

    #[tokio::test]
    async fn the_worker_wakes_for_a_job_offered_while_it_was_waiting() {
        // The lost-notification bug: build the `notified()` future after inspecting the slot and
        // a job offered in between leaves the worker asleep until the *next* capture.
        let queue = Arc::new(DecodeQueue::new());
        let waiting = tokio::spawn({
            let queue = Arc::clone(&queue);
            async move { queue.take().await }
        });
        tokio::task::yield_now().await;

        queue.offer(PreviewJob {
            frame_id: "light_00007".to_owned(),
            path: std::path::PathBuf::from("/frames/light_00007.fits"),
        });

        let job = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("the worker must wake")
            .expect("the task completes")
            .expect("a job");
        assert_eq!(job.frame_id, "light_00007");
    }

    #[tokio::test]
    async fn a_closed_queue_releases_the_worker_rather_than_parking_it_forever() {
        // What shutdown depends on: the bus closing must let the worker return, or `abort` is
        // the only thing that ever ends it.
        let queue = DecodeQueue::new();
        queue.close();
        assert!(queue.take().await.is_none());
    }

    #[tokio::test]
    async fn a_hub_with_no_camera_is_not_streaming_and_stops_without_complaint() {
        let hub = LiveViewHub::new();
        assert!(!hub.is_streaming());
        assert!(hub.latest().is_none());
        // Abort on an idle hub is what shutdown calls on a node nobody started live view on.
        hub.abort().await;
        assert!(!hub.is_streaming());
    }

    #[tokio::test]
    async fn a_late_subscriber_is_handed_the_frame_already_in_the_slot() {
        // This socket's equivalent of `/ws`'s connect snapshot: a panel that reconnects after a
        // capture shows the last preview immediately rather than an empty rectangle until the
        // next one — which, with live view stopped, could be the rest of the night.
        let hub = LiveViewHub::new();
        hub.publish_preview(&[0xFF, 0xD8, 0xFF, 0xD9], "light_00003");

        let latest = hub.latest().expect("the slot holds the preview");
        let (kind, meta, _) = decode_envelope(&latest);
        assert_eq!(kind, FrameKind::Preview);
        assert_eq!(meta["frame_id"], "light_00003");
    }

    #[tokio::test]
    async fn a_second_start_is_not_a_second_stream() {
        // Two phones, two operators, two taps on *start*. The camera has one sensor and a real
        // body has one USB conversation to give, so the second tap must be a no-op rather than a
        // second sensor loop — and must still answer success, because from the operator's side
        // live view is now running, which is what they asked for.
        let hub = Arc::new(LiveViewHub::new());
        let node = crate::test_support::TestNode::open_loopback();
        let (_, declarations) = crate::api::router();
        let state = crate::test_support::state_with(&node, declarations).await;
        connect_camera(&state).await;

        hub.start(&state.camera).await.expect("live view starts");
        assert!(hub.is_streaming());
        hub.start(&state.camera)
            .await
            .expect("a second start is fine");
        assert!(hub.is_streaming());

        hub.stop(&state.camera).await.expect("live view stops");
        assert!(
            !hub.is_streaming(),
            "stopping must leave no forwarding task behind — the orphan-work criterion"
        );
    }

    #[tokio::test]
    async fn a_slow_client_skips_frames_instead_of_accumulating_them() {
        // The depth-1 replace of §5.8.3, per client, and the property the HOL isolation rests on:
        // a client that cannot keep up falls behind in *time*, never in queue depth, so nothing
        // it does costs the node memory or makes another client wait.
        let hub = LiveViewHub::new();
        let mut client = hub.subscribe();

        for i in 0..100 {
            hub.publish_preview(&[i], &format!("light_{i:05}"));
        }

        let frame = client.next_frame().await.expect("a frame");
        let (_, meta, payload) = decode_envelope(&frame);
        assert_eq!(
            meta["frame_id"], "light_00099",
            "the client must wake to the newest frame, not the oldest of a backlog"
        );
        assert_eq!(payload, vec![99]);
    }
}

/// End-to-end tests against a bound port and a real RFC 6455 client.
///
/// Everything here needs a completed upgrade, which `tower`'s `oneshot` cannot produce — the same
/// reason `crate::ws` has a module like this one. These bind an ephemeral port and drive the
/// assembled app, with `tokio-tungstenite` as the client.
#[cfg(test)]
mod e2e {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use astroctl_core::event::{MountPosition, PierSide};
    use astroctl_core::image_frame::{HEADER_LEN, MAGIC, PROTOCOL_VERSION};
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    use super::*;
    use crate::test_support::{state_with_camera, TestNode};

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    struct Node {
        addr: SocketAddr,
        state: AppState,
        /// Kept alive for the test's duration; dropping it removes the session directory.
        _dir: TestNode,
    }

    impl Node {
        /// A node with a 128×96 sensor and no simulated download delay.
        ///
        /// `CameraProfile::fast()`, because the default is a 48 MB frame at 6000×4000 and these
        /// tests capture up to five times. The *pipeline* does not care about the size — it is
        /// the same decode either way — and the one thing a large sensor would add is minutes.
        async fn start() -> Self {
            let dir = TestNode::open_loopback().with_shutter("1/250");
            let (router, declarations) = crate::api::router();
            let (ws_router, ws_declarations) = crate::api::ws_router();
            let state = state_with_camera(
                &dir,
                declarations.into_iter().chain(ws_declarations).collect(),
                astroctl_drivers::simulator::CameraProfile::fast(),
            )
            .await;
            let app = crate::assemble(router, ws_router, state.clone());

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("binds an ephemeral port");
            let addr = listener.local_addr().expect("has a local address");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                addr,
                state,
                _dir: dir,
            }
        }

        fn ticket(&self) -> String {
            self.state
                .tickets
                .issue()
                .expect("the store issues tickets")
                .as_str()
                .to_owned()
        }

        /// Open `/ws/liveview` with a fresh ticket.
        async fn liveview(&self) -> Socket {
            let url = format!("ws://{}/ws/liveview?ticket={}", self.addr, self.ticket());
            let (socket, _) = tokio_tungstenite::connect_async(url)
                .await
                .expect("the liveview socket must open");
            socket
        }

        /// Open `/ws` and swallow the connect snapshot.
        async fn control(&self) -> Socket {
            let url = format!("ws://{}/ws?ticket={}", self.addr, self.ticket());
            let (mut socket, _) = tokio_tungstenite::connect_async(url)
                .await
                .expect("the control socket must open");
            let _snapshot = socket.next().await.expect("a snapshot").expect("readable");
            socket
        }

        async fn connect_camera(&self) {
            let _status = crate::camera::connect(State(self.state.clone()), None)
                .await
                .map_err(|failure| failure.0)
                .expect("the simulator must connect");
        }

        /// Start the preview pipeline, as `main` does at step 7.
        fn spawn_previews(&self) -> PreviewPipeline {
            super::spawn_previews(
                Arc::clone(&self.state.liveview),
                Arc::clone(&self.state.camera),
                self.state.bus.clone(),
                self.state.bus.subscribe(),
                PreviewParams::default(),
            )
        }

        async fn capture(&self) -> String {
            let accepted = self
                .state
                .camera
                .start_capture(crate::camera::CaptureCommand::default())
                .await
                .expect("the capture is accepted");
            accepted.frame_id
        }

        /// Wait until the capture the node was running has fully finished.
        async fn until_idle(&self) {
            for _ in 0..4_000 {
                if !self.state.camera.is_capturing() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the capture never finished");
        }
    }

    /// The next binary frame, decoded.
    async fn next_frame(socket: &mut Socket) -> (FrameKind, serde_json::Value, Vec<u8>) {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
                .await
                .expect("a frame should arrive")
                .expect("the socket stays open")
                .expect("a readable frame");
            if let ClientMessage::Binary(bytes) = message {
                assert_eq!(&bytes[..4], MAGIC, "every frame carries the envelope magic");
                assert_eq!(bytes[4], PROTOCOL_VERSION);
                let kind = match bytes[5] {
                    0 => FrameKind::Live,
                    1 => FrameKind::Preview,
                    other => panic!("unknown kind byte {other}"),
                };
                let meta_len = u16::from_be_bytes([bytes[6], bytes[7]]) as usize;
                let meta: serde_json::Value =
                    serde_json::from_slice(&bytes[HEADER_LEN..HEADER_LEN + meta_len])
                        .expect("meta JSON");
                return (kind, meta, bytes[HEADER_LEN + meta_len..].to_vec());
            }
        }
    }

    #[tokio::test]
    async fn the_liveview_socket_is_ticket_authenticated_exactly_as_ws_is() {
        // §4.5 makes the ticket the only way a browser authenticates *either* socket. If this
        // check were missing the socket would simply open, and the node's image stream would be
        // readable by anything that could reach the port.
        let node = Node::start().await;

        assert!(
            tokio_tungstenite::connect_async(format!("ws://{}/ws/liveview", node.addr))
                .await
                .is_err(),
            "an upgrade with no ticket must be refused"
        );
        assert!(
            tokio_tungstenite::connect_async(format!(
                "ws://{}/ws/liveview?ticket=00000000000000000000000000000000",
                node.addr
            ))
            .await
            .is_err(),
            "a ticket that was never issued must be refused"
        );

        // A bearer header must not substitute — the same rule `/ws` is held to, and for the same
        // reason: a broken ticket path would otherwise pass on a desktop client and fail on the
        // phone it was written for.
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{}/ws/liveview", node.addr),
            )
            .expect("builds");
        request.headers_mut().insert(
            "authorization",
            "Bearer s3cret".parse().expect("valid header"),
        );
        assert!(
            tokio_tungstenite::connect_async(request).await.is_err(),
            "a bearer header must not authenticate /ws/liveview"
        );

        // ...and a real ticket works, once.
        let ticket = node.ticket();
        let url = format!("ws://{}/ws/liveview?ticket={ticket}", node.addr);
        let (socket, _) = tokio_tungstenite::connect_async(url.clone())
            .await
            .expect("a fresh ticket must be accepted");
        drop(socket);
        assert!(
            tokio_tungstenite::connect_async(url).await.is_err(),
            "a replayed ticket must be refused — §4.5's single-use rule"
        );
    }

    #[tokio::test]
    async fn a_capture_pushes_its_preview_announces_it_and_serves_it_over_rest() {
        // The whole of §5.7's source 2, from one exposure: the preview reaches the socket, the
        // event says so, and the cached file answers the route the event sends a client to.
        let node = Node::start().await;
        node.connect_camera().await;
        let previews = node.spawn_previews();

        let mut images = node.liveview().await;
        let mut control = node.control().await;

        let frame_id = node.capture().await;

        // 1. the binary push (§5.7: "pushed once on the bus").
        let (kind, meta, jpeg) = next_frame(&mut images).await;
        assert_eq!(kind, FrameKind::Preview);
        assert_eq!(
            meta["frame_id"], frame_id,
            "the push must name the frame it previews, or the panel cannot label it"
        );
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "a JPEG, SOI marker first");

        // 2. `capture.progress: preview_ready` on the *control* socket, not this one.
        let mut announced = false;
        for _ in 0..40 {
            let message = tokio::time::timeout(Duration::from_secs(20), control.next())
                .await
                .expect("an event should arrive")
                .expect("the socket stays open")
                .expect("a readable frame");
            let ClientMessage::Text(text) = message else {
                panic!("/ws must never carry a binary frame — that is §8.3(5)");
            };
            let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
            if value["topic"] == "capture.progress" && value["data"]["state"] == "preview_ready" {
                assert_eq!(value["data"]["frame_id"], frame_id);
                announced = true;
                break;
            }
        }
        assert!(announced, "preview_ready must reach the control socket");

        // 3. the cache, fetchable at the route the event sends a client to (§5.8.1).
        let response = super::preview(State(node.state.clone()), Path(frame_id.clone()))
            .await
            .map_err(|failure| failure.0)
            .expect("the preview must be served");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/jpeg")
        );

        // ...and it is the file §5.7 names, in the directory derived artifacts belong in.
        let session = node.state.camera.session().expect("a session");
        let id = FrameId::parse(&frame_id).expect("a frame id");
        let cached = session.preview_path(&id);
        assert_eq!(
            cached.file_name().and_then(|n| n.to_str()),
            Some("light_00001.jpg")
        );
        let on_disk = tokio::fs::read(&cached).await.expect("the cached preview");
        assert_eq!(
            on_disk, jpeg,
            "the pushed bytes and the cached bytes are one render"
        );

        previews.abort().await;
    }

    #[tokio::test]
    async fn an_unrendered_preview_is_not_found_rather_than_rendered_on_demand() {
        // A route that decoded a 48 MB frame because someone opened a URL would put unbounded
        // blocking work behind a `read`-tier GET. "Not found" is also the honest answer while a
        // render is still queued, and it is what tells a client to wait for `preview_ready`.
        let node = Node::start().await;
        let failure = super::preview(State(node.state.clone()), Path("light_00042".to_owned()))
            .await
            .expect_err("an unrendered preview must not be served");
        assert_eq!(failure.0.http_status(), 404);

        // And a path that is not a frame id is refused before it can reach a path join.
        let failure = super::preview(
            State(node.state.clone()),
            Path("../../etc/shadow".to_owned()),
        )
        .await
        .expect_err("a traversal attempt must be refused");
        assert_eq!(
            failure.0.http_status(),
            422,
            "§4.2 maps `VALIDATION` to 422; the point is that it never reached a path join"
        );
    }

    #[tokio::test]
    async fn a_burst_of_captures_previews_the_newest_and_keeps_every_frame() {
        // The M1-T09 acceptance criterion, both halves. §5.7 chose to preview only the newest —
        // "previews are ephemeral; raw frames are what matters" — so the assertion that matters
        // is that the *frames* are all there whether or not their previews are.
        let node = Node::start().await;
        node.connect_camera().await;
        let previews = node.spawn_previews();

        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(node.capture().await);
            // Serialised because the camera takes one exposure at a time (a second is `409 BUSY`,
            // M1-T08). The burst the *queue* sees is still a burst: five `frame.saved` events
            // arrive far faster than five decodes complete.
            node.until_idle().await;
        }
        assert_eq!(ids.len(), 5);

        // Every frame is durable — the half that is not allowed to degrade.
        let view = node
            .state
            .camera
            .session_view()
            .await
            .expect("the session lists its frames");
        assert_eq!(
            view.frame_count, 5,
            "all five exposures must be on disk regardless of how many were previewed"
        );

        // The newest is the one that gets previewed.
        let session = node.state.camera.session().expect("a session");
        let last = ids.last().expect("five ids").clone();
        let last_path = session.preview_path(&FrameId::parse(&last).expect("a frame id"));
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if tokio::fs::try_exists(&last_path).await.unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            tokio::fs::try_exists(&last_path).await.unwrap_or(false),
            "the newest frame must be the one that gets previewed"
        );

        previews.abort().await;
    }

    /// **T-HOL-1, the basic version.** The full shaped-link measurement is M1-T16's.
    ///
    /// SDD §8.3(5): "A 500 KB JPEG retransmit can therefore never head-of-line-block a stop
    /// command or a position update." The claim is about *connection separation*, so the test
    /// that means anything is one where the image socket is genuinely wedged: this client
    /// connects and then never reads a byte, so its receive window closes, the node's writes
    /// block, and the depth-1 slot starts replacing.
    ///
    /// If the two streams shared a connection — one socket, multiplexed — the control client's
    /// positions would queue behind the image bytes and the gap would grow without bound. That is
    /// the failure this asserts against.
    #[tokio::test]
    async fn saturating_the_liveview_socket_leaves_control_events_on_cadence() {
        let node = Node::start().await;

        // A client that opens the socket and never reads it. Held, not dropped: dropping it would
        // close the connection and there would be nothing left to saturate.
        let _wedged = node.liveview().await;
        let mut control = node.control().await;

        // 512 KB per frame — §8.3(5)'s "500 KB JPEG", give or take.
        let payload = vec![0xAB_u8; 512 * 1024];
        let hub = Arc::clone(&node.state.liveview);
        let saturator = tokio::spawn(async move {
            for i in 0..10_000_u32 {
                hub.publish_preview(&payload, &format!("light_{i:05}"));
                // Yield rather than sleep: the point is to publish as fast as the node will take
                // them, not to pace them.
                tokio::task::yield_now().await;
            }
        });

        // Positions at 25 ms — 1 Hz's shape, compressed so the test takes seconds rather than a
        // minute. `mount.position` is the coalescing topic, which is the one whose *cadence*
        // §8.3(6) promises.
        let bus = node.state.bus.clone();
        let ticker = tokio::spawn(async move {
            for i in 0..200_u32 {
                bus.publish(MountPosition::new(
                    f64::from(i) / 100.0,
                    0.0,
                    None,
                    None,
                    PierSide::Unknown,
                ));
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });

        let mut gaps = Vec::new();
        let mut last = Instant::now();
        let mut seen = 0;
        while seen < 40 {
            let message = tokio::time::timeout(Duration::from_secs(10), control.next())
                .await
                .expect("control traffic must keep flowing while liveview is saturated")
                .expect("the control socket stays open")
                .expect("a readable frame");
            let ClientMessage::Text(text) = message else {
                panic!("/ws must never carry a binary frame — that is the whole point of §8.3(5)");
            };
            let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
            if value["topic"] == "mount.position" {
                gaps.push(last.elapsed());
                last = Instant::now();
                seen += 1;
            }
        }

        saturator.abort();
        ticker.abort();

        let worst = gaps.iter().max().copied().unwrap_or_default();
        // 25 ms nominal. 500 ms is 20× headroom — loose enough not to flake on a loaded CI box,
        // tight enough that a shared connection (where the gap grows with the image backlog,
        // unbounded) could not pass it. M1-T16 tightens this to §9's "≤ 2× baseline" over a
        // shaped 1 Mbit link.
        assert!(
            worst < Duration::from_millis(500),
            "a saturated liveview socket delayed control traffic by {worst:?}; §8.3(5) says the \
             two connections are independent"
        );
    }

    #[tokio::test]
    async fn stopping_live_view_leaves_no_stream_task_behind() {
        // "Stopping live view closes server-side work — no orphan stream tasks." A forwarding
        // task left running would keep the driver producing frames nobody reads, which on a real
        // body is a USB transfer per frame for the rest of the night.
        let node = Node::start().await;
        node.connect_camera().await;

        let started = super::start(State(node.state.clone()))
            .await
            .expect("live view starts");
        assert_eq!(started, StatusCode::ACCEPTED);
        assert!(node.state.liveview.is_streaming());

        let stopped = super::stop(State(node.state.clone()))
            .await
            .expect("live view stops");
        assert_eq!(stopped, StatusCode::ACCEPTED);
        assert!(
            !node.state.liveview.is_streaming(),
            "the forwarding task must be gone, not merely told to finish"
        );

        // Stopping twice is not an error: an operator tapping stop on two phones, or a reconnect
        // that re-sends it, must not produce a failure the panel has to explain.
        let again = super::stop(State(node.state.clone()))
            .await
            .expect("a second stop is fine");
        assert_eq!(again, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn live_view_frames_reach_a_connected_client() {
        // The acceptance criterion's first half, at the socket rather than at the hub: frames the
        // *driver* produced, through the forwarding task, out of a real WebSocket.
        let node = Node::start().await;
        node.connect_camera().await;
        let mut images = node.liveview().await;

        super::start(State(node.state.clone()))
            .await
            .expect("live view starts");

        let (kind, meta, jpeg) = next_frame(&mut images).await;
        assert_eq!(kind, FrameKind::Live);
        assert!(
            meta.get("frame_id").is_none(),
            "a live-view frame is of nothing in particular"
        );
        assert!(
            meta["ts"].is_string(),
            "every frame carries its capture time"
        );
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "a JPEG, SOI marker first");

        super::stop(State(node.state.clone()))
            .await
            .expect("live view stops");
    }
}
