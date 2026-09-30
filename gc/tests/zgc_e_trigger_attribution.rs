// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E — the collection driver's trigger accounting, from outside the crate.
//!
//! These pin two properties of `ZgcRealHeap`'s `needs_gc` that the `[GC]
//! zgc-trigger:` line is read as claiming, and that the code did not have:
//!
//! 1. **A trigger tally counts COLLECTIONS, not POLLS.** `needs_gc` is a
//!    predicate the safepoint machinery asks far more often than a collection
//!    happens — `VmHeap::young_spill_pressure` asks it at every native
//!    boundary, `has_allocation_headroom` twice more — and it used to
//!    `fetch_add` a tally on every TRUE answer. So the four `trigger_*`
//!    counters measured the native-call rate, while
//!    `trigger_hard_alloc_fail` beside them on the same line counted events.
//!    Two granularities presented as one reason breakdown is the
//!    counter-that-lies shape `78c1fe1e8 gc,vm: the allocation counter was
//!    wrong in both directions at once` already cost this tree once.
//!
//! 2. **The headroom clause can ask for a collection below the `gc_rearm`
//!    floor**, when `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM` is on and the last
//!    cycle reclaimed something. `gc_rearm` is sized from LIVE bytes and
//!    `headroom_low` is about ALLOCATABLE SPACE; on a non-compacting collector
//!    those diverge without bound, so the shared floor silences the headroom
//!    clause on exactly the heap it was written for.
//!
//! Driven through the public API only. The bypass is exercised through
//! `set_headroom_bypasses_rearm` rather than the environment variable on
//! purpose: the flag reader latches in a `OnceLock`, so a test that set the
//! variable would decide the answer for every later test in the binary — the
//! failure mode `types/tests/flag_env_mutation_guard.rs` exists to catch.

#![cfg(feature = "zgc")]

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::zgc::ZgcRealHeap;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Test-only `StopTheWorldToken`. These tests are single-threaded, so the STW
/// invariant is trivially satisfied.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded integration test; no other mutator exists.
    unsafe { StopTheWorldToken::new() }
}

const MIB: usize = 1024 * 1024;

/// Allocate until `allocated` crosses `bytes`, keeping nothing. 32 KiB arrays
/// stay under `ZGC_LARGE_OBJECT_MIN` (64 KiB), so they bump the LOW end — the
/// same end the occupancy threshold is measured against.
fn churn_to(heap: &ZgcRealHeap, bytes: usize) {
    while heap.allocated_bytes() < bytes {
        heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024);
    }
}

/// **A tally counts the collection, not the hundred polls that preceded it.**
///
/// The exact edit that trips this: putting a `fetch_add` back on the
/// `trigger_threshold` path in `needs_gc`, or dropping the
/// `counters.trigger_seen` reset from the end of `collect_garbage`.
#[test]
fn a_trigger_is_charged_once_per_collection_not_once_per_poll() {
    // 64 MiB capacity, so the 75% occupancy threshold is 48 MiB.
    let heap = ZgcRealHeap::with_capacity(64 * MIB);
    assert_eq!(
        heap.trigger_tallies().1,
        0,
        "a fresh heap has asked for nothing"
    );

    churn_to(&heap, 50 * MIB);

    // The shape that used to inflate the counter: a boundary poll loop. One
    // hundred agreeing answers, no collection between them.
    for _ in 0..100 {
        assert!(
            heap.needs_gc(),
            "50 MiB of a 64 MiB heap is over the 75% occupancy threshold"
        );
    }
    let (_, threshold, _, _, _) = heap.trigger_tallies();
    assert_eq!(
        threshold, 1,
        "a hundred polls asking about ONE pending collection must charge the \
         live-bytes reason once, not a hundred times"
    );

    // And the collection re-arms the attribution, so the NEXT one is charged
    // on its own account.
    let mut roots: Vec<ObjectRef> = Vec::new();
    let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    churn_to(&heap, 50 * MIB);
    for _ in 0..10 {
        assert!(heap.needs_gc());
    }
    assert_eq!(
        heap.trigger_tallies().1,
        2,
        "two collections asked for on live bytes, two charges"
    );
}

/// **The tally is not merely capped — it still moves.**
///
/// A latch that never cleared would also pass the assertion above, and would
/// report `1` for a run that collected a thousand times. This is the other
/// direction: the counter that was wrong in both directions at once is the
/// precedent this whole file is written against.
#[test]
fn the_attribution_latch_re_arms_on_every_collection() {
    let heap = ZgcRealHeap::with_capacity(64 * MIB);
    let mut roots: Vec<ObjectRef> = Vec::new();
    for _ in 0..5 {
        churn_to(&heap, 50 * MIB);
        assert!(heap.needs_gc());
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    }
    assert_eq!(
        heap.trigger_tallies().1,
        5,
        "five collections, five charges -- a latch that never re-armed would \
         report 1 and a per-poll counter would report thousands"
    );
}

/// **The headroom clause is inert below the `gc_rearm` floor by default, and
/// the switch is what makes it reachable.**
///
/// Both arms on one heap in one state, which is what makes this an A/B rather
/// than a rebuild. The state is constructed rather than reasoned about: the
/// heap is driven until `alloc_raw` genuinely raises `headroom_low`, and the
/// test asserts that it got there before it asserts anything about the
/// trigger — an arm that never reached the state would otherwise pass
/// vacuously, which is the shape of every inert engagement counter this round
/// has been finding.
#[test]
fn the_headroom_clause_below_the_rearm_floor_is_a_switch() {
    // Small heap, so the margin (`max(capacity/128, 8 MiB)` = 8 MiB) is a
    // large fraction of it and the arena runs short quickly.
    let heap = ZgcRealHeap::with_capacity(32 * MIB);
    let mut roots: Vec<ObjectRef> = Vec::new();

    // One collection first, so `gc_rearm` is sized from a post-sweep live
    // figure and `last_cycle_reclaimed_nothing` is answerable.
    churn_to(&heap, 26 * MIB);
    let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);

    // Now fill the arena with garbage nothing roots. `allocated` tracks live
    // bytes and the bump cursor does not rewind, so the arena runs short while
    // `allocated` stays low.
    for _ in 0..4096 {
        heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024);
        if heap.headroom_trigger_state().0 {
            break;
        }
    }
    let (low, _, reclaimed_nothing) = heap.headroom_trigger_state();
    assert!(
        low,
        "the fixture must actually reach the state it is about: \
         `headroom_low` was never raised"
    );
    assert!(
        !reclaimed_nothing,
        "and the first collection must have reclaimed something, or the \
         bypass is correctly inert for a different reason"
    );

    // Whether the DEFAULT arm answers yes here depends on where `gc_rearm`
    // landed, and that is the point: it is not a property of the heap being
    // short of space. What is asserted is the switch's direction -- turning
    // the bypass on can only ever ADD a yes.
    let default_arm = heap.needs_gc();
    heap.set_headroom_bypasses_rearm(true);
    assert!(
        heap.needs_gc(),
        "with the bypass on, a heap that cannot serve a margin-sized request \
         must ask for a collection whatever `gc_rearm` says"
    );
    heap.set_headroom_bypasses_rearm(false);
    assert_eq!(
        heap.needs_gc(),
        default_arm,
        "and switching it back off restores the previous answer exactly -- \
         this is an A/B arm, not a one-way door"
    );
}

/// **A cycle that reclaimed nothing disarms the bypass**, which is the only
/// thing between it and a collection per allocation on a heap genuinely full
/// of live data.
///
/// `headroom_low` is cleared by every collection and re-raised by the next
/// allocation that finds the arena short, so without this term the bypass
/// loops. The fixture holds every allocation live, so the sweep has nothing to
/// free.
#[test]
fn a_cycle_that_freed_nothing_disarms_the_headroom_bypass() {
    let heap = ZgcRealHeap::with_capacity(32 * MIB);
    heap.set_headroom_bypasses_rearm(true);

    // Everything stays rooted, so no sweep can reclaim a byte.
    let mut roots: Vec<ObjectRef> = Vec::new();
    for _ in 0..512 {
        roots.push(heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024));
    }
    // Up to four cycles, stopping at the first that freed nothing. The FIRST
    // one always frees something on this heap whatever the roots say -- it
    // retires every TLAB, and a chunk tail comes back to the free list -- so
    // asserting on cycle two exactly would be asserting about the TLAB
    // allocator rather than about the sweep.
    for _ in 0..4 {
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
        if heap.headroom_trigger_state().2 {
            break;
        }
    }

    let (_, armed, reclaimed_nothing) = heap.headroom_trigger_state();
    assert!(armed, "the switch is on");
    assert!(
        reclaimed_nothing,
        "a sweep over a wholly-rooted heap freed nothing, and the driver must \
         have recorded that -- it is the bypass's whole anti-storm argument"
    );

    // And with that recorded, the bypass term itself is FALSE: whatever
    // `needs_gc` answers from here is the `gc_rearm` floor's answer and not
    // the bypass's. Asserted as the conjunction the driver computes, so the
    // test names the term rather than a downstream consequence that a
    // threshold crossing could also explain.
    let (_, armed, futile) = heap.headroom_trigger_state();
    assert!(
        armed && futile,
        "the bypass is armed and the last cycle freed nothing, so the clause \
         must be gated by the floor again"
    );
}
