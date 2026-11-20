    use super::*;

    const MIB: usize = 1024 * 1024;

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
    }

    fn collect(heap: &ZgcRealHeap, roots: &mut Vec<ObjectRef>) {
        // SAFETY: single-threaded unit test.
        let stw = unsafe { StopTheWorldToken::new() };
        heap.collect_garbage(&stw, roots, &NoMonitors);
    }

    /// A 64 MiB heap whose live set (`live_mib` of 32 KiB arrays, held by
    /// `roots`) is what one collection left, with the warm-up and budget
    /// clauses out of the way.
    fn heap_with_live(live_mib: usize, pct: usize) -> (ZgcRealHeap, Vec<ObjectRef>) {
        let heap = ZgcRealHeap::with_capacity(64 * MIB);
        heap.counters.warmup_enabled.store(false, Ordering::Relaxed);
        heap.alloc_trigger_bytes.store(0, Ordering::Relaxed);
        heap.counters
            .near_full_progress_pct
            .store(pct, Ordering::Relaxed);
        let mut roots = Vec::new();
        while heap.allocated.load(Ordering::Relaxed) < live_mib * MIB {
            roots.push(heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024));
        }
        collect(&heap, &mut roots);
        (heap, roots)
    }

    fn garbage_until(heap: &ZgcRealHeap, target: usize) {
        while heap.allocated.load(Ordering::Relaxed) < target {
            heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 8 * 1024);
        }
    }

    /// The default floor holds the occupancy clauses back past the legacy
    /// floor (and counts the interval once), then fires at
    /// `live + 75% of free`; the pacing census sees the near-full collection.
fn zr6tp_near_full_the_threshold_waits_for_three_quarters_of_free_space_body() {
        let (heap, mut roots) = heap_with_live(52, director::REARM_NEAR_FULL_DEFAULT_PERCENT);
        let live = heap.allocated.load(Ordering::Relaxed);
        assert!(
            live >= heap.gc_threshold,
            "fixture: live {live} under the threshold"
        );
        let free = heap.heap_capacity() - live;
        let rearm = heap.gc_rearm.load(Ordering::Relaxed);
        let legacy = heap.counters.gc_rearm_legacy.load(Ordering::Relaxed);
        assert_eq!(legacy, live + (free / 4).max(64 * 1024));
        assert_eq!(rearm, live + (free * 3 / 4).max(MIB));
        assert!(!heap.needs_gc());
        // Past the legacy floor: the pre-z6 rule would collect here.
        garbage_until(&heap, legacy + 16 * 1024);
        assert!(heap.allocated.load(Ordering::Relaxed) < rearm);
        assert!(
            !heap.needs_gc(),
            "the near-full floor must hold the collection back"
        );
        assert!(!heap.needs_gc());
        assert_eq!(
            heap.counters.pacing.deferred.load(Ordering::Relaxed),
            1,
            "charged once per interval"
        );
        garbage_until(&heap, rearm);
        assert!(heap.needs_gc(), "and admit it at live + 75% of free");
        collect(&heap, &mut roots);
        assert_eq!(heap.counters.pacing.near_full_count(), 1);
        assert_eq!(
            heap.counters
                .pacing
                .near_full_of(director::PaceKind::Threshold),
            1
        );
        let line = heap.pacing_line();
        assert!(
            line.contains("pace_near_full=1") && line.contains("pace_floor_deferred=1"),
            "{line}"
        );
        drop(roots);
    }

    /// Kill switch: `=0` publishes the legacy floor, and the collection is
    /// admitted exactly where it was before z6 (no deferral counted).
fn zr6tp_the_kill_switch_restores_the_quarter_floor_body() {
        let (heap, roots) = heap_with_live(52, 0);
        let live = heap.allocated.load(Ordering::Relaxed);
        assert!(live >= heap.gc_threshold);
        let free = heap.heap_capacity() - live;
        let rearm = heap.gc_rearm.load(Ordering::Relaxed);
        assert_eq!(rearm, live + (free / 4).max(64 * 1024));
        assert_eq!(rearm, heap.counters.gc_rearm_legacy.load(Ordering::Relaxed));
        garbage_until(&heap, rearm);
        assert!(heap.needs_gc());
        assert_eq!(heap.counters.pacing.deferred.load(Ordering::Relaxed), 0);
        drop(roots);
    }

    /// Below the threshold the floor is unchanged (warm-up and cause-line
    /// pacing are not this switch's business).
    #[test]
    fn zr6tp_below_the_threshold_the_floor_is_unchanged() {
        let (heap, roots) = heap_with_live(20, director::REARM_NEAR_FULL_DEFAULT_PERCENT);
        let live = heap.allocated.load(Ordering::Relaxed);
        assert!(live < heap.gc_threshold);
        let free = heap.heap_capacity() - live;
        assert_eq!(heap.gc_rearm.load(Ordering::Relaxed), live + free / 4);
        assert_eq!(heap.counters.pacing.near_full_count(), 0);
        drop(roots);
    }

    /// The census kinds follow the mask; the not-a-reason bits are unasked.
    #[test]
    fn zr6tp_pace_kind_follows_the_mask() {
        use director::PaceKind;
        assert_eq!(pace_kind_for_mask(0), PaceKind::Unasked);
        assert_eq!(
            pace_kind_for_mask(TRIGGER_BIT_PACE_DEFERRED),
            PaceKind::Unasked
        );
        assert_eq!(
            pace_kind_for_mask(TRIGGER_BIT_HEADROOM_BLOCKED),
            PaceKind::Unasked
        );
        assert_eq!(
            pace_kind_for_mask(TRIGGER_BIT_THRESHOLD | TRIGGER_BIT_HEADROOM),
            PaceKind::Threshold
        );
        assert_eq!(pace_kind_for_mask(TRIGGER_BIT_HEADROOM), PaceKind::Headroom);
        assert_eq!(pace_kind_for_mask(TRIGGER_BIT_WARMUP), PaceKind::Warmup);
        assert_eq!(
            pace_kind_for_mask(TRIGGER_BIT_HIGH_USAGE),
            PaceKind::Director
        );
        assert_eq!(hotspot_cause_for_mask(TRIGGER_BIT_PACE_DEFERRED), None);
    }
