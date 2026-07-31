//! Framing: `:` + opcode + axis + payload + `\r` out, `=payload\r` or `!digit\r` back.
//!
//! # The axis digit is validated here because the mount does not validate it at all
//!
//! `:j9` returns `=000080` — a well-formed position for an axis that does not exist
//! (`spikes/skywatcher-heq5/FINDINGS.md`). The device accepts any digit and answers with
//! plausible data, so a corrupted axis digit produces a *wrong answer*, never an error. The only
//! defence is upstream: [`axis_digit`] is the sole way to put an axis on the wire and it takes
//! an [`Axis`], which has two variants and no way to spell a third.
//!
//! # Nothing here resynchronises
//!
//! The mount resynchronises on `:` after junk; the *reply* stream does not. Two frames written
//! together came back as `=0000=000080` on real hardware — an interleaving, not a queue. SDD
//! §5.2.4 forbids pipelining for exactly this reason, and [`decode_reply`] refuses anything
//! after the terminating `\r` rather than skipping to the next `=`. Repairing it here would
//! convert a detectable fault into an undetectable wrong number, on a link that carries goto
//! targets.

use astroctl_core::types::Axis;

use super::error::{MountError, ProtocolError};

/// The longest frame the protocol defines: `:` + opcode + axis + six payload characters + `\r`.
pub const MAX_FRAME_LEN: usize = 10;

/// Frame start.
const START: u8 = b':';
/// Frame terminator, both directions.
pub const TERMINATOR: u8 = b'\r';
/// Success leader.
const OK_LEADER: u8 = b'=';
/// Refusal leader.
const ERR_LEADER: u8 = b'!';

/// A request frame, complete and ready for the port.
///
/// Fixed-capacity and `Copy`: a codec that allocated per command would be allocating on the
/// 1 Hz position poll and on every one of the eight frames a goto costs, for a value that is
/// never longer than ten bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    buf: [u8; MAX_FRAME_LEN],
    len: u8,
}

impl Frame {
    /// Build `:<opcode><axis><payload>\r`.
    ///
    /// # Panics
    /// If `payload` is longer than six characters — unreachable from outside the crate, because
    /// every payload comes from an encoder in this module whose width is fixed by the protocol.
    /// The assertion is the guard on that claim.
    pub(super) fn build(opcode: u8, axis: Axis, payload: &[u8]) -> Self {
        assert!(
            payload.len() <= MAX_FRAME_LEN - 3,
            "payload of {} bytes exceeds the protocol's widest field",
            payload.len()
        );
        let mut buf = [0u8; MAX_FRAME_LEN];
        buf[0] = START;
        buf[1] = opcode;
        buf[2] = axis_digit(axis);
        buf[3..3 + payload.len()].copy_from_slice(payload);
        buf[3 + payload.len()] = TERMINATOR;
        // 3 leading bytes + payload + terminator; the assertion above bounds this at MAX_FRAME_LEN.
        #[allow(clippy::cast_possible_truncation)]
        let len = (4 + payload.len()) as u8;
        Self { buf, len }
    }

    /// The bytes to write, terminator included.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// The frame as text. ASCII by construction, so this cannot fail.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Every byte came from an opcode literal, `axis_digit`, or a hex encoder.
        core::str::from_utf8(self.as_bytes()).unwrap_or("<non-ascii frame>")
    }

    /// The opcode byte.
    ///
    /// M3-T02's raw-stream write gate needs this: the spike harnesses refuse to emit an
    /// uppercase opcode unless the experiment allow-listed it, and SDD §5.2.2 makes opcode case
    /// the safety boundary — lower case inquires, upper case moves things.
    #[must_use]
    pub const fn opcode(&self) -> u8 {
        self.buf[1]
    }

    /// Whether this frame can cause motion, i.e. whether its opcode is upper case.
    #[must_use]
    pub const fn is_action(&self) -> bool {
        self.opcode().is_ascii_uppercase()
    }
}

impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `\r` rendered as an escape, so a failing assertion is readable.
        write!(f, "Frame({:?})", self.as_str())
    }
}

impl core::fmt::Display for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The frame as a human reads it in a log or a vector file: no trailing CR.
        f.write_str(self.as_str().trim_end_matches('\r'))
    }
}

/// The axis digit: RA is 1, DEC is 2.
///
/// SDD §5.2.2 also mentions `3` for "both axes where valid". This codec does not emit it, and
/// the omission is deliberate: no Phase 1 command needs it, no capture has ever exercised it,
/// and the mount answers an axis digit it does not implement with plausible data rather than an
/// error. An unverified broadcast digit on a link that cannot report its own rejection is not a
/// convenience worth having. M3-T05 can add it once something has watched both axes respond.
#[must_use]
pub const fn axis_digit(axis: Axis) -> u8 {
    match axis {
        Axis::Ra => b'1',
        Axis::Dec => b'2',
    }
}

/// Read an axis digit back.
///
/// Only reachable for frames the codec did not build — a decoded request in a test, or a trace
/// being replayed into the vector file.
///
/// # Errors
/// [`ProtocolError::BadAxis`] for anything but `1` or `2`, `9` very much included.
pub const fn axis_from_digit(digit: u8) -> Result<Axis, ProtocolError> {
    match digit {
        b'1' => Ok(Axis::Ra),
        b'2' => Ok(Axis::Dec),
        other => Err(ProtocolError::BadAxis(other as char)),
    }
}

/// What came back: either a payload to decode, or a refusal that needs no decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply<'a> {
    /// `=…\r`. The payload, terminator and leader stripped; may be empty, which is what an
    /// action opcode answers with (`:F1` → a bare `=`, measured).
    Ok(&'a str),
    /// `!<digit>\r`. The mount understood the frame and refused it.
    Err(MountError),
}

impl Reply<'_> {
    /// The payload, or the refusal as an error.
    ///
    /// # Errors
    /// The [`MountError`] the mount sent.
    pub const fn payload(&self) -> Result<&str, MountError> {
        match self {
            Self::Ok(p) => Ok(*p),
            Self::Err(e) => Err(*e),
        }
    }
}

/// Split a reply into leader and payload.
///
/// Takes the bytes as read from the port, terminator included. M3-T02 reads until `\r` and hands
/// the result straight here; anything it read past the terminator is a fault this reports rather
/// than hides.
///
/// # Errors
/// Every way a buffer can fail to be a Synta reply — see [`ProtocolError`]. Never panics, for
/// any input at all; that is a gated property of T-COD-1 rather than a claim.
pub fn decode_reply(bytes: &[u8]) -> Result<Reply<'_>, ProtocolError> {
    if bytes.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::too_long(bytes.len()));
    }
    if bytes.is_empty() {
        return Err(ProtocolError::Empty);
    }
    // ASCII first, then UTF-8. The first check is the real one — `é` is valid UTF-8 and is not a
    // Synta byte — and the second exists so the `&str` below needs no `unwrap`, which is why it
    // reports through the same variant rather than a second one nothing can reach.
    if let Some((offset, &byte)) = bytes.iter().enumerate().find(|(_, b)| !b.is_ascii()) {
        return Err(ProtocolError::NonAscii { byte, offset });
    }
    let text = core::str::from_utf8(bytes).map_err(|e| {
        let offset = e.valid_up_to();
        ProtocolError::NonAscii {
            byte: bytes[offset],
            offset,
        }
    })?;

    let Some(end) = bytes.iter().position(|&b| b == TERMINATOR) else {
        return Err(ProtocolError::Unterminated(text.to_owned()));
    };
    if end + 1 != bytes.len() {
        return Err(ProtocolError::TrailingBytes {
            extra: bytes.len() - end - 1,
        });
    }

    let body = &text[..end];
    let mut chars = body.bytes();
    match chars.next() {
        None => Err(ProtocolError::Empty),
        Some(OK_LEADER) => Ok(Reply::Ok(&body[1..])),
        Some(ERR_LEADER) => {
            let digits = &body[1..];
            match digits.as_bytes() {
                [d] if d.is_ascii_digit() => Ok(Reply::Err(MountError::from_digit(d - b'0'))),
                _ => Err(ProtocolError::MalformedErrorFrame(digits.to_owned())),
            }
        }
        Some(other) => Err(ProtocolError::BadLeader {
            leader: char::from(other),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_frame_is_colon_opcode_axis_payload_cr() {
        let f = Frame::build(b'j', Axis::Ra, b"");
        assert_eq!(f.as_bytes(), b":j1\r");
        assert_eq!(f.to_string(), ":j1");
        let f = Frame::build(b'H', Axis::Dec, b"393000");
        assert_eq!(f.as_bytes(), b":H2393000\r");
        assert_eq!(f.as_bytes().len(), MAX_FRAME_LEN);
    }

    #[test]
    fn opcode_case_is_readable_from_the_frame() {
        // SDD §5.2.2: lower case inquires, upper case acts. M3-T02's write gate is built on it.
        assert!(!Frame::build(b'j', Axis::Ra, b"").is_action());
        assert!(!Frame::build(b'f', Axis::Ra, b"").is_action());
        assert!(Frame::build(b'J', Axis::Ra, b"").is_action());
        assert!(Frame::build(b'L', Axis::Ra, b"").is_action());
        assert_eq!(Frame::build(b'G', Axis::Ra, b"20").opcode(), b'G');
    }

    #[test]
    fn the_axis_digit_has_exactly_two_spellings_and_nine_is_not_one() {
        assert_eq!(axis_digit(Axis::Ra), b'1');
        assert_eq!(axis_digit(Axis::Dec), b'2');
        assert_eq!(axis_from_digit(b'1'), Ok(Axis::Ra));
        assert_eq!(axis_from_digit(b'2'), Ok(Axis::Dec));
        // The mount answers `:j9` with `=000080`; this codec never asks it.
        for bad in *b"039a " {
            assert_eq!(
                axis_from_digit(bad),
                Err(ProtocolError::BadAxis(bad as char)),
                "axis digit {:?} must be refused before transmission",
                bad as char
            );
        }
    }

    #[test]
    fn the_captured_replies_split_into_leader_and_payload() {
        assert_eq!(decode_reply(b"=000080\r"), Ok(Reply::Ok("000080")));
        assert_eq!(decode_reply(b"=100\r"), Ok(Reply::Ok("100")));
        assert_eq!(decode_reply(b"=10\r"), Ok(Reply::Ok("10")));
        // An action answers with a bare `=`: measured on `:F1` and `:F2`.
        assert_eq!(decode_reply(b"=\r"), Ok(Reply::Ok("")));
    }

    #[test]
    fn the_three_captured_error_frames_decode_to_their_causes() {
        assert_eq!(
            decode_reply(b"!0\r"),
            Ok(Reply::Err(MountError::UnknownCommand))
        );
        assert_eq!(
            decode_reply(b"!1\r"),
            Ok(Reply::Err(MountError::InvalidParameter))
        );
        assert_eq!(
            decode_reply(b"!3\r"),
            Ok(Reply::Err(MountError::MalformedFrame))
        );
    }

    #[test]
    fn the_pipelining_corruption_is_reported_not_repaired() {
        // Real hardware, two frames in one write: `=0000=000080`. A decoder that resynchronised
        // on the second `=` would return 8,388,608 — a plausible, wrong, undetectable answer.
        let corrupt = b"=0000=00\r";
        assert!(matches!(decode_reply(corrupt), Ok(Reply::Ok("0000=00"))));
        // ...and the shape that actually reaches the decoder — a reply with a second frame's
        // bytes trailing it — is refused outright.
        assert_eq!(
            decode_reply(b"=0000\r=00"),
            Err(ProtocolError::TrailingBytes { extra: 3 })
        );
    }

    #[test]
    fn malformed_replies_are_refused_with_a_reason_each() {
        assert_eq!(decode_reply(b""), Err(ProtocolError::Empty));
        assert_eq!(decode_reply(b"\r"), Err(ProtocolError::Empty));
        assert_eq!(
            decode_reply(b"=000080"),
            Err(ProtocolError::Unterminated("=000080".to_owned()))
        );
        assert_eq!(
            decode_reply(b"?1\r"),
            Err(ProtocolError::BadLeader { leader: '?' })
        );
        assert_eq!(
            decode_reply(b"!\r"),
            Err(ProtocolError::MalformedErrorFrame(String::new()))
        );
        assert_eq!(
            decode_reply(b"!12\r"),
            Err(ProtocolError::MalformedErrorFrame("12".to_owned()))
        );
        assert_eq!(
            decode_reply(b"!x\r"),
            Err(ProtocolError::MalformedErrorFrame("x".to_owned()))
        );
        assert_eq!(
            decode_reply(b"=00000000000\r"),
            Err(ProtocolError::too_long(13))
        );
        assert_eq!(
            decode_reply(&[b'=', 0x80, b'\r']),
            Err(ProtocolError::NonAscii {
                byte: 0x80,
                offset: 1
            })
        );
    }

    #[test]
    fn a_truncated_read_is_unterminated_rather_than_a_short_payload() {
        // The measured behaviour of a frame with no `\r` is silence and a timeout, so this is
        // what M3-T02 sees if it decodes whatever arrived before giving up. It must not look
        // like a valid two-character reply.
        assert!(matches!(
            decode_reply(b"=10"),
            Err(ProtocolError::Unterminated(_))
        ));
    }
}
