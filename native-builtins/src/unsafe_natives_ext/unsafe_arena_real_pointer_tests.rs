//! ADDR21-1 Option A (`CRATONVM_UNSAFE_REAL_POINTERS`): a store in
//! real-pointer mode keys every block by its storage's real address. Each
//! test builds its own store, so the process store's mode does not matter.
use super::unsafe_arena::ArenaStore;

#[test]
fn a_block_is_named_by_its_own_16_aligned_storage() {
    let store = ArenaStore::with_mode(true);
    assert!(store.real_pointers());
    for size in [1usize, 7, 8, 16, 24, 100, 4096] {
        let a = store.try_allocate(size).expect("block");
        assert_eq!(a & super::unsafe_arena::ARENA_TAG, 0, "a real address, not a handle");
        assert!(!store.jit_published(a), "the JIT table stays empty");
        assert_eq!(store.jit_resolve(a, 1), None, "only a handle resolves through it");
        assert_eq!(a % 16, 0, "malloc's alignment, which the JDK relies on");
        let (ptr, base, len, key) = store.peek(a, size).expect("live");
        assert_eq!(
            (ptr as i64, base as i64, len, key),
            (a, a, size, a),
            "the key IS the storage"
        );
        // The bytes C sees through the address are the bytes the store holds.
        assert!(store.copy_in(a + (size as i64 - 1), &[0xA5]));
        // SAFETY: `a` is the live block's storage, `size` bytes long.
        assert_eq!(unsafe { *((a as usize + size - 1) as *const u8) }, 0xA5);
        assert!(store.contains(a) && store.contains(a + size as i64 - 1));
        assert!(!store.contains(a + size as i64), "the end is exclusive");
        assert_eq!(store.handle_of_real(a as usize), None, "nothing to map back");
        store.free(a);
        assert!(!store.contains(a), "a freed block's address is nobody's");
    }
}

#[test]
fn a_zero_length_block_still_has_an_address_of_its_own() {
    let store = ArenaStore::with_mode(true);
    let a = store.try_allocate(0).expect("block");
    let b = store.try_allocate(0).expect("block");
    assert!(a > 0 && b > 0 && a != b, "distinct, as malloc(0)'s are");
    assert!(store.peek(a, 0).is_some());
    store.free(a);
    store.free(b);
}

#[test]
fn reallocate_keeps_the_address_within_the_allocation_and_moves_past_it() {
    let store = ArenaStore::with_mode(true);
    let a = store.try_allocate(64).expect("block");
    assert!(store.copy_in(a, &[1, 2, 3, 4, 5, 6, 7, 8]));
    // Shrink: in place, the same address, the prefix kept.
    let same = store.try_reallocate(a, 8).expect("shrink");
    assert_eq!(same, a);
    // Regrow within the allocation: in place, and the regrown bytes are zero.
    let again = store.try_reallocate(a, 32).expect("regrow");
    assert_eq!(again, a);
    let mut out = [0xFFu8; 32];
    assert!(store.copy_out(a, &mut out));
    assert_eq!(&out[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(out[8..].iter().all(|&b| b == 0), "zero-filled like Vec::resize");
    // Past the allocation: a new address (the old key dies), contents copied.
    let moved = store.try_reallocate(a, 4096).expect("grow");
    assert_ne!(moved, a);
    assert_eq!(moved % 16, 0);
    assert!(
        !matches!(store.peek(a, 1), Some((_, base, _, _)) if base as i64 == a),
        "the old address names no block of its own"
    );
    let mut head = [0u8; 8];
    assert!(store.copy_out(moved, &mut head));
    assert_eq!(head, [1, 2, 3, 4, 5, 6, 7, 8]);
    store.free(moved);
}

/// The atomics' one-walk bounds check (`unsafe_arena_access_ptr`): inside a
/// live block with `want` bytes to its end, nothing else, and never in
/// handle mode (whose callers translate the tagged handle instead).
#[test]
fn real_mode_covers_is_a_bounded_live_block_test() {
    let store = ArenaStore::with_mode(true);
    let a = store.try_allocate(16).expect("block");
    assert!(store.real_mode_covers(a, 8) && store.real_mode_covers(a + 8, 8));
    assert!(store.real_mode_covers(a + 15, 1));
    assert!(!store.real_mode_covers(a + 9, 8), "past the end");
    assert!(!store.real_mode_covers(a + 16, 1), "the end is exclusive");
    assert!(!store.real_mode_covers(a - 1, 1), "before the block");
    assert!(!store.real_mode_covers(0, 1) && !store.real_mode_covers(-8, 1));
    store.free(a);
    assert!(!store.real_mode_covers(a, 1), "a freed block's address is nobody's");
    let handles = ArenaStore::with_mode(false);
    let h = handles.try_allocate(16).expect("block");
    assert!(!handles.real_mode_covers(h, 1), "handle mode never answers it");
    handles.free(h);
}

#[test]
fn handle_mode_is_unchanged() {
    let store = ArenaStore::with_mode(false);
    assert!(!store.real_pointers());
    let h = store.try_allocate(64).expect("block");
    assert_ne!(h & super::unsafe_arena::ARENA_TAG, 0, "a tagged handle");
    let (ptr, ..) = store.peek(h, 64).expect("live");
    assert_eq!(ptr as usize % 16, 0, "the storage is 16-aligned in both modes");
    assert!(!store.contains(ptr as i64), "a real address is not a handle");
    assert_eq!(store.handle_of_real(ptr as usize), Some(h));
    // Within the handle space: the handle survives a grow past the allocation.
    let grown = store.try_reallocate(h, 64).expect("same size");
    assert_eq!(grown, h);
    store.free(h);
}
