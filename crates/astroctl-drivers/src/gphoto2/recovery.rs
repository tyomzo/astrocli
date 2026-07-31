//! The REL-03 recovery loop — SDD §5.3.1's second half, and the one M2-T02 left as a `TODO`.
//!
//! # What it is recovering from, measured rather than assumed
//!
//! M2-T01 pulled the cable out of a running live-view stream and wrote down what happened
//! (`spikes/gphoto2-r10/FINDINGS.md` step 7). Four facts came out of it and all four are visible
//! in the code below:
//!
//! 1. **The two USB failures are distinguishable.** `Could not find the requested device on the
//!    USB port` is a cable; `Could not claim the USB device` is another process. They get
//!    different operator messages, and on the claim branch gvfs is named if it is there.
//! 2. **The old handle never recovers.** Five retries on the existing `Camera` after replug all
//!    failed identically. So abandoning the thread and the context is mandatory, not a fallback,
//!    and there is no cheaper rung to try first.
//! 3. **Recovery itself is fast.** A fresh `Context` plus autodetect: **108 ms**. Which means the
//!    normal outcome of this whole module is one attempt, and every retry after it is evidence of
//!    something *else* holding the camera.
//! 4. **gvfs auto-mounts on hotplug and blocks recovery for eighty seconds.** That is the failure
//!    that breaks REL-03 on a field node with a desktop session, and it is why this loop is
//!    bounded: an operator whose sequence has stalled is owed the name of the process that took
//!    their camera, not an infinite silent retry.
//!
//! # Why it gives up
//!
//! [`MAX_ATTEMPTS`] attempts and then [`LinkState::Faulted`]. Retrying forever would be the
//! obvious kindness and it is the wrong one: the measured blocker is a *desktop mount that will
//! not release itself*, so attempt two hundred fails exactly the way attempt two did, and the
//! only thing the extra attempts buy is a night of an operator watching a stalled progress bar
//! with no message. Faulting is what puts the diagnosis in front of them — and a plain
//! `connect()` clears it, so giving up costs nothing once the cause is fixed.
//!
//! # Why this drives the state and does not publish it
//!
//! `astroctl-drivers` has no event bus, deliberately (crate docs: "drivers are silent"). So this
//! loop maintains [`LinkState`] and publishes it on a `watch` channel; turning that into
//! `camera.status` on the wire is the facade's job. **The wire has no `reconnecting` state
//! today** — `astroctl_core::event::CameraStatus` is a two-state boolean — which is recorded as
//! a spec gap in the M2-T04 task file rather than fixed here, because a §4.3 payload change is
//! not something an implementation task invents (tasks/README rule 2).

use std::sync::Arc;
use std::time::Duration;

use astroctl_core::error::DeviceError;

use super::gvfs;
use super::thread::{FaultSource, LinkFault};
use super::usbreset::{self, ResetOutcome};

/// How many times the loop rebuilds the link before giving up.
///
/// Six, against a measured 108 ms for the attempt that works. The number is not chosen to be
/// generous — it is chosen so that the *total* time to a diagnosis lands near T-CAM-1's 30 s
/// window: with [`BACKOFF_BASE`] doubling to [`BACKOFF_CEILING`] the waits are 1, 2, 4, 8 and 15
/// seconds, so an operator who is going to be told "gvfs has your camera" is told inside half a
/// minute rather than at dawn.
const MAX_ATTEMPTS: u32 = 6;

/// The first wait after a failed attempt.
///
/// One second, not zero: the failure this loop most often meets is a device that is *mid-replug*,
/// and the kernel needs a moment to finish enumerating it. Retrying instantly would spend the
/// whole attempt budget inside the window where no attempt could have succeeded.
const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The longest wait between attempts.
///
/// Fifteen seconds. Deliberately far below `astroctl-transfer`'s 300 s ceiling for the same
/// pattern, because the failures differ: a stacking server that is down stays down for minutes,
/// while a camera comes back the instant a cable is pushed in — and a five-minute wait would mean
/// an operator who fixed the problem stands in the dark waiting for software.
const BACKOFF_CEILING: Duration = Duration::from_secs(15);

/// How many failed attempts pass before a USB reset is attempted.
///
/// Two, so the measured-fast path gets a clear run first. See [`super::usbreset`] for the fuller
/// argument, of which the short form is: a reset cannot fix the failure that was actually
/// observed, and re-enumeration is precisely what invites gvfs to take the camera again.
const USB_RESET_AFTER_ATTEMPTS: u32 = 2;

/// What the driver believes about its link to the camera.
///
/// This is the driver's half of `camera.status`. The facade reads it and decides what goes on the
/// wire; nothing here publishes anything (crate docs: drivers are silent).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LinkState {
    /// No camera has been asked for, or `disconnect` was called. The resting state.
    #[default]
    Disconnected,
    /// The camera is open and answering.
    Connected,
    /// The link failed and is being rebuilt. **This is the state SDD §5.3.1 calls
    /// `reconnecting`.**
    Reconnecting {
        /// Which attempt is in flight, from 1.
        attempt: u32,
        /// How many there will be before [`Faulted`](Self::Faulted).
        of: u32,
        /// What went wrong, in words an operator can act on.
        because: String,
    },
    /// Recovery ran out of attempts. A fresh `connect()` clears it.
    Faulted {
        /// What went wrong and, where it is known, which process to go and look at.
        reason: String,
    },
}

impl LinkState {
    /// Whether the camera is usable right now.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }

    /// The operator-facing sentence for this state, or `None` while all is well.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Connected | Self::Disconnected => None,
            Self::Reconnecting { because, .. } => Some(because),
            Self::Faulted { reason } => Some(reason),
        }
    }
}

/// Capped exponential backoff that resets after a success.
///
/// The same shape as `astroctl-transfer`'s, and re-implemented rather than shared for a reason
/// the dependency matrix decides: ADD §5.6 rule 1 lets this crate name `astroctl-hal` and
/// `astroctl-core` and nothing else, and `Backoff` lives in `astroctl-transfer`. Lifting it into
/// core would be the right move the moment a third caller wants it; two callers with thirty lines
/// each is not yet a reason to move a type across the tree.
///
/// **The reset is the part that matters** and it is M1-T13's lesson, learned by the worker
/// supervisor and re-learned by the transfer agent: a backoff that only ever grows turns one bad
/// minute into an hour of idle. A camera that reconnected is a camera whose next failure starts
/// from one second again.
#[derive(Debug, Clone, Copy)]
struct Backoff {
    current: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self {
            current: BACKOFF_BASE,
        }
    }

    /// The wait before the next attempt, then double for the one after.
    fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.current.saturating_mul(2).min(BACKOFF_CEILING);
        delay
    }

    /// A success means the camera works; the next failure starts from the base again.
    fn reset(&mut self) {
        self.current = BACKOFF_BASE;
    }
}

/// What a link needs to be rebuilt, without the recovery loop knowing what a driver is.
///
/// A trait rather than a back-reference to `CanonGPhoto2Camera`, for two reasons that are really
/// one: the loop would otherwise hold a strong reference to the thing that owns it, and every
/// test of the loop would need a whole driver. With this seam the retry ladder, the backoff, the
/// branching and the alert discipline are all testable against a few lines of stub, on a machine
/// with neither camera nor libgphoto2 — which is the same argument [`super::ops::CamOps`] makes
/// one layer down, applied one layer up.
#[async_trait::async_trait]
pub(crate) trait Relink: Send + Sync + 'static {
    /// Tears down whatever link exists.
    ///
    /// `abandon` is `true` for a wedge: the thread is inside a libgphoto2 call that may never
    /// return, so it must be dropped without joining. `false` means the thread is healthy and
    /// only the camera left, in which case waiting for the context to be released is not merely
    /// safe but *required* — the next attempt would otherwise race the old one's USB claim.
    async fn tear_down(&self, abandon: bool);

    /// Builds a fresh thread and context and opens the camera.
    ///
    /// # Errors
    /// Whatever the camera said, with libgphoto2's own text intact so the caller can branch on it.
    async fn rebuild(&self) -> Result<(), DeviceError>;

    /// Where the sysfs USB devices live. Overridden in tests; `/sys/bus/usb/devices` in life.
    fn sysfs_usb_devices(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(usbreset::SYSFS_USB_DEVICES)
    }
}

/// Runs recovery for the life of the driver.
///
/// One task, started once. It owns the fault channel's receiving half, so faults from *every*
/// link this driver ever builds arrive here in order, and there can never be two recovery cycles
/// racing to rebuild the same link.
///
/// Ends when the driver is dropped, which closes the fault channel.
pub(crate) async fn run(
    relink: Arc<dyn Relink>,
    mut faults: FaultSource,
    state: tokio::sync::watch::Sender<LinkState>,
) {
    let mut backoff = Backoff::new();

    while let Some(fault) = faults.recv().await {
        // Everything that arrived before this one described the same dead link — a live-view loop
        // at 5 fps posts one per frame until its link stops answering. Drain them so the next
        // cycle is not run once per skipped frame.
        while faults.try_recv().is_ok() {}

        recover(&*relink, &fault, &state, &mut backoff).await;

        // **And drain again, which is not the same drain.** A cycle's own failed attempts post
        // faults: a `rebuild` whose `open` exceeds its budget is a wedge, and wedges report. Those
        // describe links this cycle has *already* replaced. Leaving them queued means the next
        // `recv` returns immediately with stale news and starts a second cycle — which tears down
        // the healthy link the first cycle just built, and does it moments after announcing
        // `Connected`. That failure is not theoretical; it is what
        // `a_wedged_camera_recovers_by_itself_to_a_working_capture` caught.
        //
        // Safe because a `CameraLink` reports at most once and the link that just succeeded has
        // not reported at all: anything in the queue at this instant is necessarily about an older
        // one.
        while faults.try_recv().is_ok() {}
    }

    tracing::debug!("the camera recovery loop is exiting; the driver has been dropped");
}

/// One recovery cycle: tear down, then climb the ladder until the camera answers or the attempts
/// run out.
async fn recover(
    relink: &dyn Relink,
    fault: &LinkFault,
    state: &tokio::sync::watch::Sender<LinkState>,
    backoff: &mut Backoff,
) {
    // Mandatory and first, whatever else happens. M2-T01 retried the stale handle five times
    // after a replug and every one failed with the same error, so there is nothing to salvage —
    // and holding it would keep the USB claim that the fresh attempt needs.
    relink.tear_down(fault.abandons_the_thread()).await;

    let mut because = describe(fault, relink);
    let mut reset_attempted = false;

    for attempt in 1..=MAX_ATTEMPTS {
        // One transition, one announcement (T11/T14's lesson): the state carries the attempt
        // number, so a UI can show progress without this loop emitting a fresh alert per rung.
        set(
            state,
            LinkState::Reconnecting {
                attempt,
                of: MAX_ATTEMPTS,
                because: because.clone(),
            },
        );

        match relink.rebuild().await {
            Ok(()) => {
                // T13's lesson: the *next* failure starts from one second again. Without this the
                // backoff ratchets to the ceiling permanently after one bad night.
                backoff.reset();
                tracing::info!(attempt, "the camera is back");
                set(state, LinkState::Connected);
                return;
            }
            Err(error) => {
                because = explain(&error, relink);
                tracing::warn!(
                    attempt,
                    of = MAX_ATTEMPTS,
                    %error,
                    "the camera did not come back on this attempt"
                );

                // The escalation, and only on the branch where it could possibly help. On a claim
                // conflict a reset is worse than useless: it re-enumerates the device, and
                // re-enumeration is the hotplug event that invited gvfs to take the camera in the
                // first place. See `super::usbreset` for the whole argument.
                if !reset_attempted
                    && attempt >= USB_RESET_AFTER_ATTEMPTS
                    && wants_a_usb_reset(&error)
                {
                    reset_attempted = true;
                    attempt_usb_reset(relink).await;
                }
            }
        }

        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(backoff.next_delay()).await;
        }
    }

    tracing::error!(attempts = MAX_ATTEMPTS, %because, "giving up on the camera");
    set(state, LinkState::Faulted { reason: because });
}

/// Publishes a state, but only when it is a change.
///
/// `send_if_modified` rather than `send`: the `watch` channel wakes every subscriber on every
/// send, and republishing `Reconnecting { attempt: 3, .. }` unchanged would be the alert-per-tick
/// the rest of the system is careful not to produce (§5.10.4). The attempt number *is* part of
/// the value, so a genuine rung change still gets through.
fn set(state: &tokio::sync::watch::Sender<LinkState>, next: LinkState) {
    state.send_if_modified(|current| {
        if *current == next {
            return false;
        }
        *current = next;
        true
    });
}

/// Asks the kernel to reset the camera, and carries on either way.
///
/// Best effort by construction: a refused reset is logged with the remedy and the ladder
/// continues, because the rung that was *measured* to work is the plain rebuild and it does not
/// depend on this having succeeded.
async fn attempt_usb_reset(relink: &dyn Relink) {
    let sysfs = relink.sysfs_usb_devices();
    // On the blocking pool: it opens a device node and issues a syscall. Small, but it is
    // filesystem work on a runtime the field node sizes at one or two workers (SDD §7).
    let outcome = tokio::task::spawn_blocking(move || usbreset::reset_camera(&sysfs))
        .await
        .unwrap_or_else(|error| {
            ResetOutcome::Refused(format!("the USB reset task did not finish: {error}"))
        });

    match outcome {
        ResetOutcome::Reset { node } => tracing::info!(
            node = %node.display(),
            "reset the camera's USB device; it will re-enumerate"
        ),
        // Not a warning. This is the ordinary answer when the cable really is out, and it is the
        // answer that says "keep waiting" rather than "something is wrong with the reset".
        ResetOutcome::NoDevice => {
            tracing::info!("no camera is on the USB bus to reset; waiting for one to appear")
        }
        ResetOutcome::Refused(reason) => tracing::warn!(%reason, "the USB reset was refused"),
    }
}

/// Whether a failed attempt is the kind a USB reset could plausibly clear.
///
/// Only a device that says it is *gone*. A claim conflict is another process holding the device
/// and a reset does not take it away from them; a `Rejected`/`Protocol` failure is a body that
/// answered, which is the opposite of a device needing a reset.
fn wants_a_usb_reset(error: &DeviceError) -> bool {
    matches!(error, DeviceError::Transport(detail) if gvfs::is_device_missing(detail))
}

/// The operator's sentence for the fault that started this cycle.
fn describe(fault: &LinkFault, relink: &dyn Relink) -> String {
    match fault {
        LinkFault::Wedged { operation, budget } => format!(
            "the camera stopped answering — `{operation}` ran past its {:.0} s budget, so the \
             camera thread was abandoned and is being rebuilt",
            budget.as_secs_f64()
        ),
        LinkFault::DeviceGone { detail } => device_gone(detail),
        LinkFault::ClaimLost { detail } => claim_lost(detail, relink),
        LinkFault::Unresponsive { detail, after } => unresponsive(detail, *after),
    }
}

/// The operator's sentence for a failed *attempt*.
fn explain(error: &DeviceError, relink: &dyn Relink) -> String {
    match error {
        DeviceError::Transport(detail) if gvfs::is_device_missing(detail) => device_gone(detail),
        DeviceError::Transport(detail) if gvfs::is_claim_failure(detail) => {
            claim_lost(detail, relink)
        }
        other => format!("the camera could not be reopened: {other}"),
    }
}

/// The cable branch.
///
/// Names the three physical causes and nothing else. **It must not mention gvfs**: a device that
/// is not on the bus is not being held by anything, and sending an operator to hunt for a desktop
/// mount at two in the morning — when the actual answer is a cable that a tripod leg caught, or
/// the battery that this driver's own `camera.status` has been reporting — is the wrong place at
/// the worst time. M2-T01 established that these two errors are distinguishable precisely so that
/// this message and [`claim_lost`] can be different.
fn device_gone(detail: &str) -> String {
    format!(
        "the camera is no longer on the USB bus ({detail}). Check the cable, the power switch, \
         and the battery — a body that runs flat mid-session disappears exactly like an unplugged \
         one. Reconnecting automatically."
    )
}

/// The stale-session branch — the device is there and the handle behind it is dead.
///
/// Names the state precisely, because it is the one an operator is most likely to misread: the
/// camera is still on the bus, its screen is on, `lsusb` lists it, and nothing about the hardware
/// looks wrong. Telling them to check the cable would send them to inspect something that is
/// visibly fine. What actually happened is a bus-level reset — a hub glitch, a power-management
/// event, or a `USBDEVFS_RESET` from another process — and the fix is the one the driver is
/// already applying: a fresh context.
fn unresponsive(detail: &str, after: u32) -> String {
    format!(
        "the camera is still on the USB bus but has stopped answering — {after} transfers in a \
         row failed ({detail}). That is a bus-level reset rather than an unplugged cable: the \
         device is still enumerated and the session behind it is dead. Rebuilding the connection."
    )
}

/// The claim branch, with gvfs named when it is actually there.
///
/// Delegates to [`gvfs::explain_claim_failure`] rather than writing its own message. That
/// function is the shared diagnosis M2-T02 deliberately put above the transport, it has the rule
/// this branch most needs — *name a cause only when one has been found* — and it produces the
/// `gio mount -u` line an operator can paste. Reimplementing it here would be a second, worse
/// copy that drifts, and would lose the runbook pointer.
fn claim_lost(detail: &str, relink: &dyn Relink) -> String {
    let _ = relink;
    format!(
        "{} Reconnecting automatically, but this will keep failing until the claim is released.",
        gvfs::explain_claim_failure(detail, &gvfs::gvfs_root())
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use astroctl_core::error::DeviceError;

    use super::super::thread::{fault_channel, LinkFault};
    use super::{run, Backoff, LinkState, Relink, BACKOFF_BASE, BACKOFF_CEILING, MAX_ATTEMPTS};

    /// A scriptable [`Relink`]: it fails a given number of times and then succeeds.
    #[derive(Debug)]
    struct Stub {
        /// How many rebuilds still fail before one succeeds. `u32::MAX` means "never succeed".
        fail_first: AtomicU32,
        /// The text each failure carries — libgphoto2's own words, which is what the branching
        /// under test keys off. A `String` rather than a `DeviceError` because `DeviceError` is
        /// deliberately not `Clone`, and every failure here is a `Transport`.
        error: Mutex<String>,
        /// Every rebuild attempt, counted.
        rebuilds: AtomicU32,
        /// Every tear-down, with the `abandon` flag it was given.
        tear_downs: Mutex<Vec<bool>>,
        /// A sysfs root with no camera in it, so no test ever touches a real device node.
        sysfs: PathBuf,
    }

    impl Stub {
        fn new(fail_first: u32, error: &str) -> Arc<Self> {
            Arc::new(Self {
                fail_first: AtomicU32::new(fail_first),
                error: Mutex::new(error.to_owned()),
                rebuilds: AtomicU32::new(0),
                tear_downs: Mutex::new(Vec::new()),
                // Deliberately absent: `usbreset::reset_camera` answers `NoDevice` and the ladder
                // carries on, which is exactly the behaviour under test.
                sysfs: PathBuf::from("/nonexistent/astroctl-recovery-test/sys"),
            })
        }

        fn rebuilds(&self) -> u32 {
            self.rebuilds.load(Ordering::SeqCst)
        }

        fn tear_downs(&self) -> Vec<bool> {
            self.tear_downs.lock().expect("stub").clone()
        }
    }

    #[async_trait::async_trait]
    impl Relink for Stub {
        async fn tear_down(&self, abandon: bool) {
            self.tear_downs.lock().expect("stub").push(abandon);
        }

        async fn rebuild(&self) -> Result<(), DeviceError> {
            self.rebuilds.fetch_add(1, Ordering::SeqCst);
            let remaining = self.fail_first.load(Ordering::SeqCst);
            if remaining == 0 {
                return Ok(());
            }
            if remaining != u32::MAX {
                self.fail_first.store(remaining - 1, Ordering::SeqCst);
            }
            Err(DeviceError::Transport(
                self.error.lock().expect("stub").clone(),
            ))
        }

        fn sysfs_usb_devices(&self) -> PathBuf {
            self.sysfs.clone()
        }
    }

    /// libgphoto2's own words for a camera that has been unplugged.
    const GONE: &str = "Could not find the requested device on the USB port";

    /// ...and for one that something else is holding.
    const CLAIMED: &str = "Could not claim the USB device";

    /// Starts the loop and returns the fault sender and a state receiver.
    fn start(
        stub: Arc<Stub>,
    ) -> (
        super::super::thread::FaultSink,
        tokio::sync::watch::Receiver<LinkState>,
    ) {
        let (sink, source) = fault_channel();
        let (tx, rx) = tokio::sync::watch::channel(LinkState::Disconnected);
        tokio::spawn(run(stub, source, tx));
        (sink, rx)
    }

    /// Waits for the state to satisfy `wanted`, or panics with what it actually saw.
    async fn wait_for(
        rx: &mut tokio::sync::watch::Receiver<LinkState>,
        what: &str,
        wanted: impl Fn(&LinkState) -> bool,
    ) -> LinkState {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            {
                let current = rx.borrow_and_update().clone();
                if wanted(&current) {
                    return current;
                }
            }
            tokio::select! {
                changed = rx.changed() => changed.expect("the recovery loop is still running"),
                () = tokio::time::sleep_until(deadline) => {
                    panic!("never reached {what}; stuck at {:?}", rx.borrow().clone())
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedge_abandons_the_thread_rebuilds_and_reports_connected() {
        // The whole protocol in one test: SDD §5.3.1's "abandon, respawn, reconnecting →
        // connected". The measured recovery is 108 ms on one attempt, so one rebuild is not a
        // lucky number, it is the expected one.
        let stub = Stub::new(0, GONE);
        let (faults, mut state) = start(Arc::clone(&stub));

        faults
            .send(LinkFault::Wedged {
                operation: "capture",
                budget: Duration::from_secs(60),
            })
            .expect("the loop is listening");

        wait_for(&mut state, "connected", LinkState::is_connected).await;
        assert_eq!(stub.rebuilds(), 1, "the measured path is one attempt");
        assert_eq!(
            stub.tear_downs(),
            vec![true],
            "a wedged thread must be abandoned, never joined — it is inside a C call"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_vanished_device_is_shut_down_properly_rather_than_abandoned() {
        // The difference that matters: the thread is healthy, only the camera left. Abandoning a
        // healthy thread would leak it *and* leave its USB claim in place for the attempt that
        // is about to need the device.
        let stub = Stub::new(0, GONE);
        let (faults, mut state) = start(Arc::clone(&stub));

        faults
            .send(LinkFault::DeviceGone {
                detail: GONE.to_owned(),
            })
            .expect("the loop is listening");

        wait_for(&mut state, "connected", LinkState::is_connected).await;
        assert_eq!(stub.tear_downs(), vec![false]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_reconnecting_state_names_the_cable_when_the_device_is_gone() {
        // Branch (a) of the two measured error strings. The message must send the operator to the
        // cable and the battery — never to a gvfs mount, which cannot be holding a device that is
        // not on the bus.
        let stub = Stub::new(u32::MAX, GONE);
        let (faults, mut state) = start(Arc::clone(&stub));

        faults
            .send(LinkFault::DeviceGone {
                detail: GONE.to_owned(),
            })
            .expect("the loop is listening");

        let reconnecting = wait_for(&mut state, "reconnecting", |s| {
            matches!(s, LinkState::Reconnecting { .. })
        })
        .await;
        let message = reconnecting
            .message()
            .expect("a reconnecting state explains itself");
        assert!(message.contains("cable"), "{message}");
        assert!(message.contains("battery"), "{message}");
        assert!(
            !message.contains("gvfs"),
            "an unplugged camera is not being held by a desktop mount: {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_claim_branch_names_gvfs_and_gives_up_rather_than_retrying_forever() {
        // Branch (b), and the REL-03 guard. The spike measured gvfs blocking recovery for eighty
        // seconds after a replug; a loop that retried silently forever would leave an operator
        // watching a stalled sequence with nothing to act on.
        let stub = Stub::new(u32::MAX, CLAIMED);
        let (faults, mut state) = start(Arc::clone(&stub));

        faults
            .send(LinkFault::ClaimLost {
                detail: CLAIMED.to_owned(),
            })
            .expect("the loop is listening");

        let faulted = wait_for(&mut state, "faulted", |s| {
            matches!(s, LinkState::Faulted { .. })
        })
        .await;
        let reason = faulted.message().expect("a fault explains itself");

        // libgphoto2's own text survives, because that is what a bug report is searched for...
        assert!(reason.contains(CLAIMED), "{reason}");
        // ...and the operator is told what to do next. Which half of the diagnosis fires depends
        // on whether *this* machine has a camera gvfs-mounted, so assert the property both
        // branches share, exactly as the M2-T02 test of the same message does.
        assert!(
            reason.contains("gio mount -u") || reason.contains("switched off"),
            "the message names no next step: {reason}"
        );
        assert_eq!(
            stub.rebuilds(),
            MAX_ATTEMPTS,
            "it must try the bounded number of times and then stop"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_camera_that_comes_back_late_still_comes_back() {
        // Two failures then success — a replug that took a moment to enumerate. The point is that
        // the loop does not give up before its bound, and that the state ends `Connected` rather
        // than stuck on the last `Reconnecting`.
        let stub = Stub::new(2, GONE);
        let (faults, mut state) = start(Arc::clone(&stub));

        faults
            .send(LinkFault::DeviceGone {
                detail: GONE.to_owned(),
            })
            .expect("the loop is listening");

        wait_for(&mut state, "connected", LinkState::is_connected).await;
        assert_eq!(stub.rebuilds(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn one_dead_link_produces_one_recovery_cycle_however_many_faults_it_posts() {
        // A live-view loop at 5 fps meets the same dead camera five times a second. Each of those
        // is a fault, and running a recovery cycle per fault would rebuild the link repeatedly
        // while the first rebuild was still in flight.
        let stub = Stub::new(0, GONE);
        let (faults, mut state) = start(Arc::clone(&stub));

        for _ in 0..20 {
            faults
                .send(LinkFault::DeviceGone {
                    detail: GONE.to_owned(),
                })
                .expect("the loop is listening");
        }

        wait_for(&mut state, "connected", LinkState::is_connected).await;
        // Let anything that was going to fire a second cycle do so.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            stub.rebuilds(),
            1,
            "twenty faults about one dead link are one recovery, not twenty"
        );
    }

    #[test]
    fn the_backoff_doubles_to_a_ceiling_and_resets_after_a_success() {
        // T13's lesson, asserted rather than described: without the reset, one bad night leaves
        // the backoff at the ceiling permanently and the next cable knock costs fifteen seconds
        // that should have cost one.
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next_delay(), BACKOFF_BASE);
        assert_eq!(backoff.next_delay(), BACKOFF_BASE * 2);
        assert_eq!(backoff.next_delay(), BACKOFF_BASE * 4);
        for _ in 0..10 {
            assert!(backoff.next_delay() <= BACKOFF_CEILING);
        }
        assert_eq!(backoff.next_delay(), BACKOFF_CEILING, "it stops at the cap");

        backoff.reset();
        assert_eq!(backoff.next_delay(), BACKOFF_BASE);
    }

    #[test]
    fn only_a_wedge_abandons_the_thread() {
        assert!(LinkFault::Wedged {
            operation: "capture",
            budget: Duration::from_secs(1)
        }
        .abandons_the_thread());
        assert!(!LinkFault::DeviceGone {
            detail: String::new()
        }
        .abandons_the_thread());
        assert!(!LinkFault::ClaimLost {
            detail: String::new()
        }
        .abandons_the_thread());
    }

    #[test]
    fn a_usb_reset_is_only_wanted_for_a_device_that_says_it_is_gone() {
        // The escalation must not fire on a claim conflict: re-enumeration is the hotplug event
        // that invites gvfs to take the camera, so resetting there makes the measured failure
        // *more* likely, not less.
        assert!(super::wants_a_usb_reset(&DeviceError::Transport(
            GONE.to_owned()
        )));
        assert!(!super::wants_a_usb_reset(&DeviceError::Transport(
            CLAIMED.to_owned()
        )));
        assert!(!super::wants_a_usb_reset(&DeviceError::NotConnected));
        assert!(!super::wants_a_usb_reset(&DeviceError::Rejected(
            "no card in the camera".to_owned()
        )));
    }

    #[test]
    fn the_attempt_ladder_reaches_a_diagnosis_inside_the_acceptance_window() {
        // T-CAM-1 gives recovery 30 s. The success path is one attempt at a measured 108 ms, so
        // this is about the *other* end: an operator whose camera is not coming back must be told
        // why on roughly the same timescale rather than at dawn.
        let mut backoff = Backoff::new();
        let mut total = Duration::ZERO;
        for _ in 1..MAX_ATTEMPTS {
            total += backoff.next_delay();
        }
        assert!(
            total <= Duration::from_secs(35),
            "the ladder waits {total:?} before faulting, which is too long to say nothing for"
        );
    }
}
