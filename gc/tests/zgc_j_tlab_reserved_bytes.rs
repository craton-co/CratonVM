// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The TLAB reservation is a **byte** budget, so bound it in bytes.
//!
//! # The defect this closes, and why the wave-1 fix was only half of it
//!
//! A TLAB chunk is *claimed* memory: the part of it not yet handed to an
//! object belongs to no object and to no free list, and no collection can
//! reclaim it while its owner is alive. `ZGC_TLAB_RESERVATION_SHARE` exists
//! because that fact once ate an entire heap — Tomcat's `TestNonBlockingAPI`
//! at `-Xmx2g`, ~4 000 threads × a flat 512 KiB chunk, with 1.99 GB of a
//! 2.15 GB heap on the free list in pieces and a 2 MB `char[]` with nowhere to
//! go.
//!
//! The constant's budget is **`capacity / 16` bytes**. What was enforced was a
//! **per-thread chunk size**, derived from a *claimant count*:
//! `chunk = budget / live`. Those are only the same thing if every claimant
//! refills at the same instant, and two properties say they do not:
//!
//! 1. Each thread sizes against the count *at its own refill*, so the sum
//!    outstanding is not bounded by the budget — it converges towards it only
//!    as threads re-refill.
//! 2. **The count is rebuilt at every collection.** Immediately afterwards the
//!    divisor is 1 again and the first refill of each of N threads is sized at
//!    the ceiling. `N × ceiling` outstanding is the unbounded shape, merely
//!    bounded in *time* instead of in size.
//!
//! And nothing measured the outstanding reservation at all: `allocated`
//! charges per object on the arena-TLAB path and per chunk on the VM-TLAB
//! path, so neither half of the subsystem could answer "how many arena bytes
//! are claimed by a chunk and handed to no object".
//!
//! `ZgcRealHeap::tlab_reserved_bytes()` is that number. The opt-in sizing
//! rule built on it (`CRATONVM_ZGC_TLAB_RESERVED_BYTES`) was never measured on
//! a thread-explosion workload and was retired in zt t12/rt, with the two
//! tests that pinned only its arm; the counter stays a measurement.
//!
//! # No wall-clock assertions
//!
//! Every assertion below is a byte count or an ordering between byte counts.

#![cfg(feature = "zgc")]

use std::sync::Arc;

use cratonvm_gc::tlab::TlabTailSink;
use cratonvm_gc::zgc::ZgcRealHeap;

/// 64 MiB: `chunk_bytes_for_capacity` yields a 64 KiB ceiling
/// (`capacity / 1024`) against a 4 MiB reservation budget (`capacity / 16`),
/// so the budget is reached after ~64 chunks and the bound is the binding
/// constraint rather than the clamp.
const CAPACITY: usize = 64 * 1024 * 1024;

fn heap() -> Arc<ZgcRealHeap> {
    let heap = ZgcRealHeap::new_shared(CAPACITY);
    heap.set_vm_tlab_enabled(true);
    heap
}

/// One refill on a **fresh** thread, returning the chunk's extent.
///
/// Fresh threads on purpose: a claimant is a thread, and the whole question is
/// what happens as the number of them rises. The thread exits without
/// retiring, which is also on purpose — that is the case the count-based rule
/// forgets about at the next collection and the byte-based rule does not.
fn chunk_on_a_fresh_thread(heap: &Arc<ZgcRealHeap>) -> (usize, usize) {
    let heap = Arc::clone(heap);
    std::thread::spawn(move || {
        let (ptr, size) = heap
            .refill_tlab(512 * 1024)
            .expect("a 64 MiB heap must serve a chunk");
        (ptr as usize, size)
    })
    .join()
    .expect("worker must not panic")
}

/// **The number that did not exist.** Every carve charges it, every retire
/// gives it back, and it is maintained whether or not the sizing rule uses it.
#[test]
fn the_reservation_counter_tracks_carve_and_retire() {
    let heap = heap();
    assert_eq!(heap.tlab_reserved_bytes(), 0, "a fresh heap claims nothing");
    assert_eq!(
        heap.tlab_reservation_budget_bytes(),
        CAPACITY / 16,
        "the budget is the constant's own quantity, in bytes",
    );

    // Refill and retire on THIS thread: the VM-side release is keyed on the
    // carving thread's own record, because that thread is the only party that
    // knows the chunk's extent.
    let (ptr, size) = heap.refill_tlab(64 * 1024).expect("a chunk");
    assert!(size > 0);
    assert_eq!(
        heap.tlab_reserved_bytes(),
        size,
        "a live chunk is claimed arena memory and must be counted as such",
    );

    let start = ptr as usize;
    assert!(heap.reclaim_tlab_tail(start, start + size));
    assert_eq!(
        heap.tlab_reserved_bytes(),
        0,
        "a retired chunk is no longer claimed",
    );

    // **The record is what authorises the release.** A tail handed to the
    // sink by a thread that holds no record for it must decrement nothing —
    // otherwise a cross-thread retire, or one of the arena-side `ZArenaTlab`
    // cells (which release through `tlab_retire_locked` instead), would
    // double-release and wrap the counter to `usize::MAX`, pinning every later
    // chunk at the minimum for the life of the process.
    let (ptr2, size2) = heap.refill_tlab(64 * 1024).expect("a second chunk");
    assert_eq!(heap.tlab_reserved_bytes(), size2);
    {
        let peer = Arc::clone(&heap);
        let span = ptr2 as usize;
        std::thread::spawn(move || peer.reclaim_tlab_tail(span, span + size2))
            .join()
            .expect("worker must not panic");
    }
    assert_eq!(
        heap.tlab_reserved_bytes(),
        size2,
        "a thread with no record for the span must not release it; the leak that \
         leaves is bounded, visible and in the safe direction (smaller chunks), \
         which a double-release is not",
    );
}

/// A thread that consumes its chunk whole never reaches the tail sink, so the
/// release has to happen at its **next** refill too — otherwise the thread
/// carries its own spent chunk forever and shrinks every chunk it later asks
/// for.
#[test]
fn a_thread_refilling_again_releases_its_previous_chunk() {
    let heap = heap();
    let (_, first) = heap.refill_tlab(64 * 1024).expect("a chunk");
    assert_eq!(heap.tlab_reserved_bytes(), first);

    // No `reclaim_tlab_tail` in between: this models a buffer bumped to its
    // last byte.
    let (_, second) = heap.refill_tlab(64 * 1024).expect("a second chunk");
    assert_eq!(
        heap.tlab_reserved_bytes(),
        second,
        "one chunk per thread outstanding, not two -- the previous one is \
         released when the next is carved",
    );
}

/// The claimant-count rule, whose defining property is that a collection
/// *does* let the chunk size recover (`gc/tests/zgc_c_vm_tlab_reservation_share.rs`
/// pins the recovery too).
#[test]
fn the_claimant_count_rule_still_recovers_after_a_collection() {
    let heap = heap();

    let fresh = chunk_on_a_fresh_thread(&heap).1;
    let mut crowded = fresh;
    for _ in 0..128 {
        crowded = chunk_on_a_fresh_thread(&heap).1;
    }
    assert!(crowded < fresh);

    heap.retire_all_tlabs();
    assert!(
        chunk_on_a_fresh_thread(&heap).1 > crowded,
        "with the count-based rule a collection rebuilds the divisor, so the \
         chunk size recovers even though nothing was returned",
    );
    // ...and the counter noticed that nothing was returned.
    assert!(
        heap.tlab_reserved_bytes() > heap.tlab_reservation_budget_bytes(),
        "the measurement is independent of the rule: {} B claimed against a \
         {} B budget is precisely the overshoot the count cannot see",
        heap.tlab_reserved_bytes(),
        heap.tlab_reservation_budget_bytes(),
    );
}
