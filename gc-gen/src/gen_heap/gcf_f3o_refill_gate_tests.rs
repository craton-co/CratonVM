// gcg g4/jn (2026-10-02): the lock-free gate test went with
// `CRATONVM_GC_REFILL_GATE_LOCKFREE`; the arena bound it read is still
// `pub` (`Arena::free_block_upper_bound`), so its soundness test stays.

/// gcf f3/o: the arena's published bound is never below a block it can
/// serve (`has_free_block_at_least` implies `size <= bound`).
#[test]
fn gcf_f3o_free_block_upper_bound_is_sound() {
    let mut arena = cratonvm_gc_core::arena::Arena::new(256 * 1024);
    assert_eq!(arena.free_block_upper_bound(), 0);
    arena.alloc(256 * 1024, 8).expect("exhaust the bump tail");
    arena.add_free_block(0, 64 * 1024);
    arena.add_free_block(128 * 1024, 512);
    for size in [8usize, 512, 4096, 64 * 1024, 64 * 1024 + 8] {
        if arena.has_free_block_at_least(size) {
            assert!(size <= arena.free_block_upper_bound(), "size {size}");
        }
    }
    assert!(arena.free_block_upper_bound() >= 64 * 1024);
}
