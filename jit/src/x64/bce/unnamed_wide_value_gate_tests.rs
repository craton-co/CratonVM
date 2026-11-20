use super::*;

/// `aload_0; getfield #1; aload_0; invokevirtual #2; pop; pop; return`
/// (the page's `sink(o.longField, o.virt())` shape, minus the sink).
const FIELD_THEN_CALL: [u8; 11] = [
    0x2a, 0xb4, 0x00, 0x01, 0x2a, 0xb6, 0x00, 0x02, 0x57, 0x57, 0xb1,
];

#[test]
fn the_opcode_gate_alone_misses_a_long_field_and_the_descriptor_gate_catches_it() {
    assert!(
        !code_uses_long_float_double(&FIELD_THEN_CALL, FIELD_THEN_CALL.len()),
        "no opcode here NAMES a long; this is the gap the second gate closes"
    );
    let long_field = |pc: usize, _op: u8| -> Option<u8> {
        match pc {
            1 => Some(b'J'),
            5 => Some(b'I'),
            _ => None,
        }
    };
    assert!(code_pushes_unnamed_wide_value(
        &FIELD_THEN_CALL,
        FIELD_THEN_CALL.len(),
        &long_field
    ));
    let double_call = |pc: usize, _op: u8| -> Option<u8> {
        match pc {
            1 => Some(b'I'),
            5 => Some(b'D'),
            _ => None,
        }
    };
    assert!(code_pushes_unnamed_wide_value(
        &FIELD_THEN_CALL,
        FIELD_THEN_CALL.len(),
        &double_call
    ));
}

#[test]
fn int_ref_and_void_sites_keep_a_method_int_only() {
    let narrow = |pc: usize, _op: u8| -> Option<u8> {
        match pc {
            1 => Some(b'I'),
            5 => Some(b'V'),
            _ => None,
        }
    };
    assert!(!code_pushes_unnamed_wide_value(
        &FIELD_THEN_CALL,
        FIELD_THEN_CALL.len(),
        &narrow
    ));
    let refs = |_pc: usize, _op: u8| -> Option<u8> { Some(b'L') };
    assert!(!code_pushes_unnamed_wide_value(
        &FIELD_THEN_CALL,
        FIELD_THEN_CALL.len(),
        &refs
    ));
}

#[test]
fn a_site_with_no_metadata_counts_as_wide() {
    let unknown_field = |pc: usize, _op: u8| -> Option<u8> {
        if pc == 5 {
            Some(b'I')
        } else {
            None
        }
    };
    assert!(code_pushes_unnamed_wide_value(
        &FIELD_THEN_CALL,
        FIELD_THEN_CALL.len(),
        &unknown_field
    ));
    // A float `ldc`: `ldc #3; freturn` would already trip the opcode gate
    // through `freturn`; `ldc #3; pop; return` does not.
    let ldc = [0x12u8, 0x03, 0x57, 0xb1];
    assert!(!code_uses_long_float_double(&ldc, ldc.len()));
    let float_ldc = |_pc: usize, _op: u8| -> Option<u8> { Some(b'F') };
    assert!(code_pushes_unnamed_wide_value(&ldc, ldc.len(), &float_ldc));
    let int_ldc = |_pc: usize, _op: u8| -> Option<u8> { Some(b'I') };
    assert!(!code_pushes_unnamed_wide_value(&ldc, ldc.len(), &int_ldc));
}

#[test]
fn stores_are_not_value_producing_sites() {
    // `aload_0; iconst_0; putfield #1; return` — putfield only pops.
    let put = [0x2au8, 0x03, 0xb5, 0x00, 0x01, 0xb1];
    let never = |_pc: usize, _op: u8| -> Option<u8> { None };
    assert!(!code_pushes_unnamed_wide_value(&put, put.len(), &never));
    assert!(!opcode_may_push_unnamed_wide_value(0xb5));
    assert!(!opcode_may_push_unnamed_wide_value(0xb3));
    assert!(opcode_may_push_unnamed_wide_value(0xb4));
    assert!(opcode_may_push_unnamed_wide_value(0xba));
}
