use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// -- finding 1: JSON unescape surrogate pairs + multi-byte safety --------

#[test]
fn json_unescape_decodes_astral_surrogate_pair() {
    // U+1F600 GRINNING FACE encoded as a UTF-16 surrogate pair.
    let s = json_unescape("\\uD83D\\uDE00");
    assert_eq!(
        s, "\u{1F600}",
        "surrogate pair must combine into one astral char"
    );
}

#[test]
fn json_unescape_lone_surrogate_becomes_replacement_not_dropped() {
    // A lone high surrogate must not silently vanish.
    let hi = json_unescape("a\\uD83Db");
    assert_eq!(hi, "a\u{FFFD}b");
    // A lone low surrogate likewise.
    let lo = json_unescape("a\\uDE00b");
    assert_eq!(lo, "a\u{FFFD}b");
}

#[test]
fn json_unescape_handles_bmp_and_basic_escapes() {
    assert_eq!(json_unescape("\\u0041\\n\\t\\\""), "A\n\t\"");
}

#[test]
fn json_unescape_malformed_u_escape_does_not_panic() {
    // Fewer than 4 hex digits after \u → replacement, no panic.
    assert_eq!(json_unescape("\\u12"), "\u{FFFD}");
    assert_eq!(json_unescape("\\uZZZZ"), "\u{FFFD}");
}

#[test]
fn json_utf8_char_len_classifies_widths() {
    assert_eq!(json_utf8_char_len("a".as_bytes(), 0), 1);
    assert_eq!(json_utf8_char_len("é".as_bytes(), 0), 2); // 0xC3 0xA9
    assert_eq!(json_utf8_char_len("€".as_bytes(), 0), 3); // 3-byte
    assert_eq!(json_utf8_char_len("😀".as_bytes(), 0), 4); // 4-byte
                                                           // Out-of-range index returns 0.
    assert_eq!(json_utf8_char_len("a".as_bytes(), 5), 0);
}

#[test]
fn read_json_value_no_panic_on_escaped_multibyte() {
    // `\é` inside a string would previously skip into the middle of the
    // 2-byte 'é' and panic on the slice. Now it parses cleanly.
    let s = "\"x\\éy\"";
    let mut i = 0usize;
    let v = read_json_value(s, &mut i);
    // The unescaped value keeps the backslash-prefixed unknown escape as `\é`.
    assert!(
        v.contains('é'),
        "value should retain the multi-byte char, got {v:?}"
    );
}

#[test]
fn parse_json_object_no_panic_on_escaped_multibyte_key() {
    // Malformed/odd escapes with multi-byte chars must not panic.
    let pairs = parse_json_object("{\"k\\é\":\"v\"}");
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, "v");
}

// -- finding 3: bounded inflation (compression-bomb defense) -------------

#[test]
fn inflate_bounded_rejects_oversize() {
    let data = vec![0u8; 1024];
    // Cap below the data size → error.
    let res = inflate_bounded(&data[..], Some(512));
    assert!(res.is_err(), "exceeding the cap must error");
    let e = res.unwrap_err();
    assert!(e.to_string().contains("compression bomb"));
}

#[test]
fn inflate_bounded_allows_within_cap_and_unbounded() {
    let data = vec![7u8; 1024];
    // Within cap → full read.
    let ok = inflate_bounded(&data[..], Some(4096)).expect("within cap");
    assert_eq!(ok.len(), 1024);
    // Cap disabled (None) → unbounded read.
    let un = inflate_bounded(&data[..], None).expect("unbounded");
    assert_eq!(un.len(), 1024);
}

/// The default cap must be the documented size and must be ENABLED —
/// `None` means the compression-bomb guard is switched off.
///
/// Was vacuous: `assert!(cap.map(|c| c > 0).unwrap_or(true))` accepted
/// `None`, so making the no-override branch of `gzip_max_inflated_bytes()`
/// return `None` (or making an unparseable value disable the cap) left the
/// test green. Driven through thread-scoped flag overrides so the ambient
/// process environment cannot decide the outcome.
#[test]
fn gzip_max_inflated_default_is_positive() {
    use cratonvm_types::flags::with_thread_overrides;
    const VAR: &str = "CRATONVM_MAX_INFLATED_BYTES";

    assert_eq!(GZIP_DEFAULT_MAX_INFLATED, 256 * 1024 * 1024);
    // No override → the documented default, cap ON.
    with_thread_overrides(&[(VAR, None)], || {
        assert_eq!(
            gzip_max_inflated_bytes(),
            Some(GZIP_DEFAULT_MAX_INFLATED),
            "the default cap must be enabled at the documented size"
        );
    });
    // An explicit byte count wins.
    with_thread_overrides(&[(VAR, Some("4096"))], || {
        assert_eq!(gzip_max_inflated_bytes(), Some(4096));
    });
    // Garbage falls back to the default rather than disabling the guard.
    with_thread_overrides(&[(VAR, Some("not-a-number"))], || {
        assert_eq!(gzip_max_inflated_bytes(), Some(GZIP_DEFAULT_MAX_INFLATED));
    });
    // Only an explicit `0` disables it.
    with_thread_overrides(&[(VAR, Some("0"))], || {
        assert_eq!(gzip_max_inflated_bytes(), None);
    });
}
