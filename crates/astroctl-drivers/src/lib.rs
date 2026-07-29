//! Concrete device drivers — `skywatcher`, `gphoto2` and the simulators — behind the
//! traits of `astroctl-hal`. Feature-gated; `indi`/`alpaca` adapters arrive in Phase 4.
//!
//! Per ADD §5.6 rule 1 this crate may depend only on `astroctl-hal` and `astroctl-core`,
//! and nothing above the HAL may depend on it except the binaries that register drivers.
//! Filled in by M1-T02/T06, M2 and M3. See SDD §5.2, §5.3.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.
