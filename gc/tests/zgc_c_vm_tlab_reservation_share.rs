// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The VM thread's TLAB chunk must shrink with the number of threads claiming
//! one.
//!
//! # What this pins, and why it is not a micro-benchmark
//!
//! A TLAB chunk is *claimed* memory, not allocated memory: the part of it not
//! yet handed to an object belongs to no object and to no free list, and no
//! collection can reclaim it while its owning thread is alive.
//! `ZGC_TLAB_RESERVATION_SHARE` exists because that fact once ate an entire
//! heap — Tomcat's `TestNonBlockingAPI` at `-Xmx2g`, ~4 000 threads × a flat
//! 512 KiB chunk, with 1.99 GB of the 2.15 GB heap on the free list in pieces
//! and a 2 MB `char[]` with nowhere to go. The fix was to divide a sixteenth of
//! the arena by the number of buffers actually registered.
//!
//! The divisor counted only `zgc::arena_tlab`'s own cells. Since round 9 wave 5
//! the VM thread's buffer is on by default and is what nearly every Java
//! allocation actually bumps into — and it carves from the same arena, through
//! the same sizing call, while never registering a cell. Every thread was
//! therefore sized as though it were the only claimant, which is the original
//! defect on the path that carries the allocation.
//!
//! The assertion below is the mechanism, not a timing: hand `refill_tlab` a
//! large number of distinct threads and the chunk it returns must come down.
//! Without the accounting it is pinned at the ceiling forever.

use cratonvm_gc::zgc::ZgcRealHeap;

/// 64 MiB: big enough that `chunk_bytes_for_capacity` yields a 64 KiB ceiling
/// (`capacity / 1024`) and a 4 MiB reservation budget (`capacity / 16`), so the
/// budget divided by a few dozen buffers falls below the ceiling and the
/// divisor becomes the binding constraint rather than the clamp.
const CAPACITY: usize = 64 * 1024 * 1024;

/// Enough distinct threads that `budget / live` is unambiguously below the
/// ceiling: 4 MiB / 128 == 32 KiB against a 64 KiB ceiling.
const THREADS: usize = 128;

/// Threads are spawned and joined one at a time on purpose. The count is
/// rebuilt per collection rather than held by a live registration, so a thread
/// that has counted itself stays counted until the next `retire_all_tlabs` —
/// which is exactly the property under test, and it makes the test cheap
/// (128 sequential spawns, one small chunk each) instead of 128 concurrent
/// stacks.
fn chunk_size_for_a_fresh_thread(heap: &std::sync::Arc<ZgcRealHeap>) -> usize {
    let heap = std::sync::Arc::clone(heap);
    std::thread::spawn(move || {
        let (_, size) = heap
            .refill_tlab(512 * 1024)
            .expect("a 64 MiB heap must serve a chunk");
        size
    })
    .join()
    .expect("worker must not panic")
}

#[test]
fn the_vm_tlab_chunk_shrinks_as_threads_claim_one() {
    let heap = ZgcRealHeap::new_shared(CAPACITY);
    heap.set_vm_tlab_enabled(true);

    let first = chunk_size_for_a_fresh_thread(&heap);
    assert!(
        first > 0 && first % 8 == 0,
        "a chunk must be non-empty and on the object grid: {first}",
    );

    let mut sizes = Vec::with_capacity(THREADS);
    sizes.push(first);
    for _ in 1..THREADS {
        sizes.push(chunk_size_for_a_fresh_thread(&heap));
    }
    let last = *sizes.last().expect("THREADS >= 1");

    assert!(
        last < first,
        "the {THREADS}th thread's chunk ({last} B) must be smaller than the \
         first thread's ({first} B) — a chunk is claimed memory, and sizing \
         every thread as though it were alone is what filled a 2 GB heap with \
         reservations",
    );
    // Monotone non-increasing: the divisor only ever grows between collections,
    // so a later thread can never be handed a larger chunk than an earlier one.
    for w in sizes.windows(2) {
        assert!(
            w[1] <= w[0],
            "chunk sizes must not grow as claimants accumulate: {:?}",
            sizes,
        );
    }

    // The engagement counter is what makes any claim about this arm admissible
    // at all: zero refills means the switch was off and the sizes above came
    // from nowhere.
    let (refills, refill_bytes, _, _) = heap.vm_tlab_engagement();
    assert_eq!(refills, THREADS);
    assert_eq!(
        refill_bytes,
        sizes.iter().sum::<usize>(),
        "every chunk handed out must be charged exactly once",
    );
}

/// A collection rebuilds the count from scratch, so a burst of short-lived
/// threads cannot ratchet every later chunk down for the life of the process.
///
/// This is the half that a monotone counter would get wrong, and the reason the
/// count carries an epoch instead of being decremented: there is no reliable
/// "this thread's VM buffer is gone" event, so the answer is to rebuild rather
/// than to track.
#[test]
fn retiring_the_tlabs_lets_the_chunk_size_recover() {
    let heap = ZgcRealHeap::new_shared(CAPACITY);
    heap.set_vm_tlab_enabled(true);

    let fresh = chunk_size_for_a_fresh_thread(&heap);
    for _ in 1..THREADS {
        chunk_size_for_a_fresh_thread(&heap);
    }
    let crowded = chunk_size_for_a_fresh_thread(&heap);
    assert!(crowded < fresh, "{crowded} should be below {fresh}");

    // `retire_all_tlabs` is what every collection runs first; it prunes the
    // registered cells of dead threads and opens a new counting epoch for the
    // VM-side buffers.
    heap.retire_all_tlabs();

    let after = chunk_size_for_a_fresh_thread(&heap);
    assert!(
        after > crowded,
        "after a collection the count is rebuilt from the threads that are \
         still allocating, so a dead burst must not keep the chunk small \
         ({after} B vs {crowded} B while crowded)",
    );
}
