//! The mechanics ↔ sky decomposition of SDD §5.2.3: two axis angles, a sidereal time, a
//! hemisphere, and the pier side that falls out of them.
//!
//! # The model, in four lines
//!
//! Let `s` be `+1` in the northern hemisphere and `−1` in the southern, `h` the RA axis angle and
//! `d` the DEC axis angle, both measured from the home counter `0x800000` and folded into
//! `[-180, 180)`:
//!
//! ```text
//! d ≥ 0   (normal)    dec = s·(90 − d)    HA = s·h
//! d < 0   (flipped)   dec = s·(90 + d)    HA = s·h + 180°
//! RA = LST − HA                                       (SDD §5.2.3)
//! ```
//!
//! Both branches cover the whole sky. That is not an accident of the algebra, it *is* the German
//! equatorial: every direction is reachable two ways, and which one the tube is in is the pier
//! side. Home — both counters at `0x800000` — is `d = 0`, the tube parallel to the polar axis
//! pointing at the celestial pole, which is where the operator leaves an HEQ5 and what the spike
//! measured at power-on (both axes read exactly home, `FINDINGS.md`).
//!
//! # What this agrees with, and how you can tell
//!
//! `simulator::mount` carries the same decomposition — "RA = LST − hour angle, so a mount that is
//! *not* tracking shows its RA climbing at the sidereal rate, and a mount that *is* tracking
//! turns its hour-angle axis at exactly that rate and holds RA still". `tests/position_math.rs`
//! asserts both implementations against one fixture set rather than against each other's shapes,
//! because two implementations that agree by construction agree about their shared mistakes too.
//!
//! # Two things here are structure, and one is a label
//!
//! **Structure, derived and load-bearing.** That there are exactly two branches; that they are
//! distinguished by the sign of the DEC axis angle; that the flipped branch shifts the hour angle
//! by 180°; that the DEC motor's sense reverses between them and the RA motor's does not. The
//! last of those is the classic guiding gotcha — declination corrections reverse after a meridian
//! flip and right-ascension corrections do not — and it falls out of the algebra above rather
//! than being put in by hand. [`tests::the_declination_sense_reverses_with_the_pier_and_the_right_ascension_sense_does_not`]
//! is where it is pinned down.
//!
//! **A label, `derived` and unverified.** Which branch is [`PierSide::West`] and which is
//! [`PierSide::East`]. The choice below follows ASCOM's naming: the *normal pointing state* is
//! the one reached from home without driving the declination axis through the pole, and the
//! *through-the-pole* state is the other — which is exactly what the sign of `d` distinguishes.
//! Nothing in `spikes/skywatcher-heq5/` bears on it: the spike never pointed the mount at a known
//! object, so no capture can say which physical side of the pier a positive DEC counter puts the
//! tube on. **One experiment settles it** — point at a star near the meridian, read `:j2`, and
//! look at the mount — and until it is run, [`PierSide`] here is a consistent two-valued label
//! whose *polarity* may be inverted. Everything that depends only on the polarity being
//! consistent (no-flip goto selection, guide-direction reversal, meridian limits) is correct
//! either way; only the word shown to an operator could be wrong, and inverting it is the one
//! line marked below.

use astroctl_core::config::SiteConfig;
use astroctl_core::types::{DecDegrees, Direction, PierSide, RaDec, RaHours};

use crate::skywatcher::codec::MotionDirection;

use super::angle::{AxisAngle, HourAngle, Lst};

/// Which pole the mount's polar axis is aimed at.
///
/// Derived from the sign of the configured site latitude (SDD §5.2.3 "hemisphere handling", PRD
/// §8.1 `site.latitude`). It is a mechanical fact about the installation, not a preference: it
/// reverses the sense of *both* motors relative to the sky.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hemisphere {
    /// Polar axis on the north celestial pole. Home points the tube at declination +90°.
    Northern,
    /// Polar axis on the south celestial pole. Home points the tube at declination −90°.
    Southern,
}

impl Hemisphere {
    /// From a latitude in degrees north.
    ///
    /// The equator is northern, arbitrarily. It has to go somewhere and it is the one latitude at
    /// which a German equatorial's polar axis is horizontal — a site where nobody puts one, and
    /// where either answer is equally wrong about a mount that cannot be polar aligned anyway.
    #[must_use]
    pub fn of_latitude(degrees: f64) -> Self {
        if degrees < 0.0 {
            Self::Southern
        } else {
            Self::Northern
        }
    }

    /// From the configured observing site.
    #[must_use]
    pub fn of_site(site: &SiteConfig) -> Self {
        Self::of_latitude(site.latitude)
    }

    /// `+1` northern, `−1` southern — the factor that appears in every line of the model.
    #[must_use]
    pub const fn sign(self) -> f64 {
        match self {
            Self::Northern => 1.0,
            Self::Southern => -1.0,
        }
    }

    /// The declination the tube points at with both counters at home.
    #[must_use]
    pub const fn home_declination(self) -> f64 {
        match self {
            Self::Northern => 90.0,
            Self::Southern => -90.0,
        }
    }
}

/// The two mechanical branches, named for what distinguishes them.
///
/// Public because the no-flip goto selection is expressed in these terms and a caller reading a
/// log wants the mechanical fact, not only the pier-side label derived from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Branch {
    /// The declination axis has not passed the pole: `d ≥ 0`, `HA = s·h`.
    Normal,
    /// The declination axis is past the pole: `d < 0`, `HA = s·h + 180°`.
    ThroughThePole,
}

impl Branch {
    /// The branch a declination axis angle is in.
    ///
    /// `d = 0` is [`Branch::Normal`]: at exactly home the tube is on the pole and no pier side is
    /// meaningful, so the tie has to break somewhere and breaking it toward "has not flipped" is
    /// what keeps [`super::target::goto_solution`] free to pick either side from home.
    #[must_use]
    pub fn of(dec_axis: AxisAngle) -> Self {
        if dec_axis.degrees() < 0.0 {
            Self::ThroughThePole
        } else {
            Self::Normal
        }
    }

    /// The pier side this branch corresponds to.
    ///
    /// **This mapping is the `derived`, unverified label the module docs describe.** Inverting it
    /// is inverting these two lines and nothing else.
    #[must_use]
    pub const fn pier_side(self) -> PierSide {
        match self {
            Self::Normal => PierSide::West,
            Self::ThroughThePole => PierSide::East,
        }
    }

    /// The branch a pier side names.
    #[must_use]
    pub const fn of_pier_side(pier: PierSide) -> Self {
        match pier {
            PierSide::West => Self::Normal,
            PierSide::East => Self::ThroughThePole,
        }
    }

    /// The other one.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Normal => Self::ThroughThePole,
            Self::ThroughThePole => Self::Normal,
        }
    }
}

/// Where the two axes are, mechanically.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MechPosition {
    /// Right-ascension axis angle from home.
    pub ra_axis: AxisAngle,
    /// Declination axis angle from home.
    pub dec_axis: AxisAngle,
}

impl MechPosition {
    /// Both axes at `0x800000`: tube on the pole, counterweight down.
    pub const HOME: Self = Self {
        ra_axis: AxisAngle::HOME,
        dec_axis: AxisAngle::HOME,
    };

    /// Which branch, and therefore which pier side.
    #[must_use]
    pub fn branch(self) -> Branch {
        Branch::of(self.dec_axis)
    }

    /// Which side of the pier the tube is on (SDD §5.2.3: "derived from the DEC counter").
    #[must_use]
    pub fn pier_side(self) -> PierSide {
        self.branch().pier_side()
    }
}

/// A sky position and the mechanical state that produced it.
///
/// The pier side travels with the coordinate because it is not recoverable from it: the same
/// `RaDec` is two different mount states, and `mount.position` (SDD §4.3) carries both for
/// exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyPosition {
    /// Where the telescope is pointing.
    pub coords: RaDec,
    /// Which side of the pier the tube is on.
    pub pier_side: PierSide,
}

/// Mechanical axis angles → sky coordinates (SDD §5.2.3).
///
/// Infallible. Declination is clamped rather than validated because this is the 1 Hz position
/// poll: a coordinate the arithmetic could not express is a fault in the arithmetic, and the poll
/// path must not be able to fail for one. The clamp can only bite by an `f64` epsilon — the model
/// produces `|dec| ≤ 90` exactly.
#[must_use]
pub fn mech_to_sky(mech: MechPosition, lst: Lst, hemisphere: Hemisphere) -> SkyPosition {
    let sign = hemisphere.sign();
    let d = mech.dec_axis.degrees();
    let branch = mech.branch();

    let (declination, hour_angle_degrees) = match branch {
        Branch::Normal => (sign * (90.0 - d), sign * mech.ra_axis.degrees()),
        Branch::ThroughThePole => (sign * (90.0 + d), sign * mech.ra_axis.degrees() + 180.0),
    };

    let hour_angle = HourAngle::wrapped(hour_angle_degrees);
    let right_ascension = hour_angle.right_ascension(lst);
    let dec =
        DecDegrees::new(declination.clamp(DecDegrees::MIN, DecDegrees::MAX)).unwrap_or_else(|_| {
            // Unreachable: the clamp bounds it and the input is finite. Total fallback rather
            // than `expect`, for the reason the module docs and `HourAngle::right_ascension` give.
            DecDegrees::new(0.0).unwrap_or_else(|_| unreachable!("0° is a declination"))
        });

    SkyPosition {
        coords: RaDec::new(right_ascension, dec),
        pier_side: branch.pier_side(),
    }
}

/// Sky coordinates + a chosen branch → mechanical axis angles (SDD §5.2.3).
///
/// The exact inverse of [`mech_to_sky`] for the branch given. Infallible for the same reason:
/// every operand is a validated newtype, and the model's output range is `[-180, 180)` by
/// construction.
#[must_use]
pub fn sky_to_mech(
    coords: RaDec,
    branch: Branch,
    lst: Lst,
    hemisphere: Hemisphere,
) -> MechPosition {
    let sign = hemisphere.sign();
    let hour_angle = HourAngle::of(lst, coords.ra).degrees();
    let dec = coords.dec.degrees();

    // The branch decides both halves at once, which is why they are computed together: a
    // declination taken from one branch and an hour angle from the other is a mount pointing
    // twelve hours away with a plausible-looking declination.
    let (dec_axis, ra_axis) = match branch {
        Branch::Normal => (90.0 - sign * dec, sign * hour_angle),
        Branch::ThroughThePole => (sign * dec - 90.0, sign * hour_angle + 180.0),
    };

    MechPosition {
        ra_axis: AxisAngle::wrapped(ra_axis),
        dec_axis: AxisAngle::wrapped(dec_axis),
    }
}

/// Which axis a slew or guide direction turns, and which way.
///
/// The declination half is the one worth reading twice: its sense depends on **both** the
/// hemisphere and the pier side, because `dec = s·(90 − d)` on one branch and `s·(90 + d)` on the
/// other. That is why a guide loop that worked all evening starts pushing the star out of the
/// frame after a meridian flip — and why this is a function of the mount's state rather than a
/// constant table.
///
/// Right ascension does *not* reverse with the pier side: both branches differ by a constant
/// 180°, whose derivative is zero. It reverses with the hemisphere only.
#[must_use]
pub fn motor_direction(
    direction: Direction,
    branch: Branch,
    hemisphere: Hemisphere,
) -> MotionDirection {
    let sign = hemisphere.sign();
    let rate = match direction {
        // RA = LST − HA and HA = s·h + const, so ∂RA/∂h = −s. East is increasing RA.
        Direction::East => -sign,
        Direction::West => sign,
        // dec = s·(90 ∓ d), so ∂dec/∂d = −s on the normal branch and +s past the pole.
        Direction::North => match branch {
            Branch::Normal => -sign,
            Branch::ThroughThePole => sign,
        },
        Direction::South => match branch {
            Branch::Normal => sign,
            Branch::ThroughThePole => -sign,
        },
    };
    if rate < 0.0 {
        MotionDirection::Backward
    } else {
        MotionDirection::Forward
    }
}

/// The direction the right-ascension axis must turn to hold a coordinate still — i.e. to track.
///
/// The sky turns, so the axis has to. `HA = s·h + const` and the hour angle of a fixed star grows
/// at the sidereal rate, so `h` grows at `s ×` that: **in the southern hemisphere the tracking
/// motor runs the other way**, and a driver that hardcoded forward would drive at twice sidereal
/// in the wrong direction below the equator.
#[must_use]
pub const fn tracking_direction(hemisphere: Hemisphere) -> MotionDirection {
    match hemisphere {
        Hemisphere::Northern => MotionDirection::Forward,
        Hemisphere::Southern => MotionDirection::Backward,
    }
}

/// The right ascension a *stopped* axis reports as the sky turns under it.
///
/// Not used by the driver — it is what [`mech_to_sky`] already does with a later `lst` — but
/// stating it as a function makes the property testable and names the thing SDD §5.2.3 leaves
/// implicit: a drive that has stopped does not hold a coordinate, it holds an hour angle.
#[must_use]
pub fn drifted_right_ascension(mech: MechPosition, lst: Lst, hemisphere: Hemisphere) -> RaHours {
    mech_to_sky(mech, lst, hemisphere).coords.ra
}

#[cfg(test)]
mod tests {
    use super::super::angle::wrap_signed;
    use super::*;
    use astroctl_core::types::RaHours;

    fn lst(hours: f64) -> Lst {
        Lst::from_hours(hours).expect("valid")
    }

    fn radec(ra_hours: f64, dec_degrees: f64) -> RaDec {
        RaDec::from_parts(ra_hours, dec_degrees).expect("valid")
    }

    fn axis(degrees: f64) -> AxisAngle {
        AxisAngle::from_degrees(degrees).expect("valid")
    }

    const BOTH: [Hemisphere; 2] = [Hemisphere::Northern, Hemisphere::Southern];
    const BRANCHES: [Branch; 2] = [Branch::Normal, Branch::ThroughThePole];

    #[test]
    fn home_points_at_the_pole_of_the_hemisphere_it_is_in() {
        // The one mechanical fact everything else is measured from, and the one the spike
        // observed directly: both axes read exactly `0x800000` at power-on, tube on the pole.
        for hemisphere in BOTH {
            let sky = mech_to_sky(MechPosition::HOME, lst(6.0), hemisphere);
            assert!(
                (sky.coords.dec.degrees() - hemisphere.home_declination()).abs() < 1e-9,
                "{hemisphere:?} home pointed at {}",
                sky.coords.dec.degrees()
            );
        }
    }

    #[test]
    fn the_decomposition_round_trips_on_both_branches_in_both_hemispheres() {
        // T-POS-1's round trip, at the angle layer. Every combination of hemisphere and branch,
        // over a grid that includes both poles and both meridian crossings.
        let sidereal = lst(13.25);
        let mut worst_arcsec: f64 = 0.0;
        for hemisphere in BOTH {
            for branch in BRANCHES {
                for ra_hours in [0.0, 3.5, 6.0, 11.999, 12.0, 18.25, 23.99] {
                    for dec_degrees in [-90.0, -75.0, -30.0, 0.0, 30.0, 75.0, 90.0] {
                        let target = radec(ra_hours, dec_degrees);
                        let mech = sky_to_mech(target, branch, sidereal, hemisphere);
                        let back = mech_to_sky(mech, sidereal, hemisphere);
                        let dra = (back.coords.ra.hours() - ra_hours).abs();
                        // 0 h and 24 h are one direction.
                        let dra = dra.min(24.0 - dra) * 15.0 * 3600.0;
                        let ddec = (back.coords.dec.degrees() - dec_degrees).abs() * 3600.0;
                        // At a pole the right ascension is undefined and every value of it names
                        // the same direction, so it is not asserted there.
                        let at_pole = dec_degrees.abs() >= 90.0 - 1e-9;
                        if !at_pole {
                            worst_arcsec = worst_arcsec.max(dra);
                        }
                        worst_arcsec = worst_arcsec.max(ddec);
                    }
                }
            }
        }
        assert!(
            worst_arcsec < 1e-6,
            "worst round-trip error was {worst_arcsec}″"
        );
    }

    #[test]
    fn the_two_branches_reach_the_same_sky_from_opposite_declination_counters() {
        // The German equatorial in one assertion: two mount states, one direction. If this failed
        // there would be no such thing as a meridian flip.
        let sidereal = lst(2.0);
        for hemisphere in BOTH {
            let target = radec(5.5, 22.0);
            let normal = sky_to_mech(target, Branch::Normal, sidereal, hemisphere);
            let flipped = sky_to_mech(target, Branch::ThroughThePole, sidereal, hemisphere);

            assert!(
                (normal.dec_axis.degrees() + flipped.dec_axis.degrees()).abs() < 1e-9,
                "the two declination counters must be negatives: {} and {}",
                normal.dec_axis.degrees(),
                flipped.dec_axis.degrees()
            );
            let ra_gap = wrap_signed(normal.ra_axis.degrees() - flipped.ra_axis.degrees()).abs();
            assert!((ra_gap - 180.0).abs() < 1e-9, "RA axes differ by {ra_gap}°");

            for mech in [normal, flipped] {
                let sky = mech_to_sky(mech, sidereal, hemisphere);
                assert!((sky.coords.ra.hours() - 5.5).abs() < 1e-9);
                assert!((sky.coords.dec.degrees() - 22.0).abs() < 1e-9);
            }
            assert_ne!(
                mech_to_sky(normal, sidereal, hemisphere).pier_side,
                mech_to_sky(flipped, sidereal, hemisphere).pier_side
            );
        }
    }

    #[test]
    fn the_pier_side_comes_from_the_declination_counter_and_nothing_else() {
        // SDD §5.2.3 states it as a property of the DEC counter. Asserting it directly means a
        // future edit that let the hour angle leak into the derivation fails here.
        for hemisphere in BOTH {
            for ra_axis in [-179.0, -90.0, 0.0, 45.0, 179.0] {
                for (dec_axis, expected) in [
                    (-179.0, PierSide::East),
                    (-45.0, PierSide::East),
                    (-1e-12, PierSide::East),
                    (0.0, PierSide::West),
                    (45.0, PierSide::West),
                    (179.0, PierSide::West),
                ] {
                    let mech = MechPosition {
                        ra_axis: axis(ra_axis),
                        dec_axis: axis(dec_axis),
                    };
                    assert_eq!(mech.pier_side(), expected, "dec axis {dec_axis}°");
                    assert_eq!(
                        mech_to_sky(mech, lst(9.0), hemisphere).pier_side,
                        expected,
                        "dec axis {dec_axis}° in {hemisphere:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_declination_sense_reverses_with_the_pier_and_the_right_ascension_sense_does_not() {
        // The guiding gotcha, asserted rather than discovered in Phase 3. Also the hemisphere
        // half: below the equator *both* reverse relative to the north.
        use Direction::{East, North, South, West};
        use MotionDirection::{Backward, Forward};

        let north = Hemisphere::Northern;
        assert_eq!(motor_direction(North, Branch::Normal, north), Backward);
        assert_eq!(
            motor_direction(North, Branch::ThroughThePole, north),
            Forward
        );
        assert_eq!(motor_direction(South, Branch::Normal, north), Forward);
        assert_eq!(motor_direction(East, Branch::Normal, north), Backward);
        assert_eq!(
            motor_direction(East, Branch::ThroughThePole, north),
            Backward,
            "right ascension does not care which side of the pier the tube is on"
        );
        assert_eq!(
            motor_direction(West, Branch::ThroughThePole, north),
            Forward
        );

        let south = Hemisphere::Southern;
        for (direction, branch) in [
            (North, Branch::Normal),
            (South, Branch::Normal),
            (East, Branch::Normal),
            (West, Branch::ThroughThePole),
        ] {
            assert_ne!(
                motor_direction(direction, branch, north),
                motor_direction(direction, branch, south),
                "{direction:?}/{branch:?} must reverse below the equator"
            );
        }
    }

    #[test]
    fn a_motor_direction_actually_moves_the_coordinate_the_way_it_says() {
        // The table above is a claim about signs; this checks it against the decomposition by
        // nudging the counter and looking at where the telescope ends up. A sign table that
        // disagreed with the model would pass the table test and fail this one.
        let sidereal = lst(4.0);
        let nudge = 0.01_f64;
        for hemisphere in BOTH {
            for branch in BRANCHES {
                let start = sky_to_mech(radec(4.0, 20.0), branch, sidereal, hemisphere);
                let before = mech_to_sky(start, sidereal, hemisphere);

                for direction in [Direction::North, Direction::South] {
                    let step = match motor_direction(direction, branch, hemisphere) {
                        MotionDirection::Forward => nudge,
                        MotionDirection::Backward => -nudge,
                    };
                    let moved = MechPosition {
                        dec_axis: axis(start.dec_axis.degrees() + step),
                        ..start
                    };
                    let after = mech_to_sky(moved, sidereal, hemisphere);
                    let delta = after.coords.dec.degrees() - before.coords.dec.degrees();
                    let wanted = if matches!(direction, Direction::North) {
                        nudge
                    } else {
                        -nudge
                    };
                    assert!(
                        (delta - wanted).abs() < 1e-9,
                        "{direction:?} in {hemisphere:?}/{branch:?} moved declination by {delta}°"
                    );
                }

                for direction in [Direction::East, Direction::West] {
                    let step = match motor_direction(direction, branch, hemisphere) {
                        MotionDirection::Forward => nudge,
                        MotionDirection::Backward => -nudge,
                    };
                    let moved = MechPosition {
                        ra_axis: axis(start.ra_axis.degrees() + step),
                        ..start
                    };
                    let after = mech_to_sky(moved, sidereal, hemisphere);
                    let delta =
                        wrap_signed((after.coords.ra.hours() - before.coords.ra.hours()) * 15.0);
                    let wanted = if matches!(direction, Direction::East) {
                        nudge
                    } else {
                        -nudge
                    };
                    assert!(
                        (delta - wanted).abs() < 1e-9,
                        "{direction:?} in {hemisphere:?}/{branch:?} moved RA by {delta}°"
                    );
                }
            }
        }
    }

    #[test]
    fn a_stopped_drive_lets_its_right_ascension_climb_at_the_sidereal_rate() {
        // The lesson the simulator's author wrote down: the sky moves and the tube does not, so a
        // parked mount's reported RA advances. An implementation that stored RA rather than an
        // hour angle would report it standing still, and every drift test built on it would be
        // measuring nothing.
        let hemisphere = Hemisphere::Northern;
        let mech = sky_to_mech(radec(7.0, 40.0), Branch::Normal, lst(7.0), hemisphere);
        let before = drifted_right_ascension(mech, lst(7.0), hemisphere);
        let after = drifted_right_ascension(mech, lst(8.0), hemisphere);
        let climbed = after.hours() - before.hours();
        assert!(
            (climbed - 1.0).abs() < 1e-9,
            "an hour of sidereal time moved the reported RA by {climbed} h"
        );
    }

    #[test]
    fn tracking_turns_the_right_ascension_axis_the_way_the_hemisphere_demands() {
        // ...and holding a coordinate is the same statement: advance the axis by the same hour
        // angle the sky advanced, and the reported RA does not move.
        for hemisphere in BOTH {
            let start_lst = lst(10.0);
            let mech = sky_to_mech(radec(10.5, -15.0), Branch::Normal, start_lst, hemisphere);
            let step = 15.0 * hemisphere.sign(); // one hour of sidereal time on the axis
            let tracked = MechPosition {
                ra_axis: axis(mech.ra_axis.degrees() + step),
                ..mech
            };
            let held = mech_to_sky(tracked, lst(11.0), hemisphere).coords.ra;
            assert!(
                (held.hours() - 10.5).abs() < 1e-9,
                "{hemisphere:?} tracking drifted to {} h",
                held.hours()
            );
            let expected = match hemisphere {
                Hemisphere::Northern => MotionDirection::Forward,
                Hemisphere::Southern => MotionDirection::Backward,
            };
            assert_eq!(tracking_direction(hemisphere), expected);
        }
    }

    #[test]
    fn the_hemisphere_comes_from_the_sign_of_the_configured_latitude() {
        assert_eq!(Hemisphere::of_latitude(59.9139), Hemisphere::Northern);
        assert_eq!(Hemisphere::of_latitude(-33.4489), Hemisphere::Southern);
        assert_eq!(Hemisphere::of_latitude(0.0), Hemisphere::Northern);
        assert_eq!(Hemisphere::of_latitude(-1e-12), Hemisphere::Southern);
    }

    #[test]
    fn the_branch_and_pier_side_labels_are_a_bijection() {
        for branch in BRANCHES {
            assert_eq!(Branch::of_pier_side(branch.pier_side()), branch);
            assert_ne!(branch.opposite(), branch);
            assert_eq!(branch.opposite().opposite(), branch);
        }
        assert_ne!(
            Branch::Normal.pier_side(),
            Branch::ThroughThePole.pier_side()
        );
    }

    #[test]
    fn an_unreachable_declination_never_comes_out_of_the_model() {
        // `DecDegrees` refuses anything past a pole, and this is the only place a driver could
        // manufacture one. Every axis angle in the domain, at a degree's resolution.
        for hemisphere in BOTH {
            for d in -180..180 {
                for h in [-180, -90, 0, 90] {
                    let mech = MechPosition {
                        ra_axis: axis(f64::from(h)),
                        dec_axis: axis(f64::from(d)),
                    };
                    let dec = mech_to_sky(mech, lst(0.0), hemisphere).coords.dec.degrees();
                    assert!(
                        (-90.0..=90.0).contains(&dec),
                        "dec axis {d}° produced declination {dec}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_right_ascension_at_the_seam_survives_the_conversion() {
        // 0h/24h is where a fold that used `%` instead of `rem_euclid` produces a negative right
        // ascension, which `RaHours` normalises into something plausible and wrong.
        for hemisphere in BOTH {
            for branch in BRANCHES {
                for ra_hours in [0.0, 0.001, 23.999] {
                    let target = radec(ra_hours, 10.0);
                    let mech = sky_to_mech(target, branch, lst(23.5), hemisphere);
                    let back = mech_to_sky(mech, lst(23.5), hemisphere).coords.ra;
                    let gap = (back.hours() - ra_hours).abs();
                    assert!(
                        gap.min(RaHours::HOURS_PER_TURN - gap) < 1e-9,
                        "{ra_hours} h came back as {} h",
                        back.hours()
                    );
                }
            }
        }
    }
}
