use super::bytecode_has_athrow;

#[test]
fn athrow_is_found_at_an_instruction_boundary_only() {
    // aload_0; athrow
    assert!(bytecode_has_athrow(&[0x2a, 0xbf]));
    // bipush 0xbf; ireturn -- an operand byte, not an athrow.
    assert!(!bytecode_has_athrow(&[0x10, 0xbf, 0xac]));
    // sipush 0xbfbf; pop; aload_0; athrow
    assert!(bytecode_has_athrow(&[0x11, 0xbf, 0xbf, 0x57, 0x2a, 0xbf]));
    assert!(!bytecode_has_athrow(&[]));
}

/// Wave 38 (lane L2): the optimizing route's half of the
/// `CRATONVM_OSR_NEWARRAY=0` lever asks the same walk for `newarray`.
#[test]
fn newarray_is_found_at_an_instruction_boundary_only() {
    use super::bytecode_has_opcode;
    // iconst_3; newarray int; areturn
    assert!(bytecode_has_opcode(&[0x06, 0xbc, 0x0a, 0xb0], 0xbc));
    // sipush 0xbcbc; ireturn -- operand bytes, not a newarray.
    assert!(!bytecode_has_opcode(&[0x11, 0xbc, 0xbc, 0xac], 0xbc));
}
