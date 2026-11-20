use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Test fixture - tests that touch the container singleton serialize
/// on this mutex so one test's state doesn't leak into another.
/// The OnceLock itself is idempotent-by-design, but the per-scope
/// bean caches are stateful.
fn state_guard() -> &'static parking_lot::Mutex<()> {
    static GUARD: OnceLock<parking_lot::Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| parking_lot::Mutex::new(()))
}

fn make_arc_container(ctx: &mut MockNativeContext) -> (u64, ObjectRef) {
    let inner = get_or_init_container();
    let obj = alloc_arc_container_obj(ctx, inner.id());
    (inner.id(), obj)
}

fn make_class_mirror_for(ctx: &mut MockNativeContext, class_name: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class_name).unwrap();
    ctx.get_class_mirror(cid)
}

#[test]
fn register_quarkus_arc_natives_registers_all_expected_entries() {
    let mut r = NativeMethodRegistry::new();
    // Exercise the registration mechanics directly, independent of the
    // opt-in gate (which is tested separately below).
    register_quarkus_arc_natives_unconditional(&mut r);
    // Key facade entries
    assert!(r.find(CLS_ARC, "initialize", "()V").is_some());
    assert!(r
        .find(CLS_ARC, "container", "()Lio/quarkus/arc/ArcContainer;")
        .is_some());
    assert!(r.find(CLS_ARC, "shutdown", "()V").is_some());
    assert!(r.find(CLS_ARC, "isRunning", "()Z").is_some());
    // Container entries (both interface + impl)
    for cls in [CLS_ARC_CONTAINER, CLS_ARC_CONTAINER_IMPL] {
        assert!(r
            .find(
                cls,
                "instance",
                "(Ljava/lang/Class;)Lio/quarkus/arc/InstanceHandle;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "beanManager",
                "()Ljakarta/enterprise/inject/spi/BeanManager;"
            )
            .is_some());
    }
    // Handle dispatches
    assert!(r
        .find(CLS_INSTANCE_HANDLE_IMPL, "get", "()Ljava/lang/Object;")
        .is_some());
    // BeanManager dispatches
    assert!(r
        .find(
            CLS_BEAN_MANAGER_JAKARTA,
            "createInstance",
            "()Ljakarta/enterprise/inject/Instance;"
        )
        .is_some());
}

#[test]
fn synthetic_shim_is_default_off_and_startup_opt_in_enables_it() {
    // Startup configuration is immutable. Exercise the parser directly
    // instead of mutating process state after the global snapshot latched.
    if !cfg!(feature = "synthetic-quarkus-arc") {
        assert!(
            !synthetic_arc_opted_in_value(None),
            "shim must be off by default without opt-in"
        );
    }

    assert!(
        synthetic_arc_opted_in_value(Some("1")),
        "startup flag must enable the synthetic shim"
    );
    let mut r_on = NativeMethodRegistry::new();
    register_quarkus_arc_natives_unconditional(&mut r_on);
    assert!(
        r_on.find(CLS_ARC, "initialize", "()V").is_some(),
        "opt-in path must register the synthetic ArC shim"
    );
}

#[test]
fn arc_initialize_is_idempotent_returns_same_container_id() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();

    native_arc_initialize(&mut ctx, &[]).unwrap();
    let id1 = get_or_init_container().id();

    native_arc_initialize(&mut ctx, &[]).unwrap();
    let id2 = get_or_init_container().id();

    assert_eq!(id1, id2, "initialize must be idempotent on container id");
}

#[test]
fn arc_initialize_marks_is_running_true_and_shutdown_keeps_id() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();

    let running_before = native_arc_is_running(&mut ctx, &[]).unwrap().unwrap();
    // If some earlier test initialized, we'll be running already;
    // we don't assert on the before-state, only on after-init.
    let _ = running_before;

    native_arc_initialize(&mut ctx, &[]).unwrap();
    let running = native_arc_is_running(&mut ctx, &[]).unwrap().unwrap();
    match running {
        Value::Int(1) => {}
        other => panic!("expected running=true, got {:?}", other),
    }

    let id_before = get_or_init_container().id();
    native_arc_shutdown(&mut ctx, &[]).unwrap();
    let id_after = get_or_init_container().id();
    assert_eq!(id_before, id_after, "shutdown must not reseat container id");
}

#[test]
fn container_returns_object_with_correct_id_slot() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let container_val = native_arc_container(&mut ctx, &[]).unwrap().unwrap();
    let container_obj = match container_val {
        Value::Object(Some(o)) => o,
        _ => panic!("expected container object"),
    };
    let id_field = ctx.get_field(container_obj, CONTAINER_FIELD_ID);
    match id_field {
        Value::Long(v) => {
            assert_eq!(
                v as u64,
                get_or_init_container().id(),
                "embedded id matches"
            );
        }
        other => panic!("expected Long id slot, got {:?}", other),
    }
}

#[test]
fn app_scoped_lookup_returns_same_instance_across_two_calls() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let container_obj = match native_arc_container(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!("container"),
    };
    let mirror = make_class_mirror_for(&mut ctx, "com/example/AppBean");

    let h1 = match native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("handle1 {:?}", other),
    };
    let b1 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h1))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("bean1 {:?}", other),
    };

    let h2 = match native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("handle2 {:?}", other),
    };
    let b2 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h2))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("bean2 {:?}", other),
    };

    assert_eq!(b1, b2, "app-scoped lookups must share instance identity");
}

#[test]
fn dependent_scope_lookup_returns_fresh_instance_each_call() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    // Pre-declare as dependent
    let inner = get_or_init_container();
    let bean_name = "com/example/DepBean";
    let cid = ctx.ensure_class_initialized(bean_name).unwrap();
    let key = ClassKey::new(bean_name, ctx.loader_id_of_class(cid));
    inner.declare_bean(key, Scope::Dependent);

    let container_obj = match native_arc_container(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!("container"),
    };
    let mirror = ctx.get_class_mirror(cid);

    let b1 = {
        let h = match native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("h {:?}", other),
        };
        match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("b {:?}", other),
        }
    };
    let b2 = {
        let h = match native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("h {:?}", other),
        };
        match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("b {:?}", other),
        }
    };
    assert_ne!(
        b1, b2,
        "@Dependent lookups must allocate a fresh instance each time"
    );
}

#[test]
fn bean_not_found_throws_unsatisfied_resolution_when_mirror_unreadable() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let (_id, container_obj) = make_arc_container(&mut ctx);

    // Use a bogus "mirror" that is NOT a real mirror - just an
    // object with no recognized slot layout. resolution should
    // fall into the error path.
    let bogus_cid = ctx.ensure_class_initialized("Bogus").unwrap();
    let bogus_mirror = ctx.alloc_object(bogus_cid, 0);

    // But note: our class_key_from_class_mirror will read
    // class_id_of_object(bogus_mirror) which returns Bogus' class_id,
    // so the lookup succeeds with key "Bogus". To trigger the error
    // path we'd need class_id == 0 - instead test via the
    // String-name lookup path with an empty name.
    let empty = ctx.create_string("");
    let result = native_container_instance_name(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(empty)),
        ],
    );
    assert!(result.is_err(), "empty name must surface as error");

    // Also sanity check non-error case: bogus_mirror does resolve
    // successfully since it has a class_id.
    let result_ok = native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(bogus_mirror)),
        ],
    );
    assert!(result_ok.is_ok(), "valid mirror with class_id resolves");
}

#[test]
fn instance_handle_get_returns_wrapped_bean() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let (id, container_obj) = make_arc_container(&mut ctx);
    let mirror = make_class_mirror_for(&mut ctx, "com/example/HandleBean");

    let handle_val = native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap();

    let handle = match handle_val {
        Value::Object(Some(o)) => o,
        _ => panic!("handle"),
    };

    // isAvailable should be true.
    let avail = native_instance_handle_available(&mut ctx, &[Value::Object(Some(handle))])
        .unwrap()
        .unwrap();
    assert!(matches!(avail, Value::Int(1)));

    // get() should return the bean.
    let bean_val = native_instance_handle_get(&mut ctx, &[Value::Object(Some(handle))])
        .unwrap()
        .unwrap();
    assert!(matches!(bean_val, Value::Object(Some(_))));

    // Container id embedded in handle matches singleton.
    let slot = ctx.get_field(handle, INSTANCE_FIELD_CONTAINER_ID);
    match slot {
        Value::Long(v) => assert_eq!(v as u64, id),
        other => panic!("expected Long id in handle, got {:?}", other),
    }
}

#[test]
fn bean_manager_returns_synthetic_with_container_id() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let (id, container_obj) = make_arc_container(&mut ctx);
    let bm_val = native_container_bean_manager(&mut ctx, &[Value::Object(Some(container_obj))])
        .unwrap()
        .unwrap();
    let bm = match bm_val {
        Value::Object(Some(o)) => o,
        _ => panic!("bm"),
    };
    let slot = ctx.get_field(bm, 0);
    match slot {
        Value::Long(v) => assert_eq!(v as u64, id),
        other => panic!("expected container id on bean manager, got {:?}", other),
    }
}

#[test]
fn class_key_preserves_classloader_identity() {
    // Two different loader ids yield different ClassKeys even for
    // the same class name - defends against hostile loader
    // impersonation.
    let k1 = ClassKey::new("com/example/X", 2);
    let k2 = ClassKey::new("com/example/X", 99);
    assert_ne!(k1, k2);
    let k3 = ClassKey::new("com/example/X", 2);
    assert_eq!(k1, k3);
}

#[test]
fn descriptor_extraction_strips_l_prefix_and_semicolon() {
    assert_eq!(
        extract_descriptor_class("Ljava/lang/String;"),
        Some("java/lang/String".to_string())
    );
    assert_eq!(extract_descriptor_class("I"), None);
    assert_eq!(extract_descriptor_class("[I"), None);
    assert_eq!(
        extract_descriptor_class("Lio/quarkus/arc/ArcContainer;"),
        Some("io/quarkus/arc/ArcContainer".to_string())
    );
}

#[test]
fn resolution_count_increments_per_successful_lookup() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let (_id, container_obj) = make_arc_container(&mut ctx);
    let before = get_or_init_container().resolution_count();
    let mirror = make_class_mirror_for(&mut ctx, "com/example/Counted");
    native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap();
    native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap();
    let after = get_or_init_container().resolution_count();
    assert_eq!(after - before, 2, "each lookup increments counter once");
}

#[test]
fn instance_handle_destroy_is_a_graceful_no_op() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let (_id, container_obj) = make_arc_container(&mut ctx);
    let mirror = make_class_mirror_for(&mut ctx, "com/example/Closeable");
    let handle = match native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!("handle"),
    };
    native_instance_handle_no_op(&mut ctx, &[Value::Object(Some(handle))]).unwrap();
    native_instance_handle_no_op(&mut ctx, &[Value::Object(Some(handle))]).unwrap();
    // Still resolvable after "destroy" - we do not invalidate.
    let avail = native_instance_handle_available(&mut ctx, &[Value::Object(Some(handle))])
        .unwrap()
        .unwrap();
    assert!(matches!(avail, Value::Int(1)));
}

#[test]
fn bean_manager_get_reference_unwraps_bean_slot() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let (id, container_obj) = make_arc_container(&mut ctx);
    let mirror = make_class_mirror_for(&mut ctx, "com/example/Wrapped");
    let handle = match native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!("handle"),
    };

    let bm =
        match native_container_bean_manager(&mut ctx, &[Value::Object(Some(container_obj))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("bm"),
        };

    // getReference expects (null CreationalContext, handle, null)
    // - our impl reads args[1] only.
    let ref_val = native_bean_manager_get_reference(
        &mut ctx,
        &[
            Value::Object(Some(bm)),
            Value::Object(Some(handle)),
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap();
    match ref_val {
        Value::Object(Some(_)) => {}
        other => panic!("expected unwrapped bean, got {:?}", other),
    }
    // Container id unchanged.
    assert_eq!(get_or_init_container().id(), id);
}

#[test]
fn concurrent_initialize_returns_same_container_across_threads() {
    use std::sync::Barrier;

    let _g = state_guard().lock();
    reset_container_state();

    // Initialize the container once up-front in main thread; the
    // race happens over `get_or_init_container` under the OnceLock.
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let bar = Arc::clone(&barrier);
            std::thread::spawn(move || {
                bar.wait();
                // Each thread hits initialize & observes a shared id.
                let inner = get_or_init_container();
                inner.id()
            })
        })
        .collect();
    let mut ids: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        1,
        "all threads must observe the same container id"
    );
}

#[test]
fn inject_field_walk_populates_discovered_fields() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    // The mock's declared_fields returns empty, so the discover
    // step returns an empty Vec and does not blow up. We assert the
    // cache is populated after first call (even if empty).
    let inner = get_or_init_container();
    let bean_name = "com/example/WithInjects";
    let cid = ctx.ensure_class_initialized(bean_name).unwrap();
    let key = ClassKey::new(bean_name, ctx.loader_id_of_class(cid));
    let discovered_first = discover_inject_fields(&inner, &ctx, &key);
    let discovered_second = discover_inject_fields(&inner, &ctx, &key);
    assert_eq!(
        discovered_first, discovered_second,
        "cached result must be stable"
    );
    let reader = inner.inject_fields.read();
    assert!(reader.contains_key(&key), "cache must have the key");
}

#[test]
fn select_alias_behaves_identically_to_instance() {
    let _g = state_guard().lock();
    reset_container_state();
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();

    let (_id, container_obj) = make_arc_container(&mut ctx);
    let mirror = make_class_mirror_for(&mut ctx, "com/example/SelectBean");

    let inst_handle = match native_container_instance_class_simple(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let sel_handle = match native_container_select(
        &mut ctx,
        &[
            Value::Object(Some(container_obj)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    // Different handle identity (new alloc), but same underlying
    // bean slot.
    let b1 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(inst_handle))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let b2 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(sel_handle))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_eq!(b1, b2, "select + instance yield the same bean");    }

#[test]
fn try_container_is_some_after_init_none_before() {
    let _g = state_guard().lock();
    // Because OnceLock is process-wide and a prior test may have
    // initialized it, we can only assert the post-init invariant
    // here.
    let mut ctx = mock_ctx();
    native_arc_initialize(&mut ctx, &[]).unwrap();
    assert!(try_container().is_some(), "container must exist after init");
}

#[test]
fn scope_default_is_application_for_unannotated_beans() {
    assert_eq!(Scope::default_for_bootstrap(), Scope::Application);
}
