//! gc-common w19-e: `AbstractMap.isEmpty` and the `Map.Entry` value
//! semantics used addresses read before a Java call after it. The mock
//! never moves an object, so each test's `invoke_virtual` hook plays the
//! collection: it re-addresses the native roots to a "moved copy", and
//! answers differently for the vacated copy than for the moved one.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new): the object the hook moves.
    static W19E_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// The entry key, whose `hashCode()` triggers the move.
    static W19E_KEY: Cell<usize> = const { Cell::new(0) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn reset() {
    W19E_MOVE.with(|m| m.set((0, 0)));
    W19E_KEY.with(|k| k.set(0));
}

/// `size()` moves the receiver and then fails (not an `int`), so the
/// native falls back to the slot.
fn w19e_size_moves_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "size" {
        return None;
    }
    let (old, new) = W19E_MOVE.with(|m| m.get());
    ctx.remap_native_pin_addr_for_test(old, new);
    Some(Ok(None))
}

/// The key's `hashCode()` moves the value; the moved value hashes to 2,
/// the vacated copy to 99.
fn w19e_key_hash_moves_value_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "hashCode" {
        return None;
    }
    let (old, new) = W19E_MOVE.with(|m| m.get());
    let h = if addr(receiver) == W19E_KEY.with(|k| k.get()) {
        ctx.remap_native_pin_addr_for_test(old, new);
        1
    } else if addr(receiver) == new {
        2
    } else {
        99
    };
    Some(Ok(Some(Value::Int(h))))
}

#[test]
fn w19e_abstract_map_is_empty_falls_back_to_the_moved_receivers_slot() {
    reset();
    let mut ctx = MockNativeContext::new();
    let this_old = ctx.fresh_object_ref();
    let this_new = ctx.fresh_object_ref();
    // The vacated copy's slot is junk; the moved copy's says empty.
    ctx.set_field(this_old, 1, Value::Int(7));
    ctx.set_field(this_new, 1, Value::Int(0));
    W19E_MOVE.with(|m| m.set((addr(this_old), addr(this_new))));
    ctx.set_invoke_virtual_hook(w19e_size_moves_hook);
    let mut r = NativeMethodRegistry::new();
    register_p60_abstract_map(&mut r);
    let is_empty = r
        .find("java/util/AbstractMap", "isEmpty", "()Z")
        .expect("AbstractMap.isEmpty is registered");

    let out = is_empty(&mut ctx, &[Value::Object(Some(this_old))]);
    reset();

    assert!(
        matches!(out, Ok(Some(Value::Int(1)))),
        "the fallback must read the receiver's current address, got {out:?}"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

#[test]
fn w19e_entry_hash_code_hashes_the_moved_value() {
    reset();
    let mut ctx = MockNativeContext::new();
    let key = ctx.fresh_object_ref();
    let value_old = ctx.fresh_object_ref();
    let value_new = ctx.fresh_object_ref();
    let entry = ctx.fresh_object_ref();
    ctx.set_field(entry, 0, Value::Object(Some(key)));
    ctx.set_field(entry, 1, Value::Object(Some(value_old)));
    W19E_MOVE.with(|m| m.set((addr(value_old), addr(value_new))));
    W19E_KEY.with(|k| k.set(addr(key)));
    ctx.set_invoke_virtual_hook(w19e_key_hash_moves_value_hook);
    let mut r = NativeMethodRegistry::new();
    register_entry_value_semantics(&mut r, "w19e/Entry");
    let hash_code = r
        .find("w19e/Entry", "hashCode", "()I")
        .expect("Entry.hashCode is registered");

    let out = hash_code(&mut ctx, &[Value::Object(Some(entry))]);
    reset();

    assert!(
        matches!(out, Ok(Some(Value::Int(3)))),
        "key.hashCode() ^ value.hashCode() must hash the value where it lives now \
         (1 ^ 2), got {out:?}"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
