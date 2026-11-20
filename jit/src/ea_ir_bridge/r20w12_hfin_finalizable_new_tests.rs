use super::*;
use crate::ir::{Graph, IrType, MemKind, Op};

/// `r = new R(); x = new X(); r.f0 = x; r.f1 = 5; int v = r.f1; return v;`
/// -- the shape of a spliced `WeakReference` constructor (`referent` is
/// `x`) followed by a field read. `r` is class 7, `x` class 8. When
/// `finalizable` is set, the compile resolved class 7 WITH a finalizer /
/// reference registration. Returns the graph and `[r, x, ret]`.
fn holder_with_referent(finalizable: bool) -> (Graph, [ir::NodeId; 3]) {
    let mut g = Graph::empty(24);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let r = g.add(
        Op::New {
            class_id: 7,
            num_fields: 2,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    let x = g.add(
        Op::New {
            class_id: 8,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(4),
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let five = g.add(Op::Const(5), IrType::Int, vec![], None);
    let st0 = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![ctrl, mem, r, off0, x],
        Some(8),
    );
    let st1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, st0, r, off1, five],
        Some(12),
    );
    let ld = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, st1, r, off1],
        Some(16),
    );
    let ret = g.add(Op::Return, IrType::Int, vec![ctrl, ld], Some(20));
    g.entry = start;
    g.exit = ret;
    if finalizable {
        g.finalizable_new_classes.insert(7);
    } else {
        g.finalizer_free_new_classes.insert(7);
    }
    g.finalizer_free_new_classes.insert(8);
    (g, [r, x, ret])
}

fn replaced(result: &escape_analysis::EscapeAnalysisResult, ea_id: usize) -> bool {
    result
        .scalar_replaceable
        .iter()
        .any(|i| i.alloc_node == ea_id)
}

/// Positive control: a finalizer-free holder is scalar-replaced, so the
/// test below fails for the screen and not for the shape.
#[test]
fn a_finalizer_free_holder_is_scalar_replaced() {
    let (g, [r, _, _]) = holder_with_referent(false);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    assert!(matches!(
        ea.nodes[id_map[r as usize]].op,
        escape_analysis::Op::New { .. }
    ));
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        replaced(&result, id_map[r as usize]),
        "{:?}",
        result.scalar_refusals
    );
}

/// A finalizable / `Reference` holder keeps its allocation, its stores
/// and its load, and what it holds escapes with it: the referent is
/// reachable through a real object the collector (or `finalize()`) reads,
/// so it must not become a frame slot either.
#[test]
fn a_finalizable_holder_is_never_scalar_replaced_and_escapes_its_referent() {
    let (mut g, [r, x, ret]) = holder_with_referent(true);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    assert_eq!(ea.nodes[id_map[r as usize]].op, escape_analysis::Op::Call);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(!replaced(&result, id_map[r as usize]));
    assert!(!replaced(&result, id_map[x as usize]));
    apply_ea_to_ir(&mut g, &id_map, &result);
    assert!(matches!(
        g.nodes[r as usize].op,
        Op::New { class_id: 7, .. }
    ));
    assert!(matches!(
        g.nodes[x as usize].op,
        Op::New { class_id: 8, .. }
    ));
    let ld = g.nodes[ret as usize].inputs[1];
    assert!(matches!(g.nodes[ld as usize].op, Op::Load(_)));
}
