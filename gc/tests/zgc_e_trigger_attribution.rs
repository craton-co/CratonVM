// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
/// These tests pin the live-bytes / warm-up / headroom clauses of `needs_gc`.
/// The deciding director (`CRATONVM_ZGC_RATE_TRIGGER`, default ON) replaces the
/// 75 percent live-bytes clause with its own byte thresholds, so the tests run
/// with it off; the director has its own rows. The override covers the whole
/// test: the heap reads its switches at construction and per poll.

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

/// The warm-up reason correctly precedes the live-byte threshold for a fresh
/// heap. Spend its three collections so these tests isolate threshold
/// attribution without mutating a process environment flag.
///
/// zr zs s1/tp: each of the three must be a WARM-UP cycle (asked for by the
/// warm-up clause), because the clause now counts warm-up cycles, not
/// collections (`CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS`): an unasked collection,
/// like a `System.gc()`, no longer spends a step.
fn drain_warmup_cycles(heap: &ZgcRealHeap) {
    let mut roots = Vec::new();
    for _ in 0..3 {
        churn_to(heap, 50 * MIB);
        assert!(heap.needs_gc(), "the warm-up clause asks for this cycle");
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    }
}




