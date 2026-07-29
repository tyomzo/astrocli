//! The hardware abstraction layer: the `MountDevice`, `Camera` and `GuideCamera` async
//! traits, capability structs and the driver registry.
//!
//! This is the extension contract (EXT-01/02) and is semver-stable from Phase 1 on.
//! Filled in by M1-T01. See SDD §5.1.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.
