//! `CanonGPhoto2Camera` — the [`Camera`] implementation, and the registry factory that builds it.
//!
//! This file is the async half: it holds no camera state that libgphoto2 owns, does no blocking
//! work, and exists to turn trait calls into [`CamCmd`]s and replies back into trait results.
//! Everything that could block is on the other side of [`CameraLink`].
//!
//! # Two decisions worth knowing before reading
//!
//! **Connect reads; it does not write.** `camera.default_iso` / `default_shutter` /
//! `default_format` are *not* applied to the body on connection. They are the values a session
//! starts from, and pushing them at connect time would mean a body whose mode dial forbids the
//! configured shutter — with the dial on Bulb the R10 offers only `bulb` (M2-T01) — could not be
//! connected to at all. A camera that is physically reachable must always be connectable; the
//! sequencer applies the defaults when it has somewhere to report a refusal.
//!
//! **A setter checks the cached list first and only re-reads when it is about to say no.** The
//! `Camera` trait forbids silent substitution: a value the body does not offer must be
//! `Rejected`. But the body's lists change while it is connected (that mode dial again), so a
//! cache can be wrong in both directions. Re-reading before every write would cost 222 ms on
//! every ISO change; never re-reading would refuse values the body has started accepting. So the
//! happy path uses the cache, and a token that is *about to be rejected* triggers one re-read
//! first. A false rejection becomes impossible; a false acceptance is caught by the camera
//! itself, which answers `Rejected` — which is the right answer anyway.
//!
//! # Why there is no capture-progress channel
//!
//! The facade above this driver infers the `exposing`→`downloading` transition from a timer,
//! which SDD change note 1.19.0 recorded as standing "until the real driver can observe it".
//! M2-T03 is that driver, and the answer is that it cannot — not on this body, and for a reason
//! that is a measurement rather than an omission.
//!
//! With `capturetarget=Internal RAM` the USB transfer happens **inside** `capture_image()`:
//! M2-T01 measured 2.08 s for the capture and 2.67 ms to write the resulting 32 MB to disk, which
//! is memory bandwidth, not a wire. By the time this driver regains control, exposing and
//! downloading have both already happened — a driver-reported transition would fire 2.67 ms
//! before `saved`. On the bulb path the transition *is* exactly observable, because this driver
//! closes the shutter itself, but there the facade's timer is already exact by construction since
//! the same duration drives both.
//!
//! So a progress channel would cost a parameter on `CaptureRequest` (breaking its `PartialEq`), a
//! field in every driver and a select arm in the facade, and would buy no additional truth. The
//! inferred transition is the best available fact about this camera. **This changes if the
//! capture target ever moves to `Memory card`**, which puts the 32 MB transfer inside
//! `download_to` where it is separately observable.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::config::{CameraConfig, CameraTimeouts};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    AvailableSettings, BatteryStatus, CameraCapabilities, CameraSettings, DeviceInfo, ImageFormat,
    StorageInfo,
};
use astroctl_hal::camera::{
    Camera, CaptureRequest, CaptureResult, CapturedFile, CapturedFileKind, LiveViewFrame,
};
use astroctl_hal::registry::{CameraFactory, DetectedDevice, DriverInitError};
use astroctl_hal::stream::FrameStream;
use async_trait::async_trait;
use chrono::Utc;

use super::liveview::{LinkSource, LiveView};
use super::ops::{
    format_token, interpret_choices, interpret_identity, interpret_settings, shutter_seconds,
    BodyGeometry, CamOpsFactory, CameraIdentity, CfgKey, RawChoices, RawFileRef,
};
use super::recovery::{self, Relink};
use super::thread::{CamCmd, CameraLink, FaultSink, FaultSource, OpClass};
// Only `new` builds a fault channel, and `new` exists only where there is a `CamOpsFactory` to
// pass it — the tests and the `libgphoto2` build. Same gate, or a default build warns.
#[cfg(any(test, feature = "libgphoto2"))]
use super::thread::fault_channel;

pub use super::recovery::LinkState;

/// The registry name, which must equal `CameraDriver::Gphoto2.as_str()` or the driver is
/// unreachable from configuration. Asserted by a test rather than trusted.
pub const DRIVER_NAME: &str = "gphoto2";

/// What the driver knows about the body between calls.
#[derive(Debug, Clone)]
struct Cached {
    info: DeviceInfo,
    capabilities: CameraCapabilities,
    available: AvailableSettings,
    format_choices: Vec<String>,
}

impl Cached {
    /// What is known before anything has been asked.
    ///
    /// [`Camera::capabilities`] and [`Camera::device_info`] are callable while disconnected, so
    /// there has to be an answer here. It is the *reference body's documented* capability set —
    /// R10 sensor geometry from PRD §4.3/§8.3, bulb and live view because M2-T01 proved both on
    /// real hardware — with the enumerated lists left empty, because those are the ones that
    /// genuinely vary per body and inventing them is how a UI ends up offering an ISO the camera
    /// has never heard of. After a connect, every field is what this body reported.
    fn unconnected() -> Self {
        Self {
            info: DeviceInfo {
                name: "Canon camera (gPhoto2)".to_owned(),
                model: "unknown until connected".to_owned(),
                firmware: None,
                serial: None,
                protocol: "PTP/USB (libgphoto2)".to_owned(),
            },
            capabilities: CameraCapabilities {
                has_bulb: true,
                has_live_view: true,
                has_mirror_lockup: false,
                sensor_width_px: BodyGeometry::R10.width_px,
                sensor_height_px: BodyGeometry::R10.height_px,
                pixel_size_um: BodyGeometry::R10.pixel_size_um,
                supported_formats: Vec::new(),
                min_iso: 0,
                max_iso: 0,
                min_shutter_s: 0.0,
                max_shutter_s: 0.0,
            },
            available: AvailableSettings {
                isos: Vec::new(),
                shutters: Vec::new(),
                apertures: Vec::new(),
                formats: Vec::new(),
            },
            format_choices: Vec::new(),
        }
    }
}

/// A Canon body driven over PTP/USB through libgphoto2 (SDD §5.3, PRD CAM-01/02).
///
/// A handle over [`Inner`]. The split exists because M2-T04's recovery loop and live-view pump
/// are long-lived tasks that need the same link slot and the same cache the trait methods use —
/// and a task cannot borrow from a `&self` that outlives nothing in particular. One `Arc`, shared
/// by the driver and the two loops it owns, is the whole of it.
#[derive(Debug)]
pub struct CanonGPhoto2Camera {
    inner: Arc<Inner>,
}

/// Everything the driver and its background loops share.
#[derive(Debug)]
struct Inner {
    /// Builds the blocking half. Held rather than consumed so a reconnect can spawn a second
    /// thread — the first one is never reusable once it has been abandoned.
    ops: Arc<dyn CamOpsFactory>,
    /// The camera thread, once one has been spawned. `None` while disconnected.
    link: Mutex<Option<Arc<CameraLink>>>,
    /// Everything the synchronous trait methods have to answer from.
    cached: Mutex<Cached>,
    /// Operation budgets, from `camera.timeouts`.
    timeouts: CameraTimeouts,
    /// Frames per second to pull, from `camera.live_view_fps` (PRF-02, USB-11).
    live_view_fps: u32,
    /// The running live-view session, if any.
    ///
    /// Survives a wedge on purpose: dropping the sink would end every subscriber's stream, and
    /// the field node's forwarding task does not re-subscribe. See [`super::liveview`].
    live: Mutex<Option<LiveView>>,
    /// Where a failing link reports itself. Cloned into every [`CameraLink`] this driver builds.
    faults: FaultSink,
    /// What the driver believes about the camera — the driver's half of `camera.status`.
    state: tokio::sync::watch::Sender<LinkState>,
    /// The recovery task, started on the first connect.
    ///
    /// Lazily rather than in [`new`](CanonGPhoto2Camera::new) because SDD §8.1 requires registry
    /// construction to be free of side effects, and because `new` may be called before there is a
    /// runtime to spawn on. `OnceLock` so two concurrent connects cannot start two loops — two
    /// recovery loops racing to rebuild one link is the failure this type prevents by existing.
    recovery: std::sync::OnceLock<tokio::task::JoinHandle<()>>,
    /// The fault channel's receiving half, waiting for the recovery loop to be started.
    ///
    /// Taken exactly once, by whichever connect wins the `OnceLock`.
    pending_faults: Mutex<Option<FaultSource>>,
}

impl CanonGPhoto2Camera {
    /// Builds the driver. No I/O and no thread — SDD §8.1 requires registry construction to be
    /// free of side effects, and the camera is opened by [`Camera::connect`].
    ///
    /// Compiled where there is a [`CamOpsFactory`] to pass it, which is the `libgphoto2` build
    /// and the tests. A default build has the driver, the thread and the diagnosis — everything
    /// the gates check — and nothing to point them at, which is exactly the shape of a binary
    /// built without the camera library.
    #[cfg(any(test, feature = "libgphoto2"))]
    pub(crate) fn new(config: &CameraConfig, ops: Arc<dyn CamOpsFactory>) -> Self {
        let (faults, fault_source) = fault_channel();
        let (state, _watch) = tokio::sync::watch::channel(LinkState::Disconnected);
        Self {
            inner: Arc::new(Inner {
                ops,
                link: Mutex::new(None),
                cached: Mutex::new(Cached::unconnected()),
                timeouts: config.timeouts,
                live_view_fps: config.live_view_fps,
                live: Mutex::new(None),
                faults,
                state,
                recovery: std::sync::OnceLock::new(),
                // Parked until the first connect starts the loop; see `Inner::start_recovery`.
                pending_faults: Mutex::new(Some(fault_source)),
            }),
        }
    }

    /// What the driver believes about its link to the camera (REL-03).
    ///
    /// **The driver's half of `camera.status`, and the only half it may have** — this crate has
    /// no event bus by design (crate docs: "drivers are silent"), so the facade reads this and
    /// decides what reaches the wire.
    ///
    /// **Eventually consistent, deliberately.** A wedge is detected on whichever task issued the
    /// doomed command, and the transition is published by the recovery loop the next time it is
    /// scheduled — so for one tick after a failure this still reads `Connected`. Tightening that
    /// would mean giving every [`CameraLink`] a writer on this channel, i.e. two writers racing
    /// on one state, to buy a millisecond on a value that is polled every 60 s (SDD §4.3). The
    /// authoritative answer to "is the camera usable" is what an operation returns; this is for
    /// display.
    ///
    /// Note for whoever wires that up: `astroctl_core::event::CameraStatus` is a two-state
    /// boolean today and has no `reconnecting`, which SDD §5.3.1 and the M2-T04 acceptance
    /// criterion both ask for. That gap is recorded in the task file; it is a §4.3 payload change
    /// and not something this driver could make on its own.
    #[must_use]
    pub fn link_state(&self) -> LinkState {
        self.inner.state.borrow().clone()
    }

    /// A subscription to [`link_state`](Self::link_state), for a facade that wants transitions
    /// rather than polls.
    #[must_use]
    pub fn watch_link_state(&self) -> tokio::sync::watch::Receiver<LinkState> {
        self.inner.state.subscribe()
    }
}

impl Inner {
    /// The live link, or `NotConnected`.
    ///
    /// Returns an `Arc` clone so the caller can await without holding the mutex — the workspace
    /// denies `await_holding_lock`, and more to the point a held lock here would serialise every
    /// camera command behind whichever one is currently in flight, on the *async* side, where
    /// there is no reason for it. The queueing that must happen happens on the thread.
    fn link(&self) -> Result<Arc<CameraLink>, DeviceError> {
        let link = self
            .link
            .lock()
            .expect("the camera link slot is never poisoned");
        match link.as_ref() {
            // A link whose thread has been abandoned is not a link. Reporting it as
            // `NotConnected` rather than letting the send fail gives the same answer one round
            // trip earlier and with a message that names the state.
            Some(link) if link.is_up() => Ok(Arc::clone(link)),
            _ => Err(DeviceError::NotConnected),
        }
    }

    /// The link, spawning the thread if this is the first connect.
    fn link_or_spawn(&self) -> Result<Arc<CameraLink>, DeviceError> {
        let mut slot = self
            .link
            .lock()
            .expect("the camera link slot is never poisoned");

        // A wedged link is replaced rather than reused: its thread is gone or going, and its
        // channel is closed. Dropping it here is also what joins nothing — `wedge` already took
        // the handle, so this drop cannot block on a stuck libgphoto2 call.
        if slot.as_ref().is_some_and(|link| !link.is_up()) {
            *slot = None;
        }

        if slot.is_none() {
            *slot = Some(Arc::new(CameraLink::spawn(
                Arc::clone(&self.ops),
                self.timeouts,
                self.faults.clone(),
            )?));
        }
        Ok(Arc::clone(slot.as_ref().expect("just spawned")))
    }

    /// Starts the recovery loop, once, on the first connect.
    ///
    /// Idempotent by construction: `OnceLock::get_or_init` runs the closure exactly once however
    /// many callers arrive together, so a driver whose `connect` is called from two tasks does
    /// not end up with two loops racing to rebuild the same link.
    fn start_recovery(self: &Arc<Self>) {
        self.recovery.get_or_init(|| {
            let source = self
                .pending_faults
                .lock()
                .expect("the camera fault source is never poisoned")
                .take()
                .expect("the fault source is taken exactly once, here");
            let relink: Arc<dyn Relink> = Arc::clone(self) as Arc<dyn Relink>;
            tokio::spawn(recovery::run(relink, source, self.state.clone()))
        });
    }

    /// Stops the pacing loop and tells the body to leave live view. Idempotent.
    ///
    /// **Both halves, and the order is the one that survives a dead camera.** Dropping the
    /// session first ends every subscriber's stream and stops the USB traffic immediately, which
    /// is the half that must always happen; asking the body to leave live view is the half that
    /// needs a camera, and it is best effort because a stopping command may not fail for want of
    /// something to stop (SDD §5.8.1). Doing it the other way round would make an operator's
    /// *stop* wait on a camera that has already gone.
    ///
    /// The body half is worth the round trip: a mirrorless body left in live view keeps its
    /// sensor powered and its screen lit, and this driver also reports the battery that pays for
    /// it.
    async fn stop_live_view_pump(&self) {
        let session = self
            .live
            .lock()
            .expect("the camera live-view slot is never poisoned")
            .take();
        let was_running = session.is_some();
        // Explicit rather than at end of scope, so the ordering above is a fact rather than a
        // hope about where the compiler puts the drop.
        drop(session);

        if !was_running {
            return;
        }
        let Ok(link) = Inner::link(self) else {
            return;
        };
        // Through the gate: during an exposure live view is already not running, so there is
        // nothing to stop, and queueing behind the exposure would blow `config_seconds` and wedge
        // the very camera the operator is trying to be gentle with.
        let _ = link
            .request_unless_capturing(OpClass::Config, CamCmd::StopPreview)
            .await;
    }

    /// Records a state the *driver* reached, as opposed to one recovery reached.
    ///
    /// Only two: a successful connect and a disconnect. Everything between them belongs to the
    /// recovery loop, and two writers taking turns on one channel is how a `Connected` published
    /// here would arrive after a `Reconnecting` published there and undo it. These two are safe
    /// because both happen at the operator's request, when no recovery cycle can be running: a
    /// connect that succeeds *is* a working link, and a disconnect has torn the link down.
    fn note(&self, state: LinkState) {
        self.state.send_if_modified(|current| {
            if *current == state {
                return false;
            }
            *current = state;
            true
        });
    }

    /// Stores what a successful open established.
    fn remember(&self, identity: CameraIdentity) {
        let mut cached = self
            .cached
            .lock()
            .expect("the camera cache is never poisoned");
        cached.info = identity.info;
        cached.capabilities = identity.capabilities;
        cached.available = identity.available;
        cached.format_choices = identity.format_choices;
    }

    /// The cached list for one key, and the human name used in a rejection message.
    fn cached_choices(&self, key: CfgKey) -> Vec<String> {
        let cached = self
            .cached
            .lock()
            .expect("the camera cache is never poisoned");
        match key {
            CfgKey::Iso => cached.available.isos.clone(),
            CfgKey::Shutter => cached.available.shutters.clone(),
            CfgKey::Aperture => cached.available.apertures.clone(),
            CfgKey::ImageFormat => cached.format_choices.clone(),
        }
    }

    /// Re-reads the body's choice lists and refreshes the cache with them.
    async fn refresh_choices(&self, link: &CameraLink) -> Result<RawChoices, DeviceError> {
        let raw: RawChoices = link
            .request_unless_capturing(OpClass::Config, CamCmd::GetChoices)
            .await?;
        let mut cached = self
            .cached
            .lock()
            .expect("the camera cache is never poisoned");
        cached.available = interpret_choices(&raw);
        cached.format_choices.clone_from(&raw.formats);
        Ok(raw)
    }

    /// Writes an enumerated setting, refusing a token the body does not offer.
    ///
    /// See the module docs for why the cache is consulted first and re-read only on the way to a
    /// refusal.
    async fn set_enumerated(
        &self,
        key: CfgKey,
        label: &str,
        value: &str,
    ) -> Result<(), DeviceError> {
        let link = self.link()?;

        if !self
            .cached_choices(key)
            .iter()
            .any(|choice| choice == value)
        {
            // About to refuse. Ask the body once more first — the mode dial may have moved since
            // the cache was filled, and refusing a value the camera would now accept is worse
            // than one extra config-tree read.
            let refreshed = self.refresh_choices(&link).await?;
            let offers = match key {
                CfgKey::Iso => &refreshed.isos,
                CfgKey::Shutter => &refreshed.shutters,
                CfgKey::Aperture => &refreshed.apertures,
                CfgKey::ImageFormat => &refreshed.formats,
            };
            if !offers.iter().any(|choice| choice == value) {
                return Err(DeviceError::Rejected(rejection(label, value, offers)));
            }
        }

        let value = value.to_owned();
        link.request_unless_capturing(OpClass::Config, move |reply| CamCmd::SetSetting {
            key,
            value,
            reply,
        })
        .await
    }

    /// The whole of one exposure: trigger, then land every file it produced.
    ///
    /// Shared by [`Camera::capture`] and [`Camera::capture_bulb`] because everything after the
    /// trigger is identical — the two differ only in how the shutter is opened and for how long,
    /// and duplicating the download-and-clean-up half is how the timed path and the bulb path
    /// end up with different durability.
    ///
    /// `trigger` says how the shutter is opened; `exposure` is what the caller asked for, used
    /// for the operation budget and as the fallback when the body volunteers no figure of its
    /// own.
    async fn expose(
        &self,
        request: &CaptureRequest,
        exposure: Duration,
        shutter: String,
        trigger: Trigger,
    ) -> Result<CaptureResult, DeviceError> {
        let link = self.link()?;

        // **Read before the claim, and before anything else.** `abort_capture` skips its queued
        // safety net while a capture is in flight, so from the instant the claim is taken this
        // generation is the *only* record of an abort. Reading it after the claim — or after the
        // settings round trip below, which is four config reads on the R10 — leaves a window in
        // which an operator's stop is raised, seen by nothing, and reported as done while the
        // exposure it was meant to stop runs to completion.
        let since = link.abort_generation();
        let _claim = link.claim_capture()?;

        // Read back rather than echoed: the body may have coerced a requested value, and the
        // frame metadata must record what the sensor did (HAL-03 `CaptureResult::settings`).
        // Ungated on purpose — this round trip *is* part of the exposure, and the gate exists to
        // keep everything that is not out of the queue.
        let raw_settings = link.request(OpClass::Config, CamCmd::GetSettings).await?;
        let mut settings = interpret_settings(raw_settings);
        // A bulb exposure has no shutter *token* — the mechanism is a press and a release — so
        // the result reports `bulb`, which is both what the body shows and what the simulator
        // reports for the same exposure.
        settings.shutter = shutter;

        // An abort that landed during the settings read has already happened; there is no point
        // opening the shutter to close it again.
        if link.aborted_since(since) {
            return Err(aborted());
        }

        // Sampled here rather than inside the thread because this is the last instant before the
        // command is queued; on an idle camera the queue is empty and the difference is the
        // channel send. It is the shutter-open time to within that.
        let started_at = Utc::now();
        let triggered = match trigger {
            Trigger::Timed => {
                link.request(OpClass::Capture(exposure), CamCmd::Capture)
                    .await
            }
            Trigger::Bulb(duration) => {
                let file_wait = OpClass::bulb_file_wait(duration, &self.timeouts);
                link.request(OpClass::Capture(duration), move |reply| {
                    CamCmd::CaptureBulb {
                        duration,
                        since,
                        file_wait,
                        reply,
                    }
                })
                .await
            }
        };
        let captured = match triggered {
            Ok(captured) => captured,
            Err(error) => return Err(after_a_failed_exposure(&link, since, error)),
        };

        // The abort could not interrupt the C call, but it can still prevent the frame reaching
        // the disk — which is what "nothing on disk" means for a capture aborted mid-exposure on
        // a body that cannot be interrupted. The frame stays in the camera and is overwritten by
        // the next one.
        if link.aborted_since(since) {
            return Err(aborted());
        }

        // **Raw first, which is not the order the body hands them over.** `CaptureResult::files`
        // is documented as raw first, and the obvious implementation — the trigger's own file,
        // then whatever the event drain added — gets that wrong on the reference body: shooting
        // RAW+JPEG, `capture_image()` returned the *JPEG* and the CR3 arrived as the `NewFile`
        // event, so the pair came out `[Jpeg, Raw]`. `raw()` and `jpeg()` search by kind and were
        // unaffected, which is exactly why this would have gone unnoticed until something
        // iterated `files` and treated the first entry as the science frame.
        let mut ordered = captured.files.clone();
        ordered.sort_by_key(|file| match file.kind() {
            CapturedFileKind::Raw => 0,
            CapturedFileKind::Jpeg => 1,
        });

        let mut files: Vec<CapturedFile> = Vec::with_capacity(ordered.len());
        for file in &ordered {
            let dir = request.dir.clone();
            let stem = request.stem.clone();
            let file_ref = file.clone();
            let landed = link
                .request(OpClass::Download, move |reply| CamCmd::Download {
                    file: file_ref,
                    dir,
                    stem,
                    reply,
                })
                .await;
            match landed {
                Ok(landed) => files.push(CapturedFile {
                    path: landed.path,
                    kind: landed.kind,
                    size_bytes: landed.size_bytes,
                }),
                Err(error) => {
                    // A frame that only half arrived is not a frame. Remove what was written so
                    // the session directory never shows a partial set, and so the retry does not
                    // meet the stale-temporary trap.
                    self.discard(&link, request, &captured.files, &files).await;
                    return Err(error);
                }
            }
        }

        Ok(CaptureResult {
            files,
            settings,
            started_at,
            // The camera's own figure where it has one — the R10 reports `BulbExposureTime`,
            // and M2-T01 saw it read 9 for a 10 s hold. That one-second discrepancy is the real
            // integration time, and the metadata should carry it rather than the request.
            exposure: captured
                .exposure_seconds
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                .unwrap_or(exposure),
        })
    }

    /// Removes everything a capture wrote before it failed.
    ///
    /// Both halves matter: the temporaries (which a directory listing shows and which the *next*
    /// capture under this stem would trip over) and the files that already landed under their
    /// final names (which a frame browser would show as a complete frame that has no raw).
    async fn discard(
        &self,
        link: &CameraLink,
        request: &CaptureRequest,
        expected: &[RawFileRef],
        landed: &[CapturedFile],
    ) {
        for file in landed {
            if let Err(error) = tokio::fs::remove_file(&file.path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        path = %file.path.display(),
                        %error,
                        "could not remove a frame from a capture that failed part-way"
                    );
                }
            }
        }

        let dir: PathBuf = request.dir.clone();
        let stem = request.stem.clone();
        let files = expected.to_vec();
        // Queued rather than done here so it cannot race a download still in flight on the
        // thread — the queue is the only ordering guarantee this driver has.
        let _ = link
            .request(OpClass::Config, move |reply| CamCmd::Sweep {
                dir,
                stem,
                files,
                reply,
            })
            .await;
    }
}

/// What the recovery loop does to this driver — SDD §5.3.1's respawn, from the loop's side.
#[async_trait]
impl Relink for Inner {
    async fn tear_down(&self, abandon: bool) {
        let link = self
            .link
            .lock()
            .expect("the camera link slot is never poisoned")
            .take();
        let Some(link) = link else {
            return;
        };

        if abandon {
            // A wedged thread is inside a libgphoto2 call with no cancellation. `shutdown` joins,
            // and joining here would park the recovery loop — and therefore the camera — behind a
            // call that may never return. `wedge` has already taken the handle, so dropping the
            // `Arc` joins nothing.
            drop(link);
            return;
        }

        // The thread is healthy; only the camera left. Waiting for it to release the context is
        // not merely safe here, it is required: the fresh attempt is about to ask for a USB claim
        // that the old context still holds, and M2-T02's own `Drop` note is about exactly that
        // race. `shutdown` is blocking-free — it joins a thread that is already returning.
        tokio::task::spawn_blocking(move || link.shutdown())
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the camera teardown task did not finish");
            });
    }

    async fn rebuild(&self) -> Result<(), DeviceError> {
        // `link_or_spawn` builds a *fresh* thread and, through the factory, a fresh context —
        // which M2-T01 measured as the only thing that recovers (108 ms), against a stale handle
        // that failed all five retries.
        let link = self.link_or_spawn()?;
        let raw = link.request(OpClass::Connect, CamCmd::Open).await?;
        // Re-read rather than replayed, for the same reason a redundant `connect` re-reads: the
        // camera has been away, and an operator who fixed a stalled session by turning the mode
        // dial has changed what the body offers.
        self.remember(interpret_identity(raw, BodyGeometry::R10));
        Ok(())
    }
}

/// Where the live-view pump gets the link it should use *this tick*.
///
/// Asking per tick rather than holding one is what lets live view survive a recovery: the link is
/// replaced wholesale when the camera comes back, and a pump holding the old one would talk to an
/// abandoned thread for the rest of the night.
impl LinkSource for Inner {
    fn link(&self) -> Option<Arc<CameraLink>> {
        Inner::link(self).ok()
    }
}

/// How the shutter is opened for one exposure.
///
/// Two variants rather than an `Option<Duration>` because they are two different mechanisms, not
/// one mechanism with a parameter: a timed capture is a single libgphoto2 call that returns when
/// the frame is ready, and a bulb exposure is a press, a wait this driver times itself, and a
/// release (SDD §5.3.2).
#[derive(Debug, Clone, Copy)]
enum Trigger {
    /// Fire at the shutter speed already set on the body.
    Timed,
    /// Hold the shutter open for this long.
    Bulb(Duration),
}

/// Turns a failed trigger into the error the caller should see.
///
/// An abort raised while the thread was inside a C call surfaces as whatever the body said —
/// often nothing useful — so the raised signal is what decides: if the operator asked for a stop,
/// they get `Aborted`, not a transport error describing the shutter they interrupted.
fn after_a_failed_exposure(link: &CameraLink, since: u64, error: DeviceError) -> DeviceError {
    if matches!(error, DeviceError::Aborted(_)) || link.aborted_since(since) {
        return aborted();
    }
    error
}

/// The error an abandoned capture resolves with.
///
/// `Aborted` (`ABORTED`/409), not `Rejected` (`DEVICE_REJECTED`/422): the operator asked for the
/// stop and got it, which is a completed instruction rather than a refused one. This matches the
/// simulator verbatim so that the FSM above cannot tell the two drivers apart, and it is the
/// point on which HAL-03's own prose is still wrong — `Camera::abort_capture`'s doc comment
/// promises `Rejected`. The simulator recorded the same disagreement in M1-T06; two drivers now
/// agree with each other and not with the trait, which is the evidence for correcting the trait
/// rather than the drivers.
fn aborted() -> DeviceError {
    DeviceError::Aborted("the capture was aborted by the operator".to_owned())
}

/// The message an operator gets when they ask for a value the body does not offer.
///
/// Names what was asked for and what is available, because "invalid ISO" sends someone to the
/// manual while "the camera offers 100, 125, 160 …" answers the question. Truncated, since some
/// lists have 27 entries and an error is not a settings UI.
fn rejection(label: &str, value: &str, offers: &[String]) -> String {
    const SHOWN: usize = 8;
    let listed = offers
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let more = offers.len().saturating_sub(SHOWN);
    if offers.is_empty() {
        return format!("the camera offers no {label} settings at all, so `{value}` cannot be set");
    }
    if more == 0 {
        format!("the camera does not offer {label} `{value}`; it offers {listed}")
    } else {
        format!("the camera does not offer {label} `{value}`; it offers {listed} and {more} more")
    }
}

/// Refuses a `camera.ops_via_cli` list, because this driver has no CLI path to route it to.
///
/// **Why there is no `GPhoto2Cli`.** SDD §5.3.3 designs one, and M2-T01 then went and measured
/// the thing it exists to work around: every operation this driver needs — settings, timed
/// capture, CR3 download, bulb, live view, battery, storage — works through the bindings on the
/// reference body. M2-T03 re-ran capture, bulb, download and abort against the same camera and
/// found the same. A `GPhoto2Cli` would therefore be a second implementation of every operation,
/// with its own subprocess handling, its own output parsing and its own timeouts, that no
/// configuration in the project selects and no test exercises against hardware. That is not
/// insurance, it is a second driver to keep working — and the one thing worse than not having a
/// fallback is having one that has never been run.
///
/// **Why the key is refused rather than ignored.** The config field is real, validated and
/// documented, so an operator can set it; if the driver silently did nothing, `ops_via_cli:
/// ["bulb"]` would look like it had taken effect and the operator would conclude the CLI path was
/// being used when it was not. Refusing costs one message and answers the question. If a future
/// body needs the fallback, this function is what it deletes.
fn no_cli_fallback(ops_via_cli: &[astroctl_core::config::CameraOp]) -> Option<DriverInitError> {
    if ops_via_cli.is_empty() {
        return None;
    }
    let listed = ops_via_cli
        .iter()
        .map(|op| format!("{op:?}").to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(", ");
    Some(DriverInitError::new(format!(
        "`camera.ops_via_cli` lists {listed}, but this driver has no `gphoto2` command-line \
         fallback to route them through. The M2-T01 spike measured every operation working \
         through the bindings on the reference body — bulb included — so the fallback SDD §5.3.3 \
         designs was never built rather than built and left untested. Set `camera.ops_via_cli: \
         []`."
    )))
}

// `not_yet_implemented` lived here from M2-T02 until M2-T04, and its own documentation predicted
// its removal: "every one of these disappears in M2-T03/T04; none of them is a design". Live view
// was the last caller, so it is gone. The reasoning it recorded is worth keeping even though the
// function is not, because the next driver in this crate will face the same question:
// `DeviceError`'s nine variants are all about the *device*, and none of them can say "this driver
// is not finished yet" — `Unsupported` claims the body cannot do it, contradicting the capability
// report two lines away, and `Rejected` blames the operator's request. `Protocol` was the least
// wrong, with the message carrying what the variant could not. SDD §5.3.1's M2-T02 obligation 5
// records the same decision, and both records now describe a state this driver has left.

/// The `Camera` surface, written against the shared state rather than against the handle.
///
/// These are inherent rather than the trait implementation because two of them
/// ([`connect`](Self::connect) and [`live_view_stream`](Self::live_view_stream)) need an
/// `Arc<Inner>` to hand to a background task, and a trait method cannot ask for one. The trait
/// impl below is the delegation, and is deliberately nothing else — every behaviour lives here,
/// where the recovery loop and the live-view pump can reach it too.
impl Inner {
    async fn connect(self: &Arc<Self>) -> Result<(), DeviceError> {
        // Started here rather than in `new`, because SDD §8.1 forbids side effects in registry
        // construction and because there may be no runtime until now. It outlives every
        // individual connect: a camera that is unplugged, reconnected and unplugged again is one
        // recovery loop, not three.
        self.start_recovery();

        let link = self.link_or_spawn()?;
        // Re-read on every connect, including a redundant one: the trait says connecting an
        // already-connected camera is `Ok`, and the cheapest correct way to honour that is to
        // ask the camera again. A cached identity replayed after the operator turned the mode
        // dial would describe a body that no longer exists.
        let raw = link.request(OpClass::Connect, CamCmd::Open).await?;
        self.remember(interpret_identity(raw, BodyGeometry::R10));
        // Also the reset for a `Faulted` driver: an operator who released the gvfs mount and
        // pressed *connect* gets a working camera, not a driver still sulking about the last one.
        self.note(LinkState::Connected);
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        // Before the link goes, and unconditionally: the pump holds a sink whose subscribers must
        // see the stream end, and the body must be told to leave live view while there is still a
        // camera to tell. Dropping this after the link would leave the sensor powered.
        self.stop_live_view_pump().await;

        let link = {
            let mut slot = self
                .link
                .lock()
                .expect("the camera link slot is never poisoned");
            slot.take()
        };
        let Some(link) = link else {
            // Disconnecting a disconnected camera is `Ok(())`.
            self.note(LinkState::Disconnected);
            return Ok(());
        };

        // Best effort: a camera that will not answer `Close` still has to be let go of, and the
        // thread teardown below releases the claim either way.
        let closed = link.request(OpClass::Config, CamCmd::Close).await;
        link.shutdown();
        *self
            .cached
            .lock()
            .expect("the camera cache is never poisoned") = Cached::unconnected();
        self.note(LinkState::Disconnected);
        closed
    }

    async fn settings(&self) -> Result<CameraSettings, DeviceError> {
        let raw = self
            .link()?
            .request_unless_capturing(OpClass::Config, CamCmd::GetSettings)
            .await?;
        Ok(interpret_settings(raw))
    }

    async fn available_settings(&self) -> Result<AvailableSettings, DeviceError> {
        let link = self.link()?;
        self.refresh_choices(&link).await?;
        Ok(self
            .cached
            .lock()
            .expect("the camera cache is never poisoned")
            .available
            .clone())
    }

    async fn set_iso(&self, iso: &str) -> Result<(), DeviceError> {
        self.set_enumerated(CfgKey::Iso, "ISO", iso).await
    }

    async fn set_shutter(&self, shutter: &str) -> Result<(), DeviceError> {
        self.set_enumerated(CfgKey::Shutter, "shutter speed", shutter)
            .await
    }

    async fn set_aperture(&self, aperture: &str) -> Result<(), DeviceError> {
        // A fully manual lens reports no aperture choices at all, and the trait calls that
        // `Unsupported` rather than a rejection: nothing the operator types would work, because
        // the body has no electronic control over that lens.
        if self.cached_choices(CfgKey::Aperture).is_empty() {
            return Err(DeviceError::Unsupported);
        }
        self.set_enumerated(CfgKey::Aperture, "aperture", aperture)
            .await
    }

    async fn set_image_format(&self, format: ImageFormat) -> Result<(), DeviceError> {
        let link = self.link()?;
        let choices = self.cached_choices(CfgKey::ImageFormat);
        // The token is resolved from the body's own vocabulary; a format it does not spell is
        // `Unsupported`, never the nearest entry.
        let Some(token) = format_token(format, &choices) else {
            return Err(DeviceError::Unsupported);
        };
        let value = token.to_owned();
        link.request_unless_capturing(OpClass::Config, move |reply| CamCmd::SetSetting {
            key: CfgKey::ImageFormat,
            value,
            reply,
        })
        .await
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, DeviceError> {
        // The shutter token decides whether this call is even the right one. `bulb` names no
        // duration, so a body left on it has nothing to fire at — and the honest answer is the
        // one the simulator gives, word for word, because the FSM above must not be able to tell
        // the two drivers apart. On the R10 this is not a corner case: with the physical mode
        // dial on Bulb the body offers `bulb` and nothing else (M2-T01, reconfirmed on hardware
        // for this task), so it is what a plain capture meets whenever the dial is left there.
        let shutter = self.settings().await?.shutter;
        let Some(seconds) = shutter_seconds(&shutter) else {
            return Err(DeviceError::Rejected(
                "the shutter is set to `bulb`; use capture_bulb, which carries a duration"
                    .to_owned(),
            ));
        };
        let exposure = Duration::try_from_secs_f64(seconds).map_err(|_| {
            DeviceError::Rejected(format!(
                "the camera reports a shutter speed this driver cannot time: `{shutter}`"
            ))
        })?;

        self.expose(request, exposure, shutter, Trigger::Timed)
            .await
    }

    async fn capture_bulb(
        &self,
        request: &CaptureRequest,
        duration: Duration,
    ) -> Result<CaptureResult, DeviceError> {
        // Derived from the body's own shutter list at connect time, never assumed: `has_bulb` is
        // true because the camera listed `bulb` among its shutters (see `capabilities_from`).
        if !self.capabilities().has_bulb {
            return Err(DeviceError::Unsupported);
        }
        self.expose(
            request,
            duration,
            "bulb".to_owned(),
            Trigger::Bulb(duration),
        )
        .await
    }

    async fn abort_capture(&self) -> Result<(), DeviceError> {
        // Two steps, and only the first of them is the abort.
        //
        // Raising the signal is what reaches a thread that is mid-exposure, because a command
        // could not: there is one queue, so a queued abort would be serviced *after* the capture
        // it was meant to stop. This returns immediately whether or not anything is running,
        // which is what SDD §5.8.1 requires of a stopping command.
        let Ok(link) = self.link() else {
            // No link is the strongest possible guarantee that the shutter is closed.
            return Ok(());
        };
        link.raise_abort();

        // The second step is the safety net: the retry for the one failure the spike flagged as
        // leaving the shutter open — a bulb release that itself failed.
        //
        // Sent through the gate, which is what makes it safe to send at all. While a capture is
        // running the gate answers `Busy` without queueing, so this cannot end up behind the
        // exposure it was meant to stop and cannot blow `config_seconds` and wedge the thread.
        // When the camera is idle it goes straight through.
        //
        // Best effort either way: the camera being unreachable, or busy, is not a reason to tell
        // the operator their stop failed when the signal above has already been raised.
        let _ = link
            .request_unless_capturing(OpClass::Config, CamCmd::Abort)
            .await;
        Ok(())
    }

    async fn live_view_frame(&self) -> Result<LiveViewFrame, DeviceError> {
        // A one-shot preview — a focus check — which is one gated round trip and no loop. Gated
        // for the same reason every tick of the stream is: a preview that queued behind a 300 s
        // bulb exposure would blow `config_seconds`, and a timed-out command is by definition a
        // wedged thread. The trait already documents `Busy` during a capture as the right answer.
        let jpeg = self
            .link()?
            .request_unless_capturing(OpClass::Config, CamCmd::Preview)
            .await?;
        Ok(LiveViewFrame::new(jpeg, Utc::now()))
    }

    async fn live_view_stream(self: &Arc<Self>) -> Result<FrameStream<LiveViewFrame>, DeviceError> {
        // Refused before anything is started, rather than by a pump that finds out on its first
        // tick: a body whose abilities report no preview operation cannot serve this at all, and
        // `has_live_view` is read off those abilities at connect rather than assumed.
        if !self.capabilities().has_live_view {
            return Err(DeviceError::Unsupported);
        }
        // Establishes that there is a camera. The pump tolerates the link going away afterwards —
        // that is the point of it — but starting one against a driver that has never connected
        // would answer a `NotConnected` caller with a stream that silently never produces.
        let _link = self.link()?;

        let mut live = self
            .live
            .lock()
            .expect("the camera live-view slot is never poisoned");
        // Idempotent: two calls are two cursors on one stream, never two sensor loops. The camera
        // has one imaging path, and two pumps would simply take turns blocking each other on the
        // single queue.
        let session = live.get_or_insert_with(|| {
            LiveView::start(Arc::clone(self) as Arc<dyn LinkSource>, self.live_view_fps)
        });
        Ok(session.subscribe())
    }

    async fn stop_live_view(&self) -> Result<(), DeviceError> {
        self.stop_live_view_pump().await;
        Ok(())
    }

    async fn battery(&self) -> Result<BatteryStatus, DeviceError> {
        self.link()?
            .request_unless_capturing(OpClass::Config, CamCmd::GetBattery)
            .await
    }

    async fn storage(&self) -> Result<StorageInfo, DeviceError> {
        self.link()?
            .request_unless_capturing(OpClass::Config, CamCmd::GetStorage)
            .await
    }

    async fn sensor_temperature_celsius(&self) -> Result<Option<f64>, DeviceError> {
        // No round trip on purpose. The R10's 91-entry config tree has no sensor-temperature key
        // (M2-T01 dumped it), and the trait is explicit that `None` is the routine answer for a
        // body that does not report one rather than an error. Sending a command that could only
        // ever come back empty would spend a thread round trip and a timeout budget to learn
        // something already known.
        Ok(None)
    }

    fn capabilities(&self) -> CameraCapabilities {
        self.cached
            .lock()
            .expect("the camera cache is never poisoned")
            .capabilities
            .clone()
    }

    fn device_info(&self) -> DeviceInfo {
        self.cached
            .lock()
            .expect("the camera cache is never poisoned")
            .info
            .clone()
    }
}

/// Delegation, and deliberately nothing more.
///
/// Every behaviour is on [`Inner`] so that the recovery loop and the live-view pump — which hold
/// an `Arc<Inner>` and not a `CanonGPhoto2Camera` — reach the same code the trait does. A method
/// that grew a second implementation here would be one the background loops could not see.
#[async_trait]
impl Camera for CanonGPhoto2Camera {
    async fn connect(&self) -> Result<(), DeviceError> {
        self.inner.connect().await
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        self.inner.disconnect().await
    }

    async fn settings(&self) -> Result<CameraSettings, DeviceError> {
        self.inner.settings().await
    }

    async fn available_settings(&self) -> Result<AvailableSettings, DeviceError> {
        self.inner.available_settings().await
    }

    async fn set_iso(&self, iso: &str) -> Result<(), DeviceError> {
        self.inner.set_iso(iso).await
    }

    async fn set_shutter(&self, shutter: &str) -> Result<(), DeviceError> {
        self.inner.set_shutter(shutter).await
    }

    async fn set_aperture(&self, aperture: &str) -> Result<(), DeviceError> {
        self.inner.set_aperture(aperture).await
    }

    async fn set_image_format(&self, format: ImageFormat) -> Result<(), DeviceError> {
        self.inner.set_image_format(format).await
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, DeviceError> {
        self.inner.capture(request).await
    }

    async fn capture_bulb(
        &self,
        request: &CaptureRequest,
        duration: Duration,
    ) -> Result<CaptureResult, DeviceError> {
        self.inner.capture_bulb(request, duration).await
    }

    async fn abort_capture(&self) -> Result<(), DeviceError> {
        self.inner.abort_capture().await
    }

    async fn live_view_frame(&self) -> Result<LiveViewFrame, DeviceError> {
        self.inner.live_view_frame().await
    }

    async fn live_view_stream(&self) -> Result<FrameStream<LiveViewFrame>, DeviceError> {
        self.inner.live_view_stream().await
    }

    async fn stop_live_view(&self) -> Result<(), DeviceError> {
        self.inner.stop_live_view().await
    }

    async fn battery(&self) -> Result<BatteryStatus, DeviceError> {
        self.inner.battery().await
    }

    async fn storage(&self) -> Result<StorageInfo, DeviceError> {
        self.inner.storage().await
    }

    async fn sensor_temperature_celsius(&self) -> Result<Option<f64>, DeviceError> {
        self.inner.sensor_temperature_celsius().await
    }

    fn capabilities(&self) -> CameraCapabilities {
        self.inner.capabilities()
    }

    fn device_info(&self) -> DeviceInfo {
        self.inner.device_info()
    }
}

// -----------------------------------------------------------------------------------------
// Registry factory
// -----------------------------------------------------------------------------------------

/// Builds [`CanonGPhoto2Camera`]s for the driver registry under the name `"gphoto2"` (HAL-07).
///
/// **Registered whether or not this build has libgphoto2**, and that is deliberate. Without the
/// backend the factory could simply not be registered — and an operator whose config says
/// `camera.driver: gphoto2` would then read `unknown driver "gphoto2", available: simulator`,
/// which says the driver does not exist. It does; this binary was built without the library it
/// needs. Registering and failing in [`create`](CameraFactory::create) is one more line and says
/// the true thing.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanonGPhoto2CameraFactory;

impl CanonGPhoto2CameraFactory {
    /// Builds the factory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Builds the driver as its concrete type, for a caller that needs more than `dyn Camera`.
    ///
    /// **This exists for exactly one thing that the `Camera` trait cannot express**: the REL-03
    /// link state. SDD §5.3.1 requires the recovery transition to be surfaced as
    /// `camera.status: reconnecting`, and a driver may not publish events (crate docs: "drivers
    /// are silent"), so the facade has to *read* it — through
    /// [`CanonGPhoto2Camera::link_state`] and [`watch_link_state`](CanonGPhoto2Camera::watch_link_state),
    /// neither of which is on the trait.
    ///
    /// Adding a state accessor to `Camera` instead was the alternative and is the wrong shape
    /// today: it would oblige every driver — the simulator, the guide cameras, the INDI adapter
    /// when it lands — to model a recovery protocol only this one has. If a second driver grows
    /// one, that is the moment the trait should learn about it.
    ///
    /// # Errors
    /// As [`create`](CameraFactory::create).
    #[cfg(feature = "libgphoto2")]
    pub fn create_gphoto2(
        &self,
        config: &CameraConfig,
    ) -> Result<Arc<CanonGPhoto2Camera>, DriverInitError> {
        if let Some(error) = no_cli_fallback(&config.ops_via_cli) {
            return Err(error);
        }
        Ok(Arc::new(CanonGPhoto2Camera::new(
            config,
            Arc::new(super::backend::LibGphoto2Factory::new()),
        )))
    }
}

#[async_trait]
impl CameraFactory for CanonGPhoto2CameraFactory {
    fn name(&self) -> &'static str {
        DRIVER_NAME
    }

    fn create(&self, config: &CameraConfig) -> Result<Arc<dyn Camera>, DriverInitError> {
        // Refused rather than ignored — see `no_cli_fallback` for why there is nothing to route
        // to, and why silently accepting the key would be the worse of the two answers.
        if let Some(error) = no_cli_fallback(&config.ops_via_cli) {
            return Err(error);
        }

        #[cfg(feature = "libgphoto2")]
        {
            Ok(Arc::new(CanonGPhoto2Camera::new(
                config,
                Arc::new(super::backend::LibGphoto2Factory::new()),
            )))
        }
        #[cfg(not(feature = "libgphoto2"))]
        {
            Err(DriverInitError::new(
                "this build of astroctl has no libgphoto2 support, so `camera.driver: gphoto2` \
                 cannot be used. Rebuild with `--features astroctl-drivers/libgphoto2` on a \
                 machine that has libgphoto2-dev installed, or set `camera.driver: simulator`.",
            ))
        }
    }

    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        #[cfg(feature = "libgphoto2")]
        {
            super::backend::probe_cameras().await
        }
        #[cfg(not(feature = "libgphoto2"))]
        {
            // Not an error: a build without the backend cannot see cameras, and HAL-08's scan
            // reporting nothing is the honest result. `create` is where the operator learns why.
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use astroctl_core::config::{CameraConfig, CameraDriver, CameraTimeouts};
    use astroctl_core::error::DeviceError;
    use astroctl_core::types::ImageFormat;
    use astroctl_hal::camera::{Camera, CaptureRequest};
    use astroctl_hal::registry::{CameraFactory, DriverRegistry};

    use super::super::mock::{mock_format_token, MockState};
    use super::super::thread::OpClass;
    use super::{CanonGPhoto2Camera, CanonGPhoto2CameraFactory, LinkState, DRIVER_NAME};

    /// `config/field-node.example.yaml`'s camera section, verbatim.
    fn config() -> CameraConfig {
        CameraConfig {
            driver: CameraDriver::Gphoto2,
            default_iso: "1600".to_owned(),
            default_shutter: "30".to_owned(),
            default_format: "RAW+JPEG".to_owned(),
            ops_via_cli: Vec::new(),
            timeouts: CameraTimeouts {
                config_seconds: 5,
                capture_extra_seconds: 30,
                download_seconds: 120,
            },
            live_view_fps: 5,
            indi_device: None,
        }
    }

    /// A driver wired to a mock body, and the handle that scripts it.
    fn camera() -> (Arc<super::super::mock::MockState>, CanonGPhoto2Camera) {
        let (state, factory) = MockState::new();
        (state, CanonGPhoto2Camera::new(&config(), factory))
    }

    #[tokio::test]
    async fn connecting_reads_the_body_rather_than_assuming_it() {
        let (_state, camera) = camera();

        // Before connecting, the capability report is the reference body's documented set with
        // the enumerated lists empty — because those are the ones that genuinely vary.
        assert!(camera.capabilities().supported_formats.is_empty());
        assert_eq!(camera.device_info().model, "unknown until connected");

        camera.connect().await.expect("connects");

        let info = camera.device_info();
        assert_eq!(info.model, "Canon EOS R10");
        assert_eq!(info.serial.as_deref(), Some("0123456789"));

        let caps = camera.capabilities();
        assert_eq!(caps.min_iso, 100);
        assert_eq!(caps.max_iso, 3200);
        assert!(caps.has_bulb, "the body listed `bulb` among its shutters");
        assert_eq!(caps.sensor_width_px, 6000);
        assert!(caps.supported_formats.contains(&ImageFormat::RawPlusJpeg));
    }

    #[tokio::test]
    async fn connecting_twice_is_ok_and_re_reads_the_body() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        camera.connect().await.expect("connecting again is Ok");

        let opens = state.calls().iter().filter(|c| c.op == "open").count();
        assert_eq!(
            opens, 2,
            "a redundant connect must re-read, not replay a cache — the mode dial may have moved"
        );
    }

    #[tokio::test]
    async fn a_settings_round_trip_reports_what_the_body_holds() {
        // The acceptance criterion's shape: set a value through the API, read it back from the
        // camera. On hardware this is the same sequence with a real body underneath.
        let (state, camera) = camera();
        camera.connect().await.expect("connects");

        assert_eq!(camera.settings().await.expect("reads").iso, "1600");

        camera.set_iso("800").await.expect("800 is offered");
        assert_eq!(camera.settings().await.expect("reads").iso, "800");
        // Read back from the mock body itself, not from anything the driver remembered.
        assert_eq!(state.settings().iso, "800");

        camera.set_shutter("1/4000").await.expect("offered");
        camera.set_aperture("1.4").await.expect("offered");
        let settings = camera.settings().await.expect("reads");
        assert_eq!(settings.shutter, "1/4000");
        assert_eq!(settings.aperture.as_deref(), Some("1.4"));
    }

    #[tokio::test]
    async fn a_value_the_body_does_not_offer_is_refused_and_the_message_says_what_is() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");

        let error = camera
            .set_iso("999999")
            .await
            .expect_err("the body offers no such ISO");
        let DeviceError::Rejected(message) = error else {
            panic!("a value outside the body's list is Rejected, never substituted");
        };
        assert!(message.contains("999999"), "{message}");
        assert!(
            message.contains("100"),
            "the offered values are named: {message}"
        );

        // And nothing was written.
        assert_eq!(state.settings().iso, "1600");
    }

    #[tokio::test]
    async fn a_token_the_cache_has_not_seen_triggers_one_re_read_before_being_refused() {
        // The mode-dial case. The body starts offering a shutter speed *after* the driver cached
        // its list; refusing it from a stale cache would be a false rejection the operator can
        // do nothing about.
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        state.offer_shutter("1/8000");

        camera
            .set_shutter("1/8000")
            .await
            .expect("the re-read finds the newly offered token");
        assert_eq!(state.settings().shutter, "1/8000");

        let reads = state
            .calls()
            .iter()
            .filter(|c| c.op == "read_choices")
            .count();
        assert_eq!(
            reads, 1,
            "exactly one re-read, and only because a refusal loomed"
        );
    }

    #[tokio::test]
    async fn the_happy_path_does_not_re_read_the_config_tree() {
        // 222 ms per tree walk (M2-T01). Paying that on every ISO change would make the settings
        // UI feel broken.
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        camera.set_iso("800").await.expect("offered");
        camera.set_iso("3200").await.expect("offered");

        assert_eq!(
            state
                .calls()
                .iter()
                .filter(|c| c.op == "read_choices")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn available_settings_are_the_bodys_lists_re_read_from_the_body() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        state.offer_shutter("1/8000");

        let available = camera.available_settings().await.expect("reads");
        assert!(
            available.shutters.contains(&"1/8000".to_owned()),
            "the list must come from the camera, not from a cache: {available:?}"
        );
        assert_eq!(available.isos, ["100", "800", "1600", "3200"]);
    }

    #[tokio::test]
    async fn a_manual_lens_has_no_aperture_control_and_says_so() {
        let (state, camera) = camera();
        state.remove_aperture_control();
        camera.connect().await.expect("connects");

        let error = camera.set_aperture("5.6").await.expect_err("no control");
        assert!(
            matches!(error, DeviceError::Unsupported),
            "a manual lens is Unsupported, not Rejected: {error:?}"
        );
    }

    #[tokio::test]
    async fn setting_a_format_writes_the_bodys_own_token_for_it() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");

        camera
            .set_image_format(ImageFormat::Raw)
            .await
            .expect("the body offers RAW");
        assert_eq!(
            state.settings().format,
            mock_format_token(ImageFormat::Raw).expect("a token")
        );
        assert_eq!(
            camera.settings().await.expect("reads").format,
            ImageFormat::Raw
        );
    }

    #[tokio::test]
    async fn every_operation_is_not_connected_before_connect_and_after_disconnect() {
        let (_state, camera) = camera();
        assert!(matches!(
            camera.settings().await,
            Err(DeviceError::NotConnected)
        ));

        camera.connect().await.expect("connects");
        camera.settings().await.expect("reads");

        camera.disconnect().await.expect("disconnects");
        assert!(matches!(
            camera.settings().await,
            Err(DeviceError::NotConnected)
        ));
        assert!(matches!(
            camera.set_iso("800").await,
            Err(DeviceError::NotConnected)
        ));
        // Disconnecting a disconnected camera is Ok(()).
        camera.disconnect().await.expect("idempotent");
    }

    #[tokio::test]
    async fn disconnecting_releases_the_camera_on_the_camera_thread() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        camera.disconnect().await.expect("disconnects");

        assert_eq!(state.drop_threads(), vec!["astroctl-camera".to_owned()]);
        // And the capability report goes back to the unconnected answer rather than describing
        // a body that is no longer attached.
        assert_eq!(camera.device_info().model, "unknown until connected");
    }

    #[tokio::test]
    async fn battery_and_storage_come_from_the_body() {
        let (_state, camera) = camera();
        camera.connect().await.expect("connects");

        assert_eq!(camera.battery().await.expect("battery").percent, 100);
        assert_eq!(camera.storage().await.expect("storage").free_mb, 69_500);
        // A DSLR reports no sensor temperature, and the trait calls `None` the routine answer.
        assert_eq!(
            camera
                .sensor_temperature_celsius()
                .await
                .expect("not an error"),
            None
        );
    }

    #[tokio::test]
    async fn reading_the_sensor_temperature_costs_no_round_trip() {
        // There is no such key in the R10's config tree, so asking would spend a thread hop and
        // a timeout budget to learn something already known.
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        let before = state.calls().len();
        camera.sensor_temperature_celsius().await.expect("None");
        assert_eq!(state.calls().len(), before);
    }

    #[tokio::test]
    async fn every_operation_the_trait_declares_is_implemented() {
        // This test used to be `the_operations_this_task_does_not_implement_name_the_task_that_will`
        // and asserted that live view answered with a message naming M2-T04. It is M2-T04, so it
        // now asserts the opposite: nothing in the `Camera` trait answers "not implemented".
        let (_state, camera) = camera();
        camera.connect().await.expect("connects");

        camera.live_view_frame().await.expect("a preview frame");
        let stream = camera.live_view_stream().await.expect("a stream");
        drop(stream);
        camera.stop_live_view().await.expect("stops");

        // The placeholder itself is gone rather than merely unreachable — the compiler is the
        // check, since `not_yet_implemented` was a private function and a dead one would fail the
        // `-D warnings` clippy gate.
    }

    // -------------------------------------------------------------------------------------
    // Capture, bulb, download and abort (M2-T03)
    // -------------------------------------------------------------------------------------

    /// A path inside the process's temporary directory, unique to this test.
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astroctl-gpcam-{}-{name}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("a writable temporary directory");
        dir
    }

    /// Every entry in a directory, by name, sorted.
    fn listing(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("reads the directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// A connected camera whose body is on a timed shutter rather than bulb.
    async fn timed_camera() -> (Arc<super::super::mock::MockState>, CanonGPhoto2Camera) {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        camera.set_shutter("30").await.expect("30 s is offered");
        (state, camera)
    }

    #[tokio::test]
    async fn a_timed_capture_lands_a_durable_frame_named_from_the_request() {
        let (_state, camera) = timed_camera().await;
        let dir = scratch_dir("timed");
        let request = CaptureRequest::new(&dir, "light_00042");

        let result = camera.capture(&request).await.expect("captures");

        let raw = result.raw().expect("a science file");
        assert_eq!(raw.path, dir.join("light_00042.cr3"));
        assert!(
            raw.path.exists(),
            "the frame must be on disk when capture returns"
        );
        assert_eq!(raw.kind, astroctl_hal::camera::CapturedFileKind::Raw);
        assert!(raw.size_bytes > 0);
        assert_eq!(result.total_bytes(), raw.size_bytes);

        // Nothing else in the directory: no temporary survives a successful capture.
        assert_eq!(listing(&dir), vec!["light_00042.cr3".to_owned()]);

        // The settings are read back from the body, not echoed from the request.
        assert_eq!(result.settings.shutter, "30");
        assert_eq!(result.settings.iso, "1600");
        assert_eq!(result.exposure, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn a_raw_plus_jpeg_result_lists_the_raw_first_whatever_order_the_body_used() {
        // Scripted in the order the R10 actually produced on hardware: shooting RAW+JPEG, the
        // trigger call returned the *JPEG* and the CR3 arrived as the `NewFile` event. Taking the
        // body's order would put the JPEG at `files[0]`, against HAL-03's "raw first" — and
        // `raw()`/`jpeg()` search by kind, so nothing would have noticed until a consumer
        // iterated the list and treated the first entry as the science frame.
        let (state, camera) = timed_camera().await;
        state.capture_yields(&["capt0000.jpg", "capt0000.cr3"]);
        let dir = scratch_dir("rawjpeg-order");

        let result = camera
            .capture(&CaptureRequest::new(&dir, "light_pair"))
            .await
            .expect("captures");

        assert_eq!(result.files.len(), 2);
        assert_eq!(
            result.files[0].kind,
            astroctl_hal::camera::CapturedFileKind::Raw,
            "the science file must come first however the body ordered them"
        );
        assert_eq!(
            result.files[1].kind,
            astroctl_hal::camera::CapturedFileKind::Jpeg
        );
        assert_eq!(result.files[0].path, dir.join("light_pair.cr3"));
        assert_eq!(result.files[1].path, dir.join("light_pair.jpg"));
    }

    #[tokio::test]
    async fn a_raw_plus_jpeg_capture_lands_both_files_under_one_stem() {
        let (state, camera) = timed_camera().await;
        // The second file reaches the driver as an event rather than from the trigger call.
        state.capture_yields(&["capt0000.cr3", "capt0000.jpg"]);
        let dir = scratch_dir("rawjpeg");

        let result = camera
            .capture(&CaptureRequest::new(&dir, "light_7"))
            .await
            .expect("captures");

        assert_eq!(result.files.len(), 2);
        assert_eq!(result.raw().expect("raw").path, dir.join("light_7.cr3"));
        assert_eq!(result.jpeg().expect("jpeg").path, dir.join("light_7.jpg"));
        assert_eq!(
            listing(&dir),
            vec!["light_7.cr3".to_owned(), "light_7.jpg".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_body_left_on_bulb_is_pointed_at_capture_bulb_rather_than_hanging() {
        // Not a corner case on the R10: with the physical mode dial on Bulb the body offers
        // `bulb` and nothing else, which is the state this workstation's camera was in.
        let (_state, camera) = camera();
        camera.connect().await.expect("connects");
        camera.set_shutter("bulb").await.expect("bulb is offered");
        let dir = scratch_dir("on-bulb");

        let error = camera
            .capture(&CaptureRequest::new(&dir, "light_1"))
            .await
            .expect_err("a plain capture has no duration to fire for");

        let DeviceError::Rejected(message) = error else {
            panic!("the request was well formed; the body is simply on the wrong setting");
        };
        // Word for word what the simulator says, so the FSM above cannot tell them apart.
        assert!(message.contains("capture_bulb"), "{message}");
        assert_eq!(listing(&dir), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_bulb_exposure_is_duration_driven_and_reports_what_the_body_says() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        // The hold is a real blocking wait on a real OS thread — `tokio::time::pause` cannot
        // reach it, which is the same reason the wedge test uses real time. So the duration here
        // is short and the *shape* is what is asserted: the R10 reports `BulbExposureTime 9` for
        // a 10 s hold (M2-T01), a real second between what was asked for and what the sensor did.
        state.reports_exposure(0.2);
        let dir = scratch_dir("bulb");

        let result = camera
            .capture_bulb(
                &CaptureRequest::new(&dir, "light_9"),
                Duration::from_millis(300),
            )
            .await
            .expect("bulb exposure");

        assert_eq!(result.raw().expect("raw").path, dir.join("light_9.cr3"));
        // The camera's figure, not the request's.
        assert_eq!(result.exposure, Duration::from_millis(200));
        // A bulb result reports `bulb` as its shutter, which is both what the body shows and
        // what the simulator reports for the same exposure.
        assert_eq!(result.settings.shutter, "bulb");
        // The shutter opened and closed exactly once, and is closed now.
        assert_eq!(state.shutter_log(), vec!["open", "close"]);
        assert!(!state.shutter_is_open());
    }

    #[tokio::test]
    async fn a_bulb_exposure_falls_back_to_the_requested_duration_when_the_body_says_nothing() {
        let (_state, camera) = camera();
        camera.connect().await.expect("connects");
        let dir = scratch_dir("bulb-quiet");

        let result = camera
            .capture_bulb(
                &CaptureRequest::new(&dir, "light_1"),
                Duration::from_millis(200),
            )
            .await
            .expect("bulb exposure");

        assert_eq!(result.exposure, Duration::from_millis(200));
    }

    #[tokio::test]
    async fn an_abort_mid_bulb_returns_promptly_closes_the_shutter_and_leaves_nothing_on_disk() {
        // The acceptance behaviour, and the reason `AbortSignal` is not a `CamCmd`: a queued
        // abort would be serviced after the exposure it was meant to stop.
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        let dir = scratch_dir("bulb-abort");
        let camera = Arc::new(camera);

        let exposing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move {
                camera
                    .capture_bulb(
                        &CaptureRequest::new(&dir, "light_1"),
                        // Five minutes. The test must not take five minutes.
                        Duration::from_secs(300),
                    )
                    .await
            })
        };

        // Let the exposure actually start, so this is an abort *during* a bulb hold rather than
        // one that arrives before the shutter opens.
        while !state.shutter_is_open() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let raised = std::time::Instant::now();
        camera.abort_capture().await.expect("aborting never fails");
        let error = exposing.await.expect("task").expect_err("aborted");
        let took = raised.elapsed();

        assert!(
            matches!(error, DeviceError::Aborted(_)),
            "an operator's stop is `Aborted`, not `Rejected`: {error:?}"
        );
        assert!(
            took < Duration::from_secs(5),
            "an abort mid-bulb must return promptly, took {took:?} of a 300 s exposure"
        );
        // The shutter was released on the way out — the one failure here that damages more than
        // the frame.
        assert_eq!(state.shutter_log(), vec!["open", "close"]);
        assert!(!state.shutter_is_open());
        // And nothing reached the disk, temporaries included.
        assert_eq!(listing(&dir), Vec::<String>::new());
    }

    #[tokio::test]
    async fn an_abort_before_the_download_leaves_nothing_on_disk() {
        // The timed-capture half of the same contract. `capture_image` is a C call that cannot
        // be interrupted, so the abort cannot shorten the exposure — but it can and must stop
        // the frame reaching the disk.
        let (state, camera) = timed_camera().await;
        state.capture_takes(Duration::from_millis(300));
        let dir = scratch_dir("timed-abort");
        let camera = Arc::new(camera);

        let exposing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&CaptureRequest::new(&dir, "light_1")).await })
        };
        // Long enough to be inside the uninterruptible trigger.
        tokio::time::sleep(Duration::from_millis(100)).await;
        camera.abort_capture().await.expect("aborting never fails");

        let error = exposing.await.expect("task").expect_err("aborted");
        assert!(matches!(error, DeviceError::Aborted(_)), "{error:?}");
        assert_eq!(listing(&dir), Vec::<String>::new());
        // The frame was never downloaded — it stays in the camera and the next capture
        // overwrites it.
        assert_eq!(state.download_destinations().len(), 0);
    }

    #[tokio::test]
    async fn aborting_when_nothing_is_running_is_ok_and_reaches_the_body_as_a_safety_release() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");

        camera.abort_capture().await.expect("nothing to abort");

        // The queued half runs only when the thread is free, and it is: this is the retry for
        // the one failure the spike flagged as leaving the shutter open.
        assert!(
            state.calls().iter().any(|call| call.op == "abort"),
            "an abort with the camera idle should still ask the body to release the shutter"
        );
    }

    #[tokio::test]
    async fn aborting_a_disconnected_camera_is_ok() {
        // A stopping command may not fail for want of something to stop, and no link is the
        // strongest possible guarantee that the shutter is closed.
        let (_state, camera) = camera();
        camera.abort_capture().await.expect("nothing to abort");
    }

    #[tokio::test]
    async fn a_second_capture_while_one_is_running_is_busy_rather_than_queued() {
        let (state, camera) = timed_camera().await;
        state.capture_takes(Duration::from_millis(400));
        let dir = scratch_dir("busy");
        let camera = Arc::new(camera);

        let first = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&CaptureRequest::new(&dir, "light_1")).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;

        let error = camera
            .capture(&CaptureRequest::new(&dir, "light_2"))
            .await
            .expect_err("the imaging path is taken");
        assert!(matches!(error, DeviceError::Busy(_)), "{error:?}");

        first
            .await
            .expect("task")
            .expect("the first capture finishes");
        // And the claim is released, so the camera is usable again.
        camera
            .capture(&CaptureRequest::new(&dir, "light_3"))
            .await
            .expect("the claim was released");
    }

    #[tokio::test]
    async fn a_status_poll_during_an_exposure_is_refused_rather_than_wedging_the_camera() {
        // The failure this prevents is not exotic, it is the routine one: there is one queue, so
        // a `get_battery` issued during a 300 s bulb exposure would be serviced 300 seconds
        // later — against a 5 s `config_seconds` budget. It would therefore time out, and a
        // timed-out command *is* a wedged thread by definition, so one health tick would abandon
        // the camera thread and cost the frame that was exposing.
        let (state, camera) = timed_camera().await;
        state.capture_takes(Duration::from_millis(500));
        let dir = scratch_dir("poll-during");
        let camera = Arc::new(camera);

        let exposing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&CaptureRequest::new(&dir, "light_1")).await })
        };
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Every routine status read answers immediately instead of joining the queue.
        for result in [
            camera.battery().await.err(),
            camera.storage().await.err(),
            camera.settings().await.err(),
        ] {
            assert!(
                matches!(result, Some(DeviceError::Busy(_))),
                "a status read during an exposure must be refused, not queued: {result:?}"
            );
        }

        // And the exposure it would have wedged completes normally.
        exposing
            .await
            .expect("task")
            .expect("the capture survives concurrent status polling");
        assert!(
            camera.battery().await.is_ok(),
            "and the gate lifts afterwards"
        );
    }

    #[tokio::test]
    async fn an_abort_raised_while_a_capture_is_starting_is_not_lost() {
        // The generation must be read before the claim is taken, because `abort_capture` stops
        // sending its queued safety net the moment the claim exists. An abort landing in the
        // window between the claim and the generation read would be seen by nothing at all — and
        // reported to the operator as done while the exposure ran to completion.
        let (state, camera) = timed_camera().await;
        // Every mock call blocks for 200 ms, which lays the startup out in known windows:
        // `capture`'s own shutter read occupies 0–200 ms, and `expose`'s settings read — the
        // first thing after the claim — occupies 200–400 ms. Aborting at 300 ms therefore lands
        // squarely in the window this test is about: the claim exists, so `abort_capture` sends
        // no queued command and the raised generation is the only record there is.
        state.block_calls_for(Duration::from_millis(200));
        let dir = scratch_dir("abort-startup");
        let camera = Arc::new(camera);

        let exposing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&CaptureRequest::new(&dir, "light_1")).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        camera.abort_capture().await.expect("aborting never fails");

        let error = exposing.await.expect("task").expect_err("aborted");
        assert!(
            matches!(error, DeviceError::Aborted(_)),
            "an abort raised while the capture was starting must not be swallowed: {error:?}"
        );
        assert_eq!(listing(&dir), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_failed_download_leaves_no_partial_frame_visible() {
        let (state, camera) = timed_camera().await;
        state.fail_download_with("the camera was unplugged mid-transfer");
        let dir = scratch_dir("dl-fail");

        let error = camera
            .capture(&CaptureRequest::new(&dir, "light_1"))
            .await
            .expect_err("the download failed");
        assert!(matches!(error, DeviceError::Transport(_)), "{error:?}");

        assert_eq!(
            listing(&dir),
            Vec::<String>::new(),
            "a torn download must be invisible to every consumer (REL-05)"
        );
    }

    #[tokio::test]
    async fn a_capture_whose_second_file_fails_leaves_neither_file_behind() {
        // The RAW+JPEG half of REL-05: a frame that only half arrived is not a frame, and a
        // session directory showing a `.cr3` with no `.jpg` would be a complete-looking frame
        // that the preview path cannot use.
        let (state, camera) = timed_camera().await;
        state.capture_yields(&["capt0000.cr3", "capt0000.jpg"]);
        let dir = scratch_dir("dl-half");

        // Let the raw through, then fail: the failure is scripted after the first download has
        // already been recorded.
        state.fail_download_after(1, "the card was removed");

        let error = camera
            .capture(&CaptureRequest::new(&dir, "light_1"))
            .await
            .expect_err("the second file failed");
        assert!(matches!(error, DeviceError::Transport(_)), "{error:?}");

        assert_eq!(
            listing(&dir),
            Vec::<String>::new(),
            "the raw that already landed must be removed too"
        );
    }

    #[tokio::test]
    async fn a_body_that_refuses_to_fire_is_rejected_and_writes_nothing() {
        let (state, camera) = timed_camera().await;
        state.fail_capture_with("no card in the camera");
        let dir = scratch_dir("refused");

        let error = camera
            .capture(&CaptureRequest::new(&dir, "light_1"))
            .await
            .expect_err("the body refused");
        let DeviceError::Rejected(message) = error else {
            panic!("a body that will not fire is refusing, not failing to communicate");
        };
        assert!(message.contains("no card"), "{message}");
        assert_eq!(listing(&dir), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_bulb_the_body_refuses_never_opens_the_shutter() {
        let (state, camera) = camera();
        camera.connect().await.expect("connects");
        state.fail_bulb_with("the mode dial is not on Bulb");
        let dir = scratch_dir("bulb-refused");

        let error = camera
            .capture_bulb(
                &CaptureRequest::new(&dir, "light_1"),
                Duration::from_secs(2),
            )
            .await
            .expect_err("refused");
        assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");

        assert_eq!(
            state.shutter_log(),
            Vec::<&str>::new(),
            "a refused bulb must not have opened the shutter at all"
        );
        assert_eq!(listing(&dir), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_body_with_no_bulb_capability_says_unsupported() {
        let (state, camera) = camera();
        // A body whose shutter list has no `bulb` entry — which is what `has_bulb` is derived
        // from, rather than from a table of models.
        state.remove_bulb_shutter();
        camera.connect().await.expect("connects");
        assert!(!camera.capabilities().has_bulb);

        let error = camera
            .capture_bulb(
                &CaptureRequest::new(scratch_dir("no-bulb"), "light_1"),
                Duration::from_secs(1),
            )
            .await
            .expect_err("the body cannot do it");
        assert!(matches!(error, DeviceError::Unsupported), "{error:?}");
    }

    #[tokio::test]
    async fn a_download_that_never_returns_wedges_the_thread_at_the_download_budget() {
        // The wedge-shaped silence, on the operation whose budget is its own: the camera thread
        // is inside a C call that cannot be interrupted, so the link is abandoned.
        let (state, factory) = MockState::new();
        let config = CameraConfig {
            timeouts: CameraTimeouts {
                config_seconds: 5,
                capture_extra_seconds: 30,
                // One second is the validated minimum, and the cheapest honest demonstration.
                download_seconds: 1,
            },
            ..config()
        };
        let camera = CanonGPhoto2Camera::new(&config, factory);
        camera.connect().await.expect("connects");
        camera.set_shutter("30").await.expect("offered");
        state.download_takes(Duration::from_secs(3));

        let error = camera
            .capture(&CaptureRequest::new(scratch_dir("wedge"), "light_1"))
            .await
            .expect_err("the download exceeded its budget");
        match error {
            DeviceError::Timeout(budget) => assert_eq!(budget, Duration::from_secs(1)),
            other => panic!("expected a timeout at `download_seconds`, got {other:?}"),
        }

        // And the camera is no longer usable through this link — SDD §5.3.1's wedge protocol.
        assert!(matches!(
            camera.settings().await,
            Err(DeviceError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn the_capture_budget_covers_the_exposure_twice_over_plus_the_allowance() {
        // A 300 s bulb frame is not a wedged camera. The budget has to track the exposure or it
        // either abandons the thread mid-integration or never fires at all — and it has to track
        // it *twice*, because the body shoots a matching dark frame afterwards. Measured on the
        // R10: a 30 s bulb completes in 62.9 s and a 60 s bulb in 123.0 s.
        let timeouts = CameraTimeouts {
            config_seconds: 5,
            capture_extra_seconds: 30,
            download_seconds: 120,
        };
        assert_eq!(
            OpClass::Capture(Duration::from_secs(300)).budget(&timeouts),
            Duration::from_secs(630)
        );
        assert_eq!(
            OpClass::Capture(Duration::ZERO).budget(&timeouts),
            Duration::from_secs(30)
        );
        assert_eq!(
            OpClass::Download.budget(&timeouts),
            Duration::from_secs(120)
        );

        // The two measured frames, against the example config's own allowance: the budget must
        // clear the real figure with room, or the shipped default fails good frames. An earlier
        // version budgeted `exposure + allowance` and did exactly that to a 30 s bulb.
        assert!(
            OpClass::Capture(Duration::from_secs(30)).budget(&timeouts)
                > Duration::from_secs_f64(62.9),
            "the shipped default must accommodate a measured 30 s bulb frame"
        );
        assert!(
            OpClass::Capture(Duration::from_secs(60)).budget(&timeouts)
                > Duration::from_secs_f64(123.0),
            "the shipped default must accommodate a measured 60 s bulb frame"
        );
    }

    #[tokio::test]
    async fn the_bulb_file_wait_expires_before_the_budget_it_sits_inside() {
        // The ordering that makes the driver's own diagnosis reachable. If this wait outlived the
        // operation budget, "the camera had not announced a file" could never be returned — the
        // wedge protocol would fire first and the operator would read a bare timeout instead.
        let timeouts = CameraTimeouts {
            config_seconds: 5,
            capture_extra_seconds: 30,
            download_seconds: 120,
        };
        for seconds in [1_u64, 10, 30, 60, 300] {
            let exposure = Duration::from_secs(seconds);
            let wait = OpClass::bulb_file_wait(exposure, &timeouts);
            let budget = OpClass::Capture(exposure).budget(&timeouts);
            // The thread holds the shutter and only then starts waiting.
            assert!(
                exposure + wait < budget,
                "a {seconds} s exposure waits {wait:?} inside a {budget:?} budget, which the \
                 wedge protocol would cut short"
            );
            // And it must still allow the body a full dark-frame pass.
            assert!(
                wait > exposure,
                "a {seconds} s exposure must be allowed at least its own length of readout"
            );
        }
    }

    #[tokio::test]
    async fn capturing_without_a_camera_is_not_connected() {
        let (_state, camera) = camera();
        let dir = scratch_dir("disconnected");
        assert!(matches!(
            camera.capture(&CaptureRequest::new(&dir, "light_1")).await,
            Err(DeviceError::NotConnected)
        ));
        assert!(matches!(
            camera
                .capture_bulb(
                    &CaptureRequest::new(&dir, "light_1"),
                    Duration::from_secs(1)
                )
                .await,
            Err(DeviceError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn every_blocking_capture_call_runs_on_the_camera_thread() {
        // T-ISO-1's structural claim, extended to the operations this task added: a 2 s capture
        // and a 32 MB download on a runtime worker would be half the node's concurrency gone.
        let (state, camera) = timed_camera().await;
        camera
            .capture(&CaptureRequest::new(scratch_dir("iso"), "light_1"))
            .await
            .expect("captures");

        let blocking: Vec<_> = state
            .calls()
            .into_iter()
            .filter(|call| matches!(call.op, "capture" | "download" | "capture_bulb"))
            .collect();
        assert!(!blocking.is_empty());
        for call in blocking {
            assert_eq!(
                call.thread, "astroctl-camera",
                "`{}` ran on `{}`, not the camera thread",
                call.op, call.thread
            );
        }
    }

    #[test]
    fn a_configured_cli_fallback_is_refused_rather_than_silently_ignored() {
        // The key is real, validated and documented, so an operator can set it. Doing nothing
        // would look like it had taken effect.
        use astroctl_core::config::CameraOp;
        let config = CameraConfig {
            ops_via_cli: vec![CameraOp::Bulb],
            ..config()
        };
        let error = CanonGPhoto2CameraFactory::new()
            .create(&config)
            .expect_err("there is no CLI path to route bulb through");
        let message = error.to_string();
        assert!(message.contains("bulb"), "{message}");
        assert!(
            message.contains("ops_via_cli"),
            "the message must name the key to change: {message}"
        );
    }

    #[test]
    fn the_registry_name_is_the_one_configuration_selects() {
        // If these ever drift, `camera.driver: gphoto2` silently selects nothing.
        assert_eq!(DRIVER_NAME, CameraDriver::Gphoto2.as_str());
        assert_eq!(CanonGPhoto2CameraFactory::new().name(), DRIVER_NAME);
    }

    #[test]
    fn the_factory_registers_under_gphoto2() {
        let mut registry = DriverRegistry::new();
        registry
            .register_camera(CanonGPhoto2CameraFactory::new())
            .expect("registers");
        assert!(registry.camera_drivers().contains(&DRIVER_NAME));
    }

    #[cfg(not(feature = "libgphoto2"))]
    #[test]
    fn a_build_without_the_backend_says_so_instead_of_pretending_the_driver_is_unknown() {
        // The reason the factory is registered even when it cannot build anything: this message
        // instead of `unknown driver "gphoto2", available: simulator`.
        let error = CanonGPhoto2CameraFactory::new()
            .create(&config())
            .expect_err("no backend in this build");
        let message = error.to_string();
        assert!(message.contains("libgphoto2"), "{message}");
        assert!(
            message.contains("--features"),
            "the message must say how to fix it: {message}"
        );
    }

    // -------------------------------------------------------------------------------------
    // Live view, battery/storage and wedge recovery (M2-T04)
    // -------------------------------------------------------------------------------------

    /// A config with a fast preview rate, so a test collecting frames takes milliseconds.
    ///
    /// The *shipped* rate is 5 fps and is asserted separately — see
    /// `the_shipped_default_paces_live_view_at_prf_02s_floor`. Using it here would make every
    /// live-view test wait 200 ms per frame for no additional truth.
    fn config_at_fps(fps: u32) -> CameraConfig {
        CameraConfig {
            live_view_fps: fps,
            ..config()
        }
    }

    /// A connected camera streaming live view fast, and the handle that scripts its body.
    async fn streaming_camera() -> (
        Arc<super::super::mock::MockState>,
        Arc<CanonGPhoto2Camera>,
        astroctl_hal::stream::FrameStream<astroctl_hal::camera::LiveViewFrame>,
    ) {
        let (state, factory) = MockState::new();
        let camera = Arc::new(CanonGPhoto2Camera::new(&config_at_fps(200), factory));
        camera.connect().await.expect("connects");
        let stream = camera.live_view_stream().await.expect("live view starts");
        (state, camera, stream)
    }

    /// Waits for `wanted` frames, or gives up. Returns what arrived.
    async fn collect_frames(
        stream: &mut astroctl_hal::stream::FrameStream<astroctl_hal::camera::LiveViewFrame>,
        wanted: usize,
    ) -> Vec<astroctl_hal::camera::LiveViewFrame> {
        let mut frames = Vec::new();
        for _ in 0..wanted {
            match tokio::time::timeout(Duration::from_secs(5), stream.next_frame()).await {
                Ok(Some(frame)) => frames.push(frame),
                _ => break,
            }
        }
        frames
    }

    #[tokio::test]
    async fn live_view_delivers_the_bodys_own_jpeg_bytes_unmodified() {
        // SDD §5.7 source 1: "forwarded as-is". Re-encoding 133 KB several times a second on a Pi
        // would be the most expensive thing in the preview path and would buy nothing, since the
        // body already produces JPEG.
        let (_state, camera, mut stream) = streaming_camera().await;

        let frames = collect_frames(&mut stream, 3).await;
        assert_eq!(frames.len(), 3, "the stream must produce frames");
        for frame in &frames {
            assert_eq!(
                &frame.jpeg[..2],
                &[0xFF, 0xD8],
                "the JPEG magic must survive the driver untouched"
            );
        }

        // Each frame is a *new* one, not the same slot read three times. This is the failure a
        // depth-1 watch channel makes easy to miss: a stalled producer and a working one both
        // leave a frame in the slot, so only the contents distinguish them.
        let sequence: Vec<u32> = frames
            .iter()
            .filter_map(|frame| super::super::mock::mock_preview_sequence(&frame.jpeg))
            .collect();
        assert_eq!(sequence.len(), 3);
        assert!(
            sequence.windows(2).all(|pair| pair[1] > pair[0]),
            "frames must advance, not repeat: {sequence:?}"
        );

        camera.disconnect().await.expect("disconnects");
    }

    #[tokio::test]
    async fn a_capture_pauses_live_view_without_wedging_the_camera() {
        // **The headline of this task, and the interaction that decides whether it is safe.**
        //
        // There is one queue (SDD §5.3.1), so a preview issued during an exposure would be
        // serviced after it — against `config_seconds`, five. It would therefore time out, and a
        // timed-out command *is* a wedged thread by definition: one live-view tick would abandon
        // the camera and destroy the frame. At 5 fps that is five chances a second.
        //
        // What prevents it is M2-T03's gate. `request_unless_capturing` tests the `CaptureClaim`
        // *under the sender lock* and answers `Busy` **before sending**, so no command is queued
        // and no budget timer ever starts. The pause costs frames and cannot cost the camera —
        // and that is structural, not a heuristic: there is no timer to expire.
        let (state, camera, mut stream) = streaming_camera().await;
        camera.set_shutter("30").await.expect("30 s is offered");
        let dir = scratch_dir("liveview-capture-pause");

        // Frames are flowing before the exposure.
        assert_eq!(collect_frames(&mut stream, 2).await.len(), 2);

        // A capture long enough to cover many live-view ticks — the real stall is 2.08 s and the
        // period here is 5 ms, so this is the same shape at a testable scale.
        state.capture_takes(Duration::from_millis(400));
        let exposing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&CaptureRequest::new(&dir, "light_1")).await })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        let during = state.previews();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            state.previews(),
            during,
            "live view must pull no frames at all while the imaging path is claimed"
        );

        // The exposure the pause protected completes normally...
        exposing
            .await
            .expect("task")
            .expect("the capture survives a live-view stream running against it");

        // ...the camera was never wedged — this is the assertion the whole design exists for...
        assert!(
            camera.battery().await.is_ok(),
            "a paused live view must never have wedged the camera thread"
        );
        assert!(camera.link_state().is_connected());

        // ...and frames resume on the *same* stream, with no reconnect and no restart, exactly as
        // SDD §5.7 requires of an expected gap.
        let after = collect_frames(&mut stream, 2).await;
        assert_eq!(
            after.len(),
            2,
            "live view must resume by itself once the exposure ends"
        );

        camera.disconnect().await.expect("disconnects");
    }

    #[tokio::test]
    async fn a_camera_that_stops_answering_still_wedges_the_detector() {
        // The other half of the same distinction, and the one that would be quietly lost by a
        // driver that made live view "safe" by exempting it from the budget. SDD §5.7: the
        // pipeline must tell *idle because the camera is busy* from *idle because the camera
        // stopped responding*. Nothing is capturing here, so the gate does not fire, the preview
        // is sent, and it blows its budget — which is a wedge, as designed.
        let (state, factory) = MockState::new();
        let one_second = CameraConfig {
            timeouts: CameraTimeouts {
                config_seconds: 1,
                ..config().timeouts
            },
            ..config_at_fps(200)
        };
        let camera = Arc::new(CanonGPhoto2Camera::new(&one_second, factory));
        camera.connect().await.expect("connects");

        // A preview that never returns: the thread is inside a C call that cannot be interrupted.
        state.block_calls_for(Duration::from_secs(3));

        let error = camera
            .live_view_frame()
            .await
            .expect_err("the camera stopped answering");
        match error {
            DeviceError::Timeout(budget) => assert_eq!(budget, Duration::from_secs(1)),
            other => panic!("a silent camera must time out, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn live_view_pulls_nothing_when_nobody_is_watching() {
        // M1-T09's acceptance criterion names the orphaned-work failure directly, and on a real
        // body it is a USB transfer and 133 KB per frame for the rest of the night.
        let (state, camera, stream) = streaming_camera().await;
        let _ = collect_frames(&mut stream.clone(), 1).await;

        drop(stream);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let idle = state.previews();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            state.previews(),
            idle,
            "with no subscriber the pump must stop pulling frames off the camera"
        );
        camera.disconnect().await.expect("disconnects");
    }

    #[tokio::test]
    async fn a_new_subscriber_restarts_the_frames_without_restarting_the_stream() {
        // The other side of the idle rule: the pump goes quiet rather than exiting, so a client
        // that reconnects gets frames again with no `live_view_stream` call in between.
        let (_state, camera, stream) = streaming_camera().await;
        drop(stream);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut resumed = camera.live_view_stream().await.expect("subscribes again");
        assert_eq!(
            collect_frames(&mut resumed, 2).await.len(),
            2,
            "a returning watcher must get frames without the stream being restarted"
        );
        camera.disconnect().await.expect("disconnects");
    }

    #[tokio::test]
    async fn two_subscribers_are_two_cursors_on_one_stream_not_two_sensor_loops() {
        // The trait's idempotence rule. The camera has one imaging path, and two pumps would
        // simply take turns blocking each other on the single queue.
        let (state, camera, mut first) = streaming_camera().await;
        let mut second = camera.live_view_stream().await.expect("a second cursor");

        let (a, b) = tokio::join!(
            collect_frames(&mut first, 2),
            collect_frames(&mut second, 2)
        );
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);

        // One loop, so the frames both saw came from one pull each rather than two.
        let pulled = state.previews();
        assert!(
            pulled < 12,
            "two subscribers must not double the USB traffic; {pulled} frames were pulled"
        );
        camera.disconnect().await.expect("disconnects");
    }

    #[tokio::test]
    async fn stopping_live_view_ends_the_stream_and_asks_the_body_to_leave_it() {
        // Both halves. The frames stopping is what the operator asked for; telling the body is
        // what stops a mirrorless sensor being held powered for the rest of the night, on the
        // same battery this driver reports.
        let (state, camera, mut stream) = streaming_camera().await;
        assert_eq!(collect_frames(&mut stream, 1).await.len(), 1);

        camera.stop_live_view().await.expect("stops");

        assert_eq!(
            stream.next_frame().await,
            None,
            "dropping the sink is how a driver says live view stopped"
        );
        assert_eq!(
            state.stop_previews(),
            1,
            "the body must be told to leave live view, not merely ignored"
        );

        // Idempotent, and safe when it is not running.
        camera.stop_live_view().await.expect("stopping twice is Ok");
        camera.disconnect().await.expect("disconnects");
    }

    #[tokio::test]
    async fn disconnecting_stops_live_view_before_the_camera_goes() {
        // The order matters: the body has to be told while there is still a camera to tell.
        let (state, camera, mut stream) = streaming_camera().await;
        assert_eq!(collect_frames(&mut stream, 1).await.len(), 1);

        camera.disconnect().await.expect("disconnects");

        assert_eq!(stream.next_frame().await, None);
        assert_eq!(
            state.stop_previews(),
            1,
            "a disconnect must not leave the body in live view"
        );
    }

    #[tokio::test]
    async fn a_body_that_reports_no_preview_operation_says_unsupported() {
        // Derived from the abilities the body reported at connect, never assumed — the same way
        // `has_bulb` comes from the shutter list rather than from a table of models.
        let (state, factory) = MockState::new();
        state.remove_live_view();
        let camera = CanonGPhoto2Camera::new(&config(), factory);
        camera.connect().await.expect("connects");

        assert!(!camera.capabilities().has_live_view);
        assert!(
            matches!(
                camera.live_view_stream().await,
                Err(DeviceError::Unsupported)
            ),
            "a body with no preview operation is Unsupported, not a stream that never produces"
        );
    }

    #[tokio::test]
    async fn live_view_before_connect_is_not_connected() {
        let (_state, camera) = camera();
        assert!(matches!(
            camera.live_view_stream().await,
            Err(DeviceError::NotConnected)
        ));
        assert!(matches!(
            camera.live_view_frame().await,
            Err(DeviceError::NotConnected)
        ));
        // ...but stopping is always Ok. SDD §5.8.1 forbids refusing a stop.
        camera.stop_live_view().await.expect("nothing to stop");
    }

    #[tokio::test]
    async fn a_one_shot_preview_during_an_exposure_is_busy_rather_than_queued() {
        // The same gate, on the one-shot path. The trait documents `Busy` during a capture as the
        // right answer, and it is the answer that keeps the camera alive.
        let (state, camera) = timed_camera().await;
        state.capture_takes(Duration::from_millis(400));
        let dir = scratch_dir("preview-during-capture");
        let camera = Arc::new(camera);

        let exposing = {
            let camera = Arc::clone(&camera);
            let dir = dir.clone();
            tokio::spawn(async move { camera.capture(&CaptureRequest::new(&dir, "light_1")).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            matches!(camera.live_view_frame().await, Err(DeviceError::Busy(_))),
            "a focus check during an exposure must be refused, not queued behind it"
        );
        exposing.await.expect("task").expect("the capture survives");
    }

    #[tokio::test]
    async fn the_shipped_default_paces_live_view_at_prf_02s_floor() {
        // 58.5 fps is what the hardware does; 5 fps is what PRF-02 needs and what USB-11 asks us
        // to come down to. The knob exists so the number is a decision rather than an accident,
        // and this asserts the shipped decision.
        assert_eq!(config().live_view_fps, 5);
    }

    // --- wedge recovery (REL-03) ---------------------------------------------------------

    /// Waits for the driver's link state to satisfy `wanted`.
    async fn await_state(
        camera: &CanonGPhoto2Camera,
        what: &str,
        wanted: impl Fn(&LinkState) -> bool,
    ) -> LinkState {
        let mut watch = camera.watch_link_state();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            {
                let current = watch.borrow_and_update().clone();
                if wanted(&current) {
                    return current;
                }
            }
            tokio::select! {
                changed = watch.changed() => changed.expect("the driver is alive"),
                () = tokio::time::sleep_until(deadline) => {
                    panic!("never reached {what}; stuck at {:?}", camera.link_state())
                }
            }
        }
    }

    #[tokio::test]
    async fn a_wedged_camera_recovers_by_itself_to_a_working_capture() {
        // T-CAM-1's shape, against the mock: an induced wedge, automatic recovery, and a capture
        // that works afterwards — with the state going reconnecting → connected in between.
        let (state, factory) = MockState::new();
        let one_second = CameraConfig {
            timeouts: CameraTimeouts {
                config_seconds: 1,
                ..config().timeouts
            },
            ..config()
        };
        let camera = Arc::new(CanonGPhoto2Camera::new(&one_second, factory));
        camera.connect().await.expect("connects");
        camera.set_shutter("30").await.expect("30 s is offered");
        assert!(camera.link_state().is_connected());

        // The wedge: a call that outlasts its budget, i.e. a thread inside a C call that cannot
        // be interrupted.
        let contexts_before = state.builds();
        state.block_calls_for(Duration::from_secs(2));
        let error = camera.battery().await.expect_err("over budget");
        assert!(matches!(error, DeviceError::Timeout(_)), "{error:?}");

        // The camera comes back — the mock stops blocking, as a body does once it is replugged.
        state.block_calls_for(Duration::ZERO);

        // **Wait for the rebuild, not for the state.** `link_state` is eventually consistent: the
        // wedge happens on the caller's task and the transition is published by the recovery loop
        // the next time it is scheduled, so for one tick the state still reads `Connected` — the
        // value from before the wedge. Sampling it here would pass instantly and prove nothing.
        // A *fresh context* is the unambiguous evidence that recovery ran, and it is the thing
        // M2-T01 measured as the only path that works.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while state.builds() <= contexts_before {
            assert!(
                tokio::time::Instant::now() < deadline,
                "recovery never rebuilt the context; state is {:?}",
                camera.link_state()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        await_state(&camera, "connected", LinkState::is_connected).await;

        // The acceptance criterion's real content: *working capture* afterwards, not merely a
        // link that reports itself up.
        let result = camera
            .capture(&CaptureRequest::new(scratch_dir("recovered"), "light_1"))
            .await
            .expect("the recovered camera takes a frame");
        assert!(result.raw().expect("a science file").path.exists());

        // And it is a genuinely fresh context: M2-T01 proved the stale handle never recovers.
        assert!(
            state.builds() >= 2,
            "recovery must build a new context, not retry the abandoned one"
        );
    }

    #[tokio::test]
    async fn recovery_reports_reconnecting_before_it_reports_connected() {
        // The transition SDD §5.3.1 asks to be surfaced. The driver cannot publish it — this
        // crate has no event bus by design — so it drives the state and the facade publishes.
        let (state, factory) = MockState::new();
        let one_second = CameraConfig {
            timeouts: CameraTimeouts {
                config_seconds: 1,
                ..config().timeouts
            },
            ..config()
        };
        let camera = Arc::new(CanonGPhoto2Camera::new(&one_second, factory));
        camera.connect().await.expect("connects");

        // Fail the reopen for a while, so `Reconnecting` is observable rather than a state the
        // driver passes through in 108 ms.
        state.fail_open_with("Could not find the requested device on the USB port");
        state.block_calls_for(Duration::from_secs(2));
        camera.battery().await.expect_err("over budget");
        state.block_calls_for(Duration::ZERO);

        let reconnecting = await_state(&camera, "reconnecting", |s| {
            matches!(s, LinkState::Reconnecting { .. })
        })
        .await;
        let LinkState::Reconnecting { attempt, of, .. } = &reconnecting else {
            unreachable!()
        };
        assert!(*attempt >= 1 && attempt <= of, "{reconnecting:?}");
        // The operator is told *why*, not merely that something is happening.
        let because = reconnecting
            .message()
            .expect("reconnecting explains itself");
        assert!(!because.is_empty(), "{because}");

        state.succeed_open();
        await_state(&camera, "connected", LinkState::is_connected).await;
    }

    #[tokio::test]
    async fn live_view_survives_a_recovery_and_resumes_into_the_same_stream() {
        // **The failure this test exists to prevent is silent.** Dropping the `FrameSink` on a
        // wedge would end every subscriber's stream, and the field node's forwarding task does
        // not re-subscribe — it logs "the camera's live-view stream ended" and exits. The camera
        // would recover perfectly and the operator would still be looking at a dead preview.
        //
        // So the pump outlives its link: a tick during recovery is a skipped frame, and the
        // frames resume into the *same* sink and the *same* subscriber.
        let (state, factory) = MockState::new();
        let fast = CameraConfig {
            timeouts: CameraTimeouts {
                config_seconds: 1,
                ..config().timeouts
            },
            ..config_at_fps(200)
        };
        let camera = Arc::new(CanonGPhoto2Camera::new(&fast, factory));
        camera.connect().await.expect("connects");
        let mut stream = camera.live_view_stream().await.expect("live view starts");
        assert_eq!(collect_frames(&mut stream, 2).await.len(), 2);

        // The camera leaves mid-stream, exactly as the spike's cable pull did: live view was the
        // traffic generator, and the preview call is what noticed.
        state.fail_preview_with("Could not find the requested device on the USB port");
        await_state(&camera, "recovering or reconnected", |s| {
            !matches!(s, LinkState::Connected)
        })
        .await;

        // ...and it comes back.
        state.succeed_preview();
        await_state(&camera, "connected", LinkState::is_connected).await;

        assert_eq!(
            collect_frames(&mut stream, 2).await.len(),
            2,
            "the same subscriber must see frames again, with no re-subscription"
        );
    }

    #[tokio::test]
    async fn a_vanished_camera_is_diagnosed_as_a_cable_and_never_as_gvfs() {
        // Branch (a) of M2-T01's two measured error strings. Sending an operator to hunt for a
        // desktop mount when the camera is simply unplugged is the wrong place at the worst time.
        let (state, factory) = MockState::new();
        let camera = Arc::new(CanonGPhoto2Camera::new(&config(), factory));
        camera.connect().await.expect("connects");

        state.fail_open_with("Could not find the requested device on the USB port");
        state.fail_preview_with("Could not find the requested device on the USB port");
        camera.live_view_frame().await.expect_err("the camera left");

        let state_now = await_state(&camera, "not connected", |s| !s.is_connected()).await;
        let message = state_now.message().expect("it explains itself");
        assert!(message.contains("cable"), "{message}");
        assert!(
            !message.contains("gvfs"),
            "a device that is not on the bus is not being held by anything: {message}"
        );
    }

    #[tokio::test]
    async fn a_stolen_camera_is_diagnosed_as_a_claim_conflict_with_a_next_step() {
        // Branch (b), and REL-03's real-world blocker. gvfs is named when it is actually there;
        // when it is not, the message lists the remaining causes rather than picking one.
        let (state, factory) = MockState::new();
        let camera = Arc::new(CanonGPhoto2Camera::new(&config(), factory));
        camera.connect().await.expect("connects");

        state.fail_open_with("Could not claim the USB device");
        state.fail_preview_with("Could not claim the USB device");
        camera
            .live_view_frame()
            .await
            .expect_err("something took it");

        let state_now = await_state(&camera, "not connected", |s| !s.is_connected()).await;
        let message = state_now.message().expect("it explains itself");
        assert!(
            message.contains("Could not claim the USB device"),
            "{message}"
        );
        assert!(
            message.contains("gio mount -u") || message.contains("switched off"),
            "the message names no next step: {message}"
        );
    }

    #[tokio::test]
    async fn a_device_that_is_present_but_has_stopped_answering_is_recovered_from() {
        // **The third failure mode, and M2-T01 did not see it because it pulled the cable.**
        // M2-T04 induced a bus-level reset on the real body instead — which is what a hub glitch
        // or a power-management event looks like — and found the device *still enumerated* while
        // every transfer failed with libgphoto2's `Unspecified error`. Neither measured string
        // appears, so neither text branch fires, and an earlier version of this driver sat there
        // logging skipped frames all night with a camera it could have rebuilt in 108 ms.
        //
        // Repetition is the only signal available, so repetition is what is used.
        let (state, factory) = MockState::new();
        let camera = Arc::new(CanonGPhoto2Camera::new(&config(), factory));
        camera.connect().await.expect("connects");
        let contexts_before = state.builds();

        state.fail_preview_with("Unspecified error");
        for _ in 0..super::super::thread::UNRESPONSIVE_STREAK {
            camera
                .live_view_frame()
                .await
                .expect_err("the session is dead");
        }
        state.succeed_preview();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while state.builds() <= contexts_before {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a camera that stopped answering was never rebuilt; state is {:?}",
                camera.link_state()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        await_state(&camera, "connected", LinkState::is_connected).await;
        camera
            .live_view_frame()
            .await
            .expect("the rebuilt session works");
    }

    #[tokio::test]
    async fn one_bad_transfer_is_a_lost_frame_and_not_a_lost_camera() {
        // The other side of the streak rule, and the reason it is a streak. A marginal cable that
        // drops one transfer an hour must never reach the threshold — tearing the camera down for
        // a single failure would turn a lost frame into a lost session, every hour, all night.
        let (state, camera) = camera();
        camera.connect().await.expect("connects");

        // One short of the threshold, then a success, then one short again. A running total would
        // have fired; a streak does not.
        for _ in 0..2 {
            state.fail_preview_with("Unspecified error");
            camera
                .live_view_frame()
                .await
                .expect_err("one bad transfer");
            state.succeed_preview();
            camera.live_view_frame().await.expect("and then a good one");
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            camera.link_state().is_connected(),
            "an intermittent failure must not tear the camera down: {:?}",
            camera.link_state()
        );
        assert_eq!(
            state.builds(),
            1,
            "nothing should have been rebuilt for two isolated failures"
        );
    }

    #[tokio::test]
    async fn a_failure_that_is_not_about_the_link_does_not_tear_the_camera_down() {
        // The other side of the classification. A download that could not be written, or a body
        // that refused a widget, is a failure of *that operation* — tearing the camera thread
        // down for one would turn a lost frame into a lost session.
        let (state, camera) = timed_camera().await;
        state.fail_download_with("No space left on device");

        camera
            .capture(&CaptureRequest::new(
                scratch_dir("not-a-link-fault"),
                "light_1",
            ))
            .await
            .expect_err("the download failed");

        // Give any recovery that was going to fire the chance to.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            camera.battery().await.is_ok(),
            "a per-frame failure must leave the camera connected"
        );
    }

    #[tokio::test]
    async fn a_short_live_view_soak_leaks_nothing_and_does_not_decay() {
        // The mock half of the 10-minute hardware soak. It cannot say anything about fps decay on
        // a real body — only a real body can — but it proves the plumbing: hundreds of frames
        // through the watch channel with one subscriber attached, no unbounded growth, and a
        // stream that is still producing at the end at the rate it started.
        let (state, camera, mut stream) = streaming_camera().await;

        let first_window = tokio::time::Instant::now();
        assert_eq!(collect_frames(&mut stream, 20).await.len(), 20);
        let first_window = first_window.elapsed();

        // Run for a while with the subscriber attached but reading only occasionally, which is
        // the shape of a slow client and the case where a queue would grow.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = stream.latest();
        }

        // The depth-1 channel is what bounds memory: a slow consumer skips frames rather than
        // accumulating them, so the first frame after a pause is a *recent* one and not the head
        // of a backlog. Both numbers are read at the same instant, or the comparison is against a
        // frame count that has moved on since.
        let resumed = stream.next_frame().await.expect("the stream is alive");
        let pulled = u32::try_from(state.previews()).expect("fits");
        let seen = super::super::mock::mock_preview_sequence(&resumed.jpeg).expect("a sequence");
        assert!(
            pulled.saturating_sub(seen) < 5,
            "a slow consumer must wake to the newest frame; it woke {} frames behind",
            pulled.saturating_sub(seen)
        );

        let last_window = tokio::time::Instant::now();
        let tail = collect_frames(&mut stream, 20).await;
        let last_window = last_window.elapsed();
        assert_eq!(tail.len(), 20, "the stream must still be producing");
        // And the rate has not decayed: the last twenty frames took about as long as the first
        // twenty. Generous, because this is a shared CI machine and the claim is "no decay", not
        // "identical timing".
        assert!(
            last_window < first_window * 4 + Duration::from_millis(200),
            "live view slowed from {first_window:?} to {last_window:?} over the run"
        );

        camera.disconnect().await.expect("disconnects");
    }
}
