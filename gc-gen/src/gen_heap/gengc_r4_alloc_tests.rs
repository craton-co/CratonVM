use super::*;

#[test]
fn the_bump_fit_arithmetic_is_checked_and_rounds_the_cursor() {
    assert!(GenerationalHeap::young_bump_fits(0, 64, 64));
    assert!(!GenerationalHeap::young_bump_fits(0, 64, 65));
    // A cursor off the grid is rounded UP before the request is placed.
    assert!(GenerationalHeap::young_bump_fits(1, 64, 56));
    assert!(!GenerationalHeap::young_bump_fits(1, 64, 57));
    // Neither half may wrap into a small, fitting end.
    assert!(!GenerationalHeap::young_bump_fits(
        usize::MAX,
        usize::MAX,
        1
    ));
    assert!(!GenerationalHeap::young_bump_fits(8, 1 << 20, usize::MAX));
}

/// The lock-free `young_bump_headroom` must answer exactly what the locked
/// read answers once the lock is free — the published triple is the state
/// of the last unlock, and nothing else holds the lock here.
#[test]
fn lock_free_bump_headroom_agrees_with_the_locked_read() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    for round in 0..6 {
        for _ in 0..(round * 40) {
            let _obj = heap
                .try_alloc_object(ClassId::new(0), 2)
                .expect("a 64 KiB nursery holds a few hundred small objects");
        }
        for size in [8usize, 1024, 16 * 1024, 60 * 1024, 64 * 1024, 64 * 1024 + 8] {
            let locked = {
                let from = heap.lock_young_from();
                GenerationalHeap::young_bump_fits(from.used(), from.capacity(), size)
            };
            assert_eq!(
                heap.young_bump_headroom(size),
                locked,
                "round={round} size={size}"
            );
        }
    }
}

/// The fragmentation fallback considers the un-bumped tail, not only free
/// blocks. The state: two 128-byte holes (below `frag_tlab_floor`) and a
/// ~63.5 KiB bump tail. `refill_tlab` sizes its request against tail PLUS
/// holes, so the main arm's request exceeds the tail and fails; the
/// fallback used to see "no free block reaches the floor" and refuse,
/// with 63.5 KiB of contiguous nursery sitting unused.
#[test]
fn the_refill_fallback_serves_the_bump_tail_when_no_block_reaches_the_floor() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    let (base, capacity) = {
        let mut from = heap.lock_young_from();
        let base = from.base_ptr() as usize;
        let mut offs = Vec::new();
        for _ in 0..4 {
            let p = from.alloc(128, 8).expect("a fresh 64 KiB arena");
            offs.push(p as usize - base);
        }
        // Free the 1st and 3rd: two non-adjacent 128-byte holes.
        from.add_free_block(offs[0], 128);
        from.add_free_block(offs[2], 128);
        assert_eq!(from.used(), 512);
        (base, from.capacity())
    };
    assert!(128 < cratonvm_gc_core::tlab::frag_tlab_floor());
    let (ptr, size) = heap
        .refill_tlab(1 << 20)
        .expect("the bump tail is a perfectly good mini-TLAB");
    assert_eq!(ptr as usize, base + 512, "carved from the tail, not a hole");
    assert_eq!(size, capacity - 512);
    assert_eq!(size % 8, 0);
    // SAFETY: `refill_tlab` handed this caller `[ptr, ptr + size)`, zeroed.
    let chunk = unsafe { std::slice::from_raw_parts(ptr, size) };
    assert!(
        chunk.iter().all(|&b| b == 0),
        "the chunk must come back zeroed"
    );
}

/// The main arm still serves the full request when it fits, and the chunk
/// is zeroed even though the memset now runs after the lock is released.
#[test]
fn a_refill_that_fits_is_served_whole_and_zeroed() {
    let heap = GenerationalHeap::with_sizes(256 * 1024, 64 * 1024);
    let (ptr, size) = heap.refill_tlab(64 * 1024).expect("fits");
    assert_eq!(size, 64 * 1024);
    // SAFETY: as above.
    let chunk = unsafe { std::slice::from_raw_parts(ptr, size) };
    assert!(chunk.iter().all(|&b| b == 0));
    assert!(
        heap.young_from_used() >= size,
        "the carve is visible to the published triple after the unlock"
    );
}

/// Every decision arm of the pause-goal loop is counted, on the same
/// sequence of pauses `a_halving_that_does_not_move_the_pause_is_reverted_and_latched`
/// walks — the counters are what makes a trigger sweep's arms legible.
#[test]
fn the_pause_goal_loop_counts_every_decision_arm() {
    let mut fb = TriggerFeedback::default();
    let (floor, ceiling, cap) = (16usize << 20, 128usize << 20, 256usize << 20);
    let t1 = next_young_trigger(&mut fb, 128 << 20, floor, ceiling, cap, 300, 200, 50 << 20);
    assert_eq!(fb.census.halvings, 1);
    let t2 = next_young_trigger(&mut fb, t1, floor, ceiling, cap, 295, 200, 50 << 20);
    assert_eq!(fb.census.reverts, 1);
    let t3 = next_young_trigger(&mut fb, t2, floor, ceiling, cap, 305, 200, 52 << 20);
    assert_eq!(fb.census.latched_holds, 1);
    let t4 = next_young_trigger(&mut fb, t3, floor, ceiling, cap, 250, 200, 10 << 20);
    assert_eq!(fb.census.halvings, 2, "a re-armed trial is a halving");
    let t5 = next_young_trigger(&mut fb, t4, floor, ceiling, cap, 150, 200, 10 << 20);
    let _ = next_young_trigger(&mut fb, t5, floor, ceiling, cap, 40, 200, 10 << 20);
    assert_eq!(fb.census.increases, 1);
    assert_eq!(
        (
            fb.census.halvings,
            fb.census.reverts,
            fb.census.latched_holds
        ),
        (2, 1, 1),
        "the in-band pause (150 ms) is no decision and counts nowhere"
    );
}

/// `Ord::clamp` panics when min > max. The pure function must be total
/// on its own terms, whatever a future caller passes.
#[test]
fn next_young_trigger_does_not_panic_on_an_inverted_band() {
    let mut fb = TriggerFeedback::default();
    let (floor, ceiling) = (100usize, 50usize);
    let t1 = next_young_trigger(&mut fb, 200, floor, ceiling, 1000, 300, 200, 10);
    assert_eq!(t1, 100);
    // The revert arm is the `clamp`.
    let t2 = next_young_trigger(&mut fb, t1, floor, ceiling, 1000, 295, 200, 10);
    assert_eq!(t2, 100);
    let t3 = next_young_trigger(&mut fb, t2, floor, ceiling, 1000, 10, 200, 10);
    assert_eq!(t3, 100);
}

/// The census accessor reports the live threshold and the knob that set
/// it, and `adapt_young_trigger_to_pause` feeds it — including the count
/// of pauses that also ran an old-generation reclamation.
#[test]
fn the_young_trigger_census_sees_what_the_loop_did() {
    let heap = GenerationalHeap::with_sizes(1 << 20, 1 << 20);
    let fresh = heap.young_trigger_census();
    assert_eq!(fresh.adapt_calls, 0);
    assert_eq!(fresh.trigger_low_water, 0);
    assert_eq!(fresh.semi_capacity, 1 << 20);
    assert_eq!(fresh.knob_percent, young_trigger_percent());
    assert_eq!(
        fresh.trigger_bytes,
        (1 << 20) * young_trigger_percent() / 100
    );
    assert_eq!(fresh.pause_goal_ms, heap.young_pause_goal_ms);

    // An over-goal pause: one halving.
    cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(false);
    heap.adapt_young_trigger_to_pause(300, 200, 1 << 16);
    let after = heap.young_trigger_census();
    assert_eq!(after.adapt_calls, 1);
    assert_eq!(after.halvings, 1);
    assert_eq!(after.adapt_on_major, 0);
    assert_eq!(after.trigger_low_water, after.trigger_bytes);
    assert!(after.trigger_bytes < fresh.trigger_bytes);

    // A pause whose cycle also reclaimed old gen is counted as such.
    cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(true);
    heap.adapt_young_trigger_to_pause(10, 200, 1 << 16);
    cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(false);
    let major = heap.young_trigger_census();
    assert_eq!(major.adapt_calls, 2);
    assert_eq!(major.adapt_on_major, 1);

    let line = heap.young_trigger_census_line();
    assert!(line.starts_with("[GC] young_trigger: young_trigger_bytes="));
    for key in [
        "young_trigger_knob_pct=",
        "young_pause_goal_ms=",
        "adapt_calls=2",
        "adapt_on_major=1",
        "young_trigger_low_water=",
    ] {
        assert!(line.contains(key), "{key} missing from {line}");
    }
}
