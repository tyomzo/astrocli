//! Fixtures shared by this crate's unit tests.
//!
//! Hand-rolled rather than `tempfile`: a dev-dependency has to be declared in the root manifest,
//! which is the one file every parallel task touches, and this is thirty lines. The same trade
//! `astroctl-stack` and `astroctl-ipc` already made.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use astroctl_core::config::StorageConfig;

/// A directory that deletes itself when the test that made it ends.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Create a fresh directory under the system temporary directory.
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // `cargo test` runs these as threads in one process, so the pid alone is not unique.
        let path = std::env::temp_dir().join(format!(
            "astroctl-session-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("the system temporary directory is writable");
        Self(path)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A `storage:` block (PRD §8.1) pointing at this directory, with the example thresholds.
    pub fn storage(&self) -> StorageConfig {
        self.storage_with_thresholds(20.0, 5.0)
    }

    /// A `storage:` block with the REL-12 thresholds pinned.
    ///
    /// Built by deserializing rather than by struct literal because [`StorageConfig`] is a config
    /// schema — deserialize-only by design — and going through serde is also what proves the key
    /// names in these tests are the operator's real ones.
    pub fn storage_with_thresholds(&self, warn_gb: f64, critical_gb: f64) -> StorageConfig {
        serde_json::from_value(serde_json::json!({
            "sessions_dir": self.0.join("sessions"),
            "disk_warn_free_gb": warn_gb,
            "disk_critical_free_gb": critical_gb,
        }))
        .expect("the storage block deserializes")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The equipment profile the PRD §8.1 example configures.
pub fn equipment() -> crate::Equipment {
    crate::Equipment {
        telescope: "SW 200PDS f/5".to_owned(),
        camera: "Canon R10".to_owned(),
        filter: "none".to_owned(),
    }
}

/// A capture the store can write a sidecar for.
pub fn capture_params() -> crate::CaptureParams {
    crate::CaptureParams {
        started_ts: astroctl_core::event::now_millis(),
        exposure_s: 120.0,
        settings: astroctl_core::types::CameraSettings {
            iso: "1600".to_owned(),
            shutter: "bulb".to_owned(),
            aperture: Some("5.6".to_owned()),
            format: astroctl_core::types::ImageFormat::Raw,
        },
    }
}
