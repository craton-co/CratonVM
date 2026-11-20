//! gc-common w20-d: `ds_socket_option_name` and `huc_url_string` run Java
//! (`SocketOption.name()`, `URL.toString()`) and now hand the caller's
//! receiver back CURRENT through `&mut`. The mock's `invoke_virtual` hook
//! plays a moving collection inside that call by re-pointing the native
//! pins at a second object.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new) address pair the hook moves.
    static W20D_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn w20d_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    let answer = match method_name {
        "name" => "SO_REUSEADDR",
        "toString" => "http://w20d.test/x",
        _ => return None,
    };
    let (old, new) = W20D_MOVE.with(|m| m.replace((0, 0)));
    if old != 0 {
        ctx.remap_native_pin_addr_for_test(old, new);
    }
    let s = ctx.create_string(answer);
    Some(Ok(Some(Value::Object(Some(s)))))
}

#[test]
fn w20d_socket_option_name_hands_back_the_socket_name_moved() {
    W20D_MOVE.with(|m| m.set((0, 0)));
    let mut ctx = MockNativeContext::new();
    let old_socket = ctx.fresh_object_ref();
    let new_socket = ctx.fresh_object_ref();
    let option = ctx.fresh_object_ref();
    W20D_MOVE.with(|m| m.set((addr(old_socket), addr(new_socket))));
    ctx.set_invoke_virtual_hook(w20d_hook);

    let pins = ctx.native_pin_count_for_test();
    let mut this = old_socket;
    let name = ds_socket_option_name(&mut ctx, &mut this, Some(Value::Object(Some(option))));
    assert_eq!(name, "SO_REUSEADDR");
    assert_eq!(
        addr(this),
        addr(new_socket),
        "the socket is handed back current"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins,
        "both pins are released"
    );
}

#[test]
fn w20d_huc_url_string_hands_back_the_connection_to_string_moved() {
    W20D_MOVE.with(|m| m.set((0, 0)));
    let mut ctx = MockNativeContext::new();
    // A URL whose slot 0 is no full URL text (a real URL's is its
    // protocol), so the helper asks `toString()`.
    let url = ctx.fresh_object_ref();
    let old_conn = ctx.fresh_object_ref();
    let new_conn = ctx.fresh_object_ref();
    ctx.set_field(old_conn, HUC_URL, Value::Object(Some(url)));
    ctx.set_field(new_conn, HUC_URL, Value::Object(Some(url)));
    W20D_MOVE.with(|m| m.set((addr(old_conn), addr(new_conn))));
    ctx.set_invoke_virtual_hook(w20d_hook);

    let pins = ctx.native_pin_count_for_test();
    let mut this = old_conn;
    let text = huc_url_string(&mut ctx, &mut this);
    assert_eq!(text, "http://w20d.test/x");
    assert_eq!(
        addr(this),
        addr(new_conn),
        "the connection is handed back current"
    );
    assert_eq!(ctx.native_pin_count_for_test(), pins, "the pin is released");
}
