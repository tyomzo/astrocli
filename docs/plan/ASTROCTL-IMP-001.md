# AstroCtl — Implementation Plan

**Document ID:** ASTROCTL-IMP-001
**Version:** 1.2.0
**Author:** Artiom
**Date:** 2026-07-29
**Status:** Draft
**Governing documents:** ASTROCTL-PRD-001 v1.13.0, ASTROCTL-ADD-001 v1.3.1, ASTROCTL-SDD-001 v1.6.2
**Change note (1.1.1):** Governing pins advanced after the dependency survey (PRD v1.7.0). §6 gains the risk that Phase 2a's star detection has no existing Rust binding.
**Change note (1.1.2):** Pins advanced to PRD v1.8.0 after the crates were actually built (`docs/evidence/dependency-survey-2026-07-29.md`). §6: the RAW-decoder risk is resolved to `rawler` with only speed/memory left to confirm, and a toolchain-drift risk is added — the 1.97.1 pin is a hard floor, not a preference.
**Change note (1.1.0):** Governing-document pins updated — SDD v1.1.0 now designs the M1 stack-side elements this plan always required (transfer, ingest, worker IPC), so §1's contract table no longer points at deferred design. Auth added as the third declared phase deviation. M0 crate scaffolding now references ADD §5.6 (the complete 14-crate layout, which gained `astroctl-guiding` in ADD v1.2.0) rather than SDD §3 (a subset), and the M0 deliverable list names the stack proxy its exit criterion already assumed.

**Change note (1.1.3):** M2-T01 run early because the camera became available; §6's bulb risk retired.

**Change note (1.1.4):** Pins advanced; T-ISO-1 added to M1's gated tests.

**Change note (1.1.5):** Pins advanced; T-HIL-1 step 2 executed early against the real mount.

**Change note (1.1.6):** Pins advanced after the read-only mount protocol survey.

**Change note (1.1.7):** Pin advanced to SDD v1.4.0 (mount action-opcode corrections).

**Change note (1.1.8):** Pins advanced; mount motion Phases 1–4 executed.

**Change note (1.2.0):** M0 gains T08, a two-node container harness, and its exit criterion is restated in those terms. Development is single-host; containers are how one machine tests the two-machine shape. This replaces a ranked ladder of loopback fallbacks that would have proved nothing about the deployment. The VPN and the Pi remain real-hardware gates landing with field deployment rather than M0.

---

## 1. Strategy

**Walking skeleton first.** The first delivered increment is the complete system shape — PWA, field node, stacking server, all APIs, event flow, frame flow — with every hardware and compute element behind its contract replaced by a simulator or stub. Real implementations then replace their doubles one at a time (**camera first, then mount**) with zero changes to anything else, because every swap happens behind a contract that is already exercised end-to-end:

| Contract | Defined in | Mocked by (M1) | Replaced in |
|----------|-----------|----------------|-------------|
| `Camera` trait | SDD §5.1 | `SimulatorCamera` (synthetic star fields) | M2 (gPhoto2) |
| `MountDevice` trait | SDD §5.1 | `SimulatorMount` (ramps, settle, drift) | M3 (Skywatcher) |
| Field↔stack HTTP (ingest + ack) | ADD ADR-05, SDD §5.10/§5.11 | real (thin) — it *is* the orchestration under test | — |
| Worker IPC (ADR-13) | SDD §5.12 | stub Python worker (stretch preview, no stacking math) | Phase 2b (real stacking inside the same worker) |
| REST/WS API | SDD §5.8 | real from M1 | — |

Three deliberate deviations from the PRD phase order, all de-risking. Where they conflict, **this plan's milestone order supersedes the PRD §9 phase list**; the PRD phases remain the statement of scope, not of sequence:

1. **The stacking server appears in the first increment** (PRD defers it to Phase 2b) — but only as a skeleton: ingest endpoint, ack, stub worker, preview push. Two-node orchestration, auth, the proxy, and the IPC plumbing are the highest-integration-risk items; exercising them from week one is the point of this plan. No stacking math is implemented early.
2. **Camera before mount** (PRD lists mount first). The camera driver carries the top implementation risk (gphoto2 crate coverage for the R10 — ADD §10), needs no clear skies and no field setup to validate on a desk, and real frames flowing makes every downstream feature real. The mount lands last of the Phase-1 trio because it needs the HIL bring-up discipline (T-HIL-1) and benefits from a UI that is already trustworthy.
3. **Authentication ships in M0** (PRD defers SEC-01/02/04 to Phase 2b). Retrofitting auth across an existing route table is exactly the kind of cross-cutting change that this plan's crate structure exists to avoid, and a field node that has ever run unauthenticated on a VPN is a habit, not a milestone. The token check and the startup refusal (SDD §4.5) cost hours in M0 and days later. Tier *enforcement* (SEC-03) still arrives in Phase 2c; only the annotation slot exists from M0.

**Simulators are products, not scaffolding.** They are the PRD requirement HAL-11, keep CI hardware-free forever, and get fault injection (SDD §9) so failure paths are testable on every commit.

## 2. Milestones

### M0 — Scaffolding

Repo + workspace + toolchain so that every later PR is small.

- Cargo workspace with **all 14 crate skeletons per ADD §5.6** — the complete layout, not the M0–M3 subset sketched in SDD §3 (empty but compiling, dependency rules enforced by CI job)
- `astroctl-core`: domain types, error enums, event schema, config structs + validation (SDD §4)
- Frontend pipeline: Vite build → `include_dir!` embedding → served by a hello-world axum app
- Both binaries start, load config, refuse to start without auth token (SEC-01/02 startup check), serve `/api/system/health`
- Field node reverse-proxies `/stack/*` to the stack node with auth forwarded (ADR-07) — the exit criterion below depends on it
- CI: fmt, clippy, test, frontend build, dependency-rule lint
- Two-node container harness (M0-T08): field and stack as separate containers on their own network namespaces, with a shapeable link between them
- **Exit:** the two containers run, and the PWA loads from the field container showing both nodes' health through the proxy. Development is single-host — containers are how one workstation exercises the two-machine shape honestly, and they make the shaped-link tests CI-runnable. The VPN and the Pi stay real-hardware gates that land with field deployment, not M0.

### M1 — Walking skeleton (the GUI + two-node orchestration delivery)

Everything visible and clickable; all devices simulated; frame flow real end-to-end.

Field node:
- HAL traits + registry; `SimulatorMount` (position model, slew ramps, settle time) and `SimulatorCamera` (synthetic star-field frames — FITS internally, JPEG preview; configurable noise/FWHM per PRD §4.5)
- SafeMount wrapper: altitude/meridian limits, **slew TTL dead-man's switch**, e-stop path (priority semantics against the simulator)
- Frame store with real durability discipline (tmp-fsync-rename, ID reservation)
- Live view pipeline against simulator frames (decode pool, stretch, `/ws/liveview`)
- Full Phase-1 route table incl. staleness/idempotency envelope (SDD §5.8.1) and both WS endpoints
- Transfer agent, minimal-but-real: queue dir, SHA-256, HTTP upload, ack handling, retry loop
- Event bus → WS hub → session JSONL log

Stacking server:
- Ingest endpoint (checksum verify, ack), session mirror layout
- Worker supervisor + **stub Python worker** speaking the versioned IPC protocol (ADR-13): receives frame path, returns a stretched JPEG preview (no stacking) — this validates spawn/health-ping/restart/protocol-version machinery with trivial compute
- Preview push over stack WS, proxied to the operator through the field node

PWA:
- All Phase-1 screens (SDD §5.9): connect, mount panel with D-pad (TTL renewal), camera panel, live view/preview, header status incl. link health + e-stop
- Stack status panel: connection, queue depth, last preview (USB-06 subset)

Tests gated: T-E2E-1 (against simulators), T-SLW-1, T-STALE-1, T-HOL-1, T-DUR-1, T-XFER-1, T-ING-1, T-IPC-1, T-ISO-1.

**Exit (demo):** from a phone on the VPN — connect simulated devices, slew to coordinates and watch predicted/confirmed position, capture a synthetic frame, see it saved locally, transferred, acknowledged, and its preview return from the stacking server into the UI. Kill the stack node mid-session: capture continues, queue grows, reconnect drains it. E-stop halts a simulated slew instantly.

### M2 — Real camera (Canon R10)

Swap `SimulatorCamera` → `CanonGPhoto2Camera` behind the unchanged `Camera` trait.

- **First task, before any driver structure: bulb + CR3-download spike** against the real R10 with the `gphoto2` crate — this is the go/no-go on the top ADD §10 risk; outcome decides the per-operation CLI-fallback table
- Camera thread model (SDD §5.3.1), capture flow with durability (§5.3.2), settings enumeration, battery/storage, live view stream
- Wedge-recovery (thread respawn + USB reset) with a pull-the-cable test
- Live view pipeline now decoding real CR3 previews (libraw path, T-SOAK memory watch)

**Exit:** desk session — real CR3s captured from the PWA (incl. bulb), preview in UI ≤ 3 s after exposure end, frames transferred to stack node, cable-pull recovery works, T-CAM-1 green.

### M3 — Real mount (HEQ5 Pro)

Swap `SimulatorMount` → `SkywatcherMount`. HIL bring-up is a scripted sequence, not ad hoc:

1. Codec golden-vector tests vs EQMOD traces (T-COD-1) — before powering anything
2. Handshake read-only session: version, CPR, timer freq; compare against EQMOD reference values (opcode verification per PRD §4.2 note)
3. Motion off the tripod head / clutches loose: low-speed slews, stop, e-stop latency measurement (T-SER-3 on real wire)
4. Tracking rates; position poll stability overnight (soak)
5. Goto accuracy loop under the sky; park/unpark
6. Limits verified: altitude rejection, meridian auto-stop, TTL expiry with real motors

**Exit = PRD Phase 1 exit criteria**, real hardware, plus T-HIL-1 checklist archived in the session log.

### M4 → onward

Continue per PRD phases with the SDD increment plan: 2a (session FSM, solver, planning/erfa, control pipeline) → 2b (real stacking: compute worker grows from the stub, calibration library, transfer hardening incl. pacing rule §8.3.7) → 2c (post-chain, LLM layer) → 3 → 4. Each increment repeats the M1 pattern where possible: contract + simulator first, real implementation second.

## 3. Workstreams and parallelism

Three tracks can run concurrently after M0; contracts (core types, API schemas, IPC protocol) are frozen at the start of M1 so tracks integrate continuously:

| Track | M1 content | Owner-shaped skills |
|-------|-----------|---------------------|
| A: Field backbone | HAL, simulators, SafeMount, frame store, API, transfer | Rust, hardware protocols later |
| B: Stack backbone | ingest, supervisor, stub worker, IPC crate, preview | Rust + a little Python |
| C: PWA | all screens, WS store, predictive display, link health | TypeScript/React |

Integration cadence: the E2E simulator test (T-E2E-1) runs in CI from the first week of M1 and is the tree's health signal.

## 4. Sizing and PR sequencing

### 4.1 Sizing (relative, single developer + AI pair)

| Milestone | Size | Dominant cost |
|-----------|------|---------------|
| M0 | S | decisions already made in SDD; mostly mechanical |
| M1 | L | breadth: every subsystem appears; nothing is deep |
| M2 | M | driver depth + hardware iteration loops |
| M3 | M | HIL discipline + protocol verification, calendar-gated by bench/sky time |

### 4.2 PR sequence inside M1

Each step keeps the tree green and demoable; the task tree under `docs/plan/tasks/` expands these into individual task files:
1. core types + event bus + WS hub + health
2. HAL + SimulatorMount + mount panel (position streaming visible in UI)
3. SafeMount + TTL + e-stop (safety demonstrable on simulators)
4. SimulatorCamera + frame store + capture flow + preview
5. transfer agent + stack ingest + stub worker + stack panel
6. staleness envelope + link-health UI + fault-injection tests

## 5. Definition of done (all milestones)

- SDD-named tests for the touched elements green in CI; new behavior has a test
- No `unwrap`/`expect` on I/O paths; errors reach the operator with a code from the closed enum
- Events emitted for every state change (UI never needs REST polling)
- Docs: SDD updated if the design deviated during implementation (version bump + change note) — the documents stay truthful or they die
- Demo recorded from the operator device (phone/tablet), not localhost

## 6. Risks specific to this plan

| Risk | Mitigation |
|------|-----------|
| Skeleton breadth (M1) sprawls — everything half-built, nothing demoable | PR sequence in §4.2 keeps a demoable tree; stub worker does *no* real compute; stacking math explicitly out of scope until 2b |
| Contracts churn after tracks parallelize | Contracts frozen at M1 start; changes require a version bump in the schema and a note in the SDD |
| Simulator fidelity too low → real-driver swap surprises | Simulators implement timing behavior (ramps, settle, exposure duration, download delay), not just return values; fault injection from day one |
| ~~R10 spike fails on bulb (M2)~~ — **RETIRED 2026-07-29**, ahead of M0 | The spike was run early because the hardware was available: bulb works through the crate (`eosremoterelease` press/release, camera-reported `BulbExposureTime 9` for a 10 s hold). The CLI fallback design (SDD §5.3.3) stays, unused. Evidence: `spikes/gphoto2-r10/FINDINGS.md` |
| **Phase 2a's star detection has no existing Rust binding** — `sep` is not a crate (PRD §7, §10). Discovered at the start of 2a it costs a spike; discovered mid-pipeline it stalls the control pipeline, registration, and guiding at once | Open Phase 2a with a `sep-sys` FFI spike on the M2-T01 pattern — prove the binding against a real frame before any pipeline design depends on it. libsep's API is small, so this is a bounded task, but it is *unplanned work that currently has no task file* |
| RAW decoder for CR3 previews — **resolved to `rawler` on build evidence** (`docs/evidence/dependency-survey-2026-07-29.md`); residual risk is decode speed and peak RSS on the field node, not correctness | M2-T01 validates it against a real R10 file and on the target hardware; PRF-05 is the gate. The M1-T09 `SourceFormat` seam keeps the choice additive if it has to be revisited |
| Toolchain drift between workstation, field node and CI | `rust-toolchain.toml` pins **1.97.1** exactly (M0-T01). The floor is real, not cosmetic: `rusqlite` 0.40 does not build on 1.94, which is what the workstation defaulted to when the pack was written |
