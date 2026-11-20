use super::*;
use crate::ir::{CmpOp, Graph, IrType, MemKind, Op, SafepointSnapshot};

/// The `R16Bench.alloc` loop body without the loop:
///
/// ```text
/// p = new P; p.x = a; p.y = a + 1;        // bci 13, stores at 83 / 88
/// if (a != 0) {} else { keep = p; }       // If at 34, join at 40
/// return p.x + p.y + keep.x;              // loads at 43, 48, 71
/// ```
struct Kernel {
    g: Graph,
    mem: ir::NodeId,
    a: ir::NodeId,
    b: ir::NodeId,
    new: ir::NodeId,
    stores: [ir::NodeId; 2],
    escaping_edge: ir::NodeId,
    join: ir::NodeId,
    keep: ir::NodeId,
    loads: [ir::NodeId; 2],
    sum: ir::NodeId,
    keep_x: ir::NodeId,
}

fn kernel(return_the_object_too: bool) -> Kernel {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let new = g.add(
        Op::New {
            class_id: 9,
            num_fields: 2,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(13),
    );
    let st0 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, new, off0, a],
        Some(83),
    );
    let b = g.add(Op::Add, IrType::Int, vec![a, one], Some(22));
    let st1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, st0, new, off1, b],
        Some(88),
    );
    let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![a, zero], Some(34));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cond], Some(34));
    let taken = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(34));
    let falls = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(34));
    let join = g.add(Op::Merge, IrType::Control, vec![taken, falls], Some(40));
    let nul = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let keep = g.add(Op::Phi, IrType::Ref, vec![join, nul, new], Some(40));
    let l0 = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![join, st1, new, off0],
        Some(43),
    );
    let l1 = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![join, l0, new, off1],
        Some(48),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![l0, l1], Some(51));
    let keep_x = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![join, l1, keep, off0],
        Some(71),
    );
    let total = g.add(Op::Add, IrType::Int, vec![sum, keep_x], Some(75));
    let ret = if return_the_object_too {
        // A second, non-phi use of the object: the object escapes here.
        let _ = total;
        g.add(Op::Return, IrType::Void, vec![join, new], Some(76))
    } else {
        g.add(Op::Return, IrType::Void, vec![join, total], Some(76))
    };
    g.entry = start;
    g.exit = ret;
    Kernel {
        g,
        mem,
        a,
        b,
        new,
        stores: [st0, st1],
        escaping_edge: falls,
        join,
        keep,
        loads: [l0, l1],
        sum,
        keep_x,
    }
}

#[test]
fn the_allocation_moves_onto_the_edge_that_publishes_it() {
    let mut k = kernel(false);
    let verified_before =
        crate::ir_verify::verify_graph_at_phase(&k.g, "post-escape-analysis").is_ok();
    assert_eq!(ir_sink_allocations_to_escaping_edge(&mut k.g, &[]), 1);
    let g = &k.g;
    assert_eq!(
        g.nodes[k.new as usize].op,
        Op::Dead,
        "the block allocates nothing"
    );
    for id in k.stores.iter().chain(k.loads.iter()) {
        assert_eq!(g.nodes[*id as usize].op, Op::Dead);
    }
    // The loads read the stored values.
    assert_eq!(g.nodes[k.sum as usize].inputs.to_vec(), vec![k.a, k.b]);
    // The phi receives the materialized object, allocated on the edge.
    let fresh = g.nodes[k.keep as usize].inputs[2];
    assert!(matches!(
        g.nodes[fresh as usize].op,
        Op::New {
            class_id: 9,
            num_fields: 2
        }
    ));
    assert_eq!(
        g.nodes[fresh as usize].inputs.to_vec(),
        vec![k.escaping_edge, k.mem]
    );
    assert_eq!(g.nodes[fresh as usize].bytecode_pc, Some(13));
    // The load after the join reads the merged memory, whose escaping
    // input is the chain of re-created stores.
    let mem_phi = g.nodes[k.keep_x as usize].inputs[1];
    let mp = &g.nodes[mem_phi as usize];
    assert_eq!(mp.op, Op::Phi);
    assert_eq!(mp.ty, IrType::Memory);
    assert_eq!(mp.inputs[0], k.join);
    assert_eq!(
        mp.inputs[1], k.mem,
        "the publishing-free edge keeps the block's memory"
    );
    let s1 = mp.inputs[2];
    let s1n = &g.nodes[s1 as usize];
    assert_eq!(s1n.op, Op::Store(MemKind::Int));
    assert_eq!(
        s1n.bytecode_pc,
        Some(88),
        "the compact row is keyed by the store's pc"
    );
    assert_eq!(
        (s1n.inputs[0], s1n.inputs[2], s1n.inputs[4]),
        (k.escaping_edge, fresh, k.b)
    );
    let s0n = &g.nodes[s1n.inputs[1] as usize];
    assert_eq!(s0n.op, Op::Store(MemKind::Int));
    assert_eq!(
        (s0n.inputs[0], s0n.inputs[1], s0n.inputs[2], s0n.inputs[4]),
        (k.escaping_edge, k.mem, fresh, k.a)
    );
    if verified_before {
        assert!(crate::ir_verify::verify_graph_at_phase(g, "post-escape-analysis").is_ok());
    }
}

#[test]
fn any_other_use_of_the_object_refuses() {
    let mut k = kernel(true);
    assert_eq!(ir_sink_allocations_to_escaping_edge(&mut k.g, &[]), 0);
    assert!(matches!(k.g.nodes[k.new as usize].op, Op::New { .. }));
}

/// A frame a deopt can read (here: the `keep.x` load's, at bci 71) that
/// names the object refuses: after the rewrite it would describe an
/// object that no longer exists on the path that skipped the edge.
#[test]
fn a_consultable_frame_naming_the_object_refuses() {
    let mut k = kernel(false);
    k.g.push_safepoint(SafepointSnapshot::new(71, vec![k.new], vec![], vec![]));
    assert_eq!(ir_sink_allocations_to_escaping_edge(&mut k.g, &[]), 0);
    assert!(matches!(k.g.nodes[k.new as usize].op, Op::New { .. }));
}

/// A frame at the forward join (no transfer arrives there) loses its slot
/// for the deleted node instead of naming a dead one.
#[test]
fn a_frame_no_transfer_reads_is_cleared() {
    let mut k = kernel(false);
    k.g.push_safepoint(SafepointSnapshot::new(
        40,
        vec![k.keep, k.new],
        vec![],
        vec![],
    ));
    assert_eq!(ir_sink_allocations_to_escaping_edge(&mut k.g, &[]), 1);
    assert_eq!(k.g.safepoints[0].locals, vec![k.keep, ir::NO_NODE]);
}

/// A load that runs BEFORE its field's store must not be forwarded.
#[test]
fn a_load_before_its_store_refuses() {
    let mut k = kernel(false);

    // Re-point the first load's memory at the block's entry state: it no
    // longer provably follows `st0`.
    let l0 = k.loads[0];
    k.g.set_input(l0, 1, k.mem);

    assert_eq!(ir_sink_allocations_to_escaping_edge(&mut k.g, &[]), 0);
}

