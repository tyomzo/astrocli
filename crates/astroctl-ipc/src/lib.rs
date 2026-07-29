//! The Python worker IPC protocol (ADR-13): versioned JSON messages over stdio, plus the
//! supervision state machine. Stacking-server-internal; never crosses the network.
//!
//! Filled in by M1-T13. See SDD §5.12.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.
