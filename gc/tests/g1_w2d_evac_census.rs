// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane D wave 2 — the parallel evacuator's PER-WORKER engagement census.
//!
//! # What this file is defending
//!
//! Before 2026-09-20 the evacuator published two numbers about its own
//! parallelism, `parallel=`/`serial=` and `workers_last=`, and both of them
//! are about the DISPATCH. A pause that woke 23 workers and then copied every
//! byte on the driver thread produced character-for-character the same output
//! as a pause that spread the work evenly. `lane-d-proposals.md` §3 names that
//! gap as the thing blocking evaluation of proposals 3 and 4 — a real
//! termination protocol and per-worker deques — because both are changes to
//! how work REACHES a worker.
//!
//! So the property under test is not "the numbers are plausible". It is:
//!
//!  * the census EXISTS after a pause that evacuated (a census that only
//!    appears under a flag nobody sets is the same as no census);
//!  * it is a SPLIT, indexed by worker, with the driver at row 0 — a total
//!    would not have answered the question that motivated it;
//!  * the report line is emitted UNCONDITIONALLY, including the "nothing ran"
//!    form, because a census that prints only when it is non-empty cannot be
//!    told apart from a census whose report never ran. That is the exact
//!    failure `gc_metrics::collector_decision_report` was created to fix for
//!    the G1 guard counters.
//!
//! # Why the assertions are shaped the way they are
//!
//! They are about STRUCTURE and INTERNAL CONSISTENCY, never about a particular
//! worker doing a particular amount of work. The census counters are process
//! statics shared with every other test in this binary, and the width of a
//! pause is `available_parallelism()` on the host — so an assertion like "at
//! least two workers scanned something" would be asserting the test machine,
//! which is how `dbg_mark_worker_scans`'s own probe had to be written as a
//! retry loop with an explicit one-worker bail-out. There is no wall-clock
//! bound anywhere in this file for the same reason.

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
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }
}

const FANOUT: usize = 4;
const FIELDS: usize = FANOUT + 1;
const ID_SLOT: usize = FANOUT;

/// A tree wide enough that the closure has a real frontier to spread: 4^6 is
/// 4096 nodes on the widest level, against a 256-entry per-worker local stack,
/// so every worker that participates at all both lifts and spills.
fn build_tree(gc: &G1Collector, depth: u32) -> (ObjectRef, usize) {
    fn rec(gc: &G1Collector, depth: u32, next: &mut i32) -> ObjectRef {
        let node = gc.alloc_object(ClassId::new(1), FIELDS);
        let id = *next;
        *next += 1;
        gc.set_field(node, ID_SLOT, Value::Int(id));
        if depth > 0 {
            for slot in 0..FANOUT {
                let child = rec(gc, depth - 1, next);
                gc.set_field(node, slot, Value::Object(Some(child)));
            }
        }
        node
    }
    let mut next = 0;
    let root = rec(gc, depth, &mut next);
    (root, next as usize)
}

/// The census is populated by a default (parallel) young pause, and it is a
/// SPLIT rather than a total.
#[test]
fn a_parallel_young_pause_publishes_a_per_worker_row() {
    let gc = G1Collector::new(config());
    let (root, count) = build_tree(&gc, 6);
    assert!(count > 4000, "frontier too small to be worth measuring");
    let mut roots = vec![root];
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    assert_eq!(
        result.stats.objects_copied, count,
        "the pause did not evacuate the tree, so anything the census says \
         about it is about a different pause"
    );

    let rows = cratonvm_gc::g1::g1_evac_worker_census();
    assert!(
        !rows.is_empty(),
        "no census row after a pause that copied {count} objects — the \
         instrument is not wired to the evacuator at all"
    );
    // Row 0 is the driver, and the driver ALWAYS participates as a worker, so
    // its row must be present whatever the helper width turned out to be.
    // This is the shape assertion: a row per worker, not one merged total.
    let total_scanned: u64 = rows.iter().map(|r| r.scanned).sum();
    assert!(
        total_scanned > 0,
        "the pause scanned {count} objects but every census row reads zero: \
         the counters are being published from the wrong side of the merge"
    );
    assert!(
        rows[0].pauses > 0,
        "the driver's row records no pause; row 0 is by contract the driver \
         and the driver cannot sit a pause out"
    );
}

/// The report line carries every field an operator is told to read, and says
/// so in the form the shutdown report emits.
///
/// Asserted on the KEYS, not the values: the counters behind them are process
/// statics every other test in this binary shares, so pinning a value would be
/// asserting test ordering. Presence is the property — the whole reason this
/// census exists is that the previous instruments were silent in the case that
/// mattered.
#[test]
fn the_report_line_names_every_field_it_promises() {
    let gc = G1Collector::new(config());
    let (root, _) = build_tree(&gc, 4);
    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);

    let text = cratonvm_gc::g1::g1_evac_worker_census_report();
    for needle in [
        "[GC] g1 evac-workers:",
        "rows=",
        "scanned=",
        "copied_objs=",
        "copied_bytes=",
        "seed_regions=",
        "lifts=",
        "lifted=",
        "spills=",
        "idle_ms=",
        "driver_scan_share=",
        "never_scanned_helpers=",
        // Lane W7-C — the denominator both `never_*_helpers` counts are read
        // against. Zero rows in the tail mean "dispatched and idle" only if
        // something says they were dispatched.
        "helpers_dispatched=",
        "pool_dispatch_while_busy=",
        // Lane W7-C — the expensive column. `scanned` is the reference walk;
        // these are the copy. A report that prints only the first can call a
        // pause well balanced while one thread does every byte of its copying.
        "[GC] g1 evac-copy-span:",
        "copying_pauses=",
        "empty_pauses=",
        "max_worker_bytes=",
        "span_ratio=",
        "solo_copier_pauses=",
        "copier_slots=",
        "avg_copiers_per_pause=",
        "workers_that_copied=",
        "never_copied_helpers=",
        "lifetime_span_ratio=",
        "driver_copy_share=",
        "[GC] g1 evac-worker[0] (driver):",
        "total_bytes=",
    ] {
        assert!(
            text.contains(needle),
            "the census report must carry `{needle}`; it is what an operator \
             is told to read. Report was:\n{text}"
        );
    }
    // Lane W7-C — the rename is the point, not an accident of formatting. The
    // old key was a share of `scanned` under a name that did not say so, and
    // wave 6 found the round had read it as the work distribution for four
    // waves. A bare `driver_share=` must not come back.
    assert!(
        !text.contains("driver_share="),
        "`driver_share=` is ambiguous between the scan column and the copy \
         column and must not appear; use `driver_scan_share=` or \
         `driver_copy_share=`. Report was:\n{text}"
    );
    // The per-worker rows are the point of the whole instrument; a report that
    // printed only the summary would be the total this replaced.
    assert!(
        text.lines().count() >= 3,
        "the report collapsed — the SPLIT is the instrument, not the total, \
         and the copy span is its own line:\n{text}"
    );
}

/// Lane W7-C — the copy span is a PAUSE-weighted ratio, and the population it
/// is taken over moves when a pause copies.
///
/// The failure this pins is the one the lifetime table cannot see: summing each
/// worker's bytes over the process and dividing by the largest row reports the
/// balance of a FICTIONAL pause. If a different worker copies everything in
/// each pause, the lifetime rows come out equal and the ratio rises with the
/// run length while every individual pause was 1.00. So the accumulator has to
/// advance once per pause, which is what this asserts.
///
/// A DELTA, and a BOUND rather than an equality. `g1_evac_copy_span` is
/// process-lifetime state; the four other tests in this binary run in parallel
/// threads of the same process and take their own pauses, and `build_tree`
/// itself can trigger one. Pinning `delta == 1` measured six on the first run.
/// An exact count here would be asserting test scheduling, which is the mistake
/// `w3c-instrumentation-audit.md` §7 warns about one level up.
#[test]
fn the_copy_span_population_advances_once_per_pause() {
    let gc = G1Collector::new(config());
    let (root, count) = build_tree(&gc, 5);
    assert!(count > 0, "fixture built nothing");
    let mut roots = vec![root];

    let (p0, e0, t0, m0, s0, c0) = cratonvm_gc::g1::g1_evac_copy_span();
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    let (p1, e1, t1, m1, s1, c1) = cratonvm_gc::g1::g1_evac_copy_span();

    assert!(
        (p1 - p0) + (e1 - e0) >= 1,
        "a parallel evacuation ran and the span population did not advance at \
         all — a ratio whose denominator does not track the pauses it is taken \
         over is not a per-pause ratio"
    );
    if result.stats.bytes_copied > 0 {
        assert!(
            p1 - p0 >= 1,
            "this pause copied {} bytes, so at least one copying pause must \
             have been counted",
            result.stats.bytes_copied
        );
        assert!(
            t1 - t0 > 0 && m1 - m0 > 0,
            "a copying pause must contribute to BOTH the numerator and the \
             denominator of the span; publishing one without the other is how \
             a ratio loses its denominator"
        );
        assert!(
            t1 - t0 >= m1 - m0,
            "total bytes cannot be below the largest single worker's bytes; \
             span_ratio < 1.00 would mean the max is not drawn from the sum"
        );
        assert!(
            c1 - c0 >= p1 - p0,
            "every copying pause has at least one copier, so `copier_slots` — \
             the denominator `avg_copiers_per_pause` is read against — cannot \
             advance more slowly than `copying_pauses`"
        );
    }
    // Absolute invariants, which hold whatever else the binary is doing: a
    // solo-copier pause is a copying pause, and the pause-weighted numerator
    // can never sit below its own denominator.
    assert!(
        s1 <= p1 && s0 <= p0,
        "solo_copier_pauses ({s1}) exceeded copying_pauses ({p1}) — the \
         headline reading is counted over a population it is not a subset of"
    );
    assert!(
        t1 >= m1,
        "Σ total_bytes ({t1}) fell below Σ max_worker_bytes ({m1}), which \
         would print a span_ratio below 1.00 — impossible, since the max is a \
         term of the sum"
    );
}

/// The shared-destination fallback never proves emptiness more often than it
/// is asked, and never claims a success it did not have.
///
/// This is the invariant behind the futile-pass latch (`CRATONVM_G1_EVAC_DEST_LATCH`).
/// `proved_empty` counts full passes that found nothing; it is recorded only
/// on the path that also increments `tlab_pool_exhausted`, so it can never
/// exceed it. The two being EQUAL is the pre-latch behaviour — every exhausted
/// allocation paying its own O(regions) pass — and the latch's whole purpose
/// is to drive the first far below the second.
#[test]
fn the_shared_destination_latch_cannot_outrun_the_exhaustion_it_serves() {
    let gc = G1Collector::new(config());
    let (root, _) = build_tree(&gc, 5);
    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);

    let proved = cratonvm_gc::g1::parallel_shared_dest_proved_empty();
    let exhausted = cratonvm_gc::g1::PARALLEL_TLAB_POOL_EXHAUSTED
        .load(std::sync::atomic::Ordering::Relaxed) as u64;
    assert!(
        proved <= exhausted,
        "shared_dest_proved_empty={proved} exceeds tlab_pool_exhausted={exhausted}: \
         the latch is recording proofs on a path that did not exhaust, which \
         would let it refuse an allocation the unlatched scan would have served"
    );
}

/// The pool's dispatch-while-busy fail-safe stays at zero, and it is a COUNTER
/// rather than a `debug_assert!`.
///
/// `lane-d-small-findings-not-fixed.md` item 7 left this as an assert on the
/// argument that the `dispatch` mutex makes it unreachable. That argument is
/// about the LOCK, and it is correct; what makes the counter the right shape
/// is that the site is inside a stop-the-world pause, where a panic leaves the
/// driver on a barrier no helper will ever reach. This test is the reason the
/// zero is citable: it is read on a build that actually ran pauses.
#[test]
fn the_pool_never_dispatches_while_busy() {
    let gc = G1Collector::new(config());
    for _ in 0..4 {
        let (root, _) = build_tree(&gc, 4);
        let mut roots = vec![root];
        gc.young_collection(&mut roots, &NoopMonitors);
    }
    assert_eq!(
        cratonvm_gc::evac_pool::evac_pool_dispatch_while_busy(),
        0,
        "the evacuation pool was dispatched while a job was still published; \
         `EvacPool::scope` holds `dispatch` for its whole extent, so this \
         means that invariant has been broken rather than that the counter \
         needs raising"
    );
}
