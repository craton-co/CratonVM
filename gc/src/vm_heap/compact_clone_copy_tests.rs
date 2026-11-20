use super::*;
use cratonvm_types::{CompactLayout, FieldStorageKind};

fn domain_of(heap: &VmHeap) -> u32 {
    match &heap.inner {
        VmHeapInner::Generational(h) => h.layout_domain(),
        VmHeapInner::G1(h) => h.layout_domain(),
        #[cfg(feature = "zgc")]
        VmHeapInner::Zgc(h) => h.layout_domain(),
    }
}

fn exercise(backend: GcBackend, cid: u32) {
    let heap = VmHeap::new(backend, 64 * 1024 * 1024);
    let domain = domain_of(&heap);
    // int, boolean, Object, long, double, Object, byte, Object.
    cratonvm_types::register_class_layout(
        domain,
        cid,
        Arc::new(CompactLayout {
            field_disps: vec![8, 12, 16, 24, 32, 40, 48, 56],
            is_ref: vec![false, false, true, false, false, true, false, true],
            field_kinds: vec![
                FieldStorageKind::Int,
                FieldStorageKind::Boolean,
                FieldStorageKind::Reference,
                FieldStorageKind::Long,
                FieldStorageKind::Double,
                FieldStorageKind::Reference,
                FieldStorageKind::Byte,
                FieldStorageKind::Reference,
            ],
            ref_disps: vec![16, 40, 56],
            total_size: 64,
        }),
    );
    let src = heap.alloc_object(ClassId::new(cid), 8);
    assert!(cratonvm_types::is_compact_object(heap.get_header(src)));
    let child = heap.alloc_object(ClassId::new(cid + 1), 0);
    for (slot, value, desc) in [
        (0, Value::Int(-41), b'I'),
        (1, Value::Int(1), b'Z'),
        (2, Value::Object(Some(child)), b'L'),
        (3, Value::Long(i64::MIN + 3), b'J'),
        (4, Value::Double(-2.5), b'D'),
        (6, Value::Int(-7), b'B'),
    ] {
        heap.set_field_as(src, slot, value, desc);
    }
    // Slot 5 stays null; slot 7 holds a primitive boxed into a reference
    // slot, which the verbatim copy must carry as the slot-by-slot one does.
    heap.set_field(src, 7, Value::Int(123));

    let copied = heap.alloc_object(ClassId::new(cid), 8);
    let by_slot = heap.alloc_object(ClassId::new(cid), 8);
    assert_eq!(heap.copy_compact_object_fields(src, copied), Some(()));
    for slot in 0..8 {
        heap.set_field(by_slot, slot, heap.get_field(src, slot));
    }
    for slot in 0..8 {
        assert_eq!(
            heap.get_field(copied, slot),
            heap.get_field(by_slot, slot),
            "slot {slot} on {backend:?}"
        );
        assert_eq!(heap.get_field(copied, slot), heap.get_field(src, slot));
    }
    assert_eq!(heap.get_field(copied, 2), Value::Object(Some(child)));
    assert_eq!(heap.get_field(copied, 5), Value::Object(None));

    // The oop-map reader answers what `get_field` answers as references:
    // slot 2's child; slot 5 is null and slot 7's word unboxes to an int.
    let by_get_field = |from: usize| -> Vec<usize> {
        (from..8)
            .filter_map(|slot| match heap.get_field(src, slot) {
                Value::Object(Some(r)) => Some(r.as_ptr() as usize),
                _ => None,
            })
            .collect()
    };
    for from in [0, 3] {
        let mut words = vec![0xdead];
        // SAFETY: `src` is live and nothing collects in this test.
        assert!(unsafe { heap.push_compact_reference_words(src, from, &mut words) });
        assert_eq!(words[0], 0xdead, "what was on the stack stays");
        assert_eq!(
            words[1..],
            by_get_field(from)[..],
            "from slot {from} on {backend:?}"
        );
    }
    assert_eq!(by_get_field(0), vec![child.as_ptr() as usize]);
    let real = heap.alloc_object(ClassId::new(cid + 1), 0);
    heap.set_field(src, 7, Value::Object(Some(real)));
    let mut words = Vec::new();
    // SAFETY: as above.
    assert!(unsafe { heap.push_compact_reference_words(src, 0, &mut words) });
    assert_eq!(
        words,
        by_get_field(0),
        "a real reference in the screened slot is pushed"
    );

    // Refused, and nothing written: another class, a legacy body, an array.
    cratonvm_types::register_class_layout(
        domain,
        cid + 2,
        Arc::new(CompactLayout {
            field_disps: vec![8, 12, 16, 24, 32, 40, 48, 56],
            is_ref: vec![false; 8],
            field_kinds: vec![FieldStorageKind::Int; 8],
            ref_disps: vec![],
            total_size: 64,
        }),
    );
    let other = heap.alloc_object(ClassId::new(cid + 2), 8);
    assert_eq!(heap.copy_compact_object_fields(src, other), None);
    assert_eq!(heap.get_field_as(other, 0, b'I'), Value::Int(0));
    let legacy = heap.alloc_object(ClassId::new(cid + 3), 8);
    assert!(!cratonvm_types::is_compact_object(heap.get_header(legacy)));
    assert_eq!(heap.copy_compact_object_fields(legacy, legacy), None);
    let mut words = vec![1];
    // SAFETY: as above.
    assert!(!unsafe { heap.push_compact_reference_words(legacy, 0, &mut words) });
    assert_eq!(words, vec![1], "a refusal pushes nothing");
    let array = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 8);
    assert_eq!(heap.copy_compact_object_fields(array, array), None);

    // A large layout keeps the slot-by-slot copy (G1's humongous backing).
    cratonvm_types::register_class_layout(
        domain,
        cid + 4,
        Arc::new(CompactLayout {
            field_disps: vec![8, 1024],
            is_ref: vec![false, false],
            field_kinds: vec![FieldStorageKind::Int; 2],
            ref_disps: vec![],
            total_size: 1032,
        }),
    );
    let big = heap.alloc_object(ClassId::new(cid + 4), 2);
    let big_copy = heap.alloc_object(ClassId::new(cid + 4), 2);
    heap.set_field_as(big, 1, Value::Int(5), b'I');
    assert_eq!(heap.copy_compact_object_fields(big, big_copy), None);
    assert_eq!(heap.get_field_as(big_copy, 1, b'I'), Value::Int(0));

    // j27 wave 4 (lane CLONE2): the class-level answer, asked before any
    // copy exists, agrees with the copy: the copied classes are admitted,
    // the large layout and the class with no layout are not.
    assert!(heap.compact_object_copy_ready(), "{backend:?}");
    assert!(heap.compact_object_copy_admits_class(ClassId::new(cid)), "{backend:?}");
    assert!(heap.compact_object_copy_admits_class(ClassId::new(cid + 2)));
    assert!(!heap.compact_object_copy_admits_class(ClassId::new(cid + 3)));
    assert!(!heap.compact_object_copy_admits_class(ClassId::new(cid + 4)));
}

// `exercise` registers layouts for `cid ..= cid + 4` in the process-wide
// registry, and the three tests run in parallel: their ranges must not
// overlap.
#[test]
fn generational_clone_copy_is_the_slot_by_slot_copy() {
    exercise(GcBackend::Generational, 61_530);
}
#[test]
fn g1_clone_copy_is_the_slot_by_slot_copy() {
    exercise(GcBackend::G1, 61_540);
}
#[cfg(feature = "zgc")]
#[test]
fn zgc_clone_copy_is_the_slot_by_slot_copy() {
    exercise(GcBackend::Zgc, 61_550);
}
