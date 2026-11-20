use super::*;

/// `static synchronized int min(int a, int b) { if (a < b) return a;
/// return b; }`: iload_0; iload_1; if_icmpge +5; iload_0; ireturn;
/// iload_1; ireturn.
const MIN: [u8; 9] = [0x1a, 0x1b, 0xa2, 0x00, 0x05, 0x1a, 0xac, 0x1b, 0xac];
const CLASS: u32 = 7;
const SLOT: usize = 0x1000_2000;

/// A static method `(II)I`, built as a self-locking body on `mirror`.
fn build(receiver: bool, mirror: (u32, usize)) -> Option<Graph> {
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Int, IrType::Int]);
    b.graph.receiver_param = receiver.then_some(0);
    b.set_method_self_lock(true);
    b.set_method_self_lock_mirror(mirror.0, mirror.1);
    b.build(&MIN, MIN.len())
}

/// Round 15 wave 3 (lane sync2, S15-3): the monitor is ONE `ConstClass`
/// of the mirror slot at bci 0 on the entry control and memory, the enter
/// locks it (and takes it as its memory), every snapshot names it, and
/// each return releases it.
#[test]
fn a_static_self_locking_body_locks_its_class_mirror() {
    let g = build(false, (CLASS, SLOT)).expect("builds");
    let lock = g.method_self_lock.clone().expect("a self-lock is recorded");
    let m = &g.nodes[lock.monitor as usize];
    assert_eq!(
        m.op,
        Op::ConstClass {
            holder_class_id: CLASS,
            cp_idx: SELF_LOCK_MIRROR_CP_IDX,
            slot_addr: SLOT,
        }
    );
    assert_eq!(m.ty, IrType::Ref);
    assert_eq!(m.bytecode_pc, Some(0));
    // Nodes 1 and 2 are the entry control and memory (`Proj(0)`/`Proj(1)`).
    let entry: Vec<NodeId> = vec![1, 2];
    assert_eq!(m.inputs, entry);
    let enter = &g.nodes[lock.enter as usize];
    assert_eq!(enter.op, Op::MonitorEnter);
    let locked: Vec<NodeId> = vec![1, lock.monitor, lock.monitor];
    assert_eq!(enter.inputs, locked);
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| matches!(n.op, Op::ConstClass { .. }))
            .count(),
        1
    );
    let exit_pcs: Vec<Option<usize>> = lock
        .exits
        .iter()
        .map(|&x| {
            let n = &g.nodes[x as usize];
            assert_eq!(n.op, Op::MonitorExit);
            assert_eq!(n.inputs.get(2), Some(&lock.monitor));
            n.bytecode_pc
        })
        .collect();
    assert_eq!(exit_pcs, vec![Some(6), Some(8)]);
    assert!(!g.safepoints.is_empty());
    for sp in g.safepoints.iter() {
        assert_eq!(
            sp.monitors,
            vec![lock.monitor],
            "the snapshot at bci {}",
            sp.bci
        );
    }
}

/// No class, no slot, or a receiver claim: the build is refused, never
/// built without the lock.
#[test]
fn a_static_self_lock_without_its_mirror_refuses_the_build() {
    assert!(build(false, (0, SLOT)).is_none(), "no class");
    assert!(build(false, (CLASS, 0)).is_none(), "no slot");
    assert!(build(true, (CLASS, SLOT)).is_none(), "a receiver claim");
}
