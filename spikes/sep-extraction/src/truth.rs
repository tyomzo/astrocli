//! Ground truth, and scoring SEP against it.
//!
//! # Why this is possible at all
//!
//! The simulator's sky is procedural: `StarField` dices the celestial sphere into 1-degree cells
//! and seeds each cell's contents from `(world seed, cell index)`. So for a given seed and
//! pointing the true star list is not *recorded* anywhere — it is **computed**, by the same code
//! that rendered the frame. `crates/astroctl-drivers/src/simulator/sky.rs`.
//!
//! That makes this the one measurement in the spike with a real answer to compare against.
//! Everything else (speed, memory, agreement with the Python package) compares SEP to a clock,
//! a ceiling, or to itself.
//!
//! # The convention trap, recorded because it would silently halve the score
//!
//! The simulator renders a star centred at fractional pixel coordinate `(column, row)` and
//! samples pixel `i` at coordinate exactly `i` (see `Exposure::render`: `dx = column as f64 -
//! star.column`). SEP's `x`/`y` are also 0-indexed with pixel centres at integers. **The two
//! conventions coincide, so no half-pixel shift is applied.** If a future binding feeds SEP a
//! FITS array read by astropy, that is still true; if it ever compares against a FITS *world*
//! coordinate, FITS is 1-indexed and a +1 appears. Getting this wrong produces a centroid RMS of
//! ~0.5 px that looks like a plausible measurement rather than a bug.

use astroctl_core::types::RaDec;
use astroctl_drivers::simulator::sky::{CatalogStar, Projection, StarField};

/// Arcseconds in a radian — the simulator's `profile::arcsec_per_pixel` constant, duplicated
/// here because it is private to that module.
const ARCSEC_PER_RADIAN: f64 = 206_264.806_247_096_36;

/// Plate scale for a sensor of `pitch_um` behind `focal_mm`.
pub fn arcsec_per_pixel(pitch_um: f64, focal_mm: f64) -> f64 {
    ARCSEC_PER_RADIAN * (pitch_um / 1000.0) / focal_mm
}

/// Electrons per second from a star of magnitude `m`.
///
/// **Duplicated from `sky.rs`'s private `flux_electrons`.** A spike may copy a constant; a
/// production binding must not, and this is one of the small things that argues for the scoring
/// harness eventually living beside the simulator rather than outside it.
pub fn flux_electrons(magnitude: f64) -> f64 {
    const ZERO_MAGNITUDE_ELECTRONS_PER_SECOND: f64 = 1.0e8;
    ZERO_MAGNITUDE_ELECTRONS_PER_SECOND * 10.0_f64.powf(-0.4 * magnitude)
}

/// One true star, projected into this frame.
#[derive(Debug, Clone, Copy)]
pub struct TruthStar {
    /// Column, in the simulator's (and SEP's) 0-indexed fractional-pixel convention.
    pub column: f64,
    /// Row, same convention.
    pub row: f64,
    pub magnitude: f64,
    /// Distance to the nearest other truth star, in pixels. Used to separate "SEP missed it"
    /// from "SEP merged it with its neighbour", which are different findings.
    pub nearest_neighbour: f64,
}

/// The truth catalogue for one frame.
pub struct Truth {
    pub stars: Vec<TruthStar>,
    /// Every catalogue star the projection placed, including those off the frame — reported so
    /// the in-frame fraction is visible rather than assumed.
    pub total_generated: usize,
}

impl Truth {
    /// Converts renderer row order into FITS row order.
    ///
    /// # The trap this exists for
    ///
    /// `Exposure::render` returns a row-major buffer with row 0 at the *top*. The simulator's
    /// FITS writer then emits the rows **in reverse** — `for row in (0..height).rev()` in
    /// `crates/astroctl-drivers/src/simulator/fits.rs`, because FITS numbers rows from the
    /// bottom. So the first row in the file is the *last* row the renderer produced.
    ///
    /// Anything that reads the file linearly — this spike, astropy, `sep` in Python, DS9 —
    /// therefore sees a vertically mirrored copy of the renderer's coordinate system. Truth
    /// computed from `Projection::to_pixel` is in renderer coordinates and must be flipped to
    /// meet it.
    ///
    /// **Skipping this does not produce a small error, it produces zero matches** — which is
    /// how it was found here: 37 plausible detections, 980 truth stars, and not one pair within
    /// 3 px. A detector that finds nothing looks like a broken detector, and the temptation is
    /// to go and tune the threshold.
    ///
    /// The column is unaffected; only rows are reversed.
    pub fn into_fits_row_order(mut self, height: u32) -> Self {
        let last = f64::from(height) - 1.0;
        for star in &mut self.stars {
            star.row = last - star.row;
        }
        self
    }
}

/// Computes the true star positions for a frame.
///
/// `margin` is how far outside the frame a star may sit and still be included; it exists because
/// a star just off the edge still spills flux into the frame and SEP will legitimately find
/// something there.
pub fn compute(
    seed: u64,
    pointing: RaDec,
    width: u32,
    height: u32,
    arcsec_per_pixel: f64,
    margin_px: f64,
) -> Truth {
    let field = StarField::new(seed);
    let projection = Projection::new(pointing, width, height, arcsec_per_pixel);

    // The same radius the renderer asks for, plus the margin, so nothing that could light a
    // pixel is left out of truth.
    let radius = projection.radius_degrees(width, height) + margin_px * arcsec_per_pixel / 3600.0;
    let catalogue: Vec<CatalogStar> = field.stars_within(pointing, radius);
    let total_generated = catalogue.len();

    let mut stars: Vec<TruthStar> = Vec::new();
    for star in &catalogue {
        let Some((column, row)) = projection.to_pixel(star.ra_degrees, star.dec_degrees) else {
            continue;
        };
        if column < -margin_px
            || row < -margin_px
            || column > f64::from(width) + margin_px
            || row > f64::from(height) + margin_px
        {
            continue;
        }
        stars.push(TruthStar {
            column,
            row,
            magnitude: star.magnitude,
            nearest_neighbour: f64::INFINITY,
        });
    }

    // Nearest-neighbour distance, brute force. O(n^2) over ~1000 stars is a millisecond and this
    // is a spike; a production version would grid it.
    for i in 0..stars.len() {
        let mut best = f64::INFINITY;
        for j in 0..stars.len() {
            if i == j {
                continue;
            }
            let dx = stars[i].column - stars[j].column;
            let dy = stars[i].row - stars[j].row;
            let d = (dx * dx + dy * dy).sqrt();
            if d < best {
                best = d;
            }
        }
        stars[i].nearest_neighbour = best;
    }

    Truth {
        stars,
        total_generated,
    }
}

/// A detection paired with the truth star it matched, if any.
#[derive(Debug, Clone, Copy)]
pub struct Match {
    pub truth_index: usize,
    pub detection_index: usize,
    pub dx: f64,
    pub dy: f64,
    pub distance: f64,
}

/// The result of scoring one catalogue against one truth list.
pub struct Score {
    pub matches: Vec<Match>,
    /// Truth stars with no detection within `tolerance_px`.
    pub missed: Vec<usize>,
    /// Detections with no truth star within `tolerance_px`.
    pub spurious: Vec<usize>,
}

impl Score {
    /// Centroid RMS over matched pairs, in pixels.
    pub fn centroid_rms(&self) -> f64 {
        if self.matches.is_empty() {
            return f64::NAN;
        }
        let sum: f64 = self.matches.iter().map(|m| m.distance * m.distance).sum();
        (sum / self.matches.len() as f64).sqrt()
    }

    /// Per-axis RMS, which is the number a guiding loop actually inherits — it corrects in two
    /// axes independently, so the radial figure overstates each one by root-2.
    pub fn axis_rms(&self) -> (f64, f64) {
        if self.matches.is_empty() {
            return (f64::NAN, f64::NAN);
        }
        let n = self.matches.len() as f64;
        let sx: f64 = self.matches.iter().map(|m| m.dx * m.dx).sum();
        let sy: f64 = self.matches.iter().map(|m| m.dy * m.dy).sum();
        ((sx / n).sqrt(), (sy / n).sqrt())
    }

    /// Median radial error — reported alongside RMS because a handful of blended pairs drag an
    /// RMS a long way and the median says whether that is what happened.
    pub fn centroid_median(&self) -> f64 {
        if self.matches.is_empty() {
            return f64::NAN;
        }
        let mut d: Vec<f64> = self.matches.iter().map(|m| m.distance).collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        d[d.len() / 2]
    }

    /// Largest radial error.
    pub fn centroid_max(&self) -> f64 {
        self.matches
            .iter()
            .map(|m| m.distance)
            .fold(0.0_f64, f64::max)
    }
}

/// Greedy nearest-neighbour matching, closest pairs first.
///
/// Greedy-by-distance rather than per-truth-nearest: with a crowded field, "each truth star takes
/// its nearest detection" lets one detection be claimed twice and inflates completeness. Sorting
/// all candidate pairs and consuming them in order gives a one-to-one matching, which is what the
/// completeness and false-positive numbers have to mean to be worth anything.
pub fn score(
    truth: &[TruthStar],
    detections: &[(f64, f64)],
    tolerance_px: f64,
) -> Score {
    let mut candidates: Vec<Match> = Vec::new();
    for (ti, t) in truth.iter().enumerate() {
        for (di, (dx_col, dy_row)) in detections.iter().enumerate() {
            let dx = dx_col - t.column;
            let dy = dy_row - t.row;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance <= tolerance_px {
                candidates.push(Match {
                    truth_index: ti,
                    detection_index: di,
                    dx,
                    dy,
                    distance,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        a.distance
            .partial_cmp(&b.distance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut truth_taken = vec![false; truth.len()];
    let mut det_taken = vec![false; detections.len()];
    let mut matches = Vec::new();
    for c in candidates {
        if truth_taken[c.truth_index] || det_taken[c.detection_index] {
            continue;
        }
        truth_taken[c.truth_index] = true;
        det_taken[c.detection_index] = true;
        matches.push(c);
    }

    let missed = (0..truth.len()).filter(|i| !truth_taken[*i]).collect();
    let spurious = (0..detections.len()).filter(|i| !det_taken[*i]).collect();

    Score {
        matches,
        missed,
        spurious,
    }
}

/// One magnitude bin of the completeness curve.
pub struct Bin {
    pub low: f64,
    pub high: f64,
    pub truth: usize,
    pub detected: usize,
    /// Of the missed ones, how many had a neighbour closer than the detection tolerance —
    /// i.e. were plausibly blended rather than simply too faint.
    pub missed_blended: usize,
    pub rms: f64,
}

impl Bin {
    pub fn completeness(&self) -> f64 {
        if self.truth == 0 {
            return f64::NAN;
        }
        self.detected as f64 / self.truth as f64
    }
}

/// Completeness as a function of magnitude.
pub fn bins(truth: &[TruthStar], score: &Score, blend_px: f64, step: f64) -> Vec<Bin> {
    if truth.is_empty() {
        return Vec::new();
    }
    let mut matched_for = vec![None; truth.len()];
    for m in &score.matches {
        matched_for[m.truth_index] = Some(m.distance);
    }

    let lo = truth
        .iter()
        .map(|t| t.magnitude)
        .fold(f64::INFINITY, f64::min);
    let hi = truth
        .iter()
        .map(|t| t.magnitude)
        .fold(f64::NEG_INFINITY, f64::max);

    let start = (lo / step).floor() * step;
    let mut out = Vec::new();
    let mut edge = start;
    while edge < hi + step {
        let next = edge + step;
        let mut bin = Bin {
            low: edge,
            high: next,
            truth: 0,
            detected: 0,
            missed_blended: 0,
            rms: 0.0,
        };
        let mut sq = 0.0;
        for (i, t) in truth.iter().enumerate() {
            if t.magnitude < edge || t.magnitude >= next {
                continue;
            }
            bin.truth += 1;
            match matched_for[i] {
                Some(d) => {
                    bin.detected += 1;
                    sq += d * d;
                }
                None => {
                    if t.nearest_neighbour < blend_px {
                        bin.missed_blended += 1;
                    }
                }
            }
        }
        if bin.detected > 0 {
            bin.rms = (sq / bin.detected as f64).sqrt();
        }
        if bin.truth > 0 {
            out.push(bin);
        }
        edge = next;
    }
    out
}
