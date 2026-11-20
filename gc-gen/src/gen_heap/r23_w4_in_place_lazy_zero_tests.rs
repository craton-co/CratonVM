use super::*;

/// Every recorded start inside a dead span gets an all-zero header; the
/// rest of the dead bytes, and every start outside the spans, are left
/// alone; a start whose header would cross the span's end is clamped.
#[test]
fn r23_w4_poison_zeroes_exactly_the_dead_headers() {
    let mut words = vec![0xABAB_ABAB_ABAB_ABABu64; 512];
    let base = words.as_mut_ptr() as usize;
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(base, 4096);
    for off in [0usize, 64, 256, 512, 1000, 2048] {
        assert!(starts.insert(base + off));
    }
    // Dead: [64, 520) and [1000, 1008) (a span shorter than a header).
    let dead = [(64usize, 456usize), (1000, 8)];
    // SAFETY: the spans are inside `buf`, which nothing else touches.
    let n = unsafe { poison_dead_object_headers(base, &dead, &starts) };
    assert_eq!(n, 4, "64, 256, 512 and 1000");
    // SAFETY: `words` is 4096 initialized bytes, read only from here on.
    let buf = unsafe { std::slice::from_raw_parts(base as *const u8, 4096) };
    let zero = |o: usize, l: usize| buf[o..o + l].iter().all(|&b| b == 0);
    let stale = |o: usize, l: usize| buf[o..o + l].iter().all(|&b| b == 0xAB);
    assert!(zero(64, HEADER_SIZE) && zero(256, HEADER_SIZE));
    assert!(zero(512, 8) && stale(520, 8), "clamped to the span's end");
    assert!(zero(1000, 8) && stale(1008, 8));
    assert!(stale(64 + HEADER_SIZE, 64), "a dead payload is left for the hand-out");
    assert!(stale(0, HEADER_SIZE) && stale(2048, HEADER_SIZE), "live starts untouched");
}

/// r23 w4 review (R-L4z item 2): once a span is lazy-dirty and the mirror
/// is published, `is_object_address` declines a plausible header inside
/// it (here: a whole real header, the strongest forgery), keeps
/// accepting a live object outside it, and accepts again the moment a
/// hand-out takes the span (no republish needed).
#[test]
fn r23_w4_review_is_object_address_declines_inside_a_lazy_dirty_span() {
    let heap = GenerationalHeap::new();
    let dead = heap.alloc_object(ClassId::new(1), 3);
    let live = heap.alloc_object(ClassId::new(1), 3);
    let (d, l) = (dead.as_ptr() as usize, live.as_ptr() as usize);
    assert!(heap.is_object_address(d).is_some());
    // SAFETY: `dead` is a whole object of this heap.
    let size = gen_object_total_size(unsafe { &*(d as *const ObjectHeader) });
    let off = {
        let mut from = heap.lock_young_from();
        assert!(from.contains(d as *const u8), "allocated young");
        let off = d - from.base_ptr() as usize;
        from.note_lazy_dirty(&[(off, size)]);
        assert!(from.lazy_dirty_contains(d));
        off
    };
    heap.refresh_region_bounds();
    assert!(heap.is_object_address(d).is_none(), "a header inside the span is declined");
    assert!(heap.is_object_address(l).is_some(), "a live object outside it is not");
    assert!(heap.lock_young_from().take_lazy_dirty(off, size).is_some());
    assert!(heap.is_object_address(d).is_some(), "a hand-out clears the mirror at once");
}

/// `gen-jit-young-pauses-keep-both-weak-referents-again`: a dead object
/// inside a lazy-dirty span keeps its whole header (the lazy zero's point),
/// and `is_live_young_survivor` -- the young arm of the reference round's
/// survival verdict -- must still answer "reclaimed" for it; a live
/// neighbour stays a survivor, and a hand-out of the span ends the answer.
#[test]
fn a_dead_header_inside_a_lazy_dirty_span_is_not_a_young_survivor() {
    let heap = GenerationalHeap::new();
    let dead = heap.alloc_object(ClassId::new(1), 3);
    let live = heap.alloc_object(ClassId::new(1), 3);
    let (d, l) = (dead.as_ptr() as usize, live.as_ptr() as usize);
    assert!(heap.is_live_young_survivor(d), "the positive control: a whole header reads live");
    // SAFETY: `dead` is a whole object of this heap.
    let size = gen_object_total_size(unsafe { &*(d as *const ObjectHeader) });
    let off = {
        let mut from = heap.lock_young_from();
        let off = d - from.base_ptr() as usize;
        from.note_lazy_dirty(&[(off, size)]);
        off
    };
    assert!(!heap.is_live_young_survivor(d), "a lazily reclaimed corpse is not a survivor");
    assert!(heap.is_live_young_survivor(l), "its live neighbour is");
    assert!(heap.lock_young_from().take_lazy_dirty(off, size).is_some());
    assert!(heap.is_live_young_survivor(d), "a hand-out clears the answer");
}

/// JIT round 23 wave 6 (`CRATONVM_GEN_SWEEP_LAZY_ZERO`, default ON): the
/// non-moving sweep leaves a reclaimed span dirty but in the lazy-dirty set
/// and its published mirror, so the dead header is declined without being
/// zeroed, while the rooted object stays an object.
#[test]
fn r23_w6_sweep_leaves_reclaimed_spans_dirty_and_mirrored() {
    let heap = GenerationalHeap::new();
    if !heap.sweep_lazy_zero {
        return; // the kill switch is set for this process
    }
    let dead = heap.alloc_object(ClassId::new(1), 3);
    let live = heap.alloc_object(ClassId::new(1), 3);
    let (d, l) = (dead.as_ptr() as usize, live.as_ptr() as usize);
    assert!(heap.is_object_address(d).is_some());
    let _ = heap.sweep_young_non_moving(&[live], &[], false);
    heap.refresh_region_bounds();
    assert!(heap.is_object_address(l).is_some(), "the rooted object survives");
    let from = heap.lock_young_from();
    if !from.lazy_dirty_contains(d) {
        // The sweep did not reclaim `dead` (e.g. it retained the span as
        // a conservative over-approximation); nothing to check.
        return;
    }
    // SAFETY: `d` is inside the arena (it was an object of this heap) and
    // the arena is still mapped; the read is of 8 bytes it covered.
    let word0 = unsafe { std::ptr::read(d as *const u64) };
    assert_ne!(word0, 0, "lazily reclaimed: the header was not zeroed in the pause");
    drop(from);
    assert!(
        heap.is_object_address(d).is_none(),
        "the mirror declines the dirty corpse header"
    );
}
