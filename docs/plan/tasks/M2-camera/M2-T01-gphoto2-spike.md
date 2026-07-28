# M2-T01 — Spike: bulb + CR3 download via gphoto2 crate

**Milestone:** M2 · **Depends on:** M1 · **Crates:** scratch (`spikes/gphoto2-r10/`, not in workspace)
**Spec:** ADD §10 (top risk), SDD §5.3.3 (fallback table this spike populates); PRD §4.3

## Objective

**Report task, not production code.** Determine, on the real R10, which operations the
`gphoto2` crate covers and which need the CLI fallback — the go/no-go evidence for the M2
driver design.

## Scope

Standalone binary exercising, in order, with timings logged:
1. Autodetect + connect; read config tree (dump all keys — this is the settings map source)
2. Get/set ISO, shutter, aperture, image format; enumerate choices
3. Timed capture (1/30 s) + CR3 download; verify file opens with libraw
4. **Bulb**: attempt via PTP remote-release config (`eosremoterelease`/equivalent) — 10 s exposure; the critical unknown
5. Live view: preview frame fetch rate over 30 s; measure fps + latency
6. Battery + storage reads
7. Repeat 3 with USB cable pulled mid-download: characterize the failure mode and recovery (context reinit? power cycle needed?)

Deliverable: `spikes/gphoto2-r10/FINDINGS.md` — per operation: works via crate / needs CLI /
needs custom FFI; timings; config key names for the R10; wedge behavior notes. Update the
`camera.ops_via_cli` default in config accordingly, and the SDD §5.3 if reality deviates
(version bump + change note per task rules).

## Acceptance criteria

- [ ] FINDINGS.md answers works/CLI/FFI for all 7 areas with evidence (log excerpts, timings)
- [ ] Bulb verdict explicit; if crate-bulb fails, CLI bulb (`gphoto2 --bulb`) verified as the fallback
- [ ] Cable-pull behavior documented well enough to design T04's recovery
