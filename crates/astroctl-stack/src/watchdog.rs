//! The vitals watchdog — SDD §8.1 ("watchdogs on → health `ok`"), §4.3 `system.health`, REL-12.
//!
//! SDD §3's stack-node watchdog watches "disk thresholds, worker health". Worker health arrives
//! with the supervisor in M1-T13; the disk and clock half does not depend on it and is
//! implemented here in full, because it is what the `starting` → `ok` transition of §8.1 is
//! actually waiting for: a node reports `ok` when something is watching it, not when its socket
//! is open.
//!
//! Free space matters more here than on the field node: SDD §5.11.2 makes ingest refuse new
//! frames below `disk_critical_free_gb` with a 507, which turns a full archive into a growing
//! queue on the field node rather than a lost night (REL-12).
//!
//! Alerts are **edge-triggered**. A disk that has been low for six hours is one alert, not 360 —
//! SDD §5.10.4 makes the same point about an offline stack node ("one offline alert not
//! thousands"), and an operator who learns to ignore a repeating alert has lost the alert.

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
                         ingest refuses new frames (REL-12, SDD §5.11.2)",
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
