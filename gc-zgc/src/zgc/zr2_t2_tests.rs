use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

/// zr z3/t2: the failure door's cause follows the DOOR's classification.
/// A failure site is `Allocation Stall` with or without a pending rule
/// (the z3a host regression: on a tight heap a rule is nearly always
/// pending, and the latch reading printed no stall at all); an occupancy
/// site names the pending rule (`Warmup` here, the positive control) and
/// `Allocation Rate` when none is pending. `=0`: `Allocation Stall`
/// everywhere; `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`: `None`.
#[test]
fn failure_door_cause_follows_the_door_not_the_latch() {
    let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
    assert_eq!(
        heap.allocation_failure_gc_cause(false),
        Some("Allocation Stall")
    );
    assert_eq!(
        heap.allocation_failure_gc_cause(true),
        Some("Allocation Rate")
    );
    let mut held = Vec::new();
    for _ in 0..40_000 {
        if heap.needs_gc() {
            break;
        }
        held.push(heap.alloc_object(ClassId::new(1), 64));
    }
    assert!(heap.needs_gc(), "the warm-up rule never fired");
    // Rule pending + failure site = stall.
    assert_eq!(
        heap.allocation_failure_gc_cause(false),
        Some("Allocation Stall"),
        "a failed allocation is a stall even with a rule pending"
    );
    assert_eq!(heap.allocation_failure_gc_cause(true), Some("Warmup"));
    heap.counters
        .stall_cause_from_trigger
        .store(false, Ordering::Relaxed);
    assert!(!heap.stall_classification_by_door());
    assert_eq!(
        heap.allocation_failure_gc_cause(true),
        Some("Allocation Stall")
    );
    heap.counters
        .hotspot_gc_cause
        .store(false, Ordering::Relaxed);
    assert_eq!(heap.allocation_failure_gc_cause(false), None);
    drop(held);
}


/// Young is what was allocated since the last collection, old the
/// post-collection figure (the watermark, clamped to `allocated`);
/// `=0` answers `None` (the caller keeps the whole heap in old).
#[test]
fn young_old_split_follows_the_cycle_watermark() {
    let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    // zt t27/gn5 (gn page item 10): the single-generation split; the
    // mode's is `zt6tc_a_minor_leaves_its_young_survivors_in_the_young_pool`.
    heap.set_generational_enabled(false);
    let _held: Vec<_> = (0..100)
        .map(|_| heap.alloc_object(ClassId::new(1), 8))
        .collect();
    let a = heap.allocated_bytes();
    assert!(a > 0);
    assert_eq!(heap.jmx_young_old_split(), Some((a, 0)));
    heap.cycle_alloc_watermark.store(a / 2, Ordering::Relaxed);
    assert_eq!(heap.jmx_young_old_split(), Some((a - a / 2, a / 2)));
    heap.cycle_alloc_watermark.store(a * 2, Ordering::Relaxed);
    assert_eq!(heap.jmx_young_old_split(), Some((0, a)), "clamped");
    heap.counters
        .young_pool_since_cycle
        .store(false, Ordering::Relaxed);
    assert_eq!(heap.jmx_young_old_split(), None);
}

/// After a real collection the young pool is empty and the old pool
/// holds what survived (the watermark is the post-sweep live figure).
#[test]
fn a_collection_moves_survivors_to_old_and_empties_young() {
    let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    // zt t27/gn5 (gn page item 10): single generation -- in the mode a
    // major leaves its young-by-age keeper in the young pool
    // (`CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG`, HotSpot's tenuring by age).
    heap.set_generational_enabled(false);
    let keep = heap.alloc_object(ClassId::new(1), 8);
    for _ in 0..1_000 {
        let _ = heap.alloc_object(ClassId::new(1), 8);
    }
    // SAFETY: single-threaded test; no mutator runs concurrently.
    let stw = unsafe { StopTheWorldToken::new() };
    let mut roots = [keep];
    let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
    let (young, old) = heap.jmx_young_old_split().expect("default on");
    assert_eq!(young, 0, "nothing allocated since the collection");
    assert_eq!(old, heap.allocated_bytes());
    assert!(old > 0, "the rooted object survived");
}

/// zt t6/tc (zt1tp item 7, `CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR`): a
/// collection run for a `System.gc()` request on the collecting thread is
/// not a warm-up step even with the warm-up bit raised; the same raised
/// bit without the request is one (the control).
#[test]
fn zt6tc_an_explicit_door_collection_is_not_a_warmup_step() {
    for explicit in [false, true] {
        let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
        let keep = heap.alloc_object(ClassId::new(1), 8);
        heap.counters
            .trigger_seen
            .fetch_or(TRIGGER_BIT_WARMUP, Ordering::Relaxed);
        if explicit {
            cratonvm_gc_core::gc_quiescence::request_major_gc();
        }
        {
            // SAFETY: single-threaded test; no mutator runs concurrently.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [keep];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        let _ = cratonvm_gc_core::gc_quiescence::take_major_gc_request();
        assert_eq!(
            heap.counters.warmup_cycles_done.load(Ordering::Relaxed),
            usize::from(!explicit),
            "explicit={explicit}"
        );
        let mask = heap.counters.last_cycle_trigger_mask.load(Ordering::Relaxed);
        assert_eq!(
            mask & TRIGGER_BIT_WARMUP,
            if explicit { 0 } else { TRIGGER_BIT_WARMUP },
            "explicit={explicit}: the cycle's mask"
        );
    }
}

/// zt t14/tg6 (`CRATONVM_ZGC_PROACTIVE_MAJOR_CLOCK`): the cycles HotSpot
/// runs as minors leave the proactive rule's major clock; the major
/// causes and an empty mask (a `System.gc()`) restart it.
#[test]
fn zt14tg6_only_a_hotspot_major_restarts_the_proactive_clock() {
    for (mask, stall, restarts) in [
        (TRIGGER_BIT_RATE, false, false),
        (TRIGGER_BIT_THRESHOLD, false, false),
        (TRIGGER_BIT_HIGH_USAGE, false, false),
        (TRIGGER_BIT_WARMUP, true, false),
        (0, true, false),
        (TRIGGER_BIT_WARMUP, false, true),
        (TRIGGER_BIT_PROACTIVE, false, true),
        (TRIGGER_BIT_TIMER | TRIGGER_BIT_RATE, false, true),
        (0, false, true),
    ] {
        assert_eq!(
            cycle_restarts_major_clock(mask, stall),
            restarts,
            "mask={mask:#x} stall={stall}"
        );
    }
}


/// zt t6/tc (zt1tp item 2): a management write of `SoftMaxHeapSize`
/// moves the soft maximum (and with `CRATONVM_ZGC_SOFT_MAX_PACES` its
/// threshold) at run time and records its origin; the default reports the
/// capacity with the ergonomic origin.
#[test]
fn zt6tc_soft_max_is_writeable_at_run_time() {
    let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    let cap = heap.heap_capacity();
    assert_eq!(heap.soft_max_capacity(), cap);
    assert_eq!(heap.soft_max_origin_kind(), 0, "ergonomic default");
    heap.set_soft_max_heap_bytes(cap / 2);
    assert_eq!(heap.soft_max_origin_kind(), 1, "command line");
    heap.set_soft_max_heap_bytes_by_management(cap / 4);
    assert_eq!(heap.soft_max_capacity(), cap / 4, "applied at run time");
    assert_eq!(heap.soft_max_origin_kind(), 2, "management");
    // zt t7/x4b: HotSpot 25.0.3 keeps a zero soft maximum (`Soft Max
    // Capacity: 0M`); this pinned "0 is the capacity". The capacity is
    // the `CRATONVM_ZGC_SOFT_MAX_ZERO_HOTSPOT=0` kill arm only.
    heap.set_soft_max_heap_bytes_by_management(0);
    assert_eq!(heap.soft_max_capacity(), 0, "0 is a soft maximum of 0");
    assert_eq!(heap.soft_max_origin_kind(), 2, "management");    }