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

#[test]
fn zt13x10_a_refused_drain_point_is_seen_by_the_drainer_once() {
    let q = GcNotificationQueue::new();
    assert!(q.set_enabled(JMX_SERIAL_YOUNG_COLLECTOR, true));
    let d = q.begin_drain_noting_miss().expect("a free queue");
    assert!(!q.end_drain_take_missed(d), "nothing was refused");

    let d = q.begin_drain().expect("the drainer");
    q.record(rec(1));
    assert!(q.begin_drain_noting_miss().is_none(), "refused while held");
    assert!(q.end_drain_take_missed(d), "the drainer sees the refusal");
    assert!(q.has_pending(), "the record waits for the re-drain");
    let d = q.begin_drain().expect("free again");
    assert_eq!(d.pop().map(|r| r.id), Some(1));
    assert!(!q.end_drain_take_missed(d), "the mark is taken once");

    // Positive control of the mark itself: the plain door's refusal
    // leaves none (the behaviour before zt t13, and off ZGC).
    let d = q.begin_drain().expect("the drainer");
    assert!(q.begin_drain().is_none());
    assert!(!q.end_drain_take_missed(d), "the plain door marks nothing");
}

/// The race the mark closes: a refusal and the drainer's release run
/// together. Whatever the interleaving, the refused side's retry wins
/// the drain or the drainer sees the mark -- never neither.
#[test]
fn zt13x10_either_the_retry_wins_or_the_drainer_sees_the_mark() {
    for round in 0..2000 {
        let q = std::sync::Arc::new(GcNotificationQueue::new());
        let held = q.begin_drain().expect("the drainer");
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (q2, gate2) = (std::sync::Arc::clone(&q), std::sync::Arc::clone(&gate));
        let refused = std::thread::spawn(move || {
            gate2.wait();
            q2.begin_drain_noting_miss().map(drop).is_some()
        });
        gate.wait();
        let seen = q.end_drain_take_missed(held);
        let won = refused.join().expect("the refused side");
        assert!(won || seen, "round {round}: the refusal was lost");
    }
}
