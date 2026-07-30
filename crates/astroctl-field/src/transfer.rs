//! The field node's half of the transfer agent — SDD §5.10.
//!
//! `astroctl-transfer` owns the journal, the upload and the drain loop. What lives here is the
//! part that needs things only the binary has: the `frame.saved` subscription, the frame store the
//! lag-recovery path reconciles against, and the `/api/transfer/status` route.
//!
//! # Why the enqueue is here and not in the crate
//!
//! Subscribing to `frame.saved` looks like the agent's job, and §5.10.2's diagram draws it that
//! way. But the bus drops events for a slow subscriber rather than applying backpressure (§4.3),
//! and a missed `frame.saved` is a frame that is never queued and therefore never archived —
//! silently, with no symptom until someone counts frames on the far end. The only recovery is to
//! reconcile the queue against the frames actually on disk, and the thing that knows what is on
//! disk is the frame store, which the binary holds. Putting the listener anywhere else would mean
//! handing `astroctl-transfer` a dependency on `astroctl-session` to carry a directory listing.
//!
//! That reconciliation also closes a hole §5.10 does not mention: a node that dies between
//! `frame.saved` and the journal insert would otherwise never offer that frame at all. It runs at
//! startup for exactly that reason, not only on lag.

use std::sync::Arc;

use astroctl_core::bus::{EventBus, EventSubscriber, Recv};
use astroctl_core::config::{FieldConfig, TransferMethod};
use astroctl_core::error::{ApiError, ErrorCode};
use astroctl_core::event::Topic;
use astroctl_session::FrameStore;
use astroctl_transfer::{AgentConfig, Journal, NewEntry, TransferAgent, TransferQueue, Uploader};
use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::api::{ApiFailure, AppState};

/// The file the queue lives in, inside `stacking_server.queue_dir`.
const JOURNAL_FILE: &str = "transfer.db";

/// What `/api/transfer/status` answers — SDD §5.10.4.
///
/// The first four fields are the `transfer.status` event payload; `attempts_current` is REST-only.
/// §5.10.4 settles the earlier "the two are the same data" wording the other way and says why: a
/// retry counter ticking behind a temporarily unreachable stack node is diagnostic detail the
/// operator can pull when they care, not something worth pushing to every connected client.
#[derive(Debug, Serialize, PartialEq)]
pub struct TransferStatusBody {
    /// Schema version, like every other response envelope.
    pub v: u16,
    /// `idle` | `uploading` | `offline`.
    pub state: astroctl_core::event::TransferState,
    /// Frames still owed to the archive — `queued` plus the one in flight.
    pub queue_depth: u64,
    /// Age of the oldest owed frame, or `null` on an empty queue.
    pub oldest_queued_age_s: Option<f64>,
    /// When the last ack landed, or `null` if none has.
    pub last_ack_ts: Option<String>,
    /// Attempts already spent on the frame at the head of the queue.
    pub attempts_current: u32,
}

/// The transfer agent as the rest of the binary sees it.
///
/// `None` inside means this node does not transfer: `stacking_server.enabled: false`, or a
/// `transfer_method` this increment does not implement. The route still exists in both cases —
/// the same reasoning `StackProxy` records, that a route which disappears makes authentication
/// behave differently depending on configuration — and answers `409` instead of inventing an
/// idle queue that would tell the operator their frames were fine.
#[derive(Debug)]
pub struct TransferFacade {
    queue: Option<Arc<TransferQueue>>,
    /// Why there is no queue, for the refusal message.
    disabled_because: Option<String>,
}

impl TransferFacade {
    /// A node that transfers.
    #[must_use]
    pub fn enabled(queue: Arc<TransferQueue>) -> Self {
        Self {
            queue: Some(queue),
            disabled_because: None,
        }
    }

    /// A node that does not, and the reason an operator gets told.
    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            queue: None,
            disabled_because: Some(reason.into()),
        }
    }

    /// The queue, when there is one.
    #[must_use]
    pub fn queue(&self) -> Option<&Arc<TransferQueue>> {
        self.queue.as_ref()
    }

    /// The `/api/transfer/status` body.
    async fn status_body(&self) -> Result<TransferStatusBody, ApiError> {
        let Some(queue) = self.queue() else {
            return Err(ApiError::new(
                ErrorCode::NotConnected,
                self.disabled_because
                    .clone()
                    .unwrap_or_else(|| "this node does not transfer frames".to_owned()),
            ));
        };

        let snapshot = queue.snapshot().await.map_err(|error| {
            // Not "idle": a status route that reported an empty queue because it could not read
            // the queue would be the most misleading answer it could give.
            ApiError::new(
                ErrorCode::Internal,
                format!("cannot read the transfer queue: {error}"),
            )
        })?;
        let status = queue.status_of(&snapshot, astroctl_core::event::now_millis());

        Ok(TransferStatusBody {
            v: astroctl_core::event::EVENT_SCHEMA_VERSION,
            state: status.state(),
            queue_depth: status.queue_depth(),
            oldest_queued_age_s: status.oldest_queued_age_s(),
            last_ack_ts: status
                .last_ack_ts()
                .map(|ts| ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            attempts_current: snapshot.attempts_current,
        })
    }
}

/// `GET /api/transfer/status` — SDD §5.10.4.
pub async fn status(State(state): State<AppState>) -> Result<Json<TransferStatusBody>, ApiFailure> {
    state
        .transfer
        .status_body()
        .await
        .map(Json)
        .map_err(ApiFailure)
}

/// Everything the binary must do at startup to bring the queue up (SDD §8.1, §5.10.3).
///
/// Returns the facade for `AppState` and, when this node transfers, the running agent. Opening the
/// journal is a **startup error**: a node that cannot open its queue would capture all night and
/// deliver nothing, and finding that out at the first frame is finding it out in a field, in the
/// dark — the same argument [`crate::open_session`] makes for the session directory.
///
/// # Errors
/// If `stacking_server.queue_dir` cannot be created or the journal cannot be opened.
pub async fn start(
    config: &FieldConfig,
    store: Arc<FrameStore>,
    bus: EventBus,
    events: EventSubscriber,
) -> Result<(Arc<TransferFacade>, Option<TransferAgent>), Box<dyn std::error::Error>> {
    if !config.stacking_server.enabled {
        tracing::info!(
            "`stacking_server.enabled: false` — frames stay on this node and are not transferred"
        );
        return Ok((
            Arc::new(TransferFacade::disabled(
                "the stacking server is disabled on this node (`stacking_server.enabled: false`)",
            )),
            None,
        ));
    }

    if config.stacking_server.transfer_method != TransferMethod::Http {
        // Refusing to start would be worse — the operator's mount and camera work fine — but so
        // would silently using HTTP: they asked for rsync and would believe they had it.
        tracing::warn!(
            "`stacking_server.transfer_method: rsync` is not implemented in this increment; \
             frames will stay on this node. Set it to `http` to transfer."
        );
        return Ok((
            Arc::new(TransferFacade::disabled(
                "`stacking_server.transfer_method: rsync` is not implemented; this increment \
                 transfers over `http` only",
            )),
            None,
        ));
    }

    let path = config.stacking_server.queue_dir.join(JOURNAL_FILE);
    let journal = Journal::open(path.clone()).await.map_err(|error| {
        format!(
            "cannot open the transfer queue at {}: {error}",
            path.display()
        )
    })?;

    // §5.10.3: rows left `uploading` by a crash return to `queued`. Before anything can claim one,
    // so a restart cannot race its own recovery.
    let resumed = journal.recover_interrupted().await?;
    if resumed > 0 {
        tracing::info!(
            resumed,
            "returned interrupted uploads to the queue; re-upload is safe because ingest \
             deduplicates (SDD §5.10.3)"
        );
    }

    let queue = Arc::new(TransferQueue::new(journal, bus.clone()));

    // The hole §5.10 does not name: a node that died between `frame.saved` and the insert. Run
    // before the uploader starts so the recovered frames drain in id order with everything else.
    let recovered = reconcile(&queue, &store).await;
    if recovered > 0 {
        tracing::warn!(
            recovered,
            "frames were on disk with no queue row and have been enqueued — the node was \
             interrupted between saving them and recording them as owed"
        );
    }

    let token = config
        .auth_token()
        .ok()
        .map(|secret| secret.expose().to_owned());
    let agent = TransferAgent::spawn(
        Arc::clone(&queue),
        AgentConfig {
            retry_interval: std::time::Duration::from_secs(config.stacking_server.retry_interval),
            uploader: Uploader::new(
                &config.stacking_server.host,
                config.stacking_server.port,
                token.as_deref(),
            ),
        },
        config.stacking_server.pacing,
    );

    let listener = tokio::spawn(listen(Arc::clone(&queue), store, events));
    Ok((
        Arc::new(TransferFacade::enabled(Arc::clone(&queue))),
        Some(agent.with_listener(listener)),
    ))
}

/// Bus → queue. Holds a subscriber and the frame store; nothing here can publish.
///
/// The split is `spawn_previews`': the task that reads the bus never does slow work, and the task
/// that does slow work never reads the bus.
async fn listen(queue: Arc<TransferQueue>, store: Arc<FrameStore>, mut events: EventSubscriber) {
    loop {
        match events.recv().await {
            Recv::Event(event) => {
                if event.topic != Topic::FrameSaved {
                    continue;
                }
                enqueue_saved(&queue, &event.data).await;
            }
            Recv::Lagged { skipped } => {
                // The preview pipeline treats lag as harmless, and is right to: a stale preview is
                // worth nothing. This listener is the opposite case. Each skipped event may be a
                // frame that never enters the queue and therefore never reaches the archive, and
                // nothing downstream would ever notice. So a lag is a resync, at `warn`.
                tracing::warn!(
                    skipped,
                    "the transfer listener fell behind the event bus; reconciling the queue \
                     against the frames on disk"
                );
                let recovered = reconcile(&queue, &store).await;
                if recovered > 0 {
                    tracing::warn!(recovered, "frames recovered by the reconciliation");
                }
            }
            Recv::Closed => return,
        }
    }
}

/// One `frame.saved` payload into one queue row.
async fn enqueue_saved(queue: &TransferQueue, data: &serde_json::Value) {
    let (Some(frame_id), Some(path), Some(sha256), Some(size_bytes)) = (
        data.get("frame_id").and_then(|v| v.as_str()),
        data.get("path").and_then(|v| v.as_str()),
        data.get("sha256").and_then(|v| v.as_str()),
        data.get("size_bytes").and_then(serde_json::Value::as_u64),
    ) else {
        tracing::warn!("a frame.saved event did not carry the four fields SDD §4.3 gives it");
        return;
    };

    let path = std::path::PathBuf::from(path);
    // §5.5's layout is the only thing that knows which session a frame belongs to — `frame.saved`
    // does not carry the id, and holding "the current session" would file a frame under the wrong
    // one the moment a session rolls over.
    let Some(session_id) = astroctl_transfer::session_id(&path) else {
        tracing::error!(
            frame = %frame_id,
            path = %path.display(),
            "a saved frame is not inside a session's `frames/` directory; it cannot be transferred"
        );
        return;
    };

    let entry = NewEntry {
        session_id,
        frame_id: frame_id.to_owned(),
        path,
        // The checksum the frame store read back off the disk after the commit. Not recomputed:
        // a second SHA-256 of 48 MB is most of a second of a Pi's CPU for a number already in hand.
        sha256: sha256.to_ascii_lowercase(),
        size_bytes,
    };

    match queue.journal().enqueue(entry).await {
        Ok(true) => {
            tracing::debug!(frame = %frame_id, "frame queued for transfer");
            queue.notify();
        }
        // Already queued, already acked, or already parked. All three are correct outcomes for a
        // replayed event and none is worth a line at anything above `trace`.
        Ok(false) => tracing::trace!(frame = %frame_id, "frame is already in the transfer queue"),
        Err(error) => tracing::error!(
            frame = %frame_id, %error,
            "cannot record a saved frame as owed; it will be picked up by the next reconciliation"
        ),
    }
}

/// Enqueue every frame the current session holds that the queue does not know about.
///
/// Returns how many rows were inserted. Idempotent by construction — the journal's primary key
/// does the work — so it is safe to run at startup and again on every lag.
async fn reconcile(queue: &TransferQueue, store: &FrameStore) -> u64 {
    let Some(session) = store.current() else {
        return 0;
    };
    let view = match session.view().await {
        Ok(view) => view,
        Err(error) => {
            tracing::error!(%error, "cannot list the session's frames to reconcile the queue");
            return 0;
        }
    };

    let frames_dir = session.frames_dir();
    let mut inserted = 0;
    for frame in view.frames {
        let path = frames_dir.join(&frame.file_name);
        // The sidecar carries the checksum the store read back after the commit, so the common
        // case costs no hashing at all.
        let (sha256, size_bytes) = match frame.quality {
            Some(quality) => (quality.sha256, quality.size_bytes),
            None => {
                // A frame committed by a process that died before writing its metadata is still a
                // frame (REL-05), and it is exactly the case this reconciliation exists for.
                // Hashing it costs ~800 ms of a Pi's CPU once; skipping it would cost the frame.
                match hash_frame(&path).await {
                    Some(pair) => pair,
                    None => continue,
                }
            }
        };

        let entry = NewEntry {
            session_id: view.session_id.clone(),
            frame_id: frame.frame_id.to_string(),
            path,
            sha256,
            size_bytes,
        };
        match queue.journal().enqueue(entry).await {
            Ok(true) => inserted += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "cannot enqueue a frame found by the reconciliation");
            }
        }
    }

    if inserted > 0 {
        queue.notify();
    }
    inserted
}

/// SHA-256 and size of a frame with no sidecar, on the blocking pool.
async fn hash_frame(path: &std::path::Path) -> Option<(String, u64)> {
    let path = path.to_owned();
    let hashed = tokio::task::spawn_blocking(move || {
        use sha2::Digest as _;
        // check-async: allow runs on the blocking pool, like the preview decode
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = sha2::Sha256::new();
        let size = std::io::copy(&mut file, &mut hasher)?;
        Ok::<_, std::io::Error>((format!("{:x}", hasher.finalize()), size))
    })
    .await;

    match hashed {
        Ok(Ok(pair)) => Some(pair),
        Ok(Err(error)) => {
            tracing::error!(%error, "cannot hash a frame that has no quality sidecar");
            None
        }
        Err(error) => {
            tracing::error!(%error, "the frame hashing task did not join");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{state_with, TestNode};
    use astroctl_core::event::FrameSaved;
    use astroctl_transfer::State as RowState;
    use axum::body::Body;
    use axum::http::{header, HeaderValue, Method, Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt as _;

    async fn get(state: &AppState, path: &str) -> (StatusCode, Value) {
        let (router, _) = crate::api::router();
        let (ws_router, _) = crate::api::ws_router();
        let app = crate::assemble(router, ws_router, state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(path)
                    .header(
                        header::AUTHORIZATION,
                        HeaderValue::from_static("Bearer s3cret"),
                    )
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// `with_shutter("1/250")` because the example config's default is a **30 second** exposure,
    /// which is right for an operator and is exactly one test deadline for a suite.
    async fn node() -> AppState {
        let (_, declarations) = crate::api::router();
        state_with(
            &TestNode::authenticated("s3cret").with_shutter("1/250"),
            declarations,
        )
        .await
    }

    /// The M1-T14 contract: the route answers the five fields §5.10.4 names, and an empty queue is
    /// `idle` rather than an error.
    #[tokio::test]
    async fn the_status_route_answers_the_five_fields_of_5_10_4() {
        let state = node().await;
        let (status, body) = get(&state, "/api/transfer/status").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["v"], 1);
        assert_eq!(body["state"], "idle");
        assert_eq!(body["queue_depth"], 0);
        assert_eq!(body["oldest_queued_age_s"], Value::Null);
        assert_eq!(body["last_ack_ts"], Value::Null);
        assert_eq!(body["attempts_current"], 0);
    }

    /// A node that does not transfer must not answer as though its queue were empty and healthy —
    /// that is the one answer that would tell an operator their frames were safely elsewhere.
    #[tokio::test]
    async fn a_node_that_does_not_transfer_refuses_rather_than_reporting_idle() {
        let (_, declarations) = crate::api::router();
        let state = state_with(
            &TestNode::authenticated("s3cret").with_stack_disabled(),
            declarations,
        )
        .await;
        let (status, body) = get(&state, "/api/transfer/status").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "NOT_CONNECTED");
    }

    /// The enqueue path of §5.10.2, including the session id §4.3's payload does not carry.
    #[tokio::test]
    async fn a_saved_frame_is_queued_with_the_session_its_path_names() {
        let state = node().await;
        let queue = state.transfer.queue().expect("an enabled node").clone();

        let session = state.camera.session().expect("an open session");
        let path = session.frames_dir().join("light_00001.cr3");
        let saved = FrameSaved::new("light_00001", &path, 11, "AB".repeat(32));
        enqueue_saved(&queue, &serde_json::to_value(&saved).unwrap()).await;

        let entry = queue
            .journal()
            .lookup(session.id(), "light_00001")
            .await
            .unwrap()
            .expect("the frame is owed");
        assert_eq!(entry.state, RowState::Queued);
        assert_eq!(entry.path, path);
        assert_eq!(entry.size_bytes, 11);
        assert_eq!(
            entry.sha256,
            "ab".repeat(32),
            "lowercased so §5.10.2's echo comparison cannot fail on case"
        );

        // …and the route now says so.
        let (status, body) = get(&state, "/api/transfer/status").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "uploading", "something is owed: {body}");
        assert_eq!(body["queue_depth"], 1);
        assert!(body["oldest_queued_age_s"].as_f64().is_some(), "{body}");
    }

    /// A frame outside `<session>/frames/` cannot be mirrored, and guessing a session id would put
    /// it in the wrong directory on the archive of record.
    #[tokio::test]
    async fn a_frame_outside_the_session_layout_is_not_queued() {
        let state = node().await;
        let queue = state.transfer.queue().expect("an enabled node").clone();

        let saved = FrameSaved::new("light_00001", "/tmp/stray.cr3", 11, "ab".repeat(32));
        enqueue_saved(&queue, &serde_json::to_value(&saved).unwrap()).await;
        assert_eq!(queue.snapshot().await.unwrap().depth, 0);
    }

    /// The hole §5.10 does not name: bytes on disk with no queue row, because the node died
    /// between the commit and the insert. The reconciliation is what makes that survivable, and it
    /// reads the checksum out of the sidecar rather than re-hashing.
    #[tokio::test]
    async fn reconciliation_enqueues_a_frame_that_never_got_a_row() {
        let state = node().await;
        let queue = state.transfer.queue().expect("an enabled node").clone();
        let store = state.camera.store();

        // A real capture, so the frame, its sidecar and the manifest are all genuinely there.
        // Subscribed *before* the capture is posted: `/api/camera/capture` answers 202 and the
        // frame lands on a task, so a subscriber opened afterwards races the event it is waiting
        // for — and with `CameraProfile::fast()` it loses.
        let mut events = state.bus.subscribe();
        let (status, _) = post(&state, "/api/camera/connect").await;
        assert_eq!(status, StatusCode::OK, "the simulator must connect");
        let (status, _) = post_capture(&state).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let saved = wait_for_frame(&mut events).await;

        // The frame is on disk; pretend the listener never saw the event.
        assert_eq!(queue.snapshot().await.unwrap().depth, 0);

        assert_eq!(reconcile(&queue, &store).await, 1);
        let entry = queue
            .journal()
            .lookup(&saved.0, &saved.1)
            .await
            .unwrap()
            .expect("the reconciliation found it");
        assert_eq!(entry.state, RowState::Queued);
        assert_eq!(
            entry.sha256, saved.2,
            "the sidecar's checksum, not a re-hash"
        );

        // Idempotent: running it again inserts nothing.
        assert_eq!(reconcile(&queue, &store).await, 0);
    }

    /// The shutdown invariant (SDD §7): no `EventBus` sender may outlive shutdown, or the session
    /// log's subscriber never closes and the flush costs its whole timeout. The agent's uploader
    /// publishes `transfer.acked`, so it holds one.
    #[tokio::test]
    async fn stopping_the_agent_releases_the_bus_handle_it_holds() {
        let node = TestNode::authenticated("s3cret");
        let config = node.config();
        let bus = astroctl_core::bus::EventBus::new();
        let store = Arc::new(crate::open_session(&config).await.expect("a session opens"));

        let (facade, agent) = start(&config, store, bus.clone(), bus.subscribe())
            .await
            .expect("the queue opens");
        let agent = agent.expect("an enabled node runs an agent");

        let mut events = bus.subscribe();
        agent.abort().await;
        drop(facade);
        drop(bus);

        // The aborted tasks are dropped by the runtime a scheduling turn later, not synchronously
        // by `abort`, so the senders they held go then.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(events.recv().await, Recv::Closed) {
                    return;
                }
            }
        })
        .await;
        assert!(
            closed.is_ok(),
            "an EventBus handle outlived the transfer agent, so the session log could not flush"
        );
    }

    // --- helpers that drive a real capture --------------------------------------------------

    async fn post(state: &AppState, path: &str) -> (StatusCode, Value) {
        post_json(state, path, serde_json::json!({})).await
    }

    async fn post_capture(state: &AppState) -> (StatusCode, Value) {
        post_json(state, "/api/camera/capture", serde_json::json!({})).await
    }

    async fn post_json(state: &AppState, path: &str, body: Value) -> (StatusCode, Value) {
        let (router, _) = crate::api::router();
        let (ws_router, _) = crate::api::ws_router();
        let app = crate::assemble(router, ws_router, state.clone());
        let request = crate::test_support::with_envelope(Request::builder().method(Method::POST))
            .uri(path)
            .header(
                header::AUTHORIZATION,
                HeaderValue::from_static("Bearer s3cret"),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request builds");
        let response = app.oneshot(request).await.expect("router responds");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// `(session_id, frame_id, sha256)` of the next frame the node saves.
    async fn wait_for_frame(
        events: &mut astroctl_core::bus::EventSubscriber,
    ) -> (String, String, String) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no frame.saved arrived");
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Recv::Event(event)) if event.topic == Topic::FrameSaved => {
                    let path =
                        std::path::PathBuf::from(event.data["path"].as_str().expect("a path"));
                    return (
                        astroctl_transfer::session_id(&path).expect("the session layout"),
                        event.data["frame_id"].as_str().expect("an id").to_owned(),
                        event.data["sha256"].as_str().expect("a sha").to_owned(),
                    );
                }
                Ok(Recv::Event(_) | Recv::Lagged { .. }) => {}
                Ok(Recv::Closed) | Err(_) => panic!("the bus closed before a frame was saved"),
            }
        }
    }
}
