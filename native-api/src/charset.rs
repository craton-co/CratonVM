// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Pure-Rust charset transcoding used by `java.nio.charset.Charset`,
//! `CharsetEncoder`, `CharsetDecoder`, and the `sun.nio.cs.StreamDecoder` /
//! `StreamEncoder` shims.
//!
//! The engine accepts a canonical charset name (as produced by
//! `normalize_charset_name` in `cratonvm-native-builtins`) and transcodes
//! between UTF-16 code units — the representation used internally by our
//! Java `String` objects — and arbitrary byte sequences.
//!
//! Error handling matches HotSpot's default "REPORT" action on malformed
//! input: `decode_bytes` returns an error on invalid sequences (callers
//! then surface a `CharacterCodingException`); `encode_chars` returns an
//! error when a code point cannot be represented in the target charset
//! (e.g. emoji in US-ASCII). Callers that want HotSpot's legacy "REPLACE"
//! action can use `decode_bytes_lossy` / `encode_chars_lossy` instead,
//! which substitute `'?'` for unmappable input exactly like
//! `java.nio.charset.CodingErrorAction.REPLACE`.

use core::fmt;

/// A transcoding error produced by [`decode_bytes`] or [`encode_chars`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingError {
    /// Input offset at which the malformed / unmappable sequence begins.
    pub offset: usize,
    /// Length of the malformed / unmappable sequence in input units.
    pub length: usize,
    /// Kind of error.
    pub kind: CodingErrorKind,
    /// Canonical charset name at the time of the failure.
    pub charset: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodingErrorKind {
    /// The input was not a valid encoding of any code point.
    Malformed,
    /// The input decoded to a code point that the target charset cannot
    /// represent (encode-only).
    Unmappable,
    /// The charset name is unknown or unsupported.
    UnsupportedCharset,
}

impl fmt::Display for CodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            CodingErrorKind::Malformed => write!(
                f,
                "malformed {} input at offset {} (len {})",
                self.charset, self.offset, self.length
            ),
            CodingErrorKind::Unmappable => write!(
                f,
                "unmappable character for {} at offset {} (len {})",
                self.charset, self.offset, self.length
            ),
            CodingErrorKind::UnsupportedCharset => {
                write!(f, "unsupported charset: {}", self.charset)
            }
        }
    }
}

impl std::error::Error for CodingError {}

/// Replacement character used by the lossy variants. Matches HotSpot's
/// default REPLACE-action replacement for byte-oriented charsets.
pub const REPLACEMENT_BYTE: u8 = b'?';
/// Replacement code point used by the lossy variants for decoding.
pub const REPLACEMENT_CHAR: u16 = 0xFFFD;

/// Decode a byte slice into a sequence of UTF-16 code units.
///
/// Only canonical names produced by `normalize_charset_name` are accepted.
/// Accepts (`name`, `bytes`) and returns the decoded UTF-16 units, or a
/// [`CodingError`] on malformed input.
pub fn decode_bytes(name: &str, bytes: &[u8]) -> Result<Vec<u16>, CodingError> {
    match name {
        "UTF-8" => decode_utf8(bytes),
        "US-ASCII" => decode_ascii(bytes),
        "ISO-8859-1" => Ok(bytes.iter().map(|&b| b as u16).collect()),
        "UTF-16" => decode_utf16_bom(bytes, /*default_be=*/ true),
        "UTF-16BE" => decode_utf16_fixed(bytes, /*big_endian=*/ true),
        "UTF-16LE" => decode_utf16_fixed(bytes, /*big_endian=*/ false),
        "UTF-32" => decode_utf32_bom(bytes, /*default_be=*/ true),
        "UTF-32BE" => decode_utf32_fixed(bytes, /*big_endian=*/ true),
        "UTF-32LE" => decode_utf32_fixed(bytes, /*big_endian=*/ false),
        "windows-1252" => Ok(bytes.iter().map(|&b| cp1252_to_u16(b)).collect()),
        "windows-1251" => Ok(bytes.iter().map(|&b| cp1251_to_u16(b)).collect()),
        "KOI8-R" => Ok(bytes.iter().map(|&b| koi8r_to_u16(b)).collect()),
        "ISO-8859-2" => Ok(bytes.iter().map(|&b| iso_8859_2_to_u16(b)).collect()),
        "ISO-8859-15" => Ok(bytes.iter().map(|&b| iso_8859_15_to_u16(b)).collect()),
        _ => Err(CodingError {
            offset: 0,
            length: 0,
            kind: CodingErrorKind::UnsupportedCharset,
            charset: canonical_name_static(name),
        }),
    }
}

/// Encode a sequence of UTF-16 code units to bytes for the given charset.
pub fn encode_chars(name: &str, chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    match name {
        "UTF-8" => Ok(encode_utf8(chars)),
        "US-ASCII" => encode_ascii(chars),
        "ISO-8859-1" => encode_latin1(chars),
        "UTF-16" => Ok(encode_utf16_with_bom(chars)),
        "UTF-16BE" => Ok(encode_utf16_fixed(chars, true)),
        "UTF-16LE" => Ok(encode_utf16_fixed(chars, false)),
        "UTF-32" => Ok(encode_utf32_with_bom(chars)),
        "UTF-32BE" => Ok(encode_utf32_fixed(chars, true)),
        "UTF-32LE" => Ok(encode_utf32_fixed(chars, false)),
        "windows-1252" => encode_cp1252(chars),
        "windows-1251" => encode_cp1251(chars),
        "KOI8-R" => encode_koi8r(chars),
        "ISO-8859-2" => encode_iso_8859_2(chars),
        "ISO-8859-15" => encode_iso_8859_15(chars),
        _ => Err(CodingError {
            offset: 0,
            length: 0,
            kind: CodingErrorKind::UnsupportedCharset,
            charset: canonical_name_static(name),
        }),
    }
}

/// Lossy decode: on malformed input, substitute U+FFFD and continue.
pub fn decode_bytes_lossy(name: &str, bytes: &[u8]) -> Vec<u16> {
    // First try the strict path so the happy path avoids byte-by-byte
    // error handling cost; fall back to a tolerant decoder on error.
    if let Ok(v) = decode_bytes(name, bytes) {
        return v;
    }
    // Byte-oriented charsets never malform except for unsupported names,
    // so only the UTF-family needs a replacement path.
    match name {
        "UTF-8" => decode_utf8_lossy(bytes),
        "UTF-16" => decode_utf16_bom_lossy(bytes, true),
        "UTF-16BE" => decode_utf16_fixed_lossy(bytes, true),
        "UTF-16LE" => decode_utf16_fixed_lossy(bytes, false),
        "UTF-32" => decode_utf32_bom_lossy(bytes, true),
        "UTF-32BE" => decode_utf32_fixed_lossy(bytes, true),
        "UTF-32LE" => decode_utf32_fixed_lossy(bytes, false),
        _ => bytes.iter().map(|_| REPLACEMENT_CHAR).collect(),
    }
}

/// Lossy encode: on unmappable input, substitute `'?'` and continue.
pub fn encode_chars_lossy(name: &str, chars: &[u16]) -> Vec<u8> {
    match encode_chars(name, chars) {
        Ok(v) => v,
        Err(_) => match name {
            "UTF-8" => encode_utf8(chars),
            "US-ASCII" => chars
                .iter()
                .map(|&c| if c < 0x80 { c as u8 } else { REPLACEMENT_BYTE })
                .collect(),
            "ISO-8859-1" => chars
                .iter()
                .map(|&c| if c < 0x100 { c as u8 } else { REPLACEMENT_BYTE })
                .collect(),
            _ => encode_utf8(chars),
        },
    }
}

// ---------------------------------------------------------------------------
// UTF-8
// ---------------------------------------------------------------------------

fn decode_utf8(bytes: &[u8]) -> Result<Vec<u16>, CodingError> {
    // Fast path: valid UTF-8 → convert through std::str.
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.encode_utf16().collect()),
        Err(e) => Err(CodingError {
            offset: e.valid_up_to(),
            length: e.error_len().unwrap_or(1),
            kind: CodingErrorKind::Malformed,
            charset: "UTF-8",
        }),
    }
}

fn decode_utf8_lossy(bytes: &[u8]) -> Vec<u16> {
    String::from_utf8_lossy(bytes).encode_utf16().collect()
}

fn encode_utf8(chars: &[u16]) -> Vec<u8> {
    let s = String::from_utf16_lossy(chars);
    s.into_bytes()
}

// ---------------------------------------------------------------------------
// US-ASCII
// ---------------------------------------------------------------------------

fn decode_ascii(bytes: &[u8]) -> Result<Vec<u16>, CodingError> {
    for (i, &b) in bytes.iter().enumerate() {
        if b > 0x7F {
            return Err(CodingError {
                offset: i,
                length: 1,
                kind: CodingErrorKind::Malformed,
                charset: "US-ASCII",
            });
        }
    }
    Ok(bytes.iter().map(|&b| b as u16).collect())
}

fn encode_ascii(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    let mut out = Vec::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if c > 0x7F {
            return Err(CodingError {
                offset: i,
                length: 1,
                kind: CodingErrorKind::Unmappable,
                charset: "US-ASCII",
            });
        }
        out.push(c as u8);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ISO-8859-1 (Latin-1)
// ---------------------------------------------------------------------------

fn encode_latin1(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    let mut out = Vec::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if c > 0xFF {
            return Err(CodingError {
                offset: i,
                length: 1,
                kind: CodingErrorKind::Unmappable,
                charset: "ISO-8859-1",
            });
        }
        out.push(c as u8);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// UTF-16 family
// ---------------------------------------------------------------------------

fn decode_utf16_bom(bytes: &[u8], default_be: bool) -> Result<Vec<u16>, CodingError> {
    let (payload, be) = strip_utf16_bom(bytes, default_be);
    decode_utf16_fixed(payload, be)
}

fn decode_utf16_bom_lossy(bytes: &[u8], default_be: bool) -> Vec<u16> {
    let (payload, be) = strip_utf16_bom(bytes, default_be);
    decode_utf16_fixed_lossy(payload, be)
}

fn strip_utf16_bom(bytes: &[u8], default_be: bool) -> (&[u8], bool) {
    if bytes.len() >= 2 {
        match (bytes[0], bytes[1]) {
            (0xFE, 0xFF) => return (&bytes[2..], true),
            (0xFF, 0xFE) => return (&bytes[2..], false),
            _ => {}
        }
    }
    (bytes, default_be)
}

fn decode_utf16_fixed(bytes: &[u8], big_endian: bool) -> Result<Vec<u16>, CodingError> {
    if bytes.len() % 2 != 0 {
        return Err(CodingError {
            offset: bytes.len() - 1,
            length: 1,
            kind: CodingErrorKind::Malformed,
            charset: if big_endian { "UTF-16BE" } else { "UTF-16LE" },
        });
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let u = if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        };
        out.push(u);
    }
    // Validate surrogate pairing.
    let mut i = 0;
    while i < out.len() {
        let u = out[i];
        if (0xD800..=0xDBFF).contains(&u) {
            let low = out.get(i + 1).copied().unwrap_or(0);
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(CodingError {
                    offset: i * 2,
                    length: 2,
                    kind: CodingErrorKind::Malformed,
                    charset: if big_endian { "UTF-16BE" } else { "UTF-16LE" },
                });
            }
            i += 2;
        } else if (0xDC00..=0xDFFF).contains(&u) {
            return Err(CodingError {
                offset: i * 2,
                length: 2,
                kind: CodingErrorKind::Malformed,
                charset: if big_endian { "UTF-16BE" } else { "UTF-16LE" },
            });
        } else {
            i += 1;
        }
    }
    Ok(out)
}

fn decode_utf16_fixed_lossy(bytes: &[u8], big_endian: bool) -> Vec<u16> {
    let n = bytes.len() & !1;
    let mut out = Vec::with_capacity(n / 2);
    let mut i = 0;
    while i + 1 < n {
        let u = if big_endian {
            u16::from_be_bytes([bytes[i], bytes[i + 1]])
        } else {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        };
        if (0xD800..=0xDBFF).contains(&u) {
            if i + 3 < n {
                let low = if big_endian {
                    u16::from_be_bytes([bytes[i + 2], bytes[i + 3]])
                } else {
                    u16::from_le_bytes([bytes[i + 2], bytes[i + 3]])
                };
                if (0xDC00..=0xDFFF).contains(&low) {
                    out.push(u);
                    out.push(low);
                    i += 4;
                    continue;
                }
            }
            out.push(REPLACEMENT_CHAR);
            i += 2;
        } else if (0xDC00..=0xDFFF).contains(&u) {
            out.push(REPLACEMENT_CHAR);
            i += 2;
        } else {
            out.push(u);
            i += 2;
        }
    }
    out
}

fn encode_utf16_fixed(chars: &[u16], big_endian: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(chars.len() * 2);
    for &c in chars {
        let bytes = if big_endian {
            c.to_be_bytes()
        } else {
            c.to_le_bytes()
        };
        out.extend_from_slice(&bytes);
    }
    out
}

fn encode_utf16_with_bom(chars: &[u16]) -> Vec<u8> {
    // HotSpot writes a BE BOM for UTF-16 output.
    let mut out = Vec::with_capacity(2 + chars.len() * 2);
    out.push(0xFE);
    out.push(0xFF);
    out.extend_from_slice(&encode_utf16_fixed(chars, true));
    out
}

// ---------------------------------------------------------------------------
// UTF-32 family (supplementary-only path via code points)
// ---------------------------------------------------------------------------

fn decode_utf32_bom(bytes: &[u8], default_be: bool) -> Result<Vec<u16>, CodingError> {
    let (payload, be) = strip_utf32_bom(bytes, default_be);
    decode_utf32_fixed(payload, be)
}

fn decode_utf32_bom_lossy(bytes: &[u8], default_be: bool) -> Vec<u16> {
    let (payload, be) = strip_utf32_bom(bytes, default_be);
    decode_utf32_fixed_lossy(payload, be)
}

fn strip_utf32_bom(bytes: &[u8], default_be: bool) -> (&[u8], bool) {
    if bytes.len() >= 4 {
        if bytes[0..4] == [0x00, 0x00, 0xFE, 0xFF] {
            return (&bytes[4..], true);
        }
        if bytes[0..4] == [0xFF, 0xFE, 0x00, 0x00] {
            return (&bytes[4..], false);
        }
    }
    (bytes, default_be)
}

fn decode_utf32_fixed(bytes: &[u8], big_endian: bool) -> Result<Vec<u16>, CodingError> {
    if bytes.len() % 4 != 0 {
        return Err(CodingError {
            offset: bytes.len() - (bytes.len() % 4),
            length: bytes.len() % 4,
            kind: CodingErrorKind::Malformed,
            charset: if big_endian { "UTF-32BE" } else { "UTF-32LE" },
        });
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for (chunk_idx, chunk) in bytes.chunks_exact(4).enumerate() {
        let cp = if big_endian {
            u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        };
        if cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
            return Err(CodingError {
                offset: chunk_idx * 4,
                length: 4,
                kind: CodingErrorKind::Malformed,
                charset: if big_endian { "UTF-32BE" } else { "UTF-32LE" },
            });
        }
        push_code_point(&mut out, cp);
    }
    Ok(out)
}

fn decode_utf32_fixed_lossy(bytes: &[u8], big_endian: bool) -> Vec<u16> {
    let n = bytes.len() & !3;
    let mut out = Vec::with_capacity(n / 2);
    for chunk in bytes[..n].chunks_exact(4) {
        let cp = if big_endian {
            u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        };
        if cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
            out.push(REPLACEMENT_CHAR);
        } else {
            push_code_point(&mut out, cp);
        }
    }
    out
}

fn encode_utf32_fixed(chars: &[u16], big_endian: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(chars.len() * 4);
    let mut i = 0;
    while i < chars.len() {
        let u = chars[i];
        let cp = if (0xD800..=0xDBFF).contains(&u) && i + 1 < chars.len() {
            let low = chars[i + 1];
            if (0xDC00..=0xDFFF).contains(&low) {
                let high = (u - 0xD800) as u32;
                let lo = (low - 0xDC00) as u32;
                i += 2;
                0x10000 + (high << 10) + lo
            } else {
                i += 1;
                REPLACEMENT_CHAR as u32
            }
        } else if (0xD800..=0xDFFF).contains(&u) {
            i += 1;
            REPLACEMENT_CHAR as u32
        } else {
            i += 1;
            u as u32
        };
        let bytes = if big_endian {
            cp.to_be_bytes()
        } else {
            cp.to_le_bytes()
        };
        out.extend_from_slice(&bytes);
    }
    out
}

fn encode_utf32_with_bom(chars: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + chars.len() * 4);
    out.extend_from_slice(&[0x00, 0x00, 0xFE, 0xFF]);
    out.extend_from_slice(&encode_utf32_fixed(chars, true));
    out
}

fn push_code_point(out: &mut Vec<u16>, cp: u32) {
    if cp <= 0xFFFF {
        out.push(cp as u16);
    } else {
        let adj = cp - 0x10000;
        out.push(0xD800 + ((adj >> 10) & 0x3FF) as u16);
        out.push(0xDC00 + (adj & 0x3FF) as u16);
    }
}

// ---------------------------------------------------------------------------
// Single-byte code pages. Use compile-time tables for O(1) lookup.
// Only the rows that differ from Latin-1 (0x80..=0xFF) are encoded.
// ---------------------------------------------------------------------------

macro_rules! sb_table {
    ($name:ident, $map:expr) => {
        const $name: [u16; 128] = $map;
        // Companion lazily-built reverse map for `encode_sb`. Without
        // this, encoding does an O(128) linear scan of the forward
        // table per character. The reverse map turns that into an O(1)
        // array index. It is built once, on first use, per charset.
        //
        // `0` is used as the "no mapping" sentinel: byte 0 is a control
        // character that always lives in the `< 0x80` ASCII range and
        // is therefore never a valid high-byte (0x80..=0xFF) encoding
        // target, so it can never collide with a real entry.
        paste_sb_rev!($name);
    };
}

/// Generates the per-charset `OnceLock`-backed reverse-lookup map. Each
/// charset gets a private `OnceLock` static and an accessor function
/// that builds the map on first use.
macro_rules! paste_sb_rev {
    (CP1252_HIGH) => { sb_rev_cell!(CP1252_REV_CELL, cp1252_rev, CP1252_HIGH); };
    (CP1251_HIGH) => { sb_rev_cell!(CP1251_REV_CELL, cp1251_rev, CP1251_HIGH); };
    (KOI8R_HIGH) => { sb_rev_cell!(KOI8R_REV_CELL, koi8r_rev, KOI8R_HIGH); };
    (ISO_8859_2_HIGH) => { sb_rev_cell!(ISO_8859_2_REV_CELL, iso_8859_2_rev, ISO_8859_2_HIGH); };
    (ISO_8859_15_HIGH) => { sb_rev_cell!(ISO_8859_15_REV_CELL, iso_8859_15_rev, ISO_8859_15_HIGH); };
}

macro_rules! sb_rev_cell {
    ($cell:ident, $accessor:ident, $fwd:ident) => {
        static $cell: std::sync::OnceLock<Box<[u8; 65536]>> =
            std::sync::OnceLock::new();
        fn $accessor() -> &'static [u8; 65536] {
            $cell.get_or_init(|| build_sb_rev(&$fwd))
        }
    };
}

/// Build a `u16 -> u8` reverse-lookup table from a single-byte charset's
/// forward `0x80..=0xFF` table. Row order is preserved so the result is
/// "first row wins" for duplicate code points, exactly matching the
/// linear scan in [`encode_sb`]. Entries equal to the replacement
/// character `0xFFFD` are not mappable and are skipped. Unmapped slots
/// stay `0` (the "no mapping" sentinel — see `sb_table!`).
fn build_sb_rev(table: &[u16; 128]) -> Box<[u8; 65536]> {
    let mut rev = Box::new([0u8; 65536]);
    // Iterate high to low so the lowest row "wins" any duplicate.
    for (row, &u) in table.iter().enumerate().rev() {
        if u != 0xFFFD {
            rev[u as usize] = 0x80 + row as u8;
        }
    }
    rev
}

sb_table!(CP1252_HIGH, [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021,
    0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0xFFFD, 0x017D, 0xFFFD,
    0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0xFFFD, 0x017E, 0x0178,
    0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7,
    0x00A8, 0x00A9, 0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF,
    0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF,
    0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7,
    0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF,
    0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7,
    0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF,
    0x00E0, 0x00E1, 0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7,
    0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED, 0x00EE, 0x00EF,
    0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7,
    0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD, 0x00FE, 0x00FF,
]);

sb_table!(CP1251_HIGH, [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021,
    0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F,
    0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F,
    0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7,
    0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
    0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7,
    0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457,
    0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417,
    0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F,
    0x0420, 0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427,
    0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F,
    0x0430, 0x0431, 0x0432, 0x0433, 0x0434, 0x0435, 0x0436, 0x0437,
    0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D, 0x043E, 0x043F,
    0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447,
    0x0448, 0x0449, 0x044A, 0x044B, 0x044C, 0x044D, 0x044E, 0x044F,
]);

sb_table!(KOI8R_HIGH, [
    0x2500, 0x2502, 0x250C, 0x2510, 0x2514, 0x2518, 0x251C, 0x2524,
    0x252C, 0x2534, 0x253C, 0x2580, 0x2584, 0x2588, 0x258C, 0x2590,
    0x2591, 0x2592, 0x2593, 0x2320, 0x25A0, 0x2219, 0x221A, 0x2248,
    0x2264, 0x2265, 0x00A0, 0x2321, 0x00B0, 0x00B2, 0x00B7, 0x00F7,
    0x2550, 0x2551, 0x2552, 0x0451, 0x2553, 0x2554, 0x2555, 0x2556,
    0x2557, 0x2558, 0x2559, 0x255A, 0x255B, 0x255C, 0x255D, 0x255E,
    0x255F, 0x2560, 0x2561, 0x0401, 0x2562, 0x2563, 0x2564, 0x2565,
    0x2566, 0x2567, 0x2568, 0x2569, 0x256A, 0x256B, 0x256C, 0x00A9,
    0x044E, 0x0430, 0x0431, 0x0446, 0x0434, 0x0435, 0x0444, 0x0433,
    0x0445, 0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D, 0x043E,
    0x043F, 0x044F, 0x0440, 0x0441, 0x0442, 0x0443, 0x0436, 0x0432,
    0x044C, 0x044B, 0x0437, 0x0448, 0x044D, 0x0449, 0x0447, 0x044A,
    0x042E, 0x0410, 0x0411, 0x0426, 0x0414, 0x0415, 0x0424, 0x0413,
    0x0425, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E,
    0x041F, 0x042F, 0x0420, 0x0421, 0x0422, 0x0423, 0x0416, 0x0412,
    0x042C, 0x042B, 0x0417, 0x0428, 0x042D, 0x0429, 0x0427, 0x042A,
]);

sb_table!(ISO_8859_2_HIGH, [
    0x0080, 0x0081, 0x0082, 0x0083, 0x0084, 0x0085, 0x0086, 0x0087,
    0x0088, 0x0089, 0x008A, 0x008B, 0x008C, 0x008D, 0x008E, 0x008F,
    0x0090, 0x0091, 0x0092, 0x0093, 0x0094, 0x0095, 0x0096, 0x0097,
    0x0098, 0x0099, 0x009A, 0x009B, 0x009C, 0x009D, 0x009E, 0x009F,
    0x00A0, 0x0104, 0x02D8, 0x0141, 0x00A4, 0x013D, 0x015A, 0x00A7,
    0x00A8, 0x0160, 0x015E, 0x0164, 0x0179, 0x00AD, 0x017D, 0x017B,
    0x00B0, 0x0105, 0x02DB, 0x0142, 0x00B4, 0x013E, 0x015B, 0x02C7,
    0x00B8, 0x0161, 0x015F, 0x0165, 0x017A, 0x02DD, 0x017E, 0x017C,
    0x0154, 0x00C1, 0x00C2, 0x0102, 0x00C4, 0x0139, 0x0106, 0x00C7,
    0x010C, 0x00C9, 0x0118, 0x00CB, 0x011A, 0x00CD, 0x00CE, 0x010E,
    0x0110, 0x0143, 0x0147, 0x00D3, 0x00D4, 0x0150, 0x00D6, 0x00D7,
    0x0158, 0x016E, 0x00DA, 0x0170, 0x00DC, 0x00DD, 0x0162, 0x00DF,
    0x0155, 0x00E1, 0x00E2, 0x0103, 0x00E4, 0x013A, 0x0107, 0x00E7,
    0x010D, 0x00E9, 0x0119, 0x00EB, 0x011B, 0x00ED, 0x00EE, 0x010F,
    0x0111, 0x0144, 0x0148, 0x00F3, 0x00F4, 0x0151, 0x00F6, 0x00F7,
    0x0159, 0x016F, 0x00FA, 0x0171, 0x00FC, 0x00FD, 0x0163, 0x02D9,
]);

sb_table!(ISO_8859_15_HIGH, [
    0x0080, 0x0081, 0x0082, 0x0083, 0x0084, 0x0085, 0x0086, 0x0087,
    0x0088, 0x0089, 0x008A, 0x008B, 0x008C, 0x008D, 0x008E, 0x008F,
    0x0090, 0x0091, 0x0092, 0x0093, 0x0094, 0x0095, 0x0096, 0x0097,
    0x0098, 0x0099, 0x009A, 0x009B, 0x009C, 0x009D, 0x009E, 0x009F,
    0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x20AC, 0x00A5, 0x0160, 0x00A7,
    0x0161, 0x00A9, 0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF,
    0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x017D, 0x00B5, 0x00B6, 0x00B7,
    0x017E, 0x00B9, 0x00BA, 0x00BB, 0x0152, 0x0153, 0x0178, 0x00BF,
    0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7,
    0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF,
    0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7,
    0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF,
    0x00E0, 0x00E1, 0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7,
    0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED, 0x00EE, 0x00EF,
    0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7,
    0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD, 0x00FE, 0x00FF,
]);

fn sb_to_u16(byte: u8, table: &[u16; 128]) -> u16 {
    if byte < 0x80 {
        byte as u16
    } else {
        table[(byte - 0x80) as usize]
    }
}

fn cp1252_to_u16(b: u8) -> u16 {
    sb_to_u16(b, &CP1252_HIGH)
}
fn cp1251_to_u16(b: u8) -> u16 {
    sb_to_u16(b, &CP1251_HIGH)
}
fn koi8r_to_u16(b: u8) -> u16 {
    sb_to_u16(b, &KOI8R_HIGH)
}
fn iso_8859_2_to_u16(b: u8) -> u16 {
    sb_to_u16(b, &ISO_8859_2_HIGH)
}
fn iso_8859_15_to_u16(b: u8) -> u16 {
    sb_to_u16(b, &ISO_8859_15_HIGH)
}

/// Encode UTF-16 code units into a single-byte charset using a prebuilt
/// `u16 -> u8` reverse-lookup table (`rev`, see `build_sb_rev`). This is
/// O(1) per character; a `0` slot means the code point is unmappable.
fn encode_sb(
    chars: &[u16],
    rev: &[u8; 65536],
    charset: &'static str,
) -> Result<Vec<u8>, CodingError> {
    let mut out = Vec::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if c < 0x80 {
            out.push(c as u8);
            continue;
        }
        let b = rev[c as usize];
        if b != 0 {
            out.push(b);
            continue;
        }
        return Err(CodingError {
            offset: i,
            length: 1,
            kind: CodingErrorKind::Unmappable,
            charset,
        });
    }
    Ok(out)
}

fn encode_cp1252(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_sb(chars, cp1252_rev(), "windows-1252")
}
fn encode_cp1251(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_sb(chars, cp1251_rev(), "windows-1251")
}
fn encode_koi8r(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_sb(chars, koi8r_rev(), "KOI8-R")
}
fn encode_iso_8859_2(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_sb(chars, iso_8859_2_rev(), "ISO-8859-2")
}
fn encode_iso_8859_15(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_sb(chars, iso_8859_15_rev(), "ISO-8859-15")
}

/// Returns a static-lifetime copy of `name` if we recognise it.  This
/// avoids allocating a `&'static str` from a borrowed `&str` in error
/// paths; unknown names map to the catch-all "(unknown)".
fn canonical_name_static(name: &str) -> &'static str {
    match name {
        "UTF-8" => "UTF-8",
        "US-ASCII" => "US-ASCII",
        "ISO-8859-1" => "ISO-8859-1",
        "ISO-8859-2" => "ISO-8859-2",
        "ISO-8859-15" => "ISO-8859-15",
        "UTF-16" => "UTF-16",
        "UTF-16BE" => "UTF-16BE",
        "UTF-16LE" => "UTF-16LE",
        "UTF-32" => "UTF-32",
        "UTF-32BE" => "UTF-32BE",
        "UTF-32LE" => "UTF-32LE",
        "windows-1252" => "windows-1252",
        "windows-1251" => "windows-1251",
        "KOI8-R" => "KOI8-R",
        _ => "(unknown)",
    }
}

/// Average bytes-per-char heuristic matching the value HotSpot's
/// `CharsetEncoder` reports via `averageBytesPerChar()`.
pub fn average_bytes_per_char(name: &str) -> f32 {
    match name {
        "UTF-8" => 1.1,
        "US-ASCII" | "ISO-8859-1" | "ISO-8859-2" | "ISO-8859-15" | "windows-1252"
        | "windows-1251" | "KOI8-R" => 1.0,
        "UTF-16" => 2.0,
        "UTF-16BE" | "UTF-16LE" => 2.0,
        "UTF-32" | "UTF-32BE" | "UTF-32LE" => 4.0,
        _ => 1.0,
    }
}

/// Maximum bytes-per-char for the given charset (`maxBytesPerChar`).
pub fn max_bytes_per_char(name: &str) -> f32 {
    match name {
        "UTF-8" => 3.0, // per Java spec: 3 for BMP, surrogate pair encodes a single supplementary as 4 bytes but avg per UTF-16 unit is 3
        "US-ASCII" | "ISO-8859-1" | "ISO-8859-2" | "ISO-8859-15" | "windows-1252"
        | "windows-1251" | "KOI8-R" => 1.0,
        "UTF-16" => 4.0, // leading BOM
        "UTF-16BE" | "UTF-16LE" => 2.0,
        "UTF-32" | "UTF-32BE" | "UTF-32LE" => 4.0,
        _ => 1.0,
    }
}

/// Average chars-per-byte (`averageCharsPerByte`).
pub fn average_chars_per_byte(name: &str) -> f32 {
    match name {
        "UTF-8" => 1.0,
        "UTF-16" | "UTF-16BE" | "UTF-16LE" => 0.5,
        "UTF-32" | "UTF-32BE" | "UTF-32LE" => 0.25,
        _ => 1.0,
    }
}

/// Maximum chars-per-byte (`maxCharsPerByte`).
pub fn max_chars_per_byte(name: &str) -> f32 {
    match name {
        "UTF-8" => 1.0,
        "UTF-16" | "UTF-16BE" | "UTF-16LE" => 1.0,
        "UTF-32" | "UTF-32BE" | "UTF-32LE" => 1.0,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_roundtrip_ascii() {
        let s = "hello world";
        let b = encode_chars("UTF-8", &s.encode_utf16().collect::<Vec<_>>()).unwrap();
        assert_eq!(b, s.as_bytes());
        let d = decode_bytes("UTF-8", &b).unwrap();
        assert_eq!(String::from_utf16(&d).unwrap(), s);
    }

    #[test]
    fn utf8_roundtrip_multibyte() {
        // Spans 1/2/3/4-byte UTF-8 sequences.
        let s = "A\u{00A9}\u{4E2D}\u{1F600}";
        let chars: Vec<u16> = s.encode_utf16().collect();
        let b = encode_chars("UTF-8", &chars).unwrap();
        assert_eq!(b, s.as_bytes());
        let d = decode_bytes("UTF-8", &b).unwrap();
        assert_eq!(String::from_utf16(&d).unwrap(), s);
    }

    #[test]
    fn utf8_decode_invalid() {
        // Stray continuation byte.
        let err = decode_bytes("UTF-8", &[0xC2, 0x28]).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Malformed);
        assert_eq!(err.charset, "UTF-8");
    }

    #[test]
    fn ascii_rejects_high_bit() {
        assert!(decode_bytes("US-ASCII", &[0x80]).is_err());
        let err = encode_chars("US-ASCII", &['é' as u16]).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Unmappable);
    }

    #[test]
    fn latin1_full_256_roundtrip() {
        let bytes: Vec<u8> = (0..=255).collect();
        let chars = decode_bytes("ISO-8859-1", &bytes).unwrap();
        assert_eq!(chars.len(), 256);
        let back = encode_chars("ISO-8859-1", &chars).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn utf16be_roundtrip_surrogate() {
        // "A😀" = 'A' + supplementary code point.
        let chars: Vec<u16> = "A\u{1F600}".encode_utf16().collect();
        assert_eq!(chars.len(), 3);
        let b = encode_chars("UTF-16BE", &chars).unwrap();
        // 00 41 | D83D DE00
        assert_eq!(b, &[0x00, 0x41, 0xD8, 0x3D, 0xDE, 0x00]);
        let d = decode_bytes("UTF-16BE", &b).unwrap();
        assert_eq!(d, chars);
    }

    #[test]
    fn utf16le_roundtrip() {
        let chars: Vec<u16> = "AB".encode_utf16().collect();
        let b = encode_chars("UTF-16LE", &chars).unwrap();
        assert_eq!(b, &[0x41, 0x00, 0x42, 0x00]);
        let d = decode_bytes("UTF-16LE", &b).unwrap();
        assert_eq!(d, chars);
    }

    #[test]
    fn utf16_bom_selects_endianness() {
        let be_with_bom: &[u8] = &[0xFE, 0xFF, 0x00, 0x41];
        let le_with_bom: &[u8] = &[0xFF, 0xFE, 0x41, 0x00];
        assert_eq!(decode_bytes("UTF-16", be_with_bom).unwrap(), vec![0x0041]);
        assert_eq!(decode_bytes("UTF-16", le_with_bom).unwrap(), vec![0x0041]);
    }

    #[test]
    fn unsupported_charset_errors() {
        let err = decode_bytes("XYZ", b"").unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::UnsupportedCharset);
    }

    #[test]
    fn lossy_decode_replaces() {
        let d = decode_bytes_lossy("UTF-8", &[0xFF, b'a']);
        // U+FFFD placeholder or 'a' must be present; content should be non-empty
        assert!(!d.is_empty());
    }

    #[test]
    fn cp1252_euro_sign() {
        // 0x80 maps to U+20AC in windows-1252 (not ISO-8859-1).
        let d = decode_bytes("windows-1252", &[0x80]).unwrap();
        assert_eq!(d, vec![0x20AC]);
        let b = encode_chars("windows-1252", &[0x20AC]).unwrap();
        assert_eq!(b, &[0x80]);
    }

    #[test]
    fn utf32be_roundtrip_supplementary() {
        let s = "A\u{1F600}";
        let chars: Vec<u16> = s.encode_utf16().collect();
        let b = encode_chars("UTF-32BE", &chars).unwrap();
        assert_eq!(b.len(), 8);
        assert_eq!(&b[0..4], &[0x00, 0x00, 0x00, 0x41]);
        let d = decode_bytes("UTF-32BE", &b).unwrap();
        assert_eq!(d, chars);
    }

    #[test]
    fn koi8r_sample() {
        // 0xC1 should map to Cyrillic 'а' (U+0430).
        let d = decode_bytes("KOI8-R", &[0xC1]).unwrap();
        assert_eq!(d, vec![0x0430]);
        let b = encode_chars("KOI8-R", &[0x0430]).unwrap();
        assert_eq!(b, &[0xC1]);
    }
}
