use super::iconst_arith_group;

/// Run the decoder over `code` (padded with two zero bytes, as the
/// interpreter pads a method) with the operand at pc 1.
fn run(code: &[u8], vx: i32) -> Option<(i32, usize)> {
    let mut padded = code.to_vec();
    padded.extend([0, 0]);
    // SAFETY: `padded` holds `code.len()` bytes plus two padding bytes,
    // and pc 1 is below `code.len()` in every call below.
    unsafe { iconst_arith_group(padded.as_ptr(), code.len(), 1, vx) }
}

#[test]
fn each_operation_answers_as_its_arm() {
    // iload_0; iconst_1; isub; ireturn
    assert_eq!(run(&[0x1a, 0x04, 0x64, 0xac], 10), Some((9, 3)));
    // iload_0; iconst_m1; iadd
    assert_eq!(run(&[0x1a, 0x02, 0x60, 0xac], 0), Some((-1, 3)));
    // iload_0; bipush -1; iand
    assert_eq!(run(&[0x1a, 0x10, 0xff, 0x7e, 0xac], 0x1234), Some((0x1234, 4)));
    // iload_0; bipush 127; iand
    assert_eq!(run(&[0x1a, 0x10, 0x7f, 0x7e, 0xac], 0x1ff), Some((0x7f, 4)));
    // iload_0; sipush 1000; imul
    assert_eq!(run(&[0x1a, 0x11, 0x03, 0xe8, 0x68, 0xac], 7), Some((7000, 5)));
    // iload_0; sipush -2; ior
    assert_eq!(run(&[0x1a, 0x11, 0xff, 0xfe, 0x80, 0xac], 1), Some((-1, 5)));
    // iload_0; iconst_5; ixor
    assert_eq!(run(&[0x1a, 0x08, 0x82, 0xac], 3), Some((6, 3)));
    // Wrap-around.
    assert_eq!(run(&[0x1a, 0x04, 0x60, 0xac], i32::MAX), Some((i32::MIN, 3)));
    assert_eq!(run(&[0x1a, 0x04, 0x64, 0xac], i32::MIN), Some((i32::MAX, 3)));
    assert_eq!(run(&[0x1a, 0x05, 0x68, 0xac], i32::MIN), Some((0, 3)));
    // Shifts: the distance is masked to five bits, ishr keeps the sign.
    assert_eq!(run(&[0x1a, 0x10, 33, 0x78, 0xac], 1), Some((2, 4)));
    assert_eq!(run(&[0x1a, 0x10, 0xff, 0x78, 0xac], 1), Some((i32::MIN, 4)));
    assert_eq!(run(&[0x1a, 0x10, 4, 0x7a, 0xac], -256), Some((-16, 4)));
    assert_eq!(run(&[0x1a, 0x10, 28, 0x7c, 0xac], -1), Some((15, 4)));
    assert_eq!(run(&[0x1a, 0x10, 32, 0x7c, 0xac], -1), Some((-1, 4)));
}

#[test]
fn other_shapes_decline() {
    // idiv / irem can throw: never taken.
    assert_eq!(run(&[0x1a, 0x03, 0x6c, 0xac], 1), None);
    assert_eq!(run(&[0x1a, 0x03, 0x70, 0xac], 1), None);
    // A compare-and-branch is `fused_icmp_arm!`'s.
    assert_eq!(run(&[0x1a, 0x04, 0xa2, 0x00, 0x05, 0xac], 1), None);
    // Not a constant operand.
    assert_eq!(run(&[0x1a, 0x1b, 0x60, 0xac], 1), None);
    // The operation would sit at or past the end of the code.
    assert_eq!(run(&[0x1a, 0x04], 1), None);
    assert_eq!(run(&[0x1a, 0x10, 0x05], 1), None);
    assert_eq!(run(&[0x1a, 0x11, 0x00, 0x05], 1), None);
}
