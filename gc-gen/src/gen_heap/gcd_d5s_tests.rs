use super::*;
use cratonvm_gc_core::card_table::{CardTableOptions, CARD_SIZE};
use cratonvm_gc_core::gc_quiescence::TakeoverVerdict;
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

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn moving_cycle(heap: &GenerationalHeap, roots: &mut [ObjectRef]) -> GcResult {
    cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(true);
    cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    heap.collect_garbage_inner(roots, &[], &NoMonitors).0
}

#[test]
fn array_dirty_windows_cut_every_overlap_relative_to_the_object() {
    let ranges = [(0, 512), (1024, 2048), (4096, 4608), (8192, 8704)];
    // The header's range starts below the object: its window starts at 0.
    assert_eq!(
        array_dirty_windows(&ranges, 256, 4096),
        vec![(0, 256), (768, 1792), (3840, 4096)]
    );
    // Nothing before the object and nothing past its end.
    assert_eq!(
        array_dirty_windows(&ranges, 2048, 1024),
        Vec::<(usize, usize)>::new()
    );
    assert_eq!(array_dirty_windows(&ranges, 8192, 64), vec![(0, 64)]);
    assert_eq!(
        array_dirty_windows(&[], 0, 4096),
        Vec::<(usize, usize)>::new()
    );
}

#[test]
fn a_window_reads_the_elements_whose_slot_starts_in_it() {
    let esz = ref_element_size();
    // The header window: element 0 up to the last slot starting before
    // the window's end.
    let (first, last) = window_element_span(0, ARRAY_DATA_OFFSET + 3 * esz + 1, 100, esz);
    assert_eq!((first, last), (0, 4));
    // Clamped by the element count.
    assert_eq!(window_element_span(0, usize::MAX / 2, 10, esz), (0, 10));
    // A later window.
    let lo = ARRAY_DATA_OFFSET + 50 * esz;
    assert_eq!(window_element_span(lo, lo + 2 * esz, 100, esz), (50, 52));
}

/// Producer (g): a reference array the sweep's selective promotion
/// evacuated, with young elements 3 and 900, marks the two ELEMENTS'
/// cards on an element-precise table and its header on a header-grained
/// one; a non-array holder, and a holder whose slots name nothing young,
/// mark the header.
#[test]
fn a_selectively_promoted_reference_array_marks_its_young_elements_cards() {
    let heap = heap_with(BOTH);
    let len = 10_000u32;
    let arr = heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, len)
        .expect("old gen has room");
    let a = arr.as_ptr() as usize;
    // SAFETY: `a` is the header of the live array just allocated.
    let hdr = unsafe { &*(a as *const ObjectHeader) };
    let off = |i: usize| ARRAY_DATA_OFFSET + i * ref_element_size();
    let y = heap.alloc_object(ClassId::new(0), 0);
    let ya = y.as_ptr() as usize;
    for i in [3usize, 900] {
        // SAFETY: an in-bounds element of the live array.
        unsafe { write_ref_slot((a + off(i)) as *mut u8, ya as u64) };
    }
    let is_y = |v: usize| v == ya;
    let base = Some(heap.card_table.base_addr());
    let (mut scratch, mut out) = (Vec::new(), Vec::new());
    push_promoted_holder_cards(a as *mut u8, hdr, base, &is_y, &mut scratch, &mut out);
    assert_eq!(
        out,
        vec![a + off(3), a + off(900)],
        "one mark per young element's card"
    );
    assert!(scratch.is_empty());

    out.clear();
    push_promoted_holder_cards(a as *mut u8, hdr, None, &is_y, &mut scratch, &mut out);
    assert_eq!(out, vec![a], "header-grained table: the header");

    out.clear();
    push_promoted_holder_cards(
        a as *mut u8,
        hdr,
        base,
        &|_: usize| false,
        &mut scratch,
        &mut out,
    );
    assert_eq!(out, vec![a], "an edge with no slot of its own: the header");

    let old = heap
        .try_alloc_object_old(ClassId::new(0), 2)
        .expect("old gen has room");
    heap.set_field(old, 0, Value::Object(Some(y)));
    let o = old.as_ptr() as usize;
    // SAFETY: `o` is the header of the live object just allocated.
    let ohdr = unsafe { &*(o as *const ObjectHeader) };
    out.clear();
    push_promoted_holder_cards(o as *mut u8, ohdr, base, &is_y, &mut scratch, &mut out);
    assert_eq!(out, vec![o], "not a reference array: the header");
}


/// The decline census names a slot exactly when the arm declines, and a
/// refusing window is reported as such even with another obligation open.
#[test]
fn the_takeover_decline_reason_is_none_exactly_when_the_arm_takes() {
    for bits in 0u32..(1 << 9) {
        let bit = |i: u32| (bits >> i) & 1 == 1;
        let v = TakeoverVerdict {
            frozen: bits & 1,
            helper_windows: (bits >> 1) & 1,
            unreadable: (bits >> 2) & 1,
            pins_complete: bit(3),
            derived_pointers_resolved: bit(4),
        };
        let (req, force, cov, div) = (bit(5), bit(6), bit(7), bit(8));
        let take = pinned_copy_takes_takeover_cycle(req, force, cov, div, v);
        let why = pinned_takeover_decline_reason(req, force, cov, div, v);
        assert_eq!(why.is_none(), take, "bits={bits:#b} verdict={v:?}");
        if let Some(s) = why {
            assert!(s > takeover_decline::REACHED && s < takeover_decline::LEN);
        }
    }
    let refusing = TakeoverVerdict {
        frozen: 0,
        helper_windows: 1,
        unreadable: 0,
        pins_complete: false,
        derived_pointers_resolved: true,
    };
    assert_eq!(
        pinned_takeover_decline_reason(true, false, true, false, refusing),
        Some(takeover_decline::WINDOW_REFUSED)
    );
}

/// With the take-over flag off the census stays empty.
#[test]
fn the_takeover_decline_census_is_opt_in() {
    if cratonvm_gc_core::gc_quiescence::pinned_young_copy_takeover_enabled() {
        return;
    }
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 2);
    let mut roots = vec![y];
    let _ = moving_cycle(&heap, &mut roots);
    assert_eq!(heap.pinned_takeover_declines(), [0; takeover_decline::LEN]);
}
