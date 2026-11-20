use super::*;

/// `u = n + 1` in the entry block, read by a compare there; then
/// `for (..) h = 31 * h + p.f`. `u` and the phi `h` share R13. With
/// `u_in_loop`, the loop's add reads `u` too. Returns `(graph, h, u)`.
fn shape(u_in_loop: bool) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let k0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let c31 = g.add(Op::Const(31), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let u = g.add(Op::Add, IrType::Int, vec![n, one], Some(1));
    let pre_cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![u, k0], Some(2));
    let pre_if = g.add(Op::If, IrType::Control, vec![c0, pre_cmp], Some(2));
    let enter = g.add(Op::Proj(0), IrType::Control, vec![pre_if], None);
    let skip = g.add(Op::Proj(1), IrType::Control, vec![pre_if], None);
    let region = g.add(Op::Merge, IrType::Control, vec![enter], Some(4));
    let h = g.add(Op::Phi, IrType::Int, vec![region, k0], Some(4));
    let i = g.add(Op::Phi, IrType::Int, vec![region, k0], Some(4));
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i, n], Some(6));
    let iff = g.add(Op::If, IrType::Control, vec![region, ge], Some(6));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let m = g.add(Op::Mul, IrType::Int, vec![c31, h], Some(12));
    let v = g.add(Op::Load(MemKind::Int), IrType::Int, vec![stay, m0, p, k0], Some(14));
    let rhs = if u_in_loop {
        g.add(Op::Add, IrType::Int, vec![v, u], Some(15))
    } else {
        v
    };
    let sum = g.add(Op::Add, IrType::Int, vec![m, rhs], Some(16));
    let i1 = g.add(Op::Add, IrType::Int, vec![i, one], Some(18));
    g.nodes[region as usize].inputs.push(stay);
    g.nodes[h as usize].inputs.push(sum);
    g.nodes[i as usize].inputs.push(i1);
    let out = g.add(Op::Merge, IrType::Control, vec![exit, skip], None);
    let res = g.add(Op::Phi, IrType::Int, vec![out, h, k0], None);
    g.exit = g.add(Op::Return, IrType::Int, vec![out, res], Some(30));
    (g, h, u)
}



// `u` read inside the loop is no pre-loop value: nothing is nameable.
