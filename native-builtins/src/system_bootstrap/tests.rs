use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn t14_platform_properties_returns_correct_length() {
    let mut ctx = MockNativeContext::new();
    let result = native_platform_properties(&mut ctx, &[]).unwrap();
    let arr = match result {
        Some(Value::Object(Some(arr))) => arr,
        other => panic!("expected Object(Some(_)), got {:?}", other),
    };
    let len = ctx.array_length(arr);
    assert_eq!(
        len, FIXED_LENGTH,
        "platformProperties must return {FIXED_LENGTH}-element array"
    );
}

#[test]
fn t14_platform_properties_has_os_name() {
    let mut ctx = MockNativeContext::new();
    let result = native_platform_properties(&mut ctx, &[]).unwrap();
    let arr = match result {
        Some(Value::Object(Some(arr))) => arr,
        other => panic!("expected Object(Some(_)), got {:?}", other),
    };
    // OS_NAME_NDX = 21
    let val = ctx.get_array_element(arr, OS_NAME_NDX);
    match val {
        Value::Object(Some(_)) => {} // has a string
        other => panic!(
            "os.name at index {OS_NAME_NDX} should be a string, got {:?}",
            other
        ),
    }
}

#[test]
fn t14_platform_properties_file_separator() {
    let mut ctx = MockNativeContext::new();
    let result = native_platform_properties(&mut ctx, &[]).unwrap();
    let arr = match result {
        Some(Value::Object(Some(arr))) => arr,
        other => panic!("expected Object(Some(_)), got {:?}", other),
    };
    // FILE_SEPARATOR_NDX = 4 — must be a non-null string
    let val = ctx.get_array_element(arr, FILE_SEPARATOR_NDX);
    match val {
        Value::Object(Some(_)) => {} // has a string
        other => panic!(
            "file.separator at index {FILE_SEPARATOR_NDX} should be a string, got {:?}",
            other
        ),
    }
}

#[test]
fn t14_vm_properties_returns_pairs() {
    let mut ctx = MockNativeContext::new();
    let result = native_vm_properties(&mut ctx, &[]).unwrap();
    let arr = match result {
        Some(Value::Object(Some(arr))) => arr,
        other => panic!("expected Object(Some(_)), got {:?}", other),
    };
    let len = ctx.array_length(arr);
    assert!(
        len >= 2,
        "vmProperties must return at least one key-value pair"
    );
    assert_eq!(
        len % 2,
        0,
        "vmProperties must return even-length array (key-value pairs)"
    );
}

/// i7-L2: both `SystemProps$Raw` natives answer `String[]`, their declared
/// type, and `vmProperties` keeps every key and value in order.
#[test]
fn i7_l2_system_props_arrays_are_string_typed() {
    let mut ctx = MockNativeContext::new();
    let string = ctx
        .ensure_class_initialized("java/lang/String")
        .expect("mock registers the class");
    let platform = match native_platform_properties(&mut ctx, &[]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("expected an array, got {other:?}"),
    };
    let vm = match native_vm_properties(&mut ctx, &[]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("expected an array, got {other:?}"),
    };
    assert_eq!(ctx.ref_array_component(platform), Some(string));
    assert_eq!(ctx.ref_array_component(vm), Some(string));
    let first_key = match ctx.get_array_element(vm, 0) {
        Value::Object(Some(s)) => ctx.read_string(s),
        other => panic!("vmProperties[0] must be a key, got {other:?}"),
    };
    let first_value = match ctx.get_array_element(vm, 1) {
        Value::Object(Some(s)) => ctx.read_string(s),
        other => panic!("vmProperties[1] must be a value, got {other:?}"),
    };
    assert!(first_key.is_some() && first_value.is_some());
    for i in 0..ctx.array_length(vm) {
        assert!(
            matches!(ctx.get_array_element(vm, i), Value::Object(Some(_))),
            "vmProperties slot {i} is null"
        );
    }
}

#[test]
fn t14_win32_error_mode_returns_zero() {
    let mut ctx = MockNativeContext::new();
    let result = native_win32_set_error_mode(&mut ctx, &[Value::Long(0x8001)]).unwrap();
    assert_eq!(result, Some(Value::Long(0)));
}
