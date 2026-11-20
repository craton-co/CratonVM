use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn object_ref(value: Value) -> ObjectRef {
    match value {
        Value::Object(Some(obj)) => obj,
        other => panic!("expected object, got {other:?}"),
    }
}

#[test]
fn memory_layout_varhandle_resolves_named_struct_member_width() {
    let mut reg = NativeMethodRegistry::new();
    register_p67_foreign_memory(&mut reg);
    let mut ctx = mock_ctx();

    let ptr_name = ctx.create_string("ptr");
    let ptr_layout =
        p67_layout_object(&mut ctx, "java/lang/foreign/AddressLayout", 8, 8).unwrap();
    ctx.set_field(ptr_layout, 3, Value::Object(Some(ptr_name)));

    let size_name = ctx.create_string("size");
    let size_layout =
        p67_layout_object(&mut ctx, "java/lang/foreign/ValueLayout$OfLong", 8, 8).unwrap();
    ctx.set_field(size_layout, 3, Value::Object(Some(size_name)));

    let members = ctx.new_array(ArrayElementType::Reference, 2);
    ctx.set_array_element(members, 0, Value::Object(Some(ptr_layout)));
    ctx.set_array_element(members, 1, Value::Object(Some(size_layout)));

    let struct_layout = object_ref(
        reg.find(
            "java/lang/foreign/MemoryLayout",
            "structLayout",
            "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        )
        .expect("MemoryLayout.structLayout registered")(
            &mut ctx,
            &[Value::Object(Some(members))],
        )
        .expect("structLayout ok")
        .expect("structLayout returned value"),
    );

    let path_name = ctx.create_string("size");
    let path_elem = try_alloc_concurrent_synthetic(
        &mut ctx,
        "java/lang/foreign/MemoryLayout$PathElement",
        2,
    )
    .unwrap();
    ctx.set_field(path_elem, 0, Value::Object(Some(path_name)));
    ctx.set_field(path_elem, 1, Value::Int(0));
    let path = ctx.new_array(ArrayElementType::Reference, 1);
    ctx.set_array_element(path, 0, Value::Object(Some(path_elem)));

    let vh = object_ref(
        reg.find(
            "java/lang/foreign/MemoryLayout",
            "varHandle",
            "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
        )
        .expect("MemoryLayout.varHandle registered")(
            &mut ctx,
            &[
                Value::Object(Some(struct_layout)),
                Value::Object(Some(path)),
            ],
        )
        .expect("varHandle ok")
        .expect("varHandle returned value"),
    );

    assert_eq!(ctx.get_field(vh, VH_FIELD_INDEX), Value::Int(8));
    assert_eq!(
        ctx.get_field(vh, VH_IS_STATIC),
        Value::Int(VH_KIND_MEMORY_SEGMENT)
    );
}
