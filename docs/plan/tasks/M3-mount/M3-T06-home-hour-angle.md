# M3-T06 — The home hour angle is +6h: correct the mech↔sky map

**Milestone:** M3 (post-bring-up correction) · **Depends on:** M3-T03, M3-T04 · **Crates:** astroctl-drivers
**Size:** M · **Status:** **done and verified on hardware (2026-08-02)** — the block is lifted
**Spec:** SDD §5.2.3; `spikes/skywatcher-heq5/FINDINGS.md` ("The home hour angle is +6h")

## What is wrong

`math/mech.rs` models `HA = s·h`, making both counters at `0x800000` mean hour angle zero. The
mount's home pose puts the counterweight shaft *along the meridian plane*, so a pure declination
move from home sweeps the tube east–west and can never reach the meridian — which the model says it
does. Measured on hardware from the home pose: a 90° DEC move put the tube **west on the horizon**
where the model claimed south at alt 30°, and a subsequent 90° RA move put it at the **zenith**,
which the corrected model predicts exactly.

**`HA_true = HA_model + 90°`.**

## Why no test caught it

Every fixture in the tree derives its expectations from this same model, and the mount's counters
mean whatever the model says they mean — so the error is invisible to any comparison the software
can make with itself. It was found by an operator looking at the mount. That is the argument for
the acceptance criterion below being a *physical* one.

## Scope

- Correct the constant in `mech.rs`'s `mech_to_sky` / `sky_to_mech` pair, both branches. Derive the
  `ThroughThePole` case rather than assuming it is the same offset — the flipped branch already
  carries a 180° term and the two must compose correctly.
- Rework the fixtures. The astropy-derived cases in `math` are still valid *as sky positions*; what
  changes is which axis angles produce them. Re-derive rather than nudging expected values until
  they pass — a fixture edited to match a bug is how the bug survives.
- Check every consumer: `goto_solution` (its no-flip property must still hold), the altitude limit
  in `astroctl-safety` (it computes from RA), pier-side derivation, and the simulator — whose model
  agrees with the driver's by construction and must move with it, or the two will disagree about
  the same sky.
- The simulator is the second half of the risk: it is `SimulatorMount`'s decomposition too, and
  M1-T15's predictive display asserts against its constants.

## Acceptance criteria

- [x] From the home pose on real hardware: a 90° DEC move reports the tube **west on the horizon**,
      and a following 90° RA move reports the **zenith**. Both were measured 2026-08-01 and are the
      ground truth this task is judged against
      — `math::mech::tests::the_two_swings_measured_from_the_home_pose` (axis angles, with the
      alt/az the operator saw, from an independent spherical triangle) and
      `tests/position_math.rs::t_pos_6_the_home_pose_swings_measured_on_the_mount` (the same two
      poses as 24-bit counters, through the SDD §5.2.3 seam). The recorded measurement is the
      evidence; the mount was parked and powered off for this work
- [ ] A goto to a known bright star puts it in the frame (once M2's camera is on the mount) — the
      first end-to-end check that RA is honest
      — **still open, and needs the mount.** This is the criterion that would close the remaining
      unknown below
- [x] The altitude limit refuses and permits the correct targets: a target genuinely below the
      horizon is refused, one genuinely above it is not
      — no code changed in `astroctl-safety`: it computes from a `RaDec` and was always right
      about the arithmetic. It was being handed a right ascension six hours out on the three paths
      that judge the *current* position (the manual-slew pre-flight, the 2 Hz limit watch, the
      meridian watch) and on the operator's alt/az readout. The goto pre-flight check was never
      wrong about the target, only about whether the mount would go there
- [x] Simulator and driver agree: the same commanded axis angles produce the same sky coordinates
      in both, asserted by a shared fixture
      — `the_driver_and_the_simulator_agree::the_same_axis_state_is_the_same_sky_in_both`. Note
      what the fixture had to be: the simulator holds the hour angle and declination *directly*
      and has no home counter, so agreement is asserted as the correspondence
      `simulator hour-angle axis = s·(h + 90°)` rather than as a shared constant. The simulator
      needed no arithmetic change and **could not have caught this defect** — see below

## What this could not settle

- **Nothing here was verified against the mount**, which was parked and powered off. The two
  swings of 2026-08-01 are the evidence, and they are an eye at an eyepiece rather than an
  instrument: "due west on the horizon" and "the zenith" are unmistakable to within a degree or
  two, which is ample for a six-hour error and no use at all for a small one. A goto to a named
  star, framed on the camera, is what turns this from "not six hours wrong" into a pointing
  measurement.
- **The southern-hemisphere sign is derived, not observed.** The mount is in Norway. The offset
  carries the hemisphere sign — `HA = s·(h + 90°)`, not `HA = s·h + 90°` — and that follows from
  the same `A = s·P` that already signs the hour-angle term and the declination. It is asserted
  (`the_home_hour_angle_is_six_hours_and_carries_the_hemisphere_sign`) but only a mount below the
  equator can confirm it.
- **The pier-side label is still `derived` and unverified**, exactly as it was before this task.
  M3-T06 did not touch it and does not close it; `math::mech`'s module docs still carry the one
  experiment that would.



## Why this is blocking

On 2026-08-02, with the error already documented, a sky-coordinate goto was computed *through the
broken map* to "zero the axes" and **drove the tube into the tripod**. The reasoning that the
offset cancels for an axis-zeroing move is wrong: the target was a sky coordinate derived from a
position the broken map reported, so the fault propagated into every term.

Until this task lands and the physical swing test reproduces, motion on real hardware is
axis-relative only — manual slews at rates the rotor follows, and M3-T07's park-to-home-counters,
which consults no map at all.


## Verified on the mount, 2026-08-02

An eight-step protocol proposed by the operator — each axis swung 30° both ways, returning home
between — run one command at a time with the operator confirming by eye before the next. Every step
matched the corrected model:

| step | commanded | model predicted | observed |
|------|-----------|-----------------|----------|
| 1 | DEC axis 30° (west branch) | north-west, 49° | confirmed |
| 2 | back to the pole | north, 60° | confirmed |
| 3 | DEC axis 30° (through-the-pole branch) | north-east, 49° | confirmed, shaft never moved |
| 4 | back to the pole | north, 60° | confirmed |
| 5 | out to the reference pose | north-west, 49° | confirmed |
| 6 | RA axis +30° (HA +2h) | north-north-west, 39°, shaft 30° off vertical | confirmed |
| 7 | back to the reference | north-west, 49°, shaft vertical | confirmed |
| 8 | RA axis −30° (HA −2h) | west-north-west, 61°, shaft 30° the other way | confirmed |

The decisive one is step 1: the old map sent that same command *south*, and the tube went *west*.
Step 3 additionally verifies the pier-side branch — the same declination reached on the opposite
side of the pole by the declination axis alone, with the counterweight shaft never moving, so the
driver chose the axis motion rather than a twelve-hour coordinate detour.

**A 30° protocol was chosen over the 90° swings that found the bug, deliberately.** Thirty degrees
discriminates the six-hour error exactly as well — the predicted azimuths differ by far more than
eyesight's resolution — while sweeping a third of the arc, and the collision that preceded this fix
happened during a 90° move. Smaller motions that answer the same question are the better
instrument.

Still unobserved: the southern-hemisphere sign (derived, no access to a southern mount), and the
pier-side *label* — which branch is physically east and which west — which remains `derived` and
needs a known star, not a bare mount.


## The 90° protocol, and the case that needed no motion

Repeated at 90° on the operator's request. RA axis both directions with returns between, each step
confirmed by eye before the next:

| commanded | model predicted | observed |
|-----------|-----------------|----------|
| RA axis +90° (HA +6h → +12h) | due north, 30° up; shaft horizontal | confirmed |
| back | north-west, 49°; shaft vertical | confirmed |
| RA axis −90° (HA +6h → 0h) | the zenith | confirmed, ~2° off |
| back, then DEC home | the home pose | confirmed |

**The 2° at the zenith is polar alignment, not the map.** The tube's real altitude is set by how the
mount's polar axis is physically tilted; the model computes from the *configured* latitude, still
Oslo's 59.9139° while the mount stands in Lithuania. Errors in this map arrive in units of 90°, not
2°. It is a clean illustration of the open-loop limit: the software cannot know the alignment, only
what it was told — which is what plate solving in Phase 2a exists to fix.

**The DEC 90° case was verified without moving the mount.** That swing had already been performed
earlier the same night, from home, and the tube went *west onto the horizon* — the ground truth the
whole correction was derived from. The only open question was whether the corrected model agrees, so
the command was re-issued and the altitude limit answered: **`LIMIT_ALTITUDE`, "the target is at
altitude −0.0°"**. The old model called the identical command *south at 30°*. A refusal is not
motion, so this cost nothing and risked nothing — the cheapest verification of the session, and a
reminder that a safety refusal carrying a computed number is itself an instrument.
