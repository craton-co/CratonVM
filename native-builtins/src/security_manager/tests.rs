use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Helper: look up a native and call it through the registry.
fn call_native(
    registry: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = registry
        .find(class, method, desc)
        .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
    cb(ctx, args)
}

#[test]
fn test_set_and_get_security_manager() {
    let _guard = security_state_test_lock();
    let mut ctx = MockNativeContext::new();
    // Reset global state
    set_security_manager(&mut ctx, None);

    // Initially null
    assert!(get_security_manager(&ctx).is_none());

    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

    // Set the SM
    set_security_manager(&mut ctx, Some(sm_obj));
    assert_eq!(get_security_manager(&ctx), Some(sm_obj));

    // Clear it
    set_security_manager(&mut ctx, None);
    assert!(get_security_manager(&ctx).is_none());
}

#[test]
fn test_security_manager_init() {
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "<init>",
        "()V",
        &[Value::Object(Some(sm_obj))],
    );
    assert!(result.is_ok());
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "doPrivileged must release its action root"
    );
    assert!(result.unwrap().is_none());
}

#[test]
fn test_check_permission_allows_all() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let perm =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/AllPermission", 0).unwrap();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkPermission",
        "(Ljava/security/Permission;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
    );
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
    clear_policy_and_stack();
}

#[test]
fn test_check_read_allows_all() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let file_str = ctx.create_string("/etc/passwd");

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkRead",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(file_str))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_check_connect_allows_all() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let host = ctx.create_string("example.com");

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkConnect",
        "(Ljava/lang/String;I)V",
        &[
            Value::Object(Some(sm_obj)),
            Value::Object(Some(host)),
            Value::Int(80),
        ],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_check_exit_allows_all() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkExit",
        "(I)V",
        &[Value::Object(Some(sm_obj)), Value::Int(0)],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_get_security_context() {
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "getSecurityContext",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(sm_obj))],
    );
    assert!(result.is_ok());
    let val = result.unwrap();
    assert!(val.is_some(), "getSecurityContext should return an object");
    match val.unwrap() {
        Value::Object(Some(_)) => {} // expected
        other => panic!("Expected Object(Some(_)), got {:?}", other),
    }
}

/// JIT round 16 wave 14 (lane ffm3, FFS16-1): the mirror follows the
/// SecurityManager slots of every VM, through installs and removals.
#[test]
fn the_security_manager_mirror_follows_every_slot() {
    let _guard = security_state_test_lock();
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x5f5_1600);
    let _ = set_security_manager_for_test(&ctx, None);
    assert!(!any_security_manager_installed(), "no VM holds a manager");
    // SAFETY: a non-null, aligned address that is never dereferenced
    // (no collection runs on this table here).
    let sm = unsafe { ObjectRef::from_raw(0x5000 as *mut u8) };
    let _ = set_security_manager_for_test(&ctx, Some(sm));
    assert!(any_security_manager_installed());
    assert_eq!(get_security_manager(&ctx), Some(sm));
    let _ = set_security_manager_for_test(&ctx, None);
    assert!(!any_security_manager_installed());
    let _ = set_security_manager_for_test(&ctx, Some(sm));
    forget_vm_security_state(0x5f5_1600);
    assert!(
        !any_security_manager_installed(),
        "a forgotten VM's manager is gone from the mirror"
    );
}

#[test]
fn test_system_get_set_security_manager() {
    let _guard = security_state_test_lock();

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    // Reset this VM's state (the mock reports identity 0).
    let _ = set_security_manager_for_test(&ctx, None);

    // getSecurityManager() should return null initially
    let get_sm = registry
        .find(
            "java/lang/System",
            "getSecurityManager",
            "()Ljava/lang/SecurityManager;",
        )
        .expect("getSecurityManager should be registered");

    let result = get_sm(&mut ctx, &[]);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), Some(Value::Object(None)));

    // Set a SecurityManager
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let set_sm = registry
        .find(
            "java/lang/System",
            "setSecurityManager",
            "(Ljava/lang/SecurityManager;)V",
        )
        .expect("setSecurityManager should be registered");

    let result = set_sm(&mut ctx, &[Value::Object(Some(sm_obj))]);
    assert!(result.is_ok());

    // Now getSecurityManager should return it
    let result = get_sm(&mut ctx, &[]);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), Some(Value::Object(Some(sm_obj))));

    // Clean up
    let _ = set_security_manager_for_test(&ctx, None);
}

#[test]
fn test_do_privileged_invokes_run() {
    // FIX (parallel-safety): `doPrivileged` writes the action class's
    // codeBase/signer tokens into the *process-global* per-ClassId
    // caches (`CLASS_CODE_BASE_CACHE` / `SIGNER_TOKENS_CACHE`), keyed by
    // a `ClassId` that every fresh `MockNativeContext` restarts at a low
    // value. Without serialization this test can insert an entry for the
    // same `ClassId` another doPrivileged test is asserting on,
    // corrupting `test_do_privileged_pushes_and_pops_stack`. Acquire the
    // shared lock and clear the caches so all stack/cache mutators run
    // serially — without changing any production semantics.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let action =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/PrivilegedAction", 0).unwrap();

    // Pre-arm the mock to return a value from invoke_virtual (action.run())
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
    }

    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "doPrivileged",
        "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
        &[Value::Object(Some(action))],
    );
    assert!(result.is_ok());
}

#[test]
fn test_do_privileged_exception_action() {
    // FIX (parallel-safety): see `test_do_privileged_invokes_run`.
    // doPrivileged mutates the global per-ClassId caches; serialize via
    // the shared test lock so it cannot race
    // `test_do_privileged_pushes_and_pops_stack`.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let action =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/PrivilegedExceptionAction", 0)
            .unwrap();

    // Pre-arm: return Int(42) from action.run()
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(42))));
    }

    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "doPrivileged",
        "(Ljava/security/PrivilegedExceptionAction;)Ljava/lang/Object;",
        &[Value::Object(Some(action))],
    );
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), Some(Value::Int(42)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "exception-action doPrivileged must release its action root"
    );
}

#[test]
fn test_do_privileged_with_context() {
    // FIX (parallel-safety): see `test_do_privileged_invokes_run`.
    // doPrivileged mutates the global per-ClassId caches; serialize via
    // the shared test lock so it cannot race
    // `test_do_privileged_pushes_and_pops_stack`.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let action =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/PrivilegedAction", 0).unwrap();
    let acc = try_alloc_concurrent_synthetic(&mut ctx, "java/security/AccessControlContext", 1)
        .unwrap();

    // Pre-arm
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
    }

    let result = call_native(
        &registry, &mut ctx,
        "java/security/AccessController", "doPrivileged",
        "(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;)Ljava/lang/Object;",
        &[Value::Object(Some(action)), Value::Object(Some(acc))],
    );
    assert!(result.is_ok());
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "context overload must release its action root"
    );
}

#[test]
fn test_access_controller_get_context() {
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "getContext",
        "()Ljava/security/AccessControlContext;",
        &[],
    );
    assert!(result.is_ok());
    let val = result.unwrap();
    assert!(val.is_some());
    match val.unwrap() {
        Value::Object(Some(_)) => {}
        other => panic!("Expected Object(Some(_)), got {:?}", other),
    }
}

#[test]
fn test_access_control_context_check_permission() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let acc = try_alloc_concurrent_synthetic(&mut ctx, "java/security/AccessControlContext", 1)
        .unwrap();
    let perm =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/AllPermission", 0).unwrap();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessControlContext",
        "checkPermission",
        "(Ljava/security/Permission;)V",
        &[Value::Object(Some(acc)), Value::Object(Some(perm))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_check_create_class_loader() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkCreateClassLoader",
        "()V",
        &[Value::Object(Some(sm_obj))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_check_write_and_delete() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let file_str = ctx.create_string("/tmp/test.txt");

    // checkWrite
    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkWrite",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(file_str))],
    );
    assert!(result.is_ok());

    // checkDelete
    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkDelete",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(file_str))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_check_exec() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let cmd = ctx.create_string("/usr/bin/ls");

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkExec",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(cmd))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_check_property_access() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let prop = ctx.create_string("java.home");

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkPropertyAccess",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(prop))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_check_access_thread_and_group() {
    // `checkAccess` has routed through `check_permission_impl` since the V10
    // security fix, so this test reads the process-global `ACTIVE_POLICY`
    // like every other `checkXxx` test — it asserts the no-policy ALLOW.
    // Without the lock it was the one policy-sensitive test in this module
    // running unserialized, and a policy-installing sibling scheduled
    // alongside it turned the ALLOW into a DENY: `assert!(result.is_ok())`
    // failed roughly once in twenty whole-suite runs, and 27 times in 30
    // when the two are run as a pair on two threads.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let thread = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 0).unwrap();
    let group = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ThreadGroup", 0).unwrap();

    // checkAccess(Thread)
    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkAccess",
        "(Ljava/lang/Thread;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(thread))],
    );
    assert!(result.is_ok());

    // checkAccess(ThreadGroup)
    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkAccess",
        "(Ljava/lang/ThreadGroup;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(group))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_registration_count() {
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);
    // 13 SM methods + 2 System methods + 4 AC methods + 1 ACC method = 20
    // T19 · N3 adds 4 more AC methods (getStackAccessControlContext,
    // getInheritedAccessControlContext, getProtectionDomain(Class),
    // ensureMaterializedForStackWalk) → 24.
    // T19.H9 adds 9 more Policy methods (getPolicy, getPolicyNoCheck,
    // isSet, setPolicy, getPermissions(PD), getPermissions(CS),
    // implies, refresh, <init>) → 33.
    assert!(
        registry.len() >= 33,
        "Expected at least 33 registered natives, got {}",
        registry.len()
    );
}

// -----------------------------------------------------------------------
// Policy integration + stack-walk semantics
// -----------------------------------------------------------------------

/// Serialize tests that mutate the global policy so parallel test
/// execution doesn't cause one test's `clear_policy_and_stack()` to
/// race another's `set_active_policy(Some(...))`.
///
/// **Readers need it too, not just writers.** `ACTIVE_POLICY` is a process
/// global, so a test that merely *observes* the no-policy default is racing
/// every policy-installing sibling — the writers taking this lock among
/// themselves does nothing for a reader that does not. Since the V10
/// security fix every `checkXxx` native routes through
/// `check_permission_impl`, which reads `ACTIVE_POLICY`; so the rule is:
/// **a test that calls any `checkXxx` native, or `set_active_policy`, or
/// `clear_policy_and_stack`, holds this guard for its whole body.**
/// `test_check_access_thread_and_group` was the one test in this module
/// that did not, and it failed about once in twenty whole-suite runs.
fn policy_test_lock() -> std::sync::MutexGuard<'static, ()> {
    security_state_test_lock()
}

/// Reset global state between policy-sensitive tests.
///
/// The object slots are per-VM now; every `MockNativeContext` reports
/// `vm_identity() == 0` unless a test overrides it, so clearing scope 0 is
/// exactly what the old whole-static clear did for these tests.
fn clear_policy_and_stack() {
    set_active_policy(None);
    // Clear the mock VM's Policy slot directly (no ctx needed for a clear —
    // registration only happens on store of Some). The shared permission
    // collection goes too, so a stale ObjectRef from a prior test's
    // MockNativeContext heap doesn't leak into the next test — calls to
    // `ensure_shared_permission_collection` must allocate fresh.
    with_security_state(|table| {
        if let Some(state) = table.get_mut(&0) {
            state.policy_object = None;
            state.shared_permissions = None;
            if state.is_empty() {
                table.remove(&0);
            }
        }
    });
    PRIVILEGED_STACK.with(|s| s.borrow_mut().clear());
    // Tests reuse small `ClassId` values across `MockNativeContext`
    // instances (each fresh ctx starts at id=1), so the per-ClassId
    // caches from prior tests would leak across the test boundary.
    // Clear them under the same gate as the privileged stack.
    if let Some(cache) = CLASS_CODE_BASE_CACHE.get() {
        cache.write().clear();
    }
    if let Some(cache) = SIGNER_TOKENS_CACHE.get() {
        cache.write().clear();
    }
}

#[test]
fn test_policy_denies_when_class_not_granted() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    // Policy grants FilePermission only.
    let src = r#"
        grant {
            permission java.io.FilePermission "/tmp/*", "read";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    // Ask for a Socket permission — not granted.
    let perm =
        try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
    let host = ctx.create_string("example.com:443");
    ctx.set_field(perm, 0, Value::Object(Some(host)));
    let actions = ctx.create_string("connect");
    ctx.set_field(perm, 1, Value::Object(Some(actions)));

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkPermission",
        "(Ljava/security/Permission;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
    );
    assert!(result.is_err(), "expected denial; got {result:?}");

    clear_policy_and_stack();
}

#[test]
fn test_policy_allows_exact_grant() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"
        grant {
            permission java.io.FilePermission "/tmp/*", "read,write";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let perm = try_alloc_concurrent_synthetic(&mut ctx, "java/io/FilePermission", 2).unwrap();
    let target = ctx.create_string("/tmp/foo.txt");
    ctx.set_field(perm, 0, Value::Object(Some(target)));
    let actions = ctx.create_string("read");
    ctx.set_field(perm, 1, Value::Object(Some(actions)));

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkPermission",
        "(Ljava/security/Permission;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
    );
    assert!(result.is_ok(), "expected allow; got {result:?}");

    clear_policy_and_stack();
}

#[test]
fn test_policy_all_permission_fallback() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"grant { permission java.security.AllPermission; };"#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let perm =
        try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
    let host = ctx.create_string("example.com:80");
    ctx.set_field(perm, 0, Value::Object(Some(host)));
    let actions = ctx.create_string("connect");
    ctx.set_field(perm, 1, Value::Object(Some(actions)));

    let result = call_native(
        &registry,
        &mut ctx,
        "java/lang/SecurityManager",
        "checkPermission",
        "(Ljava/security/Permission;)V",
        &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
    );
    assert!(result.is_ok());
    clear_policy_and_stack();
}

#[test]
fn test_do_privileged_pushes_and_pops_stack() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    assert_eq!(privileged_stack_depth(), 0);

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    // Class name: com/acme/PrivilegedBlob
    let action =
        try_alloc_concurrent_synthetic(&mut ctx, "com/acme/PrivilegedBlob", 0).unwrap();

    // While action.run() executes, the stack depth should be 1 and the
    // top frame should reflect com/acme/PrivilegedBlob.
    let observed: std::sync::Arc<std::sync::Mutex<(usize, Option<String>)>> =
        std::sync::Arc::new(std::sync::Mutex::new((0, None)));
    let obs_clone = observed.clone();
    // Install a callback on MockNativeContext.invoke_virtual_result via a
    // lambda that captures the observation. MockNativeContext doesn't
    // take a closure, so instead we observe inside the native itself by
    // hooking the pre-armed result.
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
    }

    // Snapshot the stack immediately before invoking doPrivileged by
    // manually pushing a sentinel and popping it after, then verifying
    // current_privileged_code_base() observed the action's code base
    // inside the native. Easier: intercept by calling push directly and
    // observing current_privileged_code_base.
    push_privileged_frame(Some("class:outer/Caller".into()));
    assert_eq!(
        current_privileged_code_base().as_deref(),
        Some("class:outer/Caller")
    );
    pop_privileged_frame();
    assert_eq!(privileged_stack_depth(), 0);

    // Now invoke doPrivileged and verify that, after completion, the
    // stack has unwound back to empty (i.e. push and pop both happened).
    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "doPrivileged",
        "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
        &[Value::Object(Some(action))],
    );
    assert!(result.is_ok());
    assert_eq!(
        privileged_stack_depth(),
        0,
        "doPrivileged must pop its frame"
    );

    // Direct check: push the same frame the wrapper would have and
    // confirm the observable mapping.
    push_privileged_frame(action_code_base(&mut ctx, action));
    assert_eq!(
        current_privileged_code_base().as_deref(),
        Some("class:com/acme/PrivilegedBlob")
    );
    pop_privileged_frame();

    let _ = obs_clone; // silence unused
    clear_policy_and_stack();
}

/// A -> B.doPrivileged(C -> checkPermission) must see B's code base,
/// not A's. We model the chain manually: push A, call doPrivileged on
/// an action whose class stands in for B's inner class, and assert the
/// permission check sees B's code base via `current_privileged_code_base`.
#[test]
fn test_do_privileged_stack_walk_semantics() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();

    // Policy: grant FilePermission to code under com/acme only.
    let src = r#"
        grant codeBase "class:com/acme/-" {
            permission java.io.FilePermission "/tmp/*", "read";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();

    // A — a class *outside* com/acme — pushes itself as the "caller" frame.
    push_privileged_frame(Some("class:com/bogus/Attacker".into()));

    // Without doPrivileged, a checkPermission would see com/bogus/Attacker
    // and be denied. Verify this is the observed code base.
    assert_eq!(
        current_privileged_code_base().as_deref(),
        Some("class:com/bogus/Attacker")
    );

    // Now B (under com/acme) calls doPrivileged with its own action class.
    let b_action = try_alloc_concurrent_synthetic(&mut ctx, "com/acme/Helper$1", 0).unwrap();

    // Inside the doPrivileged invocation, our wrapper must push B's code
    // base — overriding the caller's. We verify this by intercepting
    // `invoke_virtual`. MockNativeContext returns a fixed pre-armed
    // value, so we emulate the inside-of-run observation by calling
    // `action_code_base` and pushing it ourselves, then checking the
    // top frame is B's, not A's.
    push_privileged_frame(action_code_base(&mut ctx, b_action));
    assert_eq!(
        current_privileged_code_base().as_deref(),
        Some("class:com/acme/Helper$1"),
        "checkPermission inside doPrivileged must see B's code base, not A's"
    );

    // Policy check inside the doPrivileged: FilePermission granted because
    // the privileged frame is under com/acme/-.
    assert!(policy_allows(
        "java/io/FilePermission",
        "/tmp/x",
        "read",
        current_privileged_code_base().as_deref(),
    ));

    // Pop B's frame; now we're back to A — the same check must now be
    // denied because com/bogus is not covered by the grant.
    pop_privileged_frame();
    assert!(!policy_allows(
        "java/io/FilePermission",
        "/tmp/x",
        "read",
        current_privileged_code_base().as_deref(),
    ));

    // Clean up A's frame.
    pop_privileged_frame();
    assert_eq!(privileged_stack_depth(), 0);
    clear_policy_and_stack();
}

// -----------------------------------------------------------------------
// T11 · Real ProtectionDomain / CodeSource wiring + CIDR / signedBy
// -----------------------------------------------------------------------

#[test]
fn t11_protection_domain_populated_from_jar_url() {
    // When a class is loaded from a JAR on disk, its CodeSource URL
    // flows end-to-end into the policy engine and matches a
    // `grant codeBase "file:/..."` clause.
    //
    // We exercise the ClassManager + ClassPath pipeline directly to
    // confirm that define_class() attaches a CodeSource with the
    // right URL; the security_manager-side behavior is covered by
    // the SM trait method (see `t11_action_code_base_reads_real_url`).
    use cratonvm_classloading::{ClassLoaderId, ClassManager};
    use std::io::Write as _;

    // Build a minimal JAR with one fake .class entry.
    let dir = std::env::temp_dir().join("cratonvm-t11-pd-jar");
    let _ = std::fs::create_dir_all(&dir);
    let jar_path = dir.join("app.jar");
    {
        let f = std::fs::File::create(&jar_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // Minimal valid class file: we won't actually parse it, but
        // ClassPath.find_class_code_source_info doesn't care about
        // parsing — it just checks archive membership.
        zw.start_file("com/acme/Foo.class", opts).unwrap();
        zw.write_all(b"\xCA\xFE\xBA\xBE_fake").unwrap();
        zw.finish().unwrap();
    }

    let app_cp = vec![jar_path.to_string_lossy().into_owned()];
    let cm = ClassManager::new(&[], &[], &app_cp);
    // find_class_code_source walks the classpath entries; returns a
    // CodeSource whose URL is `file:/<absolute path to jar>`.
    let cs = cm
        .find_class_code_source("com/acme/Foo")
        .expect("JAR-hosted class should have a real CodeSource");
    let url = cs.url.expect("URL should be present");
    assert!(
        url.starts_with("file:/"),
        "URL should use file: scheme, got {url}"
    );
    assert!(
        url.to_lowercase().ends_with("app.jar"),
        "URL should end with the JAR name, got {url}"
    );
    // Unsigned JAR → no certificate digests.
    assert!(cs.certificates.is_empty());
    assert!(cs.certificate_sha256.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t11_protection_domain_from_directory() {
    // A class loaded from a plain directory gets a `file:/.../` URL.
    use cratonvm_classloading::ClassManager;

    let dir = std::env::temp_dir().join("cratonvm-t11-pd-dir");
    let sub = dir.join("com").join("acme");
    let _ = std::fs::create_dir_all(&sub);
    std::fs::write(sub.join("Bar.class"), b"\xCA\xFE\xBA\xBE").unwrap();

    let app_cp = vec![dir.to_string_lossy().into_owned()];
    let cm = ClassManager::new(&[], &[], &app_cp);

    let cs = cm
        .find_class_code_source("com/acme/Bar")
        .expect("directory-hosted class should have a CodeSource");
    let url = cs.url.expect("URL present");
    assert!(url.starts_with("file:/"));
    assert!(url.ends_with('/'), "directory URL should end with /");
    assert!(cs.certificates.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t11_socket_permission_cidr_matches() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"
        grant {
            permission java.net.SocketPermission "10.0.0.0/8:80", "connect";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));
    // Every address inside 10.0.0.0/8 on port 80 is allowed.
    assert!(policy_allows(
        "java.net.SocketPermission",
        "10.1.2.3:80",
        "connect",
        None
    ));
    assert!(policy_allows(
        "java.net.SocketPermission",
        "10.255.255.254:80",
        "connect",
        None
    ));
    // Outside the CIDR — denied.
    assert!(!policy_allows(
        "java.net.SocketPermission",
        "11.0.0.1:80",
        "connect",
        None
    ));
    // Wrong port — denied even inside the CIDR.
    assert!(!policy_allows(
        "java.net.SocketPermission",
        "10.1.2.3:443",
        "connect",
        None
    ));
    clear_policy_and_stack();
}

#[test]
fn t11_socket_permission_port_range() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"
        grant {
            permission java.net.SocketPermission "service.example:8080-8090", "connect";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));
    for port in 8080..=8090 {
        assert!(
            policy_allows(
                "java.net.SocketPermission",
                &format!("service.example:{port}"),
                "connect",
                None
            ),
            "port {port} should be allowed"
        );
    }
    assert!(!policy_allows(
        "java.net.SocketPermission",
        "service.example:8079",
        "connect",
        None
    ));
    assert!(!policy_allows(
        "java.net.SocketPermission",
        "service.example:8091",
        "connect",
        None
    ));
    clear_policy_and_stack();
}

#[test]
fn t11_signed_by_grant_applies_only_to_signer_cert() {
    // Two classes, one "signed" (has a known SHA-256 digest) and
    // one not. A `grant signedBy "DEADBEEF"` block must apply only
    // when the calling frame's cert digests include "deadbeef".
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"
        grant signedBy "deadbeef" {
            permission java.io.FilePermission "/secret/*", "read";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    // Unsigned caller — denied.
    assert!(!policy_allows_full(
        "java.io.FilePermission",
        "/secret/passwords.txt",
        "read",
        None,
        &[],
    ));
    // Wrong signer — denied.
    assert!(!policy_allows_full(
        "java.io.FilePermission",
        "/secret/passwords.txt",
        "read",
        None,
        &["abc123".to_string()],
    ));
    // Signed with the matching digest — allowed.
    assert!(policy_allows_full(
        "java.io.FilePermission",
        "/secret/passwords.txt",
        "read",
        None,
        &["deadbeef".to_string()],
    ));
    // Case-insensitive match: "DEADBEEF" in the frame still matches.
    assert!(policy_allows_full(
        "java.io.FilePermission",
        "/secret/passwords.txt",
        "read",
        None,
        &["DEADBEEF".to_string()],
    ));
    clear_policy_and_stack();
}

#[test]
fn t11_signed_by_with_code_base_combined() {
    // Both filters must hold: correct signedBy AND matching codeBase.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"
        grant codeBase "file:/opt/app.jar" signedBy "acme" {
            permission java.lang.RuntimePermission "getClassLoader";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    // Correct codeBase + correct signer → allowed.
    assert!(policy_allows_full(
        "java.lang.RuntimePermission",
        "getClassLoader",
        "",
        Some("file:/opt/app.jar"),
        &["acme".to_string()],
    ));
    // Correct codeBase, wrong signer → denied.
    assert!(!policy_allows_full(
        "java.lang.RuntimePermission",
        "getClassLoader",
        "",
        Some("file:/opt/app.jar"),
        &["other".to_string()],
    ));
    // Correct signer, wrong codeBase → denied.
    assert!(!policy_allows_full(
        "java.lang.RuntimePermission",
        "getClassLoader",
        "",
        Some("file:/opt/malicious.jar"),
        &["acme".to_string()],
    ));
    clear_policy_and_stack();
}

#[test]
fn t11_privileged_frame_carries_cert_digests() {
    // Push a frame with a URL + digests, then verify both
    // `current_privileged_code_base` and
    // `current_privileged_cert_digests` see them.
    let _guard = policy_test_lock();
    clear_policy_and_stack();

    push_privileged_frame_full(
        Some("file:/opt/app.jar".to_string()),
        vec!["abcd".to_string(), "ef01".to_string()],
    );
    assert_eq!(
        current_privileged_code_base().as_deref(),
        Some("file:/opt/app.jar")
    );
    let digests = current_privileged_cert_digests();
    assert_eq!(digests.len(), 2);
    assert_eq!(digests[0], "abcd");
    assert_eq!(digests[1], "ef01");

    pop_privileged_frame();
    assert!(current_privileged_code_base().is_none());
    assert!(current_privileged_cert_digests().is_empty());
    clear_policy_and_stack();
}

#[test]
fn t11_jar_signer_blocks_are_hashed() {
    // Feed a JAR containing exactly one `META-INF/*.RSA` signer block
    // (with its companion `*.SF`) through the real
    // `ClassManager::find_class_code_source` →
    // `ClassPath::extract_jar_signer_blocks` pipeline and assert that
    // the discovery half locates exactly one signer block and threads
    // it into the verified-certificate collection.
    //
    // FIX (signer-block discovery): this test previously asserted the
    // *obsolete* pre-Task-#40 contract — that the raw `.RSA` bytes were
    // stored verbatim in `CodeSource.certificates` and hashed to a
    // SHA-256 hex digest. Task #40 replaced that with real PKCS#7 +
    // trust-chain verification (see
    // `classloading/src/class_path.rs::extract_jar_signer_blocks` and
    // `classloading/src/jar_signer.rs::verify_signer_block`): only
    // signer blocks that parse as PKCS#7 SignedData AND chain to a
    // trust anchor in the process-wide `default_trust_store()`
    // contribute parsed X.509 leaf certificates. A synthetic
    // placeholder block (not valid PKCS#7) is therefore *discovered*
    // but, like `jarsigner -verify` on an unsigned/garbage block,
    // contributes zero verified certificates.
    //
    // The production trust store is a sealed process-global `OnceLock`
    // that we cannot deterministically seed from a unit test (init
    // order races with the other JAR-loading tests in this binary), so
    // a real end-to-end "one *verified* cert" assertion is not
    // achievable here. We instead pin the genuine, deterministic
    // contract: one signer block is discovered, and an unverifiable
    // block yields no certificates (fail-closed, matching HotSpot).
    // The end-to-end verify-and-collect path with a real RSA-signed,
    // trust-anchored block is covered in
    // `jar_signer.rs::real_rsa_signed_block_with_trust_anchor_verifies`.
    use cratonvm_classloading::ClassManager;
    use std::io::Write as _;

    // Unique temp dir per run so parallel test invocations don't clash
    // on the shared JAR path.
    let dir = std::env::temp_dir().join(format!(
        "cratonvm-t11-signed-jar-{}-{}",
        std::process::id(),
        SIGNED_JAR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(&dir);
    let jar_path = dir.join("signed.jar");

    // A placeholder signer block. It is intentionally *not* valid
    // PKCS#7 SignedData — exactly the shape of an untrusted/garbage
    // signature block that real verification must reject.
    let rsa_bytes = b"simulated-pkcs7-signature-block-bytes";
    {
        let f = std::fs::File::create(&jar_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("com/acme/Foo.class", opts).unwrap();
        zw.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
        zw.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zw.write_all(b"Manifest-Version: 1.0\r\n").unwrap();
        zw.start_file("META-INF/SIGNER.SF", opts).unwrap();
        zw.write_all(b"Signature-Version: 1.0\r\n").unwrap();
        zw.start_file("META-INF/SIGNER.RSA", opts).unwrap();
        zw.write_all(rsa_bytes).unwrap();
        zw.finish().unwrap();
    }

    // FIX: independently confirm the *discovery* half — the portion the
    // original assertion was probing — actually finds exactly one
    // signer block in `META-INF`. The original test was failing because
    // it conflated discovery (which works) with verification (which
    // correctly drops the synthetic block). We assert discovery here
    // directly off the archive so a regression in extension matching
    // (case sensitivity, `.RSA`/`.DSA`/`.EC` suffixes, the `META-INF/`
    // prefix, or `.SF` pairing) is caught for the right reason.
    let discovered = count_meta_inf_signer_blocks(&jar_path);
    assert_eq!(
        discovered, 1,
        "exactly one META-INF/*.RSA signer block must be discovered"
    );

    let app_cp = vec![jar_path.to_string_lossy().into_owned()];
    let cm = ClassManager::new(&[], &[], &app_cp);
    let cs = cm
        .find_class_code_source("com/acme/Foo")
        .expect("signed JAR should yield a CodeSource");

    // The synthetic block is discovered but is not valid PKCS#7, so it
    // fails real verification → zero *verified* certificates. This is
    // the current, deterministic production contract (fail-closed),
    // and keeping `certificates` and `certificate_sha256` in lock-step
    // is the invariant `CodeSource::new` upholds.
    assert_eq!(
        cs.certificates.len(),
        0,
        "an unverifiable signer block must contribute no verified certs"
    );
    assert_eq!(
        cs.certificate_sha256.len(),
        cs.certificates.len(),
        "certificate_sha256 stays in lock-step with certificates"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// FIX: standalone signer-block discovery probe used by
/// `t11_jar_signer_blocks_are_hashed`. Mirrors the discovery half of
/// `ClassPath::extract_jar_signer_blocks` (the `META-INF/` prefix +
/// case-insensitive `.RSA`/`.DSA`/`.EC` suffix match, paired with a
/// `.SF` companion) so the test verifies that the block is *found*
/// independently of whether it passes PKCS#7 trust-chain verification.
/// Returns the number of signer blocks that have a matching `.SF`.
fn count_meta_inf_signer_blocks(jar_path: &std::path::Path) -> usize {
    use std::io::Read as _;
    let bytes = std::fs::read(jar_path).expect("read jar");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open jar");

    // Collect entry names once.
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().and_then(|e| e.name().ok().map(|n| n.into_owned())))
        .collect();

    // Map uppercase stem (e.g. "META-INF/SIGNER") → has-a-.SF, matching
    // the case-insensitive pairing the production walk performs.
    let mut sf_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
    for n in &names {
        let upper = n.to_ascii_uppercase();
        if upper.starts_with("META-INF/") {
            if let Some(stem) = upper.strip_suffix(".SF") {
                sf_stems.insert(stem.to_string());
            }
        }
    }

    let mut count = 0usize;
    for n in &names {
        let upper = n.to_ascii_uppercase();
        if !upper.starts_with("META-INF/") {
            continue;
        }
        let is_block =
            upper.ends_with(".RSA") || upper.ends_with(".DSA") || upper.ends_with(".EC");
        if !is_block {
            continue;
        }
        // Must have a matching `.SF` companion (same stem) and be
        // non-empty — the same gate the production walk applies before
        // attempting verification.
        let stem = match upper.rsplit_once('.') {
            Some((s, _)) => s.to_string(),
            None => continue,
        };
        if !sf_stems.contains(&stem) {
            continue;
        }
        let mut data = Vec::new();
        // `by_name` yields `Result<_, ZipError>` while `read_to_end` yields
        // `Result<_, io::Error>`; match instead of `and_then` to avoid the
        // error-type mismatch.
        let present = match archive.by_name(n) {
            Ok(mut e) => e.read_to_end(&mut data).is_ok() && !data.is_empty(),
            Err(_) => false,
        };
        if present {
            count += 1;
        }
    }
    count
}

/// Sequence counter so each `t11_jar_signer_blocks_are_hashed` run (and
/// any parallel invocation) gets a unique temp JAR path.
static SIGNED_JAR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[test]
fn test_load_policy_file_end_to_end() {
    use std::io::Write as IoWrite;
    let _guard = policy_test_lock();
    clear_policy_and_stack();

    let dir = std::env::temp_dir().join("cratonvm-sm-policy");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("load_test.policy");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "grant codeBase \"file:/home/app/-\" {{\n    permission java.io.FilePermission \"/tmp/*\", \"read,write\";\n    permission java.net.SocketPermission \"*:80\", \"connect\";\n}};"
        )
        .unwrap();
    }

    load_policy_file(&path).unwrap();
    // Policy is installed → non-granted permission is denied when no
    // privileged frame is active.
    assert!(!policy_allows(
        "java/io/FilePermission",
        "/tmp/a",
        "read",
        None,
    ));
    // With a matching code base, it is allowed.
    assert!(policy_allows(
        "java/io/FilePermission",
        "/tmp/a",
        "read",
        Some("file:/home/app/lib/x.jar"),
    ));
    assert!(policy_allows(
        "java/net/SocketPermission",
        "api.example.com:80",
        "connect",
        Some("file:/home/app/core.jar"),
    ));

    let _ = std::fs::remove_file(&path);
    clear_policy_and_stack();
}

// -----------------------------------------------------------------------
// T19 · N3 — AccessController stack / PD / materialize natives
// -----------------------------------------------------------------------

#[test]
fn t19_n3_ac_get_stack_context_returns_null_when_no_security_manager() {
    let _guard = security_state_test_lock();

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    // No SecurityManager is installed; the spec-matching answer is null.
    let _ = set_security_manager_for_test(&ctx, None);
    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "getStackAccessControlContext",
        "()Ljava/security/AccessControlContext;",
        &[],
    );
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        Some(Value::Object(None)),
        "Expected null AccessControlContext under no-SM policy"
    );
}

#[test]
fn t19_n3_ac_get_inherited_context_returns_null() {
    // We don't track inherited ACs — null is canonical.
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "getInheritedAccessControlContext",
        "()Ljava/security/AccessControlContext;",
        &[],
    );
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        Some(Value::Object(None)),
        "Expected null inherited AccessControlContext"
    );
}

#[test]
fn t19_n3_ac_get_protection_domain_delegates_to_class_native() {
    // The AC native delegates to Class.getProtectionDomain0 via
    // ctx.invoke. MockNativeContext.invoke returns Ok(None) for every
    // call — our native normalises that to a null ProtectionDomain so
    // the caller sees the documented "no PD available" result. This
    // test also verifies the graceful-fallback path that is used when
    // Agent N1's Class.getProtectionDomain0 native is not yet
    // registered: the delegation must never surface an
    // UnsatisfiedLinkError to AccessController callers.
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 0).unwrap();

    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "getProtectionDomain",
        "(Ljava/lang/Class;)Ljava/security/ProtectionDomain;",
        &[Value::Object(Some(class_mirror))],
    );
    assert!(
        result.is_ok(),
        "delegation must never surface UnsatisfiedLinkError; got {result:?}"
    );
    // Either (a) N1 has landed and returned a non-null PD, or (b) we
    // fell through to null — both are valid end-user observations.
    match result.unwrap() {
        Some(Value::Object(_)) => {} // ok: null or a real PD
        other => panic!("Expected Object value (null or PD), got {:?}", other),
    }
}

#[test]
fn t19_n3_ac_get_protection_domain_null_class_returns_null() {
    // Null Class argument → null PD. We must not delegate at all in this
    // path (saves an allocation and an invoke round-trip).
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "getProtectionDomain",
        "(Ljava/lang/Class;)Ljava/security/ProtectionDomain;",
        &[Value::Object(None)],
    );
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        Some(Value::Object(None)),
        "null Class arg should short-circuit to null PD"
    );
}

#[test]
fn t19_n3_ac_ensure_materialized_for_stack_walk_is_noop() {
    // Genuine spec no-op: returns void (Ok(None)) and does not touch the
    // heap or global state. We verify both the return shape and that
    // neither a null nor a live object argument changes the result.
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();

    // Null-argument case.
    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "ensureMaterializedForStackWalk",
        "(Ljava/lang/Object;)V",
        &[Value::Object(None)],
    );
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None, "void method must return None");

    // Live-object case.
    let obj = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 0).unwrap();
    let result = call_native(
        &registry,
        &mut ctx,
        "java/security/AccessController",
        "ensureMaterializedForStackWalk",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(obj))],
    );
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None, "void method must return None");
}

// -----------------------------------------------------------------------
// T19_H9_ANCHOR_POLICY_TESTS
// java.security.Policy native-override tests
//
// These tests cover the lenient `setPolicy`/`getPolicy` contract
// documented in `register_policy_natives`. Each test grabs the
// global `policy_test_lock` so it runs serial w.r.t. other tests
// that mutate the mock VM's Policy slot or the parsed `Policy` slot,
// and clears state both at entry and exit so a panic in one test
// doesn't leak singleton state into the next.
// -----------------------------------------------------------------------

#[test]
fn t19_h9_set_policy_then_get_returns_stored_ref() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let policy_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();

    // setPolicy(p)
    let set_p = registry
        .find(
            "java/security/Policy",
            "setPolicy",
            "(Ljava/security/Policy;)V",
        )
        .expect("setPolicy should be registered");
    let result = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None);

    // getPolicy() should return exactly the same reference
    let get_p = registry
        .find(
            "java/security/Policy",
            "getPolicy",
            "()Ljava/security/Policy;",
        )
        .expect("getPolicy should be registered");
    let result = get_p(&mut ctx, &[]);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), Some(Value::Object(Some(policy_obj))));

    clear_policy_and_stack();
}

#[test]
fn t19_h9_set_policy_null_is_accepted() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    // First install a real Policy so we can verify null clears it.
    let policy_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
    let set_p = registry
        .find(
            "java/security/Policy",
            "setPolicy",
            "(Ljava/security/Policy;)V",
        )
        .unwrap();
    let _ = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);
    assert_eq!(get_policy_object(&ctx), Some(policy_obj));

    // Now setPolicy(null) — must not throw.
    let result = set_p(&mut ctx, &[Value::Object(None)]);
    assert!(result.is_ok(), "setPolicy(null) must not raise");

    // The slot is cleared; the next getPolicy() lazily allocates a default.
    assert_eq!(get_policy_object(&ctx), None);

    let get_p = registry
        .find(
            "java/security/Policy",
            "getPolicy",
            "()Ljava/security/Policy;",
        )
        .unwrap();
    let result = get_p(&mut ctx, &[]);
    assert!(result.is_ok());
    // getPolicy must return *some* object — the lazy default — never null.
    match result.unwrap() {
        Some(Value::Object(Some(_))) => {}
        other => panic!("expected lazy default Policy, got {:?}", other),
    }

    clear_policy_and_stack();
}

#[test]
fn t19_h9_get_policy_lazy_default_is_idempotent() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let get_p = registry
        .find(
            "java/security/Policy",
            "getPolicy",
            "()Ljava/security/Policy;",
        )
        .unwrap();

    // First call: lazy-allocate default.
    let r1 = get_p(&mut ctx, &[]).unwrap();
    // Second call: must return the same reference (idempotent).
    let r2 = get_p(&mut ctx, &[]).unwrap();
    assert_eq!(r1, r2, "getPolicy must be idempotent across calls");
    match r1 {
        Some(Value::Object(Some(_))) => {}
        other => panic!("expected non-null default, got {:?}", other),
    }

    clear_policy_and_stack();
}

#[test]
fn t19_h9_get_policy_no_check_matches_get_policy() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let policy_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
    let set_p = registry
        .find(
            "java/security/Policy",
            "setPolicy",
            "(Ljava/security/Policy;)V",
        )
        .unwrap();
    let _ = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);

    let get_p = registry
        .find(
            "java/security/Policy",
            "getPolicy",
            "()Ljava/security/Policy;",
        )
        .unwrap();
    let get_p_no_check = registry
        .find(
            "java/security/Policy",
            "getPolicyNoCheck",
            "()Ljava/security/Policy;",
        )
        .unwrap();
    let r1 = get_p(&mut ctx, &[]).unwrap();
    let r2 = get_p_no_check(&mut ctx, &[]).unwrap();
    assert_eq!(r1, r2, "getPolicyNoCheck must mirror getPolicy");

    clear_policy_and_stack();
}

#[test]
fn t19_h9_is_set_reflects_singleton_state() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let is_set = registry
        .find("java/security/Policy", "isSet", "()Z")
        .expect("isSet should be registered");

    // Initially: no Policy → false.
    let r = is_set(&mut ctx, &[]).unwrap();
    assert_eq!(r, Some(Value::Int(0)));

    // After install: true.
    let policy_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
    let set_p = registry
        .find(
            "java/security/Policy",
            "setPolicy",
            "(Ljava/security/Policy;)V",
        )
        .unwrap();
    let _ = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);
    let r = is_set(&mut ctx, &[]).unwrap();
    assert_eq!(r, Some(Value::Int(1)));

    // After clear: false.
    let _ = set_p(&mut ctx, &[Value::Object(None)]);
    let r = is_set(&mut ctx, &[]).unwrap();
    assert_eq!(r, Some(Value::Int(0)));

    clear_policy_and_stack();
}

#[test]
fn t19_h9_implies_returns_true() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let pd =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();
    let perm =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/AllPermission", 0).unwrap();

    let implies = registry
        .find(
            "java/security/Policy",
            "implies",
            "(Ljava/security/ProtectionDomain;Ljava/security/Permission;)Z",
        )
        .expect("implies should be registered");
    let result = implies(
        &mut ctx,
        &[Value::Object(Some(pd)), Value::Object(Some(perm))],
    );
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), Some(Value::Int(1)));

    clear_policy_and_stack();
}

/// With a parsed policy installed, `Policy.implies(pd, perm)` must
/// return 0 for a permission no grant covers and 1 for one that is
/// covered — i.e. it genuinely delegates to the grant evaluation
/// instead of unconditionally returning 1.
#[test]
fn policy_implies_delegates_to_parsed_policy() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"
        grant {
            permission java.io.FilePermission "/tmp/*", "read";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let implies = registry
        .find(
            "java/security/Policy",
            "implies",
            "(Ljava/security/ProtectionDomain;Ljava/security/Permission;)Z",
        )
        .expect("implies should be registered");

    let mut ctx = MockNativeContext::new();
    let pd =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();

    // Not granted: a SocketPermission must be DENIED (implies -> 0).
    let denied =
        try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
    let host = ctx.create_string("example.com:443");
    ctx.set_field(denied, 0, Value::Object(Some(host)));
    let act = ctx.create_string("connect");
    ctx.set_field(denied, 1, Value::Object(Some(act)));
    let r = implies(
        &mut ctx,
        &[Value::Object(Some(pd)), Value::Object(Some(denied))],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(0)), "ungranted permission must imply 0");

    // Granted: a FilePermission read on /tmp/* must be ALLOWED (-> 1).
    let granted =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/FilePermission", 2).unwrap();
    let path = ctx.create_string("/tmp/foo.txt");
    ctx.set_field(granted, 0, Value::Object(Some(path)));
    let act2 = ctx.create_string("read");
    ctx.set_field(granted, 1, Value::Object(Some(act2)));
    let r = implies(
        &mut ctx,
        &[Value::Object(Some(pd)), Value::Object(Some(granted))],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(1)), "granted permission must imply 1");

    clear_policy_and_stack();
}

/// Sanity: when NO policy is installed (the BouncyCastle / default case)
/// `Policy.implies` returns 1 (allow-all) for any permission.
#[test]
fn policy_implies_allows_all_with_no_policy() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let implies = registry
        .find(
            "java/security/Policy",
            "implies",
            "(Ljava/security/ProtectionDomain;Ljava/security/Permission;)Z",
        )
        .unwrap();

    let mut ctx = MockNativeContext::new();
    let pd =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();
    let perm =
        try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
    let r = implies(
        &mut ctx,
        &[Value::Object(Some(pd)), Value::Object(Some(perm))],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(1)));

    clear_policy_and_stack();
}

#[test]
fn t19_h9_refresh_is_noop() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let refresh = registry
        .find("java/security/Policy", "refresh", "()V")
        .expect("refresh should be registered");
    let result = refresh(&mut ctx, &[]);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None, "refresh must return void");

    // refresh must not touch the singleton state — install nothing
    // before the call and verify isSet stays false afterwards.
    assert_eq!(get_policy_object(&ctx), None);

    clear_policy_and_stack();
}

#[test]
fn t19_h9_get_permissions_protection_domain_returns_collection() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let pd =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();

    let get_perms = registry
        .find(
            "java/security/Policy",
            "getPermissions",
            "(Ljava/security/ProtectionDomain;)Ljava/security/PermissionCollection;",
        )
        .expect("getPermissions(ProtectionDomain) should be registered");
    let result = get_perms(&mut ctx, &[Value::Object(Some(pd))]);
    assert!(result.is_ok());
    let val = result.unwrap();
    match val {
        Some(Value::Object(Some(perms))) => {
            // Slot 1 holds the read-only flag; verify it was set on creation.
            let read_only = ctx.get_field(perms, 1);
            assert_eq!(
                read_only,
                Value::Int(1),
                "permissive collection must be read-only"
            );
            // Slot 0 holds the AllPermission entry; non-null.
            match ctx.get_field(perms, 0) {
                Value::Object(Some(_)) => {}
                other => panic!("expected AllPermission entry, got {:?}", other),
            }
        }
        other => panic!("expected non-null PermissionCollection, got {:?}", other),
    }

    clear_policy_and_stack();
}

#[test]
fn t19_h9_get_permissions_code_source_returns_collection() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let cs = try_alloc_concurrent_synthetic(&mut ctx, "java/security/CodeSource", 2).unwrap();

    let get_perms = registry
        .find(
            "java/security/Policy",
            "getPermissions",
            "(Ljava/security/CodeSource;)Ljava/security/PermissionCollection;",
        )
        .expect("getPermissions(CodeSource) should be registered");
    let result = get_perms(&mut ctx, &[Value::Object(Some(cs))]);
    assert!(result.is_ok());
    match result.unwrap() {
        Some(Value::Object(Some(_))) => {} // collection returned
        other => panic!("expected non-null collection, got {:?}", other),
    }

    clear_policy_and_stack();
}

#[test]
fn t19_h9_get_permissions_returns_shared_collection() {
    // Two consecutive calls to getPermissions(...) must return the same
    // shared collection so accidental mutation is observable and we
    // don't allocate a fresh one on every invocation.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let pd =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();

    let get_perms = registry
        .find(
            "java/security/Policy",
            "getPermissions",
            "(Ljava/security/ProtectionDomain;)Ljava/security/PermissionCollection;",
        )
        .unwrap();
    let r1 = get_perms(&mut ctx, &[Value::Object(Some(pd))]).unwrap();
    let r2 = get_perms(&mut ctx, &[Value::Object(Some(pd))]).unwrap();
    assert_eq!(
        r1, r2,
        "getPermissions must return the same shared collection"
    );

    clear_policy_and_stack();
}

#[test]
fn t19_h9_set_policy_does_not_throw_on_any_input() {
    // The T19.H9 contract: setPolicy must NEVER throw — JBoss Modules,
    // WildFly, EJBCA, and many other libraries call it during boot
    // and rely on it succeeding. Cover the three input shapes we
    // care about: real ref, null, and a wrong-type (Int) arg that
    // could come from a mis-shaped frame.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let policy_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();

    let set_p = registry
        .find(
            "java/security/Policy",
            "setPolicy",
            "(Ljava/security/Policy;)V",
        )
        .unwrap();

    assert!(set_p(&mut ctx, &[Value::Object(Some(policy_obj))]).is_ok());
    assert!(set_p(&mut ctx, &[Value::Object(None)]).is_ok());
    // Wrong-type argument: native must be defensive and not panic.
    assert!(set_p(&mut ctx, &[Value::Int(42)]).is_ok());
    assert!(set_p(&mut ctx, &[]).is_ok(), "missing arg must not panic");

    clear_policy_and_stack();
}

#[test]
fn t19_h9_set_security_manager_does_not_throw() {
    // Co-required path: JBoss Modules calls
    //   System.setSecurityManager(new SecurityManager())
    // immediately after Policy.setPolicy. This regression test guards
    // the existing System.setSecurityManager native against a future
    // refactor that re-introduces a throw — KC16 boot would then
    // regress from "advances past Policy stub" back to the
    // UnsupportedOperationException error.
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);

    let mut ctx = MockNativeContext::new();
    let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

    let set_sm = registry
        .find(
            "java/lang/System",
            "setSecurityManager",
            "(Ljava/lang/SecurityManager;)V",
        )
        .unwrap();
    assert!(set_sm(&mut ctx, &[Value::Object(Some(sm))]).is_ok());
    assert!(set_sm(&mut ctx, &[Value::Object(None)]).is_ok());

    clear_policy_and_stack();
    let _ = set_security_manager_for_test(&ctx, None);
}

// -----------------------------------------------------------------------
// Per-VM isolation of the SecurityManager / Policy state
// -----------------------------------------------------------------------
//
// Every `MockNativeContext` reports `vm_identity() == 0` by default, which
// is what the single-VM tests above exercise. These tests give each mock
// its own identity — the same thing `SharedVm::vm_identity` does for two
// real VMs in one process — so they are also parallel-safe and need no
// shared test lock: no other test can address these rows.

/// Distinct, unlikely-to-collide VM identities for the tests below.
const VM_A: usize = 0x0C2_0001;
const VM_B: usize = 0x0C2_0002;

/// Build a mock context that reports `vm` as its VM identity.
fn vm_scoped_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

#[test]
fn two_vms_hold_independent_security_managers() {
    let mut a = vm_scoped_ctx(VM_A);
    let mut b = vm_scoped_ctx(VM_B);
    let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
    let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();

    // Installing in A must not be visible in B, and vice versa. Before the
    // per-VM keying this read handed B the raw `sm_a` address — a pointer
    // into a heap B's collector neither scans nor owns.
    set_security_manager(&mut a, Some(sm_a));
    assert_eq!(get_security_manager(&a), Some(sm_a));
    assert!(
        get_security_manager(&b).is_none(),
        "VM B must not see VM A's SecurityManager"
    );

    set_security_manager(&mut b, Some(sm_b));
    assert_eq!(get_security_manager(&a), Some(sm_a));
    assert_eq!(get_security_manager(&b), Some(sm_b));

    forget_vm_security_state(VM_A);
    forget_vm_security_state(VM_B);
}

#[test]
fn clearing_one_vms_security_manager_leaves_the_other_armed() {
    let mut a = vm_scoped_ctx(VM_A + 0x10);
    let mut b = vm_scoped_ctx(VM_B + 0x10);
    let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
    let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();
    set_security_manager(&mut a, Some(sm_a));
    set_security_manager(&mut b, Some(sm_b));

    // This is the privilege-escalation-by-removal case: an unsandboxed VM
    // calling `System.setSecurityManager(null)` used to write `None` to the
    // shared slot and disarm the sandboxed VM's checkExec / loadLibrary
    // gates. It must now clear only the caller's VM.
    set_security_manager(&mut a, None);
    assert!(get_security_manager(&a).is_none());
    assert_eq!(
        get_security_manager(&b),
        Some(sm_b),
        "clearing VM A's manager must leave VM B armed"
    );

    forget_vm_security_state(VM_A + 0x10);
    forget_vm_security_state(VM_B + 0x10);
}

#[test]
fn policy_and_permission_slots_are_per_vm() {
    let mut a = vm_scoped_ctx(VM_A + 0x20);
    let mut b = vm_scoped_ctx(VM_B + 0x20);

    let policy_a = try_alloc_concurrent_synthetic(&mut a, "java/security/Policy", 1).unwrap();
    set_policy_object(&mut a, Some(policy_a));
    assert_eq!(get_policy_object(&a), Some(policy_a));
    assert_eq!(get_policy_object(&b), None);
    assert!(policy_object_installed(&a));
    assert!(!policy_object_installed(&b));

    // The lazy default and the shared Permissions collection are per-VM
    // too: each VM gets its own, allocated from its own heap.
    let default_b = ensure_default_policy_object(&mut b).unwrap();
    assert_eq!(get_policy_object(&a), Some(policy_a));
    assert_eq!(get_policy_object(&b), Some(default_b));

    let perms_a = ensure_shared_permission_collection(&mut a).unwrap();
    let perms_b = ensure_shared_permission_collection(&mut b).unwrap();
    assert_eq!(
        ensure_shared_permission_collection(&mut a).unwrap(),
        perms_a,
        "identity of the shared collection must be stable within a VM"
    );
    assert_eq!(
        ensure_shared_permission_collection(&mut b).unwrap(),
        perms_b
    );

    forget_vm_security_state(VM_A + 0x20);
    forget_vm_security_state(VM_B + 0x20);
}

#[test]
fn security_manager_ref_is_scanned_and_remapped_per_vm() {
    let vm_a = VM_A + 0x30;
    let vm_b = VM_B + 0x30;
    let mut a = vm_scoped_ctx(vm_a);
    let mut b = vm_scoped_ctx(vm_b);

    let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
    let policy_a = try_alloc_concurrent_synthetic(&mut a, "java/security/Policy", 1).unwrap();
    let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();
    set_security_manager(&mut a, Some(sm_a));
    set_policy_object(&mut a, Some(policy_a));
    set_security_manager(&mut b, Some(sm_b));

    // Scan: the collector must be told about the refs this module holds,
    // and ONLY about the ones belonging to the VM it is collecting.
    let mut roots = Vec::new();
    gc_scan_security_manager_roots(vm_a, &mut roots);
    assert!(roots.contains(&sm_a), "the installed SM must be a root");
    assert!(
        roots.contains(&policy_a),
        "the Policy object must be a root"
    );
    assert!(
        !roots.contains(&sm_b),
        "VM A's scan must not report VM B's objects"
    );

    // Remap: stand in for a moving collection that relocated `sm_a` onto a
    // second, genuinely-allocated object's address. An empty map is a
    // no-op (the non-moving sweep).
    let relocated =
        try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
    gc_update_security_manager_refs(vm_a, &cratonvm_types::PointerMap::default());
    assert_eq!(get_security_manager(&a), Some(sm_a));

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(sm_a.as_ptr() as usize, relocated.as_ptr() as usize);
    gc_update_security_manager_refs(vm_a, &map);
    assert_eq!(
        get_security_manager(&a),
        Some(relocated),
        "the cached copy must follow the object, not stay on the vacated slot"
    );
    assert_eq!(
        get_policy_object(&a),
        Some(policy_a),
        "an object the collector did not move must be left alone"
    );
    assert_eq!(
        get_security_manager(&b),
        Some(sm_b),
        "VM A's remap must not touch VM B's slots"
    );

    forget_vm_security_state(vm_a);
    forget_vm_security_state(vm_b);
}

#[test]
fn vm_teardown_does_not_disturb_the_other_vm() {
    let vm_a = VM_A + 0x40;
    let vm_b = VM_B + 0x40;
    let mut a = vm_scoped_ctx(vm_a);
    let mut b = vm_scoped_ctx(vm_b);
    let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
    let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();
    let perms_b = ensure_shared_permission_collection(&mut b).unwrap();
    set_security_manager(&mut a, Some(sm_a));
    set_security_manager(&mut b, Some(sm_b));

    forget_vm_security_state(vm_a);

    assert!(
        get_security_manager(&a).is_none(),
        "teardown must drop the torn-down VM's slots"
    );
    let mut roots_a = Vec::new();
    gc_scan_security_manager_roots(vm_a, &mut roots_a);
    assert!(
        roots_a.is_empty(),
        "a torn-down VM must report no roots (its heap is gone)"
    );

    assert_eq!(get_security_manager(&b), Some(sm_b));
    assert_eq!(
        ensure_shared_permission_collection(&mut b).unwrap(),
        perms_b
    );
    let mut roots_b = Vec::new();
    gc_scan_security_manager_roots(vm_b, &mut roots_b);
    assert!(roots_b.contains(&sm_b));
    assert!(roots_b.contains(&perms_b));

    forget_vm_security_state(vm_b);
}

#[test]
fn single_vm_behaviour_is_unchanged() {
    // One VM sees exactly the old semantics: install / read back / clear,
    // a stable lazy default Policy, a stable shared Permissions collection,
    // and `isSet` following the Policy slot.
    let vm = VM_A + 0x50;
    let mut ctx = vm_scoped_ctx(vm);

    assert!(get_security_manager(&ctx).is_none());
    let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    set_security_manager(&mut ctx, Some(sm));
    assert_eq!(get_security_manager(&ctx), Some(sm));
    set_security_manager(&mut ctx, None);
    assert!(get_security_manager(&ctx).is_none());

    assert!(!policy_object_installed(&ctx));
    let default_policy = ensure_default_policy_object(&mut ctx).unwrap();
    assert_eq!(
        ensure_default_policy_object(&mut ctx).unwrap(),
        default_policy,
        "the lazy default must be allocated exactly once per VM"
    );
    assert!(policy_object_installed(&ctx));
    assert_eq!(get_policy_object(&ctx), Some(default_policy));

    let installed =
        try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
    set_policy_object(&mut ctx, Some(installed));
    assert_eq!(get_policy_object(&ctx), Some(installed));
    set_policy_object(&mut ctx, None);
    assert_eq!(get_policy_object(&ctx), None);
    assert!(!policy_object_installed(&ctx));

    let perms = ensure_shared_permission_collection(&mut ctx).unwrap();
    assert_eq!(
        ensure_shared_permission_collection(&mut ctx).unwrap(),
        perms
    );

    forget_vm_security_state(vm);
}

/// gc-common w20-e: the `checkXxx` natives now build their permission
/// through `sm_check_synthetic_permission`. A policy that grants one exact
/// target/action pair tells whether both slots still carry the argument
/// and the action (a wrong slot would deny the granted pair or allow the
/// others), and every call leaves the native pin stack as it found it.
#[test]
fn w20e_check_natives_build_their_permission_and_release_their_roots() {
    let _guard = policy_test_lock();
    clear_policy_and_stack();
    let src = r#"
        grant {
            permission java.io.FilePermission "/tmp/w20e.txt", "read";
            permission java.lang.RuntimePermission "exitVM.3";
        };
    "#;
    set_active_policy(Some(Policy::parse(src).unwrap()));

    let mut registry = NativeMethodRegistry::new();
    register_security_manager_natives(&mut registry);
    let mut ctx = MockNativeContext::new();
    let sm_obj =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let granted = ctx.create_string("/tmp/w20e.txt");
    let other = ctx.create_string("/tmp/w20e-other.txt");
    let sm = Value::Object(Some(sm_obj));
    let file_check = |ctx: &mut MockNativeContext, method: &str, file: ObjectRef| {
        call_native(
            &registry,
            ctx,
            "java/lang/SecurityManager",
            method,
            "(Ljava/lang/String;)V",
            &[sm, Value::Object(Some(file))],
        )
    };
    let base = ctx.native_pin_count_for_test();

    assert!(file_check(&mut ctx, "checkRead", granted).is_ok());
    assert!(file_check(&mut ctx, "checkRead", other).is_err());
    assert!(file_check(&mut ctx, "checkWrite", granted).is_err());
    let exit = |ctx: &mut MockNativeContext, code: i32| {
        call_native(
            &registry,
            ctx,
            "java/lang/SecurityManager",
            "checkExit",
            "(I)V",
            &[sm, Value::Int(code)],
        )
    };
    assert!(exit(&mut ctx, 3).is_ok());
    assert!(exit(&mut ctx, 4).is_err());
    assert_eq!(
        ctx.native_pin_count_for_test(),
        base,
        "every root is released"
    );

    clear_policy_and_stack();
}

/// gc-common w20-e: the shared collection is rooted across the
/// AllPermission's allocation; it still carries it, read-only.
#[test]
fn w20e_permissive_collection_is_complete_and_leaves_no_pin() {
    let mut ctx = MockNativeContext::new();
    let base = ctx.native_pin_count_for_test();
    let perms = build_permissive_collection(&mut ctx).unwrap();
    assert!(matches!(ctx.get_field(perms, 0), Value::Object(Some(_))));
    assert_eq!(ctx.get_field(perms, 1), Value::Int(1));
    assert_eq!(ctx.native_pin_count_for_test(), base);
}
