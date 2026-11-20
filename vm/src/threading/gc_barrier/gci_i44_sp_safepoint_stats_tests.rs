use super::*;

/// A fresh barrier has run no safepoint.
#[test]
fn gci_i44_sp_a_fresh_barrier_counts_no_safepoint() {
    let barrier = GcBarrier::new();
    assert_eq!(barrier.safepoint_stats(), SafepointStats::default());
}

/// Each completed pause counts once, its stopped time covers the whole
/// request -> `complete_gc` span, the sync time never exceeds it, and the
/// application time grows by the interval between pauses (and only when
/// the next pause begins).
#[test]
fn gci_i44_sp_each_pause_counts_once_with_its_stopped_and_application_time() {
    let barrier = GcBarrier::new();
    std::thread::sleep(Duration::from_millis(2));
    assert!(barrier.request_stw(ThreadId(0), 1));
    let first_app = barrier.safepoint_stats().application_ns;
    assert!(
        first_app >= 2_000_000,
        "creation -> first request is application time: {first_app}"
    );
    barrier.wait_for_all();
    std::thread::sleep(Duration::from_millis(3));
    assert_eq!(barrier.safepoint_stats().count, 0, "counted at completion");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let s = barrier.safepoint_stats();
    assert_eq!(s.count, 1);
    assert!(s.stopped_ns >= 3_000_000, "the pause lasted 3 ms: {s:?}");
    assert!(s.sync_ns <= s.stopped_ns, "{s:?}");
    assert_eq!(s.application_ns, first_app, "nothing added until the next pause");

    // A completion with no pause in flight is not a safepoint.
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.safepoint_stats().count, 1);

    std::thread::sleep(Duration::from_millis(2));
    assert!(barrier.request_stw(ThreadId(0), 1));
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let t = barrier.safepoint_stats();
    assert_eq!(t.count, 2);
    assert!(t.stopped_ns >= s.stopped_ns);
    assert!(
        t.application_ns >= first_app + 2_000_000,
        "the 2 ms between the pauses is application time: {t:?}"
    );
    assert!(t.sync_ns <= t.stopped_ns);
}

/// A losing request is no safepoint of its own.
#[test]
fn gci_i44_sp_a_losing_request_adds_nothing() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert!(!barrier.request_stw(ThreadId(1), 2), "a pause is in flight");
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.safepoint_stats().count, 1);
}
