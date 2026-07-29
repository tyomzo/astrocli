//! LLM integration: provider adapters over HTTP, the tool registry and the confirmation
//! tiers.
//!
//! Per ADD §5.6 rule 3 this crate reaches the system only through HTTP calls to the local
//! API (ARC-20); it must never depend on `astroctl-session` or `astroctl-hal`.
//!
//! Scaffolded empty at M0 per ADD §5.6 / SDD §3; filled in from Phase 2c on.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.
