    use super::*;

    /// zr z1/tr: the first admitting clause in `needs_gc` order names the
    /// cause, and an empty mask leaves it to the door.
    #[test]
    fn hotspot_cause_follows_the_needs_gc_clause_order() {
        assert_eq!(hotspot_cause_for_mask(0), None);
        assert_eq!(hotspot_cause_for_mask(TRIGGER_BIT_WARMUP), Some("Warmup"));
        assert_eq!(
            hotspot_cause_for_mask(TRIGGER_BIT_THRESHOLD | TRIGGER_BIT_WARMUP),
            Some("Warmup")
        );
        assert_eq!(
            hotspot_cause_for_mask(TRIGGER_BIT_THRESHOLD),
            Some("Allocation Rate")
        );
        assert_eq!(
            hotspot_cause_for_mask(TRIGGER_BIT_PROACTIVE),
            Some("Proactive")
        );
        assert_eq!(
            hotspot_cause_for_mask(TRIGGER_BIT_HIGH_USAGE),
            Some("High Usage")
        );
        assert_eq!(
            hotspot_cause_for_mask(TRIGGER_BIT_NURSERY),
            Some("Allocation Rate")
        );
        // The not-a-reason bits name nothing.
        assert_eq!(
            hotspot_cause_for_mask(
                TRIGGER_BIT_HEADROOM_BLOCKED | TRIGGER_BIT_HEADROOM_BYPASS_SUPPRESSED
            ),
            None
        );
    }

    /// The lock-free capacity is the arena's, and the soft maximum clamps to it.
    #[test]
    fn capacity_is_lock_free_and_soft_max_clamps() {
        let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
        assert_eq!(heap.heap_capacity(), heap.arena.lock().capacity());
        assert_eq!(heap.soft_max_capacity(), heap.heap_capacity());
        heap.set_soft_max_heap_bytes(8 * 1024 * 1024);
        assert_eq!(heap.soft_max_capacity(), 8 * 1024 * 1024);
        heap.set_soft_max_heap_bytes(usize::MAX);
        assert_eq!(heap.soft_max_capacity(), heap.heap_capacity());
        heap.set_soft_max_heap_bytes(0);
        assert_eq!(heap.soft_max_capacity(), 0, "HotSpot keeps a zero soft maximum");    }

    /// Positive control for the cause plumbing: the first collection a fresh
    /// heap asks for is the warm-up rule's (10% of the soft maximum), and its
    /// pending cause reads `Warmup`; a soft maximum of half the heap moves
    /// the warm-up point down with it.
    #[test]
    fn first_collection_is_warmup_and_tracks_the_soft_max() {
        fn bytes_until_gc(soft: usize) -> (usize, Option<&'static str>) {
            let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
            heap.set_soft_max_heap_bytes(soft);
            let mut held = Vec::new();
            for _ in 0..20_000 {
                if heap.needs_gc() {
                    break;
                }
                held.push(heap.alloc_object(ClassId::new(1), 64));
            }
            assert!(heap.needs_gc(), "the warm-up rule never fired");
            (
                heap.allocated.load(Ordering::Relaxed),
                heap.hotspot_gc_cause(true),
            )
        }
        // zt t7/x4c: "no soft maximum" is the capacity (`usize::MAX` clamps to
        // it). This passed `0` for that; since zt t7/x4b a 0 is a soft maximum
        // of 0, as HotSpot 25.0.3 keeps it (`CRATONVM_ZGC_SOFT_MAX_ZERO_HOTSPOT`).
        let (full, cause) = bytes_until_gc(usize::MAX);
        assert_eq!(cause, Some("Warmup"));
        let (half, cause_half) = bytes_until_gc(32 * 1024 * 1024);
        assert_eq!(cause_half, Some("Warmup"));
        assert!(
            half < full,
            "soft max did not move the warm-up point: {half} vs {full}"
        );
        // zt t7/x4c: a soft maximum of 0 puts the warm-up point at 0, as on
        // HotSpot 25.0.3 (`-XX:SoftMaxHeapSize=0 -Xlog:gc` runs `GC(0) Major
        // Collection (Warmup)` at its first director tick, then `GC(1)` and
        // `GC(2)` Warmup back to back).
        let (zero, cause_zero) = bytes_until_gc(0);
        assert_eq!(cause_zero, Some("Warmup"));
        assert!(zero < half, "a zero soft maximum warms up first: {zero} vs {half}");
    }

    /// zt t7/tg (zt1tp item 3): the `Timer` rule end to end on the heap. Off
    /// by default (no tick wanted); `-XX:ZCollectionInterval` arms it; a
    /// tick request charges `Timer` ahead of a pending warm-up (HotSpot asks
    /// `rule_major_timer` first); the cycle that ran restarts the clock; and
    /// a request that lost to a cycle which answered it is withdrawn so it
    /// cannot label a later collection.
fn zt7tg_a_timer_request_names_the_cycle_and_the_cycle_restarts_the_clock_body() {
        use std::time::{Duration, Instant};
        struct NoMonitors;
        impl MonitorCleanup for NoMonitors {
            fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
        }
        let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
        assert!(!heap.director_tick_wanted(), "default: no tick thread");
        assert_eq!(
            heap.director_tick(Instant::now()),
            timer::ZTick::Off,
            "default: nothing armed"
        );
        heap.set_collection_interval(Some(Duration::from_millis(1)));
        assert!(heap.director_tick_wanted());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            heap.director_tick(Instant::now()),
            timer::ZTick::Collect(timer::ZTickCause::Timer),
            "1 ms after construction the 1 ms timer is due"
        );
        heap.counters
            .trigger_seen
            .fetch_or(TRIGGER_BIT_WARMUP, Ordering::Relaxed);
        heap.request_tick_cycle(timer::ZTickCause::Timer);
        assert_eq!(heap.hotspot_gc_cause(true), Some("Timer"));
        assert_eq!(heap.director_tick_counts(), (1, 0, 0));
        let keep = heap.alloc_object(ClassId::new(1), 8);
        {
            // SAFETY: single-threaded test; no mutator runs concurrently.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [keep];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        assert_eq!(
            heap.hotspot_gc_cause(false),
            Some("Timer"),
            "the cycle's cause"
        );
        assert_eq!(heap.hotspot_gc_cause(true), None, "the latch was consumed");
        // The cycle just ended: a long interval is not due right after it.
        heap.set_collection_interval(Some(Duration::from_secs(3600)));
        assert!(matches!(
            heap.director_tick(Instant::now()),
            timer::ZTick::Wait(_)
        ));
        // A request the cycle answered is taken back off the latch...
        heap.request_tick_cycle(timer::ZTickCause::Timer);
        assert!(heap.withdraw_tick_cycle_if_answered(
            timer::ZTickCause::Timer,
            Instant::now()
        ));
        assert_eq!(heap.hotspot_gc_cause(true), None, "withdrawn");
        assert_eq!(heap.director_tick_counts(), (2, 0, 1));
        // ...and one the rule still asks for is kept (control).
        heap.set_collection_interval(Some(Duration::from_nanos(1)));
        heap.request_tick_cycle(timer::ZTickCause::Timer);
        assert!(!heap.withdraw_tick_cycle_if_answered(
            timer::ZTickCause::Timer,
            Instant::now()
        ));
        assert_eq!(heap.hotspot_gc_cause(true), Some("Timer"));
        // Off again: no tick wanted.
        heap.set_collection_interval(None);
        assert!(!heap.director_tick_wanted());
    }


    /// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals` item 9): a stopped
    /// tick plans `Off` even with the timer due, so the tick thread ends.
    #[test]
    fn zt9tg2_a_stopped_director_tick_plans_off() {
        use std::time::{Duration, Instant};
        let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
        heap.set_collection_interval(Some(Duration::from_nanos(1)));
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            heap.director_tick(Instant::now()),
            timer::ZTick::Collect(timer::ZTickCause::Timer),
            "positive control: due before the stop"
        );
        heap.stop_director_tick();
        assert_eq!(heap.director_tick(Instant::now()), timer::ZTick::Off);
        heap.stop_director_tick();
        assert_eq!(heap.director_tick(Instant::now()), timer::ZTick::Off, "idempotent");
    }


    #[test]
    fn zt7tg_timer_is_named_before_warmup_and_paces_as_other() {
        assert_eq!(hotspot_cause_for_mask(TRIGGER_BIT_TIMER), Some("Timer"));
        assert_eq!(
            hotspot_cause_for_mask(TRIGGER_BIT_TIMER | TRIGGER_BIT_WARMUP | TRIGGER_BIT_RATE),
            Some("Timer")
        );
        assert_eq!(
            pace_kind_for_mask(TRIGGER_BIT_TIMER),
            director::PaceKind::Other
        );
        assert_eq!(
            TRIGGER_BIT_TIMER & (TRIGGER_BIT_DIRECTOR_DEFERRED | TRIGGER_BIT_PACE_DEFERRED),
            0
        );
    }
