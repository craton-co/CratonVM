//! gc-common w19-f: the STOMP encoder asks the message for its payload
//! through the message's CURRENT address. The hook stands in for a moving
//! collection inside `getHeaders()` (Java): it re-addresses the mock's
//! native pins from the old message to the new one.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old message, new message, headers map, command enum)
    static W19F_OBJS: Cell<[usize; 4]> = const { Cell::new([0; 4]) };
    static W19F_PAYLOAD_RECEIVER: Cell<usize> = const { Cell::new(0) };
}

fn obj(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn w19f_message_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    let [old, new, headers, command] = W19F_OBJS.with(|o| o.get());
    match method_name {
        "getHeaders" => {
            ctx.remap_native_pin_addr_for_test(old, new);
            Some(Ok(Some(Value::Object(Some(obj(headers))))))
        }
        "get" => {
            let key = match args.first() {
                Some(Value::Object(Some(k))) => ctx.read_string(*k).unwrap_or_default(),
                _ => String::new(),
            };
            if key == "stompCommand" {
                Some(Ok(Some(Value::Object(Some(obj(command))))))
            } else {
                Some(Ok(Some(Value::Object(None))))
            }
        }
        "name" => {
            let s = ctx.create_string("SEND");
            Some(Ok(Some(Value::Object(Some(s)))))
        }
        "getPayload" => {
            W19F_PAYLOAD_RECEIVER.with(|r| r.set(receiver.as_ptr() as usize));
            let s = ctx.create_string("hi");
            Some(Ok(Some(Value::Object(Some(s)))))
        }
        _ => None,
    }
}

#[test]
fn w19f_encode_reads_the_payload_from_the_moved_message() {
    let mut ctx = mock_ctx();
    let encoder = ctx.fresh_object_ref();
    let old = ctx.fresh_object_ref();
    let new = ctx.fresh_object_ref();
    let headers = ctx.fresh_object_ref();
    let command = ctx.fresh_object_ref();
    let addr = |o: ObjectRef| o.as_ptr() as usize;
    W19F_OBJS.with(|o| o.set([addr(old), addr(new), addr(headers), addr(command)]));
    W19F_PAYLOAD_RECEIVER.with(|r| r.set(0));
    ctx.set_invoke_virtual_hook(w19f_message_hook);
    let pins_before = ctx.native_pin_count_for_test();

    let out = native_spring_stomp_encoder_encode(
        &mut ctx,
        &[Value::Object(Some(encoder)), Value::Object(Some(old))],
    )
    .unwrap();

    assert_eq!(
        W19F_PAYLOAD_RECEIVER.with(|r| r.get()),
        addr(new),
        "getPayload must be asked of the message's current copy"
    );
    let Some(Value::Object(Some(frame))) = out else {
        panic!("expected a frame array, got {out:?}");
    };
    let bytes: Vec<u8> = (0..ctx.array_length(frame))
        .map(|i| ctx.get_array_element(frame, i).as_int().unwrap_or(0) as u8)
        .collect();
    assert!(bytes.starts_with(b"SEND\n"), "frame: {bytes:?}");
    assert!(bytes.ends_with(b"\nhi\0"), "frame: {bytes:?}");
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "no pin left behind"
    );
}
