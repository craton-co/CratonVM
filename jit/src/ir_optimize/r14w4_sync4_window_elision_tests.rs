use super::*;
use crate::ir::SyncSpliceWindow;

/// A window at bci 3 on `o`, with one snapshot at `snap_bci` whose
/// monitor stack is `held(o, p)`, and nothing else. Returns `(graph,
/// window enter, window exit)`.
fn lone_window(
    snap_bci: usize,
    held: impl Fn(NodeId, NodeId) -> Vec<NodeId>,
) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let o = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let p = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let ew = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, m0, o], Some(3));
    let xw = g.add(Op::MonitorExit, IrType::Memory, vec![c0, ew, o], Some(3));
    let ret = g.add(Op::Return, IrType::Void, vec![c0], Some(6));
    g.exit = ret;
    g.safepoints.push(SafepointSnapshot::new(
        snap_bci,
        vec![o, p],
        vec![],
        held(o, p),
    ));
    g.sync_splice_windows.push(SyncSpliceWindow {
        enter: ew,
        exit: xw,
        monitor: o,
        static_class_id: 0,
    });
    (g, ew, xw)
}

/// `r14w3_sync_monitor_pass_tests::region_around_a_splice(true)`:
/// `synchronized (o) { o.leaf(); }`, the region's `enter(o)` at bci 1, the
/// window at bci 3, the region's `exit(o)` at bci 7. Returns the graph and
/// `(region enter, window enter, window exit, region exit)`.
fn region_around_a_splice() -> (Graph, [NodeId; 4]) {
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
    g.sync_splice_windows.push(SyncSpliceWindow {
        enter: ew,
        exit: xw,
        monitor: o,
        static_class_id: 0,
    });
    (g, [e1, ew, xw, x1])
}

/// SY3-2: inside `synchronized (o)`, the window's pair on `o` is deleted
/// whole and dropped from the list; the region's own pair stays, and the
/// region's exit now reads the region's enter as its token.
#[test]
fn a_window_inside_a_region_on_its_object_is_deleted_whole() {
    let (mut g, [e1, ew, xw, x1]) = region_around_a_splice();
    assert_eq!(elide_region_nested_sync_windows(&mut g), 1);
    assert_eq!(g.nodes[ew as usize].op, Op::Dead);
    assert_eq!(g.nodes[xw as usize].op, Op::Dead);
    assert_eq!(g.nodes[e1 as usize].op, Op::MonitorEnter);
    assert_eq!(g.nodes[x1 as usize].op, Op::MonitorExit);
    assert_eq!(g.nodes[x1 as usize].input_opt(1), Some(e1));
    assert!(g.sync_splice_windows.is_empty());
    // Idempotent, and the bytecode pass has nothing left to misread.
    assert_eq!(elide_region_nested_sync_windows(&mut g), 0);
    assert_eq!(elide_nested_monitors(&mut g), 0);
    assert!(crate::ir::ir_sync_splice_census_line().contains("window-elided-in-region="));
}

/// Not held, held on another object, or no snapshot at the window's bci:
/// the pair stays.
#[test]
fn a_window_outside_a_region_on_its_object_is_kept() {
    let (mut free, ew, xw) = lone_window(3, |_, _| vec![]);
    assert_eq!(elide_region_nested_sync_windows(&mut free), 0);
    assert_eq!(free.nodes[ew as usize].op, Op::MonitorEnter);
    assert_eq!(free.nodes[xw as usize].op, Op::MonitorExit);
    assert_eq!(free.sync_splice_windows.len(), 1);

    let (mut other, ew, _) = lone_window(3, |_, p| vec![p]);
    assert_eq!(elide_region_nested_sync_windows(&mut other), 0);
    assert_eq!(other.nodes[ew as usize].op, Op::MonitorEnter);

    let (mut elsewhere, ew, _) = lone_window(5, |o, _| vec![o]);
    assert_eq!(elide_region_nested_sync_windows(&mut elsewhere), 0);
    assert_eq!(elsewhere.nodes[ew as usize].op, Op::MonitorEnter);

    let (mut held, ew, xw) = lone_window(3, |o, _| vec![o]);
    assert_eq!(elide_region_nested_sync_windows(&mut held), 1);
    assert_eq!(held.nodes[ew as usize].op, Op::Dead);
    assert_eq!(held.nodes[xw as usize].op, Op::Dead);
}

/// Round 14 wave 5 (lane sync5, SS8-3): inside a synchronized instance
/// method (`Graph::method_monitor_param`), a window on the receiver is
/// deleted with no snapshot naming it; one on another object, or in a
/// graph that claims no method monitor, is kept.
#[test]
fn a_window_on_the_synchronized_methods_receiver_is_deleted() {
    let build = |monitor_param: u16, claim: Option<u16>| {
        let mut g = Graph::empty(16);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let this = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let other = g.add(Op::Param(1), IrType::Ref, vec![start], None);
        let o = if monitor_param == 0 { this } else { other };
        let ew = g.add(Op::MonitorEnter, IrType::Memory, vec![c0, m0, o], Some(3));
        let xw = g.add(Op::MonitorExit, IrType::Memory, vec![c0, ew, o], Some(3));
        let ret = g.add(Op::Return, IrType::Void, vec![c0], Some(6));
        g.exit = ret;
        g.method_monitor_param = claim;
        g.sync_splice_windows.push(SyncSpliceWindow {
            enter: ew,
            exit: xw,
            monitor: o,
            static_class_id: 0,
        });
        (g, ew, xw)
    };
    let (mut held, ew, xw) = build(0, Some(0));
    assert!(held.safepoints.is_empty());
    assert_eq!(elide_region_nested_sync_windows(&mut held), 1);
    assert_eq!(held.nodes[ew as usize].op, Op::Dead);
    assert_eq!(held.nodes[xw as usize].op, Op::Dead);
    assert!(held.sync_splice_windows.is_empty());

    let (mut other, ew, _) = build(1, Some(0));
    assert_eq!(elide_region_nested_sync_windows(&mut other), 0);
    assert_eq!(other.nodes[ew as usize].op, Op::MonitorEnter);

    let (mut unclaimed, ew, _) = build(0, None);
    assert_eq!(elide_region_nested_sync_windows(&mut unclaimed), 0);
    assert_eq!(unclaimed.nodes[ew as usize].op, Op::MonitorEnter);
}

/// Round 14 wave 7 (lane sync7, RV6-1): inside a `static synchronized`
/// method of class 7 (`Graph::method_monitor_class`), a `static
/// synchronized` window of class 7 is deleted with no snapshot naming it;
/// a window of class 8, an instance window, or a graph that claims no class
/// keeps its pair.
#[test]
fn a_static_window_on_the_static_synchronized_methods_class_is_deleted() {
    let build = |window_class: u32, claim: Option<u32>| {
        let mut g = Graph::empty(16);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let mirror = g.add(
            Op::ConstClass {
                holder_class_id: 7,
                cp_idx: 5,
                slot_addr: 0,
            },
            IrType::Ref,
            vec![c0, m0],
            Some(3),
        );
        let ew = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![c0, mirror, mirror],
            Some(3),
        );
        let xw = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![c0, ew, mirror],
            Some(3),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![c0], Some(6));
        g.exit = ret;
        g.method_monitor_class = claim;
        g.sync_splice_windows.push(SyncSpliceWindow {
            enter: ew,
            exit: xw,
            monitor: mirror,
            static_class_id: window_class,
        });
        (g, ew, xw)
    };
    let (mut held, ew, xw) = build(7, Some(7));
    assert!(held.safepoints.is_empty());
    assert_eq!(elide_region_nested_sync_windows(&mut held), 1);
    assert_eq!(held.nodes[ew as usize].op, Op::Dead);
    assert_eq!(held.nodes[xw as usize].op, Op::Dead);
    assert!(held.sync_splice_windows.is_empty());

    let (mut other, ew, _) = build(8, Some(7));
    assert_eq!(elide_region_nested_sync_windows(&mut other), 0);
    assert_eq!(other.nodes[ew as usize].op, Op::MonitorEnter);

    let (mut instance, ew, _) = build(0, Some(7));
    assert_eq!(elide_region_nested_sync_windows(&mut instance), 0);
    assert_eq!(instance.nodes[ew as usize].op, Op::MonitorEnter);

    let (mut unclaimed, ew, _) = build(7, None);
    assert_eq!(elide_region_nested_sync_windows(&mut unclaimed), 0);
    assert_eq!(unclaimed.nodes[ew as usize].op, Op::MonitorEnter);
}

