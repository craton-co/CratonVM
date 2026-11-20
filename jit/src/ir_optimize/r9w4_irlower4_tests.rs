use super::*;

fn with_snapshot_liveness<R>(f: impl FnOnce() -> R) -> R {
    SNAPSHOT_LIVENESS_ON.with(|c| c.set(true));
    let r = f();
    SNAPSHOT_LIVENESS_ON.with(|c| c.set(false));
    r
}

fn snapshot(bci: usize, locals: Vec<NodeId>, stack: Vec<NodeId>) -> SafepointSnapshot {
    SafepointSnapshot::new(bci, locals, stack, Vec::new())
}

/// `p = new P(); p.f = 5; <guard at 9>; return`, in the LOWERED store
/// layout `[ctrl, mem, base, offset, value]`. The snapshot at the
/// `putfield` (bci 4) names `p` on its operand stack, as the builder's
/// does. `const_offset == false` makes the offset a non-constant, which
/// the `Op::Store` arm deopts on.
fn build(const_offset: bool) -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl],
        Some(0),
    );
    let offset = if const_offset {
        g.add(Op::Const(0), IrType::Int, vec![], None)
    } else {
        g.add(Op::Param(0), IrType::Int, vec![start], None)
    };
    let five = g.add(Op::Const(5), IrType::Int, vec![], None);
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, start, alloc, offset, five],
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
    g.safepoints
        .push(snapshot(4, vec![alloc], vec![alloc, five]));
    g.safepoints.push(snapshot(9, vec![NO_NODE], vec![]));
    (g, store)
}

#[test]
fn a_store_into_a_fresh_allocation_is_not_a_transfer_site() {
    let (g, store) = build(true);
    assert!(crate::ir_lower::node_cannot_deopt(
        &g,
        &g.nodes[store as usize]
    ));
    let mask = consultable_snapshots_with(&g, None).expect("attributable");
    assert_eq!(
        mask,
        vec![true, false, true],
        "bci 4 holds only the store, and the store emits no deopt",
    );

    let (g, store) = build(false);
    assert!(
        !crate::ir_lower::node_cannot_deopt(&g, &g.nodes[store as usize]),
        "a non-constant offset takes the arm's unconditional deopt",
    );
    let mask = consultable_snapshots_with(&g, None).expect("attributable");
    assert_eq!(mask, vec![true, true, true]);
}

#[test]
fn write_only_dse_under_liveness_reaches_a_store_into_a_fresh_allocation() {
    let (mut g, store) = build(true);
    with_snapshot_liveness(|| eliminate_write_only_stores(&mut g));
    assert_eq!(
        g.nodes[store as usize].op,
        Op::Dead,
        "named only by the store's own snapshot, which nothing can resume at",
    );

    let (mut g, store) = build(true);
    eliminate_write_only_stores(&mut g);
    assert_eq!(
        g.nodes[store as usize].op,
        Op::Store(MemKind::Int),
        "unarmed, every snapshot still disqualifies",
    );

    let (mut g, store) = build(false);
    with_snapshot_liveness(|| eliminate_write_only_stores(&mut g));
    assert_eq!(
        g.nodes[store as usize].op,
        Op::Store(MemKind::Int),
        "a store that can deopt keeps its own snapshot consultable",
    );
}

/// Two chained stores into one fresh allocation: the first one's node is
/// the second one's memory token. Under liveness both go, the chain closed
/// over them; unarmed, the `uses == 0` rule keeps the first (and step 2b
/// keeps both, since every snapshot then disqualifies the allocation).
#[test]
fn write_only_dse_under_liveness_splices_a_chained_store() {
    let build = || {
        let mut g = Graph::empty(0);
        g.exit = 0;
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 2,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(0),
        );
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let five = g.add(Op::Const(5), IrType::Int, vec![], None);
        let first = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, alloc, f0, five],
            Some(4),
        );
        let second = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, first, alloc, f1, five],
            Some(8),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(12));
        g.exit = ret;
        g.safepoints.push(snapshot(0, vec![NO_NODE], vec![]));
        g.safepoints
            .push(snapshot(4, vec![alloc], vec![alloc, five]));
        g.safepoints
            .push(snapshot(8, vec![alloc], vec![alloc, five]));
        (g, first, second)
    };

    let (mut g, first, second) = build();
    with_snapshot_liveness(|| eliminate_write_only_stores(&mut g));
    assert_eq!(g.nodes[first as usize].op, Op::Dead);
    assert_eq!(g.nodes[second as usize].op, Op::Dead);

    let (mut g, first, second) = build();
    eliminate_write_only_stores(&mut g);
    assert_eq!(g.nodes[first as usize].op, Op::Store(MemKind::Int));
    assert_eq!(g.nodes[second as usize].op, Op::Store(MemKind::Int));
}
