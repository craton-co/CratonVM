use super::*;
use crate::config::VmConfig;

/// A body that recorded the thin-bind dependency is withdrawn by the
/// first-mint sweep; a body that did not is kept, and the dependency is
/// recorded once however many sites bind.
#[test]
pub fn the_first_mint_sweep_withdraws_only_thin_bound_bodies() {
    if !cratonvm_jit::varhandle_thin_bind_retire_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    let shared = SharedVm::new(VmConfig::default());
    let cache = &shared.jit.jit_cache;
    let cid = ClassId::new(0);
    let Some(bound_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
        return;
    };
    let Some(plain_buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
        return;
    };
    let mut bound = cratonvm_jit::CompiledMethod::new(bound_buf);
    cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
    cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
    assert_eq!(bound.inlined_methods.len(), 1, "recorded once");
    cache.put("T".into(), "bound".into(), "()V".into(), cid, bound);
    let plain = cratonvm_jit::CompiledMethod::new(plain_buf);
    cache.put("T".into(), "plain".into(), "()V".into(), cid, plain);
    assert_eq!(retire_varhandle_thin_bound_bodies(&shared), 1);
    assert!(cache.get("T", "bound", "()V", cid).is_none());
    assert!(cache.get("T", "plain", "()V", cid).is_some());
    // Idempotent: nothing left to withdraw.
    assert_eq!(retire_varhandle_thin_bound_bodies(&shared), 0);
}

/// Loading a `VarHandle` subclass (the class-define listener invalidates
/// by every supertype) does not withdraw a thin-bound body: the dependency
/// names no real class.
#[test]
pub fn a_varhandle_subclass_define_keeps_thin_bound_bodies() {
    if !cratonvm_jit::varhandle_thin_bind_retire_enabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let cache = &shared.jit.jit_cache;
    let cid = ClassId::new(0);
    let Some(buf) = cratonvm_jit::ExecutableBuffer::new(64) else {
        return;
    };
    let mut bound = cratonvm_jit::CompiledMethod::new(buf);
    cratonvm_jit::add_varhandle_thin_bind_dependency(&mut bound.inlined_methods);
    cache.put("T".into(), "bound".into(), "()V".into(), cid, bound);
    assert_eq!(
        cache.invalidate_for_class_change("java/lang/invoke/VarHandle"),
        0
    );
    assert!(cache.get("T", "bound", "()V", cid).is_some());
}
