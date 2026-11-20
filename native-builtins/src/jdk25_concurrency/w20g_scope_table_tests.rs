use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

/// Drops the test's VM rows even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_jdk25_tables(vm);
        }
    }
}

fn mock_on(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn open_scope(ctx: &mut MockNativeContext) -> ObjectRef {
    let scope = try_alloc_concurrent_synthetic(ctx, CLS_TASK_SCOPE, STS_NUM_FIELDS).unwrap();
    sts_init_fields(ctx, scope, Value::Object(None), STS_POLICY_BASE);
    scope
}

fn object(ctx: &mut MockNativeContext, class: &str, fields: usize) -> ObjectRef {
    try_alloc_concurrent_synthetic(ctx, class, fields).unwrap()
}

fn rows_of(vm: usize) -> usize {
    STS_TABLE
        .peek(vm, |t| t.values().map(Vec::len).sum::<usize>())
        .unwrap_or(0)
}

/// A filed fork roots the scope, the subtask and the worker; `join` takes
/// it out, reads both through their roots, and releases every root, and
/// the emptied row is dropped.
#[test]
fn w20g_sts_a_fork_is_rooted_and_join_releases_it() {
    const VM: usize = 0x20C0_0A01;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let scope = open_scope(&mut ctx);
    let subtask = object(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS);
    ctx.set_field(
        subtask,
        SUBTASK_FIELD_STATE,
        Value::Int(SUBTASK_STATE_SUCCESS),
    );
    let worker = object(&mut ctx, "java/lang/Thread", THREAD_SYNTHETIC_NUM_FIELDS);
    let baseline = ctx.global_root_count();

    let fork = register_scope_fork(&mut ctx, scope, subtask, worker);
    assert_eq!(
        ctx.global_root_count(),
        baseline + 3,
        "the scope, the subtask and the worker are global roots"
    );
    assert_eq!(ctx.resolve_global_root(fork.subtask_root), Some(subtask));
    assert_eq!(ctx.resolve_global_root(fork.worker_root), Some(worker));
    assert_eq!(peek_scope_forks(&mut ctx, scope), vec![fork]);

    ctx.set_field(scope, STS_FIELD_TASK_COUNT, Value::Int(1));
    native_sts_join(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
    assert_eq!(
        sts_get_int(&mut ctx, scope, STS_FIELD_COMPLETED_COUNT),
        1,
        "join aggregated the subtask it read through its root"
    );
    assert_eq!(ctx.global_root_count(), baseline, "every root is released");
    assert_eq!(rows_of(VM), 0, "the emptied row is dropped");
}

/// Another VM never sees a row, and tearing one VM down leaves the
/// other's rows alone.
#[test]
fn w20g_sts_rows_are_per_vm() {
    const VM_A: usize = 0x20C0_0A02;
    const VM_B: usize = 0x20C0_0B02;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = mock_on(VM_A);
    let mut b = mock_on(VM_B);
    let scope_a = open_scope(&mut a);
    let scope_b = open_scope(&mut b);
    register_scope_owner(&mut a, scope_a);
    register_scope_owner(&mut b, scope_b);
    assert_eq!(rows_of(VM_A), 1);
    assert_eq!(rows_of(VM_B), 1);

    assert!(
        sts_find_row(&mut b, scope_a).is_none(),
        "VM B must not find VM A's scope row"
    );
    forget_vm_jdk25_tables(VM_B);
    assert_eq!(rows_of(VM_B), 0);
    assert_eq!(rows_of(VM_A), 1, "VM A's row survives VM B's teardown");
    assert!(sts_find_row(&mut a, scope_a).is_some());
}

/// The owner and the joiner share the scope's one row; `close` drops it
/// and releases the scope and joiner roots.
#[test]
fn w20g_sts_close_drops_the_owner_and_joiner_row() {
    const VM: usize = 0x20C0_0A03;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let joiner = match native_joiner_await_all(&mut ctx, &[]).unwrap() {
        Some(Value::Object(Some(j))) => j,
        other => panic!("expected a joiner, got {other:?}"),
    };
    let baseline = ctx.global_root_count();
    let scope = match native_sts_open_joiner_tracked(&mut ctx, &[Value::Object(Some(joiner))])
        .unwrap()
    {
        Some(Value::Object(Some(s))) => s,
        other => panic!("expected a scope, got {other:?}"),
    };
    assert_eq!(rows_of(VM), 1, "owner and joiner share one row");
    assert_eq!(
        ctx.global_root_count(),
        baseline + 2,
        "scope + joiner roots"
    );
    assert_eq!(get_scope_joiner(&mut ctx, scope), Some(joiner));
    assert!(check_scope_owner(&mut ctx, scope));

    native_sts_close_joiner(&mut ctx, &[Value::Object(Some(scope))]).unwrap();
    assert_eq!(rows_of(VM), 0);
    assert_eq!(ctx.global_root_count(), baseline, "both roots released");
    assert_eq!(get_scope_joiner(&mut ctx, scope), None);
}

/// `close()` of a scope whose forks were never joined closes it, reaps
/// and releases the forks, and only then throws, as the JDK does; a second
/// `close()` is then a no-op.
#[test]
fn w20g_sts_close_without_join_closes_then_throws() {
    const VM: usize = 0x20C0_0A04;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let scope = open_scope(&mut ctx);
    let subtask = object(&mut ctx, CLS_SUBTASK, SUBTASK_NUM_FIELDS);
    let worker = object(&mut ctx, "java/lang/Thread", THREAD_SYNTHETIC_NUM_FIELDS);
    let baseline = ctx.global_root_count();
    register_scope_fork(&mut ctx, scope, subtask, worker);
    ctx.set_field(scope, STS_FIELD_TASK_COUNT, Value::Int(1));

    let closed = native_sts_close(&mut ctx, &[Value::Object(Some(scope))]);
    assert!(closed.is_err(), "not joined: IllegalStateException");
    assert_eq!(
        sts_get_int(&mut ctx, scope, STS_FIELD_STATE),
        STS_STATE_CLOSED,
        "the scope is closed before the exception"
    );
    assert_eq!(rows_of(VM), 0);
    assert_eq!(ctx.global_root_count(), baseline);
    assert!(native_sts_close(&mut ctx, &[Value::Object(Some(scope))]).is_ok());
}

/// The inline fallback of `fork()` (no real threads in the mock) files
/// the fork and withdraws it again, leaving nothing rooted.
#[test]
fn w20g_sts_inline_fork_leaves_nothing_rooted() {
    const VM: usize = 0x20C0_0A05;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let scope = open_scope(&mut ctx);
    let baseline = ctx.global_root_count();
    native_sts_fork(&mut ctx, &[Value::Object(Some(scope)), Value::Object(None)]).unwrap();
    assert_eq!(rows_of(VM), 0);
    assert_eq!(ctx.global_root_count(), baseline);
}
