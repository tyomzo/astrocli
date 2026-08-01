//! Just enough FITS to read the simulator's frames and write float32 ones for the Python
//! cross-check.
//!
//! Deliberately not a FITS library. It handles exactly the two shapes this spike needs — a
//! 2-D `BITPIX=16` image with `BZERO=32768` (what `astroctl-drivers`' simulator writes) and a
//! 2-D `BITPIX=-32` image (what `astropy` will read back) — and refuses everything else loudly
//! rather than guessing.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

/// One FITS block. Everything is a multiple of this.
const BLOCK: usize = 2880;
/// One header card.
const CARD: usize = 80;

/// A 2-D image and the header keywords the spike cares about.
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// Physical sample values, already through `BZERO`/`BSCALE`.
    pub data: Vec<f32>,
    pub header: HashMap<String, String>,
}

impl Image {
    /// A header keyword as an `f64`, if present and numeric.
    pub fn number(&self, key: &str) -> Option<f64> {
        self.header.get(key)?.trim().parse::<f64>().ok()
    }

    /// A header keyword as a `u64`. Parsed via `i128` first because `SIMSEED` is written as a
    /// signed decimal by the simulator's writer but is a `u64` seed — a value above
    /// `i64::MAX` round-trips through the negative range.
    pub fn seed(&self, key: &str) -> Option<u64> {
        let raw = self.header.get(key)?.trim();
        if let Ok(v) = raw.parse::<u64>() {
            return Some(v);
        }
        raw.parse::<i64>().ok().map(|v| v as u64)
    }
}

/// Reads a 2-D FITS image from the primary HDU.
pub fn read(path: &Path) -> Result<Image, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;

    // --- header ---
    let mut header = HashMap::new();
    let mut offset = 0;
    let mut ended = false;
    while offset + CARD <= bytes.len() {
        let card = &bytes[offset..offset + CARD];
        offset += CARD;
        let text = String::from_utf8_lossy(card);
        let keyword = text.get(0..8).unwrap_or("").trim().to_string();
        if keyword == "END" {
            ended = true;
            // The header occupies whole blocks; data starts at the next block boundary.
            offset = offset.div_ceil(BLOCK) * BLOCK;
            break;
        }
        if let Some(rest) = text.get(8..) {
            if let Some(stripped) = rest.strip_prefix('=') {
                // Strip the trailing `/ comment`, but not a slash inside a quoted string.
                let mut value = stripped.trim().to_string();
                if value.starts_with('\'') {
                    if let Some(end) = value[1..].find('\'') {
                        value = value[1..=end].trim().to_string();
                    }
                } else if let Some(slash) = value.find('/') {
                    value = value[..slash].trim().to_string();
                }
                header.insert(keyword, value);
            }
        }
    }
    if !ended {
        return Err(format!("{}: no END card", path.display()));
    }

    let bitpix = header
        .get("BITPIX")
        .and_then(|v| v.trim().parse::<i32>().ok())
        .ok_or_else(|| format!("{}: no BITPIX", path.display()))?;
    let naxis = header
        .get("NAXIS")
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(0);
    if naxis != 2 {
        return Err(format!("{}: NAXIS={naxis}, only 2-D is handled", path.display()));
    }
    let width = header
        .get("NAXIS1")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| format!("{}: no NAXIS1", path.display()))?;
    let height = header
        .get("NAXIS2")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| format!("{}: no NAXIS2", path.display()))?;

    let bzero = header
        .get("BZERO")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let bscale = header
        .get("BSCALE")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(1.0);

    let count = width * height;
    let mut data = vec![0.0_f32; count];

    match bitpix {
        16 => {
            let need = offset + count * 2;
            if bytes.len() < need {
                return Err(format!(
                    "{}: truncated, need {need} bytes, have {}",
                    path.display(),
                    bytes.len()
                ));
            }
            for (i, sample) in data.iter_mut().enumerate() {
                let b = &bytes[offset + i * 2..offset + i * 2 + 2];
                let raw = i16::from_be_bytes([b[0], b[1]]);
                *sample = (bzero + bscale * f64::from(raw)) as f32;
            }
        }
        -32 => {
            let need = offset + count * 4;
            if bytes.len() < need {
                return Err(format!(
                    "{}: truncated, need {need} bytes, have {}",
                    path.display(),
                    bytes.len()
                ));
            }
            for (i, sample) in data.iter_mut().enumerate() {
                let b = &bytes[offset + i * 4..offset + i * 4 + 4];
                let raw = f32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                *sample = (bzero + bscale * f64::from(raw)) as f32;
            }
        }
        other => {
            return Err(format!(
                "{}: BITPIX={other} is not handled by this spike",
                path.display()
            ))
        }
    }

    Ok(Image {
        width,
        height,
        data,
        header,
    })
}

/// Writes a 2-D float32 FITS image — what the Python cross-check reads.
///
/// `extra` cards are written verbatim after the mandatory ones so the truth parameters travel
/// with the file.
pub fn write_f32(
    path: &Path,
    data: &[f32],
    width: usize,
    height: usize,
    extra: &[(&str, String, &str)],
) -> Result<(), String> {
    assert_eq!(data.len(), width * height, "write_f32: size mismatch");
    let file = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut out = BufWriter::new(file);

    let mut cards: Vec<String> = Vec::new();
    cards.push(card_logical("SIMPLE", true, "conforms to FITS standard"));
    cards.push(card_int("BITPIX", -32, "IEEE 754 single precision"));
    cards.push(card_int("NAXIS", 2, "two-dimensional image"));
    cards.push(card_int("NAXIS1", width as i64, "columns"));
    cards.push(card_int("NAXIS2", height as i64, "rows"));
    for (key, value, comment) in extra {
        cards.push(card_raw(key, value, comment));
    }
    cards.push(format!("{:<80}", "END"));

    // Pad the header to a whole number of blocks.
    let mut header = String::new();
    for c in &cards {
        header.push_str(c);
    }
    while header.len() % BLOCK != 0 {
        header.push(' ');
    }
    out.write_all(header.as_bytes())
        .map_err(|e| format!("write header: {e}"))?;

    // Big-endian float32, then zero-pad to a block boundary.
    let mut buf = Vec::with_capacity(data.len() * 4);
    for sample in data {
        buf.extend_from_slice(&sample.to_be_bytes());
    }
    let pad = (BLOCK - buf.len() % BLOCK) % BLOCK;
    buf.extend(std::iter::repeat_n(0_u8, pad));
    out.write_all(&buf).map_err(|e| format!("write data: {e}"))?;
    out.flush().map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

fn card_raw(key: &str, value: &str, comment: &str) -> String {
    let body = format!("{key:<8}= {value:>20} / {comment}");
    format!("{body:<80}")
}

fn card_int(key: &str, value: i64, comment: &str) -> String {
    card_raw(key, &value.to_string(), comment)
}

fn card_logical(key: &str, value: bool, comment: &str) -> String {
    card_raw(key, if value { "T" } else { "F" }, comment)
}
