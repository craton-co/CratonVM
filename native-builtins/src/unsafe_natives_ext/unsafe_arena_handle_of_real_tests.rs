//! `ffm-address-read-back-of-an-arena-pointer-is-the-real-address-not-the-handle`:
//! the real address a store of an arena segment writes reads back as the
//! block's handle while the block lives, through every resize.
use super::*;

fn real_of(handle: i64) -> usize {
    unsafe_arena_peek(handle, 0).expect("live block").1
}

#[test]
fn a_live_blocks_real_addresses_name_its_handle_until_it_is_freed() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let h = unsafe_arena_try_allocate(64).expect("block");
    let real = real_of(h);
    assert_eq!(unsafe_arena_handle_of_real(real), Some(h));
    assert_eq!(unsafe_arena_handle_of_real(real + 24), Some(h + 24));
    assert_eq!(unsafe_arena_handle_of_real(real + 64), Some(h + 64), "one past the end");
    assert_eq!(unsafe_arena_handle_of_real(0x1000), None);

    // In place (within the reserved handle space): the same handle.
    let same = unsafe_arena_try_reallocate(h, 32).expect("shrink");
    assert_eq!(same, h);
    assert_eq!(unsafe_arena_handle_of_real(real_of(h) + 8), Some(h + 8));

    // Past the reserved space: a fresh handle, which the new storage names.
    let grown = unsafe_arena_try_reallocate(h, 4096).expect("grow");
    assert_ne!(grown, h);
    let grown_real = real_of(grown);
    assert_eq!(unsafe_arena_handle_of_real(grown_real + 100), Some(grown + 100));

    unsafe_arena_free(grown);
    // Handles are never reused, so the dead one is never answered again
    // (the memory itself may already be another test's live block).
    assert_ne!(unsafe_arena_handle_of_real(grown_real + 100), Some(grown + 100));
}
