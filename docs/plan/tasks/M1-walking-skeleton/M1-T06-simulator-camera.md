# M1-T06 — SimulatorCamera: synthetic star fields

**Milestone:** M1 · **Track:** A · **Depends on:** M1-T01 · **Crates:** astroctl-drivers
**Spec:** PRD §4.5 (SimulatorCamera), HAL-11; SDD §9

## Objective

A `Camera` implementation producing plausible synthetic frames with realistic capture timing,
so every downstream stage (store, preview, transfer, stacking later) handles real-shaped data.

## Scope

- Synthetic frame generation: star field from a small bundled pseudo-catalog seeded by the *simulated mount position* (accepts an optional position provider — same generator later feeds SimulatorGuideCamera); configurable star density, FWHM (gaussian PSF), sky background level, gaussian+poisson noise; exposure time scales signal
- Output format: 16-bit FITS internally + an 8-bit JPEG rendition (the pair mimics RAW+preview); frame dimensions/config per PRD equipment profile (6000×4000 default, configurable smaller for fast tests)
- Timing realism: capture blocks for the exposure duration, then a configurable "download" delay; bulb honors duration; abort works mid-exposure
- Settings plumbing: ISO/shutter/format lists via `get_available_settings`, current settings respected by the generator (ISO → gain/noise)
- `FaultPlan` hooks: `FailCapture(n)`, `SlowDownload(x)`, `DisconnectAfter(d)`
- Live view: JPEG stream at configurable fps from the same generator at reduced size

## Acceptance criteria

- [ ] Frames visibly contain stars; mean background and star count respond to config (assert statistically, not pixel-exact)
- [ ] 2 s exposure takes ≈2 s + download delay; abort mid-exposure returns promptly with a distinct error
- [ ] FITS opens in a standard tool (verify with `fitsio` read-back test); JPEG decodes
- [ ] Registry name `"simulator"`, feature-gated with T02
