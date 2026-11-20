//! nio20 wave 3, lane L3 (W2L2-1/2/4): the one prefix table both plain
//! direct arms consult, the unbox rows, and the single-pass kill switch.

use super::*;

#[test]
fn each_bound_helper_matches_its_own_row_arm_and_shape() {
    let mut dh = crate::DirectHelperTable::EMPTY;
    dh.integer_value_of = 0x10;
    dh.long_value_of = 0x20;
    dh.object_get_class = 0x30;
    dh.integer_int_value = 0x40;
    dh.long_long_value = 0x50;
    dh.unsafe_get_int = 0x60;
    dh.unsafe_put_long = 0x68;
    let row =
        |entry, receiver, params, ret| sp_thin_prefix_row(&dh, entry, receiver, params, ret);
    use super::SpThinPrefix as P;
    assert_eq!(row(0x10, false, 1, b'L'), Some(P::IntegerValueOfBox));
    assert_eq!(row(0x20, false, 1, b'L'), Some(P::LongValueOfBox));
    assert_eq!(row(0x30, true, 0, b'L'), Some(P::ObjectGetClass));
    assert_eq!(row(0x40, true, 0, b'I'), Some(P::IntegerIntValue));
    assert_eq!(row(0x50, true, 0, b'J'), Some(P::LongLongValue));
    // The `Unsafe` rows leave the shape to their emitter.
    assert_eq!(row(0x60, true, 2, b'I'), Some(SpThinPrefix::UnsafeAccess));
    assert_eq!(row(0x68, true, 3, b'V'), Some(SpThinPrefix::UnsafeAccess));
    // The wrong arm, the wrong shape, an unknown entry, an unwired cell.
    assert_eq!(row(0x10, true, 1, b'L'), None);
    assert_eq!(row(0x30, false, 0, b'L'), None);
    assert_eq!(row(0x30, true, 1, b'L'), None);
    assert_eq!(row(0x40, true, 0, b'J'), None);
    assert_eq!(row(0x70, true, 0, b'I'), None);
    assert_eq!(row(0, true, 2, b'I'), None);
}

#[test]
fn the_single_pass_switch_covers_the_new_rows_and_not_the_box_rows() {
    let on = sp_thin_prefix_enabled();
    assert!(SpThinPrefix::IntegerValueOfBox.enabled());
    assert!(SpThinPrefix::LongValueOfBox.enabled());
    for p in [
        SpThinPrefix::ObjectGetClass,
        SpThinPrefix::UnsafeAccess,
        SpThinPrefix::IntegerIntValue,
        SpThinPrefix::LongLongValue,
    ] {
        assert_eq!(p.enabled(), on);
    }
}

#[test]
fn the_unbox_prefix_emits_nothing_without_a_class_id_or_for_an_xmm_receiver() {
    let mut c = super::super::emit::tests::test_compiler();
    let start = c.buf.pos();
    // `test_compiler` has no well-known-class resolver: no class id.
    let frame = [StackSlot::Frame(16)];
    let xmm = [StackSlot::Xmm(1)];
    assert!(c.try_emit_sp_unbox_prefix(5, false, &frame).is_none());
    assert!(c.try_emit_sp_unbox_prefix(5, true, &frame).is_none());
    assert!(c.try_emit_sp_unbox_prefix(5, false, &xmm).is_none());
    // Not exactly one argument (the receiver).
    assert!(c.try_emit_sp_unbox_prefix(5, false, &[]).is_none());
    assert_eq!(c.buf.pos(), start);
}
