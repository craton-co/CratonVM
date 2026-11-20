use super::*;
use crate::test_utils::{mock_define_full_panics, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

/// `public class <name> extends java/lang/Object {}` (version 52).
fn class_bytes(name: &str) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&5u16.to_be_bytes());
    for (index, text) in [(2u16, name), (4u16, "java/lang/Object")] {
        b.push(7);
        b.extend_from_slice(&index.to_be_bytes());
        b.push(1);
        b.extend_from_slice(&(text.len() as u16).to_be_bytes());
        b.extend_from_slice(text.as_bytes());
    }
    b.extend_from_slice(&0x0021u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    b
}

fn panicking<T>(f: impl FnOnce() -> T) -> T {
    mock_define_full_panics(true);
    let out = f();
    mock_define_full_panics(false);
    out
}

#[test]
fn zs9nd8_cl_define_class_basic_panic_arm_releases_its_pins() {
    let mut ctx = MockNativeContext::new();
    let cl = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let this = ctx.alloc_object(cl, 1);
    let bytes = class_bytes("zs9nd8/PanicA");
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    let before = ctx.native_pin_count_for_test();
    let out = panicking(|| {
        cl_define_class_basic(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(bytes.len() as i32),
            ],
        )
    });
    assert!(
        out.is_err(),
        "the panic is the define's ClassFormatError: {out:?}"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        before,
        "the panic arm left a pin"
    );
}

#[test]
fn zs9nd8_define_class_via_full_panic_arm_releases_its_pins() {
    let mut ctx = MockNativeContext::new();
    let cl = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let loader = ctx.alloc_object(cl, 1);
    let data = ctx.alloc_object(cl, 1);
    let before = ctx.native_pin_count_for_test();
    let out = panicking(|| {
        define_class_via_full(
            &mut ctx,
            "zs9nd8/PanicB",
            class_bytes("zs9nd8/PanicB"),
            0,
            cratonvm_native_api::DefineClassFull::default(),
            false,
            Some(Value::Object(Some(data))),
            Value::Object(Some(loader)),
        )
    });
    assert!(
        out.is_err(),
        "the panic is the define's ClassFormatError: {out:?}"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        before,
        "the panic arm left the class-data / loader pins"
    );
}
