use super::*;

const VM_A: usize = 0x9B00_0401;
const VM_B: usize = 0x9B00_0402;

#[test]
fn server_socket_state_and_stream_owners_are_per_vm() {
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    // The mock's identity hash is the address, so one address in two VMs
    // is exactly "two objects with the same identity hash". Never read.
    let s = unsafe { ObjectRef::from_raw(0x9B00_1000usize as *mut u8) };
    // Minted before any table guard (the registry is a leaf lock).
    let (ka, kb) = (mint_native_obj_key(&a, s), mint_native_obj_key(&b, s));
    assert_ne!(ka, kb);

    ss_set(&a, s, |e| {
        e.port = 4242;
        e.listener_id = 17;
    });
    assert!(ss_tracked(&a, s));
    assert!(
        !ss_tracked(&b, s),
        "VM B must not see VM A's ServerSocket row"
    );
    assert_eq!(ss_get(&b, s).listener_id, -1);
    assert_eq!(ss_get(&a, s).listener_id, 17);

    stream_owner_set(ka, ka);
    assert!(stream_owner_get(&b, s).is_none());
    assert_eq!(stream_owner_get(&a, s), Some(ka));

    // `SSLSessionContext` carriers and `HttpClient` shutdown state.
    ssc_bind(&a, s, s, SSC_TAG_CLIENT);
    assert_eq!(ssc_key(&a, s).map(|k| k.1), Some(SSC_TAG_CLIENT));
    assert_eq!(
        ssc_key(&b, s).map(|k| k.1),
        Some(SSC_TAG_ORPHAN),
        "VM B's carrier must not resolve to VM A's SSLContext"
    );
    // gc-common w16-a: the value is the client's weak-owner address now;
    // `0` records none.
    re5_client_shutdown_table().lock().insert(ka, 0);
    assert!(!re5_client_shutdown_table().lock().contains_key(&kb));

    forget_vm_socket_side_tables(VM_A);
    assert_eq!(ssc_key(&a, s).map(|k| k.1), Some(SSC_TAG_ORPHAN));
    assert!(!re5_client_shutdown_table().lock().contains_key(&ka));
    assert!(!ss_tracked(&a, s), "teardown drops A's ServerSocket row");
    assert!(stream_owner_get(&a, s).is_none());
    assert!(!stream_owner_table().lock().by_owner.contains_key(&ka));
    forget_vm_socket_side_tables(VM_B);
}
