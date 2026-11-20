use super::*;
use crate::ir::{Graph, IrType, MemKind, Op};

/// `p = new P(); p.x = 7; a = p.x; b = p.x; return a + b;`, a snapshot
/// naming both loads.
fn two_loads_of_one_store() -> Graph {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
    let st_p = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, p, off0, seven],
        None,
    );
    let a = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, st_p, p, off0],
        None,
    );
    let b = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, a, p, off0],
        None,
    );
    let sum = g.add(Op::Add, IrType::Int, vec![a, b], None);
    let ret = g.add(Op::Return, IrType::Int, vec![ctrl, sum], None);
    g.entry = start;
    g.exit = ret;
    let frame = crate::ir::SafepointSnapshot::new(0, vec![a, b], vec![], vec![]);
    g.safepoints.push(frame);
    g
}

/// What a graph looks like, as far as the rewrite can change it.
fn shape(g: &Graph) -> (Vec<(Op, Vec<ir::NodeId>)>, Vec<Vec<ir::NodeId>>) {
    (
        g.nodes
            .iter()
            .map(|n| (n.op.clone(), n.inputs.as_slice().to_vec()))
            .collect(),
        g.safepoints.iter().map(|s| s.locals.clone()).collect(),
    )
}

/// Handing the map builder's plans to the applier (the production path
/// since wave 7) rewrites the graph exactly as letting the applier plan
/// again does, and the builder's map is the same map.
#[test]
fn handed_over_plans_rewrite_the_graph_like_a_re_plan() {
    let pinned = std::collections::HashSet::new();

    let mut replanned = two_loads_of_one_store();
    let (ea, id_map) = escape_analysis_from_ir(&replanned);
    let result = escape_analysis::analyze_escapes(&ea);
    let map_a = build_scalar_replacement_map(&replanned, &id_map, &result, &pinned);
    let applied_a = apply_ea_to_ir_pinned(&mut replanned, &id_map, &result, &pinned);

    let mut handed = two_loads_of_one_store();
    let (ea, id_map) = escape_analysis_from_ir(&handed);
    let result = escape_analysis::analyze_escapes(&ea);
    let (map_b, plans) =
        build_scalar_replacement_map_with_plans(&handed, &id_map, &result, &pinned);
    let applied_b = apply_ea_to_ir_planned(&mut handed, &id_map, &result, &pinned, Some(plans));

    assert_eq!(applied_a, applied_b);
    assert_eq!(shape(&replanned), shape(&handed));
    let mut keys_a: Vec<_> = map_a.objects.keys().copied().collect();
    let mut keys_b: Vec<_> = map_b.objects.keys().copied().collect();
    keys_a.sort_unstable();
    keys_b.sort_unstable();
    assert_eq!(keys_a, keys_b);
}

/// An empty hand-over applies no scalar replacement at all: the applier
/// takes the plans it is given and does not plan again.
#[test]
fn an_empty_hand_over_is_not_re_planned() {
    let pinned = std::collections::HashSet::new();
    let mut g = two_loads_of_one_store();
    let before = shape(&g);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    let _ = apply_ea_to_ir_planned(&mut g, &id_map, &result, &pinned, Some(Vec::new()));
    if result.lock_elisions.is_empty() && result.lock_coarsening.is_empty() {
        assert_eq!(shape(&g), before, "nothing was planned, so nothing changed");
    }
}
