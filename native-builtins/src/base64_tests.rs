use super::{
    b64_decode, b64_encode, b64_encode_wrapped, B64_VARIANT_BASIC, B64_VARIANT_MIME,
    B64_VARIANT_URL,
};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Every expectation below is a literal line of `Base64Probe`'s output on
/// HotSpot 25 — see
/// `fixed-suite-bugs/repros/base64-decoder-parity/Base64Probe.java`.
fn err(input: &str, variant: i32) -> String {
    b64_decode(input.as_bytes(), variant).expect_err("must reject")
}

#[test]
fn decode_error_messages_match_real_jdk_wording() {
    // The message Spring Boot's Base64ProtocolResolverTests asserts on.
    assert_eq!(
        err("not valid base64", B64_VARIANT_BASIC),
        "Illegal base64 character 20"
    );
    assert_eq!(
        err("not base 64", B64_VARIANT_BASIC),
        "Illegal base64 character 20"
    );
    // One distinct message per malformed shape, not one catch-all.
    assert_eq!(
        err("A", B64_VARIANT_BASIC),
        "Input byte[] should at least have 2 bytes for base64 bytes"
    );
    assert_eq!(
        err("ABCDE", B64_VARIANT_BASIC),
        "Last unit does not have enough valid bits"
    );
    assert_eq!(
        err("A=AA", B64_VARIANT_BASIC),
        "Last unit does not have enough valid bits"
    );
    for bad_tail in ["AB=", "AB=C", "=AAA", "ABCD=", "ABCD=="] {
        assert_eq!(
            err(bad_tail, B64_VARIANT_BASIC),
            "Input byte array has wrong 4-byte ending unit",
            "input {bad_tail:?}"
        );
    }
    // Trailing data after the padding — the old decoder accepted this.
    assert_eq!(
        err("AB==CD", B64_VARIANT_BASIC),
        "Input byte array has incorrect ending byte at 4"
    );
    // MIME advances past the offending byte before reporting; BASIC does
    // not (Java's `&&` short-circuits the `sp++`).
    assert_eq!(
        err("AB==CD", B64_VARIANT_MIME),
        "Input byte array has incorrect ending byte at 5"
    );
}

#[test]
fn illegal_char_message_uses_the_signed_byte_in_hex() {
    // `Integer.toString(src[sp - 1], 16)` on a `byte[]` — 0xff is -1.
    assert_eq!(
        b64_decode(&[b'A', b'B', 0xff, b'D'], B64_VARIANT_BASIC).expect_err("must reject"),
        "Illegal base64 character -1"
    );
    assert_eq!(
        b64_decode(&[b'A', b'B', 0x80, b'D'], B64_VARIANT_BASIC).expect_err("must reject"),
        "Illegal base64 character -80"
    );
    assert_eq!(
        b64_decode(&[b'A', b'B', 0xc3, b'D'], B64_VARIANT_BASIC).expect_err("must reject"),
        "Illegal base64 character -3d"
    );
    assert_eq!(
        b64_decode(&[b'A', b'B', 0x00, b'D'], B64_VARIANT_BASIC).expect_err("must reject"),
        "Illegal base64 character 0"
    );
}

#[test]
fn mime_decoder_ignores_every_non_alphabet_byte_not_just_whitespace() {
    // RFC 2045 decoding skips illegal characters outright.
    assert_eq!(
        b64_decode(b"AB@@CD", B64_VARIANT_MIME).expect("MIME skips junk"),
        vec![0x00, 0x10, 0x83]
    );
    assert_eq!(
        b64_decode(&[b'A', b'B', 0xff, b'C', b'D'], B64_VARIANT_MIME).expect("MIME skips junk"),
        vec![0x00, 0x10, 0x83]
    );
    // `-` and `_` are not in the MIME alphabet, so both are dropped.
    assert_eq!(
        b64_decode(b"-_", B64_VARIANT_MIME).expect("skipped"),
        Vec::<u8>::new()
    );
    // …but they ARE the URL alphabet's 62/63.
    assert_eq!(b64_decode(b"-_", B64_VARIANT_URL).expect("url"), vec![0xfb]);
    // A lone char is fatal for BASIC but reaches decode0 for MIME.
    assert_eq!(
        err("A", B64_VARIANT_MIME),
        "Last unit does not have enough valid bits"
    );
    assert_eq!(
        b64_decode(b"", B64_VARIANT_MIME).expect("empty"),
        Vec::<u8>::new()
    );
}

#[test]
fn decode_accepts_the_well_formed_shapes() {
    assert_eq!(
        b64_decode(b"", B64_VARIANT_BASIC).expect("ok"),
        Vec::<u8>::new()
    );
    assert_eq!(
        b64_decode(b"AB", B64_VARIANT_BASIC).expect("ok"),
        vec![0x00]
    );
    assert_eq!(
        b64_decode(b"AB==", B64_VARIANT_BASIC).expect("ok"),
        vec![0x00]
    );
    assert_eq!(
        b64_decode(b"ABC", B64_VARIANT_BASIC).expect("ok"),
        vec![0x00, 0x10]
    );
    assert_eq!(
        b64_decode(b"ABC=", B64_VARIANT_BASIC).expect("ok"),
        vec![0x00, 0x10]
    );
    assert_eq!(
        b64_decode(b"ABCD", B64_VARIANT_BASIC).expect("ok"),
        vec![0x00, 0x10, 0x83]
    );
    assert_eq!(
        b64_decode(b"a+b/", B64_VARIANT_BASIC).expect("ok"),
        vec![0x6b, 0xe6, 0xff]
    );
}

#[test]
fn mime_encoder_omits_the_trailing_line_separator() {
    // Real `encode0` writes the separator only when more input follows, so
    // a payload ending exactly on the 76-char boundary gets none.
    let payload: Vec<u8> = (0..57u16).map(|i| i as u8).collect();
    let encoded = b64_encode(&payload, B64_VARIANT_MIME, false);
    assert_eq!(encoded.len(), 76, "57 bytes is exactly one full MIME line");
    assert!(!encoded.ends_with(b"\r\n"));
    // 58 bytes spills onto a second line, so the separator does appear.
    let payload: Vec<u8> = (0..58u16).map(|i| i as u8).collect();
    let encoded = b64_encode(&payload, B64_VARIANT_MIME, false);
    assert_eq!(encoded.len(), 82);
    assert_eq!(&encoded[76..78], b"\r\n");
    // Two full lines, still no trailing separator.
    let payload: Vec<u8> = (0..114u16).map(|i| i as u8).collect();
    let encoded = b64_encode(&payload, B64_VARIANT_MIME, false);
    assert_eq!(encoded.len(), 154);
    assert!(!encoded.ends_with(b"\r\n"));
}

#[test]
fn mime_round_trip_survives_the_line_separators() {
    let payload: Vec<u8> = (0..300u16).map(|i| (i * 7) as u8).collect();
    let encoded = b64_encode(&payload, B64_VARIANT_MIME, false);
    assert_eq!(
        b64_decode(&encoded, B64_VARIANT_MIME).expect("round trip"),
        payload
    );
    let encoded = b64_encode(&payload, B64_VARIANT_BASIC, false);
    assert_eq!(
        b64_decode(&encoded, B64_VARIANT_BASIC).expect("round trip"),
        payload
    );
}

#[test]
fn url_encoder_without_padding_omits_all_trailing_equals() {
    let sha256 = [0u8; 32];
    let sha1 = [0u8; 20];

    let sha256_encoded = b64_encode(&sha256, B64_VARIANT_URL, true);
    let sha1_encoded = b64_encode(&sha1, B64_VARIANT_URL, true);

    assert_eq!(
        sha256_encoded.len(),
        43,
        "SHA-256 base64url must be unpadded"
    );
    assert_eq!(sha1_encoded.len(), 27, "SHA-1 base64url must be unpadded");
    assert!(!sha256_encoded.contains(&b'='));
    assert!(!sha1_encoded.contains(&b'='));
}

#[test]
fn padding_flag_preserves_standard_encoder_behavior() {
    assert_eq!(b64_encode(&[0], B64_VARIANT_BASIC, false), b"AA==");
    assert_eq!(b64_encode(&[0], B64_VARIANT_BASIC, true), b"AA");
    assert_eq!(b64_encode(&[0, 0], B64_VARIANT_BASIC, false), b"AAA=");
    assert_eq!(b64_encode(&[0, 0], B64_VARIANT_BASIC, true), b"AAA");
}

/// Every row is a literal line of `scratchpad/e14/B64Line.java`'s output on
/// HotSpot 25 (`java --add-opens java.base/java.util=ALL-UNNAMED`), encoding
/// 60 bytes `0..59` — 80 base64 characters before any separator — through
/// `Base64.getMimeEncoder(lineLength, separator)`.
///
/// The rows are chosen to separate the three rules that a
/// "wrap every `linemax` characters" rewrite gets wrong, so that a
/// regression names itself:
///
/// * **rounding** — 19 wraps at 16, 21/22/23 wrap at 20 (`lineLength >> 2 << 2`);
/// * **`dlen == linemax` is an EQUALITY** — a `linemax` that is not a
///   multiple of 4 emits NO separators (22 -> 80, 6 -> 80), which a `>=`
///   would turn into 83/93;
/// * **separator length is not 2** — 80 + 3x1 = 83 for `{'\n'}` against
///   80 + 1x2 = 82 for the canonical 76/CRLF. Those two differ by ONE, which
///   is why a "roughly the right length" assertion cannot see the bug.
#[test]
fn line_wrapping_matches_hotspot_for_every_measured_linemax() {
    let payload: Vec<u8> = (0..60u16).map(|i| i as u8).collect();
    let lf = b"\n".as_slice();
    let crlf = b"\r\n".as_slice();

    // (linemax, separator, expected length) — `lineLength` already rounded,
    // exactly as `getMimeEncoder(int, byte[])` stores it.
    let rows: &[(i32, &[u8], usize)] = &[
        (-1, &[], 80),    // getEncoder()
        (0, &[], 80),     // no encoder produces this, but `> 0` must reject it
        (4, crlf, 118),   // 80 + 19x2
        (8, b"!#$", 107), // 80 + 9x3   — a 3-byte separator
        (16, lf, 84),     // 80 + 4x1
        (20, lf, 83),     // 80 + 3x1  <- THE ROW: 83, not 82
        (24, lf, 83),     // 80 + 3x1, different line count, same total
        (76, lf, 81),     // 80 + 1x1
        (76, crlf, 82),   // 80 + 1x2  — canonical getMimeEncoder()
        (1000, lf, 80),   // linemax past the payload wraps nothing
        (6, lf, 80),      // not a multiple of 4 -> `dlen == linemax` never holds
        (22, lf, 80),     // ditto
        (20, &[], 80),    // an EMPTY separator is legal and adds nothing
    ];
    for &(linemax, newline, expected) in rows {
        let encoded = b64_encode_wrapped(&payload, false, false, linemax, newline);
        assert_eq!(
            encoded.len(),
            expected,
            "linemax={linemax} newline={newline:?}: HotSpot answers {expected}"
        );
    }

    // The full text of the row that used to be wrong, not just its length:
    // four 20-character lines separated by LF, no trailing separator.
    let encoded = b64_encode_wrapped(&payload, false, false, 20, lf);
    assert_eq!(
        std::str::from_utf8(&encoded).expect("ascii"),
        "AAECAwQFBgcICQoLDA0O\nDxAREhMUFRYXGBkaGxwd\nHh8gISIjJCUmJygpKiss\nLS4vMDEyMzQ1Njc4OTo7"
    );
}

/// `dlen == linemax && sp < end`: a payload that ends exactly on a line
/// boundary gets no trailing separator, at any linemax.
#[test]
fn no_trailing_separator_at_any_linemax() {
    let lf = b"\n".as_slice();
    // 15 bytes = 20 chars = exactly one line at linemax 20.
    let exact = b64_encode_wrapped(&[0u8; 15], false, false, 20, lf);
    assert_eq!(exact.len(), 20, "HotSpot: 20, not 21");
    // 16 bytes = 20 chars + a 1-byte tail, so the separator DOES appear.
    let spill = b64_encode_wrapped(&[0u8; 16], false, false, 20, lf);
    assert_eq!(spill.len(), 25, "HotSpot: 20 + separator + \"AA==\"");
    assert_eq!(
        std::str::from_utf8(&spill).expect("ascii"),
        "AAAAAAAAAAAAAAAAAAAA\nAA=="
    );
    // 57 bytes = exactly one 76-char line.
    assert_eq!(
        b64_encode_wrapped(&[0u8; 57], false, false, 76, b"\r\n").len(),
        76
    );
}

/// A `linemax` of 1..=3 makes real JDK's block size `linemax / 4 * 3` zero,
/// and its `while (sp < sl)` loop then never advances — measured: HotSpot
/// was still running all three after 1.2 s
/// (`scratchpad/e14/B64Fab.java`). No public factory can produce it, so
/// this is a deliberate divergence: we treat it as "no wrapping". The test
/// exists so the guard is not "simplified" back into a hang.
#[test]
fn a_sub_quantum_linemax_terminates_instead_of_spinning() {
    let payload: Vec<u8> = (0..60u16).map(|i| i as u8).collect();
    for linemax in 1..=3 {
        let encoded = b64_encode_wrapped(&payload, false, false, linemax, b"\n");
        assert_eq!(
            encoded.len(),
            80,
            "linemax={linemax} must terminate, not wrap"
        );
    }
}

/// The variant-tag entry point that this module's out-of-file callers use
/// must still mean exactly what it meant before `linemax`/`newline` became
/// parameters.
#[test]
fn the_variant_tag_wrapper_still_means_76_and_crlf() {
    let payload: Vec<u8> = (0..60u16).map(|i| i as u8).collect();
    assert_eq!(
        b64_encode(&payload, B64_VARIANT_MIME, false),
        b64_encode_wrapped(&payload, false, false, 76, b"\r\n")
    );
    assert_eq!(
        b64_encode(&payload, B64_VARIANT_BASIC, false),
        b64_encode_wrapped(&payload, false, false, -1, &[])
    );
    assert_eq!(
        b64_encode(&payload, B64_VARIANT_URL, true),
        b64_encode_wrapped(&payload, true, true, -1, &[])
    );
}
