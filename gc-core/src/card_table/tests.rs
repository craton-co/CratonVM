use super::*;

/// Serialises every test that touches the PROCESS-GLOBAL `gc_metrics`
/// counters or their hot-path gate.
///
/// gengc-round2. `reset_metrics_for_test` and
/// `set_hot_path_counters_enabled` are both process-wide, the harness runs
/// tests on parallel threads, and there are now two tests that assert an
/// exact `duplicate_card_marks` value. Without this, adding the second one
/// would silently turn the first into a flake — the same trap
/// `gen_evac`'s `CAS_LOSS_ARM` was introduced for.
///
/// gen r4w3/cards3: `pub`, because the barrier-heavy randomized
/// tests (`gen_r4w3_cards3_tests` here and in `gen_heap.rs`) make
/// thousands of DUPLICATE lock-free marks, and one landing inside the
/// window where an exact-count test has the gate armed would make that
/// test fail. They take this lock too.
pub static METRICS_ARM: Mutex<()> = Mutex::new(());

/// Brute-force reference implementation of the dirty set: one
/// [`CardTable::is_dirty`] per card, which is the per-card scan the
/// grouped one replaced.
fn dirty_by_brute_force(ct: &CardTable) -> Vec<usize> {
    (0..ct.num_cards()).filter(|&i| ct.is_dirty(i)).collect()
}

/// gengc-round2: the group probe must nominate exactly the cards the old
/// per-card scan would have found — including at every group boundary,
/// which is where an off-by-one in the `base + j` reconstruction would
/// hide.
///
/// This is the equivalence the perf change rests on: `take_dirty_cards`
/// and `dirty_card_indices` now examine a card individually only if an
/// OR-fold over its group of [`SCAN_GROUP`] said something in there was
/// non-clean, so "same card set" is a claim about the grouping arithmetic.
#[test]
fn the_group_probe_finds_exactly_what_a_per_card_scan_finds() {
    // Deliberately NOT a multiple of SCAN_GROUP, so the last group is
    // short and the `chunks` remainder is exercised.
    let num = SCAN_GROUP * 5 + 3;
    let ct = CardTable::new(0x10_0000, CARD_SIZE * num);
    assert_eq!(ct.num_cards(), num);

    // First of a group, last of a group, both sides of a boundary, a lone
    // card in an otherwise clean group, and the very last (short-group)
    // card.
    let expected: Vec<usize> = vec![0, SCAN_GROUP - 1, SCAN_GROUP, SCAN_GROUP * 3 + 4, num - 1];
    for &i in &expected {
        ct.mark_dirty_lockfree(0x10_0000 + i * CARD_SIZE);
    }
    assert_eq!(dirty_by_brute_force(&ct), expected);
    assert_eq!(ct.dirty_card_indices(), expected);
    assert_eq!(ct.take_dirty_cards(), expected);
    // Consumed, so the bitmap-to-list invariant still holds.
    assert!(dirty_by_brute_force(&ct).is_empty());

    // And `clear_all` must still reach every byte through the same probe.
    for &i in &expected {
        ct.mark_dirty(0x10_0000 + i * CARD_SIZE);
    }
    assert_eq!(dirty_by_brute_force(&ct), expected);
    ct.clear_all();
    assert!(dirty_by_brute_force(&ct).is_empty());
    assert!(ct.take_dirty_cards().is_empty());
}

/// gengc-round4/cards: the bound is self-maintained. A fresh table has
/// dirtied nothing, so it scans nothing; each mark path raises the bound
/// to one past the card it dirties; consuming or clearing a card never
/// lowers it. (Round 2's `an_unwired_table_scans_the_whole_map` pinned the
/// old UNSET-initialised default and is replaced by this.)
#[test]
fn the_scan_bound_tracks_the_highest_card_ever_dirtied() {
    let ct = CardTable::new(0x1000, CARD_SIZE * 1024);
    assert_eq!(ct.scan_bound_cards(), 0, "nothing dirtied, nothing to scan");
    assert!(ct.take_dirty_cards().is_empty());
    assert_eq!(ct.cards_skipped_by_scan_bound(), 1024);

    ct.mark_dirty_lockfree(0x1000 + 39 * CARD_SIZE + 17);
    assert_eq!(ct.scan_bound_cards(), 40);
    assert_eq!(ct.take_dirty_cards(), vec![39]);
    assert_eq!(ct.cards_skipped_by_scan_bound(), 1024 + (1024 - 40));

    // Consumed — but the bound does not come back down, so a card below
    // it that is dirtied later is still inside the scan.
    assert_eq!(ct.scan_bound_cards(), 40);
    ct.mark_dirty_lockfree(0x1000 + 3 * CARD_SIZE);
    assert_eq!(ct.scan_bound_cards(), 40);
    assert_eq!(ct.take_dirty_cards(), vec![3]);

    // The last card of the map covers the whole map, and a clear keeps it.
    ct.mark_dirty(0x1000 + 1023 * CARD_SIZE);
    assert_eq!(ct.scan_bound_cards(), 1024);
    ct.clear_all();
    assert_eq!(ct.scan_bound_cards(), 1024);
    assert!(!ct.raw_card_address_escaped());
}

/// gengc-round4/cards: every card a Rust path can dirty is found by every
/// scan, on a table whose bound is self-maintained — the equivalence the
/// change rests on, checked against a brute-force `is_dirty` sweep (which
/// ignores the bound) for all three scans and all four mark paths.
#[test]
fn a_self_maintained_bound_never_hides_a_dirty_card() {
    let base = 0x10_0000usize;
    let num = 300usize;
    let marks: [usize; 6] = [0, 7, 8, 150, 151, 299];
    for path in 0..4 {
        let ct = CardTable::new(base, CARD_SIZE * num);
        for &i in &marks {
            let addr = base + i * CARD_SIZE + 5;
            match path {
                0 => ct.mark_dirty_lockfree(addr),
                1 => ct.mark_dirty(addr),
                2 => ct.mark_dirty_bulk(&[addr]),
                _ => {
                    ct.thread_local_dirty_addr(addr);
                    ct.flush_dirty_buffer();
                    ct.drain_pending();
                }
            }
        }
        let expected: Vec<usize> = marks.to_vec();
        assert_eq!(dirty_by_brute_force(&ct), expected, "path {path}");
        assert_eq!(ct.dirty_card_indices(), expected, "path {path}");
        assert_eq!(ct.take_dirty_cards(), expected, "path {path}");
        for &i in &marks {
            ct.mark_dirty_lockfree(base + i * CARD_SIZE);
        }
        ct.clear_all();
        assert!(dirty_by_brute_force(&ct).is_empty(), "path {path}");
    }
}

/// The bound is monotone upward: a lower publish is ignored, a higher one
/// raises it. A race between two publishers can therefore only ever make
/// the scan cover MORE cards.
#[test]
fn the_scan_bound_only_ever_moves_up() {
    let ct = CardTable::new(0x1000, CARD_SIZE * 64);
    assert_eq!(ct.scan_bound_cards(), 0, "self-maintained: starts empty");

    ct.set_scan_bound_bytes(CARD_SIZE * 8);
    assert_eq!(ct.scan_bound_cards(), 8, "a publish raises it");

    ct.set_scan_bound_bytes(CARD_SIZE * 4);
    assert_eq!(ct.scan_bound_cards(), 8, "a lower publish is ignored");

    ct.set_scan_bound_bytes(CARD_SIZE * 20 + 1);
    assert_eq!(ct.scan_bound_cards(), 21, "a partial card still counts");

    ct.set_scan_bound_bytes(usize::MAX);
    assert_eq!(ct.scan_bound_cards(), 64, "and it clamps to the map");
}

/// A published bound stops the scans at it, and the skip counter says how
/// much that bought — so "the bound is on and buying nothing" is
/// distinguishable from "the bound is off".
#[test]
fn a_published_bound_stops_the_scan_and_says_what_it_skipped() {
    let ct = CardTable::new(0x1000, CARD_SIZE * 1024);
    ct.set_scan_bound_bytes(CARD_SIZE * 16);
    assert_eq!(ct.scan_bound_cards(), 16);

    ct.mark_dirty_lockfree(0x1000 + 3 * CARD_SIZE);
    assert_eq!(ct.take_dirty_cards(), vec![3], "below the bound is found");
    assert_eq!(
        ct.cards_skipped_by_scan_bound(),
        (1024 - 16) as u64,
        "one pass skipped everything above the bound",
    );
}

/// Every mark path the COLLECTOR and the Rust barrier use raises a stale
/// bound, so a card dirtied above one cannot be lost.
///
/// This is the belt-and-braces half of the safety argument in
/// `CardTable::scan_bound`'s doc: promotion during a pause moves the old
/// generation's high-water mark, and the deferred-card re-mark that
/// follows it (`mark_dirty_bulk`) lands above whatever bound was published
/// before the pause.
#[test]
fn every_rust_mark_path_raises_a_stale_scan_bound() {
    let base = 0x1000usize;

    // mark_dirty_lockfree
    let ct = CardTable::new(base, CARD_SIZE * 256);
    ct.set_scan_bound_bytes(CARD_SIZE * 4);
    ct.mark_dirty_lockfree(base + 100 * CARD_SIZE);
    assert!(ct.scan_bound_cards() > 100);
    assert_eq!(ct.take_dirty_cards(), vec![100]);

    // mark_dirty
    let ct = CardTable::new(base, CARD_SIZE * 256);
    ct.set_scan_bound_bytes(CARD_SIZE * 4);
    ct.mark_dirty(base + 100 * CARD_SIZE);
    assert_eq!(ct.take_dirty_cards(), vec![100]);

    // mark_dirty_bulk — the deferred-card re-mark after promotion.
    let ct = CardTable::new(base, CARD_SIZE * 256);
    ct.set_scan_bound_bytes(CARD_SIZE * 4);
    ct.mark_dirty_bulk(&[base + 100 * CARD_SIZE, base + 200 * CARD_SIZE]);
    assert_eq!(ct.take_dirty_cards(), vec![100, 200]);

    // drain_pending — the buffered path, for as long as it exists.
    let ct = CardTable::new(base, CARD_SIZE * 256);
    ct.set_scan_bound_bytes(CARD_SIZE * 4);
    ct.thread_local_dirty(100 * CARD_SIZE);
    ct.flush_dirty_buffer();
    assert_eq!(ct.drain_pending(), 1);
    assert_eq!(ct.take_dirty_cards(), vec![100]);
}

/// The hazard that kept round 2's bound unpublished, and why it is now
/// closed structurally rather than by nobody publishing.
///
/// A store made ONLY through `jit_cards_addr` — which is what generated
/// code does, and what `Compiler::inline_card_mark_available()` would
/// re-enable — bypasses every Rust mark path, so it cannot raise the
/// bound. Round 2 pinned the resulting loss (`a_raw_store_above_a_published
/// _bound_is_why_nothing_publishes_one`) and asked for the mechanism to be
/// re-derived if a raw store were ever found. It was
/// (gengc-round4/cards): obtaining the address sets
/// `raw_card_address_escaped`, which disarms the bound for the table's
/// life, so a raw store anywhere — above the self-maintained bound AND
/// above an explicit publish — is found.
#[test]
fn handing_out_the_raw_address_disarms_the_scan_bound() {
    let ct = CardTable::new(0x1000, CARD_SIZE * 64);
    ct.mark_dirty_lockfree(0x1000 + 2 * CARD_SIZE);
    assert_eq!(ct.scan_bound_cards(), 3);
    assert!(!ct.raw_card_address_escaped());

    let cards = ct.jit_cards_addr() as *const AtomicU8;
    assert!(ct.raw_card_address_escaped());
    assert_eq!(ct.scan_bound_cards(), 64, "the whole map again");

    // SAFETY: `cards` is the table's live `[AtomicU8; 64]` and 40 < 64.
    unsafe { &*cards.add(40) }.store(CARD_DIRTY, Ordering::Release);
    assert_eq!(ct.take_dirty_cards(), vec![2, 40]);

    // An explicit publish below the raw store cannot hide it either.
    ct.set_scan_bound_bytes(CARD_SIZE * 8);
    // SAFETY: as above.
    unsafe { &*cards.add(40) }.store(CARD_DIRTY, Ordering::Release);
    assert_eq!(
        ct.take_dirty_cards(),
        vec![40],
        "once the address has escaped, no bound may hide a raw store",
    );
    assert_eq!(ct.cards_skipped_by_scan_bound(), 0);
}

/// gengc-round2: the barrier's duplicate marks must measure the barrier
/// that is actually running.
///
/// The counter used to be fed only by `drain_pending`, which has drained
/// nothing since gc-genpause F5.2 moved the barrier to
/// `mark_dirty_lockfree` — so the number `duplicate_mark_ratio` is built on
/// was structurally zero and read as "the barrier never duplicates". The
/// round-1 probe run recorded exactly that: `duplicate_marks=0` beside
/// `dirty_scanned=1980`.
///
/// gengc-round3: it must also stay **divisible by its denominator**. Each
/// `mark_dirty_lockfree` below is followed by a `record_card_mark`, which
/// is exactly what `GenerationalHeap::write_barrier` does on both arms of
/// this branch — so this test drives the production pairing, and asserts
/// `duplicate_card_marks_barrier <= card_marks_executed`. That inequality
/// holds by construction and is the cheapest guard against the numerator
/// and the denominator being fed from different places later, which is the
/// defect this whole gap page is about.
#[test]
fn the_lockfree_barrier_attributes_its_own_duplicate_marks() {
    let _arm = METRICS_ARM.lock();
    crate::gc_metrics::reset_metrics_for_test();
    let restore = crate::gc_metrics::hot_path_counters_enabled();
    crate::gc_metrics::set_hot_path_counters_enabled(true);

    let ct = CardTable::new(0x1000, CARD_SIZE * 8);
    // Three distinct cards, then five more stores that all land on cards
    // already dirty: 3 clean→dirty transitions, 5 duplicates.
    for off in [
        0usize,
        CARD_SIZE,
        2 * CARD_SIZE,
        8,
        16,
        CARD_SIZE + 8,
        24,
        32,
    ] {
        ct.mark_dirty_lockfree(0x1000 + off);
        // What `write_barrier` does next, unconditionally, on both arms.
        crate::gc_metrics::record_card_mark();
    }
    assert_eq!(ct.take_dirty_cards(), vec![0, 1, 2]);
    let raw = crate::gc_metrics::gc_metrics_raw();
    assert_eq!(
        raw.duplicate_card_marks_barrier, 5,
        "five stores hit a card that was already dirty",
    );
    assert_eq!(
        raw.duplicate_card_marks_buffered, 0,
        "nothing was buffered; the barrier half must not leak into the \
         buffered half",
    );
    assert_eq!(
        raw.duplicate_card_marks, 5,
        "the legacy total is the sum of the two halves, so its value is \
         unchanged by the split",
    );
    assert_eq!(raw.card_marks_executed, 8, "eight barrier marks in total");
    assert!(
        raw.duplicate_card_marks_barrier <= raw.card_marks_executed,
        "the duplicate numerator must never exceed its denominator: \
         {} > {}",
        raw.duplicate_card_marks_barrier,
        raw.card_marks_executed,
    );
    assert_eq!(
        crate::gc_metrics::gc_metrics_report().barrier_duplicate_mark_ratio,
        5.0 / 8.0,
        "and the published ratio is that division, nothing else",
    );

    // Gated off, the barrier counts nothing — which is what keeps a
    // process-global `fetch_add` off the hottest path in the VM. The
    // numerator and the denominator disarm TOGETHER, because the gate is
    // inside `record_duplicate_card_mark_barrier` rather than at this
    // call site.
    crate::gc_metrics::set_hot_path_counters_enabled(false);
    for off in [0usize, 8, 16] {
        ct.mark_dirty_lockfree(0x1000 + off);
        crate::gc_metrics::record_card_mark();
    }
    let raw = crate::gc_metrics::gc_metrics_raw();
    assert_eq!(
        raw.duplicate_card_marks_barrier, 5,
        "the duplicate counter is gated exactly like `record_card_mark`",
    );
    assert_eq!(
        raw.card_marks_executed, 8,
        "and so is its denominator — a ratio whose two terms are armed by \
         different flags is not a ratio",
    );
    crate::gc_metrics::set_hot_path_counters_enabled(restore);
}

/// gc-genpause F5.2: the lock-free barrier and the buffered pipeline it
/// replaces must dirty exactly the same cards.
///
/// This is the equivalence the change rests on. The two paths reach the
/// byte map by completely different routes -- one stores into it directly,
/// the other queues a byte offset through a per-thread buffer for
/// `drain_pending` to fold in at a safepoint -- so "same card set" is a
/// claim about the address arithmetic in both, not something the types
/// enforce.
#[test]
fn the_lockfree_barrier_dirties_the_same_cards_as_the_buffered_path() {
    const BASE: usize = 0x10_0000;
    const SIZE: usize = CARD_SIZE * 64;
    // Addresses spanning card boundaries, exact starts, interiors, the
    // last byte of the region, and two that share a card.
    let addrs = [
        BASE,
        BASE + 1,
        BASE + CARD_SIZE - 1,
        BASE + CARD_SIZE,
        BASE + CARD_SIZE * 7 + 13,
        BASE + CARD_SIZE * 7 + 14,
        BASE + CARD_SIZE * 63,
        BASE + SIZE - 1,
        // Out of range in both directions: neither path may record these.
        BASE - 1,
        BASE + SIZE,
        BASE + SIZE + CARD_SIZE * 4,
    ];

    let buffered = CardTable::new(BASE, SIZE);
    for &a in &addrs {
        buffered.thread_local_dirty_addr(a);
    }
    buffered.flush_all();
    buffered.drain_pending();
    let via_buffer = buffered.take_dirty_cards();

    let direct = CardTable::new(BASE, SIZE);
    for &a in &addrs {
        direct.mark_dirty_lockfree(a);
    }
    let via_direct = direct.take_dirty_cards();

    assert_eq!(
        via_direct, via_buffer,
        "the lock-free barrier must dirty exactly the cards the buffered path did"
    );
    assert!(!via_direct.is_empty(), "the fixture must dirty something");
    // And specifically: the two addresses sharing card 7 produce ONE card.
    assert_eq!(
        via_direct.iter().filter(|&&c| c == 7).count(),
        1,
        "two stores into one card are one dirty card"
    );
}

/// The conditional store in `mark_dirty_lockfree` must not turn a repeat
/// mark into a no-op that loses the card -- an already-dirty card stays
/// dirty, and is still reported once.
#[test]
fn repeated_lockfree_marks_of_one_card_stay_dirty() {
    let ct = CardTable::new(0x1000, CARD_SIZE * 4);
    for _ in 0..1000 {
        ct.mark_dirty_lockfree(0x1000 + CARD_SIZE * 2 + 8);
    }
    assert!(ct.is_dirty(2));
    assert_eq!(ct.take_dirty_cards(), vec![2]);
    // Consumed: the byte is clean again, so a re-mark genuinely
    // re-registers it (the bt18 bitmap-to-list invariant).
    assert!(!ct.is_dirty(2));
    ct.mark_dirty_lockfree(0x1000 + CARD_SIZE * 2 + 8);
    assert_eq!(ct.take_dirty_cards(), vec![2]);
}

/// A dropped `CardTable` must not leave its bucket behind in the
/// per-thread buffers.
///
/// The bucket is only ever removed by `flush_all`/`flush_dirty_buffer` on
/// the OWNING table, so a bucket whose table is gone is unreachable
/// garbage that also keeps `DirtyBufferGuard::drop` from ever deregistering
/// an exiting thread's entry (it refuses while any bucket is non-empty),
/// making `BUFFER_REGISTRY` grow without bound. `Drop for CardTable` is
/// what retires it.
///
/// Deliberately asserts on THIS THREAD'S OWN buffer rather than on
/// `BUFFER_REGISTRY`'s length: the registry is process-global and every
/// other test thread in this binary registers into it, so a length
/// assertion would be exactly the cross-test coupling
/// `scripts/gc-flake-gate.sh` exists to catch.
#[test]
fn dropping_a_table_retires_its_per_thread_bucket() {
    fn buffer_has(table_id: u64) -> bool {
        THREAD_DIRTY_BUFFER.with(|guard| {
            guard
                .buffer
                .lock()
                .buckets
                .iter()
                .any(|(id, _)| *id == table_id)
        })
    }

    const KEEP_BASE: usize = 0x30_0000;
    const DOOMED_BASE: usize = 0x40_0000;
    let keep = CardTable::new(KEEP_BASE, CARD_SIZE * 4);
    keep.thread_local_dirty_addr(KEEP_BASE + 8);

    let doomed_id = {
        let doomed = CardTable::new(DOOMED_BASE, CARD_SIZE * 4);
        doomed.thread_local_dirty_addr(DOOMED_BASE + 8);
        let id = doomed.id;
        assert!(buffer_has(id), "the fixture must buffer an offset");
        id
    };

    assert!(
        !buffer_has(doomed_id),
        "a dropped table's bucket must not outlive it — nothing else can \
         ever take it, and while it is there the owning thread can never \
         deregister from BUFFER_REGISTRY",
    );
    // The LIVE table's bucket is untouched, and still drains.
    assert!(
        buffer_has(keep.id),
        "dropping one table must not consume another's buffered offsets",
    );
    keep.flush_all();
    keep.drain_pending();
    assert_eq!(keep.take_dirty_cards(), vec![0]);
}

/// gc-genpause F3: `clear_all` writes only the dirty bytes now, so prove
/// it still leaves every card clean -- including one dirtied by the
/// lock-free barrier, which never touches the tracking list.
#[test]
fn conditional_clear_all_still_clears_every_card() {
    let ct = CardTable::new(0x1000, CARD_SIZE * 8);
    ct.mark_dirty_lockfree(0x1000 + CARD_SIZE * 3);
    ct.mark_dirty(0x1000 + CARD_SIZE * 5);
    assert!(ct.is_dirty(3) && ct.is_dirty(5));
    ct.clear_all();
    for i in 0..ct.num_cards() {
        assert!(!ct.is_dirty(i), "card {i} survived clear_all");
    }
    assert!(ct.take_dirty_cards().is_empty());
}

/// gc-common w1-g: a buffered card mark made from ANOTHER thread-local's
/// destructor must neither abort (`LocalKey::with` panics on a destroyed
/// key) nor lose the card, whichever order the platform tears the two
/// thread-locals down in.
#[test]
fn a_buffered_mark_from_a_tls_destructor_is_not_lost() {
    struct MarkOnDrop(Arc<CardTable>, usize);
    impl Drop for MarkOnDrop {
        fn drop(&mut self) {
            self.0.thread_local_dirty_addr(self.1);
        }
    }
    thread_local! {
        static LATE: std::cell::RefCell<Option<MarkOnDrop>> =
            const { std::cell::RefCell::new(None) };
    }
    const BASE: usize = 0x40_0000;
    let ct = Arc::new(CardTable::new(BASE, CARD_SIZE * 16));
    let ct2 = Arc::clone(&ct);
    std::thread::spawn(move || {
        // LATE registered first, the dirty buffer second: on
        // reverse-order platforms the buffer dies before LATE's drop.
        let late = MarkOnDrop(Arc::clone(&ct2), BASE + CARD_SIZE * 9);
        LATE.with(|l| *l.borrow_mut() = Some(late));
        ct2.thread_local_dirty_addr(BASE + CARD_SIZE * 2);
    })
    .join()
    .expect("a card mark from a TLS destructor must not abort the thread");
    ct.flush_all();
    ct.drain_pending();
    let taken = ct.take_dirty_cards();
    assert!(
        taken.contains(&9),
        "the destructor's card was lost: {taken:?}"
    );
    assert!(
        taken.contains(&2),
        "the ordinary buffered card was lost: {taken:?}"
    );
}

#[test]
fn direct_jit_atomic_mark_is_visible_to_stw_consumer() {
    let ct = CardTable::new(0x1000, CARD_SIZE * 4);
    let cards = ct.jit_cards_addr() as *const AtomicU8;
    // Model the generated x64 release byte-store for card 2.
    unsafe { &*cards.add(2) }.store(CARD_DIRTY, Ordering::Release);
    assert_eq!(ct.take_dirty_cards(), vec![2]);
    assert!(!ct.is_dirty(2));
}

/// `drain_pending` is the only place in the system that can tell a
/// *buffered* card mark that dirtied a clean card from one that hit an
/// already-dirty card, so it is where the buffered duplicate-mark counter
/// is attributed. The arithmetic must be exact — `duplicates = offsets
/// consumed - clean→dirty transitions`.
///
/// gengc-round3: it is attributed to `duplicate_card_marks_buffered`, and
/// this test now asserts that the barrier half stays at zero. The two are
/// on different clocks and different gates; the argument for or against a
/// per-thread last-card filter in the barrier is
/// `barrier_duplicate_mark_ratio`, which this path must not touch.
#[test]
fn drain_pending_attributes_duplicate_card_marks() {
    // gengc-round2: `duplicate_card_marks` now has a second writer
    // (`mark_dirty_lockfree`, gated), and both this test and
    // `the_lockfree_barrier_attributes_its_own_duplicate_marks` assert an
    // exact value of a PROCESS-GLOBAL counter. Serialise them.
    let _arm = METRICS_ARM.lock();
    crate::gc_metrics::reset_metrics_for_test();
    let ct = CardTable::new(0x1000, CARD_SIZE * 4);

    // Six buffered marks landing on three distinct cards: 3 clean→dirty
    // transitions and 3 duplicates.
    //
    // gengc-round4: the cards ALTERNATE (0,1,0,1,2,0) rather than running
    // (0,0,1,1,2,0). `DirtyPartitions::push` now drops a store whose card
    // equals the bucket's last entry's, so the running order would buffer
    // four entries and one duplicate — a different measurement. The
    // alternating order defeats the last-entry filter exactly, which keeps
    // this test about the DRAIN's attribution, where it belongs. The
    // filter has its own test:
    // `a_run_of_stores_into_one_card_collapses_to_a_single_buffered_entry`.
    for offset in [0usize, CARD_SIZE, 4, CARD_SIZE + 8, 2 * CARD_SIZE, 8] {
        ct.thread_local_dirty(offset);
    }
    ct.flush_dirty_buffer();
    assert_eq!(ct.pending_count(), 6);

    assert_eq!(ct.drain_pending(), 3, "three distinct cards became dirty");
    let raw = crate::gc_metrics::gc_metrics_raw();
    assert_eq!(
        raw.duplicate_card_marks_buffered, 3,
        "the other three offsets hit a card that was already dirty",
    );
    assert_eq!(
        raw.duplicate_card_marks_barrier, 0,
        "gengc-round3: the drain feeds the BUFFERED half only. If this \
         ever moves, the per-store barrier counter is being fed from an \
         ungated per-drain site and `barrier_duplicate_mark_ratio` has \
         stopped being a fraction of `card_marks_executed`.",
    );
    assert_eq!(
        raw.duplicate_card_marks, 3,
        "the legacy total still reads exactly as it did before the split",
    );

    // A second drain with nothing pending must not invent duplicates.
    assert_eq!(ct.drain_pending(), 0);
    assert_eq!(
        crate::gc_metrics::gc_metrics_raw().duplicate_card_marks,
        3,
        "an empty drain is not a duplicate mark",
    );

    // Re-dirtying the SAME cards after they have been consumed is three
    // genuine clean→dirty transitions again, not three duplicates.
    assert_eq!(ct.take_dirty_cards().len(), 3);
    for offset in [0usize, CARD_SIZE, 2 * CARD_SIZE] {
        ct.thread_local_dirty(offset);
    }
    ct.flush_dirty_buffer();
    assert_eq!(ct.drain_pending(), 3);
    assert_eq!(crate::gc_metrics::gc_metrics_raw().duplicate_card_marks, 3);
}

/// The remembered set's space cost, measured rather than assumed: one byte
/// per [`CARD_SIZE`] bytes of covered region, plus the dirty-index tracking
/// list, plus the undrained pending queue.
#[test]
fn retained_bytes_accounts_for_the_byte_map_and_the_queues() {
    let region = CARD_SIZE * 64;
    // gen r4w3/cards3: the exact-size claim is about the LEGACY table; a
    // summary adds one byte per `SUMMARY_GROUP` cards, checked below.
    let ct = CardTable::with_options(0x1000, region, CardTableOptions::LEGACY);
    let empty = ct.retained_bytes();
    assert_eq!(
        empty,
        region / CARD_SIZE,
        "an untouched table costs exactly the card byte-map (1/512 of the \
         covered region)",
    );
    let with_summary = CardTable::with_options(
        0x1000,
        region,
        CardTableOptions {
            summary: true,
            precise_ref_array_marks: false,
        },
    );
    assert_eq!(
        with_summary.retained_bytes(),
        region / CARD_SIZE + (region / CARD_SIZE).div_ceil(SUMMARY_GROUP),
        "the summary map costs one byte per SUMMARY_GROUP cards",
    );

    for i in 0..16 {
        ct.thread_local_dirty(i * CARD_SIZE);
    }
    ct.flush_dirty_buffer();
    assert!(
        ct.retained_bytes() > empty,
        "buffered old→young edges are retained memory too",
    );
    ct.drain_pending();
    // gcd d1/d: the drain folds the queue into the byte map and releases
    // it; there is no dirty-card index beside the map any more (it was a
    // duplicate of the byte scan, see `CardCells`), so the table is back to
    // the map's own cost.
    assert_eq!(
        ct.retained_bytes(),
        empty,
        "a drained table retains only its byte map",
    );
    assert_eq!(ct.take_dirty_cards(), (0..16).collect::<Vec<usize>>());
}

/// `card_start_addr` and `card_end_addr` must bracket a well-formed range
/// for EVERY index, including ones no caller should ever pass.
///
/// gengc-round1 2026-09-20. `card_end_addr` has always clamped to
/// `region_end` while `card_start_addr` saturated to `usize::MAX`, so an
/// out-of-range or overflowing index produced `start > end`. The two
/// obvious things a caller does with such a pair — iterate `start..end`,
/// or compute `end - start` — silently skip the card or panic on
/// underflow respectively. Neither failure names the card table.
#[test]
fn a_card_address_pair_is_never_inverted_at_any_index() {
    // A region that is NOT a whole number of cards, so the last card is
    // partial and the clamp is load-bearing for an in-range index too.
    let region = CARD_SIZE * 4 + 16;
    let ct = CardTable::new(0x1_0000, region);
    let region_end = 0x1_0000 + region;
    assert_eq!(ct.num_cards(), 5, "the partial tail gets its own card");

    for i in 0..ct.num_cards() {
        let (s, e) = (ct.card_start_addr(i), ct.card_end_addr(i));
        assert!(s < e, "card {i} has an empty or inverted range");
        assert_eq!(s, 0x1_0000 + i * CARD_SIZE);
        assert!(e <= region_end, "card {i} runs past the covered region");
    }
    // The partial last card stops at the region end, not at a card
    // boundary.
    assert_eq!(ct.card_end_addr(4), region_end);

    // Out of range, and far enough out to overflow `index * CARD_SIZE`.
    for i in [ct.num_cards(), usize::MAX / 2, usize::MAX] {
        let (s, e) = (ct.card_start_addr(i), ct.card_end_addr(i));
        assert!(
            s <= e,
            "index {i} produced an inverted range {s:#x}..{e:#x}"
        );
        assert_eq!(e - s, 0, "an out-of-range card must cover nothing");
    }
}

#[test]
fn new_card_table_all_clean() {
    let ct = CardTable::new(0x1000, 4096);
    assert_eq!(ct.num_cards(), 8); // 4096 / 512
    for i in 0..ct.num_cards() {
        assert!(!ct.is_dirty(i));
    }
}

#[test]
fn mark_dirty_and_check() {
    let ct = CardTable::new(0x1000, 4096);
    // Mark card covering address 0x1000 (card 0)
    ct.mark_dirty(0x1000);
    assert!(ct.is_dirty(0));
    assert!(!ct.is_dirty(1));

    // Mark card covering address 0x1400 (offset 0x400 = 1024, card 2)
    ct.mark_dirty(0x1400);
    assert!(ct.is_dirty(2));
}

#[test]
fn mark_dirty_last_byte_of_card() {
    let ct = CardTable::new(0x0, 2048);
    // Last byte of card 0: offset 511
    ct.mark_dirty(511);
    assert!(ct.is_dirty(0));
    assert!(!ct.is_dirty(1));

    // First byte of card 1: offset 512
    ct.mark_dirty(512);
    assert!(ct.is_dirty(1));
}

#[test]
fn clear_all() {
    let ct = CardTable::new(0x0, 2048);
    ct.mark_dirty(0);
    ct.mark_dirty(512);
    ct.mark_dirty(1024);
    assert_eq!(ct.dirty_card_indices().len(), 3);

    ct.clear_all();
    assert_eq!(ct.dirty_card_indices().len(), 0);
    for i in 0..ct.num_cards() {
        assert!(!ct.is_dirty(i));
    }
}

#[test]
fn dirty_card_indices() {
    let ct = CardTable::new(0x0, 4096);
    ct.mark_dirty(0); // card 0
    ct.mark_dirty(1536); // card 3 (1536 / 512 = 3)
    ct.mark_dirty(3584); // card 7 (3584 / 512 = 7)

    let dirty: Vec<usize> = ct.dirty_card_indices();
    assert_eq!(dirty, vec![0, 3, 7]);
}

#[test]
fn card_start_and_end_addr() {
    let ct = CardTable::new(0x1000, 2048);
    assert_eq!(ct.card_start_addr(0), 0x1000);
    assert_eq!(ct.card_end_addr(0), 0x1000 + 512);
    assert_eq!(ct.card_start_addr(1), 0x1000 + 512);
    assert_eq!(ct.card_end_addr(1), 0x1000 + 1024);

    // Last card's end should be clamped to base + region_size
    assert_eq!(ct.card_end_addr(3), 0x1000 + 2048);
}

#[test]
fn non_aligned_region_size() {
    // 1000 bytes → 2 cards (ceil(1000/512) = 2)
    let ct = CardTable::new(0x0, 1000);
    assert_eq!(ct.num_cards(), 2);
    // Last card's end is clamped
    assert_eq!(ct.card_end_addr(1), 1000);
}

#[test]
fn is_dirty_out_of_bounds_returns_false() {
    let ct = CardTable::new(0x0, 1024);
    assert!(!ct.is_dirty(999));
}

// -----------------------------------------------------------------------
// Additional tests
// -----------------------------------------------------------------------

#[test]
fn mark_dirty_outside_region_is_ignored() {
    let ct = CardTable::new(0x1000, 1024);
    // Address below base
    ct.mark_dirty(0x0);
    // Address above region
    ct.mark_dirty(0x1000 + 1024);
    ct.mark_dirty(0xFFFF);

    // No cards should be dirty
    assert_eq!(ct.dirty_card_indices().len(), 0);
}

#[test]
fn mark_dirty_first_card() {
    let ct = CardTable::new(0x0, 4096);
    ct.mark_dirty(0);
    assert!(ct.is_dirty(0));
    assert!(!ct.is_dirty(1));
}

#[test]
fn mark_dirty_last_card() {
    let ct = CardTable::new(0x0, 4096);
    // Last card covers bytes 3584..4095
    ct.mark_dirty(4095);
    let last_card = ct.num_cards() - 1;
    assert!(ct.is_dirty(last_card));
}

#[test]
fn dirty_multiple_regions() {
    let ct = CardTable::new(0x0, 8192);
    // Dirty cards 0, 3, 7, 15
    ct.mark_dirty(0); // card 0
    ct.mark_dirty(1536); // card 3
    ct.mark_dirty(3584); // card 7
    ct.mark_dirty(7680); // card 15

    let dirty: Vec<usize> = ct.dirty_card_indices();
    assert_eq!(dirty, vec![0, 3, 7, 15]);
}

#[test]
fn clear_then_dirty_again() {
    let ct = CardTable::new(0x0, 2048);
    ct.mark_dirty(0);
    ct.mark_dirty(512);
    assert_eq!(ct.dirty_card_indices().len(), 2);

    ct.clear_all();
    assert_eq!(ct.dirty_card_indices().len(), 0);

    // Re-dirty after clear
    ct.mark_dirty(1024);
    let dirty: Vec<usize> = ct.dirty_card_indices();
    assert_eq!(dirty, vec![2]);
}

#[test]
fn card_addresses_with_nonzero_base() {
    let base = 0x8000usize;
    let ct = CardTable::new(base, 2048);

    assert_eq!(ct.card_start_addr(0), 0x8000);
    assert_eq!(ct.card_end_addr(0), 0x8000 + CARD_SIZE);
    assert_eq!(ct.card_start_addr(1), 0x8000 + CARD_SIZE);
    assert_eq!(ct.card_end_addr(1), 0x8000 + 2 * CARD_SIZE);
}

#[test]
fn mark_same_card_idempotent() {
    let ct = CardTable::new(0x0, 4096);
    ct.mark_dirty(100);
    ct.mark_dirty(200);
    ct.mark_dirty(300);
    // All within card 0 (offsets 0..511)
    assert!(ct.is_dirty(0));
    assert_eq!(ct.dirty_card_indices().len(), 1);
}

#[test]
fn all_cards_dirty() {
    let ct = CardTable::new(0x0, 2048);
    let num = ct.num_cards();
    for i in 0..num {
        ct.mark_dirty(i * CARD_SIZE);
    }
    assert_eq!(ct.dirty_card_indices().len(), num);
}

#[test]
fn single_byte_region() {
    let ct = CardTable::new(0x0, 1);
    assert_eq!(ct.num_cards(), 1);
    assert_eq!(ct.card_end_addr(0), 1);
}

#[test]
fn card_size_boundary_region() {
    // Region exactly CARD_SIZE bytes
    let ct = CardTable::new(0x0, CARD_SIZE);
    assert_eq!(ct.num_cards(), 1);
    assert_eq!(ct.card_start_addr(0), 0);
    assert_eq!(ct.card_end_addr(0), CARD_SIZE);
}

#[test]
fn card_size_plus_one_region() {
    let ct = CardTable::new(0x0, CARD_SIZE + 1);
    assert_eq!(ct.num_cards(), 2);
    assert_eq!(ct.card_end_addr(1), CARD_SIZE + 1);
}

#[test]
fn mark_dirty_at_exact_base() {
    let ct = CardTable::new(0x5000, 4096);
    ct.mark_dirty(0x5000);
    assert!(ct.is_dirty(0));
}

#[test]
fn mark_dirty_at_region_end_minus_one() {
    let ct = CardTable::new(0x5000, 4096);
    ct.mark_dirty(0x5000 + 4095);
    let last = ct.num_cards() - 1;
    assert!(ct.is_dirty(last));
}

#[test]
fn mark_dirty_at_exact_region_end_is_ignored() {
    let ct = CardTable::new(0x5000, 4096);
    // Exactly at base + region_size is out of range
    ct.mark_dirty(0x5000 + 4096);
    assert_eq!(ct.dirty_card_indices().len(), 0);
}

// -----------------------------------------------------------------
// T5.5.2 — Thread-local batched dirty tests
// -----------------------------------------------------------------

#[test]
fn thread_local_dirty_accumulates_without_touching_cards() {
    let ct = CardTable::new(0x0, 8192);
    // Dirty one offset via the fast path — stays in the thread-local
    // buffer; the shared table sees nothing until a flush.
    ct.thread_local_dirty(0);
    // Not enough to auto-flush.
    assert_eq!(ct.pending_count(), 0);
    assert_eq!(ct.dirty_card_indices().len(), 0);

    // Explicit flush + drain.
    ct.flush_dirty_buffer();
    assert_eq!(ct.pending_count(), 1);
    let newly = ct.drain_pending();
    assert_eq!(newly, 1);
    assert!(ct.is_dirty(0));
}

#[test]
fn thread_local_dirty_auto_flushes_at_threshold() {
    // Use a large region so the threshold-worth of distinct card
    // offsets is well-defined.
    let ct = CardTable::new(0x0, CARD_SIZE * THREAD_BUFFER_FLUSH_THRESHOLD * 2);
    // Push exactly THREAD_BUFFER_FLUSH_THRESHOLD offsets — the last
    // one should auto-flush the buffer.
    for i in 0..THREAD_BUFFER_FLUSH_THRESHOLD {
        ct.thread_local_dirty(i * CARD_SIZE);
    }
    assert_eq!(ct.pending_count(), THREAD_BUFFER_FLUSH_THRESHOLD);

    let newly = ct.drain_pending();
    assert_eq!(newly, THREAD_BUFFER_FLUSH_THRESHOLD);
    for i in 0..THREAD_BUFFER_FLUSH_THRESHOLD {
        assert!(ct.is_dirty(i));
    }
}

#[test]
fn flush_dirty_buffer_is_idempotent_when_empty() {
    let ct = CardTable::new(0x0, 4096);
    ct.flush_dirty_buffer();
    assert_eq!(ct.pending_count(), 0);
    ct.flush_dirty_buffer();
    assert_eq!(ct.pending_count(), 0);
}

/// gengc-round4: the per-thread buffer now drops a store whose card equals
/// the bucket's LAST entry's, so a run of stores into one card collapses
/// before it ever reaches `pending_offsets`. The offsets here therefore
/// ALTERNATE cards, which is exactly the pattern the last-entry filter
/// cannot collapse — leaving `drain_pending`'s CAS as the authority on
/// what is a duplicate, which is what this test is about.
#[test]
fn drain_pending_deduplicates_same_card() {
    let ct = CardTable::new(0x0, 4096);
    // Cards 0, 1, 0 — three buffered entries, two distinct cards.
    ct.thread_local_dirty(0);
    ct.thread_local_dirty(CARD_SIZE + 88);
    ct.thread_local_dirty(100);
    ct.flush_dirty_buffer();
    assert_eq!(
        ct.pending_count(),
        3,
        "the last-entry filter must not collapse ALTERNATING cards — only runs of the same one",
    );
    let newly = ct.drain_pending();
    // Three offsets but only two new cards: the third names card 0 again.
    assert_eq!(newly, 2);
    assert!(ct.is_dirty(0) && ct.is_dirty(1));
    assert_eq!(ct.dirty_card_indices().len(), 2);
}

/// gengc-round4 — hazard (2) of
/// `gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`: a tight
/// loop writing ONE hot reference field used to queue one `usize` per
/// iteration, all of them naming the same card, through the 64-entry
/// auto-flush and into the unbounded shared `pending_offsets`. The buffer is now filtered against
/// its own last entry, so the whole run is one entry.
#[test]
fn a_run_of_stores_into_one_card_collapses_to_a_single_buffered_entry() {
    let ct = CardTable::new(0x0, CARD_SIZE * 4);
    // Ten thousand stores into one card — comfortably past
    // THREAD_BUFFER_FLUSH_THRESHOLD, so pre-fix this would have
    // auto-flushed ~156 times and left 10_000 entries pending.
    for i in 0..10_000usize {
        ct.thread_local_dirty(i % CARD_SIZE);
    }
    ct.flush_dirty_buffer();
    assert_eq!(
        ct.pending_count(),
        1,
        "a run of same-card stores must buffer exactly one offset",
    );
    assert_eq!(ct.drain_pending(), 1);
    assert!(ct.is_dirty(0));
    assert_eq!(ct.dirty_card_indices(), vec![0]);
}

#[test]
fn drain_pending_rejects_out_of_range_offset() {
    let ct = CardTable::new(0x0, 1024);
    // Offset past region_size — should be silently dropped.
    ct.thread_local_dirty(1024);
    ct.thread_local_dirty(99999);
    ct.flush_dirty_buffer();
    let newly = ct.drain_pending();
    assert_eq!(newly, 0);
}

#[test]
fn thread_local_dirty_addr_wraps_address_to_offset() {
    let ct = CardTable::new(0x10_0000, 4096);
    ct.thread_local_dirty_addr(0x10_0000 + 513); // card 1
    ct.flush_dirty_buffer();
    let newly = ct.drain_pending();
    assert_eq!(newly, 1);
    assert!(ct.is_dirty(1));
}

/// gengc-round4 — hazard (1) of
/// `gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`.
/// `clear_all` used to DROP the pending queue; it now folds it into the
/// freshly-cleaned bitmap.
///
/// The queue is still empty afterwards — that half of the old test's claim
/// is unchanged and still asserted — but the offsets in it are now cards,
/// not litter. Pre-fix this test's `take_dirty_cards` assertion failed,
/// which is precisely what the page said it would.
#[test]
fn clear_all_folds_pending_into_the_clean_bitmap() {
    let ct = CardTable::new(0x0, 4096);
    // Two distinct cards buffered under the auto-flush threshold and
    // moved to the shared queue by a peer-style explicit flush — the
    // exact shape a buffered mutator path would present at a pause whose
    // own flush+drain has already run.
    ct.thread_local_dirty(0);
    ct.thread_local_dirty(CARD_SIZE + 8);
    ct.flush_dirty_buffer();
    assert_eq!(ct.pending_count(), 2);

    ct.clear_all();

    assert_eq!(
        ct.pending_count(),
        0,
        "the queue is consumed, not left to be re-applied next cycle",
    );
    assert_eq!(
        ct.take_dirty_cards(),
        vec![0, 1],
        "a pending offset names a REAL old→young store: dropping it loses the edge for good and the next young collection frees the referent. Re-dirtying can only over-retain.",
    );
}

/// The fold must not resurrect a card the clear was there to retire: only
/// offsets that are genuinely PENDING come back.
#[test]
fn clear_all_folds_only_the_pending_queue_not_the_cards_it_cleared() {
    let ct = CardTable::new(0x0, CARD_SIZE * 8);
    // Card 5 is dirty in the bitmap and nowhere else.
    ct.mark_dirty(CARD_SIZE * 5);
    // Card 2 is only in the pending queue.
    ct.thread_local_dirty(CARD_SIZE * 2);
    ct.flush_dirty_buffer();

    ct.clear_all();

    assert_eq!(
        ct.take_dirty_cards(),
        vec![2],
        "card 5 was cleared and had nothing pending; card 2 was pending and comes back",
    );
}

#[test]
fn flush_all_drains_this_threads_buffer() {
    // T5.5.2 — `flush_all` is the GC-entry hook. For a single table on
    // the calling thread it folds this thread's buffered offsets into
    // `pending_offsets`, equivalent to `flush_dirty_buffer`.
    let ct = CardTable::new(0x0, 4096);
    ct.thread_local_dirty(0);
    assert_eq!(ct.pending_count(), 0);
    ct.flush_all();
    assert_eq!(ct.pending_count(), 1);
    let newly = ct.drain_pending();
    assert_eq!(newly, 1);
    assert!(ct.is_dirty(0));
}

#[test]
fn dying_thread_residual_offsets_survive_until_flush_all() {
    // DoHead freed-while-live regression (2026-07-15): a mutator thread
    // that buffers an old→young edge and EXITS without flushing (fewer
    // than THREAD_BUFFER_FLUSH_THRESHOLD entries buffered) must not lose
    // the record — the edge lives in the heap and outlives the thread.
    // Pre-fix, `DirtyBufferGuard::drop` deregistered the buffer and the
    // offsets were silently discarded; the next minor GC freed the
    // still-referenced young object (Tomcat's static FastHttpDateFormat
    // ConcurrentLinkedQueue nodes zeroing under worker-pool churn).
    let ct = std::sync::Arc::new(CardTable::new(0x0, 4096));
    let ct2 = std::sync::Arc::clone(&ct);
    std::thread::spawn(move || {
        // One buffered edge, far below the auto-flush threshold; the
        // thread exits immediately after, running DirtyBufferGuard::drop.
        ct2.thread_local_dirty(128);
    })
    .join()
    .unwrap();
    assert_eq!(
        ct.pending_count(),
        0,
        "edge must still be in the (orphaned) thread buffer, not pending"
    );
    // Collector at STW: must recover the dead thread's buffered edge and
    // reap the orphaned buffer.
    ct.flush_all();
    assert_eq!(ct.pending_count(), 1, "dead thread's edge must survive");
    let newly = ct.drain_pending();
    assert_eq!(newly, 1);
    assert!(ct.is_dirty(128 / CARD_SIZE));
    // Second flush_all: the orphan was reaped, nothing left to drain.
    ct.flush_all();
    assert_eq!(ct.pending_count(), 0);
}

// -----------------------------------------------------------------
// SECURITY FIX (V6) — table-id scoping regression tests
// -----------------------------------------------------------------

#[test]
fn distinct_tables_have_distinct_ids() {
    let a = CardTable::new(0x0, 4096);
    let b = CardTable::new(0x0, 4096);
    assert_ne!(a.id, b.id, "each CardTable must get a unique id");
}

#[test]
fn flush_all_does_not_steal_other_tables_entries() {
    // SECURITY FIX (V6): two tables, interleaved dirties on the SAME
    // thread (so both feed the one per-thread buffer). Draining table A
    // must not consume table B's buffered offsets, and vice-versa.
    //
    // Offsets are chosen so each lands on a distinct card and the two
    // tables' dirty card sets differ, proving no theft / no leakage.
    let a = CardTable::new(0x0, 8192);
    let b = CardTable::new(0x0, 8192);

    // Interleave below the auto-flush threshold so everything stays in
    // the shared per-thread buffer until we explicitly flush.
    a.thread_local_dirty(0); // A: card 0
    b.thread_local_dirty(CARD_SIZE); // B: card 1
    a.thread_local_dirty(2 * CARD_SIZE); // A: card 2
    b.thread_local_dirty(3 * CARD_SIZE); // B: card 3

    // Drain A via flush_all: only A's offsets should land in A's pending.
    a.flush_all();
    assert_eq!(a.pending_count(), 2, "A should own exactly its 2 offsets");
    // B's offsets must still be buffered (untouched by A's flush).
    assert_eq!(
        b.pending_count(),
        0,
        "B's buffered offsets must survive A's flush"
    );

    let a_new = a.drain_pending();
    assert_eq!(a_new, 2);
    assert!(a.is_dirty(0));
    assert!(a.is_dirty(2));
    assert!(!a.is_dirty(1), "A must not have B's card 1");
    assert!(!a.is_dirty(3), "A must not have B's card 3");

    // Now drain B: its offsets were preserved through A's flush.
    b.flush_all();
    assert_eq!(b.pending_count(), 2, "B's offsets recovered intact");
    let b_new = b.drain_pending();
    assert_eq!(b_new, 2);
    assert!(b.is_dirty(1));
    assert!(b.is_dirty(3));
    assert!(!b.is_dirty(0), "B must not have A's card 0");
    assert!(!b.is_dirty(2), "B must not have A's card 2");
}

#[test]
fn flush_dirty_buffer_is_table_scoped() {
    // Same property as above, but exercising the per-thread
    // `flush_dirty_buffer` path rather than the registry-wide `flush_all`.
    let a = CardTable::new(0x0, 8192);
    let b = CardTable::new(0x0, 8192);

    a.thread_local_dirty(0);
    b.thread_local_dirty(CARD_SIZE);

    a.flush_dirty_buffer();
    assert_eq!(a.pending_count(), 1);
    assert_eq!(b.pending_count(), 0, "A's flush must not take B's entry");

    b.flush_dirty_buffer();
    assert_eq!(
        b.pending_count(),
        1,
        "B's entry still available after A flushed"
    );

    assert_eq!(a.drain_pending(), 1);
    assert_eq!(b.drain_pending(), 1);
    assert!(a.is_dirty(0));
    assert!(b.is_dirty(1));
}

#[test]
fn cross_thread_v6_property_preserved_per_table() {
    // SECURITY FIX (V6): a *worker* thread buffers an old→young edge for
    // table A below the auto-flush threshold and then parks WITHOUT
    // flushing. A's `flush_all` (run on the main thread) must still recover
    // that edge from the global registry — the V6 cross-thread property —
    // while leaving table B's edge (buffered on the main thread) intact.
    use std::sync::mpsc;

    let a = std::sync::Arc::new(CardTable::new(0x0, 8192));
    let b = CardTable::new(0x0, 8192);

    // Main thread buffers one edge for B (card 1), unflushed.
    b.thread_local_dirty(CARD_SIZE);

    // Worker buffers one edge for A (card 2), then parks until released so
    // its buffer is still registered while we run `flush_all`.
    let (tx_ready, rx_ready) = mpsc::channel::<()>();
    let (tx_go, rx_go) = mpsc::channel::<()>();
    let a_worker = std::sync::Arc::clone(&a);
    let handle = std::thread::spawn(move || {
        a_worker.thread_local_dirty(2 * CARD_SIZE);
        tx_ready.send(()).unwrap();
        rx_go.recv().unwrap(); // park: never flushes its own buffer
    });

    // Wait until the worker has buffered, then drain A across every
    // registered thread buffer.
    rx_ready.recv().unwrap();
    a.flush_all();
    assert_eq!(
        a.pending_count(),
        1,
        "A's collector must recover the worker thread's unflushed edge"
    );
    assert_eq!(a.drain_pending(), 1);
    assert!(a.is_dirty(2), "worker's old→young edge for A is live");

    // A's cross-thread drain must not have disturbed B's buffered edge.
    assert_eq!(b.pending_count(), 0);
    b.flush_all();
    assert_eq!(
        b.pending_count(),
        1,
        "B's edge survived A's cross-thread drain"
    );
    assert_eq!(b.drain_pending(), 1);
    assert!(b.is_dirty(1));

    // Release the worker and join.
    tx_go.send(()).unwrap();
    handle.join().unwrap();
}
