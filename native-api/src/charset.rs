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
//! (e.g. emoji in US-ASCII) AND when the input UTF-16 unit sequence is
//! malformed — e.g. a lone (unpaired) surrogate in a UTF-8 encode, which
//! the strict path reports as `Malformed` rather than substituting U+FFFD.
//! Callers that want HotSpot's legacy "REPLACE"
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
    /// The input ended in the middle of an otherwise-valid multi-byte
    /// sequence (a *truncated trailing* sequence). With more input the
    /// sequence could still complete, so a streaming decoder that has NOT
    /// reached end-of-input should report this as UNDERFLOW rather than
    /// MALFORMED. At end-of-input it is treated as MALFORMED.
    Incomplete,
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
            CodingErrorKind::Incomplete => write!(
                f,
                "incomplete {} input at offset {} (len {})",
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

/// Canonicalize a charset *alias* (case-insensitive, `-`/`_`-insensitive) to
/// the canonical name this engine uses, or `None` when the alias has no
/// canonical mapping at all.
///
/// This is the single shared alias table: `normalize_charset_name` in
/// `cratonvm-native-builtins` and the `StreamEncoder`/`StreamDecoder` shims
/// in `cratonvm-native-io` all resolve through it. The stream shims used to
/// carry a stale private copy that was missing `IBM850` and every multibyte
/// family this engine has since gained, so an
/// `OutputStreamWriter(os, ibm850Charset)` silently encoded UTF-8 — Tomcat's
/// `TestDefaultServletEncoding*` DefaultServlet include conversion put
/// `C2 BD` on an ibm850 wire instead of `AB`
/// (tomcat-defaultservlet-encoding-content-failures-FIXED.md).
///
/// NOTE: `Some` here does **not** guarantee the engine can transcode the
/// charset (e.g. `KOI8-U` canonicalizes but has no codec); callers that need
/// a hard guarantee must probe `encode_chars` / `decode_bytes`.
pub fn canonical_charset_name(name: &str) -> Option<&'static str> {
    Some(match name.to_uppercase().replace(['-', '_'], "").as_str() {
        "UTF8" => "UTF-8",
        // "unicode" is the JDK's own alias for UTF-16 (`sun.nio.cs.UTF_16`'s
        // alias list is `{"UTF16", "utf16", "unicode", "UnicodeBig"}`), and
        // "UnicodeBigUnmarked"/"UnicodeLittleUnmarked" alias the no-BOM
        // BE/LE variants.
        "UTF16" | "UNICODE" | "UNICODEBIG" => "UTF-16",
        "UTF16BE" | "UNICODEBIGUNMARKED" => "UTF-16BE",
        "UTF16LE" | "UNICODELITTLEUNMARKED" => "UTF-16LE",
        "UTF32" => "UTF-32",
        "UTF32BE" => "UTF-32BE",
        "UTF32LE" => "UTF-32LE",
        // `ANSI_X3.4-1968` is what glibc's `nl_langinfo(CODESET)` answers in
        // the C/POSIX locale and therefore what HotSpot reports for
        // `native.encoding` / `stdout.encoding` there (MEASURED, Temurin
        // 25.0.3+9, `LANG=C`). It reached this table the moment CratonVM
        // started reporting the host's real encoding instead of a pinned
        // UTF-8, and an unmapped name here is an `UnsupportedCharsetException`
        // out of `Charset.forName` at bootstrap. The rest of the row is the
        // JDK's own alias list for `sun.nio.cs.US_ASCII`.
        "USASCII" | "ASCII" | "ANSIX3.41968" | "ANSIX3.41986" | "ISO646US" | "ISO646.IRV:1991"
        | "646" | "CSASCII" | "IBM367" | "CP367" | "ISOIR6" | "US" => "US-ASCII",
        // The JDK also accepts the historic `8859_1` spelling (used by
        // c3p0's resource-path reader) in addition to the ISO-prefixed
        // aliases. Underscores are removed above, yielding `88591`.
        "ISO88591" | "88591" | "LATIN1" | "ISO88591:1987" => "ISO-8859-1",
        "ISO88592" => "ISO-8859-2",
        "ISO88593" | "88593" | "LATIN3" => "ISO-8859-3",
        "ISO88594" | "88594" | "LATIN4" => "ISO-8859-4",
        "ISO88595" | "88595" | "CYRILLIC" => "ISO-8859-5",
        "ISO885915" => "ISO-8859-15",
        "SHIFTJIS" | "SJIS" | "CSSHIFTJIS" | "MSKANJI" | "WINDOWS31J" => "Shift_JIS",
        "EUCJP" | "XEUCJP" => "EUC-JP",
        "ISO2022JP" => "ISO-2022-JP",
        "BIG5" | "CSBIG5" | "BIG5HKSCS" => "Big5",
        "EUCKR" | "CSEUCKR" => "EUC-KR",
        "GB2312" | "CSGB2312" => "GB2312",
        "GBK" | "CP936" => "GBK",
        "GB18030" => "GB18030",
        // `ms<cp>` is how the JDK spells a Windows CONSOLE code page in the
        // 874..=950 band (`cratonvm_native_api::os_encoding`), so these names
        // now arrive from `stdout.encoding` on a CJK/Thai console. Each maps
        // to the closest family this engine actually transcodes; the same
        // approximation `WINDOWS31J -> Shift_JIS` above already makes.
        "MS932" => "Shift_JIS",
        "MS949" => "EUC-KR",
        "MS950" => "Big5",
        "WINDOWS1252" | "CP1252" => "windows-1252",
        "WINDOWS1251" | "CP1251" => "windows-1251",
        "WINDOWS1250" | "CP1250" => "windows-1250",
        "KOI8R" => "KOI8-R",
        "KOI8U" => "KOI8-U",
        "IBM850" | "CP850" | "850" | "CSPC850MULTILINGUAL" => "IBM850",
        "IBM1047" | "CP1047" | "1047" | "CCSID1047" => "IBM1047",
        "IBM500" | "CP500" | "500" | "CCSID500" | "EBCDICCPBE" | "EBCDICCPCH" => "IBM500",
        _ => return None,
    })
}

/// The name `InputStreamReader.getEncoding()` / `OutputStreamWriter.getEncoding()`
/// report for a canonical charset name — the JDK's **historical** name, not the
/// canonical one.
///
/// Both methods delegate to `StreamDecoder.encodingName()` /
/// `StreamEncoder.encodingName()`, which read:
///
/// ```text
/// return (cs instanceof HistoricallyNamedCharset hncs)
///        ? hncs.historicalName() : cs.name();
/// ```
///
/// Almost every charset in `java.base` implements `HistoricallyNamedCharset`,
/// so `new InputStreamReader(in, UTF_8).getEncoding()` is `"UTF8"`, not
/// `"UTF-8"` — which is what CratonVM returned, because its shims had only the
/// canonical name to hand. Found 2026-08-05 by `probes/ReaderWriterLayoutProbe`:
/// the one line of a 35-line paired transcript that diverged from Temurin
/// 25.0.3.
///
/// The table below IS that transcript — every canonical name
/// [`canonical_charset_name`] can produce, run through both methods on the host
/// JDK. Seven of the thirty report their canonical name and so are absent
/// (`UTF-16`, `UTF-32`, `UTF-32BE`, `UTF-32LE`, `Big5`, `GBK`, `GB18030`); an
/// unlisted name is returned unchanged, which is the right answer for a charset
/// that is not `HistoricallyNamedCharset`.
///
/// `GB2312 -> EUC_CN` is the one nobody would guess.
#[must_use]
pub fn historical_charset_name(canonical: &str) -> &str {
    match canonical {
        "UTF-8" => "UTF8",
        "UTF-16BE" => "UnicodeBigUnmarked",
        "UTF-16LE" => "UnicodeLittleUnmarked",
        "US-ASCII" => "ASCII",
        "ISO-8859-1" => "ISO8859_1",
        "ISO-8859-2" => "ISO8859_2",
        "ISO-8859-3" => "ISO8859_3",
        "ISO-8859-4" => "ISO8859_4",
        "ISO-8859-5" => "ISO8859_5",
        "ISO-8859-15" => "ISO8859_15",
        "Shift_JIS" => "SJIS",
        "EUC-JP" => "EUC_JP",
        "ISO-2022-JP" => "ISO2022JP",
        "EUC-KR" => "EUC_KR",
        "GB2312" => "EUC_CN",
        "windows-1250" => "Cp1250",
        "windows-1251" => "Cp1251",
        "windows-1252" => "Cp1252",
        "KOI8-R" => "KOI8_R",
        "KOI8-U" => "KOI8_U",
        "IBM850" => "Cp850",
        "IBM1047" => "Cp1047",
        "IBM500" => "Cp500",
        other => other,
    }
}

/// Map a CratonVM canonical charset name (as produced by
/// `normalize_charset_name`) to its `encoding_rs` implementation, for the
/// legacy / CJK multi-byte families the hand-written codecs above don't cover.
/// Single-byte and Unicode charsets are handled directly and return `None`
/// here so they keep their existing (faster, Java-exact) paths.
fn multibyte_encoding(name: &str) -> Option<&'static encoding_rs::Encoding> {
    Some(match name {
        "Shift_JIS" => encoding_rs::SHIFT_JIS,
        "EUC-JP" => encoding_rs::EUC_JP,
        "ISO-2022-JP" => encoding_rs::ISO_2022_JP,
        "Big5" => encoding_rs::BIG5,
        "EUC-KR" => encoding_rs::EUC_KR,
        // The JDK's GB2312 is a subset of GBK; encoding_rs folds both into GBK.
        "GBK" | "GB2312" => encoding_rs::GBK,
        "GB18030" => encoding_rs::GB18030,
        // windows-1250 has no hand-written table above; encoding_rs covers it.
        "windows-1250" => encoding_rs::WINDOWS_1250,
        _ => return None,
    })
}

/// Decode a byte slice into a sequence of UTF-16 code units.
///
/// Only canonical names produced by `normalize_charset_name` are accepted.
/// Accepts (`name`, `bytes`) and returns the decoded UTF-16 units, or a
/// [`CodingError`] on malformed input.
pub fn decode_bytes(name: &str, bytes: &[u8]) -> Result<Vec<u16>, CodingError> {
    match name {
        "UTF-8" => decode_utf8(bytes),
        "US-ASCII" => decode_ascii(bytes),
        "ISO-8859-1" | "ISO-8859-3" => Ok(bytes.iter().map(|&b| b as u16).collect()),
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
        "IBM850" => Ok(bytes.iter().map(|&b| ibm850_to_u16(b)).collect()),
        "IBM1047" => Ok(bytes.iter().map(|&b| ibm1047_to_u16(b)).collect()),
        "IBM500" => Ok(bytes.iter().map(|&b| ibm500_to_u16(b)).collect()),
        other => {
            // Legacy / CJK multi-byte charsets via encoding_rs. Strict decode:
            // `_without_replacement` returns None on malformed input (mirrors
            // the JDK's REPORT action), which we surface as a Malformed error.
            if let Some(enc) = multibyte_encoding(other) {
                match enc.decode_without_bom_handling_and_without_replacement(bytes) {
                    Some(cow) => Ok(cow.encode_utf16().collect()),
                    None => Err(CodingError {
                        offset: 0,
                        length: 0,
                        kind: CodingErrorKind::Malformed,
                        charset: canonical_name_static(name),
                    }),
                }
            } else {
                Err(CodingError {
                    offset: 0,
                    length: 0,
                    kind: CodingErrorKind::UnsupportedCharset,
                    charset: canonical_name_static(name),
                })
            }
        }
    }
}

/// Encode a sequence of UTF-16 code units to bytes for the given charset.
pub fn encode_chars(name: &str, chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    match name {
        // The strict encode path must REPORT malformed UTF-16 input (lone
        // surrogates) rather than silently substituting U+FFFD. HotSpot's
        // default REPORT action raises MalformedInputException for an unpaired
        // surrogate; lossy callers get the substituting paths below.
        "UTF-8" => encode_utf8_strict(chars),
        "US-ASCII" => {
            validate_utf16_units(chars, "US-ASCII")?;
            encode_ascii(chars)
        }
        "ISO-8859-1" => {
            validate_utf16_units(chars, "ISO-8859-1")?;
            encode_latin1(chars)
        }
        "UTF-16" => encode_utf16_with_bom_strict(chars),
        "UTF-16BE" => encode_utf16_fixed_strict(chars, true, "UTF-16BE"),
        "UTF-16LE" => encode_utf16_fixed_strict(chars, false, "UTF-16LE"),
        "UTF-32" => encode_utf32_with_bom_strict(chars),
        "UTF-32BE" => encode_utf32_fixed_strict(chars, true, "UTF-32BE"),
        "UTF-32LE" => encode_utf32_fixed_strict(chars, false, "UTF-32LE"),
        "windows-1252" => {
            validate_utf16_units(chars, "windows-1252")?;
            encode_cp1252(chars)
        }
        "windows-1251" => {
            validate_utf16_units(chars, "windows-1251")?;
            encode_cp1251(chars)
        }
        "KOI8-R" => {
            validate_utf16_units(chars, "KOI8-R")?;
            encode_koi8r(chars)
        }
        "ISO-8859-2" => {
            validate_utf16_units(chars, "ISO-8859-2")?;
            encode_iso_8859_2(chars)
        }
        "ISO-8859-15" => {
            validate_utf16_units(chars, "ISO-8859-15")?;
            encode_iso_8859_15(chars)
        }
        "IBM850" => {
            validate_utf16_units(chars, "IBM850")?;
            encode_ibm850(chars)
        }
        "IBM1047" => {
            validate_utf16_units(chars, "IBM1047")?;
            encode_ibm1047(chars)
        }
        "IBM500" => {
            validate_utf16_units(chars, "IBM500")?;
            encode_ibm500(chars)
        }
        other => {
            // Legacy / CJK multi-byte charsets via encoding_rs. `encode`
            // substitutes unmappable code points with an HTML numeric character
            // reference and flags `had_errors`; the strict (REPORT) contract
            // requires an error instead, so surface Unmappable when that fires
            // and only return the byte string when every unit mapped cleanly.
            if let Some(enc) = multibyte_encoding(other) {
                validate_utf16_units(chars, canonical_name_static(name))?;
                let s = String::from_utf16(chars).expect("surrogate pairing pre-validated");
                let (encoded, _, had_errors) = enc.encode(&s);
                if had_errors {
                    Err(CodingError {
                        offset: 0,
                        length: 0,
                        kind: CodingErrorKind::Unmappable,
                        charset: canonical_name_static(name),
                    })
                } else {
                    Ok(encoded.into_owned())
                }
            } else {
                Err(CodingError {
                    offset: 0,
                    length: 0,
                    kind: CodingErrorKind::UnsupportedCharset,
                    charset: canonical_name_static(name),
                })
            }
        }
    }
}

/// Lossy decode: on malformed input, substitute U+FFFD and continue.
pub fn decode_bytes_lossy(name: &str, bytes: &[u8]) -> Vec<u16> {
    // First try the strict path so the happy path avoids byte-by-byte
    // error handling cost; fall back to a tolerant decoder on error.
    if let Ok(v) = decode_bytes(name, bytes) {
        return v;
    }
    // Byte-oriented charsets never malform except for unsupported names --
    // with ONE exception, and the missing arm for it was a live bug until
    // 2026-08-05. `US-ASCII` is byte-oriented AND rejects every byte > 0x7F,
    // so `decode_bytes` returns `Err` and, with no arm here, control fell
    // through to the `other =>` catch-all below, whose 1:1 `b as u16` map IS
    // Latin-1. `new String(bytes, "US-ASCII")` therefore decoded 0xE9 as
    // U+00E9 where HotSpot gives U+FFFD -- high-bit bytes silently became
    // Latin-1 text instead of being reported unmappable.
    //
    // Every other byte-oriented arm in `decode_bytes` really is infallible
    // (each maps all 256 byte values through a table), so US-ASCII is the
    // whole of the exception the sentence above was missing.
    match name {
        // REPLACE action, per byte: `CharsetDecoder`'s default substitutes one
        // U+FFFD per malformed input byte, so the result keeps the input's
        // length. Checked against HotSpot rather than assumed --
        // `new String(new byte[]{(byte)0xE9,'a',(byte)0xFF}, "US-ASCII")` is
        // three chars, U+FFFD 'a' U+FFFD, not one.
        "US-ASCII" => bytes
            .iter()
            .map(|&b| if b > 0x7F { 0xFFFDu16 } else { u16::from(b) })
            .collect(),
        "UTF-8" => decode_utf8_lossy(bytes),
        "UTF-16" => decode_utf16_bom_lossy(bytes, true),
        "UTF-16BE" => decode_utf16_fixed_lossy(bytes, true),
        "UTF-16LE" => decode_utf16_fixed_lossy(bytes, false),
        "UTF-32" => decode_utf32_bom_lossy(bytes, true),
        "UTF-32BE" => decode_utf32_fixed_lossy(bytes, true),
        "UTF-32LE" => decode_utf32_fixed_lossy(bytes, false),
        // Unsupported charset name. The lossy signature is infallible, so we
        // cannot surface the `UnsupportedCharset` error here. Replacing every
        // byte with U+FFFD would be doubly wrong: it discards the input AND
        // misreports the *length* (a 1:1 byte->char map for an unknown name is
        // arbitrary). Instead fall back to ISO-8859-1 (Latin-1), the only
        // charset that maps every byte 0x00..=0xFF losslessly and identically.
        // This preserves the bytes' identity (round-trippable) rather than
        // fabricating replacement characters for valid data.
        other => {
            // Legacy / CJK multi-byte charsets via encoding_rs: tolerant decode
            // substitutes U+FFFD for malformed sequences (the REPLACE action).
            if let Some(enc) = multibyte_encoding(other) {
                let (cow, _had_errors) = enc.decode_without_bom_handling(bytes);
                cow.encode_utf16().collect()
            } else {
                bytes.iter().map(|&b| b as u16).collect()
            }
        }
    }
}

/// Lossy encode: on unmappable input, substitute `'?'` and continue.
pub fn encode_chars_lossy(name: &str, chars: &[u16]) -> Vec<u8> {
    match encode_chars(name, chars) {
        Ok(v) => v,
        // The strict encode failed on an unmappable code point. The REPLACE
        // action substitutes the charset's replacement byte (`'?'`) for each
        // unmappable unit while still encoding the mappable ones in the
        // *requested* charset. Re-encoding the whole input as UTF-8 (the old
        // catch-all) was wrong: a `windows-1252` sink would receive UTF-8
        // multibyte sequences for any supplementary character.
        Err(_) => match name {
            // Unicode charsets can represent every scalar value, so strict
            // failures here mean malformed surrogate units. Keep the bytes in
            // the requested charset while substituting U+FFFD for bad units.
            "UTF-8" => encode_utf8(chars),
            "UTF-16" => encode_utf16_with_bom_lossy(chars),
            "UTF-16BE" => encode_utf16_fixed_lossy(chars, true),
            "UTF-16LE" => encode_utf16_fixed_lossy(chars, false),
            "UTF-32" => encode_utf32_with_bom_lossy(chars),
            "UTF-32BE" => encode_utf32_fixed(chars, true),
            "UTF-32LE" => encode_utf32_fixed(chars, false),
            "US-ASCII" => chars
                .iter()
                .map(|&c| if c < 0x80 { c as u8 } else { REPLACEMENT_BYTE })
                .collect(),
            "ISO-8859-1" => chars
                .iter()
                .map(|&c| if c < 0x100 { c as u8 } else { REPLACEMENT_BYTE })
                .collect(),
            // Supported single-byte code pages: encode mappable units in the
            // target charset and substitute `'?'` for unmappable ones, rather
            // than dropping to UTF-8.
            "windows-1252" => encode_sb_lossy(chars, cp1252_rev()),
            "windows-1251" => encode_sb_lossy(chars, cp1251_rev()),
            "KOI8-R" => encode_sb_lossy(chars, koi8r_rev()),
            "ISO-8859-2" => encode_sb_lossy(chars, iso_8859_2_rev()),
            "ISO-8859-15" => encode_sb_lossy(chars, iso_8859_15_rev()),
            "IBM850" => encode_sb_lossy(chars, ibm850_rev()),
            "IBM1047" => encode_full_sb_lossy(chars, ibm1047_rev()),
            "IBM500" => encode_full_sb_lossy(chars, ibm500_rev()),
            // Legacy / CJK multi-byte charsets via encoding_rs, with `'?'`
            // substitution for unmappable units (the REPLACE action) — reached
            // only when the strict `encode_chars` above already reported an
            // unmappable unit, so most of the string is representable.
            //
            // Unsupported charset name: the lossy signature is infallible, so we
            // cannot surface `UnsupportedCharset`. Re-encoding as UTF-8 silently
            // produced bytes in the *wrong* encoding for the sink. Fall back to
            // ISO-8859-1 (the byte-identity charset) with `'?'` substitution:
            // representable units keep their byte value, the rest become `'?'`.
            // Still lossy, but never the wrong encoding.
            _ => {
                if let Some(enc) = multibyte_encoding(name) {
                    encode_multibyte_lossy(enc, chars)
                } else {
                    chars
                        .iter()
                        .map(|&c| if c < 0x100 { c as u8 } else { REPLACEMENT_BYTE })
                        .collect()
                }
            }
        },
    }
}

/// Lossy multi-byte encode: encode each code point in `chars` with the given
/// `encoding_rs` encoding, substituting [`REPLACEMENT_BYTE`] (`'?'`) for any
/// unmappable one. Encodes code-point-by-code-point so a single unmappable
/// char becomes exactly one `'?'` rather than an HTML numeric char reference.
/// Stateless CJK codecs (Shift_JIS, EUC-*, GBK, Big5) round-trip exactly this
/// way; only stateful ISO-2022-JP is approximated on the (untested) lossy path.
fn encode_multibyte_lossy(enc: &'static encoding_rs::Encoding, chars: &[u16]) -> Vec<u8> {
    let s = String::from_utf16_lossy(chars);
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let one = ch.encode_utf8(&mut buf);
        let (encoded, _, had_errors) = enc.encode(one);
        if had_errors {
            out.push(REPLACEMENT_BYTE);
        } else {
            out.extend_from_slice(&encoded);
        }
    }
    out
}

/// Lossy single-byte encode: encode each mappable code unit in the target
/// charset via its prebuilt reverse table, substituting [`REPLACEMENT_BYTE`]
/// (`'?'`) for any unmappable unit. Mirrors [`encode_sb`] but never errors.
fn encode_sb_lossy(chars: &[u16], rev: &[u8; 65536]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chars.len());
    for &c in chars {
        if c < 0x80 {
            out.push(c as u8);
        } else {
            let b = rev[c as usize];
            out.push(if b != 0 { b } else { REPLACEMENT_BYTE });
        }
    }
    out
}

/// Lossy encode for full single-byte tables whose 0x00..=0x7F byte range is
/// not ASCII (for example EBCDIC IBM1047). `rev` uses `u16::MAX` as the
/// unmapped sentinel so byte 0 can remain a valid target.
fn encode_full_sb_lossy(chars: &[u16], rev: &[u16; 65536]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chars.len());
    let replacement = rev[b'?' as usize];
    let replacement = if replacement != u16::MAX {
        replacement as u8
    } else {
        REPLACEMENT_BYTE
    };

    for &c in chars {
        let b = rev[c as usize];
        out.push(if b != u16::MAX { b as u8 } else { replacement });
    }
    out
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
            // `error_len() == None` means the input ended in the middle of a
            // valid-so-far multi-byte sequence (truncated trailing sequence) —
            // an incomplete, not malformed, encoding.
            length: e
                .error_len()
                .unwrap_or_else(|| bytes.len() - e.valid_up_to()),
            kind: if e.error_len().is_none() {
                CodingErrorKind::Incomplete
            } else {
                CodingErrorKind::Malformed
            },
            charset: "UTF-8",
        }),
    }
}

fn decode_utf8_lossy(bytes: &[u8]) -> Vec<u16> {
    String::from_utf8_lossy(bytes).encode_utf16().collect()
}

fn validate_utf16_units(chars: &[u16], charset: &'static str) -> Result<(), CodingError> {
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if (0xD800..=0xDBFF).contains(&c) {
            match chars.get(i + 1).copied() {
                Some(lo) if (0xDC00..=0xDFFF).contains(&lo) => {
                    i += 2;
                }
                None => {
                    return Err(CodingError {
                        offset: i,
                        length: 1,
                        kind: CodingErrorKind::Incomplete,
                        charset,
                    });
                }
                _ => {
                    return Err(CodingError {
                        offset: i,
                        length: 1,
                        kind: CodingErrorKind::Malformed,
                        charset,
                    });
                }
            }
        } else if (0xDC00..=0xDFFF).contains(&c) {
            return Err(CodingError {
                offset: i,
                length: 1,
                kind: CodingErrorKind::Malformed,
                charset,
            });
        } else {
            i += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JDK-faithful streaming UTF-8 decoder
//
// `String::from_utf8_lossy` follows the WHATWG/W3C "maximal subpart"
// substitution rule, but its replacement granularity at multi-byte-sequence
// *buffer boundaries* differs from `sun.nio.cs.UTF_8.Decoder` — which is what
// `java.nio.charset.CharsetDecoder` drives. Tomcat's `TestUtf8` feeds malformed
// input one byte at a time and asserts the exact number / placement of U+FFFD
// substitutions (53 cases). To match byte-for-byte we port the JDK's
// `decodeArrayLoop` + `malformedN` length accounting and run the
// `CharsetDecoder.decode` orchestrator (REPORT / REPLACE / IGNORE) over it.
// ---------------------------------------------------------------------------

/// Error action applied to a malformed / unmappable coding result, mirroring
/// `java.nio.charset.CodingErrorAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingAction {
    /// Surface the error to the caller (HotSpot default).
    Report,
    /// Substitute the replacement unit(s) and continue.
    Replace,
    /// Skip the erroneous input and continue.
    Ignore,
}

/// Terminal status of a [`utf8_decode`] call — the subset of
/// `java.nio.charset.CoderResult` states the orchestrator can hand back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Utf8DecodeStatus {
    /// All consumable input was processed; any *unconsumed* trailing bytes are
    /// an incomplete multi-byte sequence awaiting more input.
    Underflow,
    /// The output capacity was reached before all input was decoded.
    Overflow,
    /// A malformed sequence was hit while the action is REPORT.
    Malformed,
}

#[inline]
fn u8_is_not_continuation(b: u32) -> bool {
    (b & 0xC0) != 0x80
}
#[inline]
fn u8_is_malformed3(b1: u32, b2: u32, b3: u32) -> bool {
    (b1 == 0xE0 && (b2 & 0xE0) == 0x80) || u8_is_not_continuation(b2) || u8_is_not_continuation(b3)
}
#[inline]
fn u8_is_malformed3_2(b1: u32, b2: u32) -> bool {
    (b1 == 0xE0 && (b2 & 0xE0) == 0x80) || u8_is_not_continuation(b2)
}
#[inline]
fn u8_is_malformed4(b2: u32, b3: u32, b4: u32) -> bool {
    u8_is_not_continuation(b2) || u8_is_not_continuation(b3) || u8_is_not_continuation(b4)
}
#[inline]
fn u8_is_malformed4_2(b1: u32, b2: u32) -> bool {
    (b1 == 0xF0 && (b2 & 0xF0) == 0x80)
        || (b1 == 0xF4 && (b2 & 0xF0) != 0x80)
        || u8_is_not_continuation(b2)
}
#[inline]
fn u8_is_malformed4_3(b3: u32) -> bool {
    u8_is_not_continuation(b3)
}

/// Length (in bytes) of the maximal ill-formed subpart of a 3-byte sequence,
/// matching `UTF_8.malformedN(src, 3)`.
#[inline]
fn u8_malformed_n3(b1: u32, b2: u32) -> usize {
    if (b1 == 0xE0 && (b2 & 0xE0) == 0x80) || u8_is_not_continuation(b2) {
        1
    } else {
        2
    }
}
/// Length (in bytes) of the maximal ill-formed subpart of a 4-byte sequence,
/// matching `UTF_8.malformedN(src, 4)`.
#[inline]
fn u8_malformed_n4(b1: u32, b2: u32, b3: u32) -> usize {
    if b1 > 0xF4
        || (b1 == 0xF0 && (b2 < 0x90 || b2 > 0xBF))
        || (b1 == 0xF4 && (b2 & 0xF0) != 0x80)
        || u8_is_not_continuation(b2)
    {
        1
    } else if u8_is_not_continuation(b3) {
        2
    } else {
        3
    }
}

/// Outcome of one [`utf8_decode_loop`] pass (one `decodeArrayLoop` call).
enum Utf8LoopEnd {
    /// Ran out of input (any leftover is an incomplete trailing sequence).
    Underflow,
    /// Output capacity reached before the next code point could be emitted.
    Overflow,
    /// Malformed sequence of the given byte length at the current position.
    Malformed(usize),
}

/// Port of `sun.nio.cs.UTF_8.Decoder.decodeArrayLoop`: decode valid code
/// points from the front of `src` into `out` (UTF-16 units), stopping at the
/// first malformed sequence, output overflow (`out.len() == out_cap`), or
/// end of input. Returns `(bytes_consumed, end)`.
fn utf8_decode_loop(src: &[u8], out: &mut Vec<u16>, out_cap: usize) -> (usize, Utf8LoopEnd) {
    let sl = src.len();
    let mut sp = 0usize;
    while sp < sl {
        let b1 = src[sp] as u32;
        if b1 < 0x80 {
            // 1 byte: 0xxxxxxx
            if out.len() >= out_cap {
                return (sp, Utf8LoopEnd::Overflow);
            }
            out.push(b1 as u16);
            sp += 1;
        } else if (0xC2..=0xDF).contains(&b1) {
            // 2 bytes: 110xxxxx 10xxxxxx (C0/C1 are overlong → fall to `else`)
            if sl - sp < 2 {
                return (sp, Utf8LoopEnd::Underflow);
            }
            if out.len() >= out_cap {
                return (sp, Utf8LoopEnd::Overflow);
            }
            let b2 = src[sp + 1] as u32;
            if u8_is_not_continuation(b2) {
                return (sp, Utf8LoopEnd::Malformed(1));
            }
            out.push((((b1 & 0x1F) << 6) | (b2 & 0x3F)) as u16);
            sp += 2;
        } else if (0xE0..=0xEF).contains(&b1) {
            // 3 bytes: 1110xxxx 10xxxxxx 10xxxxxx
            let rem = sl - sp;
            if rem < 3 {
                if rem > 1 && u8_is_malformed3_2(b1, src[sp + 1] as u32) {
                    return (sp, Utf8LoopEnd::Malformed(1));
                }
                return (sp, Utf8LoopEnd::Underflow);
            }
            if out.len() >= out_cap {
                return (sp, Utf8LoopEnd::Overflow);
            }
            let b2 = src[sp + 1] as u32;
            let b3 = src[sp + 2] as u32;
            if u8_is_malformed3(b1, b2, b3) {
                return (sp, Utf8LoopEnd::Malformed(u8_malformed_n3(b1, b2)));
            }
            let cp = ((b1 & 0x0F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F);
            if (0xD800..=0xDFFF).contains(&cp) {
                // Surrogate code point encoded as 3 bytes (CESU-8) → malformed.
                return (sp, Utf8LoopEnd::Malformed(3));
            }
            out.push(cp as u16);
            sp += 3;
        } else if (0xF0..=0xF7).contains(&b1) {
            // 4 bytes: 11110xxx 10xxxxxx 10xxxxxx 10xxxxxx
            let rem = sl - sp;
            if rem < 4 || out_cap - out.len() < 2 {
                if rem < 4 {
                    if rem > 1 && u8_is_malformed4_2(b1, src[sp + 1] as u32) {
                        return (sp, Utf8LoopEnd::Malformed(1));
                    }
                    if rem > 2 && u8_is_malformed4_3(src[sp + 2] as u32) {
                        return (sp, Utf8LoopEnd::Malformed(2));
                    }
                    return (sp, Utf8LoopEnd::Underflow);
                }
                return (sp, Utf8LoopEnd::Overflow);
            }
            let b2 = src[sp + 1] as u32;
            let b3 = src[sp + 2] as u32;
            let b4 = src[sp + 3] as u32;
            let cp = ((b1 & 0x07) << 18) | ((b2 & 0x3F) << 12) | ((b3 & 0x3F) << 6) | (b4 & 0x3F);
            if u8_is_malformed4(b2, b3, b4) || !(0x10000..=0x10FFFF).contains(&cp) {
                return (sp, Utf8LoopEnd::Malformed(u8_malformed_n4(b1, b2, b3)));
            }
            let adj = cp - 0x10000;
            out.push((0xD800 + (adj >> 10)) as u16);
            out.push((0xDC00 + (adj & 0x3FF)) as u16);
            sp += 4;
        } else {
            // 0x80..=0xBF lone continuation, 0xC0, 0xC1, 0xF8..=0xFF.
            return (sp, Utf8LoopEnd::Malformed(1));
        }
    }
    (sp, Utf8LoopEnd::Underflow)
}

/// JDK-faithful UTF-8 decode of `src`, applying `action` (the decoder's
/// `malformedInputAction`) to malformed sequences and honoring `end_of_input`
/// exactly like the `java.nio.charset.CharsetDecoder.decode(in, out, eoi)`
/// orchestrator. Decoded UTF-16 units are appended to `out`, bounded by
/// `out_cap` (the destination's remaining capacity). Returns
/// `(bytes_consumed, status)` — the caller advances the input buffer by
/// `bytes_consumed` and maps `status` to a `CoderResult`.
pub fn utf8_decode(
    src: &[u8],
    end_of_input: bool,
    action: CodingAction,
    out: &mut Vec<u16>,
    out_cap: usize,
) -> (usize, Utf8DecodeStatus) {
    let sl = src.len();
    let mut sp = 0usize;
    loop {
        let (np, end) = utf8_decode_loop(&src[sp..], out, out_cap);
        sp += np;
        match end {
            Utf8LoopEnd::Overflow => return (sp, Utf8DecodeStatus::Overflow),
            Utf8LoopEnd::Underflow => {
                if end_of_input && sp < sl {
                    // A trailing incomplete sequence at end-of-input is treated
                    // as `malformedForLength(remaining)` by the orchestrator.
                    match action {
                        CodingAction::Report => return (sp, Utf8DecodeStatus::Malformed),
                        CodingAction::Replace => {
                            if out.len() >= out_cap {
                                return (sp, Utf8DecodeStatus::Overflow);
                            }
                            out.push(REPLACEMENT_CHAR);
                            sp = sl;
                        }
                        CodingAction::Ignore => sp = sl,
                    }
                } else {
                    return (sp, Utf8DecodeStatus::Underflow);
                }
            }
            Utf8LoopEnd::Malformed(len) => match action {
                CodingAction::Report => return (sp, Utf8DecodeStatus::Malformed),
                CodingAction::Replace => {
                    if out.len() >= out_cap {
                        return (sp, Utf8DecodeStatus::Overflow);
                    }
                    out.push(REPLACEMENT_CHAR);
                    sp += len;
                }
                CodingAction::Ignore => sp += len,
            },
        }
    }
}

/// Lossy UTF-8 encode: lone surrogates become U+FFFD (replacement char),
/// matching `CodingErrorAction.REPLACE`. Used by the `*_lossy` callers.
fn encode_utf8(chars: &[u16]) -> Vec<u8> {
    let s = String::from_utf16_lossy(chars);
    s.into_bytes()
}

/// Strict UTF-8 encode: REPORTs a malformed-input error on the first lone
/// (unpaired) surrogate rather than substituting U+FFFD.
///
/// FIX (review MEDIUM, charset.rs ~120/~264): `String::from_utf16_lossy`
/// silently replaced lone surrogates with U+FFFD even on the strict
/// (`encode_chars`) path, masking malformed input. HotSpot's default REPORT
/// action surfaces a `MalformedInputException` for an unpaired surrogate, so
/// the strict path must return a `CodingError::Malformed` instead. A high
/// surrogate (U+D800..=U+DBFF) must be immediately followed by a low surrogate
/// (U+DC00..=U+DFFF); any other arrangement is malformed.
fn encode_utf8_strict(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    validate_utf16_units(chars, "UTF-8")?;
    // All surrogates are well-paired BMP/supplementary code points: a lossless
    // `from_utf16` conversion is now guaranteed to succeed.
    Ok(String::from_utf16(chars)
        .expect("surrogate pairing pre-validated")
        .into_bytes())
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
    let mut out = Vec::with_capacity(n / 2 + if bytes.len() != n { 1 } else { 0 });
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
    if bytes.len() != n {
        out.push(REPLACEMENT_CHAR);
    }
    out
}

fn encode_utf16_fixed_strict(
    chars: &[u16],
    big_endian: bool,
    charset: &'static str,
) -> Result<Vec<u8>, CodingError> {
    validate_utf16_units(chars, charset)?;
    Ok(encode_utf16_fixed(chars, big_endian))
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

fn encode_utf16_fixed_lossy(chars: &[u16], big_endian: bool) -> Vec<u8> {
    let lossy: Vec<u16> = String::from_utf16_lossy(chars).encode_utf16().collect();
    encode_utf16_fixed(&lossy, big_endian)
}

fn encode_utf16_with_bom_strict(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    validate_utf16_units(chars, "UTF-16")?;
    Ok(encode_utf16_with_bom(chars))
}

fn encode_utf16_with_bom(chars: &[u16]) -> Vec<u8> {
    // FIX (defaultlogbackconfigurationtests-empty-utf16-bom): real JDK's
    // `"".getBytes(StandardCharsets.UTF_16)` returns an empty array, not a
    // lone BOM — `UnicodeEncoder`'s BOM write lives inside its per-character
    // encode loop, which never runs for zero input characters. This helper
    // (one-shot `String.getBytes(Charset)`/`encode_chars[_lossy]`, NOT the
    // stateful `CharsetEncoder.encode()` session path in
    // `native-builtins/src/charset.rs`, which tracks BOM-written state
    // separately) used to push the BOM unconditionally, so e.g. Logback's
    // `LayoutWrappingEncoder.headerBytes()` — which always calls
    // `convertToBytes("")` for an unconfigured header, even when there is
    // nothing to write — leaked a stray BOM directly into a UTF-16-charset
    // `ConsoleAppender`'s target stream (real `System.out` by default) the
    // moment the appender started, before any real log event ever ran.
    if chars.is_empty() {
        return Vec::new();
    }
    // HotSpot writes a BE BOM for UTF-16 output.
    let mut out = Vec::with_capacity(2 + chars.len() * 2);
    out.push(0xFE);
    out.push(0xFF);
    out.extend_from_slice(&encode_utf16_fixed(chars, true));
    out
}

fn encode_utf16_with_bom_lossy(chars: &[u16]) -> Vec<u8> {
    let lossy: Vec<u16> = String::from_utf16_lossy(chars).encode_utf16().collect();
    encode_utf16_with_bom(&lossy)
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
    let mut out = Vec::with_capacity(n / 2 + if bytes.len() != n { 1 } else { 0 });
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
    if bytes.len() != n {
        out.push(REPLACEMENT_CHAR);
    }
    out
}

fn encode_utf32_fixed_strict(
    chars: &[u16],
    big_endian: bool,
    charset: &'static str,
) -> Result<Vec<u8>, CodingError> {
    validate_utf16_units(chars, charset)?;
    Ok(encode_utf32_fixed(chars, big_endian))
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

fn encode_utf32_with_bom_strict(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    validate_utf16_units(chars, "UTF-32")?;
    Ok(encode_utf32_with_bom(chars))
}

fn encode_utf32_with_bom(chars: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + chars.len() * 4);
    out.extend_from_slice(&[0x00, 0x00, 0xFE, 0xFF]);
    out.extend_from_slice(&encode_utf32_fixed(chars, true));
    out
}

fn encode_utf32_with_bom_lossy(chars: &[u16]) -> Vec<u8> {
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
    (CP1252_HIGH) => {
        sb_rev_cell!(CP1252_REV_CELL, cp1252_rev, CP1252_HIGH);
    };
    (CP1251_HIGH) => {
        sb_rev_cell!(CP1251_REV_CELL, cp1251_rev, CP1251_HIGH);
    };
    (KOI8R_HIGH) => {
        sb_rev_cell!(KOI8R_REV_CELL, koi8r_rev, KOI8R_HIGH);
    };
    (ISO_8859_2_HIGH) => {
        sb_rev_cell!(ISO_8859_2_REV_CELL, iso_8859_2_rev, ISO_8859_2_HIGH);
    };
    (ISO_8859_15_HIGH) => {
        sb_rev_cell!(ISO_8859_15_REV_CELL, iso_8859_15_rev, ISO_8859_15_HIGH);
    };
    (IBM850_HIGH) => {
        sb_rev_cell!(IBM850_REV_CELL, ibm850_rev, IBM850_HIGH);
    };
}

macro_rules! sb_rev_cell {
    ($cell:ident, $accessor:ident, $fwd:ident) => {
        static $cell: std::sync::OnceLock<Box<[u8; 65536]>> = std::sync::OnceLock::new();
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

/// Build a reverse table for full 256-byte code pages. Unlike [`build_sb_rev`],
/// this cannot use `0` as the sentinel: EBCDIC maps U+0000 to byte 0x00.
fn build_full_sb_rev(table: &[u16; 256]) -> Box<[u16; 65536]> {
    let mut rev = Box::new([u16::MAX; 65536]);
    for (byte, &u) in table.iter().enumerate().rev() {
        if u != 0xFFFD {
            rev[u as usize] = byte as u16;
        }
    }
    rev
}

static IBM1047_REV_CELL: std::sync::OnceLock<Box<[u16; 65536]>> = std::sync::OnceLock::new();
fn ibm1047_rev() -> &'static [u16; 65536] {
    IBM1047_REV_CELL.get_or_init(|| build_full_sb_rev(&IBM1047_TO_U16))
}

static IBM500_REV_CELL: std::sync::OnceLock<Box<[u16; 65536]>> = std::sync::OnceLock::new();
fn ibm500_rev() -> &'static [u16; 65536] {
    IBM500_REV_CELL.get_or_init(|| build_full_sb_rev(&IBM500_TO_U16))
}

sb_table!(
    CP1252_HIGH,
    [
        0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
        0x2039, 0x0152, 0xFFFD, 0x017D, 0xFFFD, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022,
        0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0xFFFD, 0x017E, 0x0178, 0x00A0,
        0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9, 0x00AA, 0x00AB,
        0x00AC, 0x00AD, 0x00AE, 0x00AF, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6,
        0x00B7, 0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF, 0x00C0, 0x00C1,
        0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC,
        0x00CD, 0x00CE, 0x00CF, 0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7,
        0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF, 0x00E0, 0x00E1, 0x00E2,
        0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED,
        0x00EE, 0x00EF, 0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7, 0x00F8,
        0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD, 0x00FE, 0x00FF,
    ]
);

sb_table!(
    CP1251_HIGH,
    [
        0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409,
        0x2039, 0x040A, 0x040C, 0x040B, 0x040F, 0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022,
        0x2013, 0x2014, 0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F, 0x00A0,
        0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB,
        0x00AC, 0x00AD, 0x00AE, 0x0407, 0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6,
        0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457, 0x0410, 0x0411,
        0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C,
        0x041D, 0x041E, 0x041F, 0x0420, 0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427,
        0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F, 0x0430, 0x0431, 0x0432,
        0x0433, 0x0434, 0x0435, 0x0436, 0x0437, 0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D,
        0x043E, 0x043F, 0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447, 0x0448,
        0x0449, 0x044A, 0x044B, 0x044C, 0x044D, 0x044E, 0x044F,
    ]
);

sb_table!(
    KOI8R_HIGH,
    [
        0x2500, 0x2502, 0x250C, 0x2510, 0x2514, 0x2518, 0x251C, 0x2524, 0x252C, 0x2534, 0x253C,
        0x2580, 0x2584, 0x2588, 0x258C, 0x2590, 0x2591, 0x2592, 0x2593, 0x2320, 0x25A0, 0x2219,
        0x221A, 0x2248, 0x2264, 0x2265, 0x00A0, 0x2321, 0x00B0, 0x00B2, 0x00B7, 0x00F7, 0x2550,
        0x2551, 0x2552, 0x0451, 0x2553, 0x2554, 0x2555, 0x2556, 0x2557, 0x2558, 0x2559, 0x255A,
        0x255B, 0x255C, 0x255D, 0x255E, 0x255F, 0x2560, 0x2561, 0x0401, 0x2562, 0x2563, 0x2564,
        0x2565, 0x2566, 0x2567, 0x2568, 0x2569, 0x256A, 0x256B, 0x256C, 0x00A9, 0x044E, 0x0430,
        0x0431, 0x0446, 0x0434, 0x0435, 0x0444, 0x0433, 0x0445, 0x0438, 0x0439, 0x043A, 0x043B,
        0x043C, 0x043D, 0x043E, 0x043F, 0x044F, 0x0440, 0x0441, 0x0442, 0x0443, 0x0436, 0x0432,
        0x044C, 0x044B, 0x0437, 0x0448, 0x044D, 0x0449, 0x0447, 0x044A, 0x042E, 0x0410, 0x0411,
        0x0426, 0x0414, 0x0415, 0x0424, 0x0413, 0x0425, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C,
        0x041D, 0x041E, 0x041F, 0x042F, 0x0420, 0x0421, 0x0422, 0x0423, 0x0416, 0x0412, 0x042C,
        0x042B, 0x0417, 0x0428, 0x042D, 0x0429, 0x0427, 0x042A,
    ]
);

sb_table!(
    ISO_8859_2_HIGH,
    [
        0x0080, 0x0081, 0x0082, 0x0083, 0x0084, 0x0085, 0x0086, 0x0087, 0x0088, 0x0089, 0x008A,
        0x008B, 0x008C, 0x008D, 0x008E, 0x008F, 0x0090, 0x0091, 0x0092, 0x0093, 0x0094, 0x0095,
        0x0096, 0x0097, 0x0098, 0x0099, 0x009A, 0x009B, 0x009C, 0x009D, 0x009E, 0x009F, 0x00A0,
        0x0104, 0x02D8, 0x0141, 0x00A4, 0x013D, 0x015A, 0x00A7, 0x00A8, 0x0160, 0x015E, 0x0164,
        0x0179, 0x00AD, 0x017D, 0x017B, 0x00B0, 0x0105, 0x02DB, 0x0142, 0x00B4, 0x013E, 0x015B,
        0x02C7, 0x00B8, 0x0161, 0x015F, 0x0165, 0x017A, 0x02DD, 0x017E, 0x017C, 0x0154, 0x00C1,
        0x00C2, 0x0102, 0x00C4, 0x0139, 0x0106, 0x00C7, 0x010C, 0x00C9, 0x0118, 0x00CB, 0x011A,
        0x00CD, 0x00CE, 0x010E, 0x0110, 0x0143, 0x0147, 0x00D3, 0x00D4, 0x0150, 0x00D6, 0x00D7,
        0x0158, 0x016E, 0x00DA, 0x0170, 0x00DC, 0x00DD, 0x0162, 0x00DF, 0x0155, 0x00E1, 0x00E2,
        0x0103, 0x00E4, 0x013A, 0x0107, 0x00E7, 0x010D, 0x00E9, 0x0119, 0x00EB, 0x011B, 0x00ED,
        0x00EE, 0x010F, 0x0111, 0x0144, 0x0148, 0x00F3, 0x00F4, 0x0151, 0x00F6, 0x00F7, 0x0159,
        0x016F, 0x00FA, 0x0171, 0x00FC, 0x00FD, 0x0163, 0x02D9,
    ]
);

sb_table!(
    ISO_8859_15_HIGH,
    [
        0x0080, 0x0081, 0x0082, 0x0083, 0x0084, 0x0085, 0x0086, 0x0087, 0x0088, 0x0089, 0x008A,
        0x008B, 0x008C, 0x008D, 0x008E, 0x008F, 0x0090, 0x0091, 0x0092, 0x0093, 0x0094, 0x0095,
        0x0096, 0x0097, 0x0098, 0x0099, 0x009A, 0x009B, 0x009C, 0x009D, 0x009E, 0x009F, 0x00A0,
        0x00A1, 0x00A2, 0x00A3, 0x20AC, 0x00A5, 0x0160, 0x00A7, 0x0161, 0x00A9, 0x00AA, 0x00AB,
        0x00AC, 0x00AD, 0x00AE, 0x00AF, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x017D, 0x00B5, 0x00B6,
        0x00B7, 0x017E, 0x00B9, 0x00BA, 0x00BB, 0x0152, 0x0153, 0x0178, 0x00BF, 0x00C0, 0x00C1,
        0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC,
        0x00CD, 0x00CE, 0x00CF, 0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7,
        0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF, 0x00E0, 0x00E1, 0x00E2,
        0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED,
        0x00EE, 0x00EF, 0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7, 0x00F8,
        0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD, 0x00FE, 0x00FF,
    ]
);

// IBM850 / CP850 (DOS Latin-1 "Multilingual"). Bytes 0x00..=0x7F are ASCII;
// only the 0x80..=0xFF high half is tabulated here, in byte order, per the
// standard Unicode mapping (box-drawing glyphs map to U+25xx/U+2500-range).
sb_table!(
    IBM850_HIGH,
    [
        0x00C7, 0x00FC, 0x00E9, 0x00E2, 0x00E4, 0x00E0, 0x00E5, 0x00E7, 0x00EA, 0x00EB, 0x00E8,
        0x00EF, 0x00EE, 0x00EC, 0x00C4, 0x00C5, 0x00C9, 0x00E6, 0x00C6, 0x00F4, 0x00F6, 0x00F2,
        0x00FB, 0x00F9, 0x00FF, 0x00D6, 0x00DC, 0x00F8, 0x00A3, 0x00D8, 0x00D7, 0x0192, 0x00E1,
        0x00ED, 0x00F3, 0x00FA, 0x00F1, 0x00D1, 0x00AA, 0x00BA, 0x00BF, 0x00AE, 0x00AC, 0x00BD,
        0x00BC, 0x00A1, 0x00AB, 0x00BB, 0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x00C1, 0x00C2,
        0x00C0, 0x00A9, 0x2563, 0x2551, 0x2557, 0x255D, 0x00A2, 0x00A5, 0x2510, 0x2514, 0x2534,
        0x252C, 0x251C, 0x2500, 0x253C, 0x00E3, 0x00C3, 0x255A, 0x2554, 0x2569, 0x2566, 0x2560,
        0x2550, 0x256C, 0x00A4, 0x00F0, 0x00D0, 0x00CA, 0x00CB, 0x00C8, 0x0131, 0x00CD, 0x00CE,
        0x00CF, 0x2518, 0x250C, 0x2588, 0x2584, 0x00A6, 0x00CC, 0x2580, 0x00D3, 0x00DF, 0x00D4,
        0x00D2, 0x00F5, 0x00D5, 0x00B5, 0x00FE, 0x00DE, 0x00DA, 0x00DB, 0x00D9, 0x00FD, 0x00DD,
        0x00AF, 0x00B4, 0x00AD, 0x00B1, 0x2017, 0x00BE, 0x00B6, 0x00A7, 0x00F7, 0x00B8, 0x00B0,
        0x00A8, 0x00B7, 0x00B9, 0x00B3, 0x00B2, 0x25A0, 0x00A0,
    ]
);

// IBM1047 / Cp1047 (EBCDIC Latin-1/Open Systems). The low byte range is not
// ASCII, so this table covers all 256 byte values in byte order.
const IBM1047_TO_U16: [u16; 256] = [
    0x0000, 0x0001, 0x0002, 0x0003, 0x009C, 0x0009, 0x0086, 0x007F, 0x0097, 0x008D, 0x008E, 0x000B,
    0x000C, 0x000D, 0x000E, 0x000F, 0x0010, 0x0011, 0x0012, 0x0013, 0x009D, 0x0085, 0x0008, 0x0087,
    0x0018, 0x0019, 0x0092, 0x008F, 0x001C, 0x001D, 0x001E, 0x001F, 0x0080, 0x0081, 0x0082, 0x0083,
    0x0084, 0x000A, 0x0017, 0x001B, 0x0088, 0x0089, 0x008A, 0x008B, 0x008C, 0x0005, 0x0006, 0x0007,
    0x0090, 0x0091, 0x0016, 0x0093, 0x0094, 0x0095, 0x0096, 0x0004, 0x0098, 0x0099, 0x009A, 0x009B,
    0x0014, 0x0015, 0x009E, 0x001A, 0x0020, 0x00A0, 0x00E2, 0x00E4, 0x00E0, 0x00E1, 0x00E3, 0x00E5,
    0x00E7, 0x00F1, 0x00A2, 0x002E, 0x003C, 0x0028, 0x002B, 0x007C, 0x0026, 0x00E9, 0x00EA, 0x00EB,
    0x00E8, 0x00ED, 0x00EE, 0x00EF, 0x00EC, 0x00DF, 0x0021, 0x0024, 0x002A, 0x0029, 0x003B, 0x005E,
    0x002D, 0x002F, 0x00C2, 0x00C4, 0x00C0, 0x00C1, 0x00C3, 0x00C5, 0x00C7, 0x00D1, 0x00A6, 0x002C,
    0x0025, 0x005F, 0x003E, 0x003F, 0x00F8, 0x00C9, 0x00CA, 0x00CB, 0x00C8, 0x00CD, 0x00CE, 0x00CF,
    0x00CC, 0x0060, 0x003A, 0x0023, 0x0040, 0x0027, 0x003D, 0x0022, 0x00D8, 0x0061, 0x0062, 0x0063,
    0x0064, 0x0065, 0x0066, 0x0067, 0x0068, 0x0069, 0x00AB, 0x00BB, 0x00F0, 0x00FD, 0x00FE, 0x00B1,
    0x00B0, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F, 0x0070, 0x0071, 0x0072, 0x00AA, 0x00BA,
    0x00E6, 0x00B8, 0x00C6, 0x00A4, 0x00B5, 0x007E, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077, 0x0078,
    0x0079, 0x007A, 0x00A1, 0x00BF, 0x00D0, 0x005B, 0x00DE, 0x00AE, 0x00AC, 0x00A3, 0x00A5, 0x00B7,
    0x00A9, 0x00A7, 0x00B6, 0x00BC, 0x00BD, 0x00BE, 0x00DD, 0x00A8, 0x00AF, 0x005D, 0x00B4, 0x00D7,
    0x007B, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x00AD, 0x00F4,
    0x00F6, 0x00F2, 0x00F3, 0x00F5, 0x007D, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F, 0x0050,
    0x0051, 0x0052, 0x00B9, 0x00FB, 0x00FC, 0x00F9, 0x00FA, 0x00FF, 0x005C, 0x00F7, 0x0053, 0x0054,
    0x0055, 0x0056, 0x0057, 0x0058, 0x0059, 0x005A, 0x00B2, 0x00D4, 0x00D6, 0x00D2, 0x00D3, 0x00D5,
    0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037, 0x0038, 0x0039, 0x00B3, 0x00DB,
    0x00DC, 0x00D9, 0x00DA, 0x009F,
];

// IBM500 / CP500 (EBCDIC 500 International). Like IBM1047, this is not
// ASCII-compatible so all 256 byte values are tabulated. Byte-for-byte
// identical to real JDK25's `sun.nio.cs.ext.IBM500` (verified by dumping
// `new String(allBytes, Charset.forName("cp500"))` on the HotSpot
// baseline) — see bug-h2-charset-cp500-unsupported.md.
const IBM500_TO_U16: [u16; 256] = [
    0x0000, 0x0001, 0x0002, 0x0003, 0x009C, 0x0009, 0x0086, 0x007F, 0x0097, 0x008D, 0x008E, 0x000B,
    0x000C, 0x000D, 0x000E, 0x000F, 0x0010, 0x0011, 0x0012, 0x0013, 0x009D, 0x000A, 0x0008, 0x0087,
    0x0018, 0x0019, 0x0092, 0x008F, 0x001C, 0x001D, 0x001E, 0x001F, 0x0080, 0x0081, 0x0082, 0x0083,
    0x0084, 0x000A, 0x0017, 0x001B, 0x0088, 0x0089, 0x008A, 0x008B, 0x008C, 0x0005, 0x0006, 0x0007,
    0x0090, 0x0091, 0x0016, 0x0093, 0x0094, 0x0095, 0x0096, 0x0004, 0x0098, 0x0099, 0x009A, 0x009B,
    0x0014, 0x0015, 0x009E, 0x001A, 0x0020, 0x00A0, 0x00E2, 0x00E4, 0x00E0, 0x00E1, 0x00E3, 0x00E5,
    0x00E7, 0x00F1, 0x005B, 0x002E, 0x003C, 0x0028, 0x002B, 0x0021, 0x0026, 0x00E9, 0x00EA, 0x00EB,
    0x00E8, 0x00ED, 0x00EE, 0x00EF, 0x00EC, 0x00DF, 0x005D, 0x0024, 0x002A, 0x0029, 0x003B, 0x005E,
    0x002D, 0x002F, 0x00C2, 0x00C4, 0x00C0, 0x00C1, 0x00C3, 0x00C5, 0x00C7, 0x00D1, 0x00A6, 0x002C,
    0x0025, 0x005F, 0x003E, 0x003F, 0x00F8, 0x00C9, 0x00CA, 0x00CB, 0x00C8, 0x00CD, 0x00CE, 0x00CF,
    0x00CC, 0x0060, 0x003A, 0x0023, 0x0040, 0x0027, 0x003D, 0x0022, 0x00D8, 0x0061, 0x0062, 0x0063,
    0x0064, 0x0065, 0x0066, 0x0067, 0x0068, 0x0069, 0x00AB, 0x00BB, 0x00F0, 0x00FD, 0x00FE, 0x00B1,
    0x00B0, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F, 0x0070, 0x0071, 0x0072, 0x00AA, 0x00BA,
    0x00E6, 0x00B8, 0x00C6, 0x00A4, 0x00B5, 0x007E, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077, 0x0078,
    0x0079, 0x007A, 0x00A1, 0x00BF, 0x00D0, 0x00DD, 0x00DE, 0x00AE, 0x00A2, 0x00A3, 0x00A5, 0x00B7,
    0x00A9, 0x00A7, 0x00B6, 0x00BC, 0x00BD, 0x00BE, 0x00AC, 0x007C, 0x00AF, 0x00A8, 0x00B4, 0x00D7,
    0x007B, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x00AD, 0x00F4,
    0x00F6, 0x00F2, 0x00F3, 0x00F5, 0x007D, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F, 0x0050,
    0x0051, 0x0052, 0x00B9, 0x00FB, 0x00FC, 0x00F9, 0x00FA, 0x00FF, 0x005C, 0x00F7, 0x0053, 0x0054,
    0x0055, 0x0056, 0x0057, 0x0058, 0x0059, 0x005A, 0x00B2, 0x00D4, 0x00D6, 0x00D2, 0x00D3, 0x00D5,
    0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037, 0x0038, 0x0039, 0x00B3, 0x00DB,
    0x00DC, 0x00D9, 0x00DA, 0x009F,
];

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
fn ibm850_to_u16(b: u8) -> u16 {
    sb_to_u16(b, &IBM850_HIGH)
}
fn ibm1047_to_u16(b: u8) -> u16 {
    IBM1047_TO_U16[b as usize]
}
fn ibm500_to_u16(b: u8) -> u16 {
    IBM500_TO_U16[b as usize]
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

fn encode_full_sb(
    chars: &[u16],
    rev: &[u16; 65536],
    charset: &'static str,
) -> Result<Vec<u8>, CodingError> {
    let mut out = Vec::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        let b = rev[c as usize];
        if b != u16::MAX {
            out.push(b as u8);
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
fn encode_ibm850(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_sb(chars, ibm850_rev(), "IBM850")
}
fn encode_ibm1047(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_full_sb(chars, ibm1047_rev(), "IBM1047")
}
fn encode_ibm500(chars: &[u16]) -> Result<Vec<u8>, CodingError> {
    encode_full_sb(chars, ibm500_rev(), "IBM500")
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
        "IBM850" => "IBM850",
        "IBM1047" => "IBM1047",
        "IBM500" => "IBM500",
        "Shift_JIS" => "Shift_JIS",
        "EUC-JP" => "EUC-JP",
        "ISO-2022-JP" => "ISO-2022-JP",
        "Big5" => "Big5",
        "EUC-KR" => "EUC-KR",
        "GBK" => "GBK",
        "GB2312" => "GB2312",
        "GB18030" => "GB18030",
        "windows-1250" => "windows-1250",
        _ => "(unknown)",
    }
}

/// Average bytes-per-char heuristic matching the value HotSpot's
/// `CharsetEncoder` reports via `averageBytesPerChar()`.
pub fn average_bytes_per_char(name: &str) -> f32 {
    match name {
        "UTF-8" => 1.1,
        "US-ASCII" | "ISO-8859-1" | "ISO-8859-2" | "ISO-8859-15" | "windows-1252"
        | "windows-1251" | "KOI8-R" | "IBM850" | "IBM1047" => 1.0,
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
        | "windows-1251" | "KOI8-R" | "IBM850" | "IBM1047" => 1.0,
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
mod historical_name_tests {
    use super::*;

    /// Every canonical name `canonical_charset_name` can produce, paired with
    /// what Temurin 25.0.3 reports from
    /// `new InputStreamReader(in, cs).getEncoding()` (identical to
    /// `OutputStreamWriter`'s — both go through `HistoricallyNamedCharset`).
    ///
    /// Frozen as a transcript rather than a rule, because there is no rule: the
    /// spellings are historic, and `GB2312 -> EUC_CN` cannot be derived from
    /// anything. Regenerate with `probes/ReaderWriterLayoutProbe`'s method on
    /// the host JDK if this ever has to move.
    const HOST_JDK: &[(&str, &str)] = &[
        ("UTF-8", "UTF8"),
        ("UTF-16", "UTF-16"),
        ("UTF-16BE", "UnicodeBigUnmarked"),
        ("UTF-16LE", "UnicodeLittleUnmarked"),
        ("UTF-32", "UTF-32"),
        ("UTF-32BE", "UTF-32BE"),
        ("UTF-32LE", "UTF-32LE"),
        ("US-ASCII", "ASCII"),
        ("ISO-8859-1", "ISO8859_1"),
        ("ISO-8859-2", "ISO8859_2"),
        ("ISO-8859-3", "ISO8859_3"),
        ("ISO-8859-4", "ISO8859_4"),
        ("ISO-8859-5", "ISO8859_5"),
        ("ISO-8859-15", "ISO8859_15"),
        ("Shift_JIS", "SJIS"),
        ("EUC-JP", "EUC_JP"),
        ("ISO-2022-JP", "ISO2022JP"),
        ("Big5", "Big5"),
        ("EUC-KR", "EUC_KR"),
        ("GB2312", "EUC_CN"),
        ("GBK", "GBK"),
        ("GB18030", "GB18030"),
        ("windows-1252", "Cp1252"),
        ("windows-1251", "Cp1251"),
        ("windows-1250", "Cp1250"),
        ("KOI8-R", "KOI8_R"),
        ("KOI8-U", "KOI8_U"),
        ("IBM850", "Cp850"),
        ("IBM1047", "Cp1047"),
        ("IBM500", "Cp500"),
    ];

    #[test]
    fn historical_names_match_the_host_jdk_transcript() {
        let wrong: Vec<String> = HOST_JDK
            .iter()
            .filter(|(canon, want)| historical_charset_name(canon) != *want)
            .map(|(canon, want)| {
                format!(
                    "  {canon}: got {:?}, JDK says {want:?}",
                    historical_charset_name(canon)
                )
            })
            .collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// Twenty-three of the thirty differ from the canonical name. Asserting the
    /// COUNT keeps a table that quietly degenerates to the identity function —
    /// which is the pre-fix behaviour, and passes every "unlisted name is
    /// returned unchanged" test — from looking correct.
    #[test]
    fn most_charsets_do_not_report_their_canonical_name() {
        let differing = HOST_JDK
            .iter()
            .filter(|(canon, want)| canon != want)
            .count();
        assert_eq!(differing, 23);
    }

    #[test]
    fn an_unlisted_name_is_returned_unchanged() {
        assert_eq!(
            historical_charset_name("x-craton-nonesuch"),
            "x-craton-nonesuch"
        );
    }

    /// The table's keys must be canonical names, or a lookup can never hit.
    #[test]
    fn every_key_is_a_canonical_name() {
        for (canon, _) in HOST_JDK {
            assert_eq!(
                canonical_charset_name(canon),
                Some(*canon),
                "{canon} is not what canonical_charset_name produces"
            );
        }
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
    fn utf8_strict_encode_reports_lone_high_surrogate() {
        // FIX (review MEDIUM): a lone high surrogate must REPORT a malformed
        // error on the strict path, not silently substitute U+FFFD.
        let chars: Vec<u16> = vec![0x0041, 0xD83D, 0x0042]; // 'A', lone high, 'B'
        let err = encode_chars("UTF-8", &chars).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Malformed);
        assert_eq!(err.charset, "UTF-8");
        assert_eq!(err.offset, 1);
    }

    #[test]
    fn utf8_strict_encode_reports_lone_low_surrogate() {
        let chars: Vec<u16> = vec![0xDE00, 0x0041]; // lone low surrogate, 'A'
        let err = encode_chars("UTF-8", &chars).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Malformed);
        assert_eq!(err.offset, 0);
    }

    #[test]
    fn utf8_strict_encode_reports_trailing_high_surrogate_as_incomplete() {
        // A high surrogate at the very END of the chunk is INCOMPLETE, not
        // malformed: a streaming encoder may receive its matching low surrogate
        // in the next chunk. The shims rely on this to buffer the surrogate
        // across a split write (Tomcat BUG-TC0622). Offset points at the
        // unpaired high surrogate so callers can encode just the prefix.
        let chars: Vec<u16> = vec![0x0041, 0xD800]; // 'A', trailing high surrogate
        let err = encode_chars("UTF-8", &chars).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Incomplete);
        assert_eq!(err.charset, "UTF-8");
        assert_eq!(err.offset, 1);
        // The lossy path is unchanged — it still substitutes U+FFFD regardless
        // of the (Incomplete vs Malformed) distinction.
        assert_eq!(
            encode_chars_lossy("UTF-8", &chars),
            vec![0x41, 0xEF, 0xBF, 0xBD]
        );
    }

    #[test]
    fn utf8_strict_encode_accepts_valid_surrogate_pair() {
        // A well-formed supplementary character must still encode fine.
        let chars: Vec<u16> = "A\u{1F600}".encode_utf16().collect();
        let b = encode_chars("UTF-8", &chars).unwrap();
        assert_eq!(b, "A\u{1F600}".as_bytes());
    }

    #[test]
    fn utf8_lossy_encode_substitutes_lone_surrogate() {
        // The lossy path keeps REPLACE semantics: lone surrogate -> U+FFFD.
        let chars: Vec<u16> = vec![0x0041, 0xD83D, 0x0042];
        let b = encode_chars_lossy("UTF-8", &chars);
        // 'A' + U+FFFD (EF BF BD) + 'B'
        assert_eq!(b, vec![0x41, 0xEF, 0xBF, 0xBD, 0x42]);
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
    fn utf16_strict_encode_reports_lone_surrogate() {
        let err = encode_chars("UTF-16BE", &[0xDE00, 0x0041]).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Malformed);
        assert_eq!(err.charset, "UTF-16BE");
        assert_eq!(err.offset, 0);

        let err = encode_chars("UTF-16", &[0x0041, 0xD800]).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Incomplete);
        assert_eq!(err.charset, "UTF-16");
        assert_eq!(err.offset, 1);
    }

    #[test]
    fn utf16_lossy_encode_stays_utf16_and_replaces_surrogate() {
        let bytes = encode_chars_lossy("UTF-16BE", &[0xDE00, 0x0041]);
        assert_eq!(bytes, &[0xFF, 0xFD, 0x00, 0x41]);

        let bytes = encode_chars_lossy("UTF-16", &[0xDE00]);
        assert_eq!(bytes, &[0xFE, 0xFF, 0xFF, 0xFD]);
    }

    #[test]
    fn utf16_lossy_decode_replaces_trailing_byte() {
        let decoded = decode_bytes_lossy("UTF-16BE", &[0x00, 0x41, 0x00]);
        assert_eq!(decoded, vec![0x0041, REPLACEMENT_CHAR]);
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
    fn utf32_strict_encode_reports_lone_surrogate() {
        let err = encode_chars("UTF-32BE", &[0xDE00, 0x0041]).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Malformed);
        assert_eq!(err.charset, "UTF-32BE");
        assert_eq!(err.offset, 0);
    }

    #[test]
    fn utf32_lossy_encode_stays_utf32_and_replaces_surrogate() {
        let bytes = encode_chars_lossy("UTF-32LE", &[0xDE00, 0x0041]);
        assert_eq!(bytes, &[0xFD, 0xFF, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn utf32_lossy_decode_replaces_trailing_bytes() {
        let decoded = decode_bytes_lossy("UTF-32BE", &[0x00, 0x00, 0x00, 0x41, 0x00, 0x00]);
        assert_eq!(decoded, vec![0x0041, REPLACEMENT_CHAR]);
    }

    #[test]
    fn multibyte_strict_encode_reports_malformed_utf16() {
        let err = encode_chars("Shift_JIS", &[0xD800, 0x0041]).unwrap_err();
        assert_eq!(err.kind, CodingErrorKind::Malformed);
        assert_eq!(err.charset, "Shift_JIS");
        assert_eq!(err.offset, 0);
    }

    #[test]
    fn koi8r_sample() {
        // 0xC1 should map to Cyrillic 'а' (U+0430).
        let d = decode_bytes("KOI8-R", &[0xC1]).unwrap();
        assert_eq!(d, vec![0x0430]);
        let b = encode_chars("KOI8-R", &[0x0430]).unwrap();
        assert_eq!(b, &[0xC1]);
    }

    #[test]
    fn ibm1047_roundtrip_uses_ebcdic_low_half() {
        assert_eq!(canonical_charset_name("Cp1047"), Some("IBM1047"));
        assert_eq!(
            decode_bytes("IBM1047", &[0x40, 0x4B, 0x6F]).unwrap(),
            vec![0x20, 0x2E, 0x3F]
        );

        let s = "AZaz09?{}";
        let chars = s.encode_utf16().collect::<Vec<_>>();
        let bytes = encode_chars("IBM1047", &chars).unwrap();
        assert_eq!(
            bytes,
            vec![0xC1, 0xE9, 0x81, 0xA9, 0xF0, 0xF9, 0x6F, 0xC0, 0xD0]
        );
        let back = decode_bytes("IBM1047", &bytes).unwrap();
        assert_eq!(String::from_utf16(&back).unwrap(), s);
    }

    #[test]
    fn latin1_historic_8859_1_alias_is_supported() {
        assert_eq!(canonical_charset_name("8859_1"), Some("ISO-8859-1"));
    }

    #[test]
    fn ibm1047_lossy_replacement_is_ebcdic_question_mark() {
        assert_eq!(encode_chars_lossy("IBM1047", &[0x20AC]), vec![0x6F]);
    }

    /// `US-ASCII` is the ONE byte-oriented charset whose strict decode can
    /// fail, and until 2026-08-05 `decode_bytes_lossy` had no arm for it: the
    /// `Err` fell through to the unknown-name catch-all, whose `b as u16` map
    /// is Latin-1. `new String(bytes, "US-ASCII")` silently produced Latin-1
    /// text for any high-bit byte.
    ///
    /// Both halves matter, and the sibling test below is what makes this one
    /// non-vacuous: a fallback that returned all-U+FFFD would pass this
    /// assertion's first half while destroying the unknown-name behaviour.
    #[test]
    fn lossy_decode_us_ascii_replaces_high_bytes_rather_than_latin1() {
        // HotSpot 25, checked directly:
        //   new String(new byte[]{(byte)0xE9,'a',(byte)0xFF}, "US-ASCII")
        //     -> length 3, units FFFD 0061 FFFD
        let chars = decode_bytes_lossy("US-ASCII", &[0xE9, b'a', 0xFF]);
        assert_eq!(
            chars,
            vec![REPLACEMENT_CHAR, 0x0061, REPLACEMENT_CHAR],
            "a high-bit byte must become U+FFFD, not its Latin-1 character              (0x00E9/0x00FF is the pre-2026-08-05 answer)"
        );
        // One replacement PER BYTE: the decoded length tracks the input length.
        assert_eq!(chars.len(), 3);

        // Pure ASCII is untouched, so the fix cannot be a blanket substitution.
        assert_eq!(decode_bytes_lossy("US-ASCII", b"hi"), vec![0x0068, 0x0069]);
        assert!(decode_bytes_lossy("US-ASCII", b"hi")
            .iter()
            .all(|&c| c != REPLACEMENT_CHAR));
    }

    #[test]
    fn lossy_decode_unknown_name_is_latin1_not_all_replacement() {
        // An unsupported charset name must NOT fabricate an all-U+FFFD buffer
        // (which discards the bytes and misreports content). It falls back to
        // Latin-1 byte identity, which round-trips through ISO-8859-1.
        let bytes: &[u8] = &[0x41, 0x80, 0xFF, 0x00];
        let chars = decode_bytes_lossy("Some-Unknown-Charset", bytes);
        assert_eq!(chars, vec![0x0041, 0x0080, 0x00FF, 0x0000]);
        // None of them are the replacement character.
        assert!(chars.iter().all(|&c| c != REPLACEMENT_CHAR));
    }

    #[test]
    fn lossy_encode_unknown_name_is_latin1_with_question_mark() {
        // Old behavior re-encoded as UTF-8, producing bytes in the wrong
        // encoding. Now: representable units keep their byte value, the rest
        // become '?'. Emoji = 2 surrogate units, each unmappable -> '?'.
        let chars = "A\u{00FF}\u{1F600}".encode_utf16().collect::<Vec<_>>();
        let bytes = encode_chars_lossy("Some-Unknown-Charset", &chars);
        assert_eq!(bytes, vec![0x41, 0xFF, b'?', b'?']);
    }

    /// Mirror `TestUtf8.doTest` REPLACE phase: feed `input` one byte at a
    /// time with end_of_input=false (buffering unconsumed bytes like
    /// `ByteBuffer.compact`), then a final flush with end_of_input=true.
    fn stream_decode_replace(input: &[u8]) -> String {
        let mut leftover: Vec<u8> = Vec::new();
        let mut units: Vec<u16> = Vec::new();
        let cap = input.len().max(1) * 2;
        for &b in input {
            leftover.push(b);
            let mut chunk: Vec<u16> = Vec::new();
            let (consumed, _) =
                utf8_decode(&leftover, false, CodingAction::Replace, &mut chunk, cap);
            units.extend_from_slice(&chunk);
            leftover.drain(..consumed);
        }
        let mut chunk: Vec<u16> = Vec::new();
        let (consumed, _) = utf8_decode(&leftover, true, CodingAction::Replace, &mut chunk, cap);
        units.extend_from_slice(&chunk);
        leftover.drain(..consumed);
        String::from_utf16(&units).unwrap()
    }

    /// Mirror `TestUtf8.doTest` REPORT phase: feed one byte at a time with
    /// end_of_input=false and return the byte index of the first error, or
    /// `None` if no error surfaced during streaming (truncated sequences).
    fn stream_decode_report_index(input: &[u8]) -> Option<usize> {
        let mut leftover: Vec<u8> = Vec::new();
        let cap = input.len().max(1) * 2;
        for (i, &b) in input.iter().enumerate() {
            leftover.push(b);
            let mut chunk: Vec<u16> = Vec::new();
            let (consumed, status) =
                utf8_decode(&leftover, false, CodingAction::Report, &mut chunk, cap);
            if status == Utf8DecodeStatus::Malformed {
                return Some(i);
            }
            leftover.drain(..consumed);
        }
        None
    }

    #[test]
    fn utf8_testutf8_53_cases_replace_byte_for_byte() {
        // Every case from org.apache.tomcat.util.buf.TestUtf8 (input bytes,
        // REPORT-phase invalid byte index, REPLACE-phase expected output).
        // The decoder must reproduce HotSpot's substitution exactly.
        let cases: &[(&[u8], i32, &str)] = &[
            (&[], -1, ""),
            (&[0x41], -1, "A"),
            (&[0xC2, 0xA9], -1, "\u{00A9}"),
            (&[0xE0, 0xA4, 0x87], -1, "\u{0907}"),
            (&[0xF0, 0x90, 0x90, 0x80], -1, "\u{10400}"),
            (
                &[0x41, 0xF4, 0x90, 0x80, 0x80, 0x41],
                2,
                "A\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}A",
            ),
            (&[0x41, 0xC0, 0xC1, 0x41], 1, "A\u{FFFD}\u{FFFD}A"),
            (
                &[0x41, 0xE0, 0x80, 0xC1, 0x41],
                2,
                "A\u{FFFD}\u{FFFD}\u{FFFD}A",
            ),
            (
                &[0x41, 0xF0, 0x80, 0x80, 0xC1, 0x41],
                2,
                "A\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}A",
            ),
            (&[0x41, 0xFF, 0x41], 1, "A\u{FFFD}A"),
            (&[0x41, 0xF0, 0x41], 2, "A\u{FFFD}A"),
            (&[0x41, 0xE0, 0x41], 2, "A\u{FFFD}A"),
            (&[0x41, 0xC0, 0x41], 1, "A\u{FFFD}A"),
            (&[0x41, 0x80, 0x41], 1, "A\u{FFFD}A"),
            (
                &[
                    0x61, 0xF1, 0x80, 0x80, 0xE1, 0x80, 0xC2, 0x62, 0x80, 0x63, 0x80, 0xBF, 0x64,
                ],
                4,
                "a\u{FFFD}\u{FFFD}\u{FFFD}b\u{FFFD}c\u{FFFD}\u{FFFD}d",
            ),
            (&[0x61, 0xF0, 0x90, 0x90], 3, "a\u{FFFD}"),
            (&[0x61, 0xF0, 0x90], 2, "a\u{FFFD}"),
            (&[0x61, 0xF0], 1, "a\u{FFFD}"),
            (&[0x61, 0xF0, 0x90, 0x90, 0x61], 4, "a\u{FFFD}a"),
            (&[0x61, 0xF0, 0x90, 0x61], 3, "a\u{FFFD}a"),
            (&[0x61, 0xF0, 0x61], 2, "a\u{FFFD}a"),
            (&[0x61, 0xC0, 0x80, 0x61], 1, "a\u{FFFD}\u{FFFD}a"),
            (&[0x61, 0xC1, 0xBF, 0x61], 1, "a\u{FFFD}\u{FFFD}a"),
            (&[0x61, 0xFF, 0xFF, 0x61], 1, "a\u{FFFD}\u{FFFD}a"),
            (&[0x61, 0xE0, 0x80, 0x61], 2, "a\u{FFFD}\u{FFFD}a"),
            (&[0x61, 0xA0, 0x80, 0x61], 1, "a\u{FFFD}\u{FFFD}a"),
            (&[0x61, 0xC2, 0x00, 0x61], 2, "a\u{FFFD}\u{0000}a"),
            (&[0x61, 0xC2, 0xC0, 0x61], 2, "a\u{FFFD}\u{FFFD}a"),
            (
                &[0x61, 0xE0, 0x80, 0x80, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xE0, 0x81, 0xBF, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xE0, 0x9F, 0xBF, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xFF, 0xFF, 0xFF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF8, 0x80, 0x80, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xE0, 0xC0, 0x80, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (&[0x61, 0xE1, 0x80, 0xC0, 0x61], 3, "a\u{FFFD}\u{FFFD}a"),
            (
                &[0x61, 0xF0, 0x80, 0x80, 0x80, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF0, 0x80, 0x81, 0xBF, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF0, 0x80, 0x9F, 0xBF, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF0, 0x8F, 0xBF, 0xBF, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xFF, 0xFF, 0xFF, 0xFF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF8, 0x80, 0x80, 0x80, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF1, 0xC0, 0x80, 0x80, 0x61],
                2,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF1, 0x80, 0xC0, 0x80, 0x61],
                3,
                "a\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF1, 0x80, 0x80, 0xC0, 0x61],
                4,
                "a\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF8, 0x80, 0x80, 0x80, 0x80, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF8, 0x80, 0x80, 0x81, 0xBF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF8, 0x80, 0x80, 0x9F, 0xBF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xF8, 0x80, 0x8F, 0xBF, 0xBF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xFC, 0x80, 0x80, 0x80, 0x80, 0x80, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xFC, 0x80, 0x80, 0x80, 0x81, 0xBF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xFC, 0x80, 0x80, 0x80, 0x9F, 0xBF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[0x61, 0xFC, 0x80, 0x80, 0x8F, 0xBF, 0xBF, 0x61],
                1,
                "a\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}a",
            ),
            (
                &[
                    0xCE, 0xBA, 0xE1, 0xDB, 0xB9, 0xCF, 0x83, 0xCE, 0xBC, 0xCE, 0xB5, 0xED, 0x80,
                    0x65, 0x64, 0x69, 0x74, 0x65, 0x64,
                ],
                3,
                "\u{03BA}\u{FFFD}\u{06F9}\u{03C3}\u{03BC}\u{03B5}\u{FFFD}edited",
            ),
        ];
        assert_eq!(cases.len(), 53, "expected all 53 TestUtf8 cases");
        for (idx, (input, invalid_index, expected)) in cases.iter().enumerate() {
            let got = stream_decode_replace(input);
            assert_eq!(
                &got, expected,
                "REPLACE mismatch at case {idx}: input {input:02X?}"
            );
            // REPORT phase only asserts the index when an error actually
            // surfaces mid-stream (truncated trailing sequences never do).
            if let Some(i) = stream_decode_report_index(input) {
                assert_eq!(
                    i as i32, *invalid_index,
                    "REPORT index mismatch at case {idx}: input {input:02X?}"
                );
            }
        }
    }

    #[test]
    fn utf8_decode_overflow_is_precise() {
        // 'A' + U+00A9 (2-byte). Output capacity 1 → 'A' emitted, 1 byte
        // consumed, OVERFLOW with the 2-byte sequence left for retry.
        let src = &[0x41, 0xC2, 0xA9];
        let mut out = Vec::new();
        let (consumed, status) = utf8_decode(src, false, CodingAction::Replace, &mut out, 1);
        assert_eq!(out, vec![0x41]);
        assert_eq!(consumed, 1);
        assert_eq!(status, Utf8DecodeStatus::Overflow);
    }

    #[test]
    fn lossy_encode_single_byte_charset_substitutes_question_mark() {
        // windows-1252 cannot represent an emoji; lossy encode must emit '?'
        // in the *target* charset, NOT re-encode the whole input as UTF-8.
        // 'A' (mappable) + euro sign U+20AC (maps to 0x80) + emoji (unmappable).
        let chars = "A\u{20AC}\u{1F600}".encode_utf16().collect::<Vec<_>>();
        let bytes = encode_chars_lossy("windows-1252", &chars);
        assert_eq!(bytes, vec![b'A', 0x80, b'?', b'?']);
    }
}
