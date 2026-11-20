use super::*;
use crate::ir::SyncSpliceWindow;

/// `synchronized (o) { o.leaf(); }` with `leaf` a synchronized splice:
/// the region's `enter(o)` at bci 1, the splice's frameless pair at the
/// invoke's bci 3 (no `Call` there), the region's `exit(o)` at bci 7.
/// Returns the graph and `(region enter, window enter, window exit,
/// region exit)`.
fn region_around_a_splice(record_window: bool) -> (Graph, [NodeId; 4]) {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let o = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let e1 = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, m0, o], Some(1));
    let ew = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, e1, o], Some(3));
    let xw = g.add(Op::MonitorExit, IrType::Memory, vec![c0, ew, o], Some(3));
    let x1 = g.add(Op::MonitorExit, IrType::Memory, vec![c0, xw, o], Some(7));
    let ret = g.add(Op::Return, IrType::Void, vec![c0], Some(8));
    g.exit = ret;
    for (bci, monitors) in [(1, vec![]), (3, vec![o]), (7, vec![o]), (8, vec![])] {
        g.safepoints
            .push(SafepointSnapshot::new(bci, vec![o], vec![], monitors));
    }
    if record_window {
        g.sync_splice_windows.push(SyncSpliceWindow {
            enter: ew,
            exit: xw,
            monitor: o,
            static_class_id: 0,
        });
    }
    (g, [e1, ew, xw, x1])
}

/// The page's hazard, pinned both ways: unrecorded, the pass reads the
/// splice's enter as nested (bci 3's frame holds `o`) and its exit as not,
/// and deletes the enter alone -- the exit would then release the REGION's
/// lock. Recorded, the window's pair is left whole.
#[test]
fn a_synchronized_splice_pair_inside_a_held_region_is_left_whole() {
    let (mut bare, [_, ew, xw, _]) = region_around_a_splice(false);
    assert_eq!(
        elide_nested_monitors(&mut bare),
        1,
        "the unrecorded pair is misread"
    );
    assert_eq!(bare.nodes[ew as usize].op, Op::Dead);
    assert_eq!(bare.nodes[xw as usize].op, Op::MonitorExit);

    let (mut g, ops) = region_around_a_splice(true);
    assert_eq!(elide_nested_monitors(&mut g), 0);
    for id in ops {
        assert_ne!(g.nodes[id as usize].op, Op::Dead, "n{id} survives");
    }
}

/// `o.leaf(); synchronized (o) { .. }` shaped adjacency: a window exit
/// followed on one control node by an enter on the same reference.
/// Returns `(graph, window enter, window exit, next enter, next exit)`.
fn window_then_region(next_is_window: bool) -> (Graph, [NodeId; 4]) {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let o = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let ew = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, m0, o], Some(2));
    let xw = g.add(Op::MonitorExit, IrType::Memory, vec![c0, ew, o], Some(2));
    let e2 = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, xw, o], Some(6));
    let x2 = g.add(Op::MonitorExit, IrType::Memory, vec![c0, e2, o], Some(6));
    let ret = g.add(Op::Return, IrType::Void, vec![c0], Some(9));
    g.exit = ret;
    g.sync_splice_windows.push(SyncSpliceWindow {
        enter: ew,
        exit: xw,
        monitor: o,
        static_class_id: 0,
    });
    if next_is_window {
        g.sync_splice_windows.push(SyncSpliceWindow {
            enter: e2,
            exit: x2,
            monitor: o,
            static_class_id: 0,
        });
    }
    (g, [ew, xw, e2, x2])
}

/// SS-4: a window half is never merged with a monitor op outside every
/// window (the lowerer could pair neither and refused the compile); two
/// windows still merge, and `ir_lower` re-pairs them.
#[test]
fn a_mixed_window_coarsening_is_refused_and_two_windows_still_merge() {
    let (mut mixed, ops) = window_then_region(false);
    assert_eq!(coarsenable_monitor_pair(&mixed, ops[1]), None);
    assert_eq!(coarsen_adjacent_monitors(&mut mixed), 0);
    for id in ops {
        assert_ne!(mixed.nodes[id as usize].op, Op::Dead, "n{id} survives");
    }

    let (mut both, [ew, xw, e2, x2]) = window_then_region(true);
    assert_eq!(coarsen_adjacent_monitors(&mut both), 1);
    assert_eq!(both.nodes[xw as usize].op, Op::Dead);
    assert_eq!(both.nodes[e2 as usize].op, Op::Dead);
    assert_eq!(both.nodes[ew as usize].op, Op::MonitorEnter);
    assert_eq!(both.nodes[x2 as usize].op, Op::MonitorExit);
}
