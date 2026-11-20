use super::*;

#[test]
fn the_place_rule_names_live_dead_and_foreign_words() {
    let (lo, hi, sp) = (0x1000usize, 0x9000usize, 0x4000usize);
    assert_eq!(place_in(0x4000, lo, hi, sp), SlotPlace::OwnLive);
    assert_eq!(place_in(0x8ff8, lo, hi, sp), SlotPlace::OwnLive);
    assert_eq!(place_in(0x3ff8, lo, hi, sp), SlotPlace::OwnBelowSp);
    assert_eq!(place_in(0x1000, lo, hi, sp), SlotPlace::OwnBelowSp);
    assert_eq!(place_in(0x9000, lo, hi, sp), SlotPlace::Foreign);
    assert_eq!(place_in(0x0ff8, lo, hi, sp), SlotPlace::Foreign);
    // A word straddling the top is not inside the stack.
    assert_eq!(place_in(0x8ffc, lo, hi, sp), SlotPlace::Foreign);
    // A stack pointer outside the band (an alternate signal stack): no
    // word can be called dead.
    assert_eq!(place_in(0x2000, lo, hi, 0xf000), SlotPlace::OwnLive);
}

#[test]
fn a_local_of_the_caller_is_own_live_and_a_heap_word_is_not() {
    let local = 0usize;
    let addr = std::hint::black_box(&local) as *const usize as usize;
    let place = classify_slot(addr);
    assert!(
        matches!(place, SlotPlace::OwnLive | SlotPlace::Unknown),
        "a local of the calling frame classified {place:?}"
    );
    let boxed = Box::new(0usize);
    let heap = &*boxed as *const usize as usize;
    let place = classify_slot(heap);
    assert!(
        matches!(place, SlotPlace::Foreign | SlotPlace::Unknown),
        "a heap word classified {place:?}"
    );
}

/// gen r5w2/roots6: the heap-write rule. A range inside one region passes,
/// one that straddles two adjacent regions or leaves them does not, and a
/// range whose end overflows never passes.
#[test]
fn a_bulk_write_passes_only_inside_one_region() {
    let regions = [(0x1000usize, 0x2000usize), (0x2000, 0x3000)];
    assert!(span_inside_one(0x1000, 0x1000, &regions));
    assert!(span_inside_one(0x2ff8, 8, &regions));
    assert!(span_inside_one(0x1800, 0, &regions));
    assert!(
        !span_inside_one(0x1ff8, 16, &regions),
        "straddles two regions"
    );
    assert!(
        !span_inside_one(0x3000, 8, &regions),
        "one past the last region"
    );
    assert!(!span_inside_one(0x0ff8, 8, &regions), "below every region");
    assert!(
        !span_inside_one(usize::MAX - 4, 8, &regions),
        "overflowing end"
    );
    assert!(
        !span_inside_one(0x1000, 8, &[(0x2000, 0x1000)]),
        "an empty region"
    );
    assert!(!span_inside_one(0x1000, 8, &[]));
}

/// gen r5w2/roots6: an unchanged map passes every checkpoint, including
/// one reached after reads through it.
#[test]
fn a_map_nobody_writes_passes_every_checkpoint() {
    let mut map = cratonvm_types::PointerMap::default();
    for k in 0..64usize {
        map.insert(0x1000 + k * 8, 0x9000 + k * 8);
    }
    let mut watch = MapWatch::arm(&map, "armed");
    assert_eq!(map.get(&0x1008), Some(&0x9008));
    watch.check(&map, "after a read");
    watch.check(&map, "again");
    assert_eq!(watch.site, "again");
}

#[test]
fn every_heap_write_kind_has_a_distinct_label() {
    let kinds = [
        HeapWriteKind::EvacuationCopy,
        HeapWriteKind::InPlaceZero,
        HeapWriteKind::InPlaceFiller,
        HeapWriteKind::PromotionCopy,
        HeapWriteKind::SweepHeaderWrite,
        HeapWriteKind::SweepStretchRewrite,
        // gen r5w4/pin8: the quarantine stamp.
        HeapWriteKind::InPlaceQuarantine,
    ];
    let mut labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), kinds.len());
}

/// gen r5w4/pin8: only the value `ring` (any case, trimmed) selects ring
/// mode; the token's plain arming (`1`) keeps the printing audit.
#[test]
fn only_the_ring_value_selects_ring_mode() {
    assert!(is_ring_value("ring"));
    assert!(is_ring_value(" RING\n"));
    assert!(!is_ring_value("1"));
    assert!(!is_ring_value(""));
    assert!(!is_ring_value("rings"));
}

/// gen r5w4/pin8: the allocation-free line formatter the crash dump uses.
#[test]
fn the_ring_line_formatter_writes_hex_decimal_and_labels() {
    let mut line = LineBuf::new();
    line.hex(0);
    line.push(b" ");
    line.hex(0x7f00_dead_beef);
    line.push(b" ");
    line.dec(0);
    line.push(b" ");
    line.dec(18_446_744_073_709_551_615);
    assert_eq!(
        line.bytes(),
        b"0x0 0x7f00deadbeef 0 18446744073709551615".as_slice()
    );
    let tag = kind_code(RootWriteKind::JitFrameSlot)
        | (place_code(SlotPlace::Foreign) << 8)
        | (1 << 16);
    let mut rec = LineBuf::new();
    format_record(
        &mut rec,
        b"write",
        7,
        [42, tag, 0x1000, 0x2000, 0x3000, 0x4000, 0x5000, 3],
    );
    assert_eq!(
        std::str::from_utf8(rec.bytes()).unwrap(),
        "[root-write-audit ring] write #7 vm=3 os_tid=42 kind=jit-frame-slot slot=0x1000 \
         old=0x2000 new=0x3000 place=foreign written=true guard=[0x4000,0x5000)\n"
    );
    // gcj j1pn item 7: the longest record a dump can print fits whole.
    let tag = kind_code(RootWriteKind::BlockedPeerNativeSlot)
        | (place_code(SlotPlace::OwnBelowSp) << 8);
    let mut widest = LineBuf::new();
    let mut wide = [u64::MAX; RING_FIELDS];
    wide[1] = tag;
    format_record(&mut widest, b"anomaly", u64::MAX, wide);
    assert!(widest.bytes().ends_with(b")\n"), "the widest record line was cut");
    // A line longer than the buffer is cut, never overrun.
    let mut long = LineBuf::new();
    for _ in 0..100 {
        long.push(b"0123456789");
    }
    assert_eq!(long.bytes().len(), LINE_BUF_LEN);
}

/// gen r5w4/pin8: a ring keeps the last records (or, `keep_first`, the
/// first ones), and a slot overwritten by a later record no longer reads
/// as the earlier one.
#[test]
fn a_ring_keeps_the_window_it_promises() {
    let ring: Vec<RingRec> = (0..4).map(|_| RingRec::new()).collect();
    let next = AtomicU64::new(0);
    for i in 0..6u64 {
        ring_push(&ring, &next, false, [i, 0, 0, 0, 0, 0, 0, 0]);
    }
    assert!(
        ring_read(&ring, 0).is_none(),
        "record 0 was overwritten by record 4"
    );
    assert!(
        ring_read(&ring, 1).is_none(),
        "record 1 was overwritten by record 5"
    );
    assert_eq!(ring_read(&ring, 5).map(|f| f[0]), Some(5));
    assert_eq!(ring_read(&ring, 2).map(|f| f[0]), Some(2));

    let first: Vec<RingRec> = (0..2).map(|_| RingRec::new()).collect();
    let next_first = AtomicU64::new(0);
    for i in 0..5u64 {
        ring_push(&first, &next_first, true, [i, 0, 0, 0, 0, 0, 0, 0]);
    }
    assert_eq!(ring_read(&first, 0).map(|f| f[0]), Some(0));
    assert_eq!(ring_read(&first, 1).map(|f| f[0]), Some(1));
    assert!(
        ring_read(&first, 2).is_none(),
        "a keep-first ring stops at capacity"
    );
}

/// gci i45/bh: a record `ring_read` accepts is one whole record, while a
/// writer keeps overwriting the ring (several writers:
/// `gcj1pn_two_writers_never_leave_a_torn_record`). Every field of record
/// `n` is `n`, so a torn read shows as
/// a field that is not its record's number. The fences this pins are
/// no-ops for the compiler-visible order on x86-64, so the test is a
/// regression guard for the invariant rather than a reproducer.
#[test]
fn bh45_an_accepted_ring_record_is_never_torn() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    const LEN: usize = 8;
    const RECORDS: u64 = 200_000;
    let ring: Arc<Vec<RingRec>> = Arc::new((0..LEN).map(|_| RingRec::new()).collect());
    let next = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let (ring, next, done) = (Arc::clone(&ring), Arc::clone(&next), Arc::clone(&done));
        std::thread::spawn(move || {
            for n in 0..RECORDS {
                ring_push(&ring, &next, false, [n; RING_FIELDS]);
            }
            done.store(true, Ordering::Release);
        })
    };
    while !done.load(Ordering::Acquire) {
        let top = next.load(Ordering::Relaxed);
        for n in top.saturating_sub(LEN as u64)..top {
            if let Some(f) = ring_read(&ring, n) {
                assert!(f.iter().all(|&v| v == n), "record {n} read torn: {f:?}");
            }
        }
    }
    writer.join().expect("writer");
    for n in RECORDS - LEN as u64..RECORDS {
        assert_eq!(ring_read(&ring, n), Some([n; RING_FIELDS]), "the last window is whole");
    }
}

#[test]
fn every_kind_and_place_has_a_distinct_label() {
    let kinds = [
        RootWriteKind::BlockedPeerNativeSlot,
        RootWriteKind::JitFrameSlot,
        RootWriteKind::JitFrameDuplicate,
        RootWriteKind::JitRegisterImage,
        RootWriteKind::ShadowStackHome,
    ];
    let mut labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), kinds.len());
    let places = [
        SlotPlace::OwnLive,
        SlotPlace::OwnBelowSp,
        SlotPlace::Foreign,
        SlotPlace::Unknown,
    ];
    let mut labels: Vec<&str> = places.iter().map(|p| p.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), places.len());
}

/// gcj-j1pn-raw-root-write-owner-findings item 3: the verdict is the
/// writer's own guard. A word the guard admits is `own-live`; one in the
/// stack below the guard's lower bound -- including a word ABOVE the
/// audit's own stack pointer, the case that used to print
/// `own-live written=false` -- is `own-below-sp`; one above the guard's
/// top (the TLS clamp) or outside the stack is `foreign`.
#[test]
fn gcj1pn_the_verdict_follows_the_writers_guard() {
    let stack = Some(SlotBand::new(0x1000, 0x9000));
    let guard = SlotBand::new(0x4000, 0x8000);
    assert_eq!(place_against_guard(0x4000, stack, guard), SlotPlace::OwnLive);
    assert_eq!(place_against_guard(0x7ff8, stack, guard), SlotPlace::OwnLive);
    assert_eq!(place_against_guard(0x3ff8, stack, guard), SlotPlace::OwnBelowSp);
    assert_eq!(place_against_guard(0x1000, stack, guard), SlotPlace::OwnBelowSp);
    assert_eq!(place_against_guard(0x8000, stack, guard), SlotPlace::Foreign);
    assert_eq!(place_against_guard(0x9000, stack, guard), SlotPlace::Foreign);
    assert_eq!(place_against_guard(0x0ff8, stack, guard), SlotPlace::Foreign);
    assert_eq!(place_against_guard(0x4000, None, guard), SlotPlace::Unknown);
    // The real thing: a word between this test's frame and a guard set
    // ABOVE it (a store the guard refuses) is below the guard, whatever
    // the audit's own deeper probe would have said.
    let local = 0usize;
    let addr = std::hint::black_box(&local) as *const usize as usize;
    if let Some(stack) = crate::shadow_stack::current_thread_stack() {
        if stack.contains_slot(addr) {
            let above = SlotBand::new(addr + 8, stack.hi);
            assert_eq!(
                place_against_guard(addr, Some(stack), above),
                SlotPlace::OwnBelowSp
            );
            let around = SlotBand::new(addr, stack.hi);
            assert_eq!(place_against_guard(addr, Some(stack), around), SlotPlace::OwnLive);
        }
    }
}

/// gcj-j1pn-raw-root-write-owner-findings item 7: the budget's cut-over.
/// The first `cap` stores print, the next prints the one "exhausted"
/// line, every later one is only counted.
#[test]
fn gcj1pn_the_line_budget_cuts_over_once() {
    assert_eq!(budget_verdict(0, 3), BudgetVerdict::Print);
    assert_eq!(budget_verdict(2, 3), BudgetVerdict::Print);
    assert_eq!(budget_verdict(3, 3), BudgetVerdict::Exhausted);
    assert_eq!(budget_verdict(4, 3), BudgetVerdict::Silent);
    assert_eq!(budget_verdict(u64::MAX, 3), BudgetVerdict::Silent);
    assert_eq!(budget_verdict(0, 0), BudgetVerdict::Exhausted);
    let printed = (0..ORDINARY_LINE_BUDGET + 10)
        .filter(|&n| budget_verdict(n, ORDINARY_LINE_BUDGET) == BudgetVerdict::Print)
        .count() as u64;
    assert_eq!(printed, ORDINARY_LINE_BUDGET);
}

/// gcj-j1pn-raw-root-write-owner-findings item 7: two writers whose
/// sequence numbers collide on one record (a ring of ONE record, so every
/// pair collides) never leave a record the reader accepts with fields of
/// two writers. Writer `w`'s record `n` holds `n` in every field but the
/// last, which holds `w`; a torn record shows as a field that is not its
/// record's number. A writer that loses the claim drops its record.
#[test]
fn gcj1pn_two_writers_never_leave_a_torn_record() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    const PER_WRITER: u64 = 100_000;
    let ring: Arc<Vec<RingRec>> = Arc::new(vec![RingRec::new()]);
    let next = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..2u64)
        .map(|w| {
            let (ring, next) = (Arc::clone(&ring), Arc::clone(&next));
            std::thread::spawn(move || {
                let mut stored = 0u64;
                for _ in 0..PER_WRITER {
                    let n = next.load(Ordering::Relaxed);
                    let mut f = [n; RING_FIELDS];
                    f[RING_FIELDS - 1] = w;
                    // `n` is a guess at the sequence number `ring_push`
                    // takes; a wrong guess is a record whose fields do
                    // not match its `seq`, which the check below skips.
                    if ring_push(&ring, &next, false, f) {
                        stored += 1;
                    }
                }
                stored
            })
        })
        .collect();
    let reader = {
        let (ring, next, done) = (Arc::clone(&ring), Arc::clone(&next), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut whole = 0u64;
            while !done.load(Ordering::Acquire) {
                let top = next.load(Ordering::Relaxed);
                if let Some(f) = top.checked_sub(1).and_then(|n| ring_read(&ring, n)) {
                    // Every field a writer stored is the same guess `g`;
                    // one writer's `g` beside another's is a tear.
                    let g = f[0];
                    assert!(
                        f[..RING_FIELDS - 1].iter().all(|&v| v == g),
                        "a torn record: {f:?}"
                    );
                    assert!(f[RING_FIELDS - 1] < 2, "a torn writer field: {f:?}");
                    whole += 1;
                }
            }
            whole
        })
    };
    let stored: u64 = writers.into_iter().map(|h| h.join().expect("writer")).sum();
    done.store(true, Ordering::Release);
    let _ = reader.join().expect("reader");
    assert!(stored > 0, "no writer ever claimed the record");
    assert!(stored <= 2 * PER_WRITER);
}

/// gcj-j1pn-raw-root-write-owner-findings item 7: a [`VmTagScope`] tags
/// only while the audit is armed, nests, and restores the outer tag.
#[test]
fn gcj1pn_the_vm_tag_scope_nests_and_restores() {
    let before = current_vm_tag();
    {
        let _outer = VmTagScope::enter(11);
        let outer = current_vm_tag();
        {
            let _inner = VmTagScope::enter(22);
            if enabled() {
                assert_eq!(current_vm_tag(), 22);
            }
        }
        assert_eq!(current_vm_tag(), outer);
        if enabled() {
            assert_eq!(outer, 11);
        } else {
            assert_eq!(outer, before, "a disarmed audit tags nothing");
        }
    }
    assert_eq!(current_vm_tag(), before);
}

/// gcj-j1pn-raw-root-write-owner-findings item 7: the capture side counts
/// every call, printed or not (ring mode prints nothing; the budget is
/// shared with other tests, so only the count is checked).
#[test]
fn gcj1pn_the_capture_side_counts_every_call() {
    let own = SlotBand::new(0x1000, 0x2000);
    let (bands0, caps0) = capture_census();
    note_band_overrun(7, 0x1800, 0x3000, own);
    note_foreign_capture(7, 0x2800, 0x9000, own);
    note_foreign_capture(7, 0x2808, 0x9008, own);
    let (bands1, caps1) = capture_census();
    assert!(bands1 >= bands0 + 1);
    assert!(caps1 >= caps0 + 2);
}
