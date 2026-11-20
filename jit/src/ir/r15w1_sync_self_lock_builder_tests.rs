use super::*;

/// `synchronized int min(int a, int b) { if (a < b) return a; return b; }`:
/// iload_1; iload_2; if_icmpge +5; iload_1; ireturn; iload_2; ireturn.
const MIN: [u8; 9] = [0x1b, 0x1c, 0xa2, 0x00, 0x05, 0x1b, 0xac, 0x1c, 0xac];

/// An instance method `(II)I` (`this` in local 0, a receiver), built with
/// or without the self-lock `lib.rs` asks for.
fn build(code: &[u8], receiver: bool, self_lock: bool) -> Option<Graph> {
    let mut b = IrBuilder::new(3, 3);
    b.set_param_types(&[IrType::Ref, IrType::Int, IrType::Int]);
    b.graph.receiver_param = receiver.then_some(0);
    b.set_method_self_lock(self_lock);
    b.build(code, code.len())
}

fn count(g: &Graph, op: Op) -> usize {
    g.nodes.iter().filter(|n| n.op == op).count()
}

fn receiver(g: &Graph) -> NodeId {
    let at = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Param(0)))
        .expect("a Param(0)");
    NodeId::try_from(at).expect("a small graph")
}

/// The enter is on the entry memory, before the first snapshot; every
/// snapshot names the hold once; one exit per return, at its bci.
#[test]
fn a_self_locking_body_locks_on_entry_and_releases_before_each_return() {
    let g = build(&MIN, true, true).expect("builds");
    let this = receiver(&g);
    let lock = g.method_self_lock.clone().expect("a self-lock is recorded");
    assert_eq!(lock.monitor, this);
    let enter = &g.nodes[lock.enter as usize];
    assert_eq!(enter.op, Op::MonitorEnter);
    assert_eq!(enter.inputs.get(2), Some(&this));
    assert_eq!(enter.bytecode_pc, Some(0));
    // Node 2 is the entry memory (`Proj(1)` of `Start`).
    assert_eq!(enter.inputs.get(1), Some(&2));
    assert_eq!(count(&g, Op::MonitorEnter), 1);
    assert_eq!(count(&g, Op::MonitorExit), 2);
    assert_eq!(count(&g, Op::Return), 2);
    let exit_pcs: Vec<Option<usize>> = lock
        .exits
        .iter()
        .map(|&x| {
            let n = &g.nodes[x as usize];
            assert_eq!(n.op, Op::MonitorExit);
            assert_eq!(n.inputs.get(2), Some(&this));
            n.bytecode_pc
        })
        .collect();
    assert_eq!(exit_pcs, vec![Some(6), Some(8)]);
    assert!(!g.safepoints.is_empty());
    for sp in g.safepoints.iter() {
        assert_eq!(sp.monitors, vec![this], "the snapshot at bci {}", sp.bci);
    }
}

/// Off, the same body takes no monitor and records nothing.
#[test]
fn without_the_request_nothing_changes() {
    let g = build(&MIN, true, false).expect("builds");
    assert!(g.method_self_lock.is_none());
    assert_eq!(count(&g, Op::MonitorEnter), 0);
    assert_eq!(count(&g, Op::MonitorExit), 0);
    assert!(g.safepoints.iter().all(|sp| sp.monitors.is_empty()));
}

/// Every shape the hand-over cannot carry refuses the BUILD; none is built
/// without the lock.
#[test]
fn shapes_the_self_lock_cannot_carry_refuse_the_build() {
    // aconst_null; astore_0; iconst_0; ireturn
    let store0 = [0x01, 0x4b, 0x03, 0xac];
    assert!(build(&store0, true, true).is_none(), "a store to local 0");
    // aload_0; monitorenter; aload_0; monitorexit; iconst_0; ireturn
    let region = [0x2a, 0xc2, 0x2a, 0xc3, 0x03, 0xac];
    assert!(build(&region, true, true).is_none(), "monitor bytecode");
    assert!(
        build(&region, true, false).is_some(),
        "the region builds unlocked"
    );
    assert!(build(&MIN, false, true).is_none(), "no receiver claim");
}

/// `athrow` leaves through the exception exit, which `ir_lower` makes
/// release; the body builds, with no exit of its own.
#[test]
fn a_throw_under_the_method_monitor_builds() {
    // aload_0; athrow
    let g = build(&[0x2a, 0xbf], true, true).expect("builds");
    let lock = g.method_self_lock.clone().expect("a self-lock");
    assert!(lock.exits.is_empty());
    assert_eq!(count(&g, Op::Throw), 1);
    assert_eq!(count(&g, Op::MonitorEnter), 1);
}

/// A loop whose header is bci 0: the enter stays on the entry edge (one
/// enter, entry control), and the header's snapshot names the hold.
#[test]
fn a_loop_at_bci_zero_does_not_re_enter() {
    // L: iload_1; ifeq END; iinc 1 -1; goto L; END: iconst_0; ireturn
    let code = [
        0x1b, 0x99, 0x00, 0x09, 0x84, 0x01, 0xff, 0xa7, 0xff, 0xf9, 0x03, 0xac,
    ];
    let g = build(&code, true, true).expect("builds");
    let this = receiver(&g);
    let lock = g.method_self_lock.clone().expect("a self-lock");
    assert_eq!(count(&g, Op::MonitorEnter), 1);
    // Node 1 is the entry control (`Proj(0)` of `Start`).
    assert_eq!(g.nodes[lock.enter as usize].inputs.get(0), Some(&1));
    assert_eq!(lock.exits.len(), 1);
    assert!(g.safepoints.iter().any(|sp| sp.bci == 0));
    for sp in g.safepoints.iter() {
        assert_eq!(sp.monitors, vec![this], "the snapshot at bci {}", sp.bci);
    }
}
