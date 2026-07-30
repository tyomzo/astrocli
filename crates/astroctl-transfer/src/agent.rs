//! The upload loop, its backoff, and the events it publishes — SDD §5.10.2, §5.10.4.
//!
//! ```text
//! enqueue (field wiring) ─► notify
//! uploader: pick oldest queued ─► mark uploading ─► HEAD pre-flight ─► POST multipart
//!           ─► on ack: verify echoed sha ─► mark acked, reclaimable=1, emit transfer.acked
//!           ─► on retryable failure: mark queued, attempts+=1, backoff
//!           ─► on a verdict about the frame: mark failed, one alert, never offered again
//! ```
//!
//! # One upload in flight
//!
//! §5.10.2 fixes this and gives two reasons — ordering matters for the operator's mental model,
//! and concurrency buys nothing on a constrained tunnel. There is a third that matters more on the
//! field node: it is the only thing in this increment that keeps a transfer from starving control
//! traffic. §8.3(7)'s bandwidth cap and interactive floor are **not enforced here** — §5.10.4
//! defers that to Phase 2b and requires the keys to be parsed and validated meanwhile, which
//! `PacingConfig` already is. [`TransferAgent::spawn`] logs what it would enforce and says that it
//! does not, so the deviation is visible in the night's log rather than only in a document.
//!
//! # Stack-down is a normal state
//!
//! §5.10.2: **one** alert when the link goes offline and one when it recovers, never one per
//! attempt — a night-long outage must not produce thousands of events. The backoff doubles from
//! `stacking_server.retry_interval` to a five-minute ceiling and **resets after a success**, which
//! is the lesson M1-T13 recorded for worker restarts: a backoff that only ever grows turns one bad
//! minute into an hour of idle link.
//!
//! # Shutdown
//!
//! The uploader publishes, so it holds an [`EventBus`] handle — a broadcast sender that keeps the
//! session log's subscriber open. [`TransferAgent::abort`] must therefore run before `drop(bus)`
//! in the binary's shutdown sequence. An upload in flight at that moment is **abandoned, not
//! awaited**: §5.10.3 makes re-upload safe by design, so waiting would pay a 25 MB budget for a
//! byte range the next process will send again anyway.

use std::sync::Arc;
use std::time::Duration;

use astroctl_core::bus::EventBus;
use astroctl_core::event::{Alert, TransferAcked, TransferState, TransferStatus};

use crate::journal::{Journal, Snapshot};
use crate::upload::{Outcome, Preflight, Uploader};

/// Ceiling for the capped exponential backoff (§5.10.2).
pub const BACKOFF_CEILING: Duration = Duration::from_secs(300);

/// How often `transfer.status` is republished when nothing changed (SDD §4.3).
pub const STATUS_INTERVAL: Duration = Duration::from_secs(30);

/// How many times a frame may come back with an unreadable ack before it is parked.
///
/// The task's "sha mismatch → re-upload, alert after N failures". A `200` whose echoed checksum is
/// not ours is not a verdict about the frame, so the frame is offered again — but a peer that
/// answers incomprehensibly five times running will not start making sense on the sixth, and a
/// queue that retries it forever never drains.
pub const MAX_ECHO_MISMATCHES: u32 = 5;

/// Capped exponential backoff that resets on success (§5.10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    current: Duration,
}

impl Backoff {
    /// Start from `stacking_server.retry_interval`.
    #[must_use]
    pub fn new(base: Duration) -> Self {
        // A zero base would make the loop a spin. The config validator bounds `retry_interval` to
        // 1..=3600, so this only guards a caller that built the agent by hand.
        let base = base.max(Duration::from_secs(1));
        Self {
            base,
            current: base,
        }
    }

    /// The wait before the next attempt, then double for the one after.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(BACKOFF_CEILING);
        delay
    }

    /// A success means the link works; the next failure starts from the base again.
    pub fn reset(&mut self) {
        self.current = self.base;
    }

    /// What the next failure would wait — for tests and log lines.
    #[must_use]
    pub fn peek(&self) -> Duration {
        self.current
    }
}

/// What the agent needs to talk to a stacking server, extracted from `stacking_server` config.
///
/// Values are copied out rather than holding the whole `FieldConfig`: the house rule is that a
/// facade takes what it needs at construction, so there is exactly one place the configuration is
/// read (SDD §4.4).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Backoff base — `stacking_server.retry_interval`, in seconds.
    pub retry_interval: Duration,
    /// Where the frames go.
    pub uploader: Uploader,
}

/// Shared state between the upload loop and the `/api/transfer/status` route.
#[derive(Debug)]
pub struct TransferQueue {
    journal: Journal,
    bus: EventBus,
    /// The last `transfer.status` published, so the 30 s republish does not drown the discrete
    /// events (§4.3). Never awaited under — `clippy::await_holding_lock` is denied workspace-wide.
    last_status: std::sync::Mutex<Option<TransferStatus>>,
    /// Whether the link is currently believed down, so the offline/recovered alerts fire once
    /// each on the transition rather than once per attempt (§5.10.2).
    offline: std::sync::Mutex<bool>,
    /// Raised by the field wiring when a row is enqueued, so a fresh frame does not wait out a
    /// backoff that was scheduled for a different problem.
    wake: tokio::sync::Notify,
}

impl TransferQueue {
    /// Wrap an open journal.
    #[must_use]
    pub fn new(journal: Journal, bus: EventBus) -> Self {
        Self {
            journal,
            bus,
            last_status: std::sync::Mutex::new(None),
            offline: std::sync::Mutex::new(false),
            wake: tokio::sync::Notify::new(),
        }
    }

    /// The journal, for the field wiring's enqueue path and for tests.
    #[must_use]
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Tell the uploader there is something to do.
    pub fn notify(&self) {
        self.wake.notify_one();
    }

    /// The queue as `/api/transfer/status` reports it (§5.10.4).
    ///
    /// # Errors
    /// Propagates a [`crate::journal::JournalError`] rather than inventing a state: a status route
    /// that answered "idle" because it could not read the queue would be the most misleading
    /// possible answer.
    pub async fn snapshot(&self) -> Result<Snapshot, crate::journal::JournalError> {
        self.journal.snapshot().await
    }

    /// Whether the link is currently believed down — the third input to [`TransferState`].
    #[must_use]
    pub fn is_offline(&self) -> bool {
        *self
            .offline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Build the `transfer.status` payload from a snapshot.
    ///
    /// `uploading` is reported when the queue holds work and the link is believed up; `offline`
    /// when it is believed down, whether or not anything is queued — an operator whose stack node
    /// is unplugged needs to see that before the first frame lands, not after.
    #[must_use]
    pub fn status_of(
        &self,
        snapshot: &Snapshot,
        now: chrono::DateTime<chrono::Utc>,
    ) -> TransferStatus {
        let state = if self.is_offline() {
            TransferState::Offline
        } else if snapshot.depth > 0 {
            TransferState::Uploading
        } else {
            TransferState::Idle
        };
        TransferStatus::new(
            state,
            snapshot.depth,
            snapshot.oldest_queued_ts.map(|ts| {
                // Clamped at zero: REL-14 admits an undisciplined clock, and a negative age would
                // render in the PWA as a frame queued in the future.
                (now - ts).num_milliseconds().max(0) as f64 / 1000.0
            }),
            snapshot.last_ack_ts,
        )
    }

    /// Publish `transfer.status` if it changed, or unconditionally when `force`.
    ///
    /// Returns what the queue looks like now, whether or not anything was published.
    async fn publish_status(&self, force: bool) -> Snapshot {
        let snapshot = match self.journal.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::error!(%error, "cannot read the transfer queue for a status event");
                return Snapshot::default();
            }
        };
        let status = self.status_of(&snapshot, astroctl_core::event::now_millis());

        let changed = {
            let mut last = self
                .last_status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let changed = last.as_ref() != Some(&status);
            *last = Some(status.clone());
            changed
        };
        if changed || force {
            self.bus.publish(status);
        }
        snapshot
    }

    /// Move into or out of the offline state, emitting exactly one alert on each transition.
    fn set_offline(&self, offline: bool, code: &str, message: &str) {
        let changed = {
            let mut current = self
                .offline
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let changed = *current != offline;
            *current = offline;
            changed
        };
        if !changed {
            return;
        }
        if offline {
            // Warning, not error: §5.10.1 calls an unreachable stack node "a normal operating
            // state". The frames are durable, the capture flow is untouched, and the queue is
            // doing exactly what it exists to do.
            self.bus.publish(Alert::warning(code, message.to_owned()));
        } else {
            self.bus.publish(Alert::info(code, message.to_owned()));
        }
    }
}

/// The task that drains the queue, and the handle shutdown stops it with.
#[derive(Debug)]
pub struct TransferAgent {
    uploader: tokio::task::JoinHandle<()>,
    heartbeat: tokio::task::JoinHandle<()>,
    /// The `frame.saved` listener, which lives in the binary (it needs the frame store) but is
    /// adopted here so that shutdown has exactly one handle to stop rather than two to remember.
    listener: Option<tokio::task::JoinHandle<()>>,
}

impl TransferAgent {
    /// Adopt the binary's `frame.saved` listener into this handle.
    #[must_use]
    pub fn with_listener(mut self, listener: tokio::task::JoinHandle<()>) -> Self {
        self.listener = Some(listener);
        self
    }

    /// Stop the agent.
    ///
    /// Must be called before `drop(bus)` in the binary's shutdown sequence: both tasks hold an
    /// [`EventBus`] handle, which is a broadcast sender, and a surviving sender keeps the session
    /// log's subscriber open and costs the flush its whole timeout.
    ///
    /// An upload in flight is abandoned rather than awaited. §5.10.3 makes that safe — the row
    /// stays `uploading`, the next startup returns it to `queued`, and ingest deduplicates the
    /// re-send — so waiting would only delay the exit by however much of a 25 MB frame is left.
    pub async fn abort(self) {
        self.uploader.abort();
        self.heartbeat.abort();
        if let Some(listener) = &self.listener {
            listener.abort();
        }
        let _ = self.uploader.await;
        let _ = self.heartbeat.await;
        if let Some(listener) = self.listener {
            let _ = listener.await;
        }
    }

    /// Start the agent against an already-recovered queue.
    ///
    /// The caller is expected to have run [`Journal::recover_interrupted`] first — it is a startup
    /// step (§5.10.3, §8.1) and the binary is where startup steps are ordered and logged.
    ///
    /// `pacing` is taken only so that the deviation of §5.10.4 is stated where an operator will
    /// see it. Nothing enforces it in this increment.
    #[must_use]
    pub fn spawn(
        queue: Arc<TransferQueue>,
        config: AgentConfig,
        pacing: astroctl_core::config::PacingConfig,
    ) -> Self {
        tracing::info!(
            upstream = %config.uploader.upstream(),
            retry_interval_s = config.retry_interval.as_secs(),
            bandwidth_cap_mbps = ?pacing.bandwidth_cap_mbps,
            interactive_floor_pct = pacing.interactive_floor_pct,
            interactive_window_s = pacing.interactive_window_seconds,
            "transfer agent started. `stacking_server.pacing` is parsed and validated but NOT \
             enforced in this increment (SDD §5.10.4, §8.3(7) — enforcement lands with Phase 2b). \
             Until then the only thing bounding this node's share of the link is that exactly one \
             upload is ever in flight."
        );

        let uploader = tokio::spawn(upload_loop(Arc::clone(&queue), config));
        let heartbeat = tokio::spawn(heartbeat(queue));
        Self {
            uploader,
            heartbeat,
            listener: None,
        }
    }
}

/// `transfer.status` every 30 s even when nothing changed (§4.3), so a PWA that connected during
/// a quiet hour is not left with a blank panel until the next frame.
async fn heartbeat(queue: Arc<TransferQueue>) {
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        queue.publish_status(true).await;
    }
}

/// The drain loop of §5.10.2.
async fn upload_loop(queue: Arc<TransferQueue>, config: AgentConfig) {
    let mut backoff = Backoff::new(config.retry_interval);
    // The first status of the process, so a PWA connecting immediately after a restart sees the
    // recovered queue rather than nothing.
    queue.publish_status(true).await;

    loop {
        let claimed = match queue.journal.claim_next().await {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                // Nothing owed. Wait to be told, rather than polling: an idle observatory should
                // cost no wakeups at all.
                queue.wake.notified().await;
                continue;
            }
            Err(error) => {
                // The journal is the queue. If it cannot be read there is nothing useful to do but
                // wait and try again — and say so loudly, because this is the one failure that can
                // silently stop frames reaching the archive.
                tracing::error!(%error, "cannot read the transfer queue; retrying after a backoff");
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            }
        };

        queue.publish_status(false).await;
        let outcome = attempt(&config, &claimed).await;

        match outcome {
            AttemptOutcome::Acked { sha256, duplicate } => {
                let acked_at = astroctl_core::event::now_millis();
                if let Err(error) = queue
                    .journal
                    .mark_acked(&claimed.session_id, &claimed.frame_id, acked_at)
                    .await
                {
                    // The bytes are on the far side but this node could not write that down. Do
                    // not emit `transfer.acked`: the event is what REL-13 would let an operator
                    // reclaim on, and it must never outrun the durable record it stands for. The
                    // row stays `uploading` and the next restart re-offers the frame, which the
                    // receiver dedups.
                    tracing::error!(
                        frame = %claimed.frame_id, %error,
                        "the stack node acked a frame but the queue could not record it"
                    );
                    continue;
                }

                backoff.reset();
                queue.set_offline(
                    false,
                    "STACK_ONLINE",
                    &format!(
                        "the stacking server at {} is answering again",
                        config.uploader.upstream()
                    ),
                );

                let snapshot = queue.publish_status(false).await;
                queue.bus.publish(TransferAcked::new(
                    &claimed.frame_id,
                    &sha256,
                    acked_at,
                    snapshot.depth,
                ));
                tracing::info!(
                    frame = %claimed.frame_id,
                    session = %claimed.session_id,
                    duplicate,
                    queue_depth = snapshot.depth,
                    "frame acked by the stacking server"
                );
            }

            AttemptOutcome::Retry(reason) => {
                let attempts = queue
                    .journal
                    .requeue(&claimed.session_id, &claimed.frame_id, &reason.message)
                    .await
                    .unwrap_or(0);
                // One alert on the transition, never per attempt (§5.10.2). Every subsequent
                // failure is a `debug` line and a growing `attempts` column, both of which an
                // operator can pull when they care (§5.10.4's `attempts_current`).
                queue.set_offline(true, reason.code, &reason.message);
                let delay = backoff.next_delay();
                tracing::debug!(
                    frame = %claimed.frame_id,
                    attempts,
                    retry_in_s = delay.as_secs(),
                    reason = %reason.message,
                    "upload failed; the frame stays queued"
                );
                queue.publish_status(false).await;

                // Sleep, but wake early if a fresh frame arrives: the backoff is about the link
                // being unwell, and a new capture is not a reason to keep waiting — but it is a
                // reason to have an up-to-date queue depth on the next status event.
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = queue.wake.notified() => {}
                }
            }

            AttemptOutcome::Failed(refusal) => {
                let attempts = queue
                    .journal
                    .mark_failed(&claimed.session_id, &claimed.frame_id, &refusal.message)
                    .await
                    .unwrap_or(0);
                // `critical`, and one per frame rather than one per transition. §4.3 reserves
                // that level for "the operator must act", which is exactly what §5.10.1 says a
                // `failed` row requires — and unlike every other alert this agent raises, this
                // one does not resolve itself when the link comes back. `warning` is the level
                // `STACK_OFFLINE` already uses for a condition that clears on its own, and
                // spelling a permanent one the same way would make the two indistinguishable in
                // the header treatment they drive (§5.9). Each parked frame is its own alert
                // because each is its own thing for the operator to act on.
                queue.bus.publish(Alert::critical(
                    "TRANSFER_FAILED",
                    format!(
                        "frame {} will not be delivered and has been parked after {attempts} \
                         attempts: {}",
                        claimed.frame_id, refusal.message
                    ),
                ));
                tracing::error!(
                    frame = %claimed.frame_id,
                    session = %claimed.session_id,
                    code = %refusal.code,
                    attempts,
                    "frame parked: {}", refusal.message
                );
                // The link itself is fine — the far node answered. Do not let one refused frame
                // put the whole transfer into `offline`, and do not extend the backoff: the next
                // frame deserves an immediate try.
                backoff.reset();
                queue.set_offline(
                    false,
                    "STACK_ONLINE",
                    &format!(
                        "the stacking server at {} is answering",
                        config.uploader.upstream()
                    ),
                );
                queue.publish_status(false).await;
            }
        }
    }
}

/// What one pass over a claimed row decided.
enum AttemptOutcome {
    Acked { sha256: String, duplicate: bool },
    Retry(crate::upload::RetryReason),
    Failed(crate::upload::Refusal),
}

/// One frame: pre-flight, upload, interpret.
async fn attempt(config: &AgentConfig, entry: &crate::journal::Entry) -> AttemptOutcome {
    let Some(upload) = crate::frame_upload(entry).await else {
        return AttemptOutcome::Failed(crate::upload::Refusal {
            code: "VALIDATION".to_owned(),
            message: format!(
                "the frame at {} has no extension the archive can store it under",
                entry.path.display()
            ),
        });
    };

    // §5.11.1's pre-flight. A `Stored` answer whose checksum matches ours is an ack that cost one
    // round trip instead of a whole frame.
    if let Preflight::Stored { sha256 } = config
        .uploader
        .preflight(&entry.session_id, &entry.frame_id)
        .await
    {
        if sha256 == entry.sha256.to_ascii_lowercase() {
            tracing::info!(
                frame = %entry.frame_id,
                "the stacking server already holds this frame; the upload was skipped"
            );
            return AttemptOutcome::Acked {
                sha256,
                duplicate: true,
            };
        }
        // Stored under this id with *different* bytes. That is a conflict, and it is the receiver's
        // verdict to give, not ours: §5.11.1 makes the pre-flight an optimisation and never a gate,
        // and a `409` reached by inference here would park a frame on the strength of a header. So
        // the upload proceeds and the far side answers `FRAME_ID_CONFLICT` on the evidence.
        tracing::warn!(
            frame = %entry.frame_id,
            stored = %sha256,
            ours = %entry.sha256,
            "the stacking server holds this id with different bytes; uploading for its verdict"
        );
    }

    match config.uploader.upload(&upload).await {
        Outcome::Acked { sha256, duplicate } => AttemptOutcome::Acked { sha256, duplicate },
        Outcome::Retry(reason) => AttemptOutcome::Retry(reason),
        Outcome::Refused(refusal) => AttemptOutcome::Failed(refusal),
        Outcome::EchoMismatch { expected, echoed } => {
            let message = format!(
                "the stacking server acked {} with checksum {echoed}, but this node sent \
                 {expected}",
                entry.frame_id
            );
            // Re-offer a bounded number of times — the task's "sha mismatch → re-upload, alert
            // after N failures" — because a `200` we cannot interpret leaves the frame's fate
            // unknown, and asking again is the only way to find out.
            if entry.attempts + 1 >= MAX_ECHO_MISMATCHES {
                AttemptOutcome::Failed(crate::upload::Refusal {
                    code: "CHECKSUM_MISMATCH".to_owned(),
                    message: format!("{message} (after {MAX_ECHO_MISMATCHES} attempts)"),
                })
            } else {
                AttemptOutcome::Retry(crate::upload::RetryReason {
                    code: "STACK_ERROR",
                    message,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_to_a_ceiling_and_resets_after_a_success() {
        let mut backoff = Backoff::new(Duration::from_secs(10));
        assert_eq!(backoff.next_delay(), Duration::from_secs(10));
        assert_eq!(backoff.next_delay(), Duration::from_secs(20));
        assert_eq!(backoff.next_delay(), Duration::from_secs(40));
        for _ in 0..10 {
            backoff.next_delay();
        }
        assert_eq!(
            backoff.peek(),
            BACKOFF_CEILING,
            "a night-long outage caps out"
        );

        // M1-T13's lesson: a backoff that only grows turns one bad minute into an hour of idle
        // link once the stack node is back.
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(10));
    }

    #[test]
    fn a_zero_base_cannot_turn_the_loop_into_a_spin() {
        let mut backoff = Backoff::new(Duration::ZERO);
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }
}
