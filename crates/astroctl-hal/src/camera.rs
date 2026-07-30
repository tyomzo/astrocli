//! The imaging camera interface — PRD §4.1, HAL-03, SDD §5.3.
//!
//! Read the crate-level "contract every device trait shares" first. Two things are specific to
//! this trait:
//!
//! **A capture blocks the camera and nothing else.** On the reference body one exposure occupies
//! libgphoto2's single context for ~2 s (measured, SDD §5.3.1), during which live view stops.
//! That is a property of the hardware interface, and the design surfaces it rather than hiding
//! it — but it must stay inside the driver. Mount polling, the event bus and the API are off
//! that thread by construction, and T-ISO-1 (SDD §9) fails if that ever stops being true.
//!
//! **The driver hands back durable files, not bytes.** A frame is 32 MB and the field node has
//! 512 MB (PRF-05), so nothing streams a frame through memory to a caller that would only write
//! it to disk. [`Camera::capture`] takes the directory and the name stem — the frame store owns
//! the layout and the frame id (SDD §5.5) — and returns paths that are complete files.

use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    AvailableSettings, BatteryStatus, CameraCapabilities, CameraSettings, DeviceInfo, ImageFormat,
    StorageInfo,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::stream::FrameStream;

/// Where a capture must put its files, and under what name.
///
/// The caller owns the session layout and the frame id (SDD §5.5, `frames/light_<id>.cr3`); the
/// driver owns the extension, because only it knows whether this body produces `.cr3`, `.fits`
/// or both. Splitting it there is what lets the frame store guarantee unique ids across crashes
/// (REL-04) without knowing a thing about file formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureRequest {
    /// Directory the finished files must land in. It exists and is writable — the frame store
    /// created it.
    pub dir: PathBuf,
    /// Filename stem without extension, e.g. `light_00042`.
    pub stem: String,
}

impl CaptureRequest {
    /// Builds a request.
    ///
    /// `stem` is a bare filename component: a driver builds paths by joining it to `dir` and
    /// appending an extension, so a separator in it would write outside the session directory.
    /// Debug builds assert that; release builds trust the frame store, which is the only caller.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>, stem: impl Into<String>) -> Self {
        let request = Self {
            dir: dir.into(),
            stem: stem.into(),
        };
        debug_assert!(
            !request.stem.is_empty()
                && !request.stem.contains(std::path::is_separator)
                && request.stem != "..",
            "capture stem `{}` is not a bare filename component",
            request.stem
        );
        request
    }

    /// The path a file with this extension would take, e.g. `…/light_00042.cr3`.
    ///
    /// Drivers use this so that the temporary-name-then-rename discipline is the only naming
    /// decision they make.
    #[must_use]
    pub fn path_with_extension(&self, extension: &str) -> PathBuf {
        self.dir.join(format!("{}.{}", self.stem, extension))
    }
}

/// What one file produced by a capture is for.
///
/// Two kinds because one exposure can produce two files: the reference body shoots `RAW+JPEG`,
/// and the simulator writes a 16-bit FITS plus an 8-bit rendition (task M1-T06). A result that
/// carried a single `path` would have to pick one and lose the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapturedFileKind {
    /// The science file — camera raw (CR3) or the simulator's FITS. This is the one that gets
    /// transferred, stacked and kept (REL-11, REL-13).
    Raw,
    /// A camera-produced JPEG of the same exposure. Convenient for a preview, never the
    /// authoritative frame.
    Jpeg,
}

/// One file a capture wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedFile {
    /// Final path — a complete, durable file (see [`Camera::capture`]).
    pub path: PathBuf,
    /// What it is.
    pub kind: CapturedFileKind,
    /// Size on disk, so a caller can journal it without a `stat` (SDD §6 `quality_<id>.json`).
    pub size_bytes: u64,
}

/// The outcome of one exposure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureResult {
    /// Every file this exposure produced, raw first.
    pub files: Vec<CapturedFile>,
    /// The settings actually in force. Read back rather than echoed: a body may coerce a
    /// requested value, and the frame metadata must record what the sensor did, not what it was
    /// asked to do.
    pub settings: CameraSettings,
    /// When the shutter opened.
    pub started_at: DateTime<Utc>,
    /// How long it stayed open. For a bulb exposure this is what the camera reports if it
    /// reports anything, otherwise the requested duration.
    pub exposure: Duration,
}

impl CaptureResult {
    /// The science file, if this capture produced one.
    #[must_use]
    pub fn raw(&self) -> Option<&CapturedFile> {
        self.file_of_kind(CapturedFileKind::Raw)
    }

    /// The camera JPEG, if this capture produced one.
    #[must_use]
    pub fn jpeg(&self) -> Option<&CapturedFile> {
        self.file_of_kind(CapturedFileKind::Jpeg)
    }

    /// Bytes written by this exposure across all files — what the disk monitor consumed
    /// (REL-12).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size_bytes).sum()
    }

    fn file_of_kind(&self, kind: CapturedFileKind) -> Option<&CapturedFile> {
        self.files.iter().find(|file| file.kind == kind)
    }
}

/// One live-view frame: a JPEG as the camera produced it, plus when it was taken.
///
/// The bytes are behind an `Arc` because the WS hub fans one frame out to every connected client
/// (SDD §5.8.3) and copying 100 KB per client per frame would be the most expensive thing in the
/// preview path. `captured_at` is what lets the PWA label a stale preview rather than present it
/// as the present (SDD §8.3(8), PRF-01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveViewFrame {
    /// JPEG bytes, ready to put on the wire unmodified.
    pub jpeg: Arc<[u8]>,
    /// When the camera produced it.
    pub captured_at: DateTime<Utc>,
}

impl LiveViewFrame {
    /// Wraps JPEG bytes with a capture time.
    #[must_use]
    pub fn new(jpeg: impl Into<Arc<[u8]>>, captured_at: DateTime<Utc>) -> Self {
        Self {
            jpeg: jpeg.into(),
            captured_at,
        }
    }
}

/// An imaging camera (PRD §4.1, HAL-03).
///
/// `Debug` is a supertrait for the same reason as on
/// [`MountDevice`](crate::mount::MountDevice): everything that holds one holds it behind an
/// `Arc<dyn …>` and wants to derive `Debug`.
#[async_trait]
pub trait Camera: Debug + Send + Sync {
    /// Opens the camera and takes ownership of it for the driver's lifetime.
    ///
    /// # Errors
    /// `Transport` if the camera is absent or claimed by another process (`gvfs-gphoto2` is the
    /// usual culprit and the message should say so), `Timeout`, `Protocol`. Connecting an
    /// already-connected camera is `Ok(())`.
    async fn connect(&self) -> Result<(), DeviceError>;

    /// Releases the camera.
    ///
    /// Ends any live-view stream (subscribers see the stream end, per
    /// [`FrameStream`](crate::stream::FrameStream)). An in-flight capture is *not* abandoned:
    /// SDD §7's shutdown order finishes the download first, because a half-downloaded frame is a
    /// lost frame. Callers that want it abandoned call
    /// [`abort_capture`](Self::abort_capture) first.
    ///
    /// # Errors
    /// Transport-level failures only; disconnecting a disconnected camera is `Ok(())`.
    async fn disconnect(&self) -> Result<(), DeviceError>;

    /// The settings currently in force, read from the camera rather than remembered.
    ///
    /// # Errors
    /// `NotConnected`, `Timeout` (the settings tree is a slow read on some bodies — SDD §5.3.1
    /// bounds it at `camera.timeouts.config_seconds`), `Protocol`.
    async fn settings(&self) -> Result<CameraSettings, DeviceError>;

    /// Every value this body will accept, in the camera's own order (PRD §4.1).
    ///
    /// Feeds the settings UI; a value not in these lists must be refused by the setters.
    ///
    /// # Errors
    /// As [`settings`](Self::settings).
    async fn available_settings(&self) -> Result<AvailableSettings, DeviceError>;

    /// Selects an ISO by its camera token, e.g. `"1600"`.
    ///
    /// # Errors
    /// `Rejected` if the token is not one the camera offers — never a silent substitution of the
    /// nearest value, which would produce a frame the operator did not ask for and cannot
    /// detect. `NotConnected`, `Busy` during a capture, or a transport-level failure.
    async fn set_iso(&self, iso: &str) -> Result<(), DeviceError>;

    /// Selects a shutter speed by its camera token, e.g. `"1/250"`, `"30"`, `"bulb"`.
    ///
    /// # Errors
    /// As [`set_iso`](Self::set_iso).
    async fn set_shutter(&self, shutter: &str) -> Result<(), DeviceError>;

    /// Selects an aperture by its camera token, e.g. `"5.6"`.
    ///
    /// # Errors
    /// `Unsupported` on a body with no electronic aperture control (a manual lens), otherwise as
    /// [`set_iso`](Self::set_iso).
    async fn set_aperture(&self, aperture: &str) -> Result<(), DeviceError>;

    /// Selects the capture format.
    ///
    /// The driver maps [`ImageFormat`] onto whatever the body calls it. Formats outside
    /// [`CameraCapabilities::supported_formats`](astroctl_core::types::CameraCapabilities) are
    /// `Unsupported`.
    ///
    /// # Errors
    /// `Unsupported`, `NotConnected`, `Busy`, or a transport-level failure.
    async fn set_image_format(&self, format: ImageFormat) -> Result<(), DeviceError>;

    /// Takes one exposure at the current settings and resolves when the files are durable.
    ///
    /// **Durable means durable.** Every path in the result is a complete file: written under a
    /// temporary name, fsynced, renamed into place, with the directory fsynced (SDD §5.3.2).
    /// Consumers — the preview pipeline, the transfer agent, the operator's file browser — only
    /// ever see finished frames, which is what makes a torn download invisible instead of
    /// corrupting a session (REL-05, REL-11). A driver that skips the rename dance is not
    /// slightly less safe, it is a different design.
    ///
    /// Long by nature: exposure time plus download, seconds to minutes. It occupies the camera
    /// for that whole period — a second concurrent capture is `Busy`, and live view pauses (see
    /// the module docs). Dropping the future does **not** abort the exposure (rule 3);
    /// [`abort_capture`](Self::abort_capture) does.
    ///
    /// # Errors
    /// `Busy` if a capture is already running, `NotConnected`, `Timeout` if the camera exceeds
    /// its operation-class budget — which SDD §5.3.1 treats as a wedged camera, so the driver
    /// then rebuilds its context rather than pretending the next call will work — `Rejected` if
    /// the camera refuses (no card, autofocus failure), `Transport` if a write fails or the
    /// device disappears mid-download.
    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, DeviceError>;

    /// Takes one bulb exposure of `duration` (CAM-04) and otherwise behaves exactly like
    /// [`capture`](Self::capture).
    ///
    /// Separate from `capture` because the mechanism is different — a press/hold/release
    /// sequence the driver times itself rather than a shutter setting (SDD §5.3.2) — and because
    /// a body can support one and not the other.
    ///
    /// # Errors
    /// `Unsupported` if [`CameraCapabilities::has_bulb`](astroctl_core::types::CameraCapabilities)
    /// is false, otherwise as [`capture`](Self::capture).
    async fn capture_bulb(
        &self,
        request: &CaptureRequest,
        duration: Duration,
    ) -> Result<CaptureResult, DeviceError>;

    /// Abandons the capture in flight.
    ///
    /// Idempotent: aborting when nothing is running is `Ok(())`. It is a stopping command, so it
    /// is never staleness-rejected (SDD §5.8.1) and must be attempted even if the camera looks
    /// busy. The abandoned [`capture`](Self::capture) call resolves with
    /// [`DeviceError::Rejected`](astroctl_core::error::DeviceError::Rejected) — the caller that
    /// asked for the abort is the one holding that future, so it knows what the rejection means.
    /// Any partial file must be removed before this resolves; a stale temporary is what makes
    /// the *next* capture fail (SDD §5.3.2, measured).
    ///
    /// # Errors
    /// Transport-level failures only.
    async fn abort_capture(&self) -> Result<(), DeviceError>;

    /// One live-view frame now (PRD §4.1 `get_live_view_frame`).
    ///
    /// For a one-shot preview — a focus check — without starting a stream.
    ///
    /// # Errors
    /// `Unsupported` if [`CameraCapabilities::has_live_view`](astroctl_core::types::CameraCapabilities)
    /// is false, `Busy` during a capture, `NotConnected`, or a transport-level failure.
    async fn live_view_frame(&self) -> Result<LiveViewFrame, DeviceError>;

    /// Starts live view if it is not already running and returns a subscription (CAM-05).
    ///
    /// Idempotent: calling it twice yields two cursors on one stream, not two streams — the
    /// camera has one imaging path. The stream ends when
    /// [`stop_live_view`](Self::stop_live_view) or [`disconnect`](Self::disconnect) is called.
    ///
    /// **Gaps are normal.** Frames stop for the duration of every capture and resume afterwards
    /// (SDD §5.7). A consumer must not treat a gap as a fault or attempt to reconnect; the
    /// signal that distinguishes "busy capturing" from "wedged" is the capture progress the
    /// facade publishes, not this stream.
    ///
    /// # Errors
    /// As [`live_view_frame`](Self::live_view_frame).
    async fn live_view_stream(&self) -> Result<FrameStream<LiveViewFrame>, DeviceError>;

    /// Stops live view. Idempotent, and safe to call when it is not running.
    ///
    /// # Errors
    /// Transport-level failures only.
    async fn stop_live_view(&self) -> Result<(), DeviceError>;

    /// Battery charge and whether the body is on external power (PRD §4.1).
    ///
    /// Published as part of `camera.status` on change and every 60 s (SDD §4.3), so a driver may
    /// serve a cached reading provided it is no staler than that cadence. Battery state is how
    /// an operator finds out a multi-hour session is about to end.
    ///
    /// # Errors
    /// `NotConnected`, or a transport-level failure. A body that reports no battery (tethered,
    /// mains-powered) reports `percent: 100, charging: true` rather than erroring — the UI needs
    /// a value.
    async fn battery(&self) -> Result<BatteryStatus, DeviceError>;

    /// Free and total space on the camera's card.
    ///
    /// # Errors
    /// `NotConnected`, or a transport-level failure.
    async fn storage(&self) -> Result<StorageInfo, DeviceError>;

    /// Sensor temperature in degrees Celsius, or `None` on a body that does not report one.
    ///
    /// `None` is the routine answer for a DSLR, not an error: it is a permanent property of the
    /// body, and making callers handle `Unsupported` for a value they merely display would put
    /// an error path in front of a normal state. Reserve the error for a camera that *should*
    /// answer and did not.
    ///
    /// # Errors
    /// `NotConnected`, or a transport-level failure.
    async fn sensor_temperature_celsius(&self) -> Result<Option<f64>, DeviceError>;

    /// What this camera can do (HAL-05).
    ///
    /// Synchronous, no I/O, callable while disconnected — as on
    /// [`MountDevice::capabilities`](crate::mount::MountDevice::capabilities).
    fn capabilities(&self) -> CameraCapabilities;

    /// Human-readable identity for logs, the UI and calibration-profile tagging (HAL-06).
    ///
    /// Synchronous, no I/O, callable while disconnected. The serial number matters here beyond
    /// display: calibration masters are matched to the body that shot them (Phase 2b), and two
    /// identical models are only distinguishable by it.
    fn device_info(&self) -> DeviceInfo;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astroctl_core::types::ImageFormat;

    use super::{Camera, CaptureRequest, CapturedFileKind};
    use crate::test_double::FakeCamera;

    #[test]
    fn a_request_names_files_the_frame_store_can_predict() {
        let request = CaptureRequest::new("/sessions/2026-07-29/frames", "light_00042");
        assert_eq!(
            request.path_with_extension("cr3"),
            std::path::Path::new("/sessions/2026-07-29/frames/light_00042.cr3")
        );
        // The two-file case: same stem, two extensions, one exposure.
        assert_ne!(
            request.path_with_extension("fits"),
            request.path_with_extension("jpg")
        );
    }

    #[tokio::test]
    async fn a_capture_returns_every_file_it_wrote_inside_the_requested_directory() {
        let camera = Arc::new(FakeCamera::new()) as Arc<dyn Camera>;
        camera.connect().await.expect("connects");
        camera
            .set_image_format(ImageFormat::RawPlusJpeg)
            .await
            .expect("format is supported");

        let request = CaptureRequest::new("/tmp/astroctl-test/frames", "light_00007");
        let result = camera.capture(&request).await.expect("captures");

        let raw = result.raw().expect("a raw file");
        assert_eq!(raw.kind, CapturedFileKind::Raw);
        assert!(raw.path.starts_with(&request.dir), "{:?}", raw.path);
        let jpeg = result.jpeg().expect("RAW+JPEG produced a JPEG too");
        assert!(jpeg.path.starts_with(&request.dir));
        assert_eq!(result.total_bytes(), raw.size_bytes + jpeg.size_bytes);
        // The result reports what the camera was actually set to, not what was asked for.
        assert_eq!(result.settings.format, ImageFormat::RawPlusJpeg);
    }

    #[tokio::test]
    async fn a_second_capture_is_busy_rather_than_a_second_exposure() {
        let camera = Arc::new(FakeCamera::new());
        camera.connect().await.expect("connects");
        camera.hold_capture(true);

        let first = {
            let camera = Arc::clone(&camera);
            tokio::spawn(async move {
                camera
                    .capture(&CaptureRequest::new("/tmp/astroctl-test", "light_1"))
                    .await
            })
        };
        // Let the first capture take the camera.
        while !camera.is_capturing() {
            tokio::task::yield_now().await;
        }

        let second = camera
            .capture(&CaptureRequest::new("/tmp/astroctl-test", "light_2"))
            .await;
        assert!(
            matches!(second, Err(astroctl_core::error::DeviceError::Busy(_))),
            "got {second:?}"
        );

        camera.hold_capture(false);
        first.await.expect("task").expect("first capture completes");
    }

    #[tokio::test]
    async fn live_view_is_one_stream_however_many_subscribers() {
        let camera = Arc::new(FakeCamera::new()) as Arc<dyn Camera>;
        camera.connect().await.expect("connects");

        let mut a = camera.live_view_stream().await.expect("starts");
        let mut b = camera
            .live_view_stream()
            .await
            .expect("joins the same stream");
        camera
            .live_view_frame()
            .await
            .expect("one-shot frame works");

        let (first, second) = tokio::join!(a.next_frame(), b.next_frame());
        assert_eq!(
            first.expect("frame").jpeg,
            second.expect("frame").jpeg,
            "two subscribers must see the same stream, not two acquisitions"
        );

        camera.stop_live_view().await.expect("stops");
        assert!(
            a.next_frame().await.is_none(),
            "stopping live view ends the stream"
        );
    }
}
