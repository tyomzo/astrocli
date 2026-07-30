//! **T-XFER-1, third criterion** — SIGKILL the field node mid-upload; the row returns to `queued`,
//! the frame is re-uploaded, the journal is intact.
//!
//! The acceptance criterion is a *process* criterion, so this is the only test in the crate that
//! drives a real child process: `testdata/crash_harness.rs`, built as
//! `transfer-queue-crash-harness` and located through `CARGO_BIN_EXE_*`. The pattern is
//! `astroctl-session`'s T-DUR-1, for the same reason it gives — a durability guarantee tested by
//! dropping a struct is a guarantee about `Drop`, not about a crash.
//!
//! What makes the kill honest:
//!
//! * **It is SIGKILL.** `std::process::Child::kill` sends signal 9 on Unix; no handler runs, no
//!   destructor runs, no WAL checkpoint is taken. The test asserts the child died of exactly that
//!   signal, so a harness that merely exited early cannot pass it.
//! * **It lands in the window.** The child announces that it has claimed a frame — which is the
//!   write that marks the row `uploading` — and then parks. The kill happens after that line is
//!   read, so "mid-upload" is a fact about the process state rather than a hope about timing.
//! * **The journal is reopened from disk afterwards**, in this process, exactly as a restarted
//!   field node would — including the recovery sweep of §5.10.3.

use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use astroctl_transfer::journal::{Journal, State};

/// How long the child gets to reach its window. Generous: a debug-build binary creating a SQLite
/// database on a loaded CI runner is still far inside this, and the only thing a tight bound would
/// buy is a flaky failure that looks like a durability bug.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astroctl-xfer1-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("queue")).expect("mkdir");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the harness until it has a frame in `uploading`, SIGKILL it there, and return which frame.
fn kill_mid_upload(scratch: &Scratch, frames: u32) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_transfer-queue-crash-harness"))
        .arg(scratch.0.join("queue"))
        .arg(scratch.0.join("sessions"))
        .arg(frames.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the crash harness is built and runnable");

    // Read the ready line on another thread so a child that never reaches its window fails this
    // test on the timeout instead of hanging the suite.
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(READY_TIMEOUT)
        .expect("the harness reached its window");
    let report: serde_json::Value =
        serde_json::from_str(line.trim()).expect("the harness reports JSON");

    // SIGKILL. Uncatchable by construction: whatever the child was about to do next, it does not.
    child.kill().expect("the harness is still running");
    let status = child.wait().expect("the harness is reaped");
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(
        status.signal(),
        Some(9),
        "the harness must die of SIGKILL, not exit on its own"
    );

    report["claimed"].as_str().expect("a frame id").to_owned()
}

#[tokio::test]
async fn a_field_node_killed_mid_upload_leaves_an_intact_journal_and_resumes() {
    let scratch = Scratch::new();
    let killed = kill_mid_upload(&scratch, 3);
    assert_eq!(killed, "light_00001", "the oldest frame is claimed first");

    let db = scratch.0.join("queue").join("transfer.db");

    // Reopened from disk, exactly as a restarted node does. `Journal::open` succeeding at all is
    // most of the claim: a corrupt database or a half-applied schema fails here.
    let journal = Journal::open(db.clone())
        .await
        .expect("the journal reopens");

    // SQLite's own verdict, not ours. WAL is what makes this survivable — the process died with
    // uncheckpointed commits in `transfer.db-wal`, and the reopen replays them.
    let integrity: String = {
        let conn = rusqlite::Connection::open(&db).expect("open for integrity check");
        conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("integrity_check runs")
    };
    assert_eq!(integrity, "ok", "the journal survived SIGKILL");

    // Nothing was lost: all three enqueues are still there, and the claim is still recorded.
    let entries = journal.entries().await.expect("entries");
    assert_eq!(entries.len(), 3, "{entries:#?}");
    assert_eq!(
        entries[0].state,
        State::Uploading,
        "the row is exactly as the crash left it, not rolled back"
    );
    assert!(entries[1..].iter().all(|e| e.state == State::Queued));

    // §5.10.3's recovery sweep.
    assert_eq!(journal.recover_interrupted().await.expect("recover"), 1);

    let entries = journal.entries().await.expect("entries");
    assert_eq!(entries[0].frame_id, killed);
    assert_eq!(entries[0].state, State::Queued, "returned to the queue");
    assert_eq!(
        entries[0].attempts, 1,
        "the attempt is counted, which is how the uploader knows the frame may already be on the \
         far side and is worth a pre-flight (§5.11.1)"
    );
    assert!(
        entries[0].last_error.is_some(),
        "a resumed row says why it was resumed"
    );

    // …and it is still the head of the queue: a restart must not reorder the night.
    let claimed = journal.claim_next().await.expect("claim").expect("a row");
    assert_eq!(claimed.frame_id, killed);
}

/// Running the same harness twice against one queue directory is what a restart loop looks like.
/// The enqueues are idempotent, so the queue must not grow, and the second claim must land on the
/// same frame rather than skipping past it.
#[tokio::test]
async fn a_restart_loop_neither_duplicates_frames_nor_skips_one() {
    let scratch = Scratch::new();
    assert_eq!(kill_mid_upload(&scratch, 3), "light_00001");
    assert_eq!(
        kill_mid_upload(&scratch, 3),
        "light_00002",
        "the second run claims the next frame — the first is still `uploading`"
    );

    let journal = Journal::open(scratch.0.join("queue").join("transfer.db"))
        .await
        .expect("the journal reopens");
    assert_eq!(
        journal.entries().await.expect("entries").len(),
        3,
        "three enqueues, twice, is still three frames"
    );
    assert_eq!(
        journal.recover_interrupted().await.expect("recover"),
        2,
        "both interrupted uploads come back"
    );
    let entries = journal.entries().await.expect("entries");
    assert!(entries.iter().all(|e| e.state == State::Queued));
}
