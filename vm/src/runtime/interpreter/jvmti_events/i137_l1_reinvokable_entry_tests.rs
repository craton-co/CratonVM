use super::{reinvokable_entry_is, ReinvokableEntryScope};

#[test]
fn the_mark_names_one_depth_and_nested_scopes_restore_it() {
    assert!(!reinvokable_entry_is(0));
    assert!(!reinvokable_entry_is(3));
    {
        let _outer = ReinvokableEntryScope::enter(3);
        assert!(reinvokable_entry_is(3));
        assert!(!reinvokable_entry_is(4));
        {
            let _inner = ReinvokableEntryScope::enter(7);
            assert!(reinvokable_entry_is(7));
            assert!(!reinvokable_entry_is(3));
        }
        assert!(reinvokable_entry_is(3));
    }
    assert!(!reinvokable_entry_is(3));
}

#[test]
fn the_lambda_door_runs_its_impl_through_the_reinvoking_entry() {
    let src = include_str!("../lambda.rs");
    assert!(src.contains("let out = install_and_run_cached_frame_reinvokable("));
}

/// Interpreter round i7 wave 138, lane L1 (`CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP`):
/// the JIT helpers' unsynchronized callee goes through the compiled
/// caller's re-invoking entry, whose loop checks the pop before taking it.
#[test]
fn the_jit_helpers_callee_runs_through_the_compiled_caller_entry() {
    let src = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let monitored = src
        .find("pub(crate) fn install_and_run_cached_frame_monitored(")
        .map(|at| &src[at..]);
    assert!(monitored.is_some_and(|body| {
        let end = body[1..].find("\npub(crate) fn ").map_or(body.len(), |e| e + 1);
        body[..end].contains("return Some(install_and_run_cached_frame_from_compiled(")
    }));
    let reinvoking = src
        .find("fn install_and_run_reinvoking(")
        .map(|at| &src[at..])
        .unwrap_or("");
    let check = reinvoking.find("keep_compiled_caller_entry_pop(");
    let take = reinvoking.find("take_reinvokable_entry_pop(");
    assert!(check.is_some() && take.is_some() && check < take);
}
