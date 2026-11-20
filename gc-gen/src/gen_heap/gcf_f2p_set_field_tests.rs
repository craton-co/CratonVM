use super::*;

/// A `(class_id, field_count)` pair no other gc test allocates with. The
/// registration is monotone process state (see `heap.rs`'s
/// `with_compact_oop_scan_agrees_with_compact_oop_scan`).
const CID: u32 = 47_271;

fn register(heap: &GenerationalHeap) {
    cratonvm_types::register_class_layout(
        heap.layout_domain(),
        CID,
        std::sync::Arc::new(CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![false, true],
            field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
            ref_disps: vec![16],
            total_size: 24,
        }),
    );
}

/// A compact receiver: both field kinds store and read back through the
/// one-resolution arm, and an index past the layout is dropped (the full
/// path's out-of-bounds guard) without touching either field.
#[test]
fn gcf_f2p_compact_set_field_stores_both_kinds_and_drops_past_the_layout() {
    let heap = GenerationalHeap::with_sizes(1 << 20, 1 << 20);
    register(&heap);
    let obj = heap.alloc_object(ClassId::new(CID), 2);
    if !is_compact_object(heap.get_header(obj)) {
        assert!(
            !compact_ref_fields_enabled(),
            "a registered layout must allocate compact"
        );
        return;
    }
    let child = heap.alloc_object(ClassId::new(CID + 1), 1);
    heap.set_field(obj, 0, Value::Int(41));
    heap.set_field(obj, 1, Value::Object(Some(child)));
    assert_eq!(heap.get_field(obj, 0).as_int(), Some(41));
    assert_eq!(heap.get_field(obj, 1), Value::Object(Some(child)));
    heap.set_field(obj, 2, Value::Int(9));
    assert_eq!(
        heap.get_field(obj, 0).as_int(),
        Some(41),
        "an index past the layout is dropped"
    );
    assert_eq!(heap.get_field(obj, 1), Value::Object(Some(child)));
    heap.set_field(obj, 1, Value::Object(None));
    assert_eq!(heap.get_field(obj, 1), Value::Object(None));
}

/// The JIT helper's door stores what `set_field` stores on a compact
/// receiver, and on a LEGACY receiver (whose `off` would be meaningless)
/// it is `set_field` itself.
#[test]
fn gcf_f2p_the_resolved_ref_door_matches_set_field() {
    let heap = GenerationalHeap::with_sizes(1 << 20, 1 << 20);
    register(&heap);
    let obj = heap.alloc_object(ClassId::new(CID), 2);
    let a = heap.alloc_object(ClassId::new(CID + 1), 1);
    let b = heap.alloc_object(ClassId::new(CID + 1), 1);
    if is_compact_object(heap.get_header(obj)) {
        let (off, storage) = compact_field_slot(heap.get_header(obj), 1).unwrap();
        assert!(storage.is_reference());
        heap.set_resolved_compact_ref_field(obj, 1, off, Value::Object(Some(a)));
        assert_eq!(heap.get_field(obj, 1), Value::Object(Some(a)));
        heap.set_field(obj, 1, Value::Object(Some(b)));
        assert_eq!(heap.get_field(obj, 1), Value::Object(Some(b)));
    }
    // An unregistered class allocates legacy: the door falls back.
    let legacy = heap.alloc_object(ClassId::new(CID + 2), 2);
    assert!(!is_compact_object(heap.get_header(legacy)));
    heap.set_resolved_compact_ref_field(legacy, 1, 16, Value::Object(Some(a)));
    assert_eq!(heap.get_field(legacy, 1), Value::Object(Some(a)));
}

#[test]
fn compact_object_start_and_field_single_lookup_round_trip() {
    let heap = GenerationalHeap::with_sizes(1 << 20, 1 << 20);
    register(&heap);
    cratonvm_types::register_class_layout(
        heap.layout_domain(),
        CID + 10,
        std::sync::Arc::new(CompactLayout {
            field_disps: vec![8],
            is_ref: vec![false],
            field_kinds: vec![FieldStorageKind::Long],
            ref_disps: vec![],
            total_size: 16,
        }),
    );
    let obj = heap.alloc_object(ClassId::new(CID), 2);
    let obj_long = heap.alloc_object(ClassId::new(CID + 10), 1);
    let addr = obj.as_ptr() as usize;
    assert_eq!(heap.is_object_address(addr), Some(obj));
    assert_eq!(heap.is_object_address(addr + 4), None);
    if is_compact_object(heap.get_header(obj)) {
        heap.set_field(obj, 0, Value::Int(0x1234_5678));
        assert_eq!(heap.get_field(obj, 0).as_int(), Some(0x1234_5678));
        assert_eq!(heap.get_field(obj, 5), Value::Object(None));
    }
    if is_compact_object(heap.get_header(obj_long)) {
        heap.set_field(obj_long, 0, Value::Long(-1));
        assert_eq!(heap.get_field(obj_long, 0).as_long(), Some(-1));
        assert_eq!(
            heap.is_object_address(obj_long.as_ptr() as usize),
            Some(obj_long)
        );
        assert_eq!(heap.is_object_address(obj_long.as_ptr() as usize + 8), None);
    }
}
