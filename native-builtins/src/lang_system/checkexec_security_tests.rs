use super::*;
use crate::security_manager::{security_state_test_lock, set_security_manager_for_test};
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};

/// Helper: assert the failure is a SecurityException (regardless of
/// the exact message вЂ” the wrapping is `MethodCallFailed::InternalError(
/// VmError::Runtime(RuntimeError::SecurityException { .. }))`).
fn assert_security_exception(err: &MethodCallFailed) {
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::SecurityException { .. },
        )) => {}
        other => panic!("expected RuntimeError::SecurityException, got {other:?}",),
    }
}

// -----------------------------------------------------------------------
// (1) No SecurityManager вЂ” spawn gate is a no-op.
// -----------------------------------------------------------------------

#[test]
fn no_security_manager_allows_check_exec() {
    let _guard = security_state_test_lock();
    // Ensure no SM is installed (defensive вЂ” other tests may have set one).
    // The slot is per-VM now, so the reset needs the ctx whose identity
    // owns it. `mock_ctx()` reports the default identity 0.
    let mut ctx = mock_ctx();
    let prev = set_security_manager_for_test(&ctx, None);

    let result = check_exec_or_throw(&mut ctx, "/usr/bin/ls");
    assert!(
        result.is_ok(),
        "check_exec_or_throw must be a no-op with no SecurityManager, got {result:?}",
    );

    let _ = set_security_manager_for_test(&ctx, prev);
}

// -----------------------------------------------------------------------
// (2) SecurityManager that denies every checkExec вЂ” SecurityException
//     propagates AND the spawn does NOT happen.
// -----------------------------------------------------------------------

#[test]
fn denying_security_manager_blocks_check_exec() {
    let _guard = security_state_test_lock();
    let mut ctx = mock_ctx();
    let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let prev = set_security_manager_for_test(&ctx, Some(sm));

    // Pre-arm the mock so the next invoke_virtual returns a SecurityException.
    // This simulates a SecurityManager whose checkExec(String) denies.
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
            VmError::Runtime(RuntimeError::SecurityException {
                message: "access denied (test policy denies all exec)".to_string(),
            }),
        )));
    }

    let result = check_exec_or_throw(&mut ctx, "/usr/bin/evil");
    let err = result.expect_err("denying SM must surface SecurityException");
    assert_security_exception(&err);

    let _ = set_security_manager_for_test(&ctx, prev);
}

#[test]
fn denying_sm_blocks_runtime_exec_before_spawn() {
    let _guard = security_state_test_lock();
    // End-to-end check: a deny-all SM must short-circuit
    // native_runtime_exec_string with SecurityException вЂ” std::process::Command
    // is never invoked. Using a bogus program path proves no fallback
    // "Runtime.exec failed: ..." IOException can leak through, because
    // if the SM check is skipped the spawn would attempt the path and
    // surface IOException, not SecurityException.
    //
    // nio21 wave 5 L1: since `ac50933ed5` (2026-08-06) the check runs in
    // `spawn_and_wrap`, through the hook `install_spawn_policy_hook`
    // installs -- and nothing in a unit-test binary installs it but the
    // tests that call it. This test did not, so it passed only when
    // `denying_sm_blocks_processbuilder_start` happened to run first in
    // the same process, and failed alone (`cargo test ...
    // denying_sm_blocks_runtime_exec_before_spawn`) with the spawn's
    // `IOException`. Install it here, as that test does (idempotent).
    install_spawn_policy_hook();
    let mut ctx = mock_ctx();
    let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let prev = set_security_manager_for_test(&ctx, Some(sm));
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
            VmError::Runtime(RuntimeError::SecurityException {
                message: "deny".to_string(),
            }),
        )));
    }

    // args[0] = Runtime instance (irrelevant here), args[1] = command.
    let runtime_instance =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runtime", 0).unwrap();
    let cmd = ctx.create_string("/path/to/definitely-nonexistent-binary-xyz");
    let result = native_runtime_exec_string(
        &mut ctx,
        &[
            Value::Object(Some(runtime_instance)),
            Value::Object(Some(cmd)),
        ],
    );

    let err = result.expect_err("deny-all SM must block Runtime.exec spawn");
    assert_security_exception(&err);

    let _ = set_security_manager_for_test(&ctx, prev);
}

#[test]
fn denying_sm_blocks_processbuilder_start() {
    let _guard = security_state_test_lock();
    // Drives the REAL `ProcessBuilder.start` -- `native-io`'s, the one this
    // crate now registers. It used to drive `native_pb_start`, a stub that
    // never spawned, so what it proved was that a stub asked permission
    // before doing nothing.
    //
    // `install_spawn_policy_hook()` is the point: the gate reaches
    // `native-io` only through that hook, and nothing else in a unit-test
    // binary installs it. If the wiring regresses, the spawn goes through
    // and this test fails.
    install_spawn_policy_hook();
    let mut ctx = mock_ctx();
    let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let prev = set_security_manager_for_test(&ctx, Some(sm));
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
            VmError::Runtime(RuntimeError::SecurityException {
                message: "deny".to_string(),
            }),
        )));
    }

    // Build a ProcessBuilder synthetic with a 4-slot layout and a
    // command list. `start` reads slot 0; we plant a String[] there with
    // command[0] = "/bin/anything", a path that does not exist -- so if the
    // gate ever fails to refuse, the spawn fails with an IOException rather
    // than running something, and the assertion below still catches it.
    let pb = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ProcessBuilder", 4).unwrap();
    let cmd_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    let prog = ctx.create_string("/bin/anything");
    ctx.set_array_element(cmd_arr, 0, Value::Object(Some(prog)));
    ctx.set_field(pb, 0, Value::Object(Some(cmd_arr)));

    let result = cratonvm_native_io::process::native_process_builder_start(
        &mut ctx,
        &[Value::Object(Some(pb))],
    );
    let err = result.expect_err("deny-all SM must block ProcessBuilder.start");
    assert_security_exception(&err);

    let _ = set_security_manager_for_test(&ctx, prev);
}

// -----------------------------------------------------------------------
// (3) Allow-specific-path SecurityManager вЂ” only listed paths spawn.
// -----------------------------------------------------------------------
//
// The MockNativeContext's `invoke_virtual_result` is consumed by
// `take()` per call, so for two back-to-back checkExec invocations we
// pre-arm the mock once with Err (deny) for the first call, then leave
// it unset so the second call falls through to the default `Ok(None)`
// (allow). This mirrors a real SM that allows the second program but
// denies the first.

#[test]
fn allow_listed_sm_lets_specific_paths_through() {
    let _guard = security_state_test_lock();
    let mut ctx = mock_ctx();
    let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
    let prev = set_security_manager_for_test(&ctx, Some(sm));

    // First call: simulate a denial for the disallowed binary.
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
            VmError::Runtime(RuntimeError::SecurityException {
                message: "deny /usr/bin/danger".to_string(),
            }),
        )));
    }
    let denied = check_exec_or_throw(&mut ctx, "/usr/bin/danger");
    let err = denied.expect_err("disallowed path must surface SecurityException");
    assert_security_exception(&err);

    // Second call: invoke_virtual_result was take()n on the previous
    // call, so the mock now falls back to its default Ok(None) вЂ”
    // simulating the allow-list permitting this program.
    let allowed = check_exec_or_throw(&mut ctx, "/bin/allowed-program");
    assert!(
        allowed.is_ok(),
        "allow-listed path must pass the gate, got {allowed:?}",
    );

    let _ = set_security_manager_for_test(&ctx, prev);
}
