// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT rx27 wave 8, lane regalloc (`CRATONVM_JIT_IR_OPERAND_IN_PLACE`): a
//! compare's or an ALU op's second operand is read where it lives (its home
//! word, register or an immediate) instead of through `MOV RCX, [rbp-x]`.
//! On the caller-saved lane's pressure loop, executed.

use super::edge_loop_fixture::{edge_ref, lower_edge_loop, run};

/// Instructions with opcode `op` (after an optional REX prefix, which the
/// scan does not need to see) and an `[rbp + disp8/32]` ModRM operand.
fn rbp_ops(code: &[u8], ops: &[u8]) -> usize {
    (1..code.len())
        .filter(|&p| ops.contains(&code[p - 1]))
        .filter(|&p| matches!(code[p] & 0xC7, 0x45 | 0x85))
        .count()
}

/// `MOV r, [rbp + d]`: the slot loads.
fn slot_loads(code: &[u8]) -> usize {
    rbp_ops(code, &[0x8B])
}

/// `CMP r, [rbp + d]` and `ADD / SUB / AND / OR / XOR r, [rbp + d]`: the
/// in-place reads.
fn in_place_reads(code: &[u8]) -> usize {
    rbp_ops(code, &[0x3B, 0x03, 0x2B, 0x23, 0x0B, 0x33])
}


/// The scanners on hand-written bytes.
#[test]
fn rx27w8_the_scanners() {
    // mov rax,[rbp-8]; cmp eax,[rbp-10h]; and eax,[rbp-18h]; mov ecx,eax
    let code = [
        0x48, 0x8B, 0x45, 0xF8, 0x3B, 0x45, 0xF0, 0x23, 0x45, 0xE8, 0x89, 0xC1,
    ];
    assert_eq!(slot_loads(&code), 1);
    assert_eq!(in_place_reads(&code), 2);
}


