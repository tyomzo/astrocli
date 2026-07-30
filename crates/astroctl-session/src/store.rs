//! The frame store: sessions, write-once frames, and the ID reservation that survives a crash.
//!
//! # The order, and why it is that order
//!
//! ```text
//! reserve_frame_id  → session.json (fsync) → the id is returned    ← REL-04: persist, then grant
//!
//! begin_frame(id)   → frames/.tmp_<id>.<pid>.<nonce>               ← belongs to nobody yet
//!   camera writes into it
//! commit_frame      → fsync the file
//!                   → renameat2(RENAME_NOREPLACE) → frames/<id>.<ext>
//!                   → fsync the frames directory                   ← the name is now durable
//!                                                                    REL-05 is satisfied here
//! write_quality     → control/quality_<id>.json (tmp-fsync-rename)
//! ```
//!
//! The split between `commit_frame` and `write_quality` is deliberate and is what ADD §9.2's
//! write-ahead ordering means in practice: **the frame is durable before anything else happens**.
//! A process killed between the two leaves a frame with no sidecar — recoverable, visible, and
//! reported by [`Session::view`] as metadata-pending. The other order would leave metadata
//! describing a frame that does not exist, and downstream (the transfer agent, the stack node)
//! would act on it.
//!
//! # What this store will not do
//!
//! There is no API to modify or delete a committed frame (REL-11). Removal is the operator's, or
//! the verified-transfer retention policy's (REL-13), and neither of those is this layer. The one
//! path that touches a committed frame at all is a *retry* of `commit_frame` for the same id, and
//! it reads the stored bytes to decide whether the retry is a duplicate or a conflict — it never
//! writes them.
//!
//! # Divergences from `astroctl-stack::mirror`, which implements the same disciplines
//!
//! The mirror discards a temporary whenever anything goes wrong, because the field node still holds
//! the frame and will re-send it. **This node is the origin.** Nothing can re-send a raw here, so a
//! failed commit leaves its temporary on disk for the operator and lets the next startup sweep
//! reclaim it. For the same reason a sidecar write failure is returned to the caller rather than
//! logged: the mirror's sidecar is derived from metadata the sender still has, and this one holds
//! exposure parameters that exist nowhere else.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use astroctl_core::config::StorageConfig;
use astroctl_core::error::ErrorCode;
use astroctl_core::event::{now_millis, ts_rfc3339_millis};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::io::AsyncWriteExt as _;

use crate::disk::{self, DiskLevel, DiskStatus, DiskThresholds};
use crate::durable::{self, Linked};
use crate::frame::{FrameId, FrameKind};
use crate::manifest::{CaptureParams, Equipment, Quality, SessionManifest, SCHEMA_VERSION};

/// Where the raw frames live, under `sessions/<session_id>/` (SDD §5.5).
pub const FRAMES_DIR: &str = "frames";

/// Where the per-frame control metadata lives (SDD §5.5).
pub const CONTROL_DIR: &str = "control";

/// Where derived, regenerable artifacts live — the live-view pipeline's cached previews
/// (SDD §5.7, §6; M1-T09).
///
/// A third directory rather than a corner of `control/`, because the two hold different *kinds*
/// of thing and §6's data table already separates them. `control/` is metadata: a sidecar carries
/// exposure parameters that exist nowhere else, which is why SDD §5.5 note 6 makes a failed
/// sidecar write a returned error. A preview is a JPEG re-derived from the frame in a second —
/// §6 calls it "ephemeral, regenerable" — so losing one costs a redecode and losing a sidecar
/// costs information. Mixing them would mean a future retention pass could not tell "delete
/// freely" from "never delete" by looking at the path.
///
/// Deliberately **not** added to `testdata/session-layout.txt`. That fixture is the layout
/// `astroctl-stack` mirrors (SDD §5.11.3), and a regenerable cache is not part of the archive:
/// putting it there would oblige the stack node to reproduce a directory it has no reason to
/// carry, and it renders its own previews anyway.
pub const PREVIEW_DIR: &str = "preview";

/// The per-session manifest (SDD §5.5).
pub const SESSION_JSON: &str = "session.json";

/// The symlink under `sessions/` naming the active session (SDD §5.5).
pub const CURRENT_LINK: &str = "CURRENT";

/// Longest session slug accepted.
///
/// The session id becomes a directory name on every node the frames reach, and the id also travels
/// in `frame.saved` and the ingest URL. 64 leaves room for a date prefix inside any filesystem's
/// 255-byte component limit without the operator ever meeting that limit as a mysterious failure.
const MAX_SLUG: usize = 64;

/// Anything that stops the store from doing what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The filesystem refused.
    #[error("cannot {action} {path}: {source}")]
    Io {
        /// What was being attempted.
        action: &'static str,
        /// The path it was attempted on.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// A persisted document could not be built or parsed.
    #[error("the JSON at {path} is not what this version writes: {source}")]
    Json {
        /// The document's path.
        path: PathBuf,
        /// The parse or serialization failure.
        source: serde_json::Error,
    },
    /// Free space is below `storage.disk_critical_free_gb` (REL-12).
    #[error(
        "only {free_gb:.1} GB free on {path}, below the critical threshold of {critical_gb:.1} GB"
    )]
    DiskFull {
        /// The volume that is full.
        path: PathBuf,
        /// Free space measured at the refusal.
        free_gb: f64,
        /// The configured threshold it fell below.
        critical_gb: f64,
    },
    /// The id is already stored with different content. Nothing was written, nothing replaced
    /// (REL-11).
    #[error("frame {frame_id} is already stored with different content (sha256 {stored_sha256})")]
    FrameIdConflict {
        /// The contested id.
        frame_id: FrameId,
        /// The checksum of what the store already holds under it.
        stored_sha256: String,
    },
    /// The requested session name cannot become a directory.
    #[error("session name {slug:?} is unusable: {why}")]
    InvalidSlug {
        /// What was asked for.
        slug: String,
        /// Why it was refused.
        why: &'static str,
    },
}

impl StoreError {
    fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }

    /// The wire code this failure becomes at the API boundary (SDD §4.2).
    ///
    /// Stated here rather than in the API layer so the mapping lives with the failure it describes:
    /// a new variant cannot be added without deciding what the operator is told about it.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Io { .. } | Self::Json { .. } => ErrorCode::Internal,
            Self::DiskFull { .. } => ErrorCode::DiskFull,
            Self::FrameIdConflict { .. } => ErrorCode::FrameIdConflict,
            Self::InvalidSlug { .. } => ErrorCode::Validation,
        }
    }
}

/// What a new session needs to know about itself.
#[derive(Clone, Debug)]
pub struct NewSession {
    /// The operator's name for the night, e.g. `ngc7000`. Prefixed with the date to form the id.
    pub slug: String,
    /// What the session is pointed at, if anything has said yet.
    pub target: Option<serde_json::Value>,
    /// The equipment profile, snapshotted from configuration (`Equipment::from(&config.equipment)`).
    pub equipment: Equipment,
}

/// The field node's session tree, rooted at `storage.sessions_dir`.
#[derive(Debug)]
pub struct FrameStore {
    root: PathBuf,
    thresholds: DiskThresholds,
    /// The session captures are going into. `None` until one is opened or created — SDD §8.1 puts
    /// that between config load and the API coming up.
    active: Mutex<Option<Arc<Session>>>,
    /// Makes the store's own temporaries unique, on the same grounds as the session's: two stores
    /// opened on one tree (a test, an operator running a second binary by mistake) must not share a
    /// half-written `CURRENT` or a half-written manifest.
    nonce: AtomicU64,
}

impl FrameStore {
    /// Open the store, creating the session root and sweeping temporaries a crash left behind.
    ///
    /// The sweep happens here, before the store can be used, so it cannot race a live capture — the
    /// same placement `astroctl-stack` argues for in SDD §5.11.2.
    ///
    /// # Errors
    /// [`StoreError::Io`] if the session root cannot be created. Fatal at startup: a field node
    /// that cannot write frames has nothing to offer, and starting anyway would mean discovering it
    /// at the end of the first exposure.
    pub async fn open(storage: &StorageConfig) -> Result<Self, StoreError> {
        let root = storage.sessions_dir.clone();
        durable::create_dir_durable(&root)
            .await
            .map_err(|e| StoreError::io("create the session root", &root, e))?;

        let store = Self {
            root,
            thresholds: DiskThresholds::from(storage),
            active: Mutex::new(None),
            nonce: AtomicU64::new(1),
        };

        let swept = store.sweep_temporaries().await;
        if swept > 0 {
            tracing::warn!(
                count = swept,
                "removed temporaries left by an interrupted capture"
            );
        }
        Ok(store)
    }

    /// The session root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The REL-12 thresholds this store was configured with.
    #[must_use]
    pub const fn thresholds(&self) -> DiskThresholds {
        self.thresholds
    }

    /// Free space on the session volume against the REL-12 thresholds — what the watchdog polls.
    ///
    /// `None` when free space cannot be determined; see [`crate::disk`] for why that is not
    /// reported as an emergency.
    pub async fn disk_status(&self) -> Option<DiskStatus> {
        disk::free_gb(&self.root)
            .await
            .map(|free| self.thresholds.classify(free))
    }

    /// The session captures are currently going into.
    #[must_use]
    pub fn current(&self) -> Option<Arc<Session>> {
        self.active
            .lock()
            .expect("the active-session slot is not poisoned")
            .clone()
    }

    /// Open the session `CURRENT` points at, and make it active.
    ///
    /// This is the restart path: a field binary coming back up mid-night must continue the session
    /// it was in, because [`Session::reserve_frame_id`]'s guarantee is per session — a new session
    /// directory would start its counter at zero. Callers should try this *before*
    /// [`Self::open_or_create_session`] for exactly that reason.
    ///
    /// Returns `Ok(None)` when there is no `CURRENT`, or when it dangles because its directory was
    /// moved or deleted: neither is an error the node should refuse to start over.
    ///
    /// # Errors
    /// [`StoreError::Io`] or [`StoreError::Json`] if the session exists but its manifest cannot be
    /// read — the one case where continuing silently would risk the counter.
    pub async fn open_current(&self) -> Result<Option<Arc<Session>>, StoreError> {
        let link = self.root.join(CURRENT_LINK);
        let Ok(target) = tokio::fs::read_link(&link).await else {
            return Ok(None);
        };
        // Relative by construction (see `point_current_at`), but a hand-edited absolute link should
        // resolve rather than be pasted onto the root.
        let dir = if target.is_absolute() {
            target.clone()
        } else {
            self.root.join(&target)
        };
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            tracing::warn!(link = %link.display(), target = %target.display(), "CURRENT points at a session that is not there");
            return Ok(None);
        }

        let session_id = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let session = self.attach(session_id, dir, None).await?;
        Ok(Some(session))
    }

    /// Open the session named by today's date and `spec.slug`, creating it if it is not there, and
    /// point `CURRENT` at it.
    ///
    /// **Open-or-create, not create.** A restart that lands on an existing directory must continue
    /// it: the alternative is either refusing to start or a second session whose frame counter
    /// begins at 1 while `frames/` already holds `light_00001.cr3`.
    ///
    /// The date is UTC. SDD §2 confines local time to UI rendering, so a session that begins at
    /// 23:50 local may carry the next day's date — the observing-night question belongs to the
    /// sequence work of §5.6, and inventing an answer here would put a timezone policy in the
    /// durability layer.
    ///
    /// # Errors
    /// [`StoreError::InvalidSlug`] if the name cannot be a directory component;
    /// [`StoreError::Io`] / [`StoreError::Json`] if the directories or the manifest cannot be
    /// written.
    pub async fn open_or_create_session(
        &self,
        spec: NewSession,
    ) -> Result<Arc<Session>, StoreError> {
        validate_slug(&spec.slug)?;
        let created_ts = now_millis();
        let session_id = format!("{}_{}", created_ts.format("%Y-%m-%d"), spec.slug);
        let dir = self.root.join(&session_id);

        let session = self.attach(session_id, dir, Some(spec)).await?;
        Ok(session)
    }

    /// Load or create a session directory and install it as the active one.
    async fn attach(
        &self,
        session_id: String,
        dir: PathBuf,
        spec: Option<NewSession>,
    ) -> Result<Arc<Session>, StoreError> {
        for sub in [
            dir.join(FRAMES_DIR),
            dir.join(CONTROL_DIR),
            dir.join(PREVIEW_DIR),
        ] {
            durable::create_dir_durable(&sub)
                .await
                .map_err(|e| StoreError::io("create the session directory", &sub, e))?;
        }

        let manifest_path = dir.join(SESSION_JSON);
        let manifest = match read_manifest(&manifest_path).await? {
            // An existing manifest wins over the spec: the session already recorded the equipment
            // its frames were taken with, and re-stamping it from today's configuration is exactly
            // the drift the snapshot exists to prevent (see `manifest::Equipment`).
            Some(manifest) => manifest,
            None => {
                // No readable manifest. If the directory already holds frames, the counter is
                // rebuilt from the highest id on disk rather than restarted at zero: a manifest lost
                // to corruption must not become an id that overwrites a captured frame (REL-04,
                // REL-11). Ids granted but never used are re-granted by this path, which is
                // invisible — nothing on disk carries them.
                let recovered = highest_sequence_on_disk(&dir.join(FRAMES_DIR)).await;
                if recovered > 0 {
                    tracing::warn!(
                        session = %session_id,
                        frames_reserved = recovered,
                        "session.json was missing or unreadable; the frame counter was rebuilt from the frames on disk"
                    );
                }
                let (target, equipment) = match spec {
                    Some(spec) => (spec.target, spec.equipment),
                    None => (None, unknown_equipment()),
                };
                let mut manifest =
                    SessionManifest::new(session_id.clone(), now_millis(), target, equipment);
                manifest.frames_reserved = recovered;
                write_manifest(&manifest_path, self.next_nonce(), &manifest).await?;
                manifest
            }
        };

        let session = Arc::new(Session {
            id: session_id.clone(),
            dir,
            thresholds: self.thresholds,
            state: tokio::sync::Mutex::new(manifest),
            nonce: AtomicU64::new(1),
        });

        self.point_current_at(&session_id).await?;
        *self
            .active
            .lock()
            .expect("the active-session slot is not poisoned") = Some(Arc::clone(&session));
        Ok(session)
    }

    /// Swap `CURRENT` onto a session.
    ///
    /// The link is **relative** so the whole `sessions/` tree can be moved, copied to a bigger
    /// card, or mounted at another path without `CURRENT` pointing into a directory that no longer
    /// exists on this machine.
    async fn point_current_at(&self, session_id: &str) -> Result<(), StoreError> {
        let link = self.root.join(CURRENT_LINK);
        durable::symlink_atomic(&link, session_id, self.next_nonce())
            .await
            .map_err(|e| StoreError::io("point CURRENT at the session", &link, e))
    }

    fn next_nonce(&self) -> u64 {
        self.nonce.fetch_add(1, Ordering::Relaxed)
    }

    /// Delete temporaries left behind by a crash or an aborted capture. Returns how many went.
    ///
    /// Never fails the caller: an unreadable session directory is a reason to log and carry on, not
    /// to refuse to start.
    pub async fn sweep_temporaries(&self) -> u64 {
        let mut removed = durable::sweep_temporaries_in(&self.root).await;
        let mut sessions = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(path = %self.root.display(), %error, "cannot scan the session root for leftover temporaries");
                return removed;
            }
        };

        while let Ok(Some(entry)) = sessions.next_entry().await {
            // `file_type` here does not follow symlinks, so `CURRENT` is skipped and the session it
            // names is swept once, under its real name.
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let session = entry.path();
            for dir in [
                session.clone(),
                session.join(FRAMES_DIR),
                session.join(CONTROL_DIR),
                // The preview writer renames through a temporary like every other writer here, so
                // a node killed mid-render leaves one behind. Omitting this line would leak a
                // `.tmp_` JPEG per interrupted capture, forever, in the one directory nothing
                // else ever cleans.
                session.join(PREVIEW_DIR),
            ] {
                removed += durable::sweep_temporaries_in(&dir).await;
            }
        }

        removed
    }
}

/// One session directory: its manifest, its frames, and the counter behind its frame ids.
#[derive(Debug)]
pub struct Session {
    id: String,
    dir: PathBuf,
    thresholds: DiskThresholds,
    /// The manifest as it stands on disk. The lock covers "advance the counter and persist it", so
    /// two tasks reserving at once cannot both be granted the same number (the acceptance criterion
    /// on concurrent reservation) and cannot persist out of order.
    state: tokio::sync::Mutex<SessionManifest>,
    /// Makes each in-flight temporary name unique. See [`crate::durable`] for what shared temporary
    /// names cost.
    nonce: AtomicU64,
}

impl Session {
    /// `YYYY-MM-DD_<slug>`.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<session>/frames`.
    #[must_use]
    pub fn frames_dir(&self) -> PathBuf {
        self.dir.join(FRAMES_DIR)
    }

    /// `<session>/control`.
    #[must_use]
    pub fn control_dir(&self) -> PathBuf {
        self.dir.join(CONTROL_DIR)
    }

    /// `<session>/preview`.
    #[must_use]
    pub fn preview_dir(&self) -> PathBuf {
        self.dir.join(PREVIEW_DIR)
    }

    /// Where frame `id`'s cached preview lives — `<session>/preview/light_00042.jpg`
    /// (SDD §5.7, §5.8.1's `/api/session/frames/{id}/preview.jpg`).
    ///
    /// A pure path computation, and the reason it is a method rather than string concatenation at
    /// the call site: it takes a parsed [`FrameId`], so `../../etc/passwd` cannot reach it. The
    /// route's `{id}` segment is attacker-controlled in the sense that matters — it arrives over
    /// HTTP — and [`FrameId::parse`] is the only way to make one. Returning a path for a frame
    /// that does not exist is correct: the caller is about to write it, or is about to fail to
    /// open it, and both need the path first.
    ///
    /// Unlike [`Self::frames_dir`], the file name keeps the frame's kind prefix. The sidecar
    /// drops it (`quality_00042.json`) because the counter is per session and the kind is in the
    /// sidecar's own body; a preview has no body to carry it, and SDD §5.7 names the file
    /// `light_<id>.jpg` explicitly.
    #[must_use]
    pub fn preview_path(&self, id: &FrameId) -> PathBuf {
        self.preview_dir().join(id.file_name("jpg"))
    }

    /// A copy of the manifest as last persisted.
    pub async fn manifest(&self) -> SessionManifest {
        self.state.lock().await.clone()
    }

    /// Reserve the next frame id, persisting the counter **before** the grant returns (REL-04).
    ///
    /// The ordering is the whole point. A crash after the write but before the frame is captured
    /// burns one id — a gap in the numbering, which costs nothing. A crash between a grant and its
    /// persistence would hand the same id out twice, and the second capture would land on the first
    /// one's file. One of those failures is a cosmetic gap; the other is a lost frame.
    ///
    /// That costs two fsyncs per frame (the manifest and its directory), which against an exposure
    /// measured in seconds is not worth optimizing away — the usual trick of granting a block of
    /// ids and persisting once would burn up to a block on every restart to save milliseconds
    /// nobody is waiting for.
    ///
    /// # Errors
    /// [`StoreError::Io`] or [`StoreError::Json`] if the manifest cannot be persisted. No id is
    /// granted in that case — the counter in memory is advanced only after the write succeeds, so
    /// a retry hands out the same number it failed to persist.
    pub async fn reserve_frame_id(&self, kind: FrameKind) -> Result<FrameId, StoreError> {
        let mut manifest = self.state.lock().await;
        let sequence = manifest.frames_reserved + 1;

        let mut candidate = manifest.clone();
        candidate.frames_reserved = sequence;
        write_manifest(&self.dir.join(SESSION_JSON), self.next_nonce(), &candidate).await?;

        manifest.frames_reserved = sequence;
        Ok(FrameId::new(kind, sequence))
    }

    /// Open a temporary for a frame that is about to be captured.
    ///
    /// The returned handle's path is where the camera writes. A driver that writes the file itself
    /// (libgphoto2 does) must write *into* this path rather than replace it: the handle's file
    /// descriptor is what [`Self::commit_frame`] fsyncs, and fsync is per-inode, so it covers a
    /// foreign writer's bytes but not a foreign writer's replacement file.
    ///
    /// # Errors
    /// [`StoreError::DiskFull`] when free space is below `storage.disk_critical_free_gb` — REL-12's
    /// backstop. Refusing here rather than mid-write is the point: an exposure that starts on a full
    /// volume ends as a truncated raw, and REL-05 promises the opposite. (The graceful
    /// pause-after-the-in-flight-frame belongs to the capture flow, which knows what is in flight.)
    ///
    /// [`StoreError::Io`] if the temporary cannot be created.
    pub async fn begin_frame(
        &self,
        frame_id: FrameId,
        ext: &str,
    ) -> Result<StagedFrame, StoreError> {
        if let Some(status) = self.disk_status().await {
            if status.level == DiskLevel::Critical {
                return Err(StoreError::DiskFull {
                    path: self.dir.clone(),
                    free_gb: status.free_gb,
                    critical_gb: self.thresholds.critical_gb,
                });
            }
        }

        let frames = self.frames_dir();
        let tmp = frames.join(format!(
            "{prefix}{frame_id}.{pid}.{nonce}",
            prefix = durable::TMP_PREFIX,
            pid = std::process::id(),
            nonce = self.next_nonce(),
        ));
        // `create_new`: this process must be the one that made the name. With the nonce it can only
        // fail on a leftover from a previous run with the same pid, and adopting such a file would
        // mean capturing on top of somebody else's bytes.
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
            .map_err(|e| StoreError::io("create the capture temporary", &tmp, e))?;

        Ok(StagedFrame {
            frame_id,
            tmp,
            dest: frames.join(frame_id.file_name(ext)),
            file,
        })
    }

    /// Make a captured frame durable and visible under its final name (REL-05).
    ///
    /// fsync the bytes → link the name with `RENAME_NOREPLACE` → fsync the directory. When the
    /// call returns, the frame survives a power cut and `frame.saved` may be published.
    ///
    /// Committing an id that is already stored is **idempotent when the bytes match** — that is the
    /// retry path after a crash between the rename and whatever the caller does next — and a
    /// [`StoreError::FrameIdConflict`] when they do not. Nothing is overwritten either way (REL-11).
    ///
    /// # Errors
    /// [`StoreError::Io`] if the bytes cannot be made durable or the name cannot be linked;
    /// [`StoreError::FrameIdConflict`] as above. On any failure the temporary is **left on disk**:
    /// this node is the origin of these bytes and nothing can re-send them, so the operator gets a
    /// chance at them and the next startup sweep reclaims what they leave.
    pub async fn commit_frame(&self, staged: StagedFrame) -> Result<SavedFrame, StoreError> {
        let StagedFrame {
            frame_id,
            tmp,
            dest,
            file,
        } = staged;

        // The bytes first: a name that is durable before its contents are is how a power cut
        // produces a zero-length frame that every later layer believes in.
        file.sync_all()
            .await
            .map_err(|e| StoreError::io("fsync the captured frame", &tmp, e))?;
        drop(file);

        let (sha256, size_bytes) = durable::hash_and_size(&tmp)
            .await
            .map_err(|e| StoreError::io("read back the captured frame", &tmp, e))?;

        let linked = durable::link_no_replace(&tmp, &dest)
            .await
            .map_err(|e| StoreError::io("link the frame into the session", &dest, e))?;

        match linked {
            Linked::Stored => {
                // The contents were fsynced above; this is the *name*, which lives in the
                // directory and is just as lost without its own fsync.
                let frames = self.frames_dir();
                durable::fsync_dir(&frames)
                    .await
                    .map_err(|e| StoreError::io("fsync the frames directory", frames, e))?;
            }
            Linked::AlreadyPresent => {
                let (stored_sha256, _) = durable::hash_and_size(&dest)
                    .await
                    .map_err(|e| StoreError::io("read the stored frame", &dest, e))?;
                if stored_sha256 != sha256 {
                    return Err(StoreError::FrameIdConflict {
                        frame_id,
                        stored_sha256,
                    });
                }
                // Identical bytes: the frame was already committed and this is a retry finishing
                // the job. Adopt what is there — rewriting it is the REL-11 violation this whole
                // path exists to prevent.
                durable::remove_quietly(&tmp).await;
                tracing::info!(%frame_id, "the frame was already stored with these bytes; the retry adopted it");
            }
        }

        Ok(SavedFrame {
            frame_id,
            path: dest,
            sha256,
            size_bytes,
            committed_ts: now_millis(),
        })
    }

    /// Write `control/quality_<id>.json` for a committed frame (SDD §5.5, §6).
    ///
    /// Called after [`Self::commit_frame`], never before: the sidecar carries the checksum of the
    /// bytes as they were read back from disk, which is a statement that can only be made once they
    /// are there.
    ///
    /// # Errors
    /// [`StoreError::Io`] or [`StoreError::Json`]. Unlike the mirror's sidecar, this one is not
    /// derived from anything — the exposure parameters exist nowhere else — so the failure is the
    /// caller's to see and retry rather than a line in a log.
    pub async fn write_quality(
        &self,
        saved: &SavedFrame,
        capture: &CaptureParams,
    ) -> Result<PathBuf, StoreError> {
        let path = self.control_dir().join(saved.frame_id.quality_file_name());
        let quality = Quality {
            v: SCHEMA_VERSION,
            frame_id: saved.frame_id,
            started_ts: capture.started_ts,
            exposure_s: capture.exposure_s,
            settings: capture.settings.clone(),
            sha256: saved.sha256.clone(),
            size_bytes: saved.size_bytes,
        };

        write_json(&path, self.next_nonce(), &quality).await?;
        Ok(path)
    }

    /// Everything `/api/session/current` needs: the manifest and the frames on disk (SDD §5.8.1).
    ///
    /// Read from the directory on each call rather than from an index kept in memory. A session is
    /// bounded by a night — hundreds of frames, a few hundred bytes of sidecar each — and an index
    /// would be a second account of what is stored, free to disagree with the frames after a crash.
    /// The directory cannot.
    ///
    /// # Errors
    /// [`StoreError::Io`] if the frames directory cannot be read. A single unreadable *sidecar* is
    /// not an error: its frame is listed with `quality: null`, which is exactly the state a crash
    /// between `commit_frame` and `write_quality` leaves behind.
    pub async fn view(&self) -> Result<SessionView, StoreError> {
        let manifest = self.manifest().await;
        let frames = self.list_frames().await?;

        Ok(SessionView {
            session_id: manifest.session_id,
            created_ts: manifest.created_ts,
            target: manifest.target,
            equipment: manifest.equipment,
            frames_reserved: manifest.frames_reserved,
            sequence_state: manifest.sequence_state,
            frame_count: frames.len(),
            frames,
        })
    }

    /// Free space against the REL-12 thresholds, for the frame this session is about to take.
    pub async fn disk_status(&self) -> Option<DiskStatus> {
        disk::free_gb(&self.dir)
            .await
            .map(|free| self.thresholds.classify(free))
    }

    /// The committed frames, oldest id first.
    async fn list_frames(&self) -> Result<Vec<FrameEntry>, StoreError> {
        let frames_dir = self.frames_dir();
        let mut entries = tokio::fs::read_dir(&frames_dir)
            .await
            .map_err(|e| StoreError::io("list the frames directory", &frames_dir, e))?;

        let mut frames = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| StoreError::io("list the frames directory", &frames_dir, e))?
        {
            let path = entry.path();
            // A temporary is a capture in flight or the residue of one that died. Neither is a
            // frame, and showing one would be "a partial frame visible" in T-DUR-1's words.
            if durable::is_temporary(&path) {
                continue;
            }
            let Some(frame_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(FrameId::parse)
            else {
                continue;
            };
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }

            frames.push(FrameEntry {
                frame_id,
                file_name: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                size_bytes: metadata.len(),
                quality: self.read_quality(frame_id).await,
            });
        }

        frames.sort_by_key(|frame| frame.frame_id.sequence());
        Ok(frames)
    }

    /// A frame's sidecar, or `None` when it is absent or unreadable.
    async fn read_quality(&self, frame_id: FrameId) -> Option<Quality> {
        let path = self.control_dir().join(frame_id.quality_file_name());
        let bytes = tokio::fs::read(&path).await.ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(quality) => Some(quality),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "a quality sidecar is unreadable");
                None
            }
        }
    }

    fn next_nonce(&self) -> u64 {
        self.nonce.fetch_add(1, Ordering::Relaxed)
    }
}

/// A frame being captured: a temporary that belongs to nobody until it is committed.
#[derive(Debug)]
pub struct StagedFrame {
    frame_id: FrameId,
    tmp: PathBuf,
    dest: PathBuf,
    file: tokio::fs::File,
}

impl StagedFrame {
    /// The id this staging will commit under.
    #[must_use]
    pub const fn frame_id(&self) -> FrameId {
        self.frame_id
    }

    /// Where the camera should write. See [`Session::begin_frame`] for the one rule about it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.tmp
    }

    /// Where the frame will appear once committed. Useful to a caller that wants to log the
    /// destination before the capture starts; the file is not there yet.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.dest
    }

    /// Append bytes, for a caller that has the frame in hand rather than a driver writing the file.
    ///
    /// # Errors
    /// [`StoreError::Io`] if the write fails.
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
        self.file
            .write_all(bytes)
            .await
            .map_err(|e| StoreError::io("write the captured frame", &self.tmp, e))
    }

    /// Abandon the capture and remove the temporary.
    ///
    /// The counterpart to the "leave it on disk" rule for *failed* commits: an abort is a decision,
    /// not a failure, so there is nothing to preserve.
    pub async fn abort(self) {
        let Self { tmp, file, .. } = self;
        drop(file);
        durable::remove_quietly(&tmp).await;
    }
}

/// A frame that is on disk, fsynced, and under its final name — everything `frame.saved` needs
/// (SDD §4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedFrame {
    /// The frame's id.
    pub frame_id: FrameId,
    /// Absolute path of the stored frame.
    pub path: PathBuf,
    /// Lowercase hex SHA-256 of the stored bytes, read back from disk.
    pub sha256: String,
    /// Size of the stored frame.
    pub size_bytes: u64,
    /// When the commit completed.
    pub committed_ts: DateTime<Utc>,
}

/// What `/api/session/current` returns: the manifest, plus the frames actually on disk.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SessionView {
    /// `YYYY-MM-DD_<slug>`.
    pub session_id: String,
    /// When the session was created.
    #[serde(with = "ts_rfc3339_millis")]
    pub created_ts: DateTime<Utc>,
    /// The target, as the manifest records it.
    pub target: Option<serde_json::Value>,
    /// The equipment snapshot.
    pub equipment: Equipment,
    /// The highest id granted (REL-04). Exceeds `frame_count` by every burned or aborted id, which
    /// is information: the gap is what a crash looks like from the outside.
    pub frames_reserved: u64,
    /// Reserved for the Phase 2a sequence FSM.
    pub sequence_state: Option<serde_json::Value>,
    /// How many frames are stored.
    pub frame_count: usize,
    /// The frames, oldest id first.
    pub frames: Vec<FrameEntry>,
}

/// One stored frame, as the API reports it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct FrameEntry {
    /// The frame's id.
    pub frame_id: FrameId,
    /// The file name within `frames/`.
    pub file_name: String,
    /// Size on disk.
    pub size_bytes: u64,
    /// The sidecar, or `null` when the frame has none — a frame committed by a process that died
    /// before writing its metadata is still a frame (REL-05), and hiding it would be a lie about
    /// what the node holds.
    pub quality: Option<Quality>,
}

/// A session directory that names no equipment, for the recovery path where a manifest was lost
/// and no configuration was supplied.
///
/// Explicit sentinel strings rather than empty ones: this text ends up in a calibration-matching
/// key, and "unknown" refuses to match a real profile, whereas "" quietly matches anything a
/// future matcher treats as a wildcard.
fn unknown_equipment() -> Equipment {
    Equipment {
        telescope: "unknown".to_owned(),
        camera: "unknown".to_owned(),
        filter: "unknown".to_owned(),
    }
}

/// Reject a session name that cannot safely become a directory under the session root.
///
/// This is not cosmetic. The slug reaches the store from the API, and `..` or a leading `/` would
/// make `sessions_dir.join(id)` address a directory outside the tree the operator configured.
fn validate_slug(slug: &str) -> Result<(), StoreError> {
    let refuse = |why| {
        Err(StoreError::InvalidSlug {
            slug: slug.to_owned(),
            why,
        })
    };

    if slug.is_empty() {
        return refuse("a session needs a name");
    }
    if slug.len() > MAX_SLUG {
        return refuse("longer than 64 characters");
    }
    if slug.starts_with('.') || slug.starts_with('-') {
        return refuse("must not start with a dot or a dash");
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return refuse("only letters, digits, dashes and underscores");
    }
    Ok(())
}

/// The highest frame sequence number present in a frames directory, or 0 for none.
///
/// The counter's floor when a manifest has to be rebuilt. Temporaries are ignored: they are not
/// frames, and a partial capture's id is free to be granted again.
async fn highest_sequence_on_disk(frames_dir: &Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(frames_dir).await else {
        return 0;
    };
    let mut highest = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if durable::is_temporary(&path) {
            continue;
        }
        if let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(FrameId::parse)
        {
            highest = highest.max(id.sequence());
        }
    }
    highest
}

/// Read `session.json`, distinguishing "not there" from "there and unreadable".
///
/// Both come back as `Ok(None)` for the caller to rebuild from, and the unreadable case is logged:
/// refusing to open the session would leave the node unable to capture at all, which is a worse
/// answer to a corrupt metadata file than rebuilding the one field that matters.
async fn read_manifest(path: &Path) -> Result<Option<SessionManifest>, StoreError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::io("read the session manifest", path, error)),
    };

    match serde_json::from_slice(&bytes) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "the session manifest is unreadable and will be rebuilt");
            Ok(None)
        }
    }
}

/// Persist a manifest with the tmp-fsync-rename discipline (SDD §5.5).
async fn write_manifest(
    path: &Path,
    nonce: u64,
    manifest: &SessionManifest,
) -> Result<(), StoreError> {
    write_json(path, nonce, manifest).await
}

/// Serialize and write any of this crate's persisted documents.
async fn write_json<T: Serialize>(path: &Path, nonce: u64, value: &T) -> Result<(), StoreError> {
    // Pretty on purpose: SDD §6 keeps these files human-readable, and the reader is an operator
    // with `cat` and a head-torch.
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| StoreError::Json {
        path: path.to_owned(),
        source,
    })?;
    durable::write_atomic(path, nonce, &bytes)
        .await
        .map_err(|e| StoreError::io("write", path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{capture_params, equipment, TempDir};

    /// The layout SDD §5.5 fixes, as `astroctl-stack::mirror` asserts it from the other side.
    ///
    /// Included by path rather than through a crate dependency: it is a *fixture*, and the
    /// dependency matrix of ADD §5.6 should not acquire an edge to carry one. Both crates read this
    /// same file, which is what makes it a constraint rather than a description — a layout change
    /// here breaks the mirror's tests and vice versa.
    const LAYOUT_FIXTURE: &str = include_str!("../testdata/session-layout.txt");

    async fn store(dir: &TempDir) -> FrameStore {
        FrameStore::open(&dir.storage())
            .await
            .expect("a fresh store opens")
    }

    async fn session(store: &FrameStore) -> Arc<Session> {
        store
            .open_or_create_session(NewSession {
                slug: "ngc7000".to_owned(),
                target: None,
                equipment: equipment(),
            })
            .await
            .expect("the session is created")
    }

    /// Reserve, capture and commit one frame; returns what the store reports about it.
    async fn capture(session: &Session, body: &[u8]) -> SavedFrame {
        let id = session
            .reserve_frame_id(FrameKind::Light)
            .await
            .expect("an id is reserved");
        let mut staged = session.begin_frame(id, "cr3").await.expect("staging opens");
        staged.write_all(body).await.expect("the frame is written");
        session
            .commit_frame(staged)
            .await
            .expect("the frame commits")
    }

    #[tokio::test]
    async fn a_new_session_is_named_for_the_date_and_the_slug() {
        let dir = TempDir::new();
        let store = store(&dir).await;

        let session = session(&store).await;

        let today = now_millis().format("%Y-%m-%d").to_string();
        assert_eq!(session.id(), format!("{today}_ngc7000"));
        assert_eq!(session.dir(), store.root().join(session.id()));
    }

    /// The other half of the shared fixture: `astroctl-stack` asserts its mirror against this file,
    /// and until now nothing asserted the *origin* against it — a fixture one crate writes and one
    /// crate checks constrains nothing.
    #[tokio::test]
    async fn the_layout_matches_the_fixture_shared_with_astroctl_stack() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;

        // The fixture names `light_00042`, so the test takes 42 ids to get there rather than
        // constructing one behind the store's back — which also pins the zero padding.
        let mut saved = None;
        for _ in 0..42 {
            saved = Some(capture(&session, b"raw bytes").await);
        }
        let saved = saved.expect("42 frames were captured");
        assert_eq!(saved.frame_id.to_string(), "light_00042");
        session
            .write_quality(&saved, &capture_params())
            .await
            .expect("the sidecar is written");

        let expected: Vec<&str> = LAYOUT_FIXTURE
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        assert!(!expected.is_empty(), "the fixture is empty");
        for relative in expected {
            let path = session.dir().join(relative);
            assert!(
                tokio::fs::try_exists(&path).await.unwrap_or(false),
                "the layout fixture requires {relative}, which the frame store did not produce"
            );
        }
    }

    #[tokio::test]
    async fn a_session_has_a_preview_directory_and_names_previews_after_their_frame() {
        // SDD §5.7 names the cache `<session>/preview/light_<id>.jpg`. It exists from `attach`
        // rather than being created on first write, so the preview task never has to decide
        // whether a missing directory is a fresh session or a broken one.
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;

        assert!(
            tokio::fs::try_exists(session.preview_dir())
                .await
                .unwrap_or(false),
            "the preview directory must exist as soon as the session does"
        );

        let saved = capture(&session, b"raw bytes").await;
        assert_eq!(
            session.preview_path(&saved.frame_id),
            session.dir().join("preview").join("light_00001.jpg"),
            "the preview keeps the frame's kind prefix — the sidecar drops it, this does not"
        );
    }

    #[tokio::test]
    async fn the_sweep_reaches_the_preview_directory() {
        // A node killed mid-render leaves a `.tmp_` JPEG behind. If the sweep did not walk this
        // directory the leak would be one file per interrupted capture, forever, in the one place
        // nothing else ever cleans.
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;

        let leftover = session.preview_dir().join(".tmp_light_00007.jpg");
        tokio::fs::write(&leftover, b"half a preview")
            .await
            .expect("the temporary is written");

        assert!(store.sweep_temporaries().await >= 1);
        assert!(
            !tokio::fs::try_exists(&leftover).await.unwrap_or(true),
            "the sweep left a preview temporary behind"
        );
    }

    /// REL-04, the in-process half: the counter is on disk before the caller has the id.
    #[tokio::test]
    async fn an_id_is_persisted_before_it_is_granted() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;

        let id = session.reserve_frame_id(FrameKind::Light).await.unwrap();

        let manifest: SessionManifest = serde_json::from_slice(
            &tokio::fs::read(session.dir().join(SESSION_JSON))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(id.sequence(), 1);
        assert_eq!(manifest.frames_reserved, 1, "the grant outran the write");
        assert_eq!(manifest.v, SCHEMA_VERSION);
    }

    /// Acceptance criterion 3: two tasks reserving at once get different ids.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reservations_never_collide() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;

        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let session = Arc::clone(&session);
                tokio::spawn(async move {
                    let mut ids = Vec::new();
                    for _ in 0..10 {
                        ids.push(session.reserve_frame_id(FrameKind::Light).await.unwrap());
                    }
                    ids
                })
            })
            .collect();

        let mut ids = Vec::new();
        for task in tasks {
            ids.extend(task.await.unwrap());
        }
        ids.sort_unstable();
        let unique = {
            let mut copy = ids.clone();
            copy.dedup();
            copy
        };
        assert_eq!(ids.len(), 40);
        assert_eq!(unique.len(), 40, "an id was granted twice");
        assert_eq!(session.manifest().await.frames_reserved, 40);
    }

    /// The restart path in one process: a store reopened on the same directory continues the
    /// counter instead of handing out ids that already have frames behind them.
    #[tokio::test]
    async fn reopening_a_session_continues_its_counter() {
        let dir = TempDir::new();
        let storage = dir.storage();
        let first = FrameStore::open(&storage).await.unwrap();
        let session = session(&first).await;
        capture(&session, b"raw").await;
        drop(first);

        let second = FrameStore::open(&storage).await.unwrap();
        let resumed = second
            .open_current()
            .await
            .unwrap()
            .expect("CURRENT names the session");

        assert_eq!(resumed.id(), session.id());
        assert_eq!(
            resumed
                .reserve_frame_id(FrameKind::Light)
                .await
                .unwrap()
                .sequence(),
            2
        );
    }

    #[tokio::test]
    async fn current_is_a_relative_symlink_so_the_tree_can_move() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;

        let target = tokio::fs::read_link(store.root().join(CURRENT_LINK))
            .await
            .unwrap();

        assert_eq!(target, Path::new(session.id()));
        assert!(target.is_relative());
    }

    #[tokio::test]
    async fn no_current_means_no_session_rather_than_a_failure() {
        let dir = TempDir::new();
        let store = store(&dir).await;

        assert!(store.open_current().await.unwrap().is_none());
        assert!(store.current().is_none());
    }

    /// Acceptance criterion 2, the watch test: a committed frame is not touched by anything the
    /// store can be asked to do afterwards (REL-11).
    #[tokio::test]
    async fn every_later_operation_leaves_a_committed_frame_byte_identical() {
        let dir = TempDir::new();
        let storage = dir.storage();
        let store = FrameStore::open(&storage).await.unwrap();
        let session = session(&store).await;
        let saved = capture(&session, b"the original bytes").await;

        let before = tokio::fs::metadata(&saved.path).await.unwrap();
        let (mtime, len) = (before.modified().unwrap(), before.len());

        // Every other operation this store offers.
        session
            .write_quality(&saved, &capture_params())
            .await
            .unwrap();
        session.reserve_frame_id(FrameKind::Light).await.unwrap();
        capture(&session, b"a different frame").await;
        session.view().await.unwrap();
        session.manifest().await;
        session.disk_status().await;
        store.disk_status().await;
        store.sweep_temporaries().await;
        store.open_current().await.unwrap();
        session
            .write_quality(&saved, &capture_params())
            .await
            .unwrap();

        // A retry of the same id with the same bytes — the idempotent path, which reads the stored
        // frame and must not rewrite it.
        let mut retry = session.begin_frame(saved.frame_id, "cr3").await.unwrap();
        retry.write_all(b"the original bytes").await.unwrap();
        session.commit_frame(retry).await.unwrap();

        // And a retry with *different* bytes, which is refused.
        let mut conflicting = session.begin_frame(saved.frame_id, "cr3").await.unwrap();
        conflicting.write_all(b"tampered").await.unwrap();
        let error = conflicting_commit(&session, conflicting).await;
        assert!(
            matches!(error, StoreError::FrameIdConflict { .. }),
            "{error}"
        );

        // A whole new store on the same directory, sweep and all.
        let reopened = FrameStore::open(&storage).await.unwrap();
        reopened.open_current().await.unwrap();

        let after = tokio::fs::metadata(&saved.path).await.unwrap();
        assert_eq!(after.modified().unwrap(), mtime, "the frame's mtime moved");
        assert_eq!(after.len(), len);
        assert_eq!(
            tokio::fs::read(&saved.path).await.unwrap(),
            b"the original bytes"
        );
    }

    async fn conflicting_commit(session: &Session, staged: StagedFrame) -> StoreError {
        session
            .commit_frame(staged)
            .await
            .expect_err("a reused id with different bytes must be refused")
    }

    /// The retry of a commit that already happened is a success, not a duplicate frame.
    #[tokio::test]
    async fn recommitting_identical_bytes_is_idempotent() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;
        let saved = capture(&session, b"raw").await;

        let mut retry = session.begin_frame(saved.frame_id, "cr3").await.unwrap();
        retry.write_all(b"raw").await.unwrap();
        let again = session.commit_frame(retry).await.unwrap();

        assert_eq!(again.sha256, saved.sha256);
        assert_eq!(again.path, saved.path);
        assert_eq!(session.view().await.unwrap().frame_count, 1);
        // The losing temporary is gone: an adopted retry cleans up after itself.
        let mut entries = tokio::fs::read_dir(session.frames_dir()).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec!["light_00001.cr3".to_owned()]);
    }

    /// REL-12: a capture never starts below the critical threshold.
    #[tokio::test]
    async fn begin_frame_is_refused_below_the_critical_threshold() {
        let dir = TempDir::new();
        // Thresholds above any plausible free space make every capture a refusal, which is the only
        // way to exercise a full volume without filling one.
        let storage = dir.storage_with_thresholds(99_000.0, 90_000.0);
        let store = FrameStore::open(&storage).await.unwrap();
        let session = session(&store).await;
        let id = session.reserve_frame_id(FrameKind::Light).await.unwrap();

        let error = session
            .begin_frame(id, "cr3")
            .await
            .expect_err("a full volume refuses the capture");

        assert!(matches!(error, StoreError::DiskFull { .. }), "{error}");
        assert_eq!(error.code(), ErrorCode::DiskFull);
        assert_eq!(
            store.disk_status().await.unwrap().level,
            DiskLevel::Critical
        );
    }

    #[tokio::test]
    async fn the_warn_threshold_does_not_stop_a_capture() {
        let dir = TempDir::new();
        // Warn above any plausible free space, critical below: REL-12 alerts, capture continues.
        let storage = dir.storage_with_thresholds(99_000.0, 0.0);
        let store = FrameStore::open(&storage).await.unwrap();
        let session = session(&store).await;

        capture(&session, b"raw").await;

        assert_eq!(store.disk_status().await.unwrap().level, DiskLevel::Warn);
        assert_eq!(session.view().await.unwrap().frame_count, 1);
    }

    #[tokio::test]
    async fn an_aborted_capture_leaves_nothing_behind() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;
        let id = session.reserve_frame_id(FrameKind::Light).await.unwrap();

        let mut staged = session.begin_frame(id, "cr3").await.unwrap();
        staged.write_all(b"half a frame").await.unwrap();
        staged.abort().await;

        let mut entries = tokio::fs::read_dir(session.frames_dir()).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
        // The id is burned rather than recycled — the cheap half of REL-04's trade.
        assert_eq!(
            session
                .reserve_frame_id(FrameKind::Light)
                .await
                .unwrap()
                .sequence(),
            2
        );
    }

    #[tokio::test]
    async fn the_startup_sweep_removes_what_an_interrupted_capture_left() {
        let dir = TempDir::new();
        let storage = dir.storage();
        let store = FrameStore::open(&storage).await.unwrap();
        let session = session(&store).await;
        capture(&session, b"raw").await;
        let session_dir = session.dir().to_owned();
        tokio::fs::write(session_dir.join("frames/.tmp_light_00002.7.0"), b"half")
            .await
            .unwrap();
        tokio::fs::write(
            session_dir.join("control/.tmp_quality_00002.json.7.0"),
            b"{}",
        )
        .await
        .unwrap();
        tokio::fs::write(session_dir.join(".tmp_session.json.7.0"), b"{}")
            .await
            .unwrap();

        let reopened = FrameStore::open(&storage).await.unwrap();

        assert_eq!(
            reopened.sweep_temporaries().await,
            0,
            "open() already swept"
        );
        assert!(session_dir.join("frames/light_00001.cr3").exists());
        let view = reopened
            .open_current()
            .await
            .unwrap()
            .unwrap()
            .view()
            .await
            .unwrap();
        assert_eq!(view.frame_count, 1);
    }

    #[tokio::test]
    async fn the_view_lists_frames_in_order_with_their_sidecars() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;
        let first = capture(&session, b"one").await;
        let second = capture(&session, b"two").await;
        session
            .write_quality(&first, &capture_params())
            .await
            .unwrap();
        // `second` deliberately has no sidecar — the crash window T-DUR-1 kills in.

        let view = session.view().await.unwrap();

        assert_eq!(view.session_id, session.id());
        assert_eq!(view.equipment, equipment());
        assert_eq!(view.frames_reserved, 2);
        assert_eq!(view.frame_count, 2);
        assert_eq!(view.frames[0].frame_id, first.frame_id);
        assert_eq!(view.frames[0].file_name, "light_00001.cr3");
        assert_eq!(view.frames[0].size_bytes, 3);
        assert_eq!(
            view.frames[0].quality.as_ref().unwrap().sha256,
            first.sha256
        );
        assert_eq!(view.frames[1].frame_id, second.frame_id);
        assert!(
            view.frames[1].quality.is_none(),
            "a frame with no sidecar is still a frame"
        );
    }

    /// A file the store did not write must not appear as a frame in the API's answer.
    #[tokio::test]
    async fn the_view_ignores_temporaries_and_strangers() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;
        capture(&session, b"raw").await;
        tokio::fs::write(session.frames_dir().join(".tmp_light_00002.7.0"), b"half")
            .await
            .unwrap();
        tokio::fs::write(session.frames_dir().join("notes.txt"), b"hello")
            .await
            .unwrap();
        tokio::fs::create_dir(session.frames_dir().join("light_00009"))
            .await
            .unwrap();

        let view = session.view().await.unwrap();

        assert_eq!(view.frame_count, 1);
        assert_eq!(view.frames[0].file_name, "light_00001.cr3");
    }

    #[tokio::test]
    async fn the_view_serializes_the_way_the_api_will_return_it() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;
        let saved = capture(&session, b"raw").await;
        session
            .write_quality(&saved, &capture_params())
            .await
            .unwrap();

        let json = serde_json::to_value(session.view().await.unwrap()).unwrap();

        assert_eq!(json["frames"][0]["frame_id"], "light_00001");
        assert_eq!(json["frames"][0]["quality"]["v"], 1);
        assert_eq!(json["frames_reserved"], 1);
        assert!(json["sequence_state"].is_null());
        assert!(
            json["created_ts"].as_str().unwrap().ends_with('Z'),
            "timestamps are UTC RFC 3339 with millis (SDD §2)"
        );
    }

    #[tokio::test]
    async fn the_sidecar_records_what_the_frame_was_taken_with() {
        let dir = TempDir::new();
        let store = store(&dir).await;
        let session = session(&store).await;
        let saved = capture(&session, b"raw").await;

        let path = session
            .write_quality(&saved, &capture_params())
            .await
            .unwrap();

        assert_eq!(path, session.control_dir().join("quality_00001.json"));
        let quality: Quality =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(quality.frame_id, saved.frame_id);
        assert_eq!(quality.sha256, saved.sha256);
        assert_eq!(quality.size_bytes, 3);
        assert!((quality.exposure_s - 120.0).abs() < f64::EPSILON);
        assert_eq!(quality.settings.iso, "1600");
    }

    /// A session name reaches this layer from the API. `..` must not address the operator's home
    /// directory.
    #[tokio::test]
    async fn a_session_name_that_could_escape_the_tree_is_refused() {
        let dir = TempDir::new();
        let store = store(&dir).await;

        for slug in ["..", "../escape", "a/b", "", ".hidden", "-dash", "sp ace"] {
            let error = store
                .open_or_create_session(NewSession {
                    slug: slug.to_owned(),
                    target: None,
                    equipment: equipment(),
                })
                .await
                .expect_err("a name that cannot be a directory component was accepted");
            assert_eq!(error.code(), ErrorCode::Validation, "{slug}: {error}");
        }
    }

    /// A manifest lost to corruption must not restart the counter on top of stored frames.
    #[tokio::test]
    async fn a_corrupt_manifest_rebuilds_the_counter_from_the_frames_on_disk() {
        let dir = TempDir::new();
        let storage = dir.storage();
        let store = FrameStore::open(&storage).await.unwrap();
        let session = session(&store).await;
        for _ in 0..3 {
            capture(&session, b"raw").await;
        }
        tokio::fs::write(session.dir().join(SESSION_JSON), b"{ truncated")
            .await
            .unwrap();
        let session_dir = session.dir().to_owned();
        drop(store);

        let reopened = FrameStore::open(&storage).await.unwrap();
        let recovered = reopened.open_current().await.unwrap().unwrap();

        assert_eq!(recovered.manifest().await.frames_reserved, 3);
        assert_eq!(
            recovered
                .reserve_frame_id(FrameKind::Light)
                .await
                .unwrap()
                .sequence(),
            4,
            "the counter restarted over stored frames"
        );
        assert!(session_dir.join("frames/light_00003.cr3").exists());
    }

    #[tokio::test]
    async fn a_dangling_current_is_not_a_startup_failure() {
        let dir = TempDir::new();
        let storage = dir.storage();
        let store = FrameStore::open(&storage).await.unwrap();
        let session = session(&store).await;
        tokio::fs::remove_dir_all(session.dir()).await.unwrap();

        assert!(store.open_current().await.unwrap().is_none());
    }
}
