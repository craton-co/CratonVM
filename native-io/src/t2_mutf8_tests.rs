use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// ---- Encoder (writeUTF payload) ----

#[test]
fn t2_encode_ascii_matches_standard_utf8() {
    let bytes = encode_modified_utf8("hello");
    assert_eq!(bytes, b"hello");
}

#[test]
fn t2_encode_null_is_two_bytes_c080() {
    // The key modified-UTF-8 distinction: U+0000 is NEVER a single
    // zero byte — it's the two-byte sequence 0xC0 0x80.
    let bytes = encode_modified_utf8("\0");
    assert_eq!(bytes, [0xC0, 0x80]);
}

#[test]
fn t2_encode_latin1_uses_two_byte_form() {
    // U+00E9 (é) is two bytes in both standard UTF-8 and modified
    // UTF-8 (0xC3 0xA9), so this is a sanity check for the
    // branch rather than a semantic divergence.
    let bytes = encode_modified_utf8("é");
    assert_eq!(bytes, [0xC3, 0xA9]);
}

#[test]
fn t2_encode_bmp_char_uses_three_byte_form() {
    // U+4E2D (中) — Chinese character, 3 bytes in UTF-8.
    let bytes = encode_modified_utf8("中");
    assert_eq!(bytes, [0xE4, 0xB8, 0xAD]);
}

#[test]
fn t2_encode_supplementary_is_six_bytes_via_surrogate_pair() {
    // U+1F600 (😀) is a supplementary character. Standard UTF-8
    // encodes it in 4 bytes; modified UTF-8 encodes it as a
    // surrogate pair (0xD83D 0xDE00), each surrogate taking 3
    // bytes in the 3-byte form — **6 bytes total**.
    let bytes = encode_modified_utf8("😀");
    // High surrogate 0xD83D → 11101101 10100000 10111101 = ED A0 BD
    // Low surrogate  0xDE00 → 11101101 10111000 10000000 = ED B8 80
    assert_eq!(bytes, [0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]);
    assert_eq!(bytes.len(), 6);
}

#[test]
fn t2_encode_mixed_string_preserves_order() {
    let bytes = encode_modified_utf8("a\0中");
    assert_eq!(bytes, [b'a', 0xC0, 0x80, 0xE4, 0xB8, 0xAD]);
}

// ---- Decoder (readUTF payload) ----

#[test]
fn t2_decode_ascii_matches_standard_utf8() {
    assert_eq!(decode_modified_utf8(b"hello").unwrap(), "hello");
}

#[test]
fn t2_decode_c080_is_null_char() {
    assert_eq!(decode_modified_utf8(&[0xC0, 0x80]).unwrap(), "\0");
}

#[test]
fn t2_decode_latin1_round_trips() {
    assert_eq!(decode_modified_utf8(&[0xC3, 0xA9]).unwrap(), "é");
}

#[test]
fn t2_decode_three_byte_bmp_round_trips() {
    assert_eq!(decode_modified_utf8(&[0xE4, 0xB8, 0xAD]).unwrap(), "中");
}

#[test]
fn t2_decode_surrogate_pair_to_supplementary() {
    let decoded = decode_modified_utf8(&[0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]).unwrap();
    assert_eq!(decoded, "😀");
}

#[test]
fn t2_decode_empty_slice_returns_empty_string() {
    assert_eq!(decode_modified_utf8(b"").unwrap(), "");
}

#[test]
fn t2_decode_truncated_two_byte_errors() {
    // 0xC2 is a 2-byte leader but the continuation is missing.
    assert!(decode_modified_utf8(&[0xC2]).is_err());
}

#[test]
fn t2_decode_truncated_three_byte_errors() {
    // 0xE0 0x80 — should be 3 bytes, missing the third.
    assert!(decode_modified_utf8(&[0xE0, 0x80]).is_err());
}

#[test]
fn t2_decode_bad_continuation_byte_errors() {
    // 0xC2 followed by a non-continuation byte.
    assert!(decode_modified_utf8(&[0xC2, 0x41]).is_err());
}

#[test]
fn t2_decode_illegal_leading_byte_errors() {
    // 0xF8 has the 5-byte UTF-8 prefix which is not valid in
    // modified UTF-8 (supplementary chars use a surrogate pair).
    assert!(decode_modified_utf8(&[0xF8, 0x88, 0x80, 0x80, 0x80]).is_err());
}

#[test]
fn t2_decode_lone_high_surrogate_errors() {
    // A high surrogate without a following low surrogate is
    // malformed in modified UTF-8 (the spec specifically requires
    // pair encoding for supplementary planes).
    assert!(decode_modified_utf8(&[0xED, 0xA0, 0xBD]).is_err());
}

#[test]
fn t2_decode_lone_low_surrogate_errors() {
    assert!(decode_modified_utf8(&[0xED, 0xB8, 0x80]).is_err());
}

// ---- End-to-end round trip ----

#[test]
fn t2_round_trip_complex_string() {
    let input = "CratonVM ✨ 中文 \0 end";
    let encoded = encode_modified_utf8(input);
    let decoded = decode_modified_utf8(&encoded).unwrap();
    assert_eq!(decoded, input);
}

#[test]
fn t2_round_trip_supplementary_characters() {
    let input = "Emoji: 😀🦀 — done";
    let encoded = encode_modified_utf8(input);
    let decoded = decode_modified_utf8(&encoded).unwrap();
    assert_eq!(decoded, input);
}

// ---- JO3-3: the units-exact codec (round 15 wave 4, lane jdkonly5) ----

#[test]
fn jo3_3_encode_units_is_the_jdk_writeutf_payload() {
    assert_eq!(encode_modified_utf8_units(&[0x41]), [0x41]);
    assert_eq!(encode_modified_utf8_units(&[0]), [0xC0, 0x80]);
    assert_eq!(encode_modified_utf8_units(&[0x7FF]), [0xDF, 0xBF]);
    assert_eq!(encode_modified_utf8_units(&[0x800]), [0xE0, 0xA0, 0x80]);
    // An unpaired surrogate is encoded as the unit it is (HotSpot
    // `ED A0 80`); the `&str` path wrote U+FFFD (`EF BF BD`).
    assert_eq!(encode_modified_utf8_units(&[0xD800]), [0xED, 0xA0, 0x80]);
    assert_eq!(encode_modified_utf8_units(&[0xDC00]), [0xED, 0xB0, 0x80]);
    // A pair is two 3-byte forms, identical to the `&str` encoder's.
    let pair: Vec<u16> = "😀".encode_utf16().collect();
    assert_eq!(
        encode_modified_utf8_units(&pair),
        encode_modified_utf8("😀")
    );
}

#[test]
fn jo3_3_units_len_is_the_encoded_length() {
    let units: Vec<u16> = vec![0x41, 0, 0x7F, 0x80, 0x7FF, 0x800, 0xD800, 0xFFFF];
    assert_eq!(
        modified_utf8_units_len(&units),
        encode_modified_utf8_units(&units).len() as u64
    );
    assert_eq!(
        modified_utf8_units_len(&units),
        1 + 2 + 1 + 2 + 2 + 3 + 3 + 3
    );
}

#[test]
fn jo3_3_decode_units_keeps_unpaired_surrogates_and_never_pairs() {
    assert_eq!(
        decode_modified_utf8_units(&[0xED, 0xA0, 0x80]).unwrap(),
        [0xD800]
    );
    assert_eq!(
        decode_modified_utf8_units(&[0xED, 0xB0, 0x80]).unwrap(),
        [0xDC00]
    );
    assert_eq!(
        decode_modified_utf8_units(&[0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]).unwrap(),
        [0xD83D, 0xDE00]
    );
    assert_eq!(decode_modified_utf8_units(&[0xC0, 0x80]).unwrap(), [0]);
    assert_eq!(decode_modified_utf8_units(b"").unwrap(), Vec::<u16>::new());
    let units: Vec<u16> = vec![0x61, 0xD800, 0, 0x62, 0xDFFF, 0x4E2D];
    assert_eq!(
        decode_modified_utf8_units(&encode_modified_utf8_units(&units)).unwrap(),
        units
    );
}

#[test]
fn jo3_3_decode_units_refuses_with_the_jdk_messages() {
    let err = |b: &[u8]| decode_modified_utf8_units(b).unwrap_err();
    // A bad lead byte: its own offset.
    assert_eq!(err(&[0x80]), "malformed input around byte 0");
    assert_eq!(err(&[0x41, 0xF0]), "malformed input around byte 1");
    // A 2-byte form: the offset past the pair.
    assert_eq!(err(&[0x41, 0xC2, 0x41]), "malformed input around byte 3");
    // A 3-byte form: one less than the offset past it.
    assert_eq!(err(&[0xE4, 0x41, 0x80]), "malformed input around byte 2");
    assert_eq!(err(&[0xC2]), "malformed input: partial character at end");
    assert_eq!(
        err(&[0xE4, 0xB8]),
        "malformed input: partial character at end"
    );
}

#[test]
fn jo3_3_too_long_message_is_the_jdk_wording() {
    let units: Vec<u16> = std::iter::repeat(0x4E2D).take(30_000).collect();
    let utflen = modified_utf8_units_len(&units);
    assert_eq!(utflen, 90_000);
    let message = String::from_utf16(&modified_utf8_too_long_message(&units, utflen)).unwrap();
    assert_eq!(
        message,
        "encoded string (中中中中中中中中...中中中中中中中中) too long: 90000 bytes"
    );
}
