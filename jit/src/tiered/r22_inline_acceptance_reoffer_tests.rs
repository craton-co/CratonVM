use super::*;


fn at_c2(mgr: &TieredCompilationManager, key: &MethodKey, invocations: u64) {
    let mut methods = mgr.core.methods.lock();
    let state = methods
        .entry(key.clone())
        .or_insert_with(|| MethodState::new(key.clone()));
    state.current_tier = CompilationTier::C2;
    state.invocation_count = invocations;    }