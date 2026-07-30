//! The permanent simulator drivers (HAL-11, PRD §4.5, SDD §9).
//!
//! "Permanent" is the point. These are not scaffolding to be deleted when hardware arrives: they
//! are how the system is developed indoors, how CI tests anything that touches a device, and how
//! the two milestones with no telescope in them (M1, M2) are verified at all. SDD §9 makes them
//! first-class and names the property that earns it — **fault injection is a constructor
//! parameter**, so a failure scenario is a value a test writes down rather than a sequence of
//! calls it makes.
//!
//! | Module | Contents | Task |
//! |--------|----------|------|
//! | [`mount`]   | [`SimulatorMount`], the `MountDevice` implementation, and its factory | M1-T02 |
//! | [`motion`]  | axis kinematics: ramps, settle, plans evaluated at an instant | M1-T02 |
//! | [`camera`]  | [`SimulatorCamera`] — synthetic frames, real files, real timing | M1-T06 |
//! | [`guide`]   | [`SimulatorGuideCamera`] — small frames at high cadence, with seeing | M1-T06 |
//! | [`sky`]     | [`StarField`] and the projection: the sky both cameras look at | M1-T06 |
//! | [`fits`]    | the 16-bit FITS writer | M1-T06 |
//! | [`fault`]   | [`FaultPlan`] and [`CameraFaultPlan`] — the scripted failures | M1-T02/T06 |
//! | [`profile`] | every constant, and where it was measured | M1-T02/T06 |
//!
//! # One sky, three devices
//!
//! The mount, the camera and the guide camera are not three independent simulations. The cameras
//! take a [`sky::PointingSource`] at construction and
//! [`SimulatorMount::pointing_source`] is one, so a `goto` moves what the next frame contains —
//! and both cameras hold the same [`StarField`], so they see one sky through two telescopes. A
//! binary wires it in one line per device; a test that does not care hands a camera a bare
//! [`RaDec`](astroctl_core::types::RaDec), which is also a `PointingSource`.
//!
//! # Reading a number out of here
//!
//! [`profile`] records, for each constant, whether hardware produced it. Four groups matter:
//! the mount's 16 ms round trip and 835× goto cruise, and the camera's 2 s capture, 11 ms
//! setting write and 222 ms settings tree, are **measured**; the mount's acceleration constants
//! are **fitted** to the single ramp trace the spike captured; the settle oscillation, the manual
//! slew-speed ladder, every sensor-noise figure and the *whole* guide camera are **not measured
//! at all** and are marked as such. Nothing downstream should depend on a number from the last
//! group, and the comments say which one it is at each site rather than in a table nobody reads.
//!
//! # For the layer above (the mount facade, M1-T03)
//!
//! Three behaviours are easy to get wrong from the outside, and all three are properties of a
//! real mount rather than of this simulation:
//!
//! * **A stopped mount's right ascension keeps climbing.** The axes hold an hour angle and the
//!   sky turns underneath them, so a mount with its drive off reports RA advancing at 15.04″/s.
//!   A facade that treats "position changed" as "the mount is moving" will report a parked mount
//!   as slewing all night. Motion is what [`MountDevice::status`](astroctl_hal::mount::MountDevice::status)
//!   is for.
//! * **A `goto` that loses its axes returns [`DeviceError::Aborted`](astroctl_core::error::DeviceError::Aborted)**,
//!   with a message naming what took them ("goto aborted by an emergency stop"). M1-T02 wrote
//!   this bullet to report that no variant fit — it was `Rejected`, which maps to
//!   `DEVICE_REJECTED`/422 and tells the operator their *request* was bad when in fact their
//!   e-stop worked. M1-T03 added `Aborted`, mapping to `ABORTED`/409. Still treat any error
//!   from `goto` as "the slew did not complete" and read `status()` for what the mount is
//!   doing, rather than switching on the variant.
//! * **`goto` resolves after the settle interval**, not on arrival, so its duration includes
//!   `mount.settle_time_seconds`. That is the HAL's definition of completion — "ready for the
//!   next command" — and it is what makes `Ok(())` a safe moment to open the shutter.
//!
//! # For the layer above (the capture flow, M1-T08)
//!
//! The camera's own module documents its four surprises in full; three of them change what the
//! layer above must be written to expect, so they are repeated here:
//!
//! * **An aborted capture is [`DeviceError::Aborted`](astroctl_core::error::DeviceError::Aborted),
//!   not `Rejected`.** The `Camera` trait documentation still says `Rejected`; it was written
//!   before the variant existed, and M1-T03 added `Aborted` for the mount's identical case
//!   precisely because `Rejected` blames the operator's request for their own abort working.
//!   The trait docs are what needs correcting.
//! * **A capture is 32 MB of file and a second of CPU, and neither is on the runtime.** The
//!   camera owns an OS thread (SDD §7's shape for the real driver) and the waiting is
//!   `tokio::time`, so a paused clock makes a five-minute bulb exposure a microsecond of test —
//!   while the pixels still cost what pixels cost. A full-size frame is a 48 MB FITS and a
//!   17 MB JPEG, which is *larger* than the R10's 32 MB CR3 and is the figure PRF-07's transfer
//!   budget should plan against.
//! * **FITS rows run bottom-up.** The standard numbers image rows from the bottom, so the file's
//!   first row is the frame's last. A preview decoder that forgets it (M1-T09) produces a
//!   preview mirrored in declination — which looks plausible and is wrong.

pub mod camera;
pub mod fault;
pub mod fits;
pub mod guide;
mod imaging;
pub mod motion;
pub mod mount;
mod noise;
pub mod profile;
pub mod sky;

pub use camera::{SimulatorCamera, SimulatorCameraFactory};
pub use fault::{CameraFault, CameraFaultPlan, Fault, FaultPlan, MountCommand};
pub use guide::{SimulatorGuideCamera, SimulatorGuideCameraFactory};
pub use mount::{SimulatorMount, SimulatorMountFactory};
pub use profile::{CameraProfile, GuideCameraProfile, SimulatorProfile};
pub use sky::{PointingSource, StarField};
