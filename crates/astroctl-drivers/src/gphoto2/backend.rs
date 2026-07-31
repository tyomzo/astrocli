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

use astroctl_core::error::DeviceError;
use astroctl_core::types::{BatteryStatus, DeviceInfo, DeviceKind, StorageInfo};
use astroctl_hal::registry::DetectedDevice;
use gphoto2::widget::{RadioWidget, TextWidget};
use gphoto2::{Camera, Context};

use super::camera::DRIVER_NAME;
use super::ops::{CamOps, CamOpsFactory, CfgKey, RawChoices, RawIdentity, RawSettings};

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
        Ok(self.settings())
    }

    fn read_choices(&mut self) -> Result<RawChoices, DeviceError> {
        self.camera()?;
        Ok(self.choices())
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
}

/// Wraps a libgphoto2 error as a transport failure, preserving its exact text.
///
/// The text is load-bearing: [`super::gvfs::is_claim_failure`] branches on it to tell "something
/// else has the camera" from "the camera is not there", which M2-T01 established are
/// distinguishable and which M2-T04's recovery path depends on.
fn transport(error: gphoto2::Error) -> DeviceError {
    DeviceError::Transport(error.to_string())
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
