# M2-T01 — Spike: bulb + CR3 download via gphoto2 crate, and RAW decoder selection

**Milestone:** M2 · **Depends on:** M1 · **Crates:** scratch (`spikes/gphoto2-r10/`, not in workspace)
**Size:** M · **Status:** **done** (2026-07-29, all 8 steps) — findings in `spikes/gphoto2-r10/FINDINGS.md`
**Spec:** ADD §10 (top risk), SDD §5.3.3 (fallback table this spike populates), §5.7 (decoder seam); PRD §4.3, §7 (RAW-decoder candidates and their maturity)

> **Run early and complete.** The camera became available before M0 started, so this spike was
> executed out of sequence — which is entirely the point of a go/no-go spike. All eight steps are
> done and the top risk is retired. Step 7 additionally uncovered that gvfs breaks REL-03 recovery
> on hotplug, which is now a precondition recorded against M2-T02 and M2-T04.

## Objective

**Report task, not production code.** Two go/no-go questions, both answerable only with the
camera on the desk and a real CR3 in hand:

1. Which operations the `gphoto2` crate covers and which need the CLI fallback — evidence for
   the M2 driver design.
2. **Whether `rawler` holds up on real R10 data.** The *selection* is already made on build
   evidence (PRD §7, `docs/evidence/dependency-survey-2026-07-29.md`): rawler is pure Rust, needs
   no system library, ships a CR3/CRX decoder, an R10 camera profile, and R10 regression fixtures.
   What that evidence cannot tell you is decode *speed* and *peak memory* on the hardware this
   runs on — and PRF-05 lives or dies on the second one. Confirm, or produce the evidence to
   overturn the choice.

## Scope

Standalone binary exercising, in order, with timings logged:
1. Autodetect + connect; read config tree (dump all keys — this is the settings map source)
2. Get/set ISO, shutter, aperture, image format; enumerate choices
3. Timed capture (1/30 s) + CR3 download; verify the file is a well-formed CR3 (keep it — it is the fixture for step 8)
4. **Bulb**: attempt via PTP remote-release config (`eosremoterelease`/equivalent) — 10 s exposure; the critical unknown
5. Live view: preview frame fetch rate over 30 s; measure fps + latency
6. Battery + storage reads
7. Repeat 3 with USB cable pulled mid-download: characterize the failure mode and recovery (context reinit? power cycle needed?)
8. **`rawler` validation** against the CR3 from step 3: does it open a real R10 file; wall-clock
   and **peak RSS** for a half-size decode (the §5.7 preview path wants speed, not full
   resolution); does the Bayer pattern and the black/white levels it reports match the `r10.toml`
   profile; does RSS stay flat across 20 consecutive decodes. Run it on the *field node* if the Pi
   is the target — a decode that is comfortable on a workstation may not be on ARM. Only if it
   fails here does the bake-off reopen, and `libraw` (system `libraw_r`, bindings at 0.1.1) is
   then the sole remaining candidate, since `rawloader` has no CR3 support at all

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
- [ ] **`rawler` confirmed on real data** — decodes the R10 CR3, Bayer pattern and levels match `r10.toml`, half-size decode timed, peak RSS recorded, and no resident growth across 20 consecutive decodes (the PRF-05 condition). A failure here is a finding that reopens PRD §7, not something to work around quietly
- [ ] The R10 CR3 fixture is committed (or its provenance recorded if too large for the repo) so later decoder changes can be re-evaluated against the same file
