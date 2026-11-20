use super::*;
use crate::reservation::GRANULE;

/// A grow the OS refuses (no address space for a bigger reservation)
/// leaves the arena exactly as it was. Before the fix `grow` set
/// `high_cursor` to the REQUEST while `HeapStore::grow_to` kept the old
/// length, so `used()` (`len - high_cursor`) underflowed and `remaining()`
/// promised bytes the arena did not have. 1 EiB is past every 64-bit
/// user address space (57-bit LA57 included), so the reservation fails.
/// Only on the reserving store: the wholly committed fallback grows by
/// `Vec::resize`, which would abort instead of refusing.
#[test]
fn a_refused_grow_leaves_the_arena_unchanged() {
    let mut arena = Arena::new(2 * GRANULE);
    arena.set_uncommit_policy(0, true, std::time::Duration::ZERO);
    if arena.data.commit_bits().is_none() {
        return;
    }
    let cap = arena.capacity();
    let base = arena.base_ptr();
    let _ = arena.grow(1usize << 60);
    assert_eq!(arena.capacity(), cap, "a refused reservation keeps the old capacity");
    assert_eq!(arena.base_ptr(), base, "and the old base");
    assert_eq!(arena.high_cursor(), cap, "the high end stays at the real capacity");
    assert_eq!(arena.used(), 0, "an empty arena still reads empty");
    assert_eq!(arena.remaining(), cap, "and offers only what it has");
    assert!(arena.alloc(64, 8).is_some(), "and still allocates");
}

/// The ordinary grow inside and past the reservation still lands on the
/// requested (8-aligned) capacity with the high end at the top.
#[test]
fn an_admitted_grow_reaches_the_request() {
    let mut arena = Arena::new(2 * GRANULE);
    let _ = arena.grow(4 * GRANULE + 12);
    assert_eq!(arena.capacity(), 4 * GRANULE + 8);
    assert_eq!(arena.high_cursor(), arena.capacity());
    assert_eq!(arena.used(), 0);
    assert_eq!(arena.remaining(), arena.capacity());
}
