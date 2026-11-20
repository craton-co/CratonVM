use super::*;
use cratonvm_gc_core::card_table::{CardTableOptions, CARD_SIZE};
use std::collections::BTreeSet;

/// Deterministic xorshift, so a failing arm can be replayed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const SUMMARY: CardTableOptions = CardTableOptions {
    summary: true,
    precise_ref_array_marks: false,
};
const PRECISE: CardTableOptions = CardTableOptions {
    summary: false,
    precise_ref_array_marks: true,
};
const BOTH: CardTableOptions = CardTableOptions {
    summary: true,
    precise_ref_array_marks: true,
};

/// A heap whose card table and old-gen walk are the requested arm,
/// whatever the process's flags say.
fn heap_with(options: CardTableOptions, bot: bool) -> GenerationalHeap {
    let mut heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024)
        .with_fixed_promotion_age();
    let (base, size) = (heap.card_table.base_addr(), heap.card_table.region_size());
    // Nothing has been allocated or marked yet, so swapping the table
    // loses nothing.
    heap.card_table = CardTable::with_options(base, size, options);
    heap.old_gen.lock().set_block_offset_table_enabled(bot);
    heap
}

fn edge_set(v: &[(ObjectRef, usize, usize)]) -> BTreeSet<(usize, usize)> {
    v.iter()
        .map(|&(o, s, _)| (o.as_ptr() as usize, s))
        .collect()
}

/// `(full-scan edges, dirty-card edges, dirty-card tuples)`. The dirty-card
/// scan CONSUMES the cards, exactly as a young pause does.
fn scan(
    heap: &GenerationalHeap,
) -> (
    BTreeSet<(usize, usize)>,
    BTreeSet<(usize, usize)>,
    Vec<(ObjectRef, usize, usize)>,
) {
    let old_gen = heap.old_gen.lock();
    let young = heap.lock_young_from();
    let mut full = Vec::new();
    GenerationalHeap::scan_all_old_to_young(&old_gen, &young, &mut full);
    let mut dirty = Vec::new();
    GenerationalHeap::scan_dirty_cards(&heap.card_table, &old_gen, &young, &mut dirty);
    (edge_set(&full), edge_set(&dirty), dirty)
}

/// THE equivalence test for this lane: on a randomized old generation
/// (small objects, small reference arrays, reference arrays spanning dozens
/// of cards), after rounds of random reference stores through the real
/// barriers (`set_field`, `set_array_element`), including null stores that
/// retire edges, the dirty-card scan must deliver EXACTLY the old->young
/// `(holder, slot)` edges that the whole-old-generation scan
/// (`scan_all_old_to_young`, the `CRATONVM_GC_FULL_RSET_SCAN` oracle)
/// finds — in every combination of summary map, element-precise cards and
/// block-offset walk. Between rounds every holder the scan delivered is
/// re-marked at its HEADER, as the collector's `deferred_dirty_cards`
/// does, so header-card and element-card marks are mixed in one table.
#[test]
fn gen_r4w3_cards3_dirty_card_scan_finds_exactly_the_full_scans_edges() {
    // Thousands of duplicate barrier marks: keep them out of the window in
    // which `card_table`'s exact-count tests arm the metrics gate.
    let _arm = cratonvm_gc_core::card_table::METRICS_ARM.lock();
    let arms = [
        (CardTableOptions::LEGACY, false),
        (CardTableOptions::LEGACY, true),
        (SUMMARY, false),
        (SUMMARY, true),
        (PRECISE, false),
        (PRECISE, true),
        (BOTH, false),
        (BOTH, true),
    ];
    for (arm, &(options, bot)) in arms.iter().enumerate() {
        let heap = heap_with(options, bot);
        let mut rng = Rng(0x0DDB_1A5E_5BAD_5EED ^ ((arm as u64 + 1) * 0x9E37_79B9));
        let mut objects: Vec<(ObjectRef, usize)> = Vec::new();
        let mut arrays: Vec<(ObjectRef, usize)> = Vec::new();
        for _ in 0..400 {
            if rng.below(4) == 0 {
                let len = if rng.below(6) == 0 {
                    1000 + rng.below(4000)
                } else {
                    rng.below(64)
                };
                if let Some(a) = heap.try_alloc_array_humongous(
                    ClassId::new(0),
                    ArrayElementType::Reference,
                    len as u32,
                ) {
                    arrays.push((a, len));
                }
            } else {
                let n = 1 + rng.below(4);
                if let Some(o) = heap.try_alloc_object_old(ClassId::new(0), n) {
                    objects.push((o, n));
                }
            }
        }
        assert!(!objects.is_empty() && !arrays.is_empty(), "arm {arm}");
        let young: Vec<ObjectRef> = (0..64)
            .map(|_| heap.alloc_object(ClassId::new(0), 0))
            .collect();
        assert!(young.iter().all(|y| heap.is_in_young(y.as_ptr())));

        let mut edges_seen = 0usize;
        for round in 0..30 {
            for _ in 0..rng.below(50) {
                let value = if rng.below(5) == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(young[rng.below(young.len())]))
                };
                if rng.below(2) == 0 {
                    let (a, len) = arrays[rng.below(arrays.len())];
                    if len > 0 {
                        heap.set_array_element(a, rng.below(len), value).unwrap();
                    }
                } else {
                    let (o, n) = objects[rng.below(objects.len())];
                    heap.set_field(o, rng.below(n), value);
                }
            }
            let (full, dirty, seeded) = scan(&heap);
            assert_eq!(
                dirty, full,
                "arm {arm} ({options:?}, bot={bot}) round {round}: the dirty-card scan \
                 must deliver exactly the whole-old-gen scan's old->young edges"
            );
            edges_seen += full.len();
            // What the collector does after seeding: re-mark each holder
            // that still points into young, at its HEADER.
            let holders: Vec<usize> = seeded
                .iter()
                .map(|&(o, _, _)| o.as_ptr() as usize)
                .collect();
            heap.card_table.mark_dirty_bulk(&holders);
        }
        assert!(
            edges_seen > 100,
            "arm {arm}: the test must actually exercise edges"
        );

        // The switches did what they say.
        let stats = heap.old_gen.lock().block_offset_stats();
        assert_eq!(stats.walks > 0, bot, "arm {arm}: {stats:?}");
        assert_eq!(stats.anomalies, 0, "arm {arm}: {stats:?}");
        // One dirty card at each end of the bound: with a summary, the
        // groups in between are skipped.
        heap.card_table.clear_all();
        let skipped_before = heap.card_table.cards_skipped_by_summary();
        let (lo, _) = objects[0];
        let (hi, _) = *objects
            .iter()
            .max_by_key(|(o, _)| o.as_ptr() as usize)
            .unwrap();
        heap.card_table.mark_dirty(lo.as_ptr() as usize);
        heap.card_table.mark_dirty(hi.as_ptr() as usize);
        let _ = heap.card_table.take_dirty_cards();
        let skipped = heap.card_table.cards_skipped_by_summary() - skipped_before;
        if options.summary {
            let span = (hi.as_ptr() as usize).abs_diff(lo.as_ptr() as usize) / CARD_SIZE;
            if span > 3 * cratonvm_gc_core::card_table::SUMMARY_GROUP {
                assert!(skipped > 0, "arm {arm}: the summary skipped nothing");
            }
        } else {
            assert_eq!(skipped, 0, "arm {arm}: no summary, no summary skips");
        }
    }
}

/// Element-precise cards, directed: a barriered store into the middle of a
/// 10,000-element old reference array dirties ONLY that element's card,
/// and the scan reads only that card's elements — a young referent the
/// array holds elsewhere (planted with no barrier, so no card knows about
/// it) is not read. The two regression guards the imprecise-card page
/// named: a HEADER-card mark (the collector's deferred re-mark) still
/// scans the whole array, down to the LAST element; and a barriered store
/// into the last element is found although the header card is clean.
#[test]
fn gen_r4w3_cards3_a_mid_array_store_reads_only_its_card() {
    let _arm = cratonvm_gc_core::card_table::METRICS_ARM.lock();
    for bot in [true, false] {
        let heap = heap_with(BOTH, bot);
        let len = 10_000usize;
        let arr = heap
            .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, len as u32)
            .unwrap();
        let a = arr.as_ptr() as usize;
        let y1 = heap.alloc_object(ClassId::new(0), 0);
        let y2 = heap.alloc_object(ClassId::new(0), 0);
        let slot = |i: usize| a + ARRAY_DATA_OFFSET + i * ref_element_size();
        let card_of = |addr: usize| (addr - heap.card_table.base_addr()) / CARD_SIZE;

        // SAFETY: `slot(len - 1)` is the last element of a live old-gen
        // reference array. Deliberately NOT barriered: no card records it.
        unsafe { write_ref_slot(slot(len - 1) as *mut u8, y2.as_ptr() as u64) };
        heap.set_array_element(arr, 5000, Value::Object(Some(y1)))
            .unwrap();
        assert!(heap.card_table.is_dirty(card_of(slot(5000))));
        assert!(
            !heap.card_table.is_dirty(card_of(a)),
            "an element-precise store must not dirty the header card"
        );

        let (full, dirty, _) = scan(&heap);
        assert!(full.contains(&(a, 5000)) && full.contains(&(a, len - 1)));
        assert_eq!(
            dirty,
            BTreeSet::from([(a, 5000)]),
            "bot={bot}: only the dirty card's elements are read"
        );

        // A header-card mark means the whole array, last element included.
        heap.card_table.mark_dirty(a);
        let (_, dirty, _) = scan(&heap);
        assert_eq!(
            dirty,
            BTreeSet::from([(a, 5000), (a, len - 1)]),
            "bot={bot}"
        );

        // A barriered store into the LAST element, header card clean.
        heap.set_array_element(arr, len - 1, Value::Object(Some(y2)))
            .unwrap();
        assert!(!heap.card_table.is_dirty(card_of(a)));
        let (_, dirty, _) = scan(&heap);
        assert_eq!(dirty, BTreeSet::from([(a, len - 1)]), "bot={bot}");
    }

    // The LEGACY contract, for contrast: the same mid-array store dirties
    // the header card and the scan reads all 10,000 elements.
    let heap = heap_with(CardTableOptions::LEGACY, true);
    let len = 10_000usize;
    let arr = heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, len as u32)
        .unwrap();
    let a = arr.as_ptr() as usize;
    let y1 = heap.alloc_object(ClassId::new(0), 0);
    let y2 = heap.alloc_object(ClassId::new(0), 0);
    // SAFETY: as above.
    unsafe {
        write_ref_slot(
            (a + ARRAY_DATA_OFFSET + (len - 1) * ref_element_size()) as *mut u8,
            y2.as_ptr() as u64,
        )
    };
    heap.set_array_element(arr, 5000, Value::Object(Some(y1)))
        .unwrap();
    let (_, dirty, _) = scan(&heap);
    assert_eq!(dirty, BTreeSet::from([(a, 5000), (a, len - 1)]));
}
