//! Shared scaffolding for the T-IPC-1 integration tests.
//!
//! Every helper here is deliberately dependency-free: the workspace pins no `tempfile`, and
//! adding one to `[workspace.dependencies]` for a directory that gets created and deleted is a
//! poor trade against fifteen lines.

// Integration test files are separate crates, so each one uses a subset of this module and the
// rest reads as dead code. The alternative is splitting the module per consumer, which puts the
// same helper in two places.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use astroctl_core::bus::{EventBus, Recv};
use astroctl_core::config::WorkersConfig;
use astroctl_core::event::Topic;
use serde_json::{json, Value};

/// A directory removed when it goes out of scope.
pub struct TempDir {
    path: PathBuf,
}

static NEXT: AtomicU32 = AtomicU32::new(0);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        // `cargo test` runs these as threads in one process, so the pid alone is not unique.
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "astroctl-ipc-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create the temporary directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The repository root, from this crate's manifest directory.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repository root resolves")
}

/// `crates/astroctl-ipc/testdata`, where the fixture workers and the golden messages live.
pub fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// A shipped worker, by name — `compute_worker.py` or `astroctl_ipc.py`.
pub fn shipped_worker(name: &str) -> PathBuf {
    repo_root().join("workers").join(name)
}

/// An absolute `python3`, or `None` on a machine without one.
///
/// Absolute rather than relying on PATH resolution because that is what `workers.python_interpreter`
/// is: PRD §8.2 pins the interpreter, and the config loader rejects a relative one.
pub fn python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}

/// Whether `interpreter` can import every named module.
///
/// The preview job needs numpy and Pillow; the supervision machinery does not. Probing rather
/// than assuming is what lets the machinery tests run on a bare interpreter — which is also the
/// state of the M0-T08 stack image, where python3 is installed and `workers/requirements.txt`
/// is not.
pub fn python_can_import(interpreter: &Path, modules: &[&str]) -> bool {
    let program = format!("import {}", modules.join(", "));
    std::process::Command::new(interpreter)
        .arg("-c")
        .arg(program)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Announce a skipped test loudly. A silent skip is indistinguishable from a pass.
pub fn skip(test: &str, why: &str) {
    eprintln!("SKIP {test}: {why}");
}

/// A `workers:` block pointing at `script`, with the timings a test wants.
pub fn workers_config(
    interpreter: &Path,
    script: &Path,
    health_ping_seconds: u64,
    job_timeout_seconds: u64,
) -> WorkersConfig {
    serde_json::from_value(json!({
        "python_interpreter": interpreter,
        "compute_worker": script,
        "ml_worker": script,
        "health_ping_seconds": health_ping_seconds,
        "restart_backoff_seconds": 1,
        "job_timeout_seconds": job_timeout_seconds,
    }))
    .expect("the workers block deserializes")
}

/// Alert codes seen on the bus, collected in the background.
pub struct AlertLog {
    codes: Arc<Mutex<Vec<String>>>,
}

impl AlertLog {
    pub fn attach(bus: &EventBus) -> Self {
        let codes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&codes);
        let mut subscriber = bus.subscribe();
        tokio::spawn(async move {
            loop {
                let event = match subscriber.recv().await {
                    Recv::Event(event) => event,
                    Recv::Lagged { .. } => continue,
                    Recv::Closed => return,
                };
                if event.topic != Topic::Alert {
                    continue;
                }
                if let Some(code) = event.data.get("code").and_then(Value::as_str) {
                    let code = code.to_owned();
                    sink.lock()
                        .expect("the alert log is not poisoned")
                        .push(code);
                }
            }
        });
        Self { codes }
    }

    pub fn codes(&self) -> Vec<String> {
        self.codes
            .lock()
            .expect("the alert log is not poisoned")
            .clone()
    }

    pub fn saw(&self, code: &str) -> bool {
        self.codes().iter().any(|seen| seen == code)
    }
}

/// Poll `check` until it holds, or fail the test.
///
/// The supervisor answers the caller *before* it finishes its own bookkeeping — a job is failed
/// the moment the outcome is known, and the restart counter moves as the next session starts. A
/// bare assertion on a counter straight after `submit` returns is therefore a race, and one that
/// passes on a fast machine and fails in CI.
pub async fn eventually(label: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..240 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("timed out after 6 s waiting for {label}");
}

/// Write a minimal 2-D FITS image: 16-bit big-endian samples with the unsigned BZERO
/// convention every DSLR-to-FITS converter uses, plus a sparse star field so the asinh stretch
/// has both a background and highlights to work on.
pub fn write_fits(path: &Path, width: usize, height: usize) {
    const CARD: usize = 80;
    const BLOCK: usize = 2880;

    let card = |keyword: &str, value: &str| format!("{keyword:<8}= {value:>20}{:<50}", "");
    let mut header = String::new();
    header.push_str(&card("SIMPLE", "T"));
    header.push_str(&card("BITPIX", "16"));
    header.push_str(&card("NAXIS", "2"));
    header.push_str(&card("NAXIS1", &width.to_string()));
    header.push_str(&card("NAXIS2", &height.to_string()));
    header.push_str(&card("BZERO", "32768"));
    header.push_str(&card("BSCALE", "1"));
    header.push_str(&format!("{:<CARD$}", "END"));
    while !header.len().is_multiple_of(BLOCK) {
        header.push(' ');
    }

    let mut bytes = header.into_bytes();
    for y in 0..height {
        for x in 0..width {
            let background =
                1000.0 + 40.0 * f64::from(u32::try_from((x * 7 + y * 13) % 17).unwrap_or(0));
            let star = if x % 23 == 5 && y % 19 == 3 {
                25_000.0
            } else {
                0.0
            };
            let physical = (background + star).min(65_535.0);
            // Stored value is physical - BZERO, which is what a reader must add back.
            let raw = (physical - 32_768.0) as i16;
            bytes.extend_from_slice(&raw.to_be_bytes());
        }
    }
    std::fs::write(path, bytes).expect("write the FITS fixture");
}
