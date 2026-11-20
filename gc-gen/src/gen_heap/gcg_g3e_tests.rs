// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gcg g3/e (2026-10-01) — unit tests of lane e's wave-3 changes to the
//! stop-the-world old-gen collection:
//!
//! * `CRATONVM_GC_OLD_SHARED_STARTS` (default ON; `=0` kills): one start set per
//!   collection, shared by the BFS admission and every closure call
//!   (`gcg-g2e-proposal-one-walked-start-bitmap-per-major-20261001`);
//! * the unrewritable-only plan's per-thread veto
//!   (`gcg-g3e-unrewritable-only-plan-unpins-words-other-threads-published-FIXED-20261012`).
//!
//! The old live-sweep true-root tests were deleted with that collector. The
//! remaining refill and conservative seed tests are in
//! `gen_heap_gcd_d1d_tests.rs`.
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
/// The shared set changes no closure verdict: a block only a DEAD object
/// names is still freed, and a mark gap (a live object's referent left
/// unmarked by hand, as a push-site gap would) is still rescued by the sweep's
/// closure, with the shared set as with the binary search.
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
