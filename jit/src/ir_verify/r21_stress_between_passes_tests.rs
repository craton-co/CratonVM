use super::*;
use crate::ir::{Graph, IrType, Op};

/// `Start -> Proj(0) -> Return(Const)`, as `tests::linear_graph`.
fn linear() -> Graph {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let k = g.add(Op::Const(7), IrType::Int, vec![], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, k], None);
    g.entry = start;
    g.exit = ret;
    g
}

#[test]
fn a_dead_snapshot_slot_is_named_between_passes_and_a_clean_graph_is_not() {
    let g = linear();
    let r = verify_graph_between_passes(&g);
    assert!(r.is_ok(), "{}", r.unwrap_err());

    let mut g = linear();
    let gone = g.add(Op::Const(9), IrType::Int, vec![], None);
    g.kill(gone);
    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        4,
        vec![gone],
        vec![],
        Vec::new(),
    ));
    let text = verify_graph_between_passes(&g)
        .expect_err("a snapshot slot naming a removed node")
        .to_string();
    assert!(text.contains("refers to a removed (Dead) node"), "{text}");
    assert!(text.contains(PHASE_BETWEEN_PASSES), "{text}");    }