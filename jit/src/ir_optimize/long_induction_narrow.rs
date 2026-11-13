// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT r27 LOOP27-2: a `long` counted induction variable as an `int` one
//! (`CRATONVM_JIT_IR_NARROW_LONG_INDUCTION`).
//!
//! # What
//!
//! `for (long i = 0; i < a.length; i++) s += a[(int) i]` builds a `long` header
//! phi, an `lcmp` test against `i2l(a.length)` and `l2i(i)` indices. Every
//! loop rule downstream (the range predicates, the unroller's margin proof, the
//! vector planner, the offset rules) reads an `int` induction, so the loop kept
//! four bounds checks and a poll per unrolled trip (`R22LoopsWave2 long-sum`
//! 1085 ns against HotSpot's 266, which strip-mines the long loop into an `int`
//! one).
//!
//! This pass is that strip-mine in the one shape where it is free: the phi
//! becomes an `int` phi, every other reader gets `i2l(phi)`, and the test is
//! the `int` compare of the same two values.
//!
//! # Why it is sound
//!
//! The header's only control successor is the test `t` (`t.ctrl == header`), and
//! the back edge is reachable from `t`'s stay edge only, so every increment runs
//! after `i < bound` held on the same trip. With `bound` an `i2l` of an `int`
//! `x`, `i <= x - 1 <= i32::MAX - 1` there; the new value is `i + step`, which
//! fits an `int` when `x - 1 + step <= i32::MAX`: any `step == 1`, and `step <=
//! 3` under an `arraylength` (`VM_MAX_ARRAY_LENGTH`), the constants BCE's own
//! no-wrap proof uses. The entry value is a constant in `0..=i32::MAX` and the
//! step is positive, so `i >= 0` and `i2l(phi) == i` on every trip, in every
//! frame that names it.
//!
//! # Frames
//!
//! The snapshots keep naming the `long` values: the phi's become `i2l(phi)`, the old increment's `i2l` of the
//! new one (`widened_next`), and the compare's `lcmp` and widened bound stay, with no pc, for the frames that
//! name them. The loop rules read those as frame-only nodes (`ir_vector_loop::frame_only_wide`, the unroller's
//! clone seeds), and a loop-entry frame reads the first one's entry value from `Graph::wide_entry`.
//! `l2i(i2l(phi))` is folded to the phi.
//!
//! # What it does not touch
//!
//! A phi of another shape (a variable start, `<=` or `!=` exit, a down-count, a
//! bound that is not an `i2l` / a constant), a loop with two back edges, a test
//! that is not first after the header (a `do-while`: its increment runs before
//! its test), and the OSR route (its entry values are not constants).

use super::{build_users, eliminate_dead_nodes, loop_headers};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op};
use rustc_hash::FxHashSet;

/// `Integer.MAX_VALUE - 2`, `ir_check_elim::VM_MAX_ARRAY_LENGTH`.
const MAX_ARRAY_LENGTH: i64 = i32::MAX as i64 - 2;

/// `CRATONVM_JIT_IR_NARROW_LONG_INDUCTION` (JIT r27 LOOP27-2; perf, default ON,
/// `=0` kills): see the module notes. Read per compile; no `static`.
pub(super) fn narrow_long_induction_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_NARROW_LONG_INDUCTION")
}

/// Narrow every loop of `graph` the module notes admit; how many.
pub(super) fn narrow_long_inductions(graph: &mut Graph) -> usize {
    if graph.osr_compile || graph.optimize_osr_route {
        return 0;
    }
    let mut done = 0;
    for (header, _entry, backs) in loop_headers(graph) {
        if let [back] = backs.as_slice() {
            if narrow_one(graph, header, *back) {
                done += 1;
            }
        }
    }
    if done > 0 {
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_UNROLL") {
            eprintln!("[DBG_UNROLL] narrowed {done} long induction(s) to int");
        }
        eliminate_dead_nodes(graph);
    }
    done
}

fn const_of(graph: &Graph, id: NodeId) -> Option<i64> {
    match graph.nodes.get(id as usize)?.op {
        Op::Const(c) => Some(c),
        _ => None,
    }
}

fn negate(cc: CmpOp) -> CmpOp {
    match cc {
        CmpOp::Lt => CmpOp::Ge,
        CmpOp::Ge => CmpOp::Lt,
        CmpOp::Le => CmpOp::Gt,
        CmpOp::Gt => CmpOp::Le,
        CmpOp::Eq => CmpOp::Ne,
        CmpOp::Ne => CmpOp::Eq,
    }
}

/// Control nodes reachable from `start` (itself included) without passing
/// through `avoid`.
fn reach_avoiding(graph: &Graph, users: &[Vec<NodeId>], start: NodeId, avoid: NodeId) -> FxHashSet<NodeId> {
    let mut seen = FxHashSet::default();
    seen.insert(start);
    let mut work = vec![start];
    while let Some(c) = work.pop() {
        for &u in &users[c as usize] {
            if u != avoid && graph.nodes[u as usize].op.is_control() && seen.insert(u) {
                work.push(u);
            }
        }
    }
    seen
}

/// The loop test `t` of `header` as `(t, phi, bound, stay_is_true)`: the stay
/// edge (`Proj(0)` when `stay_is_true`) is taken exactly when `phi < bound`
/// (`LCmp(phi, bound) < 0`).
fn stay_test(
    graph: &Graph,
    users: &[Vec<NodeId>],
    header: NodeId,
    back: NodeId,
) -> Option<(NodeId, NodeId, NodeId, bool)> {
    let mut ctrl = users[header as usize]
        .iter()
        .copied()
        .filter(|&u| graph.nodes[u as usize].op.is_control());
    let t = ctrl.next()?;
    if ctrl.next().is_some() {
        return None;
    }
    let tn = &graph.nodes[t as usize];
    if tn.op != Op::If || tn.inputs.first() != Some(&header) {
        return None;
    }
    let proj = |which: u16| {
        let mut it = users[t as usize]
            .iter()
            .copied()
            .filter(|&u| graph.nodes[u as usize].op == Op::Proj(which));
        let p = it.next()?;
        it.next().is_none().then_some(p)
    };
    let (p0, p1) = (proj(0)?, proj(1)?);
    let stays = |p: NodeId, other: NodeId| {
        reach_avoiding(graph, users, p, header).contains(&back)
            && !reach_avoiding(graph, users, other, header).contains(&back)
    };
    let stay_is_true = if stays(p0, p1) {
        true
    } else if stays(p1, p0) {
        false
    } else {
        return None;
    };
    let cond = graph.nodes.get(*tn.inputs.get(1)? as usize)?;
    let (cc, x, y) = match (&cond.op, cond.inputs.as_slice()) {
        (Op::Cmp(cc), [x, y]) => (*cc, *x, *y),
        _ => return None,
    };
    let lcmp = |v: NodeId| {
        let n = graph.nodes.get(v as usize)?;
        (n.op == Op::LCmp && n.inputs.len() == 2).then(|| (n.inputs[0], n.inputs[1]))
    };
    // `LCmp(a, b) cc 0` is `a cc b`; `0 cc LCmp(a, b)` is `b cc a`.
    let (l, r) = if const_of(graph, y) == Some(0) {
        lcmp(x)?
    } else if const_of(graph, x) == Some(0) {
        let (a, b) = lcmp(y)?;
        (b, a)
    } else {
        return None;
    };
    let stay_cc = if stay_is_true { cc } else { negate(cc) };
    match stay_cc {
        CmpOp::Lt => Some((t, l, r, stay_is_true)),
        CmpOp::Gt => Some((t, r, l, stay_is_true)),
        _ => None,
    }
}

fn narrow_one(graph: &mut Graph, header: NodeId, back: NodeId) -> bool {
    if !graph
        .nodes
        .iter()
        .any(|n| n.op == Op::Phi && n.ty == IrType::Long && n.inputs.first() == Some(&header))
    {
        return false;
    }
    let users = build_users(graph);
    let hn = &graph.nodes[header as usize];
    let Some(back_slot) = hn.inputs.iter().position(|&c| c == back) else {
        return false;
    };
    if hn.inputs.len() != 2 {
        return false;
    }
    let Some((test, phi, bound, stay_is_true)) = stay_test(graph, &users, header, back) else {
        return false;
    };
    let p = &graph.nodes[phi as usize];
    if p.op != Op::Phi || p.ty != IrType::Long || p.inputs.len() != 3 || p.inputs[0] != header {
        return false;
    }
    let (entry, next) = (p.inputs[2 - back_slot], p.inputs[1 + back_slot]);
    let Some(init) = const_of(graph, entry).filter(|c| (0..=i64::from(i32::MAX)).contains(c))
    else {
        return false;
    };
    let step = match graph.nodes.get(next as usize) {
        Some(n) if n.op == Op::Add && n.ty == IrType::Long => match n.inputs.as_slice() {
            [a, b] if *a == phi => const_of(graph, *b),
            [a, b] if *b == phi => const_of(graph, *a),
            _ => None,
        },
        _ => None,
    };
    let Some(step) = step.filter(|s| *s >= 1) else {
        return false;
    };
    // The bound as an `int` node, and the largest value it can take.
    let bn = &graph.nodes[bound as usize];
    let (bound_int, bound_max) = match (&bn.op, bn.inputs.as_slice()) {
        (Op::I2L, [x]) if graph.nodes[*x as usize].ty == IrType::Int => {
            let len = graph.nodes[*x as usize].op == Op::ArrayLength;
            (Some(*x), if len { MAX_ARRAY_LENGTH } else { i64::from(i32::MAX) })
        }
        (Op::Const(k), []) if (0..=i64::from(i32::MAX)).contains(k) => (None, *k),
        _ => return false,
    };
    if bound_max - 1 + step > i64::from(i32::MAX) {
        return false;
    }
    let (phi_pc, next_pc) = (graph.nodes[phi as usize].bytecode_pc, graph.nodes[next as usize].bytecode_pc);
    let cond = graph.nodes[test as usize].inputs[1];
    let (cond_ty, cond_pc) = (graph.nodes[cond as usize].ty, graph.nodes[cond as usize].bytecode_pc);
    let step_int = graph.add(Op::Const(step), IrType::Int, vec![], None);
    let init_int = graph.add(Op::Const(init), IrType::Int, vec![], None);
    let bound_int =
        bound_int.unwrap_or_else(|| graph.add(Op::Const(bound_max), IrType::Int, vec![], None));
    let narrow = graph.add(Op::Phi, IrType::Int, vec![header, init_int, init_int], phi_pc);
    let stepped = graph.add(Op::Add, IrType::Int, vec![narrow, step_int], next_pc);
    graph.set_input(narrow, 1 + back_slot, stepped);
    let widened = graph.add(Op::I2L, IrType::Long, vec![narrow], phi_pc);
    let init_long = graph.add(Op::Const(init), IrType::Long, vec![], None);
    graph.wide_entry.insert(widened, init_long);
    // `Proj(0)` is the true edge: the stay edge is `<`, the exit edge `>=`.
    let cc = if stay_is_true { CmpOp::Lt } else { CmpOp::Ge };
    let fresh = graph.add(Op::Cmp(cc), cond_ty, vec![narrow, bound_int], cond_pc);
    // The compare it replaces and the widened bound only frames read from here
    // on: with no pc they are no program point of the loop, so the unroller's
    // per-copy frame plan does not see the bci before `i2l` as one a node carries.
    let old_cmp_inputs: Vec<NodeId> = graph.nodes[cond as usize].inputs.as_slice().to_vec();
    graph.set_input(test, 1, fresh);
    for dead in old_cmp_inputs.into_iter().chain([bound]) {
        if matches!(graph.nodes[dead as usize].op, Op::LCmp | Op::I2L) {
            graph.nodes[dead as usize].bytecode_pc = None;
        }
    }
    graph.replace_all_uses(phi, widened);
    // The old increment survives in the frames taken at it: `i2l` of the new one,
    // a body node the unroller's per-copy frames rename.
    let widened_next = graph.add(Op::I2L, IrType::Long, vec![stepped], next_pc);
    graph.replace_all_uses(next, widened_next);
    // `(int) i` reads the phi itself: `l2i(i2l(x))` is `x`.
    let l2is: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.op == Op::L2I && n.inputs.first() == Some(&widened))
        .map(|(id, _)| id as NodeId)
        .collect();
    for l2i in l2is {
        graph.replace_all_uses(l2i, narrow);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `for (long i = init; i < bound; i += step) s += a[(int) i]` as the
    /// builder leaves it. `exit_on_true`: javac's `lcmp; ifge exit` (the true
    /// edge leaves); else the stay edge is the true one.
    fn long_loop(init: i64, step: i64, exit_on_true: bool, bound: &str) -> Graph {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let len = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, mem, arr], Some(1));
        let lim = match bound {
            "len" => g.add(Op::I2L, IrType::Long, vec![len], Some(2)),
            "int" => g.add(Op::I2L, IrType::Long, vec![n], Some(2)),
            _ => g.add(Op::Const(1000), IrType::Long, vec![], None),
        };
        let init = g.add(Op::Const(init), IrType::Long, vec![], None);
        let step = g.add(Op::Const(step), IrType::Long, vec![], None);
        let merge = g.add(Op::Merge, IrType::Control, vec![ctrl, ctrl], Some(4));
        let phi = g.add(Op::Phi, IrType::Long, vec![merge, init, init], Some(4));
        let lcmp = g.add(Op::LCmp, IrType::Int, vec![phi, lim], Some(5));
        let cc = if exit_on_true { CmpOp::Ge } else { CmpOp::Lt };
        let cond = g.add(Op::Cmp(cc), IrType::Int, vec![lcmp, zero], Some(6));
        let iff = g.add(Op::If, IrType::Control, vec![merge, cond], Some(6));
        let p0 = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(6));
        let p1 = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(6));
        let (stay, exit) = if exit_on_true { (p1, p0) } else { (p0, p1) };
        let idx = g.add(Op::L2I, IrType::Int, vec![phi], Some(8));
        let _load = g.add(Op::ArrayLoad(crate::ir::MemKind::Int), IrType::Int, vec![stay, mem, arr, idx], Some(8));
        let next = g.add(Op::Add, IrType::Long, vec![phi, step], Some(12));
        g.nodes[merge as usize].inputs[1] = stay;
        g.nodes[phi as usize].inputs[2] = next;
        g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(20));
        g
    }

    fn live_phis(g: &Graph, ty: IrType) -> usize {
        g.nodes.iter().filter(|n| n.op == Op::Phi && n.ty == ty).count()
    }

    #[test]
    fn a_long_counted_loop_over_an_array_length_becomes_an_int_loop() {
        for exit_on_true in [true, false] {
            let mut g = long_loop(0, 1, exit_on_true, "len");
            assert_eq!(narrow_long_inductions(&mut g), 1);
            assert_eq!((live_phis(&g, IrType::Long), live_phis(&g, IrType::Int)), (0, 1));
            // The test is the `int` compare of the same pair, same sense.
            let iff = g.nodes.iter().position(|n| n.op == Op::If).expect("the test");
            let cond = &g.nodes[g.nodes[iff].inputs[1] as usize];
            let want = if exit_on_true { CmpOp::Ge } else { CmpOp::Lt };
            assert_eq!(cond.op, Op::Cmp(want));
            assert_eq!(g.nodes[cond.inputs[0] as usize].op, Op::Phi);
            assert_eq!(g.nodes[cond.inputs[1] as usize].op, Op::ArrayLength);
            // The index is the narrowed phi itself: `l2i(i2l(phi))` folded.
            let load = g.nodes.iter().find(|n| matches!(n.op, Op::ArrayLoad(_))).expect("index");
            assert_eq!(g.nodes[load.inputs[3] as usize].op, Op::Phi);
        }
    }

    /// A loop-entry frame can read the entry value of `i2l(new phi)` from
    /// `wide_entry` (the node itself lives only while a frame names it).
    #[test]
    fn the_widened_phi_and_step_keep_frames_and_an_entry_value() {
        let mut g = long_loop(5, 3, true, "len");
        assert_eq!(narrow_long_inductions(&mut g), 1);
        let widened: Vec<_> = g.wide_entry.keys().copied().collect();
        assert_eq!(widened.len(), 1);
        let entry = &g.nodes[g.wide_entry[&widened[0]] as usize];
        assert_eq!((&entry.op, entry.ty), (&Op::Const(5), IrType::Long));
    }

    #[test]
    fn a_stride_of_three_under_an_array_length_and_one_under_an_int_bound_are_admitted() {
        assert_eq!(narrow_long_inductions(&mut long_loop(5, 3, true, "len")), 1);
        assert_eq!(narrow_long_inductions(&mut long_loop(0, 1, true, "int")), 1);
        assert_eq!(narrow_long_inductions(&mut long_loop(0, 1, true, "const")), 1);
    }

    /// The increment must not be able to leave `int`: a stride of two under an
    /// arbitrary `int` bound wraps at `i32::MAX`, a stride of four under an
    /// array length does; a negative or variable start is not `>= 0`.
    #[test]
    fn a_loop_whose_increment_could_wrap_or_whose_start_is_not_a_constant_is_left_alone() {
        for mut g in [
            long_loop(0, 2, true, "int"),
            long_loop(0, 4, true, "len"),
            long_loop(-1, 1, true, "len"),
            long_loop(i64::from(i32::MAX) + 1, 1, true, "len"),
            long_loop(0, 0, true, "len"),
        ] {
            assert_eq!(narrow_long_inductions(&mut g), 0);
            assert_eq!(live_phis(&g, IrType::Long), 1);
        }
    }

    /// `i <= bound` is not `i < bound`: the last trip runs at `bound` itself.
    #[test]
    fn an_inclusive_test_is_left_alone() {
        let mut g = long_loop(0, 1, true, "len");
        let iff = g.nodes.iter().position(|n| n.op == Op::If).expect("the test");
        let cond = g.nodes[iff].inputs[1];
        g.nodes[cond as usize].op = Op::Cmp(CmpOp::Gt); // exit on `lcmp > 0`: stays on `<=`
        assert_eq!(narrow_long_inductions(&mut g), 0);
    }
}
