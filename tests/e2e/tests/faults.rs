//! The three faults M1-T16 asks for, against the real pair: the stacking server dies mid-session,
//! the field node restarts mid-session, and the mount's link falls out mid-slew.
//!
//! These are the tests M0-T08's containers exist for. "Kill the stacking server" is a
//! `compose stop` of a process in another network namespace, not a mocked-out client; "restart the
//! field node" is a `SIGKILL` and a fresh process against the same named volume, which is the only
//! way the `open_current` path and the transfer journal's recovery are exercised at all.
//!
//! Between them they carry T-XFER-1 (SDD §9) end to end: queue grows, capture unaffected, one
//! offline alert not thousands, queue drains in order, every frame acked exactly once, and a field
//! node killed mid-session comes back to the session it was in.

use std::time::{Duration, Instant};

use astroctl_e2e::{wait_until, EventStream, Harness};

/// Fast enough to keep three captures inside a scenario, slow enough that `exposing` is a state
/// the stream passes through. See `t_e2e_1.rs` for the full argument.
const SHUTTER: &str = "1";

/// Circumpolar from the harness site, so a goto is never refused for altitude. See `t_e2e_1.rs`.
const TARGET_RA_HOURS: f64 = 5.60;
const TARGET_DEC_NEAR: f64 = 50.0;
const TARGET_DEC_FAR: f64 = 64.0;

/// Connect both devices and put the camera on a short exposure.
async fn ready_to_capture(client: &astroctl_e2e::Client) {
    client.connect_mount().await;
    client.connect_camera().await;
    client.set_shutter(SHUTTER).await;
}

/// Capture one frame and wait until it is durable on the field node.
///
/// Deliberately does *not* wait for the ack: every caller here is about what happens to a frame
/// between being saved and being delivered, so waiting for delivery would be waiting for the
/// thing under test.
async fn capture_and_save(
    client: &astroctl_e2e::Client,
    events: &EventStream,
    index: usize,
) -> String {
    let frame_id = client.capture().await.frame_id;
    events
        .wait_for("frame.saved", Duration::from_secs(90), |event| {
            event.str_field("frame_id") == frame_id
        })
        .await;
    eprintln!("   frame {index}: {frame_id} saved");
    frame_id
}

// ============================================================================================
// T-XFER-1, first half — the stacking server dies mid-session
// ============================================================================================

/// The stacking server dies mid-session: captures carry on, the queue grows, **one** alert covers
/// the whole outage, and on recovery the queue drains and every frame is acked exactly once.
///
/// The alert count is the assertion this scenario exists for. SDD §5.10.2 asks for one alert on
/// the transition and one on recovery, "never one per attempt", and the transfer agent retries on
/// a backoff throughout — so a regression that moved the alert inside the retry loop would show up
/// here as a dozen and nowhere else. M1-T16 also made this the assertion that pins the
/// `STACK_UNREACHABLE` producer count at one (SDD change note 1.23.0): before that, the field
/// node's `stack.status` republisher raised a second alert with the same code on its own 5 s poll,
/// and this test would have counted two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stacking_server_dies_mid_session_and_the_queue_survives_it() {
    let harness = Harness::attach();
    harness.ensure_pair_running().await;
    let client = harness.client();
    let events = EventStream::connect(&client).await;
    ready_to_capture(&client).await;

    // A frame that goes all the way through first, so the scenario knows the path works before it
    // breaks it. Without this, a failure below is ambiguous between "the outage broke it" and "it
    // was never working".
    let baseline_frame = client.capture().await.frame_id;
    events
        .wait_for("transfer.acked", Duration::from_secs(90), |event| {
            event.str_field("frame_id") == baseline_frame
        })
        .await;

    // ---------------------------------------------------------------------------------------
    // The stacking server goes away.
    // ---------------------------------------------------------------------------------------
    let outage_began = Instant::now();
    harness.stop("stack");
    eprintln!("-- the stacking server is down");

    let mut queued = Vec::new();
    for index in 1..=3 {
        queued.push(capture_and_save(&client, &events, index).await);
    }

    // Capture is unaffected: three frames were taken and saved with the far node dead. That is
    // ADD §5.4.4's degraded mode, and it is the property the whole two-node split is for.
    assert_eq!(
        queued.len(),
        3,
        "captures did not continue through the outage"
    );

    // The queue grew to hold them, and says it is offline rather than idle.
    let status = wait_until(
        "the transfer queue to report the outage",
        Duration::from_mins(1),
        || async {
            let status = client.transfer_status().await;
            (status["state"] == "offline" && status["queue_depth"].as_u64().unwrap_or(0) >= 3)
                .then_some(status)
        },
    )
    .await;
    eprintln!("-- queue: {status}");

    // `stack.status` goes offline too — and carries the *last known* frame count rather than
    // zero, which is the promise that panel makes (§4.3): a count that dropped to 0 would read as
    // "the stacking server lost my session".
    let offline = events
        .wait_for_since(
            "stack.status",
            outage_began,
            Duration::from_mins(1),
            |event| event.data["connected"] == serde_json::Value::Bool(false),
        )
        .await;
    assert!(
        offline.data["session_frame_count"].as_u64().unwrap_or(0) > 0,
        "an unreachable stacking server reported a zeroed frame count: {:?}",
        offline.data
    );
    assert!(
        offline.data["worker_state"].is_null(),
        "worker_state must be null when the node cannot be asked: {:?}",
        offline.data
    );

    // ---------------------------------------------------------------------------------------
    // One alert for the whole outage.
    // ---------------------------------------------------------------------------------------
    let during_outage: Vec<_> = harness_alerts(&events, "STACK_UNREACHABLE", outage_began);
    assert_eq!(
        during_outage.len(),
        1,
        "an outage must raise exactly one STACK_UNREACHABLE (SDD §5.10.2, change note 1.23.0); \
         got {} — {:?}",
        during_outage.len(),
        during_outage
            .iter()
            .map(|event| event.str_field("message").to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        during_outage[0].str_field("severity"),
        "warning",
        "the offline alert is a warning"
    );

    // ---------------------------------------------------------------------------------------
    // It comes back.
    // ---------------------------------------------------------------------------------------
    let recovery_began = Instant::now();
    harness.start("stack");
    harness
        .wait_stack_ready(astroctl_e2e::harness::READY_TIMEOUT)
        .await;
    eprintln!("-- the stacking server is back");

    wait_until("the queue to drain", Duration::from_mins(3), || async {
        let status = client.transfer_status().await;
        (status["queue_depth"] == 0 && status["state"] != "offline").then_some(())
    })
    .await;

    // Every queued frame acked, and each exactly once. "Exactly once" is the half that matters:
    // a retry that re-uploaded an already-stored frame would still drain the queue, and the
    // receiver would still dedup it, but the *ack event* firing twice would mean REL-13's
    // reclaim signal outran the record it stands for.
    for frame_id in &queued {
        let acks: Vec<_> = events
            .topic("transfer.acked")
            .into_iter()
            .filter(|event| event.str_field("frame_id") == *frame_id)
            .collect();
        assert_eq!(
            acks.len(),
            1,
            "{frame_id} was acked {} times, expected exactly once",
            acks.len()
        );
    }

    // One recovery alert, not one per drained frame.
    let recovered = harness_alerts(&events, "STACK_ONLINE", recovery_began);
    assert_eq!(
        recovered.len(),
        1,
        "recovery must raise exactly one STACK_ONLINE, got {}",
        recovered.len()
    );

    // And no second STACK_UNREACHABLE appeared while the queue drained.
    let after = harness_alerts(&events, "STACK_UNREACHABLE", recovery_began);
    assert!(
        after.is_empty(),
        "the link recovered but {} more STACK_UNREACHABLE alerts arrived",
        after.len()
    );

    // The frames really are on the far node now.
    let session_id = client.session().await["session_id"]
        .as_str()
        .expect("the session names itself")
        .to_owned();
    for frame_id in &queued {
        let path = format!("/data/astro/sessions/{session_id}/frames/{frame_id}.fits");
        assert!(
            harness.path_exists("stack", &path)
                || harness.path_exists(
                    "stack",
                    &format!("/data/astro/sessions/{session_id}/frames/{frame_id}.cr3")
                ),
            "{frame_id} drained from the queue but is not on the stacking server's volume"
        );
    }

    assert_eq!(
        events.closed(),
        None,
        "the /ws socket closed during the outage"
    );
    eprintln!("T-XFER-1 (stack death) ok: 3 frames queued and drained, 1 alert each way");
}

// ============================================================================================
// T-XFER-1, second half — the field node dies mid-session
// ============================================================================================

/// The field node is `SIGKILL`ed mid-session and comes back to the *same* session, with its frames,
/// its journal and its event log intact.
///
/// `kill -s SIGKILL` and not `stop`: a graceful shutdown flushes, and what REL-04/REL-06 promise
/// is about what survives when nothing gets to flush. The named volume is what makes the question
/// meaningful at all — `dev-down.sh` keeps volumes for exactly this reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_field_node_is_killed_mid_session_and_resumes_the_session_it_was_in() {
    let harness = Harness::attach();
    harness.ensure_pair_running().await;
    let client = harness.client();

    {
        let events = EventStream::connect(&client).await;
        ready_to_capture(&client).await;
        capture_and_save(&client, &events, 1).await;
    }

    let before = client.session().await;
    let session_before = before["session_id"]
        .as_str()
        .expect("the session names itself")
        .to_owned();
    let frames_before: Vec<String> = before["frames"]
        .as_array()
        .expect("a session lists its frames")
        .iter()
        .filter_map(|frame| frame["frame_id"].as_str().map(ToOwned::to_owned))
        .collect();
    let reserved_before = before["frames_reserved"]
        .as_u64()
        .expect("frames_reserved is a number");
    assert!(
        !frames_before.is_empty(),
        "the scenario needs at least one frame to lose"
    );

    // ---------------------------------------------------------------------------------------
    // Kill it.
    // ---------------------------------------------------------------------------------------
    eprintln!("-- SIGKILL the field node");
    harness.kill("field");
    harness.start("field");
    harness
        .wait_field_ready(astroctl_e2e::harness::READY_TIMEOUT)
        .await;
    eprintln!("-- the field node is back");

    // ---------------------------------------------------------------------------------------
    // The session continues; it is not a new one.
    // ---------------------------------------------------------------------------------------
    let after = client.session().await;
    assert_eq!(
        after["session_id"].as_str(),
        Some(session_before.as_str()),
        "the node started a second session instead of continuing the one `CURRENT` points at — \
         which is how two frames end up sharing an id (SDD §5.5 note 1, REL-04)"
    );

    let frames_after: Vec<String> = after["frames"]
        .as_array()
        .expect("a session lists its frames")
        .iter()
        .filter_map(|frame| frame["frame_id"].as_str().map(ToOwned::to_owned))
        .collect();
    for frame_id in &frames_before {
        assert!(
            frames_after.contains(frame_id),
            "{frame_id} was durable before the kill and is missing after it"
        );
    }

    // The counter did not rewind. A restart that reset it would hand the next capture an id that
    // already names a file on disk — the one thing REL-04 forbids outright.
    let reserved_after = after["frames_reserved"]
        .as_u64()
        .expect("frames_reserved is a number");
    assert!(
        reserved_after >= reserved_before,
        "the frame counter went backwards across a restart: {reserved_before} → {reserved_after}"
    );

    // ---------------------------------------------------------------------------------------
    // The event log survived, and the *only* damage a SIGKILL may leave is a short final line.
    // ---------------------------------------------------------------------------------------
    let replay = astroctl_e2e::replay::Replay::parse(&harness.field_event_log());
    assert!(
        replay.unparseable.len() <= 1,
        "a SIGKILL may truncate the line it was writing and nothing else; {} lines are \
         unparseable: {:?}",
        replay.unparseable.len(),
        replay.unparseable
    );
    if let Some((line, _)) = replay.unparseable.first() {
        let total = replay.events.len() + replay.unparseable.len();
        assert_eq!(
            *line, total,
            "the unparseable line is not the last one — a hole in the middle of the log is \
             corruption, not a truncated write"
        );
    }
    for frame_id in &frames_before {
        assert!(
            replay.saved_frames().contains(frame_id),
            "{frame_id} is on disk but the replayed event log does not mention saving it"
        );
    }

    // ---------------------------------------------------------------------------------------
    // Uploads resume, and the node can capture again.
    // ---------------------------------------------------------------------------------------
    let events = EventStream::connect(&client).await;
    ready_to_capture(&client).await;
    let frame_id = client.capture().await.frame_id;
    assert!(
        !frames_before.contains(&frame_id),
        "a restarted node reused the frame id {frame_id}"
    );
    events
        .wait_for("transfer.acked", Duration::from_mins(2), |event| {
            event.str_field("frame_id") == frame_id
        })
        .await;

    wait_until(
        "the queue to drain after the restart",
        Duration::from_mins(3),
        || async { (client.transfer_status().await["queue_depth"] == 0).then_some(()) },
    )
    .await;

    eprintln!("T-XFER-1 (field restart) ok: session {session_before} resumed, queue drained");
}

// ============================================================================================
// The mount's link falls out mid-slew
// ============================================================================================

/// The serial link dies during a goto.
///
/// # What the operator gets
///
///   * `mount.status` flips to `fault` within about a poll period, and
///   * `mount.position` simply stops, and
///   * `GET /api/mount/position` answers 502 `DEVICE_TRANSPORT` and says it is retryable, and
///   * **one `critical` `MOUNT_LINK_LOST` alert** within `mount.serial.heartbeat_misses` polls of
///     the loss, saying the mount was slewing when contact was lost — the sentence that sends
///     them out to the rig rather than back to the phone, because nothing about an unplugged
///     cable stops a motor.
///
/// The fourth line read **"no `alert` at all"** when M1-T16 wrote this scenario, and the
/// assertion below was left standing as a live record of REL-02's gap rather than quarantined:
/// `mount.serial.heartbeat_misses` had been shipped, documented and range-validated since M0 and
/// was read by nothing, which told the operator a protection existed. M1-T17 built the arm and
/// this assertion inverted from "no alert" to "one alert", which is what it was there for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_link_falls_out_mid_slew() {
    let harness = Harness::attach();
    harness.ensure_pair_running().await;

    // The only way a fault plan reaches a container: parsed once at startup from the environment
    // and moved into the factory (SDD §9, change note 1.23.0). Three seconds is measured from the
    // *connect* below, and is comfortably inside a slew that takes seven.
    harness
        .recreate_field(&[("ASTROCTL_SIM_FAULTS", "disconnect_after=3000")])
        .await;

    let client = harness.client();
    let events = EventStream::connect(&client).await;

    let connected_at = Instant::now();
    client.connect_mount().await;

    // Pick the far target so the slew outlives the link.
    let dec_before = client.get_json("/api/mount/position").await["dec"]
        .as_f64()
        .expect("dec is a number");
    let target_dec = if dec_before > f64::midpoint(TARGET_DEC_NEAR, TARGET_DEC_FAR) {
        TARGET_DEC_NEAR
    } else {
        TARGET_DEC_FAR
    };
    client.goto(TARGET_RA_HOURS, target_dec).await;
    events
        .wait_for_since(
            "mount.status",
            connected_at,
            Duration::from_secs(30),
            |event| event.str_field("state") == "slewing",
        )
        .await;

    // ---------------------------------------------------------------------------------------
    // The link falls out.
    // ---------------------------------------------------------------------------------------
    let faulted = events
        .wait_for_since(
            "mount.status",
            connected_at,
            Duration::from_secs(30),
            |event| event.str_field("state") == "fault",
        )
        .await;
    let noticed_at = faulted.at;
    eprintln!(
        "-- the mount faulted {:.1}s after connect",
        noticed_at.duration_since(connected_at).as_secs_f64()
    );

    // The position stream stops. This is what the operator sees first, and it is *all* they see:
    // there is no event that says why.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after_fault = events.topic_between(
        "mount.position",
        noticed_at + Duration::from_millis(500),
        Instant::now(),
    );
    assert!(
        after_fault.is_empty(),
        "the mount's link is down but {} more position events arrived — the poll is publishing \
         a position it cannot read",
        after_fault.len()
    );

    // The REST route is honest about why, and says so retryably: a link that fell out is worth
    // retrying, which is why this is DEVICE_TRANSPORT and not NOT_CONNECTED.
    let position = client.get("/api/mount/position").await;
    assert_eq!(position.status, 502, "expected a 502: {}", position.body);
    assert_eq!(position.error_code(), "DEVICE_TRANSPORT");
    assert_eq!(
        position.json()["retryable"],
        serde_json::Value::Bool(true),
        "a lost link is retryable: {}",
        position.body
    );

    // REL-02, the assertion this scenario exists for. One alert, not one per failed poll: the
    // link stays down for the rest of this test, so an arm that alerted on the condition rather
    // than on the transition would have raised several by now. `noticed_at` is the `mount.status`
    // flip, which the poll publishes one tick *after* the first miss, so the alert lands about a
    // poll period later and is comfortably inside the four seconds slept above.
    let losses = harness_alerts(&events, "MOUNT_LINK_LOST", noticed_at);
    assert_eq!(
        losses.len(),
        1,
        "REL-02 asks for exactly one link-loss alert; saw {}",
        losses.len()
    );
    assert_eq!(losses[0].str_field("severity"), "critical");
    // The tube is still moving — the whole reason this alert outranks a status badge.
    let message = losses[0].str_field("message");
    assert!(
        message.contains("slewing when contact was lost"),
        "the alert must say the mount was moving when the link went: {message}"
    );

    // Nothing else fired. The interrupted goto's own `DEVICE_TRANSPORT` warning comes later, when
    // the operator commands the mount again — see below.
    let alerts = events
        .topic("alert")
        .into_iter()
        .filter(|event| event.at > noticed_at)
        .collect::<Vec<_>>();
    assert_eq!(
        alerts.len(),
        1,
        "a lost link should be one alert and no more; saw: {:?}",
        alerts
            .iter()
            .map(|event| event.str_field("code").to_owned())
            .collect::<Vec<_>>()
    );

    // Commanding the mount is the one path that *does* produce an alert today: the goto task
    // publishes the device error it failed on.
    //
    // Getting there means waiting, and the wait is itself worth recording. The interrupted goto
    // keeps its in-flight slot until its *nominal* completion, because the simulated slew is a
    // plan evaluated against the clock and a dead link does not stop the tube — which is precisely
    // the hazard REL-02's watchdog is supposed to cover. Until that moment a second goto is
    // refused `409 BUSY`, correctly. So this polls rather than assuming, and a status other than
    // 202 or 409 is a genuine surprise.
    let commanded_at = Instant::now();
    wait_until(
        "the interrupted goto to release its in-flight slot",
        Duration::from_mins(2),
        || async {
            let reply = client
                .post(
                    "/api/mount/goto",
                    Some(serde_json::json!({
                        "ra_hours": TARGET_RA_HOURS,
                        "dec_degrees": target_dec,
                    })),
                )
                .await;
            match reply.status {
                202 => Some(()),
                409 => None,
                other => panic!(
                    "goto answered {other}, expected 202 or 409 BUSY: {}",
                    reply.body
                ),
            }
        },
    )
    .await;
    let alert = events
        .wait_for_since("alert", commanded_at, Duration::from_secs(30), |event| {
            event.str_field("code") == "DEVICE_TRANSPORT"
        })
        .await;
    assert_eq!(alert.str_field("severity"), "warning", "{:?}", alert.data);

    // ---------------------------------------------------------------------------------------
    // Recovery: the fault is spent, so reconnecting works and stays working.
    // ---------------------------------------------------------------------------------------
    let reconnected_at = Instant::now();
    client.connect_mount().await;
    events
        .wait_for_since(
            "mount.position",
            reconnected_at,
            Duration::from_secs(30),
            |_| true,
        )
        .await;
    let position = client.get("/api/mount/position").await;
    assert_eq!(
        position.status, 200,
        "the mount did not recover on reconnect: {}",
        position.body
    );

    // …and the critical alert is not left standing with no answer. One `info`, once — the same
    // edge trigger read from the other side.
    events
        .wait_for_since("alert", reconnected_at, Duration::from_secs(30), |event| {
            event.str_field("code") == "MOUNT_LINK_LOST"
        })
        .await;
    let recoveries = harness_alerts(&events, "MOUNT_LINK_LOST", reconnected_at);
    assert_eq!(
        recoveries.len(),
        1,
        "recovery is announced once; saw {}",
        recoveries.len()
    );
    assert_eq!(recoveries[0].str_field("severity"), "info");

    // Put the node back the way the next scenario needs it. `ensure_pair_running` would do this
    // anyway; doing it here as well keeps the cost off whichever test runs next.
    harness.recreate_field(&[]).await;
    eprintln!("mount link loss ok: one MOUNT_LINK_LOST critical naming the slew, recovered once");
}

/// Alerts with `code`, raised after `since`.
fn harness_alerts(events: &EventStream, code: &str, since: Instant) -> Vec<astroctl_e2e::Event> {
    events
        .alerts(code)
        .into_iter()
        .filter(|event| event.at > since)
        .collect()
}
