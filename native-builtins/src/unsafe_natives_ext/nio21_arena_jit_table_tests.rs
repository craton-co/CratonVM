use super::*;

fn on() -> bool {
    unsafe_arena_jit_table_addr() != 0
}

fn real(addr: i64, want: usize) -> usize {
    unsafe_arena::store()
        .peek(addr, want)
        .map(|(p, ..)| p as usize)
        .expect("live handle")
}

/// A small block gets a SLOTTED handle (bit 62 set, bit 61 clear, low 30
/// bits clear), is published, and every in-bounds access resolves to the
/// same real byte the locked slow path names; the first out-of-bounds one
/// does not.
#[test]
fn a_small_block_is_published_and_resolves_like_the_slow_path() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena = super::arena_layout_test_lock();
    if !on() {
        return;
    }
    let h = unsafe_arena_allocate(64);
    assert!(unsafe_arena_addr_is_tagged(h));
    assert_eq!(h & (1 << 61), 0, "a 64-byte block is slotted");
    assert_eq!(h & ((1 << 30) - 1), 0, "a slotted base is window-aligned");
    assert!(unsafe_arena::store().jit_published(h));
    for (off, width) in [(0i64, 1u64), (0, 8), (60, 4), (56, 8), (63, 1)] {
        assert_eq!(
            unsafe_arena::store().jit_resolve(h + off, width),
            Some(real(h + off, width as usize)),
            "offset {off} width {width}"
        );
    }
    for (off, width) in [(61i64, 4u64), (57, 8), (64, 1), (1 << 20, 1)] {
        assert_eq!(
            unsafe_arena::store().jit_resolve(h + off, width),
            None,
            "offset {off} width {width} is past the block"
        );
    }
    // A write through the resolved address is the block's own byte.
    let p = unsafe_arena::store()
        .jit_resolve(h + 9, 1)
        .expect("resolves");
    // SAFETY: the block is live and 64 bytes long; offset 9 is inside it.
    unsafe { std::ptr::write(p as *mut u8, 0x5C) };
    assert_eq!(unsafe_arena_get_byte(h + 9), 0x5C);
    unsafe_arena_free(h);
}

/// `free` withdraws the block before it dies, so a use-after-free misses
/// inline exactly as it misses on the slow path; and the key is never
/// handed out again (a later block lands at a strictly greater handle).
#[test]
fn free_unpublishes_and_a_key_never_returns() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena = super::arena_layout_test_lock();
    if !on() {
        return;
    }
    let h = unsafe_arena_allocate(32);
    assert!(unsafe_arena::store().jit_published(h));
    unsafe_arena_free(h);
    assert!(!unsafe_arena::store().jit_published(h));
    assert_eq!(unsafe_arena::store().jit_resolve(h, 1), None);
    assert!(!unsafe_arena_contains(h), "and the slow path agrees");
    let mut last = h;
    for _ in 0..(1 << 16) + 7 {
        // More allocations than the table has entries: entries are
        // reused, keys never.
        let n = unsafe_arena_allocate(16);
        if n & (1 << 61) == 0 {
            assert!(
                n > last,
                "slotted handles only grow: {n:#x} after {last:#x}"
            );
            last = n;
        }
        unsafe_arena_free(n);
    }
    assert_eq!(unsafe_arena::store().jit_resolve(h, 1), None);
}

/// A resize withdraws the block: in place it keeps its handle on the slow
/// path only; a relocation publishes the new handle.
#[test]
fn a_resize_unpublishes_and_a_relocation_republishes() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena = super::arena_layout_test_lock();
    if !on() {
        return;
    }
    let h = unsafe_arena_allocate(60);
    assert!(unsafe_arena_put_byte(h + 3, 0x2B));
    let same = unsafe_arena_try_reallocate(h, 64).expect("resize succeeds");
    assert_eq!(same, h);
    assert!(
        !unsafe_arena::store().jit_published(h),
        "in place: withdrawn"
    );
    assert_eq!(unsafe_arena::store().jit_resolve(h + 3, 1), None);
    assert_eq!(
        unsafe_arena_get_byte(h + 3),
        0x2B,
        "the slow path still serves it"
    );
    let moved = unsafe_arena_try_reallocate(h, 4096).expect("resize succeeds");
    assert_ne!(moved, h);
    assert!(unsafe_arena::store().jit_published(moved));
    assert_eq!(
        unsafe_arena::store().jit_resolve(moved + 3, 1),
        Some(real(moved + 3, 1))
    );
    assert_eq!(unsafe_arena_get_byte(moved + 3), 0x2B);
    unsafe_arena_free(moved);
}

/// A zero-length block takes a LEGACY handle (bit 61), which the table
/// never answers for; the tag still classifies it.
#[test]
fn a_legacy_handle_is_never_resolved_inline() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena = super::arena_layout_test_lock();
    let h = unsafe_arena_allocate(0);
    assert!(unsafe_arena_addr_is_tagged(h));
    assert_ne!(h & (1 << 61), 0);
    assert_eq!(unsafe_arena::store().jit_resolve(h, 0), None);
    unsafe_arena_free(h);
}

/// The published layout constants are the struct's.
#[test]
fn the_published_layout_is_the_table_entry() {
    assert_eq!(UNSAFE_ARENA_JIT_SLOT_SHIFT, 30);
    assert_eq!(UNSAFE_ARENA_JIT_INDEX_BITS, 15);
    assert_eq!(UNSAFE_ARENA_JIT_ENTRY_SHIFT, 5);
    assert_eq!(
        (
            UNSAFE_ARENA_JIT_KEY_OFFSET,
            UNSAFE_ARENA_JIT_LEN_OFFSET,
            UNSAFE_ARENA_JIT_PTR_OFFSET
        ),
        (0, 8, 16)
    );
    if on() {
        assert_eq!(
            unsafe_arena_jit_table_addr() % 32,
            0,
            "entries are 32-aligned"
        );
    }
}
