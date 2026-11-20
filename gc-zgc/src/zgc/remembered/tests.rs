use super::*;

const YOUNG_BASE: u64 = 0x1000_0000;
const YOUNG_SIZE: u64 = 1 << 20; // 1 MiB
const OLD_BASE: u64 = 0x2000_0000;
const OLD_PAGE: usize = 1 << 16; // 64 KiB pages, 4 of them
const OLD_SIZE: u64 = (OLD_PAGE as u64) * 4;

fn ctx() -> ZRangeGenerationContext {
    ZRangeGenerationContext::new(YOUNG_BASE, YOUNG_SIZE, OLD_BASE, OLD_SIZE, OLD_PAGE)
}

fn barrier_with_all_pages_registered() -> (ZStoreBarrier, ZRangeGenerationContext) {
    let table = Arc::new(ZRememberedSetTable::new());
    let context = ctx();
    // The context registers its own pages: its ids are ContextLocal, and a
    // hand-rolled `register_old_page` loop here would declare them to be
    // allocator ids. See ZPageIdNamespace.
    assert_eq!(context.register_pages(&table), 4);
    (ZStoreBarrier::new(table), context)
}

// -----------------------------------------------------------------
// ZRememberedSet — set / query
// -----------------------------------------------------------------

/// The grain must be exactly one reference slot, and a word must cover
/// exactly one card's worth of page. Both are load-bearing claims in the
/// module header's cost analysis, so they are asserted rather than
/// asserted-in-prose.
#[test]
fn granularity_matches_the_reference_slot_and_the_card_span() {
    assert_eq!(Z_REMSET_GRAIN_BYTES, 8);
    assert_eq!(Z_REMSET_GRAIN_BYTES, cratonvm_types::REF_FIELD_SIZE);
    assert_eq!(Z_REMSET_BYTES_PER_WORD, 512);
    assert_eq!(Z_REMSET_BYTES_PER_WORD, cratonvm_gc_core::card_table::CARD_SIZE);
}

/// The 16x figure from the module header, computed rather than asserted in
/// prose: two bitmaps at one bit per 8 bytes vs. one byte per 512 bytes.
#[test]
fn memory_overhead_is_sixteen_times_the_card_table() {
    let page_size = 2 * 1024 * 1024; // ZGC small page
    let rs = ZRememberedSet::new(0, page_size);
    let bitmaps = 2 * rs.word_count() * std::mem::size_of::<AtomicU64>();
    assert_eq!(bitmaps, page_size / 32, "page_size/32 = 3.125%");
    assert_eq!(bitmaps, 64 * 1024, "64 KiB per 2 MiB page");

    let card_bytes = page_size / cratonvm_gc_core::card_table::CARD_SIZE;
    assert_eq!(card_bytes, 4 * 1024, "4 KiB per 2 MiB page");
    assert_eq!(bitmaps / card_bytes, 16);
}

#[test]
fn set_and_query_round_trip_across_word_boundaries() {
    let page_size = 4096;
    let rs = ZRememberedSet::new(3, page_size);
    assert_eq!(rs.page_id(), 3);
    assert_eq!(rs.page_size(), page_size);
    // 4096 / 8 = 512 grains = 8 words.
    assert_eq!(rs.word_count(), 8);

    // Straddle the word-0/word-1 boundary: bit 63 is offset 504, bit 64 is
    // offset 512 (the first bit of word 1), bit 65 is 520. Every entry is
    // a DISTINCT grain (0, 1, 63, 64, 65, 256, 511) so the dedup below
    // cannot mask a collision.
    let offsets = [0usize, 8, 504, 512, 520, 2048, page_size - 8];
    for &off in &offsets {
        assert!(!rs.is_remembered(off), "clean before remember: {off}");
    }
    for &off in &offsets {
        assert!(rs.remember(off), "in-range remember must succeed: {off}");
    }
    for &off in &offsets {
        assert!(rs.is_remembered(off), "remembered after remember: {off}");
    }
    // Neighbouring grains must NOT have been set — one bit is one slot.
    assert!(!rs.is_remembered(496));
    assert!(!rs.is_remembered(528));

    let mut seen: Vec<usize> = Vec::new();
    rs.iterate(|off| seen.push(off));
    let mut expected: Vec<usize> = offsets.to_vec();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(seen, expected, "iterate yields ascending, deduplicated");
    assert_eq!(rs.bits_set(), expected.len());
}

/// zr z1/gn: the per-buffer summary agrees with a full walk through a
/// remember / swap / retain / clear round trip on a 2 MiB page, and every
/// clear leaves the summary empty.
#[test]
fn the_word_summary_agrees_with_the_full_walk_across_a_swap() {
    let page = 2 * 1024 * 1024;
    let rs = ZRememberedSet::new(0, page);
    let offsets = [0usize, 8, 512, 4096 * 8, page - 8, 1_000_000 & !7];
    for off in offsets {
        assert!(rs.remember(off));
    }
    let mut expected: Vec<usize> = offsets.to_vec();
    expected.sort_unstable();
    expected.dedup();
    let mut seen = Vec::new();
    rs.iterate(|o| seen.push(o));
    assert_eq!(seen, expected);
    assert_eq!(rs.bits_set(), expected.len());
    let before = rs.stats();
    rs.swap();
    assert_eq!(rs.bits_set(), 0, "fresh current buffer");
    assert_eq!(rs.bits_set_in_snapshot(), expected.len());
    // Retain every other offset into the new current buffer.
    let mut i = 0;
    rs.iterate_snapshot_retaining(|_| {
        i += 1;
        i % 2 == 1
    });
    let after = rs.stats();
    assert_eq!(
        after.words_scanned - before.words_scanned,
        rs.word_count() as u64,
        "a scan still accounts every word of the page"
    );
    rs.clear_snapshot();
    assert_eq!(rs.bits_set_in_snapshot(), 0);
    let mut kept = Vec::new();
    rs.iterate(|o| kept.push(o));
    let want: Vec<usize> = expected.iter().copied().step_by(2).collect();
    assert_eq!(kept, want);
    rs.swap();
    rs.swap();
    assert_eq!(rs.bits_set(), 0);
    assert_eq!(rs.bits_set_in_snapshot(), 0);
    for b in 0..2 {
        assert!(
            rs.summaries[b]
                .iter()
                .all(|w| w.load(Ordering::Relaxed) == 0),
            "every clear empties the summary"
        );
    }
}

#[test]
fn offsets_within_one_grain_collapse_to_one_bit() {
    let rs = ZRememberedSet::new(0, 4096);
    // 8-byte grain: 16, 17 ... 23 are all the same slot.
    assert!(rs.remember(16));
    assert!(rs.remember(19));
    assert!(rs.remember(23));
    assert_eq!(rs.bits_set(), 1);
    let mut seen: Vec<usize> = Vec::new();
    rs.iterate(|off| seen.push(off));
    assert_eq!(seen, vec![16], "iterate reports the grain-aligned offset");
}

#[test]
fn out_of_range_offset_is_rejected_not_silently_recorded() {
    let rs = ZRememberedSet::new(0, 4096);
    assert!(
        !rs.remember(4096),
        "exactly at the page end is out of range"
    );
    assert!(!rs.remember(999_999));
    assert_eq!(rs.bits_set(), 0);
    assert!(!rs.is_remembered(4096));
}

#[test]
fn a_page_smaller_than_one_word_still_gets_a_word() {
    let rs = ZRememberedSet::new(0, 24);
    assert_eq!(rs.word_count(), 1);
    assert!(rs.remember(16));
    assert!(!rs.remember(24));
    assert_eq!(rs.bits_set(), 1);
}

// -----------------------------------------------------------------
// ZRememberedSet — sparse iteration
// -----------------------------------------------------------------

/// The zero-word skip is the whole reason this structure can be scanned
/// every young collection. Assert the SKIP COUNTER, not just the yielded
/// offsets: a bit-by-bit loop would produce the same offsets while doing
/// 64x the work, and only the counter can tell the two apart.
#[test]
fn sparse_iteration_skips_zero_words() {
    let page_size = 64 * 1024; // 8192 grains = 128 words
    let rs = ZRememberedSet::new(1, page_size);
    assert_eq!(rs.word_count(), 128);

    // One bit in the first word, one in the last: 126 words are zero.
    rs.remember(0);
    rs.remember(page_size - Z_REMSET_GRAIN_BYTES);

    let mut seen: Vec<usize> = Vec::new();
    rs.iterate(|off| seen.push(off));
    assert_eq!(seen, vec![0, page_size - Z_REMSET_GRAIN_BYTES]);

    let stats = rs.stats();
    assert_eq!(stats.words_scanned, 128, "one load per 512 bytes of page");
    assert_eq!(stats.words_skipped, 126, "126 zero words skipped whole");
    assert_eq!(stats.bits_set, 2);
    assert_eq!(stats.bits_newly_set, 2);
    assert_eq!(stats.grain_bytes, Z_REMSET_GRAIN_BYTES);

    // A second scan accumulates: these are lifetime counters.
    rs.iterate(|_| {});
    assert_eq!(rs.stats().words_scanned, 256);
    assert_eq!(rs.stats().words_skipped, 252);
}

#[test]
fn a_dense_word_is_walked_once_per_set_bit_not_once_per_bit() {
    let rs = ZRememberedSet::new(0, 4096);
    // Fill word 0 completely: 64 bits, offsets 0..512 step 8.
    for i in 0..64usize {
        rs.remember(i * Z_REMSET_GRAIN_BYTES);
    }
    let mut count = 0usize;
    rs.iterate(|_| count += 1);
    assert_eq!(count, 64);
    let stats = rs.stats();
    assert_eq!(stats.words_scanned, 8);
    assert_eq!(stats.words_skipped, 7, "only word 0 is non-zero");
}

// -----------------------------------------------------------------
// ZRememberedSet — concurrency
// -----------------------------------------------------------------

/// Every bit set by any thread must be observed. The interleaving
/// (`grain = i * THREADS + t`) deliberately puts adjacent grains from
/// DIFFERENT threads in the SAME `u64` word — that is the case a
/// non-atomic read-modify-write silently loses bits on, and a lost bit
/// here is a lost old->young edge, i.e. a freed live object.
#[test]
fn concurrent_remember_from_many_threads_loses_no_bit() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 512;
    let total = THREADS * PER_THREAD;
    let page_size = total * Z_REMSET_GRAIN_BYTES;

    let rs = Arc::new(ZRememberedSet::new(9, page_size));
    let start = Arc::new(std::sync::Barrier::new(THREADS));

    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let rs = Arc::clone(&rs);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            // Maximise the overlap: everyone starts hammering at once.
            start.wait();
            for i in 0..PER_THREAD {
                let grain = i * THREADS + t;
                assert!(rs.remember(grain * Z_REMSET_GRAIN_BYTES));
            }
        }));
    }
    for h in handles {
        h.join().expect("mutator thread panicked");
    }

    assert_eq!(rs.bits_set(), total, "no bit may be lost to a race");
    assert_eq!(rs.stats().bits_newly_set, total as u64);
    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            let grain = i * THREADS + t;
            assert!(
                rs.is_remembered(grain * Z_REMSET_GRAIN_BYTES),
                "thread {t} bit {i} (grain {grain}) was lost",
            );
        }
    }
    let mut seen = 0usize;
    rs.iterate(|_| seen += 1);
    assert_eq!(seen, total);
}

/// Two threads hammering the SAME bit is the idempotence half of the
/// `fetch_or`-needs-no-CAS-loop argument: the bit ends up set exactly
/// once, whichever order they land in.
#[test]
fn concurrent_remember_of_the_same_bit_is_idempotent() {
    const THREADS: usize = 8;
    let rs = Arc::new(ZRememberedSet::new(0, 4096));
    let start = Arc::new(std::sync::Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let rs = Arc::clone(&rs);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            for _ in 0..1000 {
                rs.remember(64);
            }
        }));
    }
    for h in handles {
        h.join().expect("mutator thread panicked");
    }
    assert_eq!(rs.bits_set(), 1);
    assert_eq!(
        rs.stats().bits_newly_set,
        1,
        "exactly one clean->set transition across 8000 stores",
    );
}

// -----------------------------------------------------------------
// ZRememberedSet — double buffering
// -----------------------------------------------------------------

#[test]
fn swap_isolates_the_collector_snapshot_from_concurrent_dirtying() {
    let rs = ZRememberedSet::new(2, 4096);
    rs.remember(0);
    rs.remember(64);

    rs.swap();

    // Everything dirtied before the swap is in the collector's snapshot...
    assert_eq!(rs.snapshot_offsets(), vec![0, 64]);
    assert!(rs.is_remembered_in_snapshot(0));
    assert!(rs.is_remembered_in_snapshot(64));
    // ...and the buffer mutators now dirty starts empty.
    assert_eq!(rs.bits_set(), 0);
    assert!(!rs.is_remembered(0));

    // A store made while the collector scans must NOT contaminate the
    // snapshot — that is the entire point of double buffering.
    rs.remember(128);
    assert_eq!(rs.bits_set(), 1);
    assert!(rs.is_remembered(128));
    assert!(!rs.is_remembered_in_snapshot(128));
    assert_eq!(rs.snapshot_offsets(), vec![0, 64]);

    // Next cycle: roles flip again, and the consumed snapshot is wiped
    // rather than accumulating forever.
    rs.swap();
    assert_eq!(rs.snapshot_offsets(), vec![128]);
    assert_eq!(rs.bits_set(), 0);

    // Third swap: nothing was dirtied, so the snapshot is empty. If the
    // wipe were missing, {0, 64} would reappear here.
    rs.swap();
    assert!(rs.snapshot_offsets().is_empty());
    assert_eq!(rs.bits_set(), 0);
}

#[test]
fn clear_snapshot_releases_the_scanned_buffer_only() {
    let rs = ZRememberedSet::new(0, 4096);
    rs.remember(0);
    rs.swap();
    rs.remember(256);
    assert_eq!(rs.snapshot_offsets(), vec![0]);
    assert_eq!(rs.bits_set(), 1);

    rs.clear_snapshot();
    assert!(rs.snapshot_offsets().is_empty());
    assert_eq!(rs.bits_set(), 1, "the mutators' buffer is untouched");
    assert!(rs.is_remembered(256));
}

#[test]
fn clear_resets_both_buffers() {
    let rs = ZRememberedSet::new(0, 4096);
    rs.remember(0);
    rs.swap();
    rs.remember(256);
    assert_eq!(rs.bits_set(), 1);
    assert_eq!(rs.bits_set_in_snapshot(), 1);

    rs.clear();
    assert_eq!(rs.bits_set(), 0);
    assert_eq!(rs.bits_set_in_snapshot(), 0);
    assert!(rs.snapshot_offsets().is_empty());
    // Re-dirtying after a clear works normally.
    rs.remember(512);
    assert!(rs.is_remembered(512));
    assert_eq!(rs.bits_set(), 1);
}

// -----------------------------------------------------------------
// ZRememberedSetTable
// -----------------------------------------------------------------

#[test]
fn table_registers_looks_up_and_removes_pages() {
    let table = ZRememberedSetTable::new();
    assert!(table.is_empty());
    assert!(table.get(0).is_none());

    let a = table.register_old_page(0, OLD_PAGE);
    let b = table.register_old_page(1, OLD_PAGE);
    assert_eq!(table.len(), 2);
    assert_eq!(a.page_id(), 0);
    assert_eq!(b.page_id(), 1);

    // Idempotent: the same page id returns the SAME set, not a fresh one.
    a.remember(64);
    let a_again = table.register_old_page(0, OLD_PAGE);
    assert!(a_again.is_remembered(64));
    assert!(Arc::ptr_eq(&a, &a_again));
    assert_eq!(table.len(), 2);

    // `get` hands back a cloned Arc that stays valid after the guard is
    // gone (the whole point of the lock-discipline note on the type).
    let fetched = table.get(0).expect("page 0 is registered");
    assert!(fetched.is_remembered(64));

    let mut ids = table.page_ids();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1]);

    let removed = table.remove(0).expect("page 0 was registered");
    assert_eq!(removed.page_id(), 0);
    assert!(table.get(0).is_none());
    assert_eq!(table.len(), 1);
    assert!(table.remove(0).is_none(), "removing twice is a no-op");
}

#[test]
fn table_remember_coarsens_an_unregistered_page_rather_than_dropping_the_edge() {
    let table = ZRememberedSetTable::new();
    table.register_old_page(0, OLD_PAGE);

    assert_eq!(table.remember(0, 64), ZRememberOutcome::Precise);
    // Page 7 was never registered: the edge must survive as a coarsening.
    assert_eq!(table.remember(7, 64), ZRememberOutcome::Coarsened);
    // An in-range page with an out-of-range offset coarsens too.
    assert_eq!(table.remember(0, OLD_PAGE), ZRememberOutcome::Coarsened);

    let mut coarse = table.take_coarse_pages();
    coarse.sort_unstable();
    assert_eq!(coarse, vec![0, 7]);
    assert!(
        table.take_coarse_pages().is_empty(),
        "taking the coarse set clears it",
    );

    // Registering a coarsened page gives it precise storage again and
    // retires the coarsening.
    table.remember(7, 64);
    table.register_old_page(7, OLD_PAGE);
    assert!(table.take_coarse_pages().is_empty());
    assert_eq!(table.remember(7, 64), ZRememberOutcome::Precise);
}

/// FINDING A (2026-08-07). Two page ids that are congruent modulo `2^32`
/// must be two different pages.
///
/// While the key was a `u32` this could not even be *expressed*: the caller
/// narrowed, `1 << 32` became `0`, and the remembered set for the young
/// collection's roots was read out of a completely unrelated page. Nothing
/// errored, nothing was counted, and the first symptom was a use-after-free
/// on an object the collector had every right to think was garbage.
///
/// `ZPageAllocator`'s `next_page_id` starts at 1, is bumped once per page
/// allocation and is never recycled downward (`page.rs:973`, `:1156`), so
/// this is not a hypothetical id — it is the id a long-running VM reaches
/// on its 4-billionth page.
#[test]
fn page_ids_congruent_modulo_two_to_the_thirty_two_are_different_pages() {
    const LOW: u64 = 7;
    const HIGH: u64 = (1u64 << 32) + 7; // truncates to LOW in a u32 key

    let table = ZRememberedSetTable::new();
    let low = table.register_old_page(LOW, OLD_PAGE);
    let high = table.register_old_page(HIGH, OLD_PAGE);

    assert_eq!(table.len(), 2, "the two ids must not collapse into one key");
    assert!(
        !Arc::ptr_eq(&low, &high),
        "the high id aliased onto the low one's bitmap",
    );
    assert_eq!(low.page_id(), LOW);
    assert_eq!(high.page_id(), HIGH);

    // Dirty a slot in each, at different offsets, and prove neither is
    // visible through the other.
    assert_eq!(table.remember(LOW, 64), ZRememberOutcome::Precise);
    assert_eq!(table.remember(HIGH, 128), ZRememberOutcome::Precise);
    assert!(low.is_remembered(64) && !low.is_remembered(128));
    assert!(high.is_remembered(128) && !high.is_remembered(64));
    assert_eq!(table.total_bits_set(), 2);

    // ... and removing one leaves the other alone.
    let removed = table.remove(HIGH).expect("the high page was registered");
    assert_eq!(removed.page_id(), HIGH);
    assert!(table.get(LOW).is_some(), "removing 2^32+7 dropped 7");
    assert_eq!(table.len(), 1);

    let mut ids = table.page_ids();
    ids.sort_unstable();
    assert_eq!(ids, vec![LOW]);
}

/// FINDING A, at the context seam: a page index past `u32::MAX` must be
/// *paged*, not refused.
///
/// The old implementation returned `None` above `u32::MAX`, which made the
/// table coarsen — safe, but it turned every store into a whole-page scan
/// from that point on, i.e. it re-created exactly the full-heap cost this
/// module exists to escape. With a `u64` id there is nothing to refuse.
#[test]
fn range_context_pages_an_index_that_does_not_fit_a_u32() {
    // 8-byte pages so a modest address reaches a huge page index.
    let c = ZRangeGenerationContext::new(0, 0, 0, 1u64 << 36, 8);
    let past_u32 = (u64::from(u32::MAX) + 1) * 8; // page index 2^32 exactly

    assert_eq!(
        c.page_of(past_u32),
        Some((1u64 << 32, 0)),
        "a page index past u32::MAX must be reported, not refused as \
         unknown and not truncated to 0",
    );
    assert_eq!(c.page_of(past_u32 + 4), Some((1u64 << 32, 4)));
    assert_eq!(c.page_base(1u64 << 32), past_u32);

    // The neighbouring index and its 32-bit truncation are distinct pages.
    assert_eq!(c.page_of(0), Some((0u64, 0)));
    assert_ne!(c.page_of(0), c.page_of(past_u32));

    // End to end through the table: the high page is remembered PRECISELY.
    // Registered in the context's own namespace — `c` has 2^33 pages, so
    // `register_pages` is exactly the call NOT to make here (see its cost
    // note); this registers the single page under test by hand, in the right
    // namespace.
    let table = ZRememberedSetTable::new();
    let (page_id, offset) = c.page_of(past_u32).expect("in range");
    table.register_old_page_in(ZPageIdNamespace::ContextLocal, page_id, 8);
    assert_eq!(table.remember(page_id, offset), ZRememberOutcome::Precise);
    assert!(
        table.take_coarse_pages().is_empty(),
        "a high page id must not coarsen",
    );
}

#[test]
fn table_swap_all_flips_every_page() {
    let table = ZRememberedSetTable::new();
    for page_id in 0..4u64 {
        table.register_old_page(page_id, OLD_PAGE);
        table.remember(page_id, 64 * (page_id as usize + 1));
    }
    assert_eq!(table.total_bits_set(), 4);
    assert_eq!(table.total_bits_set_in_snapshot(), 0);

    table.swap_all();

    assert_eq!(table.total_bits_set(), 0, "mutators resume into clean maps");
    assert_eq!(table.total_bits_set_in_snapshot(), 4);
    for page_id in 0..4u64 {
        let set = table.get(page_id).expect("registered");
        assert_eq!(
            set.snapshot_offsets(),
            vec![64 * (page_id as usize + 1)],
            "page {page_id}",
        );
    }

    table.clear_all();
    assert_eq!(table.total_bits_set(), 0);
    assert_eq!(table.total_bits_set_in_snapshot(), 0);
}

#[test]
fn table_retained_bytes_is_two_bitmaps_per_page() {
    let table = ZRememberedSetTable::new();
    assert_eq!(table.total_retained_bytes(), 0);
    table.register_old_page(0, OLD_PAGE);
    table.register_old_page(1, OLD_PAGE);
    let one = ZRememberedSet::new(0, OLD_PAGE).retained_bytes();
    assert_eq!(table.total_retained_bytes(), 2 * one);
    // The bitmaps dominate the struct: 64 KiB page => 2 KiB of bitmaps.
    assert!(one >= 2 * (OLD_PAGE / 64));
}

#[test]
fn table_is_usable_concurrently() {
    const THREADS: usize = 8;
    let table = Arc::new(ZRememberedSetTable::new());
    for page_id in 0..4u64 {
        table.register_old_page(page_id, OLD_PAGE);
    }
    let start = Arc::new(std::sync::Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let table = Arc::clone(&table);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            for i in 0..256usize {
                let page_id = ((i + t) % 4) as u64;
                let offset = (i * THREADS + t) * Z_REMSET_GRAIN_BYTES;
                assert_eq!(table.remember(page_id, offset), ZRememberOutcome::Precise,);
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
    assert_eq!(table.total_bits_set(), THREADS * 256);
    assert!(table.take_coarse_pages().is_empty());
}

// -----------------------------------------------------------------
// ZRangeGenerationContext
// -----------------------------------------------------------------

#[test]
fn range_context_classifies_and_pages_addresses() {
    let c = ctx();
    assert!(c.is_young(YOUNG_BASE));
    assert!(c.is_young(YOUNG_BASE + YOUNG_SIZE - 1));
    assert!(!c.is_young(YOUNG_BASE + YOUNG_SIZE));
    assert!(!c.is_young(OLD_BASE));

    assert!(c.is_old(OLD_BASE));
    assert!(c.is_old(OLD_BASE + OLD_SIZE - 1));
    assert!(!c.is_old(OLD_BASE + OLD_SIZE));
    assert!(!c.is_old(YOUNG_BASE));

    assert_eq!(c.old_page_count(), 4);
    assert_eq!(c.page_of(OLD_BASE), Some((0, 0)));
    assert_eq!(c.page_of(OLD_BASE + 64), Some((0, 64)));
    assert_eq!(c.page_of(OLD_BASE + OLD_PAGE as u64 + 8), Some((1, 8)),);
    assert_eq!(c.page_base(2), OLD_BASE + 2 * OLD_PAGE as u64);
    assert_eq!(c.page_of(YOUNG_BASE), None, "young pages have no rset");
    assert_eq!(c.page_of(OLD_BASE + OLD_SIZE), None);
}

// -----------------------------------------------------------------
// FINDING N7 (2026-08-07) — page-id namespaces
// -----------------------------------------------------------------

/// A `ZGenerationContext` that answers like every real-heap implementation
/// in the tree: ids straight out of `ZPageAllocator`, so it takes the
/// trait's default `page_id_namespace`. Stands in for
/// `adapters::ZHeapGenerationContext`, which this module cannot import.
struct AllocatorIdContext {
    /// `page_id -> base`, in allocation order, ids from 1 as the real
    /// allocator hands them out (`page.rs`: `next_page_id` starts at 1).
    pages: Vec<(u64, u64)>,
}

impl AllocatorIdContext {
    /// Four pages covering the same addresses as [`ctx`], but numbered the
    /// way the allocator numbers them: 1, 2, 3, 4.
    fn over_the_same_old_range() -> Self {
        AllocatorIdContext {
            pages: (0..4u64)
                .map(|i| (i + 1, OLD_BASE + i * OLD_PAGE as u64))
                .collect(),
        }
    }

    fn register_all(&self, table: &ZRememberedSetTable) {
        for &(id, _) in &self.pages {
            table.register_old_page(id, OLD_PAGE);
        }
    }
}

impl ZGenerationContext for AllocatorIdContext {
    fn is_old(&self, addr: u64) -> bool {
        addr >= OLD_BASE && addr < OLD_BASE + OLD_SIZE
    }
    fn is_young(&self, addr: u64) -> bool {
        addr >= YOUNG_BASE && addr < YOUNG_BASE + YOUNG_SIZE
    }
    fn page_of(&self, addr: u64) -> Option<(u64, usize)> {
        self.pages
            .iter()
            .find(|&&(_, base)| addr >= base && addr < base + OLD_PAGE as u64)
            .map(|&(id, base)| (id, (addr - base) as usize))
    }
    // `page_id_namespace` deliberately NOT overridden: the default must be
    // Allocator, so that every existing real-heap impl in the tree is
    // correct without being edited.
}

/// PINS the N7 verdict: the two numberings are different, they are declared
/// to be different, and the declaration is machine-readable.
///
/// `ZRangeGenerationContext` derives a dense index from 0 by address
/// arithmetic; `ZPageReal::id` counts allocations from 1. Over the *same*
/// four pages the two disagree on every one of them — and disagree by
/// producing other *valid* ids, which is what makes a mismatch alias
/// silently instead of coarsening loudly.
#[test]
fn the_range_contexts_ids_are_its_own_and_it_says_so() {
    let range = ctx();
    let allocator = AllocatorIdContext::over_the_same_old_range();

    assert_eq!(range.page_id_namespace(), ZPageIdNamespace::ContextLocal);
    assert_eq!(
        allocator.page_id_namespace(),
        ZPageIdNamespace::Allocator,
        "an impl that returns ZPageReal::id must get the DEFAULT — that is \
         why page_id_namespace is a provided method",
    );

    for i in 0..4u64 {
        let addr = OLD_BASE + i * OLD_PAGE as u64 + 64;
        let (range_id, range_off) = range.page_of(addr).expect("in the old range");
        let (alloc_id, alloc_off) = allocator.page_of(addr).expect("in the old range");
        assert_eq!(
            range_off, alloc_off,
            "the OFFSET half agrees; only the id differs"
        );
        assert_ne!(
            range_id, alloc_id,
            "page {i}: dense index {range_id} vs allocator id {alloc_id}",
        );
        // And the disagreement is an alias, not a miss: the id the range
        // context produces for page `i` is a page the allocator DID hand
        // out (for i >= 1), so a table keyed by allocator ids will happily
        // resolve it — to the wrong page.
        if i >= 1 {
            assert!(
                allocator.pages.iter().any(|&(id, _)| id == range_id),
                "index {range_id} collides with a real allocator id",
            );
        }
    }
}

/// PINS the check itself, in BOTH build profiles.
///
/// `on_reference_store` only `debug_assert!`s the pairing, which pins
/// nothing in a release test run. `context_namespace_matches` is the same
/// predicate as a plain function, so this test holds either way.
#[test]
fn the_barrier_reports_a_table_and_context_that_disagree() {
    // Empty table: compatible with anything, because it has no key yet.
    let empty = Arc::new(ZRememberedSetTable::new());
    assert_eq!(empty.page_id_namespace(), None);
    let barrier = ZStoreBarrier::new(Arc::clone(&empty));
    assert!(barrier.context_namespace_matches(&ctx()));
    assert!(barrier.context_namespace_matches(&AllocatorIdContext::over_the_same_old_range()));

    // Table registered by the allocator, driven by the range context: the
    // N7 pairing.
    let allocator_keyed = Arc::new(ZRememberedSetTable::new());
    AllocatorIdContext::over_the_same_old_range().register_all(&allocator_keyed);
    assert_eq!(
        allocator_keyed.page_id_namespace(),
        Some(ZPageIdNamespace::Allocator),
    );
    let barrier = ZStoreBarrier::new(allocator_keyed);
    assert!(
        !barrier.context_namespace_matches(&ctx()),
        "an allocator-keyed table driven by a context-local context is the \
         lost-edge configuration and must be reported",
    );
    assert!(barrier.context_namespace_matches(&AllocatorIdContext::over_the_same_old_range()));

    // ... and the correctly paired table.
    let range_keyed = Arc::new(ZRememberedSetTable::new());
    let range = ctx();
    range.register_pages(&range_keyed);
    assert_eq!(
        range_keyed.page_id_namespace(),
        Some(ZPageIdNamespace::ContextLocal),
    );
    let barrier = ZStoreBarrier::new(range_keyed);
    assert!(barrier.context_namespace_matches(&range));
    assert!(
        !barrier.context_namespace_matches(&AllocatorIdContext::over_the_same_old_range()),
        "the mismatch is symmetric",
    );
}

/// PINS what the mismatch actually costs, so nobody has to take the prose on
/// trust: the edge is not dropped and not coarsened — it lands on **another
/// live page's bitmap**.
///
/// Driven through `table.remember` rather than `on_reference_store`, because
/// the barrier's `debug_assert!` would (correctly) refuse to let this
/// configuration run in a debug build. That assertion is the fix; this test
/// is what it is protecting against.
#[test]
fn a_mismatched_namespace_aliases_onto_a_live_page_rather_than_coarsening() {
    let allocator = AllocatorIdContext::over_the_same_old_range();
    let table = ZRememberedSetTable::new();
    allocator.register_all(&table);

    // The mutator stores into a field on the SECOND old page. The range
    // context names it page 1; the allocator names it page 2.
    let field = OLD_BASE + OLD_PAGE as u64 + 512;
    let (wrong_id, offset) = ctx().page_of(field).expect("in the old range");
    let (right_id, right_offset) = allocator.page_of(field).expect("in the old range");
    assert_eq!((wrong_id, offset), (1, 512));
    assert_eq!((right_id, right_offset), (2, 512));

    assert_eq!(
        table.remember(wrong_id, offset),
        ZRememberOutcome::Precise,
        "the wrong id is a REGISTERED page, so the edge is recorded \
         precisely — into the wrong bitmap. No coarsening, no warning.",
    );
    assert!(
        table.take_coarse_pages().is_empty(),
        "nothing was coarsened: this is why the defect is silent",
    );

    let victim = table.get(wrong_id).expect("allocator page 1 is registered");
    let real_page = table.get(right_id).expect("allocator page 2 is registered");
    assert!(
        victim.is_remembered(512),
        "page 1 got a bit it never earned"
    );
    assert!(
        !real_page.is_remembered(512),
        "and page 2 — the page that actually holds the old->young edge — \
         has NOTHING. Its young referent is freed while live.",
    );
}

/// PINS the pairing constructor: `register_pages` registers exactly the ids
/// `page_of` will later produce, in the namespace it declares.
#[test]
fn register_pages_registers_exactly_the_ids_the_context_produces() {
    let table = ZRememberedSetTable::new();
    let c = ctx();
    assert_eq!(c.register_pages(&table), 4);
    assert_eq!(table.len(), 4);

    let mut ids = table.page_ids();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1, 2, 3]);
    assert_eq!(
        table.page_id_namespace(),
        Some(ZPageIdNamespace::ContextLocal),
    );

    // Every address in the old range resolves to a registered set.
    for i in 0..4u64 {
        let (id, off) = c
            .page_of(OLD_BASE + i * OLD_PAGE as u64 + 8)
            .expect("in the old range");
        assert_eq!(table.remember(id, off), ZRememberOutcome::Precise);
    }
    assert!(table.take_coarse_pages().is_empty());

    // Idempotent, and it does not re-latch.
    assert_eq!(c.register_pages(&table), 4);
    assert_eq!(table.len(), 4);
    assert_eq!(table.total_bits_set(), 4, "re-registering kept the bitmaps");
}

/// The latch is set by the first registration and read back verbatim.
///
/// Deliberately does NOT assert the conflict *panic*: that is a
/// `debug_assert!`, so a `#[should_panic]` here would pass in debug and fail
/// in release. The observable half — what the table says it is keyed by —
/// holds in both.
#[test]
fn the_table_latches_the_namespace_of_its_first_registration() {
    let fresh = ZRememberedSetTable::new();
    assert_eq!(fresh.page_id_namespace(), None, "nothing registered yet");

    let allocator_keyed = ZRememberedSetTable::new();
    allocator_keyed.register_old_page(7, OLD_PAGE);
    assert_eq!(
        allocator_keyed.page_id_namespace(),
        Some(ZPageIdNamespace::Allocator),
        "the plain register_old_page DECLARES the allocator namespace — \
         which is what all of its production call sites mean",
    );
    // A second, same-namespace registration is not a conflict.
    allocator_keyed.register_old_page(8, OLD_PAGE);
    assert_eq!(
        allocator_keyed.page_id_namespace(),
        Some(ZPageIdNamespace::Allocator),
    );

    let context_keyed = ZRememberedSetTable::new();
    context_keyed.register_old_page_in(ZPageIdNamespace::ContextLocal, 0, OLD_PAGE);
    assert_eq!(
        context_keyed.page_id_namespace(),
        Some(ZPageIdNamespace::ContextLocal),
    );

    // Removing every page does not un-latch: the table has been used, and
    // the ids that will be registered next must still agree with the ones
    // that were.
    assert!(context_keyed.remove(0).is_some());
    assert!(context_keyed.is_empty());
    assert_eq!(
        context_keyed.page_id_namespace(),
        Some(ZPageIdNamespace::ContextLocal),
    );
}

// -----------------------------------------------------------------
// ZStoreBarrier — generation gating
// -----------------------------------------------------------------

#[test]
fn store_barrier_remembers_only_old_to_young() {
    let (barrier, c) = barrier_with_all_pages_registered();

    let old_field = OLD_BASE + 512; // page 0, offset 512
    let young_value = YOUNG_BASE + 64;
    let old_value = OLD_BASE + 4096;
    let young_field = YOUNG_BASE + 128;

    // old -> young: the only case that creates an edge a young collection
    // could miss.
    assert_eq!(
        barrier.on_reference_store(&c, old_field, young_value),
        ZStoreBarrierOutcome::Precise,
    );
    let page0 = barrier.table().get(0).expect("page 0 registered");
    assert!(page0.is_remembered(512));
    assert_eq!(page0.bits_set(), 1);

    // old -> old: found by the old-generation trace, NOT the remembered
    // set. Remembering it would be pure scan cost for no safety.
    assert_eq!(
        barrier.on_reference_store(&c, old_field + 8, old_value),
        ZStoreBarrierOutcome::Filtered,
    );
    assert!(!page0.is_remembered(520));

    // young -> young: the young collection traces the whole young
    // generation, so there is nothing to remember.
    assert_eq!(
        barrier.on_reference_store(&c, young_field, young_value),
        ZStoreBarrierOutcome::Filtered,
    );

    // young -> old: an edge in the wrong direction entirely.
    assert_eq!(
        barrier.on_reference_store(&c, young_field, old_value),
        ZStoreBarrierOutcome::Filtered,
    );

    // null store into an old field: no edge.
    assert_eq!(
        barrier.on_reference_store(&c, old_field + 16, 0),
        ZStoreBarrierOutcome::Filtered,
    );

    assert_eq!(page0.bits_set(), 1, "exactly one edge was remembered");
    assert_eq!(barrier.table().total_bits_set(), 1);

    let stats = barrier.stats();
    assert_eq!(stats.stores_seen, 5);
    assert_eq!(stats.remembered_precise, 1);
    assert_eq!(stats.remembered_coarse, 0);
    assert_eq!(stats.filtered_source_not_old, 2, "the two young sources");
    assert_eq!(
        stats.filtered_value_not_young, 2,
        "the old->old store and the null store",
    );
}

/// FINDING B (2026-08-07), pinned as behaviour rather than left as prose.
///
/// This trait's `u64`s are machine addresses. Hand it the *heap offset* the
/// load barrier returns — the other domain in this subsystem — and the edge
/// is **silently filtered**: no panic, no warning, no coarsening, and a
/// remembered set that stays empty while genuine old→young edges accumulate.
///
/// The test states both halves so the failure has a signature: the offset
/// form filters and bumps `filtered_value_not_young`, the address form is
/// `Precise`. If a caller ever wires the barrier to the offset domain, the
/// only trace in production is `filtered_value_not_young == stores_seen`.
#[test]
fn a_heap_offset_passed_where_a_machine_address_belongs_is_silently_filtered() {
    let (barrier, c) = barrier_with_all_pages_registered();

    // Pretend the ZGC heap base is YOUNG_BASE, so this young object's
    // vaddr-style heap offset is 0x40 — a number that is not in any
    // generation's address range and is therefore "not young".
    let young_address = YOUNG_BASE + 0x40;
    let young_as_offset = 0x40u64;
    let old_field = OLD_BASE + 256;

    assert_eq!(
        barrier.on_reference_store(&c, old_field, young_as_offset),
        ZStoreBarrierOutcome::Filtered,
        "an offset-domain referent must not be mistaken for a young address",
    );
    assert_eq!(
        barrier.table().total_bits_set(),
        0,
        "the edge was dropped — this is the silent failure the domain note \
         on ZGenerationContext describes",
    );

    // The same edge, expressed in this trait's domain, is recorded.
    assert_eq!(
        barrier.on_reference_store(&c, old_field, young_address),
        ZStoreBarrierOutcome::Precise,
    );
    assert_eq!(barrier.table().total_bits_set(), 1);

    let stats = barrier.stats();
    assert_eq!(stats.stores_seen, 2);
    assert_eq!(
        stats.filtered_value_not_young, 1,
        "the ONLY counter that moves when the domains are crossed",
    );
    assert_eq!(stats.remembered_precise, 1);
    assert_eq!(stats.remembered_coarse, 0);
}

#[test]
fn store_barrier_records_the_field_slot_not_the_object_base() {
    let (barrier, c) = barrier_with_all_pages_registered();
    let object = OLD_BASE + 2048;
    // Field 3 of a compact object: HEADER_SIZE + 3 * REF_FIELD_SIZE.
    let field_offset = cratonvm_types::HEADER_SIZE + 3 * Z_REMSET_GRAIN_BYTES;

    assert_eq!(
        barrier.on_reference_store_in_object(&c, object, field_offset, YOUNG_BASE),
        ZStoreBarrierOutcome::Precise,
    );

    let page0 = barrier.table().get(0).expect("page 0 registered");
    let expected = 2048 + field_offset;
    assert!(page0.is_remembered(expected));
    assert!(
        !page0.is_remembered(2048),
        "the object BASE must not be marked — that is card-table \
         behaviour and it is what the precision buys us over",
    );
    assert_eq!(page0.snapshot_offsets().len(), 0);
    let mut seen: Vec<usize> = Vec::new();
    page0.iterate(|off| seen.push(off));
    assert_eq!(seen, vec![expected]);
}

#[test]
fn store_barrier_targets_the_right_page() {
    let (barrier, c) = barrier_with_all_pages_registered();
    // One edge per page, at a distinct in-page offset.
    for page_id in 0..4u64 {
        let addr = OLD_BASE + page_id * OLD_PAGE as u64 + 128;
        assert_eq!(
            barrier.on_reference_store(&c, addr, YOUNG_BASE + 8),
            ZStoreBarrierOutcome::Precise,
        );
    }
    for page_id in 0..4u64 {
        let set = barrier.table().get(page_id).expect("registered");
        assert_eq!(
            set.bits_set(),
            1,
            "page {page_id} holds exactly its own edge"
        );
        assert!(set.is_remembered(128));
    }
}

#[test]
fn store_barrier_coarsens_an_unregistered_old_page() {
    let table = Arc::new(ZRememberedSetTable::new());
    // Deliberately register only page 0 — in the context's namespace, since
    // the context below is what will key the table.
    table.register_old_page_in(ZPageIdNamespace::ContextLocal, 0, OLD_PAGE);
    let barrier = ZStoreBarrier::new(table);
    let c = ctx();

    let addr_on_page_2 = OLD_BASE + 2 * OLD_PAGE as u64 + 64;
    assert_eq!(
        barrier.on_reference_store(&c, addr_on_page_2, YOUNG_BASE),
        ZStoreBarrierOutcome::Coarsened,
        "an unregistered page must coarsen, never silently drop the edge",
    );
    assert_eq!(barrier.table().take_coarse_pages(), vec![2]);
    assert_eq!(barrier.stats().remembered_coarse, 1);
}

// -----------------------------------------------------------------
// Promotion
// -----------------------------------------------------------------

#[test]
fn promotion_remembers_every_still_young_outgoing_reference() {
    let (barrier, c) = barrier_with_all_pages_registered();

    // A young object with four reference fields is promoted to page 1 at
    // in-page offset 1024. Two fields still point at young objects, one at
    // an old object, one is null.
    let promoted = OLD_BASE + OLD_PAGE as u64 + 1024;
    let h = cratonvm_types::HEADER_SIZE;
    let outgoing: Vec<(usize, u64)> = vec![
        (h, YOUNG_BASE + 0x40),       // young  -> remember
        (h + 8, OLD_BASE + 0x20),     // old    -> skip
        (h + 16, 0),                  // null   -> skip
        (h + 24, YOUNG_BASE + 0x900), // young  -> remember
    ];

    assert_eq!(barrier.on_promote(&c, promoted, &outgoing), 2);

    let page1 = barrier.table().get(1).expect("page 1 registered");
    assert!(page1.is_remembered(1024 + h));
    assert!(page1.is_remembered(1024 + h + 24));
    assert!(!page1.is_remembered(1024 + h + 8), "old referent, no edge");
    assert!(
        !page1.is_remembered(1024 + h + 16),
        "null referent, no edge"
    );
    assert_eq!(page1.bits_set(), 2);

    let stats = barrier.stats();
    assert_eq!(stats.promotions, 1);
    assert_eq!(stats.promotion_fields_remembered, 2);
    assert_eq!(stats.remembered_precise, 2);

    // The whole point: those edges were invisible to the store barrier
    // while the object was young (young->young stores are filtered), so
    // `on_promote` is the ONLY thing that records them.
    let young_field = YOUNG_BASE + 0x100;
    assert_eq!(
        barrier.on_reference_store(&c, young_field, YOUNG_BASE + 0x40),
        ZStoreBarrierOutcome::Filtered,
    );
}

#[test]
fn promotion_into_an_unmappable_address_reports_nothing_remembered() {
    let (barrier, c) = barrier_with_all_pages_registered();
    // An address outside the old range: constraint 1 on `on_promote` was
    // violated (the caller passed the pre-copy source address).
    let bogus = YOUNG_BASE + 512;
    assert_eq!(barrier.on_promote(&c, bogus, &[(0, YOUNG_BASE)]), 0,);
    assert_eq!(barrier.stats().promotion_fields_remembered, 0);
    assert_eq!(barrier.table().total_bits_set(), 0);
}

#[test]
fn promoted_edges_survive_into_the_collectors_snapshot() {
    let (barrier, c) = barrier_with_all_pages_registered();
    let promoted = OLD_BASE + 256;
    barrier.on_promote(&c, promoted, &[(0, YOUNG_BASE + 8)]);

    // Young-mark start.
    barrier.table().swap_all();

    let page0 = barrier.table().get(0).expect("registered");
    assert_eq!(
        page0.snapshot_offsets(),
        vec![256],
        "the promotion's edge is a root for THIS young collection",
    );
}
