//! The JPEG arm of [`crate::decode`]. M2-T05.
//!
//! # Why the decode path needs this at all
//!
//! Two reasons, and only one of them is the one M2-T05 names.
//!
//! The one it names is the cheap preview: with `camera.default_format: "RAW+JPEG"` the R10 writes
//! a CR3 *and* a camera JPEG per exposure (M2-T03), and rendering the preview from the JPEG is far
//! cheaper than decompressing a 24 MP wavelet-coded mosaic. **That optimisation is not reachable
//! today** — `astroctl_field::camera` deletes every file but the raw before publishing
//! `frame.saved`, so by the time the preview pipeline sees a path the sibling is gone. See the
//! M2-T05 task notes; closing that loop is a field-node change and this crate is not where it
//! belongs.
//!
//! The one it does not name is the gap that makes this arm worth having anyway: `ImageFormat`
//! offers **`JPEG` as a capture format in its own right**, an operator may select it, and until
//! this arm existed such a frame reached [`crate::decode::decode_any`] and came back
//! `UnknownFormat` — a durable frame on disk that the node could not preview and logged as "the
//! frame could not be previewed" once per capture, forever.
//!
//! # What this arm cannot promise, and says so here rather than in a bug report
//!
//! [`crate::decode::DecodedFrame`] promises *linear* samples, and a camera JPEG is not linear: the
//! body has already applied its picture style, white balance and an sRGB transfer curve. The
//! asinh stretch downstream is designed for linear light, so on a JPEG it operates on data that
//! has been tone-mapped once already and lifts the shadows further than it would on the raw.
//!
//! The result is a usable preview and a *different* one from the CR3 path — never a wrong one, but
//! not the same image. The raw arm is the one to trust for judging a sub; this arm is for showing
//! the operator what the body recorded when the body recorded nothing else.

use crate::decode::{DecodeError, DecodedFrame};

/// Decode a JPEG into grayscale samples.
///
/// # Errors
///
/// [`DecodeError::Malformed`] if the bytes are not a readable JPEG, [`DecodeError::Unsupported`]
/// for an image too large to address.
pub(crate) fn decode_jpeg(bytes: &[u8]) -> Result<DecodedFrame, DecodeError> {
    // Luma out, rather than zune's default RGB. A camera JPEG is a colour image and the pipeline
    // renders one channel (`DecodedFrame`'s docs, and `encode_jpeg`'s `ColorType::Luma`), so the
    // choice is between letting the decoder do the RGB→Y conversion during its own colour
    // transform and doing it afterwards over three times the buffer. The decoder's is both
    // cheaper and the standard's own coefficients.
    //
    // `zune-jpeg` is also this crate's *test* reader for the JPEGs it writes (`preview.rs`). That
    // stays independent: the tests check our encoder against a foreign decoder, which is a
    // different direction from this, our decoder for foreign encoders.
    let options = zune_jpeg::zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::Luma);
    let mut decoder =
        zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);

    let pixels = decoder
        .decode()
        .map_err(|error| DecodeError::Malformed(format!("the JPEG could not be read: {error}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| DecodeError::Malformed("the JPEG carries no dimensions".to_owned()))?;

    let width = u32::from(info.width);
    let height = u32::from(info.height);
    let expected = width as usize * height as usize;
    if pixels.len() != expected {
        return Err(DecodeError::Malformed(format!(
            "the JPEG decoded to {} samples for a {width}×{height} image",
            pixels.len()
        )));
    }

    // 8-bit to 16-bit by `× 257`, not `<< 8`. Both map 0 to 0, but only 257 maps 255 to 65 535 —
    // a shift leaves the top of the range 255 short of white, which the percentile window would
    // then find and treat as the true white point. Harmless on one frame, visible as a shift when
    // a JPEG preview sits next to a raw one.
    let samples = pixels
        .into_iter()
        .map(|value| u16::from(value) * 257)
        .collect();
    DecodedFrame::new(width, height, samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG this crate wrote, read back through the arm that reads foreign ones. The point is
    /// not the round trip — it is that the arm produces the dimensions and the sample count the
    /// header declares, which is what every downstream stage indexes by.
    #[test]
    fn a_jpeg_decodes_to_one_sample_per_pixel() {
        let gray: Vec<u8> = (0..64 * 48).map(|i| (i % 251) as u8).collect();
        let mut encoded = Vec::new();
        jpeg_encoder::Encoder::new(&mut encoded, 90)
            .encode(&gray, 64, 48, jpeg_encoder::ColorType::Luma)
            .expect("encode");

        let frame = decode_jpeg(&encoded).expect("a readable JPEG");
        assert_eq!((frame.width(), frame.height()), (64, 48));
        assert_eq!(frame.samples().len(), 64 * 48);
    }

    #[test]
    fn the_full_eight_bit_range_reaches_the_full_sixteen_bit_range() {
        // The `× 257` claim in the decoder, checked at both ends through a lossless-enough path:
        // a flat white patch survives any quality setting.
        let white = vec![255_u8; 32 * 32];
        let mut encoded = Vec::new();
        jpeg_encoder::Encoder::new(&mut encoded, 100)
            .encode(&white, 32, 32, jpeg_encoder::ColorType::Luma)
            .expect("encode");

        let frame = decode_jpeg(&encoded).expect("a readable JPEG");
        let brightest = frame.samples().iter().copied().max().expect("samples");
        assert_eq!(brightest, u16::MAX, "255 must reach 65535, not 65280");
    }

    #[test]
    fn bytes_that_are_not_a_jpeg_are_malformed_rather_than_a_panic() {
        let error = decode_jpeg(b"\xFF\xD8\xFF\xE0 and then nothing valid").unwrap_err();
        assert!(
            matches!(error, DecodeError::Malformed(_)),
            "expected Malformed, got {error:?}"
        );
    }
}
