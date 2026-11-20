//! j27 wave 5 (lane JVMTIFRAME): the codes a compiled row's local read
//! answers, in the interpreter frame's order.
use super::*;

/// A static `mid(int x, long w, String s)` at a call: x, w (two slots),
/// s (null), and an unassigned local.
fn values() -> Vec<Value> {
    vec![
        Value::Int(7),
        Value::Long(1 << 40),
        Value::Uninitialized,
        Value::Object(None),
        Value::Uninitialized,
    ]
}

#[test]
fn a_home_read_answers_the_value_of_its_kind() {
    let v = values();
    assert_eq!(checked_home_value(&v, None, 0, LocalKind::Int), Ok(Value::Int(7)));
    assert_eq!(checked_home_value(&v, None, 1, LocalKind::Long), Ok(Value::Long(1 << 40)));
    assert_eq!(checked_home_value(&v, None, 3, LocalKind::Object), Ok(Value::Object(None)));
    // Never written: int 0, as an interpreter frame's slot.
    assert_eq!(checked_home_value(&v, None, 4, LocalKind::Int), Ok(Value::Int(0)));
}

#[test]
fn a_slot_outside_the_locals_is_invalid_before_any_type_check() {
    let v = values();
    assert_eq!(checked_home_value(&v, None, -1, LocalKind::Int), Err(ERR_INVALID_SLOT));
    assert_eq!(checked_home_value(&v, None, 5, LocalKind::Object), Err(ERR_INVALID_SLOT));
    // A long at the last slot has no second one.
    assert_eq!(checked_home_value(&v, None, 4, LocalKind::Long), Err(ERR_INVALID_SLOT));
}

#[test]
fn a_reference_read_as_a_primitive_and_the_reverse_mismatch() {
    let v = values();
    assert_eq!(checked_home_value(&v, None, 3, LocalKind::Int), Err(ERR_TYPE_MISMATCH));
    assert_eq!(checked_home_value(&v, None, 0, LocalKind::Object), Err(ERR_TYPE_MISMATCH));
    assert_eq!(checked_home_value(&v, None, 4, LocalKind::Object), Err(ERR_TYPE_MISMATCH));
}

#[test]
fn the_table_decides_after_the_tag() {
    let v = values();
    let not_live = Some(Declared::NotLive);
    assert_eq!(checked_home_value(&v, not_live, 0, LocalKind::Int), Err(ERR_INVALID_SLOT));
    let as_float = Some(Declared::Kind(LocalKind::Float));
    assert_eq!(checked_home_value(&v, as_float, 0, LocalKind::Int), Err(ERR_TYPE_MISMATCH));
    let as_int = Some(Declared::Kind(LocalKind::Int));
    assert_eq!(checked_home_value(&v, as_int, 0, LocalKind::Int), Ok(Value::Int(7)));
    // The tag check comes first: a reference slot read as int is a
    // mismatch even where the table says nothing covers it.
    assert_eq!(checked_home_value(&v, not_live, 3, LocalKind::Int), Err(ERR_TYPE_MISMATCH));
}

#[test]
fn a_write_is_not_served_and_the_instance_skips_the_table() {
    assert_eq!(
        asked(LocalOp::Write(LocalKind::Int, 0, super::super::LocalRead::Int(1))).err(),
        Some(ERR_OPAQUE_FRAME)
    );
    assert_eq!(asked(LocalOp::Instance), Ok((LocalKind::Object, 0, false)));
    assert_eq!(asked(LocalOp::Read(LocalKind::Long, 2)), Ok((LocalKind::Long, 2, true)));
}

/// j27 wave 8 (lane W8REV): a body compiled before its class's last
/// redefinition is read without the current table (the interpreter path's
/// rule for an obsolete frame); a current body keeps the table's answer.
#[test]
fn j27_w8rev_an_obsolete_compiled_body_is_read_without_the_table() {
    let not_live = Declared::NotLive;
    let never = RowTable { declared: not_live, redefined_at: None };
    assert_eq!(never.for_activation(Some(0)), not_live);
    assert_eq!(never.for_activation(None), not_live);
    let redefined = RowTable { declared: not_live, redefined_at: Some(5) };
    // Compiled at or after the last redefinition: the current bytecode.
    assert_eq!(redefined.for_activation(Some(5)), not_live);
    assert_eq!(redefined.for_activation(Some(9)), not_live);
    // Compiled before it, or no stamp to tell: table-less.
    assert_eq!(redefined.for_activation(Some(4)), Declared::NoTable);
    assert_eq!(redefined.for_activation(None), Declared::NoTable);
    // End to end: the stale body's slot reads its value where the current
    // table would have answered INVALID_SLOT.
    let v = values();
    let stale = redefined.for_activation(Some(4));
    assert_eq!(checked_home_value(&v, Some(stale), 0, LocalKind::Int), Ok(Value::Int(7)));
    let current = redefined.for_activation(Some(5));
    assert_eq!(
        checked_home_value(&v, Some(current), 0, LocalKind::Int),
        Err(ERR_INVALID_SLOT)
    );
}
