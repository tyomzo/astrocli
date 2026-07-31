//! The CR3 arm of [`crate::decode`], via `rawler`. SDD §5.7 source 1, M2-T05.
//!
//! Bytes in, [`DecodedFrame`] out — the same contract [`crate::decode::decode_fits`] honours, so
//! everything downstream (quarter-res box average, asinh stretch, JPEG encode) is reached
//! unchanged. That is the whole point of the exercise: M1-T09 built the enum so that M2 would add
//! an arm, not a pipeline.
//!
//! # "Half-size decode" is a binning step, not a decoder flag
//!
//! M2-T05 asks for *half-size decode → quarter-res → stretch*, and it is worth being precise about
//! where the halving happens, because `rawler` has no half-size mode and could not usefully have
//! one: CRX is a wavelet codec whose coefficients are not separable into a cheap low-resolution
//! pass through the public API. The full CFA plane is decoded either way.
//!
//! The halving is therefore what `dcraw -h` means by it and what every raw previewer does: **one
//! output sample per 2×2 CFA cell**, no interpolation. That is the cheapest correct reduction of a
//! Bayer mosaic to a grayscale raster — it costs one pass over the plane, it introduces no colour
//! artefacts because it never pretends to reconstruct colour, and it lands on exactly the
//! [`DecodedFrame`] shape the FITS path produces.
//!
//! # Why averaging all four photosites needs no knowledge of the CFA pattern
//!
//! A 2×2 Bayer cell holds one red, two green and one blue photosite in *some* order — RGGB on this
//! body, but BGGR and GRBG exist. Averaging all four gives the same number whatever the order, so
//! this decoder never asks `rawler` which pattern it found. The same algebra covers the black
//! level, which is the part that looks like it should need the pattern and does not:
//!
//! ```text
//! mean(pᵢ − bᵢ) = mean(pᵢ) − mean(bᵢ)
//! ```
//!
//! so subtracting the *mean* of the four per-channel black levels from the *mean* of the four
//! photosites is exact, not an approximation. It would stop being exact only if the cell were
//! averaged with unequal weights, which is precisely the colour reconstruction this path declines
//! to do.
//!
//! # Why the frame is *not* rescaled to full `u16` range
//!
//! The obvious next step after subtracting the black level is to rescale by `(white − black)` so
//! the frame fills a `u16` and the stretch's 65 536-entry lookup table is fully used. This decoder
//! deliberately does not, and a real R10 frame is why.
//!
//! `rawler`'s profile for this body reports `whitelevel = 12735`, but the photosites in a
//! saturated frame read **16383** — the 14-bit clipping point. The profile's white level is the
//! top of the *linear* response, not the top of the data, and those are different numbers.
//! Rescaling by it maps every value above 12735 past `u16::MAX`, so the brightest quarter of the
//! sensor's range clamps to one value before anything downstream sees it. On a star field that is
//! the stars.
//!
//! Rescaling turns out to buy nothing anyway. [`crate::stretch::Window`] takes its black and white
//! points from *percentiles of the frame*, so any monotonic linear map of the samples produces a
//! byte-identical preview — the window simply lands in a different place. The lookup table's
//! resolution is not a real concern either: ~14 000 input levels feed an 8-bit output, which is
//! oversampled by a factor of fifty.
//!
//! So the samples leave here as black-corrected sensor units, which is what [`DecodedFrame`] says
//! it holds, with nothing clipped and nothing invented.

use rawler::decoders::RawDecodeParams;
use rawler::rawsource::RawSource;
use rawler::RawImage;

use crate::decode::{DecodeError, DecodedFrame};

/// Decode a Canon CR3 into half-size grayscale linear samples.
///
/// # Errors
///
/// [`DecodeError::Malformed`] if `rawler` cannot parse the container, [`DecodeError::Unsupported`]
/// for a raw this path deliberately does not render (see the arms below).
pub(crate) fn decode_cr3(bytes: &[u8]) -> Result<DecodedFrame, DecodeError> {
    // `new_from_slice` rather than a path: this crate opens no files (module docs on
    // `crate::decode`), and the caller has already read the frame on the blocking pool.
    let source = RawSource::new_from_slice(bytes);
    let raw = rawler::decode(&source, &RawDecodeParams::default()).map_err(|error| {
        DecodeError::Malformed(format!("rawler could not read the CR3: {error}"))
    })?;

    // One component per pixel is the CFA mosaic this path is built for. A `cpp` of 3 means the
    // decoder already produced interpolated RGB — a linear DNG, say — which is a different
    // reduction (drop to luma, no binning) and no camera in scope emits one. Refusing is better
    // than binning an RGB raster as though it were a mosaic, which would silently mix channels.
    if raw.cpp != 1 {
        return Err(DecodeError::Unsupported(format!(
            "the raw has {} components per pixel; this path renders a one-component CFA mosaic",
            raw.cpp
        )));
    }

    let rawler::RawImageData::Integer(plane) = &raw.data else {
        // Float raws exist (some DNGs). The FITS path refuses them for the same reason — see
        // `decode_fits` — and nothing in this deployment produces one.
        return Err(DecodeError::Unsupported(
            "the raw holds floating-point samples; this node renders integer frames".to_owned(),
        ));
    };

    let (origin_x, origin_y, width, height) = crop(&raw)?;
    let stride = raw.width;
    if plane.len() < stride * raw.height {
        return Err(DecodeError::Malformed(format!(
            "the CFA plane is {} samples, short of the {}×{} the header declares",
            plane.len(),
            raw.width,
            raw.height
        )));
    }

    // The summed black level of one 2×2 cell — the units the loop's `cell` is in, so the
    // subtraction below costs nothing per pixel.
    let black = cell_black(&raw);

    // The 2×2 bin, in integers throughout. Four samples of at most 65 535 cannot overflow a
    // `u32`, and the only division is the `>> 2` that turns the cell sum back into a mean.
    let out_width = width / 2;
    let out_height = height / 2;
    let mut samples = Vec::with_capacity(out_width * out_height);
    for out_y in 0..out_height {
        let top = (origin_y + out_y * 2) * stride + origin_x;
        let bottom = top + stride;
        for out_x in 0..out_width {
            let left = out_x * 2;
            let cell = u32::from(plane[top + left])
                + u32::from(plane[top + left + 1])
                + u32::from(plane[bottom + left])
                + u32::from(plane[bottom + left + 1]);
            // Saturating, because the optical-black region genuinely reads below the nominal
            // black level — that is what read noise looks like from underneath — and a preview
            // has no use for negative light.
            let above_black = cell.saturating_sub(black) / 4;
            // A 16-bit plane minus a non-negative black level cannot exceed `u16::MAX`, so this
            // narrowing is lossless. The fallback saturates rather than wrapping, because a raw
            // that broke that reasoning should render a blown pixel, not a black one.
            samples.push(u16::try_from(above_black).unwrap_or(u16::MAX));
        }
    }

    DecodedFrame::new(
        u32::try_from(out_width).map_err(|_| too_big(out_width))?,
        u32::try_from(out_height).map_err(|_| too_big(out_height))?,
        samples,
    )
}

/// The rectangle of the CFA plane worth previewing, as `(x, y, width, height)`.
///
/// `crop_area` before `active_area` before the whole plane. The three differ by the sensor's
/// masked borders: `active_area` is everything that saw light, `crop_area` is the smaller
/// rectangle the camera itself would have delivered as a JPEG. `crop_area` is the right default
/// here because the preview sits next to the body's own screen during a desk session, and a
/// preview framed a few dozen pixels wider than the shot the operator composed reads as a bug.
/// The optical-black border is worth excluding for a second reason: it is a band of near-zero
/// pixels, and [`crate::stretch::Window`] takes its black point from a percentile of the whole
/// frame, so leaving it in drags the black point down and flattens the image.
///
/// Both origin and extent are forced even. The 2×2 bin has to land on CFA cell boundaries or the
/// "averaging all four covers every pattern" argument in the module docs stops holding — an odd
/// origin would put two greens and one red and one blue from *adjacent* cells into one output
/// sample, which is still a number but no longer a defensible one.
fn crop(raw: &RawImage) -> Result<(usize, usize, usize, usize), DecodeError> {
    let area = raw.crop_area.or(raw.active_area);
    let (x, y, width, height) = area.map_or((0, 0, raw.width, raw.height), |rect| {
        (rect.p.x, rect.p.y, rect.d.w, rect.d.h)
    });

    // A rectangle the header describes but the plane does not contain is a malformed file, not a
    // reason to read out of bounds.
    if x + width > raw.width || y + height > raw.height {
        return Err(DecodeError::Malformed(format!(
            "the crop rectangle {x},{y} {width}×{height} does not fit the {}×{} plane",
            raw.width, raw.height
        )));
    }

    // Rounding the origin *up* to even and the extent *down* keeps the window inside the
    // rectangle that was already checked to fit — the extent has to lose whatever the origin
    // gained, or an odd origin would push the last cell one row past the crop.
    let aligned_x = x.next_multiple_of(2);
    let aligned_y = y.next_multiple_of(2);
    let width = width.saturating_sub(aligned_x - x) & !1;
    let height = height.saturating_sub(aligned_y - y) & !1;
    let (x, y) = (aligned_x, aligned_y);

    if width < 2 || height < 2 {
        return Err(DecodeError::Malformed(format!(
            "the usable area is {width}×{height}, too small to bin"
        )));
    }
    Ok((x, y, width, height))
}

/// The summed black level of one 2×2 cell.
///
/// Already multiplied by four, which is the unit the binning loop's `cell` sum is in — so the
/// per-pixel work is one subtraction rather than a subtraction and a division.
///
/// `as_vec` flattens the black-level *pattern*: for a CFA that is the 2×2 tile of per-channel
/// levels, which is exactly the set the module docs' `mean(pᵢ − bᵢ)` identity averages over. A
/// body that reports no black level at all gets zero, which is the right answer for a raw that has
/// already had it subtracted.
fn cell_black(raw: &RawImage) -> u32 {
    let blacks = raw.blacklevel.as_vec();
    if blacks.is_empty() {
        return 0;
    }
    let mean = blacks.iter().sum::<f32>() / blacks.len() as f32;
    // Negative is not a black level; a raw claiming one is treated as claiming none.
    (mean.max(0.0) * 4.0) as u32
}

fn too_big(dimension: usize) -> DecodeError {
    DecodeError::Unsupported(format!(
        "the binned frame is {dimension} pixels on a side, past what this pipeline addresses"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity the module docs rest on, checked rather than asserted in prose: averaging a
    /// 2×2 cell and subtracting the mean black level equals averaging the four black-corrected
    /// photosites, whatever order the CFA puts them in.
    #[test]
    fn the_cell_mean_is_pattern_independent() {
        let photosites = [4000.0_f32, 5000.0, 5100.0, 900.0];
        let black_levels = [512.0_f32, 500.0, 500.0, 520.0];

        let corrected_then_averaged: f32 = photosites
            .iter()
            .zip(black_levels)
            .map(|(p, b)| p - b)
            .sum::<f32>()
            / 4.0;
        let averaged_then_corrected =
            photosites.iter().sum::<f32>() / 4.0 - black_levels.iter().sum::<f32>() / 4.0;

        assert!((corrected_then_averaged - averaged_then_corrected).abs() < 1e-3);
    }

    #[test]
    fn a_raw_that_is_not_a_raw_is_malformed_rather_than_a_panic() {
        let error = decode_cr3(b"\0\0\0\x18ftypcrx not really a raw file at all").unwrap_err();
        assert!(
            matches!(error, DecodeError::Malformed(_)),
            "expected Malformed, got {error:?}"
        );
    }
}
