//! Sky-survey diagnostic for the Layer 2 rig model (HANDOVER-2026-08-02 §3).
//!
//! Sweeps the sky above the horizon on an alt/az grid, asks the rig model for a verdict at each
//! pointing, and reports the refused fraction weighted by solid angle. The handover's acceptance
//! bar: single-digit percent before the model is trusted.
//!
//! Run: `cargo run -p astroctl-safety --example rig_survey`

use astroctl_core::config::{CameraGeometry, CounterweightGeometry, RigGeometry};
use astroctl_core::types::PierSide;
use astroctl_safety::rig::{Horizontal, RigModel};
use astroctl_safety::Site;

const ALT_STEP: f64 = 1.0;
const AZ_STEP: f64 = 2.0;

fn survey(label: &str, geometry: RigGeometry, site: Site) {
    let model = RigModel::new(Some(geometry), site).expect("geometry is Some");

    // Solid-angle-weighted tallies. A pointing is "unreachable" only when *both* pier sides
    // collide — a goto is free to pick the side that clears. "Worst case" is pier unknown,
    // which refuses if either side collides; that is what an operator with pier_side = unknown
    // would experience.
    let (mut total, mut both_blocked, mut either_blocked) = (0.0_f64, 0.0_f64, 0.0_f64);
    // Where the hard-refused sky is, by 10° altitude band: (blocked, total) weights.
    let mut bands = [(0.0_f64, 0.0_f64); 9];

    let mut alt = ALT_STEP / 2.0;
    while alt < 90.0 {
        let weight_band = alt.to_radians().cos();
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "alt < 90")]
        let band = &mut bands[(alt / 10.0) as usize];
        let mut az = AZ_STEP / 2.0;
        while az < 360.0 {
            let tube = Horizontal {
                altitude_degrees: alt,
                azimuth_degrees: az,
            };
            let west = model.collides(tube, Some(PierSide::West)).is_some();
            let east = model.collides(tube, Some(PierSide::East)).is_some();
            total += weight_band;
            band.1 += weight_band;
            if west && east {
                both_blocked += weight_band;
                band.0 += weight_band;
            }
            if west || east {
                either_blocked += weight_band;
            }
            az += AZ_STEP;
        }
        alt += ALT_STEP;
    }

    println!("== {label}");
    println!(
        "   refused (both pier sides blocked):   {:6.2}% of the sky",
        100.0 * both_blocked / total
    );
    println!(
        "   refused (either side, pier unknown): {:6.2}% of the sky",
        100.0 * either_blocked / total
    );
    for (i, (blocked, band_total)) in bands.iter().enumerate() {
        if *blocked > 0.0 {
            println!(
                "     alt {:2}–{:2}°: {:5.1}% of the band hard-blocked",
                i * 10,
                (i + 1) * 10,
                100.0 * blocked / band_total
            );
        }
    }

    // The zenith diagnostic from the handover, plus a few landmark pointings.
    for (name, alt, az) in [
        ("zenith", 90.0 - 1e-9, 0.0),
        ("pole (home pointing)", site.latitude_degrees, 0.0),
        ("due south, alt 30", 30.0, 180.0),
        ("due east, alt 10", 10.0, 90.0),
        ("horizon north", 0.0, 0.0),
    ] {
        let tube = Horizontal {
            altitude_degrees: alt,
            azimuth_degrees: az,
        };
        let west = model.collides(tube, Some(PierSide::West));
        let east = model.collides(tube, Some(PierSide::East));
        match (west, east) {
            (Some(w), Some(e)) => println!(
                "   {name:24} REFUSED both sides ({:?} at depth {:.0} mm)",
                w.what,
                w.depth_mm.max(e.depth_mm)
            ),
            (Some(c), None) | (None, Some(c)) => println!(
                "   {name:24} one side only ({:?}, depth {:.0} mm on the blocked side)",
                c.what, c.depth_mm
            ),
            (None, None) => println!("   {name:24} clear"),
        }
    }
    println!();
}

fn pole_bearing_sweep(label: &str, geometry: RigGeometry, site: Site) {
    // The dec-home diagnostic: pointing at the pole, sweep the RA bearing and print what the
    // model refuses and by how much. This is the pose an operator sits in at the start and end
    // of every session, so a refused band here is felt nightly.
    let model = RigModel::new(Some(geometry), site).expect("geometry");
    let pole = Horizontal {
        altitude_degrees: site.latitude_degrees,
        azimuth_degrees: 0.0,
    };
    println!("== bearing sweep at the pole pointing — {label}");
    let mut refused = 0;
    for b in (0..360).step_by(3) {
        if let Some(hit) = model.collides_with_dec_axis(pole, f64::from(b)) {
            refused += 3;
            if b % 15 == 0 {
                println!(
                    "   bearing {b:3}°: {:?}, penetration {:5.1} mm, {:4.0} mm below the axes",
                    hit.what, hit.penetration_mm, hit.depth_mm
                );
            }
        }
    }
    println!("   refused: ~{refused}° of the full RA turn\n");
}

fn main() {
    let site = Site {
        latitude_degrees: 54.687,
        longitude_degrees: 25.28,
    };

    // Operator measurements of 2026-08-02. Raw values: saddle 90 (dec shaft centre to saddle
    // surface) + 140 (saddle surface to tube centreline); base "630" tested under both readings
    // because the raw foot-to-foot figure was not recorded.
    let measured = RigGeometry {
        dec_axis_offset_mm: 165.0,
        tube_half_length_mm: 250.0,
        tube_radius_mm: 70.0,
        saddle_offset_mm: 230.0,
        head_height_mm: 1100.0,
        mount_body_height_mm: 250.0,
        top_radius_mm: 80.0,
        base_radius_mm: 630.0,
        mount_axis_offset_mm: 60.0,
        camera: Some(CameraGeometry {
            along_tube_mm: 140.0,
            reach_mm: 300.0,
            radius_mm: 50.0,
        }),
        counterweight: Some(CounterweightGeometry {
            length_mm: 350.0,
            radius_mm: 75.0,
        }),
    };

    survey("measured rig (base radius 630, small OTA)", measured, site);
    survey(
        "cross-check: base radius 577 (from foot-to-foot 1000)",
        RigGeometry {
            base_radius_mm: 1000.0 / 3.0_f64.sqrt(),
            ..measured
        },
        site,
    );
    // The OTA is a short Newtonian: the focuser and camera protrude ~300 mm radially from the
    // side of the tube. The capsule is rotationally symmetric, so the protrusion is covered by
    // inflating the radius — the operator's call: 250 mm.
    survey(
        "capsule fattened for the radial camera (r 250)",
        RigGeometry {
            tube_radius_mm: 250.0,
            ..measured
        },
        site,
    );

    // What the model refuses at dec-home as the RA axis turns — with the full configured rig,
    // without the counterweight, and with the bare tube radius, so a refusal names its culprit.
    let configured = RigGeometry {
        tube_radius_mm: 160.0,
        ..measured
    };
    pole_bearing_sweep("configured rig (r 160, counterweight on)", configured, site);
    pole_bearing_sweep(
        "counterweight removed",
        RigGeometry {
            counterweight: None,
            ..configured
        },
        site,
    );
    pole_bearing_sweep(
        "bare tube (r 70), counterweight on",
        RigGeometry {
            tube_radius_mm: 70.0,
            ..measured
        },
        site,
    );

    // How much radius the high sky tolerates: the knee is where zenith access is lost.
    println!("== capsule radius vs hard-blocked sky");
    for r in [70.0, 100.0, 130.0, 160.0, 170.0, 180.0, 190.0, 220.0, 250.0] {
        let g = RigGeometry {
            tube_radius_mm: r,
            ..measured
        };
        let model = RigModel::new(Some(g), site).expect("geometry is Some");
        let zenith = Horizontal {
            altitude_degrees: 90.0 - 1e-9,
            azimuth_degrees: 0.0,
        };
        let mut blocked = 0.0_f64;
        let mut total = 0.0_f64;
        let mut alt = ALT_STEP / 2.0;
        while alt < 90.0 {
            let w = alt.to_radians().cos();
            let mut az = AZ_STEP / 2.0;
            while az < 360.0 {
                let tube = Horizontal {
                    altitude_degrees: alt,
                    azimuth_degrees: az,
                };
                let both = model.collides(tube, Some(PierSide::West)).is_some()
                    && model.collides(tube, Some(PierSide::East)).is_some();
                total += w;
                if both {
                    blocked += w;
                }
                az += AZ_STEP;
            }
            alt += ALT_STEP;
        }
        println!(
            "   r {r:3.0} mm: {:5.2}% hard-blocked, zenith {}",
            100.0 * blocked / total,
            if model.collides(zenith, None).is_none() {
                "clear"
            } else {
                "REFUSED"
            }
        );
    }
}
