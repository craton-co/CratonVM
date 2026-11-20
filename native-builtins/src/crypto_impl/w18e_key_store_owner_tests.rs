use super::*;

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

/// A placeholder pair -- the ownership machinery never looks inside.
fn dummy_pair() -> RsaKeyPairData {
    let one = || BigUint::from_bytes_be(&[1]);
    RsaKeyPairData {
        public_key: RsaPublicKey { n: one(), e: one() },
        private_key: RsaPrivateKey {
            n: one(),
            d: one(),
            e: one(),
            p: None,
            q: None,
            dp: None,
            dq: None,
            qinv: None,
        },
    }
}

fn stored_rsa_handle() -> u64 {
    let id = rsa_key_next_id();
    rsa_key_store(id, dummy_pair());
    id
}

fn lock_key(ctx: &dyn NativeContext, o: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, o)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

fn present(id: u64) -> bool {
    key_store_contains_for_test(KeyStoreKind::Rsa, id)
}

/// The pair's handle goes only after BOTH key objects and the
/// `Signature` that copied it are dead.
#[test]
fn a_pairs_handle_goes_after_both_keys_and_the_signature() {
    const VM: usize = 0xE18A_0001;
    let _forget = Forget(&[VM]);
    let c = ctx_for(VM);
    let id = stored_rsa_handle();
    let (public, private, sig) = (obj(0x1_E190_0010), obj(0x1_E191_0010), obj(0x1_E192_0010));
    {
        let _hold = hold_key_handle(KeyStoreKind::Rsa, id);
        key_handle_own_obj(&c, public, KeyStoreKind::Rsa, id);
        key_handle_own_obj(&c, private, KeyStoreKind::Rsa, id);
    }
    key_handle_replace_owned(lock_key(&c, sig), Some((KeyStoreKind::Rsa, id)));
    assert_eq!(key_handle_count_for_test(KeyStoreKind::Rsa, id), 3);
    let (pu, pr, sg) = (
        public.as_ptr() as usize,
        private.as_ptr() as usize,
        sig.as_ptr() as usize,
    );
    crate::gc_sweep_lock_keys(VM, &|a| a != pu);
    assert!(
        present(id),
        "freed with the public key while the private one lives"
    );
    crate::gc_sweep_lock_keys(VM, &|a| a != pr);
    assert!(present(id), "freed while a Signature still names it");
    crate::gc_sweep_lock_keys(VM, &|a| a != sg);
    assert!(!present(id), "the pair outlived every object that names it");
    assert_eq!(key_handle_count_for_test(KeyStoreKind::Rsa, id), 0);
}

/// A real key's `RSA_REALKEY_MAP` row owns its handle: a `KeyStore.getKey`
/// proxy's fresh pair goes with the proxy.
#[test]
fn a_real_keys_row_owns_its_handle() {
    const VM: usize = 0xE18A_0002;
    let _forget = Forget(&[VM]);
    let c = ctx_for(VM);
    let id = stored_rsa_handle();
    let key = obj(0x1_E193_0010);
    {
        let _hold = hold_key_handle(KeyStoreKind::Rsa, id);
        rsa_realkey_map_set(&c, key, id);
    }
    assert!(present(id));
    let addr = key.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|a| a != addr);
    assert!(!present(id), "the proxy's key pair outlived it");
}

/// A handle no producer held is untracked: an object naming it keeps it,
/// but its death does not free it (a `Signature` copying a stranger's
/// number must not shorten that key's life).
#[test]
fn an_untracked_handle_is_never_freed_by_an_owner() {
    const VM: usize = 0xE18A_0003;
    let _forget = Forget(&[VM]);
    let c = ctx_for(VM);
    let id = stored_rsa_handle();
    let sig = obj(0x1_E194_0010);
    key_handle_replace_owned(lock_key(&c, sig), Some((KeyStoreKind::Rsa, id)));
    assert_eq!(key_handle_count_for_test(KeyStoreKind::Rsa, id), 0);
    let addr = sig.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|a| a != addr);
    assert!(present(id));
    key_store_remove(KeyStoreKind::Rsa, id);
}

/// A producer that fails before any key object owns the handle frees it
/// when its hold drops.
#[test]
fn a_failed_producer_frees_its_handle() {
    let id = stored_rsa_handle();
    let hold = hold_key_handle(KeyStoreKind::Rsa, id);
    assert!(present(id));
    drop(hold);
    assert!(!present(id));
}

/// VM teardown frees the handles only that VM's objects owned; a live
/// object moved by a collection keeps its handle.
#[test]
fn teardown_frees_one_vms_handles_and_a_moved_owner_keeps_its_own() {
    const VM_A: usize = 0xE18A_0004;
    const VM_B: usize = 0xE18A_0005;
    let _forget = Forget(&[VM_A, VM_B]);
    let (a, b) = (ctx_for(VM_A), ctx_for(VM_B));
    let (ida, idb) = (stored_rsa_handle(), stored_rsa_handle());
    let (old, new) = (0x1_E195_0010usize, 0x3_E195_0010usize);
    let ka = obj(0x2_E195_0010);
    {
        let _ha = hold_key_handle(KeyStoreKind::Rsa, ida);
        let _hb = hold_key_handle(KeyStoreKind::Rsa, idb);
        key_handle_own_obj(&a, ka, KeyStoreKind::Rsa, ida);
        key_handle_own_obj(&b, obj(old), KeyStoreKind::Rsa, idb);
    }
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM_B, &|a| a == old, &moved);
    crate::gc_sweep_lock_keys(VM_B, &|a| a == new);
    assert!(present(idb), "the moved key lost its handle");
    crate::forget_vm_lock_keys(VM_A);
    assert!(!present(ida), "A's teardown left A's key pair");
    assert!(present(idb), "A's teardown took B's key pair");
    crate::forget_vm_lock_keys(VM_B);
    assert!(!present(idb));
}
