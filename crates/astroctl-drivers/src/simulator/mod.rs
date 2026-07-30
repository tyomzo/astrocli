//! The permanent simulator drivers (HAL-11, PRD §4.5, SDD §9).
//!
//! "Permanent" is the point. These are not scaffolding to be deleted when hardware arrives: they
//! are how the system is developed indoors, how CI tests anything that touches a device, and how
//! the two milestones with no telescope in them (M1, M2) are verified at all. SDD §9 makes them
//! first-class and names the property that earns it — **fault injection is a constructor
//! parameter**, so a failure scenario is a value a test writes down rather than a sequence of
//! calls it makes.
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`mount`]   | [`SimulatorMount`], the `MountDevice` implementation, and its factory |
//! | [`motion`]  | axis kinematics: ramps, settle, plans evaluated at an instant |
//! | [`fault`]   | [`FaultPlan`] — the scripted failures |
//! | [`profile`] | [`SimulatorProfile`] — every timing constant, and where it was measured |
//!
//! # Reading a number out of here
//!
//! [`profile`] records, for each constant, whether a telescope produced it. Three groups matter:
//! the 16 ms round trip and the 835× goto cruise are **measured**; the acceleration constants are
//! **fitted** to the single ramp trace the spike captured; the settle oscillation and the manual
//! slew-speed ladder are **not measured at all** and are marked as such. Nothing downstream
//! should depend on a number from the third group, and the comments say which one it is at each
//! site rather than in a table nobody reads.
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

pub mod fault;
pub mod motion;
pub mod mount;
pub mod profile;

pub use fault::{Fault, FaultPlan, MountCommand};
pub use mount::{SimulatorMount, SimulatorMountFactory};
pub use profile::SimulatorProfile;
