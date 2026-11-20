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
