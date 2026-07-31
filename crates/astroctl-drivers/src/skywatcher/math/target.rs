//! Choosing where to send the mount: the two mechanical solutions, and why exactly one of them
//! is allowed.
//!
//! # The protocol has no absolute target, so this is where absolute targeting lives
//!
//! `H` is a **relative increment** — SDD §5.2.2 used to list an `S` opcode and
//! `spikes/skywatcher-heq5/ENCODINGS.md` established there is no such thing. M3-T01 made the
//! absence structural: no codec type takes an absolute target, so the arithmetic that turns a
//! sky coordinate into a counter has to end at a [`Move`], and this module is the end of it.
//!
//! # Nearest-valid, and what makes a solution invalid
//!
//! Every direction is reachable two ways ([`super::mech`]). Both are "correct" as coordinates and
//! only one is safe to *drive to*, because the declination axis reaches the other branch by
//! turning through zero — the pole — which on a German equatorial is the tube swinging over the
//! pier. So:
//!
//! > A goto never changes branch. The solution on the mount's current branch is the only
//! > candidate, and its axis moves take the shorter way round.
//!
//! That is a total rule, not a heuristic, and it is what makes the property testable: because
//! *both* branches cover the whole sky, the same-branch solution always exists, so refusing the
//! other one never refuses a target. The one state with a genuine choice is the pole itself
//! (`dec axis = 0`, i.e. home), where the mount is on neither side; there, and only there, this
//! picks by travel.
//!
//! A meridian flip is therefore not something goto selection does by accident. It is a separate,
//! deliberate operation — SDD §5.4's meridian limit decides when, M3-T04 sequences it — and it
//! has to be, because the flip is a large motion through the mount's own pier and needs the
//! limits consulted first.
//!
//! # What this does not decide
//!
//! Axis limits. The right-ascension axis can be commanded a full turn from home and an HEQ5's
//! cabling cannot follow it; [`GotoSolution::destination`] hands back the mechanical angles the
//! move ends at so the safety layer can refuse one, rather than this module inventing a limit
//! that belongs in configuration (SDD §5.4).

use astroctl_core::types::{PierSide, RaDec};

use crate::skywatcher::codec::{Counts, Move};

use super::angle::{AxisScale, Lst};
use super::mech::{sky_to_mech, Branch, Hemisphere, MechPosition};
use super::MathError;

/// Both axis counters at one instant.
///
/// A pair rather than two arguments because they are the same type and the two axes are the one
/// transposition this arithmetic cannot detect: swapping them produces a plausible coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AxisCounts {
    /// The right-ascension axis counter, as `:j1` reports it.
    pub ra: Counts,
    /// The declination axis counter, as `:j2` reports it.
    pub dec: Counts,
}

impl AxisCounts {
    /// Both axes at `0x800000` — what the operator's mount read at power-on.
    pub const HOME: Self = Self {
        ra: Counts::HOME,
        dec: Counts::HOME,
    };
}

/// A goto, resolved: which way each axis turns and how far, and which pier side it lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GotoSolution {
    ra: Move,
    dec: Move,
    from: AxisCounts,
    to: AxisCounts,
    branch: Branch,
}

impl GotoSolution {
    /// The right-ascension axis move — magnitude for `H`, direction for `G`.
    #[must_use]
    pub const fn ra(&self) -> Move {
        self.ra
    }

    /// The declination axis move.
    #[must_use]
    pub const fn dec(&self) -> Move {
        self.dec
    }

    /// The counters this was computed against. Stale values invalidate the whole thing — the
    /// readback expectations are absolute and are derived from these (M3-T01's `GotoProgram`).
    #[must_use]
    pub const fn from(&self) -> AxisCounts {
        self.from
    }

    /// The counters the mount should end at.
    #[must_use]
    pub const fn destination(&self) -> AxisCounts {
        self.to
    }

    /// The mechanical branch the move stays on.
    #[must_use]
    pub const fn branch(&self) -> Branch {
        self.branch
    }

    /// The pier side the mount will be on afterwards — which is the one it is on now.
    #[must_use]
    pub const fn pier_side(&self) -> PierSide {
        self.branch.pier_side()
    }

    /// The larger of the two axis moves, in counts.
    ///
    /// The right measure of "how far": the axes move together and a goto is over when the slower
    /// one arrives. **Not** a duration — SDD §5.2.3 forbids estimating slew time from distance,
    /// because the mount ramps and short moves never reach cruise.
    #[must_use]
    pub fn travel_counts(&self) -> u32 {
        self.ra.magnitude().get().max(self.dec.magnitude().get())
    }
}

/// Resolve a target into two axis moves, on the branch the mount is already on.
///
/// `lst` is injected — see [`Lst`] for why this module does not compute it. `scale` supplies the
/// counts per revolution each axis reported at handshake; they are separate arguments because the
/// mount answers `:a` per axis and nothing guarantees the two agree.
///
/// # Errors
/// [`MathError::Move`] if an axis delta will not fit the 24-bit increment register. Unreachable
/// for any counts-per-revolution the register itself can hold — the shortest path is bounded by
/// half a revolution — and kept as a `Result` because that bound is an argument rather than a
/// type, and an argument should be checkable.
pub fn goto_solution(
    from: AxisCounts,
    target: RaDec,
    lst: Lst,
    hemisphere: Hemisphere,
    ra_scale: AxisScale,
    dec_scale: AxisScale,
) -> Result<GotoSolution, MathError> {
    let current = MechPosition {
        ra_axis: ra_scale.angle_at(from.ra),
        dec_axis: dec_scale.angle_at(from.dec),
    };

    // At the pole the mount is on neither side, so both branches are legal and the choice is by
    // travel. Everywhere else there is exactly one candidate and the "choice" is a formality —
    // which is the point: a rule with one option cannot be got wrong under pressure.
    let candidates: &[Branch] = if current.dec_axis.degrees() == 0.0 {
        &[Branch::Normal, Branch::ThroughThePole]
    } else {
        match current.branch() {
            Branch::Normal => &[Branch::Normal],
            Branch::ThroughThePole => &[Branch::ThroughThePole],
        }
    };

    // At least one candidate always exists, so there is no "no solution" case to invent an error
    // for — which is the whole argument of this module restated as control flow.
    let (first, rest) = candidates.split_first().unwrap_or((&Branch::Normal, &[]));
    let mut best = solve(from, target, lst, hemisphere, ra_scale, dec_scale, *first)?;
    for &branch in rest {
        let other = solve(from, target, lst, hemisphere, ra_scale, dec_scale, branch)?;
        if (other.travel_counts(), other.ra.magnitude().get())
            < (best.travel_counts(), best.ra.magnitude().get())
        {
            best = other;
        }
    }
    Ok(best)
}

/// The solution on one named branch.
fn solve(
    from: AxisCounts,
    target: RaDec,
    lst: Lst,
    hemisphere: Hemisphere,
    ra_scale: AxisScale,
    dec_scale: AxisScale,
    branch: Branch,
) -> Result<GotoSolution, MathError> {
    let wanted = sky_to_mech(target, branch, lst, hemisphere);
    let to = AxisCounts {
        ra: ra_scale.counts_at(wanted.ra_axis),
        dec: dec_scale.counts_at(wanted.dec_axis),
    };
    // `reachable_delta` and not `shortest_delta`: the counter wraps at 2²⁴ and the mechanism at
    // CPR, so the geometrically shortest move can land on a counter that decodes 50.7° wrong.
    // The declination axis never needs the adjustment — the branch rule above confines both
    // endpoints to one half of the circle — and asserting that is cheaper than reasoning about
    // it, which `the_declination_axis_never_needs_the_wrap_adjustment` does.
    let ra = Move::from_delta(ra_scale.reachable_delta(from.ra, to.ra)?)?;
    let dec = Move::from_delta(dec_scale.reachable_delta(from.dec, to.dec)?)?;

    // `counts_at` canonicalises to the counter nearest home and the delta folds modulo the
    // revolution, so the counter the mount actually lands on is `from + delta`, which need not be
    // the canonical one. Recording the arrival rather than the canonical target is what lets a
    // caller compare it against `:j` after the move without a fold of its own.
    Ok(GotoSolution {
        ra,
        dec,
        from,
        to: AxisCounts {
            ra: from.ra.after(ra),
            dec: from.dec.after(dec),
        },
        branch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::hex::U24;
    use crate::skywatcher::codec::CountsPerRev;
    use crate::skywatcher::math::mech::mech_to_sky;

    /// The operator's HEQ5 answered `:a` with this on both axes. **A fixture.**
    const FIXTURE_CPR: u32 = 9_024_000;

    fn scale() -> AxisScale {
        AxisScale::new(CountsPerRev(U24::new(FIXTURE_CPR).expect("fits"))).expect("non-zero")
    }

    fn lst(hours: f64) -> Lst {
        Lst::from_hours(hours).expect("valid")
    }

    fn radec(ra_hours: f64, dec_degrees: f64) -> RaDec {
        RaDec::from_parts(ra_hours, dec_degrees).expect("valid")
    }

    fn counts_at(degrees: f64) -> Counts {
        scale().counts_at(crate::skywatcher::math::AxisAngle::from_degrees(degrees).expect("valid"))
    }

    fn solve_from(
        from: AxisCounts,
        target: RaDec,
        sidereal: Lst,
        hemisphere: Hemisphere,
    ) -> GotoSolution {
        goto_solution(from, target, sidereal, hemisphere, scale(), scale())
            .expect("a delta bounded by half a revolution fits the register")
    }

    /// A cheap deterministic stream — a property test without a proptest dependency, which this
    /// crate does not have and which would be a workspace decision rather than a task one.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f64 {
            // Numerical Recipes' 64-bit LCG. Any full-period generator does; the value here is
            // that the sequence is fixed, so a failure is reproducible from the seed alone.
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1_u64 << 53) as f64
        }

        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + self.next() * (high - low)
        }
    }

    #[test]
    fn a_solved_goto_lands_on_the_coordinate_it_was_given() {
        // The end-to-end statement: counters in, a target in, and the counters the move ends at
        // decode back to the target. This is the criterion T-POS-1 calls the round trip, at the
        // layer a goto is actually issued from.
        let sidereal = lst(3.75);
        let mut worst_counts: u32 = 0;
        for hemisphere in [Hemisphere::Northern, Hemisphere::Southern] {
            for start_dec_axis in [-120.0, -45.0, -0.5, 0.5, 45.0, 120.0] {
                let from = AxisCounts {
                    ra: counts_at(20.0),
                    dec: counts_at(start_dec_axis),
                };
                for ra_hours in [0.0, 4.5, 9.0, 15.5, 21.0, 23.9] {
                    for dec_degrees in [-80.0, -40.0, 0.0, 40.0, 80.0] {
                        let target = radec(ra_hours, dec_degrees);
                        let solution = solve_from(from, target, sidereal, hemisphere);
                        let landed = mech_to_sky(
                            MechPosition {
                                ra_axis: scale().angle_at(solution.destination().ra),
                                dec_axis: scale().angle_at(solution.destination().dec),
                            },
                            sidereal,
                            hemisphere,
                        );
                        let gap_ra = (landed.coords.ra.hours() - ra_hours).abs();
                        let gap_ra = gap_ra.min(24.0 - gap_ra) * 15.0;
                        let gap_dec = (landed.coords.dec.degrees() - dec_degrees).abs();
                        let counts = scale().counts_in(gap_ra.max(gap_dec)).round().abs() as u32;
                        worst_counts = worst_counts.max(counts);
                    }
                }
            }
        }
        // One count is the acceptance criterion; the rounding in `counts_at` is the only source
        // of error and it is half a count.
        assert!(
            worst_counts <= 1,
            "worst arrival was {worst_counts} counts off"
        );
    }

    #[test]
    fn no_computed_goto_path_crosses_the_pole() {
        // The acceptance property, stated as the mechanics state it: the declination axis
        // sweeps from where it is to where it is going, and zero — the pole — must not be
        // strictly inside that sweep. Ten thousand random (start, target) pairs in both
        // hemispheres.
        let mut rng = Lcg(0x5157_4348_4551_3500); // "SWQHEQ5"
        for hemisphere in [Hemisphere::Northern, Hemisphere::Southern] {
            for _ in 0..5_000 {
                let start_dec_axis = rng.between(-179.9, 179.9);
                let from = AxisCounts {
                    ra: counts_at(rng.between(-180.0, 180.0)),
                    dec: counts_at(start_dec_axis),
                };
                let target = radec(rng.between(0.0, 24.0), rng.between(-90.0, 90.0));
                let sidereal = lst(rng.between(0.0, 24.0));
                let solution = solve_from(from, target, sidereal, hemisphere);

                let start = scale().angle_at(from.dec).degrees();
                if start == 0.0 {
                    continue; // at the pole there is no side to stay on
                }
                // The path the axis sweeps, *unwrapped*: it runs monotonically from `start` to
                // `start + delta`, and the pole is the value 0. Testing the unwrapped interval
                // rather than the two folded endpoints is the point — a sweep from 170° to 190°
                // ends at a folded −170°, which looks like a sign change and is not one. It
                // crosses 180°, the anti-pole, which no tube passes through either but which is
                // not what this property is about.
                let swept = f64::from(solution.dec().delta()) * scale().degrees_per_count();
                let end = start + swept;
                let (low, high) = (start.min(end), start.max(end));
                assert!(
                    !(low < -1e-9 && high > 1e-9),
                    "a goto swept the declination axis from {start}° to {end}°, through the pole"
                );
                // The move is the shortest path, so it is at most half a turn: a sweep that
                // reached the pole the long way round would show up here first.
                assert!(
                    swept.abs() <= 180.0 + 1e-9,
                    "the declination move swept {swept}°, which is the long way round"
                );
                // The pier side is preserved, which is the same statement in the operator's
                // vocabulary.
                assert_eq!(
                    solution.pier_side(),
                    MechPosition {
                        ra_axis: scale().angle_at(from.ra),
                        dec_axis: scale().angle_at(from.dec),
                    }
                    .pier_side()
                );
            }
        }
    }

    #[test]
    fn a_goto_takes_the_short_way_round_unless_the_counter_forbids_it() {
        // "Nearest" in the only sense the mechanism has — no axis move exceeds half a revolution
        // — with the single exception the 24-bit counter forces. A move that took the long way
        // for no reason would be minutes of slew to the same coordinate; one that took the short
        // way across the counter's wrap would arrive somewhere 50.7° from where it was sent.
        let mut rng = Lcg(0x0000_5041_5448); // "PATH"
        let half = FIXTURE_CPR / 2;
        let mut long_way_taken = 0_u32;
        for _ in 0..2_000 {
            let from = AxisCounts {
                ra: counts_at(rng.between(-180.0, 180.0)),
                dec: counts_at(rng.between(-179.9, 179.9)),
            };
            let solution = solve_from(
                from,
                radec(rng.between(0.0, 24.0), rng.between(-90.0, 90.0)),
                lst(rng.between(0.0, 24.0)),
                Hemisphere::Northern,
            );
            assert!(
                solution.dec().magnitude().get() <= half,
                "the declination axis is confined to one half of the circle and can never need \
                 the wrap adjustment"
            );
            if solution.ra().magnitude().get() > half {
                long_way_taken += 1;
                let short = scale().shortest_delta(from.ra, solution.destination().ra);
                let would_land = i64::from(from.ra.get()) + short;
                assert!(
                    !(0..0x0100_0000).contains(&would_land),
                    "the long way was taken where the short way was safe"
                );
                assert!(solution.ra().magnitude().get() <= FIXTURE_CPR);
            }
        }
        assert!(
            long_way_taken > 0,
            "no sample reached the counter's boundary, so this test proved nothing about it"
        );
    }

    #[test]
    fn from_home_the_nearer_pier_side_is_chosen() {
        // Home is the one state with a real choice — the tube is on the pole, so neither branch
        // has been entered and neither is a flip. Two targets, worked out by hand at LST 12h
        // (= 180°), both at declination +60° so the declination move is 30° on either branch:
        //
        //   RA 11h  → HA = +15°.  Normal RA axis  15°; flipped 195° → −165°.  Normal wins.
        //   RA 0.667h → HA = +170°. Normal RA axis 170°; flipped 350° →  −10°.  Flipped wins.
        //
        // A rule that always answered "normal" would pass the first and fail the second, which
        // is the whole reason the second is here.
        let sidereal = lst(12.0);
        let near_the_meridian = radec(11.0, 60.0);
        let far_west = radec(2.0 / 3.0, 60.0);

        let normal = solve_from(
            AxisCounts::HOME,
            near_the_meridian,
            sidereal,
            Hemisphere::Northern,
        );
        let flipped = solve_from(AxisCounts::HOME, far_west, sidereal, Hemisphere::Northern);
        assert_eq!(normal.branch(), Branch::Normal);
        assert_eq!(flipped.branch(), Branch::ThroughThePole);

        // ...and each is genuinely the shorter of the two, which is what "nearest" has to mean.
        for (solution, target) in [(normal, near_the_meridian), (flipped, far_west)] {
            let rejected = solve(
                AxisCounts::HOME,
                target,
                sidereal,
                Hemisphere::Northern,
                scale(),
                scale(),
                solution.branch().opposite(),
            )
            .expect("both branches are solvable");
            assert!(
                solution.travel_counts() < rejected.travel_counts(),
                "chose {} counts of travel over {}",
                solution.travel_counts(),
                rejected.travel_counts()
            );
        }
    }

    #[test]
    fn away_from_home_the_branch_is_forced_however_much_further_it_is() {
        // The counterpart, and the safety property in the vocabulary of cost: once the mount is
        // on a side, a target that would be far nearer on the other side is *still* reached the
        // long way round. That is not a missed optimisation — the short way is a swing through
        // the pole, and deciding to take it is the meridian flip, which SDD §5.4 owns.
        let sidereal = lst(12.0);
        let far_west = radec(2.0 / 3.0, 60.0); // flipped is 30°, normal is 170°
        let on_the_normal_side = AxisCounts {
            ra: Counts::HOME,
            dec: counts_at(30.0),
        };
        let solution = solve_from(on_the_normal_side, far_west, sidereal, Hemisphere::Northern);
        assert_eq!(
            solution.branch(),
            Branch::Normal,
            "the branch it started on"
        );

        let cheaper = solve(
            on_the_normal_side,
            far_west,
            sidereal,
            Hemisphere::Northern,
            scale(),
            scale(),
            Branch::ThroughThePole,
        )
        .expect("valid");
        assert!(
            cheaper.travel_counts() < solution.travel_counts(),
            "this test is vacuous unless the rejected branch really is nearer"
        );
    }

    #[test]
    fn the_hand_computed_golden_case_lands_on_the_counter_it_should() {
        // A case worked out by hand, so a sign error anywhere in the chain has to survive
        // arithmetic done outside the code to get through.
        //
        // Northern hemisphere, LST 6h. Target RA 6h, dec +40°. HA = 6h − 6h = 0 → the meridian.
        // Normal branch: dec axis = 90 − 40 = 50°, RA axis = 0°.
        // 50° of 9,024,000 counts is 9,024,000 × 50/360 = 1,253,333.33 → 1,253,333 counts, so the
        // declination counter is 0x800000 + 1,253,333 = 8,388,608 + 1,253,333 = 9,641,941.
        let sidereal = lst(6.0);
        let from = AxisCounts {
            ra: Counts::HOME,
            dec: counts_at(30.0), // on the normal branch, so the solution must stay there
        };
        let solution = solve_from(from, radec(6.0, 40.0), sidereal, Hemisphere::Northern);
        assert_eq!(solution.branch(), Branch::Normal);
        assert_eq!(solution.destination().ra, Counts::HOME, "on the meridian");
        assert_eq!(solution.destination().dec.get(), 9_641_941);

        // The same target from the other branch: dec axis = 40 − 90 = −50°, RA axis = 180°.
        // −50° is 8,388,608 − 1,253,333 = 7,135,275. ±180° name one direction and the canonical
        // one is negative, so the RA axis lands half a revolution *below* home:
        // 8,388,608 − 4,512,000 = 3,876,608.
        let flipped_start = AxisCounts {
            ra: Counts::HOME,
            dec: counts_at(-30.0),
        };
        let flipped = solve_from(
            flipped_start,
            radec(6.0, 40.0),
            sidereal,
            Hemisphere::Northern,
        );
        assert_eq!(flipped.branch(), Branch::ThroughThePole);
        assert_eq!(flipped.destination().dec.get(), 7_135_275);
        assert_eq!(flipped.destination().ra.get(), 3_876_608);

        // Southern hemisphere, same instant and target: dec axis = 90 − (−1)(40) = 130°, which is
        // 9,024,000 × 130/360 = 3,258,666.67 → 3,258,667 counts above home = 11,647,275. The hour
        // angle is negated, so on the meridian the RA axis is still exactly home.
        let southern = solve_from(from, radec(6.0, 40.0), sidereal, Hemisphere::Southern);
        assert_eq!(southern.destination().ra, Counts::HOME);
        assert_eq!(southern.destination().dec.get(), 11_647_275);
    }

    #[test]
    fn a_goto_to_where_the_mount_already_points_is_a_move_of_nothing() {
        // The degenerate case that has to not be a NaN, a full turn, or a flip.
        let sidereal = lst(8.0);
        let hemisphere = Hemisphere::Northern;
        let start = AxisCounts {
            ra: counts_at(-15.0),
            dec: counts_at(35.0),
        };
        let here = mech_to_sky(
            MechPosition {
                ra_axis: scale().angle_at(start.ra),
                dec_axis: scale().angle_at(start.dec),
            },
            sidereal,
            hemisphere,
        );
        let solution = solve_from(start, here.coords, sidereal, hemisphere);
        assert!(
            solution.travel_counts() <= 1,
            "moved {} counts",
            solution.travel_counts()
        );
        assert_eq!(solution.pier_side(), here.pier_side);
    }

    #[test]
    fn no_goto_drives_a_counter_across_its_own_24_bit_wrap() {
        // The hazard `AxisScale::reachable_delta` exists for, at the layer that could produce it.
        // The counter wraps at 16,777,216 and the mechanism at 9,024,000, so a destination
        // outside `[0, 2²⁴)` decodes 50.7° wrong — in the mount as much as here. It is reachable
        // with ordinary numbers: the right-ascension axis sits at the anti-meridian and is sent
        // most of a half-turn.
        let mut rng = Lcg(0x5752_4150); // "WRAP"
        for _ in 0..3_000 {
            let from = AxisCounts {
                ra: counts_at(rng.between(-180.0, 180.0)),
                dec: counts_at(rng.between(-179.9, 179.9)),
            };
            let solution = solve_from(
                from,
                radec(rng.between(0.0, 24.0), rng.between(-90.0, 90.0)),
                lst(rng.between(0.0, 24.0)),
                Hemisphere::Northern,
            );
            for (start, mv, destination) in [
                (from.ra, solution.ra(), solution.destination().ra),
                (from.dec, solution.dec(), solution.destination().dec),
            ] {
                let arithmetic = i64::from(start.get()) + i64::from(mv.delta());
                assert!(
                    (0..0x0100_0000).contains(&arithmetic),
                    "a move from {} by {} lands at {arithmetic}, outside the counter",
                    start.get(),
                    mv.delta()
                );
                // ...and because it did not wrap, the counter the codec computes is the one the
                // arithmetic says, which is what the readback expectation is built on.
                assert_eq!(i64::from(destination.get()), arithmetic);
            }
        }
    }

    #[test]
    fn the_declination_axis_never_needs_the_wrap_adjustment() {
        // A consequence of the no-flip rule worth stating as a fact rather than leaving as a
        // coincidence: because both endpoints are on one half of the circle, the declination
        // counter stays inside 3,876,608..12,900,608 and the 24-bit boundary is 3.9 million
        // counts away in either direction. The right-ascension axis has no such confinement,
        // which is why the adjustment exists at all.
        let mut rng = Lcg(0x4445_4341); // "DECA"
        for _ in 0..3_000 {
            let from = AxisCounts {
                ra: counts_at(rng.between(-180.0, 180.0)),
                dec: counts_at(rng.between(-179.9, 179.9)),
            };
            let solution = solve_from(
                from,
                radec(rng.between(0.0, 24.0), rng.between(-90.0, 90.0)),
                lst(rng.between(0.0, 24.0)),
                Hemisphere::Northern,
            );
            assert_eq!(
                i64::from(solution.dec().delta()),
                scale().shortest_delta(from.dec, solution.destination().dec),
                "the declination move is always the geometrically shortest one"
            );
        }
    }

    #[test]
    fn the_counters_a_solution_reports_are_the_ones_the_moves_actually_reach() {
        // `destination` is what a caller compares `:j` against after the move, and it has to be
        // `from + delta` rather than the canonical target — the two differ by a whole revolution
        // whenever the shortest path wrapped.
        let mut rng = Lcg(0x4152_5249_5645); // "ARRIVE"
        for _ in 0..1_000 {
            let from = AxisCounts {
                ra: counts_at(rng.between(-180.0, 180.0)),
                dec: counts_at(rng.between(-179.9, 179.9)),
            };
            let solution = solve_from(
                from,
                radec(rng.between(0.0, 24.0), rng.between(-90.0, 90.0)),
                lst(rng.between(0.0, 24.0)),
                Hemisphere::Northern,
            );
            assert_eq!(solution.destination().ra, from.ra.after(solution.ra()));
            assert_eq!(solution.destination().dec, from.dec.after(solution.dec()));
        }
    }
}
