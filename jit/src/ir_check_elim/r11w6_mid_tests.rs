use super::*;
use crate::ir::{IrType, NO_NODE};
use crate::ir_schedule;

/// What runs in front of the loop.
enum Prelude {
    Nothing,
    /// `if (n > a.length) ..`, with the loop on its false edge (where
    /// `n <= a.length`) or, for the negative twin, on its true edge.
    IfEdge {
        loop_on_true_edge: bool,
    },
    /// `Guard(n <op> a.length)` in the entry block.
    Guard(CmpOp),
}

/// The array the body stores into, and the loop bound that goes with it.
enum Target {
    /// `a[i]`, loop bound `n`.
    Param,
    /// `b = new int[n]; b[i]`, loop bound `n`.
    NewOfBound,
    /// `b = new int[m]; b[i]`, loop bound `n`.
    NewOfOther,
    /// `b = new int[a.length]; b[i]`, loop bound a SECOND `a.length` read.
    NewOfLength,
    /// `b = new int[8]; b[i]`, loop bound a second `8` node.
    NewOfConst,
    /// `a[i]`, loop bound `Math.min(n, a.length)`.
    ParamUnderMin,
    /// `a[i]`, loop bound `Math.max(n, a.length)`: proves nothing.
    ParamUnderMax,
}

/// `[prelude] for (int i = 0; i < bound; i++) t[i] = 1;`, top-tested with
/// the body on the true edge (which is also the back edge). Returns
/// `(graph, schedule, the ArrayStore)`.
fn loop_over(prelude: Prelude, target: Target) -> (Graph, Schedule, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let m = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let entry = match prelude {
        Prelude::Nothing => c0,
        Prelude::IfEdge { loop_on_true_edge } => {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
            let gt = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![n, len], None);
            let pif = g.add(Op::If, IrType::Control, vec![c0, gt], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![pif], None);
            let f = g.add(Op::Proj(1), IrType::Control, vec![pif], None);
            if loop_on_true_edge {
                t
            } else {
                f
            }
        }
        Prelude::Guard(op) => {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
            let cmp = g.add(Op::Cmp(op), IrType::Int, vec![n, len], None);
            let _ = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, cmp], Some(0));
            c0
        }
    };
    let new_array = |g: &mut Graph, count: NodeId| {
        g.add(
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
            IrType::Ref,
            vec![entry, m0, count],
            None,
        )
    };
    let target_arr = match target {
        Target::Param | Target::ParamUnderMin | Target::ParamUnderMax => arr,
        Target::NewOfBound => new_array(&mut g, n),
        Target::NewOfOther => new_array(&mut g, m),
        Target::NewOfLength => {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![entry, m0, arr], None);
            new_array(&mut g, len)
        }
        Target::NewOfConst => {
            let eight = g.add(Op::Const(8), IrType::Int, vec![], None);
            new_array(&mut g, eight)
        }
    };
    let region = g.add(Op::Merge, IrType::Control, vec![entry], None);
    let bound = match target {
        Target::NewOfLength => g.add(Op::ArrayLength, IrType::Int, vec![region, m0, arr], None),
        Target::NewOfConst => g.add(Op::Const(8), IrType::Int, vec![], None),
        Target::ParamUnderMin | Target::ParamUnderMax => {
            let len = g.add(Op::ArrayLength, IrType::Int, vec![region, m0, arr], None);
            let op = if matches!(target, Target::ParamUnderMin) {
                crate::ir::ScalarOp::MinI
            } else {
                crate::ir::ScalarOp::MaxI
            };
            g.add(Op::ScalarIntrinsic(op), IrType::Int, vec![n, len], None)
        }
        _ => n,
    };
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(next);
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    g.nodes[region as usize].inputs.push(body);
    let st = g.add(
        Op::ArrayStore(MemKind::Int),
        IrType::Void,
        vec![body, m0, target_arr, iv, one],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, iv], Some(1));
    let sched = ir_schedule::schedule(&g);
    (g, sched, st)
}

fn range_proved(prelude: Prelude, target: Target) -> bool {
    let (g, sched, st) = loop_over(prelude, target);
    let e = analyze(&g, &sched);
    e.bounds_elided(st) && e.bounds_range_proved(st)
}

/// `if (n > a.length) ..; for (i = 0; i < n; i++) a[i] = 1;` -- the loop
/// runs where `n <= a.length`, so `i < n` is `i < a.length`.
#[test]
fn a_length_precheck_bounds_a_loop_over_n() {
    assert!(range_proved(
        Prelude::IfEdge {
            loop_on_true_edge: false
        },
        Target::Param
    ));
}

/// The same loop on the OTHER edge, where `n > a.length`: `a[n - 1]` is
/// out of bounds and the check must stay.
#[test]
fn the_wrong_edge_of_the_precheck_proves_nothing() {
    assert!(!range_proved(
        Prelude::IfEdge {
            loop_on_true_edge: true
        },
        Target::Param
    ));
}

/// Without a precheck, `i < n` says nothing about `a`: the check stays,
/// and the census still reads "no length test".
#[test]
fn a_loop_over_n_without_a_fact_keeps_its_check() {
    let (g, sched, st) = loop_over(Prelude::Nothing, Target::Param);
    let e = analyze(&g, &sched);
    assert!(!e.bounds_elided(st));
    assert_eq!(e.bounds_refusal(st), REFUSAL_NO_LENGTH_TEST);
}

/// A guard that deopts unless `n <= a.length` (or `n < a.length`) proves
/// the same as the precheck edge.
#[test]
fn a_guard_n_at_most_length_bounds_a_loop_over_n() {
    for op in [CmpOp::Le, CmpOp::Lt] {
        assert!(range_proved(Prelude::Guard(op), Target::Param), "{op:?}");
    }
}

/// `Guard(n >= a.length)` bounds `n` from BELOW, and `Eq`/`Ne` order
/// nothing: nothing proven.
#[test]
fn a_guard_the_other_way_proves_nothing() {
    for op in [CmpOp::Ge, CmpOp::Gt, CmpOp::Eq, CmpOp::Ne] {
        assert!(!range_proved(Prelude::Guard(op), Target::Param), "{op:?}");
    }
}

/// `b = new int[n]; for (i < n) b[i]` -- `b.length == n` for `b`'s life.
#[test]
fn an_array_allocated_with_the_bound_is_in_bounds() {
    assert!(range_proved(Prelude::Nothing, Target::NewOfBound));
}

/// `b = new int[a.length]; for (i < a.length) b[i]` with two separate
/// length reads of `a`: the same array, so the same length.
#[test]
fn an_array_allocated_with_another_arrays_length_is_in_bounds() {
    assert!(range_proved(Prelude::Nothing, Target::NewOfLength));
}

/// Two `8` constants are one length.
#[test]
fn an_array_allocated_with_the_constant_bound_is_in_bounds() {
    assert!(range_proved(Prelude::Nothing, Target::NewOfConst));
}

/// `for (i < Math.min(n, a.length)) a[i]`: the minimum is at most the
/// length. `Math.max` is not.
#[test]
fn a_bound_under_math_min_of_the_length_is_in_bounds() {
    assert!(range_proved(Prelude::Nothing, Target::ParamUnderMin));
    assert!(!range_proved(Prelude::Nothing, Target::ParamUnderMax));
}

/// `b = new int[m]; for (i < n) b[i]`: a different count proves nothing.
#[test]
fn an_array_allocated_with_a_different_count_keeps_its_check() {
    assert!(!range_proved(Prelude::Nothing, Target::NewOfOther));
}

/// `CRATONVM_JIT_IR_BCE_RANGE=0` drops these facts with the rest of the
/// range proof.
#[test]
fn the_range_switch_drops_the_new_facts() {
    let _off = RangeForce::off();
    assert!(!range_proved(
        Prelude::IfEdge {
            loop_on_true_edge: false
        },
        Target::Param
    ));
    assert!(!range_proved(Prelude::Nothing, Target::NewOfBound));
}
