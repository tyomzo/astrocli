//! The guide camera interface — PRD §4.1, HAL-04.
//!
//! Read the crate-level "contract every device trait shares" first. What is specific here is the
//! shape of the data: a guide camera produces small 16-bit frames at high cadence, straight into
//! a star-detection loop, so frames are raw pixel buffers rather than files. Nothing about a
//! guide frame is written to disk in the normal path — it is measured, turned into a correction,
//! and discarded.
//!
//! The guide *loop* is Phase 3, but the trait and a simulator implementation ship in M1
//! (task M1-T06). That ordering is deliberate: an interface with no implementation is an
//! interface nobody has tried to satisfy, and the cost of finding out it is wrong rises with
//! every crate written against it in the meantime.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use astroctl_core::error::{CoreError, DeviceError};
use astroctl_core::types::{DeviceInfo, GuideCameraCapabilities};
use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};

use crate::stream::FrameStream;

/// Binning factors, at least 1 on each axis.
///
/// A newtype rather than a pair of `u8` parameters because zero is the interesting failure:
/// `set_binning(0, 0)` on a raw pair is a plausible typo that reaches the device as either "no
/// binning" or a division by zero, depending on the driver. Here it cannot be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binning {
    x: u8,
    y: u8,
}

impl Binning {
    /// Unbinned — one sensor pixel per output pixel.
    pub const NONE: Self = Self { x: 1, y: 1 };

    /// Builds a binning pair.
    ///
    /// Only the lower bound is checked here; the upper bound is per-device
    /// ([`GuideCameraCapabilities::max_binning`]) and is the driver's to enforce with
    /// [`DeviceError::Unsupported`].
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if either factor is zero.
    pub fn new(x: u8, y: u8) -> Result<Self, CoreError> {
        for (axis, value) in [("horizontal binning", x), ("vertical binning", y)] {
            if value == 0 {
                return Err(CoreError::invalid_coordinate(
                    axis,
                    f64::from(value),
                    "at least 1",
                ));
            }
        }
        Ok(Self { x, y })
    }

    /// Square binning, e.g. 2×2.
    ///
    /// # Errors
    /// [`CoreError::InvalidCoordinate`] if `factor` is zero.
    pub fn square(factor: u8) -> Result<Self, CoreError> {
        Self::new(factor, factor)
    }

    /// Horizontal factor.
    #[must_use]
    pub const fn x(self) -> u8 {
        self.x
    }

    /// Vertical factor.
    #[must_use]
    pub const fn y(self) -> u8 {
        self.y
    }
}

/// How the samples in a [`GuideFrame`] are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelLayout {
    /// One luminance sample per pixel — what a guide camera normally is.
    Mono,
    /// A colour filter array, with the pattern of the *unbinned, un-cropped* sensor. Binning
    /// destroys the mosaic, so a binned colour frame is reported as [`Mono`](Self::Mono).
    Bayer(BayerPattern),
}

/// Colour-filter-array order, named by the top-left 2×2 quad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BayerPattern {
    /// Red, green / green, blue.
    Rggb,
    /// Blue, green / green, red.
    Bggr,
    /// Green, red / blue, green.
    Grbg,
    /// Green, blue / red, green.
    Gbrg,
}

/// One guide exposure.
///
/// The samples are 16-bit regardless of the sensor's real depth — a 12-bit camera fills
/// `0..=4095` and [`GuideCameraCapabilities::bit_depth`] is what tells a star detector where
/// saturation is. Row-major, `width * height` samples, no padding
/// ([`is_consistent`](Self::is_consistent) is the invariant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuideFrame {
    /// Width in pixels, after binning.
    pub width: u32,
    /// Height in pixels, after binning.
    pub height: u32,
    /// Row-major samples. Behind an `Arc` because the guide loop hands the same frame to a
    /// detector, a display path and possibly a recorder without copying it.
    pub pixels: Arc<[u16]>,
    /// Sample layout.
    pub layout: PixelLayout,
    /// Binning in force for this exposure.
    pub binning: Binning,
    /// How long the shutter was open.
    pub exposure: Duration,
    /// Gain in force, in the camera's own units.
    pub gain: u32,
    /// When the exposure started.
    pub started_at: DateTime<Utc>,
}

impl GuideFrame {
    /// Whether the buffer length matches the stated dimensions.
    ///
    /// Implementors must uphold this; consumers may assert it. A frame that lies about its
    /// dimensions does not produce a wrong correction, it produces an out-of-bounds index in the
    /// centroid loop — which is a crash in the middle of a night's imaging.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.pixels.len() as u64 == u64::from(self.width) * u64::from(self.height)
    }

    /// The instant this frame's measurement refers to: the midpoint of the exposure.
    ///
    /// A star's position in a 2 s exposure is its average over those 2 s, so a correction
    /// derived from it belongs to the middle, not the start or the end. Carrying `started_at`
    /// plus `exposure` rather than one timestamp is what makes this derivable at all.
    #[must_use]
    pub fn midpoint(&self) -> DateTime<Utc> {
        TimeDelta::from_std(self.exposure / 2)
            .map_or(self.started_at, |half| self.started_at + half)
    }
}

/// A guide camera (PRD §4.1, HAL-04).
///
/// `Debug` is a supertrait for the same reason as on
/// [`MountDevice`](crate::mount::MountDevice).
#[async_trait]
pub trait GuideCamera: Debug + Send + Sync {
    /// Opens the camera.
    ///
    /// # Errors
    /// `Transport` if the device is absent or claimed, `Timeout`, `Protocol`. Connecting an
    /// already-connected camera is `Ok(())`.
    async fn connect(&self) -> Result<(), DeviceError>;

    /// Releases the camera, ending any continuous stream.
    ///
    /// # Errors
    /// Transport-level failures only; disconnecting a disconnected camera is `Ok(())`.
    async fn disconnect(&self) -> Result<(), DeviceError>;

    /// Sets the exposure time for subsequent frames.
    ///
    /// Applies from the next exposure; a frame already integrating keeps the old value, and
    /// reports what it used in [`GuideFrame::exposure`], so a loop that changes exposure mid-run
    /// can tell which setting each frame belongs to.
    ///
    /// # Errors
    /// `Rejected` outside the camera's
    /// [`min_exposure_seconds`](GuideCameraCapabilities::min_exposure_seconds) ..
    /// [`max_exposure_seconds`](GuideCameraCapabilities::max_exposure_seconds) range,
    /// `NotConnected`, or a transport-level failure.
    async fn set_exposure(&self, exposure: Duration) -> Result<(), DeviceError>;

    /// Sets gain in the camera's own units.
    ///
    /// # Errors
    /// `Rejected` above [`max_gain`](GuideCameraCapabilities::max_gain), `NotConnected`, or a
    /// transport-level failure.
    async fn set_gain(&self, gain: u32) -> Result<(), DeviceError>;

    /// Sets binning.
    ///
    /// Changing it changes the frame dimensions and therefore the pixel scale the guide loop
    /// converts error into corrections with, so a loop must re-read
    /// [`capabilities`](Self::capabilities) — or read the dimensions off the next frame — rather
    /// than cache them across a change.
    ///
    /// # Errors
    /// `Unsupported` above [`max_binning`](GuideCameraCapabilities::max_binning), `NotConnected`,
    /// or a transport-level failure.
    async fn set_binning(&self, binning: Binning) -> Result<(), DeviceError>;

    /// Takes one exposure at the current settings and returns it.
    ///
    /// Resolves after the exposure and readout — roughly the exposure time. This is the path for
    /// a caller that needs *every* frame; the continuous stream deliberately drops frames a slow
    /// consumer missed.
    ///
    /// # Errors
    /// `Busy` if continuous acquisition owns the sensor, `NotConnected`, `Timeout`, or a
    /// transport-level failure.
    async fn capture_frame(&self) -> Result<GuideFrame, DeviceError>;

    /// Starts free-running acquisition if it is not already running, and returns a subscription.
    ///
    /// Idempotent — a second call yields another cursor on the same stream, not a second
    /// acquisition; the sensor is one. The stream ends on
    /// [`stop_continuous`](Self::stop_continuous) or [`disconnect`](Self::disconnect), and drops
    /// frames a slow consumer did not collect (see [`crate::stream`]): a guide frame that has
    /// been superseded is of no use to a loop correcting in the present.
    ///
    /// PRD §4.1 lists `start_continuous` and `continuous_stream` separately; they are one method
    /// here because two would mean either a start that hands back nothing usable or a subscribe
    /// that fails when nothing is running, and every caller wants both halves at once.
    ///
    /// # Errors
    /// `NotConnected`, or a transport-level failure.
    async fn start_continuous(&self) -> Result<FrameStream<GuideFrame>, DeviceError>;

    /// Stops free-running acquisition. Idempotent.
    ///
    /// # Errors
    /// Transport-level failures only.
    async fn stop_continuous(&self) -> Result<(), DeviceError>;

    /// Enables or disables cooling and sets the setpoint in degrees Celsius.
    ///
    /// Returns as soon as the setpoint is accepted, not when the sensor reaches it — cooling
    /// takes minutes and is watched through [`temperature_celsius`](Self::temperature_celsius).
    /// `target_celsius` is ignored when `enabled` is false.
    ///
    /// # Errors
    /// `Unsupported` if [`has_cooling`](GuideCameraCapabilities::has_cooling) is false,
    /// `Rejected` for a setpoint outside the camera's range, `NotConnected`, or a
    /// transport-level failure.
    async fn set_cooling(&self, enabled: bool, target_celsius: f64) -> Result<(), DeviceError>;

    /// Sensor temperature in degrees Celsius, or `None` on a camera that does not report one.
    ///
    /// `None` is a routine answer, not an error — same reasoning as
    /// [`Camera::sensor_temperature_celsius`](crate::camera::Camera::sensor_temperature_celsius).
    /// An uncooled camera that *does* report a reading should report it: dark-frame matching
    /// wants the temperature whether or not it was regulated.
    ///
    /// # Errors
    /// `NotConnected`, or a transport-level failure.
    async fn temperature_celsius(&self) -> Result<Option<f64>, DeviceError>;

    /// What this guide camera can do (HAL-05).
    ///
    /// Synchronous, no I/O, callable while disconnected.
    fn capabilities(&self) -> GuideCameraCapabilities;

    /// Human-readable identity for logs, the UI and calibration-profile tagging (HAL-06).
    ///
    /// Synchronous, no I/O, callable while disconnected.
    fn device_info(&self) -> DeviceInfo;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Binning, GuideCamera, GuideFrame, PixelLayout};
    use crate::test_double::FakeGuideCamera;

    #[test]
    fn binning_cannot_be_zero() {
        assert_eq!(Binning::NONE.x(), 1);
        assert_eq!(Binning::square(2).expect("valid").y(), 2);
        assert!(Binning::new(0, 1).is_err());
        assert!(Binning::new(1, 0).is_err());
        // The message an operator would see, not a debug dump.
        let err = Binning::new(2, 0).expect_err("zero is rejected");
        assert_eq!(
            err.to_string(),
            "invalid vertical binning: 0 is not at least 1"
        );
    }

    #[test]
    fn a_frame_knows_whether_its_buffer_matches_its_dimensions() {
        let mut frame = GuideFrame {
            width: 4,
            height: 2,
            pixels: Arc::from(vec![0_u16; 8]),
            layout: PixelLayout::Mono,
            binning: Binning::NONE,
            exposure: Duration::from_secs(2),
            gain: 100,
            started_at: chrono::Utc::now(),
        };
        assert!(frame.is_consistent());

        frame.width = 5;
        assert!(
            !frame.is_consistent(),
            "a truncated buffer must be detectable"
        );
    }

    #[test]
    fn a_frames_measurement_belongs_to_the_middle_of_its_exposure() {
        let started_at = chrono::Utc::now();
        let frame = GuideFrame {
            width: 1,
            height: 1,
            pixels: Arc::from(vec![0_u16; 1]),
            layout: PixelLayout::Mono,
            binning: Binning::NONE,
            exposure: Duration::from_secs(4),
            gain: 1,
            started_at,
        };
        assert_eq!(frame.midpoint() - started_at, chrono::TimeDelta::seconds(2));
    }

    #[tokio::test]
    async fn continuous_acquisition_is_usable_through_arc_dyn() {
        let camera = Arc::new(FakeGuideCamera::new()) as Arc<dyn GuideCamera>;
        camera.connect().await.expect("connects");
        camera
            .set_binning(Binning::square(2).expect("valid"))
            .await
            .expect("2x2 is within max_binning");

        let mut stream = camera.start_continuous().await.expect("starts");
        let frame = stream.next_frame().await.expect("a frame arrives");
        assert!(frame.is_consistent());
        assert_eq!(frame.binning, Binning::square(2).expect("valid"));

        camera.stop_continuous().await.expect("stops");
        assert!(
            stream.next_frame().await.is_none(),
            "stopping ends the stream"
        );
    }

    #[tokio::test]
    async fn an_absent_capability_answers_unsupported_rather_than_pretending() {
        // Rule 6: the fake reports `has_cooling: false`, so it must refuse rather than accept a
        // setpoint it will never reach.
        let camera = FakeGuideCamera::new();
        camera.connect().await.expect("connects");
        assert!(!camera.capabilities().has_cooling);
        assert!(matches!(
            camera.set_cooling(true, -10.0).await,
            Err(astroctl_core::error::DeviceError::Unsupported)
        ));
        assert!(matches!(
            camera.set_binning(Binning::square(8).expect("valid")).await,
            Err(astroctl_core::error::DeviceError::Unsupported)
        ));
    }
}
