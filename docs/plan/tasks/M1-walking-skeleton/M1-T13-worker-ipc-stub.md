# M1-T13 — Worker IPC crate, supervisor, stub Python worker

**Milestone:** M1 · **Track:** B · **Depends on:** M0 · **Crates:** astroctl-ipc, astroctl-stack, workers/
**Size:** L · **Status:** not started
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

- [ ] Round-trip: submit preview job → JPEG exists → result path correct
- [ ] `kill -9` the worker mid-job: supervisor restarts it, job retried, total disruption < 10 s
- [ ] Proto version bumped on one side only → clean refusal with actionable log, no hang
- [ ] Worker env documented: `workers/README.md` (venv setup, interpreter config key)
