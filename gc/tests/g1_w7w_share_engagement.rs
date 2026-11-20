// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W7-W — what the evacuator's load-balancing valve is actually a
//! function of, and the two graphs that differ in NOTHING a Java programmer
//! would call a shape.
//!
//! # The claim under test
//!
//! `w6p-work-sharing-engages-on-child-list-width-not-graph-width.md` states
//! the mechanism as: *"a worker hands work to its peers **only** when some
//! single `process_object` call pushes enough children at once to carry the
//! stack over 256. A depth-first walk cannot accumulate that from breadth."*
//!
//! The first half is sufficient and the second half is false, and the
//! difference decides the open question W6-P §3 leaves — whether real Java
//! graphs sit below the line. `SharedEvac::run_worker` pops ONE item and
//! extends by a WHOLE child list, so `local.len()` is exactly
//! `discovered - processed`, which on a depth-first walk is the count of
//! not-yet-popped siblings and uncles along the current root path. That
//! accumulates as `depth * (branching - 1)`. W6-P's own `tree` row is
//! consistent with both accounts and so cannot separate them: 64 branches of
//! 103 leaves reaches `63 + 103 = 166`, under the line either way.
//!
//! So this file runs the shape that separates them.
//!
//! # Three arms over the same node count
//!
//! * **`tree64`** — W6-P's falsifying case, reproduced. 64 branches of 103
//!   leaves. Predicted residue 166, no spill. The null arm: if this one
//!   spills, the instrument is measuring something else and the rest is void.
//! * **`deep-high`** — a spine of 1,664 nodes, each holding three leaves and
//!   the link to the next spine node, with **the link in the HIGHEST slot**.
//!   Widest child list in the entire graph: **four**. Predicted residue
//!   `3k + 1`, which crosses 256 at the 86th spine node and spills.
//! * **`deep-low`** — the SAME graph with the link moved to the LOWEST slot.
//!   Same node count, same edge count, same per-object child-list widths,
//!   same allocation order. Predicted residue **4**, forever.
//!
//! `deep-high` spilling is the falsification: a graph whose widest child list
//! is four engaged a valve whose threshold is 256.
//!
//! `deep-low` is why that matters in practice. The evacuator pops from the
//! BACK, so the last reference slot is descended first and the earlier ones
//! are left on the stack. Whether a linked structure's residue grows with its
//! depth or stays flat is therefore decided by **where the link sits among the
//! class's reference fields** — `java.util.HashMap$Node` declares `next` last,
//! `java.util.LinkedList$Node` declares `item` first. That is not a property
//! anyone chose, and it is not one the finding as written would predict.
//!
//! # Counters, not clocks
//!
//! Every assertion is on a count from [`EvacShareCensus`], diffed around one
//! pause. There is no wall clock in this file and nothing here is a
//! performance claim — in particular nothing here recommends changing
//! `EVAC_LOCAL_PUBLISH_HIGH`, which wave 1 measured at 2.8x and w2d measured
//! as *slower* when disarmed despite a prettier distribution.
//!
//! # ONE `#[test]`, deliberately
//!
//! [`EvacShareCensus`] is a process-global table and `libtest` runs tests on
//! several threads, so a second `#[test]` in this binary would pollute the
//! deltas this one takes. One test, sequential arms, deltas throughout —
//! the discipline `g1_w6p_mixed_parallel_equivalence.rs` adopted for the same
//! reason.
//!
//! ```text
//! cargo test -p cratonvm-gc --test g1_w7w_share_engagement -- --nocapture
//! ```

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::{EvacShareCensus, G1CollectorConfig};
use cratonvm_gc::G1Collector;
use cratonvm_types::flags;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// The constant under study. Duplicated rather than exported: this file is a
/// check ON that constant, and a test that reads the value it is checking
/// cannot notice the value changing.
const PUBLISH_HIGH: usize = 256;
/// First histogram bucket entirely at or above [`PUBLISH_HIGH`]. Buckets are
/// `0`, `1`, `2-3`, `4-7`, … so bucket 8 is `128-255` and bucket 9 is
/// `256-511`.
const BUCKET_AT_HIGH: usize = 9;

/// Nodes in every arm: `64 * 104 == 1664 * 4 == 6656`.
const NODES: usize = 6656;
/// Spine nodes in the two `deep` arms; each carries three leaves.
const SPINE: usize = 1664;
const LEAVES_PER_SPINE: usize = 3;

const FIELDS: usize = 32;
const ID_SLOT: usize = 1;
const CHILD0: usize = 2;

const CLS_ROOT: u32 = 60;
const CLS_NODE: u32 = 61;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// W6-P's `tree`: 64 branches of 103 leaves.
    Tree64,
    /// Spine with the link in the HIGHEST reference slot.
    DeepHigh,
    /// Spine with the link in the LOWEST reference slot.
    DeepLow,
}

impl Shape {
    fn label(self) -> &'static str {
        match self {
            Shape::Tree64 => "tree64   ",
            Shape::DeepHigh => "deep-high",
            Shape::DeepLow => "deep-low ",
        }
    }
    /// Is the spine link the last reference slot (so the walk descends it
    /// first and leaves the leaves behind)?
    fn link_last(self) -> bool {
        self == Shape::DeepHigh
    }
}

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 128 * 1024 * 1024,
        initial_heap_size: 128 * 1024 * 1024,
        region_size: 1024 * 1024,
        max_gc_pause_ms: 100_000,
        ..Default::default()
    }
}

fn sized_node(gc: &G1Collector, id: usize, fields: usize) -> ObjectRef {
    let o = gc.alloc_object(ClassId::new(CLS_NODE), fields);
    gc.set_field(o, ID_SLOT, Value::Int(id as i32));
    o
}

fn leaf(gc: &G1Collector, id: usize) -> ObjectRef {
    let o = sized_node(gc, id, FIELDS);
    // Padding, so a leaf is worth copying and so the ref-slot walk has
    // non-reference words to step over exactly as it does on a real object.
    for f in CHILD0..FIELDS {
        gc.set_field(o, f, Value::Int(id as i32 ^ f as i32));
    }
    o
}

fn build(gc: &G1Collector, shape: Shape) -> ObjectRef {
    if shape == Shape::Tree64 {
        let (branches, lpb) = (64usize, 103usize);
        assert_eq!(branches * (1 + lpb), NODES);
        let root = gc.alloc_object(ClassId::new(CLS_ROOT), branches);
        let mut next_leaf = branches;
        for b in 0..branches {
            let branch = sized_node(gc, b, CHILD0 + lpb);
            gc.set_field(root, b, Value::Object(Some(branch)));
            for c in 0..lpb {
                let l = leaf(gc, next_leaf);
                gc.set_field(branch, CHILD0 + c, Value::Object(Some(l)));
                next_leaf += 1;
            }
        }
        assert_eq!(next_leaf, NODES);
        return root;
    }

    // The two spine shapes. A spine node declares four reference slots; the
    // ONLY difference between the arms is which of them holds the link.
    assert_eq!(SPINE * (1 + LEAVES_PER_SPINE), NODES);
    let slots = CHILD0 + LEAVES_PER_SPINE + 1;
    let (link_slot, leaf0) = if shape.link_last() {
        (CHILD0 + LEAVES_PER_SPINE, CHILD0)
    } else {
        (CHILD0, CHILD0 + 1)
    };
    let root = gc.alloc_object(ClassId::new(CLS_ROOT), 1);
    let mut prev: Option<ObjectRef> = None;
    let mut next_leaf = SPINE;
    for s in 0..SPINE {
        let n = sized_node(gc, s, slots);
        match prev {
            None => gc.set_field(root, 0, Value::Object(Some(n))),
            Some(p) => gc.set_field(p, link_slot, Value::Object(Some(n))),
        }
        for c in 0..LEAVES_PER_SPINE {
            let l = leaf(gc, next_leaf);
            gc.set_field(n, leaf0 + c, Value::Object(Some(l)));
            next_leaf += 1;
        }
        prev = Some(n);
    }
    assert_eq!(next_leaf, NODES);
    root
}

/// Walk the surviving graph in id order. Returns the fold every arm asserts
/// on, which is the check that the pause under measurement did not quietly
/// lose or corrupt the fixture it was measuring.
fn walk(gc: &G1Collector, root: ObjectRef, shape: Shape) -> (u64, usize) {
    let mut sum: u64 = 0xcbf2_9ce4_8422_2325;
    let mut seen = 0usize;
    let mut visit = |id: usize, expect: usize| {
        assert_eq!(
            id, expect,
            "a node came back wearing id {id}, expected {expect}"
        );
        sum = sum.wrapping_mul(0x100_0000_01b3) ^ (id as u64);
        seen += 1;
    };
    if shape == Shape::Tree64 {
        let (branches, lpb) = (64usize, 103usize);
        for b in 0..branches {
            let Value::Object(Some(branch)) = gc.get_field(root, b) else {
                panic!("branch {b} was lost by the pause");
            };
            let id = gc.get_field(branch, ID_SLOT).as_int().expect("branch id") as usize;
            visit(id, b);
            for c in 0..lpb {
                let Value::Object(Some(l)) = gc.get_field(branch, CHILD0 + c) else {
                    panic!("branch {b} lost child {c}");
                };
                let id = gc.get_field(l, ID_SLOT).as_int().expect("leaf id") as usize;
                visit(id, branches + b * lpb + c);
            }
        }
    } else {
        let (link_slot, leaf0) = if shape.link_last() {
            (CHILD0 + LEAVES_PER_SPINE, CHILD0)
        } else {
            (CHILD0, CHILD0 + 1)
        };
        let Value::Object(Some(head)) = gc.get_field(root, 0) else {
            panic!("the spine head was lost by the pause");
        };
        let mut cur = head;
        for s in 0..SPINE {
            let id = gc.get_field(cur, ID_SLOT).as_int().expect("spine id") as usize;
            visit(id, s);
            for c in 0..LEAVES_PER_SPINE {
                let Value::Object(Some(l)) = gc.get_field(cur, leaf0 + c) else {
                    panic!("spine node {s} lost leaf {c}");
                };
                let id = gc.get_field(l, ID_SLOT).as_int().expect("leaf id") as usize;
                visit(id, SPINE + s * LEAVES_PER_SPINE + c);
            }
            if s + 1 == SPINE {
                break;
            }
            let Value::Object(Some(next)) = gc.get_field(cur, link_slot) else {
                panic!("the spine broke after {s} nodes");
            };
            cur = next;
        }
    }
    assert_eq!(seen, NODES, "the walk did not reach every node");
    (sum, seen)
}

/// The top non-empty bucket of a histogram, as `(index, count)`.
fn top_bucket(h: &[u64]) -> (usize, u64) {
    h.iter()
        .enumerate()
        .rev()
        .find(|(_, c)| **c > 0)
        .map(|(i, c)| (i, *c))
        .unwrap_or((0, 0))
}

fn bucket_range(i: usize) -> String {
    match i {
        0 => "0".to_string(),
        1 => "1".to_string(),
        11 => "2048+".to_string(),
        k => format!("{}-{}", 1usize << (k - 1), (1usize << k) - 1),
    }
}

/// Bytes copied per worker over one pause, and the span ratio they give.
///
/// Read from `EvacWorkerCensus` (lane W7-C's table) through its PUBLIC
/// accessor and never written here. It is in this file for one reason, and it
/// is the reason the brief for this lane insists on: **a change that opens the
/// valve more often is not thereby an improvement.** `w2d-evac-worker-census`
/// measured `CRATONVM_G1_EVAC_LOCAL_QUEUE=0` producing a prettier distribution
/// and a slower pause. So the arm that spills has to be asked whether the
/// spilling bought any copy parallelism at all, and this is the number that
/// answers it.
fn copy_span(before: &[cratonvm_gc::g1::EvacWorkerCensus]) -> (u64, u64, f64, usize) {
    let after = cratonvm_gc::g1::g1_evac_worker_census();
    let per: Vec<u64> = after
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let b = before.get(i).copied().unwrap_or_default();
            (a.seed_bytes + a.bytes_copied) - (b.seed_bytes + b.bytes_copied)
        })
        .collect();
    let total: u64 = per.iter().sum();
    let max = per.iter().copied().max().unwrap_or(0);
    let copiers = per.iter().filter(|b| **b > 0).count();
    let ratio = if max == 0 {
        0.0
    } else {
        total as f64 / max as f64
    };
    (total, max, ratio, copiers)
}

/// One arm: build, take ONE young pause on the parallel evacuator, diff the
/// share census around it.
fn run_arm(shape: Shape) -> (EvacShareCensus, u64, f64) {
    let gc = G1Collector::new(config());
    let root = build(&gc, shape);
    let mut roots = vec![root];

    // The pre-pause fold. If the fixture is already broken the comparison
    // below means nothing, so it is checked before as well as after.
    let (pre_sum, _) = walk(&gc, roots[0], shape);

    // Snapshot AFTER the build. `alloc_object` may have taken a young pause of
    // its own while filling the heap, and that pause's pushes are not this
    // arm's measurement — the delta excludes them by construction.
    let before = cratonvm_gc::g1::g1_evac_share_census();
    let workers_before = cratonvm_gc::g1::g1_evac_worker_census();
    gc.young_collection(&mut roots, &NoopMonitors);
    let after = cratonvm_gc::g1::g1_evac_share_census();
    let d = after.since(&before);
    let (bytes_total, bytes_max, span_ratio, copiers) = copy_span(&workers_before);

    let (post_sum, _) = walk(&gc, roots[0], shape);
    assert_eq!(
        pre_sum,
        post_sum,
        "{}: the pause changed the surviving graph",
        shape.label()
    );
    assert!(
        d.scans > 0,
        "{}: the parallel evacuator scanned nothing, so this arm measured no \
         closure at all — check that CRATONVM_G1_PARALLEL_EVAC is on and that \
         the share census is armed",
        shape.label()
    );

    let (cb, cn) = top_bucket(&d.child_hist);
    let (rb, rn) = top_bucket(&d.residue_hist);
    println!(
        "{}: scans={} children={} widest_child_bucket={}({}) deepest_residue_bucket={}({}) \
         spills={} from_one_wide_object={} from_accumulation={} \
         children_at_or_over_{PUBLISH_HIGH}={} residue_at_or_over_{PUBLISH_HIGH}={} \
         lifetime_children_max={} lifetime_residue_max={} \
         copied_bytes={bytes_total} max_worker_bytes={bytes_max} \
         span_ratio={span_ratio:.2} workers_that_copied={copiers} checksum=0x{:016x}",
        shape.label(),
        d.scans,
        d.children_total,
        bucket_range(cb),
        cn,
        bucket_range(rb),
        rn,
        d.spills,
        d.spills_from_one_wide_object,
        d.spills_from_accumulation,
        d.children_at_or_over_high(),
        d.residue_at_or_over_high(),
        d.children_max,
        d.residue_max,
        post_sum,
    );
    (d, post_sum, span_ratio)
}


