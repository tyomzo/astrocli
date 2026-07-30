//! The ingest journal — SDD §5.11.3, ADR-06.
//!
//! One SQLite database recording every frame this node has accepted: what it was, when it
//! arrived, and who sent it. SDD §5.11.3 calls it "the future authority for REL-13 reclaim
//! decisions", and that is the sentence that decides every trade-off in this module. A field
//! node deletes its only copy of a frame because this database said the stack node has it. It
//! may therefore *under*-claim — a row missing after a crash costs one retransmission — but it
//! must never over-claim. Two consequences, both load-bearing:
//!
//! * the row is written **after** the frame is fsynced and linked into place (§5.11.2), never
//!   before, so a crash between the two leaves a frame with no row rather than a row with no
//!   frame;
//! * `synchronous = FULL` under WAL, so the row is on the platter before the ack that quotes it
//!   leaves the node. At one frame per exposure the extra fsync is unmeasurable, and the
//!   alternative is a journal that can lose its last minute of commits to a power cut — exactly
//!   the window in which the field node was told it could reclaim.
//!
//! # Key
//!
//! The primary key is `(session_id, frame_id)`, **not** `frame_id` alone. SDD §5.10.1 gives the
//! field node's queue a bare `frame_id TEXT PRIMARY KEY`, which holds there only because that
//! journal never outlives one node's numbering: §5.5 hands out frame ids from a per-session
//! counter, so `light_00042` recurs in every session. On the stack node, which mirrors every
//! session from every field node, a bare `frame_id` key would make the second session's frame 42
//! a duplicate of the first's. §5.11.2's "if `(frame_id, sha256)` is already stored" has the same
//! gap; it is read here as scoped to the session, because no other reading is implementable.
//!
//! # Blocking
//!
//! `rusqlite` is synchronous, so every statement runs inside [`tokio::task::spawn_blocking`]. The
//! `Mutex` is what makes the connection `Sync` and simultaneously enforces ADD §10's single-writer
//! discipline; it is only ever locked *inside* the blocking closure, never across an `.await`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension as _};

/// Schema version, kept in SQLite's own `user_version`.
///
/// A database written by a newer build is refused at startup rather than migrated silently: the
/// reclaim authority is the last place to guess at what an unfamiliar column means.
const SCHEMA_VERSION: i64 = 1;

/// How long a statement waits for another writer before giving up.
///
/// Only ingest writes today. The timeout is for the 2b rebuild manager, which reads this database
/// as its work list — a rebuild scanning while a frame lands must wait, not fail.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// Anything that stops the journal from answering.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// SQLite refused a statement, or the file is not usable.
    #[error("ingest journal at {path}: {source}")]
    Sqlite {
        /// The database this failed on.
        path: PathBuf,
        /// What SQLite said.
        source: rusqlite::Error,
    },
    /// The blocking task carrying the statement panicked or was cancelled.
    #[error("the ingest journal task did not complete: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// The file on disk was written by a different schema version.
    #[error(
        "ingest journal at {path} has schema version {found}, but this build understands \
         {SCHEMA_VERSION} — it was written by a different version of astroctl"
    )]
    Schema {
        /// The database that was refused.
        path: PathBuf,
        /// Its `user_version`.
        found: i64,
    },
    /// A stored value could not be read back as the type its column promises.
    #[error(
        "ingest journal at {path}: column {column} holds {value:?}, which is not a {expected}"
    )]
    Corrupt {
        /// The database that was refused.
        path: PathBuf,
        /// Which column.
        column: &'static str,
        /// What was in it.
        value: String,
        /// What was expected.
        expected: &'static str,
    },
}

/// One frame as the journal holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFrame {
    /// Lowercase hex SHA-256 of the stored bytes.
    pub sha256: String,
    /// Size of the stored frame.
    pub size_bytes: u64,
    /// Path of the frame relative to `storage.sessions_dir`.
    pub rel_path: String,
    /// Who uploaded it, as recorded at ingest.
    pub source: Option<String>,
    /// When this node accepted it.
    pub received_ts: DateTime<Utc>,
}

/// A frame to record, once its bytes are durable.
#[derive(Clone, Debug)]
pub struct NewFrame {
    /// Session this frame belongs to.
    pub session_id: String,
    /// Frame id as the field node assigned it.
    pub frame_id: String,
    /// Lowercase hex SHA-256, verified against the bytes on disk.
    pub sha256: String,
    /// Size of the stored frame.
    pub size_bytes: u64,
    /// Path relative to `storage.sessions_dir`.
    pub rel_path: String,
    /// Who uploaded it — the peer address, or `None` when it cannot be determined.
    pub source: Option<String>,
}

/// What one session looks like after a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionCounts {
    /// Frames this node holds for the session.
    pub frame_count: u64,
    /// First arrival — not the session's creation time, which only the field node knows.
    pub first_ingest_ts: DateTime<Utc>,
    /// Most recent arrival.
    pub last_ingest_ts: DateTime<Utc>,
}

/// The outcome of recording a frame.
///
/// `stored` is the row the journal *now* holds, whether this call wrote it or found it. The
/// caller compares its own checksum against that row; the journal deliberately does not, because
/// "the archive disagrees with itself" is an answer the HTTP layer has a code for and this layer
/// does not.
#[derive(Clone, Debug)]
pub struct Recorded {
    /// Whether this call inserted the row (`false` means it was already there).
    pub inserted: bool,
    /// The authoritative row.
    pub stored: StoredFrame,
    /// The session's totals, read in the same transaction as the insert.
    pub counts: SessionCounts,
}

/// Summary for `/api/stacking/stats` (SDD §5.11.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestSession {
    /// The session that most recently received a frame.
    pub session_id: String,
    /// Frames held for it.
    pub frame_count: u64,
    /// When the last of them arrived.
    pub last_ingest_ts: DateTime<Utc>,
}

/// Why a connection could not be brought up, before there is a [`Journal`] to blame it on.
enum OpenFailure {
    Sqlite(rusqlite::Error),
    Schema(i64),
}

impl From<rusqlite::Error> for OpenFailure {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// The ingest journal.
#[derive(Debug)]
pub struct Journal {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl Journal {
    /// Open (creating if absent) and bring the schema up.
    ///
    /// # Errors
    /// [`JournalError`] if the file cannot be opened, the pragmas cannot be set, or the schema
    /// was written by a different version of this program.
    pub async fn open(path: PathBuf) -> Result<Self, JournalError> {
        let for_task = path.clone();
        match tokio::task::spawn_blocking(move || Self::open_blocking(&for_task)).await? {
            Ok(conn) => Ok(Self {
                conn: Arc::new(Mutex::new(conn)),
                path,
            }),
            Err(OpenFailure::Sqlite(source)) => Err(JournalError::Sqlite { path, source }),
            Err(OpenFailure::Schema(found)) => Err(JournalError::Schema { path, found }),
        }
    }

    /// Everything that must happen on a fresh connection, on the blocking pool.
    fn open_blocking(path: &Path) -> Result<Connection, OpenFailure> {
        let conn = Connection::open(path)?;
        // WAL so a 2b rebuild can read the work list while a frame is landing; FULL because the
        // ack that follows a commit is a durability claim (module docs).
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;

        // Read the version before touching the schema: `CREATE TABLE IF NOT EXISTS` against a
        // future layout succeeds silently and leaves the mismatch to be discovered by a wrong
        // answer later.
        let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(OpenFailure::Schema(found));
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS frames (
                 session_id  TEXT    NOT NULL,
                 frame_id    TEXT    NOT NULL,
                 sha256      TEXT    NOT NULL,
                 size_bytes  INTEGER NOT NULL,
                 rel_path    TEXT    NOT NULL,
                 source      TEXT,
                 received_ts TEXT    NOT NULL,
                 PRIMARY KEY (session_id, frame_id)
             );
             CREATE INDEX IF NOT EXISTS frames_by_arrival ON frames (received_ts);",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(conn)
    }

    /// Run one statement batch on the blocking pool.
    async fn with_conn<T, F>(&self, f: F) -> Result<T, JournalError>
    where
        F: FnOnce(&mut Connection) -> Result<T, rusqlite::Error> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            // A poisoned mutex means an earlier closure panicked while holding the connection.
            // The connection itself is still sound — an interrupted transaction rolls back when
            // its guard drops — and refusing every subsequent ingest over a past panic would turn
            // one lost frame into a lost night.
            let mut guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut guard).map_err(|source| JournalError::from_sqlite(&path, source))
        })
        .await?
    }

    /// What this node already holds for `(session_id, frame_id)`, if anything.
    ///
    /// # Errors
    /// [`JournalError`] if the query fails or a stored timestamp is unreadable.
    pub async fn lookup(
        &self,
        session_id: &str,
        frame_id: &str,
    ) -> Result<Option<StoredFrame>, JournalError> {
        let (session_id, frame_id) = (session_id.to_owned(), frame_id.to_owned());
        let path = self.path.clone();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT sha256, size_bytes, rel_path, source, received_ts
                       FROM frames WHERE session_id = ?1 AND frame_id = ?2",
                    params![session_id, frame_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()
            })
            .await?;

        row.map(|row| stored_frame(&path, row)).transpose()
    }

    /// Record a frame whose bytes are already durable, and read back the session's totals.
    ///
    /// Idempotent: a second call for the same `(session_id, frame_id)` inserts nothing and
    /// reports the row that is already there. That is what makes the field agent's blind retry
    /// safe (SDD §5.10.3) even when two retries overlap.
    ///
    /// # Errors
    /// [`JournalError`] if the transaction fails or a stored value is unreadable.
    pub async fn record(&self, frame: NewFrame) -> Result<Recorded, JournalError> {
        let path = self.path.clone();
        let received_ts = astroctl_core::event::now_millis();
        let ts_text = format_ts(received_ts);
        let size = i64::try_from(frame.size_bytes).unwrap_or(i64::MAX);

        let (inserted, stored, counts) = self
            .with_conn(move |conn| {
                let tx = conn.transaction()?;
                let inserted = tx.execute(
                    "INSERT INTO frames
                         (session_id, frame_id, sha256, size_bytes, rel_path, source, received_ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (session_id, frame_id) DO NOTHING",
                    params![
                        frame.session_id,
                        frame.frame_id,
                        frame.sha256,
                        size,
                        frame.rel_path,
                        frame.source,
                        ts_text,
                    ],
                )? == 1;

                // Read the row back rather than echoing what was passed in: on the conflict path
                // the stored row is a different frame's, and that difference is the whole point.
                let stored = tx.query_row(
                    "SELECT sha256, size_bytes, rel_path, source, received_ts
                       FROM frames WHERE session_id = ?1 AND frame_id = ?2",
                    params![frame.session_id, frame.frame_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )?;

                let counts = tx.query_row(
                    "SELECT COUNT(*), MIN(received_ts), MAX(received_ts)
                       FROM frames WHERE session_id = ?1",
                    params![frame.session_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?;

                tx.commit()?;
                Ok((inserted, stored, counts))
            })
            .await?;

        Ok(Recorded {
            inserted,
            stored: stored_frame(&path, stored)?,
            counts: SessionCounts {
                frame_count: u64::try_from(counts.0).unwrap_or(0),
                first_ingest_ts: parse_ts(&path, "received_ts", &counts.1)?,
                last_ingest_ts: parse_ts(&path, "received_ts", &counts.2)?,
            },
        })
    }

    /// The session that most recently received a frame, or `None` on a node that has ingested
    /// nothing.
    ///
    /// "Most recent arrival" rather than "most recent session id": a late-arriving frame from a
    /// session that ended yesterday (IPP-15) makes that session the one the operator is watching
    /// land, which is what `/api/stacking/stats` is for.
    ///
    /// Ordered by insertion, not by `received_ts`, for two reasons. Timestamps are millisecond
    /// resolution (SDD §2), so two sessions gaining a frame in the same millisecond tie and the
    /// answer becomes whichever row SQLite happens to reach first. And REL-14 admits an
    /// undisciplined clock: a step backwards would make an older arrival look newer, which
    /// insertion order cannot do. Nothing deletes rows in this increment, so `rowid` only ever
    /// grows.
    ///
    /// # Errors
    /// [`JournalError`] if the query fails or a stored timestamp is unreadable.
    pub async fn latest_session(&self) -> Result<Option<LatestSession>, JournalError> {
        let path = self.path.clone();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT session_id, COUNT(*), MAX(received_ts)
                       FROM frames
                      GROUP BY session_id
                      ORDER BY MAX(rowid) DESC
                      LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
            })
            .await?;

        row.map(|(session_id, count, last)| {
            Ok(LatestSession {
                session_id,
                frame_count: u64::try_from(count).unwrap_or(0),
                last_ingest_ts: parse_ts(&path, "received_ts", &last)?,
            })
        })
        .transpose()
    }
}

impl JournalError {
    fn from_sqlite(path: &Path, source: rusqlite::Error) -> Self {
        Self::Sqlite {
            path: path.to_owned(),
            source,
        }
    }
}

/// SDD §2: every persisted timestamp is UTC RFC 3339 with milliseconds. The same spelling the
/// event schema uses, so a journal row and an event about it are comparable as text.
fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_ts(path: &Path, column: &'static str, text: &str) -> Result<DateTime<Utc>, JournalError> {
    DateTime::parse_from_rfc3339(text)
        .map(|ts| ts.with_timezone(&Utc))
        .map_err(|_| JournalError::Corrupt {
            path: path.to_owned(),
            column,
            value: text.to_owned(),
            expected: "RFC 3339 timestamp",
        })
}

/// One row as the two `SELECT`s above spell it.
type FrameRow = (String, i64, String, Option<String>, String);

fn stored_frame(
    path: &Path,
    (sha256, size, rel_path, source, ts): FrameRow,
) -> Result<StoredFrame, JournalError> {
    Ok(StoredFrame {
        sha256,
        size_bytes: u64::try_from(size).map_err(|_| JournalError::Corrupt {
            path: path.to_owned(),
            column: "size_bytes",
            value: size.to_string(),
            expected: "non-negative size",
        })?,
        rel_path,
        source,
        received_ts: parse_ts(path, "received_ts", &ts)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn frame(session: &str, id: &str, sha: &str) -> NewFrame {
        NewFrame {
            session_id: session.to_owned(),
            frame_id: id.to_owned(),
            sha256: sha.to_owned(),
            size_bytes: 25 * 1024 * 1024,
            rel_path: format!("{session}/frames/{id}.cr3"),
            source: Some("10.8.0.2:51820".to_owned()),
        }
    }

    async fn journal(dir: &TempDir) -> Journal {
        Journal::open(dir.path().join("ingest.db"))
            .await
            .expect("a fresh journal opens")
    }

    #[tokio::test]
    async fn a_fresh_journal_holds_nothing() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;
        assert_eq!(journal.lookup("s1", "light_00001").await.unwrap(), None);
        assert_eq!(journal.latest_session().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_recorded_frame_is_found_again_and_counted() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;

        let recorded = journal
            .record(frame("s1", "light_00001", "aa"))
            .await
            .unwrap();
        assert!(recorded.inserted);
        assert_eq!(recorded.counts.frame_count, 1);
        assert_eq!(
            recorded.counts.first_ingest_ts,
            recorded.counts.last_ingest_ts
        );

        let found = journal.lookup("s1", "light_00001").await.unwrap().unwrap();
        assert_eq!(found.sha256, "aa");
        assert_eq!(found.rel_path, "s1/frames/light_00001.cr3");
        assert_eq!(found.received_ts, recorded.stored.received_ts);
        // The task's "received frames, timestamps, source" — the column round-trips, so REL-13's
        // future authority can say where a frame came from and not only that it arrived.
        assert_eq!(found.source.as_deref(), Some("10.8.0.2:51820"));
    }

    /// SDD §5.10.3's blind retry: the second record must not insert, must not fail, and must
    /// report the row already there.
    #[tokio::test]
    async fn recording_the_same_frame_twice_inserts_once() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;

        let first = journal
            .record(frame("s1", "light_00001", "aa"))
            .await
            .unwrap();
        let second = journal
            .record(frame("s1", "light_00001", "aa"))
            .await
            .unwrap();

        assert!(first.inserted);
        assert!(!second.inserted);
        assert_eq!(second.counts.frame_count, 1);
        assert_eq!(second.stored.received_ts, first.stored.received_ts);
    }

    /// The conflict case: the journal reports the stored row unchanged and leaves the verdict to
    /// the caller.
    #[tokio::test]
    async fn a_reused_frame_id_never_overwrites_the_recorded_checksum() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;

        journal
            .record(frame("s1", "light_00001", "aa"))
            .await
            .unwrap();
        let clash = journal
            .record(frame("s1", "light_00001", "bb"))
            .await
            .unwrap();

        assert!(!clash.inserted);
        assert_eq!(
            clash.stored.sha256, "aa",
            "the first frame's checksum stands"
        );
    }

    /// The reason the key is `(session_id, frame_id)` and not `frame_id`: per-session counters
    /// (SDD §5.5) mean every session has a `light_00001`.
    #[tokio::test]
    async fn the_same_frame_id_in_two_sessions_is_two_frames() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;

        journal
            .record(frame("s1", "light_00001", "aa"))
            .await
            .unwrap();
        let other = journal
            .record(frame("s2", "light_00001", "bb"))
            .await
            .unwrap();

        assert!(other.inserted);
        assert_eq!(other.counts.frame_count, 1, "counts are per session");
        assert_eq!(
            journal
                .lookup("s1", "light_00001")
                .await
                .unwrap()
                .unwrap()
                .sha256,
            "aa"
        );
    }

    #[tokio::test]
    async fn the_latest_session_is_the_one_that_last_received_a_frame() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;

        journal
            .record(frame("s1", "light_00001", "aa"))
            .await
            .unwrap();
        journal
            .record(frame("s1", "light_00002", "bb"))
            .await
            .unwrap();
        journal
            .record(frame("s2", "light_00001", "cc"))
            .await
            .unwrap();

        let latest = journal.latest_session().await.unwrap().unwrap();
        assert_eq!(latest.session_id, "s2");
        assert_eq!(latest.frame_count, 1);

        // …and a late arrival for the older session (IPP-15) moves the answer back to it.
        journal
            .record(frame("s1", "light_00003", "dd"))
            .await
            .unwrap();
        let latest = journal.latest_session().await.unwrap().unwrap();
        assert_eq!(latest.session_id, "s1");
        assert_eq!(latest.frame_count, 3);
    }

    /// REL-13's authority has to survive a restart, or it is not an authority.
    #[tokio::test]
    async fn rows_survive_reopening_the_database() {
        let dir = TempDir::new();
        {
            let journal = journal(&dir).await;
            journal
                .record(frame("s1", "light_00001", "aa"))
                .await
                .unwrap();
        }
        let reopened = journal(&dir).await;
        assert_eq!(
            reopened
                .lookup("s1", "light_00001")
                .await
                .unwrap()
                .unwrap()
                .sha256,
            "aa"
        );
    }

    #[tokio::test]
    async fn a_database_from_a_newer_build_is_refused_rather_than_guessed_at() {
        let dir = TempDir::new();
        let path = dir.path().join("ingest.db");
        {
            let journal = journal(&dir).await;
            journal
                .with_conn(|conn| conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1))
                .await
                .unwrap();
        }
        let error = Journal::open(path)
            .await
            .expect_err("a future schema is refused");
        assert!(
            matches!(error, JournalError::Schema { found, .. } if found == SCHEMA_VERSION + 1),
            "{error}"
        );
    }
}
