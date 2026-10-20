// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W2-C — the mark scan must follow the three class-loader side edges
//! whether it asks the registries per object or reads a per-batch snapshot.
//!
//! # What this guards
//!
//! `G1Collector::scan_object_refs` ends every object scan by following three
//! edges the object header cannot carry:
//!
//! ```text
//!   live instance  --loader_pin (by class id)--> defining ClassLoader
//!   live loader    --mirror_pin (by address)--> its class mirrors
//!   live loader    --metadata_pin (by address)--> its metadata oops
//! ```
//!
//! It did so with three process-global registry lookups PER SCANNED OBJECT. On
//! a run with no user-defined `ClassLoader` each is one relaxed atomic load
//! (the `NON_EMPTY` latch) and the cost is nil — but the latch flips
//! permanently the moment one custom loader exists, and every one of the three
//! then becomes an `RwLock` acquisition, two of them with a heap-allocating
//! `Vec` clone, on the hottest loop the collector owns, on N concurrent marking
//! threads, for the whole duration of a cycle. Every Spring / Tomcat /
//! Hibernate process has custom loaders.
//!
//! `CRATONVM_G1_MARK_SIDE_TABLES=1` hoists the lookups out of the per-object
//! loop into a per-batch snapshot (`MarkSideTables`). The snapshot carries the
//! `rset_cache_epoch` it was taken at and is rebuilt whenever a pause moves it,
//! because the VALUES are heap addresses and a pause rewrites both them and the
//! registries.
//!
//! # What the test asserts
//!
//! The correctness regression the change has to survive: an object reachable
//! ONLY through a side edge must still be marked. Stated for all three edges at
//! once, as a chain, so it also covers "a side-table target is itself scanned
//! for further side edges" — which is the property a naive snapshot that only
//! consulted the tables for ROOTS would break.
//!
//! Both arms run the same graph and must reach the same verdict. A control
//! object with no edge to it at all proves the assertion can fail.

use std::sync::Arc;

use cratonvm_gc::collector::{GarbageCollector, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::flags;
use cratonvm_types::{ClassId, ObjectRef};

/// Test-only STW witness (I-17): the cycle is driven by hand with no mutators
/// running, so the invariant the token stands for holds trivially.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded test driver; no mutator is executing.
    unsafe { StopTheWorldToken::new() }
}

/// A VM identity for the registry rows. Every registry is process-global and
/// keyed by owning VM, so a distinct value keeps these rows from being
/// mistaken for another test binary's.
const VM_ID: usize = 0x5732_4332;

// NOTE ON ISOLATION: all three registries are PROCESS-global and the test
// harness runs these tests concurrently in one process, so every test here
// takes its own `class_id` space. `loader_pin` is keyed by class id, and two
// tests sharing one id would overwrite each other's row — which is a real
// property of the registry, not a harness artefact, and the reason the id is a
// parameter rather than a constant.

fn collector() -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: 32 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }))
}

/// The graph, and the verdicts the cycle must reach about it.
struct SideEdgeGraph {
    /// Reachable from the root by an ordinary reference field.
    holder: ObjectRef,
    /// Reachable ONLY through `loader_pin`, keyed by `holder`'s class id.
    loader: ObjectRef,
    /// Reachable ONLY through `mirror_pin`, keyed by `loader`'s address.
    mirror: ObjectRef,
    /// Reachable ONLY through `metadata_pin`, keyed by `loader`'s address.
    metadata: ObjectRef,
    /// Reachable by nothing. If this comes back live the test is not measuring
    /// what it claims to.
    orphan: ObjectRef,
}

fn build(gc: &G1Collector, class_base: u32) -> (ObjectRef, SideEdgeGraph) {
    let pinned_class = class_base + 1;
    let root = gc.alloc_object(ClassId::new(class_base), 1);
    let holder = gc.alloc_object(ClassId::new(pinned_class), 0);
    gc.set_field(root, 0, cratonvm_types::Value::Object(Some(holder)));

    let loader = gc.alloc_object(ClassId::new(class_base + 2), 0);
    let mirror = gc.alloc_object(ClassId::new(class_base + 3), 0);
    let metadata = gc.alloc_object(ClassId::new(class_base + 4), 0);
    let orphan = gc.alloc_object(ClassId::new(class_base + 5), 0);

    cratonvm_types::loader_pin::set_loader_pin(VM_ID, pinned_class, loader.as_ptr() as usize);
    cratonvm_types::mirror_pin::add_mirror_pin(
        VM_ID,
        loader.as_ptr() as usize,
        mirror.as_ptr() as usize,
    );
    cratonvm_types::metadata_pin::add_metadata_pin(
        VM_ID,
        loader.as_ptr() as usize,
        metadata.as_ptr() as usize,
    );

    (
        root,
        SideEdgeGraph {
            holder,
            loader,
            mirror,
            metadata,
            orphan,
        },
    )
}

/// Everything up to — but not including — `cleanup`, which is where
/// `is_live_after_mark` is the authoritative verdict (the VM's reference
/// processing consults it at exactly this point; see
/// `VmHeap::g1_final_remark_and_cleanup`).
fn mark_to_fixed_point(gc: &G1Collector, root: ObjectRef) {
    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[root]);
    while !gc.concurrent_mark_step(usize::MAX) {}
}

fn assert_edges_followed(gc: &G1Collector, g: &SideEdgeGraph, arm: &str) {
    assert!(
        gc.is_live_after_mark(g.holder.as_ptr() as usize),
        "[{arm}] the ordinary reference edge"
    );
    assert!(
        gc.is_live_after_mark(g.loader.as_ptr() as usize),
        "[{arm}] instance -> defining loader (loader_pin) — a loader whose only \
         reference is one of its own instances"
    );
    assert!(
        gc.is_live_after_mark(g.mirror.as_ptr() as usize),
        "[{arm}] loader -> class mirror (mirror_pin) — reached only by SCANNING \
         the loader, which was itself reached only by a side edge"
    );
    assert!(
        gc.is_live_after_mark(g.metadata.as_ptr() as usize),
        "[{arm}] loader -> metadata oop (metadata_pin), same chain"
    );
    assert!(
        !gc.is_live_after_mark(g.orphan.as_ptr() as usize),
        "[{arm}] control: an object with no edge to it at all must come back \
         DEAD, or the three assertions above prove nothing"
    );
}

/// Today's per-object lookups. Pinned so the snapshot arm has a stated
/// baseline rather than an assumed one.
#[test]
fn the_per_object_registry_lookups_follow_every_side_edge() {
    flags::with_thread_overrides(&[("CRATONVM_G1_MARK_SIDE_TABLES", None)], || {
        let gc = collector();
        let (root, graph) = build(&gc, 0x5732_4300);
        mark_to_fixed_point(&gc, root);
        assert_edges_followed(&gc, &graph, "per-object");
    });
}

/// The same verdicts from the per-batch snapshot.
#[test]
fn the_per_batch_snapshot_follows_every_side_edge() {
    flags::with_thread_overrides(&[("CRATONVM_G1_MARK_SIDE_TABLES", Some("1"))], || {
        let gc = collector();
        let (root, graph) = build(&gc, 0x5732_4340);
        mark_to_fixed_point(&gc, root);
        assert_edges_followed(&gc, &graph, "snapshot");
    });
}

/// A row added to a registry AFTER the snapshot was taken is the one case the
/// hoist gives up freshness on, so it is worth stating what happens: the
/// snapshot is rebuilt whenever `rset_cache_epoch` moves, and a new marking
/// STEP always captures afresh. This drives the second of those — the shape a
/// mid-cycle `set_loader_pin` from a class-definition path produces — and
/// requires the edge to be followed.
#[test]
fn a_registry_row_added_between_steps_is_still_followed() {
    flags::with_thread_overrides(&[("CRATONVM_G1_MARK_SIDE_TABLES", Some("1"))], || {
        let gc = collector();
        let late_class = 0x5732_4320u32;
        let root = gc.alloc_object(ClassId::new(0x5732_4321), 1);
        let holder = gc.alloc_object(ClassId::new(late_class), 0);
        gc.set_field(root, 0, cratonvm_types::Value::Object(Some(holder)));
        let late_loader = gc.alloc_object(ClassId::new(0x5732_4322), 0);

        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &[root]);
        // One bounded step, so a snapshot exists and has already been consulted.
        let _ = gc.concurrent_mark_step(1);

        // Now publish the edge, the way a class definition on a mutator thread
        // would, and re-gray the holder so the closure visits it again.
        cratonvm_types::loader_pin::set_loader_pin(
            VM_ID,
            late_class,
            late_loader.as_ptr() as usize,
        );
        gc.remark(&stw(), &[root, holder]);
        while !gc.concurrent_mark_step(usize::MAX) {}

        assert!(
            gc.is_live_after_mark(late_loader.as_ptr() as usize),
            "a snapshot is per-step, so an edge published between steps is picked \
             up by the next one"
        );
    });
}
