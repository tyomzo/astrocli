//! Resident-set measurement, the same discipline the rawler spike used (M2-T01 FINDINGS:
//! "peak RSS 171 MB -> 172 MB across 20 decodes").
//!
//! Two numbers, and the difference between them is the point:
//!
//! * `VmRSS` — resident *now*. Sampled after each run, this is what shows whether memory is
//!   being returned. A rising series is a leak or a growing allocator arena.
//! * `VmHWM` — the high-water mark, monotonic for the life of the process. This is the number
//!   PRF-05's 512 MB ceiling has to be compared against, because it is the peak the OOM killer
//!   would have seen.
//!
//! A spike that reported only `VmRSS` at the end would report a small number and be wrong: the
//! allocator frees the 96 MB image back before the sample is taken.

use std::fs;

/// A resident-set sample, in bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// `VmRSS` — resident right now.
    pub current: u64,
    /// `VmHWM` — the largest `VmRSS` has ever been in this process.
    pub peak: u64,
}

impl Sample {
    pub fn current_mb(&self) -> f64 {
        self.current as f64 / 1_048_576.0
    }
    pub fn peak_mb(&self) -> f64 {
        self.peak as f64 / 1_048_576.0
    }
}

/// Reads `/proc/self/status`.
///
/// Linux-only, which is the only platform a field node runs and the only one this workstation
/// is. On anything else this returns zeroes rather than failing, because a spike that cannot
/// measure memory should still be able to report its timings.
pub fn sample() -> Sample {
    let Ok(text) = fs::read_to_string("/proc/self/status") else {
        return Sample::default();
    };
    let mut out = Sample::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            out.current = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            out.peak = parse_kb(rest);
        }
    }
    out
}

/// Resets `VmHWM` to the current `VmRSS`.
///
/// Writing `5` to `/proc/self/clear_refs` is the documented way to clear the peak (Linux
/// `Documentation/filesystems/proc.rst`). Without it, `VmHWM` carries the cost of loading the
/// frame for the rest of the process's life, and the single-shot extraction peak — the number
/// PRF-05's 512 MB ceiling actually has to be compared against — cannot be separated from it.
///
/// Returns whether the reset was accepted; on a kernel that does not support it, the caller
/// must fall back to reporting the composed figure and saying so.
pub fn reset_peak() -> bool {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/self/clear_refs")
    else {
        return false;
    };
    f.write_all(b"5\n").is_ok()
}

/// `/proc/self/status` reports `   12345 kB`.
fn parse_kb(field: &str) -> u64 {
    field
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// Simple summary statistics over a series, for the "flat or not" question.
pub struct Series {
    pub values: Vec<f64>,
}

impl Series {
    pub fn new(values: Vec<f64>) -> Self {
        Self { values }
    }

    pub fn min(&self) -> f64 {
        self.values.iter().copied().fold(f64::INFINITY, f64::min)
    }

    pub fn max(&self) -> f64 {
        self.values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
    }

    pub fn mean(&self) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        self.values.iter().sum::<f64>() / self.values.len() as f64
    }

    /// The p-th percentile by nearest rank, which for 20 samples is the honest method — a
    /// linear interpolation between two of twenty samples invents precision.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }

    /// First value minus last — the drift a leak would show.
    pub fn drift(&self) -> f64 {
        match (self.values.first(), self.values.last()) {
            (Some(a), Some(b)) => b - a,
            _ => 0.0,
        }
    }
}
