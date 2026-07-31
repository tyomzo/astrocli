//! The `CamOps` implementation that actually talks to a camera, via `libgphoto2`.
//!
//! Behind the non-default `libgphoto2` feature, because `libgphoto2_sys` runs `pkg-config` and
//! `bindgen` in its build script: on a machine without `libgphoto2-dev` this file does not fail
//! to link, it fails to *compile*. That is the whole reason the driver is split the way it is —
//! see the crate manifest.
//!
//! **Everything here is blocking and runs on the camera thread**, which is enforced structurally:
//! `CamOps` is a synchronous trait and [`super::thread`] is the only thing that builds one.
//!
//! # Deliberately thin
//!
//! This file gets values off the wire and does not interpret them. No capability derivation, no
//! format mapping, no error rewriting — those live in [`super::ops`] and [`super::thread`], where
//! CI can test them. What is left here is the part that genuinely needs a camera, and it is
//! therefore the part no CI machine can check. Keeping it small is the only lever available.
//!
//! Every measurement quoted below is M2-T01's, on a real Canon EOS R10.

use std::path::Path;
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_core::types::{BatteryStatus, DeviceInfo, DeviceKind, StorageInfo};
use astroctl_hal::registry::DetectedDevice;
use gphoto2::camera::CameraEvent;
use gphoto2::widget::{RadioWidget, TextWidget, ToggleWidget};
use gphoto2::{Camera, Context};

use super::camera::DRIVER_NAME;
use super::ops::{
    AbortSignal, CamOps, CamOpsFactory, CfgKey, RawCapture, RawChoices, RawFileRef, RawIdentity,
    RawSettings,
};

/// The Canon PTP key that opens and closes the shutter for a bulb exposure.
///
/// Not a documented interface — it is what the R10's config tree exposes, read off the body by
/// M2-T01 and re-read for M2-T03. A body without it has no bulb mechanism this driver can drive,
/// which is [`DeviceError::Unsupported`] rather than a failure.
const BULB_KEY: &str = "eosremoterelease";

/// How long to wait for each `NewFile` event after a trigger.
///
/// Short because it is paid on *every* capture: the frame the trigger already returned is in
/// hand, and this budget only decides how long the driver waits to find out whether a *second*
/// file (the JPEG of a RAW+JPEG pair) is coming. Too long and every single-file capture pays for
/// it; too short and a RAW+JPEG session silently loses its JPEGs.
const EVENT_POLL: Duration = Duration::from_millis(600);

/// How long to let the binding finish previous traffic before opening the shutter.
///
/// One poll window's worth. It is not a guess at how long the camera needs — it is one turn of
/// libgphoto2's event pump, which is what actually advances a transaction that is "in progress".
const SETTLE_BUDGET: Duration = EVENT_POLL;

/// How long to pause before retrying a shutter press the binding was too busy to accept.
const BUSY_RETRY_PAUSE: Duration = Duration::from_millis(400);

/// Slack added to an aborted exposure's own length when waiting for its orphaned frame.
///
/// The wait is `2 × time the shutter was open + this`, because the body's dark-frame pass matches
/// the real exposure. The margin covers the readout and the event hop; five seconds was measured
/// to be *just* too short on its own for a three-second abort, which is how the doubling was
/// found in the first place.
const ABORTED_DRAIN_MARGIN: Duration = Duration::from_secs(6);

/// Builds [`LibGphoto2Ops`] on the camera thread.
#[derive(Debug, Default)]
pub(crate) struct LibGphoto2Factory;

impl LibGphoto2Factory {
    /// Builds the factory.
    pub(crate) fn new() -> Self {
        Self
    }
}

impl CamOpsFactory for LibGphoto2Factory {
    fn build(&self) -> Result<Box<dyn CamOps>, DeviceError> {
        // `Context::new()` is where libgphoto2 loads its camlibs and iolibs. It runs here, on the
        // camera thread, and the context never leaves it.
        let context = Context::new().map_err(|error| {
            DeviceError::Transport(format!("libgphoto2 would not start: {error}"))
        })?;
        Ok(Box::new(LibGphoto2Ops {
            context,
            camera: None,
        }))
    }
}

/// A camera reached over PTP/USB.
///
/// No `Debug`: neither `gphoto2::Context` nor `gphoto2::Camera` implements it, and `CamOps`
/// deliberately does not require it. Only the *factory* is `Debug`, because that is the half the
/// driver holds and prints.
pub(crate) struct LibGphoto2Ops {
    context: Context,
    camera: Option<Camera>,
}

impl LibGphoto2Ops {
    /// The open camera, or `NotConnected`.
    fn camera(&self) -> Result<&Camera, DeviceError> {
        self.camera.as_ref().ok_or(DeviceError::NotConnected)
    }

    /// Reads a radio widget's current choice and its full choice list.
    ///
    /// Radio is how libgphoto2 models every enumerated setting on this body — `iso`,
    /// `shutterspeed`, `aperture`, `imageformat` are all radios. A key the body does not expose
    /// (`aperture` behind a fully manual lens) is an empty list rather than an error: the body is
    /// working, it simply has no such control, and the layer above turns that into
    /// `Unsupported`.
    fn radio(&self, key: CfgKey) -> (String, Vec<String>) {
        let Ok(camera) = self.camera() else {
            return (String::new(), Vec::new());
        };
        match camera.config_key::<RadioWidget>(key.as_str()).wait() {
            Ok(widget) => (widget.choice(), widget.choices_iter().collect()),
            Err(error) => {
                tracing::debug!(
                    key = key.as_str(),
                    %error,
                    "the camera does not expose this setting"
                );
                (String::new(), Vec::new())
            }
        }
    }

    /// Reads a text config key, e.g. `serialnumber`.
    fn text(&self, key: &str) -> Option<String> {
        let camera = self.camera().ok()?;
        camera
            .config_key::<TextWidget>(key)
            .wait()
            .ok()
            .map(|widget| widget.value())
            .filter(|value| !value.is_empty())
    }

    /// Reads every choice list in one pass.
    ///
    /// Four config-key reads rather than one tree walk: the spike measured 0.4–10 ms for a single
    /// key against 222 ms for all 91 entries, and this driver needs four of them.
    fn choices(&self) -> RawChoices {
        RawChoices {
            isos: self.radio(CfgKey::Iso).1,
            shutters: self.radio(CfgKey::Shutter).1,
            apertures: self.radio(CfgKey::Aperture).1,
            formats: self.radio(CfgKey::ImageFormat).1,
        }
    }

    /// The body's bulb press/release tokens, if it has a bulb mechanism at all.
    ///
    /// **`Press Full` must be matched more carefully than it looks.** The choice list contains
    /// both `Press Full` and `Release Full`, so a search for the first entry containing `Full`
    /// finds whichever the body happens to list first — the spike's own probe had this latent and
    /// got away with it on ordering. Matching the press as *"contains Full and does not contain
    /// Release"* is what makes it independent of the order the camera enumerates in, and getting
    /// it backwards means releasing a shutter that was never pressed and then holding it open for
    /// the exposure.
    fn bulb_tokens(&self) -> Option<(RadioWidget, String, String)> {
        let camera = self.camera().ok()?;
        let widget = camera.config_key::<RadioWidget>(BULB_KEY).wait().ok()?;
        let choices: Vec<String> = widget.choices_iter().collect();

        let press = choices
            .iter()
            .find(|choice| choice.contains("Full") && !choice.contains("Release"))?
            .clone();
        let release = choices
            .iter()
            .find(|choice| choice.contains("Release Full"))?;
        Some((widget, press, release.clone()))
    }

    /// Removes a frame from the camera's own storage.
    ///
    /// HAL-03 requires an aborted capture to leave nothing behind before it resolves, and with
    /// `capturetarget=Internal RAM` "behind" includes the camera: the body holds a frame in a
    /// buffer that only a download or a delete frees, and this path deliberately does not
    /// download.
    ///
    /// **The stronger claim is unproven.** M2-T03 saw every bulb frame fail after one aborted
    /// exposure — in a *fresh process*, so the state was on the camera rather than in the driver
    /// — and an occupied buffer was the obvious explanation. Adding this delete did not fix it,
    /// and the camera then dropped off USB altogether, which makes a fading body an equally good
    /// explanation for the whole episode and leaves the two confounded. See the pending-hardware
    /// note in the task file: the sequence to re-run is abort → bulb, on a body known to be
    /// healthy. This call is kept because the contract asks for it regardless of what it fixes.
    ///
    /// Best effort: a body that refuses the delete has told us nothing we can act on here, and
    /// the failure the operator would see is the same one they would see if we had not tried.
    fn discard_from_camera(&self, file: &RawFileRef) {
        let Ok(camera) = self.camera() else {
            return;
        };
        match camera.fs().delete_file(&file.folder, &file.name).wait() {
            Ok(()) => tracing::debug!(
                file = %file.name,
                "removed an abandoned frame from the camera's buffer"
            ),
            Err(error) => tracing::warn!(
                file = %file.name,
                %error,
                "could not remove an abandoned frame from the camera; the next capture may find \
                 the buffer still occupied"
            ),
        }
    }

    /// Opens the shutter, retrying once if the binding is still busy with the previous frame.
    ///
    /// `I/O in progress` is not a refusal, it is a *not yet*: libgphoto2 is mid-transaction and
    /// says so. M2-T03 met it on a 60 s bulb exposure issued straight after a 30 s one, with the
    /// download of the first frame complete and the driver believing the camera idle. The pre-
    /// drain above removes most of the window and this closes the rest, because "the camera is
    /// busy" is a reason to wait rather than to fail a frame the operator asked for.
    ///
    /// Exactly one retry, and only for this error: a press that fails twice, or fails for any
    /// other reason, is a real failure and returning it is what lets the operator see it.
    fn press_shutter(&self, widget: &RadioWidget, press: &str) -> Result<(), DeviceError> {
        let Err(error) = self.commit(widget, press) else {
            return Ok(());
        };
        if !format!("{error}").contains("I/O in progress") {
            return Err(error);
        }
        tracing::debug!("the camera was still busy with the previous frame; retrying the shutter");
        std::thread::sleep(BUSY_RETRY_PAUSE);
        // Pump the queue once more — this is what actually lets the binding finish, rather than
        // the sleep.
        let _ = self.drain_events(SETTLE_BUDGET, false);
        self.commit(widget, press)
    }

    /// Writes one choice to an already-fetched widget.
    fn commit(&self, widget: &RadioWidget, choice: &str) -> Result<(), DeviceError> {
        let camera = self.camera()?;
        widget.set_choice(choice).map_err(|error| {
            DeviceError::Protocol(format!(
                "the camera refused `{choice}` for {BULB_KEY}: {error}"
            ))
        })?;
        camera.set_config(widget).wait().map_err(|error| {
            DeviceError::Protocol(format!(
                "the camera refused `{choice}` for {BULB_KEY}: {error}"
            ))
        })
    }

    /// Collects `NewFile` events until the body stops producing them or `budget` runs out.
    ///
    /// This is how the second file of a RAW+JPEG pair arrives: `capture_image` hands back one
    /// path, and the other reaches the driver as an event. It is also the *only* way a bulb frame
    /// arrives, since the release call returns as soon as the shutter closes.
    ///
    /// Draining matters beyond collecting: an event left in libgphoto2's queue is read by the
    /// *next* capture, which would then download the previous frame under this frame's name.
    ///
    /// `wait_for_a_file` says whether the caller is *expecting* one — the bulb path is, because
    /// the release call returns before the body has read the frame out, so it must keep waiting
    /// through `Timeout`s. The timed path is not, because the trigger already handed it a file
    /// and this drain is only looking for siblings.
    ///
    /// **Either way the drain continues after the first file**, up to a short settle window. A
    /// version of this that returned on the first `NewFile` lost the JPEG of a RAW+JPEG *bulb*
    /// frame and left it queued for the next capture to mistake for its own — the precise failure
    /// this function exists to prevent, reintroduced by the shortcut that avoids it on the other
    /// path.
    fn drain_events(&self, budget: Duration, wait_for_a_file: bool) -> Drained {
        let Ok(camera) = self.camera() else {
            return Drained::default();
        };
        let deadline = std::time::Instant::now() + budget;
        let mut drained = Drained::default();
        // Once a file has arrived, the queue is drained to quiescence rather than to the full
        // budget: a body that has announced one file announces the rest immediately.
        let mut settle_deadline: Option<std::time::Instant> = None;

        while std::time::Instant::now() < deadline {
            if settle_deadline.is_some_and(|settle| std::time::Instant::now() >= settle) {
                break;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match camera.wait_event(remaining.min(EVENT_POLL)).wait() {
                Ok(CameraEvent::NewFile(path)) => {
                    drained.files.push(RawFileRef {
                        folder: path.folder().into_owned(),
                        name: path.name().into_owned(),
                    });
                    settle_deadline = Some(std::time::Instant::now() + EVENT_POLL);
                }
                // **This is where the exposure figure lives.** `BulbExposureTime` is not a config
                // key — M2-T03 looked for one and found none, which is why a first attempt at
                // this silently echoed the requested duration back as though the camera had
                // confirmed it. libgphoto2 surfaces the body's PTP property change as an
                // untyped event string, so it has to be caught here, in the drain, or not at all.
                Ok(CameraEvent::Unknown(text)) => {
                    if let Some(seconds) = bulb_exposure_seconds(&text) {
                        drained.exposure_seconds = Some(seconds);
                    }
                }
                // No event inside the poll window. A caller that already has its file — or that
                // was never waiting for one — is done; the bulb path keeps waiting, because the
                // body is still reading the frame out.
                Ok(CameraEvent::Timeout) => {
                    if !wait_for_a_file || !drained.files.is_empty() {
                        return drained;
                    }
                }
                Ok(_other) => {}
                Err(error) => {
                    tracing::debug!(%error, "the camera stopped reporting events");
                    return drained;
                }
            }
        }
        drained
    }

    /// Reads the settings in force.
    fn settings(&self) -> RawSettings {
        let aperture = self.radio(CfgKey::Aperture).0;
        RawSettings {
            iso: self.radio(CfgKey::Iso).0,
            shutter: self.radio(CfgKey::Shutter).0,
            // An empty choice means the key is absent — a manual lens — which is `None`, not an
            // aperture of "".
            aperture: (!aperture.is_empty()).then_some(aperture),
            format: self.radio(CfgKey::ImageFormat).0,
        }
    }
}

impl CamOps for LibGphoto2Ops {
    fn open(&mut self) -> Result<RawIdentity, DeviceError> {
        // Autodetect assumes a single camera, which is what the task specifies. `list_cameras`
        // below is what turns "more than one" into a message naming them, rather than
        // libgphoto2 silently picking the first.
        let found: Vec<_> = self
            .context
            .list_cameras()
            .wait()
            .map_err(transport)?
            .collect();
        if found.len() > 1 {
            let listed = found
                .iter()
                .map(|camera| format!("{} on {}", camera.model, camera.port))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DeviceError::Rejected(format!(
                "{} cameras are connected and this driver drives one: {listed}. \
                 Unplug the others, or wait for multi-camera support.",
                found.len()
            )));
        }

        // Measured at 190–210 ms. The error text is passed through untouched — `super::thread`
        // is what turns a claim failure into a diagnosis, and it can only do that if the
        // original words survive.
        let camera = self.context.autodetect_camera().wait().map_err(transport)?;
        let abilities = camera.abilities();
        let model = abilities.model().into_owned();
        let has_live_view = abilities.camera_operations().capture_preview();
        self.camera = Some(camera);

        Ok(RawIdentity {
            info: DeviceInfo {
                name: model.clone(),
                model,
                // The R10 reports neither over PTP config; `None` is the honest answer and the
                // UI renders it as unknown rather than as an empty string that looks like data.
                firmware: self.text("firmwareversion"),
                // Matters beyond display: calibration masters are matched to the body that shot
                // them, and two identical models are only distinguishable by this (HAL-06).
                serial: self.text("serialnumber"),
                protocol: "PTP/USB (libgphoto2)".to_owned(),
            },
            settings: self.settings(),
            choices: self.choices(),
            has_live_view,
        })
    }

    fn close(&mut self) -> Result<(), DeviceError> {
        // Dropping the `Camera` is the release. It happens on the camera thread because that is
        // the only thread that ever holds one.
        self.camera = None;
        Ok(())
    }

    fn read_settings(&mut self) -> Result<RawSettings, DeviceError> {
        self.camera()?;
        let settings = self.settings();
        // **A camera that answers nothing at all has not answered.** [`Self::radio`] deliberately
        // turns a missing key into an empty value, because that is the honest report for a
        // *particular* control the body does not have — `aperture` behind a fully manual lens.
        // But it cannot tell that apart from every key failing, and M2-T04 measured what that
        // looks like: after a bus-level reset the R10 stayed enumerated and `read_settings`
        // returned `Ok` with `iso=""`, `shutter=""`, `format=""`. A success carrying nothing is
        // worse than a failure — the facade published it as the camera's settings, and the
        // operator would have read an empty panel as a camera with no ISO rather than as a camera
        // that is gone.
        //
        // Aperture is excluded from the test for exactly the reason it is allowed to be empty.
        if settings.iso.is_empty() && settings.shutter.is_empty() && settings.format.is_empty() {
            return Err(DeviceError::Transport(
                "the camera accepted the request and reported no ISO, no shutter speed and no \
                 image format. A body answers at least one of those, so the session is dead even \
                 though the device is still on the bus."
                    .to_owned(),
            ));
        }
        Ok(settings)
    }

    fn read_choices(&mut self) -> Result<RawChoices, DeviceError> {
        self.camera()?;
        let choices = self.choices();
        // The same claim about the choice lists. `capabilities_from` already copes with an empty
        // shutter list (the mode dial on Bulb produces one), so emptiness alone is not the signal
        // — all four being empty is.
        if choices.isos.is_empty()
            && choices.shutters.is_empty()
            && choices.apertures.is_empty()
            && choices.formats.is_empty()
        {
            return Err(DeviceError::Transport(
                "the camera enumerated no settings at all — no ISOs, shutters, apertures or \
                 formats. The session is dead even though the device is still on the bus."
                    .to_owned(),
            ));
        }
        Ok(choices)
    }

    fn write_setting(&mut self, key: CfgKey, value: &str) -> Result<(), DeviceError> {
        let camera = self.camera()?;
        let widget = camera
            .config_key::<RadioWidget>(key.as_str())
            .wait()
            .map_err(|error| {
                DeviceError::Rejected(format!(
                    "the camera has no `{}` setting right now ({error}) — on this body the mode \
                     dial decides which settings exist",
                    key.as_str()
                ))
            })?;
        widget.set_choice(value).map_err(|error| {
            DeviceError::Rejected(format!(
                "the camera refused `{}` for {}: {error}",
                value,
                key.as_str()
            ))
        })?;
        // Measured at 11 ms.
        camera.set_config(&widget).wait().map_err(|error| {
            DeviceError::Rejected(format!(
                "the camera refused `{}` for {}: {error}",
                value,
                key.as_str()
            ))
        })
    }

    fn battery(&mut self) -> Result<BatteryStatus, DeviceError> {
        self.camera()?;
        // `batterylevel` is a text widget reading e.g. `100%`.
        let percent = self
            .text("batterylevel")
            .and_then(|level| level.trim_end_matches('%').trim().parse::<u8>().ok());

        Ok(BatteryStatus {
            // The trait is explicit that a body reporting no battery reports 100 % on external
            // power rather than erroring, because the UI needs a value. A tethered body with no
            // `batterylevel` key is exactly that case.
            percent: percent.unwrap_or(100),
            charging: percent.is_none(),
        })
    }

    fn storage(&mut self) -> Result<StorageInfo, DeviceError> {
        let camera = self.camera()?;
        let storages = camera.storages().wait().map_err(transport)?;

        // A body can report several volumes; the frame lands on whichever the camera chooses, so
        // the useful figure for REL-12 is the sum. One volume on the reference body.
        let mut free_bytes = 0_u64;
        let mut total_bytes = 0_u64;
        for storage in &storages {
            // NOTE: `free_kb`/`capacity_kb` are misnamed in gphoto2 3.4.1 — both accessors
            // multiply libgphoto2's kilobyte fields by 1024 and therefore return **bytes**. Read
            // the crate source, not the method name. Confirmed by arithmetic against the card
            // M2-T01 measured (127.8 GB); a hardware re-check is on the pending list.
            free_bytes = free_bytes.saturating_add(storage.free_kb().unwrap_or(0));
            total_bytes = total_bytes.saturating_add(storage.capacity_kb().unwrap_or(0));
        }

        const BYTES_PER_MB: u64 = 1024 * 1024;
        Ok(StorageInfo {
            free_mb: free_bytes / BYTES_PER_MB,
            total_mb: total_bytes / BYTES_PER_MB,
        })
    }

    fn capture(&mut self) -> Result<RawCapture, DeviceError> {
        let camera = self.camera()?;

        // Measured at 2.08 s on the R10 — and with `capturetarget=Internal RAM` that figure
        // covers the USB transfer as well as the exposure, so **the whole ~32 MB frame is
        // resident inside libgphoto2 when this returns** (M2-T01 finding 2). This one line is
        // where PRF-05's largest single allocation happens, and nothing in this process can see
        // it: it is C memory, it does not appear in a Rust allocator's counters, and it is
        // released when the file is downloaded and freed. Budgeted at 32 MB against 512 MB.
        let path = camera.capture_image().wait().map_err(|error| {
            // A body that will not fire is refusing, not failing to communicate: no card, an
            // autofocus that would not lock, a mode dial that forbids the shutter speed. The
            // operator can act on all three, which `Rejected` says and `Transport` does not.
            DeviceError::Rejected(format!("the camera would not take the picture: {error}"))
        })?;

        let mut files = vec![RawFileRef {
            folder: path.folder().into_owned(),
            name: path.name().into_owned(),
        }];
        // RAW+JPEG produces a second file, which arrives as an event rather than from the call
        // above. On a single-format body this drains nothing and costs one poll window.
        let drained = self.drain_events(EVENT_POLL, false);
        files.extend(drained.files);

        Ok(RawCapture {
            files,
            // Normally `None` on the timed path: the body announces `BulbExposureTime` after a
            // bulb hold, not after a shutter-speed exposure, where the requested figure is the
            // shutter setting and is already exact.
            exposure_seconds: drained.exposure_seconds,
        })
    }

    fn capture_bulb(
        &mut self,
        duration: Duration,
        abort: &AbortSignal,
        since: u64,
        file_wait: Duration,
    ) -> Result<RawCapture, DeviceError> {
        // SDD §5.3.2's procedure, as M2-T01 proved it on the body: press, hold, release.
        let Some((widget, press, release)) = self.bulb_tokens() else {
            // No mechanism at all — not a failure of this camera, a body that cannot do it.
            return Err(DeviceError::Unsupported);
        };

        // The shutter opens here. From this point there is exactly one thing that must happen on
        // every path out of this function, and it is the release.
        //
        // **Clear the decks first.** A bulb exposure that follows another one closely meets
        // `I/O in progress` on the press: libgphoto2's PTP layer is still finishing the previous
        // frame's traffic, and a `set_config` issued into that is refused outright. M2-T03 hit
        // this taking a 60 s frame straight after a 30 s one — which is not an edge case, it is
        // what a sequencer does all night. Pumping the event queue lets the binding finish what
        // it was doing and, as a bonus, collects any event the previous frame left behind, which
        // would otherwise be mistaken for this frame's file below.
        for leftover in self.drain_events(SETTLE_BUDGET, false).files {
            // Not merely dropped: an orphan still occupying the body's buffer is what stops the
            // exposure below producing a file at all.
            self.discard_from_camera(&leftover);
        }

        // **The shutter is released even if the press failed.** That is the ambiguous case:
        // `set_config` may have reached the body and only the acknowledgement been lost, leaving
        // the shutter open with nothing to close it. The release is idempotent — it is what
        // `abort` sends at any time — so attempting it costs a round trip and removes the one
        // failure mode that outlives the request.
        if let Err(error) = self.press_shutter(&widget, &press) {
            let _ = self.commit(&widget, &release);
            return Err(error);
        }

        let opened = std::time::Instant::now();
        let aborted = abort.hold(duration, since);
        let exposed_for = opened.elapsed();

        // Released unconditionally, before anything is allowed to return. The spike flagged the
        // failure of this call as "SHUTTER MAY STILL BE OPEN", which is the one outcome here that
        // damages more than the frame: the sensor keeps integrating with nothing to stop it.
        let released = self.commit(&widget, &release);

        if let Err(error) = released {
            // Reported as `Protocol` and *loudly*, because the recovery is physical.
            tracing::error!(
                %error,
                "the bulb release failed — the shutter may still be open; power-cycle the body \
                 if the next capture also fails"
            );
            return Err(error);
        }

        if aborted {
            // **The body still made an exposure, and the frame has to come off the camera before
            // this returns.** Not for the disk's sake — an aborted capture downloads nothing —
            // but for the *camera's*: with `capturetarget=Internal RAM` the buffer holds one
            // frame, and only a download or a delete frees it. HAL-03 requires the cleanup before
            // the abort resolves; this is the camera-side half of it. See
            // [`discard_from_camera`](Self::discard_from_camera) for what is and is not proven
            // about the consequences of skipping it.
            //
            // The wait scales with how long the shutter was actually open, because the dark-frame
            // pass matches the *real* exposure rather than the requested one — an abort three
            // seconds into a 300 s frame is followed by about three seconds of processing, not
            // three hundred. Capped by the completed path's own budget so a late abort cannot
            // outlast the operation.
            let settle = exposed_for
                .saturating_mul(2)
                .saturating_add(ABORTED_DRAIN_MARGIN)
                .min(file_wait);
            let orphaned = self.drain_events(settle, true);
            for file in &orphaned.files {
                self.discard_from_camera(file);
            }
            tracing::debug!(
                files = orphaned.files.len(),
                exposed_s = exposed_for.as_secs_f64(),
                settle_s = settle.as_secs_f64(),
                "discarded the frame from an aborted bulb exposure"
            );
            return Err(DeviceError::Aborted(
                "the capture was aborted by the operator".to_owned(),
            ));
        }

        // Unlike the timed path there is no returned path: the frame arrives only as an event,
        // and so does the exposure the body reports.
        let drained = self.drain_events(file_wait, true);
        if drained.files.is_empty() {
            return Err(DeviceError::Protocol(format!(
                "the shutter closed on a {:.0} s bulb exposure but the camera had not announced a \
                 file {:.0} s later. A Canon with long-exposure noise reduction on shoots a \
                 matching dark frame first, which roughly doubles the wait — raise \
                 `camera.timeouts.capture_extra_seconds` or turn that off on the body.",
                duration.as_secs_f64(),
                file_wait.as_secs_f64()
            )));
        }

        Ok(RawCapture {
            // The camera's own figure where it has one: M2-T01 read `BulbExposureTime 9` after a
            // 10 s hold, and that second is the difference between the request and the sensor.
            exposure_seconds: drained.exposure_seconds,
            files: drained.files,
        })
    }

    fn download(&mut self, file: &RawFileRef, destination: &Path) -> Result<u64, DeviceError> {
        let camera = self.camera()?;

        // `destination` has already been unlinked by `super::download::land` — this call returns
        // `File exists` rather than truncating, and that is the trap the caller exists to avoid.
        camera
            .fs()
            .download_to(&file.folder, &file.name, destination)
            .wait()
            .map_err(transport)?;

        // 2.67 ms for 32 MB on the reference body, because the bytes were already in RAM.
        std::fs::metadata(destination)
            .map(|meta| meta.len())
            .map_err(|error| {
                DeviceError::Transport(format!(
                    "the download reported success but {} is not readable: {error}",
                    destination.display()
                ))
            })
    }

    fn abort(&mut self) -> Result<(), DeviceError> {
        // Idempotent: releasing a shutter that is already closed is what the body does with a
        // `Release Full` at any other time, and it is not an error. A body with no bulb mechanism
        // has nothing to release, which is also not an error — refusing to stop something is
        // never the right answer to a stopping command (SDD §5.8.1).
        let Some((widget, _press, release)) = self.bulb_tokens() else {
            return Ok(());
        };
        self.commit(&widget, &release)
    }

    fn preview(&mut self) -> Result<Vec<u8>, DeviceError> {
        let camera = self.camera()?;

        // `capture_preview` *is* the start of live view on this body — there is no separate
        // start call, and the first invocation costs 390 ms because that one spins the sensor up
        // (M2-T01 measured it, then 58.5 fps sustained afterwards). The pacing loop above is what
        // decides how often this happens; here it is one frame.
        let file = camera.capture_preview().wait().map_err(transport)?;

        // Handed to the caller exactly as the body produced it. No decode and no re-encode: the
        // bytes go on the wire to the PWA unmodified (SDD §5.7 source 1), and re-encoding 133 KB
        // several times a second on a Pi would be the most expensive thing in the preview path
        // and would buy nothing — the body already produces JPEG.
        let data = file.get_data(&self.context).wait().map_err(transport)?;
        Ok(data.to_vec())
    }

    fn stop_preview(&mut self) -> Result<(), DeviceError> {
        let Ok(camera) = self.camera() else {
            // No camera is the strongest possible guarantee that its sensor is not being held
            // awake.
            return Ok(());
        };

        // `viewfinder` is the R10's own toggle for the mirror/sensor state live view puts it in.
        // A body that does not expose it has no live-view mode to leave, which is `Ok(())` rather
        // than an error — a stopping command may not fail for want of something to stop
        // (SDD §5.8.1), and simply not calling `capture_preview` again has already stopped the
        // traffic. The round trip is worth making where the key exists: a mirrorless body left in
        // live view keeps its sensor powered and its screen lit, and this driver also reports the
        // battery that pays for it.
        let Ok(widget) = camera.config_key::<ToggleWidget>("viewfinder").wait() else {
            tracing::debug!("the camera exposes no `viewfinder` control; nothing to switch off");
            return Ok(());
        };
        widget.set_toggled(false);
        if let Err(error) = camera.set_config(&widget).wait() {
            // Best effort, and logged rather than returned: the operator asked for live view to
            // stop, the frames have stopped, and failing their request over the body's own power
            // management would be answering a stop with a refusal.
            tracing::warn!(%error, "the camera would not leave live view");
        }
        Ok(())
    }
}

/// What one drain of the event queue turned up.
#[derive(Debug, Default)]
struct Drained {
    /// Files the body announced.
    files: Vec<RawFileRef>,
    /// The exposure the body reported, if it reported one.
    exposure_seconds: Option<f64>,
}

/// Extracts the seconds from a `BulbExposureTime` event string, if that is what it is.
///
/// libgphoto2 hands PTP property changes to callers as free text, so this is a parse rather than
/// a field access. The R10's is `BulbExposureTime 9` (M2-T01, for a 10 s hold — the second of
/// difference is the body's own accounting and is exactly why the reported figure is worth
/// having). Matched case-insensitively on the name and tolerantly on the separator, because the
/// format is not a documented interface and a driver that demanded one spelling would silently
/// fall back to echoing the request the day it changed.
///
/// Kept as a free function so it is testable without a camera — the string is the only thing
/// that needs checking, and CI can check it.
fn bulb_exposure_seconds(text: &str) -> Option<f64> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find("bulbexposuretime")?;
    let rest = &text[at + "bulbexposuretime".len()..];
    let number: String = rest
        .trim_start_matches(|c: char| !c.is_ascii_digit() && c != '-')
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let seconds: f64 = number.parse().ok()?;
    // A body reporting a negative or absurd figure is reporting nothing useful; the caller then
    // falls back to the duration it asked for, which is at least a number it can defend.
    (seconds.is_finite() && seconds > 0.0).then_some(seconds)
}

/// Wraps a libgphoto2 error as a transport failure, preserving its exact text.
///
/// The text is load-bearing: [`super::gvfs::is_claim_failure`] branches on it to tell "something
/// else has the camera" from "the camera is not there", which M2-T01 established are
/// distinguishable and which M2-T04's recovery path depends on.
fn transport(error: gphoto2::Error) -> DeviceError {
    DeviceError::Transport(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::bulb_exposure_seconds;

    #[test]
    fn the_bodys_bulb_exposure_event_is_parsed_rather_than_ignored() {
        // The exact string M2-T01 recorded from the R10 after a 10 s hold. Getting this wrong is
        // invisible rather than loud: the caller falls back to the *requested* duration, so the
        // metadata reads exactly what was asked for and looks perfect. M2-T03's first attempt
        // looked for a config key of this name, found none, and echoed the request back for a
        // whole hardware run before the discrepancy with the spike gave it away.
        assert_eq!(bulb_exposure_seconds("BulbExposureTime 9"), Some(9.0));
        // Spelling and separators are not a documented interface, so none of them is demanded.
        assert_eq!(bulb_exposure_seconds("bulbexposuretime: 30"), Some(30.0));
        assert_eq!(
            bulb_exposure_seconds("PTP BulbExposureTime = 120.5"),
            Some(120.5)
        );
    }

    #[test]
    fn an_event_that_is_not_an_exposure_report_yields_nothing() {
        // The caller then keeps the duration it asked for, which is a number it can defend.
        assert_eq!(bulb_exposure_seconds("Unknown PTP property 0xd1b0"), None);
        assert_eq!(bulb_exposure_seconds("BulbExposureTime"), None);
        assert_eq!(bulb_exposure_seconds("BulbExposureTime abc"), None);
        // Zero is not an exposure, and a body reporting one is reporting nothing useful.
        assert_eq!(bulb_exposure_seconds("BulbExposureTime 0"), None);
    }
}

/// Lists the cameras libgphoto2 can see (HAL-08).
///
/// On the blocking pool rather than the camera thread, and that is not an exception to the rule:
/// a probe runs before any driver exists, so there is no camera thread to run it on. It builds
/// its own context, uses it and drops it, all on the one pool thread — the constraint is that a
/// context never *moves* between threads, and this one never does.
pub(crate) async fn probe_cameras() -> Result<Vec<DetectedDevice>, DeviceError> {
    tokio::task::spawn_blocking(|| {
        let context = Context::new().map_err(transport)?;
        let found = context.list_cameras().wait().map_err(transport)?;
        Ok(found
            .map(|camera| {
                DetectedDevice::new(
                    DRIVER_NAME,
                    DeviceKind::Camera,
                    // The port is what addresses the device, e.g. `usb:001,014`.
                    camera.port,
                    camera.model,
                )
            })
            .collect())
    })
    .await
    .map_err(|error| DeviceError::Transport(format!("the camera probe did not finish: {error}")))?
}
