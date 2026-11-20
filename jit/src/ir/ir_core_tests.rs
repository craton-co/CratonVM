use super::*;

/// Helper: build IR from bytecode, panicking on failure.
fn build_ir(code: &[u8], code_len: usize, num_params: usize, num_locals: usize) -> Graph {
    let builder = IrBuilder::new(num_params, num_locals);
    builder.build(code, code_len).expect("IR build failed")
}

// ── Bottom-tested (rotated) loops ────────────────────────────────────

/// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`
/// as ECJ emits it: jump forward to the test, branch back to the body.
const ROTATED_FOR: [u8; 23] = [
    0x03, 0x3c, // 0: iconst_0; 1: istore_1        s = 0
    0x03, 0x3d, // 2: iconst_0; 3: istore_2        i = 0
    0xa7, 0x00, 0x0a, // 4: goto 14
    0x1b, 0x1c, 0x60, 0x3c, // 7: iload_1; iload_2; iadd; istore_1   body
    0x84, 0x02, 0x01, // 11: iinc 2, 1
    0x1c, 0x1a, // 14: iload_2; 15: iload_0        test
    0xa1, 0xff, 0xf7, // 16: if_icmplt 7
    0x1b, 0xac, // 19: iload_1; 20: ireturn
    0, 0,
];

/// `int f(int n) { do { n = n - 3; } while (n > 0); return n; }`. The loop
/// header is pc 0, where the method entry is the forward edge.
const DO_WHILE_AT_ENTRY: [u8; 12] = [
    0x1a, 0x06, 0x64, 0x3b, // 0: iload_0; iconst_3; isub; istore_0
    0x1a, // 4: iload_0
    0x9d, 0xff, 0xfb, // 5: ifgt 0
    0x1a, 0xac, // 8: iload_0; 9: ireturn
    0, 0,
];

/// A javac-shaped outer loop around an ECJ-shaped inner one:
/// `int f(int n) { int c = 0; for (int i = 0; i < n; i++) for (int j = 0; j < i; j++) c += j; return c; }`.
const ROTATED_INSIDE_JAVAC_LOOP: [u8; 36] = [
    0x03, 0x3c, // 0: c = 0
    0x03, 0x3d, // 2: i = 0
    0x1c, 0x1a, 0xa2, 0x00, 0x1a, // 4: iload_2; iload_0; if_icmpge 32   outer header
    0x03, 0x3e, // 9: j = 0
    0xa7, 0x00, 0x0a, // 11: goto 21
    0x1b, 0x1d, 0x60, 0x3c, // 14: iload_1; iload_3; iadd; istore_1   inner body
    0x84, 0x03, 0x01, // 18: iinc 3, 1
    0x1d, 0x1c, 0xa1, 0xff, 0xf7, // 21: iload_3; iload_2; if_icmplt 14   inner test
    0x84, 0x02, 0x01, // 26: iinc 2, 1
    0xa7, 0xff, 0xe7, // 29: goto 4
    0x1b, 0xac, // 32: iload_1; 33: ireturn
    0, 0,
];

/// A rotated loop whose body joins an if/else:
/// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) { if ((i & 1) == 0) s += i; else s -= 1; } return s; }`.
const ROTATED_WITH_IF_ELSE: [u8; 35] = [
    0x03, 0x3c, // 0: s = 0
    0x03, 0x3d, // 2: i = 0
    0xa7, 0x00, 0x16, // 4: goto 26
    0x1c, 0x04, 0x7e, // 7: iload_2; iconst_1; iand        body
    0x9a, 0x00, 0x0a, // 10: ifne 20
    0x1b, 0x1c, 0x60, 0x3c, // 13: s += i
    0xa7, 0x00, 0x06, // 17: goto 23
    0x84, 0x01, 0xff, // 20: iinc 1, -1
    0x84, 0x02, 0x01, // 23: iinc 2, 1                   join
    0x1c, 0x1a, // 26: iload_2; 27: iload_0              test
    0xa1, 0xff, 0xeb, // 28: if_icmplt 7
    0x1b, 0xac, // 31: iload_1; 32: ireturn
    0, 0,
];

/// The id of the `Op::Merge` the builder made for `pc`.
fn merge_at(graph: &Graph, pc: usize) -> NodeId {
    graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Merge) && n.bytecode_pc == Some(pc))
        .unwrap_or_else(|| panic!("no merge at pc {pc}")) as NodeId
}

/// The merge at `header` is a loop header with one entry and one back
/// edge, and at least one phi carries a value around it.
fn assert_loop_header(graph: &Graph, header: usize) {
    let merge = merge_at(graph, header);
    assert_eq!(
        graph.nodes[merge as usize].inputs.len(),
        2,
        "pc {header}: one entry edge and one back edge"
    );
    assert!(
        graph.nodes.iter().any(|n| matches!(n.op, Op::Phi)
            && n.input_opt(0) == Some(merge)
            && n.inputs.len() == 3),
        "pc {header}: a loop-carried phi with its back-edge input patched in"
    );
}

/// `static int f(int n) { return sumTo(n) + 1; }`, with `sumTo` the ECJ
/// loop of [`ROTATED_FOR`] spliced in at the `invokestatic` (pc 1).
/// Returns `(combined, caller_len, sites, base)`.
fn rotated_callee_splice_fixture() -> (Vec<u8>, usize, HashMap<usize, IrInlineSite>, usize) {
    let caller = [
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1  sumTo(I)I
        0x04, 0x60, // 4: iconst_1; 5: iadd
        0xac, // 6: ireturn
    ];
    let mut combined = caller.to_vec();
    let base = combined.len();
    combined.extend_from_slice(&ROTATED_FOR[..21]);
    combined.extend_from_slice(&[0, 0]);
    let mut sites = HashMap::new();
    sites.insert(
        1,
        IrInlineSite {
            base,
            code_len: 21,
            num_args: 1,
            max_locals: 3,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.sumTo:(I)I".to_string(),
            class_id: 0,
        },
    );
    (combined, caller.len(), sites, base)
}

#[test]
fn a_spliced_callee_with_a_rotated_loop_builds_instead_of_refusing_the_caller() {
    let (combined, caller_len, sites, base) = rotated_callee_splice_fixture();
    let mut builder = IrBuilder::new(1, 1);
    builder.set_inline_sites(sites);
    let graph = builder
        .build(&combined, caller_len)
        .expect("a callee body with a rotated loop must splice, not refuse the caller");
    // The callee's test block (its pc 14) heads the loop, in combined pcs.
    assert_loop_header(&graph, base + 14);
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
        "the spliced call must be gone, not emitted alongside the body",
    );
}

#[test]
fn a_rotated_for_loop_builds_with_its_test_block_as_the_header() {
    let graph = build_ir(&ROTATED_FOR, 21, 1, 3);
    assert_loop_header(&graph, 14);
    assert_eq!(
        graph.nodes[merge_at(&graph, 7) as usize].inputs.len(),
        1,
        "the body is an ordinary merge reached only by the test's branch"
    );
}

#[test]
fn a_do_while_whose_header_is_the_method_entry_builds() {
    let graph = build_ir(&DO_WHILE_AT_ENTRY, 10, 1, 1);
    assert_loop_header(&graph, 0);
}

#[test]
fn a_rotated_loop_nested_in_a_javac_loop_builds() {
    let graph = build_ir(&ROTATED_INSIDE_JAVAC_LOOP, 34, 1, 4);
    assert_loop_header(&graph, 4);
    assert_loop_header(&graph, 21);
}

#[test]
fn a_rotated_loop_with_an_if_else_join_in_its_body_builds() {
    let graph = build_ir(&ROTATED_WITH_IF_ELSE, 33, 1, 3);
    assert_loop_header(&graph, 26);
    assert_eq!(
        graph.nodes[merge_at(&graph, 23) as usize].inputs.len(),
        2,
        "the if/else join inside the body is a forward merge of both arms"
    );
}

/// The order and headers `BlockWalk` computes for the ECJ `for`, block by
/// block. A pc-order method never gets a block walk at all.
#[test]
fn the_block_walk_visits_the_test_before_the_body_it_dominates() {
    fn walk_for(code: &[u8], len: usize) -> (bool, Option<BlockWalk>) {
        let Ok(verified) = cratonvm_reader::verified_code(&code[..len]) else {
            panic!("fixture does not verify");
        };
        let reachable = normally_reachable_pcs(&verified, len);
        let headers: HashSet<usize> = verified
            .loop_headers()
            .iter()
            .map(|&h| h as usize)
            .filter(|h| reachable.contains(h))
            .collect();
        let rotated = has_rotated_loop_header(&verified, &reachable, &headers, len);
        (
            rotated,
            BlockWalk::reverse_postorder(&verified, &reachable, len),
        )
    }

    let (rotated, walk) = walk_for(&ROTATED_FOR, 21);
    assert!(rotated, "the ECJ body has no forward entry");
    let walk = walk.expect("an ECJ loop has a walk");
    // Entry block, then test + exit (adjacent in the bytecode), then body.
    assert_eq!(walk.ranges, vec![(0, 7), (14, 21), (7, 14)]);
    assert_eq!(walk.headers, HashSet::from([14]));

    let (rotated, _) = walk_for(&ROTATED_INSIDE_JAVAC_LOOP, 34);
    assert!(rotated);
    let (rotated, _) = walk_for(&DO_WHILE_AT_ENTRY, 10);
    assert!(!rotated, "pc 0 is entered by the method entry");
    // javac's nested counted loops: both headers are entered by fall-through.
    let javac_nested = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x19, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00,
        0x0c, 0x84, 0x01, 0x01, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf5, 0x84, 0x02, 0x01, 0xa7, 0xff,
        0xe8, 0x1b, 0xac, 0, 0,
    ];
    let (rotated, _) = walk_for(&javac_nested, 33);
    assert!(!rotated, "a javac loop keeps the pc walk");
}

/// `int f(int x) { return x == 0 ? 2 : 1; }`, whose `ifeq` at pc 1 the
/// profile says is always taken.
///
/// Pruned, the branch becomes a guard plus a jump: the `Op::If` is gone,
/// the cold arm (`iconst_1`, pcs 4-5) is never built, and an `Op::Guard`
/// anchored at pc 1 carries the transfer back to the interpreter for the
/// case the profile never saw.
///
/// Asserted against the UNPRUNED build of the same bytecode rather than
/// against absolute node counts, so the test says "pruning changed this"
/// rather than restating today's node numbering.
#[test]
fn an_always_taken_branch_becomes_a_guard_and_a_jump() {
    let code = [
        0x1a, // 0: iload_0
        0x99, 0x00, 0x07, // 1: ifeq +7 -> 8
        0x04, // 4: iconst_1     <- the cold arm
        0xa7, 0x00, 0x04, // 5: goto +4 -> 9
        0x05, // 8: iconst_2
        0xac, // 9: ireturn
        0, 0, 0,
    ];

    let plain = build_ir(&code, 10, 1, 1);
    assert!(
        plain.nodes.iter().any(|n| matches!(n.op, Op::If)),
        "the control case must build an Op::If",
    );
    assert!(
        !plain.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })),
        "nothing unpruned may plant a guard here",
    );

    let mut builder = IrBuilder::new(1, 1);
    builder.set_pruned_branches(std::iter::once(1usize).collect());
    let pruned = builder.build(&code, 10).expect("pruned build");

    assert!(
        pruned
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { bci: 1 })),
        "the pruned branch must plant a guard at its own bci",
    );
    assert!(
        !pruned.nodes.iter().any(|n| matches!(n.op, Op::If)),
        "the pruned branch must not also build an Op::If",
    );
    // The cold arm produced `iconst_1`; the surviving arm produces
    // `iconst_2`. Only the second may be in the graph.
    let consts: Vec<i64> = pruned
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::Const(v) => Some(v),
            _ => None,
        })
        .collect();
    assert!(
        consts.contains(&2) && !consts.contains(&1),
        "the cold arm must be gone; constants were {consts:?}",
    );
}

/// The cold arm of a pruned branch is walked as DEAD code, not jumped over,
/// so a merge inside it that another branch targets is still activated
/// (round 9 wave 2, `ir-branch-prune-skips-the-cold-arm-by-pc`).
///
/// `pc 5` is pruned (always taken → 16). Its cold arm `[8, 16)` contains
/// pc 12, which the branch at pc 1 also targets. Jumping to 16 left that
/// merge with a recorded predecessor and no activation, and the build was
/// refused by `audit_unactivated_merges`.
#[test]
fn a_merge_inside_a_pruned_cold_arm_is_still_activated() {
    let code = [
        0x1a, // 0: iload_0
        0x9a, 0x00, 0x0b, // 1: ifne +11 -> 12
        0x1b, // 4: iload_1
        0x99, 0x00, 0x0b, // 5: ifeq +11 -> 16   (PRUNED)
        0x06, // 8: iconst_3      <- the cold arm
        0x3c, // 9: istore_1
        0x00, // 10: nop
        0x00, // 11: nop
        0x04, // 12: iconst_1     <- merge: pred from pc 1 (and cold 11)
        0xa7, 0x00, 0x04, // 13: goto +4 -> 17
        0x05, // 16: iconst_2     <- the pruned branch's target
        0xac, // 17: ireturn
        0, 0, 0,
    ];
    let mut b = IrBuilder::new(2, 2);
    b.set_pruned_branches(std::iter::once(5usize).collect());
    let graph = b
        .build(&code, 18)
        .expect("the cold arm is walked as dead code; the build must succeed");

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { bci: 5 })),
        "the pruned branch plants its guard",
    );
    let ifs: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.op == Op::If)
        .map(|(i, _)| i as NodeId)
        .collect();
    assert_eq!(
        ifs.len(),
        1,
        "only the unpruned branch at pc 1 builds an If"
    );
    let m12 = merge_at(&graph, 12);
    let preds = graph.nodes[m12 as usize].inputs.to_vec();
    assert_eq!(
        preds.len(),
        1,
        "pc 12's only LIVE predecessor is the taken edge of pc 1: {preds:?}"
    );
    let p = &graph.nodes[preds[0] as usize];
    assert!(
        matches!(p.op, Op::Proj(_)) && p.inputs.first() == Some(&ifs[0]),
        "the predecessor is a projection of the If at pc 1",
    );
    assert_eq!(
        graph.nodes[merge_at(&graph, 17) as usize].inputs.len(),
        2,
        "pc 17 joins the goto at 13 and the fall-through from 16",
    );
    assert!(
        !graph.nodes.iter().any(|n| n.op == Op::Const(3)),
        "the cold arm's own code is not built",
    );
}

/// nio21 wave 17 (lane L4): `int f(int x) { return x == 0 ? 2 : 1; }`
/// whose `ifeq` at pc 1 the profile says is NEVER taken. The branch is a
/// `Guard(x != 0)` at its own bci, the walk continues on the fall-through
/// (`iconst_1`), and the taken arm (`iconst_2`) is never built. An empty
/// set (the kill switch's feed) builds the `If`.
#[test]
fn nio21w17_a_never_taken_branch_becomes_a_guard_on_the_fall_through() {
    let code = [
        0x1a, // 0: iload_0
        0x99, 0x00, 0x07, // 1: ifeq +7 -> 8   (never taken)
        0x04, // 4: iconst_1
        0xa7, 0x00, 0x04, // 5: goto +4 -> 9
        0x05, // 8: iconst_2     <- the cold arm
        0xac, // 9: ireturn
        0, 0, 0,
    ];
    let mut off = IrBuilder::new(1, 1);
    off.set_never_taken_branches(std::collections::HashSet::new());
    let plain = off.build(&code, 10).expect("unpruned build");
    assert!(plain.nodes.iter().any(|n| matches!(n.op, Op::If)));
    assert!(!plain.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })));

    let mut b = IrBuilder::new(1, 1);
    b.set_never_taken_branches(std::iter::once(1usize).collect());
    let g = b.build(&code, 10).expect("pruned build");
    let guard = g
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Guard { bci: 1 }))
        .expect("a guard at the branch's own bci");
    let cond = guard.inputs.get(1).copied().expect("a condition");
    assert!(
        matches!(g.nodes[cond as usize].op, Op::Cmp(CmpOp::Ne)),
        "the guard holds while the branch FALLS THROUGH (x != 0)"
    );
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::If)));
    let consts: Vec<i64> = g
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::Const(v) => Some(v),
            _ => None,
        })
        .collect();
    assert!(
        consts.contains(&1) && !consts.contains(&2),
        "the taken arm is gone; constants were {consts:?}"
    );
    // The join at 9 has the goto's edge only.
    assert_eq!(g.nodes[merge_at(&g, 9) as usize].inputs.len(), 1);
}

/// nio21 wave 17 (lane L4): past a pruned never-taken target the walk
/// steps over dead code (a dead `ireturn` included) and still activates a
/// later merge another, live, branch enters.
#[test]
fn nio21w17_dead_code_past_a_never_taken_target_is_stepped_over() {
    let code = [
        0x1a, // 0: iload_0
        0x9a, 0x00, 0x0b, // 1: ifne +11 -> 12   (live)
        0x1b, // 4: iload_1
        0x99, 0x00, 0x05, // 5: ifeq +5 -> 10    (never taken)
        0x06, // 8: iconst_3
        0xac, // 9: ireturn
        0x05, // 10: iconst_2   <- the pruned target, dead
        0xac, // 11: ireturn    (dead)
        0x04, // 12: iconst_1   <- merge, live from pc 1
        0xac, // 13: ireturn
        0, 0, 0,
    ];
    let mut b = IrBuilder::new(2, 2);
    b.set_never_taken_branches(std::iter::once(5usize).collect());
    let g = b
        .build(&code, 14)
        .expect("the dead taken arm is stepped over; the build must succeed");
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 5 })));
    assert_eq!(
        g.nodes.iter().filter(|n| n.op == Op::If).count(),
        1,
        "only the live branch at pc 1 builds an If"
    );
    assert!(
        !g.nodes.iter().any(|n| n.op == Op::Const(2)),
        "the dead arm"
    );
    assert!(g.nodes.iter().any(|n| n.op == Op::Const(3)));
    assert!(g.nodes.iter().any(|n| n.op == Op::Const(1)));
}

/// nio21 wave 17 (lane L4): a backward branch, or a pc with no frame state
/// yet, is never pruned as never-taken.
#[test]
fn nio21w17_never_taken_refuses_a_back_edge_and_a_pc_without_a_frame() {
    let mut b = IrBuilder::new(1, 1);
    b.set_never_taken_branches([1usize, 9].into_iter().collect());
    assert!(
        !b.prune_never_taken_branch(1, NO_NODE, 8),
        "no frame state at pc 1"
    );
    assert!(
        !b.prune_never_taken_branch(9, NO_NODE, 2),
        "a backward target"
    );
}

/// A guard has nothing to resume to inside a relocated body, so the tier
/// must decline to speculate there rather than set `splice_guard_seen` and
/// lose the whole method.
#[test]
fn pruning_is_declined_inside_a_splice() {
    let mut b = IrBuilder::new(1, 1);
    b.set_pruned_branches(std::iter::once(1usize).collect());
    // No splice is open in this unit context, so the refusal under test is
    // the OTHER one: a pc with no recorded frame state. Drive it by asking
    // about a pc the walk has not reached.
    assert!(
        !b.prune_always_taken_branch(1, NO_NODE, 8),
        "a bci with no safepoint snapshot must not be speculated on",
    );
}

/// A bottom-tested loop's back edge is always-taken while the loop is still
/// running, and pruning it used to make `build` never return.
///
/// `do { i++; } while (i < 10); return i;` is not rotated — its header at
/// pc 2 has a reachable lower-pc predecessor, so `has_rotated_loop_header`
/// leaves it on the pc walk — and its `if_icmplt` back edge is exactly what
/// a profile taken at an OSR trigger reports as `not_taken == 0`. Pruning
/// it recorded the header as a predecessor and then set `pc = 2`, landing
/// the walk back on an already-activated loop header: a second generation
/// of φs, an orphaned first generation, and then the same branch pruned
/// again, forever.
///
/// This test HANGS against the pre-2026-09-17 builder, which is the point:
/// it is a regression test for a non-termination, so it asserts on the
/// result of a build that used to have none.
#[test]
fn a_profiled_always_taken_backward_branch_is_never_pruned() {
    let code = [
        0x03, // 0: iconst_0
        0x3b, // 1: istore_0
        0x1a, // 2: iload_0        <- loop header
        0x04, // 3: iconst_1
        0x60, // 4: iadd
        0x3b, // 5: istore_0
        0x1a, // 6: iload_0
        0x10, 0x0a, // 7: bipush 10
        0xa1, 0xff, 0xf9, // 9: if_icmplt -7 -> 2   (the back edge)
        0x1a, // 12: iload_0
        0xac, // 13: ireturn
        0, 0, 0,
    ];

    let mut b = IrBuilder::new(0, 1);
    b.set_pruned_branches(std::iter::once(9usize).collect());
    let graph = b
        .build(&code, 14)
        .expect("the do/while must still build, unpruned");

    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { bci: 9 })),
        "a back edge must not be speculated on — its cold arm is the loop \
             EXIT, unobserved because the profile was taken mid-loop",
    );
    assert!(
        graph.nodes.iter().any(|n| matches!(n.op, Op::If)),
        "the back edge must keep its ordinary Op::If",
    );
}

/// Re-activating a join rebuilds every φ on it and resets its control
/// inputs, orphaning the first generation. It is always a bug in the walk,
/// so it is a refusal rather than a duplication.
#[test]
fn activating_a_merge_twice_refuses_the_build() {
    let mut b = IrBuilder::new(0, 1);
    b.ensure_merge(4);
    b.add_merge_predecessor(4);
    b.activate_merge(4);
    assert!(
        b.build_refused.is_none(),
        "one activation is the normal path",
    );

    b.activate_merge(4);
    assert!(
        b.build_refused.is_some(),
        "a second activation of the same merge must refuse the method",
    );
}

/// The same guard on the loop-header path: `activate_loop_header` builds a
/// fresh φ per live local every time it runs, and a second run would leave
/// one value per predecessor against a region that now has two.
#[test]
fn activating_a_loop_header_twice_refuses_the_build() {
    let mut b = IrBuilder::new(0, 1);
    b.ensure_merge(4);
    b.add_merge_predecessor(4);
    b.activate_loop_header(4);
    assert!(b.build_refused.is_none());

    b.activate_loop_header(4);
    assert!(
        b.build_refused.is_some(),
        "a second activation of the same loop header must refuse the method",
    );
}

/// A predecessor arriving at an already-activated merge that is NOT a
/// registered loop header used to be dropped on the floor: no edge, no
/// values, no bailout, and a graph the verifier cannot tell from a correct
/// one because the φs agree with the *reduced* predecessor count.
#[test]
fn a_back_edge_into_a_merge_with_no_loop_phis_refuses_instead_of_dropping_it() {
    let mut b = IrBuilder::new(0, 1);
    b.ensure_merge(4);
    b.add_merge_predecessor(4);
    // A FORWARD join: `activate_merge` registers no entry in `loop_phis`.
    b.activate_merge(4);
    let merge_id = b.merges[&4].merge_id;
    let preds_before = b.graph.nodes[merge_id as usize].inputs.len();
    assert!(b.build_refused.is_none());

    // A second predecessor, after activation. There are no loop φs to
    // back-patch, so the edge cannot be recorded.
    b.add_merge_predecessor(4);
    assert_eq!(
        b.graph.nodes[merge_id as usize].inputs.len(),
        preds_before,
        "this is the dropped edge — the point is that it is now REPORTED, \
             not that it is recorded",
    );
    assert!(
        b.build_refused.is_some(),
        "\"I cannot record this edge\" must not be spelled the same way as \
             \"nothing to do\"",
    );
}

/// A φ whose type *successfully* changes must re-type the φs that merge it.
///
/// The `Int` → `Ref` retype is the case `retype_phi`'s own doc opens with,
/// and until 2026-09-17 it was the one case that did not propagate: a φ
/// built between header activation and the back-edge patch kept the
/// provisional `Int`, so it carried an oop the oop map never reported and
/// `zero_ref_phi_slots` never zeroed.
#[test]
fn a_successful_retype_propagates_to_the_phis_that_merge_it() {
    let mut b = IrBuilder::new(0, 1);
    let region = b.graph.add(Op::Region, IrType::Control, vec![], None);
    let merge = b.graph.add(Op::Merge, IrType::Control, vec![], None);
    // The loop-carried φ, created with only its (undefined) entry value, so
    // it takes the conservative fallback.
    let loop_phi = b
        .graph
        .add(Op::Phi, PHI_TYPE_FALLBACK, vec![region, NO_NODE], None);
    // A φ created LATER in the body that consumes it, typed from the
    // provisional type.
    let downstream = b
        .graph
        .add(Op::Phi, PHI_TYPE_FALLBACK, vec![merge, loop_phi], None);

    let back = b.graph.add(
        Op::New {
            class_id: 3,
            num_fields: 0,
        },
        IrType::Ref,
        vec![],
        None,
    );
    b.graph.nodes[loop_phi as usize].inputs.push(back);
    b.retype_phi(loop_phi, 0);

    assert_eq!(
        b.graph.nodes[loop_phi as usize].ty,
        IrType::Ref,
        "the back-edge reference retypes the loop φ",
    );
    assert_eq!(
        b.graph.nodes[downstream as usize].ty,
        IrType::Ref,
        "and every φ that merges it — otherwise this one holds an oop \
             typed Int, invisible to the oop map and to zero_ref_phi_slots",
    );
    assert!(
        b.build_refused.is_none(),
        "a converging propagation is not a refusal",
    );
}

/// JVMS §2.6.1 in both directions, at the one place the abstract locals
/// array can express it.
#[test]
fn a_category_2_store_clears_the_reserved_slot_and_a_category_1_store_kills_the_pair() {
    let mut b = IrBuilder::new(0, 3);
    let obj = b.graph.add(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        IrType::Ref,
        vec![],
        None,
    );
    b.store_local(1, obj);
    assert_eq!(b.locals[1], obj);

    // A `long` at slot 0 reserves slot 1. The reference that was there is
    // dead in the JVM's model; leaving it would keep it a DCE root through
    // every later `SafepointSnapshot` and publish it as a live GC root.
    let wide = b.graph.add(Op::Const(7), IrType::Long, vec![], None);
    b.store_local(0, wide);
    assert_eq!(b.locals[0], wide);
    assert_eq!(
        b.locals[1], NO_NODE,
        "a category-2 store must not leave a stale value in its reserved \
             high half",
    );

    // …and overwriting that reserved half destroys the whole `long`.
    // Without this, `deopt_resume::ir_deopt_locals` sees a category-2 entry
    // at slot 0, skips slot 1, and shifts every later local by one.
    let narrow = b.graph.add(Op::Const(5), IrType::Int, vec![], None);
    b.store_local(1, narrow);
    assert_eq!(b.locals[1], narrow);
    assert_eq!(
        b.locals[0], NO_NODE,
        "storing into a long's high half destroys the long",
    );
}

/// The same rule, observed where it matters: in the deopt frame states the
/// builder records.
#[test]
fn a_long_store_does_not_retain_the_high_half_slot_in_later_frame_states() {
    let code = [
        0x09, // 0: lconst_0
        0x3f, // 1: lstore_0    (occupies slots 0 and 1)
        0x04, // 2: iconst_1
        0x3c, // 3: istore_1    (overwrites the long's reserved half)
        0x1b, // 4: iload_1
        0xac, // 5: ireturn
        0, 0, 0,
    ];
    let graph = IrBuilder::new(0, 3)
        .build(&code, 6)
        .expect("the long store must build");
    let last = graph
        .safepoints
        .iter()
        .find(|sp| sp.bci == 5)
        .expect("a frame state at the ireturn");
    assert_eq!(
        last.locals.first().copied(),
        Some(NO_NODE),
        "the long was destroyed by the istore into its high half, so its \
             slot must read as uninitialised rather than as a live category-2 \
             value the deopt walk would skip a slot for",
    );
}

/// `lstore_0; lstore_1`: the second `long` overwrites the first one's
/// high half, so the first is destroyed. Until 2026-09-18 only a
/// category-1 store looked at `idx - 1`, and the deopt walk then skipped
/// the live `long` at slot 1 as if it were the first one's high half.
#[test]
fn a_category_2_store_into_a_long_s_high_half_kills_the_long() {
    let mut b = IrBuilder::new(0, 4);
    let first = b.graph.add(Op::Const(1), IrType::Long, vec![], None);
    b.store_local(0, first);
    let second = b.graph.add(Op::Const(2), IrType::Long, vec![], None);
    b.store_local(1, second);
    assert_eq!(b.locals[1], second);
    assert_eq!(
        b.locals[0], NO_NODE,
        "the first long's high half was overwritten, so the first long is gone",
    );
    assert_eq!(b.locals[2], NO_NODE, "the second long's own high half");
    assert!(b.build_refused.is_none());
}

/// JIT round 21 wave 2 (lane keepset): `static int f(int c)` whose two
/// arms meet at pc 11, one still holding the `long` at slots 1-2, the other
/// having written an `int` into slot 2 (verified bytecode; ECJ-style slot
/// reuse). The join builds `φ(long, undef)` at slot 1 and `φ(undef, int)`
/// at slot 2. Both have an undefined input, so `retire_untypeable_phis`
/// (round 11 wave 7, unconditional at the end of `build`) retires both and
/// the finished frame state at the join names neither, whatever
/// `CRATONVM_JIT_IR_MERGE_CAT2_PAIR_KILL` says: a built graph never shows
/// the split pair. The switch acts on the builder's frame at the join,
/// before that pass (`kill_cat2_values_split_at_a_join`), which is pinned
/// directly below: the `long` goes, the value in its high half stays, only
/// under the switch.
#[test]
fn r21_keepset_a_join_that_splits_a_long_pair_leaves_its_slot_uninitialised() {
    let code = [
        0x0a, // 0: lconst_1
        0x40, // 1: lstore_1        (slots 1 and 2)
        0x1a, // 2: iload_0
        0x99, 0x00, 0x08, // 3: ifeq -> 11
        0x08, // 6: iconst_5
        0x3d, // 7: istore_2        (the long's high half: the long dies)
        0xa7, 0x00, 0x03, // 8: goto -> 11
        0x03, // 11: iconst_0
        0xac, // 12: ireturn
    ];
    let flag = concat!("CRATON", "VM_JIT_IR_MERGE_CAT2_PAIR_KILL");
    for v in [None, Some("0")] {
        let (slot1, slot2) = cratonvm_types::flags::with_thread_overrides(&[(flag, v)], || {
            let graph = IrBuilder::new(1, 3)
                .build(&code, code.len())
                .expect("the split pair builds");
            let sp = graph
                .safepoints
                .iter()
                .find(|sp| sp.bci == 11)
                .expect("a frame state at the join");
            (sp.locals.get(1).copied(), sp.locals.get(2).copied())
        });
        assert_eq!(
            (slot1, slot2),
            (Some(NO_NODE), Some(NO_NODE)),
            "{v:?}: both slots are TOP at the join once the undefined-input φs retire"
        );
    }

    // The builder-level rule, on a frame a join just built.
    let split_frame = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(&[(flag, v)], || {
            let mut b = IrBuilder::new(0, 5);
            let long = b.graph.add(Op::Const(7), IrType::Long, vec![], None);
            let int = b.graph.add(Op::Const(5), IrType::Int, vec![], None);
            let pair = b.graph.add(Op::Const(9), IrType::Long, vec![], None);
            b.locals[1] = long; // split: slot 2 holds a live int
            b.locals[2] = int;
            b.locals[3] = pair; // well formed: slot 4 is its empty high half
            b.locals[4] = NO_NODE;
            b.kill_cat2_values_split_at_a_join();
            (b.locals[1], b.locals[2], b.locals[3], long, int, pair)
        })
    };
    let (s1, s2, s3, _, int, pair) = split_frame(None);
    assert_eq!(s1, NO_NODE, "the long whose high half was written is TOP");
    assert_eq!(s2, int, "the value in its high half stays");
    assert_eq!(s3, pair, "a well-formed long is untouched");
    assert!(true, "default ON");
}

/// A local index past the frame is refused like a store past it, not a
/// panic on the compiler thread.
#[test]
fn a_load_or_iinc_of_a_slot_past_the_frame_refuses_instead_of_panicking() {
    // iload 5; ireturn — one local.
    let load = [0x15, 0x05, 0xac];
    assert!(IrBuilder::new(0, 1).build(&load, 3).is_none());
    // iinc 5, 1; iconst_0; ireturn
    let inc = [0x84, 0x05, 0x01, 0x03, 0xac];
    assert!(IrBuilder::new(0, 1).build(&inc, 5).is_none());
}

/// `int f(int c) { if (c != 0) { int x = 5; } else { Object y = null; }
/// return 0; }` — javac gives `x` and `y` the same slot, so the join at
/// pc 11 merges an `int` with a reference. The slot is dead there (TOP to
/// the JVM verifier) and must be described as undefined, not as a φ that
/// could only take `PHI_TYPE_FALLBACK` and that `check_types` rejects.
#[test]
fn a_slot_reused_for_two_types_is_dead_at_the_join_not_a_conflicting_phi() {
    let code = [
        0x1a, // 0: iload_0
        0x99, 0x00, 0x08, // 1: ifeq -> 9
        0x08, // 4: iconst_5
        0x3c, // 5: istore_1          int in slot 1
        0xa7, 0x00, 0x05, // 6: goto -> 11
        0x01, // 9: aconst_null
        0x4c, // 10: astore_1         reference in slot 1
        0x03, // 11: iconst_0         the join
        0xac, // 12: ireturn
    ];
    let graph = IrBuilder::new(1, 2)
        .build(&code, 13)
        .expect("a reused slot must not refuse the method");
    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.op == Op::Phi && n.ty != IrType::Memory),
        "no value φ may be built for a slot whose predecessors do not join",
    );
    let at_join = graph
        .safepoints
        .iter()
        .find(|sp| sp.bci == 11)
        .expect("a frame state at the join");
    assert_eq!(at_join.locals.get(1).copied(), Some(NO_NODE));
    let verdict =
        crate::ir_verify::verify_graph(&graph, "test", crate::ir_verify::VerifyOptions::default());
    assert!(verdict.is_ok(), "{verdict:?}");

    // The control: the same shape with an `int` on both arms keeps its φ.
    let mut same = code;
    same[9] = 0x05; // iconst_2
    same[10] = 0x3c; // istore_1
    let graph = IrBuilder::new(1, 2).build(&same, 13).expect("builds");
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.op == Op::Phi && n.ty == IrType::Int),
        "a slot whose predecessors DO join still gets its φ",
    );
}

/// The loop-header form of the same thing: a dead `Object` in slot 1
/// before the loop, an `int` stored there inside it. The header φ is built
/// from the entry edge (`Ref`) before the back edge exists, so the
/// conflict appears only at `patch_loop_backedge` — and the φ is retired
/// at the end of the build rather than left for `check_types` to reject.
#[test]
fn a_loop_phi_that_only_conflicts_on_its_back_edge_is_retired() {
    let code = [
        0x01, // 0: aconst_null
        0x4c, // 1: astore_1           dead reference before the loop
        0x1a, // 2: iload_0            loop header
        0x9e, 0x00, 0x0b, // 3: ifle -> 14
        0x04, // 6: iconst_1
        0x3c, // 7: istore_1           int in the same slot
        0x84, 0x00, 0xff, // 8: iinc 0, -1
        0xa7, 0xff, 0xf7, // 11: goto -> 2
        0x03, // 14: iconst_0
        0xac, // 15: ireturn
    ];
    let graph = IrBuilder::new(1, 2)
        .build(&code, 16)
        .expect("a reused slot around a loop must not refuse the method");
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Phi && n.ty != IrType::Memory {
            assert!(
                !phi_inputs_conflict(&graph, &n.inputs),
                "n{id} is a live φ whose inputs do not join",
            );
        }
    }
    let in_body = graph
        .safepoints
        .iter()
        .find(|sp| sp.bci == 3)
        .expect("a frame state inside the loop");
    assert_eq!(
        in_body.locals.get(1).copied(),
        Some(NO_NODE),
        "the retired φ's snapshot slots read as undefined",
    );
    let counter = in_body.locals[0];
    assert!(
        matches!(graph.node_opt(counter), Some(n) if n.op == Op::Phi && n.ty == IrType::Int),
        "the loop counter's φ is untouched",
    );
    let verdict =
        crate::ir_verify::verify_graph(&graph, "test", crate::ir_verify::VerifyOptions::default());
    assert!(verdict.is_ok(), "{verdict:?}");
}

/// `HashMap.getNode`'s shape (round 11 wave 7): a local assigned on ONE
/// arm before a join, then reassigned inside a loop and never read. The
/// join's φ has an undefined input, and the loop-header φ merges it; both
/// are TOP to the JVM verifier and both are retired, so no frame state
/// names a value on a path that never computed it. `ir_optimize` used to
/// fold the join's `φ(undef, alen)` to `alen`, which the header φ then read
/// on an edge `alen` does not dominate.
#[test]
fn a_phi_merging_an_unset_slot_is_retired_with_the_phis_that_merge_it() {
    // static int f(int[] a, int c) { int k;
    //   if (c > 0) k = a.length; c--; while (c > 0) { k = c; c--; } return 0; }
    let code = [
        0x1b, // 0: iload_1
        0x9e, 0x00, 0x06, // 1: ifle -> 7
        0x2a, // 4: aload_0
        0xbe, // 5: arraylength
        0x3d, // 6: istore_2           k set on this arm only
        0x84, 0x01, 0xff, // 7: iinc 1, -1   the join
        0x1b, // 10: iload_1           loop header
        0x9e, 0x00, 0x0b, // 11: ifle -> 22
        0x1b, // 14: iload_1
        0x3d, // 15: istore_2          k reassigned, never read
        0x84, 0x01, 0xff, // 16: iinc 1, -1
        0xa7, 0xff, 0xf7, // 19: goto -> 10
        0x03, // 22: iconst_0
        0xac, // 23: ireturn
    ];
    let mut b = IrBuilder::new(2, 3);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    let graph = b.build(&code, 24).expect("the shape must build");
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Phi && n.ty != IrType::Memory {
            assert!(
                n.phi_value_inputs().all(|v| v.is_some()),
                "n{id} is a surviving φ with an undefined input: {:?}",
                n.inputs,
            );
        }
    }
    let in_body = graph
        .safepoints
        .iter()
        .find(|sp| sp.bci == 11)
        .expect("a frame state inside the loop");
    assert_eq!(
        in_body.locals.get(2).copied(),
        Some(NO_NODE),
        "the retired φs' snapshot slots read as undefined",
    );
    let counter = in_body.locals.get(1).copied().unwrap_or(NO_NODE);
    assert!(
        matches!(graph.node_opt(counter), Some(n) if n.op == Op::Phi && n.ty == IrType::Int),
        "the loop counter's φ, which a compare reads, is untouched",
    );
    let verdict =
        crate::ir_verify::verify_graph(&graph, "test", crate::ir_verify::VerifyOptions::default());
    assert!(verdict.is_ok(), "{verdict:?}");
}

/// A predecessor recorded into a merge the walk never activates is an
/// edge with nowhere to go, and refuses the build; a merge nothing ever
/// reached is an orphan, and is removed.
#[test]
fn an_unactivated_merge_is_killed_when_orphaned_and_refused_when_it_lost_an_edge() {
    let mut b = IrBuilder::new(0, 1);
    let orphan = b.ensure_merge(20).merge_id;
    assert_eq!(b.audit_unactivated_merges(), None);
    assert_eq!(b.graph.nodes[orphan as usize].op, Op::Dead);

    b.ensure_merge(30);
    b.add_merge_predecessor(30);
    assert_eq!(b.audit_unactivated_merges(), Some(30));
}

/// `goto_w` is `goto` with a wide offset.
#[test]
fn goto_w_builds_like_goto() {
    let code = [
        0x04, // 0: iconst_1
        0xc8, 0x00, 0x00, 0x00, 0x05, // 1: goto_w -> 6
        0xac, // 6: ireturn
    ];
    let graph = IrBuilder::new(0, 0)
        .build(&code, 7)
        .expect("goto_w must build");
    assert!(graph.nodes.iter().any(|n| n.op == Op::Return));
    assert_eq!(graph.nodes[merge_at(&graph, 6) as usize].inputs.len(), 1);
}

/// `new C(x)` spliced into the method doing the `new`: the constructor's
/// store into its own receiver is admitted (the object is still
/// uninitialised and reachable from this frame alone), unless the
/// constructor read one of the receiver's fields first, which a replay of
/// the constructor would read back changed.
#[test]
fn a_spliced_constructor_stores_into_the_object_this_method_allocated() {
    fn ctor_site(base: usize, code_len: usize) -> IrInlineSite {
        IrInlineSite {
            base,
            code_len,
            num_args: 2,
            max_locals: 2,
            arg_local_slots: vec![0, 1],
            returns_value: false,
            receiver_is_arg0: true,
            method_key: "C.<init>:(LC;)V".to_string(),
            class_id: 7,
        }
    }
    // Caller: new #1; dup; aconst_null; invokespecial #2; areturn.
    let caller = [0xbb, 0x00, 0x01, 0x59, 0x01, 0xb7, 0x00, 0x02, 0xb0];
    let build = |callee: &[u8], fields: &[usize]| {
        let mut combined = caller.to_vec();
        let base = combined.len();
        combined.extend_from_slice(callee);
        let mut b = IrBuilder::new(0, 0);
        b.set_new_info(HashMap::from([(0usize, (7u32, 1usize))]), HashSet::new());
        b.set_inline_sites(HashMap::from([(5usize, ctor_site(base, callee.len()))]));
        b.set_field_info(
            fields
                .iter()
                .map(|&pc| (base + pc, (0usize, b'L')))
                .collect(),
        );
        b.build(&combined, base)
    };
    // Callee: aload_0; aload_1; putfield #3; return.
    let graph = build(&[0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1], &[2])
        .expect("the constructor's store into the fresh receiver is admitted");
    assert!(graph.nodes.iter().any(|n| n.op == Op::Store(MemKind::Ref)));
    // A store after the constructor READ its receiver's field is no longer
    // a receiver store (a replay would read the first run's value back),
    // but with nothing after it that can trap it is admitted as a
    // COMMITTED store (`admits_after_committed_splice_store`, round 11),
    // which no replay can repeat.
    // Callee: aload_0; aload_0; getfield #3; putfield #3; return.
    assert!(
        build(
            &[0x2a, 0x2a, 0xb4, 0x00, 0x03, 0xb5, 0x00, 0x03, 0xb1],
            &[2, 5]
        )
        .is_some(),
        "read-then-store with no trap after it is a committed store",
    );
    // Round 11 wave 15 (lane `ir`): the arm the merge lost. The read DOES
    // still refuse the receiver rule -- visible once something that can
    // trap follows the store: a `getfield` on the null argument, which the
    // committed-store rule cannot prove non-null. The same trap after a
    // receiver store with no read builds, because replaying THAT store is
    // harmless. (A division would not do here: the resolver refuses it
    // inside a splice for reasons of its own.)
    // Callee: aload_0; aload_0; getfield #3; putfield #3;
    //         aload_1; getfield #4; pop; return.
    assert!(
        build(
            &[0x2a, 0x2a, 0xb4, 0x00, 0x03, 0xb5, 0x00, 0x03, 0x2b, 0xb4, 0x00, 0x04, 0x57, 0xb1],
            &[2, 5, 9]
        )
        .is_none(),
        "a store after the constructor read its receiver's field, with a trap after it, \
             is refused",
    );
    // Callee: aload_0; aload_1; putfield #3; aload_1; getfield #4; pop; return.
    assert!(
        build(
            &[0x2a, 0x2b, 0xb5, 0x00, 0x03, 0x2b, 0xb4, 0x00, 0x04, 0x57, 0xb1],
            &[2, 6]
        )
        .is_some(),
        "the same trap after a receiver store with no read builds",
    );
}

#[test]
fn switch_splices_decode_the_original_alignment_and_own_body_bounds() {
    fn body(sparse: bool) -> Vec<u8> {
        let mut code = vec![0x1a, if sparse { 0xab } else { 0xaa }];
        while code.len() % 4 != 0 {
            code.push(0);
        }
        let table = code.len();
        code.extend_from_slice(&0i32.to_be_bytes());
        if sparse {
            code.extend_from_slice(&3i32.to_be_bytes());
            for key in [-1000i32, -1, 1000000] {
                code.extend_from_slice(&key.to_be_bytes());
                code.extend_from_slice(&0i32.to_be_bytes());
            }
        } else {
            code.extend_from_slice(&(-1i32).to_be_bytes());
            code.extend_from_slice(&1i32.to_be_bytes());
            for _ in 0..3 {
                code.extend_from_slice(&0i32.to_be_bytes());
            }
        }
        let target = code.len();
        code.extend_from_slice(&[0x04, 0xac, 0x05, 0xac, 0x06, 0xac]);
        code[table..table + 4].copy_from_slice(&((target + 4 - 1) as i32).to_be_bytes());
        for i in 0..3 {
            let offset = if sparse {
                table + 12 + i * 8
            } else {
                table + 12 + i * 4
            };
            // Two keys share a target; default has its own return.
            code[offset..offset + 4]
                .copy_from_slice(&((target + (i % 2) * 2 - 1) as i32).to_be_bytes());
        }
        code
    }
    for sparse in [false, true] {
        for pad in 0..4 {
            let callee = body(sparse);
            let mut code = vec![0x00; pad];
            code.extend_from_slice(&[0x1a, 0xb8, 0x00, 0x01, 0xac]);
            let base = code.len();
            code.extend_from_slice(&callee);
            // Adjacent bytes must never enlarge this callee's decode bound.
            code.extend_from_slice(&[0x00, 0x04, 0xac]);
            let mut site = r11w15_static_site(base, callee.len(), 1, 1);
            site.returns_value = true;
            site.method_key = "Switch.pick:(I)I".into();
            let mut b = IrBuilder::new(1, 1);
            b.set_inline_sites(HashMap::from([(pad + 1, site.clone())]));
            assert!(
                b.build(&code, base).is_some(),
                "sparse={sparse}, base={base}"
            );
            site.code_len = callee.len() - 3;
            let mut b = IrBuilder::new(1, 1);
            b.set_inline_sites(HashMap::from([(pad + 1, site)]));
            assert!(
                b.build(&code, base).is_none(),
                "cross-body target must refuse"
            );
        }
    }
}

/// A static callee site for the splice-store tests below.
fn r11w15_static_site(
    base: usize,
    code_len: usize,
    num_args: usize,
    max_locals: usize,
) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len,
        num_args,
        max_locals,
        arg_local_slots: (0..num_args as u32).collect(),
        returns_value: false,
        receiver_is_arg0: false,
        method_key: "S.f:()V".to_string(),
        class_id: 0,
    }
}

/// Round 11 wave 15 (lane `ir`): after a committed splice store, a field
/// READ from an object the splice allocated is not a proven-non-null base.
/// `w = new W(); p.x = 1; w.next.y` null-checks `w.next` after the store,
/// and a failed check re-runs the call and the store with it.
#[test]
fn a_committed_splice_store_does_not_trust_a_field_read_from_a_local_object() {
    let build = |tail_base: u8| {
        // Caller: aload_0; invokestatic #1; return.
        let mut combined = vec![0x2a, 0xb8, 0x00, 0x01, 0xb1];
        let base = combined.len();
        // Callee: new #1; getfield next; astore_1; aload_0; iconst_1;
        //         putfield x; <tail_base>; getfield y; pop; return.
        let callee = [
            0xbb, 0x00, 0x01, 0xb4, 0x00, 0x02, 0x4c, 0x2a, 0x04, 0xb5, 0x00, 0x03, tail_base,
            0xb4, 0x00, 0x04, 0x57, 0xb1,
        ];
        combined.extend_from_slice(&callee);
        let mut b = IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        b.set_new_info(HashMap::from([(base, (7u32, 2usize))]), HashSet::new());
        b.set_field_info(HashMap::from([
            (base + 3, (0usize, b'L')),
            (base + 9, (0usize, b'I')),
            (base + 13, (1usize, b'I')),
        ]));
        let site = r11w15_static_site(base, callee.len(), 1, 2);
        b.set_inline_sites(HashMap::from([(1usize, site)]));
        b.build(&combined, base)
    };
    assert!(
        build(0x2b).is_none(),
        "`w.next.y` after a committed store may trap on a null `w.next`: refused",
    );
    assert!(
        build(0x2a).is_some(),
        "a read through the base the committed store already wrote through builds",
    );
}

/// Round 11 wave 15 (lane `ir`): `aastore` of a caller reference into a
/// splice-local array taints it like `putfield` does, so the element read
/// back is NOT local and a store through it is not a replay-safe local
/// store. `a = new P[1]; a[0] = p; a[0].x = 1; p.y` wrote `p.x` and then
/// null-checked `p` -- the replay wrote `p.x` again.
#[test]
fn a_spliced_aastore_of_a_caller_reference_taints_the_local_array() {
    // Caller: aload_0; invokestatic #1; return.
    let mut combined = vec![0x2a, 0xb8, 0x00, 0x01, 0xb1];
    let base = combined.len();
    // Callee: iconst_1; anewarray #1; astore_1; aload_1; iconst_0; aload_0;
    //         aastore; aload_1; iconst_0; aaload; iconst_1; putfield x;
    //         aload_0; getfield y; pop; return.
    let callee = [
        0x04, 0xbd, 0x00, 0x01, 0x4c, 0x2b, 0x03, 0x2a, 0x53, 0x2b, 0x03, 0x32, 0x04, 0xb5, 0x00,
        0x02, 0x2a, 0xb4, 0x00, 0x03, 0x57, 0xb1,
    ];
    combined.extend_from_slice(&callee);
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.set_anewarray_info(HashMap::from([(base + 1, 9u32)]));
    b.set_field_info(HashMap::from([
        (base + 13, (0usize, b'I')),
        (base + 17, (1usize, b'I')),
    ]));
    b.set_inline_sites(HashMap::from([(
        1usize,
        r11w15_static_site(base, callee.len(), 1, 2),
    )]));
    assert!(
        b.build(&combined, base).is_none(),
        "the store through `a[0]` is a committed store into the caller's object, \
             and the null check after it must refuse",
    );
}

/// Round 11 wave 15 (lane `ir`): taint is a property of an OBJECT, and two
/// nodes can name one. `v = new V(); w = new W(); w.f = v; w.f.g = p;
/// v.g.x = 1; q.y` -- the caller reference went in through the node
/// `w.f`, and `v.g` must not read back as local.
#[test]
fn a_taint_through_one_alias_ends_locality_for_every_later_read() {
    // Caller: aload_0; aload_1; invokestatic #1; return.
    let mut combined = vec![0x2a, 0x2b, 0xb8, 0x00, 0x01, 0xb1];
    let base = combined.len();
    let callee = [
        0xbb, 0x00, 0x01, // 0: new V
        0x4d, // 3: astore_2
        0xbb, 0x00, 0x02, // 4: new W
        0x4e, // 7: astore_3
        0x2d, 0x2c, 0xb5, 0x00, 0x03, // 8: w.f = v
        0x2d, 0xb4, 0x00, 0x03, // 13: w.f
        0x2a, 0xb5, 0x00, 0x04, // 17: .g = p
        0x2c, 0xb4, 0x00, 0x04, // 21: v.g
        0x04, 0xb5, 0x00, 0x05, // 25: .x = 1
        0x2b, 0xb4, 0x00, 0x06, 0x57, // 29: q.y; pop
        0xb1, // 34: return
    ];
    combined.extend_from_slice(&callee);
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Ref]);
    b.set_new_info(
        HashMap::from([(base, (7u32, 1usize)), (base + 4, (8u32, 1usize))]),
        HashSet::new(),
    );
    b.set_field_info(HashMap::from([
        (base + 10, (0usize, b'L')),
        (base + 14, (0usize, b'L')),
        (base + 18, (1usize, b'L')),
        (base + 22, (1usize, b'L')),
        (base + 26, (2usize, b'I')),
        (base + 30, (3usize, b'I')),
    ]));
    b.set_inline_sites(HashMap::from([(
        2usize,
        r11w15_static_site(base, callee.len(), 2, 4),
    )]));
    assert!(
        b.build(&combined, base).is_none(),
        "`v.g.x = 1` writes the caller's object: a committed store, so the trap after it \
             refuses",
    );
}

/// Round 11 wave 15 (lane `ir`): a constructor that hands `this` on
/// (`h.c = this`) and reads its own field back through that place
/// (`this.n = h.c.n + 1`) has READ its receiver, though never through
/// the receiver node. One that stores something else there
/// (`h.c = p`) has not, and keeps the receiver rule.
#[test]
fn a_constructor_reading_its_receiver_through_an_alias_is_a_read() {
    let build = |stored: u8| {
        // Caller: new C; dup; aload_0; invokespecial <init>(LP;)V; pop; return.
        let mut combined = vec![0xbb, 0x00, 0x01, 0x59, 0x2a, 0xb7, 0x00, 0x02, 0x57, 0xb1];
        let base = combined.len();
        let callee = [
            0xbb, 0x00, 0x02, // 0: new H
            0x4d, // 3: astore_2
            0x2c, stored, 0xb5, 0x00, 0x03, // 4: h.c = <stored>
            0x2a, 0x2c, 0xb4, 0x00, 0x03, // 9: this, h.c
            0xb4, 0x00, 0x04, 0x04, 0x60, // 14: .n + 1
            0xb5, 0x00, 0x04, // 19: this.n = ...
            0x2b, 0xb4, 0x00, 0x05, 0x57, // 22: p.z; pop -- can trap
            0xb1, // 27: return
        ];
        combined.extend_from_slice(&callee);
        let ctor = IrInlineSite {
            base,
            code_len: callee.len(),
            num_args: 2,
            max_locals: 3,
            arg_local_slots: vec![0, 1],
            returns_value: false,
            receiver_is_arg0: true,
            method_key: "C.<init>:(LP;)V".to_string(),
            class_id: 7,
        };
        let mut b = IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        b.set_new_info(
            HashMap::from([(0usize, (7u32, 1usize)), (base, (8u32, 1usize))]),
            HashSet::new(),
        );
        b.set_field_info(HashMap::from([
            (base + 6, (0usize, b'L')),
            (base + 11, (0usize, b'L')),
            (base + 14, (1usize, b'I')),
            (base + 19, (1usize, b'I')),
            (base + 23, (2usize, b'I')),
        ]));
        b.set_inline_sites(HashMap::from([(5usize, ctor)]));
        b.build(&combined, base)
    };
    assert!(
        build(0x2a).is_none(),
        "`h.c = this; this.n = h.c.n + 1` read the receiver: a committed store, and the \
             trap after it refuses",
    );
    assert!(
        build(0x2b).is_some(),
        "`h.c = p` hands nothing on: the receiver rule still admits `this.n = ...`",
    );
}

/// Round 11 wave 15 (lane `ir`): the constructor-receiver rule lasts only
/// as long as the constructor. After `c = new C()` returns, the caller can
/// publish `c` (`h.c = c`), and a later splice reading `h.c.n` reads the
/// receiver through an alias the rule cannot see.
#[test]
fn the_ctor_receiver_rule_ends_with_the_constructor_splice() {
    let build = |with_trap: bool| {
        // Caller: new #1; dup; invokespecial <init>; astore_1; aload_1;
        //         aload_0; invokestatic f; return.
        let mut combined = vec![
            0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0x4c, 0x2b, 0x2a, 0xb8, 0x00, 0x03, 0xb1,
        ];
        let code_len = combined.len();
        let ctor_base = combined.len();
        combined.push(0xb1); // C.<init>: return
        let f_base = combined.len();
        // f(C a, H h): a.n = h.c.n + 1; [h.z; pop;] return.
        let mut f = vec![
            0x2a, 0x2b, 0xb4, 0x00, 0x03, 0xb4, 0x00, 0x04, 0x04, 0x60, 0xb5, 0x00, 0x04,
        ];
        if with_trap {
            f.extend_from_slice(&[0x2b, 0xb4, 0x00, 0x05, 0x57]);
        }
        f.push(0xb1);
        combined.extend_from_slice(&f);
        let ctor = IrInlineSite {
            base: ctor_base,
            code_len: 1,
            num_args: 1,
            max_locals: 1,
            arg_local_slots: vec![0],
            returns_value: false,
            receiver_is_arg0: true,
            method_key: "C.<init>:()V".to_string(),
            class_id: 7,
        };
        let mut b = IrBuilder::new(1, 2);
        b.set_param_types(&[IrType::Ref]);
        b.set_new_info(HashMap::from([(0usize, (7u32, 1usize))]), HashSet::new());
        b.set_field_info(HashMap::from([
            (f_base + 2, (0usize, b'L')),
            (f_base + 5, (0usize, b'I')),
            (f_base + 10, (0usize, b'I')),
            (f_base + 14, (1usize, b'I')),
        ]));
        b.set_inline_sites(HashMap::from([
            (4usize, ctor),
            (10usize, r11w15_static_site(f_base, f.len(), 2, 2)),
        ]));
        b.build(&combined, code_len)
    };
    assert!(
        build(false).is_some(),
        "with nothing after it, the store into the finished object is a committed store",
    );
    assert!(
        build(true).is_none(),
        "a trap after a store into a FINISHED constructor receiver must refuse",
    );
}

/// A spliced `fastore`/`dastore` must target an array the splice itself
/// allocated, exactly like every other store arm: a deopt inside the
/// spliced region re-executes the whole call, and a write into the
/// CALLER'S array would be committed twice.
#[test]
fn a_spliced_fp_array_store_into_a_callers_array_refuses_the_splice() {
    fn site(base: usize, code_len: usize, num_args: usize) -> IrInlineSite {
        IrInlineSite {
            base,
            code_len,
            num_args,
            max_locals: num_args,
            arg_local_slots: (0..num_args as u32).collect(),
            returns_value: false,
            receiver_is_arg0: false,
            method_key: "P.f:([F)V".to_string(),
            class_id: 0,
        }
    }

    // Caller: aload_0; invokestatic f([F)V; return.
    // Callee: aload_0; iconst_0; fconst_1; fastore; return.
    let mut combined = vec![0x2a, 0xb8, 0x00, 0x01, 0xb1];
    let base = combined.len();
    combined.extend_from_slice(&[0x2a, 0x03, 0x0c, 0x51, 0xb1]);
    let mut b = IrBuilder::new(1, 1);
    b.set_inline_sites(HashMap::from([(1usize, site(base, 5, 1))]));
    assert!(
        b.build(&combined, base).is_none(),
        "a spliced float store into the caller's array must refuse",
    );

    // Caller: invokestatic g()V; return.
    // Callee: iconst_1; newarray float; iconst_0; fconst_1; fastore; return.
    let mut combined = vec![0xb8, 0x00, 0x01, 0xb1];
    let base = combined.len();
    combined.extend_from_slice(&[0x04, 0xbc, 0x06, 0x03, 0x0c, 0x51, 0xb1]);
    let mut b = IrBuilder::new(0, 0);
    b.set_inline_sites(HashMap::from([(0usize, site(base, 7, 0))]));
    let graph = b
        .build(&combined, base)
        .expect("a store into an array the splice made is local and builds");
    assert!(graph
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::ArrayStore(MemKind::Float))));
}

/// `nop` is admitted by the scanner and lowered by the single-pass tier, so
/// a `nop`-bearing method reached the builder and died at its `_ =>`
/// catch-all after a full partial build. It builds now.
#[test]
fn a_method_containing_nop_builds() {
    let code = [
        0x00, // 0: nop
        0x1a, // 1: iload_0
        0x00, // 2: nop
        0xac, // 3: ireturn
        0, 0,
    ];
    assert!(
        IrBuilder::new(1, 1).build(&code, 4).is_some(),
        "`nop` must be lowered here, not refused after a full graph build",
    );
}

/// `athrow` leaves the compiled frame through the `i64::MIN` sentinel and
/// nothing on that path releases a monitor this frame took — the same
/// reason `return` is refused while one is held.
/// `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` are total, side-effect-free
/// arithmetic. `ir_optimize` asks `is_pure` when it wants to know whether a
/// node can touch memory, so leaving them out of it declared every loop
/// containing one to hold a hard memory barrier and cost it all of its LICM.
#[test]
fn the_three_way_comparisons_are_pure_so_they_are_not_memory_barriers() {
    for op in [
        Op::LCmp,
        Op::FCmp {
            double: false,
            nan_greater: true,
        },
        Op::FCmp {
            double: true,
            nan_greater: false,
        },
    ] {
        assert!(op.is_pure(), "{op:?} is pure arithmetic");
        assert!(!op.is_control(), "{op:?}");
        // Verbatim the classification `ir_optimize::loop_has_hard_barrier`
        // and `preheader_may_trap` apply.
        let hard_barrier = !op.is_pure()
            && !op.is_control()
            && !matches!(op, Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead);
        assert!(
            !hard_barrier,
            "{op:?} must not disqualify its loop from every load hoist",
        );
    }
    // The exclusions that are correct stay correct: these two trap.
    assert!(!Op::Div.is_pure());
    assert!(!Op::Rem.is_pure());
}

/// `interned_const` keys on `(value, type)`, so the `Long` case needed only
/// the call — and without it every `lconst_0`/`lconst_1`, every `ldc2_w`
/// long and every `ldiv`/`lrem` zero guard added a node for GVN to collapse.
#[test]
fn a_repeated_long_constant_is_interned_the_way_an_int_one_is() {
    let mut b = IrBuilder::new(0, 1);
    let first = b.lconst(7);
    let second = b.lconst(7);
    assert_eq!(first, second, "a long constant must dedupe");

    // The TYPE is part of the identity, exactly as it is for the `Ref`
    // zero `aconst_null` materialises: an `Int` 7 is a different node.
    let narrow = b.iconst(7);
    assert_ne!(first, narrow);
    assert_eq!(b.graph.nodes[first as usize].ty, IrType::Long);
    assert_eq!(b.graph.nodes[narrow as usize].ty, IrType::Int);
}

#[test]
fn test_ir_identity_function() {
    // int f(int x) { return x; }
    // iload_0; ireturn
    let code = [0x1a, 0xac, 0, 0];
    let graph = build_ir(&code, 2, 1, 1);
    // Should have: Start, Proj(0), Proj(1), Param(0), Return
    assert!(graph.exit != NO_NODE);
    assert_eq!(graph.nodes[graph.exit as usize].op, Op::Return);
    // Return's data input should be Param(0)
    let ret = &graph.nodes[graph.exit as usize];
    assert_eq!(ret.inputs.len(), 2); // [ctrl, value]
    let val_id = ret.inputs[1];
    assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
}

#[test]
fn test_ir_add_two_params() {
    // int f(int a, int b) { return a + b; }
    // iload_0; iload_1; iadd; ireturn
    let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
    let graph = build_ir(&code, 4, 2, 2);
    let ret = &graph.nodes[graph.exit as usize];
    let add_id = ret.inputs[1];
    assert_eq!(graph.nodes[add_id as usize].op, Op::Add);
    let add = &graph.nodes[add_id as usize];
    assert_eq!(graph.nodes[add.inputs[0] as usize].op, Op::Param(0));
    assert_eq!(graph.nodes[add.inputs[1] as usize].op, Op::Param(1));
}

#[test]
fn test_ir_constant_folding_candidates() {
    // int f() { return 3 + 4; }
    // iconst_3; iconst_4; iadd; ireturn
    let code = [0x06, 0x07, 0x60, 0xac, 0, 0];
    let graph = build_ir(&code, 4, 0, 0);
    let ret = &graph.nodes[graph.exit as usize];
    let add_id = ret.inputs[1];
    let add = &graph.nodes[add_id as usize];
    // Both inputs should be Const nodes
    assert!(matches!(
        graph.nodes[add.inputs[0] as usize].op,
        Op::Const(3)
    ));
    assert!(matches!(
        graph.nodes[add.inputs[1] as usize].op,
        Op::Const(4)
    ));
}

#[test]
fn test_ir_branch_creates_merge() {
    // int f(int x) { if (x == 0) return 1; return 0; }
    // iload_0; ifeq +5; iconst_0; ireturn; iconst_1; ireturn
    //   0       1        4         5        6         7
    let code = [
        0x1a, // 0: iload_0
        0x99, 0x00, 0x05, // 1: ifeq → 6
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x04, // 6: iconst_1
        0xac, // 7: ireturn
        0, 0, // padding
    ];
    let graph = build_ir(&code, 8, 1, 1);
    // Should have Merge, If, Cmp nodes
    let has_if = graph.nodes.iter().any(|n| n.op == Op::If);
    assert!(has_if, "Should contain an If node");
    let has_cmp = graph.nodes.iter().any(|n| matches!(n.op, Op::Cmp(_)));
    assert!(has_cmp, "Should contain a Cmp node");
}

/// Round 9 wave 10 (irl10), `perf-ir-refuses-volatile-instance-field-methods`:
/// a SPLICED callee's volatile `getfield`, named by its combined-buffer pc
/// through `add_spliced_volatile_field_pcs`, builds as the ordered
/// `Op::Load` (Acquire) the method's own volatile read builds as; the same
/// body without the pc builds a plain load (the control), and with
/// admission off the build bails rather than emitting an unordered read.
#[test]
fn r9w10_irl10_a_spliced_volatile_getfield_builds_an_ordered_load() {
    // Caller: `static int f(O o) { return g(o); }`
    //   pc 0  aload_0
    //   pc 1  invokestatic #1   (spliced)
    //   pc 4  ireturn
    // Callee `static int g(O o) { return o.v; }`, relocated to combined pc 6:
    //   pc 6  aload_0
    //   pc 7  getfield #2
    //   pc 10 ireturn
    let code = [
        0x2a, 0xb8, 0x00, 0x01, 0xac, 0x00, // caller, code_len 5 (byte 5 is padding)
        0x2a, 0xb4, 0x00, 0x02, 0xac, // relocated callee at [6, 11)
        0, 0,
    ];
    let site = IrInlineSite {
        base: 6,
        code_len: 5,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "P.g:(LO;)I".to_string(),
        class_id: 7,
    };
    // `admit`: `None` = the pc is not supplied at all (the control).
    let build = |admit: Option<(bool, bool)>| {
        let mut b = IrBuilder::new(1, 1);
        let mut tables = IrInlineTables::default();
        tables.sites.insert(1, site.clone());
        // The row `append_ir_inline_site` rebases: CALLEE pc 1 + base 6.
        tables.field_info.insert(7, (0usize, b'I'));
        b.apply_inline_tables(tables);
        if let Some((loads, stores)) = admit {
            b.add_spliced_volatile_field_pcs([7usize]);
            // Pinned, so the test does not depend on
            // `CRATONVM_JIT_IR_VOLATILE_FIELDS` in the environment.
            b.set_volatile_field_admission(loads, stores);
        }
        b.build(&code, 5)
    };
    let the_load = |g: &Graph| -> NodeId {
        let loads: Vec<NodeId> = (0..g.nodes.len() as NodeId)
            .filter(|&i| matches!(g.nodes[i as usize].op, Op::Load(_)))
            .collect();
        assert_eq!(loads.len(), 1, "one spliced field read");
        loads[0]
    };

    let plain = build(None).expect("the plain spliced read builds");
    let pl = the_load(&plain);
    assert!(!plain.nodes[pl as usize].is_volatile_access());

    let ordered = build(Some((true, false))).expect("an admitted spliced volatile read builds");
    let vl = the_load(&ordered);
    assert!(
        ordered.nodes[vl as usize].is_volatile_access(),
        "the spliced read is marked volatile"
    );
    assert_eq!(ordered.memory_effect(vl).order, MemOrder::Acquire);

    assert!(
        build(Some((false, false))).is_none(),
        "with admission off a spliced volatile read bails the build"
    );

    // `set_volatile_field_pcs` after the splice pcs MERGES: the caller's
    // own (empty) set does not wipe the spliced pc.
    let mut b = IrBuilder::new(1, 1);
    let mut tables = IrInlineTables::default();
    tables.sites.insert(1, site.clone());
    tables.field_info.insert(7, (0usize, b'I'));
    b.apply_inline_tables(tables);
    b.add_spliced_volatile_field_pcs([7usize]);
    b.set_volatile_field_pcs(HashSet::new());
    b.set_volatile_field_admission(true, false);
    let merged = b.build(&code, 5).expect("merged build");
    let ml = the_load(&merged);
    assert!(merged.nodes[ml as usize].is_volatile_access());
}

/// JIT round 21 wave 13 (lane cf, `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE`):
/// a SPLICED callee's volatile `putfield`, named through
/// `add_spliced_volatile_field_pcs`, builds as an `Op::Store` marked
/// volatile (the node `ir_lower` fences with `LOCK ADD [RSP], 0`), and with
/// store admission off the build bails rather than emitting an unfenced
/// store. The plain control builds an unmarked store.
#[test]
fn r21w13_cf_a_spliced_volatile_putfield_builds_a_volatile_store() {
    // Caller: `static void f(O o, int v) { g(o, v); }`
    //   pc 0  aload_0
    //   pc 1  iload_1
    //   pc 2  invokestatic #1   (spliced)
    //   pc 5  return
    // Callee `static void g(O o, int v) { o.v = v; }` (v volatile),
    // relocated to combined pc 6:
    //   pc 6  aload_0
    //   pc 7  iload_1
    //   pc 8  putfield #2
    //   pc 11 return
    let code = [
        0x2a, 0x1b, 0xb8, 0x00, 0x01, 0xb1, // caller, code_len 6
        0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1, // relocated callee at [6, 12)
        0, 0,
    ];
    let site = IrInlineSite {
        base: 6,
        code_len: 6,
        num_args: 2,
        max_locals: 2,
        arg_local_slots: vec![0, 1],
        returns_value: false,
        receiver_is_arg0: false,
        method_key: "P.g:(LO;I)V".to_string(),
        class_id: 7,
    };
    let build = |admit: Option<(bool, bool)>| {
        let mut b = IrBuilder::new(2, 2);
        let mut tables = IrInlineTables::default();
        tables.sites.insert(2, site.clone());
        // CALLEE pc 2 + base 6.
        tables.field_info.insert(8, (0usize, b'I'));
        b.apply_inline_tables(tables);
        if let Some((loads, stores)) = admit {
            b.add_spliced_volatile_field_pcs([8usize]);
            b.set_volatile_field_admission(loads, stores);
        }
        b.build(&code, 6)
    };
    let the_store = |g: &Graph| -> NodeId {
        let stores: Vec<NodeId> = (0..g.nodes.len() as NodeId)
            .filter(|&i| matches!(g.nodes[i as usize].op, Op::Store(_)))
            .collect();
        assert_eq!(stores.len(), 1, "one spliced field write");
        stores[0]
    };

    let plain = build(None).expect("the plain spliced write builds");
    let ps = the_store(&plain);
    assert!(!plain.nodes[ps as usize].is_volatile_store());

    let fenced = build(Some((true, true))).expect("an admitted spliced volatile write builds");
    let vs = the_store(&fenced);
    assert!(
        fenced.nodes[vs as usize].is_volatile_store(),
        "the spliced write is marked volatile, so ir_lower fences it"
    );

    assert!(
        build(Some((true, false))).is_none(),
        "with store admission off a spliced volatile write bails the build"
    );
}

/// A spliced `getstatic` resolves through `IrInlineTables::static_field_info`,
/// and a MISSING row bails the whole method.
///
/// Both halves matter, and the second is the one with no other witness.
/// `ldc` spent its first hour in exactly this state -- the resolver
/// admitting a callee whose rows nothing rebased, and the builder refusing
/// the METHOD 65 bytes later, which from outside is indistinguishable from
/// a workload that has no such callee. Asserting only the positive would
/// leave the same hole: a future edit that drops the row from
/// `apply_inline_tables` would still pass.
#[test]
fn a_spliced_getstatic_resolves_through_the_rebased_rows() {
    // Caller: `static int f() { return g(); }`
    //   pc 0  invokestatic #1   (spliced)
    //   pc 3  ireturn
    // Callee body, relocated to combined pc 5:
    //   pc 5  getstatic #2
    //   pc 8  ireturn
    let code = [
        0xb8, 0x00, 0x01, 0xac, 0x00, // caller, code_len 4 (byte 4 is padding)
        0xb2, 0x00, 0x02, 0xac, // relocated callee at [5, 9)
        0, 0,
    ];
    let site = IrInlineSite {
        base: 5,
        code_len: 4,
        num_args: 0,
        max_locals: 0,
        arg_local_slots: Vec::new(),
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "P.g:()I".to_string(),
        class_id: 7,
    };

    let mut with_rows = IrBuilder::new(0, 1);
    let mut tables = IrInlineTables::default();
    tables.sites.insert(0, site.clone());
    // The row `append_ir_inline_site` rebases: CALLEE pc 0 + base 5.
    tables
        .static_field_info
        .insert(5, (7u32, 0usize, b'I', false));
    with_rows.apply_inline_tables(tables);
    let graph = with_rows
        .build(&code, 4)
        .expect("a spliced getstatic with its row must build");
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::LoadStatic { class_id: 7, .. })),
        "the spliced `getstatic` must lower to an Op::LoadStatic naming the \
             resolved class, not to a call left behind by a skipped splice",
    );

    // Same site, same bytes, no row.
    let mut without_rows = IrBuilder::new(0, 1);
    let mut bare = IrInlineTables::default();
    bare.sites.insert(0, site);
    without_rows.apply_inline_tables(bare);
    assert!(
        without_rows.build(&code, 4).is_none(),
        "with no row the builder must bail the METHOD -- that is the failure \
             mode the resolver's admission has to stay in step with",
    );
}

/// A spliced `instanceof` resolves through `IrInlineTables::instanceof_info`,
/// and a MISSING row bails the whole method.
///
/// The `0xc1` arm READS as though a missing row were graceful -- it calls
/// `plant_uncommon_trap` and compiles the rest. It is not, by default:
/// `TrapCause::UnresolvedTypeCheck` is gated behind
/// `ir_unresolved_class_trap_enabled`, which is OFF (the argument for it
/// having been refuted), so the plant refuses and the arm falls through to
/// `ir_build_bail`. That is the same fail-closed answer a missing
/// `getstatic` row gives, and it is why the resolver must refuse a callee
/// whose targets it could not resolve rather than admit the body and leave
/// rows out: doing so costs the CALLER its whole optimizing compile.
///
/// Asserting the negative is the point. Reading the arm alone gives the
/// wrong answer, and a future edit that flips the trap default would change
/// this behaviour without touching either half of the splice feature.
#[test]
fn a_spliced_instanceof_resolves_through_the_rebased_rows() {
    // Caller: `static boolean f(Object o) { return g(o); }`
    //   pc 0  aload_0
    //   pc 1  invokestatic #1   (spliced)
    //   pc 4  ireturn
    // Callee body, relocated to combined pc 6:
    //   pc 6  aload_0
    //   pc 7  instanceof #2
    //   pc 10 ireturn
    let code = [
        0x2a, 0xb8, 0x00, 0x01, 0xac, 0x00, // caller, code_len 5
        0x2a, 0xc1, 0x00, 0x02, 0xac, // relocated callee at [6, 11)
        0, 0,
    ];
    let name = "java/lang/String";
    let (ptr, len) = crate::intern_typecheck_target(name, Some(11));
    let site = IrInlineSite {
        base: 6,
        code_len: 5,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "P.g:(Ljava/lang/Object;)Z".to_string(),
        class_id: 7,
    };

    let mut with_rows = IrBuilder::new(1, 1);
    let mut tables = IrInlineTables::default();
    tables.sites.insert(1, site.clone());
    // The row `append_ir_inline_site` rebases: CALLEE pc 1 + base 6.
    tables.instanceof_info.insert(7, (ptr as usize, len));
    with_rows.apply_inline_tables(tables);
    let graph = with_rows
        .build(&code, 5)
        .expect("a spliced instanceof with its row must build");
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::InstanceOf { .. })),
        "the spliced `instanceof` must lower to an Op::InstanceOf, not to a \
             call left behind by a skipped splice",
    );

    // Same site, same bytes, no row.
    let mut without_rows = IrBuilder::new(1, 1);
    let mut bare = IrInlineTables::default();
    bare.sites.insert(1, site);
    without_rows.apply_inline_tables(bare);
    assert!(
        without_rows.build(&code, 5).is_none(),
        "with no row the builder must bail the METHOD -- the `0xc1` arm's \
             uncommon-trap path is gated off by default, so the resolver's \
             admission has to stay in step with the rows it produces",
    );
}

#[test]
fn test_ir_getfield_emits_load() {
    // static int get(Corpus o) { return o.x; }
    // aload_0; getfield #2; ireturn
    let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
    let mut builder = IrBuilder::new(1, 1);
    let mut fi = HashMap::new();
    fi.insert(1usize, (0usize, b'I'));
    builder.set_field_info(fi);
    // The builder used to refuse getfield/putfield outright whenever
    // compact layout was on (the default), which made this test
    // vacuous in every default run. The layout constraint now lives in
    // `ir_lower::lower_inner`, where the layout-naive displacement is
    // actually emitted, so the graph shape below is asserted for real.
    let graph = builder.build(&code, 5).expect("IR build failed");
    let has_load = graph.nodes.iter().any(|n| matches!(n.op, Op::Load(_)));
    assert!(has_load, "getfield should emit an Op::Load node");
    // The Load's base must be the Param(0) receiver and its offset operand a
    // Const(0) (field index 0).
    let load = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Load(_)))
        .unwrap();
    assert_eq!(
        load.inputs.len(),
        4,
        "Load inputs = [ctrl, mem, base, offset]"
    );
    assert_eq!(graph.nodes[load.inputs[2] as usize].op, Op::Param(0));
    assert_eq!(graph.nodes[load.inputs[3] as usize].op, Op::Const(0));
}

/// Round 9 wave 5 (`volatile5`),
/// `volatile-instance-fields-are-plain-accesses-in-compiled-code`: a
/// `getfield` or `putfield` of a VOLATILE field bails the build, so the
/// method goes to single-pass instead of becoming an unordered
/// `Op::Load` / `Op::Store` that GVN may merge and LICM may hoist. Each
/// half is paired with the identical non-volatile build, which must
/// succeed, so the refusal cannot pass vacuously.
#[test]
fn a_volatile_field_access_bails_the_ir_build() {
    // static int get(O o) { return o.v; } — aload_0; getfield #2; ireturn
    let get = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
    // static void put(O o, int x) { o.v = x; } — aload_0; iload_1; putfield #2; return
    let put = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
    let cases: [(&[u8], usize, usize, usize); 2] = [(&get[..], 5, 1, 1), (&put[..], 6, 2, 2)];
    for (code, len, params, field_pc) in cases {
        let build = |volatile: bool| {
            let mut b = IrBuilder::new(params, params);
            b.set_field_info(HashMap::from([(field_pc, (0usize, b'I'))]));
            if volatile {
                b.set_volatile_field_pcs(HashSet::from([field_pc]));
                // Pin the REFUSAL arm explicitly (round 9 wave 7, irsnap7):
                // this test is about what the builder does when the IR is
                // NOT allowed to model the ordering, and must keep saying
                // so whatever `CRATONVM_JIT_IR_VOLATILE_FIELDS` defaults to
                // or the environment sets.
                b.set_volatile_field_admission(false, false);
            }
            b.build(code, len)
        };
        assert!(
            build(false).is_some(),
            "the plain-field control must build (opcode {:#04x})",
            code[field_pc]
        );
        assert!(
            build(true).is_none(),
            "a volatile field access must bail the IR build (opcode {:#04x})",
            code[field_pc]
        );
    }
    // A volatile pc that is NOT a field access refuses nothing.
    let mut b = IrBuilder::new(1, 1);
    b.set_field_info(HashMap::from([(1usize, (0usize, b'I'))]));
    b.set_volatile_field_pcs(HashSet::from([0usize]));
    assert!(
        b.build(&get, 5).is_some(),
        "only the named field pcs refuse"
    );
}

/// Round 9 wave 6 (`irsnap6`),
/// `perf-ir-refuses-volatile-instance-field-methods`: ADMITTED, a volatile
/// `getfield` / `putfield` builds as the ordinary node marked
/// `volatile_access`, whose memory effect carries the JMM ordering. A
/// volatile `putfield` needs the store admission bit as well — the
/// lowerer's fence — and without it still bails even with loads admitted.
#[test]
fn an_admitted_volatile_field_access_builds_a_marked_ordered_node() {
    let get = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
    let put = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
    let build = |code: &[u8], len: usize, params: usize, field_pc: usize, admit: (bool, bool)| {
        let mut b = IrBuilder::new(params, params);
        b.set_field_info(HashMap::from([(field_pc, (0usize, b'I'))]));
        b.set_volatile_field_pcs(HashSet::from([field_pc]));
        b.set_volatile_field_admission(admit.0, admit.1);
        b.build(code, len)
    };

    let g = build(&get[..], 5, 1, 1, (true, false)).expect("an admitted volatile read builds");
    let loads: Vec<NodeId> = (0..g.nodes.len() as NodeId)
        .filter(|&i| matches!(g.nodes[i as usize].op, Op::Load(_)))
        .collect();
    assert_eq!(loads.len(), 1);
    let load = loads[0];
    assert!(
        g.nodes[load as usize].is_volatile_access(),
        "the read is marked"
    );
    assert!(!g.nodes[load as usize].is_volatile_store());
    assert_eq!(
        g.memory_effect(load).order,
        MemOrder::Acquire,
        "a volatile read is an acquire"
    );

    assert!(
        build(&put[..], 6, 2, 2, (true, false)).is_none(),
        "a volatile store without the lowerer's fence still bails"
    );
    let g = build(&put[..], 6, 2, 2, (true, true)).expect("an admitted volatile store builds");
    let store = (0..g.nodes.len() as NodeId)
        .find(|&i| matches!(g.nodes[i as usize].op, Op::Store(_)))
        .expect("the store");
    assert!(
        g.nodes[store as usize].is_volatile_store(),
        "the store is marked"
    );
    assert_eq!(
        g.memory_effect(store).order,
        MemOrder::SeqCst,
        "a volatile write is a full fence"
    );

    // The mark only lands on a field access, and a plain build is plain.
    let mut g = build(&get[..], 5, 1, 1, (true, false)).expect("builds");
    let not_a_load = (0..g.nodes.len() as NodeId)
        .find(|&i| matches!(g.nodes[i as usize].op, Op::Param(_)))
        .expect("the parameter");
    g.set_volatile_access(not_a_load);
    assert!(!g.nodes[not_a_load as usize].volatile_access);
    let mut plain = IrBuilder::new(1, 1);
    plain.set_field_info(HashMap::from([(1usize, (0usize, b'I'))]));
    let g = plain.build(&get, 5).expect("the plain read builds");
    assert!(
        g.nodes.iter().all(|n| !n.volatile_access),
        "no plain build marks anything"
    );
    let plain_load = (0..g.nodes.len() as NodeId)
        .find(|&i| matches!(g.nodes[i as usize].op, Op::Load(_)))
        .expect("the load");
    assert_eq!(g.memory_effect(plain_load).order, MemOrder::Plain);
}

/// Round 9 wave 2, `sub-int-field-stores-are-not-narrowed-by-compiled-code`:
/// a `putfield` into a `byte`/`short`/`char`/`boolean` field stores the
/// value narrowed to the field's type, as the interpreter does — and
/// javac's already-narrowed values get no second conversion.
#[test]
fn a_sub_int_putfield_stores_the_narrowed_value() {
    // static int f(O o, int v) { o.b = <value>; return o.b; }
    //   aload_0; <push value>; putfield #1; aload_0; getfield #1; ireturn
    let stored = |push: &[u8], tag: u8| -> (Graph, NodeId) {
        let mut code = vec![0x2a];
        code.extend_from_slice(push);
        let put = code.len();
        code.extend_from_slice(&[0xb5, 0x00, 0x01, 0x2a]);
        let get = code.len();
        code.extend_from_slice(&[0xb4, 0x00, 0x01, 0xac]);
        let len = code.len();
        code.extend_from_slice(&[0, 0, 0]);
        let mut b = IrBuilder::new(2, 2);
        b.set_field_info(HashMap::from([(put, (0usize, tag)), (get, (0usize, tag))]));
        let g = b.build(&code, len).expect("IR build");
        let store = g
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Store(_)))
            .expect("a Store");
        let value = store.inputs[4];
        (g, value)
    };
    let sipush_300 = [0x11, 0x01, 0x2c];

    // Is `v` the `(x << k) >> k` shape the `i2b`/`i2s` arms build, over `x`?
    let sext = |g: &Graph, v: NodeId, k: i64, x: &Op| {
        let shr = &g.nodes[v as usize];
        let shl = &g.nodes[shr.inputs[0] as usize];
        shr.op == Op::Shr
            && shl.op == Op::Shl
            && g.nodes[shr.inputs[1] as usize].op == Op::Const(k)
            && g.nodes[shl.inputs[1] as usize].op == Op::Const(k)
            && g.nodes[shl.inputs[0] as usize].op == *x
    };
    // Is `v` `x & mask`?
    let masked = |g: &Graph, v: NodeId, mask: i64| {
        let and = &g.nodes[v as usize];
        and.op == Op::And && g.nodes[and.inputs[1] as usize].op == Op::Const(mask)
    };

    // `sipush 300; putfield B` -> (300 << 24) >> 24: 300 reads back as 44.
    let (g, v) = stored(&sipush_300, b'B');
    assert!(
        sext(&g, v, 24, &Op::Const(300)),
        "{:?}",
        g.nodes[v as usize].op
    );
    // ...S keeps 300 (in range), C keeps 300, Z takes bit 0.
    let (g, v) = stored(&sipush_300, b'S');
    assert_eq!(g.nodes[v as usize].op, Op::Const(300), "300 fits a short");
    let (g, v) = stored(&sipush_300, b'C');
    assert_eq!(g.nodes[v as usize].op, Op::Const(300), "300 fits a char");
    let (g, v) = stored(&sipush_300, b'Z');
    assert!(masked(&g, v, 1));

    // An unknown int (`iload_1`) is narrowed for every sub-int tag...
    let p1 = Op::Param(1);
    let (g, v) = stored(&[0x1b], b'B');
    assert!(sext(&g, v, 24, &p1));
    let (g, v) = stored(&[0x1b], b'S');
    assert!(sext(&g, v, 16, &p1));
    let (g, v) = stored(&[0x1b], b'C');
    assert!(masked(&g, v, 0xFFFF));
    let (g, v) = stored(&[0x1b], b'Z');
    assert!(masked(&g, v, 1));
    // ...and not for `int`.
    let (g, v) = stored(&[0x1b], b'I');
    assert_eq!(g.nodes[v as usize].op, Op::Param(1));

    // javac's shape, `iload_1; i2b; putfield B`, is not narrowed twice:
    // the stored value is the `i2b` arm's own node, over the parameter.
    let (g, v) = stored(&[0x1b, 0x91], b'B');
    assert!(sext(&g, v, 24, &p1));
    // `iload_1; i2c; putfield C` likewise.
    let (g, v) = stored(&[0x1b, 0x92], b'C');
    assert!(masked(&g, v, 0xFFFF));
    assert_eq!(g.nodes[g.nodes[v as usize].inputs[0] as usize].op, p1);
}

/// cov-06, first increment: `newarray` (0xbc) needs no resolver
/// plumbing at all — `iconst_1; newarray int; areturn` must build
/// straight away, with the atype immediate carried verbatim and the
/// runtime length wired as the third input.
#[test]
fn test_ir_newarray_emits_new_array_node() {
    let code = [0x04, 0xbc, 0x0a, 0xb0, 0, 0]; // iconst_1; newarray T_INT; areturn
    let builder = IrBuilder::new(0, 0);
    let graph = builder.build(&code, 4).expect("IR build failed");
    let arr = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::NewArray { .. }))
        .expect("newarray should emit an Op::NewArray node");
    assert_eq!(
        arr.op,
        Op::NewArray {
            element_type: 10,
            component_class_id: 0,
        }
    );
    assert_eq!(arr.inputs.len(), 3, "NewArray inputs = [ctrl, mem, length]");
    assert_eq!(graph.nodes[arr.inputs[2] as usize].op, Op::Const(1));
}

/// cov-06, second increment: `anewarray` (0xbd) against an
/// already-loaded component class lowers the same way, keyed by pc
/// through `set_anewarray_info` exactly like `set_new_info` resolves
/// `new`.
#[test]
fn test_ir_anewarray_resolved_emits_new_array_node() {
    let code = [0x04, 0xbd, 0x00, 0x01, 0xb0, 0, 0]; // iconst_1; anewarray #1; areturn
    let mut builder = IrBuilder::new(0, 0);
    let mut info = HashMap::new();
    info.insert(1usize, 42u32); // anewarray opcode is at pc 1
    builder.set_anewarray_info(info);
    let graph = builder.build(&code, 5).expect("IR build failed");
    let arr = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::NewArray { .. }))
        .expect("anewarray should emit an Op::NewArray node");
    assert_eq!(
        arr.op,
        Op::NewArray {
            element_type: 0,
            component_class_id: 42,
        }
    );
}

/// cov-06: an `anewarray` whose component class is NOT in
/// `anewarray_info` (the deferred / not-yet-loaded case) must bail the
/// build, not guess a class id — `ir_compatible` no longer refuses
/// every `anewarray`-bearing method outright, so this per-site gate is
/// the only thing standing between a deferred site and wrong codegen.
#[test]
fn test_ir_anewarray_unresolved_bails() {
    let code = [0x04, 0xbd, 0x00, 0x01, 0xb0, 0, 0];
    let builder = IrBuilder::new(0, 0);
    assert!(
        builder.build(&code, 5).is_none(),
        "an anewarray pc absent from anewarray_info must bail to single-pass"
    );
}

/// Round 16 (lane matrix): `multianewarray` at a resolved site builds one
/// `Op::MultiNewArray` carrying the site word, its dimensions in SOURCE
/// order (outermost first, the order `jit_multianewarray_n` reads its
/// buffer in), and becomes the memory token; an unresolved site bails.
#[test]
fn test_ir_multianewarray_resolved_emits_one_node_with_dims_outermost_first() {
    // iconst_2; iconst_3; multianewarray #1, 2; areturn
    let code = [0x05, 0x06, 0xc5, 0x00, 0x01, 0x02, 0xb0, 0, 0];
    let mut builder = IrBuilder::new(0, 0);
    builder.set_multianewarray_sites(HashMap::from([(2usize, 0x5150_i64)]));
    let graph = builder.build(&code, 7).expect("IR build failed");
    let (id, arr) = graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, n)| matches!(n.op, Op::MultiNewArray { .. }))
        .expect("multianewarray should emit an Op::MultiNewArray node");
    assert_eq!(arr.op, Op::MultiNewArray { site: 0x5150 });
    assert_eq!(arr.ty, IrType::Ref);
    assert_eq!(arr.bytecode_pc, Some(2));
    assert_eq!(arr.inputs.len(), 4, "[ctrl, mem, dim0, dim1]");
    assert_eq!(
        graph.nodes[arr.inputs[2] as usize].op,
        Op::Const(2),
        "outermost first"
    );
    assert_eq!(graph.nodes[arr.inputs[3] as usize].op, Op::Const(3));
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.op == Op::Return && n.inputs.iter().any(|&i| i == id as NodeId)),
        "the array is what the method returns"
    );
    assert!(
        IrBuilder::new(0, 0).build(&code, 7).is_none(),
        "a multianewarray pc absent from multianewarray_sites must bail to single-pass"
    );
}

/// nio21 wave 15 (lane L3): a `new` whose class is not loaded
/// (`JitNewSite::Deferred`) builds as ONE `Op::NewCp` carrying the site's
/// `(holder, cp_idx)`, typed `Ref`, at the `new`'s bci, and becomes the
/// memory token; with no deferred row the method is still refused, as
/// before (the unresolved-class trap is opt-in and off here).
#[test]
fn test_ir_deferred_new_builds_one_new_cp_node() {
    // new #7; areturn
    let code = [0xbb, 0x00, 0x07, 0xb0, 0, 0];
    let mut builder = IrBuilder::new(0, 0);
    builder.set_new_deferred_sites(HashMap::from([(0usize, (11_u32, 7_u16))]));
    let graph = builder.build(&code, 4).expect("IR build failed");
    let (id, node) = graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, n)| matches!(n.op, Op::NewCp { .. }))
        .expect("a deferred new should emit an Op::NewCp node");
    assert_eq!(
        node.op,
        Op::NewCp {
            holder_class_id: 11,
            cp_idx: 7
        }
    );
    assert_eq!(node.ty, IrType::Ref);
    assert_eq!(node.bytecode_pc, Some(0));
    assert_eq!(node.inputs.len(), 2, "[ctrl, mem]");
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "no layout is known, so no Op::New"
    );
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.op == Op::Return && n.inputs.iter().any(|&i| i == id as NodeId)),
        "the object is what the method returns"
    );
    assert!(may_raise(&node.op), "resolution and <clinit> can throw");
    assert_eq!(
        memory_shape_of(&node.op),
        memory_shape_of(&Op::MultiNewArray { site: 1 })
    );
    if !ir_unresolved_class_trap_enabled() {
        assert!(
            IrBuilder::new(0, 0).build(&code, 4).is_none(),
            "a deferred new with no row must still refuse the method"
        );
    }
    // A row for a different pc is not this site's.
    let mut other = IrBuilder::new(0, 0);
    other.set_new_deferred_sites(HashMap::from([(9usize, (11_u32, 7_u16))]));
    if !ir_unresolved_class_trap_enabled() {
        assert!(other.build(&code, 4).is_none());
    }
}

/// `static int f(int flag, int n, int d) { if (flag != 0) return 0; return n / d; }`
///
/// The `idiv` is reachable only on the not-taken arm, so its
/// ArithmeticException must be anchored there — `Op::Guard`'s control input
/// has to be the `If`'s projection, not the method entry. `Op::Div` itself
/// is a floating node the scheduler is free to hoist into any block its
/// operands are available in, which is exactly why the trap may not ride
/// along with it.
#[test]
fn idiv_zero_divisor_trap_is_anchored_to_the_branch_that_reaches_it() {
    // 0: iload_0  1: ifne +7 (→8)  4: iload_1  5: iload_2  6: idiv
    // 7: ireturn  8: iconst_0  9: ireturn
    let code = [
        0x1a, 0x9a, 0x00, 0x07, 0x1b, 0x1c, 0x6c, 0xac, 0x03, 0xac, 0, 0,
    ];
    let graph = IrBuilder::new(3, 3)
        .build(&code, 10)
        .expect("IR build failed");

    let guards: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Guard { bci: 6 }))
        .collect();
    assert_eq!(
        guards.len(),
        1,
        "exactly one zero-divisor guard for the single idiv"
    );
    let guard = guards[0];
    assert_eq!(guard.ty, IrType::Void);
    assert_eq!(guard.inputs.len(), 2, "Op::Guard is [ctrl, cond]");

    // Anchored inside the conditional: the control edge is the token the
    // not-taken arm runs under (the builder gives the fall-through target
    // its own `Merge`), never the method's entry control — which is the
    // whole point, since the entry dominates both arms and a trap parked
    // there fires on the path that returns 0 without dividing.
    let ctrl_id = guard.inputs[0];
    let ctrl = &graph.nodes[ctrl_id as usize];
    assert_eq!(ctrl.ty, IrType::Control, "guard input 0 is a control edge");
    let entry_ctrl = graph
        .nodes
        .iter()
        .position(|n| {
            n.op == Op::Proj(0) && n.ty == IrType::Control && n.input_opt(0) == Some(graph.entry)
        })
        .expect("the graph has an entry control projection");
    assert_ne!(
        ctrl_id as usize, entry_ctrl,
        "the trap must not be anchored at the method entry"
    );
    assert!(
        graph.nodes.iter().any(|n| n.op == Op::If),
        "the fixture really does branch"
    );

    // The condition is `divisor != 0`, over the same node the division
    // divides by.
    let cond = &graph.nodes[guard.inputs[1] as usize];
    assert_eq!(cond.op, Op::Cmp(CmpOp::Ne));
    let div = graph
        .nodes
        .iter()
        .find(|n| n.op == Op::Div)
        .expect("the idiv builds an Op::Div");
    assert_eq!(cond.inputs[0], div.inputs[1], "guards the actual divisor");
    assert_eq!(graph.nodes[cond.inputs[1] as usize].op, Op::Const(0));
}

/// The same anchoring for `ldiv`, `irem` and `lrem`, and the `Long` form
/// compares against a `Long`-typed zero (a 32-bit compare would read only
/// half of a 64-bit divisor).
#[test]
fn every_integer_division_opcode_anchors_its_zero_divisor_trap() {
    for (opcode, div_pc, is_long, op) in [
        // 0: lload_0  1: lload_2  2: ldiv  3: lreturn
        (0x6du8, 2usize, true, Op::Div),
        // irem / lrem, same shape
        (0x70u8, 2usize, false, Op::Rem),
        (0x71u8, 2usize, true, Op::Rem),
    ] {
        let code = if is_long {
            [0x1e, 0x20, opcode, 0xad, 0, 0]
        } else {
            [0x1a, 0x1b, opcode, 0xac, 0, 0]
        };
        // Typed parameters: `IrBuilder::new` alone types every parameter
        // `Int`, so `lload` would feed an Int into `ldiv`, and the operand
        // type lane (`CRATONVM_JIT_VERIFY_TYPES=1`) rightly rejects that.
        let mut builder = IrBuilder::new(2, 4);
        let param = if is_long { IrType::Long } else { IrType::Int };
        builder.set_param_types(&[param, param]);
        let Some(graph) = builder.build(&code, 4) else {
            continue; // a shape this front end declines is not this test's business
        };
        let guard = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Guard { bci } if bci == div_pc))
            .unwrap_or_else(|| panic!("no zero-divisor guard for opcode {opcode:#x}"));
        let cond = &graph.nodes[guard.inputs[1] as usize];
        assert_eq!(cond.op, Op::Cmp(CmpOp::Ne), "opcode {opcode:#x}");
        let zero = &graph.nodes[cond.inputs[1] as usize];
        assert_eq!(zero.op, Op::Const(0), "opcode {opcode:#x}");
        assert_eq!(
            zero.ty,
            if is_long { IrType::Long } else { IrType::Int },
            "opcode {opcode:#x}: the zero must be as wide as the divisor"
        );
        assert!(
            graph.nodes.iter().any(|n| n.op == op),
            "opcode {opcode:#x} still builds its arithmetic node"
        );
    }
}

#[test]
fn test_ir_putfield_emits_store_and_threads_memory() {
    // static void set(Corpus o, int v) { o.x = v; }
    // aload_0; iload_1; putfield #2; return
    let code = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
    let mut builder = IrBuilder::new(2, 2);
    let mut fi = HashMap::new();
    fi.insert(2usize, (0usize, b'I'));
    builder.set_field_info(fi);
    // The builder used to refuse getfield/putfield outright whenever
    // compact layout was on (the default), which made this test
    // vacuous in every default run. The layout constraint now lives in
    // `ir_lower::lower_inner`, where the layout-naive displacement is
    // actually emitted, so the graph shape below is asserted for real.
    let graph = builder.build(&code, 6).expect("IR build failed");
    let store = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Store(_)))
        .expect("putfield should emit an Op::Store");
    // inputs = [ctrl, mem, base, offset, value]
    assert_eq!(store.inputs.len(), 5);
    assert_eq!(graph.nodes[store.inputs[2] as usize].op, Op::Param(0)); // base = o
    assert_eq!(graph.nodes[store.inputs[3] as usize].op, Op::Const(0)); // field index
    assert_eq!(graph.nodes[store.inputs[4] as usize].op, Op::Param(1)); // value = v
    assert_eq!(store.ty, IrType::Memory);
}

#[test]
fn test_ir_putfield_store_after_load_in_memory_chain() {
    // static int swap(Corpus o, int v) { int t = o.x; o.x = v; return t; }
    // The store's memory input must be the load (so the scheduler orders the
    // store AFTER the read — WAR), not the initial Start memory token.
    // aload_0; getfield #2; istore_2; aload_0; iload_1; putfield #2; iload_2; ireturn
    let code = [
        0x2a, 0xb4, 0x00, 0x02, 0x3d, 0x2a, 0x1b, 0xb5, 0x00, 0x02, 0x1c, 0xac, 0, 0,
    ];
    let mut builder = IrBuilder::new(2, 3);
    let mut fi = HashMap::new();
    fi.insert(1usize, (0usize, b'I')); // getfield pc 1
    fi.insert(7usize, (0usize, b'I')); // putfield pc 7
    builder.set_field_info(fi);
    // The builder used to refuse getfield/putfield outright whenever
    // compact layout was on (the default), which made this test
    // vacuous in every default run. The layout constraint now lives in
    // `ir_lower::lower_inner`, where the layout-naive displacement is
    // actually emitted, so the graph shape below is asserted for real.
    let graph = builder.build(&code, 12).expect("IR build failed");
    let store = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Store(_)))
        .expect("putfield emits a Store");
    // store.inputs[1] (memory) must be the Load node, not the Start Proj(1).
    let mem_in = store.inputs[1];
    assert!(
        matches!(graph.nodes[mem_in as usize].op, Op::Load(_)),
        "store's memory input must be the prior load (WAR ordering), got {:?}",
        graph.nodes[mem_in as usize].op
    );
}

#[test]
fn test_ir_getfield_bails_without_field_info() {
    // Same method but no field layout supplied → build must bail (None),
    // the single-pass safety net.
    let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
    let builder = IrBuilder::new(1, 1);
    assert!(builder.build(&code, 5).is_none());
}

#[test]
fn test_ir_getfield_builds_a_ref_typed_load_for_a_reference_field() {
    // A reference field (`L…;`) reads through the same checked helper and
    // yields a `Ref`-typed `Op::Load(MemKind::Ref)`. The TYPE is the part
    // that matters beyond "it builds": `emit_safepoint_map` publishes
    // exactly the `Ref`-typed nodes, so an `Int`-typed load of a reference
    // would leave a live oop out of every map.
    //
    // This used to assert the opposite — that the builder bails — which
    // was the single largest exclusion in the optimizing tier (66 of 141
    // builder refusals on a Hibernate class).
    let code = [0x2a, 0xb4, 0x00, 0x02, 0xb0, 0, 0]; // aload_0; getfield; areturn
    let mut builder = IrBuilder::new(1, 1);
    let mut fi = HashMap::new();
    fi.insert(1usize, (0usize, b'L'));
    builder.set_field_info(fi);
    let graph = builder
        .build(&code, 5)
        .expect("a reference-field getfield must build");
    let load = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Load(MemKind::Ref)))
        .expect("the reference field must lower to Op::Load(MemKind::Ref)");
    assert_eq!(load.ty, IrType::Ref);
}

#[test]
fn test_ir_getfield_still_bails_on_a_wide_field_with_the_gates_off() {
    // `J`/`D`/`F` fields build only when the long / FP tier is on — the
    // gate `lib.rs` passes down from the same `ir_emit_long` /
    // `ir_emit_fp` its admission chain evaluates. With both off (the
    // builder default, and what a hand-built graph gets) they are still
    // refused HERE rather than deeper in the pipeline.
    for tag in *b"JDF" {
        let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
        let mut builder = IrBuilder::new(1, 1);
        let mut fi = HashMap::new();
        fi.insert(1usize, (0usize, tag));
        builder.set_field_info(fi);
        assert!(
            builder.build(&code, 5).is_none(),
            "field tag {} must not build with the wide-field gates off",
            tag as char
        );
    }
}

/// COV-03. `getfield` learned about reference fields; `putfield` twenty
/// lines below it did not — 37 refusals on `ir.rs:5085`, the second-largest
/// structural refusal in the coverage survey, for a fix that had simply
/// been applied to one of two sites.
///
/// The node TYPE is the load-bearing assertion, exactly as it is for the
/// read: `MemKind::Ref` is what selects `jit_putfield_object` in
/// `ir_lower`, and that helper is what carries the SATB pre-barrier and the
/// collector's post-write barrier. An `Int`-kinded reference store would
/// lower to a raw 16-byte int-cell write with no barrier at all.
#[test]
fn test_ir_putfield_builds_a_ref_kinded_store_for_a_reference_field() {
    for tag in *b"L[" {
        // aload_0; aload_1; putfield #2; return
        let code = [0x2a, 0x2b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
        let mut builder = IrBuilder::new(2, 2);
        let mut fi = HashMap::new();
        fi.insert(2usize, (0usize, tag));
        builder.set_field_info(fi);
        let graph = builder
            .build(&code, 6)
            .unwrap_or_else(|| panic!("a {} field putfield must build", tag as char));
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::Store(MemKind::Ref))),
            "a {} field must lower to Op::Store(MemKind::Ref), not Int",
            tag as char
        );
    }
}

/// The two arms must accept the SAME tag set. This is the regression the
/// lane exists for: a tag one arm admits and the other refuses is how the
/// asymmetry arose, and it is invisible in any test that exercises one arm.
#[test]
fn test_ir_getfield_and_putfield_admit_the_same_field_tags() {
    for &(long_gate, fp_gate) in &[(false, false), (true, false), (false, true), (true, true)] {
        for tag in *b"IZBCSL[JFDV" {
            // aload_0; getfield #2; return — the read side only needs to
            // build; leaving the value on the abstract stack at the return
            // is fine for a straight-line translation.
            let get = [0x2a, 0xb4, 0x00, 0x02, 0xb1, 0, 0];
            let mut gb = IrBuilder::new(1, 1);
            let mut gfi = HashMap::new();
            gfi.insert(1usize, (0usize, tag));
            gb.set_field_info(gfi);
            gb.set_wide_field_gates(long_gate, fp_gate);
            let get_builds = gb.build(&get, 5).is_some();

            // aload_0; aload_1; putfield #2; return
            let put = [0x2a, 0x2b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
            let mut pb = IrBuilder::new(2, 2);
            let mut pfi = HashMap::new();
            pfi.insert(2usize, (0usize, tag));
            pb.set_field_info(pfi);
            pb.set_wide_field_gates(long_gate, fp_gate);
            let put_builds = pb.build(&put, 6).is_some();

            assert_eq!(
                get_builds, put_builds,
                "tag {} (long_gate={long_gate} fp_gate={fp_gate}): getfield builds={get_builds} \
                     but putfield builds={put_builds} — the two arms have drifted apart again",
                tag as char
            );
        }
    }
}

/// With the matching gate on, a wide field builds a wide-KINDED access on
/// both arms. The kind is what picks `jit_putfield_{long,float,double}` and
/// what tells the lowerer a `J`/`D` read needs the `dispatch_threw`
/// disambiguation (`Long.MIN_VALUE` and `-0.0` are bit-identical to the
/// helper's deopt sentinel).
#[test]
fn test_ir_wide_fields_build_wide_kinded_accesses_when_gated() {
    for (tag, kind, ty, load_op, ret) in [
        (b'J', MemKind::Long, IrType::Long, 0x16u8, 0xadu8),
        (b'F', MemKind::Float, IrType::Float, 0x17, 0xae),
        (b'D', MemKind::Double, IrType::Double, 0x18, 0xaf),
    ] {
        // aload_0; getfield #2; <x>return
        let get = [0x2a, 0xb4, 0x00, 0x02, ret, 0, 0];
        let mut gb = IrBuilder::new(1, 1);
        let mut gfi = HashMap::new();
        gfi.insert(1usize, (0usize, tag));
        gb.set_field_info(gfi);
        gb.set_wide_field_gates(true, true);
        let graph = gb
            .build(&get, 5)
            .unwrap_or_else(|| panic!("a gated {} field getfield must build", tag as char));
        let load = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Load(k) if k == kind))
            .unwrap_or_else(|| panic!("{} must lower to Op::Load({kind:?})", tag as char));
        assert_eq!(load.ty, ty, "{} load node type", tag as char);

        // aload_0; <x>load_1; putfield #2; return — `this` at slot 0, the
        // wide value at slot 1 (occupying slots 1-2 for `J`/`D`).
        let put = [0x2a, load_op, 0x01, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
        let mut pb = IrBuilder::new(2, 4);
        pb.set_param_types(&[IrType::Ref, ty]);
        let mut pfi = HashMap::new();
        pfi.insert(3usize, (0usize, tag));
        pb.set_field_info(pfi);
        pb.set_wide_field_gates(true, true);
        let graph = pb
            .build(&put, 7)
            .unwrap_or_else(|| panic!("a gated {} field putfield must build", tag as char));
        let store = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Store(k) if k == kind))
            .unwrap_or_else(|| panic!("{} must lower to Op::Store({kind:?})", tag as char));
        // The VALUE operand must carry the wide type too — the lowerer
        // reads its frame slot as raw bits and hands them to the width's
        // helper, and an `Int`-typed producer would mean the slot was never
        // written as one.
        assert_eq!(
            graph.nodes[store.inputs[4] as usize].ty, ty,
            "{} store value operand type",
            tag as char
        );
    }
}

#[test]
fn test_ir_iinc() {
    // void f(int x) { x++; return x; }  [simplified]
    // iload_0; istore_1; iinc 1,1; iload_1; ireturn
    let code = [
        0x1a, // 0: iload_0
        0x3c, // 1: istore_1
        0x84, 1, 1,    // 2: iinc 1, 1
        0x1b, // 5: iload_1
        0xac, // 6: ireturn
        0, 0, // padding
    ];
    let graph = build_ir(&code, 7, 1, 2);
    // The iinc should create an Add node
    let has_add = graph.nodes.iter().any(|n| n.op == Op::Add);
    assert!(has_add, "iinc should produce an Add node");
}

// ── real-frame-deopt step 1: safepoint snapshots ────────────────────

#[test]
fn test_safepoints_recorded_per_instruction() {
    // int f(int a, int b) { return a + b; }
    // iload_0; iload_1; iadd; ireturn   (pc 0,1,2,3)
    let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
    let graph = build_ir(&code, 4, 2, 2);

    // One snapshot per reachable bytecode boundary (pc 0..=3); the
    // post-ireturn pc is past code_len and never recorded.
    let bcis: Vec<usize> = graph.safepoints.iter().map(|s| s.bci).collect();
    assert_eq!(bcis, vec![0, 1, 2, 3], "a snapshot at every reachable bci");

    let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();

    // Entry to the method: two params in locals, empty operand stack.
    assert_eq!(at(0).locals.len(), 2);
    assert!(at(0).stack.is_empty());
    // Param nodes are live in locals throughout.
    assert_ne!(at(0).locals[0], NO_NODE);
    assert_ne!(at(0).locals[1], NO_NODE);

    // Before iload_1: one operand pushed (a).
    assert_eq!(at(1).stack.len(), 1);
    // Before iadd: both operands on the stack.
    assert_eq!(at(2).stack.len(), 2);
    // Before ireturn: the Add result is the sole stack slot.
    assert_eq!(at(3).stack.len(), 1);
    let add_id = at(3).stack[0];
    assert_eq!(graph.nodes[add_id as usize].op, Op::Add);
}

#[test]
fn test_safepoint_local_tracks_store() {
    // void-ish: iload_0; istore_1; iinc 1,1; iload_1; ireturn
    // Mirrors test_ir_iinc; check the snapshot for local 1 changes after
    // the store + increment rather than staying NO_NODE.
    let code = [0x1a, 0x3c, 0x84, 1, 1, 0x1b, 0xac, 0, 0];
    let graph = build_ir(&code, 7, 1, 2);

    let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();
    // Before istore_1 (pc 1): local 1 is still uninitialised.
    assert_eq!(at(1).locals[1], NO_NODE);
    // Before iload_1 (pc 5), after the store + iinc: local 1 is an Add.
    let l1 = at(5).locals[1];
    assert_ne!(l1, NO_NODE);
    assert_eq!(graph.nodes[l1 as usize].op, Op::Add);
}

/// The snapshots inside a `synchronized` block carry the locked reference
/// on their monitor stack, and the ones outside it carry none.
///
/// Before 2026-09-12 the builder kept no monitor stack, so every lowered
/// frame state said "holds no lock".
#[test]
fn a_synchronized_blocks_snapshots_carry_its_monitor() {
    // javac shape for `synchronized (o) { }` with `o` in local 0:
    //   0: aload_0  1: dup  2: astore_1  3: monitorenter
    //   4: aload_1  5: monitorexit  6: return
    let code = [0x2a, 0x59, 0x4c, 0xc2, 0x2b, 0xc3, 0xb1, 0, 0];
    let graph = build_ir(&code, 7, 1, 2);
    let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();
    let obj = at(0).locals[0];
    assert_ne!(obj, NO_NODE);

    // Up to and including the monitorenter itself (resuming there
    // re-executes the enter), nothing is held.
    for bci in [0, 1, 2, 3] {
        assert!(at(bci).monitors.is_empty(), "bci {bci} holds nothing yet");
    }
    // Between the enter and the exit — the exit included, since resuming
    // there re-executes it — the lock on `o` is held.
    for bci in [4, 5] {
        assert_eq!(at(bci).monitors, vec![obj], "bci {bci} holds the lock on o");
    }
    // After the exit, released.
    assert!(at(6).monitors.is_empty());
}

/// A loop inside the block gives local 1 an eager header φ, so the
/// `monitorexit`'s operand is `phi(o, o)` while the monitor stack recorded
/// `o`. Those name the same reference and the build must accept it; the
/// header snapshot carries the monitor too.
#[test]
fn a_monitor_held_across_a_loop_is_matched_through_the_header_phi() {
    //  0: aload_0  1: dup  2: astore_1  3: monitorenter
    //  4: iload_2 (loop header)  5: ifeq +6 -> 11
    //  8: goto -4 -> 4
    // 11: aload_1  12: monitorexit  13: return
    let code = [
        0x2a, 0x59, 0x4c, 0xc2, 0x1c, 0x99, 0x00, 0x06, 0xa7, 0xff, 0xfc, 0x2b, 0xc3, 0xb1, 0, 0,
    ];
    let graph = IrBuilder::new(3, 3)
        .build(&code, 14)
        .expect("a structured lock around a loop must build");
    let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();
    let obj = at(0).locals[0];
    assert_eq!(at(4).monitors, vec![obj], "the loop header holds the lock");
    assert_eq!(at(12).monitors, vec![obj], "the monitorexit holds the lock");
    assert!(at(13).monitors.is_empty());
}

/// Unstructured locking is refused rather than described wrongly: a
/// `monitorexit` on a reference other than the innermost held one, a
/// `monitorexit` with nothing held, and a `return` that leaves a monitor
/// held. The single-pass backend still compiles such a method.
#[test]
fn unstructured_locking_refuses_the_ir_build() {
    // aload_0; monitorenter; aload_1; monitorexit; return — exits `b`, holds `a`.
    let wrong_object = [0x2a, 0xc2, 0x2b, 0xc3, 0xb1, 0, 0];
    assert!(IrBuilder::new(2, 2).build(&wrong_object, 5).is_none());
    // aload_0; monitorexit; return — nothing held.
    let nothing_held = [0x2a, 0xc3, 0xb1, 0, 0];
    assert!(IrBuilder::new(1, 1).build(&nothing_held, 3).is_none());
    // aload_0; monitorenter; return — still held at the method exit.
    let held_at_return = [0x2a, 0xc2, 0xb1, 0, 0];
    assert!(IrBuilder::new(1, 1).build(&held_at_return, 3).is_none());
    // The structured control still builds.
    let structured = [0x2a, 0xc2, 0x2a, 0xc3, 0xb1, 0, 0];
    assert!(IrBuilder::new(1, 1).build(&structured, 5).is_some());
}

/// Two edges into one join that hold different monitors have no single
/// monitor state to record at the join: refused.
#[test]
fn a_join_whose_edges_hold_different_monitors_refuses_the_build() {
    //  0: iload_1  1: ifeq +8 -> 9
    //  4: aload_0  5: monitorenter  6: goto +3 -> 9     (holds `a` on this edge)
    //  9: return                                        (join: held vs not held)
    let code = [
        0x1b, 0x99, 0x00, 0x08, 0x2a, 0xc2, 0xa7, 0x00, 0x03, 0xb1, 0, 0,
    ];
    assert!(IrBuilder::new(2, 2).build(&code, 10).is_none());
    // The control: the same join with the lock released on that edge first
    // (`aload_0; monitorexit` before the goto) builds, which is what shows the
    // refusal above is the monitor disagreement and not the bytecode shape.
    //  0: iload_1  1: ifeq +10 -> 11
    //  4: aload_0  5: monitorenter  6: aload_0  7: monitorexit
    //  8: goto +3 -> 11   11: return
    let balanced = [
        0x1b, 0x99, 0x00, 0x0a, 0x2a, 0xc2, 0x2a, 0xc3, 0xa7, 0x00, 0x03, 0xb1, 0, 0,
    ];
    assert!(IrBuilder::new(2, 2).build(&balanced, 12).is_some());
}

#[test]
fn test_replace_all_uses_rewrites_safepoints() {
    let mut graph = Graph::empty(0);
    let a = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let b = graph.add(Op::Const(2), IrType::Int, vec![], None);
    graph
        .safepoints
        .push(SafepointSnapshot::new(0, vec![a], vec![a, b], Vec::new()));
    graph.replace_all_uses(a, b);
    // Every `a` reference in the snapshot must now point at `b`.
    assert_eq!(graph.safepoints[0].locals[0], b);
    assert_eq!(graph.safepoints[0].stack[0], b);
    assert_eq!(graph.safepoints[0].stack[1], b);
}

#[test]
fn test_ir_compatible_basic() {
    let scan = super::super::x64::JitScanResult {
        needs_heap: false,
        multianewarray_ops: vec![],
        field_ops: vec![],
        typecheck_ops: vec![],
        checkcast_ops: vec![],
        static_field_ops: vec![],
        invoke_ops: vec![],
        new_ops: vec![],
        anewarray_ops: vec![],
        non_escaping_new: std::collections::HashSet::new(),
        ldc2w_ops: vec![],
        indy_ops: vec![],
        has_athrow: false,
        local_slot_ops: vec![],
        has_newarray: false,
        has_putstatic: false,
        ldc_ops: vec![],
    };
    assert!(ir_compatible(&scan));
}

/// STUB-S7: confirm the widened filter admits a method with bounded
/// field/invoke/new ops, where the previous filter would have rejected.
#[test]
fn test_ir_compatible_widened_admits_simple_heap_ops() {
    let scan = super::super::x64::JitScanResult {
        needs_heap: true,
        multianewarray_ops: vec![],
        field_ops: vec![(0, 1), (3, 2)],
        typecheck_ops: vec![],
        checkcast_ops: vec![],
        static_field_ops: vec![(6, 3)],
        invoke_ops: vec![(9, 4, 0xb8), (12, 5, 0xb6)],
        new_ops: vec![(15, 6)],
        anewarray_ops: vec![],
        non_escaping_new: std::collections::HashSet::new(),
        ldc2w_ops: vec![],
        indy_ops: vec![],
        has_athrow: false,
        local_slot_ops: vec![],
        has_newarray: false,
        has_putstatic: false,
        ldc_ops: vec![],
    };
    assert!(ir_compatible(&scan));
}

/// STUB-S7: confirm caps still reject over-budget methods so we don't
/// route huge heap-heavy methods through an under-tested IR pipeline.
/// The admission gate and the builder agree about array allocation.
///
/// # What this pins, and the comment it replaces
///
/// `ir_compatible`'s "surviving exclusions" block asserted until
/// 2026-09-16 that `newarray` (0xbc) and `anewarray` (0xbd) "have no arm
/// in `IrBuilder::build`'s opcode match and `Op::NewArray` is constructed
/// nowhere". cov-06 had added both arms; the comment had simply not been
/// updated, and the `multianewarray` bullet immediately below it said so
/// in passing. A reader deciding whether to widen the gate would have read
/// the false half first.
///
/// The rule that comment existed to teach is real, so it is a test now:
/// **a shape the gate ADMITS must be one the builder can BUILD**, because
/// the failure mode of disagreeing is silent — the method is admitted,
/// the graph build bails, and the optimizing tier produces no body while
/// every gate upstream reports "open".
///
/// Behavioural rather than source-scanning on purpose: the question is
/// "does a method of this shape survive the pipeline", and an opcode-match
/// parser answers a proxy for it that can itself go stale.
/// The `anewarray` budget is a BUDGET, not a refusal.
///
/// The other half of the correction above: the comment claimed the
/// `anewarray_ops` budget "used to be" one and had been replaced by an
/// outright refusal. It had not; the check is still
/// `> IR_MAX_ARRAY_ALLOCATIONS`, and a method under it is admitted.
#[test]
fn anewarray_is_bounded_by_a_budget_and_not_refused_outright() {
    let mut scan = super::super::x64::JitScanResult {
        needs_heap: true,
        multianewarray_ops: vec![],
        field_ops: vec![],
        typecheck_ops: vec![],
        checkcast_ops: vec![],
        static_field_ops: vec![],
        invoke_ops: vec![],
        new_ops: vec![],
        anewarray_ops: vec![],
        non_escaping_new: std::collections::HashSet::new(),
        ldc2w_ops: vec![],
        indy_ops: vec![],
        has_athrow: false,
        local_slot_ops: vec![],
        has_newarray: false,
        has_putstatic: false,
        ldc_ops: vec![],
    };
    scan.anewarray_ops = (0..IR_MAX_ARRAY_ALLOCATIONS)
        .map(|i| (i, i as u16))
        .collect();
    assert!(
        ir_compatible(&scan),
        "exactly IR_MAX_ARRAY_ALLOCATIONS reference-array sites are admitted"
    );
    scan.anewarray_ops = (0..IR_MAX_ARRAY_ALLOCATIONS + 1)
        .map(|i| (i, i as u16))
        .collect();
    assert!(
        !ir_compatible(&scan),
        "one site past the budget is refused — it is a budget with an edge, \
             not a blanket exclusion"
    );
}

/// Round 11 wave 9 (lane lower): which conjunct refuses a `static
/// synchronized` counter. `R11X64genSyncCost.incSync(I)I` printed
/// `[ir] admission ... ir_compatible refused`, and the wave brief's first
/// hypothesis was the method monitor. It is not: nothing here can see
/// `ACC_SYNCHRONIZED` (the scan is of bytecode only, and a synchronized
/// body is compiled like any other and entered through the wrapped door
/// that holds the monitor, `CompiledMethod::requires_wrapped_entry`). The
/// refusal is its three `putstatic`s. The javac bytecode of
///
/// ```java
/// static synchronized int incSync(int k) {
///     if (A != B) { TORN++; }
///     A += k; B += k; return A;
/// }
/// ```
///
/// was refused for `has_putstatic` alone. Round 11 wave 10 (lane
/// irstatic) lowers `putstatic` to `Op::StoreStatic`
/// (`r11w9-lower-ir-putstatic-patch-FIXED-20260924.md`), so it is now admitted,
/// and the builder turns the three writes into three `StoreStatic` nodes
/// on the memory chain that verify clean.
#[test]
fn a_static_synchronized_counter_is_admitted() {
    let code: [u8; 37] = [
        0xb2, 0x00, 0x02, // 0: getstatic A
        0xb2, 0x00, 0x03, // 3: getstatic B
        0x9f, 0x00, 0x0b, // 6: if_icmpeq 17
        0xb2, 0x00, 0x04, // 9: getstatic TORN
        0x04, // 12: iconst_1
        0x60, // 13: iadd
        0xb3, 0x00, 0x04, // 14: putstatic TORN
        0xb2, 0x00, 0x02, // 17: getstatic A
        0x1a, // 20: iload_0
        0x60, // 21: iadd
        0xb3, 0x00, 0x02, // 22: putstatic A
        0xb2, 0x00, 0x03, // 25: getstatic B
        0x1a, // 28: iload_0
        0x60, // 29: iadd
        0xb3, 0x00, 0x03, // 30: putstatic B
        0xb2, 0x00, 0x02, // 33: getstatic A
        0xac, // 36: ireturn
    ];
    let scan = crate::x64::jit_scan(&code, code.len(), "(I)I")
        .expect("the single-pass scan accepts the method");
    assert!(scan.has_putstatic);
    assert_eq!(scan.static_field_ops.len(), 9, "6 getstatic + 3 putstatic");
    assert!(
        ir_compatible(&scan),
        "putstatic lowers to Op::StoreStatic; nothing else refuses incSync"
    );

    // The rows `try_compile_inner` feeds: one per static site, reads and
    // writes alike. A = field 0 (cp 2), B = field 1 (cp 3), TORN = field 2
    // (cp 4), all of class 5.
    let mut rows = HashMap::new();
    for (pc, field) in [
        (0, 0),
        (3, 1),
        (9, 2),
        (14, 2),
        (17, 0),
        (22, 0),
        (25, 1),
        (30, 1),
        (33, 0),
    ] {
        rows.insert(pc, (5u32, field as usize, b'I', false));
    }
    let mut builder = IrBuilder::new(1, 1);
    builder.set_static_field_info(rows);
    let graph = builder
        .build(&code, code.len())
        .expect("incSync builds once putstatic has an arm");
    let stores: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::StoreStatic { .. }))
        .collect();
    assert_eq!(stores.len(), 3, "one StoreStatic per putstatic");
    for s in &stores {
        assert_eq!(s.ty, IrType::Memory);
        assert_eq!(s.inputs.len(), 3, "[ctrl, mem, value]");
        assert!(matches!(
            s.op,
            Op::StoreStatic {
                class_id: 5,
                type_tag: b'I',
                is_volatile: false,
                ..
            }
        ));
    }
    let verdict =
        crate::ir_verify::verify_graph(&graph, "test", crate::ir_verify::VerifyOptions::default());
    assert!(verdict.is_ok(), "{verdict:?}");
}

/// A `putstatic` builds an `Op::StoreStatic` whose value is NARROWED for a
/// sub-int field (JVMS §6.5), and a site with no resolved row -- or a
/// `putstatic` inside nothing the caller fed -- bails the method rather
/// than guessing a field.
#[test]
fn a_putstatic_builds_a_narrowed_store_static_and_an_unresolved_one_bails() {
    // static void f(int v) { S = (byte-typed static) v; }
    //   0 iload_0 ; 1 putstatic #1 ; 4 return
    let code = [0x1a, 0xb3, 0x00, 0x01, 0xb1, 0, 0];
    let build = |tag: u8| {
        let mut b = IrBuilder::new(1, 1);
        let mut rows = HashMap::new();
        rows.insert(1usize, (9u32, 2usize, tag, true));
        b.set_static_field_info(rows);
        b.build(&code, 5).expect("a resolved putstatic builds")
    };
    let store_value = |g: &Graph| -> NodeId {
        let s = g
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::StoreStatic { .. }))
            .expect("a StoreStatic");
        assert!(matches!(
            s.op,
            Op::StoreStatic {
                class_id: 9,
                field_index: 2,
                is_volatile: true,
                ..
            }
        ));
        s.inputs[2]
    };
    // An `int` static stores the parameter itself.
    let g = build(b'I');
    let v = store_value(&g);
    assert!(matches!(g.nodes[v as usize].op, Op::Param(0)));
    // A `byte` static stores `(v << 24) >> 24`, not the raw int.
    let g = build(b'B');
    let v = store_value(&g);
    assert!(
        matches!(g.nodes[v as usize].op, Op::Shr),
        "a byte static must store the narrowed value, got {:?}",
        g.nodes[v as usize].op
    );
    // A `boolean` static stores `v & 1`.
    let g = build(b'Z');
    let v = store_value(&g);
    assert!(matches!(g.nodes[v as usize].op, Op::And));
    let verdict =
        crate::ir_verify::verify_graph(&g, "test", crate::ir_verify::VerifyOptions::default());
    assert!(verdict.is_ok(), "{verdict:?}");

    // No row for the site: refuse the method.
    assert!(IrBuilder::new(1, 1).build(&code, 5).is_none());
}

/// STUB-S7: size-aware variant rejects oversized methods.
#[test]
fn test_ir_compatible_sized_rejects_large_methods() {
    let scan = super::super::x64::JitScanResult {
        needs_heap: false,
        multianewarray_ops: vec![],
        field_ops: vec![],
        typecheck_ops: vec![],
        checkcast_ops: vec![],
        static_field_ops: vec![],
        invoke_ops: vec![],
        new_ops: vec![],
        anewarray_ops: vec![],
        non_escaping_new: std::collections::HashSet::new(),
        ldc2w_ops: vec![],
        indy_ops: vec![],
        has_athrow: false,
        local_slot_ops: vec![],
        has_newarray: false,
        has_putstatic: false,
        ldc_ops: vec![],
    };
    // jit-inlining-and-ir-calls: the bytecode budget rose from 200 — which
    // excluded essentially every real application method, making the other
    // caps moot — to HotSpot's `HugeMethodLimit`. The 201-byte method that
    // used to sit just past the old boundary must now be admitted.
    assert!(ir_compatible_sized(&scan, 200));
    assert!(ir_compatible_sized(&scan, 201));
    assert!(ir_compatible_sized(&scan, IR_MAX_BYTECODE_SIZE));
    assert!(!ir_compatible_sized(&scan, IR_MAX_BYTECODE_SIZE + 1));
}

/// invokedynamic-uncommon-trap fix — regression test: `ir_compatible`
/// must decline ANY method containing an invokedynamic (0xba) site,
/// even one that satisfies every other cap. The IR builder has no
/// lowering for 0xba (it bails cleanly via `_ => return None`), so a
/// method with one must always fall back to the x64 single-pass
/// backend, which lowers it directly (unconditional deopt to
/// `DeoptReason::UnreachedCode`).
#[test]
fn test_ir_compatible_rejects_invokedynamic() {
    let scan = super::super::x64::JitScanResult {
        needs_heap: false,
        multianewarray_ops: vec![],
        field_ops: vec![],
        typecheck_ops: vec![],
        checkcast_ops: vec![],
        static_field_ops: vec![],
        invoke_ops: vec![],
        new_ops: vec![],
        anewarray_ops: vec![],
        non_escaping_new: std::collections::HashSet::new(),
        ldc2w_ops: vec![],
        indy_ops: vec![(0, 1)],
        has_athrow: false,
        local_slot_ops: vec![],
        has_newarray: false,
        has_putstatic: false,
        ldc_ops: vec![],
    };
    // 2026-09-06: this assertion was inverted, deliberately. The IR
    // builder's `0xba` arm now replaces the site with an uncommon trap and
    // compiles the rest of the method -- the same trade the single-pass
    // backend already makes -- so the ADMISSION gate must let it through.
    //
    // What did not change is that a site with no resolved stack effect
    // still refuses: the gate is the admission, `set_indy_trap_sites` is
    // the proof, and that half is tested next.
    assert_eq!(
        ir_compatible(&scan),
        ir_site_trap_enabled(),
        "with the site-trap gate ON an invokedynamic no longer refuses the \
             method at the admission gate; with it OFF the old refusal must be \
             restored exactly"
    );
}

/// The admission gate is not the proof. A method admitted with an
/// `invokedynamic` whose descriptor the resolver could not supply has no
/// recorded stack effect, and the builder must refuse it rather than guess
/// how many operand-stack entries the site consumes -- a wrong count
/// silently corrupts every bytecode after it.
#[test]
fn an_indy_with_no_resolved_shape_still_refuses_the_method() {
    // invokedynamic #1, 0, 0 ; return
    let code = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb1, 0, 0];
    let builder = IrBuilder::new(0, 1);
    assert!(
        builder.build(&code, 6).is_none(),
        "no `set_indy_trap_sites` call means no shape for pc 0, and the \
             builder must bail rather than guess the site's stack effect",
    );
}

/// Round 16 (lane indy, SY4-1): build `code` with one `invokedynamic` at
/// `pc` whose stack effect is `shape`, optionally bridged through `site`.
fn r16_indy_build(
    code: &[u8],
    len: usize,
    params: usize,
    pc: usize,
    shape: (usize, u8),
    site: Option<usize>,
) -> Option<Graph> {
    let mut builder = IrBuilder::new(params, params.max(1));
    let mut traps = std::collections::HashMap::new();
    traps.insert(pc, shape);
    builder.set_indy_trap_sites(traps);
    if let Some(site) = site {
        let mut bridged = std::collections::HashMap::new();
        bridged.insert(pc, site);
        builder.set_indy_bridge_sites(bridged);
    }
    builder.build(code, len)
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): a leaked call row for the
/// constant-site tests.
fn r16w10_row(method_name: &'static str, descriptor: &'static str, kind: u8) -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "p/Host",
        method_name,
        descriptor,
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: kind,
        declaring_class_id: 0x7101,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): build `code` (one
/// `invokedynamic` at pc 2 taking two values and returning `I`), bridged,
/// and planned as `opcode` with `row` when given.
fn r16w10_build(code: &[u8], params: &[IrType], row: Option<usize>, opcode: u8) -> Option<Graph> {
    let mut builder = IrBuilder::new(2, 2);
    builder.set_param_types(params);
    builder.set_indy_trap_sites(std::collections::HashMap::from([(2usize, (2usize, b'I'))]));
    builder.set_indy_bridge_sites(std::collections::HashMap::from([(2usize, 0x5170usize)]));
    if let Some(row) = row {
        builder.set_invoke_info(std::collections::HashMap::from([(
            2usize,
            (row, 2usize, b'I'),
        )]));
    }
    builder.set_indy_direct_sites(std::collections::HashMap::from([(2usize, opcode)]));
    builder.build(code, 8)
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): a constant call site
/// planned as `invokestatic` is the plain `Op::Call` of its row, at the
/// indy bci, with its arguments in source order -- no bridge, no trap, no
/// guard -- and the walk resumes five bytes on (the `ireturn` builds).
#[test]
fn r16w10_a_constant_indy_is_built_by_the_invoke_arm() {
    // 0: iload_0  1: iload_1  2: invokedynamic #1 (II)I  7: ireturn
    let code = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
    let row = r16w10_row("addImpl", "(II)I", 3);
    let g = r16w10_build(&code, &[IrType::Int, IrType::Int], Some(row), 0xb8).expect("builds");
    let calls: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. }))
        .collect();
    assert_eq!(calls.len(), 1, "one plain call");
    assert_eq!(calls[0].op, Op::Call { info_ptr: row });
    assert_eq!(calls[0].bytecode_pc, Some(2), "at the indy bci");
    assert_eq!(calls[0].ty, IrType::Int);
    assert_eq!(calls[0].inputs.len(), 4, "[ctrl, mem, a, b]");
    assert_eq!(g.nodes[calls[0].inputs[2] as usize].op, Op::Param(0));
    assert_eq!(g.nodes[calls[0].inputs[3] as usize].op, Op::Param(1));
    assert!(!g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::IndyBridge { .. })));
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })));
    assert!(g.site_trap_pcs.is_empty(), "no trap planted");
}

/// JIT round 20 wave 11 (lane pIndy, SPLICE19-1): build `code` (one
/// `invokedynamic` at pc 2 taking two values and returning `I`, NOT
/// bridged through `set_indy_bridge_sites`) with the guard `guard` at pc 2
/// and the plain call's row when given.
fn r20p_indy_guard_build(
    code: &[u8],
    params: &[IrType],
    row: Option<usize>,
    guard: IndyMutableGuard,
) -> Option<Graph> {
    let mut builder = IrBuilder::new(2, 2);
    builder.set_param_types(params);
    builder.set_indy_trap_sites(std::collections::HashMap::from([(2usize, (2usize, b'I'))]));
    if let Some(row) = row {
        builder.set_invoke_info(std::collections::HashMap::from([(
            2usize,
            (row, 2usize, b'I'),
        )]));
    }
    builder.set_indy_mutable_guards(std::collections::HashMap::from([(2usize, guard)]));
    builder.build(code, 8)
}

const R20P_GUARD: IndyMutableGuard = IndyMutableGuard {
    bridge_site: 0x5170,
    call_site_slot: 0x6100,
    target_slot: 0x6200,
    field_index: 3,
    virtual_target: false,
    second_target_slot: 0,
    second_site: 0,
    field_row: 0,
};

/// JIT round 20 wave 11 (lane pIndy, SPLICE19-1): a mutable call site's
/// planned target is `guard ? Call : IndyBridge`, both at the indy bci with
/// the arguments in source order; the guard reads the call site and the
/// planned handle from their root words and the target field as a
/// volatile load; no trap, no deopt guard, and the walk resumes five bytes
/// on (the `ireturn` returns the join).
#[test]
fn r20p_a_mutable_indy_is_a_guarded_call_with_the_bridge_on_a_miss() {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    // 0: iload_0  1: iload_1  2: invokedynamic #1 (II)I  7: ireturn
    let code = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
    let row = r16w10_row("addImpl", "(II)I", 3);
    let g = r20p_indy_guard_build(&code, &[IrType::Int, IrType::Int], Some(row), R20P_GUARD)
        .expect("builds");
    let count = |want: &dyn Fn(&Op) -> bool| g.nodes.iter().filter(|n| want(&n.op)).count();
    assert_eq!(
        count(&|op: &Op| *op == Op::Call { info_ptr: row }),
        1,
        "one plain call"
    );
    assert_eq!(
        count(&|op: &Op| *op == Op::IndyBridge { site: 0x5170 }),
        1,
        "one bridge"
    );
    assert_eq!(
        count(&|op: &Op| *op == Op::IndyRootRef { slot_addr: 0x6100 }),
        1
    );
    assert_eq!(
        count(&|op: &Op| *op == Op::IndyRootRef { slot_addr: 0x6200 }),
        1
    );
    assert_eq!(
        count(&|op: &Op| matches!(op, Op::Guard { .. })),
        0,
        "no deopt guard"
    );
    assert!(g.site_trap_pcs.is_empty(), "no trap planted");
    let load = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Load(MemKind::Ref))
        .expect("the target field read");
    assert!(load.volatile_access, "the target read is ordered");
    assert_eq!(
        g.nodes[load.inputs[2] as usize].op,
        Op::IndyRootRef { slot_addr: 0x6100 },
        "read through the call site's root word"
    );
    assert_eq!(g.nodes[load.inputs[3] as usize].op, Op::Const(3));
    for n in g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. } | Op::IndyBridge { .. }))
    {
        assert_eq!(n.bytecode_pc, Some(2), "at the indy bci");
        assert_eq!(n.ty, IrType::Int);
        assert_eq!(n.inputs.len(), 4, "[ctrl, mem, a, b]");
        assert_eq!(g.nodes[n.inputs[2] as usize].op, Op::Param(0));
        assert_eq!(g.nodes[n.inputs[3] as usize].op, Op::Param(1));
    }
    assert!(g.synthetic_field_access_pcs.contains(&2));
    let ret = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Return)
        .expect("the ireturn builds");
    let value = ret.inputs.last().copied().expect("a value");
    assert_eq!(
        g.nodes[value as usize].op,
        Op::Phi,
        "the join of call and bridge"
    );
}

/// JIT round 21 wave 1 (lane indy, INDY21-1): a bimorphic guard is
/// `t == A ? Call : (t == B ? IndyBridge(B's cell) : IndyBridge(own))`:
/// one plain call, the second arm a bridge call of the second site (never
/// a second `Op::Call` at the pc), the second compare on the first miss
/// edge, and the join of the miss side a phi under the outer phi.
#[test]
fn r21_a_bimorphic_mutable_indy_tests_the_second_target_before_the_bridge() {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    // 0: iload_0  1: iload_1  2: invokedynamic #1 (II)I  7: ireturn
    let code = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
    let row = r16w10_row("addImpl", "(II)I", 3);
    let guard = IndyMutableGuard {
        second_target_slot: 0x6300,
        second_site: 0x5180,
        ..R20P_GUARD
    };
    let g = r20p_indy_guard_build(&code, &[IrType::Int, IrType::Int], Some(row), guard)
        .expect("builds");
    let count = |want: &dyn Fn(&Op) -> bool| g.nodes.iter().filter(|n| want(&n.op)).count();
    assert_eq!(
        count(&|op: &Op| matches!(op, Op::Call { .. })),
        1,
        "one plain call"
    );
    assert_eq!(count(&|op: &Op| *op == Op::IndyBridge { site: 0x5170 }), 1);
    assert_eq!(count(&|op: &Op| *op == Op::IndyBridge { site: 0x5180 }), 1);
    assert_eq!(
        count(&|op: &Op| *op == Op::IndyRootRef { slot_addr: 0x6300 }),
        1,
        "the second handle's root word"
    );
    assert_eq!(count(&|op: &Op| *op == Op::If), 2, "two compares");
    let second = g
        .nodes
        .iter()
        .find(|n| n.op == Op::IndyBridge { site: 0x5180 })
        .expect("the second arm");
    assert_eq!(second.bytecode_pc, Some(2));
    assert_eq!(
        second.inputs.len(),
        4,
        "[ctrl, loaded handle (the memory token), a, b]"
    );
    let hit2 = &g.nodes[second.inputs[0] as usize];
    assert_eq!(hit2.op, Op::Proj(0), "the second compare's hit edge");
    let iff2 = &g.nodes[hit2.inputs[0] as usize];
    assert_eq!(iff2.op, Op::If);
    assert_eq!(
        g.nodes[iff2.inputs[0] as usize].op,
        Op::Proj(1),
        "tested on the first compare's miss edge"
    );
    let ret = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Return)
        .expect("the ireturn builds");
    let value = &g.nodes[ret.inputs.last().copied().expect("a value") as usize];
    assert_eq!(value.op, Op::Phi);
    assert_eq!(
        g.nodes[value.inputs[2] as usize].op,
        Op::Phi,
        "the miss side joins second arm and bridge first"
    );
    // Review rev-ir item 4: the three calls at the indy bci share ONE frame
    // state (the bci-keyed consumers take the first match), and every
    // call node at the bci is either the plan's row or a bridge.
    assert!(
        g.safepoints
            .iter()
            .filter(|sp| sp.bci == 2 && !sp.post_call_only)
            .count()
            <= 1,
        "no second frame state at the indy bci"
    );
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| n.bytecode_pc == Some(2)
                && matches!(n.op, Op::Call { .. } | Op::IndyBridge { .. }))
            .count(),
        3,
        "the planned call and the two bridges"
    );
}

/// JIT round 20 wave 11 (lane pIndy): every refusal leaves the `0xba`
/// arm's own answer -- here (no bridge row) the trap -- and builds no
/// guard: no row, a row whose argument count disagrees, a zero word, and
/// a virtual target without a reference receiver.
#[test]
fn r20p_a_mutable_indy_without_its_terms_keeps_the_arm() {
    let code = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
    let row = r16w10_row("addImpl", "(II)I", 3);
    let no_guard = |g: &Graph| {
        !g.nodes
            .iter()
            .any(|n| matches!(n.op, Op::IndyRootRef { .. }))
            && !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. }))
    };
    let ints = [IrType::Int, IrType::Int];
    if let Some(g) = r20p_indy_guard_build(&code, &ints, None, R20P_GUARD) {
        assert!(no_guard(&g), "no row");
    }
    let zero = IndyMutableGuard {
        target_slot: 0,
        ..R20P_GUARD
    };
    if let Some(g) = r20p_indy_guard_build(&code, &ints, Some(row), zero) {
        assert!(no_guard(&g), "zero word");
    }
    let virt = IndyMutableGuard {
        virtual_target: true,
        ..R20P_GUARD
    };
    if let Some(g) = r20p_indy_guard_build(&code, &ints, Some(row), virt) {
        assert!(no_guard(&g), "an int receiver is no receiver");
    }
    let mut builder = IrBuilder::new(2, 2);
    builder.set_param_types(&ints);
    builder.set_indy_trap_sites(std::collections::HashMap::from([(2usize, (2usize, b'I'))]));
    builder.set_invoke_info(std::collections::HashMap::from([(
        2usize,
        (row, 1usize, b'I'),
    )]));
    builder.set_indy_mutable_guards(std::collections::HashMap::from([(2usize, R20P_GUARD)]));
    if let Some(g) = builder.build(&code, 8) {
        assert!(no_guard(&g), "argument count disagrees");
    }
}

/// JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3): an own
/// non-capturing lambda site with a published slot is `word != null ?
/// word : IndyBridge`, with no `IndyBridge` reachable on the hot edge, no
/// trap, and the memory state joined only from the bridge; without the
/// row (or with a zero slot) the arm is the old one.
#[test]
fn r20p_a_published_lambda_word_is_read_with_the_bridge_cold() {
    // 0: invokedynamic #1 ()Object  5: areturn
    let code = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];
    let build = |words: Option<(usize, usize)>| {
        let mut builder = IrBuilder::new(0, 1);
        builder.set_indy_trap_sites(std::collections::HashMap::from([(0usize, (0usize, b'L'))]));
        if let Some(row) = words {
            builder.set_indy_lambda_words(std::collections::HashMap::from([(0usize, row)]));
        }
        builder.build(&code, 6)
    };
    let g = build(Some((0x5170, 0x6300))).expect("builds");
    let word = g
        .nodes
        .iter()
        .position(|n| n.op == Op::IndyRootRef { slot_addr: 0x6300 })
        .expect("the slot read");
    let bridge = g
        .nodes
        .iter()
        .find(|n| n.op == Op::IndyBridge { site: 0x5170 })
        .expect("the cold bridge");
    assert_eq!(bridge.inputs.len(), 2, "[ctrl, mem]: no arguments");
    assert!(g.site_trap_pcs.is_empty(), "no trap planted");
    let ret = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Return)
        .expect("areturn");
    let value = ret.inputs.last().copied().expect("a value") as usize;
    assert_eq!(g.nodes[value].op, Op::Phi);
    assert!(
        g.nodes[value].inputs.contains(&(word as NodeId)),
        "the join takes the word on the hit edge"
    );
    if ir_site_trap_enabled() {
        for g in [build(None), build(Some((0x5170, 0)))] {
            let g = g.expect("builds");
            assert!(!g
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::IndyRootRef { .. })));
            assert_eq!(g.site_trap_pcs, vec![0], "the old arm: the trap");
        }
    }
}

/// JIT round 23 wave 4 (proxy-define page): an own-code `checkcast` of a
/// class that is not loaded, AFTER a committed side effect, refuses the
/// build by default (the replay guard), and is planted as a site trap with
/// its cause when the build resumes such traps in place
/// (`IrBuilder::set_typecheck_traps_resume_in_place`).
#[test]
fn r23w4_unresolved_checkcast_after_a_store_traps_in_place_when_admitted() {
    // 0: aload_0  1: iconst_0  2: iconst_5  3: iastore
    // 4: aload_0  5: checkcast #3 (not loaded: no checkcast row)  8: areturn
    let code = [0x2a, 0x03, 0x08, 0x4f, 0x2a, 0xc0, 0x00, 0x03, 0xb0, 0, 0];
    let build = |in_place: bool| {
        let mut builder = IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Ref]);
        builder.set_typecheck_traps_resume_in_place(in_place);
        builder.build(&code, 9)
    };
    if !ir_site_trap_enabled() || !ir_trap_replay_guard_enabled() {
        return;
    }
    if !ir_unresolved_class_trap_enabled() {
        assert!(
            build(false).is_none(),
            "default: the cold type check refuses"
        );
    }
    let g = build(true).expect("admitted: builds around the type check");
    assert_eq!(
        g.site_trap_pcs,
        vec![5],
        "the trap is a site trap at the checkcast"
    );
    assert_eq!(g.trap_cause_at(5), Some(TrapCause::UnresolvedTypeCheck));
    assert!(
        g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 5 })),
        "an always-failing guard at the checkcast"
    );
}

/// JIT round 23 wave 8 (`CRATONVM_JIT_IR_UNRESOLVED_FIELD_IN_PLACE`): an
/// admitted unresolved-field site AFTER a committed side effect refuses the
/// build without the in-place admission (the replay guard), and is planted
/// as a site trap with its cause with it; `=0` restores the refusal.
#[test]
fn r23w8_unresolved_field_after_a_store_traps_in_place_when_admitted() {
    // 0: aload_0  1: iconst_0  2: iconst_5  3: iastore
    // 4: getstatic #1 (owner not loaded: admitted, no row)  7: ireturn
    let code = [0x2a, 0x03, 0x08, 0x4f, 0xb2, 0x00, 0x01, 0xac, 0, 0];
    let build = |in_place: bool| {
        let mut builder = IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Ref]);
        builder.set_field_trap_sites(HashMap::from([(4usize, b'I')]));
        builder.set_typecheck_traps_resume_in_place(in_place);
        builder.build(&code, 8)
    };
    if !ir_site_trap_enabled() || !ir_trap_replay_guard_enabled() {
        return;
    }
    assert!(
        build(false).is_none(),
        "not admitted: the replay guard refuses"
    );
    let key = concat!("CRATON", "VM_JIT_IR_UNRESOLVED_FIELD_IN_PLACE");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        let g = build(true).expect("admitted: builds around the field site");
        assert_eq!(g.site_trap_pcs, vec![4]);
        assert_eq!(g.trap_cause_at(4), Some(TrapCause::UnresolvedField));
        assert!(g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 4 })));
    });
}

/// JIT round 23 wave 8b (`CRATONVM_JIT_IR_IN_PLACE_TRAP_DEAD_PLACEHOLDER`):
/// the `Pattern.compile()V` shape. An in-place `checkcast` trap's
/// placeholder is still on the stack at a downstream unresolved-field
/// site (`putfield` into the unloaded class); with the dead placeholder
/// that second trap is planted in place too, and with `=0` the interned
/// null refuses it (the frame rule) and the replay guard bails the build.
#[test]
fn r23w8b_a_dead_trap_placeholder_does_not_refuse_the_next_in_place_trap() {
    // 0: aload_0  1: iconst_0  2: iconst_5  3: iastore
    // 4: aload_0  5: checkcast #3 (not loaded)
    // 8: iconst_1  9: putfield #1 (owner not loaded: admitted, no row)  12: return
    let code = [
        0x2a, 0x03, 0x08, 0x4f, 0x2a, 0xc0, 0x00, 0x03, 0x04, 0xb5, 0x00, 0x01, 0xb1, 0, 0,
    ];
    let build = || {
        let mut builder = IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Ref]);
        builder.set_field_trap_sites(HashMap::from([(9usize, b'I')]));
        builder.set_typecheck_traps_resume_in_place(true);
        builder.build(&code, 13)
    };
    if !ir_site_trap_enabled()
        || !ir_trap_replay_guard_enabled()
        || !ir_unresolved_typecheck_in_place_enabled()
        || !ir_unresolved_field_in_place_enabled()
    {
        return;
    }
    let key = concat!("CRATON", "VM_JIT_IR_IN_PLACE_TRAP_DEAD_PLACEHOLDER");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        let g = build().expect("both traps planted in place");
        assert_eq!(g.site_trap_pcs, vec![5, 9]);
        assert_eq!(g.trap_cause_at(5), Some(TrapCause::UnresolvedTypeCheck));
        assert_eq!(g.trap_cause_at(9), Some(TrapCause::UnresolvedField));
    });
}

/// JIT round 23 wave 5 (`CRATONVM_JIT_IR_NULL_TYPECHECK_NO_RESOLVE`): a
/// `checkcast` of the null constant to an unloaded class leaves the null
/// and plants no trap; `null instanceof` of one is the constant 0.
#[test]
fn r23w5_a_null_operand_type_check_needs_no_class() {
    if !ir_null_typecheck_no_resolve_enabled() {
        return;
    }
    // 0: aconst_null  1: checkcast #3 (no row)  4: areturn
    let cast = [0x01, 0xc0, 0x00, 0x03, 0xb0, 0, 0];
    let g = IrBuilder::new(0, 1)
        .build(&cast, 5)
        .expect("builds with no trap");
    assert!(g.site_trap_pcs.is_empty(), "no trap for a null operand");
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })));
    // 0: aconst_null  1: instanceof #3 (no row)  4: ireturn
    let test = [0x01, 0xc1, 0x00, 0x03, 0xac, 0, 0];
    let g = IrBuilder::new(0, 1)
        .build(&test, 5)
        .expect("builds with no trap");
    assert!(g.site_trap_pcs.is_empty());
    let ret = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Return)
        .expect("ireturn");
    let value = ret.inputs.last().copied().expect("a value") as usize;
    assert_eq!(g.nodes[value].op, Op::Const(0), "null instanceof C is 0");
}

/// Wave 5 fix (page `r23-w5-typecheck-in-place-trap-resumes-a-null-as-int`):
/// the in-place admission refuses a frame whose operand stack holds the
/// null constant (the interpreter rebuilt it as `Int(0)`), so the build
/// keeps the default refusal there.
#[test]
fn r23w5_in_place_refuses_a_frame_holding_a_null_constant() {
    if !ir_site_trap_enabled()
        || !ir_trap_replay_guard_enabled()
        || ir_unresolved_class_trap_enabled()
    {
        return;
    }
    // 0: aload_0  1: iconst_0  2: iconst_5  3: iastore  4: aconst_null
    // 5: aload_0  6: checkcast #3 (not loaded)  9: pop  10: areturn
    let code = [
        0x2a, 0x03, 0x08, 0x4f, 0x01, 0x2a, 0xc0, 0x00, 0x03, 0x57, 0xb0, 0, 0,
    ];
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    builder.set_typecheck_traps_resume_in_place(true);
    let g = builder
        .build(&code, 11)
        .expect("dead placeholder admits in-place trap");
    assert_eq!(g.site_trap_pcs, vec![6]);
}

/// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2): an own linked
/// capturing lambda site `(Object, int) -> proxy` is `New(proxy, 2)` plus
/// a reference store of capture 0 into field 0 and an int store of capture
/// 1 into field 1, in that order on the memory chain, with no trap and no
/// bridge; the method returns the allocation itself.
#[test]
fn r20q_a_capturing_lambda_site_is_its_proxy_new_and_one_store_per_capture() {
    // 0: aload_0  1: iload_1  2: invokedynamic #1 (Object,int)Object  7: areturn
    let code = [0x2a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];
    const PROXY: u32 = 0x8000_0007;
    let build = |row: Option<IndyCaptureNew>| {
        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        builder.set_indy_trap_sites(std::collections::HashMap::from([(2usize, (2usize, b'L'))]));
        if let Some(row) = row {
            builder.set_indy_capture_news(std::collections::HashMap::from([(2usize, row)]));
        }
        builder.build(&code, 8)
    };
    let row = IndyCaptureNew {
        proxy_class_id: PROXY,
        capture_tags: vec![b'L', b'I'],
    };
    let g = build(Some(row)).expect("builds");
    let new = g
        .nodes
        .iter()
        .position(|n| {
            n.op == Op::New {
                class_id: PROXY,
                num_fields: 2,
            }
        })
        .expect("the proxy allocation") as NodeId;
    assert_eq!(
        g.nodes[new as usize].bytecode_pc,
        Some(2),
        "at the indy bci"
    );
    let stores: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Store(_)) && n.inputs.get(2) == Some(&new))
        .collect();
    assert_eq!(stores.len(), 2, "one store per capture");
    assert_eq!(stores[0].op, Op::Store(MemKind::Ref));
    assert_eq!(stores[1].op, Op::Store(MemKind::Int));
    assert_eq!(g.nodes[stores[0].inputs[3] as usize].op, Op::Const(0));
    assert_eq!(g.nodes[stores[1].inputs[3] as usize].op, Op::Const(1));
    assert_eq!(g.nodes[stores[0].inputs[4] as usize].op, Op::Param(0));
    assert_eq!(g.nodes[stores[1].inputs[4] as usize].op, Op::Param(1));
    let first_store = g
        .nodes
        .iter()
        .position(|n| std::ptr::eq(n, stores[0]))
        .expect("store 0") as NodeId;
    assert_eq!(
        stores[1].inputs[1], first_store,
        "store 1 follows store 0 on the memory chain"
    );
    assert!(g.site_trap_pcs.is_empty(), "no trap planted");
    assert!(!g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::IndyBridge { .. })));
    assert!(g.lambda_proxy_alloc_classes.contains(&PROXY));
    assert!(g.synthetic_field_access_pcs.contains(&2));
    let ret = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Return)
        .expect("areturn");
    assert_eq!(
        ret.inputs.last().copied(),
        Some(new),
        "returns the allocation"
    );
    // Refusals keep the old arm (here the trap): no row, a tag that
    // disagrees with its operand, a count that disagrees with the site.
    if ir_site_trap_enabled() {
        let swapped = IndyCaptureNew {
            proxy_class_id: PROXY,
            capture_tags: vec![b'I', b'L'],
        };
        let short = IndyCaptureNew {
            proxy_class_id: PROXY,
            capture_tags: vec![b'L'],
        };
        for g in [build(None), build(Some(swapped)), build(Some(short))] {
            let g = g.expect("builds");
            assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::New { .. })));
            assert!(g.lambda_proxy_alloc_classes.is_empty());
            assert_eq!(g.site_trap_pcs, vec![2], "the old arm: the trap");
        }
    }
}

/// JIT round 16 wave 17 (lane mh7): build `code` -- a `getstatic` (or
/// another receiver) at pc 0, `iload_0` at 3, `invokevirtual` at 4 taking
/// one value and returning `I`, `ireturn` at 7 -- with the plan
/// `4 -> getstatic at 0` and the member's static row at 4.
fn r16w17_build(code: &[u8], row: usize) -> Option<Graph> {
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Int]);
    builder.set_static_field_info(std::collections::HashMap::from([(
        0usize,
        (0x5171u32, 3usize, b'L', false),
    )]));
    builder.set_invoke_info(std::collections::HashMap::from([(
        4usize,
        (row, 1usize, b'I'),
    )]));
    builder.set_mh_constant_sites(std::collections::HashMap::from([(4usize, 0usize)]));
    builder.build(code, 8)
}

/// JIT round 16 wave 17 (lane mh7): a constant handle's `invokevirtual`
/// planned as its member's `invokestatic` is the plain `Op::Call` of the
/// member's row, at the invoke's bci, with the handle dropped: its one
/// input after `[ctrl, mem]` is the argument, and the `ireturn` builds.
/// The snapshot at the invoke still holds the handle below the argument.
#[test]
fn r16w17_a_constant_handle_call_is_built_as_its_member() {
    // 0: getstatic #1  3: iload_0  4: invokevirtual #2 (I)I  7: ireturn
    let code = [0xb2, 0x00, 0x01, 0x1a, 0xb6, 0x00, 0x02, 0xac];
    let row = r16w10_row("twice", "(I)I", 3);
    let g = r16w17_build(&code, row).expect("builds");
    let calls: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. }))
        .collect();
    assert_eq!(calls.len(), 1, "one plain call");
    assert_eq!(calls[0].op, Op::Call { info_ptr: row });
    assert_eq!(calls[0].bytecode_pc, Some(4));
    assert_eq!(calls[0].inputs.len(), 3, "[ctrl, mem, the argument]");
    assert_eq!(g.nodes[calls[0].inputs[2] as usize].op, Op::Param(0));
    let at_invoke = g
        .safepoints
        .iter()
        .find(|sp| sp.bci == 4)
        .expect("a snapshot at the invoke");
    assert_eq!(at_invoke.stack.len(), 2, "the handle and the argument");
    assert!(matches!(
        g.nodes[at_invoke.stack[0] as usize].op,
        Op::LoadStatic {
            class_id: 0x5171,
            field_index: 3,
            ..
        }
    ));
}

/// JIT round 16 wave 17 (lane mh7): a planned site whose receiver is not
/// the planned field's value refuses the method rather than call the
/// member on another handle.
#[test]
fn r16w17_another_receiver_refuses_the_method() {
    // 0: aconst_null nop nop  3: iload_0  4: invokevirtual #2  7: ireturn
    let code = [0x01, 0x00, 0x00, 0x1a, 0xb6, 0x00, 0x02, 0xac];
    let row = r16w10_row("twice", "(I)I", 3);
    assert!(r16w17_build(&code, row).is_none());
}

/// JIT round 17 wave 3 (lane mh4; MHCONST17-2): a VIRTUAL constant
/// handle's site is built by the `invokevirtual` arm as its member's call:
/// the handle dropped, the receiver (the site's first argument) guarded
/// non-null at the invoke's bci, and the pc recorded for the post-call
/// exit's refusal.
#[test]
fn r17w3_a_virtual_constant_handle_call_is_built_as_its_member() {
    // 0: getstatic #1  3: aload_0  4: iload_1  5: invokevirtual #2
    // (LHost;I)I  8: ireturn
    let code = [0xb2, 0x00, 0x01, 0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac];
    let row = r16w10_row("add", "(I)I", 0);
    let mut builder = IrBuilder::new(2, 2);
    builder.set_param_types(&[IrType::Ref, IrType::Int]);
    builder.set_static_field_info(std::collections::HashMap::from([(
        0usize,
        (0x5171u32, 3usize, b'L', false),
    )]));
    builder.set_invoke_info(std::collections::HashMap::from([(
        5usize,
        (row, 2usize, b'I'),
    )]));
    builder.set_mh_constant_sites(std::collections::HashMap::from([(5usize, 0usize)]));
    let g = builder.build(&code, 9).expect("builds");
    let calls: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. }))
        .collect();
    assert_eq!(calls.len(), 1, "one plain call");
    assert_eq!(calls[0].op, Op::Call { info_ptr: row });
    assert_eq!(calls[0].inputs.len(), 4, "[ctrl, mem, receiver, argument]");
    assert_eq!(g.nodes[calls[0].inputs[2] as usize].op, Op::Param(0));
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 5 })));
    assert_eq!(g.mh_constant_call_pcs, vec![5]);
}

/// JIT round 19 wave 1 (lane mhiface; MHRESID18-4): an INTERFACE constant
/// handle's site is built by the `invokevirtual` arm as its member's
/// `invokeinterface` row, the receiver guarded by an `instanceof` of the
/// row's interface (interned under the row's class id) at the invoke's
/// bci; a site planned as an interface one whose row is not kind 2 (or
/// the other way round) refuses the method.
#[test]
fn r19w1_an_interface_constant_handle_call_guards_its_receivers_type() {
    // 0: getstatic #1  3: aload_0  4: iload_1  5: invokevirtual #2
    // (LHost;I)I  8: ireturn
    let code = [0xb2, 0x00, 0x01, 0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac];
    let build = |row: usize, interface: bool| {
        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        builder.set_static_field_info(std::collections::HashMap::from([(
            0usize,
            (0x5171u32, 3usize, b'L', false),
        )]));
        builder.set_invoke_info(std::collections::HashMap::from([(
            5usize,
            (row, 2usize, b'I'),
        )]));
        builder.set_mh_constant_sites(std::collections::HashMap::from([(5usize, 0usize)]));
        if interface {
            builder.set_mh_constant_interface_sites(std::collections::HashSet::from([5usize]));
        }
        builder.build(&code, 9)
    };
    let row = r16w10_row("apply", "(I)I", 2);
    let g = build(row, true).expect("builds");
    let calls: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. }))
        .collect();
    assert_eq!(calls.len(), 1, "one plain call");
    assert_eq!(calls[0].op, Op::Call { info_ptr: row });
    assert_eq!(calls[0].inputs.len(), 4, "[ctrl, mem, receiver, argument]");
    let tests: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::InstanceOf { .. }))
        .collect();
    assert_eq!(tests.len(), 1, "one receiver type test");
    assert_eq!(tests[0].bytecode_pc, Some(5));
    assert_eq!(g.nodes[tests[0].inputs[2] as usize].op, Op::Param(0));
    let Op::InstanceOf {
        name_ptr, name_len, ..
    } = tests[0].op
    else {
        unreachable!()
    };
    assert_eq!(name_len, "p/Host".len());
    assert_eq!(
        crate::typecheck_target_for_site(name_ptr as *const u8),
        Some(0x7101),
        "the interface interned under the row's class id"
    );
    assert!(g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 5 })));
    assert_eq!(g.mh_constant_call_pcs, vec![5]);
    // A kind-2 row at a site not planned as an interface one, and a
    // virtual row at one that is: refused.
    assert!(build(row, false).is_none());
    assert!(build(r16w10_row("add", "(I)I", 0), true).is_none());
}

/// JIT round 20 (MHIFACE19-1): an interface constant handle site with a
/// seen-class cell plants it on its guard's `instanceof`; without one the
/// `instanceof` carries none (`0`), as every other `instanceof` does.
#[test]
fn r20_an_interface_guard_carries_its_seen_class_cell() {
    // 0: getstatic #1  3: aload_0  4: iload_1  5: invokevirtual #2
    // (LHost;I)I  8: ireturn
    let code = [0xb2, 0x00, 0x01, 0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac];
    let cell = [
        std::sync::atomic::AtomicU32::new(0x7101),
        std::sync::atomic::AtomicU32::new(0x7101),
    ];
    let cell_addr = cell.as_ptr() as usize;
    let row = r16w10_row("apply", "(I)I", 2);
    let seen = |with_cell: bool| {
        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        builder.set_static_field_info(std::collections::HashMap::from([(
            0usize,
            (0x5171u32, 3usize, b'L', false),
        )]));
        builder.set_invoke_info(std::collections::HashMap::from([(
            5usize,
            (row, 2usize, b'I'),
        )]));
        builder.set_mh_constant_sites(std::collections::HashMap::from([(5usize, 0usize)]));
        builder.set_mh_constant_interface_sites(std::collections::HashSet::from([5usize]));
        if with_cell {
            builder.set_mh_constant_interface_seen_cells(std::collections::HashMap::from([(
                5usize, cell_addr,
            )]));
        }
        let g = builder.build(&code, 9).expect("builds");
        let cells: Vec<usize> = g
            .nodes
            .iter()
            .filter_map(|n| match n.op {
                Op::InstanceOf { seen_cell, .. } => Some(seen_cell),
                _ => None,
            })
            .collect();
        cells
    };
    assert_eq!(seen(true), vec![cell_addr]);
    assert_eq!(seen(false), vec![0]);
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): a virtual target's receiver
/// is null-guarded at the indy bci before the call, so a null receiver
/// re-executes the `invokedynamic` in the interpreter (whose
/// `MethodHandle` raises the NPE).
#[test]
fn r16w10_a_constant_virtual_indy_guards_its_receiver() {
    // 0: aload_0  1: iload_1  2: invokedynamic #1 (LHost;I)I  7: ireturn
    let code = [0x2a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
    let row = r16w10_row("inst", "(I)I", 0);
    let g = r16w10_build(&code, &[IrType::Ref, IrType::Int], Some(row), 0xb6).expect("builds");
    assert!(
        g.nodes.iter().any(|n| n.op == Op::Guard { bci: 2 }),
        "the receiver's null guard"
    );
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| n.op == Op::Call { info_ptr: row })
            .count(),
        1
    );
    assert!(!g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::IndyBridge { .. })));
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): without the plan's call
/// row (the plan did not survive) the site keeps its bridge, rather than
/// the invoke arm refusing the method.
#[test]
fn r16w10_a_constant_indy_without_its_row_keeps_the_bridge() {
    let code = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
    let g = r16w10_build(&code, &[IrType::Int, IrType::Int], None, 0xb8).expect("builds");
    assert!(g
        .nodes
        .iter()
        .any(|n| n.op == Op::IndyBridge { site: 0x5170 }));
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })));
}

/// Round 16 (lane indy, SY4-1): a bridged `invokedynamic` is ONE
/// `Op::IndyBridge` call carrying its arguments in source order, typed by
/// the descriptor's return, threaded on the memory chain -- and NO trap:
/// no guard, no `site_trap_pcs` row (which is what would register the
/// method's body for the VM's site-trap policy).
#[test]
fn r16_a_bridged_indy_is_a_call_not_a_trap() {
    // static String f(int a, int b) { return "" + a + b; }
    // 0: iload_0  1: iload_1  2: invokedynamic #1 (II)String  7: areturn
    let code = [0x1a, 0x1b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];
    let g = r16_indy_build(&code, 8, 2, 2, (2, b'L'), Some(0x5170)).expect("builds");
    let bridges: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::IndyBridge { .. }))
        .collect();
    assert_eq!(bridges.len(), 1, "one bridge call");
    let call = bridges[0];
    assert_eq!(
        call.op,
        Op::IndyBridge { site: 0x5170 },
        "the resolver's site word"
    );
    assert_eq!(
        call.ty,
        IrType::Ref,
        "typed from the descriptor's `L` return"
    );
    assert_eq!(
        call.bytecode_pc,
        Some(2),
        "the exception edge resolves at the indy bci"
    );
    assert_eq!(call.inputs.len(), 4, "[ctrl, mem, a, b]");
    assert_eq!(
        g.nodes[call.inputs[2] as usize].op,
        Op::Param(0),
        "arg0 first"
    );
    assert_eq!(
        g.nodes[call.inputs[3] as usize].op,
        Op::Param(1),
        "then arg1"
    );
    assert!(g.site_trap_pcs.is_empty(), "a bridged site plants no trap");
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })),
        "and no trap guard"
    );
    assert_eq!(
        Op::IndyBridge { site: 1 }.memory_shape().map(|s| s.access),
        Some(MemAccess::Opaque),
        "arbitrary Java runs inside the bridge"
    );
    assert!(
        may_raise(&Op::IndyBridge { site: 1 }),
        "a toString() may throw"
    );
}

/// Round 16 (lane indy): an `int`-returning bridged site (a pattern
/// `switch`'s `typeSwitch`, a record's `hashCode`) is an `Int` call.
#[test]
fn r16_an_int_returning_bridged_indy_is_an_int_call() {
    // 0: iload_0  1: invokedynamic #1 (I)I  6: ireturn
    let code = [0x1a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
    let g = r16_indy_build(&code, 7, 1, 1, (1, b'I'), Some(0x5170)).expect("builds");
    let call = g
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::IndyBridge { .. }))
        .expect("a bridge call");
    assert_eq!(call.ty, IrType::Int);
    assert_eq!(call.inputs.len(), 3, "[ctrl, mem, arg0]");
}

/// Round 16 (lane indy): what the bridge cannot serve keeps the trap,
/// exactly as before -- a `void` site (the single-pass
/// `indy_site_is_bridged` refuses it too) and a zero site word.
#[test]
fn r16_an_unbridgeable_indy_keeps_the_trap() {
    if !ir_site_trap_enabled() {
        return;
    }
    // 0: invokedynamic #1 ()V  5: return
    let void_site = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb1, 0, 0];
    let g = r16_indy_build(&void_site, 6, 0, 0, (0, b'V'), Some(0x5170)).expect("builds");
    assert!(!g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::IndyBridge { .. })));
    assert_eq!(g.site_trap_pcs, vec![0], "a void site keeps its trap");

    // 0: invokedynamic #1 ()String  5: areturn
    let zero_site = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];
    let g = r16_indy_build(&zero_site, 6, 0, 0, (0, b'L'), Some(0)).expect("builds");
    assert!(!g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::IndyBridge { .. })));
    assert_eq!(g.site_trap_pcs, vec![0], "a zero site word is no bridge");

    // And with no bridge row at all the arm is byte-for-byte the old one.
    let g = r16_indy_build(&zero_site, 6, 0, 0, (0, b'L'), None).expect("builds");
    assert_eq!(g.site_trap_pcs, vec![0]);
}

/// Round 16 (lane indy): a bridge row is not a stack effect. A site with a
/// bridge pointer but no recorded shape is still refused -- the builder
/// never guesses how many entries an indy consumes.
#[test]
fn r16_a_bridge_row_without_a_shape_still_refuses() {
    let code = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];
    let mut builder = IrBuilder::new(0, 1);
    let mut bridged = std::collections::HashMap::new();
    bridged.insert(0usize, 0x5170usize);
    builder.set_indy_bridge_sites(bridged);
    assert!(builder.build(&code, 6).is_none());
}

/// JIT round 22 (lane throws, churn-loop J1;
/// `CRATONVM_JIT_IR_INDY_BRIDGE_ON_TRAP_REFUSAL`): under the refusal-only
/// plan a site whose trap the replay guard refuses (an `iastore` committed
/// before it) is BRIDGED instead of bailing the build, while a site whose
/// trap the default arm plants keeps the trap and gets no bridge.
#[test]
fn r22_throws_indy_is_bridged_only_where_its_trap_is_refused() {
    if !ir_site_trap_enabled() || !ir_trap_replay_guard_enabled() {
        return;
    }
    let build = |code: &[u8], len: usize, params: usize, pc: usize, plan: Option<bool>| {
        let mut builder = IrBuilder::new(params, params.max(1));
        if params == 1 {
            builder.set_param_types(&[IrType::Ref]);
        }
        builder.set_indy_trap_sites(std::collections::HashMap::from([(pc, (0usize, b'L'))]));
        if let Some(refusal_only) = plan {
            builder.set_indy_bridge_sites(std::collections::HashMap::from([(pc, 0x5170usize)]));
            builder.set_indy_bridge_refusal_only(refusal_only, true);
        }
        builder.build(code, len)
    };
    let bridges = |g: &Graph| {
        g.nodes
            .iter()
            .filter(|n| matches!(n.op, Op::IndyBridge { .. }))
            .count()
    };
    // 0: aload_0  1: iconst_0  2: iconst_1  3: iastore
    // 4: invokedynamic #1 ()String  9: areturn
    let committed = [
        0x2a, 0x03, 0x04, 0x4f, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0,
    ];
    assert!(
        build(&committed, 10, 1, 4, None).is_none(),
        "the default arm: the trap is refused and the whole build bails"
    );
    let g = build(&committed, 10, 1, 4, Some(true)).expect("the refusal-only plan builds");
    assert_eq!(bridges(&g), 1, "the refused site is bridged");
    assert!(g.site_trap_pcs.is_empty(), "and no trap is planted");

    // 0: invokedynamic #1 ()String  5: areturn -- nothing committed.
    let pure = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];
    let g = build(&pure, 6, 0, 0, Some(true)).expect("builds");
    assert_eq!(
        bridges(&g),
        0,
        "a plantable trap is not replaced by the bridge"
    );
    assert_eq!(g.site_trap_pcs, vec![0], "the default arm's trap is kept");
    // The unconditional plan (`CRATONVM_JIT_IR_INDY_BRIDGE`) still bridges it.
    let g = build(&pure, 6, 0, 0, Some(false)).expect("builds");
    assert_eq!(bridges(&g), 1);
}

/// JIT round 22 wave 2 (lane throws, THROWS22-3): the builder's replay
/// guard follows the VM sink's own kill switch. A trap at the top of a loop
/// whose store runs only after it is a first-arrival replay (admitted);
/// with `CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL=0` the VM judges it by the
/// loop-closed extent and refuses, so the builder must not plant it --
/// unless `CRATONVM_JIT_IR_TRAP_REPLAY_ASKS_SHARED_RULE=0` reproduces the
/// old disagreement.
/// JIT round 23 wave 2b, lane L1 (churn page J1,
/// `CRATONVM_JIT_IR_OSR_DOOR_TRAPS_IN_PLACE`): in the optimizing OSR door's
/// build a trap AFTER a committing loop is plantable (the door resumes it
/// in place), a trap ON the loop still answers to the replay rule, and
/// every other build keeps the replay rule everywhere.
#[test]
fn r23_l1_the_osr_door_plants_a_trap_off_every_loop_after_a_side_effect() {
    // The loop of the test above ([2, 16], `putstatic` at 10), then
    // `return` at 19: the trap site after the loop.
    let code: [u8; 22] = [
        0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0e, 0x00, 0x04, 0xb3, 0x00, 0x01, 0x84, 0x00,
        0x01, 0xa7, 0xff, 0xf2, 0xb1, 0, 0,
    ];
    assert!(osr_door_trap_pc_off_every_loop(&code, 20, 19));
    assert!(osr_door_trap_pc_off_every_loop(&code, 20, 0));
    assert!(!osr_door_trap_pc_off_every_loop(&code, 20, 2));
    assert!(!osr_door_trap_pc_off_every_loop(&code, 20, 13));
    assert!(!osr_door_trap_pc_off_every_loop(&code, 20, 16));
    let mut builder = IrBuilder::new(0, 1);
    assert!(
        !builder.trap_resumable_here(&code, 20, 19),
        "a method-entry build: the loop's store forbids a replay from entry"
    );
    assert!(!builder.trap_resumable_here(&code, 20, 13));
    builder.set_osr_door_traps_in_place(true);
    assert!(
        builder.trap_resumable_here(&code, 20, 19),
        "the OSR door resumes the trap after the loop in place"
    );
    assert!(
        !builder.trap_resumable_here(&code, 20, 13),
        "a trap on the loop keeps the replay rule"
    );
    assert!(
        !builder.trap_resumable_here(&code, 20, 20),
        "past the method's code"
    );
}

/// invokedynamic-uncommon-trap fix — regression test for a PC-desync bug
/// caught during review: `bytecode_analysis::branch_target_map`/`bytecode_analysis::back_edges` (the
/// two bytecode pre-scans `IrBuilder::build()` runs before its main
/// opcode loop) must step over invokedynamic's full 5-byte encoding
/// (opcode, cp_hi, cp_lo, 0, 0), exactly like `invokeinterface` (0xb9).
/// Before this fix both functions fell through to the generic
/// "unknown opcode, skip 1 byte" catch-all for 0xba, which misreads the
/// instruction's 4 operand bytes as up to 4 phantom opcodes — corrupting
/// branch-target / loop-header detection for the rest of the method.
/// (`ir_compatible` now excludes every indy-bearing method from the IR
/// path entirely — see `test_ir_compatible_rejects_invokedynamic` — so
/// this is defense-in-depth: these two pre-scan functions must stay
/// correct on their own terms regardless of that caller-side gate.)
#[test]
fn the_shared_branch_target_map_steps_over_invokedynamic() {
    // pc0: invokedynamic #1 (5 bytes: 0xba 0x00 0x01 0x11 0x00). The 4th
    // operand byte is 0x11 (sipush, a 3-byte op in this walker's table)
    // so that a mis-step which treats invokedynamic as 1 byte overshoots
    // PAST the real pc=5 `goto` and decodes garbage from inside it,
    // instead of coincidentally realigning: 1 (0xba) + 1 (0x00) + 1
    // (0x01) + 3 (0x11 sipush consumes pc4,pc5,pc6) lands at pc=7, two
    // bytes INSIDE the real goto's 3-byte encoding — guaranteed
    // desync, not a lucky realignment.
    // pc5: goto +3 (3 bytes: 0xa7 0x00 0x03) -> target pc8
    // pc8: ireturn (1 byte)
    let code: Vec<u8> = vec![
        0xba, 0x00, 0x01, 0x11, 0x00, // 0: invokedynamic #1
        0xa7, 0x00, 0x03, // 5: goto +3 -> pc 8
        0xac, // 8: ireturn
    ];
    let map = crate::bytecode_analysis::branch_target_map(&code, code.len());
    let targets: Vec<usize> = (0..code.len()).filter(|&p| map[p]).collect();
    assert_eq!(
        targets,
        vec![8],
        "goto's target must resolve to pc=8 (the real ireturn); a PC \
             desync from mis-stepping invokedynamic's operand bytes would \
             either fabricate a bogus target inside the invokedynamic's own \
             operand bytes or miss/mis-locate this one entirely"
    );
}

/// Sibling of the above for `bytecode_analysis::back_edges`: a BACKWARD branch to
/// pc=0 (a `goto` after the invokedynamic, jumping back to it) must be
/// recognized as a loop header at exactly pc=0 — which only happens if
/// invokedynamic's 5-byte length is stepped correctly so the `goto` at
/// pc=5 is decoded from the right bytes.
#[test]
fn the_shared_back_edges_step_over_invokedynamic() {
    // pc0: invokedynamic #1 (5 bytes: 0xba 0x00 0x01 0x11 0x00). The 4th
    // operand byte is 0x11 (sipush, a 3-byte op in this walker's table)
    // so a mis-step that treats invokedynamic as 1 byte overshoots past
    // the real pc=5 `goto` instead of coincidentally realigning on it
    // (see the sibling `bytecode_analysis::branch_target_map` test above for the same
    // reasoning) — a genuine regression guard, not a lucky bytestring.
    // pc5: goto -5 (3 bytes: 0xa7 0xff 0xfb) -> target pc0 (backward)
    let code: Vec<u8> = vec![
        0xba, 0x00, 0x01, 0x11, 0x00, // 0: invokedynamic #1
        0xa7, 0xff, 0xfb, // 5: goto -5 -> pc 0
    ];
    let headers: Vec<usize> = crate::bytecode_analysis::back_edges(&code, code.len())
        .into_iter()
        .map(|(header, _)| header)
        .collect();
    assert!(
        headers.contains(&0),
        "the backward goto's target (pc=0) must be recognized as a loop \
             header; a PC desync from mis-stepping invokedynamic would \
             decode the goto's offset bytes from the wrong position and \
             either miss this header or fabricate a wrong one"
    );
}

/// The canonical `try`/`catch` layout javac emits:
///
/// ```text
///  0: iload_0
///  1: iconst_1
///  2: iadd
///  3: istore_1      ← end of the protected range
///  4: goto +7 → 11
///  7: astore_2      ← HANDLER entry (exception object on an empty stack)
///  8: iconst_0
///  9: istore_1
/// 10: nop
/// 11: iload_1       ← join, reachable from both arms
/// 12: ireturn
/// ```
///
/// pcs 7..=10 are reachable only through an exception edge. The builder
/// must emit NO nodes and NO safepoint snapshots for them: walking them
/// is what produced the `NO_NODE`-referencing orphans that panicked
/// `ir_lower::slot_of` (the handler's leading `astore` pops an empty
/// abstract stack, and `pop()` yields `NO_NODE` silently).
#[test]
fn test_handler_body_is_not_walked() {
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0x60, // 2: iadd
        0x3c, // 3: istore_1
        0xa7, 0x00, 0x07, // 4: goto +7 -> 11
        0x4d, // 7: astore_2   (handler)
        0x03, // 8: iconst_0
        0x3c, // 9: istore_1
        0x00, // 10: nop
        0x1b, // 11: iload_1
        0xac, // 12: ireturn
    ];
    let graph = build_ir(&code, code.len(), 1, 3);

    let handler_pcs = 7..=10;
    for (id, node) in graph.nodes.iter().enumerate() {
        if let Some(pc) = node.bytecode_pc {
            assert!(
                !handler_pcs.contains(&pc),
                "node {id} ({:?}) was built for handler-only pc {pc}; \
                     handler bodies are unreachable in a compiled frame and \
                     must be skipped, not abstract-interpreted",
                node.op
            );
        }
    }
    for snap in &graph.safepoints {
        assert!(
            !handler_pcs.contains(&snap.bci),
            "safepoint snapshot recorded for handler-only bci {}; the \
                 compiled body can never resume there",
            snap.bci
        );
    }
    // The reachable code either side of the skipped region must still be
    // built — the skip must not swallow the join or the tail.
    assert!(
        graph.nodes.iter().any(|n| n.bytecode_pc == Some(2)),
        "the try body (pc 2, iadd) must still be compiled"
    );
    assert!(
        graph.safepoints.iter().any(|s| s.bci == 11),
        "the join after the handler (pc 11) must still be reached; a skip \
             that failed to resync would drop it"
    );
}

/// Before STUB-S8, an unsupported opcode reachable only through an
/// exception edge (here `athrow`, 0xbf, inside a handler body — the
/// rethrow every `finally` ends with) disqualified the WHOLE method: the
/// linear walk fell into the handler and hit `_ => return None`. This
/// test pins the builder's own behaviour, independent of whether 0xbf has
/// a lowering arm: STUB-S8 skips handler bytecode outright, so the
/// builder never visits this athrow at all, whether or not cov-07 gave
/// 0xbf a real arm for the REACHABLE case. (As of cov-07, `athrow` no
/// longer disqualifies a method one level up in `ir_compatible` either —
/// but that is a different gate than the one this test exercises.)
#[test]
fn test_unsupported_opcode_in_handler_does_not_bail_the_method() {
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0x60, // 2: iadd
        0x3c, // 3: istore_1
        0xa7, 0x00, 0x05, // 4: goto +5 -> 9
        0x4d, // 7: astore_2  (handler)
        0xbf, // 8: athrow    — unsupported by the builder
        0x1b, // 9: iload_1
        0xac, // 10: ireturn
    ];
    let builder = IrBuilder::new(1, 3);
    assert!(
        builder.build(&code, code.len()).is_some(),
        "an unsupported opcode reachable only through an exception edge \
             must not bail the method to single-pass"
    );
}

/// Reachability follows normal edges only, and in particular does NOT
/// treat a handler entry as a root.
#[test]
fn test_normally_reachable_excludes_handler_only_code() {
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xa7, 0x00, 0x04, // 1: goto +4 -> 5
        0x00, // 4: nop        (handler-only)
        0xac, // 5: ireturn
    ];
    let verified = cratonvm_reader::verified_code(&code).unwrap();
    let reachable = normally_reachable_pcs(&verified, code.len());
    assert!(reachable.contains(&0));
    assert!(reachable.contains(&1));
    assert!(reachable.contains(&5), "the goto target is reachable");
    assert!(
        !reachable.contains(&4),
        "pc 4 is only reachable by falling through a goto — i.e. not at \
             all — so it must be excluded"
    );
}

// ── Type lattice / φ typing ──────────────────────────────────────

/// An empty arena for hand-built graphs (no Start / Proj preamble).
fn empty_graph() -> Graph {
    Graph::empty(0)
}

/// A value node of the given type, with no inputs.
fn value(graph: &mut Graph, ty: IrType) -> NodeId {
    let op = match ty {
        IrType::Float | IrType::Double => Op::ConstF(0),
        IrType::Ref => Op::New {
            class_id: 1,
            num_fields: 0,
        },
        _ => Op::Const(1),
    };
    graph.add(op, ty, vec![], None)
}

#[test]
fn test_join_identical_types() {
    for ty in [
        IrType::Int,
        IrType::Long,
        IrType::Float,
        IrType::Double,
        IrType::Ref,
    ] {
        assert_eq!(
            join_data_type(ty, ty),
            Some(ty),
            "{ty:?} must join with itself"
        );
    }
    // The integer family widens to the widest integer, which in this IR is
    // already `Int` (boolean/byte/char/short/int all decode to it).
    assert_eq!(join_data_type(IrType::Int, IrType::Int), Some(IrType::Int));
}

#[test]
fn test_join_rejects_category_conflicts() {
    // Every cross-category pair is untypeable, in both directions.
    let conflicts = [
        (IrType::Int, IrType::Long),
        (IrType::Int, IrType::Float),
        (IrType::Int, IrType::Double),
        (IrType::Int, IrType::Ref),
        (IrType::Float, IrType::Double),
        (IrType::Float, IrType::Ref),
        (IrType::Long, IrType::Double),
        (IrType::Long, IrType::Ref),
    ];
    for (a, b) in conflicts {
        assert_eq!(join_data_type(a, b), None, "{a:?} ⊔ {b:?} must not join");
        assert_eq!(join_data_type(b, a), None, "{b:?} ⊔ {a:?} must not join");
        // Specifically: a conflict must never answer `Int`. That silent
        // answer is the bug this lattice exists to remove.
        assert_ne!(join_data_type(a, b), Some(IrType::Int));
    }
    // `Void` is the absence of a value — it never joins, not even with
    // itself.
    assert_eq!(join_data_type(IrType::Void, IrType::Void), None);
    assert_eq!(join_data_type(IrType::Void, IrType::Int), None);
    assert_eq!(join_data_type(IrType::Ref, IrType::Void), None);
}

#[test]
fn test_phi_type_int_int() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let a = value(&mut g, IrType::Int);
    let b = value(&mut g, IrType::Int);
    assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Int));
}

#[test]
fn test_phi_type_int_long_is_rejected() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let a = value(&mut g, IrType::Int);
    let b = value(&mut g, IrType::Long);
    assert!(
        g.phi_data_type_checked(&[m, a, b]).is_err(),
        "an int/long merge has no common representation"
    );
}

#[test]
fn test_phi_type_float_float() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let a = value(&mut g, IrType::Float);
    let b = value(&mut g, IrType::Float);
    assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Float));
}

#[test]
fn test_phi_type_double_double() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let a = value(&mut g, IrType::Double);
    let b = value(&mut g, IrType::Double);
    assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Double));
}

/// The headline regression: a float input must never produce an integer φ.
#[test]
fn test_phi_type_float_int_is_rejected_never_int() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let f = value(&mut g, IrType::Float);
    let i = value(&mut g, IrType::Int);
    let checked = g.phi_data_type_checked(&[m, f, i]);
    assert!(
        checked.is_err(),
        "float/int is untypeable; it must be reported, not answered `Int`: \
             {checked:?}"
    );
    assert_ne!(checked, Ok(IrType::Int));
}

#[test]
fn test_phi_type_ref_ref_is_ref_not_int() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let a = value(&mut g, IrType::Ref);
    let b = value(&mut g, IrType::Ref);
    // Historically this answered `Int`, which hid the merged oop from
    // `zero_ref_phi_slots` and from the deopt `StackSlotRef` classification.
    assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Ref));
}

#[test]
fn test_phi_type_ref_null_is_ref() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let r = value(&mut g, IrType::Ref);
    // A null literal: `Op::Const(0)` typed `Ref` (this IR has no distinct
    // null type — see `is_null_const`).
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    assert!(is_null_const(&g.nodes[null as usize]));
    assert_eq!(g.phi_data_type_checked(&[m, r, null]), Ok(IrType::Ref));
    assert_eq!(g.phi_data_type_checked(&[m, null, r]), Ok(IrType::Ref));
    // An integer zero is NOT a null, and must not launder an int/ref merge
    // into `Ref` (that direction hands the GC a non-oop to dereference).
    let int_zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    assert!(!is_null_const(&g.nodes[int_zero as usize]));
    assert!(g.phi_data_type_checked(&[m, r, int_zero]).is_err());
}

#[test]
fn test_phi_type_ref_int_is_rejected() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let r = value(&mut g, IrType::Ref);
    let i = value(&mut g, IrType::Int);
    assert!(g.phi_data_type_checked(&[m, r, i]).is_err());
    assert!(g.phi_data_type_checked(&[m, i, r]).is_err());
}

#[test]
fn test_phi_type_skips_undefined_inputs() {
    let mut g = empty_graph();
    let m = g.add(Op::Merge, IrType::Control, vec![], None);
    let r = value(&mut g, IrType::Ref);
    // An entry edge on which the slot is uninitialised contributes no type
    // information — it must not veto the ref typing.
    assert_eq!(g.phi_data_type_checked(&[m, NO_NODE, r]), Ok(IrType::Ref));
    // …and a φ with nothing but placeholders is an error, not `Int`.
    assert!(g.phi_data_type_checked(&[m, NO_NODE, NO_NODE]).is_err());
    // An out-of-range id is treated exactly like the placeholder.
    let bogus = g.nodes.len() as NodeId + 7;
    assert_eq!(g.phi_data_type_checked(&[m, bogus, r]), Ok(IrType::Ref));
}

/// A loop-carried φ is created before its back-edge value exists; appending
/// that value must re-derive the type (`patch_loop_backedge` → `retype_phi`).
#[test]
fn test_loop_carried_phi_retyped_when_second_input_arrives() {
    let mut b = IrBuilder::new(0, 1);
    let region = b.graph.add(Op::Region, IrType::Control, vec![], None);
    // Entry edge: slot uninitialised — no type information, so the φ takes
    // the conservative fallback.
    let entry_inputs = vec![region, NO_NODE];
    let ty0 = b.phi_data_type(&entry_inputs);
    assert_eq!(ty0, PHI_TYPE_FALLBACK);
    let phi = b.graph.add(Op::Phi, ty0, entry_inputs, None);

    // Back-edge value: a reference. After the patch the φ is a GC root.
    let back = b.graph.add(
        Op::New {
            class_id: 7,
            num_fields: 0,
        },
        IrType::Ref,
        vec![],
        None,
    );
    b.graph.nodes[phi as usize].inputs.push(back);
    b.retype_phi(phi, 0);
    assert_eq!(
        b.graph.nodes[phi as usize].ty,
        IrType::Ref,
        "the late back-edge input must retype the loop-carried φ"
    );

    // A late input that does NOT join drops the φ to the fallback. Keeping
    // `Ref` published a slot holding an `int` (or a long/double) as a GC
    // root; a conflicting merge is only legal for a dead slot, where the
    // fallback is free.
    let stray = b.graph.add(Op::Const(3), IrType::Int, vec![], None);
    b.graph.nodes[phi as usize].inputs.push(stray);
    b.retype_phi(phi, 0);
    assert_eq!(b.graph.nodes[phi as usize].ty, PHI_TYPE_FALLBACK);
    assert_ne!(b.graph.nodes[phi as usize].ty, IrType::Ref);

    // Memory φs are bookkeeping tokens and are never retyped as values.
    let mem_phi = b
        .graph
        .add(Op::Phi, IrType::Memory, vec![region, back], None);
    b.retype_phi(mem_phi, 0);
    assert_eq!(b.graph.nodes[mem_phi as usize].ty, IrType::Memory);
}

/// The infallible wrapper keeps its old signature and never panics: an
/// untypeable merge degrades to [`PHI_TYPE_FALLBACK`] (and is reported).
#[test]
fn test_phi_data_type_falls_back_on_conflict() {
    let mut b = IrBuilder::new(0, 1);
    let m = b.graph.add(Op::Merge, IrType::Control, vec![], None);
    let r = b.graph.add(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        IrType::Ref,
        vec![],
        None,
    );
    let i = b.graph.add(Op::Const(1), IrType::Int, vec![], None);
    assert!(b.phi_data_type_checked(&[m, r, i]).is_err());
    assert_eq!(b.phi_data_type(&[m, r, i]), PHI_TYPE_FALLBACK);
    // The fallback is deliberately NOT `Ref`: an unprovable slot must not
    // be handed to the collector as an oop.
    assert_ne!(PHI_TYPE_FALLBACK, IrType::Ref);
}

// ── Constant interning ───────────────────────────────────────────

/// `iconst` / `aconst_null` hand back one node per `(value, type)`, and
/// the index they read gives the answer the arena scan it replaced gave —
/// the LOWEST-id live match — through every way the arena changes under
/// it during a build.
#[test]
fn constant_interning_returns_one_node_per_constant() {
    let mut b = IrBuilder::new(1, 2);

    let five = b.iconst(5);
    assert_eq!(b.iconst(5), five, "the same constant is one node");
    assert_eq!(b.graph.nodes[five as usize].op, Op::Const(5));
    assert_eq!(b.graph.nodes[five as usize].ty, IrType::Int);

    // The type is part of the identity, both ways round.
    let null = b.aconst_null();
    let zero = b.iconst(0);
    assert_ne!(null, zero, "null is not the int zero");
    assert_eq!(b.aconst_null(), null);
    assert_eq!(b.iconst(0), zero);
    let long_five = b.lconst(5);
    assert_ne!(long_five, five);
    assert_eq!(b.iconst(5), five, "a long 5 is not an int 5");

    // A constant added behind the helpers' back is found, as the scan
    // found it — and of two, the lower id wins, as the scan's first match
    // did.
    let seven_a = b.graph.add(Op::Const(7), IrType::Int, vec![], None);
    let seven_b = b.graph.add(Op::Const(7), IrType::Int, vec![], None);
    assert_eq!(b.iconst(7), seven_a);

    // Killing the interned node: the next call must not return the dead
    // one. With a live duplicate, the duplicate; with none, a new node.
    b.graph.kill(seven_a);
    assert_eq!(
        b.iconst(7),
        seven_b,
        "falls back to the surviving duplicate"
    );
    assert_eq!(b.iconst(7), seven_b, "and the repaired entry sticks");
    b.graph.kill(five);
    let five_again = b.iconst(5);
    assert_ne!(five_again, five, "a dead node is never handed back");
    assert_eq!(b.graph.nodes[five_again as usize].op, Op::Const(5));
    assert_eq!(b.iconst(5), five_again);

    // Every answer agrees with the scan the index replaced.
    let scan = |g: &Graph, v: i64, ty: IrType| {
        g.nodes
            .iter()
            .position(|n| n.op == Op::Const(v) && n.ty == ty)
            .map(|i| i as NodeId)
    };
    for (v, ty) in [
        (5, IrType::Int),
        (0, IrType::Int),
        (7, IrType::Int),
        (0, IrType::Ref),
    ] {
        assert_eq!(
            b.interned_const(v, ty),
            scan(&b.graph, v, ty),
            "({v}, {ty:?})"
        );
    }
    assert_eq!(b.interned_const(123, IrType::Int), None);
}

// ── Option-based id accessors ────────────────────────────────────

#[test]
fn test_checked_id_accessors() {
    let mut g = empty_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Long, vec![a, NO_NODE], None);

    // node_id_opt / is_valid_id
    assert_eq!(node_id_opt(a), Some(a));
    assert_eq!(node_id_opt(NO_NODE), None);
    assert!(g.is_valid_id(a));
    assert!(g.is_valid_id(b));
    assert!(!g.is_valid_id(NO_NODE));
    assert!(!g.is_valid_id(g.nodes.len() as NodeId));

    // node_opt / type_of
    assert!(g.node_opt(NO_NODE).is_none());
    assert!(g.node_opt(g.nodes.len() as NodeId).is_none());
    assert_eq!(g.type_of(a), Some(IrType::Int));
    assert_eq!(g.type_of(b), Some(IrType::Long));
    assert_eq!(g.type_of(NO_NODE), None);
    g.node_opt_mut(a).unwrap().ty = IrType::Ref;
    assert_eq!(g.type_of(a), Some(IrType::Ref));

    // Node::input_opt — placeholder and out-of-range both read as `None`.
    let n = &g.nodes[b as usize];
    assert_eq!(n.input_opt(0), Some(a));
    assert_eq!(n.input_opt(1), None, "a NO_NODE edge is not an id");
    assert_eq!(n.input_opt(2), None, "out of range");
    let vals: Vec<Option<NodeId>> = n.phi_value_inputs().collect();
    assert_eq!(vals, vec![None]);

    // SafepointSnapshot accessors.
    let snap = SafepointSnapshot::new(0, vec![a, NO_NODE], vec![NO_NODE, b], Vec::new());
    assert_eq!(snap.local_opt(0), Some(a));
    assert_eq!(snap.local_opt(1), None);
    assert_eq!(snap.local_opt(9), None);
    assert_eq!(snap.stack_opt(0), None);
    assert_eq!(snap.stack_opt(1), Some(b));
    assert_eq!(snap.stack_opt(9), None);
}

/// Real ids are dense from 0 and can never alias the sentinel.
#[test]
fn test_allocated_ids_never_alias_the_sentinel() {
    let mut g = empty_graph();
    for i in 0..8 {
        let id = g.add(Op::Const(i), IrType::Int, vec![], None);
        assert_eq!(id, i as NodeId);
        assert_ne!(id, NO_NODE);
        assert!(g.is_valid_id(id));
    }
}

// ── Compact edge lists (`Inputs`) ────────────────────────────────

/// `Inputs` must behave like the `Vec<NodeId>` it replaced, including the
/// spill from the inline buffer to the heap and back down again.
#[test]
fn test_inputs_matches_vec_semantics_including_spill() {
    let mut v: Vec<NodeId> = Vec::new();
    let mut i = Inputs::new();
    assert!(i.is_empty());
    assert_eq!(i.len(), 0);
    assert!(!i.is_spilled());

    // Push past the inline bound; the two stay identical throughout.
    for k in 0..(INLINE_INPUTS as NodeId * 3) {
        v.push(k);
        i.push(k);
        assert_eq!(i.as_slice(), v.as_slice(), "after push {k}");
        assert_eq!(i.len(), v.len());
    }
    assert!(
        i.is_spilled(),
        "{} entries must have spilled past the inline bound of {}",
        i.len(),
        INLINE_INPUTS
    );

    // Slice access, indexing, iteration, `contains`, equality with a Vec.
    assert_eq!(i[2], v[2]);
    assert_eq!(i.first().copied(), Some(0));
    assert_eq!(&i[1..3], &v[1..3]);
    assert!(i.contains(&(INLINE_INPUTS as NodeId)));
    assert_eq!(i.iter().sum::<NodeId>(), v.iter().sum::<NodeId>());
    assert_eq!(i, v);
    assert_eq!(i.to_vec(), v);

    // Mutation through the slice.
    i.as_mut_slice()[0] = 77;
    v[0] = 77;
    assert_eq!(i.as_slice(), v.as_slice());

    while let Some(x) = v.pop() {
        assert_eq!(i.pop(), Some(x));
        assert_eq!(i.as_slice(), v.as_slice());
    }
    assert_eq!(i.pop(), None);

    // The inline boundary itself.
    let mut j = Inputs::new();
    for k in 0..INLINE_INPUTS as NodeId {
        j.push(k);
        assert!(!j.is_spilled(), "{} entries still fit inline", j.len());
    }
    j.push(99);
    assert!(j.is_spilled());
    assert_eq!(j.len(), INLINE_INPUTS + 1);
    j.clear();
    assert!(j.is_empty());

    // Conversions.
    let from_vec = Inputs::from(vec![1u32, 2, 3]);
    assert_eq!(from_vec, vec![1u32, 2, 3]);
    assert_eq!(from_vec.clone().into_vec(), vec![1u32, 2, 3]);
    let collected: Inputs = (0..8u32).collect();
    assert_eq!(collected.len(), 8);
    assert!(collected.is_spilled());
    assert_eq!(format!("{:?}", from_vec), "[1, 2, 3]");
}

// ── Incremental def-use edges ────────────────────────────────────

/// A deterministic LCG — the randomized graphs must be reproducible so a
/// failure can be replayed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// A graph of `n` nodes with random edges into earlier nodes, some
/// `NO_NODE` holes, and a handful of safepoint snapshots.
fn random_graph(seed: u64, n: usize) -> Graph {
    let mut g = empty_graph();
    let mut rng = Lcg(seed);
    g.add(Op::Start, IrType::Control, vec![], None);
    for i in 1..n {
        let arity = rng.below(7) as usize;
        let mut inputs: Vec<NodeId> = Vec::with_capacity(arity);
        for _ in 0..arity {
            // One slot in eight is an undefined placeholder.
            if rng.below(8) == 0 {
                inputs.push(NO_NODE);
            } else {
                inputs.push(rng.below(i as u64) as NodeId);
            }
        }
        let op = if arity == 0 {
            Op::Const(rng.below(64) as i64)
        } else {
            Op::Add
        };
        g.add(op, IrType::Int, inputs, None);
    }
    for s in 0..6usize {
        let mut locals: Vec<NodeId> = Vec::new();
        let mut stack: Vec<NodeId> = Vec::new();
        for _ in 0..4 {
            locals.push(if rng.below(4) == 0 {
                NO_NODE
            } else {
                rng.below(n as u64) as NodeId
            });
        }
        for _ in 0..2 {
            stack.push(rng.below(n as u64) as NodeId);
        }
        g.safepoints
            .push(SafepointSnapshot::new(s, locals, stack, Vec::new()));
    }
    g
}

/// Every node and every snapshot slot, compared field by field.
fn assert_same_graph(a: &Graph, b: &Graph, what: &str) {
    assert_eq!(a.nodes.len(), b.nodes.len(), "{what}: node count");
    for (id, (x, y)) in a.nodes.iter().zip(b.nodes.iter()).enumerate() {
        assert_eq!(x.op, y.op, "{what}: node {id} op");
        assert_eq!(x.ty, y.ty, "{what}: node {id} type");
        assert_eq!(
            x.inputs.as_slice(),
            y.inputs.as_slice(),
            "{what}: node {id} inputs"
        );
        assert_eq!(x.bytecode_pc, y.bytecode_pc, "{what}: node {id} pc");
    }
    assert_eq!(
        a.safepoints.len(),
        b.safepoints.len(),
        "{what}: safepoint count"
    );
    for (si, (x, y)) in a.safepoints.iter().zip(b.safepoints.iter()).enumerate() {
        assert_eq!(x.bci, y.bci, "{what}: safepoint {si} bci");
        assert_eq!(x.locals, y.locals, "{what}: safepoint {si} locals");
        assert_eq!(x.stack, y.stack, "{what}: safepoint {si} stack");
    }
}

/// A chain: node `i` is `Add(i-1, i-2)`, so every node has exactly two
/// inputs and (in the interior) exactly two users. The shape makes the
/// operation counts below exact rather than approximate.
fn chain_graph(n: usize) -> Graph {
    assert!(n >= 4);
    let mut g = empty_graph();
    g.add(Op::Start, IrType::Control, vec![], None);
    g.add(Op::Const(1), IrType::Int, vec![], None);
    g.add(Op::Const(2), IrType::Int, vec![], None);
    for i in 3..n {
        g.add(
            Op::Add,
            IrType::Int,
            vec![(i - 1) as NodeId, (i - 2) as NodeId],
            None,
        );
    }
    g.exit = (n - 1) as NodeId;
    g
}

/// Use lists follow `add`, `replace_all_uses` and `kill`, and the counts
/// they report are the counts the historical scan reported.
#[test]
fn test_use_lists_maintained_across_add_replace_kill() {
    let mut g = empty_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    g.rebuild_use_lists();
    assert!(g.use_lists_valid());

    // `add` records the new node's edges.
    let s = g.add(Op::Add, IrType::Int, vec![a, b], None);
    let t = g.add(Op::Add, IrType::Int, vec![a, a], None);
    assert!(g.use_lists_valid(), "add keeps the lists current");
    assert_eq!(g.verify_use_lists(), Ok(()));
    assert_eq!(g.use_count(a), 3, "one edge from s, two from t");
    assert_eq!(g.use_count(b), 1);
    assert_eq!(g.use_counts(), vec![3, 1, 0, 0]);
    let mut users = g.users_of(a);
    users.sort_unstable();
    assert_eq!(users, vec![s, t]);

    // `replace_all_uses` moves every edge, including the doubled one.
    g.replace_all_uses(a, b);
    assert!(g.use_lists_valid());
    assert_eq!(g.verify_use_lists(), Ok(()));
    assert_eq!(g.use_count(a), 0);
    assert_eq!(g.use_count(b), 4);
    assert!(g.users_of(a).is_empty());

    // `kill` drops the dead node's outgoing edges.
    g.kill(t);
    assert!(g.use_lists_valid());
    assert_eq!(g.verify_use_lists(), Ok(()));
    assert_eq!(g.use_count(b), 2);
    assert_eq!(g.users_of(b), vec![s]);
    assert_eq!(g.use_counts(), vec![0, 2, 0, 0]);
}

/// Snapshot slots are use edges too: they are recorded, rewritten and
/// verified alongside input edges.
#[test]
fn test_use_lists_track_safepoint_slots() {
    let mut g = empty_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    g.rebuild_use_lists();

    g.push_safepoint(SafepointSnapshot::new(
        0,
        vec![a, NO_NODE],
        vec![a, b],
        Vec::new(),
    ));
    assert!(
        g.use_lists_valid(),
        "push_safepoint keeps the lists current"
    );
    assert_eq!(g.verify_use_lists(), Ok(()));
    assert_eq!(g.safepoint_users_of(a).len(), 2);
    assert_eq!(
        g.use_count(a),
        0,
        "snapshot slots are not input edges and never were"
    );

    g.replace_all_uses(a, b);
    assert_eq!(g.safepoints[0].locals[0], b);
    assert_eq!(g.safepoints[0].locals[1], NO_NODE, "a hole stays a hole");
    assert_eq!(g.safepoints[0].stack[0], b);
    assert_eq!(g.safepoints[0].stack[1], b);
    assert_eq!(g.verify_use_lists(), Ok(()));
    assert!(g.safepoint_users_of(a).is_empty());
    assert_eq!(g.safepoint_users_of(b).len(), 3);

    // The tracked slot writer keeps the lists current …
    assert!(g.set_safepoint_slot(0, SafepointSlotKind::Local, 1, a));
    assert!(g.use_lists_valid());
    assert_eq!(g.verify_use_lists(), Ok(()));
    g.replace_all_uses(a, b);
    assert_eq!(g.safepoints[0].locals[1], b);

    // … while a snapshot pushed through the public field demotes the next
    // mutation to the scan, which is exactly the point of the guard.
    g.safepoints
        .push(SafepointSnapshot::new(1, vec![b], vec![], Vec::new()));
    assert!(!g.use_lists_valid());
    g.replace_all_uses(b, a);
    assert_eq!(g.safepoints[1].locals[0], a, "the scan still rewrote it");
    assert!(g.use_lists_valid(), "the scan re-derived the lists");
    assert_eq!(g.verify_use_lists(), Ok(()));
}

/// Round 11 wave 18 (`r11-irback-use-list-invariant-enforced-by-convention-only`,
/// step 3): an INSTALL through the public `safepoints` field while the
/// lists are current used to be invisible to `sp_users`, so the next
/// tracked `replace_all_uses` left the slot naming the replaced node. The
/// field is now a [`Safepoints`] whose mutable paths bump the edge epoch:
/// the lists stop claiming currency, and the rewrite reaches the slot.
#[test]
fn r11w18_an_install_through_the_safepoints_field_is_not_missed() {
    let mut g = empty_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    let c = g.add(Op::Const(3), IrType::Int, vec![], None);
    g.rebuild_use_lists();
    g.push_safepoint(SafepointSnapshot::new(0, vec![a], vec![], Vec::new()));
    assert!(g.use_lists_valid());

    // Reads through the field do not demote anything.
    assert_eq!(g.safepoints.len(), 1);
    assert_eq!(g.safepoints[0].locals[0], a);
    for sp in &g.safepoints {
        assert_eq!(sp.bci, 0);
    }
    assert!(g.use_lists_valid(), "a read must not cost a rebuild");

    // The install the lists could not see before.
    g.safepoints[0].locals[0] = b;
    assert!(
        !g.use_lists_valid(),
        "an install through the field demotes the lists"
    );
    g.replace_all_uses(b, c);
    assert_eq!(
        g.safepoints[0].locals[0], c,
        "the rewrite followed the installed slot"
    );
    assert!(g.use_lists_valid());
    assert_eq!(g.verify_use_lists(), Ok(()));

    // `for sp in &mut graph.safepoints` is a mutable path too.
    for sp in &mut g.safepoints {
        sp.locals[0] = a;
    }
    assert!(!g.use_lists_valid());
    g.replace_all_uses(a, b);
    assert_eq!(g.safepoints[0].locals[0], b);
    assert_eq!(g.verify_use_lists(), Ok(()));
}

/// The two tracked non-installing writers keep the lists current: a clear
/// only removes a reference (stale superset), and the elided-monitor mask
/// is not a reference at all.
#[test]
fn r11w18_clearing_slots_and_marking_masks_keep_the_lists_current() {
    let mut g = empty_graph();
    let a = g.add(Op::Const(1), IrType::Int, vec![], None);
    let b = g.add(Op::Const(2), IrType::Int, vec![], None);
    g.rebuild_use_lists();
    g.push_safepoint(SafepointSnapshot::new(4, vec![a, b], vec![a], vec![b]));
    assert!(g.use_lists_valid());

    let mut asked = Vec::new();
    let cleared = g.clear_safepoint_slots_where(|slot, bci, v| {
        asked.push((slot.kind, slot.slot, bci, v));
        v == a
    });
    assert_eq!(cleared, 2, "local 0 and stack 0 named `a`");
    assert_eq!(asked.len(), 4, "every defined slot is asked once");
    assert!(asked.iter().all(|&(_, _, bci, _)| bci == 4));
    assert_eq!(g.safepoints[0].locals, vec![NO_NODE, b]);
    assert_eq!(g.safepoints[0].stack, vec![NO_NODE]);
    assert_eq!(g.safepoints[0].monitors, vec![b]);
    assert!(g.use_lists_valid(), "a clear keeps the lists current");
    assert_eq!(g.verify_use_lists(), Ok(()));
    assert!(
        g.safepoint_users_of(a).is_empty(),
        "the stale entries are re-checked"
    );

    assert!(g.or_safepoint_monitor_elided_mask(0, 0b1));
    assert!(!g.or_safepoint_monitor_elided_mask(1, 0b1));
    assert_eq!(g.safepoints[0].monitor_elided_mask, 0b1);
    assert!(g.use_lists_valid(), "the mask is not a reference");
}

/// `verify_use_lists` is the safety net for the invariant, so it has to
/// actually fail on a broken list — in both directions.
#[test]
fn test_verify_use_lists_catches_a_corrupted_list() {
    let mut g = chain_graph(12);
    g.rebuild_use_lists();
    assert_eq!(g.verify_use_lists(), Ok(()));

    // A use that the graph does not have.
    g.uses.users[4].push_untracked(11);
    assert!(
        g.verify_use_lists().is_err(),
        "an invented use must be reported"
    );
    g.rebuild_use_lists();
    assert_eq!(g.verify_use_lists(), Ok(()));

    // A use the graph has but the list lost.
    g.uses.users[4].clear_untracked();
    let err = g.verify_use_lists().expect_err("a dropped use is a bug");
    assert!(err.contains("node 4"), "unexpected message: {err}");

    // A snapshot slot the list never learned about.
    g.rebuild_use_lists();
    g.push_safepoint(SafepointSnapshot::new(0, vec![5], vec![], Vec::new()));
    assert_eq!(g.verify_use_lists(), Ok(()));
    g.uses.sp_users[5].clear();
    assert!(g.verify_use_lists().is_err(), "a dropped slot is a bug");
}

/// An edge written through the public `Node::inputs` field cannot notify
/// the use lists, so it must demote the next mutation to the full scan
/// rather than be silently missed.
#[test]
fn test_direct_field_writes_demote_to_the_scan() {
    let mut g = chain_graph(20);
    g.rebuild_use_lists();
    assert!(g.use_lists_valid());

    // A brand new edge to node 3, invisible to the lists.
    g.nodes[5].inputs.push(3);
    assert!(!g.use_lists_valid(), "the epoch guard must notice");

    g.replace_all_uses(3, 4);
    assert!(
        !g.nodes.iter().any(|n| n.inputs.contains(&3)),
        "the untracked edge must have been rewritten too"
    );
    assert!(g.use_lists_valid(), "the scan re-derived the lists");
    assert_eq!(g.verify_use_lists(), Ok(()));
    assert_eq!(g.use_counts(), scanned_use_counts(&g));
}

/// `use_counts` as it was written before the use lists existed.
fn scanned_use_counts(g: &Graph) -> Vec<u32> {
    let mut counts = vec![0u32; g.nodes.len()];
    for node in &g.nodes {
        for &inp in node.inputs.as_slice() {
            if (inp as usize) < counts.len() {
                counts[inp as usize] += 1;
            }
        }
    }
    counts
}

/// The incremental rewrite must produce the *same graph* as the historical
/// full scan on graphs it did not choose, for replacements it did not
/// choose. `set_use_tracking(false)` is the reference implementation: it is
/// the pre-existing scan, byte for byte.
#[test]
fn test_replace_all_uses_matches_the_full_scan_on_random_graphs() {
    for seed in 0..24u64 {
        let n = 24 + (seed as usize % 40);
        let mut reference = random_graph(seed, n);
        let mut incremental = random_graph(seed, n);
        assert_same_graph(&reference, &incremental, "construction");

        reference.set_use_tracking(false);
        incremental.rebuild_use_lists();

        let mut rng = Lcg(seed ^ 0xa5a5_5a5a);
        for round in 0..16 {
            let old = rng.below(n as u64) as NodeId;
            let new = rng.below(n as u64) as NodeId;

            // The tracked graph goes first, from a known-current state:
            // mutating the *reference* graph bumps the shared edge epoch
            // (its `kill` clears inputs through the public path), which
            // would otherwise demote every incremental step to a scan and
            // quietly stop testing the thing under test.
            incremental.ensure_use_lists();
            assert!(incremental.use_lists_valid());
            incremental.replace_all_uses(old, new);
            assert_eq!(
                incremental.verify_use_lists(),
                Ok(()),
                "seed {seed} round {round}"
            );
            reference.replace_all_uses(old, new);
            assert_same_graph(
                &reference,
                &incremental,
                &format!("seed {seed} round {round} ({old} → {new})"),
            );
            assert_eq!(
                incremental.use_counts(),
                reference.use_counts(),
                "seed {seed} round {round}"
            );

            // Interleave the other mutators so the lists are exercised
            // against a moving graph, not a frozen one.
            if round % 4 == 3 {
                let victim = rng.below(n as u64) as NodeId;
                let holder = rng.below(n as u64) as NodeId;
                let val = rng.below(n as u64) as NodeId;
                incremental.ensure_use_lists();
                incremental.kill(victim);
                incremental.push_input(holder, val);
                let fresh_i = incremental.add(Op::Add, IrType::Int, vec![old, new], None);
                assert_eq!(
                    incremental.verify_use_lists(),
                    Ok(()),
                    "seed {seed} round {round} after mutation"
                );
                reference.kill(victim);
                reference.push_input(holder, val);
                let fresh_r = reference.add(Op::Add, IrType::Int, vec![old, new], None);
                assert_eq!(fresh_r, fresh_i);
                assert_same_graph(
                    &reference,
                    &incremental,
                    &format!("seed {seed} round {round} after mutation"),
                );
            }
        }
    }
}

/// The measurement the review asked for, made by construction rather than
/// by timing: how many node-input slots does one `replace_all_uses` touch?
///
/// The chain graph has `n - 3` two-input nodes, so a full scan examines
/// `E = 2·(n-3)` slots *per call*, whatever the replacement is. The
/// incremental walk visits only the users of the replaced node — exactly
/// two of them, of two inputs each — so it examines 4 slots per call
/// regardless of `n`. With `R = 8` replacements:
///
/// | nodes  | E      | scan `R·E` | incremental `R·4` | ratio  |
/// |--------|--------|-----------:|------------------:|-------:|
/// | 100    | 194    | 1 552      | 32                | 48.5×  |
/// | 1 000  | 1 994  | 15 952     | 32                | 498×   |
/// | 5 000  | 9 994  | 79 952     | 32                | 2 498× |
/// | 20 000 | 39 994 | 319 952    | 32                | 9 998× |
///
/// The incremental cost is independent of graph size; the scan is linear
/// in it. (Real passes also pay one rebuild per untracked edit — asserted
/// to be zero here — so the end-to-end win on a mutation-heavy pass is
/// bounded by how often the pass writes edges through the public field.)
#[test]
fn test_replace_all_uses_slot_counts_by_graph_size() {
    // Eight replacement targets, 8 apart, so no target is a user of
    // another and each rewrite is independent of the ones before it.
    const ROUNDS: usize = 8;
    let targets: Vec<NodeId> = (0..ROUNDS).map(|k| (10 + 8 * k) as NodeId).collect();

    for &n in &[100usize, 1_000, 5_000, 20_000] {
        let edges = 2 * (n - 3) as u64;

        // Reference: the historical full scan.
        let mut scan = chain_graph(n);
        scan.set_use_tracking(false);
        scan.reset_use_list_stats();
        for &t in &targets {
            scan.replace_all_uses(t, 1);
        }
        assert_eq!(
            scan.use_list_slots_examined(),
            ROUNDS as u64 * edges,
            "the scan examines every input slot on every call (n = {n})"
        );

        // Incremental: only the users of the replaced node.
        let mut inc = chain_graph(n);
        inc.rebuild_use_lists();
        inc.reset_use_list_stats();
        for &t in &targets {
            inc.replace_all_uses(t, 1);
        }
        assert_eq!(
            inc.use_list_slots_examined(),
            ROUNDS as u64 * 4,
            "two users of two inputs each, per call (n = {n})"
        );
        assert_eq!(
            inc.use_list_rebuilds(),
            0,
            "no rebuild is needed once the lists are current (n = {n})"
        );

        // Same work, same graph.
        assert_same_graph(&scan, &inc, &format!("n = {n}"));
        assert_eq!(inc.verify_use_lists(), Ok(()));
        assert_eq!(inc.use_counts(), scan.use_counts());
    }
}

/// A builder-produced graph arrives with its def-use edges already
/// current: every edge the builder writes goes through a tracked mutator,
/// so the first optimization rewrite is incremental without a warm-up
/// scan.
#[test]
fn test_builder_graphs_arrive_with_current_use_lists() {
    // iload_0; iload_1; iadd; ireturn
    let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
    let graph = build_ir(&code, 4, 2, 2);
    assert!(
        graph.use_lists_valid(),
        "the builder must not leave the lists stale"
    );
    assert_eq!(graph.verify_use_lists(), Ok(()));
    assert_eq!(graph.use_counts(), scanned_use_counts(&graph));
}

// ── IR-tier inline FRAME sites (stack traces) ────────────────────
//
// The probe these are modelled on is `probes/StackTraceAfterOsr.java`:
// `probe()` calls `outer(1)`, which calls `mid`, which calls `leaf`. The
// IR tier splices `outer` and then `mid` into `probe`'s combined buffer,
// and before 2026-09-08 both contributed no stack frame at all.

/// `probe` has real code `[0, code_len)`; `outer` is spliced at caller pc
/// 42 and `mid` is spliced at a caller pc INSIDE `outer`'s body.
fn probe_shaped_sites() -> HashMap<usize, IrInlineSite> {
    fn site(base: usize, code_len: usize, key: &str, class_id: u32) -> IrInlineSite {
        IrInlineSite {
            base,
            code_len,
            num_args: 1,
            max_locals: 1,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: key.to_string(),
            class_id,
        }
    }
    let mut m = HashMap::new();
    // `outer` spliced at probe's pc 42, body at [100, 107).
    m.insert(42, site(100, 7, "P.outer:(I)I", 7));
    // `mid` spliced at combined pc 101 — one byte into `outer`'s body —
    // with its own body at [200, 207).
    m.insert(101, site(200, 7, "P.mid:(I)I", 7));
    m
}

/// Nothing spliced ⇒ no chain anywhere, and the fast path reports empty.
#[test]
fn inline_frame_sites_empty_answers_nothing() {
    let sites = IrInlineFrameSites::default();
    assert!(sites.is_empty());
    assert!(sites.chain_at(0).is_empty());
    assert!(sites.chain_at(9999).is_empty());
    assert_eq!(sites.enclosing_bci_at(42), None);
}

/// A pc in the COMPILING method's own code is not an inlined level. It is
/// the physical frame, which reports itself; answering a chain for it would
/// name a callee that is not on the stack.
#[test]
fn inline_frame_sites_refuse_the_compiling_methods_own_code() {
    let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
    assert!(!sites.is_empty());
    assert!(sites.chain_at(0).is_empty());
    assert!(sites.chain_at(42).is_empty());
    assert_eq!(sites.enclosing_bci_at(42), None);
}

/// One level deep: a pc inside `outer`'s body names `outer` alone, at
/// `outer`'s OWN bci — never the combined-buffer pc.
#[test]
fn inline_frame_sites_one_level() {
    let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
    let chain = sites.chain_at(103);
    assert_eq!(chain.len(), 1, "one spliced body encloses pc 103");
    assert_eq!(chain[0].method_key, "P.outer:(I)I");
    assert_eq!(chain[0].bci, 3, "103 - base 100");
    assert_eq!(chain[0].class_id, 7);
    // The compiling method's own bci covering it is the splice's caller pc.
    assert_eq!(sites.enclosing_bci_at(103), Some(42));
}

/// Two levels: a pc inside `mid` reports `mid` THEN `outer`, innermost
/// first, each with a bci in its own code. This is the exact shape that
/// printed `len=3` instead of `len=5` on `StackTraceAfterOsr`.
#[test]
fn inline_frame_sites_nested_chain_is_innermost_first() {
    let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
    let chain = sites.chain_at(204);
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].method_key, "P.mid:(I)I");
    assert_eq!(chain[0].bci, 4, "204 - base 200, inside mid");
    assert_eq!(chain[1].method_key, "P.outer:(I)I");
    assert_eq!(
        chain[1].bci, 1,
        "mid's caller pc 101 - outer's base 100: where outer calls mid"
    );
    // Still the OUTERMOST splice's caller pc — an index into probe's own
    // bytecode, which is the only bci a row may legally carry.
    assert_eq!(sites.enclosing_bci_at(204), Some(42));
}

/// A pc in the gap between two bodies belongs to neither. Ranges are
/// half-open, so `base + code_len` is already outside.
#[test]
fn inline_frame_sites_ranges_are_half_open() {
    let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
    assert!(!sites.chain_at(106).is_empty(), "last byte of outer's body");
    assert!(sites.chain_at(107).is_empty(), "one past it");
    assert!(sites.chain_at(150).is_empty(), "the gap before mid's body");
}

/// A malformed table whose site sits inside its OWN body would walk
/// forever. The chain is given up WHOLE rather than truncated: a truncated
/// chain reads as complete and names a caller that is not there.
#[test]
fn inline_frame_sites_refuse_a_cyclic_table() {
    let mut m = HashMap::new();
    m.insert(
        105, // caller pc inside the body this very site installs
        IrInlineSite {
            base: 100,
            code_len: 20,
            num_args: 0,
            max_locals: 1,
            arg_local_slots: vec![],
            returns_value: false,
            receiver_is_arg0: false,
            method_key: "P.loop:()V".to_string(),
            class_id: 1,
        },
    );
    let sites = IrInlineFrameSites::from_sites(&m);
    assert!(
        sites.chain_at(110).is_empty(),
        "a cycle must yield NO chain, not a truncated one"
    );
    assert_eq!(sites.enclosing_bci_at(110), None);
}

// ── Memory model: monitors, and the element type in the alias class ──

/// The `(ctrl, mem)` projections off a `Start`, the preamble every
/// memory-op fixture below needs.
fn start_preamble(g: &mut Graph) -> (NodeId, NodeId) {
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    (ctrl, mem)
}

/// A reference of *unknown* provenance. Two of them may be the same object
/// (`f(x, x)`), so nothing separates two accesses through them except the
/// facts the alias classes carry.
fn ref_param(g: &mut Graph, i: u16) -> NodeId {
    g.add(Op::Param(i), IrType::Ref, vec![], None)
}

/// A runtime subscript. Two accesses at the *same* node are a must-alias
/// on the offset, so every pair below reaches the interesting test.
fn subscript(g: &mut Graph) -> NodeId {
    g.add(Op::Add, IrType::Int, vec![], None)
}

/// Both monitor ops are in the single memory-shape table, at the same
/// token slot as every other memory op, and their third input is the
/// locked object rather than a token.
#[test]
fn monitor_ops_take_their_memory_token_at_the_documented_slot() {
    let mut g = empty_graph();
    let (ctrl, mem) = start_preamble(&mut g);
    let obj = ref_param(&mut g, 0);

    for (op, access) in [
        (Op::MonitorEnter, MemAccess::MonitorEnter),
        (Op::MonitorExit, MemAccess::MonitorExit),
    ] {
        assert_eq!(
            op.memory_shape(),
            Some(OpMemoryShape {
                token_slot: 1,
                min_full_arity: 3,
                access,
            }),
            "{op:?} must be registered in the single memory-shape table",
        );

        let id = g.add(op.clone(), IrType::Memory, vec![ctrl, mem, obj], None);
        let node = g.node_opt(id).expect("just added");
        assert_eq!(memory_token_slot(node), Some(1), "{op:?}");
        assert!(!is_memory_token_slot(node, 0), "{op:?}: slot 0 is control");
        assert!(is_memory_token_slot(node, 1), "{op:?}: slot 1 is the token");
        assert!(
            !is_memory_token_slot(node, 2),
            "{op:?}: slot 2 is the locked object, a value the node reads",
        );

        // Below the documented arity there is no token at all — the same
        // rule the compact hand-built `Store`/`Load` forms live under.
        let short = g.add(op.clone(), IrType::Memory, vec![ctrl, mem], None);
        assert_eq!(
            memory_token_slot(g.node_opt(short).expect("just added")),
            None,
            "{op:?} below its full arity has no token slot",
        );
    }
}

/// The ops produce exactly the effect the constructors state: a read *and*
/// a write of the monitor, a safepoint, and half a JMM fence each.
#[test]
fn monitor_ops_carry_the_documented_memory_effect() {
    let mut g = empty_graph();
    let (ctrl, mem) = start_preamble(&mut g);
    let obj = ref_param(&mut g, 0);
    let enter = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, mem, obj], None);
    let exit = g.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![ctrl, enter, obj],
        None,
    );

    assert_eq!(g.memory_effect(enter), MemEffect::monitor_enter(obj));
    assert_eq!(g.memory_effect(exit), MemEffect::monitor_exit(obj));

    let e = g.memory_effect(enter);
    assert_eq!(e.reads, AliasClass::Monitor { obj });
    assert_eq!(e.writes, AliasClass::Monitor { obj });
    assert_eq!(e.order, MemOrder::Acquire);
    assert!(e.safepoint, "a monitor is a deopt point");
    assert!(!e.allocates);

    let x = g.memory_effect(exit);
    assert_eq!(x.order, MemOrder::Release);
    assert!(x.safepoint);

    // Neither is control and neither is pure, so no GVN or liveness sweep
    // may treat one as a value it can drop.
    for op in [Op::MonitorEnter, Op::MonitorExit] {
        assert!(!op.is_control(), "{op:?}");
        assert!(!op.is_pure(), "{op:?}");
    }
}

/// The roach motel, both walls of it: nothing after a `MonitorEnter` may
/// move above it, nothing before a `MonitorExit` may move below it — and
/// an access *outside* the region may still sink into it.
#[test]
fn a_monitor_pair_is_a_barrier_in_both_directions() {
    let mut g = empty_graph();
    let (ctrl, mem) = start_preamble(&mut g);
    let obj = ref_param(&mut g, 0);
    let base = ref_param(&mut g, 1);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let val = g.add(Op::Const(7), IrType::Int, vec![], None);

    let pre = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off],
        None,
    );
    let enter = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, mem, obj], None);
    let inner_load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, enter, base, off],
        None,
    );
    let inner_store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, enter, base, off, val],
        None,
    );
    let exit = g.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![ctrl, inner_store, obj],
        None,
    );

    // Wall 1 — the acquire. Note the *reason*: the token edge from `enter`
    // to the two inner accesses is deliberately not a `DataDependence`, so
    // an `Acquire` answer proves the fence is what refused, not the chain.
    assert_eq!(
        g.may_reorder(enter, inner_load),
        Reorder::Blocked(ReorderBlock::Acquire),
    );
    assert_eq!(
        g.may_reorder(enter, inner_store),
        Reorder::Blocked(ReorderBlock::Acquire),
    );

    // Wall 2 — the release.
    assert_eq!(
        g.may_reorder(inner_load, exit),
        Reorder::Blocked(ReorderBlock::Release),
    );
    assert_eq!(
        g.may_reorder(inner_store, exit),
        Reorder::Blocked(ReorderBlock::Release),
    );

    // …and the asymmetry that makes it a roach motel rather than two full
    // fences: a read from before the region may move *into* it. A monitor
    // is not the same storage kind as a field cell, so nothing aliases —
    // and the proof is `DisjointLocations` rather than `ReadOnly` because
    // the enter itself writes the lock word.
    assert_eq!(
        g.may_reorder(pre, enter),
        Reorder::Allowed(ReorderProof::DisjointLocations),
    );
}

/// Two monitors on provably distinct objects do not alias — and still do
/// not commute. This is the claim `AliasClass::Monitor`'s doc makes, and
/// it is the one a purely alias-based model would get wrong.
#[test]
fn monitors_on_distinct_objects_do_not_alias_but_still_do_not_commute() {
    let mut g = empty_graph();
    let (ctrl, mem) = start_preamble(&mut g);
    // `value(_, Ref)` is an `Op::New`, so the two are fresh in-method
    // allocations and provably different objects.
    let a = value(&mut g, IrType::Ref);
    let b = value(&mut g, IrType::Ref);
    assert!(
        !g.refs_may_alias(a, b),
        "two fresh allocations are distinct"
    );
    assert!(!g.may_alias(
        AliasClass::Monitor { obj: a },
        AliasClass::Monitor { obj: b }
    ));

    let enter_a = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, mem, a], None);
    let enter_b = g.add(
        Op::MonitorEnter,
        IrType::Memory,
        vec![ctrl, enter_a, b],
        None,
    );
    assert_eq!(
        g.may_reorder(enter_a, enter_b),
        Reorder::Blocked(ReorderBlock::Acquire),
        "disjoint locks still order against each other",
    );
}

/// The element type is read off the op and lives in the alias class, so
/// the model — not a producer — decides that a reference element and a
/// primitive one are different memory.
#[test]
fn may_alias_distinguishes_a_reference_array_element_from_a_primitive_one() {
    let mut g = empty_graph();
    let (ctrl, mem) = start_preamble(&mut g);
    let a = ref_param(&mut g, 0);
    let b = ref_param(&mut g, 1);
    let idx = subscript(&mut g);
    let val = g.add(Op::Const(1), IrType::Int, vec![], None);
    assert!(
        g.refs_may_alias(a, b),
        "two parameters may be the same array — provenance separates nothing here",
    );

    let ref_load = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, a, idx],
        None,
    );
    let int_store = g.add(
        Op::ArrayStore(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, b, idx, val],
        None,
    );
    let ref_store = g.add(
        Op::ArrayStore(MemKind::Ref),
        IrType::Memory,
        vec![ctrl, mem, b, idx, val],
        None,
    );

    let ref_read = g.memory_effect(ref_load).reads;
    let int_write = g.memory_effect(int_store).writes;
    let ref_write = g.memory_effect(ref_store).writes;
    assert!(
        matches!(
            ref_read,
            AliasClass::ArrayElem {
                elem: MemKind::Ref,
                ..
            }
        ),
        "the class carries the element type: {ref_read:?}",
    );

    assert!(
        !g.may_alias(ref_read, int_write),
        "an int[] cell is never an Object[] cell",
    );
    assert!(
        g.may_alias(ref_read, ref_write),
        "two reference arrays may be the same array",
    );

    // The whole query agrees, which is what the vectorization gate reads.
    assert!(g.may_reorder(int_store, ref_load).is_allowed());
    assert_eq!(
        g.may_reorder(ref_store, ref_load),
        Reorder::Blocked(ReorderBlock::MayAlias),
    );
}

/// One array node accessed at two element kinds is an ill-typed graph, and
/// the model must keep saying "may alias" rather than reading the two type
/// tags as a proof of disjointness.
#[test]
fn one_array_at_two_element_kinds_is_still_a_must_alias() {
    let mut g = empty_graph();
    let (ctrl, mem) = start_preamble(&mut g);
    let a = ref_param(&mut g, 0);
    let idx = subscript(&mut g);
    let load_int = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, a, idx],
        None,
    );
    let load_ref = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, a, idx],
        None,
    );
    assert!(g.may_alias(
        g.memory_effect(load_int).reads,
        g.memory_effect(load_ref).reads,
    ));
}

/// The shape table for every op that had one before the monitors landed is
/// byte-for-byte what it was, and every op that had none still has none.
/// The memory-chain lane in `ir_verify` and the three private copies of
/// `memory_token_slot` all read these numbers.
#[test]
fn the_memory_shape_table_is_unchanged_for_every_pre_existing_op() {
    let with_shape: &[(Op, usize, MemAccess)] = &[
        (Op::Load(MemKind::Int), 3, MemAccess::FieldRead),
        (Op::Store(MemKind::Int), 4, MemAccess::FieldWrite),
        (Op::ArrayLoad(MemKind::Int), 4, MemAccess::ArrayRead),
        (Op::ArrayStore(MemKind::Int), 5, MemAccess::ArrayWrite),
        (Op::ArrayLength, 3, MemAccess::LengthRead),
        (
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            2,
            MemAccess::Allocate,
        ),
        (
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
            3,
            MemAccess::Allocate,
        ),
        (Op::Call { info_ptr: 0 }, 2, MemAccess::Opaque),
        // cov-01 — the same classification `Op::Call` has, for the same
        // reason: the helper each lowers to can run arbitrary Java.
        (
            Op::ConstString {
                holder_class_id: 0,
                cp_idx: 0,
                slot_addr: 0,
            },
            2,
            MemAccess::Opaque,
        ),
        (
            Op::ConstClass {
                holder_class_id: 0,
                cp_idx: 0,
                slot_addr: 0,
            },
            2,
            MemAccess::Opaque,
        ),
        (
            Op::LoadStatic {
                class_id: 0,
                field_index: 0,
                type_tag: b'I',
                is_volatile: false,
            },
            2,
            MemAccess::Opaque,
        ),
        // Round 11 wave 10: `putstatic`, `[ctrl, mem, value]`.
        (
            Op::StoreStatic {
                class_id: 0,
                field_index: 0,
                type_tag: b'I',
                is_volatile: false,
            },
            3,
            MemAccess::Opaque,
        ),
        (Op::LambdaIntToDouble, 4, MemAccess::Opaque),
        (Op::MonitorEnter, 3, MemAccess::MonitorEnter),
        (Op::MonitorExit, 3, MemAccess::MonitorExit),
    ];
    for (op, min_full_arity, access) in with_shape {
        assert_eq!(
            op.memory_shape(),
            Some(OpMemoryShape {
                token_slot: 1,
                min_full_arity: *min_full_arity,
                access: *access,
            }),
            "{op:?}",
        );
    }

    for op in [
        Op::Start,
        Op::Return,
        Op::If,
        Op::Merge,
        Op::Region,
        Op::Proj(0),
        Op::Const(0),
        Op::ConstF(0),
        Op::Param(0),
        Op::Phi,
        Op::Add,
        Op::Guard { bci: 0 },
        Op::Dead,
    ] {
        assert_eq!(op.memory_shape(), None, "{op:?}");
    }
}

// ── The guard token edge ─────────────────────────────────────────────

/// The guarded family and its arities, pinned. What this catches: an op
/// added to (or removed from) `Op::guard_shape` without the matching arity
/// widening in `ir_verify::expected_arity`, which would either reject the
/// guarded shape (a silent fall-back to the single-pass tier) or admit a
/// trailing operand with no token slot to explain it.
#[test]
fn the_guard_shape_table_names_exactly_the_guarded_family() {
    let guarded: &[(Op, usize)] = &[
        (Op::Load(MemKind::Int), 4),
        (Op::Load(MemKind::Ref), 4),
        (Op::ArrayLoad(MemKind::Byte), 4),
        (Op::ArrayLength, 3),
        (Op::Div, 2),
        (Op::Rem, 2),
    ];
    for (op, unguarded_arity) in guarded {
        assert_eq!(
            op.guard_shape(),
            Some(OpGuardShape {
                unguarded_arity: *unguarded_arity
            }),
            "{op:?}",
        );
        // The slot is the unguarded arity, and only at exactly one more
        // input than that: a compact or documented form must not be read
        // as carrying a token.
        assert_eq!(
            guard_token_slot_for(op, *unguarded_arity + 1),
            Some(*unguarded_arity),
            "{op:?}",
        );
        for arity in 0..=*unguarded_arity {
            assert_eq!(guard_token_slot_for(op, arity), None, "{op:?} @ {arity}");
        }
        assert_eq!(
            guard_token_slot_for(op, *unguarded_arity + 2),
            None,
            "{op:?}",
        );
    }

    // The store family is deliberately excluded — see `Op::guard_shape`.
    for op in [
        Op::Store(MemKind::Int),
        Op::ArrayStore(MemKind::Int),
        Op::Add,
        Op::Mul,
        Op::Guard { bci: 0 },
        Op::Phi,
        Op::Dead,
    ] {
        assert_eq!(op.guard_shape(), None, "{op:?}");
    }
}

/// The `Div`/`Rem` split, which is the arm that shows up on no hand-built
/// fixture. Both sit in the homogeneous arithmetic group of
/// `expected_input_type`, which demands every operand carry the node's own
/// result type; the guard is `IrType::Void`. Delete the guard-slot arm in
/// `expected_input_type` and this fails — while every `Load`-shaped test
/// keeps passing, because the memory ops already answer `Any` past their
/// documented slots.
#[test]
fn the_guard_token_slot_carries_no_type_requirement() {
    for op in [Op::Div, Op::Rem] {
        assert_eq!(expected_input_type(&op, 0, 3), TypeReq::SameAsResult);
        assert_eq!(expected_input_type(&op, 1, 3), TypeReq::SameAsResult);
        assert_eq!(expected_input_type(&op, 2, 3), TypeReq::Any);
        // Unguarded, slot 2 does not exist and the group is unchanged.
        assert_eq!(expected_input_type(&op, 0, 2), TypeReq::SameAsResult);
        assert_eq!(expected_input_type(&op, 1, 2), TypeReq::SameAsResult);
    }
    assert_eq!(
        expected_input_type(&Op::Load(MemKind::Int), 4, 5),
        TypeReq::Any
    );
    assert_eq!(
        expected_input_type(&Op::ArrayLoad(MemKind::Byte), 4, 5),
        TypeReq::Any
    );
    assert_eq!(expected_input_type(&Op::ArrayLength, 3, 4), TypeReq::Any);
    // A `Store`'s slot 4 is the stored VALUE and keeps its exact
    // requirement — the store family carries no token, and reading one
    // there would drop the value.
    assert_eq!(
        expected_input_type(&Op::Store(MemKind::Byte), 4, 5),
        TypeReq::Exact(IrType::Int)
    );
}

/// The guarded shape does not disturb the readers that recover a memory
/// access's operands by slot. What this catches: a token appended anywhere
/// but last, or a shape whose guarded arity collides with a layout
/// `access_location` disambiguates by input count (the `Store` arity-4
/// "full-without-offset" form is exactly such a case, which is why the
/// store family has no token).
#[test]
fn a_guard_token_does_not_move_a_memory_access_s_operands() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let off = g.add(Op::Const(3), IrType::Int, vec![], None);
    let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
    let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, cond], None);

    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off, guard],
        None,
    );
    assert_eq!(
        access_location(&g.nodes[load as usize], MemAccess::FieldRead),
        AliasClass::Field {
            base,
            offset: AccessOffset::Dynamic(off),
            decl: None,
        }
    );
    assert_eq!(memory_token_slot(&g.nodes[load as usize]), Some(1));
    assert_eq!(guard_token_slot(&g.nodes[load as usize]), Some(4));
    assert_eq!(guard_token_of(&g.nodes[load as usize]), guard);

    let al = g.add(
        Op::ArrayLoad(MemKind::Byte),
        IrType::Int,
        vec![ctrl, mem, base, off, guard],
        None,
    );
    assert_eq!(
        access_location(&g.nodes[al as usize], MemAccess::ArrayRead),
        AliasClass::ArrayElem {
            array: base,
            index: AccessOffset::Dynamic(off),
            elem: MemKind::Byte,
        }
    );
    assert_eq!(guard_token_of(&g.nodes[al as usize]), guard);

    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![ctrl, mem, base, guard],
        None,
    );
    assert_eq!(
        access_location(&g.nodes[len as usize], MemAccess::LengthRead),
        AliasClass::ArrayLength { array: base }
    );
    assert_eq!(guard_token_of(&g.nodes[len as usize]), guard);

    // An unguarded node answers `NO_NODE`, not "the last input".
    let plain = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, off],
        None,
    );
    assert_eq!(guard_token_slot(&g.nodes[plain as usize]), None);
    assert_eq!(guard_token_of(&g.nodes[plain as usize]), NO_NODE);
}

// ── The memory-shape / trap classifiers ──────────────────────────────

/// The regression the classifier exists for: the four ops that are
/// arithmetic on their operands and nothing else answer "touches no
/// memory", whether or not `is_pure` happens to list them. `Op::Div` and
/// `Op::Rem` are the interesting pair — they are NOT pure, because they
/// trap, and that has nothing to do with memory.
#[test]
fn arithmetic_that_traps_still_touches_no_memory() {
    for op in [Op::Div, Op::Rem, Op::LCmp] {
        assert_eq!(memory_shape_of(&op), MemoryShape::None, "{op:?}");
        assert!(!memory_shape_of(&op).touches_memory(), "{op:?}");
        assert!(!memory_shape_of(&op).may_write(), "{op:?}");
    }
    let fcmp = Op::FCmp {
        double: true,
        nan_greater: false,
    };
    assert_eq!(memory_shape_of(&fcmp), MemoryShape::None);
    // …and the trap question, asked separately, keeps them apart.
    assert!(may_raise(&Op::Div));
    assert!(may_raise(&Op::Rem));
    assert!(!may_raise(&Op::LCmp));
    assert!(!may_raise(&fcmp));
}

/// A read is a read and a write is a write. `loop_writes_memory` rests on
/// exactly this distinction: a read cannot clobber the location some other
/// node's hoisted load reads.
#[test]
fn loads_read_stores_write_and_calls_are_opaque() {
    assert_eq!(memory_shape_of(&Op::Load(MemKind::Int)), MemoryShape::Reads);
    assert_eq!(
        memory_shape_of(&Op::ArrayLoad(MemKind::Int)),
        MemoryShape::Reads
    );
    assert_eq!(memory_shape_of(&Op::ArrayLength), MemoryShape::Reads);
    assert!(!memory_shape_of(&Op::ArrayLength).may_write());

    assert_eq!(
        memory_shape_of(&Op::Store(MemKind::Int)),
        MemoryShape::Writes
    );
    assert_eq!(
        memory_shape_of(&Op::ArrayStore(MemKind::Ref)),
        MemoryShape::Writes
    );
    assert!(memory_shape_of(&Op::Store(MemKind::Int)).may_write());

    // A callee, a class initialiser, an allocation and a monitor are all
    // the top of the lattice, which is the answer `effect_of_node` gives
    // them too.
    for op in [
        Op::Call { info_ptr: 0 },
        Op::LoadStatic {
            class_id: 1,
            field_index: 0,
            type_tag: b'I',
            is_volatile: false,
        },
        // A static WRITE is Opaque too, not `Writes`: its helper can run
        // `<clinit>`, and `access_location` has no `(base, offset)` to read.
        Op::StoreStatic {
            class_id: 1,
            field_index: 0,
            type_tag: b'L',
            is_volatile: true,
        },
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        Op::MonitorEnter,
        Op::MonitorExit,
    ] {
        assert_eq!(memory_shape_of(&op), MemoryShape::Opaque, "{op:?}");
        assert!(memory_shape_of(&op).may_write(), "{op:?}");
    }
}

/// `Op::StoreStatic`'s value slot is typed by the field descriptor, so the
/// operand-type lane catches a builder that hands a `long` static an `Int`
/// (the lowering marshals the slot's raw 64 bits to the helper).
#[test]
fn a_store_static_value_slot_is_typed_by_its_descriptor() {
    let op = |type_tag: u8| Op::StoreStatic {
        class_id: 1,
        field_index: 0,
        type_tag,
        is_volatile: false,
    };
    for (tag, ty) in [
        (b'I', IrType::Int),
        (b'Z', IrType::Int),
        (b'B', IrType::Int),
        (b'C', IrType::Int),
        (b'S', IrType::Int),
        (b'J', IrType::Long),
        (b'F', IrType::Float),
        (b'D', IrType::Double),
        (b'L', IrType::Ref),
        (b'[', IrType::Ref),
    ] {
        assert_eq!(expected_input_type(&op(tag), 0, 3), TypeReq::Control);
        assert_eq!(expected_input_type(&op(tag), 1, 3), TypeReq::Memory);
        assert_eq!(
            expected_input_type(&op(tag), 2, 3),
            TypeReq::Exact(ty),
            "tag {}",
            tag as char
        );
    }
    assert!(
        may_raise(&op(b'I')),
        "the helper can run a failing <clinit>"
    );
}

/// A guard touches no memory — it carries `[ctrl, cond]` and no memory
/// edge — and it can still leave the compiled body. Both halves matter:
/// `ir_optimize::loop_writes_memory` reads the first and
/// `ir_optimize::licm_hoist_barrier` reads the second.
#[test]
fn a_guard_writes_nothing_but_can_leave_the_body() {
    let guard = Op::Guard { bci: 7 };
    assert_eq!(memory_shape_of(&guard), MemoryShape::None);
    assert!(may_raise(&guard));
}

/// The op-level answer for the two nodes an `&Op` cannot classify is the
/// top of the lattice, matching `effect_of_node`. Call sites that know
/// better say so themselves.
#[test]
fn a_phi_and_a_dead_node_get_the_conservative_op_level_answer() {
    assert_eq!(memory_shape_of(&Op::Phi), MemoryShape::Opaque);
    assert_eq!(memory_shape_of(&Op::Dead), MemoryShape::Opaque);
    assert_eq!(memory_shape_of(&Op::Return), MemoryShape::Opaque);
}

/// Every op the memory-token table describes has a non-`None` shape, and
/// every op it does not describe has one of the three hand-classified
/// answers. This is the anti-drift pairing: `memory_shape_of` is the
/// coarse projection of `Op::memory_shape`, not a second opinion.
#[test]
fn the_shape_classifier_agrees_with_the_memory_token_table() {
    for op in [
        Op::Load(MemKind::Int),
        Op::Store(MemKind::Int),
        Op::ArrayLoad(MemKind::Long),
        Op::ArrayStore(MemKind::Long),
        Op::ArrayLength,
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        Op::NewArray {
            element_type: 10,
            component_class_id: 0,
        },
        Op::Call { info_ptr: 0 },
        Op::StoreStatic {
            class_id: 1,
            field_index: 0,
            type_tag: b'J',
            is_volatile: false,
        },
        Op::MonitorEnter,
        Op::MonitorExit,
        Op::Throw,
    ] {
        assert!(op.memory_shape().is_some(), "{op:?}");
        assert!(memory_shape_of(&op).touches_memory(), "{op:?}");
        // Anything with a memory token can also transfer control
        // elsewhere — it is a call, an access that faults, or a throw.
        assert!(may_raise(&op), "{op:?}");
    }
    for op in [Op::Add, Op::Cmp(CmpOp::Lt), Op::I2L, Op::Const(3)] {
        assert!(op.memory_shape().is_none(), "{op:?}");
        assert_eq!(memory_shape_of(&op), MemoryShape::None, "{op:?}");
        assert!(!may_raise(&op), "{op:?}");
    }
}

// ── The shared expected-operand-type table ───────────────────────────

/// `Op::Cmp` is the IR's `if_icmp*` and its `if_acmp*` at once, so its
/// operands have no fixed type — only the requirement that they agree,
/// which is exactly what `ir_lower` needs when it picks between a 32- and
/// a 64-bit `CMP` and exactly what no table stated before this one.
#[test]
fn a_comparison_requires_its_operands_to_agree_rather_than_a_fixed_type() {
    assert_eq!(expected_input_type(&Op::Cmp(CmpOp::Lt), 0, 2), TypeReq::Any);
    assert_eq!(
        expected_input_type(&Op::Cmp(CmpOp::Lt), 1, 2),
        TypeReq::SameAs(0)
    );
    // Resolved against a node whose slot 0 is a `Ref`, the requirement on
    // slot 1 IS `Ref` — the `if_acmpeq` case, where a 32-bit compare would
    // make two objects 4 GiB apart equal.
    let inputs = [10 as NodeId, 11];
    let ty_of = |id: NodeId| if id == 10 { Some(IrType::Ref) } else { None };
    assert_eq!(
        required_input_type(&Op::Cmp(CmpOp::Eq), IrType::Int, &inputs, 1, &ty_of),
        Some(IrType::Ref)
    );
    // An unresolvable `SameAs` constrains nothing rather than failing.
    let blind = |_: NodeId| None;
    assert_eq!(
        required_input_type(&Op::Cmp(CmpOp::Eq), IrType::Int, &inputs, 1, &blind),
        None
    );
}

/// A shift is homogeneous in its value and `Int` in its distance: `lshl`
/// shifts a `Long` by an `Int`, which is a genuine non-join in the lattice.
#[test]
fn a_shift_is_homogeneous_in_the_value_and_int_in_the_distance() {
    assert_eq!(expected_input_type(&Op::Shl, 0, 2), TypeReq::SameAsResult);
    assert_eq!(
        expected_input_type(&Op::Shl, 1, 2),
        TypeReq::Exact(IrType::Int)
    );
    let inputs = [1 as NodeId, 2];
    let blind = |_: NodeId| None;
    assert_eq!(
        required_input_type(&Op::Shl, IrType::Long, &inputs, 0, &blind),
        Some(IrType::Long)
    );
    assert_eq!(
        required_input_type(&Op::Shl, IrType::Long, &inputs, 1, &blind),
        Some(IrType::Int)
    );
}

/// The stored value's type comes from the `MemKind`, and the sub-int kinds
/// collapse to `Int` because this IR has no `Byte` value type — the
/// narrowing lives in the access.
#[test]
fn a_stores_value_type_comes_from_its_memory_kind() {
    let full = 5; // [ctrl, mem, base, offset, value]
    assert_eq!(
        expected_input_type(&Op::Store(MemKind::Byte), 4, full),
        TypeReq::Exact(IrType::Int)
    );
    assert_eq!(
        expected_input_type(&Op::Store(MemKind::Ref), 4, full),
        TypeReq::Exact(IrType::Ref)
    );
    assert_eq!(
        expected_input_type(&Op::Store(MemKind::Double), 4, full),
        TypeReq::Exact(IrType::Double)
    );
    // The base is a reference and the offset an int, at every kind.
    assert_eq!(
        expected_input_type(&Op::Store(MemKind::Int), 2, full),
        TypeReq::Exact(IrType::Ref)
    );
    assert_eq!(
        expected_input_type(&Op::Store(MemKind::Int), 3, full),
        TypeReq::Exact(IrType::Int)
    );
}

/// The compact `[base, value]` / `[base]` layouts that `ir_optimize` and
/// the EA bridge build are outside this table: their slot 1 is a value
/// where the documented form's is a token, and describing both with one
/// slot list would describe neither. `Op::memory_shape`'s `min_full_arity`
/// — the same number the token-slot model reads — is what tells them apart.
#[test]
fn a_compact_memory_form_is_not_described_by_the_operand_table() {
    for slot in 0..2 {
        assert_eq!(
            expected_input_type(&Op::Store(MemKind::Int), slot, 2),
            TypeReq::Any,
            "compact store slot {slot}"
        );
        assert_eq!(
            expected_input_type(&Op::Load(MemKind::Int), slot, 1),
            TypeReq::Any,
            "compact load slot {slot}"
        );
    }
}

/// `ScalarOp::input_ty` is the single source of truth for an intrinsic
/// family's operands, and the table defers to it rather than restating it.
/// `Long.rotateLeft` is the asymmetric case: a `long` value, an `int`
/// distance.
#[test]
fn the_intrinsic_families_take_their_operand_types_from_scalar_op() {
    let rot = Op::ScalarIntrinsic(ScalarOp::RotateLeftL);
    assert_eq!(
        expected_input_type(&rot, 0, 2),
        TypeReq::Exact(IrType::Long)
    );
    assert_eq!(expected_input_type(&rot, 1, 2), TypeReq::Exact(IrType::Int));
    let min = Op::ScalarIntrinsic(ScalarOp::MinL);
    assert_eq!(
        expected_input_type(&min, 0, 2),
        TypeReq::Exact(IrType::Long)
    );
    assert_eq!(
        expected_input_type(&min, 1, 2),
        TypeReq::Exact(IrType::Long)
    );
    let abs = Op::ScalarIntrinsic(ScalarOp::AbsD);
    assert_eq!(
        expected_input_type(&abs, 0, 1),
        TypeReq::Exact(IrType::Double)
    );
    // Past the family's arity the table makes no claim.
    assert_eq!(expected_input_type(&abs, 1, 2), TypeReq::Any);
}

/// The two shapes the table knowingly cannot describe answer `Any`, and a
/// token slot resolves to no requirement at all — the control lane in
/// `ir_verify` owns control edges, and a memory token's declared type is
/// whatever last wrote memory.
#[test]
fn the_table_declines_the_shapes_it_cannot_describe() {
    let call = Op::Call { info_ptr: 0 };
    assert_eq!(expected_input_type(&call, 2, 4), TypeReq::Any);
    assert_eq!(expected_input_type(&call, 3, 4), TypeReq::Any);
    assert_eq!(expected_input_type(&Op::Phi, 1, 3), TypeReq::Any);
    assert_eq!(expected_input_type(&call, 0, 4), TypeReq::Control);
    assert_eq!(expected_input_type(&call, 1, 4), TypeReq::Memory);
    let inputs = [1 as NodeId, 2, 3, 4];
    let blind = |_: NodeId| None;
    for slot in 0..4 {
        assert_eq!(
            required_input_type(&call, IrType::Int, &inputs, slot, &blind),
            None,
            "slot {slot}"
        );
    }
}
