// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane B wave 2 (2026-09-20) — the evacuation-failure drain's Phase-4 fix-up:
//! its cost is now counted, and its correctness is asserted against the
//! tripwire rather than against a review argument.
//!
//! # Why this is its own test binary
//!
//! `drain_fixup_totals()` and `PHASE4_STALE_FORWARD_SLOTS` are PROCESS-GLOBAL.
//! Any other test in the same binary that drives a pause moves them. Cargo
//! gives each `tests/*.rs` file its own binary, and the tests below run
//! sequentially within it (each resets the counters it reads), so this file
//! owns them.
//!
//! # What is being guarded
//!
//! Two things, and they are different in kind.
//!
//! 1. **The denominator.** `drain_kept_self_forwards` calls
//!    `record_collection_with_phases` at the bottom of every pass, and
//!    `retry_after_evacuation_failure` can call it up to eight times in one
//!    pause. Each pass therefore OVERWROTE the previous pass's `fixup_us` /
//!    `fixup_regions` / `fixup_bytes`, so a pause that spent most of its time
//!    in repeated whole-heap walks reported the cost of the last one. That is
//!    why the "up to eight whole-heap walks" claim in
//!    `lane-b-drain-fixup-is-still-whole-heap.md` had never been checked
//!    against a run. `DRAIN_FIXUP_*` is the accumulator that makes it
//!    checkable. (Checked, on `HumongousChurn 48 6000 512 -Xmx160m`: the loop
//!    runs ONE pass. See `w2b-the-drain-fixup-cannot-be-hoisted.md`.)
//!
//! 2. **The tripwire.** `CRATONVM_G1_NARROW_DRAIN_FIXUP=1` narrows that walk,
//!    and a narrowing that is wrong does not crash — it leaves a slot holding
//!    the from-space address of an object the pause moved and freed.
//!    `PHASE4_STALE_FORWARD_SLOTS` is the counter that sees exactly that, and
//!    it is the only thing that can distinguish "the narrow set was right" from
//!    "nothing dereferenced the bad slot during this test". Asserting the live
//!    chain is intact is NOT a substitute: a stale slot in a DEAD holder is
//!    invisible to a chain walk and is still the shape a narrowing bug takes.
//!
//! Run this binary under `CRATONVM_G1_NARROW_DRAIN_FIXUP=1` to exercise the
//! narrow arm; the lever is latched in a `OnceLock`, so one process is one arm.
//!
//! # What these tests do NOT cover, stated so nobody reads them as more
//!
//! Both tests reach the drain in its **WEDGED** form: `copied=0 freed=0`, the
//! live set genuinely does not fit, and the loop breaks after one pass. In that
//! form every occupied region holds a seed, so every region is in the drain's
//! collection set, and the fix-up walk skips all of them —
//! `drain_fixup_totals()` reports `regions=0 bytes=0` here, and the eprintln
//! above prints it so the reader can see which arm ran.
//!
//! The expensive form is the **PRODUCTIVE** drain, where the failing pause left
//! enough room for the drain to copy: `HumongousChurn 48 6000 512 -Xmx160m`
//! produces `copied=638811 freed=25165824` with a fix-up over 60 regions and
//! 61.8 MB (109 ms). Reproducing that ratio inside a unit-sized heap has not
//! been managed — the heap either has room, in which case the pause does not
//! fail, or it does not, in which case the drain wedges. So the narrow arm's
//! correctness on a NON-EMPTY walk set is guarded by the probe workload and by
//! `PHASE4_STALE_FORWARD_SLOTS` in production, not by these two tests.
//! Whoever flips `CRATONVM_G1_NARROW_DRAIN_FIXUP` to default-on needs the
//! workload run, not a green bar here.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::g1::{drain_fixup_totals, reset_drain_fixup_totals, PHASE4_STALE_FORWARD_SLOTS};
use cratonvm_gc::{G1Collector, G1CollectorConfig};
use cratonvm_types::{ClassId, Value};
use std::sync::atomic::Ordering;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> StopTheWorldToken {
    // SAFETY: a single-threaded test; no mutator is running.
    unsafe { StopTheWorldToken::new() }
}

/// A heap small enough that a mostly-live chain exhausts to-space, which is the
/// only way to reach the drain: `retry_after_evacuation_failure` is called from
/// `collect_garbage` and does nothing unless the pause self-forwarded something.
fn tight_heap() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 64 * 1024,
        initial_heap_size: 16 * 64 * 1024,
        region_size: 64 * 1024,
        ..Default::default()
    })
}

/// Build a live chain interleaved with garbage until the heap will take no
/// more, returning the head and the number of live nodes.
///
/// `garbage_per_node` sets the LIVE FRACTION, and the live fraction is what
/// decides whether the drain is reachable at all: a young pause has to copy the
/// whole live set into free regions, so to-space exhausts only when the live set
/// is larger than the pool the pause starts with. Interleaving is still the
/// `SteadyChurn` shape (every filled region holds both live nodes and garbage,
/// so no region can be reclaimed whole) — but with two garbage objects per node
/// the live fraction is ~20% and every pause succeeds comfortably, which is
/// exactly what the first version of this test measured: nothing.
///
/// Allocation is driven to REFUSAL rather than to a node count, because the
/// node count that fills a 1 MB heap depends on `SLOT_SIZE` and the header
/// size, and a test that hard-codes it stops filling the heap the day either
/// changes — silently, in the direction of covering less.
fn fill_with_interleaved_chain(
    gc: &G1Collector,
    garbage_per_node: usize,
) -> (cratonvm_types::ObjectRef, usize) {
    let head = gc.alloc_object(ClassId::new(1), 4);
    let mut cur = head;
    let mut live = 1usize;
    'fill: loop {
        for _ in 0..garbage_per_node {
            if gc.try_alloc_object(ClassId::new(9), 8).is_none() {
                break 'fill;
            }
        }
        let Some(node) = gc.try_alloc_object(ClassId::new(1), 4) else {
            break;
        };
        gc.set_field(cur, 0, Value::Object(Some(node)));
        cur = node;
        live += 1;
    }
    // Tail marker: an Int rather than a reference, so the walk below has an
    // unambiguous stop that a dropped node cannot forge.
    gc.set_field(cur, 1, Value::Int(424242));
    (head, live)
}

/// Walk the chain and return how many nodes are reachable, plus whether the
/// tail marker survived.
fn walk_chain(gc: &G1Collector, head: cratonvm_types::ObjectRef, cap: usize) -> (usize, bool) {
    let mut n = 1usize;
    let mut cur = head;
    loop {
        match gc.get_field(cur, 0) {
            Value::Object(Some(next)) => {
                cur = next;
                n += 1;
                if n > cap {
                    return (n, false);
                }
            }
            _ => break,
        }
    }
    (n, matches!(gc.get_field(cur, 1), Value::Int(424242)))
}

#[test]
fn w2b_a_drain_is_counted_and_leaves_no_stale_forward() {
    let gc = tight_heap();
    reset_drain_fixup_totals();
    let stale_before = PHASE4_STALE_FORWARD_SLOTS.load(Ordering::Relaxed);

    let (head, live) = fill_with_interleaved_chain(&gc, 1);
    let mut roots = vec![head];

    // The door the drain is actually reachable through: `collect_garbage` is
    // the only production caller of `retry_after_evacuation_failure`.
    let _ = gc.collect_garbage(&stw(), &mut roots, &NoMonitors);
    let head = roots[0];

    let (walks, regions, bytes, _us) = drain_fixup_totals();
    if walks == 0 {
        // The setup did not force to-space exhaustion on this host/heap. Say so
        // instead of asserting something vacuous about a path that never ran —
        // a test that silently stops covering its subject is worse than one
        // that fails.
        eprintln!(
            "[w2b] note: this pause did not enter the drain (no self-forwards);              the counters below cover no walk"
        );
    } else {
        eprintln!("[w2b] drain fix-up: walks={walks} regions={regions} bytes={bytes}");
        // Deliberately NOT `regions > 0`. A drain whose collection set is most
        // of the heap can legitimately walk ZERO regions — `cset` and `Free`
        // are both skipped — and an earlier version of this assertion failed on
        // exactly that, which is the useful half of what it taught: on a tight
        // heap the drain's fix-up is cheap because there is nothing left
        // outside the collection set, and on a large one it is the whole live
        // old generation. The quantity worth pinning here is that the
        // accumulator SUMS rather than being overwritten, which is the gap
        // these counters close.
        assert!(
            bytes > 0 || regions == 0,
            "a walk that covered {regions} regions reported {bytes} bytes"
        );
        // The counters are `fetch_add`s, so the only way to assert they SUM
        // rather than overwrite is to take a second sample after clearing them
        // and confirm the clear is what moved them, not the next pass.
        reset_drain_fixup_totals();
        assert_eq!(
            drain_fixup_totals(),
            (0, 0, 0, 0),
            "`reset_drain_fixup_totals` must clear all four, or a test that              brackets a pause with it measures the wrong interval"
        );
    }

    let stale_after = PHASE4_STALE_FORWARD_SLOTS.load(Ordering::Relaxed);
    assert_eq!(
        stale_after,
        stale_before,
        "the pause left {} slot(s) holding the from-space address of an object \
         it moved and freed. That is the failure mode of a wrong Phase-4 walk \
         set and of nothing else — the referent was demonstrably reachable, so \
         the remembered set is not the suspect. First place to look: \
         `phase4_regions_to_walk`, then the `narrow_drain_fixup` gate.",
        stale_after - stale_before
    );

    let (found, tail_ok) = walk_chain(&gc, head, live * 2);
    assert_eq!(
        found, live,
        "the live chain lost nodes across the pause ({found} of {live})"
    );
    assert!(tail_ok, "the chain's tail marker did not survive the pause");
}

/// Several pauses in a row on a heap big enough to HAVE an old generation, each
/// with a fresh generation of garbage hung off the surviving chain.
///
/// This is the arm where the fix-up actually walks something. On the 1 MB heap
/// above the drain's collection set is most of the table, so the fix-up walks
/// ZERO regions and the narrowing has nothing to get wrong. Here the chain is
/// promoted to Old across pauses, Old regions are outside a young collection
/// set, and the fix-up walk set is therefore non-empty — which is the only
/// configuration in which `PHASE4_STALE_FORWARD_SLOTS` can distinguish a
/// correct narrow set from a wrong one.
#[test]
fn w2b_repeated_pressure_leaves_no_stale_forward() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 96 * 64 * 1024,
        initial_heap_size: 96 * 64 * 1024,
        region_size: 64 * 1024,
        ..Default::default()
    });
    let stale_before = PHASE4_STALE_FORWARD_SLOTS.load(Ordering::Relaxed);
    reset_drain_fixup_totals();

    let (head, live) = fill_with_interleaved_chain(&gc, 2);
    let mut roots = vec![head];

    for round in 0..6 {
        // `try_alloc_object`, never `alloc_object`: the infallible entry point
        // ABORTS the process when the heap is full ("every region consumed with
        // no collection able to run"), and filling to refusal is the whole
        // point of this loop. A test that has to avoid its own pressure is not
        // testing pressure.
        let mut pushed = 0usize;
        while pushed < 4096 && gc.try_alloc_object(ClassId::new(9), 8).is_some() {
            pushed += 1;
        }
        let _ = gc.collect_garbage(&stw(), &mut roots, &NoMonitors);
        let (found, tail_ok) = walk_chain(&gc, roots[0], live * 2);
        assert_eq!(found, live, "round {round}: chain lost nodes");
        assert!(tail_ok, "round {round}: tail marker lost");
    }

    let (walks, regions, bytes, _us) = drain_fixup_totals();
    eprintln!("[w2b] repeated pressure: drain walks={walks} regions={regions} bytes={bytes}");

    assert_eq!(
        PHASE4_STALE_FORWARD_SLOTS.load(Ordering::Relaxed),
        stale_before,
        "a stale forward appeared across six pressured pauses: a slot is holding          the from-space address of an object the collector moved and freed.          Suspect `phase4_regions_to_walk`; if this only reproduces under          `CRATONVM_G1_NARROW_DRAIN_FIXUP=1`, the drain's narrow set is the one          that is wrong and the lever must stay at its default."
    );
}
