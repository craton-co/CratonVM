// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational GC round 4 wave 2, lane `obs` (2026-09-23): the heap-side
//! numbers behind `-Xlog:gc` and the per-generation JMX beans.
//!
//! An integration test (its own process) rather than a `gen_heap.rs` unit test
//! for two reasons: constructing a heap publishes into process-global bounds
//! tables (`published_bounds_isolation.rs` has the flake that caused), and
//! forcing the non-moving young path goes through
//! `gc_quiescence::publish_moving_young_enabled`, which is process-global
//! outside the gc crate's own `cfg(test)` build. Every test in this binary is
//! indifferent to which young path runs except the one that forces it.
//!
//! See `docs/internal/reviews/gengc-round4-w2-obs-20260923.md`.

use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::gc_metrics::{
    JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED, JMX_SERIAL_OLD_COLLECTOR,
    JMX_SERIAL_YOUNG_COLLECTOR,
};
use cratonvm_gc::vm_heap::VmHeap;
use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, Value};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: each test drives its own heap from a single thread; no other
    // mutator of that heap exists, so the STW invariant holds trivially.
    unsafe { StopTheWorldToken::new() }
}

/// The `-Xlog:gc` `before->after` bug: `cratonvm -XX:+UseGenerationalGC
/// -Xlog:gc -Xmx64m BinT 16` printed `8M->8M` on every young pause.
///
/// The VM's GC-event emitter (`vm/src/runtime/interpreter/gc_events.rs::
/// gc_event_heap_used`) now reports `VmHeap::live_bytes_estimate`. This pins
/// the heap half of that: across a NON-MOVING young sweep that reclaimed
/// garbage, `allocated_bytes` (what the line used to print) does not move at
/// all, and `live_bytes_estimate` (what it prints now) falls by what the sweep
/// freed — the pair `--verbose:gc` shows as `young_free=a->b`.
#[test]
fn xlog_occupancy_sees_a_non_moving_young_sweep() {
    let heap = VmHeap::Generational(GenerationalHeap::with_sizes(4 * 1024, 8 * 1024));
    let live = heap.alloc_object(ClassId::new(1), 1);
    heap.set_field(live, 0, Value::Int(1));
    for i in 0..16 {
        let dead = heap.alloc_object(ClassId::new(9), 1);
        heap.set_field(dead, 0, Value::Int(i));
    }
    let allocated_before = heap.allocated_bytes();
    let live_before = heap.live_bytes_estimate();

    // The production default whenever a JIT frame is live.
    cratonvm_gc::gc_quiescence::publish_moving_young_enabled(false);
    cratonvm_gc::gc_quiescence::enter();
    let mut roots = vec![live];
    let result = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    cratonvm_gc::gc_quiescence::leave();

    assert!(
        result.stats.bytes_freed > 0,
        "the sweep must have reclaimed the garbage"
    );
    assert_eq!(
        heap.allocated_bytes(),
        allocated_before,
        "the non-moving sweep does not retract the bump cursor -- which is why \
         allocated_bytes printed `8M->8M` and must not be the -Xlog:gc figure"
    );
    let live_after = heap.live_bytes_estimate();
    assert!(
        live_after < live_before,
        "live_bytes_estimate must show the reclamation (before={live_before}, after={live_after})"
    );
}

/// The three pools partition the heap the way HotSpot's `jmm_GetMemoryUsage`
/// builds the heap usage from its pools: their `committed` sums to the heap's
/// committed bytes (`VmHeap::committed_bytes`, i.e. `Runtime.totalMemory()`
/// and the heap `MemoryUsage.getCommitted()`), their `used` to the live
/// estimate, and the names/managers are HotSpot Serial's.
///
/// gen r5w2/obs6 (2026-09-26): the invariant moved. It was "the pools sum to
/// `committed_heap_bytes()`", a SUM OF CAPACITIES that counted the young
/// copy reserve (reported as `Survivor Space`) and the old generation's
/// reservation (reported as `Tenured Gen` committed) — while `Runtime` and the
/// heap bean counted committed granules, so the pools and the heap bean
/// disagreed. Now all three are HotSpot Serial's `capacity()`: one
/// semi-space's commit plus the old generation's commit, the to-space
/// counted nowhere (`gengc-r4w3-hunter-runtime-memory-counts-the-copy-reserve`,
/// `gengc-r5w1-oldgen5-tenured-pool-reports-the-reservation-as-committed`).
#[test]
fn jmx_pools_partition_committed_and_used() {
    let heap = VmHeap::Generational(GenerationalHeap::with_capacity(8 * 1024 * 1024));
    let VmHeap::Generational(h) = &heap else {
        unreachable!("constructed as Generational")
    };
    for _ in 0..64 {
        let _ = h.alloc_object(ClassId::new(1), 4);
    }
    assert_eq!(
        h.try_alloc_objects_old_batch(ClassId::new(2), 4, 1).len(),
        1
    );
    let pools = h.jmx_memory_pools();
    let names: Vec<&str> = pools.iter().map(|p| p.name).collect();
    assert_eq!(names, [JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED]);

    let committed: u64 = pools.iter().map(|p| p.committed).sum();
    assert_eq!(committed, h.usable_committed_bytes() as u64);
    assert_eq!(
        committed,
        heap.committed_bytes() as u64,
        "the pools must sum to Runtime.totalMemory()'s source"
    );
    assert!(
        committed < h.committed_heap_bytes() as u64,
        "the copy reserve is not usable heap: committed {committed} must be \
         below the sum of capacities {}",
        h.committed_heap_bytes()
    );
    let used: u64 = pools.iter().map(|p| p.used).sum();
    assert_eq!(used, h.live_bytes_estimate() as u64);
    assert!(
        pools[0].used > 0,
        "64 young allocations must show in Eden Space"
    );
    assert!(
        pools[2].used > 0,
        "an old-gen allocation must show in Tenured Gen"
    );
    assert_eq!(
        (pools[1].init, pools[1].used, pools[1].committed, pools[1].max),
        (Some(0), 0, 0, None),
        "Survivor Space: survivors live in the from-space, and the to-space \
         copy reserve is in no pool"
    );
    for p in &pools {
        assert!(
            p.used <= p.committed,
            "{}: used {} > committed {}",
            p.name,
            p.used,
            p.committed
        );
        if let Some(max) = p.max {
            assert!(
                p.committed <= max,
                "{}: committed {} > max {max}",
                p.name,
                p.committed
            );
        }
        assert_eq!(
            p.collection_used, None,
            "{}: no collection has run yet",
            p.name
        );
    }
    assert_eq!(pools[2].managers, [JMX_SERIAL_OLD_COLLECTOR]);
    assert_eq!(
        pools[0].managers,
        [JMX_SERIAL_YOUNG_COLLECTOR, JMX_SERIAL_OLD_COLLECTOR]
    );

    // After a collection, collection usage is recorded for every pool.
    let mut roots = Vec::new();
    let _ = h.collect_garbage(&stw(), &mut roots, &NoMonitors);
    for p in h.jmx_memory_pools() {
        assert!(
            p.collection_used.is_some(),
            "{}: collection usage after a GC",
            p.name
        );
    }
}

/// Each completed collection is counted exactly once, under `MarkSweepCompact`
/// when it also reclaimed the old generation and under `Copy` otherwise, and
/// the two beans' counts always sum to the heap's collection total.
#[test]
fn jmx_collectors_partition_the_collections() {
    let h = GenerationalHeap::with_capacity(8 * 1024 * 1024);
    let mut roots = vec![h.alloc_object(ClassId::new(1), 1)];
    let (mut young, mut full) = (0u64, 0u64);
    for i in 0..6 {
        if i % 3 == 2 {
            // What `System.gc()` does before its collection.
            cratonvm_gc::gc_quiescence::request_major_gc();
        }
        let before = h.gc_pause_totals().0;
        let _ = h.collect_garbage(&stw(), &mut roots, &NoMonitors);
        if h.gc_pause_totals().0 == before {
            // A refused cycle collected nothing and is booked under neither
            // bean (as it is excluded from `pause_totals`).
            continue;
        }
        // The same thread-local the heap consulted when it booked the pause.
        if cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle() {
            full += 1;
        } else {
            young += 1;
        }
    }
    let [copy, msc] = h.jmx_collectors();
    assert_eq!(copy.name, JMX_SERIAL_YOUNG_COLLECTOR);
    assert_eq!(msc.name, JMX_SERIAL_OLD_COLLECTOR);
    assert_eq!((copy.count, msc.count), (young, full));
    assert_eq!(copy.count + msc.count, h.gc_pause_totals().0);
    // gen r5w3/obs7: each bean's time is the floor of its own MICROSECOND sum
    // (HotSpot's accumulated timer), so the two sum to within one millisecond
    // of the total's floor. They used to sum to the per-pause-ceiled `.2`.
    let total_ms = h.gc_pause_totals().1 / 1_000;
    assert!(copy.time_ms + msc.time_ms <= total_ms);
    assert!(copy.time_ms + msc.time_ms + 1 >= total_ms);
    assert!(
        full >= 1,
        "a requested major GC must have reclaimed old gen at least once"
    );
}

/// `is_heap_addr` now ACCEPTS from the lock-free `region_bounds` mirror and
/// falls back to the three arena locks only on a miss. Its answers must be the
/// locked test's: a live object in either generation is accepted, before and
/// after a collection that swapped the semi-spaces; null, misaligned and
/// out-of-heap addresses are not.
#[test]
fn is_heap_addr_agrees_with_the_arenas_across_a_swap() {
    let h = GenerationalHeap::with_capacity(8 * 1024 * 1024);
    let young = h.alloc_object(ClassId::new(1), 2);
    let old = h.try_alloc_objects_old_batch(ClassId::new(2), 2, 1);
    assert_eq!(old.len(), 1);
    let addr = |o: cratonvm_types::ObjectRef| o.as_ptr() as usize;
    assert_eq!(h.is_heap_addr(addr(young)).map(addr), Some(addr(young)));
    assert_eq!(h.is_heap_addr(addr(old[0])).map(addr), Some(addr(old[0])));
    assert!(h.is_heap_addr(0).is_none());
    assert!(h.is_heap_addr(addr(young) + 1).is_none(), "misaligned");
    let local = 0u64;
    assert!(
        h.is_heap_addr(std::ptr::addr_of!(local) as usize).is_none(),
        "a stack address is not a heap address"
    );

    let mut roots = vec![young, old[0]];
    let _ = h.collect_garbage(&stw(), &mut roots, &NoMonitors);
    for r in &roots {
        assert_eq!(
            h.is_heap_addr(addr(*r)).map(addr),
            Some(addr(*r)),
            "a survivor's post-collection address"
        );
    }
}

/// gen r4w6/review6: `Eden Space` and `Survivor Space` report the semi-space
/// size the heap was built with as `MemoryUsage.getInit()` (they reported
/// `-1`), never above their `max` (`new MemoryUsage` throws on `init > max`).
/// `gengc-r4w2-obs-jmx-residuals-on-the-generational-backend` item 4.
///
/// gen r5w2/obs6: only `Eden Space` now — `Survivor Space` is the empty pool
/// (`init = 0`) — and `Tenured Gen`'s init is the `-Xms` prefix the old
/// generation committed at startup (none here), no longer its reservation.
#[test]
fn young_pools_report_their_initial_size() {
    let semi = 64 * 1024;
    let h = GenerationalHeap::with_sizes(semi, 128 * 1024);
    let pools = h.jmx_memory_pools();
    let eden = &pools[0];
    assert_eq!(eden.init, Some(semi as u64), "{}", eden.name);
    if let Some(max) = eden.max {
        assert!(semi as u64 <= max, "{}: init {semi} > max {max}", eden.name);
    }
    assert_eq!(pools[1].init, Some(0), "Survivor Space is the empty pool");
    let tenured = &pools[2];
    let init = tenured.init.expect("Tenured Gen's init is recorded");
    assert!(
        init <= tenured.max.expect("Tenured Gen has a max"),
        "Tenured Gen: init {init} above max {:?}",
        tenured.max
    );
    assert!(
        init < 128 * 1024,
        "Tenured Gen's init is the -Xms prefix (none was committed), not the \
         128 KiB reservation: {init}"
    );
}

/// `Runtime.maxMemory()` is HotSpot Serial's `max_capacity()`: `-Xmx` less
/// the copy reserve, and the Eden and Tenured pools' `max` sum to it (the
/// Survivor pool's is undefined). gen r5w2/obs6,
/// `gengc-r4w3-hunter-runtime-memory-counts-the-copy-reserve`.
///
/// `with_capacity(8 MiB)`: two 2 MiB semi-spaces and a 4 MiB old generation,
/// so the usable maximum is one semi-space plus the old generation, 6 MiB.
#[test]
fn max_usable_excludes_the_copy_reserve_and_the_pool_maxes_sum_to_it() {
    const M: usize = 1024 * 1024;
    let heap = VmHeap::Generational(GenerationalHeap::with_capacity(8 * M));
    let VmHeap::Generational(h) = &heap else {
        unreachable!("constructed as Generational")
    };
    let max = heap
        .max_usable_bytes()
        .expect("an -Xmx-budgeted Generational heap knows its ceiling");
    assert_eq!(Some(max), h.max_usable_heap_bytes());
    assert_eq!(
        max,
        h.young_semi_capacity() + h.old_gen_capacity(),
        "maxMemory is one semi-space plus the old generation: -Xmx less the \
         copy reserve"
    );
    assert!(max < 8 * M, "maxMemory {max} must be below -Xmx");
    let pools = h.jmx_memory_pools();
    let defined_max: u64 = pools.iter().filter_map(|p| p.max).sum();
    assert_eq!(defined_max, max as u64, "the pools' defined max sum to maxMemory");
    assert!(
        (heap.committed_bytes() as u64) <= max as u64,
        "totalMemory <= maxMemory"
    );
    for p in &pools {
        if let Some(m) = p.max {
            assert!(p.committed <= m, "{}: committed {} > max {m}", p.name, p.committed);
        }
    }
}

/// `Tenured Gen` reports the old generation's COMMITTED size, not its
/// reservation: it rises as old-generation allocation commits granules and
/// equals `old_gen_committed_capacity()` on the reserving store. gen
/// r5w2/obs6, `gengc-r5w1-oldgen5-tenured-pool-reports-the-reservation-as-committed`.
///
/// Skipped on the wholly committed fallback (`CRATONVM_GC_RESERVE=0`, or a
/// platform that refused the reservation), where every byte is committed from
/// construction and the pool reports the touched prefix instead.
#[test]
fn tenured_pool_committed_is_the_committed_old_generation() {
    const M: usize = 1024 * 1024;
    let h = GenerationalHeap::with_capacity(64 * M);
    let reserving = {
        let og = h.old_gen_lock();
        og.committed_bytes() < og.capacity()
    };
    if !reserving {
        return;
    }
    let before = h.jmx_memory_pools()[2];
    assert!(
        before.committed < before.max.expect("Tenured Gen has a max"),
        "a fresh reserving old generation has not committed its reservation: {before:?}"
    );
    assert_eq!(before.committed, h.old_gen_committed_capacity() as u64);
    // ~6 MiB of old-generation objects: more than one 2 MiB granule.
    let mut placed = 0usize;
    for _ in 0..64 {
        placed += h.try_alloc_objects_old_batch(ClassId::new(2), 256, 48).len();
    }
    assert!(placed > 0, "fixture: the old-generation batch must allocate");
    let after = h.jmx_memory_pools()[2];
    assert!(
        after.committed > before.committed,
        "Tenured Gen committed must rise as the old generation commits \
         (before {before:?}, after {after:?})"
    );
    assert_eq!(after.committed, h.old_gen_committed_capacity() as u64);
    assert!(after.used <= after.committed);
}

/// gen r5w3/obs7 (`gengc-r5w2-obs6-proposal-survivor-pool-reports-the-last-cycles-survivors`):
/// right after a young collection `Survivor Space` holds what survived and
/// `Eden Space` nothing, as on HotSpot Serial; allocation afterwards lands in
/// `Eden Space` while `Survivor Space` stays put; eden's collection usage is
/// 0 and survivor's is the survivors; `Tenured Gen`'s collection usage does
/// not move on a young-only collection. The pools still sum to the heap's
/// committed and live bytes.
#[test]
fn survivor_pool_reports_the_last_collections_survivors() {
    let heap = VmHeap::Generational(GenerationalHeap::with_capacity(8 * 1024 * 1024));
    let VmHeap::Generational(h) = &heap else {
        unreachable!("constructed as Generational")
    };
    let mut roots = Vec::new();
    for i in 0..32 {
        let obj = h.alloc_object(ClassId::new(1), 4);
        if i % 2 == 0 {
            roots.push(obj);
        }
    }
    let before = h.gc_pause_totals().0;
    let _ = h.collect_garbage(&stw(), &mut roots, &NoMonitors);
    if h.gc_pause_totals().0 == before {
        // A refused cycle recorded nothing; there is no split to check.
        return;
    }
    // `request_major_gc` is process-global and a sibling test in this binary
    // arms it; a cycle that turned into a full one may have tenured the
    // rooted objects, so the young-only assertions are skipped then.
    let old_reclaimed = cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle();
    let pools = h.jmx_memory_pools();
    let (eden, survivor, tenured) = (pools[0], pools[1], pools[2]);
    assert_eq!(eden.used, 0, "nothing was allocated since the collection: {eden:?}");
    assert_eq!(survivor.collection_used, Some(survivor.used));
    assert_eq!(eden.collection_used, Some(0), "eden is empty after a collection");
    assert_eq!(survivor.committed, survivor.used);
    if !old_reclaimed {
        assert!(survivor.used > 0, "the rooted objects survived: {survivor:?}");
        assert_eq!(
            tenured.collection_used,
            Some(0),
            "a young-only collection leaves Tenured Gen's collection usage alone"
        );
    }
    let committed: u64 = pools.iter().map(|p| p.committed).sum();
    assert_eq!(committed, heap.committed_bytes() as u64);
    let used: u64 = pools.iter().map(|p| p.used).sum();
    assert_eq!(used, h.live_bytes_estimate() as u64);

    // Allocation after the collection is eden's; the survivors stay put.
    for _ in 0..16 {
        let _ = h.alloc_object(ClassId::new(1), 4);
    }
    let pools = h.jmx_memory_pools();
    assert_eq!(
        pools[1].used,
        survivor.used,
        "survivor is fixed until the next collection"
    );
    assert!(pools[0].used > 0, "new allocation is eden's");
    let used: u64 = pools.iter().map(|p| p.used).sum();
    assert_eq!(used, h.live_bytes_estimate() as u64);
    for p in &pools {
        assert!(
            p.used <= p.committed,
            "{}: used {} > committed {}",
            p.name,
            p.used,
            p.committed
        );
    }
}
