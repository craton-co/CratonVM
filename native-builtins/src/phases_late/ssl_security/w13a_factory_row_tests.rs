use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xA13F_1001;
const VM_B: usize = 0xA13F_1002;
const VM_C: usize = 0xA13F_1003;

/// TLS state first, then the lock keys (the order VM teardown uses).
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::t27_tls::forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

#[test]
fn p68_anchor_rows_are_per_vm_and_go_with_their_owner() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    let (dead, live) = (a.fresh_object_ref(), a.fresh_object_ref());
    for o in [dead, live] {
        let key = p68_trust_roots_key(&a, o);
        p68_ctx_trust_roots_table()
            .lock()
            .insert(key, vec![b"root".to_vec()]);
        note_p68_trust_roots_owner(&a, key, o);
    }
    // Another VM's object with the same identity hash has its own key.
    assert_ne!(p68_trust_roots_key(&a, live), p68_trust_roots_key(&b, live));
    let live_addr = live.as_ptr() as usize;
    crate::t27_tls::gc_sweep_tls_rows(VM_A, &|x| x == live_addr);
    let table = p68_ctx_trust_roots_table().lock();
    assert!(!table.contains_key(&p68_trust_roots_key(&a, dead)));
    assert!(table.contains_key(&p68_trust_roots_key(&a, live)));
}

/// gc-common w27-a: the rows go when the lock-key sweep frees the
/// factory's key (`lib.rs::sweep_lock_keys` -> the hook), which covers
/// the `lib.rs` wiring too.
#[test]
fn a_dead_factory_takes_its_identity_rows() {
    let _teardown = Teardown(&[VM_C]);
    let vm = VM_C;
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    let factory = c.fresh_object_ref();
    let key = ssl_obj_key(&c, factory);
    kmf_keystore_id_by_identity().lock().insert(key, 5);
    kmf_key_manager_cache().lock().insert(key, (5, 50));
    tmf_tm_id_by_identity().lock().insert(key, 60);
    assert!(factory_rows_name_manager_id(vm, true, 50));
    assert!(factory_rows_name_manager_id(vm, false, 60));
    // Alive: nothing goes.
    let factory_addr = factory.as_ptr() as usize;
    crate::gc_sweep_lock_keys(vm, &|x| x == factory_addr);
    assert!(kmf_keystore_id_by_identity().lock().contains_key(&key));
    crate::gc_sweep_lock_keys(vm, &|_| false);
    assert!(!kmf_keystore_id_by_identity().lock().contains_key(&key));
    assert!(!factory_rows_name_manager_id(vm, true, 50));
    assert!(!factory_rows_name_manager_id(vm, false, 60));
    // The ids were offered; the VM's next TLS sweep settles them (neither
    // is registered here, so nothing else changes).
    crate::t27_tls::gc_sweep_tls_rows(vm, &|_| false);
}
