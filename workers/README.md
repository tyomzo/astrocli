# Python workers (stacking server only)

Supervised child processes that do the heavy compute. They are the *only* Python in AstroCtl:
ARC-01 confines it to here, and everything else — both binaries, every crate — is Rust. A worker
never opens a socket, never reads the backbone's state and never touches the network. It receives
work on stdin, answers on stdout, and reads and writes frames by filesystem path (ADD §5.6
rule 4, ADR-13).

The field node is packaged without this directory at all (ADD §5.6 rule 6).

| File | What it is |
|------|------------|
| `astroctl_ipc.py` | the protocol: framing, message constructors, validation, the stdio channel. Standard library only. |
| `compute_worker.py` | the compute worker. M1 implements `preview` and nothing else (SDD §5.12.4). |
| `ml_worker.py` | Phase 3. Not present yet. |
| `requirements.txt` | what `compute_worker.py` imports, and why each bound is where it is. |

## Setting up the environment

The supervisor spawns whatever `workers.python_interpreter` names in
`stacking-server.yaml`, and that key must be an **absolute path** — the config loader refuses a
relative one, because which Python runs the workers is the last thing that should depend on the
directory systemd happened to start the service in.

```sh
python3 -m venv /data/astro/venv
/data/astro/venv/bin/pip install -r workers/requirements.txt
```

and in `stacking-server.yaml`:

```yaml
workers:
  python_interpreter: /data/astro/venv/bin/python   # absolute; created above
  compute_worker: workers/compute_worker.py         # relative resolves against the working dir
  ml_worker: workers/ml_worker.py
  health_ping_seconds: 5      # missed × 3 → kill and restart
  restart_backoff_seconds: 2  # capped exponential, 60 s ceiling
  job_timeout_seconds: 300    # must exceed 3 × health_ping_seconds
```

`workers/` must be present next to wherever the service runs, because `compute_worker.py`
imports `astroctl_ipc` from its own directory.

### Without the dependencies installed

Deliberately survivable. `numpy` and `Pillow` are imported inside the preview job, not at module
level, so a worker on a bare `python3` still:

* completes the handshake and reports which libraries it *does* have,
* answers health pings,
* refuses a protocol version mismatch with both versions named,

and fails the first `preview` job with a structured `INTERNAL` error naming
`workers/requirements.txt`. That is the difference between an operator who can read what to do
and a worker that will not start. The M0-T08 container image ships exactly this state.

## Running a worker by hand

Line framing exists so that this works (SDD §5.12.1) — it is the reason the protocol is
newline-delimited JSON rather than length-prefixed:

```sh
printf '%s\n' '{"type":"hello","proto_version":1}' \
              '{"type":"ping","nonce":1}' \
              '{"type":"shutdown"}' | python3 -u workers/compute_worker.py
```

Note `-u`. CPython block-buffers stdout when it is a pipe, so without it the frames sit in libc
until 8 KiB accumulate. The worker flushes every frame explicitly and the supervisor passes `-u`
as well, but a hand-run without it will look like a worker that has hung.

A `preview` job needs stdin to stay open — the worker exits on EOF, since at that point nothing
will read its answer — so pipe from a process that keeps writing rather than from `printf`.

## Checking the protocol has not drifted

Two hand-written definitions of one wire format drift, which is the whole reason ADR-13 asks for
a version handshake. `crates/astroctl-ipc/testdata/golden-messages.json` is the shared fixture
that makes drift a failing test instead:

```sh
python3 workers/astroctl_ipc.py crates/astroctl-ipc/testdata/golden-messages.json
```

It round-trips every message in the fixture and prints what it made of them. `cargo test -p
astroctl-ipc` runs the same fixture through the Rust enums and compares the two, field for field,
including the closed error-code vocabulary of SDD §4.2 (T-IPC-1). **A protocol change is an edit
to three files** — `protocol.rs`, `astroctl_ipc.py`, and the fixture — and the tests exist to
stop it being an edit to fewer.

## Style and CI

`.github/workflows/ci.yml` runs `ruff check workers/` and `python3 -m compileall workers/` on
every push. Both must pass; there is no `pyproject.toml`, so ruff's defaults apply — which
notably include `BLE001` and `RUF100` but *not* `E402`. Layout is `ruff format` (88 columns).
That one is not gated in CI, so run it before pushing rather than after someone else has to.

A deliberate rule waiver carries its reason at the site, the same convention as
`scripts/check-async.sh`:

```python
except Exception:  # noqa: BLE001 - see below; the breadth is the requirement
```

Comments here follow the same rule as the Rust: they explain *why*, name the requirement or the
failure prevented, and never restate the code.
