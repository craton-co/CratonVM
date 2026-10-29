//! The idle-peer spill (`CRATONVM_G1_EVAC_IDLE_SPILL`, `SharedEvac::run_worker`)
//! on the shape it exists for: a binary tree hanging off ONE root.
//!
//! A depth-first walk of such a tree keeps about `depth` un-popped siblings on
//! its local stack, far below the 256-entry valve, so before 2026-10-06 the
//! worker that took the root scanned every object while its peers idled (binary
//! trees at depth 20: driver scan share 99.1%). Two arms, one young pause each
//! on the parallel evacuator:
//!
//! * spill OFF: the census must show no idle spill at all (the kill switch is
//!   real);
//! * spill ON: whenever helpers were dispatched, at least one idle spill (the
//!   positive control for that zero), and in both arms the surviving tree must
//!   fold to the same checksum it had before the pause.
//!
//! One `#[test]` per binary: the worker census is a process-global table.
//!
//! ```text
//! cargo test -p cratonvm-gc --test g1_idle_spill_one_root_tree -- --nocapture
//! ```

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::flags;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const DEPTH: u32 = 15;
const FIELDS: usize = 3;
const ID_SLOT: usize = 0;
const LEFT: usize = 1;
const RIGHT: usize = 2;
const CLS_NODE: u32 = 71;

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 256 * 1024 * 1024,
        initial_heap_size: 256 * 1024 * 1024,
        region_size: 1024 * 1024,
        max_gc_pause_ms: 100_000,
        ..Default::default()
    }
}

/// Builds the tree bottom-up, numbering nodes in post-order.
fn build(gc: &G1Collector, depth: u32, next: &mut i32) -> ObjectRef {
    let n = gc.alloc_object(ClassId::new(CLS_NODE), FIELDS);
    if depth > 0 {
        let l = build(gc, depth - 1, next);
        let r = build(gc, depth - 1, next);
        gc.set_field(n, LEFT, Value::Object(Some(l)));
        gc.set_field(n, RIGHT, Value::Object(Some(r)));
    }
    gc.set_field(n, ID_SLOT, Value::Int(*next));
    *next += 1;
    n
}

/// Post-order fold of the ids, and the node count.
fn fold(gc: &G1Collector, n: ObjectRef, sum: &mut u64, seen: &mut usize) {
    if let Value::Object(Some(l)) = gc.get_field(n, LEFT) {
        fold(gc, l, sum, seen);
    }
    if let Value::Object(Some(r)) = gc.get_field(n, RIGHT) {
        fold(gc, r, sum, seen);
    }
    let id = gc.get_field(n, ID_SLOT).as_int().expect("node id");
    *sum = sum.wrapping_mul(0x100_0000_01b3) ^ id as u64;
    *seen += 1;
}

/// One pause; returns (idle spills, helpers dispatched) from the census delta.
fn run_arm(label: &str) -> (u64, usize) {
    let gc = G1Collector::new(config());
    let mut next = 0;
    let root = build(&gc, DEPTH, &mut next);
    let nodes = (1usize << (DEPTH + 1)) - 1;
    assert_eq!(next as usize, nodes);
    let (mut pre, mut pre_seen) = (0xcbf2_9ce4_8422_2325u64, 0usize);
    fold(&gc, root, &mut pre, &mut pre_seen);
    assert_eq!(pre_seen, nodes);

    let before = cratonvm_gc::g1::g1_evac_worker_census();
    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);
    let after = cratonvm_gc::g1::g1_evac_worker_census();

    let (mut post, mut post_seen) = (0xcbf2_9ce4_8422_2325u64, 0usize);
    fold(&gc, roots[0], &mut post, &mut post_seen);
    assert_eq!(post_seen, nodes, "{label}: the pause lost nodes");
    assert_eq!(post, pre, "{label}: the pause changed the surviving tree");

    let mut idle_spills = 0u64;
    let mut scanned = 0u64;
    let mut helpers = 0usize;
    for (i, a) in after.iter().enumerate() {
        let b = before.get(i).copied().unwrap_or_default();
        idle_spills += a.idle_spills - b.idle_spills;
        scanned += a.scanned - b.scanned;
        if i > 0 && a.pauses > b.pauses {
            helpers += 1;
        }
    }
    assert!(
        scanned > 0,
        "{label}: the parallel evacuator scanned nothing; this arm measured no closure"
    );
    println!("{label}: nodes={nodes} scanned={scanned} helpers_dispatched={helpers} idle_spills={idle_spills}");
    (idle_spills, helpers)
}

/// One pause of a small tree on a 16-region heap; returns the width the pause
/// evacuated at (`G1_EVAC_WORKERS_LAST`).
fn small_heap_width() -> usize {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        max_gc_pause_ms: 100_000,
        ..Default::default()
    });
    let mut next = 0;
    let root = build(&gc, 10, &mut next);
    let (mut pre, mut seen) = (0xcbf2_9ce4_8422_2325u64, 0usize);
    fold(&gc, root, &mut pre, &mut seen);
    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);
    let (mut post, mut post_seen) = (0xcbf2_9ce4_8422_2325u64, 0usize);
    fold(&gc, roots[0], &mut post, &mut post_seen);
    assert_eq!(
        (post, post_seen),
        (pre, seen),
        "small heap: the pause changed the tree"
    );
    cratonvm_gc::g1::G1_EVAC_WORKERS_LAST.load(std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn an_idle_peer_is_fed_from_a_one_root_tree_and_the_switch_turns_it_off() {
    let base = [
        ("CRATONVM_G1_PARALLEL_EVAC", Some("1")),
        ("CRATONVM_G1_EVAC_LOCAL_QUEUE", Some("1")),
        ("CRATONVM_G1_EVAC_CENSUS", Some("1")),
    ];
    let mut off = base.to_vec();
    off.push(("CRATONVM_G1_EVAC_IDLE_SPILL", Some("0")));
    let (spills_off, _) = flags::with_thread_overrides(&off, || run_arm("spill-off"));
    assert_eq!(
        spills_off, 0,
        "CRATONVM_G1_EVAC_IDLE_SPILL=0 must disable the idle spill"
    );

    let mut on = base.to_vec();
    on.push(("CRATONVM_G1_EVAC_IDLE_SPILL", Some("1")));
    let (spills_on, helpers) = flags::with_thread_overrides(&on, || run_arm("spill-on"));
    if helpers > 0 {
        assert!(
            spills_on > 0,
            "{helpers} helper(s) were dispatched for a one-root tree of 2^{} nodes and \
             none was ever fed below the 256 valve",
            DEPTH + 1
        );
    }

    // `CRATONVM_G1_EVAC_WORKERS_BY_POOL`: a 16-region heap has at most 16
    // regions of to-space pool, so at most two workers (one per eight), each
    // holding whole-region TLABs.
    let width = flags::with_thread_overrides(&on, small_heap_width);
    assert!(
        (1..=2).contains(&width),
        "a 16-region heap evacuated at width {width}; the pool cap allows at most 2"
    );
}
