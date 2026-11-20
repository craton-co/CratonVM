use super::osr_optimizing_not_known_better;

#[test]
fn division_remainder_and_long_compare_prefer_single_pass() {
    // lload_0; lconst_1; ldiv; lreturn
    assert!(osr_optimizing_not_known_better(&[0x1e, 0x0a, 0x6d, 0xad]));
    // lload_0; lconst_1; lrem; lreturn
    assert!(osr_optimizing_not_known_better(&[0x1e, 0x0a, 0x71, 0xad]));
    // lload_0; lload_0; lcmp; ireturn
    assert!(osr_optimizing_not_known_better(&[0x1e, 0x1e, 0x94, 0xac]));
}

/// The opcode bytes are recognised only at instruction boundaries: an
/// operand byte that happens to equal `idiv` / `lcmp` is not a division.
#[test]
fn operand_bytes_are_not_opcodes() {
    // bipush 0x6c; ireturn
    assert!(!osr_optimizing_not_known_better(&[0x10, 0x6c, 0xac]));
    // sipush 0x9494; ireturn
    assert!(!osr_optimizing_not_known_better(&[0x11, 0x94, 0x94, 0xac]));
    // iload_0; iconst_1; imul; ireturn -- plain arithmetic keeps the
    // optimizing body.
    assert!(!osr_optimizing_not_known_better(&[0x1a, 0x04, 0x68, 0xac]));
    // Round 9 wave 6: `int` division and remainder keep it too (measured
    // at least as fast there on the wave-5 binary).
    // iload_0; iconst_2; idiv; ireturn
    assert!(!osr_optimizing_not_known_better(&[0x1a, 0x05, 0x6c, 0xac]));
    // iload_0; iconst_2; irem; ireturn
    assert!(!osr_optimizing_not_known_better(&[0x1a, 0x05, 0x70, 0xac]));
    assert!(!osr_optimizing_not_known_better(&[]));
}

/// The optimizing-OSR memos are process-wide and a `ClassId` is only unique
/// within one VM: the same `(class id, method, pc)` in two VMs must key two
/// different entries, and within one VM the key must be stable.
#[test]
fn the_optimizing_osr_memo_key_is_scoped_to_its_vm() {
    use super::osr_optimizing_refusal_key;
    use cratonvm_types::ClassId;
    let cid = ClassId::new(42);
    let a = osr_optimizing_refusal_key(1, cid, "run", "()V", 7);
    assert_eq!(a, osr_optimizing_refusal_key(1, cid, "run", "()V", 7));
    assert_ne!(a, osr_optimizing_refusal_key(2, cid, "run", "()V", 7));
    assert_ne!(a, osr_optimizing_refusal_key(1, cid, "run", "()V", 8));
    assert_ne!(a, osr_optimizing_refusal_key(1, cid, "run", "(I)V", 7));
}
