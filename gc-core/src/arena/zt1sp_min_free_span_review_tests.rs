//! zt t1/sp review of the s10/sp fit predicate (`Arena::set_min_free_span`):
//! the block a fit chooses keeps serving from its rest, and the two
//! free-list exits the s10/sp cut left without the predicate (the high
//! end's last resort, the smallest-whole-block refill) now honour it.
use super::*;

/// A full arena (bump tail spent) whose only free block is a 4 KiB span.
fn full_with_one_span() -> Arena {
    let mut arena = Arena::new(64 * 1024);
    let cap = arena.capacity();
    assert!(arena.alloc(cap, 8).is_some());
    arena.add_free_block(8192, 4096);
    arena
}

/// With the `on` floor (2048), two 16-byte requests are served back to
/// back from the span and its rest stays ONE block: the s10/sp cut
/// pushed each fit's excess separately, so the second request was cut
/// from a new 2048-byte unit and 2032 bytes per object were set aside.
#[test]
fn a_fit_keeps_the_rest_of_its_block_in_one_piece() {
    let mut arena = full_with_one_span();
    let base = arena.base_ptr() as usize;
    arena.set_min_free_span(2048);
    let p1 = arena.alloc(16, 8).expect("the span") as usize - base;
    let p2 = arena.alloc(16, 8).expect("the rest of the span") as usize - base;
    assert_eq!((p1, p2), (8192, 8208), "bumped inside one block");
    assert_eq!(arena.free_list_bytes(), 4096 - 32);
    assert_eq!(
        arena.largest_free_block(),
        4096 - 32,
        "one block, not slivers"
    );
}

/// The control of the test above: off, the same two requests land at the
/// same offsets (the predicate changes which block is chosen, never how
/// a chosen block is cut).
#[test]
fn off_the_same_requests_land_at_the_same_offsets() {
    let mut arena = full_with_one_span();
    let base = arena.base_ptr() as usize;
    let p1 = arena.alloc(16, 8).expect("the span") as usize - base;
    let p2 = arena.alloc(16, 8).expect("the rest") as usize - base;
    assert_eq!((p1, p2), (8192, 8208));
    assert_eq!(arena.largest_free_block(), 4096 - 32);
}

/// The high end's free list is `alloc`'s last resort for a small object.
/// A 4 KiB high hole below an 8 KiB threshold serves nothing; turning the
/// rule off serves the request from it (the kill control).
#[test]
fn a_high_end_hole_below_the_threshold_does_not_serve() {
    let mut arena = Arena::new(8192);
    let big = arena.alloc_high(4096, 8).unwrap() as usize - arena.base_ptr() as usize;
    arena.add_free_block(big, 4096);
    assert_eq!(arena.high_free_shape(), (1, 4096, 4096));
    arena.alloc(2048, 8).unwrap();
    arena.alloc(2048, 8).unwrap();
    assert_eq!(arena.low_bump_headroom(), 0, "the two ends have met");
    arena.set_min_free_span(8192);
    assert!(
        arena.alloc(64, 8).is_none(),
        "the high hole is below the threshold"
    );
    assert_eq!(
        arena.high_free_shape(),
        (1, 4096, 4096),
        "and it stays listed"
    );
    arena.set_min_free_span(0);
    let p = arena.alloc(64, 8).expect("re-admitted") as usize - arena.base_ptr() as usize;
    assert!(p >= arena.high_cursor());
}

/// `take_smallest_block_at_least` (the gci i22/dr refill primitive) skips
/// blocks below the threshold even when `min` admits them.
#[test]
fn the_smallest_whole_block_refill_honours_the_threshold() {
    let mut arena = Arena::new(1024 * 1024);
    let base = arena.base_ptr() as usize;
    let small = arena.alloc(1024, 8).expect("hole") as usize - base;
    let _fence = arena.alloc(64, 8).expect("fence");
    let large = arena.alloc(2048, 8).expect("hole") as usize - base;
    let _fence = arena.alloc(64, 8).expect("fence");
    arena.add_free_block(small, 1024);
    arena.add_free_block(large, 2048);
    arena.set_min_free_span(2048);
    let (p, n) = arena
        .take_smallest_block_at_least(1000, 4096)
        .expect("the 2048-byte hole");
    assert_eq!((p as usize - base, n), (large, 2048));
    assert!(arena.take_smallest_block_at_least(1000, 4096).is_none());
    arena.set_min_free_span(0);
    let (p, n) = arena
        .take_smallest_block_at_least(1000, 4096)
        .expect("the 1024-byte hole, re-admitted");
    assert_eq!((p as usize - base, n), (small, 1024));
}
