//! gc-common w20-d: the `HttpHandler.handle` backstop sends
//! `sendResponseHeaders` (Java) and then `close` to the exchange; `close`
//! must reach the address the first call moved the exchange to. The
//! mock's `invoke_virtual` hook plays that move on the native pins.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new) address pair `sendResponseHeaders` moves.
    static W20D_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// The receiver `close()` was sent to.
    static W20D_CLOSED: Cell<usize> = const { Cell::new(0) };
}

fn w20d_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "sendResponseHeaders" => {
            let (old, new) = W20D_MOVE.with(|m| m.replace((0, 0)));
            if old != 0 {
                ctx.remap_native_pin_addr_for_test(old, new);
            }
            Some(Ok(None))
        }
        "close" => {
            W20D_CLOSED.with(|c| c.set(receiver.as_ptr() as usize));
            Some(Ok(None))
        }
        _ => None,
    }
}

#[test]
fn w20d_handler_backstop_closes_the_exchange_where_the_headers_call_moved_it() {
    W20D_MOVE.with(|m| m.set((0, 0)));
    W20D_CLOSED.with(|c| c.set(0));
    let mut r = NativeMethodRegistry::new();
    register_p72_http_server(&mut r);
    let handle = r
        .find(
            "com/sun/net/httpserver/HttpHandler",
            "handle",
            "(Lcom/sun/net/httpserver/HttpExchange;)V",
        )
        .expect("the HttpHandler.handle backstop must be registered");
    let mut ctx = MockNativeContext::new();
    let handler = ctx.fresh_object_ref();
    let old_exchange = ctx.fresh_object_ref();
    let new_exchange = ctx.fresh_object_ref();
    W20D_MOVE.with(|m| {
        m.set((
            old_exchange.as_ptr() as usize,
            new_exchange.as_ptr() as usize,
        ))
    });
    ctx.set_invoke_virtual_hook(w20d_hook);

    let result = handle(
        &mut ctx,
        &[
            Value::Object(Some(handler)),
            Value::Object(Some(old_exchange)),
        ],
    );
    assert!(matches!(result, Ok(None)), "the backstop answers void");
    assert_eq!(
        W20D_CLOSED.with(|c| c.get()),
        new_exchange.as_ptr() as usize,
        "close() must reach the exchange's current address"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "the pin is released");
}
