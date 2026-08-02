"""The worker end of the astroctl-ipc protocol (SDD §5.12.1, ADR-13).

This is the Python mirror of `crates/astroctl-ipc/src/protocol.rs`: one JSON object per line
on stdin/stdout, UTF-8, newline-delimited. Every worker in this directory speaks the protocol
through here rather than assembling dicts of its own, so there is one place where a field name
can be wrong.

Two definitions of one wire format drift silently — that is the whole reason ADR-13 asks for a
version handshake. `testdata/golden-messages.json` in the Rust crate is the shared fixture that
makes the drift loud instead: run

    python3 workers/astroctl_ipc.py crates/astroctl-ipc/testdata/golden-messages.json

and this module round-trips every message in it and prints what it made of them; the Rust test
does the same with its enums and compares the two, byte for byte (T-IPC-1). A field renamed on
one side fails a test rather than a night of stacking.

Dependencies: the standard library only. This module has to import cleanly on a bare `python3`
so that the handshake, the health pings and the version check all work on a machine where
`workers/requirements.txt` has not been installed — the operator learns that numpy is missing
from a structured error on the first job, not from a worker that will not start.
"""

from __future__ import annotations

import json
import os
import sys
import threading

# Must equal `PROTO_VERSION` in crates/astroctl-ipc/src/protocol.rs. Equality is checked at the
# handshake, not compatibility: SDD §5.12.2 makes a mismatch a refusal with both numbers logged.
PROTO_VERSION = 1

# Must equal `MAX_LINE_BYTES` in the Rust crate.
MAX_LINE_BYTES = 1024 * 1024

# The closed error vocabulary of SDD §4.2, mirroring `ErrorCode` in astroctl-core. A worker
# failure reaches the operator through the same codes as everything else, so this set is what a
# `result` may carry — see `result_error`.
ERROR_CODES = frozenset(
    {
        "NOT_CONNECTED",
        "UNSUPPORTED",
        "BUSY",
        # A motion stopped by an e-stop or a safety limit (M1-T03). Nothing a worker raises, but
        # the vocabulary is shared and the conformance test compares the whole set.
        "ABORTED",
        # REL-02's watchdog verdict: the mount stopped answering for `heartbeat_misses`
        # consecutive polls (M1-T17). Alert-only and nothing a worker raises; the set is shared
        # and compared whole.
        "MOUNT_LINK_LOST",
        "MOUNT_TIMEOUT",
        "CAMERA_TIMEOUT",
        "DEVICE_TIMEOUT",
        "DEVICE_TRANSPORT",
        "DEVICE_PROTOCOL",
        # The other *node* is not answering — the /stack/* proxy and the transfer agent (M1-T14).
        # Nothing a worker raises either; the set is shared and compared whole.
        "NODE_UNREACHABLE",
        "DEVICE_REJECTED",
        "VALIDATION",
        "COMMAND_STALE",
        "CHECKSUM_MISMATCH",
        "LIMIT_ALTITUDE",
        "LIMIT_MERIDIAN",
        "LIMIT_TRAVEL",
        "SLEW_TTL_EXPIRED",
        "AUTH",
        "FRAME_ID_CONFLICT",
        "DISK_FULL",
        "NOT_FOUND",
        "CANCELLED",
        "WORKER_UNAVAILABLE",
        "WORKER_CRASHED",
        "WORKER_TIMEOUT",
        "INTERNAL",
    }
)

# `JobKind` in the Rust crate. Registration, accumulation and the post-chain arrive in Phase 2b.
JOB_KINDS = frozenset({"preview"})


class ProtocolError(Exception):
    """A frame no correct peer would have sent."""


# ---------------------------------------------------------------------------------------------
# Framing
# ---------------------------------------------------------------------------------------------


def encode(message):
    """Render one message as a protocol frame, trailing newline included."""
    # `allow_nan=False` is load-bearing. Python's json module emits bare `NaN` and `Infinity`
    # for non-finite floats, which is not JSON and which serde rejects — so a percentile over a
    # blank calibration frame would produce a frame the backbone cannot read, at the exact
    # moment it is being told about a problem. Fail here, where the traceback names the field.
    frame = json.dumps(message, separators=(",", ":"), allow_nan=False)
    size = len(frame.encode("utf-8")) + 1
    if size > MAX_LINE_BYTES:
        raise ProtocolError(f"frame is {size} bytes; the limit is {MAX_LINE_BYTES}")
    return frame + "\n"


def decode_to_worker(frame):
    """Parse and validate one backbone → worker frame."""
    return _validate(_parse(frame), _TO_WORKER)


def decode_from_worker(frame):
    """Parse and validate one worker → backbone frame.

    A worker never receives these; it exists so this module can check itself against the
    golden fixture in both directions.
    """
    return _validate(_parse(frame), _FROM_WORKER)


def _parse(frame):
    if len(frame.encode("utf-8")) > MAX_LINE_BYTES:
        raise ProtocolError(f"frame exceeds {MAX_LINE_BYTES} bytes")
    try:
        message = json.loads(frame)
    except ValueError as error:
        raise ProtocolError(f"not JSON: {error}") from error
    if not isinstance(message, dict):
        raise ProtocolError(f"frame is a {type(message).__name__}, not an object")
    return message


def _validate(message, schema):
    kind = message.get("type")
    if kind not in schema:
        raise ProtocolError(f"unknown message type {kind!r}")
    for field, expected in schema[kind].items():
        if field not in message:
            raise ProtocolError(f"{kind}: missing field {field!r}")
        _check_type(kind, field, message[field], expected)
    _check_invariants(message)
    return message


def _check_type(kind, field, value, expected):
    if expected is None:
        return
    # `isinstance(True, int)` is True in Python, so an unguarded integer check would accept
    # `{"nonce": true}` and hand the backbone a frame it will reject as a type error.
    if expected is int and isinstance(value, bool):
        raise ProtocolError(f"{kind}.{field} is a bool, not an integer")
    if not isinstance(value, expected):
        raise ProtocolError(
            f"{kind}.{field} is a {type(value).__name__}, not {expected.__name__}"
        )


def _check_invariants(message):
    """Reject the shapes SDD §5.12.1's message table allows but no correct peer produces.

    `result {ok, data, error}` can express "succeeded, and here is the failure" and "failed,
    with nothing to say why". The first hands the operator a preview that does not exist and
    the second an empty error, and both are far cheaper to catch at the frame boundary than
    three layers up.
    """
    kind = message["type"]
    if kind == "result":
        if message["ok"] != (message.get("error") is None):
            raise ProtocolError(
                f"result {message['id']}: ok={message['ok']} disagrees with the error field"
            )
        error = message.get("error")
        if error is not None:
            if not isinstance(error, dict):
                raise ProtocolError("result.error is not an object")
            code = error.get("code")
            if code not in ERROR_CODES:
                raise ProtocolError(
                    f"result.error.code {code!r} is not an SDD §4.2 code"
                )
            if not isinstance(error.get("message"), str):
                raise ProtocolError("result.error.message is not a string")
    elif kind == "progress":
        if not 0 <= message["pct"] <= 100:
            raise ProtocolError(f"progress {message['id']}: pct={message['pct']}")
    elif kind == "job":
        if message["kind"] not in JOB_KINDS:
            raise ProtocolError(
                f"job {message['id']}: unknown kind {message['kind']!r}"
            )
        if not all(isinstance(path, str) for path in message["paths"]):
            raise ProtocolError(f"job {message['id']}: paths must be strings")
    elif kind in ("hello", "ping", "pong"):
        field = "proto_version" if kind == "hello" else "nonce"
        if message[field] < 0:
            raise ProtocolError(f"{kind}.{field} is negative")


# Field → expected Python type; `None` means "any JSON value". Optional fields are validated by
# `_check_invariants` instead, because their absence is meaningful.
_TO_WORKER = {
    "hello": {"proto_version": int},
    "job": {"id": int, "kind": str, "params": None, "paths": list},
    "cancel": {"id": int},
    "ping": {"nonce": int},
    "shutdown": {},
}

_FROM_WORKER = {
    "hello": {"proto_version": int, "capabilities": dict},
    "progress": {"id": int, "pct": int},
    "result": {"id": int, "ok": bool},
    "pong": {"nonce": int},
    "log": {"level": str, "message": str},
}


# ---------------------------------------------------------------------------------------------
# Messages a worker sends
# ---------------------------------------------------------------------------------------------


def capabilities(gpu=False, vram_mb=None, libs=None):
    """Build the `capabilities` object for a `hello`.

    Reported, never configured: whether CuPy actually found a device is a fact only this
    process knows, and CMP-06's CPU fallback is a decision made here rather than in the YAML.
    """
    caps = {"gpu": bool(gpu)}
    if vram_mb is not None:
        caps["vram_mb"] = int(vram_mb)
    caps["libs"] = dict(libs or {})
    return caps


def hello(caps=None):
    """The worker's half of the handshake."""
    return {
        "type": "hello",
        "proto_version": PROTO_VERSION,
        "capabilities": caps if caps is not None else capabilities(),
    }


def progress(job_id, pct):
    """Advisory progress on an in-flight job."""
    return {"type": "progress", "id": int(job_id), "pct": max(0, min(100, int(pct)))}


def result_ok(job_id, data=None):
    """A job that succeeded. `data` is the kind-specific payload."""
    message = {"type": "result", "id": int(job_id), "ok": True}
    if data is not None:
        message["data"] = data
    return message


def result_error(job_id, code, message):
    """A job that failed, in the closed error vocabulary of SDD §4.2."""
    if code not in ERROR_CODES:
        raise ProtocolError(f"{code!r} is not an SDD §4.2 error code")
    return {
        "type": "result",
        "id": int(job_id),
        "ok": False,
        "error": {"code": code, "message": str(message)},
    }


def pong(nonce):
    """Answer to a ping."""
    return {"type": "pong", "nonce": int(nonce)}


def log(level, message):
    """A line for the backbone's tracing output, with a severity it will honour."""
    return {"type": "log", "level": str(level), "message": str(message)}


# ---------------------------------------------------------------------------------------------
# Transport
# ---------------------------------------------------------------------------------------------


class Channel:
    """A worker's stdio protocol channel, safe to write to from more than one thread.

    `Channel.open()` takes file descriptor 1 away from the rest of the process. That is not
    tidiness: a stray `print`, a library banner, or a progress bar written to stdout would land
    *between* two frames and desynchronise the backbone's decoder. The symptom is the stacking
    server quietly no longer seeing results, hours later, with nothing in the log to explain it.
    After `open()`, everything Python calls stdout goes to stderr — where the supervisor
    captures it into tracing with a `worker` field — and only frames reach fd 1.

    The lock matters for the same reason. A worker answers pings on its main thread while a job
    runs on another (see `compute_worker.py`), so two threads write frames, and two interleaved
    `write` calls make one unparseable line out of two valid messages.
    """

    def __init__(self, reader, writer):
        self._reader = reader
        self._writer = writer
        self._lock = threading.Lock()

    @classmethod
    def open(cls):
        """Claim fd 1 for the protocol and send everything else to stderr."""
        sys.stdout.flush()
        protocol_fd = os.dup(1)
        os.dup2(2, 1)
        sys.stdout = sys.stderr
        writer = os.fdopen(protocol_fd, "w", encoding="utf-8", newline="\n")
        return cls(sys.stdin, writer)

    def send(self, message):
        """Write one frame. Flushed immediately — a buffered result is an unanswered job."""
        frame = encode(message)
        with self._lock:
            self._writer.write(frame)
            self._writer.flush()

    def frames(self):
        """Yield decoded backbone → worker messages until stdin closes.

        An unreadable frame is reported and skipped rather than fatal: the backbone's job
        timeout is the backstop if what was lost mattered, and a worker that exits on one bad
        line turns a hiccup into a restart.
        """
        while True:
            line = self._reader.readline()
            if not line:
                return
            line = line.strip()
            if not line:
                continue
            try:
                message = decode_to_worker(line)
            except ProtocolError as error:
                self.send(log("error", f"discarding an unreadable frame: {error}"))
                continue
            yield message

    def close(self):
        with self._lock:
            try:
                self._writer.close()
            except OSError:
                pass


# ---------------------------------------------------------------------------------------------
# Conformance self-check (T-IPC-1)
# ---------------------------------------------------------------------------------------------


def _round_trip(messages, decoder):
    """Encode and decode every message, proving this module agrees with itself on the wire."""
    decoded = []
    for message in messages:
        frame = encode(message)
        if frame.count("\n") != 1 or not frame.endswith("\n"):
            raise ProtocolError(f"{message['type']} did not encode to exactly one line")
        back = decoder(frame)
        if back != message:
            raise ProtocolError(
                f"{message['type']} did not survive a round trip: {back}"
            )
        decoded.append(back)
    return decoded


def _self_check(fixture_path):
    with open(fixture_path, encoding="utf-8") as handle:
        fixture = json.load(handle)

    rejected = []
    for frame in fixture["rejected_from_worker"]:
        try:
            decode_from_worker(frame)
        except ProtocolError:
            rejected.append(frame)

    return {
        "proto_version": PROTO_VERSION,
        "max_line_bytes": MAX_LINE_BYTES,
        "error_codes": sorted(ERROR_CODES),
        "job_kinds": sorted(JOB_KINDS),
        "to_worker": _round_trip(fixture["to_worker"], decode_to_worker),
        "from_worker": _round_trip(fixture["from_worker"], decode_from_worker),
        "rejected_from_worker": rejected,
    }


def main(argv):
    if len(argv) != 2:
        sys.stderr.write(f"usage: {argv[0]} <golden-messages.json>\n")
        return 2
    try:
        report = _self_check(argv[1])
    except (OSError, ValueError, KeyError, ProtocolError) as error:
        sys.stderr.write(f"astroctl_ipc: conformance check failed: {error}\n")
        return 1
    sys.stdout.write(json.dumps(report, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
