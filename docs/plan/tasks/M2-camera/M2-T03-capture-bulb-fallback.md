# M2-T03 — Capture + download durability + bulb + CLI fallback

**Milestone:** M2 · **Depends on:** M2-T02 · **Crates:** astroctl-drivers
**Size:** L · **Status:** not started
**Spec:** SDD §5.3.2 (flow), §5.3.3 (per-operation CLI fallback); PRD CAM-03/04, REL-05

## Objective

Real frames, durably: timed capture, bulb per the T01 verdict, streamed download into the
frame store's begin/commit discipline, and the composable CLI fallback.

## Scope

- Timed capture: trigger → wait capture event → download streamed to `begin_frame` tmp path → `commit_frame` (fsync-rename from M1-T07) → existing capture flow events fire unchanged
- Bulb: crate path or CLI path per FINDINGS.md; duration timer + release; abort mid-bulb releases shutter and cleans up
- `GPhoto2Cli` implementing the internal `CamOps` trait per operation (subprocess, parsed output, timeouts); composition driven by `camera.ops_via_cli` config list — mixing verified per T01 (e.g. bulb via CLI while settings stay on bindings)
- RAW+JPEG format: both files downloaded; JPEG stored alongside CR3 in the session (feeds preview cheaply)
- Failure paths: download failure → no partial frame visible (tmp cleanup), distinct error codes for trigger vs download failures

## Acceptance criteria

- [ ] 10 real captures incl. 2× bulb (30 s, 60 s): all CR3s open in libraw, durable, metadata correct exposure values
- [ ] Abort mid-bulb: shutter closes (audible/EXIF check), no partial frame, FSM recovers
- [ ] Force `ops_via_cli: ["capture"]`: full flow works through CLI path (proves composition)
- [ ] T-DUR-1 rerun with real camera: kill during download → clean recovery
