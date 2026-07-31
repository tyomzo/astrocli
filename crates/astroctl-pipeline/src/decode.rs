//! Source frame → linear samples. SDD §5.7 step 1.
//!
//! Bytes in, samples out. Nothing here opens a file, touches a session or knows a frame id —
//! which is what lets the whole decode path be tested from a `Vec<u8>` built in the test.
//!
//! # Why the format is an enum and not a guess at the call site
//!
//! SDD §5.7 and the M1-T09 task both said the same thing: M1 decodes the simulator's FITS and
//! M2 adds the R10's CR3 through `rawler`. [`SourceFormat`] exists so that arrival is a new
//! variant and a new `match` arm rather than a second entry point that callers have to choose
//! between — the difference between adding a format and rewriting the pipeline.
//!
//! M2-T05 collected on that: [`SourceFormat::Cr3`] and [`SourceFormat::Jpeg`] are two more arms
//! here and two more modules ([`crate::cr3`], [`crate::jpeg`]), and **nothing else in the crate
//! changed** — not [`crate::preview`], not [`crate::stretch`], not one caller. Both new arms
//! produce the same [`DecodedFrame`] the FITS arm does, which is what made that possible.
//!
//! # The row order that looks like it works
//!
//! **FITS numbers its rows from the bottom.** The first row in the data block is the *bottom*
//! row of the image, so a decoder that copies the block straight into a top-down raster renders
//! the sky mirrored in declination. It looks entirely plausible — stars are stars either way —
//! and is caught only by comparing against the frame that went in, which
//! [`tests::the_decoder_undoes_the_writers_bottom_up_row_order`] does against the simulator's own
//! writer conventions. [`decode_fits`] therefore reads the block back to front.

use std::fmt;

use astroctl_core::error::ErrorCode;

/// How many bytes a FITS header or data block occupies. The standard's unit, not a tuning knob.
const BLOCK: usize = 2880;
/// A header card is 80 characters, fixed.
const CARD: usize = 80;

/// The source formats the decode path understands.
///
/// M1-T09 wrote this enum with one variant and the note that M2 would add CR3; M2-T05 added that
/// arm and the JPEG one beside it. Every `match` in this crate is still written so that a fourth
/// format is a compile error at the arms that must change rather than a silent fall-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// The simulator's 16-bit FITS (M1-T06), and the format the field node writes today.
    Fits,
    /// Canon's raw, as the R10 writes it (M2-T05). Decoded by [`crate::cr3`].
    Cr3,
    /// A camera JPEG — the `RAW+JPEG` sibling, or a `JPEG`-format capture. See [`crate::jpeg`].
    Jpeg,
}

impl SourceFormat {
    /// Identify a frame from its leading bytes, or `None` if nothing here recognises it.
    ///
    /// Content sniffing rather than the file extension: the frame store names files from the
    /// *camera's* extension (`crate::store::begin_frame`), so an extension is a claim made by a
    /// driver about a file it produced, and the decoder should not have to trust it. The R10 makes
    /// the point concretely — the driver's download path falls back to naming an unrecognised file
    /// `.raw`, so a CR3 can reach the store under an extension no decoder claims.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        // Every conforming FITS file begins with this exact card — the standard fixes the
        // keyword, the spacing and the position of the `T`.
        if bytes.starts_with(b"SIMPLE  =                    T") {
            return Some(Self::Fits);
        }
        // CR3 is ISO base media (the MP4 container family): a `ftyp` box whose major brand is
        // `crx `. Checked at 4..12 because the first four bytes are the box length, which varies.
        // This is the same test the driver's hardware run asserts a captured frame against.
        if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"crx " {
            return Some(Self::Cr3);
        }
        // SOI, then any marker. Two bytes is a weak signature, which is why it is tested last:
        // anything that looked like FITS or CR3 has already claimed the frame.
        if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
            return Some(Self::Jpeg);
        }
        None
    }
}

impl fmt::Display for SourceFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fits => f.write_str("FITS"),
            Self::Cr3 => f.write_str("CR3"),
            Self::Jpeg => f.write_str("JPEG"),
        }
    }
}

/// A decoded frame in linear sensor units, rows **top-down**.
///
/// One sample per pixel: the sensors in scope are monochrome (the simulator writes no
/// `BAYERPAT`, M1-T06) and the Python worker's preview path is likewise grayscale, so a colour
/// plane in M1 would be three copies of the same data. SDD §5.7's "quarter-res RGB" is the
/// shape M2's CR3 arrives in; nothing above this type assumes one channel except the encoder,
/// which names [`jpeg_encoder::ColorType::Luma`] explicitly for that reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    width: u32,
    height: u32,
    samples: Vec<u16>,
}

impl DecodedFrame {
    /// Build a frame, checking that the samples match the dimensions.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Malformed`] if `samples.len() != width * height`.
    pub fn new(width: u32, height: u32, samples: Vec<u16>) -> Result<Self, DecodeError> {
        let expected = width as usize * height as usize;
        if samples.len() != expected {
            return Err(DecodeError::Malformed(format!(
                "a {width}×{height} frame needs {expected} samples, got {}",
                samples.len()
            )));
        }
        Ok(Self {
            width,
            height,
            samples,
        })
    }

    /// Columns.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Rows.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The samples, row-major and top-down.
    #[must_use]
    pub fn samples(&self) -> &[u16] {
        &self.samples
    }
}

/// Why a frame could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Nothing in [`SourceFormat::sniff`] recognised the leading bytes.
    UnknownFormat,
    /// The header is readable but describes something this build cannot render.
    Unsupported(String),
    /// The bytes claim to be a format they are not, or are truncated.
    Malformed(String),
}

impl DecodeError {
    /// The §4.2 error code this maps to.
    ///
    /// All three are `Internal` rather than `Validation`: nothing the *operator* did produced
    /// them. A frame that will not decode came off the node's own camera, and telling them their
    /// input was invalid would point at the wrong thing entirely.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        ErrorCode::Internal
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFormat => f.write_str("the frame is not in a format this node decodes"),
            Self::Unsupported(what) => write!(f, "{what}"),
            Self::Malformed(what) => write!(f, "the frame is malformed: {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode a frame of the given format.
///
/// # Errors
///
/// See [`DecodeError`].
pub fn decode(bytes: &[u8], format: SourceFormat) -> Result<DecodedFrame, DecodeError> {
    match format {
        SourceFormat::Fits => decode_fits(bytes),
        SourceFormat::Cr3 => crate::cr3::decode_cr3(bytes),
        SourceFormat::Jpeg => crate::jpeg::decode_jpeg(bytes),
    }
}

/// Decode by sniffing the format first.
///
/// # Errors
///
/// [`DecodeError::UnknownFormat`] if nothing recognises the leading bytes; otherwise as
/// [`decode`].
pub fn decode_any(bytes: &[u8]) -> Result<DecodedFrame, DecodeError> {
    let format = SourceFormat::sniff(bytes).ok_or(DecodeError::UnknownFormat)?;
    decode(bytes, format)
}

// ---------------------------------------------------------------------------------------------
// FITS
// ---------------------------------------------------------------------------------------------

/// The primary-HDU keywords this decoder reads.
#[derive(Debug)]
struct FitsHeader {
    bitpix: i64,
    width: u32,
    height: u32,
    bscale: f64,
    bzero: f64,
    /// Byte offset of the data block — always a whole number of 2880-byte blocks in.
    data_offset: usize,
}

/// Read a 16-bit FITS primary HDU into linear samples.
///
/// Handles the integer `BITPIX` values. Floating-point frames are refused rather than quantised:
/// nothing in this deployment writes one, and silently rounding a float frame to 16 bits would be
/// a lossy conversion nobody asked for, discovered later as banding in a preview.
fn decode_fits(bytes: &[u8]) -> Result<DecodedFrame, DecodeError> {
    let header = parse_fits_header(bytes)?;

    let bytes_per_sample = match header.bitpix {
        8 => 1_usize,
        16 => 2,
        32 => 4,
        -32 | -64 => {
            return Err(DecodeError::Unsupported(format!(
                "BITPIX {} is a floating-point frame; this node decodes integer FITS (SDD §5.7 \
                 — the RAW path arrives with M2)",
                header.bitpix
            )))
        }
        other => {
            return Err(DecodeError::Unsupported(format!(
                "BITPIX {other} is not a FITS sample type"
            )))
        }
    };

    let count = header.width as usize * header.height as usize;
    let needed = count
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| DecodeError::Malformed("the frame dimensions overflow".to_owned()))?;
    let data = bytes
        .get(header.data_offset..)
        .filter(|rest| rest.len() >= needed)
        .ok_or_else(|| {
            DecodeError::Malformed(format!(
                "the data block is short: {}×{} needs {needed} bytes, {} follow the header",
                header.width,
                header.height,
                bytes.len().saturating_sub(header.data_offset)
            ))
        })?;

    // Rows are written bottom-up (see the module docs), so the destination row for source row
    // `r` is `height - 1 - r`. Filling a pre-sized buffer by index rather than pushing and
    // reversing keeps one 48 MB allocation instead of two on a node budgeted at 512 MB (PRF-05).
    let mut samples = vec![0_u16; count];
    let width = header.width as usize;
    for source_row in 0..header.height as usize {
        let destination_row = header.height as usize - 1 - source_row;
        let source = source_row * width * bytes_per_sample;
        let destination = destination_row * width;
        for column in 0..width {
            let at = source + column * bytes_per_sample;
            let raw = match header.bitpix {
                8 => f64::from(data[at]),
                16 => f64::from(i16::from_be_bytes([data[at], data[at + 1]])),
                // The only remaining arm; the `match` above rejected everything else.
                _ => f64::from(i32::from_be_bytes([
                    data[at],
                    data[at + 1],
                    data[at + 2],
                    data[at + 3],
                ])),
            };
            // `BZERO`/`BSCALE` are the standard's way of carrying unsigned samples in a signed
            // type — `BITPIX = 16` with `BZERO = 32768` is what every DSLR-to-FITS converter
            // writes, and the simulator with it. Ignoring them halves the sky and inverts the
            // highlights, which is the same failure the Python worker's `_read_fits` guards
            // against and for the same reason.
            let value = raw.mul_add(header.bscale, header.bzero);
            samples[destination + column] = clamp_to_u16(value);
        }
    }

    DecodedFrame::new(header.width, header.height, samples)
}

/// Round and clamp a scaled sample into the 16-bit range the pipeline works in.
///
/// Clamping rather than wrapping: a value outside the range is a saturated pixel or a header
/// whose `BSCALE` does not describe its data, and in both cases the nearest representable
/// sample is a better preview than a wrapped one that renders a star as a black dot.
fn clamp_to_u16(value: f64) -> u16 {
    if value.is_nan() {
        return 0;
    }
    // `as` saturates on cast in Rust, but the explicit clamp documents the intent and keeps the
    // rounding visible.
    value.round().clamp(0.0, f64::from(u16::MAX)) as u16
}

/// Parse the primary header, stopping at `END`.
fn parse_fits_header(bytes: &[u8]) -> Result<FitsHeader, DecodeError> {
    if !bytes.starts_with(b"SIMPLE  =") {
        return Err(DecodeError::Malformed(
            "the file does not begin with a SIMPLE card".to_owned(),
        ));
    }

    let mut bitpix = None;
    let mut naxis = None;
    let mut width = None;
    let mut height = None;
    let mut bscale = 1.0_f64;
    let mut bzero = 0.0_f64;

    let mut offset = 0_usize;
    loop {
        let card = bytes.get(offset..offset + CARD).ok_or_else(|| {
            DecodeError::Malformed("the header ends without an END card".to_owned())
        })?;
        let keyword = String::from_utf8_lossy(&card[..8]);
        let keyword = keyword.trim_end();

        if keyword == "END" {
            // The data block starts at the next 2880-byte boundary, never immediately after the
            // END card: the standard pads the final header block with spaces.
            let consumed = offset + CARD;
            let data_offset = consumed.div_ceil(BLOCK) * BLOCK;
            let (Some(bitpix), Some(width), Some(height)) = (bitpix, width, height) else {
                return Err(DecodeError::Malformed(
                    "the header is missing BITPIX, NAXIS1 or NAXIS2".to_owned(),
                ));
            };
            if naxis != Some(2) {
                return Err(DecodeError::Unsupported(format!(
                    "NAXIS {} — this node renders two-dimensional images",
                    naxis.unwrap_or(0)
                )));
            }
            return Ok(FitsHeader {
                bitpix,
                width,
                height,
                bscale,
                bzero,
                data_offset,
            });
        }

        if let Some(value) = card_value(card) {
            match keyword {
                "BITPIX" => bitpix = value.parse::<i64>().ok(),
                "NAXIS" => naxis = value.parse::<i64>().ok(),
                "NAXIS1" => width = value.parse::<f64>().ok().map(|v| v as u32),
                "NAXIS2" => height = value.parse::<f64>().ok().map(|v| v as u32),
                "BSCALE" => bscale = value.parse().unwrap_or(1.0),
                "BZERO" => bzero = value.parse().unwrap_or(0.0),
                _ => {}
            }
        }

        offset += CARD;
    }
}

/// The value field of a fixed-format card: everything after `=`, before any `/` comment.
///
/// Returns `None` for a card with no `=` — `COMMENT`, `HISTORY` and blank padding cards, none of
/// which this decoder reads.
fn card_value(card: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(card).ok()?;
    let (_, rest) = text.split_once('=')?;
    // A quoted string value may itself contain a slash (`DATE-OBS`, a path), so the comment
    // separator is only honoured outside quotes. None of the keywords this decoder parses is a
    // string, but a header where `INSTRUME = 'Canon/EOS'` truncated the *scan* would be a bug
    // that only appears with one camera's name in it.
    let mut value = String::new();
    let mut quoted = false;
    for ch in rest.chars() {
        match ch {
            '\'' => quoted = !quoted,
            '/' if !quoted => break,
            _ => value.push(ch),
        }
    }
    Some(value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a FITS file the way `astroctl-drivers`' simulator writes one: `BITPIX = 16`,
    /// `BZERO = 32768`, big-endian, **rows bottom-up**.
    ///
    /// Deliberately a second implementation of the writer rather than a call into it —
    /// `astroctl-pipeline` may not depend on `astroctl-drivers` (ADD §5.6 rule 1), and a test
    /// that shared the writer's code could not catch the two of them agreeing on a mistake.
    fn simulator_fits(width: u32, height: u32, pixels: &[u16]) -> Vec<u8> {
        assert_eq!(pixels.len(), width as usize * height as usize);
        let mut header = String::new();
        let mut card = |text: &str| {
            let mut padded = text.to_owned();
            padded.push_str(&" ".repeat(CARD - text.len()));
            header.push_str(&padded);
        };
        card("SIMPLE  =                    T / conforms to FITS standard");
        card("BITPIX  =                   16 / 16-bit signed samples, unsigned via BZERO");
        card("NAXIS   =                    2 / two-dimensional image");
        card(&format!("NAXIS1  = {width:20} / columns"));
        card(&format!("NAXIS2  = {height:20} / rows"));
        card("BZERO   =        32768.00000000 / offset to unsigned 16-bit");
        card("BSCALE  =            1.00000000 / samples are ADU");
        card("INSTRUME= 'Canon/EOS R10'       / a slash inside quotes");
        card("END");

        let mut bytes = header.into_bytes();
        let padding = (BLOCK - bytes.len() % BLOCK) % BLOCK;
        bytes.extend(std::iter::repeat_n(b' ', padding));

        for row in (0..height as usize).rev() {
            for column in 0..width as usize {
                let sample = pixels[row * width as usize + column];
                let stored = (i32::from(sample) - 32_768) as i16;
                bytes.extend_from_slice(&stored.to_be_bytes());
            }
        }
        let data_bytes = pixels.len() * 2;
        bytes.extend(std::iter::repeat_n(
            0_u8,
            (BLOCK - data_bytes % BLOCK) % BLOCK,
        ));
        bytes
    }

    #[test]
    fn the_decoder_undoes_the_writers_bottom_up_row_order() {
        // The defect this whole test exists for: a decoder that copies the data block straight
        // through renders the sky mirrored in declination and looks entirely plausible doing it.
        // The frame is asymmetric top-to-bottom precisely so a flip cannot pass.
        let pixels: Vec<u16> = vec![
            10, 20, // top row
            30, 40, // middle
            50, 60, // bottom row
        ];
        let encoded = simulator_fits(2, 3, &pixels);
        let frame = decode_any(&encoded).expect("decodes");

        assert_eq!((frame.width(), frame.height()), (2, 3));
        assert_eq!(
            frame.samples(),
            pixels.as_slice(),
            "the decoded raster must be the one that went in, not its vertical mirror"
        );
    }

    #[test]
    fn bzero_carries_the_unsigned_convention_across_the_signed_range() {
        // The two values either side of the BZERO offset, plus both extremes. Getting this wrong
        // halves the sky and inverts the highlights rather than failing outright.
        let pixels: Vec<u16> = vec![0, 32_767, 32_768, 65_535];
        let encoded = simulator_fits(4, 1, &pixels);
        let frame = decode_any(&encoded).expect("decodes");
        assert_eq!(frame.samples(), pixels.as_slice());
    }

    #[test]
    fn a_quoted_slash_does_not_truncate_the_card_scan() {
        // `INSTRUME = 'Canon/EOS R10'` — a comment separator inside a quoted string. The header
        // in the fixture carries one, so a naive `split('/')` would mis-parse the card after it.
        let encoded = simulator_fits(2, 2, &[1, 2, 3, 4]);
        assert!(decode_any(&encoded).is_ok());
        assert_eq!(
            card_value(b"INSTRUME= 'Canon/EOS R10'       / a slash inside quotes          ")
                .as_deref(),
            Some("Canon/EOS R10")
        );
    }

    #[test]
    fn the_data_block_starts_on_a_block_boundary_not_after_the_end_card() {
        // The END card is followed by space padding to 2880. A decoder that read from
        // `end + 80` would be off by up to 2879 bytes and would render noise.
        let pixels: Vec<u16> = (0..16).collect();
        let encoded = simulator_fits(4, 4, &pixels);
        assert_eq!(encoded.len() % BLOCK, 0);
        let frame = decode_any(&encoded).expect("decodes");
        assert_eq!(frame.samples(), pixels.as_slice());
    }

    #[test]
    fn a_truncated_data_block_is_an_error_rather_than_a_short_frame() {
        // A frame cut off mid-download must not decode to a plausible smaller image.
        let encoded = simulator_fits(8, 8, &[100; 64]);
        let truncated = &encoded[..encoded.len() - BLOCK];
        assert!(matches!(
            decode_any(truncated),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn sniffing_recognises_every_format_and_refuses_everything_else() {
        let encoded = simulator_fits(2, 2, &[0; 4]);
        assert_eq!(SourceFormat::sniff(&encoded), Some(SourceFormat::Fits));
        // M2-T05: a CR3 is an ISO base-media file whose major brand is `crx `. Until this task it
        // sniffed to `None` and the node logged an undecodable frame once per capture.
        assert_eq!(
            SourceFormat::sniff(b"\0\0\0\x18ftypcrx "),
            Some(SourceFormat::Cr3)
        );
        // ISO base media, but somebody else's — an MP4 is not a frame.
        assert_eq!(SourceFormat::sniff(b"\0\0\0\x18ftypisom"), None);
        assert_eq!(
            SourceFormat::sniff(b"\xFF\xD8\xFF\xE0\x00\x10JFIF"),
            Some(SourceFormat::Jpeg)
        );
        assert_eq!(SourceFormat::sniff(b""), None);
        // A prefix of the CR3 signature must not be read past its end.
        assert_eq!(SourceFormat::sniff(b"\0\0\0\x18ftyp"), None);
        assert!(matches!(
            decode_any(b"not a frame at all"),
            Err(DecodeError::UnknownFormat)
        ));
    }

    #[test]
    fn a_floating_point_frame_is_refused_rather_than_quantised() {
        // Silently rounding a float frame to 16 bits is a lossy conversion nobody asked for,
        // and it would be discovered as banding in a preview rather than as an error.
        let mut encoded = simulator_fits(2, 2, &[0; 4]);
        let at = encoded
            .windows(6)
            .position(|w| w == b"BITPIX")
            .expect("the fixture has a BITPIX card");
        encoded[at..at + CARD].copy_from_slice(
            format!("{:<80}", "BITPIX  =                  -32 / IEEE single").as_bytes(),
        );
        assert!(matches!(
            decode_any(&encoded),
            Err(DecodeError::Unsupported(_))
        ));
    }

    /// The independent read-back: CDS's parser must agree with ours about the fixture, so a
    /// shared misreading of the standard cannot pass both.
    #[test]
    fn cds_fitsrs_agrees_with_this_decoder_about_the_fixture() {
        use std::io::Cursor;

        let pixels: Vec<u16> = (0..64).map(|i| i * 1000).collect();
        let encoded = simulator_fits(8, 8, &pixels);
        let ours = decode_any(&encoded).expect("decodes");

        let mut reader = fitsrs::Fits::from_reader(Cursor::new(encoded.clone()));
        let hdu = reader
            .next()
            .expect("a primary HDU")
            .expect("the HDU parses");
        let fitsrs::HDU::Primary(primary) = hdu else {
            panic!("the primary HDU must be an image");
        };
        let header = primary.get_header();
        assert_eq!(
            header.get_parsed::<i64>("NAXIS1").expect("NAXIS1"),
            i64::from(ours.width())
        );
        assert_eq!(
            header.get_parsed::<i64>("NAXIS2").expect("NAXIS2"),
            i64::from(ours.height())
        );
        assert_eq!(header.get_parsed::<f64>("BZERO").expect("BZERO"), 32_768.0);
    }
}
