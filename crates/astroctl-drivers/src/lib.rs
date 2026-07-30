//! Concrete device drivers — `skywatcher`, `gphoto2` and the simulators — behind the
//! traits of `astroctl-hal`. Feature-gated; `indi`/`alpaca` adapters arrive in Phase 4.
//!
//! Per ADD §5.6 rule 1 this crate may depend only on `astroctl-hal` and `astroctl-core`,
//! and nothing above the HAL may depend on it except the binaries that register drivers.
//! See SDD §5.2, §5.3.
//!
//! | Module | Contents | Feature | Task |
//! |--------|----------|---------|------|
//! | [`simulator`] | `SimulatorMount` and its fault injection (HAL-11) | `simulator` | M1-T02 |
//!
//! The camera and guide-camera simulators (M1-T06) and the Sky-Watcher driver (M3-T02) join
//! them here.
//!
//! # What a driver in this crate may and may not do
//!
//! The rules are in `astroctl_hal`'s crate documentation and are not repeated here, but two of
//! them shape every file below and are worth naming at the door:
//!
//! * **Drivers are silent.** Nothing here knows about the event bus. The mount facade
//!   (SDD §5.6, M1-T03) polls this layer and publishes; a driver that emitted events would put
//!   the same state change on the wire twice and would need a bus in every one of its tests.
//! * **Dropping a future never stops hardware.** A dropped `goto` leaves the mount slewing —
//!   the API answers `202` and drops that future on every single goto, so any other behaviour
//!   would mean nothing ever slews.
//!
//! ADD §5.6 is authoritative for the crate layout and the allowed-dependency matrix;
//! `scripts/check-deps.sh` enforces it.

#[cfg(feature = "simulator")]
pub mod simulator;
