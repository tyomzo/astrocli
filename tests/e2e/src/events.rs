//! `/ws` — the JSON event socket, recorded.
//!
//! The recorder keeps every frame with the [`Instant`] it arrived, because most of what this suite
//! asserts about events is about *when*: `mount.position` at 1 Hz with no gap over 1.5 s (T-ISO-1),
//! one alert per transition and not one per attempt (§5.10.2), a preview inside ten seconds of the
//! capture that produced it (T-E2E-1). A recorder that kept only the payloads could answer none of
//! those.
//!
//! # Closing is a finding, not an error
//!
//! `/ws` closes the socket rather than dropping events when a client falls behind, and closes it
//! again if the bus laps the subscriber. Both are correct server behaviour and both are things
//! T-ISO-1 asserts do **not** happen while a capture blocks. So the recorder records the close and
//! keeps it available for assertion instead of reconnecting behind the scenario's back — an
//! automatic reconnect would make the one symptom the PRF-04 guard exists to catch invisible.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::task::JoinHandle;

/// One event frame off the socket.
#[derive(Debug, Clone)]
pub struct Event {
    /// `mount.position`, `alert`, … — the dotted topic name as it appears on the wire.
    pub topic: String,
    /// The server's own timestamp, RFC 3339 with milliseconds.
    pub ts: String,
    /// The topic's payload.
    pub data: Value,
    /// When this client saw it. Monotonic, so it survives the clock being stepped mid-scenario.
    pub at: Instant,
}

impl Event {
    /// A string field of the payload, or `""` when absent — for assertions that want to match on
    /// a code or a state without unwrapping twice.
    #[must_use]
    pub fn str_field(&self, key: &str) -> &str {
        self.data
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct Recorded {
    events: Vec<Event>,
    /// Why the socket closed, if it did. `None` means it is still open.
    closed: Option<String>,
}

/// A live recording of `/ws`.
pub struct EventStream {
    recorded: Arc<Mutex<Recorded>>,
    /// The connect snapshot (SDD §5.8.3): the stateful topics' latest values, delivered before any
    /// live event. Kept apart from `events` so a scenario asserting "this happened" cannot be
    /// satisfied by a value that was already true when it connected.
    snapshot: Vec<Event>,
    reader: JoinHandle<()>,
    opened_at: Instant,
}

impl EventStream {
    /// Open `/ws`, consume the connect snapshot and start recording.
    ///
    /// # Panics
    ///
    /// When the upgrade fails, or when the first frame is not the snapshot the protocol promises.
    /// Both are contract violations worth failing loudly on: every later assertion in every
    /// scenario reads the recording this sets up.
    pub async fn connect(client: &crate::Client) -> Self {
        let ticket = client.ws_ticket().await;
        let url = format!(
            "{}/ws?ticket={ticket}",
            client.base().replacen("http://", "ws://", 1)
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap_or_else(|error| panic!("cannot open {url}: {error}"));

        let opened_at = Instant::now();
        let first = socket
            .next()
            .await
            .expect("the socket closed before the snapshot")
            .expect("the first frame is readable");
        let first: Value = serde_json::from_str(first.to_text().expect("the snapshot is text"))
            .expect("the snapshot is JSON");
        assert_eq!(
            first.get("type").and_then(Value::as_str),
            Some("snapshot"),
            "the first frame on /ws must be the snapshot (SDD §5.8.3), got: {first}"
        );
        let snapshot = first
            .get("events")
            .and_then(Value::as_array)
            .map(|events| events.iter().map(|event| parse(event, opened_at)).collect())
            .unwrap_or_default();

        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let sink = Arc::clone(&recorded);
        let reader = tokio::spawn(async move {
            while let Some(frame) = socket.next().await {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        sink.lock().expect("recorder lock").closed = Some(error.to_string());
                        return;
                    }
                };
                match frame {
                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        // Control frames carry `type` and no `topic`; events carry `topic` and no
                        // `type`. Discriminating on which key is *present* rather than on a tag is
                        // the protocol as specified, and asserting it here means a server that
                        // started tagging events would fail this suite at the first scenario.
                        if value.get("type").is_some() {
                            continue;
                        }
                        let now = Instant::now();
                        sink.lock()
                            .expect("recorder lock")
                            .events
                            .push(parse(&value, now));
                    }
                    tokio_tungstenite::tungstenite::Message::Close(frame) => {
                        let reason = frame.map_or_else(
                            || "closed with no frame".to_owned(),
                            |frame| format!("{} {}", frame.code, frame.reason),
                        );
                        sink.lock().expect("recorder lock").closed = Some(reason);
                        return;
                    }
                    tokio_tungstenite::tungstenite::Message::Ping(payload) => {
                        let _ = socket
                            .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                            .await;
                    }
                    _ => {}
                }
            }
            sink.lock().expect("recorder lock").closed = Some("stream ended".to_owned());
        });

        Self {
            recorded,
            snapshot,
            reader,
            opened_at,
        }
    }

    /// The connect snapshot's events, in `Topic::ALL` order.
    #[must_use]
    pub fn snapshot(&self) -> &[Event] {
        &self.snapshot
    }

    /// When the socket was opened, so cadence assertions can be anchored to it.
    #[must_use]
    pub fn opened_at(&self) -> Instant {
        self.opened_at
    }

    /// Everything recorded so far.
    #[must_use]
    pub fn all(&self) -> Vec<Event> {
        self.recorded.lock().expect("recorder lock").events.clone()
    }

    /// Everything recorded on one topic.
    #[must_use]
    pub fn topic(&self, topic: &str) -> Vec<Event> {
        self.all()
            .into_iter()
            .filter(|event| event.topic == topic)
            .collect()
    }

    /// Events on one topic that arrived within a window, for assertions that must be about what
    /// happened *during* something rather than about the whole recording.
    #[must_use]
    pub fn topic_between(&self, topic: &str, from: Instant, to: Instant) -> Vec<Event> {
        self.all()
            .into_iter()
            .filter(|event| event.topic == topic && event.at >= from && event.at <= to)
            .collect()
    }

    /// `alert` events carrying one code.
    #[must_use]
    pub fn alerts(&self, code: &str) -> Vec<Event> {
        self.topic("alert")
            .into_iter()
            .filter(|event| event.str_field("code") == code)
            .collect()
    }

    /// The order the distinct topics were first seen in — the "event stream shape" T-E2E-1 asserts.
    #[must_use]
    pub fn first_seen_order(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for event in self.all() {
            if !order.contains(&event.topic) {
                order.push(event.topic);
            }
        }
        order
    }

    /// Why the socket closed, if it has.
    ///
    /// A scenario asserting the socket stayed open is asserting the server never had to shed this
    /// subscriber — which is what "the event bus never lags a subscriber" means from out here.
    #[must_use]
    pub fn closed(&self) -> Option<String> {
        self.recorded.lock().expect("recorder lock").closed.clone()
    }

    /// Wait for an event on `topic` that satisfies `matches`.
    ///
    /// Searches what has already been recorded first, so a caller that starts waiting after the
    /// event arrived still finds it. Without that, every scenario would be a race it usually won.
    ///
    /// # Panics
    ///
    /// On timeout, naming the topic and quoting what did arrive on it.
    pub async fn wait_for(
        &self,
        topic: &str,
        timeout: Duration,
        matches: impl Fn(&Event) -> bool,
    ) -> Event {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(found) = self.topic(topic).into_iter().find(&matches) {
                return found;
            }
            if let Some(reason) = self.closed() {
                panic!("the /ws socket closed while waiting for {topic}: {reason}");
            }
            assert!(
                Instant::now() < deadline,
                "no matching `{topic}` event within {timeout:?}; saw {} on that topic: {:?}",
                self.topic(topic).len(),
                self.topic(topic)
                    .iter()
                    .map(|event| event.data.clone())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait for an event on `topic` that satisfies `matches` **and arrived after `since`**.
    ///
    /// The distinction from [`wait_for`](Self::wait_for) is the difference between "the mount is
    /// idle" and "the mount became idle", and getting it wrong is the classic way an end-to-end
    /// test passes without testing anything: a connected mount publishes `idle`, so a scenario
    /// that starts a goto and then waits for `idle` is satisfied instantly by the event from
    /// before the slew, and goes on to assert a pointing the mount never moved to. Any assertion
    /// about a *transition* wants this one.
    ///
    /// # Panics
    ///
    /// On timeout, naming the topic and quoting what arrived on it inside the window.
    pub async fn wait_for_since(
        &self,
        topic: &str,
        since: Instant,
        timeout: Duration,
        matches: impl Fn(&Event) -> bool,
    ) -> Event {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(found) = self
                .topic(topic)
                .into_iter()
                .filter(|event| event.at > since)
                .find(&matches)
            {
                return found;
            }
            if let Some(reason) = self.closed() {
                panic!("the /ws socket closed while waiting for {topic}: {reason}");
            }
            let seen: Vec<serde_json::Value> = self
                .topic(topic)
                .into_iter()
                .filter(|event| event.at > since)
                .map(|event| event.data)
                .collect();
            assert!(
                Instant::now() < deadline,
                "no matching `{topic}` event within {timeout:?} of the moment asked about; \
                 {} arrived on that topic since: {seen:?}",
                seen.len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The gaps between consecutive arrivals on a topic within a window.
    ///
    /// The first gap is measured from `from`, not from the first event, so a topic that went
    /// silent at the start of the window is caught. Measuring between events only would let a
    /// stream that stopped for two seconds and then resumed at a perfect 1 Hz pass.
    #[must_use]
    pub fn gaps(&self, topic: &str, from: Instant, to: Instant) -> Vec<Duration> {
        let events = self.topic_between(topic, from, to);
        let mut gaps = Vec::with_capacity(events.len() + 1);
        let mut previous = from;
        for event in &events {
            gaps.push(event.at.duration_since(previous));
            previous = event.at;
        }
        gaps.push(to.saturating_duration_since(previous));
        gaps
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        // The reader owns the socket; aborting it closes the connection. Left running, it would
        // hold a subscriber on the node's bus for the rest of the test binary, and a scenario
        // three files later would be measuring a node with a dozen phantom clients attached.
        self.reader.abort();
    }
}

fn parse(value: &Value, at: Instant) -> Event {
    Event {
        topic: value
            .get("topic")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        ts: value
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        data: value.get("data").cloned().unwrap_or(Value::Null),
        at,
    }
}
