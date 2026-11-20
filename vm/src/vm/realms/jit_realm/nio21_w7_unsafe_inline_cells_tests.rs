use super::UnsafeInlineCells;
use cratonvm_jit::direct_helpers::{unsafe_field_cell_index, unsafe_field_cell_word};

/// Nothing is published until a compile arms the table; once armed, a
/// cell is published once, a colliding field does not evict it, and a
/// shape that does not fit a word publishes nothing.
#[test]
fn nio21_w7_cells_publish_once_armed_and_keep_the_first_owner() {
    let cells = UnsafeInlineCells::new();
    let epoch = cratonvm_types::layout_replace_epoch();
    assert!(cells.epoch_if_armed().is_none());
    if cells.base_addr_for_compile() == 0 {
        return; // not x86-64, or a kill switch is set
    }
    assert!(cells.is_armed());
    assert_eq!(cells.epoch_if_armed(), Some(epoch));
    let i = unsafe_field_cell_index(40, 3, false);
    cells.note_compact_field(40, 3, false, 12, epoch);
    let first = cells.snapshot(i);
    // A different (class, key) with the same index keeps the first owner.
    // `40 ^ 6 == 46 ^ 0`: class 46, slot 0, int.
    assert_eq!(unsafe_field_cell_index(46, 0, false), i);
    cells.note_compact_field(46, 0, false, 8, epoch);
    let after_collision = cells.snapshot(i).0;
    // A slot past the key: nothing.
    let j = unsafe_field_cell_index(41, 0x8000, false);
    cells.note_compact_field(41, 0x8000, false, 12, epoch);
    let unfit = cells.snapshot(j).0;
    // A stale epoch never publishes.
    let k = unsafe_field_cell_index(77, 1, true);
    cells.note_compact_field(77, 1, true, 16, epoch.wrapping_sub(1));
    let stale = cells.snapshot(k).0;
    // Another test in this process may replace a layout (a class unload
    // does) and move the epoch under us; then the refusals above were
    // the right answers for a different reason, and nothing is pinned.
    if cratonvm_types::layout_replace_epoch() != epoch {
        return;
    }
    let want = unsafe_field_cell_word(40, 3, false, 12).unwrap();
    assert_eq!(first, (want, u64::from(epoch)));
    assert_eq!(after_collision, want);
    assert_eq!(unfit, 0);
    assert_eq!(stale, 0);
}


/// JIT round 20 wave 12, lane qUnsafe (AT-4): the reference half publishes
/// only once armed, into its own words, never into the int/long half.
#[test]
fn r20_qu_reference_cells_publish_into_their_own_half_once_armed() {
    use cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT;
    let cells = UnsafeInlineCells::new();
    let epoch = cratonvm_types::layout_replace_epoch();
    assert!(cells.ref_epoch_if_armed().is_none());
    let i = unsafe_field_cell_index(52, 2, true);
    // Baking the table alone does not arm the reference half.
    if cells.base_addr_for_compile() == 0 {
        return;
    }
    assert!(cells.ref_epoch_if_armed().is_none());
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    cells.arm_ref_cells();
    assert_eq!(cells.ref_epoch_if_armed(), Some(epoch));
    cells.note_compact_ref_field(52, 2, 24, epoch, || false);
    if cratonvm_types::layout_replace_epoch() != epoch {
        return;
    }
    assert_eq!(
        cells.snapshot(UNSAFE_FIELD_CELL_COUNT + i).0,
        unsafe_field_cell_word(52, 2, true, 24).unwrap()
    );
    assert_eq!(cells.snapshot(i).0, 0, "the int/long half is untouched");
}

/// JIT round 21 wave 1, lane updaters (review UPD21-R1): the reference
/// publisher takes a veto. A vetoed key publishes nothing; a key the veto
/// lets through publishes once; an already-published key never asks the
/// veto again (the hot CAS path pays nothing) and a later veto does not
/// evict it.
#[test]
fn upd21_a_vetoed_reference_cell_is_never_published_and_the_veto_is_asked_once() {
    use cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT;
    use std::cell::Cell;
    let cells = UnsafeInlineCells::new();
    let epoch = cratonvm_types::layout_replace_epoch();
    if cells.base_addr_for_compile() == 0 {
        return; // not x86-64, or a kill switch is set
    }
    cells.arm_ref_cells();
    let i = UNSAFE_FIELD_CELL_COUNT + unsafe_field_cell_index(52, 0, true);
    let word = unsafe_field_cell_word(52, 0, true, 16).unwrap();
    let asked = Cell::new(0u32);
    cells.note_compact_ref_field(52, 0, 16, epoch, || {
        asked.set(asked.get() + 1);
        true
    });
    if cratonvm_types::layout_replace_epoch() != epoch {
        return; // another test moved the epoch: nothing is pinned
    }
    assert_eq!(asked.get(), 1);
    assert_eq!(cells.snapshot(i).0, 0, "vetoed: nothing published");
    cells.note_compact_ref_field(52, 0, 16, epoch, || {
        asked.set(asked.get() + 1);
        false
    });
    if cratonvm_types::layout_replace_epoch() != epoch {
        return;
    }
    assert_eq!(asked.get(), 2);
    assert_eq!(cells.snapshot(i).0, word, "an admitted key publishes");
    cells.note_compact_ref_field(52, 0, 16, epoch, || {
        asked.set(asked.get() + 1);
        true
    });
    assert_eq!(asked.get(), 2, "a published key asks nothing");
    assert_eq!(cells.snapshot(i).0, word, "a later veto evicts nothing");
    assert_eq!(
        cells.snapshot(unsafe_field_cell_index(52, 0, true)).0,
        0,
        "the int/long half is untouched"
    );
}

/// JIT tier2 wave 2 (page agent `atm20`, review of UPD21-R1): a key whose
/// cells another field owns (a collision loser) never asks the reference
/// veto. It keeps its call, so it reaches this publisher on every CAS, and
/// the caller's veto takes the class-manager read lock and walks the
/// superclass chain for a slot below four. The first owner keeps its cell.
#[test]
fn atm20_a_collision_loser_never_asks_the_reference_veto() {
    use cratonvm_jit::direct_helpers::{unsafe_field_cell_index2, UNSAFE_FIELD_CELL_COUNT};
    use std::cell::Cell;
    let cells = UnsafeInlineCells::new();
    let epoch = cratonvm_types::layout_replace_epoch();
    if cells.base_addr_for_compile() == 0 {
        return; // not x86-64, or a kill switch is set
    }
    cells.arm_ref_cells();
    // Another class whose slot-0 reference key shares key (52, 0)'s FIRST cell.
    let first = unsafe_field_cell_index(52, 0, true);
    let Some(other) = (53u32..200_000).find(|&c| unsafe_field_cell_index(c, 0, true) == first)
    else {
        return;
    };
    cells.note_compact_ref_field(52, 0, 16, epoch, || false);
    let owner = unsafe_field_cell_word(52, 0, true, 16).unwrap();
    let asked = Cell::new(0u32);
    let ask = || {
        asked.set(asked.get() + 1);
        false
    };
    cells.note_compact_ref_field(other, 0, 24, epoch, &ask);
    cells.note_compact_ref_field(other, 0, 24, epoch, &ask);
    if cratonvm_types::layout_replace_epoch() != epoch {
        return; // another test moved the epoch: nothing is pinned
    }
    assert_eq!(
        cells.snapshot(UNSAFE_FIELD_CELL_COUNT + first).0,
        owner,
        "the first owner keeps its cell"
    );
    // Asked once for a cell it wrote (a two-way table's free second cell),
    // never for a cell it could not write, and never again once published.
    let second = UNSAFE_FIELD_CELL_COUNT + unsafe_field_cell_index2(other, 0, true);
    let other_word = unsafe_field_cell_word(other, 0, true, 24).unwrap();
    let published = cells.snapshot(second).0 == other_word;
    assert_eq!(asked.get(), u32::from(published));
}

/// JIT round 21, lane atomics (UNSAFE21-2): the sub-word half publishes
/// only once armed, into its own words (never the int/long or reference
/// halves), keyed with the "2-byte field" bit.
#[test]
fn jr21_atomics_subword_cells_publish_into_their_own_half_once_armed() {
    use cratonvm_jit::direct_helpers::{
        unsafe_subword_rmw_inline_enabled, UNSAFE_FIELD_CELL_COUNT,
    };
    let cells = UnsafeInlineCells::new();
    let epoch = cratonvm_types::layout_replace_epoch();
    assert!(cells.subword_epoch_if_armed().is_none());
    if cells.base_addr_for_compile() == 0 || !unsafe_subword_rmw_inline_enabled() {
        return;
    }
    assert_eq!(cells.subword_epoch_if_armed(), Some(epoch));
    let i = unsafe_field_cell_index(61, 4, true);
    cells.note_compact_subword_field(61, 4, true, 22, epoch);
    if cratonvm_types::layout_replace_epoch() != epoch {
        return;
    }
    assert_eq!(
        cells.snapshot(2 * UNSAFE_FIELD_CELL_COUNT + i).0,
        unsafe_field_cell_word(61, 4, true, 22).unwrap()
    );
    assert_eq!(cells.snapshot(i).0, 0, "the int/long half is untouched");
    assert_eq!(
        cells.snapshot(UNSAFE_FIELD_CELL_COUNT + i).0,
        0,
        "the reference half is untouched"
    );
}

/// JIT round 21, lane atomics (STATICCAS21-2): a static entry is wanted
/// only once a compile armed the half and only for a band offset; it is
/// published whole, key last, once (a colliding offset keeps the first
/// owner), and a zero address publishes nothing.
#[test]
fn jr21_atomics_static_entries_publish_once_in_band() {
    use cratonvm_jit::direct_helpers::{
        unsafe_static_cell_index, unsafe_static_cell_key, unsafe_static_rmw_inline_enabled,
        UNSAFE_STATIC_OFFSET_FIRST,
    };
    let cells = UnsafeInlineCells::new();
    let off = UNSAFE_STATIC_OFFSET_FIRST + 5;
    assert!(!cells.static_publish_wanted(off), "not armed yet");
    if cells.base_addr_for_compile() == 0 || !unsafe_static_rmw_inline_enabled() {
        return;
    }
    assert!(cells.static_publish_wanted(off));
    assert!(!cells.static_publish_wanted(5), "outside the static band");
    cells.note_static_target(off, true, 0x1000, 0x2000, 7, 3);
    assert_eq!(
        cells.static_snapshot(off),
        [
            unsafe_static_cell_key(off, true),
            0x1000,
            0x2000,
            (7 << 32) | 48
        ]
    );
    assert!(!cells.static_publish_wanted(off), "taken");
    let other = (off + 1..)
        .find(|&o| unsafe_static_cell_index(o) == unsafe_static_cell_index(off))
        .expect("a colliding offset");
    cells.note_static_target(other, false, 0x3000, 0x4000, 8, 1);
    assert_eq!(
        cells.static_snapshot(off)[0],
        unsafe_static_cell_key(off, true),
        "the first owner keeps the entry"
    );
    let free = (off + 1..)
        .find(|&o| unsafe_static_cell_index(o) != unsafe_static_cell_index(off))
        .expect("another entry");
    cells.note_static_target(free, false, 0, 0x4000, 8, 1);
    assert_eq!(cells.static_snapshot(free), [0; 4], "no mirror, no entry");
    assert!(cells.static_publish_wanted(free));
}

/// JIT round 21, lane atomics (STATICCAS21-2): a layout-epoch reset
/// clears the three cell halves and leaves the static entries.
#[test]
fn jr21_atomics_an_epoch_reset_keeps_the_static_entries() {
    use cratonvm_jit::direct_helpers::{unsafe_static_cell_key, UNSAFE_STATIC_OFFSET_FIRST};
    let cells = UnsafeInlineCells::new();
    if cells.base_addr_for_compile() == 0
        || !cratonvm_jit::direct_helpers::unsafe_static_rmw_inline_enabled()
    {
        return;
    }
    let off = UNSAFE_STATIC_OFFSET_FIRST + 9;
    cells.note_static_target(off, false, 0x1000, 0x2000, 9, 1);
    // A stale word 0 forces the reset on the next publication.
    cells.words[0].store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    let epoch = cratonvm_types::layout_replace_epoch();
    cells.note_compact_field(3, 1, false, 24, epoch);
    assert_eq!(
        cells.static_snapshot(off),
        [
            unsafe_static_cell_key(off, false),
            0x1000,
            0x2000,
            (9 << 32) | 16
        ]
    );
}

/// JIT round 21 proposal ATOMICS21-5: the entries' mirror words are weak
/// -- a moved mirror's word follows it, an unmoved one stays, and an
/// unloaded class's entries are withdrawn whole (the entry is free again).
#[test]
fn jr21_atomics_static_mirrors_follow_the_collector_and_die_with_the_class() {
    use cratonvm_jit::direct_helpers::{
        unsafe_static_cell_index, unsafe_static_cell_key, UNSAFE_STATIC_OFFSET_FIRST,
    };
    let cells = UnsafeInlineCells::new();
    if cells.base_addr_for_compile() == 0
        || !cratonvm_jit::direct_helpers::unsafe_static_rmw_inline_enabled()
    {
        return;
    }
    let a = UNSAFE_STATIC_OFFSET_FIRST + 3;
    let b = (a + 1..)
        .find(|&o| unsafe_static_cell_index(o) != unsafe_static_cell_index(a))
        .expect("another entry");
    cells.note_static_target(a, true, 0xA000, 0x2000, 11, 0);
    cells.note_static_target(b, false, 0xB000, 0x3000, 12, 2);
    cells.remap_static_mirrors(&|old| (old == 0xA000).then_some(0xA800));
    assert_eq!(cells.static_snapshot(a)[1], 0xA800, "moved");
    assert_eq!(cells.static_snapshot(b)[1], 0xB000, "not moved");
    cells.forget_static_class(11);
    assert_eq!(cells.static_snapshot(a), [0; 4], "withdrawn");
    assert!(cells.static_publish_wanted(a), "free again");
    assert_eq!(
        cells.static_snapshot(b),
        [
            unsafe_static_cell_key(b, false),
            0xB000,
            0x3000,
            (12 << 32) | 32
        ]
    );
}
