//! The wire contract of SDD §5.11.1, and the drain loop of §5.10.2, over a real socket.
//!
//! The receiver here is a **stand-in**, not `astroctl-stack` — ADD §5.6 rule 5 forbids the edge,
//! and the two nodes are meant to share only the HTTP contract. So this double parses the body
//! with the same library the real node does (`axum`'s `Multipart`, over `multer`) and applies the
//! same `deny_unknown_fields` strictness, which is what makes "the double accepted it" mean
//! something. It also *records what it was asked*, which is the part a unit test cannot see: that
//! the pre-flight really is asked before the body, and that a frame the node already holds costs
//! one round trip instead of a whole raw.
//!
//! The end-to-end proof against the real stack node is a live run, recorded in the M1-T11 result
//! note. This file is what keeps it from silently rotting.

use std::sync::{Arc, Mutex};

use astroctl_core::bus::{EventBus, Recv};
use astroctl_core::event::Topic;
use astroctl_transfer::journal::{Journal, NewEntry, State};
use astroctl_transfer::upload::{FrameUpload, Outcome, Preflight};
use astroctl_transfer::{AgentConfig, TransferAgent, TransferQueue, Uploader};
use axum::extract::{Multipart, Path as AxumPath, State as AxumState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;

// ---------------------------------------------------------------------------------------------
// The stand-in stack node
// ---------------------------------------------------------------------------------------------

/// How the double should answer the next `POST /api/ingest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// Store it and echo the checksum, as §5.11.1 requires.
    Store,
    /// Echo a checksum that is not the sender's — §5.10.2's verification must catch it.
    EchoWrongSha,
    /// `422 VALIDATION` — definitive about the frame.
    RefuseValidation,
    /// `507 DISK_FULL`, `retryable: true` — come back later.
    DiskFull,
    /// `500` — a transport-ish failure the sender must not treat as terminal.
    Internal,
}

#[derive(Clone)]
struct Double {
    /// Every request line the node was sent, in order.
    log: Arc<Mutex<Vec<String>>>,
    /// `(session_id, frame_id) -> sha256` of what the node holds.
    stored: Arc<Mutex<Vec<(String, String, String)>>>,
    answer: Arc<Mutex<Answer>>,
    /// Whether `HEAD /api/ingest/…` is implemented at all. The real node does not implement it
    /// yet and answers 404 (M1-T12 deferred it), which is the branch that has to work today.
    preflight_implemented: bool,
}

impl Double {
    fn new(preflight_implemented: bool) -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            stored: Arc::new(Mutex::new(Vec::new())),
            answer: Arc::new(Mutex::new(Answer::Store)),
            preflight_implemented,
        }
    }

    fn record(&self, line: impl Into<String>) {
        self.log.lock().unwrap().push(line.into());
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn set(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    fn hold(&self, session_id: &str, frame_id: &str, sha256: &str) {
        self.stored.lock().unwrap().push((
            session_id.to_owned(),
            frame_id.to_owned(),
            sha256.to_owned(),
        ));
    }

    fn lookup(&self, session_id: &str, frame_id: &str) -> Option<String> {
        self.stored
            .lock()
            .unwrap()
            .iter()
            .find(|(s, f, _)| s == session_id && f == frame_id)
            .map(|(_, _, sha)| sha.clone())
    }
}

/// The `meta` part, declared exactly as `astroctl-stack` declares it — strict, so a sender that
/// invents a key fails here the way it would fail there.
///
/// Some fields are never read. That is the point: they are here so that `deny_unknown_fields`
/// has the same surface as the real receiver's, and a sender that stopped emitting one would fail
/// to deserialize rather than quietly drop it.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FrameMeta {
    v: u16,
    session_id: String,
    frame_id: String,
    sha256: String,
    size: u64,
    ext: String,
    #[serde(default)]
    capture: Option<serde_json::Value>,
    #[serde(default)]
    session: Option<SessionMeta>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct SessionMeta {
    #[serde(default)]
    target: Option<serde_json::Value>,
    #[serde(default)]
    equipment: Option<serde_json::Value>,
    #[serde(default)]
    created_ts: Option<String>,
}

fn refusal(status: StatusCode, code: &str, retryable: bool) -> Response {
    (
        status,
        Json(serde_json::json!({
            "v": 1, "code": code, "message": "from the stand-in stack node",
            "detail": null, "retryable": retryable,
        })),
    )
        .into_response()
}

async fn ingest(AxumState(double): AxumState<Double>, mut multipart: Multipart) -> Response {
    double.record("POST /api/ingest");

    // §5.11.1: `meta` **must** precede `frame`.
    let Ok(Some(first)) = multipart.next_field().await else {
        return refusal(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION", false);
    };
    if first.name() != Some("meta") {
        return refusal(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION", false);
    }
    let Ok(text) = first.text().await else {
        return refusal(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION", false);
    };
    let meta: FrameMeta = match serde_json::from_str(&text) {
        Ok(meta) => meta,
        Err(error) => {
            double.record(format!("meta rejected: {error}"));
            return refusal(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION", false);
        }
    };
    assert_eq!(meta.v, 1, "the schema version is equality-checked");
    // Opaque, but it must at least deserialize into the receiver's strict shape.
    let _ = (
        &meta.capture,
        meta.session
            .as_ref()
            .map(|s| (&s.target, &s.equipment, &s.created_ts)),
    );

    let Ok(Some(second)) = multipart.next_field().await else {
        return refusal(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION", false);
    };
    if second.name() != Some("frame") {
        return refusal(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION", false);
    }

    // Hash as the bytes arrive, exactly as §5.11.2 does, so "the frame survived the stream" is
    // asserted on the bytes rather than on a length.
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut size = 0_u64;
    let mut field = second;
    while let Ok(Some(chunk)) = field.chunk().await {
        size += chunk.len() as u64;
        sha2::Digest::update(&mut hasher, &chunk);
    }
    let received = format!("{:x}", sha2::Digest::finalize(hasher));
    double.record(format!(
        "frame arrived: {size} bytes, sha {}",
        &received[..16]
    ));

    match *double.answer.lock().unwrap() {
        Answer::RefuseValidation => {
            return refusal(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION", false)
        }
        Answer::DiskFull => return refusal(StatusCode::INSUFFICIENT_STORAGE, "DISK_FULL", true),
        Answer::Internal => return refusal(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", true),
        Answer::EchoWrongSha => {
            return Json(serde_json::json!({
                "v": 1, "session_id": meta.session_id, "frame_id": meta.frame_id,
                "sha256": "f".repeat(64), "stored": true, "duplicate": false,
            }))
            .into_response()
        }
        Answer::Store => {}
    }

    assert_eq!(
        size, meta.size,
        "the declared size must be the delivered size"
    );
    assert_eq!(
        received, meta.sha256,
        "the declared checksum must be the delivered bytes'"
    );

    let duplicate = double.lookup(&meta.session_id, &meta.frame_id).is_some();
    if !duplicate {
        double.hold(&meta.session_id, &meta.frame_id, &meta.sha256);
    }
    Json(serde_json::json!({
        "v": 1, "session_id": meta.session_id, "frame_id": meta.frame_id,
        "sha256": meta.sha256, "stored": true, "duplicate": duplicate,
    }))
    .into_response()
}

async fn preflight(
    AxumState(double): AxumState<Double>,
    AxumPath((session_id, frame_id)): AxumPath<(String, String)>,
) -> Response {
    double.record(format!("HEAD /api/ingest/{session_id}/{frame_id}"));
    if !double.preflight_implemented {
        // What the real stack node does today.
        return StatusCode::NOT_FOUND.into_response();
    }
    match double.lookup(&session_id, &frame_id) {
        Some(sha) => (StatusCode::NO_CONTENT, [("X-Astroctl-Sha256", sha)]).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Bind the double on an ephemeral port and return it with its port.
async fn start(preflight_implemented: bool) -> (Double, u16) {
    let double = Double::new(preflight_implemented);
    let app = axum::Router::new()
        .route("/api/ingest", post(ingest))
        .route(
            "/api/ingest/{session_id}/{frame_id}",
            get(|| async { StatusCode::METHOD_NOT_ALLOWED }).head(preflight),
        )
        .with_state(double.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port is available");
    let port = listener.local_addr().expect("bound").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (double, port)
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astroctl-wire-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("2026-07-29_ngc7000").join("frames")).expect("mkdir");
        Self(path)
    }

    /// Write a frame into the §5.5 layout and return its path, checksum and size.
    fn frame(&self, frame_id: &str, bytes: &[u8]) -> (std::path::PathBuf, String, u64) {
        let path = self
            .0
            .join("2026-07-29_ngc7000")
            .join("frames")
            .join(format!("{frame_id}.fits"));
        std::fs::write(&path, bytes).expect("write frame");
        let sha = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(bytes));
        (path, sha, bytes.len() as u64)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A frame big enough that it cannot cross in one chunk — the streaming path, not a short-circuit.
fn big_frame() -> Vec<u8> {
    (0..(300 * 1024_usize)).map(|i| (i % 251) as u8).collect()
}

// ---------------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_frame_crosses_intact_with_meta_first_and_the_ack_is_verified() {
    let scratch = Scratch::new();
    let bytes = big_frame();
    let (path, sha, size) = scratch.frame("light_00042", &bytes);
    let (double, port) = start(false).await;

    let upload = FrameUpload {
        session_id: "2026-07-29_ngc7000".to_owned(),
        frame_id: "light_00042".to_owned(),
        path,
        sha256: sha.clone(),
        size_bytes: size,
        ext: "fits".to_owned(),
        capture: Some(serde_json::json!({"exposure_s": 120.0, "settings": {"iso": "1600"}})),
        session: Some(serde_json::json!({
            "target": {"name": "NGC 7000"},
            "equipment": {"telescope": "SW 200PDS"},
            "created_ts": "2026-07-29T18:00:00.000Z",
        })),
    };

    let uploader = Uploader::new("127.0.0.1", port, Some("s3cret"));
    let outcome = uploader.upload(&upload).await;
    assert_eq!(
        outcome,
        Outcome::Acked {
            sha256: sha.clone(),
            duplicate: false
        },
        "requests: {:?}",
        double.requests()
    );

    // The double hashed what actually arrived, so this asserts the bytes survived the streaming
    // body rather than that the sender believed they did.
    let log = double.requests();
    assert!(
        log.iter()
            .any(|l| l.contains(&format!("frame arrived: {size} bytes"))),
        "{log:?}"
    );

    // The second offer of bytes the node already holds is a duplicate, and §5.11.1 says a
    // duplicate means the archive has the frame — which is all an ack ever claimed.
    assert_eq!(
        uploader.upload(&upload).await,
        Outcome::Acked {
            sha256: sha,
            duplicate: true
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pre_flight_reads_a_stored_checksum_and_every_failure_means_upload() {
    let (double, port) = start(true).await;
    let uploader = Uploader::new("127.0.0.1", port, Some("s3cret"));

    // Nothing stored yet.
    assert_eq!(
        uploader
            .preflight("2026-07-29_ngc7000", "light_00042")
            .await,
        Preflight::Upload
    );

    double.hold("2026-07-29_ngc7000", "light_00042", &"ab".repeat(32));
    assert_eq!(
        uploader
            .preflight("2026-07-29_ngc7000", "light_00042")
            .await,
        Preflight::Stored {
            sha256: "ab".repeat(32)
        }
    );

    // A node that has not shipped the route yet — which is every stack node today — answers 404,
    // and §5.11.1 makes that "not stored, upload" rather than an error.
    let (unimplemented, port) = start(false).await;
    unimplemented.hold("2026-07-29_ngc7000", "light_00042", &"ab".repeat(32));
    let uploader = Uploader::new("127.0.0.1", port, Some("s3cret"));
    assert_eq!(
        uploader
            .preflight("2026-07-29_ngc7000", "light_00042")
            .await,
        Preflight::Upload,
        "a 404 pre-flight must never stop a frame being sent"
    );

    // …and so does a node that is not there at all. Port 1 on loopback refuses at once.
    let uploader = Uploader::new("127.0.0.1", 1, Some("s3cret"));
    assert_eq!(
        uploader
            .preflight("2026-07-29_ngc7000", "light_00042")
            .await,
        Preflight::Upload
    );
}

// ---------------------------------------------------------------------------------------------
// The drain loop
// ---------------------------------------------------------------------------------------------

/// Build a queue with `count` frames on disk and enqueued, plus the double they upload to.
async fn queued(
    scratch: &Scratch,
    count: usize,
    preflight_implemented: bool,
) -> (Arc<TransferQueue>, Double, u16, EventBus) {
    let (double, port) = start(preflight_implemented).await;
    let journal = Journal::open(scratch.0.join("queue").join("transfer.db"))
        .await
        .expect("a fresh journal opens");
    let bus = EventBus::new();

    for n in 1..=count {
        let frame_id = format!("light_{n:05}");
        let (path, sha256, size_bytes) = scratch.frame(&frame_id, &big_frame());
        journal
            .enqueue(NewEntry {
                session_id: "2026-07-29_ngc7000".to_owned(),
                frame_id,
                path,
                sha256,
                size_bytes,
            })
            .await
            .expect("enqueue");
    }

    (
        Arc::new(TransferQueue::new(journal, bus.clone())),
        double,
        port,
        bus,
    )
}

fn agent(queue: &Arc<TransferQueue>, port: u16) -> TransferAgent {
    TransferAgent::spawn(
        Arc::clone(queue),
        AgentConfig {
            retry_interval: std::time::Duration::from_millis(50),
            uploader: Uploader::new("127.0.0.1", port, Some("s3cret")),
        },
        astroctl_core::config::PacingConfig {
            bandwidth_cap_mbps: None,
            interactive_floor_pct: 20.0,
            interactive_window_seconds: 10,
        },
    )
}

/// Wait until the queue reports nothing owed, or give up.
async fn drained(queue: &TransferQueue) -> bool {
    wait_until(queue, |entries| {
        entries.iter().all(|e| e.state == State::Acked)
    })
    .await
}

/// Poll the journal until `ready`, with a budget generous enough that a loaded build machine
/// cannot make the assertion flake and short enough that a genuine hang is still a test failure
/// rather than a hung suite.
async fn wait_until(
    queue: &TransferQueue,
    ready: impl Fn(&[astroctl_transfer::Entry]) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let entries = queue.journal().entries().await.expect("entries");
        if ready(&entries) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

/// The acceptance criterion, automated: the queue drains oldest first and every frame is acked
/// exactly once. And the part only the receiver's log can show — the pre-flight really is asked
/// before the body, so a frame the node already holds costs one round trip instead of a raw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_drains_in_order_and_the_pre_flight_saves_a_duplicate_its_body() {
    let scratch = Scratch::new();
    let (queue, double, port, bus) = queued(&scratch, 3, true).await;

    // The node already holds frame 2 — the state a field node that crashed after the ack but
    // before recording it comes back in.
    let held = queue
        .journal()
        .lookup("2026-07-29_ngc7000", "light_00002")
        .await
        .unwrap()
        .unwrap();
    double.hold("2026-07-29_ngc7000", "light_00002", &held.sha256);

    let mut events = bus.subscribe();
    let running = agent(&queue, port);
    assert!(drained(&queue).await, "the queue must drain");

    let entries = queue.journal().entries().await.unwrap();
    assert!(
        entries
            .iter()
            .all(|e| e.state == State::Acked && e.reclaimable),
        "{entries:#?}"
    );

    // Ordering: the ack events come out oldest-first.
    let mut acked = Vec::new();
    while let Ok(Recv::Event(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await
    {
        if event.topic == Topic::TransferAcked {
            acked.push(event.data["frame_id"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(acked, ["light_00001", "light_00002", "light_00003"]);

    let log = double.requests();
    // Every frame was pre-flighted…
    for n in 1..=3 {
        assert!(
            log.iter()
                .any(|l| l == &format!("HEAD /api/ingest/2026-07-29_ngc7000/light_{n:05}")),
            "frame {n} was not pre-flighted: {log:?}"
        );
    }
    // …and exactly two bodies crossed: the one the node already held never did.
    let bodies = log
        .iter()
        .filter(|l| l.starts_with("frame arrived"))
        .count();
    assert_eq!(
        bodies, 2,
        "the duplicate's body must not have been sent: {log:?}"
    );

    running.abort().await;
}

/// §5.11.2's three definitive verdicts park the frame; everything else keeps it queued. The
/// difference matters more than any other decision in this crate: parking is the only
/// irreversible thing it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_definitive_refusal_parks_the_frame_and_a_full_disk_does_not() {
    let scratch = Scratch::new();
    let (queue, double, port, bus) = queued(&scratch, 1, false).await;
    let mut events = bus.subscribe();

    // First: a full disk. Retryable — the identical request succeeds once space is freed.
    double.set(Answer::DiskFull);
    let running = agent(&queue, port);
    assert!(
        wait_until(&queue, |e| e[0].attempts >= 2).await,
        "the frame must keep being retried behind a full disk"
    );

    let entry = queue.journal().entries().await.unwrap().remove(0);
    assert_eq!(entry.state, State::Queued, "a 507 must never park a frame");

    // A `500` is the same class of answer — §5.11.2 maps a body that stops arriving to 5xx
    // precisely so that abandoning a good frame over a dropped link is impossible.
    double.set(Answer::Internal);
    let before = entry.attempts;
    assert!(wait_until(&queue, |e| e[0].attempts > before).await);
    assert_eq!(
        queue.journal().entries().await.unwrap()[0].state,
        State::Queued,
        "a 500 must never park a frame either"
    );

    // Then the disk is emptied, and the same frame goes through untouched.
    double.set(Answer::Store);
    assert!(
        drained(&queue).await,
        "the queue must drain once the disk is free"
    );
    assert_eq!(
        queue.journal().entries().await.unwrap()[0].state,
        State::Acked
    );
    running.abort().await;

    // Exactly one offline alert for that whole burst of retries, and one recovery (§5.10.2).
    let mut alerts = Vec::new();
    while let Ok(Recv::Event(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
    {
        if event.topic == Topic::Alert {
            alerts.push(event.data["code"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(
        alerts,
        ["STACK_DISK_FULL", "STACK_ONLINE"],
        "a night-long outage must not produce thousands of events"
    );

    // Now a verdict about the frame itself.
    let scratch = Scratch::new();
    let (queue, double, port, _bus) = queued(&scratch, 1, false).await;
    double.set(Answer::RefuseValidation);
    let running = agent(&queue, port);

    assert!(wait_until(&queue, |e| e[0].state == State::Failed).await);
    let entry = queue.journal().entries().await.unwrap().remove(0);
    assert_eq!(
        entry.state,
        State::Failed,
        "a VALIDATION refusal is terminal"
    );
    assert!(!entry.reclaimable, "a refused frame is not on the far side");
    assert!(entry.last_error.is_some(), "a parked row must say why");
    assert_eq!(queue.snapshot().await.unwrap().failed, 1);
    running.abort().await;
}

/// §5.10.2: "verify echoed sha == ours". A `200` that echoes the wrong checksum is not an ack,
/// and the frame must not be marked reclaimable on the strength of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ack_with_the_wrong_checksum_never_marks_a_frame_delivered() {
    let scratch = Scratch::new();
    let (queue, double, port, _bus) = queued(&scratch, 1, false).await;
    double.set(Answer::EchoWrongSha);
    let running = agent(&queue, port);

    // It is re-offered a bounded number of times, then parked — never acked.
    assert!(
        wait_until(&queue, |e| e[0].state == State::Failed).await,
        "an uninterpretable ack must not be retried forever"
    );
    let entry = queue.journal().entries().await.unwrap().remove(0);
    assert_eq!(entry.state, State::Failed, "{entry:?}");
    assert!(
        !entry.reclaimable,
        "REL-13 must never mark this reclaimable"
    );
    assert!(
        entry.attempts >= 2,
        "it was re-offered before being parked: {entry:?}"
    );
    running.abort().await;
}

/// SDD §5.10.3, at the level the binary sees it: a row left `uploading` by a crash comes back as
/// `queued`, keeps its place in the queue, and carries the attempt that tells the uploader it may
/// already be on the far side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_upload_is_resumed_and_the_re_upload_is_deduplicated() {
    let scratch = Scratch::new();
    let (queue, double, port, _bus) = queued(&scratch, 2, true).await;

    // Claim the head and stop, as a SIGKILL mid-upload leaves it.
    let claimed = queue.journal().claim_next().await.unwrap().unwrap();
    assert_eq!(claimed.frame_id, "light_00001");
    // …and the far side did receive it before the crash.
    double.hold("2026-07-29_ngc7000", "light_00001", &claimed.sha256);

    assert_eq!(queue.journal().recover_interrupted().await.unwrap(), 1);
    let recovered = queue
        .journal()
        .lookup("2026-07-29_ngc7000", "light_00001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.state, State::Queued);
    assert_eq!(recovered.attempts, 1);

    let running = agent(&queue, port);
    assert!(drained(&queue).await);
    let entries = queue.journal().entries().await.unwrap();
    assert!(entries.iter().all(|e| e.state == State::Acked));
    // The resumed frame cost one round trip, not a retransmission: the pre-flight caught it.
    let bodies = double
        .requests()
        .iter()
        .filter(|l| l.starts_with("frame arrived"))
        .count();
    assert_eq!(bodies, 1, "{:?}", double.requests());
    running.abort().await;
}
