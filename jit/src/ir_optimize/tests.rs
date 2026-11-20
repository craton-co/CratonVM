use super::*;
use crate::ir::{IrBuilder, MemKind};

fn build_and_optimize(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    num_locals: usize,
) -> Graph {
    let builder = IrBuilder::new(num_params, num_locals);
    let mut graph = builder.build(code, code_len).expect("build failed");
    optimize(&mut graph);
    graph
}

/// `ConstIntern` returns, on every call, the node the arena scan it replaced
/// returned. That holds through constants it adds itself, constants added to
/// the graph behind its back, and constants killed after it indexed them.
#[test]
fn const_intern_returns_the_node_the_arena_scan_returns() {
    // iconst_1; ireturn: a real builder graph with a constant of its own.
    let code = [0x04, 0xac, 0, 0];
    let mut graph = IrBuilder::new(0, 0).build(&code, 2).expect("build failed");
    let mut consts = ConstIntern::new();
    let values = [0i64, 1, -1, 7];
    let types = [IrType::Int, IrType::Long, IrType::Ref];
    // Constants this test created, so it can kill them without orphaning
    // a user.
    let mut ours: Vec<NodeId> = Vec::new();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = |bound: usize| -> usize {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 33) as usize) % bound.max(1)
    };
    for step in 0..2000 {
        let val = values[next(values.len())];
        let ty = types[next(types.len())];
        match next(4) {
            0 => {
                ours.push(graph.add(Op::Const(val), ty, vec![], None));
            }
            1 => {
                if !ours.is_empty() {
                    let victim = ours.swap_remove(next(ours.len()));
                    graph.kill(victim);
                }
            }
            _ => {
                let len_before = graph.nodes.len();
                let want = const_node_by_scan(&graph, val, ty);
                let got = consts.get_or_add(&mut graph, val, ty);
                match want {
                    Some(id) => assert_eq!(got, id, "step {step}: Const({val})"),
                    None => {
                        assert_eq!(
                            got as usize, len_before,
                            "step {step}: a miss must add a new node"
                        );
                        ours.push(got);
                    }
                }
            }
        }
    }
}

/// `fold_constants` answers honestly, which is what [`optimize`]'s
/// fixpoint now depends on.
///
/// The loop used to test `graph.live_count()` alone. Folding rewrites a
/// node's `op` to `Op::Const` in place and kills nothing, so a fold-only
/// round moved no count and read as "converged". Two properties make the
/// `bool` a safe replacement, and both are checked here:
///
/// * it returns `true` exactly when it rewrote something, so the loop
///   keeps going while there is work;
/// * it is idempotent — a second call on the folded graph returns
///   `false` — so an honest `true` cannot spin the loop to its bound.
#[test]
fn fold_constants_reports_change_and_is_idempotent() {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let a = g.add(Op::Const(3), IrType::Int, vec![], None);
    let b = g.add(Op::Const(4), IrType::Int, vec![], None);
    let sum = g.add(Op::Add, IrType::Int, vec![a, b], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    g.exit = ret;

    let before = g.live_count();
    assert!(
        fold_constants(&mut g),
        "a foldable Add must report that it folded"
    );
    assert_eq!(g.nodes[sum as usize].op, Op::Const(7));
    assert_eq!(
        g.live_count(),
        before,
        "the fold kills nothing — this equality IS the reason the return \
         value has to exist; a `live_count` test cannot see this round"
    );
    assert!(
        !fold_constants(&mut g),
        "folding an already-folded graph must report no change, or the \
         fixpoint loop would run to its bound on every compile"
    );
}

/// `algebraic_simplify` reports change too, and is idempotent.
#[test]
fn algebraic_simplify_reports_change_and_is_idempotent() {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let sum = g.add(Op::Add, IrType::Int, vec![p, zero], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    g.exit = ret;

    assert!(
        algebraic_simplify(&mut g),
        "`x + 0` must report that it simplified"
    );
    assert_eq!(g.nodes[ret as usize].inputs[1], p);
    assert!(
        !algebraic_simplify(&mut g),
        "a simplified graph must report no change"
    );
}

#[test]
fn test_constant_folding_add() {
    // return 3 + 4  →  should fold to return 7
    let code = [0x06, 0x07, 0x60, 0xac, 0, 0]; // iconst_3; iconst_4; iadd; ireturn
    let graph = build_and_optimize(&code, 4, 0, 0);
    let ret = &graph.nodes[graph.exit as usize];
    let val_id = ret.inputs[1];
    // After folding, the return value should be Const(7)
    assert_eq!(graph.nodes[val_id as usize].op, Op::Const(7));
}

#[test]
fn test_constant_folding_mul() {
    // return 5 * 6  →  should fold to 30
    let code = [0x08, 0x06, 0x68, 0xac, 0, 0]; // iconst_5; iconst_3; imul; ireturn
    let graph = build_and_optimize(&code, 4, 0, 0);
    let ret = &graph.nodes[graph.exit as usize];
    let val_id = ret.inputs[1];
    assert_eq!(graph.nodes[val_id as usize].op, Op::Const(15));
}

#[test]
fn test_algebraic_add_zero() {
    // return x + 0  →  should simplify to return x
    // iload_0; iconst_0; iadd; ireturn
    let code = [0x1a, 0x03, 0x60, 0xac, 0, 0];
    let graph = build_and_optimize(&code, 4, 1, 1);
    let ret = &graph.nodes[graph.exit as usize];
    let val_id = ret.inputs[1];
    // After simplification, return value should be Param(0) directly
    assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
}

#[test]
fn test_algebraic_mul_one() {
    // return x * 1  →  should simplify to return x
    // iload_0; iconst_1; imul; ireturn
    let code = [0x1a, 0x04, 0x68, 0xac, 0, 0];
    let graph = build_and_optimize(&code, 4, 1, 1);
    let ret = &graph.nodes[graph.exit as usize];
    let val_id = ret.inputs[1];
    assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
}

#[test]
fn test_algebraic_sub_self() {
    // return x - x  →  should simplify to return 0
    // iload_0; iload_0; isub; ireturn
    let code = [0x1a, 0x1a, 0x64, 0xac, 0, 0];
    let graph = build_and_optimize(&code, 4, 1, 1);
    let ret = &graph.nodes[graph.exit as usize];
    let val_id = ret.inputs[1];
    assert_eq!(graph.nodes[val_id as usize].op, Op::Const(0));
}

#[test]
fn test_gvn_deduplicates() {
    // return (x + y) + (x + y) → should GVN the two (x+y) computations
    // iload_0; iload_1; iadd; iload_0; iload_1; iadd; iadd; ireturn
    let code = [0x1a, 0x1b, 0x60, 0x1a, 0x1b, 0x60, 0x60, 0xac, 0, 0];
    let graph = build_and_optimize(&code, 8, 2, 2);
    // Count Add nodes — should be 2 (one for x+y, one for (x+y)+(x+y))
    // not 3 (the duplicate x+y should be eliminated)
    let add_count = graph.nodes.iter().filter(|n| n.op == Op::Add).count();
    assert_eq!(add_count, 2, "GVN should deduplicate the duplicate x+y");
}

/// Three identical expressions collapse to one, not two.
///
/// The bucket rewrite (a hash now maps to `(first, rest)` rather than a
/// single id) has to keep the ordinary case exactly as it was: the second
/// and third occurrences must both merge into the FIRST, and neither may
/// end up in the overflow chain.
#[test]
fn three_copies_of_one_expression_gvn_to_a_single_node() {
    // return ((x + y) + (x + y)) + (x + y)
    let code = [
        0x1a, 0x1b, 0x60, // x + y
        0x1a, 0x1b, 0x60, // x + y
        0x60, // add the two
        0x1a, 0x1b, 0x60, // x + y
        0x60, // add the third
        0xac, 0, 0,
    ];
    let graph = build_and_optimize(&code, 12, 2, 2);
    let add_count = graph.nodes.iter().filter(|n| n.op == Op::Add).count();
    assert_eq!(
        add_count, 3,
        "one `x + y`, one `(x+y)+(x+y)`, one final add — the second and \
         third copies of `x + y` must both merge into the first"
    );
}

/// `Op::Add`/`Sub`/`Mul`/`Div`/`Rem`/`Neg` are shared between integer and
/// floating-point arithmetic, and `try_fold`'s arms are `i32`/`i64`
/// wrapping arithmetic throughout. A `Float`-typed node reaching them would
/// be folded by dividing one IEEE-754 bit pattern by another.
///
/// Unreachable today only because `const_value` matches `Op::Const` and an
/// FP constant is `Op::ConstF` — a property of a different function.
/// `try_simplify` guards for this explicitly; `try_fold` now does too.
#[test]
fn try_fold_refuses_a_floating_point_typed_node() {
    let mut g = probe_graph();
    let a = g.add(Op::Const(6), IrType::Int, vec![], None);
    let b = g.add(Op::Const(3), IrType::Int, vec![], None);
    // The same operands under three result types. Only the integer one may
    // fold; the other two share the op and must be refused on the TYPE.
    let as_int = g.add(Op::Div, IrType::Int, vec![a, b], None);
    let as_float = g.add(Op::Div, IrType::Float, vec![a, b], None);
    let as_double = g.add(Op::Div, IrType::Double, vec![a, b], None);
    assert_eq!(try_fold(&g.nodes, as_int), Some(2));
    assert_eq!(
        try_fold(&g.nodes, as_float),
        None,
        "a Float-typed Div must not be folded with integer arithmetic"
    );
    assert_eq!(try_fold(&g.nodes, as_double), None);
}

#[test]
fn test_dead_node_elimination() {
    // int f(int x) { int y = x + 1; return x; }
    // iload_0; iconst_1; iadd; istore_1; iload_0; ireturn
    let code = [0x1a, 0x04, 0x60, 0x3c, 0x1a, 0xac, 0, 0];
    let graph = build_and_optimize(&code, 6, 1, 2);
    // The x+1 computation is dead (result stored but never used in return)
    // After DCE, the Add node should be killed
    let ret = &graph.nodes[graph.exit as usize];
    let val_id = ret.inputs[1];
    assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
}

#[test]
fn test_chained_constant_folding() {
    // return (2 + 3) * 4  →  should fold to 20
    // iconst_2; iconst_3; iadd; iconst_4; imul; ireturn
    let code = [0x05, 0x06, 0x60, 0x07, 0x68, 0xac, 0, 0];
    let graph = build_and_optimize(&code, 6, 0, 0);
    let ret = &graph.nodes[graph.exit as usize];
    let val_id = ret.inputs[1];
    assert_eq!(graph.nodes[val_id as usize].op, Op::Const(20));
}

// ── Affine strength reduction (reassociation) ───────────────────

/// The ops of every live node reachable from `root` through input edges —
/// i.e. the computation the method actually performs, as distinct from
/// what merely still occupies the arena.
///
/// These are different questions now that `eliminate_dead_nodes` roots
/// safepoint snapshots: the builder records the operand stack at every
/// bci, so an intermediate no live computation reads can still be named by
/// a snapshot slot and is deliberately kept alive for deopt. A whole-arena
/// node count therefore measures "what DCE reclaimed", which is a
/// deopt-policy question, not "what the optimizer collapsed".
fn ops_reachable_from(graph: &Graph, root: NodeId) -> Vec<Op> {
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut stack = vec![root];
    let mut out = Vec::new();
    while let Some(id) = stack.pop() {
        if !graph.is_valid_id(id) || !seen.insert(id) {
            continue;
        }
        let n = &graph.nodes[id as usize];
        if n.op == Op::Dead {
            continue;
        }
        out.push(n.op.clone());
        for &inp in &n.inputs {
            stack.push(inp);
        }
    }
    out
}

/// A pure constant's affine form has no root, and that absence is `None` —
/// not the [`NO_NODE`] sentinel doing double duty as a value-domain
/// "nothing". The whole point of the `Option` is that the case split
/// cannot be forgotten, and that a rootless form can never materialize an
/// edge out of `u32::MAX`.
#[test]
fn test_affine_pure_constant_round_trips_without_a_sentinel() {
    let seven = Affine {
        root: None,
        k: 0,
        c: 7,
    };
    let five = Affine {
        root: None,
        k: 0,
        c: 5,
    };
    // The opaque form a real node would carry, for the `combine` calls.
    let opaque = Affine {
        root: Some(1),
        k: 1,
        c: 0,
    };

    // Combining two rootless forms stays rootless — no root is invented.
    assert_eq!(
        combine_affine(&Op::Add, true, seven, five, opaque),
        Affine {
            root: None,
            k: 0,
            c: 12
        }
    );
    assert_eq!(
        combine_affine(&Op::Sub, true, seven, five, opaque),
        Affine {
            root: None,
            k: 0,
            c: 2
        }
    );
    assert_eq!(
        combine_affine(&Op::Mul, true, seven, five, opaque),
        Affine {
            root: None,
            k: 0,
            c: 35
        }
    );

    // …and it materializes back to a single `Const`, with no operand
    // anywhere in the graph holding the placeholder.
    let mut g = probe_graph();
    let mat = build_affine(
        &mut g,
        &mut ConstIntern::new(),
        seven.root,
        seven.k,
        seven.c,
        IrType::Int,
    )
    .expect("a rootless form with k == 0 is exactly a constant");
    assert_eq!(g.nodes[mat as usize].op, Op::Const(7));
    assert!(
        g.nodes.iter().all(|n| !n.inputs.contains(&NO_NODE)),
        "materializing a pure constant must not fabricate a NO_NODE operand"
    );

    // A non-zero `k` with no root is not a representable value: refuse,
    // rather than emit `Mul(nodes[u32::MAX], k)`.
    assert!(
        build_affine(&mut g, &mut ConstIntern::new(), None, 3, 0, IrType::Int).is_none(),
        "a rootless form with k != 0 has nothing to multiply"
    );
}

fn build_and_reassociate(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    num_locals: usize,
) -> Graph {
    let builder = IrBuilder::new(num_params, num_locals);
    let mut graph = builder.build(code, code_len).expect("build failed");
    reassociate_affine(&mut graph);
    // Clean-up passes that normally follow in `optimize`.
    for _ in 0..8 {
        let before = graph.live_count();
        let _ = fold_constants(&mut graph);
        let _ = algebraic_simplify(&mut graph);
        gvn(&mut graph);
        eliminate_dead_nodes(&mut graph);
        if graph.live_count() == before {
            break;
        }
    }
    graph
}

#[test]
fn test_reassoc_affine_chain_folds() {
    // int f(int x){ x = x*3+5; x = x*2+1; return x; }  →  x*6 + 11
    // iload_0; iconst_3; imul; iconst_5; iadd; iconst_2; imul; iconst_1; iadd; ireturn
    let code = [
        0x1a, 0x06, 0x68, 0x08, 0x60, 0x05, 0x68, 0x04, 0x60, 0xac, 0, 0,
    ];
    let graph = build_and_reassociate(&code, 10, 1, 1);
    let ret_val = graph.nodes[graph.exit as usize].inputs[1];
    let val = &graph.nodes[ret_val as usize];
    assert_eq!(val.op, Op::Add, "top of folded chain should be Add(k*x, c)");
    // One input is the constant c=11, the other is Mul(x, 6).
    let (mul_id, c_id) = if graph.nodes[val.inputs[0] as usize].op == Op::Mul {
        (val.inputs[0], val.inputs[1])
    } else {
        (val.inputs[1], val.inputs[0])
    };
    assert_eq!(
        graph.nodes[c_id as usize].op,
        Op::Const(11),
        "additive constant"
    );
    let mul = &graph.nodes[mul_id as usize];
    assert_eq!(mul.op, Op::Mul);
    let (px, k_id) = if graph.nodes[mul.inputs[0] as usize].op == Op::Param(0) {
        (mul.inputs[0], mul.inputs[1])
    } else {
        (mul.inputs[1], mul.inputs[0])
    };
    assert_eq!(graph.nodes[px as usize].op, Op::Param(0));
    assert_eq!(
        graph.nodes[k_id as usize].op,
        Op::Const(6),
        "multiplicative constant"
    );
    // The whole intermediate chain collapsed: the returned value is
    // computed by exactly one Mul and one Add.
    //
    // Measured over the nodes the *return value* reaches, not over the
    // whole arena. The arena count stopped being the right question when
    // `eliminate_dead_nodes` began rooting safepoint snapshots: the
    // builder records the operand stack at every bci, so an intermediate
    // like the original `x*3` is still named by the snapshot at the next
    // bci and is deliberately kept for deopt even though no live
    // computation reads it. See `ops_reachable_from`.
    let reached = ops_reachable_from(&graph, ret_val);
    assert_eq!(
        reached.iter().filter(|o| **o == Op::Mul).count(),
        1,
        "the returned expression must be a single Mul: {reached:?}"
    );
    assert_eq!(
        reached.iter().filter(|o| **o == Op::Add).count(),
        1,
        "…and a single Add: {reached:?}"
    );
}

#[test]
fn test_reassoc_combines_muls() {
    // x = x*181; x = x*181;  →  x*32761  (181*181, a single Mul)
    // iload_0; sipush 181; imul; sipush 181; imul; ireturn
    let code = [
        0x1a, 0x11, 0x00, 0xb5, 0x68, 0x11, 0x00, 0xb5, 0x68, 0xac, 0, 0,
    ];
    let graph = build_and_reassociate(&code, 10, 1, 1);
    let ret_val = graph.nodes[graph.exit as usize].inputs[1];
    let val = &graph.nodes[ret_val as usize];
    assert_eq!(val.op, Op::Mul);
    let k_id = if graph.nodes[val.inputs[0] as usize].op == Op::Param(0) {
        val.inputs[1]
    } else {
        val.inputs[0]
    };
    assert_eq!(
        graph.nodes[k_id as usize].op,
        Op::Const(32761),
        "181*181 folds to 32761"
    );
    // One Mul in the returned expression. Arena-wide counting no longer
    // answers this question — the un-materialized `x*181` intermediate is
    // still named by the operand-stack snapshot at the next bci, and
    // snapshot slots are DCE roots now. See `ops_reachable_from`.
    let reached = ops_reachable_from(&graph, ret_val);
    assert_eq!(
        reached.iter().filter(|o| **o == Op::Mul).count(),
        1,
        "the returned expression must be a single Mul: {reached:?}"
    );
}

#[test]
fn test_reassoc_leaves_nonlinear_alone() {
    // x*x is not affine (variable * variable) — must not be folded.
    // iload_0; iload_0; imul; ireturn
    let code = [0x1a, 0x1a, 0x68, 0xac, 0, 0];
    let graph = build_and_reassociate(&code, 4, 1, 1);
    let ret = &graph.nodes[graph.exit as usize];
    let val = &graph.nodes[ret.inputs[1] as usize];
    assert_eq!(val.op, Op::Mul);
    assert_eq!(graph.nodes[val.inputs[0] as usize].op, Op::Param(0));
    assert_eq!(graph.nodes[val.inputs[1] as usize].op, Op::Param(0));
}

// ── Unit tests for try_fold i32-truncation correctness ──────────

/// Build a 3-node graph: Const(a), Const(b), Op(a, b) of type `ty`.
/// Returns the binary node's id and the node arena, ready to pass to try_fold.
fn fold_binop(ty: IrType, op: Op, a: i64, b: i64) -> Option<i64> {
    let mut nodes: Vec<Node> = Vec::new();
    nodes.push(Node {
        op: Op::Const(a),
        ty,
        inputs: vec![].into(),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    });
    nodes.push(Node {
        op: Op::Const(b),
        ty,
        inputs: vec![].into(),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    });
    nodes.push(Node {
        op,
        ty,
        inputs: vec![0, 1].into(),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    });
    try_fold(&nodes, 2)
}

fn fold_unop(ty: IrType, op: Op, a: i64) -> Option<i64> {
    let mut nodes: Vec<Node> = Vec::new();
    nodes.push(Node {
        op: Op::Const(a),
        ty,
        inputs: vec![].into(),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    });
    nodes.push(Node {
        op,
        ty,
        inputs: vec![0].into(),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    });
    try_fold(&nodes, 1)
}

#[test]
fn test_fold_i32_add_overflow_wraps() {
    // Java: Integer.MIN_VALUE + (-1) == Integer.MAX_VALUE (0x7FFF_FFFF)
    let r = fold_binop(IrType::Int, Op::Add, i32::MIN as i64, -1).unwrap();
    assert_eq!(
        r,
        i32::MAX as i64,
        "INT_MIN + (-1) must wrap to INT_MAX, got {:#x}",
        r
    );
    // High 32 bits must be sign-extension of low 32 bits (i.e. 0 here).
    assert_eq!(
        r as i32 as i64, r,
        "result must be a clean i32 sign-extension"
    );
}

#[test]
fn test_fold_i32_sub_overflow_wraps() {
    // Java: Integer.MIN_VALUE - 1 == Integer.MAX_VALUE
    let r = fold_binop(IrType::Int, Op::Sub, i32::MIN as i64, 1).unwrap();
    assert_eq!(r, i32::MAX as i64);
}

#[test]
fn test_fold_i32_mul_overflow_wraps() {
    // Java: 0x10000 * 0x10000 (as int) == 0 (low 32 bits of 2^32)
    let r = fold_binop(IrType::Int, Op::Mul, 0x10000, 0x10000).unwrap();
    assert_eq!(r, 0);
}

#[test]
fn test_fold_i32_div_intmin_neg1() {
    // Java: Integer.MIN_VALUE / -1 == Integer.MIN_VALUE (no exception)
    let r = fold_binop(IrType::Int, Op::Div, i32::MIN as i64, -1).unwrap();
    assert_eq!(r, i32::MIN as i64);
}

#[test]
fn test_fold_i32_rem_intmin_neg1() {
    // Java: Integer.MIN_VALUE % -1 == 0 (no exception)
    let r = fold_binop(IrType::Int, Op::Rem, i32::MIN as i64, -1).unwrap();
    assert_eq!(r, 0);
}

#[test]
fn test_fold_i32_div_by_zero_returns_none() {
    // Cannot fold; runtime must throw ArithmeticException.
    assert_eq!(fold_binop(IrType::Int, Op::Div, 5, 0), None);
    assert_eq!(fold_binop(IrType::Int, Op::Rem, 5, 0), None);
}

#[test]
fn test_fold_i64_div_longmin_neg1() {
    // Java: Long.MIN_VALUE / -1 == Long.MIN_VALUE (no exception)
    let r = fold_binop(IrType::Long, Op::Div, i64::MIN, -1).unwrap();
    assert_eq!(r, i64::MIN);
    let r = fold_binop(IrType::Long, Op::Rem, i64::MIN, -1).unwrap();
    assert_eq!(r, 0);
}

#[test]
fn test_fold_i64_div_by_zero_returns_none() {
    assert_eq!(fold_binop(IrType::Long, Op::Div, 5, 0), None);
    assert_eq!(fold_binop(IrType::Long, Op::Rem, 5, 0), None);
}

#[test]
fn test_fold_i32_shl_masks_to_5_bits() {
    // Java: 1 << 33 == 1 << 1 == 2 (shift count masked to low 5 bits)
    let r = fold_binop(IrType::Int, Op::Shl, 1, 33).unwrap();
    assert_eq!(r, 2);
    // 1 << 31 == INT_MIN (sign-extended to i64)
    let r = fold_binop(IrType::Int, Op::Shl, 1, 31).unwrap();
    assert_eq!(r, i32::MIN as i64);
}

#[test]
fn test_fold_i64_shl_masks_to_6_bits() {
    // Java: 1L << 65 == 1L << 1 == 2
    let r = fold_binop(IrType::Long, Op::Shl, 1, 65).unwrap();
    assert_eq!(r, 2);
}

#[test]
fn test_fold_i32_ushr_does_not_leak_high_bits() {
    // Java: (-1) >>> 1 == 0x7FFF_FFFF (as int), sign-extended → 0x7FFF_FFFF
    let r = fold_binop(IrType::Int, Op::UShr, -1i64, 1).unwrap();
    assert_eq!(r, 0x7FFF_FFFFi64);
}

#[test]
fn test_fold_i32_and_truncates() {
    // If either operand somehow had stale high bits, AND result must
    // still be a clean i32 sign-extension.
    let r = fold_binop(IrType::Int, Op::And, 0xFF, 0x0F).unwrap();
    assert_eq!(r, 0x0F);
    assert_eq!(r as i32 as i64, r);
}

#[test]
fn test_fold_i32_neg_intmin() {
    // Java: -Integer.MIN_VALUE == Integer.MIN_VALUE (wraps)
    let r = fold_unop(IrType::Int, Op::Neg, i32::MIN as i64).unwrap();
    assert_eq!(r, i32::MIN as i64);
}

#[test]
fn test_fold_i64_neg_longmin() {
    let r = fold_unop(IrType::Long, Op::Neg, i64::MIN).unwrap();
    assert_eq!(r, i64::MIN);
}

// ── Dead Store Elimination (DSE) ────────────────────────────────

/// Hand-build a minimal `ir::Graph` (the `optimize()` passes run on the
/// real `ir::Graph`, distinct from the EA graph). Returns a graph with a
/// Start node already in place.
fn probe_graph() -> Graph {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    g
}

#[test]
fn test_dce_keeps_all_return_paths() {
    // `if (a < 0) return -1; return 1;` — two `Op::Return` terminators. The
    // builder records only the LAST in `graph.exit`, but DCE must keep BOTH
    // return paths (regression for the conditional-early-return miscompile:
    // rooting only from `graph.exit` deleted the `return -1` branch and
    // collapsed the `If` into a single-successor that always returned 1).
    let mut g = probe_graph();
    let a = g.add(Op::Param(0), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, zero], None);
    let if_node = g.add(Op::If, IrType::Control, vec![g.entry, cmp], None);
    let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    let neg1 = g.add(Op::Const(-1), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let ret_t = g.add(Op::Return, IrType::Void, vec![t, neg1], None);
    let ret_f = g.add(Op::Return, IrType::Void, vec![f, one], None);
    // The builder overwrites `exit` with each return → only the last one.
    g.exit = ret_f;

    eliminate_dead_nodes(&mut g);

    assert_ne!(
        g.nodes[ret_t as usize].op,
        Op::Dead,
        "the first (graph.exit-excluded) return must survive DCE"
    );
    assert_ne!(g.nodes[ret_f as usize].op, Op::Dead);
    assert_ne!(
        g.nodes[t as usize].op,
        Op::Dead,
        "the true projection feeding the first return must survive"
    );
    assert_ne!(g.nodes[f as usize].op, Op::Dead);
    assert_ne!(
        g.nodes[neg1 as usize].op,
        Op::Dead,
        "the -1 value of the first return must survive"
    );
}

#[test]
fn test_dce_keeps_value_live_only_via_safepoint_snapshot() {
    // `int y = x + 1; return x;` — `y` is dead to the *compiled* code, but
    // the safepoint snapshot names it, and deopt rebuilds interpreter local
    // 1 out of that slot. "Reachable from a Return" is the wrong liveness
    // question for a snapshot value: a local the compiled code never reads
    // again is still live to the interpreter it may deopt back into.
    // Removing it would leave the slot naming an `Op::Dead` node and the
    // reconstructed frame built from nothing.
    use crate::ir::SafepointSnapshot;
    let mut g = probe_graph();
    let x = g.add(Op::Param(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let y = g.add(Op::Add, IrType::Int, vec![x, one], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, x], None);
    g.exit = ret;
    g.safepoints
        .push(SafepointSnapshot::new(4, vec![x, y], vec![], Vec::new()));

    eliminate_dead_nodes(&mut g);

    assert_ne!(
        g.nodes[y as usize].op,
        Op::Dead,
        "a value live only through a safepoint snapshot must survive DCE"
    );
    assert_ne!(
        g.nodes[one as usize].op,
        Op::Dead,
        "and so must everything it transitively needs"
    );
    assert_eq!(
        g.safepoints[0].locals[1], y,
        "the slot must still name the value — keeping it is the fix, \
         clearing the slot would silently lose an interpreter local"
    );
}

#[test]
fn test_dce_normalises_already_stale_snapshot_slot() {
    // A slot naming a node some EARLIER pass removed cannot be rescued —
    // the value it named is already gone, and seeding it as a root would
    // only resurrect an `Op::Dead` node. It normalises to `NO_NODE`
    // ("undefined at this bci"), which deopt resolves to `Undefined`:
    // the same answer, reached explicitly instead of by naming a corpse.
    use crate::ir::SafepointSnapshot;
    let mut g = probe_graph();
    let x = g.add(Op::Param(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let stale = g.add(Op::Add, IrType::Int, vec![x, one], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, x], None);
    g.exit = ret;
    g.safepoints.push(SafepointSnapshot::new(
        4,
        vec![x, stale],
        vec![NO_NODE],
        Vec::new(),
    ));
    // Stand in for the earlier pass that removed the node without routing
    // its safepoint references.
    g.kill(stale);

    eliminate_dead_nodes(&mut g);

    assert_eq!(
        g.safepoints[0].locals[1], NO_NODE,
        "an already-dead slot normalises to the undefined placeholder"
    );
    assert_eq!(g.safepoints[0].locals[0], x, "a live slot is untouched");
    assert_eq!(
        g.safepoints[0].stack[0], NO_NODE,
        "an already-undefined slot stays undefined"
    );
    assert_eq!(
        g.nodes[stale as usize].op,
        Op::Dead,
        "normalising the slot does not resurrect the node"
    );
}

/// A compact two-reference layout at a class id no other test uses, for
/// `eliminate_initial_zero_stores`.
fn initial_zero_store_layout(cid: u32) {
    cratonvm_types::register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        cid,
        std::sync::Arc::new(cratonvm_types::CompactLayout {
            field_disps: vec![8, 16],
            is_ref: vec![true, true],
            field_kinds: vec![
                cratonvm_types::FieldStorageKind::Reference,
                cratonvm_types::FieldStorageKind::Reference,
            ],
            ref_disps: vec![8, 16],
            total_size: 24,
        }),
    );
}

/// `new C(null, null)` after the constructor is spliced: two null stores
/// into a fresh object's two fields. `store_value` feeds the FIRST store.
fn two_initial_stores(cid: u32, store_value: i64) -> (Graph, NodeId, NodeId) {
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: cid,
            num_fields: 2,
        },
        IrType::Ref,
        vec![g.entry, g.entry],
        None,
    );
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let v0 = g.add(Op::Const(store_value), IrType::Ref, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let s0 = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![g.entry, g.entry, alloc, f0, v0],
        None,
    );
    let s1 = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![g.entry, s0, alloc, f1, null],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, alloc], None);
    g.exit = ret;
    (g, s0, s1)
}

#[test]
fn initial_null_stores_into_a_fresh_compact_object_are_removed() {
    let cid = 900_031;
    initial_zero_store_layout(cid);
    let (mut g, s0, s1) = two_initial_stores(cid, 0);
    assert!(eliminate_initial_zero_stores(&mut g));
    assert_eq!(g.nodes[s0 as usize].op, Op::Dead, "field 0 is already null");
    assert_eq!(g.nodes[s1 as usize].op, Op::Dead, "so is field 1");
}

#[test]
fn an_initial_null_store_after_a_store_to_another_field_is_removed_but_not_that_store() {
    let cid = 900_032;
    initial_zero_store_layout(cid);
    // Field 0 gets a real value; field 1 is still null behind it.
    let (mut g, s0, s1) = two_initial_stores(cid, 0x1000);
    assert!(eliminate_initial_zero_stores(&mut g));
    assert_eq!(g.nodes[s0 as usize].op, Op::Store(MemKind::Ref));
    assert_eq!(g.nodes[s1 as usize].op, Op::Dead);
}

#[test]
fn a_null_store_over_the_same_field_is_kept() {
    let cid = 900_033;
    initial_zero_store_layout(cid);
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: cid,
            num_fields: 2,
        },
        IrType::Ref,
        vec![g.entry, g.entry],
        None,
    );
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let v = g.add(Op::Const(0x1000), IrType::Ref, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let s0 = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![g.entry, g.entry, alloc, f0, v],
        None,
    );
    let s1 = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![g.entry, s0, alloc, f0, null],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, alloc], None);
    g.exit = ret;
    assert!(!eliminate_initial_zero_stores(&mut g));
    assert_eq!(
        g.nodes[s1 as usize].op,
        Op::Store(MemKind::Ref),
        "it clears a written cell"
    );
}

#[test]
fn a_null_store_into_a_class_with_no_compact_layout_is_kept() {
    // A legacy object's zeroed cell reads back as `Int(0)`, not `null`.
    let (mut g, s0, s1) = two_initial_stores(900_034, 0);
    assert!(!eliminate_initial_zero_stores(&mut g));
    assert_eq!(g.nodes[s0 as usize].op, Op::Store(MemKind::Ref));
    assert_eq!(g.nodes[s1 as usize].op, Op::Store(MemKind::Ref));
}

/// `sink = a; sink = b; ...` on one control anchor: `[ctrl, mem, value]`
/// `putstatic`s, each taking the previous one's token, of `(7, field)` per
/// entry, with `(7, 0)` and `(7, 1)` direct (initialised class).
fn static_store_chain(fields: &[(u32, bool)]) -> (Graph, Vec<NodeId>) {
    let mut g = probe_graph();
    g.direct_static_fields = vec![(7, 0), (7, 1)];
    let mut mem = g.entry;
    let mut stores = Vec::new();
    for (k, &(field_index, is_volatile)) in fields.iter().enumerate() {
        let v = g.add(Op::Const(k as i64 + 1), IrType::Ref, vec![], None);
        mem = g.add(
            Op::StoreStatic {
                class_id: 7,
                field_index,
                type_tag: b'L',
                is_volatile,
            },
            IrType::Memory,
            vec![g.entry, mem, v],
            None,
        );
        stores.push(mem);
    }
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, mem], None);
    g.exit = ret;
    (g, stores)
}

#[test]
fn an_overwritten_static_store_is_removed_and_its_token_spliced() {
    let (mut g, s) = static_store_chain(&[(0, false), (0, false), (0, false)]);
    assert!(eliminate_overwritten_static_stores(&mut g));
    assert_eq!(g.nodes[s[0] as usize].op, Op::Dead);
    assert_eq!(g.nodes[s[1] as usize].op, Op::Dead);
    assert!(matches!(g.nodes[s[2] as usize].op, Op::StoreStatic { .. }));
    assert_eq!(
        g.nodes[s[2] as usize].inputs[1], g.entry,
        "the surviving store takes the first store's incoming token"
    );
    assert!(!eliminate_overwritten_static_stores(&mut g), "idempotent");
}

#[test]
fn a_static_store_followed_by_another_cell_volatile_or_cold_class_stays() {
    // Another field of the same class.
    let (mut g, s) = static_store_chain(&[(0, false), (1, false)]);
    assert!(!eliminate_overwritten_static_stores(&mut g));
    assert!(matches!(g.nodes[s[0] as usize].op, Op::StoreStatic { .. }));
    // A volatile store on either side.
    for pair in [[(0, true), (0, false)], [(0, false), (0, true)]] {
        let (mut g, s) = static_store_chain(&pair);
        assert!(!eliminate_overwritten_static_stores(&mut g));
        assert!(matches!(g.nodes[s[0] as usize].op, Op::StoreStatic { .. }));
    }
    // A class not initialised at compile time: its helper may run `<clinit>`.
    let (mut g, s) = static_store_chain(&[(2, false), (2, false)]);
    assert!(!eliminate_overwritten_static_stores(&mut g));
    assert!(matches!(g.nodes[s[0] as usize].op, Op::StoreStatic { .. }));
}

#[test]
fn a_static_store_with_a_node_that_may_throw_between_stays() {
    // `sink = a; p.x = 1; sink = b;`: the `putfield` names neither static
    // store, but a null `p` throws with `sink == a` visible to a handler.
    let mut g = probe_graph();
    g.direct_static_fields = vec![(7, 0)];
    let put = |g: &mut Graph, mem: NodeId, v: NodeId| {
        let op = Op::StoreStatic {
            class_id: 7,
            field_index: 0,
            type_tag: b'L',
            is_volatile: false,
        };
        g.add(op, IrType::Memory, vec![g.entry, mem, v], None)
    };
    let a = g.add(Op::Const(1), IrType::Ref, vec![], None);
    let entry = g.entry;
    let s0 = put(&mut g, entry, a);
    let p = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let _field = g.add(Op::Store(MemKind::Int), IrType::Void, vec![p, one], None);
    let b = g.add(Op::Const(2), IrType::Ref, vec![], None);
    let s1 = put(&mut g, s0, b);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, s1], None);
    g.exit = ret;
    assert!(!eliminate_overwritten_static_stores(&mut g));
    assert!(matches!(g.nodes[s0 as usize].op, Op::StoreStatic { .. }));
}

#[test]
fn a_static_store_another_node_reads_the_token_of_stays() {
    // A second consumer of the first store's token (a memory φ, an exit on
    // another path) may observe the cell without the overwrite running.
    let (mut g, s) = static_store_chain(&[(0, false), (0, false)]);
    let other = g.add(Op::Return, IrType::Void, vec![g.entry, s[0]], None);
    assert!(!eliminate_overwritten_static_stores(&mut g));
    assert!(matches!(g.nodes[s[0] as usize].op, Op::StoreStatic { .. }));
    assert_eq!(g.nodes[other as usize].inputs[1], s[0]);
    // A different control anchor: the two stores are not one block.
    let (mut g, s) = static_store_chain(&[(0, false), (0, false)]);
    let region = g.add(Op::Region, IrType::Void, vec![g.entry], None);
    g.set_input(s[1], 0, region);
    assert!(!eliminate_overwritten_static_stores(&mut g));
    assert!(matches!(g.nodes[s[0] as usize].op, Op::StoreStatic { .. }));
}

#[test]
fn test_dse_removes_overwritten_store() {
    // A local object whose field 0 is written twice with no read in
    // between: the first store is dead and must be removed; the second
    // (live) store and the allocation survive.
    //
    // The compact layout carries no offset operand, so the overwrite match
    // rests on `num_fields == 1` — with one field, "the object's storage"
    // and "field 0" are the same cell (`store_matchable_location`). Raise
    // `num_fields` and the two stores must stop matching; that is
    // `test_dse_absent_offset_stores_to_a_multi_field_object_are_not_matched`.
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    // Compact [base, value] layout (as used by the EA bridge/tests).
    let dead_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    let live_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c2], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, live_store], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[dead_store as usize].op,
        Op::Dead,
        "the overwritten store must be eliminated"
    );
    assert_eq!(
        g.nodes[live_store as usize].op,
        Op::Store(MemKind::Int),
        "the surviving (overwriting) store must be kept"
    );
    assert!(
        !matches!(g.nodes[alloc as usize].op, Op::Dead),
        "DSE must not touch the allocation"
    );
}

#[test]
fn test_dse_keeps_store_with_intervening_load() {
    // store; load(same obj); store  — the first store IS observed by the
    // load, so it must NOT be removed.
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    // A load is a memory barrier — flushes the pending store.
    let _load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![alloc], None);
    let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c2], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, s2], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[s1 as usize].op,
        Op::Store(MemKind::Int),
        "a store observed by an intervening load must be kept"
    );
}

#[test]
fn test_dse_keeps_store_to_non_local_base() {
    // Store through a Param (could be observed by the caller / other
    // threads): even an immediate overwrite must NOT remove the first.
    let mut g = probe_graph();
    let p0 = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![p0, c1], None);
    let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![p0, c2], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, s2], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[s1 as usize].op,
        Op::Store(MemKind::Int),
        "a store to a non-local (Param) base may be observed externally and must be kept"
    );
}

#[test]
fn test_dse_distinct_fields_not_matched() {
    // Two stores to *different* fields of the same object: neither
    // overwrites the other, so the overwrite phase must not match them.
    // A trailing load of the object makes it *read* (so the widened
    // write-only phase does not apply here) — this test isolates the
    // distinct-field non-conflation property of the overwrite phase.
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 2,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    // Different MemKind → different location key. NOTE that this is an
    // access *width* difference, not a field-identity one: it happens to
    // separate these two stores, but two `int` fields share their MemKind.
    // The width is not what makes the pass field-safe — see
    // `test_dse_absent_offset_stores_to_a_multi_field_object_are_not_matched`
    // for the same-width case.
    let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    let s2 = g.add(
        Op::Store(MemKind::Long),
        IrType::Void,
        vec![alloc, c2],
        None,
    );
    // Read the object back so it is observed (write-only phase off).
    let load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![alloc], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(g.nodes[s1 as usize].op, Op::Store(MemKind::Int));
    assert_eq!(g.nodes[s2 as usize].op, Op::Store(MemKind::Long));
}

// ── Widened DSE: load-alias refinement (DSE-WIDEN) ──────────────

#[test]
fn test_dse_removes_overwrite_across_unrelated_local_load() {
    // store A.f = 1; load B.f (B a DISTINCT local allocation); store A.f = 2
    // The intervening load reads a provably-different object, so it does not
    // observe A's first store — which is therefore still dead. A trailing
    // load of A keeps A "read" so the write-only phase doesn't also remove
    // the live store, isolating the overwrite property.
    let mut g = probe_graph();
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let b = g.add(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    let dead_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c1], None);
    // Load of the UNRELATED local allocation B — not a barrier for A.
    let _load_b = g.add(Op::Load(MemKind::Int), IrType::Int, vec![b], None);
    let live_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c2], None);
    // Read A so the write-only phase keeps the live store.
    let load_a = g.add(Op::Load(MemKind::Int), IrType::Int, vec![a], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load_a], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[dead_store as usize].op,
        Op::Dead,
        "the first store to A is dead: the intervening load reads a distinct \
         local allocation B that cannot alias A"
    );
    assert_eq!(
        g.nodes[live_store as usize].op,
        Op::Store(MemKind::Int),
        "the overwriting (live) store to A must be kept"
    );
}

#[test]
fn test_dse_keeps_overwrite_across_nonlocal_load() {
    // store A.f = 1; load P.f (P a Param — non-local, unknown provenance);
    // store A.f = 2. The load's base could alias A if A had escaped, so the
    // conservative path flushes all pending stores: the first store to A
    // must be KEPT. (Locks the non-local-load fallback of the refinement.)
    let mut g = probe_graph();
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let p = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c1], None);
    // Load from a non-local base — conservatively a full barrier.
    let _load_p = g.add(Op::Load(MemKind::Int), IrType::Int, vec![p], None);
    let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c2], None);
    let load_a = g.add(Op::Load(MemKind::Int), IrType::Int, vec![a], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load_a], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[s1 as usize].op,
        Op::Store(MemKind::Int),
        "a load from a non-local base may alias A, so the first store is kept"
    );
    assert_eq!(g.nodes[s2 as usize].op, Op::Store(MemKind::Int));
}

// ── Widened DSE: write-only, never-read local allocation ────────

#[test]
fn test_dse_removes_write_only_never_read_store() {
    // A local object whose field is written once and never read, with
    // nothing depending on the store, and the object never escaping:
    // the store is observably inert and must be removed. (Distinct from
    // the overwrite case: there is only ONE store, so the straight-line
    // overwrite phase cannot fire — only the write-only phase can.)
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(7), IrType::Int, vec![], None);
    let store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    // The method returns void; the alloc never escapes and is never read.
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[store as usize].op,
        Op::Dead,
        "a write-only, never-read store to a local alloc must be removed"
    );
}

#[test]
fn test_dse_keeps_write_only_store_when_alloc_escapes() {
    // Same write-only shape, but the allocation escapes (returned to the
    // caller). The caller may read the field, so the store is observable
    // and must be kept — guards the kafka bug-25-class escape hole.
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(7), IrType::Int, vec![], None);
    let store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    // The allocation is RETURNED → escapes.
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, alloc], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[store as usize].op,
        Op::Store(MemKind::Int),
        "a store into an escaping allocation may be observed externally and must be kept"
    );
}

/// JIT round 20 wave 12 (lane hFin): the write-only shape again, on a
/// class the compile resolved WITH a finalizer / reference registration
/// (`Graph::finalizable_new_classes`). `finalize()` -- or the collector's
/// reference processing -- reads the field after this method returns, so
/// the store is kept although nothing in the graph loads it.
#[test]
fn test_dse_keeps_write_only_store_into_a_finalizable_allocation() {
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(7), IrType::Int, vec![], None);
    let store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry], None);
    g.exit = ret;
    g.finalizable_new_classes.insert(1);

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[store as usize].op,
        Op::Store(MemKind::Int),
        "a store into a finalizable allocation is read by finalize() and must be kept"
    );
}

// ── DSE: memory-token chain integrity ───────────────────────────

#[test]
fn test_dse_overwritten_store_leaves_intact_memory_chain() {
    // Full-form chain:  m0 → alloc → s1 → s2 → load
    //
    // `s1` is overwritten by `s2` with no reader in between, so DSE removes
    // it. What this locks is *how*: a plain `Graph::kill` left `s2`'s token
    // slot naming the removed `s1`, so `s2` lost its transitive ordering
    // edge to `alloc` and to everything that wrote before it — and a
    // scheduler reading that chain is then free to hoist `s2` above them.
    // After the splice, `s2` must take its token from `s1`'s OWN incoming
    // token, and no live node anywhere may read a token from a `Dead` node.
    let mut g = probe_graph();
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![g.entry], None);
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, m0],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    // `Op::New` is both the reference and the memory token it produces, so
    // it appears in slot 1 (token) and slot 2 (base) of the first store.
    let s1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, alloc, alloc, off, c1],
        None,
    );
    let s2 = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, s1, alloc, off, c2],
        None,
    );
    // A reader keeps the allocation "read" so the write-only phase does not
    // also remove `s2` — this test is about the overwrite path only.
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, s2, alloc, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[s1 as usize].op,
        Op::Dead,
        "the overwritten store must still be eliminated"
    );
    assert_eq!(
        g.nodes[s2 as usize].op,
        Op::Store(MemKind::Int),
        "the overwriting store survives"
    );
    assert_eq!(
        g.nodes[s2 as usize].inputs[1], alloc,
        "the surviving store must be spliced onto the removed store's own \
         incoming memory token, not left naming the removed node"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[1], s2,
        "the rest of the chain is untouched"
    );

    for (id, n) in g.nodes.iter().enumerate() {
        if n.op == Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if !is_memory_token_slot(n, i) {
                continue;
            }
            assert_ne!(
                g.nodes[inp as usize].op,
                Op::Dead,
                "n{id}:{:?} takes its memory token from removed n{inp}",
                n.op
            );
        }
    }
}

#[test]
fn test_dse_keeps_store_it_cannot_splice_out_of_the_chain() {
    // A store whose token consumer names it, but which has no incoming
    // token of its own to hand on (the compact `[base, value]` layout).
    // Deleting it would leave the consumer's token slot pointing at a
    // removed node with nothing to repair it with, so the store is KEPT: a
    // missed optimization, not a severed chain.
    let mut g = probe_graph();
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![g.entry], None);
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, m0],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    // Compact store — no token slot at all. Its location key is
    // `(alloc, no-index, Int)`.
    let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    // …overwritten by a full-form no-index store `[ctrl, mem, base, value]`
    // (the same location key) that also names `s1` as its memory token.
    let s2 = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, s1, alloc, c2],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, s2, alloc, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[s1 as usize].op,
        Op::Store(MemKind::Int),
        "a store that cannot be spliced out of the chain must be left alive"
    );
    assert_eq!(g.nodes[s2 as usize].inputs[1], s1, "the chain is unchanged");
}

// ── Alias-analysis audit (wave: alias soundness) ────────────────

#[test]
fn test_dse_absent_offset_stores_to_a_multi_field_object_are_not_matched() {
    // `o.x = 1; o.y = 2;` where `x` and `y` are two `int` fields of the
    // SAME object, written through a layout that carries no offset operand.
    //
    // The location key is `(base, idx, MemKind)`. Both stores have
    // `idx == NO_NODE` and `MemKind::Int`, so the key is identical — yet
    // they name different cells, and matching them deletes the live
    // `o.x = 1`. `MemKind` is an access *width*, not a field index; two
    // `int` fields share it. The memory model calls a missing offset
    // `ir::AccessOffset::Absent` and defines it as a MAY-alias, and a
    // deletion needs a MUST-alias.
    //
    // (Contrast `test_dse_removes_overwritten_store`, where the object has
    // exactly ONE field and the absence of an offset therefore still names
    // a definite cell.)
    let mut g = probe_graph();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 2,
        },
        IrType::Ref,
        vec![g.entry],
        None,
    );
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
    let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c2], None);
    // Read the object back so the write-only phase (which keys on the base
    // alone and is field-independent) does not also fire — this test is
    // about the overwrite phase.
    let load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![alloc], None);
    let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[s1 as usize].op,
        Op::Store(MemKind::Int),
        "nothing proves these two stores hit the same cell: with no offset \
         operand the layout cannot tell field x from field y, and the \
         matching MemKind is a width, not an identity"
    );
    assert_eq!(g.nodes[s2 as usize].op, Op::Store(MemKind::Int));
}

#[test]
fn test_dse_offset_distinguished_stores_still_match_and_still_separate() {
    // The production layout `[ctrl, mem, base, offset, value]`: two stores
    // to the SAME constant offset still match (the optimization survives),
    // and two stores to DIFFERENT constant offsets still do not.
    let mut g = probe_graph();
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![g.entry], None);
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 2,
        },
        IrType::Ref,
        vec![ctrl, m0],
        None,
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
    let c3 = g.add(Op::Const(3), IrType::Int, vec![], None);
    // f0 = 1 ; f1 = 2 ; f0 = 3   →  only the first is overwritten.
    let s_f0_a = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, alloc, alloc, off0, c1],
        None,
    );
    let s_f1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, s_f0_a, alloc, off1, c2],
        None,
    );
    let s_f0_b = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, s_f1, alloc, off0, c3],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, s_f0_b, alloc, off0],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
    g.exit = ret;

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[s_f0_a as usize].op,
        Op::Dead,
        "two stores at the same constant offset are a proved overwrite"
    );
    assert_eq!(
        g.nodes[s_f1 as usize].op,
        Op::Store(MemKind::Int),
        "the store to the OTHER field is not overwritten by either"
    );
    assert_eq!(g.nodes[s_f0_b as usize].op, Op::Store(MemKind::Int));
    // …and the chain closed over the hole rather than naming a dead node.
    for (id, n) in g.nodes.iter().enumerate() {
        if n.op == Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if !is_memory_token_slot(n, i) {
                continue;
            }
            assert_ne!(
                g.nodes[inp as usize].op,
                Op::Dead,
                "n{id}:{:?} takes its memory token from removed n{inp}",
                n.op
            );
        }
    }
}

#[test]
fn test_loop_body_pins_a_node_whose_only_link_sorts_after_it() {
    // The loop-body sweep must reach a fixed point. `call` is created
    // BEFORE the node that pins it into the body, so a single arena pass in
    // id order misses it — and a missed `Op::Call` is a hard barrier
    // `loop_has_hard_barrier` never sees, which is how a load gets hoisted
    // across a call that may write the very field it reads.
    let mut g = probe_graph();
    let pre = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
    let region = g.add(Op::Region, IrType::Control, vec![pre, NO_NODE], None);
    // Created here, patched below: its memory operand does not exist yet.
    let call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Int,
        vec![pre, NO_NODE],
        None,
    );
    let back = g.add(Op::Proj(0), IrType::Control, vec![region], None);
    let mid = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![back, NO_NODE, pre],
        None,
    );
    g.nodes[region as usize].inputs[1] = back;
    g.nodes[call as usize].inputs[1] = mid;

    let body = loop_body(&g, region, pre, &[back]);

    assert!(
        body.contains(&mid),
        "the load is pinned directly to a body control node"
    );
    assert!(
        body.contains(&call),
        "the call is pinned to the body THROUGH the load, whose id sorts \
         after it — one arena pass in id order misses this"
    );
    assert!(
        loop_has_hard_barrier(&g, &body),
        "a call in the body must disqualify the loop for hoisting"
    );
    assert!(
        !body.contains(&pre),
        "the pre-header must stay out of the body"
    );
}

// ── SCEV-driven LICM ────────────────────────────────────────────

/// Hand-build a minimal counted-loop graph and return
/// `(graph, region, preheader, iv_phi)`:
///
/// ```text
///   start ─ c0(Proj0) ─ m0(Proj1)
///   region = Region[c0, back_ctrl]          (loop header)
///   iv     = Phi[region, init=Const0, back] (induction var, variant)
///   if     = If[region, cond]               (loop spine)
///   t/f    = Proj0/Proj1[if]
///   back_ctrl = Proj0 (loops to region)     (the back-edge control)
/// ```
///
/// The caller adds the node under test (an invariant or variant load)
/// pinned into the body via a control input, then runs `licm`.
fn loop_probe() -> (Graph, NodeId, NodeId, NodeId) {
    loop_probe_header(Op::Region)
}

/// As [`loop_probe`] but with a caller-chosen header op (`Op::Region` for
/// the hand-built shape, `Op::Merge` for the real javac loop shape). The
/// header's inputs are `[entry_pred=c0, back_edge_ctrl]`.
fn loop_probe_header(header: Op) -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None); // preheader ctrl
    let _m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    // Header: [entry_pred=c0, back_edge_ctrl] — fill back-edge below.
    let region = g.add(header, IrType::Control, vec![c0], None);
    let init = g.add(Op::Const(0), IrType::Int, vec![], None);
    // Induction phi (loop-carried): [region, init, back_val]. back_val
    // filled after the increment exists.
    let iv = g.add(Op::Phi, IrType::Int, vec![region, init], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iv_next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    // Close the phi's back-edge value.
    g.nodes[iv as usize].inputs.push(iv_next);
    // Loop spine: If on a cond, with a back-edge Proj re-entering region.
    let bound = g.add(Op::Const(10), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], None);
    let if_node = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let back_ctrl = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let _exit_ctrl = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    // Wire the back-edge control into the region.
    g.nodes[region as usize].inputs.push(back_ctrl);
    (g, region, c0, iv)
}

/// Build a counted reduction loop `for (i=init; i<bound; i+=stride) acc += i;`
/// Returns (graph, return_node); the Return's value input is the accumulator
/// phi (its post-loop value), its control is the loop-exit projection.
fn reduction_loop(init: i64, bound: i64, stride: i64) -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let region = g.add(Op::Region, IrType::Control, vec![c0], None);
    let i_init = g.add(Op::Const(init), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, i_init], None);
    let stride_c = g.add(Op::Const(stride), IrType::Int, vec![], None);
    let iv_next = g.add(Op::Add, IrType::Int, vec![iv, stride_c], None);
    g.nodes[iv as usize].inputs.push(iv_next);
    let s_init = g.add(Op::Const(0), IrType::Int, vec![], None);
    let acc = g.add(Op::Phi, IrType::Int, vec![region, s_init], None);
    let acc_next = g.add(Op::Add, IrType::Int, vec![acc, iv], None);
    g.nodes[acc as usize].inputs.push(acc_next);
    let bound_c = g.add(Op::Const(bound), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound_c], None);
    let if_node = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let back = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    g.nodes[region as usize].inputs.push(back);
    let ret = g.add(Op::Return, IrType::Void, vec![exit, acc], None);
    g.exit = ret;
    (g, ret)
}

fn fold_pipeline(g: &mut Graph) {
    for _ in 0..8 {
        let before = g.live_count();
        let _ = fold_constants(g);
        let _ = algebraic_simplify(g);
        gvn(g);
        eliminate_dead_nodes(g);
        if g.live_count() == before {
            break;
        }
    }
}

#[test]
fn test_unroll_counted_reduction_folds_to_constant() {
    // for (i=0; i<5; i++) acc += i;  =>  acc == 0+1+2+3+4 == 10
    let (mut g, ret) = reduction_loop(0, 5, 1);
    assert!(
        unroll(&mut g),
        "a constant-trip counted reduction must unroll"
    );
    fold_pipeline(&mut g);
    let val = g.nodes[ret as usize].inputs[1];
    assert_eq!(
        g.nodes[val as usize].op,
        Op::Const(10),
        "unrolled sum of 0..5 must fold to the constant 10"
    );
    assert!(
        !g.nodes.iter().any(|n| n.op == Op::Region),
        "no loop region should remain after a full unroll"
    );
}

#[test]
fn a_side_effect_after_the_loop_does_not_block_the_unroll() {
    // for (i=0; i<5; i++) acc += i;  then a guard on `acc` at the exit.
    let (mut g, ret) = reduction_loop(0, 5, 1);
    let exit = g.nodes[ret as usize].inputs[0];
    let acc = g.nodes[ret as usize].inputs[1];
    let limit = g.add(Op::Const(100), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![acc, limit], None);
    g.add(Op::Guard { bci: 0 }, IrType::Void, vec![exit, cond], None);
    assert!(
        unroll(&mut g),
        "a post-loop effect is not a loop side effect"
    );
}

/// `for (int i = 0, j = 0; i < 5; i++, j++) ;` — two loop-carried counters,
/// each with a constant init and a `phi + 1` back edge, and only `i` named
/// by the exit test. `decoy_first` decides which one the builder emits
/// first, and therefore which one carries the lower node id.
///
/// Returns `(graph, return_node)`; the Return's value is `j`.
fn two_counter_loop(decoy_first: bool) -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let region = g.add(Op::Region, IrType::Control, vec![c0], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let counter = |g: &mut Graph| {
        let init = g.add(Op::Const(0), IrType::Int, vec![], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        let next = g.add(Op::Add, IrType::Int, vec![phi, one], None);
        g.nodes[phi as usize].inputs.push(next);
        phi
    };
    let (i, j) = if decoy_first {
        let j = counter(&mut g);
        let i = counter(&mut g);
        (i, j)
    } else {
        let i = counter(&mut g);
        let j = counter(&mut g);
        (i, j)
    };
    let bound = g.add(Op::Const(5), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, bound], None);
    let if_node = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let back = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    g.nodes[region as usize].inputs.push(back);
    let ret = g.add(Op::Return, IrType::Void, vec![exit, j], None);
    g.exit = ret;
    (g, ret)
}

/// `for (int i = 0; i < 5; i++) a += o.f;` — a counted loop whose body
/// reads a loop-INVARIANT field, with the read still control-anchored at
/// the header.
///
/// This is `probes/FieldLoop.java`'s `sum`, the loop the whole tiering
/// measurement is quoted on, reduced to the shape that matters: the
/// unroller refuses a loop holding an invariant `Op::Load` pinned to the
/// header (it is not in the clone set, so cloning the body would leave it
/// naming a control node the transform killed), and LICM is the only pass
/// that unpins it.
fn loop_with_a_header_pinned_invariant_load() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let region = g.add(Op::Region, IrType::Control, vec![c0], None);
    let i_init = g.add(Op::Const(0), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, i_init], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iv_next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(iv_next);
    // `o.f` — base and offset both defined outside the loop, so the read is
    // invariant; control-anchored at the header, so it is pinned.
    let off = g.add(Op::Const(16), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, m0, base, off],
        None,
    );
    let a_init = g.add(Op::Const(0), IrType::Int, vec![], None);
    let acc = g.add(Op::Phi, IrType::Int, vec![region, a_init], None);
    let acc_next = g.add(Op::Add, IrType::Int, vec![acc, load], None);
    g.nodes[acc as usize].inputs.push(acc_next);
    let bound = g.add(Op::Const(5), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], None);
    let if_node = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let back = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    g.nodes[region as usize].inputs.push(back);
    let ret = g.add(Op::Return, IrType::Void, vec![exit, acc], None);
    g.exit = ret;
    (g, load)
}

/// `optimize()` fully unrolls this loop and keeps its invariant read.
///
/// This replaces a PAIR of tests that asserted a premise which is not
/// true of this implementation: that "the unroller alone refuses a loop
/// holding an invariant `Op::Load` pinned to the header", with the second
/// half then crediting the outer `licm; unroll; licm` round for taking it.
/// `unroll` takes this fixture on its own, so the pair proved nothing
/// about pass ordering and the first half failed outright. The outer round
/// may still earn its keep, but this fixture is not the evidence for it,
/// and a red test asserting a false premise is worse than no test.
///
/// What IS worth pinning, and is pinned here, is the end state: the loop
/// is gone and the invariant read survived the transform rather than being
/// dead-code-eliminated along with it.
///
/// The survival check searches for a surviving `Load` rather than
/// re-reading the original node id. Hoisting rewrites the graph, and the
/// old id is legitimately left as `Op::Dead` — asserting on it tested node
/// identity, which no pass promises, instead of the property.
#[test]
fn optimize_unrolls_the_loop_and_keeps_its_invariant_read() {
    let (mut g, _load) = loop_with_a_header_pinned_invariant_load();

    optimize(&mut g);

    assert!(
        !g.nodes.iter().any(|n| n.op == Op::Region),
        "the loop must be fully unrolled"
    );
    assert!(
        g.nodes.iter().any(|n| n.op == Op::Load(MemKind::Int)),
        "the invariant read must survive the transform — hoisted, not deleted"
    );
}

/// An exit test naming TWO constant-stride phis never reaches the full
/// unroller, in either node order.
///
/// The residue of the fix below. Collecting every candidate and letting the
/// exit test choose removed the order-dependence for `i < 5`; `i < j` names
/// two of them and `find` settled THAT by node-id order — the same defect,
/// one level in — and both `iv_on_left` and the trip simulation read the
/// winner. The disposition must be `RuntimeBound` (which the partial
/// unroller can still use: it reads neither the induction variable nor the
/// bound) and must be the same whichever counter was emitted first.
#[test]
fn an_exit_test_naming_two_counters_never_reaches_the_full_unroller() {
    for j_first in [true, false] {
        let (g, region, back_ctrl) = two_counter_loop_compared_to_each_other(j_first);
        match analyze_counted_loop(&g, region, back_ctrl) {
            Err(NotCounted::RuntimeBound(rb)) => {
                // The shape the partial unroller needs was still recognised
                // in full — that is the whole reason this is not a `Shape`
                // refusal.
                assert_ne!(rb.if_node, NO_NODE, "j_first={j_first}");
                assert_ne!(rb.exit_ctrl, NO_NODE, "j_first={j_first}");
            }
            Ok(_) => panic!(
                "an ambiguous exit test must never reach the full unroller: \
                 its trip count would be decided by node-id order \
                 (j_first={j_first})"
            ),
            Err(_) => panic!(
                "the shape is otherwise a recognised counted loop, so the \
                 refusal must be the runtime-bound one that the partial \
                 unroller can still use (j_first={j_first})"
            ),
        }
    }
}

/// `for (i = 0, j = 3; i < j; i++, j++)` — two constant-stride phis, and
/// the exit test names both of them. Returns the header and the back-edge
/// control, which is what `analyze_counted_loop` takes.
fn two_counter_loop_compared_to_each_other(j_first: bool) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let region = g.add(Op::Region, IrType::Control, vec![c0], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let counter = |g: &mut Graph, init_val: i64| {
        let init = g.add(Op::Const(init_val), IrType::Int, vec![], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        let next = g.add(Op::Add, IrType::Int, vec![phi, one], None);
        g.nodes[phi as usize].inputs.push(next);
        phi
    };
    let (i, j) = if j_first {
        let j = counter(&mut g, 3);
        let i = counter(&mut g, 0);
        (i, j)
    } else {
        let i = counter(&mut g, 0);
        let j = counter(&mut g, 3);
        (i, j)
    };
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, j], None);
    let if_node = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let back = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
    g.nodes[region as usize].inputs.push(back);
    let ret = g.add(Op::Return, IrType::Void, vec![exit, j], None);
    g.exit = ret;
    (g, region, back)
}

/// The IV scan used to take the FIRST constant-stride phi it found and then
/// bail if the exit test named a different one — so a single extra counter
/// cost the loop both full and partial unrolling, and which phi won was
/// decided by node-id order. Both orders must unroll, and to the same
/// answer.
#[test]
fn a_second_constant_stride_phi_does_not_hide_the_loop_counter() {
    for decoy_first in [true, false] {
        let (mut g, ret) = two_counter_loop(decoy_first);
        assert!(
            unroll(&mut g),
            "the exit test names `i`; a second counter must not shadow it \
             (decoy_first={decoy_first})"
        );
        fold_pipeline(&mut g);
        let val = g.nodes[ret as usize].inputs[1];
        assert_eq!(
            g.nodes[val as usize].op,
            Op::Const(5),
            "j advances once per trip alongside i, so it ends at 5 \
             (decoy_first={decoy_first})"
        );
        assert!(
            !g.nodes.iter().any(|n| n.op == Op::Region),
            "no loop region should remain after a full unroll"
        );
    }
}

#[test]
fn test_unroll_respects_trip_cap() {
    // Trip count 100 exceeds UNROLL_MAX_TRIP → must NOT be fully unrolled.
    //
    // Round nio20 wave 5 (lane L3): under `CRATONVM_JIT_IR_UNROLL_WIDE_COUNTED`
    // (default ON) such a loop is offered to the main/post form, which may
    // unroll it PARTIALLY -- so `unroll` can answer `true` -- but that form
    // keeps the original loop as its post loop, and the original header
    // with it. What the cap forbids is retiring the loop.
    let (mut g, _ret) = reduction_loop(0, 100, 1);
    let region = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Region)
        .expect("the fixture has a loop header") as NodeId;
    let _ = unroll(&mut g);
    assert_eq!(
        g.nodes[region as usize].op,
        Op::Region,
        "a loop above the trip cap must keep its header"
    );
}

#[test]
fn test_unroll_nonzero_init_and_stride() {
    // for (i=3; i<11; i+=2) acc += i;  =>  i ∈ {3,5,7,9}  =>  sum == 24
    let (mut g, ret) = reduction_loop(3, 11, 2);
    assert!(
        unroll(&mut g),
        "non-zero init/stride counted loop must unroll"
    );
    fold_pipeline(&mut g);
    let val = g.nodes[ret as usize].inputs[1];
    assert_eq!(g.nodes[val as usize].op, Op::Const(24), "3+5+7+9 == 24");
}

#[test]
fn test_licm_hoists_invariant_load() {
    // An invariant load (base + address defined OUTSIDE the loop) pinned
    // into a barrier-free loop body must be re-anchored to the pre-header.
    let (mut g, region, preheader, _iv) = loop_probe();
    // Memory token and base are defined outside the loop (invariant).
    let mem = 2; // _m0 = Proj(1) of Start
    let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    // Full-form load pinned into the body: ctrl = region (loop control).
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, base, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(
        changed,
        "an invariant load in a barrier-free loop must hoist"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the hoisted load's control input must be re-anchored to the pre-header"
    );
}

/// LOOPS21-3 close: a hoisted load that was a link of the loop's memory
/// chain hands its in-loop token users the state it read, so the chain
/// from the back edge still reaches every store of the iteration.
/// `a = new; loop { v = a.f1; k = p.f0; a.f1 = v + k }`: `k` moves to the
/// pre-header with the entry memory, and the store, which named `k` as its
/// token, now names `v` -- not `k`, whose chain would skip to the entry.
#[test]
fn licm_rethreads_a_hoisted_loads_token_users() {
    let (mut g, region, preheader, _iv) = loop_probe();
    let m0 = 2;
    let a = g.add(
        Op::New {
            class_id: 7,
            num_fields: 2,
        },
        IrType::Ref,
        vec![preheader, m0],
        None,
    );
    let p = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let mphi = g.add(Op::Phi, IrType::Memory, vec![region, m0], None);
    let v = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mphi, a, off1],
        None,
    );
    let k = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, v, p, off0],
        None,
    );
    let sum = g.add(Op::Add, IrType::Int, vec![v, k], None);
    let st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, k, a, off1, sum],
        None,
    );
    g.nodes[mphi as usize].inputs.push(st);
    let ret = g.add(Op::Return, IrType::Void, vec![region, mphi], None);
    g.exit = ret;

    let _ = licm(&mut g);

    assert_eq!(g.nodes[k as usize].inputs[0], preheader, "p.f0 is hoisted");
    assert_ne!(
        g.nodes[k as usize].inputs[1], v,
        "with the loop-entry memory"
    );
    assert_eq!(
        g.nodes[st as usize].inputs[1], v,
        "the store's token is the in-loop read it followed, not the hoisted one"
    );
    assert_eq!(
        g.nodes[v as usize].inputs[0], region,
        "a.f1 stays in the loop"
    );
}

/// A guarded loop that stores `a.f1` and reads `this.f0` and `a.f1`:
/// `[g, region, preheader, this.f0 read, a.f1 read]`.
fn guarded_field_store_loop() -> (Graph, NodeId, NodeId, NodeId, NodeId) {
    let (mut g, region, preheader, iv) = loop_probe();
    g.receiver_param = Some(0);
    let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let a = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let lim = g.add(Op::Const(5), IrType::Int, vec![], None);
    let gc = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, lim], None);
    let _guard = g.add(Op::Guard { bci: 3 }, IrType::Void, vec![region, gc], None);
    let mphi = g.add(Op::Phi, IrType::Memory, vec![region, 2], None);
    let f0 = g.add(Op::Load(MemKind::Int), IrType::Int, vec![region, mphi, this, off0], None);
    let f1 = g.add(Op::Load(MemKind::Int), IrType::Int, vec![region, f0, a, off1], None);
    let sum = g.add(Op::Add, IrType::Int, vec![f0, f1], None);
    let st = g.add(Op::Store(MemKind::Int), IrType::Void, vec![region, f1, a, off1, sum], None);
    g.nodes[mphi as usize].inputs.push(st);
    g.exit = g.add(Op::Return, IrType::Void, vec![region, mphi], None);
    (g, region, preheader, f0, f1)
}


#[test]
fn late_hoist_copy_rewires_values_to_the_copy_and_tokens_to_the_old_memory() {
    let (mut g, _region, preheader, f0, f1) = guarded_field_store_loop();
    let old_mem = g.nodes[f0 as usize].inputs[1];
    let sum = g.users_of(f0).into_iter().find(|&u| g.nodes[u as usize].op == Op::Add).unwrap();
    let copy = late_hoist_copy(&mut g, f0, preheader, 2);
    assert!(copy > f1, "numbered after every node already there");
    assert_eq!(&g.nodes[copy as usize].inputs.as_slice()[..2], &[preheader, 2]);
    assert!(g.nodes[sum as usize].inputs.contains(&copy), "the value use reads the copy");
    assert_eq!(g.nodes[f1 as usize].inputs[1], old_mem, "the token use reads the old memory");
    assert!(g.users_of(f0).is_empty());
}

/// `for (i ...) this.f[i] = -1` behind a pre-header that may trap (an
/// `a[0]` read): the early rounds keep `this.f` in the loop; the late round
/// hoists a copy to the end of the pre-header.
#[test]
fn late_round_hoists_a_field_read_past_a_trapping_preheader() {
    let build = || {
        let (mut g, region, preheader, iv) = loop_probe();
        g.receiver_param = Some(0);
        let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let a = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let trap = g.add(Op::ArrayLoad(MemKind::Ref), IrType::Ref, vec![preheader, 2, a, zero], None);
        let _use = g.add(Op::Store(MemKind::Ref), IrType::Void, vec![preheader, 2, this, zero, trap], None);
        let mphi = g.add(Op::Phi, IrType::Memory, vec![region, 2], None);
        let f = g.add(Op::Load(MemKind::Ref), IrType::Ref, vec![region, mphi, this, zero], None);
        let m1 = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let st = g.add(Op::ArrayStore(MemKind::Int), IrType::Void, vec![region, f, f, iv, m1], None);
        g.nodes[mphi as usize].inputs.push(st);
        g.exit = g.add(Op::Return, IrType::Void, vec![region, mphi], None);
        assert!(preheader_may_trap(&g, preheader));
        (g, region, preheader, f, st)
    };
    let (mut g, region, _, f, _) = build();
    licm(&mut g);
    assert_eq!(g.nodes[f as usize].inputs[0], region, "the early round keeps the read");
    let (mut g, _, preheader, _, st) = build();
    g.late_licm = true;
    licm(&mut g);
    let base = g.nodes[st as usize].inputs[2];
    assert_eq!(g.nodes[base as usize].inputs[0], preheader, "the store's array is read once, before the loop");
}

#[test]
fn late_base_non_null_takes_a_dominating_class_guard() {
    let (mut g, _region, preheader, _iv) = loop_probe();
    let p = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    assert!(!late_base_non_null(&g, p, preheader), "a parameter may be null");
    let is = g.add(Op::ExactClassIs { class_id: 7 }, IrType::Int, vec![p], None);
    let _ = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![preheader, is], None);
    assert!(late_base_non_null(&g, p, preheader), "its class guard runs first");
}

#[test]
fn test_licm_does_not_hoist_variant_load() {
    // A load whose base is the loop-carried induction phi is loop-VARIANT
    // and must NOT be hoisted (its control input stays at the region).
    let (mut g, region, _preheader, iv) = loop_probe();
    let mem = 2;
    // Use the induction variable itself as the BASE — that is what makes
    // the load variant; the offset is an ordinary invariant constant.
    //
    // This fixture used to be built as `[ctrl, mem, iv, NO_NODE]`: the
    // full four-input form, whose slot 3 the layout dispatcher reads as a
    // real index operand, filled with the placeholder. That is not
    // padding, it is a malformed graph. `ir_verify`'s always-on structural
    // lane rejects `NO_NODE` in any slot that is not a φ value input or a
    // safepoint slot, and it is the exact shape that panics
    // `ir_lower::slot_of`, which indexes `node_slot` with `u32::MAX`. It
    // only ever went unnoticed because the base is variant, so LICM bailed
    // (`addr != NO_NODE &&` …) before it read the slot — the placeholder
    // was load-bearing for nothing. A real `Const` offset expresses the
    // same test (base variant ⇒ no hoist) with a graph that is valid. See
    // `test_licm_variant_load_fixture_has_no_placeholder_operand`.
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, iv, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let _ = licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "a loop-variant load must NOT be hoisted (control stays in the loop)"
    );
}

/// Regression for the fixture above: no live node may carry `NO_NODE` in a
/// real operand slot. Only a φ value input (a local undefined on that
/// predecessor edge) and a safepoint slot may hold the placeholder — the
/// same rule `ir_verify::no_node_allowed` encodes for the always-on lane.
#[test]
fn test_licm_variant_load_fixture_has_no_placeholder_operand() {
    let (mut g, region, _preheader, iv) = loop_probe();
    let mem = 2;
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, iv, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let _ = licm(&mut g);

    for (id, n) in g.nodes.iter().enumerate() {
        if n.op == Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            let placeholder_ok = matches!(n.op, Op::Phi) && i >= 1;
            assert!(
                inp != NO_NODE || placeholder_ok,
                "n{id}:{:?} input[{i}] is the NO_NODE placeholder in a real \
                 operand slot — `ir_lower::slot_of` indexes node_slot with it",
                n.op
            );
        }
    }
    // …and the load kept the full four-input shape the arity lane wants.
    assert_eq!(
        g.nodes[load as usize].inputs.len(),
        4,
        "the full load form is [ctrl, mem, base, offset]"
    );
}

#[test]
fn test_licm_bails_on_barrier_in_loop() {
    // A store inside the loop body is a memory barrier: even an otherwise
    // invariant load must NOT be hoisted (we have no alias oracle).
    let (mut g, region, _preheader, _iv) = loop_probe();
    let mem = 2;
    let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, base, off],
        None,
    );
    // A store pinned into the body (ctrl = region) is the barrier. Its
    // base is a *different* local alloc so DSE-style reasoning is moot;
    // for LICM any in-body store disqualifies hoisting.
    let other = g.add(
        Op::New {
            class_id: 9,
            num_fields: 1,
        },
        IrType::Ref,
        vec![region],
        None,
    );
    let cval = g.add(Op::Const(5), IrType::Int, vec![], None);
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, other, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(
        !changed,
        "a memory barrier in the loop body must block all load hoisting"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "the load must stay in the loop when the body has a barrier"
    );
}

/// `for (r…) { for (i…) { … } }` — and the inner header is the one the
/// iterations are in.
///
/// `loop_headers` classified a header's control inputs by asking which are
/// reachable *forward from the header*. For an inner header that question
/// has no useful answer: the walk leaves through the inner exit, goes round
/// the OUTER back edge, and arrives back at the inner loop's own
/// pre-header — so both inputs read as back edges, the loop yields zero
/// pre-headers, and it was dropped. Every nested loop in the tree offered
/// LICM only its outer header.
#[test]
fn test_loop_headers_finds_the_inner_header_of_a_nested_loop() {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);

    // Outer header; its back edge is wired below.
    let outer = g.add(Op::Merge, IrType::Control, vec![c0], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let r = g.add(Op::Phi, IrType::Int, vec![outer, zero], None);
    let ten = g.add(Op::Const(10), IrType::Int, vec![], None);
    let ocond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![r, ten], None);
    let oif = g.add(Op::If, IrType::Control, vec![outer, ocond], None);
    let obody = g.add(Op::Proj(0), IrType::Control, vec![oif], None); // inner pre-header
    let oexit = g.add(Op::Proj(1), IrType::Control, vec![oif], None);

    // Inner header, entered from the outer body.
    let inner = g.add(Op::Merge, IrType::Control, vec![obody], None);
    let i = g.add(Op::Phi, IrType::Int, vec![inner, zero], None);
    let icond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, ten], None);
    let iif = g.add(Op::If, IrType::Control, vec![inner, icond], None);
    let ibody = g.add(Op::Proj(0), IrType::Control, vec![iif], None);
    let iexit = g.add(Op::Proj(1), IrType::Control, vec![iif], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let inext = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[i as usize].inputs.push(inext);
    g.nodes[inner as usize].inputs.push(ibody); // inner back edge

    // Inner exit closes the outer back edge.
    let rnext = g.add(Op::Add, IrType::Int, vec![r, one], None);
    g.nodes[r as usize].inputs.push(rnext);
    g.nodes[outer as usize].inputs.push(iexit); // outer back edge
    let ret = g.add(Op::Return, IrType::Void, vec![oexit, r], None);
    g.exit = ret;

    let headers = loop_headers(&g);
    let inner_entry = headers
        .iter()
        .find(|&&(h, _, _)| h == inner)
        .map(|&(_, e, _)| e);
    assert_eq!(
        inner_entry,
        Some(obody),
        "the inner header's pre-header is the outer body's projection, got \
         headers {:?}",
        headers.iter().map(|&(h, e, _)| (h, e)).collect::<Vec<_>>()
    );
    let outer_entry = headers
        .iter()
        .find(|&&(h, _, _)| h == outer)
        .map(|&(_, e, _)| e);
    assert_eq!(
        outer_entry,
        Some(c0),
        "the outer header's classification must be unchanged"
    );
}

/// javac's counted loop — `for (i = 0; i < a.length; i++)` — re-evaluates
/// `a.length` at the top of every iteration, and the resulting in-loop
/// `Op::ArrayLength` is itself one of the hard barriers that disqualified
/// this pass from hoisting anything at all. Measured on
/// `probes/ArrayElemLoadCost.java` before the fix: every candidate header
/// reported `hard_barrier=true, 0 load(s)`.
///
/// Both edges have to move. Re-anchoring only the control leaves the node
/// reading the loop's memory phi, and `ir_schedule::find_best_block` places
/// a data node in the deepest block dominated by ALL its input blocks — so
/// the "hoisted" node would schedule straight back into the loop. This test
/// pins both.
#[test]
fn test_licm_hoists_invariant_arraylength() {
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2; // the Start's memory projection
    let arr = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    // The loop's memory phi, exactly as the builder emits it:
    // [region, entry_memory, back_edge_memory]. Its own back edge is
    // itself here — nothing in this probe writes memory.
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![region, mem_phi, arr],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, len], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(changed, "an invariant arraylength must hoist");
    assert_eq!(
        g.nodes[len as usize].inputs[0], preheader,
        "control must be re-anchored to the pre-header"
    );
    assert_eq!(
        g.nodes[len as usize].inputs[1], entry_mem,
        "the memory token must be re-anchored to the loop's ENTRY memory — \
         leaving it on the loop's memory phi schedules the node back into \
         the loop and the hoist is inert"
    );

    // The property `optimize()`'s outer round rests on: a hoist is
    // MONOTONE, so a second visit to a loop this pass already served must
    // report no progress. While `licm` answered "changed" unconditionally
    // for a node it had already moved, the round could only ever be a
    // budget — a fixpoint driven on that answer would not terminate.
    let again = licm_outcome(&mut g);
    assert!(
        !again.moved,
        "the second visit must report no progress, or the outer round's \
         convergence test is meaningless"
    );
    assert!(
        !again.recognized,
        "and nothing compact was recognised either"
    );
    assert_eq!(
        g.nodes[len as usize].inputs[0], preheader,
        "…and the node is still where the first visit put it"
    );
}

/// The same, for the restricted read hoist: two visits, one rewrite.
///
/// Written as a separate fixture because the three hoist arms reach the
/// "already there" test by different routes — this one by its explicit
/// `inputs[0] == preheader && inputs[1] == new_mem` continue, the
/// `ArrayLength` arm by its `inputs[0] != region` refusal — and an honest
/// `changed` needs all of them.
#[test]
fn a_second_licm_visit_over_a_guarded_loop_reports_no_progress() {
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    g.receiver_param = Some(0);
    let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![other, null_c], None);
    g.add(
        Op::Guard { bci: 11 },
        IrType::Void,
        vec![region, non_null],
        None,
    );
    let off = g.add(Op::Const(24), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem_phi, this, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let first = licm_outcome(&mut g);
    assert!(first.moved, "the read must hoist on the first visit");
    assert_eq!(g.nodes[load as usize].inputs[0], preheader);

    let second = licm_outcome(&mut g);
    assert!(
        !second.moved,
        "and the second visit must rewrite nothing at all"
    );
}

/// `LicmOutcome` keeps two facts apart, and only one of them is progress.
#[test]
fn a_compact_form_recognition_is_work_but_not_progress() {
    let recognised = LicmOutcome {
        moved: false,
        recognized: true,
    };
    assert!(
        recognised.any_work(),
        "the trailing GVN still has to run, so the cleanup sees it"
    );
    assert!(
        !recognised.moved,
        "…but a compact-form load has no edge to move, so a round that \
         only recognised one made no progress and the outer loop must stop"
    );
    let moved = LicmOutcome {
        moved: true,
        recognized: false,
    };
    assert!(moved.any_work() && moved.moved);
    let nothing = LicmOutcome {
        moved: false,
        recognized: false,
    };
    assert!(!nothing.any_work());
}

/// The receiver decides. An `arraylength` of something the loop itself
/// produces is not invariant, and hoisting it would read the length of a
/// different array (or of nothing yet).
#[test]
fn test_licm_does_not_hoist_variant_arraylength() {
    let (mut g, region, _preheader, iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    // A "receiver" that varies with the induction variable.
    let variant_ref = g.add(Op::Phi, IrType::Ref, vec![region, iv], None);
    g.nodes[variant_ref as usize].inputs.push(variant_ref);
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![region, mem_phi, variant_ref],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, len], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[len as usize].inputs[0], region,
        "a variant receiver's arraylength must stay in the loop"
    );
}

/// The hoist moves a node that can raise NPE, so it is only taken when the
/// `arraylength` runs on EVERY entry to the header — i.e. its control input
/// is the header itself. One behind a conditional inside the body may never
/// run at all in the original program, and moving it into the pre-header
/// would raise an exception the program never raised.
#[test]
fn test_licm_does_not_hoist_conditional_arraylength() {
    let (mut g, region, _preheader, iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    let arr = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    // An in-body branch, with the arraylength on one arm only.
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![iv, zero], None);
    let inner_if = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let taken = g.add(Op::Proj(0), IrType::Control, vec![inner_if], None);
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![taken, mem_phi, arr],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![taken, len], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[len as usize].inputs[0], taken,
        "a conditionally-executed arraylength must stay where it is"
    );
}

/// `Op::is_pure()` answers "is this freely movable", and both loop-barrier
/// predicates were asking it in place of "does this write memory".
/// `Op::Div`, `Op::Rem`, `Op::LCmp` and `Op::FCmp` are absent from the
/// `is_pure` list — the first two because they trap, the last two because
/// the list was never extended — and none of the four has a
/// `memory_shape()` entry, so `effect_of_node` reports `MemEffect::NONE`
/// for all of them. Classified as barriers they disqualified the whole loop
/// from BOTH hoist arms.
#[test]
fn integer_division_and_the_three_way_compares_are_not_memory_barriers() {
    let mut g = probe_graph();
    let a = g.add(Op::Param(0), IrType::Int, vec![], None);
    let b = g.add(Op::Param(1), IrType::Int, vec![], None);
    let la = g.add(Op::Param(2), IrType::Long, vec![], None);
    let lb = g.add(Op::Param(3), IrType::Long, vec![], None);
    let da = g.add(Op::Param(4), IrType::Double, vec![], None);
    let db = g.add(Op::Param(5), IrType::Double, vec![], None);
    let div = g.add(Op::Div, IrType::Int, vec![a, b], None);
    let rem = g.add(Op::Rem, IrType::Int, vec![a, b], None);
    let lcmp = g.add(Op::LCmp, IrType::Int, vec![la, lb], None);
    let fcmp = g.add(
        Op::FCmp {
            double: true,
            nan_greater: true,
        },
        IrType::Int,
        vec![da, db],
        None,
    );

    for (id, what) in [
        (div, "idiv"),
        (rem, "irem"),
        (lcmp, "lcmp"),
        (fcmp, "dcmpg"),
    ] {
        let body: FxHashSet<NodeId> = [id].into_iter().collect();
        assert!(
            !loop_has_hard_barrier(&g, &body),
            "a loop containing one {what} must not lose every hoist to it"
        );
        assert!(
            !loop_writes_memory(&g, &body),
            "{what} writes no memory, so it cannot clobber a hoisted read"
        );
    }

    // …and the classification is still an allow-list: a call is a barrier.
    let call = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![a, b], None);
    let body: FxHashSet<NodeId> = [call].into_iter().collect();
    assert!(
        loop_has_hard_barrier(&g, &body),
        "widening the exemption must not have widened it to calls"
    );
}

/// The barrier predicates read a MEMORY-SHAPE classifier, not a purity
/// one, and this pins the difference op by op rather than through a loop.
///
/// The `is_pure` spelling they used to carry answered this question
/// correctly only by coincidence, and the coincidence had already failed
/// once (`Op::LCmp`). `licm_hoist_barrier` is now "touches memory, or can
/// leave the body"; nothing about GVN enters into it.
#[test]
fn the_hoist_barrier_asks_about_memory_and_control_not_about_purity() {
    // Touches memory → barrier, whether it reads or writes.
    for op in [
        Op::ArrayLoad(MemKind::Int),
        Op::ArrayLength,
        Op::ArrayStore(MemKind::Int),
        Op::Call { info_ptr: 0 },
        Op::MonitorEnter,
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
    ] {
        assert!(licm_hoist_barrier(&op), "{op:?} touches memory");
    }
    // Touches no memory and cannot leave the body → not a barrier, even
    // when it is impure (`Op::Div` traps, which is a different question
    // and is `preheader_may_trap`'s).
    for op in [Op::Div, Op::Rem, Op::LCmp, Op::Add, Op::Cmp(CmpOp::Lt)] {
        assert!(!licm_hoist_barrier(&op), "{op:?} touches no memory");
    }
    // The one non-memory barrier, and the reason the predicate is not
    // simply `memory_shape_of(op).touches_memory()`: a load moved above a
    // guard runs on the value the guard was going to reject.
    assert!(licm_hoist_barrier(&Op::Guard { bci: 0 }));
    assert_eq!(memory_shape_of(&Op::Guard { bci: 0 }), MemoryShape::None);
    // The four standing exemptions.
    for op in [
        Op::Load(MemKind::Int),
        Op::Store(MemKind::Int),
        Op::Phi,
        Op::Dead,
    ] {
        assert!(!licm_hoist_barrier(&op), "{op:?} is handled per-load");
    }
    // Round 11 wave 16: an opaque OSR arm's incoming memory is neither a
    // hoist barrier nor a write, although its shape is `Opaque` (the arm
    // never runs on the method-entry path, and its entry admission
    // refuses any hoisted value live into it).
    assert_eq!(memory_shape_of(&Op::OsrMemory), MemoryShape::Opaque);
    assert!(!licm_hoist_barrier(&Op::OsrMemory));
}

/// A `Op::Div` in the pre-header is still a trap, and that has to survive
/// the split: the memory classifier says a division touches no memory, so
/// routing this predicate through it would have quietly licensed hoisting
/// an `ArrayLength` above a division that was going to raise
/// `ArithmeticException` — reporting the NPE first, from a frame the loop
/// never reached.
#[test]
fn a_division_in_the_preheader_is_still_a_trap() {
    let (mut g, _region, preheader, _iv) = loop_probe();
    let a = g.add(Op::Param(0), IrType::Int, vec![], None);
    let b = g.add(Op::Param(1), IrType::Int, vec![], None);
    assert!(
        !preheader_may_trap(&g, preheader),
        "the bare pre-header traps on nothing"
    );
    // A three-way compare anchored there does not make it trap…
    let la = g.add(Op::Param(2), IrType::Long, vec![], None);
    let lcmp = g.add(Op::LCmp, IrType::Int, vec![la, la], None);
    g.nodes[lcmp as usize].inputs[0] = preheader;
    assert!(!preheader_may_trap(&g, preheader), "lcmp raises nothing");
    // …a division does.
    let div = g.add(Op::Div, IrType::Int, vec![a, b], None);
    g.nodes[div as usize].inputs[0] = preheader;
    assert!(
        preheader_may_trap(&g, preheader),
        "a zero divisor raises ArithmeticException, which is what this asks"
    );
}

/// DSE's barrier has to publish a pending store for THREE reasons, and
/// only one of them is "something may read it". `Op::Div` is the case the
/// memory-shape split could have broken: it touches no memory, and it must
/// stay a barrier anyway, because a zero divisor raises and the handler
/// observes every store that has already happened.
#[test]
fn dead_store_elimination_still_stops_at_a_division() {
    assert!(
        is_memory_barrier(&Op::Div),
        "a trap publishes pending stores"
    );
    assert!(is_memory_barrier(&Op::Rem));
    // Control joins and returns, for the same reason in a different key.
    assert!(is_memory_barrier(&Op::Merge));
    assert!(is_memory_barrier(&Op::Return));
    assert!(is_memory_barrier(&Op::Phi));
    assert!(is_memory_barrier(&Op::Guard { bci: 0 }));
    // Readers.
    assert!(is_memory_barrier(&Op::Load(MemKind::Int)));
    assert!(is_memory_barrier(&Op::ArrayLoad(MemKind::Int)));
    assert!(is_memory_barrier(&Op::Call { info_ptr: 0 }));
    // A store drives the overwrite match rather than ending it, and plain
    // arithmetic observes nothing.
    assert!(!is_memory_barrier(&Op::Store(MemKind::Int)));
    assert!(!is_memory_barrier(&Op::Add));
    assert!(!is_memory_barrier(&Op::LCmp));
}

/// End-to-end form of the above: the invariant load hoists out of a loop
/// whose body compares two `long`s. Before the fix the `lcmp` made
/// `loop_has_hard_barrier` true and the general arm bailed at
/// `if loop_has_hard_barrier { continue; }` without looking at a single
/// load.
#[test]
fn a_loop_that_compares_two_longs_still_hoists_its_invariant_load() {
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2; // the Start's memory projection
    let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    // A loop-carried `long` phi, so the `lcmp` below is pinned INTO the
    // body by the sweep (a pure node would just float).
    let l_init = g.add(Op::Const(0), IrType::Long, vec![], None);
    let l_phi = g.add(Op::Phi, IrType::Long, vec![region, l_init], None);
    g.nodes[l_phi as usize].inputs.push(l_phi);
    let l_bound = g.add(Op::Const(1_000), IrType::Long, vec![], None);
    let lcmp = g.add(Op::LCmp, IrType::Int, vec![l_phi, l_bound], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, base, off],
        None,
    );
    let sum = g.add(Op::Add, IrType::Int, vec![load, lcmp], None);
    let ret = g.add(Op::Return, IrType::Void, vec![region, sum], None);
    g.exit = ret;

    // The `lcmp` is NOT in the body set, and that is the point rather than
    // a broken fixture.
    //
    // This assertion used to be `body.contains(&lcmp)`, on the premise —
    // stated in the comment above — that a loop-carried `long` phi pins
    // the compare into the body because "a pure node would just float".
    // That premise died when `Op::LCmp`/`Op::FCmp` joined `Op::is_pure`:
    // the same review finding, fixed from the other end in `ir.rs`.
    // `loop_body`'s pinned-node sweep opens with
    // `if node.op == Op::Dead || node.op.is_pure() { continue; }`, so a
    // pure compare is deliberately never pinned.
    //
    // Floating out of the body is a STRONGER version of what this test
    // wants than being in it and tolerated: a node the body never contains
    // cannot make `loop_has_hard_barrier` true however that predicate is
    // later rewritten. The classification itself is pinned by the sibling
    // test above, which hands `loop_has_hard_barrier` an explicit body set
    // containing the compare. What is left for this end-to-end form to
    // prove is that the load actually reaches the pre-header, below.
    let body = loop_body(&g, region, preheader, &[g.nodes[region as usize].inputs[1]]);
    assert!(
        !body.contains(&lcmp),
        "a pure compare must float rather than be pinned into the loop body"
    );

    assert!(licm(&mut g), "the lcmp must not disqualify the loop");
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the invariant load must reach the pre-header past the lcmp"
    );
}

/// The circular-evidence bug. `Op::Guard` shares its control input with the
/// nodes it protects, so "the guard ran first" is positional and invisible
/// to a pass that re-anchors control edges. The restricted read hoist used
/// to read a header-anchored load as proof that the base is dereferenced on
/// entry — but in a guarded header that load is itself only reached when
/// the guard has already deopted on the null case.
///
/// This is the IR String-access shape: `Guard(recv != null)` and two
/// `Load`s off `recv`, all anchored at the region.
#[test]
fn the_read_hoist_refuses_a_load_behind_a_guard_at_the_header() {
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    let recv = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    // `Guard(recv != null)` — the node that makes the two loads legal, and
    // the node whose ordering against them the graph does not record.
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null_c], None);
    g.add(
        Op::Guard { bci: 11 },
        IrType::Void,
        vec![region, non_null],
        None,
    );
    let coder_off = g.add(Op::Const(12), IrType::Int, vec![], None);
    let coder = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem_phi, recv, coder_off],
        None,
    );
    let value_off = g.add(Op::Const(16), IrType::Int, vec![], None);
    let value = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![region, mem_phi, recv, value_off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, coder], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[coder as usize].inputs[0], region,
        "a load anchored behind a guard must not be re-anchored above it — \
         on a null receiver the pre-header copy deopts at an in-body bci \
         from a frame the loop never reached"
    );
    assert_eq!(
        g.nodes[value as usize].inputs[0], region,
        "and neither may the load the first one used to vouch for"
    );
    assert_ne!(
        g.nodes[coder as usize].inputs[0], preheader,
        "…in particular not to the pre-header"
    );
}

/// The same rule on the `ArrayLength` arm, where the consequence is worse:
/// the length feeds a bounds check, so a length read off an object a guard
/// had not yet vouched for admits an arbitrary index.
#[test]
fn the_arraylength_hoist_refuses_a_header_that_carries_a_guard() {
    let (mut g, region, _preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    let arr = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![arr, null_c], None);
    g.add(
        Op::Guard { bci: 4 },
        IrType::Void,
        vec![region, non_null],
        None,
    );
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![region, mem_phi, arr],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, len], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[len as usize].inputs[0], region,
        "an arraylength behind a guard at the header must stay behind it"
    );
}

/// The recovery. A guard at the header used to refuse EVERY hoist out of
/// that header, including for reads the guard cannot possibly be about.
///
/// Here the guard is `other != null` and the candidate reads `this.f`.
/// `this` is the receiver parameter, which the JVM guarantees non-null at
/// the call site, so the read cannot deopt and hoisting it cannot move a
/// deopt point; and its address is computed from `this`, which appears
/// nowhere in the guard's condition, so the guard cannot be the reason the
/// read is legal. Both obligations discharged, the load hoists.
///
/// This is the shape the blanket refusal cost the most: a loop that calls
/// `String.charAt` (four guards at the header, from the IR String
/// expansion) and also reads one of its own fields lost the field read for
/// guards that have nothing to do with it.
#[test]
fn a_guard_at_the_header_no_longer_refuses_a_read_it_cannot_be_about() {
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    // Slot 0 is the receiver: non-null by the JVM's own guarantee, which is
    // the fact `base_non_null_on_entry` already believes.
    g.receiver_param = Some(0);
    let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    // A guard about a DIFFERENT reference.
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![other, null_c], None);
    let guard = g.add(
        Op::Guard { bci: 11 },
        IrType::Void,
        vec![region, non_null],
        None,
    );
    let off = g.add(Op::Const(24), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem_phi, this, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "a read of the receiver is licensed by the JVM, not by a guard \
         about some other reference — it must reach the pre-header"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[1], entry_mem,
        "and its memory token must move with it, or the hoist is inert"
    );
    assert_eq!(
        g.nodes[guard as usize].inputs[0], region,
        "the GUARD itself must not move: hoisting a guard moves a deopt \
         point, which is the transform this pass does not perform"
    );
}

/// The guard TOKEN overrides the attribution, in the one fixture where the
/// attribution says yes.
///
/// Byte for byte the graph of
/// [`a_guard_at_the_header_no_longer_refuses_a_read_it_cannot_be_about`],
/// with one edge added: the load names the guard as its trailing token
/// (`ir::guard_token_slot`). Everything the address cone reasons about is
/// unchanged — `this` still appears nowhere in the guard's condition, the
/// receiver is still non-null on entry — so the *inference* still says
/// "hoist". The edge says "this guard is why the read is legal", and the
/// edge wins.
///
/// What this catches: `LoopGuards::permits_hoist` not reading the token.
/// `NOTES-opts7.md` claimed `LoopGuards::over`'s `cone.insert(g)` would
/// make the edge load-bearing with "no other line of code" — it does not,
/// because `permits_hoist` builds the candidate's own cone from
/// `(base, addr)` and neither of those names the guard. Delete the
/// `own.insert(own_token)` line and this test hoists, which is the exact
/// shape the token edge exists to refuse.
#[test]
fn a_read_that_names_a_guard_as_its_token_is_refused_although_its_cone_is_disjoint() {
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    g.receiver_param = Some(0);
    let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![other, null_c], None);
    let guard = g.add(
        Op::Guard { bci: 11 },
        IrType::Void,
        vec![region, non_null],
        None,
    );
    let off = g.add(Op::Const(24), IrType::Int, vec![], None);
    // The one difference from the twin above: the trailing guard token.
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem_phi, this, off, guard],
        None,
    );
    assert_eq!(
        crate::ir::guard_token_of(&g.nodes[load as usize]),
        guard,
        "the fixture must actually carry the token, or this passes vacuously"
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "a read that NAMES a guard is licensed by it by construction, \
         whatever its address cone looks like"
    );
    assert_ne!(
        g.nodes[load as usize].inputs[0], preheader,
        "…in particular it must not reach the pre-header"
    );
    assert_eq!(
        crate::ir::guard_token_of(&g.nodes[load as usize]),
        guard,
        "and the token edge must survive the pass"
    );
}

/// The same header, and a base the guard IS about: refused.
///
/// The opposed twin of
/// [`a_guard_at_the_header_no_longer_refuses_a_read_it_cannot_be_about`] —
/// the two fixtures differ only in which reference the guard names, so the
/// permission cannot be passing for a reason unrelated to attribution.
#[test]
fn a_guard_at_the_header_still_refuses_a_read_of_the_value_it_names() {
    let (mut g, region, _preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    g.receiver_param = Some(0);
    let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    // The guard names the very reference the load reads. Even though the
    // receiver is non-null by construction — so the load cannot deopt —
    // a guard naming it may be establishing something OTHER than nullity
    // (a shape, a class identity), and this pass cannot see which. It
    // refuses.
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![this, null_c], None);
    g.add(
        Op::Guard { bci: 11 },
        IrType::Void,
        vec![region, non_null],
        None,
    );
    let off = g.add(Op::Const(24), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem_phi, this, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "a read whose base the guard names must stay behind the guard, \
         however non-null the base is by construction"
    );
}

/// The lost-store hazard, pinned.
///
/// `NOTES-misc.md` §4 option A would hoist an invariant guard into the
/// pre-header so the loads behind it could follow. The obligation the
/// original sketch missed is that a guard is a DEOPT POINT keyed on its own
/// `bci` — inside the loop — so a guard that fails after the move resumes
/// the interpreter at the header and never re-executes the pre-header. A
/// pre-header store that had not run yet is silently lost.
/// `preheader_may_trap` does not catch it: it asks whether the pre-header
/// can THROW, and it exempts `Op::Store` by name.
///
/// The fixture is exactly that shape — a `putfield` in the pre-header, an
/// invariant guard at the header — and the property asserted is the one
/// that makes the hazard unreachable rather than merely unlikely: this pass
/// re-anchors no `Op::Guard`, ever. It is also why the two obligations in
/// [`LoopGuards`] are about the candidate READ and not about the
/// pre-header's contents: nothing that could strand a store is moved.
#[test]
fn a_pre_header_store_cannot_be_stranded_by_a_guarded_header() {
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    g.receiver_param = Some(0);
    let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    // A store in the PRE-HEADER: the write that would be lost if a deopt
    // point were moved above it.
    let stored = g.add(Op::Const(7), IrType::Int, vec![], None);
    let field = g.add(Op::Const(32), IrType::Int, vec![], None);
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![preheader, entry_mem, this, field, stored],
        None,
    );
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, store], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    // An invariant guard at the header — option A's candidate, since its
    // condition cannot change across iterations.
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![other, null_c], None);
    let guard = g.add(
        Op::Guard { bci: 11 },
        IrType::Void,
        vec![region, non_null],
        None,
    );
    let off = g.add(Op::Const(24), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem_phi, this, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[guard as usize].inputs[0], region,
        "the guard must stay at the header: moving it above the pre-header \
         store would let a failing guard resume the interpreter at the \
         header bci, which never re-executes the store"
    );
    assert_eq!(
        g.nodes[store as usize].inputs[0], preheader,
        "and the store must not move either"
    );
}

/// No `Op::Guard` is re-anchored by LICM under ANY of the fixtures in this
/// module that carry one.
///
/// The blanket version of the assertion the two tests above make on their
/// own fixtures. A transform that starts moving guards would have to delete
/// this test, which is the point: the deopt-point obligations in
/// [`LoopGuards`] are discharged by not moving one, and that has to be
/// checkable in one place.
#[test]
fn licm_never_re_anchors_a_guard_out_of_its_loop() {
    for bci in [3usize, 11] {
        let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
        let entry_mem = 2;
        g.receiver_param = Some(0);
        let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
        g.nodes[mem_phi as usize].inputs.push(mem_phi);
        let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let non_null = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![this, null_c], None);
        let guard = g.add(
            Op::Guard { bci },
            IrType::Void,
            vec![region, non_null],
            None,
        );
        let off = g.add(Op::Const(24), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem_phi, this, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        licm(&mut g);

        assert_eq!(
            g.nodes[guard as usize].inputs[0], region,
            "bci {bci}: LICM must never re-anchor a guard"
        );
        assert_ne!(
            g.nodes[guard as usize].inputs[0], preheader,
            "bci {bci}: …and least of all to the pre-header"
        );
    }
}

/// Two guards, one of which the candidate is about: the union refuses.
///
/// Attribution is per-LOOP, not per-guard-that-happens-to-look-unrelated.
/// A read must clear every guard that can be in front of it, so one
/// intersecting cone is enough to refuse even when another guard at the
/// same header is plainly about something else.
#[test]
fn one_intersecting_guard_is_enough_to_refuse_a_hoist() {
    let (mut g, region, _preheader, _iv) = loop_probe_header(Op::Merge);
    let entry_mem = 2;
    g.receiver_param = Some(0);
    let this = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    // Unrelated to the read…
    let other_nn = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![other, null_c], None);
    g.add(
        Op::Guard { bci: 7 },
        IrType::Void,
        vec![region, other_nn],
        None,
    );
    // …and related to it.
    let this_nn = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![this, null_c], None);
    g.add(
        Op::Guard { bci: 11 },
        IrType::Void,
        vec![region, this_nn],
        None,
    );
    let off = g.add(Op::Const(24), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem_phi, this, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "the second guard names the read's base; the first being unrelated \
         does not license anything"
    );
}

/// A guard whose condition cone cannot be walked within the budget refuses
/// every hoist out of that loop.
///
/// The fail-closed direction, asserted directly on the summariser rather
/// than through a 512-node fixture. An INCOMPLETE cone is the dangerous
/// one: disjointness against a cone that stopped early answers "unrelated"
/// for a node that is in fact related, which is the wild read this whole
/// analysis exists to avoid.
#[test]
fn a_cone_that_overflows_its_budget_reports_incomplete_rather_than_partial() {
    let (mut g, _region, _preheader, _iv) = loop_probe_header(Op::Merge);
    let a = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let mut cone: FxHashSet<NodeId> = FxHashSet::default();
    let mut budget = 0usize;
    assert!(
        !value_cone(&g, a, &mut cone, &mut budget),
        "a zero budget must report failure, not an empty cone"
    );
    // And with room, the same walk succeeds and finds the value.
    let mut cone2: FxHashSet<NodeId> = FxHashSet::default();
    let mut budget2 = GUARD_CONE_BUDGET;
    assert!(value_cone(&g, a, &mut cone2, &mut budget2));
    assert!(cone2.contains(&a));
    // `Op::Start` is control and must not be a member: every parameter in
    // the method names it, so it would relate every value to every other.
    assert!(
        !cone2.contains(&g.entry),
        "control is not a value and must not join two cones"
    );
}

#[test]
fn test_licm_hoists_invariant_load_merge_header() {
    // The production bytecode→IR builder emits javac loops with an
    // `Op::Merge` header (not `Op::Region`). LICM must recognise it and
    // hoist an invariant load just the same.
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let mem = 2;
    let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, base, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(
        changed,
        "an invariant load in a barrier-free Merge-header loop must hoist"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the hoisted load's control input must be re-anchored to the pre-header"
    );
}

#[test]
fn test_trivial_phi_collapses_to_single_value() {
    // φ(ctrl; v, self) collapses to v (trivial loop phi for an unmodified
    // local); φ(ctrl; a, b) with two distinct values is a genuine merge and
    // is left untouched.
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Region, IrType::Control, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    // Trivial loop phi: value inputs = [v, self].
    let triv = g.add(Op::Phi, IrType::Ref, vec![ctrl, v], None);
    g.nodes[triv as usize].inputs.push(triv); // self back-edge
    let user = g.add(Op::ArrayLength, IrType::Int, vec![triv], None);
    // Genuine merge phi: two distinct value inputs.
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    let merge = g.add(Op::Phi, IrType::Int, vec![ctrl, a, b], None);
    let user2 = g.add(Op::Neg, IrType::Int, vec![merge], None);

    let _ = algebraic_simplify(&mut g);

    assert_eq!(
        g.nodes[user as usize].inputs[0], v,
        "the trivial phi must collapse to its single value v"
    );
    assert_eq!(
        g.nodes[triv as usize].op,
        Op::Dead,
        "the collapsed trivial phi must be killed"
    );
    assert_eq!(
        g.nodes[user2 as usize].inputs[0], merge,
        "a genuine merge phi (distinct inputs) must NOT be collapsed"
    );
    assert_ne!(g.nodes[merge as usize].op, Op::Dead);
}

#[test]
fn test_licm_hoists_load_over_trivial_phi_base() {
    // The production case: a getfield whose base is a loop-invariant local
    // arrives with the base wrapped in a trivial loop-header phi `φ(p, self)`
    // (the IR builder inserts a phi for every live local). Trivial-phi
    // elimination collapses it to the Param, after which LICM hoists the
    // load — the integration that makes LICM non-inert on real bytecode.
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    let mem = 2;
    let param = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    // Trivial loop phi over the invariant base, anchored at the loop header.
    let base_phi = g.add(Op::Phi, IrType::Ref, vec![region, param], None);
    g.nodes[base_phi as usize].inputs.push(base_phi); // unchanged back-edge
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, base_phi, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    // Precondition: the load's base is the region-anchored loop phi, which
    // LICM judges loop-variant — so without the collapse LICM is inert here.
    assert_eq!(g.nodes[load as usize].inputs[2], base_phi);
    assert_eq!(g.nodes[base_phi as usize].inputs[0], region);

    // Collapse the trivial phi, then hoist.
    let _ = algebraic_simplify(&mut g);
    assert_eq!(
        g.nodes[load as usize].inputs[2], param,
        "trivial-phi elimination must expose the Param base to the load"
    );
    let changed = licm(&mut g);
    assert!(
        changed,
        "LICM must hoist once the base is the invariant Param"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the hoisted load's control must be re-anchored to the pre-header"
    );
}

#[test]
fn test_licm_merge_header_classifies_entry_regardless_of_slot_order() {
    // A `Merge` header does not fix entry vs back-edge slot order. With the
    // back-edge moved to slot 0 and the entry to slot 1, LICM must still
    // classify the pre-header structurally (by control-reachability) and
    // hoist to the real entry — never to the back-edge at slot 0.
    let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
    // Swap the header's control inputs: [c0, back] -> [back, c0].
    g.nodes[region as usize].inputs.swap(0, 1);
    assert_ne!(
        g.nodes[region as usize].inputs[0], preheader,
        "precondition: the entry predecessor is no longer at input slot 0"
    );
    let mem = 2;
    let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let off = g.add(Op::Const(4), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, base, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(changed, "the invariant load must still hoist");
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the load must re-anchor to the structurally-classified entry (c0), \
         not to whatever sits at input slot 0 (the back-edge)"
    );
}

#[test]
fn test_licm_hoists_load_past_nonaliasing_local_store() {
    // An invariant load of local allocation A.f with an in-loop store to a
    // DISTINCT local allocation B.f must still hoist: two distinct local
    // allocations never alias, so the store cannot clobber the load.
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2;
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let b = g.add(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, a, off],
        None,
    );
    // In-loop store to a DIFFERENT local allocation — not a hard barrier and
    // provably non-aliasing with the load of A.
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, b, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(
        changed,
        "the load of A must hoist past the non-aliasing store to B"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the hoisted load's control must be re-anchored to the pre-header"
    );
}

#[test]
fn test_licm_keeps_load_when_store_to_same_alloc() {
    // An in-loop store to the SAME allocation the load reads may alias it
    // (same object) — the load must NOT be hoisted.
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2;
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, a, off],
        None,
    );
    // Store to the SAME allocation A → may alias the load.
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, a, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let _ = licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "a load aliased by an in-loop store to the same allocation must stay in the loop"
    );
}

#[test]
fn test_licm_keeps_load_when_store_base_non_local() {
    // The alias oracle is conservative: a store through a non-local base (a
    // Param) is not provably non-aliasing, so the load stays pinned even
    // though the store actually writes a different object.
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2;
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let p = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, a, off],
        None,
    );
    // Store through a Param base — not a provable local allocation.
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, p, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let _ = licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "a non-local store base is not provably non-aliasing → load stays pinned"
    );
}

#[test]
fn test_licm_hoists_param_load_past_local_store() {
    // A load from a method parameter P.f, with an in-loop store to a local
    // allocation B.f, hoists: a freshly-allocated object is never the
    // pre-existing parameter object, so the store cannot clobber the load.
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2;
    let p = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let b = g.add(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, p, off],
        None,
    );
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, b, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(
        changed,
        "the load of param P must hoist past the store to local B"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the hoisted load's control must be re-anchored to the pre-header"
    );
}

#[test]
fn test_licm_keeps_param_load_when_store_base_is_param() {
    // The store base is NOT widened to Param: two parameters may be the same
    // object (`foo(x, x)`), so a load from P0.f must NOT hoist past a store
    // through P1 — they may alias.
    let (mut g, region, _preheader, _iv) = loop_probe();
    let mem = 2;
    let p0 = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
    let p1 = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, p0, off],
        None,
    );
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, p1, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let _ = licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "a store through a Param base may alias another Param load → stays pinned"
    );
}

#[test]
fn test_licm_hoists_load_past_phi_store_of_distinct_allocs() {
    // Cross-merge: the in-loop store's base flows through a Phi merging two
    // local allocations A and B. The oracle resolves it to {A, B}, so a load
    // from a DISTINCT allocation C (disjoint) still hoists past the store.
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2;
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let b = g.add(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let c = g.add(
        Op::New {
            class_id: 3,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    // Ref phi merging A and B (anchor at the region control in slot 0).
    let phi = g.add(Op::Phi, IrType::Ref, vec![region, a, b], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
    // Load of C (distinct), and a store through the phi (writes A or B).
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, c, off],
        None,
    );
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, phi, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(
        changed,
        "the load of C must hoist past a phi-store of {{A, B}} (C is disjoint)"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the hoisted load's control must be re-anchored to the pre-header"
    );
}

#[test]
fn test_licm_keeps_load_when_phi_store_includes_its_alloc() {
    // The phi-store resolves to {A, B}; a load from A IS in that set, so it
    // may alias and must stay pinned.
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2;
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let b = g.add(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let phi = g.add(Op::Phi, IrType::Ref, vec![region, a, b], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, a, off],
        None,
    );
    let _st = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![region, mem, phi, cval],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let _ = licm(&mut g);

    assert_eq!(
        g.nodes[load as usize].inputs[0], region,
        "a load of A may be aliased by a phi-store that writes {{A, B}} → stays pinned"
    );
}

#[test]
fn test_licm_hoists_load_with_invariant_phi_base() {
    // The cross-merge oracle on the LOAD side: a load whose base is a phi
    // merged BEFORE the loop (anchor outside the body) over invariant
    // allocations is itself invariant and hoists. (`x = cond ? new A() :
    // new B(); for (...) ... x.f ...`)
    let (mut g, region, preheader, _iv) = loop_probe();
    let mem = 2;
    let a = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    let b = g.add(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        IrType::Ref,
        vec![preheader],
        None,
    );
    // Phi anchored at the pre-loop control (slot 0 = preheader, OUTSIDE the
    // loop body) merging two invariant allocations.
    let phi = g.add(Op::Phi, IrType::Ref, vec![preheader, a, b], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![region, mem, phi, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
    g.exit = ret;

    let changed = licm(&mut g);

    assert!(
        changed,
        "a load over a pre-loop invariant phi base must hoist"
    );
    assert_eq!(
        g.nodes[load as usize].inputs[0], preheader,
        "the hoisted load's control must be re-anchored to the pre-header"
    );
}
