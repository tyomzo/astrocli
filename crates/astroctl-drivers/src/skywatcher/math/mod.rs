//! Counters ↔ coordinates: SDD §5.2.3's position math, with no I/O in it (M3-T03).
//!
//! Pure functions and small `Copy` values. No port, no runtime, no clock — the same discipline
//! [`super::codec`] holds itself to, for the same reason: it is what lets the no-flip goto
//! property be checked over ten thousand random mount states in a unit test rather than argued
//! for in review.
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`angle`] | [`Lst`], [`HourAngle`], [`AxisAngle`], and [`AxisScale`] — the counts↔degrees arithmetic |
//! | [`mech`] | the mech↔sky decomposition, hemisphere, pier side, motor sense |
//! | [`target`] | resolving a target into two [`Move`](super::codec::Move)s, on the branch the mount is already on |
//!
//! # The seam SDD §5.2.3 asks for
//!
//! > "this module keeps the conversion behind `fn mech_to_sky(&self, counts: AxisCounts,
//! > lst: Lst) -> RaDec` so the upgrade is internal"
//!
//! [`MountGeometry::position`] is that function, with the pier side added to the return because
//! the same `RaDec` is two different mount states and `mount.position` (SDD §4.3) carries both.
//! **Local sidereal time is a parameter and is not computed here** — see [`Lst`] for why that is
//! architecture rather than laziness.
//!
//! # Nothing here is hardcoded from the mount
//!
//! Counts per revolution arrives in [`AxisScale`] from `:a`, per axis. 9,024,000 appears in this
//! module only inside `#[cfg(test)]`, labelled as the fixture PRD §4.2 says it is. That was the
//! design decision that contained the timer-frequency error (PRD §10: "its blast radius was
//! already limited by the design decision to read CPR/timer-freq from the mount at handshake"),
//! and it is the one this module inherits.

pub mod angle;
pub mod mech;
pub mod target;

pub use angle::{
    wrap_signed, wrap_turn, AxisAngle, AxisScale, HourAngle, Lst, ARCSEC_PER_DEGREE,
    DEGREES_PER_HOUR, DEGREES_PER_TURN,
};
pub use mech::{
    drifted_right_ascension, mech_to_sky, motor_direction, sky_to_mech, tracking_direction, Branch,
    Hemisphere, MechPosition, SkyPosition,
};
pub use target::{goto_solution, AxisCounts, GotoSolution};

use astroctl_core::config::SiteConfig;
use astroctl_core::types::RaDec;

use super::codec::{CountsPerRev, EncodeError};

/// What the position arithmetic can refuse.
///
/// Deliberately small. Almost everything in this module is total — the decomposition cannot fail,
/// the folds cannot fail, and a validated newtype cannot carry a `NaN` into them — so the three
/// variants here are the three places a caller can still hand over something meaningless.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum MathError {
    /// A `NaN` or an infinity where an angle was expected.
    ///
    /// Worth an error rather than a fold: `NaN.rem_euclid(360.0)` is `NaN`, and a `NaN` compares
    /// false against every limit — so an unchecked fold turns a bad input into a coordinate that
    /// passes every safety test in the node.
    #[error("{quantity} must be a finite number of degrees, got {value}")]
    NotFinite {
        /// What the value was meant to be.
        quantity: &'static str,
        /// What arrived.
        value: f64,
    },

    /// The mount reported zero counts per revolution, which every conversion divides by.
    #[error("the mount reported zero counts per revolution")]
    ZeroCountsPerRevolution,

    /// No move reaches the target without driving the counter across its own 24-bit boundary,
    /// where the counts↔angle correspondence breaks (see [`AxisScale::reachable_delta`]).
    #[error("no move from counter {from} reaches {to} without crossing the counter's 24-bit wrap")]
    UnreachableCounter {
        /// Where the axis is.
        from: u32,
        /// Where it was asked to go.
        to: u32,
    },

    /// An axis delta would not fit the 24-bit increment register.
    #[error(transparent)]
    Move(#[from] EncodeError),
}

/// One mount's geometry: what each axis counter means, and which way the sky turns over it.
///
/// The unit of configuration for everything in this module. It is `Copy` and holds three numbers,
/// so the motor controller can keep one per axis without lifetimes, and a test can build one from
/// a fixture in a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MountGeometry {
    ra: AxisScale,
    dec: AxisScale,
    hemisphere: Hemisphere,
}

impl MountGeometry {
    /// From the two `:a` replies and the observing hemisphere.
    ///
    /// Two scales rather than one because the mount answers `:a1` and `:a2` separately. They are
    /// equal on an HEQ5 — both axes reported 9,024,000 — and taking one number for both would be
    /// assuming that of every mount this driver ever meets, which is exactly the assumption the
    /// timer frequency punished.
    #[must_use]
    pub const fn new(ra: AxisScale, dec: AxisScale, hemisphere: Hemisphere) -> Self {
        Self {
            ra,
            dec,
            hemisphere,
        }
    }

    /// From the two `:a` replies and the configured site (PRD §8.1 `site.latitude`).
    ///
    /// # Errors
    /// [`MathError::ZeroCountsPerRevolution`] if either axis answered zero.
    pub fn from_handshake(
        ra: CountsPerRev,
        dec: CountsPerRev,
        site: &SiteConfig,
    ) -> Result<Self, MathError> {
        Ok(Self::new(
            AxisScale::new(ra)?,
            AxisScale::new(dec)?,
            Hemisphere::of_site(site),
        ))
    }

    /// The right-ascension axis scale.
    #[must_use]
    pub const fn ra_scale(self) -> AxisScale {
        self.ra
    }

    /// The declination axis scale.
    #[must_use]
    pub const fn dec_scale(self) -> AxisScale {
        self.dec
    }

    /// Which pole the polar axis is aimed at.
    #[must_use]
    pub const fn hemisphere(self) -> Hemisphere {
        self.hemisphere
    }

    /// The mechanical state two counters describe.
    #[must_use]
    pub fn mech(self, counts: AxisCounts) -> MechPosition {
        MechPosition {
            ra_axis: self.ra.angle_at(counts.ra),
            dec_axis: self.dec.angle_at(counts.dec),
        }
    }

    /// The counters a mechanical state corresponds to, canonicalised near home.
    #[must_use]
    pub fn counts(self, mech: MechPosition) -> AxisCounts {
        AxisCounts {
            ra: self.ra.counts_at(mech.ra_axis),
            dec: self.dec.counts_at(mech.dec_axis),
        }
    }

    /// **The SDD §5.2.3 seam**: two counters and a sidereal time in, a sky position out.
    ///
    /// Infallible, because it is the 1 Hz position poll and the poll must not be able to fail for
    /// an arithmetic reason.
    #[must_use]
    pub fn position(self, counts: AxisCounts, lst: Lst) -> SkyPosition {
        mech_to_sky(self.mech(counts), lst, self.hemisphere)
    }

    /// The counters that would put the telescope on `target` from a chosen pier side.
    #[must_use]
    pub fn counts_for(self, target: RaDec, branch: Branch, lst: Lst) -> AxisCounts {
        self.counts(sky_to_mech(target, branch, lst, self.hemisphere))
    }

    /// Resolve a goto: see [`goto_solution`] for the rule and the guarantee.
    ///
    /// # Errors
    /// [`MathError::Move`] if an axis delta will not fit the increment register.
    pub fn goto(
        self,
        from: AxisCounts,
        target: RaDec,
        lst: Lst,
    ) -> Result<GotoSolution, MathError> {
        goto_solution(from, target, lst, self.hemisphere, self.ra, self.dec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::hex::U24;
    use crate::skywatcher::codec::Counts;

    /// **A fixture** — what the operator's HEQ5 answered, per PRD §4.2.
    fn fixture_cpr() -> CountsPerRev {
        CountsPerRev(U24::new(9_024_000).expect("fits"))
    }

    fn site(latitude: f64) -> SiteConfig {
        SiteConfig {
            latitude,
            longitude: 10.7522,
            elevation: 100.0,
            timezone: "Europe/Oslo".to_owned(),
        }
    }

    #[test]
    fn the_geometry_takes_its_scale_from_the_handshake_and_its_pole_from_the_site() {
        let north = MountGeometry::from_handshake(fixture_cpr(), fixture_cpr(), &site(59.9139))
            .expect("valid");
        assert_eq!(north.hemisphere(), Hemisphere::Northern);
        assert_eq!(north.ra_scale().counts_per_revolution(), 9_024_000);

        let south = MountGeometry::from_handshake(fixture_cpr(), fixture_cpr(), &site(-33.4489))
            .expect("valid");
        assert_eq!(south.hemisphere(), Hemisphere::Southern);
    }

    #[test]
    fn a_mount_that_answers_zero_counts_per_revolution_never_produces_a_geometry() {
        assert_eq!(
            MountGeometry::from_handshake(CountsPerRev(U24::ZERO), fixture_cpr(), &site(0.0)),
            Err(MathError::ZeroCountsPerRevolution)
        );
        assert_eq!(
            MountGeometry::from_handshake(fixture_cpr(), CountsPerRev(U24::ZERO), &site(0.0)),
            Err(MathError::ZeroCountsPerRevolution)
        );
    }

    #[test]
    fn two_axes_with_different_counts_per_revolution_are_scaled_separately() {
        // The reason there are two. A mount whose declination gearing differs would, with one
        // shared scale, report a declination wrong in proportion — a smooth, plausible error that
        // a plate solve would blame on the pointing model.
        let dec = CountsPerRev(U24::new(4_512_000).expect("fits"));
        let geometry =
            MountGeometry::from_handshake(fixture_cpr(), dec, &site(59.9139)).expect("valid");
        let quarter_turn_of_each = AxisCounts {
            ra: Counts::new(0x0080_0000 + 9_024_000 / 4).expect("fits"),
            dec: Counts::new(0x0080_0000 + 4_512_000 / 4).expect("fits"),
        };
        let mech = geometry.mech(quarter_turn_of_each);
        assert!((mech.ra_axis.degrees() - 90.0).abs() < 1e-9);
        assert!((mech.dec_axis.degrees() - 90.0).abs() < 1e-9);
    }

    #[test]
    fn the_seam_round_trips_a_coordinate_through_two_counters() {
        // The property T-POS-1 names, at the seam the SDD names. One count is the budget; the
        // only rounding in the path is `counts_at`, which is half a count.
        let geometry = MountGeometry::from_handshake(fixture_cpr(), fixture_cpr(), &site(59.9139))
            .expect("valid");
        let lst = Lst::from_hours(21.5).expect("valid");
        let mut worst: u32 = 0;
        for branch in [Branch::Normal, Branch::ThroughThePole] {
            for ra_hours in [0.0, 5.5, 12.0, 18.75, 23.5] {
                for dec_degrees in [-85.0, -20.0, 0.0, 20.0, 85.0] {
                    let target = RaDec::from_parts(ra_hours, dec_degrees).expect("valid");
                    let counts = geometry.counts_for(target, branch, lst);
                    let back = geometry.position(counts, lst);
                    assert_eq!(back.pier_side, branch.pier_side());
                    let gap_ra = (back.coords.ra.hours() - ra_hours).abs();
                    let gap_ra = gap_ra.min(24.0 - gap_ra) * DEGREES_PER_HOUR;
                    let gap_dec = (back.coords.dec.degrees() - dec_degrees).abs();
                    let counts_off = geometry
                        .ra_scale()
                        .counts_in(gap_ra.max(gap_dec))
                        .abs()
                        .round() as u32;
                    worst = worst.max(counts_off);
                }
            }
        }
        assert!(worst <= 1, "worst round trip was {worst} counts");
    }
}
