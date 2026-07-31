//! A scriptable stand-in for the mount's cable (M3-T02).
//!
//! # Why this one is `pub` when the camera's mock is not
//!
//! `gphoto2::mock` is `#[cfg(test)]` and `pub(crate)`, and its own header argues that shipping it
//! would offer a second, worse simulator to anyone who found it. That argument does not transfer,
//! for two reasons. This double sits *below* the HAL — it is a cable, not a device, and there is
//! nothing here a caller could mistake for a mount. And the tests it exists for are integration
//! tests by house convention (`tests/mount_serial.rs`, so that `cargo test t_ser_1` is exactly the
//! gate), which cannot see `#[cfg(test)]` items in `src/`.
//!
//! It is also M3-T04's harness, so the surface below is meant to be read by someone who is not
//! the author.
//!
//! # The model
//!
//! A port, not a mount. [`write`](MockWire::write) stages a reply and the instant it becomes
//! readable; [`read`](MockWire::read) hands it over once that instant arrives, and
//! [`discard_input`](MockWire::discard_input) throws it away. Every delay is
//! `tokio::time::sleep`, so under `tokio::time::pause` a thousand 16 ms exchanges cost no real
//! time and every measured latency is exact rather than approximately right.
//!
//! # Scripting surface
//!
//! ```ignore
//! let port = MockPort::new();                       // answers the spike's captures, 16 ms each
//! port.answers_after(Duration::from_millis(5));     // ...but faster
//! port.answers(b'j', b"=A00080\r");                 // pin one opcode's reply
//! port.script([Scripted::DeadAir, Scripted::Garbles]);  // then a queue of one-shot faults
//! port.dribbles(true);                              // one byte per read, to stress the reassembly
//! port.disconnect();                                // sticky, until `reconnect`
//! port.refuses_opens(2);                            // the next two reopen attempts fail
//!
//! let link = SerialLink::spawn(port.factory(), …);
//! port.writes();                                    // every frame that reached the wire, timed
//! port.exchanges();                                 // how many
//! ```
//!
//! The queue is consulted first and each step is consumed by one exchange; when it is empty the
//! pinned answers are, and when there is no pin the canned table below answers. Which means a
//! test scripts only the fault it is about.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::error::DeviceError;
use tokio::time::Instant;

use super::codec::MountError;
use super::port::{Chunk, Wire, WireFactory, READ_CAPACITY};

/// The measured round trip: 2000 `:j1` exchanges gave min 14.7, p50 15.8, p99 16.9, max 17.2 ms
/// (`spikes/skywatcher-heq5/FINDINGS.md`). The simulator picked a fixed 16 ms over a distribution
/// for the same reason this does — a jittering value would make every duration assertion above it
/// probabilistic to buy nothing (`simulator/profile.rs`).
pub const MEASURED_ROUND_TRIP: Duration = Duration::from_millis(16);

/// What the port does for one exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scripted {
    /// Answer with these exact bytes. Include the `\r` if the exchange should complete.
    Replies(Vec<u8>),
    /// Answer with the mount's own refusal frame — built from [`MountError::digit`] rather than
    /// hard-coded bytes, which is what that method exists for.
    Refuses(MountError),
    /// Answer with the corruption real hardware produced when two frames were written together:
    /// `=0000=000080`, an interleaving whose terminator is in the wrong place
    /// (`spikes/skywatcher-heq5/FINDINGS.md`). Longer than any legal frame, so `decode_reply`
    /// refuses it outright rather than reading the first four characters as a number.
    Garbles,
    /// Answer with a well-formed reply whose *payload* is the wrong width for the opcode. The
    /// other half of "garbled": framing intact, value unusable, caught by `Command::decode`
    /// rather than by `decode_reply`. This is the one a decoder that assumed a width would get
    /// wrong silently.
    Mangles,
    /// Stream junk with no terminator in it at all — a wedged adapter, or the right cable at the
    /// wrong baud rate. The exchange must call this what it is rather than waiting for a `\r`
    /// that is not coming.
    Floods,
    /// Say nothing at all. The exchange runs to its timeout — the measured behaviour of a frame
    /// the mount never answers.
    DeadAir,
    /// Answer as usual, but this long after the write instead of the default round trip.
    After(Duration),
    /// Fail at the port itself, and keep failing until [`MockPort::reconnect`].
    Disconnects,
}

/// A frame that reached the wire, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The bytes, terminator included.
    pub bytes: Vec<u8>,
    /// When they were written. `tokio::time::Instant`, so this is virtual time under
    /// `tokio::time::pause` and a latency measured from it is exact.
    pub at: Instant,
}

impl Written {
    /// The opcode byte, for a test that wants to find one frame among fifty.
    #[must_use]
    pub fn opcode(&self) -> Option<u8> {
        self.bytes.get(1).copied()
    }
}

/// The shared, scriptable state. One per simulated cable; every reopen sees the same one, which
/// is what makes "it came back up and answered" testable.
#[derive(Debug, Default)]
struct Inner {
    script: VecDeque<Scripted>,
    pinned: HashMap<u8, Vec<u8>>,
    round_trip: Option<Duration>,
    dribbles: bool,
    down: bool,
    refuse_opens: usize,
    writes: Vec<Written>,
    exchanges: usize,
    opens: usize,
}

/// A scriptable stand-in for the cable. Cheap to clone; every clone is the same port.
#[derive(Debug, Clone)]
pub struct MockPort {
    inner: Arc<Mutex<Inner>>,
}

impl Default for MockPort {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPort {
    /// A port that answers the spike's own captures, one measured round trip after each write.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// This port as a [`WireFactory`], for [`SerialLink::spawn`](super::serial::SerialLink::spawn).
    #[must_use]
    pub fn factory(&self) -> Arc<dyn WireFactory> {
        Arc::new(self.clone())
    }

    fn with<T>(&self, edit: impl FnOnce(&mut Inner) -> T) -> T {
        // Never across an `.await`: the workspace denies `await_holding_lock` and every caller
        // here is synchronous by construction.
        edit(
            &mut self
                .inner
                .lock()
                .expect("the mock port's lock is never poisoned"),
        )
    }

    // --- scripting -----------------------------------------------------------------------

    /// Queue one-shot steps. Each is consumed by one exchange, in order.
    pub fn script(&self, steps: impl IntoIterator<Item = Scripted>) {
        self.with(|inner| inner.script.extend(steps));
    }

    /// Say nothing for the next `exchanges` exchanges. The shape of a mount that has stopped
    /// answering — which is what the heartbeat counts.
    pub fn goes_quiet_for(&self, exchanges: usize) {
        self.script(std::iter::repeat_n(Scripted::DeadAir, exchanges));
    }

    /// Corrupt the next `exchanges` replies at the framing level.
    pub fn garbles_next(&self, exchanges: usize) {
        self.script(std::iter::repeat_n(Scripted::Garbles, exchanges));
    }

    /// Pin one opcode's reply, overriding the canned table until it is pinned again.
    pub fn answers(&self, opcode: u8, bytes: &[u8]) {
        self.with(|inner| inner.pinned.insert(opcode, bytes.to_vec()));
    }

    /// Change the default write-to-reply delay from [`MEASURED_ROUND_TRIP`].
    pub fn answers_after(&self, delay: Duration) {
        self.with(|inner| inner.round_trip = Some(delay));
    }

    /// Hand replies back one byte per read rather than all at once, so the exchange loop's
    /// reassembly is exercised rather than assumed. A real 9600-baud line delivers a ten-byte
    /// reply over ~10 ms, so this is the more faithful mode; it is off by default only because
    /// most tests are not about reassembly.
    pub fn dribbles(&self, on: bool) {
        self.with(|inner| inner.dribbles = on);
    }

    /// Fail every operation until [`Self::reconnect`] — the cable pulled, or the adapter gone.
    pub fn disconnect(&self) {
        self.with(|inner| inner.down = true);
    }

    /// Bring the port back. Reopening is still needed: an open wire stays broken.
    pub fn reconnect(&self) {
        self.with(|inner| inner.down = false);
    }

    /// Make the next `attempts` calls to [`WireFactory::open`] fail, so a reconnect has to back
    /// off rather than succeed on the first try.
    pub fn refuses_opens(&self, attempts: usize) {
        self.with(|inner| inner.refuse_opens = attempts);
    }

    // --- observation ---------------------------------------------------------------------

    /// Every frame that reached the wire, in order, each with the instant it was written.
    #[must_use]
    pub fn writes(&self) -> Vec<Written> {
        self.with(|inner| inner.writes.clone())
    }

    /// Forget the recorded writes, so a measurement loop can look at one iteration at a time.
    pub fn clear_writes(&self) {
        self.with(|inner| inner.writes.clear());
    }

    /// How many exchanges have been started, script steps included.
    #[must_use]
    pub fn exchanges(&self) -> usize {
        self.with(|inner| inner.exchanges)
    }

    /// How many times the port has been opened. A reconnect is visible here and nowhere else.
    #[must_use]
    pub fn opens(&self) -> usize {
        self.with(|inner| inner.opens)
    }

    /// Whether the port is currently refusing everything.
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.with(|inner| inner.down)
    }
}

#[async_trait::async_trait]
impl WireFactory for MockPort {
    async fn open(&self) -> Result<Box<dyn Wire>, DeviceError> {
        let refused = self.with(|inner| {
            inner.opens += 1;
            if inner.refuse_opens > 0 {
                inner.refuse_opens -= 1;
                true
            } else {
                inner.down
            }
        });
        if refused {
            return Err(DeviceError::Transport(
                "the mock port refused to open".to_owned(),
            ));
        }
        Ok(Box::new(MockWire {
            port: self.clone(),
            staged: None,
        }))
    }

    fn describe(&self) -> String {
        "mock port".to_owned()
    }
}

/// What the mount answers at rest, from the spike's own captures
/// (`spikes/skywatcher-heq5/FINDINGS.md`). `:f` answers `101` — initialised and stopped — because
/// that is the state a driver spends its life in; the uninitialised `100` is one `answers` call
/// away for a test about `F`.
fn canned(opcode: u8) -> Option<&'static [u8]> {
    match opcode {
        b'e' => Some(b"=020401\r"),
        b'a' => Some(b"=00B289\r"),
        b'b' => Some(b"=A7FD00\r"),
        b'g' => Some(b"=10\r"),
        b'j' | b'h' | b'm' => Some(b"=000080\r"),
        b'f' => Some(b"=101\r"),
        // Step period 620, the measured sidereal constant, byte-swapped.
        b'i' => Some(b"=6C0200\r"),
        // Every action answers with a bare `=`; measured on `:F1` and `:F2`.
        action if action.is_ascii_uppercase() => Some(b"=\r"),
        // Everything else is `!0`, which is what the real mount said to `:z1` and `:y1`.
        _ => None,
    }
}

/// One open connection to the mock port.
#[derive(Debug)]
pub struct MockWire {
    port: MockPort,
    /// The reply, and the instant it becomes readable.
    staged: Option<(Instant, VecDeque<u8>)>,
}

impl MockWire {
    fn down_error() -> io::Error {
        io::Error::other("the mock port is disconnected")
    }
}

#[async_trait::async_trait]
impl Wire for MockWire {
    async fn discard_input(&mut self) -> io::Result<()> {
        if self.port.is_down() {
            return Err(Self::down_error());
        }
        // Exactly what a `tcflush(TCIFLUSH)` does to a reply that arrived too late.
        self.staged = None;
        Ok(())
    }

    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let opcode = bytes.get(1).copied().unwrap_or(0);
        let staged = self.port.with(|inner| {
            if inner.down {
                return None;
            }
            inner.exchanges += 1;
            let delay = inner.round_trip.unwrap_or(MEASURED_ROUND_TRIP);
            let (delay, reply) = match inner.script.pop_front() {
                Some(Scripted::Replies(bytes)) => (delay, Some(bytes)),
                Some(Scripted::Refuses(error)) => {
                    (delay, Some(vec![b'!', b'0' + error.digit(), b'\r']))
                }
                Some(Scripted::Garbles) => (delay, Some(b"=0000=000080\r".to_vec())),
                Some(Scripted::Mangles) => (delay, Some(b"=00\r".to_vec())),
                Some(Scripted::Floods) => (delay, Some(vec![b'x'; 40])),
                Some(Scripted::DeadAir) => (delay, None),
                Some(Scripted::After(slow)) => (slow, default_reply(inner, opcode)),
                Some(Scripted::Disconnects) => {
                    inner.down = true;
                    return None;
                }
                None => (delay, default_reply(inner, opcode)),
            };
            inner.writes.push(Written {
                bytes: bytes.to_vec(),
                at: Instant::now(),
            });
            Some((delay, reply))
        });
        let Some((delay, reply)) = staged else {
            return Err(Self::down_error());
        };
        self.staged = reply.map(|bytes| (Instant::now() + delay, bytes.into()));
        Ok(())
    }

    async fn read(&mut self, budget: Duration) -> io::Result<Chunk> {
        if self.port.is_down() {
            return Err(Self::down_error());
        }
        let Some((ready, _)) = self.staged.as_ref() else {
            // Dead air. Consume the whole quantum, exactly as a real read with nothing to read
            // does — a mock that returned instantly would spin the exchange loop and make a
            // timeout cost CPU instead of time.
            tokio::time::sleep(budget).await;
            return Ok(Chunk::empty());
        };
        let now = Instant::now();
        if now < *ready {
            tokio::time::sleep(budget.min(*ready - now)).await;
            if Instant::now() < *ready {
                return Ok(Chunk::empty());
            }
        }
        let dribbles = self.port.with(|inner| inner.dribbles);
        let Some((_, bytes)) = self.staged.as_mut() else {
            return Ok(Chunk::empty());
        };
        let take = if dribbles {
            1
        } else {
            READ_CAPACITY.min(bytes.len())
        };
        let chunk: Vec<u8> = bytes.drain(..take.min(bytes.len())).collect();
        if bytes.is_empty() {
            self.staged = None;
        }
        Ok(Chunk::from_slice(&chunk))
    }
}

/// The pinned answer for this opcode, or the canned one, or `!0`.
fn default_reply(inner: &Inner, opcode: u8) -> Option<Vec<u8>> {
    inner
        .pinned
        .get(&opcode)
        .cloned()
        .or_else(|| Some(canned(opcode).map_or_else(|| b"!0\r".to_vec(), <[u8]>::to_vec)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::{
        Command, Counts, GetHighSpeedRatio, GetPosition, HighSpeedRatio, Initialise,
    };
    use crate::skywatcher::port::{exchange, Exchange, WriteGate, Yield};
    use astroctl_core::types::Axis;

    /// One exchange over an open wire, with no task and no lanes in the way. What this module's
    /// tests are about is the *double*, so the thing under test is the double.
    async fn run<C: Command>(wire: &mut Box<dyn Wire>, cmd: &C) -> Exchange {
        exchange(
            wire.as_mut(),
            cmd.encode(),
            WriteGate::Actions,
            Duration::from_millis(500),
            &Yield::Never,
        )
        .await
    }

    /// Open, exchange once, drop — for tests where the port never goes down.
    async fn once<C: Command>(port: &MockPort, cmd: &C) -> Exchange {
        let mut wire = WireFactory::open(port).await.expect("the mock port opened");
        run(&mut wire, cmd).await
    }

    fn decoded<C: Command>(outcome: &Exchange, cmd: &C) -> C::Response {
        let Exchange::Reply(chunk) = outcome else {
            panic!("expected a reply, got {outcome:?}")
        };
        let reply = crate::skywatcher::codec::decode_reply(chunk.as_bytes()).expect("a reply");
        cmd.decode(reply.payload().expect("not a refusal"))
            .expect("the payload decoded")
    }

    #[tokio::test(start_paused = true)]
    async fn the_default_answers_are_the_spikes_own_captures() {
        // Every one of these is a real request/response pair from the operator's HEQ5. A double
        // that answered plausible invented values would let a decoder bug through.
        let port = MockPort::new();
        let position = GetPosition(Axis::Ra);
        assert_eq!(
            decoded(&once(&port, &position).await, &position),
            Counts::HOME
        );
        let ratio = GetHighSpeedRatio(Axis::Ra);
        assert_eq!(
            decoded(&once(&port, &ratio).await, &ratio),
            HighSpeedRatio(16)
        );
        // And every action answers with the bare `=` that `:F1` and `:F2` were measured to give.
        let init = Initialise(Axis::Dec);
        decoded(&once(&port, &init).await, &init);
    }

    #[tokio::test(start_paused = true)]
    async fn every_scripted_step_does_what_the_module_header_says_it_does() {
        // The scripting surface is M3-T04's harness as much as this task's, so it is checked
        // rather than merely documented — a knob that quietly does nothing would be discovered
        // as a mysterious passing test in somebody else's task.
        let port = MockPort::new();
        let position = GetPosition(Axis::Ra);

        // A pinned answer overrides the canned table, and survives across exchanges.
        port.answers(b'j', b"=A00080\r");
        for _ in 0..2 {
            assert_eq!(
                decoded(&once(&port, &position).await, &position),
                Counts::new(0x0080_00A0).expect("inside the 24-bit domain")
            );
        }

        // A queued step overrides the pin, once.
        port.script([Scripted::Replies(b"=000080\r".to_vec())]);
        assert_eq!(
            decoded(&once(&port, &position).await, &position),
            Counts::HOME
        );
        assert_eq!(
            decoded(&once(&port, &position).await, &position),
            Counts::new(0x0080_00A0).expect("inside the 24-bit domain"),
            "the pin is back once the queue is empty"
        );

        // Delays: the default is the measured round trip, `answers_after` moves it, and
        // `Scripted::After` moves one exchange without disturbing the default.
        let began = Instant::now();
        let _ = once(&port, &position).await;
        assert_eq!(began.elapsed(), MEASURED_ROUND_TRIP);

        port.answers_after(Duration::from_millis(4));
        let began = Instant::now();
        let _ = once(&port, &position).await;
        assert_eq!(began.elapsed(), Duration::from_millis(4));

        port.script([Scripted::After(Duration::from_millis(40))]);
        let began = Instant::now();
        let _ = once(&port, &position).await;
        assert_eq!(began.elapsed(), Duration::from_millis(40));
        let began = Instant::now();
        let _ = once(&port, &position).await;
        assert_eq!(
            began.elapsed(),
            Duration::from_millis(4),
            "back to the default"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn disconnecting_is_sticky_and_reopening_is_what_undoes_it() {
        let port = MockPort::new();
        let position = GetPosition(Axis::Ra);
        let mut wire = WireFactory::open(&port)
            .await
            .expect("the mock port opened");

        // `Scripted::Disconnects` fails the exchange that draws it and leaves the port down, so
        // the *same open wire* keeps failing. Recovery is a reopen, not a retry — which is the
        // property that makes the task's "drop the wire on a transport failure" the right move
        // rather than an over-reaction.
        port.script([Scripted::Disconnects]);
        for attempt in 0..2 {
            assert!(
                matches!(run(&mut wire, &position).await, Exchange::Failed(_)),
                "attempt {attempt}"
            );
        }
        assert!(port.is_down());
        assert!(
            port.writes().is_empty(),
            "a write that failed did not reach the wire and must not be recorded as if it had"
        );

        // While it is down the port will not open at all, which is what a vanished device node
        // does — and `refuses_opens` then models the reopen that loses its race with udev.
        assert!(WireFactory::open(&port).await.is_err());
        port.reconnect();
        port.refuses_opens(1);
        assert!(WireFactory::open(&port).await.is_err());

        let mut wire = WireFactory::open(&port).await.expect("replugged");
        assert_eq!(
            decoded(&run(&mut wire, &position).await, &position),
            Counts::HOME
        );
    }
}
