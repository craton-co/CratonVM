use super::*;

/// y-1 (young5 P3): a bitmap pointer loaded before a publish stays valid
/// through that publish (the replaced generation is RETIRED, not dropped)
/// and is freed only at the publish after it.
#[test]
fn f4y_a_replaced_object_start_bitmap_outlives_one_more_publish() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let base = 0x40_0000usize;
    let a = Arc::new(cratonvm_gc_core::heap_bitmap::HeapBitmap::new(base, 4096));
    a.insert(base + 64);
    let weak = Arc::downgrade(&a);
    heap.store_object_starts_locked(Some(a), None);
    // A lock-free reader loads the pointer here...
    let p =
        heap.object_starts[0].load(Ordering::Acquire) as *const cratonvm_gc_core::heap_bitmap::HeapBitmap;
    assert!(!p.is_null());
    // ...and a publish replaces it before the reader dereferences.
    let b = Arc::new(cratonvm_gc_core::heap_bitmap::HeapBitmap::new(base, 4096));
    heap.store_object_starts_locked(Some(b), None);
    assert!(
        weak.upgrade().is_some(),
        "the replaced bitmap was freed under a reader"
    );
    // SAFETY: kept alive by `object_starts_retired` (asserted just above).
    assert!(unsafe { (*p).contains(base + 64) });
    assert_eq!(
        heap.arena_object_start(0, base + 64),
        Some(false),
        "readers now see the new bitmap"
    );
    // The publish after it frees the old generation.
    heap.store_object_starts_locked(None, None);
    assert!(
        weak.upgrade().is_none(),
        "the retired generation is freed one publish later"
    );
    assert_eq!(heap.arena_object_start(0, base + 64), None);
}

/// y-2 (b): the unrecorded-root guard skips the cycle's KNOWN conservative
/// words and still refuses on a precise one.
#[test]
fn f4y_an_excluded_conservative_word_does_not_refuse_but_a_precise_one_does() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 4);
    let base = y.as_ptr() as usize;
    let interior = base + 16;
    let other = base + 24;
    let from = heap.lock_young_from();
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(from.base_ptr() as usize, from.used());
    assert!(starts.insert(base));
    // SAFETY: never dereferenced as objects by the guard's decision.
    let (r_int, r_oth) = unsafe {
        (
            ObjectRef::from_raw(interior as *mut u8),
            ObjectRef::from_raw(other as *mut u8),
        )
    };
    let exclude: FxHashSet<usize> = [interior].into_iter().collect();
    assert!(!heap.unrecorded_young_root_refuses_cycle(&[y, r_int], &from, &starts, &exclude));
    assert_eq!(
        heap.unrecorded_young_root_census(),
        [0, 0, 0],
        "an excluded word is not counted"
    );
    assert!(heap.unrecorded_young_root_refuses_cycle(
        &[y, r_int, r_oth],
        &from,
        &starts,
        &exclude
    ));
    assert_eq!(heap.unrecorded_young_root_census(), [1, 0, 1]);
    // Positive control: without the exclusion the interior word refuses too.
    let none = FxHashSet::default();
    assert!(heap.unrecorded_young_root_refuses_cycle(&[y, r_int], &from, &starts, &none));
}

/// gch h1/yp (`gcg-g5jn-...`): the exclusion census counts the offenders
/// the exclusion set absorbed -- the refusals the switch prevented -- and
/// nothing on a cycle whose exclusion set names no offender.
#[test]
fn gch_h1yp_the_exclusion_census_counts_absorbed_offenders_only() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 4);
    let base = y.as_ptr() as usize;
    let interior = base + 16;
    let from = heap.lock_young_from();
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(from.base_ptr() as usize, from.used());
    assert!(starts.insert(base));
    // SAFETY: never dereferenced as an object by the guard's decision.
    let r_int = unsafe { ObjectRef::from_raw(interior as *mut u8) };
    // An exclusion set that names only a recorded start absorbs nothing.
    let start_only: FxHashSet<usize> = [base].into_iter().collect();
    assert!(!heap.unrecorded_young_root_refuses_cycle(&[y], &from, &starts, &start_only));
    assert_eq!(heap.unrecorded_young_root_exclusion_census(), [0, 0]);
    // The interior word twice, excluded: one cycle, two absorbed roots, no refusal.
    let exclude: FxHashSet<usize> = [interior].into_iter().collect();
    assert!(!heap.unrecorded_young_root_refuses_cycle(
        &[y, r_int, r_int],
        &from,
        &starts,
        &exclude
    ));
    assert_eq!(heap.unrecorded_young_root_exclusion_census(), [1, 2]);
    assert_eq!(heap.unrecorded_young_root_census(), [0, 0, 0]);
    // Without the exclusion the same roots refuse, and the census does not move.
    let none = FxHashSet::default();
    assert!(heap.unrecorded_young_root_refuses_cycle(&[y, r_int], &from, &starts, &none));
    assert_eq!(heap.unrecorded_young_root_exclusion_census(), [1, 2]);
}

/// gch h2/yg (`gch-h1yp-unrecorded-root-latch-...`): the streak step.
#[test]
fn gch_h2yg_the_refusal_streak_extends_on_refusal_and_ends_otherwise() {
    assert_eq!(unrecorded_root_refusal_streak(0, 0, true), (1, 1));
    assert_eq!(unrecorded_root_refusal_streak(1, 1, true), (2, 2));
    assert_eq!(
        unrecorded_root_refusal_streak(2, 2, false),
        (0, 2),
        "the max survives a reset"
    );
    assert_eq!(unrecorded_root_refusal_streak(0, 2, true), (1, 2));
    assert_eq!(
        unrecorded_root_refusal_streak(u64::MAX, 7, true),
        (u64::MAX, u64::MAX)
    );
}

/// gch h2/yg: on the heap, a stream of DISTINCT unrecorded words (A, B, A)
/// refuses three cycles in a row -- the latch does not bound it -- and the
/// census says so; a repeat of the last address fails open and ends the
/// streak, as does a clean decision.
#[test]
fn gch_h2yg_a_stream_of_distinct_stale_words_shows_in_the_max_streak() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 4);
    let base = y.as_ptr() as usize;
    let from = heap.lock_young_from();
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(from.base_ptr() as usize, from.used());
    assert!(starts.insert(base));
    // SAFETY: never dereferenced as objects by the guard's decision.
    let (a, b) = unsafe {
        (
            ObjectRef::from_raw((base + 16) as *mut u8),
            ObjectRef::from_raw((base + 24) as *mut u8),
        )
    };
    let none = FxHashSet::default();
    assert_eq!(heap.unrecorded_young_root_max_streak(), 0);
    assert!(heap.unrecorded_young_root_refuses_cycle(&[y, a], &from, &starts, &none));
    assert!(heap.unrecorded_young_root_refuses_cycle(&[y, b], &from, &starts, &none));
    assert!(heap.unrecorded_young_root_refuses_cycle(&[y, a], &from, &starts, &none));
    assert_eq!(
        heap.unrecorded_young_root_max_streak(),
        3,
        "A, B, A: three refusals"
    );
    // The same lowest address again: fail-open, the streak ends.
    assert!(!heap.unrecorded_young_root_refuses_cycle(&[y, a], &from, &starts, &none));
    assert!(heap.unrecorded_young_root_refuses_cycle(&[y, b], &from, &starts, &none));
    // A clean decision ends it too.
    assert!(!heap.unrecorded_young_root_refuses_cycle(&[y], &from, &starts, &none));
    assert!(heap.unrecorded_young_root_refuses_cycle(&[y, a], &from, &starts, &none));
    assert_eq!(
        heap.unrecorded_young_root_max_streak(),
        3,
        "the longest run is kept"
    );
    assert_eq!(heap.unrecorded_young_root_census(), [5, 1, 6]);
}

/// gci i8/pe (sizer10 (b)): the echo rule, pure. An aligned word adds only
/// its companion 8 below; a misaligned one adds its aligned word and that
/// word's companion; nothing outside `[lo, hi]` is added; the original
/// words stay.
#[test]
fn gci_i8pe_probe_echoes_are_the_aligned_word_and_the_companion_below() {
    let (lo, hi) = (0x1000usize, 0x2000usize);
    let mut w = vec![0x1100, 0x1203, 0x1000, 0x2005];
    extend_with_conservative_probe_echoes(&mut w, lo, hi);
    assert_eq!(&w[..4], &[0x1100, 0x1203, 0x1000, 0x2005], "the ledger words stay first");
    let added: Vec<usize> = w[4..].to_vec();
    assert_eq!(
        added,
        vec![0x10f8, 0x1200, 0x11f8, 0x2000, 0x1ff8],
        "0x1100 -> companion 0x10f8; 0x1203 -> 0x1200 + 0x11f8; 0x1000's companion \
         0xff8 is below lo (dropped); 0x2005 -> 0x2000 (== hi, kept) + 0x1ff8",
    );
}

/// gci i8/pe: the `<= hi` edge -- a misaligned word just past the last
/// object echoes to `hi` itself (the one-past-the-end cursor) and its
/// companion; an empty set stays empty.
#[test]
fn gci_i8pe_probe_echoes_keep_the_one_past_the_end_word() {
    let mut w = vec![0x2005usize];
    extend_with_conservative_probe_echoes(&mut w, 0x1000, 0x2000);
    assert_eq!(w, vec![0x2005, 0x2000, 0x1ff8]);
    let mut empty: Vec<usize> = Vec::new();
    extend_with_conservative_probe_echoes(&mut empty, 0x1000, 0x2000);
    assert!(empty.is_empty());
}

/// gci i8/pe (vi4-pn PYT `urr_cycles_refused` 4 / 2): a frozen band word
/// `w` in the ledger, rooted by the take-over probe as `w & !7` plus the
/// companion `(w & !7) - 8`, does NOT refuse the cycle once the exclusion
/// carries the echoes; with the raw ledger only (the i4b..i7 set) the same
/// roots refuse. A precise unrecorded root elsewhere still refuses.
#[test]
fn gci_i8pe_a_takeover_companion_root_does_not_refuse_a_ledger_guarded_cycle() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 8);
    let base = y.as_ptr() as usize;
    let from = heap.lock_young_from();
    let (lo, hi) = (from.base_ptr() as usize, from.base_ptr() as usize + from.used());
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new(lo, from.used());
    assert!(starts.insert(base));
    // The frozen word, misaligned: the probe's answer is base+32, its
    // companion base+24 (both inside `y`, neither a recorded start).
    let raw_word = base + 35;
    // SAFETY: never dereferenced as objects by the guard's decision.
    let (echo, companion, precise) = unsafe {
        (
            ObjectRef::from_raw((base + 32) as *mut u8),
            ObjectRef::from_raw((base + 24) as *mut u8),
            ObjectRef::from_raw((base + 8) as *mut u8),
        )
    };
    let raw_only: FxHashSet<usize> = [raw_word].into_iter().collect();
    assert!(
        heap.unrecorded_young_root_refuses_cycle(&[y, echo, companion], &from, &starts, &raw_only),
        "positive control: the raw ledger word alone does not cover the probe's roots",
    );
    let mut words = vec![raw_word];
    extend_with_conservative_probe_echoes(&mut words, lo, hi);
    let exclude: FxHashSet<usize> = words.into_iter().collect();
    assert!(!heap.unrecorded_young_root_refuses_cycle(&[y, echo, companion], &from, &starts, &exclude));
    assert_eq!(heap.unrecorded_young_root_exclusion_census()[1], 2, "both echoes absorbed");
    assert!(
        heap.unrecorded_young_root_refuses_cycle(&[y, echo, companion, precise], &from, &starts, &exclude),
        "a root 24 bytes below the echo is not an echo and still refuses",
    );
}

/// y-5 (young5 P1): a header whose extent crosses the next free block is
/// no longer emitted by `walk_young_objects_and_gaps`; the stretch up to
/// the free block is a gap, and the walk resumes on-grid after it.
#[test]
fn f4y_a_hole_crossing_header_is_a_gap_not_an_object() {
    let heap = GenerationalHeap::with_sizes(1024 * 1024, 1024 * 1024);
    let a = heap.alloc_object(ClassId::new(1), 1);
    let x = heap.alloc_object(ClassId::new(1), 8);
    let g = heap.alloc_object(ClassId::new(1), 2);
    let xa = x.as_ptr() as usize;
    let size_x = gen_object_total_size(heap.get_header(x));
    assert_eq!(xa + size_x, g.as_ptr() as usize, "precondition: contiguous");
    {
        let mut from = heap.lock_young_from();
        let base = from.base_ptr() as usize;
        // The last 32 bytes of X's extent become a free block: X crosses it.
        from.add_free_block(xa + size_x - 32 - base, 32);
    }
    let (objs, gaps) = heap.walk_young_objects_and_gaps();
    let addrs: Vec<usize> = objs.iter().map(|&(p, _)| p as usize).collect();
    assert!(addrs.contains(&(a.as_ptr() as usize)));
    assert!(
        addrs.contains(&(g.as_ptr() as usize)),
        "the walk resumed on-grid after the hole"
    );
    assert!(
        !addrs.contains(&xa),
        "the hole-crossing header is not an object"
    );
    assert!(
        gaps.contains(&(xa, xa + size_x - 32)),
        "its stretch is a gap: {gaps:x?}"
    );
}
