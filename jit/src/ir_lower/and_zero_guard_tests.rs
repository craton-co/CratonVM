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
