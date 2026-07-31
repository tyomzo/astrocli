//! The field node's REST surface, as a client sees it.
//!
//! Every path, header name and JSON key in this file is a literal. None of it is derived from the
//! server's types — see the crate docs for why that duplication is the point rather than a smell.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Request headers of the M1-T10 command envelope (SDD §5.8.1). No `X-` prefix; the convention
/// was dropped by RFC 6648 and the server does not accept one.
const HEADER_COMMAND_ID: &str = "astroctl-command-id";
const HEADER_ISSUED_AT: &str = "astroctl-issued-at";
/// Response header echoed on every reply, and the one that says a reply came from the ledger.
pub const HEADER_SERVER_TIME: &str = "astroctl-server-time";
pub const HEADER_REPLAYED: &str = "astroctl-replayed";

/// Per-request timeout.
///
/// Well above anything the field node is allowed to take (the proxy's own operator-facing budget
/// is 30 s) so that a timeout here always means the node stopped answering rather than that a
/// scenario was impatient. Scenarios that care about *latency* measure it; they never rely on this.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// Distinguishes command ids within one process. The rest of the id is the clock and the pid, so
/// two suites running against two pairs cannot collide either.
static COMMAND_SEQ: AtomicU64 = AtomicU64::new(0);

/// One reply, with what a scenario needs to assert about it.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub server_time: Option<String>,
    pub replayed: bool,
    /// Wall-clock time from just before the request was issued to the body being fully read.
    ///
    /// The whole exchange including the body, not just the headers: the operator's e-stop is not
    /// acknowledged until the response is in hand, and a measurement that stopped at the status
    /// line would flatter a node whose body write was the slow part.
    pub elapsed: Duration,
}

impl Reply {
    /// The body as JSON.
    ///
    /// # Panics
    ///
    /// When the body is not JSON, quoting it — an HTML error page or an empty body from a
    /// misrouted request is the usual cause and the text is the diagnosis.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|error| {
            panic!(
                "expected JSON, got {} bytes that did not parse ({error}): {}",
                self.body.len(),
                self.body
            )
        })
    }

    /// Assert the status and return the body as JSON.
    ///
    /// # Panics
    ///
    /// When the status differs, quoting the body — which for this API is the error envelope, so
    /// the failure message carries the server's own `code` and `message` rather than just a number.
    #[must_use]
    pub fn expect(&self, status: u16) -> Value {
        assert_eq!(
            self.status, status,
            "expected {status}, got {}: {}",
            self.status, self.body
        );
        if self.body.is_empty() {
            return Value::Null;
        }
        self.json()
    }

    /// Assert the status and discard the body.
    ///
    /// For the routes that answer with no body at all — `liveview/start` and `liveview/stop` both
    /// return `202` and nothing else — where [`expect`](Self::expect)'s `#[must_use]` value is a
    /// `Value::Null` the caller has no use for.
    ///
    /// # Panics
    ///
    /// When the status differs.
    pub fn expect_status(&self, status: u16) {
        assert_eq!(
            self.status, status,
            "expected {status}, got {}: {}",
            self.status, self.body
        );
    }

    /// The `code` of an error envelope.
    ///
    /// # Panics
    ///
    /// When the body is not an envelope.
    #[must_use]
    pub fn error_code(&self) -> String {
        self.json()
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "no `code` in what should be an error envelope: {}",
                    self.body
                )
            })
            .to_owned()
    }
}

/// An accepted capture.
#[derive(Debug, Clone)]
pub struct Capture {
    pub frame_id: String,
    pub correlation_id: String,
    /// When the request that was *accepted* went out — the start of the operator's wait, and
    /// therefore what T-E2E-1's ten-second preview budget is measured from. Not the start of the
    /// first attempt: time spent queued behind a previous capture is not this capture's latency.
    pub accepted_at: Instant,
}

/// A client for one node's HTTP API.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    /// # Panics
    ///
    /// When reqwest cannot build a client, which on this configuration means the process is out
    /// of file descriptors.
    #[must_use]
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                // Connection reuse is on by default and stays on deliberately. A scenario that
                // opened a fresh TCP connection per request would measure the handshake in every
                // latency sample, and on a shaped 1 Mbit link (T-HOL-1) the handshake is most of
                // the measurement — which is a fact about `tc`, not about the node.
                .build()
                .expect("a plain HTTP client builds"),
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// GET, returning the raw body, for readiness polling where a failure is expected and normal.
    pub async fn get_text(&self, path: &str) -> Result<String, reqwest::Error> {
        self.http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await?
            .text()
            .await
    }

    /// GET.
    ///
    /// # Panics
    ///
    /// On a transport failure — the node not answering at all is never something a scenario
    /// asserts *about* except by stopping it deliberately, and those scenarios use compose.
    pub async fn get(&self, path: &str) -> Reply {
        self.send(self.http.get(format!("{}{path}", self.base)), false)
            .await
    }

    /// GET, asserting 200 and returning the JSON body.
    ///
    /// # Panics
    ///
    /// When the status is not 200.
    pub async fn get_json(&self, path: &str) -> Value {
        self.get(path).await.expect(200)
    }

    /// POST with a JSON body and a fresh command envelope.
    ///
    /// The envelope goes on every POST, including the routes classed `not_a_command` that ignore
    /// it. Sending it always is one rule instead of a table this suite would have to keep in
    /// agreement with `route_meta.rs` — and a scenario that wants to test the *absence* of an
    /// envelope has [`post_raw`](Self::post_raw) to say so out loud.
    pub async fn post(&self, path: &str, body: Option<Value>) -> Reply {
        let mut request = self.http.post(format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.send(request, true).await
    }

    /// POST without a command envelope — for asserting the 422 a `motion_initiating` route owes
    /// a client that omits one.
    pub async fn post_raw(&self, path: &str, body: Option<Value>) -> Reply {
        let mut request = self.http.post(format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.send(request, false).await
    }

    /// POST reusing a caller-chosen command id, for asserting idempotent replay.
    pub async fn post_with_command_id(&self, path: &str, body: Option<Value>, id: &str) -> Reply {
        let mut request = self
            .http
            .post(format!("{}{path}", self.base))
            .header(HEADER_COMMAND_ID, id)
            .header(HEADER_ISSUED_AT, now_rfc3339());
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.send(request, false).await
    }

    /// PUT with a JSON body and a command envelope.
    pub async fn put(&self, path: &str, body: Value) -> Reply {
        let request = self.http.put(format!("{}{path}", self.base)).json(&body);
        self.send(request, true).await
    }

    /// GET returning raw bytes — the frame preview route serves `image/jpeg`.
    ///
    /// # Panics
    ///
    /// On a transport failure.
    pub async fn get_bytes(&self, path: &str) -> (u16, Vec<u8>) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("GET {path} failed: {error}"));
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .unwrap_or_else(|error| panic!("GET {path} body failed: {error}"));
        (status, bytes.to_vec())
    }

    async fn send(&self, request: reqwest::RequestBuilder, envelope: bool) -> Reply {
        let request = if envelope {
            request
                .header(HEADER_COMMAND_ID, next_command_id())
                .header(HEADER_ISSUED_AT, now_rfc3339())
        } else {
            request
        };
        let started = Instant::now();
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("request to {} failed: {error}", self.base));
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned)
        };
        let server_time = header(HEADER_SERVER_TIME);
        let replayed = header(HEADER_REPLAYED).as_deref() == Some("true");
        let body = response.text().await.unwrap_or_default();
        Reply {
            status,
            body,
            server_time,
            replayed,
            elapsed: started.elapsed(),
        }
    }

    // ---------------------------------------------------------------------------------------
    // The operator's own vocabulary.
    //
    // Thin, and named for what the operator does rather than for the route, so a scenario reads
    // as the demo narrative it is meant to be an executable copy of (IMP §2/M1).
    // ---------------------------------------------------------------------------------------

    /// Connect the mount. Returns the `MountStatus` body.
    ///
    /// # Panics
    ///
    /// When the node refuses.
    pub async fn connect_mount(&self) -> Value {
        self.post("/api/mount/connect", None).await.expect(200)
    }

    /// Connect the camera. Returns the `CameraStatus` body.
    ///
    /// # Panics
    ///
    /// When the node refuses.
    pub async fn connect_camera(&self) -> Value {
        self.post("/api/camera/connect", None).await.expect(200)
    }

    /// Slew to a target. Returns the accepted correlation id (the route answers 202).
    ///
    /// # Panics
    ///
    /// When the node does not accept the goto.
    pub async fn goto(&self, ra_hours: f64, dec_degrees: f64) -> String {
        let accepted = self
            .post(
                "/api/mount/goto",
                Some(json!({ "ra_hours": ra_hours, "dec_degrees": dec_degrees })),
            )
            .await
            .expect(202);
        accepted["correlation_id"]
            .as_str()
            .expect("a goto is accepted with a correlation id")
            .to_owned()
    }

    /// Take one frame, waiting for any in-flight capture to release the slot first.
    ///
    /// # Why the wait is part of the operation
    ///
    /// `frame.saved` fires at `commit_frame` — the moment the bytes are durable — and deliberately
    /// *not* after the sidecar metadata and the local preview (SDD change note 1.19.0: the event
    /// announces the thing that cannot be re-made). The capture task outlives it by a little, and
    /// the camera takes one capture at a time, so a scenario that captures in a loop and waits on
    /// `frame.saved` between frames will intermittently meet `409 BUSY` on the next one.
    ///
    /// That 409 is the API working exactly as designed, which is why this waits for the slot
    /// rather than asserting the first attempt succeeds. Found by the flake gate: this scenario
    /// passed on its first run and failed on its second.
    ///
    /// # Panics
    ///
    /// When the node answers anything but 202 or 409, or when the slot never frees.
    pub async fn capture(&self) -> Capture {
        let deadline = Instant::now() + Duration::from_mins(3);
        loop {
            let accepted_at = Instant::now();
            let reply = self.post("/api/camera/capture", None).await;
            match reply.status {
                202 => {
                    let body = reply.json();
                    return Capture {
                        frame_id: body["frame_id"]
                            .as_str()
                            .expect("an accepted capture names its frame")
                            .to_owned(),
                        correlation_id: body["correlation_id"]
                            .as_str()
                            .expect("an accepted capture carries a correlation id")
                            .to_owned(),
                        accepted_at,
                    };
                }
                409 => {
                    assert!(
                        Instant::now() < deadline,
                        "the capture slot never freed: {}",
                        reply.body
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                other => panic!(
                    "capture answered {other}, expected 202 or 409 BUSY: {}",
                    reply.body
                ),
            }
        }
    }

    /// Set the shutter, so a scenario need not wait out the config's 30 s default.
    ///
    /// # Panics
    ///
    /// When the token is not one the simulated body offers.
    pub async fn set_shutter(&self, shutter: &str) -> Value {
        self.put("/api/camera/settings", json!({ "shutter": shutter }))
            .await
            .expect(200)
    }

    /// The e-stop (PRF-12: ≤ 20 ms handler-to-wire). Returns the whole reply, because what a
    /// scenario asserts about this route is how long it took.
    pub async fn estop(&self) -> Reply {
        // No command envelope: `/api/mount/estop` is the one route classed `exempt`, and sending
        // an envelope it must never require would let this suite pass against a node that had
        // started requiring one.
        self.post_raw("/api/mount/estop", None).await
    }

    /// The current session, including its frame list.
    ///
    /// # Panics
    ///
    /// When the route does not answer 200.
    pub async fn session(&self) -> Value {
        self.get_json("/api/session/current").await
    }

    /// The transfer queue's own view of itself (SDD §5.10.4).
    ///
    /// # Panics
    ///
    /// When the route does not answer 200.
    pub async fn transfer_status(&self) -> Value {
        self.get_json("/api/transfer/status").await
    }

    /// A single-use WebSocket ticket (SDD §4.5): a browser cannot put a header on an upgrade, so
    /// `/ws` and `/ws/liveview` are authenticated by a query parameter instead. Each socket needs
    /// its own — a ticket is spent by the upgrade that presents it.
    ///
    /// # Panics
    ///
    /// When the route does not issue one.
    pub async fn ws_ticket(&self) -> String {
        let issued = self.post("/api/auth/ws-ticket", None).await.expect(200);
        issued["ticket"]
            .as_str()
            .expect("a ticket is a string")
            .to_owned()
    }
}

/// RFC 3339 with milliseconds and a `Z`, which is what SDD §2 specifies and what the envelope's
/// staleness check parses.
fn now_rfc3339() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// A command id in the 8..=128 character window the envelope requires.
fn next_command_id() -> String {
    let seq = COMMAND_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("e2e-{}-{seq:012}", std::process::id())
}
