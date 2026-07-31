//! `MotorController` — one axis's motion semantics, over the codec and under the mount facade
//! (SDD §5.2.1, M3-T03).
//!
//! ```text
//! SkywatcherMount (impl MountDevice)          — coordinates, modes, goto logic      M3-T04
//!     └── MotorController                     — per-axis counts, motion modes, rates M3-T03
//!           └── SyntaCodec + SerialTask       — framing, encoding, lanes             M3-T01/T02
//! ```
//!
//! # Everything here is a value, not an effect
//!
//! [`MotorController`] has no port, no runtime and no clock. Its methods *build* typed commands
//! and interpret typed replies; they never send anything. That is what makes the arithmetic
//! testable — the step-period table, the goto program and the slew-complete rule are all checked
//! in unit tests with no mount and no `async` — and it is what keeps this task out of M3-T02's
//! way, since nothing here knows a serial port exists.
//!
//! The sequencing that genuinely needs an exchange lives in [`exchange`], generic over a
//! four-line trait, so M3-T04 can glue it to whatever the serial task actually offers.
//!
//! # One controller per axis, and no shared constants
//!
//! Counts per revolution, timer frequency and the high-speed ratio all arrive from *this axis's*
//! handshake replies. The HEQ5 answers identically on both, and taking one axis's answer for the
//! other would be assuming that of every mount — which is the shape of assumption that put a
//! 460,800 Hz timer frequency in PRD §4.2 for a mount that runs at 64,935.
//!
//! # What this layer refuses to make easy
//!
//! **A speed class and a step period that disagree.** They are two commands, `G` and `I`, and the
//! cost of disagreement is the verified 16× ratio. [`rate::ProgrammedRate`] holds both.
//!
//! **A goto started without its readbacks.** M3-T01 made `J`-for-a-goto take a proof; this layer
//! never produces that proof by any route other than [`exchange::run_goto`], which performs the
//! three inquiries.
//!
//! **A slew-complete decision from the counter alone.** SDD §5.2.3 says stopped *and* within
//! tolerance, and [`MotorController::arrived`] is both. A goto that landed 300,000 counts away
//! with the axis stopped is a stall, not an arrival.

pub mod exchange;
pub mod rate;

use std::time::Duration;

use astroctl_core::types::{Axis, SlewSpeed, TrackingMode};

use super::codec::{
    AxisStatus, Counts, CountsPerRev, EncodeError, FirmwareVersion, GetAxisStatus, GetPosition,
    GotoProgram, GuideRate as WireGuideRate, HighSpeedRatio, Initialise, InstantStop,
    MotionDirection, MotionMode, Move, SetGuideRate, SetMotionMode, SetStepPeriod, SpeedClass,
    StartMotion, StopMotion, TimerFrequency,
};
use super::math::{AxisScale, MathError};

pub use rate::{CountsPerSecond, ProgrammedRate, RateModel};

/// How close to the target counts an axis must be, with the drive stopped, before a goto is
/// declared complete — SDD §5.2.3's stated default.
///
/// **Ample and loose in the safe direction.** The spike measured *zero* counts of error across
/// six gotos from 0.04° to 4° in both directions, so ten counts (1.4″ at the fixture scale) is
/// three orders of magnitude of headroom against a measurement of nothing. Too tight would
/// declare a stall on a perfect goto; too loose would accept a stall as an arrival, which is why
/// [`MotorController::arrived`] tests the running bit as well and not instead.
pub const GOTO_TOLERANCE_COUNTS: u32 = 10;

/// What a motor controller can refuse.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum ControllerError {
    /// The mount reported a zero timer frequency, which every step period divides by.
    #[error("the mount reported a timer frequency of zero")]
    ZeroTimerFrequency,

    /// The mount reported a zero high-speed ratio, which the speed-class crossover divides by.
    #[error("the mount reported a high-speed ratio of zero")]
    ZeroHighSpeedRatio,

    /// A rate that is not a rate: zero, negative or non-finite.
    #[error("{0} is not an axis rate; it must be finite and positive")]
    BadRate(f64),

    /// A rate no step period can express, in either speed class.
    #[error("{counts_per_second} counts/s needs a step period of {period:.3}, which the 24-bit register cannot hold")]
    RateOutOfRange {
        /// The rate asked for.
        counts_per_second: f64,
        /// The period it would need, before rounding.
        period: f64,
    },

    /// A guide pulse whose duration is not a duration this arithmetic can use.
    #[error("a guide pulse of {0:?} is longer than this driver will program in one motion")]
    PulseTooLong(Duration),

    /// The position arithmetic refused something.
    #[error(transparent)]
    Math(#[from] MathError),

    /// The codec refused to encode something.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// What one axis reported about itself at handshake.
///
/// Every field is a *measurement from this mount*, not a constant of this driver. PRD §10 records
/// that reading them rather than hardcoding them is what limited the blast radius when the
/// documented timer frequency turned out to be wrong by 7.1×.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AxisParams {
    /// Which axis answered.
    pub axis: Axis,
    /// `:e` — motor-controller firmware and mount model.
    pub firmware: FirmwareVersion,
    /// `:a` — counts per revolution.
    pub counts_per_revolution: CountsPerRev,
    /// `:b` — timer interrupt frequency in hertz.
    pub timer_frequency: TimerFrequency,
    /// `:g` — the ratio between the two speed classes.
    pub high_speed_ratio: HighSpeedRatio,
}

/// The two commands that begin every motion, and the one that starts it.
///
/// A struct rather than three return values because `G` and `I` have to be sent in that order and
/// `J` must come last; handing back a tuple would let a caller reorder them, and the mount does
/// not complain about a step period written after the motion started — it just ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotionSequence {
    mode: SetMotionMode,
    step_period: SetStepPeriod,
    start: StartMotion,
    rate: ProgrammedRate,
}

impl MotionSequence {
    /// `G`, first.
    #[must_use]
    pub const fn motion_mode(&self) -> SetMotionMode {
        self.mode
    }

    /// `I`, second.
    #[must_use]
    pub const fn step_period(&self) -> SetStepPeriod {
        self.step_period
    }

    /// `J`, last. Unbounded — there is no target register, so there is nothing to read back
    /// (M3-T01's [`StartMotion::unbounded`]).
    #[must_use]
    pub const fn start(&self) -> StartMotion {
        self.start
    }

    /// The class and period this sequence programs.
    #[must_use]
    pub const fn rate(&self) -> ProgrammedRate {
        self.rate
    }
}

/// A guide correction: the rate register, and the displacement it is meant to produce.
///
/// # The displacement does not depend on the unverified part
///
/// `P`'s payload is one digit and `spikes/skywatcher-heq5/ENCODINGS.md` records that "semantics of
/// each level unconfirmed" — E15 would settle it and has not been run. So this type does not rely
/// on it. [`Self::offset`] is computed from the *sidereal rate and the pulse duration*, both of
/// which are known exactly, and is realised as a **bounded goto**: a self-terminating motion that
/// the spike measured landing with zero counts of error. `P` is still programmed, because SDD §5.1
/// says the driver programs it before issuing the pulse and because an ST4-driven guider would
/// use it, but nothing here is wrong if its levels turn out to mean something else.
///
/// The cost of that choice, stated: a bounded goto ramps and stops, so the *displacement* is
/// exact and the *duration* is whatever the ramp takes rather than the requested one. For a
/// guiding loop that corrects position, displacement is the contract. A rate-nudge
/// implementation — set a modified step period, wait, set it back — would honour the duration
/// instead and would have to trust `P` or the step-period relation away from sidereal. Both are
/// defensible; this one is the one with hardware behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GuidePulse {
    rate: SetGuideRate,
    offset: Move,
    duration: Duration,
}

impl GuidePulse {
    /// `P` — the autoguide rate register, programmed before the pulse (SDD §5.1).
    #[must_use]
    pub const fn guide_rate(&self) -> SetGuideRate {
        self.rate
    }

    /// The displacement, with its direction. Zero when the pulse is shorter than one count.
    #[must_use]
    pub const fn offset(&self) -> Move {
        self.offset
    }

    /// How long the pulse was asked to last.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    /// Whether the pulse moves the axis at all.
    ///
    /// A correction below one count — 0.1436″ on the operator's mount — is not a small motion,
    /// it is no motion: the increment register would carry zero and the goto would be refused for
    /// having no break point inside it. A guide loop should skip such a pulse rather than send
    /// it, and this is how it can tell.
    #[must_use]
    pub fn is_measurable(&self) -> bool {
        self.offset.magnitude().get() > 0
    }
}

/// One axis's motion semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotorController {
    params: AxisParams,
    scale: AxisScale,
    rates: RateModel,
}

impl MotorController {
    /// Build from what the handshake read.
    ///
    /// # Errors
    /// [`ControllerError::Math`] for a zero counts-per-revolution, and
    /// [`ControllerError::ZeroTimerFrequency`] / [`ControllerError::ZeroHighSpeedRatio`] for the
    /// other two divisors. All three are "the mount answered, but not with a number" — which is
    /// exactly what a mis-addressed inquiry looks like, since `:j9` answers plausible data for an
    /// axis that does not exist.
    pub fn new(params: AxisParams) -> Result<Self, ControllerError> {
        let scale = AxisScale::new(params.counts_per_revolution)?;
        let rates = RateModel::new(params.timer_frequency, params.high_speed_ratio, scale)?;
        Ok(Self {
            params,
            scale,
            rates,
        })
    }

    /// Which axis this controls.
    #[must_use]
    pub const fn axis(&self) -> Axis {
        self.params.axis
    }

    /// What the handshake read.
    #[must_use]
    pub const fn params(&self) -> AxisParams {
        self.params
    }

    /// The counts↔degrees scale for this axis.
    #[must_use]
    pub const fn scale(&self) -> AxisScale {
        self.scale
    }

    /// The rate model — timer frequency, high-speed ratio and scale.
    #[must_use]
    pub const fn rates(&self) -> RateModel {
        self.rates
    }

    // -------------------------------------------------------------------------------------
    // The commands with no arithmetic behind them
    // -------------------------------------------------------------------------------------

    /// `F` — initialise the axis. Sets the initialised bit and moves nothing (measured, E1).
    #[must_use]
    pub const fn initialise(&self) -> Initialise {
        Initialise(self.params.axis)
    }

    /// `K` — the ordinary ramped stop, for a slew or for tracking.
    #[must_use]
    pub const fn stop(&self) -> StopMotion {
        StopMotion(self.params.axis)
    }

    /// `L` — instant stop. **Emergency only** (SDD §5.2.4).
    ///
    /// Indistinguishable from `K` at low speed — 85 counts of overshoot against 84, which at that
    /// rate is one serial round trip rather than deceleration. The advantage is real only where
    /// momentum is, and at 817× sidereal the same 16 ms is ~1,370 counts.
    #[must_use]
    pub const fn emergency_stop(&self) -> InstantStop {
        InstantStop(self.params.axis)
    }

    /// `:j` — the position poll, at 1 Hz normally and 2 Hz during a goto (SDD §5.2.3).
    #[must_use]
    pub const fn read_position(&self) -> GetPosition {
        GetPosition(self.params.axis)
    }

    /// `:f` — the status poll, which is what slew-complete detection reads.
    #[must_use]
    pub const fn read_status(&self) -> GetAxisStatus {
        GetAxisStatus(self.params.axis)
    }

    // -------------------------------------------------------------------------------------
    // Motion
    // -------------------------------------------------------------------------------------

    /// Program an unbounded motion at a chosen rate — tracking, or a manual slew.
    ///
    /// # Errors
    /// [`ControllerError::RateOutOfRange`] if no step period expresses the rate.
    pub fn motion_at(
        &self,
        rate: CountsPerSecond,
        direction: MotionDirection,
    ) -> Result<MotionSequence, ControllerError> {
        let programmed = self.rates.program(rate)?;
        Ok(MotionSequence {
            mode: SetMotionMode {
                axis: self.params.axis,
                mode: MotionMode::slew(programmed.speed(), direction),
            },
            step_period: SetStepPeriod {
                axis: self.params.axis,
                period: programmed.period(),
            },
            start: StartMotion::unbounded(self.params.axis),
            rate: programmed,
        })
    }

    /// Program tracking.
    ///
    /// The direction is the caller's because it depends on the hemisphere, which is a property of
    /// the *site* and not of the axis — see [`tracking_direction`](super::math::tracking_direction).
    /// Below the equator the tracking motor runs backward, and a controller that decided this for
    /// itself would have to be told the latitude to do it.
    ///
    /// # Errors
    /// As [`Self::motion_at`].
    pub fn track(
        &self,
        mode: TrackingMode,
        direction: MotionDirection,
    ) -> Result<MotionSequence, ControllerError> {
        self.motion_at(self.rates.tracking(mode)?, direction)
    }

    /// Program a manual slew at a speed class.
    ///
    /// # Errors
    /// As [`Self::motion_at`].
    pub fn slew(
        &self,
        speed: SlewSpeed,
        direction: MotionDirection,
    ) -> Result<MotionSequence, ControllerError> {
        self.motion_at(self.rates.slew(speed)?, direction)
    }

    /// Program a bounded goto: `G`, `I`, `H`, `M`, and the readback expectations.
    ///
    /// `from` must be `:j` read immediately before, with the axis stopped — the readback
    /// expectations are absolute and computed against it, so a stale value invalidates all three.
    ///
    /// The step period is sent because the sequence includes it and because reading it back is
    /// evidence the whole write sequence landed. It does **not** set the goto speed: a 10× change
    /// left the velocity profile identical on hardware. The speed class is the only thing that
    /// does, which is why it is an argument and the period is not.
    ///
    /// The break point is the midpoint — the policy `motion.py` used for every goto it ran, at
    /// zero counts of measured error across 1,000, 10,000 and 100,000 counts in both directions.
    ///
    /// # Errors
    /// [`ControllerError::Encode`] if the move is zero-length (a break point cannot be inside a
    /// move of nothing) or longer than the increment register.
    pub fn goto(
        &self,
        from: Counts,
        target: Move,
        speed: SpeedClass,
    ) -> Result<GotoProgram, ControllerError> {
        // Tracking rate for the step period: it is ignored by a goto, and sending a *plausible*
        // ignored value is better than sending a fast one that would matter if the measurement
        // that says it is ignored is ever overturned.
        let period = self
            .rates
            .program(self.rates.tracking(TrackingMode::Sidereal)?)?
            .period();
        Ok(GotoProgram::new(
            self.params.axis,
            from,
            target,
            GotoProgram::midpoint_break(target),
            speed,
            period,
        )?)
    }

    /// The speed class a goto of `counts` should use.
    ///
    /// SDD §5.2.3: "long slews use high-speed motion mode". The threshold is a *policy*, and it is
    /// stated rather than tuned: the low class was measured at ≈5,350 counts/s and the high class
    /// at ≈87,000, and the high class's ramp is what makes a long move worth it. Half a degree —
    /// 12,533 counts at the fixture scale — is comfortably past the 1,000-count move that never
    /// left its ramp, and comfortably short of anything an operator would call a slew.
    #[must_use]
    pub fn goto_speed_class(&self, counts: u32) -> SpeedClass {
        let half_degree = self.scale.counts_in(0.5).abs();
        if f64::from(counts) >= half_degree {
            SpeedClass::High
        } else {
            SpeedClass::Low
        }
    }

    /// Program a guide pulse — see [`GuidePulse`] for what is and is not verified about it.
    ///
    /// The displacement is `rate × sidereal × duration`, in counts, rounded. The `P` level is
    /// derived from the same fraction by a mapping this driver **cannot yet check**: `indi-eqmod`
    /// sends one digit `0`–`9` as a magnitude and no source says what each level means. Nine
    /// steps over the `(0, 1]` the [`GuideRate`](astroctl_core::types::GuideRate) newtype allows
    /// is the obvious reading of "a rate magnitude"; E15 is the experiment that would confirm or
    /// replace it, and the displacement above does not depend on the answer.
    ///
    /// # Errors
    /// [`ControllerError::PulseTooLong`] if the displacement would overflow the increment
    /// register — about eleven hours at the sidereal rate on the operator's mount, i.e. a caller
    /// that has passed milliseconds where it meant seconds.
    pub fn guide_pulse(
        &self,
        rate: astroctl_core::types::GuideRate,
        direction: MotionDirection,
        duration: Duration,
    ) -> Result<GuidePulse, ControllerError> {
        let sidereal = self.rates.tracking(TrackingMode::Sidereal)?.get();
        let counts = (rate.fraction() * sidereal * duration.as_secs_f64()).round();
        if !counts.is_finite() || counts > f64::from(super::codec::U24_MAX) {
            return Err(ControllerError::PulseTooLong(duration));
        }
        let signed = counts as i64 * i64::from(direction.sign());

        // Nine levels over (0, 1]: 1.0 → `9`, and anything positive → at least `1`, because level
        // `0` in a scheme whose semantics are unknown is the one value that might mean "off".
        let level = (rate.fraction() * 9.0).round().clamp(1.0, 9.0) as u8;
        let wire_rate = WireGuideRate::new(level)?;

        Ok(GuidePulse {
            rate: SetGuideRate {
                axis: self.params.axis,
                rate: wire_rate,
            },
            offset: Move::from_delta(signed)?,
            duration,
        })
    }

    // -------------------------------------------------------------------------------------
    // Status
    // -------------------------------------------------------------------------------------

    /// Whether a goto is finished on this axis (SDD §5.2.3).
    ///
    /// Stopped **and** within tolerance. Both halves are load-bearing: an axis still running is
    /// not there yet whatever its counter says, and an axis stopped a long way from its target is
    /// a stall reported as an arrival — which is the failure that makes the next frame a smear
    /// and the one after that a plate-solve failure with no obvious cause.
    #[must_use]
    pub fn arrived(&self, status: AxisStatus, at: Counts, target: Counts) -> bool {
        !status.running && self.within_tolerance(at, target)
    }

    /// Whether two counters are within the goto tolerance of each other, the short way round.
    #[must_use]
    pub fn within_tolerance(&self, at: Counts, target: Counts) -> bool {
        self.scale.shortest_delta(at, target).unsigned_abs() <= u64::from(GOTO_TOLERANCE_COUNTS)
    }

    /// How far an axis is from a target, in arcseconds — for a log line and for the error a
    /// failed goto reports.
    #[must_use]
    pub fn separation_arcsec(&self, at: Counts, target: Counts) -> f64 {
        self.scale.shortest_delta(at, target) as f64 * self.scale.arcsec_per_count()
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! The three constants the operator's HEQ5 reported, and a controller built from them.
    //!
    //! **These are fixtures, not driver constants** (PRD §4.2: "The driver reads these at
    //! handshake and never hardcodes them; the values below are for test fixtures and
    //! hand-verification"). This module is `#[cfg(test)]` so the claim is enforced by the
    //! compiler rather than by review: nothing outside a test can reach them.

    use super::*;
    use crate::skywatcher::codec::hex::U24;

    /// `:a1`/`:a2` → `00B289`. Verified, both axes.
    pub const CPR: u32 = 9_024_000;
    /// `:b1`/`:b2` → `A7FD00`. Verified, and the corrected value.
    pub const TIMER_HZ: u32 = 64_935;
    /// `:g1`/`:g2` → `10`. Verified, both axes.
    pub const HIGH_SPEED_RATIO: u8 = 16;
    /// `:e1` → `020401`, read big-endian: firmware 2.4, model code 1 (HEQ5).
    pub const FIRMWARE_RAW: u32 = 0x0002_0401;

    /// The parameters an HEQ5 handshake produces.
    #[must_use]
    pub fn params(axis: Axis) -> AxisParams {
        AxisParams {
            axis,
            firmware: FirmwareVersion::from_raw(FIRMWARE_RAW),
            counts_per_revolution: CountsPerRev(U24::new(CPR).expect("fits")),
            timer_frequency: TimerFrequency(U24::new(TIMER_HZ).expect("fits")),
            high_speed_ratio: HighSpeedRatio(HIGH_SPEED_RATIO),
        }
    }

    /// A controller for an HEQ5 axis.
    #[must_use]
    pub fn controller(axis: Axis) -> MotorController {
        MotorController::new(params(axis)).expect("the fixture parameters are all non-zero")
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{controller, params, CPR};
    use super::*;
    use crate::skywatcher::codec::hex::{MountModel, U24};
    use crate::skywatcher::codec::Command;

    fn wire<C: Command>(command: &C) -> String {
        command.encode().to_string()
    }

    #[test]
    fn the_handshake_parameters_identify_the_mount_that_produced_them() {
        let params = params(Axis::Ra);
        assert_eq!(params.firmware.model(), Some(MountModel::Heq5));
        assert_eq!(params.counts_per_revolution.0.get(), CPR);
        let controller = MotorController::new(params).expect("valid");
        assert_eq!(controller.axis(), Axis::Ra);
        assert_eq!(controller.scale().counts_per_revolution(), CPR);
        assert_eq!(controller.rates().timer_frequency(), 64_935);
        assert_eq!(controller.rates().high_speed_ratio(), 16);
    }

    #[test]
    fn a_handshake_reply_of_zero_never_becomes_a_controller() {
        // `:j9` answers `=000080` for an axis that does not exist, so a mis-addressed inquiry
        // returns plausible data rather than an error. A zero divisor is one of the few shapes
        // that is *not* plausible, and refusing it here is the cheapest place to notice.
        for broken in [
            AxisParams {
                counts_per_revolution: CountsPerRev(U24::ZERO),
                ..params(Axis::Ra)
            },
            AxisParams {
                timer_frequency: TimerFrequency(U24::ZERO),
                ..params(Axis::Ra)
            },
            AxisParams {
                high_speed_ratio: HighSpeedRatio(0),
                ..params(Axis::Ra)
            },
        ] {
            assert!(MotorController::new(broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn tracking_programs_the_slew_mode_and_the_period_the_spike_drove() {
        // `motion.py` sent `:G110` (SLEW, low, forward) and `:I16C0200` (620) for the tracking-rate
        // run that E10 measured. Reproducing both frames is the strongest statement this module
        // can make without a mount attached.
        let controller = controller(Axis::Ra);
        let sequence = controller
            .track(TrackingMode::Sidereal, MotionDirection::Forward)
            .expect("sidereal is in range");
        assert_eq!(wire(&sequence.motion_mode()), ":G110");
        assert_eq!(wire(&sequence.step_period()), ":I16C0200");
        assert_eq!(wire(&sequence.start()), ":J1");
        assert_eq!(sequence.rate().period().get(), 620);
        assert_eq!(sequence.rate().speed(), SpeedClass::Low);
    }

    #[test]
    fn tracking_backward_changes_only_the_direction_digit() {
        // The southern hemisphere's whole difference, at this layer.
        let controller = controller(Axis::Ra);
        let north = controller
            .track(TrackingMode::Sidereal, MotionDirection::Forward)
            .expect("valid");
        let south = controller
            .track(TrackingMode::Sidereal, MotionDirection::Backward)
            .expect("valid");
        assert_eq!(wire(&north.motion_mode()), ":G110");
        assert_eq!(wire(&south.motion_mode()), ":G111");
        assert_eq!(
            wire(&north.step_period()),
            wire(&south.step_period()),
            "the rate does not depend on which way the sky turns"
        );
    }

    #[test]
    fn a_goto_sends_the_four_writes_in_protocol_order_with_a_midpoint_brake() {
        let controller = controller(Axis::Dec);
        let from = Counts::new(8_654_669).expect("fits");
        let program = controller
            .goto(
                from,
                Move::from_delta(12_345).expect("fits"),
                SpeedClass::Low,
            )
            .expect("a non-zero move");
        let frames: Vec<String> = program.writes().iter().map(ToString::to_string).collect();
        assert_eq!(
            frames,
            // 12,345 = 0x003039 → `393000`; the midpoint 6,172 = 0x00181C → `1C1800`.
            vec![":G220", ":I26C0200", ":H2393000", ":M21C1800"],
            "GOTO/low/forward, period 620, increment 12,345, brake 6,172"
        );
        assert_eq!(program.destination().get(), 8_654_669 + 12_345);
    }

    #[test]
    fn a_goto_of_nothing_is_refused_rather_than_programmed() {
        // A break point has to lie inside the move, and a move of zero has no inside. That is the
        // codec's rule; what matters here is that it fires as an error rather than as a goto with
        // a brake at count zero.
        let controller = controller(Axis::Ra);
        assert!(matches!(
            controller.goto(Counts::HOME, Move::NONE, SpeedClass::Low),
            Err(ControllerError::Encode(
                EncodeError::BreakPointOutsideMove { .. }
            ))
        ));
    }

    #[test]
    fn the_goto_speed_class_turns_over_at_half_a_degree() {
        let controller = controller(Axis::Ra);
        let half_degree = (f64::from(CPR) * 0.5 / 360.0).round() as u32; // 12,533
        assert_eq!(controller.goto_speed_class(1_000), SpeedClass::Low);
        assert_eq!(
            controller.goto_speed_class(half_degree - 1),
            SpeedClass::Low
        );
        assert_eq!(
            controller.goto_speed_class(half_degree + 1),
            SpeedClass::High
        );
        assert_eq!(controller.goto_speed_class(250_667), SpeedClass::High);
    }

    #[test]
    fn slew_complete_needs_the_drive_stopped_and_the_counter_close() {
        // SDD §5.2.3, both halves. The stalled case is the one worth having a test for: an axis
        // that stopped 300,000 counts short is a mechanical fault, and reporting it as an arrival
        // is how a night's frames become a night's smears.
        let controller = controller(Axis::Ra);
        let target = Counts::new(8_400_000).expect("fits");
        let stopped = AxisStatus::decode("101").expect("valid");
        let running = AxisStatus::decode("111").expect("valid");

        assert!(controller.arrived(stopped, target, target));
        assert!(controller.arrived(
            stopped,
            Counts::new(8_400_000 + GOTO_TOLERANCE_COUNTS).unwrap(),
            target
        ));
        assert!(!controller.arrived(
            stopped,
            Counts::new(8_400_000 + GOTO_TOLERANCE_COUNTS + 1).unwrap(),
            target
        ));
        assert!(
            !controller.arrived(running, target, target),
            "an axis still running has not arrived, whatever its counter reads"
        );
        let stalled = Counts::new(8_100_000).expect("fits");
        assert!(!controller.arrived(stopped, stalled, target));
        assert!(
            (controller.separation_arcsec(stalled, target) - 43_085.0).abs() < 10.0,
            "300,000 counts is {}″",
            controller.separation_arcsec(stalled, target)
        );
    }

    #[test]
    fn the_tolerance_is_symmetric_and_folds_the_short_way() {
        // A counter one count *below* the target is as arrived as one above it, and a target near
        // the counter's own wrap must not read as half a revolution away.
        let controller = controller(Axis::Ra);
        let target = Counts::new(5).expect("fits");
        assert!(controller.within_tolerance(Counts::new(0).unwrap(), target));
        assert!(controller.within_tolerance(Counts::new(15).unwrap(), target));
        assert!(!controller.within_tolerance(Counts::new(16).unwrap(), target));
    }

    #[test]
    fn a_guide_pulse_displaces_the_axis_by_the_sidereal_rate_times_its_duration() {
        // Half sidereal for one second is 0.5 × 104.7304 = 52.365 counts, which the mount can
        // only issue as 52 — 7.468″ where the continuous answer is 7.521″. The rounding is the
        // mount's resolution and not an approximation this driver chose, so the assertion is on
        // the count and the arcseconds are checked against what 52 counts actually is.
        let controller = controller(Axis::Ra);
        let half = astroctl_core::types::GuideRate::new(0.5).expect("in range");
        let pulse = controller
            .guide_pulse(half, MotionDirection::Forward, Duration::from_secs(1))
            .expect("valid");
        assert_eq!(pulse.offset().magnitude().get(), 52);
        assert_eq!(pulse.offset().direction(), MotionDirection::Forward);
        assert!(pulse.is_measurable());
        let arcsec =
            f64::from(pulse.offset().magnitude().get()) * controller.scale().arcsec_per_count();
        assert!((arcsec - 7.468).abs() < 0.01, "the pulse moved {arcsec}″");
        assert!(
            (arcsec - 7.521).abs() < controller.scale().arcsec_per_count(),
            "and it is within one count of the exact 7.521″"
        );

        // ...and backward carries the same magnitude with the other direction, which is `Move`'s
        // whole reason for existing.
        let back = controller
            .guide_pulse(half, MotionDirection::Backward, Duration::from_secs(1))
            .expect("valid");
        assert_eq!(back.offset().magnitude(), pulse.offset().magnitude());
        assert_eq!(back.offset().direction(), MotionDirection::Backward);
    }

    #[test]
    fn a_pulse_shorter_than_one_count_reports_that_rather_than_programming_nothing() {
        let controller = controller(Axis::Ra);
        let slow = astroctl_core::types::GuideRate::new(0.01).expect("in range");
        let pulse = controller
            .guide_pulse(slow, MotionDirection::Forward, Duration::from_millis(100))
            .expect("valid");
        assert!(
            !pulse.is_measurable(),
            "0.1 counts is not a motion this mount can make"
        );
        // The `P` register is still programmed — a level of at least 1, never 0.
        assert_eq!(wire(&pulse.guide_rate()), ":P11");
    }

    #[test]
    fn the_guide_rate_register_spans_its_nine_levels() {
        let controller = controller(Axis::Ra);
        for (fraction, digit) in [(1.0, '9'), (0.5, '5'), (0.11, '1'), (0.001, '1')] {
            let rate = astroctl_core::types::GuideRate::new(fraction).expect("in range");
            let pulse = controller
                .guide_pulse(rate, MotionDirection::Forward, Duration::from_secs(1))
                .expect("valid");
            let frame = wire(&pulse.guide_rate());
            assert_eq!(
                frame.chars().last(),
                Some(digit),
                "{fraction} produced {frame}"
            );
        }
    }

    #[test]
    fn a_pulse_long_enough_to_overflow_the_increment_register_is_refused() {
        let controller = controller(Axis::Ra);
        let full = astroctl_core::types::GuideRate::new(1.0).expect("in range");
        // 16,777,215 counts at 104.73 counts/s is about 44.5 hours.
        let pulse =
            controller.guide_pulse(full, MotionDirection::Forward, Duration::from_secs(200_000));
        assert!(matches!(pulse, Err(ControllerError::PulseTooLong(_))));
    }

    #[test]
    fn every_command_this_controller_builds_carries_its_own_axis() {
        // A goto programmed on DEC that verified against RA's registers would pass while the
        // wrong axis moved, and the mount does not validate the axis digit. The pairing is
        // per-controller, and this is where it is asserted.
        let dec = controller(Axis::Dec);
        assert_eq!(wire(&dec.initialise()), ":F2");
        assert_eq!(wire(&dec.stop()), ":K2");
        assert_eq!(wire(&dec.emergency_stop()), ":L2");
        assert_eq!(wire(&dec.read_position()), ":j2");
        assert_eq!(wire(&dec.read_status()), ":f2");
        let sequence = dec
            .slew(SlewSpeed::Medium, MotionDirection::Forward)
            .expect("valid");
        assert!(wire(&sequence.motion_mode()).starts_with(":G2"));
        assert!(wire(&sequence.step_period()).starts_with(":I2"));
        assert_eq!(wire(&sequence.start()), ":J2");
    }
}
