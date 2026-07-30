//! Motion limits, the manual-slew dead-man's switch and the emergency-stop path — SDD §5.4,
//! §5.8.1, ADR-11.
//!
//! # The shape of this crate is the architecture decision
//!
//! There is one public type that matters, [`SafeMount`], and it implements
//! [`MountDevice`](astroctl_hal::mount::MountDevice) while holding an
//! `Arc<dyn MountDevice>`. That is ADR-11 in code: *the mount facade the API and the orchestrator
//! see **is** the safety wrapper*. Nothing above the HAL can reach the driver another way, so a
//! limit cannot be bypassed by adding a route, an LLM tool (Phase 2c) or a script (MNT-15 names
//! all three), and none of those layers has to remember to ask permission first.
//!
//! The alternative — a `Safety` service the API calls before each command — was rejected for the
//! reason ADR-11 gives: it is correct in review and wrong the first time somebody adds a caller.
//!
//! # Every threshold comes from configuration
//!
//! `mount.limits.*` and `site.*` are constructor inputs. This crate contains no altitude, no
//! meridian angle and no TTL of its own: an operator who moves the telescope to a site with a
//! hill on the southern horizon changes one line of YAML, and a limit compiled into the binary
//! would be a limit they cannot change. The only constants here are cadences — how often the
//! background check looks, how far ahead a manual slew is projected — and they are named and
//! justified where they are declared.
//!
//! # Two consumers, one transform
//!
//! [`horizontal`] converts equatorial to horizontal coordinates for the configured site. Both the
//! altitude limit and the `mount.position` event the operator reads go through it, so a display
//! bug and a limit bug cannot disagree about whether a target is up. See that module for the
//! accuracy this has and for the Phase 2a swap that keeps its signature.
//!
//! # What is not here yet
//!
//! SDD §5.4's watchdog paragraph also lists serial-heartbeat freshness and camera-thread
//! liveness. The disk and clock half of that watchdog lives in the field binary and has since
//! M0-T05; the serial heartbeat needs a driver with a heartbeat, which arrives with the real
//! mount in M3, and the camera thread arrives with M1-T06. The escalation those will use is
//! already exercised here: [`SafeMount`] issues a priority-lane stop by itself when a ramped stop
//! fails during an unauthorised motion.
//!
//! ADD §5.6 is authoritative for the crate layout and the allowed-dependency matrix;
//! `scripts/check-deps.sh` enforces it.

pub mod horizontal;
mod safe_mount;

#[cfg(test)]
mod test_double;
#[cfg(test)]
mod tests;

pub use horizontal::{horizontal, hour_angle_degrees, local_sidereal_degrees, Site};
pub use safe_mount::SafeMount;
