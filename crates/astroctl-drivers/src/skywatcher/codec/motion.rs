//! `G` SetMotionMode — the two characters that decide how fast the mount moves and which way.
//!
//! # Why this is a type and not a string
//!
//! The mode character is not a bit field with a speed bit. Reading the four values as bits makes
//! it look like one, and the reading is wrong:
//!
//! | digit | motion | speed | "bit 1" says |
//! |-------|--------|-------|--------------|
//! | `0` | GOTO | **high** | 0 |
//! | `1` | SLEW | **low** | 0 |
//! | `2` | GOTO | **low** | 1 |
//! | `3` | SLEW | **high** | 1 |
//!
//! Bit 0 does mean SLEW-versus-GOTO consistently. Bit 1 means *high* speed when bit 0 is clear
//! and *low* speed when it is set — the polarity of the speed bit depends on the motion kind.
//! `spikes/skywatcher-heq5/ENCODINGS.md` calls the packing "counterintuitive"; this is the
//! shape of it. Any code that computes this character arithmetically will get one of the four
//! wrong, and getting it wrong costs a factor of sixteen in speed rather than a direction — the
//! verified high-speed ratio is 16 (`:g` → `10`), so `2` mistyped as `0` turns a 51× sidereal
//! goto into an 817× one.
//!
//! So: one table, four rows, feeding both directions, and a test that asserts the four digits
//! against the reference literally rather than round-tripping them. A round trip through a
//! table with a transposed row passes; only the literals catch it.
//!
//! Three independent sources agree on this table — the vendor wiki, `indi-eqmod`'s opcode enum
//! and its call sites (S1/S2/S3 of ENCODINGS.md) — and our own `motion.py` drove real hardware
//! with `"20"` (GOTO, low, forward) and `"10"` (SLEW, low, forward) to measured, correct results.
//!
//! # This is *not* the layout `:f` reports
//!
//! [`super::status::AxisStatus`] decodes the same three concepts out of the `f` reply, and its
//! first nibble is a plain, consistent bit field: bit 0 slew, bit 1 backward, bit 2 high speed.
//! The two encodings share their meaning and share nothing else. Assuming a mode digit can be
//! compared against a status nibble is a bug that would look right for `0` and `1`.

use super::error::ProtocolError;

/// Whether a motion has a target and stops itself, or runs until told to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotionKind {
    /// Bounded: `H`/`M` set a target and the mount decelerates and stops on its own. The
    /// self-terminating property is why bring-up's first commanded motion was a goto.
    Goto,
    /// Unbounded: runs at the step period until `K` or `L` arrives. Tracking is a slew.
    Slew,
}

/// The mount's two speed classes.
///
/// The ratio between them is not a constant of this codec — it is read at handshake from `:g`,
/// which the operator's mount answered `10`, i.e. 16.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpeedClass {
    /// Low speed. Tracking and fine slews; measured goto plateau ≈ 5,350 counts/s.
    Low,
    /// High speed. Measured goto cruise ≈ 87,000 counts/s, 835× sidereal.
    High,
}

/// Which way an axis turns.
///
/// Direction is *not* carried by the increment: `H` takes an unsigned magnitude, and this is
/// where the sign lives. `motion.py` drove ±100,000-count gotos this way with zero counts of
/// error in both directions (E13), so the split is measured rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotionDirection {
    /// Increasing counter.
    Forward,
    /// Decreasing counter.
    Backward,
}

impl MotionDirection {
    /// The sign this direction applies to a magnitude.
    #[must_use]
    pub const fn sign(self) -> i32 {
        match self {
            Self::Forward => 1,
            Self::Backward => -1,
        }
    }

    /// The direction a signed delta implies. Zero is forward, arbitrarily and harmlessly: a
    /// zero-length move goes nowhere in either direction.
    #[must_use]
    pub const fn of_delta(delta: i64) -> Self {
        if delta < 0 {
            Self::Backward
        } else {
            Self::Forward
        }
    }
}

/// The `G` payload: what kind of motion, at which speed class, in which direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotionMode {
    /// Bounded or unbounded.
    pub kind: MotionKind,
    /// Speed class.
    pub speed: SpeedClass,
    /// Direction of travel.
    pub direction: MotionDirection,
}

/// The mode digit table — the single source of truth for both directions of the mapping.
///
/// Transcribed from `spikes/skywatcher-heq5/ENCODINGS.md`, which took it from three sources
/// that agree. Encoding searches it; decoding searches it; the golden vectors assert the digits.
const MODE_TABLE: [(MotionKind, SpeedClass, u8); 4] = [
    (MotionKind::Goto, SpeedClass::High, b'0'),
    (MotionKind::Slew, SpeedClass::Low, b'1'),
    (MotionKind::Goto, SpeedClass::Low, b'2'),
    (MotionKind::Slew, SpeedClass::High, b'3'),
];

/// The direction digit table.
const DIRECTION_TABLE: [(MotionDirection, u8); 2] = [
    (MotionDirection::Forward, b'0'),
    (MotionDirection::Backward, b'1'),
];

impl MotionMode {
    /// All eight combinations, in mode-digit order. The reference set T-COD-1 checks against.
    pub const ALL: [Self; 8] = [
        Self::new(MotionKind::Goto, SpeedClass::High, MotionDirection::Forward),
        Self::new(
            MotionKind::Goto,
            SpeedClass::High,
            MotionDirection::Backward,
        ),
        Self::new(MotionKind::Slew, SpeedClass::Low, MotionDirection::Forward),
        Self::new(MotionKind::Slew, SpeedClass::Low, MotionDirection::Backward),
        Self::new(MotionKind::Goto, SpeedClass::Low, MotionDirection::Forward),
        Self::new(MotionKind::Goto, SpeedClass::Low, MotionDirection::Backward),
        Self::new(MotionKind::Slew, SpeedClass::High, MotionDirection::Forward),
        Self::new(
            MotionKind::Slew,
            SpeedClass::High,
            MotionDirection::Backward,
        ),
    ];

    /// Build a mode.
    #[must_use]
    pub const fn new(kind: MotionKind, speed: SpeedClass, direction: MotionDirection) -> Self {
        Self {
            kind,
            speed,
            direction,
        }
    }

    /// A bounded motion: the mount stops itself at the target.
    #[must_use]
    pub const fn goto(speed: SpeedClass, direction: MotionDirection) -> Self {
        Self::new(MotionKind::Goto, speed, direction)
    }

    /// An unbounded motion: tracking, or a manual slew under its TTL.
    #[must_use]
    pub const fn slew(speed: SpeedClass, direction: MotionDirection) -> Self {
        Self::new(MotionKind::Slew, speed, direction)
    }

    /// The two payload characters of `:G<axis><mode><dir>`.
    #[must_use]
    pub fn wire(self) -> [u8; 2] {
        let mode = MODE_TABLE
            .iter()
            .find(|(k, s, _)| *k == self.kind && *s == self.speed)
            .map_or(b'?', |&(_, _, digit)| digit);
        let dir = DIRECTION_TABLE
            .iter()
            .find(|(d, _)| *d == self.direction)
            .map_or(b'?', |&(_, digit)| digit);
        // Both tables are total over their enums — four kind×speed pairs, two directions — so
        // neither fallback is reachable. `map_or` rather than `expect` keeps the encode path
        // free of a panic that a future fifth speed class could otherwise reach; the `?` would
        // fail its golden vector immediately and loudly.
        [mode, dir]
    }

    /// Read a `G` payload back.
    ///
    /// Nothing on the wire produces one — `G` is a write and the mount answers it with a bare
    /// `=`. It exists so the vector file can be round-tripped in both directions and so M3-T02's
    /// mock port can understand a frame the driver sent it.
    ///
    /// # Errors
    /// [`ProtocolError::BadMotionMode`] for any pair outside the eight.
    pub fn from_wire(payload: &str) -> Result<Self, ProtocolError> {
        let bad = || ProtocolError::BadMotionMode(payload.to_owned());
        let [mode, dir] = payload.as_bytes() else {
            return Err(bad());
        };
        let &(kind, speed, _) = MODE_TABLE
            .iter()
            .find(|(_, _, d)| d == mode)
            .ok_or_else(bad)?;
        let &(direction, _) = DIRECTION_TABLE
            .iter()
            .find(|(_, d)| d == dir)
            .ok_or_else(bad)?;
        Ok(Self {
            kind,
            speed,
            direction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::MotionDirection::{Backward, Forward};
    use super::MotionKind::{Goto, Slew};
    use super::SpeedClass::{High, Low};
    use super::*;

    /// The reference table from `spikes/skywatcher-heq5/ENCODINGS.md`, written out in full.
    ///
    /// Deliberately a second, literal copy of what [`MODE_TABLE`] encodes. A round-trip test
    /// passes happily against a table with two rows swapped; only literals disagree with it.
    const REFERENCE: [(MotionKind, SpeedClass, MotionDirection, &str); 8] = [
        (Goto, High, Forward, "00"),
        (Goto, High, Backward, "01"),
        (Slew, Low, Forward, "10"),
        (Slew, Low, Backward, "11"),
        (Goto, Low, Forward, "20"),
        (Goto, Low, Backward, "21"),
        (Slew, High, Forward, "30"),
        (Slew, High, Backward, "31"),
    ];

    #[test]
    fn all_eight_combinations_match_the_reference_digits() {
        for (kind, speed, direction, expected) in REFERENCE {
            let mode = MotionMode::new(kind, speed, direction);
            assert_eq!(
                std::str::from_utf8(&mode.wire()),
                Ok(expected),
                "{kind:?}/{speed:?}/{direction:?} must encode as {expected}"
            );
            assert_eq!(MotionMode::from_wire(expected), Ok(mode));
        }
    }

    #[test]
    fn the_reference_covers_the_whole_type() {
        // If a fifth speed class or a third motion kind is ever added, this fails rather than
        // letting the reference silently stop being exhaustive.
        assert_eq!(REFERENCE.len(), MotionMode::ALL.len());
        for mode in MotionMode::ALL {
            assert!(
                REFERENCE
                    .iter()
                    .any(|&(k, s, d, _)| MotionMode::new(k, s, d) == mode),
                "{mode:?} is not in the reference table"
            );
        }
    }

    #[test]
    fn the_two_speed_classes_are_not_one_bit_of_the_mode_digit() {
        // The whole reason this is a table. GOTO reads 0 as fast; SLEW reads 0 as slow.
        assert_eq!(MotionMode::goto(High, Forward).wire()[0], b'0');
        assert_eq!(MotionMode::goto(Low, Forward).wire()[0], b'2');
        assert_eq!(MotionMode::slew(Low, Forward).wire()[0], b'1');
        assert_eq!(MotionMode::slew(High, Forward).wire()[0], b'3');
    }

    #[test]
    fn the_mode_the_bring_up_experiments_used_is_the_one_the_spike_recorded() {
        // `motion.py` sent "20" for every bounded goto and "10" for the tracking-rate slew, and
        // both produced the rates the design predicted. These two digits have hardware behind them.
        assert_eq!(MotionMode::goto(Low, Forward).wire(), *b"20");
        assert_eq!(MotionMode::slew(Low, Forward).wire(), *b"10");
    }

    #[test]
    fn direction_carries_the_sign_the_increment_does_not() {
        assert_eq!(MotionDirection::of_delta(1_000), Forward);
        assert_eq!(MotionDirection::of_delta(-1_000), Backward);
        assert_eq!(MotionDirection::of_delta(0), Forward);
        assert_eq!(Forward.sign(), 1);
        assert_eq!(Backward.sign(), -1);
    }

    #[test]
    fn a_pair_outside_the_eight_is_refused() {
        for bad in ["", "0", "40", "02", "0a", "000", " 0"] {
            assert_eq!(
                MotionMode::from_wire(bad),
                Err(ProtocolError::BadMotionMode(bad.to_owned())),
                "{bad:?} is not a motion mode"
            );
        }
    }
}
