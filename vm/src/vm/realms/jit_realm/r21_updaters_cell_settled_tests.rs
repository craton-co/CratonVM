use super::UnsafeInlineCells;
use std::sync::atomic::Ordering;

/// JIT round 21 wave 1, lane updaters (cvm-r21w1 A/B fix; lane fuzz2's
/// page, both halves): a published cell, in either half, is settled; an
/// empty one is not; an unfit slot always is; a refusal settles its OWN
/// key only, never another key's; and the reference half is armed only
/// on request.
#[test]
fn a_published_or_refused_key_is_settled_and_a_refusal_is_per_key() {
    let cells = UnsafeInlineCells::new();
    assert!(!cells.ref_volatile_publish_armed());
    assert!(cells.field_cell_settled(3, 0x8000, false, false));
    assert!(!cells.field_cell_settled(40, 3, false, false));
    if cells.base_addr_for_compile() == 0 {
        return; // not x86-64, or a kill switch is set
    }
    let epoch = cratonvm_types::layout_replace_epoch();
    cells.note_compact_field(40, 3, false, 12, epoch);
    // A hot unfit field: refused many times, it settles only itself.
    for _ in 0..(1u32 << 17) {
        cells.note_field_refusal(60, 5, true, false, epoch);
    }
    if cratonvm_types::layout_replace_epoch() != epoch {
        return; // another test moved the epoch: nothing is pinned
    }
    assert!(cells.field_cell_settled(40, 3, false, false));
    // The reference half of the same key is a different word.
    assert!(!cells.field_cell_settled(40, 3, true, true));
    assert!(cells.field_cell_settled(60, 5, true, false), "the refused key");
    assert!(!cells.field_cell_settled(61, 2, false, false), "another key still tries");
    assert!(!cells.field_cell_settled(41, 2, true, true), "the budget was not spent");
    // A COLLIDING key reads settled by design (the publisher keeps the
    // first owner): `(46, 0, int)` indexes `46 ^ 0 == 40 ^ (3 << 1)`, the
    // published `(40, 3, int)` cell.
    assert_eq!(
        cratonvm_jit::direct_helpers::unsafe_field_cell_index(46, 0, false),
        cratonvm_jit::direct_helpers::unsafe_field_cell_index(40, 3, false)
    );
    assert!(cells.field_cell_settled(46, 0, false, false), "a colliding owner holds the cell");
    // Refusing it spends nothing either.
    cells.note_field_refusal(46, 0, false, false, epoch);
    assert!(!cells.field_cell_settled(41, 2, true, true), "still no budget spent");
    // A real publication overwrites the refusal of its key.
    cells.note_compact_field(60, 5, true, 24, epoch);
    let i = cratonvm_jit::direct_helpers::unsafe_field_cell_index(60, 5, true);
    if cratonvm_types::layout_replace_epoch() == epoch {
        assert_eq!(
            cells.snapshot(i).0,
            cratonvm_jit::direct_helpers::unsafe_field_cell_word(60, 5, true, 24).unwrap()
        );
    }
}

/// Lane fuzz2's page, half 1: under a table whose epoch word is not the
/// live layout epoch, nothing is settled (the caller reaches
/// `hw_atomic_addr`, whose publication resets the table), and no refusal
/// is recorded into it.
#[test]
fn a_stale_table_is_never_settled() {
    let cells = UnsafeInlineCells::new();
    if cells.base_addr_for_compile() == 0 {
        return;
    }
    let epoch = cratonvm_types::layout_replace_epoch();
    cells.note_compact_field(40, 3, false, 12, epoch);
    // As if a layout replacement moved the epoch past the table's.
    cells.words[0].store(u64::from(epoch.wrapping_sub(1)), Ordering::Release);
    assert!(!cells.field_cell_settled(40, 3, false, false));
    cells.note_field_refusal(41, 2, false, false, epoch);
    let j = cratonvm_jit::direct_helpers::unsafe_field_cell_index(41, 2, false);
    assert_eq!(cells.snapshot(j).0, 0, "no refusal into a stale table");
    // The next publication resets the table to the live epoch.
    cells.note_compact_field(40, 3, false, 12, epoch);
    if cratonvm_types::layout_replace_epoch() == epoch {
        assert!(cells.field_cell_settled(40, 3, false, false));
    }
}

/// The backstop budget saturates and, once spent, settles every key.
#[test]
fn the_backstop_budget_saturates() {
    let cells = UnsafeInlineCells::new();
    for _ in 0..(1u32 << 16) + 1 {
        cells.spend_publish_attempt();
    }
    assert!(cells.field_cell_settled(41, 2, true, true));
}

/// r21 wave 6, lane updaters (review item 6): a colliding key's refusal
/// word settles only its own key; a refusal over a colliding refusal
/// takes the cell and spends nothing; a refusal attempted under an epoch
/// that is no longer the table's records nothing and spends one attempt;
/// and a publication owns the cell for both colliding keys, which a
/// later refusal of the collider does not evict.
#[test]
fn a_colliding_refusal_settles_only_its_own_key() {
    use cratonvm_jit::direct_helpers::{unsafe_field_cell_index, unsafe_field_cell_word};
    let cells = UnsafeInlineCells::new();
    let epoch = cratonvm_types::layout_replace_epoch();
    if cells.snapshot(0).1 != u64::from(epoch) {
        return; // another test moved the epoch: nothing is pinned
    }
    // `(55, 0, int)` indexes `55 ^ 0 == 60 ^ (5 << 1 | 1)`.
    assert_eq!(unsafe_field_cell_index(55, 0, false), unsafe_field_cell_index(60, 5, true));
    let budget = || cells.publish_budget.load(Ordering::Relaxed);
    let full = budget();
    cells.note_field_refusal(60, 5, true, false, epoch);
    if cratonvm_types::layout_replace_epoch() != epoch {
        return;
    }
    assert!(cells.field_cell_settled(60, 5, true, false), "the refused key");
    assert!(
        !cells.field_cell_settled(55, 0, false, false),
        "a colliding key's refusal settles nothing"
    );
    cells.note_field_refusal(55, 0, false, false, epoch);
    if cratonvm_types::layout_replace_epoch() != epoch {
        return;
    }
    assert!(cells.field_cell_settled(55, 0, false, false), "the later refusal owns the note");
    assert!(!cells.field_cell_settled(60, 5, true, false), "the earlier one re-attempts");
    assert_eq!(budget(), full, "a recorded refusal spends nothing");
    // An attempt that began under an epoch the table no longer has: its
    // refusal may describe a replaced layout, so it is not recorded.
    cells.note_field_refusal(61, 2, false, false, epoch.wrapping_sub(1));
    assert!(!cells.field_cell_settled(61, 2, false, false));
    assert_eq!(budget(), full - 1, "an unrecordable refusal spends one attempt");
    if cells.base_addr_for_compile() == 0 {
        return; // not x86-64, or a kill switch is set
    }
    cells.note_compact_field(55, 0, false, 16, epoch);
    if cratonvm_types::layout_replace_epoch() != epoch {
        return;
    }
    assert!(cells.field_cell_settled(55, 0, false, false));
    assert!(cells.field_cell_settled(60, 5, true, false), "a colliding owner holds the cell");
    cells.note_field_refusal(60, 5, true, false, epoch);
    assert_eq!(
        cells.snapshot(unsafe_field_cell_index(55, 0, false)).0,
        unsafe_field_cell_word(55, 0, false, 16).unwrap(),
        "a refusal never evicts a published cell"
    );
    assert_eq!(budget(), full - 1);
}
