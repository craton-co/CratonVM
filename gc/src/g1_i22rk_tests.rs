// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i22/g1rk -- `G1Collector::remark_liveness` (item 7 of
//! `gch-h4g1-g1-review-findings-FIXED-20261017`: the remark's liveness questions
//! without the snapshot mutex per call) and `trusted_mark_liveness` (the
//! post-cleanup verdict the mark-exact address-keyed sweep of
//! `gch-h4g1-proposal-g1-remark-sweeps-every-address-keyed-table-mark-exact-20261009`
//! reads, behind `CRATONVM_G1_REMARK_ADDR_KEYED_MARK_EXACT`).
//!
//! A child module of `g1` (see the `#[path]` declaration in `g1.rs`), so the
//! private items are reachable through `super`.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i22_rk_`.

use super::*;
use crate::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_types::{ClassId, ObjectRef};

const REGION: usize = 1024 * 1024;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> crate::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { crate::collector::StopTheWorldToken::new() }
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * REGION,
        initial_heap_size: 8 * REGION,
        region_size: REGION,
        // Promote on the first young pause, as `is_live_after_mark_verdicts`.
        promotion_age: 1,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

/// Two objects promoted to Old by two young pauses: `(live, dead)`.
fn promoted_pair(gc: &G1Collector) -> (ObjectRef, ObjectRef) {
    let live = gc.alloc_object(ClassId::new(0x22A1), 0);
    let dead = gc.alloc_object(ClassId::new(0x22A2), 0);
    let mut roots = vec![live, dead];
    gc.young_collection(&mut roots, &NoMonitors);
    gc.young_collection(&mut roots, &NoMonitors);
    (roots[0], roots[1])
}

fn free_region_base(gc: &G1Collector) -> Option<usize> {
    let regions = gc.regions.read();
    regions
        .iter()
        .find(|r| r.region_type() == RegionType::Free)
        .map(|r| r.data.as_ptr() as usize)
}

/// The view's verdict is `is_live_after_mark`'s, address for address: inside
/// a cycle (marked, unmarked below TAMS, above TAMS, interior, off-heap, a
/// Free region) and outside one (empty snapshot).
#[test]
fn gci_i22_rk_the_view_answers_as_is_live_after_mark() {
    let gc = collector();
    let (live, dead) = promoted_pair(&gc);

    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[live]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    let fresh = gc.alloc_object(ClassId::new(0x22A3), 0);

    let mut probes: Vec<usize> = vec![
        live.as_ptr() as usize,
        dead.as_ptr() as usize,
        fresh.as_ptr() as usize,
        live.as_ptr() as usize + 8,
        0x10,
        usize::MAX - 7,
    ];
    if let Some(base) = free_region_base(&gc) {
        probes.push(base);
    }

    let view = gc.remark_liveness();
    for &addr in &probes {
        assert_eq!(
            view.is_live(addr),
            gc.is_live_after_mark(addr),
            "in a cycle: the view and is_live_after_mark disagree at {addr:#x}"
        );
    }
    assert!(view.is_live(live.as_ptr() as usize), "marked: live");
    assert!(!view.is_live(dead.as_ptr() as usize), "unmarked below TAMS: dead");
    assert!(view.is_live(fresh.as_ptr() as usize), "above TAMS: live");
    assert!(view.is_live(0x10), "off-heap: live (no evidence)");
    drop(view);

    gc.abort_concurrent_mark();
    let after = gc.remark_liveness();
    for &addr in &probes {
        assert_eq!(
            after.is_live(addr),
            gc.is_live_after_mark(addr),
            "after an abort (empty snapshot): disagree at {addr:#x}"
        );
    }
}

/// The view copies the snapshot when it is made: the verdict is the one of
/// the snapshot of that moment, which is why the doors make it inside the
/// pause that asks.
#[test]
fn gci_i22_rk_the_view_reads_the_snapshot_it_was_made_with() {
    let gc = collector();
    let (live, dead) = promoted_pair(&gc);
    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[live]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    let view = gc.remark_liveness();
    assert!(!view.is_live(dead.as_ptr() as usize));
    gc.abort_concurrent_mark();
    // `is_live_after_mark` now answers LIVE (empty snapshot); the view still
    // holds the cycle's rows. Its bitmap read is live, and the abort leaves
    // the bitmap as it was, so its answer is the mark's.
    assert!(gc.is_live_after_mark(dead.as_ptr() as usize));
    assert!(!view.is_live(dead.as_ptr() as usize));
}

/// `trusted_mark_liveness` is `None` before any completed cycle and while
/// one marks; after a trusted `cleanup` it is `Some` and judges an unmarked
/// Old object dead and a marked one live (unless the environment turned the
/// JIT-pin coverage off, when it must stay `None`).
#[test]
fn gci_i22_rk_trusted_view_only_after_a_trusted_cleanup() {
    let gc = collector();
    assert!(
        gc.trusted_mark_liveness().is_none(),
        "no completed cycle: no evidence"
    );
    let (live, dead) = promoted_pair(&gc);
    gc.start_concurrent_mark(&stw());
    assert!(
        gc.trusted_mark_liveness().is_none(),
        "a cycle is open: its bitmap is being built"
    );
    gc.remark(&stw(), &[live]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    gc.cleanup(&stw());

    let covers = mark_proof_covers_jit_pins(g1_mark_root_screen(), g1_remark_seeds_jit_pins());
    let view = gc.trusted_mark_liveness();
    if !covers {
        assert!(view.is_none(), "the mark does not cover JIT pins: no proof");
        return;
    }
    assert!(
        gc.last_mark_trusted.load(Ordering::Acquire),
        "fixture: a drained closure of well-formed objects is trusted"
    );
    let view = view.expect("a completed, trusted mark gives a view");
    assert!(view.is_live(live.as_ptr() as usize), "marked: live");
    assert!(
        !view.is_live(dead.as_ptr() as usize),
        "unmarked below TAMS (or in a region cleanup freed): dead"
    );
    assert!(view.is_live(0x10), "off-heap: live");
}

/// An untrusted cleanup (the fail-safe kept every region) gives no view.
#[test]
fn gci_i22_rk_no_trusted_view_after_an_untrusted_cleanup() {
    let gc = collector();
    let (live, _dead) = promoted_pair(&gc);
    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[live]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    gc.cleanup(&stw());
    gc.last_mark_trusted.store(false, Ordering::Release);
    assert!(gc.trusted_mark_liveness().is_none());
}
