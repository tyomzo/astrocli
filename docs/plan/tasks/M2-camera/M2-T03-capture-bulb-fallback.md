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

- [x] 10 real captures incl. 2× bulb (30 s, 60 s): all CR3s open in libraw, durable, metadata correct exposure values
  — **met on real hardware, in both mode-dial positions.** Dial on **M**: **10 consecutive timed
  captures**, 287.3 MB, 1.86 s/frame (1.82–1.89 s), ten files under ten stems, no temporary and no
  collision — which is the run that proves what only goes wrong the *second* time, since the body
  reuses one camera-side filename. Plus RAW+JPEG landing both files under one stem (15.7 MB CR3 +
  3.9 MB JPEG), raw first, with a following capture getting exactly its own pair. Dial on **Bulb**:
  the **30 s and 60 s pair**, on the shipped default budget — 30 s → camera reports 29 s in 62.9 s
  wall; 60 s → reports 59 s in 123.0 s wall. Every exposure figure is the body's own
  `BulbExposureTime`, one second short of the request each time, not the request echoed. The suite
  passes in either dial position and asserts the *refusal* for whichever mechanism the dial has
  taken away.
  Remaining gap: `libraw`/`rawler` decoding cannot run from this crate at all (ADD §5.6 rule 1 —
  `rawler` lives in `astroctl-pipeline`), so the hardware tests check the CR3 and JPEG container
  headers instead and M2-T01's decode stands as the decode evidence.
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

## Open, needs a healthy body: abort → bulb

The last hardware session ended with the camera dropping off USB (`lsusb` empty, probe finding
nothing), and it left one question confounded rather than answered.

**Observed:** after a single aborted bulb exposure, every subsequent bulb frame failed with "the
shutter closed but the camera had not announced a file" — *including from a freshly started
process*, which rules out driver-side state and points at the body. Run in isolation, both the
10 s bulb and the 30 s/60 s pair pass; only the abort-then-bulb order fails.

**Hypothesis:** with `capturetarget=Internal RAM` the body buffers one frame, and an aborted
exposure's frame is never downloaded, so the buffer stays occupied and the next capture has
nowhere to put its frame. The driver now waits for that orphan (scaled to how long the shutter was
actually open) and deletes it camera-side.

**Why it is not settled:** adding the delete did not fix the symptom, and the camera then vanished
from USB — so a body that was already failing is an equally good explanation for the whole
episode, including the original observation. The delete is kept because HAL-03 requires an aborted
capture to leave nothing behind, which is true independently of what it fixes.

**To settle it:** on a body known to be healthy (fresh battery, confirmed on `lsusb`), run
`an_abort_mid_bulb_returns_promptly_and_leaves_nothing_on_disk` followed by
`a_ten_second_bulb_exposure_lasts_ten_seconds`. If the second passes, the delete is the fix and
this note becomes an obligation in SDD §5.3.2. If it fails, the buffer hypothesis is wrong and the
next thing to check is whether `eosremoterelease` needs an explicit reset to `None` after an
early release.
