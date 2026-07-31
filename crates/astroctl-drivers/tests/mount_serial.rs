//! **T-SER-1** and **T-SER-3** — the gated suite for the Sky-Watcher serial task
//! (SDD §5.2.4, task M3-T02).
//!
//! Every test here is named `t_ser_1_*` or `t_ser_3_*`, so `cargo test t_ser_1` and
//! `cargo test t_ser_3` are exactly those gates and nothing else. The module tests in
//! `src/skywatcher/serial.rs` cover the pieces; this file covers the two things only a
//! whole-link test can:
//!
//! * **T-SER-1** — that a timeout, a retry, a garbled reply and a pulled cable each produce the
//!   right verdict *and* the right heartbeat accounting, which are separate questions with
//!   different answers.
//! * **T-SER-3** — that an emergency stop reaches the wire inside its budget while the normal
//!   lane is loaded, and that no new normal frame is transmitted once it is waiting.
//!
//! # Everything here runs on virtual time
//!
//! `#[tokio::test(start_paused = true)]`, and the mock port's delays are `tokio::time::sleep`.
//! T-SER-3's thousand iterations therefore cost no wall-clock time and every latency below is an
//! exact figure rather than a measurement with a scheduler's noise in it. The round trip the mock
//! charges is the measured one — 16 ms, from 2000 exchanges spanning 14.7–17.2 ms
//! (`spikes/skywatcher-heq5/FINDINGS.md`) — so the numbers mean what they say.

#![cfg(feature = "skywatcher")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_core::types::Axis;
use astroctl_drivers::skywatcher::codec::{Counts, GetPosition, InstantStop, MountError};
use astroctl_drivers::skywatcher::mock_port::{MockPort, Scripted, Written};
use astroctl_drivers::skywatcher::port::WriteGate;
use astroctl_drivers::skywatcher::serial::{
    watchdog_channel, Heartbeat, SerialLink, SerialTimings, WatchdogSource,
};
use tokio::time::Instant;

/// A link over a fresh mock port. The port handle is the test's window on the wire.
fn link(timings: SerialTimings) -> (MockPort, Arc<SerialLink>, WatchdogSource) {
    let port = MockPort::new();
    let (sink, watchdog) = watchdog_channel();
    let (link, _task) = SerialLink::spawn(port.factory(), WriteGate::Actions, timings, sink);
    (port, Arc::new(link), watchdog)
}

/// Timings with no retry, so one request is one exchange and the accounting is unambiguous.
fn no_retries() -> SerialTimings {
    SerialTimings {
        retries: 0,
        ..SerialTimings::default()
    }
}

// ===========================================================================================
// T-SER-1 — timeout, retry-then-fail, garbled response, reconnect
// ===========================================================================================

#[tokio::test(start_paused = true)]
async fn t_ser_1_a_mount_that_says_nothing_times_out_at_the_configured_budget() {
    let (port, link, _watchdog) = link(no_retries());
    port.goes_quiet_for(1);

    let began = Instant::now();
    let error = link
        .send(GetPosition(Axis::Ra))
        .await
        .expect_err("a silent mount is a timeout");

    assert!(
        matches!(error, DeviceError::Timeout(budget) if budget == Duration::from_millis(500)),
        "{error:?}"
    );
    // The budget, not a fraction of it and not a multiple: 500 ms is ~29x the measured 17.2 ms
    // worst case, so a link that has genuinely stopped is the only thing that reaches it.
    assert_eq!(began.elapsed(), Duration::from_millis(500));
    assert_eq!(port.exchanges(), 1, "no retry was configured");
}

#[tokio::test(start_paused = true)]
async fn t_ser_1_a_timeout_is_retried_once_and_then_reported() {
    // SDD §5.2.4: one retry on timeout, then `DeviceError::Timeout`. Both halves, in one test,
    // because the interesting property is that the retry is bounded rather than that it happens.
    let (port, link, _watchdog) = link(SerialTimings::default());

    // One miss, then the mount answers: the retry is what the caller sees, not the miss.
    port.goes_quiet_for(1);
    let began = Instant::now();
    assert_eq!(
        link.send(GetPosition(Axis::Ra))
            .await
            .expect("the retry found a mount that was answering again"),
        Counts::HOME
    );
    assert_eq!(port.exchanges(), 2);
    assert_eq!(began.elapsed(), Duration::from_millis(516), "500 + 16");
    assert_eq!(link.consecutive_misses(), 0, "a request that succeeded");

    // Two misses: the retry is spent and the caller gets the timeout.
    port.goes_quiet_for(2);
    let error = link
        .send(GetPosition(Axis::Ra))
        .await
        .expect_err("both attempts were silent");
    assert!(matches!(error, DeviceError::Timeout(_)), "{error:?}");
    assert_eq!(port.exchanges(), 4, "two attempts, and not a third");
    assert_eq!(
        link.consecutive_misses(),
        1,
        "one *request* failed, not two"
    );
}

#[tokio::test(start_paused = true)]
async fn t_ser_1_every_shape_of_garbled_reply_counts_as_a_failure() {
    // "Garbled response (codec error) counts as a failure" — and there are three shapes of it,
    // caught in three different places. All three must reach the same verdict, because a driver
    // that retried one and not the others would be retrying by accident.
    for (label, fault) in [
        // The real hardware corruption: two frames interleaved into one over-long reply.
        ("framing", Scripted::Garbles),
        // Framing intact, payload the wrong width. Only `Command::decode` can see this one, and
        // it is the one a decoder that assumed six characters would return a wrong number for.
        ("payload width", Scripted::Mangles),
        // No terminator at all, more bytes than any frame — a wedged adapter or the wrong baud.
        ("flood", Scripted::Floods),
    ] {
        let (port, link, _watchdog) = link(no_retries());
        port.script([fault]);
        let error = link
            .send(GetPosition(Axis::Ra))
            .await
            .expect_err(&format!("{label} must not decode to a position"));
        assert!(
            matches!(error, DeviceError::Protocol(_)),
            "{label}: {error:?}"
        );
        assert_eq!(port.exchanges(), 1, "{label}");
        assert_eq!(
            link.consecutive_misses(),
            1,
            "{label} must count against the heartbeat exactly as a timeout does"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn t_ser_1_a_garbled_reply_is_retried_and_the_retry_is_believed() {
    let (port, link, _watchdog) = link(SerialTimings::default());
    port.garbles_next(1);

    assert_eq!(
        link.send(GetPosition(Axis::Ra))
            .await
            .expect("the second attempt was clean"),
        Counts::HOME
    );
    assert_eq!(port.exchanges(), 2);

    // ...and when both attempts are garbage, the caller learns it is a protocol fault rather than
    // a timeout. The distinction matters: one means the mount is mute, the other that something
    // is on the wire and it is not Synta.
    port.garbles_next(2);
    let error = link
        .send(GetPosition(Axis::Ra))
        .await
        .expect_err("both attempts were corrupt");
    assert!(matches!(error, DeviceError::Protocol(_)), "{error:?}");
    assert_eq!(link.consecutive_misses(), 1);
}

#[tokio::test(start_paused = true)]
async fn t_ser_1_a_refusal_is_an_answer_and_is_neither_retried_nor_counted() {
    // The reason M3-T01 made `MountError` and `ProtocolError` distinct types: `!1` means the
    // frame arrived, was understood and was declined. Retrying produces the same `!1` forever,
    // and counting it against the heartbeat would report a dead link over a perfectly live one.
    let (port, link, _watchdog) = link(SerialTimings::default());
    port.script([Scripted::Refuses(MountError::NotInitialised)]);

    let error = link
        .send(GetPosition(Axis::Ra))
        .await
        .expect_err("the mount declined");
    assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");
    assert!(error.to_string().contains("not initialised"), "{error}");
    assert_eq!(port.exchanges(), 1, "a settled answer is not retried");
    assert_eq!(link.consecutive_misses(), 0, "and is not a heartbeat miss");
}

#[tokio::test(start_paused = true)]
async fn t_ser_1_a_pulled_cable_is_reported_and_the_link_comes_back_when_it_is_replugged() {
    let timings = SerialTimings {
        reopen_backoff: Duration::from_millis(250),
        ..no_retries()
    };
    let (port, link, _watchdog) = link(timings);

    assert_eq!(
        link.send(GetPosition(Axis::Ra)).await.expect("healthy"),
        Counts::HOME
    );
    let opens_before = port.opens();

    // The cable comes out mid-session.
    port.disconnect();
    let error = link
        .send(GetPosition(Axis::Ra))
        .await
        .expect_err("the port is gone");
    assert!(matches!(error, DeviceError::Transport(_)), "{error:?}");

    // While it is out, requests fail fast rather than each waiting a full timeout — a poll loop
    // must not turn a missing cable into a 500 ms stall once a second.
    let began = Instant::now();
    assert!(link.send(GetPosition(Axis::Ra)).await.is_err());
    assert!(
        began.elapsed() < Duration::from_millis(100),
        "a down link answered in {:?}, which is a timeout rather than a refusal",
        began.elapsed()
    );

    // It goes back in, but the first reopen attempt still loses the race with udev.
    port.reconnect();
    port.refuses_opens(1);
    // Wait past the backoff, so the reopen is due.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = link.send(GetPosition(Axis::Ra)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        link.send(GetPosition(Axis::Ra))
            .await
            .expect("the link recovered without anyone reconnecting it by hand"),
        Counts::HOME
    );
    assert!(
        port.opens() > opens_before,
        "recovery means the port was actually reopened, not that the error stopped"
    );
    assert_eq!(link.consecutive_misses(), 0, "a success clears the streak");
}

#[tokio::test(start_paused = true)]
async fn t_ser_1_the_heartbeat_fires_after_exactly_three_misses_and_recovers_cleanly() {
    let timings = SerialTimings {
        heartbeat_misses: 3,
        ..no_retries()
    };
    let (port, link, mut watchdog) = link(timings);

    // Two misses is not a fault. The threshold is validated against hardware: 2000 consecutive
    // exchanges produced zero timeouts and zero malformed replies with a 2.5 ms spread, so three
    // in a row is a signal rather than noise (`spikes/skywatcher-heq5/FINDINGS.md`).
    port.goes_quiet_for(2);
    for expected in 1..=2 {
        assert!(link.send(GetPosition(Axis::Ra)).await.is_err());
        assert_eq!(link.consecutive_misses(), expected);
    }
    assert!(
        watchdog.try_recv().is_err(),
        "two misses must not wake the watchdog"
    );

    // The third does.
    port.goes_quiet_for(1);
    assert!(link.send(GetPosition(Axis::Ra)).await.is_err());
    assert_eq!(watchdog.try_recv(), Ok(Heartbeat::Lost { misses: 3 }));

    // ...and only once. One alert per transition, not one per failed request — the discipline
    // the rest of the system applies to alerts.
    port.goes_quiet_for(3);
    for _ in 0..3 {
        assert!(link.send(GetPosition(Axis::Ra)).await.is_err());
    }
    assert!(
        watchdog.try_recv().is_err(),
        "the link did not become lost a second time"
    );
    assert_eq!(link.consecutive_misses(), 6);

    // The mount answers again: one recovery, and the streak is back to zero.
    assert_eq!(
        link.send(GetPosition(Axis::Ra)).await.expect("answering"),
        Counts::HOME
    );
    assert_eq!(watchdog.try_recv(), Ok(Heartbeat::Recovered));
    assert!(watchdog.try_recv().is_err());
    assert_eq!(link.consecutive_misses(), 0);
}

#[tokio::test(start_paused = true)]
async fn t_ser_1_a_goto_worth_of_frames_never_pipelines() {
    // The one framing rule the hardware imposed rather than the design choosing: two frames in
    // one write provably interleave the replies (`=0000=000080`). Eight frames back to back is
    // what a goto costs, so this is the shape that would expose a pipelining bug.
    let (port, link, _watchdog) = link(SerialTimings::default());
    let began = Instant::now();
    for _ in 0..8 {
        assert!(link.send(GetPosition(Axis::Ra)).await.is_ok());
    }
    assert_eq!(port.writes().len(), 8);
    // Eight serial round trips and not one exchange less: 128 ms of dead air before a goto moves,
    // which is the figure the simulator charges its callers for the same reason.
    assert_eq!(began.elapsed(), Duration::from_millis(8 * 16));
}

// ===========================================================================================
// T-SER-3 — the priority lane
// ===========================================================================================

/// SDD §5.8.2's budget from API handler to bytes on the wire.
const ESTOP_BUDGET: Duration = Duration::from_millis(20);

#[tokio::test(start_paused = true)]
async fn t_ser_3_no_new_normal_frame_reaches_the_wire_while_a_stop_is_waiting() {
    // The lane rule itself, stated as SDD §5.2.4 states it: the in-flight normal completes, and
    // no *new* one starts. Eight are queued behind the one in flight; exactly one of them — the
    // one already on the wire — may be transmitted before the `L`.
    let (port, link, _watchdog) = link(SerialTimings::default());

    let queued: Vec<_> = (0..8)
        .map(|_| {
            let link = Arc::clone(&link);
            tokio::spawn(async move { link.send(GetPosition(Axis::Ra)).await })
        })
        .collect();

    // Mid-exchange of the first: one frame is on the wire and seven are queued behind it.
    tokio::time::sleep(Duration::from_millis(8)).await;
    assert_eq!(port.writes().len(), 1, "one normal frame is in flight");

    link.send_priority(InstantStop(Axis::Ra))
        .await
        .expect("the stop reaches the mount");

    // Everything transmitted *before* the stop. The lane reopens the moment the stop is answered,
    // so the interesting window closes as soon as it lands and the assertion must too.
    let wire: Vec<u8> = port.writes().iter().filter_map(Written::opcode).collect();
    let stop = wire
        .iter()
        .position(|&opcode| opcode == b'L')
        .expect("the stop reached the wire");
    assert_eq!(
        &wire[..stop],
        b"j",
        "the frame already in flight finished and then the stop went out; nothing from the queue \
         of seven got between them (wire was {wire:?})"
    );

    for task in queued {
        let _ = task.await;
    }
}

#[tokio::test(start_paused = true)]
async fn t_ser_3_a_stop_reaches_a_busy_wire_inside_budget_over_a_thousand_iterations() {
    // The acceptance criterion: a priority request injected under 50 cmd/s of normal load reaches
    // the wire within 20 ms, p99 over 1000 iterations.
    //
    // The load is a 20 ms interval — 50 commands a second at the measured 16 ms an exchange, so
    // the wire is 80% busy and there is nearly always a frame in flight for the stop to wait
    // behind. Virtual time makes the whole run free and every figure exact.
    let iterations: usize = std::env::var("ASTROCTL_SER3_ITERS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(1000);

    let (port, link, _watchdog) = link(SerialTimings::default());

    // The load is a tight loop rather than a 20 ms interval, and the first version of this test
    // was the interval. It produced a wire that was idle at 271 of the 1000 injections — the
    // interval and the measurement's own spacing interleaved into gaps, so a quarter of the
    // "under load" measurements were taken with nothing to be under. A loop that re-sends the
    // instant its answer arrives is the honest maximum a 16 ms round trip supports (62.5 cmd/s),
    // and what it costs is that the achieved rate has to be measured rather than assumed — which
    // is the assertion at the bottom.
    let sent = Arc::new(AtomicU32::new(0));
    let load = tokio::spawn({
        let link = Arc::clone(&link);
        let sent = Arc::clone(&sent);
        async move {
            loop {
                if link.send(GetPosition(Axis::Ra)).await.is_ok() {
                    sent.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });

    // Let the load settle so the first measurement is taken against a busy wire rather than an
    // idle one.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (began, before) = (Instant::now(), sent.load(Ordering::Relaxed));

    let mut latencies = Vec::with_capacity(iterations);
    for iteration in 0..iterations {
        port.clear_writes();
        let injected = Instant::now();
        link.send_priority(InstantStop(Axis::Ra))
            .await
            .expect("the stop reaches the mount");
        let landed = port
            .writes()
            .into_iter()
            .find(|written| written.opcode() == Some(b'L'))
            .unwrap_or_else(|| panic!("iteration {iteration}: no stop frame reached the wire"))
            .at;
        latencies.push(landed - injected);
        // Space the injections out so the normal lane keeps its 50 cmd/s rather than being
        // starved by the measurement itself — and walk the gap by a millisecond each time.
        //
        // The walk is the difference between a thousand measurements and one measurement taken a
        // thousand times. Virtual time has no jitter, so a fixed gap against a fixed 20 ms load
        // phase-locks: the first version of this test injected at the same point in the exchange
        // every iteration and reported p50 = p99 = max = 12 ms, which is one sample wearing a
        // percentile's clothes. Sweeping a full round trip visits every phase, including the
        // worst one — a stop injected the instant a normal frame goes out.
        #[allow(clippy::cast_possible_truncation)]
        let phase = (iteration % 37) as u64;
        tokio::time::sleep(Duration::from_millis(83 + phase)).await;
    }
    let carried = f64::from(sent.load(Ordering::Relaxed) - before) / began.elapsed().as_secs_f64();
    load.abort();

    // The load the criterion names, evidenced rather than intended.
    assert!(
        carried >= 50.0,
        "the normal lane carried {carried:.1} cmd/s; the criterion measures against 50"
    );

    latencies.sort_unstable();
    let at = |fraction: f64| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = ((latencies.len() as f64 * fraction) as usize).min(latencies.len() - 1);
        latencies[index]
    };
    let (p50, p99, worst) = (at(0.50), at(0.99), latencies[latencies.len() - 1]);

    // Observed, at the time of writing and deterministically under virtual time:
    // 54.5 cmd/s carried, 54 of 1000 injections found the wire momentarily free,
    // min 0 ms, p50 9 ms, p99 15 ms, max 15 ms.
    assert!(
        p99 <= ESTOP_BUDGET,
        "p99 {p99:?} exceeds the {ESTOP_BUDGET:?} budget (p50 {p50:?}, max {worst:?})"
    );
    // The worst case is bounded by one round trip, not by the request timeout — which is the
    // whole claim of the lane. Asserted separately so a regression that merely squeaked under the
    // p99 while lengthening the tail is still a failure.
    assert!(
        worst <= Duration::from_millis(17),
        "the worst stop waited {worst:?}; one measured round trip is 17.2 ms and nothing should \
         wait longer than the exchange it is behind"
    );
    assert_eq!(latencies.len(), iterations);
}

#[tokio::test(start_paused = true)]
async fn t_ser_3_a_stop_does_not_wait_out_a_stalled_normal_exchange() {
    // The case SDD §5.2.4 does not cover and §5.4 walks straight into: the watchdog issues a
    // priority-lane stop *because* the link went quiet, so the stop is queued behind a normal
    // exchange that is going to sit there for the full 500 ms timeout. At the measured 835x
    // sidereal cruise that is ~43,700 counts — 1.7 degrees of unwanted slew, on the one path whose
    // purpose is to stop one.
    let (port, link, _watchdog) = link(SerialTimings::default());

    port.goes_quiet_for(1);
    let stalled = tokio::spawn({
        let link = Arc::clone(&link);
        async move { link.send(GetPosition(Axis::Ra)).await }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(port.writes().len(), 1, "the poll is on the wire and stuck");

    let injected = Instant::now();
    link.send_priority(InstantStop(Axis::Ra))
        .await
        .expect("the stop reaches the mount");
    let landed = port
        .writes()
        .into_iter()
        .find(|written| written.opcode() == Some(b'L'))
        .expect("the stop frame")
        .at;

    let waited = landed - injected;
    assert!(
        waited <= ESTOP_BUDGET,
        "the stop waited {waited:?} behind a stalled poll; the bound is one round trip and the \
         timeout it would otherwise have sat through is 500 ms"
    );

    // The poll that lost the cable is retried rather than failed, and the retry is answered. So
    // the cost of pre-empting a stall is one extra exchange for the poll, not an error for it.
    assert_eq!(
        stalled
            .await
            .expect("the poll task finished")
            .expect("the abandoned poll was retried and answered"),
        Counts::HOME
    );
    assert_eq!(port.exchanges(), 3, "the stall, the stop, then the retry");
    assert_eq!(
        link.consecutive_misses(),
        0,
        "losing the cable to an emergency stop says nothing about whether the cable works"
    );
}

#[tokio::test(start_paused = true)]
async fn t_ser_3_a_request_that_only_ever_loses_the_cable_is_told_so_and_is_not_a_miss() {
    // With no retry left, a pre-empted request has to surface. `Busy` and not `Timeout`: the
    // mount was never given the chance to answer, and reporting a timeout would make the
    // heartbeat and the operator both believe the link had failed when it had not.
    let (port, link, mut watchdog) = link(SerialTimings {
        heartbeat_misses: 1,
        ..no_retries()
    });

    port.goes_quiet_for(1);
    let stalled = tokio::spawn({
        let link = Arc::clone(&link);
        async move { link.send(GetPosition(Axis::Ra)).await }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    link.send_priority(InstantStop(Axis::Ra))
        .await
        .expect("the stop reaches the mount");

    let error = stalled
        .await
        .expect("the poll task finished")
        .expect_err("its one attempt lost the cable");
    assert!(matches!(error, DeviceError::Busy(_)), "{error:?}");
    assert_eq!(link.consecutive_misses(), 0);
    assert!(
        watchdog.try_recv().is_err(),
        "an emergency stop must not be able to make the link look dead"
    );
}

#[tokio::test(start_paused = true)]
async fn t_ser_3_an_emergency_stop_takes_both_axes_even_when_one_of_them_fails() {
    // `Emergency stop = Priority(L axis1) + Priority(L axis2)`, and the second is not conditional
    // on the first. A `?` after the RA stop would leave DEC slewing on exactly the fault that
    // made the stop necessary.
    let (port, link, _watchdog) = link(no_retries());
    port.script([Scripted::Refuses(MountError::MotorNotStopped)]);

    let error = link
        .emergency_stop()
        .await
        .expect_err("the mount declined the first stop");
    assert!(matches!(error, DeviceError::Rejected(_)), "{error:?}");

    let frames: Vec<Vec<u8>> = port.writes().into_iter().map(|w| w.bytes).collect();
    assert_eq!(
        frames,
        vec![b":L1\r".to_vec(), b":L2\r".to_vec()],
        "both axes were commanded to stop"
    );
}
