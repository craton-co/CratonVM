use super::*;
use crate::config::VmConfig;

/// VH3-1: a thin-bound artifact is stale at publication exactly when the
/// VM no longer admits thin binds; an artifact without the dependency
/// never is.
#[test]
pub fn a_thin_bound_artifact_is_refused_once_binds_are_not_admitted() {
    if !cratonvm_jit::varhandle_thin_bind_retire_enabled()
        || !cratonvm_native_builtins::lang_invoke::vh_thin_bind_exact_gate_enabled()
    {
        return; // an explicit `0` in the test environment wins
    }
    let shared = SharedVm::new(VmConfig::default());
    let Some(bound_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
        return;
    };
    let Some(plain_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
        return;
    };
    let mut bound = cratonvm_jit::CompiledMethod::new(bound_buf);
    cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
    let plain = cratonvm_jit::CompiledMethod::new(plain_buf);
    // Gate closed: binds are admitted, nothing is stale.
    assert!(!varhandle_thin_bound_artifact_is_stale(&shared, &bound));
    assert!(!varhandle_thin_bound_artifact_is_stale(&shared, &plain));
    shared
        .natives
        .var_handle_exact_slot
        .store(1, std::sync::atomic::Ordering::Relaxed);
    assert!(varhandle_thin_bound_artifact_is_stale(&shared, &bound));
    assert!(!varhandle_thin_bound_artifact_is_stale(&shared, &plain));
}
