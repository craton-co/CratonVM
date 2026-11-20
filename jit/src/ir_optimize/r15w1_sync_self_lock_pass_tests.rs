use super::*;
use crate::ir::IrBuilder;

/// `synchronized int min(int a, int b) { if (a < b) return a; return b; }`
/// built as a self-locking body.
fn self_locking_min() -> Graph {
    let code = [0x1b, 0x1c, 0xa2, 0x00, 0x05, 0x1b, 0xac, 0x1c, 0xac];
    let mut b = IrBuilder::new(3, 3);
    b.set_param_types(&[IrType::Ref, IrType::Int, IrType::Int]);
    b.graph.receiver_param = Some(0);
    b.set_method_self_lock(true);
    b.build(&code, code.len()).expect("builds")
}

/// Round 15 wave 1 (lane sync, SS8-2): the method monitor's enter sits
/// under a bci-0 snapshot that already names its object, which is exactly
/// how a nested region's enter reads -- the pass must not delete it (every
/// exit would then release a lock never taken), nor coarsen any of its ops.
#[test]
fn the_method_monitor_is_neither_nested_nor_coarsened() {
    let mut g = self_locking_min();
    let lock = g.method_self_lock.clone().expect("a self-lock");
    assert_eq!(elide_nested_monitors(&mut g), 0);
    assert_eq!(coarsen_adjacent_monitors(&mut g), 0);
    assert_eq!(g.nodes[lock.enter as usize].op, Op::MonitorEnter);
    for &x in &lock.exits {
        assert_eq!(g.nodes[x as usize].op, Op::MonitorExit);
    }
}
