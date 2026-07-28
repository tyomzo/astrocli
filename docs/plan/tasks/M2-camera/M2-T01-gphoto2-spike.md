# M2-T01 — Spike: bulb + CR3 download via gphoto2 crate, and RAW decoder selection

**Milestone:** M2 · **Depends on:** M1 · **Crates:** scratch (`spikes/gphoto2-r10/`, not in workspace)
**Size:** M · **Status:** not started
**Spec:** ADD §10 (top risk), SDD §5.3.3 (fallback table this spike populates), §5.7 (decoder seam); PRD §4.3, §7 (RAW-decoder candidates and their maturity)

## Objective

**Report task, not production code.** Two go/no-go questions, both answerable only with the
camera on the desk and a real CR3 in hand:

1. Which operations the `gphoto2` crate covers and which need the CLI fallback — evidence for
   the M2 driver design.
2. **Which RAW decoder to build the preview path on.** PRD §7 deliberately leaves this open:
   `libraw`/`libraw-sys` are at 0.1.1 (thin bindings over a library that certainly handles CR3),
   `libraw_rs_vendor` removes the system-library dependency, and `rawler`/`rawloader` are mature
   pure-Rust decoders whose CR3 coverage is unconfirmed. Binding maturity versus decoder
   maturity is not a tradeoff that can be settled from crates.io metadata.

## Scope

Standalone binary exercising, in order, with timings logged:
1. Autodetect + connect; read config tree (dump all keys — this is the settings map source)
2. Get/set ISO, shutter, aperture, image format; enumerate choices
3. Timed capture (1/30 s) + CR3 download; verify the file is a well-formed CR3 (keep it — it is the fixture for step 8)
4. **Bulb**: attempt via PTP remote-release config (`eosremoterelease`/equivalent) — 10 s exposure; the critical unknown
5. Live view: preview frame fetch rate over 30 s; measure fps + latency
6. Battery + storage reads
7. Repeat 3 with USB cable pulled mid-download: characterize the failure mode and recovery (context reinit? power cycle needed?)
8. **RAW decoder bake-off** against the CR3 from step 3. For each of `libraw`, `libraw_rs_vendor`,
   `rawler`, `rawloader`: does it open an R10 CR3 at all; can it produce a *half-size* decode (the
   §5.7 preview path needs speed, not full resolution); wall-clock and peak RSS per decode; does
   it expose the Bayer pattern and black/white levels the Phase 2b pipeline will need; what does
   the build require (system libnraw? vendored C? pure Rust?). Record failures as findings — "does
   not support CR3" is a result, not a dead end

Deliverable: `spikes/gphoto2-r10/FINDINGS.md` — per operation: works via crate / needs CLI /
needs custom FFI; timings; config key names for the R10; wedge behavior notes; and a decoder
recommendation with the measurements behind it. Update the `camera.ops_via_cli` default in
config accordingly, record the chosen decoder in PRD §7 and SDD §5.7 (version bump + change note
per task rules), and update the M1-T09 `SourceFormat` seam if the choice implies a different
shape than assumed.

## Acceptance criteria

- [ ] FINDINGS.md answers works/CLI/FFI for all 8 areas with evidence (log excerpts, timings)
- [ ] Bulb verdict explicit; if crate-bulb fails, CLI bulb (`gphoto2 --bulb`) verified as the fallback
- [ ] Cable-pull behavior documented well enough to design T04's recovery
- [ ] **Decoder chosen**, with the measurement table justifying it and the runner-up named. The half-size decode must fit the PRF-05 story: transient buffers only, ~150–300 MB per decode, no resident growth across 20 consecutive decodes
- [ ] The R10 CR3 fixture is committed (or its provenance recorded if too large for the repo) so later decoder changes can be re-evaluated against the same file
