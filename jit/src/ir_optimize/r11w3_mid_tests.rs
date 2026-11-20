use super::*;

/// The nodes of [`body_read`].
struct BodyRead {
    g: Graph,
    region: NodeId,
    preheader: NodeId,
    back: NodeId,
    load: NodeId,
    iv: NodeId,
    zero: NodeId,
    p: NodeId,
}

/// `for (i = 0; i < 1000; i++) { [p.w = 0;] s = p.v; }`: header at bci 4,
/// exit test at bci 8, the read at bci 18 anchored on the test's stay
/// projection (the single back edge), and a snapshot at 18 naming the
/// induction φ. With `with_store` a `putfield` at bci 13 runs before the
/// read on every iteration.
fn body_read(with_store: bool) -> BodyRead {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(4));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let bound = g.add(Op::Const(1000), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(8));
    let back = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    if with_store {
        let off_w = g.add(Op::Const(1), IrType::Int, vec![], None);
        let _ = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![back, mem_phi, p, off_w, zero],
            Some(13),
        );
    }
    let off_v = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![back, mem_phi, p, off_v],
        Some(18),
    );
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[region as usize].inputs.push(back);
    let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
    g.exit = ret;
    g.safepoints
        .push(SafepointSnapshot::new(18, vec![p, iv], vec![p], vec![]));
    BodyRead {
        g,
        region,
        preheader: c0,
        back,
        load,
        iv,
        zero,
        p,
    }
}

/// The counted-trip widening (`CRATONVM_JIT_IR_LICM_HOIST_COUNTED`) moves a
/// BODY-anchored read to the pre-header. Its deopt must rebuild the first
/// iteration's frame at its own bci: the induction φ at its entry value.
#[test]
fn a_body_anchored_read_gets_the_first_iteration_frame() {
    let mut b = body_read(false);
    let body = loop_body(&b.g, b.region, b.preheader, &[b.back]);
    assert!(install_preheader_deopt_frame(
        &mut b.g,
        b.load,
        b.region,
        b.preheader,
        &body,
        true
    ));
    let si = b.g.nodes[b.load as usize]
        .frame_snapshot
        .expect("the read carries a frame of its own");
    let sp = &b.g.safepoints[si as usize];
    assert_eq!(sp.bci, 18);
    assert_eq!(sp.locals, vec![b.p, b.zero], "not the phi {}", b.iv);
    assert_eq!(sp.stack, vec![b.p]);
}

/// Without the caller's proof that the body runs, a body anchor is still
/// refused: the pre-header would run a read a zero-trip loop never does.
#[test]
fn a_body_anchored_read_is_refused_without_the_trip_proof() {
    let mut b = body_read(false);
    let body = loop_body(&b.g, b.region, b.preheader, &[b.back]);
    assert!(!install_preheader_deopt_frame(
        &mut b.g,
        b.load,
        b.region,
        b.preheader,
        &body,
        false
    ));
    assert_eq!(b.g.nodes[b.load as usize].frame_snapshot, None);
}

/// A store the first iteration runs before the read would be skipped by a
/// resume at the read's bci: refused.
#[test]
fn a_body_store_before_the_read_blocks_the_counted_trip_hoist() {
    let mut b = body_read(true);
    let body = loop_body(&b.g, b.region, b.preheader, &[b.back]);
    assert!(!install_preheader_deopt_frame(
        &mut b.g,
        b.load,
        b.region,
        b.preheader,
        &body,
        true
    ));
    assert_eq!(b.g.nodes[b.load as usize].frame_snapshot, None);
}
