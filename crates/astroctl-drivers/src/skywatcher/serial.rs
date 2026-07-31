//! The serial task: the single owner of the cable, its two lanes, the per-request timeout and
//! retry, and the heartbeat that makes link loss detectable (SDD §5.2.4, M3-T02).
//!
//! # The surface
//!
//! ```ignore
//! link.send(GetPosition(Axis::Ra)).await?      // -> Counts
//! link.send_priority(InstantStop(axis)).await? // -> ()   the e-stop lane
//! link.emergency_stop().await?                 //         `L` on both axes, both priority
//! ```
//!
//! [`SerialLink::send`] is generic over [`Command`], so a `GetPosition` gives back `Counts` and a
//! `StartMotion` gives back `()`, with no downcast and nowhere to point the wrong decoder at a
//! reply. The command never crosses a channel — its [`Frame`] does — because erasing `C` to put it
//! in a queue would erase exactly the typing M3-T01 exists to provide.
//!
//! # The two lanes
//!
//! Priority is drained first, always: `tokio::select!` with `biased`, and the normal branch is
//! *disabled* while anything is pending on the priority lane. That is SDD §5.2.4's "no new normal
//! request starts while the priority queue is non-empty", expressed as a branch that cannot be
//! taken rather than a check that could be forgotten.
//!
//! The counter behind that gate is incremented by [`SerialLink::send_priority`] **before** the
//! request is queued. The window it closes is small and real: a priority request that is in the
//! channel but not yet counted is one a stalled normal exchange cannot see.
//!
//! # What §5.2.4 does not say, and what this does about it
//!
//! §5.2.4 lets the in-flight normal request complete before the priority lane proceeds, and the
//! spike makes that generous: the measured round trip is 14.7–17.2 ms over 2000 exchanges, so an
//! emergency stop behind a *healthy* normal command waits under 20 ms and the budget holds.
//!
//! It does not hold when the mount has stopped answering, and that is precisely when §5.4 has the
//! watchdog issue a priority-lane stop — "for serial loss during motion, issue priority-lane
//! stop". Taken literally, that stop would queue behind the 500 ms timeout of the very poll that
//! detected the loss. At the measured 835× sidereal cruise, 500 ms is ~43,700 counts: 1.7° of
//! unwanted slew, on the path whose entire purpose is to stop one.
//!
//! So the rule is one sentence: **an in-flight normal exchange gets one round trip
//! ([`SerialTimings::preempt_after`]), and after that the priority lane can take the cable.** A
//! healthy exchange never notices — it answers in ≤17.2 ms and completes normally, exactly as
//! §5.2.4 says. A stalled one is abandoned, and the worst an emergency stop can wait becomes one
//! round trip instead of one timeout. The abandoned request is retried by its own caller and is
//! **not** counted against the heartbeat: losing the cable to an emergency stop says nothing about
//! whether the cable works.
//!
//! # The heartbeat
//!
//! Counted here rather than in the poll loop, because here is the only place that sees the whole
//! verdict: a reply whose framing is fine and whose payload is the wrong width is a miss, and only
//! `C::decode` can say so. Every completed request counts, not only the 1 Hz position poll —
//! a goto costs eight frames and is the moment link loss matters most, so restricting the count to
//! polls would look away at the worst time. `heartbeat_misses` consecutive misses fire
//! [`Heartbeat::Lost`] once, and the next success fires [`Heartbeat::Recovered`] once.
//!
//! **A refusal is not a miss.** `!1` means the mount received the frame, understood it and said
//! no; the link is in perfect health and retrying would produce the same `!1` forever. That
//! distinction is the reason M3-T01 made `MountError` and `ProtocolError` separate types.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::config::SerialConfig;
use astroctl_core::error::DeviceError;
use astroctl_core::types::Axis;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::codec::{decode_reply, Command, Frame, InstantStop, Reply};
use super::port::{exchange, Exchange, Wire, WireFactory, WriteGate, Yield};

// -----------------------------------------------------------------------------------------
// Timings
// -----------------------------------------------------------------------------------------

/// Every duration the serial task uses.
///
/// The first three are `mount.serial.*` verbatim (PRD §8.1). The last three have no configuration
/// key because PRD §8.1 offers none, and inventing keys in `astroctl-core` is not this task's to
/// do; they are fields rather than constants so a test can still reach them, which is the whole
/// practical content of "config-overridable".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialTimings {
    /// `mount.serial.request_timeout_ms`. One exchange's budget, not one request's — a request
    /// that retries is allowed this much per attempt.
    pub request_timeout: Duration,
    /// `mount.serial.request_retries`. Attempts after the first.
    pub retries: u32,
    /// `mount.serial.heartbeat_misses`. Consecutive failed requests before the watchdog is told.
    pub heartbeat_misses: u32,
    /// How long an in-flight *normal* exchange may run before the priority lane may take the
    /// cable from it. Measured from the instant its frame was written.
    ///
    /// 18 ms, from the measurement rather than from taste: 2000 exchanges spanned 14.7–17.2 ms
    /// (`spikes/skywatcher-heq5/FINDINGS.md`), so a bound just past the observed maximum lets
    /// every healthy exchange finish untouched and takes the cable from every one that has
    /// stopped answering. It is also therefore the worst case an emergency stop can wait —
    /// against the 20 ms budget of SDD §5.8.2, and against the 500 ms it would otherwise be.
    pub preempt_after: Duration,
    /// How long to wait before the first attempt to reopen a port that failed. The spike measured
    /// close-and-reopen at 38 ms with the first exchange succeeding, so this is not about giving
    /// the *port* time — it is about not reopening a missing device node at request rate.
    pub reopen_backoff: Duration,
    /// The ceiling the backoff doubles towards. Reset by any exchange that reaches the mount,
    /// which is the M2 lesson: backoff resets on success, not on the passage of time.
    pub reopen_backoff_max: Duration,
}

impl Default for SerialTimings {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_millis(500),
            retries: 1,
            heartbeat_misses: 3,
            preempt_after: Duration::from_millis(18),
            reopen_backoff: Duration::from_millis(250),
            reopen_backoff_max: Duration::from_secs(5),
        }
    }
}

impl From<&SerialConfig> for SerialTimings {
    fn from(config: &SerialConfig) -> Self {
        Self {
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            retries: config.request_retries,
            heartbeat_misses: config.heartbeat_misses,
            ..Self::default()
        }
    }
}

// -----------------------------------------------------------------------------------------
// The watchdog seam
// -----------------------------------------------------------------------------------------

/// What the serial link tells the watchdog about its own health.
///
/// **There is no consumer yet.** M1-T17's watchdog arm is unbuilt, and its task note is explicit
/// that this heartbeat *replaces* the signal it will otherwise have — the facade currently logs a
/// failed poll at `debug` and counts nothing — rather than adding a second one. So this is a named
/// seam with one producer, shaped like `gphoto2::thread::fault_channel` for the same reason: the
/// driver layer has no event bus (deliberately, per this crate's own rules), so a driver that
/// learns something the operator needs publishes it on a channel and lets the facade turn it into
/// an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heartbeat {
    /// `heartbeat_misses` consecutive requests failed at the link level. Fired **once** per
    /// transition, not once per failed request — the alert discipline the rest of the system uses.
    Lost {
        /// The streak that triggered it, so the log line carries the threshold that was crossed.
        misses: u32,
    },
    /// A request succeeded after a loss was reported.
    Recovered,
}

/// Where [`Heartbeat`] notices go.
pub type WatchdogSink = mpsc::UnboundedSender<Heartbeat>;
/// Where they arrive. M1-T17 owns this end.
pub type WatchdogSource = mpsc::UnboundedReceiver<Heartbeat>;

/// A watchdog channel.
///
/// Unbounded for `gphoto2::thread::fault_channel`'s reason: a heartbeat transition is a rare event
/// the watchdog **must** see, and the bounded alternative would put a choice between blocking and
/// dropping it in the path of the notification that a mount has gone quiet.
#[must_use]
pub fn watchdog_channel() -> (WatchdogSink, WatchdogSource) {
    mpsc::unbounded_channel()
}

/// The consecutive-miss counter and its edge trigger.
#[derive(Debug)]
struct LinkHealth {
    misses: AtomicU32,
    threshold: u32,
    lost: AtomicBool,
    watchdog: WatchdogSink,
}

impl LinkHealth {
    /// A request that reached the mount and got an answer it could use — or a refusal, which is
    /// also an answer.
    fn succeeded(&self) {
        self.misses.store(0, Ordering::Release);
        if self.lost.swap(false, Ordering::AcqRel) {
            let _ = self.watchdog.send(Heartbeat::Recovered);
        }
    }

    /// A request that failed at the link level, after every retry it was allowed.
    fn missed(&self) {
        let streak = self.misses.fetch_add(1, Ordering::AcqRel) + 1;
        if streak >= self.threshold && !self.lost.swap(true, Ordering::AcqRel) {
            let _ = self.watchdog.send(Heartbeat::Lost { misses: streak });
        }
    }
}

// -----------------------------------------------------------------------------------------
// The lanes
// -----------------------------------------------------------------------------------------

/// Which queue a request travels in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// Everything: the poll, the handshake, the eight frames of a goto.
    Normal,
    /// The emergency stop, and nothing else (SDD §5.2.4).
    Priority,
}

/// One queued exchange.
struct Request {
    frame: Frame,
    /// The raw outcome. Decoding is `C::decode`'s job and happens at the caller, where `C` still
    /// exists.
    reply: oneshot::Sender<Exchange>,
}

/// How deep the normal lane may get before a caller waits.
///
/// A goto costs eight frames per axis and the poll adds two a second, so sixteen is comfortably
/// more than any legitimate burst — and shallow enough that a caller looping without awaiting
/// feels it immediately rather than accumulating a queue of stale position requests whose answers
/// nobody wants. The priority lane is unbounded by contrast: an emergency stop must never wait for
/// queue space.
const NORMAL_LANE_DEPTH: usize = 16;

#[derive(Debug, Clone)]
struct Lanes {
    normal: mpsc::Sender<Request>,
    priority: mpsc::UnboundedSender<Request>,
}

// -----------------------------------------------------------------------------------------
// The link
// -----------------------------------------------------------------------------------------

/// The handle every layer above the codec holds. The task on the other end owns the cable.
#[derive(Debug)]
pub struct SerialLink {
    /// `None` once the link has been shut down. Holding the **only** senders is what makes
    /// shutdown work: dropping them closes both channels, and the task's `select!` is then the
    /// thing that tells it to stop.
    lanes: Mutex<Option<Lanes>>,
    /// Priority requests queued but not yet answered. Read by an in-flight normal exchange.
    pending_priority: Arc<AtomicUsize>,
    health: Arc<LinkHealth>,
    timings: SerialTimings,
}

impl SerialLink {
    /// Start the serial task and return the handle to it.
    ///
    /// No I/O happens here — the port is opened by the first request, so constructing a driver
    /// stays free of side effects as SDD §8.1 requires of registry construction.
    ///
    /// `gate` is the safety mode: [`WriteGate::Actions`] for a connected driver,
    /// [`WriteGate::InquiryOnly`] for anything read-only, which refuses every uppercase byte on
    /// the raw stream exactly as `spikes/skywatcher-heq5/survey.py` does.
    #[must_use]
    pub fn spawn(
        factory: Arc<dyn WireFactory>,
        gate: WriteGate,
        timings: SerialTimings,
        watchdog: WatchdogSink,
    ) -> (Self, JoinHandle<()>) {
        let (normal_tx, normal_rx) = mpsc::channel(NORMAL_LANE_DEPTH);
        let (priority_tx, priority_rx) = mpsc::unbounded_channel();
        let pending_priority = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(run(
            factory,
            gate,
            timings,
            Arc::clone(&pending_priority),
            normal_rx,
            priority_rx,
        ));

        let link = Self {
            lanes: Mutex::new(Some(Lanes {
                normal: normal_tx,
                priority: priority_tx,
            })),
            pending_priority,
            health: Arc::new(LinkHealth {
                misses: AtomicU32::new(0),
                threshold: timings.heartbeat_misses,
                lost: AtomicBool::new(false),
                watchdog,
            }),
            timings,
        };
        (link, task)
    }

    /// Send a command on the normal lane.
    ///
    /// # Errors
    /// [`DeviceError::Timeout`] after the configured retries, [`DeviceError::Protocol`] for a
    /// reply that is not one, [`DeviceError::Rejected`] for a refusal the mount sent,
    /// [`DeviceError::Transport`] for a port that failed, [`DeviceError::Busy`] if the emergency
    /// stop lane took the cable, or [`DeviceError::NotConnected`] if the link has been shut down.
    pub async fn send<C: Command>(&self, cmd: C) -> Result<C::Response, DeviceError> {
        self.dispatch(Lane::Normal, cmd).await
    }

    /// Send a command on the priority lane, jumping every queued normal request.
    ///
    /// **For the emergency stop.** SDD §5.2.4 gives this lane to `L`, and nothing else has an
    /// argument for it: every other command can wait behind a 16 ms exchange, and a lane that
    /// several callers use is not a priority lane. Nothing enforces that — it is a contract with
    /// M3-T03 and M3-T04, stated here because it is the kind that erodes quietly.
    ///
    /// # Errors
    /// As [`Self::send`].
    pub async fn send_priority<C: Command>(&self, cmd: C) -> Result<C::Response, DeviceError> {
        self.dispatch(Lane::Priority, cmd).await
    }

    /// The emergency stop of SDD §5.2.4: `L` on both axes, both on the priority lane.
    ///
    /// Both are always attempted. A failure on RA must not leave DEC slewing, which is what an
    /// early return would do — so the two run together and the first error is reported after both
    /// have had their turn.
    ///
    /// # Errors
    /// The first failure of the two, once both have been attempted.
    pub async fn emergency_stop(&self) -> Result<(), DeviceError> {
        let (ra, dec) = tokio::join!(
            self.send_priority(InstantStop(Axis::Ra)),
            self.send_priority(InstantStop(Axis::Dec))
        );
        ra.and(dec)
    }

    /// Close both lanes. The task drains nothing further, drops the wire and ends; the port is
    /// released by the time its thread joins.
    pub fn shutdown(&self) {
        drop(self.lanes.lock().map(|mut lanes| lanes.take()));
    }

    /// Whether the link is still accepting requests.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.lanes
            .lock()
            .is_ok_and(|lanes| lanes.as_ref().is_some())
    }

    /// The current consecutive-miss streak. Zero whenever the last request succeeded.
    #[must_use]
    pub fn consecutive_misses(&self) -> u32 {
        self.health.misses.load(Ordering::Acquire)
    }

    /// The timings this link was built with.
    #[must_use]
    pub const fn timings(&self) -> &SerialTimings {
        &self.timings
    }

    /// One request: encode once, exchange, retry the failures worth retrying, and account for the
    /// heartbeat exactly once at the end.
    ///
    /// `cmd` is taken **by value**, and that is not a style choice. The command lives across the
    /// `.await` below, and a `&C` there is only `Send` when `C: Sync` — so a borrowed parameter
    /// would force every caller of the generic seam M3-T03 defined (`Exchange::send`, whose bound
    /// is `C: Command + Send`) to add a bound the trait does not have. Owning it costs a `Copy` of
    /// a handful of bytes and makes the future `Send` for exactly the commands the trait admits.
    async fn dispatch<C: Command>(&self, lane: Lane, cmd: C) -> Result<C::Response, DeviceError> {
        let frame = cmd.encode();
        let mut attempt = 0;
        loop {
            // Retrying re-enters the *queue*, which is not an implementation detail: it is what
            // keeps a retrying normal request from holding the cable across two full timeouts
            // while an emergency stop waits behind it.
            let outcome = self.exchange(lane, frame).await?;
            let retryable = attempt < self.timings.retries;
            match interpret(&cmd, outcome, self.timings.request_timeout) {
                Verdict::Ok(response) => {
                    self.health.succeeded();
                    return Ok(response);
                }
                // The mount answered. The link is healthy and the answer will not change.
                Verdict::Refused(error) => {
                    self.health.succeeded();
                    return Err(error);
                }
                Verdict::Contended(error) => {
                    if retryable {
                        attempt += 1;
                        continue;
                    }
                    return Err(error);
                }
                Verdict::Miss(error) => {
                    if retryable {
                        attempt += 1;
                        continue;
                    }
                    self.health.missed();
                    return Err(error);
                }
                Verdict::Fatal(error) => return Err(error),
            }
        }
    }

    /// Queue one frame and wait for what the wire made of it.
    async fn exchange(&self, lane: Lane, frame: Frame) -> Result<Exchange, DeviceError> {
        let (sender, reply) = oneshot::channel();
        let request = Request {
            frame,
            reply: sender,
        };
        // Cloned out of the guard and the guard dropped, so nothing is held across the `.await`
        // below. The workspace denies `await_holding_lock` and this is the one place in the
        // driver that could trip it.
        let lanes = {
            let guard = self
                .lanes
                .lock()
                .map_err(|_| DeviceError::Transport("the serial link's lock is poisoned".into()))?;
            guard.clone()
        };
        let Some(lanes) = lanes else {
            return Err(DeviceError::NotConnected);
        };

        match lane {
            Lane::Priority => {
                // Counted *before* it is queued. An in-flight normal exchange watches this
                // counter, and a request that is queued but not yet counted is one it cannot see.
                self.pending_priority.fetch_add(1, Ordering::Release);
                if lanes.priority.send(request).is_err() {
                    self.pending_priority.fetch_sub(1, Ordering::Release);
                    return Err(DeviceError::NotConnected);
                }
            }
            Lane::Normal => {
                lanes
                    .normal
                    .send(request)
                    .await
                    .map_err(|_| DeviceError::NotConnected)?;
            }
        }

        // A dropped reply means the task ended mid-request. Reported, but deliberately **not**
        // counted against the heartbeat: shutting the link down must not fire the watchdog the
        // link exists to feed.
        reply.await.map_err(|_| {
            DeviceError::Transport(format!(
                "the serial task stopped while {frame} was in flight"
            ))
        })
    }
}

impl Drop for SerialLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// -----------------------------------------------------------------------------------------
// Verdicts
// -----------------------------------------------------------------------------------------

/// What one exchange's outcome means for the retry loop and for the heartbeat. The two questions
/// have different answers and separating them is the point of this enum.
enum Verdict<R> {
    /// A reply the command understood.
    Ok(R),
    /// The mount refused the command. Healthy link, settled answer.
    Refused(DeviceError),
    /// The exchange lost the cable to the priority lane. Retryable, and not a link fault.
    Contended(DeviceError),
    /// The link failed to produce a usable answer. Retryable, and counts against the heartbeat.
    Miss(DeviceError),
    /// Nothing was transmitted and retrying cannot help.
    Fatal(DeviceError),
}

fn interpret<C: Command>(cmd: &C, outcome: Exchange, budget: Duration) -> Verdict<C::Response> {
    match outcome {
        Exchange::Reply(chunk) => match decode_reply(chunk.as_bytes()) {
            Ok(Reply::Ok(payload)) => match cmd.decode(payload) {
                Ok(response) => Verdict::Ok(response),
                // Framing intact, payload unusable. A garbled response counts as a failure
                // (SDD §5.2.4) and this is the half of "garbled" that only the command can see.
                Err(error) => Verdict::Miss(DeviceError::Protocol(error.to_string())),
            },
            Ok(Reply::Err(refusal)) => Verdict::Refused(DeviceError::Rejected(refusal.to_string())),
            Err(error) => Verdict::Miss(DeviceError::Protocol(error.to_string())),
        },
        Exchange::Silent => Verdict::Miss(DeviceError::Timeout(budget)),
        Exchange::Flood { seen } => Verdict::Miss(DeviceError::Protocol(format!(
            "{seen} bytes arrived with no terminator among them; no Synta reply is that long"
        ))),
        Exchange::Failed(why) => Verdict::Miss(DeviceError::Transport(why)),
        Exchange::Yielded => Verdict::Contended(DeviceError::Busy(
            "the emergency-stop lane took the serial line",
        )),
        Exchange::Refused(refused) => Verdict::Fatal(DeviceError::Protocol(format!(
            "the write gate refused the frame: {refused}"
        ))),
    }
}

// -----------------------------------------------------------------------------------------
// The task
// -----------------------------------------------------------------------------------------

/// When the port may be reopened, and how long until the attempt after that.
struct Reopen {
    delay: Duration,
    next: Option<Instant>,
}

impl Reopen {
    fn new(timings: &SerialTimings) -> Self {
        Self {
            delay: timings.reopen_backoff,
            next: None,
        }
    }

    fn ready(&self) -> bool {
        self.next.is_none_or(|at| Instant::now() >= at)
    }

    fn failed(&mut self, timings: &SerialTimings) {
        self.next = Some(Instant::now() + self.delay);
        self.delay = (self.delay * 2).min(timings.reopen_backoff_max);
    }

    /// Reset on a *successful exchange*, not on a successful open. A port that opens cleanly and
    /// then answers nothing is still a failing link, and resetting on the open would have it
    /// reopened at request rate forever.
    fn succeeded(&mut self, timings: &SerialTimings) {
        self.delay = timings.reopen_backoff;
        self.next = None;
    }
}

/// The task. Owns the wire, drains the lanes, and never has two requests outstanding.
async fn run(
    factory: Arc<dyn WireFactory>,
    gate: WriteGate,
    timings: SerialTimings,
    pending_priority: Arc<AtomicUsize>,
    mut normal: mpsc::Receiver<Request>,
    mut priority: mpsc::UnboundedReceiver<Request>,
) {
    let mut wire: Option<Box<dyn Wire>> = None;
    let mut reopen = Reopen::new(&timings);

    loop {
        let (request, lane) = tokio::select! {
            biased;
            // Priority first, always. `biased` is what makes that a property of the code rather
            // than a coin flip taken 62 times a second.
            Some(request) = priority.recv() => (request, Lane::Priority),
            // ...and no new normal request starts while anything is pending on it. The branch is
            // disabled rather than the request deferred, so there is no path that takes one.
            Some(request) = normal.recv(), if pending_priority.load(Ordering::Acquire) == 0 => {
                (request, Lane::Normal)
            }
            else => break,
        };

        let outcome = match ensure_open(&mut wire, &factory, &mut reopen, &timings).await {
            Ok(open) => {
                let wanted = || pending_priority.load(Ordering::Acquire) > 0;
                let yield_to = match lane {
                    // Nothing takes the cable from an emergency stop.
                    Lane::Priority => Yield::Never,
                    Lane::Normal => Yield::When {
                        wanted: &wanted,
                        after: timings.preempt_after,
                    },
                };
                exchange(
                    open.as_mut(),
                    request.frame,
                    gate,
                    timings.request_timeout,
                    &yield_to,
                )
                .await
            }
            Err(why) => Exchange::Failed(why),
        };

        if matches!(outcome, Exchange::Failed(_)) {
            // The fd is suspect — unplugged, revoked, or never there. Drop it so the next request
            // reopens; the spike measured close-and-reopen at 38 ms with the first exchange
            // succeeding, so this is a cheap recovery rather than a last resort.
            wire = None;
            reopen.failed(&timings);
        } else {
            reopen.succeeded(&timings);
        }

        let _ = request.reply.send(outcome);

        if lane == Lane::Priority {
            // After the reply, not on receipt: "while the priority queue is non-empty" means
            // until the stop has actually been to the wire.
            pending_priority.fetch_sub(1, Ordering::Release);
        }
    }
}

/// The open port, opening it first if the backoff allows.
async fn ensure_open<'a>(
    wire: &'a mut Option<Box<dyn Wire>>,
    factory: &Arc<dyn WireFactory>,
    reopen: &mut Reopen,
    timings: &SerialTimings,
) -> Result<&'a mut Box<dyn Wire>, String> {
    if wire.is_none() {
        if !reopen.ready() {
            return Err(format!(
                "the link to {} is down and not due to be retried yet",
                factory.describe()
            ));
        }
        match factory.open().await {
            Ok(opened) => *wire = Some(opened),
            Err(error) => {
                reopen.failed(timings);
                return Err(error.to_string());
            }
        }
    }
    wire.as_mut()
        .ok_or_else(|| "the port vanished between opening it and using it".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::{Counts, GetPosition, MountError};
    use crate::skywatcher::mock_port::{MockPort, Scripted};

    /// A link over a mock port, with timings a test can reason about.
    fn link(port: &MockPort, timings: SerialTimings) -> (SerialLink, WatchdogSource) {
        let (sink, source) = watchdog_channel();
        let (link, _task) = SerialLink::spawn(port.factory(), WriteGate::Actions, timings, sink);
        (link, source)
    }

    #[tokio::test(start_paused = true)]
    async fn a_command_gets_its_own_response_type_back() {
        let port = MockPort::new();
        let (link, _watchdog) = link(&port, SerialTimings::default());
        assert_eq!(
            link.send(GetPosition(Axis::Ra))
                .await
                .expect("the spike's own capture"),
            Counts::HOME
        );
        assert_eq!(port.writes()[0].bytes, b":j1\r");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_not_a_miss_and_is_never_retried() {
        // `!1` means the mount received the frame and said no. Retrying produces the same `!1`,
        // and counting it against the heartbeat would report a dead link over a live one.
        let port = MockPort::new();
        let (link, _watchdog) = link(&port, SerialTimings::default());
        port.script([Scripted::Refuses(MountError::InvalidParameter)]);
        let error = link
            .send(GetPosition(Axis::Ra))
            .await
            .expect_err("the mount refused it");
        assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");
        assert_eq!(port.exchanges(), 1, "a refusal is a settled answer");
        assert_eq!(link.consecutive_misses(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_write_gate_stops_an_action_before_the_port_sees_it() {
        let port = MockPort::new();
        let (sink, _source) = watchdog_channel();
        let (link, _task) = SerialLink::spawn(
            port.factory(),
            WriteGate::InquiryOnly,
            SerialTimings::default(),
            sink,
        );
        let error = link
            .send(InstantStop(Axis::Ra))
            .await
            .expect_err("an inquiry-only link must not be able to stop the mount");
        assert!(matches!(error, DeviceError::Protocol(_)), "{error:?}");
        assert!(port.writes().is_empty(), "nothing reached the wire");
        // ...and an inquiry still works on the same link.
        assert!(link.send(GetPosition(Axis::Ra)).await.is_ok());
    }

    #[test]
    fn the_three_configured_timings_come_from_config_and_the_rest_keep_their_defaults() {
        // The seam M3-T04 builds a link through. `mount.serial` has four keys and this uses
        // three; `poll_hz` belongs to whoever drives the poll, which is not this layer.
        let timings = SerialTimings::from(&SerialConfig {
            request_timeout_ms: 250,
            request_retries: 3,
            heartbeat_misses: 5,
            poll_hz: 2,
        });
        assert_eq!(timings.request_timeout, Duration::from_millis(250));
        assert_eq!(timings.retries, 3);
        assert_eq!(timings.heartbeat_misses, 5);
        assert_eq!(
            timings.preempt_after,
            SerialTimings::default().preempt_after,
            "no config key exists for it, so it must not silently become something else"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutting_the_link_down_closes_the_port_and_refuses_further_requests() {
        let port = MockPort::new();
        let (link, _watchdog) = link(&port, SerialTimings::default());
        assert!(link.is_up());
        assert!(link.send(GetPosition(Axis::Ra)).await.is_ok());

        link.shutdown();
        assert!(!link.is_up());
        let error = link
            .send(GetPosition(Axis::Ra))
            .await
            .expect_err("a shut-down link takes no requests");
        assert!(matches!(error, DeviceError::NotConnected), "{error:?}");
        // ...and shutting down did not look like a link failure to the watchdog, which is the
        // whole reason that path is `Fatal` rather than `Miss`.
        assert_eq!(link.consecutive_misses(), 0);
        // Idempotent: a second shutdown, and the `Drop` that will follow, must both be harmless.
        link.shutdown();
    }

    #[tokio::test(start_paused = true)]
    async fn a_dribbled_reply_is_reassembled_rather_than_read_as_a_short_one() {
        // A 9600-baud line delivers a ten-byte reply over ~10 ms, so partial reads are the normal
        // case, not an edge one. A `=00` read as a complete reply would decode as a valid number.
        let port = MockPort::new();
        port.dribbles(true);
        let (link, _watchdog) = link(&port, SerialTimings::default());
        assert_eq!(
            link.send(GetPosition(Axis::Ra)).await.expect("reassembled"),
            Counts::HOME
        );
    }
}
