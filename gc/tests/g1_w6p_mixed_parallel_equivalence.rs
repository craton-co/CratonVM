// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W6-P — the two mixed drivers over ONE fixture, and the number the
//! `CRATONVM_G1_PARALLEL_MIXED` decision has been missing.
//!
//! # Why this is a unit-level measurement and not a probe A/B
//!
//! `CRATONVM_G1_PARALLEL_MIXED` has been left opt-in twice, both times for the
//! same stated reason: nobody has measured what flipping it would buy. The
//! obvious experiment — run a workload with `=0`, run it again with `=1`,
//! compare — cannot produce that number on this tree, and the reason is the
//! denominator. Eight `G1OldBurstProbe` configurations plus the standing
//! battery produced **zero** mixed pauses in up to 8,945 young ones. An A/B
//! over a workload that never takes a mixed pause compares two runs that took
//! the same code path the same number of times: zero. It still reports a
//! difference, because this host's medians move up to 9% under a provably
//! inert lever — which is how three measurements were retracted this round.
//!
//! So the pause is constructed here instead, deterministically, and the cost
//! is read as a COUNTER rather than a wall clock:
//!
//! * both drivers run over the same fixture, built by the same allocation
//!   sequence in the same process, and their surviving graphs and evacuated
//!   object sets are compared object by object;
//! * the parallel arm's per-worker census (`g1_evac_worker_census`) is diffed
//!   around the pause, giving bytes copied per worker, work-sharing counts
//!   (`lifts`/`lifted`/`spills`) and idle time;
//! * from that comes the one number the decision needs, the **copy span
//!   ratio** `total_copied / max_worker_copied` — the ceiling on what routing
//!   a mixed pause through the worker pool can buy, before any scheduling
//!   overhead is paid. A ratio of 1.00 means one worker did every byte and
//!   flipping the lever cannot buy anything at all on that shape.
//!
//! # Three shapes, because the shape is the whole answer
//!
//! `w2d-the-probe-battery-cannot-measure-evacuation-parallelism.md` is the
//! trap this file is walking into: every workload in the battery retains its
//! live set as a singly-linked chain, and a chain cannot be evacuated in
//! parallel by any protocol. A measurement that ran only a favourable shape
//! would report a ceiling and never establish that the instrument can report
//! the ABSENCE of one. All three shapes hold the same node count and within
//! 2% of the same live bytes:
//!
//! * **`chain`** — the null arm. One singly-linked list. A correct instrument
//!   reports ~1.00 here, and if it ever reports more, every other number in
//!   this file is void.
//! * **`tree`** — 64 branches of 100 leaves. Genuinely wide at the graph
//!   level: 64 independent sub-frontiers, more than twice the worker count.
//! * **`wide`** — one array of 6,464 direct children, i.e. a single object
//!   whose child list is larger than [`EVAC_LOCAL_PUBLISH_HIGH`] (256).
//!
//! The gap between `tree` and `wide` is the finding; see the page.
//!
//! # ONE `#[test]`, deliberately
//!
//! The per-worker census is a process-global table and `libtest` runs its
//! tests on several threads, so a second `#[test]` in this binary would bump
//! the rows this one is diffing. One test, sequential arms, deltas throughout.
//!
//! Run with output:
//!
//! ```text
//! cargo test -p cratonvm-gc --release --test g1_w6p_mixed_parallel_equivalence -- --nocapture
//! ```

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::{EvacWorkerCensus, G1CollectorConfig};
use cratonvm_gc::G1Collector;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One singly-linked list. The null arm.
    Chain,
    /// 64 branches of 103 leaves. Wide at the GRAPH level — nearly three
    /// times the worker count — but the root's child list (64) is below the
    /// local-stack publish threshold, so nothing is ever handed out.
    Tree,
    /// 512 branches of 12 leaves. Same node count; the root's child list is
    /// twice `EVAC_LOCAL_PUBLISH_HIGH`, which is what makes the driver
    /// publish PARENTS — the only thing that moves copy work off it.
    Wide,
    /// The shape a REAL mixed pause has. One flat holder array of every node,
    /// in an Old region that is deliberately kept OUT of the collection set,
    /// so the old live set is reachable only through the remembered set and
    /// is therefore copied in PHASE 2 rather than in the Phase-3 closure.
    Rset,
}

impl Shape {
    fn label(self) -> &'static str {
        match self {
            Shape::Chain => "chain",
            Shape::Tree => "tree ",
            Shape::Wide => "wide ",
            Shape::Rset => "rset ",
        }
    }
    /// `(branches, leaves per branch)`. `branches * (1 + lpb) == NODES` in
    /// both tree shapes, so every arm holds the same number of objects.
    fn geometry(self) -> (usize, usize) {
        match self {
            Shape::Chain | Shape::Rset => (0, 0),
            Shape::Tree => (64, 103),
            Shape::Wide => (512, 12),
        }
    }
    /// Should the holder's own region be kept out of the collection set?
    fn holder_outside_cset(self) -> bool {
        self == Shape::Rset
    }
}

// --- fixture geometry ------------------------------------------------------

/// Total nodes, identical in all three shapes: 64 * 104 == 512 * 13 == 6,656.
const NODES: usize = 6656;
/// Fields per leaf/chain node. Slot 0 is the link (unused on a leaf), slot 1
/// the id, the rest padding so a node is worth copying.
const FIELDS: usize = 32;
const ID_SLOT: usize = 1;
/// A branch's children live in slots `CHILD0 ..`.
const CHILD0: usize = 2;

const CLS_ROOT: u32 = 40;
const CLS_NODE: u32 = 41;
const CLS_JUNK: u32 = 42;

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 128 * 1024 * 1024,
        initial_heap_size: 128 * 1024 * 1024,
        region_size: 1024 * 1024,
        // Two young pauses and a node is Old — see the long note in
        // `g1_lane_d_parallel_mixed.rs`. At the default 15 this fixture
        // tenures nothing, there are no Old regions, and what runs is a young
        // pause wearing a mixed pause's name.
        promotion_age: 2,
        // The old half of the CSet is what makes this a MIXED pause, and the
        // shipped 10% cap plus the pause-goal budget would hold it to a
        // handful of regions — which measures the CAP, not the evacuator.
        // Both are widened here; production defaults are untouched and this
        // deviation is stated in the page rather than left to be inferred.
        old_cset_region_threshold_percent: 100,
        max_gc_pause_ms: 100_000,
        ..Default::default()
    }
}

/// One arm's outcome, in the terms the two arms are compared on.
struct Arm {
    checksum: u64,
    reachable: usize,
    /// `moved[i]` — did the pause evacuate the node whose id is `i`? Keyed by
    /// LOGICAL id, not by address, so it compares across two collectors whose
    /// heap bases differ.
    moved: Vec<bool>,
    forwards: usize,
    /// Pointer-map entries whose value is their key: evacuation failures.
    self_forwards: usize,
    objects_copied: usize,
    bytes_copied: usize,
    bytes_freed: usize,
    /// Per-worker census delta, driver first. Empty on the serial arm.
    census: Vec<EvacWorkerCensus>,
    /// Wall clock. Reported, and load-bearing on nothing on its own — the
    /// counters beside it are what explain it.
    pause_us: u128,
}

fn sized_node(gc: &G1Collector, id: usize, fields: usize) -> ObjectRef {
    let o = gc.alloc_object(ClassId::new(CLS_NODE), fields);
    gc.set_field(o, ID_SLOT, Value::Int(id as i32));
    for f in CHILD0..fields {
        gc.set_field(o, f, Value::Int(id as i32 ^ f as i32));
    }
    o
}

fn node(gc: &G1Collector, id: usize) -> ObjectRef {
    sized_node(gc, id, FIELDS)
}

fn build(gc: &G1Collector, shape: Shape) -> ObjectRef {
    if shape == Shape::Chain {
        let root = gc.alloc_object(ClassId::new(CLS_ROOT), 1);
        let head = node(gc, 0);
        gc.set_field(root, 0, Value::Object(Some(head)));
        let mut prev = head;
        for i in 1..NODES {
            let n = node(gc, i);
            gc.set_field(prev, 0, Value::Object(Some(n)));
            prev = n;
        }
        return root;
    }
    if shape == Shape::Rset {
        let root = gc.alloc_object(ClassId::new(CLS_ROOT), NODES);
        for i in 0..NODES {
            let leaf = node(gc, i);
            gc.set_field(root, i, Value::Object(Some(leaf)));
        }
        return root;
    }
    let (branches, lpb) = shape.geometry();
    assert_eq!(branches * (1 + lpb), NODES, "geometry must hold NODES nodes");
    let root = gc.alloc_object(ClassId::new(CLS_ROOT), branches);
    let mut next_leaf = branches;
    for b in 0..branches {
        let branch = sized_node(gc, b, CHILD0 + lpb);
        gc.set_field(root, b, Value::Object(Some(branch)));
        for c in 0..lpb {
            let leaf = node(gc, next_leaf);
            gc.set_field(branch, CHILD0 + c, Value::Object(Some(leaf)));
            next_leaf += 1;
        }
    }
    assert_eq!(next_leaf, NODES);
    root
}

/// Walk the surviving graph in id order, checking every node's identity and
/// collecting its current address. Panics if a node is missing or has the
/// wrong id, which is the corruption this whole exercise is about.
fn walk(gc: &G1Collector, root: ObjectRef, shape: Shape) -> (u64, Vec<usize>) {
    let mut sum: u64 = 0xcbf2_9ce4_8422_2325;
    let mut addrs = vec![0usize; NODES];
    let mut seen = 0usize;
    let mut visit = |id: usize, o: ObjectRef, expect: usize| {
        assert_eq!(id, expect, "a node came back wearing id {id}, expected {expect}");
        sum = sum.wrapping_mul(0x100_0000_01b3) ^ (id as u64);
        addrs[id] = o.as_ptr() as usize;
        seen += 1;
    };
    if shape == Shape::Chain {
        let Value::Object(Some(head)) = gc.get_field(root, 0) else {
            panic!("the chain head was lost by the mixed pause");
        };
        let mut cur = head;
        for i in 0..NODES {
            let id = gc.get_field(cur, ID_SLOT).as_int().expect("node id") as usize;
            visit(id, cur, i);
            if i + 1 == NODES {
                break;
            }
            let Value::Object(Some(next)) = gc.get_field(cur, 0) else {
                panic!("the chain broke after {i} nodes");
            };
            cur = next;
        }
    } else if shape == Shape::Rset {
        for i in 0..NODES {
            let Value::Object(Some(leaf)) = gc.get_field(root, i) else {
                panic!("holder slot {i} was lost by the mixed pause");
            };
            let id = gc.get_field(leaf, ID_SLOT).as_int().expect("leaf id") as usize;
            visit(id, leaf, i);
        }
    } else {
        let (branches, lpb) = shape.geometry();
        for b in 0..branches {
            let Value::Object(Some(branch)) = gc.get_field(root, b) else {
                panic!("branch {b} was lost by the mixed pause");
            };
            let id = gc.get_field(branch, ID_SLOT).as_int().expect("branch id") as usize;
            visit(id, branch, b);
            for c in 0..lpb {
                let Value::Object(Some(leaf)) = gc.get_field(branch, CHILD0 + c) else {
                    panic!("branch {b} lost child {c}");
                };
                let id = gc.get_field(leaf, ID_SLOT).as_int().expect("leaf id") as usize;
                visit(id, leaf, branches + b * lpb + c);
            }
        }
    }
    assert_eq!(seen, NODES, "the walk did not reach every node");
    (sum, addrs)
}

fn old_region_count(gc: &G1Collector) -> usize {
    let mut n = 0;
    gc.with_regions_mut(|regions| {
        n = regions
            .iter()
            .filter(|r| format!("{:?}", r.region_type()) == "Old")
            .count();
    });
    n
}

fn census_delta(before: &[EvacWorkerCensus], after: &[EvacWorkerCensus]) -> Vec<EvacWorkerCensus> {
    after
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let b = before.get(i).copied().unwrap_or_default();
            EvacWorkerCensus {
                pauses: a.pauses - b.pauses,
                seed_regions: a.seed_regions - b.seed_regions,
                seed_objs: a.seed_objs - b.seed_objs,
                seed_bytes: a.seed_bytes - b.seed_bytes,
                scanned: a.scanned - b.scanned,
                objs_copied: a.objs_copied - b.objs_copied,
                bytes_copied: a.bytes_copied - b.bytes_copied,
                lifts: a.lifts - b.lifts,
                lifted: a.lifted - b.lifted,
                spills: a.spills - b.spills,
                idle_ns: a.idle_ns - b.idle_ns,
                idle_probes: a.idle_probes - b.idle_probes,
            }
        })
        .collect()
}

/// Build a heap, age it until the live set is Old, and take one mixed pause on
/// the requested driver.
fn run_arm(shape: Shape, parallel: bool) -> Arm {
    let gc = G1Collector::new(config());
    let root = build(&gc, shape);
    let mut roots = vec![root];

    // Age. Each round allocates garbage so a young pause fires and each
    // survivor's age ticks; at `promotion_age: 2` the live set is Old well
    // before the loop ends.
    for _ in 0..6 {
        for _ in 0..4000 {
            let junk = gc.alloc_object(ClassId::new(CLS_JUNK), 4);
            gc.set_field(junk, 0, Value::Int(7));
        }
        gc.young_collection(&mut roots, &NoopMonitors);
    }

    // The pre-pause fold and the pre-pause addresses. The ageing loop moved
    // everything, so `build`'s addresses are stale; these are the keys the
    // pause's pointer map is probed with. If the pre-pause fold does not match
    // the post-pause one the fixture broke before the pause and the comparison
    // means nothing.
    let (pre_sum, addr_before) = walk(&gc, roots[0], shape);

    let olds = old_region_count(&gc);
    assert!(
        olds > 0,
        "no Old region exists, so `mixed_collection` has no old collection set \
         and would run a young pause under a mixed pause's name"
    );
    // The liveness data a real mixed pause gets from a completed mark cycle.
    // `with_regions_mut` is the sanctioned hook; see `g1_lane_d_parallel_mixed.rs`.
    //
    // For [`Shape::Rset`] the HOLDER's own region is left unstamped, so the
    // old-region selector cannot take it: the holder then survives in place
    // while everything it points at is collected, which is the only way to
    // make the old live set reachable through the REMEMBERED SET rather than
    // through a root. That is the shape a mixed pause actually has, and it is
    // the one that decides which PHASE does the copying.
    let holder_addr = roots[0].as_ptr() as usize;
    let hold_out = shape.holder_outside_cset();
    let mut excluded = 0usize;
    gc.with_regions_mut(|regions| {
        for r in regions.iter_mut() {
            if format!("{:?}", r.region_type()) != "Old" {
                continue;
            }
            let base = r.data.as_ptr() as usize;
            if hold_out && holder_addr >= base && holder_addr < base + r.data.len() {
                excluded += 1;
                continue;
            }
            r.live_bytes = r.cursor().max(1) / 8;
            r.gc_efficiency = 0.1;
        }
    });
    assert_eq!(
        excluded,
        usize::from(hold_out),
        "the holder's region was not found exactly once"
    );

    let before = cratonvm_gc::g1::g1_evac_worker_census();
    let t0 = std::time::Instant::now();
    let result = if parallel {
        gc.mixed_collection_parallel_forced(&mut roots, &NoopMonitors)
    } else {
        gc.mixed_collection(&mut roots, &NoopMonitors)
    };
    let pause_us = t0.elapsed().as_micros();
    let after = cratonvm_gc::g1::g1_evac_worker_census();

    let moved: Vec<bool> = addr_before
        .iter()
        .map(|a| result.pointer_map.contains_key(a))
        .collect();
    let self_forwards = result.pointer_map.iter().filter(|(k, v)| *k == *v).count();

    let (post_sum, _) = walk(&gc, roots[0], shape);
    assert_eq!(
        pre_sum,
        post_sum,
        "the {} mixed pause on the {} shape changed the surviving graph",
        if parallel { "PARALLEL" } else { "serial" },
        shape.label().trim()
    );

    // ...and the collector still works afterwards.
    let fresh = node(&gc, 12345);
    let mut roots2 = vec![fresh];
    gc.young_collection(&mut roots2, &NoopMonitors);
    assert_eq!(gc.get_field(roots2[0], ID_SLOT).as_int(), Some(12345));

    Arm {
        checksum: post_sum,
        reachable: NODES,
        moved,
        forwards: result.pointer_map.len(),
        self_forwards,
        objects_copied: result.stats.objects_copied,
        bytes_copied: result.stats.bytes_copied,
        bytes_freed: result.stats.bytes_freed,
        census: if parallel {
            census_delta(&before, &after)
        } else {
            Vec::new()
        },
        pause_us,
    }
}

/// The copy-span report for one parallel arm.
struct Span {
    engaged: usize,
    total: u64,
    max: u64,
    ratio: f64,
    lifts: u64,
    lifted: u64,
    spills: u64,
    idle_ms: u64,
}

fn span(census: &[EvacWorkerCensus]) -> Span {
    let per: Vec<u64> = census.iter().map(EvacWorkerCensus::total_bytes).collect();
    let total: u64 = per.iter().sum();
    let max = per.iter().copied().max().unwrap_or(0);
    Span {
        engaged: per.iter().filter(|b| **b > 0).count(),
        total,
        max,
        ratio: if max == 0 {
            0.0
        } else {
            total as f64 / max as f64
        },
        lifts: census.iter().map(|c| c.lifts).sum(),
        lifted: census.iter().map(|c| c.lifted).sum(),
        spills: census.iter().map(|c| c.spills).sum(),
        idle_ms: census.iter().map(|c| c.idle_ns).sum::<u64>() / 1_000_000,
    }
}

fn report(label: &str, serial: &Arm, parallel: &Arm) -> Span {
    let s = span(&parallel.census);
    println!(
        "[W6-P] {label}: serial copied={}B/{} objs freed={}B forwards={} ({} self) pause={}us",
        serial.bytes_copied,
        serial.objects_copied,
        serial.bytes_freed,
        serial.forwards,
        serial.self_forwards,
        serial.pause_us
    );
    println!(
        "[W6-P] {label}: parall copied={}B/{} objs freed={}B forwards={} ({} self) pause={}us",
        parallel.bytes_copied,
        parallel.objects_copied,
        parallel.bytes_freed,
        parallel.forwards,
        parallel.self_forwards,
        parallel.pause_us
    );
    println!(
        "[W6-P] {label}: workers_that_copied={} copied_total={}B max_worker={}B \
         SPAN_RATIO={:.2} lifts={} lifted={} spills={} idle_ms={}",
        s.engaged, s.total, s.max, s.ratio, s.lifts, s.lifted, s.spills, s.idle_ms
    );
    let per: Vec<u64> = parallel
        .census
        .iter()
        .map(EvacWorkerCensus::total_bytes)
        .collect();
    println!("[W6-P] {label}: per-worker bytes (driver first) = {per:?}");
    let scanned: Vec<u64> = parallel.census.iter().map(|c| c.scanned).collect();
    println!("[W6-P] {label}: per-worker scanned             = {scanned:?}");
    // The PHASE split, which is what decides whether the lever can help at
    // all: `seed_bytes` is everything copied before the Phase-3 closure began
    // (roots, keep-alives, the remembered-set source walk), and the seed walk
    // is driver-only unless `CRATONVM_G1_PARALLEL_SEED=1`.
    let seed: u64 = parallel.census.iter().map(|c| c.seed_bytes).sum();
    let seed_regions: u64 = parallel.census.iter().map(|c| c.seed_regions).sum();
    println!(
        "[W6-P] {label}: PHASE-2 seed bytes={seed} ({:.1}% of the copy) over \
         {seed_regions} source regions",
        if s.total == 0 {
            0.0
        } else {
            100.0 * seed as f64 / s.total as f64
        }
    );
    s
}

fn compare(label: &str, serial: &Arm, parallel: &Arm) {
    assert_eq!(
        serial.checksum, parallel.checksum,
        "{label}: the two mixed drivers produced DIFFERENT surviving graphs"
    );
    assert_eq!(serial.reachable, parallel.reachable, "{label}: node count");
    // Identical forwarding outcomes, keyed by logical id so the two
    // collectors' different heap bases do not enter into it.
    let diff: Vec<usize> = (0..NODES)
        .filter(|i| serial.moved[*i] != parallel.moved[*i])
        .collect();
    assert!(
        diff.is_empty(),
        "{label}: the two drivers disagreed about which objects to evacuate \
         for {} of {NODES} nodes (first few: {:?})",
        diff.len(),
        &diff[..diff.len().min(8)]
    );
    let moved_n = serial.moved.iter().filter(|m| **m).count();
    assert!(
        moved_n * 2 > NODES,
        "{label}: the pause evacuated only {moved_n} of {NODES} nodes, so the \
         comparisons above are mostly between two no-ops"
    );
    assert_eq!(
        serial.objects_copied, parallel.objects_copied,
        "{label}: objects_copied"
    );
    assert_eq!(
        serial.bytes_copied, parallel.bytes_copied,
        "{label}: bytes_copied"
    );
    assert_eq!(
        serial.bytes_freed, parallel.bytes_freed,
        "{label}: bytes_freed"
    );
    assert_eq!(
        serial.self_forwards, parallel.self_forwards,
        "{label}: evacuation failures (self-forwards) must not differ by arm"
    );
}

#[test]
fn the_two_mixed_drivers_agree_and_the_parallel_one_has_a_span_ratio() {
    // The lever's own accessor latches on first read; this test never sets it,
    // so `mixed_collection` below takes the SERIAL body and the parallel arm
    // is reached through the W6-P hook. The route census is asserted as a
    // delta (process-global — see `g1_mixed_route_counts`).
    let (rs0, rp0) = cratonvm_gc::g1::g1_mixed_route_counts();

    const BATCHES: usize = 2;
    const SHAPES: [Shape; 4] = [Shape::Chain, Shape::Tree, Shape::Wide, Shape::Rset];
    let mut ratios: Vec<Vec<f64>> = vec![Vec::new(); SHAPES.len()];
    let mut clocks: Vec<Vec<(u128, u128)>> = vec![Vec::new(); SHAPES.len()];

    // Two batches. A rate is not a measurement until a second batch reproduces
    // its order, and the order here is "wide spreads, chain and tree do not".
    for batch in 0..BATCHES {
        for (si, shape) in SHAPES.iter().copied().enumerate() {
            let serial = run_arm(shape, false);
            let parallel = run_arm(shape, true);
            compare(shape.label(), &serial, &parallel);
            let s = report(&format!("{} b{batch}", shape.label()), &serial, &parallel);
            assert!(
                parallel.census.iter().any(|c| c.pauses > 0),
                "{}: the parallel arm did not enter `parallel_evacuate` at all",
                shape.label()
            );
            ratios[si].push(s.ratio);
            clocks[si].push((serial.pause_us, parallel.pause_us));
        }
    }

    let (rs1, rp1) = cratonvm_gc::g1::g1_mixed_route_counts();
    // Six serial pauses went through `mixed_collection`. The six parallel ones
    // went through the W6-P hook, which is NOT the dispatcher, so the parallel
    // route count must not move — the check that the hook has not quietly
    // become the same path.
    assert_eq!(
        rs1 - rs0,
        (BATCHES * SHAPES.len()) as u64,
        "the mixed route census did not see every serial dispatch"
    );
    assert_eq!(
        rp1 - rp0,
        0,
        "the W6-P hook incremented the DISPATCHER's parallel count, so the two \
         arms are not distinguishable in the census"
    );
    println!(
        "[W6-P] mixed-route delta over this test: serial={} parallel={} \
         (the parallel arm is the hook, by design)",
        rs1 - rs0,
        rp1 - rp0
    );
    for (si, shape) in SHAPES.iter().enumerate() {
        println!(
            "[W6-P] {} span ratios {:?} pause us (serial,parallel) {:?}",
            shape.label(),
            ratios[si],
            clocks[si]
        );
    }

    let lo = |v: &Vec<f64>| v.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = |v: &Vec<f64>| v.iter().cloned().fold(0.0f64, f64::max);

    // THE NULL ARM. A chain's frontier is one object wide by construction, so
    // no evacuator can spread its copying and ~1.00 is the CORRECT reading. If
    // this ever comes back large, the instrument is measuring something other
    // than what it names and every number in this file is void.
    assert!(
        hi(&ratios[0]) < 1.60,
        "the CHAIN shape reported a span ratio of {:.2}. A chain cannot be \
         evacuated in parallel, so this is the instrument reporting \
         parallelism that does not exist: {:?}",
        hi(&ratios[0]),
        ratios[0]
    );
    // THE POSITIVE CONTROL. `wide` puts more than `EVAC_LOCAL_PUBLISH_HIGH`
    // (256) children under one object, which is what makes the driver publish
    // to the shared queue at all. If this stops spreading, the instrument can
    // no longer tell parallelism from its absence and the chain reading above
    // stops meaning anything.
    assert!(
        lo(&ratios[2]) > 1.60,
        "the WIDE shape did not spread its copying either ({:?}), so this file \
         can no longer distinguish a shape the evacuator can help from one it \
         cannot",
        ratios[2]
    );
    assert!(
        lo(&ratios[2]) > hi(&ratios[0]),
        "the two shapes' span ratios overlap across batches, so the difference \
         between them is not reproducible: chain={:?} wide={:?}",
        ratios[0],
        ratios[2]
    );

    // `tree` IS THE FINDING, and it is asserted loosely on purpose.
    //
    // 64 independent branches is wide at the GRAPH level — nearly three times
    // the worker count — and it still reports ~1.00, because the local stack
    // is LIFO and a depth-first walk of a fanout-100 tree never grows past the
    // 256-entry publish threshold: each branch's children are consumed before
    // the next branch is popped, so the driver never spills and the helpers
    // never see work. The assertion here is only that the number is sane;
    // a future change that raises it has FIXED something, and the page
    // (`docs/internal/g1-2026-09-20/w6p-*.md`) is what should then be updated.
    assert!(
        lo(&ratios[1]) >= 0.99,
        "the TREE shape reported a span ratio below 1 ({:?}), which is \
         arithmetically impossible unless the census is being double-counted",
        ratios[1]
    );
    if lo(&ratios[1]) > 1.60 {
        println!(
            "[W6-P] NOTE: the tree shape now spreads ({:?}). The W6-P page \
             records it as NOT spreading; the finding has changed and the page \
             is stale.",
            ratios[1]
        );
    }
}
