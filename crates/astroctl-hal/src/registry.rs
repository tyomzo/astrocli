//! Driver factories and the registry a config name resolves through — SDD §5.1, HAL-07/HAL-08.
//!
//! # Who registers what, and why it is not this crate
//!
//! `registry.create_mount(cfg.mount.driver.as_str(), &cfg.mount)?` turns `driver: skywatcher` in
//! `field-node.yaml` into an `Arc<dyn MountDevice>`. The registry it is called on is *built by
//! the binary*, which registers the factories it was compiled with (feature-gated). That is not
//! an accident of style: `astroctl-hal` cannot depend on `astroctl-drivers` — the drivers depend
//! on it — so a registry that carried a built-in list of concrete drivers would be a dependency
//! cycle. ADD §5.6 rule 1 states the consequence directly: only the two binaries may name
//! concrete drivers, and the deployable that assembles the system is therefore what supplies the
//! driver set. This module defines the shape of that handoff and nothing about its contents.
//!
//! A factory declares its own [`name`](MountFactory::name) rather than being registered under
//! one, so the string that must match the operator's YAML lives next to the driver that answers
//! to it, in one place. The other end of that match is
//! [`MountDriver::as_str`](astroctl_core::config::MountDriver::as_str).
//!
//! # Construction does no I/O
//!
//! SDD §8.1 sequences startup as "registry builds drivers (**no connect**)". A factory therefore
//! allocates and validates only: opening a serial port, spawning a camera thread that claims a
//! USB device, or anything else that can fail because hardware is absent belongs in
//! `connect()`. The reason is behavioural, not aesthetic — the field node must start, serve
//! `/api/system/health` and tell the operator what is wrong even when nothing is plugged in.
//! Spawning a task the driver owns is fine; blocking or touching a device is not.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use astroctl_core::config::{CameraConfig, GuideCameraConfig, MountConfig};
use astroctl_core::error::{CoreError, DeviceError};
use astroctl_core::types::DeviceKind;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::camera::Camera;
use crate::guide::GuideCamera;
use crate::mount::MountDevice;

// -----------------------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------------------

/// A driver refused to be built from the configuration it was given.
///
/// Deliberately not a [`DeviceError`]: nothing has been talked to yet. This is a startup
/// failure, and the operator's next action is to edit YAML — so the message must name the value
/// that is wrong, in the vocabulary of the config file.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DriverInitError(String);

impl DriverInitError {
    /// Builds an initialization failure from an operator-facing message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// A config value that failed a domain constructor — a park position outside the sky, say — is
/// an initialization failure with the constructor's own message, which already names the
/// quantity and the range.
impl From<CoreError> for DriverInitError {
    fn from(error: CoreError) -> Self {
        Self(error.to_string())
    }
}

/// What can go wrong between a name in the config and a device.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The configuration names a driver this build does not have.
    #[error("no {kind} driver named `{name}` in this build; available: {}", driver_list(.available))]
    UnknownDriver {
        /// Which slot was being filled.
        kind: DeviceKind,
        /// The name the configuration asked for.
        name: String,
        /// What this build does have, sorted — the operator's actual options.
        available: Vec<&'static str>,
    },
    /// Two factories claim the same name for the same slot. A programming error in the binary's
    /// registration table, caught at startup rather than by whichever one happened to win.
    #[error("a {kind} driver named `{name}` is already registered")]
    DuplicateDriver {
        /// Which slot.
        kind: DeviceKind,
        /// The contested name.
        name: &'static str,
    },
    /// The driver exists but refused this configuration.
    #[error("{kind} driver `{name}` cannot be built from this configuration")]
    Construction {
        /// Which slot.
        kind: DeviceKind,
        /// The driver that refused.
        name: String,
        /// Why — the driver's own message.
        #[source]
        source: DriverInitError,
    },
}

/// Renders the available-driver list, saying "none" out loud rather than trailing off after a
/// colon — an empty list means the binary registered nothing, which is a different bug from a
/// misspelled name and should not look like one.
fn driver_list(available: &[&'static str]) -> String {
    if available.is_empty() {
        "(none — this build registered no drivers for that slot)".to_owned()
    } else {
        available.join(", ")
    }
}

// -----------------------------------------------------------------------------------------
// Detection (HAL-08)
// -----------------------------------------------------------------------------------------

/// A device a driver found by itself (HAL-08): a serial port matching a known USB VID/PID, an
/// entry from gphoto2's autodetect list, a simulator reporting its own existence.
///
/// The point of `address` is that it is *paste-able*: it is exactly what the operator would put
/// in the config field that addresses this device (`mount.port`), so detection can drive a setup
/// UI rather than merely proving something is plugged in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedDevice {
    /// The driver that found it — the name to select in configuration.
    pub driver: String,
    /// Which slot it would fill.
    pub kind: DeviceKind,
    /// The config value that addresses it, e.g. `/dev/ttyUSB0` or `usb:001,014`.
    pub address: String,
    /// Human-readable identification, e.g. `Canon EOS R10`.
    pub label: String,
}

impl DetectedDevice {
    /// Reports a found device. `driver` is overwritten by the registry with the name the factory
    /// is registered under, so a probe cannot mislabel its own results.
    #[must_use]
    pub fn new(
        driver: impl Into<String>,
        kind: DeviceKind,
        address: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            driver: driver.into(),
            kind,
            address: address.into(),
            label: label.into(),
        }
    }
}

/// A probe that could not run.
///
/// Kept alongside the results instead of replacing them: a scan that fails because `/dev` is not
/// readable must not hide the camera that a different driver did find. The operator needs both
/// halves — what was found, and what could not be looked for.
#[derive(Debug)]
pub struct ProbeFailure {
    /// The driver whose probe failed.
    pub driver: &'static str,
    /// The slot it probes for.
    pub kind: DeviceKind,
    /// Why it failed.
    pub error: DeviceError,
}

/// Everything one sweep of the registry turned up.
#[derive(Debug, Default)]
pub struct ProbeResults {
    /// Devices found, in registration order per slot.
    pub found: Vec<DetectedDevice>,
    /// Probes that could not run.
    pub failures: Vec<ProbeFailure>,
}

// -----------------------------------------------------------------------------------------
// Factories
// -----------------------------------------------------------------------------------------

/// Builds mount drivers of one kind and, where the transport allows, finds them (HAL-07/08).
///
/// Implemented on a unit struct next to the driver:
///
/// ```ignore
/// #[derive(Debug)]
/// pub struct SimulatorMountFactory;
///
/// #[async_trait]
/// impl MountFactory for SimulatorMountFactory {
///     fn name(&self) -> &'static str { "simulator" }
///     fn create(&self, config: &MountConfig) -> Result<Arc<dyn MountDevice>, DriverInitError> {
///         Ok(Arc::new(SimulatorMount::new(config)?))
///     }
/// }
/// ```
#[async_trait]
pub trait MountFactory: Send + Sync + 'static {
    /// The config name this factory answers to — the `mount.driver` value, verbatim.
    fn name(&self) -> &'static str;

    /// Builds the driver. Allocation and validation only; see the module docs on why this does
    /// no I/O.
    ///
    /// # Errors
    /// [`DriverInitError`] naming the configuration value that makes this driver unusable.
    fn create(&self, config: &MountConfig) -> Result<Arc<dyn MountDevice>, DriverInitError>;

    /// Looks for devices this driver could drive (HAL-08).
    ///
    /// Optional — the default finds nothing, which is the honest answer for a driver whose
    /// transport cannot be enumerated. A probe must be read-only and must not disturb a device
    /// already in use: it runs while a session may be in progress.
    ///
    /// # Errors
    /// [`DeviceError`] if the scan itself failed (no permission on `/dev`, a helper binary
    /// missing). Finding nothing is `Ok(vec![])`, not an error.
    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        Ok(Vec::new())
    }
}

/// Builds imaging camera drivers (HAL-07/08). See [`MountFactory`] for the shape and the rules.
#[async_trait]
pub trait CameraFactory: Send + Sync + 'static {
    /// The config name this factory answers to — the `camera.driver` value, verbatim.
    fn name(&self) -> &'static str;

    /// Builds the driver; no I/O.
    ///
    /// # Errors
    /// [`DriverInitError`] naming the configuration value that makes this driver unusable.
    fn create(&self, config: &CameraConfig) -> Result<Arc<dyn Camera>, DriverInitError>;

    /// Looks for cameras this driver could drive (HAL-08).
    ///
    /// # Errors
    /// [`DeviceError`] if the scan itself failed.
    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        Ok(Vec::new())
    }
}

/// Builds guide camera drivers (HAL-07/08). See [`MountFactory`] for the shape and the rules.
#[async_trait]
pub trait GuideCameraFactory: Send + Sync + 'static {
    /// The config name this factory answers to — the `guide_camera.driver` value, verbatim.
    fn name(&self) -> &'static str;

    /// Builds the driver; no I/O.
    ///
    /// # Errors
    /// [`DriverInitError`] naming the configuration value that makes this driver unusable.
    fn create(&self, config: &GuideCameraConfig) -> Result<Arc<dyn GuideCamera>, DriverInitError>;

    /// Looks for guide cameras this driver could drive (HAL-08).
    ///
    /// # Errors
    /// [`DeviceError`] if the scan itself failed.
    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        Ok(Vec::new())
    }
}

// -----------------------------------------------------------------------------------------
// The registry
// -----------------------------------------------------------------------------------------

/// Config name → driver, for each of the three device slots (SDD §5.1, HAL-07).
///
/// Built once at startup and then read-only, so it is shared as a plain `&` rather than behind a
/// lock. Hot-swapping a driver at runtime (HAL-14) is a Phase 4 "could", and would be a second
/// registry operation, not a change to this one.
#[derive(Default)]
pub struct DriverRegistry {
    mounts: BTreeMap<&'static str, Arc<dyn MountFactory>>,
    cameras: BTreeMap<&'static str, Arc<dyn CameraFactory>>,
    guide_cameras: BTreeMap<&'static str, Arc<dyn GuideCameraFactory>>,
}

impl DriverRegistry {
    /// An empty registry. The binary fills it; see the module docs for why it is not prefilled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a mount driver under the name the factory declares.
    ///
    /// Chainable through `?`, which is how a startup table reads:
    /// `registry.register_mount(A)?.register_mount(B)?;`
    ///
    /// # Errors
    /// [`RegistryError::DuplicateDriver`] if that name is already taken — two drivers answering
    /// to one config value is a bug in the binary, and silently keeping one of them would make
    /// which driver the operator gets depend on registration order.
    pub fn register_mount(
        &mut self,
        factory: impl MountFactory,
    ) -> Result<&mut Self, RegistryError> {
        let name = factory.name();
        if self.mounts.contains_key(name) {
            return Err(RegistryError::DuplicateDriver {
                kind: DeviceKind::Mount,
                name,
            });
        }
        self.mounts.insert(name, Arc::new(factory));
        Ok(self)
    }

    /// Registers a camera driver under the name the factory declares.
    ///
    /// # Errors
    /// [`RegistryError::DuplicateDriver`] if that name is already taken.
    pub fn register_camera(
        &mut self,
        factory: impl CameraFactory,
    ) -> Result<&mut Self, RegistryError> {
        let name = factory.name();
        if self.cameras.contains_key(name) {
            return Err(RegistryError::DuplicateDriver {
                kind: DeviceKind::Camera,
                name,
            });
        }
        self.cameras.insert(name, Arc::new(factory));
        Ok(self)
    }

    /// Registers a guide camera driver under the name the factory declares.
    ///
    /// # Errors
    /// [`RegistryError::DuplicateDriver`] if that name is already taken.
    pub fn register_guide_camera(
        &mut self,
        factory: impl GuideCameraFactory,
    ) -> Result<&mut Self, RegistryError> {
        let name = factory.name();
        if self.guide_cameras.contains_key(name) {
            return Err(RegistryError::DuplicateDriver {
                kind: DeviceKind::GuideCamera,
                name,
            });
        }
        self.guide_cameras.insert(name, Arc::new(factory));
        Ok(self)
    }

    /// Builds the mount driver named by the configuration (SDD §5.1).
    ///
    /// # Errors
    /// [`RegistryError::UnknownDriver`], listing what this build does have — a driver compiled
    /// out is the likeliest cause and the operator cannot see the feature flags — or
    /// [`RegistryError::Construction`] carrying the driver's own refusal.
    pub fn create_mount(
        &self,
        name: &str,
        config: &MountConfig,
    ) -> Result<Arc<dyn MountDevice>, RegistryError> {
        let factory = self
            .mounts
            .get(name)
            .ok_or_else(|| RegistryError::UnknownDriver {
                kind: DeviceKind::Mount,
                name: name.to_owned(),
                available: self.mount_drivers(),
            })?;
        factory
            .create(config)
            .map_err(|source| RegistryError::Construction {
                kind: DeviceKind::Mount,
                name: name.to_owned(),
                source,
            })
    }

    /// Builds the camera driver named by the configuration.
    ///
    /// # Errors
    /// As [`create_mount`](Self::create_mount).
    pub fn create_camera(
        &self,
        name: &str,
        config: &CameraConfig,
    ) -> Result<Arc<dyn Camera>, RegistryError> {
        let factory = self
            .cameras
            .get(name)
            .ok_or_else(|| RegistryError::UnknownDriver {
                kind: DeviceKind::Camera,
                name: name.to_owned(),
                available: self.camera_drivers(),
            })?;
        factory
            .create(config)
            .map_err(|source| RegistryError::Construction {
                kind: DeviceKind::Camera,
                name: name.to_owned(),
                source,
            })
    }

    /// Builds the guide camera driver named by the configuration.
    ///
    /// `guide_camera.driver: null` disables guiding hardware entirely (PRD §8.1), so the caller
    /// decides whether to call this at all — an absent guide camera is a configuration, not a
    /// failure.
    ///
    /// # Errors
    /// As [`create_mount`](Self::create_mount).
    pub fn create_guide_camera(
        &self,
        name: &str,
        config: &GuideCameraConfig,
    ) -> Result<Arc<dyn GuideCamera>, RegistryError> {
        let factory = self
            .guide_cameras
            .get(name)
            .ok_or_else(|| RegistryError::UnknownDriver {
                kind: DeviceKind::GuideCamera,
                name: name.to_owned(),
                available: self.guide_camera_drivers(),
            })?;
        factory
            .create(config)
            .map_err(|source| RegistryError::Construction {
                kind: DeviceKind::GuideCamera,
                name: name.to_owned(),
                source,
            })
    }

    /// Mount driver names in this build, sorted (HAL-08 "list available drivers").
    ///
    /// Reported by `/api/system/info` (SDD §5.8.1), which is how a support question about a
    /// missing driver gets answered without asking which build someone is running.
    #[must_use]
    pub fn mount_drivers(&self) -> Vec<&'static str> {
        self.mounts.keys().copied().collect()
    }

    /// Camera driver names in this build, sorted.
    #[must_use]
    pub fn camera_drivers(&self) -> Vec<&'static str> {
        self.cameras.keys().copied().collect()
    }

    /// Guide camera driver names in this build, sorted.
    #[must_use]
    pub fn guide_camera_drivers(&self) -> Vec<&'static str> {
        self.guide_cameras.keys().copied().collect()
    }

    /// Asks every registered driver what it can see (HAL-08).
    ///
    /// Sequential rather than concurrent: a probe touches a shared resource — the USB bus, the
    /// serial subsystem — and two drivers enumerating it at once is a way to make detection
    /// flaky for no gain, since this runs on operator demand and not on a deadline. A failing
    /// probe is recorded and the sweep continues.
    pub async fn probe_all(&self) -> ProbeResults {
        let mut results = ProbeResults::default();

        for (name, factory) in &self.mounts {
            let outcome = factory.probe().await;
            results.absorb(name, DeviceKind::Mount, outcome);
        }
        for (name, factory) in &self.cameras {
            let outcome = factory.probe().await;
            results.absorb(name, DeviceKind::Camera, outcome);
        }
        for (name, factory) in &self.guide_cameras {
            let outcome = factory.probe().await;
            results.absorb(name, DeviceKind::GuideCamera, outcome);
        }

        results
    }
}

impl ProbeResults {
    /// Files one factory's answer, stamping the driver name so a probe cannot report a name the
    /// operator could not then select.
    fn absorb(
        &mut self,
        driver: &'static str,
        kind: DeviceKind,
        outcome: Result<Vec<DetectedDevice>, DeviceError>,
    ) {
        match outcome {
            Ok(found) => self.found.extend(found.into_iter().map(|mut device| {
                driver.clone_into(&mut device.driver);
                device.kind = kind;
                device
            })),
            Err(error) => self.failures.push(ProbeFailure {
                driver,
                kind,
                error,
            }),
        }
    }
}

/// Lists what is registered, which is the useful thing to see in a startup log line; the
/// factories themselves have nothing to show.
impl fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverRegistry")
            .field("mounts", &self.mount_drivers())
            .field("cameras", &self.camera_drivers())
            .field("guide_cameras", &self.guide_camera_drivers())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use astroctl_core::config::MountDriver;

    use super::{DriverRegistry, RegistryError};
    use crate::test_double::{
        mount_config, FakeCameraFactory, FakeGuideCameraFactory, FakeMountFactory,
    };

    fn registry() -> DriverRegistry {
        let mut registry = DriverRegistry::new();
        registry
            .register_mount(FakeMountFactory::named("simulator"))
            .expect("first registration")
            .register_mount(FakeMountFactory::named("skywatcher"))
            .expect("second registration");
        registry
            .register_camera(FakeCameraFactory)
            .expect("camera registration");
        registry
            .register_guide_camera(FakeGuideCameraFactory)
            .expect("guide camera registration");
        registry
    }

    #[test]
    fn a_config_name_resolves_to_a_device() {
        let registry = registry();
        let config = mount_config(MountDriver::Simulator);

        // The exact call the binary makes at startup (SDD §5.1).
        let mount = registry
            .create_mount(config.driver.as_str(), &config)
            .expect("the simulator is registered");
        assert_eq!(mount.device_info().protocol, "fake");
    }

    #[test]
    fn an_unknown_name_says_what_this_build_actually_has() {
        let registry = registry();
        let config = mount_config(MountDriver::Indi);

        let err = registry
            .create_mount(config.driver.as_str(), &config)
            .expect_err("no INDI adapter in this build");
        let message = err.to_string();
        assert!(matches!(err, RegistryError::UnknownDriver { .. }));
        // Names the slot, the missing driver and the options — the three things the operator
        // needs to fix their YAML without reading the source.
        assert_eq!(
            message,
            "no mount driver named `indi` in this build; available: simulator, skywatcher"
        );
    }

    #[test]
    fn an_empty_slot_says_so_rather_than_trailing_off() {
        let registry = DriverRegistry::new();
        let config = mount_config(MountDriver::Simulator);
        let message = registry
            .create_mount(config.driver.as_str(), &config)
            .expect_err("nothing is registered")
            .to_string();
        assert!(
            message.ends_with("this build registered no drivers for that slot)"),
            "{message}"
        );
    }

    #[test]
    fn two_drivers_cannot_claim_one_name() {
        let mut registry = DriverRegistry::new();
        registry
            .register_mount(FakeMountFactory::named("simulator"))
            .expect("first wins");
        let err = registry
            .register_mount(FakeMountFactory::named("simulator"))
            .expect_err("second is refused");
        assert_eq!(
            err.to_string(),
            "a mount driver named `simulator` is already registered"
        );
    }

    #[test]
    fn a_driver_refusing_its_configuration_says_which_driver_and_why() {
        let mut registry = DriverRegistry::new();
        registry
            .register_mount(FakeMountFactory::refusing(
                "simulator",
                "port `auto` needs udev",
            ))
            .expect("registers");
        let config = mount_config(MountDriver::Simulator);

        let err = registry
            .create_mount(config.driver.as_str(), &config)
            .expect_err("the factory refuses");
        assert!(matches!(err, RegistryError::Construction { .. }));
        assert_eq!(
            err.to_string(),
            "mount driver `simulator` cannot be built from this configuration"
        );
        // The reason is the source, so a binary printing the chain shows both halves.
        assert_eq!(
            std::error::Error::source(&err)
                .expect("a source")
                .to_string(),
            "port `auto` needs udev"
        );
    }

    #[test]
    fn the_registry_lists_its_drivers_sorted_for_system_info() {
        let registry = registry();
        assert_eq!(registry.mount_drivers(), ["simulator", "skywatcher"]);
        assert_eq!(registry.camera_drivers(), ["simulator"]);
        assert_eq!(registry.guide_camera_drivers(), ["simulator"]);
        assert!(format!("{registry:?}").contains("skywatcher"));
    }

    #[tokio::test]
    async fn probing_reports_what_was_found_and_what_could_not_be_looked_for() {
        let mut registry = DriverRegistry::new();
        registry
            // Reports itself, as the simulators do (task M1-T01 scope).
            .register_mount(FakeMountFactory::named("simulator"))
            .expect("registers")
            // ...and one that cannot scan, e.g. no permission on /dev.
            .register_mount(FakeMountFactory::unprobeable("skywatcher"))
            .expect("registers");
        registry
            .register_camera(FakeCameraFactory)
            .expect("registers");

        let results = registry.probe_all().await;

        assert_eq!(results.found.len(), 2, "{:?}", results.found);
        let mount = &results.found[0];
        assert_eq!(mount.driver, "simulator");
        assert_eq!(mount.kind, astroctl_core::types::DeviceKind::Mount);
        assert_eq!(mount.address, "simulator");

        // A driver that lied about its own name is corrected, not trusted.
        assert!(results.found.iter().all(|d| d.driver != "not-my-name"));

        assert_eq!(results.failures.len(), 1);
        assert_eq!(results.failures[0].driver, "skywatcher");
    }

    #[test]
    fn a_driver_with_no_probe_simply_finds_nothing() {
        // The default `probe` body is the honest answer for an unenumerable transport; asserting
        // it here keeps a future edit from turning "cannot scan" into a startup failure.
        let registry = registry();
        let found = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(registry.probe_all());
        assert!(found.failures.is_empty());
        assert_eq!(found.found.len(), 3, "{:?}", found.found);
    }
}
