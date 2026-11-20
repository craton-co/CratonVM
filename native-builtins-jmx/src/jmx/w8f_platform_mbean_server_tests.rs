use super::*;

#[test]
fn each_vm_gets_its_own_platform_mbean_server_and_roots_and_remaps_only_its_own() {
    // Private identities: the default mock identity 0 is shared by every
    // other mock-context test in this binary.
    const VM_A: usize = 0xF8F8_0001;
    const VM_B: usize = 0xF8F8_0002;
    let mut a = cratonvm_native_builtins::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = cratonvm_native_builtins::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);

    let sa = platform_mbean_server(&mut a).unwrap();
    let sb = platform_mbean_server(&mut b).unwrap();
    assert_ne!(
        sa.as_ptr(),
        sb.as_ptr(),
        "a second VM must not be handed the first VM's heap object"
    );
    assert_eq!(
        platform_mbean_server(&mut a).unwrap().as_ptr(),
        sa.as_ptr(),
        "one server per VM, created once"
    );

    let scan = |vm: usize| {
        let mut roots = Vec::new();
        gc_scan_platform_mbean_server_root_for_vm(vm, &mut roots);
        roots
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect::<Vec<_>>()
    };
    assert_eq!(scan(VM_A), vec![sa.as_ptr() as usize]);
    assert_eq!(scan(VM_B), vec![sb.as_ptr() as usize]);

    // VM A's pointer map moves A's server only, even if it happens to
    // carry an entry for B's address.
    let moved = 0x7777_0000usize;
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(sa.as_ptr() as usize, moved);
    map.insert(sb.as_ptr() as usize, moved + 64);
    gc_update_platform_mbean_server_ref_for_vm(VM_A, &map);
    assert_eq!(scan(VM_A), vec![moved]);
    assert_eq!(scan(VM_B), vec![sb.as_ptr() as usize]);

    // Teardown drops the row; a later VM with the same identity starts
    // clean. Idempotent.
    forget_vm_platform_mbean_server(VM_A);
    forget_vm_platform_mbean_server(VM_A);
    forget_vm_platform_mbean_server(VM_B);
    assert!(scan(VM_A).is_empty());
    assert!(scan(VM_B).is_empty());
}
