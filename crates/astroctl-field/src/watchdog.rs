//! The watchdogs — SDD §8.1 ("watchdogs on → health `ok`"), §4.3 `system.health`, §5.4, REL-02/12.
//!
//! Two arms live here, on two clocks:
//!
//! * **Disk and clock**, at the §4.3 `system.health` cadence of 60 s. It does not depend on
//!   hardware and was implemented in full at M0, because it is what the `starting` → `ok`
//!   transition of §8.1 is actually waiting for: a node reports `ok` when something is watching
//!   it, not when its socket is open. [`run`] owns it.
//! * **The mount's link**, at the mount facade's poll cadence. [`MountLink`] owns it, and
//!   `mount::poll` drives it — see that type's own note for why it is not on this file's ticker.
//!
//! Alerts are **edge-triggered**. A disk that has been low for six hours is one alert, not 360 —
//! SDD §5.10.4 makes the same point about an offline stack node ("one offline alert not
//! thousands"), and an operator who learns to ignore a repeating alert has lost the alert. The
//! mount arm obeys the same rule for the same reason, and the rule is the reason both arms are
//! written as state machines over "what changed" rather than as tests of a current condition.
//!
//! What is still missing: USB presence, and the camera thread's liveness. Both wait on a device
//! that can lose one.

use std::time::Duration;

use astroctl_core::bus::EventBus;
use astroctl_core::config::StorageConfig;
use astroctl_core::error::ErrorCode;
use astroctl_core::event::{Alert, SystemHealth};

use crate::vitals;

/// Publication cadence of `system.health` (SDD §4.3: every 60 s).
const INTERVAL: Duration = Duration::from_secs(60);

/// Alert code for free space between the warning and critical thresholds.
///
/// A free string rather than an [`ErrorCode`]: SDD §4.2's closed enum has `DISK_FULL` for the
/// critical case and nothing for the warning, and adding a code is a frozen-contract change, not
/// something an implementation task invents (tasks/README rule 2). See the M0-T05 result note.
const DISK_LOW: &str = "DISK_LOW";

/// Alert code for an undisciplined system clock (REL-14). Same situation as [`DISK_LOW`].
const CLOCK_UNSYNCED: &str = "CLOCK_UNSYNCED";

/// What the last tick saw, so only changes are announced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    disk: Option<DiskLevel>,
    clock_synced: Option<bool>,
}

/// Free space against the REL-12 thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiskLevel {
    /// Above `disk_warn_free_gb`.
    Ok,
    /// Between the warning and critical thresholds.
    Low,
    /// Below `disk_critical_free_gb` — capture pauses after the in-flight frame.
    Critical,
}

impl DiskLevel {
    fn of(free_gb: f64, storage: &StorageConfig) -> Self {
        if free_gb < storage.disk_critical_free_gb {
            Self::Critical
        } else if free_gb < storage.disk_warn_free_gb {
            Self::Low
        } else {
            Self::Ok
        }
    }
}

/// Run the watchdog until the task is dropped or aborted.
///
/// The first tick fires immediately: an operator who starts the node with a full disk should be
/// told now, not in a minute.
pub async fn run(bus: EventBus, storage: StorageConfig, uptime: vitals::Uptime) {
    let mut ticker = tokio::time::interval(INTERVAL);
    let mut seen = Seen::default();
    loop {
        ticker.tick().await;
        seen = tick(&bus, &storage, uptime, seen);
    }
}

/// One observation: publish `system.health`, and announce anything that changed.
fn tick(bus: &EventBus, storage: &StorageConfig, uptime: vitals::Uptime, seen: Seen) -> Seen {
    let free_gb = vitals::disk_free_gb(&storage.sessions_dir);
    let clock_synced = vitals::clock_synced();

    bus.publish(SystemHealth::new(
        // The event schema (SDD §4.3) types `disk_free_gb` as a number with no "unknown", so an
        // unreadable volume is published as 0.0 *and* alerted on below — the conservative
        // reading, since the alternative is a silently absent measurement.
        free_gb.unwrap_or(0.0),
        clock_synced,
        uptime.seconds(),
    ));

    let disk = free_gb.map(|gb| DiskLevel::of(gb, storage));
    if disk != seen.disk {
        match (disk, free_gb) {
            (Some(DiskLevel::Critical), Some(gb)) => {
                bus.publish(Alert::critical(
                    ErrorCode::DiskFull.as_str(),
                    format!(
                        "{gb:.1} GB free on {} — below the critical threshold of {} GB; \
                         capture pauses after the in-flight frame (REL-12)",
                        storage.sessions_dir.display(),
                        storage.disk_critical_free_gb
                    ),
                ));
            }
            (Some(DiskLevel::Low), Some(gb)) => {
                bus.publish(Alert::warning(
                    DISK_LOW,
                    format!(
                        "{gb:.1} GB free on {} — below the warning threshold of {} GB (REL-12)",
                        storage.sessions_dir.display(),
                        storage.disk_warn_free_gb
                    ),
                ));
            }
            (Some(DiskLevel::Ok), Some(gb)) => {
                // Only after a previous complaint; `seen.disk` is `None` on the first tick.
                if seen.disk.is_some() {
                    bus.publish(Alert::info(
                        DISK_LOW,
                        format!("{gb:.1} GB free — free space is back above the thresholds"),
                    ));
                }
            }
            (None, _) => {
                bus.publish(Alert::warning(
                    DISK_LOW,
                    format!(
                        "cannot determine free space on {} — check that the path exists and is \
                         mounted",
                        storage.sessions_dir.display()
                    ),
                ));
            }
            // `disk` is `Some` exactly when `free_gb` is; unreachable, and cheaper to ignore
            // than to make representable.
            (Some(_), None) => {}
        }
    }

    if seen.clock_synced != Some(clock_synced) {
        if clock_synced {
            if seen.clock_synced.is_some() {
                bus.publish(Alert::info(
                    CLOCK_UNSYNCED,
                    "system clock is disciplined again",
                ));
            }
        } else {
            bus.publish(Alert::warning(
                CLOCK_UNSYNCED,
                "the system clock is not disciplined by a time source; frame timestamps and \
                 sidereal tracking depend on it (REL-14)",
            ));
        }
    }

    Seen {
        disk,
        clock_synced: Some(clock_synced),
    }
}

// ---------------------------------------------------------------------------------------------
// The mount's link — REL-02, SDD §5.4
// ---------------------------------------------------------------------------------------------

/// What one poll cycle learned about the link to the mount.
///
/// Three answers rather than two, because "the poll got nothing" and "nothing was expected to
/// answer" are the difference between an alert and silence: an operator who pressed Disconnect
/// has not lost their mount, and a node that alerted on their own action would teach them to
/// ignore the alert that matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkSample {
    /// The mount is not connected. Nobody is expected to answer, and nothing is wrong.
    Standby,
    /// The mount answered this poll.
    Reached,
    /// The poll did not reach the mount, though the node believes it is connected.
    Missed {
        /// Whether the axes were moving the last time the node could read a status.
        ///
        /// The last time, not now — by definition the mount is not answering, so this cannot be
        /// re-read at the moment it matters. It is carried on the sample rather than remembered
        /// here because the poll loop already tracks the last status it read, and one copy of
        /// that fact is better than two that can disagree.
        slewing: bool,
    },
}

/// The consecutive-miss counter behind [`ErrorCode::MountLinkLost`] (REL-02).
///
/// # Why this is not on the 60 s ticker above
///
/// The disk arm *asks* a question on its own clock; this one can only *listen*. The evidence of a
/// lost link is a poll that failed, and the only place that sees a poll's whole verdict is the
/// poll itself — a status read that errored, or a status that came back from the driver's cache
/// with a position read that errored behind it. A second task on its own clock could observe
/// neither without the poll loop telling it, so the arm is a state machine the poll loop feeds
/// and this file keeps the alert discipline the rest of the watchdogs are written to.
///
/// # The M3-T02 seam
///
/// `skywatcher::serial::watchdog_channel()` already produces `Heartbeat::Lost { misses }` and
/// `Heartbeat::Recovered` from the driver, counted across *every* exchange rather than only the
/// 1 Hz poll — a strictly better signal, and one that can tell "the port is gone" from "the mount
/// is mute". When it is wired up it replaces the samples below: `Lost` is the transition this
/// counter exists to detect and `Recovered` is [`LinkSample::Reached`] after one, so the driver
/// feeds the same arm and the operator keeps getting exactly one alert. It is deliberately not a
/// second consumer publishing a second code — two watchdogs on one link is how an operator ends
/// up with two alerts for one cable.
#[derive(Clone, Copy, Debug)]
pub struct MountLink {
    /// `mount.serial.heartbeat_misses`, floored at 1.
    threshold: u32,
    /// Consecutive misses so far.
    misses: u32,
    /// Whether the loss has already been announced — the edge trigger.
    lost: bool,
}

impl MountLink {
    /// An arm that fires after `heartbeat_misses` consecutive misses.
    #[must_use]
    pub fn new(heartbeat_misses: u32) -> Self {
        Self {
            // The config validator's range is 1–100, so this only bites if a caller builds a
            // `MountConfig` by hand. A zero would otherwise mean "declare the link lost having
            // observed nothing", which is the one reading of the key nobody wants.
            threshold: heartbeat_misses.max(1),
            misses: 0,
            lost: false,
        }
    }

    /// Fold one poll's verdict in, and return the alert it makes true — at most one, and only on
    /// a transition.
    ///
    /// Returning the alert rather than publishing it keeps this a pure function of the samples it
    /// has seen, which is what lets a test count polls without a clock, a bus or a mount, and what
    /// makes the M3-T02 swap above a change of *source* rather than of behaviour.
    pub fn observe(&mut self, sample: LinkSample) -> Option<Alert> {
        match sample {
            LinkSample::Reached => {
                self.misses = 0;
                // Only after a complaint — the same rule the disk arm's recovery follows, and the
                // reason a healthy night is silent rather than announcing hourly that the cable
                // is still plugged in.
                self.lost.then(|| {
                    self.lost = false;
                    Alert::info(
                        ErrorCode::MountLinkLost.as_str(),
                        "the mount is answering again",
                    )
                })
            }
            LinkSample::Missed { slewing } => {
                self.misses += 1;
                if self.misses < self.threshold || self.lost {
                    return None;
                }
                self.lost = true;
                Some(Alert::critical(
                    ErrorCode::MountLinkLost.as_str(),
                    format!(
                        "the mount has not answered {} consecutive polls{} — check the cable at \
                         the mount and its power (REL-02)",
                        self.misses,
                        if slewing {
                            // The sentence that changes what the operator does next. A loss at
                            // idle is a thing to fix before the next target; a loss mid-slew is a
                            // tube still moving under a mount that has stopped listening, because
                            // nothing about an unplugged cable stops a motor.
                            ". The mount was slewing when contact was lost, so the telescope may \
                             still be moving"
                        } else {
                            ""
                        }
                    ),
                ))
            }
            LinkSample::Standby => {
                // A deliberate disconnect is not a link loss, and the streak that led up to it is
                // no longer evidence of anything.
                self.misses = 0;
                // `lost` deliberately survives. An operator who unplugs after a loss has not
                // fixed it, and clearing the flag here would mean the reconnect that *does* fix
                // it passes in silence, leaving the critical alert standing with no answer.
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astroctl_core::bus::Recv;
    use astroctl_core::event::Topic;
    use std::path::PathBuf;

    fn storage(warn: f64, critical: f64) -> StorageConfig {
        StorageConfig {
            sessions_dir: PathBuf::from("."),
            disk_warn_free_gb: warn,
            disk_critical_free_gb: critical,
        }
    }

    async fn drain(sub: &mut astroctl_core::bus::EventSubscriber) -> Vec<Topic> {
        let mut topics = Vec::new();
        while let Ok(Recv::Event(event)) =
            tokio::time::timeout(Duration::from_millis(50), sub.recv()).await
        {
            topics.push(event.topic);
        }
        topics
    }

    /// A starting point that has already seen this machine's clock state, so the disk assertions
    /// are not perturbed by whether the test host runs systemd-timesyncd.
    fn clock_already_seen() -> Seen {
        Seen {
            disk: None,
            clock_synced: Some(vitals::clock_synced()),
        }
    }

    #[tokio::test]
    async fn every_tick_publishes_system_health() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        let uptime = vitals::Uptime::started_now();
        // Thresholds far below any real volume: no alerts, just the vitals.
        let seen = tick(&bus, &storage(0.001, 0.0001), uptime, clock_already_seen());
        assert_eq!(drain(&mut sub).await, vec![Topic::SystemHealth]);
        assert_eq!(seen.disk, Some(DiskLevel::Ok));
    }

    #[tokio::test]
    async fn a_critical_disk_alerts_once_not_on_every_tick() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        let uptime = vitals::Uptime::started_now();
        // Thresholds above any plausible free space: always critical.
        let storage = storage(1e9, 1e8);

        let seen = tick(&bus, &storage, uptime, clock_already_seen());
        assert_eq!(
            drain(&mut sub).await,
            vec![Topic::SystemHealth, Topic::Alert],
            "the first observation alerts"
        );

        let mut sub = bus.subscribe();
        let seen = tick(&bus, &storage, uptime, seen);
        assert_eq!(
            drain(&mut sub).await,
            vec![Topic::SystemHealth],
            "an unchanged condition must not re-alert"
        );
        assert_eq!(seen.disk, Some(DiskLevel::Critical));
    }

    #[tokio::test]
    async fn recovery_is_announced_only_after_a_complaint() {
        let bus = EventBus::new();
        let uptime = vitals::Uptime::started_now();

        // First observation is healthy: nothing to announce.
        let mut sub = bus.subscribe();
        let healthy = tick(&bus, &storage(0.001, 0.0001), uptime, clock_already_seen());
        assert_eq!(drain(&mut sub).await, vec![Topic::SystemHealth]);

        // …but a return to healthy after a complaint is worth saying.
        let mut sub = bus.subscribe();
        let after_complaint = Seen {
            disk: Some(DiskLevel::Critical),
            ..healthy
        };
        tick(&bus, &storage(0.001, 0.0001), uptime, after_complaint);
        assert_eq!(
            drain(&mut sub).await,
            vec![Topic::SystemHealth, Topic::Alert]
        );
    }

    /// An undisciplined clock is announced once, on the tick that first sees it (REL-14).
    #[tokio::test]
    async fn the_clock_is_alerted_on_when_it_changes() {
        let bus = EventBus::new();
        let uptime = vitals::Uptime::started_now();
        let storage = storage(0.001, 0.0001);

        let mut sub = bus.subscribe();
        let seen = tick(
            &bus,
            &storage,
            uptime,
            Seen {
                disk: Some(DiskLevel::Ok),
                // Claim the opposite of the truth, so this tick is always a transition.
                clock_synced: Some(!vitals::clock_synced()),
            },
        );
        assert_eq!(
            drain(&mut sub).await,
            vec![Topic::SystemHealth, Topic::Alert],
            "a change of clock state is announced"
        );

        let mut sub = bus.subscribe();
        tick(&bus, &storage, uptime, seen);
        assert_eq!(
            drain(&mut sub).await,
            vec![Topic::SystemHealth],
            "an unchanged clock state is not re-announced"
        );
    }

    // -----------------------------------------------------------------------------------------
    // The mount's link (REL-02)
    // -----------------------------------------------------------------------------------------

    /// A miss at idle.
    const IDLE: LinkSample = LinkSample::Missed { slewing: false };
    /// A miss while the tube was moving.
    const MID_SLEW: LinkSample = LinkSample::Missed { slewing: true };

    /// `mount.serial.heartbeat_misses` is the number of polls, and it is honoured verbatim.
    ///
    /// Set to 5 rather than the shipped 3 so that a hard-coded 3 fails here: this test is the
    /// reason the key is no longer decoration, and a test that used the default value would pass
    /// against an arm that ignored it.
    #[test]
    fn the_link_is_declared_lost_on_the_configured_poll_and_not_before() {
        let mut link = MountLink::new(5);
        for poll in 1..5 {
            assert!(
                link.observe(IDLE).is_none(),
                "poll {poll} of 5 alerted — a mount is allowed to miss a reply"
            );
        }
        let alert = link.observe(IDLE).expect("the fifth miss is the threshold");
        assert_eq!(
            alert.severity(),
            astroctl_core::event::AlertSeverity::Critical
        );
        assert_eq!(alert.code(), "MOUNT_LINK_LOST");
        assert!(
            alert.message().contains('5'),
            "the alert should carry the streak that crossed the threshold: {}",
            alert.message()
        );
    }

    /// Edge-triggered: a link that has been down for an hour is one alert, not 3 600.
    #[test]
    fn a_lost_link_alerts_once_not_on_every_poll() {
        let mut link = MountLink::new(2);
        assert!(link.observe(IDLE).is_none());
        assert!(link.observe(IDLE).is_some(), "the second miss declares it");
        for _ in 0..50 {
            assert!(
                link.observe(IDLE).is_none(),
                "an unchanged condition must not re-alert"
            );
        }
    }

    /// The first successful poll clears the counter, so intermittent misses never accumulate into
    /// a loss the operator would be sent outside for.
    #[test]
    fn one_good_poll_resets_the_streak() {
        let mut link = MountLink::new(3);
        for _ in 0..10 {
            assert!(link.observe(IDLE).is_none());
            assert!(link.observe(IDLE).is_none());
            assert!(
                link.observe(LinkSample::Reached).is_none(),
                "a recovery must not be announced when nothing was announced lost"
            );
        }
    }

    /// Recovery is one `info`, and only after a complaint.
    #[test]
    fn recovery_is_announced_once_and_only_after_a_loss() {
        let mut link = MountLink::new(1);
        assert!(link.observe(IDLE).is_some());

        let alert = link
            .observe(LinkSample::Reached)
            .expect("contact restored is worth saying");
        assert_eq!(alert.severity(), astroctl_core::event::AlertSeverity::Info);
        assert_eq!(alert.code(), "MOUNT_LINK_LOST");

        for _ in 0..10 {
            assert!(
                link.observe(LinkSample::Reached).is_none(),
                "a healthy link must not announce itself every second"
            );
        }
    }

    /// The acceptance criterion `POST /api/mount/disconnect` exists for: the operator's own action
    /// is not a fault, however many polls fail while it takes effect.
    #[test]
    fn a_deliberate_disconnect_never_alerts_and_forgets_the_streak() {
        let mut link = MountLink::new(3);
        // Two misses in flight when the operator presses Disconnect — the real shape of a
        // disconnect that races the poll.
        assert!(link.observe(IDLE).is_none());
        assert!(link.observe(IDLE).is_none());
        for _ in 0..100 {
            assert!(
                link.observe(LinkSample::Standby).is_none(),
                "a disconnected mount is a state, not a link loss"
            );
        }
        // …and the streak that led into it is spent, so the next real loss costs the full three.
        assert!(link.observe(IDLE).is_none());
        assert!(link.observe(IDLE).is_none());
        assert!(link.observe(IDLE).is_some());
    }

    /// A reconnect after a loss still resolves it. `Standby` clears the counter but not the
    /// complaint, so the operator who fixes the cable is told the fix worked.
    #[test]
    fn reconnecting_after_a_loss_resolves_it() {
        let mut link = MountLink::new(1);
        assert!(link.observe(IDLE).is_some(), "lost");
        assert!(
            link.observe(LinkSample::Standby).is_none(),
            "they unplug it"
        );
        let alert = link
            .observe(LinkSample::Reached)
            .expect("the critical alert must not be left standing unanswered");
        assert_eq!(alert.severity(), astroctl_core::event::AlertSeverity::Info);
    }

    /// The sentence that sends the operator outside rather than back to the phone.
    #[test]
    fn a_loss_mid_slew_says_the_telescope_may_still_be_moving() {
        let mut idle = MountLink::new(1);
        let at_rest = idle.observe(IDLE).expect("lost");
        assert!(
            !at_rest.message().contains("slewing"),
            "a loss at idle must not imply motion: {}",
            at_rest.message()
        );

        let mut moving = MountLink::new(1);
        let in_motion = moving.observe(MID_SLEW).expect("lost");
        assert!(
            in_motion
                .message()
                .contains("slewing when contact was lost"),
            "REL-02's whole point is the tube that is still moving: {}",
            in_motion.message()
        );
    }

    /// A hand-built config cannot ask for a loss to be declared having observed nothing.
    #[test]
    fn a_zero_threshold_still_needs_one_missed_poll() {
        let mut link = MountLink::new(0);
        assert!(link.observe(LinkSample::Standby).is_none());
        assert!(link.observe(IDLE).is_some());
    }

    #[test]
    fn disk_levels_are_bounded_by_the_rel_12_thresholds() {
        let storage = storage(20.0, 5.0);
        assert_eq!(DiskLevel::of(21.0, &storage), DiskLevel::Ok);
        assert_eq!(DiskLevel::of(20.0, &storage), DiskLevel::Ok);
        assert_eq!(DiskLevel::of(19.9, &storage), DiskLevel::Low);
        assert_eq!(DiskLevel::of(5.0, &storage), DiskLevel::Low);
        assert_eq!(DiskLevel::of(4.9, &storage), DiskLevel::Critical);
    }
}
