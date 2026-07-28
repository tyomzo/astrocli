# AstroCtl — Implementation Plan

**Document ID:** ASTROCTL-IMP-001
**Version:** 1.0.0
**Author:** Artiom
**Date:** 2026-07-28
**Status:** Draft
**Governing documents:** ASTROCTL-PRD-001 v1.5.0, ASTROCTL-ADD-001 v1.1.0, ASTROCTL-SDD-001 v1.0.2

---

## 1. Strategy

**Walking skeleton first.** The first delivered increment is the complete system shape — PWA, field node, stacking server, all APIs, event flow, frame flow — with every hardware and compute element behind its contract replaced by a simulator or stub. Real implementations then replace their doubles one at a time (**camera first, then mount**) with zero changes to anything else, because every swap happens behind a contract that is already exercised end-to-end:

| Contract | Defined in | Mocked by (M1) | Replaced in |
|----------|-----------|----------------|-------------|
| `Camera` trait | SDD §5.1 | `SimulatorCamera` (synthetic star fields) | M2 (gPhoto2) |
| `MountDevice` trait | SDD §5.1 | `SimulatorMount` (ramps, settle, drift) | M3 (Skywatcher) |
| Field↔stack HTTP (ingest + ack) | ADD ADR-05 | real (thin) — it *is* the orchestration under test | — |
| Worker IPC (ADR-13) | SDD-planned v1.2.x | stub Python worker (echo/stretch preview) | Phase 2b (real stacking) |
| REST/WS API | SDD §5.8 | real from M1 | — |

Two deliberate deviations from the PRD phase order, both de-risking:

1. **The stacking server appears in the first increment** (PRD defers it to Phase 2b) — but only as a skeleton: ingest endpoint, ack, stub worker, preview push. Two-node orchestration, auth, the proxy, and the IPC plumbing are the highest-integration-risk items; exercising them from week one is the point of this plan. No stacking math is implemented early.
2. **Camera before mount** (PRD lists mount first). The camera driver carries the top implementation risk (gphoto2 crate coverage for the R10 — ADD §10), needs no clear skies and no field setup to validate on a desk, and real frames flowing makes every downstream feature real. The mount lands last of the Phase-1 trio because it needs the HIL bring-up discipline (T-HIL-1) and benefits from a UI that is already trustworthy.

**Simulators are products, not scaffolding.** They are the PRD requirement HAL-11, keep CI hardware-free forever, and get fault injection (SDD §9) so failure paths are testable on every commit.

## 2. Milestones

### M0 — Scaffolding

Repo + workspace + toolchain so that every later PR is small.

- Cargo workspace with all crate skeletons per SDD §3 (empty but compiling, dependency rules enforced by CI job)
- `astroctl-core`: domain types, error enums, event schema, config structs + validation (SDD §4)
- Frontend pipeline: Vite build → `include_dir!` embedding → served by a hello-world axum app
- Both binaries start, load config, refuse to start without auth token (SEC-01/02 startup check), serve `/api/system/health`
- CI: fmt, clippy, test, frontend build, dependency-rule lint
- **Exit:** `astroctl-field` and `astroctl-stack` run on two machines; PWA shell loads over the VPN from the field node and shows both nodes' health via the proxy.

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

Tests gated: T-E2E-1 (against simulators), T-SLW-1, T-STALE-1, T-HOL-1, T-DUR-1.

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

## 4. Sizing (relative, single developer + AI pair)

| Milestone | Size | Dominant cost |
|-----------|------|---------------|
| M0 | S | decisions already made in SDD; mostly mechanical |
| M1 | L | breadth: every subsystem appears; nothing is deep |
| M2 | M | driver depth + hardware iteration loops |
| M3 | M | HIL discipline + protocol verification, calendar-gated by bench/sky time |

Suggested first PR sequence inside M1 (each keeps the tree green and demoable):
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
| Skeleton breadth (M1) sprawls — everything half-built, nothing demoable | PR sequence in §4 keeps a demoable tree; stub worker does *no* real compute; stacking math explicitly out of scope until 2b |
| Contracts churn after tracks parallelize | Contracts frozen at M1 start; changes require a version bump in the schema and a note in the SDD |
| Simulator fidelity too low → real-driver swap surprises | Simulators implement timing behavior (ramps, settle, exposure duration, download delay), not just return values; fault injection from day one |
| R10 spike fails on bulb (M2) | CLI fallback path is designed (SDD §5.3.3); worst case bulb goes through `gphoto2` binary while everything else stays on bindings |
