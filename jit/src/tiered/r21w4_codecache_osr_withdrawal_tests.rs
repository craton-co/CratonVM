use super::*;
use crate::JitBodyWithdrawalSink as _;

fn osr_withdrawn(mgr: &TieredCompilationManager, key: &MethodKey, rounds: u32) -> bool {
    mgr.on_method_invocation(key);
    let sink = mgr.withdrawal_sink().upgrade().expect("the manager is alive");
    let wkey: crate::WithdrawnMethodKey = (
        Arc::clone(&key.class_name),
        Arc::clone(&key.method_name),
        Arc::clone(&key.descriptor),
        key.class_id,
    );
    for _ in 0..rounds {
        sink.optimizing_bodies_invalidated(std::slice::from_ref(&wkey));
    }
    let methods = mgr.core.methods.lock();
    let state = &methods[key];
    assert_eq!(state.current_tier, CompilationTier::Interpreter, "no tier change");
    state.c2_bailout
}

#[test]
fn an_osr_body_withdrawn_past_the_cutoff_bails_out_of_c2() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("r21w4/OsrStorm", "loop", "()V");
    assert!(!osr_withdrawn(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF - 1));
    assert!(osr_withdrawn(&mgr, &key, 1));
}

