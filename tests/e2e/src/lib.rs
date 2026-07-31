//! The M1 end-to-end suite — SDD §9 (T-E2E-1, T-HOL-1, T-ISO-1) and IMP §2/M1's exit criteria.
//!
//! This crate is a **client**. It never links the product crates, never reaches behind the HTTP
//! surface, and never shares a serde type with the server. Everything it asserts about the wire
//! is written out here as literal strings and hand-rolled shapes, which is the whole value: a
//! suite that deserialised into `astroctl_core::event::Event` would keep passing through a rename
//! that broke every real client, because both sides would move together. The cost is that a field
//! name appears twice in the repository. That is the price of a contract test and it is worth it.
//!
//! # What it drives
//!
//! The M0-T08 container pair — `deploy/compose.yaml`, two containers in two network namespaces
//! addressing each other by service name, with only the field node's port published. Not two
//! processes on loopback. That choice is what makes "kill the stacking server" a `compose stop`,
//! what makes `scripts/shape-link.sh` able to put a genuine 1 Mbit link between them, and what
//! makes the `/stack/*` proxy carry real cross-namespace traffic rather than a loopback shortcut.
//!
//! # How to run it
//!
//! `scripts/e2e.sh`. It builds both images, brings the pair up, and runs `cargo test` here with
//! `--test-threads=1`. Running `cargo test` in this directory by hand works too, provided a pair
//! is already up; every scenario attaches to whatever is running rather than starting its own.
//!
//! # Two rules every scenario follows
//!
//! **Serialise.** There is one container pair and the scenarios drive it destructively — a
//! scenario stops a node, saturates a link, or shapes the bridge. [`Harness::attach`] takes a
//! cross-process lock so that a stray parallel `cargo test` waits rather than interleaving, and
//! the runner passes `--test-threads=1` so the common case never reaches the lock at all.
//!
//! **Measure in-run.** No latency, cadence or throughput figure is ever compared against a
//! constant compiled into this crate. Every budget is expressed as a ratio to a baseline captured
//! from the same pair in the same run, seconds earlier — because the alternative is a suite whose
//! verdict depends on how busy the developer's laptop is, and a suite that fails for that reason
//! is a suite that gets `#[ignore]`d within a month. SDD §9's T-ISO-1 says this explicitly; the
//! rest of the suite follows the same rule.
//!
//! [`Harness::attach`]: harness::Harness::attach

pub mod client;
pub mod events;
pub mod harness;
pub mod latency;
pub mod liveview;
pub mod replay;

pub use client::Client;
pub use events::{Event, EventStream};
pub use harness::Harness;

use std::time::{Duration, Instant};

/// Poll `condition` until it returns `Some`, or fail with `what`.
///
/// The interval is 50 ms rather than something adaptive: every wait in this suite is bounded by
/// seconds and the pair is on loopback, so a fixed poll is both fast enough to keep a scenario
/// short and slow enough that the polling itself never shows up in a latency measurement taken
/// beside it.
///
/// # Panics
///
/// When `timeout` elapses without `condition` producing a value.
pub async fn wait_until<T, F, Fut>(what: &str, timeout: Duration, mut condition: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    let mut attempts = 0_u32;
    loop {
        if let Some(value) = condition().await {
            return value;
        }
        attempts += 1;
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} and {attempts} attempts waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The p-th percentile of `samples`, by the nearest-rank method.
///
/// Nearest-rank rather than an interpolating definition because these samples are latencies of
/// individual requests: the 99th percentile of 200 requests should *be* one of the requests that
/// happened, not an average of two that did. Interpolation would quietly soften exactly the tail
/// this suite exists to watch.
///
/// # Panics
///
/// When `samples` is empty.
#[must_use]
pub fn percentile(samples: &[Duration], p: f64) -> Duration {
    assert!(!samples.is_empty(), "no samples to take a percentile of");
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    // Nearest rank: ceil(p/100 * n), clamped into the index range. The casts are lossless at any
    // sample count this suite produces (a probe runs for seconds at millisecond intervals), and
    // the clamp makes a hypothetical loss harmless rather than out of bounds.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}
