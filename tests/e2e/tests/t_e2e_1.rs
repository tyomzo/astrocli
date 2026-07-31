//! **T-E2E-1** (SDD §9) — "Full API-level two-node session against simulator drivers: connect →
//! goto → capture → frame durable → transferred → acked → stub-worker preview returns through the
//! proxy; assert event stream shape."
//!
//! This is IMP §2/M1's exit narrative with assertions on it. It is deliberately written as one
//! long scenario rather than as a dozen small tests: the claim M1 makes is that the *whole* path
//! works end to end, and a suite of independent steps would pass on a system where each step works
//! and the joins between them do not. Every step's evidence is checked from two directions — the
//! event stream says it happened and the volume says it is there — because either one alone is a
//! claim the node makes about itself.
//!
//! Run it with `scripts/e2e.sh`.

use std::time::{Duration, Instant};

use astroctl_e2e::{liveview::FrameSocket, replay::Replay, wait_until, EventStream, Harness};
use serde_json::json;

/// How long a preview may take to come back from the stacking server, per SDD §9's T-E2E-1 row and
/// IMP §2/M1's "preview arrival ≤ 10 s each".
///
/// Measured from the moment the field node accepts the capture, not from the moment the frame is
/// durable: ten seconds is an *operator's* budget — the wait between pressing capture and seeing
/// the result — and it has to include the exposure, the download, the upload and the stretch.
const PREVIEW_BUDGET: Duration = Duration::from_secs(10);

/// The exposure the scenario uses.
///
/// One second rather than the config's 30: long enough that `exposing` is a state the event stream
/// genuinely passes through and not an instant that gets coalesced away, short enough that three
/// captures plus their 2 s simulated downloads fit in a suite that has to run twenty times without
/// anyone losing patience. The *download* is left at the profile's measured ~2 s, which is the
/// part T-ISO-1 cares about and the part it would be dishonest to shorten.
const SHUTTER: &str = "1";

/// Where the scenario slews to.
///
/// # Both declinations are circumpolar from the harness site, and that is not an aesthetic choice
///
/// `deploy/config/field-node.yaml` puts the node at Oslo, 59.91° N, and `mount.limits
/// .min_altitude_degrees` is 15. A target's *lowest* altitude over a day is `latitude − (90 −
/// dec)`, which for dec +50° is 19.9° and for dec +64° is 33.9° — both always above the limit.
/// M42, the obvious target and where the simulated camera points by default, sits at dec −5° and
/// from Oslo peaks at 25°: it clears the limit for a couple of hours a night and is refused with
/// `LIMIT_ALTITUDE` the rest of the time. A suite that has to run twenty times in a row must not
/// care what time it is, and this is the flake that would only ever have appeared overnight.
///
/// # Why two, and why the scenario picks between them
///
/// The pair keeps its volumes between runs, so the mount ends each run where the last one left
/// it. A fixed target would make every run after the first a zero-length goto, which never
/// publishes `slewing` and would quietly stop testing the transition. Picking whichever target
/// the mount is *not* near guarantees a real slew every time.
const TARGET_RA_HOURS: f64 = 5.60;
const TARGET_DEC_NEAR: f64 = 50.0;
const TARGET_DEC_FAR: f64 = 64.0;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_e2e_1_connect_goto_capture_transfer_preview() {
    let harness = Harness::attach();
    harness.ensure_pair_running().await;
    let client = harness.client();

    // ---------------------------------------------------------------------------------------
    // The operator opens the app: one socket for events, and — because the PWA's stack panel is
    // fed through the proxy (ADR-07) — one for the previews the stacking server pushes back.
    // ---------------------------------------------------------------------------------------
    let events = EventStream::connect(&client).await;
    let previews = FrameSocket::stack_preview(&client).await;

    // The connect snapshot is the protocol's promise that a client which arrives late still knows
    // the current state (§5.8.3). Asserting it is non-empty is asserting the promise is kept.
    assert!(
        !events.snapshot().is_empty(),
        "the connect snapshot carried no stateful topics at all"
    );

    // ---------------------------------------------------------------------------------------
    // connect
    // ---------------------------------------------------------------------------------------
    let mount = client.connect_mount().await;
    assert_eq!(mount["state"], "idle", "a connected mount is idle: {mount}");
    let camera = client.connect_camera().await;
    assert_eq!(
        camera["connected"], true,
        "camera did not connect: {camera}"
    );

    events
        .wait_for("mount.status", Duration::from_secs(10), |event| {
            event.str_field("state") == "idle"
        })
        .await;
    events
        .wait_for("camera.status", Duration::from_secs(10), |event| {
            event.data["connected"] == json!(true)
        })
        .await;

    // A connected mount polls its position at 1 Hz (MNT-02). Two events is proof the poll is
    // running rather than that one snapshot happened to be published.
    wait_until(
        "the 1 Hz position poll",
        Duration::from_secs(10),
        || async { (events.topic("mount.position").len() >= 2).then_some(()) },
    )
    .await;

    client.set_shutter(SHUTTER).await;

    // ---------------------------------------------------------------------------------------
    // goto
    // ---------------------------------------------------------------------------------------
    let before = client.get_json("/api/mount/position").await;
    let dec_before = before["dec"].as_f64().expect("dec is a number");
    let target_dec = if dec_before > f64::midpoint(TARGET_DEC_NEAR, TARGET_DEC_FAR) {
        TARGET_DEC_NEAR
    } else {
        TARGET_DEC_FAR
    };

    let commanded_at = Instant::now();
    let correlation = client.goto(TARGET_RA_HOURS, target_dec).await;
    assert_eq!(correlation.len(), 32, "a correlation id is 32 hex chars");

    // Slewing is a state the mount must be observed *entering*, not merely a state it is in when
    // asked. A node that answered `slewing` on a status GET but never published it would leave the
    // PWA's pointing readout static through the one moment it matters.
    //
    // `wait_for_since` and not `wait_for`, here and below, because both states existed before the
    // command: the mount was `idle` when it connected, so a plain wait for `idle` after issuing a
    // goto is satisfied by the event from before the slew and the assertion that follows is made
    // against a mount that never moved.
    events
        .wait_for_since(
            "mount.status",
            commanded_at,
            Duration::from_secs(30),
            |event| event.str_field("state") == "slewing",
        )
        .await;
    let settled = events
        .wait_for_since(
            "mount.status",
            commanded_at,
            Duration::from_mins(2),
            |event| event.str_field("state") == "idle",
        )
        .await;

    let position = client.get_json("/api/mount/position").await;
    let ra = position["ra"].as_f64().expect("ra is a number");
    let dec = position["dec"].as_f64().expect("dec is a number");
    // Slack in both axes: the simulator models a settle and a damped post-slew oscillation
    // (SimulatorProfile), and RA is recovered as `LST − HA` so it advances while the mount stands
    // still — demanding exactness here would be demanding the simulator be less realistic than it
    // is. A degree is far tighter than the failure this guards against, which is not moving.
    assert!(
        (ra - TARGET_RA_HOURS).abs() < 0.1 && (dec - target_dec).abs() < 1.0,
        "the mount settled at ra={ra} dec={dec}, not near the target \
         ({TARGET_RA_HOURS}, {target_dec}); last status {:?}",
        settled.data
    );

    // ---------------------------------------------------------------------------------------
    // capture ×3 — the whole path, once per frame
    // ---------------------------------------------------------------------------------------
    let session_id = client.session().await["session_id"]
        .as_str()
        .expect("the session names itself")
        .to_owned();

    let mut captured: Vec<Captured> = Vec::new();
    for index in 1..=3 {
        let capture = client.capture().await;
        let frame_id = capture.frame_id.clone();
        let started = capture.accepted_at;
        eprintln!("-- capture {index}/3: {frame_id}");

        // §4.3's capture.progress states, in order. `exposing` before `saved` is the assertion; a
        // node that published only the terminal state would give the operator a progress bar that
        // jumps from nothing to done.
        for state in ["exposing", "downloading", "saved"] {
            events
                .wait_for("capture.progress", Duration::from_secs(90), |event| {
                    event.str_field("frame_id") == frame_id && event.str_field("state") == state
                })
                .await;
        }

        let saved = events
            .wait_for("frame.saved", Duration::from_secs(30), |event| {
                event.str_field("frame_id") == frame_id
            })
            .await;
        let path = saved.str_field("path").to_owned();
        let sha256 = saved.str_field("sha256").to_owned();
        assert_eq!(sha256.len(), 64, "frame.saved carries a sha256: {saved:?}");
        assert!(
            saved.data["size_bytes"].as_u64().unwrap_or(0) > 0,
            "a saved frame has bytes: {saved:?}"
        );

        // Durable on the field node's own volume — the event says so, the filesystem confirms it.
        assert!(
            harness.path_exists("field", &path),
            "frame.saved named {path} but it is not on the field volume"
        );

        // Acked by the stacking server. The event is published only *after* the journal row is
        // written (§5.10), so its arrival is also the assertion that the durable record precedes
        // the announcement.
        let acked = events
            .wait_for("transfer.acked", PREVIEW_BUDGET, |event| {
                event.str_field("frame_id") == frame_id
            })
            .await;
        assert_eq!(
            acked.str_field("sha256"),
            sha256,
            "the ack echoed a different checksum than the frame was saved with"
        );

        // Durable on the *other* node's volume, at the mirrored layout of §5.11.2.
        let ext = path.rsplit('.').next().unwrap_or("cr3").to_owned();
        let stack_path = format!("/data/astro/sessions/{session_id}/frames/{frame_id}.{ext}");
        assert!(
            harness.path_exists("stack", &stack_path),
            "the stacking server acked {frame_id} but {stack_path} is not on its volume"
        );

        // And the preview comes back the way the operator sees it: pushed by the stacking server,
        // through the field node's proxy, onto the socket the browser holds.
        let elapsed = previews
            .wait_for_frame_id(&frame_id, started, PREVIEW_BUDGET)
            .await;
        eprintln!("   preview back in {:.2}s", elapsed.as_secs_f64());
        assert!(
            elapsed <= PREVIEW_BUDGET,
            "the preview for {frame_id} took {elapsed:?}, over the {PREVIEW_BUDGET:?} budget"
        );

        captured.push(Captured {
            frame_id,
            sha256,
            preview_latency: elapsed,
        });
    }

    // ---------------------------------------------------------------------------------------
    // The event stream's shape
    // ---------------------------------------------------------------------------------------
    let order = events.first_seen_order();
    let position_of = |topic: &str| {
        order
            .iter()
            .position(|seen| seen == topic)
            .unwrap_or_else(|| panic!("`{topic}` never appeared; the stream carried {order:?}"))
    };
    // Not a total order — `mount.position` and `stack.status` are periodic and may lead anything.
    // What must hold is the causal chain: a frame is saved before it is acked, and a capture
    // reports progress before it saves.
    assert!(
        position_of("capture.progress") < position_of("frame.saved"),
        "frame.saved preceded any capture.progress: {order:?}"
    );
    assert!(
        position_of("frame.saved") < position_of("transfer.acked"),
        "a frame was acked before it was saved: {order:?}"
    );
    for required in [
        "mount.position",
        "mount.status",
        "camera.status",
        "capture.progress",
        "frame.saved",
        "transfer.acked",
        "transfer.status",
        "stack.status",
    ] {
        assert!(
            order.iter().any(|seen| seen == required),
            "`{required}` never appeared on /ws; the stream carried {order:?}"
        );
    }

    // The socket must still be open. `/ws` closes rather than dropping events when a client falls
    // behind, so a close here would mean the assertions above were made on a truncated stream.
    assert_eq!(
        events.closed(),
        None,
        "the /ws socket closed during the scenario"
    );

    // ---------------------------------------------------------------------------------------
    // The API's own view agrees with the stream
    // ---------------------------------------------------------------------------------------
    let session = client.session().await;
    let listed: Vec<String> = session["frames"]
        .as_array()
        .expect("the session lists its frames")
        .iter()
        .filter_map(|frame| frame["frame_id"].as_str().map(ToOwned::to_owned))
        .collect();
    for frame in &captured {
        assert!(
            listed.contains(&frame.frame_id),
            "{} is not in the session's frame list",
            frame.frame_id
        );
    }

    let transfer = client.transfer_status().await;
    assert_eq!(
        transfer["queue_depth"], 0,
        "three frames were acked but the queue is not empty: {transfer}"
    );
    assert!(
        transfer["last_ack_ts"].is_string(),
        "the transfer status reports no last ack: {transfer}"
    );

    // ---------------------------------------------------------------------------------------
    // SES-07 — the session log replays to the state the API reports
    // ---------------------------------------------------------------------------------------
    let replay = Replay::parse(&harness.field_event_log());
    assert!(
        replay.unparseable.is_empty(),
        "the session log has unparseable lines: {:?}",
        replay.unparseable
    );
    replay.assert_envelopes();

    let final_state = replay.final_state();
    let logged_mount = &final_state["mount.status"];
    let live_mount = client.get_json("/api/mount/status").await;
    assert_eq!(
        logged_mount["state"], live_mount["state"],
        "replaying the log gives mount.state {} but the API says {}",
        logged_mount["state"], live_mount["state"]
    );
    assert_eq!(
        final_state["transfer.status"]["queue_depth"], transfer["queue_depth"],
        "the log's final transfer depth disagrees with /api/transfer/status"
    );

    let saved_in_log = replay.saved_frames();
    let acked_in_log = replay.acked_frames();
    for frame in &captured {
        assert!(
            saved_in_log.contains(&frame.frame_id),
            "{} is not in the replayed log's saved frames",
            frame.frame_id
        );
        assert!(
            acked_in_log.contains(&frame.frame_id),
            "{} is not in the replayed log's acked frames",
            frame.frame_id
        );
    }

    eprintln!(
        "T-E2E-1 ok: 3 frames, preview latencies {:?}",
        captured
            .iter()
            .map(|frame| format!("{:.2}s", frame.preview_latency.as_secs_f64()))
            .collect::<Vec<_>>()
    );
}

struct Captured {
    frame_id: String,
    #[allow(
        dead_code,
        reason = "kept for the failure message when an ack echoes the wrong sha"
    )]
    sha256: String,
    preview_latency: Duration,
}
