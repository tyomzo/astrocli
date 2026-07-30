//! Scripted failures — SDD §9: "fault injection is a constructor parameter so T-SER/T-E2E tests
//! express failure scenarios declaratively".
//!
//! A constructor parameter rather than a global switch or a set-during-the-test method, and the
//! difference matters twice. A global would make two tests running in the same process able to
//! break each other, which on a `cargo test` binary is every test. A setter would make the
//! failure a *step* in the test rather than a property of the device, so a scenario reads
//! "arrange, act, reach behind the HAL, act again" instead of "this mount times out once, now
//! watch what the layer above does about it".
//!
//! Every fault is consumed once. That is what makes recovery testable at all: the assertion
//! after the failing call is that the *next* call works, which is the behaviour the retry logic
//! above the HAL exists for and the behaviour nobody writes a test for when a fault is a mode
//! the device stays in.

use std::time::Duration;

use astroctl_core::error::DeviceError;

/// Which trait method an exchange belongs to, so a fault can name one.
///
/// A closed enum rather than a `&str` because a typo in `TimeoutOnce("postion")` is a test that
/// passes for the wrong reason — it asserts a timeout that never fires against a call that
/// happened to succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountCommand {
    /// [`connect`](astroctl_hal::mount::MountDevice::connect).
    Connect,
    /// [`disconnect`](astroctl_hal::mount::MountDevice::disconnect).
    Disconnect,
    /// [`position`](astroctl_hal::mount::MountDevice::position) — the 1 Hz poll.
    Position,
    /// [`status`](astroctl_hal::mount::MountDevice::status).
    Status,
    /// [`goto`](astroctl_hal::mount::MountDevice::goto).
    Goto,
    /// [`sync`](astroctl_hal::mount::MountDevice::sync).
    Sync,
    /// [`start_tracking`](astroctl_hal::mount::MountDevice::start_tracking).
    StartTracking,
    /// [`stop_tracking`](astroctl_hal::mount::MountDevice::stop_tracking).
    StopTracking,
    /// [`slew`](astroctl_hal::mount::MountDevice::slew).
    Slew,
    /// [`stop_slew`](astroctl_hal::mount::MountDevice::stop_slew).
    StopSlew,
    /// [`guide_pulse`](astroctl_hal::mount::MountDevice::guide_pulse).
    GuidePulse,
    /// [`park`](astroctl_hal::mount::MountDevice::park).
    Park,
    /// [`unpark`](astroctl_hal::mount::MountDevice::unpark).
    Unpark,
    /// [`emergency_stop`](astroctl_hal::mount::MountDevice::emergency_stop).
    EmergencyStop,
}

impl MountCommand {
    /// What an operator would call this in a log line or an error message.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::Position => "read position",
            Self::Status => "read status",
            Self::Goto => "goto",
            Self::Sync => "sync",
            Self::StartTracking => "start tracking",
            Self::StopTracking => "stop tracking",
            Self::Slew => "slew",
            Self::StopSlew => "stop slew",
            Self::GuidePulse => "guide pulse",
            Self::Park => "park",
            Self::Unpark => "unpark",
            Self::EmergencyStop => "emergency stop",
        }
    }
}

/// One scripted failure.
///
/// The four variants are the four failure *shapes* the real link has, not four arbitrary errors:
/// the mount does not answer, it answers with rubbish, the adapter falls out, or the mechanism
/// stops while the controller still thinks it is moving. Each maps onto a different
/// [`DeviceError`] and a different recovery, which is the reason for having four rather than one
/// parameterised by an error value.
#[derive(Debug, Clone, PartialEq)]
pub enum Fault {
    /// The next `command` gets no reply. The driver waits out its request timeout and its
    /// retries, then reports [`DeviceError::Timeout`]; the call after that succeeds.
    ///
    /// Modelled on the measured case: a frame the mount does not recognise as complete produces
    /// *no response at all*, and the link does not wedge — the next well-formed command works.
    TimeoutOnce(MountCommand),

    /// The `n`-th exchange after connecting (1-based) comes back unintelligible, giving
    /// [`DeviceError::Protocol`]. Everything before and after it is fine.
    ///
    /// Counted in exchanges rather than named by command because that is how the real thing
    /// arrives — a byte lost to line noise hits whatever was in flight — and because it lets a
    /// test place the corruption inside a multi-exchange sequence such as a goto.
    ///
    /// `Protocol` is deliberately not retryable (SDD §4.2): an unparseable reply repeats
    /// deterministically until the driver or the firmware changes, so a layer above that retries
    /// it is a layer that will retry forever.
    GarbledResponse(u32),

    /// The link dies this long after [`connect`](astroctl_hal::mount::MountDevice::connect).
    ///
    /// The call that discovers it gets [`DeviceError::Transport`]; every call after that gets
    /// [`DeviceError::NotConnected`] until the operator reconnects, which succeeds. **Motion
    /// does not stop** — an unplugged mount keeps slewing, which is the whole reason REL-02's
    /// watchdog exists and is the scenario the safety layer must be tested against.
    DisconnectAfter(Duration),

    /// The next slew stops dead partway and never reaches its target.
    ///
    /// The controller keeps reporting motion, so nothing on the wire says anything is wrong;
    /// the goto simply never completes and the caller's watchdog is what notices. That makes
    /// this the only fault whose symptom is a [`DeviceError::Timeout`] on a command that was
    /// *acknowledged*, which is a different code path above the HAL from a link timeout.
    StallDuringSlew,
}

/// A script of faults handed to a simulator at construction (SDD §9).
///
/// Cheap to clone so one plan can build several mounts; each mount consumes its own copy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FaultPlan {
    faults: Vec<Fault>,
}

impl FaultPlan {
    /// A mount that behaves. The default, and what every test that is not about failure uses.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A plan from a list of faults.
    ///
    /// ```
    /// use std::time::Duration;
    /// use astroctl_drivers::simulator::{Fault, FaultPlan, MountCommand};
    ///
    /// let plan = FaultPlan::new([
    ///     Fault::TimeoutOnce(MountCommand::Position),
    ///     Fault::DisconnectAfter(Duration::from_secs(30)),
    /// ]);
    /// assert_eq!(plan.len(), 2);
    /// ```
    #[must_use]
    pub fn new(faults: impl IntoIterator<Item = Fault>) -> Self {
        Self {
            faults: faults.into_iter().collect(),
        }
    }

    /// Adds one fault, for building a plan up in a test fixture.
    #[must_use]
    pub fn with(mut self, fault: Fault) -> Self {
        self.faults.push(fault);
        self
    }

    /// How many faults are still armed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.faults.len()
    }

    /// Whether the mount will behave.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.faults.is_empty()
    }

    /// Arms the plan against a live device.
    pub(super) fn arm(&self) -> ArmedFaults {
        ArmedFaults {
            remaining: self.faults.clone(),
            exchanges: 0,
        }
    }
}

/// The mutable half: what is left of the plan, and how far through the session we are.
#[derive(Debug)]
pub(super) struct ArmedFaults {
    remaining: Vec<Fault>,
    /// Exchanges since the last successful connect — what [`Fault::GarbledResponse`] counts.
    exchanges: u32,
}

/// What an exchange should do instead of succeeding.
#[derive(Debug)]
pub(super) enum Injected {
    /// No reply: wait out the timeout budget, then report it.
    Timeout,
    /// A reply nobody can parse.
    Garbled,
}

impl ArmedFaults {
    /// Restarts the exchange count. A `GarbledResponse(3)` means the third exchange of the
    /// session, so reconnecting has to put the operator back at the start of the script —
    /// otherwise the fault's position depends on how many times the test connected.
    pub(super) fn on_connect(&mut self) {
        self.exchanges = 0;
    }

    /// Consumes whatever fault applies to this exchange, if any.
    pub(super) fn next_exchange(&mut self, command: MountCommand) -> Option<Injected> {
        self.exchanges += 1;
        let exchanges = self.exchanges;
        let hit = self.remaining.iter().position(|fault| match *fault {
            Fault::TimeoutOnce(target) => target == command,
            Fault::GarbledResponse(n) => n == exchanges,
            Fault::DisconnectAfter(_) | Fault::StallDuringSlew => false,
        })?;
        match self.remaining.remove(hit) {
            Fault::TimeoutOnce(_) => Some(Injected::Timeout),
            Fault::GarbledResponse(_) => Some(Injected::Garbled),
            // Neither is reachable: the closure above never selects them.
            Fault::DisconnectAfter(_) | Fault::StallDuringSlew => None,
        }
    }

    /// Consumes the link-loss fault, returning how long after connecting the link dies.
    pub(super) fn take_disconnect_delay(&mut self) -> Option<Duration> {
        let hit = self
            .remaining
            .iter()
            .position(|fault| matches!(fault, Fault::DisconnectAfter(_)))?;
        match self.remaining.remove(hit) {
            Fault::DisconnectAfter(after) => Some(after),
            _ => None,
        }
    }

    /// Consumes the stall fault, if the next slew is meant to jam.
    pub(super) fn take_stall(&mut self) -> bool {
        let Some(hit) = self
            .remaining
            .iter()
            .position(|fault| matches!(fault, Fault::StallDuringSlew))
        else {
            return false;
        };
        self.remaining.remove(hit);
        true
    }
}

/// The error a garbled reply produces, worded for the operator who has to decide whether to
/// check a cable or a driver.
pub(super) fn garbled_error(command: MountCommand) -> DeviceError {
    DeviceError::Protocol(format!(
        "unintelligible reply to {} (simulated line corruption)",
        command.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_fires_on_its_command_and_only_once() {
        let mut armed = FaultPlan::new([Fault::TimeoutOnce(MountCommand::Position)]).arm();
        // Another command first: the fault waits for its own, it does not fire on whatever
        // happens to be next.
        assert!(armed.next_exchange(MountCommand::Status).is_none());
        assert!(matches!(
            armed.next_exchange(MountCommand::Position),
            Some(Injected::Timeout)
        ));
        // ...and the retry succeeds, which is the half that makes recovery testable.
        assert!(armed.next_exchange(MountCommand::Position).is_none());
    }

    #[test]
    fn a_garbled_reply_lands_on_the_numbered_exchange() {
        let mut armed = FaultPlan::new([Fault::GarbledResponse(2)]).arm();
        assert!(armed.next_exchange(MountCommand::Position).is_none());
        assert!(matches!(
            armed.next_exchange(MountCommand::Position),
            Some(Injected::Garbled)
        ));
        assert!(armed.next_exchange(MountCommand::Position).is_none());
    }

    #[test]
    fn reconnecting_restarts_the_exchange_count() {
        // Otherwise `GarbledResponse(1)` would mean "the first exchange, unless the test
        // reconnected first, in which case some other one".
        let mut armed = FaultPlan::new([Fault::GarbledResponse(1)]).arm();
        armed.on_connect();
        armed.exchanges = 40;
        armed.on_connect();
        assert!(matches!(
            armed.next_exchange(MountCommand::Status),
            Some(Injected::Garbled)
        ));
    }

    #[test]
    fn the_link_loss_and_stall_faults_are_taken_once_each() {
        let mut armed = FaultPlan::none()
            .with(Fault::DisconnectAfter(Duration::from_secs(5)))
            .with(Fault::StallDuringSlew)
            .arm();
        assert_eq!(armed.take_disconnect_delay(), Some(Duration::from_secs(5)));
        assert_eq!(armed.take_disconnect_delay(), None);
        assert!(armed.take_stall());
        assert!(!armed.take_stall());
        // Neither is an exchange-time fault, so neither can be consumed by one.
        assert!(armed.next_exchange(MountCommand::Goto).is_none());
    }

    #[test]
    fn an_empty_plan_never_injects_anything() {
        let mut armed = FaultPlan::none().arm();
        assert!(FaultPlan::none().is_empty());
        for _ in 0..100 {
            assert!(armed.next_exchange(MountCommand::Position).is_none());
        }
    }
}
