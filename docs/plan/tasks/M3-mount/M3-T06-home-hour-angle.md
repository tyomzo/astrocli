# M3-T06 — The home hour angle is +6h: correct the mech↔sky map

**Milestone:** M3 (post-bring-up correction) · **Depends on:** M3-T03, M3-T04 · **Crates:** astroctl-drivers
**Size:** M · **Status:** not started · **BLOCKING: no sky-coordinate goto on hardware until this lands and is re-verified**
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

- [ ] From the home pose on real hardware: a 90° DEC move reports the tube **west on the horizon**,
      and a following 90° RA move reports the **zenith**. Both were measured 2026-08-01 and are the
      ground truth this task is judged against
- [ ] A goto to a known bright star puts it in the frame (once M2's camera is on the mount) — the
      first end-to-end check that RA is honest
- [ ] The altitude limit refuses and permits the correct targets: a target genuinely below the
      horizon is refused, one genuinely above it is not
- [ ] Simulator and driver agree: the same commanded axis angles produce the same sky coordinates
      in both, asserted by a shared fixture


## Why this is blocking

On 2026-08-02, with the error already documented, a sky-coordinate goto was computed *through the
broken map* to "zero the axes" and **drove the tube into the tripod**. The reasoning that the
offset cancels for an axis-zeroing move is wrong: the target was a sky coordinate derived from a
position the broken map reported, so the fault propagated into every term.

Until this task lands and the physical swing test reproduces, motion on real hardware is
axis-relative only — manual slews at rates the rotor follows, and M3-T07's park-to-home-counters,
which consults no map at all.
