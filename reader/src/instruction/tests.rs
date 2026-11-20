use super::*;

// ---- Zero-operand opcodes ----

#[test]
fn decode_nop() {
    let (instr, next) = Instruction::decode(&[0x00], 0).unwrap();
    assert_eq!(instr, Instruction::Nop);
    assert_eq!(next, 1);
}

#[test]
fn decode_aconst_null() {
    let (instr, _) = Instruction::decode(&[0x01], 0).unwrap();
    assert_eq!(instr, Instruction::AconstNull);
}

#[test]
fn decode_iconst_m1_through_5() {
    let expected = [
        (0x02u8, Instruction::IconstM1),
        (0x03, Instruction::Iconst0),
        (0x04, Instruction::Iconst1),
        (0x05, Instruction::Iconst2),
        (0x06, Instruction::Iconst3),
        (0x07, Instruction::Iconst4),
        (0x08, Instruction::Iconst5),
    ];
    for (opcode, expected_instr) in &expected {
        let (instr, next) = Instruction::decode(&[*opcode], 0).unwrap();
        assert_eq!(instr, *expected_instr, "opcode 0x{opcode:02x}");
        assert_eq!(next, 1);
    }
}

#[test]
fn decode_lconst() {
    let (i0, _) = Instruction::decode(&[0x09], 0).unwrap();
    let (i1, _) = Instruction::decode(&[0x0a], 0).unwrap();
    assert_eq!(i0, Instruction::Lconst0);
    assert_eq!(i1, Instruction::Lconst1);
}

// ---- Operand-bearing opcodes ----

#[test]
fn decode_bipush() {
    let (instr, next) = Instruction::decode(&[0x10, 0x2A], 0).unwrap();
    assert_eq!(instr, Instruction::Bipush(42));
    assert_eq!(next, 2);
}

#[test]
fn decode_bipush_negative() {
    let (instr, _) = Instruction::decode(&[0x10, 0xFF], 0).unwrap();
    assert_eq!(instr, Instruction::Bipush(-1));
}

#[test]
fn decode_sipush() {
    // sipush 0x0100 = 256
    let (instr, next) = Instruction::decode(&[0x11, 0x01, 0x00], 0).unwrap();
    assert_eq!(instr, Instruction::Sipush(256));
    assert_eq!(next, 3);
}

#[test]
fn decode_ldc() {
    let (instr, _) = Instruction::decode(&[0x12, 0x05], 0).unwrap();
    assert_eq!(instr, Instruction::Ldc(5));
}

#[test]
fn decode_ldc_w() {
    let (instr, next) = Instruction::decode(&[0x13, 0x00, 0x0A], 0).unwrap();
    assert_eq!(instr, Instruction::LdcW(10));
    assert_eq!(next, 3);
}

// ---- Load/store shortcut opcodes ----

#[test]
fn decode_iload_0_through_3() {
    for (opcode, idx) in [(0x1au8, 0u16), (0x1b, 1), (0x1c, 2), (0x1d, 3)] {
        let (instr, _) = Instruction::decode(&[opcode], 0).unwrap();
        assert_eq!(instr, Instruction::Iload(idx), "iload_{idx}");
    }
}

#[test]
fn decode_aload_with_index() {
    let (instr, next) = Instruction::decode(&[0x19, 0x07], 0).unwrap();
    assert_eq!(instr, Instruction::Aload(7));
    assert_eq!(next, 2);
}

#[test]
fn decode_istore_0_through_3() {
    for (opcode, idx) in [(0x3bu8, 0u16), (0x3c, 1), (0x3d, 2), (0x3e, 3)] {
        let (instr, _) = Instruction::decode(&[opcode], 0).unwrap();
        assert_eq!(instr, Instruction::Istore(idx), "istore_{idx}");
    }
}

// ---- Arithmetic ----

#[test]
fn decode_iadd() {
    let (instr, _) = Instruction::decode(&[0x60], 0).unwrap();
    assert_eq!(instr, Instruction::Iadd);
}

#[test]
fn decode_iinc() {
    let (instr, next) = Instruction::decode(&[0x84, 0x03, 0x01], 0).unwrap();
    assert_eq!(
        instr,
        Instruction::Iinc {
            index: 3,
            constant: 1
        }
    );
    assert_eq!(next, 3);
}

#[test]
fn decode_iinc_negative() {
    // constant = -1 (0xFF as i8)
    let (instr, _) = Instruction::decode(&[0x84, 0x00, 0xFF], 0).unwrap();
    assert_eq!(
        instr,
        Instruction::Iinc {
            index: 0,
            constant: -1
        }
    );
}

// ---- Branch instructions ----

#[test]
fn decode_goto() {
    // goto offset +10 (0x000A)
    let (instr, next) = Instruction::decode(&[0xa7, 0x00, 0x0A], 0).unwrap();
    assert_eq!(instr, Instruction::Goto(10));
    assert_eq!(next, 3);
}

#[test]
fn decode_ifeq() {
    let (instr, _) = Instruction::decode(&[0x99, 0xFF, 0xFE], 0).unwrap();
    assert_eq!(instr, Instruction::Ifeq(-2));
}

#[test]
fn decode_goto_w() {
    // goto_w offset = 0x00000100 = 256
    let (instr, next) = Instruction::decode(&[0xc8, 0x00, 0x00, 0x01, 0x00], 0).unwrap();
    assert_eq!(instr, Instruction::GotoW(256));
    assert_eq!(next, 5);
}

// ---- Reference instructions ----

#[test]
fn decode_invokevirtual() {
    let (instr, next) = Instruction::decode(&[0xb6, 0x00, 0x0F], 0).unwrap();
    assert_eq!(instr, Instruction::Invokevirtual(15));
    assert_eq!(next, 3);
}

#[test]
fn decode_invokeinterface() {
    // invokeinterface index=5, count=2, reserved=0
    let (instr, next) = Instruction::decode(&[0xb9, 0x00, 0x05, 0x02, 0x00], 0).unwrap();
    assert_eq!(instr, Instruction::Invokeinterface { index: 5, count: 2 });
    assert_eq!(next, 5);
}

#[test]
fn decode_invokedynamic() {
    // invokedynamic index=3, reserved1=0, reserved2=0
    let (instr, _) = Instruction::decode(&[0xba, 0x00, 0x03, 0x00, 0x00], 0).unwrap();
    assert_eq!(instr, Instruction::Invokedynamic(3));
}

#[test]
fn decode_new() {
    let (instr, _) = Instruction::decode(&[0xbb, 0x00, 0x0A], 0).unwrap();
    assert_eq!(instr, Instruction::New(10));
}

// ---- Wide prefix ----

#[test]
fn decode_wide_iload() {
    // wide iload index=0x0100 (256)
    let (instr, next) = Instruction::decode(&[0xc4, 0x15, 0x01, 0x00], 0).unwrap();
    assert_eq!(instr, Instruction::Iload(256));
    assert_eq!(next, 4);
}

#[test]
fn decode_wide_iinc() {
    // wide iinc index=0x0100, constant=0x0200
    let (instr, next) = Instruction::decode(&[0xc4, 0x84, 0x01, 0x00, 0x02, 0x00], 0).unwrap();
    assert_eq!(
        instr,
        Instruction::Iinc {
            index: 256,
            constant: 512
        }
    );
    assert_eq!(next, 6);
}

// ---- Tableswitch / lookupswitch ----

#[test]
fn decode_tableswitch() {
    // tableswitch at pc=0, so padding aligns next to 4.
    // opcode at 0, padding bytes at 1,2,3, then default(4..7), low(8..11), high(12..15), one offset(16..19)
    let mut code = vec![0xaa, 0, 0, 0]; // opcode + 3 padding bytes
    code.extend(&10i32.to_be_bytes()); // default = 10
    code.extend(&1i32.to_be_bytes()); // low = 1
    code.extend(&1i32.to_be_bytes()); // high = 1 (1 offset)
    code.extend(&20i32.to_be_bytes()); // offsets[0] = 20
    let (instr, next) = Instruction::decode(&code, 0).unwrap();
    assert_eq!(
        instr,
        Instruction::Tableswitch(std::sync::Arc::new(TableSwitch {
            default: 10,
            low: 1,
            high: 1,
            offsets: vec![20],
        }))
    );
    assert_eq!(next, 20);
}

#[test]
fn decode_lookupswitch() {
    // lookupswitch at pc=0, padding to align to 4
    let mut code = vec![0xab, 0, 0, 0]; // opcode + 3 padding bytes
    code.extend(&5i32.to_be_bytes()); // default = 5
    code.extend(&2i32.to_be_bytes()); // npairs = 2
    code.extend(&100i32.to_be_bytes()); // key[0] = 100
    code.extend(&30i32.to_be_bytes()); // offset[0] = 30
    code.extend(&200i32.to_be_bytes()); // key[1] = 200
    code.extend(&40i32.to_be_bytes()); // offset[1] = 40
    let (instr, _) = Instruction::decode(&code, 0).unwrap();
    assert_eq!(
        instr,
        Instruction::Lookupswitch(std::sync::Arc::new(LookupSwitch {
            default: 5,
            pairs: vec![(100, 30), (200, 40)],
        }))
    );
}

/// A `tableswitch` header may claim up to `MAX_SWITCH_ENTRIES` offsets.
/// When the method does not actually contain them we must fail before
/// reserving for the claim, not after.
#[test]
fn tableswitch_header_larger_than_the_code_is_rejected() {
    let mut code = vec![0xaa, 0, 0, 0];
    code.extend(&0i32.to_be_bytes()); // default
    code.extend(&0i32.to_be_bytes()); // low
    code.extend(&16_383i32.to_be_bytes()); // high -> 16384 offsets, none present
    let err = Instruction::decode(&code, 0).unwrap_err();
    assert!(
        matches!(
            err,
            crate::class_reader_error::ClassReaderError::UnexpectedEndOfData { .. }
        ),
        "expected UnexpectedEndOfData, got {err:?}"
    );
}

#[test]
fn lookupswitch_npairs_larger_than_the_code_is_rejected() {
    let mut code = vec![0xab, 0, 0, 0];
    code.extend(&0i32.to_be_bytes()); // default
    code.extend(&16_384i32.to_be_bytes()); // npairs, no pairs present
    let err = Instruction::decode(&code, 0).unwrap_err();
    assert!(
        matches!(
            err,
            crate::class_reader_error::ClassReaderError::UnexpectedEndOfData { .. }
        ),
        "expected UnexpectedEndOfData, got {err:?}"
    );
}

// ---- is_branch / is_return ----

#[test]
fn is_branch_true_for_goto() {
    assert!(Instruction::Goto(10).is_branch());
    assert!(Instruction::GotoW(100).is_branch());
    assert!(Instruction::Ifeq(5).is_branch());
    assert!(Instruction::Ifnull(3).is_branch());
}

#[test]
fn is_branch_false_for_non_branches() {
    assert!(!Instruction::Nop.is_branch());
    assert!(!Instruction::Return.is_branch());
    assert!(!Instruction::Iadd.is_branch());
}

#[test]
fn is_return_true() {
    assert!(Instruction::Return.is_return());
    assert!(Instruction::Ireturn.is_return());
    assert!(Instruction::Lreturn.is_return());
    assert!(Instruction::Freturn.is_return());
    assert!(Instruction::Dreturn.is_return());
    assert!(Instruction::Areturn.is_return());
}

#[test]
fn is_return_false_for_non_returns() {
    assert!(!Instruction::Nop.is_return());
    assert!(!Instruction::Goto(0).is_return());
}

// ---- Error cases ----

#[test]
fn decode_unknown_opcode() {
    let result = Instruction::decode(&[0xFE], 0);
    assert!(result.is_err());
}

#[test]
fn decode_truncated_operand() {
    // bipush needs 1 operand byte
    let result = Instruction::decode(&[0x10], 0);
    assert!(result.is_err());
}

// ---- Decode at non-zero offset ----

#[test]
fn decode_at_offset() {
    // nop at index 0, bipush 42 at index 1
    let code = [0x00, 0x10, 0x2A];
    let (instr, next) = Instruction::decode(&code, 1).unwrap();
    assert_eq!(instr, Instruction::Bipush(42));
    assert_eq!(next, 3);
}

// ---- Multianewarray ----

#[test]
fn decode_multianewarray() {
    let (instr, next) = Instruction::decode(&[0xc5, 0x00, 0x0A, 0x02], 0).unwrap();
    assert_eq!(
        instr,
        Instruction::Multianewarray {
            index: 10,
            dimensions: 2
        }
    );
    assert_eq!(next, 4);
}

/// `lookupswitch` resolution is a binary search over the JVMS-mandated
/// sorted key table. This pins both halves of that claim: the search finds
/// every key and every gap in a sorted table, AND an UNSORTED table — which
/// only unverified bytecode can produce, and which a bare binary search
/// would silently mis-answer — still resolves through the linear fallback.
///
/// Delete the `Err(_)` arm of `LookupSwitch::target` and the unsorted half
/// of this test fails on key 5.
#[test]
fn lookupswitch_target_resolves_sorted_and_unsorted_tables() {
    let sorted = LookupSwitch {
        default: -1,
        pairs: vec![(-9, 10), (0, 20), (5, 30), (7, 40), (1000, 50)],
    };
    for (k, want) in [(-9, 10), (0, 20), (5, 30), (7, 40), (1000, 50)] {
        assert_eq!(sorted.target(k), want, "sorted key {k}");
    }
    for k in [i32::MIN, -10, -8, 1, 6, 8, 999, 1001, i32::MAX] {
        assert_eq!(sorted.target(k), -1, "sorted miss {k}");
    }

    // Not sorted: a binary search alone reports a miss on 5 here.
    let unsorted = LookupSwitch {
        default: -1,
        pairs: vec![(7, 40), (0, 20), (5, 30)],
    };
    assert_eq!(unsorted.target(7), 40);
    assert_eq!(unsorted.target(0), 20);
    assert_eq!(unsorted.target(5), 30);
    assert_eq!(unsorted.target(3), -1);

    let empty = LookupSwitch {
        default: 77,
        pairs: vec![],
    };
    assert_eq!(empty.target(0), 77);
}
