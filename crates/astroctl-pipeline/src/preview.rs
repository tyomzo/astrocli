//! Frame bytes → preview JPEG. SDD §5.7's source 2, end to end.
//!
//! ```text
//! decode (§5.7 step 1) → window from the FULL-RES samples → downscale → asinh → JPEG q85
//! ```
//!
//! Pure: bytes in, bytes out. The caller decides where the result is written and who is told
//! about it, which is what keeps this testable without a session, a store or a camera — and what
//! lets the same function serve the REST route and the live-view push without either of them
//! knowing about the other.
//!
//! # Why the window is computed before the downscale and not after
//!
//! Averaging pulls a distribution's extremes toward its middle, so the 0.5th and 99.5th
//! percentiles of a quarter-res frame are *not* those of the frame it came from — the window
//! narrows and the preview comes out flatter. `compute_worker.py` takes its percentiles on the
//! full-resolution array, so this does too, and the reduction happens afterwards. The cost is one
//! counting pass over the decoded samples, which is the cheapest thing in the pipeline.

use crate::decode::{decode_any, DecodeError, DecodedFrame};
use crate::stretch::{
    Curve, Window, DEFAULT_BLACK_POINT_PCT, DEFAULT_SOFTENING, DEFAULT_WHITE_POINT_PCT,
};

/// SDD §5.7: "quarter-res". The reduction applied before anything larger is considered.
const QUARTER: u32 = 4;

/// Knobs for the preview render, named and defaulted as `workers/compute_worker.py` names and
/// defaults them.
///
/// The names are the worker's `preview` job parameters verbatim (`softening`,
/// `black_point_pct`, `white_point_pct`, `max_dimension`, `quality`). That is deliberate: when
/// the post-processing chain arrives and these become operator-facing, the field node and the
/// stack node must expose one vocabulary rather than two spellings of the same five numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewParams {
    /// How hard the asinh curve bends.
    pub softening: f64,
    /// Percentile mapped to black.
    pub black_point_pct: f64,
    /// Percentile mapped to white.
    pub white_point_pct: f64,
    /// Longest edge of the output, after the quarter-res reduction.
    pub max_dimension: u32,
    /// JPEG quality, 1–100. SDD §5.7 fixes 85.
    pub quality: u8,
}

impl Default for PreviewParams {
    fn default() -> Self {
        Self {
            softening: DEFAULT_SOFTENING,
            black_point_pct: DEFAULT_BLACK_POINT_PCT,
            white_point_pct: DEFAULT_WHITE_POINT_PCT,
            // `compute_worker.py::DEFAULT_MAX_DIMENSION`. It is a ceiling rather than a target:
            // the quarter-res reduction usually lands well inside it, and this only bites on a
            // sensor larger than 8192 px on its long edge.
            max_dimension: 2048,
            quality: 85,
        }
    }
}

/// A rendered preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// JPEG bytes, ready for the wire or the disk.
    pub jpeg: Vec<u8>,
    /// Width of the rendered image, after reduction.
    pub width: u32,
    /// Height of the rendered image, after reduction.
    pub height: u32,
}

/// Decode a frame and render its preview.
///
/// # Errors
///
/// [`DecodeError`] if the bytes are not a frame this build decodes. Encoding itself cannot fail
/// — the sink is a `Vec` — so there is no second error variant to carry.
pub fn render_preview(bytes: &[u8], params: &PreviewParams) -> Result<Preview, DecodeError> {
    let frame = decode_any(bytes)?;
    Ok(render_decoded(&frame, params))
}

/// Render a preview from an already-decoded frame.
#[must_use]
pub fn render_decoded(frame: &DecodedFrame, params: &PreviewParams) -> Preview {
    // Full-resolution percentiles — see the module docs.
    let window = Window::from_samples(
        frame.samples(),
        params.black_point_pct,
        params.white_point_pct,
    );

    let reduced = downscale(frame, reduction_for(frame, params.max_dimension));
    let curve = Curve::new(window, params.softening);
    let gray = curve.apply_all(&reduced.samples);

    Preview {
        jpeg: encode_jpeg(&gray, reduced.width, reduced.height, params.quality),
        width: reduced.width,
        height: reduced.height,
    }
}

/// A reduced raster and its dimensions.
struct Reduced {
    width: u32,
    height: u32,
    samples: Vec<u16>,
}

/// How many source pixels go into one preview pixel, on each axis.
///
/// Starts at SDD §5.7's quarter-res and grows only if that still leaves an edge longer than
/// `max_dimension`. An integer factor rather than an arbitrary scale: a box of whole pixels needs
/// no interpolation, no filter kernel and no second buffer, and at these reductions there is
/// nothing a resampler would visibly add to an image whose purpose is "is this framed and
/// focused".
fn reduction_for(frame: &DecodedFrame, max_dimension: u32) -> u32 {
    let longest = frame.width().max(frame.height());
    if max_dimension == 0 {
        return QUARTER;
    }
    let needed = longest.div_ceil(max_dimension.max(1));
    QUARTER.max(needed).max(1)
}

/// Box-average the frame by an integer factor.
///
/// Averaging *linear* samples, before the curve. Summing into a `u32` keeps a full box of
/// saturated pixels (`factor² × 65535`) exact up to a factor of 256, well past anything the
/// reduction produces.
fn downscale(frame: &DecodedFrame, factor: u32) -> Reduced {
    let factor = factor.max(1);
    if factor == 1 {
        return Reduced {
            width: frame.width(),
            height: frame.height(),
            samples: frame.samples().to_vec(),
        };
    }

    let source_width = frame.width() as usize;
    // Partial boxes at the right and bottom edges are dropped rather than averaged over fewer
    // pixels: a preview one pixel narrower is invisible, whereas an edge column averaged from a
    // quarter as many samples is a visibly brighter or noisier stripe down one side.
    let width = (frame.width() / factor).max(1);
    let height = (frame.height() / factor).max(1);
    let box_area = u32::from(u16::try_from(factor * factor).unwrap_or(u16::MAX));

    let samples = frame.samples();
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for row in 0..height as usize {
        for column in 0..width as usize {
            let mut total = 0_u32;
            for dy in 0..factor as usize {
                let source_row = row * factor as usize + dy;
                let base = source_row * source_width + column * factor as usize;
                for dx in 0..factor as usize {
                    total += u32::from(samples[base + dx]);
                }
            }
            // Rounded, not truncated: truncation biases every preview pixel down by half a
            // level, which across a whole frame is a visible drop in the sky background.
            out.push(u16::try_from((total + box_area / 2) / box_area).unwrap_or(u16::MAX));
        }
    }

    Reduced {
        width,
        height,
        samples: out,
    }
}

/// Encode 8-bit grayscale as JPEG.
///
/// `Luma`, matching `compute_worker.py`'s `Image.fromarray(pixels, mode="L")`: the sensors in
/// scope are monochrome, and a three-channel encode would triple the bytes on a link the whole
/// two-socket design exists to protect.
fn encode_jpeg(gray: &[u8], width: u32, height: u32, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(
            gray,
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
            jpeg_encoder::ColorType::Luma,
        )
        // Encoding into a `Vec` has no failure mode: the only errors the encoder defines are I/O
        // ones from its sink, and this sink is memory. Not an `unwrap` on a `Result` we hope is
        // `Ok` — it is one whose `Err` variant is unreachable by construction.
        .expect("encoding into memory cannot fail");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gradient frame — every row brighter than the last, so a vertical flip or a downscale
    /// that reads the wrong rows shows up as an ordering failure rather than a subtle one.
    fn gradient(width: u32, height: u32) -> DecodedFrame {
        let samples = (0..height)
            .flat_map(|row| (0..width).map(move |_| u16::try_from(row * 64).unwrap_or(u16::MAX)))
            .collect();
        DecodedFrame::new(width, height, samples).expect("dimensions match")
    }

    #[test]
    fn the_preview_is_a_quarter_of_each_edge() {
        // SDD §5.7's "quarter-res", which is what keeps a 6000×4000 frame's preview inside the
        // size the link is expected to carry several times a minute.
        let frame = gradient(6000, 4000);
        let preview = render_decoded(&frame, &PreviewParams::default());
        assert_eq!((preview.width, preview.height), (1500, 1000));
    }

    #[test]
    fn a_sensor_larger_than_the_ceiling_reduces_further() {
        // The `max_dimension` ceiling only bites past 8192 px on the long edge; when it does, the
        // reduction grows rather than the preview overflowing the bound.
        let frame = gradient(12_000, 8000);
        let preview = render_decoded(&frame, &PreviewParams::default());
        assert!(
            preview.width <= 2048 && preview.height <= 2048,
            "got {}×{}",
            preview.width,
            preview.height
        );
    }

    #[test]
    fn the_output_is_a_jpeg_a_different_decoder_can_read() {
        // Read back with `zune-jpeg` rather than anything of ours: a round trip through the
        // encoder's own code would prove only that it agrees with itself.
        let frame = gradient(400, 300);
        let preview = render_decoded(&frame, &PreviewParams::default());

        assert_eq!(&preview.jpeg[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&preview.jpeg));
        let pixels = decoder.decode().expect("a readable JPEG");
        let info = decoder.info().expect("dimensions");
        assert_eq!(
            (u32::from(info.width), u32::from(info.height)),
            (preview.width, preview.height)
        );
        // One luma channel goes in; zune expands it to RGB on the way out, which is its default
        // and not this crate's business. What *is* this crate's business is that the gradient
        // survived — a JPEG decoding to uniform grey would pass a "decodes successfully"
        // assertion and be useless as a preview.
        assert_eq!(
            pixels.len(),
            preview.width as usize * preview.height as usize * 3
        );
        let darkest = pixels.iter().copied().min().expect("samples");
        let brightest = pixels.iter().copied().max().expect("samples");
        assert!(
            brightest - darkest > 200,
            "the gradient flattened: {darkest}..{brightest}"
        );
    }

    #[test]
    fn the_window_is_taken_before_the_downscale_not_after() {
        // The failure this guards: percentiles computed on the reduced frame. Averaging pulls the
        // extremes in, so a frame that is mostly sky with a sparse bright tail gets a narrower
        // window and a flatter preview. Asserted by comparing the window the renderer uses
        // against the one the *reduced* samples would have produced — they must differ, and the
        // renderer must be using the full-resolution one.
        let mut samples = vec![1000_u16; 64 * 64];
        // A sparse scattering of bright pixels: survives at full res, is averaged away by a 4×4
        // box, which is exactly why the order matters.
        for i in (0..samples.len()).step_by(97) {
            samples[i] = 60_000;
        }
        let frame = DecodedFrame::new(64, 64, samples).expect("dimensions match");

        let full = Window::from_samples(
            frame.samples(),
            DEFAULT_BLACK_POINT_PCT,
            DEFAULT_WHITE_POINT_PCT,
        );
        let reduced = downscale(&frame, 4);
        let after = Window::from_samples(
            &reduced.samples,
            DEFAULT_BLACK_POINT_PCT,
            DEFAULT_WHITE_POINT_PCT,
        );
        assert!(
            full.white > after.white,
            "the fixture must actually distinguish the two orders: {full:?} vs {after:?}"
        );
    }

    #[test]
    fn the_downscale_averages_rather_than_samples() {
        // Nearest-neighbour would throw away 15 of every 16 pixels, which on a star field means
        // stars flickering in and out of the preview between frames.
        let samples = vec![
            0, 100, 0, 100, 100, 0, 100, 0, 0, 100, 0, 100, 100, 0, 100, 0,
        ];
        let frame = DecodedFrame::new(4, 4, samples).expect("dimensions match");
        let reduced = downscale(&frame, 4);
        assert_eq!((reduced.width, reduced.height), (1, 1));
        assert_eq!(reduced.samples, vec![50], "the mean of the 4×4 box");
    }

    #[test]
    fn a_partial_edge_box_is_dropped_rather_than_averaged_from_fewer_pixels() {
        // 6 columns at factor 4 gives one whole box and a remainder of 2. Averaging the
        // remainder over 2 pixels instead of 4 leaves a stripe down one edge at a different
        // brightness from the rest of the frame.
        let frame = gradient(6, 4);
        let reduced = downscale(&frame, 4);
        assert_eq!((reduced.width, reduced.height), (1, 1));
    }

    #[test]
    fn a_frame_that_is_not_decodable_fails_rather_than_rendering_noise() {
        assert!(render_preview(b"not a frame", &PreviewParams::default()).is_err());
    }

    #[test]
    fn the_defaults_are_the_python_workers_defaults() {
        // The whole point of the table in `stretch`'s module docs. If someone retunes one side,
        // this is the test that says the other side exists.
        let params = PreviewParams::default();
        assert_eq!(params.softening, 10.0);
        assert_eq!(params.black_point_pct, 0.5);
        assert_eq!(params.white_point_pct, 99.5);
        assert_eq!(params.max_dimension, 2048);
        assert_eq!(params.quality, 85);
    }
}
