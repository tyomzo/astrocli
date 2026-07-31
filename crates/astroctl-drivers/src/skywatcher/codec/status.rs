//! `f` GetAxisStatus — three characters, five bits, and the field slew-complete detection needs.
//!
//! # The bit table
//!
//! | Nibble | Bit | Meaning |
//! |--------|-----|---------|
//! | `n1` | `0x01` | 1 = SLEW, 0 = GOTO |
//! | `n1` | `0x02` | 1 = backward, 0 = forward |
//! | `n1` | `0x04` | 1 = high speed, 0 = low |
//! | `n2` | `0x01` | 1 = axis running |
//! | `n3` | `0x01` | 1 = axis initialised (`F` has been issued) |
//!
//! **Validated in two distinct states on the operator's own mount.** At power-on both axes
//! answered `=100`: not running, not initialised. After `:F1`/`:F2` both answered `=101` — the
//! initialised bit and nothing else (`spikes/skywatcher-heq5/FINDINGS.md`, E1). A table that
//! predicted the wrong bit would have differed on one of those two.
//!
//! # Nibbles, not ASCII arithmetic
//!
//! Both `ENCODINGS.md` and SDD §5.2.2 describe the bit tests as applying "directly to the ASCII
//! characters", which works because ASCII `0`–`7` carry their value in the low three bits. It
//! stops working at `8`. `'A'` is `0x41`, so `'A' & 0x01` is 1 and the ASCII shortcut would
//! report SLEW for a nibble of `0b1010`, which is GOTO. This decoder parses the hex digit first
//! and then tests the bits, which is identical for every value the sources describe and correct
//! for the ones they do not.
//!
//! # Undocumented bits are kept, not rejected
//!
//! Only five bits have meanings we can defend. The rest are preserved in [`AxisStatus::raw`] and
//! flagged by [`AxisStatus::has_undocumented_bits`], rather than being decoded (which would be
//! inventing semantics) or refused (which would let an unknown-but-harmless firmware bit break
//! slew-complete detection, and with it every goto). The vendor wiki hints at more — a "blocked"
//! bit in `n2`, a level-switch bit in `n3` — but nothing has produced either, so this codec
//! declines to name them and hands M3-T05 the raw nibbles to name them from.

use super::error::ProtocolError;
use super::motion::{MotionDirection, MotionKind, SpeedClass};

/// Characters in an `f` reply.
pub const STATUS_CHARS: usize = 3;

/// `n1` bit 0: motion kind.
const N1_SLEW: u8 = 0x01;
/// `n1` bit 1: direction.
const N1_BACKWARD: u8 = 0x02;
/// `n1` bit 2: speed class.
const N1_HIGH_SPEED: u8 = 0x04;
/// `n2` bit 0: the axis is moving.
const N2_RUNNING: u8 = 0x01;
/// `n3` bit 0: `F` has been issued on this axis.
const N3_INITIALISED: u8 = 0x01;

/// The bits this decoder claims to understand, per nibble.
const DOCUMENTED: [u8; STATUS_CHARS] = [
    N1_SLEW | N1_BACKWARD | N1_HIGH_SPEED,
    N2_RUNNING,
    N3_INITIALISED,
];

/// What an axis reports about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AxisStatus {
    /// The motion the axis is configured for. Meaningful whether or not it is running — the
    /// mode persists after the motion ends.
    pub kind: MotionKind,
    /// Configured direction.
    pub direction: MotionDirection,
    /// Configured speed class.
    pub speed: SpeedClass,
    /// Whether the axis is moving now.
    ///
    /// **This is the slew-complete signal** (SDD §5.2.3): a goto is finished when both axes
    /// report `false` and their counters are within tolerance of the target.
    pub running: bool,
    /// Whether `F` has been issued on this axis since power-on.
    pub initialised: bool,
    /// The three nibbles as received.
    ///
    /// Kept because it is the measurement; everything above is an interpretation of it.
    pub raw: [u8; STATUS_CHARS],
}

impl AxisStatus {
    /// Decode an `f` payload.
    ///
    /// # Errors
    /// [`ProtocolError::PayloadWidth`] unless it is exactly three characters, and
    /// [`ProtocolError::NotHex`] for a character that is not a hex digit.
    pub fn decode(payload: &str) -> Result<Self, ProtocolError> {
        let bytes = payload.as_bytes();
        if bytes.len() != STATUS_CHARS {
            return Err(ProtocolError::PayloadWidth {
                payload: payload.to_owned(),
                actual: payload.chars().count(),
                expected: STATUS_CHARS,
                opcode: 'f',
            });
        }
        let mut raw = [0u8; STATUS_CHARS];
        for (i, &b) in bytes.iter().enumerate() {
            raw[i] = char::from(b).to_digit(16).map(|d| d as u8).ok_or_else(|| {
                ProtocolError::NotHex {
                    payload: payload.to_owned(),
                    ch: char::from(b),
                    offset: i,
                }
            })?;
        }
        Ok(Self::from_nibbles(raw))
    }

    /// Interpret three already-parsed nibbles.
    #[must_use]
    pub const fn from_nibbles(raw: [u8; STATUS_CHARS]) -> Self {
        Self {
            kind: if raw[0] & N1_SLEW != 0 {
                MotionKind::Slew
            } else {
                MotionKind::Goto
            },
            direction: if raw[0] & N1_BACKWARD != 0 {
                MotionDirection::Backward
            } else {
                MotionDirection::Forward
            },
            speed: if raw[0] & N1_HIGH_SPEED != 0 {
                SpeedClass::High
            } else {
                SpeedClass::Low
            },
            running: raw[1] & N2_RUNNING != 0,
            initialised: raw[2] & N3_INITIALISED != 0,
            raw,
        }
    }

    /// Re-encode the five documented bits, discarding anything [`Self::raw`] carried beyond them.
    ///
    /// For M3-T02's mock port and for the golden vectors: a fixture that has to spell `=101` by
    /// hand is a fixture that can spell it wrong.
    #[must_use]
    pub fn encode(self) -> [u8; STATUS_CHARS] {
        let mut n1 = 0u8;
        if matches!(self.kind, MotionKind::Slew) {
            n1 |= N1_SLEW;
        }
        if matches!(self.direction, MotionDirection::Backward) {
            n1 |= N1_BACKWARD;
        }
        if matches!(self.speed, SpeedClass::High) {
            n1 |= N1_HIGH_SPEED;
        }
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let nibbles = [n1, u8::from(self.running), u8::from(self.initialised)];
        nibbles.map(|n| HEX[(n & 0x0F) as usize])
    }

    /// Whether the mount set a bit this decoder does not interpret.
    ///
    /// Never true on any capture taken so far. If it becomes true on real hardware, the raw
    /// nibbles are in [`Self::raw`] and the bit table above is what needs extending — a driver
    /// should log it once rather than fail, because none of the five bits it does use are
    /// affected.
    #[must_use]
    pub fn has_undocumented_bits(&self) -> bool {
        self.raw
            .iter()
            .zip(DOCUMENTED)
            .any(|(&got, known)| got & !known != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_captured_states_decode_as_the_mount_behaved() {
        // Power-on, both axes: `=100`. Not running, not initialised.
        let at_rest = AxisStatus::decode("100").expect("three hex digits");
        assert_eq!(at_rest.kind, MotionKind::Slew);
        assert_eq!(at_rest.direction, MotionDirection::Forward);
        assert_eq!(at_rest.speed, SpeedClass::Low);
        assert!(!at_rest.running, "the mount was at rest");
        assert!(!at_rest.initialised, "`F` had never been sent");
        assert_eq!(at_rest.raw, [1, 0, 0]);

        // After `:F1`: `=101`. One bit moved, and it is the one the table predicted.
        let initialised = AxisStatus::decode("101").expect("three hex digits");
        assert!(initialised.initialised);
        assert!(!initialised.running);
        assert_eq!(
            AxisStatus {
                initialised: true,
                raw: [1, 0, 1],
                ..at_rest
            },
            initialised
        );
    }

    #[test]
    fn each_documented_bit_is_read_from_its_own_nibble() {
        let cases: [(&str, MotionKind, MotionDirection, SpeedClass, bool, bool); 8] = [
            (
                "000",
                MotionKind::Goto,
                MotionDirection::Forward,
                SpeedClass::Low,
                false,
                false,
            ),
            (
                "100",
                MotionKind::Slew,
                MotionDirection::Forward,
                SpeedClass::Low,
                false,
                false,
            ),
            (
                "200",
                MotionKind::Goto,
                MotionDirection::Backward,
                SpeedClass::Low,
                false,
                false,
            ),
            (
                "400",
                MotionKind::Goto,
                MotionDirection::Forward,
                SpeedClass::High,
                false,
                false,
            ),
            (
                "010",
                MotionKind::Goto,
                MotionDirection::Forward,
                SpeedClass::Low,
                true,
                false,
            ),
            (
                "001",
                MotionKind::Goto,
                MotionDirection::Forward,
                SpeedClass::Low,
                false,
                true,
            ),
            (
                "711",
                MotionKind::Slew,
                MotionDirection::Backward,
                SpeedClass::High,
                true,
                true,
            ),
            (
                "311",
                MotionKind::Slew,
                MotionDirection::Backward,
                SpeedClass::Low,
                true,
                true,
            ),
        ];
        for (wire, kind, direction, speed, running, initialised) in cases {
            let s = AxisStatus::decode(wire).expect("valid status");
            assert_eq!(
                (s.kind, s.direction, s.speed, s.running, s.initialised),
                (kind, direction, speed, running, initialised),
                "status {wire} decoded wrongly"
            );
            assert!(
                !s.has_undocumented_bits(),
                "{wire} sets only documented bits"
            );
            assert_eq!(
                std::str::from_utf8(&s.encode()),
                Ok(wire),
                "{wire} must re-encode"
            );
        }
    }

    #[test]
    fn the_ascii_shortcut_and_this_decoder_agree_on_every_value_the_sources_describe() {
        // ENCODINGS.md tests the bits on the ASCII character. That is correct for 0..=7 and this
        // asserts the equivalence rather than assuming it — the point being that it is an
        // equivalence over a range, not an identity.
        for n in 0..=7u8 {
            let ascii = b'0' + n;
            assert_eq!(
                ascii & 0x07,
                n,
                "ASCII '{}' carries its value in the low three bits",
                n
            );
        }
        // ...and here is where it stops. 'A' is 0x41: the shortcut reads bit 0 set, i.e. SLEW,
        // for a nibble of 0b1010, which has bit 0 clear, i.e. GOTO.
        assert_eq!(b'A' & 0x01, 1);
        assert_eq!(0x0A & 0x01, 0);
        let s = AxisStatus::decode("A00").expect("A is a hex digit");
        assert_eq!(
            s.kind,
            MotionKind::Goto,
            "the nibble, not the character, decides"
        );
        assert_eq!(s.direction, MotionDirection::Backward);
    }

    #[test]
    fn a_bit_outside_the_table_is_preserved_and_flagged_rather_than_decoded_or_refused() {
        let s = AxisStatus::decode("822").expect("hex digits");
        assert_eq!(s.raw, [8, 2, 2]);
        assert!(s.has_undocumented_bits());
        // The five bits the driver depends on are unaffected by the ones it does not know.
        assert_eq!(s.kind, MotionKind::Goto);
        assert!(!s.running, "n2 bit 0 is clear whatever bit 1 means");
        assert!(!s.initialised);
    }

    #[test]
    fn every_documented_combination_round_trips() {
        for n1 in 0..=7u8 {
            for n2 in 0..=1u8 {
                for n3 in 0..=1u8 {
                    let s = AxisStatus::from_nibbles([n1, n2, n3]);
                    assert!(!s.has_undocumented_bits());
                    let wire = s.encode();
                    let text = std::str::from_utf8(&wire).expect("ASCII");
                    assert_eq!(
                        AxisStatus::decode(text),
                        Ok(s),
                        "{text} must survive a round trip"
                    );
                }
            }
        }
    }

    #[test]
    fn the_wrong_width_is_refused_rather_than_padded() {
        for bad in ["", "1", "10", "1000"] {
            assert!(
                matches!(
                    AxisStatus::decode(bad),
                    Err(ProtocolError::PayloadWidth { .. })
                ),
                "{bad:?} is not a status payload"
            );
        }
        assert!(matches!(
            AxisStatus::decode("1G0"),
            Err(ProtocolError::NotHex { .. })
        ));
    }
}
