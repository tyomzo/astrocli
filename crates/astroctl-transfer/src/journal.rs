//! The transfer journal — SDD §5.10.1, ADR-06.
//!
//! One SQLite database, `<queue_dir>/transfer.db`, holding one row per frame this node owes the
//! stacking server. Frames are **referenced, never copied**: `path` points into the session
//! directory, so `queue_dir` holds the journal and nothing else. That keeps the write-once frame
//! the single copy on the field node (REL-11) and makes enqueue O(1) regardless of frame size.
//!
//! # What a lost row costs, and therefore why `synchronous = FULL`
//!
//! The two directions are not symmetric, and the asymmetry is what picks the pragma.
//!
//! * A lost **`acked`** row is cheap: the frame is offered again after the restart and the stack
//!   node dedups it (§5.11.2), costing one retransmission.
//! * A lost **`queued`** row is a frame that is never uploaded at all. Nothing rescans the session
//!   directory in this increment, so the row *is* the only record that the frame is owed. It would
//!   sit on the field node's SD card, durable and unarchived, with no symptom until someone
//!   counted frames on the other end.
//!
//! So the enqueue must be on the platter before the agent forgets about it, which is
//! `synchronous = FULL` under WAL — the same choice M1-T12 made for `ingest.db` and for the
//! mirror-image reason (there, an ack is a durability claim; here, an enqueue is a delivery
//! promise). At one exposure every thirty seconds the extra fsync is unmeasurable.
//!
//! # Key
//!
//! The primary key is `(session_id, frame_id)`. §5.10.1 originally wrote `frame_id TEXT PRIMARY
//! KEY`, which is wrong for the same reason §5.11.2 note 1 gives: §5.5 hands out frame ids from a
//! per-session counter, so `light_00042` recurs in every session, and a bare `frame_id` key would
//! make the second session's frame 42 collide with the first's. On this side the collision would
//! not even produce a refusal — `INSERT … ON CONFLICT DO NOTHING` would silently drop the enqueue
//! and the frame would never be sent. The SDD has since been corrected; this module implements the
//! corrected key.
//!
//! # Blocking
//!
//! `rusqlite` is synchronous, so every statement runs inside [`tokio::task::spawn_blocking`]. The
//! `Mutex` is what makes the connection `Sync` and simultaneously enforces §5.10.1's single-writer
//! discipline; it is only ever locked *inside* the blocking closure, never across an `.await`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension as _};

/// Schema version, kept in SQLite's own `user_version`.
///
/// A database written by a newer build is refused at startup rather than migrated silently: a
/// queue whose columns this build misreads is a queue that can drop a frame.
const SCHEMA_VERSION: i64 = 1;

/// How long a statement waits for another writer before giving up.
///
/// One task writes and the status route reads, so contention is between a statement and a
/// checkpoint rather than between two writers. Five seconds is the same figure the ingest journal
/// uses; a queue that cannot get a lock in five seconds has a disk problem, not a lock problem.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// Where a frame is in the state machine of SDD §5.10.1.
///
/// `queued → uploading → acked`, with `uploading → queued` on any failure. `failed` is terminal
/// and is reached only when the stack node returns a verdict *about this frame* — see
/// [`crate::upload::Refusal`] for which answers qualify and why the list is shorter than
/// §5.10.1's "any 4xx that is not 408/429".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Owed to the stack node, waiting its turn.
    Queued,
    /// The upload is in flight right now.
    Uploading,
    /// The stack node holds the bytes and its echoed checksum matched (§5.10.2).
    Acked,
    /// Definitively refused. Terminal; requires operator action (§5.10.1).
    Failed,
}

impl State {
    /// The spelling stored in the `state` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Uploading => "uploading",
            Self::Acked => "acked",
            Self::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "queued" => Some(Self::Queued),
            "uploading" => Some(Self::Uploading),
            "acked" => Some(Self::Acked),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Anything that stops the journal from answering.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// SQLite refused a statement, or the file is not usable.
    #[error("transfer journal at {path}: {source}")]
    Sqlite {
        /// The database this failed on.
        path: PathBuf,
        /// What SQLite said.
        source: rusqlite::Error,
    },
    /// The blocking task carrying the statement panicked or was cancelled.
    #[error("the transfer journal task did not complete: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// The file on disk was written by a different schema version.
    #[error(
        "transfer journal at {path} has schema version {found}, but this build understands \
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
        "transfer journal at {path}: column {column} holds {value:?}, which is not a {expected}"
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

/// A frame to enqueue, as `frame.saved` describes it plus the session it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewEntry {
    /// Session directory name — `2026-07-29_ngc7000`.
    pub session_id: String,
    /// Frame id as the frame store assigned it — `light_00042` (§5.5).
    pub frame_id: String,
    /// Absolute path of the frame in the session directory. Never copied into `queue_dir`.
    pub path: PathBuf,
    /// Lowercase hex SHA-256 as the frame store computed it.
    pub sha256: String,
    /// Size in bytes at the moment it was saved.
    pub size_bytes: u64,
}

/// One row of the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Session directory name.
    pub session_id: String,
    /// Frame id.
    pub frame_id: String,
    /// Absolute path of the frame.
    pub path: PathBuf,
    /// Lowercase hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Where it is in the state machine.
    pub state: State,
    /// How many upload attempts it has cost so far.
    pub attempts: u32,
    /// When it was enqueued.
    pub queued_ts: DateTime<Utc>,
    /// When the ack arrived, once one has.
    pub acked_ts: Option<DateTime<Utc>>,
    /// REL-13 marking: the archive of record holds this frame. Marking only — nothing deletes.
    pub reclaimable: bool,
    /// Why the last attempt did not succeed, for a `failed` row an operator has to act on.
    pub last_error: Option<String>,
}

/// What `/api/transfer/status` and the `transfer.status` event are built from (§5.10.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Rows in `queued` **or** `uploading` — everything this node still owes the archive.
    pub depth: u64,
    /// When the oldest owed frame was enqueued, if any is.
    pub oldest_queued_ts: Option<DateTime<Utc>>,
    /// When the most recent ack landed, if any has.
    pub last_ack_ts: Option<DateTime<Utc>>,
    /// Attempts already spent on the frame at the head of the queue (`attempts_current`).
    pub attempts_current: u32,
    /// Rows parked in `failed`. Not in §5.10.4's response, but the agent needs it to decide
    /// whether to keep an alert standing.
    pub failed: u64,
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

/// The durable queue.
#[derive(Debug)]
pub struct Journal {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl Journal {
    /// Open (creating if absent) and bring the schema up.
    ///
    /// The parent directory is created if it does not exist: `stacking_server.queue_dir` is an
    /// operator-supplied path that has never been written to on a fresh node, and refusing to
    /// start over a missing directory the node is perfectly able to create would be a startup
    /// failure with no information in it.
    ///
    /// # Errors
    /// [`JournalError`] if the directory or file cannot be created, the pragmas cannot be set, or
    /// the schema was written by a different version of this program.
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
        if let Some(parent) = path.parent() {
            // check-async: allow this whole function runs inside `spawn_blocking`.
            std::fs::create_dir_all(parent).map_err(|error| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(format!("cannot create {}: {error}", parent.display())),
                )
            })?;
        }

        let conn = Connection::open(path)?;
        // WAL so the status route can read the queue while an upload is being marked; FULL
        // because a lost `queued` row is a frame that is never sent (module docs).
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;

        // Read the version before touching the schema: `CREATE TABLE IF NOT EXISTS` against a
        // future layout succeeds silently and leaves the mismatch to be found by a wrong answer.
        let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(OpenFailure::Schema(found));
        }

        // §5.10.1's table, with three departures, all deliberate:
        //
        //   * `PRIMARY KEY (session_id, frame_id)` is declared where SQLite's grammar puts a
        //     table constraint — after the last column. §5.10.1 writes it in the middle of the
        //     column list, which does not parse.
        //   * §5.10.1 declares `session_id TEXT NOT NULL` twice. Once.
        //   * `last_error` is added. §5.10.1 makes `failed` terminal and "requires operator
        //     action", and the alert carrying the reason is a broadcast event that is gone by the
        //     time the operator looks. A parked row that cannot say why it parked asks the
        //     operator to reconstruct a refusal from a log file.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS queue (
                 session_id  TEXT    NOT NULL,
                 frame_id    TEXT    NOT NULL,
                 path        TEXT    NOT NULL,
                 sha256      TEXT    NOT NULL,
                 size_bytes  INTEGER NOT NULL,
                 state       TEXT    NOT NULL,
                 attempts    INTEGER NOT NULL DEFAULT 0,
                 queued_ts   TEXT    NOT NULL,
                 acked_ts    TEXT,
                 reclaimable INTEGER NOT NULL DEFAULT 0,
                 last_error  TEXT,
                 PRIMARY KEY (session_id, frame_id)
             );
             -- `(state, queued_ts)` is what `claim_next` and `snapshot` both look up on.
             -- `rowid` is deliberately absent: SQLite forbids it in an index, and it is not
             -- needed — it is the implicit tiebreak within equal `queued_ts` entries anyway.
             CREATE INDEX IF NOT EXISTS queue_by_age ON queue (state, queued_ts);",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(conn)
    }

    /// The database file, for log lines and error messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
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
            // its guard drops — and refusing every subsequent enqueue over a past panic would
            // turn one lost frame into a lost night.
            let mut guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut guard).map_err(|source| JournalError::from_sqlite(&path, source))
        })
        .await?
    }

    /// Record a frame as owed. Returns `true` if this call inserted the row.
    ///
    /// Idempotent on `(session_id, frame_id)`. A second `frame.saved` for the same frame — a
    /// replayed event, a subscriber that resynced — must not enqueue it twice, and must not
    /// resurrect a row that has already been acked.
    ///
    /// # Errors
    /// [`JournalError`] if the statement fails.
    pub async fn enqueue(&self, entry: NewEntry) -> Result<bool, JournalError> {
        let queued_ts = format_ts(astroctl_core::event::now_millis());
        let size = i64::try_from(entry.size_bytes).unwrap_or(i64::MAX);
        let path = entry.path.to_string_lossy().into_owned();

        self.with_conn(move |conn| {
            let inserted = conn.execute(
                "INSERT INTO queue
                     (session_id, frame_id, path, sha256, size_bytes, state, attempts, queued_ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 0, ?6)
                 ON CONFLICT (session_id, frame_id) DO NOTHING",
                params![
                    entry.session_id,
                    entry.frame_id,
                    path,
                    entry.sha256,
                    size,
                    queued_ts,
                ],
            )?;
            Ok(inserted == 1)
        })
        .await
    }

    /// Return every `uploading` row to `queued` — SDD §5.10.3's restart recovery.
    ///
    /// Re-upload is always safe because ingest deduplicates on `(session_id, frame_id)` and the
    /// checksum (§5.11.2), so a crash mid-upload costs one retransmission and never a lost or
    /// duplicated frame.
    ///
    /// `attempts` is incremented as part of the reset. That is not bookkeeping: `attempts > 0` is
    /// how the uploader knows a frame *may already be on the far side*, which is what makes the
    /// HEAD pre-flight worth asking (§5.11.1). A recovered row that kept `attempts = 0` would
    /// look indistinguishable from one that had never left this node.
    ///
    /// Returns how many rows were reset.
    ///
    /// # Errors
    /// [`JournalError`] if the statement fails.
    pub async fn recover_interrupted(&self) -> Result<u64, JournalError> {
        let reset = self
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE queue
                        SET state = 'queued', attempts = attempts + 1,
                            last_error = 'interrupted by a field-node restart'
                      WHERE state = 'uploading'",
                    [],
                )
            })
            .await?;
        Ok(reset as u64)
    }

    /// Claim the oldest queued frame and mark it `uploading`, in one transaction.
    ///
    /// Ordered by `queued_ts` then `rowid`. The tiebreak is load-bearing rather than pedantic:
    /// timestamps are millisecond resolution (SDD §2), so two frames enqueued in the same
    /// millisecond would otherwise drain in whatever order SQLite reached them — and "the queue
    /// drains in order" is an acceptance criterion an operator watches happen.
    ///
    /// # Errors
    /// [`JournalError`] if the transaction fails or a stored value is unreadable.
    pub async fn claim_next(&self) -> Result<Option<Entry>, JournalError> {
        let path = self.path.clone();
        let row = self
            .with_conn(move |conn| {
                let tx = conn.transaction()?;
                let claimed = tx
                    .query_row(
                        "SELECT session_id, frame_id, path, sha256, size_bytes, state, attempts,
                                queued_ts, acked_ts, reclaimable, last_error
                           FROM queue
                          WHERE state = 'queued'
                          ORDER BY queued_ts, rowid
                          LIMIT 1",
                        [],
                        read_row,
                    )
                    .optional()?;
                let Some(row) = claimed else {
                    return Ok(None);
                };
                tx.execute(
                    "UPDATE queue SET state = 'uploading'
                      WHERE session_id = ?1 AND frame_id = ?2",
                    params![row.0, row.1],
                )?;
                tx.commit()?;
                Ok(Some(row))
            })
            .await?;

        row.map(|row| entry(&path, row)).transpose()
    }

    /// Mark a frame acked and reclaim-eligible (§5.10.2, §5.10.3).
    ///
    /// `reclaimable = 1` is *marking only*: no deletion path exists in this increment. REL-13's
    /// retention policy is Phase 2b; the flag is the durable record that the archive of record has
    /// the frame, written only after the stack node's echoed checksum matched ours.
    ///
    /// # Errors
    /// [`JournalError`] if the statement fails.
    pub async fn mark_acked(
        &self,
        session_id: &str,
        frame_id: &str,
        acked_ts: DateTime<Utc>,
    ) -> Result<(), JournalError> {
        let (session_id, frame_id) = (session_id.to_owned(), frame_id.to_owned());
        let ts = format_ts(acked_ts);
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE queue
                    SET state = 'acked', acked_ts = ?3, reclaimable = 1, last_error = NULL
                  WHERE session_id = ?1 AND frame_id = ?2",
                params![session_id, frame_id, ts],
            )?;
            Ok(())
        })
        .await
    }

    /// Return a frame to `queued` after a failure that is not the frame's fault (§5.10.1).
    ///
    /// # Errors
    /// [`JournalError`] if the statement fails.
    pub async fn requeue(
        &self,
        session_id: &str,
        frame_id: &str,
        reason: &str,
    ) -> Result<u32, JournalError> {
        self.settle(session_id, frame_id, State::Queued, reason)
            .await
    }

    /// Park a frame in `failed` — terminal, operator action required (§5.10.1).
    ///
    /// # Errors
    /// [`JournalError`] if the statement fails.
    pub async fn mark_failed(
        &self,
        session_id: &str,
        frame_id: &str,
        reason: &str,
    ) -> Result<u32, JournalError> {
        self.settle(session_id, frame_id, State::Failed, reason)
            .await
    }

    /// The shared tail of `requeue`/`mark_failed`: set the state, count the attempt, record why.
    /// Returns the row's new `attempts`.
    async fn settle(
        &self,
        session_id: &str,
        frame_id: &str,
        state: State,
        reason: &str,
    ) -> Result<u32, JournalError> {
        let (session_id, frame_id) = (session_id.to_owned(), frame_id.to_owned());
        let reason = reason.to_owned();
        let state = state.as_str();
        let attempts = self
            .with_conn(move |conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "UPDATE queue
                        SET state = ?3, attempts = attempts + 1, last_error = ?4
                      WHERE session_id = ?1 AND frame_id = ?2",
                    params![session_id, frame_id, state, reason],
                )?;
                let attempts: i64 = tx.query_row(
                    "SELECT attempts FROM queue WHERE session_id = ?1 AND frame_id = ?2",
                    params![session_id, frame_id],
                    |row| row.get(0),
                )?;
                tx.commit()?;
                Ok(attempts)
            })
            .await?;
        Ok(u32::try_from(attempts).unwrap_or(u32::MAX))
    }

    /// One row, by key — for the status route's diagnostics and for tests.
    ///
    /// # Errors
    /// [`JournalError`] if the query fails or a stored value is unreadable.
    pub async fn lookup(
        &self,
        session_id: &str,
        frame_id: &str,
    ) -> Result<Option<Entry>, JournalError> {
        let (session_id, frame_id) = (session_id.to_owned(), frame_id.to_owned());
        let path = self.path.clone();
        let row = self
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT session_id, frame_id, path, sha256, size_bytes, state, attempts,
                            queued_ts, acked_ts, reclaimable, last_error
                       FROM queue WHERE session_id = ?1 AND frame_id = ?2",
                    params![session_id, frame_id],
                    read_row,
                )
                .optional()
            })
            .await?;
        row.map(|row| entry(&path, row)).transpose()
    }

    /// The queue as the status route and the `transfer.status` event see it.
    ///
    /// `depth` counts `queued` **and** `uploading`. §5.10.4 calls the field `queue_depth`, and the
    /// operator's question behind it is "how many frames does this node still owe the archive" —
    /// a frame that is halfway up the link is still owed. Counting only `queued` would make the
    /// depth of a queue with one frame left in it read `0` for the twenty minutes that frame is
    /// being uploaded, which is exactly when someone is watching.
    ///
    /// # Errors
    /// [`JournalError`] if the query fails or a stored timestamp is unreadable.
    pub async fn snapshot(&self) -> Result<Snapshot, JournalError> {
        let path = self.path.clone();
        let (depth, oldest, last_ack, attempts, failed) = self
            .with_conn(move |conn| {
                let (depth, oldest): (i64, Option<String>) = conn.query_row(
                    "SELECT COUNT(*), MIN(queued_ts) FROM queue
                      WHERE state IN ('queued', 'uploading')",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let last_ack: Option<String> = conn.query_row(
                    "SELECT MAX(acked_ts) FROM queue WHERE state = 'acked'",
                    [],
                    |row| row.get(0),
                )?;
                // The head of the queue, by the same order `claim_next` drains in, so the
                // `attempts_current` an operator reads is the counter of the frame they are
                // watching stall.
                let attempts: Option<i64> = conn
                    .query_row(
                        "SELECT attempts FROM queue
                          WHERE state IN ('queued', 'uploading')
                          ORDER BY queued_ts, rowid LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                let failed: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM queue WHERE state = 'failed'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((depth, oldest, last_ack, attempts, failed))
            })
            .await?;

        Ok(Snapshot {
            depth: u64::try_from(depth).unwrap_or(0),
            oldest_queued_ts: oldest
                .map(|ts| parse_ts(&path, "queued_ts", &ts))
                .transpose()?,
            last_ack_ts: last_ack
                .map(|ts| parse_ts(&path, "acked_ts", &ts))
                .transpose()?,
            attempts_current: attempts.map_or(0, |a| u32::try_from(a).unwrap_or(u32::MAX)),
            failed: u64::try_from(failed).unwrap_or(0),
        })
    }

    /// Every row, oldest first — the acceptance criterion's "drains in order" needs to be
    /// assertable, and an operator debugging a stuck night needs to see the whole list.
    ///
    /// # Errors
    /// [`JournalError`] if the query fails or a stored value is unreadable.
    pub async fn entries(&self) -> Result<Vec<Entry>, JournalError> {
        let path = self.path.clone();
        let rows = self
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT session_id, frame_id, path, sha256, size_bytes, state, attempts,
                            queued_ts, acked_ts, reclaimable, last_error
                       FROM queue ORDER BY queued_ts, rowid",
                )?;
                let rows = stmt
                    .query_map([], read_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        rows.into_iter().map(|row| entry(&path, row)).collect()
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

/// SDD §2: every persisted timestamp is UTC RFC 3339 with milliseconds — the same spelling the
/// event schema uses, so a journal row and the event about it are comparable as text. That is
/// also what makes `MIN(queued_ts)` and `ORDER BY queued_ts` correct: the format sorts
/// lexicographically in the same order it sorts chronologically.
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

/// One row as every `SELECT` above spells it.
type QueueRow = (
    String,
    String,
    String,
    String,
    i64,
    String,
    i64,
    String,
    Option<String>,
    i64,
    Option<String>,
);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn entry(path: &Path, row: QueueRow) -> Result<Entry, JournalError> {
    let (
        session_id,
        frame_id,
        frame_path,
        sha256,
        size,
        state,
        attempts,
        queued_ts,
        acked_ts,
        reclaimable,
        last_error,
    ) = row;

    Ok(Entry {
        session_id,
        frame_id,
        path: PathBuf::from(frame_path),
        sha256,
        size_bytes: u64::try_from(size).map_err(|_| JournalError::Corrupt {
            path: path.to_owned(),
            column: "size_bytes",
            value: size.to_string(),
            expected: "non-negative size",
        })?,
        state: State::parse(&state).ok_or_else(|| JournalError::Corrupt {
            path: path.to_owned(),
            column: "state",
            value: state.clone(),
            expected: "queued|uploading|acked|failed",
        })?,
        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        queued_ts: parse_ts(path, "queued_ts", &queued_ts)?,
        acked_ts: acked_ts
            .map(|ts| parse_ts(path, "acked_ts", &ts))
            .transpose()?,
        reclaimable: reclaimable != 0,
        last_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn new_entry(session: &str, id: &str, sha: &str) -> NewEntry {
        NewEntry {
            session_id: session.to_owned(),
            frame_id: id.to_owned(),
            path: PathBuf::from(format!("/data/sessions/{session}/frames/{id}.cr3")),
            sha256: sha.to_owned(),
            size_bytes: 25 * 1024 * 1024,
        }
    }

    async fn journal(dir: &TempDir) -> Journal {
        Journal::open(dir.path().join("queue").join("transfer.db"))
            .await
            .expect("a fresh journal opens")
    }

    #[tokio::test]
    async fn a_fresh_journal_is_empty_and_creates_its_directory() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;
        assert_eq!(journal.snapshot().await.unwrap(), Snapshot::default());
        assert!(dir.path().join("queue").is_dir(), "queue_dir is created");
    }

    #[tokio::test]
    async fn an_enqueued_frame_is_owed_and_claimable() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;
        assert!(journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap());

        let snapshot = journal.snapshot().await.unwrap();
        assert_eq!(snapshot.depth, 1);
        assert!(snapshot.oldest_queued_ts.is_some());
        assert_eq!(snapshot.last_ack_ts, None);

        let claimed = journal.claim_next().await.unwrap().expect("one queued row");
        assert_eq!(claimed.frame_id, "light_00001");
        assert_eq!(
            claimed.state,
            State::Queued,
            "the row as it was before the claim"
        );
        assert_eq!(
            journal
                .lookup("s1", "light_00001")
                .await
                .unwrap()
                .unwrap()
                .state,
            State::Uploading,
            "the claim marked it in the same transaction"
        );
        // An in-flight frame is still owed (§5.10.4's queue_depth).
        assert_eq!(journal.snapshot().await.unwrap().depth, 1);
        assert!(
            journal.claim_next().await.unwrap().is_none(),
            "nothing else is queued"
        );
    }

    /// A replayed `frame.saved` must not enqueue the same frame twice, and must never resurrect
    /// one that has already been acked.
    #[tokio::test]
    async fn enqueueing_the_same_frame_twice_inserts_once() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;

        assert!(journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap());
        assert!(!journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap());

        let claimed = journal.claim_next().await.unwrap().unwrap();
        journal
            .mark_acked(
                &claimed.session_id,
                &claimed.frame_id,
                astroctl_core::event::now_millis(),
            )
            .await
            .unwrap();

        assert!(!journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap());
        assert_eq!(
            journal
                .lookup("s1", "light_00001")
                .await
                .unwrap()
                .unwrap()
                .state,
            State::Acked,
            "an acked frame is not re-queued by a replayed event"
        );
    }

    /// The reason the key is `(session_id, frame_id)`: per-session counters (§5.5) mean every
    /// session has a `light_00001`. With a bare `frame_id` key the second session's frame 1 would
    /// silently fail to enqueue and would never be sent.
    #[tokio::test]
    async fn the_same_frame_id_in_two_sessions_is_two_frames() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;

        assert!(journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap());
        assert!(journal
            .enqueue(new_entry("s2", "light_00001", "bb"))
            .await
            .unwrap());
        assert_eq!(journal.snapshot().await.unwrap().depth, 2);
    }

    #[tokio::test]
    async fn an_ack_marks_the_frame_reclaim_eligible_and_nothing_deletes_it() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;
        journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap();
        let claimed = journal.claim_next().await.unwrap().unwrap();

        let acked_at = astroctl_core::event::now_millis();
        journal
            .mark_acked("s1", "light_00001", acked_at)
            .await
            .unwrap();

        let row = journal.lookup("s1", "light_00001").await.unwrap().unwrap();
        assert_eq!(row.state, State::Acked);
        assert!(row.reclaimable, "REL-13 marking");
        assert_eq!(row.acked_ts, Some(acked_at));
        // The frame itself is untouched — the path still points where it always did (REL-11).
        assert_eq!(row.path, claimed.path);

        let snapshot = journal.snapshot().await.unwrap();
        assert_eq!(snapshot.depth, 0);
        assert_eq!(snapshot.last_ack_ts, Some(acked_at));
    }

    #[tokio::test]
    async fn a_requeue_counts_the_attempt_and_a_failure_is_terminal() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;
        journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap();

        journal.claim_next().await.unwrap().unwrap();
        assert_eq!(
            journal
                .requeue("s1", "light_00001", "connection refused")
                .await
                .unwrap(),
            1
        );
        let row = journal.lookup("s1", "light_00001").await.unwrap().unwrap();
        assert_eq!(row.state, State::Queued);
        assert_eq!(row.last_error.as_deref(), Some("connection refused"));

        journal.claim_next().await.unwrap().unwrap();
        assert_eq!(
            journal
                .mark_failed("s1", "light_00001", "422 VALIDATION")
                .await
                .unwrap(),
            2
        );
        let row = journal.lookup("s1", "light_00001").await.unwrap().unwrap();
        assert_eq!(row.state, State::Failed);
        assert!(!row.reclaimable, "a refused frame is not on the far side");
        // Terminal: it is not offered again.
        assert!(journal.claim_next().await.unwrap().is_none());
        assert_eq!(journal.snapshot().await.unwrap().failed, 1);
    }

    /// SDD §5.10.3 — and the attempt bump that tells the uploader the frame may already be there.
    #[tokio::test]
    async fn a_restart_returns_interrupted_uploads_to_the_queue() {
        let dir = TempDir::new();
        {
            let journal = journal(&dir).await;
            journal
                .enqueue(new_entry("s1", "light_00001", "aa"))
                .await
                .unwrap();
            journal
                .enqueue(new_entry("s1", "light_00002", "bb"))
                .await
                .unwrap();
            journal.claim_next().await.unwrap().unwrap();
            // …and the process dies here, with `light_00001` marked `uploading`.
        }

        let reopened = journal(&dir).await;
        assert_eq!(
            reopened
                .lookup("s1", "light_00001")
                .await
                .unwrap()
                .unwrap()
                .state,
            State::Uploading,
            "the row survives the restart exactly as it was"
        );
        assert_eq!(reopened.recover_interrupted().await.unwrap(), 1);

        let row = reopened.lookup("s1", "light_00001").await.unwrap().unwrap();
        assert_eq!(row.state, State::Queued);
        assert_eq!(
            row.attempts, 1,
            "a recovered row may already be on the far side"
        );

        // …and it is still the head of the queue: recovery does not reorder the night.
        assert_eq!(
            reopened.claim_next().await.unwrap().unwrap().frame_id,
            "light_00001"
        );
    }

    #[tokio::test]
    async fn the_queue_drains_oldest_first_even_within_one_millisecond() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;
        for n in 1..=5 {
            journal
                .enqueue(new_entry("s1", &format!("light_{n:05}"), "aa"))
                .await
                .unwrap();
        }

        let mut drained = Vec::new();
        while let Some(row) = journal.claim_next().await.unwrap() {
            journal
                .mark_acked(
                    &row.session_id,
                    &row.frame_id,
                    astroctl_core::event::now_millis(),
                )
                .await
                .unwrap();
            drained.push(row.frame_id);
        }
        assert_eq!(
            drained,
            [
                "light_00001",
                "light_00002",
                "light_00003",
                "light_00004",
                "light_00005"
            ],
            "insertion order breaks the millisecond ties"
        );
    }

    #[tokio::test]
    async fn attempts_current_is_the_head_of_the_queue() {
        let dir = TempDir::new();
        let journal = journal(&dir).await;
        journal
            .enqueue(new_entry("s1", "light_00001", "aa"))
            .await
            .unwrap();
        journal
            .enqueue(new_entry("s1", "light_00002", "bb"))
            .await
            .unwrap();

        journal.claim_next().await.unwrap();
        journal
            .requeue("s1", "light_00001", "unreachable")
            .await
            .unwrap();
        journal.claim_next().await.unwrap();
        journal
            .requeue("s1", "light_00001", "unreachable")
            .await
            .unwrap();

        let snapshot = journal.snapshot().await.unwrap();
        assert_eq!(snapshot.depth, 2);
        assert_eq!(
            snapshot.attempts_current, 2,
            "the frame the operator is watching stall"
        );
    }

    #[tokio::test]
    async fn rows_survive_reopening_the_database() {
        let dir = TempDir::new();
        {
            let journal = journal(&dir).await;
            journal
                .enqueue(new_entry("s1", "light_00001", "aa"))
                .await
                .unwrap();
        }
        let reopened = journal(&dir).await;
        assert_eq!(reopened.snapshot().await.unwrap().depth, 1);
        assert_eq!(reopened.entries().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_database_from_a_newer_build_is_refused_rather_than_guessed_at() {
        let dir = TempDir::new();
        let path = dir.path().join("queue").join("transfer.db");
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
