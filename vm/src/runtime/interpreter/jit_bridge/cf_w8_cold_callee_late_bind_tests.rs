use super::*;

fn caller(method: &str) -> LateBindColdCaller {
    (7, Arc::from(method), Arc::from("(I)V"))
}

/// One recompile per caller, whatever number of cold callees it waited on;
/// a may-lift waiter (never registered cold) always passes.
#[test]
fn a_caller_is_recompiled_once_on_cold_callees() {
    let mut cold = LateBindColdState::default();
    let tf = caller("tryFire");
    assert!(late_bind_cold_may_register(&cold, &tf));
    cold.registered.insert((tf.clone(), "A.claim()Z".to_string()));
    cold.registered.insert((tf.clone(), "A.other()V".to_string()));
    // A key it never waited on cold: a may-lift fire, admitted, not spent.
    assert!(late_bind_cold_fire_decide(&mut cold, tf.clone(), "B.init()V"));
    assert!(cold.spent.is_empty());
    // The first cold fire admits and spends the caller.
    assert!(late_bind_cold_fire_decide(&mut cold, tf.clone(), "A.claim()Z"));
    assert!(cold.spent.contains(&tf));
    // The second cold callee of the same caller is skipped.
    assert!(!late_bind_cold_fire_decide(&mut cold, tf.clone(), "A.other()V"));
    // And the spent caller registers no more cold waiters.
    assert!(!late_bind_cold_may_register(&cold, &tf));
    // Another caller is unaffected.
    assert!(late_bind_cold_may_register(&cold, &caller("postFire")));
}


/// With nothing registered cold the filter is a no-op.
#[test]
fn an_empty_cold_table_admits_every_fire() {
    let mut cold = LateBindColdState::default();
    assert!(late_bind_cold_fire_decide(&mut cold, caller("m"), "X.y()V"));
    assert!(cold.spent.is_empty());
}

/// The registration caps bound the table.
#[test]
fn the_cold_table_is_capped() {
    let mut cold = LateBindColdState::default();
    for i in 0..LATE_BIND_CALLEE_CAP {
        cold.registered.insert((caller("m"), format!("K{i}")));
    }
    assert!(!late_bind_cold_may_register(&cold, &caller("fresh")));
}
