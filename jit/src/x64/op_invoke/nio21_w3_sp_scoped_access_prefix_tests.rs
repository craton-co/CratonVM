//! nio21 wave 3, lane L3 (W2L3-3): the single-pass `ScopedMemoryAccess`
//! prefix and the `Unsafe` prefix's off-heap arm.

use super::*;

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn the_sixteen_scoped_cells_map_to_their_shapes_and_rows() {
    let mut dh = crate::DirectHelperTable::EMPTY;
    dh.scoped_memory_get_int_internal = 0x100;
    dh.scoped_memory_put_long = 0x108;
    dh.scoped_memory_get_char = 0x110;
    dh.scoped_memory_get_short_internal = 0x118;
    let get_int = sp_scoped_access_of(&dh, 0x100).unwrap();
    assert_eq!(
        (get_int.width, get_int.is_put, get_int.ret_type()),
        (4, false, b'I')
    );
    assert_eq!(get_int.num_params(), 4);
    assert_eq!(get_int.unsafe_gate_bit(), 1 << 3);
    let put_long = sp_scoped_access_of(&dh, 0x108).unwrap();
    assert_eq!(
        (put_long.width, put_long.is_put, put_long.ret_type()),
        (8, true, b'V')
    );
    assert_eq!(put_long.num_params(), 5);
    assert_eq!(put_long.unsafe_gate_bit(), 1 << 9);
    assert_eq!(sp_scoped_access_of(&dh, 0x110).unwrap().ret_type(), b'C');
    assert_eq!(sp_scoped_access_of(&dh, 0x118).unwrap().ret_type(), b'S');
    // An unwired cell and an unknown entry.
    assert_eq!(sp_scoped_access_of(&dh, 0), None);
    assert_eq!(sp_scoped_access_of(&dh, 0x120), None);
    // The row: the instance arm with the declared shape only.
    let row = |e, recv, p, r| sp_thin_prefix_row(&dh, e, recv, p, r);
    assert_eq!(row(0x100, true, 4, b'I'), Some(SpThinPrefix::ScopedAccess));
    assert_eq!(row(0x108, true, 5, b'V'), Some(SpThinPrefix::ScopedAccess));
    assert_eq!(row(0x100, false, 4, b'I'), None);
    assert_eq!(row(0x100, true, 5, b'I'), None);
    assert_eq!(row(0x100, true, 4, b'J'), None);
}

#[test]
fn the_scoped_prefix_needs_its_admission_bit_and_frame_operands() {
    let mut c = super::super::emit::tests::test_compiler();
    c.direct_helpers.scoped_memory_get_int_internal = 0x2000; // baked, never executed
    c.direct_helpers.scoped_memory_put_int = 0x3000;
    let get_args: Vec<StackSlot> = (1..=5).map(|k| StackSlot::Frame(8 * k)).collect();
    let start = c.buf.pos();
    // No admission word: nothing.
    c.direct_helpers.unsafe_inline_admitted = 0;
    assert!(c
        .try_emit_sp_scoped_access_prefix(0, 0x2000, &get_args)
        .is_none());
    // Every bit but `getInt`'s: nothing.
    c.direct_helpers.unsafe_inline_admitted = 0x3FF & !(1 << 3);
    assert!(c
        .try_emit_sp_scoped_access_prefix(0, 0x2000, &get_args)
        .is_none());
    c.direct_helpers.unsafe_inline_admitted = 0x3FF;
    // An unknown helper, the wrong operand count, an XMM operand.
    assert!(c
        .try_emit_sp_scoped_access_prefix(0, 0x4000, &get_args)
        .is_none());
    assert!(c
        .try_emit_sp_scoped_access_prefix(0, 0x2000, &get_args[..4])
        .is_none());
    let mut xmm = get_args.clone();
    xmm[3] = StackSlot::Xmm(1);
    assert!(c
        .try_emit_sp_scoped_access_prefix(0, 0x2000, &xmm)
        .is_none());
    assert_eq!(c.buf.pos(), start);
    let on = sp_thin_prefix_enabled() && sp_scoped_access_prefix_enabled();
    let hit = c.try_emit_sp_scoped_access_prefix(0, 0x2000, &get_args);
    assert_eq!(hit.is_some(), on);
    if !on {
        return;
    }
    let get = c.buf.as_slice()[start..].to_vec();
    // The heap arm's address, the int read, its run-time swap and widening.
    assert!(has(&get, &[0x4A, 0x8D, 0x14, 0x18]), "LEA RDX, [RAX+R11]");
    assert!(has(&get, &[0x8B, 0x02]), "MOV EAX, [RDX]");
    assert!(has(&get, &[0x0F, 0xC8]), "BSWAP EAX");
    assert!(has(&get, &[0x48, 0x63, 0xC0]), "MOVSXD RAX, EAX");
    // `test_compiler` has no arena table: a handle declines (JNZ after
    // TEST R11, RAX), a real pointer is used as is.
    assert!(has(&get, &[0x49, 0x85, 0xC3, 0x0F, 0x85]));
    assert!(has(&get, &[0x4C, 0x89, 0xDA]), "MOV RDX, R11");
    // A put: the value is stored at RDX.
    let mid = c.buf.pos();
    let put_args: Vec<StackSlot> = (1..=6).map(|k| StackSlot::Frame(8 * k)).collect();
    assert!(c
        .try_emit_sp_scoped_access_prefix(0, 0x3000, &put_args)
        .is_some());
    let put = c.buf.as_slice()[mid..].to_vec();
    assert!(has(&put, &[0x89, 0x02]), "MOV [RDX], EAX");
}

