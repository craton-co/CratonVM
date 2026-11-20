use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;
use std::sync::Mutex;

// Serialize tests that mutate the global domain registry so
// parallel runs don't race each other's clear/register sequences.
fn domain_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// -----------------------------------------------------------------------
// Test helpers: module builders that record whether they were invoked.
// -----------------------------------------------------------------------

fn always_succeeds(flag: ControlFlag, who: &str) -> Arc<LoginModuleEntry> {
    let name = who.to_string();
    Arc::new(LoginModuleEntry {
        class_name: format!("test.mod.{who}"),
        flag,
        options: HashMap::new(),
        login_fn: Box::new(move |subject, _opts| {
            subject.add_principal(Principal::new(&name));
            ModuleOutcome::Success
        }),
    })
}

fn always_fails(flag: ControlFlag, who: &str) -> Arc<LoginModuleEntry> {
    Arc::new(LoginModuleEntry {
        class_name: format!("test.mod.{who}"),
        flag,
        options: HashMap::new(),
        login_fn: Box::new(|_subject, _opts| ModuleOutcome::Failure),
    })
}

// A module that records (via an Arc<AtomicU64>) whether it ran.
fn tracking_module(
    flag: ControlFlag,
    counter: Arc<AtomicU64>,
    outcome: ModuleOutcome,
) -> Arc<LoginModuleEntry> {
    Arc::new(LoginModuleEntry {
        class_name: "test.mod.tracking".into(),
        flag,
        options: HashMap::new(),
        login_fn: Box::new(move |_subject, _opts| {
            counter.fetch_add(1, Ordering::SeqCst);
            outcome
        }),
    })
}

fn jaas_config_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match (method_name, descriptor) {
        (
            "getAppConfigurationEntry",
            "(Ljava/lang/String;)[Ljavax/security/auth/login/AppConfigurationEntry;",
        ) => {
            let entry = ctx.fresh_object_ref();
            let class_name =
                ctx.create_string("org.apache.catalina.realm.TesterLoginModule");
            let flag = ctx.fresh_object_ref();
            let flag_text = ctx.create_string("LoginModuleControlFlag: sufficient");
            let options = ctx.fresh_object_ref();
            ctx.set_field(entry, 0, Value::Object(Some(class_name)));
            ctx.set_field(entry, 1, Value::Object(Some(flag)));
            ctx.set_field(entry, 2, Value::Object(Some(options)));
            ctx.set_field(flag, 0, Value::Object(Some(flag_text)));

            let arr = ctx.new_array(ArrayElementType::Reference, 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(entry)));
            Some(Ok(Some(Value::Object(Some(arr)))))
        }
        ("getLoginModuleName", "()Ljava/lang/String;") => Some(Ok(Some(ctx.get_field(receiver, 0)))),
        (
            "getControlFlag",
            "()Ljavax/security/auth/login/AppConfigurationEntry$LoginModuleControlFlag;",
        ) => Some(Ok(Some(ctx.get_field(receiver, 1)))),
        ("getOptions", "()Ljava/util/Map;") => Some(Ok(Some(ctx.get_field(receiver, 2)))),
        ("toString", "()Ljava/lang/String;") => Some(Ok(Some(ctx.get_field(receiver, 0)))),
        (
            "initialize",
            "(Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;Ljava/util/Map;Ljava/util/Map;)V",
        ) => {
            ctx.set_field(receiver, 0, args[0]);
            ctx.set_field(receiver, 1, args[1]);
            ctx.set_field(receiver, 2, args[2]);
            ctx.set_field(receiver, 3, args[3]);
            Some(Ok(None))
        }
        ("login", "()Z") => Some(Ok(Some(Value::Int(1)))),
        ("commit", "()Z") => {
            if let Value::Object(Some(subject)) = ctx.get_field(receiver, 0) {
                if let Value::Object(Some(principals)) =
                    ctx.get_field(subject, SUBJECT_SLOT_PRINCIPALS)
                {
                    ctx.set_field(principals, 1, Value::Int(1));
                }
            }
            Some(Ok(Some(Value::Int(1))))
        }
        ("abort", "()Z") => Some(Ok(Some(Value::Int(1)))),
        ("size", "()I") => Some(Ok(Some(match ctx.get_field(receiver, 1) {
            Value::Int(size) => Value::Int(size),
            _ => Value::Int(0),
        }))),
        _ => None,
    }
}

// -----------------------------------------------------------------------
// The 10 required tests.
// -----------------------------------------------------------------------

#[test]
fn t19_2_c_login_context_runs_configuration_backed_login_module() {
    let _guard = domain_test_lock();
    clear_domain_registry();

    let mut ctx = MockNativeContext::new();
    let this = match ctx
        .new_object("javax/security/auth/login/LoginContext")
        .unwrap()
    {
        Some(Value::Object(Some(login_context))) => login_context,
        other => panic!("expected LoginContext object, got {other:?}"),
    };
    let name = ctx.create_string("CustomLogin");
    let subject = match ctx.new_object("javax/security/auth/Subject").unwrap() {
        Some(Value::Object(Some(subject))) => subject,
        other => panic!("expected Subject object, got {other:?}"),
    };
    native_subject_init(&mut ctx, &[Value::Object(Some(subject))])
        .expect("Subject init should register side-table handle");

    let callback = ctx.fresh_object_ref();
    let config = ctx.fresh_object_ref();
    ctx.set_invoke_virtual_hook(jaas_config_hook);

    native_login_context_init(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(name)),
            Value::Object(Some(subject)),
            Value::Object(Some(callback)),
            Value::Object(Some(config)),
        ],
    )
    .expect("LoginContext init should retain config-backed state");

    assert_eq!(
        ctx.get_field_by_name(this, "config"),
        Value::Object(Some(config))
    );
    native_login_context_login(&mut ctx, &[Value::Object(Some(this))])
        .expect("sufficient Java LoginModule should authenticate");

    let returned_subject =
        native_login_context_get_subject(&mut ctx, &[Value::Object(Some(this))])
            .expect("getSubject should succeed")
            .expect("getSubject should return a value");
    assert_eq!(returned_subject, Value::Object(Some(subject)));

    let principals = native_subject_get_principals(&mut ctx, &[Value::Object(Some(subject))])
        .expect("getPrincipals should return the Java backing set")
        .expect("getPrincipals should return a value");
    let expected_set = ctx.get_field(subject, SUBJECT_SLOT_PRINCIPALS);
    assert_eq!(principals, expected_set);
    if let Value::Object(Some(set)) = principals {
        assert_eq!(ctx.get_field(set, 1), Value::Int(1));
    } else {
        panic!("expected principals set object, got {principals:?}");
    }
}

#[test]
#[allow(non_snake_case)]
fn t19_2_c_subject_doAs_invokes_action() {
    // Under do_as, the current subject is set for the closure's
    // scope; after return it reverts to the previous value.
    assert!(current_subject().is_none());
    let s = Subject::new();
    s.add_principal(Principal::new("alice"));
    let observed = do_as(s.clone(), || {
        let cur = current_subject().expect("subject active inside do_as");
        assert_eq!(cur.principal_count(), 1);
        "ran"
    });
    assert_eq!(observed, "ran");
    // Restored.
    assert!(current_subject().is_none());
}

#[test]
fn t19_2_c_subject_principal_set_dedup_by_name() {
    let s = Subject::new();
    s.add_principal(Principal::new("alice"));
    s.add_principal(Principal::new("alice")); // duplicate name → dedupe
    s.add_principal(Principal::new("bob"));
    // Set semantics: two distinct principal names → 2 entries.
    assert_eq!(s.principal_count(), 2);
    let names: Vec<String> = s
        .get_principals()
        .into_iter()
        .map(|p| p.name().to_string())
        .collect();
    assert!(names.contains(&"alice".to_string()));
    assert!(names.contains(&"bob".to_string()));
}

#[test]
fn t19_2_c_login_context_required_success() {
    let s = Subject::new();
    let lc = LoginContext::new(
        "test-required-ok",
        s,
        vec![
            always_succeeds(ControlFlag::Required, "a"),
            always_succeeds(ControlFlag::Required, "b"),
        ],
    );
    let r = lc.login();
    assert!(r.success);
    assert_eq!(r.outcomes.len(), 2);
    assert!(r.outcomes.iter().all(|o| *o == ModuleOutcome::Success));
}

#[test]
fn t19_2_c_login_context_required_failure_calls_remaining_modules() {
    // REQUIRED failure must NOT abort the chain — remaining
    // modules still see their login() invoked. This is
    // specifically different from REQUISITE.
    let counter = Arc::new(AtomicU64::new(0));
    let lc = LoginContext::new(
        "test-required-fail",
        Subject::new(),
        vec![
            always_fails(ControlFlag::Required, "dead"),
            tracking_module(
                ControlFlag::Optional,
                counter.clone(),
                ModuleOutcome::Success,
            ),
            tracking_module(
                ControlFlag::Optional,
                counter.clone(),
                ModuleOutcome::Success,
            ),
        ],
    );
    let r = lc.login();
    // Overall must fail because a REQUIRED failed.
    assert!(!r.success);
    // But both remaining modules executed (counter bumped twice).
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    assert_eq!(r.outcomes[0], ModuleOutcome::Failure);
    assert_eq!(r.outcomes[1], ModuleOutcome::Success);
    assert_eq!(r.outcomes[2], ModuleOutcome::Success);
}

#[test]
fn t19_2_c_login_context_requisite_aborts_on_failure() {
    // REQUISITE failure short-circuits — subsequent modules are
    // NOT invoked at all.
    let counter = Arc::new(AtomicU64::new(0));
    let lc = LoginContext::new(
        "test-requisite-abort",
        Subject::new(),
        vec![
            always_fails(ControlFlag::Requisite, "abort"),
            tracking_module(
                ControlFlag::Required,
                counter.clone(),
                ModuleOutcome::Success,
            ),
        ],
    );
    let r = lc.login();
    assert!(!r.success);
    // The second module was not reached.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    // The chain cut off after the first entry, so outcomes has
    // only one recorded result.
    assert_eq!(r.outcomes.len(), 1);
    assert_eq!(r.outcomes[0], ModuleOutcome::Failure);
}

#[test]
fn t19_2_c_login_context_sufficient_success_stops_chain() {
    // SUFFICIENT success stops the chain (remaining modules are
    // NotInvoked), and the overall result is success.
    let counter = Arc::new(AtomicU64::new(0));
    let lc = LoginContext::new(
        "test-sufficient-stop",
        Subject::new(),
        vec![
            always_succeeds(ControlFlag::Sufficient, "win"),
            tracking_module(
                ControlFlag::Required,
                counter.clone(),
                ModuleOutcome::Failure,
            ),
        ],
    );
    let r = lc.login();
    assert!(r.success);
    // Second module was skipped entirely.
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert_eq!(r.outcomes.len(), 2);
    assert_eq!(r.outcomes[0], ModuleOutcome::Success);
    assert_eq!(r.outcomes[1], ModuleOutcome::NotInvoked);
}

#[test]
fn t19_2_c_login_context_optional_failure_not_fatal() {
    // OPTIONAL failure followed by REQUIRED success → overall success.
    let lc = LoginContext::new(
        "test-optional-fail",
        Subject::new(),
        vec![
            always_fails(ControlFlag::Optional, "meh"),
            always_succeeds(ControlFlag::Required, "win"),
        ],
    );
    let r = lc.login();
    assert!(r.success);
    assert_eq!(r.outcomes[0], ModuleOutcome::Failure);
    assert_eq!(r.outcomes[1], ModuleOutcome::Success);
}

#[test]
fn t19_2_c_security_domain_service_lookup_returns_auth_manager() {
    let _guard = domain_test_lock();
    clear_domain_registry();

    let policy = ApplicationPolicy::new(vec![always_succeeds(ControlFlag::Required, "a")]);
    let sds = SecurityDomainService::new("keycloak", policy);
    register_security_domain(sds.clone());

    // Lookup by name returns the same Arc handle.
    let found =
        lookup_security_domain("keycloak").expect("registered keycloak domain should resolve");
    assert!(Arc::ptr_eq(&found, &sds));

    // AuthenticationManager is the same Rust-side object.
    let mgr = found.get_authentication_manager();
    assert!(Arc::ptr_eq(&mgr, &found));

    // The policy exposes the registered chain.
    let info = found.policy.get_authentication_info();
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].flag, ControlFlag::Required);

    // And we can build a fresh LoginContext over that chain.
    let lc = found.new_login_context("keycloak");
    let r = lc.login();
    assert!(r.success);
    clear_domain_registry();
}

#[test]
fn t19_2_c_security_identity_run_as_changes_current_principal() {
    // Verify the thread-local identity is set for the scope of
    // run_as and restored on return, even if the closure panics.
    assert!(current_identity().is_none());
    let id = SecurityIdentity::new(
        Principal::new("admin"),
        ["user".to_string(), "admin".to_string()],
    );
    assert!(id.has_role("user"));
    assert!(id.has_role("admin"));
    let observed = id.run_as(|| {
        let cur = current_identity().expect("identity active inside run_as");
        cur.principal.name().to_string()
    });
    assert_eq!(observed, "admin");
    assert!(current_identity().is_none());

    // Panic safety — the drop guard restores on unwind.
    let id2 = SecurityIdentity::new(Principal::new("u2"), std::iter::empty());
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        id2.run_as(|| {
            let _ = current_identity().unwrap();
            panic!("boom");
        })
    }));
    assert!(panicked.is_err());
    assert!(
        current_identity().is_none(),
        "identity must be cleared even after a panic"
    );
}

#[test]
fn t19_2_c_access_control_context_check_permission_delegates_to_policy() {
    // With no policy loaded (default), every permission is allowed;
    // once a non-matching policy is loaded we get DENY.
    use crate::security_manager::{set_active_policy, Policy};

    // Make sure we start with a clean slate. A concurrent test
    // that installs a policy would invalidate our expectations,
    // so grab the same lock the security_manager tests use.
    // policy_test_lock is private to that module — we make do
    // with a best-effort check here; failures show up as extra
    // DENYs that the second assertion catches.
    set_active_policy(None);
    assert!(
        crate::security_manager::policy_allows(
            "java/util/PropertyPermission",
            "java.version",
            "read",
            None,
        ),
        "default policy must allow-all"
    );

    // Install an empty policy — now everything is denied.
    let policy = Policy::default();
    set_active_policy(Some(policy));
    assert!(
        !crate::security_manager::policy_allows(
            "java/util/PropertyPermission",
            "java.version",
            "read",
            None,
        ),
        "empty policy must deny"
    );
    // Clean up so we don't break other tests.
    set_active_policy(None);
}

// -----------------------------------------------------------------------
// Additional sanity tests for the support machinery.
// -----------------------------------------------------------------------

#[test]
fn t19_2_c_constant_time_eq_same_length() {
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"sedret"));
    assert!(!constant_time_eq(b"secret", b"SECRET"));
}

#[test]
fn t19_2_c_constant_time_eq_different_length_rejects() {
    assert!(!constant_time_eq(b"short", b"longer-string"));
    assert!(!constant_time_eq(b"", b"nonempty"));
    assert!(constant_time_eq(b"", b""));
}

#[test]
fn t19_2_c_control_flag_parse_case_insensitive() {
    assert_eq!(ControlFlag::parse("REQUIRED"), Some(ControlFlag::Required));
    assert_eq!(ControlFlag::parse("required"), Some(ControlFlag::Required));
    assert_eq!(
        ControlFlag::parse("Requisite"),
        Some(ControlFlag::Requisite)
    );
    assert_eq!(
        ControlFlag::parse("SUFFICIENT"),
        Some(ControlFlag::Sufficient)
    );
    assert_eq!(ControlFlag::parse("optional"), Some(ControlFlag::Optional));
    assert_eq!(ControlFlag::parse("nope"), None);
}

#[test]
fn t19_2_c_redact_private_never_leaks_bytes() {
    let secret = b"super-secret-password";
    assert_eq!(redact_private(secret), "<redacted>");
    // And a debug-format on a LoginModuleEntry also redacts options.
    let lme = LoginModuleEntry {
        class_name: "x".into(),
        flag: ControlFlag::Required,
        options: {
            let mut m = HashMap::new();
            m.insert("password".to_string(), "s3cr3t".to_string());
            m
        },
        login_fn: Box::new(|_, _| ModuleOutcome::Success),
    };
    let dbg = format!("{lme:?}");
    assert!(dbg.contains("<redacted>"));
    assert!(!dbg.contains("s3cr3t"));
}

#[test]
fn t19_2_c_db_password_module_constant_time_matches_on_equal() {
    let s = Subject::new();
    let m = db_password_module(
        "org.jboss.security.DatabaseServerLoginModule",
        ControlFlag::Required,
        b"hash-of-p4ssw0rd".to_vec(),
        b"hash-of-p4ssw0rd".to_vec(),
        "dbuser".into(),
    );
    let lc = LoginContext::new("db", s.clone(), vec![m]);
    let r = lc.login();
    assert!(r.success);
    assert_eq!(s.principal_count(), 1);
}

#[test]
fn t19_2_c_db_password_module_rejects_mismatch() {
    let s = Subject::new();
    let m = db_password_module(
        "org.jboss.security.DatabaseServerLoginModule",
        ControlFlag::Required,
        b"hash-of-p4ssw0rd".to_vec(),
        b"hash-of-WRONG".to_vec(),
        "dbuser".into(),
    );
    let lc = LoginContext::new("db", s.clone(), vec![m]);
    let r = lc.login();
    assert!(!r.success);
    assert_eq!(s.principal_count(), 0);
}

#[test]
fn t19_2_c_internal_bootstrap_module_attaches_internal_principal() {
    let s = Subject::new();
    let m = internal_bootstrap_module(ControlFlag::Required);
    let lc = LoginContext::new("mgmt", s.clone(), vec![m]);
    let r = lc.login();
    assert!(r.success);
    assert!(s.get_principals().iter().any(|p| p.name() == "internal"));
}

#[test]
fn t19_2_c_login_context_logout_clears_last_result_and_private_creds() {
    let s = Subject::new();
    s.add_private_credential(b"p4ssw0rd");
    let lc = LoginContext::new(
        "logout-test",
        s.clone(),
        vec![always_succeeds(ControlFlag::Required, "ok")],
    );
    assert!(lc.login().success);
    assert!(lc.last_result().is_some());
    assert!(!s.get_private_credentials().is_empty());
    lc.logout();
    assert!(lc.last_result().is_none());
    assert!(s.get_private_credentials().is_empty());
}

#[test]
fn t19_2_c_security_identity_get_roles_returns_snapshot() {
    let id = SecurityIdentity::new(
        Principal::new("service"),
        ["reader", "writer"].into_iter().map(str::to_string),
    );
    let roles: Vec<String> = id.get_roles().into_iter().map(|r| r.to_string()).collect();
    assert_eq!(roles.len(), 2);
    assert!(roles.iter().any(|r| r == "reader"));
    assert!(roles.iter().any(|r| r == "writer"));
}

#[test]
fn t19_2_c_principal_eq_and_hash_on_name() {
    let p1 = Principal::new("same");
    let p2 = Principal::new("same");
    let p3 = Principal::new("other");
    assert_eq!(*p1, *p2);
    assert_ne!(*p1, *p3);
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h1 = DefaultHasher::new();
    let mut h2 = DefaultHasher::new();
    (*p1).hash(&mut h1);
    (*p2).hash(&mut h2);
    assert_eq!(h1.finish(), h2.finish());
}

#[test]
fn t19_2_c_register_wildfly_security_natives_installs_each_method() {
    let mut r = NativeMethodRegistry::new();
    register_jdk_security_natives(&mut r);
    register_wildfly_security_natives(&mut r);
    assert!(r
        .find(
            "javax/security/auth/Subject",
            "doAs",
            "(Ljavax/security/auth/Subject;Ljava/security/PrivilegedAction;)Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find("javax/security/auth/login/LoginContext", "login", "()V")
        .is_some());
    assert!(r
        .find(
            "org/jboss/as/security/SecurityDomainService",
            "getAuthenticationManager",
            "()Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find(
            "org/wildfly/security/auth/server/SecurityIdentity",
            "runAs",
            "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;"
        )
        .is_some());
    // AccessControlContext.checkPermission is registered by
    // security_manager.rs; we only register <init>.
    assert!(r
        .find(
            "java/security/AccessControlContext",
            "<init>",
            "([Ljava/security/ProtectionDomain;)V"
        )
        .is_some());
}
