//! The mirrored session archive — SDD §5.11.3, layout per §5.5, durability per §5.11.2.
//!
//! The archive is the authoritative copy of every frame the system captures (IPP-09), and the
//! contract §5.11 opens with is absolute: **an ack means the bytes are on this node's disk,
//! fsynced, and their checksum matched.** Everything here exists to make that sentence true
//! rather than approximately true.
//!
//! # The order, and why it is that order
//!
//! ```text
//! stream → frames/.tmp_<frame_id>.<nonce>   hashing as the bytes arrive
//!   → fsync the file
//!   → renameat2(RENAME_NOREPLACE) into frames/<frame_id>.<ext>
//!   → fsync the frames directory              ← the name is now durable, not just the inode
//!   → control/quality_<id>.json               (derived; a failure here does not un-store a frame)
//!   → journal row                             ← REL-13's authority, written last
//!   → session.json                            (derived)
//! ```
//!
//! Frame first, journal second: a crash between them leaves a frame nobody has recorded, which
//! costs one retransmission. The other order would leave a *record of a frame that is not there*,
//! and the field node deletes its only copy on the strength of that record (REL-13). The journal
//! may under-claim; it may never over-claim.
//!
//! # Immutability is enforced by the kernel, not by a check
//!
//! REL-11 makes raw frames immutable. The obvious implementation — `metadata()` then `rename` —
//! has a window between the two syscalls in which a concurrent retry of the same frame lands, and
//! `rename(2)` replaces silently. `renameat2(RENAME_NOREPLACE)` does the whole thing atomically
//! or fails with `EEXIST`, and `EEXIST` is a fact worth having: it is the crash-recovery path
//! below.
//!
//! The temporary name carries a nonce for the same reason. SDD §5.11.2 spells it
//! `.tmp_<frame_id>`, which two overlapping uploads of one frame id share — and the loser goes on
//! writing into a file descriptor the winner has already renamed *into the archive*. That is a
//! stored raw being modified after the fact, which is precisely what REL-11 forbids.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use astroctl_core::event::{now_millis, ts_rfc3339_millis};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufWriter};

use crate::journal::{Journal, JournalError, NewFrame, SessionCounts, StoredFrame};

/// Where the raw frames live, under `sessions/<session_id>/` (SDD §5.5).
pub const FRAMES_DIR: &str = "frames";

/// Where the per-frame control metadata lives (SDD §5.5).
pub const CONTROL_DIR: &str = "control";

/// The per-session manifest (SDD §5.5).
pub const SESSION_JSON: &str = "session.json";

/// Marks a file that is mid-write and belongs to nobody until it is renamed.
const TMP_PREFIX: &str = ".tmp_";

/// Schema version of `session.json` and the quality sidecars (SDD §2).
const MIRROR_SCHEMA_VERSION: u16 = 1;

/// Write-buffer size for the streaming frame write.
///
/// Not cosmetic: every `write` on a `tokio::fs::File` is dispatched to the blocking pool, and
/// `multer` hands over chunks as small as the network delivers them. Buffering turns a 25 MB
/// upload from a few thousand pool round-trips into a couple of dozen.
const WRITE_BUFFER_BYTES: usize = 1 << 20;

/// Read-buffer size for re-hashing a frame that is already in the archive (the recovery path).
const READ_BUFFER_BYTES: usize = 1 << 20;

/// What the field node knows about a session and this node does not (SDD §5.11.3: `session.json`
/// is *constructed* from ingest metadata, never copied).
///
/// Deliberately opaque JSON. The field node owns the schema of a target and of an equipment
/// block; re-declaring it here would make the stack node a second definition of it, and the two
/// would drift the first time a field-side field was added.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionMeta {
    /// What the session is pointed at, as the field node recorded it.
    #[serde(default)]
    pub target: Option<serde_json::Value>,
    /// The equipment block from the field node's configuration.
    #[serde(default)]
    pub equipment: Option<serde_json::Value>,
    /// When the session was created on the field node.
    #[serde(default)]
    pub created_ts: Option<DateTime<Utc>>,
}

/// `sessions/<session_id>/session.json` as this node writes it.
///
/// Every field is always serialized, `null` included — the same discipline as the error envelope
/// of SDD §4.2: a reader should not have to distinguish "absent" from "not known yet".
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Manifest {
    /// Schema version (SDD §2).
    pub v: u16,
    /// The session this describes.
    pub session_id: String,
    /// When the session was created, as reported by the field node; the first arrival on this
    /// node when it never reported one.
    #[serde(with = "ts_rfc3339_millis")]
    pub created_ts: DateTime<Utc>,
    /// Target, verbatim from the field node.
    pub target: Option<serde_json::Value>,
    /// Equipment, verbatim from the field node.
    pub equipment: Option<serde_json::Value>,
    /// What this node holds — the part the field node cannot know.
    pub mirror: MirrorStats,
}

/// The mirror's own view of a session.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MirrorStats {
    /// Frames stored here for this session.
    pub frame_count: u64,
    /// When the first of them arrived.
    #[serde(with = "ts_rfc3339_millis")]
    pub first_ingest_ts: DateTime<Utc>,
    /// When the last of them arrived. A session that ended hours ago and is still gaining frames
    /// is normal, not an anomaly (IPP-15).
    #[serde(with = "ts_rfc3339_millis")]
    pub last_ingest_ts: DateTime<Utc>,
}

/// `control/quality_<id>.json` — SDD §5.5 ("Phase 1: exposure params, sha256, size").
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Quality {
    /// Schema version (SDD §2).
    pub v: u16,
    /// The frame this describes.
    pub frame_id: String,
    /// Verified lowercase hex SHA-256 of the stored bytes.
    pub sha256: String,
    /// Size of the stored frame.
    pub size_bytes: u64,
    /// When this node accepted it.
    #[serde(with = "ts_rfc3339_millis")]
    pub received_ts: DateTime<Utc>,
    /// The capture parameters, verbatim as the field node recorded them — same reasoning as
    /// [`SessionMeta`].
    pub capture: Option<serde_json::Value>,
}

/// A frame's identity, already validated by the API layer.
#[derive(Clone, Copy, Debug)]
pub struct FrameRef<'a> {
    /// Session it belongs to.
    pub session_id: &'a str,
    /// Frame id as the field node assigned it, e.g. `light_00042`.
    pub frame_id: &'a str,
    /// Extension of the raw, without the dot, e.g. `cr3`.
    pub ext: &'a str,
}

/// What happened to a frame that was offered to the archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The bytes were written here by this call.
    Stored(SessionCounts),
    /// The archive already held these exact bytes; nothing was rewritten.
    Duplicate(SessionCounts),
    /// The id is taken by different bytes. Nothing was written, nothing was replaced (REL-11).
    Conflict {
        /// The checksum the archive holds under this id.
        stored_sha256: String,
    },
}

/// Anything that stops the archive from accepting a frame.
#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    /// The filesystem refused.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was being attempted.
        action: &'static str,
        /// The path it was attempted on.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// The journal refused. The frame may be on disk; it is not acked, so the field node keeps
    /// its copy and retries (SDD §5.10.2).
    #[error(transparent)]
    Journal(#[from] JournalError),
}

impl MirrorError {
    fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

/// The mirrored session archive on this node's storage volume.
pub struct SessionMirror {
    root: PathBuf,
    journal: Journal,
    /// Serializes the journal-record / `session.json` pair.
    ///
    /// `session.json` is a read-modify-write of derived state, so two concurrent commits to one
    /// session can otherwise write it in the wrong order and leave a stale frame count behind.
    /// One node-wide lock rather than one per session: SDD §5.10.2 puts a single upload in flight
    /// per field node, so this is contended approximately never, and a map of locks would be more
    /// machinery than the contention justifies.
    manifest_lock: tokio::sync::Mutex<()>,
    /// Makes each in-flight temporary name unique — see the module docs.
    nonce: AtomicU64,
}

impl SessionMirror {
    /// Open the archive rooted at `sessions_dir`, with its journal alongside.
    ///
    /// The journal lives *on the archive volume* on purpose: an index that can be mounted
    /// separately from the frames it indexes is an index that will one day describe a different
    /// disk's contents. PRD §8.2 has no key for a state directory, and inventing one is a config
    /// contract change, not an implementation decision.
    ///
    /// # Errors
    /// [`MirrorError`] if the root cannot be created or the journal cannot be opened. Both are
    /// fatal at startup: a stack node that cannot record an ingest cannot honour §5.11's contract,
    /// and starting anyway would mean accepting frames it cannot account for.
    pub async fn open(sessions_dir: &Path) -> Result<Self, MirrorError> {
        tokio::fs::create_dir_all(sessions_dir)
            .await
            .map_err(|e| MirrorError::io("create the session archive at", sessions_dir, e))?;
        let journal = Journal::open(sessions_dir.join("ingest.db")).await?;

        Ok(Self {
            root: sessions_dir.to_owned(),
            journal,
            manifest_lock: tokio::sync::Mutex::new(()),
            nonce: AtomicU64::new(0),
        })
    }

    /// The archive root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The journal, for the read-only status routes.
    #[must_use]
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// What this node already holds under `(session_id, frame_id)`.
    ///
    /// # Errors
    /// [`MirrorError`] if the journal cannot be queried.
    pub async fn lookup(
        &self,
        session_id: &str,
        frame_id: &str,
    ) -> Result<Option<StoredFrame>, MirrorError> {
        Ok(self.journal.lookup(session_id, frame_id).await?)
    }

    /// Delete temporaries left behind by a crash or a killed upload.
    ///
    /// Called once at startup, before the API accepts anything, so it cannot race a live upload.
    /// Without it nothing ever removes them: an ingest that dies between `create` and `rename`
    /// leaks its partial frame, and on an archive volume those add up to a disk-full alert months
    /// later with no explanation attached (REL-12).
    ///
    /// Returns the number of files removed. Never fails the caller: an unreadable session
    /// directory is a reason to log and carry on, not to refuse to start.
    pub async fn sweep_temporaries(&self) -> u64 {
        let mut removed = 0;
        let mut sessions = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(path = %self.root.display(), %error, "cannot scan the archive for leftover temporaries");
                return 0;
            }
        };

        while let Ok(Some(entry)) = sessions.next_entry().await {
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let session = entry.path();
            for dir in [
                session.clone(),
                session.join(FRAMES_DIR),
                session.join(CONTROL_DIR),
            ] {
                removed += remove_temporaries_in(&dir).await;
            }
        }

        removed
    }

    /// Begin streaming a frame into the archive.
    ///
    /// # Errors
    /// [`MirrorError`] if the session directories or the temporary file cannot be created.
    pub async fn stage(&self, frame: FrameRef<'_>, limit: u64) -> Result<Staging, MirrorError> {
        self.ensure_dirs(frame.session_id).await?;

        let nonce = self.nonce.fetch_add(1, Ordering::Relaxed);
        let path = self
            .session_dir(frame.session_id)
            .join(FRAMES_DIR)
            .join(format!(
                "{TMP_PREFIX}{frame_id}.{pid}.{nonce}",
                frame_id = frame.frame_id,
                pid = std::process::id(),
            ));

        let file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| MirrorError::io("create the upload temporary", &path, e))?;

        Ok(Staging {
            path,
            file: BufWriter::with_capacity(WRITE_BUFFER_BYTES, file),
            hasher: Sha256::new(),
            written: 0,
            limit,
        })
    }

    /// Link a verified frame into the archive and record it.
    ///
    /// # Errors
    /// [`MirrorError`] if the frame cannot be linked durably, or the journal cannot record it.
    /// The derived files — the quality sidecar and `session.json` — are *not* in that set: they
    /// are rebuildable from the frames and the journal, and failing an ingest whose bytes are
    /// already durable would ask the field node to re-send 25 MB to fix a metadata write.
    pub async fn commit(
        &self,
        frame: FrameRef<'_>,
        verified: Verified,
        capture: Option<serde_json::Value>,
        session: Option<SessionMeta>,
        source: Option<String>,
    ) -> Result<Outcome, MirrorError> {
        let frames_dir = self.session_dir(frame.session_id).join(FRAMES_DIR);
        let dest = frames_dir.join(format!("{}.{}", frame.frame_id, frame.ext));
        let Verified {
            path: staged,
            sha256,
            size_bytes,
        } = verified;

        let linked = link_no_replace(&staged, &dest).await?;
        match linked {
            Linked::AlreadyPresent => {
                // A frame already sits under this id and the journal did not know: either a crash
                // between the rename and the journal row, or a retry that overlapped its own
                // first attempt. Re-hash what is there and let the bytes decide — adopting a
                // frame whose checksum matches is free, and replacing one whose checksum does not
                // would be the REL-11 violation this whole path exists to prevent.
                let existing = hash_file(&dest).await?;
                remove_quietly(&staged).await;
                if existing != sha256 {
                    return Ok(Outcome::Conflict {
                        stored_sha256: existing,
                    });
                }
            }
            // fsync the *directory*, not just the file: the file's contents were fsynced before
            // the rename, but the name that makes them findable lives here.
            Linked::Stored => fsync_dir(&frames_dir).await?,
        }

        let received_ts = now_millis();
        let rel_path = format!(
            "{}/{}/{}.{}",
            frame.session_id, FRAMES_DIR, frame.frame_id, frame.ext
        );

        self.write_quality(frame, &sha256, size_bytes, capture, received_ts)
            .await;

        // One critical section for "record it, then say so in the manifest", so a second commit
        // cannot interleave and leave `session.json` describing an older count.
        let guard = self.manifest_lock.lock().await;
        let recorded = self
            .journal
            .record(NewFrame {
                session_id: frame.session_id.to_owned(),
                frame_id: frame.frame_id.to_owned(),
                sha256: sha256.clone(),
                size_bytes,
                rel_path,
                source,
            })
            .await?;

        if recorded.stored.sha256 != sha256 {
            // The bytes on disk matched ours but the journal remembers a different frame under
            // this id. Refuse rather than reconcile: the journal is REL-13's authority, and an
            // archive that disagrees with its own index is not something an ingest handler should
            // quietly repair.
            drop(guard);
            return Ok(Outcome::Conflict {
                stored_sha256: recorded.stored.sha256,
            });
        }

        self.write_manifest(frame.session_id, session, recorded.counts)
            .await;
        drop(guard);

        Ok(if linked == Linked::Stored && recorded.inserted {
            Outcome::Stored(recorded.counts)
        } else {
            Outcome::Duplicate(recorded.counts)
        })
    }

    /// `sessions/<session_id>`.
    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    /// Create the session's directories, fsyncing the chain the first time.
    ///
    /// The fsyncs happen only on creation. A directory entry that is not fsynced can vanish in a
    /// power cut with the fsynced frame still inside it — unreachable, and acked.
    async fn ensure_dirs(&self, session_id: &str) -> Result<(), MirrorError> {
        let session = self.session_dir(session_id);
        let fresh = !tokio::fs::try_exists(&session).await.unwrap_or(false);

        for dir in [session.join(FRAMES_DIR), session.join(CONTROL_DIR)] {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| MirrorError::io("create", &dir, e))?;
        }
        if fresh {
            fsync_dir(&session).await?;
            fsync_dir(&self.root).await?;
        }
        Ok(())
    }

    /// Write `control/quality_<id>.json`. Derived state: a failure is logged, never fatal.
    async fn write_quality(
        &self,
        frame: FrameRef<'_>,
        sha256: &str,
        size_bytes: u64,
        capture: Option<serde_json::Value>,
        received_ts: DateTime<Utc>,
    ) {
        let path = self
            .session_dir(frame.session_id)
            .join(CONTROL_DIR)
            .join(quality_file_name(frame.frame_id));
        let quality = Quality {
            v: MIRROR_SCHEMA_VERSION,
            frame_id: frame.frame_id.to_owned(),
            sha256: sha256.to_owned(),
            size_bytes,
            received_ts,
            capture,
        };

        match serde_json::to_vec_pretty(&quality) {
            Ok(bytes) => {
                if let Err(error) = write_atomic(&path, &bytes).await {
                    tracing::warn!(path = %path.display(), %error, "the frame is stored, but its quality sidecar was not written");
                }
            }
            // Unreachable: `Quality` is a plain struct over JSON-representable values.
            Err(error) => tracing::warn!(%error, "cannot serialize a quality sidecar"),
        }
    }

    /// Create or extend `session.json`. Derived state: a failure is logged, never fatal.
    async fn write_manifest(
        &self,
        session_id: &str,
        session: Option<SessionMeta>,
        counts: SessionCounts,
    ) {
        let path = self.session_dir(session_id).join(SESSION_JSON);
        let existing = read_manifest(&path).await;
        let session = session.unwrap_or_default();

        let manifest = Manifest {
            v: MIRROR_SCHEMA_VERSION,
            session_id: session_id.to_owned(),
            // Earliest wins. A session's creation time does not change, and a late arrival
            // (IPP-15) whose metadata says otherwise must not rewrite history.
            created_ts: [
                existing.as_ref().map(|m| m.created_ts),
                session.created_ts,
                Some(counts.first_ingest_ts),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(counts.first_ingest_ts),
            // Latest non-null wins: this is a mirror, so the most recent statement the field node
            // made is the one to reflect — but a frame that carries no session block must not
            // erase what an earlier one supplied.
            target: session
                .target
                .or_else(|| existing.as_ref().and_then(|m| m.target.clone())),
            equipment: session
                .equipment
                .or_else(|| existing.as_ref().and_then(|m| m.equipment.clone())),
            mirror: MirrorStats {
                frame_count: counts.frame_count,
                first_ingest_ts: counts.first_ingest_ts,
                last_ingest_ts: counts.last_ingest_ts,
            },
        };

        match serde_json::to_vec_pretty(&manifest) {
            Ok(bytes) => {
                if let Err(error) = write_atomic(&path, &bytes).await {
                    tracing::warn!(path = %path.display(), %error, "the frame is stored, but the session manifest was not updated");
                }
            }
            // Unreachable, as above.
            Err(error) => tracing::warn!(%error, "cannot serialize a session manifest"),
        }
    }
}

/// `control/quality_<id>.json` for a frame id.
///
/// SDD §5.5 names the frame `light_<id>.cr3` and its sidecar `quality_<id>.json`, so `<id>` is the
/// frame id with its kind prefix removed. The API layer guarantees the `<kind>_<id>` shape, and
/// the fallback keeps a hypothetical prefix-less id addressable rather than panicking on it.
#[must_use]
pub fn quality_file_name(frame_id: &str) -> String {
    let id = frame_id.split_once('_').map_or(frame_id, |(_, id)| id);
    format!("quality_{id}.json")
}

/// A frame being streamed into the archive.
pub struct Staging {
    path: PathBuf,
    file: BufWriter<tokio::fs::File>,
    hasher: Sha256,
    written: u64,
    limit: u64,
}

/// Why a streaming write stopped.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// The body carried more bytes than its metadata declared.
    #[error("the frame body exceeds the {limit} bytes its metadata declares")]
    TooLarge {
        /// The declared size that was exceeded.
        limit: u64,
    },
    /// The filesystem refused.
    #[error(transparent)]
    Mirror(#[from] MirrorError),
}

impl Staging {
    /// Take one chunk from the wire.
    ///
    /// # Errors
    /// [`StageError::TooLarge`] as soon as the declared size is exceeded — the check is per chunk
    /// rather than at the end, so a body that claims 25 MB and keeps sending cannot fill the
    /// archive volume before anyone notices. [`StageError::Mirror`] if the write fails.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<(), StageError> {
        self.written = self.written.saturating_add(chunk.len() as u64);
        if self.written > self.limit {
            return Err(StageError::TooLarge { limit: self.limit });
        }
        self.hasher.update(chunk);
        self.file
            .write_all(chunk)
            .await
            .map_err(|e| MirrorError::io("write", &self.path, e))?;
        Ok(())
    }

    /// Flush, fsync, and hand back what was actually received.
    ///
    /// The temporary is removed on either failure, so "nothing stored, tmp cleaned" holds for a
    /// volume that fills during the final flush and not only for a checksum that disagrees.
    ///
    /// # Errors
    /// [`MirrorError`] if the bytes cannot be made durable — which is exactly the case in which
    /// an ack would be a lie.
    pub async fn finish(self) -> Result<Verified, MirrorError> {
        let Self {
            path,
            mut file,
            hasher,
            written,
            ..
        } = self;

        if let Err(error) = file.flush().await {
            remove_quietly(&path).await;
            return Err(MirrorError::io("flush", &path, error));
        }
        if let Err(error) = file.into_inner().sync_all().await {
            remove_quietly(&path).await;
            return Err(MirrorError::io("fsync", &path, error));
        }

        Ok(Verified {
            path,
            sha256: hex(&hasher.finalize()),
            size_bytes: written,
        })
    }

    /// Abandon the upload and remove the temporary.
    pub async fn abort(self) {
        let path = self.path;
        drop(self.file);
        remove_quietly(&path).await;
    }
}

/// A frame on disk, fsynced, with the checksum of what was actually written.
pub struct Verified {
    path: PathBuf,
    sha256: String,
    size_bytes: u64,
}

impl Verified {
    /// Lowercase hex SHA-256 of the bytes received.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Bytes received.
    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// Throw the frame away — the checksum did not match, or the archive already had it.
    pub async fn discard(self) {
        remove_quietly(&self.path).await;
    }
}

/// Whether [`link_no_replace`] put the file there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Linked {
    Stored,
    AlreadyPresent,
}

/// `renameat2(RENAME_NOREPLACE)` — see the module docs for why not `rename`.
async fn link_no_replace(from: &Path, to: &Path) -> Result<Linked, MirrorError> {
    let (from_owned, to_owned) = (from.to_owned(), to.to_owned());
    let result = tokio::task::spawn_blocking(move || {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            &from_owned,
            rustix::fs::CWD,
            &to_owned,
            rustix::fs::RenameFlags::NOREPLACE,
        )
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(Linked::Stored),
        Ok(Err(rustix::io::Errno::EXIST)) => Ok(Linked::AlreadyPresent),
        Ok(Err(errno)) => Err(MirrorError::io(
            "link into the archive",
            to,
            io::Error::from(errno),
        )),
        Err(error) => Err(MirrorError::io(
            "link into the archive",
            to,
            io::Error::other(error),
        )),
    }
}

/// SHA-256 of a file already in the archive, streamed rather than read whole.
async fn hash_file(path: &Path) -> Result<String, MirrorError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| MirrorError::io("open", path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|e| MirrorError::io("read", path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// fsync a directory, so a rename into it survives a power cut.
async fn fsync_dir(dir: &Path) -> Result<(), MirrorError> {
    tokio::fs::File::open(dir)
        .await
        .map_err(|e| MirrorError::io("open for fsync", dir, e))?
        .sync_all()
        .await
        .map_err(|e| MirrorError::io("fsync", dir, e))
}

/// Write derived metadata with the same tmp-fsync-rename discipline as a frame (SDD §5.5).
///
/// A plain `rename` here, unlike a frame's: these files are derived from the frames and the
/// journal, and replacing one with a newer version is the intended operation.
async fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", name.to_string_lossy()));

    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);

    if let Err(error) = tokio::fs::rename(&tmp, path).await {
        remove_quietly(&tmp).await;
        return Err(error);
    }
    tokio::fs::File::open(dir).await?.sync_all().await
}

/// Read `session.json`, treating anything unreadable as absent.
///
/// A corrupt manifest must not fail an ingest whose frame is already durable: the manifest is
/// rebuildable from the journal, and the frame is not rebuildable from anything.
async fn read_manifest(path: &Path) -> Option<Manifest> {
    let bytes = tokio::fs::read(path).await.ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(manifest) => Some(manifest),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the session manifest is unreadable and will be rewritten from the journal");
            None
        }
    }
}

/// Remove every `.tmp_*` file directly in `dir`; returns how many went.
async fn remove_temporaries_in(dir: &Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_temporary = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(TMP_PREFIX));
        if is_temporary && tokio::fs::remove_file(&path).await.is_ok() {
            tracing::info!(path = %path.display(), "removed a temporary left by an interrupted upload");
            removed += 1;
        }
    }
    removed
}

/// Best-effort removal: the caller is already on a failure path and has nothing better to do
/// with a second error than log it.
async fn remove_quietly(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        if error.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), %error, "cannot remove a temporary");
        }
    }
}

/// Lowercase hex, the spelling `frame.saved` already uses (SDD §4.3).
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` cannot fail; the result is discarded rather than unwrapped so a
        // formatting quirk can never take the node down mid-ingest.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use chrono::SubsecRound as _;

    /// The layout SDD §5.5 fixes, as `astroctl-session` will assert it from the other side.
    /// Included by path rather than through a crate dependency: it is a *fixture*, and the
    /// dependency matrix of ADD §5.6 should not acquire an edge to carry one.
    const LAYOUT_FIXTURE: &str = include_str!("../../astroctl-session/testdata/session-layout.txt");

    async fn mirror(dir: &TempDir) -> SessionMirror {
        SessionMirror::open(&dir.path().join("sessions"))
            .await
            .expect("a fresh archive opens")
    }

    async fn store(mirror: &SessionMirror, session: &str, frame_id: &str, body: &[u8]) -> Outcome {
        let frame = FrameRef {
            session_id: session,
            frame_id,
            ext: "cr3",
        };
        let mut staging = mirror
            .stage(frame, body.len() as u64)
            .await
            .expect("stages");
        staging.write(body).await.expect("writes");
        let verified = staging.finish().await.expect("fsyncs");
        mirror
            .commit(frame, verified, None, None, Some("test".to_owned()))
            .await
            .expect("commits")
    }

    #[tokio::test]
    async fn a_stored_frame_lands_at_the_sdd_5_5_path_with_its_bytes_intact() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;

        let outcome = store(&mirror, "2026-07-29_ngc7000", "light_00042", b"raw bytes").await;

        assert!(matches!(outcome, Outcome::Stored(counts) if counts.frame_count == 1));
        let frame = mirror
            .root()
            .join("2026-07-29_ngc7000/frames/light_00042.cr3");
        assert_eq!(tokio::fs::read(&frame).await.unwrap(), b"raw bytes");
    }

    /// Acceptance criterion 4: the mirror is structurally the field node's layout. The *content*
    /// of `session.json` differs by design — SDD §5.11.3 constructs it from ingest metadata
    /// rather than copying it — so the fixture fixes the paths, which is what must not drift.
    #[tokio::test]
    async fn the_layout_matches_the_fixture_shared_with_astroctl_session() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;
        store(&mirror, "S", "light_00042", b"raw").await;

        let expected: Vec<&str> = LAYOUT_FIXTURE
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        for relative in expected {
            let path = mirror.root().join("S").join(relative);
            assert!(
                tokio::fs::try_exists(&path).await.unwrap_or(false),
                "the layout fixture requires {relative}, which the mirror did not produce"
            );
        }
    }

    #[tokio::test]
    async fn the_manifest_records_the_session_and_extends_with_each_frame() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;
        let frame = FrameRef {
            session_id: "S",
            frame_id: "light_00001",
            ext: "cr3",
        };
        let created = now_millis() - chrono::Duration::hours(3);

        let mut staging = mirror.stage(frame, 3).await.unwrap();
        staging.write(b"raw").await.unwrap();
        let verified = staging.finish().await.unwrap();
        mirror
            .commit(
                frame,
                verified,
                Some(serde_json::json!({"exposure_s": 120})),
                Some(SessionMeta {
                    target: Some(serde_json::json!({"name": "NGC 7000"})),
                    equipment: Some(serde_json::json!({"telescope": "SW 200PDS"})),
                    created_ts: Some(created),
                }),
                None,
            )
            .await
            .unwrap();

        let manifest: Manifest = serde_json::from_slice(
            &tokio::fs::read(mirror.root().join("S").join(SESSION_JSON))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.v, 1);
        assert_eq!(manifest.session_id, "S");
        assert_eq!(manifest.created_ts, created.trunc_subsecs(3));
        assert_eq!(
            manifest.target,
            Some(serde_json::json!({"name": "NGC 7000"}))
        );
        assert_eq!(manifest.mirror.frame_count, 1);

        // A second frame with no session block extends the counts and keeps what the first said.
        store(&mirror, "S", "light_00002", b"more").await;
        let manifest: Manifest = serde_json::from_slice(
            &tokio::fs::read(mirror.root().join("S").join(SESSION_JSON))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.mirror.frame_count, 2);
        assert_eq!(manifest.created_ts, created.trunc_subsecs(3));
        assert_eq!(
            manifest.target,
            Some(serde_json::json!({"name": "NGC 7000"}))
        );

        let quality: Quality = serde_json::from_slice(
            &tokio::fs::read(mirror.root().join("S/control/quality_00001.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(quality.frame_id, "light_00001");
        assert_eq!(quality.size_bytes, 3);
        assert_eq!(
            quality.capture,
            Some(serde_json::json!({"exposure_s": 120}))
        );
    }

    /// REL-11: the same id with different bytes is refused, and the stored frame is untouched.
    #[tokio::test]
    async fn a_reused_frame_id_with_different_bytes_never_replaces_the_stored_frame() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;
        store(&mirror, "S", "light_00001", b"original").await;

        let outcome = store(&mirror, "S", "light_00001", b"different").await;

        assert!(matches!(outcome, Outcome::Conflict { .. }), "{outcome:?}");
        assert_eq!(
            tokio::fs::read(mirror.root().join("S/frames/light_00001.cr3"))
                .await
                .unwrap(),
            b"original"
        );
    }

    /// The crash window between the rename and the journal row: the frame is on disk, nothing
    /// recorded it. A retry must adopt it, not refuse it and not rewrite it.
    #[tokio::test]
    async fn an_unrecorded_frame_already_on_disk_is_adopted_by_the_retry() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;
        let frames = mirror.root().join("S").join(FRAMES_DIR);
        tokio::fs::create_dir_all(&frames).await.unwrap();
        tokio::fs::write(frames.join("light_00001.cr3"), b"orphan")
            .await
            .unwrap();

        let outcome = store(&mirror, "S", "light_00001", b"orphan").await;

        assert!(
            matches!(outcome, Outcome::Duplicate(c) if c.frame_count == 1),
            "{outcome:?}"
        );
        assert_eq!(
            mirror
                .lookup("S", "light_00001")
                .await
                .unwrap()
                .expect("the retry recorded it")
                .sha256,
            hash_file(&frames.join("light_00001.cr3")).await.unwrap()
        );
    }

    #[tokio::test]
    async fn a_write_beyond_the_declared_size_is_refused_before_the_disk_fills() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;
        let mut staging = mirror
            .stage(
                FrameRef {
                    session_id: "S",
                    frame_id: "light_00001",
                    ext: "cr3",
                },
                4,
            )
            .await
            .unwrap();

        assert!(staging.write(b"1234").await.is_ok());
        let error = staging
            .write(b"5")
            .await
            .expect_err("the limit is enforced");
        assert!(
            matches!(error, StageError::TooLarge { limit: 4 }),
            "{error}"
        );
        staging.abort().await;
    }

    #[tokio::test]
    async fn aborting_leaves_nothing_behind() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;
        let frame = FrameRef {
            session_id: "S",
            frame_id: "light_00001",
            ext: "cr3",
        };
        let mut staging = mirror.stage(frame, 64).await.unwrap();
        staging.write(b"partial").await.unwrap();
        staging.abort().await;

        let frames = mirror.root().join("S").join(FRAMES_DIR);
        let mut entries = tokio::fs::read_dir(&frames).await.unwrap();
        assert!(
            entries.next_entry().await.unwrap().is_none(),
            "the frames directory must be empty"
        );
    }

    #[tokio::test]
    async fn the_startup_sweep_removes_temporaries_a_crash_left_behind() {
        let dir = TempDir::new();
        let mirror = mirror(&dir).await;
        store(&mirror, "S", "light_00001", b"raw").await;

        let frames = mirror.root().join("S").join(FRAMES_DIR);
        tokio::fs::write(frames.join(".tmp_light_00002.1.0"), b"half a frame")
            .await
            .unwrap();
        tokio::fs::write(mirror.root().join("S/.tmp_session.json"), b"{}")
            .await
            .unwrap();

        assert_eq!(mirror.sweep_temporaries().await, 2);
        assert!(tokio::fs::try_exists(frames.join("light_00001.cr3"))
            .await
            .unwrap(),);
    }

    #[test]
    fn the_quality_sidecar_drops_the_kind_prefix_the_way_sdd_5_5_writes_it() {
        assert_eq!(quality_file_name("light_00042"), "quality_00042.json");
        assert_eq!(quality_file_name("dark_00007"), "quality_00007.json");
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
