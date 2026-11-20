use super::*;


/// The screen's exact bytes: the tag byte, the band, the alignment and
/// the length bound, each declining through a `0F 8x rel32`.
#[test]
fn ref_array_screen_bytes_and_decline_edges() {
    let (b, edges) = unsafe_ref_array_screen_bytes();
    let kt = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;
    let len = cratonvm_types::ARRAY_LENGTH_OFFSET as u8;
    let mut want: Vec<u8> = vec![0x0F, 0xB6, 0x48, kt, 0x83, 0xF9, REF_ARRAY_KIND_TAGS_BYTE];
    want.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]);
    want.extend_from_slice(&[0x49, 0x83, 0xFB, 0x10, 0x0F, 0x8C, 0, 0, 0, 0]);
    want.extend_from_slice(&[0x49, 0x81, 0xFB, 0x00, 0x00, 0x00, 0x20]);
    want.extend_from_slice(&[0x0F, 0x8D, 0, 0, 0, 0]);
    want.extend_from_slice(&[0x4D, 0x8D, 0x53, 0xF0, 0x41, 0xF6, 0xC2, 0x07]);
    want.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]);
    want.extend_from_slice(&[0x8B, 0x50, len, 0x48, 0xC1, 0xE2, 0x03, 0x49, 0x39, 0xD2]);
    want.extend_from_slice(&[0x0F, 0x83, 0, 0, 0, 0]);
    assert_eq!(b, want);
    assert_eq!(edges.len(), 5);
    let ccs: Vec<u8> = edges.iter().map(|&e| b[e - 1]).collect();
    assert_eq!(ccs, [0x85, 0x8C, 0x8D, 0x85, 0x83]);
    for &e in &edges {
        assert_eq!(b[e - 2], 0x0F, "edge {e} is not a near Jcc");
        assert_eq!(&b[e..e + 4], &[0, 0, 0, 0]);
    }
}

/// A reference array of any dimension has this one tag byte; no
/// primitive array does.
#[test]
fn ref_array_tag_byte_is_the_reference_arrays_only() {
    assert_eq!(REF_ARRAY_KIND_TAGS_BYTE, 0x01);
    for d in ["[Z", "[C", "[F", "[D", "[B", "[S", "[I", "[J"] {
        assert_ne!(
            cratonvm_types::primitive_array_kind_tags_byte(d),
            Some(REF_ARRAY_KIND_TAGS_BYTE),
            "{d}"
        );        }    }