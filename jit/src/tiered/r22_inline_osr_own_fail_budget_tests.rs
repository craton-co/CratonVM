use super::*;

fn offer(
    mgr: &TieredCompilationManager,
    key: &MethodKey,
    tier_fails: u32,
    osr_fails: u32,
) -> OsrRequest {
    {
        let mut methods = mgr.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.tier_fail_count = tier_fails;
        state.osr_fail_count = osr_fails;
    }
    mgr.request_osr_classified(key, 7)
}

#[test]
fn by_default_a_spent_method_entry_budget_refuses_the_osr_request() {
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("r22/OsrBudget", "coupled", "()V");
    assert_eq!(offer(&mgr, &key, MAX_TIER_FAIL_RETRIES, 0), OsrRequest::Refused);    }