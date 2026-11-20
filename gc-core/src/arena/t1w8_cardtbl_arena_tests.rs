use super::*;
use crate::reservation::GRANULE;

#[test]
fn t1w8_cardtbl_headroom_arena_grows_without_moving_its_base() {
    let Some(mut a) = Arena::with_headroom(GRANULE, 4 * GRANULE) else {
        // A host that refuses reservations (or the reserve kill switch)
        // keeps the historical arena; nothing to check.
        return;
    };
    assert_eq!(a.capacity(), GRANULE);
    assert!(a.in_place_capacity() >= 4 * GRANULE);
    let base = a.base_ptr();
    a.grow(4 * GRANULE);
    assert_eq!(a.capacity(), 4 * GRANULE);
    assert_eq!(a.base_ptr(), base, "growth inside the reservation is in place");
    let p = a.alloc(64, 8).expect("an allocation in the grown arena");
    assert!(p as usize >= base as usize && (p as usize) < base as usize + 4 * GRANULE);
}

#[test]
fn t1w8_cardtbl_headroom_arena_refuses_a_reserve_at_or_below_capacity() {
    assert!(Arena::with_headroom(GRANULE, GRANULE).is_none());
    assert!(Arena::with_headroom(2 * GRANULE, GRANULE).is_none());
    let plain = Arena::new(GRANULE);
    assert!(plain.in_place_capacity() >= plain.capacity());
}
