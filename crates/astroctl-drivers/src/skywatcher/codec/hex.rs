//! Hex payloads, and the three different things "hex payload" means on this wire.
//!
//! # Width is not uniform, and neither is byte order
//!
//! SDD §5.2.2 describes 24-bit values as "ASCII hex with byte-swapped ordering", which is true
//! of most fields and false of three. A single `decode_payload` that assumed six byte-swapped
//! characters would mis-decode all three *silently*, because two of them are still valid hex:
//!
//! | Reply | Width | Order | Opcodes |
//! |-------|-------|-------|---------|
//! | 24-bit register | 6 | **byte-swapped** — low byte first | `a b c d h i j m r s`, and the `H I M` writes |
//! | high-speed ratio | **2** | plain | `g` |
//! | axis status | **3** | not a number at all — see [`super::status`] | `f` |
//! | firmware version | 6 | **plain, big-endian** — see [`FirmwareVersion`] | `e` |
//!
//! So there is no general decode entry point here. Each width has its own function, each
//! command names the one that fits its reply, and a command that named the wrong one would
//! fail its golden vector rather than return a number that looks fine.
//!
//! # The byte swap
//!
//! `0x123456` travels as `"563412"`: the ASCII pairs are the value's bytes, least significant
//! first. The rule is not inferred — it is the one that turns the operator's own capture
//! `00B289` into exactly 9,024,000, the counts-per-revolution figure PRD §4.2 predicts to the
//! digit. That exactness is what makes the second capture, `A7FD00` → 64,935, trustworthy
//! enough to have overturned a documented constant that was wrong by a factor of seven
//! (`spikes/skywatcher-heq5/FINDINGS.md`).
//!
//! # Case
//!
//! Encoding emits upper case, because that is what the mount emits and a golden file that
//! matches the wire byte for byte is worth more than one that needs a normalising step to read.
//! Decoding accepts either, because EQMOD traces — the other source of vectors — are not
//! consistent about it, and a codec that rejected a trace for its case would be rejecting
//! evidence.

use super::error::{EncodeError, ProtocolError};

/// The largest value a Synta 24-bit register holds.
pub const U24_MAX: u32 = 0x00FF_FFFF;

/// Characters in a 24-bit register's ASCII-hex payload.
pub const U24_CHARS: usize = 6;

/// A value that fits the protocol's 24-bit registers.
///
/// The type exists so that encoding is *total*: `encode_u24` cannot fail, because a value that
/// could not be expressed never got this far. Range checking happens once, at the constructor,
/// where the caller still has the context to say what the number was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct U24(u32);

impl U24 {
    /// Zero.
    pub const ZERO: Self = Self(0);
    /// The largest representable value, 16,777,215.
    pub const MAX: Self = Self(U24_MAX);

    /// The mechanical home counter, `0x800000` (SDD §5.2.3).
    ///
    /// Both axes of the operator's HEQ5 read exactly this at power-on, which is what made the
    /// byte-swap rule checkable against a third independent value.
    pub const HOME: Self = Self(0x0080_0000);

    /// Narrow a `u32` to the protocol's field width.
    ///
    /// # Errors
    /// [`EncodeError::NotU24`] if the value needs more than 24 bits.
    pub const fn new(value: u32) -> Result<Self, EncodeError> {
        if value > U24_MAX {
            return Err(EncodeError::NotU24 {
                value,
                max: U24_MAX,
            });
        }
        Ok(Self(value))
    }

    /// The value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Build by discarding the bits the register does not have.
    ///
    /// Internal to the codec and total, so it can appear in a `const` initialiser where
    /// [`Self::new`]'s `Result` cannot. Masking rather than truncating keeps the type's
    /// invariant unbreakable: there is no input for which this produces an invalid `U24`.
    pub(super) const fn from_masked(value: u32) -> Self {
        Self(value & U24_MAX)
    }

    /// Add a signed delta, wrapping within the 24-bit counter.
    ///
    /// **The wrap is `derived`, not measured.** The counter is 24 bits and the mount is a
    /// continuously rotating axis, so modulo arithmetic is the only behaviour that makes
    /// physical sense — but no experiment has driven a counter across `0xFFFFFF`, and the
    /// nearest anything came was a 10° move around home (`spikes/skywatcher-heq5/FINDINGS.md`).
    /// M3-T05 can settle it with a `:E` write near the boundary; until then this is the
    /// assumption a readback expectation near the wrap would be checked against.
    #[must_use]
    pub const fn wrapping_add_signed(self, delta: i32) -> Self {
        // Wrap into the 24-bit field rather than the 32-bit one: `& U24_MAX` after a wrapping
        // add is exactly modulo 2^24, and keeps the invariant this type exists to hold.
        Self(self.0.wrapping_add_signed(delta) & U24_MAX)
    }
}

impl From<U24> for u32 {
    fn from(v: U24) -> Self {
        v.0
    }
}

/// Encode a 24-bit register value, low byte first: `0x123456` → `b"563412"`.
///
/// Total, and allocation-free — which is what lets T-COD-1 walk all 16,777,216 values in a
/// unit test rather than sampling them.
#[must_use]
pub fn encode_u24(value: U24) -> [u8; U24_CHARS] {
    let v = value.get();
    let bytes = [
        (v & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        ((v >> 16) & 0xFF) as u8,
    ];
    let mut out = [0u8; U24_CHARS];
    for (i, byte) in bytes.iter().enumerate() {
        out[i * 2] = hex_digit(byte >> 4);
        out[i * 2 + 1] = hex_digit(byte & 0x0F);
    }
    out
}

/// Decode a 24-bit register payload, low byte first: `"00B289"` → 9,024,000.
///
/// # Errors
/// [`ProtocolError::PayloadWidth`] unless the payload is exactly six characters;
/// [`ProtocolError::NotHex`] naming the first offending character otherwise.
pub fn decode_u24(payload: &str, opcode: char) -> Result<U24, ProtocolError> {
    let bytes = expect_width(payload, U24_CHARS, opcode)?;
    let mut value: u32 = 0;
    for i in 0..3 {
        let hi = nibble_at(payload, bytes, i * 2)?;
        let lo = nibble_at(payload, bytes, i * 2 + 1)?;
        value |= u32::from((hi << 4) | lo) << (i * 8);
    }
    // Three bytes cannot exceed 24 bits, so the invariant holds by construction.
    Ok(U24(value))
}

/// Decode a plain big-endian hex payload of six characters: `"020401"` → `0x020401`.
///
/// Used by exactly one opcode, `e`, and the reason it is not the byte-swapped decoder is worth
/// stating because the difference is invisible in the reply itself — see [`FirmwareVersion`].
///
/// # Errors
/// As [`decode_u24`].
pub fn decode_u24_plain(payload: &str, opcode: char) -> Result<u32, ProtocolError> {
    let bytes = expect_width(payload, U24_CHARS, opcode)?;
    let mut value: u32 = 0;
    for i in 0..U24_CHARS {
        value = (value << 4) | u32::from(nibble_at(payload, bytes, i)?);
    }
    Ok(value)
}

/// Decode a two-character plain hex payload: `"10"` → 16.
///
/// The width matters more than the order here: with one byte there is nothing to swap, but a
/// decoder that assumed six characters would reject `:g`'s reply outright — or worse, if it
/// were lenient about width, read the two characters as the *low* byte of a 24-bit value and
/// return 16 by accident, which is right for this mount and wrong in general.
///
/// # Errors
/// [`ProtocolError::PayloadWidth`] unless the payload is exactly two characters;
/// [`ProtocolError::NotHex`] otherwise.
pub fn decode_u8_plain(payload: &str, opcode: char) -> Result<u8, ProtocolError> {
    let bytes = expect_width(payload, 2, opcode)?;
    let hi = nibble_at(payload, bytes, 0)?;
    let lo = nibble_at(payload, bytes, 1)?;
    Ok((hi << 4) | lo)
}

/// What `:e` reports: a firmware revision and the model of mount underneath it.
///
/// **The decode is `derived`, and it is derived against a cross-check worth recording.** The
/// operator's mount answered `=020401` and `spikes/skywatcher-heq5/FINDINGS.md` deliberately
/// declined to interpret it, leaving the reading to this task. INDI's `indi-eqmod` byte-swaps
/// the payload and then immediately reverses the swap field by field, which nets out to reading
/// the six characters as a plain big-endian number — so `020401` is `0x020401`, *not* the
/// `0x010402` the byte-swap rule would give.
///
/// That distinction is checkable rather than merely asserted. EQMOD takes the low byte as a
/// mount-model code, and `0x01` in its table is the HEQ5. The plain reading therefore names the
/// exact mount the capture came from; the byte-swapped reading gives `0x02`, an EQ5, which the
/// operator does not own. One sample is one sample — but it is a sample that could have
/// falsified the reading and did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FirmwareVersion {
    /// The six payload characters as a big-endian number, e.g. `0x020401`.
    ///
    /// Kept because it is the only part of this struct that is a *measurement*. A field log
    /// carrying the raw value stays useful after the interpretation below is revised.
    pub raw: u32,
    /// Motor-controller firmware major version — the high byte.
    pub major: u8,
    /// Motor-controller firmware minor version — the middle byte.
    pub minor: u8,
    /// EQMOD's mount-model code — the low byte. See [`Self::model`].
    pub model_code: u8,
}

impl FirmwareVersion {
    /// Split a raw `:e` value into its three bytes.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self {
            raw,
            major: ((raw >> 16) & 0xFF) as u8,
            minor: ((raw >> 8) & 0xFF) as u8,
            model_code: (raw & 0xFF) as u8,
        }
    }

    /// The mount model, where the code is one we know.
    ///
    /// `None` is not an error: a mount this table does not list still works, and the driver
    /// only uses the model for logging and for the operator-facing device name.
    #[must_use]
    pub const fn model(self) -> Option<MountModel> {
        MountModel::from_code(self.model_code)
    }
}

/// EQMOD's mount-model codes — **`derived` from the reference implementation's enum**, with one
/// entry ([`MountModel::Heq5`]) corroborated by the mount that produced our capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MountModel {
    /// `0x00`.
    Eq6,
    /// `0x01` — the operator's mount, and the corroboration for the whole table.
    Heq5,
    /// `0x02`.
    Eq5,
    /// `0x03`.
    Eq3,
    /// `0x04`.
    Eq8,
    /// `0x05`.
    AzEq6,
    /// `0x06`.
    AzEq5,
}

impl MountModel {
    /// Look up a model code.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0x00 => Some(Self::Eq6),
            0x01 => Some(Self::Heq5),
            0x02 => Some(Self::Eq5),
            0x03 => Some(Self::Eq3),
            0x04 => Some(Self::Eq8),
            0x05 => Some(Self::AzEq6),
            0x06 => Some(Self::AzEq5),
            _ => None,
        }
    }

    /// The name to show an operator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq6 => "EQ6",
            Self::Heq5 => "HEQ5",
            Self::Eq5 => "EQ5",
            Self::Eq3 => "EQ3",
            Self::Eq8 => "EQ8",
            Self::AzEq6 => "AZ-EQ6",
            Self::AzEq5 => "AZ-EQ5",
        }
    }
}

// -----------------------------------------------------------------------------------------
// internals
// -----------------------------------------------------------------------------------------

/// Check a payload's width and hand back its bytes.
///
/// Shared by every decoder above so that the width error reads the same however it was reached,
/// and so that adding a fourth width is one call site rather than a fourth hand-rolled check.
fn expect_width(payload: &str, expected: usize, opcode: char) -> Result<&[u8], ProtocolError> {
    let bytes = payload.as_bytes();
    if bytes.len() == expected {
        Ok(bytes)
    } else {
        Err(ProtocolError::PayloadWidth {
            payload: payload.to_owned(),
            actual: payload.chars().count(),
            expected,
            opcode,
        })
    }
}

/// One hex nibble, with the diagnostic the failure needs.
fn nibble_at(payload: &str, bytes: &[u8], offset: usize) -> Result<u8, ProtocolError> {
    nibble(bytes[offset]).ok_or_else(|| ProtocolError::NotHex {
        payload: payload.to_owned(),
        ch: char::from(bytes[offset]),
        offset,
    })
}

/// One ASCII hex digit to its value, either case.
const fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// One nibble value to its upper-case ASCII digit.
///
/// # Panics
/// Never in practice: callers mask to four bits first, and the `unreachable` arm is what makes
/// that contract checkable rather than assumed.
const fn hex_digit(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        10..=15 => b'A' + (n - 10),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_captured_pairs_decode_to_the_values_that_corrected_the_design() {
        // These two are the acceptance criterion of M3-T01 verbatim. The first is exact against
        // an independently known constant, which is what licenses trusting the second.
        assert_eq!(decode_u24("00B289", 'a').map(U24::get), Ok(9_024_000));
        assert_eq!(decode_u24("A7FD00", 'b').map(U24::get), Ok(64_935));
        // And the third capture: both axes read dead on the mechanical home counter.
        assert_eq!(decode_u24("000080", 'j').map(U24::get), Ok(0x0080_0000));
        assert_eq!(U24::HOME.get(), 0x0080_0000);
    }

    #[test]
    fn the_byte_swap_is_visible_in_asymmetric_digits() {
        // A palindrome would pass under either byte order; these cannot.
        let cases: [(u32, &str); 6] = [
            (0x0012_3456, "563412"),
            (0x00AB_CDEF, "EFCDAB"),
            (0x0089_B200, "00B289"), // the CPR capture
            (0x0000_FDA7, "A7FD00"), // the timer-frequency capture
            (0x0000_0001, "010000"),
            (0x00FF_0000, "0000FF"),
        ];
        for (value, wire) in cases {
            let u = U24::new(value).expect("fixture fits 24 bits");
            assert_eq!(
                std::str::from_utf8(&encode_u24(u)),
                Ok(wire),
                "{value:#08X} must encode low byte first"
            );
            assert_eq!(decode_u24(wire, 'j').map(U24::get), Ok(value));
        }
    }

    #[test]
    fn the_full_24_bit_domain_round_trips() {
        // 16,777,216 values, allocation-free, ~1 s in a debug build. Exhaustive beats sampled
        // here for the same reason it beats a property test: the domain is small enough that
        // "no counterexample exists" is provable rather than merely unrefuted.
        for value in 0..=U24_MAX {
            let u = U24(value);
            let wire = encode_u24(u);
            let text = std::str::from_utf8(&wire).expect("hex digits are ASCII");
            match decode_u24(text, 'j') {
                Ok(back) if back.get() == value => {}
                other => panic!("{value:#08X} encoded as {text:?} decoded as {other:?}"),
            }
        }
    }

    #[test]
    fn decoding_accepts_either_case_and_encoding_emits_the_mounts() {
        assert_eq!(decode_u24("00b289", 'a').map(U24::get), Ok(9_024_000));
        assert_eq!(decode_u24("00B289", 'a').map(U24::get), Ok(9_024_000));
        let u = U24::new(9_024_000).expect("fits");
        assert_eq!(std::str::from_utf8(&encode_u24(u)), Ok("00B289"));
    }

    #[test]
    fn a_value_wider_than_the_register_is_refused_before_it_is_formatted() {
        assert_eq!(
            U24::new(0x0100_0000),
            Err(EncodeError::NotU24 {
                value: 0x0100_0000,
                max: U24_MAX
            })
        );
        assert!(U24::new(U24_MAX).is_ok());
    }

    #[test]
    fn the_three_non_u24_widths_do_not_share_a_decoder() {
        // `:g` — two characters, and the value PRD §4.2 assumed.
        assert_eq!(decode_u8_plain("10", 'g'), Ok(16));
        // Pointing the u24 decoder at it fails loudly rather than inventing a number.
        assert!(matches!(
            decode_u24("10", 'g'),
            Err(ProtocolError::PayloadWidth {
                expected: 6,
                actual: 2,
                ..
            })
        ));
        // ...and pointing the two-character decoder at a u24 reply fails the same way.
        assert!(matches!(
            decode_u8_plain("00B289", 'a'),
            Err(ProtocolError::PayloadWidth {
                expected: 2,
                actual: 6,
                ..
            })
        ));
    }

    #[test]
    fn the_firmware_reply_is_not_byte_swapped_and_the_mount_model_proves_it() {
        let raw = decode_u24_plain("020401", 'e').expect("six hex characters");
        assert_eq!(
            raw, 0x0002_0401,
            "`:e` is read big-endian, not low-byte-first"
        );
        let v = FirmwareVersion::from_raw(raw);
        assert_eq!((v.major, v.minor, v.model_code), (0x02, 0x04, 0x01));
        // The falsifiable part: the low byte names the mount the capture came from.
        assert_eq!(v.model(), Some(MountModel::Heq5));
        assert_eq!(v.model().map(MountModel::as_str), Some("HEQ5"));

        // Had the byte-swap rule applied, the model code would be 0x02 — an EQ5, which is not
        // the mount that answered. This assertion is the cross-check written down.
        let swapped = decode_u24("020401", 'e').expect("also valid hex").get();
        assert_eq!(swapped, 0x0001_0402);
        assert_eq!(
            MountModel::from_code((swapped & 0xFF) as u8),
            Some(MountModel::Eq5)
        );
    }

    #[test]
    fn an_unlisted_model_code_is_not_an_error() {
        let v = FirmwareVersion::from_raw(0x0002_04FE);
        assert_eq!(
            v.model(),
            None,
            "an unknown mount still works; only its label is missing"
        );
        assert_eq!(
            v.raw, 0x0002_04FE,
            "the measurement survives the failed interpretation"
        );
    }

    #[test]
    fn a_non_hex_character_is_named_with_its_offset() {
        assert_eq!(
            decode_u24("00B2G9", 'a'),
            Err(ProtocolError::NotHex {
                payload: "00B2G9".to_owned(),
                ch: 'G',
                offset: 4
            })
        );
    }

    #[test]
    fn the_counter_wraps_within_24_bits_not_32() {
        assert_eq!(U24::MAX.wrapping_add_signed(1), U24::ZERO);
        assert_eq!(U24::ZERO.wrapping_add_signed(-1), U24::MAX);
        assert_eq!(
            U24::HOME.wrapping_add_signed(1_000).get(),
            0x0080_0000 + 1_000
        );
        assert_eq!(
            U24::HOME.wrapping_add_signed(-1_000).get(),
            0x0080_0000 - 1_000
        );
    }
}
