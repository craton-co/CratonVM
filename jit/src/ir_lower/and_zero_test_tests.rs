// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT rx27 wave 4, lane regalloc (`CRATONVM_JIT_IR_AND_ZERO_TEST_BRANCH`):
//! an `If` on `(x & mask) == 0` / `!= 0` branches on `TEST x, mask`, with no
//! `And` value and no home store, on a real bytecode loop, executed.

use super::*;
use crate::ir::IrBuilder;



/// ```java
/// static int f(int n) {
///     int c = 0;
///     for (int i = 0; i < n; i++) {
///         if ((i & 0xFFFFFF80) == 0) c++;     // the regex `ch < 128` test
///         if ((i & 4) != 0) c += 2;
///     }
///     return c;
/// }
/// ```
const MASK_LOOP: [u8; 38] = [
    0x03, 0x3c, // 0: c = 0
    0x03, 0x3d, // 2: i = 0
    0x1c, 0x1a, 0xa2, 0x00, 0x1c, // 4: if (i >= n) goto 34
    0x1c, 0x10, 0x80, 0x7e, // 9: i & -128
    0x9a, 0x00, 0x06, // 13: ifne 19
    0x84, 0x01, 0x01, // 16: c++
    0x1c, 0x07, 0x7e, // 19: i & 4
    0x99, 0x00, 0x06, // 22: ifeq 28
    0x84, 0x01, 0x02, // 25: c += 2
    0x84, 0x02, 0x01, // 28: i++
    0xa7, 0xff, 0xe5, // 31: goto 4
    0x1b, 0xac, // 34: return c
    0, 0,
];

fn mask_ref(n: i64) -> i32 {
    let mut c = 0i32;
    // Cast: the loop counter is a Java `int`.
    for i in 0..(n.max(0) as i32) {
        if i & -128 == 0 {
            c += 1;
        }
        if i & 4 != 0 {
            c += 2;
        }
    }
    c
}

extern "C" fn never_poll(_flag_addr: i64, _body_id: i64) -> i64 {
    0
}

fn lower_mask_loop(edits: &[(&str, Option<&str>)]) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let mut builder = IrBuilder::new(1, 3);
        builder.set_param_types(&[IrType::Int]);
        let mut graph = builder.build(&MASK_LOOP, 36).expect("IR build");
        crate::ir_optimize::optimize(&mut graph);
        let schedule = crate::ir_schedule::schedule(&graph);
        // A leaked poll word (no `static`): never set, so no poll is taken.
        let word: &'static mut [u64; 2] = Box::leak(Box::new([0, 0]));
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
        // `usize`, so all-zero is a valid value; only the poll is wired.
        let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.safepoint_flag_addr = word.as_ptr() as usize; // Cast: baked flag address
        helpers.safepoint_slow_path = never_poll as *const () as usize; // Cast: helper address
        lower_inner(
            &graph,
            &schedule,
            1,
            3,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("the loop lowers")
    })
}

fn run(cm: &CompiledMethod, n: i64) -> i32 {
    // SAFETY: an `int f(int)` loop body that calls nothing (the poll word is
    // never set).
    let got = unsafe { cm.try_call_with_context(0, &[n]) }.expect("call");
    // Cast: this tier's `int` result is the low 32 bits of RAX.
    got as i32
}

/// `TEST r/m32, imm` (`F7 /0 id`) instructions whose immediate is `imm`,
/// in register, `[rbp + disp8]` and `[rbp + disp32]` form.
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




/// The encodings: register (with REX for R8-R15 and for 64-bit), and the
/// frame forms with an 8- and a 32-bit displacement.
#[test]
fn rx27w4_test_encodings() {
    let (b, n) = test_reg_imm_bytes(RAX, 4, false);
    assert_eq!(&b[..n], &[0xF7, 0xC0, 4, 0, 0, 0]);
    let (b, n) = test_reg_imm_bytes(13, -128, true);
    assert_eq!(&b[..n], &[0x49, 0xF7, 0xC5, 0x80, 0xFF, 0xFF, 0xFF]);
    let (b, n) = test_frame_imm_bytes(0x10, 4, false);
    assert_eq!(&b[..n], &[0xF7, 0x45, 0xF0, 4, 0, 0, 0]);
    let (b, n) = test_frame_imm_bytes(0x200, 4, true);
    assert_eq!(
        &b[..n],
        &[0x48, 0xF7, 0x85, 0x00, 0xFE, 0xFF, 0xFF, 4, 0, 0, 0]
    );
}

/// The outer fold cancels the carry from the inner `And`.  The cancellation
/// must also restore the home that the carry planner had made unwritten, so
/// the outer `TEST` still has a location from which to read its input.
#[test]
fn rx27w4_nested_mask_restores_the_cancelled_carry_home() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let value = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let mask = g.add(Op::Const(255), IrType::Int, vec![], None);
    let inner = g.add(Op::And, IrType::Int, vec![value, mask], None);
    let outer = g.add(Op::And, IrType::Int, vec![inner, mask], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![outer, zero], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, cond], None);
    let schedule = crate::ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 16384);
    lo.carry_of = vec![None; graph.nodes.len()];
    lo.carry_deferred = vec![false; graph.nodes.len()];
    lo.home_dropped = vec![false; graph.nodes.len()];
    lo.carry_of[inner as usize] = Some((RAX, outer));
    lo.home_dropped[inner as usize] = true;
    let cancel = lo
        .and_zero_moot_carries(outer, inner, cond)
        .expect("fold can cancel the carry");
    assert_eq!(cancel, vec![inner]);
    lo.cancel_and_zero_carries(cancel);
    assert_eq!(lo.carry_of[inner as usize], None);
    assert!(
        !lo.home_dropped[inner as usize],
        "the TEST can read the restored home"
    );
    lo.alloc_slot(value);
    lo.alloc_slot(mask);
    lo.lower_data_node_tracked(inner);
    let bailout = lo.take_latched_bailout();
    assert!(
        bailout.is_none(),
        "the inner definition has a location: {bailout:?}"
    );
}
