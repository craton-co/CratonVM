use super::decode_modified_utf8_exact as d;

#[test]
fn zt27lg1_modified_utf8_names_decode_exactly() {
    assert_eq!(d(b"a/B$C").as_deref(), Some("a/B$C"));
    assert_eq!(d(&[b'a', 0xC0, 0x80, b'b']).as_deref(), Some("a\0b"));
    assert_eq!(d(&[0xC3, 0xA9]).as_deref(), Some("\u{e9}"));
    assert_eq!(
        d(&[b'x', 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]).as_deref(),
        Some("x\u{1F600}")
    );
    assert_eq!(d(&[0xED, 0xA0, 0xBD]), None, "unpaired surrogate");
    assert_eq!(d(&[0xF0, 0x9F, 0x98, 0x80]), None, "standard 4-byte form");
    assert_eq!(d(&[0x00]), None, "a raw NUL byte");
    assert_eq!(d(&[0xC3]), None, "truncated");
}
