//! The synthetic sky both simulated cameras look at — PRD §4.5, task M1-T06.
//!
//! # One sky, two cameras
//!
//! [`StarField`] is a *catalogue*, not an image: it answers "which stars are within this many
//! degrees of here, and how bright are they". The imaging camera and the guide camera are handed
//! the same `Arc<StarField>` and the same [`PointingSource`], so they necessarily agree about
//! where the stars are — a guide camera with its own private sky would let a Phase 3 guide loop
//! be developed against a star the imaging frames do not contain, and nothing would notice until
//! there was a telescope.
//!
//! # Why a procedural catalogue rather than a bundled star list
//!
//! The requirement is that the same pointing produces the same sky. A list of a few hundred real
//! stars would satisfy it only for the few hundred pointings that contain one; everywhere else
//! the frame is empty, and "everywhere else" is most of the sky. So the catalogue is generated
//! from the pointing instead: the celestial sphere is diced into one-degree cells, each cell's
//! contents are drawn from a stream keyed by (world seed, cell index), and a frame collects the
//! cells it overlaps.
//!
//! That construction buys the two properties the tests actually need:
//!
//! * **Stability.** A cell's stars depend on the cell, never on the pointing, so re-pointing at
//!   the same place returns the identical field — and *nudging* the mount by an arcsecond moves
//!   every star by an arcsecond instead of replacing the sky. A generator seeded by the pointing
//!   itself would look deterministic in a test that returns to the same coordinates exactly, and
//!   would scramble the field under a guide correction, which is precisely where it matters.
//! * **Continuity across cameras.** Both cameras project the same catalogue through their own
//!   plate scale, so a star at a given RA/Dec lands in both frames at the pixel each camera's
//!   optics put it at.
//!
//! The magnitudes are drawn from Pogson's `N(<m) ∝ 10^0.6m` slope (see
//! [`Rng::magnitude`](super::noise::Rng::magnitude)); the density is a knob because tests want a
//! sparse sky they can count and the default wants a plausible one.
//!
//! # What it is not
//!
//! No proper motion, no nebulosity, no gradient from light pollution or the Moon, no field
//! rotation, no optical aberration off-axis. Each is a real effect the Phase 2 pipeline will
//! eventually want, and each is absent rather than approximated: an invented gradient would be
//! the thing a background-extraction algorithm was tuned against, and tuning against invented
//! data is worse than having none.

use std::f64::consts::PI;
use std::fmt;
use std::time::Duration;

use astroctl_core::types::RaDec;

use super::noise::Rng;

/// Side of one catalogue cell in degrees.
///
/// One degree is a little under a frame width on the reference rig (1.28° across at 0.77″/px),
/// so an ordinary frame draws four to six cells: small enough that a frame does not generate
/// thousands of stars it then discards, large enough that the per-cell seeding cost is nothing.
const CELL_DEGREES: f64 = 1.0;

/// How many PSF sigmas a star is rendered out to.
///
/// A 2-D Gaussian holds 99.8% of its flux inside 3.5σ. Rendering further costs pixels and adds
/// nothing a 16-bit sample can represent; rendering less leaves a visible square edge around
/// bright stars, which a star detector then finds as structure.
const PSF_RADIUS_SIGMAS: f64 = 3.5;

/// FWHM to Gaussian sigma: `2 * sqrt(2 * ln 2)`.
const FWHM_TO_SIGMA: f64 = 2.354_820_045_030_949;

/// Where the telescope is pointed, asked of whoever knows.
///
/// The camera takes one of these at construction rather than a fixed coordinate, because the
/// requirement is that the frame follows the *simulated mount* (PRD §4.5). It is a trait and not
/// an `Arc<dyn MountDevice>` for two reasons: the HAL's `position()` is `async` and charges a
/// simulated 32 ms of serial latency, which a frame generator has no business paying, and
/// depending on the mount trait here would make every camera test build a mount.
///
/// The mount simulator implements it — see
/// [`SimulatorMount::pointing_source`](super::mount::SimulatorMount::pointing_source) — and so
/// does [`RaDec`], which is the whole of what a test that does not care about the mount needs.
pub trait PointingSource: fmt::Debug + Send + Sync {
    /// Where the telescope is aimed now, or `None` if it cannot say — a mount that is not
    /// connected, or not there at all.
    ///
    /// `None` is a routine answer, not an error: a camera whose mount is unplugged still takes
    /// frames, it just takes them of whatever the tube happens to be aimed at. The camera
    /// substitutes its configured default pointing and carries on, because a capture that failed
    /// because the *mount* was absent would make the camera untestable on its own.
    fn pointing(&self) -> Option<RaDec>;
}

/// A fixed pointing — the simplest possible source, and the one every camera test that is not
/// about the mount uses.
impl PointingSource for RaDec {
    fn pointing(&self) -> Option<RaDec> {
        Some(*self)
    }
}

/// One star as the catalogue holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogStar {
    /// Right ascension in degrees, `[0, 360)`.
    pub ra_degrees: f64,
    /// Declination in degrees, `[-90, 90]`.
    pub dec_degrees: f64,
    /// Apparent visual magnitude.
    pub magnitude: f64,
}

/// A procedurally generated star catalogue (PRD §4.5 "using a catalog").
///
/// Cheap to clone and cheaper to share: it holds four numbers and generates everything else on
/// demand, which is what lets both cameras hold the same `Arc` without either owning the other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StarField {
    seed: u64,
    density_per_sq_degree: f64,
    faintest_magnitude: f64,
    brightest_magnitude: f64,
}

impl Default for StarField {
    fn default() -> Self {
        Self {
            // Any fixed value; it exists so that two field nodes built from the same config show
            // the same sky, which is what makes a bug report reproducible.
            seed: 0x4153_5452_4F43_544C, // "ASTROCTL"
            density_per_sq_degree: DEFAULT_DENSITY_PER_SQ_DEGREE,
            faintest_magnitude: DEFAULT_FAINTEST_MAGNITUDE,
            brightest_magnitude: BRIGHTEST_MAGNITUDE,
        }
    }
}

/// Stars per square degree down to [`DEFAULT_FAINTEST_MAGNITUDE`].
///
/// **Invented, but anchored.** Real counts at mid-galactic latitude run to roughly 10⁴ stars per
/// square degree by magnitude 18 (Bahcall–Soneira and every survey since). The default here is
/// deliberately an order of magnitude below that: the number of stars is what a full-size frame
/// costs to render, and a default that spends a second on stars nobody looks at would be paid by
/// every test in the workspace. A test that wants a crowded field asks for one.
const DEFAULT_DENSITY_PER_SQ_DEGREE: f64 = 900.0;

/// The faint limit of the generated catalogue.
///
/// **Invented.** A 30 s sub at ISO 1600 on the reference rig reaches somewhere near magnitude 18
/// before a star stops being distinguishable from the sky, so generating fainter ones would burn
/// time producing signal that the noise swallows. The consequence is stated rather than hidden:
/// a *long* simulated exposure will not reveal a fainter population, because there is none.
const DEFAULT_FAINTEST_MAGNITUDE: f64 = 18.0;

/// The bright limit. Nothing in the sky is brighter than Sirius at −1.46, and letting the
/// magnitude draw run to −∞ produces a star whose flux overflows an `f32` band buffer.
const BRIGHTEST_MAGNITUDE: f64 = -1.5;

impl StarField {
    /// A catalogue with the default density and limit, seeded by `seed`.
    ///
    /// Two `StarField`s with the same seed are the same sky forever; that is the contract the
    /// frame tests rest on.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            ..Self::default()
        }
    }

    /// The same catalogue at a different star density, in stars per square degree.
    #[must_use]
    pub fn with_density(mut self, density_per_sq_degree: f64) -> Self {
        self.density_per_sq_degree = density_per_sq_degree.max(0.0);
        self
    }

    /// The same catalogue with a different faint limit.
    #[must_use]
    pub fn with_faintest_magnitude(mut self, magnitude: f64) -> Self {
        self.faintest_magnitude = magnitude;
        self
    }

    /// Stars per square degree this catalogue generates.
    #[must_use]
    pub fn density_per_sq_degree(&self) -> f64 {
        self.density_per_sq_degree
    }

    /// The seed this sky came from — written into every frame's FITS header, so a reported
    /// frame can be regenerated on another machine.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Every star within `radius_degrees` of `centre`, in no particular order.
    ///
    /// The cost is proportional to the area asked for, so a caller asks for the frame's
    /// half-diagonal and not for the sky.
    #[must_use]
    pub fn stars_within(&self, centre: RaDec, radius_degrees: f64) -> Vec<CatalogStar> {
        let dec0 = centre.dec.degrees();
        let ra0 = centre.ra.hours() * 15.0;

        let dec_low = (dec0 - radius_degrees).max(-90.0);
        let dec_high = (dec0 + radius_degrees).min(90.0);

        let mut stars = Vec::new();
        let mut dec_index = (dec_low / CELL_DEGREES).floor() as i64;
        while (dec_index as f64) * CELL_DEGREES < dec_high {
            let cell_dec = (dec_index as f64) * CELL_DEGREES;
            // How far in right ascension `radius_degrees` reaches at this declination. Near the
            // pole the cosine collapses and the answer is "all of it" — which is correct: every
            // meridian passes within a degree of the pole, so a frame there really does overlap
            // every RA cell.
            let cos_dec = (cell_dec + CELL_DEGREES / 2.0).to_radians().cos().abs();
            let ra_reach = if cos_dec < 1e-3 {
                180.0
            } else {
                (radius_degrees / cos_dec).min(180.0)
            };
            let ra_low = ra0 - ra_reach;
            let ra_high = ra0 + ra_reach;

            let mut ra_index = (ra_low / CELL_DEGREES).floor() as i64;
            while (ra_index as f64) * CELL_DEGREES < ra_high {
                // Wrapped *after* the loop bounds are computed, so a frame straddling 0h collects
                // the cells on both sides rather than stopping at the seam. A seam in the sky
                // would be a reproducible "the stars vanish at RA 0" bug.
                let cells_per_turn = (360.0 / CELL_DEGREES) as i64;
                let wrapped = ra_index.rem_euclid(cells_per_turn);
                self.extend_from_cell(&mut stars, wrapped, dec_index);
                ra_index += 1;
            }
            dec_index += 1;
        }
        stars
    }

    /// Appends the contents of one cell.
    fn extend_from_cell(&self, stars: &mut Vec<CatalogStar>, ra_index: i64, dec_index: i64) {
        let ra_low = (ra_index as f64) * CELL_DEGREES;
        let dec_low = (dec_index as f64) * CELL_DEGREES;
        if !(-90.0..90.0).contains(&dec_low) {
            return;
        }
        let dec_centre = dec_low + CELL_DEGREES / 2.0;
        // A cell one degree on a side covers cos(dec) square degrees of sky, so the expected
        // count has to be scaled or the poles would be as crowded as the equator in RA terms and
        // far denser on the sky.
        let area = CELL_DEGREES * CELL_DEGREES * dec_centre.to_radians().cos().abs();
        let mut rng = Rng::stream(&[self.seed, ra_index as u64, dec_index as u64]);
        let count = rng.poisson(self.density_per_sq_degree * area).round() as usize;

        stars.reserve(count);
        for _ in 0..count {
            stars.push(CatalogStar {
                ra_degrees: rng.range(ra_low, ra_low + CELL_DEGREES).rem_euclid(360.0),
                dec_degrees: rng
                    .range(dec_low, dec_low + CELL_DEGREES)
                    .clamp(-90.0, 90.0),
                magnitude: rng.magnitude(self.brightest_magnitude, self.faintest_magnitude),
            });
        }
    }
}

// -----------------------------------------------------------------------------------------
// Projection
// -----------------------------------------------------------------------------------------

/// Sky ↔ pixel for one frame: a gnomonic (TAN) projection about the pointing.
///
/// TAN because it is what every plate solve in this system will produce and consume (the WCS
/// convention astrometry.net writes, SDD §5.6), so a frame projected any other way would be a
/// frame the solver would report as distorted. Over a one-degree field the difference from a
/// plain linear scale is under a milliarcsecond, which is exactly why using the right one costs
/// nothing.
///
/// **Orientation:** north up, east left, no rotation. Field rotation is a real property of a
/// German equatorial after a meridian flip and is deliberately not modelled here (see the module
/// docs); a caller that assumes this orientation is assuming something the simulator guarantees
/// and the sky does not.
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    sin_dec0: f64,
    cos_dec0: f64,
    ra0_radians: f64,
    /// Radians of sky per pixel.
    scale: f64,
    centre_column: f64,
    centre_row: f64,
}

impl Projection {
    /// The projection for a `width × height` frame at `arcsec_per_pixel`, aimed at `pointing`.
    #[must_use]
    pub fn new(pointing: RaDec, width: u32, height: u32, arcsec_per_pixel: f64) -> Self {
        let dec0 = pointing.dec.degrees().to_radians();
        Self {
            sin_dec0: dec0.sin(),
            cos_dec0: dec0.cos(),
            ra0_radians: (pointing.ra.hours() * 15.0).to_radians(),
            scale: (arcsec_per_pixel / 3600.0).to_radians(),
            centre_column: f64::from(width - 1) / 2.0,
            centre_row: f64::from(height - 1) / 2.0,
        }
    }

    /// Half the frame diagonal in degrees — what to ask the catalogue for.
    #[must_use]
    pub fn radius_degrees(&self, width: u32, height: u32) -> f64 {
        let half_diagonal =
            (f64::from(width).powi(2) + f64::from(height).powi(2)).sqrt() / 2.0 * self.scale;
        half_diagonal.to_degrees()
    }

    /// Where a sky position lands, in fractional pixels, or `None` for a position on the far
    /// side of the sphere (where a gnomonic projection has no answer at all, rather than a wrong
    /// one).
    #[must_use]
    pub fn to_pixel(&self, ra_degrees: f64, dec_degrees: f64) -> Option<(f64, f64)> {
        let dec = dec_degrees.to_radians();
        let delta_ra = ra_degrees.to_radians() - self.ra0_radians;
        let (sin_dec, cos_dec) = dec.sin_cos();
        let (sin_delta, cos_delta) = delta_ra.sin_cos();

        let cos_distance = self.sin_dec0 * sin_dec + self.cos_dec0 * cos_dec * cos_delta;
        if cos_distance <= 1e-6 {
            return None;
        }
        let east = cos_dec * sin_delta / cos_distance;
        let north = (self.cos_dec0 * sin_dec - self.sin_dec0 * cos_dec * cos_delta) / cos_distance;
        Some((
            self.centre_column - east / self.scale,
            self.centre_row - north / self.scale,
        ))
    }

    /// The sky position a pixel looks at — the inverse of [`to_pixel`](Self::to_pixel).
    ///
    /// Used by the tests that check the two cameras agree about the sky: each measures a star in
    /// its own pixels, and this is what turns two different plate scales into one comparison.
    #[must_use]
    pub fn to_sky(&self, column: f64, row: f64) -> RaDec {
        let east = (self.centre_column - column) * self.scale;
        let north = (self.centre_row - row) * self.scale;
        let radius = (east * east + north * north).sqrt();
        let angle = radius.atan();
        let (sin_angle, cos_angle) = angle.sin_cos();

        let (dec, ra_offset) = if radius < 1e-12 {
            (self.sin_dec0.asin(), 0.0)
        } else {
            let dec = (cos_angle * self.sin_dec0 + north * sin_angle * self.cos_dec0 / radius)
                .clamp(-1.0, 1.0)
                .asin();
            let ra_offset = (east * sin_angle)
                .atan2(radius * self.cos_dec0 * cos_angle - north * sin_angle * self.sin_dec0);
            (dec, ra_offset)
        };
        let ra_degrees = (self.ra0_radians + ra_offset)
            .to_degrees()
            .rem_euclid(360.0);
        // Both components are inside their domains by construction — `asin` is clamped and the
        // right ascension is taken modulo a turn — so a failure here would be an arithmetic bug,
        // not a bad input.
        RaDec::from_parts(ra_degrees / 15.0, dec.to_degrees())
            .expect("a projected pixel is always a valid coordinate")
    }
}

// -----------------------------------------------------------------------------------------
// Exposure
// -----------------------------------------------------------------------------------------

/// A star the caller adds to whatever the catalogue supplies.
///
/// Exists for the guide camera: PRD §4.5 asks for a *configurable guide star brightness*, and a
/// guide camera whose star came from the catalogue would have a brightness that depends on where
/// the mount happens to be pointed — so a guiding test would be testing the catalogue's luck.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InjectedStar {
    /// Offset from the frame centre in pixels, `(column, row)`.
    pub offset: (f64, f64),
    /// Apparent magnitude.
    pub magnitude: f64,
}

/// Everything one synthetic exposure needs to know.
///
/// A struct rather than a long argument list because half of these are `f64` and the compiler
/// cannot tell a sky level from a read noise; a transposed pair would produce a plausible frame
/// that is wrong in a way no test would name.
#[derive(Debug, Clone)]
pub struct Exposure {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Where the telescope is aimed.
    pub pointing: RaDec,
    /// Plate scale, arcseconds per pixel (after binning).
    pub arcsec_per_pixel: f64,
    /// Integration time. Signal scales with it; noise scales with its square root, because that
    /// is what shot noise does.
    pub exposure: Duration,
    /// Electrons to ADU. This is what an ISO or a gain setting actually changes.
    pub gain_adu_per_electron: f64,
    /// Seeing, as the full width at half maximum of the Gaussian PSF.
    pub fwhm_arcsec: f64,
    /// Sky brightness in electrons per pixel per second.
    pub sky_electrons_per_second: f64,
    /// Read noise in electrons RMS.
    pub read_noise_electrons: f64,
    /// The pedestal every sample sits on, in ADU — a real sensor's bias offset, and the reason a
    /// dark frame is not zero.
    pub bias_adu: f64,
    /// Full well in electrons: where a pixel stops responding to light.
    pub full_well_electrons: f64,
    /// The largest sample value the converter can produce — 65535 for the 16-bit imaging path,
    /// 4095 for a 12-bit guide camera, so that a star detector reading
    /// [`GuideCameraCapabilities::bit_depth`](astroctl_core::types::GuideCameraCapabilities)
    /// finds saturation where it was told to.
    pub saturation_adu: u16,
    /// Stream key for the noise, or `None` for a noiseless frame.
    ///
    /// Noiseless frames are not a realism failure, they are how the geometry tests measure a
    /// centroid to a thousandth of a pixel without averaging a hundred frames first.
    pub noise_seed: Option<u64>,
    /// Atmospheric displacement of the whole field for this frame, in arcseconds `(east, north)`.
    /// This is seeing as a *guide loop* sees it: the star wanders, and the frame it wanders in
    /// does not.
    pub jitter_arcsec: (f64, f64),
    /// Stars added on top of the catalogue.
    pub injected: Vec<InjectedStar>,
    /// The catalogue this frame draws its stars from. Carried by value — it is four numbers —
    /// so an exposure is self-contained and can be handed to the camera's own thread without
    /// borrowing anything the driver holds behind a lock.
    pub field: StarField,
}

impl Exposure {
    /// Renders the frame: `width * height` samples, row-major, saturating at
    /// [`saturation_adu`](Self::saturation_adu).
    ///
    /// # Panics
    /// If `width * height` does not fit in `usize`, which on a 64-bit target means never — the
    /// largest frame the config permits is 24 megapixels.
    #[must_use]
    pub fn render(&self) -> Vec<u16> {
        let projection = Projection::new(
            self.pointing,
            self.width,
            self.height,
            self.arcsec_per_pixel,
        );
        let stars = self.placed_stars(&projection);

        let width = self.width as usize;
        let height = self.height as usize;
        let seconds = self.exposure.as_secs_f64();
        let sky = self.sky_electrons_per_second * seconds;

        let sigma = (self.fwhm_arcsec / self.arcsec_per_pixel) / FWHM_TO_SIGMA;
        let sigma = sigma.max(0.35); // an undersampled PSF is still a PSF, not a division by zero
        let radius = (PSF_RADIUS_SIGMAS * sigma).ceil().max(1.0);
        let normalisation = 1.0 / (2.0 * PI * sigma * sigma);
        let two_sigma_squared = 2.0 * sigma * sigma;

        // One row of accumulated electrons at a time. The alternative — an `f32` image the size
        // of the frame — is 96 MB for a full-size exposure on a node budgeted at 512 MB total
        // (PRF-05), and buys nothing: stars are placed, so the rows each one touches are known.
        let mut signal = vec![0.0_f32; width];
        let mut pixels = vec![0_u16; width * height];

        for row in 0..height {
            signal.fill(sky as f32);
            let row_centre = row as f64;
            for star in &stars {
                if (star.row - row_centre).abs() > radius {
                    continue;
                }
                let first = (star.column - radius).ceil().max(0.0) as usize;
                let last = ((star.column + radius).floor() as isize).min(width as isize - 1);
                if last < first as isize {
                    continue;
                }
                let dy = row_centre - star.row;
                let peak = star.electrons * normalisation;
                for (column, cell) in signal
                    .iter_mut()
                    .enumerate()
                    .take(last as usize + 1)
                    .skip(first)
                {
                    let dx = column as f64 - star.column;
                    *cell += (peak * (-(dx * dx + dy * dy) / two_sigma_squared).exp()) as f32;
                }
            }

            // Each row draws its own stream, so the noise depends on the frame and the row and
            // on nothing else — not on how the renderer happens to be chunked today.
            let mut rng = self.noise_seed.map(|seed| Rng::stream(&[seed, row as u64]));
            let out = &mut pixels[row * width..(row + 1) * width];
            for (sample, electrons) in out.iter_mut().zip(signal.iter()) {
                let mut charge = f64::from(*electrons);
                if let Some(rng) = rng.as_mut() {
                    charge = rng.poisson(charge) + rng.normal_with(0.0, self.read_noise_electrons);
                }
                let charge = charge.min(self.full_well_electrons);
                let adu = charge * self.gain_adu_per_electron + self.bias_adu;
                *sample = if adu <= 0.0 {
                    0
                } else if adu >= f64::from(self.saturation_adu) {
                    self.saturation_adu
                } else {
                    adu as u16
                };
            }
        }
        pixels
    }

    /// Projects the catalogue and the injected stars into frame coordinates, dropping everything
    /// that cannot light a pixel.
    fn placed_stars(&self, projection: &Projection) -> Vec<PlacedStar> {
        let seconds = self.exposure.as_secs_f64();
        let sigma = (self.fwhm_arcsec / self.arcsec_per_pixel) / FWHM_TO_SIGMA;
        let margin = PSF_RADIUS_SIGMAS * sigma.max(0.35);
        // The jitter moves the sky, so it moves the *catalogue*, not the frame — a guide star
        // that stayed put while the field wandered would be a mount that guides itself.
        let (jitter_column, jitter_row) = (
            -self.jitter_arcsec.0 / self.arcsec_per_pixel,
            -self.jitter_arcsec.1 / self.arcsec_per_pixel,
        );

        let radius = projection.radius_degrees(self.width, self.height)
            + margin * self.arcsec_per_pixel / 3600.0;
        let mut placed = Vec::new();
        for star in self.field.stars_within(self.pointing, radius) {
            let Some((column, row)) = projection.to_pixel(star.ra_degrees, star.dec_degrees) else {
                continue;
            };
            let column = column + jitter_column;
            let row = row + jitter_row;
            if column < -margin
                || row < -margin
                || column > f64::from(self.width) + margin
                || row > f64::from(self.height) + margin
            {
                continue;
            }
            placed.push(PlacedStar {
                column,
                row,
                electrons: flux_electrons(star.magnitude) * seconds,
            });
        }
        for star in &self.injected {
            placed.push(PlacedStar {
                column: f64::from(self.width - 1) / 2.0 + star.offset.0 + jitter_column,
                row: f64::from(self.height - 1) / 2.0 + star.offset.1 + jitter_row,
                electrons: flux_electrons(star.magnitude) * seconds,
            });
        }
        placed
    }
}

/// A catalogue star reduced to what the renderer needs.
#[derive(Debug, Clone, Copy)]
struct PlacedStar {
    column: f64,
    row: f64,
    /// Total electrons the star delivers over the whole exposure, before the PSF spreads them.
    electrons: f64,
}

/// Electrons per second from a star of magnitude `m` on the reference rig.
///
/// **Derived, not measured.** A magnitude-zero star delivers about 10⁶ photons per second per
/// cm² above the atmosphere in a broad visual band; the 200 mm primary of PRD §8.3 collects
/// 314 cm² of that, and a front-illuminated CMOS with the usual losses converts something like
/// a third of it — call it 10⁸ e⁻/s at magnitude 0. The consequence is the useful part: a
/// magnitude-12 star delivers ~1,600 e⁻/s, so it is a solid detection in a one-second frame and
/// saturates a well in twenty, which is what the reference rig actually does.
fn flux_electrons(magnitude: f64) -> f64 {
    const ZERO_MAGNITUDE_ELECTRONS_PER_SECOND: f64 = 1.0e8;
    ZERO_MAGNITUDE_ELECTRONS_PER_SECOND * 10.0_f64.powf(-0.4 * magnitude)
}

#[cfg(test)]
pub(super) mod detect {
    //! Star finding, for tests only.
    //!
    //! The frame tests must assert on what is *in* a frame — "there are stars", "the star moved"
    //! — and asserting that against the star list the renderer was given would only prove the
    //! renderer was called. So the tests measure the pixels, with the crudest detector that
    //! works: subtract a background, find local maxima above a threshold, centroid what is left.

    /// A star as measured back out of a rendered frame.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub(in crate::simulator) struct Detection {
        /// Flux-weighted centre, in fractional pixels.
        pub column: f64,
        /// Flux-weighted centre, in fractional pixels.
        pub row: f64,
        /// Peak sample value.
        pub peak: u16,
    }

    /// The median sample — the background, robustly, without sorting 24 million values.
    pub(in crate::simulator) fn background(pixels: &[u16]) -> f64 {
        let mut histogram = vec![0_u32; 65_536];
        for sample in pixels {
            histogram[*sample as usize] += 1;
        }
        let half = pixels.len() as u64 / 2;
        let mut seen = 0_u64;
        for (value, count) in histogram.iter().enumerate() {
            seen += u64::from(*count);
            if seen >= half {
                return value as f64;
            }
        }
        0.0
    }

    /// Mean sample value.
    pub(in crate::simulator) fn mean(pixels: &[u16]) -> f64 {
        pixels.iter().map(|s| f64::from(*s)).sum::<f64>() / pixels.len() as f64
    }

    /// Every local maximum more than `threshold` above the background, with its centroid.
    ///
    /// The 3×3 local-maximum test is what keeps one bright star from being counted as nine: a
    /// plain threshold count would report "star count" as a function of brightness rather than
    /// of how many stars there are.
    pub(in crate::simulator) fn stars(
        pixels: &[u16],
        width: u32,
        height: u32,
        threshold: f64,
    ) -> Vec<Detection> {
        let width = width as usize;
        let height = height as usize;
        let floor = background(pixels) + threshold;
        let mut found = Vec::new();
        for row in 1..height.saturating_sub(1) {
            for column in 1..width.saturating_sub(1) {
                let here = pixels[row * width + column];
                if f64::from(here) < floor {
                    continue;
                }
                let mut is_peak = true;
                for dy in -1_isize..=1 {
                    for dx in -1_isize..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let neighbour = pixels[(row as isize + dy) as usize * width
                            + (column as isize + dx) as usize];
                        // `>=` on one side only, so a flat-topped (saturated) star is found once
                        // rather than never.
                        if neighbour > here {
                            is_peak = false;
                        }
                    }
                }
                if is_peak {
                    found.push(centroid(pixels, width, height, column, row));
                }
            }
        }
        found
    }

    /// The brightest star in the frame, measured.
    pub(in crate::simulator) fn brightest(
        pixels: &[u16],
        width: u32,
        height: u32,
        threshold: f64,
    ) -> Option<Detection> {
        stars(pixels, width, height, threshold)
            .into_iter()
            .max_by_key(|found| found.peak)
    }

    /// Background-subtracted centre of mass in a 9×9 box — big enough for the default seeing,
    /// small enough that two stars a few pixels apart do not pull on each other.
    fn centroid(
        pixels: &[u16],
        width: usize,
        height: usize,
        column: usize,
        row: usize,
    ) -> Detection {
        const HALF_BOX: isize = 4;
        let floor = background(pixels);
        let (mut weight, mut sum_column, mut sum_row) = (0.0, 0.0, 0.0);
        for dy in -HALF_BOX..=HALF_BOX {
            for dx in -HALF_BOX..=HALF_BOX {
                let y = row as isize + dy;
                let x = column as isize + dx;
                if y < 0 || x < 0 || y >= height as isize || x >= width as isize {
                    continue;
                }
                let value = (f64::from(pixels[y as usize * width + x as usize]) - floor).max(0.0);
                weight += value;
                sum_column += value * x as f64;
                sum_row += value * y as f64;
            }
        }
        if weight <= 0.0 {
            return Detection {
                column: column as f64,
                row: row as f64,
                peak: pixels[row * width + column],
            };
        }
        Detection {
            column: sum_column / weight,
            row: sum_row / weight,
            peak: pixels[row * width + column],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointing() -> RaDec {
        RaDec::from_parts(5.5, 22.0).expect("a valid coordinate")
    }

    fn exposure(field: StarField) -> Exposure {
        Exposure {
            width: 200,
            height: 150,
            pointing: pointing(),
            arcsec_per_pixel: 2.0,
            exposure: Duration::from_secs(4),
            gain_adu_per_electron: 1.0,
            fwhm_arcsec: 4.0,
            sky_electrons_per_second: 20.0,
            read_noise_electrons: 3.0,
            bias_adu: 512.0,
            full_well_electrons: 30_000.0,
            saturation_adu: u16::MAX,
            noise_seed: Some(1),
            jitter_arcsec: (0.0, 0.0),
            injected: Vec::new(),
            field,
        }
    }

    #[test]
    fn the_same_pointing_returns_the_same_stars() {
        // The requirement in one assertion (PRD §4.5, task scope): seed + position determine the
        // sky. Asked twice, in the order a session would ask.
        let field = StarField::new(99);
        let first = field.stars_within(pointing(), 0.6);
        let second = field.stars_within(pointing(), 0.6);
        assert!(!first.is_empty());
        assert_eq!(first, second);

        // ...and a different seed is a different sky, or the seed is decoration.
        let other = StarField::new(100).stars_within(pointing(), 0.6);
        assert_ne!(first, other);
    }

    #[test]
    fn nudging_the_pointing_keeps_the_stars_and_moves_them() {
        // The property a pointing-seeded generator gets wrong: a small move must not resample
        // the sky. This is what a guide loop and a dither sequence both depend on.
        let field = StarField::new(7);
        let centre = pointing();
        let nudged =
            RaDec::from_parts(centre.ra.hours() + 0.001, centre.dec.degrees() + 0.01).expect("ok");
        let before = field.stars_within(centre, 0.3);
        let after = field.stars_within(nudged, 0.3);

        let kept = before
            .iter()
            .filter(|star| after.iter().any(|other| *other == **star))
            .count();
        assert!(
            kept as f64 > 0.8 * before.len() as f64,
            "a 36-arcsecond nudge kept {kept} of {} stars",
            before.len()
        );
    }

    #[test]
    fn density_sets_how_many_stars_there_are() {
        let sparse = StarField::new(1).with_density(50.0);
        let dense = StarField::new(1).with_density(500.0);
        let sparse_count = sparse.stars_within(pointing(), 1.0).len();
        let dense_count = dense.stars_within(pointing(), 1.0).len();
        let ratio = dense_count as f64 / sparse_count as f64;
        assert!(
            (8.0..12.0).contains(&ratio),
            "ten times the density gave {sparse_count} then {dense_count}"
        );
    }

    #[test]
    fn the_catalogue_has_no_seam_at_zero_hours_or_at_the_pole() {
        // Two coordinates where an off-by-one in the cell arithmetic produces an empty sky, and
        // where an operator would report "the simulator shows nothing near Polaris".
        let field = StarField::new(3);
        for (ra_hours, dec) in [(0.0, 10.0), (23.999, 10.0), (0.0, 89.5), (12.0, -89.5)] {
            let centre = RaDec::from_parts(ra_hours, dec).expect("valid");
            let stars = field.stars_within(centre, 0.7);
            assert!(!stars.is_empty(), "nothing at {ra_hours}h {dec}deg");
            // ...and the stars are actually near the pointing, not scattered over the sky by a
            // wrap that lost its cosine.
            let close = stars
                .iter()
                .filter(|star| (star.dec_degrees - dec).abs() < 1.5)
                .count();
            assert_eq!(close, stars.len(), "at {ra_hours}h {dec}deg");
        }
    }

    #[test]
    fn the_projection_round_trips() {
        let projection = Projection::new(pointing(), 200, 150, 2.0);
        for (column, row) in [(0.0, 0.0), (99.5, 74.5), (199.0, 149.0), (10.0, 140.0)] {
            let sky = projection.to_sky(column, row);
            let (back_column, back_row) = projection
                .to_pixel(sky.ra.hours() * 15.0, sky.dec.degrees())
                .expect("in front of the sphere");
            assert!(
                (back_column - column).abs() < 1e-6 && (back_row - row).abs() < 1e-6,
                "({column}, {row}) came back as ({back_column}, {back_row})"
            );
        }
    }

    #[test]
    fn north_is_up_and_east_is_left() {
        // Orientation is the kind of thing that is either asserted once or discovered in Phase 3
        // when every guide correction goes the wrong way.
        let projection = Projection::new(pointing(), 200, 150, 2.0);
        let centre = pointing();
        let north = projection
            .to_pixel(centre.ra.hours() * 15.0, centre.dec.degrees() + 0.02)
            .expect("visible");
        let east = projection
            .to_pixel(centre.ra.hours() * 15.0 + 0.02, centre.dec.degrees())
            .expect("visible");
        assert!(north.1 < 74.5, "north must decrease the row index");
        assert!(east.0 < 99.5, "east must decrease the column index");
    }

    #[test]
    fn a_rendered_frame_contains_stars_on_a_sky_background() {
        // The catalogue's default faint limit is magnitude 18, which a four-second exposure
        // genuinely cannot show — 25 electrons spread over a PSF is below this frame's read
        // noise, and a simulator that showed them anyway would be lying about its own sky. The
        // test therefore asks for a population it can see, which is also what a real observer
        // does when they want stars in a four-second frame.
        let field = StarField::new(11)
            .with_density(2_000.0)
            .with_faintest_magnitude(13.0);
        let pixels = exposure(field).render();
        let background = detect::background(&pixels);
        // 20 e/s for 4 s at unit gain on a 512 ADU pedestal.
        assert!(
            (background - 592.0).abs() < 25.0,
            "background is {background} ADU"
        );
        let found = detect::stars(&pixels, 200, 150, 200.0);
        assert!(!found.is_empty(), "no stars in the frame");
    }

    #[test]
    fn exposure_time_scales_the_signal_and_the_sky() {
        let mut short = exposure(StarField::new(11).with_density(400.0));
        short.exposure = Duration::from_secs(1);
        let mut long = short.clone();
        long.exposure = Duration::from_secs(8);

        let short_background = detect::background(&short.render()) - 512.0;
        let long_background = detect::background(&long.render()) - 512.0;
        let ratio = long_background / short_background;
        assert!((7.0..9.0).contains(&ratio), "eight times gave {ratio}x");
    }

    #[test]
    fn seeing_jitter_moves_the_whole_field() {
        // The guide camera's mechanism, tested where it is implemented rather than three layers
        // up: a displacement in arcseconds must come back as a displacement in pixels.
        let mut still = exposure(StarField::new(5).with_density(0.0));
        still.noise_seed = None;
        still.injected = vec![InjectedStar {
            offset: (0.0, 0.0),
            magnitude: 8.0,
        }];
        let mut shifted = still.clone();
        shifted.jitter_arcsec = (6.0, 0.0); // three pixels east at 2"/px

        let before = detect::brightest(&still.render(), 200, 150, 100.0).expect("a star");
        let after = detect::brightest(&shifted.render(), 200, 150, 100.0).expect("a star");
        let moved = before.column - after.column;
        assert!(
            (moved - 3.0).abs() < 0.05,
            "six arcseconds east moved the star {moved} px"
        );
        assert!((before.row - after.row).abs() < 0.05, "and not vertically");
    }

    #[test]
    fn a_bright_star_saturates_at_the_declared_depth() {
        // What `bit_depth` is for: a 12-bit guide frame must clip at 4095, or a star detector
        // reading the capability sees a saturated star as a merely bright one and centroids it.
        let mut twelve_bit = exposure(StarField::new(5).with_density(0.0));
        twelve_bit.saturation_adu = 4095;
        twelve_bit.injected = vec![InjectedStar {
            offset: (0.0, 0.0),
            magnitude: 2.0,
        }];
        let pixels = twelve_bit.render();
        assert_eq!(pixels.iter().copied().max(), Some(4095));
    }

    #[test]
    fn a_noiseless_frame_is_reproducible_to_the_sample() {
        let mut spec = exposure(StarField::new(13).with_density(200.0));
        spec.noise_seed = None;
        assert_eq!(spec.render(), spec.render());
        // ...and so is a noisy one, given the same stream key.
        spec.noise_seed = Some(4242);
        assert_eq!(spec.render(), spec.render());
    }
}
