use super::*;

const VM_A: usize = 0x9B00_0801;
const VM_B: usize = 0x9B00_0802;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key / root value only, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn scan(vm: usize) -> Vec<ObjectRef> {
    let mut roots = Vec::new();
    gc_scan_tls_ctx_trust_manager_roots(vm, &mut roots);
    gc_scan_tls_ctx_key_manager_roots(vm, &mut roots);
    gc_scan_default_ssl_context_root(vm, &mut roots);
    roots
}

#[test]
fn tls_rows_and_default_slots_belong_to_the_writing_vm() {
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);

    // Two VMs' objects with one identity hash (the mock's hash is the
    // address truncated to `i32`) no longer share an engine / session key.
    let engine_a = at(0x1_9B03_1000);
    let engine_b = at(0x2_9B03_1000);
    let (ka, kb) = (
        engine_objref_key(&a, engine_a),
        engine_objref_key(&b, engine_b),
    );
    assert_ne!(ka, kb);
    assert!(objref_key_owned_by(ka, VM_A) && !objref_key_owned_by(ka, VM_B));
    assert_eq!(gc_stable_objref_key(&a, engine_a), ka);

    // `SSLContext` keys: VM-unique, and their owner is recoverable.
    let (sslctx_a, sslctx_b) = (at(0x1_9B03_2000), at(0x2_9B03_2000));
    let ctx_a = ctx_obj_key(&mut a, sslctx_a).unwrap();
    let ctx_b = ctx_obj_key(&mut b, sslctx_b).unwrap();
    assert_ne!(ctx_a, ctx_b);
    assert!(ctx_key_visible_to(&a, ctx_a) && !ctx_key_visible_to(&b, ctx_a));

    let (tm_a, selector_a, ses_a, default_a) = (
        at(0x9B03_3000),
        at(0x9B03_4000),
        at(0x9B03_5000),
        at(0x9B03_6000),
    );
    ctx_trust_managers_table().lock().insert(ctx_a, vec![tm_a]);
    engine_alpn_selector_table().lock().insert(ka, selector_a);
    session_id_registry()
        .lock()
        .insert((VM_A, vec![0x9B; 32]), ses_a);
    set_runtime_default_ssl_context(VM_A, default_a);

    for o in [tm_a, selector_a, ses_a, default_a] {
        assert!(scan(VM_A).contains(&o), "VM A roots its own TLS objects");
        assert!(!scan(VM_B).contains(&o), "VM B must not root VM A's");
    }
    assert_eq!(get_runtime_default_ssl_context(VM_B), None);
    assert!(session_id_registry_ids(VM_B).is_empty());
    assert_eq!(session_by_id(VM_A, &[0x9B; 32]), Some(ses_a));
    assert_eq!(session_by_id(VM_B, &[0x9B; 32]), None);

    // VM B's pointer map never rewrites VM A's rows.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(tm_a.as_ptr() as usize, 0x9B03_9000);
    map.insert(default_a.as_ptr() as usize, 0x9B03_A000);
    gc_update_tls_ctx_trust_manager_refs(VM_B, &map);
    gc_update_default_ssl_context_ref(VM_B, &map);
    assert!(scan(VM_A).contains(&tm_a) && scan(VM_A).contains(&default_a));
    gc_update_tls_ctx_trust_manager_refs(VM_A, &map);
    gc_update_default_ssl_context_ref(VM_A, &map);
    assert!(scan(VM_A).contains(&at(0x9B03_9000)));
    assert_eq!(get_runtime_default_ssl_context(VM_A), Some(at(0x9B03_A000)));

    // Teardown (TLS state before lock keys, as `forget_vm_native_root_stores`
    // orders it) drops exactly VM A's rows.
    forget_vm_tls_state(VM_A);
    crate::forget_vm_lock_keys(VM_A);
    assert!(!scan(VM_A).contains(&at(0x9B03_9000)));
    assert!(!ctx_trust_managers_table().lock().contains_key(&ctx_a));
    assert!(!engine_alpn_selector_table().lock().contains_key(&ka));
    assert_eq!(session_by_id(VM_A, &[0x9B; 32]), None);
    assert_eq!(get_runtime_default_ssl_context(VM_A), None);
    forget_vm_tls_state(VM_B);
    crate::forget_vm_lock_keys(VM_B);
}
