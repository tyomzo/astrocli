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
//! M2-T02 delivered connect, disconnect, settings and status; M2-T03 added capture, bulb,
//! download and abort. Live view and the recovery half of the wedge protocol are M2-T04. Each
//! unimplemented operation returns an error that names its task rather than pretending the body
//! cannot do it — see `not_yet_implemented` in [`camera`].
//!
//! # No CLI fallback, deliberately
//!
//! SDD §5.3.3 designs a `GPhoto2Cli` behind the same [`ops::CamOps`] seam. It is not built.
//! M2-T01 measured every operation working through the bindings on the reference body and M2-T03
//! re-measured capture, bulb, download and abort on the same camera; the populated table is in
//! §5.3.3 and every row reads `bindings`. A second implementation of every operation that no
//! configuration selects and no hardware test exercises is not insurance. `camera.ops_via_cli` is
//! therefore *refused* rather than ignored — see `no_cli_fallback` in [`camera`].
//!
//! # The gvfs steal
//!
//! Worth reading [`gvfs`] before touching anything else in here. A desktop file manager silently
//! taking the camera is the single most likely reason this driver fails on a machine where
//! everything is plugged in and switched on, and libgphoto2's own message for it points nowhere.

mod camera;
mod download;
mod gvfs;
mod ops;
mod thread;

#[cfg(feature = "libgphoto2")]
mod backend;

#[cfg(test)]
mod mock;

pub use camera::{CanonGPhoto2Camera, CanonGPhoto2CameraFactory, DRIVER_NAME};
