/// **A high-end retraction must widen the bitmap bound, exactly as a low-end
/// one does.**
///
/// `live_bitmap_bounds` passes over `[0, low_high_water)` and
/// `[high_low_water, capacity)`. The low half has used a water mark since
/// the bounds landed; the high half used the RAW cursor, and
/// `retract_high_cursor_into_free_head` RAISES that cursor -- so the span
/// the retraction gave back fell outside the next clear and kept this
/// cycle's mark bits. Once the cursor bumps back down over them, a fresh
/// object there reads as already-marked, `mark` never hands it to the
/// worklist, and everything reachable only through it is swept while live.
///
/// Latent rather than measured: found while root-causing
/// `ZGC-RELOC-DECOMMIT.1`, which it did not cause -- that workload allocates
/// nothing at or above `ZGC_LARGE_OBJECT_MIN`, so this cursor never
/// retracts there. It needs a workload that fragments the large-object end.
#[test]
fn a_high_end_retraction_keeps_the_bitmap_bound_over_the_span_it_gave_back() {
    let mut arena = Arena::new(8192);
    let _live = arena.alloc_high(1024, 8).expect("live");
    let dead_a = arena.alloc_high(1024, 8).expect("dead a") as usize;
    let dead_b = arena.alloc_high(1024, 8).expect("dead b") as usize;
    let base = arena.base_ptr() as usize;
    let low_of_dead = (dead_b - base).min(dead_a - base);
    let cursor_before = arena.high_cursor();
    assert_eq!(
        arena.high_low_water(),
        cursor_before,
        "with no retraction yet the water mark IS the cursor"
    );

    arena.add_free_block(dead_a - base, 1024);
    arena.add_free_block(dead_b - base, 1024);
    assert_eq!(arena.coalesce_high(), 1, "the dead spans must merge");
    let handed_back = arena.retract_high_cursor_into_free_head();
    assert_eq!(handed_back, 2048, "the merged span starts at the cursor");
    assert!(
        arena.high_cursor() > cursor_before,
        "a high-end retraction RAISES the cursor"
    );

    // The bound must still reach down over the span just given back --
    // that is where this cycle's mark bits are.
    assert_eq!(
        arena.high_low_water(),
        low_of_dead,
        "the bound must cover the retracted span, or its mark bits survive \
         the clear and a later cycle reads them as a live object's"
    );
    assert!(
        arena.high_low_water() < arena.high_cursor(),
        "the water mark is BELOW the raised cursor, which is the whole point"
    );

    // And a reset (after every bitmap is clear) hands the next cycle the
    // tighter bound back.
    arena.reset_high_low_water();
    assert_eq!(
        arena.high_low_water(),
        arena.high_cursor(),
        "reset must return the bound to the current cursor, or the saving decays"
    );
}

/// **A decommitted free block reads as UNMAPPED, and `commit_for_relocation` is
/// what a direct writer must call before touching it.**
///
/// The unit-level half of the `relocate_stw` destination-commit bug (see
/// `ZgcRealHeap`'s `the_slide_commits_a_destination_the_give_back_had_decommitted`).
/// `decommit_free_blocks` documents its contract as "every path that hands
/// one out again goes through `hand_out`, which commits" -- so any path
/// that does NOT go through `hand_out`, as the compacting slide does not,
/// has to commit for itself. This pins both directions of that: the
/// give-back really does leave the granule unreadable, and
/// `commit_for_relocation` really does bring it back.
///
/// Skipped on a wholly-committed fallback store, where the whole question
/// is vacuous -- `is_readable_at` is unconditionally `true` there.
#[test]
fn commit_for_relocation_restores_a_granule_the_give_back_released() {
    use crate::reservation::GRANULE;
    // Several granules, so there is a whole one strictly inside the hole
    // after `decommit_free_blocks` rounds inward.
    let mut arena = Arena::new(8 * GRANULE);
    let live = arena.alloc(1024, 8).expect("live head");
    let base = arena.base_ptr() as usize;
    let live_off = live as usize - base;
    let hole_off = live_off + 1024;
    let hole_len = 5 * GRANULE;
    let _dead = arena.alloc(hole_len, 8).expect("dead span");

    arena.add_free_block(hole_off, hole_len);
    arena.coalesce_free_list();
    let released = arena.decommit_free_blocks();
    if released == 0 {
        // Fallback (non-reserving) store: nothing can be decommitted and
        // nothing can fault, so there is nothing to assert.
        assert!(arena.is_readable_at(hole_off + GRANULE));
        return;
    }

    let probe = (hole_off + GRANULE) & !(GRANULE - 1);
    assert!(
        !arena.is_readable_at(probe),
        "a released granule must report as unmapped, or the write-side \
         hazard this guards would be invisible"
    );
    assert!(
        arena.commit_for_relocation(probe, 64),
        "the OS must give a released granule back on request"
    );
    assert!(
        arena.is_readable_at(probe),
        "commit_for_relocation must leave the destination safe to write"
    );
    // And the re-committed bytes read as zero, which is what a caller
    // handed a reused block is entitled to.
    let p = (base + probe) as *const u8;
    assert_eq!(unsafe { *p }, 0, "a re-committed granule reads as zero");
}

use super::*;

/// gen r4w5/pinned5: `rebuild_after_in_place_evacuation` leaves the arena
/// in the state a sweep would — the cursor on the last survivor, the
/// zeroed complement on the free list — and the allocator never hands a
/// survivor's bytes out again, however long it keeps allocating.
#[test]
fn rebuild_after_in_place_evacuation_never_hands_out_a_survivor() {
    let mut arena = Arena::new(64 * 1024);
    let base = arena.base_ptr() as usize;
    let offs: Vec<usize> = (0..4)
        .map(|_| {
            arena
                .alloc(1024, 8)
                .expect("an empty 64 KiB arena fits 4 KiB") as usize
                - base
        })
        .collect();
    assert_eq!(offs, vec![0, 1024, 2048, 3072], "precondition: four bumps");
    // Two survivors (one of them "pinned"); the two dead spans were never
    // written, so they already read as zero.
    let live = [(1024usize, 1024usize), (3072, 1024)];
    let free = [(0usize, 1024usize), (2048, 1024)];
    arena.rebuild_after_in_place_evacuation(4096, &live, &free, 4096);
    assert_eq!(arena.used(), 4096, "the cursor sits on the last survivor");
    assert_eq!(arena.free_blocks_sorted(), free.to_vec());
    let mut handed = 0usize;
    while let Some(p) = arena.alloc(64, 8) {
        let off = p as usize - base;
        for &(lo, len) in &live {
            assert!(
                off + 64 <= lo || off >= lo + len,
                "handed out {off:#x}, inside the survivor at {lo:#x}+{len:#x}"
            );
        }
        handed += 1;
    }
    assert!(
        handed >= 32,
        "both free blocks and the tail served ({handed})"
    );
}

/// **`-Xms` moves committed bytes and moves nothing else.**
///
/// The contract [`Arena::commit_initial_prefix`] states, asserted rather
/// than trusted: an arena whose prefix has been committed is
/// indistinguishable from one that has not, except in `committed_bytes()`.
/// A "fix" that resized the arena, moved the cursor or disturbed the free
/// list would break the collectors that captured `base`/`capacity` once —
/// which is the failure mode
/// `docs/feature-designs/generative-gc-architecture.md`
/// forbids by name.
#[test]
fn commit_initial_prefix_moves_committed_bytes_and_nothing_else() {
    const CAP: usize = 16 * 1024 * 1024;
    let mut arena = Arena::new(CAP);

    let before = (
        arena.capacity(),
        arena.used(),
        arena.free_list_bytes(),
        arena.base_ptr() as usize,
        arena.remaining(),
    );
    let committed_before = arena.committed_bytes();

    let n = arena.commit_initial_prefix(8 * 1024 * 1024);
    assert_eq!(n, 8 * 1024 * 1024, "the request fits and must be honoured");

    assert_eq!(
        (
            arena.capacity(),
            arena.used(),
            arena.free_list_bytes(),
            arena.base_ptr() as usize,
            arena.remaining(),
        ),
        before,
        "-Xms must not resize the arena, move a cursor, touch the free list or relocate the base: every one of those is read without the arena lock by a collector that captured it at construction",
    );
    assert!(
        arena.committed_bytes() >= committed_before,
        "committed bytes cannot fall",
    );
    // On a RESERVING store this is a strict rise. On the wholly-committed
    // fallback (`CRATONVM_GC_RESERVE=0`, and any platform without a
    // reservation) the arena already had everything committed and there is
    // nothing to rise — which is not a failure, it is the store saying the
    // bytes were already there.
    if committed_before < CAP {
        assert!(
            arena.committed_bytes() > committed_before,
            "the store reserves ({committed_before} of {CAP} committed), so an 8 MiB prefix commit must raise the committed bytes; it left them at {}",
            arena.committed_bytes(),
        );
    }

    // The committed prefix is WRITABLE, which is the point of committing
    // it. A bump allocation out of the prefix must succeed and be readable.
    let p = arena
        .alloc(64, 8)
        .expect("the committed prefix must allocate");
    // SAFETY: `p` is a fresh 64-byte allocation from this arena.
    unsafe {
        std::ptr::write_bytes(p, 0xA5, 64);
        assert_eq!(*p, 0xA5);
    }
}

/// **An oversized `-Xms` is clamped, and a zero one is a no-op.**
///
/// `-Xms` above `-Xmx` is clamped rather than refused at every level of
/// this stack (`VmHeap::new_with_heap_sizing` says so for the flag), so the
/// arena has to behave the same way: the two sizes are specified
/// separately and an operator who oversizes one should still get a heap.
#[test]
fn commit_initial_prefix_clamps_and_ignores_zero() {
    const CAP: usize = 4 * 1024 * 1024;
    let mut arena = Arena::new(CAP);

    assert_eq!(
        arena.commit_initial_prefix(0),
        0,
        "no -Xms is not a commit of nothing, it is no call at all",
    );

    assert_eq!(
        arena.commit_initial_prefix(CAP * 8),
        CAP,
        "an -Xms above the arena's capacity commits the whole arena rather than being refused",
    );
    assert_eq!(
        arena.capacity(),
        CAP,
        "and clamping must not have grown the arena to meet the request",
    );
}

/// **A compaction may only drop the holes it wrote into.**
///
/// `compact_low_to` used to clear the whole low free list, on the premise
/// that a slide leaves no hole below the cursor. That holds for a slide
/// which compacts the entire low region; ZGC's compacts the pages its
/// relocation-set selector picked and leaves the rest untouched, so every
/// hole outside the destination window is a real free block — and the
/// sweep never re-discovers it, because the sweep only free-lists objects
/// that DIE.
///
/// The cost was total: on `repros/frag-churn` the sweep free-listed 400 MB,
/// the slide cleared it, and from then on the bump cursor advanced by
/// exactly the bytes allocated until it hit capacity and the VM threw
/// `OutOfMemoryError` with 99% of the heap dead.
///
/// Three blocks, one of each kind, so the test cannot pass by halves:
/// below the window (must survive), inside it (must go — a survivor may be
/// sitting on it), and straddling the new cursor (must be truncated, not
/// dropped, or the bytes below the cursor are lost too).
/// t1 wave 10 lane ZT3GN: the nursery-local install keeps, drops and
/// truncates exactly the blocks `compact_low_to` does (the vacated spans
/// are published merged, so the comparison is on the merged byte set),
/// leaves the free-byte total and the cursor equal, and refuses a slide
/// that wrote below its floor.
#[test]
fn the_nursery_install_lists_what_compact_low_to_lists() {
    const K: usize = 1024;
    fn fixture() -> Arena {
        let mut a = Arena::new(256 * K);
        let _ = a.alloc(200 * K, 8).expect("fresh arena has room");
        a.add_free_block(K, 512); // old gen, below lo: kept in place
        a.add_free_block(30 * K, 30 * K); // large, wholly below lo: in place
        a.add_free_block(63 * K, 2 * K); // straddles lo = 64K, overlaps touched
        a.add_free_block(80 * K, 256); // inside touched: dropped
        a.add_free_block(110 * K, 4 * K); // between touched and the spans: kept
        a.add_free_block(130 * K, 8 * K); // inside a vacated span: dropped
        a.add_free_block(170 * K, 640); // straddles nothing, below cursor: kept
        a.add_free_block(178 * K, 8 * K); // straddles the new cursor: truncated
        a.add_free_block(190 * K, 4 * K); // above the new cursor: dropped
        a
    }
    // Merge touching spans of a sorted block list (the install merges the
    // vacated spans; the old door lists them one per page).
    fn merged(v: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (off, sz) in v {
            match out.last_mut() {
                Some(last) if last.0 + last.1 == off => last.1 += sz,
                _ => out.push((off, sz)),
            }
        }
        out
    }
    let lo = 64 * K;
    let new_cursor = 180 * K;
    let touched = 64 * K..96 * K;
    // Two touching per-page spans and one apart.
    let vacated = [(128 * K, 144 * K), (144 * K, 160 * K), (164 * K, 168 * K)];

    let mut old = fixture();
    let r_old = old.compact_low_to(new_cursor, touched.clone(), &vacated);
    let mut new = fixture();
    let r_new = new
        .compact_nursery_to(new_cursor, lo, touched.clone(), &vacated, true)
        .expect("the slide fits the premise");
    assert_eq!(r_old, r_new);
    assert_eq!(old.used_low_for_compaction(), new.used_low_for_compaction());
    assert_eq!(old.free_list_bytes(), new.free_list_bytes());
    assert_eq!(
        merged(old.free_blocks_sorted()),
        merged(new.free_blocks_sorted())
    );
    let kept = new.free_blocks_sorted();
    assert!(kept.contains(&(K, 512)), "{kept:?}");
    assert!(kept.contains(&(178 * K, 2 * K)), "truncated: {kept:?}");
    assert!(
        kept.contains(&(128 * K, 32 * K)),
        "touching vacated spans publish as one block: {kept:?}"
    );
    // The deferred-zero arm lists the same bytes and does not write them.
    let mut lazy = fixture();
    let p = lazy.base_ptr_mut();
    // SAFETY: inside the arena, below the cursor.
    unsafe { *p.add(150 * K) = 0xAB };
    let _ = lazy
        .compact_nursery_to(new_cursor, lo, touched.clone(), &vacated, false)
        .expect("the slide fits the premise");
    assert_eq!(merged(lazy.free_blocks_sorted()), merged(kept.clone()));
    // SAFETY: as above.
    assert_eq!(unsafe { *lazy.base_ptr().add(150 * K) }, 0xAB);
    let mut eager = fixture();
    // SAFETY: as above.
    unsafe { *eager.base_ptr_mut().add(150 * K) = 0xAB };
    let _ = eager.compact_nursery_to(new_cursor, lo, touched.clone(), &vacated, true);
    // SAFETY: as above.
    assert_eq!(unsafe { *eager.base_ptr().add(150 * K) }, 0);
    // A slide that wrote below the floor does not fit: nothing changes.
    let mut refused = fixture();
    let before = refused.free_blocks_sorted();
    let cursor_before = refused.used_low_for_compaction();
    assert_eq!(
        refused.compact_nursery_to(new_cursor, lo, 60 * K..96 * K, &vacated, true),
        None
    );
    assert_eq!(refused.free_blocks_sorted(), before);
    assert_eq!(refused.used_low_for_compaction(), cursor_before);
}

#[test]
fn compaction_keeps_the_free_holes_it_did_not_write_into() {
    let mut arena = Arena::new(64 * 1024);
    // Bump the cursor out so every offset below is inside the live region.
    let _ = arena.alloc(32 * 1024, 8).expect("fresh arena has room");

    arena.add_free_block(1024, 512); // below the window
    arena.add_free_block(8192, 512); // inside the window
    arena.add_free_block(20_480, 4096); // straddles the new cursor

    let before = arena.free_list_bytes();
    assert_eq!(
        before,
        512 + 512 + 4096,
        "the fixture must set up three blocks"
    );

    // The slide wrote into [4096, 12288) and left the cursor at 22528.
    let reclaimed = arena.compact_low_to(22_528, 4096..12_288, &[]);
    assert!(reclaimed > 0, "the cursor must actually retract");

    let kept = arena.free_blocks_sorted();
    assert!(
        kept.iter().any(|&(off, sz)| off == 1024 && sz == 512),
        "a hole below the destination window is untouched memory and must \
         survive: {kept:?}"
    );
    assert!(
        !kept.iter().any(|&(off, _)| off == 8192),
        "a hole inside the destination window may have a survivor on it and \
         must be dropped: {kept:?}"
    );
    assert!(
        kept.iter().any(|&(off, sz)| off == 20_480 && sz == 2048),
        "a hole straddling the new cursor must be TRUNCATED to the part \
         below it, not dropped: {kept:?}"
    );
    assert_eq!(
        arena.free_list_bytes(),
        512 + 2048,
        "the accounting must match the blocks that survived"
    );
}

/// The `ZipContentTests` shape, in miniature: the cursor is a one-way
/// ratchet, so a request larger than the biggest hole is unservable even
/// when most of the heap is free.
///
/// Structure matters here. The free tail is deliberately NOT the largest
/// block, and a live object is left above the small holes, so the test
/// cannot pass by accident: a `retract` that returned any free block rather
/// than specifically the one ending AT the cursor would hand out bytes that
/// are still live, and one that merely coalesced would not move the cursor
/// at all. The RED it pins is the real one — `alloc` fails before the
/// retraction and succeeds after, with nothing else changed.
/// The defect this whole region exists for, reduced to eleven lines.
///
/// One small survivor inside each of four chunk-sized runs caps every hole
/// at one chunk, so a request larger than a chunk cannot be served however
/// much of the arena is free. This is the `TestNonBlockingAPI` shape
/// exactly: four AQS nodes, 544 bytes, holding 2.6 MB hostage.
///
/// Built so it cannot pass by accident: the survivors are deliberately
/// placed so that no two adjacent free runs can merge into anything as
/// large as the request, and the assertion on the LOW end is that the
/// request genuinely fails there — if that ever starts succeeding, the
/// second half of the test is proving nothing.
#[test]
fn a_survivor_in_every_chunk_caps_the_low_end_but_not_the_high_end() {
    // One "chunk" of small-object churn plus one large object, repeated —
    // which is what a running VM does, and the interleaving is the whole
    // point: it is what puts a survivor between every pair of large-object
    // holes when the two populations share one bump region.
    const CHUNK: usize = 4096;
    const LARGE: usize = 4096;
    const REQUEST: usize = LARGE * 2;
    const ROUNDS: usize = 4;

    let mut arena = Arena::new((CHUNK + LARGE) * ROUNDS);
    let base = arena.base_ptr() as usize;
    let mut low_churn = Vec::new();
    let mut large_dead = Vec::new();
    for _ in 0..ROUNDS {
        // A survivor that is never freed, then churn, then a large object.
        let _survivor = arena.alloc(64, 8).unwrap();
        let churn = arena.alloc(CHUNK - 64, 8).unwrap();
        let large = arena.alloc_high(LARGE, 8).unwrap();
        low_churn.push((churn as usize - base, CHUNK - 64));
        large_dead.push((large as usize - base, LARGE));
    }
    assert_eq!(
        arena.used(),
        (CHUNK + LARGE) * ROUNDS,
        "both ends are fully bumped: nothing can come from un-bumped space",
    );
    for (off, size) in &low_churn {
        arena.add_free_block(*off, *size);
    }
    arena.coalesce_free_list();

    // The low end is walled: its holes are `CHUNK - 64` apart, so however
    // many bytes it holds, none of them is a contiguous `REQUEST`. Asserted
    // while the HIGH free list is still empty, so the low end's
    // raid-the-other-end last resort cannot answer for it.
    assert!(
        arena.alloc(REQUEST, 8).is_none(),
        "precondition: the low end is walled by one survivor per chunk — if this succeeds the assertion below proves nothing",
    );

    // The large-object end never saw a survivor, so its holes merge into
    // one contiguous run.
    for (off, size) in &large_dead {
        arena.add_free_block(*off, *size);
    }
    arena.coalesce_high();
    assert!(
        arena.alloc_high(REQUEST, 8).is_some(),
        "the large-object end coalesced into a contiguous run",
    );
}

/// The two ends must never hand out the same byte, and the shared middle
/// is where that would happen.
#[test]
fn the_two_ends_meet_without_overlapping() {
    let mut arena = Arena::new(4096);
    let base = arena.base_ptr() as usize;
    // Deliberately asymmetric: an allocator that ignored the high end and
    // simply bumped upwards would put this at 1024, not 3072.
    let low = arena.alloc(1024, 8).unwrap() as usize - base;
    let high = arena.alloc_high(1024, 8).unwrap() as usize - base;
    assert_eq!(low, 0);
    assert_eq!(high, 3072, "the high end bumps DOWN from capacity");
    assert_eq!(arena.used(), 2048, "both regions count as used");
    assert_eq!(arena.remaining(), 2048, "and the middle is what is left");
    // Close the middle from the low side and check neither can cross.
    let _fill = arena.alloc(2048, 8).unwrap();
    assert_eq!(arena.remaining(), 0);
    assert!(arena.alloc(8, 8).is_none(), "the low end cannot cross over");
    assert!(
        arena.alloc_high(8, 8).is_none(),
        "and neither can the high end",
    );
}

/// The high end's own one-way-ratchet fix: a wholly-free head goes back to
/// the descending cursor. Watched to fail before it passed — with the
/// retraction stubbed to 0 the final allocation returns `None`, because
/// the two halves are separate free blocks and neither alone is big
/// enough.
#[test]
fn a_wholly_free_high_head_is_handed_back_to_the_descending_cursor() {
    let mut arena = Arena::new(8192);
    let base = arena.base_ptr() as usize;
    // From the top down: [live 1024][dead 1024][dead 1024]
    let _live = arena.alloc_high(1024, 8).unwrap();
    let dead_a = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let dead_b = arena.alloc_high(1024, 8).unwrap() as usize - base;
    assert_eq!(arena.high_cursor(), 8192 - 3072);
    arena.add_free_block(dead_a, 1024);
    arena.add_free_block(dead_b, 1024);
    assert_eq!(
        arena.coalesce_high(),
        1,
        "the two dead spans are adjacent and must merge into one",
    );

    let handed_back = arena.retract_high_cursor_into_free_head();
    assert_eq!(handed_back, 2048, "the merged span starts at the cursor");
    assert_eq!(arena.high_cursor(), 8192 - 1024, "cursor climbed back");
    assert_eq!(
        arena.free_list_bytes(),
        0,
        "the retracted span must leave the free list, or it is served twice",
    );
    assert_eq!(arena.used(), 1024, "only the live object is still charged");
}

/// The other half of that contract: a hole with a live object BELOW it (at
/// a lower address, i.e. bumped more recently) must not move the cursor.
/// Getting this wrong hands out live bytes.
/// The reserve has to survive ordinary low-end churn, or it buys nothing.
///
/// This is the shape that made the first version of the reserve useless:
/// the low end asks for space it could get from the reserve, gets it, and
/// the large-object region is drained before it can claim its floor. The
/// low end must be pushed onto its own free list instead, and the reserved
/// span must still be there for a large request afterwards.
#[test]
fn ordinary_low_end_churn_does_not_drain_the_reserve() {
    let mut arena = Arena::new(16384);
    arena.set_high_reserve(4096);
    let base = arena.base_ptr() as usize;

    // Fill the un-reserved low region, then free half of it so the low end
    // has a free list to fall back on.
    let mut freed = Vec::new();
    for i in 0..12 {
        let p = arena.alloc(1024, 8).unwrap() as usize - base;
        if i % 2 == 0 {
            freed.push(p);
        }
    }
    assert_eq!(arena.used(), 12288, "the un-reserved low region is full");
    for p in &freed {
        arena.add_free_block(*p, 1024);
    }
    // Six 1 KiB holes are now on the low free list. Six more requests must
    // all come from there and none from the reserve, which is exactly what
    // ordering the reserve override AFTER the free list buys.
    for _ in 0..6 {
        assert!(arena.alloc(1024, 8).is_some(), "served from the free list");
    }
    assert_eq!(
        arena.used(),
        12288,
        "no bump happened: every request came out of the free list",
    );
    assert!(
        arena.alloc_high(4096, 8).is_some(),
        "the reserve survived the churn and the large request fits",
    );
}

/// ...but the reserve is a preference, not a wall: a low-end request that
/// cannot be served any other way takes it rather than failing.
#[test]
fn the_reserve_yields_rather_than_raising_oom() {
    let mut arena = Arena::new(16384);
    arena.set_high_reserve(4096);
    // Consume everything the low end is allowed to bump.
    while arena.alloc(1024, 8).is_some() {
        if arena.used() > 16384 {
            panic!("bumped past capacity");
        }
    }
    assert_eq!(
        arena.used(),
        16384,
        "the low end took the reserve rather than failing with it free",
    );
}

/// The drain the gate exists to stop: a freed large object at the bottom of
/// the high region must NOT be handed back to the shared middle while the
/// region is at or under its floor, because the low end would take it and
/// the large-object region would shrink to its own live set one sweep at a
/// time (measured: 92 MB claimed, 3,224 bytes left).
#[test]
fn the_high_retraction_does_not_drain_the_region_below_its_floor() {
    let mut arena = Arena::new(16384);
    arena.set_high_reserve(4096);
    let base = arena.base_ptr() as usize;
    let dead = arena.alloc_high(2048, 8).unwrap() as usize - base;
    let cursor_before = arena.high_cursor();
    arena.add_free_block(dead, 2048);
    arena.coalesce_high();
    assert_eq!(
        arena.retract_high_cursor_into_free_head(),
        0,
        "the region holds 2 KiB against a 4 KiB floor — nothing to give back",
    );
    assert_eq!(arena.high_cursor(), cursor_before, "cursor must not move");
    assert_eq!(
        arena.high_free_shape().1,
        2048,
        "and the bytes stay on the HIGH free list, where the low end \
         cannot reach them",
    );

    // Over-claim relative to the floor and the EXCESS — only the excess —
    // is handed back.
    let over = arena.alloc_high(8192, 8).unwrap() as usize - base;
    arena.add_free_block(over, 8192);
    assert_eq!(arena.coalesce_high(), 1, "the two dead spans are adjacent");
    assert_eq!(
        arena.retract_high_cursor_into_free_head(),
        6144,
        "10 KiB claimed against a 4 KiB floor: 6 KiB goes back, 4 KiB stays",
    );
    assert_eq!(
        arena.high_region_bytes(),
        4096,
        "the region settled exactly on its floor",
    );
    assert_eq!(
        arena.high_free_shape().1,
        4096,
        "and the retained half is still on the HIGH free list",
    );
}

/// `compact_high_to` publishes ONE maximal block where the slide left a
/// mosaic — which is the entire reason the large-object end grew a
/// compactor. A best fit over three 1 KiB holes cannot serve 3 KiB; over
/// their merge it can, and that is the assertion.
#[test]
fn compacting_the_high_end_merges_its_holes_into_one_servable_block() {
    // Capacity exactly four allocations wide, so the two ends meet and the
    // bump path is out -- the state a fragmented heap is actually in, and
    // the only one in which a free-list miss is an `OutOfMemoryError`.
    let mut arena = Arena::new(4096);
    let base = arena.base_ptr() as usize;
    // Four 1 KiB large objects, alternating live and dead. Addresses
    // descend, so `a` is the highest.
    let a = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let b = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let c = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let d = arena.alloc_high(1024, 8).unwrap() as usize - base;
    assert!(a > b && b > c && c > d, "the high end bumps DOWN");
    // `b` and `d` die; the sweep free-lists them, leaving two 1 KiB holes
    // with a live kilobyte between them.
    arena.add_free_block(b, 1024);
    arena.add_free_block(d, 1024);
    assert_eq!(arena.high_free_shape(), (2, 2048, 1024));
    assert!(
        arena.alloc_high(2048, 8).is_none(),
        "the precondition: 2 KiB free in two blocks cannot serve 2 KiB"
    );

    // The slide the collector would have performed: `c` packs against `a`
    // (into `b`'s span), `a` stays. Everything below `c`'s new base is
    // then dead.
    //
    // SAFETY: the test owns the arena and nothing else reads these bytes.
    unsafe { std::ptr::copy((base + c) as *const u8, (base + b) as *mut u8, 1024) };
    let new_floor = b;
    let reclaimed = arena.compact_high_to(d, &[(d, new_floor)]);

    assert_eq!(
        reclaimed, 0,
        "and the pass buys CONTIGUITY, not bytes: the same two kilobytes
         were free before and after, which is why `reclaimed` is the wrong
         number to judge this pass by"
    );
    let (blocks, bytes, largest) = arena.high_free_shape();
    assert_eq!(
        (blocks, bytes, largest),
        (1, 2048, 2048),
        "the holes must come back as ONE block, not two touching ones"
    );
    assert!(
        arena.alloc_high(2048, 8).is_some(),
        "the request that could not be served before the compaction must \
         be servable after it -- that is the whole feature"
    );
}

/// A free block BELOW the authoritative floor is kept verbatim.
///
/// This is the case a pinned large object creates: the slide compacts the
/// region above it and knows nothing about the region below, so a wholesale
/// replacement of the high free list would lose every byte down there
/// permanently. `Arena::compact_low_to` records having made exactly that
/// mistake at the other end (the `repros/frag-churn` OOM at 99 % dead), and
/// this is the check that it was not repeated here.
#[test]
fn compacting_the_high_end_keeps_the_free_space_below_its_floor() {
    let mut arena = Arena::new(16384);
    let base = arena.base_ptr() as usize;
    let _top = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let hole_above = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let pinned = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let hole_below = arena.alloc_high(1024, 8).unwrap() as usize - base;
    arena.add_free_block(hole_above, 1024);
    arena.add_free_block(hole_below, 1024);
    assert_eq!(arena.high_free_shape().1, 2048);

    // The slide stopped at `pinned`: it is authoritative only from
    // `pinned + 1024` upwards, and it emptied nothing new up there.
    let reclaimed = arena.compact_high_to(pinned + 1024, &[]);

    assert_eq!(reclaimed, 0, "nothing new was freed");
    let (blocks, bytes, _largest) = arena.high_free_shape();
    assert_eq!(
        (blocks, bytes),
        (1, 1024),
        "the hole above the floor is inside the packed region and goes; \
         the one BELOW it must survive untouched"
    );
    assert_eq!(
        arena.free_blocks_sorted()[0],
        (hole_below, 1024),
        "and it must be the same block, at the same offset"
    );
}

/// A vacated span adjacent to a kept block comes back as one block, not
/// two touching ones. Contiguity is the only thing this pass buys, so
/// leaving the seam in would make it buy nothing.
#[test]
fn a_vacated_high_span_merges_with_the_hole_below_the_floor() {
    let mut arena = Arena::new(16384);
    let base = arena.base_ptr() as usize;
    let _top = arena.alloc_high(2048, 8).unwrap() as usize - base;
    let vacated = arena.alloc_high(2048, 8).unwrap() as usize - base;
    let below = arena.alloc_high(2048, 8).unwrap() as usize - base;
    arena.add_free_block(below, 2048);

    let reclaimed = arena.compact_high_to(vacated, &[(vacated, vacated + 2048)]);

    assert_eq!(reclaimed, 2048);
    assert_eq!(
        arena.high_free_shape(),
        (1, 4096, 4096),
        "the vacated span and the pre-existing hole below it are adjacent \
         and must be published as one 4 KiB block"
    );
}

/// The vacated span is ZEROED. A slid-away survivor leaves a valid-looking
/// `ObjectHeader` behind, and a conservative scanner that met one would
/// resurrect a corpse — the same contract `compact_low_to` and `reset`
/// state at the other end.
#[test]
fn compacting_the_high_end_zeroes_what_it_frees() {
    let mut arena = Arena::new(16384);
    let base = arena.base_ptr() as usize;
    let _live = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let corpse = arena.alloc_high(1024, 8).unwrap() as usize - base;
    // SAFETY: the test owns the arena; this is the "header left behind"
    // the zeroing exists to erase.
    unsafe { std::ptr::write_bytes((base + corpse) as *mut u8, 0xAB, 1024) };

    arena.compact_high_to(corpse, &[(corpse, corpse + 1024)]);

    // SAFETY: same span, still inside the arena.
    let bytes = unsafe { std::slice::from_raw_parts((base + corpse) as *const u8, 1024) };
    assert!(
        bytes.iter().all(|b| *b == 0),
        "the vacated span must be zeroed, or a conservative scan can \
         resurrect what was there"
    );
}

/// **The region tripwire fires on the ONE arm that can trip it.**
///
/// `Arena::alloc`'s low free-list exits return offsets below `high_cursor`
/// by construction, so arming them was arming nothing: the counter read
/// zero for a week on a workload whose fragmentation report was naming an
/// 80-byte `String` and a 24-byte `Object` above `high_cursor` at the same
/// time. The last-resort `high_fit` is the exit that can put a small object
/// in the large-object region, and this is the test that says so.
///
/// The delta, not the absolute: the counter is process-global and bounded
/// logging shares it with every other test in the binary.
#[test]
fn a_small_object_served_from_the_high_free_list_trips_the_region_wire() {
    let mut arena = Arena::new(8192);
    // Claim and release 4 KiB at the high end, so its free list has a block
    // and the two cursors can meet.
    let big = arena.alloc_high(4096, 8).unwrap() as usize - arena.base_ptr() as usize;
    arena.add_free_block(big, 4096);
    assert_eq!(arena.high_free_shape(), (1, 4096, 4096));

    // Bump the low end right up to `high_cursor`, so the ordinary paths are
    // all exhausted and only the last resort is left.
    arena.alloc(2048, 8).unwrap();
    arena.alloc(2048, 8).unwrap();
    assert_eq!(arena.low_bump_headroom(), 0, "the two ends have met");

    let before = small_allocations_in_large_region();
    let p = arena
        .alloc(64, 8)
        .expect("the last resort must still serve this rather than raise OutOfMemoryError");
    let off = p as usize - arena.base_ptr() as usize;
    assert!(
        off >= arena.high_cursor(),
        "the only space left was the large-object end's own free list"
    );
    assert_eq!(
        small_allocations_in_large_region(),
        before + 1,
        "and the region tripwire must SEE it -- the residual this closes is \
         an untriggered instrument reading zero"
    );
}

#[test]
fn a_high_hole_above_a_live_object_does_not_move_the_high_cursor() {
    let mut arena = Arena::new(8192);
    let base = arena.base_ptr() as usize;
    let dead = arena.alloc_high(1024, 8).unwrap() as usize - base;
    let _live_below = arena.alloc_high(512, 8).unwrap();
    let cursor_before = arena.high_cursor();
    arena.add_free_block(dead, 1024);
    assert_eq!(
        arena.retract_high_cursor_into_free_head(),
        0,
        "a hole with a live object below it is not the region's head",
    );
    assert_eq!(arena.high_cursor(), cursor_before, "cursor must not move");
    assert_eq!(arena.free_list_bytes(), 1024, "and the hole stays listed");
}

/// A reclaimed high-end span must come back to the HIGH free list, and a
/// low-end one to the low tiers. Routing by offset is the whole basis of
/// the region test, so a block landing in the wrong list would let a
/// small-object allocation eat the large-object region (and would trip the
/// low end's `offset + size <= cursor` invariant).
#[test]
fn reclaimed_spans_return_to_the_end_they_were_carved_from() {
    let mut arena = Arena::new(8192);
    let base = arena.base_ptr() as usize;
    let low = arena.alloc(1024, 8).unwrap() as usize - base;
    let high = arena.alloc_high(1024, 8).unwrap() as usize - base;
    assert_eq!(low, 0, "the low end starts at the bottom");
    assert_eq!(high, 8192 - 1024, "the high end starts at the top");
    arena.add_free_block(low, 1024);
    arena.add_free_block(high, 1024);
    assert_eq!(arena.free_list_bytes(), 2048);

    // The low tiers must not be able to serve a request out of the high
    // block: `alloc` never consults `free_high`.
    let served = arena.alloc(1024, 8).unwrap() as usize - base;
    assert_eq!(served, low, "the low end reused its own hole");
    let served_high = arena.alloc_high(1024, 8).unwrap() as usize - base;
    assert_eq!(served_high, high, "and the high end reused its own");
    assert_eq!(arena.free_list_bytes(), 0);
}

/// `retract_cursor_into_free_tail` reads the free list to find the block
/// that ends at the LOW cursor. Once the high end has been used, the
/// highest block in the arena is a high-region block that by construction
/// never ends there — so a version that looked at the whole list would
/// answer 0 forever and silently re-arm the ratchet it exists to prevent.
#[test]
fn a_high_region_block_does_not_blind_the_low_retraction() {
    let mut arena = Arena::new(8192);
    let base = arena.base_ptr() as usize;
    let _live_low = arena.alloc(1024, 8).unwrap();
    let tail = arena.alloc(1024, 8).unwrap() as usize - base;
    let dead_high = arena.alloc_high(2048, 8).unwrap() as usize - base;
    arena.add_free_block(dead_high, 2048);
    arena.add_free_block(tail, 1024);

    assert_eq!(
        arena.retract_cursor_into_free_tail(),
        1024,
        "the low tail must still be found with a high block on the list",
    );
    assert_eq!(arena.used(), 1024 + 2048, "low cursor moved, high did not");
}

#[test]
fn a_wholly_free_tail_is_handed_back_to_the_bump_cursor() {
    let mut arena = Arena::new(4096);
    // Layout: [live 512][dust 256][live 512][free tail 2816]
    let _live_a = arena.alloc(512, 8).unwrap();
    let dust = arena.alloc(256, 8).unwrap();
    let _live_b = arena.alloc(512, 8).unwrap();
    let tail = arena.alloc(2816, 8).unwrap();
    assert_eq!(arena.used(), 4096, "the arena is now fully bumped");

    let base = arena.base_ptr() as usize;
    arena.add_free_block(dust as usize - base, 256);
    arena.add_free_block(tail as usize - base, 2816);

    // A 1 KiB request cannot be served from the 256-byte hole, and the
    // cursor is at capacity — this is the ratchet.
    assert!(
        arena.largest_free_block() >= 1024,
        "precondition: the tail block itself is big enough",
    );

    let handed_back = arena.retract_cursor_into_free_tail();
    assert_eq!(handed_back, 2816, "the tail ends exactly at the cursor");
    assert_eq!(arena.used(), 1280, "cursor retreated to the tail's start");

    // The retracted span must not still be on the free list, or these
    // bytes would be handed out twice.
    assert_eq!(
        arena.free_list_bytes(),
        256,
        "only the interior dust hole should remain listed",
    );

    // And the bytes are usable again as ONE contiguous run.
    assert!(
        arena.alloc(2048, 8).is_some(),
        "a 2 KiB request must now be servable from the recovered tail",
    );
}

/// The other half of the contract: a free block that does NOT reach the
/// cursor must leave it alone. Without this, the retraction could hand back
/// a hole with live objects above it — heap corruption rather than a
/// missed optimisation.
#[test]
fn a_free_hole_below_a_live_object_does_not_move_the_cursor() {
    let mut arena = Arena::new(4096);
    let dead = arena.alloc(1024, 8).unwrap();
    let _live_above = arena.alloc(512, 8).unwrap();
    let used_before = arena.used();

    let base = arena.base_ptr() as usize;
    arena.add_free_block(dead as usize - base, 1024);

    assert_eq!(
        arena.retract_cursor_into_free_tail(),
        0,
        "a hole with a live object above it is not a tail",
    );
    assert_eq!(arena.used(), used_before, "cursor must not move");
    assert_eq!(
        arena.free_list_bytes(),
        1024,
        "and the hole stays on the free list",
    );
}

#[test]
fn arena_basic_alloc() {
    let mut arena = Arena::new(1024);
    assert_eq!(arena.used(), 0);
    assert_eq!(arena.capacity(), 1024);

    let ptr = arena.alloc(64, 8).unwrap();
    assert!(!ptr.is_null());
    assert_eq!(arena.used(), 64);
}

/// dohead-oom (2026-07-19): a satisfiable hole must be found no matter
/// how much dust was reclaimed ahead of it, once the bump tail is also
/// exhausted. Simulate the non-moving young collector's steady state
/// (cursor pinned at capacity, so every further allocation must come from
/// the free list): register a long run of too-small holes followed by one
/// genuinely-sized hole, then request that size. This used to depend on a
/// post-bump rescue scan because the small tier was a flat first-fit list
/// with a 16-block budget; with segregated size classes the fit-sized
/// hole is found directly in its own class, and the dust is never even
/// visited.
#[test]
fn arena_alloc_finds_fit_past_scan_budget_when_bump_tail_exhausted() {
    let dust_count = 20usize;
    let dust_size = 8usize;
    let fit_size = 32usize;
    let capacity = dust_count * dust_size + fit_size + 256;
    let mut arena = Arena::new(capacity);

    // Carve out `dust_count` tiny live objects, then a fit-sized one.
    let mut dust_offsets = Vec::new();
    for _ in 0..dust_count {
        let ptr = arena.alloc(dust_size, 8).unwrap();
        dust_offsets.push(unsafe { ptr.offset_from(arena.base_ptr_mut()) } as usize);
    }
    let fit_ptr = arena.alloc(fit_size, 8).unwrap();
    let fit_offset = unsafe { fit_ptr.offset_from(arena.base_ptr_mut()) } as usize;

    // Reclaim them in the same order a real sweep would walk them: dust
    // first (fills the `swap_remove`-backed scan prefix), fit-sized hole
    // last (lands past the scan budget).
    for &off in &dust_offsets {
        arena.add_free_block(off, dust_size);
    }
    arena.add_free_block(fit_offset, fit_size);

    // Exhaust the bump tail so the free list is the only remaining path.
    let remaining = arena.capacity() - arena.used();
    arena.alloc(remaining, 8).unwrap();
    assert_eq!(arena.used(), arena.capacity());

    // A bounded scan alone would only see `dust_size`-sized holes here
    // and report failure; the fallback full scan must find the
    // fit-sized hole regardless of its position in the list.
    let request_size = 24;
    assert!(
        fit_size >= request_size,
        "test fixture invariant: the reclaimed hole must satisfy the request"
    );
    assert!(
        arena.alloc(request_size, 8).is_some(),
        "alloc must fall back to a full free-list scan once the bump tail \
         is exhausted, instead of trusting a bounded scan's miss"
    );
}

#[test]
fn arena_multiple_allocs() {
    let mut arena = Arena::new(256);

    let p1 = arena.alloc(32, 8).unwrap();
    let p2 = arena.alloc(32, 8).unwrap();
    assert_ne!(p1, p2);
    assert_eq!(arena.used(), 64);
}

#[test]
fn arena_alignment() {
    let mut arena = Arena::new(256);

    // Allocate 3 bytes with alignment 1 — the arena consumes whole
    // 8-byte grid units (alignment invariant), so used() advances to 8.
    arena.alloc(3, 1).unwrap();
    assert_eq!(arena.used(), 8);

    // Next allocation with alignment 8 lands on the grid at offset 8.
    let p2 = arena.alloc(8, 8).unwrap();
    let offset = unsafe { p2.offset_from(arena.base_ptr_mut()) } as usize;
    assert_eq!(offset, 8); // aligned to 8
    assert_eq!(arena.used(), 16);
}

#[test]
fn arena_full_returns_none() {
    let mut arena = Arena::new(64);
    assert!(arena.alloc(64, 8).is_some());
    assert!(arena.alloc(1, 1).is_none());
}

#[test]
fn arena_alignment_reserves_trailing_padding() {
    let mut arena = Arena::new(64);
    let first = arena.alloc(44, 8).unwrap();
    let second = arena.alloc(8, 8).unwrap();

    assert_eq!(unsafe { first.offset_from(arena.base_ptr_mut()) }, 0);
    assert_eq!(unsafe { second.offset_from(arena.base_ptr_mut()) }, 48);
    assert_eq!(arena.used(), 56);
}

#[test]
fn arena_reset() {
    let mut arena = Arena::new(128);
    arena.alloc(64, 8).unwrap();
    assert_eq!(arena.used(), 64);

    arena.reset();
    assert_eq!(arena.used(), 0);

    // Can allocate again
    assert!(arena.alloc(128, 8).is_some());
}

#[test]
fn arena_contains() {
    let mut arena = Arena::new(256);
    let ptr = arena.alloc(32, 8).unwrap();

    assert!(arena.contains(ptr));
    assert!(arena.contains(unsafe { ptr.add(16) }));
    // Just outside the arena
    assert!(!arena.contains(std::ptr::null()));
}

#[test]
fn arena_zero_size_alloc() {
    let mut arena = Arena::new(64);
    let ptr = arena.alloc(0, 8).unwrap();
    assert!(!ptr.is_null());
    assert_eq!(arena.used(), 0);
}

#[test]
fn arena_alignment_overflow_returns_none() {
    let mut arena = Arena::new(64);
    // Push cursor near usize::MAX so alignment arithmetic would overflow
    arena.cursor = usize::MAX - 2;
    // align=8 means cursor + 7 would overflow usize
    assert!(arena.alloc(1, 8).is_none());
}

#[test]
fn arena_free_list_alloc_and_probe() {
    let mut arena = Arena::new(256);
    // Consume the whole bump region so only the free list can serve.
    assert!(arena.alloc(256, 8).is_some());
    assert!(arena.alloc(8, 8).is_none());

    // Reclaim two holes: 24B and 64B.
    arena.add_free_block(0, 24);
    arena.add_free_block(64, 64);
    assert_eq!(arena.largest_free_block(), 64);
    assert!(arena.has_free_block_at_least(24));
    assert!(arena.has_free_block_at_least(64));
    assert!(!arena.has_free_block_at_least(65));

    // Allocation from the 64B hole succeeds and leaves the remainder.
    assert!(arena.alloc(48, 8).is_some());
    assert_eq!(arena.largest_free_block(), 24);
}

#[test]
fn arena_max_free_upper_bound_tightens_and_fails_fast() {
    let mut arena = Arena::new(256);
    assert!(arena.alloc(256, 8).is_some()); // exhaust bump tail
    arena.add_free_block(0, 32);
    arena.add_free_block(64, 16);

    // Upper bound reflects the largest add (32).
    assert!(!arena.has_free_block_at_least(33)); // full scan → tightens to 32
    assert!(arena.has_free_block_at_least(32));

    // Consume the 32B block entirely; the cached bound (32) is now an
    // over-estimate but stays a SOUND upper bound: a 20B probe scans,
    // finds only the 16B block, answers false, and tightens to 16.
    assert!(arena.alloc(32, 8).is_some());
    assert!(!arena.has_free_block_at_least(20));
    assert!(arena.has_free_block_at_least(16));

    // A fresh add raises the bound again.
    arena.add_free_block(96, 40);
    assert!(arena.has_free_block_at_least(40));

    // clear/reset zero the bound → O(1) false.
    arena.clear_free_list();
    assert!(!arena.has_free_block_at_least(1));
    assert!(arena.has_free_block_at_least(0) == false);
}

#[test]
fn arena_tiered_free_lists_route_and_serve_by_size() {
    let mut arena = Arena::new(64 * 1024);
    let cap = arena.capacity();
    assert!(arena.alloc(cap, 8).is_some()); // exhaust bump tail

    // A big coalesced span + a pile of node-sized holes.
    arena.add_free_block(0, 32 * 1024); // → free_large
    for i in 0..100 {
        arena.add_free_block(40 * 1024 + i * 72, 72); // → free_small
    }

    // Chunk-sized request is served from the span tier (the small holes
    // can never satisfy it and must not be scanned — behaviorally: it
    // just succeeds).
    assert!(arena.alloc(16 * 1024, 8).is_some());
    // Object-sized requests are served from the hole tier.
    assert!(arena.alloc(72, 8).is_some());
    assert!(arena.alloc(72, 8).is_some());

    // Probes: sub-tier-boundary probe answers O(1)-yes while a span
    // remains; chunk-sized probe consults spans only.
    assert!(arena.has_free_block_at_least(72));
    assert!(arena.has_free_block_at_least(8 * 1024));
    assert!(!arena.has_free_block_at_least(33 * 1024));

    // Consume the span remainder; chunk probes now answer false while
    // hole-sized probes still answer true.
    assert!(arena.alloc(16 * 1024 - 16, 8).is_some());
    assert!(!arena.has_free_block_at_least(LARGE_BLOCK_MIN));
    assert!(arena.has_free_block_at_least(72));

    // Splitting a span routes a sub-tier remainder to the small list:
    // reclaim a 5 KiB block, carve 4.5 KiB → ~0.5 KiB remainder must
    // still be findable by a small request.
    let mut a2 = Arena::new(8 * 1024);
    let c2 = a2.capacity();
    assert!(a2.alloc(c2, 8).is_some());
    a2.add_free_block(0, 5 * 1024);
    assert!(a2.alloc(4 * 1024 + 512, 8).is_some());
    assert!(a2.has_free_block_at_least(256));
    assert!(a2.alloc(256, 8).is_some());
}

#[test]
fn arena_alloc_fail_fast_skips_scan_for_oversized_requests() {
    let mut arena = Arena::new(128);
    assert!(arena.alloc(128, 8).is_some()); // exhaust bump tail
    arena.add_free_block(0, 24);
    // Larger than any block AND no bump room → None (via the O(1)
    // fail-fast; behaviorally identical to the historical full scan).
    assert!(arena.alloc(64, 8).is_none());
    // Fits the hole → allocates from the free list.
    assert!(arena.alloc(16, 8).is_some());
}

// ----- Allocator-recorded sweep anchors (perf/gc-oracle-anchors) --------

#[test]
fn alloc_anchors_are_one_per_bucket_and_strictly_increasing() {
    let mut arena = Arena::new(64 * 1024);
    let bucket = 1usize << arena.anchor_shift;
    // Three offsets in bucket 0, one in bucket 1, one in bucket 3 —
    // recorded out of bucket order on purpose.
    arena.note_object_start(3 * bucket + 24);
    arena.note_object_start(0);
    arena.note_object_start(64);
    arena.note_object_start(128);
    arena.note_object_start(bucket + 8);
    let a = arena.take_alloc_anchors();
    // First-in-bucket wins, and draining yields ascending offsets.
    assert_eq!(a, vec![0, bucket + 8, 3 * bucket + 24]);
    assert!(a.windows(2).all(|w| w[0] < w[1]));
    // Draining leaves the table empty for the next epoch.
    assert!(arena.take_alloc_anchors().is_empty());
}

#[test]
fn alloc_anchors_bucket_table_stays_bounded_and_in_range() {
    // A 2 GiB arena must not mint a per-4 KiB table (that would be 512 K
    // entries); the shift widens until the bucket count fits the cap.
    let mut arena = Arena::new(64 * 1024);
    arena.rearm_alloc_anchors();
    assert!(arena.alloc_anchors.len() <= (1 << 16) + 1);
    // Out-of-range offsets are ignored rather than panicking (a caller
    // handing over a stale offset must never take the VM down).
    arena.note_object_start(usize::MAX);
    arena.note_object_start(arena.capacity() * 4);
    assert!(arena.take_alloc_anchors().is_empty());
}

/// gengc-round2-alloc2, 2026-09-20 — the bump path's alignment padding is
/// free-listed instead of orphaned.
///
/// Verification step 2(b) of
/// `gengc-alloc-arena-bump-path-orphans-alignment-padding-FIXED-20260923.md`
/// verbatim: allocate with `align = 16` from a cursor at offset 8 and
/// assert `free_list_bytes()` grew by the 8 padding bytes. Before this it
/// did neither — the bytes simply vanished, counted by `used()`, absent
/// from the free list, and left for a linear walk to read as an
/// `ObjectHeader`.
///
/// The setup is the whole test. `align = 8` cannot produce padding (the
/// cursor is 8-aligned by construction), so the first allocation has to
/// move the cursor OFF the 16-byte grid while staying on the 8-byte one —
/// which an 8-byte request does exactly.
#[test]
fn the_bump_path_free_lists_its_alignment_padding() {
    let mut arena = Arena::new(64 * 1024);
    assert_eq!(arena.free_list_bytes(), 0);
    // The free-list tiers align the ABSOLUTE address, so the reuse half at
    // the end needs an 8-aligned backing base. `alloc_zeroed_heap` asks for
    // `align = 1` and no type in this tree asserts more — but every real
    // allocator delivers page alignment for a 64 KiB block, and
    // `note_object_start`'s own `debug_assert` already depends on the same
    // thing. Stated here so a failure names the cause instead of showing up
    // as a mysterious free-list miss.
    assert_eq!(
        arena.base_ptr() as usize % 8,
        0,
        "the arena's backing base is not 8-aligned",
    );

    // Cursor 0 -> 8. No padding is possible at `align = 8`.
    assert!(arena.alloc(8, 8).is_some());
    assert_eq!(arena.used(), 8);
    assert_eq!(arena.free_list_bytes(), 0, "an 8-aligned bump pads nothing");

    // Cursor 8, asking for 16-byte alignment: 8 bytes of padding.
    let p = arena.alloc(32, 16).expect("the arena is nearly empty");
    // The bump tier aligns the OFFSET, not the absolute address (the
    // free-list tiers align the address; `heap_bitmap`'s header documents
    // the asymmetry and the overflow set that covers it). Assert what this
    // path actually promises.
    assert_eq!(
        (p as usize - arena.base_ptr() as usize) % 16,
        0,
        "the allocation itself must honour the alignment it asked for"
    );
    assert_eq!(
        arena.free_list_bytes(),
        8,
        "the 8 bytes the round-up stepped over must be on the free list, \
         not orphaned inside [0, cursor)"
    );
    // The padding is a legal low-tier block: both ends on the 8-byte grid.
    // (`push_block_routed` warns otherwise, and a 1..7-byte splinter there
    // would be the grid violation this fix must not introduce.)
    let blocks = arena.free_blocks_sorted();
    assert_eq!(blocks, vec![(8, 8)]);

    // ...and it is reusable, which is the accounting half: a bump-path
    // alignment gap used to be unreachable forever.
    let reused = arena
        .alloc(8, 8)
        .expect("the 8-byte hole fits an 8-byte request");
    assert_eq!(reused as usize - arena.base_ptr() as usize, 8);
    assert_eq!(arena.free_list_bytes(), 0);
}

/// gengc-round1-alloc, 2026-09-20 — an offset this arena could not have
/// handed out records nothing, and does not panic.
///
/// The grid screen added beside it must not turn a stale offset (one from
/// before a `grow`, say) into an assertion failure: that is the behaviour
/// `get_mut` has always given and a collector handing over a stale offset
/// must never take the VM down. The screen therefore runs only for offsets
/// inside the arena.
#[test]
fn note_object_start_ignores_offsets_outside_the_arena() {
    let mut arena = Arena::new(64 * 1024);
    let cap = arena.capacity();
    arena.note_object_start(cap); // one past the last byte
    arena.note_object_start(cap * 4);
    arena.note_object_start(usize::MAX);
    assert!(
        arena.take_alloc_anchors().is_empty(),
        "an out-of-range offset must record no anchor",
    );
    // ...and the last in-range grid slot still does record.
    arena.note_object_start(cap - 8);
    assert_eq!(arena.take_alloc_anchors(), vec![cap - 8]);
}

#[test]
fn reset_clears_alloc_anchors() {
    // An anchor describes the object grid of the epoch that just ended;
    // `reset` zeroes the arena, so every recorded offset is now dead.
    let mut arena = Arena::new(64 * 1024);
    assert!(arena.alloc(64, 8).is_some());
    arena.note_object_start(0);
    arena.reset();
    assert!(arena.take_alloc_anchors().is_empty());
}

// ----- Segregated small tier (perf/gc-allocation-fastpath) --------------

/// The bitmap helpers are the load-bearing part of the small tier: a wrong
/// answer either loses a satisfiable block (spurious OOM / forced GC) or
/// claims one that does not exist (failed refill).
#[test]
fn small_bucket_mask_probes_are_exact() {
    let mut mask = [0u64; SMALL_MASK_WORDS];
    assert_eq!(mask_first_from(&mask, 0), None);
    assert_eq!(mask_last(&mask), None);

    // One bit in each word, including the last.
    for k in [0usize, 1, 63, 64, 65, 127, 200, SMALL_BUCKETS - 1] {
        mask_set(&mut mask, k);
    }
    assert_eq!(mask_first_from(&mask, 0), Some(0));
    assert_eq!(mask_first_from(&mask, 1), Some(1));
    assert_eq!(mask_first_from(&mask, 2), Some(63));
    assert_eq!(mask_first_from(&mask, 64), Some(64));
    assert_eq!(mask_first_from(&mask, 66), Some(127));
    assert_eq!(mask_first_from(&mask, 128), Some(200));
    assert_eq!(mask_first_from(&mask, 201), Some(SMALL_BUCKETS - 1));
    // A start past the table must not index out of bounds.
    assert_eq!(mask_first_from(&mask, SMALL_BUCKETS), None);
    assert_eq!(mask_first_from(&mask, usize::MAX), None);
    assert_eq!(mask_last(&mask), Some(SMALL_BUCKETS - 1));

    mask_clear(&mut mask, SMALL_BUCKETS - 1);
    assert_eq!(mask_last(&mask), Some(200));
    for k in [0usize, 1, 63, 64, 65, 127, 200] {
        mask_clear(&mut mask, k);
    }
    assert_eq!(mask_last(&mask), None);
    assert_eq!(mask_first_from(&mask, 0), None);
}

/// A same-shaped request must land in its own size class and consume the
/// hole WHOLE — no split, therefore no dust for the next request to walk
/// past. This is the property the flat first-fit list could not hold.
#[test]
fn small_tier_exact_class_reuses_holes_without_splitting() {
    let node = 72usize;
    let count = 500usize;
    let mut arena = Arena::new(count * node + 4096);
    let mut offsets = Vec::new();
    for _ in 0..count {
        let p = arena.alloc(node, 8).unwrap();
        offsets.push(unsafe { p.offset_from(arena.base_ptr_mut()) } as usize);
    }
    // Exhaust the bump tail so only the free list can serve.
    let tail = arena.capacity() - arena.used();
    arena.alloc(tail, 8).unwrap();
    for &off in &offsets {
        arena.add_free_block(off, node);
    }
    let reclaimed = arena.free_list_bytes();
    assert_eq!(reclaimed, count * node);

    // Every re-allocation is an exact-class hit: the free list shrinks by
    // exactly one node each time and never grows a remainder block.
    for i in 0..count {
        assert!(
            arena.alloc(node, 8).is_some(),
            "exact-class reuse failed at iteration {i}"
        );
        assert_eq!(arena.free_list_bytes(), reclaimed - (i + 1) * node);
    }
    assert_eq!(arena.free_list_bytes(), 0);
    assert_eq!(arena.largest_free_block(), 0);
    assert!(arena.alloc(node, 8).is_none());
}

/// An allocation must not fail while the bytes it needs are present and
/// merely SPLIT across adjacent free blocks.
///
/// Coalescing used to run only in a collector's post-sweep hook, so between
/// two sweeps the list accumulated adjacency nothing merged: every `split`
/// remainder, and on ZGC every TLAB retire (which returns the unused tail of
/// a chunk whose used part the sweep already free-listed object by object).
/// The heap then reported `OutOfMemoryError` with a free list holding
/// hundreds of times the requested bytes — measured live on the 2026-08-11
/// Hibernate run as `free_list_bytes=1211378512 largest_free_block=65528`
/// against a 65552-byte request.
///
/// The arm order is the point: the request is proved to FAIL before the
/// merge and to SUCCEED after it, on the same list. A test that only
/// asserted the success would pass on an arena that never fragmented.
#[test]
fn alloc_merges_adjacent_holes_before_reporting_failure() {
    let mut arena = Arena::new(256 * 1024);
    let cap = arena.capacity();
    arena.alloc(cap, 8).unwrap(); // pin the cursor: the free list is all there is

    // Four abutting 64 KiB-ish holes: 256 KiB of free space, no single
    // block big enough for a 96 KiB request.
    let hole = 64 * 1024;
    for i in 0..4 {
        arena.add_free_block(i * hole, hole);
    }
    assert_eq!(arena.free_list_bytes(), 4 * hole);
    assert_eq!(
        arena.largest_free_block(),
        hole,
        "pre-merge the list must really be capped at one hole",
    );

    // RED: the request cannot be served block-by-block...
    assert!(
        !arena.has_free_block_at_least(96 * 1024),
        "no single block may fit — otherwise the merge is not what serves it",
    );
    // ...but the bytes are all there, contiguously.
    let merged_away = arena.coalesce_free_list();
    assert_eq!(merged_away, 3, "four abutting holes must become one span");
    assert_eq!(arena.largest_free_block(), 4 * hole);
    assert_eq!(
        arena.free_list_bytes(),
        4 * hole,
        "merging must not lose or duplicate a byte",
    );

    // A second merge on an unchanged list is free and a no-op.
    assert_eq!(arena.coalesce_free_list(), 0);

    // And `alloc` reaches the merge by itself, without a collection: rebuild
    // the same fragmented state and ask for 96 KiB directly.
    let mut arena = Arena::new(256 * 1024);
    arena.alloc(arena.capacity(), 8).unwrap();
    for i in 0..4 {
        arena.add_free_block(i * hole, hole);
    }
    assert!(
        arena.alloc(96 * 1024, 8).is_some(),
        "alloc must coalesce and retry rather than return None with 256 KiB free",
    );
    assert_eq!(arena.free_list_bytes(), 4 * hole - 96 * 1024);
}

/// The last-resort merge must back off on a heap it cannot help.
///
/// `alloc` reaches that arm once per allocation on a wedged heap, and the
/// merge is `O(n log n)` over the whole free list — ungated it showed up as
/// 9.1% of a `type.temporal.ZonedDateTimeTest` profile the day it landed.
/// A list whose holes are walled by live data has nothing to merge, so the
/// arm has to switch itself off rather than re-sort forever.
#[test]
fn the_last_resort_merge_backs_off_when_merging_never_pays() {
    let mut arena = Arena::new(64 * 1024);
    arena.alloc(arena.capacity(), 8).unwrap();
    // Non-adjacent holes: an 8-byte live wall between each pair.
    for i in 0..8 {
        arena.add_free_block(i * 1024, 1016);
    }
    assert_eq!(arena.coalesce_free_list(), 0, "nothing here is adjacent");
    let after_one = arena.coalesce_threshold;
    assert!(
        after_one > COALESCE_THRESHOLD_MIN,
        "an unproductive merge must raise the bar, got {after_one}",
    );
    // Keep feeding it non-adjacent blocks; the bar must keep rising.
    for round in 0..6 {
        for _ in 0..arena.coalesce_threshold.min(64) {
            arena.free_pushed += 1;
        }
        let before = arena.coalesce_threshold;
        assert_eq!(arena.coalesce_free_list(), 0, "round {round}");
        assert!(
            arena.coalesce_threshold >= before,
            "round {round}: the bar must never fall on an unproductive merge",
        );
    }
    assert!(
        arena.coalesce_threshold >= 4096,
        "six unproductive merges must have effectively switched the arm off, \
         got {}",
        arena.coalesce_threshold,
    );

    // ...and a productive merge re-arms it immediately, so a list that
    // really is merely split is never left unmerged.
    arena.clear_free_list();
    arena.add_free_block(0, 1024);
    arena.add_free_block(1024, 1024);
    assert_eq!(arena.coalesce_free_list(), 1);
    assert_eq!(arena.coalesce_threshold, COALESCE_THRESHOLD_MIN);
}

/// The span tier serves BEST fit, and every accounting field stays exact
/// across takes and splits.
///
/// The tier used to be a `Vec` scanned first-fit, so the block a request got
/// depended on push order: a 5 KiB request could consume a 1 MiB span and
/// leave a ~1 MiB remainder, which is how a tier that is supposed to hold "a
/// handful of spans" ends up holding thousands. Keyed by size, the answer is
/// the smallest bucket that fits.
#[test]
fn span_tier_serves_best_fit_and_keeps_its_accounting_exact() {
    let mut arena = Arena::new(4 * 1024 * 1024);
    arena.alloc(arena.capacity(), 8).unwrap(); // pin the cursor

    // Pushed largest-first on purpose: a first-fit scan would take the
    // 1 MiB span for a 5 KiB request.
    arena.add_free_block(0, 1024 * 1024);
    arena.add_free_block(2 * 1024 * 1024, 64 * 1024);
    arena.add_free_block(3 * 1024 * 1024, 8 * 1024);
    assert_eq!(arena.free_span_shape(), (3, 3));
    assert_eq!(arena.largest_free_block(), 1024 * 1024);
    let total = 1024 * 1024 + 64 * 1024 + 8 * 1024;
    assert_eq!(arena.free_list_bytes(), total);

    // 5 KiB must come out of the 8 KiB span, leaving a 3 KiB remainder that
    // routes to the SMALL tier (below LARGE_BLOCK_MIN).
    let want = 5 * 1024;
    assert!(arena.alloc(want, 8).is_some());
    assert_eq!(
        arena.free_span_shape(),
        (2, 2),
        "best fit must consume the 8 KiB span, not the 1 MiB one",
    );
    assert_eq!(arena.largest_free_block(), 1024 * 1024);
    assert_eq!(
        arena.free_list_bytes(),
        total - want,
        "the split remainder must stay on the list, exactly",
    );

    // Exhaust the 1 MiB span exactly: the bucket must disappear, so the
    // maximum falls to the next span rather than reporting a stale value.
    assert!(arena.alloc(1024 * 1024, 8).is_some());
    assert_eq!(arena.largest_free_block(), 64 * 1024);
    assert_eq!(arena.free_span_shape(), (1, 1));

    // A request past every span is an AUTHORITATIVE miss, and must agree
    // with the probe.
    assert!(!arena.has_free_block_at_least(128 * 1024));
    assert!(arena.alloc(128 * 1024, 8).is_none());
    assert!(arena.has_free_block_at_least(64 * 1024));
}

/// Two same-size spans in one bucket are two distinct blocks, and the
/// alignment window (`align > 8`) still finds a fit only some of them have.
///
/// The bucket head is taken without a per-block test, which is exact only
/// because every block in a bucket has the same size AND every offset is on
/// the 8-byte grid. For `align > 8` neither is enough — the fit depends on
/// the block's own offset — so that window is searched per block. If it were
/// not, a miss would stop being authoritative and `alloc` would report OOM
/// with a usable hole on the list.
#[test]
fn span_tier_handles_duplicate_sizes_and_the_alignment_window() {
    let mut arena = Arena::new(256 * 1024);
    arena.alloc(arena.capacity(), 8).unwrap();
    arena.add_free_block(0, 8192);
    arena.add_free_block(16384, 8192);
    assert_eq!(
        arena.free_span_shape(),
        (2, 1),
        "two blocks, one size class",
    );
    assert_eq!(arena.free_list_bytes(), 16384);
    assert!(arena.alloc(8192, 8).is_some());
    assert_eq!(arena.free_span_shape(), (1, 1));
    assert!(arena.alloc(8192, 8).is_some());
    assert_eq!(arena.free_span_shape(), (0, 0));
    assert_eq!(arena.largest_free_block(), 0);

    // Alignment window: one span whose base is 4096-aligned and one whose
    // base is not. A 4096-aligned request of exactly the span size can only
    // be served by the first.
    let mut arena = Arena::new(256 * 1024);
    arena.alloc(arena.capacity(), 8).unwrap();
    let base = arena.base_ptr() as usize;
    // Offsets chosen so `base + off` alignment differs by construction.
    let aligned_off = (4096 - (base & 4095)) & 4095;
    arena.add_free_block(aligned_off + 8192, 4096); // 4096-aligned start
    arena.add_free_block(aligned_off + 4096 + 8, 4096); // deliberately not
    let hit = arena.alloc(4096, 4096);
    assert!(
        hit.is_some(),
        "the aligned span must still be found through the alignment window",
    );
    assert_eq!(hit.unwrap() as usize & 4095, 0);
}

/// A tier holding many spans must answer a hopeless request without looking
/// at all of them.
///
/// This is the regression the size-keyed map exists for, written so the old
/// implementation FAILS IT BY TIMING OUT rather than by an assertion: 40k
/// spans x 40k failing probes is 1.6e9 block visits under the old unbounded
/// first-fit scan (minutes), and 40k range probes now (milliseconds). A
/// plain assertion could not catch it — the old code returned the right
/// answer, just not this decade. `Arena::alloc` was 76% of all CPU on
/// `DefaultCatalogAndSchemaTest` because of exactly this shape.
#[test]
fn a_hopeless_request_against_a_huge_span_tier_is_not_a_scan() {
    let spans = 40_000usize;
    let span = 8 * 1024usize;
    let mut arena = Arena::new(spans * span * 2);
    arena.alloc(arena.capacity(), 8).unwrap();
    for i in 0..spans {
        arena.add_free_block(i * span * 2, span);
    }
    assert_eq!(arena.free_span_shape(), (spans, 1));
    // Every one of these is a proven miss: no span is anywhere near 1 MiB.
    for _ in 0..spans {
        assert!(arena.alloc(1024 * 1024, 8).is_none());
    }
    // ...and the tier is untouched, so nothing was consumed on the way.
    assert_eq!(arena.free_span_shape(), (spans, 1));
    assert_eq!(arena.free_list_bytes(), spans * span);
}

/// The merge must not invent contiguity: a live object between two holes is
/// a wall, and merging across it would hand the same bytes out twice.
#[test]
fn coalesce_does_not_merge_across_a_gap() {
    let mut arena = Arena::new(64 * 1024);
    arena.alloc(arena.capacity(), 8).unwrap();
    arena.add_free_block(0, 4096);
    arena.add_free_block(4096 + 8, 4096); // 8-byte live wall
    assert_eq!(arena.coalesce_free_list(), 0);
    assert_eq!(arena.largest_free_block(), 4096);
    assert_eq!(arena.free_list_bytes(), 8192);
    assert!(arena.alloc(8192, 8).is_none());
}

/// `largest_free_block` must stay EXACTLY allocatable: `refill_tlab` sizes
/// a mini-TLAB from it and immediately allocates that many bytes, so an
/// over-estimate turns into a wedge. Check it against a brute-force
/// maximum across both tiers, through pushes, splits and clears.
#[test]
fn largest_free_block_is_allocatable_across_tiers() {
    let mut arena = Arena::new(256 * 1024);
    let cap = arena.capacity();
    arena.alloc(cap, 8).unwrap(); // exhaust the bump tail
    assert_eq!(arena.largest_free_block(), 0);

    // Small tier only.
    arena.add_free_block(0, 40);
    arena.add_free_block(64, 4088); // top small class
    assert_eq!(arena.largest_free_block(), 4088);
    // ...and it really is allocatable.
    assert!(arena.alloc(4088, 8).is_some());
    assert_eq!(arena.largest_free_block(), 40);

    // Span tier dominates whenever it is non-empty.
    arena.add_free_block(8192, 64 * 1024);
    assert_eq!(arena.largest_free_block(), 64 * 1024);
    assert!(arena.alloc(64 * 1024, 8).is_some());
    // The span was consumed whole, so the small tier is the maximum again.
    assert_eq!(arena.largest_free_block(), 40);

    arena.clear_free_list();
    assert_eq!(arena.largest_free_block(), 0);
    assert_eq!(arena.free_list_bytes(), 0);
}

/// The `max_free_upper` fail-fast gate must never under-state the truth:
/// an under-estimate makes `alloc` skip a free list that could have served
/// the request and report a false OOM (which the JIT slow path turns into
/// a forced GC). Drive it through the small-tier tightening path.
#[test]
fn max_free_upper_stays_a_sound_upper_bound() {
    let mut arena = Arena::new(1024);
    arena.alloc(1024, 8).unwrap(); // exhaust the bump tail
    arena.add_free_block(0, 128);
    arena.add_free_block(256, 64);

    // A miss tightens the bound; the bound must still cover every block.
    assert!(!arena.has_free_block_at_least(129));
    assert!(arena.max_free_upper >= 128);
    assert!(arena.alloc(128, 8).is_some());

    // After the 128 block is gone the bound may still read 128 (it only
    // ever over-estimates) but a 64-byte request must still be served.
    assert!(arena.alloc(64, 8).is_some());
    assert!(arena.alloc(8, 8).is_none());
    assert_eq!(arena.free_list_bytes(), 0);
}

/// A reclaimed hole smaller than the request must not be handed out, and
/// the escalating class search must not skip a class that could serve it.
#[test]
fn small_tier_escalates_to_the_next_occupied_class() {
    let mut arena = Arena::new(1024);
    arena.alloc(1024, 8).unwrap();
    arena.add_free_block(0, 24);
    arena.add_free_block(64, 56);

    // 32 bytes: class 4 is empty, class 7 (56) is the next occupied one.
    let p = arena.alloc(32, 8).unwrap();
    assert_eq!(unsafe { p.offset_from(arena.base_ptr_mut()) }, 64);
    // The 24-byte remainder went back as its own class, alongside the
    // untouched 24-byte hole.
    assert_eq!(arena.free_list_bytes(), 24 + 24);
    assert_eq!(arena.largest_free_block(), 24);
    // Nothing can serve 32 any more.
    assert!(arena.alloc(32, 8).is_none());
    // But two 24s can still be served.
    assert!(arena.alloc(24, 8).is_some());
    assert!(arena.alloc(24, 8).is_some());
    assert!(arena.alloc(8, 8).is_none());
}

/// The sweep's hole map must see every block regardless of which class it
/// landed in, in ascending offset order.
#[test]
fn sort_by_offset_matches_the_reference_sort_on_both_branches() {
    // A deterministic LCG rather than a dependency; the point is a spread
    // of offsets that straddles several radix digits, not randomness.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // 8 exercises the comparison branch, 5000 the radix branch, and 512 is
    // exactly the boundary.
    for &n in &[0usize, 1, 8, 511, 512, 513, 5000] {
        let mut offsets: Vec<usize> = Vec::with_capacity(n);
        while offsets.len() < n {
            // Unique, 8-aligned, inside a 512 MB arena — the real key shape.
            let off = ((next() as usize) % (64 * 1024 * 1024)) * 8;
            if !offsets.contains(&off) {
                offsets.push(off);
            }
        }
        let mut v: Vec<(usize, usize)> = offsets.iter().map(|&o| (o, o ^ 0xFF)).collect();
        let mut expected = v.clone();
        expected.sort_by_key(|&(off, _)| off);
        super::sort_by_offset(&mut v);
        assert_eq!(v, expected, "n={n}");
    }
}

#[test]
fn sort_by_offset_is_stable_on_duplicate_keys() {
    // Free blocks never share an offset, so this is belt-and-braces: it
    // pins that the radix branch cannot reorder equal keys, which is what
    // lets both branches claim to match the old `sort_by_key`.
    let mut v: Vec<(usize, usize)> = (0..2000).map(|i| (i % 4, i)).collect();
    let mut expected = v.clone();
    expected.sort_by_key(|&(off, _)| off);
    super::sort_by_offset(&mut v);
    assert_eq!(v, expected);
}

/// gen r4w6/tlab6 — `return_unused_tail`'s three answers. A tail at the
/// cursor retracts it (and forgets the chunk base's start bit and anchor
/// when the whole chunk comes back); a tail below the cursor is listed if
/// it reaches `min_free`; everything else is declined untouched.
#[test]
fn an_unused_tlab_tail_retracts_the_cursor_or_joins_the_free_list() {
    let mut a = Arena::new(64 * 1024);
    a.arm_object_starts();
    let base = a.base_ptr() as usize;
    // Two 16 KiB "TLAB" chunks, as `refill_tlab` carves them.
    let c1 = a.alloc(16 * 1024, 8).unwrap() as usize - base;
    a.note_object_start(c1);
    let c2 = a.alloc(16 * 1024, 8).unwrap() as usize - base;
    a.note_object_start(c2);
    assert_eq!(a.used_low_for_compaction(), 32 * 1024);

    // Off-grid, empty, past the cursor, or in no chunk: declined, no change.
    assert_eq!(a.return_unused_tail(c2 + 4, 64, 0), TailReturn::Declined);
    assert_eq!(a.return_unused_tail(c2, 0, 0), TailReturn::Declined);
    assert_eq!(a.return_unused_tail(c2, 32 * 1024, 0), TailReturn::Declined);
    assert_eq!(a.used_low_for_compaction(), 32 * 1024);
    assert_eq!(a.free_list_bytes(), 0);

    // The first chunk's tail is not at the cursor: below `min_free` it is
    // declined, at or above it is listed.
    let t1 = c1 + 4096;
    assert_eq!(
        a.return_unused_tail(t1, 12 * 1024, 16 * 1024),
        TailReturn::Declined
    );
    assert_eq!(
        a.return_unused_tail(t1, 12 * 1024, 8 * 1024),
        TailReturn::FreeListed
    );
    assert_eq!(a.free_list_bytes(), 12 * 1024);
    assert_eq!(
        a.used_low_for_compaction(),
        32 * 1024,
        "listing never moves the cursor"
    );

    // The second chunk comes back WHOLE (retired before its first
    // allocation): the cursor retracts onto its base, and the base stops
    // being an object start and an anchor.
    // `Some(true)` when `CRATONVM_GC_OBJECT_STARTS` armed the bitmap,
    // `None` when it is off (the default); never `Some(false)` here.
    assert_ne!(a.is_object_start(base + c2), Some(false));
    assert!(a.alloc_anchors_snapshot().contains(&c2));
    assert_eq!(
        a.return_unused_tail(c2, 16 * 1024, 1 << 30),
        TailReturn::Retracted,
        "a tail at the cursor retracts whatever its size"
    );
    assert_eq!(a.used_low_for_compaction(), c2);
    assert_ne!(a.is_object_start(base + c2), Some(true));
    assert!(!a.alloc_anchors_snapshot().contains(&c2));
    assert!(
        a.low_high_water() >= 32 * 1024,
        "the high-water mark keeps the given-back span"
    );
    // The next carve starts exactly where the returned tail did (14 KiB, so
    // the listed 12 KiB block cannot serve it first).
    let c3 = a.alloc(14 * 1024, 8).unwrap() as usize - base;
    assert_eq!(c3, c2);
}

/// A grown arena must not answer object-start queries from the bitmap it
/// had before it moved.
///
/// `grow_to` changes both the base and the span, and the bitmap is indexed
/// off the base and bounded by the span. Left in place it would answer for
/// an address space that no longer exists — in the ACCEPT direction, which
/// means handing back `Some(true)` for an address this arena never
/// allocated, and a mark-bit write into whatever is actually there.
///
/// The arm is explicit here because `Arena::new` deliberately does not do it
/// (only `GenerationalHeap` consults the bitmap, and every other `Arena` in
/// the process would otherwise pay capacity/64 for nothing).
#[test]
fn a_grown_arena_rebuilds_its_object_start_bitmap() {
    let mut a = Arena::new(4096);
    a.arm_object_starts();
    if !a.tracks_object_starts() {
        return; // switch off in this environment; nothing to assert
    }
    let p = a.alloc(64, 8).expect("fresh arena has room");
    let old_addr = p as usize;
    assert_eq!(
        a.is_object_start(old_addr),
        Some(true),
        "the allocation door must record the base it handed out"
    );

    a.reset();
    assert_eq!(
        a.is_object_start(old_addr),
        Some(false),
        "reset invalidates every base; a stale bit would be accepted next cycle"
    );

    a.grow(1 << 20);
    assert!(
        a.tracks_object_starts(),
        "a grown arena must still track, or the collector silently loses the fast path with no census entry to show for it"
    );
    let q = a.alloc(64, 8).expect("grown arena has room");
    assert_eq!(
        a.is_object_start(q as usize),
        Some(true),
        "the rebuilt bitmap must be indexed off the NEW base"
    );
    // And nothing in the new arena inherits a bit from the old geometry.
    assert_eq!(a.is_object_start(q as usize + 8), Some(false));
}

/// A freed base stops being a base at the moment the free list learns of it.
///
/// Without the removal half, a larger object later allocated over the hole
/// sees the stale bit, is accepted at an INTERIOR address, and takes a
/// mark-bit write into the middle of a live object — the corruption the
/// extent check in `is_object_address` exists to stop.
#[test]
fn freeing_a_block_clears_its_object_start_bit() {
    let mut a = Arena::new(4096);
    a.arm_object_starts();
    if !a.tracks_object_starts() {
        return;
    }
    let p = a.alloc(64, 8).expect("fresh arena has room");
    let addr = p as usize;
    let off = addr - a.base_ptr() as usize;
    assert_eq!(a.is_object_start(addr), Some(true));
    a.add_free_block(off, 64);
    assert_eq!(
        a.is_object_start(addr),
        Some(false),
        "a freed base must stop being a base"
    );
}

/// gengc-round1-alloc, 2026-09-20 — `reset` clears the object-start bitmap
/// over the BUMPED spans only, and that bound is the high-water mark
/// rather than the live cursor.
///
/// `clear_all` was O(capacity) on every collection — one store per 512
/// bytes of `-Xmx`-sized from-space whether the nursery held ten objects
/// or ten million. Bounding it to what was handed out is the same change
/// `heap_bitmap::clear_within` was added for. The bound has to be
/// `low_high_water`, not `cursor`: a retraction moves the cursor away from
/// bytes whose bits are set, and a pass bounded by the post-retraction
/// cursor leaves them set — after which the next cycle's allocation over
/// that span is ACCEPTED at an interior address.
///
/// The two allocations are 1 KiB apart on purpose: one bitmap word covers
/// 512 arena bytes, so a smaller spacing would have both bits in the word
/// the narrow bound clears anyway and the test would pass either way.
#[test]
fn reset_clears_object_start_bits_the_cursor_retracted_past() {
    let mut a = Arena::new(1 << 20);
    a.arm_object_starts();
    if !a.tracks_object_starts() {
        return; // switch off in this environment; nothing to assert
    }
    let base = a.base_ptr() as usize;
    let p = a.alloc(1024, 8).expect("fresh arena has room");
    let q = a.alloc(1024, 8).expect("fresh arena has room");
    assert_eq!(a.is_object_start(p as usize), Some(true));
    assert_eq!(a.is_object_start(q as usize), Some(true));

    // Give the second allocation's span back to the bump tail. The cursor
    // no longer covers `q`; its start bit is untouched by the retraction.
    a.retract_cursor_to(q as usize - base);
    assert_eq!(
        a.is_object_start(q as usize),
        Some(true),
        "a retraction moves a cursor, not a bitmap",
    );

    a.reset();
    assert_eq!(a.is_object_start(p as usize), Some(false));
    assert_eq!(
        a.is_object_start(q as usize),
        Some(false),
        "a bound taken from the post-retraction cursor would leave this set, \
         and the next cycle would accept an interior address as a base",
    );
}

/// An arena nobody armed answers `None`, which is NOT `Some(false)`.
///
/// The distinction is the whole safety contract: `None` means "not tracked,
/// fall back to the deduction", and a caller that collapsed it to "not a
/// base" would reject every live root in an unarmed arena.
#[test]
fn an_unarmed_arena_answers_none_rather_than_false() {
    let mut a = Arena::new(4096);
    let p = a.alloc(64, 8).expect("fresh arena has room");
    assert!(!a.tracks_object_starts());
    assert_eq!(a.is_object_start(p as usize), None);
}

#[test]
fn free_blocks_sorted_spans_every_size_class() {
    let mut arena = Arena::new(64 * 1024);
    let cap = arena.capacity();
    arena.alloc(cap, 8).unwrap();
    arena.add_free_block(16 * 1024, 8 * 1024); // span tier
    arena.add_free_block(0, 4080); // top small class
    arena.add_free_block(8 * 1024, 40); // low small class
    let sorted = arena.free_blocks_sorted();
    assert_eq!(
        sorted,
        vec![(0, 4080), (8 * 1024, 40), (16 * 1024, 8 * 1024)]
    );
}

#[test]
fn grow_rearms_alloc_anchors_for_the_new_capacity() {
    let mut arena = Arena::new(4 * 1024);
    arena.grow(1024 * 1024);
    // The table must cover the grown capacity, or every offset past the
    // old end would silently fail to record.
    let last = arena.capacity() - 8;
    arena.note_object_start(last);
    assert_eq!(arena.take_alloc_anchors(), vec![last]);
}

/// zgc10: the pristine window hands a carver exactly the never-written
/// part of a hand-out, only when opted in, and only once.
#[test]
fn pristine_window_reports_only_never_written_bytes() {
    let mut arena = Arena::new(1 << 20);
    let base = arena.base_ptr() as usize;

    // Not opted in: never reported, though the window is tracked.
    let a = arena.alloc(4096, 16).expect("a");
    assert_eq!(arena.take_last_hand_out_pristine(a), None);

    arena.set_skip_pristine_zeroing(true);
    let b = arena.alloc(8192, 16).expect("b");
    let (lo, hi) = arena
        .take_last_hand_out_pristine(b)
        .expect("a fresh bump hand-out is pristine");
    assert_eq!((lo, hi), (b as usize, b as usize + 8192));
    // One-shot.
    assert_eq!(arena.take_last_hand_out_pristine(b), None);

    // Give everything back to the bump tail and allocate over it: the part
    // that was handed out before is NOT pristine any more, the rest is.
    let used = arena.cursor;
    assert_eq!(used, (b as usize + 8192) - base);
    arena.retract_cursor_to(0);
    let c = arena.alloc(used + 4096, 16).expect("c");
    assert_eq!(c as usize, base);
    let (lo, hi) = arena
        .take_last_hand_out_pristine(c)
        .expect("the tail above the old high-water mark is pristine");
    assert_eq!(lo, base + used, "bytes below the old cursor were written");
    assert_eq!(hi, base + used + 4096);

    // A mismatched pointer is refused.
    let d = arena.alloc(64, 16).expect("d");
    assert_eq!(arena.take_last_hand_out_pristine(a), None);
    let _ = d;
}

/// r16 zgcarray ZA1: after a slide, the allocation that hands its
/// zero-filled spans out again -- the tail above the new cursor and the
/// vacated spans it free-listed below it -- is told they already read
/// zero, except the forwarding records stamped into them; the set only
/// ever shrinks, and other re-listing does not falsify it.
/// zt t31/zc11 (`CRATONVM_ZGC_SLIDE_TAIL_ZERO_AT_ALLOCATION`): an owner
/// that defers the tail leaves `[new_cursor, old_cursor)` as it was and
/// does not list it as zeroed, so the hand-out that takes it is told to
/// zero it all.
#[test]
fn a_deferred_slide_tail_is_zeroed_by_its_hand_out() {
    const K: usize = 1024;
    let mut arena = Arena::new(1 << 20);
    arena.set_skip_pristine_zeroing(true);
    arena.set_defer_slide_tail_zeroing(true);
    let base = arena.base_ptr() as usize;
    let p = arena.alloc(64 * K, 16).expect("fill") as usize;
    // SAFETY: inside the hand-out above.
    unsafe { std::ptr::write_bytes((base + 4 * K) as *mut u8, 0xAB, 60 * K) };
    let _ = arena.take_last_hand_out_zero_plan(p as *const u8, 64 * K);
    arena.compact_low_to(44 * K, 0..4 * K, &[]);
    // Nothing is listed as zeroed.
    assert_eq!(arena.slide_zero_census(), (0, 0, 0));
    // SAFETY: inside the arena; the tail was not zeroed by the slide.
    let tail = unsafe { std::slice::from_raw_parts((base + 50 * K) as *const u8, 14 * K) };
    assert!(tail.iter().all(|&b| b == 0xAB), "the slide left the tail");
    // The bump hands the tail out with no known-zero part: zero it all.
    let used = arena.used_low_for_compaction();
    assert_eq!(used, 44 * K);
    let c = arena.alloc(20 * K, 16).expect("the tail") as usize;
    assert_eq!(c, base + 44 * K);
    let plan = arena.take_last_hand_out_zero_plan(c as *const u8, 20 * K);
    assert!(
        plan.as_ref().map_or(true, |p| p.known_zero.is_empty()),
        "nothing of the tail is claimed zero: {plan:?}"
    );
    // SAFETY: `c` is this hand-out's `20 K` bytes.
    unsafe { ZeroPlan::zero(plan.as_ref(), c as *mut u8, 20 * K) };
    let handed = unsafe { std::slice::from_raw_parts(c as *const u8, 20 * K) };
    assert!(handed.iter().all(|&b| b == 0), "the hand-out zeroed it");
}

#[test]
fn a_slides_zeroed_spans_are_reported_once_minus_their_stamps() {
    const K: usize = 1024;
    let mut arena = Arena::new(1 << 20);
    arena.set_skip_pristine_zeroing(true);
    let base = arena.base_ptr() as usize;
    let p = arena.alloc(64 * K, 16).expect("fill") as usize;
    assert_eq!(p, base);
    // Dirty everything the slide is about to vacate.
    // SAFETY: inside the hand-out above.
    unsafe { std::ptr::write_bytes((base + 4 * K) as *mut u8, 0xAB, 60 * K) };
    let _ = arena.take_last_hand_out_zero_plan(p as *const u8, 64 * K);

    // Survivors packed into [0, 4K) and one left at [40K, 44K): the slide
    // vacated [8K, 40K) below the new cursor and zeroes [44K, 64K) above.
    arena.compact_low_to(44 * K, 0..4 * K, &[(8 * K, 40 * K)]);
    assert_eq!(arena.slide_zero_census(), (2, 32 * K + 20 * K, 0));
    arena.note_slide_stamp(48 * K);
    arena.note_slide_stamp(46 * K);
    assert_eq!(arena.slide_zero_census().2, 2);

    // A sweep-style re-listing elsewhere does not falsify the set.
    arena.add_free_block(4 * K, 4 * K);
    assert_eq!(arena.slide_zero_census().0, 2);

    // The vacated span, served from the free list.
    let v = arena.alloc(16 * K, 16).expect("from the free list") as usize;
    let plan = arena
        .take_last_hand_out_zero_plan(v as *const u8, 16 * K)
        .expect("a vacated span is known zero");
    let (vlo, vhi) = (base + 8 * K, base + 40 * K);
    if v >= vlo && v + 16 * K <= vhi {
        assert_eq!(plan.known_zero, vec![(v, v + 16 * K)]);
        assert_eq!(plan.slide_bytes, 16 * K);
        assert!(plan.dirty_stamps.is_empty());
    }

    // The tail, served by the bump.
    let used = arena.used_low_for_compaction();
    assert_eq!(used, 44 * K);
    let c = arena.alloc(64 * K, 16).expect("a block or the tail") as usize;
    let plan = arena.take_last_hand_out_zero_plan(c as *const u8, 64 * K);
    if c == base + 44 * K {
        let plan = plan.expect("the slide's tail is known zero");
        assert_eq!(plan.known_zero[0].0, base + 44 * K);
        assert_eq!(plan.slide_bytes, 20 * K);
        assert_eq!(plan.dirty_stamps, vec![base + 46 * K, base + 48 * K]);
    }
    // SAFETY: the slide zero-filled [44K, 64K); read it back.
    let tail = unsafe { std::slice::from_raw_parts((base + 50 * K) as *const u8, 14 * K) };
    assert!(tail.iter().all(|&b| b == 0), "the set's claim is true");
    // One-shot.
    assert_eq!(
        arena.take_last_hand_out_zero_plan(c as *const u8, 64 * K),
        None
    );

    // A relocation destination inside the set shrinks it too.
    arena.compact_low_to(4 * K, 0..4 * K, &[]);
    let (_, before, _) = arena.slide_zero_census();
    assert!(arena.commit_for_relocation(20 * K, 4 * K));
    assert_eq!(arena.slide_zero_census().1, before - 4 * K);

    // Not opted in: a slide records nothing.
    let mut plain = Arena::new(1 << 20);
    let _ = plain.alloc(64 * K, 16);
    plain.compact_low_to(4 * K, 0..4 * K, &[]);
    assert_eq!(plain.slide_zero_census(), (0, 0, 0));
}

/// ZA1: a slide over bytes the arena already knows are zero leaves them
/// alone, but still clears a forwarding stamp written into them.
#[test]
fn a_slide_skips_known_zero_bytes_but_clears_their_stamps() {
    const K: usize = 1024;
    let mut arena = Arena::new(1 << 20);
    arena.set_skip_pristine_zeroing(true);
    let base = arena.base_ptr() as usize;
    let _ = arena.alloc(64 * K, 16).expect("fill");
    let _ = arena.take_last_hand_out_zero_plan(base as *const u8, 64 * K);
    arena.compact_low_to(4 * K, 0..4 * K, &[]);
    // A forwarding record lands in the zeroed tail.
    // SAFETY: inside the arena's committed low region.
    unsafe { std::ptr::write_bytes((base + 20 * K) as *mut u8, 0xCD, 16) };
    arena.note_slide_stamp(20 * K);
    // The next slide, from a cursor above the old one, sweeps over it.
    let more = arena.alloc(64 * K, 16).expect("bump") as usize;
    let _ = arena.take_last_hand_out_zero_plan(more as *const u8, 64 * K);
    // SAFETY: the bump hand-out above.
    unsafe { std::ptr::write_bytes(more as *mut u8, 0xEE, 64 * K) };
    let top = arena.used_low_for_compaction();
    arena.compact_low_to(4 * K, 0..4 * K, &[]);
    // SAFETY: inside the arena.
    let all = unsafe { std::slice::from_raw_parts((base + 4 * K) as *const u8, top - 4 * K) };
    assert!(
        all.iter().all(|&b| b == 0),
        "everything above the new cursor reads zero"
    );
}

/// zgc10: a relocation destination and a high-end descent both leave the
/// window, so nothing that was ever writable is reported as pristine.
#[test]
fn pristine_window_excludes_relocation_targets_and_the_high_end() {
    let mut arena = Arena::new(1 << 20);
    arena.set_skip_pristine_zeroing(true);
    let base = arena.base_ptr() as usize;
    let cap = arena.capacity();

    // A slide destination far above the cursor.
    assert!(arena.commit_for_relocation(64 * 1024, 4096));
    // A large object at the top.
    let h = arena.alloc_high(16 * 1024, 8).expect("high") as usize;
    assert!(h - base < cap);

    let p = arena.alloc(128 * 1024, 16).expect("low");
    match arena.take_last_hand_out_pristine(p) {
        None => {}
        Some((lo, hi)) => {
            let (lo, hi) = (lo - base, hi - base);
            assert!(lo < hi);
            let reloc = 64 * 1024..64 * 1024 + 4096;
            assert!(
                hi <= reloc.start || lo >= reloc.end,
                "a relocation destination must never read as pristine: [{lo:#x},{hi:#x})"
            );
            assert!(hi <= h - base, "the high end must never read as pristine");
        }
    }
    // Whatever was reported, the window itself stays clear of both.
    assert!(arena.pristine_hi <= h - base);
    assert!(
        arena.pristine_lo >= 64 * 1024 + 4096 || arena.pristine_hi <= 64 * 1024,
        "window [{:#x},{:#x}) overlaps the relocation target",
        arena.pristine_lo,
        arena.pristine_hi
    );
}
