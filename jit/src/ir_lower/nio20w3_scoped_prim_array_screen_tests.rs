use super::{SCOPED_PRIM_ARRAY_TAG_LO, SCOPED_PRIM_ARRAY_TAG_SPAN};

/// What the emitted `SUB EAX, lo; TEST EAX, !span; JZ` admits, and the
/// `SHR 2; AND 3` it then derives, in Rust.
fn screen(tag: u8) -> Option<u32> {
    let biased = u32::from(tag).wrapping_sub(u32::from(SCOPED_PRIM_ARRAY_TAG_LO));
    (biased & !SCOPED_PRIM_ARRAY_TAG_SPAN == 0).then_some((biased >> 2) & 3)
}

#[test]
fn scoped_prim_array_tag_byte_screen_matches_the_layout() {
    use cratonvm_types::{element_byte_size, primitive_array_kind_tags_byte, ArrayElementType};
    let prims = [
        ("[Z", ArrayElementType::Boolean),
        ("[C", ArrayElementType::Char),
        ("[F", ArrayElementType::Float),
        ("[D", ArrayElementType::Double),
        ("[B", ArrayElementType::Byte),
        ("[S", ArrayElementType::Short),
        ("[I", ArrayElementType::Int),
        ("[J", ArrayElementType::Long),
    ];
    let mut admitted = 0;
    for (desc, et) in prims {
        let tag = primitive_array_kind_tags_byte(desc).expect("a primitive array tag");
        let log2 = screen(tag).unwrap_or_else(|| panic!("{desc} must pass the screen"));
        assert_eq!(
            1usize << log2,
            element_byte_size(et),
            "{desc}: element size"
        );
    }
    // Exactly those eight of the 256 byte values pass.
    for tag in 0..=u8::MAX {
        if screen(tag).is_some() {
            admitted += 1;
            assert!(
                prims
                    .iter()
                    .any(|(d, _)| primitive_array_kind_tags_byte(d) == Some(tag)),
                "tag {tag:#04x} passes the screen but is no primitive array"
            );
        }
    }
    assert_eq!(admitted, 8);
    // A reference array (element 0) and a plain object (kind 0) fail.
    assert!(screen(cratonvm_types::ObjectKind::Array as u8).is_none());
    assert!(screen(0).is_none());
}
