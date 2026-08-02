//! **T-POS-1** — the position math's acceptance suite (M3-T03, SDD §5.2.3, PRD §4.2).
//!
//! The task's acceptance criteria, one test each:
//!
//! * *property round-trips within 1 count* — [`t_pos_1_every_coordinate_round_trips_within_one_count`]
//! * *table-driven hemisphere/pier cases (N/S latitude × E/W pier × DEC signs)* —
//!   [`t_pos_1_the_eight_hemisphere_pier_declination_cases`]
//! * *golden goto-target cases hand-computed* — [`t_pos_1_hand_computed_goto_targets`]
//! * *speed math against the verified fixture constants* —
//!   [`t_pos_1_the_step_periods_match_the_verified_fixture_constants`]
//! * *no `f64` position leaves the module without going through the typed newtypes* —
//!   [`t_pos_1_no_bare_f64_coordinate_crosses_the_module_boundary`]
//!
//! Plus two things the criteria do not name and the design does:
//!
//! * [`no_computed_goto_path_crosses_the_pole`] — the safety property of the target selection.
//! * [`the_driver_and_the_simulator_agree`] — the same fixtures through both implementations of
//!   SDD §5.2.3's decomposition. The simulator's author wrote the direction lessons down (a
//!   stopped drive's RA climbs at sidereal; tracking holds RA) and this is where the real
//!   driver is held to them, rather than to a second reading of the same paragraph.
//!
//! # Where the numbers come from
//!
//! **The constants** are the operator's own HEQ5, read on 2026-07-29 and recorded in PRD §4.2 —
//! CPR 9,024,000, timer 64,935 Hz, high-speed ratio 16, home `0x800000`. They live in this file
//! and in `#[cfg(test)]` fixtures because that is the *only* place PRD §4.2 permits them: "the
//! driver reads these at handshake and never hardcodes them; the values below are for test
//! fixtures and hand-verification".
//!
//! **The sidereal times** trace to astropy. `astroctl-safety`'s horizontal-transform suite
//! carries alt/az values computed with astropy 8.0.1 / pyerfa 2.0.1.5 in the `TETE` (true equator
//! and equinox of date) frame with `pressure = 0`; of-date rather than ICRS for the reason that
//! suite gives — the mount's right ascension *is* an of-date coordinate derived from its
//! hour-angle axis, so comparing against a J2000 catalogue position would measure 26 years of
//! precession instead of this transform. Those alt/az values are inverted here through the
//! textbook spherical triangle to recover the hour angle, and hence the local sidereal time, at
//! each instant. The inversion is checked by recovering the fixture's own declination from it —
//! see [`astropy_sidereal_time`] — so a mistake in it fails loudly rather than moving the anchor.
//!
//! **Everything else is hand-computed**, in the comment beside the assertion.

use std::time::Duration;

use astroctl_core::types::{Axis, Direction, GuideRate, PierSide, RaDec, SlewSpeed, TrackingMode};
use astroctl_drivers::simulator::motion::{slew_rate, tracking_rate, AxisPlan};
use astroctl_drivers::simulator::profile::{
    ARCSEC_PER_COUNT, COUNTS_PER_REVOLUTION, SIDEREAL_DEG_PER_SEC, TIMER_FREQUENCY_HZ,
};
use astroctl_drivers::skywatcher::codec::{
    Counts, CountsPerRev, HighSpeedRatio, MotionDirection, SpeedClass, TimerFrequency, U24,
};
use astroctl_drivers::skywatcher::controller::{
    AxisParams, MotorController, RateModel, SlewMethod, GOTO_TOLERANCE_COUNTS,
};
use astroctl_drivers::skywatcher::math::{
    mech_to_sky, motor_direction, sky_to_mech, tracking_direction, wrap_signed, AxisAngle,
    AxisCounts, AxisScale, Branch, Hemisphere, Lst, MechPosition, MountGeometry,
};

// -----------------------------------------------------------------------------------------
// Fixtures — see the module docs for why these literals are allowed to exist here
// -----------------------------------------------------------------------------------------

/// `:a1`/`:a2` → `00B289`. Verified, both axes (PRD §4.2).
const CPR: u32 = 9_024_000;
/// `:b1`/`:b2` → `A7FD00`. Verified, and the value that corrected a 460,800 Hz figure.
const TIMER_HZ: u32 = 64_935;
/// `:g1`/`:g2` → `10`. Verified, both axes.
const HIGH_SPEED_RATIO: u8 = 16;
/// The mechanical home counter. Verified: both axes read exactly this at power-on.
const HOME: u32 = 0x0080_0000;
/// `:e1` → `020401`, read big-endian: firmware 2.4, mount model code 1 (HEQ5).
const FIRMWARE_RAW: u32 = 0x0002_0401;

/// The latitude these fixtures were **recorded and computed at**, 59.9139° N.
///
/// Deliberately *not* the shipped example's site, which moved to Vilnius on 2026-08-02. This
/// number is an anchor in two independent senses and neither survives being edited: the astropy
/// alt/az fixtures below were generated at it, and `t_pos_6`'s counters are a real motion sequence
/// performed on the operator's HEQ5 whose zenith observation is only meaningful against the
/// latitude in force when it was made. A fixture that tracked the default config would silently
/// re-baseline both the day somebody moved house.
const ANCHOR_LATITUDE: f64 = 59.9139;
/// Santiago, the southern-hemisphere site `astroctl-safety`'s fixtures also use.
const SANTIAGO_LATITUDE: f64 = -33.4489;

fn scale() -> AxisScale {
    AxisScale::new(CountsPerRev(U24::new(CPR).expect("fits"))).expect("non-zero")
}

fn geometry(latitude: f64) -> MountGeometry {
    MountGeometry::new(scale(), scale(), Hemisphere::of_latitude(latitude))
}

fn controller(axis: Axis) -> MotorController {
    MotorController::new(AxisParams {
        axis,
        firmware: astroctl_drivers::skywatcher::codec::FirmwareVersion::from_raw(FIRMWARE_RAW),
        counts_per_revolution: CountsPerRev(U24::new(CPR).expect("fits")),
        timer_frequency: TimerFrequency(U24::new(TIMER_HZ).expect("fits")),
        high_speed_ratio: HighSpeedRatio(HIGH_SPEED_RATIO),
    })
    .expect("the fixture parameters are all non-zero")
}

fn rates() -> RateModel {
    controller(Axis::Ra).rates()
}

fn radec(ra_hours: f64, dec_degrees: f64) -> RaDec {
    RaDec::from_parts(ra_hours, dec_degrees).expect("a valid fixture coordinate")
}

fn lst_hours(hours: f64) -> Lst {
    Lst::from_hours(hours).expect("a valid sidereal time")
}

fn axis_angle(degrees: f64) -> AxisAngle {
    AxisAngle::from_degrees(degrees).expect("a finite angle")
}

/// Angular separation between two directions, in arcseconds.
///
/// The right measure near a pole, where a small declination error becomes a large right-ascension
/// one and asserting on the components separately would either fail spuriously or need a
/// tolerance loose enough to hide a real error elsewhere — the reasoning `astroctl-safety`'s
/// `separation_arcmin` gives for the same choice.
///
/// **Vincenty's form, not `acos` of the dot product.** That suite compares at the arcminute and
/// `acos` is fine there; this one compares at a *tenth of an arcsecond*, and `acos` of a cosine
/// near 1 loses half its significant figures — its noise floor is about 3 mas, which is a
/// twentieth of the budget here. `atan2` of the cross product over the dot product is exact at
/// small separations, which is the only place this assertion ever looks.
fn separation_arcsec(a: RaDec, b: RaDec) -> f64 {
    let (dec_a, dec_b) = (a.dec.degrees().to_radians(), b.dec.degrees().to_radians());
    let delta_ra = ((a.ra.hours() - b.ra.hours()) * 15.0).to_radians();
    let (sin_a, cos_a) = dec_a.sin_cos();
    let (sin_b, cos_b) = dec_b.sin_cos();
    let (sin_delta, cos_delta) = delta_ra.sin_cos();

    let across = (cos_b * sin_delta).hypot(cos_a * sin_b - sin_a * cos_b * cos_delta);
    let along = sin_a * sin_b + cos_a * cos_b * cos_delta;
    across.atan2(along).to_degrees() * 3600.0
}

// -----------------------------------------------------------------------------------------
// The astropy anchor
// -----------------------------------------------------------------------------------------

/// One of `astroctl-safety`'s astropy reference cases, reused as a sidereal-time anchor.
struct AstropyFixture {
    label: &'static str,
    latitude: f64,
    ra_hours: f64,
    dec_degrees: f64,
    alt_degrees: f64,
    az_degrees: f64,
}

/// Computed with **astropy 8.0.1 / pyerfa 2.0.1.5**, `TETE` → `AltAz`, `pressure = 0`. These are
/// `astroctl-safety::horizontal`'s own fixtures verbatim; reusing them rather than generating new
/// ones means the two modules are anchored to the same instants, so a disagreement between them
/// is a disagreement about the *mount*, not about the sky.
const ASTROPY: &[AstropyFixture] = &[
    AstropyFixture {
        label: "anchor site, transiting the meridian",
        latitude: ANCHOR_LATITUDE,
        ra_hours: 20.490_284_278_437_74,
        dec_degrees: 20.0,
        alt_degrees: 50.086_060,
        az_degrees: 180.000_059,
    },
    AstropyFixture {
        label: "anchor site, a target high in the north-east",
        latitude: ANCHOR_LATITUDE,
        ra_hours: 18.615_649,
        dec_degrees: 38.783_689,
        alt_degrees: 20.624_563,
        az_degrees: 46.728_446,
    },
    AstropyFixture {
        label: "anchor site, grazing the horizon",
        latitude: ANCHOR_LATITUDE,
        ra_hours: 6.752_481,
        dec_degrees: -16.716_116,
        alt_degrees: -0.049_509,
        az_degrees: 235.090_681,
    },
    AstropyFixture {
        label: "santiago, southern hemisphere and west longitude",
        latitude: SANTIAGO_LATITUDE,
        ra_hours: 5.919_529,
        dec_degrees: -52.695_661,
        alt_degrees: 51.245_232,
        az_degrees: 134.422_787,
    },
];

/// The hour angle and local sidereal time implied by an astropy alt/az fixture.
///
/// The plain inverse of the spherical transform `astroctl-safety::horizontal` applies forward:
///
/// ```text
/// sin dec = sin alt · sin φ + cos alt · cos φ · cos A
/// tan HA  = −sin A · cos alt  ÷  (sin alt · cos φ − cos alt · sin φ · cos A)
/// LST     = RA + HA
/// ```
///
/// Independent of everything under test — it touches no `skywatcher` code — and self-checking:
/// the declination it recovers is compared against the fixture's own, so an error in the
/// inversion moves the anchor visibly rather than silently.
fn astropy_sidereal_time(fixture: &AstropyFixture) -> Lst {
    let alt = fixture.alt_degrees.to_radians();
    let az = fixture.az_degrees.to_radians();
    let lat = fixture.latitude.to_radians();

    let sin_dec = alt.sin() * lat.sin() + alt.cos() * lat.cos() * az.cos();
    let dec = sin_dec.clamp(-1.0, 1.0).asin();
    assert!(
        (dec.to_degrees() - fixture.dec_degrees).abs() < 1.0 / 60.0,
        "{}: the inversion recovered declination {:.6}° against the fixture's {:.6}° — the \
         anchor is wrong, not the code under test",
        fixture.label,
        dec.to_degrees(),
        fixture.dec_degrees
    );

    let hour_angle = (-az.sin() * alt.cos())
        .atan2(alt.sin() * lat.cos() - alt.cos() * lat.sin() * az.cos())
        .to_degrees();
    Lst::from_degrees(fixture.ra_hours * 15.0 + hour_angle).expect("finite")
}

// -----------------------------------------------------------------------------------------
// T-POS-1
// -----------------------------------------------------------------------------------------

#[test]
fn t_pos_1_every_coordinate_round_trips_within_one_count() {
    // The property, over a grid dense enough that a systematic error anywhere in the fold, the
    // hemisphere sign or the branch algebra has to show up somewhere in it: two hemispheres, both
    // pier sides, 24 right ascensions, 37 declinations from pole to pole, at four sidereal times
    // — 14,208 cases.
    //
    // Measured worst case is stated in the assertion so that a regression which merely *widens*
    // the error still fails, rather than sliding along under the one-count budget.
    let mut worst_arcsec: f64 = 0.0;
    let mut worst_label = String::new();

    for latitude in [ANCHOR_LATITUDE, SANTIAGO_LATITUDE] {
        let geometry = geometry(latitude);
        for lst_h in [0.0, 5.75, 12.5, 21.25] {
            let lst = lst_hours(lst_h);
            for branch in [Branch::Normal, Branch::ThroughThePole] {
                for ra_step in 0..24 {
                    let ra_hours = f64::from(ra_step);
                    for dec_step in 0..=36 {
                        let dec_degrees = -90.0 + f64::from(dec_step) * 5.0;
                        let target = radec(ra_hours, dec_degrees);

                        let counts = geometry.counts_for(target, branch, lst);
                        let back = geometry.position(counts, lst);

                        // ...except at a pole, where the tube lies on the polar axis and is on
                        // neither side of the pier: the two branches meet there, and the fold to
                        // `[-180, 180)` picks the negative representative for both. See
                        // `the_pier_side_is_undefined_at_a_pole` for that degeneracy on its own.
                        if dec_degrees.abs() < 90.0 {
                            assert_eq!(
                                back.pier_side,
                                branch.pier_side(),
                                "the pier side must survive the round trip"
                            );
                        }

                        // At a pole every right ascension names the same direction, so the
                        // separation — not the components — is the only meaningful comparison,
                        // and it is what this measures everywhere.
                        let gap = separation_arcsec(target, back.coords);
                        if gap > worst_arcsec {
                            worst_arcsec = gap;
                            worst_label = format!(
                                "lat {latitude}, LST {lst_h} h, {branch:?}, RA {ra_hours} h, \
                                 dec {dec_degrees}°"
                            );
                        }
                    }
                }
            }
        }
    }

    // One count is 0.1436″ at the fixture scale, and the only rounding in the path is the
    // counter, which costs half of one.
    assert!(
        worst_arcsec <= ARCSEC_PER_COUNT,
        "worst round trip was {worst_arcsec:.6}″ ({worst_label}), against a one-count budget of \
         {ARCSEC_PER_COUNT:.6}″"
    );
    assert!(
        worst_arcsec > 0.0,
        "every case was exact, which means the grid is not reaching the arithmetic"
    );
    // The measured figure, pinned so that a change which stays inside the budget is still visible.
    //
    // **0.047872″, which is a third of a count and not the half count this comment used to
    // claim.** Measured on the grid above immediately before and immediately after M3-T06's
    // correction to the hour-angle constant, and identical to the last digit both times — which
    // is the useful fact here. The correction shifts every right-ascension axis angle by exactly
    // 90°, and 90° is exactly 2,256,000 counts at this counts-per-revolution, so it moves no
    // rounding boundary at all. The residual is the declination grid's: 5° is 125,333.33 counts,
    // and a third of a count is 0.0479″.
    //
    // The old figure was carried in the comment rather than measured; it is corrected here
    // rather than in passing, because a stale number in a tolerance comment is how the next
    // regression gets waved through.
    assert!(
        worst_arcsec < 0.05,
        "the round trip degraded to {worst_arcsec:.6}″; it has been 0.047872″ (a third of a count)"
    );
}

#[test]
fn t_pos_1_the_round_trip_holds_at_the_astropy_anchored_sidereal_times() {
    // The same property at instants that trace to pyerfa rather than to round numbers, which is
    // where an error in the hour-angle fold would hide: 0 h, 6 h and 12 h are all values at which
    // several wrong expressions happen to be right.
    for fixture in ASTROPY {
        let lst = astropy_sidereal_time(fixture);
        let geometry = geometry(fixture.latitude);
        let target = radec(fixture.ra_hours, fixture.dec_degrees);
        for branch in [Branch::Normal, Branch::ThroughThePole] {
            let counts = geometry.counts_for(target, branch, lst);
            let back = geometry.position(counts, lst);
            let gap = separation_arcsec(target, back.coords);
            assert!(
                gap <= ARCSEC_PER_COUNT,
                "{} on the {branch:?} branch came back {gap:.6}″ away",
                fixture.label
            );
        }
    }
}

#[test]
fn t_pos_1_a_transiting_target_puts_the_right_ascension_axis_a_quarter_turn_below_home() {
    // The strongest single statement this suite can make with an outside reference. astropy puts
    // this target at azimuth 180.000059° from the anchor site — i.e. due south, on the meridian — so its
    // hour angle is zero. Nothing about that conclusion comes from this driver except the
    // arithmetic being tested.
    //
    // **This test used to be called `..._exactly_on_home`, and that name was the defect** (M3-T06,
    // `spikes/skywatcher-heq5/FINDINGS.md`). An outside reference established the hour angle was
    // zero; the suite then asserted that zero hour angle meant the power-on counters, which is
    // the one step astropy had nothing to say about. `HA = s·(h + 90°)` puts a transiting target
    // at `h = −90°`, a quarter turn below home: 9,024,000 / 4 = 2,256,000 counts, so
    // 8,388,608 − 2,256,000 = 6,132,608.
    //
    // The fixture is unchanged. It was always right about the *sky*; what was wrong was which
    // counter that sky corresponds to.
    const QUARTER_TURN: i64 = (CPR / 4) as i64;
    let fixture = &ASTROPY[0];
    let lst = astropy_sidereal_time(fixture);
    let geometry = geometry(fixture.latitude);
    let target = radec(fixture.ra_hours, fixture.dec_degrees);

    let counts = geometry.counts_for(target, Branch::Normal, lst);
    // Within one count, and the residual is astropy's rather than this driver's: the fixture is
    // at azimuth 180.000059°, i.e. 0.21″ short of due south, which is 1.0 count of hour angle at
    // 25,066.7 counts per degree. An exact assertion here would be asserting that the reference
    // is rounder than it is.
    let off_meridian = i64::from(counts.ra.get()) - (i64::from(HOME) - QUARTER_TURN);
    assert!(
        off_meridian.abs() <= 1,
        "a transiting target must sit the right-ascension axis a quarter turn below home \
         ({}), not {off_meridian} counts away from it",
        i64::from(HOME) - QUARTER_TURN
    );

    // ...and its declination axis is 90° − 20° = 70° from home, which is
    // 9,024,000 × 70/360 = 1,754,666.67 → 1,754,667 counts above 8,388,608. Untouched by the
    // correction, and left here as the control: if a future edit to the hour-angle constant also
    // moved this number, the two halves have been confused.
    assert_eq!(counts.dec.get(), HOME + 1_754_667);

    // The flipped branch is the same sky reached from the other side: the declination counter is
    // the mirror image and the right-ascension axis is half a revolution away, i.e. a quarter
    // turn *above* home rather than below it.
    let flipped = geometry.counts_for(target, Branch::ThroughThePole, lst);
    assert_eq!(flipped.dec.get(), HOME - 1_754_667);
    let off_flipped = i64::from(flipped.ra.get()) - (i64::from(HOME) + QUARTER_TURN);
    assert!(
        off_flipped.abs() <= 1,
        "the flipped right-ascension axis is {off_flipped} counts from a quarter turn above home"
    );
    // The half revolution between them, stated as the thing it is: one meridian flip.
    assert_eq!(
        i64::from(flipped.ra.get()) - i64::from(counts.ra.get()),
        i64::from(CPR / 2)
    );

    // The home pose itself, said plainly: with both counters at `0x800000` the tube is six hours
    // west of the meridian. This is the assertion whose absence let the defect live — the suite
    // had no case that read the sky *out* of the power-on counters, only cases that read counters
    // out of the sky.
    let at_home_hour_angle = wrap_signed(
        lst.degrees() - geometry.position(AxisCounts::HOME, lst).coords.ra.hours() * 15.0,
    );
    assert!(
        (at_home_hour_angle - 90.0).abs() < 1e-9,
        "the home pose looks at hour angle {at_home_hour_angle}°, not +6h"
    );
}

#[test]
fn t_pos_6_the_home_pose_swings_measured_on_the_mount() {
    // **M3-T06's acceptance criterion, in the units the mount was actually commanded in.**
    //
    // `math::mech`'s own `the_two_swings_measured_from_the_home_pose` asserts these two
    // observations in axis *angles*, and says where they came from. This one asserts them in
    // 24-bit *counters*, through `MountGeometry::position` — the SDD §5.2.3 seam the driver's
    // 1 Hz poll actually calls. That is the path the operator's readout came down, so it is the
    // path the correction has to be true on; an error in `AxisScale` between the two layers would
    // pass there and fail here.
    //
    // Both counters are exact, which is worth noting because nothing else in this suite is:
    // 9,024,000 / 4 = 2,256,000 and 9,024,000 / 12 = 752,000 divide without remainder, so these
    // two poses have no rounding in them at all and the assertions can be exact.
    let anchor = geometry(ANCHOR_LATITUDE);
    let lst = lst_hours(0.0);
    let hour_angle_at =
        |counts| wrap_signed(lst.degrees() - anchor.position(counts, lst).coords.ra.hours() * 15.0);

    // Swing 1 — from home, the declination axis alone driven a quarter turn. The tube was seen
    // due west on the horizon: declination 0, hour angle +6h.
    let after_dec_swing = AxisCounts {
        ra: Counts::HOME,
        dec: Counts::new(HOME + CPR / 4).expect("fits"),
    };
    let sky = anchor.position(after_dec_swing, lst);
    assert!(sky.coords.dec.degrees().abs() < 1e-9, "on the equator");
    assert!(
        (hour_angle_at(after_dec_swing) - 90.0).abs() < 1e-9,
        "six hours west, where the tube was seen: got {}°",
        hour_angle_at(after_dec_swing)
    );

    // Swing 2 — declination axis back to +30° (declination 60°) and the right-ascension axis a
    // quarter turn below home. The tube was seen at the zenith: hour angle 0, declination within
    // a tenth of a degree of the anchor latitude.
    let at_the_zenith = AxisCounts {
        ra: Counts::new(HOME - CPR / 4).expect("fits"),
        dec: Counts::new(HOME + CPR / 12).expect("fits"),
    };
    let sky = anchor.position(at_the_zenith, lst);
    assert!((sky.coords.dec.degrees() - 60.0).abs() < 1e-9);
    assert!(
        hour_angle_at(at_the_zenith).abs() < 1e-9,
        "on the meridian, which with declination 60° from latitude {ANCHOR_LATITUDE} is the zenith"
    );
    assert!(
        (sky.coords.dec.degrees() - ANCHOR_LATITUDE).abs() < 0.1,
        "a target on the meridian at the site latitude is straight up, and this is {}° from it",
        (sky.coords.dec.degrees() - ANCHOR_LATITUDE).abs()
    );

    // And the pose the mount is left in overnight, said once in counters: both axes at
    // `0x800000` is **not** the meridian. This is the assertion whose absence let the defect
    // survive M3-T03's whole acceptance suite.
    assert!(
        (hour_angle_at(AxisCounts::HOME) - 90.0).abs() < 1e-9,
        "the power-on counters look six hours west of the meridian"
    );
}

#[test]
fn the_pier_side_is_undefined_at_a_pole_and_the_model_says_so_consistently() {
    // The one degeneracy in the decomposition, pinned rather than papered over. At a celestial
    // pole the tube lies *on* the polar axis and is on neither side of the pier — so the two
    // branches meet, and the declination-axis angles they give (0° and −0°, or +180° and −180°)
    // are the same direction. The fold to `[-180, 180)` picks one representative, and which one
    // it picks is arbitrary but must be *stable*: a mount that reported a flapping pier side
    // while parked at the pole would make the meridian limit flap with it.
    let geometry = geometry(ANCHOR_LATITUDE);
    let lst = lst_hours(4.0);

    // Home: the tube on the pole the mount is aligned to. Declination axis exactly 0.
    let at_home = geometry.position(AxisCounts::HOME, lst);
    assert!((at_home.coords.dec.degrees() - 90.0).abs() < 1e-9);
    assert_eq!(at_home.pier_side, PierSide::West, "0° is not negative");

    // The far pole, reached either way round: the declination axis is half a revolution from
    // home, and ±180° is one direction whose canonical spelling is negative.
    for branch in [Branch::Normal, Branch::ThroughThePole] {
        let counts = geometry.counts_for(radec(6.0, -90.0), branch, lst);
        let there = geometry.position(counts, lst);
        assert!(
            (there.coords.dec.degrees() + 90.0).abs() < 1e-6,
            "the far pole is declination −90°, not {}",
            there.coords.dec.degrees()
        );
        assert_eq!(
            there.pier_side,
            PierSide::East,
            "whichever branch asked for it, the far pole has one spelling"
        );
    }

    // ...and one count away from a pole the pier side is meaningful again and both branches are
    // distinguishable, which is what makes the degeneracy a point rather than a region.
    let near = radec(6.0, -90.0 + ARCSEC_PER_COUNT * 2.0 / 3600.0);
    assert_ne!(
        geometry
            .position(geometry.counts_for(near, Branch::Normal, lst), lst)
            .pier_side,
        geometry
            .position(geometry.counts_for(near, Branch::ThroughThePole, lst), lst)
            .pier_side
    );
}

/// One row of the hemisphere × pier × declination-sign table.
struct MechCase {
    latitude: f64,
    branch: Branch,
    pier_side: PierSide,
    dec_degrees: f64,
    /// Hand-computed from `dec_axis = 90 − s·dec` (normal) or `s·dec − 90` (through the pole).
    /// Unchanged by M3-T06 — the declination half never depended on the home hour angle.
    expected_dec_axis: f64,
    /// Hand-computed from `ra_axis = s·HA − 90°` (normal) or `s·(HA − 180°) − 90°` (through the
    /// pole), at the fixed hour angle of +30° every row below uses.
    ///
    /// The `−90°` is M3-T06's correction: home is six hours from the meridian, so every row here
    /// moved a quarter turn. Each is re-derived from the corrected model in the row's own
    /// comment rather than shifted by 90° from what used to be written — a row nudged until it
    /// passes is a row that certifies whatever the code does.
    expected_ra_axis: f64,
}

#[test]
fn t_pos_1_the_eight_hemisphere_pier_declination_cases() {
    // N/S latitude × E/W pier × DEC sign, all eight, with every mechanical angle worked out by
    // hand from the model in `math::mech`. Both directions of the conversion are checked against
    // the same row, so a sign error that cancelled itself would still have to match a number
    // written down outside the code.
    //
    // One instant and one hour angle for all eight: LST 6 h (90°), target right ascension 4 h
    // (60°), so HA = +30° — west of the meridian, a setting target, in every row.
    //
    // The four right-ascension axis angles, derived from `HA = s·(h + 90°)` (+ 180° past the
    // pole) at HA = +30°. Each is `h` solved for, not the old row shifted:
    //
    //   northern, normal:            h = s·HA − 90        =  30 − 90        =  −60°
    //   northern, through the pole:  h = s·(HA − 180) − 90 = (30 − 180) − 90 = −240° → +120°
    //   southern, normal:            h = s·HA − 90        = −30 − 90        = −120°
    //   southern, through the pole:  h = s·(HA − 180) − 90 = 150 − 90       =   +60°
    //
    // Two cross-checks a reader can make without running anything. The two branches within a
    // hemisphere differ by 180° (−60 vs +120; −120 vs +60), which is the meridian flip and is
    // untouched by this correction. And the northern and southern normal rows are *not* mirror
    // images the way they were under `HA = s·h` (they were +30 and −30): the home offset carries
    // the same `s`, so the pair is −60 and −120, symmetric about −90° rather than about 0°.
    // −90° is where the meridian now sits, in both hemispheres.
    const LST_HOURS: f64 = 6.0;
    const RA_HOURS: f64 = 4.0;

    let table = [
        // Northern hemisphere, s = +1.
        MechCase {
            latitude: ANCHOR_LATITUDE,
            branch: Branch::Normal,
            pier_side: PierSide::West,
            dec_degrees: 40.0,
            expected_dec_axis: 50.0,
            expected_ra_axis: -60.0,
        },
        MechCase {
            latitude: ANCHOR_LATITUDE,
            branch: Branch::Normal,
            pier_side: PierSide::West,
            dec_degrees: -40.0,
            expected_dec_axis: 130.0,
            expected_ra_axis: -60.0,
        },
        MechCase {
            latitude: ANCHOR_LATITUDE,
            branch: Branch::ThroughThePole,
            pier_side: PierSide::East,
            dec_degrees: 40.0,
            expected_dec_axis: -50.0,
            expected_ra_axis: 120.0,
        },
        MechCase {
            latitude: ANCHOR_LATITUDE,
            branch: Branch::ThroughThePole,
            pier_side: PierSide::East,
            dec_degrees: -40.0,
            expected_dec_axis: -130.0,
            expected_ra_axis: 120.0,
        },
        // Southern hemisphere, s = −1: the declination reference, the hour-angle sense *and* the
        // home offset all invert, which is the whole of "hemisphere handling".
        MechCase {
            latitude: SANTIAGO_LATITUDE,
            branch: Branch::Normal,
            pier_side: PierSide::West,
            dec_degrees: 40.0,
            expected_dec_axis: 130.0,
            expected_ra_axis: -120.0,
        },
        MechCase {
            latitude: SANTIAGO_LATITUDE,
            branch: Branch::Normal,
            pier_side: PierSide::West,
            dec_degrees: -40.0,
            expected_dec_axis: 50.0,
            expected_ra_axis: -120.0,
        },
        MechCase {
            latitude: SANTIAGO_LATITUDE,
            branch: Branch::ThroughThePole,
            pier_side: PierSide::East,
            dec_degrees: 40.0,
            expected_dec_axis: -130.0,
            expected_ra_axis: 60.0,
        },
        MechCase {
            latitude: SANTIAGO_LATITUDE,
            branch: Branch::ThroughThePole,
            pier_side: PierSide::East,
            dec_degrees: -40.0,
            expected_dec_axis: -50.0,
            expected_ra_axis: 60.0,
        },
    ];

    let lst = lst_hours(LST_HOURS);
    for case in &table {
        let hemisphere = Hemisphere::of_latitude(case.latitude);
        let target = radec(RA_HOURS, case.dec_degrees);
        let label = format!(
            "lat {} / {:?} / dec {}",
            case.latitude, case.branch, case.dec_degrees
        );

        // Forward: the sky position becomes the hand-computed mechanical angles.
        let mech = sky_to_mech(target, case.branch, lst, hemisphere);
        assert!(
            (mech.dec_axis.degrees() - case.expected_dec_axis).abs() < 1e-9,
            "{label}: declination axis is {}°, expected {}°",
            mech.dec_axis.degrees(),
            case.expected_dec_axis
        );
        assert!(
            (mech.ra_axis.degrees() - case.expected_ra_axis).abs() < 1e-9,
            "{label}: right-ascension axis is {}°, expected {}°",
            mech.ra_axis.degrees(),
            case.expected_ra_axis
        );

        // The pier side is read off the declination counter and nothing else (SDD §5.2.3).
        assert_eq!(mech.pier_side(), case.pier_side, "{label}");
        assert_eq!(
            case.expected_dec_axis < 0.0,
            matches!(case.pier_side, PierSide::East),
            "{label}: the table itself must agree that the sign is what names the side"
        );

        // Back: the hand-computed angles become the sky position again.
        let recovered = mech_to_sky(
            MechPosition {
                ra_axis: axis_angle(case.expected_ra_axis),
                dec_axis: axis_angle(case.expected_dec_axis),
            },
            lst,
            hemisphere,
        );
        assert!(
            separation_arcsec(target, recovered.coords) < 1e-6,
            "{label}: came back at RA {} h dec {}°",
            recovered.coords.ra.hours(),
            recovered.coords.dec.degrees()
        );
        assert_eq!(recovered.pier_side, case.pier_side, "{label}");
    }
}

#[test]
fn t_pos_1_hand_computed_goto_targets() {
    // Golden cases: a starting counter, a target, and the counters the goto must end on, each
    // one worked out with a calculator in the comment beside it. 9,024,000 counts a revolution
    // is 25,066.667 counts a degree.
    let anchor = geometry(ANCHOR_LATITUDE);

    // A quarter of a revolution — 9,024,000 / 4 — is 2,256,000 counts, and after M3-T06 it is in
    // every one of these because it is the home hour angle expressed as a counter.
    //
    // (1) LST 6 h, target RA 6 h dec +40°, starting on the normal branch.
    //     HA = 0, so the right-ascension axis goes to s·HA − 90 = −90° = home − 2,256,000
    //     = 6,132,608. Declination axis = 90 − 40 = 50° = 1,253,333.33 → 1,253,333 counts above
    //     home = 9,641,941.
    let from = AxisCounts {
        ra: Counts::new(HOME + 200_000).expect("fits"),
        dec: Counts::new(HOME + 500_000).expect("fits"),
    };
    let solution = anchor
        .goto(from, radec(6.0, 40.0), lst_hours(6.0))
        .expect("in range");
    assert_eq!(solution.destination().ra.get(), 6_132_608);
    assert_eq!(solution.destination().dec.get(), 9_641_941);
    assert_eq!(solution.pier_side(), PierSide::West);
    //     ...and the two moves are the differences, with their directions. The right-ascension
    //     move grew from 200,000 counts to 2,456,000 — the mount starts 200,000 counts above home
    //     and has to reach 2,256,000 below it — and stayed backward.
    assert_eq!(solution.ra().delta(), -(200_000 + 2_256_000));
    assert_eq!(solution.dec().delta(), 1_253_333 - 500_000);
    assert_eq!(solution.ra().direction(), MotionDirection::Backward);
    assert_eq!(solution.dec().direction(), MotionDirection::Forward);

    // (2) The same target from the far side of the pier. Declination axis = 40 − 90 = −50°,
    //     right-ascension axis = s·(HA − 180) − 90 = −270° → the canonical +90°, i.e.
    //     home + 2,256,000 = 10,644,608 — exactly half a revolution from case (1)'s 6,132,608,
    //     because that is what a pier flip is.
    let flipped_start = AxisCounts {
        ra: Counts::HOME,
        dec: Counts::new(HOME - 500_000).expect("fits"),
    };
    let flipped = anchor
        .goto(flipped_start, radec(6.0, 40.0), lst_hours(6.0))
        .expect("in range");
    assert_eq!(flipped.destination().dec.get(), HOME - 1_253_333);
    assert_eq!(flipped.destination().ra.get(), HOME + 2_256_000);
    assert_eq!(
        flipped.destination().ra.get() - solution.destination().ra.get(),
        CPR / 2
    );
    assert_eq!(flipped.pier_side(), PierSide::East);

    // (3) A target 6 h east of the meridian: LST 12 h, RA 18 h → HA = −90°, so on the normal
    //     branch the right-ascension axis goes to −90 − 90 = −180°, whose canonical spelling is
    //     negative: home − 4,512,000 = 3,876,608. Declination +10° gives an axis angle of 80°
    //     = 2,005,333.33 → 2,005,333 counts above home.
    //
    //     Started from a mount already on the normal branch, which this case did not need to do
    //     before. From home *both* branches are candidates and the solver picks by travel, and
    //     M3-T06 changed that choice for this target — see the assertion below. Forcing the
    //     branch is what keeps this case a test of the −180° arithmetic rather than of the
    //     tie-break.
    let on_the_normal_branch = AxisCounts {
        ra: Counts::HOME,
        dec: Counts::new(HOME + 500_000).expect("fits"),
    };
    let east = anchor
        .goto(on_the_normal_branch, radec(18.0, 10.0), lst_hours(12.0))
        .expect("in range");
    assert_eq!(east.branch(), Branch::Normal);
    assert_eq!(east.destination().ra.get(), 3_876_608);
    assert_eq!(east.destination().dec.get(), HOME + 2_005_333);

    // (3b) The same target from home, where the solver is free to choose — and now chooses the
    //      other side of the pier. This is a real behavioural consequence of M3-T06 and is
    //      asserted rather than left to be discovered: at home the tube is six hours *west* of
    //      the meridian on the normal branch, so a target six hours *east* is a half-turn away
    //      there (4,512,000 counts) and a standstill on the through-the-pole branch, which costs
    //      only its 2,005,333 counts of declination. Under `HA = s·h` the two branches tied at
    //      2,256,000 counts each and the tie broke toward normal.
    //
    //      Nothing unsafe follows: home is the one pose on neither side of the pier, so this is
    //      a choice and not a flip — `no_computed_goto_path_crosses_the_pole` still holds
    //      everywhere else, and this move is genuinely the shorter one.
    let from_home = anchor
        .goto(AxisCounts::HOME, radec(18.0, 10.0), lst_hours(12.0))
        .expect("in range");
    assert_eq!(from_home.branch(), Branch::ThroughThePole);
    assert_eq!(
        from_home.destination().ra,
        Counts::HOME,
        "no RA move at all"
    );
    assert_eq!(from_home.destination().dec.get(), HOME - 2_005_333);
    assert!(from_home.travel_counts() < east.travel_counts());

    // (4) Southern hemisphere, same sky. `h = s·HA − 90 = +90 − 90 = 0`: **the right-ascension
    //     axis lands exactly on the home counter.** That is not a coincidence and it is the
    //     cleanest golden case in the suite — it says in one number that a southern mount sitting
    //     at its power-on counters is looking six hours *east* of the meridian, the mirror of the
    //     northern +6h the operator measured. Under the old model this case read
    //     home + 2,256,000 and the northern case (1) read home; the correction has swapped which
    //     of the two is the round number, which is a good sign that it moved the map rather than
    //     the fixtures.
    //
    //     The declination reference flips as it always did: 90 − (−1)(10) = 100°
    //     = 2,506,666.67 → 2,506,667 counts above home.
    let southern = geometry(SANTIAGO_LATITUDE)
        .goto(AxisCounts::HOME, radec(18.0, 10.0), lst_hours(12.0))
        .expect("in range");
    assert_eq!(southern.destination().ra, Counts::HOME);
    assert_eq!(southern.destination().dec.get(), HOME + 2_506_667);
}

#[test]
fn t_pos_1_the_step_periods_match_the_verified_fixture_constants() {
    // The acceptance criterion in full. Every expected value below is
    // `64,935 ÷ (rate in counts/s)`, hand-computed against the *verified* 64,935 Hz — PRD §4.2:
    // "Do not hand-compute against the old 460,800 figure; it was wrong by 7.1× and any expected
    // value derived from it is invalid."
    //
    //   sidereal  9,024,000 ÷ 86,164.0905 = 104.7304 counts/s → 64,935 ÷ 104.7304 = 620.02 → 620
    //   lunar     14.685″/s ÷ 0.143617″   = 102.2511 counts/s → 64,935 ÷ 102.2511 = 635.05 → 635
    //   solar     15.000″/s ÷ 0.143617″   = 104.4444 counts/s → 64,935 ÷ 104.4444 = 621.72 → 622
    let rates = rates();
    for (mode, counts_per_second, period) in [
        (TrackingMode::Sidereal, 104.730_4, 620_u32),
        (TrackingMode::Lunar, 102.2511, 635),
        (TrackingMode::Solar, 104.4444, 622),
    ] {
        let rate = rates.tracking(mode).expect("valid");
        assert!(
            (rate.get() - counts_per_second).abs() < 1e-3,
            "{mode:?} is {} counts/s, hand-computed as {counts_per_second}",
            rate.get()
        );
        let programmed = rates.program(rate).expect("in range");
        assert_eq!(programmed.period().get(), period, "{mode:?}");
        assert_eq!(programmed.speed(), SpeedClass::Low, "{mode:?}");
    }

    // 620 is the measured constant, not merely the computed one: E10 drove the mount at it for
    // 30 s and read 104.617 counts/s against the predicted 104.7304 — 0.11% agreement, which is
    // what closed the timer-frequency risk by measurement rather than inference.
    let measured_counts_per_second = 104.617;
    let predicted = rates.rate_of(
        rates
            .program(rates.tracking(TrackingMode::Sidereal).expect("valid"))
            .expect("in range"),
    );
    assert!(
        (predicted - measured_counts_per_second).abs() / measured_counts_per_second < 0.002,
        "the programmed rate is {predicted} counts/s against E10's measured {measured_counts_per_second}"
    );
}

#[test]
fn t_pos_1_no_bare_f64_coordinate_crosses_the_module_boundary() {
    // "No `f64` position leaves the module without going through the typed newtypes." A test
    // cannot inspect signatures, so it asserts the consequence: every value that carries a
    // position out of this module is constructible only through a validator, and the validators
    // refuse what a bare `f64` would carry through silently.
    assert!(AxisAngle::from_degrees(f64::NAN).is_err());
    assert!(Lst::from_degrees(f64::INFINITY).is_err());
    assert!(RaDec::from_parts(f64::NAN, 0.0).is_err());
    assert!(RaDec::from_parts(0.0, 91.0).is_err(), "past the pole");
    assert!(Counts::new(0x0100_0000).is_err(), "past 24 bits");

    // ...and the seam's own output is those types rather than numbers: a declination that the
    // decomposition could produce past a pole would have to survive `DecDegrees`, and it does not
    // exist — every mechanical angle in the domain maps inside `[-90, 90]`.
    let geometry = geometry(ANCHOR_LATITUDE);
    for dec_axis in -180..180 {
        let position = geometry.position(
            AxisCounts {
                ra: Counts::HOME,
                dec: scale().counts_at(axis_angle(f64::from(dec_axis))),
            },
            lst_hours(0.0),
        );
        let dec = position.coords.dec.degrees();
        assert!((-90.0..=90.0).contains(&dec), "axis {dec_axis}° gave {dec}");
    }
}

// -----------------------------------------------------------------------------------------
// The safety property
// -----------------------------------------------------------------------------------------

#[test]
fn no_computed_goto_path_crosses_the_pole() {
    // The design constraint, at the integration boundary: whatever the mount is pointing at and
    // wherever it is told to go, the declination axis never sweeps through zero — the pole — and
    // therefore never swings the tube over the pier. A meridian flip is a deliberate, separate
    // motion (SDD §5.4), not something a goto does on the way past.
    //
    // A fixed-seed generator rather than a proptest dependency: `astroctl-drivers` has none, and
    // adding one is a workspace decision rather than a task's.
    let mut state = 0x4E_4F_46_4C_49_50_5F_31_u64; // "NOFLIP_1"
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1_u64 << 53) as f64
    };

    for latitude in [ANCHOR_LATITUDE, SANTIAGO_LATITUDE] {
        let geometry = geometry(latitude);
        for _ in 0..5_000 {
            let start_dec_axis = next().mul_add(359.0, -179.5);
            let from = AxisCounts {
                ra: scale().counts_at(axis_angle(next().mul_add(360.0, -180.0))),
                dec: scale().counts_at(axis_angle(start_dec_axis)),
            };
            let target = radec(next() * 24.0, next().mul_add(180.0, -90.0));
            let lst = lst_hours(next() * 24.0);

            let solution = geometry.goto(from, target, lst).expect("in range");

            let start = geometry.mech(from).dec_axis.degrees();
            let swept = f64::from(solution.dec().delta()) * scale().degrees_per_count();
            let end = start + swept;
            assert!(
                !(start.min(end) < -1e-9 && start.max(end) > 1e-9),
                "a goto swept the declination axis from {start}° to {end}° at latitude \
                 {latitude}, which is through the pole"
            );
            assert_eq!(
                solution.pier_side(),
                geometry.mech(from).pier_side(),
                "and the pier side is what that guarantee is called above the HAL"
            );
            // Nearest, with the one exception the counter forces. The declination axis is
            // always the geometrically shortest move — the branch rule confines it to one half
            // of the circle, so it can never need the wrap adjustment. The right-ascension axis
            // may take the long way round, and only when the short way would have driven the
            // counter across its own 24-bit boundary, where the counts↔angle correspondence
            // breaks.
            assert!(solution.dec().magnitude().get() <= CPR / 2);
            assert!(solution.ra().magnitude().get() <= CPR);
            if solution.ra().magnitude().get() > CPR / 2 {
                let short = scale().shortest_delta(from.ra, solution.destination().ra);
                let would_land = i64::from(from.ra.get()) + short;
                assert!(
                    !(0..0x0100_0000).contains(&would_land),
                    "the long way round was taken for a move whose short way was safe: \
                     {short} counts from {} lands at {would_land}",
                    from.ra.get()
                );
            }

            // ...and it still arrives, which is what makes refusing the other branch free.
            let landed = geometry.position(solution.destination(), lst);
            assert!(
                separation_arcsec(target, landed.coords) <= ARCSEC_PER_COUNT,
                "a no-flip goto must still land on its target: wanted RA {} h dec {}°, landed \
                 RA {} h dec {}° — {}″ away, from declination axis {}°",
                target.ra.hours(),
                target.dec.degrees(),
                landed.coords.ra.hours(),
                landed.coords.dec.degrees(),
                separation_arcsec(target, landed.coords),
                start
            );
        }
    }
}

// -----------------------------------------------------------------------------------------
// The shared fixtures: this driver and the simulator, against the same numbers
// -----------------------------------------------------------------------------------------

#[test]
fn the_driver_and_the_simulator_agree() {
    // Two independent implementations of SDD §5.2.3 live in this crate. The simulator holds an
    // hour-angle axis in degrees and integrates a plan; the driver holds a 24-bit counter and
    // does integer arithmetic on it. They are asserted against each other here rather than each
    // against its own reading of the specification, because a paragraph read twice is one source,
    // not two.
    the_constants_agree();
    the_tracking_rates_agree();
    the_slew_ladder_agrees();
    a_stopped_drive_climbs_and_a_tracking_one_holds();
    the_same_axis_state_is_the_same_sky_in_both();
}

fn the_same_axis_state_is_the_same_sky_in_both() {
    // **M3-T06 acceptance criterion 4**, and the honest version of it.
    //
    // The two implementations do not share a parameterisation, and that is deliberate rather
    // than an oversight: the driver holds a 24-bit *counter* and converts it, while the
    // simulator holds the hour angle and the declination themselves — `simulator::mount`'s
    // `Axes.ra` **is** the hour angle in degrees, with no home and no counts. So "the same
    // commanded axis angles" has to be spelled out as the correspondence between them, which is
    // exactly `mech_to_sky`:
    //
    //     simulator hour-angle axis  =  s·(h + 90°)   (+180° past the pole)
    //     simulator declination axis =  s·(90 − d)    (s·(90 + d) past the pole)
    //
    // Writing it here is the point of the test. It is the equation that would have to be
    // satisfied if the simulator ever grew a mechanical-counter layer, and it is where a future
    // edit that gave the simulator a home offset of its own — or the same one twice — fails.
    //
    // What this does *not* claim: that the simulator would have caught the home hour angle.
    // It could not have, and no arrangement of it could. The simulator has no home pose to be
    // wrong about; it anchors sidereal time at connect so the mount starts on its configured
    // park coordinate, so its axis zero is a bookkeeping origin and not a counter on a mount.
    // The defect lived in the one place the two implementations were never independent.
    let hemisphere = Hemisphere::Northern;
    let lst = lst_hours(3.25);

    for branch in [Branch::Normal, Branch::ThroughThePole] {
        for ra_axis_degrees in [-179.0, -90.0, -12.5, 0.0, 47.0, 120.0] {
            for dec_axis_degrees in [-140.0_f64, -30.0, 30.0, 85.0] {
                let dec_axis_degrees = match branch {
                    Branch::Normal => dec_axis_degrees.abs(),
                    Branch::ThroughThePole => -dec_axis_degrees.abs(),
                };
                let mech = MechPosition {
                    ra_axis: axis_angle(ra_axis_degrees),
                    dec_axis: axis_angle(dec_axis_degrees),
                };

                // The driver's answer: counters through the decomposition.
                let driver = mech_to_sky(mech, lst, hemisphere);

                // The simulator's answer: seed its two axes with the mechanical state expressed
                // the way it holds it, hold them steady, and apply its own `RA = LST − HA`.
                // `AxisPlan` is the simulator's, not a re-implementation — a stopped plan is how
                // it represents an axis that is not being driven.
                let hour_angle = wrap_signed(lst.degrees() - driver.coords.ra.hours() * 15.0);
                let simulator_ra_axis = AxisPlan::steady(hour_angle, 0.0).position_at(0.0);
                let simulator_dec_axis =
                    AxisPlan::steady(driver.coords.dec.degrees(), 0.0).position_at(0.0);
                let simulator_ra_hours =
                    (lst.degrees() - simulator_ra_axis).rem_euclid(360.0) / 15.0;

                let label = format!("{branch:?} h={ra_axis_degrees} d={dec_axis_degrees}");
                let gap_ra = (simulator_ra_hours - driver.coords.ra.hours()).abs();
                assert!(
                    gap_ra.min(24.0 - gap_ra) * 15.0 * 3600.0 < 1e-6,
                    "{label}: the simulator says RA {simulator_ra_hours} h, the driver {} h",
                    driver.coords.ra.hours()
                );
                assert!(
                    (simulator_dec_axis - driver.coords.dec.degrees()).abs() < 1e-9,
                    "{label}: declination {simulator_dec_axis}° against {}°",
                    driver.coords.dec.degrees()
                );

                // The correspondence itself, stated as the equation rather than inferred from
                // the sky it produces: the simulator's hour-angle axis is the driver's
                // `s·(h + 90°)`, plus the half-turn past the pole. This is the assertion that
                // fails if the 90° is ever dropped from one side and not the other.
                let expected = wrap_signed(
                    hemisphere.sign() * (mech.ra_axis.degrees() + 90.0)
                        + match branch {
                            Branch::Normal => 0.0,
                            Branch::ThroughThePole => 180.0,
                        },
                );
                assert!(
                    (wrap_signed(simulator_ra_axis - expected)).abs() < 1e-9,
                    "{label}: the simulator's hour-angle axis is {simulator_ra_axis}°, and the \
                     driver's mechanics make it {expected}°"
                );
            }
        }
    }
}

fn the_constants_agree() {
    // The simulator's profile records the spike's measurements in degrees; this suite records
    // them in counts. A divergence here means one of them has been edited away from the mount.
    assert!((COUNTS_PER_REVOLUTION - f64::from(CPR)).abs() < f64::EPSILON);
    assert!((TIMER_FREQUENCY_HZ - f64::from(TIMER_HZ)).abs() < f64::EPSILON);
    assert!(
        (ARCSEC_PER_COUNT - scale().arcsec_per_count()).abs() < 1e-12,
        "the simulator says {ARCSEC_PER_COUNT}″ per count, the driver {}″",
        scale().arcsec_per_count()
    );
}

fn the_tracking_rates_agree() {
    // Same three rates, reached from opposite directions: the simulator quotes degrees per second
    // directly, the driver derives counts per second and a step period from the timer frequency.
    for mode in [
        TrackingMode::Sidereal,
        TrackingMode::Lunar,
        TrackingMode::Solar,
    ] {
        let simulator_deg_per_sec = tracking_rate(mode);
        let driver_deg_per_sec =
            rates().tracking(mode).expect("valid").get() * scale().degrees_per_count();
        let disagreement_arcsec_per_hour =
            (simulator_deg_per_sec - driver_deg_per_sec).abs() * 3600.0 * 3600.0;
        assert!(
            disagreement_arcsec_per_hour < 1e-6,
            "{mode:?}: the simulator turns {simulator_deg_per_sec} deg/s and the driver \
             {driver_deg_per_sec}, which is {disagreement_arcsec_per_hour}″ of drift an hour"
        );
    }
    // ...and the sidereal rate is the one the simulator's profile states independently.
    assert!((tracking_rate(TrackingMode::Sidereal) - SIDEREAL_DEG_PER_SEC).abs() < 1e-15);
}

fn the_slew_ladder_agrees() {
    // The ladder has two copies, so the one thing worth enforcing is that they are the *same*
    // ladder — otherwise a manual slew moves at one speed against the simulator and another
    // against the mount, and every timing built on the first is wrong against the second. Since
    // E16 the ladder splits by mechanism: the unbounded rungs must agree on the rate, and the
    // chunked rungs must agree that the cruise is the firmware's measured one per class (5,350
    // and 87,486 counts/s ÷ 104.7304 ≈ 51× and 835× at the fixture scale — the simulator states
    // the multiple, the driver states only the class, and this is where the two are tied).
    for speed in [SlewSpeed::Guide, SlewSpeed::Slow, SlewSpeed::Medium] {
        let SlewMethod::Unbounded(rate) = rates().slew_method(speed).expect("valid") else {
            panic!("{speed:?} must be an unbounded rung");
        };
        let simulator = slew_rate(speed);
        let driver = rate.get() * scale().degrees_per_count();
        assert!(
            (simulator - driver).abs() < 1e-12,
            "{speed:?}: simulator {simulator} deg/s, driver {driver} deg/s"
        );
    }
    for (speed, class, cruise_x_sidereal) in [
        (SlewSpeed::Fast, SpeedClass::Low, 51.0),
        (SlewSpeed::Max, SpeedClass::High, 835.0),
    ] {
        assert_eq!(
            rates().slew_method(speed).expect("valid"),
            SlewMethod::Chunked(class),
            "{speed:?}"
        );
        let simulator = slew_rate(speed) / SIDEREAL_DEG_PER_SEC;
        assert!(
            (simulator - cruise_x_sidereal).abs() < 1e-9,
            "{speed:?}: simulator says {simulator}× sidereal, the measured cruise is \
             {cruise_x_sidereal}×"
        );
    }
}

fn a_stopped_drive_climbs_and_a_tracking_one_holds() {
    // The direction lesson the simulator's author wrote down, held by both implementations
    // against the same fixture: one hour of sidereal time, an hour-angle axis that either turns
    // or does not, and the right ascension each reports.
    const ELAPSED_SECONDS: f64 = 3_600.0;
    let hemisphere = Hemisphere::Northern;
    let start_lst = lst_hours(7.0);
    let later_lst = Lst::from_degrees(start_lst.degrees() + ELAPSED_SECONDS * SIDEREAL_DEG_PER_SEC)
        .expect("finite");
    let target = radec(7.0, 40.0);

    let start = sky_to_mech(target, Branch::Normal, start_lst, hemisphere);

    // Stopped: the simulator's plan holds its axis angle, and so does the driver's counter. Both
    // must report a right ascension one sidereal hour higher, because the sky moved and the tube
    // did not.
    let stopped_plan = AxisPlan::steady(start.ra_axis.degrees(), 0.0);
    let simulator_axis = stopped_plan.position_at(ELAPSED_SECONDS);
    assert!(
        (simulator_axis - start.ra_axis.degrees()).abs() < 1e-12,
        "a stopped plan must not move its axis"
    );
    let driver_stopped = mech_to_sky(
        MechPosition {
            ra_axis: axis_angle(simulator_axis),
            dec_axis: start.dec_axis,
        },
        later_lst,
        hemisphere,
    );
    // 3,600 SI seconds is 3,600 x 360 / 86,164.0905 = 15.0410686 degrees of sky, which is
    // 1.0027379 *hours of right ascension* — the same 360.9856-degrees-a-solar-day fact
    // `astroctl-safety` tests from the sidereal-time side. Asserting a round 1.0 here would be
    // asserting that the sidereal day is 24 hours long.
    const HOURS_OF_RA_PER_SOLAR_HOUR: f64 = 1.002_737_909_35;
    let climbed = driver_stopped.coords.ra.hours() - target.ra.hours();
    assert!(
        (climbed - HOURS_OF_RA_PER_SOLAR_HOUR).abs() < 1e-6,
        "a stopped drive's right ascension climbed {climbed} h in an hour, against the \
         {HOURS_OF_RA_PER_SOLAR_HOUR} h the sky turns"
    );

    // Tracking: the simulator's plan turns the axis at the sidereal rate, and the driver reads
    // the same coordinate it started at.
    let tracking_plan = AxisPlan::steady(
        start.ra_axis.degrees(),
        tracking_rate(TrackingMode::Sidereal) * hemisphere.sign(),
    );
    let driver_tracking = mech_to_sky(
        MechPosition {
            ra_axis: axis_angle(tracking_plan.position_at(ELAPSED_SECONDS)),
            dec_axis: start.dec_axis,
        },
        later_lst,
        hemisphere,
    );
    let held = separation_arcsec(target, driver_tracking.coords);
    assert!(
        held < 0.001,
        "a tracking drive drifted {held}″ in an hour, and it should hold still"
    );

    // ...and the direction the driver would actually program for that plan is the one the plan's
    // positive rate implies, in the north and the south.
    assert_eq!(
        tracking_direction(Hemisphere::Northern),
        MotionDirection::Forward
    );
    assert_eq!(
        tracking_direction(Hemisphere::Southern),
        MotionDirection::Backward
    );
}

// -----------------------------------------------------------------------------------------
// The controller, end to end over the math
// -----------------------------------------------------------------------------------------

#[test]
fn a_goto_computed_by_the_math_is_programmable_by_the_controller() {
    // The join M3-T04 will make: the geometry produces two `Move`s, and each axis's controller
    // turns its own into a verified goto program. The point of asserting it here is that the two
    // halves were written against the same `Move` type and nothing else.
    let geometry = geometry(ANCHOR_LATITUDE);
    let from = AxisCounts {
        ra: Counts::new(HOME + 120_000).expect("fits"),
        dec: Counts::new(HOME + 900_000).expect("fits"),
    };
    let solution = geometry
        .goto(from, radec(9.0, 55.0), lst_hours(10.5))
        .expect("in range");

    for (axis, from_counts, mv) in [
        (Axis::Ra, from.ra, solution.ra()),
        (Axis::Dec, from.dec, solution.dec()),
    ] {
        let controller = controller(axis);
        let speed = controller.goto_speed_class(mv.magnitude().get());
        let program = controller
            .goto(from_counts, mv, speed)
            .expect("a non-zero move");

        // The destination the program computes is the one the solution computed.
        let expected = match axis {
            Axis::Ra => solution.destination().ra,
            Axis::Dec => solution.destination().dec,
        };
        assert_eq!(program.destination(), expected, "{axis:?}");

        // The readback expectation is absolute even though the writes are relative — the
        // asymmetry E14 measured, and the reason the program is computed against `from`.
        assert_eq!(program.expectation().target, expected, "{axis:?}");

        // And the arrival is within tolerance of itself, which is the trivial half of
        // slew-complete detection and the half a sign error breaks.
        assert!(controller.within_tolerance(expected, expected));
        assert!(!controller.within_tolerance(
            Counts::new(expected.get() + GOTO_TOLERANCE_COUNTS + 1).expect("fits"),
            expected
        ));
    }
}

#[test]
fn a_guide_pulse_moves_the_sky_the_way_the_direction_says() {
    // The end-to-end statement about guiding: a pulse north must raise the declination the
    // driver reports, on both pier sides and in both hemispheres. This is the correction that
    // reverses after a meridian flip, and getting it wrong means a guide loop that pushes the
    // star out of the frame — slowly, and only after the flip.
    let rate = GuideRate::new(1.0).expect("in range");
    let duration = Duration::from_secs(4);

    for latitude in [ANCHOR_LATITUDE, SANTIAGO_LATITUDE] {
        let hemisphere = Hemisphere::of_latitude(latitude);
        let geometry = geometry(latitude);
        for branch in [Branch::Normal, Branch::ThroughThePole] {
            let lst = lst_hours(3.0);
            let target = radec(3.5, 15.0);
            let start = geometry.counts_for(target, branch, lst);

            for direction in [Direction::North, Direction::South] {
                let motion = motor_direction(direction, branch, hemisphere);
                let pulse = controller(Axis::Dec)
                    .guide_pulse(rate, motion, duration)
                    .expect("valid");
                assert!(pulse.is_measurable());

                let after = AxisCounts {
                    dec: start.dec.after(pulse.offset()),
                    ..start
                };
                let moved = geometry.position(after, lst).coords.dec.degrees()
                    - geometry.position(start, lst).coords.dec.degrees();
                let wanted = if matches!(direction, Direction::North) {
                    1.0
                } else {
                    -1.0
                };
                assert!(
                    moved.signum() == wanted,
                    "{direction:?} at latitude {latitude} on the {branch:?} branch moved \
                     declination by {moved}°"
                );
                // Four seconds at the full sidereal rate is 4 × 104.7304 = 418.9 → 419 counts,
                // which is 60.2″.
                assert_eq!(pulse.offset().magnitude().get(), 419);
                assert!((moved.abs() * 3600.0 - 60.2).abs() < 0.1, "{moved}°");
            }
        }
    }
}
