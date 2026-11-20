use super::{
    double_arith_apply, double_arith_group, java_drem, DoubleArithGroup, DoubleArithValue,
    DoubleOperand,
};

/// Decode `code` (padded with two zero bytes) with the operand at pc 1.
fn run(code: &[u8]) -> Option<DoubleArithGroup> {
    let mut padded = code.to_vec();
    padded.extend([0, 0]);
    double_arith_group(&padded, code.len(), 1)
}

fn g(
    operand: DoubleOperand,
    op: u8,
    store: Option<usize>,
    next_pc: usize,
) -> Option<DoubleArithGroup> {
    Some(DoubleArithGroup {
        operand,
        op,
        store,
        next_pc,
    })
}

#[test]
fn javac_double_forms_decode() {
    // dload_0; dload_2; dadd; dreturn
    assert_eq!(run(&[0x26, 0x28, 0x63, 0xaf]), g(DoubleOperand::Local(2), 0x63, None, 3));
    // dload_0; dconst_1; dsub; dstore_2
    assert_eq!(
        run(&[0x26, 0x0f, 0x67, 0x49, 0xb1]),
        g(DoubleOperand::Const(1.0), 0x67, Some(2), 4)
    );
    // dload_0; dload 5; dmul; dstore 7
    assert_eq!(
        run(&[0x26, 0x18, 5, 0x6b, 0x39, 7, 0xb1]),
        g(DoubleOperand::Local(5), 0x6b, Some(7), 6)
    );
    // dload_0; dload_2; dcmpl; ifge ... -- a compare never takes a store.
    assert_eq!(
        run(&[0x26, 0x28, 0x97, 0x9c, 0x00, 0x05, 0xb1]),
        g(DoubleOperand::Local(2), 0x97, None, 3)
    );
    // A `dstore n` whose index byte is past the end: no store taken.
    assert_eq!(run(&[0x26, 0x28, 0x63, 0x39]), g(DoubleOperand::Local(2), 0x63, None, 3));
}

#[test]
fn other_shapes_decline() {
    // dneg is unary; d2i is a conversion.
    assert_eq!(run(&[0x26, 0x28, 0x77, 0xaf]), None);
    assert_eq!(run(&[0x26, 0x28, 0x8e, 0xac]), None);
    // ldc2_w operand is not taken.
    assert_eq!(run(&[0x26, 0x14, 0x00, 0x01, 0x63, 0xaf]), None);
    // The operation would sit at or past the end of the code.
    assert_eq!(run(&[0x26, 0x28]), None);
    assert_eq!(run(&[0x26, 0x18, 0x04]), None);
    assert_eq!(run(&[0x26, 0x18]), None);
}

#[test]
fn operations_answer_as_their_arms() {
    use DoubleArithValue::{Double, Int};
    assert_eq!(double_arith_apply(0x63, 1.5, 2.25), Some(Double(3.75)));
    assert_eq!(double_arith_apply(0x67, 1.0, 3.0), Some(Double(-2.0)));
    assert_eq!(double_arith_apply(0x6b, 1e308, 10.0), Some(Double(f64::INFINITY)));
    assert_eq!(double_arith_apply(0x6f, -1.0, 0.0), Some(Double(f64::NEG_INFINITY)));
    assert_eq!(double_arith_apply(0x73, 7.5, 2.0), Some(Double(java_drem(7.5, 2.0))));
    assert_eq!(double_arith_apply(0x97, f64::NAN, 1.0), Some(Int(-1)));
    assert_eq!(double_arith_apply(0x98, f64::NAN, 1.0), Some(Int(1)));
    assert_eq!(double_arith_apply(0x97, 2.0, 1.0), Some(Int(1)));
    assert_eq!(double_arith_apply(0x98, -0.0, 0.0), Some(Int(0)));
    assert_eq!(double_arith_apply(0x61, 1.0, 1.0), None);
}
