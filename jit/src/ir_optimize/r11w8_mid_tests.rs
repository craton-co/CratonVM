use super::*;
use crate::deopt::DespecRegistry;
use crate::ir::MemKind;

const KEY: &str = "T.sum:([II)I";

/// The loop shapes proposal W7-2 adds to range predication.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// `for (i = 0; i <= n; i++)`: `if_icmpgt` exit, `Cmp(Gt)[i, n]`.
    UpTo,
    /// `for (i = n; i >= 0; i--)`: `iflt` exit, `Cmp(Lt)[i, 0]`.
    DownFromN,
    /// `for (i = a.length - 1; i >= 0; i--)`.
    DownFromLengthMinusOne,
    /// `for (i = n; i >= -1; i--)`: reaches `a[-1]`.
    DownToMinusOne,
}

struct Fixture {
    g: Graph,
    entry: NodeId,
    mem0: NodeId,
    arr: NodeId,
    n: NodeId,
    load: NodeId,
}

/// `s += a[i]` in the loop `shape` names, laid out as in
/// `r11w7_mid_tests::sum_to_n`: header merge and φs at bci 10 with a
/// snapshot there, the exit test at bci 12 exiting on its TRUE edge, the
/// access on the stay edge at bci 14.
fn looped(shape: Shape) -> Fixture {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let init = match shape {
        Shape::UpTo => zero,
        Shape::DownFromN | Shape::DownToMinusOne => n,
        Shape::DownFromLengthMinusOne => {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], Some(3));
            let one = g.add(Op::Const(1), IrType::Int, vec![], None);
            g.add(Op::Sub, IrType::Int, vec![len, one], Some(5))
        }
    };
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
    let iv = g.add(Op::Phi, IrType::Int, vec![region, init], Some(10));
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0], Some(10));
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let cond = match shape {
        Shape::UpTo => g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![iv, n], Some(12)),
        Shape::DownFromN | Shape::DownFromLengthMinusOne => {
            g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, zero], Some(12))
        }
        Shape::DownToMinusOne => {
            let m1 = g.add(Op::Const(-1), IrType::Int, vec![], None);
            g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, m1], Some(12))
        }
    };
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let load = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![stay, mem_phi, arr, iv],
        Some(14),
    );
    let step = if shape == Shape::UpTo { 1 } else { -1 };
    let stride = g.add(Op::Const(step), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, stride], Some(18));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[region as usize].inputs.push(stay);
    g.exit = g.add(Op::Return, IrType::Int, vec![exit, iv], None);
    g.safepoints
        .push(SafepointSnapshot::new(10, vec![arr, n, iv], vec![], vec![]));
    g.safepoints.push(SafepointSnapshot::new(
        14,
        vec![arr, n, iv],
        vec![arr, iv],
        vec![],
    ));
    Fixture {
        g,
        entry: c0,
        mem0: m0,
        arr,
        n,
        load,
    }
}

fn only_guard(g: &Graph) -> &Node {
    let gs: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Guard { .. }))
        .collect();
    assert_eq!(gs.len(), 1, "exactly one guard");
    gs[0]
}

fn range_proved(f: &Fixture) -> bool {
    let e = crate::ir_check_elim::analyze(&f.g, &crate::ir_schedule::schedule(&f.g));
    e.bounds_elided(f.load) && e.bounds_range_proved(f.load)
}

/// `i <= n` gets the STRICT guard `n < a.length`, and with it the access
/// loses its check.
#[test]
fn an_inclusive_loop_gets_a_strict_predicate() {
    let mut f = looped(Shape::UpTo);
    assert!(!range_proved(&f), "no fact before the predicate");
    assert_eq!(
        plant_range_check_predicates(&mut f.g, &DespecRegistry::new(), KEY),
        1
    );
    let guard = only_guard(&f.g);
    assert_eq!(guard.op, Op::Guard { bci: 10 });
    assert_eq!(guard.inputs[0], f.entry);
    let cond = &f.g.nodes[guard.inputs[1] as usize];
    assert_eq!(cond.op, Op::Cmp(CmpOp::Lt), "`n <= a.length` is not enough");
    assert_eq!(cond.inputs[0], f.n);
    let len = &f.g.nodes[cond.inputs[1] as usize];
    assert_eq!(len.inputs.as_slice(), &[f.entry, f.mem0, f.arr]);
    assert!(range_proved(&f));
}

/// A down count from `n` gets `n < a.length` on its START, resuming with
/// the φ at that start.
#[test]
fn a_down_count_gets_a_predicate_on_its_start() {
    let mut f = looped(Shape::DownFromN);
    assert!(!range_proved(&f), "no fact before the predicate");
    assert_eq!(
        plant_range_check_predicates(&mut f.g, &DespecRegistry::new(), KEY),
        1
    );
    let guard = only_guard(&f.g);
    let cond = &f.g.nodes[guard.inputs[1] as usize];
    assert_eq!(cond.op, Op::Cmp(CmpOp::Lt));
    assert_eq!(cond.inputs[0], f.n);
    let si = guard.frame_snapshot.expect("the guard names its frame");
    assert_eq!(
        f.g.safepoints[si as usize].locals,
        vec![f.arr, f.n, f.n],
        "the φ at its entry value"
    );
    assert!(range_proved(&f));
}

/// `a.length - 1` is below the length already: no guard, and no check.
#[test]
fn a_down_count_from_length_minus_one_needs_no_predicate() {
    let mut f = looped(Shape::DownFromLengthMinusOne);
    assert!(range_proved(&f));
    assert_eq!(
        plant_range_check_predicates(&mut f.g, &DespecRegistry::new(), KEY),
        0
    );
}

/// A down count that runs to `-1` indexes `a[-1]`: nothing planted, and
/// the check stays.
#[test]
fn a_down_count_to_minus_one_gets_nothing() {
    let mut f = looped(Shape::DownToMinusOne);
    assert_eq!(
        plant_range_check_predicates(&mut f.g, &DespecRegistry::new(), KEY),
        0
    );
    assert!(!range_proved(&f));
}
