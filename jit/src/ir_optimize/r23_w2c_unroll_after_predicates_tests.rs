use super::*;
use crate::deopt::DespecRegistry;
use crate::ir::SafepointSnapshot;

const KEY: &str = "T.f:(LH;I)I";

/// `int s = 0; for (int i = 0; i < n; i++) s += h.k + i;` with `h` a
/// parameter: header 10, exit test 12, `h.k` at 16 on the stay edge.
/// Returns `(graph, header, the read)`.
fn loop_with_a_field_read() -> (Graph, NodeId, NodeId) {
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
    let ki = g.add(Op::Add, IrType::Int, vec![k, iv], Some(18));
    let sum = g.add(Op::Add, IrType::Int, vec![s, ki], Some(19));
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
    (g, region, k)
}

