//! The mount interface — SDD §5.1 (signatures verbatim), PRD §4.1, HAL-02.
//!
//! Read the crate-level "contract every device trait shares" first: `&self` under concurrent
//! callers, no blocking, dropping a future never stops motion, `DeviceError` only, no events,
//! honest capabilities, idempotent connect/disconnect.
//!
//! One thing is specific to this trait and worth stating on its own: **every method here can
//! move a telescope**, and the layer that decides whether it may is `astroctl-safety`'s
//! `SafeMount`, which implements this same trait and wraps the driver (ADR-11, SDD §5.4). A
//! driver therefore performs no limit checking of its own — it must not, or a mount would refuse
//! a slew for a reason the safety layer cannot explain to the operator — but it must also not
//! assume it is being called through the wrapper. Mechanical limits the *device* enforces
//! (hard stops, cable wrap) stay the driver's business and surface as
//! [`DeviceError::Rejected`](astroctl_core::error::DeviceError::Rejected).

use std::fmt::Debug;

use astroctl_core::error::DeviceError;
use astroctl_core::types::{
    Axis, DeviceInfo, Direction, GuideRate, MountCapabilities, MountStatus, MountTravel, RaDec,
    SlewSpeed, TrackingMode,
};
use async_trait::async_trait;

/// An equatorial mount (SDD §5.1, PRD §4.1, HAL-02).
///
/// `Debug` is a supertrait so that the wrappers which hold an `Arc<dyn MountDevice>` — the
/// safety wrapper, the mount facade, the app state — can derive it; without it every one of them
/// either writes a `Debug` impl by hand or drops out of the structured log entirely, and this is
/// a system whose logging convention (SDD §2) is that state is inspectable. Implementors with an
/// opaque field (a serial handle, a C context) write a one-line manual impl.
#[async_trait]
pub trait MountDevice: Debug + Send + Sync {
    /// Opens the transport and brings the mount to a usable state — the handshake that reads
    /// firmware version, counts per revolution and timer frequency (SDD §5.2.2), and initialises
    /// the axes.
    ///
    /// Startup never calls this (SDD §8.1: "registry builds drivers, no connect"): connecting is
    /// an operator action, so that switching the field node on cannot produce surprise motion.
    ///
    /// # Errors
    /// `Transport` if the port cannot be opened, `Timeout` if the device does not answer the
    /// handshake, `Protocol` if it answers with something unintelligible. Connecting an
    /// already-connected mount is `Ok(())`.
    async fn connect(&self) -> Result<(), DeviceError>;

    /// Closes the transport.
    ///
    /// **Does not stop the mount.** SDD §7 makes this explicit for the shutdown path: a service
    /// restart mid-session must leave a tracking mount tracking, because the mount is safe while
    /// tracking and a stopped one has lost the target. Callers that want motion stopped stop it
    /// first.
    ///
    /// Must succeed even from a faulted link — this is also the shutdown path, so a driver that
    /// can no longer talk to the mount still releases the port. Disconnecting an already
    /// disconnected mount is `Ok(())`.
    ///
    /// # Errors
    /// Rarely fails; a driver that cannot release its transport cleanly returns `Transport`.
    async fn disconnect(&self) -> Result<(), DeviceError>;

    /// The current sky position.
    ///
    /// Polled at 1 Hz by the facade and published as `mount.position` (MNT-02, SDD §4.3), so it
    /// is the most frequently called method in the system and must be cheap — one exchange with
    /// the device, or a cached value no older than the poll interval. The conversion from
    /// mechanical counters to RA/DEC needs local sidereal time and is the driver's job
    /// (SDD §5.2.3); nothing above the HAL sees axis counts.
    ///
    /// # Errors
    /// `NotConnected`, or `Timeout`/`Transport`/`Protocol` from the exchange.
    async fn position(&self) -> Result<RaDec, DeviceError>;

    /// Coarse state plus the three flags the UI reads (SDD §4.3 `mount.status`).
    ///
    /// The returned value must satisfy [`MountStatus::is_consistent`] — `state` and the flags
    /// describe the same instant, and a status that contradicts itself makes the UI and the
    /// session log disagree about what happened.
    ///
    /// # Errors
    /// `NotConnected`, or a transport-level failure. A driver that is connected but has lost
    /// its heartbeat reports [`MountState::Fault`](astroctl_core::types::MountState::Fault)
    /// rather than erroring — "the mount is not answering" is a state the operator must see,
    /// not an error that makes the status panel go blank.
    async fn status(&self) -> Result<MountStatus, DeviceError>;

    /// Slews to `target` and resolves when the mount has arrived and settled.
    ///
    /// Completion means *ready for the next command*: motion finished, the settle interval
    /// elapsed, and tracking restored if it was running when the goto began (SES-06,
    /// SDD §5.2.3). A caller that gets `Ok(())` may start an exposure.
    ///
    /// **Cancel-safe in the specific sense of rule 3: dropping this future does not stop the
    /// slew.** The mount keeps moving to `target`; only the waiting stops. A caller that wants
    /// the motion abandoned calls [`stop_slew`](Self::stop_slew) or
    /// [`emergency_stop`](Self::emergency_stop) — and callers do drop this future routinely,
    /// because the API answers `202` and reports progress over the WS rather than holding the
    /// request open for a two-minute slew (SDD §5.8.1).
    ///
    /// A second goto while one is in flight is
    /// [`DeviceError::Busy`](astroctl_core::error::DeviceError::Busy), never a silently
    /// retargeted slew.
    ///
    /// # Errors
    /// `Busy` if already slewing, `NotConnected`, `Rejected` if the device refuses the target
    /// (a mechanical limit), `Timeout`/`Transport`/`Protocol` from the exchange. A goto that
    /// fails mid-motion must leave the mount stopped, not drifting toward a target nobody is
    /// tracking.
    ///
    /// **A goto that *started* and was then stopped by something else — an emergency stop, a
    /// safety limit, an operator stop — is [`DeviceError::Aborted`](astroctl_core::error::DeviceError::Aborted),
    /// not `Rejected`.** The distinction reaches the operator: `Rejected` is
    /// `DEVICE_REJECTED`/422 ("your request was bad"), `Aborted` is `ABORTED`/409 ("something
    /// stopped the mount"). Answering 422 for a working e-stop is the defect this variant was
    /// added to fix (M1-T02 handoff → M1-T03).
    ///
    /// Callers should still not switch on the variant to learn where the mount is. Any error
    /// means the slew did not complete; [`status`](Self::status) is what says what it is doing
    /// now.
    async fn goto(&self, target: RaDec) -> Result<(), DeviceError>;

    /// Tells the mount that it is currently pointing at `pos`, without moving.
    ///
    /// This is the plate-solve correction path (Phase 2a) and the manual alignment path. It
    /// changes the mount's model of where it is, so a driver must apply it to both axes
    /// atomically as far as the protocol allows — a half-applied sync is a pointing model that
    /// is wrong in a way nothing downstream can detect.
    ///
    /// # Errors
    /// `Busy` while slewing (syncing mid-motion is meaningless), `NotConnected`, `Rejected`,
    /// or a transport-level failure.
    async fn sync(&self, pos: RaDec) -> Result<(), DeviceError>;

    /// Starts tracking at `mode`'s rate, or changes the rate if already tracking.
    ///
    /// # Errors
    /// `Unsupported` if `mode` is not in
    /// [`MountCapabilities::tracking_rates`](astroctl_core::types::MountCapabilities), plus the
    /// usual `NotConnected`/transport failures.
    async fn start_tracking(&self, mode: TrackingMode) -> Result<(), DeviceError>;

    /// Stops tracking. Idempotent — stopping an idle mount is `Ok(())`.
    ///
    /// This is a stopping command, so it is never staleness-rejected upstream (SDD §5.8.1): a
    /// late stop is safe. Implementations should treat it as always worth executing.
    ///
    /// # Errors
    /// `NotConnected` or a transport-level failure.
    async fn stop_tracking(&self) -> Result<(), DeviceError>;

    /// Starts a continuous manual slew on one axis and returns as soon as the mount has been
    /// commanded — *not* when the motion ends.
    ///
    /// The motion continues until [`stop_slew`](Self::stop_slew), an
    /// [`emergency_stop`](Self::emergency_stop), or the safety wrapper's TTL watcher stops it.
    /// That watcher is why this method returns early: manual slew is a dead-man's switch, the
    /// operator holds a D-pad and the authorization is renewed every few hundred milliseconds
    /// (SDD §5.8.1), so an API call that blocked for the duration of the motion would be the
    /// wrong shape entirely.
    ///
    /// Repeating the call with identical parameters while that axis is already slewing that way
    /// must be a no-op at the device — the renewal path calls it on every repeat and re-issuing
    /// motor commands at 2 Hz would make the mount stutter.
    ///
    /// # Errors
    /// `Busy` if a goto owns the axis, `Unsupported` for a speed the mount does not have,
    /// `NotConnected`, or a transport-level failure.
    async fn slew(&self, axis: Axis, dir: Direction, speed: SlewSpeed) -> Result<(), DeviceError>;

    /// Stops manual slewing on one axis, with the mount's ramped stop.
    ///
    /// Idempotent, and safe to call on an axis that is not moving. This is the primary stop for
    /// manual slew — the TTL expiry is the backstop, not the other way round (SDD §5.8.1).
    ///
    /// # Errors
    /// `NotConnected` or a transport-level failure. Note the asymmetry with `slew`: a driver
    /// should try harder here, because the caller cannot make a stop "more urgent" except by
    /// escalating to [`emergency_stop`](Self::emergency_stop).
    async fn stop_slew(&self, axis: Axis) -> Result<(), DeviceError>;

    /// Moves one axis for `duration_ms` at `rate` — the guiding correction primitive (MNT-12).
    ///
    /// `rate` is a fraction of the sidereal rate, and the driver programs it before issuing the
    /// pulse (Synta `P`, SDD §5.1/§5.2.2). Devices whose rate is fixed return `Unsupported` for
    /// any value other than their own rather than pulsing at the wrong rate — a guide loop
    /// calibrated against a rate the mount ignored produces corrections that are wrong by a
    /// constant factor and look like poor seeing.
    ///
    /// Resolves when the pulse has been *issued and completed*, so a guide loop can pace itself
    /// on it. Milliseconds rather than a `Duration` is SDD §5.1 verbatim, and matches both the
    /// wire encoding and the units a guide loop computes in.
    ///
    /// # Errors
    /// `Unsupported` if [`MountCapabilities::has_pulse_guide`](astroctl_core::types::MountCapabilities)
    /// is false or `rate` is not settable, `NotConnected`, or a transport-level failure.
    async fn guide_pulse(
        &self,
        axis: Axis,
        dir: Direction,
        duration_ms: u32,
        rate: GuideRate,
    ) -> Result<(), DeviceError>;

    /// Slews to the park position and resolves when parked.
    ///
    /// Tracking is off when this resolves; the park position itself comes from configuration
    /// (`mount.park_position`), so it is a constructor input, not a parameter here.
    ///
    /// # Errors
    /// `Busy`, `NotConnected`, or a transport-level failure.
    async fn park(&self) -> Result<(), DeviceError>;

    /// Releases the park state so motion commands are accepted again. Does not start tracking.
    ///
    /// # Errors
    /// `NotConnected` or a transport-level failure.
    async fn unpark(&self) -> Result<(), DeviceError>;

    /// Stops all motion immediately, by the most direct means the device has.
    ///
    /// **This is the safety path and it has a budget.** SDD §5.8.2 allows ≤ 20 ms from the API
    /// handler to bytes on the wire, which means an implementation must not queue behind normal
    /// traffic: the driver needs a priority lane to whatever owns the transport (SDD §5.2.4).
    /// It must be callable while any other operation is in flight, must never answer `Busy`,
    /// and must be idempotent — the operator will press the button twice.
    ///
    /// Instant stop rather than a ramped one (Synta `L`, not `K`): at low rates the two are
    /// measurably identical, but at 800× sidereal the ramp is the difference between a
    /// telescope that stops and one that keeps going for a second (SDD §5.2.4).
    ///
    /// # Errors
    /// Only transport-level failures, and even then a driver should attempt every axis before
    /// returning — a partial stop reported as a failure is still better than an early return
    /// that leaves one axis running.
    async fn emergency_stop(&self) -> Result<(), DeviceError>;

    /// What this mount can do (HAL-05).
    ///
    /// Synchronous, must not perform I/O and must work while disconnected: the registry builds
    /// the driver at startup and `/api/system/info` reports capabilities before anyone has
    /// connected (SDD §5.8.1, §8.1). Values learned from the device at connect (counts per
    /// revolution, for instance) may sharpen afterwards, but the *set* of supported operations
    /// may not change — the UI decides what to render from this.
    fn capabilities(&self) -> MountCapabilities;

    /// How far each axis has been driven from the mount's mechanical home pose, as of the last
    /// counter read — or `None` from a mount that has no home reference (M3-T07).
    ///
    /// # This is a measurement, not a limit
    ///
    /// The module docs above say a driver does no limit checking, and this does not change that.
    /// A driver *reports* travel exactly as it reports [`position`](Self::position); the rule
    /// that bounds it lives in `astroctl-safety`'s `SafeMount`, next to altitude and the meridian,
    /// because that is where a configured threshold belongs and — decisively — because the wrapper
    /// owns the only thing that runs *during* a hold. Both slew paths short-circuit an identical
    /// renewal, so a check made when the operator presses the D-pad never runs again while they
    /// keep pressing it; the wrapper's 2 Hz watch is what catches an axis winding.
    ///
    /// # Synchronous, and honest about being a cache
    ///
    /// No I/O, like [`capabilities`](Self::capabilities): the value is whatever the driver learned
    /// on its last counter read. Making it an exchange would double the wire traffic of a watch
    /// that already calls `position()` at 2 Hz, to learn something that cannot have changed since
    /// — and every caller in the system reads a position immediately before asking.
    ///
    /// # Why `Option` rather than a required answer
    ///
    /// `pier_side` was deliberately *not* put on this trait, because widening it for one driver
    /// would make every other implementor answer a question it may not have. The same objection
    /// applies here and `Option` is the answer to it: a mount with no absolute home — an INDI or
    /// Alpaca device, the simulator — says so, and the limit above simply has nothing to enforce.
    /// It is on the trait rather than beside it because ADR-11 puts enforcement below the API so
    /// that it cannot be bypassed, and a channel the binary has to remember to wire is a channel
    /// the binary will one day forget.
    fn axis_travel(&self) -> Option<MountTravel>;

    /// Human-readable identity for logs, the UI and calibration-profile tagging (HAL-06).
    ///
    /// Synchronous and callable while disconnected, same as `capabilities`. Fields the device
    /// only reports once connected — firmware version in particular — are `None` until then;
    /// that is why they are `Option` in
    /// [`DeviceInfo`](astroctl_core::types::DeviceInfo).
    fn device_info(&self) -> DeviceInfo;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astroctl_core::types::{MountState, RaDec, TrackingMode};

    use super::MountDevice;
    use crate::test_double::FakeMount;

    /// The shape every consumer above the HAL has: a struct holding the device behind a trait
    /// object, deriving `Debug`. `SafeMount` (SDD §5.4) and the mount facade (§5.8) are both
    /// this, and neither compiles if `MountDevice` loses its `Debug` supertrait.
    #[derive(Debug)]
    struct Facade {
        mount: Arc<dyn MountDevice>,
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_device_serves_concurrent_callers_through_arc_dyn() {
        let fake = Arc::new(FakeMount::new());
        let facade = Facade {
            mount: fake.clone() as Arc<dyn MountDevice>,
        };
        facade.mount.connect().await.expect("connects");

        // The 1 Hz poll, an API handler and a watchdog, concurrently — the arrangement SDD §7
        // describes and the reason every method takes `&self`.
        let poll = {
            let mount = Arc::clone(&facade.mount);
            tokio::spawn(async move {
                for _ in 0..10 {
                    mount.position().await.expect("polls");
                }
            })
        };
        let command = {
            let mount = Arc::clone(&facade.mount);
            tokio::spawn(async move {
                mount
                    .start_tracking(TrackingMode::Sidereal)
                    .await
                    .expect("tracks");
            })
        };
        let watchdog = {
            let mount = Arc::clone(&facade.mount);
            tokio::spawn(async move {
                // Whatever it sees, it must see *something* — the point is that a status read
                // concurrent with a command is legal, not what it returns mid-flight.
                mount.status().await.expect("reads status");
            })
        };

        poll.await.expect("poll task");
        command.await.expect("command task");
        watchdog.await.expect("watchdog task");

        let status = facade.mount.status().await.expect("final status");
        assert_eq!(status.state, MountState::Tracking);
        assert!(status.is_consistent());
        assert_eq!(
            fake.log()
                .iter()
                .filter(|call| call.as_str() == "position")
                .count(),
            10,
            "every concurrent poll reached the device"
        );
    }

    #[tokio::test]
    async fn dropping_a_goto_future_leaves_the_mount_slewing() {
        // Rule 3, asserted rather than asserted-in-a-doc-comment: the API drops this future on
        // every goto (it answers 202), and if that stopped the mount nothing would ever slew.
        let fake = Arc::new(FakeMount::new());
        fake.connect().await.expect("connects");

        let target = RaDec::from_parts(5.5, 22.0).expect("valid target");
        // `select!` takes the future by value and drops the losing branch when it returns —
        // which is precisely the abandonment being tested, and precisely what a client
        // disconnecting mid-request does to the handler holding this future.
        tokio::select! {
            biased;
            () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            _ = fake.goto(target) => panic!("the fake slew is slower than this"),
        }

        assert!(
            fake.status().await.expect("status").slewing,
            "a dropped future must not have stopped the slew"
        );
        // ...and stopping is explicit.
        fake.emergency_stop().await.expect("stops");
        assert!(!fake.status().await.expect("status").slewing);
    }

    #[tokio::test]
    async fn identity_and_capabilities_are_readable_before_connecting() {
        // SDD §8.1: the registry builds drivers at startup and `/api/system/info` reports them
        // long before an operator presses Connect.
        let fake = FakeMount::new();
        assert!(fake.capabilities().has_pulse_guide);
        assert_eq!(fake.device_info().protocol, "fake");
        assert!(
            fake.device_info().firmware.is_none(),
            "firmware is learned at connect, so it is None until then"
        );
        assert!(matches!(
            fake.position().await,
            Err(astroctl_core::error::DeviceError::NotConnected)
        ));
    }
}
