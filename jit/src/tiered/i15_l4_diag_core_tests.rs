use super::{
    CompilationPolicy, CompilerCore, DiagCore, MethodKey, MethodState, TieredCompilationManager,
};
use std::sync::Arc;

/// The slot neither keeps its core alive nor loses what the core counted:
/// the report read from the live core and the one retired as it dropped
/// say the same thing, and the retired one is handed out once.
#[test]
fn the_diagnostic_slot_releases_its_core_and_keeps_its_report() {
    let mgr = TieredCompilationManager::new(CompilationPolicy::default());
    let key = MethodKey::new("i15/l4/Counted", "run", "()V");
    let mut state = MethodState::new(key.clone());
    state.invocation_count = 5;
    mgr.core.methods.lock().insert(key, state);
    let slot = DiagCore::new(&mgr.core, 3);
    assert!(slot.is_for(&mgr.core));
    let other = CompilerCore::with_install_epoch_source(None);
    assert!(!slot.is_for(&other));

    let live = slot.report().expect("the core lives");
    assert_eq!(live.snap.distinct_methods, 1);
    assert_eq!(live.snap.total_invocations, 5);
    assert_eq!(live.hot_but_stuck.len(), 1, "5 invocations cross 3");
    assert!(slot.retired.lock().is_none(), "a live read retires nothing");

    // What `CompilerCore::drop` does for the reported core when the
    // report is armed (it asks `DIAG_CORE`, not this local slot).
    slot.retire(&mgr.core);
    drop(mgr);
    assert!(
        slot.core.upgrade().is_none(),
        "the slot does not keep the dropped VM's core alive"
    );
    let retired = slot.report().expect("the retired report");
    assert_eq!(retired.c1_threshold, 3);
    assert_eq!(retired.snap.distinct_methods, 1);
    assert_eq!(retired.snap.total_invocations, 5);
    assert_eq!(retired.hot_but_stuck.len(), 1);
    assert_eq!(retired.verdict_fields, live.verdict_fields);
    assert!(slot.report().is_none(), "handed out once");
}

/// No manager's core is held strongly by the process-wide slot: whichever
/// manager claimed it, the slot's strong count is only what live managers
/// and workers hold.
#[test]
fn the_process_slot_holds_no_strong_reference() {
    let mgr = TieredCompilationManager::new(CompilationPolicy::default());
    let diag = super::DIAG_CORE.get().expect("a manager was built");
    if diag.is_for(&mgr.core) {
        assert_eq!(Arc::strong_count(&mgr.core), 1);
        drop(mgr);
        assert!(diag.core.upgrade().is_none());
    }
}
