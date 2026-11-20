use super::heap_refusal_message;

/// The rows' two failures are heap refusals; the array-length refusal
/// and an unrelated message are not.
#[test]
fn gci_i47w_oe47_heap_refusals_are_recognised() {
    assert!(heap_refusal_message(Some("Java heap space")));
    assert!(heap_refusal_message(Some(
        "Java heap space (native reference array of length 8)"
    )));
    assert!(heap_refusal_message(None));
    assert!(heap_refusal_message(Some("")));
    assert!(!heap_refusal_message(Some(
        "Requested array size exceeds VM limit"
    )));
    assert!(!heap_refusal_message(Some("Metaspace")));
}

/// gci i48a/oe47c: a refused build first collects on the drainer and
/// builds once more; only a second refusal is put back on the queue.
#[test]
fn gci_i47w_oe47_a_refused_build_collects_and_retries_before_requeue() {
    let src = include_str!("../gc_events.rs");
    let start = src
        .find("pub(super) fn run_gc_notifications(")
        .expect("the drain");
    let drain = &src[start..];
    let first = drain.find("let mut delivered = deliver(&mut *thread);").expect("first build");
    let reclaim = drain
        .find("gc_notification_reclaim_before_retry(shared, &mut *thread)")
        .expect("the drainer's collection");
    let requeue = drain.find("drain.requeue_refused(rec)").expect("requeue");
    assert!(first < reclaim && reclaim < requeue);
    assert!(drain[reclaim..requeue].contains("delivered = deliver(&mut *thread);"));
}
