use super::j27_static_slot_memo_tests::setup;
use super::*;

const INIT: VhStaticInit = VhStaticInit {
    mode: "getAndAdd",
    variable: b'I',
};

fn row(read_only: bool, exact: bool, desc: &str) -> VarHandleMeta {
    VarHandleMeta {
        kind: VH_KIND_STATIC,
        class_name: "p/J27S".to_string(),
        field_name: "f".to_string(),
        field_desc: desc.to_string(),
        field_index: -1,
        class_id: 0,
        read_only,
        exact: exact.then(|| Arc::new(std::array::from_fn(|_| String::new()))),
        ref_type_class_id: 0,
        holder_class_id: 0,
    }
}

/// A resolved access reports its target once, on the memo fill; the hit
/// that follows reports nothing. A row-less handle is never writable.
#[test]
fn the_fill_reports_the_target_and_a_hit_does_not() {
    let (mut ctx, s, vh) = setup(0x27F2_0001);
    let first = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
    assert!(matches!(first, Ok(Some((c, 0))) if c == s), "{first:?}");
    let notes = ctx.static_vh_notes.borrow().clone();
    assert_eq!(notes, vec![(vh.as_ptr() as usize, s.as_u32(), 0, false, None)]);
    // A hit (when the shared generation did not move meanwhile) adds none.
    let _ = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
    assert!(ctx.static_vh_notes.borrow().len() <= 2);
}

/// Only a read-write, non-exact `int` / `long` row is writable with a kind.
#[test]
fn the_row_decides_writable_and_kind() {
    let (ctx, s, vh) = setup(0x27F2_0002);
    for (meta, want) in [
        (row(false, false, "I"), (true, Some(b'I'))),
        (row(false, false, "J"), (true, Some(b'J'))),
        (row(true, false, "I"), (false, Some(b'I'))),
        (row(false, true, "I"), (false, Some(b'I'))),
        (row(false, false, "S"), (true, None)),
    ] {
        ctx.static_vh_notes.borrow_mut().clear();
        vh_static_note_target(&ctx, vh, Some(&meta), s, 3);
        let notes = ctx.static_vh_notes.borrow().clone();
        assert_eq!(notes, vec![(vh.as_ptr() as usize, s.as_u32(), 3, want.0, want.1)]);
    }
}
