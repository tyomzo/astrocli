//! `CamOps` — the blocking half of the driver, and the seam that keeps the other half testable.
//!
//! Everything below this trait talks to libgphoto2 and can only run with a camera on the end of a
//! cable. Everything above it — the thread, the command channel, the timeouts, the wedge
//! protocol, the claim diagnosis — is ordinary Rust that must be provable on a machine with
//! neither camera nor libgphoto2, because that is what CI is.
//!
//! SDD §5.3.3 already required this seam for a different reason: the CLI fallback
//! (`GPhoto2Cli`) is specified as "the same internal `CamOps` trait by shelling out". So the
//! trait has two production implementations planned and one test implementation, and none of
//! them is privileged.
//!
//! # Two properties that are not stylistic
//!
//! **Every method is synchronous and blocking.** These calls take between 0.4 ms and several
//! seconds (M2-T01 measured a capture at 2.08 s) and there is nothing to await — libgphoto2 is a
//! blocking C library. Making the trait `async` would let an implementation park the camera
//! thread on a runtime that is not there, and would hide the exact fact the thread model exists
//! to contain.
//!
//! **The implementation is built on the camera thread, never moved to it.** That is what
//! [`CamOpsFactory`] is for: the factory is `Send` and crosses the thread boundary, the `CamOps`
//! it produces never does. `gphoto2::Context` is not safe to share or move between threads, and
//! a design that constructed one on the caller's thread and sent it over would be relying on the
//! binding's `Send` impl to be more generous than the C library it wraps.

use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    AvailableSettings, BatteryStatus, CameraCapabilities, CameraSettings, DeviceInfo, ImageFormat,
    StorageInfo,
};
use astroctl_hal::camera::CapturedFileKind;

/// A camera configuration key, as libgphoto2 spells it.
///
/// The names are the ones M2-T01 read out of the R10's 91-entry config tree, not ones inferred
/// from documentation. `shutterspeed` in particular is one word, which is the sort of thing that
/// costs an evening when it is guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CfgKey {
    /// `iso` — 27 choices on the R10.
    Iso,
    /// `shutterspeed`. **Constrained by the physical mode dial**: with the dial on Bulb the
    /// camera offers only `bulb` and `Unknown value df00`, so a write here can legitimately fail
    /// on a body that is working perfectly (M2-T01).
    Shutter,
    /// `aperture` — 22 choices, and absent entirely behind a fully manual lens.
    Aperture,
    /// `imageformat` — 23 choices, spelled in the body's own vocabulary (`RAW`, `Large Fine
    /// JPEG`, `RAW + Large Fine JPEG`), which is why [`ImageFormat`] needs mapping rather than
    /// printing.
    ImageFormat,
}

impl CfgKey {
    /// The key as libgphoto2 expects it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Iso => "iso",
            Self::Shutter => "shutterspeed",
            Self::Aperture => "aperture",
            Self::ImageFormat => "imageformat",
        }
    }
}

/// The choice lists exactly as the camera enumerated them.
///
/// Strings, because that is what the body returns. **Interpretation happens above this trait,
/// not below it** — a `CamOps` implementation's job is to get the bytes off the wire, and every
/// implementation interpreting them separately is how the CLI fallback and the binding end up
/// disagreeing about what `RAW + Large Fine JPEG` means. It also puts the interpretation where
/// CI can test it, since CI has no camera and no libgphoto2.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawChoices {
    /// `iso` choices.
    pub(crate) isos: Vec<String>,
    /// `shutterspeed` choices, which the mode dial constrains.
    pub(crate) shutters: Vec<String>,
    /// `aperture` choices; empty behind a manual lens.
    pub(crate) apertures: Vec<String>,
    /// `imageformat` choices in the body's own prose.
    pub(crate) formats: Vec<String>,
}

/// The settings in force, as the camera spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawSettings {
    /// e.g. `1600`.
    pub(crate) iso: String,
    /// e.g. `30`, `1/250`, `bulb`.
    pub(crate) shutter: String,
    /// e.g. `5.6`; `None` behind a manual lens.
    pub(crate) aperture: Option<String>,
    /// The `imageformat` token, e.g. `RAW + Large Fine JPEG`.
    pub(crate) format: String,
}

/// What opening the camera establishes, before interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawIdentity {
    /// Model, serial and firmware for the UI and for calibration tagging (HAL-06).
    pub(crate) info: DeviceInfo,
    /// The settings in force at the moment of connection.
    pub(crate) settings: RawSettings,
    /// What the body offers.
    pub(crate) choices: RawChoices,
    /// Whether the abilities report a preview/live-view operation.
    pub(crate) has_live_view: bool,
}

/// A file the body is holding, named the way the body names it.
///
/// Two strings rather than a `PathBuf` because this is not a filesystem path — it is a PTP
/// folder and filename inside the camera (`/`, `capt0000.cr3`), and joining them into something
/// that looks like a host path is how a driver ends up trying to open one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawFileRef {
    /// The camera-side folder, e.g. `/`.
    pub(crate) folder: String,
    /// The camera-side filename, e.g. `capt0000.cr3`. Its extension is the only thing that says
    /// what the body actually produced, which is why the driver takes the extension from here
    /// rather than assuming `cr3`.
    pub(crate) name: String,
}

impl RawFileRef {
    /// The filename's extension, lowercased, or `None` for a name that has none.
    ///
    /// Interpretation, so it lives above the trait: the body reports `CAPT0001.CR3` or
    /// `capt0000.cr3` depending on the path that produced it, and a session directory holding
    /// both spellings for the same kind of file is a needless thing to explain.
    pub(crate) fn extension(&self) -> Option<String> {
        let (_, extension) = self.name.rsplit_once('.')?;
        (!extension.is_empty()).then(|| extension.to_ascii_lowercase())
    }

    /// What this file is for, from its extension.
    ///
    /// Anything that is not a JPEG is the science file: a body shooting RAW+JPEG names its raw
    /// `.cr3` here and `.nef`/`.arw` on other makes, and enumerating raw extensions would mean
    /// this driver refusing to carry a frame off a body it otherwise drives perfectly.
    pub(crate) fn kind(&self) -> CapturedFileKind {
        match self.extension().as_deref() {
            Some("jpg" | "jpeg") => CapturedFileKind::Jpeg,
            _ => CapturedFileKind::Raw,
        }
    }
}

/// What one trigger produced, uninterpreted.
// No `Eq`: `exposure_seconds` is what the camera said, and a float has no equality worth
// deriving. Nothing compares two captures for identity.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RawCapture {
    /// The files the body is holding, in the order it reported them. One for RAW, two for
    /// RAW+JPEG — and the second arrives as a separate event rather than from the trigger call,
    /// which is why this is a list and not an `Option` of a second field.
    pub(crate) files: Vec<RawFileRef>,
    /// The exposure the camera reports, if it reports one at all.
    ///
    /// `None` is the ordinary answer. It is `Some` only where the body volunteers the figure —
    /// the R10 reports `BulbExposureTime` after a bulb hold (M2-T01 saw `9` for a 10 s hold) —
    /// and the caller falls back to the duration it asked for. Recording the camera's own number
    /// where there is one matters because that one-second discrepancy is the real integration
    /// time, and the frame metadata should say what the sensor did.
    pub(crate) exposure_seconds: Option<f64>,
}

/// The one signal that reaches the camera thread while it is busy.
///
/// A command cannot. There is one queue and one context (SDD §5.3.1), so a `CamCmd::Abort` sent
/// during a five-minute bulb exposure would be serviced five minutes later — which is not an
/// abort, it is a report. This is shared memory instead: a generation counter behind a condvar,
/// which the blocking thread reads at the points where it is *not* inside a libgphoto2 call.
///
/// **What it can interrupt is a property of where the thread is, and the driver does not pretend
/// otherwise.** The bulb hold is this driver's own sleep, so it ends within a condvar wakeup of
/// the abort. `capture_image()` and `download_to()` are C calls with no cancellation, so an abort
/// landing inside one takes effect when it returns. That is the same race the simulator
/// documents and it resolves the same way: raised before the exposure or before the download,
/// `Aborted` with nothing on disk; raised once the bytes are going to disk, the write wins and
/// the capture succeeds. A caller that sees `Ok` after aborting has a frame, not a lie.
#[derive(Debug, Default)]
pub(crate) struct AbortSignal {
    /// Bumped once per abort. A counter rather than a flag so that an abort raised and observed
    /// cannot arm the *next* capture: each capture records the generation it started at and only
    /// reacts to movement away from it.
    generation: Mutex<u64>,
    /// Wakes the bulb hold. Without it the hold would have to poll, and a poll interval is a
    /// choice between a busy loop and an abort that is late by up to that interval.
    changed: Condvar,
}

impl AbortSignal {
    /// The generation a capture should record before it starts.
    pub(crate) fn generation(&self) -> u64 {
        *self
            .generation
            .lock()
            .expect("the abort generation is never poisoned")
    }

    /// Raises an abort. Idempotent in effect and never fails — it is a stopping command
    /// (SDD §5.8.1), and a stop that can be refused is not one.
    pub(crate) fn raise(&self) {
        let mut generation = self
            .generation
            .lock()
            .expect("the abort generation is never poisoned");
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Whether an abort has been raised since `since`.
    pub(crate) fn raised_since(&self, since: u64) -> bool {
        self.generation() != since
    }

    /// Sleeps for `span`, returning early if an abort is raised. `true` if one was.
    ///
    /// This is the bulb hold. It is a real blocking wait because it runs on the camera thread,
    /// between the shutter opening and the shutter closing, where there is no runtime to await
    /// on and nothing else this thread may do.
    ///
    /// Compiled where there is a [`CamOps`] to call it — the mock and the real backend — for the
    /// same reason [`super::camera::CanonGPhoto2Camera::new`] is: a default build has the trait
    /// and no implementation of it, so this is the one method with no caller there. The rest of
    /// `AbortSignal` is used by the thread and is always compiled.
    #[cfg(any(test, feature = "libgphoto2"))]
    pub(crate) fn hold(&self, span: Duration, since: u64) -> bool {
        let generation = self
            .generation
            .lock()
            .expect("the abort generation is never poisoned");
        let (generation, _timed_out) = self
            .changed
            .wait_timeout_while(generation, span, |generation| *generation == since)
            .expect("the abort generation is never poisoned");
        *generation != since
    }
}

/// What opening the camera establishes about it, interpreted.
///
/// Produced by [`interpret_identity`] from a [`RawIdentity`], as one value because opening is one
/// round of work on the camera thread: autodetect, open, read the abilities, walk the config
/// tree. Splitting it into four commands would put four thread round trips and four timeouts
/// where the hardware does one thing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CameraIdentity {
    /// Model, serial and firmware for the UI and for calibration tagging (HAL-06).
    pub(crate) info: DeviceInfo,
    /// What the body can do, derived from its own enumerated choices (HAL-05).
    pub(crate) capabilities: CameraCapabilities,
    /// The settings in force at the moment of connection.
    pub(crate) settings: CameraSettings,
    /// Every value the body will accept — the camera's lists, never ours.
    pub(crate) available: AvailableSettings,
    /// The body's `imageformat` choices as it spells them, e.g. `RAW + Large Fine JPEG`.
    ///
    /// Kept beside the mapped [`ImageFormat`]s in `available` because setting a format means
    /// writing one of *these* strings back. Mapping to the enum loses the token, and
    /// reconstructing it would mean this driver deciding how a body spells its own settings.
    pub(crate) format_choices: Vec<String>,
}

/// The blocking camera operations. One implementation per transport; see the module docs.
///
/// Scope note: M2-T02 delivered connect, disconnect and settings; M2-T03 added `capture`,
/// `capture_bulb`, `download` and `abort`. M2-T04 adds the live-view pair, which belongs here for
/// the same reason these do and which the thread will dispatch the same way.
pub(crate) trait CamOps {
    /// Autodetects, opens and interrogates the camera.
    ///
    /// Returns what the camera said, uninterpreted. A `Transport` error's *text* is passed
    /// through unchanged too: the layer above turns "Could not claim the USB device" into a
    /// diagnosis (see [`gvfs`](super::gvfs)), and it can only do that if this layer has not
    /// already rewritten it.
    ///
    /// # Errors
    /// `Transport` when the device is absent or claimed by something else, `Protocol` if the
    /// camera answers but the interrogation fails.
    fn open(&mut self) -> Result<RawIdentity, DeviceError>;

    /// Releases the camera. Idempotent.
    ///
    /// # Errors
    /// Transport-level failures only.
    fn close(&mut self) -> Result<(), DeviceError>;

    /// Reads the settings currently in force, from the camera.
    ///
    /// # Errors
    /// `NotConnected`, `Protocol`, `Transport`.
    fn read_settings(&mut self) -> Result<RawSettings, DeviceError>;

    /// Re-enumerates what the body will accept.
    ///
    /// Re-read rather than cached from [`open`](Self::open) on purpose: the mode dial changes
    /// what `shutterspeed` offers while the camera is connected, and a cached list would go on
    /// offering values the body has stopped accepting.
    ///
    /// # Errors
    /// As [`read_settings`](Self::read_settings).
    fn read_choices(&mut self) -> Result<RawChoices, DeviceError>;

    /// Writes one setting.
    ///
    /// The value is a camera token that the caller has already checked against the body's own
    /// choices — this method does not validate, because the list it would validate against is one
    /// it would have to re-read on every write (11 ms measured, per write, doubled).
    ///
    /// # Errors
    /// `Rejected` if the camera refuses the token, otherwise transport-level.
    fn write_setting(&mut self, key: CfgKey, value: &str) -> Result<(), DeviceError>;

    /// Battery charge and whether the body is on external power.
    ///
    /// # Errors
    /// `NotConnected`, `Protocol`, `Transport`.
    fn battery(&mut self) -> Result<BatteryStatus, DeviceError>;

    /// Free and total space on the card.
    ///
    /// # Errors
    /// As [`battery`](Self::battery).
    fn storage(&mut self) -> Result<StorageInfo, DeviceError>;

    /// Takes one exposure at the settings already in force and returns what the body is holding.
    ///
    /// **Blocks for the whole exposure and, on this body, the whole USB transfer too.** M2-T01
    /// measured 2.08 s for a capture and then 2.67 ms to write the 32 MB to disk — roughly
    /// 12 GB/s, which is memory bandwidth, not USB. With `capturetarget=Internal RAM` libgphoto2
    /// has the entire frame buffered before [`download`](Self::download) is called at all. That
    /// is the PRF-05 consequence worth knowing here: one full frame is resident inside the C
    /// library for the duration, against a 512 MB budget.
    ///
    /// It follows that there is no exposing→downloading transition to observe *inside* this
    /// call, which is why the driver adds no progress channel — see the module docs of
    /// [`super::camera`].
    ///
    /// Does not set the shutter: the caller has already put the body where it wants it, and a
    /// capture that quietly rewrote settings would produce a frame nobody asked for.
    ///
    /// # Errors
    /// `Rejected` if the body refuses to fire (no card, autofocus failure, a mode dial that
    /// forbids it), `NotConnected`, `Transport`.
    fn capture(&mut self) -> Result<RawCapture, DeviceError>;

    /// Holds the shutter open for `duration` and returns what the body is holding (CAM-04).
    ///
    /// Duration-driven: the driver opens the shutter, waits, and closes it. The wait is
    /// [`AbortSignal::hold`], so an abort ends the exposure within a condvar wakeup rather than
    /// at the end of a five-minute integration.
    ///
    /// **The shutter is released on every path out of this method**, including the aborted one
    /// and the failed one. A bulb exposure whose release is skipped leaves the sensor
    /// integrating with nothing to stop it, which is the one failure here that damages the
    /// session rather than the frame.
    ///
    /// # Errors
    /// `Aborted` if the signal was raised during the hold — with the shutter closed and the
    /// resulting frame left on the camera, never downloaded. `Unsupported` if the body exposes no
    /// bulb mechanism, otherwise as [`capture`](Self::capture).
    fn capture_bulb(
        &mut self,
        duration: Duration,
        abort: &AbortSignal,
        since: u64,
    ) -> Result<RawCapture, DeviceError>;

    /// Copies one camera-side file to `destination`, returning the bytes written.
    ///
    /// `destination` is a temporary path the caller has already unlinked — see
    /// [`super::download`], which is where that unconditional unlink lives and why.
    ///
    /// # Errors
    /// `Transport` if the transfer fails or the device disappears, `NotConnected`.
    fn download(&mut self, file: &RawFileRef, destination: &Path) -> Result<u64, DeviceError>;

    /// Releases the shutter if this body might be holding it open. Idempotent.
    ///
    /// The safety net behind [`capture_bulb`](Self::capture_bulb)'s own release: if the release
    /// itself failed — the one case the spike flagged as *"SHUTTER MAY STILL BE OPEN"* — this is
    /// what a later abort uses to try again. On a body with no bulb mechanism it is a no-op
    /// rather than an error, because refusing to stop something is never the right answer to a
    /// stopping command.
    ///
    /// # Errors
    /// Transport-level failures only.
    fn abort(&mut self) -> Result<(), DeviceError>;
}

/// Builds a [`CamOps`] on the thread that will use it.
///
/// `Send + Sync` so the driver can keep one behind an `Arc` and hand a clone to each camera
/// thread it spawns — a reconnect after a wedge needs a *second* thread, and therefore a factory
/// that outlives the first. The `CamOps` it returns is deliberately not required to be either,
/// which is the whole point (module docs). `Debug` because everything that holds a driver holds
/// it behind an `Arc<dyn …>` and derives `Debug`, exactly as the HAL's device traits do.
pub(crate) trait CamOpsFactory: std::fmt::Debug + Send + Sync + 'static {
    /// Constructs the operations object. Runs on the camera thread.
    ///
    /// # Errors
    /// `Transport` if the underlying library cannot be initialised at all.
    fn build(&self) -> Result<Box<dyn CamOps>, DeviceError>;
}

// -----------------------------------------------------------------------------------------
// Turning the camera's own answers into a capability report
// -----------------------------------------------------------------------------------------

/// Sensor geometry, which no config key reports.
///
/// libgphoto2's config tree describes *controls*, not the sensor: there is no key for pixel pitch
/// or active area. These are body constants, and for the reference camera they are PRD §4.3/§8.3's
/// measured figures — 22.3 mm of APS-C across 6000 px is 3.72 µm. A second supported body brings
/// a second constant, not a config field, because an operator cannot be expected to know their
/// sensor's pixel pitch and a wrong guess silently corrupts every plate solve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BodyGeometry {
    /// Active width in pixels.
    pub(crate) width_px: u32,
    /// Active height in pixels.
    pub(crate) height_px: u32,
    /// Pixel pitch in micrometres.
    pub(crate) pixel_size_um: f64,
}

impl BodyGeometry {
    /// The Canon EOS R10 (PRD §4.3, §8.3).
    ///
    /// **Active area, not raw area.** M2-T01's `rawler` decode reported 6192 × 4060 including the
    /// masked border; the PRD quotes 6000 × 4000 active and that is what the plate scale and the
    /// capability report must use.
    pub(crate) const R10: Self = Self {
        width_px: 6000,
        height_px: 4000,
        pixel_size_um: 3.72,
    };
}

/// Turns the body's choice lists into the HAL's [`AvailableSettings`].
///
/// The only lossy step is `formats`, which becomes an [`ImageFormat`] set: a token the enum
/// cannot express (`Movie`, the R10's raw-quality variants) is dropped rather than approximated,
/// because a settings UI offering a format the driver cannot then select is worse than one that
/// offers fewer.
pub(crate) fn interpret_choices(raw: &RawChoices) -> AvailableSettings {
    let mut formats: Vec<ImageFormat> = Vec::new();
    for token in &raw.formats {
        if let Some(format) = format_from_token(token) {
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
    }
    AvailableSettings {
        isos: raw.isos.clone(),
        shutters: raw.shutters.clone(),
        apertures: raw.apertures.clone(),
        formats,
    }
}

/// Turns the body's current settings into the HAL's [`CameraSettings`].
///
/// A format token that maps to nothing becomes [`ImageFormat::Raw`] with a warning rather than an
/// error. The alternative is that a camera left on a video or a raw-quality variant this driver
/// does not model cannot report its settings *at all*, which would make the whole camera
/// unreadable over a field the operator only wanted to look at.
pub(crate) fn interpret_settings(raw: RawSettings) -> CameraSettings {
    let format = format_from_token(&raw.format).unwrap_or_else(|| {
        tracing::warn!(
            token = %raw.format,
            "the camera reports an image format this driver does not model; reporting RAW"
        );
        ImageFormat::Raw
    });
    CameraSettings {
        iso: raw.iso,
        shutter: raw.shutter,
        aperture: raw.aperture,
        format,
    }
}

/// Turns everything an open established into the interpreted form the driver caches.
pub(crate) fn interpret_identity(raw: RawIdentity, geometry: BodyGeometry) -> CameraIdentity {
    let available = interpret_choices(&raw.choices);
    CameraIdentity {
        capabilities: capabilities_from(&available, geometry, raw.has_live_view),
        settings: interpret_settings(raw.settings),
        format_choices: raw.choices.formats,
        available,
        info: raw.info,
    }
}

/// Derives the capability report from what the camera actually offered.
///
/// **The ISO and shutter bounds come from the body's enumerated choices, never from a table.**
/// The task is explicit that `get_available_settings` must be the camera's own list, and the
/// bounds are the same claim one step on: a hardcoded `max_iso` is a number that is wrong the
/// first time a different body is plugged in, and wrong invisibly.
pub(crate) fn capabilities_from(
    available: &AvailableSettings,
    geometry: BodyGeometry,
    has_live_view: bool,
) -> CameraCapabilities {
    // A token that parses is a bound candidate; one that does not (`Unknown value df00`, which
    // the R10 really does return with the dial on Bulb) is skipped rather than treated as zero.
    let isos: Vec<u32> = available
        .isos
        .iter()
        .filter_map(|token| token.parse::<u32>().ok())
        .collect();
    let shutters: Vec<f64> = available
        .shutters
        .iter()
        .filter_map(|token| shutter_seconds(token))
        .collect();
    // See the bounds below: an empty list must report 0..0, not inf..0.
    let (min_shutter_s, max_shutter_s) = if shutters.is_empty() {
        (0.0, 0.0)
    } else {
        (
            shutters.iter().copied().fold(f64::INFINITY, f64::min),
            shutters.iter().copied().fold(0.0, f64::max),
        )
    };

    CameraCapabilities {
        // `bulb` in the shutter list is the body telling us it has a bulb mode. Derived rather
        // than assumed: M2-T01 proved bulb works on the R10, but this driver is not R10-only.
        has_bulb: available
            .shutters
            .iter()
            .any(|token| token.eq_ignore_ascii_case("bulb")),
        has_live_view,
        // No PTP config key corresponds to mirror lockup on a mirrorless body, and M2-T01
        // deliberately left the mirror-lockup equivalent untested. Claiming it because a menu
        // item exists would promise a capability nothing has exercised.
        has_mirror_lockup: false,
        sensor_width_px: geometry.width_px,
        sensor_height_px: geometry.height_px,
        pixel_size_um: geometry.pixel_size_um,
        supported_formats: available.formats.clone(),
        min_iso: isos.iter().copied().min().unwrap_or(0),
        max_iso: isos.iter().copied().max().unwrap_or(0),
        // Zero, not infinity, when the body enumerates no timed shutter at all. That case is not
        // hypothetical: with the physical mode dial on Bulb the R10 offers only `bulb` and
        // `Unknown value df00`, neither of which names a duration, and folding the resulting
        // empty list from `f64::INFINITY` printed `shutter infs..0.0s` at an operator on real
        // hardware (M2-T03). `0..0` reads as "the body is telling us nothing", which is true;
        // `inf` reads as a defect in the reader.
        min_shutter_s,
        max_shutter_s,
    }
}

/// Seconds for a Canon shutter token, or `None` for one that names no duration.
///
/// Canon spells sub-second speeds as fractions (`1/250`) and the rest as decimals (`30`, `0.5`),
/// so both forms have to parse. `bulb` and the body's `Unknown value df00` return `None` — they
/// are shutter *settings* without a duration, and the difference matters: a capture with the
/// shutter on `bulb` is refused and pointed at `capture_bulb`, exactly as the simulator does.
pub(crate) fn shutter_seconds(token: &str) -> Option<f64> {
    if let Some((numerator, denominator)) = token.split_once('/') {
        let numerator: f64 = numerator.trim().parse().ok()?;
        let denominator: f64 = denominator.trim().parse().ok()?;
        // A zero denominator is malformed input, not an infinitely fast shutter.
        return (denominator != 0.0).then_some(numerator / denominator);
    }
    token.trim().parse().ok()
}

/// Maps an [`ImageFormat`] onto the token the body uses for it.
///
/// The R10's `imageformat` has 23 choices and spells them in prose. Matching on substrings rather
/// than on an exact table is what lets one driver serve bodies that disagree about whether it is
/// `Large Fine JPEG` or `JPEG Fine L` — and the caller only ever gets a token the camera itself
/// listed, because the search runs over `available.formats`' source list.
pub(crate) fn format_token(format: ImageFormat, choices: &[String]) -> Option<&str> {
    let wants_raw = matches!(format, ImageFormat::Raw | ImageFormat::RawPlusJpeg);
    let wants_jpeg = matches!(format, ImageFormat::Jpeg | ImageFormat::RawPlusJpeg);

    choices
        .iter()
        .find(|choice| {
            let lower = choice.to_ascii_lowercase();
            // `craw` is Canon's compressed raw and is still raw; a token naming both is the
            // RAW+JPEG entry.
            let has_raw = lower.contains("raw");
            let has_jpeg = lower.contains("jpeg") || lower.contains("jpg");
            has_raw == wants_raw && has_jpeg == wants_jpeg
        })
        .map(String::as_str)
}

/// Reads a body's `imageformat` token back into an [`ImageFormat`].
pub(crate) fn format_from_token(token: &str) -> Option<ImageFormat> {
    let lower = token.to_ascii_lowercase();
    match (
        lower.contains("raw"),
        lower.contains("jpeg") || lower.contains("jpg"),
    ) {
        (true, true) => Some(ImageFormat::RawPlusJpeg),
        (true, false) => Some(ImageFormat::Raw),
        (false, true) => Some(ImageFormat::Jpeg),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use astroctl_core::types::{AvailableSettings, ImageFormat};

    use super::{
        capabilities_from, format_from_token, format_token, shutter_seconds, BodyGeometry, CfgKey,
    };

    /// The R10's real `imageformat` vocabulary, as prose, so the mapping is tested against the
    /// shape the body actually returns rather than against three tidy words.
    fn r10_format_choices() -> Vec<String> {
        ["Large Fine JPEG", "RAW", "RAW + Large Fine JPEG"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    #[test]
    fn the_config_keys_are_the_ones_the_spike_read_off_the_body() {
        assert_eq!(CfgKey::Iso.as_str(), "iso");
        // One word. This is the one that gets guessed wrong.
        assert_eq!(CfgKey::Shutter.as_str(), "shutterspeed");
        assert_eq!(CfgKey::Aperture.as_str(), "aperture");
        assert_eq!(CfgKey::ImageFormat.as_str(), "imageformat");
    }

    #[test]
    fn canon_spells_shutter_speeds_two_ways_and_both_parse() {
        assert_eq!(shutter_seconds("1/250"), Some(1.0 / 250.0));
        assert_eq!(shutter_seconds("30"), Some(30.0));
        assert_eq!(shutter_seconds("0.5"), Some(0.5));
        // Settings that name no duration.
        assert_eq!(shutter_seconds("bulb"), None);
        // The literal string the R10 returns with the mode dial on Bulb (M2-T01).
        assert_eq!(shutter_seconds("Unknown value df00"), None);
    }

    #[test]
    fn capabilities_come_from_the_bodys_own_lists_not_from_a_table() {
        let available = AvailableSettings {
            isos: vec!["100".to_owned(), "1600".to_owned(), "32000".to_owned()],
            shutters: vec![
                "1/4000".to_owned(),
                "30".to_owned(),
                "bulb".to_owned(),
                "Unknown value df00".to_owned(),
            ],
            apertures: vec!["1.4".to_owned()],
            formats: vec![ImageFormat::Raw, ImageFormat::RawPlusJpeg],
        };

        let caps = capabilities_from(&available, BodyGeometry::R10, true);

        assert_eq!(caps.min_iso, 100);
        assert_eq!(caps.max_iso, 32000);
        assert!((caps.min_shutter_s - 1.0 / 4000.0).abs() < f64::EPSILON);
        // 30 s, not the unparseable token and not `bulb` — neither names a duration.
        assert!((caps.max_shutter_s - 30.0).abs() < f64::EPSILON);
        // `bulb` in the shutter list is what establishes the bulb capability.
        assert!(caps.has_bulb);
        assert!(caps.has_live_view);
        // Untested on real hardware, so not claimed.
        assert!(!caps.has_mirror_lockup);
        assert_eq!(caps.sensor_width_px, 6000);
        assert!((caps.pixel_size_um - 3.72).abs() < f64::EPSILON);
        assert_eq!(caps.supported_formats, available.formats);
    }

    #[test]
    fn a_body_that_enumerates_no_timed_shutter_reports_zero_bounds_rather_than_infinity() {
        // The R10 with its physical mode dial on Bulb, which is how it was found on the bench:
        // neither token names a duration, so there is no bound to report. Folding the empty list
        // from `f64::INFINITY` printed `shutter infs..0.0s` at an operator (M2-T03).
        let available = AvailableSettings {
            isos: vec!["1600".to_owned()],
            shutters: vec!["Unknown value df00".to_owned(), "bulb".to_owned()],
            apertures: vec!["1.8".to_owned()],
            formats: vec![ImageFormat::Raw],
        };

        let caps = capabilities_from(&available, BodyGeometry::R10, true);

        assert_eq!(caps.min_shutter_s, 0.0, "not infinity");
        assert_eq!(caps.max_shutter_s, 0.0);
        assert!(caps.min_shutter_s.is_finite() && caps.max_shutter_s.is_finite());
        // The body still has a bulb mode; it is only the *timed* bounds that are empty.
        assert!(caps.has_bulb);
    }

    #[test]
    fn a_body_offering_no_bulb_setting_does_not_get_a_bulb_capability() {
        let available = AvailableSettings {
            isos: vec!["200".to_owned()],
            shutters: vec!["1/100".to_owned()],
            apertures: Vec::new(),
            formats: vec![ImageFormat::Jpeg],
        };
        assert!(!capabilities_from(&available, BodyGeometry::R10, false).has_bulb);
    }

    #[test]
    fn image_formats_map_onto_the_bodys_own_prose_in_both_directions() {
        let choices = r10_format_choices();
        assert_eq!(format_token(ImageFormat::Raw, &choices), Some("RAW"));
        assert_eq!(
            format_token(ImageFormat::Jpeg, &choices),
            Some("Large Fine JPEG")
        );
        assert_eq!(
            format_token(ImageFormat::RawPlusJpeg, &choices),
            Some("RAW + Large Fine JPEG")
        );

        assert_eq!(format_from_token("RAW"), Some(ImageFormat::Raw));
        assert_eq!(
            format_from_token("RAW + Large Fine JPEG"),
            Some(ImageFormat::RawPlusJpeg)
        );
        // Canon's compressed raw is still raw.
        assert_eq!(format_from_token("cRAW"), Some(ImageFormat::Raw));
        assert_eq!(format_from_token("Movie"), None);
    }

    #[test]
    fn a_format_the_body_does_not_offer_has_no_token() {
        // A body with no raw mode at all. The caller turns this into `Unsupported`; what matters
        // here is that it does not fall back to the nearest entry, which would silently shoot
        // JPEG for a session that asked for raw.
        let jpeg_only = vec!["Large Fine JPEG".to_owned()];
        assert_eq!(format_token(ImageFormat::Raw, &jpeg_only), None);
        assert_eq!(format_token(ImageFormat::RawPlusJpeg, &jpeg_only), None);
    }
}
