//! Axis kinematics: what the simulated mount's two axes are doing at any instant.
//!
//! # Why this is a plan and not a tick
//!
//! The obvious simulator advances a position on a timer. This one does not: an axis holds a
//! *plan* — where it started, the velocity segments it will run through, and the rate it holds
//! afterwards — and any read integrates that plan forward to the caller's instant. Three things
//! follow, and each is a requirement rather than a preference:
//!
//! 1. **An e-stop lands exactly when it is issued.** SDD §5.8.2 gives the safety path a 20 ms
//!    budget and M1-T05 tests the safety wrapper against this driver. A ticking model stops at
//!    the next tick boundary, so the tick interval becomes a floor under the measured e-stop
//!    latency — a number invented by the simulator, not by the system under test.
//! 2. **Dropping a `goto` future cannot corrupt anything.** HAL rule 3 says a dropped future
//!    leaves the mount slewing. Here the plan *is* the motion: it is installed before the first
//!    await, it completes without anyone waiting for it, and the future that returns `Ok(())`
//!    mutates nothing. There is no in-flight flag to leak.
//! 3. **Position is continuous at any sample rate.** The acceptance criterion samples at 10 Hz
//!    and forbids jumps; an integrated plan is continuous at 10 kHz too.
//!
//! Everything here is synchronous, allocation-light and has no idea what a `tokio` is. The
//! device layer owns the clock; this module owns the arithmetic.

use std::f64::consts::TAU;

use astroctl_core::types::{SlewSpeed, TrackingMode};

use super::profile::{SimulatorProfile, SIDEREAL_DEG_PER_SEC};

// -----------------------------------------------------------------------------------------
// Rates
// -----------------------------------------------------------------------------------------

/// Sidereal rate in arcseconds per second — 15.041″/s, the number every tracking rate below is
/// quoted against.
const SIDEREAL_ARCSEC_PER_SEC: f64 = SIDEREAL_DEG_PER_SEC * 3600.0;

/// Axis rate for a tracking mode, in degrees per second.
///
/// Sidereal is the definition. Lunar (14.685″/s) and solar (15.000″/s) are the standard rates —
/// the Moon and Sun move east against the stars, so both are *slower* than sidereal, and a
/// simulator that made them faster would put the drift in the wrong direction for anyone
/// eyeballing a lunar sequence. Neither was measured on the HEQ5: the spike only ever ran the
/// RA axis at step period 620, which is sidereal.
#[must_use]
pub fn tracking_rate(mode: TrackingMode) -> f64 {
    let arcsec_per_sec = match mode {
        TrackingMode::Sidereal => SIDEREAL_ARCSEC_PER_SEC,
        TrackingMode::Lunar => 14.685,
        TrackingMode::Solar => 15.0,
    };
    arcsec_per_sec / 3600.0
}

/// Axis rate for a manual-slew speed class, in degrees per second.
///
/// **Not measured.** FINDINGS.md's experiment E11 — the per-class slew table — was never run,
/// so this ladder is EQMOD's conventional one. Only its top matches hardware: `SlewSpeed::Max`
/// is PRD §4.2's stated 800× maximum, against a measured goto cruise of 835×. If E11 is ever
/// run, this function is the one place that changes.
#[must_use]
pub fn slew_rate(speed: SlewSpeed) -> f64 {
    let times_sidereal = match speed {
        SlewSpeed::Guide => 1.0,
        SlewSpeed::Slow => 8.0,
        SlewSpeed::Medium => 64.0,
        SlewSpeed::Fast => 400.0,
        SlewSpeed::Max => 800.0,
    };
    times_sidereal * SIDEREAL_DEG_PER_SEC
}

// -----------------------------------------------------------------------------------------
// Velocity segments
// -----------------------------------------------------------------------------------------

/// A stretch of motion over which velocity changes linearly from `v0` to `v1`.
///
/// Signed: a negative velocity is motion the other way, and a plan's segments all share a sign,
/// which is what makes a slew monotonic.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Segment {
    seconds: f64,
    v0: f64,
    v1: f64,
}

impl Segment {
    /// Distance covered `t` seconds in, `0 <= t <= seconds`.
    fn distance_at(&self, t: f64) -> f64 {
        if self.seconds <= 0.0 {
            return 0.0;
        }
        let accel = (self.v1 - self.v0) / self.seconds;
        self.v0 * t + 0.5 * accel * t * t
    }

    /// Distance covered by the whole segment.
    fn distance(&self) -> f64 {
        self.distance_at(self.seconds)
    }
}

/// A trapezoidal (or, over a short distance, triangular) move: accelerate, cruise, decelerate.
///
/// The triangular case is not a special case bolted on — it is what the HEQ5 does whenever the
/// break point (`:M`, half the goto increment) arrives before cruise, which for a 1,000-count
/// move meant a peak of 5,350 counts/s against a cruise rate of 87,486. Goto duration is
/// therefore *not* linear in distance, and SDD §5.2.3 forbids estimating it as distance ÷ rate.
#[derive(Debug, Clone, PartialEq)]
pub struct TrapezoidProfile {
    segments: Vec<Segment>,
    distance: f64,
}

impl TrapezoidProfile {
    /// Plans a move of `distance` degrees (signed) at up to `cruise` deg/s.
    ///
    /// # Panics
    /// Never — non-finite or non-positive rates are clamped to something finite, because a NaN
    /// duration reaching the device layer becomes a slew that never ends and a test that hangs
    /// rather than fails.
    #[must_use]
    pub fn plan(distance: f64, cruise: f64, accel: f64, decel: f64) -> Self {
        let accel = positive_or(accel, 1.0);
        let decel = positive_or(decel, 1.0);
        let cruise = positive_or(cruise, 1.0);
        let magnitude = distance.abs();
        if !magnitude.is_finite() || magnitude == 0.0 {
            return Self {
                segments: Vec::new(),
                distance: 0.0,
            };
        }
        let sign = distance.signum();

        // Distance needed to reach `cruise` and come back down. If the move is shorter than
        // that, the peak is wherever the two ramps meet — the break point arriving early.
        let ramp_distance = cruise * cruise / (2.0 * accel) + cruise * cruise / (2.0 * decel);
        let peak = if ramp_distance <= magnitude {
            cruise
        } else {
            // v² / 2a + v² / 2d = distance, solved for v.
            (2.0 * magnitude / (1.0 / accel + 1.0 / decel)).sqrt()
        };

        let mut segments = Vec::with_capacity(3);
        segments.push(Segment {
            seconds: peak / accel,
            v0: 0.0,
            v1: sign * peak,
        });
        if ramp_distance <= magnitude {
            let cruise_distance = magnitude - ramp_distance;
            segments.push(Segment {
                seconds: cruise_distance / peak,
                v0: sign * peak,
                v1: sign * peak,
            });
        }
        segments.push(Segment {
            seconds: peak / decel,
            v0: sign * peak,
            v1: 0.0,
        });

        Self {
            segments,
            distance: sign * magnitude,
        }
    }

    /// Seconds of motion.
    #[must_use]
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.seconds).sum()
    }

    /// Signed displacement the whole move covers.
    #[must_use]
    pub fn distance(&self) -> f64 {
        self.distance
    }

    /// Displacement `t` seconds in, saturating at [`distance`](Self::distance) once the move is
    /// over.
    #[must_use]
    fn displacement_at(&self, t: f64) -> f64 {
        let mut covered = 0.0;
        let mut remaining = t.max(0.0);
        for segment in &self.segments {
            if remaining >= segment.seconds {
                covered += segment.distance();
                remaining -= segment.seconds;
            } else {
                return covered + segment.distance_at(remaining);
            }
        }
        // Floating-point summation of the segments lands a hair off the requested distance;
        // returning the requested value is what makes a goto land *on* the target rather than
        // within an arcsecond of it, which the SDD's 10-count tolerance would forgive but a
        // reader comparing two printed coordinates would not.
        self.distance
    }

    /// Velocity `t` seconds in; zero once the move is over.
    fn velocity_at(&self, t: f64) -> f64 {
        let mut remaining = t.max(0.0);
        for segment in &self.segments {
            if remaining >= segment.seconds {
                remaining -= segment.seconds;
            } else if segment.seconds <= 0.0 {
                return segment.v0;
            } else {
                let progress = remaining / segment.seconds;
                return segment.v0 + (segment.v1 - segment.v0) * progress;
            }
        }
        0.0
    }

    /// A single flat stretch at `rate` — a guide pulse, which changes rate rather than ramping.
    #[must_use]
    fn constant(rate: f64, seconds: f64) -> Self {
        let seconds = seconds.max(0.0);
        Self {
            segments: vec![Segment {
                seconds,
                v0: rate,
                v1: rate,
            }],
            distance: rate * seconds,
        }
    }

    /// A ramp from `v0` up to nothing — deceleration to a stop at `decel`.
    #[must_use]
    fn ramp_down(v0: f64, decel: f64) -> Self {
        let decel = positive_or(decel, 1.0);
        let seconds = v0.abs() / decel;
        Self {
            segments: vec![Segment {
                seconds,
                v0,
                v1: 0.0,
            }],
            distance: 0.5 * v0 * seconds,
        }
    }

    /// A ramp from `from` to `to`, which is then held by the plan's terminal rate.
    ///
    /// `from` is rarely zero in practice: an operator moving the speed slider while holding the
    /// D-pad re-issues the slew at a new rate, and ramping from a standstill there would mean
    /// the mount stopping dead and starting again on every speed change.
    #[must_use]
    fn ramp_between(from: f64, to: f64, accel: f64) -> Self {
        let accel = positive_or(accel, 1.0);
        let seconds = (to - from).abs() / accel;
        Self {
            segments: vec![Segment {
                seconds,
                v0: from,
                v1: to,
            }],
            distance: f64::midpoint(from, to) * seconds,
        }
    }

    /// Appends a dwell so the move occupies at least `seconds`, without changing where it ends.
    ///
    /// This is how a goto's two axes finish together. A real driver declares a goto complete when
    /// *both* axes report stopped and only then restores tracking (SDD §5.2.3), so the axis with
    /// the shorter move waits rather than resuming tracking early — and an axis that resumed
    /// early would hold the wrong right ascension, since its target was computed for the arrival
    /// of the slower one.
    #[must_use]
    fn padded_to(mut self, seconds: f64) -> Self {
        let extra = seconds - self.duration();
        if extra > 0.0 {
            self.segments.push(Segment {
                seconds: extra,
                v0: 0.0,
                v1: 0.0,
            });
        }
        self
    }

    /// Truncates the move at `fraction` of its duration and drops the rest — the mechanical
    /// stall of [`Fault::StallDuringSlew`](super::fault::Fault::StallDuringSlew).
    ///
    /// The axis stops where it stopped: no deceleration ramp, because a jammed worm does not
    /// decelerate politely.
    #[must_use]
    pub fn truncated(&self, fraction: f64) -> Self {
        let cut = self.duration() * fraction.clamp(0.0, 1.0);
        let distance = self.displacement_at(cut);
        let mut segments = Vec::with_capacity(self.segments.len());
        let mut remaining = cut;
        for segment in &self.segments {
            if remaining <= 0.0 {
                break;
            }
            if remaining >= segment.seconds {
                segments.push(*segment);
                remaining -= segment.seconds;
            } else {
                segments.push(Segment {
                    seconds: remaining,
                    v0: segment.v0,
                    v1: segment.v0 + (segment.v1 - segment.v0) * remaining / segment.seconds,
                });
                break;
            }
        }
        Self { segments, distance }
    }
}

/// Clamps a rate to something usable, so bad configuration produces a slow mount rather than a
/// NaN one.
fn positive_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

// -----------------------------------------------------------------------------------------
// Settle
// -----------------------------------------------------------------------------------------

/// The damped ring-down after motion stops (PRD §4.5 "realistic settle times").
///
/// The envelope tapers linearly to exactly zero at the end of the interval as well as decaying
/// exponentially. The taper is not cosmetic: without it the offset is still ~2% of the amplitude
/// when the interval ends and the position *steps* to its final value, which is precisely the
/// discontinuity the acceptance criterion forbids — and the kind of artefact a guiding loop
/// would later chase.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Settle {
    seconds: f64,
    amplitude: f64,
    frequency_hz: f64,
}

impl Settle {
    fn offset_at(&self, t: f64) -> f64 {
        if t <= 0.0 || t >= self.seconds || self.amplitude == 0.0 {
            return 0.0;
        }
        let progress = t / self.seconds;
        let envelope = (1.0 - progress) * (-3.0 * progress).exp();
        self.amplitude * envelope * (TAU * self.frequency_hz * t).sin()
    }
}

// -----------------------------------------------------------------------------------------
// Axis plans
// -----------------------------------------------------------------------------------------

/// Everything one axis will do from the instant the plan was installed.
///
/// Self-contained on purpose: once installed, no further action is needed for the axis to reach
/// its target, settle and resume tracking. That is what makes a dropped `goto` future harmless.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisPlan {
    /// Axis angle in degrees when the plan started.
    start: f64,
    /// The move, if this plan moves at all.
    move_profile: Option<TrapezoidProfile>,
    /// Ring-down after the move.
    settle: Option<Settle>,
    /// Rate held once the move is done — the tracking rate SES-06 restores, or zero.
    terminal_rate: f64,
}

impl AxisPlan {
    /// An axis holding still, or turning forever at `rate` (tracking).
    #[must_use]
    pub fn steady(start: f64, rate: f64) -> Self {
        Self {
            start,
            move_profile: None,
            settle: None,
            terminal_rate: rate,
        }
    }

    /// A move of `distance` degrees, then `settle_seconds` of ring-down, then `terminal_rate`.
    #[must_use]
    pub fn moving(
        start: f64,
        distance: f64,
        profile: &SimulatorProfile,
        cruise: f64,
        settle_seconds: f64,
        terminal_rate: f64,
    ) -> Self {
        let move_profile = TrapezoidProfile::plan(
            distance,
            cruise,
            profile.accel_deg_per_sec2,
            profile.decel_deg_per_sec2,
        );
        Self {
            start,
            settle: (settle_seconds > 0.0 && move_profile.duration() > 0.0).then_some(Settle {
                seconds: settle_seconds,
                amplitude: profile.settle_amplitude_arcsec / 3600.0,
                frequency_hz: profile.settle_frequency_hz,
            }),
            move_profile: Some(move_profile),
            terminal_rate,
        }
    }

    /// An axis ramping from `from_rate` to `rate` and holding it until something stops it — a
    /// manual slew, or a change of speed part-way through one.
    #[must_use]
    pub fn accelerating(start: f64, from_rate: f64, rate: f64, accel: f64) -> Self {
        Self {
            start,
            move_profile: Some(TrapezoidProfile::ramp_between(from_rate, rate, accel)),
            settle: None,
            terminal_rate: rate,
        }
    }

    /// The same plan, but not finished moving until `motion_seconds` have passed — how a goto
    /// keeps its two axes together. See [`TrapezoidProfile::padded_to`].
    #[must_use]
    pub fn synchronised_to(mut self, motion_seconds: f64) -> Self {
        self.move_profile = self.move_profile.map(|m| m.padded_to(motion_seconds));
        self
    }

    /// An axis running at `rate` for `seconds`, then holding `terminal_rate` — a guide pulse.
    #[must_use]
    pub fn pulsed(start: f64, rate: f64, seconds: f64, terminal_rate: f64) -> Self {
        Self {
            start,
            move_profile: Some(TrapezoidProfile::constant(rate, seconds)),
            settle: None,
            terminal_rate,
        }
    }

    /// The same plan with its move cut short and never resumed — a stalled axis.
    #[must_use]
    pub fn stalled(&self, fraction: f64) -> Self {
        Self {
            start: self.start,
            move_profile: self.move_profile.as_ref().map(|m| m.truncated(fraction)),
            settle: None,
            terminal_rate: 0.0,
        }
    }

    /// Axis angle `elapsed` seconds after the plan started.
    #[must_use]
    pub fn position_at(&self, elapsed: f64) -> f64 {
        let elapsed = elapsed.max(0.0);
        let Some(motion) = &self.move_profile else {
            return self.start + self.terminal_rate * elapsed;
        };
        let motion_time = motion.duration();
        if elapsed < motion_time {
            return self.start + motion.displacement_at(elapsed);
        }
        let after = elapsed - motion_time;
        let settle_offset = self.settle.map_or(0.0, |s| s.offset_at(after));
        self.start + motion.distance() + self.terminal_rate * after + settle_offset
    }

    /// Seconds until the axis has stopped moving *and* stopped ringing — when a caller may
    /// start an exposure (SDD §5.2.3, SES-06).
    #[must_use]
    pub fn settled_after(&self) -> f64 {
        let motion = self
            .move_profile
            .as_ref()
            .map_or(0.0, TrapezoidProfile::duration);
        motion + self.settle.map_or(0.0, |s| s.seconds)
    }

    /// Seconds until the axis has stopped moving, ring-down excluded.
    #[must_use]
    pub fn motion_after(&self) -> f64 {
        self.move_profile
            .as_ref()
            .map_or(0.0, TrapezoidProfile::duration)
    }

    /// The rate held once the move is over.
    #[must_use]
    pub fn terminal_rate(&self) -> f64 {
        self.terminal_rate
    }

    /// Freezes the axis where it is at `elapsed` and holds `rate` from there.
    ///
    /// This is the instant-stop path — `emergency_stop`, Synta `L`. Sampling the current
    /// position first is what makes a stop land at the instant it was issued rather than at a
    /// tick boundary; the plan it replaces is discarded, not run down.
    #[must_use]
    pub fn frozen_at(&self, elapsed: f64, rate: f64) -> Self {
        Self::steady(self.position_at(elapsed), rate)
    }

    /// Decelerates from whatever the axis is doing at `elapsed` to a stop, then holds `rate` —
    /// Synta `K`.
    ///
    /// The difference from [`frozen_at`](Self::frozen_at) is invisible at guide speed, where the
    /// spike measured `K` and `L` one count apart. At 800× sidereal it is nearly three seconds
    /// of extra travel, which is why the HAL has two calls and not one with a flag.
    #[must_use]
    pub fn ramped_stop(&self, elapsed: f64, decel: f64, rate: f64) -> Self {
        let velocity = self.velocity_at(elapsed);
        Self {
            start: self.position_at(elapsed),
            move_profile: Some(TrapezoidProfile::ramp_down(velocity, decel)),
            settle: None,
            terminal_rate: rate,
        }
    }

    /// Velocity `elapsed` seconds in, the terminal rate included.
    #[must_use]
    pub fn velocity_at(&self, elapsed: f64) -> f64 {
        let elapsed = elapsed.max(0.0);
        match &self.move_profile {
            Some(motion) if elapsed < motion.duration() => motion.velocity_at(elapsed),
            _ => self.terminal_rate,
        }
    }

    /// The same plan with a different rate held once the move is over.
    ///
    /// Used by `start_tracking`/`stop_tracking` to reach a goto that is already running: the
    /// resume rate is baked in when the goto starts so that a dropped future still resumes
    /// tracking, and this is how an operator changes that decision without disturbing the slew.
    #[must_use]
    pub fn with_terminal_rate(&self, rate: f64) -> Self {
        Self {
            start: self.start,
            move_profile: self.move_profile.clone(),
            settle: self.settle,
            terminal_rate: rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRUISE: f64 = 3.489; // deg/s, the measured 87,486 counts/s
    const ACCEL: f64 = 2.0;
    const DECEL: f64 = 1.25;

    fn profile() -> SimulatorProfile {
        SimulatorProfile::default()
    }

    #[test]
    fn a_long_move_cruises_and_a_short_one_never_reaches_cruise() {
        // The behaviour the HEQ5's break point produces: 1,000 counts peaked at 5,350 counts/s
        // against an 87,486 counts/s cruise, because deceleration began at the halfway point.
        let long = TrapezoidProfile::plan(30.0, CRUISE, ACCEL, DECEL);
        let short = TrapezoidProfile::plan(0.04, CRUISE, ACCEL, DECEL);
        assert_eq!(long.segments.len(), 3, "accel, cruise, decel");
        assert_eq!(short.segments.len(), 2, "no cruise phase at all");
        let short_peak = short.segments[0].v1;
        assert!(
            short_peak < CRUISE / 10.0,
            "a 0.04 degree move peaked at {short_peak} deg/s"
        );
    }

    #[test]
    fn duration_is_not_linear_in_distance() {
        // SDD §5.2.3 forbids estimating slew time as distance / rate, because the spike measured
        // 100x the distance taking 6.4x the time. Assert the shape, since the ramp constants are
        // fitted and the exact numbers are not the contract.
        let near = TrapezoidProfile::plan(0.04, CRUISE, ACCEL, DECEL).duration();
        let far = TrapezoidProfile::plan(4.0, CRUISE, ACCEL, DECEL).duration();
        let ratio = far / near;
        assert!(
            (2.0..40.0).contains(&ratio),
            "100x the distance took {ratio}x the time; linear would be 100x"
        );
    }

    #[test]
    fn a_move_covers_exactly_its_distance_and_no_more() {
        for distance in [0.04, 1.0, 10.0, -10.0, 90.0] {
            let motion = TrapezoidProfile::plan(distance, CRUISE, ACCEL, DECEL);
            let end = motion.displacement_at(motion.duration());
            assert!(
                (end - distance).abs() < 1e-9,
                "{distance} deg move ended at {end}"
            );
            // Past the end it stays put — an axis that kept integrating would drift off target
            // for as long as nobody looked at it.
            assert!((motion.displacement_at(motion.duration() * 3.0) - distance).abs() < 1e-9);
        }
    }

    #[test]
    fn position_is_monotonic_and_continuous_through_a_move() {
        // The acceptance criterion, at the layer that decides it. 10 Hz is what the criterion
        // names; 1 kHz is what proves there is no step hiding between two 10 Hz samples.
        let plan = AxisPlan::moving(0.0, 30.0, &profile(), CRUISE, 0.0, 0.0);
        let duration = plan.motion_after();
        let steps = 10_000;
        let mut previous = plan.position_at(0.0);
        let mut largest_step: f64 = 0.0;
        for i in 1..=steps {
            let now = plan.position_at(duration * f64::from(i) / f64::from(steps));
            assert!(now >= previous, "position went backwards at sample {i}");
            largest_step = largest_step.max(now - previous);
            previous = now;
        }
        assert!((previous - 30.0).abs() < 1e-9, "ended at {previous}");
        // One millisecond at cruise is 0.0035 deg; anything larger is a jump.
        assert!(largest_step < 0.005, "largest step was {largest_step} deg");
    }

    #[test]
    fn settle_rings_down_to_exactly_zero_offset() {
        let mut config = profile();
        config.settle_amplitude_arcsec = 60.0;
        let plan = AxisPlan::moving(0.0, 10.0, &config, CRUISE, 4.0, 0.0);
        let motion = plan.motion_after();

        // It really oscillates: some sample inside the interval is off target by an appreciable
        // fraction of the amplitude, on both sides.
        let samples: Vec<f64> = (1..400)
            .map(|i| plan.position_at(motion + 4.0 * f64::from(i) / 400.0) - 10.0)
            .collect();
        let high = samples.iter().copied().fold(f64::MIN, f64::max);
        let low = samples.iter().copied().fold(f64::MAX, f64::min);
        assert!(high > 5.0 / 3600.0, "never rang above target: {high} deg");
        assert!(low < -5.0 / 3600.0, "never rang below target: {low} deg");

        // ...and it is exactly on target at the end, with no step to get there. A residual
        // offset at the boundary is a discontinuity, which is the bug this taper prevents.
        let just_before = plan.position_at(motion + 4.0 - 1e-4);
        let after = plan.position_at(motion + 4.0);
        assert!((after - 10.0).abs() < 1e-12, "settled at {after}");
        assert!(
            (after - just_before).abs() < 1e-4,
            "stepped {} deg at the end of settling",
            after - just_before
        );
        assert!((plan.settled_after() - (motion + 4.0)).abs() < 1e-9);
    }

    #[test]
    fn a_steady_plan_tracks_forever_at_its_rate() {
        let plan = AxisPlan::steady(10.0, tracking_rate(TrackingMode::Sidereal));
        // One hour of tracking advances the axis by 15.041 degrees.
        let after_an_hour = plan.position_at(3600.0);
        assert!(
            (after_an_hour - 10.0 - 15.041_07).abs() < 1e-4,
            "an hour of sidereal moved the axis to {after_an_hour}"
        );
        assert_eq!(plan.settled_after(), 0.0, "holding is not moving");
    }

    #[test]
    fn tracking_rates_are_ordered_the_way_the_sky_is() {
        // Lunar and solar are slower than sidereal because the Moon and Sun drift east against
        // the stars. Getting the sign wrong would drift a lunar sequence the wrong way.
        let sidereal = tracking_rate(TrackingMode::Sidereal);
        assert!(tracking_rate(TrackingMode::Lunar) < tracking_rate(TrackingMode::Solar));
        assert!(tracking_rate(TrackingMode::Solar) < sidereal);
        assert!((sidereal * 3600.0 * 3600.0 - 15.041_07 * 3600.0).abs() < 1.0);
    }

    #[test]
    fn slew_speeds_climb_and_top_out_at_the_stated_maximum() {
        let speeds = [
            SlewSpeed::Guide,
            SlewSpeed::Slow,
            SlewSpeed::Medium,
            SlewSpeed::Fast,
            SlewSpeed::Max,
        ];
        for pair in speeds.windows(2) {
            assert!(slew_rate(pair[0]) < slew_rate(pair[1]), "{pair:?}");
        }
        // PRD §4.2's 800x, which is also `MountCapabilities::max_slew_speed_x_sidereal`.
        let ratio = slew_rate(SlewSpeed::Max) / SIDEREAL_DEG_PER_SEC;
        assert!((ratio - 800.0).abs() < 1e-9, "max is {ratio}x sidereal");
    }

    #[test]
    fn freezing_a_plan_stops_it_where_it_was_at_that_instant() {
        // The e-stop path. The frozen position must equal the running plan's position at the
        // same instant to the last bit: any difference is the mount teleporting on stop.
        let plan = AxisPlan::moving(0.0, 30.0, &profile(), CRUISE, 2.0, 0.0);
        for at in [0.0, 0.001, 1.0, 3.7, 8.0] {
            let running = plan.position_at(at);
            let stopped = plan.frozen_at(at, 0.0);
            assert_eq!(stopped.position_at(0.0), running);
            assert_eq!(stopped.position_at(600.0), running, "and it stays stopped");
        }
    }

    #[test]
    fn a_stalled_axis_freezes_partway_and_never_arrives() {
        let plan = AxisPlan::moving(0.0, 30.0, &profile(), CRUISE, 2.0, 0.0);
        let stalled = plan.stalled(0.5);
        let stuck = stalled.position_at(stalled.motion_after());
        assert!(
            (0.5..25.0).contains(&stuck),
            "stalled at {stuck} deg, which is neither partway nor stuck"
        );
        assert_eq!(
            stalled.position_at(10_000.0),
            stuck,
            "a stalled axis does not creep"
        );
    }

    #[test]
    fn a_move_of_zero_is_not_a_nan() {
        // Goto to where the mount already points. The interesting failure is not the distance
        // but the duration: 0/0 here would be a slew that never completes.
        let plan = AxisPlan::moving(5.0, 0.0, &profile(), CRUISE, 2.0, 0.0);
        assert_eq!(plan.motion_after(), 0.0);
        assert!(plan.settled_after().is_finite());
        assert_eq!(plan.position_at(1.0), 5.0);
    }
}
