use super::*;

/// JIT round 21, lane unsafe: a tagged cell-table word adds the byte
/// offset decode (SUB 16 / TEST 7 / JNZ / SHR 3) ahead of the slot
/// screen, strips the tag from the baked table address, and leaves the
/// untagged screen byte for byte as before.
#[test]
fn a_tagged_table_word_decodes_the_byte_offset_first() {
    let cells = 0x1122_3344_5566usize;
    let (plain, plain_edges) = unsafe_inline_field_cell_bytes(cells, 0x7788, 4);
    let (tagged, tagged_edges) =
        unsafe_inline_field_cell_bytes(cells | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG, 0x7788, 4);
    let prefix: [u8; 18] = [
        0x49, 0x83, 0xEB, 0x10, // SUB R11, 16
        0x41, 0xF6, 0xC3, 0x07, // TEST R11B, 7
        0x0F, 0x85, 0, 0, 0, 0, // JNZ rel32
        0x49, 0xC1, 0xEB, 0x03, // SHR R11, 3
    ];
    assert_eq!(&tagged[..18], &prefix);
    assert_eq!(&tagged[18..], &plain[..], "the slot screen is unchanged");
    assert_eq!(tagged_edges.len(), plain_edges.len() + 1);
    assert_eq!(tagged_edges[0], 10);
    let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);
    assert!(
        has(
            &tagged,
            &[0x49, 0xBA, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0, 0]
        ),
        "MOV R10, cells (the tag stripped)"
    );
    let (ref_tagged, _) =
        unsafe_inline_ref_field_cell_bytes(cells | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG, 0x7788);
    assert_eq!(&ref_tagged[..18], &prefix);
}
