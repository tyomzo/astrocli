//! The session orchestrator's Phase 1 skeleton — SDD §5.6.
//!
//! ```text
//! Idle ──start_capture──► Capturing ──saved──► Idle
//!    └──connect/disconnect device management──┘   Faulted (from any state; operator ack → Idle)
//! ```
//!
//! Three states, because §5.6 ships the API shape before it ships the sequence engine: Phase 2a's
//! full FSM (targets, dithering, solve-and-center, pause/resume — SES-01..06) is a *superset* of
//! these, so nothing built against `Idle`/`Capturing`/`Faulted` has to be rewritten when it lands.
//!
//! # Why this is pure, and synchronous
//!
//! Nothing here does I/O, awaits, or knows what a camera is. It answers one question — "may this
//! request proceed, and what is the node doing now" — from state alone, which buys two things:
//!
//! * It is guarded by a [`std::sync::Mutex`] rather than a `tokio` one, so the claim below can be
//!   released from `Drop`. A guard that has to `.await` to give the slot back cannot run on the
//!   error paths, which are exactly the paths that must give it back.
//! * The 409 that refuses a second capture is decided from the same value that answered `202` to
//!   the first, so the two cannot disagree — the mount facade's reason for holding its own
//!   in-flight record (SDD §5.8.1, M1-T03), applied to the camera.
//!
//! # Where Faulted comes from, and why it is not just "the last capture failed"
//!
//! Most capture failures are *answers*: a shutter the body will not accept, an operator abort, a
//! disconnected camera. They leave the node able to take the next frame and so return it to
//! `Idle`. `Faulted` is reserved for the failures after which the node's picture of the camera or
//! of the session is no longer trustworthy — a camera thread past its operation-class timeout
//! (SDD §5.3.1 calls that wedged) and a frame store that could not write. Those need an operator,
//! which is what the explicit acknowledgement is: a state a retry cannot clear, so a UI retry loop
//! cannot paper over a disk that is failing.

use std::time::{Duration, Instant};

use astroctl_core::error::{ApiError, ErrorCode};
use astroctl_session::FrameId;
use chrono::{DateTime, Utc};

/// What the node is doing, as far as capture is concerned.
#[derive(Debug)]
pub enum Phase {
    /// Nothing in flight; a capture may start.
    Idle,
    /// One capture is running.
    Capturing(Run),
    /// Something needs the operator before another capture may start.
    Faulted(Fault),
}

/// The capture the orchestrator is holding the node for.
///
/// `frame_id` is `None` between the claim and the reservation. That window is small and real: the
/// slot has to be claimed *before* [`reserve_frame_id`](astroctl_session::Session::reserve_frame_id)
/// is called, or two concurrent requests both pass the "is it idle" check and the node burns an id
/// on the one it is about to refuse.
#[derive(Debug, Clone)]
pub struct Run {
    /// Ties the request, the events and the log line together.
    pub correlation_id: String,
    /// The reserved id, once there is one.
    pub frame_id: Option<FrameId>,
    /// When the shutter opened — the origin of every `elapsed_s` in `capture.progress`.
    pub started: Instant,
    /// How long the shutter is meant to stay open, which is what the progress ticker uses to
    /// decide the `exposing` → `downloading` transition.
    pub exposure: Duration,
}

/// A fault the operator has to acknowledge.
#[derive(Debug, Clone)]
pub struct Fault {
    /// The §4.2 code, so the UI's existing code-to-message mapping covers this too.
    pub code: ErrorCode,
    /// What happened, in the words the operator gets.
    pub message: String,
    /// When it happened — an acknowledgement hours later should still say what it is clearing.
    pub at: DateTime<Utc>,
}

/// How a capture ended, as far as the FSM cares.
#[derive(Debug)]
pub enum Outcome {
    /// The frame is durable. Back to `Idle`.
    Saved,
    /// The capture did not produce a frame, but the node is fine — an abort, a refusal, a
    /// disconnected camera. Back to `Idle`, because the next capture has every chance of working.
    Ended,
    /// The node cannot be trusted to take the next frame until an operator looks.
    Faulted { code: ErrorCode, message: String },
}

/// The FSM itself.
#[derive(Debug)]
pub struct Orchestrator {
    phase: Phase,
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl Orchestrator {
    /// A node that has done nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { phase: Phase::Idle }
    }

    /// Whether a capture is in flight.
    #[cfg(test)]
    #[must_use]
    pub const fn is_capturing(&self) -> bool {
        matches!(self.phase, Phase::Capturing(_))
    }

    /// The fault the operator has to clear, if there is one.
    #[cfg(test)]
    #[must_use]
    pub const fn fault(&self) -> Option<&Fault> {
        match &self.phase {
            Phase::Faulted(fault) => Some(fault),
            _ => None,
        }
    }

    /// Take the node for a capture, or say why not.
    ///
    /// The refusals are §4.2 codes rather than bespoke strings so the PWA's existing mapping
    /// covers them: both are `BUSY`/409, because in both the device is healthy and its *state* is
    /// wrong — which is precisely what §4.2 says that code is for. They are told apart by the
    /// message and by `detail`, not by the status.
    pub fn claim(&mut self, correlation_id: &str, exposure: Duration) -> Result<(), ApiError> {
        match &self.phase {
            Phase::Capturing(run) => Err(ApiError::new(
                ErrorCode::Busy,
                "a capture is already running; wait for it to finish or abort it",
            )
            .with_detail(serde_json::json!({
                "correlation_id": run.correlation_id,
                "frame_id": run.frame_id.map(|id| id.to_string()),
            }))),
            // Not retryable, and that is the point of the state: a client that answers a 409 by
            // trying again gets the same 409 until a human has looked at the reason.
            Phase::Faulted(fault) => Err(ApiError::new(
                ErrorCode::Busy,
                format!(
                    "capture is held after a fault ({}); acknowledge it before capturing again: \
                     {}",
                    fault.code.as_str(),
                    fault.message
                ),
            )
            .with_detail(serde_json::json!({
                "fault_code": fault.code.as_str(),
                "faulted_at": fault.at.to_rfc3339(),
            }))),
            Phase::Idle => {
                self.phase = Phase::Capturing(Run {
                    correlation_id: correlation_id.to_owned(),
                    frame_id: None,
                    started: Instant::now(),
                    exposure,
                });
                Ok(())
            }
        }
    }

    /// Record the id the frame store granted, and restart the clock at shutter-open.
    ///
    /// The clock restarts here rather than at [`claim`](Self::claim) because everything between
    /// the two — the settings read, the id reservation's two fsyncs — happens before the shutter
    /// opens, and `capture.progress.elapsed_s` is how long the *exposure* has been running. A
    /// countdown that had already spent 40 ms before the shutter moved would be wrong in the one
    /// direction an operator would notice.
    pub fn arm(&mut self, correlation_id: &str, frame_id: FrameId) {
        if let Phase::Capturing(run) = &mut self.phase {
            if run.correlation_id == correlation_id {
                run.frame_id = Some(frame_id);
                run.started = Instant::now();
            }
        }
    }

    /// Record the exposure once the settings read has resolved it.
    ///
    /// Separate from [`claim`](Self::claim) because the claim has to happen *before* the camera is
    /// asked anything — two requests a microsecond apart must not both pass the idle check — and
    /// the exposure is not known until the settings come back. The value is only used to decide
    /// when `capture.progress` switches to `downloading`, so a run that is refused before reaching
    /// here never needed it.
    pub fn set_exposure(&mut self, correlation_id: &str, exposure: Duration) {
        if let Phase::Capturing(run) = &mut self.phase {
            if run.correlation_id == correlation_id {
                run.exposure = exposure;
            }
        }
    }

    /// The run in flight, if `correlation_id` still names it.
    #[cfg(test)]
    #[must_use]
    pub fn run(&self, correlation_id: &str) -> Option<&Run> {
        match &self.phase {
            Phase::Capturing(run) if run.correlation_id == correlation_id => Some(run),
            _ => None,
        }
    }

    /// End the capture `correlation_id` names.
    ///
    /// Guarded by the id for the reason the mount facade guards its goto slot (SDD §5.8.1): a task
    /// that resolves late must not evict a *newer* run's record and leave the node believing
    /// nothing is in flight while something is. Returns whether it matched.
    pub fn finish(&mut self, correlation_id: &str, outcome: Outcome) -> bool {
        match &self.phase {
            Phase::Capturing(run) if run.correlation_id == correlation_id => {}
            _ => return false,
        }

        self.phase = match outcome {
            Outcome::Saved | Outcome::Ended => Phase::Idle,
            Outcome::Faulted { code, message } => Phase::Faulted(Fault {
                code,
                message,
                at: Utc::now(),
            }),
        };
        true
    }

    /// Clear a fault. Returns what was cleared, or `None` if there was nothing to clear.
    ///
    /// Idempotent on purpose: an operator who taps acknowledge twice, or two operators on two
    /// phones, must not get an error for agreeing with the node.
    pub fn acknowledge(&mut self) -> Option<Fault> {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Faulted(fault) => Some(fault),
            // Putting it back: acknowledging during a capture must not cancel the capture, which
            // is what a bare `= Idle` would silently do.
            other => {
                self.phase = other;
                None
            }
        }
    }
}

/// How a device failure is classified — the rule in the module docs, in one place.
///
/// It is a free function rather than a `From` impl so the two arms have somewhere to be argued.
#[must_use]
pub fn classify(error: &astroctl_core::error::DeviceError) -> Outcome {
    use astroctl_core::error::DeviceError;
    use astroctl_core::types::DeviceKind;

    match error {
        // SDD §5.3.1: a breached operation-class timeout means the camera thread is wedged, and
        // the recovery is to rebuild the driver — not to fire the next exposure at it and find
        // out. Until there is a rebuild path (M2, with the real gphoto2 driver), the node says so
        // and stops rather than looping.
        DeviceError::Timeout(_) => Outcome::Faulted {
            code: ErrorCode::from_device_error(DeviceKind::Camera, error),
            message: error.to_string(),
        },
        // Everything else is an answer to this request. An abort is the operator's own doing, a
        // rejected shutter is a setting they can change, and a camera that is not plugged in is a
        // cable — none of which the *next* capture inherits.
        _ => Outcome::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astroctl_core::error::DeviceError;
    use astroctl_session::FrameKind;

    fn id(sequence: u64) -> FrameId {
        FrameId::new(FrameKind::Light, sequence)
    }

    const EXPOSURE: Duration = Duration::from_secs(30);

    #[test]
    fn a_second_capture_while_capturing_is_refused_as_busy() {
        // The acceptance criterion, at the layer that decides it. 409 rather than 202-then-alert:
        // the node already knows the answer, so it is the answer to the request.
        let mut fsm = Orchestrator::new();
        fsm.claim("first", EXPOSURE).expect("idle accepts");
        fsm.arm("first", id(1));

        let refusal = fsm.claim("second", EXPOSURE).expect_err("busy");
        assert_eq!(refusal.http_status(), 409);
        assert_eq!(refusal.code, ErrorCode::Busy);
        // The running frame is in the detail, so an operator can see they are fighting their own
        // previous tap rather than the camera.
        let detail = refusal.detail.as_ref().expect("detail");
        assert_eq!(detail["correlation_id"], "first");
        assert_eq!(detail["frame_id"], "light_00001");
    }

    #[test]
    fn a_finished_capture_returns_the_node_to_idle() {
        let mut fsm = Orchestrator::new();
        fsm.claim("run", EXPOSURE).expect("idle accepts");
        assert!(fsm.is_capturing());
        assert!(fsm.finish("run", Outcome::Saved));
        assert!(!fsm.is_capturing());
        fsm.claim("next", EXPOSURE).expect("idle again");
    }

    #[test]
    fn a_late_task_cannot_evict_a_newer_run() {
        // The mount facade's `release_goto` guard, for capture. Without the id check a capture
        // task that resolved after its run had already been replaced would clear the *new* run's
        // record, and the node would answer 202 to a third capture while two were in flight.
        let mut fsm = Orchestrator::new();
        fsm.claim("old", EXPOSURE).expect("idle accepts");
        assert!(fsm.finish("old", Outcome::Ended));
        fsm.claim("new", EXPOSURE).expect("idle again");

        assert!(
            !fsm.finish("old", Outcome::Saved),
            "the stale id must not match"
        );
        assert!(fsm.is_capturing(), "the new run is still in flight");
    }

    #[test]
    fn a_fault_holds_capture_until_it_is_acknowledged() {
        // The other acceptance criterion: `Faulted` is not cleared by time, by a retry, or by the
        // next request succeeding. Only an operator clears it.
        let mut fsm = Orchestrator::new();
        fsm.claim("run", EXPOSURE).expect("idle accepts");
        fsm.finish(
            "run",
            Outcome::Faulted {
                code: ErrorCode::CameraTimeout,
                message: "the camera stopped answering".to_owned(),
            },
        );

        for attempt in 0..3 {
            let refusal = fsm.claim("again", EXPOSURE).expect_err("held");
            assert_eq!(refusal.http_status(), 409, "attempt {attempt}");
            assert!(
                refusal.message.contains("acknowledge"),
                "the refusal must say what clears it: {}",
                refusal.message
            );
        }

        let cleared = fsm.acknowledge().expect("a fault was cleared");
        assert_eq!(cleared.code, ErrorCode::CameraTimeout);
        fsm.claim("finally", EXPOSURE)
            .expect("acknowledged, so capture is allowed again");
    }

    #[test]
    fn acknowledging_when_nothing_is_wrong_changes_nothing() {
        let mut fsm = Orchestrator::new();
        assert!(fsm.acknowledge().is_none());
        assert!(!fsm.is_capturing());

        // And it must not cancel a running capture, which is what a careless "just set Idle"
        // would do to an operator who tapped the wrong control.
        fsm.claim("run", EXPOSURE).expect("idle accepts");
        assert!(fsm.acknowledge().is_none());
        assert!(fsm.is_capturing(), "a capture in flight survives an ack");
    }

    #[test]
    fn only_a_wedged_camera_faults_the_node() {
        // The classification rule, asserted rather than left in a comment. Getting this wrong in
        // the permissive direction means a fault the operator never sees; in the strict direction
        // it means every mistyped shutter speed needs an acknowledgement.
        assert!(matches!(
            classify(&DeviceError::Timeout(Duration::from_secs(150))),
            Outcome::Faulted { .. }
        ));
        for answer in [
            DeviceError::Aborted("the operator stopped it".to_owned()),
            DeviceError::Rejected("no card".to_owned()),
            DeviceError::NotConnected,
            DeviceError::Busy("exposing"),
            DeviceError::Transport("usb went away".to_owned()),
        ] {
            assert!(
                matches!(classify(&answer), Outcome::Ended),
                "{answer} is an answer to this request, not a fault the operator must clear"
            );
        }
    }

    #[test]
    fn the_exposure_clock_starts_when_the_shutter_does() {
        // `elapsed_s` must measure the exposure, not the settings read and the two fsyncs that
        // preceded it. `arm` is the shutter-open instant.
        let mut fsm = Orchestrator::new();
        fsm.claim("run", EXPOSURE).expect("idle accepts");
        let claimed = fsm.run("run").expect("running").started;
        std::thread::sleep(Duration::from_millis(5));
        fsm.arm("run", id(7));

        let run = fsm.run("run").expect("still running");
        assert_eq!(run.frame_id, Some(id(7)));
        assert!(
            run.started > claimed,
            "arming restarts the clock at shutter-open"
        );
    }

    #[test]
    fn arming_a_run_that_is_no_longer_current_does_nothing() {
        let mut fsm = Orchestrator::new();
        fsm.claim("old", EXPOSURE).expect("idle accepts");
        fsm.finish("old", Outcome::Ended);
        fsm.claim("new", EXPOSURE).expect("idle again");

        fsm.arm("old", id(99));
        assert_eq!(
            fsm.run("new").expect("the new run").frame_id,
            None,
            "a stale arm must not stamp its id on the current run"
        );
    }
}
