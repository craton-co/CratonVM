use super::*;

/// A legacy object of `slots` slots carved from `og`, with a real header
/// (the walk strides by headers).
fn old_obj(og: &mut OldGen, slots: u32) -> usize {
    let p = og
        .alloc(HEADER_SIZE + SLOT_SIZE * slots as usize, 8)
        .expect("room in old gen");
    // SAFETY: a fresh, zeroed block sized for a header and `slots` slots.
    unsafe { (*(p as *mut ObjectHeader)).set_num_slots(slots) };
    p as usize
}

fn mark(obj: usize) {
    // SAFETY: `obj` is a live object base `old_obj` carved.
    unsafe { &*(obj as *const ObjectHeader) }.add_gc_flags(GC_FLAG_MARKED);
}

/// Only a request that IS humongous arms the compaction when refused. The
/// same function is the young-exhausted spill of an ORDINARY array
/// (`alloc_array`, `try_alloc_array_full`), and a refused spill must not
/// turn the next old-gen collection into a whole-generation compaction.
#[test]
fn only_a_humongous_refusal_arms_the_compaction() {
    if !gc_flags().old_oom_compact {
        return; // `CRATONVM_GC_OLD_OOM_COMPACT=0`: nothing arms at all
    }
    // A 64 KiB semi-space: the humongous threshold is ~32 KiB.
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    {
        let mut og = heap.old_gen.lock();
        for size in [4096usize, 256, 16] {
            while og.alloc(size, 8).is_some() {}
        }
    }
    let requests = || heap.old_gen_sizing_stats().humongous_compaction_requests;
    assert_eq!(requests(), 0);
    let small = 16usize; // an int[16]: an ordinary array
    assert!(!heap.is_humongous(ARRAY_DATA_OFFSET + small * 4));
    assert!(heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Int, small as u32)
        .is_none());
    assert_eq!(requests(), 0, "a refused SPILL arms nothing");
    assert!(!heap.old_gen.lock().humongous_refusal_pending());
    let big = 40 * 1024 / 4; // an int[] of 40 KiB: humongous, and under the 64 KiB generation
    assert!(heap.is_humongous(ARRAY_DATA_OFFSET + big * 4));
    assert!(heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Int, big as u32)
        .is_none());
    assert_eq!(requests(), 1, "a refused humongous request arms one");
    let og = heap.old_gen.lock();
    assert!(og.humongous_refusal_pending());
    assert!(og.humongous_refused_request() >= ARRAY_DATA_OFFSET + big * 4);
}

/// `settle_old_pins`: duplicates merge (sources OR), a non-base entry is
/// ignored, an object named only by live-only sources is pinned iff the
/// mark reached it, and the two resurrecting sources (interior roots,
/// unparseable young stretches) keep an unmarked object pinned. The report
/// counts each distinct pin under every source that named it.
#[test]
fn pins_are_settled_against_the_mark_and_attributed_to_their_sources() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let mut og = heap.old_gen.lock();
    let a = old_obj(&mut og, 8); // unmarked, named by a word only: dropped
    let b = old_obj(&mut og, 64); // marked: kept
    let c = old_obj(&mut og, 4); // unmarked, young stretch: kept (resurrected)
    let d = old_obj(&mut og, 2); // unmarked, interior root: kept (resurrected)
    let e = old_obj(&mut og, 1); // unmarked, one-past-the-end only: dropped
    let f = old_obj(&mut og, 3); // marked and movable: denominator only
    mark(b);
    mark(f);
    let walked = og.walk_objects();
    let base = og.base_ptr() as usize;
    let pins = vec![a, b, b, c, c + 8, d, e];
    let srcs = vec![
        OLD_PIN_SRC_WORD,
        OLD_PIN_SRC_WORD_END,
        OLD_PIN_SRC_RANGE,
        OLD_PIN_SRC_YOUNG_STRETCH,
        OLD_PIN_SRC_WORD, // `c + 8` is no base: ignored
        OLD_PIN_SRC_INTERIOR,
        OLD_PIN_SRC_RANGE_END,
    ];
    let (kept, r) = settle_old_pins(pins, srcs, &walked, base, true);
    assert_eq!(kept, vec![b, c, d]);
    assert_eq!((r.pinned, r.resurrected), (3, 2));
    let size = |slots: usize| HEADER_SIZE + SLOT_SIZE * slots;
    assert_eq!(r.pinned_bytes, size(64) + size(4) + size(2));
    assert_eq!(r.live_bytes, size(64) + size(3));
    assert_eq!(
        (r.dropped_unmarked, r.dropped_unmarked_bytes),
        (2, size(8) + size(1))
    );
    // interior, range, range end, word, word end, young stretch
    assert_eq!(r.by_source, [1, 1, 0, 0, 1, 1]);
    assert_eq!(
        r.largest,
        vec![
            (b - base, size(64), OLD_PIN_SRC_RANGE | OLD_PIN_SRC_WORD_END),
            (c - base, size(4), OLD_PIN_SRC_YOUNG_STRETCH),
            (d - base, size(2), OLD_PIN_SRC_INTERIOR),
        ],
    );
    // Nothing to pin: no pinned entries, but the full mark remains the
    // denominator for the fragmentation budget.
    let (none, empty) = settle_old_pins(Vec::new(), Vec::new(), &walked, base, true);
    assert!(none.is_empty());
    assert_eq!(empty.pinned_bytes, 0);
    assert_eq!(empty.live_bytes, size(64) + size(3));
}

/// gen r5w6/old10 — `old_pins_also_rooted`: a pin the root slice names
/// once more than the published words do (a static beside the word) is
/// "also rooted"; one named only as often as the words name it is not,
/// and an interior-valued word does not count for the base.
#[test]
fn a_pin_is_also_rooted_only_when_a_non_word_root_names_it() {
    let old_base = 0x10_0000usize;
    let largest = vec![
        (0x100, 64, OLD_PIN_SRC_WORD),
        (0x200, 64, OLD_PIN_SRC_WORD_END),
    ];
    // SAFETY: non-null, 8-aligned; never dereferenced.
    let r = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let roots = [
        r(old_base + 0x100),
        r(old_base + 0x100),
        r(old_base + 0x200),
    ];
    let words = [old_base + 0x100, old_base + 0x200, old_base + 0x208];
    assert_eq!(
        old_pins_also_rooted(&largest, old_base, &roots, &words),
        vec![true, false],
    );
    assert!(old_pins_also_rooted(&[], old_base, &roots, &words).is_empty());
}
