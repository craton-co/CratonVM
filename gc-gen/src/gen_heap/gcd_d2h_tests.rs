use super::*;

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// One young collection with the moving path's preconditions set, as the
/// roots6 tests set them. No compiled frame is live on a test thread
/// (`gc_quiescence`'s test depth is thread-local), so the root set is
/// precise.
fn moving_cycle(heap: &GenerationalHeap, roots: &mut [ObjectRef]) -> GcResult {
    cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(true);
    cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    heap.collect_garbage_inner(roots, &[], &NoMonitors).0
}

#[test]
fn the_refusal_latch_refuses_a_new_address_and_not_a_repeated_one() {
    assert!(unrecorded_root_refusal_decision(0, 0x1000));
    assert!(!unrecorded_root_refusal_decision(0x1000, 0x1000));
    assert!(unrecorded_root_refusal_decision(0x1000, 0x2000));
}

/// A precise root naming from-space at an address the object-start walk
/// did not record refuses the cycle (nothing moves, nothing is reset), and
/// the SAME address on the next cycle does not refuse again: that cycle
/// runs, the live object is copied, and the unrecorded root is left
/// unmoved, as the fail-open exit always left it.
#[test]
fn an_unrecorded_precise_root_refuses_one_cycle_then_the_cycle_runs() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 4);
    heap.set_field(y, 1, Value::Int(0x5151));
    let base = y.as_ptr() as usize;
    // An 8-aligned address inside `y`: in from-space, not an object start.
    // SAFETY: 8-aligned, inside a live 4-slot object; never dereferenced
    // as an object by this test.
    let bad = unsafe { ObjectRef::from_raw((base + 16) as *mut u8) };
    assert_eq!(heap.unrecorded_young_root_census(), [0, 0, 0]);

    let mut roots = vec![y, bad];
    let r = moving_cycle(&heap, &mut roots);
    assert!(
        heap.young_cycle_skipped.load(Ordering::Relaxed),
        "the first cycle is refused"
    );
    assert!(r.pointer_map.is_empty(), "a refused cycle forwards nothing");
    assert_eq!(roots[0].as_ptr() as usize, base, "nothing moved");
    assert_eq!(heap.get_field(roots[0], 1), Value::Int(0x5151));
    assert_eq!(heap.unrecorded_young_root_census(), [1, 0, 1]);

    let r = moving_cycle(&heap, &mut roots);
    assert!(
        !heap.young_cycle_skipped.load(Ordering::Relaxed),
        "the same address does not refuse a second cycle"
    );
    assert_eq!(heap.unrecorded_young_root_census(), [1, 1, 2]);
    assert_ne!(
        roots[0].as_ptr() as usize,
        base,
        "the live object was copied"
    );
    assert_eq!(
        r.pointer_map.get(&base).copied(),
        Some(roots[0].as_ptr() as usize)
    );
    assert_eq!(heap.get_field(roots[0], 1), Value::Int(0x5151));
    assert_eq!(
        roots[1].as_ptr() as usize,
        base + 16,
        "the unrecorded root is left unmoved (the old fail-open exit)"
    );
}

/// A cycle whose precise roots are all object starts leaves the census at
/// zero and runs.
#[test]
fn recorded_roots_leave_the_census_at_zero() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 2);
    heap.set_field(y, 1, Value::Int(3));
    let mut roots = vec![y];
    let _ = moving_cycle(&heap, &mut roots);
    assert!(!heap.young_cycle_skipped.load(Ordering::Relaxed));
    assert_eq!(heap.unrecorded_young_root_census(), [0, 0, 0]);
    assert_eq!(heap.get_field(roots[0], 1), Value::Int(3));
}

/// The pure part of the deferral: one pair per element card, the header
/// pair for a slot-less edge, and the header pair only on a header-grained
/// table or for a holder that is not a reference array.
#[test]
fn holder_card_defer_pushes_element_cards_for_a_reference_array_only() {
    let esz = ref_element_size();
    let card = cratonvm_gc_core::card_table::CARD_SIZE;
    let card_base = 0x10_0000usize;
    // Card-aligned holder: element `i`'s card is `off(i) / CARD_SIZE`.
    let holder = card_base + 8 * card;
    let off = |i: usize| ARRAY_DATA_OFFSET + i * esz;
    let array = ObjectHeader::new(
        ClassId::new(1),
        ObjectKind::Array,
        ArrayElementType::Reference,
        1000,
        1000,
    );
    // Elements 100..=102 share a card; 900 is on another.
    assert_eq!(off(100) / card, off(102) / card);
    assert_ne!(off(100) / card, off(900) / card);

    let mut out = Vec::new();
    let mut d = HolderCardDefer::new(holder, &array, Some(card_base));
    for i in [100usize, 101, 102, 900] {
        d.slot_stays_young(off(i), &mut out);
    }
    d.finish(&mut out);
    assert_eq!(out, vec![(holder, off(100)), (holder, off(900))]);

    // A slot-less edge (the loader pin) owes the header pair as well.
    let mut out = Vec::new();
    let mut d = HolderCardDefer::new(holder, &array, Some(card_base));
    d.slot_stays_young(off(900), &mut out);
    d.whole_object();
    d.finish(&mut out);
    assert_eq!(out, vec![(holder, off(900)), (holder, 0)]);

    // Header-grained table: one header pair, however many slots.
    let mut out = Vec::new();
    let mut d = HolderCardDefer::new(holder, &array, None);
    d.slot_stays_young(off(100), &mut out);
    d.slot_stays_young(off(900), &mut out);
    d.finish(&mut out);
    assert_eq!(out, vec![(holder, 0)]);

    // Not a reference array: the header pair.
    let object = ObjectHeader::new(
        ClassId::new(1),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        4,
    );
    let mut out = Vec::new();
    let mut d = HolderCardDefer::new(holder, &object, Some(card_base));
    d.slot_stays_young(HEADER_SIZE + SLOT_SIZE, &mut out);
    d.finish(&mut out);
    assert_eq!(out, vec![(holder, 0)]);

    // Nothing stayed young: nothing is pushed.
    let mut out = Vec::new();
    HolderCardDefer::new(holder, &array, Some(card_base)).finish(&mut out);
    assert!(out.is_empty());
}

/// A young reference array promoted by a moving cycle while its element
/// 9000 names an object that stays young: on an element-precise table the
/// ELEMENT's card is dirty after the cycle, not the promoted array's
/// header card (which would make the next pause rescan all ten thousand
/// elements), and the next cycle still finds the edge through it.
#[test]
fn a_promoted_reference_array_defers_its_young_elements_card_not_its_header() {
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    if !heap.card_table.precise_ref_array_marks() {
        // `CRATONVM_GC_PRECISE_ARRAY_CARDS=0` in the environment: the
        // table is header-grained by design and the header pair is right.
        return;
    }
    let cbase = heap.card_table.base_addr();
    let card_of = |addr: usize| (addr - cbase) / cratonvm_gc_core::card_table::CARD_SIZE;
    let a = heap.alloc_array(ClassId::new(2), ArrayElementType::Reference, 10_000);
    assert!(
        heap.is_in_young(a.as_ptr()),
        "not humongous at this young size"
    );
    let mut roots = vec![a];
    let mut checked = false;
    for cycle in 0..16i32 {
        if heap.is_in_old(roots[0].as_ptr()) {
            break;
        }
        // A FRESH referent every cycle, so it is age 0 on the cycle that
        // promotes the array and stays young.
        let y = heap.alloc_object(ClassId::new(7), 2);
        heap.set_field(y, 1, Value::Int(9000 + cycle));
        heap.set_array_element(roots[0], 9000, Value::Object(Some(y)))
            .expect("in bounds");
        // A megabyte of garbage beside it, so survival stays low and
        // promote-on-pressure does not tenure the referent too.
        for _ in 0..(1024 * 1024) / 48 {
            let _ = heap.try_alloc_object(ClassId::new(1), 2);
        }
        let _ = moving_cycle(&heap, &mut roots);
        let arr = roots[0];
        if !heap.is_in_old(arr.as_ptr()) {
            continue;
        }
        let Ok(Value::Object(Some(now))) = heap.get_array_element(arr, 9000) else {
            panic!("cycle {cycle}: element 9000 lost its reference");
        };
        assert_eq!(heap.get_field(now, 1), Value::Int(9000 + cycle));
        if !heap.is_in_young(now.as_ptr()) {
            // Tenured together: no old->young edge to defer.
            break;
        }
        let header_card = card_of(arr.as_ptr() as usize);
        let element_card =
            card_of(arr.as_ptr() as usize + ARRAY_DATA_OFFSET + 9000 * ref_element_size());
        assert_ne!(header_card, element_card);
        assert!(
            heap.card_table.is_dirty(element_card),
            "cycle {cycle}: the promoted array's young element keeps its own card dirty"
        );
        assert!(
            !heap.card_table.is_dirty(header_card),
            "cycle {cycle}: the promoted array's header card is clean (no whole-array rescan)"
        );
        // The next cycle finds the edge through that card.
        let _ = moving_cycle(&heap, &mut roots);
        let Ok(Value::Object(Some(again))) = heap.get_array_element(roots[0], 9000) else {
            panic!("the element lost its reference on the cycle after the promotion");
        };
        assert_eq!(heap.get_field(again, 1), Value::Int(9000 + cycle));
        checked = true;
        break;
    }
    assert!(
        checked,
        "the array was never promoted while its referent stayed young: the test \
         did not exercise the promoted scan"
    );
}
