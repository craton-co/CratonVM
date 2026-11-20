use super::*;

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// A heap whose only reference to a young object `y` (field 1 =
/// `Int(tag)`) is a slot of an old object `h` that sits in a WALK GAP: `h`
/// is carved end to end with `b`, whose kind byte no walk can size, and
/// `h`'s card is dirty. The shape of
/// `gen_r5w1_oldgen5_tests::a_young_object_named_only_from_a_walk_gap_is_a_conservative_root`.
/// Returns `(heap, y, address of h's slot)`.
fn heap_with_a_walk_gap_root(tag: i32) -> (GenerationalHeap, ObjectRef, usize) {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024)
        .with_fixed_promotion_age();
    let y = heap.alloc_object(ClassId::new(7), 2);
    heap.set_field(y, 1, Value::Int(tag));
    assert!(heap.is_in_young(y.as_ptr()));
    let size = HEADER_SIZE + SLOT_SIZE;
    let slot = {
        let mut og = heap.old_gen.lock();
        let p = og.alloc(2 * size, 8).expect("room");
        // SAFETY: a fresh, zeroed block holding two one-slot objects.
        unsafe {
            (*(p as *mut ObjectHeader)).set_num_slots(1);
            (*(p.add(size) as *mut ObjectHeader)).set_num_slots(1);
            std::ptr::write(
                p.add(size + HEADER_SIZE) as *mut Value,
                Value::Object(Some(y)),
            );
            // Not a valid `ObjectKind`: every walk breaks at `b`.
            std::ptr::write(p.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8);
        }
        p as usize + size + HEADER_SIZE
    };
    // `h` and its slot (they can straddle a card line).
    heap.card_table.mark_dirty(slot - HEADER_SIZE);
    heap.card_table.mark_dirty(slot);
    (heap, y, slot)
}

/// Is the card covering `addr` dirty?
fn card_dirty(heap: &GenerationalHeap, addr: usize) -> bool {
    let idx = (addr - heap.card_table.base_addr()) / cratonvm_gc_core::card_table::CARD_SIZE;
    heap.card_table.is_dirty(idx)
}

/// `gengc-r4w4-oldgen4-a-walk-gap-hides-old-to-young-roots-from-the-card-scan`,
/// the MOVING young cycle: the walk-gap word is a conservative root, so
/// `y` survives a cycle with no other root, IN PLACE (the word can never
/// be rewritten -- it may not be a reference), with its payload, and the
/// gap's card stays dirty for the next pause. Before gen r5w2/roots6 the
/// scan reported the word and the cycle ignored it: `y` was left in the
/// evacuated semi-space.
#[test]
fn a_moving_young_cycle_keeps_a_walk_gap_referent_alive_in_place() {
    let (heap, y, slot) = heap_with_a_walk_gap_root(4242);
    cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(true);
    cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    let mut roots: Vec<ObjectRef> = Vec::new();
    let (result, _) = heap.collect_garbage_inner(&mut roots, &[], &NoMonitors);
    let addr = y.as_ptr() as usize;
    assert_eq!(
        result.pointer_map.get(&addr).copied().unwrap_or(addr),
        addr,
        "a walk-gap referent must not move"
    );
    assert!(
        heap.is_in_young(y.as_ptr()),
        "`y` stayed in the allocation space"
    );
    assert_eq!(
        heap.get_field(y, 1),
        Value::Int(4242),
        "`y` kept its payload"
    );
    assert!(card_dirty(&heap, slot), "the gap's card stays dirty");
}

/// `gengc-r4w3-cards3-precise-array-cards-residuals` item 2: on an
/// element-precise card table, a wide old reference array whose element
/// 9000 names a young survivor keeps the ELEMENT's card dirty across a
/// moving young cycle -- not its header card, which would make every later
/// young pause rescan all ten thousand elements -- and the next cycle
/// still finds the edge through that card.
#[test]
fn a_surviving_young_element_keeps_its_own_card_dirty_not_the_arrays_header() {
    let mut heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024)
        .with_fixed_promotion_age();
    let (cbase, csize) = (heap.card_table.base_addr(), heap.card_table.region_size());
    // Nothing allocated or marked yet: swapping the table loses nothing.
    heap.card_table = CardTable::with_options(
        cbase,
        csize,
        cratonvm_gc_core::card_table::CardTableOptions {
            summary: false,
            precise_ref_array_marks: true,
        },
    );
    let a = heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, 10_000)
        .expect("room for a 10 000-element array");
    assert!(
        heap.is_in_old(a.as_ptr()),
        "a humongous array lives in old gen"
    );
    let y = heap.alloc_object(ClassId::new(7), 2);
    heap.set_field(y, 1, Value::Int(9000));
    heap.set_array_element(a, 9000, Value::Object(Some(y)))
        .unwrap();
    let card_of = |addr: usize| (addr - cbase) / cratonvm_gc_core::card_table::CARD_SIZE;
    let header_card = card_of(a.as_ptr() as usize);
    let element_card =
        card_of(a.as_ptr() as usize + ARRAY_DATA_OFFSET + 9000 * ref_element_size());
    assert_ne!(header_card, element_card);
    for cycle in 0..2 {
        // A megabyte of garbage beside the survivor, so the cycle frees
        // most of young and does not arm promote-on-pressure (which would
        // tenure the survivor on the second cycle).
        for _ in 0..(1024 * 1024) / 48 {
            let _ = heap.try_alloc_object(ClassId::new(1), 2);
        }
        cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(true);
        cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
        cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
        cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        let mut roots: Vec<ObjectRef> = Vec::new();
        let _ = heap.collect_garbage_inner(&mut roots, &[], &NoMonitors);
        let Ok(Value::Object(Some(now))) = heap.get_array_element(a, 9000) else {
            panic!("cycle {cycle}: element 9000 lost its reference");
        };
        assert!(
            heap.is_in_young(now.as_ptr()),
            "cycle {cycle}: the survivor stays young"
        );
        assert_eq!(
            heap.get_field(now, 1),
            Value::Int(9000),
            "cycle {cycle}: payload"
        );
        assert!(
            heap.card_table.is_dirty(element_card),
            "cycle {cycle}: the element's card is re-marked"
        );
        assert!(
            !heap.card_table.is_dirty(header_card),
            "cycle {cycle}: the header card is not (a whole-array rescan)"
        );
    }
}

/// The same shape through the NON-MOVING sweep: the walk-gap word must be
/// marked (through the interior-aware conservative marker, so it is among
/// the oracle's candidates) or the sweep frees `y` under it.
#[test]
fn the_non_moving_sweep_keeps_a_walk_gap_referent_alive() {
    let (heap, y, slot) = heap_with_a_walk_gap_root(777);
    let mut roots: Vec<ObjectRef> = Vec::new();
    let (result, _) = heap.run_non_moving_young_cycle(&mut roots, &[], &NoMonitors, false);
    let addr = y.as_ptr() as usize;
    assert_eq!(
        result.pointer_map.get(&addr).copied().unwrap_or(addr),
        addr,
        "a walk-gap referent is pinned against selective promotion"
    );
    assert_eq!(heap.get_field(y, 1), Value::Int(777), "`y` was not swept");
    assert!(card_dirty(&heap, slot), "the gap's card stays dirty");
}

/// gen r5w6/old10 (cards3 item 3 row b): the NON-MOVING sweep's re-dirty
/// of a reference-array hit on an element-precise table is the ELEMENT's
/// card, as the moving cycle's deferral is
/// (`a_surviving_young_element_keeps_its_own_card_dirty_not_the_arrays_header`).
/// Before, it re-dirtied the array's HEADER card, so every later pause
/// rescanned all ten thousand elements. The edge itself must survive: the
/// next non-moving cycle still finds `y` through the element's card.
#[test]
fn the_non_moving_sweep_re_dirties_the_elements_card_not_the_arrays_header() {
    let mut heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let (cbase, csize) = (heap.card_table.base_addr(), heap.card_table.region_size());
    // Nothing allocated or marked yet: swapping the table loses nothing.
    heap.card_table = CardTable::with_options(
        cbase,
        csize,
        cratonvm_gc_core::card_table::CardTableOptions {
            summary: false,
            precise_ref_array_marks: true,
        },
    );
    let a = heap
        .try_alloc_array_humongous(ClassId::new(0), ArrayElementType::Reference, 10_000)
        .expect("room for a 10 000-element array");
    assert!(
        heap.is_in_old(a.as_ptr()),
        "a humongous array lives in old gen"
    );
    let y = heap.alloc_object(ClassId::new(7), 2);
    heap.set_field(y, 1, Value::Int(9001));
    heap.set_array_element(a, 9000, Value::Object(Some(y)))
        .unwrap();
    let card_of = |addr: usize| (addr - cbase) / cratonvm_gc_core::card_table::CARD_SIZE;
    let header_card = card_of(a.as_ptr() as usize);
    let element_card =
        card_of(a.as_ptr() as usize + ARRAY_DATA_OFFSET + 9000 * ref_element_size());
    assert_ne!(header_card, element_card);
    for cycle in 0..2 {
        let mut roots: Vec<ObjectRef> = Vec::new();
        let _ = heap.run_non_moving_young_cycle(&mut roots, &[], &NoMonitors, false);
        let Ok(Value::Object(Some(now))) = heap.get_array_element(a, 9000) else {
            panic!("cycle {cycle}: element 9000 lost its reference");
        };
        assert_eq!(
            heap.get_field(now, 1),
            Value::Int(9001),
            "cycle {cycle}: payload"
        );
        if heap.is_in_young(now.as_ptr()) {
            assert!(
                heap.card_table.is_dirty(element_card),
                "cycle {cycle}: the element's card is re-marked"
            );
        }
        assert!(
            !heap.card_table.is_dirty(header_card),
            "cycle {cycle}: the header card is not (a whole-array rescan)"
        );
    }
}

/// `is_object_address` must safely reject a candidate that looks forwarded
/// (e.g. coincidental low bits 0b11 with an unmapped payload address)
/// without dereferencing through the forwarding address and faulting.
#[test]
fn is_object_address_rejects_candidate_with_forwarding_bits_without_faulting() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(1), 2);
    let ptr = obj.as_ptr();

    // 1. Test an allocated object that was evacuated / marked forwarded
    // with unmapped target 0x100000000.
    unsafe {
        let header = &mut *(ptr as *mut ObjectHeader);
        let mark = header.mark();
        let forwarded_mark =
            (mark & !cratonvm_types::MARK_STATE_MASK) | cratonvm_types::MARK_FORWARDED;
        header.mark_word.store(forwarded_mark, Ordering::Release);
        let second_word = (ptr as *mut u8).add(8) as *mut u64;
        *second_word = 0x100000000;
    }

    let result = heap.is_object_address(ptr as usize);
    assert!(
        result.is_none(),
        "forwarded object must be rejected by is_object_address"
    );

    // 2. Test an untracked candidate inside an array payload that decodes as forwarded
    let arr = heap.alloc_array(ClassId::new(2), ArrayElementType::Long, 10);
    let cand_ptr = unsafe { arr.as_ptr().add(ARRAY_DATA_OFFSET) };
    unsafe {
        // Write a header word: class_id = 1, mark_word = Object with MARK_FORWARDED
        let mark = cratonvm_types::MARK_FORWARDED
            | ((ObjectKind::Object as u32) & 0x3) << 16
            | ((GC_FLAG_HEADER as u32) & 0xF) << 24;
        let header_word = 1u64 | ((mark as u64) << 32);
        (cand_ptr as *mut u64).write(header_word);
        // Write second word: unmapped target 0x100000000
        ((cand_ptr as *mut u64).add(1)).write(0x100000000);
    }

    let result2 = heap.is_object_address(cand_ptr as usize);
    assert!(
        result2.is_none(),
        "untracked forwarded candidate must be rejected by is_object_address"
    );
}
