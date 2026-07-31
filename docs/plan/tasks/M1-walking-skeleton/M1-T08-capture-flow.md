# M1-T08 — Capture flow, camera routes, PWA camera panel

**Milestone:** M1 · **Track:** A+C · **Depends on:** M1-T06, M1-T07 · **Crates:** astroctl-field, frontend/
**Size:** L · **Status:** done
**Spec:** SDD §5.3.2 (flow, minus gphoto2 specifics), §5.6 (FSM skeleton), §5.8.1 camera rows; PRD CAM-01..04/08

## Objective

End-to-end capture against the simulator: request → exposure → durable frame → events → UI,
via the three-state orchestrator skeleton.

## Scope

- Camera facade task owning `Arc<dyn Camera>`; orchestrator skeleton FSM (Idle/Capturing/Faulted per SDD §5.6) mediating capture requests
- Capture flow per SDD §5.3.2 order using T07's begin/commit: capture → download (simulated) → commit → sha256 (spawn_blocking) → metadata → `frame.saved` event; `capture.progress` events at each stage
- Bulb path with countdown progress; abort route
- Routes: camera connect/disconnect, settings GET/PUT (available values from capabilities), capture, capture/abort, battery, storage, `/api/session/current` + frame listing
- Disk-critical behavior: capture request below critical threshold → **507** with `DISK_FULL` code (REL-12). Not 409 — SDD §4.2 fixes one status per error code, and ingest already raises `DISK_FULL` as 507 (§5.11.2). A code that means different things on different nodes is exactly what the closed enum exists to prevent
- PWA camera panel: settings selectors from available values, capture button with progress states, bulb duration input + countdown, battery/storage display; session frame list view

## PR split

Broad because the panel is the only way to demonstrate the flow, but it lands in three commits:

1. Camera facade + orchestrator FSM skeleton + capture flow (backend only, exercised by tests)
2. Camera/session routes + disk-critical behavior
3. PWA camera panel and frame list

## Acceptance criteria

- [ ] Full flow test: capture via API → `capture.progress` sequence (exposing→downloading→saved) → frame + metadata durable → appears in session listing
- [ ] Abort during bulb: prompt return, no partial frame visible, FSM back to Idle
- [ ] Second capture while Capturing → 409 `Busy`; Faulted state requires explicit ack route to clear
- [ ] Panel drives all of the above from a phone viewport
