//! gc-common w20-e: `SSLSession.toString()` dispatched `getCipherSuite()`
//! on the session's entry address after `getCreationTime()` (Java). The
//! mock never moves an object, so the `invoke_virtual` hook plays the
//! collection: `getCreationTime` re-addresses the native roots to a
//! "moved copy", and `getCipherSuite` answers differently for the vacated
//! copy than for the moved one.
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeMethodRegistry, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};
use std::cell::Cell;

thread_local! {
    /// (old, new): the session the hook moves.
    static W20E_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

fn w20e_session_moves_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    let (old, new) = W20E_MOVE.with(|m| m.get());
    match method_name {
        "getCreationTime" => {
            ctx.remap_native_pin_addr_for_test(old, new);
            Some(Ok(Some(Value::Long(7))))
        }
        "getCipherSuite" => {
            let text = if receiver.as_ptr() as usize == new {
                "TLS_W20E_MOVED"
            } else {
                "TLS_W20E_VACATED"
            };
            let s = ctx.create_string(text);
            Some(Ok(Some(Value::Object(Some(s)))))
        }
        _ => None,
    }
}

#[test]
fn w20e_session_to_string_asks_the_moved_session() {
    let mut r = NativeMethodRegistry::new();
    super::register_ssl_session_real(&mut r);
    let to_string = r
        .find(
            "javax/net/ssl/SSLSession",
            "toString",
            "()Ljava/lang/String;",
        )
        .expect("SSLSession.toString registered");

    let mut ctx = MockNativeContext::new();
    let session = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let moved = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    W20E_MOVE.with(|m| m.set((session.as_ptr() as usize, moved.as_ptr() as usize)));
    ctx.set_invoke_virtual_hook(w20e_session_moves_hook);
    let base = ctx.native_pin_count_for_test();

    let out = to_string(&mut ctx, &[Value::Object(Some(session))]);
    W20E_MOVE.with(|m| m.set((0, 0)));

    let Ok(Some(Value::Object(Some(s)))) = out else {
        panic!("toString answered {out:?}");
    };
    assert_eq!(
        ctx.read_string(s).as_deref(),
        Some("Session(7|TLS_W20E_MOVED)"),
        "getCipherSuite must be dispatched on the moved session"
    );
    assert_eq!(ctx.native_pin_count_for_test(), base, "the pin is released");
}
