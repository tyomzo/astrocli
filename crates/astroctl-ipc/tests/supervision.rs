//! T-IPC-1, the machinery half: spawn, handshake, health, restart, retry, cancel, framing.
//!
//! These drive real child processes. That is the point — SDD §5.12.4 exists so that the parts
//! most likely to be wrong (a buffered pipe, a worker killed between two writes, a handshake
//! that hangs instead of failing) are exercised from the first milestone against compute too
//! trivial to hide them.
//!
//! Most tests use the fault-injectable double in `testdata/worker_stub.py`, which needs nothing
//! but a bare `python3`. Only the one test that asserts on an actual JPEG needs
//! `workers/requirements.txt` installed, and it says so out loud when it skips.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astroctl_core::bus::EventBus;
use astroctl_core::error::ErrorCode;
use astroctl_core::event::WorkerState;
use astroctl_ipc::protocol::JobKind;
use astroctl_ipc::supervisor::{self, JobFailure};
use serde_json::json;

/// Total disruption budget for a worker death mid-job, from the M1-T13 acceptance criteria.
const DISRUPTION_BUDGET: Duration = Duration::from_secs(10);

fn stub() -> PathBuf {
    support::testdata().join("worker_stub.py")
}

fn compute_worker() -> PathBuf {
    support::shipped_worker("compute_worker.py")
}

/// Returns the interpreter, or `None` after announcing the skip.
fn interpreter(test: &str) -> Option<PathBuf> {
    match support::python3() {
        Some(python) => Some(python),
        None => {
            support::skip(test, "no python3 on PATH");
            None
        }
    }
}

#[tokio::test]
async fn spawn_starts_no_process_until_a_job_needs_one() {
    let Some(python) = interpreter("spawn_starts_no_process_until_a_job_needs_one") else {
        return;
    };
    let bus = EventBus::new();
    let config = support::workers_config(&python, &stub(), 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    // SDD §5.12.3 supervises workers as on-demand children, and astroctl-stack's startup
    // sequence says so explicitly. `None` is "no worker has ever been needed".
    assert_eq!(workers.status().state, None);
    assert_eq!(workers.status().restarts, 0);

    // Nothing should change just because time passed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(workers.status().state, None);
}

#[tokio::test]
async fn the_job_machinery_round_trips_without_the_compute_dependencies() {
    let test = "the_job_machinery_round_trips_without_the_compute_dependencies";
    let Some(python) = interpreter(test) else {
        return;
    };
    let bus = EventBus::new();
    let config = support::workers_config(&python, &stub(), 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    let data = workers
        .submit(JobKind::Preview, json!({"marker": 41}), Vec::new())
        .await
        .expect("the stub worker answers");

    assert_eq!(data["echo"]["marker"], 41);
    let status = workers.status();
    assert_eq!(status.state, Some(WorkerState::Ready));
    assert_eq!(status.restarts, 0);
    assert_eq!(status.jobs_completed, 1);
    assert_eq!(status.jobs_failed, 0);
}

#[tokio::test]
async fn pings_are_answered_while_a_job_is_running() {
    let test = "pings_are_answered_while_a_job_is_running";
    let Some(python) = interpreter(test) else {
        return;
    };

    // The property under test is the one SDD §5.12.3's ping cadence silently depends on. With a
    // 1 s ping the supervisor gives up after three unanswered probes, i.e. at about 4 s. A job
    // that busies the worker for 5 s therefore *cannot* complete unless the worker answered
    // pings while computing — a worker that reads its stdin only between jobs is killed by its
    // own health check first, twice, and the job fails.
    let bus = EventBus::new();
    let config = support::workers_config(&python, &stub(), 1, 60);
    let workers = supervisor::spawn(&config, &bus);

    let started = Instant::now();
    let data = workers
        .submit(JobKind::Preview, json!({"sleep_ms": 5_000}), Vec::new())
        .await
        .expect("a healthy worker must survive its own health check");
    let elapsed = started.elapsed();

    assert_eq!(data["echo"]["sleep_ms"], 5_000);
    assert!(
        elapsed >= Duration::from_secs(4),
        "the job returned in {elapsed:?}; it was supposed to occupy the worker for 5 s"
    );
    assert_eq!(
        workers.status().restarts,
        0,
        "the health check killed a worker that was working"
    );
}

#[tokio::test]
async fn a_protocol_version_mismatch_is_refused_without_a_retry() {
    let test = "a_protocol_version_mismatch_is_refused_without_a_retry";
    let Some(python) = interpreter(test) else {
        return;
    };

    let bus = EventBus::new();
    let alerts = support::AlertLog::attach(&bus);
    let script = support::testdata().join("worker_wrong_version.py");
    let config = support::workers_config(&python, &script, 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    let started = Instant::now();
    let failure = workers
        .submit(JobKind::Preview, json!({}), Vec::new())
        .await
        .expect_err("a version mismatch must not be usable");
    let elapsed = started.elapsed();

    assert!(
        matches!(failure, JobFailure::Unavailable(_)),
        "expected Unavailable, got {failure:?}"
    );
    // SDD §5.12.2 wants both versions in the log and in the operator's face. 99 is the double
    // this repository's PROTO_VERSION + 98.
    let text = failure.to_string();
    assert!(
        text.contains("v99"),
        "the refusal hides the worker's version: {text}"
    );
    assert!(
        text.contains("v1"),
        "the refusal hides the backbone's version: {text}"
    );
    // "no hang": this must fail at the handshake, not by waiting out a timeout.
    assert!(
        elapsed < supervisor::HANDSHAKE_TIMEOUT,
        "the refusal took {elapsed:?}; it should be immediate"
    );

    support::eventually("the mismatch alert", || {
        alerts.saw(supervisor::ALERT_PROTO_MISMATCH)
    })
    .await;
    support::eventually("the failed state", || {
        workers.status().state == Some(WorkerState::Failed)
    })
    .await;
    assert_eq!(
        workers.status().restarts,
        0,
        "a deterministic failure was retried"
    );

    // And the next submission is refused with the same reason rather than left hanging or
    // answered with "the supervisor has stopped", which would tell the operator nothing.
    let again = workers
        .submit(JobKind::Preview, json!({}), Vec::new())
        .await
        .expect_err("still refused");
    assert!(again.to_string().contains("v99"), "{again}");
}

#[tokio::test]
async fn a_worker_killed_mid_job_is_restarted_and_the_job_retried_once() {
    let test = "a_worker_killed_mid_job_is_restarted_and_the_job_retried_once";
    let Some(python) = interpreter(test) else {
        return;
    };

    let temp = support::TempDir::new("retry");
    let marker = temp.join("attempts");
    let bus = EventBus::new();
    let alerts = support::AlertLog::attach(&bus);
    let config = support::workers_config(&python, &stub(), 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    let started = Instant::now();
    let data = workers
        .submit(
            JobKind::Preview,
            json!({"crash_marker": marker, "crash_attempts": 1, "sleep_ms": 100}),
            Vec::new(),
        )
        .await
        .expect("the retry on a fresh worker must succeed");
    let elapsed = started.elapsed();

    assert_eq!(data["echo"]["crash_attempts"], 1);
    assert!(
        elapsed < DISRUPTION_BUDGET,
        "total disruption was {elapsed:?}; the acceptance budget is {DISRUPTION_BUDGET:?}"
    );
    assert_eq!(
        std::fs::metadata(&marker).expect("the marker exists").len(),
        2,
        "the job should have been attempted exactly twice"
    );
    support::eventually("the restart to be counted", || {
        workers.status().restarts == 1
    })
    .await;
    support::eventually("the restart alert", || {
        alerts.saw(supervisor::ALERT_RESTARTED)
    })
    .await;
    assert_eq!(workers.status().jobs_completed, 1);
}

#[tokio::test]
async fn a_job_that_always_kills_the_worker_fails_with_an_alert_and_no_restart_loop() {
    let test = "a_job_that_always_kills_the_worker_fails_with_an_alert_and_no_restart_loop";
    let Some(python) = interpreter(test) else {
        return;
    };

    let temp = support::TempDir::new("loop");
    let marker = temp.join("attempts");
    let bus = EventBus::new();
    let alerts = support::AlertLog::attach(&bus);
    let config = support::workers_config(&python, &stub(), 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    let failure = workers
        .submit(
            JobKind::Preview,
            json!({"crash_marker": marker, "crash_attempts": 99}),
            Vec::new(),
        )
        .await
        .expect_err("a job that always kills its worker must fail");

    match &failure {
        JobFailure::Crashed { attempts, .. } => assert_eq!(*attempts, 2),
        other => panic!("expected Crashed, got {other:?}"),
    }
    support::eventually("the job-failed alert", || {
        alerts.saw(supervisor::ALERT_JOB_FAILED)
    })
    .await;

    // The bound is what matters: two worker processes died, and because workers start on demand
    // nothing replaces the second until some other job arrives. A restart loop would keep going.
    support::eventually("both deaths to be counted", || {
        workers.status().restarts == 2
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        workers.status().restarts,
        2,
        "the worker is being restarted in a loop"
    );
    assert_eq!(
        std::fs::metadata(&marker).expect("the marker exists").len(),
        2,
        "the job was attempted more than twice"
    );
}

#[tokio::test]
async fn a_worker_that_stops_answering_pings_is_killed_and_replaced() {
    let test = "a_worker_that_stops_answering_pings_is_killed_and_replaced";
    let Some(python) = interpreter(test) else {
        return;
    };

    // A 60 s job timeout with a 1 s ping: if this test finishes at all, it was the health check
    // that noticed, not the job timeout. That distinction is the whole reason pings exist — a
    // crash closes the pipe and announces itself, a wedged worker announces nothing.
    let bus = EventBus::new();
    let script = support::testdata().join("worker_deaf.py");
    let config = support::workers_config(&python, &script, 1, 60);
    let workers = supervisor::spawn(&config, &bus);

    let started = Instant::now();
    let failure = workers
        .submit(JobKind::Preview, json!({}), Vec::new())
        .await
        .expect_err("a worker that answers nothing cannot complete a job");
    let elapsed = started.elapsed();

    match &failure {
        JobFailure::Crashed { attempts, reason } => {
            assert_eq!(*attempts, 2);
            assert!(
                reason.contains("consecutive pings"),
                "the job failed for the wrong reason: {reason}"
            );
        }
        other => panic!("expected Crashed, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(30),
        "the wedge took {elapsed:?} to notice"
    );
    support::eventually("the replacement worker to be counted", || {
        workers.status().restarts >= 1
    })
    .await;
}

#[tokio::test]
async fn a_job_that_outlives_its_timeout_is_cancelled_and_the_worker_survives() {
    let test = "a_job_that_outlives_its_timeout_is_cancelled_and_the_worker_survives";
    let Some(python) = interpreter(test) else {
        return;
    };

    let bus = EventBus::new();
    let config = support::workers_config(&python, &stub(), 1, 2);
    let workers = supervisor::spawn(&config, &bus);

    let failure = workers
        .submit(JobKind::Preview, json!({"sleep_ms": 20_000}), Vec::new())
        .await
        .expect_err("a job past workers.job_timeout_seconds must not succeed");
    assert!(
        matches!(failure, JobFailure::TimedOut(_)),
        "expected TimedOut, got {failure:?}"
    );

    // The worker honoured `cancel`, so killing it was never necessary — and the next job goes to
    // the same process. A supervisor that killed on every slow job would turn one slow frame
    // into a restart and a cold start.
    let data = workers
        .submit(JobKind::Preview, json!({"marker": 7}), Vec::new())
        .await
        .expect("the worker survived being cancelled");
    assert_eq!(data["echo"]["marker"], 7);
    assert_eq!(workers.status().restarts, 0);
}

#[tokio::test]
async fn a_job_that_ignores_cancel_costs_the_worker_its_life() {
    let test = "a_job_that_ignores_cancel_costs_the_worker_its_life";
    let Some(python) = interpreter(test) else {
        return;
    };

    let bus = EventBus::new();
    // A 5 s ping so the pings play no part: the only thing that can end this is the two-stage
    // job timeout — cancel, then kill.
    let config = support::workers_config(&python, &stub(), 5, 1);
    let workers = supervisor::spawn(&config, &bus);

    let failure = workers
        .submit(
            JobKind::Preview,
            json!({"sleep_ms": 30_000, "ignore_cancel": true}),
            Vec::new(),
        )
        .await
        .expect_err("a job that ignores cancel must still be stopped");
    assert!(
        matches!(failure, JobFailure::TimedOut(_)),
        "expected TimedOut, got {failure:?}"
    );
    support::eventually("the kill to be counted as a restart", || {
        workers.status().restarts == 1
    })
    .await;
}

#[tokio::test]
async fn a_missing_interpreter_is_refused_once_and_never_retried() {
    let bus = EventBus::new();
    let alerts = support::AlertLog::attach(&bus);
    let absent = Path::new("/nonexistent/bin/python3");
    let config = support::workers_config(absent, &stub(), 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    let failure = workers
        .submit(JobKind::Preview, json!({}), Vec::new())
        .await
        .expect_err("there is no interpreter to run");

    assert!(
        matches!(failure, JobFailure::Unavailable(_)),
        "expected Unavailable, got {failure:?}"
    );
    let text = failure.to_string();
    assert!(
        text.contains("/nonexistent/bin/python3"),
        "the refusal does not name the interpreter: {text}"
    );

    support::eventually("the unavailable alert", || {
        alerts.saw(supervisor::ALERT_UNAVAILABLE)
    })
    .await;
    support::eventually("the failed state", || {
        workers.status().state == Some(WorkerState::Failed)
    })
    .await;
    // A path that does not exist will not start existing on a backoff. Retrying it forever
    // produces one log line a minute and no progress.
    assert_eq!(workers.status().restarts, 0);
}

#[tokio::test]
async fn a_worker_exception_becomes_a_structured_error_result() {
    let test = "a_worker_exception_becomes_a_structured_error_result";
    let Some(python) = interpreter(test) else {
        return;
    };

    // The real shipped worker, not the double: a missing frame is checked before the compute
    // dependencies are imported, so this covers `compute_worker.py`'s own error path on a bare
    // interpreter.
    let bus = EventBus::new();
    let config = support::workers_config(&python, &compute_worker(), 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    let failure = workers
        .submit(
            JobKind::Preview,
            json!({}),
            vec![PathBuf::from("/nonexistent/frames/light_000001.fits")],
        )
        .await
        .expect_err("a missing frame is a failure");

    match &failure {
        JobFailure::Worker(error) => {
            assert_eq!(error.code, ErrorCode::NotFound);
            assert!(
                error.message.contains("light_000001.fits"),
                "the error does not name the frame: {error}"
            );
        }
        other => panic!("expected a worker-diagnosed failure, got {other:?}"),
    }
    assert_eq!(failure.code(), ErrorCode::NotFound);

    // Reporting a failure is not crashing: the same worker takes the next job.
    support::eventually("the worker to go idle", || {
        workers.status().state == Some(WorkerState::Ready)
    })
    .await;
    assert_eq!(workers.status().restarts, 0);
    assert_eq!(workers.status().jobs_failed, 1);
}

#[tokio::test]
async fn a_stray_write_to_stdout_does_not_corrupt_the_frame_stream() {
    let test = "a_stray_write_to_stdout_does_not_corrupt_the_frame_stream";
    let Some(python) = interpreter(test) else {
        return;
    };

    // `Channel.open()` takes fd 1 for the protocol and sends everything else to stderr. Without
    // that, a Phase 2b `print` or a library banner lands between two frames and desynchronises
    // the decoder — and the symptom appears hours later as previews quietly stopping.
    let bus = EventBus::new();
    let config = support::workers_config(&python, &stub(), 5, 30);
    let workers = supervisor::spawn(&config, &bus);

    let data = workers
        .submit(
            JobKind::Preview,
            json!({"noise": "surprise banner", "marker": 5}),
            Vec::new(),
        )
        .await
        .expect("framing must survive a stray print");

    assert_eq!(data["echo"]["marker"], 5);
    assert_eq!(workers.status().restarts, 0);
}

#[tokio::test]
async fn a_preview_job_writes_a_jpeg_beside_the_frame() {
    let test = "a_preview_job_writes_a_jpeg_beside_the_frame";
    let Some(python) = interpreter(test) else {
        return;
    };
    if !support::python_can_import(&python, &["numpy", "PIL"]) {
        support::skip(
            test,
            "numpy and Pillow are not installed in this interpreter — \
             `pip install -r workers/requirements.txt` to cover the compute path",
        );
        return;
    }

    let temp = support::TempDir::new("preview");
    let frame = temp.join("light_000001.fits");
    support::write_fits(&frame, 96, 64);

    let bus = EventBus::new();
    let config = support::workers_config(&python, &compute_worker(), 5, 60);
    let workers = supervisor::spawn(&config, &bus);

    let data = workers
        .submit(JobKind::Preview, json!({}), vec![frame.clone()])
        .await
        .expect("the preview job succeeds");

    let expected = temp.join("light_000001.jpg");
    assert_eq!(
        data["preview_path"].as_str(),
        expected.to_str(),
        "the worker reported the wrong path"
    );
    assert_eq!(data["width"], 96);
    assert_eq!(data["height"], 64);

    let bytes = std::fs::read(&expected).expect("the preview exists on disk");
    // SOI + the start of the APP0/JFIF marker: a real JPEG, not a renamed array dump.
    assert_eq!(
        &bytes[..3],
        &[0xFF, 0xD8, 0xFF],
        "the preview is not a JPEG ({} bytes)",
        bytes.len()
    );
    assert!(
        !temp.join(".tmp_light_000001.jpg").exists(),
        "the atomic-rename scratch file was left behind"
    );
    assert_eq!(workers.status().jobs_completed, 1);
}
