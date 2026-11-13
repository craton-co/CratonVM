// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT rx27 wave 8, lane regalloc (`CRATONVM_JIT_IR_OPERAND_IN_PLACE`): a
//! compare's or an ALU op's second operand is read where it lives (its home
//! word, register or an immediate) instead of through `MOV RCX, [rbp-x]`.
//! On the caller-saved lane's pressure loop, executed.

use super::edge_loop_fixture::{edge_ref, lower_edge_loop, run};

const SWITCH: &str = "CRATONVM_JIT_IR_OPERAND_IN_PLACE";

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

/// Under the switch the loop loads no more slots into registers than
/// without it, has no fewer memory-operand reads, and every
/// answer is the reference's. Both counts are printed for the host run.
#[test]
fn rx27w8_second_operands_are_read_in_place() {
    let off = lower_edge_loop(&[(SWITCH, Some("0"))]);
    let on = lower_edge_loop(&[(SWITCH, Some("1"))]);
    let (l_off, l_on) = (slot_loads(off.code_bytes()), slot_loads(on.code_bytes()));
    let (r_off, r_on) = (
        in_place_reads(off.code_bytes()),
        in_place_reads(on.code_bytes()),
    );
    eprintln!("[rx27w8] slot loads off={l_off} on={l_on}; in-place reads off={r_off} on={r_on}");
    assert!(l_on <= l_off, "no new slot load ({l_on} vs {l_off})");
    assert!(
        r_on >= r_off,
        "in-place reads only grow ({r_on} vs {r_off})"
    );
    for n in [-2i64, 0, 1, 2, 3, 10, 57, 1000, 100_000] {
        assert_eq!(run(&off, n), edge_ref(n), "f({n}) off");
        assert_eq!(run(&on, n), edge_ref(n), "f({n}) on");
    }
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

fn lower_array_stores(in_place: &str, scan: &str, alloc: &str, poison: &str) -> super::CompiledMethod {
    use super::*;
    use crate::ir::IrBuilder;
    // int x = v + 1; a[i] = x; a[0] = -1; return x;
    let code = [0x1c, 0x04, 0x60, 0x3e, 0x2a, 0x1b, 0x1d, 0x4f,
        0x2a, 0x03, 0x02, 0x4f, 0x1d, 0xac];
    cratonvm_types::flags::with_thread_overrides(&[
        (SWITCH, Some(in_place)),
        ("CRATONVM_JIT_IR_LINEAR_SCAN", Some(scan)),
        ("CRATONVM_JIT_IR_REG_ALLOC", Some(alloc)),
        ("CRATONVM_DBG_IR_POISON_INT_HIGH", Some(poison)),
    ], || {
        let mut builder = IrBuilder::new(3, 4);
        builder.set_param_types(&[IrType::Ref, IrType::Int, IrType::Int]);
        let graph = builder.build(&code, code.len()).expect("iastore fixture builds");
        let schedule = crate::ir_schedule::schedule(&graph);
        // SAFETY: the helper table is all integers. The fixture has only the
        // array guards' built-in deopt stub and never calls a runtime helper.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        lower(&graph, &schedule, 3, 4, &helpers).expect("iastore fixture lowers")
    })
}

#[test]
fn integer_array_stores_read_resident_values_and_immediates_in_place() {
    use crate::deopt::{take_last_deopt, FrameValue};
    use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};

    for (in_place, scan, alloc, expect_reg, expect_imm) in [
        ("0", "1", "1", false, false),
        ("1", "0", "0", false, true),
        ("1", "1", "0", true, true),
        ("1", "1", "1", true, true),
    ] {
        for poison in ["0", "1"] {
            let body = lower_array_stores(in_place, scan, alloc, poison);
            let stores: Vec<_> = Decoder::new(64, body.code_bytes(), DecoderOptions::NONE)
                .into_iter()
                .filter(|i| i.mnemonic() == Mnemonic::Mov && i.op0_kind() == OpKind::Memory
                    && i.memory_base() == Register::RAX && i.memory_index() == Register::RCX
                    && i.memory_index_scale() == 4)
                .collect();
            assert_eq!(stores.len(), 2);
            assert_eq!(stores[1].op1_kind() == OpKind::Immediate32, expect_imm,
                "an eligible immediate must store straight to the checked element");
            assert_eq!(stores[0].op1_register() != Register::EDX, expect_reg,
                "a resident value must store straight to the checked element");
            for v in [i32::MIN, -2, 0, 1, 0x1234_5678, i32::MAX] {
                let mut array = crate::tests::synthetic_array(4, &[6, 7, 8]);
                let a = array.as_mut_ptr() as i64;
                let x = v.wrapping_add(1);
                for (base, i) in [(a, -1), (a, 3), (0, 1)] {
                    let before = array.clone();
                    let _ = take_last_deopt();
                    // SAFETY: a live fixture array or null and two Java ints.
                    let got = unsafe { body.try_call(&[base, i, i64::from(v)]) }.expect("call");
                    assert_eq!(got, i64::MIN);
                    assert_eq!(array, before);
                    let frame = take_last_deopt().expect("a store guard must deopt");
                    assert_eq!(frame.bci, 7);
                    assert!(matches!(frame.stack.as_slice(),
                        [FrameValue::Object(b), FrameValue::Int(j), FrameValue::Int(y)]
                        if *b == base as u64 && *j as i32 == i as i32 && *y as i32 == x));
                }
                // SAFETY: index 1 and the constant index 0 are within the live array.
                let got = unsafe { body.try_call(&[a, 1, i64::from(v)]) }.expect("call");
                assert!(take_last_deopt().is_none());
                assert_eq!(got as i32, x);
                assert_eq!(array, crate::tests::synthetic_array(4, &[-1, i64::from(x), 8]));
            }
        }
    }
}
