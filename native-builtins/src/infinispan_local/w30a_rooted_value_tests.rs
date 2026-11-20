use super::tests::VmGuard;
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn ctx_for(vm: usize) -> MockNativeContext {
    let ctx = mock_ctx();
    ctx.set_vm_identity(vm);
    ctx
}

fn o(obj: ObjectRef) -> Value {
    Value::Object(Some(obj))
}

/// A synthetic `Cache` object named `name`, built through the natives
/// (`new DefaultCacheManager()` + `getCache(name)`).
fn synthetic_cache_obj(ctx: &mut MockNativeContext, name: &str) -> ObjectRef {
    let mgr_cid = ctx.ensure_class_initialized(CLS_MANAGER).unwrap();
    let mgr = ctx.alloc_object(mgr_cid, 4);
    native_dcm_init(ctx, &[o(mgr)]).unwrap();
    let name_obj = ctx.create_string(name);
    match native_dcm_get_cache(ctx, &[o(mgr), o(name_obj)]).unwrap() {
        Some(Value::Object(Some(cache))) => cache,
        other => panic!("getCache answered {other:?}"),
    }
}

fn put(ctx: &mut MockNativeContext, cache: ObjectRef, key: &str, value: Value) -> Value {
    let k = ctx.create_string(key);
    native_cache_put(ctx, &[o(cache), o(k), value])
        .unwrap()
        .unwrap()
}

fn get(ctx: &mut MockNativeContext, cache: ObjectRef, key: &str) -> Value {
    let k = ctx.create_string(key);
    native_cache_get(ctx, &[o(cache), o(k)]).unwrap().unwrap()
}

/// Any cache operation: releases the roots queued since the last one.
fn next_op(ctx: &mut MockNativeContext, cache: ObjectRef) {
    native_cache_size(ctx, &[o(cache)]).unwrap();
}

/// The global-root handle of the Java value filed under `key`.
fn entry_handle(ctx: &MockNativeContext, cache: &str, key: &str) -> usize {
    let cache = vm_caches(ctx).manager.get_cache(cache).unwrap();
    let store = cache.store.read();
    match &store.entries.get(&CacheKey::from_str(key)).unwrap().value {
        StoredValue::JavaRef(root) => root.handle(),
        other => panic!("expected a rooted Java value, got {other:?}"),
    }
}

#[test]
fn a_cached_value_is_a_root_and_reads_back_at_its_new_address() {
    const VM: usize = 0x30A0_0201;
    let _vm = VmGuard(VM);
    let mut ctx = ctx_for(VM);
    let cache = synthetic_cache_obj(&mut ctx, "w30a-reloc");
    let value = ctx.fresh_object_ref();
    let roots = ctx.global_root_count();
    assert_eq!(put(&mut ctx, cache, "k", o(value)), Value::Object(None));
    assert_eq!(ctx.global_root_count(), roots + 1, "the value is rooted");
    let handle = entry_handle(&ctx, "w30a-reloc", "k");
    assert_eq!(ctx.resolve_global_root(handle), Some(value));
    // A moving collection re-addresses the root; the entry keeps the handle.
    let moved = ctx.fresh_object_ref();
    assert!(ctx.w30a_retarget_global_root_for_test(handle, moved));
    assert_eq!(get(&mut ctx, cache, "k"), o(moved));
}

#[test]
fn displaced_evicted_removed_and_expired_roots_are_released_by_the_next_operation() {
    const VM: usize = 0x30A0_0202;
    let _vm = VmGuard(VM);
    let mut ctx = ctx_for(VM);
    let limit_one = PendingConfig {
        size_limit: 1,
        default_ttl: None,
    };
    vm_caches(&ctx)
        .manager
        .define_configuration("w30a-one", limit_one)
        .unwrap();
    let cache = synthetic_cache_obj(&mut ctx, "w30a-one");
    let base = ctx.global_root_count();

    // Displacement: `put` hands the previous value back, current, and its
    // root survives until the next operation.
    let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    put(&mut ctx, cache, "k", o(a));
    assert_eq!(put(&mut ctx, cache, "k", o(b)), o(a));
    assert_eq!(ctx.global_root_count(), base + 2);
    next_op(&mut ctx, cache);
    assert_eq!(ctx.global_root_count(), base + 1, "a's root went");

    // Eviction (limit 1): no context at eviction time; queued.
    let c = ctx.fresh_object_ref();
    put(&mut ctx, cache, "k2", o(c));
    assert_eq!(
        get(&mut ctx, cache, "k"),
        Value::Object(None),
        "k was evicted"
    );
    assert_eq!(ctx.global_root_count(), base + 1, "b's root went");

    // Removal hands the value back, then the root goes.
    let k2 = ctx.create_string("k2");
    assert_eq!(
        native_cache_remove(&mut ctx, &[o(cache), o(k2)]).unwrap(),
        Some(o(c))
    );
    next_op(&mut ctx, cache);
    assert_eq!(ctx.global_root_count(), base);

    // TTL expiry.
    let ttl = PendingConfig {
        size_limit: 16,
        default_ttl: Some(Duration::from_millis(1)),
    };
    vm_caches(&ctx)
        .manager
        .define_configuration("w30a-ttl", ttl)
        .unwrap();
    let expiring = synthetic_cache_obj(&mut ctx, "w30a-ttl");
    let d = ctx.fresh_object_ref();
    put(&mut ctx, expiring, "k", o(d));
    assert_eq!(ctx.global_root_count(), base + 1);
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(get(&mut ctx, expiring, "k"), Value::Object(None), "expired");
    next_op(&mut ctx, expiring);
    assert_eq!(
        ctx.global_root_count(),
        base,
        "the expired value's root went"
    );
}

#[test]
fn put_if_absent_keeps_the_live_value_and_releases_the_rejected_root() {
    const VM: usize = 0x30A0_0203;
    let _vm = VmGuard(VM);
    let mut ctx = ctx_for(VM);
    let cache = synthetic_cache_obj(&mut ctx, "w30a-pia");
    let base = ctx.global_root_count();
    let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let k = ctx.create_string("k");
    assert_eq!(
        native_cache_put_if_absent(&mut ctx, &[o(cache), o(k), o(a)]).unwrap(),
        Some(Value::Object(None))
    );
    let k = ctx.create_string("k");
    assert_eq!(
        native_cache_put_if_absent(&mut ctx, &[o(cache), o(k), o(b)]).unwrap(),
        Some(o(a)),
        "the live value wins"
    );
    next_op(&mut ctx, cache);
    assert_eq!(ctx.global_root_count(), base + 1, "b was never stored");
    assert_eq!(get(&mut ctx, cache, "k"), o(a));
}

#[test]
fn two_vms_with_one_cache_name_stay_separate_and_teardown_forgets_one() {
    const VM_A: usize = 0x30A0_0204;
    const VM_B: usize = 0x30A0_0205;
    let _a = VmGuard(VM_A);
    let _b = VmGuard(VM_B);
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let cache_a = synthetic_cache_obj(&mut a, "w30a-shared");
    let cache_b = synthetic_cache_obj(&mut b, "w30a-shared");
    let va = a.fresh_object_ref();
    put(&mut a, cache_a, "k", o(va));
    assert_eq!(
        get(&mut b, cache_b, "k"),
        Value::Object(None),
        "B sees none of A's entries"
    );
    let vb = b.create_string("from-b");
    put(&mut b, cache_b, "k", o(vb));
    assert_eq!(
        get(&mut a, cache_a, "k"),
        o(va),
        "A's entry is untouched by B's put"
    );
    assert!(!Arc::ptr_eq(
        &vm_caches(&a).manager.get_cache("w30a-shared").unwrap(),
        &vm_caches(&b).manager.get_cache("w30a-shared").unwrap()
    ));

    forget_vm_infinispan_state(VM_A);
    assert!(!VM_CACHES.has_row(VM_A), "A's caches went with A");
    assert!(VM_CACHES.has_row(VM_B), "B's did not");
    match get(&mut b, cache_b, "k") {
        Value::Object(Some(s)) => assert_eq!(b.read_string(s).as_deref(), Some("from-b")),
        other => panic!("B's entry must survive A's teardown, got {other:?}"),
    }
}

#[test]
fn an_entry_under_a_dead_key_object_is_dropped_by_the_lock_key_sweep() {
    const VM: usize = 0x30A0_0206;
    let _vm = VmGuard(VM);
    let mut ctx = ctx_for(VM);
    let cache = synthetic_cache_obj(&mut ctx, "w30a-identity");
    let base = ctx.global_root_count();
    let (key_obj, value) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    native_cache_put(&mut ctx, &[o(cache), o(key_obj), o(value)]).unwrap();
    assert_eq!(
        native_cache_get(&mut ctx, &[o(cache), o(key_obj)]).unwrap(),
        Some(o(value))
    );
    let inner = vm_caches(&ctx).manager.get_cache("w30a-identity").unwrap();
    assert_eq!(inner.size(), 1);

    // The key object dies: every weak slot of this VM is judged dead.
    crate::gc_sweep_lock_keys(VM, &|_| false);
    assert_eq!(inner.size(), 0, "the unreachable entry went with its key");
    assert_eq!(ctx.global_root_count(), base + 1, "its root is only queued");
    next_op(&mut ctx, cache);
    assert_eq!(
        ctx.global_root_count(),
        base,
        "and released by the next operation"
    );
}

#[test]
fn a_lookup_does_not_mint_a_key_for_an_object_never_filed() {
    const VM: usize = 0x30A0_0207;
    let _vm = VmGuard(VM);
    let mut ctx = ctx_for(VM);
    let cache = synthetic_cache_obj(&mut ctx, "w30a-lookup");
    let stranger = ctx.fresh_object_ref();
    assert_eq!(
        native_cache_get(&mut ctx, &[o(cache), o(stranger)]).unwrap(),
        Some(Value::Object(None))
    );
    assert_eq!(crate::existing_weak_lock_key(&ctx, stranger), None);
}

#[test]
fn remove_listener_finds_a_listener_that_moved() {
    const VM: usize = 0x30A0_0208;
    let _vm = VmGuard(VM);
    let mut ctx = ctx_for(VM);
    let cache = synthetic_cache_obj(&mut ctx, "w30a-listener");
    let base = ctx.global_root_count();
    let listener = ctx.fresh_object_ref();
    native_cache_add_listener(&mut ctx, &[o(cache), o(listener)]).unwrap();
    assert_eq!(ctx.global_root_count(), base + 1, "the listener is rooted");
    let inner = vm_caches(&ctx).manager.get_cache("w30a-listener").unwrap();
    let handle = inner.listeners.lock()[0].root.handle();
    let moved = ctx.fresh_object_ref();
    assert!(ctx.w30a_retarget_global_root_for_test(handle, moved));
    native_cache_remove_listener(&mut ctx, &[o(cache), o(moved)]).unwrap();
    assert_eq!(inner.listeners.lock().len(), 0);
    next_op(&mut ctx, cache);
    assert_eq!(ctx.global_root_count(), base);
}

#[test]
fn the_lru_queue_stays_bounded_under_repeated_gets() {
    let m = DefaultCacheManagerInner::new();
    let cache = m.get_cache("w30a-lru").unwrap();
    for i in 0..4 {
        cache.put(
            CacheKey::from_bytes(&(i as i32).to_le_bytes()),
            StoredValue::Primitive(Value::Int(i)),
        );
    }
    for round in 0..1_000 {
        let key = CacheKey::from_bytes(&((round % 4) as i32).to_le_bytes());
        assert!(cache.get(&key).is_some());
    }
    let store = cache.store.read();
    assert!(
        store.lru.len() <= 2 * store.entries.len() + 33,
        "{}",
        store.lru.len()
    );
}
