use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn t19_2_a_deployment_unit_get_name_returns_interned_arc_str() {
    let du = DeploymentUnit::new("keycloak-server.war");
    let n1 = du.get_name();
    let n2 = du.get_name();
    // Interned `Arc<str>` — same pointer each time.
    assert!(
        Arc::ptr_eq(&n1, &n2),
        "get_name() should return identical Arc<str> refs"
    );
    assert_eq!(&*n1, "keycloak-server.war");
}

#[test]
fn t19_2_a_services_deployment_unit_name_matches_convention() {
    let sn = wildfly_deployment_unit_name("my-app.war");
    assert_eq!(sn.canonical(), "jboss.deployment.unit.my-app.war");
    // The base constant — `Services.JBOSS_DEPLOYMENT_UNIT`.
    let base = jboss_deployment_unit_base();
    assert_eq!(base.canonical(), "jboss.deployment.unit");
    // And a derived unit must be a child of the base.
    let base2 = sn.parent().and_then(|p| p.parent()).unwrap();
    assert_eq!(base2.canonical(), "jboss.deployment");
}

/// B2/B6 gating contract:
///   * `ControlledProcessState` getState is an honest bridge → always
///     registered.
///   * The state-transition *setter* no-op shims (B6) that mask the
///     AtomicStampedReference VarHandle gap are only registered under the
///     default-OFF `app-stubs` feature; in the default build they are
///     absent so real bytecode runs.
///   * `AsyncFutureTask.await` (B2) is always registered (it has honest
///     paths) but tagged `SyntheticStub` so the audit census flags it.
#[test]
fn b2_b6_wildfly_stub_gating_contract() {
    let mut r = NativeMethodRegistry::new();
    // Deterministic regardless of the CRATONVM_NO_STUBS env: keep
    // SyntheticStub registrations so we can assert the tag + presence.
    r.set_drop_synthetic_stubs(false);
    register_wildfly_core_natives(&mut r);

    let cps = "org/jboss/as/controller/ControlledProcessState";
    // getState is an honest bridge — present in every build.
    assert!(
        r.find(
            cps,
            "getState",
            "()Lorg/jboss/as/controller/ControlledProcessState$State;",
        )
        .is_some(),
        "ControlledProcessState.getState must always be registered"
    );

    // B6: the setStarting no-op shim is gated on `app-stubs`.
    let set_starting = r.find(cps, "setStarting", "()V");
    if cfg!(feature = "app-stubs") {
        assert!(
            set_starting.is_some(),
            "with app-stubs, setStarting shim should be registered"
        );
        assert_eq!(
            r.kind_of(cps, "setStarting", "()V"),
            Some(cratonvm_native_api::NativeKind::SyntheticStub),
            "setStarting shim must be tagged SyntheticStub"
        );
    } else {
        assert!(
            set_starting.is_none(),
            "default build must NOT register the setStarting shim (real bytecode runs)"
        );
    }

    // B2: await is always registered but tagged SyntheticStub.
    assert_eq!(
        r.kind_of(
            "org/jboss/threads/AsyncFutureTask",
            "await",
            "()Lorg/jboss/threads/AsyncFuture$Status;",
        ),
        Some(cratonvm_native_api::NativeKind::SyntheticStub),
        "AsyncFutureTask.await must be tagged SyntheticStub for the audit"
    );
}

#[test]
fn t19_2_a_thread_factory_builds_jboss_named_thread() {
    let cfg = QueueExecutorConfig::new("ee");
    let exec = EnhancedQueueExecutor::new(cfg);
    let b = exec.new_thread_builder();
    // `std::thread::Builder` doesn't expose its name publicly — the
    // only way to verify is to spawn and query inside. So do that:
    // the thread stashes its name into a channel.
    let (tx, rx) = std::sync::mpsc::channel();
    b.spawn(move || {
        let name = std::thread::current()
            .name()
            .unwrap_or("<anon>")
            .to_string();
        tx.send(name).unwrap();
    })
    .unwrap();
    let observed = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    assert!(
        observed.starts_with("jboss-ee-"),
        "expected jboss-ee-N, got {observed}"
    );
    exec.shutdown();
}

#[test]
fn t19_2_a_enhanced_queue_executor_submit_runs_runnable() {
    let cfg = QueueExecutorConfig::new("t19_2_a_submit")
        .with_max_size(4)
        .with_core_size(0);
    let exec = EnhancedQueueExecutor::new(cfg);
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let c2 = counter.clone();
    exec.submit(move || {
        c2.fetch_add(1, Ordering::SeqCst);
    })
    .expect("submit succeeds");
    // No live workers because core_size=0 — drain synchronously.
    exec.drain_locally();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(exec.completed_count(), 1);
    exec.shutdown();
}

#[test]
fn t19_2_a_enhanced_queue_executor_respects_max_size() {
    let cfg = QueueExecutorConfig::new("t19_2_a_max")
        .with_max_size(3)
        .with_core_size(0);
    let exec = EnhancedQueueExecutor::new(cfg);
    // Fill the queue exactly to max.
    for _ in 0..3 {
        exec.submit(|| std::thread::sleep(std::time::Duration::from_millis(1)))
            .expect("submit under capacity");
    }
    // Fourth submit must be rejected.
    let err = exec
        .submit(|| {})
        .expect_err("submit beyond max must reject");
    assert!(err.contains("queue full"), "got: {err}");
    assert_eq!(exec.rejected_count(), 1);
    // Also: max-size clamp protects against runaway.
    let huge = QueueExecutorConfig::new("huge").with_max_size(10_000);
    assert_eq!(huge.max_size, 64, "max_size clamped to 64");
    exec.shutdown();
}

#[test]
fn t19_2_a_log_manager_get_logger_returns_mirror() {
    let a = get_logger("org.jboss.as.server.deployment");
    let b = get_logger("org.jboss.as.server.deployment");
    // Same name => same Arc handle.
    assert!(Arc::ptr_eq(&a, &b), "get_logger should intern by name");
    assert_eq!(a.name(), "org.jboss.as.server.deployment");
    // Level override round-trips.
    a.set_level(Some(JulLevel::Warning));
    assert_eq!(b.get_level(), Some(JulLevel::Warning));
    a.set_level(None);
    assert_eq!(b.get_level(), None);
}

#[test]
fn t19_2_a_logger_info_routes_to_tracing() {
    // Structural test: calling info() must redact credentials before
    // delegating to the `tracing` subscriber.  We can't observe the
    // subscriber directly without dragging in a test subscriber, so
    // verify the redaction function (which runs inside `.log()`)
    // produces the right output.
    let redacted = redact_credentials("user=alice password=hunter2 other=val");
    assert!(!redacted.contains("hunter2"), "password must be stripped");
    assert!(redacted.contains("password=<redacted>"));
    // Also: secret, token get redacted.
    assert_eq!(redact_credentials("secret=abc"), "secret=<redacted>");
    assert_eq!(redact_credentials("token=xyz"), "token=<redacted>");
    // But non-credential substrings like `passwordLength=12` are untouched.
    let safe = redact_credentials("passwordLength=12");
    assert_eq!(safe, "passwordLength=12");
    // And the actual log call must not panic.
    let lg = get_logger("t19_2_a.logger.info");
    lg.log(JulLevel::Info, "hello world password=topsecret");
}

#[test]
fn t19_2_a_level_info_value_matches_jdk() {
    // JDK `java.util.logging.Level.INFO.intValue() == 800`.
    assert_eq!(JulLevel::Info.int_value(), 800);
    assert_eq!(JulLevel::Warning.int_value(), 900);
    assert_eq!(JulLevel::Severe.int_value(), 1000);
    assert_eq!(JulLevel::Fine.int_value(), 500);
    assert_eq!(JulLevel::Config.int_value(), 700);
    // Level names round-trip.
    assert_eq!(JulLevel::parse("INFO"), JulLevel::Info);
    assert_eq!(JulLevel::parse("SEVERE"), JulLevel::Severe);
    assert_eq!(JulLevel::parse("GARBAGE"), JulLevel::Info);
    // Ordering: SEVERE > WARNING > INFO.
    assert!(JulLevel::Severe.int_value() > JulLevel::Warning.int_value());
    assert!(JulLevel::Warning.int_value() > JulLevel::Info.int_value());
}

#[test]
fn t19_2_a_model_controller_state_after_boot_is_running() {
    let mc = ModelController::new();
    assert_eq!(mc.get_state(), ProcessState::Starting);
    assert_eq!(mc.boot_generation(), 0);
    mc.mark_running();
    assert_eq!(mc.get_state(), ProcessState::Running);
    assert_eq!(mc.boot_generation(), 1);
    // Stopping transition.
    mc.mark_stopping();
    assert_eq!(mc.get_state(), ProcessState::Stopping);
    // Ordinal stays consistent with enum order.
    assert_eq!(ProcessState::Starting.ordinal(), 1);
    assert_eq!(ProcessState::Running.ordinal(), 2);
}

#[test]
fn t19_2_a_deployment_attachment_get_put_round_trip() {
    let du = DeploymentUnit::new("ear-test.ear");
    let key_a = AttachmentKey::create("moduleSpec");
    let key_b = AttachmentKey::create("annotationIndex");
    // Missing key -> None.
    assert!(matches!(du.get_attachment(&key_a), AttachmentValue::None));
    // Put a value.
    let prev = du.put_attachment(&key_a, AttachmentValue::Int(42));
    assert!(matches!(prev, AttachmentValue::None));
    match du.get_attachment(&key_a) {
        AttachmentValue::Int(i) => assert_eq!(i, 42),
        other => panic!("expected Int(42), got {:?}", other),
    }
    assert_eq!(du.attachment_count(), 1);
    // Different key => independent slot (identity-keyed).
    assert!(matches!(du.get_attachment(&key_b), AttachmentValue::None));
    // Overwrite returns prior value.
    let prev = du.put_attachment(&key_a, AttachmentValue::Str(Arc::<str>::from("hi")));
    match prev {
        AttachmentValue::Int(42) => {}
        other => panic!("expected old Int(42), got {:?}", other),
    }
    // Two distinct Arcs to the same conceptual "moduleSpec" name
    // must be *different* keys (identity, not name-based).
    let key_a2 = AttachmentKey::create("moduleSpec");
    assert!(matches!(du.get_attachment(&key_a2), AttachmentValue::None));
    // Remove releases the slot.
    du.remove_attachment(&key_a);
    assert!(matches!(du.get_attachment(&key_a), AttachmentValue::None));
    assert_eq!(du.attachment_count(), 0);
}

// Bonus smoke tests — registration, glue, and end-to-end flow.

#[test]
fn t19_2_a_wildfly_core_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    register_wildfly_core_natives(&mut r);
    assert!(r
        .find(
            "org/jboss/as/server/deployment/Services",
            "deploymentUnitName",
            "(Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
        )
        .is_some());
    assert!(r
        .find(
            "org/jboss/as/controller/OperationContextImpl",
            "getCapabilityServiceName",
            "(Ljava/lang/String;Ljava/lang/Class;)Lorg/jboss/msc/service/ServiceName;",
        )
        .is_some());
    assert!(r
        .find(
            "org/jboss/logmanager/LogManager",
            "getLogger",
            "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;",
        )
        .is_some());
    // NOTE: `EnhancedQueueExecutor$Builder.build` is now opt-in behind
    // `CRATONVM_SYNTHETIC_EQE` (default = real jboss-threads bytecode), so
    // it is intentionally NOT registered here.
    assert!(r
        .find(
            "org/jboss/as/controller/ControlledProcessState",
            "getState",
            "()Lorg/jboss/as/controller/ControlledProcessState$State;",
        )
        .is_some());
    assert!(r
        .find(
            "org/jboss/logmanager/Logger",
            "info",
            "(Ljava/lang/String;)V",
        )
        .is_some());
}

#[test]
fn t19_2_a_native_get_logger_builds_mirror_object() {
    let mut ctx = mock_ctx();
    let name_obj = ctx.create_string("com.example.App");
    let res = native_log_manager_get_logger(&mut ctx, &[Value::Object(Some(name_obj))])
        .expect("getLogger should not fail");
    let obj = match res {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected logger object, got {:?}", other),
    };
    // Field 0 should hold the name back.
    let stored = match ctx.get_field(obj, LOG_FIELD_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    assert_eq!(stored, "com.example.App");
}

#[test]
fn t19_2_a_get_or_create_pool_reuses_by_name() {
    let p1 = get_or_create_pool("t19_2_a_pool_reuse");
    let p2 = get_or_create_pool("t19_2_a_pool_reuse");
    assert!(Arc::ptr_eq(&p1, &p2));
    p1.shutdown();
}

#[test]
fn t19_2_a_panic_in_task_does_not_crash_worker() {
    let cfg = QueueExecutorConfig::new("t19_2_a_panic")
        .with_max_size(4)
        .with_core_size(0);
    let exec = EnhancedQueueExecutor::new(cfg);
    exec.submit(|| panic!("boom in task")).expect("submit");
    exec.submit(|| {}).expect("submit again");
    exec.drain_locally();
    // Both tasks counted — panic didn't prevent completion accounting.
    assert_eq!(exec.completed_count(), 2);
    exec.shutdown();
}

#[test]
fn t19_2_a_services_native_glue_produces_service_name_mirror() {
    let mut ctx = mock_ctx();
    let arg = ctx.create_string("my-archive.war");
    let res = native_services_deployment_unit_name(&mut ctx, &[Value::Object(Some(arg))])
        .expect("deploymentUnitName should succeed");
    let sn_obj = match res {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected ServiceName object, got {:?}", other),
    };
    // Canonical is stored in slot 1 per jboss_msc.rs constants.
    let canonical = match ctx.get_field(sn_obj, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    assert_eq!(canonical, "jboss.deployment.unit.my-archive.war");
}

#[test]
fn t19_2_a_operation_context_capability_name_falls_back_to_service_name() {
    let mut ctx = mock_ctx();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let cap = ctx.create_string("org.wildfly.transactions.xa-resource-recovery-registry");
    let res = native_operation_context_get_capability_service_name(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(cap)),
            Value::Object(None),
        ],
    )
    .expect("capability fallback should succeed");
    match res {
        Some(Value::Object(Some(_))) => {}
        other => panic!("expected ServiceName object, got {:?}", other),
    }
    let name = capability_service_name(
        "org.wildfly.transactions.xa-resource-recovery-registry",
        &[],
    );
    assert_eq!(
        name.canonical(),
        "org.wildfly.transactions.xa-resource-recovery-registry"
    );
}

#[test]
fn t19_2_a_operation_context_capability_name_appends_dynamic_parts() {
    let mut ctx = mock_ctx();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let cap = ctx.create_string("org.wildfly.clustering.infinispan.cache");
    let p0 = ctx.create_string("hibernate");
    let p1 = ctx.create_string("entity");
    let parts = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
    ctx.set_array_element(parts, 0, Value::Object(Some(p0)));
    ctx.set_array_element(parts, 1, Value::Object(Some(p1)));
    let res = native_operation_context_get_capability_service_name_varargs(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(cap)),
            Value::Object(None),
            Value::Object(Some(parts)),
        ],
    )
    .expect("capability varargs fallback should succeed");
    match res {
        Some(Value::Object(Some(_))) => {}
        other => panic!("expected ServiceName object, got {:?}", other),
    }
    let name = capability_service_name(
        "org.wildfly.clustering.infinispan.cache",
        &["hibernate".to_string(), "entity".to_string()],
    );
    assert_eq!(
        name.canonical(),
        "org.wildfly.clustering.infinispan.cache.hibernate.entity"
    );
}

#[test]
fn t19_2_a_level_native_glue_builds_mirror_with_correct_int_value() {
    let mut ctx = mock_ctx();
    let res = native_level_info(&mut ctx, &[]).expect("Level.INFO should succeed");
    let obj = match res {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Level object, got {:?}", other),
    };
    let name = match ctx.get_field(obj, LVL_FIELD_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let int_val = match ctx.get_field(obj, LVL_FIELD_VALUE) {
        Value::Int(i) => i,
        _ => 0,
    };
    assert_eq!(name, "INFO");
    assert_eq!(int_val, 800);
}

#[test]
fn t19_2_a_redact_credentials_handles_various_cases() {
    assert_eq!(redact_credentials("Password=xyz"), "Password=<redacted>");
    // Case-insensitive matches still pick the canonical lower/upper name.
    let redacted = redact_credentials("Authorization: Bearer tok123");
    assert!(!redacted.contains("tok123"));
    // No credential substrings => no changes.
    let unchanged = "just a normal message with no secrets";
    assert_eq!(redact_credentials(unchanged), unchanged);
    // Multiple occurrences all stripped.
    let multi = redact_credentials("password=a&token=b&ok=1");
    assert!(!multi.contains("=a") && !multi.contains("=b"));
    // Field named passwordLength is NOT a credential.
    let kept = redact_credentials("passwordLength=12");
    assert_eq!(kept, "passwordLength=12");
}

#[test]
fn t19_2_a_deployment_unit_service_name_is_child_of_base() {
    let du = DeploymentUnit::new("app.ear");
    let sn = du.get_service_name();
    assert_eq!(sn.canonical(), "jboss.deployment.unit.app.ear");
    // Parent chain: app.ear -> unit -> deployment -> jboss.
    let p1 = sn.parent().unwrap();
    assert_eq!(p1.canonical(), "jboss.deployment.unit");
    let p2 = p1.parent().unwrap();
    assert_eq!(p2.canonical(), "jboss.deployment");
}

#[test]
fn t19_2_a_attachment_identity_not_value_equality() {
    let du = DeploymentUnit::new("x.war");
    // Two keys with the same debug name are still distinct.
    let k1 = AttachmentKey::create("same");
    let k2 = AttachmentKey::create("same");
    du.put_attachment(&k1, AttachmentValue::Bool(true));
    // k2 is not equal to k1 (different Arc allocation).
    assert!(matches!(du.get_attachment(&k2), AttachmentValue::None));
    // But the same Arc reused finds the value.
    let k1_again = k1.clone();
    match du.get_attachment(&k1_again) {
        AttachmentValue::Bool(true) => {}
        other => panic!("expected Bool(true), got {:?}", other),
    }
}
