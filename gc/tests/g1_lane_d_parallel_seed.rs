// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane D — the `CRATONVM_G1_PARALLEL_SEED=1` arm.
//!
//! Phase 2 of a G1 pause walks every remembered-set source region wholesale
//! (`g1-maturation.md` §1.3), and until 2026-09-20 it did so on the driver
//! thread alone while the whole persistent worker pool sat parked. This file
//! covers the arm that spreads it across the workers.
//!
//! # Why this is a separate test binary
//!
//! The lever is latched in a `OnceLock` on first read, and
//! `cratonvm_types::flags()` reads the environment once per process, so the two
//! arms are two processes. `cargo test` gives each `tests/*.rs` file its own
//! binary, which is the mechanism this file uses: it sets the variable before
//! any collector exists, and every test in it therefore runs the parallel seed.
//! The default (driver-only) arm is what every other G1 test in the crate
//! already exercises.
//!
//! # What has to be true
//!
//! The seed walk is where the collector CONSUMES the remembered set, so the
//! failure mode is not a crash — it is a CSet-bound reference that no walker
//! rewrote, whose holder then survives into a region pointing at bytes Phase 5
//! freed. The graphs below are built so that the only route from the roots to
//! most of the live set is through a remembered-set edge out of a promoted Old
//! region, and every node is read back through the collector's own accessors
//! afterwards. A source region dropped by the partition, or a walk clamped to
//! the wrong cursor, shows up as a lost or mis-identified node.
//!
//! This file does NOT claim the arm is ready to be the default — see
//! `docs/internal/g1-2026-09-20/lane-d-parallel-seed-phase.md` for what would
//! have to be measured for that. It claims the arm is correct on the shapes it
//! covers, which is the prerequisite for anyone bothering to measure it.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// The lever, delivered through `with_process_overrides` rather than
/// `std::env::set_var`.
///
/// `CRATONVM_G1_PARALLEL_SEED` became a DECLARED flag when it was added to
/// `types/src/flag_groups.rs`, and `flags()` serves a declared name from a
/// snapshot latched on first read — so a `set_var` takes effect only if it
/// wins the race to initialise that snapshot, which is a fact about what else
/// the binary touched first rather than about this function.
/// `types/tests/flag_env_mutation_guard.rs` is the gate that says so.
///
/// The PROCESS form, not the thread-local one: the seed phase is the thing
/// under test and it runs on worker threads this test did not create.
///
/// One test per binary remains the rule, for the separate reason that the
/// `OnceLock` gate behind the flag latches once per process.
const ARM_PARALLEL_SEED: &[(&str, Option<&str>)] = &[("CRATONVM_G1_PARALLEL_SEED", Some("1"))];

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        // TENURING THRESHOLD 2, NOT THE DEFAULT 15 (2026-09-20).
        //
        // This file is named for the remembered-set SEED phase, and a
        // remembered-set source is by definition a region OUTSIDE the
        // collection set — which for a young pause means an Old region, which
        // means something has to have been promoted. At the default
        // `promotion_age: 15` the fixture below ages its holders six times and
        // nothing tenures: no region becomes Old, no rset entry is created,
        // the holders are evacuated as ordinary young survivors and the
        // closure walks their slots directly. Every assertion still passed,
        // because the graph IS intact — it was simply never reached through the
        // phase under test.
        //
        // That is the worst failure mode a test can have, and it is not
        // hypothetical: lane W2-A traced ZERO source-walk invocations through
        // this file. Two things fix it and both are needed — this threshold, so
        // the setup can produce Old regions at all, and the
        // `assert_the_seed_phase_actually_ran` tripwire below, so the day it
        // stops producing them the test FAILS instead of quietly going back to
        // testing nothing.
        promotion_age: 2,
        ..Default::default()
    }
}

/// The tripwire this file was missing: assert that the phase under test RAN.
///
/// `seed_regions` is the per-worker census's count of remembered-set source
/// regions walked (`g1::EvacWorkerCensus`). Zero means Phase 2 had no source
/// to walk, which means this binary is exercising the ordinary transitive
/// closure and calling it a seed-phase test.
///
/// Returns the total so a caller can also check the PARTITION, which is the
/// half the lever is actually about.
fn assert_the_seed_phase_actually_ran(what: &str) -> u64 {
    let rows = cratonvm_gc::g1::g1_evac_worker_census();
    let seeded: u64 = rows.iter().map(|r| r.seed_regions).sum();
    assert!(
        seeded > 0,
        "{what}: no evacuation worker walked a single remembered-set source          region. The fixture did not produce an Old region holding an edge          into the collection set, so the seed phase this file is named for          never ran and every assertion below is about the ordinary closure.          Per-worker rows: {:?}",
        rows.iter().map(|r| r.seed_regions).collect::<Vec<_>>(),
    );
    seeded
}

const FANOUT: usize = 3;
const ID_SLOT: usize = FANOUT;
const FIELDS: usize = FANOUT + 1;

fn node(gc: &G1Collector, id: i32) -> ObjectRef {
    let o = gc.alloc_object(ClassId::new(21), FIELDS);
    gc.set_field(o, ID_SLOT, Value::Int(id));
    o
}

/// The shape the seed phase exists for: a set of HOLDERS that have been aged
/// into Old regions, each pointing at a FRESH young object allocated after the
/// last pause. The young objects are reachable ONLY through those holders, so
/// the pause finds them by walking remembered-set sources — which, with the
/// lever armed, is the partitioned walk.
#[test]
fn young_objects_reachable_only_through_promoted_holders_all_survive() {
    cratonvm_types::flags::with_process_overrides(ARM_PARALLEL_SEED, || {
        young_objects_reachable_only_through_promoted_holders_all_survive_inner();
    });
}

fn young_objects_reachable_only_through_promoted_holders_all_survive_inner() {
    let gc = G1Collector::new(config());

    // Enough holders to spread over several regions, so the source set the
    // partition divides is genuinely larger than one entry.
    const HOLDERS: usize = 4000;
    let root = gc.alloc_object(ClassId::new(22), HOLDERS);
    let mut holders = Vec::with_capacity(HOLDERS);
    for h in 0..HOLDERS {
        let holder = node(&gc, h as i32);
        gc.set_field(root, h, Value::Object(Some(holder)));
        holders.push(holder);
    }
    let mut roots = vec![root];

    // Age the holders until they tenure into Old regions.
    for _ in 0..6 {
        gc.young_collection(&mut roots, &NoopMonitors);
    }
    let root = roots[0];
    for (h, holder) in holders.iter_mut().enumerate() {
        let Value::Object(Some(cur)) = gc.get_field(root, h) else {
            panic!("holder {h} was lost while ageing");
        };
        *holder = cur;
    }

    // Now hang a FRESH young object off each holder. The only path to these is
    // the remembered-set edge the write barrier just recorded.
    for (h, &holder) in holders.iter().enumerate() {
        let fresh = node(&gc, 1_000_000 + h as i32);
        gc.set_field(holder, 0, Value::Object(Some(fresh)));
    }
    // ...plus a pile of garbage so the pause has a real Eden to reclaim.
    for _ in 0..2000 {
        let junk = gc.alloc_object(ClassId::new(23), 2);
        gc.set_field(junk, 0, Value::Int(0));
    }

    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);

    // THE PHASE UNDER TEST RAN. Checked before the graph assertions, because
    // an intact graph proves nothing about a walk that never happened.
    let seeded = assert_the_seed_phase_actually_ran("promoted-holder fixture");
    // ...and it was PARTITIONED, which is the half the lever is about. The
    // partition only engages with more than one source region and more than
    // one worker, so this is conditional on the host rather than asserted
    // outright: a one-core machine legitimately walks every source on the
    // driver. What must never happen is the driver walking sources while
    // helpers that were dispatched walked none.
    let rows = cratonvm_gc::g1::g1_evac_worker_census();
    if seeded > 1 && rows.len() > 1 {
        let helper_seeded: u64 = rows.iter().skip(1).map(|r| r.seed_regions).sum();
        assert!(
            helper_seeded > 0,
            "CRATONVM_G1_PARALLEL_SEED=1 is armed and {seeded} source regions              were walked across {} workers, but every one of them was walked              by the driver. The lever is set and the partition did not engage.              Per-worker rows: {:?}",
            rows.len(),
            rows.iter().map(|r| r.seed_regions).collect::<Vec<_>>(),
        );
    }

    // Every holder must still name its fresh child, and that child must carry
    // the id it was given. A source region the partition skipped loses the
    // EDGE, not the holder — so the holder survives with a slot pointing into
    // a freed region, which is what this read would fault or mis-read on.
    let root = roots[0];
    for h in 0..HOLDERS {
        let Value::Object(Some(holder)) = gc.get_field(root, h) else {
            panic!("holder {h} was lost by the pause");
        };
        assert_eq!(
            gc.get_field(holder, ID_SLOT).as_int(),
            Some(h as i32),
            "holder {h} did not survive intact"
        );
        let Value::Object(Some(fresh)) = gc.get_field(holder, 0) else {
            panic!(
                "holder {h} lost the young child that was reachable only \
                 through its remembered-set edge"
            );
        };
        assert_eq!(
            gc.get_field(fresh, ID_SLOT).as_int(),
            Some(1_000_000 + h as i32),
            "holder {h}'s young child was not evacuated intact"
        );
    }

    repeated_pauses_with_a_partitioned_seed_keep_a_deep_graph_intact();
}

/// The same partition, driven repeatedly, with a graph deep enough that the
/// seed's own gray pushes feed a real transitive closure afterwards. This is
/// the arm where a seeding worker's TLAB has to carry into Phase 3 rather than
/// being retired between the phases.
///
/// A plain function, called from the single `#[test]` above rather than being
/// one of its own: `std::env::set_var` is only sound while no other thread can
/// be reading the environment, and the libtest harness runs `#[test]`s on
/// several threads at once. One test per binary is what keeps the lever's
/// arming window single-threaded.
fn repeated_pauses_with_a_partitioned_seed_keep_a_deep_graph_intact() {
    let gc = G1Collector::new(config());
    let seeded_before: u64 = cratonvm_gc::g1::g1_evac_worker_census()
        .iter()
        .map(|r| r.seed_regions)
        .sum();

    const CHAINS: usize = 1500;
    const DEPTH: u32 = 5;
    let root = gc.alloc_object(ClassId::new(24), CHAINS);
    for c in 0..CHAINS {
        let mut head = node(&gc, (c * 100) as i32);
        gc.set_field(root, c, Value::Object(Some(head)));
        for d in 1..DEPTH {
            let next = node(&gc, (c * 100) as i32 + d as i32);
            gc.set_field(head, 0, Value::Object(Some(next)));
            head = next;
        }
    }
    let mut roots = vec![root];

    for pause in 0..8 {
        for _ in 0..1000 {
            let junk = gc.alloc_object(ClassId::new(25), 2);
            gc.set_field(junk, 0, Value::Int(pause));
        }
        gc.young_collection(&mut roots, &NoopMonitors);

        let root = roots[0];
        for c in 0..CHAINS {
            let Value::Object(Some(mut cur)) = gc.get_field(root, c) else {
                panic!("chain {c} was lost on pause {pause}");
            };
            let mut ids = Vec::new();
            for _ in 0..DEPTH {
                ids.push(gc.get_field(cur, ID_SLOT).as_int().expect("id"));
                match gc.get_field(cur, 0) {
                    Value::Object(Some(next)) => cur = next,
                    _ => break,
                }
            }
            let want: Vec<i32> = (0..DEPTH as i32).map(|d| (c * 100) as i32 + d).collect();
            assert_eq!(
                ids, want,
                "chain {c} is corrupt after pause {pause} — the partitioned \
                 seed lost or duplicated a link"
            );
        }
    }

    // EIGHT PAUSES OVER A PROMOTED GRAPH MUST HAVE WALKED SOURCES. Without
    // this the arm degenerates to "a deep graph survives eight young pauses",
    // which is true of the serial evacuator, of the parallel one, and of a
    // build with the seed partition deleted.
    let seeded_after: u64 = cratonvm_gc::g1::g1_evac_worker_census()
        .iter()
        .map(|r| r.seed_regions)
        .sum();
    assert!(
        seeded_after > seeded_before,
        "eight pauses over {CHAINS} promoted chains walked no remembered-set          source at all ({seeded_before} -> {seeded_after}). Nothing tenured,          so this arm exercised the ordinary closure under the seed phase's          name"
    );
}
