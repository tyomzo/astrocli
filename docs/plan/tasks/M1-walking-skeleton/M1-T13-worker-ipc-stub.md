# M1-T13 — Worker IPC crate, supervisor, stub Python worker

**Milestone:** M1 · **Track:** B · **Depends on:** M0 · **Crates:** astroctl-ipc, astroctl-stack, workers/
**Size:** L · **Status:** done
**Spec:** SDD §5.12 (framing, message set v1, handshake, supervision, stub worker); ADD ADR-13, §6.2
**Tests gated:** T-IPC-1

## Objective

Validate the entire backbone↔worker machinery — spawn, handshake, health, restart, job
round-trip — with a worker whose compute is trivial. When real stacking arrives (Phase 2b),
only the worker's insides change.

## Scope

- `astroctl-ipc`: versioned JSON message frames over stdio (length-prefixed lines); message set v1: `hello{proto_version, capabilities}`, `job{id, kind, params, paths}`, `progress{id, pct}`, `result{id, ok, data|error}`, `ping/pong`; typed Rust structs + a small Python mirror module (`workers/astroctl_ipc.py`)
- Supervisor in astroctl-stack: spawn `workers/compute_worker.py` via configured Python interpreter, handshake (proto mismatch → refuse + alert), ping every 5 s, missed×3 → kill, restart with capped backoff, restart counter in health
- Stub `compute_worker.py`: implements handshake + `job{kind: "preview"}` — load frame (FITS via astropy or pure-numpy reader — keep `requirements.txt` minimal), asinh stretch, write JPEG next to frame, return its path. **No stacking math.**
- Crash resilience: worker exception → structured error result; worker segfault/kill → supervisor restart, in-flight job retried once then failed with alert
- Job queue: submit/await API for the backbone (used by T14)

## Acceptance criteria

- [x] Round-trip: submit preview job → JPEG exists → result path correct
- [x] `kill -9` the worker mid-job: supervisor restarts it, job retried, total disruption < 10 s
- [x] Proto version bumped on one side only → clean refusal with actionable log, no hang
- [x] Worker env documented: `workers/README.md` (venv setup, interpreter config key)

## Result notes

The supervisor lives in **`astroctl-ipc`**, behind a default-on `supervisor` Cargo feature,
rather than in `astroctl-stack`. SDD §5.12 titles the section for `astroctl-ipc` and the scope
bullet above says `astroctl-stack`; the feature satisfies both readings and keeps ADD §5.6
rule 6 mechanical — with `default-features = false` the field binary links no process
management at all. `astroctl-stack` wires it in one line (see below) and owns no supervision
logic. Deviation recorded per tasks/README rule 1.

Wiring for whoever picks up M1-T12/T14, in `astroctl-stack`:

```toml
astroctl-ipc = { workspace = true }          # [dependencies]
```
```rust
let workers = astroctl_ipc::supervisor::spawn(&config.workers, &bus);
```

Deliberate decisions beyond SDD §5.12, each argued at its site in `supervisor.rs`:

- Workers start **on demand**, not at boot — `astroctl-stack/src/main.rs` already says so.
  `WorkerStatus::state` is therefore `Option<WorkerState>`, because the frozen event-schema
  enum has no variant for "never started".
- A job that **timed out** is not retried; a job whose **worker died** is. §5.12.3's
  retry-once rule is written for lost work, and a slow job is not lost work.
- A **missing or unexecutable interpreter** is permanent, like a version mismatch: it will
  fail identically on every backoff, so it is refused once with an alert naming the key.
- `MAX_LINE_BYTES` (1 MiB) and a **bounded frame reader**; §5.12.1 sets no bound.
- The stub worker **answers pings on its main thread while computing on another**. Without
  that, §5.12.3's own health check SIGKILLs any job longer than three ping intervals — pinned
  by `pings_are_answered_while_a_job_is_running`, which fails with
  `Crashed { reason: "no answer to 3 consecutive pings" }` if the worker is made
  single-threaded.

Open items for the SDD increment that expands §5.12 (Phase 2b): §4.2 has no `CANCELLED` code
and no worker-shaped code, so everything the supervisor itself produces lands on `INTERNAL`;
and `StackStatus::online()` cannot express "no worker started yet".
