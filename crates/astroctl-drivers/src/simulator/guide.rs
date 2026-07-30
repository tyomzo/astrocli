//! `SimulatorGuideCamera` — a [`GuideCamera`] off the same sky as the imaging camera (PRD §4.5,
//! HAL-04/HAL-11, task M1-T06).
//!
//! # Why this exists before the guide loop does
//!
//! The autoguider is Phase 3. The trait and this implementation ship in M1 because an interface
//! with no implementation is an interface nobody has tried to satisfy — the `astroctl-hal`
//! module docs say so at the trait itself — and because the cost of finding out it is wrong
//! rises with every crate written against it. Building the driver now is what makes
//! [`GuideFrame`]'s shape (16-bit samples whatever the depth, binning that changes the
//! dimensions, a midpoint timestamp) a set of decisions someone has had to honour.
//!
//! # What it reproduces
//!
//! * **The same sky as the imaging camera**, projected through a different, coarser telescope.
//!   Both cameras hold the same [`StarField`](super::sky::StarField) value, so a star at a given
//!   RA/Dec lands in both frames — which is the property a Phase 3 loop will silently depend on
//!   the first time someone compares a guide star to a plate solve.
//! * **Seeing that moves the star between frames.** The whole field is displaced by a Gaussian
//!   draw each exposure, which is what a guide loop chases. It is deliberately a *field*
//!   displacement, not a per-star one: within a guider's few-arcminute view the atmosphere moves
//!   everything together, and a simulator that jittered each star independently would let a
//!   centroid-averaging bug look like an improvement.
//! * **Frames that cost something.** Exposure plus a readout, at 12 bits, from a real
//!   computation on the camera's own thread — see [`imaging`](super::imaging) for why that
//!   thread exists.
//!
//! # What it does not
//!
//! No dark current, no amp glow, no hot pixels, no cloud. Hot pixels in particular are the
//! classic false guide star and are worth having *when there is a detector to fool*; inventing
//! them now would mean tuning Phase 3's star selection against a defect distribution nobody
//! measured.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use astroctl_core::config::{GuideCameraConfig, GuideCameraDriver};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{DeviceInfo, DeviceKind, GuideCameraCapabilities};
use astroctl_hal::guide::{Binning, GuideCamera, GuideFrame, PixelLayout};
use astroctl_hal::registry::{DetectedDevice, DriverInitError, GuideCameraFactory};
use astroctl_hal::stream::{FrameSink, FrameStream};
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::imaging::{CameraThread, Job};
use super::noise::Rng;
use super::profile::GuideCameraProfile;
use super::sky::{Exposure, InjectedStar, PointingSource};

/// Stream key separating this sensor's noise from the imaging camera's.
const NOISE_STREAM: u64 = 0x4755_4944_4543_414D; // "GUIDECAM"

/// Stream key for the seeing draw, kept apart from the pixel noise so that changing the seeing
/// does not change the read noise of the same frame — which would make every comparison between
/// two seeing values a comparison of two different sensors.
const SEEING_STREAM: u64 = 0x5345_4549_4E47_0000; // "SEEING\0\0"

/// ADU per electron.
///
/// **Invented**, and chosen for one property: at gain 100 (the profile's maximum) a star bright
/// enough to guide on saturates the 12-bit range, so the gain knob has a visible top end. A
/// guide camera's "gain" is not an ISO — it is the sensor's own units, which is why the trait
/// takes a bare `u32` and the driver decides what it means.
const GAIN_AT_UNITY: f64 = 0.02;

/// What the driver believes about its link.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Link {
    Down,
    Up,
}

/// A running continuous acquisition.
#[derive(Debug)]
struct Continuous {
    stream: FrameStream<GuideFrame>,
    task: JoinHandle<()>,
}

/// Everything behind the lock.
#[derive(Debug)]
struct State {
    link: Link,
    exposure: Duration,
    gain: u32,
    binning: Binning,
    cooling: Option<f64>,
    thread: Option<Arc<CameraThread>>,
    continuous: Option<Continuous>,
    /// Frames produced since construction. Seeds both the pixel noise and the seeing draw, so a
    /// run of frames is a reproducible *sequence* rather than a set of independent samples —
    /// which is what makes "the centroid variance responds to the seeing parameter" a fixed
    /// number rather than a probability.
    frames: u64,
}

/// Everything the driver and its acquisition loop both need.
#[derive(Debug)]
struct Shared {
    profile: GuideCameraProfile,
    pointing: Arc<dyn PointingSource>,
    /// One sensor: `capture_frame` is `Busy` while continuous acquisition owns it.
    sensor: Semaphore,
    state: Mutex<State>,
}

impl Shared {
    fn locked(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("guide camera state is never poisoned")
    }

    fn connected(&self) -> Result<Arc<CameraThread>, DeviceError> {
        let state = self.locked();
        if state.link == Link::Down {
            return Err(DeviceError::NotConnected);
        }
        state.thread.clone().ok_or(DeviceError::NotConnected)
    }

    /// Renders one frame at the current settings.
    ///
    /// The whole method is `&self` and takes the lock twice — once to read the settings and once
    /// to bump the frame counter — with the render in between and neither lock held across it.
    async fn expose(&self) -> Result<GuideFrame, DeviceError> {
        let thread = self.connected()?;
        let (exposure, gain, binning, frame) = {
            let mut state = self.locked();
            state.frames += 1;
            (state.exposure, state.gain, state.binning, state.frames)
        };

        // Binning sums charge, so a 2×2 frame is a quarter the pixels at four times the signal
        // and twice the plate scale. Rendering at the binned scale directly (rather than
        // rendering full-size and summing) is the same frame to within the noise on one pixel,
        // and is four times less arithmetic — which matters at a guider's cadence.
        let bin = f64::from(binning.x().max(binning.y()));
        let width = self.profile.width / u32::from(binning.x());
        let height = self.profile.height / u32::from(binning.y());
        let saturation = (1_u32 << self.profile.bit_depth) - 1;

        // Seeing: one displacement for the whole field, drawn per frame.
        let mut seeing = Rng::stream(&[SEEING_STREAM, frame]);
        let jitter = (
            seeing.normal_with(0.0, self.profile.seeing_arcsec),
            seeing.normal_with(0.0, self.profile.seeing_arcsec),
        );

        let plan = Exposure {
            width,
            height,
            pointing: self
                .pointing
                .pointing()
                .unwrap_or(self.profile.default_pointing),
            arcsec_per_pixel: self.profile.arcsec_per_pixel * bin,
            exposure,
            gain_adu_per_electron: GAIN_AT_UNITY * f64::from(gain.max(1)),
            fwhm_arcsec: self.profile.fwhm_arcsec,
            sky_electrons_per_second: self.profile.sky_electrons_per_second * bin * bin,
            read_noise_electrons: self.profile.read_noise_electrons,
            bias_adu: self.profile.bias_adu,
            full_well_electrons: self.profile.full_well_electrons,
            saturation_adu: u16::try_from(saturation).unwrap_or(u16::MAX),
            noise_seed: Some(NOISE_STREAM ^ frame),
            jitter_arcsec: jitter,
            injected: self
                .profile
                .guide_star_magnitude
                .map(|magnitude| InjectedStar {
                    offset: (0.0, 0.0),
                    magnitude,
                })
                .into_iter()
                .collect(),
            field: self.profile.field,
        };

        let started_at = Utc::now();
        // The exposure is time passing and the readout is the transfer; both are waited on the
        // runtime clock, and only the pixels are computed on the camera's thread. See the
        // `imaging` module docs for why the wait is not a blocked thread.
        tokio::time::sleep(exposure).await;
        let pixels = thread
            .submit(|reply| Job::Samples {
                exposure: Box::new(plan),
                reply,
            })
            .await?;
        tokio::time::sleep(self.profile.readout).await;

        Ok(GuideFrame {
            width,
            height,
            pixels: pixels.into(),
            // Mono whatever the sensor is: a guide camera normally is, and a *binned* colour
            // frame would have to report `Mono` anyway because binning destroys the mosaic.
            layout: PixelLayout::Mono,
            binning,
            exposure,
            gain,
            started_at,
        })
    }
}

// -----------------------------------------------------------------------------------------
// The driver
// -----------------------------------------------------------------------------------------

/// A guide camera looking at the simulated sky (PRD §4.5, HAL-11).
#[derive(Debug)]
pub struct SimulatorGuideCamera {
    shared: Arc<Shared>,
}

impl SimulatorGuideCamera {
    /// Builds a guide camera from the operator's configuration plus the two constructor
    /// parameters SDD §9 and PRD §4.5 require: the profile, and where the telescope points.
    ///
    /// There is no fault plan here, and the omission is deliberate rather than an oversight: the
    /// Phase 3 guide loop is what would consume guide-camera failures, nothing in M1 or M2 can
    /// observe one, and a fault vocabulary invented now would be invented against no caller. The
    /// imaging camera has one because M1-T08's capture flow is written against it this milestone.
    ///
    /// # Errors
    /// [`DriverInitError`] if the configuration names a driver this is not.
    pub fn new(
        config: &GuideCameraConfig,
        profile: GuideCameraProfile,
        pointing: Arc<dyn PointingSource>,
    ) -> Result<Self, DriverInitError> {
        if let Some(driver) = config.driver {
            if driver != GuideCameraDriver::Simulator {
                return Err(DriverInitError::new(format!(
                    "`guide_camera.driver: {}` is not this driver",
                    driver.as_str()
                )));
            }
        }
        Ok(Self {
            shared: Arc::new(Shared {
                profile,
                pointing,
                sensor: Semaphore::new(1),
                state: Mutex::new(State {
                    link: Link::Down,
                    // One second is the guide exposure most loops start from: long enough to
                    // average the fastest seeing, short enough to correct before the mount has
                    // drifted anywhere.
                    exposure: Duration::from_secs(1),
                    gain: 50,
                    binning: Binning::NONE,
                    cooling: None,
                    thread: None,
                    continuous: None,
                    frames: 0,
                }),
            }),
        })
    }

    /// A guide camera on the default configuration.
    ///
    /// # Errors
    /// [`DriverInitError`] as [`new`](Self::new); unreachable with the built-in defaults.
    pub fn with_profile(
        profile: GuideCameraProfile,
        pointing: Arc<dyn PointingSource>,
    ) -> Result<Self, DriverInitError> {
        Self::new(&default_config(), profile, pointing)
    }

    /// The free-running acquisition loop.
    ///
    /// No timer: a guide camera free-runs as fast as exposure plus readout allows, and the
    /// interval *is* that sum. A ticker would either drop frames the sensor produced or ask for
    /// frames it cannot yet deliver.
    async fn continuous_loop(shared: Arc<Shared>, sink: FrameSink<GuideFrame>) {
        loop {
            // Held for the whole exposure, which is what makes `capture_frame` answer `Busy`
            // rather than quietly interleaving a frame into someone else's cadence.
            let Ok(_permit) = shared.sensor.try_acquire() else {
                // Nothing else can hold the sensor while this loop owns it, so this is
                // unreachable in practice; yielding rather than spinning keeps it harmless if it
                // ever stops being.
                tokio::task::yield_now().await;
                continue;
            };
            let Ok(frame) = shared.expose().await else {
                // The camera went away or was disconnected. Returning drops the sink, which ends
                // every subscriber's stream — the signal a consumer needs, and the only one it
                // gets from either cause.
                return;
            };
            sink.publish(frame);
        }
    }
}

#[async_trait]
impl GuideCamera for SimulatorGuideCamera {
    async fn connect(&self) -> Result<(), DeviceError> {
        if self.shared.locked().link == Link::Up {
            return Ok(());
        }
        tokio::time::sleep(self.shared.profile.connect).await;
        let mut state = self.shared.locked();
        state.link = Link::Up;
        state.thread = Some(Arc::new(CameraThread::start("astroctl-simguide")));
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        let continuous = {
            let mut state = self.shared.locked();
            state.link = Link::Down;
            state.thread = None;
            state.continuous.take()
        };
        if let Some(continuous) = continuous {
            continuous.task.abort();
        }
        Ok(())
    }

    async fn set_exposure(&self, exposure: Duration) -> Result<(), DeviceError> {
        let seconds = exposure.as_secs_f64();
        let profile = &self.shared.profile;
        if seconds < profile.min_exposure_seconds || seconds > profile.max_exposure_seconds {
            return Err(DeviceError::Rejected(format!(
                "{seconds} s is outside this camera's {} s to {} s range",
                profile.min_exposure_seconds, profile.max_exposure_seconds
            )));
        }
        // Applies from the *next* exposure: a frame already integrating keeps the old value and
        // reports it, which is what lets a loop that changes exposure mid-run tell which setting
        // each frame belongs to.
        self.shared.locked().exposure = exposure;
        Ok(())
    }

    async fn set_gain(&self, gain: u32) -> Result<(), DeviceError> {
        if gain > self.shared.profile.max_gain {
            return Err(DeviceError::Rejected(format!(
                "gain {gain} is above this camera's maximum of {}",
                self.shared.profile.max_gain
            )));
        }
        self.shared.locked().gain = gain;
        Ok(())
    }

    async fn set_binning(&self, binning: Binning) -> Result<(), DeviceError> {
        let max = self.shared.profile.max_binning;
        if binning.x() > max || binning.y() > max {
            return Err(DeviceError::Unsupported);
        }
        self.shared.locked().binning = binning;
        Ok(())
    }

    async fn capture_frame(&self) -> Result<GuideFrame, DeviceError> {
        let _permit = self
            .shared
            .sensor
            .try_acquire()
            .map_err(|_| DeviceError::Busy("continuous acquisition owns the sensor"))?;
        self.shared.expose().await
    }

    async fn start_continuous(&self) -> Result<FrameStream<GuideFrame>, DeviceError> {
        self.shared.connected()?;
        let mut state = self.shared.locked();
        if let Some(continuous) = state.continuous.as_ref() {
            // Another cursor on the same stream, not a second acquisition: the sensor is one.
            return Ok(continuous.stream.subscribe());
        }
        let (sink, stream) = FrameStream::channel();
        let subscription = stream.subscribe();
        let task = tokio::spawn(Self::continuous_loop(Arc::clone(&self.shared), sink));
        state.continuous = Some(Continuous { stream, task });
        Ok(subscription)
    }

    async fn stop_continuous(&self) -> Result<(), DeviceError> {
        if let Some(continuous) = self.shared.locked().continuous.take() {
            continuous.task.abort();
        }
        Ok(())
    }

    async fn set_cooling(&self, enabled: bool, target_celsius: f64) -> Result<(), DeviceError> {
        // Rule 6: a capability reported false must be refused, not accepted and ignored. A
        // driver that accepted a setpoint it will never reach would have the operator watching a
        // temperature that was never going to move.
        if !self.capabilities().has_cooling {
            return Err(DeviceError::Unsupported);
        }
        self.shared.locked().cooling = enabled.then_some(target_celsius);
        Ok(())
    }

    async fn temperature_celsius(&self) -> Result<Option<f64>, DeviceError> {
        self.shared.connected()?;
        Ok(self.shared.profile.sensor_temperature_celsius)
    }

    fn capabilities(&self) -> GuideCameraCapabilities {
        let profile = &self.shared.profile;
        GuideCameraCapabilities {
            sensor_width_px: profile.width,
            sensor_height_px: profile.height,
            pixel_size_um: profile.pixel_size_um,
            max_gain: profile.max_gain,
            bit_depth: profile.bit_depth,
            max_binning: profile.max_binning,
            // No cooling, which is what a guider of this class is — and which makes this driver
            // the one place the `Unsupported` arm of `set_cooling` is exercised before there is
            // hardware.
            has_cooling: false,
            min_exposure_seconds: profile.min_exposure_seconds,
            max_exposure_seconds: profile.max_exposure_seconds,
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "AstroCtl Simulator Guide Camera".to_owned(),
            model: "ASI120MM-class simulator".to_owned(),
            firmware: None,
            serial: Some("SIM-GUIDE-000001".to_owned()),
            protocol: "simulator".to_owned(),
        }
    }
}

// -----------------------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------------------

/// Builds [`SimulatorGuideCamera`]s for the driver registry under the name `"simulator"`
/// (HAL-07).
#[derive(Debug, Clone)]
pub struct SimulatorGuideCameraFactory {
    profile: GuideCameraProfile,
    pointing: Arc<dyn PointingSource>,
}

impl Default for SimulatorGuideCameraFactory {
    fn default() -> Self {
        let profile = GuideCameraProfile::default();
        Self {
            profile,
            pointing: Arc::new(profile.default_pointing),
        }
    }
}

impl SimulatorGuideCameraFactory {
    /// A factory building guide cameras on the default profile and a fixed pointing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A factory building guide cameras on `profile`.
    #[must_use]
    pub fn with_profile(mut self, profile: GuideCameraProfile) -> Self {
        self.profile = profile;
        self
    }

    /// A factory whose cameras follow `pointing` — normally the simulated mount, and normally
    /// the same source the imaging camera was given, so that the two agree about the sky.
    #[must_use]
    pub fn following(mut self, pointing: Arc<dyn PointingSource>) -> Self {
        self.pointing = pointing;
        self
    }
}

#[async_trait]
impl GuideCameraFactory for SimulatorGuideCameraFactory {
    fn name(&self) -> &'static str {
        // Must equal `GuideCameraDriver::Simulator.as_str()`; asserted by a test below.
        "simulator"
    }

    fn create(&self, config: &GuideCameraConfig) -> Result<Arc<dyn GuideCamera>, DriverInitError> {
        Ok(Arc::new(SimulatorGuideCamera::new(
            config,
            self.profile,
            Arc::clone(&self.pointing),
        )?))
    }

    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        Ok(vec![DetectedDevice::new(
            "simulator",
            DeviceKind::GuideCamera,
            "simulator",
            "AstroCtl Simulator Guide Camera (no hardware required)",
        )])
    }
}

/// The configuration a bare [`SimulatorGuideCamera::with_profile`] uses.
fn default_config() -> GuideCameraConfig {
    GuideCameraConfig {
        driver: Some(GuideCameraDriver::Simulator),
        asi_index: None,
        qhy_id: None,
        indi_device: None,
    }
}

#[cfg(test)]
mod tests {
    use astroctl_core::types::RaDec;
    use astroctl_hal::camera::{Camera, CaptureRequest};
    use astroctl_hal::registry::DriverRegistry;

    use super::super::camera::{SimulatorCamera, SimulatorCameraFactory};
    use super::super::profile::CameraProfile;
    use super::super::sky::{detect, Projection, StarField};
    use super::*;

    /// A small guide camera on a bright field.
    ///
    /// 96×72 at the profile's 3.2″/px is five arcminutes across — a real guider's field is
    /// larger, but the *scale* is what the arithmetic depends on, and a small frame keeps a
    /// hundred-frame seeing test inside a millisecond.
    fn profile() -> GuideCameraProfile {
        GuideCameraProfile {
            width: 96,
            height: 72,
            connect: Duration::ZERO,
            field: StarField::new(4242)
                .with_density(500.0)
                .with_faintest_magnitude(12.0),
            ..GuideCameraProfile::default()
        }
    }

    fn camera_with(profile: GuideCameraProfile) -> Arc<SimulatorGuideCamera> {
        camera_aimed(profile, profile.default_pointing)
    }

    fn camera_aimed(profile: GuideCameraProfile, pointing: RaDec) -> Arc<SimulatorGuideCamera> {
        Arc::new(
            SimulatorGuideCamera::new(&default_config(), profile, Arc::new(pointing))
                .expect("valid"),
        )
    }

    async fn connected() -> Arc<SimulatorGuideCamera> {
        let camera = camera_with(profile());
        camera.connect().await.expect("connects");
        camera
    }

    /// Where the guide star landed in one frame, in pixels.
    fn centroid(frame: &GuideFrame) -> (f64, f64) {
        let found = detect::brightest(&frame.pixels, frame.width, frame.height, 200.0)
            .expect("a guide star");
        (found.column, found.row)
    }

    /// Standard deviation of a set of values.
    fn sigma(values: &[f64]) -> f64 {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
    }

    // --- acceptance: the frames ---------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_frame_is_consistent_and_carries_what_it_was_shot_with() {
        let camera = connected().await;
        camera
            .set_exposure(Duration::from_millis(500))
            .await
            .expect("in range");
        camera.set_gain(80).await.expect("in range");

        let frame = camera.capture_frame().await.expect("a frame");
        assert!(
            frame.is_consistent(),
            "the buffer must match the dimensions"
        );
        assert_eq!((frame.width, frame.height), (96, 72));
        assert_eq!(frame.exposure, Duration::from_millis(500));
        assert_eq!(frame.gain, 80);
        assert_eq!(frame.binning, Binning::NONE);
        assert_eq!(frame.layout, PixelLayout::Mono);
        // The measurement belongs to the middle of the exposure.
        assert_eq!(
            frame.midpoint() - frame.started_at,
            chrono::TimeDelta::milliseconds(250)
        );
        // 12-bit samples, so a star detector reading `bit_depth` finds saturation where it was
        // told to.
        assert!(
            frame.pixels.iter().all(|sample| *sample <= 4095),
            "a 12-bit frame must not carry 16-bit values"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_exposure_costs_its_own_time_plus_the_readout() {
        let camera = connected().await;
        camera
            .set_exposure(Duration::from_secs(2))
            .await
            .expect("in range");
        let start = tokio::time::Instant::now();
        camera.capture_frame().await.expect("a frame");
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(2) + GuideCameraProfile::default().readout
        );
    }

    #[tokio::test(start_paused = true)]
    async fn binning_changes_the_dimensions_and_the_scale_together() {
        // The trait's warning made concrete: a loop that cached the pixel scale across a binning
        // change would convert every centroid error into a correction twice the size it should
        // be.
        let camera = connected().await;
        let unbinned = camera.capture_frame().await.expect("a frame");
        camera
            .set_binning(Binning::square(2).expect("valid"))
            .await
            .expect("within max_binning");
        let binned = camera.capture_frame().await.expect("a frame");

        assert_eq!((binned.width, binned.height), (48, 36));
        assert!(binned.is_consistent());
        assert_eq!(binned.binning, Binning::square(2).expect("valid"));
        // Four pixels of sky in one well: the background rises about fourfold above the bias.
        let bias = 100.0;
        let ratio = (detect::mean(&binned.pixels) - bias) / (detect::mean(&unbinned.pixels) - bias);
        assert!(
            (2.5..5.5).contains(&ratio),
            "binning changed the level {ratio}x"
        );

        assert!(matches!(
            camera.set_binning(Binning::square(8).expect("valid")).await,
            Err(DeviceError::Unsupported)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn gain_and_exposure_are_refused_outside_the_camera_range() {
        let camera = connected().await;
        assert!(matches!(
            camera.set_gain(101).await,
            Err(DeviceError::Rejected(_))
        ));
        assert!(matches!(
            camera.set_exposure(Duration::from_secs(120)).await,
            Err(DeviceError::Rejected(_))
        ));
        assert!(matches!(
            camera.set_exposure(Duration::from_micros(100)).await,
            Err(DeviceError::Rejected(_))
        ));
        // An absent capability is refused rather than pretended (HAL rule 6).
        assert!(matches!(
            camera.set_cooling(true, -10.0).await,
            Err(DeviceError::Unsupported)
        ));
        assert_eq!(
            camera.temperature_celsius().await.expect("reads"),
            Some(11.5),
            "an uncooled camera that reports a temperature should report it"
        );
    }

    // --- acceptance: seeing ---------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn injected_seeing_moves_the_guide_star_and_the_variance_follows_it() {
        // Acceptance criterion 4. Two seeing values, sixty frames each, and the measured scatter
        // of the guide star's centroid must follow the parameter — in *arcseconds*, so the
        // assertion is about the sky rather than about this sensor's pixel size.
        const FRAMES: usize = 60;

        async fn scatter(seeing_arcsec: f64) -> (f64, f64) {
            let camera = camera_with(GuideCameraProfile {
                seeing_arcsec,
                // A well-sampled star on an empty field, for two separate reasons.
                //
                // Empty, so that `brightest` is certainly the *injected* star: with catalogue
                // stars present, a saturated pair could swap places between frames and the
                // "scatter" measured would be the distance between two stars.
                //
                // Well sampled, because the profile's default 3.5″ FWHM at 3.2″/px puts a star
                // across barely one pixel, and a centre-of-mass centroid on an undersampled star
                // under-reports its motion by about half — pixel-phase bias, the classic reason
                // an undersampled guider guides worse than its seeing. That is a real property
                // of a real guide scope and it stays in the default profile; it is not what this
                // test is about, which is whether the seeing parameter reaches the frame at all.
                fwhm_arcsec: 12.0,
                field: StarField::new(4242).with_density(0.0),
                ..profile()
            });
            camera.connect().await.expect("connects");
            let mut columns = Vec::with_capacity(FRAMES);
            let mut rows = Vec::with_capacity(FRAMES);
            for _ in 0..FRAMES {
                let frame = camera.capture_frame().await.expect("a frame");
                let (column, row) = centroid(&frame);
                columns.push(column);
                rows.push(row);
            }
            let scale = GuideCameraProfile::default().arcsec_per_pixel;
            (sigma(&columns) * scale, sigma(&rows) * scale)
        }

        let (steady_x, steady_y) = scatter(0.5).await;
        let (turbulent_x, turbulent_y) = scatter(4.0).await;

        // Each measured scatter is within a third of the seeing it was asked for. Wider than a
        // statistician would like because sixty frames of a Gaussian have their own spread, and
        // because a centroid on a noisy 12-bit frame is not free — but narrow enough that a
        // seeing parameter that did nothing, or that was applied in pixels instead of
        // arcseconds, fails it.
        for (asked, measured) in [
            (0.5, steady_x),
            (0.5, steady_y),
            (4.0, turbulent_x),
            (4.0, turbulent_y),
        ] {
            assert!(
                (measured - asked).abs() < asked * 0.35_f64,
                "seeing {asked}\" produced {measured:.2}\" of scatter"
            );
        }
        // ...and the two regimes are unambiguously different, which is the acceptance
        // criterion's own wording: the variance *responds* to the parameter.
        assert!(
            turbulent_x > 4.0 * steady_x,
            "{turbulent_x:.2}\" vs {steady_x:.2}\""
        );
    }

    #[tokio::test(start_paused = true)]
    async fn without_seeing_the_guide_star_does_not_move_at_all() {
        // The control for the test above: with the atmosphere switched off the star sits still
        // to a hundredth of a pixel, so any scatter measured above is the seeing and not the
        // centroid algorithm.
        let camera = camera_with(GuideCameraProfile {
            seeing_arcsec: 0.0,
            fwhm_arcsec: 12.0,
            field: StarField::new(4242).with_density(0.0),
            ..profile()
        });
        camera.connect().await.expect("connects");
        let mut columns = Vec::new();
        for _ in 0..20 {
            columns.push(centroid(&camera.capture_frame().await.expect("a frame")).0);
        }
        assert!(sigma(&columns) < 0.05, "scatter {} px", sigma(&columns));
    }

    #[tokio::test(start_paused = true)]
    async fn the_guide_star_brightness_is_configurable() {
        // PRD §4.5 asks for the knob by name. Two magnitudes apart is 6.3× the flux, and the
        // point of the assertion is that a *dim* guide star is dim — a simulator that clipped
        // everything to saturation would make star selection untestable.
        //
        // Magnitudes 11 and 13 rather than 9 and 11: at this gain a magnitude-9 star fills the
        // 12-bit well, and comparing two saturated peaks measures the converter rather than the
        // star. (That clipping is itself correct — see the frame test — and it is exactly why a
        // guide loop picks a *medium* star.)
        let bright = camera_with(GuideCameraProfile {
            guide_star_magnitude: Some(11.0),
            seeing_arcsec: 0.0,
            field: StarField::new(4242).with_density(0.0),
            ..profile()
        });
        let faint = camera_with(GuideCameraProfile {
            guide_star_magnitude: Some(13.0),
            seeing_arcsec: 0.0,
            field: StarField::new(4242).with_density(0.0),
            ..profile()
        });
        bright.connect().await.expect("connects");
        faint.connect().await.expect("connects");

        let peak = |frame: &GuideFrame| {
            f64::from(
                detect::brightest(&frame.pixels, frame.width, frame.height, 100.0)
                    .expect("a star")
                    .peak,
            ) - 100.0
        };
        let bright_peak = peak(&bright.capture_frame().await.expect("a frame"));
        let faint_peak = peak(&faint.capture_frame().await.expect("a frame"));
        assert!(
            bright_peak > 4.0 * faint_peak,
            "magnitude 11 peaked at {bright_peak} and magnitude 13 at {faint_peak}"
        );
    }

    // --- acceptance: the stream ---------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn continuous_acquisition_delivers_frames_at_the_configured_cadence() {
        let camera = connected().await;
        camera
            .set_exposure(Duration::from_secs(1))
            .await
            .expect("in range");

        let mut stream = camera.start_continuous().await.expect("starts");
        let mut second = camera
            .start_continuous()
            .await
            .expect("joins the same stream");

        let start = tokio::time::Instant::now();
        let first = stream.next_frame().await.expect("a frame arrives");
        // Exposure plus readout, exactly — the cadence a guide loop budgets its corrections
        // against.
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(1) + GuideCameraProfile::default().readout
        );
        assert!(first.is_consistent());

        // Two cursors, one acquisition.
        let next = stream.next_frame().await.expect("another frame");
        let same = second.next_frame().await.expect("the same stream");
        assert_eq!(next.started_at, same.started_at);
        assert!(next.started_at > first.started_at);

        // While it is free-running the sensor is busy — a single-shot capture must not
        // interleave itself into someone else's cadence.
        let interloper = camera.capture_frame().await;
        assert!(
            matches!(interloper, Err(DeviceError::Busy(_))),
            "{interloper:?}"
        );

        camera.stop_continuous().await.expect("stops");
        tokio::time::timeout(Duration::from_secs(10), async {
            while stream.next_frame().await.is_some() {}
        })
        .await
        .expect("stopping ends the stream");
        // ...and the sensor is free again.
        camera.capture_frame().await.expect("a single frame");
    }

    #[tokio::test(start_paused = true)]
    async fn disconnecting_ends_the_stream_and_stops_the_camera_answering() {
        let camera = connected().await;
        let mut stream = camera.start_continuous().await.expect("starts");
        stream.next_frame().await.expect("a frame");

        camera.disconnect().await.expect("disconnects");
        tokio::time::timeout(Duration::from_secs(10), async {
            while stream.next_frame().await.is_some() {}
        })
        .await
        .expect("disconnect ends the stream");
        assert!(matches!(
            camera.capture_frame().await,
            Err(DeviceError::NotConnected)
        ));
        camera.connect().await.expect("reconnects");
        camera.capture_frame().await.expect("and works");
    }

    // --- acceptance: one sky, two cameras -------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn both_cameras_put_the_same_star_at_the_same_place_in_the_sky() {
        // The requirement behind "the guide camera reads the same generator": a star is at an
        // RA and a Dec, and two cameras with different plate scales must both say so. Measured
        // in each camera's own pixels and converted through each camera's own projection, which
        // is the only comparison that would catch a scale or an orientation error.
        let dir = std::env::temp_dir().join(format!("astroctl-simsky-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a writable directory");
        let pointing = RaDec::from_parts(5.5, 22.0).expect("valid");
        let sky = StarField::new(90210)
            .with_density(600.0)
            .with_faintest_magnitude(11.0);

        // The imaging camera: a wide, fine-scaled frame.
        let imaging = SimulatorCamera::with_profile(
            CameraProfile {
                width: 200,
                height: 150,
                arcsec_per_pixel: 4.0,
                fwhm_arcsec: 12.0,
                field: sky,
                ..CameraProfile::fast()
            },
            Arc::new(pointing),
        )
        .expect("valid");
        imaging.connect().await.expect("connects");
        let result = imaging
            .capture(&CaptureRequest::new(dir.clone(), "shared_sky"))
            .await
            .expect("captures");

        // The guide camera: coarser, and with a field deliberately *inside* the imaging one — a
        // guider that saw further than the imaging chain could pick a star the imaging camera
        // was never going to contain, and the test would fail for a reason that is not a bug.
        // No injected star either: the point is that it finds a *catalogue* star, which is the
        // only way the two can be compared at all.
        let guide = camera_aimed(
            GuideCameraProfile {
                width: 60,
                height: 60,
                arcsec_per_pixel: 8.0,
                fwhm_arcsec: 20.0,
                guide_star_magnitude: None,
                seeing_arcsec: 0.0,
                field: sky,
                ..profile()
            },
            // The same pointing the imaging camera was given — which is the whole subject of
            // the test, and which the shared `PointingSource` is how a real deployment gets.
            pointing,
        );
        guide.connect().await.expect("connects");
        guide
            .set_exposure(Duration::from_secs(4))
            .await
            .expect("in range");
        let frame = guide.capture_frame().await.expect("a frame");

        // Where the guide camera says its brightest star is, in the sky.
        let (column, row) = centroid(&frame);
        let guide_sky =
            Projection::new(pointing, frame.width, frame.height, 8.0).to_sky(column, row);

        // ...and the imaging camera must have a star there.
        let samples = {
            use fitsrs::{Fits, Pixels, HDU};
            let file = std::fs::File::open(&result.raw().expect("raw").path).expect("exists");
            let mut hdus = Fits::from_reader(std::io::BufReader::new(file));
            let Some(Ok(HDU::Primary(hdu))) = hdus.next() else {
                panic!("no primary HDU");
            };
            let Pixels::I16(pixels) = hdus.get_data(&hdu).pixels() else {
                panic!("expected 16-bit samples");
            };
            let bottom_up: Vec<u16> = pixels.map(|s| (i32::from(s) + 32_768) as u16).collect();
            // **FITS rows run bottom-up.** The standard numbers rows from the bottom of the
            // image, so the file's first row is the frame's last one — and a consumer that
            // forgets it gets a frame mirrored in declination, which is precisely the failure
            // this comparison caught while it was being written. M1-T09's preview decoder has
            // the same obligation.
            bottom_up
                .chunks_exact(200)
                .rev()
                .flatten()
                .copied()
                .collect::<Vec<u16>>()
        };
        let imaging_projection = Projection::new(pointing, 200, 150, 4.0);
        let separation = detect::stars(&samples, 200, 150, 400.0)
            .into_iter()
            .map(|star| {
                let sky = imaging_projection.to_sky(star.column, star.row);
                let d_dec = sky.dec.degrees() - guide_sky.dec.degrees();
                let d_ra = (sky.ra.hours() - guide_sky.ra.hours())
                    * 15.0
                    * guide_sky.dec.degrees().to_radians().cos();
                (d_ra.hypot(d_dec) * 3600.0).abs()
            })
            .fold(f64::INFINITY, f64::min);
        assert!(
            separation < 10.0,
            "the guide star is {separation:.1}\" from the nearest star the imaging camera sees"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- acceptance: the registry ---------------------------------------------------------------

    #[test]
    fn all_three_simulator_drivers_register_under_one_name() {
        // Acceptance criterion 5, asserted for the whole family at once: `camera.driver`,
        // `mount.driver` and `guide_camera.driver` set to `simulator` must each resolve, or a
        // config that says "simulator" three times gets a device for some of them.
        use super::super::mount::SimulatorMountFactory;

        let mut registry = DriverRegistry::new();
        registry
            .register_mount(SimulatorMountFactory::new())
            .expect("registers");
        registry
            .register_camera(SimulatorCameraFactory::new())
            .expect("registers");
        registry
            .register_guide_camera(SimulatorGuideCameraFactory::new())
            .expect("registers");

        assert_eq!(registry.mount_drivers(), ["simulator"]);
        assert_eq!(registry.camera_drivers(), ["simulator"]);
        assert_eq!(registry.guide_camera_drivers(), ["simulator"]);
        assert_eq!(
            SimulatorGuideCameraFactory::new().name(),
            GuideCameraDriver::Simulator.as_str()
        );

        let config = default_config();
        let guide = registry
            .create_guide_camera(config.driver.expect("configured").as_str(), &config)
            .expect("builds");
        assert_eq!(guide.device_info().protocol, "simulator");
        assert_eq!(guide.capabilities().bit_depth, 12);
    }

    #[tokio::test]
    async fn the_factory_reports_itself() {
        let found = SimulatorGuideCameraFactory::new()
            .probe()
            .await
            .expect("probes");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, DeviceKind::GuideCamera);
    }

    #[test]
    fn a_configuration_naming_another_driver_is_refused() {
        let error = SimulatorGuideCameraFactory::new()
            .create(&GuideCameraConfig {
                driver: Some(GuideCameraDriver::Asi),
                ..default_config()
            })
            .expect_err("not this driver");
        assert!(error.to_string().contains("asi"), "{error}");
    }
}
