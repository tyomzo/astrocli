# M2-T05 — Desk integration: real-camera E2E + CR3 preview + soak

**Milestone:** M2 · **Depends on:** M2-T03, M2-T04 · **Crates:** astroctl-pipeline, config, tests
**Size:** M · **Status:** done (three items need a human at the desk — see below)
**Spec:** IMP §2/M2 exit criteria; SDD §5.7 (CR3 decode variant), §9 T-SOAK subset

## Objective

The M1 demo, rerun with a real camera: prove the swap changed nothing outside the driver,
and add the CR3 decode path the preview pipeline was structured for.

## Scope

- Preview decoder: add the CR3 variant (half-size decode → quarter-res → stretch) to the M1-T09 `SourceFormat` enum, using **the decoder M2-T01 selected** — not necessarily libraw; JPEG sibling used when RAW+JPEG format is active (cheaper)
- Config: switch example field config camera driver to `gphoto2` with `simulator` documented as the alternative; sim remains CI default
- Desk E2E (scripted, evidence-captured): PWA session — connect R10, settings, live view, 5 timed + 1 bulb capture, previews ≤ 3 s after exposure end (PRF-timing log), frames acked by stack node, stack preview returns
- 2 h soak: capture every 60 s; field node RSS ≤ 512 MB steady (PRF-05 with real decode spikes excluded per definition), no wedges, zero lost frames
- Update `DEMO.md` with the real-camera variant

## Acceptance criteria

- [x] E2E script passes with evidence bundle (timings, logs, RSS plot) committed under `docs/evidence/m2/`
  — `scripts/desk-e2e.sh`, bundle in `docs/evidence/m2/`. Preview **0.124 s** against a 3 s budget;
  live view **5.1 fps** at 149 KB/frame; frame acked by the stack node. The five timed frames need
  the mode dial moved — see below.
- [x] Zero code changes outside astroctl-drivers, the decoder variant, config, and tests — diffstat
  attached as proof of the contract's integrity — `docs/evidence/m2/diffstat.txt`. **`astroctl-drivers`
  is not in the diffstat at all**: M2-T02..T04 had finished it, so a real camera end to end needed no
  driver change either.
- [~] IMP §2/M2 exit criteria all demonstrated — everything but the 2 h soak and the cable pull,
  both of which are one command and belong to an operator at the desk.

## Results

**M2-T03's open question is answered.** `an_aborted_bulb_does_not_poison_the_next_one`, run on the
healthy body: the abort returned in 1.40 s and the following 10 s bulb **succeeded** — camera
reported 9 s, a well-formed 1.4 MB CR3. The buffer-orphan hypothesis holds; M2-T03's failure was
the fading battery, not `eosremoterelease` needing a reset to `None`. That suspect is cleared.

**CR3 decode, on a real R10 frame, release build:** half-size decode (24 MP → 3000×2000) **74-81 ms**,
quarter-res + asinh + JPEG **6 ms**, total **80-87 ms**. Peak RSS +36 MB for one decode.

**The preview the operator receives is 750×500, one component** — CR3 6000×4000 → half-size
3000×2000 → quarter-res 750×500 → asinh → Luma JPEG, served from
`/api/session/frames/{id}/preview.jpg`.

### CR3 vs the JPEG sibling

M2-T05 asks for the JPEG sibling to be used when `RAW+JPEG` is active, because it is cheaper.
**That is not reachable, and the reason is in `astroctl-field`, not here.** `camera.rs` deletes
every file but the raw before publishing `frame.saved`:

> M1 keeps one file per frame id; the camera JPEG is not the authoritative frame (HAL-03) […] so
> keeping it would be a second copy of the same exposure that nothing reads.

By the time the preview pipeline sees a path, the sibling is gone. Reported rather than patched:
keeping it is a frame-store shape change, which is outside this task's permitted diffstat and is a
decision about what a *session* contains rather than about a decoder.

It would also buy less than it looks. The CR3 path costs 80 ms against a 3000 ms budget, so the
saving is not needed; and a camera JPEG is **not linear** — the body has applied its picture style,
white balance and an sRGB curve — so an asinh stretch designed for linear light produces a
*different* image from it, not a cheaper version of the same one. The raw arm is the one to trust
for judging a sub.

The JPEG arm is implemented anyway, for a gap M2-T05 does not mention: `ImageFormat::JPEG` is a
selectable capture format, and until now such a frame sniffed to `UnknownFormat` — a durable frame
the node logged as unpreviewable, once per capture, forever.

### The `reconnecting` publish

§4.3's `CameraStatus` gained no field. The route is `DISK_LOW`'s: a free-string `Alert` code,
`CAMERA_RECONNECTING` (warning going down, info coming back) and `CAMERA_LINK_FAULTED` (critical).

What makes that honest rather than merely convenient: during a recovery the driver's `link()`
returns `NotConnected`, **not** `Busy`, so `status_from` already takes its failure arm and
`camera.status` publishes `connected: false` — which is true. The boolean carries the fact; the
alert carries the reason and the fact that something is being done about it. `publish_link_state`
also forces the status publish on **every** transition, because `Topic::Alert` is not stateful and
never reaches a client's connect snapshot, and because REL-03's 30 s recovery is half the status
poll's own 60 s period.

### The soak found the thing M2-T03 could not pin down

A 16-minute run (16 × 4 s bulb at 60 s): **RSS peak 101 MB** — 19 % of PRF-05's 512 MB, flat, with
the decode spikes inside the samples. Memory is not a concern on this path.

**Rounds 11-16 lost their frames, and it is the third sighting of one failure.** After ten good
bulbs the R10 stopped announcing files while continuing to answer everything else: battery 100 %,
storage unchanged, config reads fine, every capture accepted, every shutter fired, every
`exposing` → `downloading` published. No node wedge — it kept working perfectly, the camera did
not.

The driver's diagnostic fires six times and blames long-exposure noise reduction. That is the
right first guess for one slow frame and **the wrong cause here**: LENR is per-frame, so round 1
would have failed too. Something cumulative changed at round 11.

**Then the body left the USB bus.** Minutes after the soak, node idle, nothing capturing, battery
still 100 %: `lsusb` stopped listing `04a9:32f8` at all. No software reset, nothing unplugged. It
needs a physical power cycle.

The whole progression, on a full battery, in one sitting: ten good bulbs → six where the shutter
fires and no file is ever announced → the device off the bus.

M2-T03 met the middle stage and left it confounded with a fading battery. M2-T04 met the last one
after a `USBDEVFS_RESET` and wrote "it is *not* only the battery". **This is the third sighting,
with neither a flat battery nor a reset available to blame.** Sustained bulb capture alone reaches
it.

This bears on M2's exit criteria directly: the 2 h soak asks for ~120 captures and this body
stopped at ten. The useful next step is one variable at a time — exposure, interval,
`capture_extra_seconds`, and long-exposure NR on the body — to find what moves the boundary.
**That is a task of its own**, and it is the biggest open question M2 leaves.

## Spec gaps and findings

- **The shipped example config could not start a node.** `config/field-node.example.yaml` has said
  `driver: gphoto2` since M0 while `build_camera` registered only `simulator`; the test fixtures
  rewrite the string to `simulator` on the way past, so nothing ever caught it. Fixed here.
- **A blown frame previews as black, identically to a dark frame.** With >99.5 % of a frame at one
  value both stretch percentiles land on it, the window collapses, and `stretch.rs`'s
  `white = black + 1.0` guard maps that value to zero. The guard is deliberate and matches
  `workers/compute_worker.py`, so it is not a defect to fix in one implementation — but the
  consequence is that an operator judging exposure from the preview cannot tell over- from
  under-exposure. Worth a §5.7 note, or a preview that reports its own window.
- **`rawler`'s R10 white level is not the sensor's clipping point** — the profile says 12735,
  saturated photosites read 16383. Normalising by it clips the top quarter of the range. The
  decoder does not rescale at all; the percentile window downstream makes any linear map
  equivalent, so there was nothing to gain and highlights to lose.
- **`rawler` is LGPL-2.1 where the workspace is MIT.** Compatible to use; weak copyleft (relinking)
  to *ship*. `docs/evidence/dependency-survey-2026-07-29.md` recorded this crate's decoders,
  camera profiles and regression fixtures and did not record its licence. Wants a decision before
  a binary is distributed, not before one is run at a desk.
- **PRF-05 and glibc arenas.** Repeated 24 MP CR3 decodes settle at 253 MB RSS on a 32-core host —
  flat, not a leak, and it scales with cores rather than frames. `MALLOC_ARENA_MAX=2` takes the
  same measurement to 102 MB with no cost in decode time. The desk scripts set it; **`deploy/`
  does not**, because `deploy/` is outside this task's diffstat. That is the follow-up.
- **The mode dial makes five-timed-plus-one-bulb impossible in a single run.** On **M** the body
  offers no `bulb`; on **B** it offers nothing else. The script detects which and prints the
  command for the other half. Not a defect — a property of the body worth writing down, since the
  task's own wording assumes one run can do both.
- **With the dial on B the body reports its second shutter as `Unknown value df00`** — libgphoto2
  declining to name a Canon code, not a driver fault.
- **`libgphoto2-dev` is not installed on the reference desk machine**, and has not been for any of
  M2. Every hardware run has gone through an unpacked `.deb` in a scratch directory;
  `scripts/desk-e2e.sh` prints the recipe when the build fails.

## Pending — needs a human at the desk

Each is one command; all are in `docs/evidence/m2/README.md` §6.

1. **The five timed frames.** Turn the mode dial to **M**, then
   `scripts/desk-e2e.sh --no-build --frames 5 --bulb 0`. Lower the ISO and stop down first, or the
   preview will be a black rectangle for the reason above.
2. **The 2 h soak.** `scripts/desk-soak.sh --hours 2 --bulb 4`. A 16-minute run is recorded as
   proof the machinery works; the full one has **not** been run.
3. **The cable pull.** `scripts/desk-cable-pull.sh`, then pull at the *camera* end when it says
   ARMED. The script is the observation half only — the software `USBDEVFS_RESET` stand-in is
   destructive on this body (M2-T04) and is deliberately not used.
4. **The battery reading against the body's display**, still open from M2-T04. The driver reads
   100 %; ±5 % is the bar.
