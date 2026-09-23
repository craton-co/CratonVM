// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W3-C — `[GC-SUMMARY] young count=` was not a count of young pauses,
//! and every drain pass printed an all-zero region census.
//!
//! # The two defects
//!
//! `drain_kept_self_forwards` — the evacuation-failure recovery pass — calls
//! `record_collection_with_phases(G1CollectionType::YoungOnly, …)`, and
//! `retry_after_evacuation_failure` calls it up to **eight** times inside one
//! young pause that has already recorded itself. So:
//!
//! 1. the pause-percentile population held up to nine entries per stop of the
//!    world, eight of them nested intervals of the first. A 6 ms drain pass and
//!    a 94 ms pause sat in the same ranking, and `total_us` summed overlapping
//!    intervals;
//! 2. `phases.record_region_census(&regions)` is called on four of the FIVE
//!    paths that record a collection, and the drain was the fifth — so a drain
//!    pass printed `free_regions=0 eden_regions=0 surv_regions=0 old_regions=0
//!    hum_regions=0 cset_regions=0` beside `objects_copied=112601`. Observed on
//!    `G1ChurnPauseProbe 32 150 -Xmx128m --nojit`.
//!
//! `young_collection_parallel` carries a PARITY note about the previous two
//! instances of (2) and says of it: *"an all-zero census that reads as a fact
//! about the heap and is a fact about the instrument."* This was the third.
//!
//! # Why this is its own test binary
//!
//! `pause_history` is per collector, but the oracle these tests use to decide
//! whether a drain actually happened — `drain_fixup_totals()` — is
//! PROCESS-GLOBAL. Cargo gives each `tests/*.rs` its own binary, so no other
//! file can perturb it; but `#[test]` functions inside one binary run on
//! PARALLEL THREADS, so the two tests here would reset each other's baseline.
//! (They did: the first version of this file passed under `--test-threads=1`
//! and failed under the default.) Both take `SERIAL`.
//!
//! # What this file does NOT assert
//!
//! Nothing about timing. The fixture reaches the drain in its WEDGED form
//! (`copied=0 freed=0`, the live set genuinely does not fit) — see the long
//! note in `g1_w2b_drain_fixup.rs` about why the PRODUCTIVE form has not been
//! reproduced inside a unit-sized heap. That is fine here: the wedged form
//! still records a pass, which is the thing under test.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::g1::{drain_fixup_totals, reset_drain_fixup_totals};
use cratonvm_gc::{G1Collector, G1CollectorConfig};
use cratonvm_types::{ClassId, Value};
use std::sync::Mutex;

/// Serialises the two tests against the process-global `DRAIN_FIXUP_*`
/// counters they use as their "did a drain happen" oracle.
static SERIAL: Mutex<()> = Mutex::new(());

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> StopTheWorldToken {
    // SAFETY: a single-threaded test; no mutator is running.
    unsafe { StopTheWorldToken::new() }
}

/// A heap small enough that a mostly-live chain exhausts to-space, which is the
/// only way to reach the drain. Same shape and same reasoning as
/// `g1_w2b_drain_fixup.rs::tight_heap`.
fn tight_heap() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 64 * 1024,
        initial_heap_size: 16 * 64 * 1024,
        region_size: 64 * 1024,
        ..Default::default()
    })
}

/// Fill until refusal with a live chain interleaved with `garbage_per_node`
/// dead objects. `1` gives a live fraction high enough that the pause cannot
/// copy the live set into the free pool, which is what makes the drain
/// reachable at all.
fn fill_with_interleaved_chain(
    gc: &G1Collector,
    garbage_per_node: usize,
) -> cratonvm_types::ObjectRef {
    let head = gc.alloc_object(ClassId::new(1), 4);
    let mut cur = head;
    'fill: loop {
        for _ in 0..garbage_per_node {
            if gc.try_alloc_object(ClassId::new(9), 8).is_none() {
                break 'fill;
            }
        }
        let Some(node) = gc.try_alloc_object(ClassId::new(1), 4) else {
            break;
        };
        gc.set_field(cur, 0, Value::Object(Some(node)));
        cur = node;
    }
    gc.set_field(cur, 1, Value::Int(424242));
    head
}

/// Did the drain actually run? Answered by `DRAIN_FIXUP_WALKS`, which is
/// incremented once per pass by `drain_kept_self_forwards` itself.
///
/// **This oracle is deliberately independent of `drain_pass`.** The first
/// version of these tests asked `phases.drain_pass` both to find the passes
/// and to assert they were flagged, so with the fix reverted it found none,
/// took the "no drain occurred" early return, and PASSED — a test that
/// silently stopped covering its subject, which is the exact failure the
/// early return exists to avoid. A regression test for a flag must not use
/// that flag to decide whether the thing it flags happened.
fn drain_ran() -> bool {
    drain_fixup_totals().0 > 0
}

#[test]
fn a_drain_pass_is_recorded_as_a_drain_pass_and_carries_a_real_region_census() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let gc = tight_heap();
    reset_drain_fixup_totals();
    let head = fill_with_interleaved_chain(&gc, 1);
    let mut roots = vec![head];

    // `collect_garbage` is the only production door to
    // `retry_after_evacuation_failure`.
    let _ = gc.collect_garbage(&stw(), &mut roots, &NoMonitors);

    if !drain_ran() {
        // On a host or heap where this fixture does not exhaust to-space the
        // subject of the test never ran. Say so rather than assert something
        // vacuous. Same convention as `g1_w2b_drain_fixup.rs`.
        eprintln!(
            "[w3c] note: this pause did not enter the drain (no self-forwards); \
             the assertions below cover no pass"
        );
        return;
    }

    let hist = gc.pause_history_snapshot();
    let drains: Vec<_> = hist.iter().filter(|r| r.phases.drain_pass).collect();
    assert!(
        !drains.is_empty(),
        "`drain_fixup_totals()` says {} drain pass(es) ran, but NOT ONE record in \
         the pause ring is flagged `drain_pass`. Every one of them is therefore \
         sitting in the young-pause percentiles as if it were a stop-the-world \
         pause of its own.",
        drain_fixup_totals().0,
    );

    for d in &drains {
        // THE PARITY FIX. Before it, every one of these was zero.
        //
        // `total_regions` is the one that cannot legitimately be zero: it is
        // `regions.len()` of a heap that has just been collected. The others
        // (free/eden/surv/old/hum) can each individually be zero on a real
        // heap, which is exactly why an all-zero census was readable as a fact
        // about the heap rather than about the instrument.
        assert!(
            d.phases.total_regions > 0,
            "a drain pass must carry the region census the other four recording \
             paths take: total_regions={} free={} eden={} surv={} old={} hum={}",
            d.phases.total_regions,
            d.phases.free_regions,
            d.phases.eden_regions,
            d.phases.surv_regions,
            d.phases.old_regions,
            d.phases.hum_regions,
        );
        let counted = d.phases.free_regions
            + d.phases.eden_regions
            + d.phases.surv_regions
            + d.phases.old_regions
            + d.phases.hum_regions;
        assert_eq!(
            counted, d.phases.total_regions,
            "the census must partition the table: {counted} counted of {} total",
            d.phases.total_regions
        );
    }
}

#[test]
fn drain_passes_are_reported_but_are_not_counted_as_young_pauses() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let gc = tight_heap();
    reset_drain_fixup_totals();
    let head = fill_with_interleaved_chain(&gc, 1);
    let mut roots = vec![head];
    let _ = gc.collect_garbage(&stw(), &mut roots, &NoMonitors);

    // Independent of `drain_pass` — see `drain_ran`.
    let passes_that_really_ran = drain_fixup_totals().0 as u64;

    let hist = gc.pause_history_snapshot();
    let total_records = hist.len() as u64;
    let drain_records = hist.iter().filter(|r| r.phases.drain_pass).count() as u64;
    assert_eq!(
        drain_records, passes_that_really_ran,
        "`DRAIN_FIXUP_WALKS` counted {passes_that_really_ran} drain pass(es) and the \
         pause ring flags {drain_records}. Every unflagged one is being counted as a \
         young pause."
    );

    let s = gc
        .pause_summary()
        .expect("a collection ran, so there is a summary");

    // Nothing is hidden: every record in the ring is either a pause or a drain
    // pass, and the summary accounts for all of them.
    assert_eq!(
        s.drain_passes, drain_records,
        "the summary must report every drain pass in the ring"
    );
    assert_eq!(
        s.young.count + s.mixed.count + s.drain_passes,
        total_records,
        "young({}) + mixed({}) + drain({}) must account for all {total_records} \
         records; a record that belongs to none of the three has been dropped \
         from the report rather than reclassified",
        s.young.count,
        s.mixed.count,
        s.drain_passes,
    );

    if passes_that_really_ran == 0 {
        eprintln!("[w3c] note: no drain pass occurred; the exclusion is untested on this run");
        return;
    }

    // THE DEFECT. Before the fix, `young.count == total_records` because every
    // drain pass was recorded as `YoungOnly`.
    assert!(
        s.young.count < total_records,
        "young count={} equals the whole ring ({total_records}) although {drain_records} \
         of those records are drain passes recorded INSIDE a young pause. This is the \
         defect: the young percentiles are being computed over a population that \
         contains nested intervals of its own members.",
        s.young.count,
    );
    assert!(
        s.drain_pass_us > 0,
        "a drain pass took measurable time but `drain_pass_us` is zero"
    );
}
