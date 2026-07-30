//! The session orchestrator FSM, the sequence model, the frame store and its persistence.
//!
//! M1-T07 delivers the frame store: session directories, write-once frames, crash-safe metadata and
//! frame-ID reservation (SDD §5.5, §6; PRD §5.9, REL-04/05/11/12). The orchestrator FSM of §5.6 is
//! M1-T08's.
//!
//! Everything downstream trusts this layer absolutely — the transfer agent treats `frame.saved` as
//! proof of durability, and the stack node's retention policy eventually deletes the field node's
//! only copy on the strength of it — so the guarantees are stated plainly:
//!
//! 1. **A frame id is never granted twice.** The counter is persisted before
//!    [`Session::reserve_frame_id`] returns, so a crash burns an id rather than reusing one
//!    (REL-04). A gap in the numbering is normal and is what a crash looks like from outside.
//! 2. **A committed frame is never modified.** No API here can rewrite or delete one; the frame is
//!    linked into place with `renameat2(RENAME_NOREPLACE)`, which makes overwriting it impossible
//!    rather than merely disallowed (REL-11).
//! 3. **A frame is durable before anything else happens.** fsync, then rename, then fsync the
//!    directory — and only then the sidecar, the event, the queue (REL-05, ADD §9.2).
//! 4. **A capture does not start on a full disk.** [`Session::begin_frame`] refuses below
//!    `storage.disk_critical_free_gb` with [`StoreError::DiskFull`], because a capture that starts
//!    below it finishes as a truncated raw (REL-12).
//!
//! # Layout
//!
//! ```text
//! sessions/CURRENT -> 2026-07-30_ngc7000        # relative symlink to the active session
//! sessions/2026-07-30_ngc7000/session.json      # v1: id, created, equipment, counter, sequence_state
//!                            frames/light_00042.cr3
//!                            control/quality_00042.json
//! ```
//!
//! The paths are fixed by `testdata/session-layout.txt`, which this crate and `astroctl-stack`
//! assert against from opposite sides — SDD §5.11.3 requires the mirrored archive to be "byte-for-
//! byte the same structure", and a fixture only one crate checks is a fixture that cannot catch
//! drift.
//!
//! # Using it (the shape M1-T08 wires into the field binary)
//!
//! ```no_run
//! # use astroctl_session::{CaptureParams, Equipment, FrameKind, FrameStore, NewSession};
//! # async fn example(storage: &astroctl_core::config::StorageConfig,
//! #                  equipment: &astroctl_core::config::EquipmentConfig,
//! #                  capture: CaptureParams) -> Result<(), Box<dyn std::error::Error>> {
//! let store = FrameStore::open(storage).await?;
//!
//! // The restart path first: a node coming back mid-night continues the session it was in.
//! let session = match store.open_current().await? {
//!     Some(session) => session,
//!     None => {
//!         store
//!             .open_or_create_session(NewSession {
//!                 slug: "ngc7000".to_owned(),
//!                 target: None,
//!                 equipment: Equipment::from(equipment),
//!             })
//!             .await?
//!     }
//! };
//!
//! let id = session.reserve_frame_id(FrameKind::Light).await?;   // persisted before it returns
//! let staged = session.begin_frame(id, "cr3").await?;           // refuses on a full disk
//! // ... the camera driver writes into `staged.path()` ...
//! let saved = session.commit_frame(staged).await?;              // durable here; publish frame.saved
//! session.write_quality(&saved, &capture).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ADD §5.6 is authoritative for the crate layout and the allowed-dependency matrix;
//! `scripts/check-deps.sh` enforces it.

mod durable;

pub mod disk;
pub mod frame;
pub mod manifest;
pub mod store;

#[cfg(test)]
mod test_support;

pub use disk::{DiskLevel, DiskStatus, DiskThresholds};
pub use frame::{FrameId, FrameKind};
pub use manifest::{CaptureParams, Equipment, Quality, SessionManifest, SCHEMA_VERSION};
pub use store::{
    FrameEntry, FrameStore, NewSession, SavedFrame, Session, SessionView, StagedFrame, StoreError,
    CONTROL_DIR, CURRENT_LINK, FRAMES_DIR, PREVIEW_DIR, SESSION_JSON,
};
