//! gc-common w15-f. Each test uses its own VM identity (`0xF15_02xx`), so
//! the process-wide link table never shares a row between parallel tests,
//! and each drops its VM's rows before it returns.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new): the address `getHostString` moves.
    static W15F_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// The receiver `getHostName` was dispatched on.
    static W15F_HOST_NAME_RECEIVER: Cell<usize> = const { Cell::new(0) };
}

/// `getAddress` answers null, `getHostString` moves the address and
/// answers null, `getHostName` records its receiver and answers null.
fn w15f_socket_address_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "getAddress" => Some(Ok(Some(Value::Object(None)))),
        "getHostString" => {
            let (old, new) = W15F_MOVE.with(|m| m.get());
            ctx.remap_native_pin_addr_for_test(old, new);
            Some(Ok(Some(Value::Object(None))))
        }
        "getHostName" => {
            W15F_HOST_NAME_RECEIVER.with(|r| r.set(receiver.as_ptr() as usize));
            Some(Ok(Some(Value::Object(None))))
        }
        _ => None,
    }
}

/// The re-read used to happen once, after `getAddress`: a collection
/// inside `getHostString` left `getHostName` dispatched on the vacated
/// address.
#[test]
fn w15f_inet_from_socket_address_asks_the_moved_address_for_its_host_name() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF15_0201);
    let socket_addr = ctx.fresh_object_ref();
    let moved = ctx.fresh_object_ref();
    W15F_MOVE.with(|m| m.set((socket_addr.as_ptr() as usize, moved.as_ptr() as usize)));
    W15F_HOST_NAME_RECEIVER.with(|r| r.set(0));
    ctx.set_invoke_virtual_hook(w15f_socket_address_hook);

    let result = p72_inet_from_socket_address(&mut ctx, socket_addr).unwrap();

    assert_eq!(
        result,
        Some(Value::Object(None)),
        "no host: a null InetAddress"
    );
    assert_eq!(
        W15F_HOST_NAME_RECEIVER.with(|r| r.get()),
        moved.as_ptr() as usize,
        "getHostName must be dispatched on the address getHostString moved it to"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

fn w15f_row(owner_kind: u8, ctx: &MockNativeContext, owner: ObjectRef) -> Option<HttpLinkRow> {
    let key = (owner_kind, http_context_key(ctx, owner));
    http_link_tables().lock().rows.get(&key).copied()
}

/// The retire criterion of
/// `common-w15f-httpserver-link-rows-outlive-their-owner`: a dead owner's
/// row goes, and its global root is released by the VM's next link
/// operation.
#[test]
fn w15f_a_dead_owners_link_row_is_swept_and_its_root_released() {
    const VM: usize = 0xF15_0202;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let owner = ctx.fresh_object_ref();
    let target = ctx.fresh_object_ref();
    http_link_set(&mut ctx, HTTP_LINK_EXECUTOR, owner, Some(target));
    assert_eq!(http_link_get(&ctx, HTTP_LINK_EXECUTOR, owner), Some(target));
    let handle = w15f_row(HTTP_LINK_EXECUTOR, &ctx, owner).unwrap().handle;

    let none = cratonvm_types::PointerMap::default();
    assert_eq!(gc_sweep_http_link_rows(VM, &none, &|_| false), 1);
    assert_eq!(
        http_link_get(&ctx, HTTP_LINK_EXECUTOR, owner),
        None,
        "row dropped"
    );
    assert_eq!(
        ctx.resolve_global_root(handle),
        Some(target),
        "the sweep has no context: the root waits for the next link operation"
    );

    let other = ctx.fresh_object_ref();
    http_link_set(&mut ctx, HTTP_LINK_EXECUTOR, other, None);
    assert_eq!(
        ctx.resolve_global_root(handle),
        None,
        "released by the next operation"
    );
    forget_vm_http_link_rows(VM);
}

/// A moved owner's row is re-addressed, not judged; a live owner's row and
/// another VM's row are left alone.
#[test]
fn w15f_the_link_sweep_follows_a_moved_owner_and_keeps_live_and_foreign_rows() {
    const VM: usize = 0xF15_0203;
    const OTHER_VM: usize = 0xF15_0204;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let moving = ctx.fresh_object_ref();
    let staying = ctx.fresh_object_ref();
    let new_home = ctx.fresh_object_ref();
    let target = ctx.fresh_object_ref();
    http_link_set(&mut ctx, HTTP_LINK_CONTEXT_SERVER, moving, Some(target));
    http_link_set(&mut ctx, HTTP_LINK_CONTEXT_SERVER, staying, Some(target));
    let mut other = MockNativeContext::new();
    other.set_vm_identity(OTHER_VM);
    let foreign = other.fresh_object_ref();
    let foreign_target = other.fresh_object_ref();
    http_link_set(
        &mut other,
        HTTP_LINK_CONTEXT_SERVER,
        foreign,
        Some(foreign_target),
    );

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(moving.as_ptr() as usize, new_home.as_ptr() as usize);
    let staying_addr = staying.as_ptr() as usize;
    let dropped = gc_sweep_http_link_rows(VM, &moved, &|addr| addr == staying_addr);

    assert_eq!(dropped, 0);
    assert_eq!(
        w15f_row(HTTP_LINK_CONTEXT_SERVER, &ctx, moving)
            .unwrap()
            .owner,
        new_home.as_ptr() as usize,
        "the moved owner's row now names its new address"
    );
    assert!(w15f_row(HTTP_LINK_CONTEXT_SERVER, &ctx, staying).is_some());
    assert_eq!(
        http_link_get(&other, HTTP_LINK_CONTEXT_SERVER, foreign),
        Some(foreign_target),
        "another VM's row is not this sweep's"
    );
    forget_vm_http_link_rows(VM);
    forget_vm_http_link_rows(OTHER_VM);
}

/// Teardown drops the VM's rows and only those.
#[test]
fn w15f_forgetting_a_vm_drops_only_its_link_rows() {
    const VM: usize = 0xF15_0205;
    const OTHER_VM: usize = 0xF15_0206;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let mut other = MockNativeContext::new();
    other.set_vm_identity(OTHER_VM);
    let (owner, target) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let (o2, t2) = (other.fresh_object_ref(), other.fresh_object_ref());
    http_link_set(&mut ctx, HTTP_LINK_EXECUTOR, owner, Some(target));
    http_link_set(&mut other, HTTP_LINK_EXECUTOR, o2, Some(t2));

    assert_eq!(forget_vm_http_link_rows(VM), 1);
    assert_eq!(http_link_get(&ctx, HTTP_LINK_EXECUTOR, owner), None);
    assert_eq!(http_link_get(&other, HTTP_LINK_EXECUTOR, o2), Some(t2));
    assert_eq!(forget_vm_http_link_rows(VM), 0, "idempotent");
    forget_vm_http_link_rows(OTHER_VM);
}

/// `setAuthenticator` hands back the authenticator it replaced, and that
/// authenticator's global root is released by the next link operation
/// instead of living for the life of the process.
#[test]
fn w15f_a_replaced_authenticator_is_returned_then_unrooted() {
    const VM: usize = 0xF15_0207;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let hctx = ctx.fresh_object_ref();
    let (first, second) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    assert_eq!(
        http_context_set_authenticator(&mut ctx, hctx, Some(first)),
        None
    );
    let first_handle = w15f_row(HTTP_LINK_AUTHENTICATOR, &ctx, hctx)
        .unwrap()
        .handle;

    let previous = http_context_set_authenticator(&mut ctx, hctx, Some(second));

    assert_eq!(previous, Some(first));
    assert_eq!(http_context_authenticator(&ctx, hctx), Some(second));
    assert_eq!(
        ctx.resolve_global_root(first_handle),
        Some(first),
        "still rooted while it is being handed back"
    );
    assert_eq!(http_context_authenticator(&ctx, hctx), Some(second));
    let unrelated = ctx.fresh_object_ref();
    http_link_set(&mut ctx, HTTP_LINK_EXECUTOR, unrelated, None);
    assert_eq!(ctx.resolve_global_root(first_handle), None);
    forget_vm_http_link_rows(VM);
}

/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 9): two live `HttpContext`s with ONE identity hash (the mock's hash
/// is the address truncated to `i32`, lesson qq) keep their own
/// authenticators; B's `setAuthenticator` used to displace A's, and B's
/// `getAuthenticator` answered A's. A dead one's row goes alone.
#[test]
fn w29d_same_hash_contexts_keep_their_own_authenticators() {
    const VM: usize = 0xF29_0D01;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only, 8-byte aligned, never dereferenced by the link
    // table or the mock's identity hash.
    let a = unsafe { ObjectRef::from_raw(0x1_0F29_0D00usize as *mut u8) };
    let b = unsafe { ObjectRef::from_raw(0x2_0F29_0D00usize as *mut u8) };
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two contexts share an identity hash"
    );
    let (auth_a, auth_b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    assert_eq!(
        http_context_set_authenticator(&mut ctx, a, Some(auth_a)),
        None
    );
    assert_eq!(
        http_context_authenticator(&ctx, b),
        None,
        "B must not answer A's authenticator"
    );
    assert_eq!(
        http_context_set_authenticator(&mut ctx, b, Some(auth_b)),
        None,
        "B's set must not displace A's"
    );
    assert_eq!(http_context_authenticator(&ctx, a), Some(auth_a));
    assert_eq!(http_context_authenticator(&ctx, b), Some(auth_b));

    let b_addr = b.as_ptr() as usize;
    let none = cratonvm_types::PointerMap::default();
    assert_eq!(
        gc_sweep_http_link_rows(VM, &none, &|addr| addr == b_addr),
        1
    );
    assert_eq!(
        http_context_authenticator(&ctx, b),
        Some(auth_b),
        "B's row stays"
    );
    forget_vm_http_link_rows(VM);
    crate::forget_vm_lock_keys(VM);
}
