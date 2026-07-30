//! `SimulatorCamera` — a [`Camera`] that produces real files and takes real time (PRD §4.5,
//! HAL-11, SDD §9).
//!
//! # What it is faithful to
//!
//! Not a protocol: PTP over USB is the gphoto2 driver's problem (M2), and simulating its bytes
//! would test the codec twice. What this reproduces is what a caller above the HAL can observe,
//! and would otherwise get wrong:
//!
//! * **A capture takes as long as a capture.** The exposure, then the download the spike
//!   measured at 2.08 s. A caller written against an instant camera is a caller that assumes a
//!   sequence of sixty 30-second frames takes thirty minutes, and then reports the field node as
//!   broken when it takes thirty-two.
//! * **The camera is occupied while it is capturing.** A second capture is `Busy`, live view
//!   stops and resumes (SDD §5.7 calls the gap expected), and a setting cannot be changed
//!   mid-exposure. One imaging path, as the hardware has.
//! * **Frames are durable files, not bytes.** Written under a temporary name, fsynced, renamed,
//!   the directory fsynced (SDD §5.3.2) — so a consumer never sees a partial frame, which is
//!   what REL-05 and REL-11 actually mean.
//! * **The frames contain a sky.** Stars from the [catalogue](super::sky), placed by where the
//!   *mount* is pointed, with Poisson and read noise on top. A pipeline developed against
//!   gradient-free grey rectangles is a pipeline nobody has tested.
//! * **It fails the way a camera fails** — [`CameraFaultPlan`] scripts a refused exposure, a
//!   download that crawls, and a body that falls off the USB bus mid-session.
//!
//! # For the layer above (the capture flow, M1-T08)
//!
//! Four things are easy to get wrong from the outside:
//!
//! * **An aborted capture returns [`DeviceError::Aborted`]**, not `Rejected`. The `Camera` trait
//!   docs say `Rejected`, and they were written (M1-T01) before `Aborted` existed — M1-T03 added
//!   it for the mount's identical case, because `Rejected` maps to `DEVICE_REJECTED`/422 and
//!   tells the operator their *request* was bad at the moment their abort worked. `Aborted` maps
//!   to `ABORTED`/409. The trait documentation is what needs correcting, not this driver.
//! * **The abort window ends when the file starts being written.** An abort during the exposure
//!   or the download resolves promptly and leaves nothing on disk. An abort that arrives while
//!   the frame is being written loses the race and the capture succeeds — which is SDD §7's
//!   shutdown rule ("if capturing, finish download … a half-downloaded frame is a lost frame")
//!   applied to the same instant.
//! * **Dropping the capture future does not stop the exposure**, but it does release the camera
//!   when the exposure ends, exactly as `abort_capture` would. The simulator has no way to keep
//!   computing a frame nobody is waiting for, and no reason to.
//! * **A capture with the shutter on `"bulb"` is `Rejected`**, pointing at
//!   [`capture_bulb`](Camera::capture_bulb). The spike found the R10's mode dial constrains what
//!   the API can set — with the dial on Bulb, `shutterspeed` offers only `bulb` — so a caller
//!   that assumes a settable shutter speed will meet this on real hardware too.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use astroctl_core::config::{CameraConfig, CameraDriver, CameraTimeouts};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    AvailableSettings, BatteryStatus, CameraCapabilities, CameraSettings, DeviceInfo, DeviceKind,
    ImageFormat, RaDec, StorageInfo,
};
use astroctl_hal::camera::{
    Camera, CaptureRequest, CaptureResult, CapturedFile, CapturedFileKind, LiveViewFrame,
};
use astroctl_hal::registry::{CameraFactory, DetectedDevice, DriverInitError};
use astroctl_hal::stream::{FrameSink, FrameStream};
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::fault::{ArmedCameraFaults, CameraFaultPlan};
use super::fits::FrameMetadata;
use super::imaging::{CameraThread, CaptureOutput, Job};
use super::profile::CameraProfile;
use super::sky::{Exposure, PointingSource};

/// Signal a live-view frame integrates, regardless of the shutter setting.
///
/// A real body's live view is a short integration read out continuously and amplified hard; it
/// is not the exposure the shutter is set to, and it is not photometric. One second of synthetic
/// signal is the value that makes stars visible for framing, which is the entire purpose of the
/// stream (CAM-05). A live view showing an honest 1/30 s of a star field would be a black
/// rectangle, i.e. a framing aid that cannot be used to frame.
const LIVE_VIEW_INTEGRATION: Duration = Duration::from_secs(1);

/// Stream key separating the imaging camera's noise from the guide camera's.
///
/// Without it, two cameras rendering the same frame index would draw the *same* noise, and a
/// test comparing the two sensors would be comparing one sensor to itself.
const NOISE_STREAM: u64 = 0x494D_4147_494E_4700; // "IMAGING\0"

/// ADU per electron at ISO 100.
///
/// **Invented, and calibrated to one thing:** it puts ISO 1600 — the PRD's default and the ISO
/// the reference rig actually shoots — at 4 ADU/e⁻, so a 30,000 e⁻ full well fills 60% of the
/// 16-bit range and a bright star clips before the well does. That is the regime a DSLR
/// astrophotographer works in, and it is what makes the ISO knob change something a test can see.
const GAIN_AT_ISO_100: f64 = 0.25;

// -----------------------------------------------------------------------------------------
// Settings the body offers
// -----------------------------------------------------------------------------------------

/// The ISO tokens the camera offers.
///
/// **The count is measured, the values are not.** The spike found 27 ISO choices on the R10 but
/// never dumped them (FINDINGS.md step 2 records counts only). This is the standard Canon
/// third-stop ladder to the R10's native 32000 plus the expanded 51200 — 27 tokens, which is the
/// right number for the right reason, but any individual token is plausible rather than
/// confirmed.
const ISO_TOKENS: &[&str] = &[
    "100", "125", "160", "200", "250", "320", "400", "500", "640", "800", "1000", "1250", "1600",
    "2000", "2500", "3200", "4000", "5000", "6400", "8000", "10000", "12800", "16000", "20000",
    "25600", "32000", "51200",
];

/// The shutter tokens the camera offers, fastest first, `"bulb"` last.
///
/// Fractions below a second and decimals above, which is how every Canon body spells them and
/// therefore how a caller's string comparison has to work. `"bulb"` is in the list because the
/// body offers it as a shutter *setting*; selecting it and then calling
/// [`Camera::capture`](astroctl_hal::camera::Camera::capture) is refused, because a bulb exposure
/// needs a duration and `capture` has nowhere to put one.
const SHUTTER_TOKENS: &[&str] = &[
    "1/4000", "1/3200", "1/2500", "1/2000", "1/1600", "1/1250", "1/1000", "1/800", "1/640",
    "1/500", "1/400", "1/320", "1/250", "1/200", "1/160", "1/125", "1/100", "1/80", "1/60", "1/50",
    "1/40", "1/30", "1/25", "1/20", "1/15", "1/13", "1/10", "1/8", "1/6", "1/5", "1/4", "1/3",
    "0.4", "0.5", "0.6", "0.8", "1", "1.3", "1.6", "2", "2.5", "3.2", "4", "5", "6", "8", "10",
    "13", "15", "20", "25", "30", "bulb",
];

/// The aperture tokens the camera offers.
///
/// **The count is measured** — 22 choices, with the Sigma 23 mm F1.4 the spike had mounted — and
/// the f/1.4 to f/16 third-stop ladder is exactly 22 values, which is why these are the tokens.
const APERTURE_TOKENS: &[&str] = &[
    "1.4", "1.6", "1.8", "2", "2.2", "2.5", "2.8", "3.2", "3.5", "4", "4.5", "5", "5.6", "6.3",
    "7.1", "8", "9", "10", "11", "13", "14", "16",
];

/// Turns a shutter token into a duration, or `None` for `"bulb"`.
fn shutter_seconds(token: &str) -> Option<f64> {
    if token.eq_ignore_ascii_case("bulb") {
        return None;
    }
    if let Some((numerator, denominator)) = token.split_once('/') {
        let numerator: f64 = numerator.trim().parse().ok()?;
        let denominator: f64 = denominator.trim().parse().ok()?;
        if denominator == 0.0 {
            return None;
        }
        return Some(numerator / denominator);
    }
    token.parse().ok()
}

/// Turns an ISO token into the gain it selects, in ADU per electron.
fn gain_for_iso(token: &str) -> f64 {
    // An unparseable token cannot reach here — the setters only accept values from the list —
    // but falling back to ISO 100 rather than panicking keeps a future editing mistake out of
    // the capture path.
    let iso: f64 = token.parse().unwrap_or(100.0);
    GAIN_AT_ISO_100 * iso / 100.0
}

// -----------------------------------------------------------------------------------------
// Internal state
// -----------------------------------------------------------------------------------------

/// What the driver believes about its USB link.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Link {
    /// Never opened, or closed cleanly.
    Down,
    /// Open. `drops_at` is armed by [`CameraFault::DisconnectAfter`](super::fault::CameraFault).
    Up { drops_at: Option<Instant> },
    /// Opened and then lost — the cable-pull case the spike measured, where every retry on the
    /// old handle failed and only a fresh context recovered.
    Faulted,
}

/// A running live-view stream.
#[derive(Debug)]
struct LiveView {
    /// A cursor kept so that a second `live_view_stream()` call subscribes to the running stream
    /// rather than starting a second one — the camera has one imaging path.
    stream: FrameStream<LiveViewFrame>,
    /// The producer loop. Aborting it drops the sink, which is what ends every subscriber's
    /// stream (see [`FrameStream`]).
    task: JoinHandle<()>,
}

/// Everything behind the lock.
#[derive(Debug)]
struct State {
    link: Link,
    faults: ArmedCameraFaults,
    settings: CameraSettings,
    /// The camera's own thread, alive between `connect` and `disconnect`.
    thread: Option<Arc<CameraThread>>,
    live_view: Option<LiveView>,
    /// Frames produced since construction — the noise stream key. Incremented by captures *and*
    /// live-view frames, so no two synthetic frames share a noise realisation.
    frames: u64,
    /// Whether an exposure is in progress.
    ///
    /// Separate from the imaging permit, and the separation is the point. The permit answers
    /// "is the sensor in use *this instant*", which includes the milliseconds a live-view frame
    /// is being generated. `Busy` answers a different question — "is this camera in the middle
    /// of an exposure" — and conflating them makes a capture issued while a preview happened to
    /// be rendering fail with `Busy` perhaps one time in ten. That is a real camera's behaviour
    /// nowhere: libgphoto2 would serialise the two on its single context, not refuse the second.
    capturing: bool,
}

impl State {
    fn connected(&self) -> Result<(), DeviceError> {
        match self.link {
            Link::Up { .. } => Ok(()),
            Link::Down => Err(DeviceError::NotConnected),
            Link::Faulted => Err(link_lost()),
        }
    }

    fn thread(&self) -> Result<Arc<CameraThread>, DeviceError> {
        self.connected()?;
        self.thread.clone().ok_or(DeviceError::NotConnected)
    }
}

/// Marks the camera as exposing for as long as it lives.
///
/// A guard rather than a pair of calls because `run_capture` returns from five places, four of
/// them errors: a scripted failure, an abort, a download timeout, a dead thread. A flag cleared
/// by hand would survive one of those and leave the camera permanently `Busy` — which is the
/// failure mode that ends a night's imaging, since every subsequent frame is refused.
#[derive(Debug)]
struct CaptureClaim<'a> {
    shared: &'a Shared,
}

impl<'a> CaptureClaim<'a> {
    /// Claims the camera, or reports what it is already doing.
    fn take(shared: &'a Shared) -> Result<Self, DeviceError> {
        let mut state = shared.locked();
        if state.capturing {
            return Err(DeviceError::Busy("the camera is already exposing"));
        }
        state.capturing = true;
        Ok(Self { shared })
    }
}

impl Drop for CaptureClaim<'_> {
    fn drop(&mut self) {
        self.shared.locked().capturing = false;
    }
}

/// The error a lost camera produces.
///
/// `Transport` rather than `NotConnected`, and the wording is the spike's: with the cable out,
/// libgphoto2 says "Could not find the requested device on the USB port", and with the device
/// claimed by something else it says "Could not claim the USB device". Those are different
/// operator actions — check the cable, or kill `gvfs-gphoto2` — so the message has to
/// distinguish them even in a simulator.
fn link_lost() -> DeviceError {
    DeviceError::Transport(
        "could not find the camera on the USB port (simulated disconnect)".to_owned(),
    )
}

/// Everything the driver and its live-view loop both need.
///
/// Split out from [`SimulatorCamera`] so the loop can hold an `Arc` of it: a spawned task cannot
/// borrow the driver, and a `Weak<SimulatorCamera>` would make every access an upgrade that can
/// fail for a reason the loop cannot report.
#[derive(Debug)]
struct Shared {
    profile: CameraProfile,
    timeouts: CameraTimeouts,
    pointing: Arc<dyn PointingSource>,
    /// The one imaging path — the single context of SDD §5.3.1, which everything that touches
    /// the sensor queues on. A capture holds it for the whole exposure and download; live view
    /// takes it per frame and skips the frames it cannot get, which is what makes the documented
    /// gap during a capture a property of the model rather than a special case. See
    /// [`State::capturing`] for why refusal and serialisation are two different mechanisms.
    imaging: Semaphore,
    /// Bumped by [`Camera::abort_capture`]. A capture waiting on the exposure or the download
    /// watches this and gives up when it changes — a generation counter rather than a flag, so
    /// an abort issued while nothing is capturing cannot arm itself for the *next* capture.
    aborts: watch::Sender<u64>,
    state: Mutex<State>,
}

impl Shared {
    fn locked(&self) -> MutexGuard<'_, State> {
        // Poisoning would mean a panic inside one of these short synchronous sections, which
        // would be a bug in this file rather than a condition to recover from.
        self.state.lock().expect("camera state is never poisoned")
    }

    /// Checks the link, applying the scripted disconnect if its time has come.
    fn check_link(&self) -> Result<(), DeviceError> {
        let mut state = self.locked();
        if let Link::Up { drops_at: Some(at) } = state.link {
            if Instant::now() >= at {
                state.link = Link::Faulted;
                return Err(link_lost());
            }
        }
        state.connected()
    }

    /// Where the telescope is aimed, or the profile's default if nothing can say.
    fn pointing(&self) -> RaDec {
        self.pointing
            .pointing()
            .unwrap_or(self.profile.default_pointing)
    }

    /// Builds the exposure to render: the sky, the optics, the settings and the noise stream.
    fn plan(
        &self,
        width: u32,
        height: u32,
        exposure: Duration,
        settings: &CameraSettings,
    ) -> Exposure {
        let mut state = self.locked();
        state.frames += 1;
        let frame = state.frames;
        drop(state);

        // A live-view frame is smaller than the sensor but looks at the *same* field, so its
        // plate scale is stretched to cover it. A preview showing a crop of the frame would make
        // framing by live view produce a photograph of somewhere else.
        let scale =
            self.profile.arcsec_per_pixel * f64::from(self.profile.width) / f64::from(width);
        Exposure {
            width,
            height,
            pointing: self.pointing(),
            arcsec_per_pixel: scale,
            exposure,
            gain_adu_per_electron: gain_for_iso(&settings.iso),
            fwhm_arcsec: self.profile.fwhm_arcsec,
            sky_electrons_per_second: self.profile.sky_electrons_per_second,
            read_noise_electrons: self.profile.read_noise_electrons,
            bias_adu: self.profile.bias_adu,
            full_well_electrons: self.profile.full_well_electrons,
            saturation_adu: u16::MAX,
            noise_seed: Some(NOISE_STREAM ^ frame),
            jitter_arcsec: (0.0, 0.0),
            injected: Vec::new(),
            field: self.profile.field,
        }
    }

    /// Takes the imaging path for an operation that is not itself a capture.
    ///
    /// `Busy` if an exposure is running — the trait's contract for a setting change or a
    /// one-shot preview during a capture — and otherwise a wait, bounded by whatever short
    /// operation currently holds the sensor.
    async fn take_imaging_path(&self) -> Result<tokio::sync::SemaphorePermit<'_>, DeviceError> {
        if self.locked().capturing {
            return Err(DeviceError::Busy("the camera is exposing"));
        }
        self.imaging
            .acquire()
            .await
            .map_err(|_| DeviceError::Busy("the camera is exposing"))
    }

    /// Waits out `span`, or returns as soon as an abort is issued.
    ///
    /// `timeout` rather than `select!`: the macro would pull tokio's `macros` feature into a
    /// library that otherwise needs `sync`, `time` and `rt`, for a two-branch race that
    /// `timeout` expresses exactly. Resolving *early* is the abort; timing out is the wait
    /// completing normally, which reads backwards for about a second and then never again.
    async fn wait_or_abort(&self, span: Duration) -> Result<(), DeviceError> {
        let mut aborts = self.aborts.subscribe();
        match tokio::time::timeout(span, aborts.changed()).await {
            Err(_elapsed) => Ok(()),
            Ok(_aborted) => Err(DeviceError::Aborted(
                "the capture was aborted by the operator".to_owned(),
            )),
        }
    }
}

// -----------------------------------------------------------------------------------------
// The driver
// -----------------------------------------------------------------------------------------

/// A camera that produces synthetic star fields and is not there (PRD §4.5, HAL-11).
///
/// Build it directly in a test — `SimulatorCamera::new(&config, profile, faults, pointing)` — or
/// through [`SimulatorCameraFactory`] when it has to come from `camera.driver: simulator` in the
/// operator's YAML.
#[derive(Debug)]
pub struct SimulatorCamera {
    shared: Arc<Shared>,
}

impl SimulatorCamera {
    /// Builds a camera from the same configuration a real driver gets, plus the three things
    /// SDD §9 and PRD §4.5 require to be constructor parameters: the timing profile, the fault
    /// plan, and where the telescope is pointed.
    ///
    /// # Errors
    /// [`DriverInitError`] if `camera.default_iso`, `default_shutter` or `default_format` names
    /// something this body does not offer — a startup failure whose fix is an edit to YAML, so
    /// the message names the value and the alternatives.
    pub fn new(
        config: &CameraConfig,
        profile: CameraProfile,
        faults: &CameraFaultPlan,
        pointing: Arc<dyn PointingSource>,
    ) -> Result<Self, DriverInitError> {
        let format = parse_format(&config.default_format)?;
        if !ISO_TOKENS.contains(&config.default_iso.as_str()) {
            return Err(DriverInitError::new(format!(
                "`camera.default_iso: {}` is not an ISO this body offers; try one of {}",
                config.default_iso,
                ISO_TOKENS.join(", ")
            )));
        }
        if !SHUTTER_TOKENS.contains(&config.default_shutter.as_str()) {
            return Err(DriverInitError::new(format!(
                "`camera.default_shutter: {}` is not a shutter speed this body offers; \
                 try `30`, `1/250` or `bulb`",
                config.default_shutter
            )));
        }

        Ok(Self {
            shared: Arc::new(Shared {
                profile,
                timeouts: config.timeouts,
                pointing,
                imaging: Semaphore::new(1),
                aborts: watch::channel(0).0,
                state: Mutex::new(State {
                    link: Link::Down,
                    faults: faults.arm(),
                    settings: CameraSettings {
                        iso: config.default_iso.clone(),
                        shutter: config.default_shutter.clone(),
                        // The reference body has an electronic aperture because the spike's lens
                        // did; a manual lens reports `None` and the trait's `Unsupported` arm.
                        aperture: Some("5.6".to_owned()),
                        format,
                    },
                    thread: None,
                    live_view: None,
                    frames: 0,
                    capturing: false,
                }),
            }),
        })
    }

    /// A camera on the default configuration, for callers that only want a camera.
    ///
    /// # Errors
    /// [`DriverInitError`] as [`new`](Self::new); unreachable with the built-in defaults.
    pub fn with_profile(
        profile: CameraProfile,
        pointing: Arc<dyn PointingSource>,
    ) -> Result<Self, DriverInitError> {
        Self::new(
            &default_config(),
            profile,
            &CameraFaultPlan::none(),
            pointing,
        )
    }

    /// Charges the latency of one settings-tree operation and checks the link.
    async fn config_exchange(&self, cost: Duration) -> Result<(), DeviceError> {
        self.shared.check_link()?;
        // Refused during an exposure — on one context, reading the configuration while a capture
        // runs is not possible, and a caller that expects it to be will find that out here rather
        // than on hardware (SDD §5.3.1: "one context means one queue"). But *queued* behind a
        // live-view frame rather than refused: see [`State::capturing`]. The test that pins this
        // down is `a_live_view_frame_in_flight_delays_a_capture_rather_than_refusing_it`, and it
        // was checked against the wrong version — with a `try_acquire` here the second capture
        // of five is refused.
        let _permit = self.shared.take_imaging_path().await?;
        tokio::time::sleep(cost).await;
        Ok(())
    }

    /// Sets one of the token-valued settings, refusing anything the body does not offer.
    async fn set_token(
        &self,
        name: &str,
        value: &str,
        allowed: &[&str],
        apply: impl FnOnce(&mut CameraSettings),
    ) -> Result<(), DeviceError> {
        self.config_exchange(self.shared.profile.setting).await?;
        if !allowed.contains(&value) {
            // Never a silent substitution of the nearest value: a frame shot at an exposure the
            // operator did not ask for is a frame they cannot detect is wrong.
            return Err(DeviceError::Rejected(format!(
                "`{value}` is not a {name} this camera offers"
            )));
        }
        apply(&mut self.shared.locked().settings);
        Ok(())
    }

    /// The body of both [`capture`](Camera::capture) and [`capture_bulb`](Camera::capture_bulb).
    ///
    /// The sequence, and where each step can fail:
    ///
    /// 1. claim the camera, or `Busy` — decided from state before any `.await`, so that two
    ///    captures issued a microsecond apart cannot both pass, then queue for the sensor
    ///    itself, which a live-view frame may hold for a millisecond;
    /// 2. the scripted capture failure, if this frame is the one;
    /// 3. the exposure — abortable;
    /// 4. the download — abortable, and bounded by `camera.timeouts.download_seconds`;
    /// 5. render and write, on the camera's own thread — not abortable, because from here the
    ///    frame exists and abandoning it would lose it.
    async fn run_capture(
        &self,
        request: &CaptureRequest,
        exposure: Duration,
        shutter: String,
    ) -> Result<CaptureResult, DeviceError> {
        let shared = &self.shared;
        shared.check_link()?;
        // The claim is a state change, so `Busy` cannot depend on how the runtime happened to
        // interleave two callers; the guard clears it however this function leaves — and it has
        // five ways to leave, four of them errors.
        let _claim = CaptureClaim::take(shared)?;
        let permit = shared
            .imaging
            .acquire()
            .await
            .map_err(|_| DeviceError::Busy("the camera is already exposing"))?;

        let (thread, settings) = {
            let mut state = shared.locked();
            if let Some(scripted) = state.faults.next_capture() {
                return Err(scripted);
            }
            let mut settings = state.settings.clone();
            settings.shutter = shutter;
            (state.thread()?, settings)
        };

        // The pointing is sampled here, at shutter-open, and the whole frame is planned from it.
        // A frame planned after the exposure would show the sky the mount reached, which for a
        // slewing mount is a different sky (and for a tracking one, the same — which is why the
        // bug would survive every test that does not slew).
        let started_at = Utc::now();
        let plan = shared.plan(
            shared.profile.width,
            shared.profile.height,
            exposure,
            &settings,
        );

        shared.wait_or_abort(exposure).await?;

        let download = shared
            .profile
            .download
            .mul_f64(shared.locked().faults.take_download_factor());
        let budget = Duration::from_secs(shared.timeouts.download_seconds);
        if download > budget {
            // SDD §5.3.1 treats a breached operation-class timeout as a wedged camera, which the
            // facade above recovers from by rebuilding the driver. Reporting the *budget* rather
            // than the download's own length is deliberate: the caller is being told what it
            // waited, not what the camera was secretly going to do.
            tokio::time::sleep(budget).await;
            return Err(DeviceError::Timeout(budget));
        }
        shared.wait_or_abort(download).await?;

        let output = CaptureOutput {
            dir: request.dir.clone(),
            stem: request.stem.clone(),
            fits: matches!(settings.format, ImageFormat::Raw | ImageFormat::RawPlusJpeg),
            jpeg: matches!(
                settings.format,
                ImageFormat::Jpeg | ImageFormat::RawPlusJpeg
            ),
            meta: FrameMetadata {
                started_at,
                exposure_seconds: exposure.as_secs_f64(),
                pointing: plan.pointing,
                iso: settings.iso.clone(),
                gain_adu_per_electron: plan.gain_adu_per_electron,
                pixel_size_um: shared.profile.pixel_size_um,
                focal_length_mm: shared.profile.focal_length_mm,
                sensor_temperature_celsius: shared.profile.sensor_temperature_celsius,
                sky_seed: shared.profile.field.seed(),
            },
        };
        let written = thread
            .submit(|reply| Job::Capture {
                exposure: Box::new(plan),
                output,
                reply,
            })
            .await?;
        drop(permit);

        Ok(CaptureResult {
            files: written
                .into_iter()
                .map(|file| CapturedFile {
                    path: file.path,
                    kind: if file.is_raw {
                        CapturedFileKind::Raw
                    } else {
                        CapturedFileKind::Jpeg
                    },
                    size_bytes: file.size_bytes,
                })
                .collect(),
            settings,
            started_at,
            exposure,
        })
    }

    /// Renders one live-view frame through the camera's thread.
    async fn preview(shared: &Arc<Shared>) -> Result<LiveViewFrame, DeviceError> {
        let (settings, thread) = {
            let state = shared.locked();
            (state.settings.clone(), state.thread()?)
        };
        let plan = shared.plan(
            shared.profile.live_view_width,
            shared.profile.live_view_height,
            LIVE_VIEW_INTEGRATION,
            &settings,
        );
        let jpeg = thread
            .submit(|reply| Job::Preview {
                exposure: Box::new(plan),
                reply,
            })
            .await?;
        Ok(LiveViewFrame::new(jpeg, Utc::now()))
    }

    /// The live-view producer loop.
    ///
    /// Runs until it is aborted by [`stop_live_view`](Camera::stop_live_view) or
    /// [`disconnect`](Camera::disconnect). It takes the imaging path per frame and **skips** the
    /// frames it cannot get, which is how "live view pauses for the duration of a capture"
    /// (SDD §5.7) happens without anything coordinating it.
    async fn live_view_loop(shared: Arc<Shared>, sink: FrameSink<LiveViewFrame>) {
        let period = Duration::from_secs_f64(1.0 / shared.profile.live_view_fps.max(0.001));
        let mut ticker = tokio::time::interval(period);
        // Burst would fire the missed ticks back to back after a capture, producing a flurry of
        // frames whose timestamps all say "now" — the opposite of what a preview is for.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if shared.check_link().is_err() {
                // The camera went away. Returning drops the sink, which ends every subscriber's
                // stream — the same signal they get from `stop_live_view`, because from a
                // consumer's side they are the same event: no more frames.
                return;
            }
            // `try_acquire`, not `acquire`: a frame the sensor was busy for is a frame that
            // should never be taken, not one to take late. Waiting would queue previews behind a
            // five-minute exposure and then deliver a burst of them, all stale.
            let Ok(_permit) = shared.imaging.try_acquire() else {
                continue; // a capture owns the sensor; this is the expected gap
            };
            let Ok(frame) = Self::preview(&shared).await else {
                return;
            };
            sink.publish(frame);
        }
    }
}

#[async_trait]
impl Camera for SimulatorCamera {
    async fn connect(&self) -> Result<(), DeviceError> {
        if matches!(self.shared.locked().link, Link::Up { .. }) {
            return Ok(()); // idempotent, and free (HAL rule 7)
        }
        tokio::time::sleep(self.shared.profile.connect).await;

        let mut state = self.shared.locked();
        state.faults.on_connect();
        let drops_at = state
            .faults
            .take_disconnect_delay()
            .map(|after| Instant::now() + after);
        state.link = Link::Up { drops_at };
        // One thread per connected session, as SDD §5.3.1 gives the real driver: a reconnect
        // after a fault gets a *fresh* one, which is the recovery path the spike proved is the
        // only one that works.
        state.thread = Some(Arc::new(CameraThread::start("astroctl-simcam")));
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        let (live_view, _thread) = {
            let mut state = self.shared.locked();
            state.link = Link::Down;
            // Dropping the last `Arc` closes the job queue, and the thread exits after whatever
            // it is doing. An in-flight capture is therefore *finished*, not abandoned — SDD §7's
            // shutdown order, for the reason it gives: a half-downloaded frame is a lost frame.
            (state.live_view.take(), state.thread.take())
        };
        if let Some(live_view) = live_view {
            live_view.task.abort();
        }
        tokio::time::sleep(self.shared.profile.setting).await;
        Ok(())
    }

    async fn settings(&self) -> Result<CameraSettings, DeviceError> {
        self.config_exchange(self.shared.profile.setting).await?;
        Ok(self.shared.locked().settings.clone())
    }

    async fn available_settings(&self) -> Result<AvailableSettings, DeviceError> {
        // The slow read: 91 config entries in 222 ms, measured. This is why the operation has a
        // `config_seconds` timeout above it at all.
        self.config_exchange(self.shared.profile.settings_tree)
            .await?;
        Ok(AvailableSettings {
            isos: ISO_TOKENS.iter().map(|token| (*token).to_owned()).collect(),
            shutters: SHUTTER_TOKENS
                .iter()
                .map(|token| (*token).to_owned())
                .collect(),
            apertures: APERTURE_TOKENS
                .iter()
                .map(|token| (*token).to_owned())
                .collect(),
            formats: vec![
                ImageFormat::Raw,
                ImageFormat::Jpeg,
                ImageFormat::RawPlusJpeg,
            ],
        })
    }

    async fn set_iso(&self, iso: &str) -> Result<(), DeviceError> {
        self.set_token("ISO", iso, ISO_TOKENS, |settings| {
            settings.iso = iso.to_owned();
        })
        .await
    }

    async fn set_shutter(&self, shutter: &str) -> Result<(), DeviceError> {
        self.set_token("shutter speed", shutter, SHUTTER_TOKENS, |settings| {
            settings.shutter = shutter.to_owned();
        })
        .await
    }

    async fn set_aperture(&self, aperture: &str) -> Result<(), DeviceError> {
        self.set_token("aperture", aperture, APERTURE_TOKENS, |settings| {
            settings.aperture = Some(aperture.to_owned());
        })
        .await
    }

    async fn set_image_format(&self, format: ImageFormat) -> Result<(), DeviceError> {
        self.config_exchange(self.shared.profile.setting).await?;
        self.shared.locked().settings.format = format;
        Ok(())
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, DeviceError> {
        let shutter = self.shared.locked().settings.shutter.clone();
        let Some(seconds) = shutter_seconds(&shutter) else {
            return Err(DeviceError::Rejected(
                "the shutter is set to `bulb`; use capture_bulb, which carries a duration"
                    .to_owned(),
            ));
        };
        self.run_capture(request, Duration::from_secs_f64(seconds), shutter)
            .await
    }

    async fn capture_bulb(
        &self,
        request: &CaptureRequest,
        duration: Duration,
    ) -> Result<CaptureResult, DeviceError> {
        self.run_capture(request, duration, "bulb".to_owned()).await
    }

    async fn abort_capture(&self) -> Result<(), DeviceError> {
        // Never staleness-rejected, never refused for a link that is down: a stopping command is
        // attempted whatever the device looks like (SDD §5.8.1). Bumping the counter with no
        // capture running is the idempotent case and costs nothing.
        self.shared
            .aborts
            .send_modify(|generation| *generation += 1);
        Ok(())
    }

    async fn live_view_frame(&self) -> Result<LiveViewFrame, DeviceError> {
        self.shared.check_link()?;
        let _permit = self.shared.take_imaging_path().await?;
        Self::preview(&self.shared).await
    }

    async fn live_view_stream(&self) -> Result<FrameStream<LiveViewFrame>, DeviceError> {
        self.shared.check_link()?;
        let mut state = self.shared.locked();
        if let Some(live_view) = state.live_view.as_ref() {
            // Two cursors on one stream, not two streams.
            return Ok(live_view.stream.subscribe());
        }
        let (sink, stream) = FrameStream::channel();
        let subscription = stream.subscribe();
        let task = tokio::spawn(Self::live_view_loop(Arc::clone(&self.shared), sink));
        state.live_view = Some(LiveView { stream, task });
        Ok(subscription)
    }

    async fn stop_live_view(&self) -> Result<(), DeviceError> {
        if let Some(live_view) = self.shared.locked().live_view.take() {
            live_view.task.abort();
        }
        Ok(())
    }

    async fn battery(&self) -> Result<BatteryStatus, DeviceError> {
        self.shared.check_link()?;
        // Constant, and deliberately so. The spike measured `batterylevel=100%` for the whole
        // session on a tethered body, and a discharge curve would be invented — see the
        // `CameraProfile` docs' standing rule about numbers no telescope produced. When the
        // low-battery path exists (Phase 2), the knob belongs with it.
        Ok(BatteryStatus {
            percent: 100,
            charging: true,
        })
    }

    async fn storage(&self) -> Result<StorageInfo, DeviceError> {
        self.shared.check_link()?;
        // Measured: the R10's card reported 127.8 GB with 69.5 GB free. Constant because the
        // frames go to the *host* (`capturetarget=Internal RAM`), so the card does not fill.
        Ok(StorageInfo {
            free_mb: 69_500,
            total_mb: 127_800,
        })
    }

    async fn sensor_temperature_celsius(&self) -> Result<Option<f64>, DeviceError> {
        self.shared.check_link()?;
        Ok(self.shared.profile.sensor_temperature_celsius)
    }

    fn capabilities(&self) -> CameraCapabilities {
        CameraCapabilities {
            has_bulb: true,
            has_live_view: true,
            // Measured, in the sense that the body has no mirror: the R10 is mirrorless
            // (PRD §4.3). A capability reported true must work, and this one cannot.
            has_mirror_lockup: false,
            sensor_width_px: self.shared.profile.width,
            sensor_height_px: self.shared.profile.height,
            pixel_size_um: self.shared.profile.pixel_size_um,
            supported_formats: vec![
                ImageFormat::Raw,
                ImageFormat::Jpeg,
                ImageFormat::RawPlusJpeg,
            ],
            min_iso: 100,
            max_iso: 51_200,
            min_shutter_s: 1.0 / 4000.0,
            max_shutter_s: 30.0,
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "AstroCtl Simulator Camera".to_owned(),
            model: "EOS R10-class simulator".to_owned(),
            firmware: None,
            // A serial that is stable and obviously synthetic. It matters beyond display:
            // calibration masters are matched to the body that shot them (HAL-06), so a camera
            // with no serial would produce frames no master could ever be matched to.
            serial: Some("SIM-CAM-000001".to_owned()),
            protocol: "simulator".to_owned(),
        }
    }
}

/// Turns a `camera.default_format` string into a format, naming the alternatives if it cannot.
fn parse_format(token: &str) -> Result<ImageFormat, DriverInitError> {
    match token.to_ascii_uppercase().as_str() {
        "RAW" => Ok(ImageFormat::Raw),
        "JPEG" | "JPG" => Ok(ImageFormat::Jpeg),
        "RAW+JPEG" => Ok(ImageFormat::RawPlusJpeg),
        _ => Err(DriverInitError::new(format!(
            "`camera.default_format: {token}` is not a format; use `RAW`, `JPEG` or `RAW+JPEG`"
        ))),
    }
}

// -----------------------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------------------

/// Builds [`SimulatorCamera`]s for the driver registry under the name `"simulator"` (HAL-07).
///
/// The profile, the fault plan and the pointing source live on the *factory* rather than in
/// [`CameraConfig`], for the reason [`SimulatorMountFactory`](super::mount::SimulatorMountFactory)
/// gives: the config is the operator's YAML, an operator has no reason to script a capture
/// failure, and `deny_unknown_fields` would reject a test's attempt to put one there anyway.
///
/// The pointing source is the interesting one. A binary that registers this factory *after*
/// building its mount hands the mount in, and every simulated frame then shows the sky the
/// simulated telescope is aimed at (PRD §4.5). A factory built without one produces frames of
/// the profile's default pointing, which is what a camera-only test wants.
#[derive(Debug, Clone)]
pub struct SimulatorCameraFactory {
    profile: CameraProfile,
    faults: CameraFaultPlan,
    pointing: Arc<dyn PointingSource>,
}

impl Default for SimulatorCameraFactory {
    fn default() -> Self {
        let profile = CameraProfile::default();
        Self {
            profile,
            faults: CameraFaultPlan::none(),
            pointing: Arc::new(profile.default_pointing),
        }
    }
}

impl SimulatorCameraFactory {
    /// A factory building cameras with the measured R10 timing, no faults, and a fixed pointing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A factory building cameras on `profile`.
    #[must_use]
    pub fn with_profile(mut self, profile: CameraProfile) -> Self {
        self.profile = profile;
        self
    }

    /// A factory building cameras that fail as `faults` says (SDD §9).
    #[must_use]
    pub fn with_faults(mut self, faults: CameraFaultPlan) -> Self {
        self.faults = faults;
        self
    }

    /// A factory whose cameras follow `pointing` — normally the simulated mount.
    #[must_use]
    pub fn following(mut self, pointing: Arc<dyn PointingSource>) -> Self {
        self.pointing = pointing;
        self
    }
}

#[async_trait]
impl CameraFactory for SimulatorCameraFactory {
    fn name(&self) -> &'static str {
        // Must equal `CameraDriver::Simulator.as_str()`, or the driver is unreachable from
        // configuration. Asserted by a test below rather than trusted.
        "simulator"
    }

    fn create(&self, config: &CameraConfig) -> Result<Arc<dyn Camera>, DriverInitError> {
        Ok(Arc::new(SimulatorCamera::new(
            config,
            self.profile,
            &self.faults,
            Arc::clone(&self.pointing),
        )?))
    }

    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        // HAL-08. `address` is normally what the operator pastes into the config field that
        // addresses the device — but PRD §8.1's `camera` section has no such field, so the
        // honest value is the driver name itself: there is nothing to paste.
        Ok(vec![DetectedDevice::new(
            "simulator",
            DeviceKind::Camera,
            "simulator",
            "AstroCtl Simulator Camera (no hardware required)",
        )])
    }
}

/// The configuration a bare [`SimulatorCamera::with_profile`] uses.
///
/// Mirrors `config/field-node.example.yaml`'s camera section, and is deliberately not public: a
/// caller that wants particular timeouts should say so in a [`CameraConfig`], which is what the
/// real driver reads too.
fn default_config() -> CameraConfig {
    CameraConfig {
        driver: CameraDriver::Simulator,
        default_iso: "1600".to_owned(),
        default_shutter: "30".to_owned(),
        default_format: "RAW+JPEG".to_owned(),
        ops_via_cli: Vec::new(),
        timeouts: CameraTimeouts {
            config_seconds: 5,
            capture_extra_seconds: 30,
            download_seconds: 120,
        },
        indi_device: None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use astroctl_hal::registry::DriverRegistry;

    use super::super::fault::CameraFault;
    use super::super::sky::{detect, StarField};
    use super::*;

    /// A path inside the process's temporary directory, unique to this test.
    ///
    /// Tests in one binary run in parallel, so a shared directory would make two captures race
    /// over one filename and fail in whichever order the scheduler picked.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astroctl-simcam-{}-{name}", std::process::id()));
        // A leftover from a previous run would make the "nothing on disk" assertions pass or
        // fail for a reason that has nothing to do with this run.
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).expect("a writable temporary directory");
        dir
    }

    /// A small camera looking at a bright, crowded field.
    ///
    /// 160×120 at 3″/px is a fifth of a degree across, which at this density holds a few dozen
    /// stars — enough to count, small enough that a test renders in a millisecond. The faint
    /// limit is pulled in to magnitude 13 because the *default* limit of 18 is honestly
    /// invisible in a short exposure, and a test that asserted otherwise would be asserting
    /// against a simulator that lies about its own noise.
    fn profile() -> CameraProfile {
        CameraProfile {
            width: 160,
            height: 120,
            arcsec_per_pixel: 3.0,
            fwhm_arcsec: 9.0,
            live_view_width: 80,
            live_view_height: 60,
            field: StarField::new(4242)
                .with_density(3_000.0)
                .with_faintest_magnitude(13.0),
            ..CameraProfile::default()
        }
    }

    /// The configuration with a two-second shutter — the acceptance criterion's exposure.
    fn config() -> CameraConfig {
        CameraConfig {
            default_shutter: "2".to_owned(),
            ..default_config()
        }
    }

    fn camera_with(profile: CameraProfile, faults: CameraFaultPlan) -> Arc<SimulatorCamera> {
        let pointing = Arc::new(profile.default_pointing);
        Arc::new(
            SimulatorCamera::new(&config(), profile, &faults, pointing)
                .expect("the default configuration is valid"),
        )
    }

    async fn connected() -> Arc<SimulatorCamera> {
        let camera = camera_with(profile(), CameraFaultPlan::none());
        camera.connect().await.expect("connects");
        camera
    }

    fn request(dir: &Path, stem: &str) -> CaptureRequest {
        CaptureRequest::new(dir.to_path_buf(), stem)
    }

    // --- acceptance: timing ----------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_two_second_exposure_takes_two_seconds_plus_the_download() {
        // Acceptance criterion 2, on a paused clock so the assertion is on the *exact* duration
        // rather than on a wall-clock measurement with a tolerance wide enough to hide a bug.
        let dir = scratch_dir("timing");
        let camera = connected().await;

        let start = Instant::now();
        let result = camera
            .capture(&request(&dir, "light_00001"))
            .await
            .expect("captures");
        let elapsed = start.elapsed();

        assert_eq!(result.exposure, Duration::from_secs(2));
        assert_eq!(
            elapsed,
            Duration::from_secs(2) + CameraProfile::default().download,
            "a 2 s exposure plus the measured 2 s download"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn bulb_honours_the_duration_it_was_given() {
        let dir = scratch_dir("bulb");
        let camera = connected().await;

        let start = Instant::now();
        let result = camera
            .capture_bulb(&request(&dir, "light_00002"), Duration::from_secs(300))
            .await
            .expect("captures");
        assert_eq!(start.elapsed(), Duration::from_secs(302));
        assert_eq!(result.exposure, Duration::from_secs(300));
        // The settings the frame reports are what the camera *did*, so a bulb frame says bulb
        // whatever the shutter dial was on.
        assert_eq!(result.settings.shutter, "bulb");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_with_the_shutter_on_bulb_is_refused_rather_than_hanging() {
        // The spike's finding, reproduced: with the mode dial on Bulb the R10 offers only
        // `bulb`, so this is a state real hardware puts a caller in. `capture` has nowhere to
        // put a duration, so the honest answer is to say so and name the method that does.
        let dir = scratch_dir("bulb-shutter");
        let camera = connected().await;
        camera.set_shutter("bulb").await.expect("bulb is offered");

        let error = camera
            .capture(&request(&dir, "light_00003"))
            .await
            .expect_err("refused");
        assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");
        assert!(error.to_string().contains("capture_bulb"), "{error}");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_mid_exposure_returns_promptly_and_leaves_nothing_on_disk() {
        // Acceptance criterion 2's other half. "Promptly" is asserted as *zero virtual time*
        // after the abort, which is stronger than a tolerance and is what the operator's Stop
        // button needs to be.
        let dir = scratch_dir("abort");
        let camera = connected().await;

        let capturing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move {
                camera
                    .capture_bulb(&request(&dir, "light_00004"), Duration::from_secs(600))
                    .await
            })
        };
        // Let the exposure start, then let a minute of it pass.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;

        let abort_at = Instant::now();
        camera.abort_capture().await.expect("aborts");
        let error = capturing
            .await
            .expect("the task completes")
            .expect_err("the capture was abandoned");

        assert!(matches!(error, DeviceError::Aborted(_)), "{error:?}");
        assert_eq!(
            abort_at.elapsed(),
            Duration::ZERO,
            "an abort must not wait out the exposure it is cancelling"
        );
        // "Any partial file must be removed before this resolves" — here there is nothing to
        // remove, because the bytes only reach the disk after the last abortable step.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // ...and the camera is usable again: the abort released the imaging path.
        camera
            .capture(&request(&dir, "light_00005"))
            .await
            .expect("the next capture works");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_capture_is_busy_rather_than_a_second_exposure() {
        let dir = scratch_dir("busy");
        let camera = connected().await;

        let first = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&request(&dir, "light_1")).await })
        };
        tokio::task::yield_now().await;

        let second = camera.capture(&request(&dir, "light_2")).await;
        assert!(matches!(second, Err(DeviceError::Busy(_))), "{second:?}");
        // A setting cannot be changed through an exposure either — one context, one queue.
        let during = camera.set_iso("800").await;
        assert!(matches!(during, Err(DeviceError::Busy(_))), "{during:?}");

        first.await.expect("task").expect("the first capture wins");
        camera.set_iso("800").await.expect("free again");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_view_frame_in_flight_delays_a_capture_rather_than_refusing_it() {
        // The distinction `State::capturing` exists for. Live view is running, so at any instant
        // the sensor may be held for the millisecond a preview takes to render; a capture issued
        // then must queue, not fail. Refusing would give the capture flow above (M1-T08) a
        // `Busy` it cannot act on and cannot reproduce, roughly one attempt in ten.
        let dir = scratch_dir("liveview-capture-race");
        let camera = connected().await;
        let _stream = camera.live_view_stream().await.expect("starts");

        for frame in 0..5 {
            let stem = format!("light_{frame}");
            camera
                .capture(&request(&dir, &stem))
                .await
                .unwrap_or_else(|error| {
                    panic!("capture {frame} was refused while live view ran: {error:?}")
                });
            // Let the preview loop take the sensor again between captures.
            tokio::time::advance(Duration::from_millis(200)).await;
        }
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- acceptance: the files ---------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_capture_writes_a_fits_and_a_jpeg_that_independent_readers_open() {
        // Acceptance criterion 3, end to end: not "the writer round-trips" (that is asserted in
        // `fits`) but "the file a capture produced opens".
        use fitsrs::{Fits, Pixels, HDU};

        let dir = scratch_dir("files");
        let camera = connected().await;
        let result = camera
            .capture(&request(&dir, "light_00042"))
            .await
            .expect("captures");

        let raw = result.raw().expect("a science file");
        let jpeg = result.jpeg().expect("RAW+JPEG produced a rendition too");
        assert_eq!(raw.path, dir.join("light_00042.fits"));
        assert_eq!(jpeg.path, dir.join("light_00042.jpg"));
        assert_eq!(result.total_bytes(), raw.size_bytes + jpeg.size_bytes);

        let file = fs::File::open(&raw.path).expect("the FITS exists");
        let mut hdus = Fits::from_reader(std::io::BufReader::new(file));
        let Some(Ok(HDU::Primary(hdu))) = hdus.next() else {
            panic!("fitsrs did not find a primary HDU");
        };
        let header = hdu.get_header();
        assert_eq!(header.get_parsed::<i64>("NAXIS1").expect("NAXIS1"), 160);
        assert_eq!(header.get_parsed::<i64>("NAXIS2").expect("NAXIS2"), 120);
        assert_eq!(header.get_parsed::<f64>("EXPTIME").expect("EXPTIME"), 2.0);
        // The header carries what the stacker will need to match this frame to another.
        assert_eq!(
            header.get_parsed::<i64>("SIMSEED").expect("SIMSEED") as u64,
            4242
        );
        let image = hdus.get_data(&hdu);
        let Pixels::I16(samples) = image.pixels() else {
            panic!("expected 16-bit samples");
        };
        assert_eq!(samples.count(), 160 * 120);

        let bytes = fs::read(&jpeg.path).expect("the JPEG exists");
        let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&bytes));
        decoder.decode().expect("zune-jpeg decodes the rendition");
        assert_eq!(
            decoder.info().expect("dimensions").width as u32,
            160,
            "the rendition is the frame, not a thumbnail"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn the_format_setting_decides_which_files_exist() {
        let dir = scratch_dir("formats");
        let camera = connected().await;

        camera
            .set_image_format(ImageFormat::Raw)
            .await
            .expect("supported");
        let raw_only = camera
            .capture(&request(&dir, "raw_only"))
            .await
            .expect("captures");
        assert!(raw_only.raw().is_some() && raw_only.jpeg().is_none());

        camera
            .set_image_format(ImageFormat::Jpeg)
            .await
            .expect("supported");
        let jpeg_only = camera
            .capture(&request(&dir, "jpeg_only"))
            .await
            .expect("captures");
        assert!(jpeg_only.raw().is_none() && jpeg_only.jpeg().is_some());
        assert!(!dir.join("jpeg_only.fits").exists());
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- acceptance: what is in the frames ---------------------------------------------------

    /// Captures one frame and reads its samples back out of the FITS file it wrote.
    ///
    /// Through the file rather than out of the renderer, so every statistical assertion below is
    /// made against what a consumer would actually load.
    async fn capture_samples(camera: &SimulatorCamera, request: &CaptureRequest) -> Vec<u16> {
        let result = camera.capture(request).await.expect("captures");
        read_samples(&result.raw().expect("a science file").path)
    }

    /// Reads a frame back out of its FITS file as samples.
    fn read_samples(path: &PathBuf) -> Vec<u16> {
        use fitsrs::{Fits, Pixels, HDU};
        let file = fs::File::open(path).expect("the FITS exists");
        let mut hdus = Fits::from_reader(std::io::BufReader::new(file));
        let Some(Ok(HDU::Primary(hdu))) = hdus.next() else {
            panic!("no primary HDU");
        };
        let Pixels::I16(samples) = hdus.get_data(&hdu).pixels() else {
            panic!("expected 16-bit samples");
        };
        samples.map(|s| (i32::from(s) + 32_768) as u16).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn frames_contain_stars_and_the_background_follows_the_sky_level() {
        // Acceptance criterion 1, asserted statistically against a fixed seed — so the numbers
        // below are properties of this sky, not probabilities.
        let dir = scratch_dir("statistics");
        let dark_sky = camera_with(profile(), CameraFaultPlan::none());
        dark_sky.connect().await.expect("connects");
        let bright_sky = camera_with(
            CameraProfile {
                sky_electrons_per_second: 60.0,
                ..profile()
            },
            CameraFaultPlan::none(),
        );
        bright_sky.connect().await.expect("connects");

        let dark = read_samples(
            &dark_sky
                .capture(&request(&dir, "dark_sky"))
                .await
                .expect("captures")
                .raw()
                .expect("a science file")
                .path,
        );
        let bright = read_samples(
            &bright_sky
                .capture(&request(&dir, "bright_sky"))
                .await
                .expect("captures")
                .raw()
                .expect("a science file")
                .path,
        );

        // The frame visibly contains stars: peaks well above a background that is itself well
        // above the bias pedestal.
        let found = detect::stars(&dark, 160, 120, 500.0);
        assert!(found.len() > 10, "only {} stars", found.len());

        // Three times the sky is three times the signal over the bias — the pedestal does not
        // scale, which is exactly what makes it a pedestal.
        let bias = 512.0;
        let dark_sky_adu = detect::mean(&dark) - bias;
        let bright_sky_adu = detect::mean(&bright) - bias;
        let ratio = bright_sky_adu / dark_sky_adu;
        assert!(
            (2.5..3.5).contains(&ratio),
            "three times the sky gave {dark_sky_adu} then {bright_sky_adu} ADU"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn star_count_follows_the_configured_density() {
        let dir = scratch_dir("density");
        let sparse = camera_with(
            CameraProfile {
                field: StarField::new(4242)
                    .with_density(1_000.0)
                    .with_faintest_magnitude(13.0),
                ..profile()
            },
            CameraFaultPlan::none(),
        );
        let dense = camera_with(
            CameraProfile {
                field: StarField::new(4242)
                    .with_density(4_000.0)
                    .with_faintest_magnitude(13.0),
                ..profile()
            },
            CameraFaultPlan::none(),
        );
        sparse.connect().await.expect("connects");
        dense.connect().await.expect("connects");

        let count = |samples: Vec<u16>| detect::stars(&samples, 160, 120, 500.0).len();
        let few = count(read_samples(
            &sparse
                .capture(&request(&dir, "sparse"))
                .await
                .expect("captures")
                .raw()
                .expect("raw")
                .path,
        ));
        let many = count(read_samples(
            &dense
                .capture(&request(&dir, "dense"))
                .await
                .expect("captures")
                .raw()
                .expect("raw")
                .path,
        ));
        let ratio = many as f64 / few as f64;
        assert!(
            (2.5..5.0).contains(&ratio),
            "four times the density gave {few} then {many} stars"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn iso_changes_the_gain_the_frame_is_digitised_at() {
        // "current settings respected by the generator (ISO → gain/noise)" — asserted where an
        // operator would see it: the same sky, four stops apart, is four stops brighter.
        let dir = scratch_dir("iso");
        let camera = connected().await;

        camera.set_iso("100").await.expect("offered");
        let low = read_samples(
            &camera
                .capture(&request(&dir, "iso100"))
                .await
                .expect("captures")
                .raw()
                .expect("raw")
                .path,
        );
        camera.set_iso("1600").await.expect("offered");
        let high = read_samples(
            &camera
                .capture(&request(&dir, "iso1600"))
                .await
                .expect("captures")
                .raw()
                .expect("raw")
                .path,
        );

        let bias = 512.0;
        let ratio = (detect::mean(&high) - bias) / (detect::mean(&low) - bias);
        assert!(
            (12.0..20.0).contains(&ratio),
            "ISO 100 to 1600 changed the level by {ratio}x, not the expected 16"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn the_frame_follows_the_mount_rather_than_a_fixed_sky() {
        // PRD §4.5's actual requirement: the star field comes from *the simulated mount
        // position*. Two different pointings, one camera profile, and the frames must differ —
        // and the same pointing twice must give the same stars in the same places.
        let dir = scratch_dir("pointing");
        let here = RaDec::from_parts(5.5, 22.0).expect("valid");
        let there = RaDec::from_parts(18.0, -10.0).expect("valid");

        let aimed_at = |pointing: RaDec| {
            SimulatorCamera::new(
                &config(),
                profile(),
                &CameraFaultPlan::none(),
                Arc::new(pointing),
            )
            .expect("valid")
        };
        let camera = aimed_at(here);
        camera.connect().await.expect("connects");
        let first = capture_samples(&camera, &request(&dir, "here")).await;
        let again = capture_samples(&camera, &request(&dir, "here_again")).await;

        let moved = aimed_at(there);
        moved.connect().await.expect("connects");
        let elsewhere = capture_samples(&moved, &request(&dir, "there")).await;

        // Compared as *positions within a pixel* rather than as an identical list: the sky is
        // the same in both frames, but the noise on top of it is not, so a marginal star can
        // shift a detector's centroid by a fraction of a pixel or split into two peaks. Demanding
        // byte-equal detections would be demanding that the simulator have no noise.
        let matched = |a: &[u16], b: &[u16]| -> f64 {
            let left = detect::stars(a, 160, 120, 500.0);
            let right = detect::stars(b, 160, 120, 500.0);
            let hits = left
                .iter()
                .filter(|star| {
                    right
                        .iter()
                        .any(|other| (star.column - other.column).hypot(star.row - other.row) < 1.5)
                })
                .count();
            hits as f64 / left.len() as f64
        };
        assert!(
            matched(&first, &again) > 0.95,
            "the same pointing must give the same sky; {:.0}% of the stars matched",
            matched(&first, &again) * 100.0
        );
        assert!(
            matched(&first, &elsewhere) < 0.1,
            "a different pointing must give a different sky; {:.0}% of the stars matched",
            matched(&first, &elsewhere) * 100.0
        );
        // ...and consecutive frames of that same field are not sample-identical, because two
        // exposures are not one exposure taken twice: the sky is a function of the pointing, the
        // noise is a function of the frame number.
        assert_ne!(first, again, "two exposures must not be sample-identical");

        // The other half of that, and a property worth pinning down: a *fresh* camera on the
        // same configuration replays the same frames. Two field nodes built from one config see
        // one sky, which is what makes a reported frame reproducible somewhere else (and is why
        // the seed is written into the FITS header).
        let replay = aimed_at(here);
        replay.connect().await.expect("connects");
        assert_eq!(
            capture_samples(&replay, &request(&dir, "replay")).await,
            first,
            "the same configuration must replay the same frame"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_camera_following_the_simulated_mount_shoots_where_the_telescope_points() {
        // PRD §4.5's requirement end to end, through the two simulators wired the way a field
        // node wires them: the camera holds the mount's pointing source, the operator slews, and
        // the next frame is of somewhere else — with the frame's own header saying where.
        use super::super::mount::{SimulatorMount, SimulatorMountFactory};
        use super::super::profile::SimulatorProfile;
        use astroctl_hal::mount::MountDevice;
        use astroctl_hal::registry::MountFactory;

        let dir = scratch_dir("follows-mount");
        let mount =
            Arc::new(SimulatorMount::with_profile(SimulatorProfile::instant()).expect("valid"));
        mount.connect().await.expect("connects");
        let camera = SimulatorCamera::new(
            &config(),
            profile(),
            &CameraFaultPlan::none(),
            mount.pointing_source(),
        )
        .expect("valid");
        camera.connect().await.expect("connects");

        // Reads the RA and Dec the frame's FITS header recorded, in degrees.
        let header_pointing = |path: &PathBuf| -> (f64, f64) {
            use fitsrs::{Fits, HDU};
            let file = fs::File::open(path).expect("the FITS exists");
            let mut hdus = Fits::from_reader(std::io::BufReader::new(file));
            let Some(Ok(HDU::Primary(hdu))) = hdus.next() else {
                panic!("no primary HDU");
            };
            let header = hdu.get_header();
            (
                header.get_parsed::<f64>("RA").expect("RA"),
                header.get_parsed::<f64>("DEC").expect("DEC"),
            )
        };

        // The mount starts parked at 0h/+90° in the bare default configuration; this test's
        // config parks it at 12h/+45°.
        let parked = mount.position().await.expect("position");
        let first = camera
            .capture(&request(&dir, "at_park"))
            .await
            .expect("captures");
        let (ra, dec) = header_pointing(&first.raw().expect("raw").path);
        assert!(
            (ra - parked.ra.hours() * 15.0).abs() < 0.1 && (dec - parked.dec.degrees()).abs() < 0.1,
            "the frame says {ra},{dec}; the mount says {},{}",
            parked.ra.hours() * 15.0,
            parked.dec.degrees()
        );

        let target = RaDec::from_parts(18.0, -10.0).expect("valid");
        mount.goto(target).await.expect("slews");
        let second = camera
            .capture(&request(&dir, "at_target"))
            .await
            .expect("captures");
        let (ra, dec) = header_pointing(&second.raw().expect("raw").path);
        assert!(
            (ra - 270.0).abs() < 0.1 && (dec + 10.0).abs() < 0.1,
            "after a goto to 18h/-10 the frame says {ra},{dec}"
        );

        // ...and the two frames are of different skies, not the same sky relabelled.
        let a = read_samples(&first.raw().expect("raw").path);
        let b = read_samples(&second.raw().expect("raw").path);
        assert_ne!(a, b);

        // Finally, the wiring a binary does: a factory carrying the mount's pointing source.
        let registry_camera = SimulatorCameraFactory::new()
            .with_profile(profile())
            .following(mount.pointing_source())
            .create(&config())
            .expect("builds");
        assert_eq!(registry_camera.device_info().protocol, "simulator");
        // The mount factory is what a binary would have built that mount with.
        assert_eq!(SimulatorMountFactory::new().name(), "simulator");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- acceptance: live view ---------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn live_view_delivers_frames_at_the_configured_rate_and_stops_for_a_capture() {
        let dir = scratch_dir("liveview");
        let camera = connected().await;

        let mut stream = camera.live_view_stream().await.expect("starts");
        let mut second = camera
            .live_view_stream()
            .await
            .expect("joins the same stream");

        // 5 fps by default: one frame per 200 ms of virtual time.
        let frame = stream.next_frame().await.expect("a frame arrives");
        assert!(!frame.jpeg.is_empty());
        let also = second.next_frame().await.expect("the same stream");
        assert_eq!(
            also.jpeg,
            stream.latest().expect("a frame").jpeg,
            "two subscribers must see one stream, not two acquisitions"
        );

        // ...and it is a real JPEG at the reduced live-view size.
        let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(frame.jpeg.as_ref()));
        decoder.decode().expect("decodes");
        let info = decoder.info().expect("dimensions");
        assert_eq!((info.width, info.height), (80, 60));

        // During a capture the stream goes quiet — SDD §5.7 calls this expected, and a consumer
        // that treated it as a fault would reconnect on every frame the operator takes.
        let capturing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&request(&dir, "light_lv")).await })
        };
        tokio::task::yield_now().await;
        let mut fresh = camera.live_view_stream().await.expect("still running");
        let during = tokio::time::timeout(Duration::from_millis(1500), fresh.next_frame()).await;
        assert!(
            during.is_err(),
            "live view produced a frame while the sensor was exposing"
        );
        capturing.await.expect("task").expect("captures");

        // ...and resumes afterwards without anyone restarting it.
        assert!(
            fresh.next_frame().await.is_some(),
            "live view did not resume"
        );

        camera.stop_live_view().await.expect("stops");
        // A cursor that has been away has one frame waiting for it — the newest, per the
        // stream's latest-only rule — so "the stream ended" is what happens *after* the frames
        // already published, not instead of them. Draining under a timeout asserts it ends at
        // all, which is the property; without the timeout a stream that never ended would hang
        // the suite rather than fail it.
        tokio::time::timeout(Duration::from_secs(5), async {
            while stream.next_frame().await.is_some() {}
        })
        .await
        .expect("stopping live view ends the stream");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_one_shot_live_view_frame_needs_no_stream() {
        let camera = connected().await;
        let frame = camera.live_view_frame().await.expect("a frame");
        assert!(!frame.jpeg.is_empty());
        assert!(
            camera.shared.locked().live_view.is_none(),
            "no stream started"
        );
    }

    // --- acceptance: faults ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_scripted_capture_failure_hits_its_frame_and_the_next_one_works() {
        let dir = scratch_dir("failcapture");
        let camera = camera_with(
            profile(),
            CameraFaultPlan::new([CameraFault::FailCapture(2)]),
        );
        camera.connect().await.expect("connects");

        camera
            .capture(&request(&dir, "light_1"))
            .await
            .expect("the first frame is fine");
        let error = camera
            .capture(&request(&dir, "light_2"))
            .await
            .expect_err("the second is scripted to fail");
        assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");
        // A failed capture must not leave the camera busy, or the retry is answered `Busy`
        // forever.
        camera
            .capture(&request(&dir, "light_3"))
            .await
            .expect("the retry works");
        assert!(!dir.join("light_2.fits").exists());
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_download_costs_exactly_what_the_plan_says() {
        // T-ISO-1's knob (SDD §9). The point of the assertion is that the multiplier reaches the
        // clock: a fault that did nothing would leave the isolation test measuring an idle node.
        let dir = scratch_dir("slowdownload");
        let camera = camera_with(
            profile(),
            CameraFaultPlan::new([CameraFault::SlowDownload(10.0)]),
        );
        camera.connect().await.expect("connects");

        let start = Instant::now();
        camera
            .capture(&request(&dir, "slow"))
            .await
            .expect("captures");
        assert_eq!(start.elapsed(), Duration::from_secs(2 + 20));

        // Consumed once: the next frame is the camera's ordinary self.
        let start = Instant::now();
        camera
            .capture(&request(&dir, "normal"))
            .await
            .expect("captures");
        assert_eq!(start.elapsed(), Duration::from_secs(2 + 2));
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_download_past_its_timeout_is_a_wedged_camera() {
        // SDD §5.3.1: a breached operation-class timeout is not a slow camera, it is a wedged
        // one, and the facade above rebuilds the driver rather than waiting longer.
        let dir = scratch_dir("downloadtimeout");
        let camera = camera_with(
            profile(),
            CameraFaultPlan::new([CameraFault::SlowDownload(100.0)]),
        );
        camera.connect().await.expect("connects");

        let start = Instant::now();
        let error = camera
            .capture(&request(&dir, "wedged"))
            .await
            .expect_err("200 s exceeds the 120 s download budget");
        assert!(
            matches!(error, DeviceError::Timeout(budget) if budget == Duration::from_secs(120)),
            "{error:?}"
        );
        // The caller waited the budget, not the camera's secret 200 seconds.
        assert_eq!(start.elapsed(), Duration::from_secs(2 + 120));
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(start_paused = true)]
    async fn a_camera_that_falls_off_the_bus_says_so_and_can_be_reconnected() {
        let dir = scratch_dir("disconnect");
        let camera = camera_with(
            profile(),
            CameraFaultPlan::new([CameraFault::DisconnectAfter(Duration::from_secs(30))]),
        );
        camera.connect().await.expect("connects");
        camera
            .capture(&request(&dir, "before"))
            .await
            .expect("the camera is there");

        tokio::time::advance(Duration::from_secs(60)).await;
        let error = camera
            .capture(&request(&dir, "after"))
            .await
            .expect_err("the camera is gone");
        assert!(matches!(error, DeviceError::Transport(_)), "{error:?}");
        // The operator's next action is to check the cable, so the message says which failure
        // this is — the spike's two are distinguishable and so are these.
        assert!(error.to_string().contains("USB port"), "{error}");

        // Every call after that is NotConnected until a reconnect, which succeeds — the spike's
        // finding that only a fresh context recovers.
        let again = camera.settings().await.expect_err("still gone");
        assert!(matches!(again, DeviceError::Transport(_)), "{again:?}");
        camera.connect().await.expect("reconnects");
        camera
            .capture(&request(&dir, "recovered"))
            .await
            .expect("the fresh context works");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- settings --------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_value_the_body_does_not_offer_is_refused_rather_than_rounded() {
        let camera = connected().await;
        let available = camera.available_settings().await.expect("reads the tree");
        assert_eq!(available.isos.len(), 27, "the spike counted 27 ISO choices");
        assert_eq!(
            available.apertures.len(),
            22,
            "the spike counted 22 aperture choices"
        );
        assert!(available.shutters.contains(&"bulb".to_owned()));

        let error = camera.set_iso("1601").await.expect_err("not offered");
        assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");
        // The setting did not change behind the refusal.
        assert_eq!(camera.settings().await.expect("reads").iso, "1600");

        camera.set_iso("6400").await.expect("offered");
        assert_eq!(camera.settings().await.expect("reads").iso, "6400");
    }

    #[tokio::test(start_paused = true)]
    async fn reading_the_settings_tree_costs_what_the_spike_measured() {
        // 91 config entries in 222 ms. The number is why `camera.timeouts.config_seconds` exists
        // at all, and a simulator that answered instantly would let a caller poll it in a loop.
        let camera = connected().await;
        let start = Instant::now();
        camera.available_settings().await.expect("reads");
        assert_eq!(start.elapsed(), Duration::from_millis(222));
    }

    #[tokio::test(start_paused = true)]
    async fn every_call_needs_a_connection() {
        let camera = camera_with(profile(), CameraFaultPlan::none());
        assert!(matches!(
            camera.settings().await,
            Err(DeviceError::NotConnected)
        ));
        assert!(matches!(
            camera.live_view_frame().await,
            Err(DeviceError::NotConnected)
        ));
        assert!(matches!(
            camera.battery().await,
            Err(DeviceError::NotConnected)
        ));
        // ...except the stopping one, which is attempted whatever the device looks like.
        camera
            .abort_capture()
            .await
            .expect("abort is never refused");
    }

    #[tokio::test(start_paused = true)]
    async fn disconnecting_ends_live_view_and_stops_the_camera_answering() {
        let camera = connected().await;
        let mut stream = camera.live_view_stream().await.expect("starts");
        stream.next_frame().await.expect("a frame");

        camera.disconnect().await.expect("disconnects");
        assert!(
            stream.next_frame().await.is_none(),
            "disconnect must end the stream"
        );
        assert!(matches!(
            camera.settings().await,
            Err(DeviceError::NotConnected)
        ));
        // Idempotent, both ways.
        camera.disconnect().await.expect("again");
        camera.connect().await.expect("and back");
        camera.connect().await.expect("idempotent");
    }

    // --- capabilities and identity ------------------------------------------------------------

    #[test]
    fn the_capabilities_are_the_reference_body() {
        let camera = camera_with(CameraProfile::default(), CameraFaultPlan::none());
        let capabilities = camera.capabilities();
        assert_eq!(capabilities.sensor_width_px, 6000);
        assert_eq!(capabilities.sensor_height_px, 4000);
        assert!((capabilities.pixel_size_um - 3.72).abs() < 1e-9);
        assert!(capabilities.has_bulb && capabilities.has_live_view);
        // The R10 is mirrorless: a capability reported true must work, and this one cannot.
        assert!(!capabilities.has_mirror_lockup);
        assert_eq!(camera.device_info().protocol, "simulator");
        assert!(camera.device_info().serial.is_some());
    }

    // --- acceptance: the registry -------------------------------------------------------------

    #[test]
    fn the_factory_is_registrable_under_the_name_the_config_uses() {
        // Acceptance criterion 5. If `name()` and `CameraDriver::as_str()` ever disagree the
        // driver is unreachable from YAML, and the operator's error is "no camera driver named
        // simulator" on a build that has one.
        let mut registry = DriverRegistry::new();
        registry
            .register_camera(SimulatorCameraFactory::new())
            .expect("registers");
        assert_eq!(registry.camera_drivers(), ["simulator"]);
        assert_eq!(
            SimulatorCameraFactory::new().name(),
            CameraDriver::Simulator.as_str()
        );

        let config = config();
        let camera = registry
            .create_camera(config.driver.as_str(), &config)
            .expect("builds");
        assert_eq!(camera.device_info().protocol, "simulator");
    }

    #[tokio::test]
    async fn the_factory_reports_itself() {
        let found = SimulatorCameraFactory::new().probe().await.expect("probes");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, DeviceKind::Camera);
    }

    #[test]
    fn a_configuration_the_body_cannot_honour_is_refused_at_startup() {
        // The registry's contract: `create` validates and does no I/O, and the message names the
        // YAML value — the operator's next action is an edit, not a debugger.
        let factory = SimulatorCameraFactory::new();
        let error = factory
            .create(&CameraConfig {
                default_iso: "999999".to_owned(),
                ..config()
            })
            .expect_err("no such ISO");
        assert!(error.to_string().contains("camera.default_iso"), "{error}");

        let error = factory
            .create(&CameraConfig {
                default_format: "TIFF".to_owned(),
                ..config()
            })
            .expect_err("no such format");
        assert!(error.to_string().contains("RAW+JPEG"), "{error}");
    }

    #[test]
    fn a_factory_can_carry_a_fault_plan_into_the_registry() {
        let factory = SimulatorCameraFactory::new()
            .with_profile(profile())
            .with_faults(CameraFaultPlan::new([CameraFault::FailCapture(1)]));
        let camera = factory.create(&config()).expect("builds");
        let dir = scratch_dir("factory-faults");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime");
        let error = runtime.block_on(async {
            camera.connect().await.expect("connects");
            camera.capture(&request(&dir, "light_1")).await
        });
        assert!(
            matches!(error, Err(DeviceError::Rejected(_))),
            "the scripted failure must apply to a registry-built camera too: {error:?}"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    // --- the full-size frame -----------------------------------------------------------------

    #[test]
    #[ignore = "3.6 s debug / 0.8 s release for one 6000x4000 frame; --ignored --nocapture prints it"]
    fn a_full_size_frame_is_generated_and_written() {
        // The reference body's own frame, once, so the cost is a measurement rather than a
        // guess — it is what a session pays per exposure, and M1-T08's capture flow budgets
        // against it.
        //
        // Measured on an x86_64 workstation, 2026-07-30: **0.77 s in release, 3.63 s in debug**,
        // producing a 48 MB FITS and a 17 MB JPEG. Three things follow, and they are the reason
        // this test exists at all rather than the reason it is ignored:
        //
        //  * The FITS is *larger* than the R10's 32 MB CR3, so PRF-07's 5 s transfer budget and
        //    the bufferbloat rule should plan against 48 MB for simulated sessions.
        //  * The JPEG is 17 MB because a sky-limited frame is mostly shot noise, and noise does
        //    not compress. A real body's JPEG of the same scene would be smaller; nothing should
        //    infer a rendition size from this one.
        //  * A Raspberry Pi is perhaps 4–6× slower than this workstation per core, which puts a
        //    full-size synthetic frame in the seconds — comparable to the 2 s download it sits
        //    beside, and the reason the generation is on the camera's own thread rather than on
        //    a runtime worker (see the `imaging` module).
        //
        // Ignored by default because the gate would pay 3.6 s on every `cargo test` for a number
        // that changes only when the generator does.
        let dir = scratch_dir("fullsize");
        let profile = CameraProfile::default();
        let camera = camera_with(profile, CameraFaultPlan::none());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime");

        let started = std::time::Instant::now();
        let result = runtime.block_on(async {
            camera.connect().await.expect("connects");
            camera
                .capture(&request(&dir, "light_fullsize"))
                .await
                .expect("captures")
        });
        let elapsed = started.elapsed();

        let raw = result.raw().expect("a science file");
        // 6000 × 4000 × 2 bytes, plus a header block and the padding to 2880.
        assert!(
            raw.size_bytes >= 48_000_000,
            "the FITS is {} bytes",
            raw.size_bytes
        );
        println!(
            "full-size 6000x4000: {:.2} s wall clock, FITS {} MB, JPEG {} MB",
            elapsed.as_secs_f64(),
            raw.size_bytes / 1_000_000,
            result.jpeg().expect("a rendition").size_bytes / 1_000_000
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
