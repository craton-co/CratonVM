use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::Value;

fn make_runtime_value_obj(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CLS_RUNTIME_VALUE).unwrap();
    ctx.alloc_object(cid, 2)
}

#[test]
fn register_quarkus_staticinit_natives_registers_all_expected_entries() {
    let mut r = NativeMethodRegistry::new();
    register_quarkus_staticinit_natives(&mut r);

    // Representative entries across each sub-class.
    assert!(r
        .find(CLS_RUNTIME_VALUE, "getValue", "()Ljava/lang/Object;")
        .is_some());
    assert!(r
        .find(CLS_RUNTIME_VALUE, "<init>", "(Ljava/lang/Object;)V")
        .is_some());
    assert!(r
        .find(CLS_APPLICATION_CONFIG, "name", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(CLS_DATASOURCE_CONFIG, "jdbcUrl", "()Ljava/lang/String;")
        .is_some());
    assert!(r.find(CLS_TIMING, "mainStarted", "()V").is_some());
}

#[test]
fn t19_3_runtime_value_wraps_pre_materialized_value() {
    let mut ctx = mock_ctx();
    let rv = make_runtime_value_obj(&mut ctx);
    let payload = ctx.create_string("hello");

    native_rv_init_value(
        &mut ctx,
        &[Value::Object(Some(rv)), Value::Object(Some(payload))],
    )
    .unwrap();

    let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
        .unwrap()
        .unwrap();
    match got {
        Value::Object(Some(o)) => assert_eq!(o, payload),
        _ => panic!("expected payload ObjectRef, got {:?}", got),
    }
}

#[test]
fn t19_3_runtime_value_lazy_supplier_invoked_on_first_get() {
    let mut ctx = mock_ctx();
    let rv = make_runtime_value_obj(&mut ctx);
    let supplier_cid = ctx.ensure_class_initialized("TestSupplier").unwrap();
    let supplier_obj = ctx.alloc_object(supplier_cid, 1);

    // Prime invoke_virtual to return a specific String on first call.
    let supplier_result = ctx.create_string("lazy-materialized");
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(supplier_result)))));
    }

    native_rv_init_supplier(
        &mut ctx,
        &[Value::Object(Some(rv)), Value::Object(Some(supplier_obj))],
    )
    .unwrap();

    // First getValue → supplier invoked
    let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
        .unwrap()
        .unwrap();
    match got {
        Value::Object(Some(o)) => assert_eq!(o, supplier_result),
        _ => panic!("expected supplier result, got {:?}", got),
    }
    // And the materialized value now lives in field 0.
    match ctx.get_field(rv, RV_FIELD_VALUE) {
        Value::Object(Some(o)) => assert_eq!(o, supplier_result),
        other => panic!("expected field populated, got {:?}", other),
    }
}

#[test]
fn t19_3_runtime_value_caches_after_first_get() {
    let mut ctx = mock_ctx();
    let rv = make_runtime_value_obj(&mut ctx);
    let supplier_cid = ctx.ensure_class_initialized("TestSupplier").unwrap();
    let supplier_obj = ctx.alloc_object(supplier_cid, 1);

    // First call returns A; second call would return B — but we
    // expect the second call to be short-circuited and never dispatch.
    let first_result = ctx.create_string("first");
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(first_result)))));
    }

    native_rv_init_supplier(
        &mut ctx,
        &[Value::Object(Some(rv)), Value::Object(Some(supplier_obj))],
    )
    .unwrap();

    let first = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
        .unwrap()
        .unwrap();

    // Prime a DIFFERENT invoke_virtual_result; if the second getValue
    // dispatched to the supplier it would pick this up. Since it
    // should hit the cached value, we end up with the first result.
    let second_result = ctx.create_string("second");
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(second_result)))));
    }

    let second = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
        .unwrap()
        .unwrap();

    assert_eq!(
        first, second,
        "second getValue must return cached first result"
    );
    // And the queued second_result is still waiting (never consumed).
    let queued = unsafe { &*ctx.invoke_virtual_result.get() };
    assert!(
        queued.is_some(),
        "second getValue must not have dispatched to invoke_virtual"
    );
}

#[test]
fn t19_3_application_config_defaults_from_env_fallback() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized(CLS_APPLICATION_CONFIG)
        .unwrap();
    let ac = ctx.alloc_object(cid, 2);
    native_ac_init(&mut ctx, &[Value::Object(Some(ac))]).unwrap();

    // Without any env/property: name() returns null.
    let n = native_ac_name(&mut ctx, &[Value::Object(Some(ac))])
        .unwrap()
        .unwrap();
    match n {
        Value::Object(None) => {}
        other => panic!("expected null name, got {:?}", other),
    }

    // With a system property fallback registered:
    ctx.set_system_property("quarkus.application.name", "keycloak");
    let n2 = native_ac_name(&mut ctx, &[Value::Object(Some(ac))])
        .unwrap()
        .unwrap();
    let s = match n2 {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        other => panic!("expected populated name, got {:?}", other),
    };
    assert_eq!(s, "keycloak");

    // With an explicit field value: the field wins over the fallback.
    let explicit = ctx.create_string("explicit-app");
    ctx.set_field(ac, AC_FIELD_NAME, Value::Object(Some(explicit)));
    let n3 = native_ac_name(&mut ctx, &[Value::Object(Some(ac))])
        .unwrap()
        .unwrap();
    match n3 {
        Value::Object(Some(o)) => assert_eq!(o, explicit),
        other => panic!("expected explicit name, got {:?}", other),
    }
}

#[test]
fn t19_3_data_source_config_jdbc_url_from_recorded_or_env() {
    let mut ctx = mock_ctx();
    let cid = ctx.ensure_class_initialized(CLS_DATASOURCE_CONFIG).unwrap();
    let ds = ctx.alloc_object(cid, 4);
    native_dsrc_init(&mut ctx, &[Value::Object(Some(ds))]).unwrap();

    // No field value and no env fallback visible (DATABASE_URL etc
    // won't be set in test env) — native returns null.
    let u = native_dsrc_jdbc_url(&mut ctx, &[Value::Object(Some(ds))])
        .unwrap()
        .unwrap();
    match u {
        Value::Object(None) => {}
        Value::Object(Some(o)) => {
            // If the CI host happens to have DATABASE_URL set, accept
            // that too — just verify the native returned a usable
            // Java String wrapper.
            let s = ctx.read_string(o).unwrap_or_default();
            assert!(!s.is_empty(), "DATABASE_URL from env must be non-empty");
        }
        other => panic!("unexpected jdbcUrl return: {:?}", other),
    }

    // Field set wins over env.
    let recorded = ctx.create_string("jdbc:h2:mem:test");
    ctx.set_field(ds, DSRC_FIELD_JDBC_URL, Value::Object(Some(recorded)));
    let u2 = native_dsrc_jdbc_url(&mut ctx, &[Value::Object(Some(ds))])
        .unwrap()
        .unwrap();
    match u2 {
        Value::Object(Some(o)) => assert_eq!(o, recorded),
        other => panic!("expected recorded jdbcUrl, got {:?}", other),
    }

    // Username / password / driver getters all work off the same
    // pattern — one combined smoke check.
    let user = ctx.create_string("sa");
    ctx.set_field(ds, DSRC_FIELD_USERNAME, Value::Object(Some(user)));
    let u3 = native_dsrc_username(&mut ctx, &[Value::Object(Some(ds))])
        .unwrap()
        .unwrap();
    match u3 {
        Value::Object(Some(o)) => assert_eq!(o, user),
        other => panic!("expected username, got {:?}", other),
    }
}

#[test]
fn t19_3_timing_phases_no_op_but_return_gracefully() {
    let mut ctx = mock_ctx();
    // All Timing natives take a receiver-less descriptor — we just
    // verify each one returns `Ok(None)` rather than panicking.
    assert!(
        native_timing_static_init_started(&mut ctx, &[Value::Int(1)])
            .unwrap()
            .is_none()
    );
    assert!(native_timing_static_init_stopped(&mut ctx, &[])
        .unwrap()
        .is_none());
    assert!(native_timing_main_started(&mut ctx, &[]).unwrap().is_none());
    assert!(native_timing_print_startup_time(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Int(0),
            Value::Int(0),
        ]
    )
    .unwrap()
    .is_none());

    // This VM's Timing row must hold every phase's timestamp.
    let vm = ctx.vm_identity();
    let slots = timing_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
        .copied()
        .unwrap_or([0; 4]);
    assert!(
        slots[TIMING_FIELD_BOOT_START] > 0,
        "boot_start slot should hold a non-zero nanosecond timestamp"
    );
    assert!(
        slots[TIMING_FIELD_BOOT_STOP] > 0,
        "boot_stop slot should hold a non-zero nanosecond timestamp"
    );
    assert!(
        slots[TIMING_FIELD_MAIN_START] > 0,
        "main_start slot should hold a non-zero nanosecond timestamp"
    );
    assert!(
        slots[TIMING_FIELD_MAIN_STOP] > 0,
        "main_stop slot should hold a non-zero nanosecond timestamp"
    );
}

#[test]
fn t19_3_application_lifecycle_natives_are_graceful_no_ops() {
    let mut ctx = mock_ctx();
    assert!(native_app_lifecycle_no_op(&mut ctx, &[]).unwrap().is_none());
}

#[test]
fn t19_3_runtime_value_null_supplier_returns_null_gracefully() {
    // Supplier returning null must NOT panic — record + return null
    // per the mission-spec error-handling rule.
    let mut ctx = mock_ctx();
    let rv = make_runtime_value_obj(&mut ctx);
    let supplier_cid = ctx.ensure_class_initialized("TestSupplier").unwrap();
    let supplier_obj = ctx.alloc_object(supplier_cid, 1);

    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
    }
    native_rv_init_supplier(
        &mut ctx,
        &[Value::Object(Some(rv)), Value::Object(Some(supplier_obj))],
    )
    .unwrap();

    let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
        .unwrap()
        .unwrap();
    assert!(
        matches!(got, Value::Object(None)),
        "null-returning supplier must propagate null"
    );
}

// -----------------------------------------------------------------
// T19.H3: bootstrap-runner replacement natives
// -----------------------------------------------------------------

#[test]
fn t19_h3_serialized_application_read_returns_populated_sa() {
    let mut ctx = mock_ctx();
    let sa = match native_serialized_application_read(
        &mut ctx,
        &[Value::Object(None), Value::Object(None)],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected SerializedApplication ObjectRef, got {:?}", other),
    };
    // main class populated.
    let name = match ctx.get_field(sa, SA_FIELD_MAIN_CLASS) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        other => panic!("expected String, got {:?}", other),
    };
    assert_eq!(name, "org.keycloak.quarkus.runtime.KeycloakMain");
    // runner classloader populated (non-null synthetic).
    assert!(matches!(
        ctx.get_field(sa, SA_FIELD_RUNNER_CL),
        Value::Object(Some(_))
    ));
}

#[test]
fn t19_h3_serialized_application_getters_round_trip() {
    let mut ctx = mock_ctx();
    let sa = match native_serialized_application_read(&mut ctx, &[])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let mc = native_serialized_application_get_main_class(&mut ctx, &[Value::Object(Some(sa))])
        .unwrap()
        .unwrap();
    let name = match mc {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => panic!(),
    };
    assert_eq!(name, "org.keycloak.quarkus.runtime.KeycloakMain");

    let cl = native_serialized_application_get_runner_cl(&mut ctx, &[Value::Object(Some(sa))])
        .unwrap()
        .unwrap();
    assert!(matches!(cl, Value::Object(Some(_))));
}

#[test]
fn t19_h3_serialized_application_get_main_class_throws_on_empty_field() {
    let mut ctx = mock_ctx();
    // Build an SA whose main-class field was never populated (i.e. it
    // did NOT come through `read()`). Previously this fabricated the
    // compile-time Keycloak main; now it must fail loud rather than
    // silently launch an app-specific default.
    let cid = ctx.ensure_class_initialized(CLS_SERIALIZED_APP).unwrap();
    let sa = ctx.alloc_object(cid, 2);
    // Leave both slots at default (Value::Int(0) from MockNativeContext).
    let err =
        native_serialized_application_get_main_class(&mut ctx, &[Value::Object(Some(sa))])
            .unwrap_err();
    match err {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::IllegalStateException { .. },
            ),
        ) => {}
        other => panic!("expected IllegalStateException, got {other:?}"),
    }
}

#[test]
fn t19_h3_sanitize_main_class_candidate_accepts_and_rejects() {
    assert_eq!(
        sanitize_main_class_candidate("com.example.App").as_deref(),
        Some("com.example.App")
    );
    assert_eq!(
        sanitize_main_class_candidate("com/example/App").as_deref(),
        Some("com/example/App")
    );
    assert_eq!(
        sanitize_main_class_candidate("Outer$Inner$Run").as_deref(),
        Some("Outer$Inner$Run")
    );
    assert!(sanitize_main_class_candidate("").is_none());
    assert!(sanitize_main_class_candidate(";evil").is_none());
    assert!(sanitize_main_class_candidate("../traversal").is_none());
    assert!(sanitize_main_class_candidate("spaces here").is_none());
    assert!(sanitize_main_class_candidate(&"a".repeat(513)).is_none());
    // Newlines blocked.
    assert!(sanitize_main_class_candidate("foo\nbar").is_none());
}

#[test]
fn t19_h3_quarkus_bootstrap_constants_match_runtime_jar() {
    // Values cross-checked by disassembling
    // `io.quarkus.quarkus-bootstrap-runner-3.20.0.jar` —
    // SerializedApplication.MAGIC / .VERSION. Locking them in
    // here catches a future Quarkus upgrade that silently changes
    // the constants.
    assert_eq!(QUARKUS_BOOTSTRAP_MAGIC, -265202638);
    assert_eq!(QUARKUS_BOOTSTRAP_VERSION, 2);
}

// -----------------------------------------------------------------
// T19.H5: RunnerClassLoader.loadClass sanitizer tests
// -----------------------------------------------------------------

#[test]
fn t19_h5_sanitize_load_class_name_accepts_binary_names() {
    assert_eq!(
        sanitize_load_class_name("java.lang.String").as_deref(),
        Some("java.lang.String")
    );
    assert_eq!(
        sanitize_load_class_name("java/lang/String").as_deref(),
        Some("java/lang/String")
    );
    assert_eq!(
        sanitize_load_class_name("com.example.Foo$Inner").as_deref(),
        Some("com.example.Foo$Inner")
    );
}

#[test]
fn t19_h5_sanitize_load_class_name_rejects_dangerous_inputs() {
    assert!(sanitize_load_class_name("").is_none());
    assert!(sanitize_load_class_name(&"a".repeat(513)).is_none());
    assert!(sanitize_load_class_name("/absolute").is_none());
    assert!(sanitize_load_class_name("../../etc/passwd").is_none());
    assert!(sanitize_load_class_name("foo bar").is_none()); // spaces
    assert!(sanitize_load_class_name("semi;colon").is_none());
    assert!(sanitize_load_class_name("has\nnewline").is_none());
    assert!(sanitize_load_class_name("has\0nul").is_none());
    assert!(sanitize_load_class_name("has\\backslash").is_none());
}

#[test]
fn t19_h5_runner_class_loader_load_class_happy_path_returns_mirror() {
    let mut ctx = mock_ctx();
    // Allocate a fake RunnerClassLoader instance (the `this` arg).
    let rcl_cid = ctx
        .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
        .unwrap();
    let rcl = ctx.alloc_object(rcl_cid, 1);
    // Pre-load a class in the mock so ensure_class_initialized returns Ok.
    ctx.ensure_class_initialized("com/example/App").unwrap();
    let name = ctx.create_string("com.example.App");
    let result = native_runner_class_loader_load_class(
        &mut ctx,
        &[Value::Object(Some(rcl)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(mirror)) => {
            // The mirror's name field (slot 1 in mock) should echo the class name.
            match ctx.get_field(mirror, 1) {
                Value::Object(Some(s)) => {
                    // Mock's get_class_mirror stores the internal name.
                    assert_eq!(ctx.read_string(s).as_deref(), Some("com/example/App"));
                }
                _ => panic!("mirror name slot empty"),
            }
        }
        other => panic!("expected Class mirror, got {other:?}"),
    }
}

#[test]
fn t19_h5_runner_class_loader_load_class_null_name_throws_npe() {
    let mut ctx = mock_ctx();
    let rcl_cid = ctx
        .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
        .unwrap();
    let rcl = ctx.alloc_object(rcl_cid, 1);
    let err = native_runner_class_loader_load_class(
        &mut ctx,
        &[Value::Object(Some(rcl)), Value::Object(None)],
    )
    .unwrap_err();
    match err {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { .. },
            ),
        ) => {}
        other => panic!("expected NPE, got {other:?}"),
    }
}

#[test]
fn t19_h5_runner_class_loader_load_class_rejects_bad_name() {
    let mut ctx = mock_ctx();
    let rcl_cid = ctx
        .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
        .unwrap();
    let rcl = ctx.alloc_object(rcl_cid, 1);
    let bad = ctx.create_string("../../etc/passwd");
    let err = native_runner_class_loader_load_class(
        &mut ctx,
        &[Value::Object(Some(rcl)), Value::Object(Some(bad))],
    )
    .unwrap_err();
    match err {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::ClassNotFoundException { class_name },
            ),
        ) => {
            assert_eq!(class_name, "../../etc/passwd");
        }
        other => panic!("expected CNFE, got {other:?}"),
    }
}

#[test]
fn t19_h5_runner_class_loader_load_class_normalizes_dots() {
    // The internal VM uses slash-delimited names; the input uses dots.
    // Verify the translation happens before ensure_class_initialized.
    let mut ctx = mock_ctx();
    let rcl_cid = ctx
        .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
        .unwrap();
    let rcl = ctx.alloc_object(rcl_cid, 1);
    // Pre-load using the slash form.
    ctx.ensure_class_initialized("org/keycloak/quarkus/runtime/KeycloakMain")
        .unwrap();
    let name = ctx.create_string("org.keycloak.quarkus.runtime.KeycloakMain");
    let res = native_runner_class_loader_load_class(
        &mut ctx,
        &[Value::Object(Some(rcl)), Value::Object(Some(name))],
    );
    assert!(res.is_ok(), "dots should normalize to slashes");
}

#[test]
fn t19_h5_runner_class_loader_load_class_with_resolve_delegates() {
    let mut ctx = mock_ctx();
    let rcl_cid = ctx
        .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
        .unwrap();
    let rcl = ctx.alloc_object(rcl_cid, 1);
    ctx.ensure_class_initialized("a/b/C").unwrap();
    let name = ctx.create_string("a.b.C");
    let result = native_runner_class_loader_load_class_with_resolve(
        &mut ctx,
        &[
            Value::Object(Some(rcl)),
            Value::Object(Some(name)),
            Value::Int(1), // resolve=true
        ],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(result, Value::Object(Some(_))));
}
