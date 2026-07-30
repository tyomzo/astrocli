//! The `/ws` event hub — SDD §5.8.3, §4.3, §4.5, §8.3.
//!
//! One JSON socket carrying control and status events. Binary frames belong on `/ws/liveview`
//! (§8.3(5), M1-T09) and nothing here ever sends one.
//!
//! # The two frame shapes
//!
//! §4.3 requires the WS frame and the session-log line to be the *identical* serialization of
//! [`Event`], so an event frame is exactly `{v, ts, topic, data}` and has nowhere to put an
//! envelope. §5.8.3 additionally requires a connect snapshot and a `ping` answer, neither of
//! which is an event. They are therefore **control frames carrying `type` where an event carries
//! `topic`**, and the two are told apart by which key is present. Nothing carries both.
//!
//! That convention is not in the SDD. It was proposed by M1-T04's `frontend/mock/README.md`,
//! the PWA is written and tested against it, and this module adopts it unchanged.
//!
//! # `ping` is an application message, not RFC 6455
//!
//! §5.8.3 says "the hub answers `ping` frames immediately". That cannot mean a protocol-level
//! control frame: the browser `WebSocket` API has no way to *send* a ping or to observe a pong,
//! so a client that could only be answered at the protocol level could never ask. Ping is
//! therefore `{"type":"ping","id":n}` and the answer echoes the id.
//!
//! # Default subscription is every topic
//!
//! §5.8.3 says subscribe messages "filter topics server-side" and does not say what a client
//! that sends none receives. The answer has to be everything: the PWA sends no subscribe
//! message at all, so a hub that required one would deliver nothing to the only client there is.
//! `subscribe` narrows; `unsubscribe` narrows further.
//!
//! # Why one client's slowness cannot become another's, or the bus's
//!
//! Three tasks per connection, each with exactly one thing it waits on:
//!
//! | Task | Waits on | Never waits on |
//! |---|---|---|
//! | pump | the event bus | the socket |
//! | writer | the outbox, then the socket | the bus |
//! | reader | the socket's read half | anything else |
//!
//! The pump exists because of the failure it prevents. If the task that reads the broadcast also
//! wrote to the socket, a client stalled mid-write would stop reading the bus, fall behind
//! [`EVENT_BUS_CAPACITY`](astroctl_core::bus::EVENT_BUS_CAPACITY), and start losing events *at
//! the broadcast* — where the loss is silent and hits discrete events the hub is required never
//! to drop. Draining the bus into a per-client [`Outbox`] moves the bound to a place that can
//! coalesce positions and close the connection when discrete events really do pile up.
//!
//! None of the three uses `select!`, so no future is ever cancelled mid-frame and no reasoning
//! about the cancel-safety of a half-read WebSocket message is required.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use astroctl_core::bus::Recv;
use astroctl_core::event::{Event, Topic, EVENT_SCHEMA_VERSION};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::stream::StreamExt;
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::api::AppState;
use crate::auth::unauthorized;
use crate::ticket::TicketRejection;

/// Discrete events a client may fall behind by before it is disconnected (SDD §5.8.3: "64
/// events with a latest-only slot for `mount.position`").
///
/// Overflow closes the socket rather than dropping an event, which is §4.3's rule ("bounded
/// per-client queue, close on overflow") and is the more honest of the two failures: the PWA
/// reconnects and resnapshots, so the operator sees current state. Dropping one discrete event
/// would leave them looking at a panel that is quietly missing a frame or an alert with nothing
/// anywhere to say so.
const CLIENT_QUEUE_DEPTH: usize = 64;

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/// A control frame — the `type`-carrying half of the wire.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlFrame {
    /// The first frame after every upgrade (§5.8.3).
    Snapshot {
        v: u16,
        ts: String,
        events: Vec<Event>,
    },
    /// The answer to an application-level ping.
    Pong {
        v: u16,
        ts: String,
        /// Echoed from the request, so a client can match an answer to a question and derive
        /// RTT rather than "time since some pong".
        id: i64,
        /// The node's clock, which is what §5.8.1's skew correction is measured against.
        server_time: String,
    },
}

/// A frame from the client (§5.8.3's subscribe/unsubscribe, plus ping).
///
/// Unknown `type` values are ignored rather than fatal: a client newer than this node is a
/// normal condition after an upgrade, and dropping the connection over a message we simply do
/// not implement would turn a forward-compatible client into a reconnect loop.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    Ping { id: i64 },
    Subscribe { topics: Vec<String> },
    Unsubscribe { topics: Vec<String> },
}

/// RFC 3339 with milliseconds, the format SDD §2 fixes for every timestamp on the wire.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------------------------
// Snapshot store
// ---------------------------------------------------------------------------------------------

/// The latest event on each stateful topic, for the connect snapshot (§5.8.3).
///
/// Fed by one task per node rather than assembled on demand by querying every subsystem: a
/// snapshot built by asking the mount, the camera and the transfer agent in turn would take as
/// long as the slowest of them and could observe a state no single instant ever had.
#[derive(Debug, Default)]
pub struct SnapshotStore {
    latest: Mutex<Vec<(Topic, Event)>>,
}

impl SnapshotStore {
    /// An empty store — the state of a node that has published nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an event if its topic is stateful.
    fn record(&self, event: &Event) {
        if !event.topic.is_stateful() {
            return;
        }
        let mut latest = self.lock();
        if let Some(slot) = latest.iter_mut().find(|(topic, _)| *topic == event.topic) {
            slot.1 = event.clone();
        } else {
            latest.push((event.topic, event.clone()));
        }

        // A disconnected mount has no position, and saying so means *removing* the topic rather
        // than publishing a null one. The PWA reduces a topic absent from the snapshot back to
        // unknown, so this is what makes a client that reconnects after the mount was unplugged
        // show no coordinates instead of the ones from before the drop — §5.9's "resnapshot
        // rather than resume from a hole", and `telemetry.test.ts` asserts it.
        if event.topic == Topic::MountStatus && mount_status_is_disconnected(event) {
            latest.retain(|(topic, _)| *topic != Topic::MountPosition);
        }
    }

    /// The snapshot, in §4.3 table order so two clients connecting at the same instant get
    /// byte-identical frames.
    fn snapshot(&self) -> Vec<Event> {
        let latest = self.lock();
        Topic::ALL
            .into_iter()
            .filter_map(|topic| {
                latest
                    .iter()
                    .find(|(candidate, _)| *candidate == topic)
                    .map(|(_, event)| event.clone())
            })
            .collect()
    }

    /// See [`crate::ticket::TicketStore::lock`] for why poisoning is recovered rather than
    /// propagated: one unrelated panic must not permanently disable every future connection.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(Topic, Event)>> {
        self.latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether a `mount.status` event reports a disconnected mount.
///
/// Reads the serialized payload rather than a typed struct because the store holds `Event`s,
/// whose `data` is deliberately `serde_json::Value` (§4.3: one struct, one serialization). A
/// payload that cannot be read is treated as "not disconnected", which errs toward keeping a
/// position in the snapshot — the failure mode is a stale coordinate for one poll interval,
/// against dropping a live one.
fn mount_status_is_disconnected(event: &Event) -> bool {
    event.data.get("state").and_then(serde_json::Value::as_str) == Some("disconnected")
}

/// Keep `store` current from the bus. Runs until the bus closes at shutdown.
pub async fn maintain_snapshot(
    store: Arc<SnapshotStore>,
    mut events: astroctl_core::bus::EventSubscriber,
) {
    loop {
        match events.recv().await {
            Recv::Event(event) => store.record(&event),
            // Lagging here means the snapshot missed an intermediate value, not that it is
            // wrong: the next event on each topic replaces it. Nothing to resync.
            Recv::Lagged { skipped } => {
                tracing::debug!(skipped, "snapshot store fell behind the bus");
            }
            Recv::Closed => break,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Per-client outbox
// ---------------------------------------------------------------------------------------------

/// What the writer task pulls out of the outbox.
#[derive(Debug)]
enum Outgoing {
    /// A pre-serialized frame.
    Text(String),
    /// The client fell too far behind; send nothing more and close.
    Overflowed,
}

/// The bounded, coalescing queue of SDD §5.8.3.
#[derive(Debug)]
struct Outbox {
    state: Mutex<OutboxState>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct OutboxState {
    /// Discrete events and control frames, in order. Bounded at [`CLIENT_QUEUE_DEPTH`].
    discrete: VecDeque<String>,
    /// The latest-only slot. A position that has not been sent when a newer one arrives is
    /// replaced, because a stale coordinate has no value once a newer one exists — whereas a
    /// dropped `frame.saved` is information nothing can recover.
    position: Option<String>,
    overflowed: bool,
    closed: bool,
}

impl Outbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(OutboxState::default()),
            notify: Notify::new(),
        }
    }

    /// Queue an event, coalescing it if its topic says so.
    ///
    /// Returns `false` once the client has overflowed, so the pump can stop serializing frames
    /// for a connection that is already being torn down.
    fn push_event(&self, event: &Event) -> bool {
        let Ok(frame) = serde_json::to_string(event) else {
            // Unreachable for the payloads in §4.3 — they are plain structs of primitives. Not
            // an `expect`: a panic in the pump would take down a task holding no lock anyone
            // else needs, but the node would silently stop feeding that client.
            tracing::error!(topic = %event.topic, "event failed to serialize; dropped");
            return true;
        };
        self.push(frame, event.topic.is_coalescable())
    }

    /// Queue a control frame. Never coalesced — a snapshot and a pong are both one-of-a-kind.
    fn push_control(&self, frame: String) -> bool {
        self.push(frame, false)
    }

    fn push(&self, frame: String, coalescable: bool) -> bool {
        {
            let mut state = self.lock();
            if state.closed || state.overflowed {
                return false;
            }
            if coalescable {
                state.position = Some(frame);
            } else {
                if state.discrete.len() >= CLIENT_QUEUE_DEPTH {
                    // §4.3: close on overflow. The queue is left as it is; the writer sees the
                    // flag and closes without draining, because a client that could not keep up
                    // with 64 events will not catch up on the way out.
                    state.overflowed = true;
                    self.notify.notify_one();
                    return false;
                }
                state.discrete.push_back(frame);
            }
        }
        self.notify.notify_one();
        true
    }

    /// Mark the connection finished, waking the writer so it can exit.
    fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_one();
    }

    /// Take the next frame, waiting for one if the outbox is empty.
    ///
    /// Discrete frames go first. Under the bound that is a queue of at most 64, so a position
    /// cannot be starved for long; over it the connection is closing anyway.
    async fn recv(&self) -> Option<Outgoing> {
        loop {
            // The future is created *before* the state is inspected. The other order loses a
            // notification delivered between the check and the await, and the client then sits
            // silent until the next event happens to arrive.
            let notified = self.notify.notified();
            {
                let mut state = self.lock();
                if state.overflowed {
                    return Some(Outgoing::Overflowed);
                }
                if let Some(frame) = state.discrete.pop_front() {
                    return Some(Outgoing::Text(frame));
                }
                if let Some(frame) = state.position.take() {
                    return Some(Outgoing::Text(frame));
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, OutboxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ---------------------------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------------------------

/// Which topics a client wants. Empty set means *all* — see the module docs.
#[derive(Debug)]
struct Subscriptions {
    /// `None` is the default-everything state, distinct from an empty explicit set (which a
    /// client reaches by unsubscribing from everything and which correctly delivers nothing).
    selected: Mutex<Option<HashSet<Topic>>>,
}

impl Subscriptions {
    fn all() -> Self {
        Self {
            selected: Mutex::new(None),
        }
    }

    fn wants(&self, topic: Topic) -> bool {
        match &*self.lock() {
            None => true,
            Some(set) => set.contains(&topic),
        }
    }

    fn subscribe(&self, topics: &[String]) {
        let mut guard = self.lock();
        // The first `subscribe` narrows from everything; subsequent ones add.
        let set = guard.get_or_insert_with(HashSet::new);
        for name in topics {
            if let Some(topic) = topic_by_name(name) {
                set.insert(topic);
            }
        }
    }

    fn unsubscribe(&self, topics: &[String]) {
        let mut guard = self.lock();
        // Unsubscribing from a client that never subscribed starts from everything, which is
        // what it was receiving.
        let set = guard.get_or_insert_with(|| Topic::ALL.into_iter().collect());
        for name in topics {
            if let Some(topic) = topic_by_name(name) {
                set.remove(&topic);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<HashSet<Topic>>> {
        self.selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Resolve a wire topic name, ignoring anything this build does not know.
///
/// An unknown name is not an error for the same reason an unknown frame `type` is not: a client
/// built against a newer node may name a topic that does not exist here yet, and refusing the
/// whole message would be a worse answer than honouring the names we recognise.
fn topic_by_name(name: &str) -> Option<Topic> {
    Topic::ALL.into_iter().find(|topic| topic.as_str() == name)
}

// ---------------------------------------------------------------------------------------------
// The upgrade
// ---------------------------------------------------------------------------------------------

/// The `?ticket=` query parameter of SDD §4.5.
///
/// Shared with `/ws/liveview` (`crate::liveview::upgrade`) rather than declared twice: §4.5 makes
/// the ticket the only way a browser authenticates *either* socket, and two extractors would be
/// two places for one of them to grow a second accepted spelling.
#[derive(Debug, Deserialize)]
pub struct UpgradeParams {
    #[serde(default)]
    ticket: Option<String>,
}

impl UpgradeParams {
    /// The presented ticket, if the caller sent one.
    #[must_use]
    pub fn ticket(&self) -> Option<&str> {
        self.ticket.as_deref()
    }
}

/// `GET /ws` — SDD §5.8.1, authenticated by ticket (§4.5), **not** by a bearer header.
///
/// # Why a bearer header is not accepted here even when one is present
///
/// §4.5 makes the ticket the only way a browser authenticates this socket. Accepting a header as
/// well would mean the PWA could pass its ticket handling being broken — the header would carry
/// it — and the bug would surface later, on a real phone, as a socket that never opens. The mock
/// refuses it for the same reason and the PWA's tests depend on it.
///
/// This route is deliberately merged *outside* the bearer layer (see `crate::assemble`). It is
/// not an exemption bolted onto the auth middleware: it is a different authentication scheme for
/// one route, and keeping it structural means nothing can accidentally extend it to a second.
pub async fn upgrade(
    State(state): State<AppState>,
    Query(params): Query<UpgradeParams>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(rejection) = state.tickets.consume(params.ticket.as_deref()) {
        // The reason is logged, never returned. Telling a caller which of "unknown", "expired"
        // and "already spent" their guess was is an oracle for guessing the next one.
        tracing::info!(reason = rejection.reason(), "WS upgrade refused");
        return unauthorized(&astroctl_core::error::ApiError::new(
            astroctl_core::error::ErrorCode::Auth,
            TicketRejection::MESSAGE,
        ));
    }

    // Subscribe and snapshot *here*, before the upgrade completes, and hand the connection only
    // those two values — never the `AppState` it came from.
    //
    // That is a shutdown requirement, not tidiness. `AppState` holds an `EventBus`, which is a
    // broadcast *sender*, so a connection holding one keeps the channel open for as long as the
    // client stays attached. `main`'s shutdown drops every sender in order to close the session
    // log's subscriber and flush it; one live phone would have been enough to make that flush
    // time out and lose the tail of the night's event log. The hub only ever reads the bus, so
    // it has no business holding something that can publish to it.
    //
    // Subscribing before taking the snapshot is the other ordering that matters: the reverse has
    // a hole exactly one scheduling gap wide, and an event published inside it would be in
    // neither the snapshot nor the stream — which for `mount.status` means a stale badge for the
    // rest of the night.
    let events = state.bus.subscribe();
    let snapshot = ControlFrame::Snapshot {
        v: EVENT_SCHEMA_VERSION,
        ts: now_rfc3339(),
        events: state.snapshots.snapshot(),
    };

    upgrade.on_upgrade(move |socket| serve(socket, events, snapshot))
}

/// Drive one client for the life of its connection.
async fn serve(
    socket: WebSocket,
    events: astroctl_core::bus::EventSubscriber,
    snapshot: ControlFrame,
) {
    let outbox = Arc::new(Outbox::new());
    let subscriptions = Arc::new(Subscriptions::all());

    match serde_json::to_string(&snapshot) {
        Ok(frame) => {
            outbox.push_control(frame);
        }
        Err(error) => {
            tracing::error!(%error, "snapshot failed to serialize");
            return;
        }
    }

    let (sink, stream) = socket.split();

    let pump = tokio::spawn(pump(
        Arc::clone(&outbox),
        Arc::clone(&subscriptions),
        events,
    ));
    let writer = tokio::spawn(write(Arc::clone(&outbox), sink));
    let reader = tokio::spawn(read(Arc::clone(&outbox), subscriptions, stream));

    // The reader finishing means the client went away — a close frame, a transport error, or a
    // dead tunnel. That is the one event that ends a connection from this side, so everything
    // else is torn down behind it.
    let _ = reader.await;
    outbox.close();
    pump.abort();
    let _ = writer.await;
}

/// Bus → outbox. Never touches the socket; see the module docs for why that is the point.
async fn pump(
    outbox: Arc<Outbox>,
    subscriptions: Arc<Subscriptions>,
    mut events: astroctl_core::bus::EventSubscriber,
) {
    loop {
        match events.recv().await {
            Recv::Event(event) => {
                if subscriptions.wants(event.topic) && !outbox.push_event(&event) {
                    // Overflowed or closed; the writer is already tearing the connection down.
                    return;
                }
            }
            Recv::Lagged { skipped } => {
                // The bus itself outran this client — a different failure from the per-client
                // bound, and one this connection cannot fix by catching up. §4.3's rule is to
                // drop the consumer rather than apply backpressure to the bus, and the PWA
                // reconnects and resnapshots, which is the only way it gets a consistent view
                // again.
                tracing::warn!(skipped, "WS client fell behind the event bus; closing");
                outbox.close();
                return;
            }
            Recv::Closed => {
                outbox.close();
                return;
            }
        }
    }
}

/// Outbox → socket. The only task that ever waits on a write.
async fn write(outbox: Arc<Outbox>, mut sink: futures_util::stream::SplitSink<WebSocket, Message>) {
    while let Some(item) = outbox.recv().await {
        match item {
            Outgoing::Text(frame) => {
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    // The peer is gone. The reader will notice too and end the connection; there
                    // is nothing useful to say about a socket that has already closed.
                    break;
                }
            }
            Outgoing::Overflowed => {
                tracing::warn!(
                    depth = CLIENT_QUEUE_DEPTH,
                    "WS client exceeded its queue depth; closing so it resnapshots"
                );
                break;
            }
        }
    }
    let _ = sink.close().await;
}

/// Socket → ping answers and subscription changes.
///
/// A plain `while let` over the stream rather than a `select!`, so there is no branch that could
/// cancel a partially-read frame. Everything this task produces goes into the outbox for the
/// writer to send, which is what keeps a stalled write from blocking a ping answer.
async fn read(
    outbox: Arc<Outbox>,
    subscriptions: Arc<Subscriptions>,
    mut stream: futures_util::stream::SplitStream<WebSocket>,
) {
    while let Some(message) = stream.next().await {
        let Ok(message) = message else { break };
        match message {
            Message::Text(text) => {
                let Ok(frame) = serde_json::from_str::<ClientFrame>(&text) else {
                    // Unknown or malformed: ignored, not fatal. See `ClientFrame`.
                    continue;
                };
                match frame {
                    ClientFrame::Ping { id } => {
                        let now = now_rfc3339();
                        let pong = ControlFrame::Pong {
                            v: EVENT_SCHEMA_VERSION,
                            ts: now.clone(),
                            id,
                            server_time: now,
                        };
                        if let Ok(encoded) = serde_json::to_string(&pong) {
                            if !outbox.push_control(encoded) {
                                break;
                            }
                        }
                    }
                    ClientFrame::Subscribe { topics } => subscriptions.subscribe(&topics),
                    ClientFrame::Unsubscribe { topics } => subscriptions.unsubscribe(&topics),
                }
            }
            Message::Close(_) => break,
            // A browser cannot send a protocol ping, but a `wscat` or a proxy can. axum answers
            // protocol pings itself; nothing is needed here.
            Message::Ping(_) | Message::Pong(_) => {}
            // Binary belongs on `/ws/liveview` (§8.3(5)). Ignored rather than fatal: it is not
            // worth dropping an operator's telemetry link over a frame we simply do not read.
            Message::Binary(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    // `event::MountState` deliberately, not `types::MountState`: the wire enum is the
    // five-value one, and mixing them up is exactly what `to_wire_state` exists to prevent.
    use astroctl_core::event::{
        Alert, EventPayload, MountPosition, MountState, MountStatus, PierSide,
    };

    use super::*;

    fn position_event(ra: f64) -> Event {
        MountPosition::new(ra, 0.0, None, None, PierSide::Unknown).into_event()
    }

    fn alert_event(code: &str) -> Event {
        Alert::info(code, "test").into_event()
    }

    #[tokio::test]
    async fn positions_coalesce_while_discrete_events_all_survive() {
        // The §5.8.3 acceptance criterion, and the reason the outbox is not an `mpsc`. The
        // client here is stalled in the strongest sense available: nothing is calling `recv`
        // while the events arrive.
        let outbox = Outbox::new();

        for i in 0..500 {
            assert!(outbox.push_event(&position_event(f64::from(i) / 100.0)));
        }
        for i in 0..10 {
            assert!(outbox.push_event(&alert_event(&format!("ALERT_{i}"))));
        }

        let mut positions = 0;
        let mut alerts = Vec::new();
        while let Some(Outgoing::Text(frame)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), outbox.recv())
                .await
                .unwrap_or(None)
        {
            let value: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
            match value["topic"].as_str() {
                Some("mount.position") => positions += 1,
                Some("alert") => alerts.push(value["data"]["code"].as_str().unwrap().to_owned()),
                other => panic!("unexpected frame {other:?}"),
            }
        }

        assert_eq!(
            positions, 1,
            "500 positions must coalesce to the one that is still true"
        );
        assert_eq!(
            alerts,
            (0..10).map(|i| format!("ALERT_{i}")).collect::<Vec<_>>(),
            "every discrete event, in order, none dropped"
        );
    }

    #[tokio::test]
    async fn the_surviving_position_is_the_newest_one() {
        // Coalescing that kept the *oldest* would be worse than no coalescing: the operator
        // would watch a stale coordinate while the mount moved.
        let outbox = Outbox::new();
        for i in 0..100 {
            outbox.push_event(&position_event(f64::from(i)));
        }
        let Some(Outgoing::Text(frame)) = outbox.recv().await else {
            panic!("expected a frame");
        };
        let value: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert!((value["data"]["ra"].as_f64().expect("ra") - 99.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn a_client_that_exceeds_the_queue_depth_is_closed_rather_than_silently_trimmed() {
        // §4.3: "bounded per-client queue, close on overflow". Silently dropping the 65th event
        // would leave a panel missing a frame with nothing anywhere to say so.
        let outbox = Outbox::new();
        for i in 0..CLIENT_QUEUE_DEPTH {
            assert!(
                outbox.push_event(&alert_event(&format!("A{i}"))),
                "the queue must accept exactly its depth"
            );
        }
        assert!(
            !outbox.push_event(&alert_event("OVERFLOW")),
            "the event past the bound must be refused, not queued"
        );
        assert!(matches!(outbox.recv().await, Some(Outgoing::Overflowed)));
    }

    #[tokio::test]
    async fn coalescing_alone_never_overflows_a_client() {
        // The practical consequence of the latest-only slot: a phone that stalls for ten minutes
        // on a bad tunnel accumulates one position, not six hundred, so 1 Hz telemetry cannot by
        // itself be what closes the socket.
        let outbox = Outbox::new();
        for i in 0..10_000 {
            assert!(outbox.push_event(&position_event(f64::from(i) / 1000.0)));
        }
    }

    #[test]
    fn a_disconnected_mount_leaves_no_position_in_the_snapshot() {
        // The PWA reduces an absent topic back to unknown, so this is what makes a reconnecting
        // client show no coordinates instead of the ones from before the mount was unplugged.
        let store = SnapshotStore::new();
        store.record(&MountStatus::new(MountState::Idle, None).into_event());
        store.record(&position_event(5.5));
        assert_eq!(store.snapshot().len(), 2);

        store.record(&MountStatus::disconnected().into_event());
        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].topic, Topic::MountStatus);
    }

    #[test]
    fn occurrences_never_enter_the_snapshot() {
        // Replaying an alert on every reconnect shows the operator a warning they already dealt
        // with; on a link that reconnects as often as this one, that is a stream of stale news.
        let store = SnapshotStore::new();
        store.record(&alert_event("DISK_LOW"));
        store.record(
            &astroctl_core::event::FrameSaved::new("light_00001", "/tmp/f", 1, "ab").into_event(),
        );
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn the_snapshot_keeps_only_the_latest_event_per_topic_in_table_order() {
        let store = SnapshotStore::new();
        store.record(&position_event(1.0));
        store.record(&MountStatus::new(MountState::Idle, None).into_event());
        store.record(&position_event(2.0));

        let snapshot = store.snapshot();
        assert_eq!(
            snapshot.iter().map(|e| e.topic).collect::<Vec<_>>(),
            vec![Topic::MountPosition, Topic::MountStatus],
            "§4.3 table order, so two clients connecting at once get identical frames"
        );
        assert!((snapshot[0].data["ra"].as_f64().expect("ra") - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_default_subscription_is_every_topic() {
        // The PWA sends no subscribe message at all, so a hub that required one would deliver
        // nothing to the only client there is.
        let subs = Subscriptions::all();
        for topic in Topic::ALL {
            assert!(subs.wants(topic), "{topic} must be on by default");
        }
    }

    #[test]
    fn subscribe_narrows_and_unsubscribe_narrows_further() {
        let subs = Subscriptions::all();
        subs.subscribe(&["mount.position".to_owned(), "alert".to_owned()]);
        assert!(subs.wants(Topic::MountPosition));
        assert!(subs.wants(Topic::Alert));
        assert!(!subs.wants(Topic::SystemHealth));

        subs.unsubscribe(&["alert".to_owned()]);
        assert!(subs.wants(Topic::MountPosition));
        assert!(!subs.wants(Topic::Alert));
    }

    #[test]
    fn unsubscribing_first_starts_from_everything() {
        // A client that never subscribed is receiving everything, so removing one topic must
        // leave the other nine — not leave it with nothing.
        let subs = Subscriptions::all();
        subs.unsubscribe(&["mount.position".to_owned()]);
        assert!(!subs.wants(Topic::MountPosition));
        assert!(subs.wants(Topic::MountStatus));
        assert!(subs.wants(Topic::Alert));
    }

    #[test]
    fn an_unknown_topic_name_is_ignored_rather_than_fatal() {
        // A client built against a newer node names topics this build does not have.
        let subs = Subscriptions::all();
        subs.subscribe(&["mount.position".to_owned(), "weather.seeing".to_owned()]);
        assert!(subs.wants(Topic::MountPosition));
        assert!(!subs.wants(Topic::Alert));
    }

    #[test]
    fn control_frames_carry_type_and_events_carry_topic_and_never_both() {
        // The one part of the wire the SDD does not pin down. The PWA tells the two apart by
        // which key is present, so a control frame that grew a `topic` — or an event that grew a
        // `type` — would be parsed as the wrong kind entirely.
        let snapshot = serde_json::to_value(ControlFrame::Snapshot {
            v: EVENT_SCHEMA_VERSION,
            ts: "2026-07-30T21:04:05.123Z".to_owned(),
            events: vec![position_event(1.0)],
        })
        .expect("serializes");
        assert_eq!(snapshot["type"], "snapshot");
        assert!(snapshot.get("topic").is_none());
        assert_eq!(snapshot["v"], 1);
        assert_eq!(snapshot["events"][0]["topic"], "mount.position");
        assert!(snapshot["events"][0].get("type").is_none());

        let pong = serde_json::to_value(ControlFrame::Pong {
            v: EVENT_SCHEMA_VERSION,
            ts: "2026-07-30T21:04:05.123Z".to_owned(),
            id: 42,
            server_time: "2026-07-30T21:04:05.123Z".to_owned(),
        })
        .expect("serializes");
        assert_eq!(pong["type"], "pong");
        assert_eq!(pong["id"], 42);
        assert!(pong.get("topic").is_none());
    }

    #[test]
    fn the_client_frame_vocabulary_is_the_three_the_pwa_sends() {
        let ping: ClientFrame = serde_json::from_str(r#"{"type":"ping","id":7}"#).expect("ping");
        assert!(matches!(ping, ClientFrame::Ping { id: 7 }));

        let subscribe: ClientFrame =
            serde_json::from_str(r#"{"type":"subscribe","topics":["alert"]}"#).expect("subscribe");
        assert!(matches!(subscribe, ClientFrame::Subscribe { .. }));

        let unsubscribe: ClientFrame =
            serde_json::from_str(r#"{"type":"unsubscribe","topics":[]}"#).expect("unsubscribe");
        assert!(matches!(unsubscribe, ClientFrame::Unsubscribe { .. }));

        // A frame this build does not implement must not be fatal — see `ClientFrame`.
        assert!(serde_json::from_str::<ClientFrame>(r#"{"type":"resume","from":3}"#).is_err());
    }
}

/// End-to-end tests against a bound port and a real RFC 6455 client.
///
/// Everything here needs a completed upgrade, which `tower`'s `oneshot` cannot produce: it
/// drives the `Service` and never hands the connection back, so the handshake has nowhere to go.
/// These bind an ephemeral port and drive the assembled app — the same `crate::assemble` the
/// binary calls — with `tokio-tungstenite` as the client.
#[cfg(test)]
mod e2e {
    use std::net::SocketAddr;

    use astroctl_core::event::{Alert, EventPayload, MountPosition, PierSide};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    use crate::api::AppState;
    use crate::test_support::{state_with, TestNode};

    /// A node listening on an ephemeral port, plus the state its handlers share.
    struct Node {
        addr: SocketAddr,
        state: AppState,
    }

    impl Node {
        async fn start() -> Self {
            let node = TestNode::open_loopback();
            let (router, declarations) = crate::api::router();
            let (ws_router, ws_declarations) = crate::api::ws_router();
            let state = state_with(
                &node,
                declarations.into_iter().chain(ws_declarations).collect(),
            )
            .await;
            let app = crate::assemble(router, ws_router, state.clone());

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("binds an ephemeral port");
            let addr = listener.local_addr().expect("has a local address");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self { addr, state }
        }

        fn url(&self, ticket: &str) -> String {
            format!("ws://{}/ws?ticket={ticket}", self.addr)
        }
    }

    /// Connect, and return the socket plus the first frame the node sent.
    async fn connect(
        node: &Node,
        ticket: &str,
    ) -> Result<
        (
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            serde_json::Value,
        ),
        String,
    > {
        let (mut socket, _) = tokio_tungstenite::connect_async(node.url(ticket))
            .await
            .map_err(|e| e.to_string())?;
        let first = socket
            .next()
            .await
            .ok_or_else(|| "the socket closed before sending anything".to_owned())?
            .map_err(|e| e.to_string())?;
        let ClientMessage::Text(text) = first else {
            return Err(format!("the first frame was not text: {first:?}"));
        };
        let value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok((socket, value))
    }

    #[tokio::test]
    async fn a_valid_ticket_is_accepted_and_replaying_it_is_not() {
        // SDD §4.5's single-use rule, end to end: the ticket is consumed *by the upgrade*, so
        // this is the only place the rule can actually be observed.
        let node = Node::start().await;
        let ticket = node.state.tickets.issue().expect("issues");

        let (mut socket, _) = connect(&node, ticket.as_str())
            .await
            .expect("a fresh ticket must be accepted");
        let _ = socket.close(None).await;

        let replay = connect(&node, ticket.as_str()).await;
        assert!(
            replay.is_err(),
            "a replayed ticket must be refused; the upgrade succeeded instead"
        );
    }

    #[tokio::test]
    async fn no_ticket_and_an_unknown_ticket_are_both_refused() {
        // The bearer layer cannot cover this route, so if the ticket check were missing the
        // socket would simply open. That is the failure this asserts against.
        let node = Node::start().await;

        assert!(
            tokio_tungstenite::connect_async(format!("ws://{}/ws", node.addr))
                .await
                .is_err(),
            "an upgrade with no ticket must be refused"
        );
        assert!(
            connect(&node, "00000000000000000000000000000000")
                .await
                .is_err(),
            "an upgrade with a ticket that was never issued must be refused"
        );
    }

    #[tokio::test]
    async fn a_bearer_header_does_not_substitute_for_a_ticket() {
        // §4.5 makes the ticket the *only* way a browser authenticates this socket. Accepting a
        // header as well would let a broken ticket path pass on a desktop client and fail on the
        // phone it was written for. The mock refuses it for the same reason.
        let node = Node::start().await;
        let request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{}/ws", node.addr),
            );
        let mut request = request.expect("builds");
        request.headers_mut().insert(
            "authorization",
            "Bearer s3cret".parse().expect("valid header"),
        );
        assert!(
            tokio_tungstenite::connect_async(request).await.is_err(),
            "a bearer header must not authenticate /ws"
        );
    }

    #[tokio::test]
    async fn the_snapshot_is_the_first_frame_and_carries_current_state() {
        // §5.8.3: "on `/ws` connect the hub sends a state snapshot ... so the UI never renders
        // from partial state". First frame, not eventually.
        let node = Node::start().await;
        node.state
            .snapshots
            .record(&MountPosition::new(5.5, 22.0, None, None, PierSide::Unknown).into_event());

        let ticket = node.state.tickets.issue().expect("issues");
        let (_socket, first) = connect(&node, ticket.as_str()).await.expect("connects");

        assert_eq!(
            first["type"], "snapshot",
            "the first frame must be the snapshot"
        );
        assert!(
            first.get("topic").is_none(),
            "a control frame carries no topic"
        );
        assert_eq!(first["v"], 1);
        let events = first["events"].as_array().expect("events is a list");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["topic"], "mount.position");
        assert_eq!(events[0]["data"]["ra"], 5.5);
        assert_eq!(
            events[0]["data"]["alt"],
            serde_json::Value::Null,
            "alt/az are null until M1-T05's topocentric transform"
        );
    }

    #[tokio::test]
    async fn an_event_published_after_the_snapshot_arrives_on_the_socket() {
        // The other half of "snapshot first": everything after it is live.
        let node = Node::start().await;
        let ticket = node.state.tickets.issue().expect("issues");
        let (mut socket, first) = connect(&node, ticket.as_str()).await.expect("connects");
        assert_eq!(first["type"], "snapshot");

        node.state.bus.publish(Alert::info("TEST_CODE", "hello"));

        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("an event should arrive")
            .expect("the socket stays open")
            .expect("a readable frame");
        let ClientMessage::Text(text) = frame else {
            panic!("expected text, got {frame:?}");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["topic"], "alert", "an event frame carries `topic`");
        assert!(
            value.get("type").is_none(),
            "an event frame carries no `type`"
        );
        assert_eq!(value["data"]["code"], "TEST_CODE");
    }

    #[tokio::test]
    async fn ping_is_answered_immediately_with_the_same_id() {
        // §5.8.3's "the hub answers `ping` frames immediately", at the application level —
        // the browser WebSocket API cannot send or observe an RFC 6455 ping, so a protocol-level
        // answer would be one no browser could ever ask for.
        let node = Node::start().await;
        let ticket = node.state.tickets.issue().expect("issues");
        let (mut socket, _) = connect(&node, ticket.as_str()).await.expect("connects");

        socket
            .send(ClientMessage::Text(r#"{"type":"ping","id":4242}"#.into()))
            .await
            .expect("sends");

        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("a pong should arrive")
            .expect("the socket stays open")
            .expect("a readable frame");
        let ClientMessage::Text(text) = frame else {
            panic!("expected text, got {frame:?}");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["type"], "pong");
        assert_eq!(value["id"], 4242, "the id must be echoed, not regenerated");
        assert!(value["server_time"].is_string());
    }

    #[tokio::test]
    async fn a_client_that_sends_no_subscribe_still_receives_every_topic() {
        // The PWA sends no subscribe message at all. A hub that required one would deliver
        // nothing to the only client there is.
        let node = Node::start().await;
        let ticket = node.state.tickets.issue().expect("issues");
        let (mut socket, _) = connect(&node, ticket.as_str()).await.expect("connects");

        node.state
            .bus
            .publish(MountPosition::new(1.0, 2.0, None, None, PierSide::Unknown));
        node.state.bus.publish(Alert::info("SECOND", "also"));

        let mut topics = Vec::new();
        for _ in 0..2 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .expect("a frame should arrive")
                .expect("the socket stays open")
                .expect("a readable frame");
            if let ClientMessage::Text(text) = frame {
                let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
                if let Some(topic) = value["topic"].as_str() {
                    topics.push(topic.to_owned());
                }
            }
        }
        assert!(topics.contains(&"mount.position".to_owned()));
        assert!(topics.contains(&"alert".to_owned()));
    }
}
