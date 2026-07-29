# M1-T16 — E2E suite, fault injection, session log, demo script

**Milestone:** M1 · **Track:** all · **Depends on:** all M1 tasks · **Crates:** tests/, scripts/
**Size:** L · **Status:** not started
**Spec:** SDD §9 (T-E2E-1, T-HOL-1, T-DUR-1 full versions); IMP §2/M1 exit criteria, §5
**Tests gated:** T-E2E-1, T-HOL-1 (shaped link), plus wiring T-SLW-1/T-STALE-1/T-DUR-1/T-XFER-1/T-ING-1/T-IPC-1/T-ISO-1 into one suite

## Objective

Turn the M1 exit criteria into an executable, CI-run test suite plus a scripted demo — the
permanent health signal for all later work.

## Scope

- E2E harness: spawns both binaries (temp dirs, ephemeral ports, single-machine mode) + drives the REST/WS API as a client; helper lib for scenario scripts
- T-E2E-1: connect sims → goto → capture ×3 → assert event sequences, durable frames, transfer acks, preview arrival ≤ 10 s each
- Fault scenarios (using FaultPlans + process kills): stack death mid-session (queue/drain), field restart (session + journal recovery), mount disconnect during slew (watchdog alert)
- T-HOL-1 full: `tc`/`toxiproxy`-shaped 1 Mbit link; saturate liveview; assert `/ws` cadence and e-stop POST ≤ 2× baseline
- **T-ISO-1** (SDD §9): configure `SimulatorCamera` with a ~2 s blocking capture and a slow download to mimic the measured R10 behaviour, then assert *during* it — `mount.position` cadence holds 1 Hz with no gap > 1.5 s, `/api/mount/position` and `/api/system/health` p99 ≤ 2× idle baseline, e-stop still meets its ≤ 20 ms budget, no bus subscriber lags. Repeat with the decode pool saturated. Baselines are captured in the same run, never hardcoded
- Session JSONL log verification: every scenario's event log replayable — parse full file, reconstruct final state, compare to API state (SES-07 basic)
- `scripts/demo-m1.sh`: launches two nodes with demo config, prints the phone URL + token QR; demo walkthrough doc `docs/plan/tasks/M1-walking-skeleton/DEMO.md` matching the IMP exit narrative
- CI: suite wired as the `e2e` job (activates M0-T07 placeholder); shaped-link test may be nightly-only if slow — document the split

## PR split

The largest task in M1 and deliberately so — it is the milestone's health signal, and its parts
share one harness. Five commits:

1. E2E harness (two-binary spawn, REST/WS client helpers) + T-E2E-1
2. Fault scenarios (stack death, field restart, mount disconnect) + T-XFER-1/T-ING-1/T-IPC-1 wiring
3. T-ISO-1 thread-isolation suite (see below) — the PRF-04 regression guard
4. T-HOL-1 on a shaped link + CI job wiring (activating the M0-T07 placeholder)
5. `scripts/demo-m1.sh` + `DEMO.md`

## Acceptance criteria

- [ ] All named tests green in CI from a fresh clone
- [ ] Demo script: two machines, phone on VPN, full IMP §2/M1 demo executes as written
- [ ] Flake check: e2e suite ×20 consecutive runs, zero flakes (fix or quarantine with issue, no silent retries)
