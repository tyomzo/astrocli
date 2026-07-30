//! `SimulatorMount` — a [`MountDevice`] with the HEQ5's timing and no telescope (PRD §4.5,
//! HAL-11, SDD §9).
//!
//! # What it is faithful to
//!
//! Not the wire protocol — that is the real driver's job (M3-T02) and simulating bytes would
//! only test the codec twice. What this reproduces is everything a caller *above* the HAL can
//! observe and would otherwise get wrong:
//!
//! * **Nothing is instant.** Every method costs at least one 16 ms serial round trip, because a
//!   caller written against a free device is a caller that polls in a loop and discovers the
//!   cost on a Raspberry Pi in a field in the dark.
//! * **Slews take the time they take.** Trapezoidal ramps calibrated to the one measured goto
//!   trace, so a 30° goto is eleven seconds of motion and not an assignment.
//! * **The link fails the way links fail** — [`FaultPlan`] scripts timeouts, garbled replies,
//!   an unplugged adapter and a jammed axis.
//! * **The safety path can be tested against it.** An `emergency_stop` lands exactly when it is
//!   issued (M1-T05 measures against this), and it does not queue behind whatever else is on the
//!   wire, because the real driver has a priority lane and a simulator without one would let a
//!   latency bug through.
//!
//! # The two mechanical axes
//!
//! State is a *hour-angle axis* and a *declination axis* in degrees, plus a sidereal clock —
//! the same decomposition SDD §5.2.3 gives the real driver, where RA/DEC exists only as a
//! conversion from counters and local sidereal time. Keeping it means the physics comes out
//! right without any special cases: RA = LST − hour angle, so a mount that is *not* tracking
//! shows its RA climbing at the sidereal rate (the sky moved, the tube did not), and a mount
//! that *is* tracking turns its hour-angle axis at exactly that rate and holds RA still. A
//! simulator that stored RA/DEC directly would have to special-case both, and would get the
//! lunar and solar rates backwards.
//!
//! Sidereal time here is relative: no site longitude reaches a driver ([`MountConfig`] has none
//! and [`MountFactory::create`](astroctl_hal::registry::MountFactory::create) passes nothing
//! else), so the simulator anchors LST at connect such that the mount starts pointing at its
//! configured park position. Only *differences* in LST affect anything observable, so this costs
//! nothing and avoids inventing a longitude.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use astroctl_core::config::{MountConfig, MountDriver, MountLimits, ParkPosition, SerialConfig};
use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    Axis, DeviceInfo, DeviceKind, Direction, GuideRate, MountCapabilities, MountState, MountStatus,
    RaDec, SlewSpeed, TrackingMode,
};
use astroctl_hal::mount::MountDevice;
use astroctl_hal::registry::{DetectedDevice, DriverInitError, MountFactory};
use async_trait::async_trait;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::fault::{garbled_error, ArmedFaults, FaultPlan, Injected, MountCommand};
use super::motion::{slew_rate, tracking_rate, AxisPlan};
use super::profile::{SimulatorProfile, SIDEREAL_DEG_PER_SEC};

/// How far into a slew [`Fault::StallDuringSlew`](super::fault::Fault::StallDuringSlew) jams the
/// axis. Past the ramp-up and well short of the target, so a stall is unambiguous: the mount
/// moved, and then it stopped somewhere that is not where it was told to be.
const STALL_FRACTION: f64 = 0.6;

/// Exchanges each command costs, in units of one 16 ms round trip.
///
/// Taken from the Synta sequences of SDD §5.2.2/§5.2.3 rather than guessed, because the count is
/// what a caller feels: a goto costs eight (`G`, `I`, `H`, `M`, three mandated readbacks, `J`)
/// and is therefore ~128 ms of dead air before anything moves, while a stop costs one.
mod exchanges {
    /// `:e`, `:a`, `:b`, `:g`, `:f`, `:F` — the handshake of SDD §5.2.2.
    pub const CONNECT: u32 = 6;
    /// Closing a port talks to the kernel, not to the mount; charged one to keep it honest that
    /// the operation is not free (the spike measured 38 ms for close+reopen).
    pub const DISCONNECT: u32 = 1;
    /// `:j1` and `:j2`.
    pub const POSITION: u32 = 2;
    /// `:f1` and `:f2`.
    pub const STATUS: u32 = 2;
    /// The goto sequence, both axes pipelined as far as the protocol allows.
    pub const GOTO: u32 = 8;
    /// `:E1` and `:E2` — set both counters.
    pub const SYNC: u32 = 2;
    /// `:G1`, `:I1`, `:J1` — tracking is a low-speed SLEW, not a mode.
    pub const TRACKING: u32 = 3;
    /// `:K1` — a ramped stop.
    pub const STOP: u32 = 1;
    /// `:G`, `:I`, `:J` for the axis being moved.
    pub const SLEW: u32 = 3;
    /// `:P` to set the rate, then the pulse itself.
    pub const GUIDE: u32 = 2;
    /// `:F1`, `:F2`. The Synta protocol has no park opcode — park is a host-side goto and
    /// unpark is re-initialising the axes, which is the only wire traffic it causes.
    pub const UNPARK: u32 = 2;
}

// -----------------------------------------------------------------------------------------
// Internal state
// -----------------------------------------------------------------------------------------

/// What the driver believes about its transport.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Link {
    /// Never opened, or closed cleanly.
    Down,
    /// Open. `drops_at` is armed by [`Fault::DisconnectAfter`](super::fault::Fault).
    Up { drops_at: Option<Instant> },
    /// Opened and then lost. Distinct from `Down` because the HAL requires a mount that has
    /// stopped answering to report [`MountState::Fault`] rather than to make `status()` fail —
    /// "the mount is not answering" is a state the operator must see, not a blank panel.
    Faulted,
}

/// What owns an axis right now.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Activity {
    /// Holding still or tracking; anybody may take it.
    Steady,
    /// A command is on the wire. Held from before the first `.await` so that two gotos issued a
    /// millisecond apart cannot both be accepted — `Busy` must not depend on wire timing.
    Reserved { slewing: bool },
    /// A goto owns the axis until `finish` (motion *and* settle).
    Goto { finish: Instant, parking: bool },
    /// A manual slew owns the axis until something stops it. Never finishes on its own, which
    /// is why the dead-man's switch above the HAL exists (SDD §5.8.1).
    Manual { dir: Direction, speed: SlewSpeed },
    /// A guide pulse, which is a rate change rather than a slew: it owns the axis but does not
    /// make the mount report `slewing`, or every guided exposure would show the mount slewing
    /// at 1 Hz for the whole night.
    Guiding { finish: Instant },
    /// Jammed mid-slew and going nowhere.
    Stalled,
}

/// One mechanical axis: where it started, what it is doing, and since when.
#[derive(Debug)]
struct AxisState {
    started: Instant,
    plan: AxisPlan,
    activity: Activity,
}

impl AxisState {
    fn new(position: f64, now: Instant) -> Self {
        Self {
            started: now,
            plan: AxisPlan::steady(position, 0.0),
            activity: Activity::Steady,
        }
    }

    fn elapsed(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.started).as_secs_f64()
    }

    fn position(&self, now: Instant) -> f64 {
        self.plan.position_at(self.elapsed(now))
    }

    fn velocity(&self, now: Instant) -> f64 {
        self.plan.velocity_at(self.elapsed(now))
    }

    /// Whether another command may take this axis.
    fn owned(&self, now: Instant) -> bool {
        match self.activity {
            Activity::Steady => false,
            Activity::Reserved { .. } | Activity::Manual { .. } | Activity::Stalled => true,
            Activity::Goto { finish, .. } | Activity::Guiding { finish } => now < finish,
        }
    }

    /// Whether the mount should report itself slewing.
    fn slewing(&self, now: Instant) -> bool {
        match self.activity {
            Activity::Steady | Activity::Guiding { .. } => false,
            Activity::Reserved { slewing } => slewing,
            Activity::Manual { .. } | Activity::Stalled => true,
            Activity::Goto { finish, .. } => now < finish,
        }
    }

    fn parking(&self, now: Instant) -> bool {
        matches!(self.activity, Activity::Goto { finish, parking: true } if now < finish)
    }

    /// True once a park goto has arrived — which is what makes `park` self-completing: the
    /// mount is parked because the plan finished, not because a future was still there to say so
    /// (HAL rule 3).
    fn arrived_parked(&self, now: Instant) -> bool {
        matches!(self.activity, Activity::Goto { finish, parking: true } if now >= finish)
    }

    /// Stops the axis dead where it is and holds `rate` — Synta `L`.
    fn halt(&mut self, now: Instant, rate: f64) {
        self.plan = self.plan.frozen_at(self.elapsed(now), rate);
        self.started = now;
        self.activity = Activity::Steady;
    }

    /// Stops the axis with its deceleration ramp — Synta `K`.
    fn ramp_to_stop(&mut self, now: Instant, decel: f64, rate: f64) {
        self.plan = self.plan.ramped_stop(self.elapsed(now), decel, rate);
        self.started = now;
        self.activity = Activity::Steady;
    }

    /// Changes what the axis does once it has finished moving, without disturbing the move.
    ///
    /// This is how `start_tracking`/`stop_tracking` reach a goto that is already in flight: the
    /// rate to resume at is baked into the plan when the goto starts (so that dropping the goto
    /// future still leaves the mount tracking), and an operator who changes their mind mid-slew
    /// has to be able to change that baked-in decision.
    fn set_terminal_rate(&mut self, now: Instant, rate: f64) {
        if self.elapsed(now) < self.plan.settled_after() {
            self.plan = self.plan.with_terminal_rate(rate);
        } else {
            self.plan = self.plan.frozen_at(self.elapsed(now), rate);
            self.started = now;
        }
    }
}

/// Both axes and the sidereal clock they are read against.
#[derive(Debug)]
struct Axes {
    /// When the sidereal clock was anchored.
    epoch: Instant,
    /// Local sidereal time in degrees at [`epoch`](Self::epoch).
    lst0_degrees: f64,
    /// Hour-angle axis, degrees. RA = LST − this.
    ra: AxisState,
    /// Declination axis, degrees.
    dec: AxisState,
}

impl Axes {
    fn lst_degrees(&self, now: Instant) -> f64 {
        self.lst0_degrees
            + now.saturating_duration_since(self.epoch).as_secs_f64() * SIDEREAL_DEG_PER_SEC
    }

    fn axis(&mut self, axis: Axis) -> &mut AxisState {
        match axis {
            Axis::Ra => &mut self.ra,
            Axis::Dec => &mut self.dec,
        }
    }
}

/// Everything behind the lock.
#[derive(Debug)]
struct State {
    link: Link,
    faults: ArmedFaults,
    /// `None` until the first connect: the sidereal clock needs a runtime clock to anchor to,
    /// and a factory builds drivers at startup where there may not be one (SDD §8.1).
    axes: Option<Axes>,
    tracking: Option<TrackingMode>,
    parked: bool,
    /// Bumped by every command that takes the axes away from someone. A `goto` that wakes to
    /// find this changed knows it was overridden and by what, without any shared cancellation
    /// token — and, unlike a token, this survives the goto future being dropped.
    generation: u64,
    /// What last took the axes, for the message the overridden caller returns.
    overridden_by: &'static str,
}

impl State {
    fn connected(&self) -> Result<(), DeviceError> {
        match self.link {
            Link::Up { .. } => Ok(()),
            Link::Down => Err(DeviceError::NotConnected),
            Link::Faulted => Err(link_lost()),
        }
    }

    fn axes(&mut self) -> Result<&mut Axes, DeviceError> {
        self.axes.as_mut().ok_or(DeviceError::NotConnected)
    }

    fn parked_at(&self, now: Instant) -> bool {
        self.parked
            || self
                .axes
                .as_ref()
                .is_some_and(|axes| axes.ra.arrived_parked(now) || axes.dec.arrived_parked(now))
    }

    /// Takes `wanted` for a command, freezing each axis at this instant so that whatever it was
    /// doing stops accumulating while the command is on the wire.
    ///
    /// Returns the generation the caller must still hold when it comes back.
    fn reserve(
        &mut self,
        now: Instant,
        wanted: &[Axis],
        slewing: bool,
        by: &'static str,
    ) -> Result<u64, DeviceError> {
        self.connected()?;
        if self.parked_at(now) {
            return Err(DeviceError::Rejected(
                "the mount is parked; unpark before commanding motion".to_owned(),
            ));
        }
        let axes = self.axes()?;
        for axis in wanted {
            if axes.axis(*axis).owned(now) {
                return Err(DeviceError::Busy("the mount is already moving"));
            }
        }
        for axis in wanted {
            let state = axes.axis(*axis);
            let rate = state.plan.terminal_rate();
            state.halt(now, rate);
            state.activity = Activity::Reserved { slewing };
        }
        self.generation += 1;
        self.overridden_by = by;
        Ok(self.generation)
    }

    /// Gives the axes back after a command failed on the wire, so a timeout does not leave the
    /// mount permanently `Busy`.
    fn release(&mut self, now: Instant, generation: u64, tracking_rate_now: f64) {
        if self.generation != generation {
            return; // somebody else already took over; theirs wins
        }
        let Ok(axes) = self.axes() else { return };
        for axis in [Axis::Ra, Axis::Dec] {
            let state = axes.axis(axis);
            if matches!(state.activity, Activity::Reserved { .. }) {
                let rate = if axis == Axis::Ra {
                    tracking_rate_now
                } else {
                    0.0
                };
                state.halt(now, rate);
            }
        }
    }

    /// The RA-axis rate that tracking implies right now.
    fn tracking_rate(&self) -> f64 {
        self.tracking.map_or(0.0, tracking_rate)
    }

    fn status_at(&self, now: Instant) -> MountStatus {
        let Some(axes) = self.axes.as_ref() else {
            return MountStatus::disconnected();
        };
        let slewing = axes.ra.slewing(now) || axes.dec.slewing(now);
        let parked = self.parked_at(now);
        match self.link {
            Link::Down => MountStatus::disconnected(),
            // Reported as it is, motion included: a mount that has stopped answering while it is
            // still slewing is the case REL-02's watchdog exists for, and hiding the motion here
            // would make the scariest state look like the calmest one.
            Link::Faulted => MountStatus {
                state: MountState::Fault,
                tracking: self.tracking,
                slewing,
                parked,
            },
            Link::Up { .. } => {
                if slewing {
                    let parking = axes.ra.parking(now) || axes.dec.parking(now);
                    MountStatus {
                        state: if parking {
                            MountState::Parking
                        } else {
                            MountState::Slewing
                        },
                        // The mode that is running or will be restored, not "off because the
                        // axes are busy" — otherwise the UI's tracking indicator blinks off for
                        // the duration of every goto.
                        tracking: self.tracking,
                        slewing: true,
                        parked: false,
                    }
                } else if parked {
                    MountStatus {
                        state: MountState::Parked,
                        tracking: None,
                        slewing: false,
                        parked: true,
                    }
                } else if let Some(mode) = self.tracking {
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
                }
            }
        }
    }
}

/// The error a lost link produces. `Transport` rather than `NotConnected` because the two mean
/// different things to the operator: one says "press Connect", the other says "look at the
/// cable", and SDD §4.2 maps them to different codes for exactly that reason.
fn link_lost() -> DeviceError {
    DeviceError::Transport("the serial link is down (simulated disconnect)".to_owned())
}

// -----------------------------------------------------------------------------------------
// The driver
// -----------------------------------------------------------------------------------------

/// A mount that moves like an HEQ5 and is not there (PRD §4.5, HAL-11).
///
/// Build it directly in a test — `SimulatorMount::new(&config, profile, faults)` — or through
/// [`SimulatorMountFactory`] when the mount has to come from `mount.driver: simulator` in the
/// operator's YAML.
#[derive(Debug)]
pub struct SimulatorMount {
    profile: SimulatorProfile,
    /// Where `park` goes (`mount.park_position`).
    park: RaDec,
    /// `mount.settle_time_seconds` — how long the tube rings before a caller may expose.
    settle: Duration,
    /// `mount.serial.request_timeout_ms`, reported by an injected timeout.
    request_timeout: Duration,
    /// What a timeout actually costs a caller: the timeout, plus every retry
    /// (`mount.serial.request_retries`) waiting the same amount again.
    timeout_budget: Duration,
    /// The serial line. One exchange at a time, because there is one cable — and `emergency_stop`
    /// deliberately does *not* take it (SDD §5.2.4's priority lane). Without this the simulator
    /// would make a driver that queues its e-stop behind a stalled poll look correct.
    line: Semaphore,
    state: Mutex<State>,
}

impl SimulatorMount {
    /// Builds a simulator from the same configuration a real driver gets, plus the two knobs
    /// SDD §9 requires to be constructor parameters.
    ///
    /// # Errors
    /// [`DriverInitError`] if `mount.park_position` is not a coordinate.
    pub fn new(
        config: &MountConfig,
        profile: SimulatorProfile,
        faults: &FaultPlan,
    ) -> Result<Self, DriverInitError> {
        let park = RaDec::from_parts(
            config.park_position.ra_hours,
            config.park_position.dec_degrees,
        )?;
        let request_timeout = Duration::from_millis(config.serial.request_timeout_ms);
        Ok(Self {
            profile,
            park,
            settle: Duration::from_secs(u64::from(config.settle_time_seconds)),
            request_timeout,
            timeout_budget: request_timeout * (1 + config.serial.request_retries),
            line: Semaphore::new(1),
            state: Mutex::new(State {
                link: Link::Down,
                faults: faults.arm(),
                axes: None,
                tracking: None,
                parked: false,
                generation: 0,
                overridden_by: "nothing",
            }),
        })
    }

    /// A simulator on the default configuration, for callers that only want a mount.
    ///
    /// # Errors
    /// [`DriverInitError`] as [`new`](Self::new); unreachable with the built-in defaults.
    pub fn with_profile(profile: SimulatorProfile) -> Result<Self, DriverInitError> {
        Self::new(&default_config(), profile, &FaultPlan::none())
    }

    fn locked(&self) -> MutexGuard<'_, State> {
        // Poisoning would mean a panic inside one of these short synchronous sections, which
        // would be a bug in this file rather than a condition to recover from.
        self.state
            .lock()
            .expect("simulator state is never poisoned")
    }

    /// One request/response exchange with the simulated controller: takes the line, waits, and
    /// applies whatever fault the script has for it.
    ///
    /// Returns the instant the reply landed — every caller needs it, and taking it here rather
    /// than calling `Instant::now()` again afterwards keeps the "when did this happen" answer
    /// consistent between the wait and the state change it causes.
    async fn exchange(&self, command: MountCommand, count: u32) -> Result<Instant, DeviceError> {
        let permit = self
            .line
            .acquire()
            .await
            .expect("the simulated serial line is never closed");
        let outcome = self.arbitrate(command, count);
        let result = match outcome? {
            Some(Injected::Timeout) => {
                // The line stays held for the whole budget: a mount that is not answering blocks
                // the cable, which is the entire reason the e-stop lane exists.
                tokio::time::sleep(self.timeout_budget).await;
                Err(DeviceError::Timeout(self.request_timeout))
            }
            Some(Injected::Garbled) => {
                tokio::time::sleep(self.profile.round_trip).await;
                Err(garbled_error(command))
            }
            None => {
                tokio::time::sleep(self.profile.round_trip * count).await;
                Ok(Instant::now())
            }
        };
        drop(permit);
        result
    }

    /// The synchronous half of an exchange: is the link up, has it just died, and does the fault
    /// script have anything to say. Split out so no lock is ever held across an `.await`.
    fn arbitrate(
        &self,
        command: MountCommand,
        count: u32,
    ) -> Result<Option<Injected>, DeviceError> {
        let now = Instant::now();
        let mut state = self.locked();
        if let Link::Up {
            drops_at: Some(at), ..
        } = state.link
        {
            if now >= at {
                // The adapter fell out. Motion is deliberately untouched: an unplugged mount
                // keeps slewing, and a simulator that stopped here would make the safety layer
                // look correct without ever testing it.
                state.link = Link::Faulted;
                return Err(link_lost());
            }
        }
        state.connected()?;
        for _ in 0..count {
            if let Some(injected) = state.faults.next_exchange(command) {
                return Ok(Some(injected));
            }
        }
        Ok(None)
    }

    /// Reads both axes and turns them into a sky position.
    ///
    /// The two error terms live here rather than in the axis plans on purpose: periodic error
    /// and polar drift are things the mount *does not know about*. They perturb what an observer
    /// measures, not what the controller believes, so a goto still lands on its commanded
    /// counters and a plate solve still finds the mount somewhere else. Putting them in the
    /// plans would make the mount correct for them by construction, which is the one thing they
    /// exist to prevent.
    fn sky_position(&self, axes: &Axes, now: Instant) -> Result<RaDec, DeviceError> {
        let hour_angle = axes.ra.position(now);
        let periodic_error = if self.profile.periodic_error_arcsec == 0.0 {
            0.0
        } else {
            let turns = hour_angle / self.profile.worm_period_degrees;
            self.profile.periodic_error_arcsec / 3600.0 * (turns * std::f64::consts::TAU).sin()
        };
        let drift_minutes = now.saturating_duration_since(axes.epoch).as_secs_f64() / 60.0;
        let drift = self.profile.polar_drift_arcsec_per_min * drift_minutes / 3600.0;

        let ra_degrees = axes.lst_degrees(now) - hour_angle + periodic_error;
        // Clamped rather than wrapped: a declination axis driven past the pole is a mechanical
        // impossibility, and `DecDegrees` would reject it — on the 1 Hz poll path, which must
        // not be able to fail for an arithmetic reason.
        let dec_degrees = (axes.dec.position(now) + drift).clamp(-90.0, 90.0);
        RaDec::from_parts(ra_degrees / 15.0, dec_degrees)
            .map_err(|err| DeviceError::Protocol(err.to_string()))
    }

    /// Installs the two axis plans for a goto and returns when it will be over.
    fn start_goto(&self, state: &mut State, now: Instant, target: RaDec, parking: bool) -> Instant {
        let settle_seconds = self.settle.as_secs_f64();
        let restore_rate = if parking { 0.0 } else { state.tracking_rate() };
        let cruise = self.profile.goto_cruise_deg_per_sec;
        let stall = state.faults.take_stall();
        let axes = state
            .axes
            .as_mut()
            .expect("reserve() proved the axes exist");

        let hour_angle = axes.ra.position(now);
        let lst = axes.lst_degrees(now);
        let target_ra_degrees = target.ra.hours() * 15.0;

        let declination = axes.dec.position(now);
        let dec_delta = target.dec.degrees() - declination;
        let dec_plan = AxisPlan::moving(
            declination,
            dec_delta,
            &self.profile,
            cruise,
            settle_seconds,
            0.0,
        );

        // The target hour angle depends on when the mount arrives, and the arrival time depends
        // on the hour angle — the sky moves during a slew. Three passes converge to well under an
        // arcsecond, a fraction of the SDD's 10-count goto tolerance. A single pass would land a
        // 12 s slew three arcseconds east: small, but a *systematic* error in one direction,
        // which is exactly the kind that later gets blamed on the pointing model.
        //
        // The arrival being solved for is the arrival of the *goto*, not of the RA axis — which
        // is why the declination move is planned first and seeds the iteration. Aiming the RA
        // axis at its own arrival instead would leave the mount holding the right ascension the
        // sky had several seconds before the slew actually ended.
        let mut travel = dec_plan.motion_after();
        let mut hour_angle_delta = 0.0;
        for _ in 0..3 {
            let arrival_lst = lst + travel * SIDEREAL_DEG_PER_SEC;
            hour_angle_delta = wrap_degrees(arrival_lst - target_ra_degrees - hour_angle);
            let ra_travel = AxisPlan::moving(
                hour_angle,
                hour_angle_delta,
                &self.profile,
                cruise,
                0.0,
                0.0,
            )
            .motion_after();
            travel = ra_travel.max(dec_plan.motion_after());
        }

        let ra_plan = AxisPlan::moving(
            hour_angle,
            hour_angle_delta,
            &self.profile,
            cruise,
            settle_seconds,
            restore_rate,
        )
        .synchronised_to(travel);
        let dec_plan = dec_plan.synchronised_to(travel);
        let finish =
            now + Duration::from_secs_f64(ra_plan.settled_after().max(dec_plan.settled_after()));

        for (axis, plan) in [(&mut axes.ra, ra_plan), (&mut axes.dec, dec_plan)] {
            axis.started = now;
            axis.plan = if stall {
                plan.stalled(STALL_FRACTION)
            } else {
                plan
            };
            axis.activity = if stall {
                Activity::Stalled
            } else {
                Activity::Goto { finish, parking }
            };
        }
        finish
    }

    /// The body behind both [`MountDevice::goto`] and [`MountDevice::park`].
    async fn run_goto(&self, target: RaDec, parking: bool) -> Result<(), DeviceError> {
        let command = if parking {
            MountCommand::Park
        } else {
            MountCommand::Goto
        };
        // Reserved before the first await. `Busy` is a decision the driver makes from its own
        // state, so it must not depend on how long the wire takes — otherwise two gotos issued a
        // millisecond apart would both pass the check and the second would silently retarget the
        // first, which the HAL forbids in as many words.
        let generation = {
            let mut state = self.locked();
            let now = Instant::now();
            if parking && state.parked_at(now) {
                return Ok(()); // parking a parked mount is not an error
            }
            let reserved = state.reserve(now, &[Axis::Ra, Axis::Dec], true, command.as_str())?;
            if parking {
                // A park stops tracking before it moves, and stays stopped: `MountStatus`
                // requires a parked mount to report no tracking mode at all.
                state.tracking = None;
            }
            reserved
        };

        let landed = match self.exchange(command, exchanges::GOTO).await {
            Ok(landed) => landed,
            Err(err) => {
                let mut state = self.locked();
                let rate = state.tracking_rate();
                state.release(Instant::now(), generation, rate);
                return Err(err);
            }
        };

        let (finish, stalled) = {
            let mut state = self.locked();
            if state.generation != generation {
                return Err(overridden(command, state.overridden_by));
            }
            let finish = self.start_goto(&mut state, landed, target, parking);
            let stalled = matches!(
                state.axes().map(|axes| axes.ra.activity),
                Ok(Activity::Stalled)
            );
            (finish, stalled)
        };

        // The whole slew happens here, and nothing waits on this future for it to happen: the
        // plans installed above run to completion, settle, and resume tracking whether or not
        // anybody is still holding this future (HAL rule 3).
        tokio::time::sleep_until(finish).await;

        let mut state = self.locked();
        if state.generation != generation {
            return Err(overridden(command, state.overridden_by));
        }
        if stalled {
            // A goto that fails mid-motion must leave the mount stopped, not drifting toward a
            // target nobody is tracking (SDD §5.1). The axes are already stopped; what this
            // clears is the ownership, so the operator's retry is not answered `Busy`.
            let now = Instant::now();
            if let Ok(axes) = state.axes() {
                axes.ra.halt(now, 0.0);
                axes.dec.halt(now, 0.0);
            }
            state.tracking = None;
            return Err(DeviceError::Timeout(
                finish.saturating_duration_since(landed),
            ));
        }
        Ok(())
    }

    /// Stops both axes dead and forgets tracking — the shared body of `emergency_stop`.
    ///
    /// Tracking stops too because on a Synta mount tracking *is* a slew: `L` on axis 1 stops the
    /// sidereal drive along with everything else. A simulator that kept tracking through an
    /// e-stop would be modelling a mount that does not exist.
    fn halt_everything(&self, now: Instant, reason: &'static str) {
        let mut state = self.locked();
        state.tracking = None;
        state.generation += 1;
        state.overridden_by = reason;
        if let Ok(axes) = state.axes() {
            axes.ra.halt(now, 0.0);
            axes.dec.halt(now, 0.0);
        }
    }
}

/// The error an in-flight goto returns when something else took the axes.
///
/// [`DeviceError::Aborted`] exists because of this call site: M1-T02 had to return `Rejected`
/// here and said so in the crate docs, and `Rejected` maps to `DEVICE_REJECTED`/422 — telling
/// the operator their *request* was malformed at the moment their emergency stop worked.
/// M1-T03 added the variant and it maps to `ABORTED`/409 instead.
///
/// It remains true that callers should not switch on the variant to decide what the mount is
/// doing: `Aborted` says the slew did not finish, not where the tube ended up. Read `status()`
/// for that.
fn overridden(command: MountCommand, by: &'static str) -> DeviceError {
    DeviceError::Aborted(format!("{} aborted by {by}", command.as_str()))
}

/// Folds a difference in degrees into `[-180, 180)` so the mount takes the short way round.
fn wrap_degrees(degrees: f64) -> f64 {
    let wrapped = (degrees + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped >= 180.0 {
        -180.0
    } else {
        wrapped
    }
}

/// Whether a direction is meaningful on an axis.
///
/// `slew(Axis::Ra, Direction::North)` is a caller bug, and the mount is the last place that can
/// still catch it — one layer up it becomes an axis moving the wrong way, which looks like a
/// wiring fault.
fn direction_sign(axis: Axis, dir: Direction) -> Result<f64, DeviceError> {
    match (axis, dir) {
        // East is increasing RA, and RA = LST − hour angle, so east is a *decreasing* hour
        // angle. Getting this backwards sends the mount the wrong way at 800× sidereal.
        (Axis::Ra, Direction::East) => Ok(-1.0),
        (Axis::Ra, Direction::West) => Ok(1.0),
        (Axis::Dec, Direction::North) => Ok(1.0),
        (Axis::Dec, Direction::South) => Ok(-1.0),
        _ => Err(DeviceError::Rejected(format!(
            "{dir:?} is not a direction on the {axis:?} axis"
        ))),
    }
}

#[async_trait]
impl MountDevice for SimulatorMount {
    async fn connect(&self) -> Result<(), DeviceError> {
        {
            let state = self.locked();
            if matches!(state.link, Link::Up { .. }) {
                // Idempotent, and free: the operator's Connect button on a UI showing stale
                // state must not cost a second handshake (HAL rule 7).
                return Ok(());
            }
        }
        // Not `exchange`: the link is down by definition, so the connected check would refuse
        // the one command whose job is to fix that.
        let permit = self
            .line
            .acquire()
            .await
            .expect("the simulated serial line is never closed");
        let injected = self.locked().faults.next_exchange(MountCommand::Connect);
        let result = match injected {
            Some(Injected::Timeout) => {
                tokio::time::sleep(self.timeout_budget).await;
                Err(DeviceError::Timeout(self.request_timeout))
            }
            Some(Injected::Garbled) => {
                tokio::time::sleep(self.profile.round_trip).await;
                Err(garbled_error(MountCommand::Connect))
            }
            None => {
                tokio::time::sleep(self.profile.round_trip * exchanges::CONNECT).await;
                Ok(())
            }
        };
        drop(permit);
        result?;

        let now = Instant::now();
        let mut state = self.locked();
        state.faults.on_connect();
        let drops_at = state
            .faults
            .take_disconnect_delay()
            .map(|after| now + after);
        state.link = Link::Up { drops_at };
        if state.axes.is_none() {
            // A mount at power-on reads its counters at home (`0x800000` on both axes,
            // measured). The simulator's equivalent is "wherever the park position is", with
            // sidereal time anchored so that is true — see the module docs on why LST is
            // relative here. It is *not* flagged parked: the Synta protocol has no park state,
            // so demanding an unpark the hardware does not demand would be inventing a state
            // machine that M3 then has to delete.
            state.axes = Some(Axes {
                epoch: now,
                lst0_degrees: self.park.ra.hours() * 15.0,
                ra: AxisState::new(0.0, now),
                dec: AxisState::new(self.park.dec.degrees(), now),
            });
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        {
            let state = self.locked();
            if matches!(state.link, Link::Down) {
                return Ok(());
            }
        }
        tokio::time::sleep(self.profile.round_trip * exchanges::DISCONNECT).await;
        // The port is released before the fault is reported, and the axes are left alone. Both
        // are the HAL's requirements verbatim: disconnect must succeed from a faulted link, and
        // it must not stop the mount — a service restart mid-session leaves a tracking mount
        // tracking, because a stopped one has lost the target.
        let mut state = self.locked();
        state.link = Link::Down;
        match state.faults.next_exchange(MountCommand::Disconnect) {
            Some(_) => Err(DeviceError::Transport(
                "the port was released but the mount did not acknowledge (simulated)".to_owned(),
            )),
            None => Ok(()),
        }
    }

    async fn position(&self) -> Result<RaDec, DeviceError> {
        let now = self
            .exchange(MountCommand::Position, exchanges::POSITION)
            .await?;
        let mut state = self.locked();
        let axes = state.axes()?;
        // Reborrowed immutably: `sky_position` reads both axes and the clock together, so the
        // hour angle and the sidereal time it is subtracted from are from the same instant.
        let axes: &Axes = axes;
        self.sky_position(axes, now)
    }

    async fn status(&self) -> Result<MountStatus, DeviceError> {
        {
            // A mount that has lost its link still has a status, and it is `Fault`. Answering
            // from cache without an exchange is what a real driver does here too — it has just
            // failed to reach the device and has nothing new to ask.
            let state = self.locked();
            if state.link == Link::Faulted {
                return Ok(state.status_at(Instant::now()));
            }
        }
        let now = self
            .exchange(MountCommand::Status, exchanges::STATUS)
            .await?;
        let status = self.locked().status_at(now);
        debug_assert!(
            status.is_consistent(),
            "the simulator published a self-contradictory status: {status:?}"
        );
        Ok(status)
    }

    async fn goto(&self, target: RaDec) -> Result<(), DeviceError> {
        self.run_goto(target, false).await
    }

    async fn sync(&self, pos: RaDec) -> Result<(), DeviceError> {
        {
            let mut state = self.locked();
            let now = Instant::now();
            state.connected()?;
            let axes = state.axes()?;
            if axes.ra.owned(now) || axes.dec.owned(now) {
                return Err(DeviceError::Busy("cannot sync while the mount is moving"));
            }
        }
        let now = self.exchange(MountCommand::Sync, exchanges::SYNC).await?;
        let mut state = self.locked();
        let tracking_rate = state.tracking_rate();
        let axes = state.axes()?;
        // Both counters in one locked section: a half-applied sync is a pointing model that is
        // wrong in a way nothing downstream can detect (SDD §5.1).
        let hour_angle = axes.lst_degrees(now) - pos.ra.hours() * 15.0;
        axes.ra = AxisState::new(hour_angle, now);
        axes.ra.plan = AxisPlan::steady(hour_angle, tracking_rate);
        axes.dec = AxisState::new(pos.dec.degrees(), now);
        Ok(())
    }

    async fn start_tracking(&self, mode: TrackingMode) -> Result<(), DeviceError> {
        self.locked().connected()?;
        let now = self
            .exchange(MountCommand::StartTracking, exchanges::TRACKING)
            .await?;
        let mut state = self.locked();
        if state.parked_at(now) {
            return Err(DeviceError::Rejected(
                "the mount is parked; unpark before tracking".to_owned(),
            ));
        }
        state.tracking = Some(mode);
        let rate = tracking_rate(mode);
        // Reaches a goto in flight too, changing the rate it will resume at rather than
        // disturbing the slew — an operator switching to lunar mid-slew means the goto should
        // end in lunar, not that the slew should stop.
        state.axes()?.ra.set_terminal_rate(now, rate);
        Ok(())
    }

    async fn stop_tracking(&self) -> Result<(), DeviceError> {
        self.locked().connected()?;
        let now = self
            .exchange(MountCommand::StopTracking, exchanges::STOP)
            .await?;
        let mut state = self.locked();
        state.tracking = None;
        state.axes()?.ra.set_terminal_rate(now, 0.0);
        Ok(())
    }

    async fn slew(&self, axis: Axis, dir: Direction, speed: SlewSpeed) -> Result<(), DeviceError> {
        let sign = direction_sign(axis, dir)?;
        let generation = {
            let mut state = self.locked();
            let now = Instant::now();
            state.connected()?;
            let current = state.axes()?.axis(axis);
            if current.activity == (Activity::Manual { dir, speed }) {
                // A no-op at the device, exactly as the HAL requires: the dead-man's switch above
                // renews this at 2 Hz, and re-issuing the motor command on every renewal would
                // restart the ramp and make the mount stutter under the operator's thumb. It
                // costs no exchange either, which is the point — the renewal must be cheap.
                return Ok(());
            }
            if matches!(current.activity, Activity::Manual { .. }) {
                // A *different* direction or speed on an axis the operator is already driving is
                // a speed change, not a conflict: they moved the slider without letting go of the
                // D-pad. Releasing the axis here lets the reservation below take it, and the new
                // plan ramps from whatever it is doing now rather than from a standstill.
                current.activity = Activity::Steady;
            }
            state.reserve(now, &[axis], true, "a manual slew")?
        };

        let landed = match self.exchange(MountCommand::Slew, exchanges::SLEW).await {
            Ok(landed) => landed,
            Err(err) => {
                let mut state = self.locked();
                let rate = state.tracking_rate();
                state.release(Instant::now(), generation, rate);
                return Err(err);
            }
        };

        let mut state = self.locked();
        if state.generation != generation {
            return Err(overridden(MountCommand::Slew, state.overridden_by));
        }
        let accel = self.profile.accel_deg_per_sec2;
        let target = state.axes()?.axis(axis);
        let start = target.position(landed);
        // From whatever the axis is doing now, which is the sidereal rate if it was tracking and
        // the old slew rate if the operator just moved the speed slider. Ramping from zero would
        // make the mount stop dead and start again on both.
        let from = target.velocity(landed);
        target.started = landed;
        // Ramps to the class rate and then holds it forever: a manual slew ends only when
        // something stops it, which is the shape the whole TTL design above the HAL assumes.
        target.plan = AxisPlan::accelerating(start, from, sign * slew_rate(speed), accel);
        target.activity = Activity::Manual { dir, speed };
        Ok(())
    }

    async fn stop_slew(&self, axis: Axis) -> Result<(), DeviceError> {
        self.locked().connected()?;
        let now = self
            .exchange(MountCommand::StopSlew, exchanges::STOP)
            .await?;
        let mut state = self.locked();
        // Bumped whether or not anything was moving: a goto being stopped has to learn about it,
        // and "stop an axis that is not moving" is explicitly legal, so there is nothing to
        // condition on that would not also be a race.
        state.generation += 1;
        state.overridden_by = "a stop";
        let tracking_rate = state.tracking_rate();
        let decel = self.profile.decel_deg_per_sec2;
        let rate = if axis == Axis::Ra { tracking_rate } else { 0.0 };
        // Ramped, not instant — Synta `K`, not `L`. At 800× sidereal the difference is a degree
        // of extra travel, which is why `emergency_stop` is a separate call rather than this one
        // with a flag.
        state.axes()?.axis(axis).ramp_to_stop(now, decel, rate);
        Ok(())
    }

    async fn guide_pulse(
        &self,
        axis: Axis,
        dir: Direction,
        duration_ms: u32,
        rate: GuideRate,
    ) -> Result<(), DeviceError> {
        let sign = direction_sign(axis, dir)?;
        let generation = {
            let mut state = self.locked();
            state.reserve(Instant::now(), &[axis], false, "a guide pulse")?
        };

        let landed = match self
            .exchange(MountCommand::GuidePulse, exchanges::GUIDE)
            .await
        {
            Ok(landed) => landed,
            Err(err) => {
                let mut state = self.locked();
                let tracking = state.tracking_rate();
                state.release(Instant::now(), generation, tracking);
                return Err(err);
            }
        };

        let pulse = Duration::from_millis(u64::from(duration_ms));
        let finish = landed + pulse;
        {
            let mut state = self.locked();
            if state.generation != generation {
                return Err(overridden(MountCommand::GuidePulse, state.overridden_by));
            }
            let base = if axis == Axis::Ra {
                state.tracking_rate()
            } else {
                0.0
            };
            let target = state.axes()?.axis(axis);
            let start = target.position(landed);
            target.started = landed;
            // A guide pulse is a rate *offset*, not a slew: the axis keeps tracking underneath
            // and the correction rides on top, which is what makes a pulse while tracking move
            // the star by rate × duration rather than by the whole slew rate.
            target.plan = AxisPlan::pulsed(
                start,
                base + sign * rate.fraction() * SIDEREAL_DEG_PER_SEC,
                pulse.as_secs_f64(),
                base,
            );
            target.activity = Activity::Guiding { finish };
        }
        // Resolves when the pulse has been issued *and* completed, so a guide loop can pace
        // itself on it (SDD §5.1). The plan finishes on its own if this future is dropped.
        tokio::time::sleep_until(finish).await;
        Ok(())
    }

    async fn park(&self) -> Result<(), DeviceError> {
        self.run_goto(self.park, true).await
    }

    async fn unpark(&self) -> Result<(), DeviceError> {
        {
            let mut state = self.locked();
            let now = Instant::now();
            state.connected()?;
            let axes = state.axes()?;
            if axes.ra.owned(now) || axes.dec.owned(now) {
                return Err(DeviceError::Busy("the mount is still parking"));
            }
        }
        let now = self
            .exchange(MountCommand::Unpark, exchanges::UNPARK)
            .await?;
        let mut state = self.locked();
        state.parked = false;
        // The park flag is *derived* from a finished park goto so that a dropped `park()` future
        // still leaves the mount parked; clearing it therefore means clearing that too, or the
        // mount would report itself parked again on the next status read.
        if let Ok(axes) = state.axes() {
            axes.ra.halt(now, 0.0);
            axes.dec.halt(now, 0.0);
        }
        Ok(())
    }

    async fn emergency_stop(&self) -> Result<(), DeviceError> {
        // No `self.line` permit and no `Busy`: SDD §5.2.4 gives the e-stop its own lane to the
        // transport precisely so it cannot queue behind an in-flight request, and SDD §5.8.2
        // budgets the whole path at 20 ms. A simulator that made the e-stop wait for a stalled
        // poll to time out would let a driver with no priority lane pass its own latency test.
        tokio::time::sleep(self.profile.round_trip).await;
        // Applied *after* the round trip, not before: the bytes take 16 ms to reach the mount,
        // and the spike measured exactly that as the stop overshoot (84 counts at 5,350 counts/s
        // is 15.7 ms of travel — command latency, not deceleration). Stopping the model before
        // the wait would erase a real, measured effect that gets much larger at slew speed.
        self.halt_everything(Instant::now(), "an emergency stop");
        Ok(())
    }

    fn capabilities(&self) -> MountCapabilities {
        MountCapabilities {
            // The mount can *exhibit* periodic error (see `SimulatorProfile`); it cannot correct
            // it. MNT-13 is a Phase 4 "could", and a capability reported true must work.
            has_pec: false,
            has_pulse_guide: true,
            tracking_rates: vec![
                TrackingMode::Sidereal,
                TrackingMode::Lunar,
                TrackingMode::Solar,
            ],
            // PRD §4.2's stated maximum. The measured goto cruise is 835×, but this field is what
            // the UI offers the operator as a manual-slew class, and that ladder tops out at 800.
            max_slew_speed_x_sidereal: 800,
            // 9,024,000 counts per revolution fits in 24 bits, and the Synta counters are 24-bit
            // (home is `0x800000`). Measured.
            position_resolution_bits: 24,
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "AstroCtl Simulator Mount".to_owned(),
            model: "HEQ5-class simulator".to_owned(),
            // Learned at connect, so `None` until then — the same shape a real driver has, and
            // the reason the field is an `Option` at all.
            firmware: matches!(self.locked().link, Link::Up { .. })
                .then(|| "2.04.01 (simulated)".to_owned()),
            serial: None,
            protocol: "simulator".to_owned(),
        }
    }
}

// -----------------------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------------------

/// Builds [`SimulatorMount`]s for the driver registry under the name `"simulator"` (HAL-07).
///
/// The profile and the fault plan live on the *factory* rather than in [`MountConfig`], because
/// the config is the operator's YAML and neither belongs there — an operator has no reason to
/// script a timeout, and a test has no way to put one in a config struct that
/// `deny_unknown_fields` would reject. A binary that wants a faulty simulator registers a
/// factory carrying the fault plan; a test that wants one builds the mount directly.
#[derive(Debug, Clone, Default)]
pub struct SimulatorMountFactory {
    profile: SimulatorProfile,
    faults: FaultPlan,
}

impl SimulatorMountFactory {
    /// A factory building mounts with the measured HEQ5 timing and no faults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A factory building mounts on `profile`.
    #[must_use]
    pub fn with_profile(mut self, profile: SimulatorProfile) -> Self {
        self.profile = profile;
        self
    }

    /// A factory building mounts that fail as `faults` says (SDD §9).
    #[must_use]
    pub fn with_faults(mut self, faults: FaultPlan) -> Self {
        self.faults = faults;
        self
    }
}

#[async_trait]
impl MountFactory for SimulatorMountFactory {
    fn name(&self) -> &'static str {
        // Must equal `MountDriver::Simulator.as_str()`, or the driver is unreachable from
        // configuration. Asserted by test below rather than trusted.
        "simulator"
    }

    fn create(&self, config: &MountConfig) -> Result<Arc<dyn MountDevice>, DriverInitError> {
        Ok(Arc::new(SimulatorMount::new(
            config,
            self.profile,
            &self.faults,
        )?))
    }

    async fn probe(&self) -> Result<Vec<DetectedDevice>, DeviceError> {
        // The simulator is the one driver that can always report itself (HAL-08). `auto` rather
        // than a made-up device node because `address` is what the operator pastes into
        // `mount.port`, and the config validator accepts only `auto` or an absolute path — a
        // pretty label there would fail validation the moment somebody followed the advice.
        Ok(vec![DetectedDevice::new(
            "simulator",
            DeviceKind::Mount,
            "auto",
            "AstroCtl Simulator Mount (no hardware required)",
        )])
    }
}

/// The configuration a bare `SimulatorMount::with_profile` uses.
///
/// Mirrors `config/field-node.example.yaml`'s mount section. It exists so a test that does not
/// care about configuration does not have to build one, and it is deliberately *not* public: a
/// caller that wants specific settle or timeout behaviour should say so in a `MountConfig`,
/// which is what the real driver reads too.
fn default_config() -> MountConfig {
    MountConfig {
        driver: MountDriver::Simulator,
        port: "auto".to_owned(),
        baud: 9600,
        park_position: ParkPosition {
            ra_hours: 0.0,
            dec_degrees: 90.0,
        },
        settle_time_seconds: 3,
        serial: SerialConfig {
            request_timeout_ms: 500,
            request_retries: 1,
            heartbeat_misses: 3,
            poll_hz: 1,
        },
        limits: MountLimits {
            min_altitude_degrees: 15.0,
            meridian_limit_minutes: 5.0,
            slew_ttl_default_ms: 500,
            slew_ttl_max_ms: 2000,
        },
        indi_device: None,
        ascom_host: None,
    }
}

#[cfg(test)]
mod tests {
    use astroctl_hal::registry::DriverRegistry;

    use super::super::fault::Fault;
    use super::*;

    /// Every timing test runs on `#[tokio::test(start_paused = true)]`: tokio's clock then jumps
    /// straight to the next deadline whenever the runtime has nothing to do, so a 90-second slew
    /// is tested in microseconds and — more importantly — the assertions are on *exact* virtual
    /// durations rather than on wall-clock measurements with a tolerance wide enough to hide the
    /// bug. A test asserting "the goto took between 10 and 20 seconds" against a real clock is a
    /// test that fails on a loaded CI box; here it cannot.
    ///
    /// Park is 12h/+45° rather than the pole so that both axes have room to move either way, and
    /// so no test accidentally asserts against a declination clamp.
    fn config() -> MountConfig {
        MountConfig {
            park_position: ParkPosition {
                ra_hours: 12.0,
                dec_degrees: 45.0,
            },
            settle_time_seconds: 3,
            ..default_config()
        }
    }

    fn mount() -> Arc<SimulatorMount> {
        Arc::new(
            SimulatorMount::new(&config(), SimulatorProfile::default(), &FaultPlan::none())
                .expect("the default configuration is valid"),
        )
    }

    fn mount_with(faults: FaultPlan) -> Arc<SimulatorMount> {
        Arc::new(
            SimulatorMount::new(&config(), SimulatorProfile::default(), &faults)
                .expect("the default configuration is valid"),
        )
    }

    async fn connected() -> Arc<SimulatorMount> {
        let mount = mount();
        mount.connect().await.expect("connects");
        mount
    }

    /// Separation between two positions in arcseconds — small-angle, which is ample for
    /// assertions about a mount that is either on target or a degree away from it.
    fn separation_arcsec(a: RaDec, b: RaDec) -> f64 {
        let dec_mid = f64::midpoint(a.dec.degrees(), b.dec.degrees()).to_radians();
        let d_ra = (a.ra.hours() - b.ra.hours()) * 15.0 * dec_mid.cos();
        let d_dec = a.dec.degrees() - b.dec.degrees();
        (d_ra * d_ra + d_dec * d_dec).sqrt() * 3600.0
    }

    /// Runs a future and reports how far the (virtual) clock moved.
    async fn timed<F: std::future::Future>(future: F) -> Duration {
        let began = Instant::now();
        let _ = future.await;
        began.elapsed()
    }

    /// Asserts the drive is stopped, over `interval`.
    ///
    /// "Stopped" is not "the position stopped changing", and getting that wrong is the easiest
    /// mistake to make about an equatorial mount: the axes hold an *hour angle*, the sky turns
    /// underneath them, and a mount with its motors off therefore reports its right ascension
    /// climbing at exactly the sidereal rate. Asserting that the reported position froze would be
    /// asserting that the mount is still tracking — the opposite of what these tests mean.
    ///
    /// So: declination fixed, and right ascension advancing at 15.041″/s and no faster.
    async fn assert_drive_stopped(mount: &SimulatorMount, interval: Duration) {
        let before = mount.position().await.expect("position");
        let began = Instant::now();
        tokio::time::sleep(interval).await;
        let after = mount.position().await.expect("position");
        let elapsed = began.elapsed().as_secs_f64();

        let dec_moved = (after.dec.degrees() - before.dec.degrees()).abs() * 3600.0;
        assert!(
            dec_moved < 0.5,
            "the declination axis moved {dec_moved} arcsec with the drive stopped"
        );
        let ra_moved = (after.ra.hours() - before.ra.hours()) * 15.0 * 3600.0;
        let sidereal = elapsed * 15.041_07;
        assert!(
            (ra_moved - sidereal).abs() < sidereal * 0.005,
            "right ascension advanced {ra_moved} arcsec in {elapsed} s; a stopped mount's \
             advances {sidereal} (anything else means the drive is still running)"
        );
    }

    // --- acceptance: goto duration -------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_thirty_degree_goto_takes_as_long_as_a_telescope_would() {
        // Acceptance criterion 1, asserted as bounds rather than a number: the ramp constants are
        // fitted and could legitimately be re-fitted, but "somewhere between ten seconds and half
        // a minute" is what makes the M3 swap boring, and is what a caller must not be allowed to
        // assume away.
        let mount = connected().await;
        // Tracking on, which is the operating mode a goto happens in: with the drive off the
        // mount arrives correctly and then the sky slides past it, so "did it land" would be a
        // question about how long the assertion took rather than about the slew.
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let start = mount.position().await.expect("reads position");
        let target = RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 30.0)
            .expect("30 degrees south of the park position is still sky");

        let began = Instant::now();
        mount.goto(target).await.expect("arrives");
        let elapsed = began.elapsed();

        assert!(
            elapsed >= Duration::from_secs(10),
            "a 30 degree goto took {elapsed:?}; an HEQ5 needs ~11 s of motion at 800x sidereal"
        );
        assert!(
            elapsed <= Duration::from_secs(30),
            "a 30 degree goto took {elapsed:?}, which is slower than the mount"
        );
        // ...and it landed. The tolerance is a fraction of the SDD's 10-count goto tolerance.
        let landed = mount.position().await.expect("reads position");
        assert!(
            separation_arcsec(landed, target) < 0.5,
            "landed {} arcsec from the target",
            separation_arcsec(landed, target)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn goto_duration_grows_sub_linearly_with_distance() {
        // The spike's headline non-obvious result: 100x the distance took 6.4x the time, because
        // ramps dominate short moves. A caller estimating slew time as distance / rate is wrong
        // in both directions, and this is the property that says so.
        async fn goto_seconds(degrees: f64) -> f64 {
            let mount = connected().await;
            let start = mount.position().await.expect("position");
            let target = RaDec::from_parts(start.ra.hours(), start.dec.degrees() - degrees)
                .expect("valid target");
            let began = Instant::now();
            mount.goto(target).await.expect("arrives");
            began.elapsed().as_secs_f64()
        }

        let short = goto_seconds(0.5).await;
        let long = goto_seconds(50.0).await;
        let time_ratio = long / short;
        assert!(
            time_ratio < 100.0 / 3.0,
            "100x the distance took {time_ratio}x the time, which is nearly linear"
        );
        assert!(time_ratio > 1.5, "distance barely mattered: {time_ratio}x");
    }

    // --- acceptance: position is continuous ----------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn position_is_monotonic_and_continuous_sampled_at_ten_hertz() {
        // Acceptance criterion 2, through the trait rather than through the kinematics: this is
        // what the position poll above the HAL actually sees. Declination is the axis under test
        // because it has no sidereal term, so "monotonic" means exactly one thing.
        let mount = connected().await;
        let start = mount.position().await.expect("position");
        let target =
            RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 20.0).expect("valid target");

        let slewing = Arc::clone(&mount);
        let goto = tokio::spawn(async move { slewing.goto(target).await });

        let mut samples = Vec::new();
        while !goto.is_finished() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            samples.push(mount.position().await.expect("position").dec.degrees());
        }
        goto.await.expect("goto task").expect("arrives");

        assert!(samples.len() > 30, "only {} samples", samples.len());
        // The goto does not resolve until the tube has stopped ringing, so the tail of this
        // sample run is the settle — which oscillates by design and is *not* monotonic. Split at
        // the point the axis arrives: monotonic before, inside the settle band after.
        let target_dec = target.dec.degrees();
        let band = 30.0 / 3600.0;
        let arrived = samples
            .iter()
            .position(|dec| (dec - target_dec).abs() <= band)
            .expect("the mount never arrived");

        let mut largest = 0.0_f64;
        for pair in samples[..=arrived].windows(2) {
            assert!(
                pair[1] <= pair[0] + 1e-9,
                "declination went backwards during the slew: {} then {}",
                pair[0],
                pair[1]
            );
            largest = largest.max(pair[0] - pair[1]);
        }
        // At the 3.49 deg/s cruise rate, ~130 ms between samples is at most ~0.46 deg. Anything
        // materially larger is the mount teleporting between two polls.
        assert!(
            largest < 0.6,
            "largest step between samples was {largest} deg"
        );
        assert!(arrived > 20, "the slew was over in {arrived} samples");
        // It really did move, rather than being continuous by standing still.
        assert!(samples[0] - samples[arrived] > 15.0);
        // ...and once it has arrived it stays arrived: the ring-down never leaves the band.
        for (i, dec) in samples.iter().enumerate().skip(arrived) {
            assert!(
                (dec - target_dec).abs() <= band,
                "sample {i} left the settle band at {dec}"
            );
        }
    }

    // --- acceptance: the fault plan -------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_scripted_timeout_fires_once_and_the_retry_succeeds() {
        let mount = mount_with(FaultPlan::new([Fault::TimeoutOnce(MountCommand::Position)]));
        mount.connect().await.expect("connects");

        let began = Instant::now();
        let err = mount
            .position()
            .await
            .expect_err("the mount does not answer");
        assert!(matches!(err, DeviceError::Timeout(_)), "{err:?}");
        // The caller waited out the configured budget: 500 ms for the request and 500 ms for the
        // single retry. A timeout that returned instantly would let a caller poll a dead mount in
        // a tight loop and never notice.
        assert_eq!(began.elapsed(), Duration::from_millis(1000));

        mount.position().await.expect("the retry gets through");
    }

    #[tokio::test(start_paused = true)]
    async fn a_garbled_reply_is_a_protocol_error_and_is_not_sticky() {
        // Exchange 3 lands inside the second `position()` call, which costs two.
        let mount = mount_with(FaultPlan::new([Fault::GarbledResponse(3)]));
        mount.connect().await.expect("connects");
        mount.position().await.expect("exchanges 1 and 2 are clean");

        let err = mount.position().await.expect_err("exchange 3 is corrupt");
        assert!(matches!(err, DeviceError::Protocol(_)), "{err:?}");
        assert!(
            err.to_string().contains("read position"),
            "the message must name what failed: {err}"
        );
        mount.position().await.expect("exchange 4 is clean again");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unplugged_adapter_faults_the_link_without_stopping_the_mount() {
        let mount = mount_with(FaultPlan::new([Fault::DisconnectAfter(
            Duration::from_secs(5),
        )]));
        mount.connect().await.expect("connects");
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");

        tokio::time::sleep(Duration::from_secs(6)).await;
        let err = mount.position().await.expect_err("the link is gone");
        assert!(matches!(err, DeviceError::Transport(_)), "{err:?}");

        // The status still answers, and it says `Fault` rather than going blank — "the mount is
        // not answering" is a state the operator must see (HAL, `status`).
        let status = mount.status().await.expect("status still answers");
        assert_eq!(status.state, MountState::Fault);
        assert!(status.is_consistent());

        // And the mount is still tracking, because pulling a cable does not stop a telescope.
        // This is the scenario REL-02's watchdog and the safety layer exist for; a simulator that
        // quietly stopped the motion here would let both ship untested.
        assert_eq!(status.tracking, Some(TrackingMode::Sidereal));

        // Recovery: reconnecting works and the fault is spent.
        mount.connect().await.expect("reconnects");
        mount.position().await.expect("and answers again");
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Tracking
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stall_mid_slew_never_arrives_and_leaves_the_mount_stopped() {
        let mount = mount_with(FaultPlan::new([Fault::StallDuringSlew]));
        mount.connect().await.expect("connects");
        let start = mount.position().await.expect("position");
        let target =
            RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 30.0).expect("valid target");

        let err = mount.goto(target).await.expect_err("the axis jams");
        // A timeout on a command the mount *acknowledged* — a different code path above the HAL
        // from a link that never answered at all, and the reason this fault exists separately.
        assert!(matches!(err, DeviceError::Timeout(_)), "{err:?}");

        let stuck = mount.position().await.expect("position");
        assert!(
            separation_arcsec(stuck, target) > 3600.0,
            "the mount arrived after all"
        );
        assert!(
            separation_arcsec(stuck, start) > 3600.0,
            "the mount never moved, so nothing stalled"
        );

        // Stopped, not drifting toward a target nobody is tracking (SDD §5.1).
        assert_drive_stopped(&mount, Duration::from_secs(60)).await;

        // Recovery: the fault is spent, so the operator's retry works.
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        mount.goto(target).await.expect("the retry arrives");
        let landed = mount.position().await.expect("position");
        assert!(separation_arcsec(landed, target) < 1.0);
    }

    // --- acceptance: the registry ---------------------------------------------------------

    #[test]
    fn the_factory_is_registrable_under_the_name_the_config_uses() {
        // Acceptance criterion 4, and EXT-01's actual claim: adding a driver is one crate and one
        // registration with nothing above the HAL changed. If `name()` and
        // `MountDriver::as_str()` ever disagree the driver is simply unreachable from YAML, and
        // the operator's error is "no driver named simulator" on a build that has one.
        let mut registry = DriverRegistry::new();
        registry
            .register_mount(SimulatorMountFactory::new())
            .expect("registers");
        assert_eq!(registry.mount_drivers(), ["simulator"]);
        assert_eq!(
            SimulatorMountFactory::new().name(),
            MountDriver::Simulator.as_str()
        );

        let config = config();
        let mount = registry
            .create_mount(config.driver.as_str(), &config)
            .expect("builds");
        assert_eq!(mount.device_info().protocol, "simulator");
        assert!(mount.capabilities().has_pulse_guide);
    }

    #[tokio::test]
    async fn the_factory_reports_itself_and_its_address_is_pasteable() {
        // HAL-08. The address has to be a value `mount.port` actually accepts, or a setup UI that
        // offers it produces a config the validator then rejects.
        let found = SimulatorMountFactory::new().probe().await.expect("probes");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].address, "auto");
        assert_eq!(found[0].kind, DeviceKind::Mount);
    }

    #[test]
    fn a_factory_can_carry_a_fault_plan_into_the_registry() {
        // `MountConfig` has nowhere to put a fault plan and should not grow one, so the factory is
        // how a scripted failure reaches a mount built the way the binary builds it — which is
        // what an end-to-end failure test (T-E2E-1) needs.
        let factory = SimulatorMountFactory::new()
            .with_profile(SimulatorProfile::instant())
            .with_faults(FaultPlan::new([Fault::TimeoutOnce(MountCommand::Connect)]));
        let mount = factory.create(&config()).expect("builds");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime");
        let err = runtime
            .block_on(mount.connect())
            .expect_err("the scripted timeout applies to a registry-built mount too");
        assert!(matches!(err, DeviceError::Timeout(_)), "{err:?}");
    }

    // --- the safety path -------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn an_emergency_stop_mid_slew_stops_at_the_instant_it_is_issued() {
        // The property M1-T05 measures against. Not "stops soon" — stops *here*, with no tick
        // quantisation, because a tick interval in the simulator would become a floor under the
        // e-stop latency the safety layer reports.
        let mount = connected().await;
        let start = mount.position().await.expect("position");
        let target =
            RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 40.0).expect("valid target");

        let slewing = Arc::clone(&mount);
        let goto = tokio::spawn(async move { slewing.goto(target).await });
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(mount.status().await.expect("status").slewing);

        let began = Instant::now();
        mount.emergency_stop().await.expect("stops");
        // One round trip and not a millisecond more: the bytes take 16 ms to reach the mount and
        // then it stops. SDD §5.8.2 budgets 20 ms for the whole path.
        assert_eq!(began.elapsed(), Duration::from_millis(16));

        let stopped = mount.position().await.expect("position");
        assert!(
            separation_arcsec(stopped, target) > 3600.0,
            "the e-stop landed after the slew had already finished"
        );
        // The drive is off and stays off — for two minutes, which at the 3.49 deg/s cruise rate
        // it was doing would be over 400 degrees of travel if anything were still running.
        assert_drive_stopped(&mount, Duration::from_secs(120)).await;

        // The abandoned goto learns that it was overridden, and by what.
        let err = goto.await.expect("task").expect_err("the goto was aborted");
        assert!(
            err.to_string().contains("emergency stop"),
            "the goto must say what stopped it: {err}"
        );
        // Idempotent — the operator will press the button twice.
        mount.emergency_stop().await.expect("stops again");
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Idle
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_emergency_stop_does_not_queue_behind_a_stalled_exchange() {
        // SDD §5.2.4's priority lane, at the layer that has to have one. The line is held for a
        // full second by a command that will never be answered; without a separate lane the
        // e-stop would wait for it, and a driver missing the lane would still pass its own
        // latency test against a simulator that did not model the contention.
        let mount = mount_with(FaultPlan::new([Fault::TimeoutOnce(MountCommand::Position)]));
        mount.connect().await.expect("connects");
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");

        let polling = Arc::clone(&mount);
        let poll = tokio::spawn(async move { polling.position().await });
        tokio::task::yield_now().await;

        let began = Instant::now();
        mount.emergency_stop().await.expect("stops");
        assert_eq!(
            began.elapsed(),
            Duration::from_millis(16),
            "the e-stop waited for the stalled poll"
        );

        assert!(
            poll.await.expect("task").is_err(),
            "the poll still times out"
        );
        // An e-stop stops tracking too: on a Synta mount the sidereal drive *is* a slew, and `L`
        // stops it along with everything else.
        assert_eq!(mount.status().await.expect("status").tracking, None);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_slew_ramps_where_emergency_stop_does_not() {
        // Synta `K` versus `L`. The spike could not tell them apart at 5,350 counts/s; at 800x
        // sidereal the ramp is seconds of extra travel, which is the entire reason the HAL has
        // two calls rather than one with a flag.
        async fn travel_after_stop(emergency: bool) -> f64 {
            let mount = connected().await;
            mount
                .slew(Axis::Dec, Direction::North, SlewSpeed::Max)
                .await
                .expect("slews");
            tokio::time::sleep(Duration::from_secs(10)).await;

            let at_command = mount.position().await.expect("position");
            if emergency {
                mount.emergency_stop().await.expect("stops");
            } else {
                mount.stop_slew(Axis::Dec).await.expect("stops");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
            let finally = mount.position().await.expect("position");
            finally.dec.degrees() - at_command.dec.degrees()
        }

        let ramped = travel_after_stop(false).await;
        let instant = travel_after_stop(true).await;
        assert!(
            ramped > instant + 1.0,
            "a ramped stop travelled {ramped} deg and an instant one {instant} deg"
        );
        // Both stop: neither is still running half a minute later.
        assert!(
            ramped < 10.0,
            "the ramped stop never finished: {ramped} deg"
        );
    }

    // --- cancel-safety (HAL rule 3) --------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn dropping_a_goto_future_leaves_the_mount_slewing_and_it_still_arrives() {
        // The API answers 202 and drops this future on every goto (SDD §5.8.1), so any other
        // behaviour would mean nothing ever slews. The second half matters as much as the first:
        // the mount must also *finish*, settle and resume tracking with nobody watching, which is
        // why the motion is a plan rather than something the future drives.
        let mount = connected().await;
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let start = mount.position().await.expect("position");
        let target = RaDec::from_parts(start.ra.hours() + 1.0, start.dec.degrees() - 10.0)
            .expect("valid target");

        tokio::select! {
            biased;
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = mount.goto(target) => panic!("the simulated slew is slower than this"),
        }
        assert!(
            mount.status().await.expect("status").slewing,
            "a dropped future must not have stopped the slew"
        );

        tokio::time::sleep(Duration::from_secs(60)).await;
        let landed = mount.position().await.expect("position");
        assert!(
            separation_arcsec(landed, target) < 1.0,
            "unattended, the slew ended {} arcsec off target",
            separation_arcsec(landed, target)
        );
        // Tracking was restored by the plan, not by the future nobody was holding (SES-06).
        let status = mount.status().await.expect("status");
        assert_eq!(status.state, MountState::Tracking);
        assert!(!status.slewing);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_goto_is_busy_rather_than_a_silently_retargeted_slew() {
        let mount = connected().await;
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let start = mount.position().await.expect("position");
        let first = RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 30.0).expect("valid");
        let second =
            RaDec::from_parts(start.ra.hours(), start.dec.degrees() + 20.0).expect("valid");

        let slewing = Arc::clone(&mount);
        let goto = tokio::spawn(async move { slewing.goto(first).await });
        tokio::time::sleep(Duration::from_secs(2)).await;

        let err = mount.goto(second).await.expect_err("one slew at a time");
        assert!(matches!(err, DeviceError::Busy(_)), "{err:?}");
        goto.await
            .expect("task")
            .expect("the first goto still arrives");
        let landed = mount.position().await.expect("position");
        assert!(
            separation_arcsec(landed, first) < 1.0,
            "it went somewhere else"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn busy_does_not_depend_on_how_long_the_wire_takes() {
        // The race the reservation exists for: both gotos pass their `Busy` check before either
        // has finished its eight-exchange command sequence. Without reserving the axes up front
        // the second would win and silently retarget the first, which the HAL forbids in as many
        // words — and the failure would be invisible at any latency below the test's timing.
        let mount = connected().await;
        let start = mount.position().await.expect("position");
        let a = RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 30.0).expect("valid");
        let b = RaDec::from_parts(start.ra.hours(), start.dec.degrees() + 20.0).expect("valid");

        let one = Arc::clone(&mount);
        let two = Arc::clone(&mount);
        let (first, second) = tokio::join!(
            tokio::spawn(async move { one.goto(a).await }),
            tokio::spawn(async move { two.goto(b).await })
        );
        let outcomes = [first.expect("task").is_ok(), second.expect("task").is_ok()];
        assert_eq!(
            outcomes.iter().filter(|ok| **ok).count(),
            1,
            "exactly one goto must be accepted, got {outcomes:?}"
        );
    }

    // --- tracking --------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn tracking_holds_right_ascension_and_idling_lets_the_sky_slide_past() {
        // The whole reason the axes are stored as hour angle rather than as RA/DEC. Both halves
        // are the same arithmetic seen from two sides: the hour-angle axis turns at the sidereal
        // rate and RA stands still, or it does not turn and RA climbs at exactly that rate. A
        // simulator storing RA directly would need a special case for each, and would get the
        // lunar rate backwards.
        let mount = connected().await;
        let idle_start = mount.position().await.expect("position");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        let idle_end = mount.position().await.expect("position");

        let drifted_hours = idle_end.ra.hours() - idle_start.ra.hours();
        assert!(
            (drifted_hours - 1.002_74).abs() < 0.001,
            "an idle mount's RA moved {drifted_hours} h in an hour; a sidereal hour is 1.0027 h"
        );
        assert!(
            (idle_end.dec.degrees() - idle_start.dec.degrees()).abs() < 1e-9,
            "declination moved on an idle mount"
        );

        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let tracked_start = mount.position().await.expect("position");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        let tracked_end = mount.position().await.expect("position");
        assert!(
            separation_arcsec(tracked_start, tracked_end) < 0.5,
            "a tracking mount drifted {} arcsec in an hour",
            separation_arcsec(tracked_start, tracked_end)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_lunar_rate_lets_the_stars_slip_west() {
        // Lunar is slower than sidereal, so a mount tracking the Moon sees the star field move.
        // The direction is the assertion: a sign error here is invisible until somebody images
        // the Moon and everything trails the wrong way.
        let mount = connected().await;
        mount
            .start_tracking(TrackingMode::Lunar)
            .await
            .expect("tracks");
        let start = mount.position().await.expect("position");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        let end = mount.position().await.expect("position");

        let slip_arcsec = (end.ra.hours() - start.ra.hours()) * 15.0 * 3600.0;
        // (15.041 − 14.685) arcsec/s over an hour ≈ 1,280 arcsec of RA.
        assert!(
            (slip_arcsec - 1281.0).abs() < 20.0,
            "the lunar rate slipped {slip_arcsec} arcsec in an hour"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_tracking_mid_slew_changes_what_the_goto_resumes_into() {
        // The rate a goto resumes at is baked into the plan when it starts, so that a dropped
        // future still resumes tracking. An operator changing their mind mid-slew has to be able
        // to change that decision, or the mount starts tracking after they said not to.
        let mount = connected().await;
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let start = mount.position().await.expect("position");
        let target =
            RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 30.0).expect("valid");

        let slewing = Arc::clone(&mount);
        let goto = tokio::spawn(async move { slewing.goto(target).await });
        tokio::time::sleep(Duration::from_secs(3)).await;
        mount.stop_tracking().await.expect("stops tracking");
        goto.await.expect("task").expect("the goto still arrives");

        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Idle
        );
        let landed = mount.position().await.expect("position");
        tokio::time::sleep(Duration::from_secs(600)).await;
        let later = mount.position().await.expect("position");
        assert!(
            later.ra.hours() > landed.ra.hours(),
            "the mount kept tracking after being told to stop"
        );
    }

    // --- manual slew ------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_manual_slew_runs_until_it_is_stopped() {
        let mount = connected().await;
        let start = mount.position().await.expect("position");
        mount
            .slew(Axis::Dec, Direction::North, SlewSpeed::Medium)
            .await
            .expect("slews");

        tokio::time::sleep(Duration::from_secs(10)).await;
        let moving = mount.position().await.expect("position");
        assert!(
            moving.dec.degrees() > start.dec.degrees(),
            "north must increase declination"
        );
        assert!(mount.status().await.expect("status").slewing);

        // 64x sidereal is 0.267 deg/s, so ten seconds is ~2.67 degrees, less the ramp-up.
        let travelled = moving.dec.degrees() - start.dec.degrees();
        assert!(
            (2.5..2.8).contains(&travelled),
            "medium speed moved {travelled} deg in 10 s"
        );

        mount.stop_slew(Axis::Dec).await.expect("stops");
        tokio::time::sleep(Duration::from_secs(30)).await;
        let stopped = mount.position().await.expect("position");
        tokio::time::sleep(Duration::from_secs(30)).await;
        let settled = mount.position().await.expect("position");
        assert!((settled.dec.degrees() - stopped.dec.degrees()).abs() < 1e-6);
        assert!(!mount.status().await.expect("status").slewing);
    }

    #[tokio::test(start_paused = true)]
    async fn repeating_a_manual_slew_is_a_no_op_at_the_device() {
        // The dead-man's switch above the HAL renews this every few hundred milliseconds
        // (SDD §5.8.1). Re-issuing the motor command on each renewal would restart the ramp and
        // make the mount stutter under the operator's thumb, so the renewal must cost nothing
        // *and* must not disturb the motion.
        let mount = connected().await;
        mount
            .slew(Axis::Ra, Direction::West, SlewSpeed::Fast)
            .await
            .expect("slews");
        tokio::time::sleep(Duration::from_secs(5)).await;

        let before = mount.position().await.expect("position");
        let began = Instant::now();
        for _ in 0..10 {
            mount
                .slew(Axis::Ra, Direction::West, SlewSpeed::Fast)
                .await
                .expect("renews");
        }
        assert_eq!(
            began.elapsed(),
            Duration::ZERO,
            "a renewal touched the wire"
        );
        let after = mount.position().await.expect("position");
        // West is a decreasing RA, and the renewals did not restart the ramp.
        assert!(after.ra.hours() < before.ra.hours());

        // A *different* speed is a real command, so it costs an exchange.
        let began = Instant::now();
        mount
            .slew(Axis::Ra, Direction::West, SlewSpeed::Slow)
            .await
            .expect("changes speed");
        assert!(began.elapsed() >= Duration::from_millis(16));
    }

    #[tokio::test(start_paused = true)]
    async fn a_direction_that_is_not_on_the_axis_is_refused() {
        // One layer up this becomes an axis moving the wrong way, which looks like a wiring fault
        // and gets debugged as one.
        let mount = connected().await;
        let err = mount
            .slew(Axis::Ra, Direction::North, SlewSpeed::Slow)
            .await
            .expect_err("north is not an RA direction");
        assert!(matches!(err, DeviceError::Rejected(_)), "{err:?}");
        let err = mount
            .guide_pulse(
                Axis::Dec,
                Direction::East,
                100,
                GuideRate::new(0.5).expect("valid"),
            )
            .await
            .expect_err("east is not a declination direction");
        assert!(matches!(err, DeviceError::Rejected(_)), "{err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_manual_slew_is_refused_while_a_goto_owns_the_axis() {
        let mount = connected().await;
        let start = mount.position().await.expect("position");
        let target =
            RaDec::from_parts(start.ra.hours(), start.dec.degrees() - 30.0).expect("valid");
        let slewing = Arc::clone(&mount);
        let goto = tokio::spawn(async move { slewing.goto(target).await });
        tokio::time::sleep(Duration::from_secs(2)).await;

        let err = mount
            .slew(Axis::Dec, Direction::North, SlewSpeed::Slow)
            .await
            .expect_err("the goto owns both axes");
        assert!(matches!(err, DeviceError::Busy(_)), "{err:?}");
        goto.await.expect("task").expect("arrives");
    }

    // --- guiding ----------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_guide_pulse_displaces_the_axis_by_rate_times_duration() {
        // MNT-12. The number matters: a guide loop calibrated against a rate the mount ignored
        // produces corrections wrong by a constant factor, and that looks like poor seeing rather
        // than like a bug.
        let mount = connected().await;
        let before = mount.position().await.expect("position");
        let rate = GuideRate::new(0.5).expect("valid");

        let began = Instant::now();
        mount
            .guide_pulse(Axis::Dec, Direction::North, 1000, rate)
            .await
            .expect("pulses");
        // Resolves when the pulse has been issued *and* completed, so a guide loop can pace
        // itself on it: two exchanges plus the pulse itself.
        assert_eq!(began.elapsed(), Duration::from_millis(32 + 1000));

        let after = mount.position().await.expect("position");
        // 0.5 x sidereal for one second is 7.52 arcsec.
        let moved = (after.dec.degrees() - before.dec.degrees()) * 3600.0;
        assert!(
            (moved - 7.52).abs() < 0.05,
            "the pulse moved {moved} arcsec"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_guide_pulse_rides_on_top_of_tracking_rather_than_replacing_it() {
        // An RA pulse while tracking must move the star by rate x duration, not by the whole slew
        // rate: the correction is an offset to the tracking rate, which is how ST4 guiding
        // actually works.
        let mount = connected().await;
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let before = mount.position().await.expect("position");
        mount
            .guide_pulse(
                Axis::Ra,
                Direction::East,
                2000,
                GuideRate::new(1.0).expect("valid"),
            )
            .await
            .expect("pulses");
        let after = mount.position().await.expect("position");

        // One second at the full sidereal rate is 15.04 arcsec; two seconds is 30.1.
        let moved = (after.ra.hours() - before.ra.hours()) * 15.0 * 3600.0;
        assert!(
            (moved - 30.08).abs() < 0.3,
            "the pulse moved {moved} arcsec of RA"
        );

        // Guiding is not slewing: a mount reporting `slewing` through every guided exposure would
        // make the UI useless for the entire night.
        let status = mount.status().await.expect("status");
        assert_eq!(status.state, MountState::Tracking);
        assert!(!status.slewing);
    }

    // --- park, sync, capabilities ------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn parking_slews_home_stops_tracking_and_refuses_motion_until_unparked() {
        let mount = connected().await;
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let start = mount.position().await.expect("position");
        let elsewhere =
            RaDec::from_parts(start.ra.hours() + 2.0, start.dec.degrees() - 25.0).expect("valid");
        mount.goto(elsewhere).await.expect("arrives");

        let parking = Arc::clone(&mount);
        let park = tokio::spawn(async move { parking.park().await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Parking
        );
        park.await.expect("task").expect("parks");

        let status = mount.status().await.expect("status");
        assert_eq!(status.state, MountState::Parked);
        assert!(status.parked && !status.slewing);
        assert_eq!(status.tracking, None, "a parked mount is not tracking");
        assert!(status.is_consistent());

        let err = mount.goto(elsewhere).await.expect_err("parked");
        assert!(matches!(err, DeviceError::Rejected(_)), "{err:?}");

        mount.unpark().await.expect("unparks");
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Idle
        );
        mount.goto(elsewhere).await.expect("moves again");
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_park_future_still_leaves_the_mount_parked() {
        // The same rule-3 property as `goto`, and the one most likely to be got wrong: it is
        // tempting to set the parked flag when the future completes, which makes parking depend
        // on somebody still listening.
        let mount = connected().await;
        let start = mount.position().await.expect("position");
        let away =
            RaDec::from_parts(start.ra.hours() + 3.0, start.dec.degrees() - 30.0).expect("valid");
        mount.goto(away).await.expect("arrives");

        tokio::select! {
            biased;
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = mount.park() => panic!("parking is slower than this"),
        }
        // Poll for it the way the facade will, rather than sleeping past the arrival: park stops
        // tracking, so from the moment it arrives the sky starts sliding past the reported
        // position and "where did it park" gets vaguer the longer the test waits.
        let mut waited = Duration::ZERO;
        while !mount.status().await.expect("status").parked {
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += Duration::from_millis(132);
            assert!(waited < Duration::from_secs(120), "it never parked");
        }
        let parked_at = mount.position().await.expect("position");
        assert!(
            (parked_at.dec.degrees() - 45.0).abs() < 1.0 / 3600.0,
            "parked at declination {} rather than the configured 45",
            parked_at.dec
        );
        assert!(
            (parked_at.ra.hours() - 12.0).abs() * 3600.0 < 60.0,
            "parked at {parked_at} rather than at the configured park position"
        );
        // The tracking motor is off, which is the half of parking that is not about position.
        assert_drive_stopped(&mount, Duration::from_secs(60)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn sync_moves_the_model_and_not_the_mount() {
        // The plate-solve correction path (PLS-03, MNT-10): it changes where the mount thinks it
        // is, atomically on both axes, without any motion.
        let mount = connected().await;
        let claimed = RaDec::from_parts(3.5, -12.25).expect("valid");
        mount.sync(claimed).await.expect("syncs");

        let now = mount.position().await.expect("position");
        assert!(
            separation_arcsec(now, claimed) < 1.0,
            "sync left the mount at {now}, not {claimed}"
        );
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Idle
        );

        // Syncing mid-slew is meaningless and is refused rather than half-applied.
        let target = RaDec::from_parts(5.0, 10.0).expect("valid");
        let slewing = Arc::clone(&mount);
        let goto = tokio::spawn(async move { slewing.goto(target).await });
        tokio::time::sleep(Duration::from_secs(2)).await;
        let err = mount.sync(claimed).await.expect_err("busy");
        assert!(matches!(err, DeviceError::Busy(_)), "{err:?}");
        goto.await.expect("task").expect("arrives");
    }

    #[tokio::test(start_paused = true)]
    async fn identity_and_capabilities_read_before_connecting_and_sharpen_after() {
        // SDD §8.1: the registry builds drivers at startup and `/api/system/info` reports
        // capabilities long before an operator presses Connect.
        let mount = mount();
        let before = mount.capabilities();
        assert!(before.has_pulse_guide);
        assert!(
            !before.has_pec,
            "the simulator exhibits PE; it cannot correct it"
        );
        assert_eq!(before.max_slew_speed_x_sidereal, 800);
        assert_eq!(before.position_resolution_bits, 24);
        assert!(mount.device_info().firmware.is_none());
        assert!(matches!(
            mount.position().await,
            Err(DeviceError::NotConnected)
        ));

        mount.connect().await.expect("connects");
        assert!(
            mount.device_info().firmware.is_some(),
            "firmware is learned at connect"
        );
        assert_eq!(
            mount.capabilities().tracking_rates.len(),
            before.tracking_rates.len(),
            "the *set* of supported operations may not change at connect"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connect_and_disconnect_are_idempotent_and_disconnect_leaves_it_tracking() {
        // SDD §7 is explicit: a service restart mid-session must leave a tracking mount tracking,
        // because the mount is safe while tracking and a stopped one has lost the target.
        let mount = connected().await;
        mount.connect().await.expect("already connected is Ok");
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let before = mount.position().await.expect("position");

        mount.disconnect().await.expect("disconnects");
        mount
            .disconnect()
            .await
            .expect("already disconnected is Ok");
        assert!(matches!(
            mount.position().await,
            Err(DeviceError::NotConnected)
        ));

        tokio::time::sleep(Duration::from_secs(600)).await;
        mount.connect().await.expect("reconnects");
        // The counters survived, and the mount tracked the whole time it was unattended.
        let after = mount.position().await.expect("position");
        assert!(
            separation_arcsec(before, after) < 1.0,
            "it drifted {} arcsec while disconnected, so it stopped tracking",
            separation_arcsec(before, after)
        );
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Tracking
        );
    }

    // --- latency and the optional error terms --------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn every_command_costs_at_least_one_serial_round_trip() {
        // The property a caller must not be able to assume away. A driver that answers instantly
        // trains the layers above it to poll in a loop, and the cost then shows up on a Pi in a
        // field in the dark rather than in a test.
        let mount = connected().await;
        let claimed = RaDec::from_parts(6.0, 20.0).expect("valid");

        for (label, elapsed) in [
            ("position", timed(mount.position()).await),
            ("status", timed(mount.status()).await),
            (
                "start_tracking",
                timed(mount.start_tracking(TrackingMode::Sidereal)).await,
            ),
            ("stop_tracking", timed(mount.stop_tracking()).await),
            ("sync", timed(mount.sync(claimed)).await),
            ("stop_slew", timed(mount.stop_slew(Axis::Ra)).await),
            ("emergency_stop", timed(mount.emergency_stop()).await),
            ("unpark", timed(mount.unpark()).await),
            ("disconnect", timed(mount.disconnect()).await),
            ("connect", timed(mount.connect()).await),
        ] {
            assert!(
                elapsed >= Duration::from_millis(16),
                "{label} answered in {elapsed:?}, faster than the cable allows"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_error_and_drift_perturb_the_position_when_enabled() {
        let profile = SimulatorProfile {
            periodic_error_arcsec: 15.0,
            polar_drift_arcsec_per_min: 2.0,
            ..SimulatorProfile::default()
        };
        let mount = SimulatorMount::new(&config(), profile, &FaultPlan::none()).expect("builds");
        mount.connect().await.expect("connects");
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");

        // A tracking mount holds RA perfectly, so anything that moves it is an error term.
        let start = mount.position().await.expect("position");
        let mut swing = (f64::MAX, f64::MIN);
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let now = mount.position().await.expect("position");
            let offset = (now.ra.hours() - start.ra.hours()) * 15.0 * 3600.0;
            swing = (swing.0.min(offset), swing.1.max(offset));
        }
        // Periodic error is periodic: it swings both ways rather than accumulating.
        assert!(swing.0 < -5.0, "PE never swung west: {swing:?}");
        assert!(swing.1 > 5.0, "PE never swung east: {swing:?}");
        assert!(swing.1 - swing.0 < 40.0, "PE exceeded its own amplitude");

        // Drift accumulates: twenty minutes at 2 arcsec/min is 40 arcsec of declination.
        let end = mount.position().await.expect("position");
        let drifted = (end.dec.degrees() - start.dec.degrees()) * 3600.0;
        assert!(
            (drifted - 40.0).abs() < 1.0,
            "declination drifted {drifted} arcsec"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_error_terms_are_off_by_default() {
        // Every pointing assertion elsewhere in the workspace depends on this.
        let mount = connected().await;
        mount
            .start_tracking(TrackingMode::Sidereal)
            .await
            .expect("tracks");
        let start = mount.position().await.expect("position");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        let end = mount.position().await.expect("position");
        assert!(separation_arcsec(start, end) < 0.5);
    }

    // --- concurrency ------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn one_mount_serves_the_poll_a_command_and_a_watchdog_at_once() {
        // The arrangement SDD §7 describes and the reason every trait method takes `&self`.
        let mount = connected().await;
        let poll = {
            let mount = Arc::clone(&mount);
            tokio::spawn(async move {
                for _ in 0..10 {
                    mount.position().await.expect("polls");
                }
            })
        };
        let command = {
            let mount = Arc::clone(&mount);
            tokio::spawn(async move { mount.start_tracking(TrackingMode::Sidereal).await })
        };
        let watchdog = {
            let mount = Arc::clone(&mount);
            tokio::spawn(async move { mount.status().await.expect("status") })
        };

        poll.await.expect("poll task");
        command.await.expect("command task").expect("tracks");
        let seen = watchdog.await.expect("watchdog task");
        assert!(seen.is_consistent());
        assert_eq!(
            mount.status().await.expect("status").state,
            MountState::Tracking
        );
    }
}
