use super::*;
use cratonvm_gc_core::card_table::{CardTableOptions, JitCardView, CARD_SIZE, JIT_CARD_VIEW_MAGIC};
use std::collections::BTreeSet;

const BOTH: CardTableOptions = CardTableOptions {
    summary: true,
    precise_ref_array_marks: true,
};

fn heap_with(options: CardTableOptions) -> GenerationalHeap {
    let mut heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let (base, size) = (heap.card_table.base_addr(), heap.card_table.region_size());
    // Nothing has been marked yet, so swapping the table loses nothing.
    heap.card_table = CardTable::with_options(base, size, options);
    heap
}

/// The dirty-card scan's `(holder, slot)` set. CONSUMES the cards.
fn dirty_edges(heap: &GenerationalHeap) -> BTreeSet<(usize, usize)> {
    let old_gen = heap.old_gen.lock();
    let young = heap.lock_young_from();
    let mut dirty = Vec::new();
    GenerationalHeap::scan_dirty_cards(&heap.card_table, &old_gen, &young, &mut dirty);
    dirty
        .iter()
        .map(|&(o, s, _)| (o.as_ptr() as usize, s))
        .collect()
}

/// The gate page's pinning test: what a ZGC heap does to the PROCESS block
/// (construction republishes the age-floor shape; `reset_generational_state`
/// clears `post_active`) no longer reaches a generational heap's plan.
#[test]
fn a_zgc_heaps_process_block_writes_do_not_reach_this_heaps_plan() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    if !jit_ref_store_gates_enabled() {
        assert_eq!(heap.jit_ref_store_plan(), (0, 0, 0, 0));
        return;
    }
    let plan = heap.jit_ref_store_plan();
    let (pre, post, floor, mask) = plan;
    assert_eq!(
        pre,
        jit_satb_pre_gate_addr(),
        "the pre gate is the process-wide marker disjunction"
    );
    assert_ne!(
        post, &JIT_REF_STORE_GATES.post_active as *const AtomicU8 as usize,
        "the post gate is this heap's own byte"
    );
    assert_eq!((floor, mask), (0, GC_FLAG_OLD_GEN));

    // Exactly the two process-block writes a ZGC heap makes.
    publish_jit_ref_store_plan(JIT_YOUNG_FLOOR_AGE_ZERO);
    set_jit_ref_store_post_active(false);

    assert_eq!(heap.jit_ref_store_plan(), plan, "the plan did not move");
    // SAFETY: `post` is the address of this heap's boxed gate block, which
    // lives as long as `heap`.
    let post_byte = unsafe { &*(post as *const AtomicU8) }.load(Ordering::Acquire);
    assert_eq!(post_byte, 1, "ZGC's reset no longer switches our cards off");
    // Leave the process block as a ZGC heap leaves it once armed.
    set_jit_ref_store_post_active(true);
}

/// Two generational heaps, two plans: per heap, not per process.
#[test]
fn two_generational_heaps_publish_two_post_gates() {
    let a = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    let b = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    if !jit_ref_store_gates_enabled() {
        return;
    }
    assert_ne!(a.jit_ref_store_plan().1, b.jit_ref_store_plan().1);
    assert_eq!(a.jit_ref_store_plan().0, b.jit_ref_store_plan().0);
}

/// `jit_card_table_info` now hands out the READ-ONLY view, so fetching it
/// (which `build_helpers_for` does in every generational run) leaves the
/// scan bound and the summary map armed.
#[test]
fn fetching_the_jit_card_info_leaves_the_bound_and_summary_armed() {
    let _arm = cratonvm_gc_core::card_table::METRICS_ARM.lock();
    let heap = heap_with(BOTH);
    let (view, base, end) = heap.jit_card_table_info();
    assert!(!heap.card_table.raw_card_address_escaped());
    // SAFETY: `view` is the heap's live card view.
    let v = unsafe { &*(view as *const JitCardView) };
    assert_eq!(v.magic, JIT_CARD_VIEW_MAGIC);
    assert_eq!(v.old_base.load(Ordering::Acquire), base);
    assert_eq!(v.old_end.load(Ordering::Acquire), end);
    // gcf f2/p: `BOTH` also carries the summary map, which the view now
    // names (`FLAG_SUMMARY`, for the compiled card-store barrier).
    // gcj j2/jt2: a card-aligned base also names the exact byte-map base
    // (`FLAG_BYTE_MAP_BASE`, a capability bit the compiler reads).
    assert_eq!(
        v.flags,
        JitCardView::FLAG_PRECISE_REF_ARRAYS
            | JitCardView::FLAG_SUMMARY
            | JitCardView::FLAG_BYTE_MAP_BASE
    );

    let old = heap
        .try_alloc_object_old(ClassId::new(0), 2)
        .expect("old gen has room");
    let young = heap.alloc_object(ClassId::new(0), 0);
    heap.set_field(old, 0, Value::Object(Some(young)));
    assert!(heap.card_table.scan_bound_cards() < heap.card_table.num_cards());
    let edges = dirty_edges(&heap);
    assert_eq!(edges.len(), 1, "the one old->young edge: {edges:?}");
    assert!(heap.card_table.cards_skipped_by_scan_bound() > 0);
}

/// The inline barrier's slow path calls `jit_write_barrier(vm, slot, val)`
/// with the ELEMENT's slot address (rounded down to 8) for a precise
/// reference array. The generational barrier decides by address only, so
/// that must dirty the element's card and the scan must deliver exactly
/// that element — the same outcome as `set_array_element`.
#[test]
fn an_interior_slot_address_marks_the_element_card() {
    let _arm = cratonvm_gc_core::card_table::METRICS_ARM.lock();
    let heap = heap_with(BOTH);
    let len = 10_000usize;
    let arr = heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, len as u32)
        .expect("old gen has room");
    let a = arr.as_ptr() as usize;
    let y = heap.alloc_object(ClassId::new(0), 0);
    let idx = 7_001usize;
    let slot = a + ARRAY_DATA_OFFSET + idx * ref_element_size();
    // SAFETY: `slot` is an in-bounds element of a live old reference array.
    unsafe { write_ref_slot(slot as *mut u8, y.as_ptr() as u64) };
    // SAFETY: an interior address of a live old object, used by the
    // generational barrier as an address only — never dereferenced.
    let mark = unsafe { ObjectRef::from_raw((slot & !7) as *mut u8) };
    heap.write_barrier(mark, Value::Object(Some(y)));
    let card_of = |addr: usize| (addr - heap.card_table.base_addr()) / CARD_SIZE;
    assert!(heap.card_table.is_dirty(card_of(slot)));
    assert!(
        !heap.card_table.is_dirty(card_of(a)),
        "the header card stays clean"
    );
    assert_eq!(dirty_edges(&heap), BTreeSet::from([(a, idx)]));
    // A young referent address as the "mark" is not an old holder: no card.
    heap.write_barrier(y, Value::Object(Some(y)));
    assert!(dirty_edges(&heap).is_empty());
}

/// The audit fix: a precise element mark is a DIRTY answer for that edge,
/// an unbarriered element is a clean one (reported against the ELEMENT's
/// card), and a header mark covers every element.
#[test]
fn the_rset_audit_card_test_reads_the_element_card_of_a_precise_array() {
    let _arm = cratonvm_gc_core::card_table::METRICS_ARM.lock();
    for options in [BOTH, CardTableOptions::LEGACY] {
        let heap = heap_with(options);
        let len = 10_000usize;
        let arr = heap
            .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, len as u32)
            .expect("old gen has room");
        let a = arr.as_ptr() as usize;
        // SAFETY: `a` is the header of the live array just allocated.
        let hdr = unsafe { &*(a as *const ObjectHeader) };
        let y = heap.alloc_object(ClassId::new(0), 0);
        let card_of = |addr: usize| (addr - heap.card_table.base_addr()) / CARD_SIZE;
        let slot = |i: usize| a + ARRAY_DATA_OFFSET + i * ref_element_size();
        heap.set_array_element(arr, 5000, Value::Object(Some(y)))
            .unwrap();
        // SAFETY: last element of the live array; deliberately unbarriered.
        unsafe { write_ref_slot(slot(len - 1) as *mut u8, y.as_ptr() as u64) };
        let barriered = rset_edge_card_is_dirty(&heap.card_table, a, hdr, 5000);
        let missed = rset_edge_card_is_dirty(&heap.card_table, a, hdr, len - 1);
        if options.precise_ref_array_marks {
            assert_eq!(barriered, (true, card_of(slot(5000))), "element mark");
            assert_eq!(missed, (false, card_of(slot(len - 1))), "a real miss");
            heap.card_table.mark_dirty(a);
            assert_eq!(
                rset_edge_card_is_dirty(&heap.card_table, a, hdr, len - 1),
                (true, card_of(a)),
                "a header mark covers every element"
            );
        } else {
            // LEGACY: the store dirtied the header card, which covers both.
            assert_eq!(barriered, (true, card_of(a)));
            assert_eq!(missed, (true, card_of(a)));
        }
    }
}
