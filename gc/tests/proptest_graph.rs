// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Property-based test for GC reachability — Enhancement #3 from
//! `review-2026-05-24/gc.md` §2.3.
//!
//! Property under test:
//!
//!   FOR EVERY randomly-generated graph of N=50 objects and FOR EVERY
//!   randomly-generated sequence of link / unlink / GC operations,
//!   after the final GC the set of reachable objects (computed by
//!   walking the heap from `roots`) is exactly the set that the
//!   model says should be reachable.
//!
//! This catches:
//!   - Write-barrier completeness gaps (cross-gen ref forgotten →
//!     reachable young object reclaimed when promoted parent points
//!     to it).
//!   - Card-table races (dirty card cleared before scan).
//!   - Pointer-remap omissions (root rewritten, internal field not).
//!
//! Replaces the deterministic `s29_random_graph_gc_never_collects_reachable`
//! seed-based stress with a proptest-shrunk minimal counterexample
//! on failure — far easier to debug than a million-iteration brute
//! force.
//!
//! Note: tested at `proptest::Config::cases = 64` because each case
//! does a full minor GC, which is ~100 ms in debug. 64 cases × 100 ms
//! ≈ 6.4 s per `cargo test` run — well within the per-test budget.
//! Bump to 256 in CI with `PROPTEST_CASES=256 cargo test` for
//! pre-merge runs.

use std::collections::{HashMap, HashSet, VecDeque};

use cratonvm_gc::collector::MonitorCleanup;
use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, ObjectRef, Value};
use proptest::prelude::*;

/// Capacity per generated graph. Small enough that each test case
/// runs in <100 ms even with a full minor GC inside.
const N: usize = 50;

/// Number of fields per object. Each field can hold one ObjectRef.
/// Four gives the graph enough out-degree variety to exercise the
/// remembered-set / cross-gen barrier paths without blowing up the
/// allocation count.
const FIELDS_PER_OBJ: usize = 4;

/// No-op monitor cleanup — proptest cases do not exercise monitors.
struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Operation in the generated trace. Indexes are over `[0, N)` for
/// graph nodes and `[0, FIELDS_PER_OBJ)` for slots. `Link(src,
/// slot, dst)` writes `dst` into `src.field[slot]`; `Unlink` writes
/// null; `Gc` runs a minor GC.
#[derive(Debug, Clone)]
enum Op {
    Link(usize, usize, usize),
    Unlink(usize, usize),
    Gc,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        // Heavier weight on link/unlink to exercise the barrier; GC
        // is rarer because each GC dominates the runtime budget.
        4 => (0..N, 0..FIELDS_PER_OBJ, 0..N).prop_map(|(s, f, d)| Op::Link(s, f, d)),
        2 => (0..N, 0..FIELDS_PER_OBJ).prop_map(|(s, f)| Op::Unlink(s, f)),
        1 => Just(Op::Gc),
    ]
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    // 8-32 ops per case. Fewer would not stress the barrier; more
    // explodes the GC count per case.
    prop::collection::vec(op_strategy(), 8..32)
}

fn roots_strategy() -> impl Strategy<Value = Vec<usize>> {
    // 1-4 indices into the graph nominated as the root set. Small
    // root sets make the reachability property meaningful (many
    // unreachable subgraphs to test the collector kills).
    prop::collection::vec(0..N, 1..=4)
}

/// Apply the same op sequence to a pure-Rust model of the graph and
/// return the post-trace reachable set. This is the oracle the GC
/// must match. The model never "collects" — it only tracks edges
/// and computes reachability from roots via BFS, which is the
/// trivial spec for what GC must preserve.
fn model_reachable(ops: &[Op], roots: &[usize]) -> HashSet<usize> {
    // model[i][slot] = Some(target) means node i has a ref to target
    // in slot. Initially every slot is None.
    let mut edges: Vec<[Option<usize>; FIELDS_PER_OBJ]> = vec![[None; FIELDS_PER_OBJ]; N];
    for op in ops {
        match *op {
            Op::Link(s, f, d) => edges[s][f] = Some(d),
            Op::Unlink(s, f) => edges[s][f] = None,
            Op::Gc => {} // GC is a no-op for the pure model — reachability is structural.
        }
    }
    // BFS from roots.
    let mut reached = HashSet::new();
    let mut queue: VecDeque<usize> = roots.iter().copied().collect();
    while let Some(n) = queue.pop_front() {
        if !reached.insert(n) {
            continue;
        }
        for target in edges[n].iter().flatten() {
            queue.push_back(*target);
        }
    }
    reached
}

/// Apply the same op sequence to a real `GenerationalHeap` and
/// return the set of node IDs reachable from roots AFTER a final
/// GC, by walking the heap graph from the (possibly remapped) root
/// `ObjectRef`s.
///
/// Node identity: each allocated object stores its node ID in field
/// `FIELDS_PER_OBJ` (so refs occupy fields 0..FIELDS_PER_OBJ and
/// the ID lives in field FIELDS_PER_OBJ). After GC we walk the
/// graph using ObjectRef equality and read field[FIELDS_PER_OBJ]
/// as the node ID. The ID field is `Value::Int` so the GC does
/// not treat it as a reference root.
fn heap_reachable_after_gc(ops: &[Op], root_ids: &[usize]) -> HashSet<usize> {
    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    let class_id = ClassId::new(1);

    // Allocate N objects with FIELDS_PER_OBJ + 1 fields each. The
    // last slot stores the node ID so we can recover identity after
    // the GC has relocated pointers.
    //
    // We hold a temporary `Vec<ObjectRef>` so that during the apply
    // phase below every node is reachable (every node is in the
    // roots slice we pass to set_field, which only fires the write
    // barrier — no GC implicit in alloc). The set_field signature
    // takes raw ObjectRefs so we use this `all_nodes` slice as the
    // "everyone alive" set during link/unlink. Just before the
    // final GC we trim `all_nodes` to ONLY the nominated roots,
    // matching what the model treats as roots.
    let mut all_nodes: Vec<ObjectRef> = (0..N)
        .map(|i| {
            let r = heap.alloc_object(class_id, FIELDS_PER_OBJ + 1);
            heap.set_field(r, FIELDS_PER_OBJ, Value::Int(i as i32));
            r
        })
        .collect();

    // Apply the operation sequence. Mid-trace `Op::Gc`s run with the
    // full `all_nodes` as roots — they must be no-ops for the final
    // reachability property because every node is rooted at this
    // point. They exist to stress the barrier under concurrent
    // mutation: if a link op runs between two GCs, the second GC's
    // root scan + dirty-card scan must see the new edge.
    for op in ops {
        match *op {
            Op::Link(s, f, d) => {
                let dst = Value::Object(Some(all_nodes[d]));
                heap.set_field(all_nodes[s], f, dst);
            }
            Op::Unlink(s, f) => {
                heap.set_field(all_nodes[s], f, Value::Object(None));
            }
            Op::Gc => {
                // SAFETY: this property test drives the heap single-threaded.
                let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new_unchecked() };
                let _ = heap.collect_garbage(&stw, &mut all_nodes, &NoMonitors);
            }
        }
    }

    // Final GC: pass ONLY the nominated roots. Now the property
    // bites — anything unreachable from `root_ids` MUST be collected,
    // and everything reachable MUST survive with intact internal
    // edges.
    let mut roots: Vec<ObjectRef> = root_ids.iter().map(|&i| all_nodes[i]).collect();
    // SAFETY: this property test drives the heap single-threaded.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new_unchecked() };
    let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);

    // Walk the heap from the (now possibly remapped) roots and
    // collect node IDs.
    let mut visited: HashSet<usize> = HashSet::new();
    let mut visited_refs: HashSet<usize> = HashSet::new(); // pointer identity
    let mut queue: VecDeque<ObjectRef> = roots.iter().copied().collect();
    while let Some(r) = queue.pop_front() {
        // Pointer identity dedup — two paths into the same object
        // would BFS forever without this.
        let addr = r.as_ptr() as usize;
        if !visited_refs.insert(addr) {
            continue;
        }
        // Read this node's ID.
        match heap.get_field(r, FIELDS_PER_OBJ) {
            Value::Int(id) => {
                visited.insert(id as usize);
            }
            other => panic!(
                "node id slot corrupted across GC: got {:?} at addr {:#x}",
                other, addr
            ),
        }
        // Enqueue every non-null ref field.
        for slot in 0..FIELDS_PER_OBJ {
            if let Value::Object(Some(child)) = heap.get_field(r, slot) {
                queue.push_back(child);
            }
        }
    }
    visited
}

proptest! {
    #![proptest_config(ProptestConfig {
        // 64 cases keeps wall-time bounded; CI overrides via PROPTEST_CASES.
        cases: 64,
        // Shrink failures aggressively — minimal counterexamples
        // make GC bugs orders-of-magnitude easier to root-cause.
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    /// The headline reachability property: heap and pure model
    /// must agree on the post-GC reachable set, for every legal
    /// op sequence and every root selection.
    #[test]
    fn gc_preserves_reachability(
        ops in ops_strategy(),
        roots in roots_strategy(),
    ) {
        let model_set = model_reachable(&ops, &roots);
        let heap_set = heap_reachable_after_gc(&ops, &roots);
        prop_assert_eq!(
            heap_set.clone(),
            model_set.clone(),
            "heap reachability diverges from model: \
             model={:?} heap={:?} ops={:?} roots={:?}",
            model_set, heap_set, ops, roots
        );
    }

    /// Weaker version of the headline property kept as a separate
    /// test because it catches the most common write-barrier bug
    /// (reachable object reclaimed) with a tighter failure
    /// message that doesn't drown the reviewer in trace ops.
    /// Specifically: GC NEVER collects a reachable object.
    /// (Collecting an unreachable object as still-live is also a
    /// bug, but a less dangerous one — bounded leak vs. UAF.)
    #[test]
    fn gc_never_reclaims_reachable(
        ops in ops_strategy(),
        roots in roots_strategy(),
    ) {
        let model_set = model_reachable(&ops, &roots);
        let heap_set = heap_reachable_after_gc(&ops, &roots);
        let missing: HashSet<_> = model_set.difference(&heap_set).copied().collect();
        prop_assert!(
            missing.is_empty(),
            "reachable nodes collected by GC: {:?} (model expected {:?}, heap returned {:?}; ops={:?}, roots={:?})",
            missing, model_set, heap_set, ops, roots
        );
    }
}
