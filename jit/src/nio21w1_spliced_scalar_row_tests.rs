use super::{ir, ir_splice_scalar_intrinsic_enabled, ir_spliced_scalar_row, x64};

/// The surrogate tests and the int families are admitted as STATIC rows
/// only; a long / floating-point family, a non-static row and an unknown
/// method are not (they keep the unbindable-call rule).
#[test]
fn only_static_int_shaped_scalar_rows_are_exempt() {
    if !ir_splice_scalar_intrinsic_enabled() || !ir::ir_scalar_intrinsics_enabled() {
        return;
    }
    if ir::ir_char_surrogate_scalar_enabled() {
        assert_eq!(
            ir_spliced_scalar_row("java/lang/Character", "isHighSurrogate", "(C)Z", 3),
            Some(ir::ScalarOp::IsHighSurrogate)
        );
        assert_eq!(
            ir_spliced_scalar_row("java/lang/Character", "isLowSurrogate", "(C)Z", 3),
            Some(ir::ScalarOp::IsLowSurrogate)
        );
    }
    assert_eq!(
        ir_spliced_scalar_row("java/lang/Integer", "min", "(II)I", 3),
        Some(ir::ScalarOp::MinI)
    );
    assert_eq!(
        ir_spliced_scalar_row("java/lang/Integer", "min", "(II)I", 1),
        None
    );
    assert_eq!(
        ir_spliced_scalar_row("java/lang/Long", "min", "(JJ)J", 3),
        None
    );
    // The PEXT/PDEP pair is the one long-shaped family, on a BMI2 host.
    for method in ["compress", "expand"] {
        assert_eq!(
            ir_spliced_scalar_row("java/lang/Long", method, "(JJ)J", 3).is_some(),
            x64::has_bmi2() && ir::bit_compress::ir_bit_compress_expand_enabled()
        );
    }
    assert_eq!(
        ir_spliced_scalar_row("java/lang/Long", "reverseBytes", "(J)J", 3),
        Some(ir::ScalarOp::ReverseBytesL)
    );
    assert_eq!(
        ir_spliced_scalar_row("java/lang/Math", "abs", "(D)D", 3),
        None
    );
    assert_eq!(
        ir_spliced_scalar_row("java/lang/Character", "isSurrogate", "(C)Z", 3),
        None
    );
}

/// The post-plan pass sees only spliced rows (combined pc past the
/// method's own code) that name a scalar family.
#[test]
fn the_post_plan_pass_takes_only_spliced_scalar_rows() {
    if !ir_splice_scalar_intrinsic_enabled() || !ir::ir_scalar_intrinsics_enabled() {
        return;
    }
    let min = Box::new(crate::JitInvokeInfo {
        class_name: "java/lang/Integer",
        method_name: "min",
        descriptor: "(II)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    let other = Box::new(crate::JitInvokeInfo {
        class_name: "p/Q",
        method_name: "f",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    let mut tables = ir::IrInlineTables::default();
    let min_addr = &*min as *const crate::JitInvokeInfo as usize;
    let other_addr = &*other as *const crate::JitInvokeInfo as usize;
    tables.invoke_info.insert(4, (min_addr, 2, b'I')); // own pc: skipped
    tables.invoke_info.insert(20, (min_addr, 2, b'I'));
    tables.invoke_info.insert(24, (other_addr, 1, b'I'));
    let rows = super::ir_plan_spliced_scalar_intrinsics(&tables, 10);
    assert_eq!(rows, vec![(20, ir::ScalarOp::MinI)]);
}
