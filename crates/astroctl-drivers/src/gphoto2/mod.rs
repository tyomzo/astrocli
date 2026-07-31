//! The Canon gPhoto2 camera driver — SDD §5.3, PRD CAM-01/02.
//!
//! Built on M2-T01, which put a real Canon EOS R10 on the end of a cable and measured every
//! operation this driver will need (`spikes/gphoto2-r10/FINDINGS.md`). Numbers quoted in these
//! modules are from that spike unless they say otherwise, and where the spike contradicted the
//! design the spike wins.
//!
//! # The shape of it
//!
//! ```text
//!   Camera trait (async)          camera.rs      CanonGPhoto2Camera
//!         │
//!         │  CamCmd  ──std::sync::mpsc──▶  ┌──────────────────────────┐
//!         │                                │  thread "astroctl-camera" │  thread.rs
//!         ◀──tokio::sync::oneshot───────── │  owns the context         │
//!                                          └──────────┬───────────────┘
//!                                                     │  CamOps (blocking)   ops.rs
//!                                                     ▼
//!                                   backend.rs (libgphoto2)  ·  mock.rs (tests)
//! ```
//!
//! Two boundaries, each doing one job:
//!
//! * **The channel** keeps blocking work off the caller's runtime. Everything left of it is
//!   async and quick; everything right of it can take two seconds and does.
//! * **[`ops::CamOps`]** keeps the C library out of the tests. The thread, the timeouts and the
//!   wedge protocol are exercised in CI against [`mock`], on machines with neither camera nor
//!   libgphoto2 — which is every CI machine, and was this workstation until M2-T02 staged the
//!   library by hand.
//!
//! # What this driver does *not* do yet
//!
//! M2-T02 delivers connect, disconnect, settings and status. Capture, bulb and download are
//! M2-T03; live view and the recovery half of the wedge protocol are M2-T04. Each unimplemented
//! operation returns an error that names its task rather than pretending the body cannot do it —
//! see `not_yet_implemented` in [`camera`].
//!
//! # The gvfs steal
//!
//! Worth reading [`gvfs`] before touching anything else in here. A desktop file manager silently
//! taking the camera is the single most likely reason this driver fails on a machine where
//! everything is plugged in and switched on, and libgphoto2's own message for it points nowhere.

mod camera;
mod gvfs;
mod ops;
mod thread;

#[cfg(feature = "libgphoto2")]
mod backend;

#[cfg(test)]
mod mock;

pub use camera::{CanonGPhoto2Camera, CanonGPhoto2CameraFactory, DRIVER_NAME};
