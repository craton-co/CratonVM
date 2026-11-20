use super::{long_arith_apply, long_arith_group, LongArithGroup, LongArithValue, LongOperand};

/// Decode `code` (padded with two zero bytes) with the operand at pc 1.
fn run(code: &[u8]) -> Option<LongArithGroup> {
    let mut padded = code.to_vec();
    padded.extend([0, 0]);
    long_arith_group(&padded, code.len(), 1, false)
}

/// Wave 105 (`CRATONVM_INTERP_FUSED_LONG_INT_OPERAND`).
fn run_int(code: &[u8]) -> Option<LongArithGroup> {
    let mut padded = code.to_vec();
    padded.extend([0, 0]);
    long_arith_group(&padded, code.len(), 1, true)
}

#[test]
fn int_operand_forms_decode_only_when_on() {
    // lload_1; iload_3; i2l; ladd; lstore_1 -- `sum += x`
    let sum = [0x1f, 0x1d, 0x85, 0x61, 0x40, 0xb1];
    assert_eq!(run_int(&sum), g(LongOperand::IntLocal(3), 0x61, Some(1), 5));
    assert_eq!(run(&sum), None);
    // lload_0; iload 5; i2l; lmul; lstore 7
    assert_eq!(
        run_int(&[0x1e, 0x15, 5, 0x85, 0x69, 0x37, 7, 0xb1]),
        g(LongOperand::IntLocal(5), 0x69, Some(7), 7)
    );
    // lload_0; iload_2; i2l; lcmp; ifge ...
    assert_eq!(
        run_int(&[0x1e, 0x1c, 0x85, 0x94, 0x9c, 0x00, 0x05, 0xb1]),
        g(LongOperand::IntLocal(2), 0x94, None, 4)
    );
    // No `i2l` after the `iload`: not a long operand.
    assert_eq!(run_int(&[0x1e, 0x1c, 0x60, 0xb1]), None);
    // `i2l` followed by a division (can throw) or a shift: declined.
    assert_eq!(run_int(&[0x1e, 0x1c, 0x85, 0x6d, 0xad]), None);
    assert_eq!(run_int(&[0x1e, 0x1c, 0x85, 0x79, 0xad]), None);
    // The group would run past the code.
    assert_eq!(run_int(&[0x1e, 0x1c, 0x85]), None);
    assert_eq!(run_int(&[0x1e, 0x15, 5]), None);
    // The long forms still decode with the switch on.
    assert_eq!(run_int(&[0x1e, 0x20, 0x61, 0xad]), g(LongOperand::Local(2), 0x61, None, 3));
}

fn g(
    operand: LongOperand,
    op: u8,
    store: Option<usize>,
    next_pc: usize,
) -> Option<LongArithGroup> {
    Some(LongArithGroup {
        operand,
        op,
        store,
        next_pc,
    })
}

#[test]
fn javac_long_forms_decode() {
    // lload_0; lload_2; ladd; lreturn
    assert_eq!(run(&[0x1e, 0x20, 0x61, 0xad]), g(LongOperand::Local(2), 0x61, None, 3));
    // lload_0; lconst_1; lsub; lstore_2
    assert_eq!(
        run(&[0x1e, 0x0a, 0x65, 0x41, 0xb1]),
        g(LongOperand::Const(1), 0x65, Some(2), 4)
    );
    // lload_0; lload 5; lxor; lstore 7
    assert_eq!(
        run(&[0x1e, 0x16, 5, 0x83, 0x37, 7, 0xb1]),
        g(LongOperand::Local(5), 0x83, Some(7), 6)
    );
    // lload_0; lload_2; lcmp; ifge ... -- lcmp never takes a store.
    assert_eq!(
        run(&[0x1e, 0x20, 0x94, 0x9c, 0x00, 0x05, 0xb1]),
        g(LongOperand::Local(2), 0x94, None, 3)
    );
    // An `lstore n` whose index byte is past the end: no store taken.
    assert_eq!(run(&[0x1e, 0x20, 0x61, 0x37]), g(LongOperand::Local(2), 0x61, None, 3));
}

#[test]
fn other_shapes_decline() {
    // ldiv / lrem can throw.
    assert_eq!(run(&[0x1e, 0x20, 0x6d, 0xad]), None);
    assert_eq!(run(&[0x1e, 0x20, 0x71, 0xad]), None);
    // A shift's distance is an int: `lload; lload; lshl` is not javac's.
    assert_eq!(run(&[0x1e, 0x20, 0x79, 0xad]), None);
    // ldc2_w operand is not taken.
    assert_eq!(run(&[0x1e, 0x14, 0x00, 0x01, 0x61, 0xad]), None);
    // The operation would sit at or past the end of the code.
    assert_eq!(run(&[0x1e, 0x20]), None);
    assert_eq!(run(&[0x1e, 0x16, 0x04]), None);
    assert_eq!(run(&[0x1e, 0x16]), None);
}

#[test]
fn operations_answer_as_their_arms() {
    use LongArithValue::{Int, Long};
    assert_eq!(long_arith_apply(0x61, i64::MAX, 1), Some(Long(i64::MIN)));
    assert_eq!(long_arith_apply(0x65, i64::MIN, 1), Some(Long(i64::MAX)));
    assert_eq!(long_arith_apply(0x69, 1 << 62, 4), Some(Long(0)));
    assert_eq!(long_arith_apply(0x7f, -1, 0x1234), Some(Long(0x1234)));
    assert_eq!(long_arith_apply(0x81, 0x10, 0x01), Some(Long(0x11)));
    assert_eq!(long_arith_apply(0x83, -1, 0), Some(Long(-1)));
    assert_eq!(long_arith_apply(0x94, -5, 3), Some(Int(-1)));
    assert_eq!(long_arith_apply(0x94, 3, 3), Some(Int(0)));
    assert_eq!(long_arith_apply(0x94, i64::MAX, i64::MIN), Some(Int(1)));
    assert_eq!(long_arith_apply(0x6d, 1, 1), None);
}
