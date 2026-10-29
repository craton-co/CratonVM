// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcf f2/c (2026-10-01) — unit tests of lane c's wave-2 changes to the
//! stop-the-world old-gen collection:
//!
//! * `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` (opt-in): a requested major on the
//!   non-moving path compacts the old generation
//!   (`gcf-f1c-stw-full-gc-never-compacts-old-gen-by-default-20261001`);
//! * the walked mark's admission oracle as a base bitmap
//!   (`gcf-f1c-proposal-stw-major-admits-marks-through-the-bot-not-a-binary-search-20261001`;
//!   its switch `CRATONVM_GC_OLD_MARK_ADMISSION` and the block-offset arm were
//!   deleted by gcg g4/og, the bitmap is `CRATONVM_GC_OLD_SHARED_STARTS`'s).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcf_f2c_`.

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

// ---------------------------------------------------------------------------
// CRATONVM_GC_REQUESTED_MAJOR_COMPACTS
// ---------------------------------------------------------------------------

/// The flags a requested-major test pins: the switch as given, the true-root
/// seed on and wide (the defaults), `System.gc()` on the non-moving cycle.
fn requested_flags(
    on: Option<&'static str>,
    wide: Option<&'static str>,
) -> [(&'static str, Option<&'static str>); 4] {
    [
        ("CRATONVM_GC_REQUESTED_MAJOR_COMPACTS", on),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS", None),
        ("CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE", wide),
        ("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG", None),
    ]
}

/// The decision: only a requested major, only with the switch, never on top
/// of a fragmentation request (which plans the same compaction already), and
/// never under a NARROW true-root seed (the plan would put the major back on
/// the legacy seed). The switch is not even read for an unrequested major.
#[test]
fn gcf_f2c_requested_major_compaction_decision_table() {
    let read = std::cell::Cell::new(0u32);
    let on = || {
        read.set(read.get() + 1);
        true
    };
    // (frag, requested, armed, wide) -> wanted, with the switch on.
    assert!(requested_major_compaction_wanted(
        false, true, true, true, on
    ));
    assert!(requested_major_compaction_wanted(
        false, true, false, false, on
    ));
    assert!(
        !requested_major_compaction_wanted(false, true, true, false, on),
        "narrow seed"
    );
    assert!(
        !requested_major_compaction_wanted(true, true, true, true, on),
        "frag plans it"
    );
    let before = read.get();
    assert!(!requested_major_compaction_wanted(
        false, false, false, false, on
    ));
    assert_eq!(
        read.get(),
        before,
        "an unrequested major does not read the switch"
    );
    assert!(
        !requested_major_compaction_wanted(false, true, true, true, || false),
        "switch off"
    );
}

/// `dead` (64 slots) lies below `keep`, the one root. A requested major with
/// the switch on plans the compaction (the injected plan stands in for
/// `OldPinnedCompact::for_this_pause`, whose gates read process state other
/// tests may set) and `keep` slides down over `dead`; with the switch off the
/// plan is never asked for and `dead` is freed in place. Both free exactly
/// `dead`.
#[test]
fn gcf_f2c_a_requested_major_compacts_old_gen_only_under_the_switch() {
    for on in [true, false] {
        let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
        let (dead, keep) = {
            let mut og = heap.old_gen.lock();
            let dead = old_obj(&mut og, 64);
            let keep = old_obj(&mut og, 2);
            (dead, keep)
        };
        assert!(
            dead < keep,
            "precondition: the dead block lies below the live one"
        );
        let asked = std::cell::Cell::new(false);
        // gci i8/rv: the off arm is `=0` since the switch went default ON
        // (gci-user-flip-20261013); unset now reads ON.
        let flags = requested_flags(if on { Some("1") } else { Some("0") }, None);
        let (_reclaimed, map) = cratonvm_types::flags::with_thread_overrides(&flags, || {
            let _armed = TrueRootMajorScope::arm_requested(true, true);
            heap.sweep_old_gen_non_moving_with(
                // SAFETY: `keep` is a live old-gen object base.
                &[unsafe { ObjectRef::from_raw(keep as *mut u8) }],
                &cratonvm_types::PointerMap::default(),
                |n| {
                    asked.set(true);
                    assert_eq!(n, 1, "the caller's roots");
                    Some(Ok(OldPinnedCompact::new(Vec::new(), 0)))
                },
            )
        });
        let s = heap.old_gen_sizing_stats();
        let keep_now = map.get(&keep).copied().unwrap_or(keep);
        let og = heap.old_gen.lock();
        assert_eq!(og.used(), footprint(2), "on={on}: exactly `dead` is freed");
        assert!(
            og.is_allocated_addr(keep_now as *const u8),
            "on={on}: keep survives"
        );
        if on {
            assert!(asked.get(), "the requested major asked for a plan");
            assert_eq!(
                (s.pinned_compactions, s.requested_major_compactions),
                (1, 1),
                "{s:?}"
            );
            assert!(keep_now < keep, "keep slid down over the dead block");
        } else {
            assert!(!asked.get(), "no plan without the switch");
            assert_eq!(
                (s.pinned_compactions, s.requested_major_compactions),
                (0, 0),
                "{s:?}"
            );
            assert_eq!(keep_now, keep, "in place");
        }
        assert_eq!(
            s.true_root_majors, 1,
            "the true-root seed ran on both arms: {s:?}"
        );
    }
}

/// An UNREQUESTED major (occupancy) does not compact under the switch, and a
/// requested one under a narrow true-root seed does not either.
#[test]
fn gcf_f2c_the_switch_leaves_unrequested_and_narrow_majors_in_place() {
    for (requested, wide) in [(false, None), (true, Some("0"))] {
        let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
        let keep = {
            let mut og = heap.old_gen.lock();
            let _dead = old_obj(&mut og, 64);
            old_obj(&mut og, 2)
        };
        let asked = std::cell::Cell::new(false);
        let flags = requested_flags(Some("1"), wide);
        let (_reclaimed, map) = cratonvm_types::flags::with_thread_overrides(&flags, || {
            let _armed = TrueRootMajorScope::arm_requested(requested, true);
            heap.sweep_old_gen_non_moving_with(
                // SAFETY: `keep` is a live old-gen object base.
                &[unsafe { ObjectRef::from_raw(keep as *mut u8) }],
                &cratonvm_types::PointerMap::default(),
                |_| {
                    asked.set(true);
                    Some(Ok(OldPinnedCompact::new(Vec::new(), 0)))
                },
            )
        });
        assert!(!asked.get(), "requested={requested} wide={wide:?}: no plan");
        assert_eq!(map.get(&keep).copied().unwrap_or(keep), keep);
        let s = heap.old_gen_sizing_stats();
        assert_eq!(
            (s.pinned_compactions, s.requested_major_compactions),
            (0, 0),
            "{s:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The in-place free loop's watched cursor
// ---------------------------------------------------------------------------

/// `AscendingMembership` answers ascending queries exactly as the set's
/// `contains`, for the members its filter keeps, and is empty without a set.
#[test]
fn gcf_f2c_ascending_membership_answers_as_the_set() {
    // gci i21/ws: the sweeps' watched set is a `WatchedSnapshot` now.
    let set: crate::gc_quiescence::WatchedSnapshot = [0x10usize, 0x48, 0x50, 0x400, 0x9000, 0x9008]
        .into_iter()
        .collect();
    let keep = |a: usize| a < 0x9000;
    let mut m = AscendingMembership::new(Some(&set), keep);
    for a in (0..0x9100usize).step_by(8) {
        assert_eq!(m.contains(a), set.contains(&a) && keep(a), "{a:#x}");
    }
    let mut none = AscendingMembership::new(None, |_| true);
    assert!((0..64usize).all(|a| !none.contains(a)));
}

/// The free loop's flags and survival proofs through the cursor: a watched
/// survivor gets its identity entry and a watched dead object is freed (the
/// in-place arm's two uses of the watched set).
#[test]
fn gcf_f2c_the_in_place_sweep_proves_watched_survivors_through_the_cursor() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let young = heap.lock_young_from();
    let mut og = heap.old_gen.lock();
    let keep = old_obj(&mut og, 2);
    let dead = old_obj(&mut og, 3);
    let kept_child = old_obj(&mut og, 1);
    set(keep, 0, kept_child);
    // SAFETY: `keep` is a live old-gen object base.
    let mut roots = [unsafe { ObjectRef::from_raw(keep as *mut u8) }];
    // Thread-local: set for this collection, cleared after it.
    crate::gc_quiescence::set_watched_referents(&[keep, dead, kept_child, 0x8]);
    let map = GenerationalHeap::old_gen_gc(&mut roots, &young, &mut og, false, &[]);
    crate::gc_quiescence::set_watched_referents(&[]);
    assert_eq!(
        map.get(&keep),
        Some(&keep),
        "a watched survivor is proved alive"
    );
    assert_eq!(map.get(&kept_child), Some(&kept_child));
    assert!(!map.contains_key(&dead), "a watched dead object is absent");
    assert!(!og.is_allocated_addr(dead as *const u8));
}

// ---------------------------------------------------------------------------
// The mark admission (gcf f2/c). gcg g4/og deleted its switch (triage tg1:
// no gain) and its `bot` arm; the `bitmap` arm lives on as the shared start
// set (`CRATONVM_GC_OLD_SHARED_STARTS`), which these tests now drive.
// ---------------------------------------------------------------------------

#[test]
fn gcf_f2c_the_base_bitmap_refuses_a_grid_it_cannot_represent() {
    let p = |a: usize| (a as *mut u8, 16usize);
    assert!(WalkedStarts::build(&[]).is_none(), "empty");
    assert!(
        WalkedStarts::build(&[p(0x1000), p(0x1004)]).is_none(),
        "unaligned"
    );
    assert!(
        WalkedStarts::build(&[p(0x1010), p(0x1000)]).is_none(),
        "descending"
    );
    assert!(
        WalkedStarts::build(&[p(0x1000), p(0x1000)]).is_none(),
        "duplicate"
    );
    assert!(
        WalkedStarts::build(&[p(0x1000), p(0x1020), p(0x1018)]).is_none(),
        "not ascending"
    );
    let bases = [0x1000usize, 0x1010, 0x2008];
    let s = WalkedStarts::build(&bases.map(p)).expect("a sane grid");
    for a in 0xff0..0x2020 {
        assert_eq!(s.has(a), bases.contains(&a), "{a:#x}");
    }
}

/// The equivalence the start set rests on: over every byte address of a
/// generation with holes (and a margin on both sides), the bitmap admission
/// (and the admission without a start set) answer `has_base`,
/// `interior_base` and `authoritative_base` exactly as the walked grid does.
#[test]
fn gcf_f2c_mark_admission_answers_as_the_walked_grid() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 512 * 1024);
    let mut og = heap.old_gen.lock();
    let mut objs: Vec<(usize, usize)> = Vec::new();
    for i in 0..300u32 {
        let slots = 1 + (i * 7) % 40;
        objs.push((old_obj(&mut og, slots), footprint(slots as usize)));
    }
    // Holes: every third object, and a run of three.
    for (i, &(p, size)) in objs.iter().enumerate() {
        if i % 3 == 1 || (100..103).contains(&i) {
            // SAFETY: `p` is a block of `size` bytes `old_obj` carved, freed once.
            unsafe { og.free(p as *mut u8, size) };
        }
    }
    let (walked, gaps) = og.walk_objects_with_gaps();
    assert!(gaps.is_empty(), "a sane heap walks without a gap");
    assert!(
        walked.len() > 150,
        "precondition: most objects are live ({})",
        walked.len()
    );
    let grid: &[(*mut u8, usize)] = &walked;
    let starts = WalkedStarts::build(grid).expect("a sane grid");
    let bitmap = MarkAdmission {
        walked: grid,
        starts: Some(&starts),
    };
    let plain = MarkAdmission {
        walked: grid,
        starts: None,
    };
    let lo = og.base_ptr() as usize;
    let hi = lo + og.high_water();
    for addr in lo.saturating_sub(64)..hi + 64 {
        let want = (
            grid.has_base(addr),
            grid.interior_base(addr),
            grid.authoritative_base(addr),
        );
        let got = (
            bitmap.has_base(addr),
            bitmap.interior_base(addr),
            bitmap.authoritative_base(addr),
        );
        assert_eq!(got, want, "bitmap at {addr:#x}");
        let got = (
            plain.has_base(addr),
            plain.interior_base(addr),
            plain.authoritative_base(addr),
        );
        assert_eq!(got, want, "no start set at {addr:#x}");
    }
}

/// End to end: a walked in-place collection admits through the grid and
/// through the shared start set and frees exactly the same blocks.
/// `keep -> a -> b` is live; `d -> e -> a` is dead (a dead object naming a
/// live one).
#[test]
fn gcf_f2c_a_major_through_each_admission_frees_the_same_blocks() {
    // gcg g6/og: the start set is default ON, so the grid arm is `=0`.
    for mode in [Some("0"), Some("1")] {
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
        let flags = [("CRATONVM_GC_OLD_SHARED_STARTS", mode)];
        let _ = cratonvm_types::flags::with_thread_overrides(&flags, || {
            GenerationalHeap::old_gen_gc(&mut roots, &young, &mut og, false, &[])
        });
        assert_eq!(
            og.used(),
            footprint(2) + footprint(3) + footprint(1),
            "mode {mode:?}"
        );
        for p in [keep, a, b] {
            assert!(
                og.is_allocated_addr(p as *const u8),
                "mode {mode:?}: {p:#x} is live"
            );
        }
        for p in [d, e] {
            assert!(
                !og.is_allocated_addr(p as *const u8),
                "mode {mode:?}: {p:#x} is dead"
            );
        }
        let s = og.sizing_stats();
        assert_eq!(
            s.mark_admission_bitmaps,
            u64::from(mode == Some("1")),
            "mode {mode:?}: the start set counts"
        );
    }
}

// ---------------------------------------------------------------------------
// gcf f4/c: CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS
// ---------------------------------------------------------------------------

/// The armed bits: the requested bit is the request's alone; the true-root
/// bit needs the seed on and either the request or the all-majors switch.
#[test]
fn gcf_f4c_true_root_major_bits_table() {
    let b = true_root_major_bits;
    assert_eq!(
        b(false, true, false),
        0,
        "occupancy major, switch off: legacy"
    );
    assert_eq!(
        b(false, true, true),
        ARMED_TRUE_ROOTS,
        "occupancy major, switch on"
    );
    assert_eq!(b(false, false, true), 0, "the seed itself off");
    assert_eq!(b(true, true, false), ARMED_REQUESTED | ARMED_TRUE_ROOTS);
    assert_eq!(b(true, false, true), ARMED_REQUESTED);
}

/// gch h3/mj: `CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS` is a parity fix and
/// default ON (vh3b `mj3b_cf_def_jit_*` STALE 3/3, `mj3b_cf_am_jit` ok); `=0`
/// restores the legacy seed on an unrequested non-moving major.
#[test]
fn gch_h3mj_all_majors_true_roots_is_default_on_and_0_kills_it() {
    let read = |v: Option<&'static str>| {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_FULL_GC_TRUE_ROOTS_ALL_MAJORS", v)],
            full_gc_true_roots_all_majors_enabled,
        )
    };
    assert!(read(None), "unset: on");
    assert!(read(Some("1")));
    assert!(!read(Some("0")), "=0 kills");
}

/// An OCCUPANCY major (not requested) runs the true-root seed only under the
/// switch, and the switch never makes it a requested major (no compaction
/// plan is asked for under `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`).
#[test]
fn gcf_f4c_an_occupancy_major_takes_the_true_root_seed_only_under_the_switch() {
    for all in [false, true] {
        let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
        let keep = {
            let mut og = heap.old_gen.lock();
            let _dead = old_obj(&mut og, 64);
            old_obj(&mut og, 2)
        };
        let asked = std::cell::Cell::new(false);
        let flags = requested_flags(Some("1"), None);
        let _ = cratonvm_types::flags::with_thread_overrides(&flags, || {
            let _armed = TrueRootMajorScope::arm_major(false, true, all);
            heap.sweep_old_gen_non_moving_with(
                // SAFETY: `keep` is a live old-gen object base.
                &[unsafe { ObjectRef::from_raw(keep as *mut u8) }],
                &cratonvm_types::PointerMap::default(),
                |_| {
                    asked.set(true);
                    Some(Ok(OldPinnedCompact::new(Vec::new(), 0)))
                },
            )
        });
        let s = heap.old_gen_sizing_stats();
        assert_eq!(s.true_root_majors, u64::from(all), "all={all}: {s:?}");
        assert!(
            !asked.get(),
            "all={all}: an occupancy major plans no compaction"
        );
        assert_eq!(
            heap.old_gen.lock().used(),
            footprint(2),
            "all={all}: the dead block is freed"
        );
    }
}

// ---------------------------------------------------------------------------
// gcf f4/c: CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY
// ---------------------------------------------------------------------------

/// A word is unpinned iff it equals a movable object address that no
/// unrewritable word also names; interior / other words stay pins.
#[test]
fn gcf_f4c_movable_words_are_unpinned_unless_vetoed() {
    let movable = |a: usize| a == 0x1000 || a == 0x2000;
    let unrewritable = |a: usize| a == 0x2000;
    let (pins, dropped) =
        movable_words_unpinned(vec![0x1000, 0x1008, 0x2000, 0x3000], movable, unrewritable);
    assert_eq!(
        pins,
        vec![0x1008, 0x2000, 0x3000],
        "0x1000 movable; 0x2000 vetoed"
    );
    assert_eq!(dropped, 1);
    let (pins, dropped) = movable_words_unpinned(vec![0x1000], |_| false, |_| false);
    assert_eq!(
        (pins, dropped),
        (vec![0x1000], 0),
        "nothing movable: every word pins"
    );
}

/// The plan's words decide the slide: `dead` below `keep`, a fragmentation
/// request armed. With `keep`'s base as a pinned word it stays put; with the
/// word filtered out (a movable word) it slides over the hole and the map
/// reports the move -- the map the VM's `remap_active_jit_frames` then applies
/// to the oop-map slot that published it.
#[test]
fn gcf_f4c_an_unpinned_movable_word_lets_its_object_slide() {
    for movable in [false, true] {
        let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
        let keep = {
            let mut og = heap.old_gen.lock();
            let _dead = old_obj(&mut og, 64);
            let keep = old_obj(&mut og, 2);
            og.request_fragmentation_compaction();
            keep
        };
        let (words, dropped) =
            movable_words_unpinned(vec![keep], |a| movable && a == keep, |_| false);
        assert_eq!(dropped, usize::from(movable));
        let (_reclaimed, map) = heap.sweep_old_gen_non_moving_with(
            // SAFETY: `keep` is a live old-gen object base.
            &[unsafe { ObjectRef::from_raw(keep as *mut u8) }],
            &cratonvm_types::PointerMap::default(),
            move |_| Some(Ok(OldPinnedCompact::with_range(words, (0, 0)))),
        );
        let keep_now = map.get(&keep).copied().unwrap_or(keep);
        let s = heap.old_gen_sizing_stats();
        assert_eq!(s.pinned_compactions, 1, "movable={movable}: {s:?}");
        if movable {
            assert!(keep_now < keep, "an unpinned object slides over the hole");
        } else {
            assert_eq!(keep_now, keep, "a pinned object stays");
        }
        assert_eq!(
            heap.old_gen.lock().used(),
            footprint(2),
            "movable={movable}"
        );
    }
}
