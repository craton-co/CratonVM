use super::jit_compact_field_slot;
use cratonvm_types::{
    register_class_layout, unregister_class_layout, ArrayElementType, ClassId, CompactLayout,
    FieldStorageKind, ObjectHeader, ObjectKind, FIRST_LAYOUT_DOMAIN,
};
use std::sync::Arc;

#[test]
fn field_proof_obeys_object_flags_bounds_replacement_and_unload() {
    let cid = 60_001;
    let register = |offset, storage| {
        register_class_layout(
            FIRST_LAYOUT_DOMAIN,
            cid,
            Arc::new(CompactLayout {
                field_disps: vec![offset],
                is_ref: vec![false],
                field_kinds: vec![storage],
                ref_disps: vec![],
                total_size: 32,
            }),
        );
    };
    register(8, FieldStorageKind::Int);
    let mut header = ObjectHeader::new(
        ClassId::new(cid),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        1,
    );
    let raw = (&header as *const ObjectHeader) as i64;
    unsafe {
        assert_eq!(jit_compact_field_slot(raw, 0), None);
    }
    header.set_compact_shape(1, 32);
    for _ in 0..3 {
        unsafe {
            assert_eq!(
                jit_compact_field_slot(raw, 0),
                Some((8, FieldStorageKind::Int))
            );
            assert_eq!(jit_compact_field_slot(raw, -1), None);
            assert_eq!(jit_compact_field_slot(raw, 1), None);
        }
    }
    register(16, FieldStorageKind::Long);
    unsafe {
        assert_eq!(
            jit_compact_field_slot(raw, 0),
            Some((16, FieldStorageKind::Long))
        );
    }
    unregister_class_layout(cid);
    unsafe {
        assert_eq!(jit_compact_field_slot(raw, 0), None);
    }
}
