use super::*;

/// zki (page `zgc-zt23hunt4-*` item 4): a store backed by explicit
/// hugetlbfs pages commits whole granules from the pool or refuses with
/// nothing mapped -- never a run that faults (`SIGBUS`) on first touch --
/// and a decommit / reset hands the pages back. Holds whatever the host's
/// pool holds (an empty pool refuses every commit, cleanly).
#[cfg(target_os = "linux")]
#[test]
fn zki_explicit_huge_pages_commit_from_the_pool_or_refuse_cleanly() {
    let mut store = HeapStore::new(8 * GRANULE, "zki-test");
    assert_eq!(
        store.as_ptr() as usize % GRANULE,
        0,
        "the reservation is 2 MiB aligned"
    );
    if !store.use_explicit_huge_pages() {
        return; // no MADV_POPULATE_WRITE on hugetlbfs here
    }
    assert!(store.is_explicit_huge_pages());
    if store.commit_range(0, 2 * GRANULE) {
        assert_eq!(store.committed_bytes(), 2 * GRANULE);
        // SAFETY: committed just above, inside the store.
        unsafe {
            let p = store.as_mut_ptr();
            assert_eq!(*p.add(GRANULE + 17), 0);
            *p.add(GRANULE + 17) = 7;
            assert_eq!(*p.add(GRANULE + 17), 7);
        }
        let (reset, decommitted) = store.reset_range(GRANULE, GRANULE, "zki-test");
        assert_eq!(
            (reset, decommitted),
            (0, GRANULE),
            "a hugetlbfs reset is a decommit"
        );
        assert_eq!(store.committed_bytes(), GRANULE);
        assert_eq!(store.decommit_range(0, GRANULE, "zki-test"), GRANULE);
    } else {
        assert_eq!(store.committed_bytes(), 0, "a refused commit maps nothing");
    }
    // Once anything is committed the backing cannot change.
    let mut plain = HeapStore::new(4 * GRANULE, "zki-test");
    if plain.commit_range(0, GRANULE) {
        assert!(!plain.use_explicit_huge_pages());
    }
}

/// zt t4/x3: a side-table discard is exact or refused. A run that is not
/// whole kernel pages (an unaligned start, or a length the kernel would
/// round UP to a page) touches nothing, so a byte just past it keeps its
/// value; a whole-page run is given back and reads zero (positive
/// control).
#[cfg(target_os = "linux")]
#[test]
fn zt4x3_a_side_table_discard_is_exact_or_refused() {
    let page = os_page_size();
    assert!(page >= 4096 && page.is_power_of_two(), "page {page}");
    let len = 4 * page;
    let Some(p) = map_side_table(len) else {
        return;
    };
    // SAFETY: `[p, p + len)` is the mapping just made; single-threaded.
    // The discards' "all zero" precondition is waived on purpose: the
    // test owns the mapping and checks which bytes were dropped.
    unsafe {
        *p.add(page + 8) = 0x5A;
        *p.add(3 * page) = 0xA5;
        // Not whole pages: refused, nothing touched.
        assert!(!discard_side_table_pages(p.add(8), page));
        assert!(!discard_side_table_pages(p.add(2 * page), page / 2));
        assert_eq!(*p.add(page + 8), 0x5A);
        assert_eq!(*p.add(3 * page), 0xA5);
        // Whole pages: given back, they read zero; the page after keeps
        // its byte.
        assert!(discard_side_table_pages(p.add(page), page));
        assert_eq!(*p.add(page + 8), 0);
        assert_eq!(*p.add(3 * page), 0xA5);
        unmap_side_table(p, len);
    }
}

/// **A reset granule gives its pages back and stays READABLE** (round 9
/// wave 9, `zgc9`; `HeapStore::reset_range`).
///
/// The property the JIT's published read bound needs: after the give-back
/// the granule must still be committed in the bitmap, loadable without a
/// fault, and read as zero -- the same contract a re-committed granule
/// has -- while the partial granules at either edge keep their bytes.
#[test]
fn a_reset_granule_stays_committed_readable_and_reads_as_zero() {
    let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 8 * GRANULE));
    // SAFETY: committed above.
    unsafe {
        *res.base.add(GRANULE + 8) = 0x11; // partial leading granule
        *res.base.add(2 * GRANULE + 99) = 0xCD; // wholly inside the range
        *res.base.add(4 * GRANULE + 7) = 0xEF; // wholly inside the range
        *res.base.add(5 * GRANULE + 64) = 0x22; // partial trailing granule
    }
    // Mid-granule at both ends: only granules 2..5 are wholly inside.
    let (reset, decommitted) = res.reset_range(GRANULE + 16, 4 * GRANULE, "test");
    if reset == 0 && decommitted == 0 {
        return; // platform declined; nothing to assert
    }
    if decommitted != 0 {
        // The Windows arm could not commit the run again (commit limit).
        // Then it must be accounted as a hole, never as committed.
        assert!(!res.is_committed(2) || !res.is_committed(4));
        return;
    }
    assert_eq!(reset, 3 * GRANULE);
    assert_eq!(res.committed_granules(), 8, "a reset must not uncommit");
    for g in 0..8 {
        assert!(res.is_committed(g), "granule {g} must still be committed");
    }
    // SAFETY: every granule is still committed read-write.
    unsafe {
        assert_eq!(
            *res.base.add(2 * GRANULE + 99),
            0,
            "a reset granule reads as zero"
        );
        assert_eq!(
            *res.base.add(4 * GRANULE + 7),
            0,
            "a reset granule reads as zero"
        );
        assert_eq!(
            *res.base.add(GRANULE + 8),
            0x11,
            "the partial leading granule survives"
        );
        assert_eq!(
            *res.base.add(5 * GRANULE + 64),
            0x22,
            "the partial trailing granule survives"
        );
        // And it is writable without a commit.
        *res.base.add(3 * GRANULE + 5) = 0x5A;
        assert_eq!(*res.base.add(3 * GRANULE + 5), 0x5A);
    }
}

/// **A reset skips uncommitted granules** rather than committing them.
#[test]
fn a_reset_never_commits_what_was_not_committed() {
    let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, GRANULE));
    let (reset, decommitted) = res.reset_range(0, 4 * GRANULE, "test");
    assert_eq!(decommitted, 0);
    assert!(
        reset <= GRANULE,
        "only the one committed granule can be reset"
    );
    assert_eq!(res.committed_granules(), 1);
    assert!(!res.is_committed(1) && !res.is_committed(3));
}

/// **A reservation commits nothing until it is asked to.**
///
/// The whole point on Windows, where the `alloc_zeroed` block this replaces
/// takes the full `-Xmx` commit charge before the VM has run a bytecode.
#[test]
fn a_fresh_reservation_has_committed_nothing() {
    let Some(res) = Reservation::reserve(16 * GRANULE) else {
        return; // no reservation on this platform; the fallback covers it
    };
    assert_eq!(res.committed_granules(), 0);
    assert_eq!(res.len, 16 * GRANULE);
    assert_eq!(res.reserved_len, 16 * GRANULE);
}

/// **Committing is idempotent, and only the granules touched are charged.**
#[test]
fn committing_a_range_charges_only_the_granules_it_spans() {
    let Some(mut res) = Reservation::reserve(16 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 1));
    assert_eq!(
        res.committed_granules(),
        1,
        "one byte must commit one granule"
    );
    assert!(res.commit_range(0, 1));
    assert_eq!(
        res.committed_granules(),
        1,
        "committing twice must not double-charge"
    );
    // Spanning a boundary takes both.
    assert!(res.commit_range(GRANULE - 8, 16));
    assert_eq!(res.committed_granules(), 2);
}

/// **Committed memory is writable and reads as zero.**
///
/// `Arena` inherited that contract from `alloc_zeroed` and depends on it:
/// the bump path hands out memory without zeroing it, and only reused
/// free-list blocks are explicitly cleared.
#[test]
fn committed_memory_is_zero_and_writable() {
    let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, GRANULE));
    // SAFETY: the range was just committed.
    unsafe {
        let p = res.base;
        for i in [0usize, 1, 4095, GRANULE - 1] {
            assert_eq!(*p.add(i), 0, "byte {i} of a fresh granule is not zero");
        }
        *p.add(1234) = 0xAB;
        assert_eq!(*p.add(1234), 0xAB);
    }
}

/// **Decommitting rounds INWARD, so a partial granule is never dropped.**
///
/// The bytes beside a partial granule are still in use, and a decommit that
/// rounded outward would take them with it -- silently, because the next
/// read of a dropped page returns zero rather than faulting.
#[test]
fn decommit_releases_only_whole_granules_inside_the_range() {
    let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 8 * GRANULE));
    assert_eq!(res.committed_granules(), 8);
    // A range that starts and ends mid-granule: only granules 2..5 are
    // wholly inside it.
    let released = res.decommit_range(GRANULE + 16, 4 * GRANULE, "test");
    if released == 0 {
        return; // platform declined; nothing to assert about the split
    }
    assert_eq!(released, 3 * GRANULE);
    assert_eq!(res.committed_granules(), 5);
    assert!(
        res.is_committed(1),
        "the partial leading granule must survive"
    );
    assert!(
        res.is_committed(5),
        "the partial trailing granule must survive"
    );
    assert!(!res.is_committed(2));
    assert!(!res.is_committed(4));
}

/// **A decommitted granule comes back zeroed.**
///
/// The contract `decommit_range` states, and the one that makes reuse safe:
/// a re-committed granule must look like a fresh one, not like whatever the
/// dead objects left.
#[test]
fn a_recommitted_granule_reads_as_zero_again() {
    let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 2 * GRANULE));
    // SAFETY: committed above.
    unsafe { *res.base.add(GRANULE + 99) = 0xCD };
    if res.decommit_range(GRANULE, GRANULE, "test") == 0 {
        return; // platform declined
    }
    assert!(res.commit_range(GRANULE, GRANULE));
    // SAFETY: re-committed above.
    assert_eq!(unsafe { *res.base.add(GRANULE + 99) }, 0);
}

/// **Committing past the end of the reservation is refused, not clamped.**
///
/// The caller is about to WRITE there. A clamp would return `true` for a
/// range it did not make writable.
#[test]
fn committing_out_of_bounds_is_refused() {
    let Some(mut res) = Reservation::reserve(2 * GRANULE) else {
        return;
    };
    assert!(!res.commit_range(0, 4 * GRANULE));
    assert!(!res.commit_range(2 * GRANULE, 1));
}

/// **THE RING NAMES THE SPAN, THE SITE, AND WHETHER IT CAME BACK.**
///
/// This is the instrument a heap use-after-free is diagnosed with: the
/// crash handler asks `recent_decommit_covering(fault_addr)` and prints the
/// answer. Three things have to hold for that to be worth printing, and all
/// three are asserted here, because each one silently degrades the report
/// to a wrong answer rather than to no answer:
///
///  * an address inside a released span is FOUND, and one outside it is not
///    -- a ring that reports every address explains every crash as a
///    use-after-free;
///  * the SITE survives the round trip, since it is what distinguishes a
///    missing root from a mis-sized sweep;
///  * a span committed again is FLAGGED rather than forgotten. An erased
///    slot reads as "no record", which is indistinguishable from a wild
///    pointer -- the exact confusion the ring exists to end.
#[test]
fn the_decommit_ring_answers_for_a_released_span_and_flags_its_return() {
    let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 8 * GRANULE));
    let base = res.base as usize;
    let before = decommits_total();
    if res.decommit_range(2 * GRANULE, 2 * GRANULE, "free-list-low") == 0 {
        return; // platform declined the give-back; nothing was recorded
    }
    assert!(decommits_total() > before, "the give-back must be counted");

    let hit = recent_decommit_covering(base + 2 * GRANULE + 4096)
        .expect("an address inside the released span must be found");
    assert_eq!(hit.0, base + 2 * GRANULE);
    assert_eq!(hit.1, 2 * GRANULE);
    assert_eq!(hit.2, "free-list-low", "the site must survive the ring");
    assert_eq!(
        hit.3 & DECOMMIT_RECOMMITTED,
        0,
        "nothing has re-committed this span yet"
    );

    // Still committed, and never released: a hit here would make every
    // report a false positive.
    assert!(
        recent_decommit_covering(base + 7 * GRANULE).is_none(),
        "an address outside every released span must NOT be reported"
    );

    // And the trap: take it back, and the same address must now read as
    // reuse rather than as evidence.
    assert!(res.commit_range(2 * GRANULE, 2 * GRANULE));
    let after = recent_decommit_covering(base + 2 * GRANULE + 4096)
        .expect("the slot must be kept, not erased");
    assert_ne!(
        after.3 & DECOMMIT_RECOMMITTED,
        0,
        "a span committed again must be flagged, so the handler stops \
         calling it a use-after-free"
    );
}

/// **A DROPPED RESERVATION'S SPANS MUST BE FORGOTTEN**, or the ring names
/// memory that now belongs to someone else.
///
/// The ring keeps absolute addresses. A reservation's address space goes
/// back to the OS when it drops, and the next mapping can be handed the
/// same addresses — so an entry that outlived its reservation makes the
/// crash handler report a live, unrelated mapping as "memory the collector
/// gave away". A confident wrong answer, which is worse than the silence
/// the ring exists to end.
///
/// This is not hypothetical and it is not Windows-visible. The Linux
/// workspace run of 2026-09-05 failed
/// `the_decommit_ring_answers_for_a_released_span_and_flags_its_return`
/// with `left: 6291456, right: 4194304`: tests run in parallel, one
/// reservation's freed addresses were re-issued to another's, and the scan
/// answered with the DEAD reservation's 3-granule span for a 2-granule
/// give-back. The same code passed on Windows, whose reuse pattern happened
/// not to collide — a platform difference standing in for a real defect.
#[test]
fn a_dropped_reservations_spans_are_forgotten() {
    let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 4 * GRANULE));
    let base = res.base as usize;
    if res.decommit_range(GRANULE, 2 * GRANULE, "free-list-low") == 0 {
        return; // platform declined; nothing was recorded to forget
    }
    let probe = base + GRANULE + 4096;
    assert!(
        recent_decommit_covering(probe).is_some(),
        "the span must be findable while its reservation is alive"
    );
    drop(res);
    assert!(
        recent_decommit_covering(probe).is_none(),
        "a span whose reservation has been released must not be reported: those addresses can already belong to another mapping"
    );
}

/// **THE NEWEST ENTRY WINS**, because slot order is not time order.
///
/// The ring is circular, so a scan that takes the first covering slot it
/// meets can return a stale entry that a newer, accurate one has already
/// superseded. Two give-backs at the same address, the second narrower:
/// a first-match scan answers with whichever landed in the lower slot.
#[test]
fn the_ring_answers_with_the_most_recent_span_covering_an_address() {
    let Some(mut res) = Reservation::reserve(8 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 8 * GRANULE));
    let base = res.base as usize;
    if res.decommit_range(GRANULE, 4 * GRANULE, "unbumped-middle") == 0 {
        return;
    }
    // Re-commit and give back a NARROWER span inside the first one, so both
    // entries cover the probe and only the second is current.
    assert!(res.commit_range(GRANULE, 4 * GRANULE));
    if res.decommit_range(2 * GRANULE, GRANULE, "free-list-high") == 0 {
        return;
    }
    let probe = base + 2 * GRANULE + 64;
    let hit = recent_decommit_covering(probe).expect("covered by both spans");
    assert_eq!(
        (hit.1, hit.2),
        (GRANULE, "free-list-high"),
        "the ring must answer with the LATEST give-back covering the address, not with whichever slot the scan reached first"
    );
}

/// **The owned fallback behaves as the block it replaces.**
#[test]
fn the_owned_store_is_wholly_committed_and_ignores_decommit() {
    let mut store = HeapStore::Owned(vec![0u8; 4096]);
    assert_eq!(store.len(), 4096);
    assert_eq!(store.committed_bytes(), 4096);
    assert!(store.commit_range(0, 4096));
    assert_eq!(store.decommit_range(0, 4096, "test"), 0);
    assert_eq!(store.committed_bytes(), 4096);
}

/// gci i42/ta: a `with_headroom` store grows in place up to its
/// reservation and REFUSES past it -- base, length and contents unchanged
/// -- instead of re-reserving, copying and releasing the mapping its owner
/// (the old generation) still addresses.
#[test]
fn gci_i42ta_a_headroom_store_never_moves_its_base() {
    let mut store = HeapStore::with_headroom(2 * GRANULE, 4 * GRANULE, "test");
    if !matches!(store, HeapStore::Reserved(_)) {
        // No reservation here: the owned arm, whose in-place capacity is
        // its length, which a caller that needs headroom declines.
        assert_eq!(store.in_place_capacity(), store.len());
        return;
    }
    assert!(store.in_place_capacity() >= 4 * GRANULE);
    let base = store.as_ptr();
    assert!(store.commit_range(0, 8));
    // SAFETY: committed above.
    unsafe { *store.as_mut_ptr() = 0x5A };
    assert_eq!(store.grow_to(3 * GRANULE), base, "inside the headroom: in place");
    assert_eq!(store.len(), 3 * GRANULE);
    let past = store.in_place_capacity() + GRANULE;
    assert_eq!(store.grow_to(past), base, "past the headroom: refused, not moved");
    assert_eq!(store.len(), 3 * GRANULE, "a refused growth leaves the length");
    // SAFETY: the original mapping is still this store's and committed.
    assert_eq!(unsafe { *store.as_ptr() }, 0x5A, "the old mapping was not released");
}

/// gci i42/ta: the refused-headroom fallback -- a plain reserved store of
/// the bare capacity, which `with_headroom` returns when the bigger
/// reservation is refused -- is pinned too, and says it has no headroom.
/// Built through the same `pinned_in_place` step `with_headroom` takes,
/// since a test cannot make the OS refuse one reservation and grant the
/// next.
#[test]
fn gci_i42ta_a_refused_headroom_fallback_is_pinned_and_reports_no_headroom() {
    let mut store = HeapStore::new(2 * GRANULE, "test").pinned_in_place();
    if !matches!(store, HeapStore::Reserved(_)) {
        return;
    }
    assert_eq!(
        store.in_place_capacity(),
        2 * GRANULE,
        "the fallback's in-place capacity is its own reservation, not the headroom asked for"
    );
    let base = store.as_ptr();
    assert!(store.commit_range(0, 8));
    // SAFETY: committed above.
    unsafe { *store.as_mut_ptr() = 0xC3 };
    assert_eq!(store.grow_to(4 * GRANULE), base);
    assert_eq!(store.len(), 2 * GRANULE);
    // SAFETY: still this store's committed first granule.
    assert_eq!(unsafe { *store.as_ptr() }, 0xC3);
}

/// gci i42/ta: a store NOT built by `with_headroom` keeps its moving arm
/// (`Arena::grow` relies on it): growing past the reservation either moves
/// with the bytes or, when the OS refuses the bigger reservation, stays.
#[test]
fn gci_i42ta_a_plain_store_still_grows_past_its_reservation() {
    let mut store = HeapStore::new(GRANULE, "test");
    if !matches!(store, HeapStore::Reserved(_)) {
        return;
    }
    assert!(store.commit_range(0, 8));
    // SAFETY: committed above.
    unsafe { *store.as_mut_ptr() = 0x77 };
    let base = store.as_ptr();
    let after = store.grow_to(4 * GRANULE);
    if after == base {
        // The OS refused the bigger reservation: nothing moved.
        assert_eq!(store.len(), GRANULE);
    } else {
        assert_eq!(store.len(), 4 * GRANULE);
        // SAFETY: the copy committed the destination's first granule.
        assert_eq!(unsafe { *store.as_ptr() }, 0x77, "the bytes moved with it");
    }
}

/// gci i42/ta: a give-back the OS refused leaves its ring entry flagged
/// re-taken, so a later fault inside the still-mapped span is not reported
/// as a heap use-after-free naming the release site.
#[test]
fn gci_i42ta_a_refused_decommit_is_flagged_retaken_in_the_ring() {
    let Some(mut res) = Reservation::reserve(4 * GRANULE) else {
        return;
    };
    assert!(res.commit_range(0, 4 * GRANULE));
    let addr = res.base as usize + GRANULE;
    // What `decommit_granules` does before the syscall...
    record_decommit(addr, GRANULE, "gci-i42ta-refused");
    let mut cleared = Vec::new();
    if res.is_committed(1) {
        res.set_committed(1, false);
        res.committed -= 1;
        cleared.push(1);
    }
    // ...and what it now does when the syscall is refused.
    res.undo_refused_decommit(1, GRANULE, cleared);
    assert!(res.is_committed(1), "the cleared bit is restored");
    assert_eq!(res.committed_granules(), 4);
    let hit = recent_decommit_covering(addr + 64).expect("the entry is kept");
    assert_eq!(hit.2, "gci-i42ta-refused");
    assert_ne!(
        hit.3 & DECOMMIT_RECOMMITTED,
        0,
        "a refused give-back must not read as memory that went back to the OS"
    );
}
