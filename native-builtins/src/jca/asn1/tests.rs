use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn encode_length_short_form() {
    assert_eq!(encode_length(0), vec![0x00]);
    assert_eq!(encode_length(127), vec![0x7F]);
}

#[test]
fn encode_length_long_form() {
    assert_eq!(encode_length(128), vec![0x81, 0x80]);
    assert_eq!(encode_length(0x100), vec![0x82, 0x01, 0x00]);
    assert_eq!(encode_length(0x10000), vec![0x83, 0x01, 0x00, 0x00]);
}

#[test]
fn oid_round_trip_2_5_4_3() {
    // 2.5.4.3 -> 0x55 0x04 0x03
    let der = encode_oid("2.5.4.3").unwrap();
    assert_eq!(der, vec![0x06, 0x03, 0x55, 0x04, 0x03]);
    let (tag, hdr, content_len, total) = read_header(&der).unwrap();
    assert_eq!(tag, TAG_OID);
    assert_eq!(content_len, 3);
    assert_eq!(total, 5);
    let oid = read_oid(&der[hdr..hdr + content_len]).unwrap();
    assert_eq!(oid, "2.5.4.3");
}

#[test]
fn oid_round_trip_long_arc() {
    // 1.2.840.113549.1.1.1 (rsaEncryption)
    let der = encode_oid("1.2.840.113549.1.1.1").unwrap();
    let (tag, hdr, content_len, _) = read_header(&der).unwrap();
    assert_eq!(tag, TAG_OID);
    let back = read_oid(&der[hdr..hdr + content_len]).unwrap();
    assert_eq!(back, "1.2.840.113549.1.1.1");
}

#[test]
fn pick_string_tag_basic() {
    assert_eq!(pick_string_tag("Test"), TAG_PRINTABLE);
    assert_eq!(pick_string_tag("Acme Corp"), TAG_PRINTABLE);
    // Non-printable: contains '!'
    assert_eq!(pick_string_tag("hi!"), TAG_UTF8_STRING);
    // UTF-8 chars
    assert_eq!(pick_string_tag("Acmé"), TAG_UTF8_STRING);
}

#[test]
fn read_header_rejects_indefinite() {
    let data = vec![0x30, 0x80];
    assert_eq!(read_header(&data), Err(DerError::BadLength));
}

#[test]
fn directory_string_utf8_and_bmp() {
    assert_eq!(
        read_directory_string(TAG_UTF8_STRING, b"hello").as_deref(),
        Some("hello")
    );
    // BMP UCS-2BE for "hi"
    assert_eq!(
        read_directory_string(TAG_BMP, &[0x00, b'h', 0x00, b'i']).as_deref(),
        Some("hi")
    );
}
