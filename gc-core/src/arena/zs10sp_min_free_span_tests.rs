//! zs s10/sp: `Arena::set_min_free_span`, the fit predicate behind
//! `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`.
use super::*;

/// A full arena (bump tail spent) whose free list holds one 16-byte
/// remainder, one 32-byte hole and one 4 KiB span.
fn full_with_holes() -> Arena {
    let mut arena = Arena::new(64 * 1024);
    let cap = arena.capacity();
    assert!(arena.alloc(cap, 8).is_some());
    arena.add_free_block(1024, 16);
    arena.add_free_block(2048, 32);
    arena.add_free_block(8192, 4096);
    arena
}

/// The positive control: off (`0`), the 16-byte request is served from the
/// 16-byte remainder -- the CratonVM shape the L4 probes enter `main`
/// through.
#[test]
fn off_a_sixteen_byte_request_takes_the_remainder() {
    let mut arena = full_with_holes();
    let base = arena.data.as_ptr() as usize;
    let p = arena.alloc(16, 8).expect("served") as usize;
    assert_eq!(p - base, 1024, "the 16-byte remainder");
}

/// On (`T = 32`, the refused request's size): neither the 16-byte
/// remainder may serve it; the 32-byte hole may (it is not smaller than
/// T), and the excess goes back on the list. With nothing of at least T
/// left, the request is refused (HotSpot's shape after a failed refill),
/// and turning the rule off serves it again: nothing was lost.
#[test]
fn on_no_block_smaller_than_the_threshold_serves() {
    let mut arena = full_with_holes();
    let base = arena.data.as_ptr() as usize;
    let free_before = arena.free_list_bytes();
    arena.set_min_free_span(32);
    let p = arena.alloc(16, 8).expect("the 32-byte hole") as usize;
    assert_eq!(p - base, 2048);
    assert_eq!(
        arena.free_list_bytes(),
        free_before - 16,
        "the excess went back"
    );
    // T = 8 KiB: only fresh bump space would do; there is none.
    arena.set_min_free_span(8 * 1024);
    assert!(
        arena.alloc(16, 8).is_none(),
        "refused: no block of at least T"
    );
    assert!(arena.alloc(24, 8).is_none());
    arena.set_min_free_span(0);
    assert!(
        arena.alloc(16, 8).is_some(),
        "re-admitted: the blocks never left"
    );
}

/// A bump TAIL smaller than T is a hole too: with 16 bytes of bump room
/// left, T = 32 refuses a 16-byte request the tail would serve off.
#[test]
fn a_bump_tail_smaller_than_the_threshold_does_not_serve() {
    let mut arena = Arena::new(64 * 1024);
    let cap = arena.capacity();
    assert!(
        arena.alloc(cap - 16, 8).is_some(),
        "16 bytes of bump room left"
    );
    arena.set_min_free_span(32);
    assert!(
        arena.alloc(16, 8).is_none(),
        "the 16-byte tail is set aside"
    );
    arena.set_min_free_span(0);
    assert!(
        arena.alloc(16, 8).is_some(),
        "and serves again when re-admitted"
    );
}

/// A request at least as large as T is unaffected, and the rule never
/// touches a bump region of at least T.
#[test]
fn requests_at_or_above_the_threshold_and_bump_space_are_unaffected() {
    let mut arena = full_with_holes();
    arena.set_min_free_span(32);
    assert!(
        arena.alloc(4096, 8).is_some(),
        "the span serves a big request"
    );
    let mut fresh = Arena::new(64 * 1024);
    fresh.set_min_free_span(2048);
    assert!(
        fresh.alloc(16, 8).is_some(),
        "bump space serves small requests"
    );
    assert_eq!(fresh.min_free_span(), 2048);
    fresh.set_min_free_span(13);
    assert_eq!(fresh.min_free_span(), 16, "rounded to the object grid");
}
