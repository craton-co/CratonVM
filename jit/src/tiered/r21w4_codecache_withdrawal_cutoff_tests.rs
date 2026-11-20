use super::*;
use crate::JitBodyWithdrawalSink as _;

fn withdrawn_key(key: &MethodKey) -> crate::WithdrawnMethodKey {
    (
        Arc::clone(&key.class_name),
        Arc::clone(&key.method_name),
        Arc::clone(&key.descriptor),
        key.class_id,
    )
}

/// Publish a C2 body for `key` and have it invalidated (the sink's charge,
/// then the withdrawal's demotion), `rounds` times; answer whether the
/// method is bailed out of C2 afterwards.
fn invalidated_rounds(mgr: &TieredCompilationManager, key: &MethodKey, rounds: u32) -> bool {
    mgr.on_method_invocation(key);
    let sink = mgr.withdrawal_sink().upgrade().expect("the manager is alive");
    let wkey = withdrawn_key(key);
    for _ in 0..rounds {
        mgr.compilation_complete(key, CompilationTier::C2, 10);
        sink.optimizing_bodies_invalidated(std::slice::from_ref(&wkey));
        sink.bodies_withdrawn(std::slice::from_ref(&wkey));
    }
    mgr.core.methods.lock()[key].c2_bailout
}

#[test]
fn a_c2_body_invalidated_past_the_cutoff_stays_c1() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("r21w4/Storm", "hot", "(I)I");
    assert!(
        !invalidated_rounds(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF - 1),
        "one invalidation below the cutoff still recompiles at C2"
    );
    assert!(
        invalidated_rounds(&mgr, &key, 1),
        "the cutoff-th invalidation bails out of C2"
    );
    assert_eq!(
        mgr.core.methods.lock()[&key].c2_withdrawals,
        WITHDRAWAL_RECOMPILE_CUTOFF
    );
}

/// rev-x64 item 8: a withdrawal that is not a decompilation (a flush, an
/// eviction, an unload sweep, the sweeper's `note_body_withdrawn`) is not
/// charged, however many times it repeats.
#[test]
fn a_plain_withdrawal_is_not_charged() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("r21w4/Storm", "reloaded", "(I)I");
    mgr.on_method_invocation(&key);
    let sink = mgr.withdrawal_sink().upgrade().expect("the manager is alive");
    let wkey = withdrawn_key(&key);
    for _ in 0..WITHDRAWAL_RECOMPILE_CUTOFF + 10 {
        mgr.compilation_complete(&key, CompilationTier::C2, 10);
        sink.bodies_withdrawn(std::slice::from_ref(&wkey));
        mgr.compilation_complete(&key, CompilationTier::C2, 10);
        mgr.note_body_withdrawn(&key);
    }
    let methods = mgr.core.methods.lock();
    assert_eq!(methods[&key].c2_withdrawals, 0);
    assert!(!methods[&key].c2_bailout);
}


/// A redefinition is new bytecode: the count starts again.
#[test]
fn a_redefinition_resets_the_withdrawal_count() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("r21w4/Storm", "redefined", "(I)I");
    assert!(invalidated_rounds(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF));
    mgr.on_class_redefined(ClassId::new(0), "r21w4/Storm");
    let methods = mgr.core.methods.lock();
    assert_eq!(methods[&key].c2_withdrawals, 0);
    assert!(!methods[&key].c2_bailout);
}
