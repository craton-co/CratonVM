use super::*;

/// gen r5w3/evac7 (L1): the span pool hands out buffers in span order,
/// never past a span's end, retires a span that cannot serve a minimal
/// request, and `dest_written` answers exactly "below its span's cursor".
/// Pure arithmetic over made-up addresses: nothing is read or written.
#[test]
fn the_in_place_span_pool_claims_in_order_and_knows_what_it_wrote() {
    let pinned = [(0x5000usize, 64usize), (0x5100, 32)];
    let dest = [
        (0x1000usize, 0x1000usize, 0x1100usize),
        (0x2000, 0x2000, 0x3000),
    ];
    let pool = ParInPlace::new(&pinned, &dest);
    assert!(pool.is_pinned_base(0x5000) && pool.is_pinned_base(0x5100));
    assert!(
        !pool.is_pinned_base(0x5008),
        "an interior word is not a base"
    );
    assert!(!pool.is_pinned_base(0x4ff8) && !pool.is_pinned_base(0x5200));

    // gcj yg3-residuals: the page filter answers exactly the binary
    // search over pinned bases spread across pages (a gap of empty pages,
    // a base at a page's last word, one on the first word of the next).
    let spread = [
        (0x10_0000usize, 64usize),
        (0x10_0ff8, 8),
        (0x10_1000, 16),
        (0x10_9010, 4096),
        (0x10_c000, 32),
    ];
    let pool2 = ParInPlace::new(&spread, &[]);
    for a in (0x0f_f000..0x10_e000usize).step_by(8) {
        assert_eq!(
            pool2.is_pinned_base(a),
            spread.iter().any(|&(b, _)| b == a),
            "address {a:#x}"
        );
    }

    // Span 0 has 256 bytes: a 4 KiB request takes all of it.
    assert_eq!(pool.claim(64, 4096), Some((0x1000, 0x1100)));
    assert!(pool.dest_written(0x1000) && pool.dest_written(0x10f8));
    assert!(!pool.dest_written(0x1100), "one past the claim");
    assert!(!pool.dest_written(0x2000), "span 1 is untouched");
    assert!(!pool.dest_written(0x0ff8), "below every span");
    // Span 0 is empty now (and retired); span 1 serves the next claim.
    assert_eq!(pool.claim(64, 1024), Some((0x2000, 0x2400)));
    assert_eq!(pool.span_next.load(Ordering::Relaxed), 1, "span 0 retired");
    assert!(pool.dest_written(0x23f8) && !pool.dest_written(0x2400));
    // An exact claim, then one that no longer fits anywhere.
    assert_eq!(pool.claim(0xC00, 0xC00), Some((0x2400, 0x3000)));
    assert_eq!(pool.claim(8, 8), None, "every span is spent");
    assert_eq!(pool.cursors(), vec![0x1100, 0x3000]);
}

#[test]
fn plan_runs_bufferless_rather_than_declining_when_slack_is_thin() {
    // Exactly enough room for the survivors and not a byte more. This is
    // very nearly the real case — a young GC triggers with from-space
    // ~99.9% full — and it must still parallelise, bufferless.
    let plan = ParEvac::plan(1024, 0x1000, 1024, 8).expect("zero slack still parallelises");
    assert_eq!(plan.workers, 8, "the worker ask is honoured");
    assert_eq!(
        plan.plab_bytes, 0,
        "no buffers: every object takes its own exact span"
    );
    assert_eq!(plan.waste_allowance, 0);

    // Slack too thin to be worth carving: still bufferless, not declined.
    let plan = ParEvac::plan(1024 + 8 * PLAB_FLOOR_BYTES, 0x1000, 1024, 8).expect("planned");
    assert_eq!(plan.plab_bytes, 0);

    // Ample slack: buffers, capped by what they are worth against the live
    // set rather than by the slack.
    let plan = ParEvac::plan(1024 + 64 * PLAB_MIN_BYTES, 0x1000, 1024, 8).expect("ample slack");
    assert!(plan.plab_bytes >= PLAB_FLOOR_BYTES);

    // The reservation, at every shape: buffers plus the abandoned-tail
    // allowance must fit inside the slack that exists. That sum IS what
    // keeps a worker from bumping past the arena.
    for (headroom, asked) in [
        (1024usize, 8usize),
        (1024 + 8 * PLAB_FLOOR_BYTES, 8),
        (1024 + 64 * PLAB_MIN_BYTES, 8),
        (1024 + 9 * PLAB_MIN_BYTES, 3),
    ] {
        let p = ParEvac::plan(headroom, 0x1000, 1024, asked).expect("planned");
        assert!(
            p.workers * p.plab_bytes + p.waste_allowance <= headroom - 1024,
            "reservation overruns the slack at plab={}",
            p.plab_bytes,
        );
    }

    // The one thing that still declines: to-space not covering from-space,
    // which the collection's own backstop should have caught first.
    assert!(ParEvac::plan(1023, 0x1000, 1024, 8).is_none());
}

/// The shape a real young collection actually has, as measured.
///
/// `bench/BinT.java` at depth 18, `-Xmx512m`, first moving cycle:
/// `to_headroom = 134,217,728` against `from_used = 134,096,736`. A young
/// GC triggers with from-space **99.91% full**, so the Cheney invariant's
/// `to_headroom >= from_used` leaves 120,992 bytes and nothing else.
///
/// The first budget worst-cased abandoned tails at `from_used / 7` — 19 MB
/// — and declined here, which means it would have declined on every cycle
/// of every real workload. Every unit test stayed green because each sizes
/// its to-space generously; nothing but a real run could have shown it.
/// This test is that run, frozen.
#[test]
fn plan_accepts_the_measured_shape_of_a_real_young_collection() {
    const TO_HEADROOM: usize = 134_217_728;
    const FROM_USED: usize = 134_096_736;
    let plan = ParEvac::plan(TO_HEADROOM, 0x1000, FROM_USED, 8)
        .expect("a real cycle's 121 KiB of slack must be enough to parallelise");
    assert!(plan.plab_bytes >= PLAB_MIN_BYTES);
    // The reservation is the whole point: buffers plus the allowance must
    // fit in the slack, or a worker could bump past the arena.
    let slack = TO_HEADROOM - FROM_USED;
    assert!(
        8 * plan.plab_bytes + plan.waste_allowance <= slack,
        "reservation {} exceeds the {slack} bytes of slack that exist",
        8 * plan.plab_bytes + plan.waste_allowance,
    );
}

#[test]
fn plan_declines_a_misaligned_to_space_cursor() {
    // Plenty of headroom, but the pad an aligned start would leave is too
    // small for any sentinel, so nothing could make it walkable.
    let head = 4096 + 4096 / 7 + 2 * PLAB_MAX_BYTES + 64;
    assert!(ParEvac::plan(head, 0x1004, 4096, 2).is_none());
    let plan = ParEvac::plan(head, 0x1008, 4096, 2).expect("an aligned cursor is accepted");
    assert_eq!(plan.region_start, 0x1008);
    // The workers' ceiling is the RESERVATION, not the whole tail: the
    // backing store maps lazily, and only `reserved` bytes get committed.
    assert_eq!(plan.region_end, 0x1008 + plan.reserved);
    assert!(
        plan.reserved <= head,
        "the reservation must fit inside the tail it was cut from",
    );
    assert!(
        plan.reserved >= 4096,
        "it must at least cover the survivors",
    );
}

/// The per-worker buffer must track the live set, not sit at a constant.
///
/// Measured cost of the constant: eight workers on a 393 KiB live set
/// retired 327 KiB of filler — nearly a byte of dead to-space per byte of
/// survivor — because each held a 64 KiB buffer it could not fill. The
/// clamp is what keeps both ends sane: a tiny cycle does not reserve
/// megabytes, and a huge one does not degrade into a `fetch_add` per
/// object.
#[test]
fn the_buffer_size_tracks_the_live_set_between_its_two_clamps() {
    let big = 1024 * 1024 * 1024;
    let plan = |from_used: usize, workers: usize| {
        ParEvac::plan(big, 0x1000, from_used, workers)
            .expect("a gigabyte of headroom covers every case here")
            .plab_bytes
    };
    // Tiny live set: the floor, not `from_used / 32`.
    assert_eq!(plan(64 * 1024, 8), PLAB_MIN_BYTES);
    // Huge live set: the ceiling, not `from_used / 32`.
    assert_eq!(plan(512 * 1024 * 1024, 8), PLAB_MAX_BYTES);
    // In between: proportional, and 8-aligned so every buffer base stays
    // on the object grid.
    let mid = plan(512 * 1024, 8);
    assert_eq!(mid, (512 * 1024 / 8 / PLAB_LIVE_DIVISOR) & !7);
    assert!(mid > PLAB_MIN_BYTES && mid < PLAB_MAX_BYTES);
    assert_eq!(mid % 8, 0);
    // More workers on the same live set means smaller buffers each, so the
    // total reserved does not grow with the worker count.
    assert!(plan(64 * 1024 * 1024, 16) <= plan(64 * 1024 * 1024, 4));
}

/// Everything a `ParEvac` needs, kept alive for the test's duration.
struct Fixture {
    from: cratonvm_gc_core::arena::Arena,
    to: cratonvm_gc_core::arena::Arena,
    old: OldGen,
    obj: *mut u8,
}

/// One legitimate single-slot object in a fresh from-space.
fn fixture() -> Fixture {
    let mut from = cratonvm_gc_core::arena::Arena::new(64 * 1024);
    let to = cratonvm_gc_core::arena::Arena::new(512 * 1024);
    let old = OldGen::new(64 * 1024);
    let size = HEADER_SIZE + cratonvm_gc_core::heap::SLOT_SIZE;
    let obj = from.alloc(size, 8).expect("fresh arena serves one object");
    let header = ObjectHeader::new(
        cratonvm_types::ClassId::new(5),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        1,
    );
    // SAFETY: `obj` is a fresh `size`-byte allocation, 8-aligned.
    unsafe {
        std::ptr::write(obj as *mut ObjectHeader, header);
        std::ptr::write_bytes(obj.add(HEADER_SIZE), 0, cratonvm_gc_core::heap::SLOT_SIZE);
    }
    Fixture { from, to, old, obj }
}

/// Serialises the two tests that deliberately LOSE a forwarding CAS.
///
/// Both read [`EVAC_CAS_LOSSES`] before and after their own call and
/// assert an exact delta, and the counter is process-global while the
/// harness runs tests on parallel threads — so without this lock the
/// second test to be added silently turns the first into a flake. (The
/// alternative, relaxing both to `>=`, would throw away the one assertion
/// that proves the loser arm ran at all.) gengc-round1 2026-09-20.
static CAS_LOSS_ARM: Mutex<()> = Mutex::new(());

/// `evacuate`'s forwarding-CAS LOSER must adopt the winner's address AND
/// record the forward.
///
/// Recording is the half that is easy to leave out and impossible to
/// notice: the loser already has the right address to hand back, so the
/// object graph comes out correct and only the ROOT SET is wrong — a root
/// still naming the from-space copy, unremappable because `pointer_map`
/// has no key for it, dangling the moment from-space is reset. That is
/// verbatim the defect G1's identical arm carried.
#[test]
fn a_forwarding_cas_loser_adopts_the_winner_and_records_the_forward() {
    let _serial = CAS_LOSS_ARM.lock();
    let mut fx = fixture();
    let mut starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    assert!(starts.insert(fx.obj as usize));

    // The address the "winning" worker installs while we are mid-copy.
    // Any plausible, 8-aligned address inside a destination arena will do.
    let winner_addr = fx.to.base_ptr() as usize + 256 * 1024;
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
        .expect("512 KiB of to-space covers one object plus a buffer");
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    );

    let losses_before = EVAC_CAS_LOSSES.load(Ordering::Relaxed);
    let _guard = set_race_hook(Box::new(move |src: *mut u8| {
        // SAFETY: `src` is the from-space object `evacuate` is copying.
        let h = unsafe { &*(src as *const ObjectHeader) };
        h.set_forwarding_address(winner_addr as *mut u8);
    }));

    let mut shard = EvacShard::default();
    // SAFETY: `fx.obj` is an aligned object start inside from-space.
    let out = unsafe { evac.evacuate(&mut shard, fx.obj) };

    assert_eq!(
        out as usize, winner_addr,
        "the loser must converge on the winner's address, not its own copy",
    );
    assert!(
        shard.forwards.contains(&(fx.obj as usize, winner_addr)),
        "the loser's forward went unrecorded — a root naming {:#x} could \
         never be remapped",
        fx.obj as usize,
    );
    assert_eq!(
        shard.objects_copied, 0,
        "an abandoned speculative copy must not be counted as a survivor",
    );
    assert_eq!(
        EVAC_CAS_LOSSES.load(Ordering::Relaxed),
        losses_before + 1,
        "the loser arm's own counter did not move, so this test proved nothing",
    );
}

/// gcj j2/x1: an in-place claim whose self-forward CAS loses to a
/// NON-forwarded word (a peer's age / flag change in the quartet since
/// `observed` was read) retries from that word: the object is claimed,
/// queued for its scan and recorded with the word the claim replaced.
/// Before, it was handed back unclaimed: a pinned seed kept but never
/// traced, a kept survivor left out of `in_place_live`.
#[test]
fn gcj_j2x1_an_in_place_claim_lost_to_a_quartet_change_retries() {
    let mut fx = fixture();
    let mut starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    assert!(starts.insert(fx.obj as usize));
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
        .expect("512 KiB of to-space covers one object plus a buffer");
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    );
    // SAFETY: `fx.obj` is the fixture's live, aligned object start.
    let hdr = unsafe { &*(fx.obj as *const ObjectHeader) };
    for kept in [None, Some(HEADER_SIZE + cratonvm_gc_core::heap::SLOT_SIZE)] {
        let stale = hdr.mark();
        assert!(!ObjectHeader::is_forwarded_mark(stale), "{kept:?}");
        // A peer's age change after `stale` was read.
        hdr.set_gc_age(ObjectHeader::gc_age_of(stale).wrapping_add(1) & 1);
        let fresh = hdr.mark();
        assert_ne!(fresh, stale, "the fixture changed the quartet");
        let mut shard = EvacShard::default();
        // SAFETY: as above; the shard is this test's own.
        let out = unsafe { evac.claim_in_place(&mut shard, hdr, fx.obj, stale, kept) };
        let a = fx.obj as usize;
        assert_eq!(out as usize, a, "{kept:?}: claimed where it is");
        assert!(hdr.is_self_forwarded(), "{kept:?}: the retry won the CAS");
        assert!(shard.work.contains(&a), "{kept:?}: queued for its scan");
        assert!(shard.forwards.contains(&(a, a)), "{kept:?}: identity pair");
        match kept {
            None => assert_eq!(shard.in_place_pinned, vec![(a, fresh)]),
            Some(size) => {
                assert_eq!(shard.in_place_kept, vec![(a, fresh)]);
                assert_eq!(shard.in_place_live, vec![(a, size)]);
            }
        }
        hdr.restore_self_forwarded_mark(fresh);
    }
}

/// gcj j3/x2 (fail-first: before, `evacuate` returned `old_ptr` with no
/// forward and no pair): a copy whose forwarding CAS loses to a
/// NON-forwarded word (a quartet change after the snapshot) starts over
/// and forwards the object, as `claim_in_place` retries.
#[test]
fn gcj3x2_a_copy_lost_to_a_quartet_change_starts_over_and_forwards() {
    let _serial = CAS_LOSS_ARM.lock();
    let mut fx = fixture();
    let starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    assert!(starts.insert(fx.obj as usize));
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    );
    let fired = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let hook_fired = std::rc::Rc::clone(&fired);
    let _guard = set_race_hook(Box::new(move |src: *mut u8| {
        hook_fired.set(hook_fired.get() + 1);
        if hook_fired.get() == 1 {
            // SAFETY: `src` is the from-space object `evacuate` is copying.
            let h = unsafe { &*(src as *const ObjectHeader) };
            // A peer's age change between the snapshot and the claim.
            h.set_gc_age(ObjectHeader::gc_age_of(h.mark()).wrapping_add(1) & 1);
        }
    }));
    let mut shard = EvacShard::default();
    // SAFETY: `fx.obj` is an aligned object start inside from-space.
    let out = unsafe { evac.evacuate(&mut shard, fx.obj) } as usize;
    let a = fx.obj as usize;
    assert_eq!(fired.get(), 2, "the hook ran on the lost claim and on the retry");
    assert_ne!(out, a, "copied, not left where it was");
    // SAFETY: as above.
    let hdr = unsafe { &*(fx.obj as *const ObjectHeader) };
    assert!(hdr.is_forwarded(), "the retry won the claim");
    assert_eq!(hdr.forwarding_address() as usize, out);
    assert!(shard.forwards.contains(&(a, out)), "the pair is recorded");
    assert_eq!(shard.tally[5], 1, "one copy won");
}

/// A forwarding-CAS loser that had already PROMOTED its speculative copy
/// must leave a reference-free filler behind, not a live-looking object.
///
/// gengc-round1 2026-09-20. In young to-space an abandoned copy is inert:
/// nothing names it and next cycle's sweep reclaims it. In OLD gen it is
/// not, because `scan_dirty_cards` is liveness-blind — it reads the
/// reference slots of every object whose header starts in a dirty card. An
/// abandoned promotion's slots still hold THIS cycle's from-space
/// addresses, which become live-looking again two cycles later when the
/// semispaces swap back. The filler removes the slots while keeping the
/// block exactly as walkable (and exactly as reclaimable) as before.
#[test]
fn a_cas_loser_that_promoted_stamps_a_filler_over_the_abandoned_block() {
    let _serial = CAS_LOSS_ARM.lock();
    let mut fx = fixture();
    // `ObjectStartBits::insert` takes `&self`, so no `mut` is needed here.
    let starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    assert!(starts.insert(fx.obj as usize));
    let winner_addr = fx.to.base_ptr() as usize + 256 * 1024;
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
    let promoted_size = HEADER_SIZE + cratonvm_gc_core::heap::SLOT_SIZE;
    let abandoned_before = PAR_EVAC_ABANDONED_OLD_BYTES.load(Ordering::Relaxed);
    {
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            // force_promote_all: send the speculative copy to OLD gen,
            // which is the arm this test exists for.
            true,
            3,
            false,
            false,
        );
        let guard = set_race_hook(Box::new(move |src: *mut u8| {
            // SAFETY: `src` is the from-space object `evacuate` is copying.
            let h = unsafe { &*(src as *const ObjectHeader) };
            h.set_forwarding_address(winner_addr as *mut u8);
        }));
        let mut shard = EvacShard::default();
        // SAFETY: `fx.obj` is an aligned object start inside from-space.
        let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
        assert_eq!(
            out as usize, winner_addr,
            "the loser must still converge on the winner's address",
        );
        assert_eq!(
            shard.tally[1], 0,
            "an abandoned promotion is not a survivor"
        );
        drop(guard);
        // Hand the promotion buffer's never-written tail back so the walk
        // below sees the abandoned block and nothing else.
        evac.retire_plab(&mut shard);
    }
    assert_eq!(
        PAR_EVAC_ABANDONED_OLD_BYTES.load(Ordering::Relaxed),
        abandoned_before + promoted_size as u64,
        "the abandoned-promotion arm's own counter did not move, so this \
         test proved nothing",
    );

    let objects = fx.old.walk_objects();
    assert_eq!(
        objects.len(),
        1,
        "the old-gen walk must still stride the abandoned block as one object",
    );
    let (ptr, size) = objects[0];
    assert_eq!(
        size, promoted_size,
        "the filler must consume the abandoned block exactly, or `compact`'s \
         walked-bytes-vs-used-bytes check abandons every future compaction",
    );
    // SAFETY: `walk_objects` yielded this as a valid object start.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    assert_eq!(header.class_id, cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID);
    assert_eq!(header.kind(), ObjectKind::Array);
    assert_eq!(
        header.element_type(),
        ArrayElementType::Int,
        "an int[] has no reference slots for the dirty-card scan to read",
    );
}

/// Rotating a worker's YOUNG to-space buffer must not cost it its OLD-gen
/// promotion buffer.
///
/// gengc-round1 2026-09-20. `plab_alloc`'s refill path called
/// `retire_plab`, which also returns the promotion buffer's tail to the
/// old generation — under the old-gen mutex, on the young allocation path.
/// Every to-space buffer rotation therefore took the collector's one
/// shared lock and forced the next promotion to take it again to carve a
/// replacement buffer, which is precisely the per-object mutex traffic the
/// promotion buffer was introduced to remove.
#[test]
fn rotating_the_young_buffer_keeps_the_promotion_buffer() {
    let mut fx = fixture();
    let starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
        .expect("512 KiB of to-space buys buffers");
    assert!(
        plan.plab_bytes > 0,
        "this test needs a BUFFERED cycle; a bufferless one never rotates",
    );
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    );
    let mut shard = EvacShard::default();

    // Carve a promotion buffer and remember exactly where it stands.
    let promoted = evac
        .promote_alloc(&mut shard, 32)
        .expect("a fresh old gen serves a promotion buffer");
    assert!(evac.in_old(promoted));
    let (old_cursor, old_end) = (shard.old_plab.cursor, shard.old_plab.end);
    assert!(old_end > old_cursor, "the promotion buffer is live");

    // Carve the young buffer, then fill it until it has to rotate.
    evac.plab_alloc(&mut shard, 48)
        .expect("the first object carves the to-space buffer");
    let first_plab_end = shard.plab.end;
    assert!(first_plab_end > 0);
    let mut rotated = false;
    for _ in 0..1024 {
        evac.plab_alloc(&mut shard, 48)
            .expect("the reservation covers these objects");
        if shard.plab.end != first_plab_end {
            rotated = true;
            break;
        }
    }
    assert!(rotated, "the buffer never rotated, so nothing was proved");
    assert!(
        !shard.plab_gaps.is_empty(),
        "the abandoned tail must be published for filling, or the arena \
         stops being walkable as next cycle's from-space",
    );
    assert_eq!(
        (shard.old_plab.cursor, shard.old_plab.end),
        (old_cursor, old_end),
        "the promotion buffer must survive a young-buffer rotation untouched",
    );
}

/// The already-forwarded FAST path must record too, for the same reason —
/// when the target is in OLD gen, where it may be a selective-promotion
/// forward from an earlier non-moving cycle that nothing else records.
///
/// gen r4w4/young4 (2026-09-24): a YOUNG to-space target is not
/// re-recorded. Every to-space forward is installed by a CAS winner of this
/// very phase, whose own push is merged; the fast path's copy was a
/// duplicate pushed once per re-encountered edge. Both halves are pinned
/// here.
#[test]
fn an_already_forwarded_object_still_records_its_forward() {
    // -> the shard's forwards after one fast-path hit on `target(fx)`.
    fn fast_path_forwards(
        target_of: fn(&Fixture) -> usize,
    ) -> (usize, usize, Vec<(usize, usize)>) {
        let mut fx = fixture();
        let mut starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
        );
        assert!(starts.insert(fx.obj as usize));
        let target = target_of(&fx);
        // SAFETY: `fx.obj` carries a valid header written by `fixture`.
        unsafe {
            (*(fx.obj as *const ObjectHeader)).set_forwarding_address(target as *mut u8);
        }

        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            false,
            3,
            false,
            false,
        );
        let mut shard = EvacShard::default();
        // SAFETY: as above.
        let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
        assert_eq!(out as usize, target, "the fast path hands back the forward");
        assert_eq!(shard.objects_copied, 0, "nothing was copied");
        assert_eq!(shard.tally[4], 1, "counted as a re-encounter either way");
        (fx.obj as usize, target, shard.forwards)
    }

    // An OLD-gen target: recorded.
    let (obj, target, forwards) = fast_path_forwards(|fx| fx.old.extent().0 + 64);
    assert_eq!(forwards, vec![(obj, target)]);

    // A young TO-space target: its winner recorded it; no duplicate.
    let (_, _, forwards) = fast_path_forwards(|fx| fx.to.base_ptr() as usize + 128 * 1024);
    assert!(
        forwards.is_empty(),
        "a to-space forward is this phase's own and already recorded by its winner",
    );
}

/// gcj j3/x2 R9 (pn2 item 16): a refusal of a PINNED base past the
/// per-shard cap still reaches the driver (`refused_pinned_past_cap`), so
/// `CRATONVM_GEN_PINNED_REFUSED_FORWARD_KEPT` keeps it; a non-pinned one
/// past the cap is dropped, as before.
#[test]
fn gcj3x2_r9_a_pinned_refusal_past_the_cap_still_reaches_the_driver() {
    let mut fx = fixture();
    let starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
    // Pure bookkeeping: `note_refusal` reads no byte at these addresses.
    let pinned_base = 0x7000_0000usize;
    let ip = ParInPlace::new(&[(pinned_base, 64)], &[]);
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    )
    .with_in_place(&ip);
    let mut shard = EvacShard::default();
    for i in 0..REFUSALS_PER_SHARD {
        evac.note_refusal(&mut shard, 0x100 + i * 8, "suspect-header");
    }
    evac.note_refusal(&mut shard, 0x9999_0000, "suspect-header");
    evac.note_refusal(&mut shard, pinned_base, "suspect-header");
    assert_eq!(
        shard.refusals.len(),
        REFUSALS_PER_SHARD,
        "the ledger's cap holds"
    );
    assert_eq!(shard.refused_pinned_past_cap, vec![pinned_base]);
    // Under the cap a pinned refusal is in `refusals` only.
    let mut under = EvacShard::default();
    evac.note_refusal(&mut under, pinned_base, "suspect-header");
    assert_eq!(under.refusals, vec![(pinned_base, "suspect-header")]);
    assert!(under.refused_pinned_past_cap.is_empty());
}

/// gcj j3/x2 R9: `claim_in_place`'s retry keeps going only for a word of
/// the same shape (a flag, age or payload change); another kind, compact
/// bit or array element tag is refused.
#[test]
fn gcj3x2_r9_the_claim_retry_rescreens_the_shape() {
    let kind = |k: ObjectKind| (k as u32) << 16; // the kind tag's bits (16..17)
    let compact = u32::from(cratonvm_gc_core::heap::GC_FLAG_COMPACT) << 24;
    let arr = |elem: u32| kind(ObjectKind::Array) | (elem << 18);
    let obj = kind(ObjectKind::Object);
    assert!(same_mark_shape(arr(3), arr(3) | 5), "payload bits");
    assert!(!same_mark_shape(arr(3), arr(4)), "element tag");
    assert!(!same_mark_shape(obj, arr(3)), "kind");
    assert!(!same_mark_shape(obj, obj | compact), "compact bit");
    assert!(
        same_mark_shape(obj | compact | (5 << 18), obj | compact | (9 << 18)),
        "a compact instance's hash bits are not a tag"
    );
}

/// A forwarding word left over from an earlier cycle can hold 8-aligned
/// non-null GARBAGE (BUG-Z). Following it would send the root remap into
/// freed memory, so it has to be refused — the object is left unmoved,
/// exactly as the serial evacuator's identical guard does.
#[test]
fn a_forwarding_target_outside_both_destinations_is_refused() {
    let mut fx = fixture();
    let mut starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    assert!(starts.insert(fx.obj as usize));
    // SAFETY: `fx.obj` carries a valid header written by `fixture`.
    unsafe {
        (*(fx.obj as *const ObjectHeader)).set_forwarding_address(0x1110 as *mut u8);
    }

    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    );
    let mut shard = EvacShard::default();
    // SAFETY: as above.
    let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
    assert_eq!(out, fx.obj, "a bogus forward must leave the object unmoved");
    assert!(shard.forwards.is_empty(), "and must not be recorded");
}

/// An address that is inside from-space and 8-aligned but is NOT an object
/// start — an interior word arriving from a conservative root — must be
/// handed straight back. Forwarding through it would install a pointer in
/// the middle of a live object.
#[test]
fn an_interior_address_is_refused_without_writing_anything() {
    let mut fx = fixture();
    let mut starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    assert!(starts.insert(fx.obj as usize));
    // SAFETY: `fx.obj + 8` is inside the object's own allocation.
    let interior = unsafe { fx.obj.add(8) };

    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    );
    // SAFETY: `interior` is a readable, aligned address inside from-space.
    let mut shard = EvacShard::default();
    let out = unsafe { evac.evacuate(&mut shard, interior) };
    assert_eq!(out, interior);
    assert!(shard.forwards.is_empty());
    // gengc-round4-move: and the refusal is RECORDED, in the serial
    // evacuator's vocabulary, for the driver to put in the cycle's ledger.
    assert_eq!(
        shard.refusals,
        vec![(interior as usize, "not-an-object-start")],
        "a parallel refusal must reach the same ledger a serial one does",
    );
    // SAFETY: the object header is intact if nothing was written through it.
    let h = unsafe { &*(fx.obj as *const ObjectHeader) };
    assert!(!h.is_forwarded(), "the real object must be untouched");
}

/// gengc-round4-move: the bufferless lever's plan keeps the worker count,
/// drops every buffer, and reserves exactly the survivors — the
/// reservation a thin-slack cycle already runs on.
#[test]
fn a_bufferless_plan_keeps_the_workers_and_reserves_exactly_the_survivors() {
    const FROM_USED: usize = 1024 * 1024;
    let plan = ParEvac::plan(64 * 1024 * 1024, 0x1000, FROM_USED, 8).expect("ample slack");
    assert!(
        plan.plab_bytes > 0,
        "precondition: a BUFFERED plan to strip"
    );
    let workers = plan.workers;
    let b = plan.into_bufferless();
    assert_eq!(b.workers, workers, "the lever must not change the width");
    assert_eq!(b.plab_bytes, 0);
    assert_eq!(b.waste_allowance, 0);
    assert_eq!(b.reserved, FROM_USED, "exactly the survivors");
    assert_eq!(b.region_start, 0x1000);
    assert_eq!(b.region_end, 0x1000 + FROM_USED);
}

/// gengc-round4-move: the shared to-space cursor never moves past the
/// region, so a refusal is exact and cannot be manufactured by a peer's
/// failing request (the `fetch_add`/`fetch_sub` undo it replaced left the
/// cursor inflated for the duration of every failed bump).
#[test]
fn the_shared_cursor_never_leaves_the_region() {
    let mut fx = fixture();
    let starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
        .expect("plan")
        .into_bufferless();
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        false,
        3,
        false,
        false,
    );
    let first = evac.bump_shared(8).expect("the region holds the survivors");
    assert_eq!(first, plan.region_start);
    // More than what is left: refused, and the cursor did not move.
    assert!(evac.bump_shared(plan.reserved).is_none());
    assert_eq!(evac.to_cursor_end(), plan.region_start + 8);
    // Exactly what is left: served, and the cursor lands on the end.
    let rest = plan.reserved - 8;
    assert!(evac.bump_shared(rest).is_some());
    assert_eq!(evac.to_cursor_end(), plan.region_end);
    // Nothing left at all.
    assert!(evac.bump_shared(8).is_none());
    assert_eq!(
        evac.to_cursor_end(),
        plan.region_end,
        "a refusal must not move the cursor"
    );
}

/// gengc-round4-move: a young survivor that finds the reserved to-space
/// region exhausted is TENURED, not a `process::abort()`.
///
/// The region can run out on a near-total-survival cycle because a
/// forwarding-CAS loser's abandoned copy spends bytes `plan` never
/// budgeted. This drives the exhausted state directly.
#[test]
fn an_exhausted_to_space_region_tenures_instead_of_aborting() {
    let mut fx = fixture();
    let starts =
        cratonvm_gc_core::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
    assert!(starts.insert(fx.obj as usize));
    let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
        .expect("plan")
        .into_bufferless();
    let evac = ParEvac::new(
        fx.from.base_ptr() as usize,
        fx.from.used(),
        fx.from.capacity(),
        &starts,
        fx.to.base_ptr() as usize,
        fx.to.capacity(),
        &plan,
        &mut fx.old,
        // Not force-promoted, and the fixture object is age 0 against a
        // promotion age of 3: it WANTS young to-space.
        false,
        3,
        false,
        false,
    );
    // Spend the whole region, as abandoned CAS-loser copies would.
    evac.bump_shared(plan.reserved)
        .expect("the region is exactly this big");
    let before = PAR_EVAC_OVERFLOW_PROMOTIONS.load(Ordering::Relaxed);
    let mut shard = EvacShard::default();
    // SAFETY: `fx.obj` is an aligned object start inside from-space.
    let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
    assert_ne!(out, fx.obj, "the object must still be relocated");
    assert!(
        evac.in_old(out as usize),
        "with to-space spent, the survivor must land in old gen",
    );
    // SAFETY: `out` is the object `evacuate` just wrote.
    let h = unsafe { &*(out as *const ObjectHeader) };
    assert!(
        h.is_old_gen(),
        "an early-tenured copy must carry the old-gen flag"
    );
    assert_eq!(shard.tally[1], 1, "and be tallied as a promotion");
    assert!(
        PAR_EVAC_OVERFLOW_PROMOTIONS.load(Ordering::Relaxed) > before,
        "the overflow arm's own counter did not move",
    );
    evac.retire_plab(&mut shard);
}

/// gengc-round4-move: a worker that UNWINDS out of the drain must not
/// strand its peers on the termination handshake.
///
/// Termination waits for every worker to register idle; a panicking worker
/// never does, so before `WorkerExit` the survivors — the driver included
/// — waited on `cv` forever, inside a stop-the-world pause, with the panic
/// that caused it never re-raised. Here worker 1 of 2 panics (via the
/// race-hook seam, mid-`evacuate`) and worker 2 must still come home.
#[test]
fn a_worker_that_unwinds_does_not_strand_its_peers() {
    use cratonvm_types::{ObjectRef, Value};
    let mut from = cratonvm_gc_core::arena::Arena::new(64 * 1024);
    let to = cratonvm_gc_core::arena::Arena::new(512 * 1024);
    let mut old = OldGen::new(64 * 1024);
    let size = HEADER_SIZE + cratonvm_gc_core::heap::SLOT_SIZE;
    let parent = from.alloc(size, 8).expect("parent");
    let child = from.alloc(size, 8).expect("child");
    for (p, cid) in [(parent, 5u32), (child, 6u32)] {
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(cid),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            1,
        );
        // SAFETY: `p` is a fresh `size`-byte allocation, 8-aligned.
        unsafe {
            std::ptr::write(p as *mut ObjectHeader, header);
            std::ptr::write_bytes(p.add(HEADER_SIZE), 0, cratonvm_gc_core::heap::SLOT_SIZE);
        }
    }
    // parent.field0 = child (a legacy 16-byte `Value` cell).
    // SAFETY: the cell is inside `parent`'s body; `child` is a live start.
    unsafe {
        std::ptr::write(
            parent.add(HEADER_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(child))),
        );
    }
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(from.base_ptr() as usize, from.used());
    assert!(starts.insert(parent as usize));
    assert!(starts.insert(child as usize));
    let (to_cursor, to_headroom) = to.parallel_evacuation_region();
    let plan = ParEvac::plan(to_headroom, to_cursor, from.used(), 2).expect("plan");
    let evac = ParEvac::new(
        from.base_ptr() as usize,
        from.used(),
        from.capacity(),
        &starts,
        to.base_ptr() as usize,
        to.capacity(),
        &plan,
        &mut old,
        false,
        3,
        false,
        false,
    );

    // Copy the parent (no hook yet), then publish its copy as the only
    // work: whoever scans it evacuates the child and hits the hook.
    let mut s0 = EvacShard::default();
    // SAFETY: `parent` is an aligned object start inside from-space.
    unsafe { evac.evacuate(&mut s0, parent) };
    assert_eq!(s0.work.len(), 1, "the parent's copy is queued for scanning");
    evac.shared.lock().stack.append(&mut s0.work);

    let hook = set_race_hook(Box::new(|_| panic!("injected evacuation fault")));
    let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the only queued address is the parent's fresh copy.
        unsafe { evac.run_worker(&mut s0, 2, None) }
    }));
    drop(hook);
    assert!(died.is_err(), "precondition: worker 1 must have unwound");
    assert_eq!(evac.shared.lock().dead, 1, "the unwind was not registered");

    let (tx, rx) = std::sync::mpsc::channel();
    let evac_ref = &evac;
    std::thread::scope(|sc| {
        sc.spawn(move || {
            let mut s1 = EvacShard::default();
            // SAFETY: the shared stack is empty; nothing is scanned.
            unsafe { evac_ref.run_worker(&mut s1, 2, None) };
            let _ = tx.send(());
        });
        let came_home = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
        if !came_home {
            // Release the stranded worker so the scope can join, then fail.
            evac_ref.shared.lock().done = true;
            evac_ref.cv.notify_all();
        }
        assert!(
            came_home,
            "the surviving worker waited forever for a peer that had unwound"
        );
    });
}

/// gcf f3/y: a worker's destination-span buffer doubles per refill, 4 KiB
/// to 32 KiB, and a small span still yields only what it has left.
#[test]
fn f3y_the_in_place_buffer_doubles_to_its_cap() {
    // Pure cursor arithmetic: no byte of these spans is touched.
    let start = 0x10_0000usize;
    let ip = ParInPlace::new(&[], &[(start, start, start + (1 << 20))]);
    let mut shard = EvacShard::default();
    for _ in 0..960 {
        ParEvac::in_place_alloc(&ip, &mut shard, 64).expect("room");
    }
    // 4 + 8 + 16 + 32 KiB claimed, each filled exactly by 64-byte objects.
    assert_eq!(ip.cursors()[0] - start, 60 * 1024);
    ParEvac::in_place_alloc(&ip, &mut shard, 64).expect("room");
    assert_eq!(
        ip.cursors()[0] - start,
        92 * 1024,
        "the fifth buffer stays at the cap"
    );

    // A 1 KiB span serves a buffer of at most 1 KiB however large the want.
    let small = ParInPlace::new(&[], &[(start, start, start + 1024)]);
    let mut s2 = EvacShard::default();
    s2.next_ip_plab = 32 * 1024;
    assert!(ParEvac::in_place_alloc(&small, &mut s2, 64).is_some());
    assert_eq!(small.cursors()[0] - start, 1024);
}

/// gcf f2/y: the worker's age table counts a young copy once, by the CAS
/// winner, at its new age -- and nothing for a CAS loser, whose repeated
/// forward pair the driver used to count a second time. Off unless asked.
#[test]
fn f2y_the_shard_age_table_counts_the_winner_once_and_never_the_loser() {
    let _serial = CAS_LOSS_ARM.lock();
    for (lose, census) in [(false, true), (true, true), (false, false)] {
        let mut fx = fixture();
        let mut starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
        );
        assert!(starts.insert(fx.obj as usize));
        let size = crate::gen_heap::gen_object_total_size(
            // SAFETY: `fixture` wrote a valid header at `fx.obj`.
            unsafe { &*(fx.obj as *const ObjectHeader) },
        );
        let winner_addr = fx.to.base_ptr() as usize + 256 * 1024;
        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            false,
            3,
            false,
            false,
        )
        .with_age_census(census);
        let _guard = lose.then(|| {
            set_race_hook(Box::new(move |src: *mut u8| {
                // SAFETY: `src` is the from-space object being copied.
                let h = unsafe { &*(src as *const ObjectHeader) };
                h.set_forwarding_address(winner_addr as *mut u8);
            }))
        });
        let mut shard = EvacShard::default();
        // SAFETY: `fx.obj` is an aligned object start inside from-space.
        unsafe { evac.evacuate(&mut shard, fx.obj) };
        let total: u64 = shard.ages.iter().sum();
        match (lose, census) {
            (false, true) => {
                assert_eq!(shard.ages[1], size as u64, "one copy, at its new age 1");
                assert_eq!(total, size as u64);
            }
            (true, true) => assert_eq!(total, 0, "a CAS loser counted its object"),
            _ => assert_eq!(total, 0, "the census is off by default"),
        }
        evac.retire_plab(&mut shard);
    }
}

#[test]
fn a_big_filler_is_a_walkable_int_array_and_a_small_one_is_the_sentinel() {
    let mut buf = vec![0xAAu8; 256];
    let base = (buf.as_mut_ptr() as usize + 7) & !7;
    // SAFETY: `base..base+64` is inside `buf` and 8-aligned.
    unsafe { install_gap_filler(base, 64) };
    // SAFETY: a filler header was just written here.
    let h = unsafe { &*(base as *const ObjectHeader) };
    assert_eq!(h.class_id, cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID);
    assert_eq!(h.kind(), ObjectKind::Array);
    assert_eq!(
        crate::gen_heap::gen_object_total_size(h),
        64,
        "the filler must consume its span exactly or the next walk desyncs"
    );

    // SAFETY: `base+64 .. base+72` is inside `buf` and 8-aligned.
    unsafe { install_gap_filler(base + 64, 8) };
    // SAFETY: the sentinel's two u32s were just written here.
    let cid = unsafe { std::ptr::read((base + 64) as *const u32) };
    let gap = unsafe { std::ptr::read((base + 68) as *const u32) };
    assert_eq!(cid, cratonvm_gc_core::tlab::GAP_FILLER_CLASS_ID.as_u32());
    assert_eq!(gap, 8);
}

/// gci i41/ev — the filler tiling (`filler_chunk_len`): a span one
/// `int[]` can describe is one chunk; a larger one is tiled by chunks
/// whose `int[]` length never passes `i32::MAX`, that are 8-multiples of
/// at least a header each, and that cover the span exactly (a remainder
/// too short for a header is folded into the previous chunk's place).
/// Pure: no 8 GiB buffer is needed to check the arithmetic.
#[cfg(target_pointer_width = "64")]
#[test]
fn gci_i41ev_a_filler_span_past_one_int_array_is_tiled_exactly() {
    let one_max = HEADER_SIZE + MAX_FILLER_DATA_BYTES as usize;
    for size in [HEADER_SIZE, 64, 1 << 20, one_max] {
        assert_eq!(filler_chunk_len(size), size, "one int[] at {size}");
    }
    let cases = [
        one_max + 8,
        one_max + HEADER_SIZE,
        one_max + 64,
        2 * one_max,
        2 * one_max + 8,
        3 * one_max - 8,
        1usize << 34,
        (1usize << 35) + 8,
    ];
    for total in cases {
        let mut left = total;
        let mut chunks = 0;
        while left > 0 {
            let c = filler_chunk_len(left);
            assert!(c >= HEADER_SIZE && c % 8 == 0 && c <= left, "{total}: chunk {c}");
            let len = (c - HEADER_SIZE) / 4;
            assert!(len <= i32::MAX as usize, "{total}: int[] length {len}");
            // The walkers' own sizing of an `int[]` of that length.
            assert_eq!(
                cratonvm_gc_core::heap::array_data_size(len, ArrayElementType::Int)
                    .map(|d| (HEADER_SIZE + d + 7) & !7),
                Ok(c),
                "{total}: chunk {c} must size back to itself",
            );
            let rest = left - c;
            assert!(rest == 0 || rest >= HEADER_SIZE, "{total}: remainder {rest}");
            left = rest;
            chunks += 1;
            assert!(chunks < 64, "{total}: the tiling must terminate");
        }
    }
}

