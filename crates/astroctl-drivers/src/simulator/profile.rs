//! The numbers the simulator is calibrated to — PRD §4.5, SDD §9.
//!
//! Every constant here is either a measurement from the HEQ5 spike
//! (`spikes/skywatcher-heq5/FINDINGS.md`) or is marked as *not* one. That distinction is the
//! whole value of this file: a simulator whose timing was invented is a simulator that makes the
//! M3 hardware swap a surprise, and the only defence against slowly inventing numbers is writing
//! down, next to each one, whether a telescope produced it.
//!
//! The profile is a plain value with public fields and `Default`. Tests that care about one
//! knob write `SimulatorProfile { slew_latency: …, ..Default::default() }` rather than
//! threading a builder, and a test that cares about none gets the mount as measured.

use std::time::Duration;

// -----------------------------------------------------------------------------------------
// Measured constants (spikes/skywatcher-heq5/FINDINGS.md)
// -----------------------------------------------------------------------------------------

/// Counts per revolution on both axes — `:a1`/`:a2` → `00B289`. **Measured.**
///
/// Not used to step a counter (the simulator works in degrees), but it is what converts every
/// measured counts/s figure below into the degrees/s this module actually holds, so it belongs
/// next to them rather than in a comment.
pub const COUNTS_PER_REVOLUTION: f64 = 9_024_000.0;

/// Motor-controller timer frequency in hertz — `:b1`/`:b2` → `A7FD00`. **Measured**, and it
/// corrects PRD §4.2's assumed 460,800 Hz by a factor of 7.0963.
///
/// Unused arithmetically here for the same reason the real driver barely uses it after the
/// handshake: it only sets the *step period* of tracking and manual slew, and the simulator
/// holds rates directly. Recorded because a driver reading a different value from a real mount
/// is reading a different mount.
pub const TIMER_FREQUENCY_HZ: f64 = 64_935.0;

/// Length of the sidereal day in seconds — the definition, not a measurement.
const SIDEREAL_DAY_SECONDS: f64 = 86_164.090_5;

/// Degrees of axis rotation per second at the sidereal rate.
///
/// 9,024,000 counts ÷ 86,164.0905 s = 104.7304 counts/s, which the spike measured as 104.617
/// over 1,863 samples — 0.11% low, i.e. the mount is right and the arithmetic is right. The
/// simulator uses the exact value; reproducing a 0.11% crystal error would make every
/// coordinate assertion in the workspace carry a tolerance for no benefit.
pub const SIDEREAL_DEG_PER_SEC: f64 = 360.0 / SIDEREAL_DAY_SECONDS;

/// Arcseconds one axis count subtends: 360° ÷ 9,024,000 = 0.1436″. **Measured** (via CPR).
///
/// This is the unit the SDD's goto tolerance (10 counts) and the spike's stop-overshoot
/// (84 counts = 12.1″) are quoted in, so tests that want to say "within a count" say it here.
pub const ARCSEC_PER_COUNT: f64 = 360.0 * 3600.0 / COUNTS_PER_REVOLUTION;

/// Peak goto rate in degrees/second: 87,486 counts/s. **Measured** (one 10° goto trace).
///
/// 835× sidereal, against PRD §4.2's stated 800× maximum — the requirement is a round number and
/// the mount is 4% faster than it. The simulator reproduces the mount.
const GOTO_CRUISE_DEG_PER_SEC: f64 = 87_486.0 * 360.0 / COUNTS_PER_REVOLUTION;

/// Serial round-trip time for one request/response exchange.
///
/// **Measured**: 2,000 `:j1` exchanges at 9600 8N1 gave min 14.7 ms, p50 15.8 ms, p99 16.9 ms,
/// max 17.2 ms — a 2.5 ms spread over the whole set. The simulator uses a fixed 16 ms rather
/// than a distribution: the value of injecting latency at all is that callers cannot assume
/// device access is free, and a jittering value would make every duration assertion in every
/// test above the HAL probabilistic to buy nothing.
///
/// Only `:j1` was ever timed, so applying this to every command is an *assumption* — a stated
/// one. See FINDINGS.md's open item on per-command-class timing.
const SERIAL_ROUND_TRIP_MS: u64 = 16;

/// Exchanges the Synta goto sequence costs before motion starts: `G`, `I`, `H`, `M`, the three
/// mandated readbacks (`h`, `i`, `m`) and `J`. **Derived** from SDD §5.2.3's command sequence,
/// which is itself confirmed against hardware (experiment E14).
///
/// At 16 ms each this is ~128 ms of dead time before a goto moves, which is most of the gap
/// between the spike's motion-only figure for a 1,000-count goto (0.19 s) and its wall-clock one
/// (0.64 s). A simulator that started moving instantly would hide it.
const GOTO_COMMAND_EXCHANGES: u32 = 8;

// -----------------------------------------------------------------------------------------
// The profile
// -----------------------------------------------------------------------------------------

/// How the simulated mount moves and how long it takes to answer (PRD §4.5).
///
/// Constructed by [`Default`] to the HEQ5 as measured, with the two *optical* error terms
/// (periodic error, polar drift) at zero. Those default off deliberately: they exist for the
/// Phase 2 pipeline and guiding work, and a mount whose reported position wanders by 12″ on its
/// own would force a tolerance into every pointing assertion written before then. A test that
/// wants them sets them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimulatorProfile {
    /// One request/response exchange with the simulated motor controller.
    ///
    /// Charged on every trait method — see [`SERIAL_ROUND_TRIP_MS`] for the measurement and for
    /// why it is fixed rather than sampled.
    pub round_trip: Duration,

    /// Cruise rate of a goto, in degrees of axis rotation per second.
    pub goto_cruise_deg_per_sec: f64,

    /// Acceleration entering a slew, in degrees/second².
    ///
    /// **Fitted**, not measured directly: the spike's one ramp trace samples axis rate at 0.8 s
    /// intervals, and each sample is an average over its window rather than an instantaneous
    /// rate, so the true ramp is steeper than the trace looks. 2.0 °/s² reproduces the trace's
    /// 1.6–2.0 s rise to cruise.
    pub accel_deg_per_sec2: f64,

    /// Deceleration leaving a slew, in degrees/second². Lower than the acceleration because the
    /// measured trace decelerates over 2.8 s having accelerated in ~1.7 s — the asymmetry is in
    /// the data, and a symmetric profile would finish a long goto half a second early.
    pub decel_deg_per_sec2: f64,

    /// Peak amplitude of the damped oscillation the tube shows after motion stops, in
    /// arcseconds.
    ///
    /// **Not measured.** The spike could not see it: the HEQ5 Pro is open-loop with no
    /// encoders, so its own counters report the commanded position whatever the tube does, and
    /// the only settling figure in FINDINGS.md is a plausibility bracket (30″–5′ of backlash)
    /// explicitly marked as not a measurement. 6″ is a deliberately small stand-in — big enough
    /// that a settle-aware caller behaves differently, small enough that nothing downstream
    /// starts depending on the number. Experiment E19 is what would replace it.
    pub settle_amplitude_arcsec: f64,

    /// Frequency of that oscillation, in hertz. **Not measured**; see
    /// [`settle_amplitude_arcsec`](Self::settle_amplitude_arcsec).
    pub settle_frequency_hz: f64,

    /// Peak periodic error in arcseconds, or 0 to disable it (MNT-13, PRD §4.5).
    ///
    /// Periodic error is a function of *worm position*, not of time, which is why it is applied
    /// against the RA axis angle rather than the clock: park the mount and the error parks with
    /// it. A typical HEQ5 shows ±10–20″; the `:s` register reports a PEC period of 66,844
    /// counts, i.e. 135 worm turns per revolution, which is where
    /// [`worm_period_degrees`](Self::worm_period_degrees) comes from.
    pub periodic_error_arcsec: f64,

    /// Degrees of RA-axis rotation per worm turn: 360° ÷ 135 teeth = 2.667°, which at sidereal
    /// rate is one turn per 638 s. **Measured** (the 135 falls out of the `:s` PEC period).
    pub worm_period_degrees: f64,

    /// Declination drift from polar misalignment, in arcseconds per minute, or 0 to disable.
    ///
    /// The reason unguided subframes trail. Off by default; the guiding work (Phase 3) is what
    /// turns it on.
    pub polar_drift_arcsec_per_min: f64,
}

impl Default for SimulatorProfile {
    fn default() -> Self {
        Self {
            round_trip: Duration::from_millis(SERIAL_ROUND_TRIP_MS),
            goto_cruise_deg_per_sec: GOTO_CRUISE_DEG_PER_SEC,
            accel_deg_per_sec2: 2.0,
            decel_deg_per_sec2: 1.25,
            settle_amplitude_arcsec: 6.0,
            settle_frequency_hz: 1.5,
            periodic_error_arcsec: 0.0,
            worm_period_degrees: 360.0 / 135.0,
            polar_drift_arcsec_per_min: 0.0,
        }
    }
}

impl SimulatorProfile {
    /// A mount that answers instantly and moves instantly.
    ///
    /// For tests *above* the HAL whose subject is not the mount — a route table, an event
    /// shape, a session state machine — where the measured profile would only make the suite
    /// slower without testing anything. Anything whose subject *is* timing must use
    /// [`Default`], and the safety tests (T05) must, because an instant slew has no interior for
    /// an e-stop to land in.
    #[must_use]
    pub fn instant() -> Self {
        Self {
            round_trip: Duration::ZERO,
            // Not infinity: the profile's arithmetic divides by these, and an infinite rate
            // makes every duration a NaN rather than a zero.
            goto_cruise_deg_per_sec: 1.0e6,
            accel_deg_per_sec2: 1.0e9,
            decel_deg_per_sec2: 1.0e9,
            settle_amplitude_arcsec: 0.0,
            ..Self::default()
        }
    }

    /// The dead time before a goto starts moving: the eight-exchange Synta command sequence.
    #[must_use]
    pub fn goto_command_time(&self) -> Duration {
        self.round_trip * GOTO_COMMAND_EXCHANGES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_derived_constants_match_the_spike_measurements() {
        // 104.7304 counts/s is the sidereal rate FINDINGS.md derives and then measures as
        // 104.617. If this drifts, the mount and the simulator disagree about what a star does.
        let counts_per_sec = SIDEREAL_DEG_PER_SEC * COUNTS_PER_REVOLUTION / 360.0;
        assert!(
            (counts_per_sec - 104.730_4).abs() < 0.001,
            "sidereal is {counts_per_sec} counts/s"
        );
        // 0.1436 arcsec/count, the unit the SDD's 10-count goto tolerance is quoted in.
        assert!((ARCSEC_PER_COUNT - 0.143_6).abs() < 0.000_1);
        // The measured 87,486 counts/s cruise is 835× sidereal — 4% above PRD §4.2's stated
        // 800× maximum, which is the requirement being round rather than the mount being wrong.
        let ratio = SimulatorProfile::default().goto_cruise_deg_per_sec / SIDEREAL_DEG_PER_SEC;
        assert!(
            (835.0..836.0).contains(&ratio),
            "cruise is {ratio}x sidereal"
        );
        // The timer frequency is recorded, not used; assert the value so a typo in it is a test
        // failure rather than a comment nobody reads.
        assert!((TIMER_FREQUENCY_HZ - 64_935.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_default_profile_reproduces_the_measured_ten_degree_goto() {
        // The one ramp trace the spike captured: 250,667 counts (10°) in 5.61 s wall clock,
        // including the command sequence. This is the single most load-bearing number in the
        // file — it is the only end-to-end timing the simulator can be checked against.
        let profile = SimulatorProfile::default();
        let motion = super::super::motion::TrapezoidProfile::plan(
            10.0,
            profile.goto_cruise_deg_per_sec,
            profile.accel_deg_per_sec2,
            profile.decel_deg_per_sec2,
        );
        let total = motion.duration() + profile.goto_command_time().as_secs_f64();
        assert!(
            (5.0..6.0).contains(&total),
            "a 10 degree goto takes {total} s; the HEQ5 took 5.61 s"
        );
    }

    #[test]
    fn the_instant_profile_is_fast_but_finite() {
        // A NaN duration here would surface three layers up as a slew that never ends.
        let profile = SimulatorProfile::instant();
        let motion = super::super::motion::TrapezoidProfile::plan(
            10.0,
            profile.goto_cruise_deg_per_sec,
            profile.accel_deg_per_sec2,
            profile.decel_deg_per_sec2,
        );
        assert!(motion.duration().is_finite());
        assert!(motion.duration() < 0.001, "{} s", motion.duration());
    }
}
