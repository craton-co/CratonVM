use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_SLOT: usize = 0x0D1E_0001;
const VM_PLAIN: usize = 0x0D1E_0002;

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_parked_tls_sockets(vm);
            crate::t27_tls::forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

/// The listeners `sock`'s slot holds, in order.
fn slot_listeners(ctx: &MockNativeContext, sock: ObjectRef) -> Vec<ObjectRef> {
    let slot = ssl_sock_listener_slot(ctx, sock).expect("the socket has a listener slot");
    match ctx.get_field(sock, slot) {
        Value::Object(Some(arr)) => (0..ctx.array_length(arr))
            .filter_map(|i| match ctx.get_array_element(arr, i) {
                Value::Object(Some(l)) => Some(l),
                _ => None,
            })
            .collect(),
        Value::Int(SSL_SOCK_NO_LISTENERS) => Vec::new(),
        other => panic!("the listener slot holds {other:?}"),
    }
}

#[test]
fn a_slot_socket_keeps_its_listeners_in_its_own_field() {
    let _teardown = Teardown(&[VM_SLOT]);
    let mut registry = NativeMethodRegistry::new();
    register_p68_ssl(&mut registry);
    let find = |name: &str| {
        registry
            .find(
                "javax/net/ssl/SSLSocket",
                name,
                "(Ljavax/net/ssl/HandshakeCompletedListener;)V",
            )
            .expect("SSLSocket listener native should be registered")
    };
    let (add, remove) = (
        find("addHandshakeCompletedListener"),
        find("removeHandshakeCompletedListener"),
    );
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_SLOT);
    let sock = alloc_ssl_socket(&mut ctx, NEW13_SSL_SOCK_FIELDS).unwrap();
    let n = ctx.object_num_fields(sock);
    assert!(
        n > NEW13_SSL_SOCK_FIELDS,
        "the slot is above the natives' slot map"
    );
    assert_eq!(
        ssl_sock_listener_slot(&ctx, sock),
        Some(n - 1),
        "the LAST slot"
    );
    assert_eq!(
        ctx.get_field(sock, n - 1),
        Value::Int(SSL_SOCK_NO_LISTENERS)
    );

    let (l1, l2) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let base = ctx.global_root_count();
    for l in [l1, l2, l1] {
        add(
            &mut ctx,
            &[Value::Object(Some(sock)), Value::Object(Some(l))],
        )
        .unwrap();
    }
    assert_eq!(
        slot_listeners(&ctx, sock),
        vec![l1, l2],
        "in order; a re-add is idempotent"
    );
    assert_eq!(
        ctx.global_root_count(),
        base,
        "no global root: the socket's field holds them"
    );
    let no_row = existing_ssl_obj_key(&ctx, sock)
        .map_or(true, |k| !handshake_listeners().lock().contains_key(&k));
    assert!(no_row, "no side-table row");

    remove(
        &mut ctx,
        &[Value::Object(Some(sock)), Value::Object(Some(l1))],
    )
    .unwrap();
    assert_eq!(slot_listeners(&ctx, sock), vec![l2]);
    assert!(
        remove(
            &mut ctx,
            &[Value::Object(Some(sock)), Value::Object(Some(l1))]
        )
        .is_err(),
        "a listener removed twice"
    );
    remove(
        &mut ctx,
        &[Value::Object(Some(sock)), Value::Object(Some(l2))],
    )
    .unwrap();
    assert_eq!(
        ctx.get_field(sock, n - 1),
        Value::Int(SSL_SOCK_NO_LISTENERS),
        "the last one out restores the marker"
    );
    assert_eq!(ssl_sock_listener_slot(&ctx, sock), Some(n - 1));
    assert_eq!(ctx.global_root_count(), base);
}

/// Only the exact class with a marked (or array-holding) last slot is a
/// slot socket: a same-class object some other code allocated, and any
/// other object, go to the side table.
#[test]
fn an_unmarked_socket_is_not_a_slot_socket() {
    let _teardown = Teardown(&[VM_PLAIN]);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_PLAIN);
    let cid = ctx.ensure_class_initialized(SSL_SOCKET_CLASS).unwrap();
    let plain = ctx.alloc_object(cid, NEW13_SSL_SOCK_FIELDS);
    assert_eq!(ssl_sock_listener_slot(&ctx, plain), None);
    let other = ctx.fresh_object_ref();
    assert_eq!(ssl_sock_listener_slot(&ctx, other), None);
}
