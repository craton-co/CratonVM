use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

#[test]
fn bytebuffer_allocate_initializes_real_address_for_bulk_copy() {
    let mut ctx = MockNativeContext::new();
    let result = native_heap_bytebuffer_allocate(&mut ctx, &[Value::Int(8)])
        .expect("ByteBuffer.allocate native succeeds")
        .expect("ByteBuffer.allocate returns object");
    let buffer = match result {
        Value::Object(Some(buffer)) => buffer,
        other => panic!("expected ByteBuffer object, got {other:?}"),
    };

    assert_eq!(ctx.get_field_by_name(buffer, "position"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(buffer, "limit"), Value::Int(8));
    assert_eq!(ctx.get_field_by_name(buffer, "capacity"), Value::Int(8));
    assert_eq!(ctx.get_field_by_name(buffer, "mark"), Value::Int(-1));
    assert_eq!(ctx.get_field_by_name(buffer, "offset"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(buffer, "address"), Value::Long(16));
    let hb = match ctx.get_field_by_name(buffer, "hb") {
        Value::Object(Some(hb)) => hb,
        other => panic!("expected hb byte array, got {other:?}"),
    };
    assert_eq!(ctx.array_length(hb), 8);
}

#[test]
fn heap_byte_buffer_memorysegment_constructors_initialize_real_fields() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    for descriptor in [
        "([BIILjava/lang/foreign/MemorySegment;)V",
        "([BIILjdk/internal/access/foreign/MemorySegmentProxy;)V",
    ] {
        assert!(
            registry
                .find("java/nio/HeapByteBuffer", "<init>", descriptor)
                .is_some(),
            "HeapByteBuffer constructor {descriptor} must be native in real-JDK mode"
        );
    }

    let init = registry
        .find(
            "java/nio/HeapByteBuffer",
            "<init>",
            "([BIILjdk/internal/access/foreign/MemorySegmentProxy;)V",
        )
        .expect("JDK17 HeapByteBuffer(byte[],int,int,segment) native");

    let mut ctx = MockNativeContext::new();
    let buffer_class = ctx
        .ensure_class_initialized("java/nio/HeapByteBuffer")
        .expect("mock HeapByteBuffer class");
    let buffer = ctx.alloc_object(buffer_class, 8);
    let bytes = ctx.new_array(ArrayElementType::Byte, 6);

    init(
        &mut ctx,
        &[
            Value::Object(Some(buffer)),
            Value::Object(Some(bytes)),
            Value::Int(2),
            Value::Int(3),
            Value::Object(None),
        ],
    )
    .expect("constructor native succeeds");

    assert_eq!(ctx.get_field_by_name(buffer, "mark"), Value::Int(-1));
    assert_eq!(ctx.get_field_by_name(buffer, "position"), Value::Int(2));
    assert_eq!(ctx.get_field_by_name(buffer, "limit"), Value::Int(5));
    assert_eq!(ctx.get_field_by_name(buffer, "capacity"), Value::Int(6));
    assert_eq!(
        ctx.get_field_by_name(buffer, "hb"),
        Value::Object(Some(bytes))
    );
    assert_eq!(ctx.get_field_by_name(buffer, "offset"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(buffer, "address"), Value::Long(16));
}
