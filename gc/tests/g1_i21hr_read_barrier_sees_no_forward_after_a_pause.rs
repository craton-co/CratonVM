// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i21/hr -- on G1, the interpreter's software read barrier
//! (`VmHeap::load_and_forward`) can never repair a word a compiled load reads
//! raw, because no forward is visible to it once a pause has ended.
//!
//! # Why this is pinned
//!
//! The G1 parked-virtual-thread holder page
//! (`docs/internal/gc/gci-i15orch-g1-parked-virtual-thread-holder-graph-reads-wrong-on-the-default-path-RETIRED-UNFIXED-20261018.md`)
//! carried a reading, (r1), that a reference slot left naming a from-space
//! original is read as the live copy by the interpreter (every loaded
//! reference passes through `load_and_forward`) and as the original by
//! compiled code (`jit_getfield` -> `jit_load_ref_slot` reads the slot raw).
//! That asymmetry needs a word that `load_and_forward` maps to something other
//! than itself between pauses. `load_and_forward_inner` changes its argument
//! only when the address is a live object START (`is_object_start`) whose
//! header is FORWARDED. These tests pin that no such address exists after a G1
//! pause, for every from-space key the pause's own pointer map names:
//!
//! * a freed collection-set region is `Free`, and `live_span_at` answers
//!   `None` for it, so the address is not an object start;
//! * a re-issued region is zero-filled by `bump_alloc_initialized` before the
//!   allocator hands a byte out, so an object start there is a fresh header;
//! * a kept region (evacuation failure) has its forwards retired and its moved
//!   originals overwritten by fillers (`retire_forwards`,
//!   `free_or_keep_cset`); that arm is pinned in-crate by
//!   `no_object_carries_a_forwarding_tag_after_a_pause_that_keeps_regions`.
//!
//! If one of these ever fails, interpreted and compiled loads of the same slot
//! can disagree on G1 and (r1) is back on the table.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --test g1_i21hr_read_barrier_sees_no_forward_after_a_pause`.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::{G1Collector, ObjectHeader};
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const REGION: usize = 1024 * 1024;

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 32 * REGION,
        initial_heap_size: 32 * REGION,
        region_size: REGION,
        ..G1CollectorConfig::default()
    })
}

/// A rooted singly-linked chain of `len` one-field objects, plus `garbage`
/// unreachable ones, all in Eden.
fn chain_with_garbage(gc: &G1Collector, len: usize, garbage: usize) -> ObjectRef {
    let head = gc.alloc_object(ClassId::new(31), 1);
    let mut prev = head;
    for i in 0..len {
        let next = gc.alloc_object(ClassId::new(31), 1);
        gc.set_field(prev, 0, Value::Object(Some(next)));
        prev = next;
        if i % 4 == 0 {
            for _ in 0..garbage {
                let junk = gc.alloc_object(ClassId::new(32), 2);
                gc.set_field(junk, 1, Value::Int(7));
            }
        }
    }
    head
}

/// Every key `k` the pause moved (`k != v`) must be invisible to the read
/// barrier: not an object start, or an object start whose header is not
/// FORWARDED. Returns how many moved keys were checked.
fn assert_no_forward_visible(gc: &G1Collector, map: &cratonvm_types::PointerMap, when: &str) -> usize {
    let mut moved = 0usize;
    for (&k, &v) in map.iter() {
        if k == v {
            continue;
        }
        moved += 1;
        if !gc.is_object_start(k) {
            continue;
        }
        // SAFETY: `is_object_start` just proved a live, tag-validated object
        // header at `k`, and nothing runs between the test and this read.
        let header = unsafe { &*(k as *const ObjectHeader) };
        assert!(
            !header.is_forwarded(),
            "{when}: from-space key 0x{k:x} (moved to 0x{v:x}) is a live object start whose \
             header is still FORWARDED -- `load_and_forward` would follow it while a compiled \
             load reads the slot raw, so interpreted and compiled code could see different objects"
        );
    }
    moved
}

/// One young pause: every from-space original it moved is unreachable to the
/// read barrier as soon as the pause returns.
#[test]
fn gci_i21hr_no_from_space_original_is_a_forwarded_object_start_after_a_young_pause() {
    let gc = collector();
    let head = chain_with_garbage(&gc, 2000, 8);
    let mut roots = vec![head];
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    let moved = assert_no_forward_visible(&gc, &result.pointer_map, "after the pause");
    assert!(
        moved > 0,
        "fixture: the pause must have moved something, or this test checks nothing"
    );
    // The chain still reads whole through the collector's own accessors.
    let mut cur = roots[0];
    let mut n = 0usize;
    while let Value::Object(Some(next)) = gc.get_field(cur, 0) {
        cur = next;
        n += 1;
    }
    assert_eq!(n, 2000, "the rooted chain must survive the pause intact");
}

/// The freed regions are re-issued by later allocation (G1AUD-10: `reset` no
/// longer scrubs them, the allocator zero-fills what it hands out). An old
/// from-space address that becomes an object start again must hold a FRESH
/// header, never the stale forward, and a second pause's keys obey the same
/// rule.
#[test]
fn gci_i21hr_reissued_from_space_never_exposes_a_stale_forward() {
    let gc = collector();
    let head = chain_with_garbage(&gc, 1500, 8);
    let mut roots = vec![head];
    let first = gc.young_collection(&mut roots, &NoopMonitors);
    assert!(
        assert_no_forward_visible(&gc, &first.pointer_map, "after pause 1") > 0,
        "fixture: pause 1 must move something"
    );
    // Re-issue the freed space: enough fresh allocation to take back the
    // regions pause 1 freed.
    let fresh = chain_with_garbage(&gc, 1500, 8);
    roots.push(fresh);
    let reissued = first.pointer_map.keys().filter(|&&k| gc.is_object_start(k)).count();
    assert_no_forward_visible(&gc, &first.pointer_map, "after re-issuing pause 1's from-space");
    // A second pause over survivors and the fresh chain.
    let second = gc.young_collection(&mut roots, &NoopMonitors);
    assert!(
        assert_no_forward_visible(&gc, &second.pointer_map, "after pause 2") > 0,
        "fixture: pause 2 must move something"
    );
    assert_no_forward_visible(&gc, &first.pointer_map, "pause 1's keys after pause 2");
    // Recorded, not asserted: whether the fixture actually re-issued an old
    // from-space address as an object start (the assertion above is vacuous
    // for keys it did not).
    eprintln!("[i21hr] pause-1 keys re-issued as object starts before pause 2: {reissued}");
}
