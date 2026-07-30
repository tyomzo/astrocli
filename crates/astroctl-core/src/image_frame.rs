//! The binary image-frame envelope carried by `/ws/liveview` and `/ws/preview` — SDD §5.8.3.
//!
//! ```text
//! 0      4        5      6        8              8+meta_len
//! +------+--------+------+--------+--------------+---------------------------+
//! |"ACLV"| version| kind | meta_len (u16 BE)     | meta JSON | JPEG bytes    |
//! +------+--------+------+--------+--------------+---------------------------+
//! ```
//!
//! §5.8.3 fixes this shape: `kind` is `0` for live view and `1` for a preview, and the metadata
//! carries `ts` always and `frame_id` on a preview.
//!
//! # Why this lives in `astroctl-core` rather than in the node that first needed it
//!
//! M1-T09 wrote the encoder inside `astroctl-field`, which was right while there was one producer.
//! M1-T14 gives it a second: the stacking server's `/ws/preview` (SDD §5.11.1) emits the identical
//! envelope, because the PWA decodes both sockets with one function and a client that had to know
//! which node it was talking to before it could read a frame would defeat the point of the shared
//! shape.
//!
//! Two producers of one wire format must not be two copies of it. The usual reason this workspace
//! duplicates code between the binaries — ADD §5.6 rule 5 forbids them depending on each other,
//! and axum may not enter `astroctl-core` (SDD §4.2) — does not apply here: this module is bytes
//! in, bytes out, with no HTTP type anywhere in it. So it belongs in the crate both binaries
//! already depend on, and the alternative is two encoders drifting into a decoder that half-works.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Marks a frame as this protocol's rather than a bare JPEG. A client reading `FF D8` here has
/// found a node older than the envelope and should say so, not render half a header as pixels.
pub const MAGIC: &[u8; 4] = b"ACLV";

/// Envelope version. Bumped only for a change a v1 client could not skip.
pub const PROTOCOL_VERSION: u8 = 1;

/// Fixed header length: magic(4) + version(1) + kind(1) + meta length(2).
pub const HEADER_LEN: usize = 8;

/// Which of the two image sources a frame came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// The camera's live-view stream (CAM-05). Superseded by the next one, seconds old at most.
    Live,
    /// A rendered preview — the field node's capture preview (CAM-06, IPP-04) or the stacking
    /// server's preview of an ingested frame (SDD §5.12.4). Worth keeping on screen until the
    /// next one, which is why it is told apart from a live frame at all.
    Preview,
}

impl FrameKind {
    /// The `kind` byte of SDD §5.8.3.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Live => 0,
            Self::Preview => 1,
        }
    }
}

/// What travels with the JPEG.
#[derive(Debug, Serialize)]
struct FrameMeta<'a> {
    /// RFC 3339 with milliseconds — SDD §2's format, the same one every event carries, so a
    /// client can compare a frame's age against its telemetry age without a second parser.
    ts: String,
    /// The frame this previews. Absent on a live-view frame, which is of nothing in particular.
    #[serde(skip_serializing_if = "Option::is_none")]
    frame_id: Option<&'a str>,
}

/// Encode one binary WebSocket payload.
///
/// Encoded **once, when published**, not once per client: every subscriber receives the identical
/// bytes, and re-serializing a 200 KB frame per connected phone is work a node does not have
/// (SDD §7's runtime budget) for an answer that cannot differ.
#[must_use]
pub fn encode(
    kind: FrameKind,
    jpeg: &[u8],
    ts: DateTime<Utc>,
    frame_id: Option<&str>,
) -> Arc<[u8]> {
    let meta = FrameMeta {
        ts: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        frame_id,
    };
    // Unreachable for a struct of a `String` and an `Option<&str>`; falling back to an empty
    // object keeps a frame flowing rather than dropping an image over its label.
    let meta = serde_json::to_vec(&meta).unwrap_or_else(|_| b"{}".to_vec());
    let meta_len = u16::try_from(meta.len()).unwrap_or(u16::MAX);

    let mut out = Vec::with_capacity(HEADER_LEN + meta.len() + jpeg.len());
    out.extend_from_slice(MAGIC);
    out.push(PROTOCOL_VERSION);
    out.push(kind.as_byte());
    out.extend_from_slice(&meta_len.to_be_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(jpeg);
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> DateTime<Utc> {
        "2026-07-29T21:04:05.123Z"
            .parse()
            .expect("a fixed timestamp parses")
    }

    /// The byte layout of SDD §5.8.3, asserted position by position. This is the contract the
    /// PWA's `decodeLiveFrame` is written against, so it is checked as bytes rather than by
    /// round-tripping through a decoder that could share a mistake with the encoder.
    #[test]
    fn the_envelope_has_the_layout_5_8_3_specifies() {
        let frame = encode(FrameKind::Preview, b"\xff\xd8jpeg", ts(), Some("light_00042"));

        assert_eq!(&frame[0..4], MAGIC);
        assert_eq!(frame[4], PROTOCOL_VERSION);
        assert_eq!(frame[5], 1, "preview is kind 1");

        let meta_len = usize::from(u16::from_be_bytes([frame[6], frame[7]]));
        let meta: serde_json::Value =
            serde_json::from_slice(&frame[HEADER_LEN..HEADER_LEN + meta_len]).expect("meta is json");
        assert_eq!(meta["ts"], "2026-07-29T21:04:05.123Z");
        assert_eq!(meta["frame_id"], "light_00042");

        assert_eq!(&frame[HEADER_LEN + meta_len..], b"\xff\xd8jpeg");
    }

    /// A live frame is of nothing in particular, so it carries no `frame_id` *key* — not a null.
    /// A decoder that reads `frame_id` as "the frame being previewed" must not be handed one.
    #[test]
    fn a_live_frame_omits_the_frame_id_rather_than_nulling_it() {
        let frame = encode(FrameKind::Live, b"jpeg", ts(), None);
        assert_eq!(frame[5], 0, "live is kind 0");

        let meta_len = usize::from(u16::from_be_bytes([frame[6], frame[7]]));
        let meta: serde_json::Value =
            serde_json::from_slice(&frame[HEADER_LEN..HEADER_LEN + meta_len]).expect("meta is json");
        assert!(
            meta.get("frame_id").is_none(),
            "the key must be absent, not null: {meta}"
        );
    }

    /// The two nodes must produce byte-identical envelopes for the same preview — that is the
    /// whole reason this module is in `astroctl-core` rather than copied into each binary.
    #[test]
    fn one_encoder_means_the_two_nodes_cannot_disagree() {
        let field = encode(FrameKind::Preview, b"jpeg", ts(), Some("light_00007"));
        let stack = encode(FrameKind::Preview, b"jpeg", ts(), Some("light_00007"));
        assert_eq!(field, stack);
    }
}
