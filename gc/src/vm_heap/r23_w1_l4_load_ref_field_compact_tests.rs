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
    // Wide references only: the narrow width is a process-wide switch
    // tests must not flip (`narrow_oop::disable_for_test`).
    assert!(!cratonvm_types::narrow_oop::narrow_oops_enabled());
    cratonvm_types::register_class_layout(
        domain,
        cid,
        Arc::new(CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![false, true],
            field_kinds: vec![FieldStorageKind::Int, FieldStorageKind::Reference],
            ref_disps: vec![16],
            total_size: 24,
        }),
    );
    let obj = heap.alloc_object(ClassId::new(cid), 2);
    assert!(cratonvm_types::is_compact_object(heap.get_header(obj)));
    let child = heap.alloc_object(ClassId::new(cid + 1), 0);
    // SAFETY (every call below): live objects of this heap, no collection.
    unsafe {
        assert_eq!(heap.load_ref_field_compact(obj, 1), Some(0), "null");
        heap.set_field(obj, 1, Value::Object(Some(child)));
        assert_eq!(
            heap.load_ref_field_compact(obj, 1),
            Some(child.as_ptr() as u64)
        );
        assert_eq!(heap.load_ref_field_compact(obj, 0), None, "an int row");
        assert_eq!(heap.load_ref_field_compact(obj, 2), None, "no such slot");
        let legacy = heap.alloc_object(ClassId::new(cid + 2), 2);
        assert_eq!(heap.load_ref_field_compact(legacy, 1), None, "legacy body");
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
        assert_eq!(heap.load_ref_field_compact(arr, 1), None, "array");
    }
    assert_eq!(heap.get_field(obj, 1), Value::Object(Some(child)));
}

#[test]
fn r23_w1_l4_load_ref_field_compact_matches_get_field() {
    exercise(GcBackend::Generational, 0x5_2301);
    exercise(GcBackend::G1, 0x5_2311);
    #[cfg(feature = "zgc")]
    exercise(GcBackend::Zgc, 0x5_2321);
}
