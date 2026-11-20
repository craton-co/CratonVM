use super::*;

const BOTH: CardTableOptions = CardTableOptions {
    summary: true,
    precise_ref_array_marks: true,
};

/// Exactly the arithmetic the emitted fast path performs
/// (`jit/src/x64/objects.rs::emit_gen_card_check`): load `old_base` and
/// `cards_neg` from the view, `idx = (mark - old_base) >> CARD_SHIFT`,
/// `&card = idx - cards_neg`, read the byte. `None` where the emitted code
/// would not read at all (outside `[old_base, old_end)`).
fn read_like_the_jit(view_addr: usize, mark: usize) -> Option<bool> {
    // SAFETY: `view_addr` is `CardTable::jit_card_view_addr` of a table
    // the caller keeps alive for the whole call.
    let view = unsafe { &*(view_addr as *const JitCardView) };
    let base = view.old_base.load(Ordering::Acquire);
    let end = view.old_end.load(Ordering::Acquire);
    if mark < base || mark >= end {
        return None;
    }
    let idx = (mark - base) >> cratonvm_types::CARD_SHIFT;
    let card = idx.wrapping_sub(view.cards_neg.load(Ordering::Acquire));
    // SAFETY: `mark` is inside the covered range, so `idx < num_cards`
    // and `card` is `&cards[idx]` of the live byte map.
    Some(unsafe { &*(card as *const AtomicU8) }.load(Ordering::Acquire) == CARD_DIRTY)
}

/// The view names this table, and handing it out disarms NOTHING — the
/// whole point of the protocol, and the difference from `jit_cards_addr`.
#[test]
fn the_view_describes_the_table_and_does_not_disarm_the_bound_or_the_summary() {
    let base = 0x50_0000usize;
    let num = SUMMARY_GROUP * 5 + 9;
    let ct = CardTable::with_options(base, CARD_SIZE * num, BOTH);
    let addr = ct.jit_card_view_addr();
    assert_ne!(addr, 0);
    assert_eq!(addr, ct.jit_card_view() as *const JitCardView as usize);
    // SAFETY: the table is alive.
    let view = unsafe { &*(addr as *const JitCardView) };
    assert_eq!(view.magic, JIT_CARD_VIEW_MAGIC);
    assert_eq!(view.old_base.load(Ordering::Acquire), base);
    assert_eq!(view.old_end.load(Ordering::Acquire), base + CARD_SIZE * num);
    // gcf f2/p: `BOTH` has the summary map too, which the view now names.
    // gcj j2/jt2: a card-aligned base also names the exact byte-map base
    // (`FLAG_BYTE_MAP_BASE`, a capability bit the compiler reads).
    assert_eq!(
        view.flags,
        JitCardView::FLAG_PRECISE_REF_ARRAYS
            | JitCardView::FLAG_SUMMARY
            | JitCardView::FLAG_BYTE_MAP_BASE
    );
    assert!(!ct.raw_card_address_escaped(), "the view is not an escape");
    assert!(ct.summary_active(), "the summary stays armed");
    ct.mark_dirty_lockfree(base + 3 * CARD_SIZE);
    assert_eq!(ct.scan_bound_cards(), 4, "the bound stays self-maintained");
    assert_eq!(ct.take_dirty_cards(), vec![3]);
    assert!(ct.cards_skipped_by_scan_bound() > 0);
    // Only now compare the negated map address against the raw one, which
    // escapes the table (and is why this assertion comes last).
    assert_eq!(
        view.cards_neg.load(Ordering::Acquire).wrapping_neg(),
        ct.jit_cards_addr()
    );
    let legacy = CardTable::with_options(base, CARD_SIZE * num, CardTableOptions::LEGACY);
    // gcj j2/jt2: the legacy table still names the aligned byte-map base.
    assert_eq!(legacy.jit_card_view().flags, JitCardView::FLAG_BYTE_MAP_BASE);
}

/// The layout the JIT copies. A drift here is a barrier that reads the
/// wrong word, so it is pinned from this side as well as the JIT's.
#[test]
fn the_view_layout_is_the_documented_one() {
    assert_eq!(
        std::mem::offset_of!(JitCardView, magic),
        JitCardView::MAGIC_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, old_base),
        JitCardView::OLD_BASE_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, old_end),
        JitCardView::OLD_END_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, cards_neg),
        JitCardView::CARDS_NEG_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, flags),
        JitCardView::FLAGS_OFFSET
    );
    // gcf f1/p: the four trace counters follow `flags`, one word each.
    let trace = std::mem::offset_of!(JitCardView, trace);
    assert_eq!(trace, JitCardView::TRACE_SP_DIRTY_OFFSET);
    assert_eq!(trace + 8, JitCardView::TRACE_SP_CLEAN_OFFSET);
    assert_eq!(trace + 16, JitCardView::TRACE_IR_DIRTY_OFFSET);
    assert_eq!(trace + 24, JitCardView::TRACE_IR_CLEAN_OFFSET);
    // gcf f2/p: the card-store barrier's two words follow the trace.
    assert_eq!(
        std::mem::offset_of!(JitCardView, scan_bound),
        JitCardView::SCAN_BOUND_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, summary_neg),
        JitCardView::SUMMARY_NEG_OFFSET
    );
    // gci i23/lc: the skip-edge counters and the lean-POST counter are
    // APPENDED; every offset above is unchanged.
    let skip = std::mem::offset_of!(JitCardView, skip_trace);
    assert_eq!(skip, JitCardView::SKIP_TRACE_YOUNG_OFFSET);
    assert_eq!(skip + 8, JitCardView::SKIP_TRACE_NULL_OFFSET);
    assert_eq!(skip + 16, JitCardView::SKIP_TRACE_OLD_VALUE_OFFSET);
    assert_eq!(skip + 24, JitCardView::SKIP_TRACE_OUTSIDE_OFFSET);
    assert_eq!(
        std::mem::offset_of!(JitCardView, lean_post_trace),
        JitCardView::LEAN_POST_TRACE_OFFSET
    );
    assert!(
        JitCardView::LEAN_POST_TRACE_OFFSET < 0x80,
        "every trace counter must stay a positive disp8 off the view"
    );
    // gcj j2/jt2: the shift-first card word and the post-only site count
    // are APPENDED too.
    assert_eq!(
        std::mem::offset_of!(JitCardView, card_base_neg),
        JitCardView::CARD_BASE_NEG_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, post_only_sites),
        JitCardView::POST_ONLY_SITES_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, post_only_completed),
        JitCardView::POST_ONLY_COMPLETED_OFFSET
    );
    // t1 lane D: the hull words, appended.
    assert_eq!(
        std::mem::offset_of!(JitCardView, hull_lo),
        JitCardView::HULL_LO_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, hull_len),
        JitCardView::HULL_LEN_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, hull_buf_neg),
        JitCardView::HULL_BUF_NEG_OFFSET
    );
    assert_eq!(
        std::mem::offset_of!(JitCardView, hull_summary_neg),
        JitCardView::HULL_SUMMARY_NEG_OFFSET
    );
    assert_eq!(std::mem::size_of::<JitCardView>(), 184);
}

/// gcj j2/jt2 (`CRATONVM_JIT_CARD_BYTE_MAP_BASE`): on a card-aligned table
/// the shift-first card address `(mark >> CARD_SHIFT) - card_base_neg`
/// names exactly the card `(mark - old_base) >> CARD_SHIFT` does, for
/// every byte of the first, a middle and the last card; an unaligned base
/// publishes no word and clears the flag.
/// t1 lane D (`CRATONVM_GC_WHOLE_HEAP_CARDS`): a hull table's compiled
/// address `((mark - hull_lo) >> CARD_SHIFT) - hull_buf_neg` names the
/// table's own card for every old mark, lands inside the hull map (never
/// in the old window) for a mark of the young range below or above it,
/// and the hull compare refuses a mark outside; the summary is off; a
/// scattered hull and an uncovered range are refused.
#[test]
fn t1d_a_hull_table_names_the_old_card_and_keeps_young_marks_in_its_junk() {
    let old_base = 0x100_0000usize;
    let old_size = CARD_SIZE * 256;
    let young_lo = 0x80_0000usize; // below old
    let young_hi_end = old_base + old_size + CARD_SIZE * 512; // above old
    let opts = CardTableOptions {
        summary: true,
        precise_ref_array_marks: true,
    };
    let ct = CardTable::with_hull(
        old_base,
        old_size,
        opts,
        (young_lo, young_hi_end),
        young_hi_end - young_lo,
        1,
    )
    .expect("a compact hull is accepted");
    assert!(ct.whole_heap());
    let v = ct.jit_card_view();
    assert_ne!(v.flags & JitCardView::FLAG_WHOLE_HEAP, 0);
    assert_ne!(v.flags & JitCardView::FLAG_SUMMARY, 0, "the summary stays on");
    let lo = v.hull_lo.load(Ordering::Relaxed);
    // The hull summary names the table's own summary byte for an old mark.
    let hull_summary = v.hull_summary_neg.load(Ordering::Relaxed).wrapping_neg();
    let own_summary = v.summary_neg.load(Ordering::Relaxed).wrapping_neg();
    for card in [0usize, 63, 64, 255] {
        let mark = old_base + card * CARD_SIZE + 8;
        assert_eq!(
            ((mark - lo) >> (cratonvm_types::CARD_SHIFT + 6)) + hull_summary,
            ((mark - old_base) >> (cratonvm_types::CARD_SHIFT + 6)) + own_summary,
            "card {card}"
        );
    }
    let len = v.hull_len.load(Ordering::Relaxed);
    let buf = v.hull_buf_neg.load(Ordering::Relaxed).wrapping_neg();
    assert_eq!(lo, young_lo);
    assert_eq!(len, young_hi_end - young_lo);
    let cards = v.cards_neg.load(Ordering::Relaxed).wrapping_neg();
    let compiled = |mark: usize| -> Option<usize> {
        let idx = mark.wrapping_sub(lo);
        (idx < len).then(|| (idx >> cratonvm_types::CARD_SHIFT) + buf)
    };
    for card in [0usize, 100, 255] {
        for byte in [0usize, 8, CARD_SIZE - 1] {
            let mark = old_base + card * CARD_SIZE + byte;
            assert_eq!(compiled(mark), Some(cards + card), "old mark {mark:#x}");
        }
    }
    let window = cards..cards + ct.num_cards();
    let map = buf..buf + len / CARD_SIZE;
    for young in [young_lo, old_base - 8, old_base + old_size, young_hi_end - 8] {
        let a = compiled(young).expect("inside the hull");
        assert!(map.contains(&a) && !window.contains(&a), "{young:#x}");
    }
    assert_eq!(compiled(young_lo - 8), None);
    assert_eq!(compiled(young_hi_end), None);
    // A store through the junk is invisible to every scan.
    // SAFETY: `a` lies inside the live hull buffer (asserted above).
    unsafe { (*(compiled(young_lo).unwrap() as *const AtomicU8)).store(CARD_DIRTY, Ordering::Relaxed) };
    ct.set_scan_bound_bytes(old_size);
    assert!(ct.take_dirty_cards().is_empty());
    // A compiled store into an old card (summary byte, card, summary byte)
    // is seen once the bound covers it.
    let m = old_base + 70 * CARD_SIZE;
    let s = ((m - lo) >> (cratonvm_types::CARD_SHIFT + 6)) + hull_summary;
    // SAFETY: inside the live summary and card buffers.
    unsafe {
        (*(s as *const AtomicU8)).store(CARD_DIRTY, Ordering::Relaxed);
        (*(compiled(m).unwrap() as *const AtomicU8)).store(CARD_DIRTY, Ordering::Relaxed);
        (*(s as *const AtomicU8)).store(CARD_DIRTY, Ordering::Relaxed);
    }
    assert_eq!(ct.take_dirty_cards(), vec![70]);
    // Refusals.
    assert!(CardTable::with_hull(old_base, old_size, opts, (young_lo, young_hi_end), old_size, 2).is_none());
    assert!(CardTable::with_hull(old_base, old_size, opts, (old_base + CARD_SIZE, young_hi_end), usize::MAX, 1).is_none());
    assert!(CardTable::with_hull(old_base + 8, old_size, opts, (young_lo, young_hi_end), usize::MAX, 1).is_none());
    // Without a hull nothing changes.
    assert!(!CardTable::with_options(old_base, old_size, opts).whole_heap());
}

#[test]
fn gcj2jt2_the_shift_first_card_address_names_the_same_card() {
    let base = 0x40_0000usize;
    let ct = CardTable::new(base, CARD_SIZE * 64);
    let v = ct.jit_card_view();
    assert_ne!(v.flags & JitCardView::FLAG_BYTE_MAP_BASE, 0);
    let neg = v.card_base_neg.load(Ordering::Relaxed);
    let cards = v.cards_neg.load(Ordering::Relaxed).wrapping_neg();
    for card in [0usize, 31, 63] {
        for byte in [0usize, 1, 8, CARD_SIZE - 1] {
            let mark = base + card * CARD_SIZE + byte;
            let classic = ((mark - base) >> cratonvm_types::CARD_SHIFT) + cards;
            let shift_first = (mark >> cratonvm_types::CARD_SHIFT).wrapping_sub(neg);
            assert_eq!(shift_first, classic, "card {card} byte {byte}");
        }
    }
    let odd = CardTable::new(base + 8, CARD_SIZE * 64);
    let ov = odd.jit_card_view();
    assert_eq!(ov.flags & JitCardView::FLAG_BYTE_MAP_BASE, 0);
    assert_eq!(ov.card_base_neg.load(Ordering::Relaxed), 0);
}

/// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): the take-over's
/// completion dirties the card of every in-range register word (a header
/// and an interior element address alike) and leaves every other card
/// clean; out-of-range words (a young value, a stack pointer, zero) are
/// ignored. The site count starts at zero.
#[test]
fn gcj2jt2_the_register_completion_dirties_exactly_the_in_range_words() {
    let base = 0x40_0000usize;
    let ct = CardTable::new(base, CARD_SIZE * 64);
    assert_eq!(ct.jit_post_only_card_sites(), 0);
    let holder = base + 3 * CARD_SIZE + 16;
    let element = base + 9 * CARD_SIZE + 200;
    let words = [0usize, holder, 0x10_0000, element, base + CARD_SIZE * 64, 7];
    assert_eq!(ct.complete_frozen_register_marks(&words), 2);
    assert_eq!(ct.jit_post_only_card_completions(), 2, "the engagement count");
    let dirty = ct.take_dirty_cards();
    assert_eq!(dirty, vec![3, 9]);
}

/// gci i23/lc: a fresh table's skip-edge and lean-POST counters read zero
/// through their readers, and each reader names its own words.
#[test]
fn gci_i23lc_the_skip_and_lean_post_counters_read_back() {
    let ct = CardTable::new(0x40_0000, CARD_SIZE * 64);
    assert_eq!(ct.jit_card_skip_trace(), [0; 4]);
    assert_eq!(ct.jit_card_lean_post_trace(), 0);
    let v = ct.jit_card_view();
    for (i, c) in v.skip_trace.iter().enumerate() {
        c.fetch_add(i as u64 + 1, Ordering::Relaxed); // Cast: a small test count
    }
    v.lean_post_trace.fetch_add(7, Ordering::Relaxed);
    assert_eq!(ct.jit_card_skip_trace(), [1, 2, 3, 4]);
    assert_eq!(ct.jit_card_lean_post_trace(), 7);
    assert_eq!(ct.jit_card_check_trace(), [0; 4], "the card-byte pair is separate");
}

/// gcf f2/p: the view's scan-bound MIRROR follows every raise of the
/// self-maintained bound and never exceeds it, and `summary_neg` names the
/// summary map (or is `0` without one).
#[test]
fn gcf_f2p_the_view_mirrors_the_scan_bound_and_names_the_summary() {
    let base = 0x60_0000usize;
    let num = SUMMARY_GROUP * 3 + 5;
    let ct = CardTable::with_options(base, CARD_SIZE * num, BOTH);
    let v = ct.jit_card_view();
    assert_eq!(v.scan_bound.load(Ordering::Acquire), 0, "nothing dirtied");
    ct.mark_dirty_lockfree(base + 7 * CARD_SIZE + 16);
    assert_eq!(ct.scan_bound_cards(), 8);
    assert_eq!(
        v.scan_bound.load(Ordering::Acquire),
        8,
        "the mirror follows a raise"
    );
    ct.mark_dirty_lockfree(base + 2 * CARD_SIZE);
    assert_eq!(
        v.scan_bound.load(Ordering::Acquire),
        8,
        "and never moves down"
    );
    ct.set_scan_bound_bytes(usize::MAX);
    assert_eq!(
        v.scan_bound.load(Ordering::Acquire),
        num,
        "clamped to the map, as the bound"
    );
    assert!(v.scan_bound.load(Ordering::Acquire) <= ct.scan_bound_cards());
    assert_ne!(v.summary_neg.load(Ordering::Acquire), 0);
    assert_ne!(v.flags & JitCardView::FLAG_SUMMARY, 0);
    let legacy = CardTable::with_options(base, CARD_SIZE * num, CardTableOptions::LEGACY);
    assert_eq!(
        legacy.jit_card_view().summary_neg.load(Ordering::Acquire),
        0
    );
    assert_eq!(legacy.jit_card_view().flags & JitCardView::FLAG_SUMMARY, 0);
}

/// gcf f2/p: what the compiled card-store barrier writes -- the summary
/// byte, the card byte, the summary byte again, each through the view's
/// negated addresses exactly as the emitted code forms them -- is a mark
/// the collector's scan finds, the same as `mark_dirty_lockfree`'s.
#[test]
fn gcf_f2p_a_store_through_the_view_words_is_found_by_the_scan() {
    let base = 0x80_0000usize;
    let num = SUMMARY_GROUP * 4;
    let ct = CardTable::with_options(base, CARD_SIZE * num, BOTH);
    // Raise the bound past the card the "compiled" store marks, as an
    // earlier Rust mark would have.
    ct.mark_dirty_lockfree(base + (SUMMARY_GROUP * 3) * CARD_SIZE);
    let _ = ct.take_dirty_cards();
    let v = ct.jit_card_view();
    let mark = base + (SUMMARY_GROUP * 2 + 5) * CARD_SIZE + 24;
    let old_base = v.old_base.load(Ordering::Acquire);
    let idx = (mark - old_base) >> cratonvm_types::CARD_SHIFT;
    assert!(
        idx < v.scan_bound.load(Ordering::Acquire),
        "the store is under the mirror"
    );
    let summary = ((mark - old_base) >> (cratonvm_types::CARD_SHIFT + 6))
        .wrapping_sub(v.summary_neg.load(Ordering::Acquire));
    let card = idx.wrapping_sub(v.cards_neg.load(Ordering::Acquire));
    // SAFETY: both addresses lie inside this live table's maps (the
    // index arithmetic is the emitted code's, checked above).
    unsafe {
        (*(summary as *const AtomicU8)).store(CARD_DIRTY, Ordering::Release);
        (*(card as *const AtomicU8)).store(CARD_DIRTY, Ordering::Release);
        (*(summary as *const AtomicU8)).store(CARD_DIRTY, Ordering::Release);
    }
    assert_eq!(ct.take_dirty_cards(), vec![idx]);
}

/// gcf f1/p: the trace counters start at zero, are what
/// `jit_card_check_trace` reports, and a refresh of the run-time words
/// leaves them alone.
#[test]
fn gcf_f1p_the_card_check_trace_counters_read_back_in_order() {
    let ct = CardTable::with_options(0x70_0000, CARD_SIZE * 16, BOTH);
    assert_eq!(ct.jit_card_check_trace(), [0; 4]);
    let v = ct.jit_card_view();
    for (i, c) in v.trace.iter().enumerate() {
        c.fetch_add(i as u64 + 1, Ordering::Relaxed);
    }
    ct.refresh_jit_card_view();
    assert_eq!(ct.jit_card_check_trace(), [1, 2, 3, 4]);
}

/// gcf f1/p: the zero-allocated maps (`clean_card_bytes`) read clean
/// everywhere, have exactly the requested length, and mark / scan / clear
/// exactly as before -- on a map large enough (16 Mi cards, 8 GiB of
/// covered old generation) that the allocator hands back fresh zero pages
/// rather than recycled memory.
#[test]
fn gcf_f1p_a_zero_allocated_card_map_reads_clean_and_marks_as_before() {
    let v = clean_card_bytes(4096 + 3);
    assert_eq!(v.len(), 4096 + 3);
    assert!(v.iter().all(|b| b.load(Ordering::Relaxed) == CARD_CLEAN));
    assert!(clean_card_bytes(0).is_empty());

    let base = 0x10_0000_0000usize;
    let num = 16 * 1024 * 1024;
    let ct = CardTable::with_options(base, CARD_SIZE * num, BOTH);
    assert_eq!(ct.num_cards(), num);
    for idx in [0, 1, SUMMARY_GROUP, num / 2, num - 1] {
        assert!(!ct.is_dirty(idx), "card {idx} must start clean");
    }
    assert!(ct.dirty_card_indices().is_empty());
    let marks = [3usize, SUMMARY_GROUP * 7 + 1, num / 2, num - 1];
    for &idx in &marks {
        ct.mark_dirty_lockfree(base + idx * CARD_SIZE + 8);
    }
    assert_eq!(ct.take_dirty_cards(), marks.to_vec());
    assert!(ct.dirty_card_indices().is_empty(), "the take consumed them");
    let legacy = CardTable::with_options(base, CARD_SIZE * 1000, CardTableOptions::LEGACY);
    legacy.mark_dirty_lockfree(base + 999 * CARD_SIZE);
    assert_eq!(legacy.dirty_card_indices(), vec![999]);
}

/// The card the emitted read lands on is the card the Rust barrier marks,
/// for every address in a region that is not a whole number of cards and
/// starts off a card boundary.
#[test]
fn a_card_read_through_the_view_is_the_card_the_barrier_marks() {
    let base = 0x60_0000usize + 0x48;
    let size = CARD_SIZE * 37 + 200;
    let ct = CardTable::with_options(base, size, BOTH);
    let view = ct.jit_card_view_addr();
    assert_eq!(read_like_the_jit(view, base - 8), None);
    assert_eq!(read_like_the_jit(view, base + size), None);
    for off in (0..size).step_by(40) {
        let a = base + off;
        assert_eq!(read_like_the_jit(view, a), Some(false), "{off:#x}: clean");
        ct.mark_dirty_lockfree(a);
        assert_eq!(read_like_the_jit(view, a), Some(true), "{off:#x}: marked");
        assert!(ct.is_dirty(off / CARD_SIZE));
        ct.clear_all();
    }
}

/// The inline protocol end to end, against a brute-force model: every
/// "store" reads its card through the view and calls the Rust barrier only
/// on a clean answer. The scans must return exactly the modelled set, and
/// the bound and the summary must both still be skipping cards — which is
/// what the old raw-store emitter could not keep.
#[test]
fn check_then_call_keeps_every_card_and_both_skips_armed() {
    let _arm = super::tests::METRICS_ARM.lock();
    let base = 0x70_0000usize;
    let num = SUMMARY_GROUP * 16;
    let ct = CardTable::with_options(base, CARD_SIZE * num, BOTH);
    let view = ct.jit_card_view_addr();
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut model: std::collections::BTreeSet<usize> = Default::default();
    let mut calls = 0usize;
    let mut stores = 0usize;
    for round in 0..40 {
        // Stores confined to summary groups 0 and 2, so the bound (at most
        // three groups) and the summary (group 1, always clean) both have
        // something to skip.
        for _ in 0..64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let group_bytes = CARD_SIZE * SUMMARY_GROUP;
            let group = if x & 1 == 0 { 0 } else { 2 };
            let a = base + group * group_bytes + ((x >> 1) as usize % group_bytes);
            stores += 1;
            if read_like_the_jit(view, a) == Some(false) {
                calls += 1;
                ct.mark_dirty_lockfree(a);
            }
            model.insert((a - base) / CARD_SIZE);
        }
        let expected: Vec<usize> = model.iter().copied().collect();
        if round % 2 == 0 {
            assert_eq!(ct.take_dirty_cards(), expected, "round {round}");
        } else {
            assert_eq!(ct.dirty_card_indices(), expected, "round {round}");
            ct.clear_all();
        }
        model.clear();
    }
    assert!(calls < stores, "a dirty card must spare the call");
    assert!(!ct.raw_card_address_escaped());
    assert!(
        ct.scan_bound_cards() <= 3 * SUMMARY_GROUP,
        "the bound tracks the stores"
    );
    assert!(ct.cards_skipped_by_scan_bound() > 0, "the bound is armed");
    assert!(ct.cards_skipped_by_summary() > 0, "the summary is armed");
}

/// `refresh_jit_card_view` republishes the current geometry; today that is
/// the construction-time geometry, so it is idempotent.
#[test]
fn refreshing_the_view_is_idempotent_for_a_fixed_table() {
    let base = 0x80_0000usize;
    let ct = CardTable::with_options(base, CARD_SIZE * 100, BOTH);
    let v = ct.jit_card_view();
    let before = (
        v.old_base.load(Ordering::Acquire),
        v.old_end.load(Ordering::Acquire),
        v.cards_neg.load(Ordering::Acquire),
    );
    ct.refresh_jit_card_view();
    let after = (
        v.old_base.load(Ordering::Acquire),
        v.old_end.load(Ordering::Acquire),
        v.cards_neg.load(Ordering::Acquire),
    );
    assert_eq!(before, after);
    assert!(!ct.raw_card_address_escaped());
    // gen r4w5/review5: the idempotence above holds for a refresh that
    // does nothing at all, so it cannot tell a working refresh from a
    // deleted body. Clobber the three run-time words and require the
    // refresh to put back exactly the table's geometry.
    v.old_base.store(1, Ordering::Release);
    v.old_end.store(2, Ordering::Release);
    v.cards_neg.store(3, Ordering::Release);
    ct.refresh_jit_card_view();
    let restored = (
        v.old_base.load(Ordering::Acquire),
        v.old_end.load(Ordering::Acquire),
        v.cards_neg.load(Ordering::Acquire),
    );
    assert_eq!(
        restored, before,
        "refresh re-derives the view from the table"
    );
    assert_eq!(restored.0, base);
    assert_eq!(restored.1, base + CARD_SIZE * 100);
}
