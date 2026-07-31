//! The quantities the registers hold, each with its own type.
//!
//! Every one of these is a 24-bit number on the wire and they are trivially confusable there —
//! a step period, a counter and a counts-per-revolution are the same six characters. Above the
//! codec they are different kinds of thing, and M3-T03's arithmetic (`period = timer_freq /
//! counts_per_second`) mixes three of them in one expression. Separate types are what stop a
//! transposition in that expression from compiling.
//!
//! # [`Move`] is the one that carries two halves of one fact
//!
//! The protocol splits a relative move across two commands: `H` takes an unsigned magnitude and
//! `G` carries the direction. Nothing on the wire ties them together, and a driver that computed
//! them separately could send a magnitude of 1,000 with a backward direction and get a move to
//! the wrong place with no error anywhere. [`Move`] is constructed from a signed delta and hands
//! out both halves, so the two commands cannot disagree about which way the mount is going.

use super::error::EncodeError;
use super::hex::{U24, U24_MAX};
use super::motion::MotionDirection;

/// An absolute axis counter, as `:j` reports it.
///
/// Open-loop: it is the number of steps the motor controller believes it has issued, not encoder
/// feedback — the HEQ5 Pro has no position encoders (PRD §4.2). Everything downstream that
/// converts this to a sky coordinate inherits that caveat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Counts(pub U24);

impl Counts {
    /// The mechanical home counter, `0x800000`.
    pub const HOME: Self = Self(U24::HOME);

    /// Build from a raw count.
    ///
    /// # Errors
    /// [`EncodeError::NotU24`] beyond 24 bits.
    pub const fn new(value: u32) -> Result<Self, EncodeError> {
        match U24::new(value) {
            Ok(v) => Ok(Self(v)),
            Err(e) => Err(e),
        }
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Where a move lands, starting here.
    #[must_use]
    pub const fn after(self, mv: Move) -> Self {
        Self(self.0.wrapping_add_signed(mv.delta()))
    }
}

impl From<Counts> for u32 {
    fn from(c: Counts) -> Self {
        c.get()
    }
}

/// Counts per revolution of an axis, as `:a` reports it.
///
/// Read at handshake and **never hardcoded** (M3-T03): 9,024,000 is what the operator's HEQ5
/// answers and is a test fixture, not a constant of the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CountsPerRev(pub U24);

/// The motor controller's timer interrupt frequency in hertz, as `:b` reports it.
///
/// The measured value is 64,935 Hz, which corrected a documented 460,800 Hz that was wrong by a
/// factor of 7.0963 — see `spikes/skywatcher-heq5/FINDINGS.md`. Read at handshake for the same
/// reason as [`CountsPerRev`], and with rather more feeling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerFrequency(pub U24);

/// Timer ticks between motor steps — `I` writes it, `:i` reads it back.
///
/// Smaller is faster: `rate = timer_frequency / step_period`. 620 is the sidereal tracking
/// constant, confirmed on hardware at 104.617 counts/s against a predicted 104.7304.
///
/// **It does not control goto speed.** A 10× change left the goto velocity profile identical
/// (`spikes/skywatcher-heq5/FINDINGS.md`, E3), so `I` governs slew and tracking only. It is
/// still part of every goto sequence, and it is still worth reading back — a step period that
/// failed to land is evidence the whole write sequence went astray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepPeriod(pub U24);

impl StepPeriod {
    /// The measured sidereal tracking period, offered as a fixture rather than a constant.
    ///
    /// It is only correct at the timer frequency it was measured against, and the driver reads
    /// that at handshake. M3-T03 computes periods; this is what its arithmetic should reproduce
    /// for a mount that answers `:b` with 64,935.
    pub const SIDEREAL_AT_64935_HZ: Self = Self(U24::from_masked(620));

    /// Build from a raw period.
    ///
    /// # Errors
    /// [`EncodeError::NotU24`] beyond 24 bits.
    pub const fn new(value: u32) -> Result<Self, EncodeError> {
        match U24::new(value) {
            Ok(v) => Ok(Self(v)),
            Err(e) => Err(e),
        }
    }

    /// The period.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl From<StepPeriod> for u32 {
    fn from(p: StepPeriod) -> Self {
        p.get()
    }
}

/// The ratio between the two speed classes, as `:g` reports it.
///
/// One byte, two characters, no byte swap. The operator's mount answers 16, confirming PRD
/// §4.2's assumption — the one §4.2 figure the spike did *not* have to correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HighSpeedRatio(pub u8);

/// The ST4 autoguide rate `P` sets: one decimal digit.
///
/// **Semantics unverified.** `indi-eqmod` sends a single character `0`–`9` as a rate magnitude
/// and `spikes/skywatcher-heq5/ENCODINGS.md` records that the meaning of each level is not
/// confirmed. The codec therefore enforces the range and asserts nothing about what a given
/// level does; E15 in the motion plan is what would establish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuideRate(u8);

impl GuideRate {
    /// Build from a level.
    ///
    /// # Errors
    /// [`EncodeError::GuideRateOutOfRange`] outside `0..=9`.
    pub const fn new(level: u8) -> Result<Self, EncodeError> {
        if level > 9 {
            return Err(EncodeError::GuideRateOutOfRange(level as u32));
        }
        Ok(Self(level))
    }

    /// The level.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The single payload character.
    #[must_use]
    pub const fn digit(self) -> u8 {
        b'0' + self.0
    }
}

/// A relative move: how far, and which way.
///
/// The two halves travel in different commands and this type is what keeps them consistent.
/// [`Self::magnitude`] goes to `H` or `M`; [`Self::direction`] goes to `G`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Move {
    magnitude: U24,
    direction: MotionDirection,
}

impl Move {
    /// A move of zero counts.
    pub const NONE: Self = Self {
        magnitude: U24::ZERO,
        direction: MotionDirection::Forward,
    };

    /// Build from a signed delta in counts.
    ///
    /// This is the only constructor, and taking a signed value is the point: a caller who has
    /// computed "target minus current" hands that number over directly and cannot lose its sign
    /// on the way to the wire.
    ///
    /// # Errors
    /// [`EncodeError::MoveTooLong`] if the magnitude exceeds the 24-bit increment register.
    pub const fn from_delta(delta: i64) -> Result<Self, EncodeError> {
        let magnitude = delta.unsigned_abs();
        if magnitude > U24_MAX as u64 {
            return Err(EncodeError::MoveTooLong { delta });
        }
        // Bounded by the check above, so the narrowing is exact.
        let magnitude = U24::from_masked(magnitude as u32);
        Ok(Self {
            magnitude,
            direction: MotionDirection::of_delta(delta),
        })
    }

    /// The unsigned magnitude — what `H` and `M` carry.
    #[must_use]
    pub const fn magnitude(self) -> U24 {
        self.magnitude
    }

    /// The direction — what `G` carries.
    #[must_use]
    pub const fn direction(self) -> MotionDirection {
        self.direction
    }

    /// The signed delta this move represents.
    ///
    /// Always fits an `i32`: the magnitude is bounded by 24 bits.
    #[must_use]
    pub const fn delta(self) -> i32 {
        self.magnitude.get() as i32 * self.direction.sign()
    }

    /// A shorter move in the same direction — how a break point relates to its goto.
    ///
    /// # Errors
    /// [`EncodeError::BreakPointOutsideMove`] unless `counts` lies inside this move. A break
    /// point of zero is refused along with one past the target: both describe a deceleration
    /// that is not part of the motion, and both are what a sign error looks like.
    pub const fn shortened_to(self, counts: u32) -> Result<Self, EncodeError> {
        let total = self.magnitude.get();
        if counts == 0 || counts > total {
            return Err(EncodeError::BreakPointOutsideMove {
                brake: counts,
                total,
            });
        }
        // `counts <= total` and `total` is a valid `U24`, so this cannot mask anything off.
        Ok(Self {
            magnitude: U24::from_masked(counts),
            direction: self.direction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_keeps_its_sign_and_its_magnitude_together() {
        let forward = Move::from_delta(1_000).expect("fits");
        assert_eq!(forward.magnitude().get(), 1_000);
        assert_eq!(forward.direction(), MotionDirection::Forward);
        assert_eq!(forward.delta(), 1_000);

        // The whole reason the type exists: `H` sees 1,000 either way, and only `G` differs.
        let backward = Move::from_delta(-1_000).expect("fits");
        assert_eq!(backward.magnitude().get(), 1_000);
        assert_eq!(backward.direction(), MotionDirection::Backward);
        assert_eq!(backward.delta(), -1_000);
        assert_eq!(forward.magnitude(), backward.magnitude());
    }

    #[test]
    fn a_move_longer_than_the_register_is_refused_in_both_directions() {
        assert!(Move::from_delta(i64::from(U24_MAX)).is_ok());
        assert!(Move::from_delta(-i64::from(U24_MAX)).is_ok());
        assert_eq!(
            Move::from_delta(i64::from(U24_MAX) + 1),
            Err(EncodeError::MoveTooLong { delta: 16_777_216 })
        );
        assert_eq!(
            Move::from_delta(-i64::from(U24_MAX) - 1),
            Err(EncodeError::MoveTooLong { delta: -16_777_216 })
        );
        // i64::MIN has no positive counterpart; `unsigned_abs` is why that is not a panic.
        assert!(matches!(
            Move::from_delta(i64::MIN),
            Err(EncodeError::MoveTooLong { .. })
        ));
    }

    #[test]
    fn a_counter_plus_a_move_is_where_the_readback_should_point() {
        // E14's numbers: position 8,654,669, `:H` = 12,345, `:h` read 8,667,014.
        let at = Counts::new(8_654_669).expect("fits");
        let mv = Move::from_delta(12_345).expect("fits");
        assert_eq!(at.after(mv).get(), 8_667_014);
        // ...and the same move backward, which E13 exercised but E14 did not read back.
        let back = Move::from_delta(-12_345).expect("fits");
        assert_eq!(at.after(back).get(), 8_642_324);
    }

    #[test]
    fn a_break_point_must_lie_inside_the_move_it_decelerates() {
        let mv = Move::from_delta(-10_000).expect("fits");
        let brake = mv.shortened_to(5_000).expect("inside the move");
        assert_eq!(brake.magnitude().get(), 5_000);
        assert_eq!(
            brake.direction(),
            MotionDirection::Backward,
            "brake follows the move"
        );
        assert!(
            mv.shortened_to(10_000).is_ok(),
            "a break point at the target is degenerate, not wrong"
        );
        assert_eq!(
            mv.shortened_to(10_001),
            Err(EncodeError::BreakPointOutsideMove {
                brake: 10_001,
                total: 10_000
            })
        );
        assert_eq!(
            mv.shortened_to(0),
            Err(EncodeError::BreakPointOutsideMove {
                brake: 0,
                total: 10_000
            })
        );
    }

    #[test]
    fn the_guide_rate_is_one_digit() {
        for level in 0..=9u8 {
            let r = GuideRate::new(level).expect("in range");
            assert_eq!(r.get(), level);
            assert_eq!(r.digit(), b'0' + level);
        }
        assert_eq!(
            GuideRate::new(10),
            Err(EncodeError::GuideRateOutOfRange(10))
        );
    }

    #[test]
    fn the_sidereal_fixture_is_the_measured_period() {
        assert_eq!(StepPeriod::SIDEREAL_AT_64935_HZ.get(), 620);
    }
}
