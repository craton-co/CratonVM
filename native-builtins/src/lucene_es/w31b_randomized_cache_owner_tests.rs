use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const VM: usize = 0x31B0_1E08;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        forget_vm_randomized_caches(VM);
        forget_vm_lock_keys(VM); // gc-common w32-a: the owners' keys
    }
}

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key only; hashed and compared by address, never
    // dereferenced (8-byte aligned, lesson o).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[test]
fn a_colliding_thread_or_context_is_not_served_another_ones_row() {
    let _forget = Forget;
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM);
    let (context, map) = (at(0x31B0_2008), at(0x31B0_3008));
    let (t1, t2) = (at(0x1_31B0_4008), at(0x2_31B0_4008));

    // Per-thread resources: two threads, one hash, one context.
    let k1 = randomized_per_thread_key(&c, context, map, t1);
    let k2 = randomized_per_thread_key(&c, context, map, t2);
    assert!(k1 == k2, "premise: the colliding threads share a key");
    let resources = c.fresh_object_ref();
    randomized_per_thread_cache_store(&mut c, k1, resources, context, t1);
    assert_eq!(
        randomized_per_thread_cache_lookup(&c, k1, randomized_owners(context, Some(t1))),
        Some(resources)
    );
    assert_eq!(
        randomized_per_thread_cache_lookup(&c, k2, randomized_owners(context, Some(t2))),
        None,
        "t2 was served t1's PerThreadResources"
    );
    // A moved thread misses (its caller recomputes and re-files).
    assert_eq!(
        randomized_per_thread_cache_lookup(
            &c,
            k1,
            randomized_owners(context, Some(at(0x3_31B0_4008))),
        ),
        None
    );

    // Random: two contexts, one hash, one thread.
    let (c1, c2) = (at(0x1_31B0_5008), at(0x2_31B0_5008));
    assert_eq!(
        c.identity_hash_code(c1),
        c.identity_hash_code(c2),
        "premise"
    );
    let key = RandomizedRandomCacheKey {
        vm: VM,
        context: c.identity_hash_code(c1),
        thread: c.identity_hash_code(t1),
    };
    let random = c.fresh_object_ref();
    randomized_random_cache_store(&mut c, key, random, c1, t1);
    assert_eq!(
        randomized_random_cache_lookup(&c, key, randomized_owners(c1, Some(t1))),
        Some(random)
    );
    assert_eq!(
        randomized_random_cache_lookup(&c, key, randomized_owners(c2, Some(t1))),
        None,
        "context c2 was served c1's Random"
    );
}
