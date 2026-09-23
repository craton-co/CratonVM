// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane D — the parallel evacuation closure, exercised through the PUBLIC
//! driver rather than through the internal one.
//!
//! # Why these go through `young_collection` and not `young_collection_parallel`
//!
//! `young_collection_parallel` is `pub(crate)`, so an integration test cannot
//! name it — and that is the right shape for these particular tests. What they
//! are checking is the arm a production pause takes, and the only honest way to
//! ask for that arm is to ask for a pause. `CRATONVM_G1_PARALLEL_EVAC` defaults
//! to ON, so `young_collection` dispatches here unless something in the
//! environment says otherwise; if a future change flips that default back, the
//! census assertion in [`the_default_young_pause_uses_the_parallel_evacuator`]
//! fails rather than these tests quietly becoming a second copy of the serial
//! suite. That failure mode is the whole point: this file's history already has
//! a test that passed with `retire_forwards` deleted because it ran the other
//! arm.
//!
//! # What is actually being covered
//!
//! The 2026-09-20 lane-D changes to `SharedEvac::run_worker` — per-worker local
//! gray stacks, a batched lift from the shared queue, and an overflow spill
//! back to it — change the ORDER in which the closure visits the object graph
//! and nothing else. An ordering change in a transitive closure has exactly two
//! failure modes worth testing for, and neither shows on a small graph:
//!
//!   * an object reached but never scanned (its own references left pointing
//!     into the collection set, which Phase 5 then frees), and
//!   * a termination bug, where the `outstanding` counter reaches zero while
//!     work is still sitting in some worker's local stack — a silently
//!     truncated closure, not a hang.
//!
//! Both need a frontier wide enough to cross `EVAC_LOCAL_PUBLISH_HIGH` (256) so
//! the spill path runs at all, and deep enough that a truncated closure loses
//! something a shallow one would not. The graphs below are built for that, and
//! every one of them is verified by reading EVERY field of EVERY node back
//! through the collector's own accessors after the pause — a checksum over the
//! reachable set, which is the one assertion that catches a lost subtree
//! wherever it was lost.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        // Large enough that the graphs below never reach to-space exhaustion:
        // an evacuation failure is a legitimate outcome with its own drain
        // path, but it is a DIFFERENT path, and a test that sometimes takes it
        // is a test that sometimes checks something else.
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }
}

/// A node with `FANOUT` reference fields followed by one int field carrying the
/// node's identity. Reading that int back after a pause is what proves the node
/// was evacuated intact rather than merely reachable.
const FANOUT: usize = 4;
const ID_SLOT: usize = FANOUT;
const FIELDS: usize = FANOUT + 1;

/// Build a tree of `levels` deep with `FANOUT` children per node, returning the
/// root and the number of nodes. Node `n`'s int field holds `n`.
fn build_tree(gc: &G1Collector, levels: u32) -> (ObjectRef, usize) {
    let mut next_id = 0i32;
    fn build(gc: &G1Collector, depth: u32, next_id: &mut i32, count: &mut usize) -> ObjectRef {
        let me = gc.alloc_object(ClassId::new(7), FIELDS);
        let id = *next_id;
        *next_id += 1;
        *count += 1;
        gc.set_field(me, ID_SLOT, Value::Int(id));
        if depth > 0 {
            for slot in 0..FANOUT {
                let child = build(gc, depth - 1, next_id, count);
                gc.set_field(me, slot, Value::Object(Some(child)));
            }
        }
        me
    }
    let mut count = 0usize;
    let root = build(gc, levels, &mut next_id, &mut count);
    (root, count)
}

/// Walk the tree through the collector's accessors and return the set of ids
/// found, asserting the shape on the way. Any node whose reference slots were
/// not rewritten, or whose body was not copied, shows up here as a missing id,
/// a duplicate id or a panic inside `get_field`.
fn collect_ids(gc: &G1Collector, root: ObjectRef, out: &mut Vec<i32>) {
    let id = gc
        .get_field(root, ID_SLOT)
        .as_int()
        .expect("every node carries its id in its last slot");
    out.push(id);
    for slot in 0..FANOUT {
        if let Value::Object(Some(child)) = gc.get_field(root, slot) {
            collect_ids(gc, child, out);
        }
    }
}

fn assert_tree_intact(gc: &G1Collector, root: ObjectRef, expected: usize) {
    let mut ids = Vec::with_capacity(expected);
    collect_ids(gc, root, &mut ids);
    assert_eq!(
        ids.len(),
        expected,
        "the closure lost {} of {expected} nodes — a subtree was reached but \
         never scanned, or the gray frontier terminated early",
        expected.saturating_sub(ids.len())
    );
    ids.sort_unstable();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            *id, i as i32,
            "node identities are not a permutation of 0..{expected}: the body \
             of at least one node was not copied intact"
        );
    }
}

/// The premise every other test in this file rests on. If the default pause is
/// not the parallel one, the rest of the file is testing the serial evacuator
/// under a misleading name.
#[test]
fn the_default_young_pause_uses_the_parallel_evacuator() {
    let gc = G1Collector::new(config());
    let (before_parallel, before_serial, _) = cratonvm_gc::g1::g1_young_evac_counts();
    let obj = gc.alloc_object(ClassId::new(1), 1);
    let mut roots = vec![obj];
    gc.young_collection(&mut roots, &NoopMonitors);
    let (after_parallel, after_serial, workers) = cratonvm_gc::g1::g1_young_evac_counts();
    assert_eq!(
        after_serial, before_serial,
        "a default young pause took the SERIAL evacuator; the rest of this \
         file is then not covering the arm it says it is"
    );
    assert!(
        after_parallel > before_parallel,
        "the parallel dispatch counter did not move"
    );
    assert!(workers >= 1, "worker count census is not populated");
}

/// A frontier several times wider than `EVAC_LOCAL_PUBLISH_HIGH`, so the spill
/// arm runs, plus enough depth that a truncated closure loses a measurable
/// subtree rather than a leaf.
#[test]
fn a_wide_deep_tree_survives_the_parallel_closure_intact() {
    let gc = G1Collector::new(config());
    // 4^6 leaves = 4096 nodes on the widest level, against a 256-entry local
    // stack: every worker spills, repeatedly.
    let (root, count) = build_tree(&gc, 6);
    assert!(
        count > 4000,
        "the graph must be wide enough to cross the spill threshold (got {count})"
    );
    let mut roots = vec![root];
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    assert_eq!(
        result.stats.objects_copied, count,
        "every live node should have been evacuated exactly once"
    );
    assert_tree_intact(&gc, roots[0], count);
}

/// The same graph across several pauses, so survivors age, some promote into
/// Old regions, and later pauses reach the graph through remembered sets rather
/// than only through the root. Promotion is the arm that drives the Old TLAB —
/// and therefore `claim_pool_slot`'s front cursor and the `resume` list — which
/// the single-pause test above never touches.
#[test]
fn a_graph_that_ages_into_old_regions_survives_repeated_parallel_pauses() {
    let gc = G1Collector::new(config());
    let (root, count) = build_tree(&gc, 5);
    let mut roots = vec![root];
    for pause in 0..8 {
        // Allocate garbage between pauses so each collection has a real Eden to
        // reclaim and the collection set is never trivially empty.
        for _ in 0..500 {
            let junk = gc.alloc_object(ClassId::new(9), 2);
            gc.set_field(junk, 0, Value::Int(pause));
        }
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_tree_intact(&gc, roots[0], count);
    }
}

/// A pause whose collection set holds nothing live. This is the case the
/// Phase-3 dispatch now declines to wake the worker pool for, and the property
/// that must not change with it is that the pause still runs, still reclaims,
/// and still leaves the (empty) root set usable.
#[test]
fn an_all_garbage_young_generation_is_reclaimed_without_waking_the_pool() {
    let gc = G1Collector::new(config());
    for _ in 0..20_000 {
        let junk = gc.alloc_object(ClassId::new(3), 2);
        gc.set_field(junk, 0, Value::Int(1));
    }
    let mut roots: Vec<ObjectRef> = Vec::new();
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    assert_eq!(
        result.stats.objects_copied, 0,
        "nothing was reachable, so nothing should have been copied"
    );
    assert!(
        result.stats.bytes_freed > 0,
        "an all-garbage young generation must be reclaimed"
    );
    // ...and the collector is still usable afterwards, which is the half a
    // wedged termination protocol would fail.
    let obj = gc.alloc_object(ClassId::new(4), 1);
    gc.set_field(obj, 0, Value::Int(5));
    let mut roots = vec![obj];
    gc.young_collection(&mut roots, &NoopMonitors);
    assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(5));
}

/// A DIAMOND-heavy graph: many parents share the same children, so several
/// workers race the forwarding CAS on one object and the CAS-loser arms of
/// `SharedEvac::evacuate` are actually reached. Those arms are where the
/// forward has to be recorded even though the copy was abandoned, and where the
/// survivor-age histogram used to be charged for a copy nobody published.
#[test]
fn a_shared_subgraph_converges_on_one_copy_under_several_workers() {
    let gc = G1Collector::new(config());
    // One shared tier, referenced from every node of a wide parent tier.
    const SHARED: usize = 512;
    const PARENTS: usize = 1024;
    let shared: Vec<ObjectRef> = (0..SHARED)
        .map(|i| {
            let o = gc.alloc_object(ClassId::new(11), FIELDS);
            gc.set_field(o, ID_SLOT, Value::Int(i as i32));
            o
        })
        .collect();
    let root = gc.alloc_object(ClassId::new(12), PARENTS);
    for p in 0..PARENTS {
        let parent = gc.alloc_object(ClassId::new(13), FIELDS);
        gc.set_field(parent, ID_SLOT, Value::Int(p as i32));
        for slot in 0..FANOUT {
            gc.set_field(
                parent,
                slot,
                Value::Object(Some(shared[(p * FANOUT + slot) % SHARED])),
            );
        }
        gc.set_field(root, p, Value::Object(Some(parent)));
    }

    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);

    // Every parent must see exactly the shared node it named, and every
    // reference to one shared node must have converged on ONE address — which
    // is the property a mis-decoded CAS-loser target breaks.
    let root = roots[0];
    let mut shared_addr: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    for p in 0..PARENTS {
        let Value::Object(Some(parent)) = gc.get_field(root, p) else {
            panic!("parent {p} was lost by the closure");
        };
        assert_eq!(gc.get_field(parent, ID_SLOT).as_int(), Some(p as i32));
        for slot in 0..FANOUT {
            let Value::Object(Some(child)) = gc.get_field(parent, slot) else {
                panic!("parent {p} slot {slot} lost its shared child");
            };
            let want = ((p * FANOUT + slot) % SHARED) as i32;
            let got = gc
                .get_field(child, ID_SLOT)
                .as_int()
                .expect("a shared node must carry its id");
            assert_eq!(
                got, want,
                "parent {p} slot {slot} points at the wrong shared node"
            );
            let addr = child.as_ptr() as usize;
            match shared_addr.get(&want) {
                Some(&seen) => assert_eq!(
                    seen, addr,
                    "shared node {want} exists at TWO addresses after the pause \
                     — a CAS loser installed its abandoned copy instead of \
                     adopting the winner's"
                ),
                None => {
                    shared_addr.insert(want, addr);
                }
            }
        }
    }
    assert_eq!(shared_addr.len(), SHARED, "a shared node was lost entirely");
}
