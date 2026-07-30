//! **T-DUR-1** — SIGKILL between begin and commit, and between commit and metadata.
//!
//! The acceptance criterion is a *process* criterion, so these tests are the only ones in the crate
//! that drive a real child process: `testdata/crash_harness.rs`, built as
//! `frame-store-crash-harness` and located through `CARGO_BIN_EXE_*`. The pattern is
//! `astroctl-ipc`'s, which drives real workers for the same reason — a durability guarantee tested
//! by dropping a struct is a guarantee about `Drop`, not about a crash.
//!
//! What makes the kill honest:
//!
//! * **It is SIGKILL.** `std::process::Child::kill` sends signal 9 on Unix; no handler runs, no
//!   destructor runs, no buffered write is flushed. The tests assert the child died of exactly that
//!   signal, so a harness that merely exited early cannot pass them.
//! * **It lands in the window.** The child announces on stdout at the point it has reached and then
//!   parks forever. The kill happens after that line is read, so "between begin and commit" is a
//!   fact about the process state rather than a hope about timing.
//! * **The store is reopened from disk afterwards**, in this process, exactly as a restarted field
//!   node would — including the startup sweep.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use astroctl_session::{FrameKind, FrameStore, SessionManifest, StoreError};
use serde_json::Value;

/// How long the child gets to reach its window. Generous: a debug-build binary opening a store on a
/// loaded CI runner is still far inside this, and the only thing a tight bound would buy is a flaky
/// failure that looks like a durability bug.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The bytes every scenario captures, chosen so a test can compare the stored frame byte for byte.
const FRAME_BYTES: &str = "T-DUR-1 raw frame bytes";

/// A directory removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astroctl-tdur1-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("the system temporary directory is writable");
        Self(path)
    }

    fn sessions(&self) -> PathBuf {
        self.0.join("sessions")
    }

    /// The `storage:` block a restarted field node would load (PRD §8.1 keys).
    ///
    /// Thresholds of zero for the same reason the harness uses them: a CI runner with little free
    /// space must not turn a durability test into a REL-12 refusal.
    fn storage(&self) -> astroctl_core::config::StorageConfig {
        serde_json::from_value(serde_json::json!({
            "sessions_dir": self.sessions(),
            "disk_warn_free_gb": 0.0,
            "disk_critical_free_gb": 0.0,
        }))
        .expect("the storage block deserializes")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the harness to `phase`, SIGKILL it there, and return what it reported.
fn kill_at(sessions: &Path, phase: &str) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_frame-store-crash-harness"))
        .arg(sessions)
        .arg(phase)
        .arg(FRAME_BYTES)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the crash harness is built and runnable");

    let report = read_ready_line(&mut child, phase);

    // SIGKILL. Uncatchable by construction: whatever the child was about to do next, it does not.
    child.kill().expect("the harness is still running");
    let status = child.wait().expect("the harness is reaped");

    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(
        status.signal(),
        Some(9),
        "the harness did not die of SIGKILL ({status:?}); it must not be exiting on its own"
    );

    serde_json::from_str(&report).expect("the harness reports JSON")
}

/// Read the child's one-line report, bounded, so a harness that never reaches its window fails the
/// test instead of hanging it.
fn read_ready_line(child: &mut Child, phase: &str) -> String {
    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(read.ok().filter(|n| *n > 0).map(|_| line));
    });

    match rx.recv_timeout(READY_TIMEOUT) {
        Ok(Some(line)) => line,
        Ok(None) => {
            let _ = child.kill();
            panic!("the harness closed stdout without reaching {phase}");
        }
        Err(_) => {
            let _ = child.kill();
            panic!("the harness did not reach {phase} within {READY_TIMEOUT:?}");
        }
    }
}

/// Reopen the store the way a restarted field node does, and hand back its session.
async fn restart(dir: &TempDir) -> std::sync::Arc<astroctl_session::Session> {
    let store = FrameStore::open(&dir.storage())
        .await
        .expect("the store reopens after the crash");
    store
        .open_current()
        .await
        .expect("CURRENT is readable")
        .expect("CURRENT names the crashed session")
}

/// `session.json` parses — the third of T-DUR-1's three assertions, made from the bytes on disk
/// rather than from the store's in-memory copy.
fn parse_manifest(session_dir: &Path) -> SessionManifest {
    let bytes = std::fs::read(session_dir.join(astroctl_session::SESSION_JSON))
        .expect("session.json exists after the crash");
    serde_json::from_slice(&bytes).expect("session.json parses after the crash")
}

fn entries(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Killed between `begin_frame` and `commit_frame`.
///
/// Nothing may be visible as a frame, and the id that was reserved may never come back.
#[tokio::test]
async fn a_kill_between_begin_and_commit_leaves_no_frame_and_burns_the_id() {
    let dir = TempDir::new("begin");
    let report = kill_at(&dir.sessions(), "after-begin");

    let session_dir = PathBuf::from(report["session_dir"].as_str().unwrap());
    let frames = session_dir.join(astroctl_session::FRAMES_DIR);
    assert_eq!(report["frame_id"], "light_00001");

    // Before the restart: the process really was in the window — a partial capture on disk under a
    // temporary name, and no frame under its final one.
    assert!(
        !frames.join("light_00001.cr3").exists(),
        "an uncommitted frame is visible under its final name"
    );
    let leftovers = entries(&frames);
    assert_eq!(
        leftovers.len(),
        1,
        "expected one temporary, got {leftovers:?}"
    );
    assert!(
        leftovers[0].starts_with(".tmp_light_00001."),
        "unexpected leftover {leftovers:?}"
    );

    // The counter was persisted before the id was granted (REL-04), so it survived the kill.
    assert_eq!(parse_manifest(&session_dir).frames_reserved, 1);

    // The restart: sweep, then the invariants.
    let session = restart(&dir).await;
    assert_eq!(
        entries(&frames),
        Vec::<String>::new(),
        "the startup sweep left the interrupted capture behind"
    );
    let view = session.view().await.expect("the session lists");
    assert_eq!(view.frame_count, 0, "a partial frame is visible");
    assert_eq!(view.frames_reserved, 1);

    let next = session
        .reserve_frame_id(FrameKind::Light)
        .await
        .expect("the counter still grants");
    assert_eq!(
        next.sequence(),
        2,
        "the id reserved before the crash was granted again"
    );
}

/// Killed between `commit_frame` and `write_quality`.
///
/// The frame is durable — that is what REL-05 promises and what the commit ordering delivers — and
/// only its sidecar is missing. The id is still spent.
#[tokio::test]
async fn a_kill_between_commit_and_metadata_keeps_the_frame_and_loses_only_the_sidecar() {
    let dir = TempDir::new("commit");
    let report = kill_at(&dir.sessions(), "after-commit");

    let session_dir = PathBuf::from(report["session_dir"].as_str().unwrap());
    let frame = session_dir.join("frames/light_00001.cr3");
    let sidecar = session_dir.join("control/quality_00001.json");

    // Before the restart: the frame is whole, the sidecar was never written.
    assert_eq!(
        std::fs::read(&frame).expect("the committed frame survived the kill"),
        FRAME_BYTES.as_bytes()
    );
    assert!(
        !sidecar.exists(),
        "the sidecar cannot have been written yet"
    );
    assert_eq!(entries(&session_dir.join("control")), Vec::<String>::new());
    assert_eq!(parse_manifest(&session_dir).frames_reserved, 1);

    // The restart.
    let session = restart(&dir).await;
    let view = session.view().await.expect("the session lists");
    assert_eq!(view.frame_count, 1);
    assert_eq!(view.frames[0].frame_id.to_string(), "light_00001");
    assert_eq!(view.frames[0].size_bytes, FRAME_BYTES.len() as u64);
    assert!(
        view.frames[0].quality.is_none(),
        "a frame whose metadata write was interrupted is still a frame, listed without it"
    );

    assert_eq!(
        session
            .reserve_frame_id(FrameKind::Light)
            .await
            .unwrap()
            .sequence(),
        2
    );
    // Untouched by the restart, the sweep, and the listing (REL-11).
    assert_eq!(std::fs::read(&frame).unwrap(), FRAME_BYTES.as_bytes());
}

/// Two crashes in a row, in the same session — the state a bad night actually produces.
///
/// The point is that the guarantee composes: an id burned by the first crash is not recycled by the
/// second, so the frame the second run commits cannot land on the first run's file.
#[tokio::test]
async fn ids_are_never_reused_across_successive_crashes() {
    let dir = TempDir::new("twice");

    let first = kill_at(&dir.sessions(), "after-begin");
    let second = kill_at(&dir.sessions(), "after-commit");

    assert_eq!(first["frame_id"], "light_00001");
    assert_eq!(
        second["frame_id"], "light_00002",
        "the second run re-granted the id the first run burned"
    );
    assert_eq!(first["session_dir"], second["session_dir"]);

    let session = restart(&dir).await;
    let view = session.view().await.unwrap();
    assert_eq!(view.frames_reserved, 2);
    assert_eq!(view.frame_count, 1);
    assert_eq!(view.frames[0].frame_id.to_string(), "light_00002");
    assert_eq!(
        std::fs::read(session.dir().join("frames/light_00002.cr3")).unwrap(),
        FRAME_BYTES.as_bytes()
    );
}

/// The store the crashed process left is not merely readable — it is usable, and the frame it
/// commits next is a normal frame with a normal sidecar.
#[tokio::test]
async fn capture_resumes_normally_after_a_crash() {
    let dir = TempDir::new("resume");
    kill_at(&dir.sessions(), "after-begin");

    let session = restart(&dir).await;
    let id = session.reserve_frame_id(FrameKind::Light).await.unwrap();
    let mut staged = session.begin_frame(id, "cr3").await.unwrap();
    staged.write_all(b"a frame after the crash").await.unwrap();
    let saved = session.commit_frame(staged).await.unwrap();
    session
        .write_quality(
            &saved,
            &astroctl_session::CaptureParams {
                started_ts: astroctl_core::event::now_millis(),
                exposure_s: 30.0,
                settings: astroctl_core::types::CameraSettings {
                    iso: "800".to_owned(),
                    shutter: "30".to_owned(),
                    aperture: None,
                    format: astroctl_core::types::ImageFormat::Raw,
                },
            },
        )
        .await
        .unwrap();

    let view = session.view().await.unwrap();
    assert_eq!(view.frame_count, 1);
    assert_eq!(view.frames[0].frame_id.to_string(), "light_00002");
    assert_eq!(
        view.frames[0].quality.as_ref().unwrap().sha256,
        saved.sha256
    );
}

/// A sanity check on the error surface T-DUR-1 does not cover: the store's failures carry the wire
/// code the API layer will answer with (SDD §4.2), so a crash-adjacent conflict is not a 500.
#[test]
fn a_conflicting_frame_id_is_a_conflict_rather_than_an_internal_error() {
    let error = StoreError::FrameIdConflict {
        frame_id: astroctl_session::FrameId::new(FrameKind::Light, 1),
        stored_sha256: "abcd".to_owned(),
    };
    assert_eq!(error.code().http_status(), 409);
}
