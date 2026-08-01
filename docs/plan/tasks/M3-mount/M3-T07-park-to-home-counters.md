# M3-T07 — Park must target the home counters, not a sky coordinate

**Milestone:** M3 (post-bring-up correction) · **Depends on:** M3-T04 · **Crates:** astroctl-drivers, astroctl-core (config)
**Size:** S · **Status:** not started
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
