// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Switch-instruction padding tests for every legal `pc % 4` start.
//!
//! Per JVMS §6.5 (`tableswitch` / `lookupswitch`), the bytecode that
//! follows the opcode must be aligned to a 4-byte boundary *within the
//! method's code array*. Between the opcode (1 byte) and the next 4-aligned
//! offset there are 0–3 padding bytes. The existing in-crate tests in
//! `reader/src/instruction.rs` cover the `pc = 0` case (3 padding bytes)
//! only. This file pins all four cases — `pc % 4 ∈ {0, 1, 2, 3}` — so a
//! padding-loop regression (off-by-one in `while next % 4 != 0` or
//! similar) is caught here rather than during VM bring-up.
//!
//! Reader gap §2.3-2 from `.claude/review-2026-05-24/reader.md`.

use cratonvm_reader::instruction::Instruction;

/// Build a code buffer holding a `tableswitch` opcode at the given
/// `pc`, surrounded by enough leading no-op bytes to put the opcode
/// at the requested position. After the opcode we emit `padding_len`
/// padding bytes followed by a tiny well-formed body (default + low +
/// high == low → one offset).
fn build_tableswitch_at(pc: usize) -> (Vec<u8>, usize) {
    let mut code = vec![0x00u8; pc]; // pre-pad with `nop` so the opcode lands at `pc`
    code.push(0xaa); // tableswitch opcode
                     // Pad to the next 4-aligned offset from (pc + 1).
    let after_opcode = pc + 1;
    let mut next = after_opcode;
    while next % 4 != 0 {
        code.push(0u8);
        next += 1;
    }
    let body_start = code.len();
    // default = 7, low = 0, high = 0 → exactly one offset (10).
    code.extend_from_slice(&7i32.to_be_bytes());
    code.extend_from_slice(&0i32.to_be_bytes());
    code.extend_from_slice(&0i32.to_be_bytes());
    code.extend_from_slice(&10i32.to_be_bytes());
    (code, body_start)
}

/// Build a code buffer holding a `lookupswitch` opcode at the given
/// `pc`, padded to the next 4-aligned offset, followed by a one-pair
/// body.
fn build_lookupswitch_at(pc: usize) -> (Vec<u8>, usize) {
    let mut code = vec![0x00u8; pc];
    code.push(0xab); // lookupswitch opcode
    let after_opcode = pc + 1;
    let mut next = after_opcode;
    while next % 4 != 0 {
        code.push(0u8);
        next += 1;
    }
    let body_start = code.len();
    // default = 11, npairs = 1, (key=42, offset=20)
    code.extend_from_slice(&11i32.to_be_bytes());
    code.extend_from_slice(&1i32.to_be_bytes());
    code.extend_from_slice(&42i32.to_be_bytes());
    code.extend_from_slice(&20i32.to_be_bytes());
    (code, body_start)
}

fn decode_at(code: &[u8], pc: usize) -> Instruction {
    let (instr, _next) = Instruction::decode(code, pc).expect("decode must succeed");
    instr
}

// ----- tableswitch -----

#[test]
fn tableswitch_pc_0_padding_3() {
    let (code, _body) = build_tableswitch_at(0);
    let instr = decode_at(&code, 0);
    assert_eq!(
        instr,
        Instruction::Tableswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::TableSwitch {
                default: 7,
                low: 0,
                high: 0,
                offsets: vec![10],
            }
        ))
    );
}

#[test]
fn tableswitch_pc_1_padding_2() {
    let (code, _body) = build_tableswitch_at(1);
    let instr = decode_at(&code, 1);
    assert_eq!(
        instr,
        Instruction::Tableswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::TableSwitch {
                default: 7,
                low: 0,
                high: 0,
                offsets: vec![10],
            }
        ))
    );
}

#[test]
fn tableswitch_pc_2_padding_1() {
    let (code, _body) = build_tableswitch_at(2);
    let instr = decode_at(&code, 2);
    assert_eq!(
        instr,
        Instruction::Tableswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::TableSwitch {
                default: 7,
                low: 0,
                high: 0,
                offsets: vec![10],
            }
        ))
    );
}

#[test]
fn tableswitch_pc_3_padding_0() {
    // pc = 3 → opcode is at byte 3, next byte at 4 which is already
    // 4-aligned → zero padding bytes.
    let (code, _body) = build_tableswitch_at(3);
    let instr = decode_at(&code, 3);
    assert_eq!(
        instr,
        Instruction::Tableswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::TableSwitch {
                default: 7,
                low: 0,
                high: 0,
                offsets: vec![10],
            }
        ))
    );
}

// ----- lookupswitch -----

#[test]
fn lookupswitch_pc_0_padding_3() {
    let (code, _body) = build_lookupswitch_at(0);
    let instr = decode_at(&code, 0);
    assert_eq!(
        instr,
        Instruction::Lookupswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::LookupSwitch {
                default: 11,
                pairs: vec![(42, 20)],
            }
        ))
    );
}

#[test]
fn lookupswitch_pc_1_padding_2() {
    let (code, _body) = build_lookupswitch_at(1);
    let instr = decode_at(&code, 1);
    assert_eq!(
        instr,
        Instruction::Lookupswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::LookupSwitch {
                default: 11,
                pairs: vec![(42, 20)],
            }
        ))
    );
}

#[test]
fn lookupswitch_pc_2_padding_1() {
    let (code, _body) = build_lookupswitch_at(2);
    let instr = decode_at(&code, 2);
    assert_eq!(
        instr,
        Instruction::Lookupswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::LookupSwitch {
                default: 11,
                pairs: vec![(42, 20)],
            }
        ))
    );
}

#[test]
fn lookupswitch_pc_3_padding_0() {
    let (code, _body) = build_lookupswitch_at(3);
    let instr = decode_at(&code, 3);
    assert_eq!(
        instr,
        Instruction::Lookupswitch(std::sync::Arc::new(
            cratonvm_reader::instruction::LookupSwitch {
                default: 11,
                pairs: vec![(42, 20)],
            }
        ))
    );
}
