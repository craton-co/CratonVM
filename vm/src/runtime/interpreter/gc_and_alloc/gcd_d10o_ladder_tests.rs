//! gcd d10/o: G1's full cycle before an overhead-limit error, the native
//! door's shared ladder, and the delivery thread's notification name.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;

const CAP: usize = 128 * 1024 * 1024;

/// The verdict is the overhead limit's own yardstick: 2 % of the heap
/// freed, measured from the caller's `before_live`; a live set that grew
/// meanwhile frees nothing, and a zero capacity is never productive.
#[test]
fn the_g1_cycle_is_productive_at_two_percent() {
    let two_percent = CAP / 50;
    assert!(g1_overhead_cycle_productive(
        90_000_000,
        90_000_000 - two_percent - 8,
        CAP
    ));
    assert!(!g1_overhead_cycle_productive(
        90_000_000,
        90_000_000 - two_percent + 8,
        CAP
    ));
    assert!(!g1_overhead_cycle_productive(90_000_000, 95_000_000, CAP));
    assert!(!g1_overhead_cycle_productive(90_000_000, 0, 0));
}

/// Off G1 the helper runs nothing and keeps the verdict, whatever the
/// streak says: the Generational and ZGC exits are unchanged.
#[test]
fn the_g1_cycle_is_g1_only() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "d10o-g1-only");
    let cycles0 = shared
        .mem
        .gc_cycle_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        g1_overhead_limit_full_cycle(&shared, &mut thread, CAP),
        G1OverheadCycle::default()
    );
    assert_eq!(
        shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed),
        cycles0
    );
}

/// gcg g3o residual: off G1 the shared rung runs nothing.
#[test]
fn the_g1_latched_rung_is_g1_only() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "g3o-rung-g1-only");
    let cycles0 = shared
        .mem
        .gc_cycle_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        g1_latched_overhead_rung(&shared, &mut thread, CAP),
        G1OverheadCycle::default()
    );
    assert_eq!(
        shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed),
        cycles0
    );
}

/// gcg g3o residual: the exit's decision. A judged cycle that freed
/// nothing attempts only on the grace, and takes it only then; a
/// productive, sliver-freeing or unjudged one always attempts.
#[test]
fn the_g1_latched_exit_attempts_after_a_productive_cycle_or_on_the_grace() {
    let taken = std::cell::Cell::new(0);
    let grace = |answer: bool| {
        taken.set(taken.get() + 1);
        answer
    };
    let cycle = |productive, cycle_freed| G1OverheadCycle {
        productive,
        cycle_freed,
    };
    assert!(g1_latched_exit_attempts(true, cycle(true, 1 << 20), || {
        grace(false)
    }));
    assert!(g1_latched_exit_attempts(
        true,
        cycle(false, 4096),
        || grace(false)
    ));
    assert!(g1_latched_exit_attempts(false, cycle(false, 0), || grace(
        false
    )));
    assert_eq!(taken.get(), 0, "no grace taken when the cycle decides");
    assert!(g1_latched_exit_attempts(true, cycle(false, 0), || grace(
        true
    )));
    assert!(!g1_latched_exit_attempts(true, cycle(false, 0), || grace(
        false
    )));
    assert_eq!(taken.get(), 2);
}

/// gcg g3o residual: on G1 with the streak latched, the native door takes
/// the shared exit -- a collection runs -- and gives its caller the
/// retry (productive, or futile on the first of the bounded grace),
/// ungraced in the verdict's sense (no caller-side grace on G1).
#[test]
fn a_latched_g1_native_ladder_runs_the_rung_then_one_ungraced_retry() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "g3o-native-g1");
    shared
        .mem
        .gc_unproductive_streak
        .store(u32::MAX / 2, std::sync::atomic::Ordering::Relaxed);
    if !gc_overhead_limit_exceeded(&shared) {
        // This VM's limit is off (the HotSpot default without the
        // backstop): nothing latches, and the door's latched arm is
        // unreachable.
        return;
    }
    let cycles0 = shared
        .mem
        .gc_cycle_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let v = native_reclaim_before_alloc_retry_graced(&shared, &mut thread);
    assert_eq!(
        v,
        NativeReclaimVerdict {
            retry: true,
            graced: false
        }
    );
    assert!(
        shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed)
            > cycles0,
        "the rung collected"
    );
}

/// gcg g3o residual: every allocation door's G1 latched arm is the shared
/// rung, and none keeps its own G1 cycle call.
#[test]
fn every_door_takes_the_one_g1_latched_rung() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let read = |p: &str| {
        std::fs::read_to_string(root.join(p))
            .expect(p)
            .replace("\r\n", "\n")
    };
    let ga = read("runtime/interpreter/gc_and_alloc/gc_notifications_go_to_delivery.rs");
    // The split `vm/src/jit/helpers/*.rs`, concatenated (origin/dev `00529e979f`).
    let helpers = crate::jit::helpers::read_helpers_src_from_disk().replace("\r\n", "\n");
    let exceptions = read("runtime/exceptions.rs");
    let body = |src: &str, start: &str| -> String {
        let at = src.find(start).expect(start);
        let tail = &src[at..];
        let end = tail.find("\n}\n").expect("fn end");
        tail[..end].to_string()
    };
    let interp = body(&ga, "fn collect_and_retry_with_thread<");
    let native = body(&ga, "fn native_reclaim_ladder_tail(");
    let jit = body(&helpers, "fn jit_latched_overhead_limit_throws(");
    assert!(interp.contains("g1_latched_overhead_exit("));
    assert!(native.contains("g1_latched_overhead_exit("));
    assert!(jit.contains("jit_g1_latched_overhead_exit("));
    assert!(exceptions.contains("gc_and_alloc::g1_latched_overhead_exit("));
    for (door, src) in [("native", &native), ("jit", &jit), ("interp", &interp)] {
        assert!(
            !src.contains("g1_overhead_limit_full_cycle("),
            "{door} keeps its own G1 cycle"
        );
    }
    assert!(!exceptions.contains("g1_overhead_limit_full_cycle("));
}

/// On a heap with room (no overhead streak) the native door's ladder
/// collects and earns its caller the retry, as it always did.
#[test]
fn an_unlatched_native_ladder_earns_the_retry() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "d10o-native-ladder");
    assert!(!gc_overhead_limit_exceeded(&shared));
    let cycles0 = shared
        .mem
        .gc_cycle_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(native_reclaim_before_alloc_retry(&shared, &mut thread));
    assert!(
        shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed)
            > cycles0,
        "the ladder collected"
    );
}

/// Nothing queued: the delivery thread's notification step returns at
/// once and leaves the thread's name alone; with notifications, the name
/// is restored after the delivery (a thread with no Java mirror, as in a
/// unit VM, renames nothing on the heap).
#[test]
fn the_notification_step_restores_the_thread_name() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), FINALIZER_THREAD_NAME);
    run_gc_notifications_as_notification_thread(&shared, &mut thread);
    assert_eq!(thread.name, FINALIZER_THREAD_NAME);
    set_delivery_mirror_name(&shared, &thread);
    assert!(thread.java_thread_obj.is_none());
    assert_eq!(NOTIFICATION_THREAD_NAME, "Notification Thread");
}
