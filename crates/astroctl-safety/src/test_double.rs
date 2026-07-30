//! A mount that records what it was asked to do, for this crate's tests.
//!
//! # Why not the simulator
//!
//! `SimulatorMount` is the workspace's shared double and it is the right thing to test the API
//! layer against. It cannot be used here: ADD §5.6 rule 1 lets nothing above the HAL depend on a
//! concrete driver, so `astroctl-safety` may name `astroctl-hal` and `astroctl-core` and nothing
//! else — `scripts/check-deps.sh` fails the build on the edge, dev-dependencies included. That is
//! the rule working rather than getting in the way: a safety layer that had a driver in its test
//! tree is a safety layer that can grow a special case for one.
//!
//! What this needs and the simulator does not offer is the **command log** the MNT-15 acceptance
//! criterion is written against — "the mount was never commanded" is only assertable against a
//! record of every call. So the double is written to that requirement: an ordered log, a position
//! the test chooses, and per-call delays so a stop can be raced against a slow one.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    Axis, DeviceInfo, Direction, GuideRate, MountCapabilities, MountState, MountStatus, RaDec,
    SlewSpeed, TrackingMode,
};
use astroctl_hal::mount::MountDevice;
use async_trait::async_trait;

/// A mount that records every call and moves only when a test says so.
#[derive(Debug)]
pub struct RecordingMount {
    state: Mutex<State>,
    /// How long `goto` takes, so a stop can be issued while one is in flight.
    goto_duration: Duration,
    /// How long `slew` takes to reach the device — the in-flight command an e-stop must not
    /// queue behind.
    slew_duration: Duration,
}

#[derive(Debug)]
struct State {
    log: Vec<String>,
    position: RaDec,
    connected: bool,
    slewing: [bool; 2],
    tracking: Option<TrackingMode>,
    /// Bumped by every stop, so an in-flight goto can tell it was overridden — the same
    /// mechanism the simulator uses, and the reason a stopped goto is `Aborted` and not `Ok`.
    generation: u64,
}

impl RecordingMount {
    /// A connected mount pointing at `position`.
    pub fn at(position: RaDec) -> Self {
        Self {
            state: Mutex::new(State {
                log: Vec::new(),
                position,
                connected: true,
                slewing: [false, false],
                tracking: None,
                generation: 0,
            }),
            goto_duration: Duration::from_secs(2),
            slew_duration: Duration::ZERO,
        }
    }

    /// A mount whose `slew` takes `duration` to reach the device — the in-flight command a stop
    /// has to be able to overtake.
    #[must_use]
    pub fn with_slow_slew(mut self, duration: Duration) -> Self {
        self.slew_duration = duration;
        self
    }

    /// Hand it out the way every consumer above the HAL holds a device.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn locked(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every call this mount received, in order.
    pub fn log(&self) -> Vec<String> {
        self.locked().log.clone()
    }

    /// Whether the log contains any call at all that could have moved the telescope.
    pub fn was_commanded(&self) -> bool {
        self.locked().log.iter().any(|call| {
            matches!(
                call.as_str(),
                "goto" | "slew" | "park" | "sync" | "guide_pulse" | "start_tracking"
            )
        })
    }

    pub fn is_slewing(&self, axis: Axis) -> bool {
        self.locked().slewing[axis_index(axis)]
    }

    pub fn tracking(&self) -> Option<TrackingMode> {
        self.locked().tracking
    }

    /// Move the mount without going through a command — a test choosing where the sky is.
    pub fn place(&self, position: RaDec) {
        self.locked().position = position;
    }

    fn record(&self, what: &str) {
        self.locked().log.push(what.to_owned());
    }
}

const fn axis_index(axis: Axis) -> usize {
    match axis {
        Axis::Ra => 0,
        Axis::Dec => 1,
    }
}

#[async_trait]
impl MountDevice for RecordingMount {
    async fn connect(&self) -> Result<(), DeviceError> {
        self.record("connect");
        self.locked().connected = true;
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), DeviceError> {
        self.record("disconnect");
        self.locked().connected = false;
        Ok(())
    }

    async fn position(&self) -> Result<RaDec, DeviceError> {
        self.record("position");
        let state = self.locked();
        if state.connected {
            Ok(state.position)
        } else {
            Err(DeviceError::NotConnected)
        }
    }

    async fn status(&self) -> Result<MountStatus, DeviceError> {
        self.record("status");
        let state = self.locked();
        let slewing = state.slewing.iter().any(|s| *s);
        Ok(MountStatus {
            state: if !state.connected {
                MountState::Disconnected
            } else if slewing {
                MountState::Slewing
            } else if state.tracking.is_some() {
                MountState::Tracking
            } else {
                MountState::Idle
            },
            tracking: state.tracking,
            slewing,
            parked: false,
        })
    }

    async fn goto(&self, target: RaDec) -> Result<(), DeviceError> {
        let generation = {
            let mut state = self.locked();
            state.log.push("goto".to_owned());
            state.slewing = [true, true];
            state.generation
        };
        tokio::time::sleep(self.goto_duration).await;
        let mut state = self.locked();
        if state.generation != generation {
            return Err(DeviceError::Aborted("goto aborted by a stop".to_owned()));
        }
        state.slewing = [false, false];
        state.position = target;
        Ok(())
    }

    async fn sync(&self, pos: RaDec) -> Result<(), DeviceError> {
        self.record("sync");
        self.locked().position = pos;
        Ok(())
    }

    async fn start_tracking(&self, mode: TrackingMode) -> Result<(), DeviceError> {
        self.record("start_tracking");
        self.locked().tracking = Some(mode);
        Ok(())
    }

    async fn stop_tracking(&self) -> Result<(), DeviceError> {
        self.record("stop_tracking");
        let mut state = self.locked();
        state.tracking = None;
        state.generation += 1;
        Ok(())
    }

    async fn slew(
        &self,
        axis: Axis,
        _dir: Direction,
        _speed: SlewSpeed,
    ) -> Result<(), DeviceError> {
        self.record("slew");
        tokio::time::sleep(self.slew_duration).await;
        self.locked().slewing[axis_index(axis)] = true;
        Ok(())
    }

    async fn stop_slew(&self, axis: Axis) -> Result<(), DeviceError> {
        self.record("stop_slew");
        let mut state = self.locked();
        state.slewing[axis_index(axis)] = false;
        state.generation += 1;
        Ok(())
    }

    async fn guide_pulse(
        &self,
        _axis: Axis,
        _dir: Direction,
        _duration_ms: u32,
        _rate: GuideRate,
    ) -> Result<(), DeviceError> {
        self.record("guide_pulse");
        Ok(())
    }

    async fn park(&self) -> Result<(), DeviceError> {
        self.record("park");
        Ok(())
    }

    async fn unpark(&self) -> Result<(), DeviceError> {
        self.record("unpark");
        Ok(())
    }

    async fn emergency_stop(&self) -> Result<(), DeviceError> {
        // No sleep and no shared line: SDD §5.2.4 gives the e-stop its own lane, and a double
        // that made it wait would let a wrapper with a lock in the wrong place pass its own test.
        let mut state = self.locked();
        state.log.push("emergency_stop".to_owned());
        state.slewing = [false, false];
        state.tracking = None;
        state.generation += 1;
        Ok(())
    }

    fn capabilities(&self) -> MountCapabilities {
        MountCapabilities {
            has_pec: false,
            has_pulse_guide: true,
            tracking_rates: vec![TrackingMode::Sidereal],
            max_slew_speed_x_sidereal: 800,
            position_resolution_bits: 24,
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "recording double".to_owned(),
            model: "test".to_owned(),
            firmware: None,
            serial: None,
            protocol: "test".to_owned(),
        }
    }
}
