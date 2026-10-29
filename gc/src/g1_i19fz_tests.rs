// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i19/fz -- the collector half of
//! `gci-i19sw-g1-resurrected-finalizable-reads-a-null-weakreference-field-...-20261014`
//! (default path): `cleanup`'s dead-object scrub (`scrub_dead_refs_below_tams`)
//! nulls every reference slot of an object in a Survivor region below TAMS that
//! the mark did not reach. A finalizable object waiting in the VM's finalizer
//! queue is exactly such an object unless the marking doors root the queue
//! (`append_pending_finalizer_mark_roots` in
//! `vm/src/runtime/interpreter/gc_and_alloc.rs`, behind
//! `CRATONVM_G1_MARK_PENDING_FINALIZERS`). These tests pin both sides of that
//! contract: unrooted, the holder's field reads null after cleanup (the writer
//! of the probe's null); rooted at the remark, it keeps its field.
//!
//! A child module of `g1` (see the `#[path]` declaration in `g1.rs`), so the
//! private items are reachable through `super`.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i19_fz_`.

use super::*;
use crate::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_types::{ClassId, ObjectRef, Value};

const REGION: usize = 1024 * 1024;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> crate::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { crate::collector::StopTheWorldToken::new() }
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * REGION,
        // The whole heap committed up front, as `small_config` does: these
        // tests read the region table directly.
        initial_heap_size: 8 * REGION,
        region_size: REGION,
        promotion_age: 3,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

/// The probe's shape: a holder `x` whose field 0 names `w`, copied by a young
/// pause into a Survivor region (below the next mark's TAMS), then a cycle
/// whose remark roots `x` or not. Returns field 0 of `x` after `cleanup`.
fn holder_field_after_cycle(root_the_holder_at_remark: bool) -> Value {
    let gc = collector();
    let w = gc.alloc_object(ClassId::new(0x19F2), 0);
    let x = gc.alloc_object(ClassId::new(0x19F1), 2);
    gc.set_field(x, 0, Value::Object(Some(w)));
    let mut roots = vec![x];
    gc.young_collection(&mut roots, &NoMonitors);
    let x = roots[0];
    {
        let regions = gc.regions.read();
        let idx = gc
            .region_for_ptr(&regions, x.as_ptr())
            .expect("the holder is a heap object");
        assert_eq!(
            regions[idx].region_type(),
            RegionType::Survivor,
            "fixture: the young pause copied the holder into a Survivor region"
        );
    }
    assert!(
        matches!(gc.get_field(x, 0), Value::Object(Some(_))),
        "fixture: the copy keeps its field"
    );

    // The finalizer queue is not an ordinary root: before gci i19/fz nothing
    // named `x` to the marker at either door.
    gc.start_concurrent_mark(&stw());
    let mark_roots: Vec<ObjectRef> = if root_the_holder_at_remark {
        vec![x]
    } else {
        Vec::new()
    };
    gc.remark(&stw(), &mark_roots);
    while !gc.concurrent_mark_step(usize::MAX) {}
    assert_eq!(
        gc.is_live_after_mark(x.as_ptr() as usize),
        root_the_holder_at_remark,
        "fixture: the mark reached the holder only when it was a root"
    );
    gc.cleanup(&stw());
    gc.get_field(x, 0)
}

/// The writer of the null: a Survivor-region object the completed mark did
/// not reach has its reference slots scrubbed by `cleanup`, although it is
/// still in place (and, for a queued finalizable, about to be `finalize()`d).
#[test]
fn gci_i19_fz_cleanup_scrubs_the_fields_of_an_unrooted_survivor() {
    if !g1_cleanup_scrub() {
        return; // `CRATONVM_G1_CLEANUP_SCRUB=0` in the environment: no writer
    }
    assert_eq!(
        holder_field_after_cycle(false),
        Value::Object(None),
        "an unmarked object below TAMS is scrubbed: this is the null the probe read"
    );
}

/// The fix's premise: rooted at the remark (as the finalizer queue now is),
/// the holder is marked and traced, so cleanup leaves its field alone and the
/// object it names stays live through the cycle.
#[test]
fn gci_i19_fz_a_mark_root_keeps_its_fields_through_cleanup() {
    assert!(
        matches!(holder_field_after_cycle(true), Value::Object(Some(_))),
        "a rooted holder keeps the reference only it holds"
    );
}
