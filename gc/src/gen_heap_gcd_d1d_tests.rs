// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcd d1/d (2026-09-27) — `gen_heap.rs` functions of lane d's region, tested
//! here (a file of its own) so parallel lanes do not meet at the end of
//! `gen_heap.rs`.
//!
//! gcg g5/og (2026-10-02): this file used to hold the O(live) stop-the-world
//! old-gen collection (`CRATONVM_GC_OLD_LIVE_SWEEP`, gen r5w5/old9, with
//! `CRATONVM_GC_OLD_LIVE_KEPT_BITMAP`, `CRATONVM_GC_OLD_LIVE_RESURRECT`,
//! `CRATONVM_GC_OLD_LIVE_TRUE_ROOTS` and `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY`)
//! and its tests. It was deleted: it lost to the walked collection on every
//! measured shape (`docs/feature-designs/generative-gc-architecture.md`).
//! Only the refill and conservative young-word seed tests remain; their file
//! is named for their test module rather than the deleted collector (gcj O8).

use super::*;

const KIB: usize = 1024;

/// `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object`, clamp 2: a
/// young generation whose only free span is an 8 KiB bump tail cannot hold
/// a 20 KiB object, so a refill for it declines WITHOUT carving; a floor
/// the tail can hold is served from the tail; the unfloored refill still
/// carves the tail, byte for byte as before.
#[test]
fn a_refill_that_cannot_hold_the_object_declines_without_carving() {
    let heap = GenerationalHeap::with_sizes(256 * KIB, 256 * KIB);
    let (cap, used) = {
        let f = heap.lock_young_from();
        (f.capacity(), f.used())
    };
    let fill = (cap - used - 8 * KIB) & !7;
    let (_, got) = heap.refill_tlab(fill).expect("room for the fill");
    assert_eq!(got, fill);
    let tail = {
        let f = heap.lock_young_from();
        f.capacity() - f.used()
    };
    assert!((8 * KIB..20 * KIB).contains(&tail), "tail {tail}");
    let used_before = heap.lock_young_from().used();
    assert!(
        heap.refill_tlab_at_least(64 * KIB, 20 * KIB).is_none(),
        "no span can hold a 20 KiB object"
    );
    assert_eq!(
        heap.lock_young_from().used(),
        used_before,
        "nothing was carved"
    );
    let (_, small) = heap
        .refill_tlab_at_least(64 * KIB, 4 * KIB)
        .expect("the tail holds 4 KiB");
    assert_eq!(small, tail & !7, "served from the tail");
    assert!(heap.refill_tlab(64 * KIB).is_none(), "young is now full");
}

/// `gengc-r4w6-review6-old-pinned-compaction-residuals`, "Also noted" 1: an
/// old-gen object named only by an INTERIOR word of a young stretch the
/// walk cannot parse is pinned AND marked and pushed for the BFS when a
/// pinned compaction is planned (so its side-table edges are traced like
/// any marked object's); without a plan, nothing changes (not marked).
#[test]
fn an_interior_word_in_an_unparseable_young_stretch_is_marked_for_the_bfs() {
    let heap = GenerationalHeap::with_sizes(64 * KIB, 64 * KIB);
    let x = {
        let mut og = heap.old_gen.lock();
        let p = og
            .alloc(HEADER_SIZE + SLOT_SIZE * 4, 8)
            .expect("room in old gen");
        // SAFETY: a fresh, zeroed block sized for a header and 4 slots.
        unsafe { (*(p as *mut ObjectHeader)).set_num_slots(4) };
        p as usize
    };
    // The young stretch: an object whose header claims far more than
    // from-space holds (the walk cannot parse it), carrying `x + 8`.
    let tail = heap.alloc_object(ClassId::new(0), 8);
    // SAFETY: `tail` is a live young object of 8 legacy slots; its first
    // slot word and its header are inside it.
    unsafe {
        std::ptr::write(
            (tail.as_ptr() as usize + HEADER_SIZE) as *mut u64,
            (x + 8) as u64,
        );
        (*(tail.as_ptr() as *mut ObjectHeader)).set_num_slots(1 << 20);
    }
    let young = heap.lock_young_from();
    let og = heap.old_gen.lock();
    let walked = og.walk_objects();
    assert!(walked.iter().any(|&(p, _)| p as usize == x), "x is walked");
    // Re-derived at each use: the marker writes the header through its
    // own `&mut` in between.
    // SAFETY: `x` is the old-gen object carved above, alive for the test.
    let marked = || unsafe { &*(x as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;

    let mut plain = Vec::new();
    GenerationalHeap::mark_young_to_old_refs(&young, &og, &walked[..], &[], &mut plain, None);
    assert!(plain.is_empty(), "no plan: an interior word marks nothing");
    assert!(!marked());

    let mut worklist = Vec::new();
    let mut pins = Vec::new();
    GenerationalHeap::mark_young_to_old_refs(
        &young,
        &og,
        &walked[..],
        &[],
        &mut worklist,
        Some(&mut pins),
    );
    assert!(
        pins.contains(&x) && pins.iter().all(|&p| p == x),
        "pins {pins:?}"
    );
    assert_eq!(
        worklist.iter().filter(|&&p| p as usize == x).count(),
        1,
        "pushed once for the BFS"
    );
    assert!(marked(), "marked");
    // SAFETY: as above.
    unsafe { &*(x as *const ObjectHeader) }.clear_gc_flags(GC_FLAG_MARKED);
}
