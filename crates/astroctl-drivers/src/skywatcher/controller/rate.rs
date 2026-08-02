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

    /// The rate a manual-slew speed class asks of this axis.
    ///
    /// **Anchored to E11, run by ear on 2026-08-01 and corrected here on 2026-08-02.** The
    /// previous ladder was EQMOD's — 1×, 8×, 64×, 400×, 800× — and this mount does not do it.
    /// Standing at a bare HEQ5 Pro with the clutches engaged, the operator heard 1× and 8× turn
    /// the rotor and **64× ramp up, jam and stop**. The counter reported 6,993 counts/s throughout
    /// the stall, because a Synta counter counts *commanded* steps, so nothing in this system
    /// could see it. Three of the five classes were therefore promising motion the mount does not
    /// produce, and the top two by a factor of fifty.
    ///
    /// The new ladder stops below the **speed-class crossover**, and that is not a coincidence —
    /// it is the best available hypothesis for what E11 actually heard. On this mount the crossover
    /// sits at `f/r = 16`, i.e. 4,058 counts/s or **38.8× sidereal**. Every rate that was heard to
    /// turn is below it and programs in the low class; the one that stalled is above it and
    /// programs in the high class, where the axis advances in sixteen-count jumps instead of single
    /// counts. So E11 may not have found a rate ceiling at all: it may have found that this mount
    /// will not start an unbounded slew in the high class. The corrected ladder keeps all five
    /// classes in the low one.
    ///
    /// # What this is not, and the experiment that would settle it
    ///
    /// It is **not** a claim that the mount cannot slew fast: a *goto* was measured cruising at
    /// 835× sidereal on the same hardware, and a goto uses the high class. So the high class works
    /// for a bounded, ramped move and failed for an unbounded one, which is why the hypothesis
    /// above is about the *start* rather than about the rate.
    ///
    /// `SlewSpeed::Max` therefore no longer means "the mount's maximum rate" — PRD §4.2's 800× and
    /// `MountCapabilities::max_slew_speed_x_sidereal` still report the mount's capability, which
    /// goto reaches. It means the fastest rate this driver will start an unbounded slew at. The
    /// two were the same number while the ladder was EQMOD's and nobody had listened to the mount.
    ///
    /// Two experiments would replace the hypothesis with a measurement, both ten minutes by ear:
    /// drive 32× and then 39× (either side of the crossover) and hear whether the stall follows
    /// the *class* rather than the rate; and if it does, try a high-class rate started from a
    /// ramped rather than unbounded profile.
    ///
    /// The same ladder as `simulator::motion::slew_rate`, deliberately, so the simulator and the
    /// real mount move at the same speed for the same request; `tests/position_math.rs` asserts
    /// it.
    ///
    /// # Errors
    /// As [`Self::tracking`].
    pub fn slew(self, speed: SlewSpeed) -> Result<CountsPerSecond, ControllerError> {
        let times_sidereal = match speed {
            SlewSpeed::Guide => 1.0,
            SlewSpeed::Slow => 8.0,
            SlewSpeed::Medium => 16.0,
            SlewSpeed::Fast => 24.0,
            SlewSpeed::Max => 32.0,
        };
        CountsPerSecond::from_arcsec_per_sec(times_sidereal * SIDEREAL_ARCSEC_PER_SEC, self.scale)
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

    #[test]
    fn the_whole_slew_ladder_lands_on_hand_computed_periods() {
        // Guide and slow keep the smooth one-count class; medium and above cannot, because their
        // low-class period would be under the ratio. Every number below is
        // `rate = n × 104.7304 counts/s`, then `64,935 ÷ rate` (low) or `16 × 64,935 ÷ rate`
        // (high), rounded — worked out here so the code has to agree with arithmetic done
        // outside it rather than with itself.
        let model = model();
        for (speed, class, period) in [
            (SlewSpeed::Guide, SpeedClass::Low, 620_u32), //   104.730 c/s → 620.02
            (SlewSpeed::Slow, SpeedClass::Low, 78),       //   837.844 c/s →  77.50
            // All five are in the low class after the E11 correction, deliberately: the crossover
            // at 38.8× is the suspected stall boundary, so the ladder stops below it.
            (SlewSpeed::Medium, SpeedClass::Low, 39), // 1,675.68  c/s →  38.75
            (SlewSpeed::Fast, SpeedClass::Low, 26),   // 2,513.52  c/s →  25.83
            (SlewSpeed::Max, SpeedClass::Low, 19),    // 3,351.36  c/s →  19.38
        ] {
            let programmed = model
                .program(model.slew(speed).expect("valid"))
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
                .program(model.slew(SlewSpeed::Guide).expect("valid"))
                .expect("in range")
                .period()
                .get(),
            620
        );
    }

    #[test]
    fn the_slew_ladder_climbs_to_the_stated_maximum() {
        let model = model();
        let ladder = [
            SlewSpeed::Guide,
            SlewSpeed::Slow,
            SlewSpeed::Medium,
            SlewSpeed::Fast,
            SlewSpeed::Max,
        ];
        for pair in ladder.windows(2) {
            let slower = model.slew(pair[0]).expect("valid").get();
            let faster = model.slew(pair[1]).expect("valid").get();
            assert!(slower < faster, "{pair:?}");
        }
        // 32×, and deliberately *not* PRD §4.2's 800×: that figure is the mount's capability and
        // a goto reaches it, while this ladder is what the driver will start an unbounded slew at.
        // E11 heard 64× stall. `MountCapabilities::max_slew_speed_x_sidereal` still reports 800.
        let ratio = model
            .slew(SlewSpeed::Max)
            .expect("valid")
            .times_sidereal(model.scale());
        assert!((ratio - 32.0).abs() < 1e-9, "max is {ratio}× sidereal");
        // And every class stays below the crossover, which is the point of the correction.
        for speed in [
            SlewSpeed::Guide,
            SlewSpeed::Slow,
            SlewSpeed::Medium,
            SlewSpeed::Fast,
            SlewSpeed::Max,
        ] {
            let programmed = model
                .program(model.slew(speed).expect("valid"))
                .expect("in range");
            assert_eq!(
                programmed.speed(),
                SpeedClass::Low,
                "{speed:?} crossed into the high class, which is what E11 heard stall"
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
