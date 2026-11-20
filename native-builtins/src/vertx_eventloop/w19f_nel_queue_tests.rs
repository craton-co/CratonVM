//! gc-common w19-f: a `Runnable` queued by a nested `execute` is held by a
//! global root while it waits, runs once when the outer task returns, and
//! leaves no root behind. (The mock never moves an object, so the test
//! checks the rooting and the release, not a relocation.)
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// The inner task's address, and what the hook saw.
    static W19F_INNER: Cell<usize> = const { Cell::new(0) };
    static W19F_RUNS: Cell<usize> = const { Cell::new(0) };
    static W19F_INNER_RUNS: Cell<usize> = const { Cell::new(0) };
    static W19F_ROOTS_WHILE_QUEUED: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn w19f_run_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "run" {
        return None;
    }
    let n = W19F_RUNS.with(|r| {
        let n = r.get();
        r.set(n + 1);
        n
    });
    let inner = W19F_INNER.with(|c| c.get());
    if receiver.as_ptr() as usize == inner {
        W19F_INNER_RUNS.with(|r| r.set(r.get() + 1));
    } else if n == 0 {
        // The outer task submits the inner one: a nested `execute`.
        let inner_ref = unsafe { ObjectRef::from_raw(inner as *mut u8) };
        run_or_enqueue_nel_runnable(ctx, inner_ref, "w19f-nested");
        W19F_ROOTS_WHILE_QUEUED.with(|c| c.set(ctx.global_root_count()));
    }
    Some(Ok(None))
}

#[test]
fn w19f_nested_execute_roots_the_queued_task_and_releases_it_after_the_run() {
    W19F_RUNS.with(|r| r.set(0));
    W19F_INNER_RUNS.with(|r| r.set(0));
    W19F_ROOTS_WHILE_QUEUED.with(|c| c.set(usize::MAX));
    NEL_EXEC_DEPTH.with(|d| d.set(0));
    NEL_EXEC_QUEUE.with(|q| q.borrow_mut().clear());

    let mut ctx = mock_ctx();
    let outer = ctx.fresh_object_ref();
    let inner = ctx.fresh_object_ref();
    W19F_INNER.with(|c| c.set(inner.as_ptr() as usize));
    ctx.set_invoke_virtual_hook(w19f_run_hook);
    let roots_before = ctx.global_root_count();

    run_or_enqueue_nel_runnable(&mut ctx, outer, "w19f-outer");

    assert_eq!(
        W19F_ROOTS_WHILE_QUEUED.with(|c| c.get()),
        roots_before + 1,
        "the queued task must be held by a global root while it waits"
    );
    assert_eq!(
        W19F_INNER_RUNS.with(|r| r.get()),
        1,
        "the queued task runs once"
    );
    assert_eq!(W19F_RUNS.with(|r| r.get()), 2, "outer + inner");
    assert_eq!(
        ctx.global_root_count(),
        roots_before,
        "the root is released"
    );
    assert!(NEL_EXEC_QUEUE.with(|q| q.borrow().is_empty()));
    assert_eq!(NEL_EXEC_DEPTH.with(|d| d.get()), 0);
}
