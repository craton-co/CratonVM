use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xB27B_0B01;
const VM_B: usize = 0xB27B_0B02;
const VM_C: usize = 0xB27B_0B03;
const VM_D: usize = 0xB27B_0B04;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_keystores(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn carried_by(c: &crate::test_utils::MockNativeContext, obj: ObjectRef) -> Option<Vec<i32>> {
    let key = crate::existing_weak_lock_key(c, obj)?;
    registry()
        .read()
        .carriers
        .get(&key)
        .map(|row| row.ids.clone())
}

fn empty_store() -> LoadedKeyStore {
    LoadedKeyStore::default()
}

/// Two live same-hash objects: separate identity rows and separate
/// carrier rows; a re-stamp of one leaves the other's record alone.
#[test]
fn two_live_same_hash_objects_keep_separate_rows() {
    let _teardown = Teardown(&[VM_A]);
    let a = ctx_in(VM_A);
    let (o1, o2) = (at(0x1_B27B_5000), at(0x2_B27B_5000));
    assert_eq!(
        a.identity_hash_code(o1),
        a.identity_hash_code(o2),
        "premise: the two objects share an identity hash"
    );
    let (s1, s2) = (
        keystore_register_in_vm(VM_A, empty_store()),
        keystore_register_in_vm(VM_A, empty_store()),
    );
    file_store_id_row(&a, o1, s1);
    assert_eq!(
        lookup_store_id_row(&a, o2),
        None,
        "a live collider does not resolve the first one's store"
    );
    file_store_id_row(&a, o2, s2);
    assert_eq!(lookup_store_id_row(&a, o1), Some(s1));
    assert_eq!(lookup_store_id_row(&a, o2), Some(s2));

    note_store_carrier(&a, o1, s1);
    note_store_carrier(&a, o2, s1);
    assert_eq!(carried_by(&a, o1), Some(vec![s1]));
    assert_eq!(carried_by(&a, o2), Some(vec![s1]));
    // `o1` stops carrying `s1`; `o2` still does, so the store stays.
    forget_store_carrier(&a, o1, s1);
    assert_eq!(carried_by(&a, o1), None);
    assert_eq!(carried_by(&a, o2), Some(vec![s1]));
    assert!(
        keystore_lookup(s1).is_some(),
        "the live collider still carries it"
    );
}

/// The lock-key sweep frees a dead carrier's key: its identity row and
/// carrier row go (`lib.rs::sweep_lock_keys` -> `forget_keystore_keys`),
/// and the store it was the last carrier of is released. A live carrier's
/// rows and store stay.
#[test]
fn a_freed_key_drops_its_rows_and_releases_the_store() {
    let _teardown = Teardown(&[VM_B]);
    let b = ctx_in(VM_B);
    let (dead, live) = (at(0x1_B27B_6000), at(0x2_B27B_6000));
    let (s_dead, s_live) = (
        keystore_register_in_vm(VM_B, empty_store()),
        keystore_register_in_vm(VM_B, empty_store()),
    );
    file_store_id_row(&b, dead, s_dead);
    note_store_carrier(&b, dead, s_dead);
    file_store_id_row(&b, live, s_live);
    note_store_carrier(&b, live, s_live);
    let k_dead = crate::existing_weak_lock_key(&b, dead).expect("filed");

    let live_addr = live.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM_B, &|x| x == live_addr);

    assert!(
        crate::existing_weak_lock_key(&b, dead).is_none(),
        "the key was freed"
    );
    assert!(!store_id_by_identity()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&k_dead));
    assert!(!registry().read().carriers.contains_key(&k_dead));
    assert!(keystore_lookup(s_dead).is_none(), "its last carrier died");
    assert!(keystore_lookup(s_live).is_some());
    assert_eq!(lookup_store_id_row(&b, live), Some(s_live));
    assert_eq!(carried_by(&b, live), Some(vec![s_live]));
}

/// One address in two VMs never shares a key; one VM's lock-key teardown
/// drops exactly its own rows.
#[test]
fn two_vms_stay_separate() {
    let _teardown = Teardown(&[VM_C, VM_D]);
    let (c, d) = (ctx_in(VM_C), ctx_in(VM_D));
    let obj = at(0x1_B27B_7000);
    let (sc, sd) = (
        keystore_register_in_vm(VM_C, empty_store()),
        keystore_register_in_vm(VM_D, empty_store()),
    );
    file_store_id_row(&c, obj, sc);
    file_store_id_row(&d, obj, sd);
    note_store_carrier(&c, obj, sc);
    note_store_carrier(&d, obj, sd);
    let kc = crate::existing_weak_lock_key(&c, obj).expect("filed");
    assert_ne!(Some(kc), crate::existing_weak_lock_key(&d, obj));
    assert_eq!(lookup_store_id_row(&c, obj), Some(sc));
    assert_eq!(lookup_store_id_row(&d, obj), Some(sd));

    crate::forget_vm_lock_keys(VM_C);
    assert!(!registry().read().carriers.contains_key(&kc));
    assert_eq!(lookup_store_id_row(&d, obj), Some(sd));
    assert_eq!(carried_by(&d, obj), Some(vec![sd]));
    assert!(keystore_lookup(sd).is_some());
}
