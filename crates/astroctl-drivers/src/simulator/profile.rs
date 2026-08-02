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

use astroctl_core::types::RaDec;

use super::sky::StarField;

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

    /// The pose this rig powers on at **and** parks to (M3-T07).
    ///
    /// One field for both because on the real mount they are one fact: power-on assigns
    /// `0x800000` to both axis counters regardless of where the metal is, and park's whole job is
    /// to return to the pose that assumption describes. A simulator with two separate settings
    /// could be configured into the mismatch between belief and reality that M3-T07 exists to
    /// remove, and would then be a worse model than the mount.
    ///
    /// It is here rather than in `mount.park_position` — which was deleted — because it is a
    /// property of the simulated rig, like the cruise rate above, and not something an operator
    /// chooses. Defaults to the north celestial pole, which is where the home pose looks.
    ///
    /// **The simulator cannot reproduce the defect M3-T07 fixed**, and that is worth knowing
    /// rather than working around: it holds an `RaDec` and moves it, so there are no counters, no
    /// `0x800000`, and nothing that can be "at the pole while an axis is 215° from home". A
    /// simulator test could never have caught this. The mount's own counters had to.
    pub home: RaDec,
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
            home: north_celestial_pole(),
        }
    }
}

/// `0h +90°` — where a northern mount's home pose points.
///
/// Not a `const`, because [`RaDec`] is built through a validating constructor. The fallback is
/// unreachable (`0h +90°` is a coordinate) and is written as one rather than an `expect` so that
/// no panic exists on a path a driver constructor runs.
fn north_celestial_pole() -> RaDec {
    RaDec::from_parts(0.0, 90.0).unwrap_or_else(|_| unreachable!("0h +90° is a coordinate"))
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

// -----------------------------------------------------------------------------------------
// The camera (Canon EOS R10 — spikes/gphoto2-r10/FINDINGS.md, PRD §4.3/§8.3)
// -----------------------------------------------------------------------------------------

/// Arcseconds subtended by one radian, to four more digits than anyone needs. Converts a pixel
/// pitch and a focal length into a plate scale.
const ARCSEC_PER_RADIAN: f64 = 206_264.806_247_096_36;

/// Plate scale in arcseconds per pixel for a sensor of `pitch_um` behind `focal_mm`.
#[must_use]
pub fn arcsec_per_pixel(pitch_um: f64, focal_mm: f64) -> f64 {
    ARCSEC_PER_RADIAN * (pitch_um / 1000.0) / focal_mm
}

/// What the simulated imaging camera is, and how long it takes to do things (PRD §4.5).
///
/// The timings are the R10 as the M2 spike measured it; the *optical* numbers are the reference
/// rig of PRD §8.3; the *sensor noise* numbers are invented and say so. As with
/// [`SimulatorProfile`], every field is public with a `Default`, so a test that cares about one
/// knob writes `CameraProfile { download: …, ..Default::default() }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraProfile {
    /// Frame width in pixels. 6000 on the reference body (PRD §8.3); tests use far less,
    /// because a full-size frame is 24 megapixels of arithmetic per exposure.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Plate scale after the telescope. 0.77″/px on the reference rig — 3.72 µm pixels behind
    /// 1000 mm (PRD §8.3, which states the same figure).
    pub arcsec_per_pixel: f64,
    /// Pixel pitch in micrometres, for the FITS header and the capability report. **Measured**
    /// (PRD §4.3: 22.3 mm APS-C across 6000 px).
    pub pixel_size_um: f64,
    /// Focal length in millimetres — PRD §8.3's `sw200pds-r10-none` profile.
    pub focal_length_mm: f64,

    /// Opening the camera. **Measured**: autodetect + connect took 190–210 ms.
    pub connect: Duration,
    /// One setting read or write. **Measured**: 11 ms per write, 0.4–10 ms per read.
    pub setting: Duration,
    /// Reading the whole settings tree. **Measured**: 91 config entries in 222 ms, which is why
    /// [`Camera::available_settings`](astroctl_hal::camera::Camera::available_settings) has a
    /// `config_seconds` timeout in the first place.
    pub settings_tree: Duration,
    /// What a capture costs beyond the exposure itself — the "download".
    ///
    /// **Measured, and not where it looks.** The spike timed a capture at 2.08 s
    /// trigger-to-file-ready on a short exposure, then wrote the 32 MB to disk in 2.67 ms. With
    /// `capturetarget=Internal RAM` the USB transfer happens *inside* the capture call, so the
    /// two seconds are the transfer and the disk write is nothing. The simulator charges them
    /// after the exposure, where a caller can see them, because that is where they are on a body
    /// configured to download straight to the host — and because T-ISO-1 (SDD §9) is specified
    /// as "a realistic ~2 s blocking capture and a slow download" and needs somewhere to put the
    /// slow download.
    pub download: Duration,
    /// Live-view frames per second.
    ///
    /// **Not the measurement**, deliberately. The R10 sustains 58.5 fps (measured, 133 KB per
    /// frame), but SDD §5.7 rate-limits the stream to 5 fps on a LAN and every synthetic frame
    /// costs real CPU — so a simulator defaulting to 58.5 would spend eleven times the node's
    /// budget generating frames the pipeline then throws away. A test that wants the body's own
    /// cadence sets it.
    pub live_view_fps: f64,
    /// Live-view frame width. The spike measured the *size* of a frame (133 KB) but never its
    /// dimensions; 960×640 keeps the sensor's 3:2 aspect at a size a JPEG of that order fits.
    pub live_view_width: u32,
    /// Live-view frame height.
    pub live_view_height: u32,

    /// Seeing, as the FWHM of a star image in arcseconds. **Invented**: 3″ is an ordinary night
    /// at a UK back-garden site, and at 0.77″/px it puts a star across four pixels, which is
    /// what a centroid needs to be worth computing.
    pub fwhm_arcsec: f64,
    /// Sky brightness in electrons per pixel per second. **Invented**: chosen so a 30 s sub sits
    /// near 600 e⁻ — a sky-limited exposure, which is what a light-polluted site actually gives
    /// and what makes the background dominate the read noise.
    pub sky_electrons_per_second: f64,
    /// Read noise in electrons RMS. **Invented**, but in the right place: a modern APS-C CMOS is
    /// 1.5–4 e⁻ depending on ISO.
    pub read_noise_electrons: f64,
    /// Bias pedestal in ADU. **Invented**; the R10's own black level is 2047 (measured by
    /// `rawler` in the spike), and the simulator uses a round 512 because its samples are 16-bit
    /// rather than the R10's 14.
    pub bias_adu: f64,
    /// Full well in electrons. **Invented**: 30,000 e⁻ is typical for a 3.7 µm pixel, and it is
    /// what makes bright stars saturate — a simulator whose stars never clip would let a
    /// saturation-aware stacker be written and never exercised.
    pub full_well_electrons: f64,
    /// Sensor temperature, or `None` for a body that reports none.
    ///
    /// The R10 reports no sensor temperature over PTP, so the honest default is `None` — and
    /// that makes the simulator the one place the `Option` arm of
    /// [`Camera::sensor_temperature_celsius`](astroctl_hal::camera::Camera::sensor_temperature_celsius)
    /// is exercised before there is hardware.
    pub sensor_temperature_celsius: Option<f64>,

    /// The sky this camera looks at.
    pub field: StarField,
    /// Where the tube points when no [`PointingSource`](super::sky::PointingSource) can say —
    /// no mount configured, or a mount that is not connected. M42, because a simulator that
    /// opens on an empty patch of sky looks broken.
    pub default_pointing: RaDec,
}

impl Default for CameraProfile {
    fn default() -> Self {
        Self {
            width: 6000,
            height: 4000,
            arcsec_per_pixel: arcsec_per_pixel(3.72, 1000.0),
            pixel_size_um: 3.72,
            focal_length_mm: 1000.0,
            connect: Duration::from_millis(200),
            setting: Duration::from_millis(11),
            settings_tree: Duration::from_millis(222),
            download: Duration::from_millis(2000),
            live_view_fps: 5.0,
            live_view_width: 960,
            live_view_height: 640,
            fwhm_arcsec: 3.0,
            sky_electrons_per_second: 20.0,
            read_noise_electrons: 3.0,
            bias_adu: 512.0,
            full_well_electrons: 30_000.0,
            sensor_temperature_celsius: None,
            field: StarField::default(),
            // 05h35m −05°23′ — the Orion Nebula. Cannot fail; both components are in range.
            default_pointing: RaDec::from_parts(5.5833, -5.3911)
                .expect("M42 is a valid coordinate"),
        }
    }
}

impl CameraProfile {
    /// A small, instant camera — for tests whose subject is not the camera.
    ///
    /// 128×96 frames and no latency anywhere. Anything whose subject *is* timing must use
    /// [`Default`] and a paused clock, and anything whose subject is a frame must at least
    /// enlarge this, because a 128-pixel frame at the default plate scale sees a hundredth of a
    /// square degree and may honestly contain no stars.
    #[must_use]
    pub fn fast() -> Self {
        Self {
            width: 128,
            height: 96,
            connect: Duration::ZERO,
            setting: Duration::ZERO,
            settings_tree: Duration::ZERO,
            download: Duration::ZERO,
            live_view_width: 64,
            live_view_height: 48,
            ..Self::default()
        }
    }
}

/// What the simulated guide camera is (PRD §4.5, HAL-04).
///
/// **Nothing here is measured.** There is no guide camera in the project yet and no spike has
/// touched one, so every number is an invented stand-in for a small mono CMOS on a short guide
/// scope — the ASI120MM-and-50-mm-finder combination most of amateur astronomy guides with. The
/// values are chosen to be *representative* rather than precise, and the one that matters is the
/// plate scale: at 3.2″/px a guide camera is four times coarser than the imaging chain, which is
/// what makes sub-pixel centroiding necessary rather than optional.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuideCameraProfile {
    /// Unbinned sensor width.
    pub width: u32,
    /// Unbinned sensor height.
    pub height: u32,
    /// Pixel pitch in micrometres.
    pub pixel_size_um: f64,
    /// Guide scope focal length in millimetres.
    pub focal_length_mm: f64,
    /// Plate scale, arcseconds per unbinned pixel.
    pub arcsec_per_pixel: f64,
    /// Highest gain the camera accepts, in its own units.
    pub max_gain: u32,
    /// Bits actually filled. 12 is what a small CMOS guider gives, and it is why a guide frame
    /// saturates at 4095 rather than 65535.
    pub bit_depth: u8,
    /// Largest binning factor on either axis.
    pub max_binning: u8,
    /// Shortest accepted exposure, in seconds.
    pub min_exposure_seconds: f64,
    /// Longest accepted exposure, in seconds.
    pub max_exposure_seconds: f64,
    /// Readout and USB transfer for one frame — the floor under the frame interval, and the
    /// reason a 0.5 s guide exposure is not a 2 Hz loop.
    pub readout: Duration,
    /// Opening the camera.
    pub connect: Duration,

    /// Atmospheric seeing as the guide loop meets it: the standard deviation, in arcseconds, of
    /// the whole field's position from one frame to the next.
    ///
    /// This is *not* the same quantity as [`fwhm_arcsec`](Self::fwhm_arcsec), and conflating them
    /// is the mistake this comment exists to prevent. FWHM is how big a star is in a long
    /// exposure — the time-average of the wander. This is how far the star moves between short
    /// exposures, which is what a guide algorithm chases and what a mount cannot correct faster
    /// than. 1.5″ RMS is ordinary seeing.
    pub seeing_arcsec: f64,
    /// Star size in a guide frame, FWHM in arcseconds.
    pub fwhm_arcsec: f64,
    /// Brightness of the star the simulator guarantees at the field centre.
    ///
    /// PRD §4.5 asks for this knob by name. It exists because a guide star drawn from the
    /// catalogue would have a brightness that depends on where the mount is pointed, which would
    /// make a guiding test a test of the catalogue's luck.
    pub guide_star_magnitude: Option<f64>,
    /// Sky brightness in electrons per pixel per second.
    pub sky_electrons_per_second: f64,
    /// Read noise in electrons RMS. Higher than the imaging sensor's, as a small guider's is.
    pub read_noise_electrons: f64,
    /// Bias pedestal in ADU.
    pub bias_adu: f64,
    /// Full well in electrons.
    pub full_well_electrons: f64,
    /// Sensor temperature, or `None`. An uncooled guider that reports a reading is common, so
    /// this defaults to a number: it is the arm of the API a cooled-camera path will need.
    pub sensor_temperature_celsius: Option<f64>,
    /// The sky it looks at — the same value the imaging camera holds, which is what makes the
    /// two cameras agree (see [`StarField`]).
    pub field: StarField,
    /// Where it looks when nothing can say.
    pub default_pointing: RaDec,
}

impl Default for GuideCameraProfile {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 960,
            pixel_size_um: 3.75,
            focal_length_mm: 240.0,
            arcsec_per_pixel: arcsec_per_pixel(3.75, 240.0),
            max_gain: 100,
            bit_depth: 12,
            max_binning: 4,
            min_exposure_seconds: 0.001,
            max_exposure_seconds: 60.0,
            readout: Duration::from_millis(60),
            connect: Duration::from_millis(120),
            seeing_arcsec: 1.5,
            fwhm_arcsec: 3.5,
            guide_star_magnitude: Some(8.0),
            sky_electrons_per_second: 12.0,
            read_noise_electrons: 5.0,
            bias_adu: 100.0,
            full_well_electrons: 14_000.0,
            sensor_temperature_celsius: Some(11.5),
            field: StarField::default(),
            default_pointing: RaDec::from_parts(5.5833, -5.3911)
                .expect("M42 is a valid coordinate"),
        }
    }
}

impl GuideCameraProfile {
    /// A small, instant guide camera for tests that are not about the guider.
    #[must_use]
    pub fn fast() -> Self {
        Self {
            width: 128,
            height: 96,
            readout: Duration::ZERO,
            connect: Duration::ZERO,
            ..Self::default()
        }
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
    fn the_camera_profile_reproduces_the_equipment_profile_of_the_prd() {
        let camera = CameraProfile::default();
        // PRD §8.3's `sw200pds-r10-none` states 0.77 arcsec/px for these three numbers. If this
        // drifts, every simulated plate scale disagrees with the operator's own equipment sheet.
        assert_eq!(camera.width, 6000);
        assert_eq!(camera.height, 4000);
        assert!(
            (camera.arcsec_per_pixel - 0.77).abs() < 0.005,
            "plate scale is {}",
            camera.arcsec_per_pixel
        );
        // The measured 2.08 s capture overhead, rounded to the 2 s that T-ISO-1 (SDD §9) calls
        // "a realistic ~2 s blocking capture".
        assert_eq!(camera.download, Duration::from_secs(2));

        // The guide chain is deliberately coarser — four times, which is what makes the guide
        // loop's sub-pixel centroiding necessary rather than a nicety.
        let guide = GuideCameraProfile::default();
        assert!(
            (guide.arcsec_per_pixel / camera.arcsec_per_pixel - 4.2).abs() < 0.2,
            "the guider is {}x the imaging scale",
            guide.arcsec_per_pixel / camera.arcsec_per_pixel
        );
        // Both cameras must start from the same sky, or they are two simulators that happen to
        // be configured alike (task M1-T06: "the guide camera reads the same generator").
        assert_eq!(camera.field, guide.field);
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
