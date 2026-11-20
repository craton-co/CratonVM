//! gc-common w20-e: `digest(byte[])` on a provider's digest dispatched
//! `engineDigest` on the receiver's entry address after `engineUpdate`
//! (Java). The mock never moves an object, so the `invoke_virtual` hook
//! plays the collection: `engineUpdate` re-addresses the native roots to a
//! "moved copy", and `engineDigest` answers differently for the vacated
//! copy than for the moved one.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new): the digest the hook moves.
    static W20E_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

fn w20e_update_moves_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    let (old, new) = W20E_MOVE.with(|m| m.get());
    match method_name {
        "engineUpdate" => {
            ctx.remap_native_pin_addr_for_test(old, new);
            Some(Ok(None))
        }
        "engineDigest" => {
            let answer = if receiver.as_ptr() as usize == new {
                2
            } else {
                99
            };
            Some(Ok(Some(Value::Int(answer))))
        }
        _ => None,
    }
}

#[test]
fn w20e_digest_input_asks_the_moved_provider_digest() {
    let mut ctx = MockNativeContext::new();
    // A provider's digest: any class but `java/security/MessageDigest`.
    let digest =
        crate::try_alloc_concurrent_synthetic(&mut ctx, "com/example/W20eDigest", 2).unwrap();
    let moved =
        crate::try_alloc_concurrent_synthetic(&mut ctx, "com/example/W20eDigest", 2).unwrap();
    let input = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 3);
    W20E_MOVE.with(|m| m.set((digest.as_ptr() as usize, moved.as_ptr() as usize)));
    ctx.set_invoke_virtual_hook(w20e_update_moves_hook);
    let base = ctx.native_pin_count_for_test();

    let out = md_digest_input(
        &mut ctx,
        &[Value::Object(Some(digest)), Value::Object(Some(input))],
    );
    W20E_MOVE.with(|m| m.set((0, 0)));

    assert_eq!(
        out.ok().flatten(),
        Some(Value::Int(2)),
        "engineDigest must be dispatched on the moved digest"
    );
    assert_eq!(ctx.native_pin_count_for_test(), base, "the pin is released");
}
