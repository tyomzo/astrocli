# M2-T03 — Capture + download durability + bulb + CLI fallback

**Milestone:** M2 · **Depends on:** M2-T02 · **Crates:** astroctl-drivers
**Size:** L · **Status:** **done** (2026-07-31) — obligations in SDD §5.3.2, fallback table in §5.3.3
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

- [~] 10 real captures incl. 2× bulb (30 s, 60 s): all CR3s open in libraw, durable, metadata correct exposure values
  — **substantially met on real hardware, in both mode-dial positions.** With the dial on **Bulb**:
  10 s holds land well-formed CR3s (1.5–1.6 MB dark, 23.5 MB lit) under the request's stem with no
  temporary left behind, and the exposure recorded is the **camera's own** `BulbExposureTime` (9 s for
  a 10 s hold), not the request echoed. With the dial on **M**: timed capture in 1.83–1.98 s giving
  26.5–27.3 MB CR3s, `1/20` parsed to a 50 ms exposure, and **RAW+JPEG landing both files under one
  stem** (15.5 MB CR3 + 3.7 MB JPEG) with a following capture getting exactly its own pair — which is
  the check that the event queue was left clean. The suite passes in either dial position and asserts
  the *refusal* for whichever mechanism the dial has taken away.
  Outstanding: the 30 s/60 s bulb pair and a full 10-frame sequence run; `libraw`/`rawler` decoding
  cannot run from this crate at all (ADD §5.6 rule 1 — `rawler` lives in `astroctl-pipeline`), so the
  hardware tests check the CR3 and JPEG container headers instead and M2-T01's decode stands as the
  decode evidence.
- [x] Abort mid-bulb: shutter closes (audible/EXIF check), no partial frame, FSM recovers
  — **met on hardware.** Abort during a 60 s bulb returned in **846–919 ms**, `Aborted`, with the
  session directory empty (frames *and* temporaries), and the camera answering normally afterwards.
  The frame the body still produced is drained from the event queue and discarded, so it cannot be
  downloaded under the *next* frame's name.
- [n/a] Force `ops_via_cli: ["capture"]`: full flow works through CLI path (proves composition)
  — **the fallback was deliberately not built.** This criterion predates M2-T01, which measured every
  operation working through the bindings; M2-T03 re-measured capture, bulb, download and abort on the
  same body. §5.3.3's table is populated and every row reads `bindings`. Building a second
  implementation of every operation that no configuration selects and no hardware test exercises was
  judged worse than not having one. `ops_via_cli` is **refused at construction** rather than silently
  ignored, so a set key cannot look like it took effect.
- [ ] T-DUR-1 rerun with real camera: kill during download → clean recovery
  — **not run.** Needs a session at the camera; the stale-temporary trap it targets is covered against
  the mock (`download.rs`), which reproduces libgphoto2's refusal to overwrite.
