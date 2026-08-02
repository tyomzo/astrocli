# M3-T08 — Body frame, Layer 1: the mount knows which branch it is on

**Milestone:** M3 (post-T07 addendum) · **Depends on:** M3-T04, M3-T06, M3-T07 · **Crates:** astroctl-hal, astroctl-drivers, astroctl-safety, astroctl-core
**Size:** S · **Status:** todo
**Spec:** ADD ADR-14; SDD §5.4.1 (the rule), §5.4.2 (this task), §5.2.3 (the derivation); PRD MNT-15

## Why this task exists

The altitude limit does not protect the declination axis in one of its two directions, and cannot,
because it is asked a body-frame question in the celestial frame.

`SafeMount::lookahead` predicts one degree of motion by adding a delta to the celestial position and
assuming `Direction::North ⇒ declination increases`. That is true on the normal branch only. The
home pose **is** the pole, so the first step of a northward declination move crosses to the flipped
branch, where the same command decreases declination. From then on the predictor reports a tube
climbing toward the pole while the real one descends — and because obligation 5's descent guard
(`ahead_altitude <= here_altitude`) is then never satisfied, the check does not mis-estimate, it
**permits unconditionally**.

Measured on the operator's HEQ5, 2026-08-02, from the home pose:

| commanded | stopped at | by | tube ended |
|---|---|---|---|
| DEC south sense | 73.35° travel, alt 14.35° | `LIMIT_ALTITUDE` — correct | just under the 15° floor |
| DEC north sense | 180.0° travel | `LIMIT_TRAVEL` — the *cable* limit | **60° below the horizon** |

The two arcs are mirror images and should stop at the same 72.6°. Only M3-T07's travel limit, added
that morning for unrelated reasons, ended the second one. With an OTA fitted this is the 2026-08-01
tripod strike again, from a different direction.

## Scope

Layer 1 of SDD §5.4.2 and nothing beyond it: the mount reports its branch, and the safety wrapper
uses it instead of assuming. No geometry, no collision model — that is §5.4.3 and is deliberately
not in this task.

- **Driver derives and reports the branch.** §5.2.3 already specifies it (`d < 0` implies flipped);
  §5.4 obligation 3 records that no driver performs it, which is why `mount.position.pier_side`
  reports `unknown` on hardware today. Derive it from the declination counter the driver already
  reads. The field exists in the event schema — this makes it truthful, and adds no topic.
- **HAL surface on M3-T07's terms**: synchronous, cached, `Option`, alongside `axis_travel()` rather
  than replacing it. `None` from a driver with no body state, per ADR-02 — an INDI or ASCOM adapter
  has no counters and must remain expressible.
- **`SafeMount` applies the branch to the declination arms of `lookahead` only.** §5.4.2 derives
  that `∂HA/∂h = s` on both branches while `∂dec/∂d` inverts, so the right-ascension arms are
  correct as written and must not be touched. If they turn out to need the branch, §5.4.2's
  derivation is wrong and that is the finding — say so rather than patching both.
- **The fallback is the positional check**, per §5.4.2: a driver reporting no branch cannot be given
  a directional guarantee. Accept and state the limitation in the manner of obligation 3; do not
  invent a heuristic. The observed-sense alternative is recorded in §5.4.2 as considered and not
  chosen.
- The simulator has no counters and no branch. Whatever it reports must not make the wrapper's
  branch path untested — if the simulator cannot exercise it, the driver tests must.

## Acceptance criteria

- [ ] From the home pose, a declination slew in **either** sense is refused at the same travel angle
      (within the look-ahead's one degree), and that angle is the altitude floor's, not the travel
      limit's. This is the whole point of the task and it must be a test, not a hardware note.
- [ ] A declination slew that *unwinds* an axis already below the floor is still permitted, on both
      branches — obligation 5's guarantee must survive the fix rather than be traded for it.
- [ ] `mount.position.pier_side` reports a real side on the skywatcher driver, on both branches, and
      the golden fixture moves with it if the payload does.
- [ ] The right-ascension arms of `lookahead` are unchanged, and a test pins the branch-invariance
      claim (`∂HA/∂h = s` on both branches) so that §5.4.2's derivation is falsifiable in CI rather
      than by reasoning.
- [ ] A driver returning `None` for body state produces the documented positional fallback, and the
      accepted limitation is stated where a reader of the code will meet it.

## Hardware verification (operator, mount bare)

Repeat the measurement that produced this task: from home, drive DEC in the north sense at 8× and
confirm it now refuses at ≈73° with `LIMIT_ALTITUDE` rather than running to 180°. Then the south
sense, which must be unchanged. Then confirm both senses still unwind from below the floor.

Do it with the mount bare, as it was when the defect was found. **Note that the altitude floor is
computed from the configured site**, which is still the example's Oslo — so the angle it stops at is
only correct if the site is. Fix the site first or expect the number to be wrong in a way that has
nothing to do with this task.
