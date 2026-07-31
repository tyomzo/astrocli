//! **T-ISO-1** (SDD §9) — the PRF-04 thread-isolation guard.
//!
//! > While a capture + download runs (simulator configured with a realistic ~2 s blocking capture
//! > and a slow download), assert concurrently: `mount.position` events keep 1 Hz cadence with no
//! > gap > 1.5 s; `/api/mount/position` and `/api/system/health` p99 latency stays ≤ 2× the idle
//! > baseline; an e-stop issued mid-download still meets its ≤ 20 ms budget; the event bus never
//! > lags a subscriber. Repeat with a decode job saturating the blocking pool.
//!
//! # This is a regression guard, not a measurement
//!
//! §9 says so in as many words, and it changes how the file is written. The numbers it prints are
//! a by-product; its job is to fail the day somebody puts a blocking call on a runtime worker, or
//! sizes the runtime one-thread-per-core and starves the camera's OS thread, or awaits a decode on
//! the async executor. Every one of those is invisible to a unit test, harmless under light load,
//! and catastrophic at 2 a.m. on a four-core Pi with a 32 MB download in flight.
//!
//! # Every budget is a ratio to a baseline captured seconds earlier
//!
//! Every budget here is a ratio to a baseline measured from the same pair, in the same run,
//! immediately before the load — which is the difference between a guard that survives contact
//! with CI and one that gets `#[ignore]`d the first week a runner is busy.
//!
//! PRF-12's 20 ms e-stop budget is the one absolute number, and it is applied to the latency the
//! *load adds* rather than to the round trip. That is not a softening; it is the only honest
//! reading from outside the container, and the assertion carries the arithmetic. Briefly: a client
//! can time the HTTP round trip, which contains the simulated mount answering — a fixed 16 ms
//! serial exchange, measured on real hardware at 14.7–17.2 ms. The observed round trip is ~18 ms,
//! so ~16 ms of it is the device and ~2 ms is the node, and comparing that total to 20 ms would be
//! comparing the simulator's own constant to a budget it nearly fills, twenty times, on a shared
//! runner. T-SER-3 owns the instrumented handler-to-wire measurement; this owns the isolation.

use std::time::{Duration, Instant};

use astroctl_e2e::latency::{Measurement, Probe};
use astroctl_e2e::{percentile, EventStream, Harness};

/// §4.3's declared cadence for `mount.position`, and §9's tolerance on it.
const POSITION_CADENCE: Duration = Duration::from_secs(1);
const MAX_POSITION_GAP: Duration = Duration::from_millis(1500);

/// PRF-12's e-stop budget.
const ESTOP_BUDGET: Duration = Duration::from_millis(20);

/// §9's multiplier on the idle baseline.
const LATENCY_MULTIPLIER: u32 = 2;

/// How long the idle baseline runs. Long enough for a p99 to mean something at the probe interval
/// below (≈200 samples), short enough not to dominate the scenario.
const BASELINE_WINDOW: Duration = Duration::from_secs(10);

/// Gap between probe requests. The probe sleeps *between* requests rather than on a fixed period,
/// so a route that slows down produces fewer samples rather than overlapping ones — see
/// `latency::Probe`.
const PROBE_INTERVAL: Duration = Duration::from_millis(40);

/// The exposure for the load phase.
///
/// Two seconds, matching §9's "realistic ~2 s blocking capture". The *download* is the simulator's
/// measured R10 default (~2 s, `CameraProfile::download`) and is deliberately left alone: it is
/// the blocking phase this test is named for, and shortening it would be shortening the test.
const LOAD_SHUTTER: &str = "2";

/// What one load window produced.
struct Window {
    label: &'static str,
    position: Measurement,
    health: Measurement,
    estops: Vec<Duration>,
    position_gaps: Vec<Duration>,
    events_closed: Option<String>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn t_iso_1_a_blocking_capture_does_not_stall_the_node() {
    let harness = Harness::attach();
    harness.ensure_pair_running().await;
    let client = harness.client();
    let events = EventStream::connect(&client).await;

    client.connect_mount().await;
    client.connect_camera().await;
    client.set_shutter(LOAD_SHUTTER).await;

    // ---------------------------------------------------------------------------------------
    // The idle baseline — measured now, from this pair, on this machine.
    // ---------------------------------------------------------------------------------------
    let baseline = measure_idle(&client).await;
    eprintln!("-- idle baseline");
    eprintln!("   {}", baseline.position.summary());
    eprintln!("   {}", baseline.health.summary());
    eprintln!(
        "   e-stop p99 {:.1}ms over {} calls",
        percentile(&baseline.estops, 99.0).as_secs_f64() * 1000.0,
        baseline.estops.len()
    );

    // ---------------------------------------------------------------------------------------
    // Phase 1 — a blocking capture and its slow download.
    // ---------------------------------------------------------------------------------------
    let under_capture = measure_under_load(&client, &events, "capture", false).await;
    report(&under_capture, &baseline);

    // ---------------------------------------------------------------------------------------
    // Phase 2 — the same, with the decode pool saturated.
    //
    // Live view is what saturates it: every frame off the camera is decoded and re-encoded on the
    // blocking pool, continuously, while the capture's own download holds a thread of its own.
    // §9 asks for this second pass precisely because the first one can pass on a node whose pool
    // happens to have a spare thread.
    // ---------------------------------------------------------------------------------------
    let under_both = measure_under_load(&client, &events, "capture + saturated decode", true).await;
    report(&under_both, &baseline);

    // ---------------------------------------------------------------------------------------
    // The assertions, after both phases, so a failure in the first still prints the second.
    // ---------------------------------------------------------------------------------------
    for window in [&under_capture, &under_both] {
        assert_window(window, &baseline);
    }

    eprintln!("T-ISO-1 ok: both phases held the idle baseline");
}

/// Probe both routes and the e-stop with nothing else happening.
async fn measure_idle(client: &astroctl_e2e::Client) -> Baseline {
    let position = Probe::start(client.clone(), "/api/mount/position", PROBE_INTERVAL);
    let health = Probe::start(client.clone(), "/api/system/health", PROBE_INTERVAL);
    let estops = estop_series(client, BASELINE_WINDOW).await;
    Baseline {
        position: position.stop().await,
        health: health.stop().await,
        estops,
    }
}

struct Baseline {
    position: Measurement,
    health: Measurement,
    estops: Vec<Duration>,
}

/// Run one capture (optionally with live view saturating the decode pool) and measure everything
/// that is supposed to be unaffected by it.
async fn measure_under_load(
    client: &astroctl_e2e::Client,
    events: &EventStream,
    label: &'static str,
    saturate_decode: bool,
) -> Window {
    // The live-view socket has to be held open, not merely started: the field node stops producing
    // frames when nothing is listening, so a scenario that only POSTed `liveview/start` would
    // measure an idle decode pool and call it saturated.
    let _liveview = if saturate_decode {
        client
            .post("/api/camera/liveview/start", None)
            .await
            .expect_status(202);
        Some(astroctl_e2e::liveview::FrameSocket::liveview(client).await)
    } else {
        None
    };
    if saturate_decode {
        // Let the stream actually get going before the window opens.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let capture = client.capture().await;
    let opened = Instant::now();

    let position = Probe::start(client.clone(), "/api/mount/position", PROBE_INTERVAL);
    let health = Probe::start(client.clone(), "/api/system/health", PROBE_INTERVAL);

    // E-stops *during* the download, which is the half of the capture §9 names. The exposure comes
    // first and blocks nothing interesting; the download is where a blocking call on a runtime
    // worker would show up.
    events
        .wait_for("capture.progress", Duration::from_secs(90), |event| {
            event.str_field("frame_id") == capture.frame_id
                && event.str_field("state") == "downloading"
        })
        .await;
    let estops = estop_series(client, Duration::from_secs(4)).await;

    // Wait out the whole capture, so the window covers it end to end.
    events
        .wait_for("frame.saved", Duration::from_secs(90), |event| {
            event.str_field("frame_id") == capture.frame_id
        })
        .await;
    let closed = Instant::now();

    let position = position.stop().await.between(opened, closed);
    let health = health.stop().await.between(opened, closed);
    let position_gaps = events.gaps("mount.position", opened, closed);

    if saturate_decode {
        client
            .post("/api/camera/liveview/stop", None)
            .await
            .expect_status(202);
    }

    Window {
        label,
        position,
        health,
        estops,
        position_gaps,
        events_closed: events.closed(),
    }
}

/// Fire e-stops across `window`, returning each one's round-trip time.
///
/// Spaced rather than hammered: the point is to catch the executor being unavailable at an
/// arbitrary moment during a blocking operation, not to benchmark the route. Hammering it would
/// also make the *load* partly this function's own doing.
async fn estop_series(client: &astroctl_e2e::Client, window: Duration) -> Vec<Duration> {
    let deadline = Instant::now() + window;
    let mut samples = Vec::new();
    while Instant::now() < deadline {
        let reply = client.estop().await;
        assert_eq!(
            reply.status, 200,
            "the e-stop must never be refused: {}",
            reply.body
        );
        samples.push(reply.elapsed);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(!samples.is_empty(), "no e-stops were issued");
    samples
}

fn report(window: &Window, baseline: &Baseline) {
    eprintln!("-- under load: {}", window.label);
    eprintln!(
        "   {}  (idle p99 {:.1}ms)",
        window.position.summary(),
        baseline.position.p99().as_secs_f64() * 1000.0
    );
    eprintln!(
        "   {}  (idle p99 {:.1}ms)",
        window.health.summary(),
        baseline.health.p99().as_secs_f64() * 1000.0
    );
    eprintln!(
        "   e-stop max {:.1}ms over {} calls (idle max {:.1}ms)",
        window
            .estops
            .iter()
            .max()
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0,
        window.estops.len(),
        baseline
            .estops
            .iter()
            .max()
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0
    );
    eprintln!(
        "   mount.position: {} events, largest gap {:.2}s",
        window.position_gaps.len().saturating_sub(1),
        window
            .position_gaps
            .iter()
            .max()
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
    );
}

fn assert_window(window: &Window, baseline: &Baseline) {
    let label = window.label;

    // --- the event bus never lagged this subscriber ---------------------------------------
    //
    // From out here, "the bus lagged a subscriber" and "the client fell behind" have one symptom:
    // `/ws` closes the socket rather than dropping events. So a socket that is still open is the
    // assertion, and it is a strong one — it covers both halves of §5.8.3's shedding rule.
    assert_eq!(
        window.events_closed, None,
        "[{label}] the /ws socket was shed during the load; the bus lagged its subscriber"
    );

    // --- mount.position kept 1 Hz ----------------------------------------------------------
    let largest = window
        .position_gaps
        .iter()
        .max()
        .copied()
        .unwrap_or_default();
    assert!(
        largest <= MAX_POSITION_GAP,
        "[{label}] mount.position went quiet for {:.2}s, over the {:.1}s tolerance on its \
         {:.0}s cadence — the poll task was starved, which is exactly what PRF-04 forbids. \
         All gaps: {:?}",
        largest.as_secs_f64(),
        MAX_POSITION_GAP.as_secs_f64(),
        POSITION_CADENCE.as_secs_f64(),
        window
            .position_gaps
            .iter()
            .map(|gap| format!("{:.2}", gap.as_secs_f64()))
            .collect::<Vec<_>>()
    );

    // --- the read routes stayed within 2× idle ---------------------------------------------
    for (measured, idle) in [
        (&window.position, &baseline.position),
        (&window.health, &baseline.health),
    ] {
        assert!(
            measured.failures().is_empty(),
            "[{label}] {} answered a non-200 under load: {:?}",
            measured.path,
            measured.failures()
        );
        let budget = idle.p99() * LATENCY_MULTIPLIER;
        assert!(
            measured.p99() <= budget,
            "[{label}] {} p99 was {:.1}ms under load against a {:.1}ms idle p99 — over the \
             {LATENCY_MULTIPLIER}× budget of {:.1}ms. A read route slowing down while the camera \
             thread blocks means the two are sharing an executor.",
            measured.path,
            measured.p99().as_secs_f64() * 1000.0,
            idle.p99().as_secs_f64() * 1000.0,
            budget.as_secs_f64() * 1000.0
        );
    }

    // --- the e-stop was not slowed by the load ---------------------------------------------
    //
    // # What this measures, and what it deliberately does not
    //
    // PRF-12's budget is **handler-to-wire**: from the handler accepting the request to the stop
    // bytes leaving for the mount. What a client outside the container can time is the whole HTTP
    // round trip, which additionally contains the simulated mount *answering* — a fixed 16 ms
    // serial exchange (`SERIAL_ROUND_TRIP_MS`, measured on real hardware at 14.7–17.2 ms). So a
    // round trip of ~18 ms is ~16 ms of device and ~2 ms of node, and asserting that round trip
    // against 20 ms would be asserting mostly the simulator's own constant, a hair under the
    // budget, from twenty runs on a shared runner. That is a flake, not a guard. The instrumented
    // version of the absolute budget is T-SER-3's, which can see the wire.
    //
    // What *is* assertable from out here — and is what T-ISO-1 is actually for — is that the load
    // adds nothing. Two forms, because they fail differently: the ratio catches a proportional
    // slowdown, and the excess-over-idle catches a fixed stall that a generous baseline would
    // otherwise absorb. The excess is the node's own contribution under load, and PRF-12's 20 ms
    // is exactly the right number to hold it to.
    let worst = window.estops.iter().max().copied().unwrap_or_default();
    let idle_worst = baseline.estops.iter().max().copied().unwrap_or_default();
    let idle_typical = percentile(&baseline.estops, 50.0);
    let all = || {
        window
            .estops
            .iter()
            .map(|sample| format!("{:.1}", sample.as_secs_f64() * 1000.0))
            .collect::<Vec<_>>()
    };

    assert!(
        worst <= idle_worst * LATENCY_MULTIPLIER,
        "[{label}] the slowest e-stop under load took {:.1}ms against an idle worst of {:.1}ms — \
         over the {LATENCY_MULTIPLIER}× budget. The e-stop is sharing something with the camera. \
         All: {:?}",
        worst.as_secs_f64() * 1000.0,
        idle_worst.as_secs_f64() * 1000.0,
        all()
    );

    let added = worst.saturating_sub(idle_typical);
    assert!(
        added <= ESTOP_BUDGET,
        "[{label}] the load added {:.1}ms to the slowest e-stop ({:.1}ms against a {:.1}ms idle \
         median), over PRF-12's {:.0}ms. All: {:?}",
        added.as_secs_f64() * 1000.0,
        worst.as_secs_f64() * 1000.0,
        idle_typical.as_secs_f64() * 1000.0,
        ESTOP_BUDGET.as_secs_f64() * 1000.0,
        all()
    );
}
