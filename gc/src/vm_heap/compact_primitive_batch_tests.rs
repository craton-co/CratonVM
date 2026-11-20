use super::*;
use cratonvm_types::{CompactLayout, FieldStorageKind};

fn exercise(backend: GcBackend, cid: u32) {
    let heap = VmHeap::new(backend, 64 * 1024 * 1024);
    let domain = match &heap.inner {
        VmHeapInner::Generational(h) => h.layout_domain(),
        VmHeapInner::G1(h) => h.layout_domain(),
        #[cfg(feature = "zgc")]
        VmHeapInner::Zgc(h) => h.layout_domain(),
    };
    cratonvm_types::register_class_layout(
        domain,
        cid,
        Arc::new(CompactLayout {
            field_disps: vec![8, 12, 16, 24, 32, 40],
            is_ref: vec![false, false, false, false, false, true],
            field_kinds: vec![
                FieldStorageKind::Int,
                FieldStorageKind::Boolean,
                FieldStorageKind::Long,
                FieldStorageKind::Float,
                FieldStorageKind::Double,
                FieldStorageKind::Reference,
            ],
            ref_disps: vec![40],
            total_size: 48,
        }),
    );
    let actual = heap.alloc_object(ClassId::new(cid), 6);
    let expected = heap.alloc_object(ClassId::new(cid), 6);
    assert!(cratonvm_types::is_compact_object(heap.get_header(actual)));
    let slots = [(0, b'I'), (1, b'Z'), (2, b'J'), (3, b'F'), (4, b'D')];
    let values = [
        Value::Int(41),
        Value::Int(2),
        Value::Int(-17),
        Value::Float(1.25),
        Value::Double(-2.5),
    ];
    for ((index, desc), value) in slots.iter().zip(&values) {
        heap.set_field_as(expected, *index, *value, *desc);
    }
    assert_eq!(
        heap.try_set_compact_primitive_fields(actual, &slots, &values),
        Some(actual)
    );
    for (index, desc) in slots {
        assert_eq!(
            heap.get_field_as(actual, index, desc),
            heap.get_field_as(expected, index, desc)
        );
    }
    let read_slots = [
        (0, b'I'),
        (1, b'Z'),
        (2, b'J'),
        (3, b'F'),
        (4, b'D'),
        (5, b'L'),
    ];
    let mut batch = [Value::Uninitialized; 6];
    assert_eq!(
        heap.try_get_compact_fields(actual, &read_slots, &mut batch),
        Some(actual)
    );
    for (i, (slot, desc)) in read_slots.iter().enumerate() {
        assert_eq!(batch[i], heap.get_field_as(actual, *slot, *desc));
    }
    // An invalid later slot must not publish a partially-read output.
    let mut untouched = [Value::Int(77); 2];
    assert_eq!(
        heap.try_get_compact_fields(actual, &[(0, b'I'), (6, b'I')], &mut untouched),
        None
    );
    assert_eq!(untouched, [Value::Int(77); 2]);
    // The actual reference kind overrides a misleading primitive hint.
    let child = heap.alloc_object(ClassId::new(cid + 10), 0);
    heap.set_field_as(actual, 5, Value::Object(Some(child)), b'L');
    assert_eq!(
        heap.try_set_compact_primitive_fields(
            actual,
            &[(0, b'I'), (5, b'I')],
            &[Value::Int(99), Value::Int(7)]
        ),
        None
    );
    assert_eq!(heap.get_field_as(actual, 0, b'I'), Value::Int(41));
    assert_eq!(
        heap.get_field_as(actual, 5, b'L'),
        Value::Object(Some(child))
    );
    // Validate every slot before the first store: a declined batch leaves
    // its prefix untouched for the caller's ordinary per-slot replay.
    assert_eq!(
        heap.try_set_compact_primitive_fields(
            actual,
            &[(0, b'I'), (6, b'I')],
            &[Value::Int(99), Value::Int(7)]
        ),
        None
    );
    assert_eq!(heap.get_field_as(actual, 0, b'I'), Value::Int(41));
    assert_eq!(
        heap.try_set_compact_primitive_fields(
            actual,
            &[(0, b'I'), (1, b'Z')],
            &[Value::Object(Some(child)), Value::Int(1)]
        ),
        None
    );
    let legacy = heap.alloc_object(ClassId::new(cid + 11), 6);
    assert_eq!(
        heap.try_set_compact_primitive_fields(
            legacy,
            &[(0, b'I'), (1, b'I')],
            &[Value::Int(1), Value::Int(2)]
        ),
        None
    );
    // Reference reads must still unwrap the compatibility wrapper, before
    // descriptor coercion; compare the existing scalar accessor.
    heap.set_field(actual, 5, Value::Int(123));
    assert_eq!(
        heap.try_get_compact_fields(actual, &[(0, b'I'), (5, b'I')], &mut untouched),
        Some(actual)
    );
    assert_eq!(untouched[1], heap.get_field_as(actual, 5, b'I'));
    assert_eq!(untouched[1], Value::Int(123));
    assert_eq!(
        heap.try_get_compact_fields(legacy, &[(0, b'I'), (1, b'I')], &mut untouched),
        None
    );
    // Sparse/large layouts keep the scalar path (including fragmented G1).
    cratonvm_types::register_class_layout(
        domain,
        cid + 12,
        Arc::new(CompactLayout {
            field_disps: vec![8, 1024],
            is_ref: vec![false, false],
            field_kinds: vec![FieldStorageKind::Int; 2],
            ref_disps: vec![],
            total_size: 1032,
        }),
    );
    let sparse = heap.alloc_object(ClassId::new(cid + 12), 2);
    assert_eq!(
        heap.try_get_compact_fields(sparse, &[(0, b'I'), (1, b'I')], &mut untouched),
        None
    );
    assert_eq!(
        heap.try_set_compact_primitive_fields(
            sparse,
            &[(0, b'I'), (1, b'I')],
            &[Value::Int(1); 2]
        ),
        None
    );
    let array = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 6);
    assert_eq!(
        heap.try_get_compact_fields(array, &[(0, b'I'), (1, b'I')], &mut untouched),
        None
    );
    assert_eq!(
        heap.try_set_compact_primitive_fields(
            array,
            &[(0, b'I'), (1, b'I')],
            &[Value::Int(1), Value::Int(2)]
        ),
        None
    );
}

#[test]
fn generational_compact_primitive_batch_preserves_scalar_semantics() {
    exercise(GcBackend::Generational, 61_431);
}
#[test]
fn g1_compact_primitive_batch_preserves_scalar_semantics() {
    exercise(GcBackend::G1, 61_432);
}
#[cfg(feature = "zgc")]
#[test]
fn zgc_compact_primitive_batch_preserves_scalar_semantics() {
    exercise(GcBackend::Zgc, 61_433);
}

/// zt t18/zb1 (zr1br A2a): the batch read reads reference slots raw, so an
/// ARMED ZGC heap declines it to the per-slot accessors (which barrier),
/// and the per-slot answers are the batch's unarmed answers.
/// `zgc_load_barrier_armed` follows the heap, and is false on the others.
#[cfg(feature = "zgc")]
#[test]
fn zt18zb1_an_armed_zgc_heap_declines_the_compact_batch_read() {
    let heap = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
    let VmHeapInner::Zgc(z) = &heap.inner else {
        panic!("a ZGC heap");
    };
    let cid = 61_434;
    cratonvm_types::register_class_layout(
        z.layout_domain(),
        cid,
        Arc::new(CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![false, true],
            field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
            ref_disps: vec![16],
            total_size: 24,
        }),
    );
    let target = heap.alloc_object(ClassId::new(1), 0);
    let obj = heap.alloc_object(ClassId::new(cid), 2);
    assert!(cratonvm_types::is_compact_object(heap.get_header(obj)));
    heap.set_field_as(obj, 0, Value::Int(5), b'I');
    heap.set_field_as(obj, 1, Value::Object(Some(target)), b'L');
    let slots = [(0, b'I'), (1, b'L')];

    let mut unarmed = [Value::Uninitialized; 2];
    let served = heap.try_get_compact_fields(obj, &slots, &mut unarmed);
    assert!(!heap.zgc_load_barrier_armed());

    assert!(z.arm_load_barrier_plain_stores(false));
    let armed_flag = heap.zgc_load_barrier_armed();
    let mut armed = [Value::Uninitialized; 2];
    let declined = heap.try_get_compact_fields(obj, &slots, &mut armed);
    let scalar: Vec<Value> = slots
        .iter()
        .map(|&(i, d)| heap.get_field_as(obj, i, d))
        .collect();
    z.set_barrier_color(None);

    assert_eq!(served, Some(obj), "unarmed: the batch serves");
    assert!(armed_flag, "armed: the predicate follows the heap");
    assert_eq!(declined, None, "armed: the batch declines");
    assert_eq!(armed, [Value::Uninitialized; 2], "and writes nothing");
    assert_eq!(
        scalar,
        unarmed.to_vec(),
        "the per-slot path answers the same"
    );
    assert_eq!(unarmed[1], Value::Object(Some(target)));
}

/// zt t19/zb2 (zr1br A2b): the JIT helpers' read seam
/// (`load_ref_slot_barriered`, what `jit_aaload` and `jit_getfield`'s
/// compact reference arm read through) decodes a COLORED element to its
/// object and null to 0, and `zgc_load_barrier_colored_stores` follows
/// the heap's mode: off unarmed, off in plain-store mode, on in
/// colored-store mode.
#[cfg(feature = "zgc")]
#[test]
fn zt19zb2_the_jit_read_seam_decodes_colored_elements() {
    // zt t20/zb3: one colored-store heap per process at a time (the
    // colored-slot decode base is owned).
    let _colored = cratonvm_gc_zgc::zgc::colored_arming_test_lock();
    let heap = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
    let VmHeapInner::Zgc(z) = &heap.inner else {
        panic!("a ZGC heap");
    };
    let target = heap.alloc_object(ClassId::new(1), 0);
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let unarmed = heap.zgc_load_barrier_colored_stores();
    assert!(z.arm_load_barrier_plain_stores(false));
    let plain_mode = heap.zgc_load_barrier_colored_stores();
    z.set_barrier_color(None);

    assert!(z.arm_load_barrier_colored_stores());
    let colored_mode = heap.zgc_load_barrier_colored_stores();
    heap.set_array_element(arr, 0, Value::Object(Some(target)))
        .expect("in bounds");
    let slot0 = heap.array_data_ptr(arr).expect("an array") as *const u8;
    // SAFETY: element 1 of the same live two-element reference array.
    let slot1 = unsafe { slot0.add(8) };
    // SAFETY: both are live 8-aligned reference elements.
    let word0 = unsafe { std::ptr::read_volatile(slot0 as *const u64) };
    let read0 = unsafe { heap.load_ref_slot_barriered(slot0) };
    let read1 = unsafe { heap.load_ref_slot_barriered(slot1) };
    z.set_barrier_color(None);

    assert!(
        !unarmed && !plain_mode && colored_mode,
        "the predicate follows the mode"
    );
    assert!(
        cratonvm_gc_zgc::zgc::vaddr::is_colored_word(word0),
        "the element holds a colored word: {word0:#x}"
    );
    assert_eq!(read0, target.as_ptr() as u64, "decoded to its object");
    assert_eq!(read1, 0, "null stays 0");
}

/// The armed `System.arraycopy` arm (`copy_reference_slots_colored`):
/// refused unarmed; armed with colored stores it copies each element as a
/// colored word naming the same object (null stays null), forward and as
/// an overlapping same-array copy to a higher index.
#[cfg(feature = "zgc")]
#[test]
fn the_armed_reference_copy_stores_colored_words_of_the_same_objects() {
    let _colored = cratonvm_gc_zgc::zgc::colored_arming_test_lock();
    let heap = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
    let VmHeapInner::Zgc(z) = &heap.inner else {
        panic!("a ZGC heap");
    };
    let a = heap.alloc_object(ClassId::new(1), 0);
    let b = heap.alloc_object(ClassId::new(1), 0);
    let src = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 4);
    let dst = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 4);
    let base = |arr| heap.array_data_ptr(arr).expect("an array") as usize;
    let unarmed = z.copy_reference_slots_colored(base(src), 0, base(dst), 0, 4, false);

    assert!(z.arm_load_barrier_colored_stores());
    for (i, v) in [Some(a), None, Some(b), Some(a)].into_iter().enumerate() {
        heap.set_array_element(src, i, Value::Object(v)).expect("in bounds");
    }
    let forward = z.copy_reference_slots_colored(base(src), 0, base(dst), 1, 3, false);
    // Overlapping: src[0..3] -> src[1..4], memmove order.
    let overlap = z.copy_reference_slots_colored(base(src), 0, base(src), 1, 3, true);
    let word = |arr, i: usize| {
        // SAFETY: element `i` of a live four-element reference array.
        unsafe { std::ptr::read_volatile((base(arr) + i * 8) as *const u64) }
    };
    let read = |arr, i: usize| {
        // SAFETY: as above.
        unsafe { heap.load_ref_slot_barriered((base(arr) + i * 8) as *const u8) }
    };
    let dst_words: Vec<u64> = (1..4).map(|i| word(dst, i)).collect();
    let dst_reads: Vec<u64> = (0..4).map(|i| read(dst, i)).collect();
    let src_reads: Vec<u64> = (0..4).map(|i| read(src, i)).collect();
    z.set_barrier_color(None);

    assert!(!unarmed, "refused unarmed");
    assert!(forward && overlap, "armed: copied");
    assert!(cratonvm_gc_zgc::zgc::vaddr::is_colored_word(dst_words[0]), "a colored word");
    assert!(cratonvm_gc_zgc::zgc::vaddr::is_colored_word(dst_words[2]), "a colored word");
    let (pa, pb) = (a.as_ptr() as u64, b.as_ptr() as u64);
    assert_eq!(dst_reads, vec![0, pa, 0, pb], "dst[1..4] = src[0..3]");
    assert_eq!(src_reads, vec![pa, pa, 0, pb], "memmove order on overlap");
}
