use super::decode_utf8_into_chars;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn to_string(chars: &[u16]) -> String {
    String::from_utf16(chars).unwrap()
}

#[test]
fn ra2_ascii_roundtrip() {
    let bytes = b"hello world";
    let (chars, consumed, tail, low) = decode_utf8_into_chars(bytes, true, 100);
    assert_eq!(to_string(&chars), "hello world");
    assert_eq!(consumed, bytes.len());
    assert!(tail.is_empty());
    assert!(low.is_none());
}

#[test]
fn ra2_two_byte_copyright_sign_decodes_to_a9() {
    // U+00A9 © = 0xC2 0xA9 in UTF-8.
    let (chars, _, _, _) = decode_utf8_into_chars(&[0xC2, 0xA9], true, 100);
    assert_eq!(chars, vec![0x00A9]);
}

#[test]
fn ra2_three_byte_cjk_decodes_single_bmp_char() {
    // U+4E2D 中 = 0xE4 0xB8 0xAD
    let (chars, _, _, _) = decode_utf8_into_chars(&[0xE4, 0xB8, 0xAD], true, 100);
    assert_eq!(chars, vec![0x4E2Du16]);
}

#[test]
fn ra2_four_byte_supplementary_yields_surrogate_pair() {
    // U+1F600 😀 = 0xF0 0x9F 0x98 0x80 → surrogates D83D, DE00
    let (chars, _, _, low) = decode_utf8_into_chars(&[0xF0, 0x9F, 0x98, 0x80], true, 100);
    assert_eq!(chars, vec![0xD83Du16, 0xDE00u16]);
    assert!(low.is_none());
}

#[test]
fn ra2_split_multibyte_stashes_tail() {
    // Only 2 bytes of the 3-byte 中 arrive
    let (chars, consumed, tail, _) = decode_utf8_into_chars(&[0xE4, 0xB8], false, 100);
    assert!(chars.is_empty());
    assert_eq!(consumed, 0);
    assert_eq!(tail, vec![0xE4, 0xB8]);
}

#[test]
fn ra2_deferred_low_surrogate_on_tight_buffer() {
    // Supplementary char but only 1 slot left — high surrogate
    // emitted, low surrogate deferred.
    let (chars, _, _, low) = decode_utf8_into_chars(&[0xF0, 0x9F, 0x98, 0x80], true, 1);
    assert_eq!(chars, vec![0xD83Du16]);
    assert_eq!(low, Some(0xDE00u16));
}

#[test]
fn ra2_invalid_leading_byte_yields_replacement() {
    let (chars, _, _, _) = decode_utf8_into_chars(&[0xFF, b'x'], true, 100);
    assert_eq!(chars, vec![0xFFFDu16, b'x' as u16]);
}

#[test]
fn ra2_overlong_encoding_rejected() {
    // Overlong / (U+002F) as 0xC0 0xAF
    let (chars, _, _, _) = decode_utf8_into_chars(&[0xC0, 0xAF], true, 100);
    assert_eq!(chars, vec![0xFFFDu16]);
}

#[test]
fn ra2_truncated_at_eof_yields_replacement() {
    // 3-byte starter followed by EOF — emit replacement.
    let (chars, _, tail, _) = decode_utf8_into_chars(&[0xE4, 0xB8], true, 100);
    assert_eq!(chars, vec![0xFFFDu16]);
    assert!(tail.is_empty());
}

#[test]
fn ra2_four_byte_party_popper_expected_pair() {
    // U+1F389 🎉 = 0xF0 0x9F 0x8E 0x89 → D83C, DF89 (per RA.2 spec).
    let (chars, _, _, low) = decode_utf8_into_chars(&[0xF0, 0x9F, 0x8E, 0x89], true, 100);
    assert_eq!(chars, vec![0xD83Cu16, 0xDF89u16]);
    assert!(low.is_none());
}

#[test]
fn ra2_split_across_two_reads_rejoins_correctly() {
    // First call: only 2 of 3 bytes of U+2603 ☃ (0xE2 0x98 0x83) arrive.
    let (chars1, consumed1, tail1, _) = decode_utf8_into_chars(&[0x48, 0xE2, 0x98], false, 100);
    assert_eq!(chars1, vec![b'H' as u16]);
    assert_eq!(consumed1, 1);
    assert_eq!(tail1, vec![0xE2, 0x98]);

    // Second call: caller prepends stashed tail with the continuation byte.
    let mut next = tail1.clone();
    next.push(0x83);
    next.push(b'i');
    let (chars2, consumed2, tail2, _) = decode_utf8_into_chars(&next, true, 100);
    assert_eq!(chars2, vec![0x2603u16, b'i' as u16]);
    assert_eq!(consumed2, next.len());
    assert!(tail2.is_empty());
}

#[test]
fn ra2_mixed_ascii_and_bmp_200_byte_fixture() {
    // 200-byte fixture containing ASCII, Latin-1, and BMP chars.
    let mut src = String::new();
    for _ in 0..40 {
        src.push_str("a©中b");
    } // 1 + 2 + 3 + 1 = 7 bytes/iter → 280 bytes
    let bytes: Vec<u8> = src.bytes().collect();
    let (chars, consumed, tail, _) = decode_utf8_into_chars(&bytes, true, 10_000);
    assert_eq!(consumed, bytes.len());
    assert!(tail.is_empty());
    let decoded = to_string(&chars);
    assert_eq!(decoded, src);
}
