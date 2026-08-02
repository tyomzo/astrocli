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
//! d ≥ 0   (normal)    dec = s·(90 − d)    HA = s·(h + 90°)
//! d < 0   (flipped)   dec = s·(90 + d)    HA = s·(h + 90°) + 180°
//! RA = LST − HA                                       (SDD §5.2.3)
//! ```
//!
//! Both branches cover the whole sky. That is not an accident of the algebra, it *is* the German
//! equatorial: every direction is reachable two ways, and which one the tube is in is the pier
//! side. Home — both counters at `0x800000` — is `d = 0`, the tube parallel to the polar axis
//! pointing at the celestial pole, which is where the operator leaves an HEQ5 and what the spike
//! measured at power-on (both axes read exactly home, `FINDINGS.md`).
//!
//! # The `s·90°` is the home hour angle, and it was measured (M3-T06)
//!
//! Until 2026-08-01 this module carried `HA = s·h`, i.e. **home is the meridian**. It is not.
//! The correction is derived below and was measured twice on the operator's HEQ5 from the home
//! pose — counterweight shaft down, tube along the polar axis, both counters at home after a
//! power cycle. [`tests::the_two_swings_measured_from_the_home_pose`] is those two observations
//! as assertions, and it is the only test in this file that is not the model checked against
//! itself. That distinction is the whole lesson of the defect: every fixture here derives its
//! expectations from this same arithmetic, and the mount's counters mean whatever the arithmetic
//! says they mean, so a wrong constant is invisible to the software. It took an operator looking
//! at metal.
//!
//! ## The derivation
//!
//! Work in the equatorial frame with an orthonormal right-handed triad `(M, W, P)`:
//!
//! * `P` — the **north** celestial pole (always north, in both hemispheres: declination is
//!   measured north-positive and hour angle is measured about the NCP wherever you stand).
//! * `M` — declination 0, hour angle 0: the point where the celestial equator crosses the
//!   meridian.
//! * `W` — declination 0, hour angle **+6 h**: due west on the horizon, for every latitude.
//!
//! A direction at `(HA, dec)` is `cos(dec)·cos(HA)·M + cos(dec)·sin(HA)·W + sin(dec)·P`, so a
//! rotation about `P` in the right-hand sense increases the hour angle. Now two body-fixed facts
//! about the mechanism, neither of which changes when the mount is carried across the equator:
//!
//! 1. **The polar axis** points at the pole of the hemisphere: `A = s·P`. The tube lies along it
//!    at home, which is `dec = s·90` — [`Hemisphere::home_declination`].
//! 2. **The counterweight shaft lies *along* the declination axis** and hangs down at home. It is
//!    therefore perpendicular to `A` and in the meridian plane, which leaves only `±M`; and the
//!    downward one is `C = −M` in **both** hemispheres. (North: `M` is due south at altitude
//!    `90 − φ`, above the horizon, so `−M` is the one pointing down. South: `M` is due *north* at
//!    altitude `90 − |φ|`, still above the horizon, so `−M` is still the one pointing down.)
//!
//! Rotating the tube about the declination axis by the counter angle `d` therefore sweeps it
//! through the plane perpendicular to `M` — the `P`/`W` plane. **East–west.** A pure declination
//! move from home cannot reach the meridian, which is exactly what `HA = s·h` claimed it did.
//!
//! Concretely, with `σ = ±1` the wiring's handedness about `C`:
//!
//! ```text
//! tube(d, h=0) = R(C, σd)·A = cos(σd)·A + sin(σd)·(C × A)
//! northern: C × A = (−M) × P = W       so  tube = cos(d)·P + sin(d)·W   (σ = +1, below)
//! southern: C × A = (−M) × (−P) = −W   so  tube = −[cos(d)·P + sin(d)·W]
//! ```
//!
//! `σ = +1` is the measurement, not an assumption: from home, `d = +90°` put the tube **due west
//! on the horizon**, which is `+W`, so `sin(σ·90°) = +1`.
//!
//! Reading `(HA, dec)` off those vectors for `d ∈ (0°, 180°)`: northern gives `dec = 90 − d` and
//! `HA = +90°`; southern is the antipode, `dec = −(90 − d)` and `HA = +90° + 180°`, and the RA
//! axis rotation is about `A = −P`, so it *subtracts*. Both collapse to
//!
//! ```text
//! dec = s·(90 − d)        HA = s·(h + 90°)
//! ```
//!
//! which is the northern `HA = h + 90°` the mount was measured at, and the sign `s` on the
//! constant rather than a bare `+90°`. That sign is the one thing the hardware could not tell us
//! — the mount is in Norway — and it follows from the same `A = s·P` that already puts `s` on
//! `h` and on the declination.
//!
//! ## Why the flipped branch is the *same* offset plus the 180° it already had
//!
//! The two branches are not two models. There is one rotation, and the branch split is only how
//! `(HA, dec)` is written down once `cos(dec)` changes sign. Take the normal-branch expressions
//! as unfolded quantities and let `d` go negative:
//!
//! ```text
//! dec_pre = s·(90 − d)    HA_pre = s·(h + 90°)
//! ```
//!
//! For `s = +1, d < 0` that gives `dec_pre > 90°`, which is not a declination. The standard fold
//! past a pole is `(dec, HA) → (180° − dec, HA + 180°)` — the tube has gone over the top, so the
//! declination comes back down the other side and the hour angle jumps half a turn:
//!
//! ```text
//! dec = 180° − s·(90 − d) = s·(90 + d)        HA = s·(h + 90°) + 180°
//! ```
//!
//! So the `s·90°` is carried by **both** branches and the `180°` is the fold's own companion
//! term, not a competing offset. They compose by addition because they come from different
//! places: one is where home points, the other is what going past the pole does. A reader can
//! check the composition without a mount by taking the branch difference — it is still exactly
//! `180°`, as [`tests::the_two_branches_reach_the_same_sky_from_opposite_declination_counters`]
//! asserts, so the meridian flip is unchanged by this correction.
//!
//! Note also what the constant does *not* touch: `∂HA/∂h = s` and `∂dec/∂d = ∓s` are unchanged,
//! so [`motor_direction`], [`tracking_direction`] and every rate in the driver are exactly as
//! they were. A constant has no derivative. What was wrong was only *where the sky is*, never
//! *which way it moves* — which is why the spike's motion experiments all stand.
//!
//! # What this agrees with, and how you can tell
//!
//! `simulator::mount` carries the *second half* of this decomposition and not the first, and the
//! difference is worth stating precisely because M3-T06 turned on it. The simulator holds an
//! hour angle and a declination **directly**, in degrees, and applies "RA = LST − hour angle, so
//! a mount that is *not* tracking shows its RA climbing at the sidereal rate, and a mount that
//! *is* tracking turns its hour-angle axis at exactly that rate and holds RA still". What it has
//! no equivalent of is the *counters*: there is no home, no `0x800000`, and therefore no
//! `HA = s·(h + 90°)`. Its axis zero is a bookkeeping origin that sidereal time is anchored
//! against at connect, not a pose a mount can be left in.
//!
//! So the simulator could not have caught the home hour angle, and no arrangement of it could
//! have: the defect lived in the one conversion the two implementations never shared.
//! `tests/position_math.rs`'s `the_driver_and_the_simulator_agree` asserts both against one
//! fixture set — including, since M3-T06, the correspondence `simulator hour-angle axis =
//! s·(h + 90°)` written out as an equation, so that a future edit which gives the simulator a
//! home offset of its own has somewhere to fail.
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

/// The hour angle the tube is at with both counters at home, before the hemisphere sign.
///
/// **The correction M3-T06 exists for.** Home is *not* the meridian: the counterweight shaft is
/// the declination axis and hangs in the meridian plane, so the tube swings east–west about it
/// and sits six hours from the meridian at `d = 0`. Measured — see the module docs for the
/// derivation and `spikes/skywatcher-heq5/FINDINGS.md` for the two swings that pinned it.
///
/// One named constant rather than a literal in four places, because the four places must move
/// together: `mech_to_sky` and `sky_to_mech`, each on both branches.
const HOME_HOUR_ANGLE_DEGREES: f64 = 90.0;

/// What the declination axis passing the pole adds to the hour angle.
///
/// Not the same kind of number as [`HOME_HOUR_ANGLE_DEGREES`] and deliberately spelled
/// separately: this one is the pole fold's companion term (module docs), it is unsigned by the
/// hemisphere because `±180°` name the same half-turn, and it is what makes a meridian flip a
/// flip. The two compose by addition.
const THROUGH_THE_POLE_DEGREES: f64 = 180.0;

/// The two mechanical branches, named for what distinguishes them.
///
/// Public because the no-flip goto selection is expressed in these terms and a caller reading a
/// log wants the mechanical fact, not only the pier-side label derived from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Branch {
    /// The declination axis has not passed the pole: `d ≥ 0`, `HA = s·(h + 90°)`.
    Normal,
    /// The declination axis is past the pole: `d < 0`, `HA = s·(h + 90°) + 180°`.
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

    // `h + 90°` and not `h`: home is six hours from the meridian (M3-T06). The hemisphere sign
    // multiplies the whole bracket because the constant comes from the same `A = s·P` the `h`
    // does — see the module docs.
    let hour_angle_from_home = sign * (mech.ra_axis.degrees() + HOME_HOUR_ANGLE_DEGREES);

    let (declination, hour_angle_degrees) = match branch {
        Branch::Normal => (sign * (90.0 - d), hour_angle_from_home),
        Branch::ThroughThePole => (
            sign * (90.0 + d),
            hour_angle_from_home + THROUGH_THE_POLE_DEGREES,
        ),
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
    //
    // The right-ascension half is [`mech_to_sky`] undone term by term, in reverse order: take
    // the flipped branch's half-turn back off, undo the hemisphere sign (`s⁻¹ = s`), then take
    // the home hour angle off. So `h = s·HA − 90°` on the normal branch and
    // `h = s·(HA − 180°) − 90°` past the pole — which the `[-180, 180)` fold below reduces to
    // `s·HA + 90°`, since `−s·180°` and `+180°` name the same half-turn in either hemisphere.
    // The two branches therefore still differ by exactly 180°, which is what makes them one
    // meridian flip apart.
    let (dec_axis, ra_axis) = match branch {
        Branch::Normal => (
            90.0 - sign * dec,
            sign * hour_angle - HOME_HOUR_ANGLE_DEGREES,
        ),
        Branch::ThroughThePole => (
            sign * dec - 90.0,
            sign * (hour_angle - THROUGH_THE_POLE_DEGREES) - HOME_HOUR_ANGLE_DEGREES,
        ),
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

    /// Altitude and azimuth of an hour angle and declination, seen from a latitude.
    ///
    /// The textbook spherical triangle, written out here rather than called from
    /// `astroctl-safety::horizontal` because this crate may not depend on that one (ADD §5.6
    /// rule 1). That is a happy constraint for this particular test: the arithmetic below shares
    /// nothing with the model under test, so "the tube was pointing west, on the horizon" is
    /// checked against geometry rather than against another reading of `mech_to_sky`.
    ///
    /// Azimuth is degrees from north through east, matching `AzDegrees`.
    fn horizontal(hour_angle_degrees: f64, dec_degrees: f64, latitude_degrees: f64) -> (f64, f64) {
        let (sin_ha, cos_ha) = hour_angle_degrees.to_radians().sin_cos();
        let (sin_dec, cos_dec) = dec_degrees.to_radians().sin_cos();
        let (sin_lat, cos_lat) = latitude_degrees.to_radians().sin_cos();
        let altitude = (sin_dec * sin_lat + cos_dec * cos_lat * cos_ha)
            .clamp(-1.0, 1.0)
            .asin()
            .to_degrees();
        let azimuth = (-cos_dec * sin_ha)
            .atan2(sin_dec * cos_lat - cos_dec * sin_lat * cos_ha)
            .to_degrees()
            .rem_euclid(360.0);
        (altitude, azimuth)
    }

    #[test]
    fn the_two_swings_measured_from_the_home_pose() {
        // **The ground truth of M3-T06, and the only assertion in this file that is not the model
        // checked against itself.** Two swings run on the operator's HEQ5 on 2026-08-01 from the
        // home pose — counterweight shaft down, tube along the polar axis, both counters at
        // `0x800000` after a power cycle — and recorded in `spikes/skywatcher-heq5/FINDINGS.md`
        // ("The home hour angle is +6h"). Every other fixture in the tree derives its expectation
        // from this arithmetic, so only these two numbers can fail when the arithmetic is wrong.
        //
        // The site *as configured during that session* was 59.9139° N — Oslo, the example
        // config's default at the time. It is not stated in the finding but it is recoverable
        // from it and worth recovering, because it is what pins the second swing's axis angle:
        // the finding records what the *old* model displayed, and only one reading of the
        // commanded moves reproduces those numbers. See below.
        //
        // **It must not be updated to follow the shipped default**, which moved to Vilnius on
        // 2026-08-02. This is a record of an observation, and an observation is tied to the
        // conditions it was made under; re-baselining it to a latitude the mount was never
        // operated at would quietly destroy the only fixture that can fail when the arithmetic
        // is wrong.
        let hemisphere = Hemisphere::Northern;
        const SESSION_LATITUDE: f64 = 59.9139;

        // Swing 1 — "DEC axis +90° (dec 90 → 0), RA axis untouched".
        //
        //   old model said:  south, altitude 30°   (HA 0, dec 0 → alt = cos φ = 30.08°)
        //   the tube was at: due west, on the horizon
        //
        // The declination is the half that was never in doubt, and it confirms the reading of
        // the command: `dec 90 → 0` is `d = +90°`, since `dec = 90 − d`.
        let after_dec_swing = MechPosition {
            ra_axis: axis(0.0),
            dec_axis: axis(90.0),
        };
        let sky = mech_to_sky(after_dec_swing, lst(0.0), hemisphere);
        assert!(
            sky.coords.dec.degrees().abs() < 1e-9,
            "a 90° declination move from home leaves the tube on the celestial equator, not at {}",
            sky.coords.dec.degrees()
        );
        let hour_angle = HourAngle::of(lst(0.0), sky.coords.ra).degrees();
        assert!(
            (hour_angle - 90.0).abs() < 1e-9,
            "the home hour angle is +6h, so this swing ends at HA +6h, not {hour_angle}°"
        );

        // ...and said the way the operator saw it. Both of these are *exact* and independent of
        // the latitude: hour angle +6h at declination 0 is the west point of the horizon at every
        // site on Earth, which is what makes this observation such good evidence — there is no
        // site error, no clock error and no polar-alignment error hiding in it.
        let (altitude, azimuth) =
            horizontal(hour_angle, sky.coords.dec.degrees(), SESSION_LATITUDE);
        assert!(
            altitude.abs() < 1e-9,
            "the tube was on the horizon; the model puts it at altitude {altitude}°"
        );
        assert!(
            (azimuth - 270.0).abs() < 1e-9,
            "the tube was due west; the model puts it at azimuth {azimuth}°"
        );

        // Swing 2 — "then DEC to 60°, then RA axis 90° (HA −6h)".
        //
        //   old model said:  north-east, altitude 48.5°
        //   the tube was at: straight up — the zenith
        //
        // `dec = 90 − d` puts the declination axis at `d = +30°`. The right-ascension axis is at
        // **−90°**, and the finding's own parenthetical is what says so: it records the old model
        // reading HA −6h, and the old model was `HA = h`. The check below is that this is the
        // reading which reproduces the recorded display — `h = +90°` would have shown HA +6h,
        // altitude 29.9° and due *north*, which is not what was written down. So the finding's
        // "RA axis +90°" is the size of the commanded move in the operator's own direction
        // labels, and the hour angle beside it is the authority on its sign.
        let after_ra_swing = MechPosition {
            ra_axis: axis(-90.0),
            dec_axis: axis(30.0),
        };
        let (old_model_altitude, old_model_azimuth) = horizontal(-90.0, 60.0, SESSION_LATITUDE);
        assert!(
            (old_model_altitude - 48.5).abs() < 0.05 && (old_model_azimuth - 49.0).abs() < 0.5,
            "this is the reading of the commanded moves that reproduces the display the finding \
             recorded (north-east, alt 48.5°); got azimuth {old_model_azimuth}°, altitude \
             {old_model_altitude}°"
        );

        let sky = mech_to_sky(after_ra_swing, lst(0.0), hemisphere);
        assert!(
            (sky.coords.dec.degrees() - 60.0).abs() < 1e-9,
            "declination 60°, not {}",
            sky.coords.dec.degrees()
        );
        let hour_angle = HourAngle::of(lst(0.0), sky.coords.ra).degrees();
        assert!(
            hour_angle.abs() < 1e-9,
            "this swing ends on the meridian, not at HA {hour_angle}°"
        );

        // The zenith, which is the observation the finding calls the stronger of the two because
        // it is unmistakable. Declination 60° on the meridian at latitude 59.9139° is 0.086° from
        // straight up — the corrected model lands there, and *no* value of the missing constant
        // other than +6h does.
        let (altitude, _) = horizontal(hour_angle, sky.coords.dec.degrees(), SESSION_LATITUDE);
        assert!(
            altitude > 89.9,
            "the tube was pointing straight up; the model puts it at altitude {altitude}°"
        );
    }

    #[test]
    fn the_home_hour_angle_is_six_hours_and_carries_the_hemisphere_sign() {
        // The constant on its own, in both hemispheres, so the `s` on it is pinned by a test and
        // not only by the derivation. The mount that measured it is in Norway, so the southern
        // half here is derived rather than observed — flagged in the module docs and in M3-T06's
        // report, and it follows from the same `A = s·P` that already signs `h` and the
        // declination.
        //
        // Stated on the declination axis a hair off home, because *at* home the tube is on the
        // pole and every hour angle names the same direction — which is exactly why this error
        // could hide at the one pose an operator leaves the mount in.
        for hemisphere in BOTH {
            let just_off_home = MechPosition {
                ra_axis: AxisAngle::HOME,
                dec_axis: axis(1.0),
            };
            let sky = mech_to_sky(just_off_home, lst(0.0), hemisphere);
            let hour_angle = HourAngle::of(lst(0.0), sky.coords.ra).degrees();
            assert!(
                (hour_angle - hemisphere.sign() * 90.0).abs() < 1e-9,
                "{hemisphere:?} home sits at HA {hour_angle}°, not {}°",
                hemisphere.sign() * 90.0
            );
        }
    }

    #[test]
    fn a_pure_declination_move_from_home_can_never_reach_the_meridian() {
        // The geometric statement the defect violated, asserted as an impossibility rather than
        // as a value: the counterweight shaft *is* the declination axis and hangs in the meridian
        // plane, so swinging the tube about it sweeps east–west and stays six hours from the
        // meridian for the whole sweep. The old model had this reaching HA 0 at `d = 90°`.
        //
        // A test that only checked `d = 90°` would pass again the moment someone "fixed" the
        // constant to some other wrong value; this one holds across the entire sweep.
        for hemisphere in BOTH {
            for tenths in -1_800..1_800 {
                let d = f64::from(tenths) / 10.0;
                let mech = MechPosition {
                    ra_axis: AxisAngle::HOME,
                    dec_axis: axis(d),
                };
                let sky = mech_to_sky(mech, lst(0.0), hemisphere);
                // At a pole every hour angle is the same direction, so it is not asserted there.
                if sky.coords.dec.degrees().abs() >= 90.0 - 1e-9 {
                    continue;
                }
                let hour_angle = HourAngle::of(lst(0.0), sky.coords.ra).degrees();
                assert!(
                    (hour_angle.abs() - 90.0).abs() < 1e-9,
                    "declination axis {d}° with the RA axis at home reached HA {hour_angle}°, \
                     which the counterweight shaft's geometry forbids"
                );
            }
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
