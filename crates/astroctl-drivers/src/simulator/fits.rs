//! A 16-bit FITS writer — the simulator's equivalent of the reference body's CR3.
//!
//! # Why this is written here rather than taken from a crate
//!
//! PRD §7 pins `fitsio` 0.21.10 for FITS I/O, and `fitsio` links **cfitsio**: the dependency
//! survey lists `libcfitsio-dev` as an M1 system package for exactly this reason. Two things
//! argue against dragging that in to *write* a file:
//!
//! * A single-HDU, uncompressed, 16-bit image is about sixty lines of the FITS standard —
//!   fixed-format 80-character cards in 2880-byte blocks, big-endian samples, `BZERO` to carry
//!   unsigned values. It is one of the few file formats whose specification is shorter than the
//!   binding to a library that implements it.
//! * M1-T13's Python worker reads these frames with numpy and no astropy ("keep
//!   `requirements.txt` minimal"), so the output has to stay inside the subset a hand-written
//!   reader can parse anyway. Owning the writer is what keeps that subset an intentional
//!   decision instead of whatever the library defaulted to.
//!
//! The read side is where an independent implementation earns its keep, and that is where one is
//! used: the tests parse these files with `fitsrs` — a pure-Rust FITS parser from CDS, the
//! observatory-software group behind Aladin — so "it round-trips" means a reader that has never
//! seen this file's writer agrees with it. A test that read the bytes back with this module
//! would only prove the module is self-consistent.
//!
//! # The subset written
//!
//! One primary HDU. `BITPIX = 16` with `BZERO = 32768`, which is the standard's own way of
//! storing unsigned 16-bit data and what every astronomical tool expects from a 16-bit camera.
//! No extensions, no compression, no checksum, and **no `BAYERPAT`**: the simulator's sensor is
//! monochrome (see [`sky`](super::sky)), and announcing a mosaic that is not there would send
//! the Phase 2 pipeline through a debayer that would smear every star into a colour fringe.

use std::io::{self, Write};

use astroctl_core::types::RaDec;
use chrono::{DateTime, SecondsFormat, Utc};

/// Every FITS file is a whole number of these. Header and data are padded to it independently.
const BLOCK: usize = 2880;

/// One header card is exactly 80 characters, 36 to a block.
const CARD: usize = 80;

/// The offset that makes `BITPIX = 16` hold unsigned samples.
const BZERO: i32 = 32_768;

/// What the header says about the exposure, beyond its dimensions.
///
/// These are the keywords the rest of the system will read: the preview decoder (M1-T09) needs
/// the dimensions and nothing else, but the stacker (Phase 2b) matches frames by exposure, gain
/// and pointing, and a frame that did not record them is a frame that cannot be calibrated
/// against another one. Written now, while the format is being invented, rather than added
/// after the first session that needed them.
#[derive(Debug, Clone)]
pub struct FrameMetadata {
    /// When the shutter opened.
    pub started_at: DateTime<Utc>,
    /// How long it stayed open.
    pub exposure_seconds: f64,
    /// Where the telescope was aimed.
    pub pointing: RaDec,
    /// The ISO token in force, e.g. `"1600"`.
    pub iso: String,
    /// Electrons to ADU, as the frame was digitised.
    pub gain_adu_per_electron: f64,
    /// Pixel pitch in micrometres.
    pub pixel_size_um: f64,
    /// Focal length in millimetres.
    pub focal_length_mm: f64,
    /// Sensor temperature in degrees Celsius, when the body reports one.
    pub sensor_temperature_celsius: Option<f64>,
    /// The catalogue seed this sky came from — the one number that makes a reported frame
    /// reproducible on another machine.
    pub sky_seed: u64,
}

/// Writes a complete FITS file: header, samples, padding.
///
/// `pixels` is row-major with `width * height` samples. FITS numbers its rows from the *bottom*,
/// which is why the rows go out in reverse: a viewer that puts NAXIS2 at the top would otherwise
/// show every simulated frame mirrored, and the first person to notice would be whoever compares
/// a simulated plate solve against a real one.
///
/// # Errors
/// Whatever `writer` returns. Nothing here can fail on its own.
///
/// # Panics
/// If `pixels` is not exactly `width * height` samples — a caller bug, and the alternative is a
/// truncated file that only fails when something tries to read it.
pub fn write_image<W: Write>(
    writer: &mut W,
    width: u32,
    height: u32,
    pixels: &[u16],
    meta: &FrameMetadata,
) -> io::Result<()> {
    assert_eq!(
        pixels.len(),
        width as usize * height as usize,
        "the pixel buffer does not match the stated dimensions"
    );

    writer.write_all(&header(width, height, meta))?;

    // A row at a time, converted in place: the alternative is a second buffer the size of the
    // frame, which for a full-size exposure is 48 MB on a node budgeted at 512 MB (PRF-05).
    let mut row_bytes = vec![0_u8; width as usize * 2];
    for row in (0..height as usize).rev() {
        let start = row * width as usize;
        for (sample, out) in pixels[start..start + width as usize]
            .iter()
            .zip(row_bytes.chunks_exact_mut(2))
        {
            // `value - 32768` is what BZERO undoes on the way back in. Done in `i32` because the
            // subtraction leaves the `u16` range at both ends.
            let stored = (i32::from(*sample) - BZERO) as i16;
            out.copy_from_slice(&stored.to_be_bytes());
        }
        writer.write_all(&row_bytes)?;
    }

    let data_bytes = pixels.len() * 2;
    let padding = (BLOCK - data_bytes % BLOCK) % BLOCK;
    if padding > 0 {
        // The standard says the final block is padded with zeros for image data (it is *spaces*
        // for headers, which is the kind of asymmetry that produces a file every tool but one
        // can read).
        writer.write_all(&vec![0_u8; padding])?;
    }
    Ok(())
}

/// The complete header, padded to a whole number of blocks.
fn header(width: u32, height: u32, meta: &FrameMetadata) -> Vec<u8> {
    let mut cards = Cards::default();
    // The six mandatory cards, in the order the standard mandates — a reader is allowed to
    // reject the file if SIMPLE is not first.
    cards.logical("SIMPLE", true, "conforms to FITS standard");
    cards.integer("BITPIX", 16, "16-bit signed samples, unsigned via BZERO");
    cards.integer("NAXIS", 2, "two-dimensional image");
    cards.integer("NAXIS1", i64::from(width), "columns");
    cards.integer("NAXIS2", i64::from(height), "rows");
    cards.float("BZERO", 32_768.0, "offset to unsigned 16-bit");
    cards.float("BSCALE", 1.0, "samples are ADU");

    cards.string("IMAGETYP", "LIGHT", "light frame");
    cards.float("EXPTIME", meta.exposure_seconds, "exposure in seconds");
    cards.string(
        "DATE-OBS",
        // Milliseconds and no trailing Z: FITS times are UTC by definition and the standard's
        // format has no zone suffix. SDD §2's "UTC RFC 3339 with milliseconds" is the same
        // instant written the way the rest of the system writes it.
        meta.started_at
            .to_rfc3339_opts(SecondsFormat::Millis, true)
            .trim_end_matches('Z'),
        "UTC at shutter open",
    );
    cards.string("INSTRUME", "AstroCtl Simulator Camera", "camera");
    cards.string("TELESCOP", "AstroCtl Simulator", "telescope");
    cards.string("SWCREATE", "astroctl-drivers simulator", "writer");

    // Pointing twice: sexagesimal for the tools that expect a camera-control program's header,
    // degrees for anything doing arithmetic. Writing only one of them means half the readers
    // parse strings and the other half find nothing.
    //
    // Space-separated, and **not** through `RaHours`/`DecDegrees`'s `Display`. Those render for
    // an operator — `05:34:59.9` and `-05°23'28"` — and the degree sign is not ASCII. A FITS
    // header is ASCII by definition (the standard allows 0x20–0x7E and nothing else), and
    // astropy replaces anything else with `?` and warns. This was found by pointing astropy at
    // a real capture, which is exactly what a third reader is for; `sexagesimal_*` below are
    // what the file gets instead.
    cards.string(
        "OBJCTRA",
        &sexagesimal_hours(meta.pointing.ra.hours()),
        "RA at frame centre",
    );
    cards.string(
        "OBJCTDEC",
        &sexagesimal_degrees(meta.pointing.dec.degrees()),
        "Dec at frame centre",
    );
    cards.float("RA", meta.pointing.ra.hours() * 15.0, "degrees J2000");
    cards.float("DEC", meta.pointing.dec.degrees(), "degrees J2000");

    cards.string("ISOSPEED", &meta.iso, "ISO token as the body spells it");
    cards.float("GAIN", meta.gain_adu_per_electron, "ADU per electron");
    cards.float("XPIXSZ", meta.pixel_size_um, "pixel pitch, micrometres");
    cards.float("YPIXSZ", meta.pixel_size_um, "pixel pitch, micrometres");
    cards.float("FOCALLEN", meta.focal_length_mm, "focal length, mm");
    if let Some(celsius) = meta.sensor_temperature_celsius {
        cards.float("CCD-TEMP", celsius, "sensor temperature, C");
    }
    // Not a standard keyword, and deliberately present: this is the number that turns "the
    // simulator produced a strange frame" into a frame anyone else can regenerate.
    cards.integer("SIMSEED", meta.sky_seed as i64, "synthetic sky seed");
    cards.raw("END");

    let mut bytes = cards.into_bytes();
    let padding = (BLOCK - bytes.len() % BLOCK) % BLOCK;
    // Headers pad with spaces, data pads with zeros. The standard is explicit about both and
    // some readers check.
    bytes.extend(std::iter::repeat_n(b' ', padding));
    bytes
}

/// `HH MM SS.s` — the `OBJCTRA` convention, ASCII only.
fn sexagesimal_hours(hours: f64) -> String {
    let tenths = (hours.rem_euclid(24.0) * 36_000.0).round() as i64 % (24 * 36_000);
    format!(
        "{:02} {:02} {:02}.{}",
        tenths / 36_000,
        (tenths / 600) % 60,
        (tenths / 10) % 60,
        tenths % 10
    )
}

/// `±DD MM SS` — the `OBJCTDEC` convention, ASCII only.
fn sexagesimal_degrees(degrees: f64) -> String {
    let sign = if degrees < 0.0 { '-' } else { '+' };
    let arcsec = (degrees.abs() * 3600.0).round() as i64;
    format!(
        "{}{:02} {:02} {:02}",
        sign,
        arcsec / 3600,
        (arcsec / 60) % 60,
        arcsec % 60
    )
}

/// An accumulating list of fixed-format header cards.
#[derive(Debug, Default)]
struct Cards {
    bytes: Vec<u8>,
}

impl Cards {
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// `KEYWORD = value / comment`, padded to 80 characters.
    ///
    /// Fixed format: the value ends at column 30 for everything but strings, which start at
    /// column 11. Free-format FITS exists and is legal; fixed format is what the older readers
    /// in this corner of the world actually accept, and there is no cost to writing it.
    fn push(&mut self, keyword: &str, value: &str, comment: &str) {
        assert!(keyword.len() <= 8, "`{keyword}` is not a FITS keyword");
        let mut card = format!("{keyword:<8}= {value:>20}");
        if !comment.is_empty() {
            card.push_str(" / ");
            card.push_str(comment);
        }
        self.write_card(&card);
    }

    fn raw(&mut self, text: &str) {
        self.write_card(text);
    }

    /// Writes one card: sanitised to printable ASCII, truncated to 80, padded with spaces.
    ///
    /// The sanitising is not defensive decoration. A FITS header is defined over ASCII 0x20–0x7E
    /// and nothing else, so a stray `°` or an accented name in a value produces a file astropy
    /// reads with a warning and some older readers reject outright — and, worse, a multi-byte
    /// character makes the card 80 *bytes* but 79 *characters*, which silently shifts the whole
    /// 80-column grid for every card after it. Substituting `?` keeps the grid exact and makes
    /// the damage visible in the one place someone would look.
    ///
    /// Truncated rather than wrapped: a comment long enough to overflow is a comment, and a card
    /// that ran into the next one would corrupt the *following* keyword. Truncation is by
    /// character after the substitution, so it can never split a multi-byte sequence — the
    /// byte-wise `String::truncate` this replaced would have panicked on one.
    fn write_card(&mut self, text: &str) {
        let card: Vec<u8> = text
            .chars()
            .map(|c| {
                if c.is_ascii_graphic() || c == ' ' {
                    c as u8
                } else {
                    b'?'
                }
            })
            .take(CARD)
            .collect();
        let padding = CARD - card.len();
        self.bytes.extend(card);
        self.bytes.extend(std::iter::repeat_n(b' ', padding));
    }

    fn logical(&mut self, keyword: &str, value: bool, comment: &str) {
        self.push(keyword, if value { "T" } else { "F" }, comment);
    }

    fn integer(&mut self, keyword: &str, value: i64, comment: &str) {
        self.push(keyword, &value.to_string(), comment);
    }

    /// A floating-point card.
    ///
    /// Always written with a decimal point, because `EXPTIME = 30` is an integer card and a
    /// reader that types its values will hand a caller an `i64` where it expected a float —
    /// which is a crash in a Python worker rather than a rounding difference.
    fn float(&mut self, keyword: &str, value: f64, comment: &str) {
        let rendered = if value == value.trunc() && value.abs() < 1e15 {
            format!("{value:.1}")
        } else {
            format!("{value:.6}")
        };
        self.push(keyword, &rendered, comment);
    }

    /// A string card: single-quoted, at least eight characters wide, doubled internal quotes.
    fn string(&mut self, keyword: &str, value: &str, comment: &str) {
        let escaped = value.replace('\'', "''");
        let padded = format!("'{escaped:<8}'");
        // Strings are the one value type the standard left-justifies, so they do not go through
        // `push`'s right-justifying format.
        let mut card = format!("{keyword:<8}= {padded}");
        if !comment.is_empty() {
            card.push_str(" / ");
            card.push_str(comment);
        }
        self.write_card(&card);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn metadata() -> FrameMetadata {
        FrameMetadata {
            started_at: DateTime::parse_from_rfc3339("2026-07-30T21:15:04.250Z")
                .expect("a valid timestamp")
                .with_timezone(&Utc),
            exposure_seconds: 30.0,
            pointing: RaDec::from_parts(5.5, 22.0).expect("valid"),
            iso: "1600".to_owned(),
            gain_adu_per_electron: 4.0,
            pixel_size_um: 3.72,
            focal_length_mm: 1000.0,
            sensor_temperature_celsius: Some(11.5),
            sky_seed: 0xDEAD_BEEF,
        }
    }

    #[test]
    fn the_file_is_a_whole_number_of_blocks() {
        // A FITS file that is not block-aligned is a FITS file some readers stop at.
        for (width, height) in [(1_u32, 1_u32), (17, 13), (200, 150)] {
            let mut bytes = Vec::new();
            let pixels = vec![1000_u16; width as usize * height as usize];
            write_image(&mut bytes, width, height, &pixels, &metadata()).expect("writes");
            assert_eq!(
                bytes.len() % BLOCK,
                0,
                "{width}x{height} produced {} bytes",
                bytes.len()
            );
        }
    }

    #[test]
    fn every_card_is_eighty_characters_and_the_header_ends() {
        let header = header(200, 150, &metadata());
        assert_eq!(header.len() % BLOCK, 0);
        let text = String::from_utf8(header).expect("headers are ASCII");
        assert!(text.starts_with("SIMPLE  =                    T"));
        for card in text.as_bytes().chunks(CARD) {
            assert_eq!(card.len(), CARD);
        }
        assert!(text.contains("END     "));
        // The keyword that must *not* be there: a monochrome frame claiming a Bayer pattern
        // would be debayered into colour fringes by the Phase 2 pipeline.
        assert!(!text.contains("BAYERPAT"));
    }

    #[test]
    fn a_quote_in_a_value_cannot_break_the_card_grid() {
        let mut cards = Cards::default();
        cards.string("OBJECT", "Barnard's Loop", "");
        let text = String::from_utf8(cards.into_bytes()).expect("ASCII");
        assert_eq!(text.len(), CARD);
        assert!(text.contains("'Barnard''s Loop'"), "{text}");
    }

    #[test]
    fn the_header_is_ascii_even_when_a_value_is_not() {
        // Found by astropy, which reads a non-ASCII header with a warning and a `?` — and by the
        // arithmetic behind it: a two-byte character makes a card 80 bytes but 79 columns, and
        // every card after it is then misaligned. The pointing is the live case, because
        // `DecDegrees` renders `-05°23'28"` for an operator and that is what this header used to
        // carry.
        let header = header(16, 16, &metadata());
        assert!(header.is_ascii(), "the header is not ASCII");
        let text = String::from_utf8(header).expect("ASCII");
        assert!(
            text.contains("'+22 00 00'"),
            "OBJCTDEC is not sexagesimal ASCII: {text}"
        );
        assert!(
            text.contains("'05 30 00.0'"),
            "OBJCTRA is not sexagesimal ASCII"
        );

        // ...and a value that is not ASCII is substituted rather than shifting the grid.
        let mut cards = Cards::default();
        cards.string("OBJECT", "M42 \u{2014} Orion", "");
        let bytes = cards.into_bytes();
        assert_eq!(bytes.len(), CARD);
        assert!(bytes.is_ascii());
    }

    #[test]
    fn the_data_block_is_big_endian_and_offset_by_bzero() {
        // The two ways a 16-bit FITS goes wrong: byte order, and forgetting that BITPIX 16 is
        // *signed*. Both produce a file that opens and shows noise.
        let mut bytes = Vec::new();
        let pixels = vec![0_u16, 32_768, 65_535, 1];
        write_image(&mut bytes, 2, 2, &pixels, &metadata()).expect("writes");
        let data = &bytes[bytes.len() - BLOCK..];
        // Rows are written bottom-up, so the second row (65535, 1) comes first.
        assert_eq!(
            i16::from_be_bytes([data[0], data[1]]) as i32 + BZERO,
            65_535
        );
        assert_eq!(i16::from_be_bytes([data[2], data[3]]) as i32 + BZERO, 1);
        assert_eq!(i16::from_be_bytes([data[4], data[5]]) as i32 + BZERO, 0);
        assert_eq!(
            i16::from_be_bytes([data[6], data[7]]) as i32 + BZERO,
            32_768
        );
    }

    #[test]
    fn a_reader_that_never_saw_this_writer_gets_the_pixels_back() {
        // Acceptance criterion: "FITS opens in a standard tool". `fitsrs` is the pure-Rust
        // stand-in for the `fitsio`/cfitsio pair the acceptance list names — see the module docs
        // for why the system dependency is not taken on for a test.
        use fitsrs::{Fits, Pixels, HDU};

        let width = 37_u32;
        let height = 23_u32;
        let pixels: Vec<u16> = (0..width * height)
            .map(|i| (i * 61 % 65_535) as u16)
            .collect();
        let mut bytes = Vec::new();
        write_image(&mut bytes, width, height, &pixels, &metadata()).expect("writes");

        let mut hdus = Fits::from_reader(Cursor::new(bytes));
        let Some(Ok(HDU::Primary(hdu))) = hdus.next() else {
            panic!("fitsrs did not find a primary HDU");
        };
        let header = hdu.get_header();
        assert_eq!(
            header.get_parsed::<i64>("NAXIS1").expect("NAXIS1"),
            i64::from(width)
        );
        assert_eq!(
            header.get_parsed::<i64>("NAXIS2").expect("NAXIS2"),
            i64::from(height)
        );
        assert_eq!(header.get_parsed::<f64>("EXPTIME").expect("EXPTIME"), 30.0);
        assert_eq!(
            header.get_parsed::<f64>("BZERO").expect("BZERO"),
            f64::from(BZERO)
        );

        let image = hdus.get_data(&hdu);
        let Pixels::I16(samples) = image.pixels() else {
            panic!("expected 16-bit samples");
        };
        // Undo BZERO and the bottom-up row order, and the frame must be the one that went in.
        let read_back: Vec<u16> = samples
            .map(|sample| (i32::from(sample) + BZERO) as u16)
            .collect();
        let expected: Vec<u16> = pixels
            .chunks_exact(width as usize)
            .rev()
            .flatten()
            .copied()
            .collect();
        assert_eq!(read_back, expected);
    }
}
