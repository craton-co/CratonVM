use super::*;

fn graph_with_start() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    (g, start)
}

/// The nodes of [`arraylength_loop`].
struct AlLoop {
    g: Graph,
    region: NodeId,
    preheader: NodeId,
    iv: NodeId,
    zero: NodeId,
    arr: NodeId,
    mem_phi: NodeId,
    len: NodeId,
}

/// `for (i = 0; i < a.length; i++) {}` with the header test at bci 20 and
/// a snapshot there naming the induction phi as a local and on the stack.
fn arraylength_loop() -> AlLoop {
    let (mut g, start) = graph_with_start();
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(20));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![region, mem_phi, arr],
        Some(20),
    );
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, len], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(21));
    let back = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[region as usize].inputs.push(back);
    let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
    g.exit = ret;
    g.safepoints.push(SafepointSnapshot::new(
        20,
        vec![arr, iv],
        vec![iv, arr],
        vec![],
    ));
    AlLoop {
        g,
        region,
        preheader: c0,
        iv,
        zero,
        arr,
        mem_phi,
        len,
    }
}

/// The hoisted `arraylength` deopts from the pre-header, where the phi's
/// home is not written yet. Its frame must name the phi's ENTRY value.
#[test]
fn a_hoisted_arraylength_deopts_with_the_loop_entry_frame() {
    let mut l = arraylength_loop();
    assert!(licm(&mut l.g));
    assert_eq!(l.g.nodes[l.len as usize].inputs[0], l.preheader, "hoisted");
    let si = l.g.nodes[l.len as usize]
        .frame_snapshot
        .expect("the hoisted node carries a frame of its own");
    let sp = &l.g.safepoints[si as usize];
    assert_eq!(sp.bci, 20);
    assert_eq!(sp.locals, vec![l.arr, l.zero], "not the phi {}", l.iv);
    assert_eq!(sp.stack, vec![l.zero, l.arr]);
    assert_eq!(
        l.g.safepoints[0].locals,
        vec![l.arr, l.iv],
        "the original snapshot is left alone"
    );
}

/// A store the header runs before the `arraylength`'s bci would be skipped
/// by a resume at that bci: no hoist.
#[test]
fn a_header_store_before_the_arraylength_blocks_its_hoist() {
    let mut l = arraylength_loop();
    let _st = l.g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![l.region, l.mem_phi, l.arr, l.zero, l.zero],
        Some(15),
    );
    let _ = licm(&mut l.g);
    assert_eq!(l.g.nodes[l.len as usize].inputs[0], l.region, "not hoisted");
    assert_eq!(l.g.nodes[l.len as usize].frame_snapshot, None);
}

/// A snapshot slot naming a value computed inside the loop (not a header
/// phi) cannot be described at the pre-header: no hoist.
#[test]
fn a_frame_naming_an_in_loop_value_blocks_the_hoist() {
    let mut l = arraylength_loop();
    let one = l.g.add(Op::Const(1), IrType::Int, vec![], None);
    let bumped = l.g.add(Op::Add, IrType::Int, vec![l.iv, one], None);
    l.g.safepoints[0].stack = vec![bumped, l.arr];
    let _ = licm(&mut l.g);
    assert_eq!(l.g.nodes[l.len as usize].inputs[0], l.region, "not hoisted");
}

/// `for (r < 10) { for (i = 0; i < 4; i++) a += i * s; }` where `s` is the
/// OUTER loop's phi, fed by the inner loop's result. The inner header has
/// both inputs reachable from itself, so it used to be
/// `not_single_backedge`; with dominance alone, the variance closure ran
/// out through the outer phi and tried to clone it.
#[test]
fn an_inner_loop_with_a_constant_trip_is_fully_unrolled() {
    let (mut g, start) = graph_with_start();
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let outer = g.add(Op::Merge, IrType::Control, vec![c0], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let four = g.add(Op::Const(4), IrType::Int, vec![], None);
    let ten = g.add(Op::Const(10), IrType::Int, vec![], None);
    let r = g.add(Op::Phi, IrType::Int, vec![outer, zero], None);
    let s = g.add(Op::Phi, IrType::Int, vec![outer, one], None);
    let ocond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![r, ten], None);
    let oif = g.add(Op::If, IrType::Control, vec![outer, ocond], None);
    let obody = g.add(Op::Proj(0), IrType::Control, vec![oif], None);
    let oexit = g.add(Op::Proj(1), IrType::Control, vec![oif], None);
    let inner = g.add(Op::Merge, IrType::Control, vec![obody], None);
    let i = g.add(Op::Phi, IrType::Int, vec![inner, zero], None);
    let a = g.add(Op::Phi, IrType::Int, vec![inner, s], None);
    let icond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, four], None);
    let iif = g.add(Op::If, IrType::Control, vec![inner, icond], None);
    let ibody = g.add(Op::Proj(0), IrType::Control, vec![iif], None);
    let iexit = g.add(Op::Proj(1), IrType::Control, vec![iif], None);
    let prod = g.add(Op::Mul, IrType::Int, vec![i, s], None);
    let anext = g.add(Op::Add, IrType::Int, vec![a, prod], None);
    let inext = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[i as usize].inputs.push(inext);
    g.nodes[a as usize].inputs.push(anext);
    g.nodes[inner as usize].inputs.push(ibody);
    let rnext = g.add(Op::Add, IrType::Int, vec![r, one], None);
    g.nodes[r as usize].inputs.push(rnext);
    g.nodes[s as usize].inputs.push(a);
    g.nodes[outer as usize].inputs.push(iexit);
    let ret = g.add(Op::Return, IrType::Int, vec![oexit, s], None);
    g.exit = ret;

    assert!(unroll(&mut g), "the inner loop unrolls");
    assert_eq!(g.nodes[inner as usize].op, Op::Dead, "inner header retired");
    assert_eq!(g.nodes[outer as usize].op, Op::Merge, "outer loop intact");
    assert_eq!(g.nodes[outer as usize].inputs.as_slice(), &[c0, obody]);
    let s_back = g.nodes[s as usize].inputs[2];
    assert_ne!(s_back, a, "the outer phi now reads the unrolled result");
    assert_ne!(g.nodes[s_back as usize].op, Op::Dead);
}

/// `while (true) { x = p.f; }`: the header's back edge is the header. Loop
/// discovery sees a loop whose back edge is itself, the body is what the
/// header anchors, and the unroller (no exit test) leaves it alone.
#[test]
fn a_self_looping_header_is_a_loop_with_itself_as_back_edge() {
    let (mut g, start) = graph_with_start();
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let h = g.add(Op::Merge, IrType::Control, vec![c0], Some(4));
    g.nodes[h as usize].inputs.push(h);
    let off = g.add(Op::Const(12), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![h, m0, p, off],
        Some(6),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![c0], None);
    g.exit = ret;

    let hs = loop_headers(&g);
    assert_eq!(hs.len(), 1);
    assert_eq!(hs[0].0, h);
    assert_eq!(hs[0].1, c0);
    assert_eq!(hs[0].2, vec![h]);
    let body = loop_body(&g, h, c0, &[h]);
    assert!(body.contains(&h) && body.contains(&load));
    assert!(!body.contains(&c0));
    assert!(!unroll(&mut g), "no exit test: not a counted loop");
    assert_eq!(g.nodes[h as usize].inputs.as_slice(), &[c0, h]);
}
