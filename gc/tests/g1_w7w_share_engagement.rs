// SPDX-License-Identifier: Apache-2.0
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
        assert_eq!(id, expect, "a node came back wearing id {id}, expected {expect}");
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
    let ratio = if max == 0 { 0.0 } else { total as f64 / max as f64 };
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
        pre_sum, post_sum,
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

#[test]
fn the_valve_opens_on_stack_depth_and_a_four_wide_graph_can_open_it() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_EVAC_SHARE_CENSUS", Some("1")),
            // Named explicitly rather than relied on. This finding is about
            // the default-ON young path, and an arm that silently ran the
            // serial evacuator would report `scans=0` — which the assertion
            // in `run_arm` catches, but naming the flag is what makes the
            // configuration readable from the file rather than from the
            // reader's memory of what the default is this month.
            ("CRATONVM_G1_PARALLEL_EVAC", Some("1")),
            ("CRATONVM_G1_EVAC_LOCAL_QUEUE", Some("1")),
        ],
        || {
            // --- arm 1: W6-P's `tree`, reproduced --------------------------
            //
            // The null arm. 64 sub-frontiers, 103-wide child lists, and the
            // valve stays shut. If this spills, every other number below is
            // measuring something other than what it claims.
            let (tree, _, tree_span) = run_arm(Shape::Tree64);
            assert_eq!(
                tree.spills, 0,
                "tree64 is W6-P's falsifying shape and it must not spill: \
                 63 unconsumed branches plus 103 leaves is 166, under the 256 line"
            );
            assert_eq!(
                tree.children_at_or_over_high(),
                0,
                "tree64's widest child list is 103; nothing in it reaches the line"
            );
            assert_eq!(
                tree.residue_at_or_over_high(),
                0,
                "tree64's deepest residue is 166; nothing in it reaches the line"
            );
            assert_eq!(
                top_bucket(&tree.residue_hist).0,
                BUCKET_AT_HIGH - 1,
                "tree64's residue should top out in the 128-255 bucket (166) — \
                 the bucket IMMEDIATELY below the line, \
                 which is the reading that says the valve is NEAR but shut"
            );

            // --- arm 2: the falsifier --------------------------------------
            //
            // Widest child list in the graph: four. The valve opens anyway.
            let (high, high_sum, high_span) = run_arm(Shape::DeepHigh);
            assert_eq!(
                high.children_at_or_over_high(),
                0,
                "deep-high's widest child list is 4 — no object in it comes \
                 anywhere near the 256-wide child list W6-P names as the \
                 engaging quantity"
            );
            assert_eq!(
                top_bucket(&high.child_hist).0,
                3,
                "deep-high's child lists are 0 (leaves), 1 (root) and 4 (spine); \
                 bucket 3 is 4-7 and nothing above it should exist"
            );
            assert!(
                high.spills_from_accumulation >= 1,
                "THE FALSIFICATION. deep-high must spill, and every one of its \
                 spills must be attributed to accumulation rather than to a \
                 wide child list: residue grows 3 per spine node and crosses \
                 256 at the 86th. Got spills={} from_accumulation={}",
                high.spills,
                high.spills_from_accumulation
            );
            assert_eq!(
                high.spills_from_one_wide_object, 0,
                "no child list in deep-high exceeds the threshold, so no spill \
                 in it can be explained by child-list width"
            );
            assert!(
                high.residue_at_or_over_high() >= 1,
                "deep-high must reach the line it spills at"
            );

            // --- and here is why opening the valve is not the same as fixing
            // --- anything ------------------------------------------------
            //
            // deep-high spills, repeatedly, and its copy span ratio is still
            // 1.00: ONE worker copied every byte. That is not a contradiction,
            // it is the mechanism. `process_object` evacuates a gray object's
            // REFERENTS, so by the time an address is on the local stack its
            // own bytes are already copied and charged to whoever scanned its
            // parent. The copy work a spilled item carries is therefore the
            // number of CSet-bound references INSIDE it — and the half this
            // valve gives away is the OLDEST half, which on a depth-first walk
            // is the accumulated LEAVES. Leaves carry nothing.
            //
            // `run_worker`'s own note says the front is "the most likely to be
            // a genuinely independent subtree for another worker". On this
            // shape it is the opposite: the unexplored frontier is at the
            // BACK, because the back is what the walk is descending.
            //
            // This is the assertion that stops the next person from reading a
            // rising spill count as a win. W2-D measured a prettier
            // distribution bought with a slower pause; this is the same trap
            // one step earlier — an engagement that buys not even the pretty
            // distribution.
            assert!(
                (high_span - 1.0).abs() < 0.01,
                "deep-high spilled {} times and the copy span ratio is {high_span:.2}. \
                 If this ever rises, the spill path started handing out work \
                 that carries bytes and this comment needs rewriting — which is \
                 a result, not a failure",
                high.spills
            );

            // --- arm 3: the same graph, one field order apart ---------------
            //
            // Same nodes, same edges, same child-list widths, same allocation
            // order. The link moves from the last reference slot to the first,
            // the LIFO pops it before its siblings instead of after, and the
            // residue never leaves single digits.
            let (low, low_sum, low_span) = run_arm(Shape::DeepLow);
            assert_eq!(
                low_sum, high_sum,
                "the two deep arms must fold to the SAME checksum — they are \
                 the same graph, visited in the same id order, and only the \
                 slot the link occupies differs"
            );
            assert_eq!(
                low.spills, 0,
                "deep-low holds the same graph as deep-high with the link in \
                 the FIRST reference slot instead of the last. The walk then \
                 pops each spine node's leaves before descending, so the \
                 residue is flat and the valve never opens"
            );
            assert_eq!(
                low.scans, high.scans,
                "the two deep arms must scan the same number of objects"
            );
            assert_eq!(
                low.children_total, high.children_total,
                "the two deep arms must push the same number of children"
            );
            assert_eq!(
                low.child_hist, high.child_hist,
                "the two deep arms must have IDENTICAL child-list distributions; \
                 that is the whole point — the only thing that differs is the \
                 ORDER the slots are declared in"
            );
            assert_eq!(
                low.residue_hist[4..].iter().sum::<u64>(),
                0,
                "deep-low's residue is 4 at its deepest (bucket 3 is 4-7), so \
                 no bucket above 3 may be populated"
            );

            // The finding, stated as a relation between the three arms rather
            // than as three separate numbers.
            assert!(
                high.residue_at_or_over_high() > 0
                    && low.residue_at_or_over_high() == 0
                    && tree.residue_at_or_over_high() == 0,
                "the summary relation: only the arm whose LINK IS LAST reaches \
                 the publish threshold, and it is the arm with the narrowest \
                 child lists of the three"
            );
            assert!(
                (tree_span - 1.0).abs() < 0.01 && (low_span - 1.0).abs() < 0.01,
                "the two non-spilling arms must also read 1.00, or the 1.00 on \
                 deep-high says nothing about the spill: \
                 tree={tree_span:.2} deep-low={low_span:.2}"
            );

            // --- the instrument has to be readable from a shipped binary ----
            //
            // W2-D's reason, applied to this census: a counter whose only
            // reader is this test file is a counter a release run cannot
            // quote. The line goes out on `collector_decision_report`, which
            // is emitted on BOTH exit arms.
            let report = cratonvm_gc::g1::g1_evac_share_census_report();
            assert!(
                report.contains("evac-share:") && report.contains("evac-share-hist:"),
                "both report lines must be present, armed: {report}"
            );
            assert!(
                report.contains("from_accumulation="),
                "the discriminator has to be ON the report line, not only in \
                 this test's asserts: {report}"
            );
        },
    );

    // Disarmed, the report must say so in words rather than print zeros that
    // would read as "this workload never came near the threshold".
    flags::with_thread_overrides(&[("CRATONVM_G1_EVAC_SHARE_CENSUS", None)], || {
        let report = cratonvm_gc::g1::g1_evac_share_census_report();
        assert!(
            report.contains("disarmed"),
            "an unarmed share census must not emit a distribution: {report}"
        );
        },
    );
}
