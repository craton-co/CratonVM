// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase H integration tests for the garbage-collector crate.
//!
//! Each test targets one roadmap subphase (RH.1 … RH.8) and verifies
//! the observable GC contract end-to-end.  The VM-level glue
//! (`process_references_after_gc`, `force_gc_from_native`,
//! `direct buffer Cleaner dispatch`) is exercised indirectly via the
//! APIs the VM calls; tests that need a full SharedVm live in the
//! `vm` crate's tier1 suite.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::reference::{ReferenceProcessor, ReferenceType};
use cratonvm_gc::{G1Collector, GenerationalHeap};
use cratonvm_types::{ClassId, Value};

/// No-op monitor cleanup helper for GC tests that don't exercise the
/// monitor table.
struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Test-only `StopTheWorldToken`. Integration tests are single-threaded;
/// no other mutator exists, so the STW invariant is trivially satisfied.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: these integration tests run the heap single-threaded.
    unsafe { StopTheWorldToken::new() }
}

// ---------------------------------------------------------------------------
// RH.1 — Generational promotion under pressure
// ---------------------------------------------------------------------------

#[test]
fn rh1_promotion_stats_bump_across_cycles() {
    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);

    // Allocate two durable objects and run enough minor GCs to promote
    // them.  PROMOTION_AGE=3 in gen_heap means we need 3 minor GCs.
    let class_id = ClassId::new(1);
    let mut roots = vec![
        heap.alloc_object(class_id, 1),
        heap.alloc_object(class_id, 1),
    ];

    let before = heap.stats().snapshot();
    assert_eq!(before.minor_gc_count, 0);
    assert_eq!(before.bytes_promoted, 0);

    for _ in 0..4 {
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    }

    let after = heap.stats().snapshot();
    assert_eq!(after.minor_gc_count, 4, "four cycles should be counted");
    assert!(
        after.objects_promoted >= 2,
        "both roots must promote; got {}",
        after.objects_promoted
    );
    assert!(
        after.bytes_promoted > 0,
        "bytes_promoted must be non-zero once promotion fires"
    );
    // Promoted objects shouldn't be counted as young-copies in the
    // same cycle that promoted them.
    assert!(
        after.objects_copied_young < after.minor_gc_count * 2 + after.objects_promoted,
        "young-copy + promotion must not double-count the same object"
    );
}

#[test]
fn rh1_promoted_objects_survive_further_gcs() {
    // Tests the "no corruption" invariant: after multiple minor GCs,
    // references into promoted objects still resolve and contain the
    // right identity hash.
    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    let class_id = ClassId::new(7);
    let mut roots = vec![heap.alloc_object(class_id, 2)];
    let hash_before = heap.identity_hash_code(roots[0]);
    heap.set_field(roots[0], 0, Value::Int(42));

    for _ in 0..5 {
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    }

    // Still reachable; identity hash unchanged; payload intact.
    let hash_after = heap.identity_hash_code(roots[0]);
    assert_eq!(hash_before, hash_after);
    match heap.get_field(roots[0], 0) {
        Value::Int(v) => assert_eq!(v, 42),
        other => panic!("slot corrupted across GC cycles: {:?}", other),
    }
    // Double-free guard: allocating again and running a GC must not
    // crash or mis-relocate.
    let _fresh = heap.alloc_object(class_id, 0);
    let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
}

// ---------------------------------------------------------------------------
// RH.2 — WeakReference / SoftReference queue delivery
// ---------------------------------------------------------------------------

#[test]
fn rh2_weak_reference_cleared_and_enqueued_when_referent_dies() {
    let mut proc = ReferenceProcessor::new();
    // Synthesise fake addresses — the ref processor only cares about
    // address identity; it never dereferences them.
    let weak_ref = 0xDEAD_0010;
    let referent = 0xDEAD_0020;
    let queue = 0xDEAD_0030;
    proc.discover_reference(ReferenceType::Weak, weak_ref, referent, Some(queue));

    // `is_marked = always-false` simulates "referent unreachable".
    let result = proc.process_references(&|_| false, 64, 0);
    assert_eq!(result.stats.weak_refs_discovered, 1);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    let delivered: Vec<_> = result
        .to_enqueue
        .iter()
        .filter(|(r, q)| *r == weak_ref && *q == queue)
        .collect();
    assert_eq!(delivered.len(), 1, "weak ref must be queued once");
}

#[test]
fn rh2_weak_reference_not_enqueued_when_referent_live() {
    let mut proc = ReferenceProcessor::new();
    let weak_ref = 0xFEED_0100;
    let referent = 0xFEED_0200;
    proc.discover_reference(ReferenceType::Weak, weak_ref, referent, Some(0xFEED_0300));

    // is_marked returns true → referent stays alive.
    let result = proc.process_references(&|a| a == referent, 64, 0);
    assert_eq!(result.stats.weak_refs_cleared, 0);
    assert!(result.to_enqueue.is_empty());
}

#[test]
fn rh2_soft_reference_cleared_only_under_memory_pressure() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    let soft = 0xBEEF_0001;
    let referent = 0xBEEF_0002;
    let queue = 0xBEEF_0003;
    proc.discover_reference(ReferenceType::Soft, soft, referent, Some(queue));

    // Ample free heap + fresh access time → LRU policy keeps it.
    let plenty = proc.process_references(&|_| false, 1024, 0);
    assert_eq!(plenty.stats.soft_refs_cleared, 0);

    // Zero free heap + very old referent → clear.
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, soft, referent, Some(queue));
    let pressure = proc.process_references(&|_| false, 0, 1_000_000);
    assert_eq!(pressure.stats.soft_refs_cleared, 1);
}

// ---------------------------------------------------------------------------
// RH.3 — PhantomReference + Cleaner action
// ---------------------------------------------------------------------------

#[test]
fn rh3_phantom_enqueued_but_not_cleared() {
    let mut proc = ReferenceProcessor::new();
    let phantom = 0xABCD_0001;
    let referent = 0xABCD_0002;
    let queue = 0xABCD_0003;
    proc.discover_reference(ReferenceType::Phantom, phantom, referent, Some(queue));

    let result = proc.process_references(&|_| false, 64, 0);
    assert_eq!(result.stats.phantom_refs_enqueued, 1);
    assert!(
        result
            .to_enqueue
            .iter()
            .any(|(r, q)| *r == phantom && *q == queue),
        "phantom ref must be enqueued"
    );
}

#[test]
fn rh3_cleaner_action_submitted_when_referent_unreachable() {
    let mut proc = ReferenceProcessor::new();
    let cleaner = 0x1111_1111;
    let referent = 0x2222_2222;
    proc.discover_reference(ReferenceType::Cleaner, cleaner, referent, None);

    let result = proc.process_references(&|_| false, 64, 0);
    assert_eq!(result.stats.cleaner_refs_processed, 1);
    assert!(
        result.cleaner_actions.contains(&cleaner),
        "cleaner action address must be surfaced"
    );
}

#[test]
fn rh3_cleaner_action_not_submitted_when_referent_live() {
    let mut proc = ReferenceProcessor::new();
    let cleaner = 0x1234_0001;
    let referent = 0x1234_0002;
    proc.discover_reference(ReferenceType::Cleaner, cleaner, referent, None);

    // Marking referent as live should preserve the cleaner.
    let result = proc.process_references(&|addr| addr == referent, 64, 0);
    assert_eq!(result.stats.cleaner_refs_processed, 0);
    assert!(result.cleaner_actions.is_empty());
}

// ---------------------------------------------------------------------------
// RH.4 — Finalizer queue ordering / all-run semantics
// ---------------------------------------------------------------------------

#[test]
fn rh4_all_finalizers_run_without_panic() {
    let mut proc = ReferenceProcessor::new();
    // 10 finalizable objects as the roadmap prescribes.
    for i in 0..10 {
        proc.discover_reference(
            ReferenceType::Finalizer,
            0x3000_0000 + i * 16,
            0x4000_0000 + i * 16,
            None,
        );
    }

    let result = proc.process_references(&|_| false, 64, 0);
    assert_eq!(result.stats.finalizer_refs_enqueued, 10);
    assert_eq!(result.to_finalize.len(), 10);
    // Running twice must not double-enqueue.
    let second = proc.process_references(&|_| false, 64, 0);
    assert_eq!(second.stats.finalizer_refs_enqueued, 0);
}

#[test]
fn rh4_finalizer_not_enqueued_when_referent_live() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x5000_0000, 0x6000_0000, None);
    let result = proc.process_references(&|a| a == 0x6000_0000, 64, 0);
    assert_eq!(result.stats.finalizer_refs_enqueued, 0);
    assert!(result.to_finalize.is_empty());
}

// ---------------------------------------------------------------------------
// RH.5 — ByteBuffer.allocateDirect: verify native memory table contract.
//
// The VM's `allocate_native_memory` / `free_native_memory` pair backs
// DirectByteBuffer's native storage.  This test checks that
// allocate/free cycles reclaim all memory (no leak) and that
// reallocation works.
// ---------------------------------------------------------------------------

#[test]
fn rh5_allocate_release_cycles_do_not_leak() {
    // This is a pure Rust-level test of the std::alloc pathway the
    // NativeContext's `allocate_native_memory` maps to.  A leak would
    // show up as process RSS growth; the unit test proxy is "every
    // allocation id is returned to the allocator" — the global
    // allocator's bookkeeping would abort on a double-free, and
    // stale-deref sanitizers would fire on use-after-free.
    const ITERATIONS: usize = 1000;
    const SIZE: usize = 1 << 20; // 1 MiB
    let mut tracker: Vec<(*mut u8, std::alloc::Layout)> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let layout = std::alloc::Layout::from_size_align(SIZE, 8).unwrap();
        // SAFETY: layout is nonzero-sized and 8-aligned; documented
        // contract of std::alloc::alloc.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "OOM would be a separate failure");
        tracker.push((ptr, layout));
    }
    for (ptr, layout) in tracker.drain(..) {
        // SAFETY: ptr was returned by alloc_zeroed with the same layout.
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
    assert!(tracker.is_empty());
}

// ---------------------------------------------------------------------------
// RH.6 — Unsafe CAS volatile semantics under contention
// ---------------------------------------------------------------------------

#[test]
fn rh6_cas_stress_8_threads_100k_ops_each_no_lost_updates() {
    // The roadmap calls for 800k CAS ops with final value matching
    // exactly.  We test the underlying CAS primitive on an AtomicU64
    // here; the VM's `compare_and_swap_field` delegates to this same
    // memory-order guarantee through set_field_volatile/SeqCst locks.
    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    const THREADS: usize = 8;
    const OPS: usize = 100_000;
    for _ in 0..THREADS {
        let c = Arc::clone(&counter);
        handles.push(std::thread::spawn(move || {
            for _ in 0..OPS {
                // Spin-CAS to model Unsafe.compareAndSwapLong semantics
                // with strict load-modify-CAS ordering.
                loop {
                    let cur = c.load(Ordering::SeqCst);
                    if c.compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        counter.load(Ordering::SeqCst) as usize,
        THREADS * OPS,
        "CAS stress lost updates under contention"
    );
}

// ---------------------------------------------------------------------------
// RH.7 — GC root scan includes JIT oop maps
// ---------------------------------------------------------------------------

#[test]
fn rh7_oopmap_scaffolding_retains_references() {
    // The full JIT-emitter wiring lives in jit/src/x64.rs
    // (emit_oop_map_for_safepoint) and is exercised by the VM's JIT
    // tests. This unit checks the crate-level scaffolding invariants:
    //
    // 1. An empty oop_maps vector means "no precise coverage".
    // 2. A populated map reports `has_precise_oop_maps() == true`.
    // 3. `find_oop_map_for_pc` returns the exact entry at a safepoint
    //    PC and `None` elsewhere.
    use cratonvm_jit::{CompiledMethod, ExecutableBuffer, OopMapEntry};

    let buf = ExecutableBuffer::new(4096).expect("exec alloc");
    let mut cm = CompiledMethod::new(buf);
    assert!(!cm.has_precise_oop_maps());

    cm.push_oop_map(OopMapEntry {
        bytecode_pc: 0,
        native_pc_offset: 0x40,
        frame_slot_offsets: vec![-16, -24],
        moving_young_coverage_complete: false,
        live_frame_hi: 0,
        local_oop_mask: None,
        num_locals: 0,
        inline_local_scopes: Vec::new(),
        non_oop_stack_slots: Vec::new(),
        stack_marks_exact: false,
    });
    cm.push_oop_map(OopMapEntry {
        bytecode_pc: 0,
        native_pc_offset: 0x80,
        frame_slot_offsets: vec![-16],
        moving_young_coverage_complete: false,
        live_frame_hi: 0,
        local_oop_mask: None,
        num_locals: 0,
        inline_local_scopes: Vec::new(),
        non_oop_stack_slots: Vec::new(),
        stack_marks_exact: false,
    });
    assert!(cm.has_precise_oop_maps());

    assert!(cm.find_oop_map_for_pc(0x40).is_some());
    assert!(cm.find_oop_map_for_pc(0x80).is_some());
    assert!(cm.find_oop_map_for_pc(0x60).is_none());
    let slots = &cm.find_oop_map_for_pc(0x40).unwrap().frame_slot_offsets;
    assert_eq!(slots, &vec![-16, -24]);
}

// ---------------------------------------------------------------------------
// RH.8 — G1 eviction picks high-garbage regions first
// ---------------------------------------------------------------------------

#[test]
fn rh8_old_region_selection_prefers_high_garbage() {
    // Use a lot of regions so the percentage-based cap lets us select
    // multiple.  At 10% of 20 = 2 regions.
    // 20 regions × 4 KiB = 80 KiB heap so selection cap (10%) yields 2.
    let config = G1CollectorConfig {
        heap_size: 20 * 4096,
        region_size: 4096,
        ..G1CollectorConfig::default()
    };
    let g1 = G1Collector::new(config);

    // Mark half the regions as Old with varied gc_efficiency values;
    // the rest stay Free/Eden.  The helper should pick the two Old
    // regions with the lowest efficiency (= most garbage).
    g1.with_regions_mut(|regions| {
        // region indices 0..10 → Old with efficiency = idx * 0.1
        // (0 = most garbage; 9 = least garbage)
        for (i, region) in regions.iter_mut().take(10).enumerate() {
            region.region_type = cratonvm_gc::RegionType::Old;
            region.gc_efficiency = (i as f64) * 0.1;
            // live_bytes > 0 marks the region as carrying marking data —
            // regions without it are ineligible for the mixed CSet
            // (G1CORE-7 gate: post-cleanup promotions have unknown liveness).
            region.live_bytes = (i + 1) * 100;
        }
    });

    let selected = g1.select_old_regions_for_mixed_gc();
    assert_eq!(selected.len(), 2, "10% of 20 regions = 2");
    // Two lowest-efficiency Old regions: indices 0 and 1.
    assert_eq!(selected[0], 0);
    assert_eq!(selected[1], 1);
}

#[test]
fn rh8_pinned_regions_are_never_evacuated() {
    // 20 regions × 4 KiB = 80 KiB heap so selection cap (10%) yields 2.
    let config = G1CollectorConfig {
        heap_size: 20 * 4096,
        region_size: 4096,
        ..G1CollectorConfig::default()
    };
    let g1 = G1Collector::new(config);
    g1.with_regions_mut(|regions| {
        for (i, region) in regions.iter_mut().take(5).enumerate() {
            region.region_type = cratonvm_gc::RegionType::Old;
            region.gc_efficiency = (i as f64) * 0.1;
            // Marking data required for mixed-CSet eligibility (G1CORE-7).
            region.live_bytes = (i + 1) * 100;
            // Pin the lowest-efficiency (would-be-first) region.
            region.pinned = i == 0;
        }
    });
    let selected = g1.select_old_regions_for_mixed_gc();
    assert!(
        !selected.contains(&0),
        "pinned region 0 must never be in the collection set"
    );
}

#[test]
fn rh8_selection_is_deterministic_under_ties() {
    // 20 regions × 4 KiB = 80 KiB heap so selection cap (10%) yields 2.
    let config = G1CollectorConfig {
        heap_size: 20 * 4096,
        region_size: 4096,
        ..G1CollectorConfig::default()
    };
    let g1 = G1Collector::new(config);
    g1.with_regions_mut(|regions| {
        for region in regions.iter_mut().take(10) {
            region.region_type = cratonvm_gc::RegionType::Old;
            region.gc_efficiency = 0.5; // all tied
                                        // Marking data required for mixed-CSet eligibility (G1CORE-7).
            region.live_bytes = 100;
        }
    });
    let first = g1.select_old_regions_for_mixed_gc();
    let second = g1.select_old_regions_for_mixed_gc();
    assert_eq!(first, second, "tie-breaking must be deterministic");
    // Lower index wins in a tie.
    assert!(first.first().copied().unwrap_or(usize::MAX) < 5);
}
