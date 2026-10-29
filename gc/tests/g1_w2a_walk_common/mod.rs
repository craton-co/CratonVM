// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — the ONE workload that every arm of the unified
//! remembered-set source walk has to produce the same answer for.
//!
//! # Why the workload is shared and the binaries are not
//!
//! `G1Collector::scan_source_region_for_cset_refs` (serial) and
//! `SharedEvac::seed_source_region` (parallel) were two copies of one walk
//! until wave 2; defect G1-9 and five further 2026-09-20 findings were
//! divergences between them, and three of those changed what the pause FOUND
//! rather than how fast it found it. They now share one body
//! (`walk_source_region_for_cset_refs`), and this module is the evidence that
//! the remaining per-arm glue does not reintroduce the difference.
//!
//! Which arm runs is `CRATONVM_G1_PARALLEL_EVAC`, and the card cursor is
//! `CRATONVM_G1_CARD_CURSOR`. Both are latched in a `OnceLock` on first read,
//! and `cratonvm_types::flags()` reads the environment once per process — so
//! one arm is one process. `cargo test` gives each `tests/*.rs` file its own
//! binary, and the three files beside this module are the three arms:
//!
//! * `g1_w2a_source_walk_defaults` — the DEFAULT configuration (parallel
//!   evacuator, cursor off, cleaning off, prune off). This is the one that says
//!   the unification did not change shipping behaviour;
//! * `g1_w2a_source_walk_parallel` — the parallel arm with every wave-2 lever
//!   armed (adaptive cursor, card cleaning, scan-time prune);
//! * `g1_w2a_source_walk_serial` — the SERIAL arm with the same levers.
//!
//! Every one of them calls [`run`] and asserts the identical expectation, which
//! is fully determined by the construction below. "Both arms find the same
//! graph" is exactly the property the duplication kept breaking.
//!
//! # What the graph is built to reach
//!
//! Every live object except the root is reachable ONLY through a remembered-set
//! edge out of a tenured holder, so anything the source walk fails to scan is
//! not merely slow to find — it is lost, and reading it back faults or returns
//! the wrong id. Three holder shapes, because the walk dispatches on three:
//!
//! * a FLAT object with several reference slots (the `num_slots` arm, and the
//!   one whose 16-byte legacy stride leaves a region twice as fast as an array
//!   element — the arm a clamp fires on first);
//! * a reference ARRAY (the `array_length` arm);
//! * a flat holder whose referent is itself a holder, so the seed's own gray
//!   pushes have to feed a real transitive closure afterwards.
//!
//! # WHAT THIS WORKLOAD DOES NOT PROMISE: A JUMP COUNT
//!
//! Read this before asserting a lower bound on any engagement counter off
//! [`run`]. It cost a day of flake hunting on 2026-09-21 and it is not
//! obvious from the code.
//!
//! Every assertion INSIDE `run` is about the graph, and the graph is fully
//! determined by the construction — which is why the three `g1_w2a_*` arms can
//! all assert `run() == expected()` and never flake. The engagement counters a
//! CALLER reads afterwards are a different kind of quantity. They are
//! properties of the heap's LAYOUT, and the layout is not determined here.
//!
//! Concretely, for `CRATONVM_G1_BLOCK_OFFSETS`: a jump fires when a source
//! region's walk stands at a boundary with a clean card run ahead of it and
//! another dirty card beyond that. So the jump count is roughly
//!
//! > Σ over walked Old source regions of (separated dirty-card clusters − 1)
//!
//! and a region with ONE cluster contributes an end-of-region break instead
//! (`block_offset_no_jump_census`'s first element), while a region with NONE
//! is screened out whole by F-05 and never walked. Which objects share a
//! region, and therefore how the clusters fall, is decided by the PARALLEL
//! EVACUATOR: each worker promotes into its own TLAB, so which worker copies
//! a given holder decides which Old region it lands in, and that is work
//! stealing — i.e. OS scheduling.
//!
//! Measured on an 8-core Linux host, 30-50 runs per cell, under a synthetic
//! load average of ~49 (this fixture, `CRATONVM_G1_BLOCK_OFFSETS=1`,
//! `CRATONVM_G1_CARD_CLEAN=1`):
//!
//! ```text
//!   CRATONVM_G1_WORKERS=1      jumps=6   30/30       deterministic
//!   CRATONVM_G1_PARALLEL_EVAC=0 jumps=6  50/50       deterministic
//!   CRATONVM_G1_WORKERS=2      jumps ∈ {2, 6, 8}     14 / 4 / 12
//!   CRATONVM_G1_WORKERS=4      jumps ∈ {2, 6, 8}     14 / 1 / 15
//!   default (8 workers)        jumps ∈ {2, 8}        12 / 38
//! ```
//!
//! On an IDLE host the default arm reads 8 in 60 runs out of 60, which is why
//! this was invisible: the distribution only opens up under contention. The
//! low mode reaches zero — `HOLDERS = 12000` produces `jumps=0 tails=44` even
//! idle, on exactly the same code — and a zero fails
//! `g1_w5b_producer_rule_is_enforced`'s `jumps > 0` and
//! `g1_w5b_sampled_audit`'s `jumps >= PERIOD`.
//!
//! **So: a binary that asserts a lower bound on jumps must pin the layout**,
//! with `CRATONVM_G1_WORKERS=1` (keeps the parallel evacuator's code path and
//! removes only the interleaving) or `CRATONVM_G1_PARALLEL_EVAC=0` (the serial
//! arm, which `g1_w4a_block_offsets_serial` already pins and which is why that
//! one binary never flaked). The four that do are marked.
//!
//! Widening the margin instead was tried and does not work: the count is
//! bounded by the number of walked Old source regions, not by how much is
//! written. Spacing the later-pause stores from every 3rd holder to every
//! 17th moved it 8 → 30 and then back to 16 at every 97th; adding sparse
//! dirty anchors in array space moved it not at all (8 → 8 → 8); raising
//! `HOLDERS` from 1200 to 12000 moved it 8 → 0.
//!
//! The full record, with every arm and every refuted hypothesis, is
//! `g1-block-offset-jump-count-is-a-layout-coin-flip-FIXED-20260921.md`.
//!
//! What is NOT layout-dependent, and is therefore safe to assert without a
//! pin: `entries_refused`, `unvouched`, `violations` (all 0 in every one of
//! the ~350 runs above) and the vouch count (2775-2841). Those are the
//! producer rule itself, which is what these binaries are actually about.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

pub struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        // TENURE FAST, AND THIS IS LOAD-BEARING.
        //
        // The default is 15, which means a fixture that runs six young pauses
        // never promotes anything: every holder is still in a Survivor region,
        // every Survivor region is in the next pause's collection set, and the
        // holders are therefore EVACUATED — so the closure walks their slots
        // and the remembered-set source walk is never asked anything. A test
        // written that way passes with the source walk deleted, which is the
        // "the lever never engaged" trap in test form, and it is why this line
        // is here rather than a longer ageing loop.
        //
        // `ages_promote_before_the_fixture_stops_ageing` below is the tripwire
        // that keeps this true if a default moves.
        promotion_age: 2,
        ..Default::default()
    }
}

/// Flat holders: slot 0 is the young child, slot 1 the chained holder, slot 2
/// the array, slot 3 the id.
const CHILD: usize = 0;
const CHAIN: usize = 1;
const ARRAY: usize = 2;
const ID: usize = 3;
const FIELDS: usize = 4;

/// Elements per reference array holder. Larger than one card's worth of
/// pointers (512 / 8 = 64) so an array holder straddles cards and the
/// per-object screen's "any card overlapping the object" question is a real
/// one rather than a single-card lookup.
const ARRAY_LEN: usize = 96;

/// How many flat holders. Enough to spread the source set over many regions,
/// so a partitioned or screened walk that drops a region is visible.
const HOLDERS: usize = 1200;

fn node(gc: &G1Collector, id: i32) -> ObjectRef {
    let o = gc.alloc_object(ClassId::new(41), FIELDS);
    gc.set_field(o, ID, Value::Int(id));
    o
}

/// Build the graph, age it into Old, hang fresh young objects off every holder,
/// run a young pause, and read the whole thing back.
///
/// Panics with a message naming the lost edge if any arm of the walk missed
/// one. Returns `(holders_checked, array_elements_checked)` so the caller can
/// assert the workload actually ran rather than short-circuiting.
pub fn run() -> (usize, usize) {
    let gc = G1Collector::new(config());

    let root = gc.alloc_object(ClassId::new(42), HOLDERS);
    let mut holders = Vec::with_capacity(HOLDERS);
    for h in 0..HOLDERS {
        let holder = node(&gc, h as i32);
        // Every holder owns a reference array, so the array arm of the walk is
        // exercised by every source region rather than by a corner of the heap.
        let arr = gc.alloc_array(ClassId::new(43), ArrayElementType::Reference, ARRAY_LEN);
        gc.set_field(holder, ARRAY, Value::Object(Some(arr)));
        gc.set_field(root, h, Value::Object(Some(holder)));
        holders.push(holder);
    }
    let mut roots = vec![root];

    // Age until the holders and their arrays tenure into Old regions. From here
    // on a store into a holder is a cross-region store, which is what puts the
    // holder's region into the young CSet's remembered set and dirties its
    // card.
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

    // THE TRIPWIRE. Everything below is about the remembered-set source walk,
    // and the source walk is only reached for a holder the pause does NOT
    // evacuate — i.e. one in an Old region. A holder still sitting in Survivor
    // is in the next pause's collection set, gets evacuated, and has its slots
    // walked by the closure instead, so a fixture whose holders never tenure
    // asserts nothing about this lane and passes with the whole walk deleted.
    //
    // That is not hypothetical: it is what this fixture did at the default
    // `promotion_age` of 15, and it is why `config()` lowers it. This assert is
    // what keeps a later change to that default from quietly making the file
    // decorative again.
    assert!(
        gc.count_regions(cratonvm_gc::region::RegionType::Old) > 0,
        "no holder tenured, so nothing below reaches the remembered-set source \
         walk -- check `config()`'s promotion_age against the collector default"
    );

    // Now hang FRESH young objects off every holder. The only route to any of
    // them is the remembered-set edge the write barrier just recorded.
    //
    //   holder.CHILD -> a fresh young node
    //   holder.CHAIN -> a fresh young node whose own CHILD is another fresh one
    //                   (so the seed's gray pushes must feed a closure)
    //   holder.ARRAY[i] -> a fresh young node, for a spread of i
    for (h, &holder) in holders.iter().enumerate() {
        let child = node(&gc, 1_000_000 + h as i32);
        gc.set_field(holder, CHILD, Value::Object(Some(child)));

        let chain = node(&gc, 2_000_000 + h as i32);
        let deep = node(&gc, 3_000_000 + h as i32);
        gc.set_field(chain, CHILD, Value::Object(Some(deep)));
        gc.set_field(holder, CHAIN, Value::Object(Some(chain)));

        let Value::Object(Some(arr)) = gc.get_field(holder, ARRAY) else {
            panic!("holder {h} lost its array while ageing");
        };
        // A spread rather than every element: a fully-populated array would
        // dirty every card it covers and hide the sparse case the cursor is
        // for, while an empty one would never reach the element loop.
        let mut i = h % 7;
        while i < ARRAY_LEN {
            let e = node(&gc, 4_000_000 + (h * ARRAY_LEN + i) as i32);
            gc.set_array_element(arr, i, Value::Object(Some(e)))
                .expect("array store");
            i += 5;
        }
    }
    // Garbage, so the pause has a real Eden to reclaim and the CSet is not
    // trivially empty.
    for _ in 0..4000 {
        let junk = gc.alloc_object(ClassId::new(44), 2);
        gc.set_field(junk, 0, Value::Int(0));
    }

    let mut roots = vec![root];
    gc.young_collection(&mut roots, &NoopMonitors);

    // Read everything back. A source region the walk skipped, or an object its
    // per-object card screen stepped over, loses the EDGE rather than the
    // holder — so the holder survives with a slot pointing into a region
    // Phase 5 freed, which is what these reads catch.
    let root = roots[0];
    let mut elements = 0usize;
    for h in 0..HOLDERS {
        let Value::Object(Some(holder)) = gc.get_field(root, h) else {
            panic!("holder {h} was lost by the pause");
        };
        assert_eq!(
            gc.get_field(holder, ID).as_int(),
            Some(h as i32),
            "holder {h} did not survive intact"
        );

        let Value::Object(Some(child)) = gc.get_field(holder, CHILD) else {
            panic!("holder {h} lost the young child reachable only through its rset edge");
        };
        assert_eq!(
            gc.get_field(child, ID).as_int(),
            Some(1_000_000 + h as i32),
            "holder {h}'s young child was not evacuated intact"
        );

        let Value::Object(Some(chain)) = gc.get_field(holder, CHAIN) else {
            panic!("holder {h} lost its chained young holder");
        };
        assert_eq!(
            gc.get_field(chain, ID).as_int(),
            Some(2_000_000 + h as i32),
            "holder {h}'s chained holder was not evacuated intact"
        );
        let Value::Object(Some(deep)) = gc.get_field(chain, CHILD) else {
            panic!(
                "holder {h}'s chained holder lost ITS child — the seed found the \
                 source's referent but its gray push never fed the closure"
            );
        };
        assert_eq!(
            gc.get_field(deep, ID).as_int(),
            Some(3_000_000 + h as i32),
            "holder {h}'s second-level child was not evacuated intact"
        );

        let Value::Object(Some(arr)) = gc.get_field(holder, ARRAY) else {
            panic!("holder {h} lost its array");
        };
        let mut i = h % 7;
        while i < ARRAY_LEN {
            let Ok(Value::Object(Some(e))) = gc.get_array_element(arr, i) else {
                panic!("holder {h} array element {i} was lost by the source walk");
            };
            assert_eq!(
                gc.get_field(e, ID).as_int(),
                Some(4_000_000 + (h * ARRAY_LEN + i) as i32),
                "holder {h} array element {i} was not evacuated intact"
            );
            elements += 1;
            i += 5;
        }
    }

    // Drive several more pauses over the same graph. The first pause left the
    // holders' cards in whatever state cleaning chose; a walk whose screen is
    // wrong about a CLEANED card loses an edge on the SECOND pause, not the
    // first, which is exactly the shape of the clamp defect this wave closed.
    let mut roots = vec![root];
    for pause in 0..5 {
        let root = roots[0];
        for h in (0..HOLDERS).step_by(3) {
            let Value::Object(Some(holder)) = gc.get_field(root, h) else {
                panic!("holder {h} was lost before pause {pause}");
            };
            let fresh = node(&gc, 5_000_000 + (pause * HOLDERS + h) as i32);
            gc.set_field(holder, CHILD, Value::Object(Some(fresh)));
        }
        for _ in 0..2000 {
            let junk = gc.alloc_object(ClassId::new(45), 2);
            gc.set_field(junk, 0, Value::Int(pause as i64 as i32));
        }
        gc.young_collection(&mut roots, &NoopMonitors);

        let root = roots[0];
        for h in (0..HOLDERS).step_by(3) {
            let Value::Object(Some(holder)) = gc.get_field(root, h) else {
                panic!("holder {h} was lost by pause {pause}");
            };
            let Value::Object(Some(fresh)) = gc.get_field(holder, CHILD) else {
                panic!("holder {h} lost its child on pause {pause}");
            };
            assert_eq!(
                gc.get_field(fresh, ID).as_int(),
                Some(5_000_000 + (pause * HOLDERS + h) as i32),
                "holder {h}'s child was not evacuated intact on pause {pause}"
            );
            // The array's elements have not been rewritten since the first
            // pause, so they are the part that a wrongly-cleaned card drops.
            let Value::Object(Some(arr)) = gc.get_field(holder, ARRAY) else {
                panic!("holder {h} lost its array on pause {pause}");
            };
            let mut i = h % 7;
            while i < ARRAY_LEN {
                let Ok(Value::Object(Some(e))) = gc.get_array_element(arr, i) else {
                    panic!("holder {h} array element {i} was lost on pause {pause}");
                };
                assert_eq!(
                    gc.get_field(e, ID).as_int(),
                    Some(4_000_000 + (h * ARRAY_LEN + i) as i32),
                    "holder {h} array element {i} was corrupted by pause {pause}"
                );
                i += 5;
            }
        }
    }

    (HOLDERS, elements)
}

/// The expectation every arm asserts. Derived from the construction, so it is a
/// statement about the workload and not about whichever arm happened to run
/// first.
pub fn expected() -> (usize, usize) {
    let elements: usize = (0..HOLDERS)
        .map(|h| {
            let mut n = 0;
            let mut i = h % 7;
            while i < ARRAY_LEN {
                n += 1;
                i += 5;
            }
            n
        })
        .sum();
    (HOLDERS, elements)
}
