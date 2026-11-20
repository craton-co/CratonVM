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

/// Two VMs' digests with one identity hash accumulate separately (they
/// shared -- and `digest()`ed -- one buffer), the JIT's by-hash fast path
/// declines on the shared bucket instead of feeding one of them, and A's
/// teardown takes only A's row.
#[test]
fn two_vms_digests_with_one_identity_hash_keep_distinct_accumulators() {
    const VM_A: usize = 0xE18D_0001;
    const VM_B: usize = 0xE18D_0002;
    let _forget = Forget(&[VM_A, VM_B]);
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let (ma, mb) = (obj(0x1_E187_0010), obj(0x2_E187_0010));
    let hash = a.identity_hash_code(ma);
    assert_eq!(hash, b.identity_hash_code(mb));
    write_accumulator(&mut a, ma, &[]);
    // A lone row of A's: the fast path serves A, and never B (w36-a).
    assert!(
        !update_fastpath::append_byte(VM_B, hash, b'?'),
        "VM B's fast path fed VM A's lone digest"
    );
    assert!(update_fastpath::append_byte(VM_A, hash, b'x'));
    append_accumulator(&mut a, ma, b"yz");
    assert!(!accumulator_present(&mut b, mb), "B saw A's accumulator");
    write_accumulator(&mut b, mb, &[]);
    append_accumulator(&mut b, mb, b"B");
    assert!(
        !update_fastpath::append_byte(VM_A, hash, b'!'),
        "the fast path fed one of two digests sharing a hash"
    );
    assert_eq!(read_accumulator(&mut a, ma), b"xyz".to_vec());
    assert_eq!(read_accumulator(&mut b, mb), b"B".to_vec());
    crate::forget_vm_lock_keys(VM_A);
    assert_eq!(read_accumulator(&mut b, mb), b"B".to_vec());
    // B's row is alone in the bucket again: B's fast path serves it, A's
    // does not.
    assert!(!update_fastpath::append_byte(VM_A, hash, b'?'));
    assert!(update_fastpath::append_byte(VM_B, hash, b'2'));
    assert_eq!(read_accumulator(&mut b, mb), b"B2".to_vec());
}

/// gc-common w36-a (`common-w22b-md-update-fastpath-class-id-is-process-wide`):
/// the class verdict is a NAME judged in the asking VM, not an id latched
/// by the first VM. VM A's `MessageDigest` has class id 17 and VM B's has
/// 23, while id 17 in VM B names BouncyCastle's digest subclass: once A has
/// hashed, B's subclass must not be "ours" (the process-wide id made it
/// so, and its bytes were appended to another digest), and B's own
/// `MessageDigest` must be (the latched id kept it off the fast path).
#[test]
fn the_class_verdict_is_judged_per_vm() {
    // Each VM's class table, as the JIT resolves it (`vm/src/jit/helpers.rs
    // ::md_update_class_is_ours` asks its own VM for the class name).
    let vm_a = |id: u32| match id {
        17 => Some("java/security/MessageDigest"),
        _ => None,
    };
    let vm_b = |id: u32| match id {
        17 => Some("org/bouncycastle/jcajce/provider/digest/SHA256$Digest"),
        23 => Some("java/security/MessageDigest"),
        _ => None,
    };
    let ours = |table: &dyn Fn(u32) -> Option<&'static str>, id: u32| {
        table(id).is_some_and(update_fastpath::class_name_is_ours)
    };
    assert!(ours(&vm_a, 17), "A's own MessageDigest");
    assert!(
        !ours(&vm_b, 17),
        "B's provider subclass was judged by A's id"
    );
    assert!(ours(&vm_b, 23), "B's own MessageDigest was refused");
    assert!(!ours(&vm_a, 23));
}

/// A dead digest's row goes with its key: a later digest with its
/// identity hash starts empty, and the fast path finds no bucket.
#[test]
fn a_dead_digests_row_goes_with_its_key() {
    const VM: usize = 0xE18D_0003;
    let _forget = Forget(&[VM]);
    let mut c = ctx_for(VM);
    let dead = obj(0x1_E188_0010);
    let hash = c.identity_hash_code(dead);
    write_accumulator(&mut c, dead, b"half a message");
    assert!(accumulator_present(&mut c, dead));
    let addr = dead.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != addr) >= 1);
    assert!(
        accumulators().lock().get(&hash).is_none(),
        "the dead digest's bytes outlived it"
    );
    assert!(!update_fastpath::append_byte(VM, hash, b'x'));
    let reborn = obj(0x2_E188_0010);
    assert!(!accumulator_present(&mut c, reborn));
    assert!(read_accumulator(&mut c, reborn).is_empty());
}

/// A live digest that a collection moves keeps its accumulator.
#[test]
fn a_moved_digest_keeps_its_accumulator() {
    const VM: usize = 0xE18D_0004;
    let _forget = Forget(&[VM]);
    let mut c = ctx_for(VM);
    let (old, new) = (0x1_E189_0010usize, 0x3_E189_0010usize);
    write_accumulator(&mut c, obj(old), b"before the move");
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    append_accumulator(&mut c, obj(new), b"; after");
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(
        read_accumulator(&mut c, obj(new)),
        b"before the move; after".to_vec()
    );
}
