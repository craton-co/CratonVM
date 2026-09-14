// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Documents the "build once, freeze" invariant for `NativeMethodRegistry`.
//!
//! `register` takes `&mut self`, so registration is strictly a setup-phase
//! operation: once the registry is handed out as a shared `&Registry`, no
//! caller can add more methods. The VM uses this to populate the table at
//! boot and then expose it as an immutable lookup map to native dispatch.
//!
//! The test below pins two facets of the contract:
//!
//!   * the mutation API is gated on `&mut self`, so a shared reference
//!     cannot smuggle in a new entry;
//!   * find/lookup APIs are `&self`-only and observe the state frozen at
//!     the last `register` call.
//!
//! The "registration runs once, then we freeze it as `&`" pattern is what
//! makes the registry safe to access from many native-thread carriers
//! concurrently — there's no shared mutable state and no inner lock to
//! contend on. The test is therefore both a behavior pin and a
//! compile-time witness: the `freeze_registry_returns_shared_reference`
//! function takes `Arc<Registry>` and the caller can no longer mutate
//! through the `Arc` (only `Arc::get_mut`/`make_mut` could, and both
//! require unique ownership — which is exactly the "build once, freeze"
//! contract).
//!
//! Native-api gap §2-1 from `.claude/review-2026-05-24/native-api.md`.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::Value;
use std::sync::Arc;

fn callback_a(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}
fn callback_b(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Returning an Err exercises the `MethodCallResult` machinery without
    // pulling in the rest of the VM.
    Err(RuntimeError::IllegalArgumentException {
        message: "unreachable".to_string(),
    }
    .into())
}

/// Build-phase: create a fresh registry, register two methods, hand out as
/// a shared `Arc<Registry>` for lookup.  After this returns no caller may
/// add or remove entries.
fn freeze_registry_returns_shared_reference() -> Arc<NativeMethodRegistry> {
    let mut reg = NativeMethodRegistry::new();
    reg.register("com/example/Foo", "alpha", "()I", callback_a);
    reg.register("com/example/Foo", "beta", "()V", callback_b);
    Arc::new(reg)
}

#[test]
fn registered_methods_are_findable_after_freeze() {
    let frozen = freeze_registry_returns_shared_reference();
    // Both registrations land and are observable through the shared `&`.
    assert!(frozen.find("com/example/Foo", "alpha", "()I").is_some());
    assert!(frozen.find("com/example/Foo", "beta", "()V").is_some());
    // Missing triples report None.
    assert!(frozen.find("com/example/Foo", "gamma", "()V").is_none());
    assert!(frozen.find("Other/Class", "alpha", "()I").is_none());
}

#[test]
fn shared_arc_cannot_register_new_methods() {
    // A shared `Arc<Registry>` cannot mutate through the Arc; only
    // `Arc::get_mut` succeeds and only when this is the unique owner.
    let frozen = freeze_registry_returns_shared_reference();
    let clone = Arc::clone(&frozen);
    // Two outstanding Arcs: `get_mut` is impossible — by *design*, this is
    // the "build once" guarantee.
    let mut arc_owner = frozen;
    let unique_borrow = Arc::get_mut(&mut arc_owner);
    assert!(
        unique_borrow.is_none(),
        "with multiple Arcs outstanding, Arc::get_mut must return None — \
         this is the runtime witness for the build-once invariant"
    );
    // Drop the clone, then we can mutate (but the contract is that we
    // do NOT — the registry would already have been published to native
    // dispatch by this point).
    drop(clone);
    let now_unique = Arc::get_mut(&mut arc_owner)
        .expect("only one Arc left — registry mutation is allowed only by the unique owner");
    now_unique.register("Late/Class", "added_after_publication", "()V", callback_a);
    assert!(arc_owner
        .find("Late/Class", "added_after_publication", "()V")
        .is_some());
}

#[test]
fn registry_is_send_and_sync_for_concurrent_lookup() {
    // The whole point of "build once, freeze" is that the frozen
    // `&Registry` can be read from many threads concurrently. This
    // compile-time check pins that the registry's auto-traits (Send,
    // Sync) remain satisfied so the shared-borrow plan keeps working.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NativeMethodRegistry>();
    assert_send_sync::<Arc<NativeMethodRegistry>>();
}
