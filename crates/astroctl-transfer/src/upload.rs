//! The sending half of ADR-05 — SDD §5.10.2 against the contract of §5.11.1.
//!
//! One frame, one POST: `multipart/form-data` with the `meta` part first and the `frame` part
//! second, streamed off the SD card so a 25 MB raw never exists in this process's heap. The
//! ordering is not a convention — §5.11.1 requires it, because the receiver needs the destination,
//! the declared size and the dedup answer before it can start writing.
//!
//! # What an answer means
//!
//! §5.10.1 says `failed` is terminal and is reached on "a 4xx that is not 408/429". That rule
//! predates §5.11.2, which then enumerated the answers the receiver actually gives and made two of
//! them disagree with their status class: `507 DISK_FULL` carries `retryable: true`, and a dropped
//! body is deliberately mapped to 5xx rather than 4xx. So the status class is no longer the
//! discriminator, and this module does not use it as one.
//!
//! What it uses instead is **whether the answer is about this frame**. Three codes are:
//! `CHECKSUM_MISMATCH`, `VALIDATION` and `FRAME_ID_CONFLICT` are verdicts the receiver reached by
//! looking at these bytes and this id, and re-sending them gets the identical verdict — so the row
//! parks in `failed` with an alert (§5.11.2's closing paragraph calls exactly these three
//! definitive). Everything else is about the *link or the deployment*: a 401 is a token the
//! operator can fix, a 404 is a node that has not been upgraded yet, a 507 is a disk that can be
//! emptied. Parking a frame over any of those would throw away the night's data to punish a
//! configuration mistake, and each of them becomes correct again without the frame changing at
//! all — which is the definition of retryable.
//!
//! That is a deliberate narrowing of §5.10.1's blanket rule, and it is the one place this module
//! knowingly departs from the letter of the spec.
//!
//! # The HEAD pre-flight
//!
//! §5.11.1 added `HEAD /api/ingest/{session_id}/{frame_id}` so a duplicate does not cost its full
//! body: HTTP forbids answering before the body drains, so without it a frame the node already
//! holds still costs ~200 s at 1 Mbit. It is asked before every upload and is **never a gate**:
//! any answer that is not a `204` with a matching checksum means "upload it".
//!
//! It is worth asking even on a frame that has never been offered, and the reason is not dedup: a
//! HEAD is one round trip, so it discovers an unreachable stack node in an RTT instead of after a
//! partial 25 MB write. The offline transition of §5.10.2 therefore happens promptly and without
//! spending the link on a body nobody is reading.

use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Empty, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::header::{self, HeaderValue};
use hyper::http::uri::{Authority, Scheme};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::AsyncReadExt as _;

/// The `meta` part's schema version — §5.11.1 equality-checks it, like the worker handshake.
const META_SCHEMA_VERSION: u16 = 1;

/// How much of the frame is read per chunk.
///
/// 64 KiB is half a second of a 1 Mbit link and one comfortable SD-card read. Larger chunks would
/// buy nothing (the socket is the bottleneck by three orders of magnitude) and would make the
/// abandon-at-shutdown window longer for no reason.
const CHUNK_BYTES: usize = 64 * 1024;

/// How long the pre-flight may take before it is treated as "not stored".
///
/// Short on purpose: its whole value is being cheap. A stack node that needs longer than this to
/// answer a key lookup is a stack node the upload is about to discover is unwell anyway.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(15);

/// Fixed part of the upload budget — connect, TLS-less handshake, the receiver's fsync and its
/// journal write after the last byte.
const UPLOAD_OVERHEAD: Duration = Duration::from_secs(120);

/// The throughput below which an upload is considered stalled rather than slow, in bytes/s.
///
/// 16 KiB/s is 128 kbit — an eighth of the 1 Mbit link T-HOL-1 shapes, and far below anything a
/// working VPN delivers. The budget has to be derived rather than fixed because the same 25 MB
/// frame is 20 seconds on a LAN and 200 on a tether, and PRD §8.1 has no key for it: a single
/// constant generous enough for the slow case would let a genuinely wedged upload hold the queue
/// for an hour, and one tight enough for the fast case would abandon every real night.
const MIN_THROUGHPUT_BYTES_PER_S: u64 = 16 * 1024;

/// What the stack node said about a frame it was offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `200` and the echoed checksum matched ours (§5.10.2). `duplicate` means the node already
    /// held those bytes, which counts as acked — the archive has the frame either way.
    Acked {
        /// The checksum the node echoed, verified equal to ours.
        sha256: String,
        /// Whether the node already held the bytes.
        duplicate: bool,
    },
    /// `200`, but the echoed checksum is not the one we sent.
    ///
    /// Not a verdict about the frame and not a transport failure: it means this exchange cannot be
    /// interpreted at all, so the frame is neither acked nor refused. The caller re-offers it a
    /// bounded number of times and then parks it, because a peer that keeps answering
    /// incomprehensibly will not start making sense on the tenth try.
    EchoMismatch {
        /// What we sent.
        expected: String,
        /// What came back.
        echoed: String,
    },
    /// The link, the far node, or its disk. Try again after a backoff; never terminal.
    Retry(RetryReason),
    /// A definitive verdict about *this frame*. Terminal (§5.11.2).
    Refused(Refusal),
}

/// Why an upload will be tried again, and what to tell the operator once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryReason {
    /// Alert code, emitted once on the transition rather than once per attempt (§5.10.2).
    pub code: &'static str,
    /// Operator-facing explanation.
    pub message: String,
}

impl RetryReason {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A refusal that re-sending cannot change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The error code the stack node returned, or a locally-determined one.
    pub code: String,
    /// Operator-facing explanation, including the receiver's own message where there was one.
    pub message: String,
}

impl Refusal {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// The three receiver verdicts that are about the frame rather than the link (§5.11.2).
///
/// Everything not in this list is retried, including every other 4xx. See the module docs for why
/// the list is shorter than §5.10.1's "any 4xx that is not 408/429".
const TERMINAL_CODES: [&str; 3] = ["CHECKSUM_MISMATCH", "VALIDATION", "FRAME_ID_CONFLICT"];

/// What the pre-flight learned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preflight {
    /// The node holds this frame with this checksum.
    Stored {
        /// The checksum it reported.
        sha256: String,
    },
    /// The node does not hold it, or could not say. Either way: upload.
    Upload,
}

/// One frame, as the uploader needs to describe it on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameUpload {
    /// Session directory name.
    pub session_id: String,
    /// Frame id — `light_00042`.
    pub frame_id: String,
    /// Absolute path of the frame.
    pub path: PathBuf,
    /// Lowercase hex SHA-256 as the frame store recorded it.
    pub sha256: String,
    /// Exact size in bytes.
    pub size_bytes: u64,
    /// Extension without the dot, lowercase — the stored name is `<frame_id>.<ext>`.
    pub ext: String,
    /// `control/quality_<id>.json`, forwarded verbatim. Opaque to both nodes by design: the field
    /// node owns the schema (§5.5) and a second declaration on the receiving side would drift.
    pub capture: Option<serde_json::Value>,
    /// `{target, equipment, created_ts}` projected out of `session.json` (§5.11.3).
    pub session: Option<serde_json::Value>,
}

/// The `meta` part, spelled exactly as §5.11.1 fixes it.
///
/// Serialized by hand into a `serde_json::Map` rather than derived from a struct, because the
/// receiver is `deny_unknown_fields` **and** rejects a `null` where it expects an absent key
/// differently from an absent one: `capture` and `session` are omitted entirely when there is
/// nothing to say, which is the shape the receiver's `#[serde(default)] Option<_>` reads as
/// "nothing to say" without any ambiguity about whether we meant it.
fn meta_json(frame: &FrameUpload) -> serde_json::Value {
    let mut meta = serde_json::Map::new();
    meta.insert("v".into(), META_SCHEMA_VERSION.into());
    meta.insert("session_id".into(), frame.session_id.clone().into());
    meta.insert("frame_id".into(), frame.frame_id.clone().into());
    meta.insert("sha256".into(), frame.sha256.clone().into());
    meta.insert("size".into(), frame.size_bytes.into());
    meta.insert("ext".into(), frame.ext.clone().into());
    if let Some(capture) = &frame.capture {
        meta.insert("capture".into(), capture.clone());
    }
    if let Some(session) = &frame.session {
        meta.insert("session".into(), session.clone());
    }
    serde_json::Value::Object(meta)
}

/// The body type both requests share: a `HEAD` sends nothing, a `POST` streams.
type UploadBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// The HTTP client half of the transfer agent.
#[derive(Debug, Clone)]
pub struct Uploader {
    client: Client<HttpConnector, UploadBody>,
    upstream: Authority,
    /// `Authorization: Bearer …`, pre-rendered. `None` on a node with no token configured, which
    /// is the SDD §4.5 loopback posture and the shape the test doubles run in.
    ///
    /// PRD §8.1/§8.2 give both nodes the same `auth_token_env` (§4.5's "the shared token"), so
    /// there is no separate stack credential to configure — the same reasoning `proxy.rs` records.
    authorization: Option<HeaderValue>,
}

impl Uploader {
    /// Build an uploader aimed at a stacking server.
    ///
    /// Plain HTTP: the two nodes talk over the VPN, which is the encrypted layer (PRD §7,
    /// ADD §5.5). No connector TLS, no certificate management on a Pi — the same call
    /// `StackProxy::new` makes, for the same reason.
    #[must_use]
    pub fn new(host: &str, port: u16, token: Option<&str>) -> Self {
        Self {
            client: Client::builder(TokioExecutor::new()).build_http(),
            upstream: authority(host, port),
            authorization: token
                .map(|token| format!("Bearer {token}"))
                .and_then(|value| HeaderValue::from_str(&value).ok())
                .map(|mut value| {
                    value.set_sensitive(true);
                    value
                }),
        }
    }

    /// `http://host:port` — for log lines and alert messages.
    #[must_use]
    pub fn upstream(&self) -> String {
        format!("http://{}", self.upstream)
    }

    /// Ask whether the stack node already holds this frame (§5.11.1).
    ///
    /// Never fails: every failure — a 404, a 500, an unreachable node, a malformed header, a
    /// timeout — answers [`Preflight::Upload`]. The pre-flight is an optimisation and must never
    /// be able to *stop* a frame being sent, so there is no error path for a caller to get wrong.
    ///
    /// Today the stack node has no such route and answers `404` (M1-T12 deferred it), which lands
    /// on exactly that branch: correct behaviour now, and a saved upload the day it lands.
    pub async fn preflight(&self, session_id: &str, frame_id: &str) -> Preflight {
        let uri = match self.uri(&format!("/api/ingest/{session_id}/{frame_id}")) {
            Ok(uri) => uri,
            Err(error) => {
                tracing::debug!(%error, "the pre-flight URI would not build; uploading");
                return Preflight::Upload;
            }
        };

        let request = match self.request(Method::HEAD, uri).body(empty_body()) {
            Ok(request) => request,
            Err(error) => {
                tracing::debug!(%error, "the pre-flight request would not build; uploading");
                return Preflight::Upload;
            }
        };

        let response = match tokio::time::timeout(PREFLIGHT_TIMEOUT, self.client.request(request))
            .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::debug!(%error, "the pre-flight did not reach the stack node; uploading");
                return Preflight::Upload;
            }
            Err(_) => {
                tracing::debug!("the pre-flight timed out; uploading");
                return Preflight::Upload;
            }
        };

        if response.status() != StatusCode::NO_CONTENT {
            return Preflight::Upload;
        }
        response
            .headers()
            .get("x-astroctl-sha256")
            .and_then(|value| value.to_str().ok())
            .map(|sha| Preflight::Stored {
                sha256: sha.trim().to_ascii_lowercase(),
            })
            .unwrap_or(Preflight::Upload)
    }

    /// Offer one frame to the stack node and interpret the answer.
    ///
    /// The frame is opened and its length checked *before* the request is built, so a frame that
    /// has been truncated or removed under the queue is refused here rather than 25 MB later by
    /// the receiver — a much better diagnostic, and on a 1 Mbit link a much cheaper one.
    pub async fn upload(&self, frame: &FrameUpload) -> Outcome {
        let file = match open_frame(&frame.path, frame.size_bytes).await {
            Ok(file) => file,
            Err(refusal) => return Outcome::Refused(refusal),
        };

        let uri = match self.uri("/api/ingest") {
            Ok(uri) => uri,
            Err(error) => {
                return Outcome::Refused(Refusal::new(
                    "VALIDATION",
                    format!("cannot address the stacking server: {error}"),
                ))
            }
        };

        let boundary = boundary();
        let (body, content_type) = multipart(&boundary, frame, file);
        let request = match self
            .request(Method::POST, uri)
            .header(header::CONTENT_TYPE, content_type)
            .body(body)
        {
            Ok(request) => request,
            Err(error) => {
                return Outcome::Refused(Refusal::new(
                    "VALIDATION",
                    format!("cannot build the upload request: {error}"),
                ))
            }
        };

        // No `Content-Length`: the body is streamed, so its length is not known to the transport
        // layer, and the receiver does not need it — §5.11.1 puts the exact size in the `meta`
        // part precisely so the frame can arrive chunked and still be bounded per chunk (§5.11.2).
        let budget = upload_budget(frame.size_bytes);
        let response = match tokio::time::timeout(budget, self.client.request(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                // A `507 DISK_FULL` frequently arrives as a broken pipe rather than as a response:
                // §5.11.2 answers it *without* draining the body, deliberately, so the connection
                // closes while this side is still writing. Both are the same verdict — come back
                // later — so the transport failure needs no special case beyond saying so.
                return Outcome::Retry(RetryReason::new(
                    "STACK_UNREACHABLE",
                    format!(
                        "cannot deliver {} to the stacking server at {}: {error}",
                        frame.frame_id,
                        self.upstream()
                    ),
                ));
            }
            Err(_) => {
                return Outcome::Retry(RetryReason::new(
                    "STACK_UNREACHABLE",
                    format!(
                        "the upload of {} exceeded its {} s budget for {} bytes",
                        frame.frame_id,
                        budget.as_secs(),
                        frame.size_bytes
                    ),
                ))
            }
        };

        let status = response.status();
        let body = match response.into_body().collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) => {
                return Outcome::Retry(RetryReason::new(
                    "STACK_UNREACHABLE",
                    format!("the stacking server's answer did not finish arriving: {error}"),
                ))
            }
        };

        classify(status, &body, frame, &self.upstream())
    }

    fn request(&self, method: Method, uri: Uri) -> hyper::http::request::Builder {
        let builder = Request::builder().method(method).uri(uri);
        match &self.authorization {
            Some(value) => builder.header(header::AUTHORIZATION, value.clone()),
            None => builder,
        }
    }

    fn uri(&self, path: &str) -> Result<Uri, hyper::http::Error> {
        Uri::builder()
            .scheme(Scheme::HTTP)
            .authority(self.upstream.clone())
            .path_and_query(path)
            .build()
    }
}

/// Turn a status and a body into a verdict.
fn classify(status: StatusCode, body: &[u8], frame: &FrameUpload, upstream: &str) -> Outcome {
    if status.is_success() {
        let Ok(ack) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Outcome::Retry(RetryReason::new(
                "STACK_ERROR",
                format!("the stacking server's {status} answer was not JSON"),
            ));
        };
        let echoed = ack
            .get("sha256")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        // §5.10.2: "verify echoed sha == ours". Case-insensitive on both sides — §5.11.1 accepts
        // uppercase hex on the wire and echoes it lowercased, so a byte comparison of the raw
        // strings would fail on nothing but spelling.
        if echoed != frame.sha256.to_ascii_lowercase() {
            return Outcome::EchoMismatch {
                expected: frame.sha256.clone(),
                echoed,
            };
        }
        return Outcome::Acked {
            sha256: echoed,
            // §5.11.1: `duplicate: true` means the node already held those bytes. `stored` is
            // always `true` on a 200 and says nothing a sender can act on, so it is not read.
            duplicate: ack.get("duplicate").and_then(serde_json::Value::as_bool) == Some(true),
        };
    }

    // The error envelope of §4.2, when there is one. Not every refusal has one: a bad multipart
    // boundary is answered by the framework as plain text, and an unknown route is an empty 404.
    let envelope = serde_json::from_slice::<serde_json::Value>(body).ok();
    let code = envelope
        .as_ref()
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let message = envelope
        .as_ref()
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .map_or_else(
            || String::from_utf8_lossy(body).trim().to_owned(),
            str::to_owned,
        );

    if TERMINAL_CODES.contains(&code.as_str()) {
        return Outcome::Refused(Refusal::new(
            code,
            format!(
                "the stacking server refused {} definitively: {message}",
                frame.frame_id
            ),
        ));
    }

    let retry_code = match status {
        StatusCode::INSUFFICIENT_STORAGE => "STACK_DISK_FULL",
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "STACK_REJECTED_AUTH",
        _ => "STACK_ERROR",
    };
    Outcome::Retry(RetryReason::new(
        retry_code,
        format!(
            "the stacking server at {upstream} answered {} for {}{}",
            status.as_u16(),
            frame.frame_id,
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        ),
    ))
}

/// How long one upload may take, derived from its size.
fn upload_budget(size_bytes: u64) -> Duration {
    UPLOAD_OVERHEAD + Duration::from_secs(size_bytes / MIN_THROUGHPUT_BYTES_PER_S)
}

/// Open the frame and check it is still what the journal says it is.
async fn open_frame(path: &Path, expected: u64) -> Result<tokio::fs::File, Refusal> {
    let file = tokio::fs::File::open(path).await.map_err(|error| {
        Refusal::new(
            "FRAME_MISSING",
            format!(
                "the frame at {} cannot be read and can never be delivered: {error}",
                path.display()
            ),
        )
    })?;

    let len = file
        .metadata()
        .await
        .map_err(|error| {
            Refusal::new(
                "FRAME_MISSING",
                format!("cannot stat the frame at {}: {error}", path.display()),
            )
        })?
        .len();

    if len != expected {
        // REL-11 says a stored raw is never modified, so this is not a race the queue should wait
        // out — it is a frame that is no longer the frame the checksum describes, and no number of
        // retries will make it one again.
        return Err(Refusal::new(
            "FRAME_CHANGED",
            format!(
                "the frame at {} is {len} bytes but was recorded as {expected}; its recorded \
                 checksum no longer describes it",
                path.display()
            ),
        ));
    }
    Ok(file)
}

/// An empty body, for the pre-flight.
fn empty_body() -> UploadBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// A multipart boundary that cannot collide with the frame's own bytes.
///
/// 128 bits from the OS CSPRNG rather than a counter or a timestamp: the boundary is compared
/// against every byte of a 25 MB binary raw, and a predictable one is a delimiter an adversary —
/// or, far more likely, a sensor producing a very regular bit pattern — could reproduce inside
/// the payload, which would truncate the frame at the receiver without any error anywhere.
fn boundary() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Unreachable on any supported platform. Falling back keeps a CSPRNG outage from stopping
        // the night's transfers, at the cost of a boundary that is merely unlikely rather than
        // unpredictable — and the receiver rejects a truncated frame on its checksum regardless.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        bytes[..4].copy_from_slice(&nanos.to_le_bytes());
    }
    let mut boundary = String::from("astroctl-");
    for byte in bytes {
        boundary.push_str(&format!("{byte:02x}"));
    }
    boundary
}

/// The two parts of §5.11.1, `meta` first, with the frame streamed rather than buffered.
fn multipart(
    boundary: &str,
    frame: &FrameUpload,
    file: tokio::fs::File,
) -> (UploadBody, HeaderValue) {
    let meta = meta_json(frame).to_string();
    // `Content-Type: application/json` on the `meta` part is not required — the receiver reads it
    // with multer's `text()`, which defaults to UTF-8 — but it is what the part is, and a receiver
    // that one day sniffs types should find the truth there.
    let prologue = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"meta\"\r\n\
         Content-Type: application/json\r\n\r\n\
         {meta}\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"frame\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    );
    // No `filename` on the frame part. §5.11.2 ignores it and says why — a filename is the classic
    // place a path traversal arrives from — so sending one would be offering the receiver a string
    // it is right to distrust.
    let epilogue = format!("\r\n--{boundary}--\r\n");

    let state = SendState::Prologue {
        bytes: Bytes::from(prologue),
        file,
        remaining: frame.size_bytes,
        epilogue: Bytes::from(epilogue),
    };

    let stream = futures_util::stream::unfold(state, |state| async move { state.step().await });
    let content_type = HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}"))
        // The boundary is hex from this module, so it is always a legal header value.
        .unwrap_or_else(|_| HeaderValue::from_static("multipart/form-data"));

    (StreamBody::new(stream).boxed_unsync(), content_type)
}

/// Where the body generator is up to.
///
/// A hand-rolled state machine rather than a channel and a feeder task: the stream is polled by
/// hyper exactly as fast as the socket drains, so the read of the next 64 KiB happens when the
/// link is ready for it. A feeder task would need its own bound to achieve the same thing and
/// would add a task to abort at shutdown.
enum SendState {
    Prologue {
        bytes: Bytes,
        file: tokio::fs::File,
        remaining: u64,
        epilogue: Bytes,
    },
    Frame {
        file: tokio::fs::File,
        remaining: u64,
        epilogue: Bytes,
    },
    Done,
}

impl SendState {
    async fn step(self) -> Option<(Result<Frame<Bytes>, std::io::Error>, Self)> {
        match self {
            Self::Prologue {
                bytes,
                file,
                remaining,
                epilogue,
            } => Some((
                Ok(Frame::data(bytes)),
                Self::Frame {
                    file,
                    remaining,
                    epilogue,
                },
            )),
            Self::Frame {
                mut file,
                remaining,
                epilogue,
            } => {
                if remaining == 0 {
                    return Some((Ok(Frame::data(epilogue)), Self::Done));
                }
                let want = usize::try_from(remaining)
                    .unwrap_or(CHUNK_BYTES)
                    .min(CHUNK_BYTES);
                let mut buffer = vec![0_u8; want];
                match file.read(&mut buffer).await {
                    Ok(0) => Some((
                        // The file shrank between the length check and here. Erroring the body
                        // aborts the request rather than sending a short frame the receiver would
                        // have to diagnose from a checksum.
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!("the frame ended {remaining} bytes early"),
                        )),
                        Self::Done,
                    )),
                    Ok(read) => {
                        buffer.truncate(read);
                        Some((
                            Ok(Frame::data(Bytes::from(buffer))),
                            Self::Frame {
                                file,
                                remaining: remaining - read as u64,
                                epilogue,
                            },
                        ))
                    }
                    Err(error) => Some((Err(error), Self::Done)),
                }
            }
            Self::Done => None,
        }
    }
}

/// `host:port`, bracketing an IPv6 literal — the same rule `proxy.rs` applies, for the same
/// reason: `stacking_server.host` is validated to be a bare host or IP but may well be `::1`.
fn authority(host: &str, port: u16) -> Authority {
    let text = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Authority::from_maybe_shared(text.clone()).unwrap_or_else(|_| {
        tracing::error!(authority = %text, "stacking_server host/port is not a valid authority");
        Authority::from_static("127.0.0.1:0")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> FrameUpload {
        FrameUpload {
            session_id: "2026-07-29_ngc7000".to_owned(),
            frame_id: "light_00042".to_owned(),
            path: PathBuf::from("/data/sessions/2026-07-29_ngc7000/frames/light_00042.cr3"),
            sha256: "a".repeat(64),
            size_bytes: 11,
            ext: "cr3".to_owned(),
            capture: None,
            session: None,
        }
    }

    #[test]
    fn the_meta_part_is_exactly_the_schema_of_5_11_1() {
        let meta = meta_json(&frame());
        assert_eq!(meta["v"], 1);
        assert_eq!(meta["session_id"], "2026-07-29_ngc7000");
        assert_eq!(meta["frame_id"], "light_00042");
        assert_eq!(meta["size"], 11);
        assert_eq!(meta["ext"], "cr3");
        // Omitted, not null: the receiver is `deny_unknown_fields` and reads an absent key through
        // `#[serde(default)]`, so "nothing to say" is said by saying nothing.
        assert!(meta.get("capture").is_none(), "{meta}");
        assert!(meta.get("session").is_none(), "{meta}");

        let mut with_extras = frame();
        with_extras.capture = Some(serde_json::json!({"exposure_s": 120.0}));
        with_extras.session = Some(serde_json::json!({"target": null}));
        let meta = meta_json(&with_extras);
        assert_eq!(meta["capture"]["exposure_s"], 120.0);
        assert!(meta["session"].is_object());
    }

    #[test]
    fn a_matching_echo_is_an_ack_and_a_duplicate_counts_as_one() {
        let frame = frame();
        let body = serde_json::json!({
            "v": 1, "session_id": frame.session_id, "frame_id": frame.frame_id,
            "sha256": frame.sha256, "stored": true, "duplicate": false,
        })
        .to_string();
        assert_eq!(
            classify(StatusCode::OK, body.as_bytes(), &frame, "u"),
            Outcome::Acked {
                sha256: frame.sha256.clone(),
                duplicate: false
            }
        );

        // §5.11.1: `duplicate: true` means the node already holds those bytes — the archive has
        // the frame, which is all an ack ever claimed.
        let body = body.replace("\"duplicate\":false", "\"duplicate\":true");
        assert_eq!(
            classify(StatusCode::OK, body.as_bytes(), &frame, "u"),
            Outcome::Acked {
                sha256: frame.sha256.clone(),
                duplicate: true
            }
        );
    }

    /// The receiver accepts uppercase hex and echoes it lowercased (§5.11.1), so a byte comparison
    /// would fail on spelling alone.
    #[test]
    fn the_echo_comparison_ignores_hex_case() {
        let mut frame = frame();
        frame.sha256 = "A".repeat(64);
        let body = serde_json::json!({"sha256": "a".repeat(64), "duplicate": false}).to_string();
        assert!(matches!(
            classify(StatusCode::OK, body.as_bytes(), &frame, "u"),
            Outcome::Acked { .. }
        ));
    }

    #[test]
    fn a_wrong_echo_is_neither_an_ack_nor_a_refusal() {
        let frame = frame();
        let body = serde_json::json!({"sha256": "b".repeat(64), "duplicate": false}).to_string();
        assert_eq!(
            classify(StatusCode::OK, body.as_bytes(), &frame, "u"),
            Outcome::EchoMismatch {
                expected: frame.sha256.clone(),
                echoed: "b".repeat(64),
            }
        );
    }

    /// The three verdicts §5.11.2 calls definitive, and nothing else.
    #[test]
    fn only_a_verdict_about_this_frame_is_terminal() {
        let frame = frame();
        for (status, code) in [
            (StatusCode::UNPROCESSABLE_ENTITY, "CHECKSUM_MISMATCH"),
            (StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION"),
            (StatusCode::CONFLICT, "FRAME_ID_CONFLICT"),
        ] {
            let body = serde_json::json!({"v": 1, "code": code, "message": "no"}).to_string();
            assert!(
                matches!(classify(status, body.as_bytes(), &frame, "u"), Outcome::Refused(r) if r.code == code),
                "{code} must park the frame"
            );
        }
    }

    /// Every other refusal is about the link or the deployment: a token the operator can fix, a
    /// node that has not been upgraded, a disk that can be emptied. Parking the night's frames
    /// over any of those would be the expensive mistake.
    #[test]
    fn a_refusal_about_the_link_is_retried_however_it_is_spelled() {
        let frame = frame();
        let cases = [
            (StatusCode::UNAUTHORIZED, "AUTH", "STACK_REJECTED_AUTH"),
            (
                StatusCode::INSUFFICIENT_STORAGE,
                "DISK_FULL",
                "STACK_DISK_FULL",
            ),
            (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", "STACK_ERROR"),
            (StatusCode::BAD_GATEWAY, "", "STACK_ERROR"),
            (StatusCode::TOO_MANY_REQUESTS, "", "STACK_ERROR"),
        ];
        for (status, code, expected) in cases {
            let body = serde_json::json!({"v": 1, "code": code, "message": "m"}).to_string();
            match classify(status, body.as_bytes(), &frame, "u") {
                Outcome::Retry(reason) => assert_eq!(reason.code, expected, "for {status}"),
                other => panic!("{status} must be retryable, got {other:?}"),
            }
        }
    }

    /// A 404 today is a stack node that has not shipped the route yet, not a lost frame. An empty
    /// body must not become a refusal by accident.
    #[test]
    fn an_empty_bodied_404_is_retried() {
        let frame = frame();
        match classify(StatusCode::NOT_FOUND, b"", &frame, "stack:8471") {
            Outcome::Retry(reason) => {
                assert_eq!(reason.code, "STACK_ERROR");
                assert!(reason.message.contains("404"), "{}", reason.message);
            }
            other => panic!("a 404 must be retryable, got {other:?}"),
        }
    }

    #[test]
    fn the_upload_budget_scales_with_the_frame() {
        // A 25 MB frame at the 128 kbit floor: generous enough for the shaped link of T-HOL-1,
        // bounded enough that a wedged upload does not hold the queue for an hour.
        let budget = upload_budget(25 * 1024 * 1024);
        assert!(budget.as_secs() > 1_600, "{budget:?}");
        assert!(budget.as_secs() < 1_900, "{budget:?}");
        // …and an empty queue entry still gets the fixed overhead.
        assert_eq!(upload_budget(0), UPLOAD_OVERHEAD);
    }

    #[test]
    fn every_boundary_is_different() {
        let a = boundary();
        let b = boundary();
        assert_ne!(a, b);
        assert!(a.starts_with("astroctl-"));
        assert_eq!(a.len(), "astroctl-".len() + 32);
    }

    #[tokio::test]
    async fn a_frame_that_changed_under_the_queue_is_refused_locally() {
        let dir = crate::test_support::TempDir::new();
        let path = dir.path().join("light_00001.cr3");
        tokio::fs::write(&path, b"eleven byte").await.unwrap();

        assert!(open_frame(&path, 11).await.is_ok());

        let refusal = open_frame(&path, 25)
            .await
            .expect_err("a size change is refused");
        assert_eq!(refusal.code, "FRAME_CHANGED");

        let refusal = open_frame(&dir.path().join("gone.cr3"), 11)
            .await
            .expect_err("a missing frame is refused");
        assert_eq!(refusal.code, "FRAME_MISSING");
    }

    #[test]
    fn an_ipv6_upstream_is_bracketed() {
        assert_eq!(authority("::1", 8471).as_str(), "[::1]:8471");
        assert_eq!(authority("stack.vpn", 8471).as_str(), "stack.vpn:8471");
    }
}
