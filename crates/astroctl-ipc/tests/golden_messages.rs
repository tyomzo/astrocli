//! T-IPC-1, the drift half: one fixture, asserted against **both** implementations.
//!
//! `testdata/golden-messages.json` is the only place the v1 wire format is written down as data.
//! These tests hold `protocol.rs` to it, hold `workers/astroctl_ipc.py` to it by running the
//! module's own conformance entry point, and then hold the two to each other. Renaming a field in
//! one language and not the other is the failure this exists to make loud — the alternative is
//! finding out from a stacking server that has stopped producing previews.

mod support;

use std::process::Command;

use astroctl_core::error::ErrorCode;
use astroctl_ipc::protocol::{FromWorker, JobKind, ToWorker, MAX_LINE_BYTES, PROTO_VERSION};
use serde_json::Value;

fn fixture() -> Value {
    let path = support::testdata().join("golden-messages.json");
    let text = std::fs::read_to_string(&path).expect("the golden fixture is readable");
    serde_json::from_str(&text).expect("the golden fixture is JSON")
}

fn messages(fixture: &Value, key: &str) -> Vec<Value> {
    fixture[key]
        .as_array()
        .unwrap_or_else(|| panic!("the fixture has no `{key}` array"))
        .clone()
}

#[test]
fn the_fixture_pins_the_protocol_constants() {
    let fixture = fixture();
    assert_eq!(fixture["proto_version"], Value::from(PROTO_VERSION));
    assert_eq!(fixture["max_line_bytes"], Value::from(MAX_LINE_BYTES));
}

#[test]
fn the_fixture_error_codes_are_exactly_the_closed_enum() {
    // `WorkerError.code` is `ErrorCode`, so the Python mirror carries a copy of that enum's wire
    // spellings. This is what stops the copy from going stale when a code is added in M0-T02's
    // module and nowhere else.
    let mut expected: Vec<String> = ErrorCode::ALL
        .iter()
        .map(|code| code.as_str().to_owned())
        .collect();
    expected.sort();

    let listed: Vec<String> = messages(&fixture(), "error_codes")
        .iter()
        .map(|code| code.as_str().unwrap_or_default().to_owned())
        .collect();

    assert_eq!(
        listed, expected,
        "the fixture's error codes have drifted from ErrorCode"
    );
}

#[test]
fn the_fixture_job_kinds_are_exactly_the_rust_enum() {
    // One kind in this increment (SDD §5.12.4). Adding `Register` in Phase 2b must fail here
    // until the fixture and the Python mirror agree that it exists.
    let all = [JobKind::Preview];
    let expected: Vec<Value> = all
        .iter()
        .map(|kind| serde_json::to_value(kind).expect("a job kind serializes"))
        .collect();
    assert_eq!(messages(&fixture(), "job_kinds"), expected);
}

#[test]
fn every_fixture_message_round_trips_through_the_rust_types() {
    let fixture = fixture();

    for message in messages(&fixture, "to_worker") {
        let frame = serde_json::to_string(&message).expect("the fixture message serializes");
        let decoded = ToWorker::decode(&frame)
            .unwrap_or_else(|error| panic!("{message} did not decode: {error}"));
        let re_encoded = decoded.encode().expect("re-encodes");
        let back: Value = serde_json::from_str(&re_encoded).expect("re-encoded frame is JSON");
        assert_eq!(
            back, message,
            "ToWorker changed the message on the way through"
        );
        assert_eq!(re_encoded.matches('\n').count(), 1);
    }

    for message in messages(&fixture, "from_worker") {
        let frame = serde_json::to_string(&message).expect("the fixture message serializes");
        let decoded = FromWorker::decode(&frame)
            .unwrap_or_else(|error| panic!("{message} did not decode: {error}"));
        let re_encoded = decoded.encode().expect("re-encodes");
        let back: Value = serde_json::from_str(&re_encoded).expect("re-encoded frame is JSON");
        assert_eq!(
            back, message,
            "FromWorker changed the message on the way through"
        );
        assert_eq!(re_encoded.matches('\n').count(), 1);
    }
}

#[test]
fn every_rejected_frame_is_refused_by_the_rust_decoder() {
    for frame in messages(&fixture(), "rejected_from_worker") {
        let frame = frame.as_str().expect("rejected frames are strings");
        let outcome = FromWorker::decode(frame);
        assert!(
            outcome.is_err(),
            "`{frame}` decoded successfully; the fixture says it must not"
        );
    }
}

#[test]
fn the_python_mirror_agrees_with_the_rust_types() {
    let Some(interpreter) = support::python3() else {
        support::skip(
            "the_python_mirror_agrees_with_the_rust_types",
            "no python3 on PATH — the Python half of T-IPC-1 could not be checked",
        );
        return;
    };

    let module = support::shipped_worker("astroctl_ipc.py");
    let fixture_path = support::testdata().join("golden-messages.json");
    let output = Command::new(&interpreter)
        .arg(&module)
        .arg(&fixture_path)
        .output()
        .expect("running the Python mirror's conformance check");

    assert!(
        output.status.success(),
        "`python3 {} {}` failed: {}",
        module.display(),
        fixture_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value =
        serde_json::from_slice(&output.stdout).expect("the conformance report is JSON");
    let fixture = fixture();

    // The two implementations agree on the constants...
    assert_eq!(report["proto_version"], Value::from(PROTO_VERSION));
    assert_eq!(report["max_line_bytes"], Value::from(MAX_LINE_BYTES));

    // ...on the closed error vocabulary...
    let mut expected_codes: Vec<String> = ErrorCode::ALL
        .iter()
        .map(|code| code.as_str().to_owned())
        .collect();
    expected_codes.sort();
    let python_codes: Vec<String> = report["error_codes"]
        .as_array()
        .expect("the report lists error codes")
        .iter()
        .map(|code| code.as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        python_codes, expected_codes,
        "workers/astroctl_ipc.py's ERROR_CODES has drifted from astroctl-core's ErrorCode"
    );

    // ...on every message, field for field...
    assert_eq!(report["to_worker"], fixture["to_worker"]);
    assert_eq!(report["from_worker"], fixture["from_worker"]);
    assert_eq!(report["job_kinds"], fixture["job_kinds"]);

    // ...and on what a correct worker never sends.
    assert_eq!(
        report["rejected_from_worker"], fixture["rejected_from_worker"],
        "the two implementations disagree about which frames are invalid"
    );
}
