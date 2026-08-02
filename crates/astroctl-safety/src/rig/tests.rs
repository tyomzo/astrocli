//! The collision model, checked on properties rather than on invented numbers.
//!
//! An early draft of this file asserted things like "the zenith must be clear" against a tripod
//! nobody had measured. That is fitting the rig to the test: the assertion passes or fails on my
//! guess at a leg splay, not on whether the geometry is right. What is asserted here instead are
//! properties that hold for *any* consistent rig — symmetry, monotonicity, which obstacle is
//! reported, and that the pose (not just the pointing) is what decides.

use astroctl_core::config::CounterweightGeometry;

use super::*;

/// A rig in the shape of the operator's, with the two numbers they gave and placeholders for the
/// rest. **The tripod figures are unmeasured** and are here to exercise the arithmetic, not to
/// describe anybody's tripod — which is why `mount.geometry` ships absent.
const RIG: RigGeometry = RigGeometry {
    dec_axis_offset_mm: 180.0,
    tube_half_length_mm: 450.0,
    tube_radius_mm: 120.0,
    saddle_offset_mm: 180.0,
    head_height_mm: 1250.0,
    mount_body_height_mm: 250.0,
    top_radius_mm: 80.0,
    base_radius_mm: 650.0,
    counterweight: None,
};

const VILNIUS: Site = Site {
    latitude_degrees: 54.6872,
    longitude_degrees: 25.2797,
};

fn model() -> RigModel {
    RigModel::new(Some(RIG), VILNIUS).expect("geometry configured")
}

fn pointing(altitude_degrees: f64, azimuth_degrees: f64) -> Horizontal {
    Horizontal {
        altitude_degrees,
        azimuth_degrees,
    }
}

#[test]
fn no_geometry_configured_means_no_collision_limit() {
    assert!(
        RigModel::new(None, VILNIUS).is_none(),
        "an unmeasured rig must get no limit rather than a guessed one"
    );
}

#[test]
fn a_tube_driven_straight_down_is_refused() {
    // The pose the 2026-08-01 strike ended in. Whatever the tripod's dimensions, a tube pointing
    // into the ground from a mount standing on that ground has to be refused.
    let hit = model()
        .collides(pointing(-90.0, 0.0), Some(PierSide::West))
        .expect("driving the tube into the ground must be refused");
    assert!(
        hit.depth_mm > 0.0,
        "the offending point should be below the axes, got {} mm",
        hit.depth_mm
    );
}

#[test]
fn the_verdict_depends_on_the_mechanical_pose_and_not_only_the_pointing() {
    // The same sky direction is two different mechanical poses which put the tube on opposite
    // sides of the mount. This is the whole reason Layer 2 needs Layer 1: a model given only the
    // pointing direction would be forced to answer identically for both, and one of the two
    // answers would be wrong.
    let m = model();
    let mut differed = false;
    for altitude in [0.0, 10.0, 20.0, 30.0, 40.0] {
        for azimuth in [0.0, 90.0, 180.0, 270.0] {
            let p = pointing(altitude, azimuth);
            if m.collides(p, Some(PierSide::West)) != m.collides(p, Some(PierSide::East)) {
                differed = true;
            }
        }
    }
    assert!(
        differed,
        "pier side changed no verdict anywhere, so the model is ignoring the mechanical pose"
    );
}

#[test]
fn an_unknown_pier_side_is_refused_if_either_side_would_be() {
    // Not knowing which side the tube is on cannot make a pose safe.
    let m = model();
    for altitude in [-10.0, 0.0, 15.0, 45.0, 80.0] {
        let p = pointing(altitude, 180.0);
        let either = m.collides(p, Some(PierSide::West)).is_some()
            || m.collides(p, Some(PierSide::East)).is_some();
        assert_eq!(
            m.collides(p, None).is_some(),
            either,
            "unknown pier side disagreed with the worst of the two at altitude {altitude}"
        );
    }
}

#[test]
fn a_longer_tube_is_never_safer() {
    // The monotonicity that actually holds. Altitude monotonicity does *not*: a tube centred on
    // the axes hangs its lower end 450 mm down when it points at the zenith and lifts both ends to
    // axis height when it points at the horizon, so it can collide at the zenith, come clear near
    // the horizon, and collide again pointing down. An early draft of this file asserted the
    // opposite and the model was right — worth recording, because "tipping down is always worse"
    // is exactly the intuition somebody will bring to this code next.
    let short = model();
    let long = RigModel::new(
        Some(RigGeometry {
            tube_half_length_mm: RIG.tube_half_length_mm * 2.0,
            ..RIG
        }),
        VILNIUS,
    )
    .expect("geometry");
    for step in 0..=36 {
        let altitude = 90.0 - f64::from(step) * 5.0;
        for azimuth in [0.0, 90.0, 180.0, 270.0] {
            let p = pointing(altitude, azimuth);
            if short.collides(p, None).is_some() {
                assert!(
                    long.collides(p, None).is_some(),
                    "doubling the tube cleared a pose that collided at {altitude}°/{azimuth}°"
                );
            }
        }
    }
}

#[test]
fn the_home_pose_is_swept_rather_than_guessed() {
    // At the pole every right-ascension angle produces the same pointing, so the pose does not
    // determine where the assembly is and the model sweeps. The property that must hold is that
    // the swept answer is the *worst* one, not an arbitrary one — so a rig whose tube is long
    // enough to reach the legs from the home pose must be refused there.
    let reaching = RigModel::new(
        Some(RigGeometry {
            tube_half_length_mm: 2_000.0,
            ..RIG
        }),
        VILNIUS,
    )
    .expect("geometry");
    assert!(
        reaching.collides(pointing(54.6872, 0.0), None).is_some(),
        "a tube long enough to reach the ground was cleared at the singular home pose"
    );
}

#[test]
fn a_counterweight_is_never_safer_and_an_unmeasured_one_changes_nothing() {
    // Adding a part to the moving assembly can only add refusals, and leaving it unmeasured must
    // leave the verdicts exactly as they were — the same no-guessed-limits rule as the rig itself.
    let bare = model();
    let with_shaft = RigModel::new(
        Some(RigGeometry {
            counterweight: Some(CounterweightGeometry {
                length_mm: 400.0,
                radius_mm: 100.0,
            }),
            ..RIG
        }),
        VILNIUS,
    )
    .expect("geometry");
    for step in 0..=36 {
        let altitude = 90.0 - f64::from(step) * 5.0;
        for azimuth in [0.0, 90.0, 180.0, 270.0] {
            let p = pointing(altitude, azimuth);
            if bare.collides(p, None).is_some() {
                assert!(
                    with_shaft.collides(p, None).is_some(),
                    "adding a counterweight cleared a pose that collided at {altitude}°/{azimuth}°"
                );
            }
        }
    }
}

#[test]
fn a_counterweight_long_enough_to_reach_the_ground_is_refused() {
    // A near-pointlike tube isolates the shaft, the same way the pencil-thin tripod isolates the
    // ground below. Pointing at the horizon due east puts the declination axis in the meridian
    // plane, the steepest it can lean at this latitude — cos φ below horizontal — so a shaft
    // longer than head height by that factor reaches the ground on the side that swings it down.
    let stub_tube = RigGeometry {
        tube_half_length_mm: 50.0,
        tube_radius_mm: 30.0,
        saddle_offset_mm: 50.0,
        ..RIG
    };
    let reach = (RIG.head_height_mm + 1.0) / VILNIUS.latitude_degrees.to_radians().cos();
    let with_shaft = RigModel::new(
        Some(RigGeometry {
            counterweight: Some(CounterweightGeometry {
                length_mm: reach,
                radius_mm: 75.0,
            }),
            ..stub_tube
        }),
        VILNIUS,
    )
    .expect("geometry");
    let east_horizon = pointing(0.0, 90.0);
    assert!(
        RigModel::new(Some(stub_tube), VILNIUS)
            .expect("geometry")
            .collides(east_horizon, None)
            .is_none(),
        "the shaftless rig must clear this pose, or the shaft is not what is being tested"
    );
    assert!(
        with_shaft.collides(east_horizon, None).is_some(),
        "a shaft reaching the ground was cleared at the east horizon"
    );
}

#[test]
fn the_southern_hemisphere_polar_axis_points_south_and_up() {
    // `s·(0, cos φ, sin φ)` has to right *both* components below the equator. If it did not, the
    // polar axis would point below the horizon and every pose would be judged against a mount
    // buried in the ground — which would pass a naive "does it refuse things" test.
    let santiago = Site {
        latitude_degrees: -33.4489,
        longitude_degrees: -70.6693,
    };
    let m = RigModel::new(Some(RIG), santiago).expect("geometry");
    assert!(m.polar.u > 0.0, "the elevated pole is above the horizon");
    assert!(
        m.polar.n < 0.0,
        "in the south it is due south, not due north"
    );
}

#[test]
fn the_ground_is_reported_separately_from_the_tripod() {
    // Different obstacles, different operator remedies: "you are about to reach the legs" and "the
    // rig is below its own feet, your head height is wrong" are not the same sentence. A pencil-
    // thin tripod isolates the ground case.
    let thin = RigModel::new(
        Some(RigGeometry {
            top_radius_mm: 1.0,
            base_radius_mm: 2.0,
            // Long enough that the tube actually reaches the floor; at the shipped half-length it
            // stops 800 mm short of it, which is the point of having a head height at all.
            tube_half_length_mm: 1_500.0,
            ..RIG
        }),
        VILNIUS,
    )
    .expect("geometry");
    let hit = thin
        .collides(pointing(-90.0, 0.0), Some(PierSide::West))
        .expect("the tube reaches the ground");
    assert_eq!(hit.what, Obstacle::Ground);
}
