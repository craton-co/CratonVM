use super::*;
use crate::ir::{Graph, IrType, Op, SafepointSnapshot, NO_NODE};

/// One in-loop guard whose frame names two params (one twice), a constant
/// and an undefined slot: two live values. An out-of-loop guard with a
/// bigger frame is not counted, and a guard with no snapshot is
/// `unresolved`, never a guess.
#[test]
fn counts_distinct_non_constant_values_of_in_loop_guards_only() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let b = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let c = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let k = g.add(Op::Const(7), IrType::Int, vec![], None);
    let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
    let in_loop = g.add(
        Op::Guard { bci: 10 },
        IrType::Void,
        vec![ctrl, cond],
        Some(10),
    );
    let outside = g.add(
        Op::Guard { bci: 20 },
        IrType::Void,
        vec![ctrl, cond],
        Some(20),
    );
    let lost = g.add(
        Op::Guard { bci: 30 },
        IrType::Void,
        vec![ctrl, cond],
        Some(30),
    );
    g.push_safepoint(SafepointSnapshot::new(
        10,
        vec![a, b, k, NO_NODE],
        vec![a],
        vec![],
    ));
    g.push_safepoint(SafepointSnapshot::new(20, vec![a, b, c], vec![], vec![b]));

    let blocks: Vec<(u32, Vec<u32>)> =
        vec![(0, vec![outside]), (1, vec![in_loop]), (2, vec![lost])];
    let census = guard_frame_census_over(&g, blocks.iter().map(|(d, n)| (*d, n.as_slice())));
    assert_eq!(census.in_loop_guards, 2);
    assert_eq!(census.max_live, 2);
    assert_eq!(census.max_live_bci, Some(10));
    assert_eq!(census.unresolved, 1);
    assert_eq!(
        census.describe(),
        "guard-frame in-loop=2 max-live=2@10 unresolved=1"
    );
}

#[test]
fn no_loop_is_the_empty_census() {
    let mut g = Graph::empty(4);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
    let guard = g.add(
        Op::Guard { bci: 0 },
        IrType::Void,
        vec![start, cond],
        Some(0),
    );
    let blocks = [vec![guard]];
    let census = guard_frame_census_over(&g, blocks.iter().map(|n| (0, n.as_slice())));
    assert_eq!(census, GuardFrameCensus::default());
    assert_eq!(census.describe(), "guard-frame in-loop=0 max-live=0");
}

/// j27 wave 7 (lane T1ACCEPT, ACCEPT21-5's call half): a Java call left in
/// a loop is counted and named; one outside every loop is not.
#[test]
fn counts_java_calls_left_in_loops() {
    let mut g = Graph::empty(8);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let in_loop = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![start], Some(4));
    let outside = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![start], Some(9));
    let blocks: Vec<(u32, Vec<u32>)> = vec![(0, vec![outside]), (1, vec![in_loop])];
    let census = guard_frame_census_over(&g, blocks.iter().map(|(d, n)| (*d, n.as_slice())));
    assert_eq!(census.in_loop_calls, 1);
    assert_eq!(census.in_loop_guards, 0);
    assert_eq!(census.describe(), "guard-frame in-loop=0 max-live=0 calls-in-loop=1");
}
