# M3-T06 — The home hour angle is +6h: correct the mech↔sky map

**Milestone:** M3 (post-bring-up correction) · **Depends on:** M3-T03, M3-T04 · **Crates:** astroctl-drivers
**Size:** M · **Status:** code done (2026-08-02) — **still BLOCKING for hardware gotos until the physical swing test is re-run**
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
