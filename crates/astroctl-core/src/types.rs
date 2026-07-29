//! Domain types — SDD §4.1 (coordinates, modes) and §5.1 (device data and capabilities).
//!
//! Two rules govern everything in this module and neither is negotiable:
//!
//! 1. **Constructors are the only way in.** Every quantity with a domain (`RaHours`,
//!    `DecDegrees`, `AltDegrees`, `AzDegrees`, `GuideRate`) is a newtype over a *private*
//!    `f64`. Out-of-range input is [`CoreError::InvalidCoordinate`] — never a silent clamp,
//!    never a silent wrap. The one exception is right ascension, which is *defined* modulo
//!    24 h and whose normalization is therefore mathematics rather than a repaired mistake;
//!    the same goes for azimuth modulo 360°.
//! 2. **No public `f64` field carries a coordinate.** Units live in the type (SDD §2, "make
//!    unit bugs unrepresentable"), so a value can only be read out through an accessor whose
//!    name states the unit (`.hours()`, `.degrees()`).
//!
//! Deserialization goes through the constructors too (`#[serde(try_from = "f64")]`): a JSON
//! body claiming `"dec": 91.0` is a deserialization *error*, not a value. A derived
//! `Deserialize` — which is what SDD §4.1 literally shows — would be a hole straight through
//! rule 1, since every value arriving over the API arrives that way.
//!
//! Out of scope here (SDD §5.1, `astroctl-planning`, Phase 2a): coordinate *transforms*.
//! Nothing in this module converts RA/DEC to alt/az; `AltAz` is only a container.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

// ---------------------------------------------------------------------------------------
// Coordinates and rates
// ---------------------------------------------------------------------------------------

/// Right ascension in hours, normalized to `[0, 24)` by the constructor (SDD §4.1).
///
/// Normalizing rather than rejecting is correct here: RA is an angle on a circle, so 25 h and
/// 1 h name the same direction. Non-finite input is still an error — `NaN` is not a direction.
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct RaHours(f64);

impl RaHours {
    /// Hours in a full turn of right ascension.
    pub const HOURS_PER_TURN: f64 = 24.0;

    /// Normalizes `hours` into `[0, 24)`.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if `hours` is `NaN` or infinite.
    pub fn new(hours: f64) -> Result<Self, CoreError> {
        if !hours.is_finite() {
            return Err(CoreError::invalid_coordinate(
                "right ascension",
                hours,
                "a finite number of hours",
            ));
        }
        let mut normalized = hours.rem_euclid(Self::HOURS_PER_TURN);
        // `rem_euclid` can return exactly 24.0 for a tiny negative input: -1e-16 + 24.0
        // rounds to 24.0 in f64. Half-open means half-open.
        if normalized >= Self::HOURS_PER_TURN {
            normalized = 0.0;
        }
        Ok(Self(normalized))
    }

    /// The value in hours, guaranteed to be in `[0, 24)`.
    #[must_use]
    pub const fn hours(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for RaHours {
    type Error = CoreError;
    fn try_from(hours: f64) -> Result<Self, Self::Error> {
        Self::new(hours)
    }
}

/// Declination in degrees, validated to `[-90, +90]` inclusive (SDD §4.1).
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct DecDegrees(f64);

impl DecDegrees {
    /// Lowest legal declination (the south celestial pole).
    pub const MIN: f64 = -90.0;
    /// Highest legal declination (the north celestial pole).
    pub const MAX: f64 = 90.0;

    /// Validates `degrees` against `[-90, +90]`.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if `degrees` is `NaN`, infinite or outside the range.
    /// The range is *not* wrapped: a declination of 91° is a bug in the caller, not a
    /// coordinate one degree past the pole.
    pub fn new(degrees: f64) -> Result<Self, CoreError> {
        if !degrees.is_finite() {
            return Err(CoreError::invalid_coordinate(
                "declination",
                degrees,
                "a finite number of degrees",
            ));
        }
        if !(Self::MIN..=Self::MAX).contains(&degrees) {
            return Err(CoreError::invalid_coordinate(
                "declination",
                degrees,
                "within [-90, +90] degrees",
            ));
        }
        Ok(Self(degrees))
    }

    /// The value in degrees, guaranteed to be in `[-90, +90]`.
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for DecDegrees {
    type Error = CoreError;
    fn try_from(degrees: f64) -> Result<Self, Self::Error> {
        Self::new(degrees)
    }
}

/// Altitude above the horizon in degrees, validated to `[-90, +90]` inclusive.
///
/// Negative altitudes are legal and meaningful — a target below the horizon has one, and the
/// altitude limit (SDD §5.4) exists precisely to refuse it.
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct AltDegrees(f64);

impl AltDegrees {
    /// Lowest legal altitude (straight down).
    pub const MIN: f64 = -90.0;
    /// Highest legal altitude (the zenith).
    pub const MAX: f64 = 90.0;

    /// Validates `degrees` against `[-90, +90]`.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if `degrees` is `NaN`, infinite or outside the range.
    pub fn new(degrees: f64) -> Result<Self, CoreError> {
        if !degrees.is_finite() {
            return Err(CoreError::invalid_coordinate(
                "altitude",
                degrees,
                "a finite number of degrees",
            ));
        }
        if !(Self::MIN..=Self::MAX).contains(&degrees) {
            return Err(CoreError::invalid_coordinate(
                "altitude",
                degrees,
                "within [-90, +90] degrees",
            ));
        }
        Ok(Self(degrees))
    }

    /// The value in degrees, guaranteed to be in `[-90, +90]`.
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for AltDegrees {
    type Error = CoreError;
    fn try_from(degrees: f64) -> Result<Self, Self::Error> {
        Self::new(degrees)
    }
}

/// Azimuth in degrees, normalized to `[0, 360)` by the constructor; 0° is north, 90° east.
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct AzDegrees(f64);

impl AzDegrees {
    /// Degrees in a full turn.
    pub const DEGREES_PER_TURN: f64 = 360.0;

    /// Normalizes `degrees` into `[0, 360)`.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if `degrees` is `NaN` or infinite.
    pub fn new(degrees: f64) -> Result<Self, CoreError> {
        if !degrees.is_finite() {
            return Err(CoreError::invalid_coordinate(
                "azimuth",
                degrees,
                "a finite number of degrees",
            ));
        }
        let mut normalized = degrees.rem_euclid(Self::DEGREES_PER_TURN);
        if normalized >= Self::DEGREES_PER_TURN {
            normalized = 0.0;
        }
        Ok(Self(normalized))
    }

    /// The value in degrees, guaranteed to be in `[0, 360)`.
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for AzDegrees {
    type Error = CoreError;
    fn try_from(degrees: f64) -> Result<Self, Self::Error> {
        Self::new(degrees)
    }
}

/// Guide-pulse rate as a fraction of the sidereal rate, validated to `(0.0, 1.0]` (MNT-12).
///
/// Zero is rejected deliberately: a "guide pulse" that cannot move the mount is a silent
/// no-op, and a guiding loop correcting with zero rate looks exactly like a working one.
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct GuideRate(f64);

impl GuideRate {
    /// The full sidereal rate — the maximum a guide pulse may request.
    pub const MAX: f64 = 1.0;

    /// Validates `fraction` against `(0.0, 1.0]`.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if `fraction` is `NaN`, infinite, non-positive or
    /// greater than 1.0.
    pub fn new(fraction: f64) -> Result<Self, CoreError> {
        if !fraction.is_finite() {
            return Err(CoreError::invalid_coordinate(
                "guide rate",
                fraction,
                "a finite fraction of the sidereal rate",
            ));
        }
        if fraction <= 0.0 || fraction > Self::MAX {
            return Err(CoreError::invalid_coordinate(
                "guide rate",
                fraction,
                "within (0.0, 1.0] of the sidereal rate",
            ));
        }
        Ok(Self(fraction))
    }

    /// The value as a fraction of sidereal, guaranteed to be in `(0.0, 1.0]`.
    #[must_use]
    pub const fn fraction(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for GuideRate {
    type Error = CoreError;
    fn try_from(fraction: f64) -> Result<Self, Self::Error> {
        Self::new(fraction)
    }
}

// ---------------------------------------------------------------------------------------
// Coordinate pairs
// ---------------------------------------------------------------------------------------

/// An equatorial coordinate pair (SDD §4.1).
#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaDec {
    /// Right ascension.
    pub ra: RaHours,
    /// Declination.
    pub dec: DecDegrees,
}

impl RaDec {
    /// Builds a pair from already-validated components.
    #[must_use]
    pub const fn new(ra: RaHours, dec: DecDegrees) -> Self {
        Self { ra, dec }
    }

    /// Builds a pair from raw numbers, validating both.
    ///
    /// The API layer's `{ra_hours, dec_degrees}` request bodies land here.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if either component is invalid.
    pub fn from_parts(ra_hours: f64, dec_degrees: f64) -> Result<Self, CoreError> {
        Ok(Self::new(
            RaHours::new(ra_hours)?,
            DecDegrees::new(dec_degrees)?,
        ))
    }
}

/// A horizontal coordinate pair (SDD §4.1).
///
/// Populated by the safety monitor's topocentric transform (SDD §5.4, M1-T05), never here.
#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub struct AltAz {
    /// Altitude above the horizon.
    pub alt: AltDegrees,
    /// Azimuth, measured from north through east.
    pub az: AzDegrees,
}

impl AltAz {
    /// Builds a pair from already-validated components.
    #[must_use]
    pub const fn new(alt: AltDegrees, az: AzDegrees) -> Self {
        Self { alt, az }
    }

    /// Builds a pair from raw numbers, validating both.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if either component is invalid.
    pub fn from_parts(alt_degrees: f64, az_degrees: f64) -> Result<Self, CoreError> {
        Ok(Self::new(
            AltDegrees::new(alt_degrees)?,
            AzDegrees::new(az_degrees)?,
        ))
    }
}

// ---------------------------------------------------------------------------------------
// Astronomical notation (PRD USB-05)
// ---------------------------------------------------------------------------------------

/// Tenths of a second of time in 24 hours — the resolution `RaHours` renders at.
const TENTHS_PER_TURN: i64 = 24 * 60 * 60 * 10;
/// Arcseconds in a full turn — the resolution the degree types render at.
const ARCSEC_PER_TURN: i64 = 360 * 60 * 60;

/// `HH:MM:SS.s`, e.g. `05:55:15.6`.
///
/// Rounding carries: 23h59m59.99s renders as `00:00:00.0`, never `24:00:00.0`.
impl fmt::Display for RaHours {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = (self.0 * 36_000.0).round() as i64;
        let tenths = tenths.rem_euclid(TENTHS_PER_TURN);
        write!(
            f,
            "{:02}:{:02}:{:02}.{}",
            tenths / 36_000,
            (tenths / 600) % 60,
            (tenths / 10) % 60,
            tenths % 10
        )
    }
}

/// Renders a signed angle as `±DD°MM'SS"`; shared by declination and altitude.
fn write_signed_dms(f: &mut fmt::Formatter<'_>, degrees: f64) -> fmt::Result {
    let sign = if degrees < 0.0 { '-' } else { '+' };
    let arcsec = (degrees.abs() * 3600.0).round() as i64;
    write!(
        f,
        "{}{:02}°{:02}'{:02}\"",
        sign,
        arcsec / 3600,
        (arcsec / 60) % 60,
        arcsec % 60
    )
}

/// `±DD°MM'SS"`, e.g. `+07°24'25"` (PRD USB-05).
impl fmt::Display for DecDegrees {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_signed_dms(f, self.0)
    }
}

/// `±DD°MM'SS"`, e.g. `-05°00'00"`.
impl fmt::Display for AltDegrees {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_signed_dms(f, self.0)
    }
}

/// `DDD°MM'SS"`, e.g. `127°30'00"` — unsigned, three digits, wrapping at 360°.
impl fmt::Display for AzDegrees {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let arcsec = (self.0 * 3600.0).round() as i64;
        let arcsec = arcsec.rem_euclid(ARCSEC_PER_TURN);
        write!(
            f,
            "{:03}°{:02}'{:02}\"",
            arcsec / 3600,
            (arcsec / 60) % 60,
            arcsec % 60
        )
    }
}

/// `HH:MM:SS.s ±DD°MM'SS"`.
impl fmt::Display for RaDec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.ra, self.dec)
    }
}

/// `alt ±DD°MM'SS" az DDD°MM'SS"`.
impl fmt::Display for AltAz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "alt {} az {}", self.alt, self.az)
    }
}

// ---------------------------------------------------------------------------------------
// Motion enums (SDD §4.1)
// ---------------------------------------------------------------------------------------

/// Tracking rate class (SDD §4.1).
///
/// "Off" is not a mode: the API's `{mode: "sidereal"|"lunar"|"solar"|"off"}` body (SDD §5.8.1)
/// maps `"off"` to `stop_tracking()`, so the absence of tracking is `Option::None` here.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackingMode {
    /// Star rate.
    Sidereal,
    /// Moon rate.
    Lunar,
    /// Sun rate.
    Solar,
}

/// A mount axis (SDD §4.1).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Right-ascension axis (Synta axis 1).
    Ra,
    /// Declination axis (Synta axis 2).
    Dec,
}

/// A slew direction (SDD §4.1).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Increasing declination.
    North,
    /// Decreasing declination.
    South,
    /// Increasing right ascension.
    East,
    /// Decreasing right ascension.
    West,
}

/// Manual-slew speed class (SDD §4.1); concrete rates are a driver concern.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlewSpeed {
    /// Guide rate — the slowest class.
    Guide,
    /// Slow centering rate.
    Slow,
    /// Medium rate.
    Medium,
    /// Fast rate.
    Fast,
    /// The mount's maximum rate.
    Max,
}

/// Which side of the pier the optical tube is on (SDD §4.3 `mount.position`, §5.2.3).
///
/// Derived from the declination axis counter, never commanded directly.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PierSide {
    /// Tube east of the pier — the normal state for a target west of the meridian.
    East,
    /// Tube west of the pier.
    West,
}

// ---------------------------------------------------------------------------------------
// Device data (SDD §5.1)
// ---------------------------------------------------------------------------------------

/// Which kind of device an error or event came from.
///
/// Exists so the error mapping can pick a device-qualified code (`MOUNT_TIMEOUT` per SDD
/// §4.2) from a device-agnostic [`crate::error::DeviceError`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// The mount.
    Mount,
    /// The imaging camera.
    Camera,
    /// The guide camera (Phase 3).
    GuideCamera,
}

/// Identity of a connected device (SDD §5.1 `device_info`, PRD §4.1).
///
/// `firmware` and `serial` are optional because the two device families report different
/// things: PRD §4.1 gives the mount `(name, model, firmware, protocol)` and the camera
/// `(name, model, serial, protocol)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Human-readable device name, e.g. `"Skywatcher HEQ5 Pro"`.
    pub name: String,
    /// Model identifier as reported by the device.
    pub model: String,
    /// Firmware version string, when the device reports one.
    pub firmware: Option<String>,
    /// Serial number, when the device reports one.
    pub serial: Option<String>,
    /// Transport/protocol name, e.g. `"synta-serial"`, `"gphoto2"`.
    pub protocol: String,
}

/// Camera battery state (SDD §5.1, PRD §4.1).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatteryStatus {
    /// Charge level in percent, `0..=100`.
    pub percent: u8,
    /// Whether the camera reports being charged (mains/USB power).
    pub charging: bool,
}

/// Camera storage state (PRD §4.1 `get_storage_info`, SDD §5.8.1 `/api/camera/storage`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageInfo {
    /// Free space on the camera's card, in megabytes.
    pub free_mb: u64,
    /// Total size of the camera's card, in megabytes.
    pub total_mb: u64,
}

/// Coarse mount lifecycle state, the `state` field of the `mount.status` event (SDD §4.3).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountState {
    /// No transport open.
    Disconnected,
    /// Connected, motors idle, not tracking.
    Idle,
    /// A goto or manual slew is in progress.
    Slewing,
    /// Tracking at one of the [`TrackingMode`] rates.
    Tracking,
    /// Moving to the park position.
    Parking,
    /// Parked.
    Parked,
    /// Communication degraded or lost while connected (SDD §5.2.4 heartbeat misses).
    Fault,
}

/// Mount status (SDD §4.3 `mount.status`, §5.8.1 `/api/mount/status`).
///
/// The booleans are redundant with `state` by design — they are what the event payload of
/// SDD §4.3 specifies, and a UI that wants "is it moving?" should not have to know the state
/// machine. [`MountStatus::is_consistent`] is the invariant between them.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountStatus {
    /// Coarse lifecycle state.
    pub state: MountState,
    /// Active tracking rate, or `None` when tracking is off.
    pub tracking: Option<TrackingMode>,
    /// Whether a goto or manual slew is in progress.
    pub slewing: bool,
    /// Whether the mount is parked.
    pub parked: bool,
}

impl MountStatus {
    /// The status of a mount that is not connected.
    #[must_use]
    pub const fn disconnected() -> Self {
        Self {
            state: MountState::Disconnected,
            tracking: None,
            slewing: false,
            parked: false,
        }
    }

    /// Whether the mount is tracking.
    #[must_use]
    pub const fn is_tracking(&self) -> bool {
        self.tracking.is_some()
    }

    /// Whether `state` agrees with the three flags.
    ///
    /// Drivers assert this before publishing; a status where `state` says `Tracking` and
    /// `tracking` is `None` would make the UI and the log disagree about the same instant.
    #[must_use]
    pub const fn is_consistent(&self) -> bool {
        match self.state {
            MountState::Slewing => self.slewing && !self.parked,
            MountState::Tracking => self.tracking.is_some() && !self.slewing && !self.parked,
            MountState::Parked => self.parked && !self.slewing && self.tracking.is_none(),
            MountState::Parking => self.slewing && !self.parked,
            MountState::Idle => !self.slewing && !self.parked && self.tracking.is_none(),
            MountState::Disconnected | MountState::Fault => true,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Capabilities (SDD §5.1)
// ---------------------------------------------------------------------------------------

/// What a mount driver can do (SDD §5.1, verbatim).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountCapabilities {
    /// Periodic-error correction available.
    pub has_pec: bool,
    /// Pulse guiding available (MNT-12).
    pub has_pulse_guide: bool,
    /// Tracking rates the mount supports.
    pub tracking_rates: Vec<TrackingMode>,
    /// Maximum slew rate as a multiple of sidereal.
    pub max_slew_speed_x_sidereal: u32,
    /// Bits of resolution in the axis position counters.
    pub position_resolution_bits: u8,
}

/// Image format a camera can produce (PRD §4.1, §8.1 `camera.default_format`).
///
/// The serialized spellings are the ones PRD §8.1 uses in config, so a config value and an
/// API value are the same string.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageFormat {
    /// Camera raw only (CR3 on the reference body).
    #[serde(rename = "RAW")]
    Raw,
    /// JPEG only.
    #[serde(rename = "JPEG")]
    Jpeg,
    /// Both files per exposure.
    #[serde(rename = "RAW+JPEG")]
    RawPlusJpeg,
}

/// What a camera driver can do (PRD §4.1; SDD §5.1 "same pattern" as [`MountCapabilities`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraCapabilities {
    /// Bulb exposures available (CAM-04).
    pub has_bulb: bool,
    /// Live view available (CAM-05).
    pub has_live_view: bool,
    /// Mirror lockup available.
    pub has_mirror_lockup: bool,
    /// Sensor width in pixels.
    pub sensor_width_px: u32,
    /// Sensor height in pixels.
    pub sensor_height_px: u32,
    /// Pixel pitch in micrometres.
    pub pixel_size_um: f64,
    /// Formats the body can be set to.
    pub supported_formats: Vec<ImageFormat>,
    /// Lowest selectable ISO.
    pub min_iso: u32,
    /// Highest selectable ISO.
    pub max_iso: u32,
    /// Shortest selectable shutter time, in seconds.
    pub min_shutter_s: f64,
    /// Longest selectable non-bulb shutter time, in seconds.
    pub max_shutter_s: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- RaHours normalization -------------------------------------------------------

    #[test]
    fn ra_normalizes_into_half_open_range() {
        for (input, expected) in [
            (0.0, 0.0),
            (23.5, 23.5),
            (24.0, 0.0),
            (25.0, 1.0),
            (48.0, 0.0),
            (-1.0, 23.0),
            (-24.0, 0.0),
            (-25.0, 23.0),
        ] {
            let ra = RaHours::new(input).expect("finite input is valid");
            assert_eq!(ra.hours(), expected, "RaHours::new({input})");
            assert!(ra.hours() >= 0.0 && ra.hours() < 24.0);
        }
    }

    #[test]
    fn ra_tiny_negative_does_not_land_on_twentyfour() {
        // `(-1e-16).rem_euclid(24.0)` is exactly 24.0 in f64 — the half-open range would be
        // violated by the obvious one-liner.
        let ra = RaHours::new(-1e-16).expect("finite input is valid");
        assert!(ra.hours() < 24.0, "got {}", ra.hours());
        assert_eq!(ra.hours(), 0.0);
    }

    #[test]
    fn ra_rejects_non_finite() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = RaHours::new(bad).expect_err("non-finite must be rejected");
            assert!(matches!(err, CoreError::InvalidCoordinate { .. }));
        }
    }

    // --- DecDegrees validation -------------------------------------------------------

    #[test]
    fn dec_accepts_inclusive_bounds() {
        for good in [-90.0, -89.999, 0.0, 45.5, 89.999, 90.0] {
            let dec = DecDegrees::new(good).expect("in range");
            assert_eq!(dec.degrees(), good);
        }
    }

    #[test]
    fn dec_rejects_out_of_range_without_clamping() {
        for bad in [-90.000_001, 90.000_001, 91.0, -180.0, 270.0] {
            let err = DecDegrees::new(bad).expect_err("out of range must be rejected");
            assert!(matches!(err, CoreError::InvalidCoordinate { .. }), "{bad}");
        }
    }

    #[test]
    fn dec_rejects_non_finite() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(DecDegrees::new(bad).is_err(), "{bad}");
        }
    }

    // --- Altitude / azimuth ----------------------------------------------------------

    #[test]
    fn alt_validates_and_az_normalizes() {
        assert!(AltDegrees::new(-90.0).is_ok());
        assert!(AltDegrees::new(90.0).is_ok());
        assert!(AltDegrees::new(90.001).is_err());
        assert!(AltDegrees::new(f64::NAN).is_err());

        assert_eq!(AzDegrees::new(0.0).expect("valid").degrees(), 0.0);
        assert_eq!(AzDegrees::new(360.0).expect("valid").degrees(), 0.0);
        assert_eq!(AzDegrees::new(-90.0).expect("valid").degrees(), 270.0);
        assert_eq!(AzDegrees::new(450.0).expect("valid").degrees(), 90.0);
        assert!(AzDegrees::new(-1e-16).expect("valid").degrees() < 360.0);
        assert!(AzDegrees::new(f64::INFINITY).is_err());
    }

    // --- GuideRate -------------------------------------------------------------------

    #[test]
    fn guide_rate_is_a_half_open_fraction_of_sidereal() {
        for good in [f64::MIN_POSITIVE, 0.1, 0.5, 0.9, 1.0] {
            assert!(GuideRate::new(good).is_ok(), "{good}");
        }
        for bad in [0.0, -0.0, -0.5, 1.000_001, 2.0, f64::NAN, f64::INFINITY] {
            assert!(GuideRate::new(bad).is_err(), "{bad}");
        }
        assert_eq!(GuideRate::new(0.5).expect("valid").fraction(), 0.5);
    }

    // --- Display goldens (PRD USB-05) ------------------------------------------------

    #[test]
    fn ra_display_golden_cases() {
        for (hours, expected) in [
            (0.0, "00:00:00.0"),
            (12.5, "12:30:00.0"),
            (5.919_53, "05:55:10.3"),      // Betelgeuse
            (23.999_999_99, "00:00:00.0"), // rounding carries a full turn, never 24:00:00.0
            (1.0 / 36_000.0, "00:00:00.1"),
            (23.5, "23:30:00.0"),
        ] {
            let ra = RaHours::new(hours).expect("valid");
            assert_eq!(ra.to_string(), expected, "RaHours::new({hours})");
        }
    }

    #[test]
    fn dec_display_golden_cases() {
        for (degrees, expected) in [
            (0.0, "+00°00'00\""),
            (-0.0, "+00°00'00\""), // negative zero is still zero, not "-00°00'00\""
            (90.0, "+90°00'00\""),
            (-90.0, "-90°00'00\""),
            (7.407_056, "+07°24'25\""), // Betelgeuse
            (-0.5, "-00°30'00\""),
            (0.999_999_9, "+01°00'00\""), // carries; never "+00°59'60\""
        ] {
            let dec = DecDegrees::new(degrees).expect("valid");
            assert_eq!(dec.to_string(), expected, "DecDegrees::new({degrees})");
        }
    }

    #[test]
    fn az_display_is_three_digit_unsigned() {
        for (degrees, expected) in [
            (0.0, "000°00'00\""),
            (127.5, "127°30'00\""),
            (359.999_999_9, "000°00'00\""), // carries to 0, never 360°
            (90.25, "090°15'00\""),
        ] {
            let az = AzDegrees::new(degrees).expect("valid");
            assert_eq!(az.to_string(), expected, "AzDegrees::new({degrees})");
        }
    }

    #[test]
    fn pair_display_is_both_halves() {
        // Betelgeuse, as the mount panel would render it (PRD USB-05).
        let pos = RaDec::from_parts(5.919_53, 7.407_056).expect("valid");
        assert_eq!(pos.to_string(), "05:55:10.3 +07°24'25\"");
        let horiz = AltAz::from_parts(-5.0, 127.5).expect("valid");
        assert_eq!(horiz.to_string(), "alt -05°00'00\" az 127°30'00\"");
    }

    // --- serde -----------------------------------------------------------------------

    #[test]
    fn coordinates_serialize_as_bare_numbers() {
        let ra = RaHours::new(12.5).expect("valid");
        assert_eq!(serde_json::to_string(&ra).expect("serializes"), "12.5");
        let dec = DecDegrees::new(-30.25).expect("valid");
        assert_eq!(serde_json::to_string(&dec).expect("serializes"), "-30.25");
    }

    #[test]
    fn deserialization_goes_through_the_constructor() {
        // Validation, not a silent value: this is the hole a derived `Deserialize` leaves.
        assert!(serde_json::from_str::<DecDegrees>("91.0").is_err());
        assert!(serde_json::from_str::<DecDegrees>("-90.5").is_err());
        assert!(serde_json::from_str::<GuideRate>("0.0").is_err());
        assert!(serde_json::from_str::<GuideRate>("1.5").is_err());
        assert!(serde_json::from_str::<AltDegrees>("100.0").is_err());
        // ...and normalization applies on the way in, too.
        let ra: RaHours = serde_json::from_str("25.0").expect("normalizes");
        assert_eq!(ra.hours(), 1.0);
        let az: AzDegrees = serde_json::from_str("-90.0").expect("normalizes");
        assert_eq!(az.degrees(), 270.0);
    }

    #[test]
    fn radec_round_trips() {
        let pos = RaDec::from_parts(12.5, -30.25).expect("valid");
        let json = serde_json::to_string(&pos).expect("serializes");
        assert_eq!(json, r#"{"ra":12.5,"dec":-30.25}"#);
        let back: RaDec = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, pos);
    }

    #[test]
    fn altaz_round_trips_with_event_field_names() {
        // SDD §4.3 `mount.position` carries `alt`/`az`.
        let horiz = AltAz::from_parts(45.0, 180.0).expect("valid");
        let json = serde_json::to_string(&horiz).expect("serializes");
        assert_eq!(json, r#"{"alt":45.0,"az":180.0}"#);
        assert_eq!(
            serde_json::from_str::<AltAz>(&json).expect("deserializes"),
            horiz
        );
    }

    /// Serializes `value` and asserts it is a JSON string, returning it.
    fn wire(value: impl Serialize) -> String {
        match serde_json::to_value(value).expect("serializes") {
            serde_json::Value::String(s) => s,
            other => panic!("expected a JSON string, got {other}"),
        }
    }

    #[test]
    fn enums_serialize_as_the_documented_strings() {
        assert_eq!(wire(TrackingMode::Sidereal), "sidereal");
        assert_eq!(wire(TrackingMode::Lunar), "lunar");
        assert_eq!(wire(TrackingMode::Solar), "solar");
        assert_eq!(wire(Axis::Ra), "ra");
        assert_eq!(wire(Axis::Dec), "dec");
        assert_eq!(wire(Direction::North), "north");
        assert_eq!(wire(Direction::South), "south");
        assert_eq!(wire(Direction::East), "east");
        assert_eq!(wire(Direction::West), "west");
        assert_eq!(wire(SlewSpeed::Guide), "guide");
        assert_eq!(wire(SlewSpeed::Slow), "slow");
        assert_eq!(wire(SlewSpeed::Medium), "medium");
        assert_eq!(wire(SlewSpeed::Fast), "fast");
        assert_eq!(wire(SlewSpeed::Max), "max");
        assert_eq!(wire(PierSide::East), "east");
        assert_eq!(wire(PierSide::West), "west");
        assert_eq!(wire(MountState::Disconnected), "disconnected");
        assert_eq!(wire(MountState::Parking), "parking");
        assert_eq!(wire(DeviceKind::GuideCamera), "guide_camera");
        // PRD §8.1 `camera.default_format: "RAW+JPEG"` — config and API agree.
        assert_eq!(wire(ImageFormat::Raw), "RAW");
        assert_eq!(wire(ImageFormat::Jpeg), "JPEG");
        assert_eq!(wire(ImageFormat::RawPlusJpeg), "RAW+JPEG");
        // Round-trip the odd one out, since its spelling is not derivable from the variant.
        assert_eq!(
            serde_json::from_str::<ImageFormat>("\"RAW+JPEG\"").expect("deserializes"),
            ImageFormat::RawPlusJpeg
        );
    }

    #[test]
    fn mount_status_round_trips_and_matches_the_event_payload() {
        let status = MountStatus {
            state: MountState::Tracking,
            tracking: Some(TrackingMode::Sidereal),
            slewing: false,
            parked: false,
        };
        let json = serde_json::to_value(status).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "state": "tracking", "tracking": "sidereal",
                "slewing": false, "parked": false
            })
        );
        assert_eq!(
            serde_json::from_value::<MountStatus>(json).expect("deserializes"),
            status
        );
        assert!(status.is_consistent() && status.is_tracking());

        let idle = MountStatus::disconnected();
        assert!(!idle.is_tracking());
        assert_eq!(
            serde_json::to_value(idle).expect("serializes")["tracking"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn mount_status_consistency_catches_contradictions() {
        let lying = MountStatus {
            state: MountState::Tracking,
            tracking: None,
            slewing: false,
            parked: false,
        };
        assert!(!lying.is_consistent());
        let also_lying = MountStatus {
            state: MountState::Parked,
            tracking: None,
            slewing: true,
            parked: true,
        };
        assert!(!also_lying.is_consistent());
    }

    #[test]
    fn device_data_round_trips() {
        let info = DeviceInfo {
            name: "Skywatcher HEQ5 Pro".to_owned(),
            model: "HEQ5".to_owned(),
            firmware: Some("2.11".to_owned()),
            serial: None,
            protocol: "synta-serial".to_owned(),
        };
        let json = serde_json::to_string(&info).expect("serializes");
        assert_eq!(
            serde_json::from_str::<DeviceInfo>(&json).expect("deserializes"),
            info
        );

        let battery = BatteryStatus {
            percent: 87,
            charging: false,
        };
        assert_eq!(
            serde_json::to_value(battery).expect("serializes"),
            serde_json::json!({"percent": 87, "charging": false})
        );

        let storage = StorageInfo {
            free_mb: 12_000,
            total_mb: 64_000,
        };
        assert_eq!(
            serde_json::from_value::<StorageInfo>(
                serde_json::to_value(storage).expect("serializes")
            )
            .expect("deserializes"),
            storage
        );
    }

    #[test]
    fn capabilities_round_trip() {
        let mount = MountCapabilities {
            has_pec: false,
            has_pulse_guide: true,
            tracking_rates: vec![TrackingMode::Sidereal, TrackingMode::Lunar],
            max_slew_speed_x_sidereal: 800,
            position_resolution_bits: 24,
        };
        let json = serde_json::to_string(&mount).expect("serializes");
        assert_eq!(
            serde_json::from_str::<MountCapabilities>(&json).expect("deserializes"),
            mount
        );

        let camera = CameraCapabilities {
            has_bulb: true,
            has_live_view: true,
            has_mirror_lockup: false,
            sensor_width_px: 6000,
            sensor_height_px: 4000,
            pixel_size_um: 3.72,
            supported_formats: vec![ImageFormat::Raw, ImageFormat::Jpeg],
            min_iso: 100,
            max_iso: 32_000,
            min_shutter_s: 1.0 / 4000.0,
            max_shutter_s: 30.0,
        };
        let json = serde_json::to_string(&camera).expect("serializes");
        assert_eq!(
            serde_json::from_str::<CameraCapabilities>(&json).expect("deserializes"),
            camera
        );
    }
}
