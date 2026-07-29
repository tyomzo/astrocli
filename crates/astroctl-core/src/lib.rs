//! Shared types (serde models), events, the configuration schema, auth primitives and
//! unit/coordinate helpers. Every other crate in the workspace depends on this one.
//!
//! Filled in by M0-T02 (domain types + error model), M0-T03 (event schema + bus) and
//! M0-T04 (config). See SDD §4.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.
