use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// -----------------------------------------------------------------------
// T15.1.5 вЂ” Finalizer.register
// -----------------------------------------------------------------------

#[test]
fn finalizer_register_with_object() {
    let mut ctx = mock_ctx();
    let obj = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
    let r = native_finalizer_register(&mut ctx, &[Value::Object(Some(obj))]);
    assert!(r.is_ok());
    assert_eq!(r.unwrap(), None); // void
                                  // gc-common w5-d: the VM's finalizable-registration hook, not the
                                  // wire-3 (`Cleaner`) `discover_reference` it used to call.
    assert_eq!(*ctx.register_if_finalizable_calls.borrow(), vec![obj]);
}

#[test]
fn finalizer_register_with_null() {
    let mut ctx = mock_ctx();
    let r = native_finalizer_register(&mut ctx, &[Value::Object(None)]);
    assert!(r.is_ok()); // null is silently ignored
    assert!(ctx.register_if_finalizable_calls.borrow().is_empty());
}

// -----------------------------------------------------------------------
// T15.1.6 вЂ” Array.newArray
// -----------------------------------------------------------------------

#[test]
fn array_new_array_int() {
    let mut ctx = mock_ctx();
    let mirror = ctx.create_string("int");
    let r = native_array_new_array(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(5)]);
    let arr = match r.unwrap() {
        Some(Value::Object(Some(a))) => a,
        other => panic!("expected array, got {other:?}"),
    };
    assert_eq!(ctx.array_length(arr), 5);
}

#[test]
fn array_new_array_negative_size() {
    let mut ctx = mock_ctx();
    let mirror = ctx.create_string("int");
    let r = native_array_new_array(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(-1)]);
    assert!(
        r.is_err(),
        "negative size should throw NegativeArraySizeException"
    );
}

// -----------------------------------------------------------------------
// T15.1.3 вЂ” ClassLoader.defineClass0/1/2
// -----------------------------------------------------------------------

#[test]
fn define_class1_empty_bytes_throws() {
    let mut ctx = mock_ctx();
    // Create a byte array with non-CAFEBABE bytes
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    ctx.set_array_element(arr, 0, Value::Int(0));
    ctx.set_array_element(arr, 1, Value::Int(0));
    ctx.set_array_element(arr, 2, Value::Int(0));
    ctx.set_array_element(arr, 3, Value::Int(0));
    let name = ctx.create_string("com/example/Foo");
    let r = native_classloader_define_class1(
        &mut ctx,
        &[
            Value::Object(None), // loader
            Value::Object(Some(name)),
            Value::Object(Some(arr)),
            Value::Int(0),       // offset
            Value::Int(4),       // length
            Value::Object(None), // pd
            Value::Object(None), // source
        ],
    );
    assert!(
        r.is_err(),
        "invalid class bytes must throw, not return null"
    );
}

#[test]
fn define_class2_reads_bytebuffer_backing_array() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    for i in 0..4 {
        ctx.set_array_element(arr, i, Value::Int(0));
    }
    let bb = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(bb, 0, Value::Object(Some(arr)));
    ctx.set_field(bb, 1, Value::Int(0));
    ctx.set_field(bb, 2, Value::Int(4));
    ctx.set_field(bb, 3, Value::Int(4));
    let name = ctx.create_string("com/example/Foo");
    let r = native_classloader_define_class2(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(name)),
            Value::Object(Some(bb)),
            Value::Int(0),
            Value::Int(4),
            Value::Object(None),
            Value::Object(None),
        ],
    );
    match r {
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Linkage(
                cratonvm_types::error::LinkageError::ClassFormatError { message, .. },
            ),
        )) => assert!(
            message.contains("defineClass2"),
            "expected defineClass2 class-format failure, got {message}"
        ),
        other => panic!("expected ClassFormatError from ByteBuffer handler, got {other:?}"),
    }
}

#[test]
fn define_class0_empty_bytes_throws() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let name = ctx.create_string("com/example/Foo");
    let r = native_classloader_define_class0(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Object(Some(name)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(4),
            Value::Object(None),
            Value::Int(0),
            Value::Int(0),
            Value::Object(None),
        ],
    );
    assert!(
        r.is_err(),
        "invalid class bytes must throw, not return null"
    );
}

#[test]
fn define_class1_out_of_bounds() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 2);
    let name = ctx.create_string("com/example/Foo");
    let r = native_classloader_define_class1(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(name)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(10), // length > array size
            Value::Object(None),
            Value::Object(None),
        ],
    );
    assert!(r.is_err(), "should throw ArrayIndexOutOfBoundsException");
}
