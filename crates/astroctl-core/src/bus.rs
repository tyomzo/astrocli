//! Event bus and session-log sink — SDD §4.3, §5.8.3, §7; ADD §6.2.
//!
//! One [`tokio::sync::broadcast`] channel carries every operator-visible state change. The WS
//! hub (M1) and the [`JsonlSink`] here are both plain subscribers, so the log records exactly
//! what the UI was told, in the same bytes (SES-07).
//!
//! # Never backpressure the publisher
//!
//! [`EventBus::publish`] is a synchronous, non-blocking, infallible call. That is the whole
//! design: SDD §5.8.3 requires that a slow consumer be dropped rather than allowed to push back,
//! because *a stalled phone must never stall capture*. A `broadcast` channel with a bounded ring
//! gives that for free — when the ring wraps, the slowest receiver loses the oldest entries and
//! the sender is untouched.
//!
//! The cost is that a slow subscriber silently misses events unless someone tells it. So we do
//! tell it: [`EventSubscriber::recv`] returns [`Recv::Lagged`] with the number of skipped
//! events, and the subscriber must handle that arm to compile. Its recovery is to resync from
//! the hub's state snapshot (§5.8.3) — the hub's job, not the bus's.
//!
//! # Capacity
//!
//! [`EVENT_BUS_CAPACITY`] is 256 per SDD §7. At the Phase 1 event rate — `mount.position` at
//! 1 Hz plus discrete state changes — that is roughly four minutes of telemetry of slack for a
//! subscriber that stops polling. A subscriber further behind than that has a real problem, and
//! a snapshot resync is a better answer than a longer ring.

use std::io;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use crate::event::{Event, EventPayload};

/// Broadcast ring capacity for the event bus (SDD §7).
pub const EVENT_BUS_CAPACITY: usize = 256;

/// The one event pipeline (ADD §6.2).
///
/// Cheap to clone; every clone publishes into the same channel. Hold one `EventBus` in the
/// application state and hand clones to the components that emit.
///
/// ```
/// use astroctl_core::bus::{EventBus, Recv};
/// use astroctl_core::event::{SystemHealth, Topic};
///
/// # let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// # runtime.block_on(async {
/// let bus = EventBus::new();
/// let mut sub = bus.subscribe();
///
/// bus.publish(SystemHealth::new(180.5, true, 3600));
///
/// match sub.recv().await {
///     Recv::Event(event) => {
///         assert_eq!(event.topic, Topic::SystemHealth);
///         assert_eq!(event.data["disk_free_gb"], 180.5);
///     }
///     // Lag is an outcome the compiler makes you consider, not an error to swallow.
///     Recv::Lagged { skipped } => panic!("missed {skipped} events"),
///     Recv::Closed => panic!("bus closed"),
/// }
/// # });
/// ```
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    capacity: usize,
}

impl EventBus {
    /// A bus with the SDD §7 capacity of 256.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(EVENT_BUS_CAPACITY)
    }

    /// A bus with an explicit ring capacity.
    ///
    /// Production code uses [`EventBus::new`]; this exists so tests can force lag deterministically
    /// without publishing hundreds of events.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero — a zero-capacity broadcast channel cannot exist.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx, capacity }
    }

    /// Publish a typed payload on the topic its type declares.
    ///
    /// Returns the number of subscribers the event reached; zero is normal (nobody connected,
    /// no session log open) and is not an error. Never blocks, never fails, never applies
    /// backpressure — see the module docs.
    pub fn publish<P: EventPayload>(&self, payload: P) -> usize {
        self.publish_event(payload.into_event())
    }

    /// Publish an already-constructed [`Event`] — replay, tests, or forwarding a log line.
    ///
    /// Prefer [`EventBus::publish`]: it derives the topic from the payload type, so the pair
    /// cannot be mismatched.
    pub fn publish_event(&self, event: Event) -> usize {
        // `send` errors only when there are no receivers, which is a normal operating state.
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribe from the current position; a subscriber never sees events published before it.
    #[must_use]
    pub fn subscribe(&self) -> EventSubscriber {
        EventSubscriber {
            rx: self.tx.subscribe(),
        }
    }

    /// Number of live subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Ring capacity — how far behind a subscriber may fall before it starts losing events.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// One subscriber's view of the bus.
#[derive(Debug)]
pub struct EventSubscriber {
    rx: broadcast::Receiver<Event>,
}

impl EventSubscriber {
    /// Await the next bus outcome.
    ///
    /// Returns an enum rather than a `Result` so that lag is a first-class outcome the caller
    /// must consider, not an error that is easy to `?`-away or log-and-ignore. Losing events is
    /// a normal, expected consequence of the no-backpressure rule, and the consumer's job is to
    /// resync — silently continuing from the new position would leave the UI showing a state
    /// that never existed.
    pub async fn recv(&mut self) -> Recv {
        match self.rx.recv().await {
            Ok(event) => Recv::Event(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => Recv::Lagged { skipped },
            Err(broadcast::error::RecvError::Closed) => Recv::Closed,
        }
    }
}

/// What a subscriber got from the bus.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum Recv {
    /// The next event, in publication order.
    Event(Event),
    /// The subscriber fell more than [`EventBus::capacity`] behind and `skipped` events were
    /// overwritten before it read them. Reception continues from the oldest retained event on
    /// the next call; the consumer should resync from a snapshot (§5.8.3) rather than assume
    /// continuity.
    Lagged {
        /// How many events were dropped for this subscriber.
        skipped: u64,
    },
    /// Every [`EventBus`] handle has been dropped; no further events will arrive.
    Closed,
}

// ---------------------------------------------------------------------------------------------
// Session log sink
// ---------------------------------------------------------------------------------------------

/// What a finished [`JsonlSink`] wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonlSinkStats {
    /// Lines appended to the log.
    pub written: u64,
    /// Events the sink itself missed because it fell behind (see [`Recv::Lagged`]). Non-zero
    /// means the session log has holes: the disk could not keep up with the bus.
    pub lagged: u64,
    /// Events that could not be serialized and were skipped rather than killing the log.
    pub dropped: u64,
}

/// Appends every bus event to the session log as one JSON object per line (SDD §6).
///
/// # Crash tolerance
///
/// Each event is rendered into a single buffer ending in `\n` and written with one `write_all`
/// followed by an explicit `flush`. Nothing is held in a user-space buffer between events, so an
/// abrupt process death — `abort`, `SIGKILL`, a panic in another task — cannot lose a line that
/// the sink had already consumed, and cannot leave a half-written record from buffered spillover.
/// The file is opened in append mode, so the write lands at the end of the file atomically with
/// respect to any other appender.
///
/// The sink deliberately does **not** `fsync` per line. `fsync` would defend against power loss
/// rather than process death, at the cost of a disk flush per `mount.position` event — a
/// write amplification the Pi's SD card would pay 3600 times an hour for a log that is
/// regenerable telemetry, not the archive of record. Frames get the fsync discipline (§5.3.2);
/// the session log does not.
#[derive(Debug)]
pub struct JsonlSink {
    file: tokio::fs::File,
    path: PathBuf,
    subscriber: EventSubscriber,
    stats: JsonlSinkStats,
}

impl JsonlSink {
    /// Subscribe to `bus` and open (or create) the session log at `path` for appending.
    ///
    /// Subscription happens before the first `await`, so no event published after this call
    /// returns can be missed. Opening here rather than inside the spawned task means a bad log
    /// path fails during startup (§8.1) instead of silently producing no log at 2 a.m.
    ///
    /// # Errors
    ///
    /// Any I/O error opening or creating the file.
    pub async fn open(bus: &EventBus, path: impl AsRef<Path>) -> io::Result<Self> {
        let subscriber = bus.subscribe();
        let path = path.as_ref().to_path_buf();
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            file,
            path,
            subscriber,
            stats: JsonlSinkStats::default(),
        })
    }

    /// The log file being appended to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Drain the bus into the log until every [`EventBus`] handle is dropped.
    ///
    /// # Errors
    ///
    /// I/O failures are fatal and returned: a log that cannot be written is worth surfacing.
    /// A serialization failure is not — it costs one line, is counted in
    /// [`JsonlSinkStats::dropped`], and the log keeps running.
    pub async fn run(mut self) -> io::Result<JsonlSinkStats> {
        loop {
            match self.subscriber.recv().await {
                Recv::Event(event) => match event.to_jsonl_line() {
                    Ok(line) => {
                        self.file.write_all(line.as_bytes()).await?;
                        self.file.flush().await?;
                        self.stats.written += 1;
                    }
                    Err(_) => self.stats.dropped += 1,
                },
                Recv::Lagged { skipped } => self.stats.lagged += skipped,
                Recv::Closed => break,
            }
        }
        self.file.flush().await?;
        Ok(self.stats)
    }

    /// Run the sink on its own task.
    ///
    /// The handle resolves with the sink's stats once the bus closes, which is how the shutdown
    /// sequence of SDD §7 ("flush session log → exit") knows the log is complete.
    pub fn spawn(self) -> tokio::task::JoinHandle<io::Result<JsonlSinkStats>> {
        tokio::spawn(self.run())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Alert, CaptureProgress, SystemHealth, Topic};
    use std::time::{Duration, Instant};

    /// Set on the re-executed child of `jsonl_sink_survives_process_abort`; its value is the log
    /// path. Its presence is what makes the child take the write-then-abort branch.
    const ABORT_LOG_ENV: &str = "ASTROCTL_T03_ABORT_LOG";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astroctl-t03-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir creates");
        dir
    }

    #[tokio::test]
    async fn publishes_typed_payloads_on_their_own_topic() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();

        assert_eq!(bus.capacity(), 256, "SDD §7 fixes the capacity at 256");
        assert_eq!(bus.subscriber_count(), 1);
        assert_eq!(bus.publish(SystemHealth::new(180.5, true, 3600)), 1);

        match sub.recv().await {
            Recv::Event(event) => {
                assert_eq!(event.topic, Topic::SystemHealth);
                assert_eq!(event.data["disk_free_gb"], 180.5);
            }
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_is_not_an_error() {
        let bus = EventBus::new();
        assert_eq!(bus.publish(Alert::info("STARTED", "session started")), 0);
    }

    #[tokio::test]
    async fn dropping_the_bus_closes_subscribers() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        drop(bus);
        assert_eq!(sub.recv().await, Recv::Closed);
    }

    /// SDD §5.8.3: the bus never applies backpressure; the slow consumer is told it lagged.
    #[tokio::test]
    async fn slow_subscriber_is_told_it_lagged_and_publishers_never_block() {
        const BURST: u64 = 5_000;

        let bus = EventBus::new();
        let mut sub = bus.subscribe();

        let start = Instant::now();
        for i in 0..BURST {
            bus.publish(SystemHealth::new(0.0, true, i));
        }
        let elapsed = start.elapsed();

        // `publish` is a synchronous `fn`, so it *cannot* await a stalled consumer; the timing
        // assertion is the behavioural backstop against someone making it async later.
        assert!(
            elapsed < Duration::from_secs(1),
            "publishing {BURST} events behind a stalled subscriber took {elapsed:?} — \
             the bus must never wait on a consumer"
        );

        let skipped = match sub.recv().await {
            Recv::Lagged { skipped } => skipped,
            other => panic!("expected an explicit Lagged signal, got {other:?}"),
        };
        assert_eq!(
            skipped,
            BURST - EVENT_BUS_CAPACITY as u64,
            "the ring retains exactly the last {EVENT_BUS_CAPACITY} events"
        );

        // Reception resumes at the oldest retained event, not at the newest.
        match sub.recv().await {
            Recv::Event(event) => assert_eq!(event.data["uptime_s"], skipped),
            other => panic!("expected the oldest retained event, got {other:?}"),
        }

        // …and the tail is intact, so a resyncing consumer can trust what it does receive.
        let mut last = skipped;
        while let Recv::Event(event) = sub.recv().await {
            last += 1;
            assert_eq!(event.data["uptime_s"], last);
            if last == BURST - 1 {
                break;
            }
        }
        assert_eq!(last, BURST - 1);
    }

    /// The same rule with a genuinely slow consumer: it sleeps, the publisher does not wait.
    #[tokio::test]
    async fn a_genuinely_slow_consumer_does_not_stall_capture() {
        let bus = EventBus::with_capacity(8);
        let mut sub = bus.subscribe();

        let consumer = tokio::spawn(async move {
            let mut lagged_total = 0_u64;
            let mut seen = 0_u64;
            loop {
                match sub.recv().await {
                    Recv::Event(_) => {
                        seen += 1;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Recv::Lagged { skipped } => lagged_total += skipped,
                    Recv::Closed => break,
                }
            }
            (seen, lagged_total)
        });

        // Give the consumer a chance to park on `recv` before the burst.
        tokio::task::yield_now().await;

        let start = Instant::now();
        for i in 0..1_000 {
            bus.publish(CaptureProgress::exposing(format!("light_{i:05}"), i as f64));
        }
        let publish_elapsed = start.elapsed();
        assert!(
            publish_elapsed < Duration::from_millis(500),
            "publisher waited {publish_elapsed:?} on a sleeping consumer"
        );

        drop(bus);
        let (seen, lagged_total) = consumer.await.expect("consumer task joins");
        assert!(
            lagged_total > 0,
            "the slow consumer should have been told it lagged"
        );
        assert!(
            seen > 0,
            "the slow consumer should still have received events"
        );
    }

    #[tokio::test]
    async fn jsonl_sink_writes_one_parseable_line_per_event_in_order() {
        let dir = temp_dir("order");
        let path = dir.join("session.jsonl");

        let bus = EventBus::new();
        let sink = JsonlSink::open(&bus, &path).await.expect("sink opens");
        assert_eq!(sink.path(), path);
        let handle = sink.spawn();

        for i in 0..32 {
            bus.publish(CaptureProgress::saved(format!("light_{i:05}"), i as f64));
        }
        drop(bus);

        let stats = handle
            .await
            .expect("sink task joins")
            .expect("sink writes cleanly");
        assert_eq!(stats.written, 32);
        assert_eq!(stats.lagged, 0);
        assert_eq!(stats.dropped, 0);

        let contents = std::fs::read_to_string(&path).expect("log reads");
        assert!(
            contents.ends_with('\n'),
            "every record is newline-terminated"
        );
        let events: Vec<Event> = contents
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one Event"))
            .collect();
        assert_eq!(events.len(), 32);
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.topic, Topic::CaptureProgress);
            assert_eq!(event.data["frame_id"], format!("light_{i:05}"));
        }

        // The sink appends: reopening must not truncate the existing session log.
        let bus = EventBus::new();
        let handle = JsonlSink::open(&bus, &path)
            .await
            .expect("sink reopens")
            .spawn();
        bus.publish(Alert::info("REOPENED", "second run"));
        drop(bus);
        handle.await.expect("joins").expect("writes");
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("log reads")
                .lines()
                .count(),
            33
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The crash-tolerance criterion: a process that dies without any shutdown must still leave a
    /// log every line of which parses.
    ///
    /// The test re-executes its own binary with [`ABORT_LOG_ENV`] set; the child publishes a
    /// burst, gives the sink time to drain, and calls `std::process::abort()` — no unwinding, no
    /// destructors, no flush-on-drop. Anything the sink was holding in a user-space buffer is
    /// lost, which is exactly what the assertions below would catch.
    #[test]
    fn jsonl_sink_survives_process_abort() {
        const EVENTS: usize = 64;

        if let Ok(path) = std::env::var(ABORT_LOG_ENV) {
            // ---- child ----
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("child runtime builds");
            runtime.block_on(async move {
                let bus = EventBus::new();
                let handle = JsonlSink::open(&bus, &path)
                    .await
                    .expect("child sink opens")
                    .spawn();
                for i in 0..EVENTS {
                    bus.publish(CaptureProgress::saved(format!("light_{i:05}"), i as f64));
                }
                // Let the sink drain, then die *inside* the runtime: the handle is never
                // joined, the bus is never dropped, the runtime is never shut down, and no
                // destructor anywhere gets a chance to flush on our behalf.
                tokio::time::sleep(Duration::from_millis(500)).await;
                drop(handle);
                std::process::abort();
            });
        }

        // ---- parent ----
        let dir = temp_dir("abort");
        let path = dir.join("session.jsonl");
        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args([
                "bus::tests::jsonl_sink_survives_process_abort",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ABORT_LOG_ENV, &path)
            .output()
            .expect("child test process runs");
        assert!(
            !output.status.success(),
            "child was expected to die by abort, exited {:?}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let contents = std::fs::read_to_string(&path).expect("log survives the abort");
        assert!(
            contents.ends_with('\n'),
            "log ends mid-record — the last write was not atomic:\n{contents}"
        );
        let events: Vec<Event> = contents
            .lines()
            .map(|line| serde_json::from_str(line).expect("every surviving line parses"))
            .collect();
        assert_eq!(
            events.len(),
            EVENTS,
            "expected every consumed event on disk after the abort"
        );
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.topic, Topic::CaptureProgress);
            assert_eq!(event.data["frame_id"], format!("light_{i:05}"));
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
