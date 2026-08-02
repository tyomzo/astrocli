//! `SkywatcherMount` — the [`MountDevice`] the rest of the system talks to (SDD §5.2.1, M3-T04).
//!
//! ```text
//! SkywatcherMount (impl MountDevice)          — coordinates, modes, goto logic       ← this file
//!     └── MotorController + MountGeometry     — per-axis counts, rates, the sky map   M3-T03
//!           └── SyntaCodec + SerialLink       — framing, encoding, two lanes          M3-T01/T02
//! ```
//!
//! Everything below this file is a value or a cable. This is where they become a telescope: the
//! handshake that turns four inquiries into two [`MotorController`]s and a [`MountGeometry`], the
//! goto that turns an `RaDec` into two verified programs, and the emergency stop that has to reach
//! the wire while all of that is in flight.
//!
//! # Three things that are not obvious from the layer diagram
//!
//! **Local sidereal time is injected.** [`SiderealClock`] is a one-method seam and this crate has
//! no implementation of it, deliberately: ADD §5.6 rule 1 lets `astroctl-drivers` depend on
//! `astroctl-hal` and `astroctl-core` and nothing else, and the workspace's one sidereal-time
//! implementation is `astroctl_safety::local_sidereal_degrees`. A second implementation here would
//! be a second answer to "where is the sky", which is the kind of duplication that shows up months
//! later as a pointing model that is wrong by a constant nobody can find. The field node owns the
//! wiring; the trait method has the same name and the same units as the function that satisfies it,
//! so the adapter is one line.
//!
//! **A goto is supervised by a task this driver owns, not by the caller's future.** The HAL is
//! explicit that dropping the `goto` future must not stop the slew — and SES-06 is equally explicit
//! that tracking is restored when the motion ends. Those two can only both be true if something
//! other than the caller's future is still running when the axes stop. It also fixes a defect that
//! is invisible until it happens: the ownership claim that makes a second goto `Busy` has to be
//! released by *something*, and a claim released only by the future that took it leaves a dropped
//! goto reporting `Busy` forever. See [`supervise`].
//!
//! **The emergency stop touches no state a normal command holds.** [`Shared::link`] and
//! [`Shared::state`] are separate locks for one reason: `emergency_stop` reads the first and never
//! the second until after the bytes are gone. A stalled goto holds nothing the stop needs, so the
//! stop's worst case is the priority lane's, which M3-T02 measured and bounded at one round trip.
//! [`tests::an_emergency_stop_reaches_the_wire_while_a_normal_exchange_is_wedged`] is the
//! assertion; the module docs of [`serial`](super::serial) are the mechanism.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use astroctl_core::config::{MountConfig, SiteConfig};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    Axis, AxisTravel, DeviceInfo, Direction, GuideRate, MountCapabilities, MountState, MountStatus,
    MountTravel, PierSide, RaDec, SlewSpeed, TrackingMode,
};
use astroctl_hal::mount::MountDevice;
use astroctl_hal::registry::{DetectedDevice, DriverInitError, MountFactory};
use async_trait::async_trait;
use tokio::sync::oneshot;

use super::codec::{
    AxisStatus, Command, Counts, FirmwareVersion, GotoProgram, MotionDirection, Move, SpeedClass,
};
use super::controller::exchange::{handshake, run_goto, Exchange as ExchangeSeam, SequenceError};
use super::controller::{AxisParams, ControllerError, MotorController, SlewMethod};
use super::math::{
    dec_axis_hour_angle, motor_direction, tracking_direction, AxisCounts, Branch, Lst,
    MountGeometry,
};
use super::port::{WireFactory, WriteGate};
use super::serial::{watchdog_channel, SerialLink, SerialTimings, WatchdogSink, WatchdogSource};

// -----------------------------------------------------------------------------------------
// Timings this layer owns
// -----------------------------------------------------------------------------------------

/// How often a goto asks whether it is over (SDD §5.2.3: "polls `j`/`f` at 2 Hz during goto").
const GOTO_POLL: Duration = Duration::from_millis(500);

/// Consecutive "stopped, and not where it was sent" polls before a goto is called a stall.
///
/// Two, not one, and the reason is the first poll rather than the mount. `J` is answered as soon
/// as the frame is parsed and the ramp starts after it; a poll that lands in that window would see
/// a stopped axis short of its target and declare a perfectly good goto a mechanical failure. The
/// spike measured *zero* counts of error across six gotos, so a genuine stall stays stalled and
/// costs one extra 500 ms poll to confirm — against a false stall, which aborts a slew the operator
/// asked for and leaves them looking for a fault that is not there.
const STALL_POLLS: u32 = 2;

/// How often a guide pulse asks whether its bounded goto has landed.
///
/// Not [`GOTO_POLL`]: a guiding loop issues corrections of 50–2000 ms and paces itself on this
/// call resolving, so a 2 Hz poll would make every sub-500 ms pulse cost 500 ms and halve the
/// correction rate the loop believes it has.
const GUIDE_POLL: Duration = Duration::from_millis(50);

/// The longest a guide pulse may take to land before the driver stops waiting for it.
///
/// A bounded goto of a few counts is over in well under a second; ten times the requested duration
/// plus a floor is generous against the ramp and short enough that a wedged axis does not stall a
/// guiding loop for the rest of the night.
fn guide_deadline(requested: Duration) -> Duration {
    (requested * 10).max(Duration::from_secs(2))
}

/// How often the chunk chain checks whether its bounded goto has run out (E16).
///
/// Half a second against a 30° chunk that cruises for ~8.6 s in the high class: the chain wakes
/// seventeen times per chunk, cheap on a link whose exchanges cost ~16 ms, and the pause an
/// operator feels between chunks is bounded by one tick plus the goto's own wind-up.
const CHUNK_POLL: Duration = Duration::from_millis(500);

/// PRD §4.2's stated maximum manual-slew class, in multiples of sidereal.
///
/// The *goto* cruise was measured at 835×, which is faster — but this field is what the UI offers
/// as a manual-speed ladder and `RateModel::slew` tops that ladder out at 800. Reporting the goto
/// cruise here would offer an operator a manual speed the driver never programs.
const MAX_SLEW_X_SIDEREAL: u32 = 800;

/// The Synta axis counters are 24-bit — home is `0x800000`, measured on both axes.
///
/// Reported before the handshake, when nothing better is known. Afterwards
/// [`Shared::capabilities`] narrows it to the bits the mount's own counts-per-revolution actually
/// spans, which is the "may sharpen after connect" the HAL allows.
const COUNTER_BITS: u8 = 24;

// -----------------------------------------------------------------------------------------
// The injected clock
// -----------------------------------------------------------------------------------------

/// Where the driver gets local sidereal time.
///
/// **One method, and this crate implements it exactly once — as a constant, for tests.** The real
/// implementation is `astroctl_safety::local_sidereal_degrees(site, Utc::now())`, which this crate
/// may not name (ADD §5.6 rule 1), so the field node supplies it:
///
/// ```ignore
/// #[derive(Debug)]
/// struct NodeClock(astroctl_safety::Site);
///
/// impl SiderealClock for NodeClock {
///     fn local_sidereal_degrees(&self) -> f64 {
///         astroctl_safety::local_sidereal_degrees(self.0, chrono::Utc::now())
///     }
/// }
/// ```
///
/// The method name and units are that function's, verbatim, so the adapter cannot get the
/// conversion wrong by having to do one. SDD §5.2.3 calls this the seam that keeps the Phase 2a
/// erfa upgrade internal; it is a trait rather than a `Fn` so it carries a `Debug` name into the
/// driver's own log lines.
pub trait SiderealClock: fmt::Debug + Send + Sync {
    /// Local sidereal time in degrees. Any finite value; the driver folds it.
    fn local_sidereal_degrees(&self) -> f64;
}

/// A clock that always reads the same sidereal time.
///
/// For tests, and for M3-T05's bring-up: a hardware session that wants to compare two readings of
/// the same counter needs the sky to hold still between them, and a fixed clock is the only way to
/// get that without stopping time. It is a constant rather than an algorithm, so it is not the
/// second sidereal-time implementation [`SiderealClock`] exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixedSiderealTime(
    /// Degrees.
    pub f64,
);

impl SiderealClock for FixedSiderealTime {
    fn local_sidereal_degrees(&self) -> f64 {
        self.0
    }
}

// -----------------------------------------------------------------------------------------
// The cable
// -----------------------------------------------------------------------------------------

/// Where the driver's serial task gets its port.
#[derive(Debug)]
enum Cable {
    /// `mount.port` / `mount.baud`, opened (or autodetected) at connect.
    Configured {
        /// `auto`, or an absolute device node.
        port: String,
        /// `mount.baud`.
        baud: u32,
    },
    /// A factory somebody handed over — the mock port in tests, and M3-T05's bring-up harness.
    Given(Arc<dyn WireFactory>),
}

// -----------------------------------------------------------------------------------------
// What the handshake produced
// -----------------------------------------------------------------------------------------

/// The mount as the handshake found it: two controllers and the geometry they share.
///
/// `Copy`, because every field is, and because a method that needs it wants a snapshot rather than
/// a borrow it would then be holding across an `.await`.
#[derive(Debug, Clone, Copy)]
struct Session {
    ra: MotorController,
    dec: MotorController,
    geometry: MountGeometry,
    firmware: FirmwareVersion,
}

impl Session {
    /// The controller for one axis.
    const fn controller(&self, axis: Axis) -> &MotorController {
        match axis {
            Axis::Ra => &self.ra,
            Axis::Dec => &self.dec,
        }
    }
}

/// What owns an axis right now.
///
/// Tracking is deliberately **not** here. On a Synta mount tracking is a low-speed `SLEW` — the
/// same `G`/`I`/`J` a manual slew uses — so the axis is "running" while tracking and a state
/// machine that treated running as ownership would answer `Busy` to every goto on a tracking
/// mount, which is every goto anybody makes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Activity {
    /// Idle, or tracking. Anybody may take it.
    Free,
    /// A goto owns both axes until its supervisor releases them.
    Goto {
        /// A park reports `Parking` rather than `Slewing`.
        parking: bool,
    },
    /// A manual slew owns the axis until something stops it. Never ends on its own — which is
    /// what the dead-man's switch above the HAL exists for (SDD §5.8.1).
    Manual {
        /// What the operator asked for, so a renewal with the same parameters costs no exchange.
        dir: Direction,
        /// Likewise.
        speed: SlewSpeed,
    },
    /// A guide pulse: owns the axis, but is not a slew (a guided exposure must not report the
    /// mount slewing at 1 Hz all night).
    Guiding,
}

impl Activity {
    /// Whether another command has to wait for this one.
    const fn owned(self) -> bool {
        !matches!(self, Self::Free)
    }

    /// Whether it makes the mount report itself slewing.
    const fn slewing(self) -> bool {
        matches!(self, Self::Goto { .. } | Self::Manual { .. })
    }
}

/// Everything behind the state lock.
#[derive(Debug)]
struct State {
    /// `None` until the handshake has run.
    session: Option<Session>,
    /// The mode that is running, or that a goto will restore. Not "off because the axes are busy",
    /// or the UI's tracking indicator would blink off for the duration of every slew.
    tracking: Option<TrackingMode>,
    parked: bool,
    ra: Activity,
    dec: Activity,
    /// The mechanical branch, cached from the last counter read.
    ///
    /// A declination slew's motor sense depends on it (`motor_direction`), and it changes only
    /// through a meridian flip — which this driver never performs by accident, because
    /// `goto_solution` refuses to change branch. So a cached value is correct until something
    /// physically moves the tube past the pole, and caching it is what keeps a 2 Hz manual-slew
    /// renewal from costing a counter read.
    branch: Option<Branch>,
    /// The counters as of the last read, for [`MountDevice::axis_travel`] (M3-T07).
    ///
    /// Cached rather than fetched on demand so that reporting travel costs no exchange: the
    /// safety watch reads a position at 2 Hz and asks for travel immediately afterwards, so the
    /// value is fresh by construction and a second pair of `:j` frames would learn nothing.
    ///
    /// **Updated by `read_counts`, unlike `branch`, and the asymmetry is deliberate.** A stalled
    /// axis keeps counting steps the metal never made (E11), which corrupts a *branch* into
    /// inverting the declination motor's sense — a wrong answer. It cannot corrupt travel in the
    /// dangerous direction: a phantom count over-reports how far the axis has gone, so a limit
    /// built on it refuses earlier than the metal requires. Erring toward "stop winding" is the
    /// error to have.
    counts: Option<AxisCounts>,
    /// The mechanical sense each axis's manual slew resolved to — see [`State::sense`] (M3-T08).
    ra_sense: Option<MotionDirection>,
    /// Likewise for the declination axis. This is the one that matters: it is the axis whose sky
    /// direction inverts at the pole, so it is the one where "what the operator asked for" and
    /// "which way the metal is turning" stop agreeing.
    dec_sense: Option<MotionDirection>,
    /// Bumped by everything that takes the axes away from someone. A goto that wakes to find this
    /// changed knows it was overridden — and, unlike a cancellation token, this survives the
    /// caller's future being dropped.
    generation: u64,
    /// What last took them, for the message the overridden caller gets.
    overridden_by: &'static str,
}

impl State {
    const fn activity(&mut self, axis: Axis) -> &mut Activity {
        match axis {
            Axis::Ra => &mut self.ra,
            Axis::Dec => &mut self.dec,
        }
    }

    /// The mechanical sense a manual slew resolved to, per axis (M3-T08).
    ///
    /// Read only while the axis is [`Activity::Manual`], which is why nothing clears it: a value
    /// left behind by a finished slew is never consulted. It is kept beside `Activity` rather than
    /// inside it because `Activity`'s equality is what makes a dead-man's-switch renewal a no-op
    /// (see `slew`), and widening it with a field the caller does not supply would break that.
    const fn sense(&mut self, axis: Axis) -> &mut Option<MotionDirection> {
        match axis {
            Axis::Ra => &mut self.ra_sense,
            Axis::Dec => &mut self.dec_sense,
        }
    }

    /// The session, or `NotConnected`.
    fn session(&self) -> Result<Session, DeviceError> {
        self.session.ok_or(DeviceError::NotConnected)
    }

    /// Take `wanted` for a command, refusing if anything else holds them.
    ///
    /// Returns the generation the caller must still find when it comes back. **Synchronous and
    /// before the first `.await`**, because `Busy` is a decision this driver makes from its own
    /// state: two gotos issued a millisecond apart must not both pass because the wire was slow.
    fn claim(
        &mut self,
        wanted: &[Axis],
        activity: Activity,
        by: &'static str,
    ) -> Result<u64, DeviceError> {
        self.session()?;
        if self.parked {
            return Err(DeviceError::Rejected(
                "the mount is parked; unpark before commanding motion".to_owned(),
            ));
        }
        for axis in wanted {
            if self.activity(*axis).owned() {
                return Err(DeviceError::Busy("the mount is already moving"));
            }
        }
        for axis in wanted {
            *self.activity(*axis) = activity;
        }
        self.generation += 1;
        self.overridden_by = by;
        Ok(self.generation)
    }

    /// Give the axes back, unless somebody else has since taken them.
    fn release(&mut self, generation: u64) {
        if self.generation != generation {
            return;
        }
        self.ra = Activity::Free;
        self.dec = Activity::Free;
    }

    /// Take everything, for a stop. Nothing may refuse this.
    fn seize(&mut self, by: &'static str) -> u64 {
        self.ra = Activity::Free;
        self.dec = Activity::Free;
        self.generation += 1;
        self.overridden_by = by;
        self.generation
    }
}

// -----------------------------------------------------------------------------------------
// The driver
// -----------------------------------------------------------------------------------------

/// Everything a spawned supervisor needs, which is everything.
#[derive(Debug)]
struct Shared {
    cable: Cable,
    timings: SerialTimings,
    site: SiteConfig,
    clock: Arc<dyn SiderealClock>,
    /// `mount.settle_time_seconds`.
    settle: Duration,
    /// The sink every link this driver spawns reports to. Created once, at construction, so a
    /// reconnect does not invalidate a receiver M1-T17 is already holding.
    watchdog: WatchdogSink,
    /// The receiving end, until somebody takes it.
    watchdog_source: Mutex<Option<WatchdogSource>>,
    /// The live link.
    ///
    /// **Its own lock, and that is the whole design of the emergency stop.** Nothing in a normal
    /// command holds this across an `.await` — it is cloned out and dropped — so the stop's path
    /// from trait call to priority-lane send waits on nothing a stalled exchange can hold.
    link: Mutex<Option<Arc<SerialLink>>>,
    /// Motion state. **Never touched by `emergency_stop` before the bytes are gone.**
    state: Mutex<State>,
    /// Set while a link is being opened, so two concurrent connects do not open two ports.
    connecting: tokio::sync::Mutex<()>,
    /// Only for the log line that says which mount this is.
    describe: String,
    /// Bumped whenever a guide pulse or a goto needs a unique name in a log.
    sequence: AtomicU64,
}

/// A Sky-Watcher (Synta) mount over a serial cable — SDD §5.2, PRD §4.2, MNT-01..08.
///
/// Built by [`SkywatcherMountFactory`] from `mount.driver: skywatcher`, or directly in a test with
/// [`SkywatcherMount::over_wire`].
#[derive(Debug)]
pub struct SkywatcherMount {
    shared: Arc<Shared>,
}

impl SkywatcherMount {
    /// Build from the operator's configuration.
    ///
    /// **No I/O.** SDD §8.1 requires registry construction to be free of side effects, so the port
    /// is not opened — and not even *chosen*, since `mount.port: auto` is resolved by a probe that
    /// belongs to `connect`.
    ///
    /// # Errors
    /// [`DriverInitError`] — none is produced today, but the signature is
    /// [`MountFactory::create`]'s and a driver that cannot be built from a valid configuration is
    /// a case this type is entitled to grow back.
    pub fn new(
        config: &MountConfig,
        site: SiteConfig,
        clock: Arc<dyn SiderealClock>,
    ) -> Result<Self, DriverInitError> {
        let describe = if config.port == "auto" {
            "autodetect".to_owned()
        } else {
            config.port.clone()
        };
        Self::assemble(
            config,
            site,
            clock,
            Cable::Configured {
                port: config.port.clone(),
                baud: config.baud,
            },
            describe,
        )
    }

    /// Build over a wire somebody else owns.
    ///
    /// The seam the mock port plugs into, and the one M3-T05's bring-up harness uses to drive real
    /// hardware through a wire it can also log. It is `pub` for the reason
    /// [`mock_port`](super::mock_port) is: this is the layer M3-T04 and M3-T05 are tested at, and a
    /// seam only the crate's own tests can reach is a seam the hardware task cannot use.
    ///
    /// # Errors
    /// As [`Self::new`].
    pub fn over_wire(
        config: &MountConfig,
        site: SiteConfig,
        clock: Arc<dyn SiderealClock>,
        wire: Arc<dyn WireFactory>,
    ) -> Result<Self, DriverInitError> {
        let describe = wire.describe();
        Self::assemble(config, site, clock, Cable::Given(wire), describe)
    }

    fn assemble(
        config: &MountConfig,
        site: SiteConfig,
        clock: Arc<dyn SiderealClock>,
        cable: Cable,
        describe: String,
    ) -> Result<Self, DriverInitError> {
        let (watchdog, source) = watchdog_channel();
        Ok(Self {
            shared: Arc::new(Shared {
                cable,
                timings: SerialTimings::from(&config.serial),
                site,
                clock,
                settle: Duration::from_secs(u64::from(config.settle_time_seconds)),
                watchdog,
                watchdog_source: Mutex::new(Some(source)),
                link: Mutex::new(None),
                state: Mutex::new(State {
                    session: None,
                    tracking: None,
                    parked: false,
                    ra: Activity::Free,
                    dec: Activity::Free,
                    branch: None,
                    counts: None,
                    ra_sense: None,
                    dec_sense: None,
                    generation: 0,
                    overridden_by: "nothing",
                }),
                connecting: tokio::sync::Mutex::new(()),
                describe,
                sequence: AtomicU64::new(0),
            }),
        })
    }

    /// The heartbeat notices this driver's serial link produces (SDD §5.2.4).
    ///
    /// **A named seam with no consumer in this task.** M1-T17's watchdog arm owns the other end;
    /// until it exists the notices are dropped by the channel, which is the correct behaviour for
    /// an unbuilt subscriber and is why this is a channel rather than a callback. Returns `None`
    /// on the second call — a receiver has exactly one owner, and handing out a second one would
    /// silently split the stream between two watchdogs.
    ///
    /// The channel outlives every individual link, so a reconnect does not invalidate a receiver
    /// somebody is already holding.
    #[must_use]
    pub fn take_watchdog(&self) -> Option<WatchdogSource> {
        self.shared
            .watchdog_source
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// The pier side of the last counter read, or `None` before the first.
    ///
    /// `MountDevice::position` returns an `RaDec` and SDD §4.3's `mount.position` carries a pier
    /// side too; the same coordinate is two different mount states, so the fact is not recoverable
    /// from the return value. Exposed here rather than smuggled into the trait, because widening
    /// `MountDevice` for one driver would make every other implementor answer a question it may
    /// not have.
    #[must_use]
    pub fn pier_side(&self) -> Option<PierSide> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.branch)
            .map(Branch::pier_side)
    }

    /// The saddle direction's hour angle, from the cached counters (SDD §5.4.3).
    ///
    /// From `counts` rather than the `branch` cache, like `motion_lookahead`: this is the bearing
    /// of a *position*, and the whole point of the number is the pose the mount is in right now.
    /// A stalled axis's phantom counts can skew it (E11), but only toward a bearing the metal has
    /// not reached — the collision limit then refuses a pose the rig is not yet in, which is the
    /// error to have.
    #[must_use]
    pub fn dec_axis_hour_angle_degrees(&self) -> Option<f64> {
        let state = self.shared.state.lock().ok()?;
        let session = state.session?;
        let counts = state.counts?;
        drop(state);
        Some(dec_axis_hour_angle(
            session.geometry.mech(counts),
            session.geometry.hemisphere(),
        ))
    }
}

impl Shared {
    fn locked(&self) -> MutexGuard<'_, State> {
        // Poisoning means a panic inside one of these short synchronous sections, which would be a
        // bug in this file rather than a condition to recover from.
        self.state.lock().expect("mount state is never poisoned")
    }

    /// The live link, or `NotConnected`.
    ///
    /// Cloned out of the guard so nothing is held across the caller's `.await` — the workspace
    /// denies `await_holding_lock`, and this is the accessor every command path goes through.
    fn link(&self) -> Result<Arc<SerialLink>, DeviceError> {
        let held = self
            .link
            .lock()
            .map_err(|_| DeviceError::Transport("the mount's link lock is poisoned".to_owned()))?
            .clone();
        held.ok_or(DeviceError::NotConnected)
    }

    /// The link and the session together — what almost every command needs.
    fn engaged(&self) -> Result<(Arc<SerialLink>, Session), DeviceError> {
        let session = self.locked().session()?;
        Ok((self.link()?, session))
    }

    /// Local sidereal time, folded and validated.
    ///
    /// # Errors
    /// [`DeviceError::Protocol`] if the injected clock produced a `NaN` or an infinity. That is a
    /// fault in the node's clock wiring rather than in the mount, but it has to be an error here:
    /// a `NaN` compares false against every limit, so letting one through would produce a
    /// coordinate that passes every safety test in the node.
    fn lst(&self) -> Result<Lst, DeviceError> {
        let degrees = self.clock.local_sidereal_degrees();
        Lst::from_degrees(degrees).map_err(|error| {
            DeviceError::Protocol(format!(
                "the injected sidereal clock read {degrees}: {error}"
            ))
        })
    }

    /// Read both axis counters, and cache the branch they imply.
    ///
    /// Two exchanges, `:j1` then `:j2`. They are not simultaneous and cannot be — there is one
    /// cable — so the pair is 16 ms of skew. At the sidereal rate that is 1.7 counts, a quarter of
    /// an arcsecond, and at the fastest slew ~1,400 counts; a goto reads them with the axes
    /// stopped, which is when it does not matter, and the position poll publishes them, which is
    /// when a quarter of an arcsecond does not matter either.
    async fn read_counts(
        &self,
        link: &SerialLink,
        session: Session,
    ) -> Result<AxisCounts, DeviceError> {
        let ra = link.send(session.ra.read_position()).await?;
        let dec = link.send(session.dec.read_position()).await?;
        let counts = AxisCounts { ra, dec };
        // **The branch is not updated here**, and the reason is a hardware truth: a Synta counter
        // counts *commanded* steps, so a stalled axis keeps reporting motion the metal never made
        // (E11, `spikes/skywatcher-heq5/FINDINGS.md`). This mount parks at the pole, where the
        // branch boundary lies, so a stall near home walks the phantom counter across it — and a
        // branch derived from fiction inverts the declination motor's sense on the operator's next
        // press. Observed exactly that way: a jam at 64×, then the same button driving the other
        // direction.
        //
        // Nothing a *nudge* does can legitimately change pier side either: it moves arcseconds,
        // and the side is a property of which half of the meridian the mount is working. Only the
        // deliberate acts change it — a goto that chose a side, or a fresh connect — so those are
        // the only places that write it (`refresh_branch`).
        //
        // The *counters* are cached here, though, because `axis_travel` needs them and a stall
        // cannot make that answer unsafe — see `State::counts`.
        self.locked().counts = Some(counts);
        Ok(counts)
    }

    /// Re-derive the pier branch from the counters, for the deliberate acts that can change it.
    ///
    /// Separate from [`Self::read_counts`] so that the 1 Hz position poll — which runs while an
    /// axis may be stalled and lying — cannot rewrite it. See that method for why.
    async fn refresh_branch(
        &self,
        link: &SerialLink,
        session: Session,
    ) -> Result<AxisCounts, DeviceError> {
        let counts = self.read_counts(link, session).await?;
        self.locked().branch = Some(session.geometry.mech(counts).branch());
        Ok(counts)
    }

    /// Read one axis's status word.
    async fn read_status(
        &self,
        link: &SerialLink,
        session: Session,
        axis: Axis,
    ) -> Result<AxisStatus, DeviceError> {
        link.send(session.controller(axis).read_status()).await
    }

    /// Which way a motor must turn for a sky direction, on the branch the mount is on.
    ///
    /// The declination sense reverses with the pier side, so this needs the branch — and reads it
    /// from the mount when the cache is cold, because guessing it is the classic guiding gotcha
    /// with the sign inverted.
    async fn sense(
        &self,
        link: &SerialLink,
        session: Session,
        direction: Direction,
    ) -> Result<MotionDirection, DeviceError> {
        let cached = self.locked().branch;
        // **Right ascension does not need it.** Both branches differ by a constant 180°, whose
        // derivative is zero, so an east/west motion's sense depends on the hemisphere alone
        // (M3-T03's `motor_direction`). That is worth a special case rather than a tidy uniform
        // read: a manual slew is renewed at 2 Hz by the dead-man's switch, and making the renewal
        // cost two counter reads would put 32 ms of wire traffic under the operator's thumb twice
        // a second for as long as they hold the D-pad.
        let branch = match (cached, axis_of(direction)) {
            (Some(branch), _) => branch,
            (None, Axis::Ra) => Branch::Normal,
            (None, Axis::Dec) => {
                self.refresh_branch(link, session).await?;
                self.locked().branch.unwrap_or(Branch::Normal)
            }
        };
        Ok(motor_direction(
            direction,
            branch,
            session.geometry.hemisphere(),
        ))
    }

    /// Stop both axes with the ramped stop, best effort, both always attempted.
    ///
    /// The failure path of every motion command. A failure on RA must not leave DEC slewing, which
    /// is what an early return would do.
    async fn halt(&self, link: &SerialLink, session: Session) -> Result<(), DeviceError> {
        let ra = link.send(session.ra.stop()).await;
        let dec = link.send(session.dec.stop()).await;
        ra.and(dec)
    }

    /// Start tracking on the right-ascension axis, at the mode's rate.
    ///
    /// Three frames — `G`, `I`, `J` — and the direction comes from the hemisphere, because below
    /// the equator the tracking motor runs backward and a driver that hardcoded forward would
    /// drive at twice sidereal the wrong way.
    async fn drive_tracking(
        &self,
        link: &SerialLink,
        session: Session,
        mode: TrackingMode,
    ) -> Result<(), DeviceError> {
        let sequence = session
            .ra
            .track(mode, tracking_direction(session.geometry.hemisphere()))
            .map_err(controller_error)?;
        link.send(sequence.motion_mode()).await?;
        link.send(sequence.step_period()).await?;
        link.send(sequence.start()).await?;
        Ok(())
    }

    /// Capabilities, sharpened by the handshake if it has run.
    fn capabilities(&self) -> MountCapabilities {
        let bits = self.locked().session.map_or(COUNTER_BITS, |session| {
            // What the mount's own counts-per-revolution actually spans, which is the honest
            // answer to "bits of resolution in the axis position counters": the register is 24
            // bits wide but a mount with a coarser gearbox does not have 24 bits of *resolution*.
            let cpr = session.geometry.ra_scale().counts_per_revolution();
            let spanned = u8::try_from(u32::BITS - cpr.leading_zeros()).unwrap_or(COUNTER_BITS);
            spanned.min(COUNTER_BITS)
        });
        MountCapabilities {
            // The mount has no PEC opcode and this driver has no PEC model. MNT-13 is a Phase 4
            // "could", and a capability reported true must work.
            has_pec: false,
            has_pulse_guide: true,
            tracking_rates: vec![
                TrackingMode::Sidereal,
                TrackingMode::Lunar,
                TrackingMode::Solar,
            ],
            max_slew_speed_x_sidereal: MAX_SLEW_X_SIDEREAL,
            position_resolution_bits: bits,
        }
    }
}

/// A controller or geometry refusal, as the HAL sees it.
///
/// `Protocol` rather than `Rejected`: the mount was never asked. These are arithmetic failures —
/// a rate no step period expresses, a move the increment register cannot hold — and `Rejected`
/// maps to `DEVICE_REJECTED`/422, which would tell an operator the mount refused a command it
/// never received.
fn controller_error(error: ControllerError) -> DeviceError {
    DeviceError::Protocol(error.to_string())
}

/// A sequence failure, as the HAL sees it.
///
/// The two shapes stay distinct all the way up: a link failure is the link's, and a readback
/// disagreement is `Protocol` because **nothing has moved** and the frame is what is wrong.
fn sequence_error(error: SequenceError<DeviceError>) -> DeviceError {
    match error {
        SequenceError::Link(error) => error,
        SequenceError::Readback(mismatch) => DeviceError::Protocol(mismatch.to_string()),
    }
}

/// The one-line forwarding impl SDD §5.2.1's layering asks for.
///
/// M3-T03 wrote its sequences against a four-line trait so it could be built, tested and reviewed
/// before the serial task existed. This is the whole glue between them: `SerialLink::send` is
/// already `async fn send<C: Command>(&self, cmd: C) -> Result<C::Response, DeviceError>`, which is
/// [`ExchangeSeam::send`] with the error type filled in.
impl ExchangeSeam for SerialLink {
    type Error = DeviceError;

    fn send<C>(
        &self,
        command: C,
    ) -> impl std::future::Future<Output = Result<C::Response, DeviceError>> + Send
    where
        C: Command + Send,
    {
        Self::send(self, command)
    }
}

// -----------------------------------------------------------------------------------------
// Goto
// -----------------------------------------------------------------------------------------

/// One axis's part of a goto: what was programmed, and where it should end.
#[derive(Debug, Clone, Copy)]
struct Leg {
    axis: Axis,
    target: Counts,
    /// `false` when the axis was already there — no program was sent and nothing has to arrive.
    programmed: bool,
}

/// What a supervised motion is aiming at.
///
/// The two arms are different *kinds* of destination, not two spellings of one, and separating
/// them is the whole of M3-T07.
#[derive(Debug, Clone, Copy)]
enum Destination {
    /// A sky coordinate, resolved through the mech↔sky map, the site and local sidereal time.
    Sky(RaDec),
    /// Both axis counters at the mechanical home, `0x800000`.
    ///
    /// **Deliberately not expressible as [`Self::Sky`], and that is the point.** Power-on sets
    /// both counters to home regardless of where the metal is, so the contract of parking is not
    /// "point at the pole" but "return to the pose power-on will assume". The pole is where the
    /// home pose happens to look, and a sky target there cannot constrain the right-ascension
    /// axis at all: at declination 90 every right ascension names the same point, so the goto is
    /// satisfied by the declination axis alone. That is how a park reported success on
    /// 2026-08-02 with the RA axis 215.6° from home and nothing commanded.
    ///
    /// This arm consults no coordinate map, no sidereal clock and no site. It is the one motion
    /// in the driver that is correct independently of M3-T06's hour-angle correction — and, per
    /// the collision entry in `spikes/skywatcher-heq5/FINDINGS.md`, the correct primitive to have
    /// whenever the map is under suspicion.
    Home,
}

/// One axis's move, before anything has been sent: where it starts, how far, where it lands.
///
/// The seam between "what does this destination mean" and "how is a goto programmed". Both
/// [`Destination`] arms produce this, and only one of them has ever heard of the sky.
#[derive(Debug, Clone, Copy)]
struct Step {
    axis: Axis,
    start: Counts,
    movement: Move,
    arrival: Counts,
}

/// The two moves a sky coordinate implies, through the coordinate map (SDD §5.2.3).
fn sky_steps(
    session: Session,
    from: AxisCounts,
    target: RaDec,
    lst: Lst,
) -> Result<[Step; 2], DeviceError> {
    let solution = session
        .geometry
        .goto(from, target, lst)
        .map_err(|error| DeviceError::Protocol(error.to_string()))?;
    Ok([
        Step {
            axis: Axis::Ra,
            start: from.ra,
            movement: solution.ra(),
            arrival: solution.destination().ra,
        },
        Step {
            axis: Axis::Dec,
            start: from.dec,
            movement: solution.dec(),
            arrival: solution.destination().dec,
        },
    ])
}

/// The two moves that put both counters back on `0x800000` — M3-T07's whole subject.
///
/// # Why the raw delta and not the short way round
///
/// [`AxisScale::shortest_delta`](super::math::AxisScale::shortest_delta) reduces modulo the counts
/// per revolution, so from 215.6° past home it would return −144.4° — the *same mechanical pose*,
/// reached by winding a further 144° in the direction that made the problem. That is right for a
/// goto, whose subject is where the tube points, and wrong here, whose subject is the counter
/// itself. Park unwinds; it does not take the scenic route to a congruent angle.
///
/// So the delta is `home − counts` exactly, and the arrival is `Counts::HOME` exactly. The
/// magnitude always fits the 24-bit increment register: both endpoints are 24-bit counters and
/// home is `0x800000`, so `|delta| ≤ 0x800000 < 0xFFFFFF`. The `map_err` is therefore unreachable
/// rather than defensive, and is written as a total conversion instead of an `expect` because
/// panicking is not a thing to do while parking a telescope.
fn home_steps(from: AxisCounts) -> Result<[Step; 2], DeviceError> {
    let home = i64::from(Counts::HOME.get());
    let step = |axis: Axis, start: Counts| -> Result<Step, DeviceError> {
        let movement = Move::from_delta(home - i64::from(start.get()))
            .map_err(|error| DeviceError::Protocol(error.to_string()))?;
        Ok(Step {
            axis,
            start,
            movement,
            arrival: Counts::HOME,
        })
    };
    Ok([step(Axis::Ra, from.ra)?, step(Axis::Dec, from.dec)?])
}

/// Program a goto on both axes and start them, or leave nothing moving.
///
/// The order is per axis and sequential because there is one cable, and each axis's eight frames
/// end in `J` before the next axis's `G` — which is deliberate. The alternative (both axes'
/// registers, then both `J`s) starts them closer together, and buys a synchronisation nobody needs
/// at the cost of a window in which RA is programmed, DEC is not, and a failure leaves half a goto
/// in the registers with no way to tell which half.
async fn program_goto(
    shared: &Shared,
    link: &SerialLink,
    session: Session,
    steps: [Step; 2],
    generation: u64,
) -> Result<[Leg; 2], DeviceError> {
    let mut done = steps.map(|step| Leg {
        axis: step.axis,
        target: step.arrival,
        programmed: false,
    });

    for (
        index,
        Step {
            axis,
            start,
            movement,
            arrival,
        },
    ) in steps.into_iter().enumerate()
    {
        // Checked before **every** axis, not once at the top. Programming a goto is eight frames
        // and ~128 ms, and an emergency stop that arrives in the middle of it must not be followed
        // by this loop calmly sending the next axis's `J`. This is the check that makes the stop
        // win a race it would otherwise only appear to win.
        if shared.locked().generation != generation {
            return Err(aborted(shared, false));
        }
        if movement.magnitude().get() == 0 {
            // A move of nothing has no inside for a break point, so the codec refuses it — and
            // rightly: an axis that is already there does not need commanding, and sending `J`
            // against a zero increment would be asking the mount what to do with it.
            continue;
        }
        let controller = session.controller(axis);
        // Per axis, not per goto. The class decides the ramp, and a declination move of 200 counts
        // sent in the high class because *right ascension* had far to go would spend its whole
        // journey inside a ramp built for a slew a hundred times longer.
        let class = controller.goto_speed_class(movement.magnitude().get());
        let program: GotoProgram = controller
            .goto(start, movement, class)
            .map_err(controller_error)?;
        debug_assert_eq!(program.destination(), arrival, "the codec and the solution");

        // The only route to a bounded `J` in this driver: four writes, three readbacks, the
        // comparison, and `J` only if it held (SDD §5.2.2). ~48 ms against ~128 ms of dead air.
        run_goto(link, &program).await.map_err(sequence_error)?;
        done[index].programmed = true;
    }

    Ok(done)
}

/// Wait for a programmed goto to finish, settle, and restore tracking (SDD §5.2.3, SES-06).
///
/// # Why this runs in a task the driver owns
///
/// The HAL requires a dropped `goto` future to leave the mount slewing, and SES-06 requires
/// tracking to be restored when the slew ends. Both can only hold if something other than the
/// caller's future is still running at the end of the motion — and there is a second, quieter
/// reason: the ownership claim that makes a concurrent goto `Busy` is released here, so a caller
/// that walks away does not leave the mount `Busy` for the rest of the night.
///
/// The caller's future waits on `report`. Dropping it drops the receiver, `send` fails, and this
/// keeps going — which is the whole point.
async fn supervise(
    shared: Arc<Shared>,
    destination: Destination,
    parking: bool,
    generation: u64,
    report: oneshot::Sender<Result<(), DeviceError>>,
) {
    let outcome = run_supervised(&shared, destination, parking, generation).await;

    // Release before reporting, so a caller that immediately issues its next command finds the
    // axes free. `release` is a no-op if something else has since taken them.
    shared.locked().release(generation);
    let _ = report.send(outcome);
}

async fn run_supervised(
    shared: &Arc<Shared>,
    destination: Destination,
    parking: bool,
    generation: u64,
) -> Result<(), DeviceError> {
    let (link, session) = shared.engaged()?;
    let restore = shared.locked().tracking.filter(|_| !parking);

    // The counters the program is built against must be read with the axes stopped and used
    // immediately: the readback expectations are absolute and computed from them, so a stale
    // value invalidates all three (M3-T03's `MotorController::goto`).
    let from = shared.read_counts(&link, session).await?;
    if shared.locked().generation != generation {
        // Two exchanges have gone by. A stop that landed during them has already reached the
        // motors; what it cannot do on its own is stop this function from programming a fresh
        // motion on top of it.
        return Err(aborted(shared, parking));
    }

    // The clock is read inside the `Sky` arm and nowhere else. A park that asked for local
    // sidereal time would fail on a node whose clock wiring is broken — and would be *right* to,
    // if it needed the answer. It does not: driving a counter to `0x800000` is the same motion at
    // every instant of the night, so making park depend on the clock would be inventing a way for
    // the one motion that stows the telescope to become unavailable.
    let steps = match destination {
        Destination::Sky(target) => sky_steps(session, from, target, shared.lst()?)?,
        Destination::Home => home_steps(from)?,
    };

    let legs = match program_goto(shared, &link, session, steps, generation).await {
        Ok(legs) => legs,
        Err(error) => {
            // Nothing may be moving, but something might: a failure between RA's `J` and DEC's
            // `G` leaves one axis under way. "A goto that fails mid-motion must leave the mount
            // stopped, not drifting toward a target nobody is tracking" (SDD §5.1).
            let _ = shared.halt(&link, session).await;
            return Err(error);
        }
    };

    match poll_to_completion(shared, &link, session, &legs, generation).await {
        Ok(()) => {}
        Err(error) => {
            let _ = shared.halt(&link, session).await;
            return Err(error);
        }
    }

    // **Tracking before the settle, not after.** SDD §5.2.3 lists completion then tracking restore
    // and the HAL lists settle then tracking; the physics decides the order. Settling is about the
    // tube ringing, and a mount that spends `settle_time_seconds` stopped is a mount whose target
    // has drifted 15″ per second out of the frame. Restoring first costs the same three frames and
    // holds the target while the tube settles, which is what the settle interval is *for*.
    if let Some(mode) = restore {
        shared.drive_tracking(&link, session, mode).await?;
    }
    if !shared.settle.is_zero() {
        tokio::time::sleep(shared.settle).await;
    }

    if shared.locked().generation != generation {
        return Err(aborted(shared, parking));
    }
    if parking {
        let mut state = shared.locked();
        state.parked = true;
        state.tracking = None;
    }
    Ok(())
}

/// The 2 Hz completion poll of SDD §5.2.3: stopped **and** within tolerance, on every axis that
/// was programmed.
async fn poll_to_completion(
    shared: &Arc<Shared>,
    link: &SerialLink,
    session: Session,
    legs: &[Leg; 2],
    generation: u64,
) -> Result<(), DeviceError> {
    let mut stalled = [0u32; 2];
    loop {
        tokio::time::sleep(GOTO_POLL).await;
        if shared.locked().generation != generation {
            return Err(aborted(shared, false));
        }

        let mut arrived = true;
        for (index, leg) in legs.iter().enumerate() {
            if !leg.programmed {
                continue;
            }
            let controller = session.controller(leg.axis);
            let status = shared.read_status(link, session, leg.axis).await?;
            let at = link.send(controller.read_position()).await?;
            if controller.arrived(status, at, leg.target) {
                continue;
            }
            arrived = false;
            if status.running {
                stalled[index] = 0;
                continue;
            }
            // Stopped, and not where it was sent. One such poll may be the gap between `J` and
            // the ramp; two is a mount that has stopped somewhere it was not asked to.
            stalled[index] += 1;
            if stalled[index] >= STALL_POLLS {
                return Err(DeviceError::Timeout(GOTO_POLL * STALL_POLLS));
            }
        }
        if arrived {
            // A goto is one of the two acts that can legitimately change pier side, and the
            // counters are fresh with the axes stopped — the one moment they are trustworthy. So
            // this is where the branch is re-derived, rather than from the running poll.
            let counts = shared.refresh_branch(link, session).await?;
            let _ = counts;
            return Ok(());
        }
    }
}

/// The error an overridden goto returns.
///
/// [`DeviceError::Aborted`], never `Rejected`: `Rejected` is `DEVICE_REJECTED`/422 and would tell
/// the operator their request was malformed at the exact moment their emergency stop worked.
fn aborted(shared: &Shared, parking: bool) -> DeviceError {
    let what = if parking { "park" } else { "goto" };
    let by = shared.locked().overridden_by;
    DeviceError::Aborted(format!("{what} aborted by {by}"))
}

// -----------------------------------------------------------------------------------------
// The trait
// -----------------------------------------------------------------------------------------

#[async_trait]
impl MountDevice for SkywatcherMount {
    async fn connect(&self) -> Result<(), DeviceError> {
        // One connect at a time. Two operators pressing Connect on a stale UI must not open two
        // ports and leave one of them orphaned; a tokio mutex because this *is* held across the
        // handshake's awaits, which is exactly what a std one may not be.
        let _opening = self.shared.connecting.lock().await;
        if self.shared.link().is_ok() && self.shared.locked().session.is_some() {
            // Idempotent and free (HAL rule 7): the operator's Connect on a UI showing stale state
            // must not cost a second handshake.
            return Ok(());
        }

        let factory = open_factory(&self.shared).await?;
        let (link, _task) = SerialLink::spawn(
            factory,
            // `Actions` — this is the connected driver, and the one thing that may move a mount.
            // Autodetect probed under `InquiryOnly`; nothing between the two can transmit an
            // uppercase byte, because nothing between the two transmits at all.
            WriteGate::Actions,
            self.shared.timings,
            self.shared.watchdog.clone(),
        );
        let link = Arc::new(link);

        let session = match handshake_both(&link, &self.shared.site).await {
            Ok(session) => session,
            Err(error) => {
                link.shutdown();
                return Err(error);
            }
        };

        // `F` only where the mount says it is needed. E1 measured `:F1`/`:F2` setting the
        // initialised bit and moving nothing — but what `F` does to the *counter* is unmeasured,
        // and re-initialising a mount that has been pointing all night is a risk with no protocol
        // benefit. A reconnect to a running session therefore costs two `:f` reads and no writes.
        for axis in [Axis::Ra, Axis::Dec] {
            let controller = session.controller(axis);
            let status = link.send(controller.read_status()).await?;
            if !status.initialised {
                link.send(controller.initialise()).await?;
            }
        }

        if let Ok(mut slot) = self.shared.link.lock() {
            *slot = Some(Arc::clone(&link));
        }
        {
            let mut state = self.shared.locked();
            state.session = Some(session);
            state.ra = Activity::Free;
            state.dec = Activity::Free;
        }
        tracing::info!(
            port = %self.shared.describe,
            firmware = %firmware_string(session.firmware),
            counts_per_revolution = session.geometry.ra_scale().counts_per_revolution(),
            "Sky-Watcher mount connected"
        );
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        // **Does not stop the mount** (HAL, SDD §7): a service restart mid-session must leave a
        // tracking mount tracking, because the mount is safe while tracking and a stopped one has
        // lost the target. So this releases the port and nothing else.
        let link = self
            .shared
            .link
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(link) = link {
            link.shutdown();
        }
        let mut state = self.shared.locked();
        state.session = None;
        state.ra = Activity::Free;
        state.dec = Activity::Free;
        Ok(())
    }

    async fn position(&self) -> Result<RaDec, DeviceError> {
        let (link, session) = self.shared.engaged()?;
        let counts = self.shared.read_counts(&link, session).await?;
        // Read after the counters, not before: the sky moved while the two `:j` frames were on the
        // wire, and taking the later time is the one that matches the later counter.
        let lst = self.shared.lst()?;
        Ok(session.geometry.position(counts, lst).coords)
    }

    async fn status(&self) -> Result<MountStatus, DeviceError> {
        let Ok((link, session)) = self.shared.engaged() else {
            return Ok(MountStatus::disconnected());
        };

        // A mount that has stopped answering reports `Fault` rather than making this call fail —
        // "the mount is not answering" is a state the operator must see, not a blank status panel.
        //
        // The declination read is skipped when the first one failed, and that is a cost decision:
        // against a silent mount each request is a 500 ms timeout plus its retry, so asking both
        // axes would make a 1 Hz status poll spend two of every second waiting for a cable that is
        // not there. One request is enough to learn the answer and is also enough to *notice the
        // recovery* — which is why this attempts the exchange at all rather than short-circuiting
        // on the miss counter. A status call that could only ever confirm the fault would leave
        // the panel red until something else happened to poll.
        let ra = self.shared.read_status(&link, session, Axis::Ra).await;
        let running = match ra {
            Ok(ra) => self
                .shared
                .read_status(&link, session, Axis::Dec)
                .await
                .ok()
                .map(|dec| (ra.running, dec.running)),
            Err(_) => None,
        };

        let mut state = self.shared.locked();
        if let Some((ra, dec)) = running {
            // The one thing the driver's own record cannot know: a manual slew has no completion
            // signal, so an axis stopped by a hard limit, a stall or somebody's hand controller
            // would otherwise be reported as slewing until the operator let go of the D-pad.
            for (axis, moving) in [(Axis::Ra, ra), (Axis::Dec, dec)] {
                if !moving && matches!(state.activity(axis), Activity::Manual { .. }) {
                    *state.activity(axis) = Activity::Free;
                }
            }
        }

        // One failed read is not a fault: SDD §5.2.4 puts the threshold at three consecutive
        // misses precisely so a single dropped frame does not turn the operator's status panel
        // red. Below the threshold the last known state stands, which is the honest answer —
        // nothing has changed except that one question went unanswered.
        let faulted =
            running.is_none() && link.consecutive_misses() >= self.shared.timings.heartbeat_misses;
        let slewing = state.ra.slewing() || state.dec.slewing();
        let parking = matches!(state.ra, Activity::Goto { parking: true })
            || matches!(state.dec, Activity::Goto { parking: true });
        let status = if faulted {
            MountStatus {
                state: MountState::Fault,
                tracking: state.tracking,
                slewing,
                parked: state.parked,
            }
        } else if slewing {
            MountStatus {
                state: if parking {
                    MountState::Parking
                } else {
                    MountState::Slewing
                },
                tracking: state.tracking,
                slewing: true,
                parked: false,
            }
        } else if state.parked {
            MountStatus {
                state: MountState::Parked,
                tracking: None,
                slewing: false,
                parked: true,
            }
        } else if let Some(mode) = state.tracking {
            MountStatus {
                state: MountState::Tracking,
                tracking: Some(mode),
                slewing: false,
                parked: false,
            }
        } else {
            MountStatus {
                state: MountState::Idle,
                tracking: None,
                slewing: false,
                parked: false,
            }
        };
        debug_assert!(
            status.is_consistent(),
            "the mount driver published a self-contradictory status: {status:?}"
        );
        Ok(status)
    }

    async fn goto(&self, target: RaDec) -> Result<(), DeviceError> {
        run_goto_command(&self.shared, Destination::Sky(target), false).await
    }

    async fn sync(&self, pos: RaDec) -> Result<(), DeviceError> {
        // `:E` sets an axis counter, and this driver does not send it. The reason is not that the
        // opcode is missing — M3-T01 declined to model it — but that the spike never exercised it:
        // `spikes/skywatcher-heq5/ENCODINGS.md` records `E` as documented and unverified, and a
        // half-applied sync is a pointing model that is wrong in a way nothing downstream can
        // detect (SDD §5.1). M3-T05 is the task that can measure it; until then this is honest
        // rather than plausible.
        let _ = pos;
        self.shared.locked().session()?;
        Err(DeviceError::Unsupported)
    }

    async fn start_tracking(&self, mode: TrackingMode) -> Result<(), DeviceError> {
        let (link, session) = self.shared.engaged()?;
        {
            let state = self.shared.locked();
            if state.parked {
                return Err(DeviceError::Rejected(
                    "the mount is parked; unpark before tracking".to_owned(),
                ));
            }
            if state.ra.owned() {
                // Except while a goto is running: SES-06 says the goto restores tracking when it
                // lands, and an operator switching to lunar mid-slew means the goto should end in
                // lunar. So the mode is recorded and the wire is left alone.
                if matches!(state.ra, Activity::Goto { .. }) {
                    drop(state);
                    self.shared.locked().tracking = Some(mode);
                    return Ok(());
                }
                return Err(DeviceError::Busy("the right-ascension axis is slewing"));
            }
        }
        self.shared.drive_tracking(&link, session, mode).await?;
        self.shared.locked().tracking = Some(mode);
        Ok(())
    }

    async fn stop_tracking(&self) -> Result<(), DeviceError> {
        let (link, session) = self.shared.engaged()?;
        // Always worth executing: this is a stopping command, and a late stop is safe
        // (SDD §5.8.1). `K`, the ramped stop — tracking is a low-speed slew and `L` is for
        // emergencies only.
        let sent = link.send(session.ra.stop()).await;
        self.shared.locked().tracking = None;
        sent
    }

    async fn slew(&self, axis: Axis, dir: Direction, speed: SlewSpeed) -> Result<(), DeviceError> {
        if axis_of(dir) != axis {
            // The mount is the last place that can catch this. One layer up it becomes an axis
            // moving the wrong way, which looks like a wiring fault.
            return Err(DeviceError::Rejected(format!(
                "{dir:?} is not a direction on the {axis:?} axis"
            )));
        }
        let (link, session) = self.shared.engaged()?;

        {
            let mut state = self.shared.locked();
            if *state.activity(axis) == (Activity::Manual { dir, speed }) {
                // A no-op at the device, as the HAL requires: the dead-man's switch renews this at
                // 2 Hz and re-issuing `G`/`I`/`J` on every renewal would restart the ramp and make
                // the mount stutter under the operator's thumb. It costs no exchange either, which
                // is the point — the renewal has to be cheap.
                return Ok(());
            }
            if matches!(*state.activity(axis), Activity::Manual { .. }) {
                // A different direction or speed on an axis the operator is already driving is a
                // speed change, not a conflict: they moved the slider without letting go.
                *state.activity(axis) = Activity::Free;
            }
        }

        let generation = self.shared.locked().claim(
            &[axis],
            Activity::Manual { dir, speed },
            "a manual slew",
        )?;

        let sense = self.shared.sense(&link, session, dir).await?;
        // Recorded before the frames go out, so that `motion_lookahead` can never see an axis
        // running with no sense on record (M3-T08). A slew that fails to start leaves a value
        // behind, which is harmless: it is only read while the axis is `Manual`, and the failure
        // path below frees it.
        *self.shared.locked().sense(axis) = Some(sense);
        let method = session
            .controller(axis)
            .slew_method(speed)
            .map_err(controller_error)?;
        let sent = match method {
            SlewMethod::Unbounded(rate) => {
                let sequence = session
                    .controller(axis)
                    .motion_at(rate, sense)
                    .map_err(controller_error)?;
                async {
                    link.send(sequence.motion_mode()).await?;
                    link.send(sequence.step_period()).await?;
                    link.send(sequence.start()).await
                }
                .await
            }
            // E16: above ~32× sidereal this mount cannot start an unbounded slew and refuses to
            // be re-rated while running one, so a fast rung is chained bounded gotos — the one
            // motion whose acceleration the firmware supplies. The chunk starts here; the chain
            // lives in the spawned watcher below.
            SlewMethod::Chunked(class) => {
                let controller = session.controller(axis);
                match link.send(controller.read_position()).await {
                    Ok(from) => match controller.slew_chunk(from, sense, class) {
                        Ok(program) => run_goto(&*link, &program).await.map_err(sequence_error),
                        Err(error) => Err(controller_error(error)),
                    },
                    Err(error) => Err(error),
                }
            }
        };

        if sent.is_err() {
            // A failed slew must not leave the axis owned, or a timeout would make the mount
            // permanently `Busy` to the operator trying again.
            self.shared.locked().release(generation);
            return sent;
        }

        if let SlewMethod::Chunked(class) = method {
            // The chain: while the operator holds the button, a finished chunk begets the next.
            // Spawned so a stop is never queued behind it — the stop path seizes the axes, the
            // generation moves, and this task sees it and dies. The one race worth naming: a `K`
            // landing between this task's liveness check and its `J` restarts a stopped axis, so
            // every restart is followed by a re-check that re-stops — `K` is idempotent, and the
            // dead-man's switch above this driver bounds even the window nobody thought of.
            let shared = Arc::clone(&self.shared);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(CHUNK_POLL).await;
                    {
                        let mut state = shared.locked();
                        if state.generation != generation
                            || *state.activity(axis) != (Activity::Manual { dir, speed })
                        {
                            return;
                        }
                    }
                    let Ok(status) = shared.read_status(&link, session, axis).await else {
                        return;
                    };
                    if status.running {
                        continue;
                    }
                    // The chunk ran out under a held button: issue the next one.
                    let controller = session.controller(axis);
                    let Ok(from) = link.send(controller.read_position()).await else {
                        return;
                    };
                    let Ok(program) = controller.slew_chunk(from, sense, class) else {
                        return;
                    };
                    if run_goto(&*link, &program).await.is_err() {
                        return;
                    }
                    let stale = {
                        let mut state = shared.locked();
                        state.generation != generation
                            || *state.activity(axis) != (Activity::Manual { dir, speed })
                    };
                    if stale {
                        let _ = link.send(controller.stop()).await;
                        return;
                    }
                }
            });
        }
        Ok(())
    }

    async fn stop_slew(&self, axis: Axis) -> Result<(), DeviceError> {
        let (link, session) = self.shared.engaged()?;
        // Bumped whether or not anything was moving: a goto being stopped has to learn about it,
        // and stopping an axis that is not moving is explicitly legal, so there is nothing to
        // condition on that would not also be a race.
        self.shared.locked().seize("a stop");
        // `K`, not `L`. At 800× sidereal the difference is a degree of extra travel, which is why
        // `emergency_stop` is a separate call rather than this one with a flag.
        link.send(session.controller(axis).stop()).await
    }

    async fn guide_pulse(
        &self,
        axis: Axis,
        dir: Direction,
        duration_ms: u32,
        rate: GuideRate,
    ) -> Result<(), DeviceError> {
        if axis_of(dir) != axis {
            return Err(DeviceError::Rejected(format!(
                "{dir:?} is not a direction on the {axis:?} axis"
            )));
        }
        let (link, session) = self.shared.engaged()?;
        let duration = Duration::from_millis(u64::from(duration_ms));
        let generation = self
            .shared
            .locked()
            .claim(&[axis], Activity::Guiding, "a guide pulse")?;
        let guard = Claim {
            shared: &self.shared,
            generation,
        };

        let sense = self.shared.sense(&link, session, dir).await?;
        let controller = session.controller(axis);
        let pulse = controller
            .guide_pulse(rate, sense, duration)
            .map_err(controller_error)?;

        // `P` first, per SDD §5.1 — the driver programs the rate before issuing the pulse. What
        // its levels mean is unverified (E15), which is why the *displacement* below does not
        // depend on it.
        link.send(pulse.guide_rate()).await?;

        if !pulse.is_measurable() {
            // Below one count — 0.1436″ on the operator's mount — is not a small motion, it is no
            // motion. Sending it would be asking the mount for a goto with no break point inside
            // it, which it refuses. A guiding loop still gets the pacing it asked for.
            tokio::time::sleep(duration).await;
            drop(guard);
            return Ok(());
        }

        let from = link.send(controller.read_position()).await?;
        // Low speed, always: a guide correction is tens of counts and the high class advances the
        // axis in 16-count jumps, which at this size *is* the error budget.
        let program = controller
            .goto(from, pulse.offset(), SpeedClass::Low)
            .map_err(controller_error)?;
        let target = program.destination();
        run_goto(&*link, &program).await.map_err(sequence_error)?;

        // A bounded goto ramps and stops, so the displacement is exact and the duration is
        // whatever the ramp takes (M3-T03's `GuidePulse`). Waiting for the axis rather than for
        // the clock is what makes the *displacement* the contract a guiding loop can rely on.
        let deadline = tokio::time::Instant::now() + guide_deadline(duration);
        loop {
            tokio::time::sleep(GUIDE_POLL).await;
            let status = self.shared.read_status(&link, session, axis).await?;
            let at = link.send(controller.read_position()).await?;
            if controller.arrived(status, at, target) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(DeviceError::Timeout(guide_deadline(duration)));
            }
        }

        // On the right-ascension axis the pulse *replaced* tracking — a bounded goto is a motion
        // mode, and the mount has exactly one per axis. Restoring it is not optional: a guiding
        // loop that stopped the drive with its first correction would watch the star walk out of
        // the frame at the sidereal rate.
        let restore = self.shared.locked().tracking;
        if axis == Axis::Ra {
            if let Some(mode) = restore {
                self.shared.drive_tracking(&link, session, mode).await?;
            }
        }
        drop(guard);
        Ok(())
    }

    /// Drives **both axis counters** to `0x800000`, the pose power-on assumes (M3-T07, MNT-07).
    ///
    /// Not a goto to a sky coordinate. See [`Destination::Home`] for why no sky target can state
    /// this requirement, and [`home_steps`] for why the move is the raw counter delta rather than
    /// the short way round.
    async fn park(&self) -> Result<(), DeviceError> {
        if self.shared.locked().parked {
            return Ok(()); // parking a parked mount is not an error
        }
        run_goto_command(&self.shared, Destination::Home, true).await?;
        // The Synta protocol has no park opcode, so a park is a goto and then a stop: the axes are
        // already stopped by the bounded goto, and `K` is what makes sure nothing restarted them.
        let (link, session) = self.shared.engaged()?;
        self.shared.halt(&link, session).await
    }

    async fn unpark(&self) -> Result<(), DeviceError> {
        self.shared.locked().session()?;
        // **No wire traffic, deliberately.** The protocol has no park state to release; park is a
        // host-side flag over a goto. The obvious alternative — re-issuing `F` — would be sending
        // an action opcode whose effect on the axis *counter* nobody has measured, to a mount whose
        // counter is the night's pointing model. There is nothing to gain and a whole session to
        // lose. M3-T05 can measure `F` and this can become one frame if it turns out to be free.
        self.shared.locked().parked = false;
        Ok(())
    }

    async fn emergency_stop(&self) -> Result<(), DeviceError> {
        // ---------------------------------------------------------------------------------
        // Everything above this line is the whole safety path, and it must stay this short.
        //
        // `Shared::link` is a lock no normal command holds across an `.await` — every one of them
        // clones the handle out and drops the guard — so the worst this can wait for is a clone.
        // The motion state, which a stalled goto *does* touch, is not read until after the bytes
        // are gone. That ordering is the reason `link` and `state` are two locks and not one.
        // ---------------------------------------------------------------------------------
        let link = self.shared.link()?;
        // `L` on both axes, both on the priority lane, both always attempted: a failure on RA must
        // not leave DEC slewing. SDD §5.2.4 budgets this at ≤ 20 ms to bytes-on-wire and M3-T02's
        // preemption rule bounds it at one round trip even against a mount that has gone silent.
        let stopped = link.emergency_stop().await;

        // Now, and only now, the bookkeeping. Tracking stops too, because on a Synta mount
        // tracking *is* a slew: `L` on axis 1 stops the sidereal drive along with everything else,
        // and a driver that kept reporting `Tracking` would be describing a mount that does not
        // exist.
        {
            let mut state = self.shared.locked();
            state.seize("an emergency stop");
            state.tracking = None;
        }
        stopped
    }

    fn capabilities(&self) -> MountCapabilities {
        self.shared.capabilities()
    }

    /// Travel from `0x800000` on both axes, as of the last counter read (M3-T07).
    ///
    /// `None` before the handshake or before the first read — there is no home reference until
    /// the counts per revolution are known, and inventing one would hand the safety wrapper a
    /// number it would then enforce a limit against.
    /// The pier side of the last counter read (M3-T08 promoted this from an inherent method).
    ///
    /// It was inherent because widening `MountDevice` for one driver would have made every other
    /// implementor answer a question it may not have; `Option` is the answer to that objection,
    /// the same one `axis_travel` takes. Until it moved here, `mount.position.pier_side` reported
    /// `unknown` on real hardware while the driver knew the answer — SDD §5.4's obligation 3,
    /// outstanding since M1-T05.
    fn pier_side(&self) -> Option<PierSide> {
        Self::pier_side(self)
    }

    fn dec_axis_hour_angle_degrees(&self) -> Option<f64> {
        Self::dec_axis_hour_angle_degrees(self)
    }

    /// Advance the mechanical state one step and project — SDD §5.4.1, §5.4.2 (M3-T08).
    ///
    /// The whole of this method is deciding *which mechanical sense* the step runs in, because
    /// the projection itself already handles the branch
    /// ([`MountGeometry::after_motion`](super::math::MountGeometry::after_motion)).
    ///
    /// A **moving** axis uses the sense it is actually running. Re-resolving `dir` against the
    /// branch the mount is on now would invert the answer the moment a held slew crossed the pole,
    /// and would invert it in the worst possible direction: the tube is descending, and the fresh
    /// resolution says the motion that is now "north" climbs. An **idle** axis has no running
    /// sense, so `dir` is resolved against the branch of the current declination angle, which is
    /// what starting the motion would do.
    fn motion_lookahead(&self, axis: Axis, dir: Direction, degrees: f64) -> Option<RaDec> {
        if axis_of(dir) != axis {
            // Not a direction on this axis. The driver refuses this in `slew` with a message about
            // the axis; here there is simply no motion to describe.
            return None;
        }
        let mut state = self.shared.locked();
        let session = state.session?;
        let counts = state.counts?;
        let running = matches!(*state.activity(axis), Activity::Manual { .. });
        let recorded = *state.sense(axis);
        drop(state);

        let hemisphere = session.geometry.hemisphere();
        let sense = match (running, recorded) {
            (true, Some(sense)) => sense,
            _ => motor_direction(
                dir,
                // From the *counters*, not the `branch` cache: the cache is deliberately not
                // refreshed on every counter read (see `State::branch`), and this is the one
                // caller that needs the branch of a position rather than of a session.
                Branch::of(session.geometry.mech(counts).dec_axis),
                hemisphere,
            ),
        };

        let lst = self.shared.lst().ok()?;
        Some(
            session
                .geometry
                .after_motion(counts, axis, sense, degrees, lst)
                .coords,
        )
    }

    fn axis_travel(&self) -> Option<MountTravel> {
        let state = self.shared.locked();
        let session = state.session?;
        let counts = state.counts?;
        // Cold cache is `Normal`, matching `sense`: the branch only decides which sky direction
        // *names* the homeward motion on the declination axis. Getting it wrong swaps two labels
        // and never changes how far the axis has travelled, which is the number the limit
        // compares. It also cannot be wrong for long — one `sense` call on a declination slew
        // fills the cache from the mount.
        let branch = state.branch.unwrap_or(Branch::Normal);
        drop(state);

        let hemisphere = session.geometry.hemisphere();
        let of = |axis: Axis, scale: super::math::AxisScale, at: Counts| {
            let travel = scale.travel_from_home(at);
            AxisTravel {
                degrees: travel.abs(),
                homeward: homeward(axis, travel, branch, hemisphere),
            }
        };
        Some(MountTravel {
            ra: of(Axis::Ra, session.geometry.ra_scale(), counts.ra),
            dec: of(Axis::Dec, session.geometry.dec_scale(), counts.dec),
        })
    }

    fn device_info(&self) -> DeviceInfo {
        let session = self.shared.locked().session;
        DeviceInfo {
            name: "Sky-Watcher Mount".to_owned(),
            model: session.map_or_else(
                || "Synta (unidentified until connect)".to_owned(),
                |session| {
                    session.firmware.model().map_or_else(
                        || format!("Synta model {:#04x}", session.firmware.model_code),
                        |model| model.as_str().to_owned(),
                    )
                },
            ),
            // Learned at connect, so `None` until then — which is why the field is an `Option`.
            firmware: session.map(|session| firmware_string(session.firmware)),
            // The protocol carries no serial number; `:e` is firmware and model, and nothing else
            // identifies one HEQ5 from another.
            serial: None,
            protocol: "synta-serial".to_owned(),
        }
    }
}

/// Releases an axis claim however the future ends, cancellation included.
///
/// Only for the short commands. A goto's claim is released by its supervisor instead, because the
/// HAL requires a dropped goto future to leave the mount slewing and a claim that vanished with
/// the future would let a second goto retarget a slew that is still under way.
struct Claim<'a> {
    shared: &'a Arc<Shared>,
    generation: u64,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.shared.locked().release(self.generation);
    }
}

/// The firmware string an operator sees.
///
/// `2.04.01` rather than `020401`: the three bytes are a major, a minor and a model code, and
/// printing them as one hex blob would ask every reader to split it again. The raw value stays in
/// the `FirmwareVersion` for a log that wants the measurement.
fn firmware_string(firmware: FirmwareVersion) -> String {
    format!(
        "{}.{:02}.{:02}",
        firmware.major, firmware.minor, firmware.model_code
    )
}

/// The sky direction that drives an axis back toward home (M3-T07).
///
/// `travel` is signed in counter terms — positive means an increasing counter — so the motor has
/// to run the other way, and which *sky* direction that is depends on the hemisphere and, for the
/// declination axis, on the mechanical branch. This is the one place that mapping is inverted, and
/// it is inverted by asking [`motor_direction`] rather than by restating its table: a second copy
/// of the sign rules is a second chance to get a limit's escape hatch backwards, which would refuse
/// the direction that unwinds and permit the one that winds.
fn homeward(
    axis: Axis,
    travel: f64,
    branch: Branch,
    hemisphere: super::math::Hemisphere,
) -> Direction {
    let wanted = if travel > 0.0 {
        MotionDirection::Backward
    } else {
        MotionDirection::Forward
    };
    let (first, second) = match axis {
        Axis::Ra => (Direction::East, Direction::West),
        Axis::Dec => (Direction::North, Direction::South),
    };
    if motor_direction(first, branch, hemisphere) == wanted {
        first
    } else {
        second
    }
}

/// Which axis a sky direction belongs to.
const fn axis_of(direction: Direction) -> Axis {
    match direction {
        Direction::East | Direction::West => Axis::Ra,
        Direction::North | Direction::South => Axis::Dec,
    }
}

/// The body of both `goto` and `park`: claim synchronously, supervise asynchronously, wait.
async fn run_goto_command(
    shared: &Arc<Shared>,
    destination: Destination,
    parking: bool,
) -> Result<(), DeviceError> {
    shared.link()?;
    let what = if parking { "a park" } else { "a goto" };
    let generation = {
        let mut state = shared.locked();
        if parking {
            // A park stops tracking before it moves and stays stopped: `MountStatus` requires a
            // parked mount to report no tracking mode at all.
            state.tracking = None;
        }
        state.claim(&[Axis::Ra, Axis::Dec], Activity::Goto { parking }, what)?
    };
    let _ = shared.sequence.fetch_add(1, Ordering::Relaxed);

    let (report, wait) = oneshot::channel();
    tokio::spawn(supervise(
        Arc::clone(shared),
        destination,
        parking,
        generation,
        report,
    ));
    wait.await.unwrap_or_else(|_| {
        Err(DeviceError::Transport(
            "the goto supervisor stopped without reporting".to_owned(),
        ))
    })
}

/// Both axes' handshakes, and the geometry they imply.
async fn handshake_both(link: &SerialLink, site: &SiteConfig) -> Result<Session, DeviceError> {
    let ra_params: AxisParams = handshake(link, Axis::Ra).await.map_err(sequence_error)?;
    let dec_params: AxisParams = handshake(link, Axis::Dec).await.map_err(sequence_error)?;
    let ra = MotorController::new(ra_params).map_err(controller_error)?;
    let dec = MotorController::new(dec_params).map_err(controller_error)?;
    let geometry = MountGeometry::from_handshake(
        ra_params.counts_per_revolution,
        dec_params.counts_per_revolution,
        site,
    )
    .map_err(|error| DeviceError::Protocol(error.to_string()))?;
    Ok(Session {
        ra,
        dec,
        geometry,
        // The two axes answer `:e` separately and an HEQ5 answers identically; the right-ascension
        // controller's is the one reported, because a `DeviceInfo` has one firmware field and
        // picking the first axis is at least a rule.
        firmware: ra_params.firmware,
    })
}

/// The wire factory this driver's link should use, resolving `mount.port: auto` if it has to.
async fn open_factory(shared: &Shared) -> Result<Arc<dyn WireFactory>, DeviceError> {
    match &shared.cable {
        Cable::Given(factory) => Ok(Arc::clone(factory)),
        #[cfg(feature = "serialport")]
        Cable::Configured { port, baud } => {
            let path = if port == "auto" {
                // Autodetect probes under `WriteGate::InquiryOnly` — `survey.py`'s byte-level rule
                // applied to the one situation where it matters most, a port whose identity is
                // exactly what is not yet known.
                super::port::autodetect(*baud, shared.timings.request_timeout).await?
            } else {
                std::path::PathBuf::from(port)
            };
            Ok(Arc::new(super::port::SerialPortFactory::new(path, *baud)))
        }
        #[cfg(not(feature = "serialport"))]
        Cable::Configured { port, baud } => {
            let _ = baud;
            // The same split `libgphoto2` has, for the same reason: the fd layer needs
            // `libudev-dev` at build time and CI has none, so it is off by default and the field
            // node turns it on. Construction still succeeds — capabilities and identity are
            // readable while disconnected — and the failure names the missing feature rather than
            // the missing mount.
            Err(DeviceError::Transport(format!(
                "this build of astroctl-drivers has no serial port implementation, so `{port}` \
                 cannot be opened; rebuild with the `serialport` feature (astroctl-field: \
                 `--features serialport`)"
            )))
        }
    }
}

// -----------------------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------------------

/// Builds [`SkywatcherMount`]s for the driver registry under the name `"skywatcher"` (HAL-07).
///
/// # Why the site and the clock live on the factory
///
/// [`MountFactory::create`] is handed a [`MountConfig`] and nothing else, and this driver needs two
/// things that are not in it: the observing site (whose latitude decides which pole the polar axis
/// is aimed at) and a source of local sidereal time. Both are properties of the *node*, not of the
/// mount, and both are already held by the binary that builds the registry — so they are
/// constructor parameters here, exactly as `SimulatorMountFactory`'s profile and fault plan are.
/// Widening the trait for one driver would make every other factory carry a site it never reads.
#[derive(Debug, Clone)]
pub struct SkywatcherMountFactory {
    site: SiteConfig,
    clock: Arc<dyn SiderealClock>,
}

impl SkywatcherMountFactory {
    /// A factory for one observing site, with the clock the node supplies.
    #[must_use]
    pub fn new(site: SiteConfig, clock: Arc<dyn SiderealClock>) -> Self {
        Self { site, clock }
    }
}

#[async_trait]
impl MountFactory for SkywatcherMountFactory {
    fn name(&self) -> &'static str {
        // Must equal `MountDriver::Skywatcher.as_str()`, or the driver named by the shipped
        // `config/field-node.example.yaml` is unreachable from configuration. Asserted below
        // rather than trusted.
        "skywatcher"
    }

    fn create(&self, config: &MountConfig) -> Result<Arc<dyn MountDevice>, DriverInitError> {
        Ok(Arc::new(SkywatcherMount::new(
            config,
            self.site.clone(),
            Arc::clone(&self.clock),
        )?))
    }

    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        // **Scan, do not open.** `port::autodetect` proves a port by talking to it, and that is
        // the right thing at `connect` — where the operator has asked for the mount — but the
        // wrong thing here: `probe_all` runs from `/api/system/detect` and may be called while the
        // driver is connected, and opening the port the serial task owns would take the cable out
        // from under a slew. So this reports what is worth opening and lets connect decide.
        #[cfg(feature = "serialport")]
        {
            let found = tokio::task::spawn_blocking(super::port::scan)
                .await
                .map_err(|error| {
                    DeviceError::Transport(format!("the port scan task failed: {error}"))
                })??;
            Ok(found
                .candidates
                .into_iter()
                .map(|candidate| {
                    DetectedDevice::new(
                        "skywatcher",
                        astroctl_core::types::DeviceKind::Mount,
                        candidate.path.display().to_string(),
                        format!(
                            "{} USB-serial adapter — possible Sky-Watcher mount",
                            candidate.adapter.family
                        ),
                    )
                })
                .collect())
        }
        #[cfg(not(feature = "serialport"))]
        {
            // Nothing to report rather than an error: a build without the fd layer has no way to
            // enumerate ports, and a probe that failed would put a red line in `/api/system/detect`
            // for a driver that is simply not compiled in.
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::mock_port::{MockPort, Scripted};
    use astroctl_core::config::{MountDriver, MountLimits, SerialConfig};
    use astroctl_hal::registry::DriverRegistry;

    /// Vilnius, the site the shipped example configures.
    fn site() -> SiteConfig {
        SiteConfig {
            latitude: 54.6872,
            longitude: 25.2797,
            elevation: 112.0,
            timezone: "Europe/Vilnius".to_owned(),
        }
    }

    /// `config/field-node.example.yaml`'s mount section, as a struct.
    fn config() -> MountConfig {
        MountConfig {
            driver: MountDriver::Skywatcher,
            port: "auto".to_owned(),
            baud: 9600,
            // Zero, so a test that is not about settling does not pay three virtual seconds of it.
            settle_time_seconds: 0,
            serial: SerialConfig {
                request_timeout_ms: 500,
                request_retries: 1,
                heartbeat_misses: 3,
                poll_hz: 1,
            },
            limits: MountLimits {
                min_altitude_degrees: 15.0,
                meridian_limit_minutes: 15.0,
                max_travel_from_home_degrees: 180.0,
                slew_ttl_default_ms: 500,
                slew_ttl_max_ms: 2000,
            },
            geometry: None,
            indi_device: None,
            ascom_host: None,
        }
    }

    fn mount(port: &MockPort) -> SkywatcherMount {
        SkywatcherMount::over_wire(
            &config(),
            site(),
            Arc::new(FixedSiderealTime(180.0)),
            port.factory(),
        )
        .expect("the example park position is a coordinate")
    }

    fn frames(port: &MockPort) -> Vec<String> {
        port.writes()
            .into_iter()
            .map(|written| {
                String::from_utf8_lossy(&written.bytes)
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn the_handshake_reads_both_axes_and_transmits_no_action_it_did_not_have_to() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount
            .connect()
            .await
            .expect("the mock answers the spike's own captures");

        // The canned `:f` is `101` — initialised and stopped, which is the state a mount spends
        // its life in — so no `F` is sent. Every frame here is lowercase, which is the rule the
        // spike's own read-only harnesses held themselves to.
        assert_eq!(
            frames(&port),
            vec![":e1", ":a1", ":b1", ":g1", ":e2", ":a2", ":b2", ":g2", ":f1", ":f2",],
        );
        assert!(
            frames(&port)
                .iter()
                .all(|frame| frame.as_bytes()[1].is_ascii_lowercase()),
            "the handshake must not be able to move the mount"
        );

        // ...and connecting twice is free (HAL rule 7).
        mount.connect().await.expect("idempotent");
        assert_eq!(frames(&port).len(), 10);
    }

    #[tokio::test(start_paused = true)]
    async fn an_uninitialised_axis_gets_its_f_and_an_initialised_one_does_not() {
        let port = MockPort::new();
        // What the operator's mount answered at power-on: not running, *not* initialised.
        port.answers(b'f', b"=100\r");
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        let sent = frames(&port);
        assert!(sent.contains(&":F1".to_owned()), "{sent:?}");
        assert!(sent.contains(&":F2".to_owned()), "{sent:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn identity_and_capabilities_are_readable_before_connecting() {
        // SDD §8.1: the registry builds drivers at startup and `/api/system/info` reports them
        // long before an operator presses Connect.
        let port = MockPort::new();
        let mount = mount(&port);
        let capabilities = mount.capabilities();
        assert!(capabilities.has_pulse_guide);
        assert!(!capabilities.has_pec);
        assert_eq!(capabilities.position_resolution_bits, 24);
        assert_eq!(mount.device_info().protocol, "synta-serial");
        assert!(
            mount.device_info().firmware.is_none(),
            "firmware is learned at connect"
        );
        assert!(port.writes().is_empty(), "none of that touched the wire");
        assert!(matches!(
            mount.position().await,
            Err(DeviceError::NotConnected)
        ));
        assert_eq!(
            mount
                .status()
                .await
                .expect("a disconnected mount has a status"),
            MountStatus::disconnected()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_capabilities_sharpen_to_the_counts_per_revolution_the_mount_reported() {
        // 9,024,000 spans 24 bits, so the operator's HEQ5 does not change the answer. A mount with
        // a coarser gearbox would, and reporting the register width for it would be claiming a
        // resolution the mechanism does not have.
        let port = MockPort::new();
        port.answers(b'a', b"=A00F00\r"); // 0x000FA0 = 4000 counts per revolution
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        assert_eq!(mount.capabilities().position_resolution_bits, 12);
    }

    #[tokio::test(start_paused = true)]
    async fn the_position_is_the_counters_read_through_the_injected_sidereal_time() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        port.clear_writes();

        // Both counters at home: the tube is on the pole, so declination is +90° in the northern
        // hemisphere and the right ascension is whatever LST says, because every hour angle meets
        // at the pole.
        let position = mount.position().await.expect("the mount answered");
        assert_eq!(frames(&port), vec![":j1", ":j2"]);
        assert!(
            (position.dec.degrees() - 90.0).abs() < 1e-9,
            "home points at the pole, not {}",
            position.dec.degrees()
        );
        // The position poll deliberately does **not** publish a pier side. A Synta counter counts
        // commanded steps, so a stalled axis reports motion the metal never made; letting the poll
        // derive the branch let a jam near the pole flip the declination motor's sense under the
        // operator's next press (E11, observed 2026-08-01). Only `connect` and an arrived `goto`
        // — the acts that can legitimately change pier side, with the axes stopped — write it.
        assert_eq!(
            mount.pier_side(),
            None,
            "a routine position read must not derive the branch"
        );
    }

    /// The branch *is* established by the deliberate acts, or `sense` would have nothing to use.
    #[tokio::test(start_paused = true)]
    async fn a_cold_declination_slew_establishes_the_branch_before_moving() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        port.clear_writes();
        assert_eq!(mount.pier_side(), None, "cold");

        mount
            .slew(Axis::Dec, Direction::North, SlewSpeed::Slow)
            .await
            .expect("the mount accepted the slew");

        // It read the counters *before* programming motion — that read is the deliberate one.
        let sent = frames(&port);
        assert!(
            sent.iter().position(|f| f.starts_with(":j")).unwrap()
                < sent.iter().position(|f| f.starts_with(":G")).unwrap(),
            "the branch is established before the motion mode is chosen, not after: {sent:?}"
        );
        assert!(mount.pier_side().is_some(), "and it was cached");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_mount_shows_its_right_ascension_climbing() {
        // The property the simulator's module header states and SDD §5.2.3 implies: RA = LST − HA,
        // so a drive that has stopped does not hold a coordinate, it holds an hour angle.
        #[derive(Debug)]
        struct Turning(Mutex<f64>);
        impl SiderealClock for Turning {
            fn local_sidereal_degrees(&self) -> f64 {
                *self.0.lock().expect("uncontended")
            }
        }

        let port = MockPort::new();
        // Off the pole, so the right ascension is a real number rather than a degenerate one.
        port.answers(b'j', b"=000090\r"); // 0x900000, a quarter turn past home on both axes
        let clock = Arc::new(Turning(Mutex::new(180.0)));
        let mount = SkywatcherMount::over_wire(&config(), site(), clock.clone(), port.factory())
            .expect("valid");
        mount.connect().await.expect("connects");

        let before = mount.position().await.expect("polled");
        *clock.0.lock().expect("uncontended") += 15.0; // one hour of sidereal time
        let after = mount.position().await.expect("polled");
        let climbed = (after.ra.hours() - before.ra.hours()).rem_euclid(24.0);
        assert!(
            (climbed - 1.0).abs() < 1e-6,
            "the sky turned an hour and the mount's RA moved {climbed} h"
        );
        assert!((after.dec.degrees() - before.dec.degrees()).abs() < 1e-9);
    }

    #[tokio::test(start_paused = true)]
    async fn tracking_programs_the_low_speed_slew_the_spike_drove() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        port.clear_writes();

        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("sidereal is in range");
        // `motion.py` sent exactly these for the run E10 measured.
        assert_eq!(frames(&port), vec![":G110", ":I16C0200", ":J1"]);
        let status = mount.status().await.expect("status");
        assert_eq!(status.state, MountState::Tracking);
        assert_eq!(status.tracking, Some(TrackingMode::Sidereal));
        assert!(status.is_consistent());

        port.clear_writes();
        mount.stop_tracking().await.expect("stops");
        assert_eq!(frames(&port), vec![":K1"], "the ramped stop, not `L`");
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Idle
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_manual_slew_renewal_costs_no_exchange_and_a_change_of_speed_does() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        port.clear_writes();

        mount
            .slew(Axis::Ra, Direction::West, SlewSpeed::Slow)
            .await
            .expect("slews");
        let first = frames(&port).len();
        assert!(first >= 3, "G, I, J at least: {:?}", frames(&port));

        // The dead-man's switch renews at 2 Hz. Re-issuing the motor commands would restart the
        // ramp and make the mount stutter under the operator's thumb.
        for _ in 0..4 {
            mount
                .slew(Axis::Ra, Direction::West, SlewSpeed::Slow)
                .await
                .expect("renews");
        }
        assert_eq!(
            frames(&port).len(),
            first,
            "a renewal is a no-op at the device"
        );

        // A different speed is a slider move, not a conflict. Medium rather than Fast, because
        // Fast is a chunked rung since E16 and its goto readbacks want a scripted mount — the
        // slider-move rule is the thing under test here, and two unbounded rungs exercise it.
        mount
            .slew(Axis::Ra, Direction::West, SlewSpeed::Medium)
            .await
            .expect("changes speed");
        assert!(frames(&port).len() > first);

        port.clear_writes();
        mount.stop_slew(Axis::Ra).await.expect("stops");
        assert_eq!(frames(&port), vec![":K1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_direction_that_is_not_on_the_axis_is_refused_before_it_reaches_the_wire() {
        // `slew(Axis::Ra, Direction::North)` is a caller bug, and the driver is the last place
        // that can catch it: one layer up it becomes an axis moving the wrong way.
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        port.clear_writes();
        assert!(matches!(
            mount
                .slew(Axis::Ra, Direction::North, SlewSpeed::Slow)
                .await,
            Err(DeviceError::Rejected(_))
        ));
        assert!(port.writes().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_emergency_stop_reaches_the_wire_while_a_normal_exchange_is_wedged() {
        // The lock-freedom claim, asserted rather than argued. A goto is in flight against a mount
        // that has stopped answering; the stop must not wait for it. T-SER-3's conditions.
        let port = MockPort::new();
        let mount = Arc::new(mount(&port));
        mount.connect().await.expect("connects");
        port.clear_writes();

        // The goto's first counter read goes unanswered: the mount is silent and the normal lane
        // will sit on it for the full 500 ms request timeout. That is the wedge — an exchange that
        // holds the cable and cannot be asked to give it up politely.
        port.script([Scripted::DeadAir]);

        let slewing = {
            let mount = Arc::clone(&mount);
            tokio::spawn(async move {
                mount
                    .goto(RaDec::from_parts(5.5, 22.0).expect("valid"))
                    .await
            })
        };
        // Long enough for the goto's first frame to be on the wire and unanswered, and far short
        // of the 500 ms it will otherwise wait.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!port.writes().is_empty(), "the goto reached the wire");
        port.clear_writes();

        let began = tokio::time::Instant::now();
        mount.emergency_stop().await.expect("both axes stopped");

        // **Measured at the wire, not at the call's return.** SDD §5.8.2 budgets "API handler to
        // bytes-on-wire", and the reply is a further round trip that no amount of lock discipline
        // can remove — there is one cable, and the second axis's `L` necessarily follows the
        // first's reply by one round trip. What this driver owns, and what this asserts, is the
        // half from the trait call to the first stop byte.
        let writes = port.writes();
        let first_stop = writes
            .iter()
            .find(|written| written.opcode() == Some(b'L'))
            .expect("an emergency stop must reach the wire");
        let latency = first_stop.at.saturating_duration_since(began);
        assert!(
            latency <= Duration::from_millis(20),
            "the emergency stop took {latency:?} to reach the wire, past SDD §5.8.2's 20 ms \
             budget — behind an exchange that had 450 ms of its own timeout still to run"
        );

        let stops: Vec<String> = frames(&port);
        assert!(
            stops.contains(&":L1".to_owned()) && stops.contains(&":L2".to_owned()),
            "both axes must be stopped instantly: {stops:?}"
        );

        // ...and the goto it interrupted says so with `Aborted`, not `Rejected`.
        let outcome = slewing.await.expect("the supervisor task ran");
        assert!(
            matches!(outcome, Err(DeviceError::Aborted(_))),
            "{outcome:?}"
        );
        assert_eq!(mount.shared.locked().tracking, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_goto_while_one_is_running_is_busy_and_never_a_retarget() {
        let port = MockPort::new();
        let mount = Arc::new(mount(&port));
        mount.connect().await.expect("connects");

        let first = {
            let mount = Arc::clone(&mount);
            tokio::spawn(async move {
                mount
                    .goto(RaDec::from_parts(1.0, 10.0).expect("valid"))
                    .await
            })
        };
        // One millisecond: less than a round trip, so the first goto is still on its first frame.
        // The claim is taken synchronously before that frame, which is the whole point — `Busy` is
        // a decision from the driver's own state and must not depend on how fast the wire is.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(matches!(
            mount
                .goto(RaDec::from_parts(2.0, 20.0).expect("valid"))
                .await,
            Err(DeviceError::Busy(_))
        ));
        // The canned mock answers every `:h` with home, so the first goto fails its pre-motion
        // readback rather than completing — which is fine here and asserted properly elsewhere.
        let _ = first.await.expect("the supervisor ran");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_heartbeat_is_a_fault_state_and_not_a_failed_status_call() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        // Three consecutive misses is `mount.serial.heartbeat_misses`; each request retries once,
        // so six silent exchanges is three missed requests.
        port.goes_quiet_for(64);
        for _ in 0..3 {
            let _ = mount.position().await;
        }
        let status = mount
            .status()
            .await
            .expect("a faulted mount still has a status");
        assert_eq!(status.state, MountState::Fault);
        assert!(status.is_consistent());
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_reaches_the_caller_as_a_rejection_rather_than_a_link_failure() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        port.script([Scripted::Refuses(
            crate::skywatcher::codec::MountError::InvalidParameter,
        )]);
        let error = mount.position().await.expect_err("the mount said no");
        assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_releases_the_port_without_stopping_the_mount() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        port.clear_writes();

        mount.disconnect().await.expect("releases the port");
        assert!(
            port.writes().is_empty(),
            "a service restart mid-session must leave a tracking mount tracking: {:?}",
            frames(&port)
        );
        assert!(matches!(
            mount.position().await,
            Err(DeviceError::NotConnected)
        ));
        // Idempotent both ways.
        mount.disconnect().await.expect("already disconnected");
        mount.connect().await.expect("reconnects");
    }

    #[tokio::test(start_paused = true)]
    async fn sync_says_unsupported_rather_than_guessing_at_an_unverified_opcode() {
        let port = MockPort::new();
        let mount = mount(&port);
        mount.connect().await.expect("connects");
        port.clear_writes();
        assert!(matches!(
            mount
                .sync(RaDec::from_parts(3.0, 30.0).expect("valid"))
                .await,
            Err(DeviceError::Unsupported)
        ));
        assert!(port.writes().is_empty());
    }

    #[test]
    fn the_watchdog_source_has_exactly_one_owner() {
        let port = MockPort::new();
        let mount = mount(&port);
        assert!(mount.take_watchdog().is_some());
        assert!(
            mount.take_watchdog().is_none(),
            "a second receiver would silently split the heartbeat between two watchdogs"
        );
    }

    #[test]
    fn the_registry_name_is_the_one_the_shipped_example_configures() {
        let factory = SkywatcherMountFactory::new(site(), Arc::new(FixedSiderealTime(0.0)));
        assert_eq!(factory.name(), MountDriver::Skywatcher.as_str());

        let mut registry = DriverRegistry::new();
        registry.register_mount(factory).expect("registers");
        // The whole path the shipped `config/field-node.example.yaml` takes: `mount.driver` →
        // registry → a constructed driver. Construction does no I/O, so this works with no mount,
        // no port and no `serialport` feature — which is what SDD §8.1 requires of it.
        let driver = registry
            .create_mount(MountDriver::Skywatcher.as_str(), &config())
            .expect("the example config builds a driver");
        assert_eq!(driver.device_info().protocol, "synta-serial");
        assert!(driver.capabilities().has_pulse_guide);
    }

    #[tokio::test]
    async fn a_build_without_the_fd_layer_refuses_to_connect_and_says_which_feature() {
        // Construction succeeds and connect fails with a message naming the missing feature rather
        // than the missing mount — the shape `libgphoto2` established. Under `--features
        // serialport` the same call reaches autodetect instead, which is M3-T05's path.
        let factory = SkywatcherMountFactory::new(site(), Arc::new(FixedSiderealTime(0.0)));
        let driver = factory.create(&config()).expect("constructs");
        let error = driver.connect().await.expect_err("there is no mount here");
        assert!(matches!(error, DeviceError::Transport(_)), "{error:?}");
    }
}
