//! Latency probes — the measurement half of T-ISO-1 and T-HOL-1.
//!
//! # Every budget is a ratio, never a constant
//!
//! SDD §9's T-ISO-1 says "p99 latency stays ≤ 2× the idle baseline" and means it literally: the
//! baseline is captured from the same pair, in the same run, minutes earlier. Nothing in this
//! module compares a duration to a number written by a developer, because a suite that did would
//! be measuring the CI runner's mood and would be turned off the first week it went red for it.
//!
//! # Why a probe rather than a loop in the scenario
//!
//! What both tests assert is about latency *during* something — during a blocking capture, during
//! a saturated link. A scenario cannot both drive the load and take the measurement in one task
//! without the measurement's own pauses becoming part of the load. So the probe is a task, started
//! before the load and stopped after it, and the scenario in between does the interesting thing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

/// One request, timed.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    pub elapsed: Duration,
    pub status: u16,
}

/// A running latency measurement of one route.
pub struct Probe {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Vec<Sample>>,
    path: String,
}

impl Probe {
    /// Start polling `path` every `interval` until [`stop`](Self::stop) is called.
    ///
    /// The interval is a floor, not a period: the probe sleeps `interval` *between* requests, so a
    /// route that has become slow produces fewer samples rather than a backlog of overlapping
    /// ones. That matters for the honesty of the p99 — an overlapping probe measures queueing it
    /// caused itself.
    #[must_use]
    pub fn start(client: crate::Client, path: &str, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let route = path.to_owned();
        let handle = tokio::spawn(async move {
            let mut samples = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                let at = Instant::now();
                let reply = client.get(&route).await;
                samples.push(Sample {
                    at,
                    elapsed: reply.elapsed,
                    status: reply.status,
                });
                tokio::time::sleep(interval).await;
            }
            samples
        });
        Self {
            stop,
            handle,
            path: path.to_owned(),
        }
    }

    /// Stop and collect.
    ///
    /// # Panics
    ///
    /// When the probe task panicked — which means the node stopped answering a route the scenario
    /// assumed was up, and is worth surfacing as itself rather than as an empty sample set.
    pub async fn stop(self) -> Measurement {
        self.stop.store(true, Ordering::Relaxed);
        let samples = self
            .handle
            .await
            .unwrap_or_else(|error| panic!("the {} probe died: {error}", self.path));
        Measurement {
            path: self.path,
            samples,
        }
    }
}

/// What a probe collected.
#[derive(Debug, Clone)]
pub struct Measurement {
    pub path: String,
    pub samples: Vec<Sample>,
}

impl Measurement {
    /// Only the samples issued inside a window — used to score the load period out of a probe that
    /// also covered the quiet moments either side of it.
    #[must_use]
    pub fn between(&self, from: Instant, to: Instant) -> Self {
        Self {
            path: self.path.clone(),
            samples: self
                .samples
                .iter()
                .copied()
                .filter(|sample| sample.at >= from && sample.at <= to)
                .collect(),
        }
    }

    /// # Panics
    ///
    /// When there are no samples — a percentile of nothing is not a small number, it is a broken
    /// measurement, and returning zero would let a scenario pass on a probe that never ran.
    #[must_use]
    pub fn p99(&self) -> Duration {
        self.percentile(99.0)
    }

    /// # Panics
    ///
    /// When there are no samples.
    #[must_use]
    pub fn p50(&self) -> Duration {
        self.percentile(50.0)
    }

    /// # Panics
    ///
    /// When there are no samples.
    #[must_use]
    pub fn percentile(&self, p: f64) -> Duration {
        assert!(
            !self.samples.is_empty(),
            "no samples for {} — did the probe run?",
            self.path
        );
        let durations: Vec<Duration> = self.samples.iter().map(|sample| sample.elapsed).collect();
        crate::percentile(&durations, p)
    }

    /// # Panics
    ///
    /// When there are no samples.
    #[must_use]
    pub fn max(&self) -> Duration {
        self.samples
            .iter()
            .map(|sample| sample.elapsed)
            .max()
            .unwrap_or_else(|| panic!("no samples for {}", self.path))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Every sample whose status was not 200.
    ///
    /// A route that answered slowly is the subject of these tests; a route that answered `502`
    /// under load is a different and worse finding, and lumping the two together as "latency" would
    /// hide it.
    #[must_use]
    pub fn failures(&self) -> Vec<Sample> {
        self.samples
            .iter()
            .copied()
            .filter(|sample| sample.status != 200)
            .collect()
    }

    /// A one-line summary for the test log, so a run that passed still says by how much.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.samples.is_empty() {
            return format!("{}: no samples", self.path);
        }
        format!(
            "{}: n={} p50={:.1}ms p99={:.1}ms max={:.1}ms",
            self.path,
            self.len(),
            self.p50().as_secs_f64() * 1000.0,
            self.p99().as_secs_f64() * 1000.0,
            self.max().as_secs_f64() * 1000.0,
        )
    }
}
