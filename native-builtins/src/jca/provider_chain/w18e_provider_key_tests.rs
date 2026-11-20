use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for vm in self.0 {
            crate::forget_vm_lock_keys(*vm);
        }
    }
}

fn has_key(ctx: &dyn NativeContext, p: ObjectRef, key: &str) -> Option<bool> {
    provider_own_keys(ctx, p, |keys| keys.contains(key))
}

/// VM B's provider with the identity hash of VM A's sees none of A's keys
/// (the bare-hash key answered `true`, and BouncyCastle's `addAlgorithm`
/// then threw `duplicate provider key` in B's constructor), and A's
/// teardown takes only A's row.
#[test]
fn a_second_vms_provider_with_one_identity_hash_sees_no_rows() {
    const VM_A: usize = 0xE18B_0001;
    const VM_B: usize = 0xE18B_0002;
    let _forget = Forget(&[VM_A, VM_B]);
    let a = ctx_for(VM_A);
    let b = ctx_for(VM_B);
    let (pa, pb) = (obj(0x1_E183_0010), obj(0x2_E183_0010));
    assert_eq!(a.identity_hash_code(pa), b.identity_hash_code(pb));
    note_provider_instance_key(&a, pa, "MessageDigest.GOST3411".to_string());
    assert_eq!(has_key(&a, pa, "MessageDigest.GOST3411"), Some(true));
    assert_eq!(
        has_key(&b, pb, "MessageDigest.GOST3411"),
        None,
        "B's provider saw A's key: a spurious `duplicate provider key`"
    );
    note_provider_instance_key(&b, pb, "Cipher.AES".to_string());
    let ka = provider_object_key(&a, pa);
    crate::forget_vm_lock_keys(VM_A);
    assert!(!provider_instance_keys().lock().contains_key(&ka));
    assert_eq!(has_key(&b, pb, "Cipher.AES"), Some(true));
    assert_eq!(has_key(&b, pb, "MessageDigest.GOST3411"), Some(false));
}

/// A dead provider's rows and a dead service's class-name / attribute rows
/// go with their keys, and a later object with the same identity hash
/// inherits none of them.
#[test]
fn dead_provider_and_service_rows_go_with_their_keys() {
    const VM: usize = 0xE18B_0003;
    let _forget = Forget(&[VM]);
    let c = ctx_for(VM);
    let (prov, svc) = (obj(0x1_E184_0010), obj(0x1_E185_0010));
    note_provider_instance_key(&c, prov, "Signature.SHA256withRSA".to_string());
    let kp = provider_object_key(&c, prov);
    let ks = provider_object_key(&c, svc);
    service_classname_table()
        .lock()
        .insert(ks, "w18e.DeadSpi".to_string());
    service_attributes_table()
        .lock()
        .insert(ks, vec![("KeySize".to_string(), "256".to_string())]);
    let (pa, sa) = (prov.as_ptr() as usize, svc.as_ptr() as usize);
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != pa && a != sa) >= 2);
    assert!(!provider_instance_keys().lock().contains_key(&kp));
    assert!(!service_classname_table().lock().contains_key(&ks));
    assert!(!service_attributes_table().lock().contains_key(&ks));
    let (prov2, svc2) = (obj(0x2_E184_0010), obj(0x2_E185_0010));
    assert_eq!(has_key(&c, prov2, "Signature.SHA256withRSA"), None);
    let ks2 = provider_object_key(&c, svc2);
    assert_ne!(ks2, ks);
    assert!(!service_classname_table().lock().contains_key(&ks2));
}

/// A live provider that a collection moves keeps its rows.
#[test]
fn a_moved_provider_keeps_its_rows() {
    const VM: usize = 0xE18B_0004;
    let _forget = Forget(&[VM]);
    let c = ctx_for(VM);
    let (old, new) = (0x1_E186_0010usize, 0x3_E186_0010usize);
    note_provider_instance_key(&c, obj(old), "KeyStore.W18E".to_string());
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(has_key(&c, obj(new), "KeyStore.W18E"), Some(true));
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(has_key(&c, obj(new), "KeyStore.W18E"), Some(true));
}
