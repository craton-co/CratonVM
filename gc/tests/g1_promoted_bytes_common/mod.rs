// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The one workload both G1 evacuators have to produce the same promotion
//! figure for.
//!
//! # What is being measured and why it needed a counter
//!
//! `interpreter::note_gc_productivity` decides whether a forced collection was
//! PRODUCTIVE, and `GC_OVERHEAD_LIMIT_CYCLES` consecutive unproductive ones
//! throw `OutOfMemoryError: GC overhead limit exceeded`. One of its three
//! inputs is `VmHeap::bytes_promoted_total`, which exists for a stated reason:
//! "a promotion-only cycle conserves live bytes but did useful
//! allocation-enabling work".
//!
//! **G1 promotes.** Its young and mixed pauses evacuate Eden and Survivor
//! objects into Old regions; that is what a G1 pause mostly does. Until
//! 2026-09-21 the collector had no promoted-bytes counter of any kind, so the
//! dispatcher answered a hard `0` and the one credit the metric grants for
//! draining young was inexpressible on the backend whose pauses consist of
//! draining young
//! (`docs/internal/gc/heap-gc-overhead-limit-reads-two-hard-zeros-on-g1-and-zgc-20260920-RETIRED-20260921.md`).
//!
//! # Why the workload is shared and the binaries are not
//!
//! There are TWO evacuators -- `G1Collector::evacuate_object` (serial) and
//! `SharedEvac::evacuate` (parallel) -- and they install their forwarding
//! words differently: the serial one stores, the parallel one CASes and can
//! lose. A promotion counter bumped on the wrong side of that CAS is inflated
//! in proportion to the worker count, which is exactly the defect the F-18
//! survivor histogram had and had fixed on 2026-09-20. So the figure has to be
//! asserted on both arms, and the arm is `CRATONVM_G1_PARALLEL_EVAC`, which
//! `flags()` latches once per process -- one arm is one binary.
//!
//! What both arms must agree on is not an exact byte count (the parallel arm
//! allocates out of per-worker TLABs and may place objects differently) but the
//! two properties that make the figure a promotion figure at all: it is
//! non-zero once the fixture tenures, and it is bounded by what the heap
//! actually holds. A counter bumped per CAS attempt rather than per published
//! copy breaks the upper bound; a counter that is never bumped breaks the
//! lower one.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Objects in the live set. Large enough to spread over several regions, so
/// the parallel arm genuinely dispatches more than one worker and the CAS
/// races this is about are reachable.
const HOLDERS: usize = 4000;
const FIELDS: usize = 2;

/// `promotion_age: 2`, not the default 15, for the reason
/// `g1_lane_d_parallel_seed` states at length: at 15 a fixture that ages its
/// holders a handful of times tenures NOTHING, every assertion about promotion
/// passes vacuously, and the test measures the ordinary survivor path while
/// claiming to measure promotion.
fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        promotion_age: 2,
        ..Default::default()
    }
}

/// `(bytes promoted, bytes the heap holds afterwards)`.
pub fn run() -> (u64, u64) {
    let gc = G1Collector::new(config());

    assert_eq!(
        gc.bytes_promoted_total(),
        0,
        "a collector that has not run a pause has promoted nothing",
    );

    let root = gc.alloc_object(ClassId::new(22), HOLDERS);
    for h in 0..HOLDERS {
        let holder = gc.alloc_object(ClassId::new(21), FIELDS);
        gc.set_field(holder, 1, Value::Int(h as i32));
        gc.set_field(root, h, Value::Object(Some(holder)));
    }
    let mut roots = vec![root];

    // Age the holders past the tenuring threshold, interleaving garbage so each
    // pause has a real Eden to evacuate rather than an empty one.
    for _ in 0..6 {
        for _ in 0..2000 {
            let junk = gc.alloc_object(ClassId::new(23), 2);
            gc.set_field(junk, 0, Value::Int(0));
        }
        gc.young_collection(&mut roots, &NoopMonitors);
    }

    // The graph has to still BE a graph. A promotion figure taken from a pause
    // that lost the live set is a count of nothing in particular.
    let root = roots[0];
    for h in 0..HOLDERS {
        let Value::Object(Some(holder)) = gc.get_field(root, h) else {
            panic!("holder {h} was lost while ageing");
        };
        assert_eq!(
            gc.get_field(holder, 1).as_int(),
            Some(h as i32),
            "holder {h} did not survive the ageing intact",
        );
    }

    (gc.bytes_promoted_total(), gc.allocated_bytes() as u64)
}

/// Assert the two properties both arms must have. Takes the arm's name so a
/// failure says which binary produced it.
pub fn assert_promotion_is_counted(arm: &str, (promoted, live): (u64, u64)) {
    assert!(
        promoted > 0,
        "{arm}: the fixture tenured {HOLDERS} holders and the collector reports \
         ZERO bytes promoted. G1 pauses ARE young drains, so a hard zero here \
         is the GC-overhead limit's one promotion credit being inexpressible on \
         this backend -- the defect \
         docs/internal/gc/heap-gc-overhead-limit-reads-two-hard-zeros-on-g1-and-zgc-20260920-RETIRED-20260921.md \
         was opened about.",
    );

    // A per-object floor: every promoted object carries at least its header, so
    // a counter wired to the wrong quantity (a COUNT of objects, a region
    // total) fails this without the test having to know the object layout.
    assert!(
        promoted >= HOLDERS as u64,
        "{arm}: {promoted} bytes promoted for {HOLDERS} tenured holders is below \
         one byte apiece; the counter is reading a different quantity from the \
         one it is named for",
    );

    // THE UPPER BOUND IS THE ONE THE CAS RACE BREAKS. The parallel evacuator
    // CASes its forwarding word, and a loser's copy is abandoned to-space
    // garbage that no reference names. A counter bumped before that CAS counts
    // every loser, so it grows with the worker count and can exceed what the
    // heap actually holds. Only published copies are promoted bytes.
    assert!(
        promoted <= live,
        "{arm}: {promoted} bytes reported promoted, but the heap only holds \
         {live} allocated bytes. A promotion counted per CAS ATTEMPT rather \
         than per PUBLISHED copy counts every racing worker's abandoned copy -- \
         the same defect the F-18 survivor histogram had on 2026-09-20.",
    );
}

/// The same fixture through `VmHeap`, which is where the metric actually reads
/// it. A counter that exists on `G1Collector` and is swallowed by the
/// dispatcher is the exact shape this page recorded for ZGC: a producer with no
/// consumer, invisible to every test that asks the collector directly.
pub fn assert_the_dispatcher_delegates(arm: &str) {
    use cratonvm_gc::vm_heap::{G1ConfigOverrides, GcBackend, VmHeap};

    let heap = VmHeap::new_with_overrides(
        GcBackend::G1,
        64 * 1024 * 1024,
        G1ConfigOverrides::default(),
    );
    assert_eq!(
        heap.bytes_promoted_total(),
        0,
        "{arm}: nothing collected yet"
    );

    let VmHeap::G1(state) = &heap else {
        unreachable!("asked for G1")
    };
    let gc: &G1Collector = &state.collector;

    let root = gc.alloc_object(ClassId::new(22), HOLDERS);
    for h in 0..HOLDERS {
        let holder = gc.alloc_object(ClassId::new(21), FIELDS);
        gc.set_field(root, h, Value::Object(Some(holder)));
    }
    // SIXTEEN pauses, not the six `run` uses. `G1ConfigOverrides` carries no
    // `promotion_age` -- it is the `-XX:` knob set, and there is no `-XX:` knob
    // for the tenuring threshold -- so a heap built through the dispatcher runs
    // at the default 15 and an object has to survive fifteen collections before
    // anything is promoted. Ageing six times here would tenure nothing and the
    // assertion below would be vacuous, which is the failure mode
    // `g1_lane_d_parallel_seed` documents for this exact fixture.
    let mut roots: Vec<ObjectRef> = vec![root];
    for _ in 0..16 {
        for _ in 0..500 {
            let junk = gc.alloc_object(ClassId::new(23), 2);
            gc.set_field(junk, 0, Value::Int(0));
        }
        gc.young_collection(&mut roots, &NoopMonitors);
    }

    // The counter on the collector itself must have moved, or the assertion
    // below cannot tell a swallowing dispatcher from a fixture that promoted
    // nothing -- and "the fixture promoted nothing" is the way this test goes
    // quietly vacuous.
    assert!(
        gc.bytes_promoted_total() > 0,
        "{arm}: the fixture did not tenure anything in 16 pauses, so this test          is asserting nothing about the dispatcher. Check the tenuring          threshold before reading the assertion below.",
    );

    assert!(
        heap.bytes_promoted_total() > 0,
        "{arm}: `G1Collector::bytes_promoted_total` moved but \
         `VmHeap::bytes_promoted_total` still reads zero -- the dispatcher arm \
         is swallowing the counter, which is the producer-with-no-consumer half \
         of the page this closes",
    );
}
