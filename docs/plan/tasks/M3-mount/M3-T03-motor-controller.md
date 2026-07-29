# M3-T03 — Motor controller + position math

**Milestone:** M3 · **Depends on:** M3-T01 · **Crates:** astroctl-drivers
**Size:** L · **Status:** not started
**Spec:** SDD §5.2.3 (position math, goto, pier side); PRD §4.2 parameters
**Tests gated:** T-POS-1

## Objective

Per-axis motion semantics and the counts↔coordinates math, with pier-side handling — pure
logic over the codec, testable without hardware.

## Scope

- `MotorController` per axis: init sequence (version, CPR, timer freq reads → stored params), motion-mode selection, speed computation (step period from timer freq + rate), start/stop/instant-stop, goto target set + start, status decode
- Position math per SDD §5.2.3: counts↔hours/degrees with CPR from handshake (never hardcoded 9,024,000 — that's a test fixture value only), hemisphere handling from site config latitude sign
- `mech_to_sky`/`sky_to_mech` behind the documented seam (LST parameter injected; simple LST-from-clock provider for now, erfa provider in Phase 2a)
- Pier side derivation from DEC counts; goto target selection choosing pier side (nearest valid, no flip-through-pole)
- Tracking-rate step periods for sidereal/lunar/solar from timer frequency
- Guide pulse: rate set + timed offset per SDD

## Acceptance criteria

- [ ] T-POS-1: property round-trips within 1 count; table-driven hemisphere/pier cases (N/S latitude × E/W pier × DEC signs); golden goto-target cases hand-computed
- [ ] Speed math: step periods for all rates match hand-computed values for the **verified** fixture constants — CPR 9,024,000 and timer frequency **64,935 Hz** (PRD §4.2). Do not hand-compute against the old 460,800 figure; it was wrong by 7.1× and any expected value derived from it is invalid
- [ ] No `f64` position leaves the module without going through the typed newtypes
