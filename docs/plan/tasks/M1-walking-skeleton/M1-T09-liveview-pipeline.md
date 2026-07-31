# M1-T09 — Live view pipeline, /ws/liveview, preview panel

**Milestone:** M1 · **Track:** A+C · **Depends on:** M1-T08 · **Crates:** astroctl-pipeline, astroctl-field, frontend/
**Size:** L · **Status:** done
**Spec:** SDD §5.7, §5.8.3 (two-socket hub), §8.3(5)(6); PRD CAM-05/06, IPP-04
**Tests gated:** T-HOL-1 (with T16's harness; basic version here)

## Objective

The visual feedback loop: camera live view streaming and last-captured-frame preview, on a
dedicated binary WS that can never head-of-line-block control traffic.

## Scope

- `/ws/liveview` endpoint: binary JPEG frames, per-client depth-1 replace queue, auth on upgrade
- Live view source: camera facade's stream (simulator fps-limited), per-client rate adaptation hook (fixed rate now, adaptive later — leave the seam)
- Preview source: on `frame.saved`, decode job → blocking pool (queue depth 1, replace semantics per SDD §5.7): FITS/RAW read → quarter-res → asinh auto-stretch → JPEG q85 → cache `preview/light_<id>.jpg` → push once on liveview socket + `capture.progress: preview_ready`
- Preview also served as `GET /api/session/frames/{id}/preview.jpg`
- PWA: live view panel with start/stop, preview display switching to newest frame, pinch-zoom basic
- **Distinguish "paused because capturing" from "stream broken"** (SDD §5.3.1, §5.7): a gap driven by `capture.progress` is normal and must not trigger reconnect, alerts, or client teardown; the panel shows a capturing state with countdown. Only an unexplained stall is a wedge. The simulator can reproduce this now — give it a blocking capture — so the behaviour is built and tested before the real camera makes it unavoidable in M2
- Decode implementation for M1 handles the simulator's FITS; the libraw CR3 path is M2 — structure the decoder as `enum SourceFormat` so M2 adds a variant, not a rewrite

## Acceptance criteria

- [ ] Live view visibly streams in the PWA; stopping closes the stream server-side (no orphan work)
- [ ] Burst of 5 captures: only newest previews rendered (replace semantics verified), all 5 frames durable
- [ ] Basic HOL check: saturate liveview socket with large frames on a throttled connection; `/ws` position events keep 1 Hz cadence (full shaped-link version in T16)
