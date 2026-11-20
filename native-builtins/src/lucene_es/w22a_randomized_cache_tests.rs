use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test uses them.
const W22A_VM_A: usize = 0x0A22_5A08;
const W22A_VM_B: usize = 0x0A22_5B08;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        forget_vm_randomized_caches(W22A_VM_A);
        forget_vm_randomized_caches(W22A_VM_B);
        // gc-common w32-a: the rows' owner lock keys.
        forget_vm_lock_keys(W22A_VM_A);
        forget_vm_lock_keys(W22A_VM_B);
    }
}

/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 4): the three RandomizedRunner caches hold rooted handles per VM
/// only -- a handle that no longer resolves is a miss, never a raw
/// pre-collection address -- and a VM's teardown drops its rows and no
/// other VM's.
#[test]
fn w22a_randomized_caches_are_rooted_per_vm_and_forgotten_with_the_vm() {
    let _forget = Forget;
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(W22A_VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(W22A_VM_B);

    let context_a = a.fresh_object_ref();
    let key_a = RandomizedContextCacheKey {
        vm: W22A_VM_A,
        thread: 1,
        group: 2,
    };
    let key_b = RandomizedContextCacheKey {
        vm: W22A_VM_B,
        thread: 1,
        group: 2,
    };
    // gc-common w31-b: every row names its owners' addresses; these
    // stand-ins are never dereferenced (w32-a: the rows also mint their
    // weak lock keys, which hash and compare the address only).
    // SAFETY: keys only, 8-byte aligned (lesson o).
    let o1 = unsafe { ObjectRef::from_raw(0x22A0_1008usize as *mut u8) };
    let o2 = unsafe { ObjectRef::from_raw(0x22A0_2008usize as *mut u8) };
    let owners = randomized_owners(o1, Some(o2));
    let thread_owner = randomized_owners(o1, None);
    randomized_context_cache_store(&mut a, key_a, context_a, o1);
    assert_eq!(
        randomized_context_cache_lookup(&a, key_a, thread_owner),
        Some(context_a)
    );
    assert_eq!(
        randomized_context_cache_lookup(&b, key_b, thread_owner),
        None
    );
    // A's root is gone: the row is a miss (the old code answered its raw
    // fallback address here).
    let root = randomized_context_cache()
        .lock()
        .get(&key_a)
        .map(|e| e.root)
        .expect("A's context row");
    assert!(a.remove_global_root(root));
    assert_eq!(
        randomized_context_cache_lookup(&a, key_a, thread_owner),
        None
    );

    let random_b = b.fresh_object_ref();
    let random_key_b = RandomizedRandomCacheKey {
        vm: W22A_VM_B,
        context: 3,
        thread: 4,
    };
    randomized_random_cache_store(&mut b, random_key_b, random_b, o1, o2);
    let per_thread_a = a.fresh_object_ref();
    let per_thread_key_a = RandomizedPerThreadCacheKey {
        vm: W22A_VM_A,
        context: 5,
        map: 6,
        thread: 7,
    };
    randomized_per_thread_cache_store(&mut a, per_thread_key_a, per_thread_a, o1, o2);
    assert_eq!(
        randomized_per_thread_cache_lookup(&a, per_thread_key_a, owners),
        Some(per_thread_a)
    );

    forget_vm_randomized_caches(W22A_VM_A);
    assert!(!randomized_context_cache().lock().contains_key(&key_a));
    assert!(!randomized_per_thread_cache()
        .lock()
        .contains_key(&per_thread_key_a));
    assert_eq!(
        randomized_random_cache_lookup(&b, random_key_b, owners),
        Some(random_b),
        "B's rows survive A's teardown"
    );
    forget_vm_randomized_caches(W22A_VM_B);
    assert!(!randomized_random_cache().lock().contains_key(&random_key_b));
}
