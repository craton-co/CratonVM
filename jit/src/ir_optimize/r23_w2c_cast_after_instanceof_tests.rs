use super::*;

/// `if (o instanceof T) return ((T) o).v;` as the builder lays it out:
/// `If(Cmp(Eq, InstanceOf{T}(o), 0))`, the cast on `Proj(cast_proj)`
/// naming target `cast_target`, the read through the cast. Returns
/// `(graph, o, instanceof, cast, read)`.
fn shape(cast_proj: u32, cast_target: usize) -> (Graph, NodeId, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let o = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let io = g.add(
        Op::InstanceOf {
            name_ptr: 0x7000,
            name_len: 3,
            seen_cell: 0,
            header_only: false,
        },
        IrType::Int,
        vec![c0, m0, o],
        Some(1),
    );
    let eq = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![io, zero], Some(4));
    let iff = g.add(Op::If, IrType::Control, vec![c0, eq], Some(4));
    let p0 = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let p1 = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let at = if cast_proj == 1 { p1 } else { p0 };
    let cast = g.add(
        Op::CheckCast {
            name_ptr: cast_target,
            name_len: 3,
        },
        IrType::Ref,
        vec![at, io, o],
        Some(8),
    );
    let read = g.add(Op::Load(MemKind::Int), IrType::Int, vec![at, cast, cast, one], Some(11));
    let ret = g.add(Op::Return, IrType::Int, vec![at, read], None);
    g.exit = ret;
    let _ = p0;
    (g, o, io, cast, read)
}

