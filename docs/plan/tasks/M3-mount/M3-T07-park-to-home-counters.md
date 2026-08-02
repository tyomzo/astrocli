# M3-T07 — Park must target the home counters, not a sky coordinate

**Milestone:** M3 (post-bring-up correction) · **Depends on:** M3-T04 · **Crates:** astroctl-drivers, astroctl-core (config)
**Size:** S · **Status:** not started · **raised to urgent 2026-08-02** — see the second observation
**Spec:** PRD MNT-07, REL-04; SDD §5.2.3; observed 2026-08-02

## What is wrong

`park` performs a goto to `mount.park_position` — a **sky** coordinate, shipped as
`ra_hours: 0.0, dec_degrees: 90.0`. At declination 90 the right ascension is degenerate: every RA
value describes the same point, the celestial pole. The goto is therefore satisfied by moving the
declination axis alone, and **the RA axis stays wherever it happened to be**.

Observed on hardware: from dec 60 with the RA axis 90° from home, park moved the tube 30° and left
the counterweight shaft horizontal. The tube did point at the pole — the target was met — but the
mount was not in its home pose.

## Why it matters more than it looks

Power-on sets both counters to `0x800000` **regardless of where the metal is**. That is the only
absolute reference an open-loop Synta mount has. So the contract of parking is not "point at the
pole", it is "return to the pose that power-on will assume" — and a park that leaves one axis
unconstrained manufactures precisely the mismatch between belief and reality that cost an evening
on 2026-08-01: counters reading home while the tube is a quarter turn away.

The sky-coordinate formulation cannot express the requirement. Any park target with `dec = 90`
under-constrains RA by construction, and any target with `dec ≠ 90` is not the home pose.

## Scope

- Park drives **both axes to the home counter** (`0x800000`), as a bounded goto per axis — not a
  sky-coordinate goto. This is also the one motion in the driver that needs no coordinate map at
  all, which is a virtue: it stays correct independently of M3-T06's hour-angle correction.
- Decide the fate of `mount.park_position` in PRD §8.1. Options, to be argued rather than assumed:
  drop it (the home pose is a mechanical fact, not an operator preference); keep it for a *second*
  "stow" concept distinct from home; or keep it as an axis-angle pair rather than a sky coordinate.
  Whichever wins, the shipped example must not imply a sky target constrains both axes.
- The parked interlock and `unpark` are unchanged; only where park drives to.
- Watch the interaction with M3-T06: park-to-counters is independent of the sky map, so this task
  must not reintroduce a dependency on it.

## Acceptance criteria

- [ ] From any pose, park leaves **both counters at `0x800000`** — asserted against the mock port
      by the frames sent, not by the reported sky position, which is degenerate at the pole
- [ ] On hardware: park from a position with the RA axis away from home returns the counterweight
      shaft to vertical and the tube to the polar axis — the pose an operator would set by hand
- [ ] A configuration whose park position cannot be expressed is rejected at load rather than
      silently under-constraining an axis


## Second observation, 2026-08-02: 215° of wind, and a park that reported success

Manual slewing wound the RA axis **215.6° from home** — more than half a turn. The operator pressed
Home. The app then reported `parked: true`, `dec 90`, tube at the pole, and the axis stayed where it
was. Every one of those statements was true; together they were useless.

This is the same degeneracy as the first observation, in its worst form: park's sky target was
already satisfied, so nothing moved, and the interlock latched over a mount wound half a turn from
its home pose. Recovery was by hand — power off, loosen the clutch, unwind, power on.

**A second gap, and the more dangerous one: manual slew has no travel limit.** Nothing bounds how
far an axis can be driven from home. On a bare mount that is untidy; with a telescope, a power lead
and a USB cable attached it is how a cable is torn out or a tube is driven into the pier. The mount
itself will not stop you — Synta motion has no soft limits — and the operator holding a D-pad has no
indication of accumulated travel at all.

**So this task grows a sibling requirement**, and both belong to the same fix because they share a
cause (nothing in the system tracks distance from home as a quantity worth bounding):

- Park drives both axes to the home counter (the original scope).
- Manual slew is bounded: refuse — or at minimum warn, redundantly encoded per §5.9 — beyond a
  configured travel from home. The number is a property of the rig's cabling, so it belongs in
  config, and its default should be conservative enough to protect a cabled mount.
- The PWA shows accumulated travel from home while a hold is in progress. An operator cannot
  otherwise know, and the counter is the only thing that does.
