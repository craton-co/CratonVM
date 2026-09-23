// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E, wave 2 (2026-09-20) — G1 has a resizing POLICY now, and it does not
//! oscillate.
//!
//! `docs/internal/g1-2026-09-20/lane-e-heap-never-shrinks-and-never-resizes-to-occupancy.md`
//! recorded three things: nothing widened the heap because the collector was
//! running too often, the shrink had one production caller most workloads never
//! reach (`cleanup`), and `heap_capacity()` reported `-Xmx` rather than a
//! capacity. This file is the regression for all three.
//!
//! # What these tests are careful about
//!
//! The brief for this wave says it in one line: *a heap that shrinks under a
//! live program and then has to grow again is worse than one that never
//! shrinks*. So the property under test is not "it shrinks" — that is easy and
//! it is the failure mode. It is **stability**: apply the policy to its own
//! output and it must reach a fixed point. `the_policy_reaches_a_fixed_point`
//! is the whole of that claim, iterated, over a grid of starting states.
//!
//! Everything here drives the `_within` forms, which take the opt-in as a
//! parameter. `CRATONVM_G1_HEAP_RESIZE` is latched once per process from a
//! flag snapshot taken before `main`, so a test that set the environment
//! variable would exercise the arm that does nothing.

use cratonvm_gc::g1::g1_dbg_heap_resize_decision;
use cratonvm_gc::{G1Collector, G1CollectorConfig, GarbageCollector};

/// 64 regions of 64 KiB, with `-Xms` at four regions so the floor is low
/// enough for a shrink to have somewhere to go.
fn heap_64_regions() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 64 * 1024,
        initial_heap_size: 4 * 64 * 1024,
        region_size: 64 * 1024,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// The policy, as a pure function
// ---------------------------------------------------------------------------

#[test]
fn a_heap_collecting_too_often_grows() {
    // 10 % GC overhead, comfortable occupancy: the growth rule the finding says
    // does not exist. 20 regions used of a 64-region target is 31 %, which is
    // inside the shrink band on occupancy alone — so this test also proves the
    // overhead axis can grow the heap on its own.
    let (dir, next) = g1_dbg_heap_resize_decision(40, 12, 4, 64, 100_000, 0);
    assert_eq!(dir, 1, "10% GC overhead must grow the heap");
    assert!(next > 40 && next <= 64, "grew to {next}");
}

#[test]
fn a_full_heap_grows_even_when_gc_is_cheap() {
    // Occupancy is the other growth axis: a heap at 90 % of its soft capacity
    // is about to start collecting too often, and waiting for the overhead
    // figure to say so costs a full GC interval to discover.
    let (dir, next) = g1_dbg_heap_resize_decision(40, 36, 4, 64, 0, 0);
    assert_eq!(dir, 1, "90% occupancy must grow the heap");
    assert!(next > 40);
}

#[test]
fn a_quiet_empty_heap_shrinks_but_only_after_the_streak() {
    // 8 of 40 regions used (20 %), GC overhead 0.1 %. Inside the shrink band on
    // both axes — but a single quiet pause must not give memory back, because a
    // burst that has just ended looks exactly like a program that never needed
    // the memory.
    for streak in 0..4u32 {
        let (dir, _) = g1_dbg_heap_resize_decision(40, 8, 4, 64, 1_000, streak);
        assert_eq!(dir, 0, "streak {streak} is below the threshold; must hold");
    }
    let (dir, next) = g1_dbg_heap_resize_decision(40, 8, 4, 64, 1_000, 4);
    assert_eq!(dir, -1, "four consecutive quiet pauses may shrink");
    assert!(next < 40, "shrank to {next}");
    assert!(
        next >= 4,
        "must never go below -Xms (4 regions), got {next}"
    );
}

#[test]
fn a_quiet_heap_that_is_still_busy_collecting_does_not_shrink() {
    // Occupancy says "empty", overhead says "this program is collecting 3 % of
    // the time". The gap between the grow threshold (5 %) and the shrink
    // threshold (1 %) is the hysteresis on the overhead axis, and a heap that
    // is collecting 3 % of the time is doing its job, not wasting memory.
    let (dir, _) = g1_dbg_heap_resize_decision(40, 8, 4, 64, 30_000, 99);
    assert_eq!(dir, 0);
}

#[test]
fn the_floor_and_the_ceiling_are_absolute() {
    // At `-Xms`, the quietest possible heap holds.
    let (dir, next) = g1_dbg_heap_resize_decision(4, 0, 4, 64, 0, 99);
    assert_eq!(dir, 0, "at -Xms with nothing live, must still hold");
    assert_eq!(next, 4);

    // At `-Xmx`, the busiest possible heap holds.
    let (dir, next) = g1_dbg_heap_resize_decision(64, 64, 4, 64, 1_000_000, 0);
    assert_eq!(
        dir, 0,
        "at -Xmx with 100% overhead, there is nothing to grow"
    );
    assert_eq!(next, 64);
}

#[test]
fn the_policy_reaches_a_fixed_point() {
    // THE test on this page. A sizing policy that oscillates is worse than no
    // sizing policy: it pays the shrink's decommit and the grow's page faults
    // forever and converges on nothing. So: hold the workload constant, apply
    // the policy to its own output, and require that it stops.
    //
    // The streak is held at its maximum throughout, which is the WORST case for
    // stability — it removes the time-axis hysteresis entirely and leaves only
    // the dead band and the settle point to do the work. If it converges here
    // it converges with the streak too.
    for used in [0usize, 1, 5, 12, 20, 33, 50, 63, 64] {
        for overhead in [0u64, 5_000, 30_000, 60_000, 200_000] {
            let mut target = 64usize;
            let mut seen = vec![target];
            for step in 0..64 {
                let (dir, next) =
                    g1_dbg_heap_resize_decision(target, used, 4, 64, overhead, u32::MAX);
                if dir == 0 {
                    break;
                }
                assert!(
                    !seen.contains(&next),
                    "used={used} overhead={overhead}: the policy revisited target {next} \
                     at step {step} — that is a cycle, i.e. oscillation. Trace: {seen:?}"
                );
                seen.push(next);
                target = next;
                assert!(
                    step < 63,
                    "used={used} overhead={overhead}: no fixed point in 64 steps ({seen:?})"
                );
            }
        }
    }
}

#[test]
fn a_grow_triggered_by_occupancy_reaches_the_band_in_one_step() {
    // The anti-storm rule on the growth side. `needs_gc` measures the free
    // fraction against the soft capacity, so a live set that has outgrown the
    // capacity fires the trigger on every allocation. A fixed 20 % step would
    // need eleven pauses to climb from 6 regions to the ~33 that put a 20-region
    // live set back inside the band — eleven real collections nobody asked for.
    // Growth therefore jumps at least to the settle point.
    let (dir, next) = g1_dbg_heap_resize_decision(6, 20, 4, 64, 0, 0);
    assert_eq!(dir, 1);
    assert!(
        20 * 100 / next < 70,
        "one grow left occupancy at {}%, still in the growth band — the next          allocation would fire the trigger again",
        20 * 100 / next
    );
}

#[test]
fn a_shrink_lands_inside_the_band_it_was_shrinking_out_of() {
    // The settle point is why the fixed-point test above passes. A shrink that
    // stopped the moment it left the low band would land ON the edge of the
    // grow band, and the very next pause would grow it back. So: after one
    // shrink, the resulting occupancy must be strictly below the grow
    // threshold.
    let used = 8usize;
    let (dir, next) = g1_dbg_heap_resize_decision(64, used, 4, 64, 0, 99);
    assert_eq!(dir, -1);
    let occupancy_pct = used * 100 / next;
    assert!(
        occupancy_pct < 70,
        "a shrink landed at {occupancy_pct}% occupancy, which is at or above the \
         growth threshold — the next pause would undo it"
    );
}

// ---------------------------------------------------------------------------
// The policy, wired into a collector
// ---------------------------------------------------------------------------

#[test]
fn the_policy_is_inert_unless_it_is_armed() {
    let gc = heap_64_regions();
    // A census that screams "shrink": 60 of 64 regions Free, nothing above
    // region 4, and a GC-overhead figure of zero.
    gc.dbg_set_gc_overhead_ppm(0);
    for _ in 0..16 {
        assert!(
            gc.dbg_apply_heap_resize(60, 64, 4, false).is_none(),
            "with CRATONVM_G1_HEAP_RESIZE unset the policy must decide nothing"
        );
    }
    assert_eq!(
        gc.heap_target_regions(),
        64,
        "the soft capacity must read as the whole region grid when the flag is off"
    );
    let (grows, shrinks, uncommitted, _) = gc.heap_resize_counts();
    assert_eq!((grows, shrinks, uncommitted), (0, 0, 0));
}

#[test]
fn an_armed_policy_shrinks_a_quiet_heap_and_then_stops() {
    let gc = heap_64_regions();
    gc.dbg_set_gc_overhead_ppm(0);

    // Pauses' worth of "the heap is nearly empty and GC is nearly free". The
    // first four build the streak; the rest shrink by a tenth each until the
    // settle point stops them, which from 64 regions down to the six that hold
    // four live regions at ~60 % occupancy takes a few dozen steps.
    let mut shrinks = 0;
    for _ in 0..400 {
        if let Some((dir, _, _)) = gc.dbg_apply_heap_resize(60, 64, 4, true) {
            assert_eq!(dir, "shrink", "a quiet empty heap must never grow");
            shrinks += 1;
        }
    }
    assert!(shrinks > 0, "an armed policy must eventually shrink");

    let (target, floor, ceiling, _) = gc.dbg_heap_target_state();
    assert!(target < ceiling, "the soft capacity must have fallen");
    assert!(target >= floor, "and must never fall below -Xms");

    // And then STOP. More identical pauses must not keep shrinking forever —
    // the settle point holds it where four live regions sit at ~60 % occupancy,
    // which is inside the dead band and therefore not a shrink candidate at
    // all. This assertion IS the anti-oscillation claim, on a live collector.
    let before = gc.dbg_heap_target_state().0;
    for _ in 0..400 {
        gc.dbg_apply_heap_resize(60, 64, 4, true);
    }
    let after = gc.dbg_heap_target_state().0;
    assert_eq!(
        before, after,
        "the policy kept shrinking a heap it had already sized: {before} -> {after}"
    );
}

#[test]
fn an_armed_policy_grows_a_heap_that_is_collecting_too_often() {
    let gc = heap_64_regions();
    // First drive it down, so there is room to grow back.
    gc.dbg_set_gc_overhead_ppm(0);
    for _ in 0..400 {
        gc.dbg_apply_heap_resize(60, 64, 4, true);
    }
    let shrunk = gc.dbg_heap_target_state().0;
    assert!(shrunk < 64);

    // Now the program wakes up: 12 % of wall clock in GC.
    gc.dbg_set_gc_overhead_ppm(120_000);
    let mut grew = false;
    for _ in 0..24 {
        if let Some((dir, _, _)) = gc.dbg_apply_heap_resize(40, 64, 24, true) {
            assert_eq!(dir, "grow");
            grew = true;
        }
    }
    assert!(grew, "a heap at 12% GC overhead must widen");
    assert!(
        gc.dbg_heap_target_state().0 > shrunk,
        "and the soft capacity must actually be larger than it was"
    );
    let (grows, _, _, _) = gc.heap_resize_counts();
    assert!(grows > 0, "and the run total must record it");
}

#[test]
fn a_shrink_gives_pages_back_when_the_shrink_is_safe() {
    // The half of the finding that says the shrink was unreachable: it now runs
    // from the per-pause path, not only from `cleanup`.
    //
    // Skipped rather than failed on a platform (or a `CRATONVM_G1_RESERVE_HEAP=0`
    // environment) where the heap is not a real reservation — there is nothing
    // to decommit there and `heap_is_reserved()` says so, which is exactly why
    // it is a query and not an assumption.
    let gc = heap_64_regions();
    if !gc.heap_is_reserved() {
        // A legitimate platform gate, said out loud rather than returning
        // silently: on a build or OS with no reservation implementation (or
        // under `CRATONVM_G1_RESERVE_HEAP=0`) the whole arena is committed up
        // front and there is nothing to decommit. A quiet `return` here would
        // make this test green on a machine where it can never run.
        eprintln!("SKIPPED: this heap is not a reservation, so nothing can be decommitted");
        return;
    }
    // Grow the committed prefix by claiming regions. `-Xms` is four regions, so
    // this has to allocate well past 256 KiB or there is nothing above the
    // floor to give back — which is exactly what the first version of this test
    // failed to do, and what the unconditional assertion below caught.
    for _ in 0..40_000 {
        let _ = gc.alloc_object(cratonvm_types::ClassId::new(1), 4);
    }
    let committed_before = gc.committed_bytes();
    assert!(
        committed_before > 8 * 64 * 1024,
        "the fixture did not grow the committed prefix past -Xms ({} bytes), so          there is nothing for a shrink to release and this test would pass on a          collector that cannot shrink at all",
        committed_before
    );

    // Now simulate what Phase 5 does to a heap whose live set has collapsed:
    // everything above region 3 becomes Free. Written straight to the table
    // rather than grown through a pause because the census this test hands
    // `dbg_apply_heap_resize` MUST agree with the real region table — a
    // synthetic census claiming regions are Free while Eden still occupies them
    // would have the shrink unmap live memory, which is the one thing the
    // safety argument here is about.
    const KEEP: usize = 4;
    gc.with_regions_mut(|regions| {
        for r in regions.iter_mut().skip(KEEP) {
            r.set_region_type(cratonvm_gc::region::RegionType::Free);
            r.set_cursor(0);
        }
    });
    let free_now = gc.count_regions(cratonvm_gc::region::RegionType::Free) as u32;
    let total = 64u32;
    assert_eq!(free_now, total - KEEP as u32);

    gc.dbg_set_gc_overhead_ppm(0);
    let mut released_total = 0usize;
    for _ in 0..400 {
        if let Some((_, _, released)) = gc.dbg_apply_heap_resize(free_now, total, KEEP as u32, true)
        {
            released_total += released;
        }
    }
    assert!(
        gc.committed_bytes() <= committed_before,
        "the committed prefix must never grow as a result of a shrink"
    );
    // Asserted, not guarded by `if released_total > 0`. The whole point of the
    // finding is that the shrink had one production caller most workloads never
    // reach, so a test that tolerates "nothing was released" would pass on the
    // broken world it exists to rule out.
    assert!(
        released_total > 0,
        "a quiet, nearly-empty heap released no pages at all across 400 pauses          (committed {committed_before} before) — the pause-driven shrink is not          reaching `uncommit_to_bytes`"
    );
    assert!(
        gc.committed_bytes() < committed_before,
        "bytes were reported released but the committed prefix did not fall"
    );
    assert!(
        gc.committed_bytes() >= 4 * 64 * 1024,
        "the shrink went below -Xms"
    );
    eprintln!(
        "pause-driven shrink: committed {committed_before} -> {} ({released_total} released)",
        gc.committed_bytes()
    );
}

// ---------------------------------------------------------------------------
// Small finding 6 — `heap_capacity()` over-reported by up to `region_size - 1`
// ---------------------------------------------------------------------------

#[test]
fn heap_capacity_reports_the_region_grid_and_not_xmx() {
    // `num_regions = heap_size / region_size` TRUNCATES, so a `-Xmx` that is
    // not a whole number of regions describes memory the collector can never
    // hand out. `VmHeap` builds `Runtime.freeMemory()` out of this number.
    let region_size = 64 * 1024usize;
    let gc = G1Collector::new(G1CollectorConfig {
        // Ten regions and a half.
        heap_size: 10 * region_size + region_size / 2,
        initial_heap_size: 2 * region_size,
        region_size,
        ..Default::default()
    });
    assert_eq!(
        gc.heap_capacity(),
        10 * region_size,
        "heap_capacity must be the region grid, not -Xmx"
    );
    assert!(
        gc.heap_capacity() <= gc.reserved_bytes(),
        "a capacity larger than the reservation is not a capacity"
    );
}
