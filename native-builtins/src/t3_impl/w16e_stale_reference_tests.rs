use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::MethodCallResult;

thread_local! {
    static READ_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// `available()` answers 3; the first `read(byte[])` MOVES the buffer
/// (fills a fresh array and re-points the native's pin at it) and answers
/// 3; the second answers EOF.
fn move_buffer_on_read(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "available" => Some(Ok(Some(Value::Int(3)))),
        "read" => {
            let n = READ_CALLS.with(|c| {
                c.set(c.get() + 1);
                c.get()
            });
            if n > 1 {
                return Some(Ok(Some(Value::Int(-1))));
            }
            let Some(Value::Object(Some(old))) = args.first().copied() else {
                return Some(Ok(Some(Value::Int(-1))));
            };
            let len = ctx.array_length(old);
            let moved = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
            ctx.write_byte_array_from(moved, 0, b"abc");
            ctx.remap_native_pin_addr_for_test(old.as_ptr() as usize, moved.as_ptr() as usize);
            Some(Ok(Some(Value::Int(3))))
        }
        _ => None,
    }
}

/// The bytes a `read` wrote are copied out of the buffer's CURRENT copy.
/// The old loop read them back through the address taken before the
/// call, i.e. out of the vacated copy (here: zeros).
#[test]
fn stax_input_is_read_back_from_the_buffer_the_read_left() {
    READ_CALLS.with(|c| c.set(0));
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xE16_0201);
    ctx.set_invoke_virtual_hook(move_buffer_on_read);
    let stream = ctx.fresh_object_ref();
    let text = stax_read_input_stream(&mut ctx, stream);
    assert_eq!(text, "abc");
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// An empty binding store grows to hold the appended pair. The grow path
/// doubled the capacity, which for an empty array is 0, and then stored
/// the pair past its end.
#[test]
fn an_empty_jndi_binding_store_grows_to_hold_the_first_binding() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xE16_0202);
    let cid = cratonvm_types::ClassId::new(0);
    let this = ctx.alloc_object(cid, 1);
    let bindings = ctx.alloc_object(cid, 3);
    let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let vals = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    ctx.set_field(bindings, 0, Value::Object(Some(keys)));
    ctx.set_field(bindings, 1, Value::Object(Some(vals)));
    ctx.set_field(bindings, 2, Value::Int(0));
    ctx.set_field(this, 0, Value::Object(Some(bindings)));
    let name = ctx.create_string("java:comp/env");
    let value = ctx.fresh_object_ref();

    jndi_put_binding(
        &mut ctx,
        this,
        Value::Object(Some(name)),
        Value::Object(Some(value)),
    )
    .expect("the put must succeed");

    assert_eq!(ctx.get_field(bindings, 2), Value::Int(1));
    let Value::Object(Some(keys)) = ctx.get_field(bindings, 0) else {
        panic!("the grown key array must be published");
    };
    let Value::Object(Some(vals)) = ctx.get_field(bindings, 1) else {
        panic!("the grown value array must be published");
    };
    assert!(ctx.array_length(keys) >= 1);
    assert_eq!(ctx.get_array_element(keys, 0), Value::Object(Some(name)));
    assert_eq!(ctx.get_array_element(vals, 0), Value::Object(Some(value)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
