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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::config::{CameraConfig, CameraTimeouts};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    AvailableSettings, BatteryStatus, CameraCapabilities, CameraSettings, DeviceInfo, ImageFormat,
    StorageInfo,
};
use astroctl_hal::camera::{Camera, CaptureRequest, CaptureResult, LiveViewFrame};
use astroctl_hal::registry::{CameraFactory, DetectedDevice, DriverInitError};
use astroctl_hal::stream::FrameStream;
use async_trait::async_trait;

use super::ops::{
    format_token, interpret_choices, interpret_identity, interpret_settings, BodyGeometry,
    CamOpsFactory, CameraIdentity, CfgKey, RawChoices,
};
use super::thread::{CamCmd, CameraLink, OpClass};

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
#[derive(Debug)]
pub struct CanonGPhoto2Camera {
    /// Builds the blocking half. Held rather than consumed so a reconnect can spawn a second
    /// thread — the first one is never reusable once it has been abandoned.
    ops: Arc<dyn CamOpsFactory>,
    /// The camera thread, once one has been spawned. `None` while disconnected.
    link: Mutex<Option<Arc<CameraLink>>>,
    /// Everything the synchronous trait methods have to answer from.
    cached: Mutex<Cached>,
    /// Operation budgets, from `camera.timeouts`.
    timeouts: CameraTimeouts,
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
        Self {
            ops,
            link: Mutex::new(None),
            cached: Mutex::new(Cached::unconnected()),
            timeouts: config.timeouts,
        }
    }

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
            )?));
        }
        Ok(Arc::clone(slot.as_ref().expect("just spawned")))
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
        let raw: RawChoices = link.request(OpClass::Config, CamCmd::GetChoices).await?;
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
        link.request(OpClass::Config, move |reply| CamCmd::SetSetting {
            key,
            value,
            reply,
        })
        .await
    }
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

/// The error every operation M2-T02 does not implement returns.
///
/// **None of `DeviceError`'s nine variants means "this driver is not finished".** `Unsupported`
/// would say the R10 cannot do it, which is false and is contradicted by `capabilities()` two
/// lines away; `Rejected` would blame the operator's request. `Protocol` is the closest honest
/// fit — it maps to `DEVICE_PROTOCOL`/502, i.e. "your request was fine, the thing behind the API
/// could not serve it", which is exactly the situation — and the message carries the part the
/// variant cannot. Every one of these disappears in M2-T03/T04; none of them is a design.
fn not_yet_implemented(operation: &str, task: &str) -> DeviceError {
    DeviceError::Protocol(format!(
        "the gphoto2 driver cannot {operation} yet — that is {task}. \
         The simulator driver supports it today; set `camera.driver: simulator` to use it."
    ))
}

#[async_trait]
impl Camera for CanonGPhoto2Camera {
    async fn connect(&self) -> Result<(), DeviceError> {
        let link = self.link_or_spawn()?;
        // Re-read on every connect, including a redundant one: the trait says connecting an
        // already-connected camera is `Ok`, and the cheapest correct way to honour that is to
        // ask the camera again. A cached identity replayed after the operator turned the mode
        // dial would describe a body that no longer exists.
        let raw = link.request(OpClass::Connect, CamCmd::Open).await?;
        self.remember(interpret_identity(raw, BodyGeometry::R10));
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        let link = {
            let mut slot = self
                .link
                .lock()
                .expect("the camera link slot is never poisoned");
            slot.take()
        };
        let Some(link) = link else {
            // Disconnecting a disconnected camera is `Ok(())`.
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
        closed
    }

    async fn settings(&self) -> Result<CameraSettings, DeviceError> {
        let raw = self
            .link()?
            .request(OpClass::Config, CamCmd::GetSettings)
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
        link.request(OpClass::Config, move |reply| CamCmd::SetSetting {
            key: CfgKey::ImageFormat,
            value,
            reply,
        })
        .await
    }

    async fn capture(&self, _request: &CaptureRequest) -> Result<CaptureResult, DeviceError> {
        Err(not_yet_implemented("capture", "M2-T03"))
    }

    async fn capture_bulb(
        &self,
        _request: &CaptureRequest,
        _duration: Duration,
    ) -> Result<CaptureResult, DeviceError> {
        Err(not_yet_implemented("take a bulb exposure", "M2-T03"))
    }

    async fn abort_capture(&self) -> Result<(), DeviceError> {
        // A stopping command must never fail for want of something to stop (SDD §5.8.1), and
        // this driver cannot start a capture yet — so there is provably nothing in flight and
        // `Ok(())` is the true answer, not a stub. M2-T03 gives it something to do.
        Ok(())
    }

    async fn live_view_frame(&self) -> Result<LiveViewFrame, DeviceError> {
        Err(not_yet_implemented("produce a live-view frame", "M2-T04"))
    }

    async fn live_view_stream(&self) -> Result<FrameStream<LiveViewFrame>, DeviceError> {
        Err(not_yet_implemented("stream live view", "M2-T04"))
    }

    async fn stop_live_view(&self) -> Result<(), DeviceError> {
        // Idempotent and safe when not running — and it is never running. Same reasoning as
        // `abort_capture`.
        Ok(())
    }

    async fn battery(&self) -> Result<BatteryStatus, DeviceError> {
        self.link()?
            .request(OpClass::Config, CamCmd::GetBattery)
            .await
    }

    async fn storage(&self) -> Result<StorageInfo, DeviceError> {
        self.link()?
            .request(OpClass::Config, CamCmd::GetStorage)
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
}

#[async_trait]
impl CameraFactory for CanonGPhoto2CameraFactory {
    fn name(&self) -> &'static str {
        DRIVER_NAME
    }

    fn create(&self, config: &CameraConfig) -> Result<Arc<dyn Camera>, DriverInitError> {
        #[cfg(feature = "libgphoto2")]
        {
            Ok(Arc::new(CanonGPhoto2Camera::new(
                config,
                Arc::new(super::backend::LibGphoto2Factory::new()),
            )))
        }
        #[cfg(not(feature = "libgphoto2"))]
        {
            let _ = config;
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

    use astroctl_core::config::{CameraConfig, CameraDriver, CameraTimeouts};
    use astroctl_core::error::DeviceError;
    use astroctl_core::types::ImageFormat;
    use astroctl_hal::camera::Camera;
    use astroctl_hal::registry::{CameraFactory, DriverRegistry};

    use super::super::mock::{mock_format_token, MockState};
    use super::{CanonGPhoto2Camera, CanonGPhoto2CameraFactory, DRIVER_NAME};

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
    async fn the_operations_this_task_does_not_implement_name_the_task_that_will() {
        let (_state, camera) = camera();
        camera.connect().await.expect("connects");

        let request = astroctl_hal::camera::CaptureRequest::new("/tmp/astroctl-test", "light_1");
        let error = camera.capture(&request).await.expect_err("M2-T03");
        let DeviceError::Protocol(message) = error else {
            panic!("an unimplemented operation is not the device's fault");
        };
        assert!(message.contains("M2-T03"), "{message}");
        assert!(
            message.contains("simulator"),
            "the message must name what the operator can use today: {message}"
        );

        let error = camera.live_view_stream().await.expect_err("M2-T04");
        assert!(format!("{error}").contains("M2-T04"), "{error}");

        // The two stopping commands are honest `Ok`s rather than stubs: nothing can be running,
        // so "stopped" is the true answer, and SDD §5.8.1 forbids refusing a stop.
        camera.abort_capture().await.expect("nothing to abort");
        camera.stop_live_view().await.expect("nothing to stop");
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
}
