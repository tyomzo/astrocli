//! Test doubles for the three device traits, and the factories that build them.
//!
//! These exist to answer one question the trait definitions cannot answer by themselves: *is
//! this shape implementable and usable?* They are written the way a real driver is — state
//! behind a lock, no `&mut self`, `Arc<dyn …>` handed to spawned tasks — so that a signature
//! which forces an awkward implementation shows up here rather than in four driver crates.
//!
//! They are deliberately not published behind a feature flag. `SimulatorMount` and
//! `SimulatorCamera` (tasks M1-T02/T06) are the shared doubles for the rest of the workspace;
//! a second set maintained here would be the same thing built twice.
//!
//! Everything here produces frames only when asked, never on a timer, except the guide camera's
//! free-running mode — which has to be free-running to be tested at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::config::{
    CameraConfig, GuideCameraConfig, MountConfig, MountDriver, MountLimits, SerialConfig,
};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    AvailableSettings, BatteryStatus, CameraCapabilities, CameraSettings, DeviceInfo, DeviceKind,
    GuideCameraCapabilities, ImageFormat, MountCapabilities, MountState, MountStatus, RaDec,
    StorageInfo, TrackingMode,
};
use async_trait::async_trait;
use chrono::Utc;

use crate::camera::{
    Camera, CaptureRequest, CaptureResult, CapturedFile, CapturedFileKind, LiveViewFrame,
};
use crate::guide::{Binning, GuideCamera, GuideFrame, PixelLayout};
use crate::mount::MountDevice;
use crate::registry::{
    CameraFactory, DetectedDevice, DriverInitError, GuideCameraFactory, MountFactory,
};
use crate::stream::{FrameSink, FrameStream};

/// A `MountConfig` fixture. Field-for-field explicit rather than parsed from YAML: this is the
/// value a factory is handed, and a fixture that goes through the loader would be testing the
/// loader.
pub fn mount_config(driver: MountDriver) -> MountConfig {
    MountConfig {
        driver,
        port: "/dev/null".to_owned(),
        baud: 9600,
        settle_time_seconds: 2,
        serial: SerialConfig {
            request_timeout_ms: 500,
            request_retries: 1,
            heartbeat_misses: 3,
            poll_hz: 1,
        },
        limits: MountLimits {
            min_altitude_degrees: 15.0,
            meridian_limit_minutes: 5.0,
            max_travel_from_home_degrees: 180.0,
            slew_ttl_default_ms: 500,
            slew_ttl_max_ms: 2000,
        },
        geometry: None,
        indi_device: None,
        ascom_host: None,
    }
}

// -----------------------------------------------------------------------------------------
// Mount
// -----------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FakeMountState {
    connected: bool,
    slewing: bool,
    tracking: Option<TrackingMode>,
    log: Vec<String>,
}

/// A mount that records what it was asked to do.
#[derive(Debug, Default)]
pub struct FakeMount {
    state: Mutex<FakeMountState>,
}

impl FakeMount {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every method call, in order — the "simulator command log" a limits test asserts against
    /// (task M1-T05 acceptance: "mount never commanded").
    pub fn log(&self) -> Vec<String> {
        self.locked().log.clone()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, FakeMountState> {
        self.state.lock().expect("test double is never poisoned")
    }

    fn record(&self, what: &str) -> Result<(), DeviceError> {
        let mut state = self.locked();
        state.log.push(what.to_owned());
        if state.connected {
            Ok(())
        } else {
            Err(DeviceError::NotConnected)
        }
    }
}

#[async_trait]
impl MountDevice for FakeMount {
    async fn connect(&self) -> Result<(), DeviceError> {
        let mut state = self.locked();
        state.log.push("connect".to_owned());
        state.connected = true;
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        let mut state = self.locked();
        state.log.push("disconnect".to_owned());
        state.connected = false;
        Ok(())
    }

    async fn position(&self) -> Result<RaDec, DeviceError> {
        self.record("position")?;
        RaDec::from_parts(3.0, 45.0).map_err(|e| DeviceError::Protocol(e.to_string()))
    }

    async fn status(&self) -> Result<MountStatus, DeviceError> {
        let state = self.locked();
        if !state.connected {
            return Err(DeviceError::NotConnected);
        }
        Ok(MountStatus {
            state: if state.slewing {
                MountState::Slewing
            } else if state.tracking.is_some() {
                MountState::Tracking
            } else {
                MountState::Idle
            },
            tracking: state.tracking,
            slewing: state.slewing,
            parked: false,
        })
    }

    async fn goto(&self, _target: RaDec) -> Result<(), DeviceError> {
        {
            let mut state = self.locked();
            state.log.push("goto".to_owned());
            if !state.connected {
                return Err(DeviceError::NotConnected);
            }
            if state.slewing {
                return Err(DeviceError::Busy("slew in progress"));
            }
            state.slewing = true;
        }
        // Long enough that a test can drop the future mid-slew, which is the contract that
        // matters here: dropping must not stop the mount.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.locked().slewing = false;
        Ok(())
    }

    async fn sync(&self, _pos: RaDec) -> Result<(), DeviceError> {
        self.record("sync")
    }

    async fn start_tracking(&self, mode: TrackingMode) -> Result<(), DeviceError> {
        self.record("start_tracking")?;
        self.locked().tracking = Some(mode);
        Ok(())
    }

    async fn stop_tracking(&self) -> Result<(), DeviceError> {
        self.record("stop_tracking")?;
        self.locked().tracking = None;
        Ok(())
    }

    async fn slew(
        &self,
        _axis: astroctl_core::types::Axis,
        _dir: astroctl_core::types::Direction,
        _speed: astroctl_core::types::SlewSpeed,
    ) -> Result<(), DeviceError> {
        self.record("slew")?;
        self.locked().slewing = true;
        Ok(())
    }

    async fn stop_slew(&self, _axis: astroctl_core::types::Axis) -> Result<(), DeviceError> {
        self.record("stop_slew")?;
        self.locked().slewing = false;
        Ok(())
    }

    async fn guide_pulse(
        &self,
        _axis: astroctl_core::types::Axis,
        _dir: astroctl_core::types::Direction,
        _duration_ms: u32,
        _rate: astroctl_core::types::GuideRate,
    ) -> Result<(), DeviceError> {
        self.record("guide_pulse")
    }

    async fn park(&self) -> Result<(), DeviceError> {
        self.record("park")
    }

    async fn unpark(&self) -> Result<(), DeviceError> {
        self.record("unpark")
    }

    async fn emergency_stop(&self) -> Result<(), DeviceError> {
        // Never checks `connected` and never returns `Busy` — rule for this method, SDD §5.8.2.
        let mut state = self.locked();
        state.log.push("emergency_stop".to_owned());
        state.slewing = false;
        state.tracking = None;
        Ok(())
    }

    /// `None` — a fake with no mechanism has no home to measure travel from.
    fn axis_travel(&self) -> Option<astroctl_core::types::MountTravel> {
        None
    }

    /// `None` — a fake with no mechanism is on no side of a pier.
    fn pier_side(&self) -> Option<astroctl_core::types::PierSide> {
        None
    }

    /// `None` — likewise, a fake with no mechanism has no branch to project a motion through.
    fn motion_lookahead(
        &self,
        _axis: astroctl_core::types::Axis,
        _dir: astroctl_core::types::Direction,
        _degrees: f64,
    ) -> Option<astroctl_core::types::RaDec> {
        None
    }

    fn capabilities(&self) -> MountCapabilities {
        MountCapabilities {
            has_pec: false,
            has_pulse_guide: true,
            tracking_rates: vec![TrackingMode::Sidereal, TrackingMode::Lunar],
            max_slew_speed_x_sidereal: 800,
            position_resolution_bits: 24,
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "Fake Mount".to_owned(),
            model: "fake-1".to_owned(),
            // Learned at connect on a real driver, so absent here — the shape callers must
            // tolerate before anyone has pressed Connect.
            firmware: None,
            serial: None,
            protocol: "fake".to_owned(),
        }
    }
}

// -----------------------------------------------------------------------------------------
// Camera
// -----------------------------------------------------------------------------------------

#[derive(Debug)]
struct FakeCameraState {
    connected: bool,
    capturing: bool,
    format: ImageFormat,
    live_view: Option<Arc<FrameSink<LiveViewFrame>>>,
}

/// `ImageFormat` has no `Default` on purpose — which format a body powers up in is a driver
/// decision, not a domain one — so this double picks its own.
impl Default for FakeCameraState {
    fn default() -> Self {
        Self {
            connected: false,
            capturing: false,
            format: ImageFormat::Raw,
            live_view: None,
        }
    }
}

/// A camera that produces frames only when asked.
#[derive(Debug, Default)]
pub struct FakeCamera {
    state: Mutex<FakeCameraState>,
    hold: AtomicBool,
}

impl FakeCamera {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next capture park until released, so a test can observe the busy window.
    pub fn hold_capture(&self, hold: bool) {
        self.hold.store(hold, Ordering::SeqCst);
    }

    /// Whether an exposure is in flight.
    pub fn is_capturing(&self) -> bool {
        self.locked().capturing
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, FakeCameraState> {
        self.state.lock().expect("test double is never poisoned")
    }
}

#[async_trait]
impl Camera for FakeCamera {
    async fn connect(&self) -> Result<(), DeviceError> {
        self.locked().connected = true;
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        let mut state = self.locked();
        state.connected = false;
        state.live_view = None;
        Ok(())
    }

    async fn settings(&self) -> Result<CameraSettings, DeviceError> {
        let state = self.locked();
        if !state.connected {
            return Err(DeviceError::NotConnected);
        }
        Ok(CameraSettings {
            iso: "1600".to_owned(),
            shutter: "30".to_owned(),
            aperture: None,
            format: state.format,
        })
    }

    async fn available_settings(&self) -> Result<AvailableSettings, DeviceError> {
        Ok(AvailableSettings {
            isos: vec!["100".to_owned(), "1600".to_owned()],
            shutters: vec!["1/250".to_owned(), "30".to_owned(), "bulb".to_owned()],
            apertures: Vec::new(),
            formats: vec![ImageFormat::Raw, ImageFormat::RawPlusJpeg],
        })
    }

    async fn set_iso(&self, _iso: &str) -> Result<(), DeviceError> {
        Ok(())
    }

    async fn set_shutter(&self, _shutter: &str) -> Result<(), DeviceError> {
        Ok(())
    }

    async fn set_aperture(&self, _aperture: &str) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported)
    }

    async fn set_image_format(&self, format: ImageFormat) -> Result<(), DeviceError> {
        self.locked().format = format;
        Ok(())
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, DeviceError> {
        let format = {
            let mut state = self.locked();
            if !state.connected {
                return Err(DeviceError::NotConnected);
            }
            if state.capturing {
                return Err(DeviceError::Busy("capture in progress"));
            }
            state.capturing = true;
            state.format
        };

        while self.hold.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        let mut files = vec![CapturedFile {
            path: request.path_with_extension("cr3"),
            kind: CapturedFileKind::Raw,
            size_bytes: 32 * 1024 * 1024,
        }];
        if format == ImageFormat::RawPlusJpeg {
            files.push(CapturedFile {
                path: request.path_with_extension("jpg"),
                kind: CapturedFileKind::Jpeg,
                size_bytes: 4 * 1024 * 1024,
            });
        }

        let settings = CameraSettings {
            iso: "1600".to_owned(),
            shutter: "30".to_owned(),
            aperture: None,
            format,
        };
        self.locked().capturing = false;
        Ok(CaptureResult {
            files,
            settings,
            started_at: Utc::now(),
            exposure: Duration::from_secs(30),
        })
    }

    async fn capture_bulb(
        &self,
        request: &CaptureRequest,
        duration: Duration,
    ) -> Result<CaptureResult, DeviceError> {
        let mut result = self.capture(request).await?;
        result.exposure = duration;
        Ok(result)
    }

    async fn abort_capture(&self) -> Result<(), DeviceError> {
        self.hold.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn live_view_frame(&self) -> Result<LiveViewFrame, DeviceError> {
        let sink = {
            let state = self.locked();
            if !state.connected {
                return Err(DeviceError::NotConnected);
            }
            state.live_view.clone()
        };
        let frame = LiveViewFrame::new(vec![0xFF, 0xD8, 0xFF, 0xD9], Utc::now());
        // A real driver's stream runs on its own; this one publishes exactly when asked, so a
        // test never races an acquisition timer.
        if let Some(sink) = sink {
            sink.publish(frame.clone());
        }
        Ok(frame)
    }

    async fn live_view_stream(&self) -> Result<FrameStream<LiveViewFrame>, DeviceError> {
        let mut state = self.locked();
        if !state.connected {
            return Err(DeviceError::NotConnected);
        }
        if let Some(sink) = &state.live_view {
            // Second subscriber joins the one stream rather than starting another acquisition.
            return Ok(sink.subscribe());
        }
        let (sink, stream) = FrameStream::channel();
        state.live_view = Some(Arc::new(sink));
        Ok(stream)
    }

    async fn stop_live_view(&self) -> Result<(), DeviceError> {
        self.locked().live_view = None;
        Ok(())
    }

    async fn battery(&self) -> Result<BatteryStatus, DeviceError> {
        Ok(BatteryStatus {
            percent: 87,
            charging: false,
        })
    }

    async fn storage(&self) -> Result<StorageInfo, DeviceError> {
        Ok(StorageInfo {
            free_mb: 12_000,
            total_mb: 64_000,
        })
    }

    async fn sensor_temperature_celsius(&self) -> Result<Option<f64>, DeviceError> {
        Ok(None)
    }

    fn capabilities(&self) -> CameraCapabilities {
        CameraCapabilities {
            has_bulb: true,
            has_live_view: true,
            has_mirror_lockup: false,
            sensor_width_px: 6000,
            sensor_height_px: 4000,
            pixel_size_um: 3.72,
            supported_formats: vec![ImageFormat::Raw, ImageFormat::RawPlusJpeg],
            min_iso: 100,
            max_iso: 32_000,
            min_shutter_s: 1.0 / 4000.0,
            max_shutter_s: 30.0,
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "Fake Camera".to_owned(),
            model: "fake-cam".to_owned(),
            firmware: None,
            serial: Some("SN-0001".to_owned()),
            protocol: "fake".to_owned(),
        }
    }
}

// -----------------------------------------------------------------------------------------
// Guide camera
// -----------------------------------------------------------------------------------------

/// A guide camera with a genuinely free-running mode, because that is the part of the trait a
/// synchronous double would not exercise.
#[derive(Debug)]
pub struct FakeGuideCamera {
    connected: AtomicBool,
    running: Arc<AtomicBool>,
    binning: Mutex<Binning>,
    stream: Mutex<Option<FrameStream<GuideFrame>>>,
}

impl Default for FakeGuideCamera {
    fn default() -> Self {
        Self {
            connected: AtomicBool::new(false),
            running: Arc::new(AtomicBool::new(false)),
            binning: Mutex::new(Binning::NONE),
            stream: Mutex::new(None),
        }
    }
}

impl FakeGuideCamera {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn frame(binning: Binning) -> GuideFrame {
        let width = 64 / u32::from(binning.x());
        let height = 48 / u32::from(binning.y());
        GuideFrame {
            width,
            height,
            pixels: Arc::from(vec![0_u16; (width * height) as usize]),
            layout: PixelLayout::Mono,
            binning,
            exposure: Duration::from_millis(500),
            gain: 100,
            started_at: Utc::now(),
        }
    }
}

#[async_trait]
impl GuideCamera for FakeGuideCamera {
    async fn connect(&self) -> Result<(), DeviceError> {
        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        self.connected.store(false, Ordering::SeqCst);
        self.stop_continuous().await
    }

    async fn set_exposure(&self, _exposure: Duration) -> Result<(), DeviceError> {
        Ok(())
    }

    async fn set_gain(&self, gain: u32) -> Result<(), DeviceError> {
        if gain > self.capabilities().max_gain {
            return Err(DeviceError::Rejected(format!("gain {gain} above maximum")));
        }
        Ok(())
    }

    async fn set_binning(&self, binning: Binning) -> Result<(), DeviceError> {
        let max = self.capabilities().max_binning;
        if binning.x() > max || binning.y() > max {
            return Err(DeviceError::Unsupported);
        }
        *self.binning.lock().expect("not poisoned") = binning;
        Ok(())
    }

    async fn capture_frame(&self) -> Result<GuideFrame, DeviceError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(DeviceError::NotConnected);
        }
        Ok(Self::frame(*self.binning.lock().expect("not poisoned")))
    }

    async fn start_continuous(&self) -> Result<FrameStream<GuideFrame>, DeviceError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(DeviceError::NotConnected);
        }
        let mut held = self.stream.lock().expect("not poisoned");
        if let Some(stream) = held.as_ref() {
            return Ok(stream.subscribe());
        }

        let (sink, stream) = FrameStream::channel();
        let binning = *self.binning.lock().expect("not poisoned");
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                sink.publish(Self::frame(binning));
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            // Dropping the sink here is what ends every subscriber's stream.
        });
        *held = Some(stream.clone());
        Ok(stream)
    }

    async fn stop_continuous(&self) -> Result<(), DeviceError> {
        self.running.store(false, Ordering::SeqCst);
        *self.stream.lock().expect("not poisoned") = None;
        Ok(())
    }

    async fn set_cooling(&self, _enabled: bool, _target_celsius: f64) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported)
    }

    async fn temperature_celsius(&self) -> Result<Option<f64>, DeviceError> {
        Ok(Some(11.5))
    }

    fn capabilities(&self) -> GuideCameraCapabilities {
        GuideCameraCapabilities {
            sensor_width_px: 64,
            sensor_height_px: 48,
            pixel_size_um: 3.75,
            max_gain: 600,
            bit_depth: 12,
            max_binning: 4,
            has_cooling: false,
            min_exposure_seconds: 0.001,
            max_exposure_seconds: 60.0,
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "Fake Guide Camera".to_owned(),
            model: "fake-guide".to_owned(),
            firmware: None,
            serial: None,
            protocol: "fake".to_owned(),
        }
    }
}

// -----------------------------------------------------------------------------------------
// Factories
// -----------------------------------------------------------------------------------------

/// A mount factory that can be told to answer to any name, to refuse its configuration, or to
/// fail its probe — the three outcomes the registry has to distinguish.
pub struct FakeMountFactory {
    name: &'static str,
    refusal: Option<&'static str>,
    probe_fails: bool,
}

impl FakeMountFactory {
    #[must_use]
    pub fn named(name: &'static str) -> Self {
        Self {
            name,
            refusal: None,
            probe_fails: false,
        }
    }

    #[must_use]
    pub fn refusing(name: &'static str, reason: &'static str) -> Self {
        Self {
            name,
            refusal: Some(reason),
            probe_fails: false,
        }
    }

    #[must_use]
    pub fn unprobeable(name: &'static str) -> Self {
        Self {
            name,
            refusal: None,
            probe_fails: true,
        }
    }
}

#[async_trait]
impl MountFactory for FakeMountFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn create(&self, _config: &MountConfig) -> Result<Arc<dyn MountDevice>, DriverInitError> {
        match self.refusal {
            Some(reason) => Err(DriverInitError::new(reason)),
            None => Ok(Arc::new(FakeMount::new())),
        }
    }

    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        if self.probe_fails {
            return Err(DeviceError::Transport(
                "cannot read /dev: permission denied".to_owned(),
            ));
        }
        Ok(vec![DetectedDevice::new(
            // Deliberately wrong: the registry stamps the registered name over it.
            "not-my-name",
            DeviceKind::Mount,
            self.name,
            "Fake Mount",
        )])
    }
}

/// A camera factory that finds one camera.
pub struct FakeCameraFactory;

#[async_trait]
impl CameraFactory for FakeCameraFactory {
    fn name(&self) -> &'static str {
        "simulator"
    }

    fn create(&self, _config: &CameraConfig) -> Result<Arc<dyn Camera>, DriverInitError> {
        Ok(Arc::new(FakeCamera::new()))
    }

    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        Ok(vec![DetectedDevice::new(
            "simulator",
            DeviceKind::Camera,
            "usb:001,014",
            "Fake Camera",
        )])
    }
}

/// A guide camera factory that does not implement `probe` at all — the default-body case.
pub struct FakeGuideCameraFactory;

#[async_trait]
impl GuideCameraFactory for FakeGuideCameraFactory {
    fn name(&self) -> &'static str {
        "simulator"
    }

    fn create(&self, _config: &GuideCameraConfig) -> Result<Arc<dyn GuideCamera>, DriverInitError> {
        Ok(Arc::new(FakeGuideCamera::new()))
    }
}
