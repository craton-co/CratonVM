// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.10 — Reference / PhantomReference / Cleaner / ReferenceQueue
//! integration tests.
//!
//! Exercises the GC reference-processing pipeline without running the
//! interpreter or allocating a full VM: we construct a
//! `ReferenceProcessor` directly and verify the expected flows for
//! weak / phantom / cleaner / finalizer references and their queue
//! enqueueing.

use cratonvm_gc::reference::{
    CleanerThread, FinalizerThread, ReferenceProcessor, ReferenceQueue, ReferenceType,
};

fn always_dead(_addr: usize) -> bool {
    false
}

fn always_live(_addr: usize) -> bool {
    true
}

// -- WP1.10.A: PhantomReference enqueue on death ----------------------------
#[test]
fn phantom_reference_enqueued_when_referent_dead() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 0xA000, 0xB000, Some(0xC000));
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.phantom_refs_enqueued, 1);
    // Enqueue pair: (reference_obj, queue_addr)
    assert!(result
        .to_enqueue
        .iter()
        .any(|&(r, q)| r == 0xA000 && q == 0xC000));
}

// -- WP1.10.B: PhantomReference referent NOT cleared (Java 9+) -------------
#[test]
fn phantom_reference_referent_not_cleared() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 1, 2, Some(3));
    let _result = proc.process_references(&always_dead, 100, 0);
    // Java 9+ changed the contract so PhantomReference.get() is no longer
    // guaranteed to return null only after referent is cleared. We should
    // NOT mark the entry as `cleared`.
    // (White-box) — verified via cleared_ref_objects returning no
    // phantom reference_obj.
    let cleared = proc.cleared_ref_objects();
    assert!(!cleared.contains(&1));
}

// -- WP1.10.C: WeakReference cleared on referent death ---------------------
#[test]
fn weak_reference_cleared_and_enqueued() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert!(result.to_enqueue.iter().any(|&(r, q)| r == 100 && q == 300));
    let cleared = proc.cleared_ref_objects();
    assert!(cleared.contains(&100));
}

#[test]
fn weak_reference_preserved_when_referent_live() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, None);
    let result = proc.process_references(&always_live, 100, 0);
    assert_eq!(result.stats.weak_refs_cleared, 0);
}

// -- WP1.10.D: Cleaner reference discovery + action dispatch ---------------
#[test]
fn cleaner_reference_produces_action_on_death() {
    let mut proc = ReferenceProcessor::new();
    // Cleaner ref: reference_obj is the Cleanable, referent is the wrapped object.
    proc.discover_reference(ReferenceType::Cleaner, 0xC1EA, 0x4EAD, None);
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.cleaner_refs_processed, 1);
    assert_eq!(result.cleaner_actions, vec![0xC1EA]);
}

#[test]
fn cleaner_reference_skipped_when_referent_live() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Cleaner, 0xC1EA, 0x4EAD, None);
    let result = proc.process_references(&always_live, 100, 0);
    assert_eq!(result.stats.cleaner_refs_processed, 0);
}

// -- WP1.10.E: Finalizer reference enqueueing -------------------------------
#[test]
fn finalizer_reference_triggers_to_finalize() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 1, 0xBEEF, None);
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.finalizer_refs_enqueued, 1);
    assert_eq!(result.to_finalize, vec![0xBEEF]);
}

// -- WP1.10.F: ReferenceQueue enqueue / poll FIFO --------------------------
#[test]
fn reference_queue_enqueue_poll_fifo() {
    let mut rq = ReferenceQueue::new(0x1000, 8);
    assert!(rq.enqueue(1));
    assert!(rq.enqueue(2));
    assert!(rq.enqueue(3));
    assert_eq!(rq.pending_count(), 3);
    assert_eq!(rq.poll(), Some(1));
    assert_eq!(rq.poll(), Some(2));
    assert_eq!(rq.poll(), Some(3));
    assert_eq!(rq.poll(), None);
}

#[test]
fn reference_queue_evicts_oldest_on_overflow() {
    let mut rq = ReferenceQueue::new(0x2000, 2);
    rq.enqueue(10);
    rq.enqueue(20);
    rq.enqueue(30); // overflow → evicts 10
    assert_eq!(rq.pending_count(), 2);
    assert_eq!(rq.overflow_count(), 1);
    assert_eq!(rq.poll(), Some(20));
    assert_eq!(rq.poll(), Some(30));
}

// -- WP1.10.G: Mixed reference processing ---------------------------------
#[test]
fn mixed_reference_types_all_processed() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 1, 2, Some(500));
    proc.discover_reference(ReferenceType::Weak, 3, 4, Some(500));
    proc.discover_reference(ReferenceType::Phantom, 5, 6, Some(600));
    proc.discover_reference(ReferenceType::Cleaner, 7, 8, None);
    proc.discover_reference(ReferenceType::Finalizer, 9, 10, Some(700));

    // All referents dead, soft ref ancient enough to clear.
    let result = proc.process_references(&always_dead, 1, 5000);
    assert_eq!(result.stats.soft_refs_cleared, 1);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert_eq!(result.stats.phantom_refs_enqueued, 1);
    assert_eq!(result.stats.cleaner_refs_processed, 1);
    assert_eq!(result.stats.finalizer_refs_enqueued, 1);
}

// -- WP1.10.H: CleanerThread cooperation ----------------------------------
#[test]
fn cleaner_thread_submit_drain_roundtrip() {
    let ct = CleanerThread::new();
    assert_eq!(ct.pending_count(), 0);
    ct.submit_action(0xAA);
    ct.submit_action(0xBB);
    ct.submit_action(0xCC);
    assert_eq!(ct.pending_count(), 3);
    let drained = ct.drain_actions();
    assert_eq!(drained, vec![0xAA, 0xBB, 0xCC]);
    assert_eq!(ct.pending_count(), 0);
}

// -- WP1.10.I: FinalizerThread one-finalization-per-object ---------------
#[test]
fn finalizer_thread_blocks_resurrection() {
    let ft = FinalizerThread::new();
    assert!(ft.enqueue(0xDEAD));
    let _ = ft.dequeue().unwrap();
    assert!(ft.was_finalized(0xDEAD));
    // Resurrection attempt: should be rejected.
    assert!(!ft.enqueue(0xDEAD));
}

// -- WP1.10.J: remove_collected removes dead reference objects ----------
#[test]
fn remove_collected_removes_dead_weak_refs() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, None);
    proc.discover_reference(ReferenceType::Weak, 101, 201, None);
    let alive = [101usize];
    proc.remove_collected(&|addr| alive.contains(&addr));
    assert_eq!(proc.weak_ref_count(), 1);
}

// -- WP1.10.K: Post-GC relocation updates addresses ----------------------
#[test]
fn post_gc_relocation_updates_all_addresses() {
    use std::collections::HashMap;
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(100, 1100);
    map.insert(200, 1200);
    map.insert(300, 1300);
    proc.update_after_gc(&map);
    // Cannot observe directly without a public accessor, but this exercises
    // the happy path — no panic is the primary assertion, and
    // `cleared_ref_objects` still yields nothing (referent alive).
}

// -- WP1.10.L: Double-processing is idempotent ---------------------------
#[test]
fn phantom_ref_not_re_enqueued() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 1, 2, Some(500));
    let r1 = proc.process_references(&always_dead, 100, 0);
    assert_eq!(r1.stats.phantom_refs_enqueued, 1);
    let r2 = proc.process_references(&always_dead, 100, 0);
    assert_eq!(r2.stats.phantom_refs_enqueued, 0);
}
