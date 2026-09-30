// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational GC round 4 wave 2, lane `oldgen2` (2026-09-23): the old
//! generation's reserve/commit backing store, seen from the heap's public API.
//!
//! `docs/internal/gc/gengc-r4-oldgen-backing-store-is-committed-up-front-and-never-returned-FIXED-20260923.md`:
//! the old generation used to be one `Vec<u8>` of half of `-Xmx`, which took
//! the whole commit charge at startup on Windows and which `-Xms` could not
//! lower. Each test here has two right answers, because
//! `CRATONVM_GC_RESERVE=0` (or a refused reservation) legitimately puts the
//! generation back on a wholly committed block; the reserving answer is the
//! one this lane changed.
//!
//! See `docs/internal/reviews/gengc-round4-w2-oldgen2-20260923.md`.

use cratonvm_gc::GenerationalHeap;
use cratonvm_types::ClassId;

const MIB: usize = 1024 * 1024;

/// `(old committed, old capacity)`.
fn old_commit(heap: &GenerationalHeap) -> (usize, usize) {
    (heap.old_gen_trigger_stats().1, heap.old_gen_capacity())
}

#[test]
fn a_fresh_heap_does_not_commit_its_old_generation() {
    let heap = GenerationalHeap::with_capacity(64 * MIB);
    let (committed, capacity) = old_commit(&heap);
    assert_eq!(capacity, 32 * MIB, "the 50/50 split is unchanged");
    if committed == capacity {
        // Wholly committed fallback: the historical behaviour.
        return;
    }
    assert!(
        committed < capacity,
        "a reserving old generation must not be committed whole at startup \
         ({committed} of {capacity})",
    );
    assert!(
        heap.os_committed_bytes() < heap.committed_heap_bytes(),
        "os_committed_bytes must report the old generation's committed bytes, \
         not its capacity",
    );
}

#[test]
fn an_xms_above_the_young_pair_commits_an_old_generation_prefix() {
    let heap = GenerationalHeap::with_capacity(64 * MIB);
    // Young semis are 16 MiB apiece; 48 MiB leaves 16 MiB for the old gen.
    heap.commit_initial_heap(48 * MIB);
    let (committed, _) = old_commit(&heap);
    assert!(
        committed >= 16 * MIB,
        "-Xms beyond what the young pair can absorb must be committed in the \
         old generation, or the startup commit falls short of -Xms \
         (old committed {committed})",
    );
    assert!(heap.os_committed_bytes() >= 48 * MIB);
}

#[test]
fn objects_allocated_directly_in_old_gen_are_inside_it_and_commit_on_demand() {
    let heap = GenerationalHeap::with_capacity(64 * MIB);
    let (before, capacity) = old_commit(&heap);
    let objs = heap.try_alloc_objects_old_batch(ClassId::new(1), 2, 64);
    assert_eq!(objs.len(), 64, "an empty old generation has room for 64 objects");
    for o in &objs {
        assert!(
            heap.is_in_old(o.as_ptr()),
            "an object the old generation handed out must be inside it",
        );
    }
    let (after, _) = old_commit(&heap);
    if before < capacity {
        assert!(after > before, "the first old-gen allocation commits a granule");
        assert!(after < capacity, "...and only what it needs");
    }
    let stats = heap.old_gen_trigger_stats().0;
    assert_eq!(stats.commit_refusals, 0);
    assert_eq!(stats.alloc_failures, 0);
}
