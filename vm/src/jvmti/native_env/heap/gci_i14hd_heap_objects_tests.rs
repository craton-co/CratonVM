use super::*;

/// gci i14/hd item 8: the JVMTI walk lists no humongous filler, no TLAB
/// `int[]` filler and no instance / reference array whose class the store
/// does not hold; a primitive array and a known class's objects stay.
#[test]
fn gci_i14hd_jvmti_walk_lists_only_classed_objects() {
    let known: HashSet<u32> = [7u32].into_iter().collect();
    let filler = cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID.as_u32();
    assert!(heap_object_listed(
        ObjectKind::Object,
        7,
        ArrayElementType::Reference,
        &known
    ));
    assert!(!heap_object_listed(
        ObjectKind::Object,
        8,
        ArrayElementType::Reference,
        &known
    ));
    assert!(!heap_object_listed(
        ObjectKind::HumongousFiller,
        7,
        ArrayElementType::Int,
        &known
    ));
    assert!(!heap_object_listed(
        ObjectKind::Array,
        filler,
        ArrayElementType::Int,
        &known
    ));
    assert!(heap_object_listed(
        ObjectKind::Array,
        8,
        ArrayElementType::Int,
        &known
    ));
    assert!(heap_object_listed(
        ObjectKind::Array,
        7,
        ArrayElementType::Reference,
        &known
    ));
    assert!(!heap_object_listed(
        ObjectKind::Array,
        8,
        ArrayElementType::Reference,
        &known
    ));
}
