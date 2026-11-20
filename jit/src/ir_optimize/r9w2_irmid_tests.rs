use super::*;
use crate::ir::{IrBuilder, SafepointSnapshot};

fn graph_with_start() -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    (g, start, ctrl)
}

fn snapshot(bci: usize, locals: Vec<NodeId>, stack: Vec<NodeId>) -> SafepointSnapshot {
    SafepointSnapshot::new(bci, locals, stack, Vec::new())
}

/// Run `f` with snapshot liveness armed on this thread, restoring it after.
fn with_snapshot_liveness<R>(f: impl FnOnce() -> R) -> R {
    SNAPSHOT_LIVENESS_ON.with(|c| c.set(true));
    let r = f();
    SNAPSHOT_LIVENESS_ON.with(|c| c.set(false));
    r
}

/// irlower R1: a pipeline that bailed between `optimize` and lowering
/// must not hand its `true` to the next compile on this thread.
#[test]
fn optimize_clears_a_stale_unreachable_frames_net() {
    UNROLL_USED_UNREACHABLE_FRAMES.with(|c| c.set(true));
    // iconst_1; ireturn -- nothing to unroll.
    let code = [0x04, 0xac, 0, 0];
    let mut g = IrBuilder::new(0, 0).build(&code, 2).expect("build");
    optimize(&mut g);
    assert!(
        !take_unroll_used_unreachable_frames(),
        "optimize must start from a cleared net, and set it only for a loop it transformed",
    );
}

/// `static int f(boolean s) { if (s) { while (true) {} } return 1; }`
///
/// Nothing in the loop is a DCE root: no memory op, no call, no guard,
/// and the loop phi of `s` is trivial and collapses. Before round 9 wave 2
/// the loop's `Merge` and the `If`'s fall-through projection were deleted
/// and the `If` survived with one live arm, so `f(true)` had no code for
/// its spin.
#[test]
fn an_effect_free_infinite_loop_keeps_its_control() {
    let code = [
        0x1a, // 0: iload_0
        0x99, 0x00, 0x06, // 1: ifeq +6 -> 7
        0xa7, 0x00, 0x00, // 4: goto +0 -> 4   (while (true) {})
        0x04, // 7: iconst_1
        0xac, // 8: ireturn
        0, 0, 0,
    ];
    let mut g = IrBuilder::new(1, 1).build(&code, 9).expect("build");
    optimize(&mut g);

    let if_id = g
        .nodes
        .iter()
        .position(|n| n.op == Op::If)
        .expect("the branch survives") as NodeId;
    let arms: Vec<NodeId> = g
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Proj(_)) && n.inputs.first() == Some(&if_id))
        .map(|(i, _)| i as NodeId)
        .collect();
    assert_eq!(arms.len(), 2, "both arms of the If survive DCE: {arms:?}");
    for &arm in &arms {
        assert!(
            g.nodes
                .iter()
                .any(|n| n.op != Op::Dead && n.inputs.iter().any(|&i| i == arm)),
            "arm n{arm} leads somewhere",
        );
    }
    assert!(
        g.nodes
            .iter()
            .any(|n| n.op == Op::Merge && n.bytecode_pc == Some(4)),
        "the spin loop's header survives",
    );
}

/// The same shape by hand, through `eliminate_dead_nodes` alone.
#[test]
fn dce_keeps_control_that_leads_to_no_root_but_is_reachable_from_start() {
    let (mut g, _start, ctrl) = graph_with_start();
    let s = g.add(Op::Param(0), IrType::Int, vec![], None);
    let if_node = g.add(Op::If, IrType::Control, vec![ctrl, s], None);
    let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    let spin = g.add(Op::Merge, IrType::Control, vec![t], None);
    g.nodes[spin as usize].inputs.push(spin);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let ret = g.add(Op::Return, IrType::Void, vec![f, one], None);
    g.exit = ret;
    // Control nothing reaches from Start is still reclaimed.
    let orphan = g.add(Op::Merge, IrType::Control, vec![], None);

    eliminate_dead_nodes(&mut g);

    for (id, what) in [
        (t, "the spin arm"),
        (spin, "the spin loop"),
        (f, "the exit arm"),
    ] {
        assert_ne!(g.nodes[id as usize].op, Op::Dead, "{what} must survive");
    }
    assert_eq!(
        g.nodes[orphan as usize].op,
        Op::Dead,
        "unreachable control is reclaimed"
    );
}

/// Which snapshots a deopt can consult: those at a trapping node's bci, a
/// guard's bci, and a join's bci -- and none at all when a trapping node's
/// bci has no snapshot (a spliced body's pc).
#[test]
fn consultable_snapshots_are_trap_and_join_bcis() {
    let (mut g, _start, ctrl) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![x, zero], Some(5));
    g.add(
        Op::Guard { bci: 5 },
        IrType::Void,
        vec![ctrl, cond],
        Some(5),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, x], Some(8));
    g.exit = ret;
    g.safepoints.push(snapshot(3, vec![x], vec![]));
    g.safepoints.push(snapshot(5, vec![x], vec![cond]));
    g.safepoints.push(snapshot(8, vec![x], vec![x]));
    assert_eq!(
        consultable_snapshots(&g),
        Some(vec![false, true, false]),
        "only the guard's bci can be resumed at",
    );

    g.add(Op::Merge, IrType::Control, vec![ctrl], Some(3));
    assert_eq!(
        consultable_snapshots(&g),
        Some(vec![true, true, false]),
        "a join is an OSR entry",
    );

    // A trapping node at a pc no snapshot describes cannot be attributed.
    g.add(Op::ArrayLength, IrType::Int, vec![x], Some(1000));
    assert_eq!(consultable_snapshots(&g), None);
}

/// Round 9 wave 3: with the build's splice resume map, a trapping node of a
/// spliced body is attributed to the snapshot at the OUTERMOST `invoke`
/// enclosing it -- where `ir_lower`'s `resume_bci` sends its deopt --
/// instead of making the whole graph unattributable. A join inside a body
/// marks its resume bci too.
#[test]
fn a_spliced_trap_is_attributed_to_its_outermost_invoke() {
    let (mut g, _start, ctrl) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, x], Some(8));
    g.exit = ret;
    g.safepoints.push(snapshot(2, vec![x], vec![x])); // top-level invoke A
    g.safepoints.push(snapshot(5, vec![x], vec![x])); // top-level invoke B
    g.safepoints.push(snapshot(8, vec![x], vec![x]));
    // Caller code is 10 bytes. A's body is [10, 20) and holds an invoke at
    // combined pc 13 whose body is [20, 26); B's body is [26, 30).
    let map = crate::ir::IrSpliceResumeMap {
        code_len: 10,
        bodies: vec![(10, 10, 2), (20, 6, 13), (26, 4, 5)],
    };
    // A trap two splices deep resumes at A.
    g.add(Op::ArrayLength, IrType::Int, vec![x], Some(22));
    assert_eq!(
        consultable_snapshots_with(&g, None),
        None,
        "without the map a spliced pc is unattributable, as in wave 2",
    );
    assert_eq!(
        consultable_snapshots_with(&g, Some(&map)),
        Some(vec![true, false, false]),
    );
    // A join inside B's body makes B's snapshot consultable too.
    g.add(Op::Merge, IrType::Control, vec![ctrl], Some(27));
    assert_eq!(
        consultable_snapshots_with(&g, Some(&map)),
        Some(vec![true, true, false]),
    );
    // A pc past the caller's code that lies in no body is still refused.
    g.add(Op::ArrayLength, IrType::Int, vec![x], Some(40));
    assert_eq!(consultable_snapshots_with(&g, Some(&map)), None);
}

/// With liveness armed, a value only an UNCONSULTABLE snapshot names is
/// reclaimed and the slot reads "undefined"; a value a consultable one
/// names is kept. Unarmed, both are kept (the pre-wave-2 rule).
#[test]
fn snapshot_liveness_roots_only_consultable_snapshots() {
    let build = || {
        let (mut g, _start, ctrl) = graph_with_start();
        let x = g.add(Op::Param(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let y = g.add(Op::Add, IrType::Int, vec![x, one], Some(2));
        let z = g.add(Op::Sub, IrType::Int, vec![x, one], Some(4));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![x, zero], Some(7));
        g.add(
            Op::Guard { bci: 7 },
            IrType::Void,
            vec![ctrl, cond],
            Some(7),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, x], Some(9));
        g.exit = ret;
        g.safepoints.push(snapshot(2, vec![x, y], vec![]));
        g.safepoints.push(snapshot(7, vec![x, z], vec![cond]));
        (g, y, z)
    };

    let (mut plain, y, _) = build();
    eliminate_dead_nodes(&mut plain);
    assert_ne!(
        plain.nodes[y as usize].op,
        Op::Dead,
        "unarmed: every snapshot is a root"
    );

    let (mut g, y, z) = build();
    with_snapshot_liveness(|| eliminate_dead_nodes(&mut g));
    assert_eq!(
        g.nodes[y as usize].op,
        Op::Dead,
        "armed: bci 2 cannot be resumed at"
    );
    assert_eq!(
        g.safepoints[0].locals[1], NO_NODE,
        "and its slot now reads undefined"
    );
    assert_ne!(
        g.nodes[z as usize].op,
        Op::Dead,
        "the guard's frame keeps what it names"
    );
    assert_eq!(g.safepoints[1].locals[1], z);
}

/// The wave-1 regression (`a_store_into_an_allocation_a_safepoint_names_is_not_write_only`)
/// with liveness ARMED and the pruned-branch trap as the only deopt site
/// that names the allocation: the store must still be kept. And the
/// converse -- named only at a bci nothing can resume at -- is write-only.
#[test]
fn write_only_dse_under_liveness_still_honours_the_trap_frame() {
    let build = |trap_names_alloc: bool| {
        let (mut g, start, ctrl) = graph_with_start();
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![start],
            Some(0),
        );
        let five = g.add(Op::Const(5), IrType::Int, vec![], None);
        let store = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![alloc, five],
            Some(4),
        );
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        g.add(
            Op::Guard { bci: 9 },
            IrType::Void,
            vec![ctrl, zero],
            Some(9),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(12));
        g.exit = ret;
        g.safepoints.push(snapshot(0, vec![NO_NODE], vec![]));
        g.safepoints.push(snapshot(4, vec![NO_NODE], vec![]));
        g.safepoints.push(snapshot(6, vec![alloc], vec![]));
        let at_trap = if trap_names_alloc { alloc } else { NO_NODE };
        g.safepoints.push(snapshot(9, vec![at_trap], vec![]));
        (g, store)
    };

    let (mut g, store) = build(true);
    with_snapshot_liveness(|| eliminate_dead_stores(&mut g));
    assert_eq!(
        g.nodes[store as usize].op,
        Op::Store(MemKind::Int),
        "the trap at bci 9 resumes with the allocation in its frame",
    );

    let (mut g, store) = build(false);
    with_snapshot_liveness(|| eliminate_dead_stores(&mut g));
    assert_eq!(
        g.nodes[store as usize].op,
        Op::Dead,
        "named only at bci 6, which nothing can resume at: write-only",
    );

    let (mut g, store) = build(false);
    eliminate_dead_stores(&mut g);
    assert_eq!(
        g.nodes[store as usize].op,
        Op::Store(MemKind::Int),
        "unarmed, every snapshot still disqualifies",
    );
}
