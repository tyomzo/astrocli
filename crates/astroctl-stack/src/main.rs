//! The stacking-server binary: frame ingest, the calibration index, job control, the rebuild
//! manager, preview and export.
//!
//! Per ADD §5.6 rule 5 it never depends on `astroctl-field`. Booted for real by M0-T05.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.

fn main() {
    println!(
        "astroctl-stack {} — scaffold (M0-T01); bootstrap lands in M0-T05",
        env!("CARGO_PKG_VERSION")
    );
}
