use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// One VM per test (a sweep judges every slot of its VM): the first test
// uses `VM_A`, the second `VM_B` and `VM_C`.
const VM_A: usize = 0x32A0_1A08;
const VM_B: usize = 0x32A0_1B08;
const VM_C: usize = 0x32A0_1C08;

/// Forgets exactly the VMs its test used.
struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_randomized_caches(vm);
            forget_vm_lock_keys(vm);
        }
    }
}

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key only; hashed and compared by address, never
    // dereferenced (8-byte aligned, lesson o).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(obj: ObjectRef) -> usize {
    obj.as_ptr() as usize
}

fn per_thread_root(key: &RandomizedPerThreadCacheKey) -> Option<usize> {
    randomized_per_thread_cache()
        .lock()
        .get(key)
        .map(|row| row.root)
}

#[test]
fn a_dead_threads_rows_go_at_the_sweep_and_their_roots_at_the_next_store() {
    let _forget = Forget(&[VM_A]);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_A);
    let base = c.global_root_count();
    let (context, map) = (at(0x32A0_2008), at(0x32A0_3008));
    let dead = at(0x1_32A0_4008);
    let live = at(0x32A0_5008);
    // `twin` shares `dead`'s hash (4 GiB apart) and stays alive.
    let twin = at(0x2_32A0_4008);
    assert_eq!(
        c.identity_hash_code(dead),
        c.identity_hash_code(twin),
        "premise"
    );

    // The dead thread's three rows: per-thread resources, Random, context.
    let dead_key = randomized_per_thread_key(&c, context, map, dead);
    let dead_resources = c.fresh_object_ref();
    randomized_per_thread_cache_store(&mut c, dead_key, dead_resources, context, dead);
    let random_key = RandomizedRandomCacheKey {
        vm: VM_A,
        context: c.identity_hash_code(context),
        thread: c.identity_hash_code(dead),
    };
    let dead_random = c.fresh_object_ref();
    randomized_random_cache_store(&mut c, random_key, dead_random, context, dead);
    let context_key = RandomizedContextCacheKey {
        vm: VM_A,
        thread: c.identity_hash_code(dead),
        group: 0x32A0_6008,
    };
    let dead_context_obj = c.fresh_object_ref();
    randomized_context_cache_store(&mut c, context_key, dead_context_obj, dead);

    // A live thread's row.
    let live_key = randomized_per_thread_key(&c, context, map, live);
    assert!(live_key != dead_key, "premise: distinct hashes");
    let live_resources = c.fresh_object_ref();
    randomized_per_thread_cache_store(&mut c, live_key, live_resources, context, live);
    assert_eq!(c.global_root_count(), base + 4);

    let dead_roots = [
        per_thread_root(&dead_key).expect("dead per-thread row"),
        randomized_random_cache()
            .lock()
            .get(&random_key)
            .map(|row| row.root)
            .expect("dead Random row"),
        randomized_context_cache()
            .lock()
            .get(&context_key)
            .map(|row| row.root)
            .expect("dead context row"),
    ];

    // A collection finds `dead` gone (and everything else alive).
    let dropped = crate::gc_sweep_lock_keys(VM_A, &|a| a != addr(dead));
    assert_eq!(dropped, 1, "only the dead thread's key is freed");
    assert!(
        per_thread_root(&dead_key).is_none(),
        "dead per-thread row kept"
    );
    assert!(!randomized_random_cache().lock().contains_key(&random_key));
    assert!(!randomized_context_cache().lock().contains_key(&context_key));
    assert_eq!(
        randomized_per_thread_cache_lookup(
            &c,
            live_key,
            randomized_owners(context, Some(live))
        ),
        Some(live_resources),
        "a live thread's row survives"
    );
    // The sweep has no context: the roots are queued, still allocated.
    assert_eq!(c.global_root_count(), base + 4);

    // The next store releases them; `twin`, same hash as `dead`, files a
    // row of its own under the key `dead`'s row had.
    let twin_resources = c.fresh_object_ref();
    randomized_per_thread_cache_store(&mut c, dead_key, twin_resources, context, twin);
    for root in dead_roots {
        assert_eq!(
            c.resolve_global_root(root),
            None,
            "dead row's root {root} kept"
        );
    }
    assert_eq!(c.global_root_count(), base + 2);
    assert_eq!(
        randomized_per_thread_cache_lookup(
            &c,
            dead_key,
            randomized_owners(context, Some(twin))
        ),
        Some(twin_resources)
    );
    assert_eq!(
        randomized_per_thread_cache_lookup(
            &c,
            dead_key,
            randomized_owners(context, Some(dead))
        ),
        None,
        "a dead thread is never served its same-hash successor's row"
    );

    // A second sweep that finds `dead` gone again frees nothing of `twin`'s:
    // its row names `twin`'s own key.
    crate::gc_sweep_lock_keys(VM_A, &|a| a != addr(dead));
    assert!(per_thread_root(&dead_key).is_some(), "twin's row dropped");

    // The context dies: every row it owns goes (the live thread's too).
    crate::gc_sweep_lock_keys(VM_A, &|a| a != addr(context));
    assert!(per_thread_root(&live_key).is_none());
    assert!(per_thread_root(&dead_key).is_none());
    randomized_release_deferred(&mut c);
    assert_eq!(c.global_root_count(), base);
}

#[test]
fn a_sweep_of_one_vm_leaves_another_vms_rows() {
    let _forget = Forget(&[VM_B, VM_C]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_B);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_C);
    // The same stand-in addresses in both VMs: each VM mints its own keys.
    let (context, map, thread) = (at(0x32A0_7008), at(0x32A0_8008), at(0x32A0_9008));
    let key_a = randomized_per_thread_key(&a, context, map, thread);
    let key_b = randomized_per_thread_key(&b, context, map, thread);
    assert!(key_a != key_b, "premise: rows are per VM");
    let resources_a = a.fresh_object_ref();
    let resources_b = b.fresh_object_ref();
    randomized_per_thread_cache_store(&mut a, key_a, resources_a, context, thread);
    randomized_per_thread_cache_store(&mut b, key_b, resources_b, context, thread);
    let owners = randomized_owners(context, Some(thread));

    // Everything of `a`'s VM dies.
    crate::gc_sweep_lock_keys(VM_B, &|_| false);
    assert!(per_thread_root(&key_a).is_none(), "A's row kept");
    assert_eq!(
        randomized_per_thread_cache_lookup(&b, key_b, owners),
        Some(resources_b),
        "B's row went with A's sweep"
    );
    // A's root is released through A's context, never B's.
    let b_roots = b.global_root_count();
    randomized_release_deferred(&mut b);
    assert_eq!(b.global_root_count(), b_roots);
    let a_roots = a.global_root_count();
    randomized_release_deferred(&mut a);
    assert_eq!(a.global_root_count(), a_roots - 1);
}

// `native_randomized_context_push`: the relocation test. Only that test
// touches these.
const VM_D: usize = 0x32A0_1D08;
static OLD_RANDOMNESS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static NEW_RANDOMNESS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static RESOURCES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PUSHED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn push_hook(
    ctx: &mut crate::test_utils::MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    use std::sync::atomic::Ordering::Relaxed;
    match method {
        // `perThreadResources.get(thread)` is Java: a collection moves
        // the `Randomness` being pushed.
        "get" => {
            ctx.remap_native_pin_addr_for_test(
                OLD_RANDOMNESS.load(Relaxed),
                NEW_RANDOMNESS.load(Relaxed),
            );
            Some(Ok(Some(Value::Object(Some(at(RESOURCES.load(Relaxed)))))))
        }
        "push" => {
            if let Some(Value::Object(Some(pushed))) = args.first() {
                PUSHED.store(addr(*pushed), Relaxed);
            }
            Some(Ok(None))
        }
        _ => None,
    }
}

fn declare_field(
    ctx: &crate::test_utils::MockNativeContext,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) {
    ctx.set_declared_fields(
        class_id,
        vec![cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: class_id,
            is_static: false,            }],
    );
}

/// gc-common w32-a: `RandomizedContext.push` held the `Randomness` raw
/// across `getPerThread` (Java) and pushed its entry address.
#[test]
fn push_hands_the_deque_the_randomness_current_address() {
    use std::sync::atomic::Ordering::Relaxed;
    let _forget = Forget(&[VM_D]);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_D);
    let context_class = ClassId::new(0x32A0_0D01);
    let resources_class = ClassId::new(0x32A0_0D02);
    declare_field(&c, context_class, "perThreadResources", "Ljava/util/Map;");
    declare_field(
        &c,
        resources_class,
        "randomnesses",
        "Ljava/util/ArrayDeque;",
    );
    let context = c.alloc_object(context_class, 4);
    let resources = c.alloc_object(resources_class, 4);
    let map = c.fresh_object_ref();
    let deque = c.fresh_object_ref();
    c.set_field_by_name(context, "perThreadResources", Value::Object(Some(map)));
    c.set_field_by_name(resources, "randomnesses", Value::Object(Some(deque)));
    let randomness = c.fresh_object_ref();
    OLD_RANDOMNESS.store(addr(randomness), Relaxed);
    // The moved copy keeps the low 32 bits, i.e. the identity hash
    // (lesson qq). Never dereferenced: the hook only records it.
    NEW_RANDOMNESS.store(addr(randomness) + (1usize << 40), Relaxed);
    RESOURCES.store(addr(resources), Relaxed);
    c.set_invoke_virtual_hook(push_hook);
    let pins = c.native_pin_count_for_test();

    native_randomized_context_push(
        &mut c,
        &[
            Value::Object(Some(context)),
            Value::Object(Some(randomness)),
        ],
    )
    .expect("push");

    assert_eq!(
        PUSHED.load(Relaxed),
        NEW_RANDOMNESS.load(Relaxed),
        "push handed the deque the Randomness's pre-`getPerThread` address"
    );
    assert_eq!(c.native_pin_count_for_test(), pins, "push leaked a pin");
}
