"""A fault-injectable worker double for the astroctl-ipc supervision tests.

Not shipped, and not a worker: `workers/compute_worker.py` is the real one. This exists so the
supervision tests can ask for behaviour a correct worker never exhibits — dying mid-job,
ignoring `cancel`, writing junk to stdout — and so that the machinery T-IPC-1 is actually about
(spawn, handshake, ping, restart, retry) is testable on a bare `python3`, with no numpy and no
Pillow anywhere near it.

It speaks the protocol through `workers/astroctl_ipc.py`, so every test that uses it is also one
more assertion that the shipped mirror module works.

Behaviour comes from each job's `params` and never from the environment: `cargo test` runs tests
as threads inside one process, so an env-var switch would leak between tests running at once.

Recognised params:
  sleep_ms        stay busy this long, in slices, checking for `cancel` between them
  ignore_cancel   keep going anyway — the path that gets a worker killed
  crash_marker    file used to count attempts at a job across worker restarts
  crash_attempts  SIGKILL self on the first N attempts (needs crash_marker)
  fail            return a structured error result carrying this message
  fail_code       the SDD §4.2 code to fail with (default INTERNAL)
  noise           write this to stdout before answering, the way a stray `print` would
"""

from __future__ import annotations

import os
import signal
import sys
import threading
import time

# This file is deliberately outside `workers/`, so the shipped protocol module is not on the
# path CPython derives from a script's own directory. testdata → astroctl-ipc → crates → root.
_HERE = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.normpath(os.path.join(_HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(_ROOT, "workers"))

# Imported after the bootstrap above, which is what puts it on the path.
import astroctl_ipc as ipc

_SLICE_MS = 50


class StubWorker:
    def __init__(self, channel):
        self._channel = channel
        self._lock = threading.Lock()
        self._cancelled = set()

    def run(self):
        for message in self._channel.frames():
            kind = message["type"]
            if kind == "ping":
                self._channel.send(ipc.pong(message["nonce"]))
            elif kind == "hello":
                self._channel.send(ipc.hello())
            elif kind == "job":
                # Same shape as the real worker: compute on its own thread so the main thread
                # keeps answering pings, which is the property the supervisor depends on.
                threading.Thread(
                    target=self._execute, args=(message,), daemon=True
                ).start()
            elif kind == "cancel":
                with self._lock:
                    self._cancelled.add(message["id"])
            elif kind == "shutdown":
                return 0
        return 0

    def _execute(self, message):
        job_id = message["id"]
        params = message["params"] if isinstance(message["params"], dict) else {}

        self._maybe_crash(params)

        noise = params.get("noise")
        if noise:
            # Exactly what a Phase 2b `print` or a library banner would do. It must land in the
            # supervisor's log, not between two frames.
            print(noise)
            sys.stdout.write(f"{noise} again\n")

        if not self._sleep(job_id, params):
            self._channel.send(
                ipc.result_error(job_id, "INTERNAL", f"job {job_id} cancelled")
            )
            return

        failure = params.get("fail")
        if failure:
            self._channel.send(
                ipc.result_error(job_id, params.get("fail_code", "INTERNAL"), failure)
            )
            return

        self._channel.send(ipc.result_ok(job_id, {"echo": params}))

    def _maybe_crash(self, params):
        marker = params.get("crash_marker")
        wanted = int(params.get("crash_attempts", 0))
        if not marker or wanted <= 0:
            return
        # The marker's size counts attempts, and it survives the worker it kills — which is how
        # a "crash once, then succeed" test outlives the process it is testing.
        with open(marker, "a", encoding="utf-8") as handle:
            handle.write("x")
        if os.path.getsize(marker) <= wanted:
            self._channel.send(ipc.log("warn", "crashing on purpose"))
            os.kill(os.getpid(), signal.SIGKILL)

    def _sleep(self, job_id, params):
        """Busy for `sleep_ms`. Returns False if the job was cancelled."""
        remaining = int(params.get("sleep_ms", 0))
        ignore_cancel = bool(params.get("ignore_cancel", False))
        while remaining > 0:
            time.sleep(min(_SLICE_MS, remaining) / 1000.0)
            remaining -= _SLICE_MS
            if ignore_cancel:
                continue
            with self._lock:
                if job_id in self._cancelled:
                    return False
        return True


def main():
    channel = ipc.Channel.open()
    try:
        return StubWorker(channel).run()
    finally:
        channel.close()


if __name__ == "__main__":
    raise SystemExit(main())
