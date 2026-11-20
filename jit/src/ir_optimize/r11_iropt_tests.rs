use super::*;

fn graph_with_start() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    (g, start)
}

/// `for (r = 0; r < 10; r++) for (i = 0; i < 10; i++) {}` — the shape of
/// `test_loop_headers_finds_the_inner_header_of_a_nested_loop`.
struct Nest {
    g: Graph,
    c0: NodeId,
    outer: NodeId,
    inner: NodeId,
    obody: NodeId,
    ibody: NodeId,
    iexit: NodeId,
    r: NodeId,
    i: NodeId,
}

fn nested() -> Nest {
    let (mut g, start) = graph_with_start();
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let outer = g.add(Op::Merge, IrType::Control, vec![c0], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let r = g.add(Op::Phi, IrType::Int, vec![outer, zero], None);
    let ten = g.add(Op::Const(10), IrType::Int, vec![], None);
    let ocond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![r, ten], None);
    let oif = g.add(Op::If, IrType::Control, vec![outer, ocond], None);
    let obody = g.add(Op::Proj(0), IrType::Control, vec![oif], None);
    let oexit = g.add(Op::Proj(1), IrType::Control, vec![oif], None);
    let inner = g.add(Op::Merge, IrType::Control, vec![obody], None);
    let i = g.add(Op::Phi, IrType::Int, vec![inner, zero], None);
    let icond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, ten], None);
    let iif = g.add(Op::If, IrType::Control, vec![inner, icond], None);
    let ibody = g.add(Op::Proj(0), IrType::Control, vec![iif], None);
    let iexit = g.add(Op::Proj(1), IrType::Control, vec![iif], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let inext = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[i as usize].inputs.push(inext);
    g.nodes[inner as usize].inputs.push(ibody);
    let rnext = g.add(Op::Add, IrType::Int, vec![r, one], None);
    g.nodes[r as usize].inputs.push(rnext);
    g.nodes[outer as usize].inputs.push(iexit);
    let ret = g.add(Op::Return, IrType::Void, vec![oexit, r], None);
    g.exit = ret;
    Nest {
        g,
        c0,
        outer,
        inner,
        obody,
        ibody,
        iexit,
        r,
        i,
    }
}

/// The prefilter `loop_headers` and `unroll` now apply must keep BOTH
/// headers of a nest, and nothing else.
#[test]
fn retreating_edge_targets_are_exactly_the_headers_of_a_nest() {
    let n = nested();
    let users = build_users(&n.g);
    let t = retreating_edge_targets(&n.g, &users).expect("the graph has a Start");
    let mut got: Vec<NodeId> = t.into_iter().collect();
    got.sort_unstable();
    let mut want = vec![n.outer, n.inner];
    want.sort_unstable();
    assert_eq!(got, want);
    let mut hs: Vec<(NodeId, NodeId)> =
        loop_headers(&n.g).iter().map(|&(h, e, _)| (h, e)).collect();
    hs.sort_unstable();
    let mut want_hs = vec![(n.outer, n.c0), (n.inner, n.obody)];
    want_hs.sort_unstable();
    assert_eq!(hs, want_hs);
}

#[test]
fn retreating_edge_targets_needs_a_start() {
    let (mut g, _start) = graph_with_start();
    let c = g.add(Op::Const(1), IrType::Int, vec![], None);
    g.entry = c;
    let users = build_users(&g);
    assert!(
        retreating_edge_targets(&g, &users).is_none(),
        "no Start at `entry` means no prefilter: every merge stays a candidate"
    );
}

/// An enclosing loop's induction φ holds one value for the whole run of
/// the inner loop. The old rule demanded invariant value inputs as well,
/// and `r`'s back-edge input `r + 1` leads back to `r` — which bottomed out
/// at the depth bound as VARIANT, so `row.length` / `node.val` of an outer
/// induction value never hoisted out of any inner loop.
#[test]
fn an_outer_induction_phi_is_invariant_in_the_inner_loop() {
    let n = nested();
    let inner_body = loop_body(&n.g, n.inner, n.obody, &[n.ibody]);
    assert!(is_loop_invariant(&n.g, n.r, n.inner, &inner_body));
    assert!(
        !is_loop_invariant(&n.g, n.i, n.inner, &inner_body),
        "the inner loop's own induction φ is variant"
    );
    let outer_body = loop_body(&n.g, n.outer, n.c0, &[n.iexit]);
    assert!(
        !is_loop_invariant(&n.g, n.r, n.outer, &outer_body),
        "`r` is anchored at the outer header, which is in the outer body"
    );
    assert!(
        !is_loop_invariant(&n.g, n.i, n.outer, &outer_body),
        "`i` is anchored at the inner header, which is in the outer body"
    );
}

/// A single counted loop `region`, entered from `entry_ctrl`, bounded by
/// `bound`. Returns `(region, back)`.
fn add_loop(g: &mut Graph, entry_ctrl: NodeId, bound: NodeId) -> (NodeId, NodeId) {
    let region = g.add(Op::Region, IrType::Control, vec![entry_ctrl], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(next);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let back = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let _exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    g.nodes[region as usize].inputs.push(back);
    (region, back)
}

/// `x = x ^ (x << 13)`, forty times, above a loop. Each step names `x`
/// twice, so the unmemoised walk made 2^40 visits for one query.
#[test]
fn the_invariance_query_is_linear_on_a_shared_value_dag() {
    let (mut g, start) = graph_with_start();
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let ten = g.add(Op::Const(10), IrType::Int, vec![], None);
    let (region, back) = add_loop(&mut g, c0, ten);
    let mut x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let thirteen = g.add(Op::Const(13), IrType::Int, vec![], None);
    for _ in 0..40 {
        let s = g.add(Op::Shl, IrType::Int, vec![x, thirteen], None);
        x = g.add(Op::Xor, IrType::Int, vec![x, s], None);
    }
    let body = loop_body(&g, region, c0, &[back]);
    assert!(is_loop_invariant(&g, x, region, &body));
}

/// A pre-loop load below a long chain of `if`/`else` diamonds. The walk
/// used to climb the load's whole dominating control chain: exponential in
/// the diamonds, and past the depth bound it answered VARIANT, so a loop
/// late in a long method could not treat a field-loaded array as
/// invariant. A load pinned outside the body now answers at once.
#[test]
fn a_load_pinned_before_the_loop_is_invariant_however_deep_its_control() {
    let (mut g, start) = graph_with_start();
    let mut ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Int, vec![start], None);
    for _ in 0..48 {
        let iff = g.add(Op::If, IrType::Control, vec![ctrl, p], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        ctrl = g.add(Op::Merge, IrType::Control, vec![t, f], None);
    }
    let this = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let arr = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, m0, this, off],
        None,
    );
    let (region, back) = add_loop(&mut g, ctrl, p);
    let body = loop_body(&g, region, ctrl, &[back]);
    assert!(!body.contains(&arr));
    assert!(is_loop_invariant(&g, arr, region, &body));

    // A load pinned INSIDE the body stays variant.
    let inside = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, m0, this, off],
        None,
    );
    let body = loop_body(&g, region, ctrl, &[back]);
    assert!(body.contains(&inside));
    assert!(!is_loop_invariant(&g, inside, region, &body));
}

/// `for (…) s += this.f; sink(s);` — the call runs once, AFTER the loop.
/// `loop_body`'s sweep used to admit the exit projection (its `If` is in
/// the body), then everything anchored at it, so the call became a hard
/// barrier of the loop and the invariant load stayed in it.
#[test]
fn a_call_after_the_loop_is_not_a_barrier_of_the_loop() {
    let (mut g, start) = graph_with_start();
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let ten = g.add(Op::Const(10), IrType::Int, vec![], None);
    let (region, back) = add_loop(&mut g, c0, ten);
    let exit = g.nodes.len() as NodeId - 1; // `add_loop`'s exit projection
    assert_eq!(g.nodes[exit as usize].op, Op::Proj(1));
    let this = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, m0, this, off],
        None,
    );
    let call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Int,
        vec![exit, m0, load],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![exit, call], None);
    g.exit = ret;

    let body = loop_body(&g, region, c0, &[back]);
    assert!(body.contains(&load));
    assert!(
        !body.contains(&exit),
        "the exit projection is not in the loop"
    );
    assert!(
        !body.contains(&call),
        "a call pinned after the exit is not in the loop"
    );
    assert!(!loop_has_hard_barrier(&g, &body));

    assert!(licm(&mut g));
    assert_eq!(
        g.nodes[load as usize].inputs[0], c0,
        "the invariant load reaches the pre-header"
    );
}

/// The same leak, one level up: for an inner loop it went round the OUTER
/// back edge and swallowed the inner loop's own pre-header, and
/// `licm_outcome` skips the general hoist for a loop whose pre-header is in
/// its body.
#[test]
fn an_inner_loop_body_does_not_swallow_its_pre_header() {
    let n = nested();
    let body = loop_body(&n.g, n.inner, n.obody, &[n.ibody]);
    assert!(!body.contains(&n.obody));
    assert!(!body.contains(&n.outer));
    assert!(!body.contains(&n.iexit));
    assert!(body.contains(&n.inner) && body.contains(&n.ibody) && body.contains(&n.i));
}
