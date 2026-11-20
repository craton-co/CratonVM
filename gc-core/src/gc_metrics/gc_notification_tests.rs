use super::*;

fn pools(eden: u64, tenured: u64) -> [JmxPoolUsage; 3] {
    let p = |name, used| JmxPoolUsage {
        name,
        init: None,
        used,
        committed: 1 << 20,
        max: Some(1 << 21),
        collection_used: None,
        managers: serial_pool_managers(name),
    };
    [
        p(JMX_POOL_EDEN, eden),
        p(JMX_POOL_SURVIVOR, 0),
        p(JMX_POOL_TENURED, tenured),
    ]
}

/// gen r5w3/obs7: Serial's threshold support table.
#[test]
fn serial_threshold_support_matches_hotspot() {
    assert_eq!(serial_pool_threshold_support(JMX_POOL_EDEN), (false, true));
    assert_eq!(
        serial_pool_threshold_support(JMX_POOL_SURVIVOR),
        (false, true)
    );
    assert_eq!(
        serial_pool_threshold_support(JMX_POOL_TENURED),
        (true, true)
    );
    assert_eq!(serial_pool_threshold_support("G1 Old Gen"), (false, false));
}

/// gcd d10/o retirement: HotSpot 25.0.4's G1 / ZGC answers.
#[test]
fn backend_threshold_support_matches_hotspot_25() {
    for (pool, want) in [
        ("G1 Eden Space", (false, true)),
        ("G1 Survivor Space", (false, true)),
        ("G1 Old Gen", (true, true)),
        ("ZGC Young Generation", (false, true)),
        ("ZGC Old Generation", (true, true)),
        ("Tenured Gen", (false, false)),
        ("ZHeap", (false, false)),
    ] {
        assert_eq!(backend_pool_threshold_support(pool), want, "{pool}");
    }
}

/// gen r5w3/obs7: nothing is checked or queued until a threshold is set,
/// and a collection threshold only fires for a pool the collection
/// affected — every one at or above it (HotSpot's counter sensor), with
/// the usage at the crossing; falling below clears the sensor.
#[test]
fn collection_threshold_is_a_counter_sensor_over_affected_pools() {
    let q = GcNotificationQueue::new();
    let t = q.pool_thresholds();
    t.check(&pools(0, 5_000), &[JMX_POOL_TENURED]);
    assert!(!t.is_armed() && !q.has_pending(), "unarmed: no work");

    assert_eq!(
        t.set(JMX_POOL_TENURED, PoolThresholdKind::Collection, 1_000),
        0
    );
    assert!(t.is_armed());
    // A young collection does not affect Tenured Gen.
    t.check(&pools(0, 5_000), &[JMX_POOL_EDEN, JMX_POOL_SURVIVOR]);
    assert!(!q.has_pending());
    // A full collection leaving it at 5 000 >= 1 000: one trigger.
    t.check(&pools(0, 5_000), JMX_SERIAL_OLD_POOLS);
    assert!(q.has_pending(), "the drain must see the sensor request");
    let reqs = t.take_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        (reqs[0].pool, reqs[0].kind, reqs[0].count),
        (JMX_POOL_TENURED, PoolThresholdKind::Collection, 1)
    );
    assert_eq!(reqs[0].trigger_usage, Some([-1, 5_000, 1 << 20, 1 << 21]));
    assert!(!q.has_pending());
    // Still above after the next full collection: triggered again (a
    // counter, unlike the usage gauge).
    t.check(&pools(0, 4_000), JMX_SERIAL_OLD_POOLS);
    let reqs = t.take_requests();
    assert_eq!((reqs.len(), reqs[0].count), (1, 1));
    // Below: the sensor, on, is cleared.
    t.check(&pools(0, 10), JMX_SERIAL_OLD_POOLS);
    let reqs = t.take_requests();
    assert_eq!(
        (reqs.len(), reqs[0].count, reqs[0].trigger_usage),
        (1, 0, None)
    );
    // Below again, sensor off: nothing.
    t.check(&pools(0, 10), JMX_SERIAL_OLD_POOLS);
    assert!(t.take_requests().is_empty());
    // Disarmed.
    assert_eq!(
        t.set(JMX_POOL_TENURED, PoolThresholdKind::Collection, 0),
        1_000
    );
    assert!(!t.is_armed());
}

/// gen r5w3/obs7: the usage threshold is HotSpot's gauge sensor — one
/// trigger per crossing however many samples stay above it, and one
/// clear when usage falls back below.
#[test]
fn usage_threshold_is_a_gauge_sensor() {
    let q = GcNotificationQueue::new();
    let t = q.pool_thresholds();
    t.set(JMX_POOL_TENURED, PoolThresholdKind::Usage, 1_000);
    t.check(&pools(0, 2_000), &[]);
    t.check(&pools(0, 3_000), &[]);
    let reqs = t.take_requests();
    assert_eq!(reqs.len(), 1, "one crossing, one request: {reqs:?}");
    assert_eq!(
        (
            reqs[0].kind,
            reqs[0].count,
            reqs[0].trigger_usage.map(|u| u[1])
        ),
        (PoolThresholdKind::Usage, 1, Some(2_000)),
        "the usage recorded at the crossing"
    );
    // On, still above: nothing new.
    t.check(&pools(0, 2_500), &[]);
    assert!(!q.has_pending());
    assert!(t.take_requests().is_empty());
    // Below: cleared once.
    t.check(&pools(0, 500), &[]);
    t.check(&pools(0, 400), &[]);
    let reqs = t.take_requests();
    assert_eq!(
        (reqs.len(), reqs[0].count, reqs[0].trigger_usage),
        (1, 0, None)
    );
    // A new crossing triggers again.
    t.check(&pools(0, 1_000), &[]);
    assert_eq!(t.take_requests()[0].count, 1);
    // A collection threshold was never set: eden's usage is not checked.
    t.check(&pools(1 << 30, 0), JMX_SERIAL_OLD_POOLS);
    assert_eq!(t.threshold(JMX_POOL_EDEN, PoolThresholdKind::Collection), 0);
}

/// gcf f4/r: a tenured-only sample on old-generation growth trips the
/// usage gauge once, outside any collection; unarmed it is one load and
/// nothing pends.
#[test]
fn gcf_f4r_usage_threshold_trips_on_old_generation_growth() {
    let q = GcNotificationQueue::new();
    let t = q.pool_thresholds();
    let tenured = |used| pools(0, used)[2];
    assert!(!t.check_usage_on_growth(&tenured(5_000)), "unarmed");
    assert!(!q.has_pending());
    t.set(JMX_POOL_TENURED, PoolThresholdKind::Usage, 1_000);
    assert!(!t.check_usage_on_growth(&tenured(900)), "below");
    assert!(t.check_usage_on_growth(&tenured(1_500)), "crossed");
    assert!(
        q.has_pending(),
        "the allocation slow path's drain must see it"
    );
    let reqs = t.take_requests();
    assert_eq!(
        (reqs.len(), reqs[0].pool, reqs[0].kind, reqs[0].count),
        (1, JMX_POOL_TENURED, PoolThresholdKind::Usage, 1)
    );
    assert!(
        !t.check_usage_on_growth(&tenured(1_800)),
        "still above: no new request"
    );
}

/// gci i16/nt: a non-heap pool's usage threshold lives in the same table
/// and is HotSpot's gauge: one trigger per crossing, counted, a clear
/// when usage falls back (class unloading), and a heap-only sample never
/// touches it.
#[test]
fn gci_i16_nt_a_metaspace_usage_threshold_is_a_gauge_in_the_same_table() {
    let q = GcNotificationQueue::new();
    let t = q.pool_thresholds();
    let metaspace = |used| JmxPoolUsage {
        name: "Metaspace",
        init: Some(0),
        used,
        committed: used,
        max: None,
        collection_used: None,
        managers: &["Metaspace Manager"],
    };
    t.set("Metaspace", PoolThresholdKind::Usage, 10_000);
    t.check(&[metaspace(9_999)], &[]);
    assert!(!q.has_pending(), "below the threshold");
    // A heap sample (no `Metaspace` in it) leaves the entry alone.
    t.check(&pools(1 << 20, 1 << 20), JMX_SERIAL_OLD_POOLS);
    assert!(!q.has_pending());
    t.check(&[metaspace(12_000)], &[]);
    t.check(&[metaspace(15_000)], &[]);
    let reqs = t.take_requests();
    assert_eq!(reqs.len(), 1, "one crossing, one request: {reqs:?}");
    assert_eq!(
        (reqs[0].pool, reqs[0].kind, reqs[0].count, reqs[0].trigger_usage),
        (
            "Metaspace",
            PoolThresholdKind::Usage,
            1,
            Some([0, 12_000, 12_000, -1])
        ),
        "the usage at the crossing, `max` undefined"
    );
    t.check(&[metaspace(4_000)], &[]);
    let reqs = t.take_requests();
    assert_eq!((reqs.len(), reqs[0].count, reqs[0].trigger_usage), (1, 0, None));
}

/// gci i16/nt: the `Metaspace` sample cadence -- due before the first
/// sample, then only after `max(64 KiB, used / 64)` class-file bytes, or
/// at once when the unloaded-class count moved.
#[test]
fn gci_i16_nt_the_metaspace_sample_cadence_follows_growth_and_unloading() {
    let t = PoolThresholds::default();
    assert!(t.non_heap_walk_due(0, 0), "never sampled: due");
    t.note_non_heap_walk(1 << 20, 0);
    assert!(!t.non_heap_walk_due(0, 0), "a collection end with nothing unloaded");
    assert!(!t.non_heap_walk_due(60 * 1024, 0), "60 KiB < 64 KiB");
    assert!(t.non_heap_walk_due(4 * 1024, 0), "64 KiB grown");
    t.note_non_heap_walk(64 << 20, 0);
    // 64 MiB / 64 = 1 MiB.
    assert!(!t.non_heap_walk_due(512 * 1024, 0));
    assert!(!t.non_heap_walk_due(511 * 1024, 0));
    assert!(t.non_heap_walk_due(1024, 0), "1 MiB grown");
    t.note_non_heap_walk(64 << 20, 0);
    assert!(t.non_heap_walk_due(0, 3), "classes were unloaded");
    t.note_non_heap_walk(60 << 20, 3);
    assert!(!t.non_heap_walk_due(0, 3));
}

/// gen r5w2/obs6: pool peaks are raised at collection edges (`used` and
/// `committed` separately, as HotSpot's `record_peak_memory_usage`), never
/// lowered by a smaller sample, and a reset sets them to the given
/// current usage.
#[test]
fn pool_peaks_rise_at_collection_edges_and_reset_to_current() {
    let q = GcNotificationQueue::new();
    assert_eq!(q.pool_peak(JMX_POOL_EDEN), None, "nothing sampled yet");
    q.note_pool_peaks(&pools(900, 10));
    q.note_pool_peaks(&pools(100, 30));
    assert_eq!(q.pool_peak(JMX_POOL_EDEN), Some((900, 1 << 20)));
    assert_eq!(q.pool_peak(JMX_POOL_TENURED), Some((30, 1 << 20)));
    q.reset_pool_peak(JMX_POOL_EDEN, 5, 1 << 19);
    assert_eq!(q.pool_peak(JMX_POOL_EDEN), Some((5, 1 << 19)));
    q.note_pool_peaks(&pools(7, 30));
    assert_eq!(q.pool_peak(JMX_POOL_EDEN), Some((7, 1 << 20)));
    assert_eq!(q.pool_peak("No Such Pool"), None);
}

/// gen r5w2/obs6: the concurrent cycle's JFR identity opens at the
/// initial mark, learns its marking start when that pause ends, and is
/// taken exactly once by the remark; a stale stamp for an older cycle is
/// ignored.
#[test]
fn concurrent_cycle_jfr_opens_stamps_and_is_taken_once() {
    let c = ConcurrentCycleJfr::default();
    assert_eq!(c.take(), None);
    c.open(7);
    c.mark_started(6, 111);
    c.mark_started(7, 222);
    assert_eq!(c.take(), Some((7, 222)));
    assert_eq!(c.take(), None, "taken once");
    c.open(9);
    c.open(10);
    assert_eq!(
        c.take(),
        Some((10, 0)),
        "a newer initial mark replaces an abandoned cycle"
    );
}

/// gci i13/gp: a full collection's abort mark is per cycle id, taken
/// once, and a stale mark for another cycle matches nothing.
#[test]
fn gci_i13_gp_the_abort_mark_is_per_cycle_and_taken_once() {
    let c = ConcurrentCycleJfr::default();
    assert!(!c.take_aborted(4), "nothing aborted yet");
    c.mark_aborted(4);
    assert!(!c.take_aborted(5), "another cycle is not aborted");
    assert!(c.take_aborted(4));
    assert!(!c.take_aborted(4), "taken once");
    c.mark_aborted(6);
    c.open(8);
    assert!(!c.take_aborted(8), "a stale mark does not abort a newer cycle");
    // The open cycle's own state is untouched by the mark.
    assert_eq!(c.open_id(), Some(8));
}

/// zs s10/jf3: the mark-start pause noted for a cycle is taken once, and
/// only by that cycle's id; another cycle's entry is dropped. zt t1/jf:
/// `take_any_initial_pause` drains whatever is noted.
#[test]
fn zs10jf3_initial_pause_is_taken_once_by_its_cycle() {
    let c = ConcurrentCycleJfr::default();
    assert_eq!(c.take_initial_pause(7), None, "nothing noted");
    c.note_initial_pause(7, 1_000, 50);
    assert_eq!(c.take_initial_pause(7), Some((1_000, 50)));
    assert_eq!(c.take_initial_pause(7), None, "taken once");
    c.note_initial_pause(8, 2_000, 60);
    assert_eq!(c.take_initial_pause(9), None, "another cycle's entry");
    assert_eq!(c.take_initial_pause(8), None, "and it is dropped");
    c.note_initial_pause(10, 3_000, 70);
    assert_eq!(c.take_any_initial_pause(), Some((10, 3_000, 70)));
    assert_eq!(c.take_any_initial_pause(), None);
    // zt t2/jf: the cycle's before figures, by the same rule.
    assert_eq!(c.take_cycle_heap_before(10), None);
    c.note_cycle_heap_before(10, 4_096, 8_192);
    assert_eq!(c.take_cycle_heap_before(11), None, "another cycle's entry");
    assert_eq!(c.take_cycle_heap_before(10), None, "and it is dropped");
    c.note_cycle_heap_before(12, 4_096, 8_192);
    assert_eq!(c.take_cycle_heap_before(12), Some((4_096, 8_192)));
    assert_eq!(c.take_cycle_heap_before(12), None, "taken once");
}

fn snap(all: u64, old: u64, eden: u64, tenured: u64) -> GcBeanSnapshot {
    GcBeanSnapshot::now(
        serial_collectors((all, all * 1000, all), (old, old * 1000, old)),
        pools(eden, tenured),
    )
}

#[test]
fn a_young_cycle_is_a_minor_notification_from_copy() {
    let before = snap(4, 1, 900, 100);
    let after = snap(5, 1, 50, 120);
    let ns = gc_notifications_between(&before, &after, "Allocation Failure", 77);
    assert_eq!(ns.len(), 1, "a young cycle moves `Copy` only");
    let n = &ns[0];
    assert_eq!(
        (n.collector, n.action, n.cause, n.id, n.timestamp_ms),
        ("Copy", "end of minor GC", "Allocation Failure", 4, 77)
    );
    assert_eq!(n.before[0].used, 900);
    assert_eq!(n.after[0].used, 50);
    assert!(n.end >= n.start);
}

#[test]
fn a_full_cycle_is_a_major_notification_from_marksweepcompact_only() {
    // `all` and `old` both move; `Copy`'s count (all - old) does not, so
    // the full collection is ONE notification, under the full collector.
    let before = snap(5, 1, 50, 120);
    let after = snap(6, 2, 0, 80);
    let ns = gc_notifications_between(&before, &after, "System.gc()", 1);
    assert_eq!(ns.len(), 1, "ONE notification for a full collection");
    let n = &ns[0];
    assert_eq!(
        (n.collector, n.action, n.cause, n.id),
        ("MarkSweepCompact", "end of major GC", "System.gc()", 2)
    );
}

#[test]
fn a_refused_cycle_notifies_nothing() {
    let s = snap(5, 1, 50, 120);
    assert!(gc_notifications_between(&s, &s, "Allocation Failure", 0).is_empty());
}

fn rec(collector: &'static str, id: u64) -> GcNotificationRecord {
    let s = snap(0, 0, 0, 0);
    GcNotificationRecord {
        collector,
        action: GC_ACTION_MINOR,
        cause: "Allocation Failure",
        id,
        start: s.at,
        end: s.at,
        timestamp_ms: 0,
        before: s.pools.clone(),
        after: s.pools,
    }
}

#[test]
fn only_a_bean_with_a_listener_queues_but_every_bean_keeps_its_last_gc() {
    let q = GcNotificationQueue::new();
    q.record(rec("Copy", 1));
    assert!(!q.has_pending(), "no listener: nothing queued");
    assert_eq!(
        q.last("Copy").map(|r| r.id),
        Some(1),
        "getLastGcInfo still works"
    );
    assert!(q.set_enabled("Copy", true));
    assert!(
        !q.set_enabled("G1 Young Generation", true),
        "not this heap's bean"
    );
    q.record(rec("Copy", 2));
    q.record(rec("MarkSweepCompact", 1));
    assert!(q.has_pending());
    let d = q.begin_drain().unwrap();
    assert_eq!(d.pop().map(|r| (r.collector, r.id)), Some(("Copy", 2)));
    assert_eq!(d.pop(), None, "MarkSweepCompact had no listener");
    d.note_outcome(GcNotificationOutcome::Delivered);
    drop(d);
    assert!(!q.has_pending());
    assert_eq!(q.last("MarkSweepCompact").map(|r| r.id), Some(1));
    let c = q.census();
    assert_eq!((c.recorded, c.queued, c.delivered), (3, 1, 1));
}

/// gcg h1/ob (`gcd-d10o-first-gc-notification-arrives-a-second-late`):
/// the first queued notification's wait and delivery are stamped once,
/// in order; a drain before anything was queued stamps nothing, and later
/// notifications do not move the split.
#[test]
fn gch_h1ob_first_notification_latency_is_split_once() {
    let q = GcNotificationQueue::new();
    assert_eq!(q.census().first_delivery_us, (None, None));
    drop(q.begin_drain().unwrap());
    assert_eq!(
        q.first_delivery_split(),
        FirstDeliverySplit::default(),
        "nothing queued yet"
    );
    assert!(q.set_enabled("Copy", true));
    q.record(rec("Copy", 1));
    let f = q.first_delivery_split();
    assert!(f.queued.is_some() && f.drain_started.is_none());
    assert_eq!(q.census().first_delivery_us, (None, None));
    let d = q.begin_drain().unwrap();
    let _ = d.pop();
    d.note_outcome(GcNotificationOutcome::Delivered);
    drop(d);
    let first = q.first_delivery_split();
    assert!(first.delivered.is_some());
    let (wait, deliver) = q.census().first_delivery_us;
    assert!(wait.is_some() && deliver.is_some());
    q.record(rec("Copy", 2));
    let d = q.begin_drain().unwrap();
    let _ = d.pop();
    d.note_outcome(GcNotificationOutcome::Delivered);
    drop(d);
    assert_eq!(q.first_delivery_split(), first, "stamped once");
    let line = q.census().summary_line();
    assert!(line.contains(" notif_first_wait_us="), "{line}");
    assert!(line.contains(" notif_first_deliver_us="), "{line}");
    assert!(GcNotificationCensus::default()
        .summary_line()
        .ends_with("notif_first_wait_us=- notif_first_deliver_us=-"));
}

#[test]
fn one_drainer_at_a_time_and_the_right_is_released_on_drop() {
    let q = GcNotificationQueue::new();
    let d = q.begin_drain().unwrap();
    assert!(
        q.begin_drain().is_none(),
        "a re-entrant drain must not nest"
    );
    drop(d);
    assert!(q.begin_drain().is_some());
}

#[test]
fn an_undrained_queue_is_bounded_and_drops_the_oldest() {
    let q = GcNotificationQueue::new();
    q.set_enabled("Copy", true);
    for id in 1..=(GC_NOTIFICATION_QUEUE_CAPACITY as u64 + 3) {
        q.record(rec("Copy", id));
    }
    assert_eq!(q.census().dropped, 3);
    let d = q.begin_drain().unwrap();
    assert_eq!(d.pop().map(|r| r.id), Some(4), "the three oldest went");
}

#[test]
fn the_summary_line_keys_are_prefixed() {
    let line = GcNotificationCensus::default().summary_line();
    assert!(line.starts_with("[GC] gc_notifications: notif_recorded=0 "));
    for word in line.split_whitespace().skip(2) {
        assert!(word.starts_with("notif_"), "{word}");
    }
}

// ---- gc-common w7-f: G1 and ZGC beans ----

fn sample(used: u64, committed: u64) -> JmxPoolSample {
    JmxPoolSample {
        init: None,
        used,
        committed,
        max: None,
    }
}

fn backend_snap(b: &BackendGcBeans, samples: &[JmxPoolSample]) -> GcBeanSnapshot {
    GcBeanSnapshot::now(b.collectors(), b.pools(samples))
}

/// W7F-1: G1 reports HotSpot's three collectors and three pools, and the
/// names join both ways (every pool a collector lists exists, and lists
/// that collector as a manager) — the JConsole join the pre-w7 answer
/// (`G1 Young Generation` over `Eden Space`/`Old Gen`) failed.
#[test]
fn w7f_g1_and_zgc_beans_have_hotspots_names_and_join_both_ways() {
    for shape in [BackendBeanShape::G1, BackendBeanShape::Zgc] {
        let b = BackendGcBeans::new(shape);
        let collectors = b.collectors();
        let pools = b.pools(&[sample(1, 2), sample(3, 4), sample(5, 6)]);
        if shape == BackendBeanShape::Zgc {
            // HotSpot 25: every ZGC collector manages both pools (zr
            // z2/t2: listed cycles first, then pauses, by default).
            for p in &pools {
                let mut got = p.managers.to_vec();
                got.sort_unstable();
                let mut want = shape.collectors().to_vec();
                want.sort_unstable();
                assert_eq!(got, want, "{}", p.name);
            }
        }
        assert_eq!(pools.len(), shape.pools().len());
        for c in &collectors {
            for p in c.pools {
                let pool = pools.iter().find(|q| q.name == *p);
                assert!(pool.is_some(), "{} lists unknown pool {p}", c.name);
                assert!(pool.is_some_and(|q| q.managers.contains(&c.name)));
            }
        }
        for p in &pools {
            for m in p.managers {
                let c = collectors.iter().find(|c| c.name == *m);
                assert!(
                    c.is_some_and(|c| c.pools.contains(&p.name)),
                    "{m} / {}",
                    p.name
                );
            }
        }
        assert!(b.notifications().set_enabled(collectors[0].name, true));
        assert!(
            !b.notifications().set_enabled("Copy", true),
            "not this heap's bean"
        );
    }
    let g1: Vec<&str> = BackendGcBeans::new(BackendBeanShape::G1)
        .collectors()
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(
        g1,
        [
            "G1 Young Generation",
            "G1 Old Generation",
            "G1 Concurrent GC"
        ]
    );
    assert_eq!(
        BackendBeanShape::G1.pools(),
        ["G1 Eden Space", "G1 Survivor Space", "G1 Old Gen"]
    );
    // gcd d10/o retirement: HotSpot 25's generational ZGC names, in its
    // bean order (25.0.4, `Gcd10oBeanShapeProbe`).
    assert_eq!(
        BackendBeanShape::Zgc.collectors(),
        [
            "ZGC Minor Cycles",
            "ZGC Minor Pauses",
            "ZGC Major Cycles",
            "ZGC Major Pauses"
        ]
    );
    assert_eq!(
        BackendBeanShape::Zgc.pools(),
        ["ZGC Young Generation", "ZGC Old Generation"]
    );
}

/// W7F-2: a G1 collection pause moves `G1 Young Generation` only and is
/// ONE `end of minor GC` notification; a marking pause moves `G1
/// Concurrent GC`; `G1 Old Generation` (no full collections) stays 0.
/// Each bean's time is the floor of its microsecond pause sum (gce e1/o).
#[test]
fn w7f_g1_collection_and_marking_pauses_count_under_their_beans() {
    let b = BackendGcBeans::new(BackendBeanShape::G1);
    let s = [sample(900, 1 << 20), sample(0, 0), sample(100, 1 << 21)];
    let before = backend_snap(&b, &s);
    b.note_collection(400_000); // 0.4 ms
    let after = backend_snap(
        &b,
        &[
            sample(0, 1 << 20),
            sample(50, 1 << 20),
            sample(120, 1 << 21),
        ],
    );
    let ns = gc_notifications_between(&before, &after, "Allocation Failure", 5);
    assert_eq!(ns.len(), 1);
    assert_eq!(
        (ns[0].collector, ns[0].action, ns[0].id),
        ("G1 Young Generation", "end of minor GC", 1)
    );
    assert_eq!(ns[0].before.len(), 3);
    assert_eq!(ns[0].after[1].used, 50);
    let c = b.collectors();
    assert_eq!(
        (c[0].count, c[0].time_ms),
        (1, 0),
        "a sub-ms pause moves the count, not the time (HotSpot's accumulated timer)"
    );
    assert_eq!((c[1].count, c[2].count), (0, 0));

    let before = backend_snap(&b, &s);
    b.note_non_collection_pause(2_000_000);
    let after = backend_snap(&b, &s);
    let ns = gc_notifications_between(&before, &after, "No GC", 6);
    assert_eq!(ns.len(), 1);
    assert_eq!(
        (ns[0].collector, ns[0].action, ns[0].id),
        ("G1 Concurrent GC", "end of concurrent GC pause", 1)
    );
    let c = b.collectors();
    assert_eq!((c[0].count, c[2].count, c[2].time_ms), (1, 1, 2));
}

/// gci i12/ge (`gci-i10dl-...-door-residuals-20261013` (b)): a booked G1
/// full collection moves `G1 Old Generation` only and is ONE `end of major
/// GC` notification; ZGC has no such bean (no-op).
#[test]
fn gci_i12_ge_a_g1_full_collection_counts_under_the_old_bean() {
    let b = BackendGcBeans::new(BackendBeanShape::G1);
    let s = [sample(900, 1 << 20), sample(0, 0), sample(100, 1 << 21)];
    let before = backend_snap(&b, &s);
    b.note_full_collection(3_000_000);
    let after = backend_snap(&b, &s);
    let ns = gc_notifications_between(&before, &after, "System.gc()", 7);
    assert_eq!(ns.len(), 1);
    assert_eq!(
        (ns[0].collector, ns[0].action, ns[0].id),
        ("G1 Old Generation", "end of major GC", 1)
    );
    let c = b.collectors();
    assert_eq!((c[0].count, c[1].count, c[1].time_ms, c[2].count), (0, 1, 3, 0));
    let z = BackendGcBeans::new(BackendBeanShape::Zgc);
    z.note_full_collection(3_000_000);
    assert!(z.collectors().iter().all(|c| c.count == 0));
}

/// gci i12/ge: `open_id` reads the open cycle's id and leaves it open.
#[test]
fn gci_i12_ge_the_open_cycle_id_is_read_without_closing_it() {
    let c = ConcurrentCycleJfr::default();
    assert_eq!(c.open_id(), None);
    c.open(41);
    c.mark_started(41, 9);
    assert_eq!(c.open_id(), Some(41));
    assert_eq!(c.take(), Some((41, 9)), "still open, start kept");
    assert_eq!(c.open_id(), None);
    // Item 4 of the i12/ge findings page: a re-open (a retried remark)
    // drops `open`'s start but not the cycle's start.
    c.open(41);
    assert_eq!(c.take(), Some((41, 0)));
    assert_eq!(c.cycle_start(41), Some(9));
    assert_eq!(c.cycle_start(42), None, "another cycle");
    c.open(42);
    c.mark_started(41, 99);
    assert_eq!(c.cycle_start(41), Some(9), "a stale id stamps nothing");
}

/// W7F-3: a ZGC collection is a cycle AND a pause — two notifications,
/// one per bean, as HotSpot sends them; a mark start is a pause only.
/// gcd d10/o retirement: a whole-heap collection is HotSpot 25's Major
/// pair (a `System.gc()` on HotSpot 25 moves exactly these two), a young
/// cycle the Minor pair.
#[test]
fn w7f_zgc_collection_is_a_cycle_and_a_pause() {
    let b = BackendGcBeans::new(BackendBeanShape::Zgc);
    let s = |u| [sample(0, 0), sample(u, 1000)];
    let before = backend_snap(&b, &s(800));
    b.note_collection(1_500_000);
    let after = backend_snap(&b, &s(200));
    let ns = gc_notifications_between(&before, &after, "System.gc()", 9);
    let got: Vec<(&str, &str, u64)> =
        ns.iter().map(|n| (n.collector, n.action, n.id)).collect();
    assert_eq!(
        got,
        [
            ("ZGC Major Cycles", "end of GC cycle", 1),
            ("ZGC Major Pauses", "end of GC pause", 1)
        ]
    );
    b.note_non_collection_pause(100_000);
    let c = b.collectors();
    assert_eq!(
        (c[0].count, c[1].count, c[2].count, c[3].count),
        (0, 0, 1, 2)
    );
    assert_eq!((c[2].time_ms, c[3].time_ms), (1, 1));

    let before = backend_snap(&b, &s(200));
    b.note_collection_of(300_000, CollectionKind::Minor);
    let after = backend_snap(&b, &s(100));
    let ns = gc_notifications_between(&before, &after, "Allocation Rate", 10);
    let got: Vec<(&str, &str, u64)> =
        ns.iter().map(|n| (n.collector, n.action, n.id)).collect();
    assert_eq!(
        got,
        [
            ("ZGC Minor Cycles", "end of GC cycle", 1),
            ("ZGC Minor Pauses", "end of GC pause", 1)
        ]
    );
    let c = b.collectors();
    assert_eq!(
        (c[0].count, c[1].count, c[2].count, c[3].count),
        (1, 1, 1, 2)
    );
}

/// Wave 9 lane G (gch-h4g1): a young-only G1 pause stamps eden and
/// survivor only; `G1 Old Gen` keeps 0 (HotSpot's figure before any
/// affecting collection), then a full stamp moves it.
#[test]
fn w9g_a_young_only_stamp_leaves_the_old_pools_collection_usage() {
    let b = BackendGcBeans::new(BackendBeanShape::G1);
    let s = [sample(0, 4096), sample(512, 1024), sample(9000, 16384)];
    let pools = b.pools(&s);
    b.note_collection_usage_of(&pools, Some(G1_YOUNG_ONLY_POOLS));
    let got: Vec<(&str, Option<u64>)> = b
        .pools(&s)
        .iter()
        .map(|p| (p.name, p.collection_used))
        .collect();
    assert_eq!(
        got,
        [
            ("G1 Eden Space", Some(0)),
            ("G1 Survivor Space", Some(512)),
            ("G1 Old Gen", Some(0))
        ]
    );
    b.note_collection_usage(&pools);
    assert_eq!(b.pools(&s)[2].collection_used, Some(9000));
    let s2 = [sample(0, 4096), sample(256, 1024), sample(9500, 16384)];
    b.note_collection_usage_of(&b.pools(&s2), Some(G1_YOUNG_ONLY_POOLS));
    let after = b.pools(&s2);
    assert_eq!(
        (after[1].collection_used, after[2].collection_used),
        (Some(256), Some(9000))
    );
}

/// gce e1/o (`gce-e1o-g1-zgc-collection-time-adds-a-millisecond-per-pause`):
/// 1 000 pauses of 100 µs read 100 ms on every bean that counts them, as
/// HotSpot's accumulated timer does — not 1 100 (the sum plus one
/// millisecond per pause).
#[test]
fn e1o_a_thousand_100us_pauses_read_100ms_on_each_counting_bean() {
    for shape in [BackendBeanShape::G1, BackendBeanShape::Zgc] {
        let b = BackendGcBeans::new(shape);
        for _ in 0..1_000 {
            b.note_collection(100_000);
        }
        let c = b.collectors();
        for &i in shape.collection_beans(CollectionKind::Major) {
            assert_eq!(
                (c[i].count, c[i].time_ms),
                (1_000, 100),
                "{shape:?} bean {}",
                c[i].name
            );
        }
    }
}

/// zr z2/t2 (`CRATONVM_ZGC_HOTSPOT_BEAN_ORDER`): ZGC pools list cycles
/// then pauses (HotSpot 25, census vz0), and a collection's pause
/// notification is queued before its cycle notification; `=0` restores
/// the bean order on both. G1 is untouched by either arm.
#[test]
fn zr2t2_zgc_pool_managers_and_notifications_follow_hotspot_order() {
    let on = BackendGcBeans::with_order(BackendBeanShape::Zgc, true);
    let off = BackendGcBeans::with_order(BackendBeanShape::Zgc, false);
    let pools = on.pools(&[sample(1, 2), sample(3, 4)]);
    for p in &pools {
        assert_eq!(
            p.managers,
            [
                JMX_ZGC_MINOR_CYCLES_COLLECTOR,
                JMX_ZGC_MAJOR_CYCLES_COLLECTOR,
                JMX_ZGC_MINOR_PAUSES_COLLECTOR,
                JMX_ZGC_MAJOR_PAUSES_COLLECTOR,
            ],
            "{}",
            p.name
        );
    }
    for p in &off.pools(&[sample(1, 2), sample(3, 4)]) {
        assert_eq!(p.managers, BackendBeanShape::Zgc.collectors());
    }
    let before = backend_snap(&on, &[sample(0, 0), sample(0, 0)]);
    on.note_collection(1_000);
    let after = backend_snap(&on, &[sample(0, 0), sample(0, 0)]);
    let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
    let order =
        |recs: &[GcNotificationRecord]| recs.iter().map(|r| r.collector).collect::<Vec<_>>();
    assert_eq!(
        order(&recs),
        [
            JMX_ZGC_MAJOR_CYCLES_COLLECTOR,
            JMX_ZGC_MAJOR_PAUSES_COLLECTOR
        ],
        "bean order before ordering"
    );
    let mut recs_off = recs.clone();
    off.order_notifications(&mut recs_off);
    assert_eq!(order(&recs_off), order(&recs), "=0 keeps bean order");
    on.order_notifications(&mut recs);
    assert_eq!(
        order(&recs),
        [
            JMX_ZGC_MAJOR_PAUSES_COLLECTOR,
            JMX_ZGC_MAJOR_CYCLES_COLLECTOR
        ]
    );
    let g1 = BackendGcBeans::with_order(BackendBeanShape::G1, true);
    let g1_pools = g1.pools(&[sample(1, 2), sample(3, 4), sample(5, 6)]);
    assert_eq!(
        g1_pools[2].managers,
        BackendBeanShape::G1.pool_managers(JMX_POOL_G1_OLD)
    );
}

/// zr z2/t2 (orchestrator z2b follow-up): DELIVERY order, not just the
/// sorted batch. A ZGC collection's records go through
/// `order_notifications` and then `GcNotificationQueue::record` one by
/// one (`record_gc_notification`); the delivery loop
/// (`run_gc_notifications`) pops the queue and calls
/// `createGCNotification` synchronously per record, so what `pop` returns
/// is the order a listener receives. With listeners on both Major beans:
/// pause first by default, cycle first under
/// `CRATONVM_ZGC_HOTSPOT_BEAN_ORDER=0`; a second collection queued behind
/// the first keeps FIFO order between collections.
#[test]
fn zr2t2_zgc_pause_notification_is_delivered_before_the_cycle() {
    fn delivered(order_on: bool) -> Vec<(&'static str, u64)> {
        let b = BackendGcBeans::with_order(BackendBeanShape::Zgc, order_on);
        for name in BackendBeanShape::Zgc.collectors() {
            assert!(b.notifications().set_enabled(name, true));
        }
        for _ in 0..2 {
            let before = backend_snap(&b, &[sample(0, 0), sample(0, 0)]);
            b.note_collection(1_000);
            let after = backend_snap(&b, &[sample(0, 0), sample(0, 0)]);
            let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
            b.order_notifications(&mut recs);
            for rec in recs {
                b.notifications().record(rec);
            }
        }
        let drain = b.notifications().begin_drain().expect("one drainer");
        let mut out = Vec::new();
        while let Some(r) = drain.pop() {
            out.push((r.collector, r.id));
        }
        out
    }
    assert_eq!(
        delivered(true),
        [
            (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 1),
            (JMX_ZGC_MAJOR_CYCLES_COLLECTOR, 1),
            (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 2),
            (JMX_ZGC_MAJOR_CYCLES_COLLECTOR, 2),
        ]
    );
    assert_eq!(
        delivered(false),
        [
            (JMX_ZGC_MAJOR_CYCLES_COLLECTOR, 1),
            (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 1),
            (JMX_ZGC_MAJOR_CYCLES_COLLECTOR, 2),
            (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 2),
        ]
    );
}

/// zs s2/ob: the three ZGC-only `ZServiceability` shapes (HotSpot 25.0.3,
/// `Zs2ObBeanSurface`) and their kill arms; G1 is untouched by both.
#[test]
fn zs2ob_zgc_pause_usage_pool_init_and_peaks_follow_hotspot() {
    let on = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    let off = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches::KILLED,
    );
    let raw = [
        JmxPoolSample {
            init: Some(0),
            used: 0,
            committed: 0,
            max: Some(1 << 28),
        },
        JmxPoolSample {
            init: Some(64 << 20),
            used: 5,
            committed: 5,
            max: Some(1 << 28),
        },
    ];
    // Pool init: young = MinHeapSize, old = 0 -- once the minimum is set.
    let init = |b: &BackendGcBeans| b.pools(&raw).iter().map(|p| p.init).collect::<Vec<_>>();
    assert_eq!(init(&on), [Some(0), Some(64 << 20)], "minimum not set yet");
    on.set_zgc_min_capacity(8 << 20);
    off.set_zgc_min_capacity(8 << 20);
    assert_eq!(init(&on), [Some(8 << 20), Some(0)]);
    assert_eq!(
        init(&off),
        [Some(0), Some(64 << 20)],
        "=0 keeps the sampler's split"
    );

    // Pause records carry all-zero usage; cycle records keep theirs.
    let before = backend_snap(&on, &[sample(900, 1000), sample(100, 100)]);
    on.note_collection(1_000);
    let after = backend_snap(&on, &[sample(0, 1000), sample(200, 200)]);
    let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
    let mut recs_off = recs.clone();
    on.unrecord_pause_usage(&mut recs);
    off.unrecord_pause_usage(&mut recs_off);
    assert_eq!(
        recs_off,
        gc_notifications_between(&before, &after, "System.gc()", 1)
    );
    for r in &recs {
        let zero = r
            .before
            .iter()
            .chain(&r.after)
            .all(|p| (p.init, p.used, p.committed, p.max) == (Some(0), 0, 0, Some(0)));
        assert_eq!(zero, r.action == GC_ACTION_PAUSE, "{}", r.collector);
        // zs s5/ob4: a pause record also gains the zero `Metaspace` key;
        // zt t1/ob: and the other four HotSpot non-heap pools' keys.
        let keys = 2 + if r.action == GC_ACTION_PAUSE {
            HOTSPOT_NON_HEAP_POOLS.len()
        } else {
            0
        };
        assert_eq!(r.before.len(), keys, "every pool keeps its key");
    }

    // Peaks: sampled at the edges when on, never when off.
    on.note_pool_peaks(&before.pools);
    on.note_pool_peaks(&after.pools);
    off.note_pool_peaks(&before.pools);
    assert_eq!(on.pool_peak(JMX_POOL_ZGC_YOUNG), Some((900, 1000)));
    assert_eq!(on.pool_peak(JMX_POOL_ZGC_OLD), Some((200, 200)));
    assert_eq!(off.pool_peak(JMX_POOL_ZGC_YOUNG), None);
    on.reset_pool_peak(JMX_POOL_ZGC_YOUNG, 7, 1000);
    assert_eq!(on.pool_peak(JMX_POOL_ZGC_YOUNG), Some((7, 1000)));

    // G1: every ZGC switch is off whatever is passed.
    let g1 = BackendGcBeans::with_switches(
        BackendBeanShape::G1,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    g1.set_zgc_min_capacity(8 << 20);
    let s = [sample(1, 2), sample(3, 4), sample(5, 6)];
    assert_eq!(g1.pools(&s)[0].init, s[0].init);
    g1.note_pool_peaks(&g1.pools(&s));
    assert_eq!(g1.pool_peak(JMX_POOL_G1_EDEN), None);
    let mut g1_recs = vec![rec(JMX_G1_YOUNG_COLLECTOR, 1)];
    g1_recs[0].action = GC_ACTION_PAUSE;
    let copy = g1_recs.clone();
    g1.unrecord_pause_usage(&mut g1_recs);
    assert_eq!(g1_recs, copy);
}

/// zs s4/ob3: a ZGC collection is booked on `ZGC Major Pauses` as one
/// pause per phase span (`Pause Mark Start` / `Pause Mark End` /
/// `Pause Relocate Start`; two when the mark start was its own pause),
/// the spans summing to the pause; the cycle bean counts it once. The
/// notifications follow: consecutive ids, contiguous spans, all before
/// the cycle's. A split that does not fit the pause, the `=0` kill and
/// G1 book one pause, as before.
/// zs s5/ob4: a pause record built before the `Metaspace` pool was
/// handed out (no sample, so `append_metaspace_usage` added no key) still
/// carries an all-zero `Metaspace` usage, as every HotSpot pause `GcInfo`
/// does -- without it the `GcInfoBuilder`, which names every pool, read
/// the JMM's undefined `-1` init / max (host `Zs4Ob3PauseShape`:
/// `pause_gcinfo_usage_zero=false`). Cycle records, the `=0` kills and G1
/// are unchanged.
#[test]
fn zs5ob4_unpublished_metaspace_pause_record_reads_zero() {
    let mk = |sw: ZgcBeanShapeSwitches, shape: BackendBeanShape| {
        BackendGcBeans::with_switches(shape, true, sw)
    };
    let on = mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::Zgc);
    let s = [sample(10, 20), sample(30, 40)];
    let before = backend_snap(&on, &s);
    on.note_collection(1_000);
    let after = backend_snap(&on, &s);
    let fresh = || gc_notifications_between(&before, &after, "System.gc()", 1);
    assert!(!on.metaspace_samples_wanted(), "never handed out");
    let mut recs = fresh();
    on.append_metaspace_usage(&mut recs, None, None);
    on.unrecord_pause_usage(&mut recs);
    let meta = |r: &GcNotificationRecord| {
        [&r.before, &r.after].map(|side| {
            side.iter()
                .filter(|p| p.name == JMX_POOL_METASPACE)
                .map(|p| (p.init, p.used, p.committed, p.max))
                .collect::<Vec<_>>()
        })
    };
    for r in &recs {
        let want: Vec<(Option<u64>, u64, u64, Option<u64>)> = if r.action == GC_ACTION_PAUSE {
            vec![(Some(0), 0, 0, Some(0))]
        } else {
            vec![]
        };
        assert_eq!(meta(r), [want.clone(), want], "{}", r.collector);
    }
    // Published and sampled: still exactly one key, zero on the pause.
    on.note_metaspace_published();
    let mut recs = fresh();
    on.append_metaspace_usage(&mut recs, Some(500), Some(600));
    on.unrecord_pause_usage(&mut recs);
    let pause = recs.iter().find(|r| r.action == GC_ACTION_PAUSE).unwrap();
    assert_eq!(
        meta(pause),
        [
            vec![(Some(0), 0, 0, Some(0))],
            vec![(Some(0), 0, 0, Some(0))]
        ]
    );
    // The kills and G1 add nothing.
    for b in [
        mk(
            ZgcBeanShapeSwitches {
                pause_usage_unrecorded: false,
                ..ZgcBeanShapeSwitches::DEFAULT
            },
            BackendBeanShape::Zgc,
        ),
        // zt t1/ob: the HotSpot non-heap set zeroes `Metaspace` on its
        // own, so this kill arm turns it off too.
        mk(
            ZgcBeanShapeSwitches {
                metaspace_pool: false,
                hotspot_non_heap_pools: false,
                ..ZgcBeanShapeSwitches::DEFAULT
            },
            BackendBeanShape::Zgc,
        ),
        mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::G1),
    ] {
        let mut recs = fresh();
        b.unrecord_pause_usage(&mut recs);
        assert!(recs.iter().all(|r| meta(r) == [vec![], vec![]]));
    }
}

/// zt t1/ob (the merge decision): with HotSpot's five non-heap pools
/// listed on ZGC, a pause record carries an all-zero key for every one
/// of them (HotSpot 25: every pause `GcInfo` entry is zero; a missing key
/// was rendered from the pool's usage at delivery), also with the
/// `Metaspace` edge samples killed; a cycle record's `Metaspace` key
/// carries the walk's committed figure and is joined by the walk's
/// `Compressed Class Space` figures. Kill arm (`=0`): the zs s3/ob2 shape
/// (zero `Metaspace` only, `committed = used`, no class space key; zt
/// t2/x1: the t1 cut kept the walk's committed here). G1: nothing.
#[test]
fn zt1ob_zgc_gcinfo_carries_hotspots_non_heap_pool_set() {
    let mk = |sw: ZgcBeanShapeSwitches, shape: BackendBeanShape| {
        BackendGcBeans::with_switches(shape, true, sw)
    };
    let on = mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::Zgc);
    let kill = mk(
        ZgcBeanShapeSwitches {
            hotspot_non_heap_pools: false,
            ..ZgcBeanShapeSwitches::DEFAULT
        },
        BackendBeanShape::Zgc,
    );
    let no_edges = mk(
        ZgcBeanShapeSwitches {
            metaspace_pool: false,
            ..ZgcBeanShapeSwitches::DEFAULT
        },
        BackendBeanShape::Zgc,
    );
    let g1 = mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::G1);
    assert!(on.hotspot_non_heap_pools_enabled());
    assert!(!kill.hotspot_non_heap_pools_enabled(), "=0 kill");
    assert!(!g1.hotspot_non_heap_pools_enabled(), "G1 never");
    let s = [sample(10, 20), sample(30, 40)];
    let names = |side: &[JmxPoolUsage]| {
        let mut n: Vec<&str> = side.iter().map(|p| p.name).collect();
        n.sort_unstable();
        n
    };
    let mut want: Vec<&str> = HOTSPOT_NON_HEAP_POOLS.iter().map(|&(n, _)| n).collect();
    want.extend([JMX_POOL_ZGC_YOUNG, JMX_POOL_ZGC_OLD]);
    want.sort_unstable();
    // The edge walks state HotSpot's figures.
    let figures = |used: u64| MetaspaceSpaceFigures {
        used,
        committed: used + 4_096,
        class_space: Some((used / 10, 65_536, 1 << 30)),
    };
    for b in [&on, &kill, &no_edges] {
        b.note_metaspace_published();
        assert_eq!(b.metaspace_sample_figures((5, 0), || figures(500)), 500);
        assert_eq!(b.metaspace_sample_figures((5, 0), || figures(999)), 500);
        assert_eq!(b.metaspace_sample_figures((6, 0), || figures(600)), 600);
    }
    let collect = |b: &BackendGcBeans| {
        let before = backend_snap(b, &s);
        b.note_collection(1_000);
        let after = backend_snap(b, &s);
        let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
        b.append_metaspace_usage(&mut recs, Some(500), Some(600));
        b.unrecord_pause_usage(&mut recs);
        recs
    };
    let quad = |p: &JmxPoolUsage| (p.init, p.used, p.committed, p.max);
    let find = |side: &[JmxPoolUsage], name: &str| {
        side.iter().find(|p| p.name == name).map(quad)
    };
    for r in collect(&on) {
        if r.action == GC_ACTION_PAUSE {
            for side in [&r.before, &r.after] {
                assert_eq!(names(side), want, "every pool has a key");
                assert!(side.iter().all(|p| quad(p) == (Some(0), 0, 0, Some(0))));
            }
        } else {
            assert_eq!(
                find(&r.before, JMX_POOL_METASPACE),
                Some((Some(0), 500, 4_596, None)),
                "the walk's committed"
            );
            assert_eq!(
                find(&r.after, JMX_POOL_COMPRESSED_CLASS_SPACE),
                Some((Some(0), 60, 65_536, Some(1 << 30)))
            );
            assert_eq!(r.before.len(), 4, "two heap pools, Metaspace, class space");
        }
    }
    // Pause zeros even without the `Metaspace` edge samples.
    for r in collect(&no_edges) {
        if r.action == GC_ACTION_PAUSE {
            assert_eq!(names(&r.before), want);
        } else {
            assert_eq!(r.before.len(), 2, "no edge samples");
        }
    }
    // Positive control: the kill arm's pause names Metaspace only, and
    // its cycle record has no class-space key.
    for r in collect(&kill) {
        assert_eq!(find(&r.before, JMX_POOL_COMPRESSED_CLASS_SPACE), None);
        assert_eq!(r.before.len(), 3, "{}", r.collector);
        // zt t2/x1: and zs s3/ob2's `committed = used`, though the edge
        // walk stated a larger committed figure.
        if r.action != GC_ACTION_PAUSE {
            assert_eq!(
                find(&r.before, JMX_POOL_METASPACE),
                Some((Some(0), 500, 500, None)),
                "kill arm: committed = used"
            );
        }
    }
    assert_eq!(quad(&kill.metaspace_pool_usage(500)), (Some(0), 500, 500, None));
    // G1: a pause record is left as it was.
    let mut g1_recs = vec![rec(JMX_G1_YOUNG_COLLECTOR, 1)];
    g1_recs[0].action = GC_ACTION_PAUSE;
    let copy = g1_recs.clone();
    g1.unrecord_pause_usage(&mut g1_recs);
    assert_eq!(g1_recs, copy);
    // A figure no walk stated reads `committed = used`.
    assert_eq!(quad(&on.metaspace_pool_usage(7)), (Some(0), 7, 7, None));
    assert_eq!(on.class_space_pool_usage(7), None);
}

/// zt t3/ob (`zgc-zt1ob-non-heap-pool-residuals` item 4): a back-filled
/// first-cycle record carries its prefix walk's `committed` and, with
/// HotSpot's non-heap pools listed, its `Compressed Class Space` entry.
/// Kill arm (`CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS=0`): `committed =
/// used`, no class-space entry, the zs s6/ob5 shape.
#[test]
fn zt3ob_a_backfilled_record_carries_the_prefix_figures() {
    let killed = ZgcBeanShapeSwitches {
        hotspot_non_heap_pools: false,
        ..ZgcBeanShapeSwitches::DEFAULT
    };
    for (sw, committed, ccs) in [
        (ZgcBeanShapeSwitches::DEFAULT, 65_536, true),
        (killed, 500, false),
    ] {
        let b = BackendGcBeans::with_switches(BackendBeanShape::Zgc, true, sw);
        let s = [sample(10, 20), sample(30, 40)];
        let before = backend_snap(&b, &s);
        b.note_collection(1_000);
        let after = backend_snap(&b, &s);
        let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
        b.append_metaspace_usage(&mut recs, None, None);
        b.unrecord_pause_usage(&mut recs);
        for r in &recs {
            b.notifications().record(r.clone());
        }
        b.note_metaspace_deferred(&recs, Some(7), Some(7));
        let f = MetaspaceSpaceFigures {
            used: 500,
            committed: 65_536,
            class_space: Some((50, 65_536, 1 << 30)),
        };
        assert_eq!(b.backfill_metaspace_figures(|_| Some(f)), 1);
        let cycle = recs
            .iter()
            .find(|r| r.action != GC_ACTION_PAUSE)
            .unwrap()
            .collector;
        let last = b.notifications().last(cycle).unwrap();
        let ms = last
            .before
            .iter()
            .find(|p| p.name == JMX_POOL_METASPACE)
            .unwrap();
        assert_eq!((ms.used, ms.committed), (500, committed));
        let has_ccs = last
            .after
            .iter()
            .any(|p| p.name == JMX_POOL_COMPRESSED_CLASS_SPACE);
        assert_eq!(has_ccs, ccs);
    }
}

/// zt t3/ob: HotSpot's code-heap `init` is stated on ZGC with the five
/// non-heap pools listed; the kill, the zs s3/ob2 list and G1 state none.
#[test]
fn zt3ob_code_heap_init_is_stated_on_zgc_only() {
    let mk = |sw: ZgcBeanShapeSwitches, shape: BackendBeanShape| {
        BackendGcBeans::with_switches(shape, true, sw)
    };
    let on = mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::Zgc);
    assert_eq!(on.hotspot_code_heap_init_bytes(), Some(2_555_904));
    for off in [
        mk(
            ZgcBeanShapeSwitches {
                hotspot_code_heap_init: false,
                ..ZgcBeanShapeSwitches::DEFAULT
            },
            BackendBeanShape::Zgc,
        ),
        // The zs s3/ob2 list has no code heaps to state it for.
        mk(
            ZgcBeanShapeSwitches {
                hotspot_non_heap_pools: false,
                ..ZgcBeanShapeSwitches::DEFAULT
            },
            BackendBeanShape::Zgc,
        ),
        mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::G1),
    ] {
        assert_eq!(off.hotspot_code_heap_init_bytes(), None);
    }
}

/// zt t6/ob4 (`zgc-zt1ob-non-heap-pool-residuals` item 3): a ZGC cycle
/// record carries the three code heaps sampled at its two edges (before
/// != after when code was installed during the collection), in the pool
/// beans' shape; its pause records stay all zero. Kill arm
/// (`CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES=0`) and the zs s3/ob2 list carry
/// no code-heap key (rendered at delivery, as before); G1 neither. The
/// start sample is taken once.
#[test]
fn zt6ob4_cycle_records_carry_code_heap_edge_samples() {
    const NP: &str = "CodeHeap 'non-profiled nmethods'";
    let mk = |sw: ZgcBeanShapeSwitches, shape: BackendBeanShape| {
        BackendGcBeans::with_switches(shape, true, sw)
    };
    let on = mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::Zgc);
    let quad = |p: &JmxPoolUsage| (p.init, p.used, p.committed, p.max);
    let find = |side: &[JmxPoolUsage], name: &str| {
        side.iter().find(|p| p.name == name).map(quad)
    };
    let collect = |b: &BackendGcBeans| {
        let s = [sample(10, 20), sample(30, 40)];
        let before = backend_snap(b, &s);
        b.note_collection(1_000);
        let after = backend_snap(b, &s);
        let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
        b.note_code_heap_before(Some(4_000));
        let code_before = b.take_code_heap_before();
        b.append_code_heap_usage(&mut recs, code_before, Some(9_000), Some(1 << 30));
        b.unrecord_pause_usage(&mut recs);
        recs
    };
    assert!(on.code_heap_edges_wanted());
    let recs = collect(&on);
    assert!(recs.iter().any(|r| r.action != GC_ACTION_PAUSE));
    for r in &recs {
        if r.action == GC_ACTION_PAUSE {
            assert_eq!(find(&r.before, NP), Some((Some(0), 0, 0, Some(0))));
            continue;
        }
        assert_eq!(
            find(&r.before, NP),
            Some((Some(2_555_904), 4_000, 4_000, Some(1 << 30))),
            "the start edge's sample"
        );
        assert_eq!(
            find(&r.after, NP),
            Some((Some(2_555_904), 9_000, 9_000, Some(1 << 30))),
            "the end edge's sample"
        );
        assert_eq!(
            find(&r.after, "CodeHeap 'non-nmethods'"),
            Some((Some(2_555_904), 0, 0, None))
        );
        assert_eq!(
            find(&r.before, "CodeHeap 'profiled nmethods'"),
            Some((Some(2_555_904), 0, 0, None))
        );
        assert_eq!(r.before.len(), 5, "two heap pools, three code heaps");
    }
    assert_eq!(on.take_code_heap_before(), None, "taken once");
    // The gci i15/nh init under the init kill.
    let no_init = mk(
        ZgcBeanShapeSwitches {
            hotspot_code_heap_init: false,
            ..ZgcBeanShapeSwitches::DEFAULT
        },
        BackendBeanShape::Zgc,
    );
    let u = no_init.code_heap_pool_usages(7, None);
    assert_eq!(
        u.iter().map(quad).collect::<Vec<_>>(),
        vec![
            (Some(0), 0, 0, None),
            (Some(0), 0, 0, None),
            (Some(2 * 1024 * 1024), 7, 7, None),
        ]
    );
    // Kill arm and the zs s3/ob2 list: no code-heap key on a cycle.
    for off in [
        mk(
            ZgcBeanShapeSwitches {
                code_heap_edges: false,
                ..ZgcBeanShapeSwitches::DEFAULT
            },
            BackendBeanShape::Zgc,
        ),
        mk(
            ZgcBeanShapeSwitches {
                hotspot_non_heap_pools: false,
                ..ZgcBeanShapeSwitches::DEFAULT
            },
            BackendBeanShape::Zgc,
        ),
    ] {
        assert!(!off.code_heap_edges_wanted());
        for r in collect(&off).iter().filter(|r| r.action != GC_ACTION_PAUSE) {
            assert_eq!(find(&r.before, NP), None);
        }
    }
    // G1 never samples them.
    assert!(!mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::G1).code_heap_edges_wanted());
}

/// zt t3/ob (`zgc-zt2x1-wave-t1-review-residuals` item 7): two walks
/// with the same `used` and different `committed` (a class unloaded and
/// another of the same footprint loaded) never answer one another's
/// figures: the newer replaces the older, and a memo hit needs the
/// figures of its own census. Positive control: the census whose figures
/// were replaced walks again.
#[test]
fn zt3ob_remembered_metaspace_figures_are_keyed_by_census() {
    let on = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    let f = |used: u64, committed: u64| MetaspaceSpaceFigures {
        used,
        committed,
        class_space: Some((used / 10, committed / 10, 1 << 30)),
    };
    let walks = std::cell::Cell::new(0);
    let walk = |x: MetaspaceSpaceFigures| {
        walks.set(walks.get() + 1);
        x
    };
    assert_eq!(on.metaspace_sample_figures((10, 0), || walk(f(500, 65_536))), 500);
    assert_eq!(on.metaspace_sample_figures((11, 1), || walk(f(500, 131_072))), 500);
    assert_eq!(walks.get(), 2);
    assert_eq!(on.metaspace_pool_usage(500).committed, 131_072, "the newer walk");
    assert_eq!(on.metaspace_figures_for_key((10, 0)), None, "the older one is gone");
    assert_eq!(
        on.metaspace_figures_for_key((11, 1)).map(|x| x.committed),
        Some(131_072)
    );
    // The memo is (11, 1): a hit, no walk.
    assert_eq!(on.metaspace_sample_figures((11, 1), || walk(f(1, 1))), 500);
    assert_eq!(walks.get(), 2);
    // Census (10, 0) again: its figures were replaced, so it walks.
    assert_eq!(on.metaspace_sample_figures((10, 0), || walk(f(500, 65_536))), 500);
    assert_eq!(walks.get(), 3, "positive control");
    assert_eq!(on.metaspace_pool_usage(500).committed, 65_536);
    // Distinct `used`: both kept.
    assert_eq!(on.metaspace_sample_figures((12, 1), || walk(f(600, 196_608))), 600);
    assert_eq!(on.metaspace_pool_usage(500).committed, 65_536);
    assert_eq!(on.metaspace_pool_usage(600).committed, 196_608);
}

/// zs s9/ob6: while a census entry waits (the drain's back-fill found
/// the class lock busy), the keyless cycle record at the head of the
/// queue is held back with everything behind it; a pause record, a
/// record that has its key, or one no entry names is delivered. After
/// the back-fill the held record goes out with its key. Positive control:
/// plain `pop` would have delivered it keyless.
#[test]
fn zs9ob6_a_keyless_cycle_record_is_held_until_its_backfill() {
    let b = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    let s = [sample(10, 20), sample(30, 40)];
    b.notifications()
        .set_enabled(JMX_ZGC_MAJOR_CYCLES_COLLECTOR, true);
    b.notifications()
        .set_enabled(JMX_ZGC_MAJOR_PAUSES_COLLECTOR, true);
    let before = backend_snap(&b, &s);
    b.note_collection(1_000);
    let after = backend_snap(&b, &s);
    let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
    b.order_notifications(&mut recs);
    b.append_metaspace_usage(&mut recs, None, None);
    b.unrecord_pause_usage(&mut recs);
    for r in &recs {
        b.notifications().record(r.clone());
    }
    b.note_metaspace_deferred(&recs, Some(7), Some(7));
    let has_key =
        |r: &GcNotificationRecord| r.before.iter().any(|p| p.name == JMX_POOL_METASPACE);
    let drain = b.notifications().begin_drain().expect("one drainer");
    // The pause record (first, HotSpot order) goes; the keyless cycle
    // record is held.
    let first = drain
        .pop_unless(|r| b.metaspace_record_waiting(r))
        .expect("pause");
    assert_eq!(first.action, GC_ACTION_PAUSE);
    assert!(
        drain
            .pop_unless(|r| b.metaspace_record_waiting(r))
            .is_none(),
        "held"
    );
    assert!(b.notifications().has_pending(), "still queued");
    // The back-fill gives it its key; it is then delivered.
    assert_eq!(b.backfill_metaspace(|cut| Some(u64::from(cut) * 10)), 2);
    let cycle = drain
        .pop_unless(|r| b.metaspace_record_waiting(r))
        .expect("released");
    assert!(cycle.action != GC_ACTION_PAUSE && has_key(&cycle));
    assert!(!b.metaspace_record_waiting(&cycle));
    drop(drain);
    // Positive control: without the hold, `pop` hands out the keyless
    // record.
    let before = backend_snap(&b, &s);
    b.note_collection(1_000);
    let after = backend_snap(&b, &s);
    let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
    b.order_notifications(&mut recs);
    for r in &recs {
        b.notifications().record(r.clone());
    }
    b.note_metaspace_deferred(&recs, Some(8), Some(8));
    let drain = b.notifications().begin_drain().expect("one drainer");
    let keyless: Vec<_> = std::iter::from_fn(|| drain.pop())
        .filter(|r| r.action != GC_ACTION_PAUSE)
        .collect();
    assert_eq!(keyless.len(), 1);
    assert!(!has_key(&keyless[0]), "the defect the hold prevents");
}

/// zs s6/ob5: a cycle record built before the `Metaspace` pool was
/// handed out keeps its edges' class-store census and gains the measured
/// key at the back-fill (the first `GcInfoBuilder`), from the figure of
/// the classes below each cut; the pause record keeps its zero key; the
/// census is consumed; a bean with no listener keeps one entry. After
/// publication, under the `=0` kill and on G1 nothing is kept. Also the
/// `Metaspace` edge peaks and the threshold switch.
#[test]
fn zs6ob5_first_cycle_metaspace_key_is_backfilled_from_its_census() {
    let mk = |sw: ZgcBeanShapeSwitches, shape: BackendBeanShape| {
        BackendGcBeans::with_switches(shape, true, sw)
    };
    let on = mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::Zgc);
    let s = [sample(10, 20), sample(30, 40)];
    let collect = |b: &BackendGcBeans, cut: Option<u32>| {
        let before = backend_snap(b, &s);
        b.note_collection(1_000);
        let after = backend_snap(b, &s);
        let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
        b.append_metaspace_usage(&mut recs, None, None);
        b.unrecord_pause_usage(&mut recs);
        for r in &recs {
            b.notifications().record(r.clone());
        }
        b.note_metaspace_deferred(&recs, cut, cut);
        recs
    };
    let meta = |r: &GcNotificationRecord| {
        [&r.before, &r.after].map(|side| {
            side.iter()
                .filter(|p| p.name == JMX_POOL_METASPACE)
                .map(|p| (p.init, p.used, p.committed, p.max))
                .collect::<Vec<_>>()
        })
    };
    assert!(on.metaspace_census_wanted());
    let recs = collect(&on, Some(10));
    let cycle = recs
        .iter()
        .find(|r| r.action != GC_ACTION_PAUSE)
        .unwrap()
        .collector;
    let pause = recs
        .iter()
        .find(|r| r.action == GC_ACTION_PAUSE)
        .unwrap()
        .collector;
    // No listener: the second collection's entry replaces the first's.
    collect(&on, Some(12));
    assert_eq!(on.metaspace_deferred_cuts(), vec![12]);
    assert_eq!(
        meta(&on.notifications().last(cycle).unwrap()),
        [vec![], vec![]]
    );
    let patched = on.backfill_metaspace(|cut| Some(u64::from(cut) * 100));
    assert_eq!(patched, 1);
    assert_eq!(
        meta(&on.notifications().last(cycle).unwrap()),
        [
            vec![(Some(0), 1200, 1200, None)],
            vec![(Some(0), 1200, 1200, None)]
        ]
    );
    let zero = vec![(Some(0), 0, 0, Some(0))];
    assert_eq!(
        meta(&on.notifications().last(pause).unwrap()),
        [zero.clone(), zero]
    );
    assert!(on.metaspace_deferred_cuts().is_empty(), "consumed");
    assert_eq!(on.backfill_metaspace(|_| Some(1)), 0);
    // A cut with no figure leaves the record as it was.
    collect(&on, Some(14));
    assert_eq!(on.backfill_metaspace(|_| None), 0);
    assert_eq!(
        meta(&on.notifications().last(cycle).unwrap()),
        [vec![], vec![]]
    );
    // Published: the edges sample instead; nothing is kept.
    on.note_metaspace_published();
    assert!(!on.metaspace_census_wanted());
    collect(&on, Some(16));
    assert!(on.metaspace_deferred_cuts().is_empty());
    // The kill and G1 keep nothing.
    for b in [
        mk(
            ZgcBeanShapeSwitches {
                metaspace_first_cycles: false,
                ..ZgcBeanShapeSwitches::DEFAULT
            },
            BackendBeanShape::Zgc,
        ),
        mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::G1),
    ] {
        assert!(!b.metaspace_census_wanted());
        collect(&b, Some(10));
        assert!(b.metaspace_deferred_cuts().is_empty());
        assert_eq!(b.backfill_metaspace(|_| Some(1)), 0);
    }
    // Edge peaks: the larger edge, both fields; the kill samples none.
    on.note_metaspace_peaks(Some(700), Some(650));
    assert_eq!(on.pool_peak(JMX_POOL_METASPACE), Some((700, 700)));
    let no_peak = mk(
        ZgcBeanShapeSwitches {
            metaspace_peak_at_gc: false,
            ..ZgcBeanShapeSwitches::DEFAULT
        },
        BackendBeanShape::Zgc,
    );
    no_peak.note_metaspace_peaks(Some(700), Some(650));
    assert_eq!(no_peak.pool_peak(JMX_POOL_METASPACE), None);
    // The threshold switch.
    assert!(on.metaspace_threshold_enabled());
    assert!(!mk(
        ZgcBeanShapeSwitches {
            metaspace_threshold: false,
            ..ZgcBeanShapeSwitches::DEFAULT
        },
        BackendBeanShape::Zgc
    )
    .metaspace_threshold_enabled());
    assert!(
        !mk(ZgcBeanShapeSwitches::DEFAULT, BackendBeanShape::G1).metaspace_threshold_enabled()
    );
}

/// zt t31/zc11: a collection that closes a concurrent cycle credits the
/// cycle bean the cycle's whole span (from its mark start) and the pause
/// bean its own pauses; no cycle start, or one after the pause, is the
/// pause alone. The cycle state hands the start out once.
#[test]
fn zt31zc11_a_closed_cycle_books_the_cycle_bean_from_its_start() {
    use std::time::Duration;
    let us = Duration::from_micros;
    let on = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    let s = [sample(10, 20), sample(30, 40)];
    let before = backend_snap(&on, &s);
    let opened = before.at;
    let t0 = opened + us(5_000);
    let end = t0 + us(1_000);
    let (ms, me) = (t0 + us(100), t0 + us(400));
    let three = ZgcPauseSplit::new(Some(ms), me);
    on.note_collection_split_from(Some(opened), t0, end, CollectionKind::Major, Some(three));
    let load = |b: &BackendGcBeans, i: usize| {
        (
            b.counts[i].load(Ordering::Relaxed),
            b.pause_ns[i].load(Ordering::Relaxed),
        )
    };
    assert_eq!(load(&on, 2), (1, 6_000_000), "the cycle: from its start");
    assert_eq!(load(&on, 3), (3, 1_000_000), "the pauses: this collection's");
    // No start, or a start after the pause: the pause alone.
    on.note_collection_split_from(None, t0, end, CollectionKind::Major, None);
    on.note_collection_split_from(Some(end), t0, end, CollectionKind::Major, None);
    assert_eq!(load(&on, 2), (3, 8_000_000));
    let cycle = ConcurrentCycleJfr::default();
    assert_eq!(cycle.take_cycle_open_at(), None);
    cycle.note_cycle_open_at(opened);
    assert_eq!(cycle.take_cycle_open_at(), Some(opened));
    assert_eq!(cycle.take_cycle_open_at(), None, "taken once");
    // zt t32: the cause lives as long as the open cycle.
    cycle.note_cycle_open_at(opened);
    cycle.note_cycle_cause("System.gc()");
    assert_eq!(cycle.open_cycle_cause(), Some("System.gc()"));
    let read = cycle.open_cycle_cause();
    assert_eq!(read, Some("System.gc()"), "read, not taken");
    assert_eq!(cycle.take_cycle_open_at(), Some(opened));
    let closed = cycle.open_cycle_cause();
    assert_eq!(closed, None, "the closing collection ends it");
    assert_eq!(cycle.requested_cause(), None);
    cycle.set_requested_cause(Some("System.gc()"));
    assert_eq!(cycle.requested_cause(), Some("System.gc()"));
    cycle.set_requested_cause(None);
    assert_eq!(cycle.requested_cause(), None);
}

#[test]
fn zs4ob3_zgc_collection_books_one_pause_per_phase_span() {
    use std::time::Duration;
    let us = Duration::from_micros;
    let on = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    let s = [sample(10, 20), sample(30, 40)];
    let before = backend_snap(&on, &s);
    let t0 = before.at + us(10);
    let (ms, me, end) = (t0 + us(100), t0 + us(400), t0 + us(1_000));
    let three = ZgcPauseSplit::new(Some(ms), me);
    assert_eq!((three.pauses(), three.bounds()), (3, &[ms, me][..]));
    assert_eq!(
        on.note_collection_split(t0, end, CollectionKind::Major, Some(three)),
        Some(three)
    );
    let load = |b: &BackendGcBeans, i: usize| {
        (
            b.counts[i].load(Ordering::Relaxed),
            b.pause_ns[i].load(Ordering::Relaxed),
        )
    };
    assert_eq!(load(&on, 2), (1, 1_000_000), "the cycle: once, whole");
    assert_eq!(load(&on, 3), (3, 1_000_000), "three pauses, same sum");
    assert_eq!(load(&on, 0), (0, 0));
    let mut after = backend_snap(&on, &s);
    after.at = end + us(10);
    let mut recs = gc_notifications_between(&before, &after, "System.gc()", 1);
    on.split_pause_records(&mut recs, &three);
    on.order_notifications(&mut recs);
    let shape: Vec<_> = recs
        .iter()
        .map(|r| (r.collector, r.id, r.start, r.end))
        .collect();
    assert_eq!(
        shape,
        [
            (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 1, before.at, ms),
            (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 2, ms, me),
            (JMX_ZGC_MAJOR_PAUSES_COLLECTOR, 3, me, after.at),
            (JMX_ZGC_MAJOR_CYCLES_COLLECTOR, 1, before.at, after.at),
        ]
    );

    // A cycle whose mark start was its own pause: two more.
    let before = after;
    let t0 = before.at + us(10);
    let (me, end) = (t0 + us(300), t0 + us(500));
    let two = ZgcPauseSplit::new(None, me);
    assert_eq!(two.pauses(), 2);
    assert!(on
        .note_collection_split(t0, end, CollectionKind::Major, Some(two))
        .is_some());
    assert_eq!(load(&on, 3), (5, 1_500_000));
    let mut after = backend_snap(&on, &s);
    after.at = end + us(10);
    let mut recs = gc_notifications_between(&before, &after, "Allocation Rate", 2);
    on.split_pause_records(&mut recs, &two);
    on.order_notifications(&mut recs);
    let ids: Vec<_> = recs.iter().map(|r| (r.action, r.id)).collect();
    assert_eq!(
        ids,
        [
            (GC_ACTION_PAUSE, 4),
            (GC_ACTION_PAUSE, 5),
            (GC_ACTION_CYCLE, 2)
        ]
    );
    assert_eq!((recs[0].end, recs[1].start), (me, me));

    // A boundary outside the pause (left by another collection): one.
    let stale = ZgcPauseSplit::new(Some(t0 - us(5)), me);
    assert!(!stale.fits(t0, end));
    assert_eq!(
        on.note_collection_split(t0, end, CollectionKind::Major, Some(stale)),
        None
    );
    assert_eq!(load(&on, 3), (6, 2_000_000));
    // A young cycle splits on the Minor pair.
    assert!(on
        .note_collection_split(t0, end, CollectionKind::Minor, Some(two))
        .is_some());
    assert_eq!((load(&on, 0).0, load(&on, 1).0), (1, 2));

    // `=0`: one pause per collection, whatever the heap measured.
    let off = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches {
            pause_phases: false,
            ..ZgcBeanShapeSwitches::DEFAULT
        },
    );
    assert!(!off.pause_phases_enabled());
    assert_eq!(
        off.note_collection_split(t0, end, CollectionKind::Major, Some(three)),
        None
    );
    assert_eq!(load(&off, 3), (1, 500_000));
    // G1: the switch is off whatever is passed; one young-bean pause.
    let g1 = BackendGcBeans::with_switches(
        BackendBeanShape::G1,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    assert!(!g1.pause_phases_enabled());
    assert_eq!(
        g1.note_collection_split(t0, end, CollectionKind::Major, Some(two)),
        None
    );
    assert_eq!(load(&g1, 0), (1, 500_000));
    assert_eq!((load(&g1, 1).0, load(&g1, 2).0), (0, 0));

    // Clamping keeps the span count when a record's edges are inside a
    // boundary (never on the booked path, which checks `fits`).
    let (spans, n) = three.spans(ms + us(1), ms + us(2));
    assert_eq!(n, 3);
    assert_eq!(spans[2].1, ms + us(2));
    assert!(spans[..n].windows(2).all(|w| w[0].1 == w[1].0));
}

/// zs s3/ob2: the ZGC `Metaspace` pool (HotSpot 25.0.3 `Ob2Pools`:
/// `init = 0`, `max = -1` or `-XX:MaxMetaspaceSize`, managed by
/// `Metaspace Manager`; a `ZGC Major Cycles` GcInfo carries its edge
/// figures, a `ZGC Major Pauses` one zeros), the walk memo, the
/// published gate, the `=0` kill and the G1 no-op.
#[test]
fn zs3ob2_zgc_metaspace_pool_follows_hotspot() {
    let on = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    let off = BackendGcBeans::with_switches(
        BackendBeanShape::Zgc,
        true,
        ZgcBeanShapeSwitches {
            metaspace_pool: false,
            ..ZgcBeanShapeSwitches::DEFAULT
        },
    );
    let g1 = BackendGcBeans::with_switches(
        BackendBeanShape::G1,
        true,
        ZgcBeanShapeSwitches::DEFAULT,
    );
    assert!(on.metaspace_pool_enabled());
    assert!(!off.metaspace_pool_enabled(), "=0 kill");
    assert!(!g1.metaspace_pool_enabled(), "G1 never");

    // The pool's shape.
    let p = on.metaspace_pool_usage(1000);
    assert_eq!(
        (
            p.name,
            p.init,
            p.used,
            p.committed,
            p.max,
            p.collection_used
        ),
        (JMX_POOL_METASPACE, Some(0), 1000, 1000, None, None)
    );
    assert_eq!(p.managers, &[JMX_METASPACE_MANAGER]);
    on.set_metaspace_max(Some(64 << 20));
    assert_eq!(on.metaspace_pool_usage(1000).max, Some(64 << 20));
    assert_eq!(
        on.metaspace_pool_usage((64 << 20) + 1).max,
        Some((64 << 20) + 1),
        "MemoryUsage's used <= max invariant holds"
    );
    on.set_metaspace_max(None);

    // The memo: one walk per class census.
    let walks = std::cell::Cell::new(0);
    let walk = |v: u64| {
        walks.set(walks.get() + 1);
        v
    };
    assert_eq!(on.metaspace_last(), None);
    assert_eq!(on.metaspace_sample((10, 0), || walk(500)), 500);
    assert_eq!(on.metaspace_sample((10, 0), || walk(999)), 500);
    assert_eq!(walks.get(), 1, "same census, no walk");
    assert_eq!(on.metaspace_sample((11, 0), || walk(600)), 600);
    assert_eq!(on.metaspace_sample((11, 1), || walk(550)), 550);
    assert_eq!(walks.get(), 3);
    assert_eq!(on.metaspace_last(), Some(550));
    assert_eq!(on.metaspace_memo_for((11, 1)), Some(550));
    assert_eq!(on.metaspace_memo_for((12, 1)), None, "no walk, no figure");
    assert_eq!(walks.get(), 3);

    // Records: nothing until the pool is handed out.
    let before = backend_snap(&on, &[sample(900, 1000), sample(100, 100)]);
    on.note_collection(1_000);
    let after = backend_snap(&on, &[sample(0, 1000), sample(200, 200)]);
    let base = gc_notifications_between(&before, &after, "System.gc()", 1);
    let mut recs = base.clone();
    assert!(!on.metaspace_samples_wanted());
    on.append_metaspace_usage(&mut recs, Some(500), Some(600));
    assert_eq!(recs, base, "not published: unchanged");
    on.note_metaspace_published();
    off.note_metaspace_published();
    g1.note_metaspace_published();
    assert!(on.metaspace_samples_wanted());
    assert!(!off.metaspace_samples_wanted() && !g1.metaspace_samples_wanted());

    on.append_metaspace_usage(&mut recs, Some(500), Some(600));
    on.append_metaspace_usage(&mut recs, Some(1), Some(2));
    on.unrecord_pause_usage(&mut recs);
    assert_eq!(recs.len(), 2);
    for r in &recs {
        let find = |v: &[JmxPoolUsage]| {
            let m: Vec<_> = v.iter().filter(|p| p.name == JMX_POOL_METASPACE).collect();
            assert_eq!(m.len(), 1, "one key, appended once");
            (m[0].init, m[0].used, m[0].committed, m[0].max)
        };
        if r.action == GC_ACTION_PAUSE {
            assert_eq!(find(&r.before), (Some(0), 0, 0, Some(0)));
            assert_eq!(find(&r.after), (Some(0), 0, 0, Some(0)));
        } else {
            assert_eq!(find(&r.before), (Some(0), 500, 500, None));
            assert_eq!(find(&r.after), (Some(0), 600, 600, None));
        }
        // zt t1/ob: a pause record also names HotSpot's other four
        // non-heap pools; a cycle record no class space (the used-only
        // walks above stated none).
        let keys = if r.action == GC_ACTION_PAUSE {
            2 + HOTSPOT_NON_HEAP_POOLS.len()
        } else {
            3
        };
        assert_eq!(r.before.len(), keys);
    }

    // One edge missing: the other stands in; neither: pause records only.
    let mut one = base.clone();
    on.append_metaspace_usage(&mut one, None, Some(600));
    assert!(one.iter().all(|r| r.before.len() == 3));
    let mut none = base.clone();
    on.append_metaspace_usage(&mut none, None, None);
    for r in &none {
        assert_eq!(r.before.len(), 2 + usize::from(r.action == GC_ACTION_PAUSE));
    }

    // The before-edge stash is taken once.
    on.note_metaspace_before(Some(42));
    assert_eq!(on.take_metaspace_before(), Some(42));
    assert_eq!(on.take_metaspace_before(), None);

    // Kill and G1: no-ops.
    for b in [&off, &g1] {
        let mut r = base.clone();
        b.append_metaspace_usage(&mut r, Some(500), Some(600));
        assert_eq!(r, base);
    }
}

/// W7F-4: pools obey `MemoryUsage`'s constructor invariants whatever the
/// sample, report no collection usage before the first collection, and
/// the usage the last collection's seal stamped after it.
#[test]
fn w7f_backend_pools_are_valid_memory_usages_with_collection_usage() {
    let b = BackendGcBeans::new(BackendBeanShape::Zgc);
    let raced = JmxPoolSample {
        init: None,
        used: 700,
        committed: 500,
        max: Some(600),
    };
    let p = b.pools(&[raced]);
    assert_eq!((p[0].used, p[0].committed, p[0].max), (700, 700, Some(700)));
    assert_eq!(
        p[0].collection_used, None,
        "undefined before any collection"
    );
    b.note_collection_usage(&b.pools(&[sample(0, 0), sample(123, 1000)]));
    let p = b.pools(&[sample(0, 0), sample(900, 1000)]);
    assert_eq!(
        (p[0].collection_used, p[1].collection_used),
        (Some(0), Some(123))
    );
    assert!(b.pools(&[]).is_empty(), "no sample, no pool");
}

/// W7F-5: a backend queue serves exactly its own beans; the generational
/// queue is unchanged (`Copy` / `MarkSweepCompact`).
#[test]
fn w7f_queues_serve_their_own_beans() {
    let serial = GcNotificationQueue::new();
    assert!(serial.set_enabled("MarkSweepCompact", true));
    assert!(!serial.set_enabled("G1 Concurrent GC", true));
    let g1 = GcNotificationQueue::with_collectors(BackendBeanShape::G1.collectors());
    assert!(g1.set_enabled("G1 Concurrent GC", true));
    let mut r = rec("G1 Concurrent GC", 3);
    r.action = GC_ACTION_CONCURRENT_PAUSE;
    g1.record(r);
    g1.record(rec("G1 Young Generation", 8));
    assert_eq!(g1.last("G1 Young Generation").map(|r| r.id), Some(8));
    let d = g1.begin_drain().unwrap();
    assert_eq!(
        d.pop().map(|r| (r.collector, r.id)),
        Some(("G1 Concurrent GC", 3))
    );
    assert_eq!(d.pop(), None, "the young bean had no listener");
}
