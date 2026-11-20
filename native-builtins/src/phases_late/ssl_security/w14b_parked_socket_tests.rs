use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeHeapAccess, NativeSystemAccess};

const VM_A: usize = 0xB14E_0A01;
const VM_B: usize = 0xB14E_0B01;

/// Two VMs whose sockets draw the same identity hash never read each
/// other's staged trust scope, and a VM's teardown drops only its own
/// staged scopes and parked connections.
/// gc-common w16-d: staging now records the socket as a weak owner, so
/// the test's VMs leave `t27_tls` weak-owner rows; drop them on every
/// exit path.
struct Teardown;
impl Drop for Teardown {
    fn drop(&mut self) {
        for vm in [VM_A, VM_B] {
            forget_vm_parked_tls_sockets(vm);
            crate::t27_tls::forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

#[test]
fn parked_ssl_sockets_are_per_vm_and_go_at_teardown() {
    let _teardown = Teardown;
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    // The mock's identity hash is the address, so one fake socket has the
    // same hash under both VMs.
    let sock = unsafe { ObjectRef::from_raw(0x0b14_0000_1400usize as *mut u8) };
    assert_eq!(a.identity_hash_code(sock), b.identity_hash_code(sock));

    stash_pending_ssl_socket_connect_ctx(&a, sock, vec![b"root-a".to_vec()], Some(7), None);
    assert!(
        take_pending_ssl_socket_connect_ctx(&b, sock).0.is_empty(),
        "B read A's row"
    );
    assert_eq!(
        take_pending_ssl_socket_connect_ctx(&a, sock).0,
        vec![b"root-a".to_vec()]
    );

    stash_pending_ssl_socket_connect_ctx(&a, sock, vec![b"root-a".to_vec()], None, None);
    stash_pending_ssl_socket_connect_ctx(&b, sock, vec![b"root-b".to_vec()], None, None);
    let pid = stash_pending_connect_socket(PendingConnectSocket {
        host: "localhost".into(),
        port: 1,
        extra_roots: Vec::new(),
        java_tm_key: None,
        max_protocol: None,
        client_identity: None,
        tcp: None,
        vm: VM_A,
        sock_key: ssl_sock_key(&a, sock).1,
    });
    forget_vm_parked_tls_sockets(VM_A);
    assert!(
        take_pending_connect_socket(pid).is_none(),
        "A's parked socket survived"
    );
    assert!(
        take_pending_ssl_socket_connect_ctx(&a, sock).0.is_empty(),
        "A's scope survived"
    );
    assert_eq!(
        take_pending_ssl_socket_connect_ctx(&b, sock).0,
        vec![b"root-b".to_vec()]
    );
}
