//! The field-node binary: the axum application, the WebSocket hub, the `/stack/*` reverse
//! proxy and PWA serving.
//!
//! Per ADD §5.6 rule 5 it never depends on `astroctl-stack`; the two share only
//! `astroctl-core`/`astroctl-ipc` and the HTTP contract. Booted for real by M0-T05.
//!
//! Scaffolded by M0-T01 — no functional code yet. ADD §5.6 is authoritative for the
//! crate layout and the allowed-dependency matrix; `scripts/check-deps.sh` enforces it.

fn main() {
    println!(
        "astroctl-field {} — scaffold (M0-T01); bootstrap lands in M0-T05",
        env!("CARGO_PKG_VERSION")
    );
}
