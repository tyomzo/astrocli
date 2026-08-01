//! `SkywatcherMount` against a mount that answers bytes (M3-T04).
//!
//! # What is different about this file
//!
//! `src/skywatcher/mount.rs`'s own tests drive the driver over
//! [`MockPort`](astroctl_drivers::skywatcher::mock_port::MockPort), which is a *cable*: it answers
//! the spike's captures and scripts the ways a cable fails. That is the right double for a
//! handshake, a fault, or a latency measurement, and it is the wrong one for a goto — because a
//! goto's pre-motion readback asks the mount what it thinks it was told, and a port that answers
//! `:h` with the same string forever cannot be told anything.
//!
//! So this file carries a second double, [`SyntaMount`], which models the four registers a goto
//! writes and the motion that follows. It is deliberately **not** in `src/`: a register-modelling
//! mount is a mount simulator, `SimulatorMount` already is one, and shipping a second would offer
//! anyone who found it a worse copy of a device that exists. `mock_port` is `pub` because it is a
//! cable and there is nothing there a caller could mistake for a mount; this is the other case.
//!
//! Every test runs on `#[tokio::test(start_paused = true)]`, so the 16 ms round trips and the
//! 500 ms goto polls cost no real time and every latency assertion is exact rather than
//! approximately right.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::config::{
    MountConfig, MountDriver, MountLimits, ParkPosition, SerialConfig, SiteConfig,
};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{Axis, MountState, RaDec, SlewSpeed, TrackingMode};
use astroctl_drivers::skywatcher::codec::{decode_u24, encode_u24, U24};
use astroctl_drivers::skywatcher::port::{Chunk, Wire, WireFactory, READ_CAPACITY};
use astroctl_drivers::skywatcher::{FixedSiderealTime, SkywatcherMount};
use astroctl_hal::mount::MountDevice;
use tokio::time::Instant;

// -----------------------------------------------------------------------------------------
// The measured constants of the operator's HEQ5. **Fixtures**, per PRD §4.2.
// -----------------------------------------------------------------------------------------

/// `:a1`/`:a2` → `00B289`.
const CPR: u32 = 9_024_000;
/// `:b1`/`:b2` → `A7FD00`.
const TIMER_HZ: u32 = 64_935;
/// The counter at power-on, both axes.
const HOME: u32 = 0x0080_0000;
/// The counter's own modulus. Not the mechanism's — that is [`CPR`], and the gap between them is
/// the 50.7° hazard `AxisScale::reachable_delta` exists for.
const COUNTER_MODULUS: i64 = 0x0100_0000;
/// 2000 `:j1` exchanges gave min 14.7, p50 15.8, max 17.2 ms.
const ROUND_TRIP: Duration = Duration::from_millis(16);
/// How long this double takes to complete a bounded goto. Three 500 ms polls' worth, so a test
/// sees the axis running before it sees it arrive.
const TRAVEL: Duration = Duration::from_millis(1_200);

// -----------------------------------------------------------------------------------------
// The double
// -----------------------------------------------------------------------------------------

/// A fault, armed against the next frame carrying a given opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Say nothing. The exchange runs to its timeout.
    DeadAir,
    /// `!<digit>` — the mount understood and refused. `2` is "motor not stopped", which is what a
    /// real mount answers to a `J` issued against an axis that is still moving.
    Refuse(u8),
    /// A reply whose framing survives and whose payload is the wrong width — the half of "garbled"
    /// only `Command::decode` can see.
    Mangle,
    /// Fail the port from here on, until [`SyntaMount::replug`].
    Unplug,
}

/// One axis's registers and the motion they describe.
#[derive(Debug)]
struct AxisState {
    position: u32,
    goto_target: u32,
    break_point: u32,
    step_period: u32,
    /// From the last `G`: bounded or unbounded, fast or slow, and which way.
    bounded: bool,
    high_speed: bool,
    backward: bool,
    running: bool,
    initialised: bool,
    /// When a bounded motion lands. `None` while nothing is under way.
    lands_at: Option<Instant>,
    /// Added to the `:h` reply — the byte-swap fault, simulated at the register the mount reports.
    corrupt_target_by: i64,
}

impl AxisState {
    fn new() -> Self {
        Self {
            position: HOME,
            goto_target: HOME,
            break_point: HOME,
            step_period: 0,
            bounded: false,
            high_speed: false,
            backward: false,
            running: false,
            // What the mount reads after `:F`, which is the state it spends its life in.
            initialised: true,
            lands_at: None,
            corrupt_target_by: 0,
        }
    }

    /// Advance the model to `now`.
    ///
    /// A bounded motion lands **exactly** on its target, which is not generosity: E13 measured
    /// zero counts of error across six gotos from 0.04° to 4° in both directions, so a double that
    /// scattered the arrival would be modelling a mount nobody has seen.
    fn tick(&mut self, now: Instant) {
        if let Some(at) = self.lands_at {
            if now >= at {
                self.position = self.goto_target;
                self.running = false;
                self.lands_at = None;
            }
        }
    }

    /// Apply a signed displacement to a 24-bit counter.
    fn displaced(&self, magnitude: u32) -> u32 {
        let signed = i64::from(magnitude) * if self.backward { -1 } else { 1 };
        u32::try_from((i64::from(self.position) + signed).rem_euclid(COUNTER_MODULUS))
            .expect("a value reduced modulo 2^24 fits a u32")
    }

    /// The `:f` payload — three nibbles, the layout `AxisStatus` decodes.
    fn status_payload(&self) -> String {
        let mut n1 = 0u8;
        if !self.bounded {
            n1 |= 0x01; // SLEW
        }
        if self.backward {
            n1 |= 0x02;
        }
        if self.high_speed {
            n1 |= 0x04;
        }
        format!(
            "{n1:X}{}{}",
            u8::from(self.running),
            u8::from(self.initialised)
        )
    }
}

#[derive(Debug)]
struct Inner {
    ra: AxisState,
    dec: AxisState,
    /// Frames that reached the wire, in order, with the instant each was written.
    writes: Vec<(String, Instant)>,
    faults: VecDeque<(u8, Fault)>,
    down: bool,
    /// Answer `:a` with this instead of [`CPR`], for a mount with different gearing.
    counts_per_revolution: u32,
}

impl Inner {
    fn axis(&mut self, digit: u8) -> &mut AxisState {
        if digit == b'2' {
            &mut self.dec
        } else {
            &mut self.ra
        }
    }

    /// The armed fault for this opcode, if the next queued one matches it.
    fn take_fault(&mut self, opcode: u8) -> Option<Fault> {
        if self.faults.front().is_some_and(|(op, _)| *op == opcode) {
            self.faults.pop_front().map(|(_, fault)| fault)
        } else {
            None
        }
    }
}

/// A Sky-Watcher motor controller that answers bytes.
#[derive(Debug, Clone)]
struct SyntaMount {
    inner: Arc<Mutex<Inner>>,
}

impl SyntaMount {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                ra: AxisState::new(),
                dec: AxisState::new(),
                writes: Vec::new(),
                faults: VecDeque::new(),
                down: false,
                counts_per_revolution: CPR,
            })),
        }
    }

    fn with<T>(&self, edit: impl FnOnce(&mut Inner) -> T) -> T {
        edit(
            &mut self
                .inner
                .lock()
                .expect("the double's lock is never poisoned"),
        )
    }

    fn factory(&self) -> Arc<dyn WireFactory> {
        Arc::new(self.clone())
    }

    /// Put an axis counter somewhere other than home.
    fn park_counter(&self, axis: Axis, counts: u32) {
        self.with(|inner| {
            let state = if axis == Axis::Ra {
                &mut inner.ra
            } else {
                &mut inner.dec
            };
            state.position = counts;
            state.goto_target = counts;
        });
    }

    /// Arm a fault against the next frame carrying `opcode`.
    fn then(&self, opcode: u8, fault: Fault) {
        self.with(|inner| inner.faults.push_back((opcode, fault)));
    }

    /// Report `:h` wrong by `delta` on one axis — the byte-swapped increment, at the register the
    /// mount reports rather than at the bytes, so the *check* is what is under test.
    fn corrupt_goto_readback(&self, axis: Axis, delta: i64) {
        self.with(|inner| {
            let state = if axis == Axis::Ra {
                &mut inner.ra
            } else {
                &mut inner.dec
            };
            state.corrupt_target_by = delta;
        });
    }

    fn replug(&self) {
        self.with(|inner| inner.down = false);
    }

    /// Every frame that reached the wire.
    fn frames(&self) -> Vec<String> {
        self.with(|inner| inner.writes.iter().map(|(f, _)| f.clone()).collect())
    }

    /// When a frame was written, by its exact text.
    fn written_at(&self, frame: &str) -> Option<Instant> {
        self.with(|inner| {
            inner
                .writes
                .iter()
                .find(|(text, _)| text == frame)
                .map(|(_, at)| *at)
        })
    }

    fn clear_frames(&self) {
        self.with(|inner| inner.writes.clear());
    }

    fn counter(&self, axis: Axis) -> u32 {
        self.with(|inner| {
            if axis == Axis::Ra {
                inner.ra.position
            } else {
                inner.dec.position
            }
        })
    }

    fn running(&self, axis: Axis) -> bool {
        self.with(|inner| {
            let now = Instant::now();
            let state = if axis == Axis::Ra {
                &mut inner.ra
            } else {
                &mut inner.dec
            };
            state.tick(now);
            state.running
        })
    }

    /// The reply to one request frame, or `None` for dead air.
    fn reply(&self, frame: &str) -> Option<Vec<u8>> {
        let now = Instant::now();
        self.with(|inner| {
            let bytes = frame.as_bytes();
            let opcode = bytes.get(1).copied().unwrap_or(b'?');
            let digit = bytes.get(2).copied().unwrap_or(b'1');
            let payload = frame.get(3..).unwrap_or_default().trim_end().to_owned();

            match inner.take_fault(opcode) {
                Some(Fault::Unplug) => {
                    inner.down = true;
                    return None;
                }
                Some(Fault::DeadAir) => {
                    inner.writes.push((frame.trim_end().to_owned(), now));
                    return None;
                }
                Some(Fault::Refuse(code)) => {
                    inner.writes.push((frame.trim_end().to_owned(), now));
                    return Some(vec![b'!', b'0' + code, b'\r']);
                }
                Some(Fault::Mangle) => {
                    inner.writes.push((frame.trim_end().to_owned(), now));
                    return Some(b"=00\r".to_vec());
                }
                None => {}
            }
            inner.writes.push((frame.trim_end().to_owned(), now));

            let cpr = inner.counts_per_revolution;
            // The magnitude a write carried, decoded the way the mount decodes it.
            let written = decode_u24(&payload, '?').map(U24::get).unwrap_or(0);
            let state = inner.axis(digit);
            state.tick(now);

            let payload = match opcode {
                // --- inquiries ------------------------------------------------------------
                b'e' => "020401".to_owned(), // firmware 2.4, model code 1: an HEQ5
                b'a' => hex(cpr),
                b'b' => hex(TIMER_HZ),
                b'g' => "10".to_owned(), // high-speed ratio 16
                b'j' => hex(state.position),
                b'f' => state.status_payload(),
                b'h' => hex(u32::try_from(
                    (i64::from(state.goto_target) + state.corrupt_target_by)
                        .rem_euclid(COUNTER_MODULUS),
                )
                .expect("reduced modulo 2^24")),
                b'm' => hex(state.break_point),
                b'i' => hex(state.step_period),

                // --- actions --------------------------------------------------------------
                b'G' => {
                    // The mode digit is a four-row table, not a bit field: `0` GOTO/high,
                    // `1` SLEW/low, `2` GOTO/low, `3` SLEW/high. A double that computed it
                    // arithmetically would agree with a driver that made the same mistake.
                    let mode = payload.as_bytes().first().copied().unwrap_or(b'0');
                    state.bounded = matches!(mode, b'0' | b'2');
                    state.high_speed = matches!(mode, b'0' | b'3');
                    state.backward = payload.as_bytes().get(1).copied() == Some(b'1');
                    String::new()
                }
                b'I' => {
                    state.step_period = written;
                    String::new()
                }
                b'H' => {
                    // A **relative** increment whose sign lives in the last `G`, read back as an
                    // absolute counter. That asymmetry is the whole reason the readback catches a
                    // byte-swapped write.
                    state.goto_target = state.displaced(written);
                    String::new()
                }
                b'M' => {
                    state.break_point = state.displaced(written);
                    String::new()
                }
                b'J' => {
                    state.running = true;
                    state.lands_at = state.bounded.then(|| now + TRAVEL);
                    String::new()
                }
                b'K' | b'L' => {
                    state.running = false;
                    state.lands_at = None;
                    String::new()
                }
                b'F' => {
                    state.initialised = true;
                    String::new()
                }
                b'P' => String::new(),
                // What the real mount said to `:z1` and `:y1`.
                _ => return Some(b"!0\r".to_vec()),
            };
            let mut reply = Vec::with_capacity(payload.len() + 2);
            reply.push(b'=');
            reply.extend_from_slice(payload.as_bytes());
            reply.push(b'\r');
            Some(reply)
        })
    }
}

/// A 24-bit value as the mount transmits it — byte-swapped ASCII hex.
fn hex(value: u32) -> String {
    String::from_utf8(encode_u24(U24::new(value & 0x00FF_FFFF).expect("masked")).to_vec())
        .expect("hex is ASCII")
}

#[async_trait::async_trait]
impl WireFactory for SyntaMount {
    async fn open(&self) -> Result<Box<dyn Wire>, DeviceError> {
        if self.with(|inner| inner.down) {
            return Err(DeviceError::Transport(
                "the mount's adapter is unplugged".to_owned(),
            ));
        }
        Ok(Box::new(SyntaWire {
            mount: self.clone(),
            staged: None,
        }))
    }

    fn describe(&self) -> String {
        "a Sky-Watcher motor controller (test double)".to_owned()
    }
}

/// One open connection to the double.
#[derive(Debug)]
struct SyntaWire {
    mount: SyntaMount,
    staged: Option<(Instant, VecDeque<u8>)>,
}

#[async_trait::async_trait]
impl Wire for SyntaWire {
    async fn discard_input(&mut self) -> io::Result<()> {
        if self.mount.with(|inner| inner.down) {
            return Err(io::Error::other("the mount's adapter is unplugged"));
        }
        self.staged = None;
        Ok(())
    }

    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.mount.with(|inner| inner.down) {
            return Err(io::Error::other("the mount's adapter is unplugged"));
        }
        let frame = String::from_utf8_lossy(bytes).into_owned();
        let reply = self.mount.reply(&frame);
        if self.mount.with(|inner| inner.down) {
            // `Fault::Unplug` fires during the reply, so the write that drew it fails.
            return Err(io::Error::other("the mount's adapter is unplugged"));
        }
        self.staged = reply.map(|bytes| (Instant::now() + ROUND_TRIP, bytes.into()));
        Ok(())
    }

    async fn read(&mut self, budget: Duration) -> io::Result<Chunk> {
        if self.mount.with(|inner| inner.down) {
            return Err(io::Error::other("the mount's adapter is unplugged"));
        }
        let Some((ready, _)) = self.staged.as_ref() else {
            // Consume the whole quantum, exactly as a real read with nothing to read does.
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
        let Some((_, bytes)) = self.staged.as_mut() else {
            return Ok(Chunk::empty());
        };
        let take = READ_CAPACITY.min(bytes.len());
        let chunk: Vec<u8> = bytes.drain(..take).collect();
        if bytes.is_empty() {
            self.staged = None;
        }
        Ok(Chunk::from_slice(&chunk))
    }
}

// -----------------------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------------------

/// Oslo — the site `config/field-node.example.yaml` ships.
fn site() -> SiteConfig {
    SiteConfig {
        latitude: 59.9139,
        longitude: 10.7522,
        elevation: 25.0,
        timezone: "Europe/Oslo".to_owned(),
    }
}

fn config(settle_seconds: u32) -> MountConfig {
    MountConfig {
        driver: MountDriver::Skywatcher,
        port: "auto".to_owned(),
        baud: 9600,
        park_position: ParkPosition {
            ra_hours: 0.0,
            dec_degrees: 90.0,
        },
        settle_time_seconds: settle_seconds,
        serial: SerialConfig {
            request_timeout_ms: 500,
            request_retries: 1,
            heartbeat_misses: 3,
            poll_hz: 1,
        },
        limits: MountLimits {
            min_altitude_degrees: 15.0,
            meridian_limit_minutes: 15.0,
            slew_ttl_default_ms: 500,
            slew_ttl_max_ms: 2000,
        },
        indi_device: None,
        ascom_host: None,
    }
}

/// A connected driver over the double, with the sky held still at LST 12 h.
///
/// Fixed rather than turning, and that is what makes the assertions exact: a goto's target counter
/// depends on the sidereal time it was computed at, so a clock that advanced between the solution
/// and the readback would make every expected counter a range instead of a number.
async fn connected(mount: &SyntaMount, settle_seconds: u32) -> Arc<SkywatcherMount> {
    let driver = Arc::new(
        SkywatcherMount::over_wire(
            &config(settle_seconds),
            site(),
            Arc::new(FixedSiderealTime(180.0)),
            mount.factory(),
        )
        .expect("the example park position is a coordinate"),
    );
    driver
        .connect()
        .await
        .expect("the double answers a handshake");
    driver
}

/// The right ascension whose hour angle puts the RA axis at `offset` counts from home.
///
/// Expressed as arithmetic rather than a decimal, so the expected counter below is exact rather
/// than exact-to-within-rounding.
///
/// `RA = LST − HA` with `HA = s·(h + 90°)` (northern, so `s = +1`) and LST pinned at 12 h by
/// [`connected`]. In hours: `RA = 12 − (h + 90°)/15 = 6 − h_hours`. **The `6` was a `12` before
/// M3-T06** — home is six hours west of the meridian, not on it, so the right ascension that
/// parks the axis at a given counter is six hours earlier than this suite used to believe.
///
/// Every counter expectation downstream is unchanged, and that is the point: the axis offsets
/// these tests assert are mechanical facts about the moves, and only the sky coordinate that
/// asks for them moved.
fn ra_hours_for_axis_offset(offset: f64) -> f64 {
    (6.0 - offset / f64::from(CPR) * 24.0).rem_euclid(24.0)
}

/// The declination that leaves the DEC axis exactly where it is.
fn dec_degrees_for_axis_counts(counts: u32) -> f64 {
    let axis_degrees = (f64::from(counts) - f64::from(HOME)) / f64::from(CPR) * 360.0;
    90.0 - axis_degrees
}

// -----------------------------------------------------------------------------------------
// The session
// -----------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_complete_session_connects_tracks_gotos_and_stops_with_the_frames_sdd_5_2_mandates() {
    // The acceptance criterion, whole: connect → track → goto (completion detected) → estop, with
    // the byte stream asserted against the sequence the SDD specifies rather than against whatever
    // the driver happened to send.
    let mount = SyntaMount::new();
    // Off the pole on both axes, so neither the geometry nor the goto is degenerate.
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000); // DEC axis at +45°, i.e. declination +45°
    let driver = connected(&mount, 0).await;

    assert_eq!(
        mount.frames(),
        vec![":e1", ":a1", ":b1", ":g1", ":e2", ":a2", ":b2", ":g2", ":f1", ":f2"],
        "the handshake of SDD §5.2.2, both axes, and not one action opcode"
    );

    // --- tracking -------------------------------------------------------------------------
    mount.clear_frames();
    driver
        .start_tracking(TrackingMode::Sidereal)
        .await
        .expect("sidereal is in range");
    assert_eq!(
        mount.frames(),
        vec![":G110", ":I16C0200", ":J1"],
        "`motion.py` drove exactly these for the run E10 measured"
    );
    assert!(
        mount.running(Axis::Ra),
        "tracking is a slew, and it is running"
    );

    // --- goto -----------------------------------------------------------------------------
    // A target that moves right ascension only: the declination axis is already where +45° puts
    // it, so the driver must send nothing at all on axis 2 (a move of zero has no inside for a
    // break point, and the codec refuses one).
    let ra_target = ra_hours_for_axis_offset(400_000.0);
    let dec_target = dec_degrees_for_axis_counts(HOME + 1_128_000);
    mount.clear_frames();

    driver
        .goto(RaDec::from_parts(ra_target, dec_target).expect("a valid coordinate"))
        .await
        .expect("the goto completed");

    let frames = mount.frames();
    // `:j1 :j2` — the counters the program is built against, read with the axes stopped.
    assert_eq!(&frames[..2], &[":j1", ":j2"]);
    // Then the eight frames of SDD §5.2.2's goto, right ascension only.
    let goto: Vec<&String> = frames[2..10].iter().collect();
    let opcodes: String = goto
        .iter()
        .map(|frame| char::from(frame.as_bytes()[1]))
        .collect();
    assert_eq!(
        opcodes, "GIHMhmiJ",
        "four writes, three readbacks, then `J` — and never `J` first: {goto:?}"
    );
    assert!(
        goto.iter().all(|frame| frame.as_bytes()[2] == b'1'),
        "the declination axis was already there and must not be commanded: {goto:?}"
    );
    // Mode digit `0` is GOTO at the *high* class — 200,000 counts is sixteen times the half-degree
    // crossover, which is what `goto_speed_class` is for. The digit table is not a bit field, and
    // `0` mistyped for `2` is a factor of sixteen in speed rather than a wrong direction.
    assert_eq!(goto[0], &":G100", "GOTO, high speed, forward");

    // The completion poll, at 2 Hz, `:f` then `:j` per SDD §5.2.3.
    let poll: Vec<&String> = frames[10..].iter().collect();
    assert!(
        poll.windows(2)
            .any(|pair| pair[0] == ":f1" && pair[1] == ":j1"),
        "the completion poll reads the status and the counter: {poll:?}"
    );
    assert!(
        poll.iter().filter(|frame| **frame == ":f1").count() >= 2,
        "1.2 s of travel at 2 Hz is at least two polls: {poll:?}"
    );

    // ...and it landed on the counter the solution named, exactly.
    assert_eq!(mount.counter(Axis::Ra), HOME + 400_000);
    assert_eq!(mount.counter(Axis::Dec), HOME + 1_128_000);
    assert!(
        mount.running(Axis::Ra),
        "the axis is running again — because tracking was restored, and on a Synta mount tracking \
         *is* a slew. `:f` alone cannot tell that from a manual one, which is why the driver keeps \
         its own record of what it asked for."
    );
    assert!(
        !mount.running(Axis::Dec),
        "and nothing turns the declination axis"
    );

    // SES-06: tracking was running when the goto began, so it is running now.
    let tail: Vec<&String> = frames[frames.len() - 3..].iter().collect();
    assert_eq!(
        tail,
        vec![":G110", ":I16C0200", ":J1"],
        "the goto restores tracking before it returns (SES-06): {frames:?}"
    );
    assert_eq!(
        driver.status().await.expect("status").state,
        MountState::Tracking
    );

    // --- emergency stop -------------------------------------------------------------------
    mount.clear_frames();
    driver.emergency_stop().await.expect("both axes stopped");
    assert_eq!(
        mount.frames(),
        vec![":L1", ":L2"],
        "instant stop on both axes, and nothing else"
    );
    assert!(!mount.running(Axis::Ra));
    let status = driver.status().await.expect("status");
    assert_eq!(status.state, MountState::Idle);
    assert_eq!(
        status.tracking, None,
        "on a Synta mount tracking *is* a slew, so `L` stopped it"
    );
}

#[tokio::test(start_paused = true)]
async fn a_corrupted_goto_register_stops_the_motion_before_any_motor_is_commanded() {
    // SDD §5.2.2's pre-motion readback, at the driver. The mount answers a request for a
    // nonexistent axis with plausible data, so nothing but a readback catches a mis-encoded write
    // — and the only correct response is to never send `J`.
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 0).await;
    mount.clear_frames();

    // 12,345 = 0x003039; with its bytes transposed it is 0x393000, so the readback is off by the
    // difference. The exact fault SDD §5.2.2 names.
    mount.corrupt_goto_readback(Axis::Ra, 0x0039_3000 - 12_345);

    let error = driver
        .goto(
            RaDec::from_parts(
                ra_hours_for_axis_offset(400_000.0),
                dec_degrees_for_axis_counts(HOME + 1_128_000),
            )
            .expect("valid"),
        )
        .await
        .expect_err("a goto whose registers disagree must not start");
    assert!(matches!(error, DeviceError::Protocol(_)), "{error:?}");

    let frames = mount.frames();
    assert!(
        !frames.iter().any(|frame| frame.starts_with(":J")),
        "no motion command may reach the wire after a failed readback: {frames:?}"
    );
    assert!(
        frames.iter().any(|frame| frame.starts_with(":h1")),
        "the readback must actually have been performed: {frames:?}"
    );
    assert!(!mount.running(Axis::Ra) && !mount.running(Axis::Dec));
    assert_eq!(mount.counter(Axis::Ra), HOME + 200_000, "nothing moved");
}

#[tokio::test(start_paused = true)]
async fn a_goto_near_the_counter_wrap_goes_the_long_way_round_rather_than_50_degrees_wrong() {
    // M3-T03's finding, end to end through the driver. The counter wraps at 2²⁴ and the mechanism
    // at 9,024,000, and 2²⁴ mod CPR is 7,753,216 — so a move whose destination falls outside
    // `[0, 2²⁴)` lands on a counter that decodes **50.7° wrong**, in the mount as much as in the
    // arithmetic. `reachable_delta` takes the shortest move that stays inside the counter; this
    // asserts that the *driver* therefore programs the long way round and lands where it meant to.
    let mount = SyntaMount::new();
    // The bottom of the canonical band: RA axis at −180°, 3,876,608 counts. A shortest-path move
    // of −4,000,000 from here would arrive at −123,392, which is not a counter.
    let from_ra = HOME - CPR / 2; // 3,876,608
    mount.park_counter(Axis::Ra, from_ra);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 0).await;
    mount.clear_frames();

    // The RA axis wanted at +20.4255° — 512,000 counts past home, i.e. counter 8,900,608.
    let target = RaDec::from_parts(
        ra_hours_for_axis_offset(512_000.0),
        dec_degrees_for_axis_counts(HOME + 1_128_000),
    )
    .expect("valid");
    driver.goto(target).await.expect("the goto completed");

    // The geometric shortest path is −4,000,000. The one the driver must have taken is +5,024,000,
    // which is 1,024,000 counts further and the only one that is a counter the whole way.
    let frames = mount.frames();
    let increment = frames
        .iter()
        .find(|frame| frame.starts_with(":H1"))
        .expect("an increment was written");
    assert_eq!(
        increment, ":H100A94C",
        "5,024,000 = 0x4CA900, transmitted low byte first — the long way round, not −4,000,000"
    );
    let mode = frames
        .iter()
        .find(|frame| frame.starts_with(":G1"))
        .expect("a mode was written");
    assert_eq!(
        mode, ":G100",
        "GOTO, high speed, and direction digit `0` — *forward*, the long way. The geometrically \
         shorter move is backward, and taking it would drive the counter below zero."
    );

    assert_eq!(
        mount.counter(Axis::Ra),
        8_900_608,
        "and it landed on the counter the solution named"
    );
    // The proof the hazard was avoided: the arrival is a 24-bit counter, and the *angle* it
    // decodes to is the one asked for rather than 50.7° from it.
    let position = driver.position().await.expect("polled");
    assert!(
        (position.ra.hours() - target.ra.hours()).abs() < 1e-6,
        "landed at {} h, meant {} h — a 50.7° error is 3.38 h",
        position.ra.hours(),
        target.ra.hours()
    );
}

#[tokio::test(start_paused = true)]
async fn an_emergency_stop_during_the_completion_poll_reaches_the_wire_inside_the_budget() {
    // T-SER-3's conditions, at the driver: a goto is in flight, its 2 Hz poll is on the cable, and
    // the stop must not queue behind it.
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 0).await;

    let target = RaDec::from_parts(
        ra_hours_for_axis_offset(400_000.0),
        dec_degrees_for_axis_counts(HOME + 1_128_000),
    )
    .expect("valid");
    let slewing = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.goto(target).await })
    };

    // Into the poll loop: programming is eight frames (~128 ms) and the first poll lands at 500 ms.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(mount.running(Axis::Ra), "the goto is under way");
    mount.clear_frames();

    let began = Instant::now();
    driver.emergency_stop().await.expect("stops");
    let landed = mount
        .written_at(":L1")
        .expect("the emergency stop reached the wire");
    let latency = landed.saturating_duration_since(began);
    assert!(
        latency <= Duration::from_millis(20),
        "the stop took {latency:?} to reach the wire, past SDD §5.8.2's 20 ms budget"
    );

    assert!(!mount.running(Axis::Ra), "the axis is stopped");
    let outcome = slewing.await.expect("the supervisor ran");
    assert!(
        matches!(outcome, Err(DeviceError::Aborted(_))),
        "an e-stopped goto is `Aborted` (409), never `Rejected` (422): {outcome:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_goto_that_is_refused_at_j_leaves_the_mount_stopped() {
    // `!2` is "motor not stopped" — what a real mount answers to a `J` issued against an axis that
    // is still moving. The refusal is a settled answer from a healthy link, so it must reach the
    // caller as a rejection and must not be retried into a heartbeat loss.
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 0).await;
    mount.clear_frames();
    mount.then(b'J', Fault::Refuse(2));

    let error = driver
        .goto(
            RaDec::from_parts(
                ra_hours_for_axis_offset(400_000.0),
                dec_degrees_for_axis_counts(HOME + 1_128_000),
            )
            .expect("valid"),
        )
        .await
        .expect_err("the mount refused to start");
    assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");

    let frames = mount.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.starts_with(":J"))
            .count(),
        1,
        "a refusal is a settled answer and must never be retried: {frames:?}"
    );
    assert!(
        frames.iter().any(|frame| frame.starts_with(":K")),
        "a goto that fails mid-motion leaves the mount stopped (SDD §5.1): {frames:?}"
    );
    assert!(!mount.running(Axis::Ra) && !mount.running(Axis::Dec));
}

#[tokio::test(start_paused = true)]
async fn dead_air_in_the_middle_of_programming_a_goto_stops_what_had_started() {
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 0).await;
    mount.clear_frames();
    // Silent from the goto increment onwards. One retry, then the request gives up.
    mount.then(b'H', Fault::DeadAir);
    mount.then(b'H', Fault::DeadAir);

    let error = driver
        .goto(
            RaDec::from_parts(
                ra_hours_for_axis_offset(400_000.0),
                dec_degrees_for_axis_counts(HOME + 1_128_000),
            )
            .expect("valid"),
        )
        .await
        .expect_err("a mount that stopped answering cannot be sent a goto");
    assert!(matches!(error, DeviceError::Timeout(_)), "{error:?}");

    let frames = mount.frames();
    assert!(
        !frames.iter().any(|frame| frame.starts_with(":J")),
        "nothing may start after the programming failed: {frames:?}"
    );
    assert!(
        frames.iter().any(|frame| frame.starts_with(":K")),
        "and the failure path stops both axes anyway: {frames:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_garbled_position_reply_mid_poll_fails_the_goto_rather_than_moving_the_target() {
    // The half of "garbled" only `Command::decode` can see: framing intact, payload the wrong
    // width. A driver that shrugged at it would compare the target against a number it invented.
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 0).await;

    let target = RaDec::from_parts(
        ra_hours_for_axis_offset(400_000.0),
        dec_degrees_for_axis_counts(HOME + 1_128_000),
    )
    .expect("valid");
    let slewing = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.goto(target).await })
    };
    // Past the programming, into the poll loop, then break the counter reads.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for _ in 0..4 {
        mount.then(b'j', Fault::Mangle);
    }

    let outcome = slewing.await.expect("the supervisor ran");
    let error = outcome.expect_err("a goto cannot complete against a garbled counter");
    assert!(matches!(error, DeviceError::Protocol(_)), "{error:?}");
    assert!(
        mount.frames().iter().any(|frame| frame.starts_with(":K")),
        "the failure path stops the axes"
    );
}

#[tokio::test(start_paused = true)]
async fn an_adapter_pulled_mid_slew_becomes_a_heartbeat_loss_and_a_fault_state() {
    // The mount keeps slewing — an unplugged adapter does not stop a telescope — and the driver's
    // job is to say so rather than to go quiet. REL-02's watchdog is the consumer.
    let mount = SyntaMount::new();
    let driver = connected(&mount, 0).await;
    driver
        .slew(
            Axis::Ra,
            astroctl_core::types::Direction::West,
            SlewSpeed::Fast,
        )
        .await
        .expect("slews");
    assert!(mount.running(Axis::Ra));

    mount.then(b'f', Fault::Unplug);
    // `heartbeat_misses` is 3, and one status poll is one failed request once the link is down, so
    // it takes three of them to cross the threshold. Below it the last known state stands, which
    // is deliberate: a single dropped frame must not turn the panel red.
    for _ in 0..3 {
        let _ = driver.status().await;
    }

    let status = driver
        .status()
        .await
        .expect("a mount that stopped answering still has a status");
    assert_eq!(
        status.state,
        MountState::Fault,
        "\"the mount is not answering\" is a state the operator must see, not a blank panel"
    );
    assert!(
        status.slewing,
        "and it is still slewing — hiding the motion would make the scariest state look calmest"
    );
    assert!(status.is_consistent());

    // The watchdog was told, once.
    let heartbeat = driver.take_watchdog();
    assert!(heartbeat.is_some(), "the heartbeat seam is exposed");

    // ...and replugging recovers, because the serial task reopens the port — after its backoff,
    // which doubles on every failure and is capped at five seconds. Waiting it out is the
    // assertion: recovery is not instant, and a test that pretended otherwise would be asserting
    // against a reopen-at-request-rate loop rather than against the design.
    mount.replug();
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
        driver.status().await.expect("status").state,
        MountState::Slewing,
        "the mount never stopped slewing; only the cable went away"
    );
}

#[tokio::test(start_paused = true)]
async fn a_dropped_goto_future_still_lands_the_slew_and_restores_tracking() {
    // HAL rule 3 and SES-06 in one assertion, which is the pair that forced the goto's supervision
    // into a task the driver owns: the API answers `202` and drops this future on every goto, and
    // a mount whose tracking restore lived in the dropped future would end every slew stopped.
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 0).await;
    driver
        .start_tracking(TrackingMode::Sidereal)
        .await
        .expect("tracks");
    mount.clear_frames();

    let target = RaDec::from_parts(
        ra_hours_for_axis_offset(400_000.0),
        dec_degrees_for_axis_counts(HOME + 1_128_000),
    )
    .expect("valid");
    // `select!` takes the future by value and drops the losing branch — precisely what a handler
    // answering `202` does to the future it is holding.
    tokio::select! {
        biased;
        () = tokio::time::sleep(Duration::from_millis(200)) => {}
        _ = driver.goto(target) => panic!("the goto is slower than this"),
    }

    // Nobody is waiting on it any more. It must still finish.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        mount.counter(Axis::Ra),
        HOME + 400_000,
        "the slew completed"
    );
    let frames = mount.frames();
    assert_eq!(
        &frames[frames.len() - 3..],
        &[":G110", ":I16C0200", ":J1"],
        "and tracking was restored (SES-06) with nobody holding the future: {frames:?}"
    );

    // ...and the axes are free again, so the operator's next goto is not answered `Busy` forever.
    assert_eq!(
        driver.status().await.expect("status").state,
        MountState::Tracking
    );
    driver
        .goto(target)
        .await
        .expect("a second goto is accepted");
}

#[tokio::test(start_paused = true)]
async fn a_park_ends_stopped_untracked_and_refusing_motion_until_it_is_unparked() {
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 400_000);
    let driver = connected(&mount, 0).await;
    driver
        .start_tracking(TrackingMode::Sidereal)
        .await
        .expect("tracks");
    mount.clear_frames();

    driver.park().await.expect("parks");
    let frames = mount.frames();
    assert_eq!(
        &frames[frames.len() - 2..],
        &[":K1", ":K2"],
        "a park is a goto and then a stop — the protocol has no park opcode: {frames:?}"
    );
    let status = driver.status().await.expect("status");
    assert_eq!(status.state, MountState::Parked);
    assert!(status.parked);
    assert_eq!(
        status.tracking, None,
        "a parked mount reports no tracking mode at all"
    );
    assert!(status.is_consistent());

    // Motion is refused while parked, and parking a parked mount is not an error.
    assert!(matches!(
        driver
            .slew(
                Axis::Ra,
                astroctl_core::types::Direction::West,
                SlewSpeed::Slow
            )
            .await,
        Err(DeviceError::Rejected(_))
    ));
    driver.park().await.expect("idempotent");

    mount.clear_frames();
    driver.unpark().await.expect("unparks");
    assert!(
        mount.frames().is_empty(),
        "unpark is a host-side flag: the protocol has no park state, and re-issuing `F` would \
         write an action opcode whose effect on the counter nobody has measured — {:?}",
        mount.frames()
    );
    assert_eq!(
        driver.status().await.expect("status").state,
        MountState::Idle
    );
}

#[tokio::test(start_paused = true)]
async fn a_guide_pulse_displaces_the_axis_by_the_sidereal_rate_and_puts_tracking_back() {
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 400_000);
    let driver = connected(&mount, 0).await;
    driver
        .start_tracking(TrackingMode::Sidereal)
        .await
        .expect("tracks");
    mount.clear_frames();

    let rate = astroctl_core::types::GuideRate::new(0.5).expect("in range");
    driver
        .guide_pulse(Axis::Ra, astroctl_core::types::Direction::West, 1_000, rate)
        .await
        .expect("the pulse landed");

    let frames = mount.frames();
    assert_eq!(
        frames[0], ":P15",
        "`P` is programmed before the pulse (SDD §5.1)"
    );
    assert!(
        frames.iter().any(|frame| frame.starts_with(":H1")),
        "the displacement is realised as a bounded goto: {frames:?}"
    );
    // Half sidereal for one second is 0.5 × 104.7304 = 52.365 counts, which the mount can only
    // issue as 52. West is increasing hour angle, i.e. a forward counter in the north.
    assert_eq!(
        mount.counter(Axis::Ra) - (HOME + 200_000),
        52,
        "the displacement is the sidereal rate times the duration, to the count"
    );
    assert_eq!(
        &frames[frames.len() - 3..],
        &[":G110", ":I16C0200", ":J1"],
        "a pulse replaced tracking, so tracking is put back: {frames:?}"
    );
    assert_eq!(
        driver.status().await.expect("status").state,
        MountState::Tracking
    );
}

#[tokio::test(start_paused = true)]
async fn the_settle_interval_happens_after_tracking_is_restored_and_not_before() {
    // SDD §5.2.3 lists completion then tracking restore; the HAL lists settle then tracking. The
    // physics decides: settling is the tube ringing, and three seconds of a stopped mount is 45″
    // of sky walking out of the frame before the first exposure.
    let mount = SyntaMount::new();
    mount.park_counter(Axis::Ra, HOME + 200_000);
    mount.park_counter(Axis::Dec, HOME + 1_128_000);
    let driver = connected(&mount, 3).await;
    driver
        .start_tracking(TrackingMode::Sidereal)
        .await
        .expect("tracks");
    mount.clear_frames();

    let began = Instant::now();
    driver
        .goto(
            RaDec::from_parts(
                ra_hours_for_axis_offset(400_000.0),
                dec_degrees_for_axis_counts(HOME + 1_128_000),
            )
            .expect("valid"),
        )
        .await
        .expect("completes");
    let took = began.elapsed();

    let restarted = mount
        .written_at(":J1")
        .expect("tracking was restarted")
        .saturating_duration_since(began);
    assert!(
        took.saturating_sub(restarted) >= Duration::from_secs(3),
        "the settle interval ({took:?} total) must follow the tracking restore at {restarted:?}"
    );
}
