//! Angles, counters, and the exact arithmetic between them.
//!
//! # Three angles that are not interchangeable
//!
//! [`Lst`], [`HourAngle`] and [`AxisAngle`] are all "an angle in degrees" and all three appear in
//! one expression — `RA = LST − HA`, `HA = ±(axis angle + 90°) (+180°)`. They are separate types
//! because the folding conventions differ (`[0, 360)` for sidereal time, `[-180, 180)` for the
//! other two)
//! and because a transposition in that expression is a pointing error of up to twelve hours that
//! nothing downstream can detect: the mount would answer every inquiry consistently and point at
//! the wrong sky.
//!
//! # The counter fold is integer arithmetic, not floating point
//!
//! [`AxisScale::angle_at`] reduces `counts − home` modulo the counts-per-revolution **in `i64`**
//! before it ever reaches an `f64`. Doing the fold in degrees instead would lose the low bits of
//! a nine-million-count offset — 9,024,000 needs 24 bits and `f64` has 53, so the division itself
//! is exact, but a `rem_euclid` after two roundings is not, and the round-trip test in this module
//! asserts a tolerance of *zero* counts. The acceptance criterion (T-POS-1) says one count; this
//! arithmetic gives none, and asserting the stronger property is what would catch the weaker one
//! degrading.
//!
//! # The counter wraps at 2²⁴, and the mechanism wraps at CPR
//!
//! These are different numbers: 16,777,216 against 9,024,000, and 2²⁴ mod CPR is 7,753,216
//! counts — 309.3°. So a counter that has crossed the 24-bit boundary decodes to an angle that is
//! **wrong by 50.7°**, silently, in the mount as much as here.
//!
//! It looked unreachable and it is not. The canonical band [`AxisScale::counts_at`] produces runs
//! 3,876,608..12,900,608, which leaves 3.88 million counts of slack under the boundary — but a
//! single move may be 4.51 million, so a right-ascension axis sitting near the anti-meridian and
//! sent most of a half-turn overshoots the bottom of the counter by up to 635,392. The property
//! test in [`super::target`] found it; nothing in the arithmetic would have.
//!
//! [`AxisScale::reachable_delta`] is the answer, and [`super::target::goto_solution`] is the only
//! thing that needs it: destinations a whole revolution apart name the same direction in the sky,
//! so it takes the shortest one that stays inside the counter. The declination axis never needs
//! the adjustment, because the no-flip rule confines both endpoints to one half of the circle.

use crate::skywatcher::codec::{Counts, CountsPerRev};

use super::MathError;

/// Degrees in one full turn.
pub const DEGREES_PER_TURN: f64 = 360.0;

/// Degrees of hour angle per hour of right ascension.
pub const DEGREES_PER_HOUR: f64 = 15.0;

/// Arcseconds in a degree.
pub const ARCSEC_PER_DEGREE: f64 = 3_600.0;

/// The mechanical home counter, `0x800000` — verified on both axes of the operator's HEQ5
/// (`spikes/skywatcher-heq5/FINDINGS.md`).
const HOME_COUNTS: i64 = 0x0080_0000;

/// The 24-bit counter's modulus. See the module docs: this is *not* the counts per revolution.
const COUNTER_MODULUS: i64 = 0x0100_0000;

/// Folds an angle into `[-180, 180)`.
///
/// Half-open at the top so that every direction has exactly one representation; +180° and −180°
/// are the same direction and the canonical one is negative.
#[must_use]
pub fn wrap_signed(degrees: f64) -> f64 {
    let wrapped = (degrees + 180.0).rem_euclid(DEGREES_PER_TURN) - 180.0;
    // `rem_euclid` returns exactly 360.0 for inputs a hair below zero, which lands here as +180 —
    // the one value the half-open range excludes.
    if wrapped >= 180.0 {
        -180.0
    } else {
        wrapped
    }
}

/// Folds an angle into `[0, 360)`.
#[must_use]
pub fn wrap_turn(degrees: f64) -> f64 {
    let wrapped = degrees.rem_euclid(DEGREES_PER_TURN);
    if wrapped >= DEGREES_PER_TURN {
        0.0
    } else {
        wrapped
    }
}

/// Rejects a non-finite input at the door, naming what it was meant to be.
fn finite(quantity: &'static str, value: f64) -> Result<f64, MathError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(MathError::NotFinite { quantity, value })
    }
}

/// Local sidereal time as an angle, folded into `[0, 360)`.
///
/// **Injected, never computed here.** SDD §5.2.3 puts the clock behind a seam so the Phase 2a
/// erfa pipeline can replace it without touching this arithmetic, and this crate could not
/// compute it anyway: `astroctl-drivers` may depend only on `astroctl-hal` and `astroctl-core`
/// (ADD §5.6 rule 1), and the workspace's one sidereal-time implementation lives in
/// `astroctl-safety::horizontal::local_sidereal_degrees`. A second copy of a GMST series is
/// exactly the duplication that crate's own documentation argues against — two implementations
/// let a limit and a display disagree — so the seam is a parameter and the field node supplies
/// the one implementation there is.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Lst(f64);

impl Lst {
    /// From degrees, folded into `[0, 360)`.
    ///
    /// # Errors
    /// [`MathError::NotFinite`] for `NaN` or an infinity.
    pub fn from_degrees(degrees: f64) -> Result<Self, MathError> {
        Ok(Self(wrap_turn(finite("local sidereal time", degrees)?)))
    }

    /// From hours, folded into `[0, 24)`.
    ///
    /// # Errors
    /// [`MathError::NotFinite`] for `NaN` or an infinity.
    pub fn from_hours(hours: f64) -> Result<Self, MathError> {
        Self::from_degrees(finite("local sidereal time", hours)? * DEGREES_PER_HOUR)
    }

    /// Degrees, in `[0, 360)`.
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }

    /// Hours, in `[0, 24)`.
    #[must_use]
    pub fn hours(self) -> f64 {
        self.0 / DEGREES_PER_HOUR
    }
}

/// Hour angle in degrees, folded into `[-180, 180)`.
///
/// Negative is east of the meridian (rising), positive is west (setting) — the same sign
/// convention `astroctl-safety`'s meridian watch compares against, and the one SDD §5.4's
/// meridian limit is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct HourAngle(f64);

impl HourAngle {
    /// The meridian.
    pub const MERIDIAN: Self = Self(0.0);

    /// From degrees, folded into `[-180, 180)`.
    ///
    /// # Errors
    /// [`MathError::NotFinite`] for `NaN` or an infinity.
    pub fn from_degrees(degrees: f64) -> Result<Self, MathError> {
        Ok(Self(wrap_signed(finite("hour angle", degrees)?)))
    }

    /// `HA = LST − RA` (SDD §5.2.3).
    #[must_use]
    pub fn of(lst: Lst, right_ascension: astroctl_core::types::RaHours) -> Self {
        Self::wrapped(lst.degrees() - right_ascension.hours() * DEGREES_PER_HOUR)
    }

    /// Fold a value the caller has already established is finite.
    ///
    /// Crate-internal so the decomposition in [`super::mech`] can build one without a `Result`
    /// it would have to launder: every operand there is a validated newtype, so the check
    /// [`Self::from_degrees`] performs has already happened at the boundary.
    pub(super) fn wrapped(degrees: f64) -> Self {
        Self(wrap_signed(degrees))
    }

    /// `RA = LST − HA` — the same identity read the other way (SDD §5.2.3).
    ///
    /// Infallible: both operands are finite by construction and [`RaHours`] normalises.
    ///
    /// [`RaHours`]: astroctl_core::types::RaHours
    #[must_use]
    pub fn right_ascension(self, lst: Lst) -> astroctl_core::types::RaHours {
        let hours = (lst.degrees() - self.0) / DEGREES_PER_HOUR;
        astroctl_core::types::RaHours::new(hours).unwrap_or_else(|_| {
            // Unreachable: both terms are finite, so the quotient is, and `RaHours::new` rejects
            // only non-finite input. A fallback rather than an `expect` because this is on the
            // 1 Hz position poll (SDD §5.2.3), which must not be able to panic for an arithmetic
            // reason — the same argument the simulator's `sky_position` makes.
            astroctl_core::types::RaHours::new(0.0).unwrap_or_else(|_| unreachable!("0 h is an RA"))
        })
    }

    /// Degrees, in `[-180, 180)`.
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }

    /// Hours, in `[-12, 12)`.
    #[must_use]
    pub fn hours(self) -> f64 {
        self.0 / DEGREES_PER_HOUR
    }
}

/// A mechanical axis angle in degrees, measured from the home counter, folded into `[-180, 180)`.
///
/// This is what the axis counter means and nothing more: it is not a declination, not an hour
/// angle, and not signed the same way as either. [`super::mech`] is where it becomes a sky
/// coordinate, and it needs the hemisphere and the pier side to do it.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct AxisAngle(f64);

impl AxisAngle {
    /// The home counter, `0x800000`.
    pub const HOME: Self = Self(0.0);

    /// From degrees, folded into `[-180, 180)`.
    ///
    /// # Errors
    /// [`MathError::NotFinite`] for `NaN` or an infinity.
    pub fn from_degrees(degrees: f64) -> Result<Self, MathError> {
        Ok(Self(wrap_signed(finite("axis angle", degrees)?)))
    }

    /// Fold a value the caller has already established is finite — see
    /// [`HourAngle::wrapped`].
    pub(super) fn wrapped(degrees: f64) -> Self {
        Self(wrap_signed(degrees))
    }

    /// Degrees, in `[-180, 180)`.
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

/// Counts per revolution, and every conversion that depends on it.
///
/// Constructed from what `:a` reported at handshake and from nothing else. 9,024,000 appears in
/// this crate only inside `#[cfg(test)]` fixtures and in the simulator's profile, both labelled;
/// PRD §4.2 and SDD §5.2.3 both make the read mandatory, and the timer-frequency episode is why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AxisScale {
    counts_per_revolution: u32,
}

impl AxisScale {
    /// From the handshake's `:a` reply.
    ///
    /// # Errors
    /// [`MathError::ZeroCountsPerRevolution`] for a reply of zero, which every conversion here
    /// would divide by. A mount that answers `000000` is a mount that is not answering.
    pub fn new(counts_per_revolution: CountsPerRev) -> Result<Self, MathError> {
        let counts_per_revolution = counts_per_revolution.0.get();
        if counts_per_revolution == 0 {
            return Err(MathError::ZeroCountsPerRevolution);
        }
        Ok(Self {
            counts_per_revolution,
        })
    }

    /// The counts per revolution this scale was built from.
    #[must_use]
    pub const fn counts_per_revolution(self) -> u32 {
        self.counts_per_revolution
    }

    /// Degrees one count subtends.
    #[must_use]
    pub fn degrees_per_count(self) -> f64 {
        DEGREES_PER_TURN / f64::from(self.counts_per_revolution)
    }

    /// Arcseconds one count subtends — 0.1436″ on the operator's mount, the unit SDD §5.2.3's
    /// 10-count goto tolerance and the spike's 84-count stop overshoot are both quoted in.
    #[must_use]
    pub fn arcsec_per_count(self) -> f64 {
        self.degrees_per_count() * ARCSEC_PER_DEGREE
    }

    /// Counts in `degrees` of axis rotation, unrounded and signed.
    #[must_use]
    pub fn counts_in(self, degrees: f64) -> f64 {
        degrees / DEGREES_PER_TURN * f64::from(self.counts_per_revolution)
    }

    /// The mechanical angle a counter represents.
    ///
    /// The fold is exact integer arithmetic: `counts − home` is reduced modulo the counts per
    /// revolution into `[-CPR/2, CPR/2)` before any division happens. See the module docs for why
    /// that matters and for the 2²⁴-versus-CPR hazard it cannot protect against.
    #[must_use]
    pub fn angle_at(self, counts: Counts) -> AxisAngle {
        let modulus = i64::from(self.counts_per_revolution);
        let mut offset = (i64::from(counts.get()) - HOME_COUNTS).rem_euclid(modulus);
        if 2 * offset >= modulus {
            offset -= modulus;
        }
        AxisAngle(wrap_signed(offset as f64 * self.degrees_per_count()))
    }

    /// The counter nearest home that sits at `angle`.
    ///
    /// Always within half a revolution of `0x800000`, so for any counts-per-revolution the
    /// register can hold, the result is a valid 24-bit counter — see the module docs.
    #[must_use]
    pub fn counts_at(self, angle: AxisAngle) -> Counts {
        let offset = self.counts_in(angle.degrees()).round() as i64;
        let raw = (HOME_COUNTS + offset).rem_euclid(COUNTER_MODULUS) as u32;
        match Counts::new(raw) {
            Ok(counts) => counts,
            // Unreachable: `rem_euclid(2^24)` is exactly the constructor's domain. A total
            // fallback rather than an `expect`, for the reason `HourAngle::right_ascension` gives.
            Err(_) => Counts::HOME,
        }
    }

    /// The signed count delta that moves `from` to `to` by the shorter way round.
    ///
    /// In `[-CPR/2, CPR/2)`, so it always fits the 24-bit increment register and always fits an
    /// `i32`. "Shorter way round" is the whole of what makes a goto *nearest*: the same
    /// destination reached the long way is the same coordinate and several minutes of slew.
    ///
    /// **This is the geometric answer and not always the one to send** — see
    /// [`Self::reachable_delta`].
    #[must_use]
    pub fn shortest_delta(self, from: Counts, to: Counts) -> i64 {
        let modulus = i64::from(self.counts_per_revolution);
        let mut delta = (i64::from(to.get()) - i64::from(from.get())).rem_euclid(modulus);
        if 2 * delta >= modulus {
            delta -= modulus;
        }
        delta
    }

    /// The shortest delta whose **destination is still a 24-bit counter**.
    ///
    /// # The one place the two moduli bite
    ///
    /// The counter wraps at 2²⁴ and the mechanism wraps at CPR, and 2²⁴ mod 9,024,000 is
    /// 7,753,216 counts — 309.3°. So a move whose arithmetic destination falls outside
    /// `[0, 2²⁴)` lands on a counter that decodes to an angle **50.7° wrong**, silently, in the
    /// mount as much as in this module. It is reachable with ordinary numbers: the canonical band
    /// runs 3,876,608..12,900,608 and a single move may be 4,512,000 counts, which overshoots the
    /// bottom of the counter by up to 635,392.
    ///
    /// Since the destinations differ by whole revolutions, they name the same direction in the
    /// sky, so the fix costs nothing but distance: take the shortest delta that lands in range.
    /// For the right-ascension axis that is the long way round on the rare occasion it triggers —
    /// a few minutes of slew, once, at the anti-meridian — and for the declination axis it never
    /// triggers at all, because [`super::target::goto_solution`] confines both endpoints to one
    /// half of the circle and both halves sit comfortably inside the counter. That the no-flip
    /// rule also removes this hazard is a consequence rather than a design, but it is a real one.
    ///
    /// # What is not verified
    ///
    /// **What the mount does at the boundary is unmeasured.** `U24::wrapping_add_signed`
    /// documents modulo-2²⁴ as *derived, not measured* — no experiment has driven a counter
    /// across `0xFFFFFF`. This function's job is therefore to keep the driver away from the
    /// boundary rather than to be right about it, which is the correct response to an unknown
    /// that only a hardware experiment can close (`FINDINGS.md`'s open items; M3-T05).
    ///
    /// # Errors
    /// [`MathError::UnreachableCounter`] if no whole-revolution offset lands the destination in
    /// range with a magnitude the increment register can hold. Unreachable for any
    /// counts-per-revolution the register itself can hold.
    pub fn reachable_delta(self, from: Counts, to: Counts) -> Result<i64, MathError> {
        let modulus = i64::from(self.counts_per_revolution);
        let shortest = self.shortest_delta(from, to);
        let base = i64::from(from.get());
        [shortest, shortest + modulus, shortest - modulus]
            .into_iter()
            .filter(|delta| {
                let destination = base + delta;
                (0..COUNTER_MODULUS).contains(&destination)
                    && delta.unsigned_abs() <= u64::from(super::super::codec::U24_MAX)
            })
            .min_by_key(|delta| delta.abs())
            .ok_or(MathError::UnreachableCounter {
                from: from.get(),
                to: to.get(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::hex::U24;

    /// The operator's HEQ5 answered `:a` with this. **A fixture, not a constant of the driver** —
    /// see [`AxisScale`].
    const FIXTURE_CPR: u32 = 9_024_000;

    fn scale() -> AxisScale {
        AxisScale::new(CountsPerRev(U24::new(FIXTURE_CPR).expect("fits"))).expect("non-zero")
    }

    #[test]
    fn home_is_the_zero_of_the_mechanical_angle() {
        assert_eq!(scale().angle_at(Counts::HOME), AxisAngle::HOME);
        assert_eq!(scale().counts_at(AxisAngle::HOME), Counts::HOME);
    }

    #[test]
    fn a_quarter_turn_is_a_quarter_of_the_counts_per_revolution() {
        let scale = scale();
        let quarter = Counts::new(0x0080_0000 + FIXTURE_CPR / 4).expect("fits");
        assert!((scale.angle_at(quarter).degrees() - 90.0).abs() < 1e-9);
        assert_eq!(
            scale.counts_at(AxisAngle::from_degrees(90.0).unwrap()),
            quarter
        );
    }

    #[test]
    fn every_counter_within_half_a_turn_of_home_round_trips_to_the_exact_count() {
        // T-POS-1 asks for one count. This asserts zero, over the whole band the mechanism can
        // reach, at a stride that is coprime with nothing in particular so the samples do not all
        // land on the same residue.
        let scale = scale();
        let half = (FIXTURE_CPR / 2) as i64;
        let mut worst = 0_i64;
        let mut offset = -half;
        while offset < half {
            let counts = Counts::new((0x0080_0000 + offset) as u32).expect("in band");
            let back = scale.counts_at(scale.angle_at(counts));
            worst = worst.max((i64::from(back.get()) - i64::from(counts.get())).abs());
            offset += 7_919; // a prime stride: 1,139 samples across the band
        }
        assert_eq!(worst, 0, "the round trip lost {worst} counts");
    }

    #[test]
    fn the_shortest_delta_never_goes_the_long_way() {
        let scale = scale();
        let home = Counts::HOME;
        // A target 359° "ahead" is 1° behind, and a goto that took the long way would slew for
        // minutes to arrive at the same place.
        let almost_all_the_way = scale.counts_at(AxisAngle::from_degrees(-1.0).unwrap());
        let delta = scale.shortest_delta(home, almost_all_the_way);
        assert!(delta < 0, "expected a backward move, got {delta}");
        // 9,024,000 ÷ 360 = 25,066.67 counts in a degree, so one degree is 25,067 counts and not
        // the 25,066 an integer division would suggest. Half a count is the whole error budget
        // here and rounding down would spend it before the mount is even involved.
        assert_eq!(delta.abs(), 25_067);

        // ...and the bound the increment register needs.
        let half = i64::from(FIXTURE_CPR) / 2;
        for to in [0_u32, 1, 0x00FF_FFFF, 0x0080_0000] {
            let d = scale.shortest_delta(home, Counts::new(to).unwrap());
            assert!((-half..=half).contains(&d), "{to} gave a delta of {d}");
        }
    }

    #[test]
    fn the_hour_angle_identity_runs_both_ways() {
        // RA = LST − HA and HA = LST − RA are the same statement, and getting one of them
        // backwards is a pointing error that grows to twelve hours and looks like a clock fault.
        let lst = Lst::from_hours(18.5).expect("valid");
        for ra_hours in [0.0, 1.25, 11.99, 12.0, 18.5, 23.75] {
            let ra = astroctl_core::types::RaHours::new(ra_hours).expect("valid");
            let ha = HourAngle::of(lst, ra);
            let back = ha.right_ascension(lst);
            assert!(
                (back.hours() - ra_hours).abs() < 1e-9,
                "{ra_hours} h came back as {} h",
                back.hours()
            );
        }
        // A target at the local sidereal time is on the meridian, by definition.
        let transiting = astroctl_core::types::RaHours::new(18.5).expect("valid");
        assert!(HourAngle::of(lst, transiting).degrees().abs() < 1e-9);
    }

    #[test]
    fn the_folds_are_half_open_where_they_claim_to_be() {
        for degrees in [
            -540.0,
            -180.000_001,
            -180.0,
            -0.5,
            0.0,
            179.999,
            180.0,
            360.0,
            540.0,
        ] {
            let signed = wrap_signed(degrees);
            assert!((-180.0..180.0).contains(&signed), "{degrees} → {signed}");
            let turn = wrap_turn(degrees);
            assert!((0.0..360.0).contains(&turn), "{degrees} → {turn}");
        }
        assert_eq!(
            wrap_signed(180.0),
            -180.0,
            "+180 and -180 are one direction"
        );
    }

    #[test]
    fn a_non_finite_angle_is_refused_rather_than_folded() {
        // `rem_euclid` of a NaN is a NaN, which then compares false against every limit — so an
        // unchecked fold turns a bad input into a coordinate that passes every safety test.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                Lst::from_degrees(bad),
                Err(MathError::NotFinite { .. })
            ));
            assert!(matches!(
                AxisAngle::from_degrees(bad),
                Err(MathError::NotFinite { .. })
            ));
            assert!(matches!(
                HourAngle::from_degrees(bad),
                Err(MathError::NotFinite { .. })
            ));
        }
    }

    #[test]
    fn a_mount_that_answers_zero_counts_per_revolution_is_refused() {
        assert_eq!(
            AxisScale::new(CountsPerRev(U24::ZERO)),
            Err(MathError::ZeroCountsPerRevolution)
        );
    }

    #[test]
    fn the_fixture_scale_reproduces_the_arcseconds_per_count_the_spike_quotes() {
        // 360° ÷ 9,024,000 = 0.1436″. Every counts-based tolerance in the SDD is quoted in it.
        assert!((scale().arcsec_per_count() - 0.143_617).abs() < 1e-6);
    }
}
