use super::*;
use crate::ir::{CmpOp, IrType};

/// The caller's loop contains a guarded get: an inherited leaf on the
/// hit, a virtual call on the miss, a memory phi, then getstatic TARGET.
#[test]
fn a_static_final_hoist_replaces_an_inner_splice_memory_phi() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let limit = g.add(Op::Const(16), IrType::Int, vec![], None);
    let index = g.add(Op::Phi, IrType::Int, vec![header, zero], None);
    let mem = g.add(Op::Phi, IrType::Memory, vec![header, mem0], None);
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![index, limit], None);
    let test = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![test], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![test], None);
    let receiver = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let exact = g.add(
        Op::ExactClassIs { class_id: 7 },
        IrType::Int,
        vec![receiver],
        Some(11),
    );
    let guard = g.add(Op::If, IrType::Control, vec![stay, exact], Some(11));
    let hit = g.add(Op::Proj(0), IrType::Control, vec![guard], None);
    let miss = g.add(Op::Proj(1), IrType::Control, vec![guard], None);
    let call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Ref,
        vec![miss, mem, receiver],
        Some(11),
    );
    let join = g.add(Op::Merge, IrType::Control, vec![hit, miss], Some(11));
    let joined_mem = g.add(Op::Phi, IrType::Memory, vec![join, mem, call], None);
    let load = g.add(
        Op::LoadStatic {
            class_id: 9,
            field_index: 0,
            type_tag: b'L',
            is_volatile: false,
        },
        IrType::Ref,
        vec![join, joined_mem],
        Some(14),
    );
    g.add(Op::KeepAlive, IrType::Void, vec![join, load], Some(17));
    let next = g.add(Op::Add, IrType::Int, vec![index, one], None);
    g.nodes[header as usize].inputs.push(join);
    g.nodes[index as usize].inputs.push(next);
    g.nodes[mem as usize].inputs.push(load);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], None);
    g.direct_static_fields = vec![(9, 0)];
    g.trusted_final_static_fields = vec![(9, 0)];
    let body = loop_body_with(&g, &build_users(&g), header, entry, &[join]);
    assert!(
        !body.contains(&joined_mem),
        "inner join phis are not in the membership set"
    );
    assert_eq!(hoist_loop_static_reads_in(&mut g, true), 1);
    assert_eq!(g.nodes[load as usize].inputs, vec![entry, mem0]);
    assert!(crate::ir_schedule::schedule(&g)
        .misplaced_data_nodes(&g)
        .is_empty());
}
