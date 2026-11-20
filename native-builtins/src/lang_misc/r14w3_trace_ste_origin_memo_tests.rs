use super::{SteOriginMemo, STE_ORIGIN_FIELDS};
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeContext, NativeHandleScope, NativeHeapAccess,
};
use cratonvm_types::{ClassId, Value};

/// The test mock drops a by-name store to a field its class does not
/// declare and answers `Int(0)` for the read, so the element class is
/// declared with the four origin fields at slots 0..4.
fn declare_ste(ctx: &mut crate::test_utils::MockNativeContext) -> ClassId {
    let ste = ctx
        .ensure_class_initialized("java/lang/StackTraceElement")
        .unwrap_or_else(|e| panic!("no StackTraceElement: {e:?}"));
    let fields = STE_ORIGIN_FIELDS
        .iter()
        .enumerate()
        .map(|(slot, name)| FieldMetadata {
            name: (*name).to_string(),
            descriptor: if *name == "format" {
                "B"
            } else {
                "Ljava/lang/String;"
            }
            .to_string(),
            access_flags: 0x0002,
            slot_index: slot,
            declaring_class_id: ste,
            is_static: false,
        })
        .collect();
    ctx.set_declared_fields(ste, fields);
    ste
}

#[test]
fn a_later_frame_of_a_class_copies_the_earlier_elements_origin() {
    let mut ctx = mock_ctx();
    let ste_class = declare_ste(&mut ctx);
    let mut scope = NativeHandleScope::new(&mut ctx);
    let arr = scope.new_ref_array(ste_class, 2);
    let arr_h = scope.root(arr);
    let module = scope.alloc_object(ClassId::new(2), 0);
    let version = scope.alloc_object(ClassId::new(2), 0);
    let loader = scope.alloc_object(ClassId::new(2), 0);
    let first = scope.alloc_object(ste_class, STE_ORIGIN_FIELDS.len());
    // Guard against a vacuous test: the stores below must be readable.
    scope.set_field_by_name(first, "format", Value::Int(3));
    assert_eq!(scope.get_field_by_name(first, "format"), Value::Int(3));
    scope.set_field_by_name(first, "moduleName", Value::Object(Some(module)));
    scope.set_field_by_name(first, "moduleVersion", Value::Object(Some(version)));
    scope.set_field_by_name(first, "classLoaderName", Value::Object(Some(loader)));
    scope.set_field_by_name(first, "format", Value::Int(3));
    scope.set_array_element(arr, 0, Value::Object(Some(first)));
    let frame_class = ClassId::new(42);
    let mut memo = SteOriginMemo::new(&arr_h, true);
    memo.at(0);
    memo.remember(frame_class);
    let second = scope.alloc_object(ste_class, STE_ORIGIN_FIELDS.len());
    let second_h = scope.root(second);
    memo.at(1);
    // Another class: nothing to copy.
    assert!(!memo.copy_into(&scope, &second_h, ClassId::new(43)));
    assert_ne!(scope.get_field_by_name(second, "format"), Value::Int(3));
    assert_ne!(
        scope.get_field_by_name(second, "moduleName"),
        Value::Object(Some(module))
    );
    assert!(memo.copy_into(&scope, &second_h, frame_class));
    for field in STE_ORIGIN_FIELDS {
        assert_eq!(
            scope.get_field_by_name(second, field),
            scope.get_field_by_name(first, field),
            "{field}"
        );
    }
    // The first row for a class stays its source.
    memo.remember(frame_class);
    assert_eq!(memo.rows.get(&frame_class), Some(&0));
}

#[test]
fn a_disabled_memo_records_and_copies_nothing() {
    let mut ctx = mock_ctx();
    let mut scope = NativeHandleScope::new(&mut ctx);
    let arr = scope.new_ref_array(ClassId::new(1), 1);
    let arr_h = scope.root(arr);
    let mut memo = SteOriginMemo::new(&arr_h, false);
    memo.remember(ClassId::new(42));
    assert!(memo.rows.is_empty());
    let ste = scope.alloc_object(ClassId::new(1), 8);
    let ste_h = scope.root(ste);
    assert!(!memo.copy_into(&scope, &ste_h, ClassId::new(42)));
}
