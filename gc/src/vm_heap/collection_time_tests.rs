use super::*;

/// gce e1/o: the JMX collection time is the floor of the microsecond
/// pause sum (HotSpot's accumulated timer), not the sum plus one
/// millisecond per collection.
#[test]
fn collection_time_is_the_floor_of_the_pause_sum() {
    assert_eq!(pause_sum_as_collection_time_ms(0), 0);
    // A sub-millisecond pause does not move it (HotSpot's behaviour)...
    assert_eq!(pause_sum_as_collection_time_ms(400), 0);
    // ...the sum does, once it crosses the next millisecond.
    assert_eq!(pause_sum_as_collection_time_ms(1_200), 1);
    // 1 000 pauses of 100 µs: 100 ms, not 1 100.
    assert_eq!(pause_sum_as_collection_time_ms(1_000 * 100), 100);
    assert_eq!(pause_sum_as_collection_time_ms(125_400), 125);
    assert_eq!(pause_sum_as_collection_time_ms(u64::MAX), u64::MAX / 1_000);
}

/// A fresh heap reports zero where the backend keeps time, and ZGC —
/// which keeps none — says so rather than inventing a figure.
#[test]
fn a_fresh_heap_reports_zero_time_or_none() {
    const MB: usize = 1024 * 1024;
    let generational = VmHeap::new(GcBackend::Generational, 16 * MB);
    assert_eq!(generational.collection_time_ms(), Some(0));
    let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
    assert_eq!(g1.collection_time_ms(), Some(0));
    assert_eq!(g1.collection_count(), 0);
    #[cfg(feature = "zgc")]
    assert_eq!(
        VmHeap::new(GcBackend::Zgc, 16 * MB).collection_time_ms(),
        None
    );
}

/// gc-common w7-f: the dispatcher's JMX half. G1 carries its bean state in
/// the heap (per heap, so per VM), and its pool samples are three valid
/// `MemoryUsage`s that partition the committed heap; Generational has no
/// backend state (its beans are its own); ZGC samples its one pool (its
/// bean state lives in the VM's `HeapRealm`).
#[test]
fn w7f_backend_bean_state_and_pool_samples() {
    use cratonvm_gc_core::gc_metrics::BackendBeanShape;
    const MB: usize = 1024 * 1024;
    let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
    let beans = g1.backend_gc_beans().expect("G1 books its beans");
    assert_eq!(beans.shape(), BackendBeanShape::G1);
    let samples = g1.backend_pool_samples();
    assert_eq!(samples.len(), BackendBeanShape::G1.pools().len());
    for s in &samples {
        assert!(s.used <= s.committed, "{s:?}");
        assert!(s.max.map_or(true, |m| s.committed <= m), "{s:?}");
    }
    let committed: u64 = samples.iter().map(|s| s.committed).sum();
    assert!(
        committed >= g1.committed_bytes() as u64,
        "the three G1 pools cover the committed heap ({committed} < {})",
        g1.committed_bytes()
    );
    let pools = beans.pools(&samples);
    assert_eq!(pools[2].name, "G1 Old Gen");
    assert_eq!(pools[2].max, Some(g1.heap_capacity() as u64));
    // A second heap has its own state: per heap, never shared.
    let other = VmHeap::new(GcBackend::G1, 64 * MB);
    beans.note_collection(1_000);
    assert_eq!(beans.collectors()[0].count, 1);
    assert_eq!(other.backend_gc_beans().unwrap().collectors()[0].count, 0);

    let generational = VmHeap::new(GcBackend::Generational, 16 * MB);
    assert!(generational.backend_gc_beans().is_none());
    assert!(generational.backend_pool_samples().is_empty());
    #[cfg(feature = "zgc")]
    {
        let z = VmHeap::new(GcBackend::Zgc, 16 * MB);
        assert!(z.backend_gc_beans().is_none(), "ZGC's state is the VM's");
        let s = z.backend_pool_samples();
        assert_eq!(s.len(), BackendBeanShape::Zgc.pools().len());
        assert_eq!(s[0].used, 0, "nothing allocated yet");
        assert_eq!(s[0].init, Some(0));
        assert!(
            s[1].init.is_some(),
            "the old pool carries the startup commit"
        );
        assert!(s[1].used <= s[1].committed);
        // zr z3/t2: under `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE` (default
        // on) old is `committed == used` and young carries the rest of the
        // heap's commit; under `=0` young is empty and old carries it all.
        // Either way the pools' committed sum to the heap's, as HotSpot 25
        // ZGC's do (`Zr3T2HeapVsPools`, host).
        assert_eq!(
            s[0].committed + s[1].committed,
            z.committed_bytes() as u64,
            "pools' committed sum to the heap's"
        );
    }
}

/// gci i14/mp finding 4: a G1 humongous array checks `G1 Old Gen`'s usage
/// threshold at once (HotSpot's `detect_low_memory_for_collected_pools`
/// after the allocation); nothing is checked while no threshold is armed,
/// and an ordinary (young) array does not check it.
#[test]
fn gci_i14_mp_a_g1_humongous_array_checks_the_old_usage_threshold() {
    use cratonvm_gc_core::gc_metrics::{PoolThresholdKind, JMX_POOL_G1_OLD};
    const MB: usize = 1024 * 1024;
    let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
    let beans = g1.backend_gc_beans().expect("G1 books its beans");
    let thresholds = beans.notifications().pool_thresholds();
    // Unarmed: a humongous allocation queues nothing.
    let big = g1.try_alloc_array(ClassId::new(1), ArrayElementType::Byte, 4 * MB);
    assert!(big.is_some(), "a 4 MiB array fits a 64 MiB G1 heap");
    assert!(!thresholds.has_pending());
    // Armed below the old pool's usage now: the next humongous array
    // crosses it without a collection.
    thresholds.set(JMX_POOL_G1_OLD, PoolThresholdKind::Usage, 1);
    let small = g1.try_alloc_array(ClassId::new(1), ArrayElementType::Byte, 64);
    assert!(small.is_some());
    assert!(!thresholds.has_pending(), "a young array does not grow the old pool");
    let big2 = g1.try_alloc_array(ClassId::new(1), ArrayElementType::Byte, 4 * MB);
    assert!(big2.is_some());
    assert!(thresholds.has_pending(), "the humongous array crossed the usage threshold");
    let requests = thresholds.take_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].pool, JMX_POOL_G1_OLD);
    assert!(requests[0].trigger_usage.is_some(), "a trigger, not a clear");
}

/// zr z3/t2 (host follow-up): the stall classification is the door's.
/// A failure site is a stall even with a trigger rule pending (the z3a
/// regression: `stall_total=0` at `-Xmx48m`); an occupancy site is not;
/// `=0` counts every door but the refill trigger's; off ZGC never.
#[cfg(feature = "zgc")]
#[test]
fn zr3t2_rule_pending_and_a_failure_site_is_a_stall() {
    use cratonvm_types::ClassId;
    let z = VmHeap::new(GcBackend::Zgc, 64 * 1024 * 1024);
    let VmHeapInner::Zgc(h) = &z.inner else { unreachable!() };
    let mut held = Vec::new();
    for _ in 0..40_000 {
        if h.needs_gc() {
            break;
        }
        held.push(h.alloc_object(ClassId::new(1), 64));
    }
    assert!(h.needs_gc(), "a rule is pending");
    assert!(
        z.zgc_forced_collection_is_stall(false, false),
        "failure site"
    );
    assert_eq!(z.zgc_forced_door_gc_cause(false), Some("Allocation Stall"));
    assert!(
        !z.zgc_forced_collection_is_stall(true, false),
        "occupancy site"
    );
    assert!(
        !z.zgc_forced_collection_is_stall(true, true),
        "refill trigger"
    );
    assert_ne!(z.zgc_forced_door_gc_cause(true), Some("Allocation Stall"));
    h.counters_for_test_set_stall_classification(false);
    assert!(
        z.zgc_forced_collection_is_stall(true, false),
        "=0: z2 reading"
    );
    assert!(
        !z.zgc_forced_collection_is_stall(true, true),
        "=0: refill excluded"
    );
    drop(held);
    let g1 = VmHeap::new(GcBackend::G1, 64 * 1024 * 1024);
    assert!(!g1.zgc_forced_collection_is_stall(false, false));
    assert_eq!(g1.zgc_forced_door_gc_cause(false), None);
}

/// zr z3/t2: HotSpot 25 ZGC's young committed (host, -Xmx256m:
/// heap 268435456, old 4194304 -> young 264241152), never below young
/// used, and saturating when old exceeds the commit.
#[cfg(feature = "zgc")]
#[test]
fn zr3t2_zgc_young_committed_is_heap_less_old() {
    const M: usize = 1024 * 1024;
    assert_eq!(
        super::zgc_young_pool_committed(256 * M, 2 * M, 4 * M),
        252 * M
    );
    assert_eq!(super::zgc_young_pool_committed(8 * M, 6 * M, 4 * M), 6 * M);
    assert_eq!(super::zgc_young_pool_committed(4 * M, 1 * M, 8 * M), 1 * M);
}

/// zr z3/t2: on a heap with survivors and fresh garbage the two pools
/// still sum to the heap's committed, and old stays `committed == used`.
#[cfg(feature = "zgc")]
#[test]
fn zr3t2_zgc_pools_sum_to_heap_committed_after_a_collection() {
    use cratonvm_types::ClassId;
    let z = VmHeap::new(GcBackend::Zgc, 32 * 1024 * 1024);
    let VmHeapInner::Zgc(h) = &z.inner else { unreachable!() };
    let keep = h.alloc_object(ClassId::new(1), 8);
    for _ in 0..2_000 {
        let _ = h.alloc_object(ClassId::new(1), 8);
    }
    struct NoMonitors;
    impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
    }
    // SAFETY: single-threaded unit test.
    let stw = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
    let mut roots = [keep];
    let _ = cratonvm_gc_core::collector::GarbageCollector::collect_garbage(
        h.as_ref(),
        &stw,
        &mut roots,
        &NoMonitors,
    );
    for _ in 0..500 {
        let _ = h.alloc_object(ClassId::new(1), 8);
    }
    let s = z.backend_pool_samples();
    if h.jmx_young_old_split().is_some() {
        assert_eq!(s[1].committed, s[1].used, "old: committed == used");
        assert!(s[0].committed >= s[0].used);
    }
    assert_eq!(
        s[0].committed + s[1].committed,
        (z.committed_bytes() as u64).max(s[0].used + s[1].used)
    );
}
