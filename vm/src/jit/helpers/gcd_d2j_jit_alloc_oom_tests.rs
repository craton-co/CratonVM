//! gcd d2/j: `jit_alloc_oom` with no JIT thread is counted per VM.
use super::*;
use crate::config::VmConfig;

#[test]
fn a_jit_oom_without_a_jit_thread_is_counted_per_vm() {
    // SAFETY: only asks whether this test thread installed a JIT thread
    // (it did not); the guard, if any, drops at once.
    if unsafe { jit_thread_mut() }.is_some() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let count = || {
        shared
            .mem
            .alloc_ladder
            .jit_alloc_oom_without_thread
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    assert_eq!(count(), 0);
    assert_eq!(
        jit_alloc_oom(&shared, "Java heap space (new_object class_id 1 fields 2)"),
        0
    );
    assert_eq!(jit_alloc_oom(&shared, "Java heap space"), 0);
    assert_eq!(count(), 2);
    let other = SharedVm::new(VmConfig::default());
    assert_eq!(
        other
            .mem
            .alloc_ladder
            .jit_alloc_oom_without_thread
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}
