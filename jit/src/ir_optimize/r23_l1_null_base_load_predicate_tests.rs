use super::*;
use crate::deopt::{DespecRegistry, SPECULATION_ID_ANY};
use crate::ir::SafepointSnapshot;

const KEY: &str = "T.f:(LH;I)I";

/// `int s = 0; for (int i = 0; i < n; i++) s += h.k;` as javac lays it
/// out: the header merge at bci 10 (its snapshot names `h`, `n`, `i`,
/// `s`), the exit test at 12, `h.k` at 16 on the stay edge, so the body
/// may run zero times and `h` (a parameter) may be null. With `store`, the
/// body also writes `h.k = s` at 20. Returns `(graph, entry ctrl, h, zero,
/// the read, its user)`.
fn field_read_loop(store: bool) -> (Graph, NodeId, NodeId, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let h = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let s = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let k = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![stay, m0, h, one],
        Some(16),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![s, k], Some(19));
    if store {
        g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![stay, m0, h, one, sum],
            Some(20),
        );
    }
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(22));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[s as usize].inputs.push(sum);
    g.nodes[region as usize].inputs.push(stay);
    let ret = g.add(Op::Return, IrType::Int, vec![exit, s], None);
    g.exit = ret;
    g.safepoints
        .push(SafepointSnapshot::new(10, vec![h, n, iv, s], vec![], vec![]));
    g.safepoints.push(SafepointSnapshot::new(
        16,
        vec![h, n, iv, s],
        vec![h],
        vec![],
    ));
    (g, c0, h, zero, k, sum)
}

fn guards(g: &Graph) -> Vec<NodeId> {
    (0..g.nodes.len())
        .filter(|&i| matches!(g.nodes[i].op, Op::Guard { .. }))
        .map(|i| i as NodeId)
        .collect()
}

/// The read moves to the pre-header behind `Guard(h != null)` at the
/// HEADER bci, named by the loop-entry frame (`i` and `s` at their entry
/// value 0). So a null `h` with `n <= 0` (a zero-trip loop) deopts to the
/// first test of the loop, which the interpreter takes to the exit: no
/// NPE, as in Java. With `n > 0` the interpreter reaches `h.k` and throws
/// there.
#[test]
fn r23_l1_a_possibly_null_base_read_leaves_behind_a_loop_entry_null_guard() {
    let (mut g, entry, h, zero, k, sum) = field_read_loop(false);
    assert_eq!(
        plant_null_base_load_predicates(&mut g, &DespecRegistry::new(), KEY),
        1
    );
    assert_eq!(g.nodes[k as usize].op, Op::Dead, "the in-body read is gone");
    let gs = guards(&g);
    assert_eq!(gs.len(), 1);
    let guard = &g.nodes[gs[0] as usize];
    assert_eq!(guard.op, Op::Guard { bci: 10 });
    assert_eq!(guard.bytecode_pc, Some(10));
    assert_eq!(guard.inputs[0], entry, "in the pre-header");
    let cond = &g.nodes[guard.inputs[1] as usize];
    assert_eq!(cond.op, Op::Cmp(CmpOp::Ne));
    assert_eq!(cond.inputs[0], h);
    assert_eq!(g.nodes[cond.inputs[1] as usize].op, Op::Const(0));
    let frame = &g.safepoints[guard.frame_snapshot.expect("a frame") as usize];
    assert_eq!(frame.bci, 10, "resumes at the loop entry, not at the read");
    assert_eq!(frame.locals[2], zero, "`i` at its entry value");
    assert_eq!(frame.locals[3], zero, "`s` at its entry value");
    let copy = g.nodes[sum as usize].inputs[1];
    let cn = &g.nodes[copy as usize];
    assert_eq!(cn.op, Op::Load(MemKind::Int));
    assert_eq!(cn.inputs[0], entry, "the read is in the pre-header");
    assert_eq!(cn.inputs[2], h);
    assert_eq!(
        cn.bytecode_pc,
        Some(16),
        "the row of the field is keyed by the pc of the read"
    );
    assert!(copy > gs[0], "created after the guard, so it runs after it");
    // JIT r23 wave 9 (W9-L7): an id is no order -- the guard is the copy's
    // trailing guard token, the edge the scheduler orders by.
    assert_eq!(cn.inputs.len(), 5);
    assert_eq!(cn.inputs[4], gs[0], "the copy names its guard as its token");
    assert_eq!(crate::ir::guard_token_of(cn), gs[0]);
    // And the schedule puts the guard first in the pre-header block.
    let schedule = crate::ir_schedule::schedule(&g);
    let b = schedule.node_to_block[copy as usize];
    assert_eq!(schedule.node_to_block[gs[0] as usize], b, "one block");
    let order = &schedule.blocks[b].nodes;
    let at = |x: NodeId| order.iter().position(|&y| y == x).expect("scheduled");
    assert!(at(gs[0]) < at(copy), "the guard runs before the copy");
}

/// A body that writes the field reads a different value each trip:
/// nothing moves and no guard is planted.
#[test]
fn r23_l1_a_loop_that_stores_to_the_field_keeps_its_read() {
    let (mut g, _, _, _, k, _) = field_read_loop(true);
    assert_eq!(
        plant_null_base_load_predicates(&mut g, &DespecRegistry::new(), KEY),
        0
    );
    assert_eq!(g.nodes[k as usize].op, Op::Load(MemKind::Int));
    assert!(guards(&g).is_empty());
    assert_eq!(g.safepoints.len(), 2, "no frame copy either");
}

/// A withdrawal at the header (the guard failed often enough) plants
/// nothing; one for another method changes nothing.
#[test]
fn r23_l1_a_de_specialised_header_gets_no_null_base_predicate() {
    let despec = DespecRegistry::new();
    despec.insert(KEY, 10, SPECULATION_ID_ANY);
    let (mut g, _, _, _, k, _) = field_read_loop(false);
    assert_eq!(plant_null_base_load_predicates(&mut g, &despec, KEY), 0);
    assert_eq!(g.nodes[k as usize].op, Op::Load(MemKind::Int));
    assert_eq!(g.safepoints.len(), 2);
    let elsewhere = DespecRegistry::new();
    elsewhere.insert("T.other:()V", 10, SPECULATION_ID_ANY);
    let (mut g, _, _, _, _, _) = field_read_loop(false);
    assert_eq!(plant_null_base_load_predicates(&mut g, &elsewhere, KEY), 1);
}



/// [`field_read_loop`] as an OSR-door graph builds it: the loop's
/// pre-header is the opaque OSR entry merge of its header (bci 10) --
/// `If(OsrEntryFlag { 10 })`, whose OSR arm imports `h`, `n` and memory
/// (`OsrLocal` / `OsrMemory`) -- and the header's phis read the merge's
/// phis. With `arm == false` the same graph has no OSR arm (a header
/// whose OSR stub would enter past the pre-header). Returns
/// `(graph, pre-header, the read, its user)`.
fn osr_field_read_loop(arm: bool) -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    g.osr_compile = true;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let h0 = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n0 = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let (pre, h, n, mem, iv0, s0) = if arm {
        let flag = g.add(Op::OsrEntryFlag { header_bci: 10 }, IrType::Int, vec![c0], None);
        let iff = g.add(Op::If, IrType::Control, vec![c0, flag], None);
        let normal = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let osr = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let pre = g.add(Op::Merge, IrType::Control, vec![normal, osr], Some(10));
        let om = g.add(Op::OsrMemory, IrType::Memory, vec![osr], None);
        let oh = g.add(Op::OsrLocal(0), IrType::Ref, vec![osr], None);
        let on = g.add(Op::OsrLocal(1), IrType::Int, vec![osr], None);
        let oi = g.add(Op::OsrLocal(2), IrType::Int, vec![osr], None);
        let os = g.add(Op::OsrLocal(3), IrType::Int, vec![osr], None);
        let mem = g.add(Op::Phi, IrType::Memory, vec![pre, m0, om], Some(10));
        let h = g.add(Op::Phi, IrType::Ref, vec![pre, h0, oh], Some(10));
        let n = g.add(Op::Phi, IrType::Int, vec![pre, n0, on], Some(10));
        let iv0 = g.add(Op::Phi, IrType::Int, vec![pre, zero, oi], Some(10));
        let s0 = g.add(Op::Phi, IrType::Int, vec![pre, zero, os], Some(10));
        (pre, h, n, mem, iv0, s0)
    } else {
        (c0, h0, n0, m0, zero, zero)
    };
    let region = g.add(Op::Merge, IrType::Control, vec![pre], Some(10));
    let iv = g.add(Op::Phi, IrType::Int, vec![region, iv0], Some(10));
    let s = g.add(Op::Phi, IrType::Int, vec![region, s0], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let k = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![stay, mem, h, one],
        Some(16),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![s, k], Some(19));
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(22));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[s as usize].inputs.push(sum);
    g.nodes[region as usize].inputs.push(stay);
    let ret = g.add(Op::Return, IrType::Int, vec![exit, s], None);
    g.exit = ret;
    g.safepoints
        .push(SafepointSnapshot::new(10, vec![h, n, iv, s], vec![], vec![]));
    g.safepoints.push(SafepointSnapshot::new(
        16,
        vec![h, n, iv, s],
        vec![h],
        vec![],
    ));
    (g, pre, k, sum)
}

/// JIT round 23 wave 3c (`CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_OSR`):
/// in an OSR-door graph the read leaves only when its pre-header is the
/// header's opaque OSR entry merge. The guard then sits on that merge
/// (both entries pass it), named by the loop-entry frame, whose slots are
/// the merge's phis, so the OSR arm's interpreter locals are what the
/// interpreter resumes with. Without the OSR arm, an OSR stub could enter
/// past the pre-header, and nothing moves.
#[test]
fn r23_l1_an_osr_loop_reads_through_its_entry_merge_or_not_at_all() {
    let (mut g, pre, k, sum) = osr_field_read_loop(true);
    assert_eq!(
        plant_null_base_load_predicates(&mut g, &DespecRegistry::new(), KEY),
        1
    );
    assert_eq!(g.nodes[k as usize].op, Op::Dead);
    let gs = guards(&g);
    assert_eq!(gs.len(), 1);
    let guard = &g.nodes[gs[0] as usize];
    assert_eq!(guard.op, Op::Guard { bci: 10 });
    assert_eq!(guard.inputs[0], pre, "on the OSR entry merge");
    let frame = &g.safepoints[guard.frame_snapshot.expect("a frame") as usize];
    assert_eq!(frame.bci, 10);
    for slot in &frame.locals {
        assert!(
            g.nodes[*slot as usize].op == Op::Phi
                && g.nodes[*slot as usize].inputs.first() == Some(&pre),
            "every slot is a phi of the entry merge"
        );
    }
    let copy = g.nodes[sum as usize].inputs[1];
    assert_eq!(g.nodes[copy as usize].inputs[0], pre);

    let (mut g, _, k, _) = osr_field_read_loop(false);
    assert_eq!(
        plant_null_base_load_predicates(&mut g, &DespecRegistry::new(), KEY),
        0,
        "no opaque OSR entry merge: an OSR stub may bypass the pre-header"
    );
    assert_eq!(g.nodes[k as usize].op, Op::Load(MemKind::Int));
    assert!(guards(&g).is_empty());
}

/// r23 wave 3 review (`CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_OSR`): an
/// inner header's OSR arm on the loop's back-edge path -- an
/// `If(OsrEntryFlag)` whose arm rejoins the body -- refuses the predicate
/// even when that arm imports no memory. `OsrLocal` and `OsrEntryFlag` are
/// pure, so `loop_body` never holds them; the arm's `If` is what is left
/// to see, and an OSR stub entering there would skip the guard.
#[test]
fn r23_l1_an_inner_osr_arm_in_the_body_refuses_the_predicate() {
    let (mut g, pre, k, _) = osr_field_read_loop(true);
    let region = (0..g.nodes.len())
        .find(|&i| {
            let n = &g.nodes[i];
            n.op == Op::Merge && n.inputs.len() == 2 && n.inputs[0] == pre
        })
        .expect("the loop header") as NodeId;
    let stay = g.nodes[region as usize].inputs[1];
    let flag = g.add(Op::OsrEntryFlag { header_bci: 16 }, IrType::Int, vec![stay], None);
    let iff = g.add(Op::If, IrType::Control, vec![stay, flag], None);
    let normal = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let arm = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let inner = g.add(Op::Merge, IrType::Control, vec![normal, arm], Some(16));
    g.nodes[region as usize].inputs[1] = inner;
    assert_eq!(
        plant_null_base_load_predicates(&mut g, &DespecRegistry::new(), KEY),
        0,
        "an inner OSR arm inside the body may enter past the pre-header"
    );
    assert_eq!(g.nodes[k as usize].op, Op::Load(MemKind::Int));
    assert!(guards(&g).is_empty());
}
