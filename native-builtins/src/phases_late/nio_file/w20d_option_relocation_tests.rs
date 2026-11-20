//! gc-common w20-d: the option / attribute scanners call Java (`toString`,
//! `name`, `value`) on an element and then read the array, or the same
//! element, again. The mock's `invoke_virtual` hook plays a moving
//! collection there: it re-points the native pins at a second copy, and
//! the scanner must read that copy.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new) address pair the hook moves on its first call.
    static W20D_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// The receiver `value()` was called on.
    static W20D_VALUE_RECEIVER: Cell<usize> = const { Cell::new(0) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn w20d_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    let answer = match method_name {
        "toString" => "READ",
        "name" => "posix:permissions",
        "value" => {
            W20D_VALUE_RECEIVER.with(|c| c.set(addr(receiver)));
            return Some(Ok(None));            }
        _ => return None,
    };
    let (old, new) = W20D_MOVE.with(|m| m.replace((0, 0)));
    if old != 0 {
        ctx.remap_native_pin_addr_for_test(old, new);
    }
    let s = ctx.create_string(answer);
    Some(Ok(Some(Value::Object(Some(s)))))
}

fn reset() {
    W20D_MOVE.with(|m| m.set((0, 0)));
    W20D_VALUE_RECEIVER.with(|c| c.set(0));
}

#[test]
fn w20d_open_option_scan_reads_the_array_that_to_string_moved() {
    reset();
    let mut ctx = MockNativeContext::new();
    // Slot 0 is no String, so the scanner asks `toString()`.
    let unnamed = ctx.fresh_object_ref();
    let append = ctx.fresh_object_ref();
    let append_name = ctx.create_string("APPEND");
    ctx.set_field(append, 0, Value::Object(Some(append_name)));
    // The array before the "collection": its second slot is empty, so a
    // scan that keeps reading it never sees APPEND.
    let old_arr = ctx.new_ref_array(ClassId::new(0), 2);
    ctx.set_array_element(old_arr, 0, Value::Object(Some(unnamed)));
    let new_arr = ctx.new_ref_array(ClassId::new(0), 2);
    ctx.set_array_element(new_arr, 0, Value::Object(Some(unnamed)));
    ctx.set_array_element(new_arr, 1, Value::Object(Some(append)));
    W20D_MOVE.with(|m| m.set((addr(old_arr), addr(new_arr))));
    ctx.set_invoke_virtual_hook(w20d_hook);

    let pins = ctx.native_pin_count_for_test();
    let flags = fsp_scan_open_options(&mut ctx, Some(Value::Object(Some(old_arr))));
    assert!(
        flags.read,
        "the unnamed option answered READ through toString()"
    );
    assert!(
        flags.append,
        "the second option must be read from the array's current address"
    );
    assert_eq!(ctx.native_pin_count_for_test(), pins, "the pin is released");
}

#[test]
fn w20d_posix_attribute_value_is_asked_of_the_attribute_name_moved() {
    reset();
    let mut ctx = MockNativeContext::new();
    let old_attr = ctx.fresh_object_ref();
    let new_attr = ctx.fresh_object_ref();
    let attrs = ctx.new_ref_array(ClassId::new(0), 1);
    ctx.set_array_element(attrs, 0, Value::Object(Some(old_attr)));
    W20D_MOVE.with(|m| m.set((addr(old_attr), addr(new_attr))));
    ctx.set_invoke_virtual_hook(w20d_hook);

    let pins = ctx.native_pin_count_for_test();
    let mode = posix_mode_from_file_attributes(&mut ctx, Some(&Value::Object(Some(attrs))));
    assert_eq!(mode, None, "value() answered null");
    assert_eq!(
        W20D_VALUE_RECEIVER.with(|c| c.get()),
        addr(new_attr),
        "value() must be sent to the address name() moved the attribute to"
    );
    assert_eq!(ctx.native_pin_count_for_test(), pins, "the scope is closed");
}
