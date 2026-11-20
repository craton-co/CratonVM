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
    // StackFrameInfo's shape: references and ints interleaved.
    cratonvm_types::register_class_layout(
        domain,
        cid,
        Arc::new(CompactLayout {
            field_disps: vec![8, 16, 24, 32, 40, 48],
            is_ref: vec![true, false, true, false, true, true],
            field_kinds: vec![
                FieldStorageKind::Reference,
                FieldStorageKind::Int,
                FieldStorageKind::Reference,
                FieldStorageKind::Int,
                FieldStorageKind::Reference,
                FieldStorageKind::Reference,
            ],
            ref_disps: vec![8, 24, 40, 48],
            total_size: 56,
        }),
    );
    let actual = heap.alloc_object(ClassId::new(cid), 6);
    let expected = heap.alloc_object(ClassId::new(cid), 6);
    assert!(cratonvm_types::is_compact_object(heap.get_header(actual)));
    let a = heap.alloc_object(ClassId::new(cid + 10), 0);
    let b = heap.alloc_object(ClassId::new(cid + 11), 0);
    let slots = [(0, b'L'), (1, b'I'), (2, b'L'), (3, b'I'), (4, b'L'), (5, b'L')];
    let values = [
        Value::Object(Some(a)),
        Value::Int(0x40),
        Value::Object(Some(b)),
        Value::Int(-1),
        Value::Object(None),
        Value::Object(Some(a)),
    ];
    for ((index, desc), value) in slots.iter().zip(&values) {
        heap.set_field_as(expected, *index, *value, *desc);
    }
    assert_eq!(heap.try_set_compact_fields(actual, &slots, &values), Some(actual));
    for (index, desc) in slots {
        assert_eq!(
            heap.get_field_as(actual, index, desc),
            heap.get_field_as(expected, index, desc),
            "slot {index}"
        );
    }
    // A primitive into a reference slot is boxed by the scalar accessor:
    // the batch declines and writes nothing, the int slot included.
    assert_eq!(
        heap.try_set_compact_fields(
            actual,
            &[(1, b'I'), (2, b'L')],
            &[Value::Int(7), Value::Int(5)]
        ),
        None
    );
    assert_eq!(heap.get_field_as(actual, 1, b'I'), Value::Int(0x40));
    // An object into a primitive slot declines.
    assert_eq!(
        heap.try_set_compact_fields(
            actual,
            &[(0, b'L'), (1, b'I')],
            &[Value::Object(Some(b)), Value::Object(Some(a))]
        ),
        None
    );
    assert_eq!(heap.get_field_as(actual, 0, b'L'), Value::Object(Some(a)));
    // A slot past the layout declines before the first store.
    assert_eq!(
        heap.try_set_compact_fields(
            actual,
            &[(0, b'L'), (6, b'L')],
            &[Value::Object(Some(b)), Value::Object(None)]
        ),
        None
    );
    assert_eq!(heap.get_field_as(actual, 0, b'L'), Value::Object(Some(a)));
    // A legacy receiver and an array keep the scalar path.
    let legacy = heap.alloc_object(ClassId::new(cid + 12), 6);
    assert_eq!(
        heap.try_set_compact_fields(
            legacy,
            &[(0, b'L'), (1, b'I')],
            &[Value::Object(None), Value::Int(1)]
        ),
        None
    );
    let array = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 6);
    assert_eq!(
        heap.try_set_compact_fields(array, &[(0, b'L'), (1, b'L')], &[Value::Object(None); 2]),
        None
    );
}

#[test]
fn generational_compact_mixed_batch_preserves_scalar_semantics() {
    exercise(GcBackend::Generational, 62_741);
}
#[test]
fn g1_compact_mixed_batch_preserves_scalar_semantics() {
    exercise(GcBackend::G1, 62_761);
}
#[cfg(feature = "zgc")]
#[test]
fn zgc_compact_mixed_batch_preserves_scalar_semantics() {
    exercise(GcBackend::Zgc, 62_781);
}
