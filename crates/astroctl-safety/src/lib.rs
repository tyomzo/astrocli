//! Motion limits, watchdogs and the emergency-stop priority path. The mount facade the API
//! and orchestrator see *is* the safety wrapper (ADR-11), which holds an
//! `Arc<dyn MountDevice>` rather than any concrete driver.
//!
//! Filled in by M1-T05. See SDD §5.4.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.
