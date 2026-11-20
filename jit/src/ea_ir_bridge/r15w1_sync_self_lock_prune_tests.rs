use super::*;

/// `synchronized int min(int a, int b) { if (a < b) return a; return b; }`
/// never reads `this`, so the bytecode calls local 0 dead everywhere.
const MIN: [u8; 9] = [0x1b, 0x1c, 0xa2, 0x00, 0x05, 0x1b, 0xac, 0x1c, 0xac];

fn build(self_lock: bool) -> ir::Graph {
    let mut b = ir::IrBuilder::new(3, 3);
    b.set_param_types(&[ir::IrType::Ref, ir::IrType::Int, ir::IrType::Int]);
    b.graph.receiver_param = Some(0);
    b.set_method_self_lock(self_lock);
    b.build(&MIN, MIN.len()).expect("builds")
}

/// Round 15 wave 1 (lane sync, SS8-2): a self-locking body's frames keep
/// local 0 (the VM hands the monitor over only when local 0 names the
/// locked receiver); an ordinary body's lose it where it is dead.
#[test]
fn a_self_locking_body_keeps_local_zero_in_every_frame() {
    let mut locked = build(true);
    ir_prune_dead_snapshot_locals(&mut locked, &MIN, MIN.len());
    let this = locked
        .method_self_lock
        .as_ref()
        .map(|lock| lock.monitor)
        .expect("a self-lock");
    assert!(!locked.safepoints.is_empty());
    for sp in locked.safepoints.iter() {
        assert_eq!(
            sp.locals.first(),
            Some(&this),
            "the snapshot at bci {}",
            sp.bci
        );
    }
    let mut plain = build(false);
    ir_prune_dead_snapshot_locals(&mut plain, &MIN, MIN.len());
    assert!(
        plain
            .safepoints
            .iter()
            .any(|sp| sp.locals.first() == Some(&ir::NO_NODE)),
        "the control: a dead local 0 is cut from an ordinary body's frames"
    );
}
