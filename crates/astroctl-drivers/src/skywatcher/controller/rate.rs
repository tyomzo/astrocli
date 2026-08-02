//! Rates, and the step period that produces them.
//!
//! # The one relation this whole module rests on
//!
//! ```text
//! step period = timer frequency ÷ rate in counts per second
//! ```
//!
//! **Provenance, stated the way the spike states it.** `spikes/skywatcher-heq5/ENCODINGS.md`
//! files it under *Lower-confidence items* and calls it "the least-confirmed piece": "The simplest
//! plausible relation is `period = timer_freq / counts_per_second`, giving **620** for sidereal at
//! 64,935 Hz. … Treat 620 as a hypothesis to be tested, not a derived constant."
//!
//! The bring-up then tested it, and the sequence is worth having straight because the two spike
//! documents disagree about the experiment number:
//!
//! * **E5** tried to measure it *inside a bounded goto* and the premise was falsified —
//!   `MOTION-PLAN.md`: "**Executed and disproved.** GOTO ignores the step period — a 10× change
//!   left the rate unchanged at 5,350 counts/s."
//! * **E10** measured it in SLEW mode, which is where `I` actually governs anything: step period
//!   620, 1,863 samples over 30 s, **104.617 counts/s** against a sidereal rate of 104.7304, for
//!   an implied timer frequency of 64,862 — **0.11% from the 64,935 the handshake reported**.
//!
//! So the relation is confirmed, in slew and tracking only, at 0.11%. `ENCODINGS.md` line 86 and
//! `MOTION-PLAN.md` line 34 both name **E8** as the experiment that would settle it; E8 is the
//! `L`-mid-travel test and the step-period work is E5/E10. The IDs in `ENCODINGS.md` are stale;
//! the measurements are not.
//!
//! **What only hardware can still confirm** is the relation away from sidereal. Every rate below
//! other than sidereal is this formula extrapolated: one point on a line is not a slope, and a
//! controller that divided the timer frequency by something other than the count rate — by a worm
//! period, say, which is the form `indi-eqmod`'s `SetRARate` uses — would agree at 620 and
//! disagree everywhere else. E11 (the per-class slew table) is the experiment; it has not been run.
//!
//! # Goto speed is not here, and that is not an omission
//!
//! `I` does not control goto velocity — measured, PRD §4.2 and SDD §5.2.2 both now say so. A goto
//! ramps trapezoidally toward a fixed cruise and the *only* thing a driver chooses is the speed
//! class. There is deliberately no function here that turns a desired goto duration into a step
//! period, because such a function would be wrong and would look right.

use astroctl_core::types::{SlewSpeed, TrackingMode};

use crate::skywatcher::codec::{HighSpeedRatio, SpeedClass, StepPeriod, TimerFrequency, U24_MAX};
use crate::skywatcher::math::{AxisScale, ARCSEC_PER_DEGREE, DEGREES_PER_TURN};

use super::ControllerError;

/// Length of the sidereal day in seconds — the definition, not a measurement.
pub const SIDEREAL_DAY_SECONDS: f64 = 86_164.090_5;

/// The sidereal rate in arcseconds per second: 15.0411″/s.
///
/// The number every other rate here is quoted against, and the same one
/// `simulator::motion::tracking_rate` uses. `tests/position_math.rs` asserts the two agree.
pub const SIDEREAL_ARCSEC_PER_SEC: f64 =
    DEGREES_PER_TURN * ARCSEC_PER_DEGREE / SIDEREAL_DAY_SECONDS;

/// The lunar tracking rate in arcseconds per second.
///
/// **Not measured on this mount.** The spike ran the RA axis at step period 620 and nothing else;
/// `MOTION-PLAN.md` hands "the sidereal/lunar/solar step-period table" to this task with no data
/// behind it. 14.685″/s is the standard mean lunar rate, and it is *slower* than sidereal because
/// the Moon moves east against the stars — a table that made it faster would drift a lunar
/// sequence the wrong way, which is the error that is invisible for ten minutes and obvious in an
/// hour. Same value as `simulator::motion::tracking_rate`, by cross-check rather than by sharing.
pub const LUNAR_ARCSEC_PER_SEC: f64 = 14.685;

/// The solar tracking rate in arcseconds per second — 15.000″/s, likewise standard and likewise
/// unmeasured here. Slower than sidereal, faster than lunar.
pub const SOLAR_ARCSEC_PER_SEC: f64 = 15.0;

/// An axis rate in counts per second: strictly positive and finite.
///
/// A newtype because the step-period expression mixes three quantities that are all "a number
/// about speed" — a frequency in hertz, a rate in counts per second and a period in timer ticks —
/// and transposing two of them yields a plausible register value. M3-T01 separated the two that
/// reach the wire; this separates the one that does not.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct CountsPerSecond(f64);

impl CountsPerSecond {
    /// From a rate in counts per second.
    ///
    /// # Errors
    /// [`ControllerError::BadRate`] for zero, a negative rate or a non-finite one. Direction is
    /// not carried here — it lives in [`MotionDirection`](crate::skywatcher::codec::MotionDirection),
    /// where `G` can read it — so a negative rate is a caller that has put the sign in the wrong
    /// half of the command pair.
    pub fn new(counts_per_second: f64) -> Result<Self, ControllerError> {
        if !counts_per_second.is_finite() || counts_per_second <= 0.0 {
            return Err(ControllerError::BadRate(counts_per_second));
        }
        Ok(Self(counts_per_second))
    }

    /// From a sky rate in arcseconds per second, at a given axis scale.
    ///
    /// # Errors
    /// As [`Self::new`].
    pub fn from_arcsec_per_sec(
        arcsec_per_sec: f64,
        scale: AxisScale,
    ) -> Result<Self, ControllerError> {
        Self::new(arcsec_per_sec / scale.arcsec_per_count())
    }

    /// The rate in counts per second.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// The same rate as a multiple of sidereal, at a given axis scale.
    #[must_use]
    pub fn times_sidereal(self, scale: AxisScale) -> f64 {
        self.0 * scale.arcsec_per_count() / SIDEREAL_ARCSEC_PER_SEC
    }
}

/// How a manual-slew rung moves the mount (E16).
///
/// Split by *mechanism* rather than by rate, because that is the split the hardware imposes: an
/// unbounded slew starts cold and this mount only starts cold up to ~32× sidereal, while a
/// bounded goto is firmware-ramped and cruises at a fixed rate per class. See
/// [`RateModel::slew_method`] for the measurements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlewMethod {
    /// Start an unbounded slew at this rate; the mount holds it until stopped.
    Unbounded(CountsPerSecond),
    /// Chain bounded gotos in this speed class while the command is held; cruise is the
    /// firmware's own (≈51× sidereal low, ≈835× high — measured, and not adjustable: a goto
    /// ignores the step-period register).
    Chunked(SpeedClass),
}

/// A programmed rate: which speed class, and the step period that goes with it.
///
/// The two travel together because they are two halves of one decision. `G`'s speed bit and `I`'s
/// period are sent as separate commands, and a period computed for the low class sent alongside a
/// mode that says high is a **16× speed error** — the ratio the mount reported at `:g`. That is
/// the same failure mode M3-T01's [`MotionMode`](crate::skywatcher::codec::MotionMode) exists to
/// prevent one layer down, and it deserves the same treatment here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProgrammedRate {
    speed: SpeedClass,
    period: StepPeriod,
}

impl ProgrammedRate {
    /// The speed class `G` must carry.
    #[must_use]
    pub const fn speed(self) -> SpeedClass {
        self.speed
    }

    /// The step period `I` must carry.
    #[must_use]
    pub const fn period(self) -> StepPeriod {
        self.period
    }
}

/// The timer frequency, high-speed ratio and axis scale a rate has to be expressed against.
///
/// All three come from the handshake (`:b`, `:g`, `:a`). None is a constant of this driver —
/// PRD §10 records that reading them is what contained the timer-frequency error, and the
/// high-speed ratio is what the crossover below is derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RateModel {
    timer_frequency: u32,
    high_speed_ratio: u8,
    scale: AxisScale,
}

impl RateModel {
    /// From the handshake replies.
    ///
    /// # Errors
    /// [`ControllerError::ZeroTimerFrequency`] or [`ControllerError::ZeroHighSpeedRatio`] — both
    /// are divisors, and a mount that answers zero to either is not answering.
    pub fn new(
        timer_frequency: TimerFrequency,
        high_speed_ratio: HighSpeedRatio,
        scale: AxisScale,
    ) -> Result<Self, ControllerError> {
        let timer_frequency = timer_frequency.0.get();
        if timer_frequency == 0 {
            return Err(ControllerError::ZeroTimerFrequency);
        }
        if high_speed_ratio.0 == 0 {
            return Err(ControllerError::ZeroHighSpeedRatio);
        }
        Ok(Self {
            timer_frequency,
            high_speed_ratio: high_speed_ratio.0,
            scale,
        })
    }

    /// The timer frequency in hertz, as `:b` reported it.
    #[must_use]
    pub const fn timer_frequency(self) -> u32 {
        self.timer_frequency
    }

    /// The high-speed ratio, as `:g` reported it.
    #[must_use]
    pub const fn high_speed_ratio(self) -> u8 {
        self.high_speed_ratio
    }

    /// The axis scale, as `:a` reported it.
    #[must_use]
    pub const fn scale(self) -> AxisScale {
        self.scale
    }

    /// The rate a tracking mode asks of this axis.
    ///
    /// # Errors
    /// [`ControllerError::BadRate`] only if the axis scale is degenerate, which
    /// [`AxisScale`] already refuses to construct.
    pub fn tracking(self, mode: TrackingMode) -> Result<CountsPerSecond, ControllerError> {
        let arcsec_per_sec = match mode {
            TrackingMode::Sidereal => SIDEREAL_ARCSEC_PER_SEC,
            TrackingMode::Lunar => LUNAR_ARCSEC_PER_SEC,
            TrackingMode::Solar => SOLAR_ARCSEC_PER_SEC,
        };
        CountsPerSecond::from_arcsec_per_sec(arcsec_per_sec, self.scale)
    }

    /// How a manual-slew speed class is realised on this axis.
    ///
    /// **Anchored to E11 and E16, both run by ear at the mount.** E11 (2026-08-01): 1× and 8×
    /// turn, 64× ramps up, jams and stops — while the counter reports the steps the rotor never
    /// made, because a Synta counter counts *commanded* steps. E16 (2026-08-02) ran the
    /// discriminating pair and then the workarounds: **32× turns, 39× jams**, a software ramp of
    /// the running axis buzzes at every rung (the protocol refuses rate changes during a
    /// high-speed slew — EQMOD's source states it), and both reference drivers only ever start
    /// high-speed slews cold, which is exactly what this mount refuses. So the standing start is
    /// the wall, and no unbounded ladder gets past it.
    ///
    /// What does get past it: the **bounded goto**, the one primitive whose acceleration lives in
    /// the firmware — measured cruising at ≈835× sidereal in the high class and ≈51× in the low
    /// (and the step-period register is *ignored* by a goto, so those cruise rates are the only
    /// two on offer). The ladder is therefore split by mechanism:
    ///
    /// * **Guide, Slow, Medium — unbounded slews** at 1×, 8×, 32×: every rate a rung commands was
    ///   heard to start from standstill.
    /// * **Fast, Max — [`SlewMethod::Chunked`]**: the driver chains bounded gotos in the low and
    ///   high class respectively while the operator holds the button, and stops with `K` on
    ///   release. Cruise is the firmware's ≈51× / ≈835×.
    ///
    /// The same split as `simulator::motion::slew_rate`, deliberately, so the simulator and the
    /// real mount move at the same speed for the same request; `tests/position_math.rs` asserts
    /// it. (The simulator holds the cruise rate continuously — it does not model the pause
    /// between chunks.)
    ///
    /// # Errors
    /// As [`Self::tracking`], for the unbounded rungs.
    pub fn slew_method(self, speed: SlewSpeed) -> Result<SlewMethod, ControllerError> {
        let unbounded = |times_sidereal: f64| {
            Ok(SlewMethod::Unbounded(CountsPerSecond::from_arcsec_per_sec(
                times_sidereal * SIDEREAL_ARCSEC_PER_SEC,
                self.scale,
            )?))
        };
        match speed {
            SlewSpeed::Guide => unbounded(1.0),
            SlewSpeed::Slow => unbounded(8.0),
            SlewSpeed::Medium => unbounded(32.0),
            SlewSpeed::Fast => Ok(SlewMethod::Chunked(SpeedClass::Low)),
            SlewSpeed::Max => Ok(SlewMethod::Chunked(SpeedClass::High)),
        }
    }

    /// The step period and speed class that produce `rate`.
    ///
    /// # The crossover is a policy, and the policy is the ratio the mount reported
    ///
    /// In the low class `period = f / r`; in the high class the mount issues `k` counts per step
    /// instead of one, so the same rate needs `period = k·f / r`. Both express the rate, and the
    /// two costs point in opposite directions:
    ///
    /// * The **low** class moves one count at a time — 0.1436″ on the operator's mount — but its
    ///   period is `k` times shorter, so rounding it to an integer quantises the *rate* by about
    ///   `1/p`. At a period of 3 that is a 33% rate error.
    /// * The **high** class has `k` times the period and therefore `k` times the rate resolution,
    ///   but it advances the axis in `k`-count jumps: 16 counts, 2.3″, at whatever interval the
    ///   period gives. For tracking or guiding that granularity *is* the error budget; for a slew
    ///   nobody can see it.
    ///
    /// So: low while `f / r ≥ k`, high below it. The threshold is the ratio because that is where
    /// the low class's rate error first reaches `1/k` — the same fraction the high class's jumps
    /// represent — so below it the low class is worse on both counts at once. It is a **policy**
    /// rather than a protocol fact, like the midpoint break point, and unlike that one it has no
    /// measurement behind it: `FINDINGS.md`'s experiment E11, the per-class slew table, was never
    /// run. What it does have is that both sides of it come from the mount's own `:b` and `:g`
    /// rather than from a number in this file.
    ///
    /// The residual, stated: at the mount's *maximum* rate the high-class period is 12 and the
    /// rate error 3%. No class does better there — 800× sidereal is 83,784 counts/s against a
    /// 16 × 64,935 tick/s ceiling — so that is the mount's limit and not this rule's.
    ///
    /// # Errors
    /// [`ControllerError::RateOutOfRange`] if even the high class cannot express the rate — the
    /// period would round to zero (too fast) or overflow the 24-bit register (too slow).
    pub fn program(self, rate: CountsPerSecond) -> Result<ProgrammedRate, ControllerError> {
        let frequency = f64::from(self.timer_frequency);
        let ratio = f64::from(self.high_speed_ratio);
        let low = frequency / rate.get();

        let (speed, exact) = if low >= ratio {
            (SpeedClass::Low, low)
        } else {
            (SpeedClass::High, ratio * low)
        };

        let rounded = exact.round();
        if !(1.0..=f64::from(U24_MAX)).contains(&rounded) {
            return Err(ControllerError::RateOutOfRange {
                counts_per_second: rate.get(),
                period: exact,
            });
        }
        let period = StepPeriod::new(rounded as u32)?;
        Ok(ProgrammedRate { speed, period })
    }

    /// The rate a programmed step period and speed class actually produce.
    ///
    /// The inverse of [`Self::program`] up to the rounding, and the function that says *how much*
    /// rounding cost: a caller that wants to know the tracking error it is about to accept asks
    /// this and compares.
    #[must_use]
    pub fn rate_of(self, programmed: ProgrammedRate) -> f64 {
        let frequency = f64::from(self.timer_frequency);
        let period = f64::from(programmed.period().get());
        match programmed.speed() {
            SpeedClass::Low => frequency / period,
            SpeedClass::High => frequency * f64::from(self.high_speed_ratio) / period,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::hex::U24;
    use crate::skywatcher::codec::CountsPerRev;

    /// **Fixtures**, all three read from the operator's own HEQ5 on 2026-07-29 and recorded in
    /// PRD §4.2. They are here because the acceptance criterion is "step periods … match
    /// hand-computed values for the **verified** fixture constants", and nowhere else.
    const FIXTURE_CPR: u32 = 9_024_000;
    const FIXTURE_TIMER_HZ: u32 = 64_935;
    const FIXTURE_HIGH_SPEED_RATIO: u8 = 16;

    fn model() -> RateModel {
        RateModel::new(
            TimerFrequency(U24::new(FIXTURE_TIMER_HZ).expect("fits")),
            HighSpeedRatio(FIXTURE_HIGH_SPEED_RATIO),
            AxisScale::new(CountsPerRev(U24::new(FIXTURE_CPR).expect("fits"))).expect("non-zero"),
        )
        .expect("the fixture constants are all non-zero")
    }

    #[test]
    fn the_sidereal_step_period_is_the_620_the_mount_was_measured_at() {
        // The acceptance criterion, and the single most load-bearing assertion in this module.
        //
        // Hand-computed: 9,024,000 counts ÷ 86,164.0905 s = 104.7304 counts/s. 64,935 ÷ 104.7304
        // = 620.02, which rounds to 620 — exactly the constant E10 drove the mount at and
        // measured 104.617 counts/s from. Deriving this against the old 460,800 Hz figure would
        // give 4,400 and predict 743 counts/s; PRD §4.2 says any such value is invalid.
        let model = model();
        let sidereal = model.tracking(TrackingMode::Sidereal).expect("valid");
        assert!(
            (sidereal.get() - 104.730_4).abs() < 1e-3,
            "sidereal is {} counts/s",
            sidereal.get()
        );

        let programmed = model.program(sidereal).expect("in range");
        assert_eq!(programmed.period(), StepPeriod::SIDEREAL_AT_64935_HZ);
        assert_eq!(programmed.period().get(), 620);
        assert_eq!(
            programmed.speed(),
            SpeedClass::Low,
            "tracking is a low-speed slew — `motion.py` sent mode digit `10` for it"
        );
    }

    #[test]
    fn the_lunar_and_solar_step_periods_are_the_hand_computed_ones() {
        // 0.143617″ per count at the fixture CPR.
        //   lunar   14.685 ÷ 0.143617 = 102.2511 counts/s → 64,935 ÷ 102.2511 = 635.05 → 635
        //   solar   15.000 ÷ 0.143617 = 104.4444 counts/s → 64,935 ÷ 104.4444 = 621.72 → 622
        let model = model();
        for (mode, counts_per_second, period) in [
            (TrackingMode::Lunar, 102.2511, 635_u32),
            (TrackingMode::Solar, 104.4444, 622),
        ] {
            let rate = model.tracking(mode).expect("valid");
            assert!(
                (rate.get() - counts_per_second).abs() < 1e-3,
                "{mode:?} is {} counts/s, expected {counts_per_second}",
                rate.get()
            );
            let programmed = model.program(rate).expect("in range");
            assert_eq!(programmed.period().get(), period, "{mode:?}");
            assert_eq!(programmed.speed(), SpeedClass::Low, "{mode:?}");
        }
    }

    #[test]
    fn the_tracking_rates_are_ordered_the_way_the_sky_is() {
        // Lunar and solar are *slower* than sidereal, so their step periods are *longer*. Getting
        // the sign wrong drifts a lunar sequence east instead of west, which looks like poor
        // polar alignment for the first ten minutes.
        let model = model();
        let rate = |mode| model.tracking(mode).expect("valid").get();
        assert!(rate(TrackingMode::Lunar) < rate(TrackingMode::Solar));
        assert!(rate(TrackingMode::Solar) < rate(TrackingMode::Sidereal));

        let period = |mode| {
            model
                .program(model.tracking(mode).expect("valid"))
                .expect("in range")
                .period()
                .get()
        };
        assert!(period(TrackingMode::Sidereal) < period(TrackingMode::Solar));
        assert!(period(TrackingMode::Solar) < period(TrackingMode::Lunar));
    }

    /// The rate an unbounded rung commands, or a panic for a chunked one.
    fn unbounded(model: RateModel, speed: SlewSpeed) -> CountsPerSecond {
        match model.slew_method(speed).expect("valid") {
            SlewMethod::Unbounded(rate) => rate,
            SlewMethod::Chunked(class) => panic!("{speed:?} is chunked ({class:?}), not a rate"),
        }
    }

    #[test]
    fn the_unbounded_rungs_land_on_hand_computed_periods() {
        // Every number below is `rate = n × 104.7304 counts/s`, then `64,935 ÷ rate`, rounded —
        // worked out here so the code has to agree with arithmetic done outside it rather than
        // with itself. All three rungs keep the smooth one-count class, and all three are rates
        // the mount was heard to start from standstill (E11: 1× and 8×; E16: 32×).
        let model = model();
        for (speed, class, period) in [
            (SlewSpeed::Guide, SpeedClass::Low, 620_u32), //   104.730 c/s → 620.02
            (SlewSpeed::Slow, SpeedClass::Low, 78),       //   837.844 c/s →  77.50
            (SlewSpeed::Medium, SpeedClass::Low, 19),     // 3,351.37 c/s →  19.38
        ] {
            let programmed = model
                .program(unbounded(model, speed))
                .expect("in range");
            assert_eq!(programmed.speed(), class, "{speed:?}");
            assert_eq!(programmed.period().get(), period, "{speed:?}");
        }
    }

    #[test]
    fn the_crossover_is_where_the_low_class_stops_being_the_better_of_the_two() {
        // The rule `program` documents, checked at its own boundary rather than through the
        // ladder: a rate whose low-class period is just above the ratio keeps the low class, and
        // one just below it switches. Both sides come from `:b` and `:g`, so a mount reporting
        // different values moves the boundary rather than breaking it.
        let model = model();
        let ratio = f64::from(FIXTURE_HIGH_SPEED_RATIO);
        let frequency = f64::from(FIXTURE_TIMER_HZ);

        let just_slow_enough = CountsPerSecond::new(frequency / (ratio + 0.5)).expect("positive");
        let just_too_fast = CountsPerSecond::new(frequency / (ratio - 0.5)).expect("positive");
        assert_eq!(
            model.program(just_slow_enough).expect("in range").speed(),
            SpeedClass::Low
        );
        assert_eq!(
            model.program(just_too_fast).expect("in range").speed(),
            SpeedClass::High
        );

        // Guide speed is one times sidereal, so it must be the tracking period exactly — the one
        // point on the ladder that a measurement stands behind.
        assert_eq!(
            model
                .program(unbounded(model, SlewSpeed::Guide))
                .expect("in range")
                .period()
                .get(),
            620
        );
    }

    #[test]
    fn the_ladder_splits_by_mechanism_at_the_standing_start_limit() {
        // E16's measurement, as the ladder: the unbounded rungs climb 1× → 8× → 32× — every one
        // a rate this mount was heard to start — and the rungs above the standing-start limit are
        // chunked gotos, low class then high, because the firmware's goto ramp is the only
        // working acceleration. The chunked rungs carry no rate on purpose: a goto ignores the
        // step-period register, so the cruise is the firmware's and pretending otherwise would be
        // a number with nothing behind it.
        let model = model();
        let slow = [SlewSpeed::Guide, SlewSpeed::Slow, SlewSpeed::Medium];
        for pair in slow.windows(2) {
            assert!(
                unbounded(model, pair[0]).get() < unbounded(model, pair[1]).get(),
                "{pair:?}"
            );
        }
        // 32× exactly: the fastest standing start E16 heard succeed (39× jammed).
        let top = unbounded(model, SlewSpeed::Medium).times_sidereal(model.scale());
        assert!((top - 32.0).abs() < 1e-9, "medium is {top}× sidereal");
        for (speed, class) in [
            (SlewSpeed::Fast, SpeedClass::Low),
            (SlewSpeed::Max, SpeedClass::High),
        ] {
            assert_eq!(
                model.slew_method(speed).expect("valid"),
                SlewMethod::Chunked(class),
                "{speed:?}"
            );
        }
    }

    #[test]
    fn programming_a_rate_and_reading_it_back_agrees_to_the_quantisation_the_crossover_promises() {
        // The claim in `program`'s documentation, checked rather than asserted: the worst rate
        // error over the whole usable range is one part in the high-speed ratio.
        let model = model();
        let mut worst: f64 = 0.0;
        let mut rate = 1.0_f64;
        while rate < 100_000.0 {
            let counts = CountsPerSecond::new(rate).expect("positive");
            let programmed = model.program(counts).expect("in range");
            let actual = model.rate_of(programmed);
            worst = worst.max((actual - rate).abs() / rate);
            rate *= 1.05;
        }
        let budget = 1.0 / f64::from(FIXTURE_HIGH_SPEED_RATIO);
        assert!(
            worst < budget,
            "worst rate error was {:.3}%, budget {:.3}%",
            worst * 100.0,
            budget * 100.0
        );
    }

    #[test]
    fn a_rate_the_register_cannot_express_is_refused_at_both_ends() {
        let model = model();
        // Faster than the timer can step even in the high class: 16 × 64,935 ticks/s is the most
        // the mount can issue, so anything past it has no period at all.
        let too_fast = CountsPerSecond::new(20_000_000.0).expect("positive");
        assert!(matches!(
            model.program(too_fast),
            Err(ControllerError::RateOutOfRange { .. })
        ));
        // Slower than one count per 16.7 million ticks — about one count every four minutes.
        let too_slow = CountsPerSecond::new(1e-9).expect("positive");
        assert!(matches!(
            model.program(too_slow),
            Err(ControllerError::RateOutOfRange { .. })
        ));
    }

    #[test]
    fn a_rate_that_is_not_a_rate_is_refused_before_it_reaches_the_arithmetic() {
        for bad in [0.0, -1.0, -104.73, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(CountsPerSecond::new(bad), Err(ControllerError::BadRate(_))),
                "{bad} is not an axis rate"
            );
        }
        // A negative rate in particular: the sign belongs in `G`, and a caller that put it here
        // would otherwise get a negative period, round it to zero and program the fastest step
        // the mount can take.
        assert!(matches!(
            CountsPerSecond::new(-104.73),
            Err(ControllerError::BadRate(_))
        ));
    }

    #[test]
    fn a_mount_that_answers_zero_to_the_handshake_divisors_is_refused() {
        let scale =
            AxisScale::new(CountsPerRev(U24::new(FIXTURE_CPR).expect("fits"))).expect("non-zero");
        assert_eq!(
            RateModel::new(TimerFrequency(U24::ZERO), HighSpeedRatio(16), scale),
            Err(ControllerError::ZeroTimerFrequency)
        );
        assert_eq!(
            RateModel::new(
                TimerFrequency(U24::new(FIXTURE_TIMER_HZ).expect("fits")),
                HighSpeedRatio(0),
                scale
            ),
            Err(ControllerError::ZeroHighSpeedRatio)
        );
    }

    #[test]
    fn the_period_scales_with_the_timer_frequency_the_mount_reports_and_not_with_a_constant() {
        // The property that made the timer-frequency correction survivable: nothing here knows
        // 64,935. A mount reporting twice the frequency gets twice the period for the same rate,
        // and the *rate* — which is what the sky cares about — is unchanged.
        let scale =
            AxisScale::new(CountsPerRev(U24::new(FIXTURE_CPR).expect("fits"))).expect("non-zero");
        let doubled = RateModel::new(
            TimerFrequency(U24::new(FIXTURE_TIMER_HZ * 2).expect("fits")),
            HighSpeedRatio(FIXTURE_HIGH_SPEED_RATIO),
            scale,
        )
        .expect("valid");
        let sidereal = doubled.tracking(TrackingMode::Sidereal).expect("valid");
        let programmed = doubled.program(sidereal).expect("in range");
        assert_eq!(programmed.period().get(), 1_240, "620 × 2");
        assert!((doubled.rate_of(programmed) - 104.730_4).abs() < 0.02);

        // ...and the old, wrong 460,800 figure would have produced this, which is the number
        // PRD §4.2 says every fixture built on it is invalid for.
        let wrong = RateModel::new(
            TimerFrequency(U24::new(460_800).expect("fits")),
            HighSpeedRatio(FIXTURE_HIGH_SPEED_RATIO),
            scale,
        )
        .expect("valid");
        let period = wrong
            .program(wrong.tracking(TrackingMode::Sidereal).expect("valid"))
            .expect("in range")
            .period()
            .get();
        assert_eq!(period, 4_400);
        assert_ne!(
            period, 620,
            "the driver must follow the mount, not a constant"
        );
    }
}
