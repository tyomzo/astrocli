//! What the safety wrapper is for, asserted against a mount that records every command.
//!
//! Every timing test runs on `#[tokio::test(start_paused = true)]`, the pattern the simulator's
//! own suite established: tokio's clock jumps to the next deadline whenever the runtime is idle,
//! so a 2-second lease is tested in microseconds and the assertion is on an *exact* virtual
//! duration instead of a wall-clock measurement with a tolerance wide enough to hide the bug.
//! T-SLW-1 is a 100 ms budget; asserting it against a real clock on a loaded CI box would mean
//! either a flaky test or a budget of 500 ms, and neither is the requirement.

use std::sync::Arc;
use std::time::Duration;

use astroctl_core::bus::{EventBus, Recv};
use astroctl_core::config::MountLimits;
use astroctl_core::error::{DeviceError, ErrorCode, Limit};
use astroctl_core::event::Topic;
use astroctl_core::types::{Axis, Direction, PierSide, RaDec, SlewSpeed, TrackingMode};
use astroctl_hal::mount::MountDevice;
use chrono::{DateTime, Utc};

use crate::horizontal::{horizontal, Site};
use crate::test_double::{LookaheadModel, RecordingMount};
use crate::SafeMount;

/// The example config's site (`config/field-node.example.yaml`): Vilnius.
const VILNIUS: Site = Site {
    latitude_degrees: 54.6872,
    longitude_degrees: 25.2797,
};

/// The example config's limits, field for field.
const EXAMPLE_LIMITS: MountLimits = MountLimits {
    min_altitude_degrees: 15.0,
    meridian_limit_minutes: 15.0,
    max_travel_from_home_degrees: 180.0,
    slew_ttl_default_ms: 500,
    slew_ttl_max_ms: 2000,
};

/// A target that is above `min_altitude_degrees` right now, and one that is below it.
///
/// Computed from the current sidereal time rather than written down, because "above the horizon"
/// is a fact about the clock: a fixture pair chosen in July is below the horizon in January, and
/// a test that started failing six months after it was written would be blamed on anything but
/// the sky.
fn target_at_altitude(site: Site, altitude_degrees: f64, at: DateTime<Utc>) -> RaDec {
    // Declination equal to (latitude − 90 + altitude) puts the target at `altitude` when it is on
    // the meridian below the pole… but the simple, robust construction is a search: the transform
    // is the thing under test elsewhere, and here it is only being used to pick a target.
    let lst_hours = crate::local_sidereal_degrees(site, at) / 15.0;
    let mut best = RaDec::from_parts(lst_hours, 0.0).expect("a coordinate");
    let mut best_error = f64::INFINITY;
    for step in 0..1_800 {
        let dec = -90.0 + f64::from(step) / 10.0;
        let Ok(candidate) = RaDec::from_parts(lst_hours, dec) else {
            continue;
        };
        let error = (horizontal(candidate, site, at).alt.degrees() - altitude_degrees).abs();
        if error < best_error {
            best_error = error;
            best = candidate;
        }
    }
    assert!(
        best_error < 0.2,
        "no declination on the meridian reaches altitude {altitude_degrees}° from this site"
    );
    best
}

fn above_the_limit() -> RaDec {
    target_at_altitude(VILNIUS, 45.0, Utc::now())
}

fn below_the_limit() -> RaDec {
    target_at_altitude(VILNIUS, -20.0, Utc::now())
}

fn wrap(device: &Arc<RecordingMount>, limits: MountLimits, bus: &EventBus) -> SafeMount {
    SafeMount::new(
        Arc::clone(device) as Arc<dyn MountDevice>,
        limits,
        VILNIUS,
        bus.clone(),
    )
}

/// The first alert on the bus, or `None` if none arrives before the deadline.
async fn next_alert(events: &mut astroctl_core::bus::EventSubscriber) -> Option<(String, String)> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Recv::Event(event)) if event.topic == Topic::Alert => {
                return Some((
                    event.data["code"].as_str().unwrap_or_default().to_owned(),
                    event.data["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ));
            }
            Ok(Recv::Event(_) | Recv::Lagged { .. }) => {}
            Ok(Recv::Closed) | Err(_) => return None,
        }
    }
}

// -------------------------------------------------------------------------------------------
// MNT-15 — the altitude limit
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_goto_below_the_horizon_limit_never_reaches_the_mount() {
    // The M1-T05 acceptance criterion, asserted the way it is written: not merely "the call
    // failed" but "the device was never commanded". A wrapper that forwarded first and refused
    // afterwards would pass a test that only checked the return value, and would have started a
    // slew into the ground.
    let device = RecordingMount::at(above_the_limit()).shared();
    let bus = EventBus::new();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    let error = safe
        .goto(below_the_limit())
        .await
        .expect_err("a target below the limit must be refused");

    assert!(
        matches!(
            error,
            DeviceError::LimitViolation {
                limit: Limit::Altitude,
                ..
            }
        ),
        "got {error:?}"
    );
    assert_eq!(
        ErrorCode::from_device_error(astroctl_core::types::DeviceKind::Mount, &error),
        ErrorCode::LimitAltitude,
    );
    assert_eq!(ErrorCode::LimitAltitude.http_status(), 403);
    assert!(
        !device.was_commanded(),
        "the mount was commanded despite the refusal: {:?}",
        device.log()
    );
    // The message has to carry both numbers, or the operator goes to the config file in the dark
    // to find out what the limit is.
    let message = error.to_string();
    assert!(message.contains("15.0"), "{message}");
}

#[tokio::test]
async fn the_early_check_and_the_enforcing_check_always_agree() {
    // `check_goto` exists so the API can answer 403 instead of 202-then-alert (see its docs). The
    // hazard that shape introduces is drift: a route that asks one question and a wrapper that
    // enforces another would produce a node that accepts a slew it then refuses, or — far worse —
    // one that answers 403 for targets it would have happily driven to, training the operator to
    // ignore the limit. They call the same function; this is the assertion that they still do.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    for dec in [-80.0, -40.0, -20.0, 0.0, 20.0, 45.0, 89.0] {
        for ra in [0.0, 6.0, 12.0, 18.0] {
            let target = RaDec::from_parts(ra, dec).expect("a coordinate");
            let early = safe.check_goto(target).is_err();
            let enforced = safe.goto(target).await.is_err();
            assert_eq!(
                early, enforced,
                "the early check and the enforcing check disagreed about {ra} h / {dec}°"
            );
        }
    }
}

#[tokio::test]
async fn a_goto_above_the_limit_is_forwarded() {
    // The other half: a limit that refuses everything is not a limit, it is a broken mount.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
    safe.goto(above_the_limit()).await.expect("a high target");
    assert!(device.log().contains(&"goto".to_owned()));
}

#[tokio::test]
async fn the_altitude_limit_is_the_configured_number_and_nothing_else() {
    // "All limit behaviour driven purely by config values (no constants in code)". Two wrappers
    // over the same device at the same instant, differing only in the configured minimum, must
    // disagree about the same target — which they cannot do if the threshold is a constant.
    let target = target_at_altitude(VILNIUS, 20.0, Utc::now());
    let device = RecordingMount::at(above_the_limit()).shared();

    let permissive = wrap(
        &device,
        MountLimits {
            min_altitude_degrees: 10.0,
            ..EXAMPLE_LIMITS
        },
        &EventBus::new(),
    );
    permissive.goto(target).await.expect("20° > 10°");

    let strict = wrap(
        &device,
        MountLimits {
            min_altitude_degrees: 30.0,
            ..EXAMPLE_LIMITS
        },
        &EventBus::new(),
    );
    let error = strict.goto(target).await.expect_err("20° < 30°");
    assert!(error.to_string().contains("30.0"), "{error}");
}

#[tokio::test]
async fn a_manual_slew_that_would_descend_below_the_limit_is_refused_before_the_motors() {
    // MNT-15 covers slew as well as goto. A manual slew has no target, so the check is on the
    // direction: from a target near the southern horizon, driving further south descends.
    let device = RecordingMount::at(target_at_altitude(VILNIUS, 15.5, Utc::now())).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    let error = safe
        .slew_for(
            Axis::Dec,
            Direction::South,
            SlewSpeed::Medium,
            Duration::from_millis(500),
        )
        .await
        .expect_err("driving further down must be refused");
    assert!(matches!(
        error,
        DeviceError::LimitViolation {
            limit: Limit::Altitude,
            ..
        }
    ));
    assert!(
        !device.log().contains(&"slew".to_owned()),
        "the axis was commanded: {:?}",
        device.log()
    );
}

#[tokio::test]
async fn a_mount_below_the_limit_can_still_be_driven_back_up() {
    // The trap a positional check would set: refuse every slew while below the limit and the
    // operator cannot recover the mount without power-cycling it. From below the horizon,
    // northward (up, from the site latitude, for a southern target) must be allowed.
    let device = RecordingMount::at(target_at_altitude(VILNIUS, -20.0, Utc::now())).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.slew_for(
        Axis::Dec,
        Direction::North,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("climbing out of the limit must be allowed");
    assert!(device.log().contains(&"slew".to_owned()));
}

// -------------------------------------------------------------------------------------------
// SDD §5.8.1 — the dead-man's switch (T-SLW-1)
// -------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn t_slw_1_dropped_renewals_stop_the_axis_within_the_ttl_plus_a_hundred_milliseconds() {
    // The gated test. The scenario is not a bug in the app: it is a dropped VPN packet, a phone
    // that locked mid-hold, or a browser tab that died — the axis is turning and nothing is going
    // to ask it to stop.
    let device = RecordingMount::at(above_the_limit()).shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    let ttl = Duration::from_millis(EXAMPLE_LIMITS.slew_ttl_default_ms);
    let started = tokio::time::Instant::now();
    safe.slew_for(Axis::Ra, Direction::East, SlewSpeed::Medium, ttl)
        .await
        .expect("the slew starts");
    assert!(device.is_slewing(Axis::Ra), "the axis should be turning");

    // Renewals stop arriving here. Nothing else in this test asks the mount to do anything.
    let alert = next_alert(&mut events)
        .await
        .expect("an alert is published");
    let elapsed = started.elapsed();

    assert!(
        !device.is_slewing(Axis::Ra),
        "the axis was still turning when the TTL alert fired"
    );
    assert!(
        elapsed <= ttl + Duration::from_millis(100),
        "the axis stopped {elapsed:?} after the lease was granted; T-SLW-1 allows \
         {:?}",
        ttl + Duration::from_millis(100)
    );
    assert_eq!(alert.0, ErrorCode::SlewTtlExpired.as_str());
    assert!(
        alert.1.contains("right ascension"),
        "the alert should name the axis in the operator's words: {}",
        alert.1
    );
}

#[tokio::test(start_paused = true)]
async fn an_identical_repeat_extends_the_lease_without_touching_the_motors() {
    // §5.8.1: "a repeat with identical parameters extends the deadline without re-issuing motor
    // commands". Re-issuing them at 2 Hz restarts the ramp and makes the mount stutter under the
    // operator's thumb — and on a real mount it is four extra exchanges a second on a 9600 baud
    // line, which is the budget the e-stop lane is trying to protect.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
    let ttl = Duration::from_millis(500);

    safe.slew_for(Axis::Ra, Direction::East, SlewSpeed::Medium, ttl)
        .await
        .expect("first lease");

    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        safe.slew_for(Axis::Ra, Direction::East, SlewSpeed::Medium, ttl)
            .await
            .expect("renewal");
    }

    let log = device.log();
    // Exactly one motor command for five requests. The renewals also did not cost the *position*
    // read the first authorisation needs for its limit check — the reads in this log belong to
    // the background watch, which polls on its own 2 Hz schedule whether or not renewals arrive.
    assert_eq!(
        log.iter().filter(|call| call.as_str() == "slew").count(),
        1,
        "a renewal re-issued a motor command: {log:?}"
    );
    assert!(
        log.iter()
            .all(|call| matches!(call.as_str(), "slew" | "position")),
        "a renewal sent something other than the one slew: {log:?}"
    );
    assert!(
        device.is_slewing(Axis::Ra),
        "one second of renewals at half the TTL must keep the axis moving"
    );
}

#[tokio::test(start_paused = true)]
async fn a_renewal_with_different_parameters_is_a_new_authorisation() {
    // The operator moved the speed slider without letting go of the D-pad. That is a new motion
    // and must reach the device — treating it as a renewal would leave the mount at the old speed
    // while the UI showed the new one.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
    let ttl = Duration::from_millis(500);

    safe.slew_for(Axis::Ra, Direction::East, SlewSpeed::Slow, ttl)
        .await
        .expect("first");
    safe.slew_for(Axis::Ra, Direction::East, SlewSpeed::Max, ttl)
        .await
        .expect("faster");

    assert_eq!(
        device
            .log()
            .iter()
            .filter(|call| call.as_str() == "slew")
            .count(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn the_ttl_is_clamped_to_the_configured_maximum_rather_than_refused() {
    // §5.8.1: "default 500 ms, max 2000 ms, clamped server-side". This is the *only*
    // implementation of that rule — the API route reports what this returns, so the lease the
    // wrapper enforces and the number the operator's app renews against cannot drift apart.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    assert_eq!(safe.resolve_ttl(None), Duration::from_millis(500));
    assert_eq!(safe.resolve_ttl(Some(250)), Duration::from_millis(250));
    assert_eq!(safe.resolve_ttl(Some(60_000)), Duration::from_millis(2_000));

    // And the clamp is the configured number, not a constant.
    let looser = wrap(
        &device,
        MountLimits {
            slew_ttl_default_ms: 800,
            slew_ttl_max_ms: 4_000,
            ..EXAMPLE_LIMITS
        },
        &EventBus::new(),
    );
    assert_eq!(looser.resolve_ttl(None), Duration::from_millis(800));
    assert_eq!(
        looser.resolve_ttl(Some(60_000)),
        Duration::from_millis(4_000)
    );
}

#[tokio::test(start_paused = true)]
async fn an_explicit_stop_releases_the_lease_so_the_watcher_does_not_fire_afterwards() {
    // Release is the primary stop path and TTL expiry is the backstop (§5.8.1). A backstop that
    // fires after every normal release would fill the operator's alert list with events that
    // describe nothing going wrong, and an alert nobody reads is an alert that has stopped
    // working.
    let device = RecordingMount::at(above_the_limit()).shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.slew_for(
        Axis::Ra,
        Direction::East,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("the slew starts");
    safe.stop_slew(Axis::Ra).await.expect("release");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1), events.recv())
            .await
            .is_err(),
        "the TTL watcher alerted on an axis the operator had already stopped"
    );
}

// -------------------------------------------------------------------------------------------
// MNT-15 — the continuous check during motion
// -------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_slew_that_drifts_below_the_limit_is_stopped_by_the_background_check() {
    // The pre-flight check passes at the moment the button is pressed; the mount then keeps
    // moving. SDD §5.4's "background limit check at 2 Hz while manual slew active" is what
    // catches the rest of the motion, and the lease is long enough here that the dead-man's
    // switch is not what stops it.
    let device = RecordingMount::at(target_at_altitude(VILNIUS, 25.0, Utc::now())).shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.slew_for(
        Axis::Dec,
        Direction::South,
        SlewSpeed::Medium,
        Duration::from_millis(2_000),
    )
    .await
    .expect("25° is above the 15° limit");

    // The mount moved while nobody was looking — which is what a slew is.
    device.place(target_at_altitude(VILNIUS, 14.0, Utc::now()));

    let alert = next_alert(&mut events).await.expect("an alert");
    assert_eq!(alert.0, ErrorCode::LimitAltitude.as_str());
    assert!(!device.is_slewing(Axis::Dec), "the axis should be stopped");
}

// -------------------------------------------------------------------------------------------
// M3-T07 — travel from the home pose
// -------------------------------------------------------------------------------------------

/// A mount 200° from home on the right-ascension axis, unwound by slewing east.
///
/// Past the example config's 180° ceiling, and near the 215.6° an operator actually reached on
/// 2026-08-02 by holding a D-pad with nothing in the system counting.
fn wound_past_the_limit() -> Arc<RecordingMount> {
    RecordingMount::at(above_the_limit())
        .with_travel((200.0, Direction::East), (0.0, Direction::North))
        .shared()
}

#[tokio::test]
async fn a_manual_slew_that_would_wind_an_axis_further_is_refused_before_the_mount_is_commanded() {
    // The gap the second observation of M3-T07 names: Synta motion has no soft limits and nothing
    // tracked distance from home, so a thumb on a D-pad could wind an axis indefinitely. With a
    // telescope, a power lead and a USB cable attached that is a torn cable or a tube in the pier.
    let device = wound_past_the_limit();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    let error = safe
        .slew_for(
            Axis::Ra,
            // West winds it further — east is what `with_travel` said unwinds.
            Direction::West,
            SlewSpeed::Medium,
            Duration::from_millis(500),
        )
        .await
        .expect_err("200 degrees from home is past the 180 degree limit");

    assert!(
        matches!(
            error,
            DeviceError::LimitViolation {
                limit: Limit::Travel,
                ..
            }
        ),
        "got {error:?}"
    );
    assert_eq!(
        ErrorCode::from_device_error(astroctl_core::types::DeviceKind::Mount, &error),
        ErrorCode::LimitTravel,
    );
    assert_eq!(ErrorCode::LimitTravel.http_status(), 403);
    assert!(
        !device.was_commanded(),
        "the mount was commanded despite the refusal: {:?}",
        device.log()
    );

    // The message names the limit and the current travel, and says which way is out. An operator
    // reading this is in the dark with a D-pad that has stopped answering one of its buttons.
    let message = error.to_string();
    assert!(message.contains("200.0"), "no current travel in: {message}");
    assert!(message.contains("180.0"), "no limit in: {message}");
    assert!(
        message.contains("max_travel_from_home_degrees"),
        "no config key in: {message}"
    );
    assert!(message.contains("east"), "no way out in: {message}");
}

#[tokio::test]
async fn the_direction_that_unwinds_is_always_allowed_however_far_the_axis_has_gone() {
    // The escape hatch, and the reason the check is directional rather than positional. A bound
    // that refused both directions once exceeded would trap the mount at exactly the moment
    // somebody needs to drive it back — which is how 2026-08-02 ended: power off, loosen the
    // clutch, unwind by hand.
    let device = wound_past_the_limit();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.slew_for(
        Axis::Ra,
        Direction::East,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("the homeward direction is never refused");
    assert!(device.is_slewing(Axis::Ra), "the unwind reached the mount");
}

#[tokio::test]
async fn the_other_axis_is_untouched_by_one_axis_being_wound() {
    // The limit is per axis because the travel is. A wound right-ascension axis must not stop the
    // operator framing in declination.
    let device = wound_past_the_limit();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.slew_for(
        Axis::Dec,
        Direction::South,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("the declination axis is at home");
    assert!(device.is_slewing(Axis::Dec));
}

#[tokio::test(start_paused = true)]
async fn an_axis_winding_past_the_limit_under_a_held_dpad_is_stopped_by_the_watch() {
    // **The assertion the requirement actually needs.** Both slew paths short-circuit an identical
    // renewal — the app renews at 2 Hz for as long as a thumb is down, and re-checking on every
    // renewal would put four extra exchanges a second on a 9600-baud line — so the pre-flight
    // check above runs *once*, at the press. A hold that starts inside the limit and winds past it
    // is invisible to it. That is precisely the 215.6 degree case: the operator never let go.
    //
    // So the watch has to carry the check, and the lease here is long enough that the dead-man's
    // switch is not what stops the axis.
    let device = RecordingMount::at(above_the_limit())
        .with_travel((170.0, Direction::East), (0.0, Direction::North))
        .shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.slew_for(
        Axis::Ra,
        Direction::West,
        SlewSpeed::Medium,
        Duration::from_millis(2_000),
    )
    .await
    .expect("170 degrees is inside the limit at the moment of the press");
    assert!(device.is_slewing(Axis::Ra), "the slew started");

    // The axis keeps winding while nobody re-checks it — which is what a held D-pad is.
    device.set_travel(181.0, 0.0);

    let alert = next_alert(&mut events).await.expect("an alert");
    assert_eq!(alert.0, ErrorCode::LimitTravel.as_str());
    assert!(
        alert.1.contains("181.0") && alert.1.contains("unwind"),
        "the alert must say how far and which way out: {}",
        alert.1
    );
    assert!(!device.is_slewing(Axis::Ra), "the axis should be stopped");
}

#[tokio::test]
async fn a_mount_with_no_home_reference_is_not_gated_by_a_limit_it_cannot_measure() {
    // `RecordingMount` reports `None` unless told otherwise, which is what an INDI or Alpaca
    // device — or the simulator — reports: no counters, no `0x800000`, no travel. The limit then
    // has nothing to act on, and inventing a number so that it could would be enforcing a cable
    // bound against a quantity nobody measured.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.slew_for(
        Axis::Ra,
        Direction::West,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("a mount with no home reference is not gated");
    assert!(device.is_slewing(Axis::Ra));
}

// -------------------------------------------------------------------------------------------
// MNT-16 — the meridian watch
// -------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn tracking_stops_when_the_mount_crosses_the_meridian_limit() {
    // MNT-16. The mount is tracking a target that reaches the configured limit past the meridian;
    // continuing would drive the tube into the pier.
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let now = Utc::now();
    // Just short of the limit: 14 minutes past the meridian, against a configured 15.
    let device = RecordingMount::at(target_at_hour_angle(VILNIUS, 14.0, now)).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.start_tracking(TrackingMode::Sidereal)
        .await
        .expect("tracking starts");
    // One tick to establish where the mount was, then the sky carries it past the limit.
    tokio::time::sleep(Duration::from_millis(600)).await;
    device.place(target_at_hour_angle(VILNIUS, 15.5, Utc::now()));

    let alert = next_alert(&mut events).await.expect("an alert");
    assert_eq!(alert.0, ErrorCode::LimitMeridian.as_str());
    assert!(
        alert.1.contains("15") && alert.1.contains("meridian"),
        "the alert should say how far past the meridian the limit is: {}",
        alert.1
    );
    assert_eq!(device.tracking(), None, "tracking should be stopped");
}

#[tokio::test(start_paused = true)]
async fn a_target_acquired_west_of_the_limit_is_left_alone() {
    // The reason the watch fires on a crossing rather than on a comparison. Phase 1 has no pier
    // side (SDD §5.2.3 leaves it to a driver that derives it, and none does yet), so a bare
    // `hour_angle > limit` test would stop tracking the instant an operator acquired anything in
    // the western sky — targets that are perfectly safe, because the mount flipped to reach them.
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let device = RecordingMount::at(target_at_hour_angle(VILNIUS, 90.0, Utc::now())).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.start_tracking(TrackingMode::Sidereal)
        .await
        .expect("tracking starts");
    for minutes in [90.5_f64, 91.0, 91.5] {
        tokio::time::sleep(Duration::from_millis(600)).await;
        device.place(target_at_hour_angle(VILNIUS, minutes, Utc::now()));
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(1), events.recv())
            .await
            .is_err(),
        "a target already west of the limit was stopped as though it had just crossed it"
    );
    assert_eq!(device.tracking(), Some(TrackingMode::Sidereal));
}

/// A direction whose hour angle is `minutes` of time past the meridian at `at`.
fn target_at_hour_angle(site: Site, minutes: f64, at: DateTime<Utc>) -> RaDec {
    let lst_hours = crate::local_sidereal_degrees(site, at) / 15.0;
    RaDec::from_parts((lst_hours - minutes / 60.0).rem_euclid(24.0), 45.0)
        .expect("a coordinate on the meridian side")
}

// -------------------------------------------------------------------------------------------
// REL-01 / SDD §5.8.2 — the e-stop path
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_emergency_stop_reaches_the_device_while_a_slew_is_in_flight() {
    // SDD §5.2.4 gives the e-stop its own lane to the transport, and §5.8.2 budgets the whole
    // handler-to-wire path at 20 ms. A wrapper that took its own lock before forwarding would
    // reintroduce, one layer up, exactly the queue the driver avoids — and the failure would only
    // appear when something was already going wrong.
    let device = RecordingMount::at(above_the_limit())
        .with_slow_slew(Duration::from_secs(5))
        .shared();
    let safe = Arc::new(wrap(&device, EXAMPLE_LIMITS, &EventBus::new()));

    let slewing = {
        let safe = Arc::clone(&safe);
        tokio::spawn(async move {
            safe.slew_for(
                Axis::Ra,
                Direction::East,
                SlewSpeed::Max,
                Duration::from_millis(2_000),
            )
            .await
        })
    };
    // Long enough for the spawned task to be inside the device call, short enough that the
    // assertion below is about the e-stop rather than about scheduling.
    tokio::time::sleep(Duration::from_millis(50)).await;

    tokio::time::timeout(Duration::from_millis(500), safe.emergency_stop())
        .await
        .expect("the e-stop must not wait for the slew to finish")
        .expect("the e-stop succeeds");

    assert!(device.log().contains(&"emergency_stop".to_owned()));
    let log = device.log();
    let stop_at = log
        .iter()
        .position(|c| c == "emergency_stop")
        .expect("stop");
    let slew_at = log.iter().position(|c| c == "slew").expect("slew");
    assert!(
        stop_at > slew_at,
        "the stop should have overtaken an in-flight slew, log: {log:?}"
    );
    assert!(!slewing.is_finished(), "the slew was still in flight");
}

#[tokio::test]
async fn an_emergency_stop_is_never_gated_by_a_limit() {
    // REL-01: "must work regardless of application state". Including — especially — with the
    // mount below its own horizon limit, which is exactly when an operator reaches for it.
    let device = RecordingMount::at(below_the_limit()).shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.emergency_stop().await.expect("never refused");
    let alert = next_alert(&mut events).await.expect("an alert");
    assert_eq!(alert.0, "EMERGENCY_STOP");
    assert!(
        alert.1.contains("halted"),
        "the operator's app renders this: {}",
        alert.1
    );
}

#[tokio::test(start_paused = true)]
async fn an_emergency_stop_cancels_the_leases_it_stopped() {
    // Otherwise the TTL watcher fires half a second later and stops an axis that is already
    // stopped, publishing a `SLEW_TTL_EXPIRED` the operator would read as a second fault.
    let device = RecordingMount::at(above_the_limit()).shared();
    let bus = EventBus::new();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.slew_for(
        Axis::Ra,
        Direction::East,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("the slew starts");
    safe.emergency_stop().await.expect("stops");

    let mut events = bus.subscribe();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1), events.recv())
            .await
            .is_err(),
        "a TTL alert fired for an axis an emergency stop had already stopped"
    );
}

// -------------------------------------------------------------------------------------------
// Shutdown — the invariant M1-T03's follow-up made a hard rule
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn dropping_the_wrapper_releases_the_event_bus_handle_its_watch_holds() {
    // The regression M1-T03 paid for once (`78519e4`): a task holding an `EventBus` clone is a
    // broadcast *sender*, and the session log's writer only closes and flushes when the last
    // sender goes. A background watch that outlived the wrapper would cost a full flush timeout
    // and the tail of the night's telemetry, every restart made while the mount was moving.
    //
    // `Drop` is what guarantees it rather than a `shutdown()` the binary has to remember, because
    // the binary forgot twice.
    let device = RecordingMount::at(above_the_limit()).shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.slew_for(
        Axis::Ra,
        Direction::East,
        SlewSpeed::Medium,
        Duration::from_millis(2_000),
    )
    .await
    .expect("a watch is now running");

    drop(safe);
    drop(bus);

    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await, Recv::Closed) {
                return;
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "an EventBus handle outlived the safety wrapper, so the event log could not flush"
    );
}

#[tokio::test(start_paused = true)]
async fn the_watch_stops_itself_when_there_is_nothing_left_to_watch() {
    // An idle node must not carry a 2 Hz timer and a 2 Hz serial read for a mount nobody is
    // driving. The observable proof is that the device stops being polled.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.slew_for(
        Axis::Ra,
        Direction::East,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("the slew starts");
    safe.stop_slew(Axis::Ra).await.expect("released");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let quiet = device.log().len();
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
        device.log().len(),
        quiet,
        "the watch was still polling a mount that is doing nothing: {:?}",
        device.log()
    );
}

#[tokio::test(start_paused = true)]
async fn a_slew_after_the_watch_exited_gets_a_new_watch() {
    // The race the exit path has to get right: the watch clears its own slot under the same lock
    // a new authorisation takes to spawn a replacement. If it did not, the second hold of the
    // night would have no dead-man's switch at all — the most dangerous possible way for this to
    // fail, because everything would look like it was working.
    let device = RecordingMount::at(above_the_limit()).shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    safe.slew_for(
        Axis::Ra,
        Direction::East,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("first hold");
    safe.stop_slew(Axis::Ra).await.expect("released");
    tokio::time::sleep(Duration::from_secs(5)).await;

    safe.slew_for(
        Axis::Dec,
        Direction::North,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("second hold");

    let alert = next_alert(&mut events)
        .await
        .expect("the second hold must have a dead-man's switch too");
    assert_eq!(alert.0, ErrorCode::SlewTtlExpired.as_str());
    assert!(!device.is_slewing(Axis::Dec));
}

// -------------------------------------------------------------------------------------------
// Pass-through
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_stopping_and_stowing_commands_are_never_gated() {
    // A safety layer that could refuse a stop would be the most dangerous component in the
    // system, and `park` is checked by nothing on purpose: the park position is the operator's
    // own configuration, and refusing to park a mount because its park position is low refuses
    // the one action that makes it safe.
    let device = RecordingMount::at(below_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.stop_slew(Axis::Ra).await.expect("stop");
    safe.stop_tracking().await.expect("stop tracking");
    safe.park().await.expect("park");
    safe.disconnect().await.expect("disconnect");

    for expected in ["stop_slew", "stop_tracking", "park", "disconnect"] {
        assert!(device.log().contains(&expected.to_owned()), "{expected}");
    }
}

#[tokio::test]
async fn the_wrapper_reports_the_wrapped_devices_identity() {
    // The UI decides what to render from `capabilities()` (HAL rule 6). A wrapper that answered
    // for itself would hide the mount's pulse-guide support behind a safety layer that has no
    // opinion about it.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
    assert_eq!(safe.device_info().protocol, "test");
    assert!(safe.capabilities().has_pulse_guide);
    assert!(Arc::ptr_eq(
        safe.inner(),
        &(Arc::clone(&device) as Arc<dyn MountDevice>)
    ));
}

#[tokio::test]
async fn the_alt_az_the_operator_reads_is_the_one_the_limit_used() {
    // The acceptance criterion's "so a limit bug and a display bug cannot disagree", asserted as
    // an identity rather than as a tolerance: both paths call the same function.
    let device = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
    let target = below_the_limit();

    let displayed = safe.horizontal(target);
    assert!(
        displayed.alt.degrees() < EXAMPLE_LIMITS.min_altitude_degrees,
        "the display says {}° while the limit is {}°",
        displayed.alt.degrees(),
        EXAMPLE_LIMITS.min_altitude_degrees
    );
    assert!(safe.goto(target).await.is_err());
}

// ---------------------------------------------------------------------------------------------
// M3-T08 — the altitude limit on a mount that has crossed the pole (SDD §5.4.1, §5.4.2)
// ---------------------------------------------------------------------------------------------

/// The defect, as a test: past the pole, `north` is the direction that descends.
///
/// This is the shape of the 2026-08-02 hardware finding. Before M3-T08 the wrapper predicted
/// motion by adding a delta to the sky coordinate and assuming north raises declination, so on
/// this mount it reported the tube climbing while it descended — and because the descent guard
/// then saw no descent, it permitted the motion *unconditionally*, for a full 180° of travel.
///
/// The mount here differs from the one above in exactly one respect: it answers
/// `motion_lookahead`, i.e. it is willing to say which way its metal turns.
#[tokio::test]
async fn a_declination_slew_that_descends_past_the_pole_is_refused() {
    let device = RecordingMount::at(target_at_altitude(VILNIUS, 15.5, Utc::now()))
        .with_lookahead(LookaheadModel::flipped_branch(1.0))
        .shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    let error = safe
        .slew_for(
            Axis::Dec,
            Direction::North,
            SlewSpeed::Medium,
            Duration::from_millis(500),
        )
        .await
        .expect_err("north descends on this branch and must be refused");
    assert!(matches!(
        error,
        DeviceError::LimitViolation {
            limit: Limit::Altitude,
            ..
        }
    ));
    assert!(
        !device.log().contains(&"slew".to_owned()),
        "the axis was commanded: {:?}",
        device.log()
    );
}

/// And the converse, so the fix is not simply "refuse declination slews".
///
/// On the same mount at the same position, the *other* declination command climbs, and must be
/// allowed. A test that only asserted the refusal above would pass against a wrapper that had
/// stopped moving the declination axis at all.
#[tokio::test]
async fn the_climbing_declination_direction_is_still_permitted_past_the_pole() {
    let device = RecordingMount::at(target_at_altitude(VILNIUS, 15.5, Utc::now()))
        .with_lookahead(LookaheadModel::flipped_branch(1.0))
        .shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.slew_for(
        Axis::Dec,
        Direction::South,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("south climbs on this branch and must be allowed");
    assert!(device.log().contains(&"slew".to_owned()));
}

/// Obligation 5's guarantee has to survive the fix, on the branch it was never tested on.
///
/// An axis below the limit must still be drivable back up. `a_mount_below_the_limit_can_still_be
/// _driven_back_up` asserts this for a mount with no mechanical state; this asserts it for one
/// past the pole, where the direction that recovers is the opposite of the one that does there.
#[tokio::test]
async fn a_mount_below_the_limit_past_the_pole_can_still_be_driven_back_up() {
    let device = RecordingMount::at(target_at_altitude(VILNIUS, -20.0, Utc::now()))
        .with_lookahead(LookaheadModel::flipped_branch(1.0))
        .shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());

    safe.slew_for(
        Axis::Dec,
        Direction::South,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("climbing out of the limit must be allowed on either branch");
    assert!(device.log().contains(&"slew".to_owned()));
}

/// The right-ascension axis takes the same answer from either branch.
///
/// SDD §5.4.2 derives that `∂HA/∂h` carries the hemisphere sign on *both* branches — the flip adds
/// a constant 180°, which has no derivative — so only declination needed the branch. That claim
/// bounded the change, so it is pinned here rather than left as reasoning: an east slew from a
/// position near the western horizon is refused identically whichever branch the mount reports.
#[tokio::test]
async fn the_right_ascension_axis_is_unaffected_by_the_branch() {
    let at = Utc::now();
    let mut verdicts = Vec::new();
    for model in [
        LookaheadModel::normal_branch(1.0),
        LookaheadModel::flipped_branch(1.0),
    ] {
        let device = RecordingMount::at(target_at_altitude(VILNIUS, 15.5, at))
            .with_lookahead(model)
            .shared();
        let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
        for dir in [Direction::East, Direction::West] {
            let allowed = safe
                .slew_for(Axis::Ra, dir, SlewSpeed::Medium, Duration::from_millis(500))
                .await
                .is_ok();
            verdicts.push(allowed);
            safe.stop_slew(Axis::Ra).await.expect("stop");
        }
    }
    assert_eq!(
        verdicts[..2],
        verdicts[2..],
        "the branch changed a right-ascension verdict, so §5.4.2's derivation is wrong"
    );
}

/// A held slew that crosses the pole: nothing the operator did changed, and the button inverted.
///
/// The load-bearing case, and the one the pre-flight check structurally cannot catch. Both slew
/// paths short-circuit an identical renewal, so the check made at the press never runs again while
/// the thumb is down — and a declination hold from near the home pose crosses the branch *during*
/// that hold, turning the direction that was climbing into the one that descends. Only the watch
/// sees it.
#[tokio::test]
async fn a_held_declination_slew_is_stopped_when_crossing_the_pole_turns_it_downward() {
    let device = RecordingMount::at(target_at_altitude(VILNIUS, 15.5, Utc::now()))
        .with_lookahead(LookaheadModel::normal_branch(1.0))
        .shared();
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let safe = wrap(&device, EXAMPLE_LIMITS, &bus);

    // North climbs on the branch the mount is on at the press, so this is allowed — correctly.
    safe.slew_for(
        Axis::Dec,
        Direction::North,
        SlewSpeed::Medium,
        Duration::from_millis(2_000),
    )
    .await
    .expect("north climbs on this branch at the moment of the press");
    assert!(device.is_slewing(Axis::Dec), "the slew started");

    // The axis crosses the pole. The operator is still holding north; the metal is now descending.
    device.set_lookahead(LookaheadModel::flipped_branch(1.0));

    let alert = next_alert(&mut events).await.expect("an alert");
    assert_eq!(alert.0, ErrorCode::LimitAltitude.as_str());
    assert!(
        !device.is_slewing(Axis::Dec),
        "the axis kept descending under a held button"
    );
}

/// The pier side a driver knows must reach the layer that publishes it (M3-T08).
///
/// It was an inherent method on the skywatcher driver, so `mount.position.pier_side` reported
/// `unknown` on hardware whose driver had the answer — SDD §5.4's obligation 3, outstanding since
/// M1-T05. The wrapper adds nothing here and must subtract nothing either.
#[tokio::test]
async fn the_pier_side_a_driver_reports_reaches_the_wrapper() {
    let device = RecordingMount::at(above_the_limit())
        .with_pier_side(PierSide::East)
        .shared();
    let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
    assert_eq!(safe.pier_side(), Some(PierSide::East));

    // And a device with no mechanical state still says nothing, rather than guessing east.
    let bodiless = RecordingMount::at(above_the_limit()).shared();
    let safe = wrap(&bodiless, EXAMPLE_LIMITS, &EventBus::new());
    assert_eq!(safe.pier_side(), None);
}

/// The altitude limit depends on **both** axes, not on declination alone.
///
/// Raised by the operator on 2026-08-02, looking at the "73° of declination travel" figure from
/// the M3-T08 investigation and asking whether it could really be a property of the declination
/// axis by itself. It cannot, and the figure was a special case: that measurement was taken with
/// the right-ascension axis at home, where the hour angle is 6h, `cos(HA)` is zero, and the
/// altitude reduces to `asin(sin φ · sin δ)` — declination alone. Move the right-ascension axis
/// and the term comes back. At this site the same declination travel that is safe near the
/// meridian (≈110°) runs out at ≈40° near the anti-meridian.
///
/// Both positions below have the **same declination**, so a limit that looked only at declination
/// would have to give them the same verdict. They must differ.
#[tokio::test]
async fn the_altitude_limit_depends_on_the_hour_angle_not_only_the_declination() {
    let at = Utc::now();
    let lst_hours = crate::local_sidereal_degrees(VILNIUS, at) / 15.0;
    const DEC: f64 = 20.0;

    // On the meridian, declination 20° is ~55° up at this latitude: plenty of room to descend.
    let on_the_meridian = RaDec::from_parts(lst_hours.rem_euclid(24.0), DEC).expect("valid");
    // Twelve hours away, the *same* declination is below the horizon entirely.
    let anti_meridian = RaDec::from_parts((lst_hours + 12.0).rem_euclid(24.0), DEC).expect("valid");

    assert!(
        horizontal(on_the_meridian, VILNIUS, at).alt.degrees()
            > horizontal(anti_meridian, VILNIUS, at).alt.degrees() + 40.0,
        "the fixture is not exercising the hour-angle term"
    );

    let permitted = {
        let device = RecordingMount::at(on_the_meridian).shared();
        let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
        safe.slew_for(
            Axis::Dec,
            Direction::South,
            SlewSpeed::Medium,
            Duration::from_millis(500),
        )
        .await
        .is_ok()
    };
    let refused = {
        let device = RecordingMount::at(anti_meridian).shared();
        let safe = wrap(&device, EXAMPLE_LIMITS, &EventBus::new());
        safe.slew_for(
            Axis::Dec,
            Direction::South,
            SlewSpeed::Medium,
            Duration::from_millis(500),
        )
        .await
        .is_err()
    };

    assert!(
        permitted && refused,
        "same declination, opposite verdicts expected — permitted on the meridian: {permitted}, \
         refused at the anti-meridian: {refused}"
    );
}

/// The collision limit refuses a manual slew, and only when geometry is configured (SDD §5.4.3).
///
/// Two assertions in one because the pair is the point: the same mount, the same command, and the
/// only difference is whether the operator has measured their rig. A node that has not is not
/// protected and must not pretend to be.
#[tokio::test]
async fn a_slew_into_the_tripod_is_refused_only_once_the_rig_is_measured() {
    use astroctl_core::config::RigGeometry;

    // A rig whose tube is long enough that pointing anywhere low puts it in the legs.
    let geometry = RigGeometry {
        dec_axis_offset_mm: 180.0,
        tube_half_length_mm: 1_400.0,
        tube_radius_mm: 120.0,
        saddle_offset_mm: 180.0,
        head_height_mm: 1_250.0,
        mount_body_height_mm: 250.0,
        top_radius_mm: 80.0,
        base_radius_mm: 650.0,
        counterweight: None,
    };
    let at = Utc::now();
    let low = target_at_altitude(VILNIUS, 20.0, at);

    let unmeasured = RecordingMount::at(low).shared();
    SafeMount::with_geometry(
        Arc::clone(&unmeasured) as Arc<dyn MountDevice>,
        EXAMPLE_LIMITS,
        VILNIUS,
        None,
        EventBus::new(),
    )
    .slew_for(
        Axis::Dec,
        Direction::South,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect("with no geometry there is no collision limit");

    let measured = RecordingMount::at(low).shared();
    let error = SafeMount::with_geometry(
        Arc::clone(&measured) as Arc<dyn MountDevice>,
        EXAMPLE_LIMITS,
        VILNIUS,
        Some(geometry),
        EventBus::new(),
    )
    .slew_for(
        Axis::Dec,
        Direction::South,
        SlewSpeed::Medium,
        Duration::from_millis(500),
    )
    .await
    .expect_err("the rig is in the legs and the slew must be refused");
    assert!(matches!(
        error,
        DeviceError::LimitViolation {
            limit: Limit::Collision,
            ..
        }
    ));
    assert!(
        !measured.log().contains(&"slew".to_owned()),
        "the axis was commanded despite the refusal: {:?}",
        measured.log()
    );
}
