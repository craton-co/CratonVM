//! gce e2/o: the latched overhead exit's bounded grace.
use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

/// gcf f2/o: every JIT door that takes the grace also counts its success
/// (`gcf-f1o-latched-grace-ok-is-counted-by-one-door-of-five`): the four
/// taking sites of `jit_newarray_body`, `jit_new_object_body` and
/// `jit_anewarray_object_body` (two) bind `graced`, and the four success
/// exits after them (newarray's tail, new_object's TLAB arm and tail,
/// anewarray's tail) report it.
#[test]
fn gcf_f2o_every_jit_grace_site_counts_its_success() {
    let src = crate::jit::helpers::HELPERS_SRC;
    // gcg g3o residual: the four sites take it through `jit_latched_grace`
    // (which stands down on G1, whose exit decides its own grace), the
    // one place that calls `latched_overhead_grace`.
    assert_eq!(
        src.matches("gc_and_alloc::latched_overhead_grace(vm)")
            .count(),
        1,
        "only `jit_latched_grace` takes the grace"
    );
    let takes = src.matches("jit_latched_grace(vm)").count();
    let reports = src
        .matches("gc_and_alloc::note_latched_grace_outcome(vm, true)")
        .count();
    // JIT round 22 wave 12 (`CRATONVM_JIT_ALLOC_LATCH_AFTER_FAILURE`):
    // `jit_new_object_body`'s failure arm takes it too, counted by the
    // same tail.
    assert_eq!(
        takes, 5,
        "a grace site was added or removed: count its outcome"
    );
    assert_eq!(reports, 4, "every success exit after a grace reports it");
    let in_a_conjunction =
        "&& !crate::runtime::interpreter::gc_and_alloc::latched_overhead_grace";
    assert!(
        !src.contains(in_a_conjunction),
        "a grace taken inside a conjunction cannot be reported"
    );
}

/// gcf f2/o: the native door's verdict: a heap with room retries
/// unconditionally and spends no grace.
#[test]
fn gcf_f2o_an_unlatched_native_verdict_is_not_graced() {
    let shared = SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        ..crate::config::VmConfig::default()
    });
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    let mut thread = JvmThread::new(ThreadId(0), "f2o");
    let v = native_reclaim_before_alloc_retry_graced(&shared, &mut thread);
    assert!(v.retry);
    assert!(!v.graced);
}

/// Eight graced attempts per episode, then none; a reset refills it.
#[test]
fn the_grace_is_bounded_per_latched_episode() {
    let used = AtomicU32::new(0);
    for _ in 0..LATCHED_GRACE_ATTEMPTS {
        assert!(latched_grace_take(&used));
    }
    assert!(!latched_grace_take(&used), "the budget is spent");
    assert!(!latched_grace_take(&used));
    assert_eq!(used.load(Ordering::Relaxed), LATCHED_GRACE_ATTEMPTS);
    used.store(0, Ordering::Relaxed);
    assert!(latched_grace_take(&used), "a reset streak refills it");
}

