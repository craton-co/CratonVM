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

const PSS: PssParams = PssParams {
    hash: crypto_impl::PssHash::Sha256,
    mgf_hash: crypto_impl::PssHash::Sha256,
    salt_len: 32,
};

/// A row in each of the six tables for `s`.
fn fill(ctx: &mut dyn NativeContext, s: ObjectRef, algo: i32, kid: u64, payload: &[u8]) {
    set_sig_algo(ctx, s, algo);
    set_sig_state(ctx, s, STATE_SIGN);
    set_sig_keyid(ctx, s, kid);
    let key = sig_key(ctx, s);
    sig_payload_table().lock().insert(key, payload.to_vec());
    sig_pss_table().lock().insert(key, PSS);
    sig_user_spi_table()
        .lock()
        .insert(key, ("W17B".to_string(), "w17b.Spi".to_string()));
}

fn rows_of(key: SigKey) -> [bool; 6] {
    // One statement per table: a guard temporary lives to the end of its
    // statement, and the two `Scratch`-level tables must never be held
    // together (the lock-order check panics).
    let algo = sig_algo_table().lock().contains_key(&key);
    let state = sig_state_table().lock().contains_key(&key);
    let keyid = sig_keyid_table().lock().contains_key(&key);
    let payload = sig_payload_table().lock().contains_key(&key);
    let pss = sig_pss_table().lock().contains_key(&key);
    let spi = sig_user_spi_table().lock().contains_key(&key);
    [algo, state, keyid, payload, pss, spi]
}

/// Two VMs' `Signature`s with one identity hash keep distinct rows, and
/// one VM's teardown takes only its own (the old `(vm, hash)` key was
/// never dropped at teardown).
#[test]
fn two_vms_signatures_with_one_identity_hash_keep_distinct_rows() {
    const VM_A: usize = 0xB17C_0001;
    const VM_B: usize = 0xB17C_0002;
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let (sa, sb) = (obj(0x1_B170_0010), obj(0x2_B170_0010));
    assert_eq!(a.identity_hash_code(sa), b.identity_hash_code(sb));
    fill(&mut a, sa, SIG_SHA256_RSA, 11, b"from A");
    assert_eq!(get_sig_algo(&mut b, sb), None, "B read A's algorithm");
    fill(&mut b, sb, SIG_SHA256_ECDSA, 22, b"from B");
    let (ka, kb) = (sig_key(&mut a, sa), sig_key(&mut b, sb));
    assert_ne!(ka, kb);
    assert_eq!(get_sig_algo(&mut a, sa), Some(SIG_SHA256_RSA));
    assert_eq!(get_sig_keyid(&mut a, sa), Some(11));
    assert_eq!(get_sig_keyid(&mut b, sb), Some(22));

    crate::forget_vm_lock_keys(VM_A);
    assert_eq!(rows_of(ka), [false; 6], "A's teardown left rows");
    assert_eq!(rows_of(kb), [true; 6], "A's teardown took B's rows");
    assert_eq!(get_sig_algo(&mut b, sb), Some(SIG_SHA256_ECDSA));
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
    assert_eq!(rows_of(kb), [false; 6]);
}

/// A dead `Signature`'s rows go with its key, in all six tables, and a
/// later `Signature` with its identity hash inherits none of them (the old
/// key found them by hash).
#[test]
fn a_dead_signatures_rows_go_with_its_key() {
    const VM: usize = 0xB17C_0003;
    let mut c = ctx_for(VM);
    let dead = obj(0x1_B171_0010);
    fill(&mut c, dead, SIG_SHA256_RSA, 33, b"payload");
    let kd = sig_key(&mut c, dead);
    assert_eq!(rows_of(kd), [true; 6]);
    let addr = dead.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != addr) >= 1);
    assert_eq!(
        rows_of(kd),
        [false; 6],
        "the dead Signature's rows outlived it"
    );

    let reborn = obj(0x2_B171_0010);
    assert_ne!(sig_key(&mut c, reborn), kd);
    assert_eq!(get_sig_algo(&mut c, reborn), None);
    assert_eq!(get_sig_state(&mut c, reborn), None);
    assert_eq!(get_sig_keyid(&mut c, reborn), None);
    assert_eq!(get_sig_pss(&mut c, reborn), None);
    assert_eq!(get_sig_user_spi(&mut c, reborn), None);
    crate::forget_vm_lock_keys(VM);
}

/// A live `Signature` that a collection moves keeps its rows.
#[test]
fn a_moved_signature_keeps_its_rows() {
    const VM: usize = 0xB17C_0004;
    let mut c = ctx_for(VM);
    let (old, new) = (0x1_B172_0010usize, 0x3_B172_0010usize);
    fill(&mut c, obj(old), SIG_SHA256_ECDSA, 44, b"moved");
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(get_sig_algo(&mut c, obj(new)), Some(SIG_SHA256_ECDSA));
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(get_sig_keyid(&mut c, obj(new)), Some(44));
    assert_eq!(get_sig_pss(&mut c, obj(new)), Some(PSS));
    let key = sig_key(&mut c, obj(new));
    assert_eq!(
        sig_payload_table().lock().get(&key).map(Vec::as_slice),
        Some(&b"moved"[..])
    );
    crate::forget_vm_lock_keys(VM);
}

/// `RSA_REALKEY_MAP`, two VMs: B's real RSA key with the identity hash of
/// A's resolves to nothing, not to A's handle, and A's teardown takes only
/// A's row.
#[test]
fn two_vms_real_rsa_keys_with_one_identity_hash_keep_distinct_handles() {
    const VM_A: usize = 0xB17C_0005;
    const VM_B: usize = 0xB17C_0006;
    let a = ctx_for(VM_A);
    let b = ctx_for(VM_B);
    let (ka, kb) = (obj(0x1_B173_0010), obj(0x2_B173_0010));
    assert_eq!(a.identity_hash_code(ka), b.identity_hash_code(kb));
    crypto_impl::rsa_realkey_map_set(&a, ka, 0xB17A);
    assert_eq!(crypto_impl::rsa_realkey_map_get(&b, kb), None);
    crypto_impl::rsa_realkey_map_set(&b, kb, 0xB17B);
    assert_eq!(crypto_impl::rsa_realkey_map_get(&a, ka), Some(0xB17A));
    assert_eq!(crypto_impl::rsa_realkey_map_get(&b, kb), Some(0xB17B));
    crate::forget_vm_lock_keys(VM_A);
    assert_eq!(crypto_impl::rsa_realkey_map_get(&b, kb), Some(0xB17B));
    assert_eq!(crypto_impl::rsa_realkey_map_get(&a, ka), None);
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
    assert_eq!(crypto_impl::rsa_realkey_map_get(&b, kb), None);
    crate::forget_vm_lock_keys(VM_B);
}

/// A dead real RSA key's row goes with its key, and a later key object
/// with its identity hash does not sign with the dead key's handle.
#[test]
fn a_dead_real_rsa_keys_row_goes_with_its_key() {
    const VM: usize = 0xB17C_0007;
    let c = ctx_for(VM);
    let dead = obj(0x1_B174_0010);
    crypto_impl::rsa_realkey_map_set(&c, dead, 0xB17D);
    let kd = crypto_impl::rsa_realkey_key_for(&c, dead);
    assert!(crypto_impl::rsa_realkey_map_contains_for_test(kd));
    let addr = dead.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != addr) >= 1);
    assert!(
        !crypto_impl::rsa_realkey_map_contains_for_test(kd),
        "the dead key's row outlived it"
    );
    let reborn = obj(0x2_B174_0010);
    assert_ne!(crypto_impl::rsa_realkey_key_for(&c, reborn), kd);
    assert_eq!(crypto_impl::rsa_realkey_map_get(&c, reborn), None);
    crate::forget_vm_lock_keys(VM);
}

/// A live real RSA key that a collection moves keeps its handle.
#[test]
fn a_moved_real_rsa_key_keeps_its_handle() {
    const VM: usize = 0xB17C_0008;
    let c = ctx_for(VM);
    let (old, new) = (0x1_B175_0010usize, 0x3_B175_0010usize);
    crypto_impl::rsa_realkey_map_set(&c, obj(old), 0xB17E);
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(crypto_impl::rsa_realkey_map_get(&c, obj(new)), Some(0xB17E));
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(crypto_impl::rsa_realkey_map_get(&c, obj(new)), Some(0xB17E));
    crate::forget_vm_lock_keys(VM);
}
