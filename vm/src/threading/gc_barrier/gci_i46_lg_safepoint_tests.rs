use super::*;

fn pause(barrier: &GcBarrier) {
    assert!(barrier.request_stw(ThreadId(0), 1));
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// M3: a `complete_gc` with no pause in flight neither counts nor moves
/// the application-time base, so the interval before it stays
/// application time. (Before i46/lg the base was `released_at_ns`, which
/// the spurious call rewrote, and the 3 ms below were lost.)
#[test]
fn gci_i46_lg_a_spurious_completion_keeps_the_application_interval() {
    let barrier = GcBarrier::new();
    pause(&barrier);
    let first = barrier.safepoint_stats();
    assert_eq!(first.count, 1);
    std::thread::sleep(Duration::from_millis(3));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.safepoint_stats(), first, "a spurious completion adds nothing");
    assert!(barrier.request_stw(ThreadId(0), 1));
    let s = barrier.safepoint_stats();
    assert!(
        s.application_ns >= first.application_ns + 3_000_000,
        "the 3 ms before the spurious call are application time: {first:?} -> {s:?}"
    );
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.safepoint_stats().count, 2);
}

/// Residual 6: a pause marked as no HotSpot safepoint adds no count, no
/// stopped or sync time, and gives its application interval back at
/// once; the next counted pause's interval spans it.
#[test]
fn gci_i46_lg_a_pause_that_is_no_safepoint_is_not_counted() {
    let barrier = GcBarrier::new();
    // Outside a pause: a no-op.
    barrier.note_pause_not_a_safepoint();
    pause(&barrier);
    let first = barrier.safepoint_stats();
    std::thread::sleep(Duration::from_millis(2));
    assert!(barrier.request_stw(ThreadId(0), 1));
    barrier.wait_for_all();
    assert!(barrier.safepoint_stats().application_ns > first.application_ns);
    barrier.note_pause_not_a_safepoint();
    barrier.note_pause_not_a_safepoint();
    assert_eq!(
        barrier.safepoint_stats().application_ns,
        first.application_ns,
        "taken back inside the pause, once"
    );
    std::thread::sleep(Duration::from_millis(1));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let after = barrier.safepoint_stats();
    assert_eq!(after.count, 1);
    assert_eq!(after.stopped_ns, first.stopped_ns);
    assert_eq!(after.sync_ns, first.sync_ns);
    assert_eq!(after.application_ns, first.application_ns);
    // The TTSP census itself keeps every pause.
    assert_eq!(barrier.ttsp_stats().pauses, 2);
    std::thread::sleep(Duration::from_millis(2));
    assert!(barrier.request_stw(ThreadId(0), 1));
    let s = barrier.safepoint_stats();
    assert!(
        s.application_ns >= first.application_ns + 5_000_000,
        "from the first pause's release, across the uncounted one: {first:?} -> {s:?}"
    );
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let t = barrier.safepoint_stats();
    assert_eq!(t.count, 2);
    assert!(t.sync_ns <= t.stopped_ns);
}


/// Residual 2: the innermost scope of the calling thread names its
/// frame-trace pauses; another thread, and the thread after the last
/// scope drops, see none.
#[test]
fn gci_i46_lg_frame_trace_op_scopes_nest_per_thread() {
    let barrier = GcBarrier::new();
    assert_eq!(barrier.frame_trace_op(), None);
    let outer = barrier.frame_trace_op_scope(None);
    assert_eq!(barrier.frame_trace_op(), Some(None));
    {
        let _inner = barrier.frame_trace_op_scope(Some("PrintThreads"));
        assert_eq!(barrier.frame_trace_op(), Some(Some("PrintThreads")));
        std::thread::scope(|s| {
            s.spawn(|| assert_eq!(barrier.frame_trace_op(), None, "per thread"));
        });
    }
    assert_eq!(barrier.frame_trace_op(), Some(None));
    drop(outer);
    assert_eq!(barrier.frame_trace_op(), None);
    assert_eq!(barrier.frame_trace_ops_used.load(Ordering::Acquire), 0);
}
