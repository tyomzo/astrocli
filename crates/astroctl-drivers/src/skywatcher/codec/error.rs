//! The two failure directions, kept apart on purpose.
//!
//! [`EncodeError`] is *our* fault: a value the protocol cannot express was offered to the codec.
//! It is caught before a byte leaves the process, and every one of them is a programming error
//! in a layer above.
//!
//! [`ProtocolError`] is the *mount's* — or the cable's: something arrived that the protocol does
//! not describe. SDD §5.2.4 makes a `ProtocolError` count as a failed exchange for the retry and
//! heartbeat logic, which is why it is a distinct type rather than a string: M3-T02 must be able
//! to tell "the mount said no" ([`MountError`], a well-formed refusal) from "that was not a
//! reply" (this) without matching on message text.
//!
//! Neither is a `DeviceError`. Mapping to `astroctl_core::error::DeviceError` is the *driver's*
//! job (M3-T04) because only the driver knows which HAL call was in flight; a codec that mapped
//! its own errors would have to guess. `ProtocolError` → `DeviceError::Protocol` and
//! `MountError` → `DeviceError::Rejected` are the intended edges, and they are documented here
//! rather than implemented here for that reason.

use super::frame::MAX_FRAME_LEN;

/// A value the Synta protocol cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// A register field is 24 bits wide; this value needs more.
    #[error("{value} does not fit in the protocol's 24-bit field (max {max})")]
    NotU24 {
        /// The offending value.
        value: u32,
        /// The largest value a 24-bit register holds, i.e. 16,777,215.
        max: u32,
    },
    /// A relative move whose magnitude overflows a 24-bit register.
    ///
    /// The magnitude, not the endpoint: `H` carries `|delta|` and the sign lives in the `G`
    /// direction bit, so a move is out of range exactly when its length is.
    #[error("move of {delta} counts has a magnitude the 24-bit increment register cannot hold")]
    MoveTooLong {
        /// The requested signed delta in counts.
        delta: i64,
    },
    /// `P` takes one decimal digit.
    #[error("guide rate {0} is outside the single-digit range the `P` opcode carries (0..=9)")]
    GuideRateOutOfRange(u32),
    /// The break point must lie between the start of the move and its target.
    ///
    /// A break point past the target would ask the mount to begin decelerating after it has
    /// already arrived, and one on the far side of the start asks it to decelerate before it
    /// sets off. Neither is a motion; both are almost certainly a sign error.
    #[error(
        "break point {brake} counts is not inside the {total}-count move it decelerates \
         (expected 1..={total})"
    )]
    BreakPointOutsideMove {
        /// Requested break-point magnitude, in counts from the start of the move.
        brake: u32,
        /// Magnitude of the move itself.
        total: u32,
    },
}

/// Something arrived from the mount that the protocol does not describe.
///
/// Every variant names the offending bytes or width, because the one thing a protocol error must
/// survive is being read in a field log at 3 a.m. by someone with no serial sniffer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// Nothing at all, or only a bare terminator.
    #[error("empty reply")]
    Empty,
    /// No `\r`. The mount terminates every reply; the absence of one means the read stopped early.
    #[error("reply {0:?} is not terminated by CR")]
    Unterminated(String),
    /// Bytes after the terminating `\r`.
    ///
    /// **This is the pipelining corruption, and it is deliberately not repaired here.** Writing
    /// two frames in one call provably interleaves the replies on real hardware
    /// (`=0000=000080`, `spikes/skywatcher-heq5/FINDINGS.md`), which is why SDD §5.2.4 forbids
    /// pipelining. A decoder that resynchronised on the second `=` would turn that corruption
    /// into a plausible-looking wrong answer — the single worst outcome available. It refuses
    /// instead, and M3-T02 flushes and retries.
    #[error("{extra} byte(s) follow the terminating CR — the reply stream is out of step")]
    TrailingBytes {
        /// How many bytes came after the first `\r`.
        extra: usize,
    },
    /// A reply longer than any frame the protocol defines.
    ///
    /// Bounded before anything is copied, so a wedged link that streams garbage cannot make the
    /// decoder allocate.
    #[error("reply is {len} bytes; no Synta frame exceeds {max}")]
    TooLong {
        /// Length of the offered buffer.
        len: usize,
        /// [`MAX_FRAME_LEN`].
        max: usize,
    },
    /// A byte outside ASCII. The whole protocol is printable ASCII plus `\r`.
    #[error("reply contains a non-ASCII byte {byte:#04x} at offset {offset}")]
    NonAscii {
        /// The offending byte.
        byte: u8,
        /// Where it was.
        offset: usize,
    },
    /// A reply that begins with neither `=` nor `!`.
    #[error("reply begins with {leader:?}, expected '=' (ok) or '!' (error)")]
    BadLeader {
        /// The first byte, as a character.
        leader: char,
    },
    /// `!` followed by something other than exactly one digit.
    #[error("error frame payload {0:?} is not a single decimal digit")]
    MalformedErrorFrame(String),
    /// The payload is the wrong length for the command that asked for it.
    ///
    /// The width is per opcode and is *not* uniform — `:g` answers two characters, `:f` three,
    /// the u24 registers six, and an action answers with none at all. This is the error that
    /// fires when a decoder is pointed at the wrong one.
    #[error("payload {payload:?} is {actual} characters, expected {expected} for opcode {opcode}")]
    PayloadWidth {
        /// The payload as received.
        payload: String,
        /// How many characters it has.
        actual: usize,
        /// How many the opcode's reply carries.
        expected: usize,
        /// The opcode whose reply this was meant to be.
        opcode: char,
    },
    /// A payload character that is not a hex digit.
    #[error("payload {payload:?} contains {ch:?} at offset {offset}, which is not a hex digit")]
    NotHex {
        /// The payload as received.
        payload: String,
        /// The offending character.
        ch: char,
        /// Where it was within the payload.
        offset: usize,
    },
    /// An axis digit that is neither `1` (RA) nor `2` (DEC).
    ///
    /// Only reachable when decoding a frame the codec did not build — the encode side cannot
    /// produce one, because [`astroctl_core::types::Axis`] has exactly two variants.
    #[error("{0:?} is not an axis digit (1 = RA, 2 = DEC)")]
    BadAxis(char),
    /// A motion-mode pair that is not one of the eight the protocol defines.
    #[error("{0:?} is not a motion mode: expected a mode digit 0..=3 and a direction digit 0..=1")]
    BadMotionMode(String),
}

impl ProtocolError {
    /// Build a [`ProtocolError::TooLong`] against the one true bound.
    pub(super) const fn too_long(len: usize) -> Self {
        Self::TooLong {
            len,
            max: MAX_FRAME_LEN,
        }
    }
}

/// A refusal the mount itself issued: `!` + one digit + `\r`.
///
/// **Three of these are captures, the rest are read from the vendor protocol.** `!0`, `!1` and
/// `!3` were provoked deliberately on the operator's HEQ5 (`:z1`, `:j`, and a bare `:`
/// respectively — `spikes/skywatcher-heq5/FINDINGS.md`) and their meanings are confirmed by what
/// was sent to produce them. The others come from the vendor wiki (source S1 of
/// `spikes/skywatcher-heq5/ENCODINGS.md`) and are marked `derived` in
/// `testdata/synta_vectors.txt`; M3-T05 upgrades any it manages to provoke.
///
/// [`MountError::MotorNotStopped`] is the one that matters operationally: the mount refuses a
/// mode change while an axis is moving, so it is the reply a driver gets for a goto issued
/// during a slew. It has no capture yet only because nothing has tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MountError {
    /// `!0` — the opcode is not one this firmware implements. **Verified** (`:z1`, `:y1`).
    #[error("unknown command")]
    UnknownCommand,
    /// `!1` — the frame was the wrong length for its opcode, e.g. a missing axis digit.
    /// **Verified** (`:j` with no axis).
    #[error("command length error: missing or invalid parameter")]
    InvalidParameter,
    /// `!2` — the axis is still moving and the command needs it stopped. **Derived** (S1).
    #[error("motor not stopped")]
    MotorNotStopped,
    /// `!3` — a character the parser could not use, e.g. a frame with no opcode at all.
    /// **Verified** (a bare `:`).
    #[error("invalid character in frame")]
    MalformedFrame,
    /// `!4` — the axis has not been initialised with `F`. **Derived** (S1).
    #[error("axis not initialised")]
    NotInitialised,
    /// `!5` — the motor driver is asleep. **Derived** (S1).
    #[error("driver sleeping")]
    DriverSleeping,
    /// `!7` — PEC training is in progress. **Derived** (S1).
    #[error("PEC training running")]
    PecTrainingRunning,
    /// `!8` — no PEC data to work with. **Derived** (S1).
    #[error("no valid PEC data")]
    NoValidPecData,
    /// A digit this firmware uses and we do not recognise.
    ///
    /// Present so that a future firmware cannot turn an error frame into a
    /// [`ProtocolError`] — a refusal we cannot name is still unambiguously a refusal, and
    /// reporting it as a framing fault would send M3-T02 down the retry path for a command the
    /// mount will refuse every time.
    #[error("mount error code {0}")]
    Unrecognised(u8),
}

impl MountError {
    /// Decode the single digit of an `!` frame. Total: every digit maps to something.
    #[must_use]
    pub const fn from_digit(digit: u8) -> Self {
        match digit {
            0 => Self::UnknownCommand,
            1 => Self::InvalidParameter,
            2 => Self::MotorNotStopped,
            3 => Self::MalformedFrame,
            4 => Self::NotInitialised,
            5 => Self::DriverSleeping,
            7 => Self::PecTrainingRunning,
            8 => Self::NoValidPecData,
            other => Self::Unrecognised(other),
        }
    }

    /// The digit the mount would send for this error — the inverse of [`Self::from_digit`],
    /// and what lets a mock port in M3-T02 script a refusal without hard-coding bytes.
    #[must_use]
    pub const fn digit(self) -> u8 {
        match self {
            Self::UnknownCommand => 0,
            Self::InvalidParameter => 1,
            Self::MotorNotStopped => 2,
            Self::MalformedFrame => 3,
            Self::NotInitialised => 4,
            Self::DriverSleeping => 5,
            Self::PecTrainingRunning => 7,
            Self::NoValidPecData => 8,
            Self::Unrecognised(d) => d,
        }
    }

    /// Whether a real HEQ5 has been observed producing this code.
    ///
    /// The provenance is data rather than a comment because M3-T05 reports against it: a
    /// bring-up that provokes `!2` flips one of these, and the vector file records it.
    #[must_use]
    pub const fn is_verified(self) -> bool {
        matches!(
            self,
            Self::UnknownCommand | Self::InvalidParameter | Self::MalformedFrame
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_digit_round_trips_through_the_error_table() {
        for d in 0..=u8::MAX {
            let e = MountError::from_digit(d);
            assert_eq!(
                e.digit(),
                d,
                "code {d} did not survive the round trip as {e:?}"
            );
        }
    }

    #[test]
    fn the_three_captured_codes_carry_the_meanings_the_captures_established() {
        // `:z1` → `!0`, `:j` → `!1`, `:` → `!3`. The meanings are not guesses: each is what the
        // frame that provoked it was wrong about.
        assert_eq!(MountError::from_digit(0), MountError::UnknownCommand);
        assert_eq!(MountError::from_digit(1), MountError::InvalidParameter);
        assert_eq!(MountError::from_digit(3), MountError::MalformedFrame);
        for e in [
            MountError::UnknownCommand,
            MountError::InvalidParameter,
            MountError::MalformedFrame,
        ] {
            assert!(e.is_verified(), "{e:?} has a hardware capture");
        }
    }

    #[test]
    fn codes_without_a_capture_say_so() {
        for e in [
            MountError::MotorNotStopped,
            MountError::NotInitialised,
            MountError::DriverSleeping,
            MountError::PecTrainingRunning,
            MountError::NoValidPecData,
            MountError::Unrecognised(6),
        ] {
            assert!(
                !e.is_verified(),
                "{e:?} has no hardware capture and must not claim one"
            );
        }
    }

    #[test]
    fn an_unknown_code_is_still_a_refusal_not_a_framing_fault() {
        // 6 and 9 are unassigned in the vendor list. A driver must still stop retrying.
        assert_eq!(MountError::from_digit(6), MountError::Unrecognised(6));
        assert_eq!(MountError::from_digit(9), MountError::Unrecognised(9));
    }
}
