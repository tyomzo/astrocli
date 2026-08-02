//! Equatorial → horizontal, for the configured observing site (MNT-03, SDD §5.2.3, §5.4).
//!
//! # One implementation, two consumers
//!
//! [`horizontal`] is called by the altitude limit inside [`SafeMount`](crate::SafeMount) *and* by
//! the `mount.position` event that shows the operator where the telescope is pointing. That is
//! deliberate and it is the reason this lives here rather than in the API layer: with two
//! implementations, a display bug and a limit bug can disagree about whether a target is up, and
//! the operator would be looking at a number that says 20° while the node refuses the slew at 12°.
//! One function makes that disagreement impossible rather than unlikely.
//!
//! # What this is, and what Phase 2a replaces it with
//!
//! Phase 1 is the plain spherical transform: Greenwich mean sidereal time from the system clock,
//! plus the site longitude, gives local sidereal time; LST minus right ascension gives the hour
//! angle; the rest is one spherical triangle. It has no refraction, no nutation, no aberration,
//! no proper motion and no polar motion, and it assumes UT1 ≈ UTC.
//!
//! Measured against astropy (which wraps the same liberfa Phase 2a will bind) over the fixture
//! set below, the worst disagreement is **0.23 arcmin**, and the largest single term is the
//! equation of the equinoxes — mean sidereal time rather than apparent, worth up to about 1.1 s
//! of time, i.e. 0.28′ of hour angle. That is invisible on a horizon limit whose configured
//! values are whole degrees, and it is three orders of magnitude away from mattering to a
//! plate solve, which is why the solve pipeline does not use this and Phase 2a exists.
//!
//! **TODO (SDD §5.2.3, Phase 2a):** replace the body of [`horizontal`] with the erfa
//! apparent-place pipeline from `astroctl-planning`, keeping this signature. Every caller is
//! written against the signature and none against the arithmetic, so the swap is internal — and
//! the fixture test below becomes the parity suite ADD §10 asks for, with the tolerance tightened
//! from arcminutes to arcseconds on the same day.

use astroctl_core::config::SiteConfig;
use astroctl_core::types::{AltAz, RaDec};
use chrono::{DateTime, Utc};

/// Degrees of hour angle per hour of right ascension.
const DEGREES_PER_HOUR: f64 = 15.0;

/// Degrees the sky turns in one minute of time — `meridian_limit_minutes` is in minutes of time,
/// and this is what converts it into the hour angle the watch compares against.
pub const DEGREES_PER_MINUTE_OF_TIME: f64 = DEGREES_PER_HOUR / 60.0;

/// Days from the J2000.0 epoch to the Unix epoch, negated: `days_since_j2000 = unix_days -
/// 10957.5`. Written as one constant rather than `jd - 2_451_545.0` so the subtraction of two
/// numbers near 2.45 million never happens in `f64`.
const UNIX_DAYS_TO_J2000: f64 = 10_957.5;

/// Milliseconds in a day.
const MILLIS_PER_DAY: f64 = 86_400_000.0;

/// The observing site, as this transform needs it (`site.latitude` / `site.longitude`).
///
/// Elevation is deliberately absent: it changes a star's apparent altitude only through
/// refraction and diurnal parallax, neither of which Phase 1 models. Carrying a field the
/// arithmetic ignores would imply it was being used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Site {
    /// Degrees north, `[-90, 90]` — validated at config load (SDD §4.4).
    pub latitude_degrees: f64,
    /// Degrees **east**, `[-180, 180]`. The sign is the classic trap: a site west of Greenwich
    /// has a negative longitude here, and getting it backwards moves the whole sky by twice the
    /// site's longitude — for Vilnius, 25.3° of hour angle, which is close to an hour and forty
    /// minutes of the
    /// wrong sky and looks exactly like a mount that is not tracking.
    pub longitude_degrees: f64,
}

impl Site {
    /// The site an operator configured.
    #[must_use]
    pub const fn from_config(site: &SiteConfig) -> Self {
        Self {
            latitude_degrees: site.latitude,
            longitude_degrees: site.longitude,
        }
    }
}

/// Where `target` is in the sky as seen from `site` at `at` (MNT-03).
///
/// Azimuth is measured from north through east, matching
/// [`AzDegrees`](astroctl_core::types::AzDegrees).
///
/// See the module docs for the accuracy this has and for the Phase 2a swap that keeps this exact
/// signature.
#[must_use]
pub fn horizontal(target: RaDec, site: Site, at: DateTime<Utc>) -> AltAz {
    let hour_angle = hour_angle_degrees(target, site, at).to_radians();
    let declination = target.dec.degrees().to_radians();
    let latitude = site.latitude_degrees.to_radians();

    let (sin_dec, cos_dec) = declination.sin_cos();
    let (sin_lat, cos_lat) = latitude.sin_cos();
    let (sin_ha, cos_ha) = hour_angle.sin_cos();

    // `clamp` before `asin`: the product of eight trig calls can land a hair outside [-1, 1] at
    // the poles, and `asin` of 1.0000000000000002 is NaN — which would propagate into the limit
    // comparison as "not less than the minimum" and pass a target that is straight down.
    let altitude = (sin_dec * sin_lat + cos_dec * cos_lat * cos_ha)
        .clamp(-1.0, 1.0)
        .asin()
        .to_degrees();
    let azimuth = (-cos_dec * sin_ha)
        .atan2(sin_dec * cos_lat - cos_dec * sin_lat * cos_ha)
        .to_degrees();

    AltAz::from_parts(altitude, azimuth).unwrap_or_else(|_| {
        // Unreachable: `asin` is bounded to [-90, 90] and `AzDegrees` normalises any finite
        // value, so only a NaN reaches here and `RaDec`'s constructors already refuse those.
        //
        // Not an `expect`. This is the safety path: a panic here takes the node down while it is
        // deciding whether a slew is legal, and the fallback is chosen to fail *closed* — an
        // altitude of −90° is below every configured minimum, so a coordinate this function
        // could not make sense of refuses the motion rather than authorising it.
        AltAz::from_parts(-90.0, 0.0).unwrap_or_else(|_| unreachable!("−90°/0° is a direction"))
    })
}

/// Hour angle of `target` in degrees, folded into `[-180, 180)`.
///
/// Negative is east of the meridian (rising), positive is west (setting, and on a German
/// equatorial mount the direction in which the tube approaches the pier — MNT-16).
#[must_use]
pub fn hour_angle_degrees(target: RaDec, site: Site, at: DateTime<Utc>) -> f64 {
    let lst = local_sidereal_degrees(site, at);
    wrap_signed(lst - target.ra.hours() * DEGREES_PER_HOUR)
}

/// Local sidereal time in degrees, `[0, 360)` — SDD §5.2.3's "system clock + site longitude".
#[must_use]
pub fn local_sidereal_degrees(site: Site, at: DateTime<Utc>) -> f64 {
    (greenwich_mean_sidereal_degrees(at) + site.longitude_degrees).rem_euclid(360.0)
}

/// Greenwich mean sidereal time in degrees, `[0, 360)`.
///
/// The IAU 1982 series as given by Meeus (*Astronomical Algorithms*, eq. 12.4). Mean, not
/// apparent: the equation of the equinoxes needs a nutation model, which is what Phase 2a's erfa
/// binding brings. See the module docs for what that costs here.
fn greenwich_mean_sidereal_degrees(at: DateTime<Utc>) -> f64 {
    let days = days_since_j2000(at);
    let centuries = days / 36_525.0;
    (280.460_618_37 + 360.985_647_366_29 * days + 0.000_387_933 * centuries * centuries
        - centuries * centuries * centuries / 38_710_000.0)
        .rem_euclid(360.0)
}

/// Days elapsed since J2000.0 (2000-01-01T12:00:00Z), fractional.
///
/// Computed from the Unix timestamp rather than by building a Julian date and subtracting
/// 2 451 545: those two numbers agree to six significant figures, and the subtraction throws away
/// the precision the sidereal series needs.
fn days_since_j2000(at: DateTime<Utc>) -> f64 {
    // `timestamp_millis` is exact in `f64` for any date this system will ever see: it stays under
    // 2^53 milliseconds until the year 287396.
    at.timestamp_millis() as f64 / MILLIS_PER_DAY - UNIX_DAYS_TO_J2000
}

/// Folds an angle into `[-180, 180)`.
fn wrap_signed(degrees: f64) -> f64 {
    let wrapped = (degrees + 180.0).rem_euclid(360.0) - 180.0;
    // `rem_euclid` can return exactly 360.0 for inputs a hair below zero, which lands here as
    // +180 — the one value the half-open range excludes.
    if wrapped >= 180.0 {
        -180.0
    } else {
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One reference case: a site, an instant, a direction, and where astropy says it is.
    struct Fixture {
        label: &'static str,
        latitude: f64,
        longitude: f64,
        utc: &'static str,
        ra_hours: f64,
        dec_degrees: f64,
        alt_degrees: f64,
        az_degrees: f64,
    }

    /// The acceptance criterion's independent reference.
    ///
    /// Computed with **astropy 8.0.1 / pyerfa 2.0.1.5**, transforming a `TETE` (true equator and
    /// equinox of date) direction to `AltAz` with `pressure = 0` so refraction is excluded on
    /// both sides. TETE rather than ICRS is the frame that makes the comparison mean anything:
    /// the mount's right ascension is an *of-date* coordinate derived from its hour-angle axis
    /// (SDD §5.2.3), and comparing it against a J2000 catalogue position would be measuring 26
    /// years of precession — about 22 arcmin — rather than this transform.
    ///
    /// The generator lives in the M1-T05 result notes; regenerating it needs astropy, which is
    /// not a build dependency of this workspace, so the numbers are checked in as literals. That
    /// is the same trade `astroctl-ipc`'s golden messages make.
    const FIXTURES: &[Fixture] = &[
        Fixture {
            label: "oslo, a target high in the north-east",
            latitude: 59.9139,
            longitude: 10.7522,
            utc: "2026-03-21T22:00:00Z",
            ra_hours: 18.615_649,
            dec_degrees: 38.783_689,
            alt_degrees: 20.624_563,
            az_degrees: 46.728_446,
        },
        Fixture {
            // On the horizon to within three arcminutes — the case a limit comparison is most
            // likely to get wrong, because every sign error in the transform is small here.
            label: "oslo, grazing the horizon",
            latitude: 59.9139,
            longitude: 10.7522,
            utc: "2026-03-21T22:00:00Z",
            ra_hours: 6.752_481,
            dec_degrees: -16.716_116,
            alt_degrees: -0.049_509,
            az_degrees: 235.090_681,
        },
        Fixture {
            // Required by the acceptance criterion: a target that is *below* the horizon must
            // come out below it, with a negative number rather than a clamp at zero.
            label: "oslo, well below the horizon",
            latitude: 59.9139,
            longitude: 10.7522,
            utc: "2026-03-21T22:00:00Z",
            ra_hours: 9.0,
            dec_degrees: -40.0,
            alt_degrees: -12.044_582,
            az_degrees: 199.470_084,
        },
        Fixture {
            // Required by the acceptance criterion: on the meridian, where the hour angle is
            // zero and azimuth is exactly south. A longitude sign error is most visible here.
            label: "oslo, transiting the meridian",
            latitude: 59.9139,
            longitude: 10.7522,
            utc: "2026-07-30T23:12:00Z",
            ra_hours: 20.490_284_278_437_74,
            dec_degrees: 20.0,
            alt_degrees: 50.086_060,
            az_degrees: 180.000_059,
        },
        Fixture {
            // Southern hemisphere and a *western* longitude: the two signs this transform can
            // get backwards, in one case, at a site that is not the one in the example config.
            label: "santiago, southern hemisphere and west longitude",
            latitude: -33.4489,
            longitude: -70.6693,
            utc: "2026-11-05T04:30:00Z",
            ra_hours: 5.919_529,
            dec_degrees: -52.695_661,
            alt_degrees: 51.245_232,
            az_degrees: 134.422_787,
        },
        Fixture {
            // Near the pole, where altitude is nearly the latitude whatever the hour angle is —
            // so a transform that had lost the hour angle entirely would still pass this one on
            // altitude and fail it on azimuth.
            label: "oslo, near the celestial pole",
            latitude: 59.9139,
            longitude: 10.7522,
            utc: "2026-01-15T03:00:00Z",
            ra_hours: 2.530_301,
            dec_degrees: 89.264_109,
            alt_degrees: 59.413_481,
            az_degrees: 358.931_514,
        },
        Fixture {
            // Within a degree of the zenith, where azimuth is ill-conditioned: a small altitude
            // error becomes a large azimuth one, which is why the assertion below is on angular
            // separation rather than on the two components independently.
            label: "the equator, a target at the zenith",
            latitude: 0.0,
            longitude: 0.0,
            utc: "2030-06-01T12:00:00Z",
            ra_hours: 4.612_222,
            dec_degrees: 0.0,
            alt_degrees: 89.162_209,
            az_degrees: 269.999_255,
        },
    ];

    /// Angular separation between two horizontal directions, in arcminutes.
    ///
    /// The right measure for this assertion: near the zenith a 14-arcsecond altitude error shows
    /// up as most of a degree of azimuth, and asserting on the components separately would either
    /// fail that case spuriously or need a tolerance loose enough to hide a real error elsewhere.
    fn separation_arcmin(a: AltAz, b: AltAz) -> f64 {
        let (alt_a, az_a) = (a.alt.degrees().to_radians(), a.az.degrees().to_radians());
        let (alt_b, az_b) = (b.alt.degrees().to_radians(), b.az.degrees().to_radians());
        let cosine = alt_a.sin() * alt_b.sin() + alt_a.cos() * alt_b.cos() * (az_a - az_b).cos();
        cosine.clamp(-1.0, 1.0).acos().to_degrees() * 60.0
    }

    fn at(utc: &str) -> DateTime<Utc> {
        utc.parse::<DateTime<Utc>>().expect("an RFC 3339 instant")
    }

    #[test]
    fn the_transform_agrees_with_astropy_to_within_an_arcminute() {
        // The M1-T05 acceptance criterion. One arcminute is the budget; the measured worst case
        // is 0.23′, so the margin is a factor of four — enough that a real regression fails this
        // and clock jitter does not.
        let mut worst: f64 = 0.0;
        for fixture in FIXTURES {
            let site = Site {
                latitude_degrees: fixture.latitude,
                longitude_degrees: fixture.longitude,
            };
            let target = RaDec::from_parts(fixture.ra_hours, fixture.dec_degrees)
                .expect("fixture coordinates are valid");
            let reference = AltAz::from_parts(fixture.alt_degrees, fixture.az_degrees)
                .expect("reference coordinates are valid");

            let computed = horizontal(target, site, at(fixture.utc));
            let separation = separation_arcmin(computed, reference);
            assert!(
                separation < 1.0,
                "{}: {separation:.4}′ from the astropy reference (alt {:.6} vs {:.6}, az \
                 {:.6} vs {:.6})",
                fixture.label,
                computed.alt.degrees(),
                reference.alt.degrees(),
                computed.az.degrees(),
                reference.az.degrees(),
            );
            worst = worst.max(separation);
        }
        assert!(
            worst > 0.0,
            "every case agreed exactly, which means the fixtures are not being computed"
        );
    }

    #[test]
    fn a_target_below_the_horizon_reports_a_negative_altitude() {
        // The limit compares `alt < min_altitude_degrees`, so a transform that clamped at zero
        // — or returned an unsigned altitude — would authorise a slew into the ground.
        let fixture = &FIXTURES[2];
        let site = Site {
            latitude_degrees: fixture.latitude,
            longitude_degrees: fixture.longitude,
        };
        let target =
            RaDec::from_parts(fixture.ra_hours, fixture.dec_degrees).expect("valid fixture");
        assert!(horizontal(target, site, at(fixture.utc)).alt.degrees() < 0.0);
    }

    #[test]
    fn a_transiting_target_has_a_zero_hour_angle() {
        // The meridian watch (MNT-16) is a comparison against this number, so it is asserted
        // directly rather than only through the altitude it produces.
        let fixture = &FIXTURES[3];
        let site = Site {
            latitude_degrees: fixture.latitude,
            longitude_degrees: fixture.longitude,
        };
        let target =
            RaDec::from_parts(fixture.ra_hours, fixture.dec_degrees).expect("valid fixture");
        let hour_angle = hour_angle_degrees(target, site, at(fixture.utc));
        assert!(
            hour_angle.abs() < 0.01,
            "a transiting target should have an hour angle of zero, got {hour_angle}°"
        );
    }

    #[test]
    fn east_longitude_advances_sidereal_time_and_west_retards_it() {
        // The sign trap named in `Site::longitude_degrees`. Two sites 90° apart must differ by
        // exactly six hours of sidereal time, in the direction that puts the eastern one ahead.
        let when = at("2026-07-30T23:12:00Z");
        let east = local_sidereal_degrees(
            Site {
                latitude_degrees: 0.0,
                longitude_degrees: 45.0,
            },
            when,
        );
        let west = local_sidereal_degrees(
            Site {
                latitude_degrees: 0.0,
                longitude_degrees: -45.0,
            },
            when,
        );
        let difference = wrap_signed(east - west);
        assert!(
            (difference - 90.0).abs() < 1e-9,
            "east should lead west by 90°, got {difference}°"
        );
    }

    #[test]
    fn sidereal_time_advances_faster_than_the_clock() {
        // A day of solar time is 360.9856° of sidereal rotation, not 360°. Getting this wrong is
        // a four-minute-per-day drift that looks fine for an evening and is an hour out in a
        // fortnight — which is exactly how long a horizon limit could be silently wrong.
        let site = Site {
            latitude_degrees: 59.9139,
            longitude_degrees: 10.7522,
        };
        let start = at("2026-07-30T23:12:00Z");
        let a_day_later = start + chrono::Duration::days(1);
        let advance = wrap_signed(
            local_sidereal_degrees(site, a_day_later) - local_sidereal_degrees(site, start),
        );
        assert!(
            (advance - 0.985_647).abs() < 1e-4,
            "one solar day should advance sidereal time by 0.9856°, got {advance}°"
        );
    }

    #[test]
    fn a_pole_on_target_does_not_produce_a_nan() {
        // `asin` of a value a hair past 1.0 is NaN, and NaN fails every `<` comparison — so a
        // limit check on a NaN altitude would *pass* a target that is straight down. The clamp in
        // `horizontal` is what prevents it; this is the case that reaches it.
        let site = Site {
            latitude_degrees: 90.0,
            longitude_degrees: 0.0,
        };
        let pole = RaDec::from_parts(0.0, 90.0).expect("the north celestial pole");
        let computed = horizontal(pole, site, at("2026-07-30T23:12:00Z"));
        assert!(computed.alt.degrees().is_finite());
        assert!(
            (computed.alt.degrees() - 90.0).abs() < 1e-6,
            "from the north pole the celestial pole is at the zenith, got {}",
            computed.alt.degrees()
        );
    }

    #[test]
    fn the_hour_angle_is_folded_into_a_signed_half_turn() {
        // The meridian watch tests `hour_angle > limit`, which is only meaningful on a signed
        // range: on [0, 360) every rising target would read as 300-something and be "past" it.
        for degrees in [-540.0, -180.0, -0.5, 0.0, 179.5, 180.0, 360.0, 540.0] {
            let wrapped = wrap_signed(degrees);
            assert!(
                (-180.0..180.0).contains(&wrapped),
                "{degrees}° wrapped to {wrapped}°"
            );
        }
    }
}
