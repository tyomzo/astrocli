//! The collision model, checked on properties rather than on invented numbers.
//!
//! An early draft of this file asserted things like "the zenith must be clear" against a tripod
//! nobody had measured. That is fitting the rig to the test: the assertion passes or fails on my
//! guess at a leg splay, not on whether the geometry is right. What is asserted here instead are
//! properties that hold for *any* consistent rig — symmetry, monotonicity, which obstacle is
//! reported, and that the pose (not just the pointing) is what decides.

use astroctl_core::config::{CameraGeometry, CounterweightGeometry};

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
    mount_axis_offset_mm: 0.0,
    head_axis_angle_degrees: 90.0,
    counterweight: None,
    camera: None,
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
    // The pose the 2026-08-01 strike ended in. Whatever the tripod's dimensions, a tube long
    // enough to reach past its own head height, pointing into the ground from a mount standing
    // on that ground, has to be refused. (RIG's own 450 mm half-tube genuinely clears here — the
    // saddle stack holds it 360 mm out from the polar axis and its tip 800 mm off the ground —
    // which the sideways-saddle bug of 2026-08-02 masked by holding the model's tube 230 mm
    // closer to the pier than the metal.)
    let long = RigModel::new(
        Some(RigGeometry {
            tube_half_length_mm: 1_400.0,
            ..RIG
        }),
        VILNIUS,
    )
    .expect("geometry");
    let hit = long
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

/// A pointing built from equatorial coordinates, so a test can state an hour angle and get the
/// alt/az the model consumes — the inverse arithmetic shares nothing with the code under test.
fn pointing_from_equatorial(ha_degrees: f64, dec_degrees: f64, site: Site) -> Horizontal {
    let (sin_ha, cos_ha) = ha_degrees.to_radians().sin_cos();
    let (sin_dec, cos_dec) = dec_degrees.to_radians().sin_cos();
    let (sin_lat, cos_lat) = site.latitude_degrees.to_radians().sin_cos();
    let altitude = (sin_dec * sin_lat + cos_dec * cos_lat * cos_ha)
        .clamp(-1.0, 1.0)
        .asin()
        .to_degrees();
    let azimuth = (-cos_dec * sin_ha)
        .atan2(sin_dec * cos_lat - cos_dec * sin_lat * cos_ha)
        .to_degrees()
        .rem_euclid(360.0);
    Horizontal {
        altitude_degrees: altitude,
        azimuth_degrees: azimuth,
    }
}

#[test]
fn a_known_dec_axis_agrees_with_the_pier_side_it_implies() {
    // Away from the pole the bearing and the pier side carry the same fact in different units:
    // the declination axis sits a quarter-turn from the tube's hour circle, on the side the
    // branch names — `b = HA − 90°` on the normal branch (pier west), `HA + 90°` past the pole
    // (northern hemisphere). If the two paths ever disagree, one of the sign conventions is
    // inverted, which is exactly the class of defect ADR-14 exists to keep out.
    let m = model();
    for ha_step in 0..12 {
        let ha = f64::from(ha_step) * 30.0 - 180.0;
        for dec in [-40.0, 0.0, 30.0, 60.0, 85.0] {
            let p = pointing_from_equatorial(ha, dec, VILNIUS);
            for (pier, bearing) in [
                (PierSide::West, ha - 90.0),
                (PierSide::East, ha + 90.0),
            ] {
                assert_eq!(
                    m.collides(p, Some(pier)).is_some(),
                    m.collides_with_dec_axis(p, bearing).is_some(),
                    "pier {pier:?} and its bearing disagreed at HA {ha}°, dec {dec}°"
                );
            }
        }
    }
}

#[test]
fn at_the_pole_the_reported_axis_replaces_the_sweep() {
    // The hardware fact of 2026-08-02: with only a pointing, home is judged by the worst of
    // every pose sharing it, and the mount cannot move. The bearing is the missing fact. A tube
    // long enough that the sweep refuses the pole must be *clear* at bearing 0 — counterweight
    // down, the pose the mount actually parks in — and still refused at bearing 180°, saddle
    // down, because knowing the pose removes the guess and not the protection.
    let long = RigModel::new(
        Some(RigGeometry {
            tube_half_length_mm: 1_000.0,
            ..RIG
        }),
        VILNIUS,
    )
    .expect("geometry");
    let pole = pointing(VILNIUS.latitude_degrees, 0.0);
    assert!(
        long.collides(pole, None).is_some(),
        "the sweep must refuse a tube this long at the pole"
    );
    assert!(
        long.collides_with_dec_axis(pole, 0.0).is_none(),
        "the home pose, counterweight down, is clear — refusing it is the 2026-08-02 prison"
    );
    assert!(
        long.collides_with_dec_axis(pole, 180.0).is_some(),
        "saddle down at the pole hangs this tube into the legs and must still be refused"
    );
}

#[test]
fn a_camera_is_never_safer_and_reaches_along_the_saddle_direction() {
    // Two properties in one sweep. Adding the stack can only add refusals; and the stack points
    // along +d̂ — the saddle side, "in front of the mount" — so at the pole with the saddle
    // straight down it must reach *below* the tube and hit things a bare rig clears. The stub
    // tube isolates it, the same trick the counterweight and ground tests use.
    let stub = RigGeometry {
        tube_half_length_mm: 50.0,
        tube_radius_mm: 30.0,
        saddle_offset_mm: 50.0,
        ..RIG
    };
    let with_camera = |reach: f64| {
        RigModel::new(
            Some(RigGeometry {
                camera: Some(CameraGeometry {
                    along_tube_mm: 0.0,
                    reach_mm: reach,
                    radius_mm: 50.0,
                }),
                ..stub
            }),
            VILNIUS,
        )
        .expect("geometry")
    };
    let bare = RigModel::new(Some(stub), VILNIUS).expect("geometry");
    let long = with_camera((RIG.head_height_mm + 100.0) / VILNIUS.latitude_degrees.to_radians().cos());

    // Never safer, over the sweep the other parts use.
    for step in 0..=36 {
        let altitude = 90.0 - f64::from(step) * 5.0;
        for azimuth in [0.0, 90.0, 180.0, 270.0] {
            let p = pointing(altitude, azimuth);
            if bare.collides(p, None).is_some() {
                assert!(
                    long.collides(p, None).is_some(),
                    "adding a camera cleared a pose that collided at {altitude}°/{azimuth}°"
                );
            }
        }
    }
    // Reaches along +d̂: saddle-down at the pole sends a long-enough stack into the ground,
    // where the bare stub rig is clear.
    let pole = pointing(VILNIUS.latitude_degrees, 0.0);
    assert!(
        bare.collides_with_dec_axis(pole, 180.0).is_none(),
        "the stub rig must clear saddle-down at the pole, or the camera is not what is tested"
    );
    assert!(
        long.collides_with_dec_axis(pole, 180.0).is_some(),
        "a stack reaching past head height was cleared pointing at the ground"
    );
}

#[test]
fn the_tripod_stands_on_the_azimuth_axis_and_not_on_the_crossing() {
    // The mount's third axis: on a real GEM head the RA∩DEC crossing hangs forward of the
    // vertical column the tripod is centred on (~60 mm on the operator's, 2026-08-03). The
    // property: growing the offset slides the cone away under the crossing, so the pose whose
    // tube hangs on the crossing's own side of the column must gain room — its penetration
    // shrinks — while offset 0 must reproduce the old model exactly (every other test in this
    // file runs at 0 and is that assertion).
    let at = |offset: f64| {
        RigModel::new(
            Some(RigGeometry {
                tube_half_length_mm: 1_000.0,
                mount_axis_offset_mm: offset,
                ..RIG
            }),
            VILNIUS,
        )
        .expect("geometry")
    };
    let pole = pointing(VILNIUS.latitude_degrees, 0.0);
    // Saddle straight down at the pole pointing: the under-the-pier pose, tube low end on the
    // north side of the column.
    let centred = at(0.0)
        .collides_with_dec_axis(pole, 180.0)
        .expect("this rig collides under the pier with the column on the crossing");
    let offset = at(120.0)
        .collides_with_dec_axis(pole, 180.0)
        .map_or(0.0, |hit| hit.penetration_mm);
    assert!(
        offset < centred.penetration_mm,
        "moving the column back {offset} vs {} did not open room on the crossing's side",
        centred.penetration_mm
    );
}

#[test]
fn a_leaning_head_boss_carries_the_tripod_out_from_under_the_crossing() {
    // The head's boss descends toward the pole side ("60 degrees other direction" — the
    // operator corrected the first guess), so its foot — and the tripod standing under it —
    // lands poleward of the crossing, *toward* where a short tube swings at the under-the-pier
    // pose. On the operator-shaped rig the lean therefore CLOSES room there; 90° must reproduce
    // the old plumb-line model exactly (every other test here runs at 90). Not asserted for a
    // tube spanning both sides of the cone, where the arms trade places.
    let short_rig = RigGeometry {
        dec_axis_offset_mm: 165.0,
        saddle_offset_mm: 230.0,
        tube_half_length_mm: 250.0,
        tube_radius_mm: 80.0,
        mount_axis_offset_mm: 60.0,
        ..RIG
    };
    let at = |angle: f64| {
        RigModel::new(
            Some(RigGeometry {
                head_axis_angle_degrees: angle,
                ..short_rig
            }),
            VILNIUS,
        )
        .expect("geometry")
    };
    let pole = pointing(VILNIUS.latitude_degrees, 0.0);
    let plumb = at(90.0)
        .collides_with_dec_axis(pole, 180.0)
        .expect("the short rig collides under the pier when the boss is taken as vertical");
    let leaning = at(60.0)
        .collides_with_dec_axis(pole, 180.0)
        .expect("with the tripod's foot poleward the tube is deeper in, not clear");
    assert!(
        leaning.penetration_mm > plumb.penetration_mm,
        "the poleward lean ({} mm) did not close room vs plumb ({} mm)",
        leaning.penetration_mm,
        plumb.penetration_mm
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
fn penetration_deepens_as_the_pose_does() {
    // The escape rule's load-bearing property: `penetration_mm` must order colliding poses by
    // how bad they are, or "permit what shrinks it" would permit the wrong motions. The tripod
    // shell is not monotone along a sweep (its interior is free space — see the longer-tube
    // test), but the ground is: a tube tipping further below the horizon reaches deeper into it,
    // whatever the rig's dimensions. The pencil-thin tripod isolates the ground case.
    let m = RigModel::new(
        Some(RigGeometry {
            top_radius_mm: 1.0,
            base_radius_mm: 2.0,
            tube_half_length_mm: 1_500.0,
            ..RIG
        }),
        VILNIUS,
    )
    .expect("geometry");
    let mut last: Option<f64> = None;
    for altitude in [-70.0, -80.0, -90.0] {
        let hit = m
            .collides(pointing(altitude, 180.0), Some(PierSide::West))
            .unwrap_or_else(|| panic!("a tube this long at {altitude}° reaches the ground"));
        assert_eq!(hit.what, Obstacle::Ground);
        assert!(hit.penetration_mm > 0.0, "a collision penetrates by definition");
        if let Some(previous) = last {
            assert!(
                hit.penetration_mm > previous,
                "tipping lower shrank the penetration: {previous} → {} at {altitude}°",
                hit.penetration_mm
            );
        }
        last = Some(hit.penetration_mm);
    }
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
