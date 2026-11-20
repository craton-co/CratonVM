use super::*;

/// Interpreter round i1 wave 51, lane L7: the one-call header read
/// answers what the two `_validated` twins answer for an object start on
/// every backend, and nothing for an address the validator refuses.
#[test]
fn i51_l7_one_header_read_agrees_with_the_twins() {
    const MB: usize = 1024 * 1024;
    #[allow(unused_mut)]
    let mut heaps = vec![
        VmHeap::new(GcBackend::Generational, 16 * MB),
        VmHeap::new(GcBackend::G1, 64 * MB),
    ];
    #[cfg(feature = "zgc")]
    heaps.push(VmHeap::new(GcBackend::Zgc, 16 * MB));
    for heap in &heaps {
        let obj = heap.alloc_object(ClassId::new(7), 2);
        let arr = heap.alloc_array(ClassId::new(9), ArrayElementType::Long, 3);
        for o in [obj, arr] {
            let h = heap
                .header_if_object_start(o.as_ptr() as usize)
                .expect("an object start");
            assert_eq!(h.kind(), heap.kind_of_validated(o));
            assert_eq!(h.class_id, heap.class_id_of_validated(o));
            if h.kind() == ObjectKind::Array {
                assert_eq!(h.element_type(), heap.element_type_of_validated(o));
            }
        }
        // Exactly the validator's answer elsewhere (an interior word,
        // null, a misaligned address).
        for addr in [obj.as_ptr() as usize + 8, arr.as_ptr() as usize + 4, 0] {
            assert_eq!(
                heap.header_if_object_start(addr).is_some(),
                heap.is_object_start(addr),
                "addr {addr:#x}"
            );
        }
        assert!(heap.header_if_object_start(0).is_none());
    }
}
