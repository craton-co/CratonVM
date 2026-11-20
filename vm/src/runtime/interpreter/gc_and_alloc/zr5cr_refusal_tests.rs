//! zr z5/cr: the ZGC refusal census, the futile-refusal rule (with the
//! owed clear-soft collection and the native-boundary relief) and the
//! opt-in capacity refill gate.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_gc::heap::ArrayElementType;
use std::sync::atomic::Ordering;

#[cfg(feature = "zgc")]
fn zgc_vm() -> Option<SharedVm> {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    shared.mem.heap.is_zgc().then_some(shared)    }

/// Allocate well over the futile window, so the next refusal is not futile.
fn allocate_a_window(shared: &SharedVm) {
    let _ = shared
        .mem
        .heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 64 * 1024)
        .expect("a fresh heap serves 64 KiB");
    assert!(shared
        .mem
        .heap
        .zgc_bytes_allocated_since_cycle()
        .is_some_and(|w| w >= ZGC_REFUSAL_FUTILE_WINDOW));
}

#[test]
fn the_futile_decision_needs_a_skippable_site_and_a_small_window() {
    let w = ZGC_REFUSAL_FUTILE_WINDOW;
    assert!(refusal_skip_decision(true, 0));
    assert!(refusal_skip_decision(true, w - 1));
    assert!(
        !refusal_skip_decision(true, w),
        "a window of 4 KiB collects"
    );
    assert!(
        !refusal_skip_decision(false, 0),
        "site not followed by the soft rung"
    );
    // The soft rung and the occupancy doors are never skippable.
    assert!(!ZGC_REFUSAL_SKIP_SITES.contains(&ZGC_REFUSAL_SOFT_RUNG_SITE));
    for site in ZGC_OCCUPANCY_FORCED_SITES {
        assert!(!ZGC_REFUSAL_SKIP_SITES.contains(site), "{site}");
    }
    assert!(!ZGC_REFUSAL_SKIP_SITES.contains(&"tlab-refill-wedge"));
}

#[test]
#[cfg(feature = "zgc")]
fn a_refusal_on_an_unchanged_heap_skips_and_the_soft_rung_pays_the_collection() {
    let Some(shared) = zgc_vm() else {
        return; // a build without the `zgc` feature
    };
    let mut thread = JvmThread::new(ThreadId(0), "zr5cr-futile");
    let cycles = || shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    allocate_a_window(&shared);
    let c0 = cycles();
    // A refusal after real allocation collects.
    assert_eq!(
        maybe_gc_forced_collected_stamped(&shared, &mut thread, "alloc-object-shared", None),
        Some(true)
    );
    let c1 = cycles();
    assert!(c1 > c0);
    // Nothing allocated since: the next refusal's forced collection is futile.
    assert_eq!(
        maybe_gc_forced_collected_stamped(&shared, &mut thread, "jit-helpers", None),
        Some(false)
    );
    assert_eq!(cycles(), c1, "a skipped refusal runs no cycle");
    // The ladder's clear-soft rung owes it a collection: paid even though
    // this VM holds no soft reference ...
    assert!(last_ditch_clear_soft_refs(&shared, &mut thread));
    let c2 = cycles();
    assert!(c2 > c1);
    // ... once.
    assert!(!last_ditch_clear_soft_refs(&shared, &mut thread));
    assert_eq!(cycles(), c2);
    // The hard-latch relief on an unchanged heap is futile too.
    assert!(zgc_hard_failure_relief_is_futile(&shared));
    // An occupancy door is never skipped.
    assert!(maybe_gc_forced_collected(
        &shared,
        &mut thread,
        "tlab-alloc-shaped"
    ));
    assert!(cycles() > c2);
    let line = zgc_refusal_line(&shared).expect("a ZGC line");
    assert!(
        line.starts_with(
            "[GC] zgc-refusal: collections=2 skipped_futile=1 owed_paid=1 relief_skipped=1 \
             soft_skipped_room=0 "
        ),
        "{line}"
    );
    // The soft rung's collection followed the first refusal collection.
    assert!(line.contains(" after_refusal=1 streak_max=2 "), "{line}");
    assert!(
        line.ends_with(
            " exhausted_armed=0 exhausted_refused=0 near_full_soft_clears=0 \
             exhausted_spent=0 gate_refused=0 gate_collects=0 gate_collects_dropped=0 \
             gate_collects_joined=0 oom_fresh_marks=0"
        ),
        "{line}"
    );
}


#[test]
#[cfg(feature = "zgc")]
fn a_refusal_after_real_allocation_still_collects() {
    let Some(shared) = zgc_vm() else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(0), "zr5cr-window");
    allocate_a_window(&shared);
    let _ = maybe_gc_forced_collected_stamped(&shared, &mut thread, "gc-alloc-array", None);
    allocate_a_window(&shared);
    let c1 = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    assert_eq!(
        maybe_gc_forced_collected_stamped(&shared, &mut thread, "gc-alloc-array", None),
        Some(true)
    );
    assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > c1);
    allocate_a_window(&shared);
    assert!(!zgc_hard_failure_relief_is_futile(&shared));
}




#[test]
fn a_stamped_decision_is_not_a_refusal_and_other_collectors_are_untouched() {
    #[cfg(feature = "zgc")]
    if let Some(shared) = zgc_vm() {
        assert_eq!(zgc_refusal_window(&shared, "jit-helpers", Some(3)), None);
        assert_eq!(zgc_refusal_window(&shared, "tlab-alloc-shaped", None), None);
        assert_eq!(zgc_refusal_window(&shared, "tlab-refill-wedge", None), None);
        assert!(zgc_refusal_window(&shared, "jit-helpers", None).is_some());
    }
    let gen = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(zgc_refusal_line(&gen), None);
    assert_eq!(zgc_refusal_window(&gen, "jit-helpers", None), None);
    assert!(!zgc_hard_failure_relief_is_futile(&gen));
    let mut thread = JvmThread::new(ThreadId(0), "zr5cr-gen");
    let c = gen.mem.gc_cycle_count.load(Ordering::Relaxed);
    let _ = maybe_gc_forced_collected_stamped(&gen, &mut thread, "jit-helpers", None);
    let _ = maybe_gc_forced_collected_stamped(&gen, &mut thread, "jit-helpers", None);
    assert!(
        gen.mem.gc_cycle_count.load(Ordering::Relaxed) >= c + 2,
        "Generational never skips"
    );
}
