// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The TLAB reservation **peak** — the number that would have settled whether
//! `CRATONVM_ZGC_TLAB_RESERVED_BYTES` should be the default. (zt t12/rt retired
//! that opt-in byte rule unmeasured; the peak stays a measurement, and the
//! test that pinned it on both sizing arms now pins the one rule left.)
//!
//! # Why a peak and not the current value
//!
//! `gc/tests/zgc_j_tlab_reserved_bytes.rs` pins `tlab_reserved_bytes()`: how
//! many arena bytes are inside a live chunk *right now*. That closed the third
//! complaint of
//! `docs/feature-designs/zgc-architecture.md`
//! — the subsystem could not answer the question at all — but it cannot
//! settle the first one, which is whether the byte rule should replace the
//! count rule.
//!
//! It cannot, because the two rules do not disagree about the current value.
//! They disagree about the **sum outstanding at the worst instant**:
//!
//! * `ZGC_TLAB_RESERVATION_SHARE` is a budget in bytes (`capacity / 16`);
//! * the count rule enforces a per-thread chunk *size* (`budget / live`), so
//!   with N threads refilling at different moments the sum outstanding is
//!   bounded only as they re-refill — and the count is rebuilt at every
//!   collection, which re-opens the window each cycle;
//! * at teardown, when a report is read, nearly every chunk has been retired
//!   and the current value is ~0 on both arms.
//!
//! `tlab_reserved_bytes_peak()` is monotone, so it survives the retire and is
//! still there to be read. Read as `peak / tlab_reservation_budget_bytes()`,
//! on both arms of one binary: far above 1 on the count arm is the unbounded
//! shape; near 1 on the byte arm is the budget doing its job; the two arms
//! agreeing means the workload never reached the regime and that run decides
//! nothing.
//!
//! # No wall-clock assertions
//!
//! Every assertion below is a byte count or an ordering between byte counts.

#![cfg(feature = "zgc")]

use std::sync::Arc;

use cratonvm_gc::tlab::TlabTailSink;
use cratonvm_gc::zgc::ZgcRealHeap;

/// 64 MiB, the same geometry `zgc_j_tlab_reserved_bytes.rs` uses: a 64 KiB
/// chunk ceiling (`capacity / 1024`) against a 4 MiB reservation budget
/// (`capacity / 16`), so the budget binds before the clamp does.
const CAPACITY: usize = 64 * 1024 * 1024;

fn heap() -> Arc<ZgcRealHeap> {
    let heap = ZgcRealHeap::new_shared(CAPACITY);
    heap.set_vm_tlab_enabled(true);
    heap
}

/// The peak is a high-water mark: it rises with a carve and **does not fall**
/// with the retire that empties the current value.
///
/// That is the whole property, and it is the reason the counter is readable at
/// shutdown at all — a report taken after every thread has retired sees
/// `tlab_reserved_bytes() == 0` on both sizing arms and learns nothing.
#[test]
fn the_peak_survives_the_retire_that_empties_the_current_value() {
    let heap = heap();
    assert_eq!(
        heap.tlab_reserved_bytes_peak(),
        0,
        "a fresh heap claims nothing"
    );

    let (ptr, size) = heap
        .refill_tlab(64 * 1024)
        .expect("a 64 MiB heap serves a chunk");
    assert!(size > 0);
    assert_eq!(heap.tlab_reserved_bytes(), size);
    assert_eq!(heap.tlab_reserved_bytes_peak(), size);

    let start = ptr as usize;
    assert!(heap.reclaim_tlab_tail(start, start + size));
    assert_eq!(
        heap.tlab_reserved_bytes(),
        0,
        "the retire returns the chunk, as zgc_j_tlab_reserved_bytes.rs pins",
    );
    assert_eq!(
        heap.tlab_reserved_bytes_peak(),
        size,
        "...and the peak does not follow it down. A counter that did would read zero \
         at every shutdown and could not distinguish the two sizing rules at all.",
    );
}

/// Concurrent carves on many threads: the peak must end up at least as large
/// as the largest total that was ever simultaneously outstanding.
///
/// The threads exit without retiring, deliberately — that is the case the
/// count-based rule forgets about at the next collection and the byte-based
/// rule does not, and it is the shape of the Tomcat `TestNonBlockingAPI`
/// defect `ZGC_TLAB_RESERVATION_SHARE` exists for.
#[test]
fn the_peak_is_at_least_the_sum_of_chunks_held_at_once() {
    let heap = heap();
    const THREADS: usize = 8;

    let sizes: Vec<usize> = (0..THREADS)
        .map(|_| {
            let h = Arc::clone(&heap);
            std::thread::spawn(move || {
                let (_ptr, size) = h.refill_tlab(64 * 1024).expect("a chunk");
                size
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|j| j.join().expect("worker must not panic"))
        .collect();

    let held: usize = sizes.iter().sum();
    assert!(held > 0);
    assert_eq!(
        heap.tlab_reserved_bytes(),
        held,
        "eight threads exited without retiring, so all eight chunks are still claimed",
    );
    assert!(
        heap.tlab_reserved_bytes_peak() >= held,
        "peak {} must be at least the {} bytes outstanding at once",
        heap.tlab_reserved_bytes_peak(),
        held,
    );
}

/// The counter is charged by the claimant-count rule's carve.
#[test]
fn the_peak_is_charged_by_the_claimant_count_rule() {
    let heap = heap();
    let (_ptr, size) = heap.refill_tlab(64 * 1024).expect("a chunk");
    assert!(
        heap.tlab_reserved_bytes_peak() >= size,
        "the peak must be charged by the carve",
    );
    assert!(
        heap.tlab_reservation_budget_bytes() > 0,
        "the budget is the denominator the peak is read against",
    );
}
