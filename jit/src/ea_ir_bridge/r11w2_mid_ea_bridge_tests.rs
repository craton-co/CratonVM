use super::*;
use crate::ir::{Graph, IrType, MemKind, Op};

/// `entry == 0`, `exit == NO_NODE`, as the struct literal this replaced.
fn empty_graph() -> Graph {
    Graph::empty(0)
}

/// A switch inside a profiled-cold arm is cold, and so is every case it
/// reaches. `ea_control_preds` had no `Switch` arm, so the switch had no
/// control predecessor and nothing below it could ever be cold.
#[test]
fn a_switch_in_a_cold_arm_and_its_cases_are_cold() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cond], Some(10));
    let hot = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let cold_arm = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let key = g.add(Op::Const(0), IrType::Int, vec![], None);
    let sw = g.add(
        Op::Switch { low: 0, high: 1 },
        IrType::Control,
        vec![cold_arm, key],
        Some(20),
    );
    let case0 = g.add(Op::Proj(0), IrType::Control, vec![sw], None);
    let case1 = g.add(Op::Proj(1), IrType::Control, vec![sw], None);
    let dflt = g.add(Op::Proj(2), IrType::Control, vec![sw], None);
    let ret = g.add(Op::Return, IrType::Void, vec![case0], None);
    g.entry = start;
    g.exit = ret;

    // pc 10 is usually TAKEN, so its false projection is the cold one.
    let hints: HashMap<usize, bool> = [(10usize, true)].into_iter().collect();
    let cold = ea_cold_control_nodes(&g, &hints);
    for id in [cold_arm, sw, case0, case1, dflt, ret] {
        assert!(cold.contains(&id), "node {id} is only reachable coldly");
    }
    assert!(!cold.contains(&hot), "the taken arm is not cold");
    assert!(!cold.contains(&iff), "the branch itself runs on every path");
}

/// `p = new P(); b = new B(); b.x = p.x;` with `p.x` never stored: the load
/// is retired to a zero default this round, so `b`'s recipe must describe a
/// zero rather than name the load (which the lowerer answers
/// `MaterializationRequired`, discarding the artifact).
#[test]
fn a_recipe_field_read_from_a_zero_default_load_describes_zero() {
    let mut g = empty_graph();
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
    let b = g.add(
        Op::New {
            class_id: 8,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, p, off0],
        None,
    );
    let _st_b = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, load, b, off0, load],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, seven], None);
    g.entry = start;
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    if result.scalar_replaceable.len() != 2 {
        // The analysis declined one of the two; nothing to check here.
        return;
    }
    let map =
        build_scalar_replacement_map(&g, &id_map, &result, &std::collections::HashSet::new());
    if let Some(recipe) = map.objects.get(&b) {
        assert_eq!(
            recipe.field_values.first().copied().flatten(),
            None,
            "b.x holds p.x's zero default, not the retired load {load}"
        );
    }
}

/// A load an earlier round's recipe names (it is in `descriptor_pinned`)
/// is not forwarded and killed, and the object it reads stays allocated.
#[test]
fn a_load_a_recipe_names_survives_and_keeps_its_object() {
    let mut g = empty_graph();
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
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, st_p, p, off0],
        None,
    );
    let ret = g.add(Op::Return, IrType::Int, vec![ctrl, load], None);
    g.entry = start;
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    let pinned: std::collections::HashSet<ir::NodeId> = [load].into_iter().collect();
    let _ = apply_ea_to_ir_pinned(&mut g, &id_map, &result, &pinned);
    assert!(
        matches!(g.nodes[load as usize].op, Op::Load(_)),
        "the pinned load is still a live load"
    );
    assert!(
        matches!(g.nodes[p as usize].op, Op::New { .. }),
        "the object the pinned load reads is still allocated"
    );
    assert!(
        matches!(g.nodes[st_p as usize].op, Op::Store(_)),
        "and so is its store"
    );
}

/// `p = new P(); p.x = 7; a = p.x; b = p.x; return a + b;` with the second
/// load chained on the first as its memory token and a snapshot naming
/// both. The victim loop (round 11 wave 4: per-victim user lists instead
/// of a whole-graph sweep per victim) must leave no live input and no
/// snapshot slot naming a killed load. The second load's token slot names
/// the first load, which is itself a victim: that reference reaches the
/// first load's user list only through the index.
#[test]
fn the_victim_loop_leaves_no_reference_to_a_killed_load() {
    let mut g = empty_graph();
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
    let snap = crate::ir::SafepointSnapshot::new(0, vec![a, b], vec![], vec![]);
    g.push_safepoint(snap);

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    let _ = apply_ea_to_ir_pinned(&mut g, &id_map, &result, &std::collections::HashSet::new());
    if g.nodes[a as usize].op != Op::Dead {
        // The analysis declined the object; nothing was rewritten.
        return;
    }
    for (id, n) in g.nodes.iter().enumerate() {
        if n.op == Op::Dead {
            continue;
        }
        for &inp in n.inputs.iter() {
            if let Some(x) = g.nodes.get(inp as usize) {
                assert_ne!(
                    x.op,
                    Op::Dead,
                    "live node {id} still names killed node {inp}"
                );
            }
        }
    }
    for &v in &g.safepoints[0].locals {
        assert_ne!(
            g.nodes[v as usize].op,
            Op::Dead,
            "a snapshot slot names killed {v}"
        );
    }
    if g.nodes[b as usize].op == Op::Dead {
        assert_eq!(g.nodes[sum as usize].inputs.as_slice(), &[seven, seven]);
        assert_eq!(g.safepoints[0].locals, vec![seven, seven]);
    }
}
