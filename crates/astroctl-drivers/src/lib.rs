//! Concrete device drivers — `skywatcher`, `gphoto2` and the simulators — behind the
//! traits of `astroctl-hal`. Feature-gated; `indi`/`alpaca` adapters arrive in Phase 4.
//!
//! Per ADD §5.6 rule 1 this crate may depend only on `astroctl-hal` and `astroctl-core`,
//! and nothing above the HAL may depend on it except the binaries that register drivers.
//! See SDD §5.2, §5.3.
//!
//! | Module | Contents | Feature | Task |
//! |--------|----------|---------|------|
//! | [`simulator`] | `SimulatorMount`, `SimulatorCamera`, `SimulatorGuideCamera` and their fault injection (HAL-11) | `simulator` | M1-T02, M1-T06 |
//! | [`gphoto2`] | `CanonGPhoto2Camera` — camera thread, command channel, timeouts, gvfs diagnosis | `gphoto2` | M2-T02 |
//! | `gphoto2::backend` | the libgphoto2 calls themselves | `libgphoto2` | M2-T02 |
//!
//! The Sky-Watcher driver (M3-T02) joins them here.
//!
//! **Two features for one camera driver.** `gphoto2` is the driver and is on by default;
//! `libgphoto2` adds the binding that talks to the hardware and is off by default, because
//! `libgphoto2_sys` runs `pkg-config` and `bindgen` in its build script and CI has no
//! libgphoto2 — a machine without it cannot *compile* the crate, let alone link it. The manifest
//! argues this at length; the consequence for a reader here is that everything in
//! [`gphoto2`] except `backend` is compiled, linted and tested by every gate.
//!
//! # What a driver in this crate may and may not do
//!
//! The rules are in `astroctl_hal`'s crate documentation and are not repeated here, but three of
//! them shape every file below and are worth naming at the door:
//!
//! * **Drivers are silent.** Nothing here knows about the event bus. The mount facade
//!   (SDD §5.6, M1-T03) polls this layer and publishes; a driver that emitted events would put
//!   the same state change on the wire twice and would need a bus in every one of its tests.
//! * **Dropping a future never stops hardware.** A dropped `goto` leaves the mount slewing —
//!   the API answers `202` and drops that future on every single goto, so any other behaviour
//!   would mean nothing ever slews.
//! * **Long synchronous work belongs on a driver-owned thread.** SDD §7 gives the camera an OS
//!   thread because libgphoto2 blocks; the *simulator* camera has one for a different reason
//!   that applies just as hard — a 24-megapixel frame is real arithmetic, and the field node
//!   runs as few as one runtime worker. A driver that computes on the caller's runtime is a
//!   driver that fails T-ISO-1 (SDD §9) for the whole node.
//!
//! ADD §5.6 is authoritative for the crate layout and the allowed-dependency matrix;
//! `scripts/check-deps.sh` enforces it.

#[cfg(feature = "gphoto2")]
pub mod gphoto2;

#[cfg(feature = "simulator")]
pub mod simulator;
