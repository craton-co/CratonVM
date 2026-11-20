use super::{splice_unresolved_ldc_row_verdict as v, UnresolvedLdcRow as R};

#[test]
fn a_resolved_row_is_left_to_the_ordinary_checks() {
    assert_eq!(v(Some(0x12), true, 7, false, 0x1000, true, true), R::NotUnresolved);
    assert_eq!(v(Some(0x12), true, 7, true, 0x1000, false, false), R::NotUnresolved);
}

#[test]
fn an_unresolved_string_row_becomes_an_exit_only_with_switch_and_chain() {
    assert_eq!(v(Some(0x12), true, 7, false, 0, true, true), R::Exit);
    assert_eq!(v(Some(0x13), true, 7, false, 0, true, true), R::Exit);
    assert_eq!(v(Some(0x12), true, 7, true, 0, true, true), R::Exit, "class row on unexecuted arm");
    assert_eq!(
        v(Some(0x12), true, 7, false, 0, false, true),
        R::Refuse("ldc-non-numeric-constant"),
        "=0 restores the refusal"
    );
    assert_eq!(
        v(Some(0x12), true, 7, false, 0, true, false),
        R::Refuse("ldc-non-numeric-constant"),
        "no chain snapshot to resume through"
    );
}

#[test]
fn a_malformed_unresolved_row_refuses_the_body() {
    let mismatch = R::Refuse("ir-splice-ref-ldc-row-mismatch");
    assert_eq!(v(Some(0x12), true, 0, false, 0, true, true), mismatch, "no holder");
    assert_eq!(v(Some(0x12), false, 7, false, 0, true, true), mismatch, "pc past the body");
    assert_eq!(v(Some(0xbb), true, 7, false, 0, true, true), mismatch, "not an ldc");
    assert_eq!(v(None, true, 7, false, 0, true, true), mismatch);
}
