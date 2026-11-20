use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Forgets its VM's cache state and lock keys on drop, pass or fail, and
/// only that VM's (lessons c, pp).
pub(super) struct VmGuard(pub(super) usize);

impl Drop for VmGuard {
    fn drop(&mut self) {
        forget_vm_infinispan_state(self.0);
        crate::forget_vm_lock_keys(self.0);
    }
}

fn fresh_cache(name: &str, limit: usize) -> Arc<CacheInner> {
    let m = DefaultCacheManagerInner::new();
    let _ = m.define_configuration(
        name,
        PendingConfig {
            size_limit: limit,
            default_ttl: None,
        },
    );
    m.get_cache(name).unwrap()
}

#[test]
fn t19_10_put_get_roundtrip() {
    let cache = fresh_cache("realms", 1024);
    cache.put(
        CacheKey::from_str("realm-master"),
        StoredValue::Str("master-value".to_string()),
    );
    match cache.get(&CacheKey::from_str("realm-master")) {
        Some(StoredValue::Str(s)) => assert_eq!(s, "master-value"),
        other => panic!("unexpected: {other:?}"),
    }
    let (puts, gets, hits, _) = cache.stats();
    assert_eq!(puts, 1);
    assert_eq!(gets, 1);
    assert_eq!(hits, 1);
}

#[test]
fn t19_10_remove_returns_previous_value() {
    let cache = fresh_cache("users", 16);
    cache.put(
        CacheKey::from_str("u1"),
        StoredValue::Primitive(Value::Int(42)),
    );
    let prev = cache.remove(&CacheKey::from_str("u1"));
    assert!(matches!(prev, Some(StoredValue::Primitive(Value::Int(42)))));
    assert!(cache.get(&CacheKey::from_str("u1")).is_none());
}

#[test]
fn t19_10_eviction_on_size_limit() {
    let cache = fresh_cache("sessions", 3);
    for i in 0..5 {
        cache.put(
            CacheKey::from_bytes(&(i as i32).to_le_bytes()),
            StoredValue::Primitive(Value::Int(i)),
        );
    }
    // The oldest two keys (0, 1) must have been evicted.
    assert_eq!(cache.size(), 3);
    let (_, _, _, evictions) = cache.stats();
    assert!(evictions >= 2, "expected >=2 evictions, got {evictions}");
    assert!(cache
        .get(&CacheKey::from_bytes(&0i32.to_le_bytes()))
        .is_none());
    assert!(cache
        .get(&CacheKey::from_bytes(&4i32.to_le_bytes()))
        .is_some());
}

#[test]
fn t19_10_ttl_expiry() {
    let cache = Arc::new(CacheInner::new(
        "expiring".to_string(),
        16,
        Some(Duration::from_millis(50)),
    ));
    cache.put(
        CacheKey::from_str("k"),
        StoredValue::Primitive(Value::Int(1)),
    );
    assert!(cache.get(&CacheKey::from_str("k")).is_some());
    std::thread::sleep(Duration::from_millis(70));
    assert!(
        cache.get(&CacheKey::from_str("k")).is_none(),
        "TTL should have expired"
    );
    // Expiry path must also have incremented eviction counter.
    let (_, _, _, evictions) = cache.stats();
    assert!(evictions >= 1);
}

#[test]
fn t19_10_listener_fires_on_put() {
    let cache = fresh_cache("listened", 4);
    let mut ctx = mock_ctx();
    let listener_obj = ctx.alloc_object(ClassId::new(0), 1);
    let idx = cache.add_listener(VmCaches::default().root(&mut ctx, listener_obj).unwrap());
    assert_eq!(idx, 0);
    cache.put(
        CacheKey::from_str("k"),
        StoredValue::Primitive(Value::Int(1)),
    );
    let count = cache.listeners.lock().len();
    assert_eq!(count, 1, "listener still registered after put");
}

#[test]
fn t19_10_listener_panic_does_not_corrupt_cache() {
    let cache = fresh_cache("panicky", 4);
    // We can't easily stash a panic closure through the ListenerHandle
    // (it's an ObjectRef) but we can prove the catch_unwind guard in
    // dispatch() won't propagate. Verify that after a listener is
    // registered and a put fires dispatch, the store's state is
    // still consistent.
    let mut ctx = mock_ctx();
    let listener_obj = ctx.alloc_object(ClassId::new(0), 1);
    cache.add_listener(VmCaches::default().root(&mut ctx, listener_obj).unwrap());
    cache.put(
        CacheKey::from_str("k1"),
        StoredValue::Primitive(Value::Int(1)),
    );
    cache.put(
        CacheKey::from_str("k2"),
        StoredValue::Primitive(Value::Int(2)),
    );
    assert_eq!(cache.size(), 2);
    assert!(cache.get(&CacheKey::from_str("k1")).is_some());
    assert!(cache.get(&CacheKey::from_str("k2")).is_some());
}

#[test]
fn t19_10_concurrent_put_get_eight_threads() {
    let cache = fresh_cache("concurrent", 1024);
    let cache_shared = cache.clone();
    let mut handles = vec![];
    for t in 0..8 {
        let c = cache_shared.clone();
        let h = std::thread::spawn(move || {
            for i in 0..128 {
                let key = format!("t{t}-k{i}");
                c.put(
                    CacheKey::from_str(&key),
                    StoredValue::Primitive(Value::Int(i)),
                );
                let got = c.get(&CacheKey::from_str(&key));
                assert!(got.is_some(), "thread {t} failed to read back {key}");
            }
        });
        handles.push(h);
    }
    for h in handles {
        h.join().unwrap();
    }
    // Every insert should be present (1024 = exactly the limit).
    assert_eq!(cache.size(), 8 * 128);
}

#[test]
fn t19_10_cache_name_validation_rejects_bad_names() {
    // Empty
    assert!(validate_cache_name("").is_err());
    // Path traversal
    assert!(validate_cache_name("../etc/passwd").is_err());
    assert!(validate_cache_name("foo/../bar").is_err());
    // Reserved separators
    assert!(validate_cache_name("foo/bar").is_err());
    assert!(validate_cache_name("foo\\bar").is_err());
    assert!(validate_cache_name("C:\\Windows").is_err());
    // Control bytes
    assert!(validate_cache_name("foo\0bar").is_err());
    assert!(validate_cache_name("foo\nbar").is_err());
    assert!(validate_cache_name("foo\x1fbar").is_err());
    // Length
    let huge = "a".repeat(256);
    assert!(validate_cache_name(&huge).is_err());
    // Positives
    assert!(validate_cache_name("realms").is_ok());
    assert!(validate_cache_name("user-sessions").is_ok());
    assert!(validate_cache_name("authenticationSessions").is_ok());
    assert!(validate_cache_name("a.b.c").is_ok());
}

#[test]
fn t19_10_get_cache_is_idempotent() {
    let m = DefaultCacheManagerInner::new();
    let a = m.get_cache("idempotent").unwrap();
    let b = m.get_cache("idempotent").unwrap();
    assert!(Arc::ptr_eq(&a, &b), "getCache must return the same Arc");
}

#[test]
fn t19_10_get_cache_different_configs_errors() {
    let m = DefaultCacheManagerInner::new();
    // Start with limit=10.
    m.define_configuration(
        "conflict",
        PendingConfig {
            size_limit: 10,
            default_ttl: None,
        },
    )
    .unwrap();
    let _first = m.get_cache("conflict").unwrap();
    // Re-defining with a different limit must be rejected.
    let err = m
        .define_configuration(
            "conflict",
            PendingConfig {
                size_limit: 1000,
                default_ttl: None,
            },
        )
        .unwrap_err();
    assert!(err.contains("already configured"), "got: {err}");
}

#[test]
fn t19_10_cache_key_equality_across_constructors() {
    let k1 = CacheKey::from_str("hello");
    let k2 = CacheKey::from_bytes(b"hello");
    assert_eq!(k1, k2);
    let mut map: HashMap<CacheKey, i32> = HashMap::new();
    map.insert(k1.clone(), 1);
    assert_eq!(map.get(&k2), Some(&1));
}

#[test]
fn t19_10_clear_empties_but_keeps_cache_registered() {
    let m = DefaultCacheManagerInner::new();
    let cache = m.get_cache("to-clear").unwrap();
    for i in 0..5 {
        cache.put(
            CacheKey::from_bytes(&(i as i32).to_le_bytes()),
            StoredValue::Primitive(Value::Int(i)),
        );
    }
    assert_eq!(cache.size(), 5);
    cache.clear();
    assert_eq!(cache.size(), 0);
    assert_eq!(m.cache_count(), 1);
}

#[test]
fn t19_10_remove_listener_succeeds_only_when_registered() {
    let cache = fresh_cache("listener-dedup", 8);
    let mut ctx = mock_ctx();
    let state = VmCaches::default();
    let l1 = ctx.alloc_object(ClassId::new(0), 1);
    let l2 = ctx.alloc_object(ClassId::new(0), 1);
    cache.add_listener(state.root(&mut ctx, l1).unwrap());
    assert!(cache.remove_listener(&ctx, l1));
    assert!(
        !cache.remove_listener(&ctx, l2),
        "never-registered listener"
    );
    assert_eq!(cache.listeners.lock().len(), 0);
    // The removed listener's root is released by the next operation.
    assert_eq!(ctx.global_root_count(), 1);
    assert_eq!(state.release_pending(&mut ctx), 1);
    assert_eq!(ctx.global_root_count(), 0);
}

#[test]
fn t19_10_put_if_absent_preserves_existing() {
    let cache = fresh_cache("put-if-absent", 8);
    cache.put(
        CacheKey::from_str("k"),
        StoredValue::Primitive(Value::Int(1)),
    );
    // Simulate putIfAbsent: peek first.
    let already = cache.get(&CacheKey::from_str("k"));
    assert!(already.is_some());
    // Only overwrite if None — we purposely don't.
    match cache.get(&CacheKey::from_str("k")) {
        Some(StoredValue::Primitive(Value::Int(1))) => {}
        other => panic!("got: {other:?}"),
    }
}

#[test]
fn t19_10_evict_updates_lru_order() {
    let cache = fresh_cache("lru", 3);
    cache.put(
        CacheKey::from_str("a"),
        StoredValue::Primitive(Value::Int(1)),
    );
    cache.put(
        CacheKey::from_str("b"),
        StoredValue::Primitive(Value::Int(2)),
    );
    cache.put(
        CacheKey::from_str("c"),
        StoredValue::Primitive(Value::Int(3)),
    );
    // Re-access `a` so it's the MRU.
    cache.get(&CacheKey::from_str("a"));
    // Fourth insert should now evict `b` (LRU).
    cache.put(
        CacheKey::from_str("d"),
        StoredValue::Primitive(Value::Int(4)),
    );
    assert!(cache.get(&CacheKey::from_str("a")).is_some());
    assert!(
        cache.get(&CacheKey::from_str("b")).is_none(),
        "b should have been evicted"
    );
    assert!(cache.get(&CacheKey::from_str("c")).is_some());
    assert!(cache.get(&CacheKey::from_str("d")).is_some());
}

#[test]
fn t19_10_native_registration_smoke() {
    let mut r = NativeMethodRegistry::new();
    register_infinispan_natives(&mut r);
    assert!(r
        .find(
            CLS_MANAGER,
            "getCache",
            "(Ljava/lang/String;)Lorg/infinispan/Cache;"
        )
        .is_some());
    assert!(r
        .find(
            CLS_CACHE_IMPL,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
        )
        .is_some());
    assert!(r.find(CLS_CACHE, "size", "()I").is_some());
    assert!(r
        .find(CLS_CACHE, "addListener", "(Ljava/lang/Object;)V")
        .is_some());
}

#[test]
fn t19_10_define_configuration_size_limit_respected() {
    let m = DefaultCacheManagerInner::new();
    m.define_configuration(
        "tiny",
        PendingConfig {
            size_limit: 2,
            default_ttl: None,
        },
    )
    .unwrap();
    let cache = m.get_cache("tiny").unwrap();
    cache.put(
        CacheKey::from_str("a"),
        StoredValue::Primitive(Value::Int(1)),
    );
    cache.put(
        CacheKey::from_str("b"),
        StoredValue::Primitive(Value::Int(2)),
    );
    cache.put(
        CacheKey::from_str("c"),
        StoredValue::Primitive(Value::Int(3)),
    );
    assert_eq!(cache.size(), 2, "size_limit=2 must be honoured");
}

/// Regression test for the `ConfigurationBuilder.build()`
/// `ClassCastException` fix
/// (keycloak-model-infinispan-configurationbuilder-classcastexception.md,
/// now fixed): `native_dcm_define_configuration` must read size/ttl off
/// a `Configuration` object through its real accessor API
/// (`memory().maxCount()` / `expiration().lifespan()`, both via
/// `invoke_virtual`) rather than a raw synthetic slot index. This test
/// stands in for a real `Configuration` object with a mock whose
/// `invoke_virtual` hook only understands those four real method names —
/// it has NO synthetic slots at all, so if the native regressed back to
/// `ctx.get_field(obj, N)` this test's mock would return `Value::Int(0)`
/// (the mock's zeroed default) for every field read, silently passing
/// with the wrong (default) values instead of the scripted 777/45000 —
/// so the test also asserts the values are NOT the size/ttl defaults.
#[test]
fn t19_10_define_configuration_reads_via_real_accessor_api_not_raw_slots() {
    fn hook(
        ctx: &mut crate::test_utils::MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        match method_name {
            "memory" => Some(Ok(Some(Value::Object(Some(ctx.fresh_object_ref()))))),
            "maxCount" => Some(Ok(Some(Value::Long(777)))),
            "expiration" => Some(Ok(Some(Value::Object(Some(ctx.fresh_object_ref()))))),
            "lifespan" => Some(Ok(Some(Value::Long(45_000)))),
            _ => None,
        }
    }

    const VM: usize = 0x30A0_0101;
    let _vm = VmGuard(VM);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    ctx.set_invoke_virtual_hook(hook);

    let name_obj = ctx.create_string("real-cfg-cache");
    // Stand-in for the real `Configuration` object real
    // `ConfigurationBuilder.build()` bytecode would construct — its own
    // fields are irrelevant since the fix reads through `invoke_virtual`,
    // never `get_field`, on this object.
    let config_obj = ctx.fresh_object_ref();

    let result = native_dcm_define_configuration(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(name_obj)),
            Value::Object(Some(config_obj)),
        ],
    );
    assert!(
        result.is_ok(),
        "defineConfiguration must succeed: {result:?}"
    );

    let cache = vm_caches(&ctx).manager.get_cache("real-cfg-cache").unwrap();
    assert_eq!(
        cache.size_limit, 777,
        "size_limit must come from memory().maxCount(), not a default/garbage slot read"
    );
    assert_eq!(
        cache.default_ttl,
        Some(Duration::from_millis(45_000)),
        "default_ttl must come from expiration().lifespan(), not a default/garbage slot read"
    );
    assert_ne!(cache.size_limit, DEFAULT_SIZE_LIMIT);
}

fn real_dcm_define_configuration_hook(
    ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match (method_name, descriptor) {
        (
            "read",
            "(Lorg/infinispan/configuration/cache/Configuration;)Lorg/infinispan/configuration/cache/ConfigurationBuilder;",
        ) => {
            assert_eq!(args.len(), 1);
            ctx.set_field(receiver, 0, args[0]);
            Some(Ok(Some(Value::Object(Some(receiver)))))
        }
        ("isTemplate", "()Z") => Some(Ok(Some(Value::Int(0)))),
        (
            "template",
            "(Z)Lorg/infinispan/configuration/cache/ConfigurationBuilder;",
        ) => {
            assert_eq!(args.len(), 1);
            ctx.set_field(receiver, 1, args[0]);
            Some(Ok(Some(Value::Object(Some(receiver)))))
        }
        (
            "putConfiguration",
            "(Ljava/lang/String;Lorg/infinispan/configuration/cache/ConfigurationBuilder;)Lorg/infinispan/configuration/cache/Configuration;",
        ) => {
            assert_eq!(args.len(), 2);
            let builder = match args[1] {
                Value::Object(Some(o)) => o,
                other => panic!("expected builder object, got {other:?}"),
            };
            ctx.set_field(receiver, 0, args[0]);
            ctx.set_field(receiver, 1, args[1]);
            Some(Ok(Some(ctx.get_field(builder, 0))))
        }
        _ => None,
    }
}

#[test]
fn t19_10_real_dcm_define_configuration_delegates_to_real_configuration_manager() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(real_dcm_define_configuration_hook);

    let manager_class = ctx.ensure_class_initialized(CLS_MANAGER).unwrap();
    let this = ctx.alloc_object(manager_class, 4);
    let global_component_registry = ctx.fresh_object_ref();
    let configuration_manager = ctx.fresh_object_ref();
    ctx.set_field_by_name(
        this,
        "globalComponentRegistry",
        Value::Object(Some(global_component_registry)),
    );
    ctx.set_field_by_name(
        this,
        "configurationManager",
        Value::Object(Some(configuration_manager)),
    );

    let name = ctx.create_string("___protobuf_metadata");
    let config = ctx.fresh_object_ref();
    let result = native_dcm_define_configuration(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(name)),
            Value::Object(Some(config)),
        ],
    )
    .expect("real DefaultCacheManager defineConfiguration should delegate");

    assert_eq!(result, Some(Value::Object(Some(config))));
    assert_eq!(
        ctx.get_field(configuration_manager, 0),
        Value::Object(Some(name)),
        "real manager path must pass the cache name to ConfigurationManager"
    );
    let builder = match ctx.get_field(configuration_manager, 1) {
        Value::Object(Some(o)) => o,
        other => panic!("expected builder passed to putConfiguration, got {other:?}"),
    };
    assert_eq!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(builder))
            .as_deref(),
        Some(CLS_CONFIG_BUILDER)
    );
    assert_eq!(
        ctx.get_field(builder, 0),
        Value::Object(Some(config)),
        "builder.read(Configuration) must receive the original real configuration"
    );
    assert_eq!(
        ctx.get_field(builder, 1),
        Value::Int(0),
        "builder.template(false) should mirror the real config template bit"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

fn real_dcm_cache_exists_hook(
    ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match (method_name, descriptor) {
        ("selectCache", "(Ljava/lang/String;)Ljava/lang/String;") => {
            assert_eq!(args.len(), 1);
            ctx.set_field(receiver, 0, args[0]);
            let selected = ctx.create_string("___protobuf_metadata");
            Some(Ok(Some(Value::Object(Some(selected)))))
        }
        ("containsKey", "(Ljava/lang/Object;)Z") => {
            assert_eq!(args.len(), 1);
            ctx.set_field(receiver, 0, args[0]);
            Some(Ok(Some(Value::Int(1))))
        }
        _ => None,
    }
}

#[test]
fn t19_10_real_dcm_cache_exists_uses_real_alias_resolution_and_cache_map() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(real_dcm_cache_exists_hook);

    let manager_class = ctx.ensure_class_initialized(CLS_MANAGER).unwrap();
    let this = ctx.alloc_object(manager_class, 4);
    let global_component_registry = ctx.fresh_object_ref();
    let configuration_manager = ctx.fresh_object_ref();
    let caches = ctx.fresh_object_ref();
    ctx.set_field_by_name(
        this,
        "globalComponentRegistry",
        Value::Object(Some(global_component_registry)),
    );
    ctx.set_field_by_name(
        this,
        "configurationManager",
        Value::Object(Some(configuration_manager)),
    );
    ctx.set_field_by_name(this, "caches", Value::Object(Some(caches)));

    let alias = ctx.create_string("protobuf-alias");
    let result = native_dcm_cache_exists(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(alias))],
    )
    .expect("real DefaultCacheManager cacheExists should delegate");

    assert_eq!(result, Some(Value::Int(1)));
    assert_eq!(
        ctx.get_field(configuration_manager, 0),
        Value::Object(Some(alias)),
        "cacheExists must ask ConfigurationManager.selectCache for aliases"
    );
    let selected = match ctx.get_field(caches, 0) {
        Value::Object(Some(o)) => o,
        other => panic!("expected selected cache name passed to containsKey, got {other:?}"),
    };
    assert_eq!(
        ctx.read_string(selected).as_deref(),
        Some("___protobuf_metadata")
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

#[test]
fn t19_10_define_configuration_rejects_bad_name() {
    let m = DefaultCacheManagerInner::new();
    let err = m
        .define_configuration(
            "../evil",
            PendingConfig {
                size_limit: 4,
                default_ttl: None,
            },
        )
        .unwrap_err();
    assert!(err.contains("'../'"), "got: {err}");
}

#[test]
fn t19_10_stored_value_roundtrip_through_ctx() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let state = VmCaches::default();
    let stored = StoredValue::from_value(&mut ctx, &state, Value::Object(Some(s))).unwrap();
    match &stored {
        StoredValue::Str(t) => assert_eq!(t, "hello"),
        other => panic!("expected Str, got: {other:?}"),
    }
    let back = stored.to_value(&mut ctx);
    match back {
        Value::Object(Some(obj)) => {
            assert_eq!(ctx.read_string(obj).as_deref(), Some("hello"));
        }
        other => panic!("expected Object, got: {other:?}"),
    }
}

#[test]
fn t19_10_cache_names_listing() {
    let m = DefaultCacheManagerInner::new();
    m.get_cache("a").unwrap();
    m.get_cache("b").unwrap();
    m.get_cache("c").unwrap();
    let mut names = m.cache_names();
    names.sort();
    assert_eq!(
        names,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}
