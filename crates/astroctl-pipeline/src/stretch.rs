//! The asinh auto-stretch. SDD §5.7 step 3.
//!
//! # This is a second implementation of an algorithm that already exists
//!
//! `workers/compute_worker.py::_asinh_stretch` renders the *stack* node's previews and this
//! renders the *field* node's, and the operator sees both — often of the same exposure, minutes
//! apart. Two implementations that disagree about the curve would show them the same frame twice
//! with different contrast and no way to tell which was true, so the semantics here are matched
//! to the Python deliberately and asserted, not merely intended to be similar:
//!
//! | | `compute_worker.py` | here |
//! |---|---|---|
//! | black point | `numpy.percentile(finite, 0.5)` | [`percentile`], same linear interpolation |
//! | white point | `numpy.percentile(finite, 99.5)` | as above |
//! | degenerate window | `white = black + 1.0` when `white <= black` | identical |
//! | normalisation | `clip((x - black) / (white - black), 0, 1)` | identical |
//! | curve | `arcsinh(n * s) / asinh(s)`, `s = 10.0` | identical |
//! | quantisation | `(clip(c,0,1) * 255.0 + 0.5).astype("uint8")` | identical, truncating cast |
//! | percentile domain | the **full-resolution** frame | the full-resolution frame |
//!
//! The last row is the one that is easy to lose. Percentiles taken after a downscale are not the
//! same numbers — averaging pulls the extremes in, so the window narrows and the preview comes
//! out flatter. The black and white points are therefore computed on the frame as decoded, and
//! only then is the frame reduced (see [`crate::preview`]).
//!
//! **One divergence, deliberate.** The Python worker stretches at full resolution and downsamples
//! the 8-bit result; this crate downsamples the linear samples and stretches the reduction. The
//! curve, the window and the quantisation are identical, so the *tone* of the two previews is the
//! same; what differs is `mean(asinh(x))` against `asinh(mean(x))` inside a 4×4 box, which is
//! below one output level on anything but a star edge. Taken because averaging *linear* samples
//! is the photometrically correct order, and because it is 16× fewer transcendental calls in the
//! capture path on a Pi — SDD §5.7 states the pipeline in this order for the same reason.

/// Softening — how hard the curve bends. `compute_worker.py::DEFAULT_SOFTENING`.
pub const DEFAULT_SOFTENING: f64 = 10.0;
/// Black point percentile. `compute_worker.py::DEFAULT_BLACK_POINT_PCT`.
pub const DEFAULT_BLACK_POINT_PCT: f64 = 0.5;
/// White point percentile. `compute_worker.py::DEFAULT_WHITE_POINT_PCT`.
pub const DEFAULT_WHITE_POINT_PCT: f64 = 99.5;

/// Every value a `u16` sample can take — the histogram is exact rather than binned.
const LEVELS: usize = u16::MAX as usize + 1;

/// The black and white points a stretch maps between, in linear sample units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Everything at or below this renders black.
    pub black: f64,
    /// Everything at or above this renders white.
    pub white: f64,
}

impl Window {
    /// The window `compute_worker.py` would choose for these samples.
    ///
    /// Percentiles come off an exact 65,536-entry histogram rather than a sort: the frame is 24
    /// million samples and `n log n` on that is seconds of a Pi's CPU, where one counting pass
    /// and a scan of a fixed table is milliseconds. For integer samples the histogram is not an
    /// approximation — every distinct value has its own bucket, so the order statistics it
    /// yields are the ones a sort would.
    ///
    /// An empty frame yields the degenerate `0..1` window rather than a failure. The Python
    /// raises `VALIDATION` on a frame with no finite samples; that case cannot arise here,
    /// because [`crate::decode`] produces `u16` and every `u16` is finite.
    #[must_use]
    pub fn from_samples(samples: &[u16], black_pct: f64, white_pct: f64) -> Self {
        let mut histogram = vec![0_u64; LEVELS];
        for sample in samples {
            histogram[*sample as usize] += 1;
        }
        let total = samples.len() as u64;

        let black = percentile(&histogram, total, black_pct);
        let mut white = percentile(&histogram, total, white_pct);
        if white <= black {
            // A flat frame — a dark with the cap on, or a blown exposure. Not an error; it has
            // nothing to stretch, and a zero-width window would divide by zero. The Python does
            // exactly this, and the `+ 1.0` matters: it makes the two agree on what a dark frame
            // looks like rather than one showing black and the other showing noise.
            white = black + 1.0;
        }
        Self { black, white }
    }

    /// Width of the window, never zero.
    fn span(self) -> f64 {
        (self.white - self.black).max(f64::MIN_POSITIVE)
    }
}

/// The `q`th percentile of a histogram, with NumPy's default linear interpolation.
///
/// NumPy's `percentile` with `method="linear"` is defined on the *virtual index*
/// `pos = q/100 * (n - 1)`: the result interpolates between the order statistics either side of
/// it. Reproducing that exactly — rather than taking the nearest order statistic — is what makes
/// the two implementations agree to the last bit on the same frame, and it is only a few lines
/// more than getting it approximately right.
#[must_use]
pub fn percentile(histogram: &[u64], total: u64, q: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let position = (q / 100.0).clamp(0.0, 1.0) * (total - 1) as f64;
    let lower_rank = position.floor();
    let fraction = position - lower_rank;
    // `total - 1` is the largest rank, so this cannot run off the end.
    let lower_rank = lower_rank as u64;

    let low = value_at_rank(histogram, lower_rank);
    if fraction == 0.0 {
        return low;
    }
    let high = value_at_rank(histogram, lower_rank + 1);
    low + fraction * (high - low)
}

/// The value at a zero-based sorted rank, read out of a cumulative walk of the histogram.
fn value_at_rank(histogram: &[u64], rank: u64) -> f64 {
    let mut seen = 0_u64;
    for (value, count) in histogram.iter().enumerate() {
        seen += *count;
        if seen > rank {
            return value as f64;
        }
    }
    // Only reachable for a rank past the last sample, which the callers cannot produce.
    (histogram.len() - 1) as f64
}

/// A precomputed sample → 8-bit mapping.
///
/// The curve's domain is exactly the 65,536 values a `u16` can hold, so evaluating it per pixel
/// computes the same 65,536 answers over and over — 1.5 million `asinh` calls for a quarter-res
/// frame, or 24 million for a full one. The table is built once and indexed, which is not a
/// micro-optimisation on the node this runs on: it is the difference between a preview arriving
/// while the operator is still looking at the capture and arriving after they have moved on.
///
/// Being a table over the whole domain, it is *exactly* the per-pixel formula and not an
/// approximation of it — [`tests::the_lookup_table_is_exactly_the_per_pixel_formula`] asserts
/// that over every input value.
#[derive(Debug, Clone)]
pub struct Curve {
    table: Vec<u8>,
}

impl Curve {
    /// Build the curve for a window and a softening factor.
    #[must_use]
    pub fn new(window: Window, softening: f64) -> Self {
        let span = window.span();
        // `asinh(s)` is the normaliser that makes the curve pass through (0,0) and (1,1), so
        // `softening` bends the curve without changing its range. Guarded because a caller may
        // pass a softening of zero, where the ratio is 0/0.
        let normalise = softening.asinh();
        let normalise = if normalise > 0.0 {
            1.0 / normalise
        } else {
            0.0
        };

        let table = (0..LEVELS)
            .map(|value| {
                let normalized = ((value as f64 - window.black) / span).clamp(0.0, 1.0);
                let curve = (normalized * softening).asinh() * normalise;
                // `+ 0.5` then a *truncating* cast — the same round-half-up NumPy's
                // `.astype("uint8")` performs after the Python adds the same 0.5. A `round()`
                // here would differ from the worker on exact halves.
                (curve.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
            })
            .collect();
        Self { table }
    }

    /// Map one linear sample to its 8-bit output.
    #[must_use]
    pub fn apply(&self, sample: u16) -> u8 {
        self.table[sample as usize]
    }

    /// Map a whole frame.
    #[must_use]
    pub fn apply_all(&self, samples: &[u16]) -> Vec<u8> {
        samples.iter().map(|sample| self.apply(*sample)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-pixel formula, written out exactly as `compute_worker.py` writes it, for the
    /// table to be checked against.
    fn python_formula(value: f64, window: Window, softening: f64) -> u8 {
        let normalized = ((value - window.black) / (window.white - window.black)).clamp(0.0, 1.0);
        let curve = (normalized * softening).asinh() / softening.asinh();
        (curve.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    }

    #[test]
    fn the_lookup_table_is_exactly_the_per_pixel_formula() {
        // The table is an optimisation only if it is indistinguishable from the thing it
        // replaces — over the *whole* domain, not at a few sample points.
        let window = Window {
            black: 1234.5,
            white: 45_678.25,
        };
        let curve = Curve::new(window, DEFAULT_SOFTENING);
        for value in 0..=u16::MAX {
            assert_eq!(
                curve.apply(value),
                python_formula(f64::from(value), window, DEFAULT_SOFTENING),
                "the table and the formula disagree at {value}"
            );
        }
    }

    #[test]
    fn the_curve_pins_both_ends_and_rises_monotonically() {
        // asinh(n*s)/asinh(s) maps 0 to 0 and 1 to 1 for any softening, which is what makes
        // `softening` a shape control rather than a brightness control. If it ever stopped being
        // true, changing the softening would also change the exposure of every preview.
        for softening in [0.1_f64, 1.0, DEFAULT_SOFTENING, 100.0, 10_000.0] {
            let window = Window {
                black: 0.0,
                white: 65_535.0,
            };
            let curve = Curve::new(window, softening);
            assert_eq!(curve.apply(0), 0, "softening {softening} lifted the floor");
            assert_eq!(
                curve.apply(u16::MAX),
                255,
                "softening {softening} clipped the ceiling"
            );
            let mut previous = 0_u8;
            for value in (0..=u16::MAX).step_by(97) {
                let current = curve.apply(value);
                assert!(
                    current >= previous,
                    "the curve fell at {value} with softening {softening}"
                );
                previous = current;
            }
        }
    }

    #[test]
    fn a_flat_frame_widens_the_window_instead_of_dividing_by_zero() {
        // A dark with the cap on. The Python's `white = black + 1.0` is reproduced exactly so
        // the two nodes render a dark frame the same way rather than one black and one noise.
        let flat = vec![4096_u16; 1000];
        let window = Window::from_samples(&flat, DEFAULT_BLACK_POINT_PCT, DEFAULT_WHITE_POINT_PCT);
        assert_eq!(window.black, 4096.0);
        assert_eq!(window.white, 4097.0);

        let curve = Curve::new(window, DEFAULT_SOFTENING);
        assert_eq!(curve.apply(4096), 0);
    }

    #[test]
    fn percentiles_match_numpys_linear_interpolation() {
        // Values 0..=9, one of each: NumPy's virtual index is q/100*(n-1) = q/100*9.
        //
        //   numpy.percentile(numpy.arange(10), 0.5)  -> 0.045
        //   numpy.percentile(numpy.arange(10), 50)   -> 4.5
        //   numpy.percentile(numpy.arange(10), 99.5) -> 8.955
        //
        // Taking the nearest order statistic instead would give 0, 4 and 9 — visibly different
        // black and white points on a real frame.
        let mut histogram = vec![0_u64; LEVELS];
        for bucket in histogram.iter_mut().take(10) {
            *bucket = 1;
        }
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(percentile(&histogram, 10, 0.5), 0.045));
        assert!(close(percentile(&histogram, 10, 50.0), 4.5));
        assert!(close(percentile(&histogram, 10, 99.5), 8.955));
        assert!(close(percentile(&histogram, 10, 0.0), 0.0));
        assert!(close(percentile(&histogram, 10, 100.0), 9.0));
    }

    #[test]
    fn the_window_comes_from_the_default_percentiles_not_the_extremes() {
        // The point of 0.5/99.5 rather than min/max: two dead pixels and two saturated stars must
        // not set the window for the whole frame. A min/max stretch of this fixture would map
        // 0→black and 65535→white, leaving the entire signal — the 1000..1999 ramp — squeezed
        // into the bottom 3% of the curve and the preview looking empty.
        let mut samples: Vec<u16> = (0..1000_u16).map(|i| 1000 + i).collect();
        samples[0] = 0;
        samples[1] = 0;
        samples[998] = 65_535;
        samples[999] = 65_535;

        let window =
            Window::from_samples(&samples, DEFAULT_BLACK_POINT_PCT, DEFAULT_WHITE_POINT_PCT);
        assert!(
            window.black > 1000.0 && window.black < 1010.0,
            "the 0.5th percentile must clear the dead pixels and land in the signal, got {}",
            window.black
        );
        assert!(
            window.white > 1990.0 && window.white < 2000.0,
            "the 99.5th percentile must sit below the saturated tail, got {}",
            window.white
        );
    }

    #[test]
    fn an_empty_frame_yields_a_usable_window_rather_than_a_panic() {
        let window = Window::from_samples(&[], DEFAULT_BLACK_POINT_PCT, DEFAULT_WHITE_POINT_PCT);
        assert_eq!(window.black, 0.0);
        assert_eq!(window.white, 1.0);
    }
}
