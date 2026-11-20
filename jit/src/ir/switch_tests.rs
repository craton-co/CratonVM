use super::*;

/// `int f(int x) { switch (x) { … } }` as a `tableswitch` over the slots
/// `low ..= low + arms.len() - 1`.
///
/// `arms[i]` is `Some(v)` for a slot with its own arm returning `v`, and
/// `None` for a slot wired to the DEFAULT target — which is how javac pads
/// a `tableswitch` whose cases have holes, and the shape the trimming and
/// the live-case count are about. The default arm returns 99.
///
/// Layout: `iload_0` at 0, `tableswitch` at 1, JVMS padding to the next
/// multiple of 4 (so the operands start at 4), then `default`, `low`,
/// `high` and one offset per slot, then the arms as `bipush v; ireturn`.
fn tableswitch_body(low: i32, arms: &[Option<i8>]) -> Vec<u8> {
    let n = arms.len();
    let arm_base = 16 + 4 * n;
    let live: Vec<usize> = (0..n).filter(|&i| arms[i].is_some()).collect();
    let default_at = arm_base + 3 * live.len();
    let mut code = vec![0x1a, 0xaa, 0x00, 0x00];
    code.extend_from_slice(&((default_at as i32) - 1).to_be_bytes());
    code.extend_from_slice(&low.to_be_bytes());
    code.extend_from_slice(&(low + n as i32 - 1).to_be_bytes());
    for i in 0..n {
        let at = match live.iter().position(|&l| l == i) {
            Some(j) => arm_base + 3 * j,
            None => default_at,
        };
        code.extend_from_slice(&((at as i32) - 1).to_be_bytes());
    }
    for &i in &live {
        let v = arms[i].unwrap_or(0);
        code.extend_from_slice(&[0x10, v as u8, 0xac]);
    }
    code.extend_from_slice(&[0x10, 99, 0xac]);
    code
}

/// The same, as a `lookupswitch` over `keys` in the order given — which is
/// NOT necessarily ascending, because one of the things under test is what
/// happens when it is not.
fn lookupswitch_body(keys: &[i32]) -> Vec<u8> {
    let n = keys.len();
    let arm_base = 12 + 8 * n;
    let default_at = arm_base + 3 * n;
    let mut code = vec![0x1a, 0xab, 0x00, 0x00];
    code.extend_from_slice(&((default_at as i32) - 1).to_be_bytes());
    code.extend_from_slice(&(n as i32).to_be_bytes());
    for (i, &key) in keys.iter().enumerate() {
        code.extend_from_slice(&key.to_be_bytes());
        code.extend_from_slice(&(((arm_base + 3 * i) as i32) - 1).to_be_bytes());
    }
    for i in 0..n {
        code.extend_from_slice(&[0x10, 10 + i as u8, 0xac]);
    }
    code.extend_from_slice(&[0x10, 99, 0xac]);
    code
}

fn build_graph(code: &[u8]) -> Graph {
    let len = code.len();
    // Four trailing zero bytes: the strict decoders read fixed-width
    // operands, and a fixture that ended exactly at the last instruction
    // would be refused for running off the buffer rather than for the
    // reason the test is about.
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0, 0, 0]);
    match IrBuilder::new(1, 1).build(&padded, len) {
        Some(g) => g,
        None => panic!("the fixture did not build"),
    }
}

fn switch_bounds(g: &Graph) -> Vec<(i32, i32)> {
    g.nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::Switch { low, high } => Some((low, high)),
            _ => None,
        })
        .collect()
}

fn projections_of(g: &Graph, producer: NodeId) -> Vec<usize> {
    let mut out: Vec<usize> = g
        .nodes
        .iter()
        .filter(|n| n.inputs.first().copied() == Some(producer))
        .filter_map(|n| match n.op {
            Op::Proj(k) => Some(k as usize),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out
}

fn the_switch(g: &Graph) -> NodeId {
    match g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Switch { .. }))
    {
        Some(i) => i as NodeId,
        None => panic!("no Op::Switch in this graph"),
    }
}

// ── The density rules ────────────────────────────────────────────

/// The reason both predicates compute the span in `i64`, as a test rather
/// than only as a comment.
///
/// `tableswitch low=i32::MIN high=i32::MAX` is legal bytecode. In `i32`,
/// `high - low + 1` wraps to **0** — which reads as "span zero, perfectly
/// dense" and would ask `ir_lower` for a jump table with 2^32 entries.
#[test]
fn a_full_int_range_switch_does_not_look_dense_through_an_overflowed_span() {
    assert_eq!(
        i32::MAX.wrapping_sub(i32::MIN).wrapping_add(1),
        0,
        "the i32 computation both predicates are avoiding"
    );
    assert!(!switch_is_dense(i32::MIN, i32::MAX, 2));
    assert!(!switch_is_representable(i32::MIN, i32::MAX, 2));
    // Claiming every key is a live case does not buy it a table either:
    // the projection budget is tested first.
    assert!(!switch_is_dense(i32::MIN, i32::MAX, usize::MAX));
    assert!(!switch_is_representable(i32::MIN, i32::MAX, usize::MAX));
    // And the arity the verifier computes is enormous rather than small,
    // so a `Proj` of such a node fails the projection lane instead of
    // passing a plausible bound.
    assert_eq!(
        switch_projection_count(i32::MIN, i32::MAX),
        u32::MAX as usize
    );
}

#[test]
fn a_contiguous_case_set_is_dense_and_a_scattered_one_is_only_representable() {
    // Eight contiguous keys: no waste at all.
    assert!(switch_is_dense(0, 7, 8));
    // Every third key live — three slots per case. Past the table rule,
    // inside the builder's admission: the band the binary search exists
    // for.
    assert!(!switch_is_dense(0, 20, 7));
    assert!(switch_is_representable(0, 20, 7));
    // Two keys a thousand apart: neither.
    assert!(!switch_is_dense(1, 1000, 2));
    assert!(!switch_is_representable(1, 1000, 2));
}

/// A shape the lowerer would put in a table but the builder would never
/// build is a dead arm; a shape the builder builds and the lowerer has no
/// rule for is a wrong answer. Neither is possible while this holds.
#[test]
fn every_table_dense_switch_is_one_the_builder_may_build() {
    for low in [-70i32, -1, 0, 1, 1000] {
        for span in 1i64..=300 {
            let Some(high) = i64::from(low).checked_add(span - 1) else {
                continue;
            };
            let Ok(high) = i32::try_from(high) else {
                continue;
            };
            for cases in [1usize, 2, 3, 8, 64, 255] {
                if switch_is_dense(low, high, cases) {
                    assert!(
                        switch_is_representable(low, high, cases),
                        "dense but not representable: [{low}, {high}] with {cases} cases"
                    );
                }
            }
        }
    }
}

/// The projection budget fits `Op::Proj`'s index, and the span and the
/// budget are the same fact expressed twice.
///
/// Before 2026-09-17 this was `the_projection_budget_is_the_width_of_a_proj_
/// index` and asserted EQUALITY with `u8::MAX + 1`: the budget was the width.
/// It is now a budget inside a `u16`, so what is pinned is that it fits --
/// the `const` assertion beside the constant already makes an overflow a
/// compile error; this states the arithmetic around it.
#[test]
fn the_projection_budget_fits_the_width_of_a_proj_index() {
    assert!(SWITCH_MAX_PROJECTIONS <= u16::MAX as usize + 1);
    assert_eq!(SWITCH_MAX_SPAN, SWITCH_MAX_PROJECTIONS as i64 - 1);
    let span = SWITCH_MAX_SPAN as i32;
    let slots = SWITCH_MAX_SPAN as usize;
    // The widest admissible switch spends its last index on the default.
    assert!(switch_is_dense(0, span - 1, slots));
    assert_eq!(switch_projection_count(0, span - 1), SWITCH_MAX_PROJECTIONS);
    assert!(!switch_is_dense(0, span, slots + 1));
    assert!(!switch_is_representable(0, span, slots + 1));
}

/// The projection cap REFUSES; it does not truncate. End to end, through
/// the builder, on real `tableswitch` bytecode.
///
/// # Why this test and not only the predicate asserts above
///
/// The only thing standing between a switch one slot past the budget and
/// `Op::Proj(k as u16)` naming a projection the switch does not have is
/// `dense_switch_bounds`'s call to [`switch_is_representable`]. Covered only
/// by asserts on the predicate itself, a regression in `dense_switch_bounds`
/// (dropping the call, or moving it after the bounds are returned) would be
/// invisible to the builder tests.
///
/// Both arms matter and they differ by exactly one slot, derived from
/// [`SWITCH_MAX_PROJECTIONS`] rather than written as numbers -- this test
/// used to hard-code 255 and 256, which were the `u8` width, and a budget
/// change would have left it testing the old boundary:
///
/// * `SWITCH_MAX_SPAN` slots, all live -- the widest admissible switch.
/// * one more -- must keep the `Cmp`/`If` chain, whose projections are only
///   ever `Proj(0)` and `Proj(1)`. The method still compiles; the refusal is
///   a choice of lowering, not a bail.
#[test]
fn a_switch_one_slot_past_the_projection_budget_keeps_the_compare_chain() {
    let span = SWITCH_MAX_SPAN as usize;
    // The widest switch the builder may build.
    let widest: Vec<Option<i8>> = (0..span).map(|i| Some((i % 100) as i8)).collect();
    let g = build_graph(&tableswitch_body(0, &widest));
    assert_eq!(switch_bounds(&g), vec![(0, span as i32 - 1)]);
    let projs = projections_of(&g, the_switch(&g));
    assert_eq!(
        projs.len(),
        SWITCH_MAX_PROJECTIONS,
        "every case edge plus the default"
    );
    assert_eq!(
        projs.last().copied(),
        Some(SWITCH_MAX_PROJECTIONS - 1),
        "the default edge is the last projection the budget allows"
    );

    // One past. No `Op::Switch` at all.
    let one_past: Vec<Option<i8>> = (0..=span).map(|i| Some((i % 100) as i8)).collect();
    let g = build_graph(&tableswitch_body(0, &one_past));
    assert!(
        switch_bounds(&g).is_empty(),
        "a switch one slot past the budget must be REFUSED the dense form"
    );
    assert!(
        g.nodes.iter().any(|n| matches!(n.op, Op::Cmp(_))),
        "…and refused into the compare chain, which is what still compiles it"
    );
    // The decisive assertion: no projection index above 1 exists anywhere,
    // so nothing was produced by an `as u16` past the budget.
    let widest_proj = g
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::Proj(k) => Some(k),
            _ => None,
        })
        .max();
    assert!(
        matches!(widest_proj, Some(0) | Some(1)),
        "the chain's projections are an If's two edges, got {widest_proj:?}"
    );
}

/// **The optimisation, proven.** A switch of 256 dense cases -- one past the
/// old `u8` ceiling -- now builds an `Op::Switch`, where until 2026-09-17 it
/// was refused into a 256-case compare chain.
///
/// Without this, widening `Op::Proj` could be a pure no-op (the budget left
/// at 256) and every other test here would still pass. This is the one that
/// fails if the widening did not actually buy anything.
#[test]
fn a_switch_past_the_old_u8_ceiling_now_gets_a_switch_node() {
    assert!(
        SWITCH_MAX_PROJECTIONS > u8::MAX as usize + 1,
        "the budget must exceed the old u8 width, or this item bought nothing"
    );
    let old_one_past: Vec<Option<i8>> = (0..256).map(|i| Some((i % 100) as i8)).collect();
    let g = build_graph(&tableswitch_body(0, &old_one_past));
    assert_eq!(
        switch_bounds(&g),
        vec![(0, 255)],
        "256 dense cases are within the budget now, and get a switch node"
    );
    let projs = projections_of(&g, the_switch(&g));
    assert_eq!(projs.len(), 257, "256 case edges plus the default");
    assert_eq!(
        projs.last().copied(),
        Some(256),
        "projection 256 is namable now -- it was the first index a u8 could not hold"
    );
}

// ── The builder ──────────────────────────────────────────────────

#[test]
fn a_dense_tableswitch_builds_one_switch_node_with_a_projection_per_slot() {
    let g = build_graph(&tableswitch_body(0, &[Some(10), Some(20), Some(30)]));
    assert_eq!(switch_bounds(&g), vec![(0, 2)]);
    // Three cases and the default: `Proj(0..=2)` and `Proj(3)`.
    assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2, 3]);
    // And the five-nodes-per-case chain is gone rather than joined: there
    // is no equality compare against the key left in the graph.
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::Cmp(_))),
        "the dense form replaces the compare chain"
    );
}

/// A `lookupswitch` whose keys are negative normalises like any other: the
/// key range is `[-3, -1]` and nothing about it is a special case.
#[test]
fn a_lookupswitch_over_negative_keys_normalises_into_the_dense_form() {
    let g = build_graph(&lookupswitch_body(&[-3, -2, -1]));
    assert_eq!(switch_bounds(&g), vec![(-3, -1)]);
    assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2, 3]);
}

/// The trimming: javac's leading padding slots end up OUTSIDE the range
/// rather than as projections inside it, because a key outside
/// `[low, high]` already goes where those slots were going.
#[test]
fn a_tableswitch_padded_with_default_slots_is_trimmed_to_its_live_range() {
    // Slots 0, 1 and 2 are default padding; 3 and 4 are the real cases.
    let g = build_graph(&tableswitch_body(
        0,
        &[None, None, None, Some(30), Some(40)],
    ));
    assert_eq!(switch_bounds(&g), vec![(3, 4)]);
    assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2]);
}

/// A hole INSIDE the live range is not trimmed — it is a projection wired
/// to the default target, which is what keeps the dense index space total.
#[test]
fn a_hole_inside_the_live_range_keeps_its_own_projection() {
    let g = build_graph(&tableswitch_body(1, &[Some(10), None, Some(30)]));
    assert_eq!(switch_bounds(&g), vec![(1, 3)]);
    assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2, 3]);
}

/// Too sparse for the dense form to be worth its projections: the chain
/// stays, and it stays CORRECT — which is the whole reason the floor is
/// kept reachable rather than deleted.
#[test]
fn a_sparse_lookupswitch_keeps_the_compare_chain() {
    let g = build_graph(&lookupswitch_body(&[1, 1000]));
    assert!(switch_bounds(&g).is_empty());
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Cmp(CmpOp::Eq)))
            .count(),
        2,
        "one equality compare per listed key"
    );
}

/// JVMS §6.5 requires a `lookupswitch`'s pairs to be sorted and distinct.
/// Nothing on this path verifies it, and a duplicate key would want two
/// projections for one slot of the dense form — so an unsorted payload
/// keeps the chain, which compares against every listed key in table order
/// whatever the payload contains.
#[test]
fn an_unsorted_or_duplicated_lookupswitch_keeps_the_compare_chain() {
    assert!(switch_bounds(&build_graph(&lookupswitch_body(&[5, 1, 3]))).is_empty());
    assert!(switch_bounds(&build_graph(&lookupswitch_body(&[2, 2, 3]))).is_empty());
}

/// JVMS gives a switch one edge per listed key and one default edge, and
/// says nothing about them being distinct targets. Three cases that all
/// return the same value still have three edges.
#[test]
fn every_case_edge_and_the_default_edge_survive_the_dense_form() {
    let g = build_graph(&tableswitch_body(0, &[Some(10), Some(10), Some(10)]));
    let switch_node = the_switch(&g);
    assert_eq!(projections_of(&g, switch_node), vec![0, 1, 2, 3]);
    let edges = g
        .nodes
        .iter()
        .filter(|n| n.inputs.first().copied() == Some(switch_node))
        .count();
    assert_eq!(
        edges, 4,
        "three cases and the default, every one of them its own edge"
    );
}

/// Every slot pointing at the default is an instruction that always takes
/// the default: there is nothing for a multi-way node to dispatch on, and
/// the chain's trailing jump is already the whole instruction.
#[test]
fn a_tableswitch_whose_every_slot_is_the_default_keeps_the_chain() {
    let g = build_graph(&tableswitch_body(0, &[None, None, None]));
    assert!(switch_bounds(&g).is_empty());
}

// ── The sparse search tree (round 12 wave 2, lane irbuild) ────────

/// Follow the switch's compare tree for `key` (the method's `Param(0)`)
/// from the control token the first `If` hangs off, and answer the
/// bytecode pc of the `Merge` the chosen edge enters. Every compare the
/// walk meets must be `Cmp(Eq|Lt)` of the key against a constant.
fn dispatch(g: &Graph, key: i32) -> usize {
    let Some(first_if) = g.nodes.iter().position(|n| n.op == Op::If) else {
        panic!("no If in this graph");
    };
    let mut cur = g.nodes[first_if].inputs[0];
    for _ in 0..g.nodes.len() {
        let Some(if_id) = g
            .nodes
            .iter()
            .position(|n| n.op == Op::If && n.inputs.first().copied() == Some(cur))
        else {
            break;
        };
        let cmp = &g.nodes[g.nodes[if_id].inputs[1] as usize];
        assert!(
            matches!(g.nodes[cmp.inputs[0] as usize].op, Op::Param(0)),
            "the tree compares the switch key"
        );
        let Op::Const(c) = g.nodes[cmp.inputs[1] as usize].op else {
            panic!("the tree compares against a constant");
        };
        let taken = match cmp.op {
            Op::Cmp(CmpOp::Eq) => i64::from(key) == c,
            Op::Cmp(CmpOp::Lt) => i64::from(key) < c,
            ref other => panic!("unexpected compare {other:?}"),
        };
        let want = if taken { 0 } else { 1 };
        let Some(next) = g.nodes.iter().position(|n| {
            n.op == Op::Proj(want) && n.inputs.first().copied() == Some(if_id as NodeId)
        }) else {
            panic!("an If without its projection {want}");
        };
        cur = next as NodeId;
    }
    let Some(merge) = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Merge && n.inputs.iter().any(|&i| i == cur))
    else {
        panic!("edge {cur} enters no merge");
    };
    merge.bytecode_pc.unwrap_or(usize::MAX)
}

/// A `lookupswitch` over ten scattered keys — the shape of every `String`
/// switch — is refused the dense form and was a ten-compare chain. It is
/// now a tree of `key < pivot` tests over equality leaves, and every key,
/// every gap between keys and both extremes of `int` still reach exactly
/// the edge the JVMS gives them.
#[test]
fn a_large_sparse_lookupswitch_dispatches_through_a_search_tree() {
    let keys = [
        i32::MIN,
        -1_000_000,
        -5000,
        -7,
        0,
        3,
        100,
        9999,
        40_000,
        1 << 20,
        i32::MAX,
    ];
    let g = build_graph(&lookupswitch_body(&keys));
    assert!(switch_bounds(&g).is_empty(), "too sparse for Op::Switch");
    let lt = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Cmp(CmpOp::Lt)))
        .count();
    let eq = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Cmp(CmpOp::Eq)))
        .count();
    assert!(lt >= 1, "a search tree splits on at least one pivot");
    assert_eq!(
        eq,
        keys.len(),
        "each live key is tested for equality exactly once"
    );
    let n = keys.len();
    let arm_base = 12 + 8 * n;
    let default_at = arm_base + 3 * n;
    for (i, &k) in keys.iter().enumerate() {
        assert_eq!(dispatch(&g, k), arm_base + 3 * i, "key {k}");
    }
    for k in [
        i32::MIN + 1,
        -1_000_001,
        -6,
        -1,
        1,
        4,
        99,
        101,
        10_000,
        (1 << 20) + 1,
        i32::MAX - 1,
    ] {
        assert_eq!(dispatch(&g, k), default_at, "non-key {k}");
    }
}

/// A sparse `tableswitch` whose padding slots point at the default: those
/// slots are dropped from the tree (their key reaches the default through
/// the leaves' fall-through), and the live ones still dispatch exactly.
#[test]
fn a_sparse_tableswitch_drops_its_default_slots_from_the_tree() {
    let arms: Vec<Option<i8>> = (0..100)
        .map(|i| if i % 12 == 0 { Some(i as i8) } else { None })
        .collect();
    let g = build_graph(&tableswitch_body(0, &arms));
    assert!(switch_bounds(&g).is_empty(), "9 live keys over 100 slots");
    let eq = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Cmp(CmpOp::Eq)))
        .count();
    assert_eq!(eq, 9, "only the live keys are compared");
    let arm_base = 16 + 4 * arms.len();
    let live: Vec<usize> = (0..arms.len()).filter(|&i| arms[i].is_some()).collect();
    let default_at = arm_base + 3 * live.len();
    for k in -3..103 {
        let want = match usize::try_from(k)
            .ok()
            .and_then(|k| live.iter().position(|&l| l == k))
        {
            Some(j) => arm_base + 3 * j,
            None => default_at,
        };
        assert_eq!(dispatch(&g, k), want, "key {k}");
    }
}

/// Below the tree's threshold, and for a payload that is not strictly
/// ascending, the historical chain is kept compare for compare.
#[test]
fn small_or_unsorted_sparse_switches_keep_the_chain() {
    let g = build_graph(&lookupswitch_body(&[1, 1000, 5000, 90_000]));
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Cmp(CmpOp::Lt))));
    let unsorted = [9000, 1, 70_000, 5, 123_456, -40, 800, 6000, 2_000_000];
    let g = build_graph(&lookupswitch_body(&unsorted));
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Cmp(CmpOp::Lt))));
    // First match wins on the chain, whatever the order.
    let arm_base = 12 + 8 * unsorted.len();
    for (i, &k) in unsorted.iter().enumerate() {
        assert_eq!(dispatch(&g, k), arm_base + 3 * i, "key {k}");
    }
}
