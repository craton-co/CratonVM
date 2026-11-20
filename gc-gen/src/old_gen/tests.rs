/// gcj j1yg-pause round 3: a pause whose own promotions alone could make
/// the occupancy trigger due may PROMISE to stay minor (the trigger is
/// then deferred), though it is not certainly minor; one already due, or
/// whose promotions cannot fit one free block, may not.
#[test]
fn gcj_j1yg_a_pause_may_promise_minor_below_the_trigger() {
    let mut og = OldGen::new(1024 * 1024);
    assert!(og.alloc(600 * 1024, 8).is_some());
    assert!(!og.young_pause_certainly_leaves_old(200 * 1024));
    assert!(og.young_pause_may_promise_minor(200 * 1024));
    assert!(!og.young_pause_may_promise_minor(600 * 1024), "cannot fit one block");
    assert!(og.alloc(200 * 1024, 8).is_some());
    assert!(!og.young_pause_may_promise_minor(16 * 1024), "already due");
}

use super::*;

/// Lay out `count` fixed-size objects back to back in a fresh old gen
/// and return their `(ptr, offset)` pairs. Headers must be real: the
/// walker derives every object boundary by striding header-to-header.
fn old_gen_with_objects(count: usize, slots: u32) -> (OldGen, Vec<usize>) {
    let mut og = OldGen::new(64 * 1024);
    let base = og.base_ptr() as usize;
    let mut offsets = Vec::new();
    for i in 0..count {
        let body = SLOT_SIZE * slots as usize;
        let ptr = og.alloc(HEADER_SIZE + body, 8).unwrap();
        // SAFETY: `ptr` is a fresh, correctly sized old-gen allocation.
        unsafe {
            std::ptr::write(
                ptr as *mut ObjectHeader,
                ObjectHeader::new(
                    cratonvm_types::ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    slots,
                ),
            );
        }
        offsets.push(ptr as usize - base);
    }
    (og, offsets)
}

/// The reference semantics this walk had before it was given a monotone
/// cursor: an object is collected iff its start offset lies in ANY dirty
/// range. Kept here as the oracle the fast path is compared against.
fn expected_in_ranges(offsets: &[usize], ranges: &[(usize, usize)]) -> Vec<usize> {
    offsets
        .iter()
        .copied()
        .filter(|off| ranges.iter().any(|&(s, e)| *off >= s && *off < e))
        .collect()
}

fn collected_offsets(og: &OldGen, ranges: &[(usize, usize)]) -> Vec<usize> {
    let base = og.base_ptr() as usize;
    og.walk_objects_in_card_ranges(ranges)
        .into_iter()
        .map(|(ptr, _)| ptr as usize - base)
        .collect()
}

#[test]
fn card_range_walk_collects_exactly_the_objects_starting_in_a_dirty_range() {
    let (og, offsets) = old_gen_with_objects(8, 2);
    // One range covering objects 2..4 only.
    let ranges = vec![(offsets[2], offsets[4])];
    assert_eq!(
        collected_offsets(&og, &ranges),
        expected_in_ranges(&offsets, &ranges)
    );
    assert_eq!(
        collected_offsets(&og, &ranges),
        vec![offsets[2], offsets[3]]
    );
}

#[test]
fn card_range_walk_handles_several_disjoint_ranges() {
    let (og, offsets) = old_gen_with_objects(10, 3);
    // Sorted, non-overlapping, with gaps — what the card table produces.
    let ranges = vec![
        (offsets[1], offsets[2]),
        (offsets[4], offsets[6]),
        (offsets[9], offsets[9] + 8),
    ];
    assert_eq!(
        collected_offsets(&og, &ranges),
        expected_in_ranges(&offsets, &ranges)
    );
}

#[test]
fn card_range_walk_is_empty_when_no_object_starts_in_a_range() {
    let (og, offsets) = old_gen_with_objects(6, 2);
    // A range strictly inside object 3's body, so no object *starts* in it.
    let ranges = vec![(offsets[3] + 8, offsets[3] + 16)];
    assert!(collected_offsets(&og, &ranges).is_empty());
    assert!(expected_in_ranges(&offsets, &ranges).is_empty());
}

#[test]
fn card_range_walk_matches_the_any_predicate_for_every_prefix_and_suffix() {
    // The monotone cursor added 2026-08-02 replaced a per-object
    // `ranges.iter().any(..)`, and returns early once the cursor passes
    // the last range. Sweep a family of range sets — including ones that
    // end before the last object, which is what exercises that early
    // return — and require the two to agree exactly.
    let (og, offsets) = old_gen_with_objects(12, 2);
    for lo in 0..offsets.len() {
        for hi in (lo + 1)..=offsets.len() {
            let end = if hi == offsets.len() {
                offsets[hi - 1] + 8
            } else {
                offsets[hi]
            };
            let ranges = vec![(offsets[lo], end)];
            assert_eq!(
                collected_offsets(&og, &ranges),
                expected_in_ranges(&offsets, &ranges),
                "disagreement for objects {lo}..{hi}"
            );
        }
    }
}

/// A NESTED free block must not drag the card-range walk's cursor
/// backwards, which would make it parse free bytes as object headers.
///
/// gengc-round1 2026-09-20. `walk_objects` has taken `cursor.max(..)` since
/// the `OLD_FREE_LIST_OVERLAPS` double-free diagnostic landed;
/// `walk_objects_in_card_ranges` — the walk the minor GC's old→young card
/// scan actually uses — did not. A phantom header on this path truncates
/// the dirty-card root set rather than merely mis-reporting an object, and
/// a lost old→young root is a live young object freed.
///
/// The nesting is produced the way production can produce it: an over-free
/// whose span is not object-aligned — the GCAUD-1 shape, where `free`'s
/// rounding disagreed with `alloc`'s and left a sliver. `coalesce_free_blocks`
/// deliberately refuses to collapse overlapping blocks, so the nesting
/// survives to the next walk.
#[test]
fn card_range_walk_survives_a_nested_free_block() {
    let (mut og, offsets) = old_gen_with_objects(8, 2);
    let base = og.base_ptr() as usize;
    let stride = offsets[1] - offsets[0];
    // Outer: objects 2..6 freed as one span. Inner: a 24-byte span nested
    // inside it that ends in the MIDDLE of object 3's footprint.
    // SAFETY: both spans lie inside blocks this old gen allocated. The
    // overlap is the defect being modelled; nothing reads the freed bytes.
    unsafe {
        og.free((base + offsets[2]) as *mut u8, stride * 4);
        og.free((base + offsets[3]) as *mut u8, 24);
    }
    // Without the `max` the cursor rewinds from 6*stride back to
    // `offsets[3] + 24` — an address inside a freed object's zeroed
    // payload — and the walk parses those bytes as a run of phantom
    // 16-byte objects, so the objects that really are there (6 and 7) are
    // never yielded and a pile that is not there is.
    let full = vec![(0usize, offsets[7] + stride)];
    assert_eq!(
        collected_offsets(&og, &full),
        vec![offsets[0], offsets[1], offsets[6], offsets[7]],
        "a nested free block must not rewind the card-range walk's cursor",
    );
    // The real invariant: this walk and `walk_objects` (which has carried
    // the guard all along) must agree on an all-covering range.
    let all: Vec<usize> = og
        .walk_objects()
        .into_iter()
        .map(|(p, _)| p as usize - base)
        .collect();
    assert_eq!(
        collected_offsets(&og, &full),
        all,
        "the filtered and unfiltered old-gen walks disagree",
    );
}

#[test]
fn card_range_walk_with_no_ranges_collects_nothing() {
    let (og, _offsets) = old_gen_with_objects(4, 2);
    assert!(collected_offsets(&og, &[]).is_empty());
}

/// gen-gc-five: a promotion buffer's unused tail goes back to the free
/// list without stamping the reclaim epoch (it never held an object, so
/// no remark snapshot can name it), and is servable again at once.
#[test]
fn releasing_an_unused_tail_keeps_the_reclaim_epoch() {
    let mut og = OldGen::new(64 * 1024);
    let epoch = og.reclaim_epoch();
    // gengc-round2: a promotion buffer is carved with the BUFFER form, so
    // the debug carve registry knows its tail may come back. See
    // `OldGen::unzeroed_buffer_carves`.
    let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
    let used = og.used();
    // SAFETY: `[p + 1024, p + 4096)` is the untouched tail of the block just carved.
    unsafe { og.release_unused_tail(p.add(1024), 3072) };
    assert_eq!(
        og.reclaim_epoch(),
        epoch,
        "a never-used tail is not a reclaim"
    );
    assert_eq!(og.used(), used - 3072);
    let again = og
        .alloc(3072, 8)
        .expect("the tail is a servable free block");
    assert_eq!(again as usize, p as usize + 1024);
}

#[test]
fn releasing_an_eight_byte_tail_stamps_a_walkable_gap_sentinel() {
    let mut og = OldGen::new(64 * 1024);
    let p = og.alloc_unzeroed_buffer(64, 8).unwrap();
    // Make the used prefix a real, reference-free object so the walk can
    // prove that the released sub-header tail does not desynchronise it.
    unsafe { crate::gen_evac::write_filler(p as usize, 56) };
    let used = og.used();
    unsafe { og.release_unused_tail(p.add(56), 8) };

    assert_eq!(og.used(), used, "an eight-byte sentinel remains occupied");
    let objects = og.walk_objects();
    assert_eq!(objects, vec![(p, 56)]);
    assert!(og.walk_objects_with_gaps().1.is_empty());
}

#[test]
fn malformed_promotion_tails_are_refused_without_breaking_the_walk() {
    let mut og = OldGen::new(64 * 1024);
    let p = og.alloc_unzeroed_buffer(64, 8).unwrap();
    unsafe { crate::gen_evac::write_filler(p as usize, 64) };
    let used = og.used();

    // A 12-byte fragment is not an eight-byte-granular object span, and
    // the second call starts at an unaligned address. Both are rejected
    // in release builds too; the untouched, valid filler remains walkable.
    unsafe {
        og.release_unused_tail(p.add(48), 12);
        og.release_unused_tail(p.add(49), 8);
    }

    assert_eq!(og.used(), used);
    assert_eq!(og.walk_objects(), vec![(p, 64)]);
    assert!(og.walk_objects_with_gaps().1.is_empty());
}

#[test]
fn misaligned_free_is_refused_without_losing_the_object() {
    let mut og = OldGen::new(64 * 1024);
    let p = og.alloc(HEADER_SIZE, 8).unwrap();
    unsafe {
        std::ptr::write(
            p as *mut ObjectHeader,
            ObjectHeader::new(
                cratonvm_types::ClassId::new(1),
                ObjectKind::Object,
                ArrayElementType::Int,
                0,
                0,
            ),
        );
    }
    let used = og.used();
    unsafe { og.free(p.add(1), HEADER_SIZE) };

    assert_eq!(og.used(), used);
    assert_eq!(og.walk_objects(), vec![(p, HEADER_SIZE)]);
    assert!(og.walk_objects_with_gaps().1.is_empty());
}

/// gengc-round1 2026-09-20. `compact`'s Phase 4 zero is now bounded by the
/// high-water mark instead of by CAPACITY, so the pause no longer carries
/// a `memset` of the whole trailing free block (nor commits every page of
/// a store `OldGen::new` deliberately leaves unmapped).
///
/// Two properties have to survive that: every byte that ever held an
/// object is still zeroed — H2-CID0's diagnostic depends on a stale
/// reference reading an all-zero `ClassId(0)` header rather than plausible
/// garbage — and the mark itself must track allocation, or the bound could
/// silently shrink below what was written.
#[test]
fn compaction_still_zeroes_everything_that_ever_held_an_object() {
    let (mut og, offsets) = old_gen_with_objects(8, 2);
    let stride = offsets[1] - offsets[0];
    let top = offsets[7] + stride;
    assert_eq!(
        og.high_water(),
        top,
        "the high-water mark must follow the allocations",
    );
    assert!(
        og.capacity() > top * 8,
        "the whole point is a capacity far above the high-water mark",
    );

    // Object 0 is live; 1..8 are garbage this compaction drops.
    let base = og.base_ptr() as usize;
    // SAFETY: `old_gen_with_objects` wrote a valid header here.
    unsafe {
        (*((base + offsets[0]) as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
    }
    let _moved = og.compact();

    assert_eq!(og.used(), stride, "only the marked object survives");
    // SAFETY: `[stride, top)` is inside the backing store and now free.
    let dropped =
        unsafe { std::slice::from_raw_parts((base + stride) as *const u8, top - stride) };
    assert!(
        dropped.iter().all(|&b| b == 0),
        "a compacted-away object left non-zero bytes behind — a stale \
         reference to it would read plausible garbage instead of ClassId(0)",
    );
    assert_eq!(
        og.high_water(),
        og.used(),
        "the mark must come back down with the compacted end, or the next \
         compaction re-zeroes storage it has already proved clean",
    );
}

/// The bounded best-fit probe must never turn "there is a fit" into an OOM.
///
/// gengc-round1 2026-09-20. `alloc_from_buckets` now stops REFINING the
/// best fit after [`BEST_FIT_PROBE_LIMIT`] fitting candidates, because a
/// bucket's length is unbounded even though its blocks' sizes are within a
/// factor of two — a non-compacting sweep can leave one bucket holding
/// every block it freed, reinstating the O(N) scan the segregated free
/// list exists to remove. The cap counts FITTING candidates only, so a
/// bucket with no fit is still scanned exhaustively; this pins that, which
/// is the round-5 #2 "cannot spuriously OOM" property.
///
/// gen r4w4/oldgen4: with four classes per power of two the pair is
/// `256`/`312` (both in `[256, 320)`), and the generation is filled
/// EXACTLY so no other free block exists: the non-fit deferral
/// (`NONFIT_PROBE_LIMIT`) then finds no larger class and must finish the
/// deferred one — the "cannot spuriously OOM" half of its contract.
#[test]
fn a_lone_fit_behind_a_long_run_of_too_small_blocks_is_still_found() {
    // All of these land in the same class (`[256, 320)`), which is also
    // the class a 300-byte request starts its search in.
    const TOO_SMALL: usize = 256;
    const FITS: usize = 312;
    const RUN: usize = BEST_FIT_PROBE_LIMIT * 2;
    assert!(
        RUN > NONFIT_PROBE_LIMIT,
        "the run must outlast the non-fit probe cap"
    );
    assert_eq!(bucket_for(TOO_SMALL), bucket_for(FITS));
    assert_eq!(min_satisfying_bucket(300 + 7), bucket_for(TOO_SMALL));
    let mut og = OldGen::new(RUN * TOO_SMALL + FITS);

    let mut small = Vec::new();
    for _ in 0..RUN {
        small.push(og.alloc(TOO_SMALL, 8).expect("sized for exactly these"));
    }
    let big = og.alloc(FITS, 8).expect("and one larger block");
    assert_eq!(
        og.free_block_count(),
        0,
        "no other free block to escalate to"
    );
    for p in small {
        // SAFETY: each came from `alloc(TOO_SMALL, 8)` on this old gen and
        // is freed exactly once with its own size.
        unsafe { og.free(p, TOO_SMALL) };
    }
    // Freed LAST, so it sits at the end of the bucket — behind more
    // non-fitting blocks than the probe limit.
    // SAFETY: as above.
    unsafe { og.free(big, FITS) };

    let got = og
        .alloc(300, 8)
        .expect("the only fitting block in the bucket must still be found");
    assert_eq!(
        got as usize, big as usize,
        "the allocator served the request from somewhere other than the \
         one block in this bucket that fits",
    );
    let s = og.sizing_stats();
    assert_eq!(
        (s.nonfit_deferrals, s.nonfit_deferral_resumes),
        (1, 1),
        "the scan deferred after the cap and had to come back"
    );
}

/// gen r4w4/oldgen4 — the other half of the non-fit cap: when a LARGER
/// class can serve the request, a class holding more than
/// `NONFIT_PROBE_LIMIT` too-small blocks in front of its one fit costs
/// exactly `NONFIT_PROBE_LIMIT` probes, not a walk of the class. The fit
/// behind the run stays on the free list, still findable.
#[test]
fn a_long_run_of_too_small_blocks_is_deferred_when_a_larger_class_fits() {
    const TOO_SMALL: usize = 256;
    const FITS: usize = 312;
    const RUN: usize = NONFIT_PROBE_LIMIT * 4;
    let mut og = OldGen::new(128 * 1024);
    let mut small = Vec::new();
    for _ in 0..RUN {
        small.push(og.alloc(TOO_SMALL, 8).expect("128 KiB covers these"));
    }
    let fit = og.alloc(FITS, 8).expect("and the fit");
    for p in small {
        // SAFETY: each came from `alloc(TOO_SMALL, 8)` and is freed once.
        unsafe { og.free(p, TOO_SMALL) };
    }
    // SAFETY: as above.
    unsafe { og.free(fit, FITS) };

    let before = og.trigger_stats().nonfit_probes;
    let got = og.alloc(300, 8).expect("the trailing block serves it");
    assert_ne!(got, fit, "the larger class won: the deep hole was deferred");
    assert_eq!(
        og.trigger_stats().nonfit_probes - before,
        NONFIT_PROBE_LIMIT as u64,
        "bounded by the cap, not by the run"
    );
    assert_eq!(og.sizing_stats().nonfit_deferrals, 1);
    assert_eq!(og.sizing_stats().nonfit_deferral_resumes, 0);
    // The fit is still on the free list: the exhaustive fallback finds it
    // once nothing larger is left (exercised by the lone-fit test above);
    // here, a walk of the free list says so directly.
    let fit_off = fit as usize - og.base_ptr() as usize;
    assert!(
        og.with_sorted_free_blocks(|s| s.iter().any(|b| b.offset == fit_off && b.size == FITS))
    );
}

/// gen r4w4/oldgen4 — the size classes: four per power of two, lower
/// bounds strictly increasing, and every class above a request's start
/// class holds only blocks larger than the request.
#[test]
fn size_classes_split_each_power_of_two_in_quarters() {
    assert_eq!(bucket_for(0), 0);
    assert_eq!(bucket_for(8), 0);
    assert_eq!(bucket_for(16), SUB_CLASSES);
    // Below 64 bytes every 8-aligned size has a class of its own.
    let small: Vec<usize> = (2..8).map(|k| bucket_for(8 * k)).collect();
    let mut dedup = small.clone();
    dedup.dedup();
    assert_eq!(small, dedup, "{small:?}");
    // [256, 320) [320, 384) [384, 448) [448, 512)
    assert_eq!(bucket_for(256), bucket_for(319));
    assert_eq!(bucket_for(320), bucket_for(256) + 1);
    assert_eq!(bucket_for(511), bucket_for(256) + 3);
    assert_eq!(bucket_for(512), bucket_for(256) + 4);
    // Monotone, and (from 32 bytes, where a class is at least 8 bytes
    // wide) consecutive 8-aligned sizes never skip a class.
    let mut prev = bucket_for(32);
    for size in (32..1 << 20).step_by(8) {
        let b = bucket_for(size);
        assert!(b >= prev && b <= prev + 1, "size {size}: {prev} -> {b}");
        prev = b;
    }
    assert_eq!(bucket_for(usize::MAX), NUM_BUCKETS - 1);
    assert_eq!(bucket_for(1 << 40), NUM_BUCKETS - 1);
}

/// gci i41/ev — a generation whose ONLY free blocks are isolated holes of
/// exactly `hole` bytes (every other block of `n` freed, so no two holes
/// touch and the coalesce-and-retry cannot help).
fn og_with_exact_holes(n: usize, hole: usize) -> (OldGen, Vec<usize>) {
    let mut og = OldGen::new(n * hole);
    let mut blocks = Vec::new();
    for _ in 0..n {
        blocks.push(og.alloc(hole, 8).expect("sized for exactly these") as usize);
    }
    assert_eq!(og.free_block_count(), 0, "filled exactly");
    let mut holes = Vec::new();
    for (i, &p) in blocks.iter().enumerate() {
        if i % 2 == 0 {
            // SAFETY: each came from `alloc(hole, 8)` on this old gen and
            // is freed exactly once with its own size.
            unsafe { og.free(p as *mut u8, hole) };
            holes.push(p);
        }
    }
    (og, holes)
}

/// gci i41/ev (`CRATONVM_GC_OLD_EXACT_START_CLASS`): a 16-byte and a
/// 24-byte request are served from an exactly fitting hole of their own
/// size. With the pre-i41 start (`bucket_for(size + 7)`) the search began
/// one class ABOVE those holes and refused both requests on a free list
/// holding nothing else; that arm is pinned here too.
#[test]
fn gci_i41ev_a_16_or_24_byte_request_fits_an_exact_hole() {
    for hole in [HEADER_SIZE, HEADER_SIZE + 8] {
        assert_ne!(
            bucket_for(hole),
            bucket_for(hole + 7),
            "the shape this pins: `size + 7` lands one class higher at {hole}",
        );
        assert_eq!(alloc_start_bucket(hole, 8, true), bucket_for(hole));
        let (mut og, holes) = og_with_exact_holes(16, hole);
        // The default arm (set explicitly so an exported `=0` in the
        // test environment cannot flip what this pins).
        og.exact_start_class = true;
        let got = og.alloc(hole, 8).map(|p| p as usize);
        assert!(
            got.is_some_and(|g| holes.contains(&g)),
            "one of the free {hole}-byte holes must serve the request, got {got:?}",
        );
        assert_eq!(og.free_block_count(), holes.len() - 1, "an exact fit: no remainder");

        // The `=0` arm: the old start class never sees the holes.
        let (mut old, _) = og_with_exact_holes(16, hole);
        old.exact_start_class = false;
        assert!(
            old.alloc(hole, 8).is_none(),
            "pre-i41 start skips the exact-fit class at {hole}"
        );
    }
}

/// gci i41/ev — from 32 bytes up an 8-aligned request starts in the same
/// class either way (a class there is at least 8 bytes wide and starts on
/// a multiple of 8), so the switch changes the start only for the two
/// small classes above and for `align > 8`.
#[test]
fn gci_i41ev_the_start_class_moves_only_below_32_bytes_at_align_8() {
    for size in (32..1 << 16).step_by(8) {
        assert_eq!(
            alloc_start_bucket(size, 8, true),
            alloc_start_bucket(size, 8, false),
            "size {size}",
        );
    }
    for size in [HEADER_SIZE, HEADER_SIZE + 8] {
        assert!(alloc_start_bucket(size, 8, true) < alloc_start_bucket(size, 8, false));
    }
}

#[test]
fn new_old_gen() {
    let og = OldGen::new(4096);
    assert_eq!(og.used(), 0);
    assert_eq!(og.capacity(), 4096);
    // Round-5 #14: free_list is now segregated; only the count is observable.
    assert_eq!(og.free_block_count(), 1);
}

#[test]
fn alloc_basic() {
    let mut og = OldGen::new(4096);
    let ptr = og.alloc(64, 8).unwrap();
    assert!(!ptr.is_null());
    assert_eq!(og.used(), 64);
    assert!(og.contains(ptr));
}

#[test]
fn alloc_multiple() {
    let mut og = OldGen::new(4096);
    let p1 = og.alloc(64, 8).unwrap();
    let p2 = og.alloc(128, 8).unwrap();
    assert_ne!(p1, p2);
    assert_eq!(og.used(), 192);
}

#[test]
fn alloc_alignment() {
    let mut og = OldGen::new(4096);
    // Allocate 3 bytes, then 64 with align 8
    let _p1 = og.alloc(3, 1).unwrap();
    let p2 = og.alloc(64, 8).unwrap();
    // p2 should be 8-byte aligned
    assert_eq!(p2 as usize % 8, 0);
}

#[test]
fn alloc_alignment_reserves_compact_object_padding() {
    let mut og = OldGen::new(4096);
    let first = og.alloc(44, 8).unwrap();
    let second = og.alloc(8, 8).unwrap();

    assert_eq!(second as usize - first as usize, 48);
    // OldGen reserves at least one header for any request, so the second
    // 8-byte request occupies one production header after the 48-byte
    // aligned extent. Derived rather than restated: this number moved 80 ->
    // 72 with the 2026-08-06 header shrink, and a literal here would have
    // to be re-derived by hand every time the layout moves.
    assert_eq!(og.used(), 48 + cratonvm_types::HEADER_SIZE);
}

#[test]
fn alloc_full_returns_none() {
    let mut og = OldGen::new(128);
    let _p1 = og.alloc(128, 1).unwrap();
    assert!(og.alloc(1, 1).is_none());
}

#[test]
fn free_basic() {
    let mut og = OldGen::new(4096);
    let p1 = og.alloc(64, 8).unwrap();
    assert_eq!(og.used(), 64);

    unsafe { og.free(p1, 64) };
    assert_eq!(og.used(), 0);
    // Should be able to allocate again
    let p2 = og.alloc(64, 8).unwrap();
    assert!(!p2.is_null());
}

/// xt-helper-window OOM regression: with compaction unavailable, freeing
/// a run of adjacent blocks must not leave the generation unable to serve
/// a request that the run's COMBINED bytes cover. Before the fix the size
/// buckets held N isolated blocks of `S` bytes and an `S * 4` request
/// returned `None` even though `N * S` contiguous bytes were free.
///
/// The trailing free space is deliberately consumed first: without that,
/// the request is served from the untouched tail and the test proves
/// nothing.
#[test]
fn adjacent_frees_are_coalesced_so_a_larger_request_still_fits() {
    let mut og = OldGen::new(64 * 1024);
    const S: usize = 256;
    const N: usize = 16;

    let mut ptrs = Vec::new();
    for _ in 0..N {
        ptrs.push(og.alloc(S, 8).expect("fresh old gen must serve S bytes"));
    }
    let tail = og.capacity() - og.used();
    og.alloc(tail, 8)
        .expect("the trailing free block must be allocatable");
    assert_eq!(og.free_block_count(), 0, "no free space must remain");

    for p in ptrs.drain(..) {
        // SAFETY: each `p` came from `alloc(S, 8)` on this OldGen and is
        // freed exactly once, with its own size.
        unsafe { og.free(p, S) };
    }
    assert_eq!(og.free_block_count(), N, "N isolated blocks, all adjacent");

    let big = S * 4;
    assert!(
        og.alloc(big, 8).is_some(),
        "a {big}-byte request must be served out of {N} adjacent {S}-byte \
         free blocks — this is the old-gen half of the xt-helper-window \
         OutOfMemoryError (the free list never coalesces without compaction)"
    );
}

/// gengc-round2. The mirror of
/// `adjacent_frees_are_coalesced_so_a_larger_request_still_fits`, which is
/// what `gengc-oldgen-fragmentation-has-no-trigger-FIXED-20260924.md` asks for:
/// build the fragmented state, assert the POLICY PREDICATE fires, run the
/// scheduled repair, assert the signal recovers.
///
/// The point of the test is that the trigger no longer requires an
/// allocation to have already failed.
#[test]
fn fragmentation_is_detected_before_an_allocation_fails_and_the_repair_fixes_it() {
    const BIG: usize = 2048;
    const SMALL: usize = 1024;
    const FREED: usize = 40;

    let mut og = OldGen::new(64 * 1024);
    // One larger object sets the scale the predicate judges against, then
    // the rest of the generation is filled with SMALL blocks so nothing is
    // left over — the free list this test builds must be the fragmented
    // middle, not an untouched tail.
    og.alloc(BIG, 8).unwrap();
    let small_count = (og.capacity() - og.used()) / SMALL;
    let mut ptrs = Vec::new();
    for _ in 0..small_count {
        ptrs.push(og.alloc(SMALL, 8).unwrap());
    }
    assert_eq!(og.free_block_count(), 0, "no free space must remain");
    assert_eq!(
        og.recent_max_request(),
        BIG,
        "the allocator must remember the largest request it served",
    );

    // A contiguous run of small blocks goes back: the SUM of free bytes is
    // large, every individual hole is smaller than the largest request.
    for p in ptrs.drain(..FREED) {
        // SAFETY: each `p` came from `alloc(SMALL, 8)` and is freed once.
        unsafe { og.free(p, SMALL) };
    }
    assert_eq!(og.free_bytes_and_largest(), (FREED * SMALL, SMALL));
    assert!(
        og.fragmentation_repair_due(),
        "{FREED} adjacent {SMALL}-byte holes with a {BIG}-byte recent \
         request is exactly the 'bytes but no room' state the pair was \
         added to detect (free={:?} recent_max={})",
        og.free_bytes_and_largest(),
        og.recent_max_request(),
    );

    let merged = og.repair_fragmentation();
    assert_eq!(merged, FREED - 1, "every hole was adjacent to the next");
    assert_eq!(og.fragmentation_repairs(), 1);
    assert_eq!(og.free_block_count(), 1);
    assert_eq!(og.largest_free_block(), FREED * SMALL);

    // The reset of `recent_max_request` is the hysteresis: an idle
    // generation cannot provoke a second repair.
    assert_eq!(og.recent_max_request(), 0);
    assert!(!og.fragmentation_repair_due());
}

/// The predicate is a pure function of four numbers so it can be tested
/// without a heap, and each of its three clauses suppresses a specific
/// false positive. Pin all of them.
#[test]
fn the_fragmentation_predicate_suppresses_each_false_positive() {
    let cap = 1024 * 1024;
    let fires = |free, largest, recent| {
        OldGen::fragmentation_warrants_repair(cap, free, largest, recent)
    };

    // The real thing: plenty free, no hole big enough for what is being
    // asked for.
    assert!(fires(cap / 2, 1024, 4096));

    // Nothing allocated since the last repair — no scale to judge against,
    // and the clause that stops a repair firing every cycle.
    assert!(!fires(cap / 2, 1024, 0));

    // Genuinely near-full for the sizes being asked for: coalescing cannot
    // conjure bytes. At the K boundary and just over it.
    assert!(!fires(FRAGMENTATION_FIT_FACTOR * 4096 - 1, 1024, 4096));
    assert!(fires(FRAGMENTATION_FIT_FACTOR * 4096, 1024, 4096));

    // Not fragmented: some single hole still serves the biggest recent
    // request. At the boundary and one byte below it.
    assert!(!fires(cap / 2, 4096, 4096));
    assert!(fires(cap / 2, 4095, 4096));

    // A negative control the gap page calls for: a generation allocated
    // once and never freed must never trigger.
    let mut og = OldGen::new(64 * 1024);
    for _ in 0..8 {
        og.alloc(512, 8).unwrap();
    }
    assert!(
        !og.fragmentation_repair_due(),
        "an unfragmented generation must not schedule a repair",
    );
}

/// gengc-round2. `release_unused_tail` skips the reclaim-epoch stamp; the
/// two paths must stay pinned as DIFFERENT on purpose, which is the other
/// half of `releasing_an_unused_tail_keeps_the_reclaim_epoch`.
#[test]
fn freeing_the_same_span_does_bump_the_reclaim_epoch() {
    let mut og = OldGen::new(64 * 1024);
    let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
    let epoch = og.reclaim_epoch();
    // SAFETY: `p` covers 4096 bytes carved from this generation.
    unsafe { og.free(p, 4096) };
    assert_ne!(
        og.reclaim_epoch(),
        epoch,
        "a `free` releases storage that HELD an object, so every \
         address-keyed snapshot taken before it is ambiguous — that is the \
         difference `release_unused_tail` is claiming not to have",
    );
}

/// gengc-round2: the carve registry must actually fire.
///
/// An assertion that has never been seen to fail is not evidence. This is
/// failure mode 2 from the gap page: a block that was handed out whole —
/// `promote_alloc`'s "object on its own block" fallback carves exactly
/// this way — with part of it later routed through `release_unused_tail`,
/// which would silently skip the epoch stamp for a span that DID hold an
/// object.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "outstanding alloc_unzeroed_buffer carve")]
fn releasing_a_tail_of_a_block_that_was_not_a_buffer_is_caught() {
    let mut og = OldGen::new(64 * 1024);
    // Plain `alloc_unzeroed` — an object on its own block, NOT a buffer.
    let p = og.alloc_unzeroed(4096, 8).unwrap();
    // SAFETY (deliberately violated): this is the contract breach the
    // registry exists to catch, so the call is the test.
    unsafe { og.release_unused_tail(p.add(1024), 3072) };
}

/// And the other shape: a span that is inside a registered buffer but is
/// not its tail. `retire_old_plab` only ever releases `[cursor, end)`, so
/// a middle chunk means something changed that the epoch-skip argument was
/// never made for.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "outstanding alloc_unzeroed_buffer carve")]
fn releasing_a_middle_chunk_of_a_buffer_is_caught() {
    let mut og = OldGen::new(64 * 1024);
    let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
    // SAFETY (deliberately violated): see above.
    unsafe { og.release_unused_tail(p.add(1024), 1024) };
}

/// A buffer's tail can only be handed back once: the registry retires the
/// entry, so a repeat release — which would double-free the span onto the
/// free list — is caught too.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "outstanding alloc_unzeroed_buffer carve")]
fn releasing_the_same_buffer_tail_twice_is_caught() {
    let mut og = OldGen::new(64 * 1024);
    let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
    // SAFETY: the first release is legitimate.
    unsafe { og.release_unused_tail(p.add(1024), 3072) };
    // SAFETY (deliberately violated): the second is not.
    unsafe { og.release_unused_tail(p.add(1024), 3072) };
}

/// The merge itself: adjacent blocks collapse, and a second call reports
/// `0` so `alloc`'s coalesce-and-retry cannot loop.
#[test]
fn coalesce_free_blocks_merges_adjacent_and_is_idempotent() {
    let mut og = OldGen::new(64 * 1024);
    const S: usize = 128;
    let mut ptrs = Vec::new();
    for _ in 0..8 {
        ptrs.push(og.alloc(S, 8).unwrap());
    }
    for p in ptrs.drain(..) {
        // SAFETY: see the test above.
        unsafe { og.free(p, S) };
    }
    let before = og.free_block_count();
    let merged = og.coalesce_free_blocks();
    assert!(merged > 0, "8 adjacent frees must merge (before={before})");
    assert!(
        og.free_block_count() < before,
        "the free-block count must drop ({before} -> {})",
        og.free_block_count()
    );
    assert_eq!(
        og.coalesce_free_blocks(),
        0,
        "a second merge must report no progress, so alloc's retry is one-shot"
    );
}

/// Two holes separated by a LIVE object must stay separate — merging them
/// would hand out a span covering the live object.
#[test]
fn coalesce_free_blocks_never_merges_across_a_live_object() {
    let mut og = OldGen::new(64 * 1024);
    const S: usize = 128;
    let a = og.alloc(S, 8).unwrap();
    let live = og.alloc(S, 8).unwrap();
    let c = og.alloc(S, 8).unwrap();
    let tail = og.capacity() - og.used();
    og.alloc(tail, 8)
        .expect("the trailing free block must be allocatable");

    // SAFETY: `a` and `c` each came from `alloc(S, 8)` and are freed once;
    // `live` is deliberately left allocated between them.
    unsafe {
        og.free(a, S);
        og.free(c, S);
    }
    assert_eq!(og.free_block_count(), 2);

    og.coalesce_free_blocks();
    assert!(
        og.alloc(S * 2, 8).is_none(),
        "two {S}-byte holes separated by a LIVE object must not satisfy a \
         {}-byte request",
        S * 2
    );
    // The live object is untouched and its own hole is still usable.
    let _ = live;
    assert!(
        og.alloc(S, 8).is_some(),
        "each individual hole still serves S"
    );
}

#[test]
fn free_coalesce_forward() {
    let mut og = OldGen::new(4096);
    let p1 = og.alloc(64, 8).unwrap();
    let p2 = og.alloc(64, 8).unwrap();
    let _p3 = og.alloc(64, 8).unwrap();

    // Free p2, then p1.
    unsafe { og.free(p2, 64) };
    unsafe { og.free(p1, 64) };

    // Round-5 #14: free() defers coalescing to the next compact().
    // The big trailing free block (everything after p3) still
    // satisfies a 128-byte allocation directly.
    assert_eq!(og.used(), 64); // only p3 remains
    let big = og.alloc(128, 8).unwrap();
    assert!(!big.is_null());
}

#[test]
fn free_coalesce_backward() {
    let mut og = OldGen::new(4096);
    let p1 = og.alloc(64, 8).unwrap();
    let p2 = og.alloc(64, 8).unwrap();

    // Free p1, then p2.
    unsafe { og.free(p1, 64) };
    unsafe { og.free(p2, 64) };
    assert_eq!(og.used(), 0);

    // Round-5 #14: free() no longer coalesces immediately — coalescing
    // is deferred to the next mark-compact. After freeing both blocks
    // the total free capacity is preserved and the heap can serve a
    // fresh allocation that exactly matches one of the freed blocks
    // (proving the bucketed freelist still hands back contiguous
    // memory).
    let p3 = og.alloc(64, 8).unwrap();
    assert!(!p3.is_null());
}

#[test]
fn contains() {
    let og = OldGen::new(256);
    let base = og.base_ptr();
    assert!(og.contains(base));
    assert!(og.contains(unsafe { base.add(128) }));
    assert!(!og.contains(std::ptr::null()));
    assert!(!og.contains(unsafe { base.add(256) })); // exclusive end
}

#[test]
fn walk_objects_empty() {
    let og = OldGen::new(4096);
    let objects = og.walk_objects();
    assert!(objects.is_empty());
}

#[test]
fn walk_objects_with_allocations() {
    let mut og = OldGen::new(4096);

    // Allocate two objects manually with proper headers
    let obj_size1 = HEADER_SIZE + 2 * SLOT_SIZE; // 2 fields
    let p1 = og.alloc(obj_size1, 8).unwrap();
    unsafe {
        let header = &mut *(p1 as *mut ObjectHeader);
        header.set_num_slots(2);
    }

    let obj_size2 = HEADER_SIZE + SLOT_SIZE; // 1 field
    let p2 = og.alloc(obj_size2, 8).unwrap();
    unsafe {
        let header = &mut *(p2 as *mut ObjectHeader);
        header.set_num_slots(1);
    }

    let objects = og.walk_objects();
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0].0, p1);
    assert_eq!(objects[0].1, obj_size1);
    assert_eq!(objects[1].0, p2);
    assert_eq!(objects[1].1, obj_size2);
}

/// `HIB-DCAST-LATEPHASE.1`: a walk that desyncs from real object
/// boundaries lands on arbitrary bytes. Before this fix `scan_region`
/// read those bytes as a typed `ObjectKind` unconditionally — instant UB
/// for a tag outside `{0, 1, 2}`, which optimized code lowered to a
/// `SIGILL` inside `OldGen::compact` (deterministic faulting RVA across
/// repeated crashes against the real `DefaultCatalogAndSchemaTest`
/// workload). This corrupts only the raw tag byte of an otherwise
/// legitimately-allocated object, mirroring the regression style already
/// used for the conservative-root validators (see
/// `source-debug-jit-conservative-root-invalid-header-tag-sigill.md`),
/// and asserts the walk stops at the corrupted header instead of
/// trusting it.
#[test]
fn walk_objects_stops_at_a_desynced_invalid_kind_tag_instead_of_trapping() {
    let mut og = OldGen::new(4096);

    let obj_size1 = HEADER_SIZE + 2 * SLOT_SIZE;
    let p1 = og.alloc(obj_size1, 8).unwrap();
    unsafe {
        let header = &mut *(p1 as *mut ObjectHeader);
        header.set_num_slots(2);
    }

    let obj_size2 = HEADER_SIZE + SLOT_SIZE;
    let p2 = og.alloc(obj_size2, 8).unwrap();
    unsafe {
        let header = &mut *(p2 as *mut ObjectHeader);
        header.set_num_slots(1);
    }

    // 0xFF is not a declared ObjectKind discriminant (0=Object, 1=Array,
    // 2=HumongousFiller).
    // SAFETY: `p2 + cratonvm_types::KIND_TAGS_BYTE_OFFSET` is the `kind` byte of a live
    // allocation from this OldGen; writing a raw `u8` there does not
    // require the resulting value to be a valid `ObjectKind`.
    unsafe {
        std::ptr::write(p2.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8);
    }

    let before = WALK_DESYNC_HITS.load(std::sync::atomic::Ordering::Relaxed);
    let objects = og.walk_objects();
    let after = WALK_DESYNC_HITS.load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        objects,
        vec![(p1, obj_size1)],
        "the walk must stop at the corrupted header, not trust it"
    );
    assert!(
        after > before,
        "expected the desync guard to fire and bump WALK_DESYNC_HITS"
    );
}

/// Dangling-ref guard (Phase 0): a MARKED object that references an
/// UNMARKED old-gen object must not be left pointing at floating garbage
/// after compaction. The closure promotes the target to live and relocates
/// it; the referrer's field must end up pointing at the target's *new*,
/// still-valid location — never at zeroed/overwritten bytes.
#[test]
fn compact_promotes_unmarked_target_of_live_ref() {
    let mut og = OldGen::new(4096);

    // Object A: live, one reference field.
    let a_size = HEADER_SIZE + SLOT_SIZE;
    let a = og.alloc(a_size, 8).unwrap();

    // Dead filler between A and B so that, once it is reclaimed, B must
    // actually slide to a lower address during compaction (giving it a
    // pointer-map entry and exercising the relocation path).
    let filler_size = HEADER_SIZE + 4 * SLOT_SIZE;
    let filler = og.alloc(filler_size, 8).unwrap();
    unsafe {
        (*(filler as *mut ObjectHeader)).set_num_slots(4);
        // left UNMARKED -> dead -> reclaimed.
    }

    // Object B: the (initially unmarked) target. Tag it with a distinctive
    // identity hash so we can prove we land on real B data, not zeroes.
    const B_TAG: i32 = 0x5EED_BEEF_u32 as i32;
    let b_size = HEADER_SIZE + SLOT_SIZE;
    let b = og.alloc(b_size, 8).unwrap();

    unsafe {
        // A is live, references B in field 0.
        let a_hdr = &mut *(a as *mut ObjectHeader);
        a_hdr.set_num_slots(1);
        a_hdr.add_gc_flags(GC_FLAG_MARKED);
        let a_field0 = a.add(HEADER_SIZE) as *mut Value;
        std::ptr::write(a_field0, Value::Object(Some(ObjectRef::from_raw(b))));

        // B is left UNMARKED (would be floating garbage without the guard).
        let b_hdr = &mut *(b as *mut ObjectHeader);
        b_hdr.set_num_slots(1);
        // A long (legacy) header keeps its identity hash in the aux word.
        b_hdr
            .mark_word
            .store(0, std::sync::atomic::Ordering::Relaxed);
        b_hdr.identity_hash(|| B_TAG).unwrap();
    }

    let map = og.compact();

    // A is the lowest-address live object, so it stays put (no map entry);
    // B must have been promoted and relocated, hence present in the map.
    let b_new = *map
        .get(&(b as usize))
        .expect("unmarked target reachable from a live ref must be promoted + relocated");

    unsafe {
        // A's field must now point at B's NEW location, in-bounds.
        let a_field0 = a.add(HEADER_SIZE) as *const Value;
        let field_val = std::ptr::read(a_field0);
        let Value::Object(Some(target)) = field_val else {
            panic!("A's reference field was clobbered: {field_val:?}");
        };
        assert_eq!(
            target.as_ptr() as usize,
            b_new,
            "A's field must be forwarded to B's new location, not left dangling",
        );

        // The forwarded B must still hold real B data (tag preserved),
        // proving we did not leave the slot pointing at zeroed memory.
        let b_hdr = &*(b_new as *const ObjectHeader);
        assert_eq!(
            b_hdr.installed_identity_hash(),
            B_TAG,
            "B data lost across compaction"
        );
        // GC metadata cleared on the survivor.
        assert!(!b_hdr.is_forwarded());
        assert_eq!(b_hdr.gc_flags() & GC_FLAG_MARKED, 0);
    }
}

/// GCAUD-1 — `free` must return the SAME extent `alloc` reserved.
///
/// `alloc_from_buckets` rounds the reservation up to a multiple of `align`
/// and charges `used_bytes` that rounded amount; `free` mirrored only the
/// `max(HEADER_SIZE, 8)` half of that expression. The bytes between the
/// unrounded and rounded ends were therefore never returned to the free
/// list: `used_bytes` drifted up, and — the part that matters —
/// `walk_objects` derives allocated extents as the GAPS between free
/// blocks, so the sliver read as an allocated non-object-start. The walk
/// then mis-parses it as a header and `break`s out of the region, dropping
/// every later object in it from the walk that drives compaction.
#[test]
fn free_returns_the_whole_extent_alloc_reserved() {
    let mut og = OldGen::new(4096);
    // 44 is not a multiple of 8: `alloc` reserves 48 (see
    // `alloc_alignment_reserves_compact_object_padding`).
    let a = og.alloc(44, 8).expect("fresh old gen serves 44 bytes");
    let b = og.alloc(64, 8).expect("fresh old gen serves 64 bytes");
    assert_eq!(
        b as usize - a as usize,
        48,
        "alloc must reserve the align-rounded extent",
    );
    assert_eq!(og.used(), 48 + 64);

    // SAFETY: `a` is a live block of this OldGen and 44 is the size it was
    // requested with — exactly the call shape the accounting must survive.
    unsafe { og.free(a, 44) };
    assert_eq!(
        og.used(),
        64,
        "freeing with the requested size must decrement the RESERVED size",
    );

    // The recovered hole must be the full 48 bytes, not 44: a 48-byte
    // request has to be servable from it, at the same address.
    let c = og
        .alloc(48, 8)
        .expect("the freed 48-byte hole must be reusable");
    assert_eq!(
        c, a,
        "a 48-byte request must land back in the freed block, not past the \
         live neighbour — a short free leaves an unusable 4-byte sliver",
    );
}

/// GCAUD-2 — the compactor must refuse to slide a heap it cannot fully
/// forward.
///
/// Phase 0 closes the live set over old gen so Phase 1 can stamp a
/// forwarding address on every object a live referrer can reach. It
/// assumed `walk_objects` yields EVERY old-gen object. It does not: a
/// block that is on the free list is invisible to the walk (allocated
/// extents are the gaps between free blocks), which is exactly the state
/// an under-marking in-place sweep leaves behind. Phase 0 used to `|=`
/// `GC_FLAG_MARKED` into that freed block anyway, Phase 1 gave it no
/// forwarding address, and Phase 3 slid a different object onto it —
/// the compactor manufacturing the dangling pointer it exists to prevent.
///
/// The fail-safe answer is to reclaim nothing this cycle.
#[test]
fn compact_is_abandoned_when_a_live_ref_escapes_the_object_walk() {
    let mut og = OldGen::new(4096);
    const C_TAG: i32 = 0x0BAD_F00D_u32 as i32;

    // A: the live referrer.
    let a_size = HEADER_SIZE + SLOT_SIZE;
    let a = og.alloc(a_size, 8).unwrap();
    // B: the target, freed below so the object walk stops yielding it.
    let b_size = HEADER_SIZE + SLOT_SIZE;
    let b = og.alloc(b_size, 8).unwrap();
    // Dead filler, so a compaction that DID run would definitely move C.
    let filler_size = HEADER_SIZE + 4 * SLOT_SIZE;
    let filler = og.alloc(filler_size, 8).unwrap();
    // C: a second live object, the one whose non-movement proves the
    // compaction was abandoned rather than merely uneventful.
    let c_size = HEADER_SIZE + SLOT_SIZE;
    let c = og.alloc(c_size, 8).unwrap();

    // SAFETY: every pointer above is a live block of this OldGen, sized to
    // hold a header plus the slots written here.
    unsafe {
        (*(filler as *mut ObjectHeader)).set_num_slots(4);

        let a_hdr = &mut *(a as *mut ObjectHeader);
        a_hdr.set_num_slots(1);
        a_hdr.add_gc_flags(GC_FLAG_MARKED);
        std::ptr::write(
            a.add(HEADER_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(b))),
        );

        (*(b as *mut ObjectHeader)).set_num_slots(1);

        let c_hdr = &mut *(c as *mut ObjectHeader);
        c_hdr.set_num_slots(1);
        c_hdr.add_gc_flags(GC_FLAG_MARKED);
        // A long (legacy) header keeps its identity hash in the aux word.
        c_hdr
            .mark_word
            .store(0, std::sync::atomic::Ordering::Relaxed);
        c_hdr.identity_hash(|| C_TAG).unwrap();
    }

    // Return B's block to the free list WITHOUT zeroing it — the exact
    // shape the in-place old-gen sweep produces when the mark under-marks.
    // SAFETY: `(b, b_size)` is the pair `alloc` handed out.
    unsafe { og.free(b, b_size) };
    assert!(
        og.walk_objects().iter().all(|&(p, _)| p != b),
        "precondition: the freed block must be invisible to the object walk",
    );

    let used_before = og.used();
    let escapes_before = COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed);
    let map = og.compact();

    assert!(
        map.is_empty(),
        "a compaction that cannot forward every live reference must relocate \
         nothing, so it can report no relocations",
    );
    assert!(
        COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed) > escapes_before,
        "the abandoned compaction must be counted, not silent",
    );
    assert_eq!(
        og.used(),
        used_before,
        "an abandoned compaction must not rewrite the occupancy",
    );

    // SAFETY: nothing moved, so every pointer above still names its object.
    unsafe {
        // A's slot is untouched — still naming B, which is still where it
        // was. A rewrite here (or a slide over B) is the use-after-free.
        let Value::Object(Some(target)) = std::ptr::read(a.add(HEADER_SIZE) as *const Value)
        else {
            panic!("A's reference field was clobbered by an abandoned compaction");
        };
        assert_eq!(target.as_ptr(), b);

        // C did not slide over the filler, and its mark bit was cleared so
        // the next cycle starts from a clean slate.
        let c_hdr = &*(c as *const ObjectHeader);
        assert_eq!(
            c_hdr.installed_identity_hash(),
            C_TAG,
            "C must not have moved"
        );
        assert_eq!(
            c_hdr.gc_flags() & GC_FLAG_MARKED,
            0,
            "marks must be cleared"
        );
        assert!(!c_hdr.is_forwarded());
    }
}

/// RandomizedContext WeakHashMap<Thread,...> fix regression: a live
/// old-gen object that stays in place during sliding compaction (the
/// lowest-address survivor, exactly like object A in
/// `compact_promotes_unmarked_target_of_live_ref` above) gets NO
/// `pointer_map` entry by design — but if it is a WATCHED referent
/// (`gc_quiescence::set_watched_referents`), it must get an IDENTITY
/// entry so post-GC reference processing's `is_marked` check can
/// recognize it as alive. An unwatched object that also stays in place
/// must still get no entry at all (bounded cost — this must not start
/// recording every stationary survivor).
#[test]
fn compact_records_identity_map_for_watched_stationary_survivor() {
    let mut og = OldGen::new(4096);

    // Object A: lowest address, stays in place after compaction.
    let a_size = HEADER_SIZE + SLOT_SIZE;
    let a = og.alloc(a_size, 8).unwrap();
    unsafe {
        let a_hdr = &mut *(a as *mut ObjectHeader);
        a_hdr.set_num_slots(1);
        a_hdr.add_gc_flags(GC_FLAG_MARKED);
    }

    // Object B: also stays in place (contiguous with A, nothing to
    // reclaim between them) — left UNWATCHED as a control.
    let b_size = HEADER_SIZE + SLOT_SIZE;
    let b = og.alloc(b_size, 8).unwrap();
    unsafe {
        let b_hdr = &mut *(b as *mut ObjectHeader);
        b_hdr.set_num_slots(1);
        b_hdr.add_gc_flags(GC_FLAG_MARKED);
    }

    cratonvm_gc_core::gc_quiescence::set_watched_referents(&[a as usize]);
    let map = og.compact();
    cratonvm_gc_core::gc_quiescence::set_watched_referents(&[]);

    assert_eq!(
        map.get(&(a as usize)),
        Some(&(a as usize)),
        "watched stationary survivor must get an identity pointer_map entry"
    );
    assert!(
        !map.contains_key(&(b as usize)),
        "unwatched stationary survivor must NOT get a pointer_map entry (bounded cost)"
    );
}

/// GCAUD-8 — [`OldGen::close_live_set`] is the in-place sweep's half of the
/// guard the compactor has had since Phase 0 was written.
///
/// Both halves must hold at once, so both are asserted here:
///
/// * **the fix** — an UNMARKED object that a MARKED object still references
///   is promoted to live, so the sweep that runs next cannot free it under
///   a live pointer;
/// * **the positive control** — an object that is unmarked *and*
///   unreferenced is left exactly as it was. A closure that promoted
///   everything would satisfy the first assertion and reclaim nothing ever
///   again, which is the failure mode this pairing exists to catch.
#[test]
fn close_live_set_promotes_a_referenced_target_and_leaves_real_garbage_dead() {
    let mut og = OldGen::new(4096);

    // A: live referrer.
    let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    // B: A's target, deliberately left unmarked — the object a mark-phase
    // gap loses and the sweep would otherwise free.
    let b = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    // C: unmarked AND unreferenced — genuine garbage, the control.
    let c = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();

    // SAFETY: all three are live blocks of this OldGen, each sized for a
    // header plus the single slot written below.
    unsafe {
        let a_hdr = &mut *(a as *mut ObjectHeader);
        a_hdr.set_num_slots(1);
        a_hdr.add_gc_flags(GC_FLAG_MARKED);
        std::ptr::write(
            a.add(HEADER_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(b))),
        );
        (*(b as *mut ObjectHeader)).set_num_slots(1);
        (*(c as *mut ObjectHeader)).set_num_slots(1);
    }

    let objects = og.walk_objects();
    let (rescued, escaped) = og.close_live_set(&objects);

    assert_eq!(
        rescued, 1,
        "exactly the one referenced-but-unmarked object must be promoted",
    );
    assert!(
        !escaped,
        "every referent here is a walked base — nothing escaped the grid",
    );

    // SAFETY: nothing moved; both pointers still name their object.
    unsafe {
        assert_ne!(
            (*(b as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
            0,
            "an unmarked object a LIVE object references must be retained",
        );
        assert_eq!(
            (*(c as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
            0,
            "POSITIVE CONTROL: unreferenced garbage must stay dead, or the \
             sweep this feeds would stop reclaiming anything at all",
        );
    }

    // Idempotent: a second run has nothing left to do.
    let (rescued_again, _) = og.close_live_set(&objects);
    assert_eq!(rescued_again, 0, "the closure must reach a fixpoint");
}

/// gce e1/c — `close_live_set_collecting` names every object it rescued,
/// both the ones the linear pass reaches ahead of its cursor and the ones
/// its drain picks up behind it, and nothing else: the true-root major
/// sends exactly these back through its mark body.
#[test]
fn gce_e1c_close_live_set_collecting_names_each_rescued_base() {
    let mut og = OldGen::new(4096);
    // Allocation order a, b, c, d: a (marked) -> c (ahead of the cursor),
    // c -> b (behind it, so the drain rescues it), d unreferenced garbage.
    let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let b = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let c = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let d = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    // SAFETY: four live blocks of this OldGen, each sized for a header
    // plus the one slot written below.
    unsafe {
        let a_hdr = &mut *(a as *mut ObjectHeader);
        a_hdr.set_num_slots(1);
        a_hdr.add_gc_flags(GC_FLAG_MARKED);
        std::ptr::write(
            a.add(HEADER_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(c))),
        );
        (*(b as *mut ObjectHeader)).set_num_slots(1);
        (*(c as *mut ObjectHeader)).set_num_slots(1);
        std::ptr::write(
            c.add(HEADER_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(b))),
        );
        (*(d as *mut ObjectHeader)).set_num_slots(1);
    }
    let objects = og.walk_objects();
    let mut out = Vec::new();
    let (rescued, escaped) = og.close_live_set_collecting(&objects, &[], &mut out);
    assert_eq!(rescued, 2);
    assert!(!escaped);
    let mut got = out.clone();
    got.sort_unstable();
    let mut want = vec![b as usize, c as usize];
    want.sort_unstable();
    assert_eq!(got, want, "exactly the rescued bases, each once");
    // SAFETY: nothing moved.
    unsafe {
        assert_eq!(
            (*(d as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
            0,
            "unreferenced garbage stays dead",
        );
    }
    out.clear();
    assert_eq!(og.close_live_set_collecting(&objects, &[], &mut out).0, 0);
    assert!(out.is_empty(), "a fixpoint rescues, and names, nothing");
}

/// GCAUD-8 — the closure must also report the escape the in-place sweep
/// cannot repair: a live object pointing at a block that is ALREADY on the
/// free list. It must not write a mark bit through that address (it is
/// unallocated memory the allocator may reissue at any moment) and it must
/// not promote it into the walked set.
#[test]
fn close_live_set_reports_a_referent_that_is_already_freed() {
    let mut og = OldGen::new(4096);

    let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let b_size = HEADER_SIZE + SLOT_SIZE;
    let b = og.alloc(b_size, 8).unwrap();

    // SAFETY: both are live blocks of this OldGen.
    unsafe {
        let a_hdr = &mut *(a as *mut ObjectHeader);
        a_hdr.set_num_slots(1);
        a_hdr.add_gc_flags(GC_FLAG_MARKED);
        std::ptr::write(
            a.add(HEADER_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(b))),
        );
        (*(b as *mut ObjectHeader)).set_num_slots(1);
    }

    // The state an under-marking sweep leaves behind: B freed, A still
    // naming it, B's bytes not zeroed.
    // SAFETY: `(b, b_size)` is exactly the pair `alloc` handed out.
    unsafe { og.free(b, b_size) };

    let objects = og.walk_objects();
    let (rescued, escaped) = og.close_live_set(&objects);

    assert!(
        escaped,
        "a referent outside the object walk must be reported"
    );
    assert_eq!(rescued, 0, "nothing in the walked set needed promoting");
    // SAFETY: `b`'s block is unallocated but still mapped inside `og`.
    unsafe {
        assert_eq!(
            (*(b as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
            0,
            "the closure must NOT write a mark bit into a free block",
        );
    }
}

// -----------------------------------------------------------------------
// gen r4/oldgen (2026-09-23)
// -----------------------------------------------------------------------

/// The live-set closure as it was before round 4 — a re-scan-everything
/// fixpoint — kept as the ORACLE the worklist form is held to. Same
/// admission proof, same slot walk, same mark writes; only the iteration
/// strategy differs.
fn reference_fixpoint_closure(
    objects: &[(*mut u8, usize)],
    data: (usize, usize),
) -> (usize, bool) {
    let is_walked_base = |addr: usize| {
        objects
            .binary_search_by_key(&addr, |&(p, _)| p as usize)
            .is_ok()
    };
    let mut escaped = false;
    let mut promoted_total = 0usize;
    loop {
        let mut promoted_any = false;
        for &(obj_ptr, size) in objects {
            // SAFETY: `obj_ptr` is a walked object base.
            let is_marked =
                unsafe { (*(obj_ptr as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 };
            if !is_marked {
                continue;
            }
            OldGen::for_each_old_gen_ref(obj_ptr, size, data, |ref_ptr| {
                if !is_walked_base(ref_ptr) {
                    escaped = true;
                    return;
                }
                // SAFETY: `ref_ptr` is a walked object base.
                let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                if ref_header.gc_flags() & GC_FLAG_MARKED == 0 {
                    ref_header.add_gc_flags(GC_FLAG_MARKED);
                    promoted_any = true;
                    promoted_total += 1;
                }
            });
        }
        if !promoted_any {
            break;
        }
    }
    (promoted_total, escaped)
}

/// Slots per object in [`graph_old_gen`].
const GRAPH_SLOTS: usize = 3;
/// Footprint of one [`graph_old_gen`] object.
const GRAPH_OBJ: usize = HEADER_SIZE + GRAPH_SLOTS * SLOT_SIZE;

/// `n` legacy objects laid out back to back, slot `s` of object `i`
/// holding a reference to object `edges[i][s]` (or null), and exactly the
/// objects with `marked[i]` carrying `GC_FLAG_MARKED`.
fn graph_old_gen(
    edges: &[[Option<usize>; GRAPH_SLOTS]],
    marked: &[bool],
) -> (OldGen, Vec<*mut u8>) {
    let n = edges.len();
    let mut og = OldGen::new(64 * 1024);
    let mut ptrs = Vec::with_capacity(n);
    for _ in 0..n {
        let p = og
            .alloc(GRAPH_OBJ, 8)
            .expect("64 KiB holds the whole graph");
        // SAFETY: `p` is a fresh, zeroed allocation of `GRAPH_OBJ` bytes.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(
                    cratonvm_types::ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    GRAPH_SLOTS as u32,
                ),
            );
        }
        ptrs.push(p);
    }
    for (i, (row, &live)) in edges.iter().zip(marked).enumerate() {
        for (s, target) in row.iter().enumerate() {
            // SAFETY: every `ptrs[t]` is a live, 8-aligned object base of
            // this generation, and slot `s < GRAPH_SLOTS` lies inside
            // object `i`'s body.
            unsafe {
                let v = match target {
                    Some(t) => Value::Object(Some(ObjectRef::from_raw(ptrs[*t]))),
                    None => Value::Object(None),
                };
                std::ptr::write(ptrs[i].add(HEADER_SIZE + s * SLOT_SIZE) as *mut Value, v);
            }
        }
        if live {
            // SAFETY: `ptrs[i]` holds the header written above.
            unsafe { (*(ptrs[i] as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
        }
    }
    (og, ptrs)
}

/// xorshift64 — deterministic, dependency-free.
struct Xorshift(u64);
impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn is_marked(p: *mut u8) -> bool {
    // SAFETY: callers pass object bases of a live test generation.
    unsafe { (*(p as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 }
}

/// gen r4/oldgen — design item 5: the worklist closure must produce the
/// SAME marked set, the same rescue count and the same escape verdict as
/// the fixpoint it replaced, on graphs chosen to hit every shape the
/// fixpoint handled differently: sparse and dense random graphs with
/// self-loops, a chain whose every link points BACKWARDS in address order
/// (the fixpoint's worst case — one full pass per link), a backward chain
/// with random cross edges, and all of those with a referenced block
/// already on the free list so the escape arm is compared too.
#[test]
fn the_worklist_closure_matches_the_old_fixpoint_exactly() {
    const N: usize = 120;
    for seed in 1..=64u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        let mut marked = vec![false; N];
        let shape = seed % 4;
        for (i, (row, live)) in edges.iter_mut().zip(marked.iter_mut()).enumerate() {
            for (s, slot) in row.iter_mut().enumerate() {
                *slot = match shape {
                    0 => {
                        if rng.below(3) == 0 {
                            Some(rng.below(N))
                        } else {
                            None
                        }
                    }
                    1 => Some(rng.below(N)),
                    2 => {
                        if s == 0 && i > 0 {
                            Some(i - 1)
                        } else {
                            None
                        }
                    }
                    _ => {
                        if s == 0 && i > 0 {
                            Some(i - 1)
                        } else if rng.below(4) == 0 {
                            Some(rng.below(N))
                        } else {
                            None
                        }
                    }
                };
            }
            *live = if shape == 2 {
                i == N - 1
            } else {
                rng.below(8) == 0
            };
        }
        // Half the cases: one unmarked object is already free, so a
        // referrer that reaches it is an ESCAPE in both algorithms.
        let freed = if seed % 2 == 0 {
            (0..N).rev().find(|&i| !marked[i])
        } else {
            None
        };
        let build = || {
            let (mut og, ptrs) = graph_old_gen(&edges, &marked);
            if let Some(f) = freed {
                // SAFETY: `(ptrs[f], GRAPH_OBJ)` is exactly what `alloc`
                // handed out.
                unsafe { og.free(ptrs[f], GRAPH_OBJ) };
            }
            (og, ptrs)
        };
        let (oracle_og, oracle_ptrs) = build();
        let (og, ptrs) = build();
        let oracle_walk = oracle_og.walk_objects();
        let walk = og.walk_objects();
        assert_eq!(oracle_walk.len(), walk.len());

        let expected = reference_fixpoint_closure(&oracle_walk, oracle_og.extent());
        let got = og.close_live_set(&walk);
        assert_eq!(
            got, expected,
            "seed {seed} (shape {shape}): (rescued, escaped) differs from the fixpoint",
        );
        for (i, (&p, &q)) in ptrs.iter().zip(&oracle_ptrs).enumerate() {
            if Some(i) == freed {
                continue;
            }
            assert_eq!(
                is_marked(p),
                is_marked(q),
                "seed {seed} (shape {shape}): object {i}'s mark differs from the fixpoint",
            );
        }
    }
}

/// What `gcj_j1og_the_forward_index_compacts_exactly_as_the_hash_map`
/// compares: did the compaction use the index, the pointer map and every
/// survivor's slots (as offsets from the base, ascending), and `used`.
type CompactSnapshot = (
    bool,
    Vec<(usize, usize)>,
    Vec<(usize, Vec<Option<usize>>)>,
    usize,
);

fn gcj_j1og_compact_snapshot(
    edges: &[[Option<usize>; GRAPH_SLOTS]],
    marked: &[bool],
    index: &str,
) -> CompactSnapshot {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX", Some(index))],
        || {
            let (mut og, _ptrs) = graph_old_gen(edges, marked);
            let base = og.base_ptr() as usize;
            let before = og.forward_index_compactions();
            let map = og.compact();
            let used_index = og.forward_index_compactions() != before;
            let mut moved: Vec<(usize, usize)> =
                map.iter().map(|(&s, &d)| (s - base, d - base)).collect();
            moved.sort_unstable();
            let mut heap: Vec<(usize, Vec<Option<usize>>)> = Vec::new();
            for (p, _size) in og.walk_objects() {
                let slots: Vec<Option<usize>> = (0..GRAPH_SLOTS)
                    .map(|s| {
                        // SAFETY: a survivor base of this graph's layout,
                        // slot `s` inside its body.
                        let slot = unsafe { p.add(HEADER_SIZE + s * SLOT_SIZE) };
                        // SAFETY: as above; the slot holds a `Value`.
                        match unsafe { std::ptr::read(slot as *const Value) } {
                            Value::Object(Some(r)) => Some(r.as_ptr() as usize - base),
                            _ => None,
                        }
                    })
                    .collect();
                heap.push((p as usize - base, slots));
            }
            (used_index, moved, heap, og.used())
        },
    )
}

/// gcj j1/og (`CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX`) — the full-GC slide
/// over random graphs (dead runs between survivors, so most survivors
/// move) leaves the SAME heap with the sorted forwarding index as with the
/// hash map it replaced: the same pointer map, the same survivors at the
/// same offsets, every reference slot naming the same object. Offsets, not
/// addresses: the two generations have different bases.
#[test]
fn gcj_j1og_the_forward_index_compacts_exactly_as_the_hash_map() {
    const N: usize = 600;
    for seed in 1..=24u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
        let mut marked = vec![false; N];
        for m in marked.iter_mut() {
            *m = rng.below(3) != 0;
        }
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        for (i, row) in edges.iter_mut().enumerate() {
            for slot in row.iter_mut() {
                if rng.below(4) != 0 {
                    // Mostly marked targets; a dead target of a live holder
                    // exercises Phase 0's rescue on both arms.
                    let t = rng.below(N);
                    if marked[t] || rng.below(8) == 0 || !marked[i] {
                        *slot = Some(t);
                    }
                }
            }
        }
        let (on_used, on_moved, on_heap, on_bytes) =
            gcj_j1og_compact_snapshot(&edges, &marked, "1");
        let (off_used, off_moved, off_heap, off_bytes) =
            gcj_j1og_compact_snapshot(&edges, &marked, "0");
        assert!(on_used, "seed {seed}: the default arm resolved through the index");
        assert!(!off_used, "seed {seed}: `=0` kept the hash map");
        assert_eq!(on_moved, off_moved, "seed {seed}: pointer maps differ");
        assert_eq!(on_heap, off_heap, "seed {seed}: compacted heaps differ");
        assert_eq!(on_bytes, off_bytes, "seed {seed}: used bytes differ");
        assert!(!on_moved.is_empty(), "seed {seed}: the shape moved nothing");
    }
}

/// The pinned compaction's base-independent snapshot for
/// `gcj_j2x1_the_pinned_forward_index_compacts_exactly_as_the_hash_map`:
/// whether Phase 2 resolved through the index, the pointer map, every
/// survivor's slots (an address outside the generation -- the stand-in
/// young word -- as `usize::MAX`), `used`, and the young-reference holders,
/// all as offsets from the base.
fn gcj_j2x1_pinned_compact_snapshot(
    edges: &[[Option<usize>; GRAPH_SLOTS]],
    marked: &[bool],
    pins: &[usize],
    young_holders: &[usize],
    index: &str,
) -> (CompactSnapshot, Vec<usize>) {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX", Some(index))],
        || {
            let (mut og, ptrs) = graph_old_gen(edges, marked);
            // A stand-in "young generation": four words outside old gen.
            let young_words = [0u64; 4];
            let y = young_words.as_ptr() as usize;
            for &h in young_holders {
                // SAFETY: the last slot of a graph object, inside its body.
                unsafe {
                    std::ptr::write(
                        ptrs[h].add(HEADER_SIZE + (GRAPH_SLOTS - 1) * SLOT_SIZE) as *mut Value,
                        Value::Object(Some(ObjectRef::from_raw(y as *mut u8))),
                    );
                }
            }
            let base = og.base_ptr() as usize;
            let (lo, hi) = og.extent();
            let walk = og.walk_objects();
            let seq = og.free_list_seq();
            let before = og.forward_index_compactions();
            let done = og
                .compact_around_pins(
                    &walk,
                    seq,
                    pins.iter().map(|&i| ptrs[i] as usize).collect(),
                    (y, y + 32),
                    &HashMap::new(),
                )
                .expect("a healthy grid compacts");
            let used_index = og.forward_index_compactions() != before;
            let mut moved: Vec<(usize, usize)> = done
                .pointer_map
                .iter()
                .map(|(&s, &d)| (s - base, d - base))
                .collect();
            moved.sort_unstable();
            let mut heap: Vec<(usize, Vec<Option<usize>>)> = Vec::new();
            for (p, _size) in og.walk_objects() {
                let slots: Vec<Option<usize>> = (0..GRAPH_SLOTS)
                    .map(|s| {
                        // SAFETY: a survivor base of this graph's layout,
                        // slot `s` inside its body.
                        let slot = unsafe { p.add(HEADER_SIZE + s * SLOT_SIZE) };
                        // SAFETY: as above; the slot holds a `Value`.
                        match unsafe { std::ptr::read(slot as *const Value) } {
                            Value::Object(Some(r)) => {
                                let a = r.as_ptr() as usize;
                                Some(if (lo..hi).contains(&a) {
                                    a - base                                    } else {
                                    usize::MAX                                    })                                }
                            _ => None,
                        }
                    })
                    .collect();
                heap.push((p as usize - base, slots));
            }
            let mut holders: Vec<usize> =
                done.young_ref_holders.iter().map(|&h| h - base).collect();
            holders.sort_unstable();
            ((used_index, moved, heap, og.used()), holders)
        },
    )
}

/// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 1,
/// `CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX`) — the PINNED compaction
/// (`compact_around_pins`, Phase 2 through `CompactDestinations`) over
/// random graphs with random pins and young-holding survivors leaves the
/// same heap with the sorted index as with the hash map: the same pointer
/// map, survivors, slots, `used` and young-reference holders, and the
/// engagement count rises only under `=1`.
#[test]
fn gcj_j2x1_the_pinned_forward_index_compacts_exactly_as_the_hash_map() {
    const N: usize = 400;
    for seed in 1..=16u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut marked = vec![false; N];
        for m in marked.iter_mut() {
            *m = rng.below(3) != 0;
        }
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        for (i, row) in edges.iter_mut().enumerate() {
            // The last slot is the young word's, for the holders below.
            for slot in row.iter_mut().take(GRAPH_SLOTS - 1) {
                if rng.below(4) != 0 {
                    let t = rng.below(N);
                    if marked[t] || rng.below(8) == 0 || !marked[i] {
                        *slot = Some(t);
                    }
                }
            }
        }
        let pins: Vec<usize> = (0..N)
            .filter(|&i| marked[i] && rng.below(16) == 0)
            .collect();
        let holders: Vec<usize> = (0..N)
            .filter(|&i| marked[i] && rng.below(10) == 0)
            .collect();
        let ((on_used, on_moved, on_heap, on_bytes), on_holders) =
            gcj_j2x1_pinned_compact_snapshot(&edges, &marked, &pins, &holders, "1");
        let ((off_used, off_moved, off_heap, off_bytes), off_holders) =
            gcj_j2x1_pinned_compact_snapshot(&edges, &marked, &pins, &holders, "0");
        assert!(on_used, "seed {seed}: `=1` resolved through the index");
        assert!(!off_used, "seed {seed}: `=0` kept the hash map");
        assert_eq!(on_moved, off_moved, "seed {seed}: pointer maps differ");
        assert_eq!(on_heap, off_heap, "seed {seed}: compacted heaps differ");
        assert_eq!(on_bytes, off_bytes, "seed {seed}: used bytes differ");
        assert_eq!(on_holders, off_holders, "seed {seed}: young holders differ");
        assert!(!on_moved.is_empty(), "seed {seed}: the shape moved nothing");
        assert!(
            !on_holders.is_empty(),
            "seed {seed}: no survivor holds young"
        );
    }
}

/// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 3, R1 of
/// commit `d92c635aa5`) — `for_each_old_gen_ref`'s COMPACT arm is capped by
/// the WALKED size: a compact object whose registered layout is larger than
/// the size the walk recorded never reports a reference slot at or past
/// that size (every slot of the full layout holds an in-generation address,
/// so an uncapped walk would report all four).
#[test]
fn gcj_j2x1_the_compact_arm_of_for_each_old_gen_ref_stops_at_the_walked_size() {
    const CID: u32 = 4391;
    const DOMAIN: u32 = 0x2_0391;
    let disps: Vec<u32> = (0..4).map(|i| (HEADER_SIZE + i * 8) as u32).collect();
    let full = HEADER_SIZE + 4 * 8;
    cratonvm_types::register_class_layout(
        DOMAIN,
        CID,
        std::sync::Arc::new(cratonvm_types::CompactLayout {
            field_disps: disps.clone(),
            is_ref: vec![true; 4],
            field_kinds: vec![cratonvm_types::FieldStorageKind::Reference; 4],
            ref_disps: disps.clone(),
            total_size: full as u32,
        }),
    );
    let mut og = OldGen::new(64 * 1024);
    let target = og.alloc(64, 8).expect("room");
    let p = og.alloc(full, 8).expect("room");
    // SAFETY: `p` is a fresh, zeroed allocation of `full` bytes.
    unsafe {
        std::ptr::write(
            p as *mut ObjectHeader,
            ObjectHeader::new(
                cratonvm_types::ClassId::new(CID),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                4,
            ),
        );
        (*(p as *mut ObjectHeader)).add_gc_flags(cratonvm_types::GC_FLAG_COMPACT);
        for (i, &d) in disps.iter().enumerate() {
            // Slot `i` names a distinct in-generation address.
            std::ptr::write(p.add(d as usize) as *mut u64, target as u64 + 8 * i as u64);
        }
    }
    let report = |walked: usize| -> Vec<usize> {
        let mut seen = Vec::new();
        OldGen::for_each_old_gen_ref(p, walked, og.extent(), |r| {
            seen.push((r - target as usize) / 8)
        });
        seen
    };
    assert_eq!(
        report(full),
        vec![0, 1, 2, 3],
        "the walked size covers the layout"
    );
    let walked = HEADER_SIZE + 2 * 8;
    let capped = report(walked);
    assert_eq!(
        capped,
        vec![0, 1],
        "slots at or past the walked size are not read"
    );
    for i in capped {
        assert!(
            disps[i] as usize + 8 <= walked,
            "slot {i} lies inside the walked size"
        );
    }
}

/// gcj j2/og2 — the full-GC slide with its Phase 2 on `workers` threads
/// (`0`: the serial loop), as a base-independent snapshot: whether the
/// parallel arm ran, the pointer map, every survivor's slots, `used`.
fn gcj_j2og2_par_adjust_snapshot(
    edges: &[[Option<usize>; GRAPH_SLOTS]],
    marked: &[bool],
    workers: usize,
) -> CompactSnapshot {
    let (mut og, _ptrs) = graph_old_gen(edges, marked);
    og.set_par_adjust_for_test(workers, 2);
    let base = og.base_ptr() as usize;
    let before = og.par_adjust_compactions();
    let map = og.compact();
    let ran_parallel = og.par_adjust_compactions() != before;
    let mut moved: Vec<(usize, usize)> =
        map.iter().map(|(&s, &d)| (s - base, d - base)).collect();
    moved.sort_unstable();
    let mut heap: Vec<(usize, Vec<Option<usize>>)> = Vec::new();
    for (p, _size) in og.walk_objects() {
        let slots: Vec<Option<usize>> = (0..GRAPH_SLOTS)
            .map(|s| {
                // SAFETY: a survivor base of this graph's layout, slot `s`
                // inside its body.
                let slot = unsafe { p.add(HEADER_SIZE + s * SLOT_SIZE) };
                // SAFETY: as above; the slot holds a `Value`.
                match unsafe { std::ptr::read(slot as *const Value) } {
                    Value::Object(Some(r)) => Some(r.as_ptr() as usize - base),
                    _ => None,
                }
            })
            .collect();
        heap.push((p as usize - base, slots));
    }
    (ran_parallel, moved, heap, og.used())
}

/// gcj j2/og2 (`CRATONVM_GC_OLD_PAR_ADJUST`) — the full-GC slide with its
/// Phase 2 split over 2, 3 and 7 workers leaves the SAME heap as the serial
/// loop on random graphs: the same pointer map, the same survivors, every
/// slot naming the same object. The serial arm (`workers = 0`) does not
/// count a parallel compaction; every armed one does.
#[test]
fn gcj_j2og2_the_parallel_adjust_compacts_exactly_as_the_serial_loop() {
    const N: usize = 600;
    for seed in 1..=12u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut marked = vec![false; N];
        for m in marked.iter_mut() {
            *m = rng.below(3) != 0;
        }
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        for (i, row) in edges.iter_mut().enumerate() {
            for slot in row.iter_mut() {
                if rng.below(4) != 0 {
                    let t = rng.below(N);
                    if marked[t] || rng.below(8) == 0 || !marked[i] {
                        *slot = Some(t);
                    }
                }
            }
        }
        let (serial_par, serial_moved, serial_heap, serial_used) =
            gcj_j2og2_par_adjust_snapshot(&edges, &marked, 0);
        assert!(!serial_par, "seed {seed}: workers = 0 is the serial loop");
        assert!(!serial_moved.is_empty(), "seed {seed}: the shape moved nothing");
        for workers in [2usize, 3, 7] {
            let (par, moved, heap, used) =
                gcj_j2og2_par_adjust_snapshot(&edges, &marked, workers);
            assert!(par, "seed {seed} x{workers}: the parallel arm ran");
            assert_eq!(moved, serial_moved, "seed {seed} x{workers}: pointer maps");
            assert_eq!(heap, serial_heap, "seed {seed} x{workers}: compacted heaps");
            assert_eq!(used, serial_used, "seed {seed} x{workers}: used bytes");
        }
    }
}

/// t1 lane F (gcj-j1og step 1, pinned half) — the pinned compaction with
/// its Phase 2 on `workers` threads, as a base-independent snapshot:
/// whether the parallel arm ran, the pointer map, the young-holder list
/// IN ORDER, every survivor's slots, `used`. `young_slice` is a byte range
/// of the generation (offsets) treated as "young", so some slots take the
/// holder-noting arm; `pin` pins the object at that index.
fn t1_f_pinned_par_adjust_snapshot(
    edges: &[[Option<usize>; GRAPH_SLOTS]],
    marked: &[bool],
    workers: usize,
    young_slice: (usize, usize),
    pin: usize,
) -> (bool, Vec<(usize, usize)>, Vec<usize>, Vec<(usize, Vec<Option<usize>>)>, usize) {
    let (mut og, ptrs) = graph_old_gen(edges, marked);
    og.set_par_adjust_for_test(workers, 2);
    let base = og.base_ptr() as usize;
    let before = og.par_adjust_compactions();
    let walk = og.walk_objects();
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins(
            &walk,
            seq,
            vec![ptrs[pin] as usize],
            (base + young_slice.0, base + young_slice.1),
            &HashMap::new(),
        )
        .expect("compacts");
    let ran_parallel = og.par_adjust_compactions() != before;
    let mut moved: Vec<(usize, usize)> =
        done.pointer_map.iter().map(|(&s, &d)| (s - base, d - base)).collect();
    moved.sort_unstable();
    let holders: Vec<usize> = done.young_ref_holders.iter().map(|&h| h - base).collect();
    let mut heap: Vec<(usize, Vec<Option<usize>>)> = Vec::new();
    for (p, _size) in og.walk_objects() {
        let slots: Vec<Option<usize>> = (0..GRAPH_SLOTS)
            .map(|s| {
                // SAFETY: a survivor base of this graph's layout, slot `s`
                // inside its body; the slot holds a `Value`.
                let slot = unsafe { p.add(HEADER_SIZE + s * SLOT_SIZE) };
                match unsafe { std::ptr::read(slot as *const Value) } {
                    Value::Object(Some(r)) => Some((r.as_ptr() as usize).wrapping_sub(base)),
                    _ => None,
                }
            })
            .collect();
        heap.push((p as usize - base, slots));
    }
    (ran_parallel, moved, holders, heap, og.used())
}

/// t1 lane F (`CRATONVM_GC_OLD_PAR_ADJUST`, the pinned compactor's Phase 2)
/// — split over 2, 3 and 7 workers it leaves the SAME heap, pointer map
/// and young-holder list (order included) as the serial loop.
#[test]
fn t1_f_the_pinned_parallel_adjust_compacts_exactly_as_the_serial_loop() {
    const N: usize = 600;
    for seed in 1..=8u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        let mut marked = vec![false; N];
        for m in marked.iter_mut() {
            *m = rng.below(3) != 0;
        }
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        for (i, row) in edges.iter_mut().enumerate() {
            for slot in row.iter_mut() {
                if rng.below(4) != 0 {
                    let t = rng.below(N);
                    if marked[t] || rng.below(8) == 0 || !marked[i] {
                        *slot = Some(t);
                    }
                }
            }
        }
        // A pinned live object somewhere in the middle.
        let pin = (N / 2..N).find(|&i| marked[i]).expect("a live object");
        // The "young" slice: a stretch near the top of the generation, so
        // references into it are noted and left (deterministically).
        let obj = GRAPH_OBJ;
        let young = ((N - 40) * obj, N * obj);
        let serial = t1_f_pinned_par_adjust_snapshot(&edges, &marked, 0, young, pin);
        assert!(!serial.0, "seed {seed}: workers = 0 is the serial loop");
        assert!(!serial.2.is_empty(), "seed {seed}: no young holder noted");
        for workers in [2usize, 3, 7] {
            let par = t1_f_pinned_par_adjust_snapshot(&edges, &marked, workers, young, pin);
            assert!(par.0, "seed {seed} x{workers}: the parallel arm ran");
            assert_eq!(par.1, serial.1, "seed {seed} x{workers}: pointer maps");
            assert_eq!(par.2, serial.2, "seed {seed} x{workers}: young holders");
            assert_eq!(par.3, serial.3, "seed {seed} x{workers}: compacted heaps");
            assert_eq!(par.4, serial.4, "seed {seed} x{workers}: used bytes");
        }
    }
}

/// gcj j2/og2 — the switch's value rule and the chunk plan, pure.
#[test]
fn gcj_j2og2_the_par_adjust_switch_and_chunk_plan() {
    assert_eq!(par_adjust_workers_from(None, 8), 4);
    for off in ["", "0", "off", "false", "no", "x"] {
        assert_eq!(par_adjust_workers_from(Some(off), 8), 0, "{off:?}");
    }
    assert_eq!(par_adjust_workers_from(Some("1"), 8), 4);
    assert_eq!(par_adjust_workers_from(Some("on"), 2), 2);
    assert_eq!(par_adjust_workers_from(Some("1"), 1), 1);
    assert_eq!(par_adjust_workers_from(Some(" 6 "), 2), 6);
    assert_eq!(par_adjust_workers_from(Some("99"), 2), PAR_ADJUST_MAX_WORKERS);
    // Serial unless two workers and the floor are reached.
    assert_eq!(par_adjust_chunks(1_000_000, 0, PAR_ADJUST_MIN_SURVIVORS), 1);
    assert_eq!(par_adjust_chunks(1_000_000, 1, PAR_ADJUST_MIN_SURVIVORS), 1);
    assert_eq!(par_adjust_chunks(PAR_ADJUST_MIN_SURVIVORS - 1, 4, PAR_ADJUST_MIN_SURVIVORS), 1);
    assert_eq!(par_adjust_chunks(PAR_ADJUST_MIN_SURVIVORS, 4, PAR_ADJUST_MIN_SURVIVORS), 4);
    assert_eq!(par_adjust_chunks(3, 8, 2), 3);
    assert_eq!(par_adjust_chunks(1, 8, 0), 1);
}

/// The fixpoint's worst case, directly: a chain whose every link points to
/// the object BELOW it, with only the top marked. The fixpoint needed one
/// full pass over the generation per link; the worklist rescues the whole
/// chain in its single linear pass plus one drain.
#[test]
fn a_backward_chain_is_closed_by_one_pass_and_a_drain() {
    const N: usize = 200;
    let mut edges = vec![[None; GRAPH_SLOTS]; N];
    for (i, e) in edges.iter_mut().enumerate().skip(1) {
        e[0] = Some(i - 1);
    }
    let mut marked = vec![false; N];
    marked[N - 1] = true;
    let (og, ptrs) = graph_old_gen(&edges, &marked);
    let walk = og.walk_objects();
    let deep_before = CLOSE_LIVE_SET_DEEP_CALLS.load(std::sync::atomic::Ordering::Relaxed);
    let scans_before = CLOSE_LIVE_SET_WORKLIST_SCANS.load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(og.close_live_set(&walk), (N - 1, false));
    assert!(
        ptrs.iter().all(|&p| is_marked(p)),
        "the whole chain is live"
    );
    // Process-global and monotone, so `>=` is exact enough under a
    // parallel test harness: other tests can only ADD.
    assert!(
        CLOSE_LIVE_SET_DEEP_CALLS.load(std::sync::atomic::Ordering::Relaxed) > deep_before,
        "a backward chain is precisely what the old fixpoint needed 3+ passes for",
    );
    assert!(
        CLOSE_LIVE_SET_WORKLIST_SCANS.load(std::sync::atomic::Ordering::Relaxed)
            >= scans_before + (N - 1) as u64,
        "every rescued link behind the cursor is scanned by the drain",
    );
    // Idempotent.
    assert_eq!(og.close_live_set(&walk), (0, false));
}

/// gcg g2/e — `CRATONVM_GC_OLD_CLOSE_BITMAP`: the closure admitting
/// through a start bitmap gives the searched closure's marked set, rescue
/// count and escape verdict, on the same graph shapes as the fixpoint test
/// (random sparse / dense, backward chains, a referenced freed block),
/// and rescues the whole backward chain through its drain.
#[test]
fn gcg_g2e_the_bitmap_closure_matches_the_searched_closure_exactly() {
    const N: usize = 120;
    for seed in 1..=64u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        let mut marked = vec![false; N];
        let shape = seed % 4;
        for (i, (row, live)) in edges.iter_mut().zip(marked.iter_mut()).enumerate() {
            for (s, slot) in row.iter_mut().enumerate() {
                *slot = match shape {
                    0 => (rng.below(3) == 0).then(|| rng.below(N)),
                    1 => Some(rng.below(N)),
                    2 => (s == 0 && i > 0).then(|| i - 1),
                    _ => {
                        if s == 0 && i > 0 {
                            Some(i - 1)
                        } else if rng.below(4) == 0 {
                            Some(rng.below(N))
                        } else {
                            None
                        }
                    }
                };
            }
            *live = if shape == 2 {
                i == N - 1
            } else {
                rng.below(8) == 0
            };
        }
        let freed = if seed % 2 == 0 {
            (0..N).rev().find(|&i| !marked[i])
        } else {
            None
        };
        let build = || {
            let (mut og, ptrs) = graph_old_gen(&edges, &marked);
            if let Some(f) = freed {
                // SAFETY: `(ptrs[f], GRAPH_OBJ)` is exactly what `alloc`
                // handed out.
                unsafe { og.free(ptrs[f], GRAPH_OBJ) };
            }
            (og, ptrs)
        };
        let (s_og, s_ptrs) = build();
        let (b_og, b_ptrs) = build();
        let s_walk = s_og.walk_objects();
        let b_walk = b_og.walk_objects();
        let searched =
            OldGen::close_live_set_impl(&s_walk, s_og.extent(), &[], None, false, 0);
        let mut rescued = Vec::new();
        let bitmap =
            OldGen::close_live_set_impl(&b_walk, b_og.extent(), &[], Some(&mut rescued), true, 0);
        assert_eq!(bitmap, searched, "seed {seed} (shape {shape})");
        assert_eq!(
            rescued.len(),
            bitmap.0,
            "seed {seed}: every rescue is reported"
        );
        for (i, (&p, &q)) in b_ptrs.iter().zip(&s_ptrs).enumerate() {
            if Some(i) == freed {
                continue;
            }
            assert_eq!(
                is_marked(p),
                is_marked(q),
                "seed {seed} (shape {shape}): object {i}"
            );
        }
    }
}

/// t1 w8 J1OG — `CRATONVM_GC_OLD_PAR_CLOSE`: the closure with the
/// parallel read-only verification on 2/3/7 workers gives the serial
/// closure's verdict, rescues (same objects, same order) and marked set,
/// on the fixpoint test's graph shapes (an under-marked first call falls
/// back to the serial body), and a second call over the now-closed set is
/// answered by the verification alone with the serial escape verdict.
#[test]
fn t1_w8_j1og_the_parallel_close_verification_matches_the_serial_closure() {
    use std::sync::atomic::Ordering::Relaxed;
    const N: usize = 120;
    for seed in 1..=64u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        let mut marked = vec![false; N];
        let shape = seed % 4;
        for (i, (row, live)) in edges.iter_mut().zip(marked.iter_mut()).enumerate() {
            for (s, slot) in row.iter_mut().enumerate() {
                *slot = match shape {
                    0 => (rng.below(3) == 0).then(|| rng.below(N)),
                    1 => Some(rng.below(N)),
                    2 => (s == 0 && i > 0).then(|| i - 1),
                    _ => {
                        if s == 0 && i > 0 {
                            Some(i - 1)
                        } else if rng.below(4) == 0 {
                            Some(rng.below(N))
                        } else {
                            None
                        }
                    }
                };
            }
            *live = if shape == 2 {
                i == N - 1
            } else {
                rng.below(8) == 0
            };
        }
        let freed = if seed % 2 == 0 {
            (0..N).rev().find(|&i| !marked[i])
        } else {
            None
        };
        let build = || {
            let (mut og, ptrs) = graph_old_gen(&edges, &marked);
            if let Some(f) = freed {
                // SAFETY: `(ptrs[f], GRAPH_OBJ)` is exactly what `alloc`
                // handed out.
                unsafe { og.free(ptrs[f], GRAPH_OBJ) };
            }
            (og, ptrs)
        };
        let (s_og, s_ptrs) = build();
        let s_walk = s_og.walk_objects();
        let mut s_rescued = Vec::new();
        let s_first = s_og.close_live_set_collecting(&s_walk, &[], &mut s_rescued);
        let s_second = s_og.close_live_set(&s_walk);
        assert_eq!(s_second.0, 0, "seed {seed}: the serial closure is a fixed point");
        for workers in [2usize, 3, 7] {
            let (mut p_og, p_ptrs) = build();
            p_og.set_par_close_workers_for_test(workers);
            let p_walk = p_og.walk_objects();
            let mut p_rescued = Vec::new();
            let p_first = p_og.close_live_set_collecting(&p_walk, &[], &mut p_rescued);
            assert_eq!(p_first, s_first, "seed {seed} workers {workers}: first call");
            let off = |v: &[usize], base: &[*mut u8]| -> Vec<usize> {
                v.iter()
                    .map(|&a| base.iter().position(|&p| p as usize == a).unwrap())
                    .collect()
            };
            assert_eq!(
                off(&p_rescued, &p_ptrs),
                off(&s_rescued, &s_ptrs),
                "seed {seed} workers {workers}: rescues and their order"
            );
            let verified = CLOSE_LIVE_SET_PAR_VERIFIED.load(Relaxed);
            let p_second = p_og.close_live_set(&p_walk);
            assert_eq!(p_second, s_second, "seed {seed} workers {workers}: second call");
            assert!(
                CLOSE_LIVE_SET_PAR_VERIFIED.load(Relaxed) > verified,
                "seed {seed} workers {workers}: the closed set is answered in parallel"
            );
            for (i, (&p, &q)) in p_ptrs.iter().zip(&s_ptrs).enumerate() {
                if Some(i) == freed {
                    continue;
                }
                assert_eq!(
                    is_marked(p),
                    is_marked(q),
                    "seed {seed} workers {workers}: object {i}"
                );
            }
        }
    }
}

/// gcg g3/e — `CRATONVM_GC_OLD_SHARED_STARTS`: the closures admitting
/// through ONE start set built per collection (the in-mark
/// `close_live_set_collecting_shared`, then the sweep's
/// `close_live_set_shared` over the same grid) give the searched
/// closures' marked set, rescues (same objects, same order), and escape
/// verdicts, on the same graph shapes as the g2/e test, with no size
/// floor (the shared set is built whatever the grid's size).
#[test]
fn gcg_g3e_a_shared_start_set_closes_exactly_as_the_searched_closure() {
    const N: usize = 96;
    for seed in 1..=48u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0xD6E8_FEB8_6659_FD93) | 1);
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        let mut marked = vec![false; N];
        let shape = seed % 4;
        for (i, (row, live)) in edges.iter_mut().zip(marked.iter_mut()).enumerate() {
            for (s, slot) in row.iter_mut().enumerate() {
                *slot = match shape {
                    0 => (rng.below(3) == 0).then(|| rng.below(N)),
                    1 => Some(rng.below(N)),
                    2 => (s == 0 && i > 0).then(|| i - 1),
                    _ => {
                        if s == 0 && i > 0 {
                            Some(i - 1)
                        } else if rng.below(4) == 0 {
                            Some(rng.below(N))
                        } else {
                            None
                        }
                    }
                };
            }
            *live = if shape == 2 {
                i == N - 1
            } else {
                rng.below(8) == 0
            };
        }
        let freed = if seed % 2 == 0 {
            (0..N).rev().find(|&i| !marked[i])
        } else {
            None
        };
        let build = || {
            let (mut og, ptrs) = graph_old_gen(&edges, &marked);
            if let Some(f) = freed {
                // SAFETY: `(ptrs[f], GRAPH_OBJ)` is exactly what `alloc`
                // handed out.
                unsafe { og.free(ptrs[f], GRAPH_OBJ) };
            }
            (og, ptrs)
        };
        let (s_og, s_ptrs) = build();
        let (b_og, b_ptrs) = build();
        let s_walk = s_og.walk_objects();
        let b_walk = b_og.walk_objects();
        let starts = WalkStartBits::build(&b_walk).expect("a sane grid");
        let mut s_rescued = Vec::new();
        let mut b_rescued = Vec::new();
        let searched = s_og.close_live_set_collecting(&s_walk, &[], &mut s_rescued);
        let shared =
            b_og.close_live_set_collecting_shared(&b_walk, &[], &mut b_rescued, &starts);
        assert_eq!(
            shared, searched,
            "seed {seed} (shape {shape}): in-mark closure"
        );
        let s_base = s_og.base_ptr() as usize;
        let b_base = b_og.base_ptr() as usize;
        let s_off: Vec<usize> = s_rescued.iter().map(|&a| a - s_base).collect();
        let b_off: Vec<usize> = b_rescued.iter().map(|&a| a - b_base).collect();
        assert_eq!(
            b_off, s_off,
            "seed {seed}: the same rescues in the same order"
        );
        // The sweep's closure over the same grid and the same set: nothing
        // left to rescue, the same escape verdict.
        let searched2 = s_og.close_live_set(&s_walk);
        let shared2 = b_og.close_live_set_shared(&b_walk, &[], &starts);
        assert_eq!(shared2, searched2, "seed {seed}: the sweep's closure");
        assert_eq!(shared2.0, 0, "seed {seed}: a closed set rescues nothing");
        for (i, (&p, &q)) in b_ptrs.iter().zip(&s_ptrs).enumerate() {
            if Some(i) == freed {
                continue;
            }
            assert_eq!(
                is_marked(p),
                is_marked(q),
                "seed {seed} (shape {shape}): object {i}"
            );
        }
    }
}

/// gci i28/cl — offsets of the marked objects of a twin (the freed one
/// skipped: its header is a free-list block).
fn i28cl_marked_offsets(og: &OldGen, ptrs: &[*mut u8], freed: Option<usize>) -> Vec<usize> {
    let base = og.base_ptr() as usize;
    ptrs.iter()
        .enumerate()
        .filter(|&(i, &p)| Some(i) != freed && is_marked(p))
        .map(|(_, &p)| p as usize - base)
        .collect()
}

/// gci i28/cl — mark the objects `pick` names (unmarked, not freed) on a
/// twin, as the finalizer round's seed and drain do, plus each one's slot-0
/// target one level down (a partial drain, so the closure has work);
/// returns every object it marked, as addresses of that twin: the pops.
fn i28cl_mark_round(
    ptrs: &[*mut u8],
    edges: &[[Option<usize>; GRAPH_SLOTS]],
    pick: &[usize],
    freed: Option<usize>,
) -> Vec<usize> {
    let mut pops = Vec::new();
    let mark = |i: usize, pops: &mut Vec<usize>| {
        if Some(i) == freed || is_marked(ptrs[i]) {
            return;
        }
        // SAFETY: `ptrs[i]` is a live graph object's header.
        unsafe { (*(ptrs[i] as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
        pops.push(ptrs[i] as usize);
    };
    for &i in pick {
        mark(i, &mut pops);
        if let Some(t) = edges[i][0] {
            mark(t, &mut pops);
        }
    }
    pops
}

/// gci i28/cl (`CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL`) — the equality
/// twin: after a closure ran to a fixed point and a "finalizer round"
/// marked more objects, `close_live_set_from` over exactly the objects
/// marked since gives the full closure's rescue count, rescue SET and
/// marks, and the full closure's escape verdict once ORed with the earlier
/// closure's -- twice in a row (the second round's seeds are its new marks
/// plus the first call's rescues, as the mark loop's pops are), with and
/// without the shared start set, on the g2/e graph shapes (random sparse /
/// dense, backward chains, a referenced freed block).
#[test]
fn gci_i28cl_incremental_post_close_matches_the_full_closure() {
    const N: usize = 96;
    for seed in 1..=64u64 {
        let mut rng = Xorshift(seed.wrapping_mul(0xA076_1D64_78BD_642F) | 1);
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        let mut marked = vec![false; N];
        let shape = seed % 4;
        for (i, (row, live)) in edges.iter_mut().zip(marked.iter_mut()).enumerate() {
            for (s, slot) in row.iter_mut().enumerate() {
                *slot = match shape {
                    0 => (rng.below(3) == 0).then(|| rng.below(N)),
                    1 => (rng.below(2) == 0).then(|| rng.below(N)),
                    2 => (s == 0 && i > 0).then(|| i - 1),
                    _ => {
                        if s == 0 && i > 0 {
                            Some(i - 1)
                        } else if rng.below(4) == 0 {
                            Some(rng.below(N))
                        } else {
                            None
                        }
                    }
                };
            }
            *live = rng.below(10) == 0;            }
        let freed = if seed % 2 == 0 {
            (0..N).rev().find(|&i| !marked[i])
        } else {
            None
        };
        let build = || {
            let (mut og, ptrs) = graph_old_gen(&edges, &marked);
            if let Some(f) = freed {
                // SAFETY: `(ptrs[f], GRAPH_OBJ)` is exactly what `alloc`
                // handed out.
                unsafe { og.free(ptrs[f], GRAPH_OBJ) };
            }
            (og, ptrs)
        };
        let (f_og, f_ptrs) = build();
        let (i_og, i_ptrs) = build();
        let f_walk = f_og.walk_objects();
        let i_walk = i_og.walk_objects();
        let starts =
            (seed % 3 == 0).then(|| WalkStartBits::build(&i_walk).expect("a sane grid"));
        // The pre-close, run to its fixed point on both twins.
        let pre_f = f_og.close_live_set(&f_walk);
        let pre_i = i_og.close_live_set(&i_walk);
        assert_eq!(pre_f, pre_i, "seed {seed}: the twins agree before the round");
        assert_eq!(f_og.close_live_set(&f_walk).0, 0, "seed {seed}: a fixed point");
        assert_eq!(i_og.close_live_set(&i_walk).0, 0, "seed {seed}: a fixed point");
        let mut acc_escaped = pre_i.1;
        let mut carried: Vec<usize> = Vec::new();
        for round in 0..2 {
            let pick: Vec<usize> = (0..1 + rng.below(4)).map(|_| rng.below(N)).collect();
            let f_pops = i28cl_mark_round(&f_ptrs, &edges, &pick, freed);
            let mut i_seeds = i28cl_mark_round(&i_ptrs, &edges, &pick, freed);
            assert_eq!(f_pops.len(), i_seeds.len(), "seed {seed} round {round}: same pops");
            i_seeds.extend(carried.iter().copied());
            let mut f_rescued = Vec::new();
            let mut i_rescued = Vec::new();
            let full = f_og.close_live_set_collecting(&f_walk, &[], &mut f_rescued);
            let inc = i_og.close_live_set_from(
                &i_walk,
                &[],
                &i_seeds,
                &mut i_rescued,
                starts.as_ref(),
            );
            acc_escaped |= inc.1;
            assert_eq!(
                inc.0, full.0,
                "seed {seed} (shape {shape}) round {round}: rescue count"
            );
            assert_eq!(
                acc_escaped, full.1,
                "seed {seed} (shape {shape}) round {round}: escape verdict, ORed with the pre-close's"
            );
            let f_base = f_og.base_ptr() as usize;
            let i_base = i_og.base_ptr() as usize;
            let mut f_off: Vec<usize> = f_rescued.iter().map(|&a| a - f_base).collect();
            let mut i_off: Vec<usize> = i_rescued.iter().map(|&a| a - i_base).collect();
            f_off.sort_unstable();
            i_off.sort_unstable();
            assert_eq!(i_off, f_off, "seed {seed} round {round}: the same rescue set");
            assert_eq!(
                i28cl_marked_offsets(&i_og, &i_ptrs, freed),
                i28cl_marked_offsets(&f_og, &f_ptrs, freed),
                "seed {seed} (shape {shape}) round {round}: the same marks"
            );
            carried = i_rescued;
        }
    }
}

/// gci i28/cl — the seed screen: an unmarked seed is not visited (the full
/// pass skips it too), a non-base address and a duplicate change nothing,
/// and an empty seed list rescues nothing.
#[test]
fn gci_i28cl_unmarked_interior_and_repeated_seeds_are_screened() {
    // 0 -> 1 -> 2, 3 -> 4; only 0 is marked, then closed.
    let mut edges = vec![[None; GRAPH_SLOTS]; 5];
    edges[0][0] = Some(1);
    edges[1][0] = Some(2);
    edges[3][0] = Some(4);
    let mut marked = vec![false; 5];
    marked[0] = true;
    let (og, ptrs) = graph_old_gen(&edges, &marked);
    let walk = og.walk_objects();
    assert_eq!(og.close_live_set(&walk), (2, false));
    let mut out = Vec::new();
    assert_eq!(og.close_live_set_from(&walk, &[], &[], &mut out, None), (0, false));
    // 3 is unmarked: not visited, so 4 stays unmarked (garbage, as the
    // full closure leaves it).
    let seeds = [
        ptrs[3] as usize,
        ptrs[0] as usize + 8,
        ptrs[0] as usize,
        ptrs[0] as usize,
    ];
    assert_eq!(og.close_live_set_from(&walk, &[], &seeds, &mut out, None), (0, false));
    assert!(out.is_empty());
    assert!(!is_marked(ptrs[3]) && !is_marked(ptrs[4]));
    assert_eq!(og.close_live_set(&walk), (0, false), "the full closure agrees");
    // A newly marked 3 is a seed: its child is rescued, once.
    // SAFETY: `ptrs[3]` holds a graph header.
    unsafe { (*(ptrs[3] as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
    let seeds = [ptrs[3] as usize, ptrs[3] as usize];
    assert_eq!(og.close_live_set_from(&walk, &[], &seeds, &mut out, None), (1, false));
    assert_eq!(out, vec![ptrs[4] as usize]);
}

/// gci i28/cl — the soundness control: the precondition is load-bearing.
/// A marked object that is neither closed by an earlier fixed point nor
/// in `seeds` is invisible to the incremental closure (its child stays
/// unmarked) while the full closure rescues the child -- which is why the
/// mark loop records EVERY pop of the round, and why the switch requires
/// the idle skip (`OldGenFinalizerPass::seed` otherwise runs a closure that
/// marks without a push).
#[test]
fn gci_i28cl_an_unrecorded_mark_is_missed_so_every_pop_must_be_a_seed() {
    let mut edges = vec![[None; GRAPH_SLOTS]; 3];
    edges[1][0] = Some(2);
    let mut marked = vec![false; 3];
    marked[0] = true;
    marked[1] = true; // marked "since", but not recorded
    let (og, ptrs) = graph_old_gen(&edges, &marked);
    let walk = og.walk_objects();
    let mut out = Vec::new();
    let seeds = [ptrs[0] as usize];
    assert_eq!(
        og.close_live_set_from(&walk, &[], &seeds, &mut out, None),
        (0, false)
    );
    assert!(!is_marked(ptrs[2]), "the unrecorded mark's child is missed");
    assert_eq!(og.close_live_set_collecting(&walk, &[], &mut out), (1, false));
    assert!(is_marked(ptrs[2]), "the full closure rescues it");
}

/// gen r4/oldgen — a merge that has already run is not re-run until
/// something is freed, and skipping it never costs an allocation: a free
/// re-arms it, and the allocation-failure retry then merges as before.
#[test]
fn coalesce_is_skipped_until_something_is_freed_and_still_merges_after() {
    let mut og = OldGen::new(1024);
    let a = og.alloc(256, 8).unwrap();
    let b = og.alloc(256, 8).unwrap();
    let c = og.alloc(256, 8).unwrap();
    let _d = og.alloc(256, 8).unwrap();
    assert_eq!(og.free_block_count(), 0, "four 256-byte blocks fill 1 KiB");
    // SAFETY: `a` and `c` are blocks `alloc` handed out, freed whole.
    unsafe {
        og.free(a, 256);
        og.free(c, 256);
    }
    assert!(!og.free_list_maximal, "a free must re-arm the merge");
    assert_eq!(
        og.coalesce_free_blocks(),
        0,
        "a and c are separated by live b"
    );
    assert!(og.free_list_maximal);

    // Nothing can serve 512 bytes. The failure's coalesce retry is now
    // skipped rather than re-sorting the list to learn the same thing.
    assert!(og.alloc(512, 8).is_none());
    assert!(
        og.free_list_maximal,
        "an allocation cannot create adjacency"
    );
    assert_eq!(og.free_block_count(), 2);

    // SAFETY: as above.
    unsafe { og.free(b, 256) };
    assert!(!og.free_list_maximal);
    let big = og
        .alloc(700, 8)
        .expect("a, b and c are contiguous once b is free: the retry must merge them");
    assert_eq!(big, a);
}

/// gen r4/oldgen — a promotion BUFFER must not become the scale the
/// fragmentation predicate judges holes by; a real request still does.
#[test]
fn a_promotion_buffer_carve_does_not_set_the_fragmentation_scale() {
    let mut og = OldGen::new(1 << 20);
    assert!(og.alloc_unzeroed_buffer(64 * 1024, 8).is_some());
    assert_eq!(og.recent_max_request(), 0, "a buffer is not a request");
    assert!(og.alloc_unzeroed(4096, 8).is_some());
    assert_eq!(og.recent_max_request(), 4096, "an object-sized block is");
    assert!(og.alloc_unzeroed_buffer(128 * 1024, 8).is_some());
    assert_eq!(og.recent_max_request(), 4096);
}

/// Three one-slot objects `a`, `b`, `c`, with `a` and `c` marked and `b`
/// dead between them.
fn marked_dead_marked() -> (OldGen, *mut u8, *mut u8, *mut u8) {
    let mut og = OldGen::new(4096);
    let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let b = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let c = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    // SAFETY: all three are zeroed live blocks sized for one slot.
    unsafe {
        for p in [a, b, c] {
            (*(p as *mut ObjectHeader)).set_num_slots(1);
        }
        (*(a as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
        (*(c as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
    }
    (og, a, b, c)
}

/// gen r4/oldgen — pause-path redundant walks item 3: compacting over the
/// grid the caller already derived is the same compaction as walking again.
#[test]
fn compacting_over_a_fresh_grid_matches_walking_again() {
    let (mut og1, _a1, _b1, c1) = marked_dead_marked();
    let map1 = og1.compact();
    let (mut og2, _a2, _b2, c2) = marked_dead_marked();
    let walk = og2.walk_objects();
    let seq = og2.free_list_seq();
    let map2 = og2.compact_with_walked_objects(walk, seq, &HashMap::new());

    let rel = |base: *const u8, p: usize| p - base as usize;
    let c1_to = *map1.get(&(c1 as usize)).expect("c slides down over b");
    let c2_to = *map2.get(&(c2 as usize)).expect("c slides down over b");
    assert_eq!(rel(og1.base_ptr(), c1_to), rel(og2.base_ptr(), c2_to));
    assert_eq!(og1.used(), og2.used());
    assert_eq!(og1.used(), 2 * (HEADER_SIZE + SLOT_SIZE));
}

/// gen r4/oldgen — a grid taken before the free list changed is not
/// trusted. Compacting over this one would still list `b`, whose block is
/// back on the free list, so `walked_bytes != used_bytes` would abandon
/// the compaction and relocate nothing; the re-walk sees the real layout
/// and slides `c` into `b`'s hole.
#[test]
fn compacting_over_a_stale_grid_rewalks_instead() {
    let (mut og, _a, b, c) = marked_dead_marked();
    let stale = og.walk_objects();
    let seq = og.free_list_seq();
    // SAFETY: `(b, HEADER_SIZE + SLOT_SIZE)` is exactly what `alloc`
    // handed out.
    unsafe { og.free(b, HEADER_SIZE + SLOT_SIZE) };
    assert_ne!(og.free_list_seq(), seq, "a free must move the stamp");

    let map = og.compact_with_walked_objects(stale, seq, &HashMap::new());
    assert_eq!(
        map.get(&(c as usize)),
        Some(&(b as usize)),
        "the compaction must run against the current layout",
    );
    assert_eq!(og.used(), 2 * (HEADER_SIZE + SLOT_SIZE));
}

/// gen r4/oldgen — the in-place sweep now returns each run of ADJACENT
/// dead objects as one block instead of one block per object. That is
/// only sound if the result is indistinguishable from the old per-object
/// frees once the sweep's own trailing `coalesce_free_blocks` has run:
/// same occupancy, same free list, same object walk.
#[test]
fn freeing_a_contiguous_dead_run_as_one_block_equals_freeing_each_then_coalescing() {
    let size = HEADER_SIZE + 2 * SLOT_SIZE;
    let dead = [1usize, 2, 3, 5, 7, 8];
    let runs = [(1usize, 3usize), (5, 1), (7, 2)];

    let (mut each, offs_each) = old_gen_with_objects(10, 2);
    let base_each = each.base_ptr() as usize;
    for &i in &dead {
        // SAFETY: each is exactly one `alloc`ed block.
        unsafe { each.free((base_each + offs_each[i]) as *mut u8, size) };
    }
    each.coalesce_free_blocks();

    let (mut batched, offs_batched) = old_gen_with_objects(10, 2);
    let base_batched = batched.base_ptr() as usize;
    for &(first, len) in &runs {
        // SAFETY: objects `first..first + len` are adjacent `alloc`ed
        // blocks (`old_gen_with_objects` lays them out back to back), so
        // their union is exactly the span they were handed.
        unsafe { batched.free((base_batched + offs_batched[first]) as *mut u8, len * size) };
    }
    batched.coalesce_free_blocks();

    assert_eq!(offs_each, offs_batched, "identical layouts to start from");
    assert_eq!(each.used(), batched.used());
    let free_list = |og: &OldGen| {
        og.with_sorted_free_blocks(|s| s.iter().map(|b| (b.offset, b.size)).collect::<Vec<_>>())
    };
    assert_eq!(free_list(&each), free_list(&batched));
    let walked = |og: &OldGen| {
        let base = og.base_ptr() as usize;
        og.walk_objects()
            .into_iter()
            .map(|(p, s)| (p as usize - base, s))
            .collect::<Vec<_>>()
    };
    assert_eq!(walked(&each), walked(&batched));
}

// -----------------------------------------------------------------
// gen r4w2/oldgen2 (2026-09-23): the old-gen trigger
// -----------------------------------------------------------------

/// With the hysteresis OFF — the default — the verdict RUNS exactly when
/// the pre-2026-09-23 expression `(cap > 0 && used >= cap * 75 / 100) ||
/// requested` did, over every combination of the inputs the hysteresis
/// could have looked at. This is the "default behaviour is byte-for-byte
/// unchanged" claim, checked rather than argued.
#[test]
fn the_trigger_with_hysteresis_off_is_the_bare_occupancy_test() {
    let caps = [0usize, 1, 100, 1600, 4096, 1 << 20];
    for &cap in &caps {
        let mut useds: Vec<usize> = vec![0, 1, cap / 2, cap.saturating_sub(1), cap];
        let floor = cap * 75 / 100;
        useds.extend([floor.saturating_sub(1), floor, floor + 1, floor + cap / 16]);
        for &used in &useds {
            let lasts = [None, Some(0), Some(floor), Some(used), Some(cap)];
            for &last in &lasts {
                for failures in [0u64, 1, 7] {
                    for requested in [false, true] {
                        let old = (cap > 0 && used >= cap * 75 / 100) || requested;
                        let v = OldGen::major_trigger_verdict(
                            cap, used, last, failures, requested, false,
                        );
                        assert_eq!(
                            v.runs(),
                            old,
                            "cap={cap} used={used} last={last:?} failures={failures} \
                             requested={requested}: {v:?}",
                        );
                        assert_ne!(v, MajorTrigger::Suppressed, "OFF never suppresses");
                    }
                }
            }
        }
    }
}

/// The hysteresis, clause by clause, at a capacity where the numbers are
/// easy to read: 1600 bytes, floor 1200, `cap / 16` = 100.
#[test]
fn the_hysteresis_skips_a_repeat_collection_until_growth_or_a_failure() {
    let cap = 1600;
    let v = |used, last, failures, requested| {
        OldGen::major_trigger_verdict(cap, used, last, failures, requested, true)
    };
    // Below the floor: never, whatever else is true.
    assert_eq!(v(1199, Some(1190), 3, false), MajorTrigger::NotDue);
    // A request always runs, even below the floor.
    assert_eq!(v(0, Some(1500), 0, true), MajorTrigger::Requested);
    // Never collected before: the floor alone decides.
    assert_eq!(v(1200, None, 0, false), MajorTrigger::Occupancy);
    // Last collection left 1250 used, i.e. 350 free: the next one is due
    // after max(100, 350 / 2) = 175 more bytes, at 1425.
    assert_eq!(v(1424, Some(1250), 0, false), MajorTrigger::Suppressed);
    assert_eq!(v(1425, Some(1250), 0, false), MajorTrigger::Occupancy);
    // ...unless an allocation has failed since: then at once.
    assert_eq!(
        v(1300, Some(1250), 1, false),
        MajorTrigger::AllocationFailure
    );
    // Last collection left the generation nearly full (1500 used, 100
    // free): `cap / 16` is the binding term (max(100, 50)), so a
    // generation that is legitimately tight is not collected on every
    // cycle either — only once it is full, or once something fails.
    assert_eq!(v(1599, Some(1500), 0, false), MajorTrigger::Suppressed);
    assert_eq!(v(1600, Some(1500), 0, false), MajorTrigger::Occupancy);
    assert_eq!(
        v(1510, Some(1500), 1, false),
        MajorTrigger::AllocationFailure
    );
    // Used went DOWN since (a concurrent sweep freed): no growth at all.
    assert_eq!(v(1300, Some(1400), 0, false), MajorTrigger::Suppressed);
    // And the same states with the hysteresis off report, but run.
    let off = OldGen::major_trigger_verdict(cap, 1424, Some(1250), 0, false, false);
    assert_eq!(off, MajorTrigger::WouldSuppress);
    assert!(off.runs());
}

/// The live form: a collection's end is what arms the hysteresis, a
/// refused allocation is what disarms it, and a refused optional BUFFER
/// carve is not a refused allocation.
#[test]
fn a_collection_arms_the_hysteresis_and_a_refused_allocation_disarms_it() {
    let (mut og, _) = old_gen_with_objects(1, 1);
    let cap = og.capacity();
    // Fill to just past the floor with plain blocks.
    while og.used() < cap * 75 / 100 {
        og.alloc(256, 8).expect("room below the floor");
    }
    assert_eq!(og.major_trigger(false, true), MajorTrigger::Occupancy);
    // A collection that reclaimed nothing.
    let used = og.used();
    og.note_collection_end(used);
    let s = og.trigger_stats();
    assert_eq!((s.collections, s.low_yield, s.occupancy), (1, 1, 1));
    assert_eq!(og.major_trigger(false, true), MajorTrigger::Suppressed);
    assert_eq!(og.major_trigger(false, false), MajorTrigger::WouldSuppress);

    // A promotion BUFFER that does not fit is optional: not a failure.
    assert!(og.alloc_unzeroed_buffer(cap, 8).is_none());
    assert_eq!(og.trigger_stats().alloc_failures, 0);
    assert_eq!(og.major_trigger(false, true), MajorTrigger::Suppressed);

    // An object that does not fit is.
    assert!(og.alloc(cap, 8).is_none());
    assert_eq!(og.trigger_stats().alloc_failures, 1);
    assert_eq!(
        og.major_trigger(false, true),
        MajorTrigger::AllocationFailure
    );

    // The next collection's end re-arms it.
    og.note_collection_end(og.used());
    assert_eq!(og.major_trigger(false, true), MajorTrigger::Suppressed);
    let s = og.trigger_stats();
    assert_eq!(
        (
            s.suppressed,
            s.would_suppress,
            s.allocation_failure,
            s.collections
        ),
        (3, 1, 1, 2)
    );
}

// -----------------------------------------------------------------
// gen r4w2/oldgen2 (2026-09-23): the reserve/commit backing store
// -----------------------------------------------------------------

/// A generation big enough to reserve (at least one granule) and a way to
/// tell which store the process gave it: `CRATONVM_GC_RESERVE=0`, or a
/// refused reservation, leaves it on the wholly committed arm, where every
/// assertion below has a different (and simpler) right answer.
fn big_old_gen(granules: usize) -> (OldGen, bool) {
    let og = OldGen::new(granules * GRANULE);
    let reserved = og.commit_bits().is_some();
    (og, reserved)
}

#[test]
fn a_reserved_old_gen_commits_only_what_allocation_reaches() {
    let (mut og, reserved) = big_old_gen(8);
    let base = og.base_ptr() as usize;
    let cap = og.capacity();
    assert_eq!(cap, 8 * GRANULE, "reserving must not round the capacity");
    if !reserved {
        // Wholly committed: the historical behaviour, byte for byte.
        assert_eq!(og.committed_bytes(), cap);
        assert!(og.contains((base + cap - 8) as *const u8));
        return;
    }
    assert_eq!(og.committed_bytes(), 0, "construction commits nothing");
    assert!(
        !og.contains(base as *const u8),
        "nothing is backed yet, so nothing is readable old-gen storage",
    );

    let p = og.alloc(HEADER_SIZE + 64, 8).expect("first allocation");
    assert_eq!(p as usize, base);
    assert_eq!(og.committed_bytes(), GRANULE, "one granule, on demand");
    assert!(og.contains(p));
    // SAFETY: `alloc` handed out `HEADER_SIZE + 64` committed bytes at `p`.
    let bytes = unsafe { std::slice::from_raw_parts(p, HEADER_SIZE + 64) };
    assert!(bytes.iter().all(|&b| b == 0), "alloc still zeroes");
    assert!(
        !og.contains((base + 3 * GRANULE) as *const u8),
        "reserved-but-uncommitted space is not readable old-gen storage",
    );

    // A request that crosses granules commits the whole run it needs.
    let q = og
        .alloc(2 * GRANULE, 8)
        .expect("a multi-granule allocation");
    assert!(og.committed_bytes() >= 3 * GRANULE);
    // SAFETY: the allocation spans `2 * GRANULE` committed bytes at `q`;
    // touching its last byte is what would fault if the commit were short.
    unsafe { std::ptr::write_volatile(q.add(2 * GRANULE - 1), 0xAB) };
    assert!(og.contains(unsafe { q.add(2 * GRANULE - 1) }));

    // `-Xms`: an explicit prefix commit raises the readable mark too.
    let committed = og.commit_initial_prefix(6 * GRANULE);
    assert_eq!(committed, 6 * GRANULE);
    assert!(og.committed_bytes() >= 6 * GRANULE);
    assert!(og.contains((base + 6 * GRANULE - 8) as *const u8));
    assert!(!og.contains((base + 6 * GRANULE) as *const u8));
    assert_eq!(og.trigger_stats().commit_refusals, 0);
}

/// Compaction's tail zero becomes a give-back of whole granules where the
/// store can, and the range still reads as zero afterwards — the H2-CID0
/// contract the `memset` existed for.
#[test]
fn compaction_gives_whole_tail_granules_back_and_the_tail_still_reads_zero() {
    let (mut og, reserved) = big_old_gen(8);
    let base = og.base_ptr() as usize;
    // Six 1 MiB-bodied objects: about three granules of storage.
    let slots = (1024 * 1024 / SLOT_SIZE) as u32;
    let mut ptrs = Vec::new();
    for _ in 0..6 {
        let p = og
            .alloc(HEADER_SIZE + slots as usize * SLOT_SIZE, 8)
            .expect("room");
        // SAFETY: `p` is a fresh, correctly sized allocation.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(
                    cratonvm_types::ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    slots,
                ),
            );
        }
        ptrs.push(p);
    }
    let top = og.high_water();
    // Only the first survives.
    // SAFETY: a header was written there above.
    unsafe { (*(ptrs[0] as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
    let _ = og.compact();
    let end = og.used();
    assert!(end < GRANULE, "one object survives");
    // SAFETY: `[end, top)` is inside the store and was committed by the
    // allocations above; a reset granule stays mapped and readable.
    let tail = unsafe { std::slice::from_raw_parts((base + end) as *const u8, top - end) };
    assert!(
        tail.iter().all(|&b| b == 0),
        "the compacted tail must read as zero"
    );
    let whole = (top / GRANULE) * GRANULE - end.div_ceil(GRANULE) * GRANULE;
    let resets_to_zero = cfg!(any(target_os = "linux", target_os = "windows"));
    if reserved && resets_to_zero {
        assert_eq!(
            og.bytes_given_back(),
            whole as u64,
            "every whole granule of the dropped range goes back to the OS",
        );
    } else {
        assert_eq!(
            og.bytes_given_back(),
            0,
            "the memset arm gives nothing back"
        );
    }
    // The storage is still usable: the next allocation lands on the
    // compacted end and reads as zero.
    let p = og
        .alloc(HEADER_SIZE + 64, 8)
        .expect("reuse after compaction");
    assert_eq!(p as usize, base + end);
    assert!(og.contains(p));
}

// -----------------------------------------------------------------
// gen r4w3/oldgen3 (2026-09-23)
// -----------------------------------------------------------------

/// `n` isolated `hole`-byte holes — each separated from the next by a live
/// 16-byte keeper so nothing can coalesce them — plus the big trailing
/// block.
fn og_with_isolated_holes(n: usize, hole: usize) -> OldGen {
    let mut og = OldGen::new(1 << 20);
    let mut holes = Vec::new();
    for _ in 0..n {
        holes.push(og.alloc(hole, 8).expect("1 MiB covers the holes"));
        og.alloc(HEADER_SIZE, 8).expect("and the keepers");
    }
    for p in holes {
        // SAFETY: each came from `alloc(hole, 8)` on this old gen and is
        // freed exactly once with its own size.
        unsafe { og.free(p, hole) };
    }
    og
}

/// `gengc-r4w2-oldgen2-best-fit-scan-is-unbounded-over-non-fitting-blocks`:
/// a request a little larger than a class full of holes used to walk every
/// hole on every call. The bucket's size bound skips the class in O(1)
/// whenever it proves nothing there fits, tightens itself after a scan
/// that found nothing, and never hides a block that DOES fit.
///
/// gen r4w4/oldgen4: 48/56 no longer share a class (below 64 bytes every
/// 8-aligned size has its own), so the pair is 1040/1100 in `[1024,
/// 1280)`; and `N` stays under `NONFIT_PROBE_LIMIT` so the full walks this
/// test counts are not deferred (the deferral has its own test).
#[test]
fn a_class_that_cannot_fit_the_request_is_skipped_without_a_scan() {
    const N: usize = 40;
    const HOLE: usize = 1040;
    const REQ: usize = 1100;
    assert!(N < NONFIT_PROBE_LIMIT);
    let mut og = og_with_isolated_holes(N, HOLE);
    assert_eq!(
        bucket_for(HOLE),
        bucket_for(REQ),
        "one class holds both sizes"
    );
    assert_eq!(
        min_satisfying_bucket(REQ + 7),
        bucket_for(HOLE),
        "and the request starts there"
    );

    // Every hole is 1040 bytes, so the bound is 1040 < 1100: skip at once.
    let s0 = og.trigger_stats();
    let r1 = og.alloc(REQ, 8).expect("the trailing block serves it");
    let s1 = og.trigger_stats();
    assert_eq!(s1.nonfit_probes, s0.nonfit_probes, "no hole was walked");
    assert_eq!(s1.bucket_skips, s0.bucket_skips + 1);

    // A FITTING block raises the bound, and the next request finds it —
    // behind every hole, exactly where the unbounded scan found it.
    // SAFETY: `r1` came from `alloc(REQ, 8)` above.
    unsafe { og.free(r1, REQ) };
    let r2 = og.alloc(REQ, 8).expect("the freed block");
    assert_eq!(
        r2, r1,
        "placement is unchanged: the fitting block is reused"
    );
    let s2 = og.trigger_stats();
    assert_eq!(s2.nonfit_probes - s1.nonfit_probes, N as u64);

    // The bound is now stale-high (1100, with no such block left): one
    // full scan finds nothing and TIGHTENS it...
    let _r3 = og.alloc(REQ, 8).expect("the trailing block again");
    let s3 = og.trigger_stats();
    assert_eq!(s3.nonfit_probes - s2.nonfit_probes, N as u64);
    assert_eq!(
        og.buckets[bucket_for(HOLE)].size_bound,
        HOLE,
        "tightened to the real maximum"
    );
    // ...so the next one skips again.
    let _r4 = og.alloc(REQ, 8).expect("and again");
    let s4 = og.trigger_stats();
    assert_eq!(s4.nonfit_probes, s3.nonfit_probes);
    assert_eq!(s4.bucket_skips, s3.bucket_skips + 1);

    // A request the holes DO fit is served from them, as before.
    let before = og.free_block_count();
    let small = og.alloc(1000, 8).expect("a hole fits 1000");
    assert!(
        og.free_block_count() <= before,
        "served from a hole, not the tail"
    );
    assert!(
        (small as usize) < (r1 as usize),
        "the holes all lie below the first tail carve"
    );
}

/// The bound is an UPPER bound on every block in its bucket after every
/// kind of mutation — the property that makes the skip unable to hide a
/// fit (and so unable to turn into a spurious OOM).
#[test]
fn bucket_size_bounds_stay_upper_bounds_through_every_mutation() {
    let check = |og: &OldGen, what: &str| {
        for (k, b) in og.buckets.iter().enumerate() {
            let max = b.iter().map(|f| f.size).max().unwrap_or(0);
            assert!(
                b.size_bound >= max,
                "bucket {k} bound {} < block {max} after {what}",
                b.size_bound
            );
        }
    };
    let mut og = OldGen::new(256 * 1024);
    let mut live: Vec<(*mut u8, usize)> = Vec::new();
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..4000 {
        let r = next();
        if r % 3 != 0 || live.is_empty() {
            let size = 16 + (r as usize >> 8) % 600;
            if let Some(p) = og.alloc(size, 8) {
                live.push((p, size));
            }
            check(&og, "alloc");
        } else {
            let i = (r as usize >> 16) % live.len();
            let (p, size) = live.swap_remove(i);
            // SAFETY: `(p, size)` is exactly what `alloc` returned.
            unsafe { og.free(p, size) };
            check(&og, "free");
        }
        if round % 500 == 499 {
            og.coalesce_free_blocks();
            check(&og, "coalesce");
        }
    }
    // And a compaction's rebuild, on a generation with real headers (the
    // blocks above are header-less zeros, which a walk would stride as
    // 16-byte objects): nothing is marked, so it drops all of them.
    let (mut og2, _) = old_gen_with_objects(8, 2);
    check(&og2, "setup");
    let _ = og2.compact();
    check(&og2, "compact");
    assert!(og2.alloc(1024, 8).is_some());
}

/// Lay out `n` two-slot objects, free the ones `dead` names (without
/// coalescing), and return the old gen and every object offset.
fn og_with_holes(n: usize, dead: &[usize]) -> (OldGen, Vec<usize>) {
    let (mut og, offsets) = old_gen_with_objects(n, 2);
    let base = og.base_ptr() as usize;
    for &i in dead {
        // SAFETY: each is exactly one `alloc`ed block.
        unsafe { og.free((base + offsets[i]) as *mut u8, HEADER_SIZE + 2 * SLOT_SIZE) };
    }
    (og, offsets)
}

fn rel_walk(og: &OldGen) -> Vec<(usize, usize)> {
    let base = og.base_ptr() as usize;
    og.walk_objects()
        .into_iter()
        .map(|(p, s)| (p as usize - base, s))
        .collect()
}

/// `walk_objects_from` in slices of any size, and `for_each_object`, visit
/// exactly what `walk_objects` returns, in the same order — including
/// across region boundaries (the freed objects split the generation into
/// several allocated regions).
#[test]
fn gcj_retire_sweep_merge_keeps_walk_and_prefix_after_mutator_allocation() {
    let size = HEADER_SIZE + 2 * SLOT_SIZE;
    let (mut og, offsets) = og_with_holes(32, &[2, 7, 11, 18, 29]);
    let base = og.base_ptr() as usize;
    let runs = [(base + offsets[3], size), (base + offsets[19], 2 * size)];
    unsafe { og.free_runs_ascending(&runs) };
    assert!(!og.sorted_free_dirty.get());
    let merged = rel_walk(&og);
    og.invalidate_sorted_free();
    assert_eq!(rel_walk(&og), merged, "merge equals a bucket rebuild");
    // A normal mutation must invalidate both sorted blocks and prefix ends.
    unsafe { og.free((base + offsets[24]) as *mut u8, size) };
    let expected = rel_walk(&og);
    for from in offsets.iter().copied().filter(|o| expected.iter().any(|v| v.0 == *o)) {
        let mut got = Vec::new();
        og.walk_objects_from(from, usize::MAX, |p, n| got.push((p as usize - base, n)));
        assert_eq!(got, expected.iter().copied().filter(|v| v.0 >= from).collect::<Vec<_>>());
    }
}

#[test]
fn gcj_retire_resumed_walk_preserves_nested_free_block_cover() {
    let (mut og, offsets) = og_with_holes(16, &[2, 3, 4, 5]);
    let size = HEADER_SIZE + 2 * SLOT_SIZE;
    // Deliberate corrupt nested free block: the walk must still never
    // rewind into the covering free span when it resumes beyond the nest.
    og.coalesce_free_blocks();
    og.buckets[bucket_for(size)].push(FreeBlock { offset: offsets[3], size });
    og.invalidate_sorted_free();
    let from = offsets[5];
    let mut got = Vec::new();
    og.walk_objects_from(from, usize::MAX, |p, n| got.push((p as usize - og.base_ptr() as usize, n)));
    assert_eq!(got, rel_walk(&og).into_iter().filter(|v| v.0 >= from).collect::<Vec<_>>());
}

#[test]
fn a_budgeted_walk_in_slices_visits_exactly_what_walk_objects_returns() {
    let (og, _offsets) = og_with_holes(12, &[0, 3, 4, 8, 11]);
    let base = og.base_ptr() as usize;
    let expected = rel_walk(&og);
    assert_eq!(expected.len(), 7);

    let mut each = Vec::new();
    og.for_each_object(|p, s| each.push((p as usize - base, s)));
    assert_eq!(
        each, expected,
        "for_each_object is walk_objects without the Vec"
    );

    for budget in 1..=8 {
        let mut got = Vec::new();
        let mut from = 0usize;
        let mut calls = 0;
        loop {
            calls += 1;
            assert!(calls < 100, "a budgeted walk must make progress");
            let next =
                og.walk_objects_from(from, budget, |p, s| got.push((p as usize - base, s)));
            match next {
                Some(n) => {
                    assert!(
                        expected.iter().any(|&(o, _)| o == n),
                        "budget {budget}: resume offset {n:#x} is not an object start",
                    );
                    from = n;
                }
                None => break,
            }
        }
        assert_eq!(got, expected, "budget {budget}");
    }
}

/// The two things that may happen between two slices of the concurrent
/// sweep, and what `walk_objects_from` must do about each: an allocation
/// into a hole BELOW the resume offset (this sweep's own frees, coalesced)
/// is not revisited, and the budget running out exactly at a region's end
/// resumes at the NEXT region's first object, not at the free block — so
/// an allocation into that free block cannot straddle the resume point.
#[test]
fn a_resumed_walk_is_not_confused_by_allocations_between_slices() {
    let size = HEADER_SIZE + 2 * SLOT_SIZE;
    // Objects 0..4, a hole of two (4, 5), objects 6..10.
    let (mut og, offsets) = og_with_holes(10, &[4, 5]);
    let base = og.base_ptr() as usize;

    // Four objects end exactly at the hole: resume at object 6.
    let mut first = Vec::new();
    let resume = og
        .walk_objects_from(0, 4, |p, _| first.push(p as usize - base))
        .expect("more to walk");
    assert_eq!(first, offsets[0..4].to_vec());
    assert_eq!(
        resume, offsets[6],
        "the next REGION's first object, not the free block"
    );

    // "This sweep" frees objects 1..3 (below the resume offset); an
    // allocation failure coalesces them with nothing further; a new object
    // is then allocated into the hole at 4 and one into the freed run.
    // SAFETY: objects 1 and 2 are adjacent `alloc`ed blocks.
    unsafe { og.free((base + offsets[1]) as *mut u8, 2 * size) };
    og.coalesce_free_blocks();
    let hole = og
        .alloc(2 * size, 8)
        .expect("the old hole at 4..6 or the freed run");
    let other = og.alloc(size, 8).expect("room");
    for p in [hole, other] {
        // SAFETY: fresh allocations of at least one two-slot object.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(
                    cratonvm_types::ClassId::new(9),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    if p == hole { 2 * 2 + 1 } else { 2 },
                ),
            );
        }
    }
    assert!((hole as usize - base) < resume && (other as usize - base) < resume);

    let mut rest = Vec::new();
    assert_eq!(
        og.walk_objects_from(resume, usize::MAX, |p, _| rest.push(p as usize - base)),
        None
    );
    assert_eq!(
        rest,
        offsets[6..10].to_vec(),
        "exactly the objects at and after the resume point"
    );
}

/// `gengc-r4w2-oldgen2-in-place-sweep-never-returns-pages-and-old-gen-cannot-grow`
/// item 1: the candidate sample is taken whether or not the flag is on;
/// the give-back itself is opt-in, bounded to large free blocks below the
/// high-water mark, refused above the trigger floor, and rate-limited.
#[test]
fn an_in_place_give_back_returns_large_free_granules_only_when_asked() {
    let (mut og, reserved) = big_old_gen(16);
    let base = og.base_ptr() as usize;
    let mut blocks = Vec::new();
    for _ in 0..12 {
        blocks.push(og.alloc(GRANULE, 8).expect("room for twelve granules"));
    }
    assert_eq!(og.high_water(), 12 * GRANULE);
    // Free granules 2..9 as one run (7 whole granules), and granule 10 on
    // its own (1 granule: below `GIVE_BACK_MIN_GRANULES`).
    for &p in &blocks[2..9] {
        // SAFETY: each is exactly one `alloc(GRANULE, 8)` block; nothing
        // was written into it that anything reads.
        unsafe { og.free(p, GRANULE) };
    }
    // SAFETY: as above.
    unsafe { og.free(blocks[10], GRANULE) };
    og.coalesce_free_blocks();
    if reserved {
        // Make the run's pages resident and non-zero, so a give-back is
        // observable as zeros.
        // SAFETY: `[2G, 9G)` is a committed free block of this store.
        unsafe { std::ptr::write_bytes((base + 2 * GRANULE) as *mut u8, 0xA5, 7 * GRANULE) };
    }

    // Flag off: only the sample. The trailing block is above high water
    // and the single-granule hole is too small, so the run is all of it.
    assert_eq!(og.after_in_place_sweep(false), 0);
    assert_eq!(
        og.trigger_stats().give_back_candidate_bytes,
        (7 * GRANULE) as u64
    );
    assert_eq!(og.bytes_given_back(), 0);

    let given = og.after_in_place_sweep(true);
    if !reserved {
        assert_eq!(
            given, 0,
            "the wholly committed store has nothing to give back"
        );
        return;
    }
    assert_eq!(given, 7 * GRANULE);
    assert_eq!(og.bytes_given_back(), (7 * GRANULE) as u64);
    assert_eq!(og.trigger_stats().in_place_give_backs, 1);
    if cfg!(any(
        target_os = "linux",
        target_os = "windows",
        target_os = "macos"
    )) {
        // SAFETY: a reset granule stays mapped and readable.
        let run = unsafe {
            std::slice::from_raw_parts((base + 2 * GRANULE) as *const u8, 7 * GRANULE)
        };
        assert!(
            run.iter().all(|&b| b == 0),
            "a given-back granule reads back as zero"
        );
    }
    assert!(
        og.contains((base + 5 * GRANULE) as *const u8),
        "and is still readable old-gen storage"
    );

    // Rate limit: not again until `GIVE_BACK_EVERY` collections have ended.
    assert_eq!(og.after_in_place_sweep(true), 0);
    for _ in 0..GIVE_BACK_EVERY {
        og.note_collection_end(og.used());
    }
    assert_eq!(og.after_in_place_sweep(true), 7 * GRANULE);

    // At or above the trigger floor nothing is given back — the
    // generation is about to reuse those pages — even with a candidate on
    // hand and the rate limit satisfied. Fill exactly to the floor
    // (24 MiB of 32): the 8 MiB tail, the 2 MiB hole, then 6 MiB off the
    // front of the run, which leaves [5G, 9G) — still four whole granules.
    for _ in 0..GIVE_BACK_EVERY {
        og.note_collection_end(og.used());
    }
    let tail = og.alloc(4 * GRANULE, 8).expect("the exact-fit tail");
    assert_eq!(tail as usize, base + 12 * GRANULE);
    let hole = og.alloc(GRANULE, 8).expect("the exact-fit hole");
    assert_eq!(hole as usize, base + 10 * GRANULE);
    let front = og.alloc(3 * GRANULE, 8).expect("the front of the run");
    assert_eq!(front as usize, base + 2 * GRANULE);
    assert_eq!(og.used(), og.capacity() * 75 / 100);
    assert_eq!(og.after_in_place_sweep(true), 0);
    assert_eq!(
        og.trigger_stats().give_back_candidate_bytes,
        (4 * GRANULE) as u64
    );
    assert_eq!(og.trigger_stats().in_place_give_backs, 2);

    // The storage stays usable after a give-back.
    assert!(og.alloc(64, 8).is_some());
}

/// The wave-3 trigger instruments: a peek decides nothing and counts
/// nothing; a collection the hysteresis WOULD have skipped reports what it
/// reclaimed; a concurrent collection is a collection (and is not charged
/// to a pending STW verdict).
#[test]
fn the_trigger_instruments_attribute_each_collection_to_its_reason() {
    let (mut og, _) = old_gen_with_objects(1, 1);
    let cap = og.capacity();
    let mut blocks = Vec::new();
    // A margin above the floor, so freeing a few blocks below does not
    // drop the generation under it.
    while og.used() < cap * 75 / 100 + 2048 {
        blocks.push(og.alloc(256, 8).expect("room below the floor"));
    }
    // Peeking is free.
    let s0 = og.trigger_stats();
    assert!(og.major_trigger_peek(false, false).runs());
    assert_eq!(og.trigger_stats(), s0, "a peek records nothing");

    // Arm the hysteresis with a collection that reclaimed nothing.
    assert_eq!(og.major_trigger(false, false), MajorTrigger::Occupancy);
    og.note_collection_end(og.used());
    assert_eq!(og.trigger_stats().would_suppress_freed_bytes, 0);

    // The next decision is one the hysteresis would skip; the collection
    // it runs frees two blocks.
    assert_eq!(og.major_trigger(false, false), MajorTrigger::WouldSuppress);
    let before = og.used();
    for p in blocks.drain(..2) {
        // SAFETY: exactly the `(p, 256)` pairs `alloc` returned.
        unsafe { og.free(p, 256) };
    }
    og.note_collection_end(before);
    let s = og.trigger_stats();
    assert_eq!(s.would_suppress_freed_bytes, 512);
    assert_eq!(s.would_suppress_low_yield, 1, "512 bytes is under cap / 16");
    assert_eq!(s.freed_bytes, 512);
    assert_eq!(s.collections, 2);
    assert!(s.largest_free_after_last > 0 && s.free_blocks_after_last > 0);

    // A concurrent collection: counted, not attributed to a stale verdict.
    assert_eq!(og.major_trigger(false, false), MajorTrigger::WouldSuppress);
    for p in blocks.drain(..1) {
        // SAFETY: as above.
        unsafe { og.free(p, 256) };
    }
    og.note_concurrent_collection_end(256);
    let s = og.trigger_stats();
    assert_eq!(s.concurrent_collections, 1);
    assert_eq!(s.collections, 3);
    assert_eq!(s.freed_bytes, 768);
    assert_eq!(
        s.would_suppress_freed_bytes, 512,
        "the concurrent cycle was not a WouldSuppress run"
    );
}

// -----------------------------------------------------------------
// gen r4w4/oldgen4 (2026-09-24)
// -----------------------------------------------------------------

/// gcj j2/og2 (O1) — HotSpot's `_shrink_factor` step, pure.
#[test]
fn gcj_j2og2_the_shrink_factor_steps_as_hotspots() {
    assert_eq!(next_shrink_factor(0), 10);
    assert_eq!(next_shrink_factor(10), 40);
    assert_eq!(next_shrink_factor(40), 100);
    assert_eq!(next_shrink_factor(100), 100);
}

/// gcj j2/og2 (O1) — the cut of a shrink-factor release: from the top of
/// the committed runs, rounded UP to a granule, never below `keep_end`.
#[test]
fn gcj_j2og2_the_shrink_factor_cut_releases_from_the_top() {
    let g = GRANULE;
    let base = 0x1000_0000usize;
    // One run [1g, 10g): 1g releases [9g, 10g); 3.6g rounds to [7g, 10g).
    let one = [(base + g, 9 * g)];
    assert_eq!(shrink_factor_cut(&one, base, g, g), 9 * g);
    assert_eq!(shrink_factor_cut(&one, base, g, 9 * g * 40 / 100), 7 * g);
    // The whole excess, and more, stops at `keep_end`.
    assert_eq!(shrink_factor_cut(&one, base, g, 9 * g), g);
    assert_eq!(shrink_factor_cut(&one, base, 2 * g, 20 * g), 2 * g);
    // Nothing to release: the top.
    assert_eq!(shrink_factor_cut(&one, base, g, 0), 10 * g);
    // Two runs with a hole: [1g, 3g) and [5g, 6g); 2g releases [5g, 6g)
    // and the top granule of the lower run.
    let two = [(base + g, 2 * g), (base + 5 * g, g)];
    assert_eq!(shrink_factor_cut(&two, base, g, 2 * g), 2 * g);
    assert_eq!(shrink_factor_cut(&[], base, g, g), usize::MAX);
}

/// gcj j2/og2 (O1) — armed, consecutive shrinking collections release 0,
/// 10, 40, then 100 % of the excess over the `MaxHeapFreeRatio` target,
/// from the top; a collection without excess resets the factor. The
/// default arm (the existing test below) releases the whole excess on the
/// second collection.
#[test]
fn gcj_j2og2_an_armed_shrink_factor_releases_in_hotspots_steps() {
    let (mut og, reserved) = big_old_gen(16);
    if !reserved {
        return;
    }
    og.set_shrink_factor_for_test(true);
    let keep = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor");
    let mut big = Vec::new();
    for _ in 0..10 {
        big.push(og.alloc(GRANULE, 8).expect("ten granules"));
    }
    for p in big {
        // SAFETY: each is exactly one `alloc(GRANULE, 8)` block.
        unsafe { og.free(p, GRANULE) };
    }
    og.coalesce_free_blocks();
    assert_eq!(og.committed_bytes(), 11 * GRANULE);
    let r0 = og.resize_after_collection(true);
    assert!(r0.damped && r0.released == 0, "factor 0: {r0:?}");
    let r1 = og.resize_after_collection(true);
    assert_eq!(r1.released, GRANULE, "10 % of 10 granules: {r1:?}");
    assert!(!r1.damped);
    let r2 = og.resize_after_collection(true);
    assert_eq!(r2.released, 3 * GRANULE, "40 % of 9, rounded down: {r2:?}");
    let r3 = og.resize_after_collection(true);
    assert_eq!(r3.released, 6 * GRANULE, "100 % of the rest: {r3:?}");
    assert_eq!(og.committed_bytes(), GRANULE, "down to the survivor's granule");
    assert!(og.contains(keep));
    assert!(og.readable_end + READ_SLACK <= og.committed_bytes());
    // No excess left: the factor resets, so a new excess starts at 0 %.
    let r4 = og.resize_after_collection(true);
    assert_eq!(r4.released, 0);
    assert_eq!(og.shrink_factor, 0);
    let s = og.sizing_stats();
    assert_eq!((s.shrinks, s.shrinks_damped), (3, 1));
}

/// The shrink: the first shrinking collection only records (damped),
/// the next decommits the free tail down to the live set, the generation
/// grows back on demand and reads zero, and a committed end that advanced
/// since the last collection damps the next shrink again.
#[test]
fn a_shrink_after_collection_decommits_the_free_tail_and_the_generation_regrows() {
    let (mut og, reserved) = big_old_gen(16);
    let base = og.base_ptr() as usize;
    let keep = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor");
    let mut big = Vec::new();
    for _ in 0..10 {
        big.push(og.alloc(GRANULE, 8).expect("ten granules"));
    }
    for p in big {
        // SAFETY: each is exactly one `alloc(GRANULE, 8)` block.
        unsafe { og.free(p, GRANULE) };
    }
    og.coalesce_free_blocks();
    let committed_before = og.committed_bytes();

    let r0 = og.resize_after_collection(true);
    assert_eq!(
        r0.released, 0,
        "the first shrinking collection releases nothing"
    );
    if !reserved {
        // The wholly committed store has nothing to decommit, ever.
        assert!(!r0.damped);
        assert_eq!(og.sizing_stats().shrinks, 0);
        return;
    }
    assert!(r0.damped, "...because the generation has only grown so far");
    assert!(committed_before >= 10 * GRANULE);
    assert_eq!(og.committed_bytes(), committed_before);

    let r1 = og.resize_after_collection(true);
    assert!(!r1.damped);
    assert!(
        r1.released >= 9 * GRANULE,
        "the free tail above the survivor's granule goes back ({} bytes)",
        r1.released
    );
    assert_eq!(og.committed_bytes(), committed_before - r1.released);
    assert!(
        og.committed_bytes() <= GRANULE,
        "down to the survivor's granule"
    );
    assert!(og.contains(keep), "the survivor is still readable");
    assert!(
        !og.contains((base + 3 * GRANULE) as *const u8),
        "a decommitted granule is no longer readable old-gen storage"
    );
    // The mark screen reads a whole header at any address `contains`
    // admits: the committed prefix must extend `READ_SLACK` past it.
    assert!(
        og.readable_end + READ_SLACK <= og.committed_bytes(),
        "a shrink must keep READ_SLACK committed above readable_end"
    );
    assert!(!og.contains((base + og.committed_bytes() - 1) as *const u8));
    assert_eq!(og.committed_capacity(), og.committed_bytes());

    // Growth is on demand: the next allocation re-commits, from the
    // committed end, and reads zero.
    let q = og.alloc(5 * GRANULE, 8).expect("the generation grows back");
    // SAFETY: `alloc` handed out `5 * GRANULE` committed, zeroed bytes.
    let bytes = unsafe { std::slice::from_raw_parts(q, 5 * GRANULE) };
    assert!(bytes.iter().all(|&b| b == 0));
    // SAFETY: the last byte of the allocation; faults if the commit is short.
    unsafe { std::ptr::write_volatile(q.add(5 * GRANULE - 1), 0x5A) };
    // SAFETY: pointer arithmetic inside the same allocation.
    assert!(og.contains(unsafe { q.add(5 * GRANULE - 1) }));

    // The committed end advanced since the last resize: damped...
    // SAFETY: `(q, 5 * GRANULE)` is what `alloc` returned.
    unsafe { og.free(q, 5 * GRANULE) };
    og.coalesce_free_blocks();
    let r2 = og.resize_after_collection(true);
    assert!(r2.damped && r2.released == 0);
    // ...and the next collection, with no growth in between, shrinks.
    let r3 = og.resize_after_collection(true);
    assert!(r3.released >= 4 * GRANULE, "{r3:?}");
    let s = og.sizing_stats();
    assert_eq!((s.shrinks, s.shrinks_damped), (2, 2));
    assert!(s.committed_peak >= 10 * GRANULE as u64);
    assert_eq!(og.trigger_stats().commit_refusals, 0);

    // The kill switch: `allow_shrink = false` never releases anything.
    let q2 = og.alloc(6 * GRANULE, 8).expect("again");
    // SAFETY: as above.
    unsafe { og.free(q2, 6 * GRANULE) };
    og.coalesce_free_blocks();
    let _ = og.resize_after_collection(false);
    assert_eq!(og.resize_after_collection(false).released, 0);
}

/// gen r4w6/oldpin6 — a concurrent sweep's end arms the resize for the
/// young pauses that follow: nothing is due before it; a damped attempt is
/// kept for the next pause; the undamped one shrinks (READ_SLACK kept above
/// `readable_end`, the survivor readable) and answers it; a stop-the-world
/// resize answers it too; and a generation that keeps growing between
/// young pauses runs out of attempts instead of sorting its free list on
/// every pause.
#[test]
fn a_concurrent_sweep_end_arms_a_bounded_resize_for_the_young_pauses() {
    let (mut og, reserved) = big_old_gen(16);
    assert_eq!(
        og.resize_after_concurrent_sweep_if_due(true),
        None,
        "nothing due yet"
    );
    let keep = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor");
    let mut big = Vec::new();
    for _ in 0..10 {
        big.push(og.alloc(GRANULE, 8).expect("ten granules"));
    }
    let committed_peak = og.committed_bytes();
    for p in big {
        // SAFETY: each is exactly one `alloc(GRANULE, 8)` block.
        unsafe { og.free(p, GRANULE) };
    }
    og.coalesce_free_blocks();
    og.note_concurrent_collection_end(10 * GRANULE);
    assert_eq!(og.concurrent_resize_attempts(), CONCURRENT_RESIZE_ATTEMPTS);
    let r0 = og
        .resize_after_concurrent_sweep_if_due(true)
        .expect("due after a concurrent end");
    assert_eq!(r0.released, 0);
    if !reserved {
        assert!(!r0.damped, "the wholly committed store has nothing to damp");
        assert_eq!(
            og.concurrent_resize_attempts(),
            0,
            "an undamped resize answers it"
        );
        return;
    }
    assert!(r0.damped, "the generation has only grown so far");
    assert_eq!(
        og.concurrent_resize_attempts(),
        CONCURRENT_RESIZE_ATTEMPTS - 1,
        "a damped attempt is kept for the next young pause"
    );
    let r1 = og
        .resize_after_concurrent_sweep_if_due(true)
        .expect("still due");
    assert!(
        r1.released >= 9 * GRANULE,
        "the free tail goes back: {r1:?}"
    );
    assert_eq!(og.concurrent_resize_attempts(), 0);
    assert_eq!(
        og.resize_after_concurrent_sweep_if_due(true),
        None,
        "answered"
    );
    assert!(og.contains(keep), "the survivor stays readable");
    assert!(
        og.readable_end + READ_SLACK <= og.committed_bytes(),
        "READ_SLACK stays committed above readable_end"
    );
    assert!(og.committed_bytes() < committed_peak);
    let s = og.sizing_stats();
    assert_eq!(
        (s.concurrent_sweep_resizes, s.concurrent_sweep_shrinks),
        (2, 1)
    );
    assert_eq!(s.shrinks, 1);

    // A stop-the-world resize in the same pause answers a pending one.
    og.note_concurrent_collection_end(0);
    let _ = og.resize_after_collection(true);
    assert_eq!(og.concurrent_resize_attempts(), 0);

    // Bounded: growth between every pair of young pauses keeps each
    // attempt damped, and the attempts run out.
    og.note_concurrent_collection_end(0);
    for i in 0..usize::from(CONCURRENT_RESIZE_ATTEMPTS) {
        let n = 5 + i;
        let q = og
            .alloc(n * GRANULE, 8)
            .expect("growth between young pauses");
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(q, n * GRANULE) };
        og.coalesce_free_blocks();
        let r = og
            .resize_after_concurrent_sweep_if_due(true)
            .expect("within the bound");
        assert!(r.damped && r.released == 0, "attempt {i}: {r:?}");
    }
    assert_eq!(
        og.resize_after_concurrent_sweep_if_due(true),
        None,
        "bounded"
    );
}

/// gen r4w6/oldpin6 — the shrink edges the round asked for: a COMPLETELY
/// full generation (no free tail: nothing to release, nothing read past
/// the reservation), and one whose last object sits at the very end of
/// the reservation after everything below it died (the tail run is empty,
/// so the shrink keeps everything: it may only release a FREE tail).
#[test]
fn a_shrink_of_a_full_generation_or_one_with_its_last_object_at_the_end_releases_nothing() {
    let (mut og, reserved) = big_old_gen(8);
    let cap = og.capacity();
    // Completely full: one block covering the whole reservation.
    let all = og.alloc(cap, 8).expect("the whole generation");
    assert_eq!(og.used(), cap);
    assert_eq!(og.tail_free_start(), cap, "no free tail");
    let _ = og.resize_after_collection(true);
    let r = og.resize_after_collection(true);
    assert_eq!(
        r.released, 0,
        "a full generation has no free tail to release"
    );
    // SAFETY: the last byte of a live, committed block.
    unsafe { std::ptr::write_volatile(all.add(cap - 1), 0x5A) };
    // SAFETY: exactly what `alloc` returned.
    unsafe { og.free(all, cap) };
    og.coalesce_free_blocks();

    // The last object at the very end of the reservation, everything
    // below it dead.
    let low = og
        .alloc(cap - GRANULE, 8)
        .expect("everything but the last granule");
    let last = og.alloc(GRANULE, 8).expect("the last granule");
    assert_eq!(last as usize + GRANULE, og.base_ptr() as usize + cap);
    // SAFETY: exactly what `alloc` returned.
    unsafe { og.free(low, cap - GRANULE) };
    og.coalesce_free_blocks();
    let _ = og.resize_after_collection(true);
    let r = og.resize_after_collection(true);
    assert_eq!(r.released, 0, "only a trailing FREE run is released");
    assert!(og.contains(last), "the last object stays readable");
    // SAFETY: the last byte of the reservation, inside `last`.
    unsafe { std::ptr::write_volatile(last.add(GRANULE - 1), 0xA5) };
    if reserved {
        assert!(
            og.committed_bytes() >= cap,
            "nothing below the last object was decommitted"
        );
    }
}

/// A shrink never goes below the `-Xms` prefix, nor below the last
/// allocated byte, nor below `used / 0.3`.
#[test]
fn a_shrink_keeps_the_xms_floor_the_last_object_and_the_free_ratio() {
    let (mut og, reserved) = big_old_gen(16);
    if !reserved {
        return;
    }
    assert_eq!(og.commit_initial_prefix(6 * GRANULE), 6 * GRANULE);
    assert_eq!(og.sizing_stats().commit_floor, (6 * GRANULE) as u64);
    let _ = og.resize_after_collection(true); // the damped first one
    let _ = og.resize_after_collection(true);
    assert!(
        og.committed_bytes() >= 6 * GRANULE,
        "never below the -Xms prefix ({} committed)",
        og.committed_bytes()
    );

    // An object near the top pins everything below it.
    let (mut og, _) = big_old_gen(16);
    let low = og.alloc(GRANULE, 8).expect("low");
    let mid = og.alloc(8 * GRANULE, 8).expect("middle");
    let high = og.alloc(HEADER_SIZE + 64, 8).expect("high");
    // SAFETY: exactly what `alloc` returned.
    unsafe { og.free(mid, 8 * GRANULE) };
    let _ = low;
    let _ = og.resize_after_collection(true);
    let _ = og.resize_after_collection(true);
    assert!(
        og.contains(high),
        "the last allocated object stays committed"
    );

    // The free ratio: 4 granules live need `4 / 0.3` = 13.3 granules.
    let (mut og, _) = big_old_gen(16);
    let live = og.alloc(4 * GRANULE, 8).expect("live");
    let dead = og.alloc(11 * GRANULE, 8).expect("dead");
    // SAFETY: exactly what `alloc` returned.
    unsafe { og.free(dead, 11 * GRANULE) };
    og.coalesce_free_blocks();
    let _ = og.resize_after_collection(true);
    let r = og.resize_after_collection(true);
    assert_eq!(
        r.released, 0,
        "releasing 1-2 granules is below SHRINK_MIN_GRANULES"
    );
    assert!(og.contains(live));
}

// -----------------------------------------------------------------
// gen r5w3/oldgen7 (2026-09-26)
// -----------------------------------------------------------------

/// The interior decommit
/// (`gengc-r5w1-oldgen5-proposal-old-gen-interior-decommit`): a survivor
/// near the top pins the tail shrink, and the large dead run below it is
/// released instead — after one damped pass, down to the free-ratio
/// target — leaving holes that `contains` / `contains_range` refuse, that
/// the walk never reads, and that an allocation re-commits (the hole flag
/// dropping once every hole is refilled).
#[test]
fn an_interior_decommit_releases_a_dead_run_below_a_survivor_at_the_top() {
    let (mut og, reserved) = big_old_gen(32);
    let base = og.base_ptr() as usize;
    let low = og.alloc(HEADER_SIZE + 64, 8).expect("a low survivor");
    let mid = og.alloc(20 * GRANULE, 8).expect("a large run that dies");
    let high = og
        .alloc(HEADER_SIZE + 64, 8)
        .expect("a survivor near the top");
    // SAFETY: exactly what `alloc` returned.
    unsafe { og.free(mid, 20 * GRANULE) };
    og.coalesce_free_blocks();
    if !reserved {
        assert_eq!(og.decommit_interior_free_runs(), 0, "nothing to decommit");
        return;
    }
    // The tail shrink cannot help: `high` pins the committed end.
    let _ = og.resize_after_collection(true);
    let _ = og.resize_after_collection(true);
    let committed_before = og.committed_bytes();
    assert!(committed_before >= 20 * GRANULE, "{committed_before}");

    assert_eq!(
        og.decommit_interior_free_runs(),
        0,
        "the first pass is damped"
    );
    assert_eq!(og.sizing_stats().interior_decommits_damped, 1);
    let released = og.decommit_interior_free_runs();
    assert!(
        released >= 18 * GRANULE,
        "the dead run's interior goes back: {released}"
    );
    assert_eq!(og.committed_bytes(), committed_before - released);
    let s = og.sizing_stats();
    assert_eq!(
        (s.interior_decommits, s.interior_decommitted_bytes),
        (1, released as u64)
    );
    assert_eq!(s.shrinks, 0, "the tail shrink released nothing");

    // Readers: the survivors stay readable; the hole is not old-gen
    // storage, for a point, a header's width at its lower edge, or an
    // extent across it.
    assert!(og.contains(low) && og.contains(high));
    assert!(og.contains_range(low, HEADER_SIZE + 64));
    assert!(
        !og.contains((base + 5 * GRANULE) as *const u8),
        "a released granule"
    );
    assert!(
        !og.contains((base + GRANULE - 8) as *const u8),
        "a header read at the hole's lower edge would straddle into it"
    );
    let run_start = mid as *const u8;
    assert!(
        !og.contains_range(run_start, 20 * GRANULE),
        "an extent across the hole"
    );
    assert_eq!(
        og.walk_objects().iter().map(|&(_, s)| s).sum::<usize>(),
        og.used()
    );
    assert_eq!(og.decommit_interior_free_runs(), 0, "below the target now");

    // An allocation into the hole re-commits what it hands out and reads
    // zero; the flag stays while holes remain...
    let q = og
        .alloc(13 * GRANULE, 8)
        .expect("fits only the run below `high`");
    assert!(
        (q as usize) < high as usize,
        "placed in the hole, not the tail"
    );
    // SAFETY: `alloc` handed out `13 * GRANULE` committed, zeroed bytes.
    let bytes = unsafe { std::slice::from_raw_parts(q, 13 * GRANULE) };
    assert!(bytes.iter().all(|&b| b == 0));
    // SAFETY: the last byte of the allocation; faults if the commit is short.
    unsafe { std::ptr::write_volatile(q.add(13 * GRANULE - 1), 0x5A) };
    assert!(og.sizing_stats().hole_recommits >= 1);
    assert!(og.readable_holes, "the rest of the hole is still released");
    // ...and drops once the last hole is refilled.
    let rest = (high as usize) - (q as usize + 13 * GRANULE);
    let r = og
        .alloc(rest, 8)
        .expect("the rest of the run, an exact fit");
    // SAFETY: the last byte of the allocation.
    unsafe { std::ptr::write_volatile(r.add(rest - 1), 0xA5) };
    assert!(
        !og.readable_holes,
        "every hole refilled: back to the one-number test"
    );
    assert!(og.contains(r));
}

/// gen r5w6/old10 — the post-concurrent resize keeps an attempt when its
/// INTERIOR half is damped. The shape `GenR4W6OldShrinkProbe` makes: the
/// tail resize is undamped (nothing advanced since the last resize) and
/// releases nothing (a survivor pins the top), which used to end the
/// episode on the same pause whose interior pass was damped (its first
/// pass: the committed size "grew"). Now the next young pause runs both
/// halves again and the dead run goes back; the episode then ends. A
/// stop-the-world resize leaves nothing to keep.
#[test]
fn a_damped_interior_pass_keeps_the_post_concurrent_resize_for_the_next_pause() {
    let (mut og, reserved) = big_old_gen(32);
    let _low = og.alloc(HEADER_SIZE + 64, 8).expect("a low survivor");
    let mid = og.alloc(20 * GRANULE, 8).expect("a large run that dies");
    let high = og
        .alloc(HEADER_SIZE + 64, 8)
        .expect("a survivor near the top");
    // SAFETY: exactly what `alloc` returned.
    unsafe { og.free(mid, 20 * GRANULE) };
    og.coalesce_free_blocks();
    if !reserved {
        return;
    }
    // The tail damping's baseline: nothing advances from here on.
    let _ = og.resize_after_collection(true);
    let _ = og.resize_after_collection(true);
    let committed_before = og.committed_bytes();

    og.note_concurrent_collection_end(20 * GRANULE);
    let r0 = og.resize_after_concurrent_sweep_if_due(true).expect("due");
    assert!(
        !r0.damped && r0.released == 0,
        "`high` pins the tail: {r0:?}"
    );
    assert_eq!(
        og.concurrent_resize_attempts(),
        0,
        "the tail half alone ends the episode"
    );
    assert_eq!(
        og.decommit_interior_after_concurrent_resize(),
        0,
        "the first pass is damped"
    );
    assert_eq!(
        og.concurrent_resize_attempts(),
        CONCURRENT_RESIZE_ATTEMPTS - 1,
        "...so the episode keeps an attempt"
    );

    let r1 = og
        .resize_after_concurrent_sweep_if_due(true)
        .expect("kept for this pause");
    assert!(!r1.damped, "{r1:?}");
    let released = og.decommit_interior_after_concurrent_resize();
    assert!(
        released >= 18 * GRANULE,
        "the dead run's interior goes back: {released}"
    );
    assert_eq!(og.committed_bytes(), committed_before - released);
    assert_eq!(
        og.concurrent_resize_attempts(),
        0,
        "an undamped pass ends the episode"
    );
    assert_eq!(
        og.resize_after_concurrent_sweep_if_due(true),
        None,
        "answered"
    );
    assert!(og.contains(high), "the survivor stays readable");

    // A stop-the-world resize answers an episode: nothing left to keep.
    og.note_concurrent_collection_end(0);
    let _ = og.resize_after_collection(true);
    assert!(!og.keep_concurrent_resize_attempt());
    assert_eq!(og.concurrent_resize_attempts(), 0);
}

/// The pinned compaction RE-COMMITS holes an interior decommit left
/// instead of refusing (blocker 1 of the interior-decommit proposal),
/// and completes.
#[test]
fn a_pinned_compaction_recommits_interior_holes_instead_of_refusing() {
    let (mut og, reserved) = big_old_gen(32);
    let a = pc_obj(&mut og, 2, true);
    // A dead `int[]` of 20 granules, freed in place.
    let len = (20 * GRANULE / 4) as u32;
    let dead = pc_int_array(&mut og, len, false);
    let dead_size = (ARRAY_DATA_OFFSET + len as usize * 4 + 7) & !7;
    let b = pc_obj(&mut og, 2, true);
    // SAFETY: the dead array's whole reserved extent.
    unsafe { og.free(dead, dead_size) };
    og.coalesce_free_blocks();
    if !reserved {
        return;
    }
    let _ = og.decommit_interior_free_runs(); // damped
    assert!(og.decommit_interior_free_runs() >= 18 * GRANULE);
    assert!(og.readable_holes);
    let walk = og.walk_objects();
    assert_eq!(walk.len(), 2, "the two survivors");
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins(&walk, seq, Vec::new(), (0, 0), &HashMap::new())
        .expect("re-commits and compacts");
    assert!(!og.readable_holes, "the holes were re-committed");
    let b_new = done.pointer_map[&(b as usize)];
    assert!(b_new < b as usize, "`b` slid down into the freed run");
    assert_eq!(b_new, a as usize + HEADER_SIZE + 2 * SLOT_SIZE);
    assert_eq!(og.used(), 2 * (HEADER_SIZE + 2 * SLOT_SIZE));
}

/// Growing into the young budget
/// (`gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget`): a
/// generation built with headroom grows IN PLACE after a refused request
/// the collection did not make room for, to `MinHeapFreeRatio` capped at
/// its maximum; a promotion-buffer refusal and a request beyond the
/// maximum grow nothing; a generation without headroom never grows.
#[test]
fn a_refused_allocation_grows_a_generation_with_headroom_in_place() {
    let (mut plain, _) = big_old_gen(8);
    assert!(plain.alloc(9 * GRANULE, 8).is_none());
    assert_eq!(plain.grow_after_refusal(), 0, "no headroom, no growth");
    assert_eq!(plain.growth_max(), plain.capacity());

    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 12 * GRANULE);
    let base = og.base_ptr() as usize;
    assert_eq!(
        og.capacity(),
        8 * GRANULE,
        "the capacity starts at its share"
    );
    if og.commit_bits().is_none() {
        assert_eq!(
            og.growth_max(),
            8 * GRANULE,
            "a fallback store cannot grow in place"
        );
        return;
    }
    assert_eq!(og.growth_max(), 12 * GRANULE);
    assert_eq!(og.grow_after_refusal(), 0, "nothing refused");
    let live = og
        .alloc(7 * GRANULE, 8)
        .expect("seven granules of live data");
    // An optional buffer carve is not a reason to grow.
    assert!(og.alloc_unzeroed_buffer(2 * GRANULE, 8).is_none());
    assert_eq!(og.grow_after_refusal(), 0, "a buffer refusal grows nothing");
    assert!(og.alloc(2 * GRANULE, 8).is_none(), "full at its share");
    let epoch = og.reclaim_epoch();
    // used 7 granules: `used / 0.6` = 11.7 granules, rounded up to 12.
    assert_eq!(og.grow_after_refusal(), 4 * GRANULE);
    assert_eq!(og.capacity(), 12 * GRANULE);
    assert_eq!(og.base_ptr() as usize, base, "grown in place");
    assert_ne!(
        og.reclaim_epoch(),
        epoch,
        "an open concurrent cycle must abandon"
    );
    let q = og
        .alloc(2 * GRANULE, 8)
        .expect("the refused request fits now");
    // SAFETY: the last byte of the allocation; faults if the commit is short.
    unsafe { std::ptr::write_volatile(q.add(2 * GRANULE - 1), 0x5A) };
    assert!(og.contains(q) && og.contains(live));
    assert_eq!(og.grow_after_refusal(), 0, "answered");
    // Beyond the maximum: the refusal stands.
    assert!(og.alloc(4 * GRANULE, 8).is_none());
    assert_eq!(og.grow_after_refusal(), 0);
    let s = og.sizing_stats();
    assert_eq!(
        (s.borrow_growths, s.borrowed_bytes),
        (1, (4 * GRANULE) as u64)
    );
    assert_eq!(s.growth_max_bytes, (12 * GRANULE) as u64);
    assert_eq!(og.used(), 9 * GRANULE);
}

/// Alignment padding can make a hole with the raw request size unusable.
#[test]
fn a_refused_aligned_allocation_grows_past_a_raw_size_only_hole() {
    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 12 * GRANULE);
    if og.commit_bits().is_none() {
        return;
    }
    let _first = og.alloc(HEADER_SIZE, 8).expect("aligned first block");
    let hole = og.alloc(24, 8).expect("24-byte block");
    let _fill = og
        .alloc(og.capacity() - og.used(), 8)
        .expect("fill the remaining capacity");
    // SAFETY: release exactly the second allocation's 24-byte extent.
    unsafe { og.free(hole, 24) };
    og.coalesce_free_blocks();
    assert_eq!(og.largest_free_block(), 24);

    assert!(og.alloc(24, 16).is_none(), "alignment makes the hole too small");
    assert!(og.grow_after_refusal() > 0, "growth includes pad bytes");
    assert!(og.alloc(24, 16).is_some(), "the grown tail fits");
}

/// A successful reactive pinned repair keeps bounded reservation headroom.
#[test]
fn a_reactive_fragmentation_repair_preserves_one_growth_granule() {
    let g = GRANULE;
    let mut og = OldGen::with_growth_headroom(8 * g, 12 * g);
    if og.commit_bits().is_none() {
        return;
    }
    og.set_growth_soft_max(8 * g);
    let _live = og.alloc(6 * g, 8).expect("live old objects");
    let free_after_repair = og.alloc(2 * g, 8).expect("fill current capacity");
    assert!(og.alloc(2 * g, 8).is_none(), "the request is refused");
    // SAFETY: this is the complete block allocated immediately above.
    unsafe { og.free(free_after_repair, 2 * g) };
    og.coalesce_free_blocks();
    assert!(og.largest_free_block() >= 2 * g);

    og.note_fragmentation_pinned_attempt();
    og.note_fragmentation_pinned_repair();
    assert_eq!(og.grow_after_refusal(), g);
    assert_eq!(og.capacity(), 9 * g);
    assert!(og.alloc(2 * g, 8).is_some());
}

#[test]
fn an_unneeded_fragmentation_attempt_does_not_grow_old_gen() {
    let g = GRANULE;
    let mut og = OldGen::with_growth_headroom(8 * g, 12 * g);
    if og.commit_bits().is_none() {
        return;
    }
    og.set_growth_soft_max(8 * g);
    let _live = og.alloc(6 * g, 8).expect("live old objects");
    let free_after_sweep = og.alloc(2 * g, 8).expect("fill current capacity");
    assert!(og.alloc(2 * g, 8).is_none(), "the request is refused");
    // SAFETY: this is the complete block allocated immediately above.
    unsafe { og.free(free_after_sweep, 2 * g) };
    og.coalesce_free_blocks();
    og.note_fragmentation_pinned_attempt();

    assert_eq!(og.grow_after_refusal(), 0);
    assert_eq!(og.capacity(), 8 * g);
    assert!(og.alloc(2 * g, 8).is_some());
}

/// JIT r20 wave 18, lane gcI: with a soft cap the free-ratio slack stops
/// there, and a request the soft capacity cannot hold grows the generation
/// past it by what the request needs, up to the reservation.
#[test]
fn jr20_w18_gci_a_refusal_grows_past_the_soft_cap_only_by_what_it_needs() {
    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
    if og.commit_bits().is_none() {
        return;
    }
    og.set_growth_soft_max(10 * GRANULE);
    let _live = og
        .alloc(7 * GRANULE, 8)
        .expect("seven granules of live data");
    assert!(og.alloc(2 * GRANULE, 8).is_none(), "full at its share");
    // used 7: the slack wants 11.7 granules, the soft cap stops it at 10,
    // which holds the 9 the request needs.
    assert_eq!(og.grow_after_refusal(), 2 * GRANULE);
    assert_eq!(og.capacity(), 10 * GRANULE);
    let _more = og.alloc(2 * GRANULE, 8).expect("the refused request fits");
    // used 9 of 10: a 4-granule request needs 13 -- past the soft cap.
    assert!(og.alloc(4 * GRANULE, 8).is_none());
    assert_eq!(
        og.grow_after_refusal(),
        3 * GRANULE,
        "grows by what it needs"
    );
    assert_eq!(og.capacity(), 13 * GRANULE);
    let q = og.alloc(4 * GRANULE, 8).expect("the grown tail holds it");
    // SAFETY: the last byte of the allocation; faults if the commit is short.
    unsafe { std::ptr::write_volatile(q.add(4 * GRANULE - 1), 0x5A) };
    // Beyond the reservation: the refusal stands.
    assert!(og.alloc(4 * GRANULE, 8).is_none());
    assert_eq!(og.grow_after_refusal(), 0);
}

/// gcj j1/og (`CRATONVM_GC_OLD_FREE_RATIO_GROWTH`) — the target is
/// HotSpot's `used / (1 - MinHeapFreeRatio)`, granule-rounded, never below
/// the capacity and never above the limit.
#[test]
fn gcj_j1og_the_min_free_ratio_capacity_is_hotspots_expansion_target() {
    let g = GRANULE;
    // 40 % free already: no growth.
    assert_eq!(OldGen::min_free_ratio_capacity(6 * g, 40, 10 * g, 20 * g), 10 * g);
    // used 7 of 10 (30 % free): 7 / 0.6 = 11.67 -> 12 granules.
    assert_eq!(OldGen::min_free_ratio_capacity(7 * g, 40, 10 * g, 20 * g), 12 * g);
    // The limit caps it.
    assert_eq!(OldGen::min_free_ratio_capacity(9 * g, 40, 10 * g, 11 * g), 11 * g);
    // MinHeapFreeRatio=0: never; =100: straight to the limit.
    assert_eq!(OldGen::min_free_ratio_capacity(10 * g, 0, 10 * g, 20 * g), 10 * g);
    assert_eq!(OldGen::min_free_ratio_capacity(1, 100, 10 * g, 20 * g), 20 * g);
    // A limit below the capacity never shrinks it.
    assert_eq!(OldGen::min_free_ratio_capacity(9 * g, 40, 10 * g, 8 * g), 10 * g);
}

/// gcj j1/og — armed, a generation with headroom grows to restore
/// `MinHeapFreeRatio` without any refusal, stops at the soft cap, counts
/// the growth apart from the refusal growths, and keeps the new span
/// allocatable; disarmed (`=0`), nothing grows.
#[test]
fn gcj_j1og_a_full_generation_grows_to_the_free_ratio_only_when_armed() {
    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
    if og.commit_bits().is_none() {
        return;
    }
    og.set_growth_soft_max(11 * GRANULE);
    let _live = og
        .alloc(7 * GRANULE, 8)
        .expect("seven granules of live data");
    // Off (the `=0` arm): no growth, nothing counted.
    og.set_free_ratio_growth_for_test(false);
    assert_eq!(og.grow_to_min_free_ratio(), 0);
    assert_eq!(og.capacity(), 8 * GRANULE);
    og.set_free_ratio_growth_for_test(true);
    let epoch = og.reclaim_epoch();
    // used 7: 7 / 0.6 = 11.67 granules, capped at the soft 11.
    assert_eq!(og.grow_to_min_free_ratio(), 3 * GRANULE);
    assert_eq!(og.capacity(), 11 * GRANULE);
    assert_ne!(og.reclaim_epoch(), epoch, "an open concurrent cycle must abandon");
    // At the soft cap: no further slack growth.
    assert_eq!(og.grow_to_min_free_ratio(), 0);
    let q = og.alloc(3 * GRANULE, 8).expect("the grown span is allocatable");
    // SAFETY: the last byte of the allocation; faults if the commit is short.
    unsafe { std::ptr::write_volatile(q.add(3 * GRANULE - 1), 0x5A) };
    let s = og.sizing_stats();
    assert_eq!(
        (s.free_ratio_growths, s.free_ratio_grown_bytes),
        (1, (3 * GRANULE) as u64)
    );
    assert_eq!((s.borrow_growths, s.borrowed_bytes), (0, 0));
    // Without headroom the switch changes nothing.
    let (mut plain, _) = big_old_gen(8);
    plain.set_free_ratio_growth_for_test(true);
    let _p = plain.alloc(7 * GRANULE, 8);
    assert_eq!(plain.grow_to_min_free_ratio(), 0);
}

#[test]
fn gcj_a_concurrent_sweep_defers_free_ratio_growth_to_one_young_pause() {
    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
    if og.commit_bits().is_none() {
        return;
    }
    og.set_free_ratio_growth_for_test(true);
    og.set_growth_soft_max(11 * GRANULE);
    og.alloc(7 * GRANULE, 8)
        .expect("live set above the free-ratio target");
    assert_eq!(og.grow_after_concurrent_sweep_if_due(), 0);
    og.note_concurrent_collection_end(0);
    assert_eq!(og.capacity(), 8 * GRANULE, "a concurrent slice cannot resize");
    let epoch = og.reclaim_epoch();
    assert_eq!(og.grow_after_concurrent_sweep_if_due(), 3 * GRANULE);
    assert_eq!(og.capacity(), 11 * GRANULE);
    assert_ne!(og.reclaim_epoch(), epoch);

    og.set_growth_soft_max(16 * GRANULE);
    og.alloc(2 * GRANULE, 8)
        .expect("promotions after the completed cycle");
    assert_eq!(og.grow_after_concurrent_sweep_if_due(), 0, "one evaluation per cycle");
    og.note_concurrent_collection_end(0);
    assert_eq!(og.grow_after_concurrent_sweep_if_due(), 4 * GRANULE);
    assert_eq!(og.capacity(), 15 * GRANULE);
    assert_eq!(og.sizing_stats().free_ratio_growths, 2);
}

#[test]
fn gcj_stw_growth_and_the_kill_arm_consume_deferred_concurrent_growth() {
    for enabled in [false, true] {
        let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
        if og.commit_bits().is_none() {
            return;
        }
        og.set_free_ratio_growth_for_test(enabled);
        og.set_growth_soft_max(11 * GRANULE);
        og.alloc(7 * GRANULE, 8).expect("live set");
        og.note_concurrent_collection_end(0);
        if enabled {
            assert_eq!(og.grow_to_min_free_ratio(), 3 * GRANULE, "intervening STW collection");
            og.set_growth_soft_max(16 * GRANULE);
            og.alloc(2 * GRANULE, 8).expect("new promotions");
            og.note_concurrent_sweep_stopped(0);
        } else {
            assert_eq!(og.grow_after_concurrent_sweep_if_due(), 0);
            assert_eq!(og.capacity(), 8 * GRANULE);
            og.set_free_ratio_growth_for_test(true);
        }
        assert_eq!(og.grow_after_concurrent_sweep_if_due(), 0, "the prior cycle was answered");
    }
}

/// gcj j2/og2 B4 — a concurrent cycle's end leaves the free-ratio growth
/// due exactly once: nothing before a sweep ended, one growth after it
/// (counted as after-concurrent), nothing on the next pause.
#[test]
fn gcj_j2og2_a_concurrent_sweep_end_leaves_one_free_ratio_growth_due() {
    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
    if og.commit_bits().is_none() {
        return;
    }
    og.set_growth_soft_max(11 * GRANULE);
    og.set_free_ratio_growth_for_test(true);
    let _live = og
        .alloc(7 * GRANULE, 8)
        .expect("seven granules of live data");
    assert_eq!(og.grow_to_min_free_ratio_after_concurrent_sweep_if_due(), 0);
    assert_eq!(og.capacity(), 8 * GRANULE);
    og.note_concurrent_collection_end(0);
    assert_eq!(
        og.grow_to_min_free_ratio_after_concurrent_sweep_if_due(),
        3 * GRANULE
    );
    assert_eq!(og.capacity(), 11 * GRANULE);
    assert_eq!(og.grow_to_min_free_ratio_after_concurrent_sweep_if_due(), 0);
    let s = og.sizing_stats();
    assert_eq!(
        (s.free_ratio_growths, s.free_ratio_growths_after_concurrent),
        (1, 1)
    );
}

/// gen r5w6/old10 — a FRAGMENTATION refusal no compaction answered (a
/// survivor pinned between the holes, as on the non-moving path's pinned
/// compaction) grows the generation so its trailing free run holds the
/// request. Before, `used + refused` fitted the capacity in total, so the
/// generation did not grow and the refusal stood although the reservation
/// had room; and a growth to `used + refused` would have left the tail
/// too short anyway.
#[test]
fn a_fragmentation_refusal_grows_the_tail_until_it_holds_the_request() {
    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
    if og.commit_bits().is_none() {
        return;
    }
    let low = og.alloc(GRANULE, 8).expect("a low survivor");
    let mid = og.alloc(3 * GRANULE, 8).expect("a run that dies");
    let high = og.alloc(GRANULE, 8).expect("a survivor between the holes");
    // SAFETY: exactly what `alloc` returned.
    unsafe { og.free(mid, 3 * GRANULE) };
    og.coalesce_free_blocks();
    // 6 of 8 granules free, in two runs of 3: a 5-granule request fails.
    assert_eq!(og.used(), 2 * GRANULE);
    assert!(og.alloc(5 * GRANULE, 8).is_none(), "fragmented");
    // The tail run starts at 5 granules: 5 + 5 = 10 granules.
    assert_eq!(og.grow_after_refusal(), 2 * GRANULE);
    assert_eq!(og.capacity(), 10 * GRANULE);
    let q = og
        .alloc(5 * GRANULE, 8)
        .expect("the grown tail holds the request");
    assert!(
        (q as usize) > high as usize,
        "placed in the tail, above the survivor"
    );
    // SAFETY: the last byte of the allocation; faults if the commit is short.
    unsafe { std::ptr::write_volatile(q.add(5 * GRANULE - 1), 0x5A) };
    assert!(og.contains(low) && og.contains(high));
    assert_eq!(og.grow_after_refusal(), 0, "answered");
}

/// Humongous arrays from the top
/// (`gengc-r5w1-oldgen5-proposal-humongous-arrays-from-the-top-of-old-gen`,
/// its unit test): alternating humongous and small requests put every
/// small object below the first humongous one, the humongous ones back to
/// back from the reservation's end, only their own pages committed (the
/// gap is a hole); freed, they merge with the gap into ONE block, which
/// the tail shrink then returns.
#[test]
fn humongous_arrays_placed_from_the_top_merge_into_the_free_tail_when_they_die() {
    let (mut og, reserved) = big_old_gen(16);
    let base = og.base_ptr() as usize;
    let cap = og.capacity();
    if !reserved {
        // No holes on the wholly committed store: the ordinary best fit.
        let p = og.alloc_from_top(2 * GRANULE, 8, true).expect("fits");
        assert_eq!(p as usize, base);
        assert_eq!(og.sizing_stats().humongous_top_fallbacks, 1);
        return;
    }
    let small_size = HEADER_SIZE + 64;
    let mut small = Vec::new();
    let mut big = Vec::new();
    for _ in 0..3 {
        small.push(og.alloc(small_size, 8).expect("small"));
        big.push(og.alloc_from_top(2 * GRANULE, 8, true).expect("humongous"));
    }
    small.push(og.alloc(small_size, 8).expect("small"));
    assert_eq!(
        big[0] as usize + 2 * GRANULE,
        base + cap,
        "the first at the very top"
    );
    assert_eq!(
        big[1] as usize + 2 * GRANULE,
        big[0] as usize,
        "back to back"
    );
    assert_eq!(big[2] as usize + 2 * GRANULE, big[1] as usize);
    let lowest_big = big[2] as usize;
    assert!(small.iter().all(|&p| p as usize + small_size <= lowest_big));
    assert_eq!(og.sizing_stats().humongous_top_allocs, 3);
    // Only the small objects' granule and the arrays' are committed.
    assert!(
        og.committed_bytes() <= 7 * GRANULE,
        "{}",
        og.committed_bytes()
    );
    assert!(og.readable_holes, "the gap below the arrays is a hole");
    assert!(!og.contains((base + 5 * GRANULE) as *const u8));
    assert!(og.contains(big[2]) && og.contains(small[3]));
    // SAFETY: the last byte of the topmost array; faults if uncommitted.
    unsafe { std::ptr::write_volatile(big[0].add(2 * GRANULE - 1), 0x5A) };

    for p in big {
        // SAFETY: exactly what `alloc_from_top` returned.
        unsafe { og.free(p, 2 * GRANULE) };
    }
    og.coalesce_free_blocks();
    let (_, largest) = og.free_bytes_and_largest();
    assert_eq!(
        largest,
        cap - 4 * small_size,
        "the dead arrays merged with the gap"
    );
    let _ = og.resize_after_collection(true);
    let r = og.resize_after_collection(true);
    assert!(
        r.released >= 6 * GRANULE,
        "the tail shrink returns them: {r:?}"
    );
    assert!(og.committed_bytes() <= GRANULE);
    assert!(og.contains(small[0]));
    assert_eq!(
        og.walk_objects().iter().map(|&(_, s)| s).sum::<usize>(),
        og.used()
    );
}

/// The fragmentation trigger's refusal half: an allocation refused while
/// the free list holds 4x its size arms ONE compaction request, a full
/// generation's refusal and a promotion buffer's do not, and a completed
/// compaction answers it (and makes the request fit).
#[test]
fn a_fragmentation_refusal_arms_one_compaction_request_and_a_compaction_answers_it() {
    // 1024 three-slot (64-byte) objects fill 64 KiB exactly.
    let (mut og, offsets) = old_gen_with_objects(1024, 3);
    assert_eq!(og.free_block_count(), 0);
    let base = og.base_ptr() as usize;
    // A FULL generation: a refusal, but not a fragmentation one.
    assert!(og.alloc(64, 8).is_none());
    assert_eq!(og.sizing_stats().fragmentation_refusals, 0);
    assert!(!og.arm_fragmentation_compaction());

    // Every other object dies: 32 KiB free, no hole larger than 64 bytes.
    for (i, &off) in offsets.iter().enumerate() {
        if i % 2 == 0 {
            // SAFETY: each is exactly one `alloc`ed 64-byte object.
            unsafe { og.free((base + off) as *mut u8, 64) };
        } else {
            // SAFETY: a header `old_gen_with_objects` wrote.
            unsafe { (*((base + off) as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
        }
    }
    // The ladder's single-request form: room in total, no hole.
    assert!(og.fragmented_for(1024));
    assert!(!og.fragmented_for(64), "a 64-byte hole serves 64");
    assert!(
        !og.fragmented_for(40_000),
        "32 KiB free cannot serve 40 000"
    );
    // An optional promotion BUFFER that does not fit is not the signal.
    assert!(og.alloc_unzeroed_buffer(1024, 8).is_none());
    assert_eq!(og.fragmentation_pending_request(), 0);
    // An object that does not fit is.
    assert!(og.alloc(1024, 8).is_none());
    assert_eq!(og.fragmentation_pending_request(), 1024);
    assert_eq!(og.sizing_stats().fragmentation_refusals, 1);

    // One request per episode.
    assert!(og.arm_fragmentation_compaction());
    assert!(!og.arm_fragmentation_compaction());
    assert!(og.take_fragmentation_compaction_request());
    assert!(!og.take_fragmentation_compaction_request());
    assert_eq!(og.sizing_stats().fragmentation_compactions_requested, 1);

    // The compaction the request asks for makes the request fit.
    let _ = og.compact();
    assert_eq!(og.used(), 512 * 64);
    assert_eq!(og.sizing_stats().compactions, 1);
    assert!(og.alloc(1024, 8).is_some());
}

/// The fragmentation trigger's post-collection half: the existing
/// `fragmentation_repair_due` state arms a request, once, and consumes the
/// scale it judged against.
#[test]
fn a_fragmented_free_list_after_a_collection_arms_the_request_once() {
    let mut og = OldGen::new(64 * 1024);
    og.alloc(2048, 8).unwrap();
    let small_count = (og.capacity() - og.used()) / 1024;
    let mut ptrs = Vec::new();
    for _ in 0..small_count {
        ptrs.push(og.alloc(1024, 8).unwrap());
    }
    for p in ptrs.iter().step_by(2).take(20) {
        // SAFETY: each `p` came from `alloc(1024, 8)` and is freed once.
        unsafe { og.free(*p, 1024) };
    }
    assert!(og.fragmentation_repair_due());
    assert!(og.note_fragmentation_after_collection());
    assert_eq!(og.fragmentation_pending_request(), 2048);
    assert_eq!(og.recent_max_request(), 0, "the scale is consumed");
    assert!(!og.note_fragmentation_after_collection());
}

/// `walk_objects_with_gaps` reports the unwalked tail of a region whose
/// scan broke, the walked and gap bytes add up to `used`, and a reference
/// INTO the gap is not an escape for the gap-aware closure (it is for the
/// plain one, which cannot tell it from a freed block).
#[test]
fn a_broken_region_is_reported_as_a_gap_and_a_reference_into_it_is_not_an_escape() {
    let mut og = OldGen::new(4096);
    let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let g = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
    let base = og.base_ptr() as usize;
    // SAFETY: both are live blocks sized for a header and one slot.
    unsafe {
        let a_hdr = &mut *(a as *mut ObjectHeader);
        a_hdr.set_num_slots(1);
        a_hdr.add_gc_flags(GC_FLAG_MARKED);
        std::ptr::write(
            a.add(HEADER_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(g))),
        );
        (*(g as *mut ObjectHeader)).set_num_slots(1);
        // Not a valid `ObjectKind`: the walk breaks here.
        std::ptr::write(g.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8);
    }
    let (walk, gaps) = og.walk_objects_with_gaps();
    assert_eq!(walk, vec![(a, HEADER_SIZE + SLOT_SIZE)]);
    let g_off = g as usize - base;
    assert_eq!(gaps, vec![(g_off, g_off + HEADER_SIZE + SLOT_SIZE)]);
    let walked: usize = walk.iter().map(|&(_, s)| s).sum();
    let gap: usize = gaps.iter().map(|&(s, e)| e - s).sum();
    assert_eq!(walked + gap, og.used());

    assert!(
        og.close_live_set(&walk).1,
        "the plain closure calls it an escape"
    );
    assert!(
        !og.close_live_set_with_gaps(&walk, &gaps).1,
        "the gap-aware closure knows the gap is retained"
    );
    assert_eq!(og.note_walk_gap_recovery(gap), 0);
    assert_eq!(og.sizing_stats().walk_gap_recoveries, 1);
    assert_eq!(og.sizing_stats().walk_gap_bytes_last, gap as u64);
    // A complete walk has no gaps.
    let (clean, _) = old_gen_with_objects(4, 2);
    assert!(clean.walk_objects_with_gaps().1.is_empty());
}

// --- gen r4w5/oldcompact5: compaction around pinned objects -------------

/// A legacy object with `slots` reference slots; marked (live) or not.
fn pc_obj(og: &mut OldGen, slots: u32, marked: bool) -> *mut u8 {
    let p = og
        .alloc(HEADER_SIZE + SLOT_SIZE * slots as usize, 8)
        .unwrap();
    // SAFETY: a fresh, zeroed block sized for a header and `slots` slots.
    unsafe {
        (*(p as *mut ObjectHeader)).set_num_slots(slots);
        if marked {
            (*(p as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
        }
    }
    p
}

/// An `int[len]`; marked (live) or not.
fn pc_int_array(og: &mut OldGen, len: u32, marked: bool) -> *mut u8 {
    let p = og.alloc(ARRAY_DATA_OFFSET + len as usize * 4, 8).unwrap();
    // SAFETY: a fresh, zeroed block sized for the array header and body.
    unsafe {
        std::ptr::write(
            p as *mut ObjectHeader,
            ObjectHeader::new(
                cratonvm_types::ClassId::new(1),
                ObjectKind::Array,
                ArrayElementType::Int,
                len,
                len,
            ),
        );
        if marked {
            (*(p as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
        }
    }
    p
}

fn pc_set_ref(holder: *mut u8, slot: usize, target: usize) {
    // SAFETY: test objects are legacy objects with at least `slot + 1`
    // slots; `target` is non-null and 8-aligned.
    unsafe {
        std::ptr::write(
            holder.add(HEADER_SIZE + slot * SLOT_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(target as *mut u8))),
        );
    }
}

fn pc_get_ref(holder: usize, slot: usize) -> usize {
    let slot_addr = holder + HEADER_SIZE + slot * SLOT_SIZE;
    // SAFETY: as for `pc_set_ref`.
    match unsafe { std::ptr::read(slot_addr as *const Value) } {
        Value::Object(Some(r)) => r.as_ptr() as usize,
        _ => 0,
    }
}

fn pc_size(slots: usize) -> usize {
    HEADER_SIZE + SLOT_SIZE * slots
}

/// The pinned object keeps its address; the live objects above it slide
/// down to its end and are forwarded; every reference is rewritten — the
/// pinned object's own included — and a young reference is left alone
/// but its holder is reported at its NEW address; the walk, the
/// accounting and the free list all describe the new layout.
#[test]
fn a_pinned_object_keeps_its_address_and_the_rest_slides_around_it() {
    let mut og = OldGen::new(64 * 1024);
    let d0 = pc_obj(&mut og, 4, false);
    let p = pc_obj(&mut og, 2, true);
    let d1 = pc_obj(&mut og, 3, false);
    let m1 = pc_obj(&mut og, 2, true);
    let m2 = pc_obj(&mut og, 1, true);
    let _d2 = pc_obj(&mut og, 5, false);
    // A stand-in "young generation": four words outside old gen.
    let young_words = [0u64; 4];
    let y = young_words.as_ptr() as usize;
    pc_set_ref(p, 0, m1 as usize);
    pc_set_ref(m1, 0, m2 as usize);
    pc_set_ref(m1, 1, p as usize);
    pc_set_ref(m2, 0, y);

    let walk = og.walk_objects();
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins(&walk, seq, vec![p as usize], (y, y + 32), &HashMap::new())
        .expect("a healthy grid compacts");

    assert_eq!(done.pinned, 1);
    assert_eq!(done.moved, 2);
    assert_eq!(
        done.pointer_map
            .get(&(p as usize))
            .copied()
            .unwrap_or(p as usize),
        p as usize,
        "the pin does not move",
    );
    let m1_new = done.pointer_map[&(m1 as usize)];
    let m2_new = done.pointer_map[&(m2 as usize)];
    assert_eq!(
        m1_new,
        p as usize + pc_size(2),
        "m1 slides to the pin's end"
    );
    assert_eq!(m1_new, d1 as usize, "...which is where the dead d1 was");
    assert_eq!(m2_new, m1_new + pc_size(2));
    assert_eq!(
        pc_get_ref(p as usize, 0),
        m1_new,
        "the pin's own slot is forwarded"
    );
    assert_eq!(pc_get_ref(m1_new, 0), m2_new);
    assert_eq!(
        pc_get_ref(m1_new, 1),
        p as usize,
        "a reference to the pin is unchanged"
    );
    assert_eq!(pc_get_ref(m2_new, 0), y, "a young reference is not touched");
    assert_eq!(done.young_ref_holders, vec![m2_new]);
    for q in [p as usize, m1_new, m2_new] {
        // SAFETY: each is a survivor's header after the slide.
        let h = unsafe { &*(q as *const ObjectHeader) };
        assert!(!h.is_forwarded(), "the forward is retired");
        assert_eq!(h.gc_flags() & GC_FLAG_MARKED, 0, "the mark is cleared");
    }
    let after: Vec<(usize, usize)> = og
        .walk_objects()
        .iter()
        .map(|&(q, s)| (q as usize, s))
        .collect();
    assert_eq!(
        after,
        vec![
            (p as usize, pc_size(2)),
            (m1_new, pc_size(2)),
            (m2_new, pc_size(1))
        ]
    );
    assert_eq!(og.used(), 2 * pc_size(2) + pc_size(1));
    let (free, largest) = og.free_bytes_and_largest();
    assert_eq!(
        free + og.used(),
        og.capacity(),
        "used + free is the capacity"
    );
    assert_eq!(
        og.free_block_count(),
        2,
        "the hole in front of the pin, and the tail"
    );
    assert!(
        !og.is_allocated_addr(d0),
        "the hole in front of the pin is free"
    );
    assert_eq!(
        largest,
        og.capacity() - (m2_new + pc_size(1) - og.base_ptr() as usize)
    );
    assert_eq!(
        og.high_water(),
        m2_new + pc_size(1) - og.base_ptr() as usize
    );
    let s = og.sizing_stats();
    assert_eq!(
        (
            s.pinned_compactions,
            s.compactions,
            s.pinned_compaction_pins_last
        ),
        (1, 1, 1)
    );
    // The hole in front of the pin is reusable, and the pin is untouched.
    assert_eq!(og.alloc(pc_size(4), 8), Some(d0));
    assert_eq!(pc_get_ref(p as usize, 0), m1_new);
}

/// gcd d4/n (`gcd-d3m-phase5-compacts-old-gen-under-unmapped-compiled-words`):
/// the veto counter counts, returns the previous count, and is reported by
/// `sizing_stats`.
#[test]
fn gcd_d4n_the_moving_major_veto_is_counted() {
    let mut og = OldGen::new(64 * 1024);
    assert_eq!(og.sizing_stats().moving_major_compaction_vetoes, 0);
    assert_eq!(og.note_moving_major_compaction_veto(), 0);
    assert_eq!(og.note_moving_major_compaction_veto(), 1);
    assert_eq!(og.sizing_stats().moving_major_compaction_vetoes, 2);
}

/// gch h1/jo: the moving legacy-seed major counter counts, returns the
/// new count, is reported by `sizing_stats`, and leaves the requested
/// majors' true-root counters alone (it is not a fallback).
#[test]
fn gch_h1jo_moving_legacy_seed_majors_are_counted_apart_from_fallbacks() {
    let mut og = OldGen::new(64 * 1024);
    assert_eq!(og.sizing_stats().moving_legacy_seed_majors, 0);
    assert_eq!(og.note_moving_legacy_seed_major(), 1);
    assert_eq!(og.note_moving_legacy_seed_major(), 2);
    let s = og.sizing_stats();
    assert_eq!(s.moving_legacy_seed_majors, 2);
    assert_eq!(s.true_root_fallbacks, 0);
    assert_eq!(s.true_root_majors, 0);
    assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::MovingCycle), 0);
}

/// gch h2/og — the moving Phase 5 peeks at a pending compaction request
/// before arming the true-root seed; the peek must not consume it (the
/// major's `take_fragmentation_compaction_request` stays the one consumer).
#[test]
fn gch_h2og_a_pending_compaction_request_is_peeked_not_consumed() {
    let mut og = OldGen::new(64 * 1024);
    assert!(!og.fragmentation_compaction_pending());
    og.request_fragmentation_compaction();
    assert!(og.fragmentation_compaction_pending());
    assert!(
        og.fragmentation_compaction_pending(),
        "a peek does not consume"
    );
    assert!(og.take_fragmentation_compaction_request());
    assert!(!og.fragmentation_compaction_pending(), "the take does");
}

/// gcd d3/n (`gengc-r4w3-cards3-precise-array-cards-residuals`, item 3
/// producer (d)). A surviving reference array with young elements reports
/// each young element's FINAL address, one per card, on an element-precise
/// table, and its final base (the header card) on a header-grained one; an
/// ordinary object reports its final base either way. The young slots are
/// left as they were.
#[test]
fn gcd_d3n_a_compacted_reference_array_reports_its_young_elements_cards() {
    // The stand-in young words below are raw process addresses; a narrow
    // geometry could not encode them.
    if cratonvm_types::narrow_oop::narrow_oops_enabled() {
        return;
    }
    let esz = ref_element_size();
    let card = cratonvm_gc_core::card_table::CARD_SIZE;
    let off = |i: usize| ARRAY_DATA_OFFSET + i * esz;
    let young_words = [0u64; 4];
    let y = young_words.as_ptr() as usize;
    const LEN: u32 = 1000;
    let young_elements = [3usize, 900, 901];
    for element_cards in [false, true] {
        let mut og = OldGen::new(64 * 1024);
        let _dead = pc_obj(&mut og, 4, false);
        let arr = og.alloc(off(LEN as usize), 8).expect("room");
        // SAFETY: a fresh, zeroed block sized for the array header and
        // `LEN` reference elements; the stores stay inside it.
        unsafe {
            std::ptr::write(
                arr as *mut ObjectHeader,
                ObjectHeader::new(
                    cratonvm_types::ClassId::new(1),
                    ObjectKind::Array,
                    ArrayElementType::Reference,
                    LEN,
                    LEN,
                ),
            );
            (*(arr as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
            for &i in &young_elements {
                cratonvm_types::narrow_oop::write_ref_slot(arr.add(off(i)), (y + 8) as u64);
            }
        }
        let m = pc_obj(&mut og, 2, true);
        pc_set_ref(m, 1, y);

        let walk = og.walk_objects();
        let seq = og.free_list_seq();
        let card_base = og.base_ptr() as usize;
        let done = og
            .compact_around_pins_with_element_cards(
                &walk,
                seq,
                Vec::new(),
                (y, y + 32),
                &HashMap::new(),
                element_cards.then_some(card_base),
            )
            .expect("a healthy grid compacts");
        let arr_new = done.pointer_map[&(arr as usize)];
        let m_new = done.pointer_map[&(m as usize)];
        assert!(
            arr_new < arr as usize,
            "the array slid onto the dead object"
        );
        let mut expected = Vec::new();
        if element_cards {
            let mut last = usize::MAX;
            for &i in &young_elements {
                let c = (arr_new + off(i) - card_base) / card;
                if c != last {
                    last = c;
                    expected.push(arr_new + off(i));
                }
            }
            assert!(
                !expected.contains(&arr_new) && expected.len() >= 2,
                "element addresses, not the base",
            );
        } else {
            expected.push(arr_new);
        }
        expected.push(m_new);
        assert_eq!(
            done.young_ref_holders, expected,
            "element_cards={element_cards}"
        );
        for &i in &young_elements {
            // SAFETY: a slot of the survivor at its final address.
            let v = unsafe { read_ref_slot(arr_new.wrapping_add(off(i)) as *const u8) };
            assert_eq!(v as usize, y + 8, "a young element is not touched");
        }
        assert_eq!(pc_get_ref(m_new, 1), y);
    }
}

/// The false-`OutOfMemoryError` shape at the `OldGen` level: the free
/// bytes hold a request, no hole does, and one live object sits between
/// the holes. With the object in front PINNED, the live array behind it
/// slides down onto it and the request fits. The object-start table then
/// finds both survivors where they are now.
#[test]
fn a_humongous_request_fits_after_a_pinned_compaction() {
    let mut og = OldGen::new(64 * 1024);
    let pin = pc_obj(&mut og, 1, true);
    let _a = pc_int_array(&mut og, 2048, false);
    let b = pc_int_array(&mut og, 2048, true);
    let _c = pc_int_array(&mut og, 2048, false);
    // SAFETY: element 0 of the live `int[2048]` `b`.
    unsafe { std::ptr::write(b.add(ARRAY_DATA_OFFSET) as *mut i32, 0x5EED_1234) };
    // The dead arrays go back to the free list, as the in-place sweep
    // leaves them: two holes and the tail, none big enough.
    let walk = og.walk_objects();
    for &(q, size) in &walk {
        // SAFETY: a walked object's header.
        let marked = unsafe { &*(q as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;
        if !marked {
            // SAFETY: `(q, size)` is exactly a walked allocation.
            unsafe { og.free(q, size) };
        }
    }
    let _ = og.coalesce_free_blocks();
    let (free, largest) = og.free_bytes_and_largest();
    let need = largest + 8;
    assert!(free >= need, "precondition: the bytes are there in total");
    assert!(og.fragmented_for(need), "precondition: no single hole fits");

    let walk = og.walk_objects();
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins(&walk, seq, vec![pin as usize], (0, 0), &HashMap::new())
        .expect("compacts");
    let b_new = done.pointer_map[&(b as usize)];
    assert_eq!(
        b_new,
        pin as usize + pc_size(1),
        "b slides onto the pin's end"
    );
    assert_eq!(
        // SAFETY: element 0 of `b` at its new address.
        unsafe { std::ptr::read((b_new + ARRAY_DATA_OFFSET) as *const i32) },
        0x5EED_1234,
        "b's payload moved with it",
    );
    assert!(done.largest_free_after >= need);
    assert!(!og.fragmented_for(need));
    // Object starts are coherent: the anchored card walk finds each
    // survivor at its new address.
    let base = og.base_ptr() as usize;
    let b_off = b_new - base;
    let hits = og.walk_card_ranges(&[(0, 8), (b_off, b_off + 8)], false);
    let found: Vec<usize> = hits.iter().map(|h| h.ptr as usize).collect();
    assert_eq!(found, vec![pin as usize, b_new]);
    assert!(og.alloc(need, 8).is_some(), "the request fits now");
}

/// A refusal moves nothing: an escaping reference (a live object naming a
/// FREED block) leaves the free list, the accounting and every address as
/// they were, and the marks set for the in-place sweep to use.
#[test]
fn a_pinned_compaction_refuses_an_escape_and_changes_nothing() {
    let mut og = OldGen::new(4096);
    let a = pc_obj(&mut og, 1, true);
    let gone = pc_obj(&mut og, 1, false);
    let c = pc_obj(&mut og, 1, true);
    pc_set_ref(a, 0, gone as usize);
    // SAFETY: `gone` is exactly one allocation of this size.
    unsafe { og.free(gone, pc_size(1)) };
    let walk = og.walk_objects();
    let seq = og.free_list_seq();
    let used = og.used();
    let r = og.compact_around_pins(&walk, seq, Vec::new(), (0, 0), &HashMap::new());
    assert_eq!(r.err(), Some(PinnedCompactRefusal::Escape));
    assert_eq!(og.free_list_seq(), seq, "the free list is untouched");
    assert_eq!(og.used(), used);
    assert_eq!(og.walk_objects(), walk);
    // SAFETY: `c` is a live walked object.
    assert_ne!(
        unsafe { &*(c as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED,
        0
    );
    assert_eq!(og.sizing_stats().pinned_compactions, 0);
    // A stale grid is refused too.
    let (mut og2, _a2, b2, _c2) = marked_dead_marked();
    let stale = og2.walk_objects();
    let seq2 = og2.free_list_seq();
    // SAFETY: `b2` is exactly one allocation of this size.
    unsafe { og2.free(b2, HEADER_SIZE + SLOT_SIZE) };
    assert_eq!(
        og2.compact_around_pins(&stale, seq2, Vec::new(), (0, 0), &HashMap::new())
            .err(),
        Some(PinnedCompactRefusal::StaleGrid),
    );
}

/// With no pins the pinned compaction IS the sliding compaction: the same
/// destinations and the same accounting as `compact`.
#[test]
fn with_no_pins_the_pinned_compaction_matches_compact() {
    let (mut og1, _a1, _b1, c1) = marked_dead_marked();
    let map1 = og1.compact();
    let (mut og2, _a2, _b2, c2) = marked_dead_marked();
    let walk = og2.walk_objects();
    let seq = og2.free_list_seq();
    let done = og2
        .compact_around_pins(&walk, seq, Vec::new(), (0, 0), &HashMap::new())
        .expect("compacts");
    let rel = |base: *const u8, q: usize| q - base as usize;
    assert_eq!(
        rel(og1.base_ptr(), map1[&(c1 as usize)]),
        rel(og2.base_ptr(), done.pointer_map[&(c2 as usize)]),
    );
    assert_eq!(og1.used(), og2.used());
    assert_eq!(og1.free_block_count(), og2.free_block_count());
    assert_eq!(og1.high_water(), og2.high_water());
}

/// gcd d9/a: the pinned compaction says which entries of its `watch` list
/// it DROPPED (the true-root major's dead promotion destinations: after
/// the slide a moved object may sit at a dropped address, so the caller
/// cannot ask the free list). A live watched object is not reported, and
/// without a list nothing is.
#[test]
fn gcd_d9a_a_pinned_compaction_reports_the_watched_objects_it_drops() {
    let (mut og, a, b, c) = marked_dead_marked();
    let walk = og.walk_objects();
    let seq = og.free_list_seq();
    let mut watch = vec![a as usize, b as usize, c as usize];
    watch.sort_unstable();
    let done = og
        .compact_around_pins_watching(
            &walk,
            seq,
            Vec::new(),
            (0, 0),
            &HashMap::new(),
            None,
            &watch,
        )
        .expect("compacts");
    assert_eq!(
        done.watch_dropped,
        vec![b as usize],
        "only the dead watched object"
    );
    assert!(
        done.pointer_map.contains_key(&(c as usize)),
        "precondition: c slid onto the dropped b's address",
    );

    let (mut og2, _a2, _b2, _c2) = marked_dead_marked();
    let walk2 = og2.walk_objects();
    let seq2 = og2.free_list_seq();
    let done2 = og2
        .compact_around_pins(&walk2, seq2, Vec::new(), (0, 0), &HashMap::new())
        .expect("compacts");
    assert!(done2.watch_dropped.is_empty(), "no list, no report");
}

/// gcd d9/a: the true-root fallbacks are counted per reason, into the
/// total, and every reason has its own slot and label.
#[test]
fn gcd_d9a_true_root_fallbacks_are_counted_per_reason() {
    let mut og = OldGen::new(64 * 1024);
    assert_eq!(og.note_true_root_fallback(TrueRootFallback::Promoted), 1);
    assert_eq!(og.note_true_root_fallback(TrueRootFallback::WalkGap), 2);
    assert_eq!(og.note_true_root_fallback(TrueRootFallback::Promoted), 3);
    let s = og.sizing_stats();
    assert_eq!(s.true_root_fallbacks, 3);
    assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::Promoted), 2);
    assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::WalkGap), 1);
    assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::PinnedPlan), 0);
    // gci i4/og (`gch-h3mj-...-review-findings` item 5): the compacting
    // decline has its own slot and label.
    assert_eq!(og.note_true_root_fallback(TrueRootFallback::Compacting), 4);
    let s = og.sizing_stats();
    assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::Compacting), 1);
    assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::PinnedPlan), 0);
    assert_eq!(TrueRootFallback::Compacting.label(), "compacting");
    og.note_true_root_promotions(5, 2);
    let s = og.sizing_stats();
    assert_eq!(
        (s.true_root_promotions, s.true_root_promotions_dead),
        (5, 2)
    );
    let mut labels: Vec<&str> = TrueRootFallback::ALL.iter().map(|r| r.label()).collect();
    let idx: Vec<usize> = TrueRootFallback::ALL.iter().map(|r| r.index()).collect();
    assert_eq!(
        idx,
        (0..TrueRootFallback::COUNT).collect::<Vec<_>>(),
        "slots in order"
    );
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), TrueRootFallback::COUNT, "labels are unique");
}

// --- gen r4w6/oldpin6: compaction edges ---------------------------------

fn pc_int_at(arr: usize, i: usize) -> i32 {
    // SAFETY: callers pass a live `int[]` base and an in-bounds index.
    unsafe { std::ptr::read((arr + ARRAY_DATA_OFFSET + 4 * i) as *const i32) }
}

fn pc_set_int(arr: *mut u8, i: usize, v: i32) {
    // SAFETY: callers pass a live `int[]` base and an in-bounds index.
    unsafe { std::ptr::write(arr.add(ARRAY_DATA_OFFSET + 4 * i) as *mut i32, v) };
}

/// A COMPLETELY full generation whose last object — pinned — ends exactly
/// at the end of the reservation: the pin keeps its address and payload,
/// the live objects below it slide to the bottom, the dead bytes become
/// ONE gap in front of the pin, there is no tail block (the cursor ends at
/// the reservation's end, so nothing is zeroed or given back past it), and
/// the gap is allocatable.
#[test]
fn a_pinned_object_at_the_very_end_of_a_full_generation_stays_put() {
    let mut og = OldGen::new(64 * 1024);
    let cap = og.capacity();
    let base = og.base_ptr() as usize;
    let mut arrays = Vec::new();
    for i in 0..6 {
        let a = pc_int_array(&mut og, 1024, i % 2 == 1);
        pc_set_int(a, 0, 100 + i);
        arrays.push(a);
    }
    let size = ARRAY_DATA_OFFSET + 1024 * 4;
    let rem = cap - og.used();
    assert!(
        rem > ARRAY_DATA_OFFSET && (rem - ARRAY_DATA_OFFSET) % 8 == 0,
        "rem {rem}"
    );
    let last_len = u32::try_from((rem - ARRAY_DATA_OFFSET) / 4).expect("small");
    let last = pc_int_array(&mut og, last_len, true);
    pc_set_int(last, 0, 0x1A57);
    pc_set_int(last, last_len as usize - 1, 0x0E7D);
    assert_eq!(og.used(), cap, "precondition: completely full");
    assert_eq!(
        last as usize + rem,
        base + cap,
        "precondition: the pin ends the reservation"
    );

    let walk = og.walk_objects();
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins(&walk, seq, vec![last as usize], (0, 0), &HashMap::new())
        .expect("a full generation compacts around its last object");
    assert_eq!((done.pinned, done.moved), (1, 3));
    assert!(
        !done.pointer_map.contains_key(&(last as usize)),
        "the pin does not move"
    );
    for (k, &i) in [1usize, 3, 5].iter().enumerate() {
        let new = done.pointer_map[&(arrays[i] as usize)];
        assert_eq!(new, base + k * size, "live array {i} slides to slot {k}");
        assert_eq!(
            pc_int_at(new, 0),
            100 + i as i32,
            "its payload moved with it"
        );
    }
    assert_eq!(
        pc_int_at(last as usize, 0),
        0x1A57,
        "the pin's payload is untouched"
    );
    assert_eq!(pc_int_at(last as usize, last_len as usize - 1), 0x0E7D);
    // gen r4w6 (review6 finding 4): arrays 3 and 5 moved out of what is
    // now the gap in front of the pin; their old images read zero, header
    // and payload, not a forwarded header and the old ints.
    for i in [3usize, 5] {
        let old = arrays[i] as usize;
        // SAFETY: `old` is inside the committed gap `[base + 3 * size,
        // last)`; the first word is the old header's mark word.
        let mark = unsafe { std::ptr::read(old as *const u64) };
        assert_eq!(mark, 0, "array {i}'s old header is zeroed");
        assert_eq!(pc_int_at(old, 0), 0, "array {i}'s old payload is zeroed");
    }
    assert_eq!(og.used(), 3 * size + rem);
    assert_eq!(
        og.free_block_count(),
        1,
        "one gap in front of the pin, no tail block"
    );
    assert_eq!(og.largest_free_block(), 3 * size);
    assert_eq!(
        og.high_water(),
        cap,
        "the last survivor ends at the reservation's end"
    );
    assert_eq!(
        og.alloc(3 * size, 8),
        Some((base + 3 * size) as *mut u8),
        "the gap is reusable"
    );
    assert_eq!(og.used(), cap, "full again");
}

/// A pinned HUMONGOUS object (a 64 KiB `int[]` in a 256 KiB generation,
/// dead objects on both sides): it keeps its address and every byte of its
/// payload; the survivor below it slides to the bottom, the survivors
/// above it slide down onto its end, and the free list is the gap in front
/// of it plus the tail.
#[test]
fn a_pinned_humongous_object_keeps_its_address_and_its_neighbours_slide() {
    let mut og = OldGen::new(256 * 1024);
    let base = og.base_ptr() as usize;
    let _d0 = pc_obj(&mut og, 4, false);
    let m0 = pc_obj(&mut og, 2, true);
    let _d1 = pc_int_array(&mut og, 1024, false);
    let hum_len = 16 * 1024u32;
    let hum = pc_int_array(&mut og, hum_len, true);
    pc_set_int(hum, 0, 0x4855);
    pc_set_int(hum, hum_len as usize - 1, 0x4D47);
    let _d2 = pc_int_array(&mut og, 512, false);
    let m1 = pc_obj(&mut og, 1, true);
    let m2 = pc_int_array(&mut og, 256, true);
    pc_set_int(m2, 255, 0x0202);
    pc_set_ref(m1, 0, hum as usize);
    pc_set_ref(m0, 0, m2 as usize);
    let hum_size = ARRAY_DATA_OFFSET + hum_len as usize * 4;

    let walk = og.walk_objects();
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins(&walk, seq, vec![hum as usize], (0, 0), &HashMap::new())
        .expect("compacts around a humongous pin");
    assert_eq!((done.pinned, done.moved), (1, 3));
    assert!(!done.pointer_map.contains_key(&(hum as usize)));
    let m0_new = done.pointer_map[&(m0 as usize)];
    let m1_new = done.pointer_map[&(m1 as usize)];
    let m2_new = done.pointer_map[&(m2 as usize)];
    assert_eq!(
        m0_new, base,
        "the survivor below the pin slides to the bottom"
    );
    assert_eq!(
        m1_new,
        hum as usize + hum_size,
        "the survivors above land on the pin's end"
    );
    assert_eq!(m2_new, m1_new + pc_size(1));
    assert_eq!(
        pc_int_at(hum as usize, 0),
        0x4855,
        "the humongous payload is untouched"
    );
    assert_eq!(pc_int_at(hum as usize, hum_len as usize - 1), 0x4D47);
    assert_eq!(pc_int_at(m2_new, 255), 0x0202);
    assert_eq!(
        pc_get_ref(m1_new, 0),
        hum as usize,
        "a reference to the pin is unchanged"
    );
    assert_eq!(
        pc_get_ref(m0_new, 0),
        m2_new,
        "a reference to a moved object is forwarded"
    );
    assert_eq!(
        og.free_block_count(),
        2,
        "the gap in front of the pin, and the tail"
    );
    let (free, _) = og.free_bytes_and_largest();
    assert_eq!(free + og.used(), og.capacity());
    assert_eq!(og.high_water(), m2_new + ARRAY_DATA_OFFSET + 256 * 4 - base);
    let walked: Vec<usize> = og.walk_objects().iter().map(|&(q, _)| q as usize).collect();
    assert_eq!(walked, vec![m0_new, hum as usize, m1_new, m2_new]);
}

/// gen r5w1/oldgen5 — a refused humongous request arms ONE compaction
/// request whatever the free list holds right now (the collection it
/// provokes is what frees the dead neighbours), a request larger than the
/// generation arms nothing, and taking the request clears the humongous
/// mark with it; a compaction answers both.
#[test]
fn a_humongous_refusal_arms_a_compaction_request_until_it_is_taken() {
    let mut og = OldGen::new(64 * 1024);
    assert!(!og.humongous_refusal_pending());
    assert!(
        !og.note_humongous_refusal(64 * 1024 + 8),
        "larger than the generation"
    );
    assert!(!og.take_fragmentation_compaction_request());
    // A FULL generation: `note_refused_request` records no fragmentation,
    // the humongous refusal still arms.
    let (mut full, _) = old_gen_with_objects(1024, 3);
    assert!(full.alloc(32 * 1024, 8).is_none());
    assert_eq!(full.fragmentation_pending_request(), 0);
    assert!(full.note_humongous_refusal(32 * 1024));
    assert!(full.humongous_refusal_pending());
    let s = full.sizing_stats();
    assert_eq!(
        (
            s.humongous_compaction_requests,
            s.fragmentation_compactions_requested
        ),
        (1, 1)
    );
    assert!(full.take_fragmentation_compaction_request());
    assert!(!full.humongous_refusal_pending(), "taken with the request");
    assert!(!full.take_fragmentation_compaction_request(), "once");
    // A completed compaction answers a pending one.
    assert!(og.note_humongous_refusal(1024));
    let _ = og.compact();
    assert!(!og.humongous_refusal_pending());
    assert!(!og.take_fragmentation_compaction_request());
}

/// gen r5w2/alloc6 — the refused request's size rides the request: the
/// largest of several refusals, 0 once the request is taken or answered,
/// and 0 while no humongous refusal is pending (a fragmentation request
/// armed another way does not report one).
#[test]
fn the_refused_humongous_size_rides_the_request_and_leaves_with_it() {
    let mut og = OldGen::new(64 * 1024);
    assert_eq!(og.humongous_refused_request(), 0);
    og.request_fragmentation_compaction();
    assert_eq!(og.humongous_refused_request(), 0, "not a humongous request");
    assert!(og.take_fragmentation_compaction_request());
    assert!(og.note_humongous_refusal(8 * 1024));
    assert!(og.note_humongous_refusal(24 * 1024));
    assert!(og.note_humongous_refusal(16 * 1024));
    assert_eq!(
        og.humongous_refused_request(),
        24 * 1024,
        "the largest refusal"
    );
    assert!(og.take_fragmentation_compaction_request());
    assert_eq!(og.humongous_refused_request(), 0, "taken with the request");
    assert!(og.note_humongous_refusal(4 * 1024));
    let _ = og.compact();
    assert_eq!(
        og.humongous_refused_request(),
        0,
        "answered by a compaction"
    );
    // A new refusal after the answer starts from its own size.
    assert!(og.note_humongous_refusal(2 * 1024));
    assert_eq!(og.humongous_refused_request(), 2 * 1024);
}

// ---- gcj j3/og3 (2026-10-19) ------------------------------------------

/// gcj j3/og3 — a random graph whose MARKED set is closed under its edges
/// (a marked object references marked objects only; a dead one anything),
/// i.e. exactly what a gap-free mark leaves for Phase 0.
fn gcj_j3og3_closed_graph(
    seed: u64,
    n: usize,
) -> (Vec<[Option<usize>; GRAPH_SLOTS]>, Vec<bool>) {
    let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut marked = vec![false; n];
    for m in marked.iter_mut() {
        *m = rng.below(3) != 0;
    }
    let live: Vec<usize> = (0..n).filter(|&i| marked[i]).collect();
    let mut edges = vec![[None; GRAPH_SLOTS]; n];
    for (i, row) in edges.iter_mut().enumerate() {
        for slot in row.iter_mut() {
            if rng.below(4) == 0 {
                continue;
            }
            *slot = if marked[i] {
                Some(live[rng.below(live.len())])
            } else {
                Some(rng.below(n))
            };
        }
    }
    (edges, marked)
}

/// gcj j3/og3 — the base-independent snapshot a slide leaves, plus the
/// generation's Phase 0 skip and disagreement counts.
type GcjJ3og3Snapshot = (
    Vec<(usize, usize)>,
    Vec<(usize, Vec<Option<usize>>)>,
    usize,
    u64,
    u64,
);

/// gcj j3/og3 — the survivors' slots of `og`, as offsets from `base`.
fn gcj_j3og3_heap(og: &OldGen, base: usize) -> Vec<(usize, Vec<Option<usize>>)> {
    let mut heap: Vec<(usize, Vec<Option<usize>>)> = Vec::new();
    for (p, _size) in og.walk_objects() {
        let slots: Vec<Option<usize>> = (0..GRAPH_SLOTS)
            .map(|s| {
                // SAFETY: a survivor base of this graph's layout, slot `s`
                // inside its body.
                let slot = unsafe { p.add(HEADER_SIZE + s * SLOT_SIZE) };
                // SAFETY: as above; the slot holds a `Value`.
                match unsafe { std::ptr::read(slot as *const Value) } {
                    Value::Object(Some(r)) => Some(r.as_ptr() as usize - base),
                    _ => None,
                }
            })
            .collect();
        heap.push((p as usize - base, slots));
    }
    heap
}

/// gcj j3/og3 — compact `edges` / `marked` through the slide with the
/// Phase 0 verdict `trusted` (and the verify mode `verify`).
fn gcj_j3og3_slide_snapshot(
    edges: &[[Option<usize>; GRAPH_SLOTS]],
    marked: &[bool],
    trusted: Option<bool>,
    verify: bool,
) -> GcjJ3og3Snapshot {
    let (mut og, _ptrs) = graph_old_gen(edges, marked);
    og.set_trust_mark_verify_for_test(verify);
    let base = og.base_ptr() as usize;
    let objects = og.walk_objects();
    let seq = og.free_list_seq();
    let flags = HashMap::new();
    let map = og.compact_with_walked_objects_trusting(objects, seq, &flags, None, trusted);
    let mut moved: Vec<(usize, usize)> =
        map.iter().map(|(&s, &d)| (s - base, d - base)).collect();
    moved.sort_unstable();
    let heap = gcj_j3og3_heap(&og, base);
    let s = og.sizing_stats();
    (
        moved,
        heap,
        og.used(),
        s.phase0_skips,
        s.trust_mark_disagreements,
    )
}

/// gcj j3/og3 (Phase 0 drop) — over a CLOSED mark, the slide handed
/// `Some(false)` skips Phase 0 (counted) and leaves exactly the heap the
/// Phase 0 arm leaves: same map, same survivors, same slots, same `used`.
/// The verify mode runs Phase 0 anyway and finds nothing to disagree with.
#[test]
fn gcj_j3og3_a_trusted_closed_mark_skips_phase0_and_compacts_the_same_heap() {
    for seed in 1..=16u64 {
        let (edges, marked) = gcj_j3og3_closed_graph(seed, 400);
        let plain = gcj_j3og3_slide_snapshot(&edges, &marked, None, false);
        let trusted = gcj_j3og3_slide_snapshot(&edges, &marked, Some(false), false);
        let verified = gcj_j3og3_slide_snapshot(&edges, &marked, Some(false), true);
        assert_eq!(plain.3, 0, "seed {seed}: no verdict, no skip");
        assert_eq!(trusted.3, 1, "seed {seed}: the verdict skipped Phase 0");
        assert_eq!(verified.3, 0, "seed {seed}: the verify mode runs Phase 0");
        assert_eq!(verified.4, 0, "seed {seed}: a closed mark has no disagreement");
        assert_eq!(plain.0, trusted.0, "seed {seed}: pointer maps differ");
        assert_eq!(plain.1, trusted.1, "seed {seed}: compacted heaps differ");
        assert_eq!(plain.2, trusted.2, "seed {seed}: used bytes differ");
        assert_eq!(plain.1, verified.1, "seed {seed}: the verify arm's heap differs");
        assert!(!plain.0.is_empty(), "seed {seed}: the shape moved nothing");
    }
}

/// gcj j3/og3 — the positive control of the verify mode: a marked object
/// that references an UNMARKED one (a mark push-site gap) handed
/// `Some(false)` is counted as a disagreement, and Phase 0's own answer is
/// used, so the rescued objects survive exactly as on the plain arm.
#[test]
fn gcj_j3og3_verify_counts_a_gap_and_keeps_phase0s_rescue() {
    // 0 (marked) -> 1 (unmarked: the gap) -> 2 (unmarked); 3 dead.
    let edges = vec![
        [Some(1), None, None],
        [Some(2), None, None],
        [None, None, None],
        [Some(0), None, None],
    ];
    let marked = vec![true, false, false, false];
    let plain = gcj_j3og3_slide_snapshot(&edges, &marked, None, false);
    let verified = gcj_j3og3_slide_snapshot(&edges, &marked, Some(false), true);
    assert_eq!(verified.4, 1, "the gap is a disagreement");
    assert_eq!(verified.3, 0, "and Phase 0 ran");
    assert_eq!(plain.1, verified.1, "Phase 0's rescue kept 1 and 2 on both arms");
    assert_eq!(plain.1.len(), 3, "0, 1 and 2 survive; 3 is dead");
}

/// gcj j3/og3 — `Some(true)` (the proof found an escape) abandons the slide
/// exactly as Phase 0's escape arm does: nothing moves, `used` is
/// unchanged, and no mark survives the call.
#[test]
fn gcj_j3og3_a_trusted_escape_abandons_the_slide() {
    let (edges, marked) = gcj_j3og3_closed_graph(7, 200);
    let (mut og, ptrs) = graph_old_gen(&edges, &marked);
    let used = og.used();
    let objects = og.walk_objects();
    let seq = og.free_list_seq();
    let flags = HashMap::new();
    let map = og.compact_with_walked_objects_trusting(objects, seq, &flags, None, Some(true));
    assert!(map.is_empty(), "an abandoned slide moves nothing");
    assert_eq!(og.used(), used);
    assert!(ptrs.iter().all(|&p| !is_marked(p)), "the escape arm clears every mark");
    assert_eq!(og.sizing_stats().phase0_skips, 1);
}

/// gcj j3/og3 (`CRATONVM_GC_OLD_PINNED_COMPACT_REUSE_CLOSE`) — the pinned
/// compactor takes the caller's verdict only when Phase 0a marked no pin:
/// a pin that was already marked keeps the skip; an UNMARKED pin (marked
/// by Phase 0a, so untraced) drops it, and Phase 0b runs and keeps what
/// the pin references.
#[test]
fn gcj_j3og3_the_pinned_compactor_drops_the_verdict_when_it_marks_a_pin() {
    // 0 marked; 1 unmarked, references 2 (unmarked); 3 marked.
    let edges = vec![
        [None, None, None],
        [Some(2), None, None],
        [None, None, None],
        [Some(0), None, None],
    ];
    let marked = vec![true, false, false, true];
    let flags = HashMap::new();
    // A marked pin: the verdict stands.
    let (mut og, ptrs) = graph_old_gen(&edges, &marked);
    let objects = og.walk_objects();
    let seq = og.free_list_seq();
    let pins = vec![ptrs[3] as usize];
    let done = og
        .compact_around_pins_trusting(
            &objects,
            seq,
            pins,
            (0, 0),
            &flags,
            None,
            &[],
            Some(false),
            None,
            None,
        )
        .expect("no escape");
    assert_eq!(og.sizing_stats().phase0_skips, 1, "a marked pin keeps the skip");
    assert_eq!(done.pinned, 1);
    // An unmarked pin: Phase 0b runs and keeps its referent 2.
    let (mut og, ptrs) = graph_old_gen(&edges, &marked);
    let objects = og.walk_objects();
    let seq = og.free_list_seq();
    let pins = vec![ptrs[1] as usize];
    let done = og
        .compact_around_pins_trusting(
            &objects,
            seq,
            pins,
            (0, 0),
            &flags,
            None,
            &[],
            Some(false),
            None,
            None,
        )
        .expect("no escape");
    assert_eq!(og.sizing_stats().phase0_skips, 0, "a newly marked pin drops the verdict");
    assert_eq!(og.walk_objects().len(), 4, "0, the pin 1, its referent 2 and 3 survive");
    assert_eq!(done.pinned, 1);
}

/// gcj og3 P2 — the window chooser: the cheapest pin-free run of
/// survivors whose gaps reach the target, "not needed" when one gap
/// already holds the request, and the whole slide when pins cut every
/// candidate short.
#[test]
fn gcj_og3_p2_the_slide_window_is_the_cheapest_pin_free_run() {
    // Survivors of 100 bytes with gaps of 10 in front of each, except a
    // cheap stretch of 20-byte survivors from offset 1000.
    let mut s = Vec::new();
    let mut off = 0;
    for k in 0..20 {
        let size = if (9..14).contains(&k) { 20 } else { 100 };
        off += 10;
        s.push((off, size, false));
        off += size;
    }
    let cap = off + 10;
    // One gap holds 10: a request of 10 needs no slide.
    assert_eq!(fragmentation_slide_window(&s, cap, 10, 40), SlideWindow::NotNeeded);
    // Four gaps of 10 make 40: three survivors between them; the cheap
    // stretch (20-byte survivors 9..=13) wins.
    match fragmentation_slide_window(&s, cap, 11, 40) {
        SlideWindow::Window { lo, hi } => {
            let inside: Vec<_> = s.iter().filter(|o| o.0 >= lo && o.0 < hi).collect();
            assert_eq!(inside.len(), 3, "{inside:?}");
            assert!(inside.iter().all(|o| o.1 == 20), "{inside:?}");
        }
        w => panic!("expected a window, got {w:?}"),
    }
    // One survivor between two gaps holds the bare request; the cheapest
    // such window is a 20-byte survivor.
    let single = |w: SlideWindow, sv: &[(usize, usize, bool)]| match w {
        SlideWindow::Window { lo, hi } => {
            let inside: Vec<_> = sv.iter().filter(|o| o.0 >= lo && o.0 < hi).copied().collect();
            assert_eq!(inside.len(), 1, "{inside:?}");
            inside[0]
        }
        w => panic!("expected a window, got {w:?}"),
    };
    // A pin on every other survivor leaves no window of three, so the
    // bare request decides, on an unpinned survivor.
    let pinned: Vec<_> = s.iter().enumerate().map(|(k, &(o, z, _))| (o, z, k % 2 == 1)).collect();
    let o = single(fragmentation_slide_window(&pinned, cap, 11, 40), &pinned);
    assert_eq!((o.1, o.2), (20, false));
    // Not even all the gaps together reach the headroom: the factor-four
    // fallback still chooses the cheapest four-object window.
    match fragmentation_slide_window(&s, cap, 11, 10_000) {
        SlideWindow::Window { lo, hi } => {
            let inside: Vec<_> = s.iter().filter(|o| o.0 >= lo && o.0 < hi).collect();
            assert_eq!(inside.len(), 4, "{inside:?}");
            assert!(inside.iter().all(|o| o.1 == 20), "{inside:?}");
        }
        w => panic!("expected the factor-four fallback window, got {w:?}"),
    }
    // Every survivor pinned: no window at all.
    let all: Vec<_> = s.iter().map(|&(o, z, _)| (o, z, true)).collect();
    assert_eq!(fragmentation_slide_window(&all, cap, 11, 40), SlideWindow::Whole);
    assert_eq!(fragmentation_slide_window(&[], 64, 128, 512), SlideWindow::Whole);
    assert_eq!(fragmentation_slide_window(&[], 64, 64, 256), SlideWindow::NotNeeded);
}

/// The post-integration planner must retain the smaller useful target
/// when the requested gap is unavailable to any single pin-free window.
#[test]
fn gcj_og3_p2_the_slide_window_falls_back_to_an_intermediate_gap() {
    let mut survivors = Vec::new();
    let mut offset = 0;
    for _ in 0..5 {
        offset += 10;
        survivors.push((offset, 20, false));
        offset += 20;
    }
    let capacity = offset + 10;
    // Five ten-byte gaps cannot meet the requested 70-byte target, but
    // they can meet the factor-four margin (44) without sliding whole.
    match fragmentation_slide_window(&survivors, capacity, 11, 70) {
        SlideWindow::Window { lo, hi } => {
            let inside: Vec<_> = survivors
                .iter()
                .filter(|o| o.0 >= lo && o.0 < hi)
                .collect();
            assert_eq!(inside.len(), 4, "the intermediate target selects four objects");
        }
        w => panic!("expected an intermediate window, got {w:?}"),
    }
}

/// A full 64 KiB graph generation whose every fourth object is dead and
/// whose live objects form one chain through slot 0; the tail is under two
/// objects. Returns the generation, the bases and the live count.
fn gcj_og3_p2_holey_chain() -> (OldGen, Vec<*mut u8>, usize) {
    let n = 64 * 1024 / GRAPH_OBJ - 1;
    let marked: Vec<bool> = (0..n).map(|k| k % 4 != 3).collect();
    let live: Vec<usize> = (0..n).filter(|&k| marked[k]).collect();
    let mut edges = vec![[None; GRAPH_SLOTS]; n];
    for w in live.windows(2) {
        edges[w[0]][0] = Some(w[1]);
        edges[w[1]][1] = Some(w[0]);
    }
    let (og, ptrs) = graph_old_gen(&edges, &marked);
    (og, ptrs, live.len())
}

/// The chain from the generation's lowest object through slot 0: its
/// length, and whether every link is a walked object base whose slot 1
/// points back.
fn gcj_og3_p2_chain_intact(og: &OldGen) -> (usize, bool) {
    let bases: std::collections::HashSet<usize> =
        og.walk_objects().iter().map(|&(p, _)| p as usize).collect();
    let slot = |p: usize, s: usize| -> usize {
        // SAFETY: `p` is a walked graph object; slot `s < GRAPH_SLOTS`.
        match unsafe { std::ptr::read((p + HEADER_SIZE + s * SLOT_SIZE) as *const Value) } {
            Value::Object(Some(r)) => r.as_ptr() as usize,
            _ => 0,
        }
    };
    let mut p = og.walk_objects()[0].0 as usize;
    let (mut len, mut ok) = (1, true);
    loop {
        let next = slot(p, 0);
        if next == 0 {
            return (len, ok);
        }
        ok &= bases.contains(&next) && slot(next, 1) == p;
        p = next;
        len += 1;
    }
}

/// gcj og3 P2 — a fragmentation-triggered compaction slides only its
/// window: the objects below it keep their addresses, far fewer than the
/// live set move, the block it makes holds `FRAGMENTATION_FIT_FACTOR`
/// times the request, and every reference (into, out of and across the
/// window) still resolves.
#[test]
fn gcj_og3_p2_a_windowed_slide_moves_only_the_window() {
    let (mut og, ptrs, live) = gcj_og3_p2_holey_chain();
    let objects = og.walk_objects();
    let seq = og.free_list_seq();
    let flags = HashMap::new();
    let request = 2 * GRAPH_OBJ;
    let done = og
        .compact_around_pins_trusting(
            &objects,
            seq,
            Vec::new(),
            (0, 0),
            &flags,
            None,
            &[],
            None,
            None,
            Some(request),
        )
        .expect("no escape");
    assert!(done.moved > 0, "the window moved something");
    assert!(done.moved < live / 4, "only a window moved: {} of {live}", done.moved);
    assert!(done.largest_free_after >= FRAGMENTATION_FIT_FACTOR * request);
    assert_eq!(og.used(), live * GRAPH_OBJ);
    assert_eq!(og.walk_objects()[0].0, ptrs[0], "the prefix stays");
    assert_eq!(gcj_og3_p2_chain_intact(&og), (live, true));
    assert_eq!(og.sizing_stats().frag_window_compactions, 1);
    assert!(og.walk_objects().iter().all(|&(p, _)| !is_marked(p)));
}

/// gcj og3 P2 — when the mark shows a run of dead objects that holds the
/// request, the compaction declines with `NotNeeded` and moves nothing;
/// without a goal the same generation slides whole, as before.
#[test]
fn gcj_og3_p2_a_dead_run_that_holds_the_request_needs_no_slide() {
    let (mut og, ptrs, live) = gcj_og3_p2_holey_chain();
    // Every dead object is a run of one object's bytes: a request of one
    // object fits the hole the in-place sweep leaves.
    let objects = og.walk_objects();
    let seq = og.free_list_seq();
    let flags = HashMap::new();
    let r = og.compact_around_pins_trusting(
        &objects,
        seq,
        Vec::new(),
        (0, 0),
        &flags,
        None,
        &[],
        None,
        None,
        Some(GRAPH_OBJ),
    );
    assert_eq!(r.err(), Some(PinnedCompactRefusal::NotNeeded));
    assert_eq!(og.used(), (64 * 1024 / GRAPH_OBJ - 1) * GRAPH_OBJ, "nothing freed");
    assert_eq!(og.sizing_stats().frag_window_not_needed, 1);
    assert_eq!(og.walk_objects()[1].0, ptrs[1], "nothing moved");
    // No goal: the whole slide, which moves everything above the first hole.
    let objects = og.walk_objects();
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins_trusting(
            &objects,
            seq,
            Vec::new(),
            (0, 0),
            &flags,
            None,
            &[],
            None,
            None,
            None,
        )
        .expect("no escape");
    assert!(done.moved > live / 2);
    assert_eq!(og.sizing_stats().frag_window_compactions, 0);
    assert_eq!(gcj_og3_p2_chain_intact(&og), (live, true));
}

/// gcj j3/og3 — HotSpot's `AdaptivePaddedAverage(10, 3)`: the first sample
/// is the average with no deviation; the second is weighted 1/2 (`100 /
/// count`), and the deviation is taken from the NEW average.
#[test]
fn gcj_j3og3_the_padded_average_follows_hotspots_weights() {
    let mut a = PaddedAverage::default();
    assert_eq!(a.padded(), 0);
    a.sample(1000);
    assert_eq!((a.avg, a.dev, a.padded()), (1000, 0, 1000));
    a.sample(3000);
    // avg = 1000 + 2000 * 50 / 100 = 2000; dev = 0 + |3000 - 2000| / 2.
    assert_eq!((a.avg, a.dev), (2000, 500));
    assert_eq!(a.padded(), 2000 + 3 * 500);
    // Past 10 samples the weight is AdaptiveSizePolicyWeight (10 %).
    for _ in 0..20 {
        a.sample(2000);
    }
    assert_eq!(a.count(), 22);
    let before = a.avg;
    a.sample(12_000);
    assert_eq!(a.avg, before + (12_000 - before) * 10 / 100);
    assert!(a.padded() > a.avg);
}

/// gcj j3/og3 — the Serial-shaped verdict as a table.
#[test]
fn gcj_j3og3_the_serial_verdict_runs_when_the_padded_promotion_does_not_fit() {
    let v = OldGen::serial_trigger_verdict;
    let cap = 1000;
    assert_eq!(v(0, 0, None, 0, 10, 0), MajorTrigger::NotDue, "no generation");
    assert_eq!(v(cap, 100, None, 0, 10, 1), MajorTrigger::AllocationFailure);
    assert_eq!(v(cap, 950, None, 0, 10, 0), MajorTrigger::NotDue, "95 % and it fits");
    assert_eq!(v(cap, 950, None, 0, 51, 0), MajorTrigger::Occupancy, "50 free < 51");
    assert_eq!(v(cap, 100, None, 0, 901, 0), MajorTrigger::Occupancy);
    // The ceiling: a generation that cannot grow, at or above 75 %, whose
    // largest block cannot hold the recent request although enough is free.
    assert_eq!(v(cap, 800, Some(10), 20, 0, 0), MajorTrigger::Occupancy);
    assert_eq!(v(cap, 800, Some(30), 20, 0, 0), MajorTrigger::NotDue, "the request fits");
    assert_eq!(v(cap, 700, Some(10), 20, 0, 0), MajorTrigger::NotDue, "below the floor");
}

/// gcj j3/og3 — the switch's integration in `major_trigger`: one promotion
/// sample per counted decision, `serial_trigger_due` when it runs, and
/// `serial_trigger_over_floor_skips` for a decision at or above 75 % it
/// does not run (one the 75 % trigger would have run). Off, the bare
/// occupancy test as before.
#[test]
fn gcj_j3og3_the_serial_trigger_samples_promotions_and_counts_its_verdicts() {
    let cap = 1 << 20;
    let mut og = OldGen::new(cap);
    og.set_serial_trigger_for_test(true);
    let _a = og.alloc(800 * 1024, 8).expect("800 KiB");
    // No baseline yet: no sample, padded 0, 78 % used: not due, and
    // counted as a decision the 75 % floor would have run.
    assert_eq!(og.major_trigger(false, false), MajorTrigger::NotDue);
    assert_eq!(og.sizing_stats().serial_trigger_over_floor_skips, 1);
    assert_eq!(og.serial_trigger_padded_promotion(), 0);
    // 200 KiB promoted since: the padded average (200 KiB) no longer fits
    // what is left.
    let _b = og.alloc(200 * 1024, 8).expect("200 KiB");
    assert_eq!(og.serial_trigger_padded_promotion(), 0, "sampled at a decision only");
    assert_eq!(og.major_trigger(false, false), MajorTrigger::Occupancy);
    assert_eq!(og.serial_trigger_padded_promotion(), 200 * 1024);
    assert_eq!(og.sizing_stats().serial_trigger_due, 1);
    // A request still collects.
    assert_eq!(og.major_trigger(true, false), MajorTrigger::Requested);
    // Off: the 75 % floor.
    let mut off = OldGen::new(cap);
    let _c = off.alloc(800 * 1024, 8).expect("800 KiB");
    assert!(off.major_trigger(false, false).runs());
    assert_eq!(off.sizing_stats().serial_trigger_over_floor_skips, 0);
}

/// gcj j3/og3 (B2) — the capacity target as a pure function: the largest
/// of the construction capacity, the committed end and the
/// `MinHeapFreeRatio` capacity, granule-rounded, never above the capacity.
#[test]
fn gcj_j3og3_the_ungrow_target_never_cuts_below_committed_or_the_free_ratio() {
    let g = GRANULE;
    let t = OldGen::ungrow_target;
    assert_eq!(t(g, 40, 8 * g, g, 11 * g), 8 * g, "the construction capacity");
    assert_eq!(t(6 * g, 40, 8 * g, 7 * g, 11 * g), 10 * g, "6 / 0.6 = 10 granules");
    assert_eq!(t(g, 40, 8 * g, 9 * g + 1, 11 * g), 10 * g, "the committed end, rounded");
    assert_eq!(t(9 * g, 40, 8 * g, 9 * g, 11 * g), 11 * g, "15 granules, capped");
    assert_eq!(t(g, 100, 8 * g, g, 11 * g), 11 * g, "MinHeapFreeRatio=100: no shrink");
}

/// gcj j3/og3 (B2, `CRATONVM_GC_OLD_CAPACITY_SHRINK`) — a capacity that grew
/// (the free-ratio growth) comes back to the construction capacity once
/// the live set left and the committed resize released the tail: not at
/// the growth's collection nor the next (damping), only when armed; the
/// base, the growth limit (B3's fixed pool max) and the survivor stay, and
/// the generation can grow again afterwards.
#[test]
fn gcj_j3og3_a_grown_capacity_shrinks_back_once_the_tail_is_released() {
    let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
    if og.commit_bits().is_none() {
        return;
    }
    og.set_growth_soft_max(11 * GRANULE);
    og.set_free_ratio_growth_for_test(true);
    og.set_capacity_shrink_for_test(true);
    let base = og.base_ptr();
    let keep = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor");
    let big = og.alloc(7 * GRANULE, 8).expect("seven granules");
    assert_eq!(og.grow_to_min_free_ratio(), 3 * GRANULE);
    assert_eq!(og.capacity(), 11 * GRANULE);
    // The live set leaves.
    // SAFETY: exactly the `alloc(7 * GRANULE, 8)` block above.
    unsafe { og.free(big, 7 * GRANULE) };
    og.coalesce_free_blocks();
    // Damping: the growth's collection (0) and the next one.
    assert_eq!(og.ungrow_after_collection(), 0, "the collection that grew");
    og.note_collection_end(og.used());
    assert_eq!(og.ungrow_after_collection(), 0, "one collection after the growth");
    og.note_collection_end(og.used());
    // The committed resize releases the tail (its first call is damped).
    for _ in 0..3 {
        let _ = og.resize_after_collection(true);
    }
    assert!(og.committed_bytes() <= 8 * GRANULE, "the tail went back first");
    og.set_capacity_shrink_for_test(false);
    assert_eq!(og.ungrow_after_collection(), 0, "off: nothing");
    og.set_capacity_shrink_for_test(true);
    let epoch = og.reclaim_epoch();
    assert_eq!(og.ungrow_after_collection(), 3 * GRANULE);
    assert_eq!(og.capacity(), 8 * GRANULE, "back to the construction capacity");
    assert_ne!(og.reclaim_epoch(), epoch, "an open concurrent cycle must abandon");
    assert_eq!(og.base_ptr(), base, "the base never moves");
    assert!(og.contains(keep), "the survivor stays readable");
    assert_eq!(og.growth_limit(), 11 * GRANULE, "the fixed pool max does not move");
    assert_eq!(og.used() + og.free_bytes_and_largest().0, og.capacity());
    let s = og.sizing_stats();
    assert_eq!((s.capacity_shrinks, s.capacity_shrunk_bytes), (1, (3 * GRANULE) as u64));
    // At the construction capacity: nothing more.
    og.note_collection_end(og.used());
    og.note_collection_end(og.used());
    assert_eq!(og.ungrow_after_collection(), 0);
    // And it grows again on demand.
    let q = og.alloc(7 * GRANULE, 8).expect("the construction capacity holds it");
    assert_eq!(og.grow_to_min_free_ratio(), 3 * GRANULE);
    // SAFETY: the last byte of the allocation; faults if the commit is short.
    unsafe { std::ptr::write_volatile(q.add(7 * GRANULE - 1), 0x5A) };
}

/// gcj j3/og3 (O5) — a coalesce that merged something installs the sorted
/// view itself; it must be exactly the view a rebuild from the buckets
/// gives (same blocks, same order), and the walks over it must agree.
#[test]
fn gcj_j3og3_a_merging_coalesce_installs_the_view_a_rebuild_gives() {
    let (mut og, offsets) = old_gen_with_objects(48, 2);
    let base = og.base_ptr() as usize;
    let size = HEADER_SIZE + 2 * SLOT_SIZE;
    // Free runs of two and three neighbours, so the merge has work.
    for (i, &off) in offsets.iter().enumerate() {
        if i % 5 == 1 || i % 5 == 2 || i % 5 == 3 {
            // SAFETY: each is exactly one `alloc(size, 8)` block above.
            unsafe { og.free((base + off) as *mut u8, size) };
        }
    }
    assert!(og.coalesce_free_blocks() > 0, "the shape merges");
    let installed: Vec<(usize, usize)> =
        og.with_sorted_free_blocks(|v| v.iter().map(|b| (b.offset, b.size)).collect());
    let walked: Vec<usize> = og.walk_objects().iter().map(|&(p, _)| p as usize).collect();
    og.invalidate_sorted_free();
    let rebuilt: Vec<(usize, usize)> =
        og.with_sorted_free_blocks(|v| v.iter().map(|b| (b.offset, b.size)).collect());
    let rewalked: Vec<usize> = og.walk_objects().iter().map(|&(p, _)| p as usize).collect();
    assert_eq!(installed, rebuilt, "the installed view is the rebuilt one");
    assert_eq!(walked, rewalked);
    let kept = (0..48).filter(|i| !matches!(i % 5, 1..=3)).count();
    assert_eq!(walked.len(), kept, "the survivors, no more");
}
