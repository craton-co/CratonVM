use super::{jmx_check_non_heap_thresholds, jmx_peak_queue, NonHeapThresholdCheck, SharedVm};
use crate::config::VmConfig;
use cratonvm_gc::gc_metrics::PoolThresholdKind;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// gci i18/dk (`gci-i17rv-i16-default-path-review-findings-RETIRED-UNFIXED-20261018`
/// finding 8): the split metaspace walk is the figure `heap_summary`
/// reports; at a collection's end, with another thread holding the class
/// manager for writing, the check returns without waiting and leaves the
/// `Metaspace` sample due (nothing recorded); once the writer is gone the
/// next collection-end check samples under its one guard and records the
/// cadence (the walk is no longer due), triggering nothing below a
/// threshold nobody reaches.
#[test]
fn gci_i18dk_collection_end_samples_metaspace_under_its_one_guard() {
    let shared = Arc::new(SharedVm::new(crate::VmConfig::default()));
    {
        use crate::runtime::serviceability::VmDiagnosticState;
        let summary = VmDiagnosticState::heap_summary(&*shared).metaspace_used;
        let walk =
            crate::vm::vm_init::metaspace_used_bytes(&shared.classes.class_manager.read());
        assert_eq!(walk as u64, summary, "one walk, one figure");
    }
    let (queue, _) = jmx_peak_queue(&shared).expect("every backend describes its pools");
    let t = queue.pool_thresholds();
    t.set("Metaspace", PoolThresholdKind::Usage, u64::MAX / 2);
    let unloaded = || {
        shared
            .debug
            .diagnostic_counters
            .classes_unloaded
            .load(Ordering::Relaxed)
    };
    assert!(t.non_heap_walk_due(0, unloaded()), "never sampled: due at once");

    let (held_tx, held_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let writer = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _w = shared.classes.class_manager.write();
            let _ = held_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
        })
    };
    held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the writer holds the class manager");
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let checker = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            jmx_check_non_heap_thresholds(&shared, NonHeapThresholdCheck::CollectionEnd);
            let _ = done_tx.send(());
        })
    };
    let returned_while_held = done_rx.recv_timeout(Duration::from_secs(10)).is_ok();
    let _ = release_tx.send(());
    writer.join().expect("writer");
    checker.join().expect("checker");
    assert!(
        returned_while_held,
        "a collection-end check never waits for a class-manager writer"
    );
    assert!(
        t.non_heap_walk_due(0, unloaded()),
        "a skipped sample records nothing: the walk stays due"
    );

    jmx_check_non_heap_thresholds(&shared, NonHeapThresholdCheck::CollectionEnd);
    assert!(
        !t.non_heap_walk_due(0, unloaded()),
        "sampled under the guard: the next walk waits for growth or an unload"
    );
    assert!(!t.has_pending(), "nothing reaches the threshold");
    t.set("Metaspace", PoolThresholdKind::Usage, 0);
}

/// gci i18/dk (i16/nt's NARROWED code-heap half): the compile path's
/// check (`NonHeapThresholdCheck::CodeAllocated`) samples the code heaps
/// only. With `Metaspace` and a code heap both armed and another thread
/// holding the class manager for writing, it returns without waiting and
/// leaves the `Metaspace` sample due (it never walks the class store);
/// with only `Metaspace` armed it records nothing either.
#[test]
fn gci_i18dk_the_compile_path_check_samples_code_heaps_only() {
    let shared = Arc::new(SharedVm::new(crate::VmConfig::default()));
    let (queue, _) = jmx_peak_queue(&shared).expect("every backend describes its pools");
    let t = queue.pool_thresholds();
    let unloaded = || {
        shared
            .debug
            .diagnostic_counters
            .classes_unloaded
            .load(Ordering::Relaxed)
    };
    t.set("Metaspace", PoolThresholdKind::Usage, u64::MAX / 2);
    jmx_check_non_heap_thresholds(&shared, NonHeapThresholdCheck::CodeAllocated);
    assert!(
        t.non_heap_walk_due(0, unloaded()),
        "only Metaspace armed: the compile path samples nothing"
    );
    t.set(
        "CodeHeap 'non-profiled nmethods'",
        PoolThresholdKind::Usage,
        u64::MAX / 2,
    );

    let (held_tx, held_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let writer = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _w = shared.classes.class_manager.write();
            let _ = held_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
        })
    };
    held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the writer holds the class manager");
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let checker = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            jmx_check_non_heap_thresholds(&shared, NonHeapThresholdCheck::CodeAllocated);
            let _ = done_tx.send(());
        })
    };
    let returned_while_held = done_rx.recv_timeout(Duration::from_secs(10)).is_ok();
    let _ = release_tx.send(());
    writer.join().expect("writer");
    checker.join().expect("checker");
    assert!(returned_while_held, "the compile path never takes the class-manager lock");
    assert!(
        t.non_heap_walk_due(0, unloaded()),
        "the compile path never samples Metaspace"
    );
    assert!(!t.has_pending(), "no code heap reaches u64::MAX / 2");
    t.set("CodeHeap 'non-profiled nmethods'", PoolThresholdKind::Usage, 0);
    t.set("Metaspace", PoolThresholdKind::Usage, 0);
}
