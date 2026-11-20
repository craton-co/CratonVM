use super::*;
use crate::ir::{Graph, IrType, Op};

/// `synchronized (o) {} ; S = o;` with `o` fresh: the static write is the
/// only thing that publishes `o`, and it must publish it GLOBALLY -- no
/// scalar replacement, no lock elision, and the EA state is
/// `GlobalEscape` (a static is visible to every thread), not the
/// `ArgEscape` a callee argument gets.
#[test]
fn a_reference_written_to_a_static_escapes_globally() {
    let mut g = Graph::empty(12);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    let enter = g.add(
        Op::MonitorEnter,
        IrType::Memory,
        vec![ctrl, mem, obj],
        Some(4),
    );
    let exit = g.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![ctrl, enter, obj],
        Some(6),
    );
    let store = g.add(
        Op::StoreStatic {
            class_id: 9,
            field_index: 0,
            type_tag: b'L',
            is_volatile: false,
        },
        IrType::Memory,
        vec![ctrl, exit, obj],
        Some(8),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(11));
    g.entry = start;
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    assert_eq!(
        ea.nodes[id_map[store as usize]].op,
        escape_analysis::Op::Other,
        "a static write is the unmodelled-publication node, not a call"
    );
    let result = escape_analysis::analyze_escapes(&ea);
    assert_eq!(
        result.escape_states.get(&id_map[obj as usize]),
        Some(&escape_analysis::EscapeState::GlobalEscape)
    );
    assert!(result.lock_elisions.is_empty());
    assert!(result
        .scalar_replaceable
        .iter()
        .all(|s| s.alloc_node != id_map[obj as usize]));
    apply_ea_to_ir(&mut g, &id_map, &result);
    assert_eq!(g.nodes[enter as usize].op, Op::MonitorEnter);
    assert_eq!(g.nodes[exit as usize].op, Op::MonitorExit);
    assert!(matches!(g.nodes[store as usize].op, Op::StoreStatic { .. }));
}
