use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xB27B_0A01;
const VM_B: usize = 0xB27B_0A02;
const VM_C: usize = 0xB27B_0A03;
const VM_D: usize = 0xB27B_0A04;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

/// Forgets this test's VMs only, even when an assertion fails: the rows
/// first (as `forget_vm_native_root_stores` orders it), then the keys.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_trust_manager_states(vm);
            forget_vm_key_manager_states(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn has_row(
    table: &std::sync::Mutex<std::collections::HashMap<usize, ManagerIdRow>>,
    key: usize,
) -> bool {
    table
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
}

#[test]
fn two_live_same_hash_managers_keep_separate_ids() {
    let _teardown = Teardown(&[VM_A]);
    let a = ctx_in(VM_A);
    let (m1, m2) = (at(0x1_B27B_1000), at(0x2_B27B_1000));
    assert_eq!(
        a.identity_hash_code(m1),
        a.identity_hash_code(m2),
        "premise: the two managers share an identity hash"
    );
    file_manager_id_row(&a, km_id_by_identity(), m1, 11);
    assert_eq!(
        lookup_manager_id_row(&a, km_id_by_identity(), m2),
        None,
        "a live collider does not read the first one's KeyManager id"
    );
    file_manager_id_row(&a, km_id_by_identity(), m2, 12);
    file_manager_id_row(&a, tm_id_by_identity(), m1, 21);
    file_manager_id_row(&a, tm_id_by_identity(), m2, 22);
    assert_eq!(lookup_manager_id_row(&a, km_id_by_identity(), m1), Some(11));
    assert_eq!(lookup_manager_id_row(&a, km_id_by_identity(), m2), Some(12));
    assert_eq!(lookup_manager_id_row(&a, tm_id_by_identity(), m1), Some(21));
    assert_eq!(lookup_manager_id_row(&a, tm_id_by_identity(), m2), Some(22));
    // A lookup of an object never keyed leaves no slot behind.
    let stranger = at(0x3_B27B_1000);
    assert_eq!(
        lookup_manager_id_row(&a, km_id_by_identity(), stranger),
        None
    );
    assert!(crate::existing_weak_lock_key(&a, stranger).is_none());
}

/// The lock-key sweep frees a dead object's key and drops its rows in
/// both tables (`lib.rs::sweep_lock_keys` -> `forget_x509_manager_keys`);
/// a live object's rows stay.
#[test]
fn a_freed_key_drops_its_rows() {
    let _teardown = Teardown(&[VM_B]);
    let b = ctx_in(VM_B);
    let (dead, live) = (at(0x1_B27B_2000), at(0x2_B27B_2000));
    file_manager_id_row(&b, km_id_by_identity(), dead, 31);
    file_manager_id_row(&b, tm_id_by_identity(), dead, 32);
    file_manager_id_row(&b, km_id_by_identity(), live, 33);
    let k_dead = crate::existing_weak_lock_key(&b, dead).expect("filed");
    let k_live = crate::existing_weak_lock_key(&b, live).expect("filed");

    let live_addr = live.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM_B, &|x| x == live_addr);

    assert!(
        crate::existing_weak_lock_key(&b, dead).is_none(),
        "the key was freed"
    );
    assert!(!has_row(km_id_by_identity(), k_dead));
    assert!(!has_row(tm_id_by_identity(), k_dead));
    assert!(has_row(km_id_by_identity(), k_live));
    assert_eq!(
        lookup_manager_id_row(&b, km_id_by_identity(), live),
        Some(33)
    );
}

/// One address in two VMs never shares a key; one VM's lock-key teardown
/// drops exactly its own rows.
#[test]
fn two_vms_stay_separate() {
    let _teardown = Teardown(&[VM_C, VM_D]);
    let (c, d) = (ctx_in(VM_C), ctx_in(VM_D));
    let obj = at(0x1_B27B_3000);
    file_manager_id_row(&c, km_id_by_identity(), obj, 41);
    file_manager_id_row(&d, km_id_by_identity(), obj, 42);
    let (kc, kd) = (
        crate::existing_weak_lock_key(&c, obj).expect("filed"),
        crate::existing_weak_lock_key(&d, obj).expect("filed"),
    );
    assert_ne!(kc, kd);
    assert_eq!(
        lookup_manager_id_row(&c, km_id_by_identity(), obj),
        Some(41)
    );
    assert_eq!(
        lookup_manager_id_row(&d, km_id_by_identity(), obj),
        Some(42)
    );

    crate::forget_vm_lock_keys(VM_C);
    assert!(
        !has_row(km_id_by_identity(), kc),
        "the freed key's row went"
    );
    assert_eq!(
        lookup_manager_id_row(&d, km_id_by_identity(), obj),
        Some(42)
    );
}
