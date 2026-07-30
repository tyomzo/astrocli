//! Message set and framing for protocol v1 (SDD §5.12.1).
//!
//! One JSON object per line on the child's stdin/stdout, UTF-8, newline-delimited. Line framing
//! rather than length prefixing is deliberate: a developer can pipe messages into
//! `compute_worker.py` from a shell and read what comes back, which is the difference between
//! debugging a worker and guessing at one.
//!
//! Everything here is inert — serde types and two functions — so that a consumer which only
//! needs to *speak* the protocol links no process management (see the crate docs and ADD §5.6
//! rule 6).
//!
//! # Leniency, deliberately asymmetric
//!
//! Unknown fields are ignored on decode and no message carries `deny_unknown_fields`. That is
//! not laziness. [`ToWorker::Hello`]/[`FromWorker::Hello`] must survive contact with a worker
//! built against a *different* protocol version far enough for the version numbers to be
//! compared, because a version mismatch is the one failure SDD §5.12.2 wants reported with both
//! numbers and an actionable log line. A strict decoder turns "worker speaks v2, backbone
//! speaks v1" into "malformed line", which is the same message a corrupt pipe produces and
//! points the operator at nothing.
//!
//! What is *not* lenient is the shape of a result: see [`FromWorker::validate`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use astroctl_core::error::ErrorCode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The protocol version this backbone speaks (SDD §5.12.2).
///
/// Equality is required at the handshake — not a compatibility range. Bumping this without
/// bumping `PROTO_VERSION` in `workers/astroctl_ipc.py` is exactly the drift ADR-13 exists to
/// catch, and it is caught at worker startup rather than on the first job.
pub const PROTO_VERSION: u16 = 1;

/// Largest single frame either side will accept, newline included.
///
/// SDD §5.12.1 sets no bound. One is needed anyway: pixel data never travels this channel
/// (frames are passed by path), so a megabyte is already three orders of magnitude more than
/// any legitimate message, and without a cap a worker that loses its framing streams its entire
/// heap into the backbone's memory one `\n`-free byte at a time.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Backbone-assigned job identifier. Unique within one supervisor, not across restarts.
pub type JobId = u64;

/// Ping/pong correlation value. Monotonic per worker session.
pub type Nonce = u64;

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// Backbone → worker (SDD §5.12.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToWorker {
    /// Opens the session; the worker answers with its own [`FromWorker::Hello`].
    Hello {
        /// The backbone's [`PROTO_VERSION`].
        proto_version: u16,
    },
    /// Work to do. `paths` carries the frames by filesystem path — never their contents.
    Job {
        /// Correlates every [`FromWorker::Progress`] and the [`FromWorker::Result`].
        id: JobId,
        /// What kind of work.
        kind: JobKind,
        /// Kind-specific parameters; free-form so a job type can gain a knob without a
        /// protocol version bump.
        params: serde_json::Value,
        /// Input frames, absolute paths on the stacking server's filesystem.
        paths: Vec<PathBuf>,
    },
    /// Asks the worker to abandon a job. Advisory: a worker that ignores it is killed when
    /// `workers.job_timeout_seconds` runs out a second time.
    Cancel {
        /// The job to abandon.
        id: JobId,
    },
    /// Liveness probe; the worker answers [`FromWorker::Pong`] with the same nonce.
    Ping {
        /// Echoed back in the pong.
        nonce: Nonce,
    },
    /// Asks the worker to exit cleanly. Sent before the supervisor closes stdin.
    Shutdown,
}

/// Worker → backbone (SDD §5.12.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromWorker {
    /// The worker's half of the handshake.
    Hello {
        /// The version `workers/astroctl_ipc.py` was built against.
        proto_version: u16,
        /// What this worker can do — reported, never assumed.
        capabilities: WorkerCaps,
    },
    /// Progress on an in-flight job. Advisory; a worker need not send any.
    Progress {
        /// The job being reported on.
        id: JobId,
        /// Percent complete, `0..=100`.
        pct: u8,
    },
    /// Terminal outcome of a job. Exactly one of `data` / `error` is meaningful — see
    /// [`FromWorker::validate`].
    Result {
        /// The job that finished.
        id: JobId,
        /// Whether it succeeded.
        ok: bool,
        /// Success payload; for `Preview`, `{"preview_path": …}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
        /// Failure detail, in the error vocabulary of SDD §4.2.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WorkerError>,
    },
    /// Answer to a [`ToWorker::Ping`].
    Pong {
        /// The nonce from the ping.
        nonce: Nonce,
    },
    /// A log line the worker wants in the backbone's tracing output. Distinct from the worker's
    /// stderr, which is captured too but is not part of the protocol.
    Log {
        /// `trace`/`debug`/`info`/`warn`/`error`; anything else is treated as `info`.
        level: String,
        /// The message.
        message: String,
    },
}

/// What a job asks the worker to compute.
///
/// `Preview` is the whole set in this increment (SDD §5.12.4). Registration, accumulation and
/// the post-chain arrive with Phase 2b, and adding them is a protocol version bump because the
/// worker must be able to refuse a kind it does not implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Stretch one frame and write a JPEG beside it.
    Preview,
}

/// What a worker reports it can do, at handshake time (SDD §5.12.1).
///
/// Reported rather than configured: `gpu.enabled: true` in the operator's YAML is a wish, and
/// whether CuPy actually found a device is a fact only the worker process knows (CMP-06's CPU
/// fallback is a worker-side decision).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCaps {
    /// Whether this worker has a usable GPU context.
    pub gpu: bool,
    /// VRAM the worker sees, when it has a GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_mb: Option<u64>,
    /// Library name → version, for the ones that decide which code path the worker takes
    /// (`numpy`, `cupy`, `PIL`). Ordered so the handshake log line is stable between restarts.
    #[serde(default)]
    pub libs: BTreeMap<String, String>,
}

/// A failure inside the worker, in the closed error vocabulary of SDD §4.2.
///
/// The code is [`ErrorCode`] and not a worker-private enum on purpose: a Python traceback that
/// reaches the operator should look like every other failure in the system, and the PWA already
/// switches on these strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerError {
    /// Machine-readable code, serialized `SCREAMING_SNAKE_CASE`.
    pub code: ErrorCode,
    /// Operator-facing sentence.
    pub message: String,
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

// ---------------------------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------------------------

/// Everything that can go wrong between two well-formed processes.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// A frame exceeded [`MAX_LINE_BYTES`].
    #[error(
        "IPC frame is {bytes} bytes; the limit is {} — the worker has lost its framing",
        MAX_LINE_BYTES
    )]
    TooLong {
        /// Size of the offending frame.
        bytes: usize,
    },

    /// A frame was not valid UTF-8. The protocol is UTF-8 JSON; binary on this channel means
    /// something is writing to fd 1 that should not be.
    #[error("IPC frame is not valid UTF-8 at byte {offset} — something is writing binary to the worker's stdout")]
    NotUtf8 {
        /// Byte offset of the first invalid sequence.
        offset: usize,
    },

    /// A frame did not parse as a message of the expected direction.
    #[error("malformed IPC frame: {source} (frame began `{snippet}`)")]
    Malformed {
        /// Leading bytes of the frame, for the log line.
        snippet: String,
        /// What serde objected to.
        #[source]
        source: serde_json::Error,
    },

    /// `ok` and the presence of `error` disagreed.
    #[error("job {id} reported ok={ok} but {} an error — the worker's result shape is wrong", if *ok { "carried" } else { "carried no" })]
    ContradictoryResult {
        /// The job the result claimed to be for.
        id: JobId,
        /// The `ok` flag as sent.
        ok: bool,
    },

    /// A progress report outside `0..=100`.
    #[error("job {id} reported {pct}% progress")]
    ProgressOutOfRange {
        /// The job the report claimed to be for.
        id: JobId,
        /// The value as sent.
        pct: u8,
    },

    /// A message could not be serialized. Only reachable through a `params` value that serde
    /// cannot render, e.g. a map with non-string keys.
    #[error("could not serialize an IPC message: {0}")]
    Encode(#[source] serde_json::Error),
}

impl ToWorker {
    /// Render as one protocol frame, newline included.
    ///
    /// # Errors
    /// [`ProtocolError::Encode`] if `params` is not serializable, [`ProtocolError::TooLong`] if
    /// the result exceeds [`MAX_LINE_BYTES`].
    pub fn encode(&self) -> Result<String, ProtocolError> {
        encode_frame(self)
    }

    /// Parse one frame. The trailing newline is optional.
    ///
    /// # Errors
    /// [`ProtocolError::TooLong`] or [`ProtocolError::Malformed`].
    pub fn decode(frame: &str) -> Result<Self, ProtocolError> {
        decode_frame(frame)
    }
}

impl FromWorker {
    /// Render as one protocol frame, newline included.
    ///
    /// # Errors
    /// As [`ToWorker::encode`].
    pub fn encode(&self) -> Result<String, ProtocolError> {
        encode_frame(self)
    }

    /// Parse and validate one frame. The trailing newline is optional.
    ///
    /// # Errors
    /// [`ProtocolError::TooLong`], [`ProtocolError::Malformed`], or whatever
    /// [`FromWorker::validate`] rejects.
    pub fn decode(frame: &str) -> Result<Self, ProtocolError> {
        let message: Self = decode_frame(frame)?;
        message.validate()?;
        Ok(message)
    }

    /// Reject the message shapes SDD §5.12.1's struct definition allows but no correct worker
    /// produces.
    ///
    /// `Result { ok, data, error }` can represent `ok: true` alongside an error, and `ok: false`
    /// with nothing to say why. Both are silent-wrong-answer bugs — the first hands the operator
    /// a preview that does not exist, the second an empty failure — so they are turned into a
    /// loud decode failure at the crate boundary rather than being handled at every call site.
    ///
    /// # Errors
    /// [`ProtocolError::ContradictoryResult`] or [`ProtocolError::ProgressOutOfRange`].
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            FromWorker::Result { id, ok, error, .. } if *ok == error.is_some() => {
                Err(ProtocolError::ContradictoryResult { id: *id, ok: *ok })
            }
            FromWorker::Progress { id, pct } if *pct > 100 => {
                Err(ProtocolError::ProgressOutOfRange { id: *id, pct: *pct })
            }
            _ => Ok(()),
        }
    }
}

fn encode_frame<T: Serialize>(message: &T) -> Result<String, ProtocolError> {
    let mut frame = serde_json::to_string(message).map_err(ProtocolError::Encode)?;
    // serde_json's compact form never emits a bare newline, so appending one is the whole
    // framing: no escaping pass, and no way for a message body to forge a frame boundary.
    let bytes = frame.len() + 1;
    if bytes > MAX_LINE_BYTES {
        return Err(ProtocolError::TooLong { bytes });
    }
    frame.push('\n');
    Ok(frame)
}

fn decode_frame<T: DeserializeOwned>(frame: &str) -> Result<T, ProtocolError> {
    if frame.len() > MAX_LINE_BYTES {
        return Err(ProtocolError::TooLong { bytes: frame.len() });
    }
    let body = frame.trim_end_matches(['\r', '\n']);
    serde_json::from_str(body).map_err(|source| ProtocolError::Malformed {
        snippet: snippet(body),
        source,
    })
}

/// The first 120 characters of a frame, for an error message. Bounded because the frame that
/// failed to parse may be the megabyte of garbage that failing to parse is telling us about.
fn snippet(body: &str) -> String {
    const LIMIT: usize = 120;
    match body.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body.to_owned(),
    }
}

/// Decode a frame that arrived as bytes, keeping "not UTF-8" distinguishable from "not JSON".
///
/// # Errors
/// [`ProtocolError::NotUtf8`] on invalid UTF-8, otherwise as [`FromWorker::decode`].
pub fn decode_from_worker_bytes(frame: &[u8]) -> Result<FromWorker, ProtocolError> {
    let text = std::str::from_utf8(frame).map_err(|e| ProtocolError::NotUtf8 {
        offset: e.valid_up_to(),
    })?;
    FromWorker::decode(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_frame_is_one_line_and_ends_in_a_newline() {
        let frame = ToWorker::Ping { nonce: 7 }.encode().expect("ping encodes");
        assert_eq!(frame, "{\"type\":\"ping\",\"nonce\":7}\n");
        assert_eq!(frame.matches('\n').count(), 1);
    }

    #[test]
    fn a_multiline_string_inside_a_message_cannot_forge_a_frame_boundary() {
        let frame = FromWorker::Log {
            level: "warn".to_owned(),
            message: "line one\nline two".to_owned(),
        }
        .encode()
        .expect("log encodes");
        assert_eq!(frame.matches('\n').count(), 1, "frame was {frame:?}");
    }

    #[test]
    fn shutdown_is_a_bare_tag() {
        let frame = ToWorker::Shutdown.encode().expect("shutdown encodes");
        assert_eq!(frame, "{\"type\":\"shutdown\"}\n");
    }

    #[test]
    fn absent_data_and_error_are_omitted_not_null() {
        // The Python mirror omits them too; asserting it here is what keeps the golden fixture
        // expressible in one spelling.
        let frame = FromWorker::Result {
            id: 3,
            ok: true,
            data: Some(json!({"preview_path": "/tmp/a.jpg"})),
            error: None,
        }
        .encode()
        .expect("result encodes");
        assert!(!frame.contains("error"), "frame was {frame}");
    }

    #[test]
    fn a_result_that_is_ok_and_carries_an_error_is_rejected() {
        let frame =
            r#"{"type":"result","id":4,"ok":true,"error":{"code":"INTERNAL","message":"x"}}"#;
        let err = FromWorker::decode(frame).expect_err("contradictory result must not decode");
        assert!(
            matches!(err, ProtocolError::ContradictoryResult { id: 4, ok: true }),
            "{err}"
        );
    }

    #[test]
    fn a_failure_with_no_error_is_rejected() {
        let frame = r#"{"type":"result","id":5,"ok":false}"#;
        let err = FromWorker::decode(frame).expect_err("empty failure must not decode");
        assert!(
            matches!(err, ProtocolError::ContradictoryResult { id: 5, ok: false }),
            "{err}"
        );
    }

    #[test]
    fn progress_above_a_hundred_percent_is_rejected() {
        let frame = r#"{"type":"progress","id":6,"pct":200}"#;
        let err = FromWorker::decode(frame).expect_err("200% must not decode");
        assert!(
            matches!(err, ProtocolError::ProgressOutOfRange { id: 6, pct: 200 }),
            "{err}"
        );
    }

    #[test]
    fn a_hello_from_a_future_worker_still_yields_its_version() {
        // The whole point of the leniency documented at the top of this module: a v2 worker's
        // extra capability field must not turn a version mismatch into "malformed frame".
        let frame = r#"{"type":"hello","proto_version":2,
            "capabilities":{"gpu":true,"vram_mb":24576,"libs":{"cupy":"13.0"},
            "tensor_cores":true},"scheduler":"fifo"}"#
            .replace('\n', "");
        let hello = FromWorker::decode(&frame).expect("a future hello must still parse");
        match hello {
            FromWorker::Hello { proto_version, .. } => assert_eq!(proto_version, 2),
            other => panic!("expected hello, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_parsed() {
        let frame = format!(
            "{{\"type\":\"log\",\"level\":\"info\",\"message\":\"{}\"}}",
            "x".repeat(MAX_LINE_BYTES)
        );
        let err = FromWorker::decode(&frame).expect_err("an oversized frame must not decode");
        assert!(matches!(err, ProtocolError::TooLong { .. }), "{err}");
    }

    #[test]
    fn a_malformed_frame_names_only_its_first_line_worth_of_content() {
        let err = FromWorker::decode(&"z".repeat(4096)).expect_err("garbage must not decode");
        let text = err.to_string();
        assert!(text.len() < 400, "error message was {} chars", text.len());
    }

    #[test]
    fn binary_on_stdout_is_reported_as_such_and_not_as_bad_json() {
        let err = decode_from_worker_bytes(b"{\"type\":\"pong\",\"nonce\":\xff}")
            .expect_err("invalid UTF-8 must not decode");
        assert!(matches!(err, ProtocolError::NotUtf8 { .. }), "{err}");
    }

    #[test]
    fn every_to_worker_variant_round_trips() {
        let messages = [
            ToWorker::Hello {
                proto_version: PROTO_VERSION,
            },
            ToWorker::Job {
                id: 1,
                kind: JobKind::Preview,
                params: json!({"softening": 10.0}),
                paths: vec![PathBuf::from("/data/astro/sessions/s/frames/light_1.fits")],
            },
            ToWorker::Cancel { id: 1 },
            ToWorker::Ping { nonce: 42 },
            ToWorker::Shutdown,
        ];
        for message in messages {
            let frame = message.encode().expect("encodes");
            let back = ToWorker::decode(&frame).expect("decodes");
            assert_eq!(back, message);
        }
    }

    #[test]
    fn every_from_worker_variant_round_trips() {
        let messages = [
            FromWorker::Hello {
                proto_version: PROTO_VERSION,
                capabilities: WorkerCaps {
                    gpu: false,
                    vram_mb: None,
                    libs: BTreeMap::from([("numpy".to_owned(), "2.4.3".to_owned())]),
                },
            },
            FromWorker::Progress { id: 1, pct: 50 },
            FromWorker::Result {
                id: 1,
                ok: true,
                data: Some(json!({"preview_path": "/tmp/light_1.jpg"})),
                error: None,
            },
            FromWorker::Result {
                id: 2,
                ok: false,
                data: None,
                error: Some(WorkerError {
                    code: ErrorCode::NotFound,
                    message: "no such frame".to_owned(),
                }),
            },
            FromWorker::Pong { nonce: 42 },
            FromWorker::Log {
                level: "info".to_owned(),
                message: "started".to_owned(),
            },
        ];
        for message in messages {
            let frame = message.encode().expect("encodes");
            let back = FromWorker::decode(&frame).expect("decodes");
            assert_eq!(back, message);
        }
    }
}
