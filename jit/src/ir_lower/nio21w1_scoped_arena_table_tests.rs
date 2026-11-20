use super::{
    scoped_arena_jit_table, SCOPED_ARENA_ENTRY_SHIFT, SCOPED_ARENA_INDEX_BITS,
    SCOPED_ARENA_SLOT_SHIFT, WELL_KNOWN_UNSAFE_ARENA_JIT_TABLE,
};

unsafe extern "C" fn table_at_0x10000(ctx: i64, which: i64) -> i64 {
    match (ctx, which) {
        (0x1000, WELL_KNOWN_UNSAFE_ARENA_JIT_TABLE) => 0x10000,
        // Every other selector must not be mistaken for the table.
        _ => 0x20,
    }
}

unsafe extern "C" fn misaligned(_ctx: i64, _which: i64) -> i64 {
    0x10008
}

unsafe extern "C" fn table_off(_ctx: i64, _which: i64) -> i64 {
    0
}

fn table(f: unsafe extern "C" fn(i64, i64) -> i64, ctx: usize) -> crate::DirectHelperTable {
    crate::DirectHelperTable {
        well_known_class_id: f as *const () as usize,
        static_base_resolver_ctx: ctx,
        ..crate::DirectHelperTable::EMPTY
    }
}


/// The handle arithmetic the emitted resolve does, in Rust: the index and
/// key of a slotted handle, and that a LEGACY handle (bit 61) can never
/// produce a key equal to any slotted one, so it always misses.
#[test]
fn a_legacy_handle_never_matches_a_slotted_key() {
    let tag: i64 = 1 << 62;
    let window = (1i64 << SCOPED_ARENA_SLOT_SHIFT) - 1;
    let mask = (1i64 << SCOPED_ARENA_INDEX_BITS) - 1;
    for seq in [1i64, 2, 0x7FFF, 0x8000, 0x8001, (1 << 31) - 1] {
        let handle = tag | (seq << SCOPED_ARENA_SLOT_SHIFT);
        assert_eq!(handle & (1 << 61), 0, "a slotted handle keeps bit 61 clear");
        let a = handle + 0x3FFF_FFF0;
        assert_eq!(
            a & !window,
            handle,
            "every offset in the window keys the block"
        );
        assert_eq!((a >> SCOPED_ARENA_SLOT_SHIFT) & mask, seq & mask);
        assert_eq!(
            (((a >> SCOPED_ARENA_SLOT_SHIFT) & mask) << SCOPED_ARENA_ENTRY_SHIFT) % 32,
            0
        );
    }
    let legacy = tag | (1 << 61) | 0x10_0000_0000;
    assert_ne!((legacy + 64) & !window & (1 << 61), 0);
}
