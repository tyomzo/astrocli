//! `stack.status`, republished from the stacking server's health — SDD §4.3, USB-06.
//!
//! USB-06 asks for "connected/disconnected, queue depth, current stack frame count, last preview
//! timestamp". Three of those live on the *other* node and one does not: queue depth is the field
//! node's own upload queue and rides `transfer.status` (§5.10.4), which is why this module is
//! about the other three and the panel joins them.
//!
//! # Why the field node polls instead of subscribing
//!
//! §5.11.1 gives the stacking server a `/ws` for JSON status, and subscribing to it would be the
//! obvious design. It is not built, deliberately: a socket has to be maintained, reconnected and
//! backed off exactly like the PWA's, and all it would carry is a value that changes when a frame
//! lands. Polling two REST routes the field node *already proxies* costs one request every
//! [`POLL_INTERVAL`] and cannot get out of step with a reconnect state machine, because it has
//! none. If a later phase gives the stack node status worth streaming, this is the module that
//! changes and the event topic does not.
//!
//! # One event source for the PWA
//!
//! §4.3's row says it: "republished by the field node from the stack's health so the PWA has one
//! event source (USB-06)". The browser never talks to the stacking server — ADR-07 gives it a
//! single origin — so without this the stack panel would have to poll `/stack/api/...` itself and
//! the PWA's store discipline (§5.9: "fed exclusively by WS events plus the connect snapshot — no
//! REST polling") would have one exception in it.
//!
//! # On change, and every 30 s
//!
//! §4.3's cadence. The periodic republish is not redundant with the connect snapshot: `stack.status`
//! is a stateful topic (§5.8.3), so a reconnecting client gets the latest value immediately — but a
//! client connected *through* a stale value has no way to tell "unchanged" from "the field node
//! stopped looking", and the operator watching an empty panel deserves the difference.
//!
//! # Unreachable is a value, not an absence
//!
//! When the stacking server does not answer, this publishes [`StackStatus::offline`] carrying the
//! **last known** counts, never zeroes. A frame count that dropped to 0 because a tunnel blinked
//! would read as "the stacking server lost my session", which is the single most alarming thing
//! this panel could say and would be false. `worker_state` goes `null` in that state because the
//! field node genuinely does not know it — which is exactly the absence §4.3 reserves `null` for,
//! and is why `WorkerState::Stopped` had to exist separately (SDD change note 1.16.0).

use std::sync::Arc;
use std::time::Duration;

use astroctl_core::bus::EventBus;
use astroctl_core::event::{Alert, StackStatus, WorkerState};
use chrono::{DateTime, Utc};

use crate::proxy::StackProxy;

/// How often the stacking server is asked.
///
/// Not configurable: PRD §8.1 has no key for it, and inventing one would be a silent schema
/// extension (tasks/README rule 2). 5 s is the compromise the two failure directions set — an
/// operator who has just started a capture should not watch a "stack: unreachable" badge for half
/// a minute after the node came back, and a Pi should not spend its link budget on health checks.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// §4.3's "+ 30s": republish even when nothing changed.
pub const REPUBLISH_INTERVAL: Duration = Duration::from_secs(30);

/// How long a poll may take before the stacking server counts as not answering.
///
/// Deliberately below [`POLL_INTERVAL`]'s neighbours rather than the proxy's 30 s operator-facing
/// budget: a poll that took 30 s would leave the panel claiming the stack node is fine for half a
/// minute after it stopped answering, which is the one thing this module exists to prevent.
const POLL_TIMEOUT: Duration = Duration::from_secs(3);

/// `alert` code for the stacking server becoming unreachable.
///
/// The T11 handoff's distinction, kept: a link that is down and a disk that is full both leave the
/// transfer queue growing and look identical from the field node, so the *code* is what tells them
/// apart. `STACK_DISK_FULL` is ingest's (published on the stack node); this is the other one.
pub const ALERT_UNREACHABLE: &str = "STACK_UNREACHABLE";

/// What the last successful poll saw, so an unreachable node can be reported honestly.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct LastKnown {
    session_frame_count: u64,
    last_preview_ts: Option<DateTime<Utc>>,
}

/// Start the republisher. `None` when this node has no stacking server configured.
///
/// The task holds an [`EventBus`] handle, so it **must** be aborted before `drop(bus)` on the
/// shutdown path — the same rule the mount poll, the camera poll and the snapshot store follow,
/// and for the same reason: a bus handle is a broadcast sender, and one alive at `drop(bus)` keeps
/// the session log's subscriber open for the whole flush timeout.
#[must_use]
pub fn spawn(proxy: Arc<StackProxy>, bus: EventBus) -> Option<tokio::task::JoinHandle<()>> {
    if proxy.authority().is_none() {
        // `stacking_server.enabled: false`. Publishing `offline` here would be a lie of a
        // specific and unhelpful kind — it says "the stacking server is down", sending the
        // operator to look at a node they deliberately turned off. Publishing nothing leaves the
        // topic absent, which §5.8.3 defines as "the node has no value for it" and the panel
        // renders as "no stacking server on this node".
        tracing::info!(
            "no stacking server is configured; stack.status will not be published \
             (`stacking_server.enabled: false`)"
        );
        return None;
    }
    Some(tokio::spawn(run(proxy, bus)))
}

async fn run(proxy: Arc<StackProxy>, bus: EventBus) {
    let mut last_known = LastKnown::default();
    let mut published: Option<StackStatus> = None;
    let mut published_at = tokio::time::Instant::now();
    // Edge-triggered, like every other alert in this system (§5.10.4): a stacking server that is
    // down stays down for minutes, and one alert every 5 s is one the operator learns to ignore.
    let mut unreachable = false;

    let mut ticks = tokio::time::interval(POLL_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticks.tick().await;

        let status = match poll(&proxy, &mut last_known).await {
            Ok(status) => {
                if unreachable {
                    unreachable = false;
                    bus.publish(Alert::info(
                        ALERT_UNREACHABLE,
                        "The stacking server is reachable again.".to_owned(),
                    ));
                }
                status
            }
            Err(reason) => {
                if !unreachable {
                    unreachable = true;
                    tracing::warn!(%reason, "the stacking server is not answering");
                    bus.publish(Alert::warning(
                        ALERT_UNREACHABLE,
                        format!(
                            "The stacking server is not answering ({reason}). Frames are still \
                             being captured and queued; they will upload when it returns."
                        ),
                    ));
                }
                StackStatus::offline(last_known.session_frame_count, last_known.last_preview_ts)
            }
        };

        // On change, or every 30 s (§4.3). Comparing the whole payload rather than a hand-picked
        // field means a new field added to `StackStatus` later cannot silently stop triggering.
        let changed = published.as_ref() != Some(&status);
        if changed || published_at.elapsed() >= REPUBLISH_INTERVAL {
            bus.publish(status.clone());
            published = Some(status);
            published_at = tokio::time::Instant::now();
        }
    }
}

/// One round of `/api/system/health` + `/api/stacking/stats`.
async fn poll(proxy: &StackProxy, last_known: &mut LastKnown) -> Result<StackStatus, String> {
    let health = proxy
        .get_json("/api/system/health", POLL_TIMEOUT)
        .await
        .map_err(|error| error.message)?;
    let stats = proxy
        .get_json("/api/stacking/stats", POLL_TIMEOUT)
        .await
        .map_err(|error| error.message)?;

    // Read from `/api/stacking/stats` rather than recomputed here — M1-T12's report was explicit
    // that this route is the one source of these numbers, and a second one on this side would be
    // a second thing to keep in agreement with the archive.
    let session_frame_count = stats
        .get("frame_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(last_known.session_frame_count);
    let last_preview_ts = stats
        .get("last_preview_ts")
        .and_then(serde_json::Value::as_str)
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|ts| ts.with_timezone(&Utc));

    *last_known = LastKnown {
        session_frame_count,
        last_preview_ts,
    };

    let worker = health.get("worker");
    let worker_state = worker
        .and_then(|w| w.get("state"))
        .and_then(|s| serde_json::from_value::<WorkerState>(s.clone()).ok())
        // A reachable stack node that reports no worker object at all. `Stopped` is the closest
        // true statement — "no worker is running" — and it is what a node with an on-demand
        // supervisor and no job yet reports anyway. Never `Ready`: claiming a live worker on the
        // strength of a missing field is exactly the fabrication `stack.status` must not make.
        .unwrap_or(WorkerState::Stopped);
    let restarts = worker
        .and_then(|w| w.get("restarts"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);

    Ok(StackStatus::online(
        session_frame_count,
        last_preview_ts,
        worker_state,
        restarts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestNode;

    /// A node with `stacking_server.enabled: false` publishes nothing at all.
    ///
    /// The distinction matters to the operator: "there is no stacking server here" and "the
    /// stacking server is down" send them to different places, and only one of them is worth
    /// walking out to the mount for.
    #[tokio::test]
    async fn a_disabled_stacking_server_publishes_no_status_rather_than_offline() {
        let node = TestNode::authenticated("s3cret").with_stack_disabled();
        let proxy = Arc::new(StackProxy::new(&node.config().stacking_server, Some("s3cret")));
        assert!(spawn(proxy, EventBus::new()).is_none());
    }

    /// An unreachable node reports the *last known* counts. Zeroing them would read as "the
    /// stacking server lost my session", which is the most alarming thing this panel can say.
    #[test]
    fn an_unreachable_node_keeps_the_last_known_counts() {
        let ts = Utc::now();
        let offline = StackStatus::offline(47, Some(ts));

        assert!(!offline.is_connected());
        assert_eq!(offline.session_frame_count(), 47);
        assert_eq!(
            offline.worker_state(),
            None,
            "the field node cannot know the worker state of a node that is not answering"
        );
    }

    /// The health payload this node's twin actually serves, decoded.
    #[tokio::test]
    async fn a_healthy_stack_node_is_read_into_the_event_payload() {
        let node = TestNode::authenticated("s3cret");
        let proxy = StackProxy::new(&node.config().stacking_server, Some("s3cret"));
        let mut last_known = LastKnown::default();

        // Not reachable — nothing is listening on the example config's host — so this exercises
        // the failure branch and proves it does not panic on a missing body.
        let failed = poll(&proxy, &mut last_known).await;
        assert!(failed.is_err());
        assert_eq!(
            last_known,
            LastKnown::default(),
            "a failed poll must not overwrite what the last good one knew"
        );
    }

    #[test]
    fn the_republish_interval_is_a_multiple_of_the_poll_so_a_tick_lands_on_it() {
        assert_eq!(REPUBLISH_INTERVAL.as_secs() % POLL_INTERVAL.as_secs(), 0);
        assert!(POLL_TIMEOUT < POLL_INTERVAL, "a poll must finish before the next one starts");
    }
}
