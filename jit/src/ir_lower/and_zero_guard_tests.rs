// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT rx27 wave 6, lane regalloc (`CRATONVM_JIT_IR_AND_ZERO_TEST_GUARD`): a
//! pruned branch's guard on `(x & mask) == 0` deopts on `TEST x, mask`, and
//! the `And` its frame state names (the operand the re-executed `ifeq` reads)
//! is rebuilt in the guard's deopt pad. Executed through a real deopt.

use super::*;
use crate::deopt::{take_last_deopt, FrameValue};
use crate::ir::SafepointSnapshot;
use crate::ir_schedule;

const SWITCH: &str = "CRATONVM_JIT_IR_AND_ZERO_TEST_GUARD";

/// `int f(int x) { guard((x & -128) == 0) [bci 5]; return x; }` -- the shape
/// `prune_never_taken_branch` leaves for `iload x; bipush -128; iand; ifne`:
/// the guard's frame has `x` in local 0 and the masked value on the stack.
fn guard_graph() -> Graph {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let x = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let mask = graph.add(Op::Const(-128), IrType::Int, vec![], None);
    let and = graph.add(Op::And, IrType::Int, vec![x, mask], Some(3));
    let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
    let cmp = graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![and, zero], None);
    let _guard = graph.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, cmp], None);
    let ret = graph.add(Op::Return, IrType::Void, vec![ctrl, x], None);
    graph.exit = ret;
    graph
        .safepoints
        .push(SafepointSnapshot::new(5, vec![x], vec![and], Vec::new()));
    graph
}

fn helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value; the body calls no helper.
    unsafe { std::mem::zeroed() }
}

fn lower_guard(edits: &[(&str, Option<&str>)]) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let graph = guard_graph();
        let schedule = crate::ir_schedule::schedule(&graph);
        lower(&graph, &schedule, 1, 1, &helpers()).expect("lower the guarded method")
    })
}

/// `TEST r/m32, imm` instructions with immediate `imm` (register and
/// `[rbp + disp8/32]` forms).
fn test_imm_sites(code: &[u8], imm: i32) -> usize {
    let want = imm.to_le_bytes();
    (0..code.len().saturating_sub(4))
        .filter(|&p| code[p..p + 4] == want)
        .filter(|&p| {
            let at = |back: usize| p.checked_sub(back).map(|q| code[q]);
            (at(2) == Some(0xF7) && at(1).is_some_and(|m| m & 0xF8 == 0xC0))
                || (at(3) == Some(0xF7) && at(2) == Some(0x45))
                || (at(6) == Some(0xF7) && at(5) == Some(0x85))
        })
        .count()
}

/// Run `f(x)`: `Ok(result)` when the guard passes, `Err(frame)` with the
/// reconstructed frame when it deopts.
fn run(cm: &CompiledMethod, x: i64) -> Result<i64, (Vec<FrameValue>, Vec<FrameValue>)> {
    let _ = take_last_deopt();
    // SAFETY: the body was compiled by this test for one `int` argument and
    // runs on this thread; it calls nothing but the deopt stub.
    let got = unsafe { cm.try_call(&[x]) }.expect("call");
    match take_last_deopt() {
        None => Ok(got),
        Some(frame) => {
            assert_eq!(got, i64::MIN, "a deopt returns the sentinel");
            assert_eq!(frame.bci, 5, "resumes at the guard's bci");
            Err((frame.locals, frame.stack))
        }
    }
}

/// Under the switch the guard is a `TEST` (no `AND`, no home store on the
/// passing path) and a failing guard's frame still carries `x & -128` on its
/// stack, rebuilt by the pad: every outcome equals the unswitched body's.
#[test]
fn rx27w6_a_pruned_guard_names_its_and_and_the_pad_rebuilds_it() {
    let off = lower_guard(&[(SWITCH, Some("0"))]);
    let on = lower_guard(&[(SWITCH, Some("1"))]);
    assert_eq!(test_imm_sites(off.code_bytes(), -128), 0);
    assert_eq!(
        test_imm_sites(on.code_bytes(), -128),
        1,
        "the guard deopts on TEST x, -128"
    );
    for x in [
        0i64,
        5,
        127,
        128,
        200,
        1 << 20,
        -1,
        -128,
        i64::from(i32::MAX),
    ] {
        assert_eq!(run(&on, x), run(&off, x), "f({x})");
    }
    // The frame the interpreter resumes with, spelled out once.
    assert_eq!(run(&on, 5), Ok(5));
    assert_eq!(
        run(&on, 200),
        Err((vec![FrameValue::Int(200)], vec![FrameValue::Int(128)])),
        "x in local 0, x & -128 on the stack for the re-executed ifne"
    );
}

/// Without the guard's frame naming the `And` the remat is not needed; with
/// another snapshot naming it (another exit would read the unwritten word)
/// the guard keeps its `AND`.
#[test]
fn rx27w6_an_and_named_by_another_frame_keeps_its_and() {
    let edits = [(SWITCH, Some("1"))];
    let cm = cratonvm_types::flags::with_thread_overrides(&edits, || {
        let mut graph = guard_graph();
        let and = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::And)
            .expect("the And") as NodeId;
        let x = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Param(0))
            .expect("x") as NodeId;
        graph
            .safepoints
            .push(SafepointSnapshot::new(9, vec![x], vec![and], Vec::new()));
        let schedule = crate::ir_schedule::schedule(&graph);
        lower(&graph, &schedule, 1, 1, &helpers()).expect("lower")
    });
    assert_eq!(
        test_imm_sites(cm.code_bytes(), -128),
        0,
        "named elsewhere: no TEST"
    );
    assert_eq!(run(&cm, 5), Ok(5));
    assert_eq!(
        run(&cm, 200),
        Err((vec![FrameValue::Int(200)], vec![FrameValue::Int(128)]))
    );
}

fn elided_guard_graph(other_exit: bool) -> (Graph, NodeId) {
    let mut graph = guard_graph();
    let x = graph.nodes.iter().position(|n| n.op == Op::Param(0)).unwrap() as NodeId;
    let and = graph.nodes.iter().position(|n| n.op == Op::And).unwrap() as NodeId;
    let guard = graph.nodes.iter().position(|n| matches!(n.op, Op::Guard { bci: 5 })).unwrap() as NodeId;
    let start = graph.entry;
    let ctrl = graph.nodes[guard as usize].inputs[0];
    let arg = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let low = graph.add(Op::Const(127), IrType::Int, vec![], None);
    graph.nodes[x as usize].op = Op::And;
    graph.nodes[x as usize].inputs = vec![arg, low].into();
    if other_exit {
        let seven = graph.add(Op::Const(7), IrType::Int, vec![], None);
        let cmp = graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![x, seven], None);
        graph.add(Op::Guard { bci: 9 }, IrType::Void, vec![ctrl, cmp], Some(9));
        graph.safepoints.push(SafepointSnapshot::new(9, vec![arg], vec![and], Vec::new()));
    }
    (graph, guard)
}

fn high_mask_and_sites(code: &[u8]) -> usize {
    use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind};
    let mut decoder = Decoder::new(64, code, DecoderOptions::NONE);
    let mut count = 0;
    while decoder.can_decode() {
        let i = decoder.decode();
        if i.mnemonic() == Mnemonic::And && (0..i.op_count()).any(|n| {
            matches!(i.op_kind(n), OpKind::Immediate32 | OpKind::Immediate32to64
                | OpKind::Immediate8to32 | OpKind::Immediate8to64)
                && i.immediate(n) as u32 as i32 == -128
        }) { count += 1; }
    }
    count
}

#[test]
fn an_elided_guard_does_not_materialize_its_snapshot_only_and() {
    for alloc in ["0", "1"] {
        for poison in ["0", "1"] {
            for other_exit in [false, true] {
                cratonvm_types::flags::with_thread_overrides(&[
                    (SWITCH, Some("1")),
                    ("CRATONVM_JIT_IR_GUARD_FACT_ELIDE", Some("1")),
                    ("CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES", Some("1")),
                    ("CRATONVM_JIT_IR_REG_ALLOC", Some(alloc)),
                    ("CRATONVM_DBG_IR_POISON_INT_HIGH", Some(poison)),
                ], || {
                    let (graph, guard) = elided_guard_graph(other_exit);
                    let schedule = ir_schedule::schedule(&graph);
                    assert!(crate::ir_check_elim::analyze(&graph, &schedule).guard_elided(guard),
                        "the fixture must exercise an actually elided guard");
                    let cm = lower(&graph, &schedule, 1, 1, &helpers()).expect("lower");
                    assert_eq!(high_mask_and_sites(cm.code_bytes()) == 0, !other_exit,
                        "a guard that cannot exit must not materialize its snapshot-only And; another real exit must retain it");
                    for arg in (0..=255).chain([i32::MIN, i32::MIN + 1, -256, -128, -1, 263, i32::MAX]) {
                        let _ = take_last_deopt();
                        // SAFETY: this is the fixture's one-int compiled entry; its only
                        // runtime transfer is the existing deopt stub.
                        let got = unsafe { cm.try_call(&[i64::from(arg)]) }.expect("call");
                        let value = arg & 127;
                        if other_exit && value == 7 {
                            let frame = take_last_deopt().expect("the other guard must deopt");
                            assert_eq!(got, i64::MIN);
                            assert_eq!(frame.bci, 9);
                            // VM resume converts FrameValue::Int words with `as i32`;
                            // the debug poison deliberately occupies their upper half.
                            let ints = |values: &[FrameValue]| values.iter().map(|v| match v {
                                FrameValue::Int(word) => *word as i32,
                                other => panic!("expected an int resume value, got {other:?}"),
                            }).collect::<Vec<_>>();
                            assert_eq!(ints(&frame.locals), vec![arg]);
                            assert_eq!(ints(&frame.stack), vec![0]);
                        } else {
                            assert!(take_last_deopt().is_none());
                            assert_eq!(got, i64::from(value));
                        }
                    }
                });
            }
        }
    }
}
