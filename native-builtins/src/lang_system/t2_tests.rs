use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn t2_thread_sleep_millis_nanos_zero_is_noop() {
    let mut ctx = mock_ctx();
    let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(0)]);
    assert!(r.is_ok());
}

#[test]
fn t2_thread_sleep_rejects_negative_millis() {
    let mut ctx = mock_ctx();
    let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(-1), Value::Int(0)]);
    assert!(r.is_err(), "negative millis must throw");
}

#[test]
fn t2_thread_sleep_rejects_negative_nanos() {
    let mut ctx = mock_ctx();
    let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(-1)]);
    assert!(r.is_err(), "negative nanos must throw");
}

#[test]
fn t2_thread_sleep_rejects_oversized_nanos() {
    let mut ctx = mock_ctx();
    let r =
        native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(1_000_000)]);
    assert!(r.is_err(), "nanos >= 1_000_000 must throw");
}

#[test]
fn t2_thread_sleep_accepts_max_nanos() {
    let mut ctx = mock_ctx();
    let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(999_999)]);
    assert!(r.is_ok(), "nanos = 999_999 must be accepted");
}

/// i6-L2: `Thread.getStackTrace0` / `dumpThreads` arrays are allocated
/// with the `StackTraceElement` component once that class is loaded, so
/// `Thread.asyncGetStackTrace`'s `(StackTraceElement[])` cast holds under
/// the strict array-cast rule; `Object` only before it is loaded.
#[test]
fn i6_l2_stack_trace_arrays_take_the_stack_trace_element_component() {
    let mut ctx = mock_ctx();
    assert_eq!(stack_trace_element_component_id(&mut ctx).as_u32(), 0);
    let ste = ctx
        .ensure_class_initialized("java/lang/StackTraceElement")
        .expect("the mock loads any name");
    assert_eq!(stack_trace_element_component_id(&mut ctx), ste);
}

/// i7-L2: `dumpThreads`' outer array takes `StackTraceElement[]` as its
/// component once the array class exists; `Object` when the context can
/// neither find nor load it (the mock's `load_class` answers nothing).
#[test]
fn i7_l2_dump_threads_outer_array_takes_the_array_class_component() {
    let mut ctx = mock_ctx();
    assert_eq!(stack_trace_element_array_class_id(&mut ctx).as_u32(), 0);
    let ste_array = ctx
        .ensure_class_initialized("[Ljava/lang/StackTraceElement;")
        .expect("the mock loads any name");
    assert_eq!(stack_trace_element_array_class_id(&mut ctx), ste_array);
}
