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
        "a high-bit byte must become U+FFFD, not its Latin-1 character \
         (0x00E9/0x00FF is the pre-2026-08-05 answer)"
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
