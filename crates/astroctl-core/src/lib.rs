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
//!
//! ADD §5.6 is authoritative for the crate layout and the allowed-dependency matrix;
//! `scripts/check-deps.sh` enforces it.

pub mod bus;
pub mod config;
pub mod error;
pub mod event;
pub mod types;
