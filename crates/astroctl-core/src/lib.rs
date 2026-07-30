//! Shared types (serde models), events, the configuration schema, auth primitives and
//! unit/coordinate helpers. Every other crate in the workspace depends on this one.
//!
//! Module ownership is fixed so M0-T02, M0-T03 and M0-T04 can proceed in parallel without
//! contending for this file. Each task fills its own modules and does not touch the others:
//!
//! | Module          | Task    | Spec        |
//! |-----------------|---------|-------------|
//! | [`types`]       | M0-T02  | SDD §4.1    |
//! | [`error`]       | M0-T02  | SDD §4.2    |
//! | [`event`]       | M0-T03  | SDD §4.3    |
//! | [`bus`]         | M0-T03  | SDD §4.3/§7 |
//! | [`config`]      | M0-T04  | SDD §4.4, PRD §8.1/§8.2 |
//! | [`image_frame`] | M1-T14  | SDD §5.8.3  |
//!
//! [`image_frame`] arrived later than the table's parallelism argument and for a different
//! reason: M1-T14 gave the `/ws/liveview` envelope a second producer on the stacking server, and
//! a wire format the PWA decodes with one function must have one encoder behind it.
//!
//! ADD §5.6 is authoritative for the crate layout and the allowed-dependency matrix;
//! `scripts/check-deps.sh` enforces it.

pub mod bus;
pub mod config;
pub mod error;
pub mod event;
pub mod image_frame;
pub mod types;
