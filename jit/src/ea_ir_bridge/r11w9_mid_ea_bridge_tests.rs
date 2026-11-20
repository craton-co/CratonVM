use super::*;
use crate::ir::{Graph, IrType, Op};

/// `synchronized (new Object()) {}` built with the ops the builder emits:
/// `[ctrl, mem, obj]` monitors on the memory chain. `publish` returns the
/// object, which lets another thread reach it. Returns the graph and
/// `[object, enter, exit]`.
fn locked_fresh_object(publish: bool) -> (Graph, [ir::NodeId; 3]) {
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
    let ret = if publish {
        g.add(Op::Return, IrType::Ref, vec![ctrl, obj], Some(8))
    } else {
        g.add(Op::Return, IrType::Void, vec![ctrl], Some(8))
    };
    g.entry = start;
    g.exit = ret;
    (g, [obj, enter, exit])
}

/// HotSpot's EliminateLocks shape reaches the production applier: the
/// bridge sees both monitors, the analysis offers the object whole, and
/// both ops die with the chain spliced.
#[test]
fn a_lock_on_a_confined_allocation_is_elided_whole() {
    let (mut g, [obj, enter, exit]) = locked_fresh_object(false);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert_eq!(result.lock_elisions.len(), 1, "{:?}", result.lock_refusals);
    assert_eq!(result.lock_elisions[0].object, id_map[obj as usize]);
    assert_eq!(result.lock_elisions[0].monitors.len(), 2);
    apply_ea_to_ir(&mut g, &id_map, &result);
    assert_eq!(g.nodes[enter as usize].op, Op::Dead);
    assert_eq!(g.nodes[exit as usize].op, Op::Dead);
}

/// Round 14 wave 3 (lane fixup): `o.leaf(); synchronized (o) { .. }` on a
/// fresh `o` -- a recorded splice window, then a bytecode region. EA
/// coarsening would delete the window's exit and the region's enter;
/// the lowerer can pair neither, so the plan is refused. Two windows, or
/// two bytecode pairs, are not split and stay coarsenable.
#[test]
fn an_ea_coarsening_across_a_window_and_a_bytecode_pair_is_refused() {
    let build = |record: [bool; 2]| {
        let mut g = Graph::empty(16);
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
        let ew = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, mem, obj],
            Some(4),
        );
        let xw = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![ctrl, ew, obj],
            Some(4),
        );
        let e2 = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, xw, obj],
            Some(8),
        );
        let x2 = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![ctrl, e2, obj],
            Some(10),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(12));
        g.entry = start;
        g.exit = ret;
        for (i, (enter, exit)) in [(ew, xw), (e2, x2)].into_iter().enumerate() {
            if record[i] {
                g.sync_splice_windows.push(ir::SyncSpliceWindow {
                    enter,
                    exit,
                    monitor: obj,
                    static_class_id: 0,
                });
            }
        }
        (g, xw, e2)
    };
    for (record, splits) in [
        ([true, false], true),
        ([false, true], true),
        ([true, true], false),
        ([false, false], false),
    ] {
        let (g, xw, e2) = build(record);
        // Identity numbering: EA node `k` is IR node `k`.
        let reverse_map: HashMap<escape_analysis::NodeId, ir::NodeId> =
            (0..g.nodes.len()).map(|i| (i, i as ir::NodeId)).collect();
        let inner = [xw as escape_analysis::NodeId, e2 as escape_analysis::NodeId];
        assert_eq!(
            ea_coarsening_splits_a_sync_window(&g, &reverse_map, inner),
            splits,
            "windows recorded {record:?}"
        );
    }
}

/// A returned object is reachable from another thread: its lock is a
/// real happens-before edge and stays.
#[test]
fn a_lock_on_a_published_allocation_stays() {
    let (mut g, [_, enter, exit]) = locked_fresh_object(true);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(result.lock_elisions.is_empty());
    apply_ea_to_ir(&mut g, &id_map, &result);
    assert_eq!(g.nodes[enter as usize].op, Op::MonitorEnter);
    assert_eq!(g.nodes[exit as usize].op, Op::MonitorExit);
}
