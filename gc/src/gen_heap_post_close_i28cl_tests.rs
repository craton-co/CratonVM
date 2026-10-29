// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i28/cl (2026-10-15) — the heap-level twin of the incremental
//! post-resurrection closure
//! (`gcg-g3e-proposal-post-resurrection-closure-scans-only-what-the-round-marked-20261001`,
//! `CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL`, opt-in): a true-root major with a
//! NON-idle finalizer round (a dead candidate with a two-deep subgraph) keeps
//! and frees exactly what the full post-close arm does, on both
//! `ROUNDS_IN_BFS` arms, and the switch is inert without the idle skip.
//! The closure-level equality twins are in `old_gen.rs` (`gci_i28cl_`).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gci_i28cl_`.

use super::*;

/// Test-only `StopTheWorldToken` (the harness runs the heap single-threaded).
fn stw() -> crate::collector::StopTheWorldToken {
    // SAFETY: these unit tests run the heap single-threaded.
    unsafe { crate::collector::StopTheWorldToken::new() }
}

struct NoMonitors;
impl crate::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const INCREMENTAL: &str = "CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL";

/// The h4/pc fixture's flags (`gen_heap_oldmark_h4pc_tests.rs` `pc_flags`)
/// plus this lane's switch, all pinned explicitly.
fn cl_flags(
    in_bfs: bool,
    skip_post: bool,
    incremental: bool,
) -> [(&'static str, Option<&'static str>); 8] {
    let on = |b: bool| Some(if b { "1" } else { "0" });
    [
        ("CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS", on(in_bfs)),
        ("CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE", on(skip_post)),
        (INCREMENTAL, on(incremental)),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD", Some("0")),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS", None),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE", None),
        ("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG", None),
        ("CRATONVM_GC_REQUESTED_MAJOR_COMPACTS", Some("0")),
    ]
}

/// `n` tenured objects of two slots each, on a fresh heap.
fn tenured(n: usize) -> (GenerationalHeap, Vec<ObjectRef>) {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024).with_fixed_promotion_age();
    let monitors = NoMonitors;
    let mut roots: Vec<ObjectRef> = (0..n)
        .map(|_| heap.alloc_object(ClassId::new(0), 2))
        .collect();
    for _ in 0..PROMOTION_AGE {
        heap.collect_garbage(&stw(), &mut roots, &monitors);
    }
    assert!(
        roots.iter().all(|r| heap.is_in_old(r.as_ptr())),
        "precondition: tenured"
    );
    (heap, roots)
}

/// -> (alive per fixture object, resurrected indices, depth-2 closure indices
/// sorted, true-root majors). Fixture: `live` (the root, a live candidate),
/// `dead` (an unreachable candidate) -> `child` -> `grand` -> `live` (a back
/// edge into the strong set), and garbage `g1` -> `g2` -> `child` (unmarked
/// referrers of the resurrected subgraph keep nothing).
fn run(in_bfs: bool, skip: bool, incremental: bool) -> (Vec<bool>, Vec<usize>, Vec<usize>, u64) {
    let (heap, old) = tenured(6);
    let (live, dead, child, grand, g1, g2) = (old[0], old[1], old[2], old[3], old[4], old[5]);
    heap.set_field(dead, 0, Value::Object(Some(child)));
    heap.set_field(child, 0, Value::Object(Some(grand)));
    heap.set_field(grand, 1, Value::Object(Some(live)));
    heap.set_field(g1, 0, Value::Object(Some(g2)));
    heap.set_field(g2, 0, Value::Object(Some(child)));
    let cands: Vec<usize> = vec![live.as_ptr() as usize, dead.as_ptr() as usize];
    let mut pass = OldGenFinalizerPass::new(&cands);
    cratonvm_types::flags::with_thread_overrides(&cl_flags(in_bfs, skip, incremental), || {
        let _armed = TrueRootMajorScope::arm(true);
        let _ = heap.sweep_old_gen_non_moving_finalizing(
            &[live],
            &cratonvm_types::PointerMap::default(),
            &mut pass,
        );
    });
    let fixture = [live, dead, child, grand, g1, g2];
    let og = heap.old_gen.lock();
    let alive: Vec<bool> = fixture
        .iter()
        .map(|r| og.is_allocated_addr(r.as_ptr()))
        .collect();
    let majors = og.sizing_stats().true_root_majors;
    drop(og);
    let addrs = fixture.map(|o| o.as_ptr() as usize);
    let index = |a: &usize| {
        addrs
            .iter()
            .position(|f| f == a)
            .unwrap_or_else(|| panic!("0x{a:x} is not a fixture object: {addrs:x?}"))
    };
    let resurrected: Vec<usize> = pass.resurrected.iter().map(index).collect();
    let mut closure: Vec<usize> = pass.closure.iter().map(index).collect();
    closure.sort_unstable();
    (alive, resurrected, closure, majors)
}

/// The incremental post-close changes no mark on a non-idle round: the dead
/// candidate is resurrected with its two-deep subgraph, the depth-2 set names
/// exactly those three, the garbage is freed -- the same on every arm.
#[test]
fn gci_i28cl_incremental_post_close_keeps_the_same_marks_on_a_non_idle_round() {
    let base = run(false, true, false);
    assert_eq!(base.3, 1, "precondition: a true-root major ran: {base:?}");
    assert_eq!(
        base.0,
        vec![true, true, true, true, false, false],
        "dead candidate kept with child and grandchild, garbage freed: {base:?}"
    );
    assert_eq!(base.1, vec![1], "one resurrected: {base:?}");
    assert_eq!(base.2, vec![1, 2, 3], "the depth-2 set: {base:?}");
    for (in_bfs, skip, incremental) in [
        (false, true, true),
        (true, true, false),
        (true, true, true),
        // Not armed without the idle skip: the full post-close runs.
        (false, false, true),
        (true, false, true),
    ] {
        assert_eq!(
            run(in_bfs, skip, incremental),
            base,
            "in_bfs={in_bfs} skip={skip} incremental={incremental}: the same marks",
        );
    }
}

/// The switch is opt-in: unset reads OFF, `1` reads ON.
#[test]
fn gci_i28cl_the_switch_is_default_off() {
    cratonvm_types::flags::with_thread_overrides(&[(INCREMENTAL, None)], || {
        assert!(!old_post_close_incremental_enabled());
    });
    cratonvm_types::flags::with_thread_overrides(&[(INCREMENTAL, Some("1"))], || {
        assert!(old_post_close_incremental_enabled());
    });
    cratonvm_types::flags::with_thread_overrides(&[(INCREMENTAL, Some("0"))], || {
        assert!(!old_post_close_incremental_enabled());
    });
}
