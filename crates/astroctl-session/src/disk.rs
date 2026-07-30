//! Free space on the volume the frames land on, and the REL-12 thresholds that gate capture.
//!
//! The store answers the question; it does not act on it. REL-12's pause semantics — warn, then
//! pause *after* the in-flight frame — live in the capture flow, because only that layer knows what
//! "in flight" means. What lives here is the refusal at [`crate::Session::begin_frame`], which is
//! the backstop: a capture that starts below the critical threshold is a capture that fails
//! mid-write, and a half-written frame is the failure mode REL-05 exists to prevent.

use std::path::Path;

use astroctl_core::config::StorageConfig;
use serde::Serialize;

/// Bytes in one gigabyte, as this system counts them.
///
/// **2^30, not 10^9.** The same constant, for the same reason, as `astroctl-stack::vitals` and
/// `astroctl-field`: neither PRD §8.1 nor SDD §4.3 defines the unit behind `disk_warn_free_gb`, the
/// two conventions differ by 7%, and the binary one reports the smaller number for the same free
/// space — so a safety threshold fires slightly early rather than slightly late, and the figure
/// matches what `df -h` shows the operator who goes and checks. All three must agree, because the
/// operator compares them.
const BYTES_PER_GB: f64 = (1_u64 << 30) as f64;

/// The REL-12 thresholds, taken from `storage.*` (PRD §8.1). No default lives in this crate: a
/// threshold the operator cannot see in their own configuration file is a threshold they cannot
/// tune, and the config validator already guarantees `critical < warn`.
#[derive(Clone, Copy, Debug)]
pub struct DiskThresholds {
    /// Free space below which a warning alert is raised.
    pub warn_gb: f64,
    /// Free space below which capture stops.
    pub critical_gb: f64,
}

impl From<&StorageConfig> for DiskThresholds {
    fn from(storage: &StorageConfig) -> Self {
        Self {
            warn_gb: storage.disk_warn_free_gb,
            critical_gb: storage.disk_critical_free_gb,
        }
    }
}

/// Which side of the thresholds the volume is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskLevel {
    /// Room to spare.
    Ok,
    /// Below `disk_warn_free_gb`: alert, keep capturing (REL-12).
    Warn,
    /// Below `disk_critical_free_gb`: no new frame may be started.
    Critical,
}

/// What the watchdog polls and what `begin_frame` consults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct DiskStatus {
    /// Free space available to this process, in GB (2^30 bytes).
    pub free_gb: f64,
    /// The level that free space maps onto.
    pub level: DiskLevel,
}

impl DiskThresholds {
    /// Classify a free-space figure.
    #[must_use]
    pub fn classify(self, free_gb: f64) -> DiskStatus {
        let level = if free_gb < self.critical_gb {
            DiskLevel::Critical
        } else if free_gb < self.warn_gb {
            DiskLevel::Warn
        } else {
            DiskLevel::Ok
        };
        DiskStatus { free_gb, level }
    }
}

/// Free space on the filesystem holding `path`, in GB, or `None` if it cannot be determined.
///
/// `None` rather than `0.0` on failure, exactly as `astroctl-stack::vitals` argues it: zero free
/// space is an emergency the node must react to, and an unreadable mount must never be reported as
/// one. The callers here treat `None` as "keep going" — refusing to capture because `statvfs`
/// failed would lose frames to a diagnostic, and losing frames is what REL-05 forbids.
///
/// In `spawn_blocking` because `statvfs` is a filesystem round trip: on a healthy local volume it
/// is microseconds, and on the SD card that has just started failing — the case where this is
/// called every second by the watchdog — it is exactly the syscall that hangs.
pub(crate) async fn free_gb(path: &Path) -> Option<f64> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || statvfs_free_gb(&path))
        .await
        .ok()
        .flatten()
}

fn statvfs_free_gb(path: &Path) -> Option<f64> {
    // `statvfs` needs an existing path; the session directory may not exist yet on a fresh node, so
    // walk up to the nearest ancestor that does. That answers the question actually being asked —
    // how much room is there on the volume the frames will land on.
    let mut probe: &Path = path;
    let stat = loop {
        match rustix::fs::statvfs(probe) {
            Ok(stat) => break stat,
            Err(_) => probe = probe.parent()?,
        }
    };

    // `f_bavail` is what a non-root process may actually use, which is the number that decides
    // whether the next frame fits — `f_bfree` includes the reserved blocks it cannot touch.
    let bytes = (stat.f_bavail as f64) * (stat.f_frsize as f64);
    Some(bytes / BYTES_PER_GB)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> DiskThresholds {
        DiskThresholds {
            warn_gb: 20.0,
            critical_gb: 5.0,
        }
    }

    #[test]
    fn the_thresholds_come_from_the_storage_section() {
        let storage: StorageConfig = serde_json::from_value(serde_json::json!({
            "sessions_dir": "/data/astro/sessions",
            "disk_warn_free_gb": 20.0,
            "disk_critical_free_gb": 5.0
        }))
        .unwrap();

        let thresholds = DiskThresholds::from(&storage);

        assert!((thresholds.warn_gb - 20.0).abs() < f64::EPSILON);
        assert!((thresholds.critical_gb - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn each_side_of_a_threshold_classifies_the_way_rel_12_reads() {
        assert_eq!(thresholds().classify(21.0).level, DiskLevel::Ok);
        assert_eq!(thresholds().classify(20.0).level, DiskLevel::Ok);
        assert_eq!(thresholds().classify(19.9).level, DiskLevel::Warn);
        assert_eq!(thresholds().classify(5.0).level, DiskLevel::Warn);
        assert_eq!(thresholds().classify(4.9).level, DiskLevel::Critical);
        assert_eq!(thresholds().classify(0.0).level, DiskLevel::Critical);
    }

    #[tokio::test]
    async fn free_space_is_reported_for_a_real_directory() {
        let free = free_gb(Path::new("."))
            .await
            .expect("cwd is on a filesystem");
        assert!(free.is_finite() && free >= 0.0, "implausible free space");
    }

    /// The fresh-node case: `sessions_dir` does not exist yet, but its volume does.
    #[tokio::test]
    async fn free_space_walks_up_to_an_existing_ancestor() {
        let missing = std::env::temp_dir().join("astroctl-m1t07-absent/sessions/2026");
        assert!(free_gb(&missing).await.is_some());
    }
}
