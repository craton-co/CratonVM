use super::*;

fn rec(id: u64) -> GcNotificationRecord {
    let now = std::time::Instant::now();
    GcNotificationRecord {
        collector: JMX_SERIAL_YOUNG_COLLECTOR,
        action: GC_ACTION_MINOR,
        cause: "Allocation Failure",
        id,
        start: now,
        end: now,
        timestamp_ms: 0,
        before: Vec::new(),
        after: Vec::new(),
    }
}

fn queue_with(ids: &[u64]) -> GcNotificationQueue {
    let q = GcNotificationQueue::new();
    assert!(q.set_enabled(JMX_SERIAL_YOUNG_COLLECTOR, true));
    for &id in ids {
        q.record(rec(id));
    }
    q
}

/// A refused record goes back to the HEAD, stays pending, and is the next
/// one popped -- in order, before the records queued behind it.
#[test]
fn gci_i47w_oe47_a_refused_notification_is_kept_at_the_head() {
    let q = queue_with(&[1, 2]);
    {
        let d = q.begin_drain().expect("drainer");
        let first = d.pop().expect("one");
        assert_eq!(first.id, 1);
        assert!(d.requeue_refused(first));
    }
    assert!(q.has_pending());
    let d = q.begin_drain().expect("drainer");
    assert_eq!(d.pop().map(|r| r.id), Some(1));
    d.note_outcome(GcNotificationOutcome::Delivered);
    assert_eq!(d.pop().map(|r| r.id), Some(2));
    let c = q.census();
    assert_eq!((c.delivered, c.failed, c.dropped), (1, 0, 0));
}

/// Bounded: after the streak bound the record is given up (the caller
/// counts it `Failed`), and a completed delivery restarts the streak.
#[test]
fn gci_i47w_oe47_the_refusal_streak_is_bounded_and_reset_by_a_delivery() {
    let q = queue_with(&[7]);
    let d = q.begin_drain().expect("drainer");
    for _ in 0..GC_NOTIFICATION_REFUSED_RETRIES {
        let r = d.pop().expect("kept");
        assert!(d.requeue_refused(r));
    }
    let r = d.pop().expect("kept");
    assert!(!d.requeue_refused(r), "the bound gives it up");
    assert!(d.pop().is_none());
    q.record(rec(8));
    let r = d.pop().expect("new");
    assert!(d.requeue_refused(r), "the streak restarted");
    let r = d.pop().expect("again");
    d.note_outcome(GcNotificationOutcome::Delivered);
    assert_eq!(r.id, 8);
    assert_eq!(q.refused_streak.load(Ordering::Relaxed), 0);
    assert!(refused_notification_kept(0));
    assert!(!refused_notification_kept(GC_NOTIFICATION_REFUSED_RETRIES));
}
