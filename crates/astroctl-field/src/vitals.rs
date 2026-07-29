//! Node vitals for `/api/system/health` and the `system.health` event (SDD §4.3, REL-12/14).

use std::path::Path;
use std::time::Instant;

/// Bytes in one gigabyte, as this system counts them.
///
/// **2^30, not 10^9.** Neither PRD §8.1 nor SDD §4.3 defines the unit behind
/// `disk_warn_free_gb` / `disk_critical_free_gb`, and the two conventions differ by 7%. The
/// binary one is chosen because these are *safety* thresholds: it reports the smaller number for
/// the same free space, so the warning fires slightly early rather than slightly late, and it
/// matches what `df -h` shows the operator when they go and check.
const BYTES_PER_GB: f64 = (1_u64 << 30) as f64;

/// The marker systemd-timesyncd creates once the clock has been disciplined.
///
/// This is the M0 answer to `clock_synced` and it is a real check on the deployment target
/// (Raspberry Pi OS runs systemd-timesyncd), not a placeholder — but it is only *one* time
/// source. A node running chrony or ntpd reports `false` here despite having a good clock.
/// REL-14 wants a real clock-discipline check (offset and dispersion, not a boolean); that
/// arrives with the watchdog work it belongs to.
const TIMESYNC_MARKER: &str = "/run/systemd/timesync/synchronized";

/// Free space on the filesystem holding `path`, in GB, or `None` if it cannot be determined.
///
/// `None` rather than `0.0` on failure: zero free space is an emergency the node must react to,
/// and a missing directory or an unreadable mount must never be reported as one.
#[must_use]
pub fn disk_free_gb(path: &Path) -> Option<f64> {
    // `statvfs` needs an existing path; the session directory may not exist yet on a fresh node,
    // so walk up to the nearest ancestor that does. That answers the question actually being
    // asked — how much room is there on the volume the frames will land on.
    let mut probe = path;
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

/// Whether the system clock is disciplined (REL-14). See [`TIMESYNC_MARKER`] for the caveat.
#[must_use]
pub fn clock_synced() -> bool {
    Path::new(TIMESYNC_MARKER).exists()
}

/// Process uptime, seconds — the `uptime_s` field of `system.health` (SDD §4.3).
#[derive(Clone, Copy, Debug)]
pub struct Uptime(Instant);

impl Uptime {
    /// Start the clock. Called once, at the top of `main`.
    #[must_use]
    pub fn started_now() -> Self {
        Self(Instant::now())
    }

    /// Whole seconds since the process started.
    #[must_use]
    pub fn seconds(&self) -> u64 {
        self.0.elapsed().as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_free_is_reported_for_a_real_directory() {
        let free = disk_free_gb(Path::new(".")).expect("the working directory is on a filesystem");
        assert!(
            free.is_finite() && free >= 0.0,
            "implausible free space {free}"
        );
    }

    /// The fresh-node case: the sessions directory does not exist yet, but its volume does.
    #[test]
    fn disk_free_walks_up_to_an_existing_ancestor() {
        let missing = std::env::temp_dir().join("astroctl-m0t05-does-not-exist/sessions/2026");
        let free = disk_free_gb(&missing).expect("falls back to an existing ancestor");
        assert!(free.is_finite() && free >= 0.0);
    }

    /// An absolute path always terminates at `/`, so the only way to get `None` is a relative
    /// path whose ancestors run out. Worth pinning: it is what guarantees a configured
    /// `sessions_dir` never silently reports 0 GB free.
    #[test]
    fn disk_free_is_none_only_when_the_ancestors_run_out() {
        assert!(disk_free_gb(Path::new("/nonexistent-root-astroctl/x")).is_some());
        assert!(disk_free_gb(Path::new("astroctl-m0t05-relative/x")).is_none());
    }

    #[test]
    fn uptime_starts_at_zero_and_does_not_go_backwards() {
        let uptime = Uptime::started_now();
        assert_eq!(uptime.seconds(), 0);
        assert_eq!(uptime.seconds(), 0);
    }

    /// Not an assertion about this machine's clock — just that the check is a pure function of
    /// the marker file and answers without panicking either way.
    #[test]
    fn clock_synced_matches_the_marker_file() {
        assert_eq!(clock_synced(), Path::new(TIMESYNC_MARKER).exists());
    }
}
