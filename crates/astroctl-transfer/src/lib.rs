//! The durable field-to-stack transfer queue: persistent job store, sha256 checksums and
//! bounded retry with backoff.
//!
//! Filled in by M1-T11. See SDD §5.10.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.
