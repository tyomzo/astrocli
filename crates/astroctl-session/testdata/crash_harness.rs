//! The T-DUR-1 fixture: a process that reaches a chosen point in the frame store and stops there,
//! so a test can SIGKILL it and reopen the store on what it left behind.
//!
//! A test cannot kill itself: SIGKILL runs no destructors, flushes no buffers and returns to
//! nobody, which is exactly the property T-DUR-1 needs and exactly why it has to happen to a *child*
//! process. This binary is that child. It is the Rust counterpart of
//! `astroctl-ipc/testdata/worker_stub.py`, and it lives in `testdata/` for the same reason: it is a
//! fixture, not a deliverable.
//!
//! ```text
//! frame-store-crash-harness <sessions-root> <phase> <frame-bytes>
//! ```
//!
//! Phases, each named for the window it parks in:
//!
//! | phase          | reached state                                            |
//! |----------------|----------------------------------------------------------|
//! | `after-begin`  | id reserved, temporary written — **not** committed        |
//! | `after-commit` | frame committed and durable — sidecar **not** written      |
//!
//! On arrival it prints one line of JSON on stdout and then parks forever. The line is the
//! synchronization point: when the parent has read it, the process is known to be in the window and
//! nowhere else, so the kill lands where the test says it lands rather than wherever the scheduler
//! happened to be.

use std::io::Write as _;

use astroctl_session::{FrameKind, FrameStore, NewSession};

/// The session every phase works in. Fixed so the parent can predict the directory name.
const SLUG: &str = "tdur1";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, root, phase, content] = args.as_slice() else {
        eprintln!("usage: frame-store-crash-harness <sessions-root> <phase> <frame-bytes>");
        std::process::exit(2);
    };

    // Thresholds of zero, not the operator's: this fixture is about crash windows, and a CI runner
    // that happens to have 4 GB free must not turn a durability test into a REL-12 refusal.
    let storage = serde_json::from_value(serde_json::json!({
        "sessions_dir": root,
        "disk_warn_free_gb": 0.0,
        "disk_critical_free_gb": 0.0,
    }))
    .expect("the storage block deserializes");

    let store = FrameStore::open(&storage).await.expect("the store opens");
    let session = match store.open_current().await.expect("CURRENT is readable") {
        Some(session) => session,
        None => store
            .open_or_create_session(NewSession {
                slug: SLUG.to_owned(),
                target: None,
                equipment: astroctl_session::Equipment {
                    telescope: "harness".to_owned(),
                    camera: "harness".to_owned(),
                    filter: "none".to_owned(),
                },
            })
            .await
            .expect("the session is created"),
    };

    let frame_id = session
        .reserve_frame_id(FrameKind::Light)
        .await
        .expect("an id is reserved");
    let mut staged = session
        .begin_frame(frame_id, "cr3")
        .await
        .expect("the capture begins");
    staged
        .write_all(content.as_bytes())
        .await
        .expect("the frame is written");

    let mut report = serde_json::json!({
        "session_dir": session.dir(),
        "session_id": session.id(),
        "frame_id": frame_id.to_string(),
        "tmp": staged.path(),
        "dest": staged.destination(),
    });

    match phase.as_str() {
        // Killed here, the store has: a reserved id, a temporary holding a partial frame, and no
        // frame under its final name.
        "after-begin" => {}
        // Killed here, the store has: a committed, fsynced frame and no sidecar for it.
        "after-commit" => {
            let saved = session
                .commit_frame(staged)
                .await
                .expect("the frame commits");
            report["sha256"] = saved.sha256.clone().into();
            report["size_bytes"] = saved.size_bytes.into();
        }
        other => {
            eprintln!("unknown phase {other}");
            std::process::exit(2);
        }
    }

    println!("{report}");
    std::io::stdout().flush().expect("stdout flushes");

    // Park. The parent kills the process from here; nothing below this line ever runs, which is the
    // point — no destructor, no flush, no tidy-up the real crash would not have done either.
    std::future::pending::<()>().await;
}
