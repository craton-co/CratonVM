use super::*;
use crate::deopt::DespecRegistry;
use crate::ir::{CmpOp, IrType, MemKind, SafepointSnapshot};

const KEY: &str = "T.f:(I)I";

/// `for (i = 0; i < n; i++) s += TL.get() == null ? 1 : 0;` with the
/// trap-form chain built as the builder builds it (class guard, thread,
/// `threadLocals` null guard, `table`, length, hash, one probe with an
/// `If` on the referent and a second probe whose miss is a guard, the
/// join, `value`), `TL` a parameter. With `store`, the body also writes
/// a field. Returns `(graph, region, entry pred, value)`.
fn tl_loop(store: bool) -> (Graph, NodeId, NodeId, NodeId) {
    tl_loop_with(store, false)
}

/// [`tl_loop`]; with `set`, the chain ends in `e.value = TL` (a `set`'s
/// store) instead of the value read, and the returned node is the store.
fn tl_loop_with(store: bool, set: bool) -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let tl = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let s = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let mem = g.add(Op::Phi, IrType::Memory, vec![region, m0], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    // The chain at the invoke pc 16.
    let pc = 16;
    let first = g.nodes.len() as NodeId;
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let guard = |g: &mut Graph, ctrl: NodeId, cond: NodeId| {
        g.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cond],
            Some(pc),
        );
    };
    let cls = g.add(
        Op::ExactClassIs { class_id: 40 },
        IrType::Int,
        vec![tl],
        Some(pc),
    );
    guard(&mut g, stay, cls);
    let t = g.add(
        Op::CurrentThreadMirror { offset: 64 },
        IrType::Ref,
        vec![stay],
        Some(pc),
    );
    let t_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![t, null], Some(pc));
    guard(&mut g, stay, t_ok);
    let i5 = g.add(Op::Const(5), IrType::Int, vec![], None);
    let map = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![stay, mem, t, i5],
        Some(pc),
    );
    let map_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![map, null], Some(pc));
    guard(&mut g, stay, map_ok);
    let tab = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![stay, map, map, one],
        Some(pc),
    );
    let len = g.add(Op::ArrayLength, IrType::Int, vec![stay, tab, tab], Some(pc));
    let len_ok = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![len, zero], Some(pc));
    guard(&mut g, stay, len_ok);
    let h = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![stay, tab, tl, zero],
        Some(pc),
    );
    let mask = g.add(Op::Sub, IrType::Int, vec![len, one], Some(pc));
    let i0 = g.add(Op::And, IrType::Int, vec![h, mask], Some(pc));
    let e0 = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![stay, h, tab, i0],
        Some(pc),
    );
    let e0_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![e0, null], Some(pc));
    guard(&mut g, stay, e0_ok);
    let r0 = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![stay, e0, e0, zero],
        Some(pc),
    );
    let same0 = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![r0, tl], Some(pc));
    let probe = g.add(Op::If, IrType::Control, vec![stay, same0], Some(pc));
    let found = g.add(Op::Proj(0), IrType::Control, vec![probe], Some(pc));
    let other = g.add(Op::Proj(1), IrType::Control, vec![probe], Some(pc));
    let r0_live = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![r0, null], Some(pc));
    guard(&mut g, other, r0_live);
    let i1 = g.add(Op::And, IrType::Int, vec![i0, mask], Some(pc));
    let e1 = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![other, r0, tab, i1],
        Some(pc),
    );
    let e1_ok = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![e1, null], Some(pc));
    guard(&mut g, other, e1_ok);
    let r1 = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![other, e1, e1, zero],
        Some(pc),
    );
    let same1 = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![r1, tl], Some(pc));
    guard(&mut g, other, same1);
    let join = g.add(Op::Merge, IrType::Control, vec![found, other], Some(pc));
    let mphi = g.add(Op::Phi, IrType::Memory, vec![join, r0, r1], Some(pc));
    let e = g.add(Op::Phi, IrType::Ref, vec![join, e0, e1], Some(pc));
    let value = if set {
        g.add(
            Op::Store(MemKind::Ref),
            IrType::Memory,
            vec![join, mphi, e, i5, tl],
            Some(pc),
        )
    } else {
        g.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![join, mphi, e, i5],
            Some(pc),
        )
    };
    let end = g.nodes.len() as NodeId;
    g.tl_chains.push(TlChainRecord {
        recv: tl,
        entry_ctrl: stay,
        entry_mem: mem,
        hit_ctrl: join,
        value: if set { NO_NODE } else { value },
        store: if set { value } else { NO_NODE },
        first,
        end,
        pc,
    });
    let isnull = if set {
        one
    } else {
        g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![value, null], Some(19))
    };
    let sum = g.add(Op::Add, IrType::Int, vec![s, isnull], Some(20));
    let mut back_mem = value;
    if store {
        back_mem = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![join, value, tl, zero, sum],
            Some(21),
        );
    }
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(22));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[s as usize].inputs.push(sum);
    g.nodes[mem as usize].inputs.push(back_mem);
    g.nodes[region as usize].inputs.push(join);
    let ret = g.add(Op::Return, IrType::Int, vec![exit, s], None);
    g.exit = ret;
    g.safepoints.push(SafepointSnapshot::new(
        10,
        vec![tl, n, iv, s],
        vec![],
        vec![],
    ));
    g.safepoints.push(SafepointSnapshot::new(
        16,
        vec![tl, n, iv, s],
        vec![tl],
        vec![],
    ));
    let after = if set { Vec::new() } else { vec![value] };
    g.safepoints.push(SafepointSnapshot::new(
        19,
        vec![tl, n, iv, s],
        after,
        vec![],
    ));
    (g, region, c0, value)
}

fn plant(g: &mut Graph, despec: &DespecRegistry) -> usize {
    cratonvm_types::flags::with_thread_overrides(&[], || {
        plant_tl_chain_predicates(g, despec, KEY, false)
    })
}

/// The chain moves: the header is entered from the chain's join, the
/// chain starts on the old entry edge, every guard is a header predicate
/// with the loop-entry frame (`i` and `s` at `0`), the array reads name
/// it too, and the body's memory no longer runs through the value read.
#[test]
fn tlget21_3_the_chain_moves_to_the_pre_header() {
    let (mut g, region, c0, value) = tl_loop(false);
    let rec = g.tl_chains[0].clone();
    assert_eq!(plant(&mut g, &DespecRegistry::new()), 1);
    assert!(g.nodes[region as usize].inputs.contains(&rec.hit_ctrl));
    assert!(!g.nodes[region as usize].inputs.contains(&c0));
    let guards: Vec<NodeId> = (rec.first..rec.end)
        .filter(|&id| matches!(g.nodes[id as usize].op, Op::Guard { .. }))
        .collect();
    assert_eq!(guards.len(), 8);
    for &gd in &guards {
        let n = &g.nodes[gd as usize];
        assert_eq!(n.op, Op::Guard { bci: 10 });
        let si = n.frame_snapshot.expect("a named frame");
        let sp = &g.safepoints[si as usize];
        assert_eq!(sp.bci, 10);
        assert!(!sp.locals.iter().any(|&v| g.nodes[v as usize].op == Op::Phi));
        if crate::deopt::predicate_own_speculation_enabled() {
            assert!(g
                .predicate_guard_kinds
                .contains(&(gd, crate::deopt::PredicateKind::ThreadLocal as u8)));
        }
    }
    for id in rec.first..rec.end {
        let n = &g.nodes[id as usize];
        if matches!(n.op, Op::ArrayLoad(_) | Op::ArrayLength) {
            assert_eq!(n.bytecode_pc, Some(10));
            assert!(n.frame_snapshot.is_some());
        }
        assert!(
            !n.inputs.contains(&rec.entry_ctrl),
            "n{id} still on the body control"
        );
        assert!(
            !n.inputs.contains(&rec.entry_mem),
            "n{id} still on the body memory"
        );
    }
    // The chain's first nodes hang on the old entry edge.
    assert!(g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::Guard { .. }) && n.inputs[0] == c0));
    // The back edge's memory was the value read (a token use): it is the
    // loop's own memory phi now; the value's VALUE use is unchanged.
    let mem_phi = &g.nodes[rec.entry_mem as usize];
    assert_eq!(mem_phi.inputs.get(2), Some(&rec.entry_mem));
    assert!(g
        .users_of(value)
        .iter()
        .any(|&u| g.nodes[u as usize].op == Op::Cmp(CmpOp::Eq)
            && g.nodes[u as usize].bytecode_pc == Some(19)));
}


/// A `set` chain's lookup moves; its store stays in the loop on the moved
/// found entry, at the chain's entry control and memory; the guards are
/// header predicates.
#[test]
fn tlget21_3_a_set_chain_moves_its_lookup_and_keeps_its_store() {
    let (mut g, region, _c0, st) = tl_loop_with(false, true);
    let rec = g.tl_chains[0].clone();
    assert_eq!(plant(&mut g, &DespecRegistry::new()), 1);
    assert!(g.nodes[region as usize].inputs.contains(&rec.hit_ctrl));
    let n = &g.nodes[st as usize];
    assert_eq!(n.op, Op::Store(MemKind::Ref));
    assert_eq!(
        n.inputs[0], rec.entry_ctrl,
        "the store stays on the body control"
    );
    assert_eq!(n.inputs[1], rec.entry_mem, "and the body memory");
    assert_eq!(n.bytecode_pc, Some(rec.pc));
    let guards = (rec.first..rec.end)
        .filter(|&id| g.nodes[id as usize].op == Op::Guard { bci: 10 })
        .count();
    assert_eq!(guards, 8);
}

/// A chain node another frame names (here the map read) keeps the chain
/// in the loop.
#[test]
fn tlget21_3_a_leaked_temporary_keeps_the_chain() {
    let (mut g, _, _, _) = tl_loop(false);
    let rec = g.tl_chains[0].clone();
    let map = (rec.first..rec.end)
        .find(|&id| g.nodes[id as usize].op == Op::Load(MemKind::Ref))
        .expect("the map read");
    g.safepoints
        .push(SafepointSnapshot::new(18, vec![map], vec![], vec![]));
    assert_eq!(plant(&mut g, &DespecRegistry::new()), 0);
}
