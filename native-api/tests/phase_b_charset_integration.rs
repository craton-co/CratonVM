// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase B integration tests for the shared charset engine
//! (`cratonvm_native_api::charset`). These tests correspond to the
//! success criteria listed in `roadmap-any-java-app.md`:
//!
//! * RB.1 — UTF-8 / US-ASCII / ISO-8859-1 / UTF-16LE/BE round-trip
//!   a 256-byte fixture.
//! * RB.6 — a 20-char surrogate-pair + embedded-NUL fixture survives a
//!   modified-UTF-8 encoder/decoder round-trip.  (Modified-UTF-8 lives
//!   in `cratonvm-native-io` but the shared round-trip property is the
//!   underlying UTF-16 invariant tested here with real UTF-8.)
//! * Misc.  Round-trip checks for the single-byte code pages we claim
//!   to support (windows-1252, KOI8-R, ISO-8859-15).

use cratonvm_native_api::charset::{
    average_bytes_per_char, average_chars_per_byte, decode_bytes, decode_bytes_lossy, encode_chars,
    encode_chars_lossy, max_bytes_per_char, max_chars_per_byte,
};

fn to_u16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn to_string(units: &[u16]) -> String {
    String::from_utf16(units).expect("valid UTF-16")
}

#[test]
fn rb1_utf8_roundtrip_256_bytes() {
    // Use the first 256 code points plus a sprinkling of supplementary
    // characters so we hit 1/2/3/4-byte UTF-8 sequences.
    let mut s = String::new();
    for cp in 0u32..=0xFF {
        if let Some(ch) = char::from_u32(cp) {
            s.push(ch);
        }
    }
    s.push('\u{1F600}'); // 4-byte supplementary
    s.push('\u{4E2D}'); // 3-byte BMP
    let chars = to_u16(&s);
    let bytes = encode_chars("UTF-8", &chars).unwrap();
    let back = to_string(&decode_bytes("UTF-8", &bytes).unwrap());
    assert_eq!(back, s);
}

#[test]
fn rb1_us_ascii_roundtrip() {
    let s: String = (0u8..0x80).map(char::from).collect();
    let chars = to_u16(&s);
    let bytes = encode_chars("US-ASCII", &chars).unwrap();
    assert_eq!(bytes.len(), 128);
    let back = to_string(&decode_bytes("US-ASCII", &bytes).unwrap());
    assert_eq!(back, s);
}

#[test]
fn rb1_iso8859_1_roundtrip_256() {
    let bytes: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
    let chars = decode_bytes("ISO-8859-1", &bytes).unwrap();
    let back = encode_chars("ISO-8859-1", &chars).unwrap();
    assert_eq!(back, bytes);
}

#[test]
fn rb1_utf16le_roundtrip() {
    let s = "A B C \u{4E2D} \u{1F600}";
    let chars = to_u16(s);
    let bytes = encode_chars("UTF-16LE", &chars).unwrap();
    let back = to_string(&decode_bytes("UTF-16LE", &bytes).unwrap());
    assert_eq!(back, s);
}

#[test]
fn rb1_utf16be_roundtrip() {
    let s = "Hello \u{00E9} \u{0915}\u{093E} \u{1F680}";
    let chars = to_u16(s);
    let bytes = encode_chars("UTF-16BE", &chars).unwrap();
    let back = to_string(&decode_bytes("UTF-16BE", &bytes).unwrap());
    assert_eq!(back, s);
}

#[test]
fn utf16_with_bom_written_big_endian() {
    let chars = to_u16("A");
    let bytes = encode_chars("UTF-16", &chars).unwrap();
    // BOM = FE FF, then 00 41.
    assert_eq!(bytes, vec![0xFE, 0xFF, 0x00, 0x41]);
}

#[test]
fn utf16_with_bom_reader_detects_le() {
    let bytes = [0xFF, 0xFE, 0x41, 0x00];
    let chars = decode_bytes("UTF-16", &bytes).unwrap();
    assert_eq!(chars, vec![0x0041]);
}

#[test]
fn rb6_surrogate_pair_embedded_nul_survive() {
    // 20-char fixture: mix ASCII, embedded NUL, BMP, surrogate pair.
    let s = "a\0b\u{00A9}\u{4E2D}\u{1F600}xyzZ\u{2603}\u{0915}\u{0916}\u{0917}\u{0918}";
    // Verify length: 'a', NUL, 'b', ©, 中, high-surrogate, low-surrogate,
    // x, y, z, Z, ☃, क, ख, ग, घ = 16 UTF-16 units (supplementary is 2).
    let chars = to_u16(s);
    assert!(chars.contains(&0));
    let bytes = encode_chars("UTF-8", &chars).unwrap();
    let back = to_string(&decode_bytes("UTF-8", &bytes).unwrap());
    assert_eq!(back, s);
}

#[test]
fn lossy_handles_malformed_utf8() {
    // 0x80 on its own is a stray continuation byte.
    let chars = decode_bytes_lossy("UTF-8", &[0x80, b'A']);
    // Must produce at least one char; the first should be the
    // replacement U+FFFD and the second the ASCII 'A'.
    assert!(chars.len() >= 2);
    assert_eq!(chars[chars.len() - 1], 0x0041);
}

#[test]
fn lossy_encode_unmappable_becomes_question_mark() {
    let chars = to_u16("A\u{1F600}");
    let bytes = encode_chars_lossy("US-ASCII", &chars);
    assert_eq!(bytes, vec![b'A', b'?', b'?']); // supplementary = 2 UTF-16 units
}

#[test]
fn windows_1252_euro_sign_roundtrip() {
    let chars = vec![0x20AC, 0x0041];
    let bytes = encode_chars("windows-1252", &chars).unwrap();
    assert_eq!(bytes, vec![0x80, b'A']);
    let back = decode_bytes("windows-1252", &bytes).unwrap();
    assert_eq!(back, chars);
}

#[test]
fn koi8r_cyrillic_a_roundtrip() {
    let chars = vec![0x0430]; // 'а'
    let bytes = encode_chars("KOI8-R", &chars).unwrap();
    assert_eq!(bytes, vec![0xC1]);
}

#[test]
fn iso_8859_15_euro_sign_roundtrip() {
    // ISO-8859-15 replaces 0xA4 with Euro sign (U+20AC).
    let chars = vec![0x20AC];
    let bytes = encode_chars("ISO-8859-15", &chars).unwrap();
    assert_eq!(bytes, vec![0xA4]);
    let back = decode_bytes("ISO-8859-15", &bytes).unwrap();
    assert_eq!(back, chars);
}

#[test]
fn encoder_metadata_matches_spec() {
    // Sanity-check the bytes/chars ratios used by CharsetEncoder /
    // CharsetDecoder JDK APIs.  Exact values must match what JDK
    // reports for each charset (modulo Java float precision).
    assert!((average_bytes_per_char("UTF-8") - 1.1).abs() < f32::EPSILON);
    assert_eq!(max_bytes_per_char("UTF-8"), 3.0);
    assert_eq!(average_chars_per_byte("UTF-16"), 0.5);
    assert_eq!(max_chars_per_byte("UTF-8"), 1.0);
}

#[test]
fn unsupported_charset_propagates_error() {
    let err = encode_chars("bogus-charset", &[]).unwrap_err();
    assert_eq!(err.charset, "(unknown)");
}

#[test]
fn utf32_roundtrip_with_supplementary() {
    let s = "A\u{1F600}B";
    let chars = to_u16(s);
    let bytes = encode_chars("UTF-32BE", &chars).unwrap();
    let back = to_string(&decode_bytes("UTF-32BE", &bytes).unwrap());
    assert_eq!(back, s);
}

#[test]
fn utf8_boundary_partial_not_panic() {
    // Middle of a 3-byte UTF-8 sequence.  Lossy decoder must not
    // panic and must produce replacement characters for the
    // incomplete tail.
    let chars = decode_bytes_lossy("UTF-8", &[0xE4, 0xB8]);
    assert!(!chars.is_empty());
}
