use super::*;

#[test]
fn null_checked_ref_phi_enables_guarded_scalar_replacement() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let cond_p = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let iff0 = g.add(Op::If, IrType::Control, vec![ctrl, cond_p], None);
    let t0 = g.add(Op::Proj(0), IrType::Control, vec![iff0], None);
    let f0 = g.add(Op::Proj(1), IrType::Control, vec![iff0], None);
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![t0, mem],
        None,
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let c7 = g.add(Op::Const(7), IrType::Int, vec![], None);
    let st = g.add(
        Op::Store(crate::ir::MemKind::Int),
        IrType::Memory,
        vec![t0, alloc, alloc, off0, c7],
        None,
    );
    let merge = g.add(Op::Merge, IrType::Control, vec![t0, f0], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let phi = g.add(Op::Phi, IrType::Ref, vec![merge, alloc, null], None);
    let mphi = g.add(Op::Phi, IrType::Memory, vec![merge, st, mem], None);
    let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![phi, null], None);
    let iff1 = g.add(Op::If, IrType::Control, vec![merge, cmp], None);
    let non_null_p = g.add(Op::Proj(0), IrType::Control, vec![iff1], None);
    let null_p = g.add(Op::Proj(1), IrType::Control, vec![iff1], None);
    let ld = g.add(
        Op::Load(crate::ir::MemKind::Int),
        IrType::Int,
        vec![non_null_p, mphi, phi, off0],
        None,
    );
    let join = g.add(Op::Merge, IrType::Control, vec![non_null_p, null_p], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let res = g.add(Op::Phi, IrType::Int, vec![join, ld, zero], None);
    let ret = g.add(Op::Return, IrType::Void, vec![join, res], None);
    g.entry = start;
    g.exit = ret;

    let folded = fold_null_checked_ref_phis(&mut g);
    assert_eq!(folded, 1);

    let (ea, id_map) = crate::ea_ir_bridge::escape_analysis_from_ir(&g);
    let ea_phi = id_map[phi as usize];
    assert!(ea.null_guarded_phis.contains(&ea_phi));
    let res_ea = crate::escape_analysis::analyze_escapes(&ea);
    assert!(
        res_ea
            .scalar_replaceable
            .iter()
            .any(|sr| sr.alloc_node == id_map[alloc as usize]),
        "null-guarded phi must allow scalar replacement"
    );
    crate::ea_ir_bridge::apply_ea_to_ir(&mut g, &id_map, &res_ea);
    assert_eq!(g.nodes[alloc as usize].op, Op::Dead);
    assert_eq!(g.nodes[phi as usize].op, Op::Dead);
}
