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
use astroctl_core::types::{CameraSettings, DeviceKind};
use astroctl_hal::camera::{Camera, CaptureRequest, CaptureResult};
use astroctl_session::{
    CaptureParams, DiskLevel, FrameId, FrameKind, FrameStore, Session, SessionView, StoreError,
};
use crate::orchestrator::{self, Fault, Orchestrator, Outcome};

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
