use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::VmError;
use cratonvm_types::ArrayElementType;

/// Build a `String[]`; `None` becomes a genuine null element.
fn string_array(ctx: &mut dyn NativeContext, items: &[Option<&str>]) -> Value {
    let arr = ctx.new_array(ArrayElementType::Reference, items.len());
    for (i, item) in items.iter().enumerate() {
        let v = match item {
            Some(s) => {
                let obj = ctx.create_string(s);
                Value::Object(Some(obj))
            }
            None => Value::Object(None),
        };
        ctx.set_array_element(arr, i, v);
    }
    Value::Object(Some(arr))
}

fn assert_npe(err: &MethodCallFailed) {
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. },
        )) => {}
        other => panic!("expected NullPointerException, got {other:?}"),
    }
}

#[test]
fn a_null_command_array_is_a_null_pointer_exception() {
    let mut ctx = mock_ctx();
    let err = checked_cmdarray(&mut ctx, &Value::Object(None))
        .expect_err("a null cmdarray must throw");
    assert_npe(&err);
}

/// The old reader DROPPED a null element, so `exec(["/bin/echo", null])`
/// silently ran `/bin/echo` with no arguments -- a different command line
/// than the caller wrote. HotSpot throws.
#[test]
fn a_null_element_is_a_null_pointer_exception_not_a_shorter_command() {
    let mut ctx = mock_ctx();
    let arr = string_array(&mut ctx, &[Some("/bin/echo"), None]);
    let err = checked_cmdarray(&mut ctx, &arr).expect_err("a null element must throw");
    assert_npe(&err);
}

#[test]
fn an_empty_command_array_is_an_array_index_out_of_bounds() {
    let mut ctx = mock_ctx();
    let arr = string_array(&mut ctx, &[]);
    let err = checked_cmdarray(&mut ctx, &arr).expect_err("an empty cmdarray must throw");
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ArrayIndexOutOfBoundsException { index, .. },
        )) => assert_eq!(index, 0, "the failing read is cmdarray[0]"),
        other => panic!("expected ArrayIndexOutOfBoundsException, got {other:?}"),
    }
}

/// `Runtime.exec` must return with the child STILL RUNNING.
///
/// This is the regression test for the defect itself. `exec` used to call
/// `Command::output()`, which runs the child to completion and buffers its
/// output -- so `exec` blocked for the child's whole lifetime, `isAlive()`
/// was never true, `exitValue()` never threw `IllegalThreadStateException`
/// (the signal Tomcat's `CGIServlet` polls on), and a child reading stdin
/// could never be fed, because the caller only gets the pipe once `exec`
/// has returned. Against the old code this test spends five seconds inside
/// `exec` and then fails.
///
/// The bound is deliberately loose (2.5s against a 5s child): the claim is
/// "returned before the child finished", not a latency budget, and this
/// runs on a shared, heavily loaded host.
///
/// The child is left to exit on its own. Reaping it would mean reaching
/// into `native-io`'s private Process layout for the handle, and a stray
/// `sleep 5` costs nothing.
#[test]
#[cfg(not(target_os = "windows"))]
fn runtime_exec_returns_while_the_child_is_still_running() {
    // Serialize against the checkexec tests: the spawn policy hook is
    // process-global once installed, so a deny-all SecurityManager
    // installed by one of those tests would refuse this spawn.
    let _guard = crate::security_manager::security_state_test_lock();
    let mut ctx = mock_ctx();
    let arr = string_array(&mut ctx, &[Some("/bin/sleep"), Some("5")]);
    let started = std::time::Instant::now();
    let result = native_runtime_exec_array(&mut ctx, &[Value::Object(None), arr]);
    let elapsed = started.elapsed();
    assert!(
        result.is_ok(),
        "spawning /bin/sleep must succeed, got {result:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(2500),
        "Runtime.exec must not wait for the child: took {elapsed:?}"
    );
}

#[test]
fn a_well_formed_command_array_reads_back_in_order() {
    let mut ctx = mock_ctx();
    let arr = string_array(&mut ctx, &[Some("/bin/echo"), Some("a b"), Some("c")]);
    let cmd = checked_cmdarray(&mut ctx, &arr).expect("a valid cmdarray must not throw");
    assert_eq!(cmd, vec!["/bin/echo", "a b", "c"]);
}
