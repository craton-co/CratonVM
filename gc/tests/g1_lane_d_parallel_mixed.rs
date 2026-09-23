// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane D — the `CRATONVM_G1_PARALLEL_MIXED=1` route.
//!
//! `mixed_collection` has refused the parallel evacuator unconditionally since
//! a note citing a rare young-path race that "lives in the shared
//! `parallel_evacuate` closure that `mixed_collection_parallel` also drives".
//! Meanwhile `CRATONVM_G1_PARALLEL_EVAC` was flipped to default-ON, so
//! `young_collection` enters that same closure on every production pause. The
//! consequence is not that mixed is safer — it is that `mixed_collection_parallel`
//! became live code with NO route to it outside unit tests, so no gauntlet arm
//! and no soak could exercise it and the question "is that race still real"
//! could not be asked of the mixed path at all.
//!
//! `CRATONVM_G1_PARALLEL_MIXED=1` is the opt-in route. Default-off keeps
//! production byte-for-byte as it is today. This file is the proof that the
//! route exists and reclaims old-generation regions correctly on the shapes it
//! covers; it is NOT a verdict on the race (see
//! `docs/internal/g1-2026-09-20/lane-d-mixed-is-serial-for-a-race-the-young-path-runs-anyway.md`).
//!
//! ONE `#[test]` per binary, deliberately: the lever is read through
//! `std::env::var` and latched in a `OnceLock`, and `set_var` is only sound
//! while no other thread can be reading the environment — which the libtest
//! harness's default multi-threaded run does not guarantee.

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
        // TENURING THRESHOLD 2, NOT THE DEFAULT 15 (2026-09-20).
        //
        // A MIXED pause is defined by having OLD regions in its collection
        // set, and an Old region only exists because something was promoted.
        // At the default threshold the ageing loop below runs eight young
        // pauses and tenures nothing, so `with_regions_mut` finds no Old
        // region to stamp liveness onto, the old candidate set is empty, and
        // what runs is a young pause wearing a mixed pause's name. Every
        // assertion still passed. The `assert_old_regions_exist` tripwire
        // below is the other half: the day this setup stops producing Old
        // regions the test must FAIL, not quietly go back to testing nothing.
        promotion_age: 2,
        ..Default::default()
    }
}

/// How many Old regions exist right now. The mixed pause's precondition, and
/// the number this file used to assume rather than check.
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

const FANOUT: usize = 3;
const ID_SLOT: usize = FANOUT;
const FIELDS: usize = FANOUT + 1;

fn node(gc: &G1Collector, id: i32) -> ObjectRef {
    let o = gc.alloc_object(ClassId::new(31), FIELDS);
    gc.set_field(o, ID_SLOT, Value::Int(id));
    o
}

#[test]
fn an_opted_in_mixed_pause_runs_the_parallel_evacuator_and_keeps_the_graph() {
    // The lever goes in through `with_process_overrides`, not `set_var`:
    // `CRATONVM_G1_PARALLEL_MIXED` is a DECLARED flag, served from a snapshot
    // latched on first read, so a `set_var` only lands if it wins the race to
    // initialise that snapshot. `types/tests/flag_env_mutation_guard.rs` gates
    // this. The process form reaches the evacuation workers, which this test
    // does not create.
    cratonvm_types::flags::with_process_overrides(
        &[("CRATONVM_G1_PARALLEL_MIXED", Some("1"))],
        an_opted_in_mixed_pause_runs_the_parallel_evacuator_and_keeps_the_graph_inner,
    );
}

fn an_opted_in_mixed_pause_runs_the_parallel_evacuator_and_keeps_the_graph_inner() {
    let gc = G1Collector::new(config());

    // A graph big enough to spread over several regions, aged until a good deal
    // of it has been promoted into Old — which is what gives a mixed pause an
    // old collection set to choose from.
    const HOLDERS: usize = 3000;
    let root = gc.alloc_object(ClassId::new(32), HOLDERS);
    for h in 0..HOLDERS {
        let holder = node(&gc, h as i32);
        gc.set_field(root, h, Value::Object(Some(holder)));
    }
    let mut roots = vec![root];
    for _ in 0..8 {
        for _ in 0..1500 {
            let junk = gc.alloc_object(ClassId::new(33), 2);
            gc.set_field(junk, 0, Value::Int(0));
        }
        gc.young_collection(&mut roots, &NoopMonitors);
    }

    // Give the old regions the liveness data `mixed_collection` selects on.
    // `with_regions_mut` is the sanctioned test hook for exactly this: a real
    // `gc_efficiency` only exists after a completed concurrent mark cycle, and
    // the point of this test is the EVACUATOR, not the mark.
    let olds = old_region_count(&gc);
    assert!(
        olds > 0,
        "the ageing loop produced NO Old region, so there is no old collection          set and `mixed_collection` would run a young pause under a mixed          pause's name. This is the check that keeps this file from passing          vacuously"
    );
    gc.with_regions_mut(|regions| {
        for r in regions.iter_mut() {
            if format!("{:?}", r.region_type()) == "Old" {
                r.live_bytes = r.cursor().max(1) / 8;
                r.gc_efficiency = 0.1;
            }
        }
    });

    let before = cratonvm_gc::g1::g1_young_evac_counts();
    // The per-worker census counts one `pauses` per `parallel_evacuate` call,
    // so this is the ONLY direct evidence that the mixed pause reached the
    // parallel evacuator. `workers_last` cannot say it: it is a process static
    // that the eight young pauses above already moved, so the old
    // `after.2 >= 1` assertion was satisfied before `mixed_collection` was
    // even called.
    let evac_calls_before: u64 = cratonvm_gc::g1::g1_evac_worker_census()
        .first()
        .map(|r| r.pauses)
        .unwrap_or(0);
    let result = gc.mixed_collection(&mut roots, &NoopMonitors);
    let after = cratonvm_gc::g1::g1_young_evac_counts();
    let evac_calls_after: u64 = cratonvm_gc::g1::g1_evac_worker_census()
        .first()
        .map(|r| r.pauses)
        .unwrap_or(0);
    assert_eq!(
        before.0, after.0,
        "a mixed pause must not be recorded as a parallel YOUNG cycle"
    );
    assert!(
        evac_calls_after > evac_calls_before,
        "`CRATONVM_G1_PARALLEL_MIXED=1` is armed and {olds} Old regions were          selectable, but the mixed pause did not enter `parallel_evacuate`          ({evac_calls_before} -> {evac_calls_after}). It took the serial body,          which is the arm this file exists NOT to test"
    );

    // The whole graph must still be readable and intact. A mixed pause moves
    // Old objects, so a missed remembered-set source or a lost forward here is
    // a dangling reference into a region the pause freed — the failure this
    // read is looking for.
    let root = roots[0];
    for h in 0..HOLDERS {
        let Value::Object(Some(holder)) = gc.get_field(root, h) else {
            panic!("holder {h} was lost by the parallel mixed pause");
        };
        assert_eq!(
            gc.get_field(holder, ID_SLOT).as_int(),
            Some(h as i32),
            "holder {h} did not survive the parallel mixed pause intact"
        );
    }
    // Sanity: the pause did SOMETHING. A mixed collection that copies nothing
    // and frees nothing would make every assertion above vacuous.
    assert!(
        result.stats.objects_copied > 0 || result.stats.bytes_freed > 0,
        "the parallel mixed pause neither copied nor freed anything"
    );

    // ...and the collector still works afterwards.
    let fresh = node(&gc, 999);
    let mut roots2 = vec![fresh];
    gc.young_collection(&mut roots2, &NoopMonitors);
    assert_eq!(gc.get_field(roots2[0], ID_SLOT).as_int(), Some(999));
}
