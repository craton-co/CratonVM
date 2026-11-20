use super::{
    scoped_screened_shift_bytes, SCOPED_PRIM_ARRAY_TAG_LO, SCOPED_PRIM_ARRAY_TAG_SPAN,
};

/// The screened arm's `(tag >> 2) & 3` is the full screen's `((tag - lo)
/// >> 2) & 3` for every tag the full screen admits -- the only tags a
/// screened base can carry.
#[test]
fn the_screened_shift_equals_the_full_screens() {
    let mut seen = 0;
    for tag in 0..=u8::MAX {
        let biased = u32::from(tag).wrapping_sub(u32::from(SCOPED_PRIM_ARRAY_TAG_LO));
        if biased & !SCOPED_PRIM_ARRAY_TAG_SPAN != 0 {
            continue;
        }
        seen += 1;
        assert_eq!(
            (u32::from(tag) >> 2) & 3,
            (biased >> 2) & 3,
            "tag {tag:#04x}"
        );
    }
    assert_eq!(seen, 8);
}

/// `MOVZX ECX, BYTE [RDX + disp8]` (ModRM 01 001 010), `SHR ECX, 2`,
/// `AND ECX, 3`, with the tag byte's offset as the displacement.
#[test]
fn the_screened_shift_bytes_read_the_tag_byte_into_ecx() {
    let b = scoped_screened_shift_bytes();
    assert_eq!(&b[..3], &[0x0F, 0xB6, 0x4A]);
    assert_eq!(
        usize::from(b[3]),
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as usize
    );
    assert!(b[3] < 0x80, "a disp8 past 127 would read before the object");
    assert_eq!(&b[4..], &[0xC1, 0xE9, 0x02, 0x83, 0xE1, 0x03]);
}
