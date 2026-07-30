//! The T-XFER-1 fixture: a process that gets a frame into `uploading` and stops there, so a test
//! can SIGKILL it and reopen the journal on what it left behind.
//!
//! A test cannot kill itself: SIGKILL runs no destructors, flushes no buffers and returns to
//! nobody, which is exactly the property T-XFER-1's third criterion needs and exactly why it has to
//! happen to a *child* process. This binary is that child, and it is the same fixture pattern
//! `astroctl-session/testdata/crash_harness.rs` established for T-DUR-1 — it lives in `testdata/`
//! because it is a fixture, not a deliverable, and neither deployment image copies it.
//!
//! ```text
//! transfer-queue-crash-harness <queue-dir> <frames-root> <frame-count>
//! ```
//!
//! It enqueues `<frame-count>` frames, claims the oldest — which is the write that marks a row
//! `uploading`, and therefore the exact state a field node is in while a body is on the wire —
//! prints one line of JSON on stdout, and parks forever. That line is the synchronization point:
//! once the parent has read it, the process is known to be mid-upload and nowhere else, so the kill
//! lands where the test says it lands rather than wherever the scheduler happened to be.
//!
//! No file is ever written for the frames. The journal references frames rather than copying them
//! (§5.10.1), and nothing on the path under test opens one, so creating 48 MB of nothing would only
//! make the fixture slower and its failure modes less obvious.

use std::io::Write as _;

use astroctl_transfer::journal::{Journal, NewEntry};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, queue_dir, frames_root, count] = args.as_slice() else {
        eprintln!("usage: transfer-queue-crash-harness <queue-dir> <frames-root> <frame-count>");
        std::process::exit(2);
    };
    let count: u32 = count.parse().expect("frame-count is a number");

    let db = std::path::Path::new(queue_dir).join("transfer.db");
    let journal = Journal::open(db.clone()).await.expect("the journal opens");

    for n in 1..=count {
        let frame_id = format!("light_{n:05}");
        journal
            .enqueue(NewEntry {
                session_id: "2026-07-29_ngc7000".to_owned(),
                path: std::path::Path::new(frames_root)
                    .join("2026-07-29_ngc7000")
                    .join("frames")
                    .join(format!("{frame_id}.fits")),
                sha256: format!("{n:064x}"),
                size_bytes: 48_003_840,
                frame_id,
            })
            .await
            .expect("enqueue");
    }

    let claimed = journal
        .claim_next()
        .await
        .expect("claim")
        .expect("something was queued");

    // Printed and flushed before parking: the parent kills on this line, so it has to be on the
    // wire before the process becomes unkillable-at-a-known-point.
    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "{}",
        serde_json::json!({ "claimed": claimed.frame_id, "db": db.display().to_string() })
    )
    .expect("stdout is writable");
    stdout.flush().expect("stdout flushes");

    // Park. The parent's SIGKILL is what ends this process.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
