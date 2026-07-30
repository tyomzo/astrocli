//! Test-only fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A directory that removes itself when the test ends.
///
/// The same shape as `astroctl-stack`'s. A shared helper crate for six lines would be a dependency
/// edge in ADD §5.6's matrix carrying a `mkdir`.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Create a fresh directory under the system temporary directory.
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astroctl-transfer-test-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
