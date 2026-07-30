//! The camera facade and the capture flow — SDD §5.3.2, §5.6, §4.3's three camera topics.
//!
//! The second vertical slice, and the first one that writes to disk: HTTP in, `Arc<dyn Camera>` at
//! the bottom, a durable frame and four events out. Where the mount facade (M1-T03) owns motion,
//! this owns the exposure and the frame it produces.
//!
//! # The flow, and which crate owns each step
//!
//! ```text
//!  route │ claim the orchestrator (§5.6)        ← 409 BUSY if one is running or a fault is held
//!        │ read settings, resolve the exposure  ← 422 if the shutter is `bulb` and no duration came
//!        │ disk gate                            ← 507 DISK_FULL, before the camera is touched
//!        │ reserve_frame_id                     ← astroctl-session (T07); persisted before granted
//!        │ 202 {correlation_id, frame_id}
//!   task │ capture.progress: exposing …         ← 1 Hz, so a 300 s bulb has a countdown
//!        │ Camera::capture / capture_bulb       ← astroctl-drivers (T06); its own OS thread
//!        │ capture.progress: downloading
//!        │ begin_frame → rename → commit_frame  ← astroctl-session; durable when commit returns
//!        │ capture.progress: saved + frame.saved
//!        │ write_quality                        ← the sidecar, after the frame (SDD §5.5 note 3)
//! ```
//!
//! # Two handoffs that do not quite meet, and what this file does about it
//!
//! **1. The frame's bytes are written twice-owned.** SDD §5.3.2 has the driver download *into*
//! `<session>/frames/.tmp_<id>`, and [`astroctl_session::StagedFrame`] is built for exactly that —
//! its docs require a driver to "write *into* this path rather than replace it". But the HAL's
//! [`CaptureRequest`](astroctl_hal::camera::CaptureRequest) carries a **directory and a stem**, and
//! the driver appends its own extension and does its own temp-fsync-rename (HAL-03: "a driver that
//! skips the rename dance is not slightly less safe, it is a different design"). There is no way to
//! hand a driver the staged file. Two layers own the durability dance and neither can be told to
//! stop.
//!
//! This file bridges it by capturing into a scratch directory the session owns and **renaming** the
//! science file onto the staged path. The rename is free — same filesystem — and every durability
//! property survives it, which is the only reason it is acceptable:
//!
//! * the frame's *bytes* are already fsynced, by the driver, before `capture` returns (HAL-03);
//! * the frame's *name* is made durable by `commit_frame`, which fsyncs `frames/` after its own
//!   rename.
//!
//! What is lost is `commit_frame`'s fsync of the handle it holds, which after the rename points at
//! an orphaned empty inode. That fsync is not what makes this frame durable — the driver's was —
//! so the invariant the store's rule protects still holds, by other means. The *rule* is
//! nonetheless being broken, and the honest fix is in the HAL rather than here: `CaptureRequest`
//! should carry a destination path. That is an M2 change, because it is `libgphoto2`'s
//! `download_to` that the shape was drawn for and M2 is when that driver arrives (recorded in
//! SDD §5.3.2).
//!
//! **2. The extension is the driver's, so the frame store cannot be gated on it.**
//! `begin_frame(id, ext)` needs the extension to name the destination, and only the driver knows
//! whether this body produces `.cr3`, `.fits` or both. So `begin_frame` cannot run before the
//! capture — which is why the disk refusal REL-12 requires is a *separate*, explicit read of the
//! disk in the route. That is not duplication: it is T05's `check_goto` shape. Asking before the
//! `202` is what makes `507 DISK_FULL` the answer to the operator's request instead of an alert
//! thirty seconds later; `begin_frame` is still the thing that enforces it.
//!
//! # Why the exposure→download transition is inferred rather than observed
//!
//! [`Camera::capture`] is one await with no progress callback, so nothing outside the driver can
//! see the shutter close. The ticker below therefore switches to `downloading` when the *requested*
//! exposure elapses. That is exact for a simulated body and right to within the shutter's own
//! accuracy for a real one, and it is the only honest option the HAL leaves: the alternative is to
//! publish nothing between `exposing` and `saved`, which is a two-second-to-five-minute silence in
//! the one widget that tells the operator the camera is alive (SDD §5.9).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use astroctl_core::bus::EventBus;
use astroctl_core::error::{ApiError, DeviceError, ErrorCode};
use astroctl_core::event::{self, CaptureProgress, FrameSaved};
use astroctl_core::types::{CameraSettings, DeviceKind, ImageFormat};
use astroctl_hal::camera::{Camera, CaptureRequest, CaptureResult};
use astroctl_session::{
    CaptureParams, DiskLevel, FrameId, FrameKind, FrameStore, Session, SessionView, StoreError,
};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiFailure, AppState};
use crate::orchestrator::{self, Fault, Orchestrator, Outcome};

/// Schema version of the settings body (SDD §2).
const SETTINGS_SCHEMA_VERSION: u16 = 1;

/// How often `capture.progress` reports an exposure in flight.
///
/// One second, which is `mount.position`'s cadence and for the same reason: it is the rate at which
/// a number on a screen reads as *live* rather than as a frozen panel. A 30 s exposure gets 30
/// updates and a 300 s bulb gets 300, which is what CAM-04's countdown is.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// How often `camera.status` is republished with nothing having changed (SDD §4.3).
const STATUS_INTERVAL: Duration = Duration::from_secs(60);

/// Where a capture's files land before they are handed to the frame store.
///
/// Inside the session so the rename onto the staged path never crosses a filesystem, and dot-
/// prefixed so that a stray file left by a crash is obviously not a frame. It is deliberately *not*
/// `frames/`: [`SessionView`] lists that directory, and a scratch file named after its frame id
/// would appear in the operator's frame list as a frame that had not been committed.
const CAPTURE_STAGING: &str = ".capture";

// ---------------------------------------------------------------------------------------------
// The facade
// ---------------------------------------------------------------------------------------------

/// Everything the camera routes, the capture task and the status poll share.
#[derive(Debug)]
pub struct CameraFacade {
    /// The driver, behind the HAL (SDD §5.1). Unlike the mount there is no safety wrapper: nothing
    /// a camera does can move a telescope.
    device: Arc<dyn Camera>,
    /// The frame store (T07). Held rather than a bare `Arc<Session>` so that the session the node
    /// is writing into is always the one `CURRENT` names, not a handle captured at startup.
    store: Arc<FrameStore>,
    bus: EventBus,
    /// The FSM of SDD §5.6.
    ///
    /// A `std` mutex, and that is load-bearing rather than an optimisation: [`CaptureClaim`] gives
    /// the slot back from `Drop`, which cannot `.await`. Nothing is awaited under this lock —
    /// `clippy::await_holding_lock` is denied workspace-wide and would catch it if it were.
    orchestrator: std::sync::Mutex<Orchestrator>,
    /// The task running the capture, so shutdown can wait for it and then release its bus handle.
    inflight: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The last `camera.status` published, so the 60 s republish does not drown the discrete
    /// events the hub may not drop (SDD §4.3, §5.8.3).
    last_status: std::sync::Mutex<Option<event::CameraStatus>>,
}

/// What `POST /api/camera/capture` was asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureCommand {
    /// A bulb exposure of this many seconds (CAM-04), or `None` for the configured shutter.
    pub bulb_seconds: Option<f64>,
}

/// What the node answers a capture request with.
#[derive(Debug, Clone)]
pub struct Accepted {
    /// Ties this request to its events and its log lines.
    pub correlation_id: String,
    /// The id the frame will have, so a client can filter `capture.progress` to its own request
    /// without waiting for the first event to tell it.
    pub frame_id: String,
    /// The exposure the node resolved, in seconds — a bulb duration, or the shutter token parsed.
    pub exposure_s: f64,
}

impl CameraFacade {
    /// Wrap a driver and a frame store.
    #[must_use]
    pub fn new(device: Arc<dyn Camera>, store: Arc<FrameStore>, bus: EventBus) -> Self {
        Self {
            device,
            store,
            bus,
            orchestrator: std::sync::Mutex::new(Orchestrator::new()),
            inflight: std::sync::Mutex::new(None),
            last_status: std::sync::Mutex::new(None),
        }
    }

    fn fsm(&self) -> std::sync::MutexGuard<'_, Orchestrator> {
        self.orchestrator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether a capture is in flight.
    ///
    /// `#[cfg(test)]`, and that is a statement about the API rather than about the tests: the FSM's
    /// state reaches a client through the `409` a capture is refused with and the `alert` a fault
    /// raises, not through a route of its own. SDD §5.8.1 has no camera-state row and §4.3 has no
    /// topic for it, so adding either here would be inventing wire surface. See the note in
    /// `finish` for what that costs the panel.
    #[cfg(test)]
    #[must_use]
    pub fn is_capturing(&self) -> bool {
        self.fsm().is_capturing()
    }

    /// The fault an operator has to acknowledge, if there is one. See [`is_capturing`] on why this
    /// is not a route.
    #[cfg(test)]
    #[must_use]
    pub fn fault(&self) -> Option<Fault> {
        self.fsm().fault().cloned()
    }

    /// Clear a fault (`POST /api/camera/fault/ack`), returning what was cleared.
    ///
    /// Publishes an `alert` rather than answering with the new state alone, because SDD §5.9
    /// forbids the UI rendering a state change from its own request: the panel learns that capture
    /// is available again from the event stream like everything else.
    pub fn acknowledge_fault(&self) -> Option<Fault> {
        let cleared = self.fsm().acknowledge();
        if let Some(fault) = &cleared {
            tracing::info!(code = fault.code.as_str(), "capture fault acknowledged");
            self.bus.publish(event::Alert::info(
                "CAPTURE_FAULT_CLEARED",
                format!(
                    "the capture fault ({}) was acknowledged; capture is available again",
                    fault.code.as_str()
                ),
            ));
        }
        cleared
    }

    /// The active session, or the failure to report when there is not one.
    ///
    /// There always is one on a started node — SDD §8.1 opens or creates it before the API comes
    /// up — so this is `INTERNAL` rather than `NOT_FOUND`: a node serving requests without a
    /// session has a startup bug, and telling the operator "no session" would send them looking
    /// for something to create.
    fn session(&self) -> Result<Arc<Session>, ApiError> {
        self.store.current().ok_or_else(|| {
            ApiError::new(
                ErrorCode::Internal,
                "this node has no open session; it should have opened one at startup (SDD §8.1)",
            )
        })
    }

    /// `GET /api/session/current` — SDD §5.8.1's "session.json view + frame list".
    pub async fn session_view(&self) -> Result<SessionView, ApiError> {
        let session = self.session()?;
        session.view().await.map_err(store_failure)
    }

    /// Start one capture: the whole of the route's synchronous half.
    ///
    /// Every refusal it can see is answered here, before the `202` — T05's rule, because `202`
    /// means the work began and an operator who gets it followed by an alert has been told the
    /// opposite of what happened.
    pub async fn start_capture(
        self: &Arc<Self>,
        command: CaptureCommand,
    ) -> Result<Accepted, ApiError> {
        let correlation_id = correlation_id()?;

        // Claimed first, and before anything that costs an fsync. Two requests a microsecond apart
        // must not both pass the "is it idle" test, and the loser must not have burned a frame id
        // on its way to a 409.
        let claim = CaptureClaim::take(self.as_ref(), &correlation_id)?;

        let session = self.session()?;

        // REL-12, and it comes *first* among the checks — before the camera is asked anything at
        // all. Two reasons, and both are about which answer the operator gets:
        //
        // * §5.8.1's `202` means the work began. The store's own gate (`begin_frame`, SDD §5.5
        //   note 7) runs inside the spawned task, long after the answer went out, so a capture
        //   refused there reaches the operator as an alert instead of as the answer to what they
        //   asked. This is T05's `check_goto` shape: ask before answering, enforce below.
        // * A full disk is the condition that would *destroy* the frame rather than merely fail to
        //   take it, and it is a `statvfs` rather than a device round trip. Refusing on the
        //   cheapest and most destructive condition first is the ordering that costs nothing.
        //
        // The consequence worth naming: on a node whose disk is critical *and* whose camera is
        // unplugged, this answers `507` rather than `409 NOT_CONNECTED`. That is the right one —
        // plugging the camera in would not make the capture possible.
        let critical_gb = self.store.thresholds().critical_gb;
        if let Some(disk) = session.disk_status().await {
            if disk.level == DiskLevel::Critical {
                return Err(ApiError::new(
                    ErrorCode::DiskFull,
                    format!(
                        "only {:.1} GB free under the session directory, below the critical \
                         threshold of {critical_gb:.1} GB; free space before capturing",
                        disk.free_gb
                    ),
                )
                .with_detail(serde_json::json!({
                    "free_gb": disk.free_gb,
                    "critical_gb": critical_gb,
                })));
            }
        }
        // Free space that cannot be *determined* is not a refusal (SDD §5.5 note 7): losing frames
        // to a failed `statvfs` is the outcome REL-05 forbids.

        // Doubles as the connection check: a camera nobody has connected answers `NotConnected`,
        // which is the 409 the operator needs rather than a `202` for an exposure that will never
        // start.
        let settings = self
            .device
            .settings()
            .await
            .map_err(|e| camera_failure(&e))?;

        let exposure = self.resolve_exposure(&settings, command)?;
        self.fsm().set_exposure(&correlation_id, exposure);

        let frame_id = session
            .reserve_frame_id(FrameKind::Light)
            .await
            .map_err(store_failure)?;

        // The clock starts here — see `Orchestrator::arm`.
        self.fsm().arm(&correlation_id, frame_id);

        let facade = Arc::clone(self);
        let id_for_task = correlation_id.clone();
        let bulb = command.bulb_seconds.map(Duration::from_secs_f64);
        let task = tokio::spawn(async move {
            run_capture(facade, id_for_task, session, frame_id, exposure, bulb).await;
        });
        self.track_inflight(task);

        claim.commit();
        Ok(Accepted {
            correlation_id,
            frame_id: frame_id.to_string(),
            exposure_s: exposure.as_secs_f64(),
        })
    }

    /// Turn the request and the camera's settings into one exposure, or refuse.
    ///
    /// Both refusals exist because of a measured property of the reference body (SDD §5.3.2): with
    /// the mode dial on Bulb the API can only offer `bulb`, so "the shutter is set to a value
    /// `capture` cannot use" is a state an operator reaches by turning a dial, not a programming
    /// error. The driver refuses it too — this asks first, so the refusal is the answer.
    fn resolve_exposure(
        &self,
        settings: &CameraSettings,
        command: CaptureCommand,
    ) -> Result<Duration, ApiError> {
        match command.bulb_seconds {
            Some(seconds) => {
                if !seconds.is_finite() || seconds <= 0.0 {
                    return Err(ApiError::new(
                        ErrorCode::Validation,
                        "`bulb_seconds` must be a positive number of seconds",
                    ));
                }
                if !self.device.capabilities().has_bulb {
                    return Err(ApiError::new(
                        ErrorCode::Unsupported,
                        "this camera has no bulb mode; capture at one of its shutter speeds \
                         instead",
                    ));
                }
                Ok(Duration::from_secs_f64(seconds))
            }
            None => shutter_seconds(&settings.shutter)
                .map(Duration::from_secs_f64)
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::DeviceRejected,
                        format!(
                            "the shutter is set to `{}`, which needs a duration; send \
                             `bulb_seconds` or select a timed shutter speed",
                            settings.shutter
                        ),
                    )
                }),
        }
    }

    /// Abandon the capture in flight (`POST /api/camera/capture/abort`).
    ///
    /// A stopping command: never refused for state, never staleness-rejected (SDD §5.8.1). It does
    /// not touch the FSM — the capture task owns that transition and will make it when its own
    /// future resolves, which is what keeps "the node thinks it is idle" and "the camera is still
    /// exposing" from ever being true at once.
    pub async fn abort_capture(&self) -> Result<(), DeviceError> {
        self.device.abort_capture().await
    }

    /// Remember the capture task, so shutdown can wait for it.
    fn track_inflight(&self, handle: tokio::task::JoinHandle<()>) {
        let mut slot = self
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(handle);
    }

    /// SDD §7 step 3: "finish an in-flight download (bounded)".
    ///
    /// Waiting rather than aborting is the whole point, and it is the one place this node
    /// deliberately delays its own shutdown. A half-downloaded frame is a lost frame (SDD §7), and
    /// the exposure has already been spent — so the node pays up to `budget` to keep it. Past that
    /// it gives up, because an operator power-cycling a Pi that will not die is worse than one lost
    /// frame.
    ///
    /// Either way the task is gone when this returns, which is the other thing shutdown needs: the
    /// task holds an [`EventBus`] handle, and a surviving sender stalls the session log's flush for
    /// its whole timeout (the M1-T03 follow-up made that a hard rule).
    pub async fn finish_inflight(&self, budget: Duration) {
        let handle = self
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(mut handle) = handle else {
            return;
        };

        if tokio::time::timeout(budget, &mut handle).await.is_err() {
            tracing::warn!(
                budget_s = budget.as_secs(),
                "a capture was still running at shutdown and did not finish in time; abandoning it"
            );
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Read the camera's vitals and publish `camera.status` if they changed.
    ///
    /// Returns what was published, so a route that has just connected can answer with the same
    /// value the event carried instead of reading the camera twice.
    pub async fn publish_status(&self, force: bool) -> event::CameraStatus {
        let status = match (self.device.battery().await, self.device.storage().await) {
            (Ok(battery), Ok(storage)) => {
                event::CameraStatus::connected(battery.percent, battery.charging, storage.free_mb)
            }
            // Either read failing means the camera is not answering, and §4.3 is explicit that the
            // battery and storage fields are `null` rather than `0` in that case: a zeroed battery
            // renders as an empty gauge, which is a lie the operator would act on.
            _ => event::CameraStatus::disconnected(),
        };

        let changed = {
            let mut last = self
                .last_status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let changed = last.as_ref() != Some(&status);
            *last = Some(status);
            changed
        };

        if changed || force {
            self.bus.publish(status);
        }
        status
    }
}

// ---------------------------------------------------------------------------------------------
// The claim
// ---------------------------------------------------------------------------------------------

/// Holds the orchestrator for one capture, and gives it back if the request never got started.
///
/// A guard rather than a `match` with cleanup on every arm, because the route between the claim and
/// the spawn has five ways to fail and four of them were added after the first. The guard makes
/// "the slot is released on the error paths" a property of the type instead of a thing to remember.
struct CaptureClaim<'a> {
    facade: &'a CameraFacade,
    correlation_id: String,
    committed: bool,
}

impl<'a> CaptureClaim<'a> {
    fn take(facade: &'a CameraFacade, correlation_id: &str) -> Result<Self, ApiError> {
        // Zero for now: the exposure is not known until the settings come back, and the claim has
        // to be taken before anything is asked of the camera. `Orchestrator::set_exposure` fills
        // it in a few lines later.
        facade.fsm().claim(correlation_id, Duration::ZERO)?;
        Ok(Self {
            facade,
            correlation_id: correlation_id.to_owned(),
            committed: false,
        })
    }

    /// The capture is spawned; the task owns the slot from here.
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for CaptureClaim<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.facade
                .fsm()
                .finish(&self.correlation_id, Outcome::Ended);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The capture flow (SDD §5.3.2)
// ---------------------------------------------------------------------------------------------

/// One capture, from shutter-open to durable frame. Runs on its own task; never returns an error,
/// because there is nobody left to return one to — every failure is an `alert` and an FSM
/// transition.
async fn run_capture(
    facade: Arc<CameraFacade>,
    correlation_id: String,
    session: Arc<Session>,
    frame_id: FrameId,
    exposure: Duration,
    bulb: Option<Duration>,
) {
    let frame = frame_id.to_string();
    let staging = session.dir().join(CAPTURE_STAGING);
    if let Err(error) = tokio::fs::create_dir_all(&staging).await {
        // The driver does not create its own output directory (HAL-03), so this is the one setup
        // step the facade owes it.
        return fail(
            &facade,
            &correlation_id,
            &frame,
            Outcome::Faulted {
                code: ErrorCode::Internal,
                message: format!(
                    "cannot create the capture staging directory {}: {error}",
                    staging.display()
                ),
            },
        );
    }

    let request = CaptureRequest::new(&staging, &frame);
    let outcome = expose(&facade, &correlation_id, &frame, &request, exposure, bulb).await;

    let result = match outcome {
        Ok(result) => result,
        Err(error) => {
            // Nothing to clean up on the abort path — the driver leaves nothing on disk when it
            // gives up before the write (T06), and when it loses the race to the write it returns
            // `Ok` instead. Any other failure may have left partial output; sweep it either way.
            sweep_staging(&staging, &frame).await;
            tracing::warn!(correlation_id = %correlation_id, frame = %frame, %error, "capture failed");
            facade.bus.publish(event::Alert::warning(
                ErrorCode::from_device_error(DeviceKind::Camera, &error).as_str(),
                error.to_string(),
            ));
            return finish(&facade, &correlation_id, orchestrator::classify(&error));
        }
    };

    match store_frame(&facade, &session, frame_id, &result).await {
        Ok(()) => finish(&facade, &correlation_id, Outcome::Saved),
        Err(outcome) => {
            sweep_staging(&staging, &frame).await;
            finish(&facade, &correlation_id, outcome);
        }
    }
}

/// Run the exposure while `capture.progress` reports it.
///
/// The ticker and the capture share one task rather than the capture spawning a reporter, so there
/// is exactly one `EventBus` handle to account for at shutdown and no way for the reporter to
/// outlive what it is reporting on.
async fn expose(
    facade: &CameraFacade,
    correlation_id: &str,
    frame: &str,
    request: &CaptureRequest,
    exposure: Duration,
    bulb: Option<Duration>,
) -> Result<CaptureResult, DeviceError> {
    let started = std::time::Instant::now();
    facade.bus.publish(CaptureProgress::exposing(frame, 0.0));

    let device = Arc::clone(&facade.device);
    let capture = async move {
        match bulb {
            Some(duration) => device.capture_bulb(request, duration).await,
            None => device.capture(request).await,
        }
    };
    tokio::pin!(capture);

    let shutter_closes = tokio::time::sleep(exposure);
    tokio::pin!(shutter_closes);

    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    // The first tick of a tokio interval fires immediately, and `exposing(0.0)` has already gone
    // out above.
    ticker.tick().await;

    let mut downloading = false;
    let result = loop {
        tokio::select! {
            outcome = &mut capture => break outcome,
            () = &mut shutter_closes, if !downloading => {
                downloading = true;
                facade.bus.publish(CaptureProgress::downloading(
                    frame,
                    started.elapsed().as_secs_f64(),
                ));
            }
            _ = ticker.tick(), if !downloading => {
                // CAM-04's countdown: the panel subtracts `elapsed_s` from the exposure it was
                // told in the 202. Publishing the elapsed time rather than the remaining one keeps
                // this payload the same shape for a timed exposure and a bulb.
                facade.bus.publish(CaptureProgress::exposing(
                    frame,
                    started.elapsed().as_secs_f64(),
                ));
            }
        }
    };

    if result.is_ok() && !downloading {
        // A capture shorter than its own exposure — a fast shutter, or a driver that resolved on
        // the same instant the timer did. The stage still happened, so the operator's panel must
        // still see it: the acceptance criterion is the *sequence*, and a race must not be able to
        // drop a state out of it.
        facade.bus.publish(CaptureProgress::downloading(
            frame,
            started.elapsed().as_secs_f64(),
        ));
    }

    tracing::debug!(
        correlation_id = %correlation_id,
        frame = %frame,
        elapsed_ms = started.elapsed().as_millis() as u64,
        ok = result.is_ok(),
        "exposure finished"
    );
    result
}

/// Hand the captured file to the frame store and publish what that produced.
async fn store_frame(
    facade: &CameraFacade,
    session: &Session,
    frame_id: FrameId,
    result: &CaptureResult,
) -> Result<(), Outcome> {
    let frame = frame_id.to_string();

    // The science file if there is one, otherwise whatever the exposure did produce: an operator
    // who selected `JPEG` gets a JPEG frame rather than an error saying there is no raw.
    let Some(captured) = result.raw().or_else(|| result.jpeg()) else {
        return Err(Outcome::Faulted {
            code: ErrorCode::Internal,
            message: "the camera reported a successful capture with no files".to_owned(),
        });
    };
    let extension = captured
        .path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("raw")
        .to_owned();

    let staged = session
        .begin_frame(frame_id, &extension)
        .await
        .map_err(|error| store_outcome(&error))?;
    let staged_path = staged.path().to_owned();

    // The bridge described in the module docs. A rename, not a copy: the frame is 48 MB from the
    // simulator and 32 MB from the reference body, and reading it back to write it again is an
    // extra 64-96 MB through a Pi's SD card per exposure.
    if let Err(error) = tokio::fs::rename(&captured.path, &staged_path).await {
        return Err(Outcome::Faulted {
            code: ErrorCode::Internal,
            message: format!(
                "cannot move the captured frame into the session: {} → {}: {error}",
                captured.path.display(),
                staged_path.display()
            ),
        });
    }

    // Everything the exposure produced besides the frame itself. M1 keeps one file per frame id;
    // the camera JPEG is not the authoritative frame (HAL-03) and the preview the PWA shows is
    // generated from the raw by the pipeline (SDD §5.7, M1-T09), so keeping it would be a second
    // copy of the same exposure that nothing reads.
    for extra in &result.files {
        if extra.path != captured.path {
            let _ = tokio::fs::remove_file(&extra.path).await;
        }
    }

    let saved = session
        .commit_frame(staged)
        .await
        .map_err(|error| store_outcome(&error))?;

    // `frame.saved` belongs here and not after the sidecar. §4.3 defines the event as "a frame is
    // durable on the field node's disk" and §5.10 has the transfer agent treat it as proof of
    // exactly that — which `commit_frame` returning is. SDD §5.3.2's older ordering puts it after
    // the metadata write; that predates T07 splitting the two, and §5.5 note 3 settles it the other
    // way: the frame is durable before its metadata exists, and a frame with no sidecar is still a
    // frame (REL-05).
    facade.bus.publish(CaptureProgress::saved(
        &frame,
        result.exposure.as_secs_f64(),
    ));
    facade.bus.publish(FrameSaved::new(
        &frame,
        &saved.path,
        saved.size_bytes,
        // Never recomputed. `commit_frame` hashed the file on the blocking pool on its way to
        // linking it (T07), and a second SHA-256 of 48 MB is ~800 ms of a Pi's CPU for a number
        // that is already in hand.
        &saved.sha256,
    ));
    tracing::info!(
        frame = %frame,
        path = %saved.path.display(),
        size_bytes = saved.size_bytes,
        sha256 = %saved.sha256,
        "frame saved"
    );

    let params = CaptureParams {
        started_ts: result.started_at,
        exposure_s: result.exposure.as_secs_f64(),
        settings: result.settings.clone(),
    };
    if let Err(error) = session.write_quality(&saved, &params).await {
        // Loud, and not a fault. The frame is on disk and the next capture has every chance of
        // working, so holding the whole session for an operator would cost more than the sidecar
        // is worth — but the sidecar carries the exposure parameters and they exist nowhere else
        // (SDD §5.5 note 6), so this is the one I/O failure in the flow that is `critical`.
        tracing::error!(frame = %frame, %error, "the frame's metadata sidecar could not be written");
        facade.bus.publish(event::Alert::critical(
            error.code().as_str(),
            format!(
                "frame {frame} is stored but its exposure metadata could not be written: {error}"
            ),
        ));
    }

    Ok(())
}

/// Remove anything a failed capture left in the staging directory.
///
/// Best-effort by design: the frame store's own startup sweep is the backstop (SDD §5.5), and a
/// staging directory that cannot be cleaned is not a reason to fault a node that is otherwise
/// taking frames.
async fn sweep_staging(staging: &Path, frame: &str) {
    let Ok(mut entries) = tokio::fs::read_dir(staging).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.contains(frame) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// End the run and, if it faulted, tell the operator.
fn finish(facade: &CameraFacade, correlation_id: &str, outcome: Outcome) {
    if let Outcome::Faulted { code, message } = &outcome {
        tracing::error!(correlation_id = %correlation_id, code = code.as_str(), %message, "capture faulted");
        facade
            .bus
            .publish(event::Alert::critical(code.as_str(), message.clone()));
    }
    facade.fsm().finish(correlation_id, outcome);
}

/// The setup-failure path: report and end the run in one call.
fn fail(facade: &CameraFacade, correlation_id: &str, frame: &str, outcome: Outcome) {
    tracing::error!(correlation_id = %correlation_id, frame = %frame, "capture could not start");
    finish(facade, correlation_id, outcome);
}

/// A store failure, as an FSM outcome.
///
/// The split is the same one [`orchestrator::classify`] makes for devices: `DiskFull` is an answer
/// the operator can act on and the next capture may well succeed after they have deleted something,
/// while an I/O or JSON failure means the session directory is not behaving and the node should
/// stop rather than write more frames into it.
fn store_outcome(error: &StoreError) -> Outcome {
    match error {
        StoreError::DiskFull { .. } | StoreError::FrameIdConflict { .. } => Outcome::Ended,
        _ => Outcome::Faulted {
            code: error.code(),
            message: error.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The 60 s status poll
// ---------------------------------------------------------------------------------------------

/// Publish `camera.status` on change and every 60 s (SDD §4.3).
///
/// Runs until aborted at shutdown, like [`crate::mount::poll`] and for the same reason: it holds an
/// [`EventBus`] handle, so it has to stop before `main` drops the rest.
pub async fn poll(facade: Arc<CameraFacade>) {
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        // `force`, because §4.3 gives this topic a floor cadence as well as a change trigger: a
        // client that has been connected for an hour should not have to wonder whether the battery
        // reading is an hour old or simply unchanged.
        facade.publish_status(true).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Turn a shutter token into seconds, or `None` for `bulb`.
///
/// A second copy of the driver's own parser, and deliberately so: this one runs *above* the HAL, on
/// tokens that came back from `settings()`, and the trait has no method that would let a facade ask
/// "how long is this shutter". Making it a HAL method would put a Canon spelling convention in the
/// abstraction; leaving it here keeps the guess where its consequences are (a wrong answer moves the
/// progress transition, not the exposure).
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
    token.trim().parse().ok()
}

/// A driver failure as the API envelope, named as the camera's.
///
/// `CAMERA_TIMEOUT` rather than the device-agnostic `DEVICE_TIMEOUT` is the difference between
/// telling the operator to check the camera and telling them to check "a device".
pub fn camera_failure(error: &DeviceError) -> ApiError {
    ApiError::from_device_error(DeviceKind::Camera, error)
}

/// A frame-store failure as the API envelope, keeping the store's own status.
///
/// `StoreError::code()` is what makes `DISK_FULL` a 507 here and a 507 on the stack node's ingest:
/// one code, one status, wherever it is raised (SDD §4.2).
pub fn store_failure(error: StoreError) -> ApiError {
    ApiError::new(error.code(), error.to_string())
}

/// A correlation id for a capture, from the same generator the mount's goto uses.
fn correlation_id() -> Result<String, ApiError> {
    crate::ticket::Ticket::generate()
        .map(|t| t.as_str().to_owned())
        .map_err(|e| {
            ApiError::new(
                ErrorCode::Internal,
                format!("could not generate a correlation id: {e}"),
            )
        })
}

// ---------------------------------------------------------------------------------------------
// Request and response bodies
// ---------------------------------------------------------------------------------------------

/// `POST /api/camera/connect` / `disconnect` — SDD §5.8.1 takes no body on either.
///
/// Declared, and empty, for the reason [`crate::mount::ConnectRequest`] is: the PWA posts `{}` and
/// a bare `POST` with no body at all is a reasonable thing for `curl` to do, and neither should be
/// a 422.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectRequest {}

/// `POST /api/camera/capture` — SDD §5.8.1's `{}` or `{bulb_seconds}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequestBody {
    /// A bulb exposure of this many seconds (CAM-04). Omitted means "use the shutter setting".
    #[serde(default)]
    pub bulb_seconds: Option<f64>,
}

/// What `POST /api/camera/capture` answers — §5.8.1's `202 + WS progress`.
#[derive(Debug, Serialize)]
pub struct CaptureAccepted {
    correlation_id: String,
    /// The id this exposure will be stored under, so a client can filter the progress stream to
    /// its own request rather than to "the most recent capture".
    frame_id: String,
    /// What the node resolved the exposure to. The panel's countdown subtracts the event's
    /// `elapsed_s` from this, which is why it is in the *answer* rather than only in the events:
    /// the first `capture.progress` may arrive a second later, and a countdown that starts blank
    /// looks like a capture that did not start.
    exposure_s: f64,
    watch_topic: &'static str,
}

/// `GET`/`PUT /api/camera/settings` — §5.8.1's `{iso, shutter, aperture, format}` + available
/// values.
///
/// The current settings are flattened rather than nested under a `current` key so the body reads
/// exactly as §5.8.1 writes it, and so a client that only wants to display the settings does not
/// have to know the shape of the availability lists to reach them.
#[derive(Debug, Serialize)]
pub struct SettingsView {
    v: u16,
    #[serde(flatten)]
    current: CameraSettings,
    available: astroctl_core::types::AvailableSettings,
}

/// `PUT /api/camera/settings` — every field optional, because the panel changes one at a time.
///
/// A partial update rather than a whole-object replace: the alternative would make changing the
/// ISO require the client to send back a shutter it read a moment ago, which over a tunnel is how
/// two operators' changes silently overwrite each other.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    #[serde(default)]
    iso: Option<String>,
    #[serde(default)]
    shutter: Option<String>,
    #[serde(default)]
    aperture: Option<String>,
    #[serde(default)]
    format: Option<ImageFormat>,
}

/// What `POST /api/camera/capture/abort` answers.
///
/// One field, and deliberately not a status: SDD §5.9 forbids the UI rendering a mutation from its
/// own request, so what the *camera* did arrives as `capture.progress` and an `alert`. What this
/// body claims is only that the node delivered the abort, which is the one thing it knows.
#[derive(Debug, Serialize)]
pub struct AbortAccepted {
    requested: bool,
}

/// What `POST /api/camera/fault/ack` answers.
#[derive(Debug, Serialize)]
pub struct FaultAcknowledged {
    /// Whether there was a fault to clear. `false` is a success: acknowledging nothing is the
    /// idempotent case, not an error.
    cleared: bool,
    /// The code that was cleared, or `null`.
    code: Option<&'static str>,
    message: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `POST /api/camera/connect` — SDD §5.8.1.
pub async fn connect(
    State(state): State<AppState>,
    body: Option<Json<ConnectRequest>>,
) -> Result<Json<event::CameraStatus>, ApiFailure> {
    let _ = body;
    state
        .camera
        .device
        .connect()
        .await
        .map_err(|e| ApiFailure(camera_failure(&e)))?;

    // Published immediately rather than waiting up to a minute for the poll: the operator pressed
    // Connect and is watching for the badge to change.
    Ok(Json(state.camera.publish_status(true).await))
}

/// `POST /api/camera/disconnect` — SDD §5.8.1.
///
/// Does **not** abort a capture in flight. HAL-03 is explicit that disconnect finishes the
/// download, because SDD §7's reasoning applies here too: a half-downloaded frame is a lost frame,
/// and the exposure has already been spent. An operator who wants it abandoned aborts first.
pub async fn disconnect(
    State(state): State<AppState>,
    body: Option<Json<ConnectRequest>>,
) -> Result<Json<event::CameraStatus>, ApiFailure> {
    let _ = body;
    state
        .camera
        .device
        .disconnect()
        .await
        .map_err(|e| ApiFailure(camera_failure(&e)))?;

    Ok(Json(state.camera.publish_status(true).await))
}

/// `GET /api/camera/settings` — SDD §5.8.1.
pub async fn settings(State(state): State<AppState>) -> Result<Json<SettingsView>, ApiFailure> {
    let device = &state.camera.device;
    let current = device
        .settings()
        .await
        .map_err(|e| ApiFailure(camera_failure(&e)))?;
    let available = device
        .available_settings()
        .await
        .map_err(|e| ApiFailure(camera_failure(&e)))?;

    Ok(Json(SettingsView {
        v: SETTINGS_SCHEMA_VERSION,
        current,
        available,
    }))
}

/// `PUT /api/camera/settings` — SDD §5.8.1.
///
/// Every value is applied through the driver, which refuses a token the body does not offer
/// (HAL-03: "never a silent substitution of the nearest value, which would produce a frame the
/// operator did not ask for and cannot detect"). The reply is **read back from the camera** rather
/// than echoed, for the same reason: a body may coerce a value, and the settings the operator sees
/// must be the ones the sensor will use.
pub async fn update_settings(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<SettingsView>, ApiFailure> {
    let device = &state.camera.device;

    // Sequential and fail-fast. A partial application is a real outcome — the third setting can be
    // refused after the first two landed — and the reply says so by reporting what is now in force
    // rather than by pretending the request was atomic.
    if let Some(iso) = &update.iso {
        device
            .set_iso(iso)
            .await
            .map_err(|e| ApiFailure(camera_failure(&e)))?;
    }
    if let Some(shutter) = &update.shutter {
        device
            .set_shutter(shutter)
            .await
            .map_err(|e| ApiFailure(camera_failure(&e)))?;
    }
    if let Some(aperture) = &update.aperture {
        device
            .set_aperture(aperture)
            .await
            .map_err(|e| ApiFailure(camera_failure(&e)))?;
    }
    if let Some(format) = update.format {
        device
            .set_image_format(format)
            .await
            .map_err(|e| ApiFailure(camera_failure(&e)))?;
    }

    settings(State(state)).await
}

/// `POST /api/camera/capture` — SDD §5.8.1's `202`, §5.3.2's flow.
pub async fn capture(
    State(state): State<AppState>,
    body: Option<Json<CaptureRequestBody>>,
) -> Result<Response, ApiFailure> {
    let request = body.map(|Json(body)| body).unwrap_or_default();
    let accepted = state
        .camera
        .start_capture(CaptureCommand {
            bulb_seconds: request.bulb_seconds,
        })
        .await
        .map_err(ApiFailure)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(CaptureAccepted {
            correlation_id: accepted.correlation_id,
            frame_id: accepted.frame_id,
            exposure_s: accepted.exposure_s,
            watch_topic: event::Topic::CaptureProgress.as_str(),
        }),
    )
        .into_response())
}

/// `POST /api/camera/capture/abort` — SDD §5.8.1.
///
/// A stopping command, so it takes no body extractor and refuses nothing for state: aborting when
/// nothing is running is `200`, exactly as HAL-03 requires of the driver. §5.8.1's rule is that a
/// late stop is safe and a late start is not, and this is the stop half.
pub async fn abort_capture(
    State(state): State<AppState>,
) -> Result<Json<AbortAccepted>, ApiFailure> {
    state
        .camera
        .abort_capture()
        .await
        .map_err(|e| ApiFailure(camera_failure(&e)))?;
    Ok(Json(AbortAccepted { requested: true }))
}

/// `POST /api/camera/fault/ack` — the explicit acknowledgement SDD §5.6 requires.
///
/// The route exists because the state cannot be cleared any other way: a retry gets the same 409,
/// and time does not clear it. That is the point — `Faulted` is raised when the node's picture of
/// the camera or the session is no longer trustworthy, and a UI retry loop must not be able to
/// paper over a disk that is failing.
pub async fn acknowledge_fault(State(state): State<AppState>) -> Json<FaultAcknowledged> {
    match state.camera.acknowledge_fault() {
        Some(fault) => Json(FaultAcknowledged {
            cleared: true,
            code: Some(fault.code.as_str()),
            message: Some(fault.message),
        }),
        None => Json(FaultAcknowledged {
            cleared: false,
            code: None,
            message: None,
        }),
    }
}

/// `GET /api/camera/battery` — SDD §5.8.1.
pub async fn battery(
    State(state): State<AppState>,
) -> Result<Json<astroctl_core::types::BatteryStatus>, ApiFailure> {
    state
        .camera
        .device
        .battery()
        .await
        .map(Json)
        .map_err(|e| ApiFailure(camera_failure(&e)))
}

/// `GET /api/camera/storage` — SDD §5.8.1.
///
/// The camera's *card*, not the node's disk. They answer different questions and REL-12 is about
/// the second one: with the reference body shooting to `Internal RAM` the card never fills, so a
/// panel that showed this number as "space left" would be reassuring and wrong. `system.health`'s
/// `disk_free_gb` is the one that governs whether capture may continue.
pub async fn storage(
    State(state): State<AppState>,
) -> Result<Json<astroctl_core::types::StorageInfo>, ApiFailure> {
    state
        .camera
        .device
        .storage()
        .await
        .map(Json)
        .map_err(|e| ApiFailure(camera_failure(&e)))
}

/// `GET /api/session/current` — SDD §5.8.1's "session.json view + frame list".
///
/// Straight from [`SessionView`], which reads `frames/` on every call rather than serving an
/// in-memory index. That is what makes a frame written by a *previous* run of this binary appear
/// after a restart, and what makes a frame whose sidecar write was interrupted appear with
/// `quality: null` instead of vanishing (SDD §5.5 note 3, REL-05).
pub async fn session_current(
    State(state): State<AppState>,
) -> Result<Json<SessionView>, ApiFailure> {
    state
        .camera
        .session_view()
        .await
        .map(Json)
        .map_err(ApiFailure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{state_with, state_with_camera, TestNode};
    use astroctl_core::bus::Recv;
    use astroctl_core::event::{CaptureState, Event, Topic};
    use astroctl_drivers::simulator::CameraProfile;

    // -----------------------------------------------------------------------------------------
    // Harness
    // -----------------------------------------------------------------------------------------

    /// Drive the real router, assembled exactly as `main` assembles it.
    async fn call(
        state: &AppState,
        method: axum::http::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        use tower::ServiceExt as _;

        let (router, _) = crate::api::router();
        let (ws_router, _) = crate::api::ws_router();
        let app = crate::assemble(router, ws_router, state.clone());

        let mut request = axum::http::Request::builder().method(method).uri(path);
        let body = match body {
            Some(json) => {
                request = request.header(axum::http::header::CONTENT_TYPE, "application/json");
                axum::body::Body::from(serde_json::to_vec(&json).expect("serializes"))
            }
            None => axum::body::Body::empty(),
        };
        let response = app
            .oneshot(request.body(body).expect("request builds"))
            .await
            .expect("the router responds");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn post(state: &AppState, path: &str) -> (StatusCode, serde_json::Value) {
        call(
            state,
            axum::http::Method::POST,
            path,
            Some(serde_json::json!({})),
        )
        .await
    }

    async fn get(state: &AppState, path: &str) -> (StatusCode, serde_json::Value) {
        call(state, axum::http::Method::GET, path, None).await
    }

    /// A node whose camera takes a 1/250 s exposure, so a capture is milliseconds rather than the
    /// half a minute `camera.default_shutter` asks the operator's camera for.
    async fn node() -> AppState {
        node_from(TestNode::open_loopback().with_shutter("1/250")).await
    }

    async fn node_from(node: TestNode) -> AppState {
        let (_, declarations) = crate::api::router();
        state_with(&node, declarations).await
    }

    /// Connect the camera, as an operator does before capturing.
    async fn connected() -> AppState {
        let state = node().await;
        let (status, _) = post(&state, "/api/camera/connect").await;
        assert_eq!(status, StatusCode::OK, "the simulator must connect");
        state
    }

    /// Collect events until `stop` says to, or the deadline passes.
    async fn drain(
        events: &mut astroctl_core::bus::EventSubscriber,
        mut stop: impl FnMut(&Event) -> bool,
    ) -> Vec<Event> {
        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return collected;
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Recv::Event(event)) => {
                    let done = stop(&event);
                    collected.push(event);
                    if done {
                        return collected;
                    }
                }
                Ok(Recv::Lagged { .. }) => {}
                Ok(Recv::Closed) | Err(_) => return collected,
            }
        }
    }

    /// Wait until the capture the node was running has fully finished.
    ///
    /// `frame.saved` is published the moment the frame is durable, which is deliberately *before*
    /// the sidecar is written and before the FSM returns to `Idle` — SDD §5.5 note 3 puts the
    /// metadata after the frame on purpose. So a test that reads the sidecar, or starts a second
    /// capture, the instant that event arrives is racing the tail of the flow it just watched.
    /// This is not a flake to paper over: it is the ordering under test, seen from outside.
    async fn until_idle(state: &AppState) {
        for _ in 0..2_000 {
            if !state.camera.is_capturing() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the capture never finished");
    }

    /// Wait until the driver is demonstrably parked in its exposure.
    ///
    /// The abort is a generation counter (T06), deliberately so — "an abort issued while nothing
    /// is capturing cannot arm itself for the *next* capture". A test that aborts before the
    /// driver has reached its wait therefore aborts nothing and then waits out the whole exposure.
    /// The first progress tick with a non-zero `elapsed_s` is a 1 Hz tick, which cannot be
    /// published until the capture future has been polled and parked.
    async fn until_exposing(events: &mut astroctl_core::bus::EventSubscriber) {
        drain(events, |event| {
            event.topic == Topic::CaptureProgress
                && event.data["state"] == "exposing"
                && event.data["elapsed_s"].as_f64().is_some_and(|s| s > 0.0)
        })
        .await;
    }

    /// The `capture.progress` states, in the order they were published.
    fn progress_states(events: &[Event]) -> Vec<CaptureState> {
        events
            .iter()
            .filter(|event| event.topic == Topic::CaptureProgress)
            .filter_map(|event| serde_json::from_value::<CaptureProgress>(event.data.clone()).ok())
            .map(|progress| progress.state())
            .collect()
    }

    fn payload_of(events: &[Event], topic: Topic) -> Option<serde_json::Value> {
        events
            .iter()
            .find(|event| event.topic == topic)
            .map(|event| event.data.clone())
    }

    // -----------------------------------------------------------------------------------------
    // The acceptance criterion: the whole flow
    // -----------------------------------------------------------------------------------------

    /// M1-T08's first acceptance criterion, end to end and through the real routes:
    /// capture via the API → `capture.progress` sequence → frame and metadata durable → listed.
    #[tokio::test]
    async fn a_capture_runs_the_whole_flow_from_the_api_to_the_session_listing() {
        let state = connected().await;
        let mut events = state.bus.subscribe();

        let (status, accepted) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        let frame_id = accepted["frame_id"]
            .as_str()
            .expect("a frame id")
            .to_owned();
        assert_eq!(
            frame_id, "light_00001",
            "the first frame of a fresh session"
        );
        assert_eq!(accepted["watch_topic"], "capture.progress");
        assert!(accepted["correlation_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));

        let observed = drain(&mut events, |event| event.topic == Topic::FrameSaved).await;

        // 1. the progress sequence of §4.3, in order and with nothing missing.
        assert_eq!(
            progress_states(&observed),
            vec![
                CaptureState::Exposing,
                CaptureState::Downloading,
                CaptureState::Saved
            ],
            "the operator's panel renders the capture from these and nothing else: {observed:#?}"
        );

        // 2. `frame.saved` — the durability report the transfer agent (M1-T11) acts on.
        let saved = payload_of(&observed, Topic::FrameSaved).expect("frame.saved");
        assert_eq!(saved["frame_id"], frame_id.as_str());
        assert!(
            saved["size_bytes"].as_u64().is_some_and(|n| n > 0),
            "{saved}"
        );
        let sha = saved["sha256"].as_str().expect("a hash").to_owned();
        assert_eq!(sha.len(), 64, "lowercase hex sha256: {sha}");
        assert_eq!(sha, sha.to_ascii_lowercase());

        // 3. the frame is on disk, at the path the event named, under the session directory.
        let path = std::path::PathBuf::from(saved["path"].as_str().expect("a path"));
        assert!(path.is_file(), "the frame must exist at {}", path.display());
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("light_00001.fits"),
            "the simulator writes FITS, and the store names it from the frame id"
        );
        let on_disk = tokio::fs::metadata(&path).await.expect("stat").len();
        assert_eq!(
            on_disk,
            saved["size_bytes"].as_u64().expect("size"),
            "the event's size must be the file's, not the camera's report of it"
        );

        // 4. the sidecar, with the exposure parameters that exist nowhere else (SDD §5.5 note 6).
        //    Written *after* the event above, which is the ordering §5.5 note 3 requires.
        until_idle(&state).await;
        let sidecar = path
            .parent()
            .expect("frames/")
            .parent()
            .expect("the session")
            .join("control")
            .join("quality_00001.json");
        let quality: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&sidecar).await.expect("the sidecar reads"))
                .expect("json");
        assert_eq!(quality["frame_id"], frame_id.as_str());
        assert_eq!(quality["settings"]["iso"], "1600");
        assert_eq!(quality["settings"]["shutter"], "1/250");
        assert_eq!(
            quality["sha256"], sha,
            "the sidecar and the event must carry the same hash — the store computed it once and \
             both read that one value"
        );

        // 5. and the listing the panel renders.
        let (status, view) = get(&state, "/api/session/current").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["frame_count"], 1, "{view}");
        assert_eq!(view["frames"][0]["frame_id"], frame_id.as_str());
        assert_eq!(view["frames"][0]["file_name"], "light_00001.fits");
        assert_eq!(view["frames"][0]["quality"]["sha256"], sha);
        assert!(view["session_id"]
            .as_str()
            .is_some_and(|id| id.ends_with("_session")));
    }

    /// The camera JPEG is not a second frame.
    ///
    /// The example config shoots `RAW+JPEG`, so every capture produces two files. Keeping both
    /// would put a second copy of every exposure in the session that nothing reads: the preview
    /// the PWA shows is generated from the raw by the pipeline (SDD §5.7), not taken from the
    /// camera.
    #[tokio::test]
    async fn a_raw_plus_jpeg_capture_stores_one_frame_and_keeps_the_raw() {
        let state = connected().await;
        let mut events = state.bus.subscribe();
        let (status, _) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        drain(&mut events, |event| event.topic == Topic::FrameSaved).await;
        until_idle(&state).await;

        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(
            view["frame_count"], 1,
            "one frame id, one stored file: {view}"
        );
        assert_eq!(view["frames"][0]["file_name"], "light_00001.fits");
    }

    /// A `JPEG`-only capture stores the JPEG, because it is the only file the exposure produced.
    ///
    /// The extension comes from the driver — HAL-03 gives it that job, because only the driver
    /// knows whether a body writes `.cr3`, `.fits` or `.jpg` — so the stored frame's *name* is
    /// evidence that the facade read it from the capture rather than assuming one. Assuming would
    /// mean an operator who selected JPEG got a `light_00001.fits` full of JPEG bytes.
    #[tokio::test]
    async fn a_jpeg_only_capture_stores_the_jpeg_under_its_own_extension() {
        let state = node_from(
            TestNode::open_loopback()
                .with_shutter("1/250")
                .with_format("JPEG"),
        )
        .await;
        let (status, _) = post(&state, "/api/camera/connect").await;
        assert_eq!(status, StatusCode::OK);

        let mut events = state.bus.subscribe();
        let (status, _) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        drain(&mut events, |event| event.topic == Topic::FrameSaved).await;
        until_idle(&state).await;

        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(view["frame_count"], 1, "{view}");
        assert_eq!(
            view["frames"][0]["file_name"], "light_00001.jpg",
            "the frame is named from the file the camera actually wrote: {view}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // The FSM's three acceptance criteria
    // -----------------------------------------------------------------------------------------

    /// "Second capture while Capturing → 409 `Busy`", through the routes.
    #[tokio::test]
    async fn a_second_capture_while_one_is_running_is_refused_with_409_busy() {
        // A long bulb, so the first capture is reliably still running when the second arrives —
        // and so the refusal is tested rather than a race between two fast captures.
        let state = connected().await;
        let (first, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/camera/capture",
            Some(serde_json::json!({"bulb_seconds": 60.0})),
        )
        .await;
        assert_eq!(first, StatusCode::ACCEPTED);

        let (second, body) = post(&state, "/api/camera/capture").await;
        assert_eq!(second, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "BUSY");
        assert_eq!(body["retryable"], false);
        assert_eq!(
            body["detail"]["frame_id"], "light_00001",
            "the refusal names the run the operator is fighting: {body}"
        );

        // The refused request must not have burned a frame id: the claim is taken before the
        // reservation precisely so that a double tap costs nothing.
        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(
            view["frames_reserved"], 1,
            "a refused capture must not consume an id: {view}"
        );

        state.camera.abort_capture().await.expect("abort");
    }

    /// "Faulted state requires explicit ack route to clear."
    ///
    /// The fault is a real one rather than an injected state: the download exceeds
    /// `camera.timeouts.download_seconds`, which SDD §5.3.1 defines as a wedged camera.
    #[tokio::test]
    async fn a_faulted_node_refuses_capture_until_the_ack_route_clears_it() {
        let node = TestNode::open_loopback()
            .with_shutter("1/250")
            .with_download_timeout(1);
        let (_, declarations) = crate::api::router();
        let state = state_with_camera(
            &node,
            declarations,
            CameraProfile {
                // Longer than the one-second budget above, which is the only way to reach §5.3.1's
                // wedged-camera path: the breach is a comparison between a profile knob and a
                // config value, so a test of it has to move both.
                download: Duration::from_secs(2),
                ..CameraProfile::fast()
            },
        )
        .await;
        let (status, _) = post(&state, "/api/camera/connect").await;
        assert_eq!(status, StatusCode::OK);

        let mut events = state.bus.subscribe();
        let (accepted, _) = post(&state, "/api/camera/capture").await;
        assert_eq!(accepted, StatusCode::ACCEPTED);

        let observed = drain(&mut events, |event| {
            event.topic == Topic::Alert
                && event.data["code"] == "CAMERA_TIMEOUT"
                && event.data["severity"] == "critical"
        })
        .await;
        assert!(
            observed.iter().any(|e| e.topic == Topic::Alert
                && e.data["code"] == "CAMERA_TIMEOUT"
                && e.data["severity"] == "critical"),
            "a wedged camera must reach the operator as a critical alert: {observed:#?}"
        );
        assert!(
            state.camera.fault().is_some(),
            "the node must hold the fault"
        );

        // Retrying does not clear it — that is the whole point of the state.
        for attempt in 0..3 {
            let (status, body) = post(&state, "/api/camera/capture").await;
            assert_eq!(status, StatusCode::CONFLICT, "attempt {attempt}: {body}");
            assert_eq!(body["code"], "BUSY");
            assert_eq!(body["detail"]["fault_code"], "CAMERA_TIMEOUT", "{body}");
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("acknowledge")),
                "the refusal must say what clears it: {body}"
            );
        }

        // Only the route does.
        let (status, ack) = post(&state, "/api/camera/fault/ack").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ack["cleared"], true, "{ack}");
        assert_eq!(ack["code"], "CAMERA_TIMEOUT");
        assert!(state.camera.fault().is_none());

        // Acknowledging again is a success, not an error: two phones, or one impatient thumb.
        let (status, ack) = post(&state, "/api/camera/fault/ack").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ack["cleared"], false, "{ack}");
    }

    /// "Abort during bulb: prompt return, no partial frame visible, FSM back to Idle."
    #[tokio::test]
    async fn an_abort_during_a_bulb_exposure_leaves_no_frame_and_returns_to_idle() {
        let state = connected().await;
        let mut events = state.bus.subscribe();

        let (accepted, body) = call(
            &state,
            axum::http::Method::POST,
            "/api/camera/capture",
            Some(serde_json::json!({"bulb_seconds": 600.0})),
        )
        .await;
        assert_eq!(accepted, StatusCode::ACCEPTED, "{body}");
        assert!(
            (body["exposure_s"].as_f64().expect("a duration") - 600.0).abs() < f64::EPSILON,
            "the answer carries the exposure the countdown is measured against: {body}"
        );

        // Wait for the shutter to be demonstrably open before aborting, so this tests the abort
        // path rather than a race with the claim — see `until_exposing`.
        until_exposing(&mut events).await;

        let aborted_at = std::time::Instant::now();
        let (status, abort) = post(&state, "/api/camera/capture/abort").await;
        assert_eq!(status, StatusCode::OK, "{abort}");
        assert_eq!(abort["requested"], true);

        // Prompt: the ten-minute exposure must not be waited out.
        let observed = drain(&mut events, |event| {
            event.topic == Topic::Alert && event.data["code"] == "ABORTED"
        })
        .await;
        assert!(
            aborted_at.elapsed() < Duration::from_secs(10),
            "an aborted bulb must return promptly, not at its scheduled end"
        );
        assert!(
            observed
                .iter()
                .any(|e| e.topic == Topic::Alert && e.data["code"] == "ABORTED"),
            "the operator's own abort is reported as ABORTED, not DEVICE_REJECTED: {observed:#?}"
        );
        assert!(
            !observed.iter().any(|e| e.topic == Topic::FrameSaved),
            "an aborted capture must not report a frame"
        );

        // No partial frame is visible, and the FSM is back to Idle — which the next capture proves
        // better than any accessor could.
        until_idle(&state).await;
        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(
            view["frame_count"], 0,
            "no frame from an aborted capture: {view}"
        );
        assert!(!state.camera.is_capturing());
        let (status, _) = post(&state, "/api/camera/capture").await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "an abort returns the node to Idle, so the next capture is accepted"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Disk-critical (REL-12)
    // -----------------------------------------------------------------------------------------

    /// The 507, and that it happens **before the camera is touched**.
    ///
    /// The camera is deliberately left disconnected. If the disk gate ran after the settings read,
    /// this would answer `409 NOT_CONNECTED` — so the status is the assertion that the ordering is
    /// right, not merely that the threshold works.
    #[tokio::test]
    async fn a_disk_below_the_critical_threshold_refuses_capture_with_507_before_the_camera() {
        let state = node_from(
            TestNode::open_loopback()
                .with_shutter("1/250")
                .with_disk_critical_above_any_volume(),
        )
        .await;

        let (status, body) = post(&state, "/api/camera/capture").await;
        assert_eq!(
            status,
            StatusCode::INSUFFICIENT_STORAGE,
            "SDD §4.2 fixes one status per code, and ingest already raises DISK_FULL as 507: \
             {body}"
        );
        assert_eq!(body["code"], "DISK_FULL");
        assert_eq!(body["retryable"], false);
        assert!(body["detail"]["free_gb"].as_f64().is_some(), "{body}");
        assert!(body["detail"]["critical_gb"].as_f64().is_some(), "{body}");

        // Nothing was started: no id burned, no frame, and the node is still idle.
        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(view["frames_reserved"], 0, "{view}");
        assert_eq!(view["frame_count"], 0, "{view}");
        assert!(!state.camera.is_capturing());
    }

    /// The other half of the same rule: a healthy disk must not refuse, or the check carries no
    /// information.
    #[tokio::test]
    async fn a_healthy_disk_does_not_refuse_capture() {
        let state = connected().await;
        let (status, body) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    }

    // -----------------------------------------------------------------------------------------
    // Refusals answered before the 202
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_disconnected_camera_refuses_capture_rather_than_accepting_it() {
        let state = node().await;
        let (status, body) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "NOT_CONNECTED");
        assert!(
            !state.camera.is_capturing(),
            "the claim must have been released"
        );
    }

    /// A `bulb` shutter with no duration is refused here rather than by the driver.
    ///
    /// T06's camera returns `Rejected` for exactly this, which would arrive *after* the `202` —
    /// so the operator would be told their capture started and then, separately, that it did not.
    /// The measured reason it matters: with the R10's mode dial on Bulb the API can only offer
    /// `bulb`, so this is a state an operator reaches by turning a dial.
    #[tokio::test]
    async fn a_bulb_shutter_with_no_duration_is_refused_before_the_202() {
        let state = node_from(TestNode::open_loopback().with_shutter("bulb")).await;
        let (status, _) = post(&state, "/api/camera/connect").await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "DEVICE_REJECTED");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains("bulb_seconds")),
            "the refusal must name the way out: {body}"
        );
        assert!(!state.camera.is_capturing());

        // And the same camera captures fine when the duration is supplied.
        let (status, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/camera/capture",
            Some(serde_json::json!({"bulb_seconds": 0.05})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_nonsense_bulb_duration_is_a_validation_failure() {
        let state = connected().await;
        for bad in [0.0, -1.0] {
            let (status, body) = call(
                &state,
                axum::http::Method::POST,
                "/api/camera/capture",
                Some(serde_json::json!({ "bulb_seconds": bad })),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "for {bad}: {body}"
            );
            assert_eq!(body["code"], "VALIDATION");
        }
        assert!(!state.camera.is_capturing());
    }

    // -----------------------------------------------------------------------------------------
    // The remaining §5.8.1 camera rows
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn the_settings_route_reports_the_current_values_and_everything_the_body_offers() {
        let state = connected().await;
        let (status, body) = get(&state, "/api/camera/settings").await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // §5.8.1's `{iso, shutter, aperture, format}`, flattened as the row is written.
        assert_eq!(body["iso"], "1600");
        assert_eq!(body["shutter"], "1/250");
        assert_eq!(body["format"], "RAW+JPEG");
        assert_eq!(body["aperture"], "5.6");

        // "+ available values" — what the settings selectors are built from.
        let isos = body["available"]["isos"].as_array().expect("isos");
        assert!(isos.contains(&serde_json::json!("1600")), "{body}");
        assert!(
            body["available"]["shutters"]
                .as_array()
                .expect("shutters")
                .contains(&serde_json::json!("bulb")),
            "the body offers `bulb` as a shutter setting even though `capture` refuses it: {body}"
        );
        assert_eq!(
            body["available"]["formats"],
            serde_json::json!(["RAW", "JPEG", "RAW+JPEG"])
        );
    }

    #[tokio::test]
    async fn settings_are_applied_and_read_back_from_the_camera() {
        let state = connected().await;
        let (status, body) = call(
            &state,
            axum::http::Method::PUT,
            "/api/camera/settings",
            Some(serde_json::json!({"iso": "800", "format": "RAW"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["iso"], "800");
        assert_eq!(body["format"], "RAW");
        assert_eq!(body["shutter"], "1/250", "an absent field is left alone");

        // Read back independently, so this asserts the camera changed rather than the reply.
        let (_, again) = get(&state, "/api/camera/settings").await;
        assert_eq!(again["iso"], "800");
    }

    /// A token the body does not offer is refused, never silently substituted (HAL-03).
    #[tokio::test]
    async fn a_setting_the_camera_does_not_offer_is_refused() {
        let state = connected().await;
        let (status, body) = call(
            &state,
            axum::http::Method::PUT,
            "/api/camera/settings",
            Some(serde_json::json!({"iso": "999999"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "DEVICE_REJECTED");

        let (_, unchanged) = get(&state, "/api/camera/settings").await;
        assert_eq!(
            unchanged["iso"], "1600",
            "a refused setting changes nothing"
        );
    }

    /// The values are the simulator's measured ones (M1-T06), not invented placeholders.
    #[tokio::test]
    async fn battery_and_storage_report_what_the_camera_measures() {
        let state = connected().await;

        let (status, battery) = get(&state, "/api/camera/battery").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(battery["percent"], 100);
        assert_eq!(battery["charging"], true);

        let (status, storage) = get(&state, "/api/camera/storage").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(storage["free_mb"], 69_500, "the R10's card, measured");
        assert_eq!(storage["total_mb"], 127_800);
    }

    /// `camera.status` reaches the operator on connect rather than up to a minute later.
    #[tokio::test]
    async fn connecting_publishes_camera_status_immediately() {
        let state = node().await;
        let mut events = state.bus.subscribe();

        let (status, body) = post(&state, "/api/camera/connect").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["connected"], true, "{body}");
        assert_eq!(body["battery_pct"], 100);
        assert_eq!(body["storage_free_mb"], 69_500);

        let observed = drain(&mut events, |event| event.topic == Topic::CameraStatus).await;
        let published = payload_of(&observed, Topic::CameraStatus).expect("camera.status");
        assert_eq!(published["connected"], true, "{published}");

        // And disconnecting says so with nulls rather than zeroes: a zeroed battery renders as an
        // empty gauge, which is a lie the operator would act on (SDD §4.3).
        let (_, body) = post(&state, "/api/camera/disconnect").await;
        assert_eq!(body["connected"], false, "{body}");
        assert_eq!(body["battery_pct"], serde_json::Value::Null);
        assert_eq!(body["storage_free_mb"], serde_json::Value::Null);
    }

    // -----------------------------------------------------------------------------------------
    // The session listing
    // -----------------------------------------------------------------------------------------

    /// A node with no frames yet still has a session, because §8.1 opens one at startup.
    ///
    /// A `404` here would send the operator looking for something to create; there is nothing to
    /// create, and the frame list being empty is the answer.
    #[tokio::test]
    async fn a_fresh_node_reports_an_open_session_with_no_frames() {
        let state = node().await;
        let (status, view) = get(&state, "/api/session/current").await;
        assert_eq!(status, StatusCode::OK, "{view}");
        assert_eq!(view["frame_count"], 0);
        assert_eq!(view["frames"], serde_json::json!([]));
        assert_eq!(view["frames_reserved"], 0);
        // The equipment profile from the operator's config, which is what tags frames for
        // calibration matching (PRD §5.9).
        assert_eq!(view["equipment"]["telescope"], "SW 200PDS f/5");
        assert_eq!(view["equipment"]["camera"], "Canon R10");
        assert!(view["created_ts"]
            .as_str()
            .is_some_and(|ts| ts.ends_with('Z')));
    }

    /// SDD §5.5 note 3, through the API: a frame whose sidecar is missing is **listed**, not
    /// hidden. A crash between `commit_frame` and the metadata write leaves exactly this state,
    /// and REL-05 says the frame is still a frame.
    #[tokio::test]
    async fn a_frame_with_no_sidecar_is_listed_with_a_null_quality() {
        let state = connected().await;
        let mut events = state.bus.subscribe();
        let (status, _) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let observed = drain(&mut events, |event| event.topic == Topic::FrameSaved).await;
        let path = std::path::PathBuf::from(
            payload_of(&observed, Topic::FrameSaved).expect("frame.saved")["path"]
                .as_str()
                .expect("a path"),
        );
        until_idle(&state).await;

        // Simulate the interrupted metadata write by removing what it produced.
        let sidecar = path
            .parent()
            .expect("frames/")
            .parent()
            .expect("the session")
            .join("control")
            .join("quality_00001.json");
        tokio::fs::remove_file(&sidecar).await.expect("removes");

        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(view["frame_count"], 1, "the frame is still a frame: {view}");
        assert_eq!(view["frames"][0]["frame_id"], "light_00001");
        assert_eq!(view["frames"][0]["quality"], serde_json::Value::Null);
    }

    /// Frames accumulate across captures with unique ids, which is the counter REL-04 protects.
    #[tokio::test]
    async fn consecutive_captures_get_consecutive_ids() {
        let state = connected().await;
        for expected in ["light_00001", "light_00002"] {
            let mut events = state.bus.subscribe();
            let (status, accepted) = post(&state, "/api/camera/capture").await;
            assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
            assert_eq!(accepted["frame_id"], expected);
            drain(&mut events, |event| event.topic == Topic::FrameSaved).await;
            until_idle(&state).await;
        }

        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(view["frame_count"], 2, "{view}");
        assert_eq!(view["frames"][0]["frame_id"], "light_00001");
        assert_eq!(view["frames"][1]["frame_id"], "light_00002");
    }

    // -----------------------------------------------------------------------------------------
    // Shutdown (SDD §7)
    // -----------------------------------------------------------------------------------------

    /// The M1-T03 regression, for the two handles M1-T08 added.
    ///
    /// The capture task holds an [`EventBus`] handle, which is a broadcast *sender*, so a capture
    /// in flight would otherwise stall the session log's flush for its whole timeout — and a
    /// capture in flight is, like a slew, exactly when a service restart lands.
    ///
    /// It also asserts the §7 step 3 ordering: `finish_inflight` **waits** for the download rather
    /// than aborting it, because a half-downloaded frame is a lost frame.
    #[tokio::test]
    async fn shutdown_finishes_an_in_flight_capture_and_releases_its_bus_handle() {
        let state = connected().await;
        let mut events = state.bus.subscribe();
        let (status, _) = post(&state, "/api/camera/capture").await;
        assert_eq!(status, StatusCode::ACCEPTED, "a capture must be in flight");

        let camera = Arc::clone(&state.camera);
        camera.finish_inflight(Duration::from_secs(20)).await;

        // Waited, not abandoned: the frame the exposure had already paid for is on disk.
        let (_, view) = get(&state, "/api/session/current").await;
        assert_eq!(
            view["frame_count"], 1,
            "shutdown finishes the download rather than losing the frame (SDD §7): {view}"
        );

        drop(state);
        drop(camera);

        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(events.recv().await, Recv::Closed) {
                    return;
                }
            }
        })
        .await;
        assert!(
            closed.is_ok(),
            "an EventBus handle outlived the camera facade, so the session log could not flush"
        );
    }

    /// A capture that will not finish must not hold shutdown open indefinitely.
    #[tokio::test]
    async fn shutdown_gives_up_on_a_capture_that_will_not_finish() {
        let state = connected().await;
        let (status, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/camera/capture",
            Some(serde_json::json!({"bulb_seconds": 3600.0})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let started = std::time::Instant::now();
        state
            .camera
            .finish_inflight(Duration::from_millis(200))
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an operator power-cycling a Pi that will not die is worse than one lost frame"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Units
    // -----------------------------------------------------------------------------------------

    #[test]
    fn shutter_tokens_parse_the_way_the_camera_spells_them() {
        // Fractions below a second and decimals above — how every Canon body spells them, and
        // therefore what a facade above the HAL has to read. Getting this wrong does not change
        // the exposure (the driver parses its own), it moves the `downloading` transition, which
        // is a countdown that finishes at the wrong moment.
        assert_eq!(shutter_seconds("30"), Some(30.0));
        assert_eq!(shutter_seconds("1/250"), Some(0.004));
        assert_eq!(shutter_seconds("0.4"), Some(0.4));
        assert_eq!(shutter_seconds("bulb"), None);
        assert_eq!(
            shutter_seconds("BULB"),
            None,
            "case is the camera's business"
        );
        assert_eq!(shutter_seconds("1/0"), None, "not an infinite exposure");
        assert_eq!(shutter_seconds("nonsense"), None);
    }
}
