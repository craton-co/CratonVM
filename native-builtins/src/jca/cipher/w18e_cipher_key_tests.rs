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

fn put(ctx: &mut dyn NativeContext, c: ObjectRef, algo: &str, key: &[u8]) {
    let k = obj_key(ctx, c);
    with_table_write(|t| {
        t.insert(
            k,
            CipherState {
                algorithm: algo.to_string(),
                mode: 1,
                key_bytes: key.to_vec(),
                ..Default::default()
            },
        );
    });
}

fn mode_and_key(ctx: &mut dyn NativeContext, c: ObjectRef) -> Option<(i32, Vec<u8>)> {
    let k = obj_key(ctx, c);
    with_table_read(|t| t.get(&k).map(|s| (s.mode, s.key_bytes.clone())))
}

fn row_present(k: CipherKey) -> bool {
    with_table_read(|t| t.contains_key(&k))
}

/// Two VMs' `Cipher`s with one identity hash keep distinct rows, and one
/// VM's teardown takes only its own (the old `(vm, hash)` row was never
/// removed, not even at teardown).
#[test]
fn two_vms_ciphers_with_one_identity_hash_keep_distinct_rows() {
    const VM_A: usize = 0xE18C_0001;
    const VM_B: usize = 0xE18C_0002;
    let _forget = Forget(&[VM_A, VM_B]);
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let (ca, cb) = (obj(0x1_E180_0010), obj(0x2_E180_0010));
    assert_eq!(a.identity_hash_code(ca), b.identity_hash_code(cb));
    put(&mut a, ca, "AES/GCM/NoPadding", b"key-of-A");
    assert_eq!(mode_and_key(&mut b, cb), None, "B read A's cipher state");
    put(&mut b, cb, "AES/CBC/PKCS5Padding", b"key-of-B");
    let (ka, kb) = (obj_key(&mut a, ca), obj_key(&mut b, cb));
    assert_ne!(ka, kb);
    assert_eq!(mode_and_key(&mut a, ca), Some((1, b"key-of-A".to_vec())));
    crate::forget_vm_lock_keys(VM_A);
    assert!(
        !row_present(ka),
        "A's teardown left its row, key bytes included"
    );
    assert!(row_present(kb), "A's teardown took B's row");
    assert_eq!(mode_and_key(&mut b, cb), Some((1, b"key-of-B".to_vec())));
}

/// A dead `Cipher`'s row (key bytes, IV, buffered input) goes with its
/// key, and a later `Cipher` with its identity hash inherits nothing (the
/// old key found the dead row by hash, GCM / ChaCha20 reuse history
/// included).
#[test]
fn a_dead_ciphers_row_goes_with_its_key() {
    const VM: usize = 0xE18C_0003;
    let _forget = Forget(&[VM]);
    let mut c = ctx_for(VM);
    let dead = obj(0x1_E181_0010);
    put(&mut c, dead, "ChaCha20-Poly1305", b"secret-key-bytes");
    let kd = obj_key(&mut c, dead);
    assert!(row_present(kd));
    let addr = dead.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != addr) >= 1);
    assert!(!row_present(kd), "the dead Cipher's key bytes outlived it");
    let reborn = obj(0x2_E181_0010);
    assert_ne!(obj_key(&mut c, reborn), kd);
    assert_eq!(
        mode_and_key(&mut c, reborn),
        None,
        "a later Cipher inherited the dead one's state"
    );
}

/// A live `Cipher` that a collection moves keeps its row.
#[test]
fn a_moved_cipher_keeps_its_row() {
    const VM: usize = 0xE18C_0004;
    let _forget = Forget(&[VM]);
    let mut c = ctx_for(VM);
    let (old, new) = (0x1_E182_0010usize, 0x3_E182_0010usize);
    put(&mut c, obj(old), "AES/GCM/NoPadding", b"moved-key");
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(
        mode_and_key(&mut c, obj(new)),
        Some((1, b"moved-key".to_vec()))
    );
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(
        mode_and_key(&mut c, obj(new)),
        Some((1, b"moved-key".to_vec()))
    );
}
