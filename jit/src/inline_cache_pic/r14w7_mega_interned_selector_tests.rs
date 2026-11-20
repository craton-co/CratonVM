//! Round 14 wave 7, lane mega: the shape-4 census peeks at the selector
//! map and must never intern (a census that bound or grew anything would
//! change what it measures).
use super::*;

#[test]
fn a_peek_answers_only_an_interned_selector_and_interns_nothing() {
    let table = MegaDispatchTable::new();
    let ctx = mega_selector_loader_context(3);
    assert_eq!(
        table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 0),
        None
    );
    assert_eq!(
        table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 0),
        None,
        "the first peek interned nothing"
    );
    let id = table
        .selector_id_in_context("p/A", "f", "()V", 0, ctx, 0)
        .expect("interned");
    assert_eq!(
        table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 0),
        Some(id)
    );
    // Every field of the key counts: another kind, context or owner is
    // another selector.
    assert_eq!(
        table.interned_selector_in_context("p/A", "f", "()V", 2, ctx, 0),
        None
    );
    assert_eq!(
        table.interned_selector_in_context(
            "p/A",
            "f",
            "()V",
            0,
            mega_selector_loader_context(4),
            0
        ),
        None
    );
    assert_eq!(
        table.interned_selector_in_context("p/A", "f", "()V", 0, ctx, 9),
        None
    );
    // And the next interned id is the one after `id`: no peek took one.
    let next = table
        .selector_id_in_context("p/A", "g", "()V", 0, ctx, 0)
        .expect("interned");
    assert_eq!(next, id + 1);
}
