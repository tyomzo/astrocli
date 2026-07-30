//! Frame identity — `light_00042` and the sidecar name derived from it.
//!
//! The spelling is fixed by SDD §5.5 (`frames/light_<id>.cr3`, `control/quality_<id>.json`) and
//! asserted from both sides by `testdata/session-layout.txt`. It is a wire format in every sense
//! that matters: the id travels in `frame.saved`, in the transfer journal, and in the stack node's
//! ingest — so it is parsed and printed in one place rather than formatted at each call site.

use std::fmt;
use std::str::FromStr;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

/// Digits the sequence number is padded to.
///
/// Five, because `light_00042` is what SDD §5.5 writes. A session that exceeded 99999 frames would
/// widen to six and stop sorting lexically — at one frame per two minutes that is 138 nights of
/// continuous exposure, so the alternative (a wider field forever) costs readability for a case
/// that cannot occur.
const SEQUENCE_DIGITS: usize = 5;

/// What a frame is for. The prefix of every frame id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// A frame of the target.
    Light,
    /// Sensor noise at the same exposure and temperature.
    Dark,
    /// The optical train's response to uniform illumination.
    Flat,
    /// The sensor's read-noise floor.
    Bias,
}

impl FrameKind {
    /// The prefix as it appears in a frame id.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Flat => "flat",
            Self::Bias => "bias",
        }
    }
}

impl FromStr for FrameKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            "flat" => Ok(Self::Flat),
            "bias" => Ok(Self::Bias),
            _ => Err(()),
        }
    }
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A frame's identity within one session: a kind and the session's sequence number.
///
/// **The sequence is per session, not per kind.** SDD §5.5 names the sidecar `quality_<id>.json`
/// with the kind prefix dropped, so `light_00042` and `dark_00042` would collide on one
/// `quality_00042.json` — two frames, one metadata file, and the second write silently describing
/// the first frame. A single counter per session makes that unrepresentable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId {
    kind: FrameKind,
    sequence: u64,
}

impl FrameId {
    /// Build an id. Only the frame store may sensibly call this — a sequence number that did not
    /// come from [`crate::Session::reserve_frame_id`] has no persistence behind it (REL-04).
    #[must_use]
    pub const fn new(kind: FrameKind, sequence: u64) -> Self {
        Self { kind, sequence }
    }

    /// What the frame is for.
    #[must_use]
    pub const fn kind(self) -> FrameKind {
        self.kind
    }

    /// The session-wide sequence number behind this id.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// The file name of the frame itself, e.g. `light_00042.cr3`.
    #[must_use]
    pub fn file_name(self, ext: &str) -> String {
        format!("{self}.{ext}")
    }

    /// The sidecar's file name, e.g. `quality_00042.json`.
    ///
    /// The kind prefix is dropped because SDD §5.5 writes it that way; see the type's docs for what
    /// that costs and why one counter per session pays it.
    #[must_use]
    pub fn quality_file_name(self) -> String {
        format!(
            "quality_{sequence:0width$}.json",
            sequence = self.sequence,
            width = SEQUENCE_DIGITS
        )
    }

    /// Parse `light_00042` — the inverse of [`fmt::Display`], used when reading a session
    /// directory back (a listing, or a restart).
    ///
    /// Rejects anything it does not fully understand rather than guessing: an unparseable name in
    /// `frames/` is a file the store did not write, and treating it as a frame would put a
    /// stranger's file into `/api/session/current`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (kind, sequence) = text.split_once('_')?;
        let kind = kind.parse().ok()?;
        // Rejects a leading `+`, spaces, and anything else `u64::from_str` would otherwise wave
        // through into an id that does not round-trip.
        if sequence.is_empty() || !sequence.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self::new(kind, sequence.parse().ok()?))
    }
}

impl fmt::Display for FrameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{kind}_{sequence:0width$}",
            kind = self.kind,
            sequence = self.sequence,
            width = SEQUENCE_DIGITS
        )
    }
}

/// Serialized as the string `light_00042`, never as a struct: this is what the id looks like
/// everywhere else in the system (SDD §4.3's `frame.saved`, §5.11's ingest), and a JSON object
/// would make the store the one place that spells it differently.
impl Serialize for FrameId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for FrameId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| D::Error::custom(format!("not a frame id: {raw}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_printed_the_way_sdd_5_5_writes_it() {
        assert_eq!(
            FrameId::new(FrameKind::Light, 42).to_string(),
            "light_00042"
        );
        assert_eq!(FrameId::new(FrameKind::Dark, 7).to_string(), "dark_00007");
        assert_eq!(
            FrameId::new(FrameKind::Light, 42).file_name("cr3"),
            "light_00042.cr3"
        );
    }

    /// The sidecar drops the kind prefix — the naming that forces one counter per session.
    #[test]
    fn the_sidecar_name_matches_the_mirrors_and_drops_the_kind() {
        assert_eq!(
            FrameId::new(FrameKind::Light, 42).quality_file_name(),
            "quality_00042.json"
        );
        assert_eq!(
            FrameId::new(FrameKind::Dark, 7).quality_file_name(),
            "quality_00007.json"
        );
    }

    #[test]
    fn parsing_round_trips_and_refuses_what_it_did_not_write() {
        for id in [
            FrameId::new(FrameKind::Light, 1),
            FrameId::new(FrameKind::Bias, 99_999),
            FrameId::new(FrameKind::Flat, 123_456),
        ] {
            assert_eq!(FrameId::parse(&id.to_string()), Some(id));
        }

        for bad in [
            "light",
            "light_",
            "light_abc",
            "light_-1",
            "light_ 1",
            "preview_00001",
            "",
            "_00001",
        ] {
            assert!(FrameId::parse(bad).is_none(), "{bad} parsed");
        }
    }

    #[test]
    fn an_id_travels_as_a_string() {
        let id = FrameId::new(FrameKind::Light, 42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"light_00042\"");
        assert_eq!(serde_json::from_str::<FrameId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<FrameId>("\"nope\"").is_err());
    }
}
