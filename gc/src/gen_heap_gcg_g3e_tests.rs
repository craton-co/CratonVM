// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcg g3/e (2026-10-01) — unit tests of lane e's wave-3 changes to the
//! stop-the-world old-gen collection:
//!
//! * `CRATONVM_GC_OLD_SHARED_STARTS` (opt-in): one walked start set per
//!   collection, shared by the BFS admission and every closure call
//!   (`gcg-g2e-proposal-one-walked-start-bitmap-per-major-20261001`);
//! * the unrewritable-only plan's per-thread veto
//!   (`gcg-g3e-unrewritable-only-plan-unpins-words-other-threads-published-FIXED-20261012`).
//!
//! The live sweep's true-root arm is tested beside the live sweep
//! (`gen_heap_live_sweep.rs`, `gcg_g3e_*`).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcg_g3e_`.

use super::*;

/// A legacy old-gen object of `slots` slots with a real header (the walk
/// strides by headers).
fn old_obj(og: &mut OldGen, slots: u32) -> usize {
    let p = og
        .alloc(HEADER_SIZE + SLOT_SIZE * slots as usize, 8)
        .expect("room in old gen");
    // SAFETY: a fresh, zeroed block sized for a header and `slots` slots.
    unsafe { (*(p as *mut ObjectHeader)).set_num_slots(slots) };
    p as usize
}

/// Store `target` into legacy slot `slot` of `holder`.
fn set(holder: usize, slot: usize, target: usize) {
    // SAFETY: `holder` is a live legacy object with more than `slot` slots,
    // and `target` is a non-null, 8-aligned object base.
    unsafe {
        std::ptr::write(
            (holder + HEADER_SIZE + slot * SLOT_SIZE) as *mut Value,
            Value::Object(Some(ObjectRef::from_raw(target as *mut u8))),
        );
    }
}

fn footprint(slots: usize) -> usize {
    HEADER_SIZE + slots * SLOT_SIZE
}

/// End to end: a walked in-place collection with the shared start set (alone,
/// and beside the per-call closure bitmap it supersedes) frees exactly the
/// blocks the default collection frees. `keep -> a -> b` is live; `d -> e ->
/// a` is dead (a dead object naming a live one). The BFS admits through the
/// shared set whenever it is on (`mark_admission_bitmaps`); gcg g4/og deleted
/// the `CRATONVM_GC_OLD_MARK_ADMISSION` arms this test also crossed.
#[test]
fn gcg_g3e_a_major_through_the_shared_start_set_frees_the_same_blocks() {
    let arms: [(Option<&'static str>, Option<&'static str>); 3] =
        // gcg g6/og: the start set is default ON, so its off arm is `=0`.
        [(Some("0"), None), (Some("1"), None), (Some("1"), Some("1"))];
    for (shared, close_bitmap) in arms {
        let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
        let young = heap.lock_young_from();
        let mut og = heap.old_gen.lock();
        let keep = old_obj(&mut og, 2);
        let a = old_obj(&mut og, 3);
        let d = old_obj(&mut og, 5);
        let b = old_obj(&mut og, 1);
        let e = old_obj(&mut og, 2);
        set(keep, 0, a);
        set(a, 1, b);
        set(d, 0, e);
        set(e, 0, a);
        // SAFETY: `keep` is a live old-gen object base.
        let mut roots = [unsafe { ObjectRef::from_raw(keep as *mut u8) }];
        let flags = [
            ("CRATONVM_GC_OLD_SHARED_STARTS", shared),
            ("CRATONVM_GC_OLD_CLOSE_BITMAP", close_bitmap),
        ];
        let _ = cratonvm_types::flags::with_thread_overrides(&flags, || {
            GenerationalHeap::old_gen_gc(&mut roots, &young, &mut og, false, &[])
        });
        let arm = format!("shared={shared:?} close_bitmap={close_bitmap:?}");
        assert_eq!(
            og.used(),
            footprint(2) + footprint(3) + footprint(1),
            "{arm}"
        );
        for p in [keep, a, b] {
            assert!(
                og.is_allocated_addr(p as *const u8),
                "{arm}: {p:#x} is live"
            );
            // SAFETY: a kept object base.
            let m = unsafe { &*(p as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED;
            assert_eq!(m, 0, "{arm}: the sweep cleared every kept mark");
        }
        for p in [d, e] {
            assert!(
                !og.is_allocated_addr(p as *const u8),
                "{arm}: {p:#x} is dead"
            );
        }
        let s = og.sizing_stats();
        let want_bitmap = u64::from(shared == Some("1"));
        assert_eq!(s.mark_admission_bitmaps, want_bitmap, "{arm}: {s:?}");
    }
}

/// The shared set changes no closure verdict: a block only a DEAD object
/// names is still freed, and a mark gap (a live object's referent left
/// unmarked by hand, as a push-site gap would) is still rescued by the sweep's
/// closure, with the shared set as with the binary search.
#[test]
fn gcg_g3e_the_sweeps_closure_rescues_through_the_shared_set() {
    for shared in [Some("0"), Some("1")] {
        let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
        let young = heap.lock_young_from();
        let mut og = heap.old_gen.lock();
        let keep = old_obj(&mut og, 2);
        let hidden = old_obj(&mut og, 1);
        let dead = old_obj(&mut og, 4);
        // `keep -> hidden`. The gap is made by giving `keep` a STALE mark
        // beforehand: the root loop finds it marked, does not push it, and
        // the BFS never scans it, so only the sweep's closure (a linear pass
        // over every marked walked object) finds `hidden`.
        set(keep, 0, hidden);
        set(dead, 0, hidden);
        // SAFETY: an old-gen object base of this heap.
        unsafe { &*(keep as *const ObjectHeader) }.add_gc_flags(GC_FLAG_MARKED);
        // SAFETY: `keep` is a live old-gen object base.
        let mut roots = [unsafe { ObjectRef::from_raw(keep as *mut u8) }];
        let flags = [
            ("CRATONVM_GC_OLD_SHARED_STARTS", shared),
            ("CRATONVM_GC_OLD_CLOSE_BITMAP", None),
        ];
        let _ = cratonvm_types::flags::with_thread_overrides(&flags, || {
            GenerationalHeap::old_gen_gc(&mut roots, &young, &mut og, false, &[])
        });
        assert!(
            og.is_allocated_addr(keep as *const u8),
            "shared={shared:?}: keep"
        );
        assert!(
            og.is_allocated_addr(hidden as *const u8),
            "shared={shared:?}: the closure rescues what a marked object names",
        );
        assert!(
            !og.is_allocated_addr(dead as *const u8),
            "shared={shared:?}: dead is freed"
        );
        assert_eq!(og.used(), footprint(2) + footprint(1), "shared={shared:?}");
    }
}

/// gcg g3/e — the unrewritable-only plan drops only the CALLING thread's
/// movable words. A word another thread published (its deposit, a helper
/// window) stays a pin even when the caller's movable set names the same
/// object: the movable / unrewritable sets are the caller's thread-locals,
/// so they cannot speak for the other thread's word.
#[test]
fn gcg_g3e_unrewritable_only_keeps_every_word_another_thread_published() {
    let movable = |a: usize| a == 0x1000 || a == 0x2000 || a == 0x3000;
    let unrewritable = |a: usize| a == 0x3000;
    // Own: 0x1000 (movable, dropped), 0x3000 (vetoed), 0x4000 (no claim).
    // Others: 0x2000 (movable on the CALLER, but published by a peer).
    let (pins, dropped) = movable_words_unpinned_by_thread(
        vec![0x1000, 0x3000, 0x4000],
        vec![0x2000],
        movable,
        unrewritable,
    );
    assert_eq!(dropped, 1, "only the caller's own movable word is dropped");
    let mut sorted = pins.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![0x2000, 0x3000, 0x4000]);
    // The caller ALSO published 0x2000: the peer's copy still pins it.
    let (pins, dropped) =
        movable_words_unpinned_by_thread(vec![0x2000, 0x1000], vec![0x2000], movable, unrewritable);
    assert_eq!(dropped, 1, "0x1000 only");
    assert!(
        pins.contains(&0x2000),
        "a peer's word keeps its object pinned"
    );
    assert!(!pins.contains(&0x1000));
    // No peer at all: exactly the gcf f4/c rule.
    let (pins_t, dropped_t) = movable_words_unpinned_by_thread(
        vec![0x1000, 0x2000, 0x3000],
        Vec::new(),
        movable,
        unrewritable,
    );
    let (pins_f, dropped_f) =
        movable_words_unpinned(vec![0x1000, 0x2000, 0x3000], movable, unrewritable);
    assert_eq!((pins_t, dropped_t), (pins_f, dropped_f));
}
