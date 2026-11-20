use super::*;

/// Regression test for `netty-javac-120-c1-door-off-loses-resource-symbol`:
/// when a loop exit is split across a guarded class check (`split_const_phi_ifs`),
/// and `plant_class_predicates` later plants a higher-`NodeId` `Op::Guard` on the
/// pre-header and folds the in-loop class check `If` to `Const(1)`, pruning the
/// dead loop-body arm and running `collapse_trivial_merges` must NOT collapse the
/// post-loop `Merge` into the pre-header when the post-loop block holds lower-`NodeId`
/// side-effecting nodes (`Load`, `Call`). Collapsing them into one block causes
/// `ir_schedule` (which chains same-block impure nodes in ascending `NodeId` order)
/// to schedule the post-loop `Call` BEFORE the pre-header `Guard`.
#[test]
fn collapse_trivial_merges_preserves_block_boundary_when_pred_has_newer_effects() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let recv = g.add(Op::Param(0), IrType::Ref, vec![start], None);

    // Pre-header Merge and loop header Merge.
    let pre = g.add(Op::Merge, IrType::Control, vec![c0], Some(96));
    let header = g.add(Op::Merge, IrType::Control, vec![pre], Some(96));

    // In-loop class check `If` whose hit arm (`Proj(0)`) exits the loop directly
    // to `post_merge` (the shape after `split_const_phi_ifs` on an empty iterator),
    // and whose miss arm (`Proj(1)`) loops back to `header`.
    let exact = g.add(
        Op::ExactClassIs { class_id: 1307 },
        IrType::Int,
        vec![recv],
        Some(96),
    );
    let iff = g.add(Op::If, IrType::Control, vec![header, exact], Some(96));
    let hit = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(96));
    let miss = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(96));
    g.nodes[header as usize].inputs.push(miss);

    // Post-loop Merge and post-loop `Call` allocated at bytecode build time
    // (so `post_call` has a LOWER `NodeId` than predicates planted later).
    let post_merge = g.add(Op::Merge, IrType::Control, vec![hit], Some(255));
    let post_call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![post_merge, m0, recv],
        Some(260),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![post_merge], Some(270));
    g.exit = ret;

    // Later in `optimize_with`: `plant_class_predicates` plants a higher-`NodeId`
    // `Op::Guard` on `pre` and folds `iff`'s condition to `Const(1)`.
    let guard_cond = g.add(
        Op::ExactClassIs { class_id: 1307 },
        IrType::Int,
        vec![recv],
        Some(96),
    );
    let pre_guard = g.add(
        Op::Guard { bci: 96 },
        IrType::Void,
        vec![pre, guard_cond],
        Some(96),
    );
    assert!(
        pre_guard > post_call,
        "planted pre-header guard must have a higher NodeId than the post-loop call"
    );
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    g.nodes[iff as usize].inputs[1] = one;

    // Prune the folded `If` and run `collapse_trivial_merges`.
    prune_constant_ifs(&mut g);
    collapse_trivial_merges(&mut g);

    let sched = crate::ir_schedule::schedule(&g);
    let guard_block = sched.node_to_block[pre_guard as usize];
    let call_block = sched.node_to_block[post_call as usize];
    if guard_block == call_block {
        let nodes = &sched.blocks[guard_block].nodes;
        let guard_pos = nodes.iter().position(|&n| n == pre_guard).unwrap();
        let call_pos = nodes.iter().position(|&n| n == post_call).unwrap();
        assert!(
            guard_pos < call_pos,
            "pre-header Guard (pos {guard_pos}) must execute before post-loop Call (pos {call_pos})"
        );
    } else {
        let dom = crate::ir_schedule::Dominators::compute(&sched.blocks);
        assert!(
            dom.dominates(guard_block, call_block),
            "pre-header Guard block ({guard_block}) must strictly dominate post-loop Call block ({call_block})"
        );
    }
}

#[test]
fn prune_constant_ifs_and_unroll_preserve_block_boundary_when_pred_has_newer_effects() {
    // Direct effect on `live_proj` (no intermediate `Merge`) with newer effect on `if_ctrl`.
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let recv = g.add(Op::Param(0), IrType::Ref, vec![start], None);

    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iff = g.add(Op::If, IrType::Control, vec![c0, one], Some(10));
    let hit = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(10));
    let _miss = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(10));
    let post_call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![hit, m0, recv],
        Some(20),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![hit], Some(30));
    g.exit = ret;

    let guard_cond = g.add(
        Op::ExactClassIs { class_id: 42 },
        IrType::Int,
        vec![recv],
        Some(5),
    );
    let pre_guard = g.add(
        Op::Guard { bci: 5 },
        IrType::Void,
        vec![c0, guard_cond],
        Some(5),
    );
    assert!(pre_guard > post_call);

    prune_constant_ifs(&mut g);
    collapse_trivial_merges(&mut g);

    let sched = crate::ir_schedule::schedule(&g);
    let guard_block = sched.node_to_block[pre_guard as usize];
    let call_block = sched.node_to_block[post_call as usize];
    assert_ne!(guard_block, call_block);
    let dom = crate::ir_schedule::Dominators::compute(&sched.blocks);
    assert!(dom.dominates(guard_block, call_block));
}

#[test]
fn hoist_pure_loop_class_init_guards_hoists_header_and_skips_zero_trip_or_storing_loop() {
    // 1. A ClassInitTest at `region` (runs on iteration 0 before any exit) hoists.
    let mut g = Graph::empty(32);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);

    let region = g.add(Op::Region, IrType::Control, vec![c0], Some(10));
    let snap = crate::ir::SafepointSnapshot::new(10, vec![n], vec![], vec![]);
    g.push_safepoint(snap);
    assert!(g.set_node_frame_snapshot(region, 0));

    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let test = g.add(
        Op::ClassInitTest {
            state_addr: 0x1000,
            initialized: 1,
        },
        IrType::Int,
        vec![region],
        Some(10),
    );
    let in_guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![region, test], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(11));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(11));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(11));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(11));
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(14));
    assert!(g.push_input(region, body));
    assert!(g.push_input(i_phi, i_next));
    let ret = g.add(Op::Return, IrType::Void, vec![exit], Some(20));
    g.exit = ret;

    let hoisted = hoist_pure_loop_class_init_guards(&mut g);
    assert_eq!(hoisted, 1, "header ClassInitTest in pure loop should hoist");
    let new_cond = g.nodes[in_guard as usize].inputs[1];
    assert!(matches!(g.nodes[new_cond as usize].op, Op::Const(1)));

    // 2. A ClassInitTest inside post-condition `body` (`while (i < n) { new Foo(); }`)
    // MUST NOT hoist above `i < n` (when n <= 0 the body never runs, soJVMS §5.5
    // forbids trapping/initializing `Foo` at the loop header).
    let mut g_zero_trip = Graph::empty(32);
    let sz = g_zero_trip.add(Op::Start, IrType::Void, vec![], None);
    let cz = g_zero_trip.add(Op::Proj(0), IrType::Control, vec![sz], None);
    let nz = g_zero_trip.add(Op::Param(0), IrType::Int, vec![sz], None);
    let z0 = g_zero_trip.add(Op::Const(0), IrType::Int, vec![], None);
    let z1 = g_zero_trip.add(Op::Const(1), IrType::Int, vec![], None);
    let rz = g_zero_trip.add(Op::Region, IrType::Control, vec![cz], Some(10));
    g_zero_trip.push_safepoint(crate::ir::SafepointSnapshot::new(10, vec![nz], vec![], vec![]));
    assert!(g_zero_trip.set_node_frame_snapshot(rz, 0));
    let iz = g_zero_trip.add(Op::Phi, IrType::Int, vec![rz, z0], Some(10));
    let condz = g_zero_trip.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iz, nz], Some(10));
    let iffz = g_zero_trip.add(Op::If, IrType::Control, vec![rz, condz], Some(10));
    let bz = g_zero_trip.add(Op::Proj(0), IrType::Control, vec![iffz], Some(10));
    let ez = g_zero_trip.add(Op::Proj(1), IrType::Control, vec![iffz], Some(10));
    let tz = g_zero_trip.add(
        Op::ClassInitTest {
            state_addr: 0x1000,
            initialized: 1,
        },
        IrType::Int,
        vec![bz],
        Some(12),
    );
    let _gz = g_zero_trip.add(Op::Guard { bci: 12 }, IrType::Void, vec![bz, tz], Some(12));
    let iz_next = g_zero_trip.add(Op::Add, IrType::Int, vec![iz, z1], Some(14));
    assert!(g_zero_trip.push_input(rz, bz));
    assert!(g_zero_trip.push_input(iz, iz_next));
    g_zero_trip.exit = g_zero_trip.add(Op::Return, IrType::Void, vec![ez], Some(20));
    assert_eq!(
        hoist_pure_loop_class_init_guards(&mut g_zero_trip),
        0,
        "post-check body ClassInitTest must not hoist onto 0-trip loop entry"
    );

    // 3. Adding a Store in the loop must block hoisting even at `region`.
    let mut g2 = Graph::empty(32);
    let s2 = g2.add(Op::Start, IrType::Void, vec![], None);
    let c2 = g2.add(Op::Proj(0), IrType::Control, vec![s2], None);
    let m2 = g2.add(Op::Proj(1), IrType::Memory, vec![s2], None);
    let obj2 = g2.add(Op::Param(0), IrType::Ref, vec![s2], None);
    let r2 = g2.add(Op::Region, IrType::Control, vec![c2], Some(10));
    let snap2 = crate::ir::SafepointSnapshot::new(10, vec![], vec![], vec![]);
    g2.push_safepoint(snap2);
    assert!(g2.set_node_frame_snapshot(r2, 0));
    let z2 = g2.add(Op::Const(0), IrType::Int, vec![], None);
    let t2 = g2.add(
        Op::ClassInitTest {
            state_addr: 0x1000,
            initialized: 1,
        },
        IrType::Int,
        vec![r2],
        Some(10),
    );
    let _g2 = g2.add(Op::Guard { bci: 10 }, IrType::Void, vec![r2, t2], Some(10));
    let iff2 = g2.add(Op::If, IrType::Control, vec![r2, z2], Some(11));
    let b2 = g2.add(Op::Proj(0), IrType::Control, vec![iff2], Some(11));
    let e2 = g2.add(Op::Proj(1), IrType::Control, vec![iff2], Some(11));
    let _st2 = g2.add(Op::Store(crate::ir::MemKind::Int), IrType::Memory, vec![b2, m2, obj2, z2, z2], Some(13));
    assert!(g2.push_input(r2, b2));
    let ret2 = g2.add(Op::Return, IrType::Void, vec![e2], Some(20));
    g2.exit = ret2;
    let _ = m0;
    assert_eq!(hoist_pure_loop_class_init_guards(&mut g2), 0, "store in loop must block hoist");
}

#[test]
fn hoist_invariant_loads_skips_non_dominating_instanceof_branch_and_class_word_guard() {
    // Case A: non-null receiver `this` (`receiver_param = Some(0)`), loop contains
    // `if (this instanceof Sub) { sum += ((Sub) this).field7; }`.
    // `if_true` does NOT dominate the back-edge (`Merge`), so hoisting `field7`
    // to `c0` reads out-of-bounds on base instances!
    let mut g = Graph::empty(48);
    g.receiver_param = Some(0);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let off7 = g.add(Op::Const(7), IrType::Int, vec![], None);

    let region = g.add(Op::Region, IrType::Control, vec![c0], Some(10));
    g.push_safepoint(crate::ir::SafepointSnapshot::new(10, vec![base, n], vec![], vec![]));
    assert!(g.set_node_frame_snapshot(region, 0));
    let m_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0], Some(10));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let loop_cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(10));
    let loop_if = g.add(Op::If, IrType::Control, vec![region, loop_cond], Some(10));
    let loop_body = g.add(Op::Proj(0), IrType::Control, vec![loop_if], Some(10));
    let loop_exit = g.add(Op::Proj(1), IrType::Control, vec![loop_if], Some(10));

    let is_sub = g.add(Op::ExactClassIs { class_id: 99 }, IrType::Int, vec![base], Some(12));
    let sub_if = g.add(Op::If, IrType::Control, vec![loop_body, is_sub], Some(12));
    let sub_then = g.add(Op::Proj(0), IrType::Control, vec![sub_if], Some(12));
    let sub_else = g.add(Op::Proj(1), IrType::Control, vec![sub_if], Some(12));
    let sub_load = g.add(
        Op::Load(crate::ir::MemKind::Int),
        IrType::Int,
        vec![sub_then, m_phi, base, off7],
        Some(14),
    );
    let merge = g.add(Op::Merge, IrType::Control, vec![sub_then, sub_else], Some(16));
    let m_join = g.add(Op::Phi, IrType::Memory, vec![merge, sub_load, m_phi], Some(16));
    let val_join = g.add(Op::Phi, IrType::Int, vec![merge, sub_load, zero], Some(16));
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(18));
    let _keep = g.add(Op::Add, IrType::Int, vec![i_next, val_join], Some(18));
    assert!(g.push_input(region, merge));
    assert!(g.push_input(m_phi, m_join));
    assert!(g.push_input(i_phi, i_next));
    g.exit = g.add(Op::Return, IrType::Void, vec![loop_exit, i_phi], Some(22));

    assert_eq!(
        super::plant_null_base_load_predicates::hoist_invariant_guards_and_loads(&mut g, false),
        0,
        "load inside non-dominating instanceof branch must never hoist into preheader"
    );

    // Case B: dominating `Guard(Cmp(Eq, ClassWordOf(base), C))` that could not be hoisted
    // (e.g., `ClassWordOf` is not hoisted by Step A): Step B must NOT hoist `sub_load`
    // ahead of the un-hoisted class-word guard!
    let mut g2 = Graph::empty(48);
    g2.receiver_param = Some(0);
    let s2 = g2.add(Op::Start, IrType::Void, vec![], None);
    let c2 = g2.add(Op::Proj(0), IrType::Control, vec![s2], None);
    let m2 = g2.add(Op::Proj(1), IrType::Memory, vec![s2], None);
    let b2 = g2.add(Op::Param(0), IrType::Ref, vec![s2], None);
    let n2 = g2.add(Op::Param(1), IrType::Int, vec![s2], None);
    let z2 = g2.add(Op::Const(0), IrType::Int, vec![], None);
    let o2 = g2.add(Op::Const(1), IrType::Int, vec![], None);
    let c99 = g2.add(Op::Const(99), IrType::Int, vec![], None);
    let f7 = g2.add(Op::Const(7), IrType::Int, vec![], None);

    let r2 = g2.add(Op::Region, IrType::Control, vec![c2], Some(10));
    g2.push_safepoint(crate::ir::SafepointSnapshot::new(10, vec![b2, n2], vec![], vec![]));
    assert!(g2.set_node_frame_snapshot(r2, 0));
    let mp2 = g2.add(Op::Phi, IrType::Memory, vec![r2, m2], Some(10));
    let ip2 = g2.add(Op::Phi, IrType::Int, vec![r2, z2], Some(10));
    let lc2 = g2.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![ip2, n2], Some(10));
    let lif2 = g2.add(Op::If, IrType::Control, vec![r2, lc2], Some(10));
    let lb2 = g2.add(Op::Proj(0), IrType::Control, vec![lif2], Some(10));
    let le2 = g2.add(Op::Proj(1), IrType::Control, vec![lif2], Some(10));

    // Add a non-invariant dummy input or ClassWordOf inside the loop where Step A stops
    // after 24 guards — easiest: `ClassWordOf(b2)` compared against loop-variant `ip2`
    // or saturate MAX_GUARDS_PER_LOOP? Wait: `ClassWordOf(b2) == 99` IS invariant in Step A!
    // What if `header_bci` is in `despec` (so Step A refuses to hoist the guard), OR
    // `MAX_GUARDS_PER_LOOP` (24) is already reached? Let's test `MAX_GUARDS_PER_LOOP`
    // by checking that if a `Guard(Cmp(Eq, ClassWordOf(b2), c99))` remains in `body`,
    // `sub_load` is NOT hoisted past it!
    for k in 0..24 {
        let ck = g2.add(Op::Const(1000 + k), IrType::Int, vec![], None);
        let ne = g2.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![n2, ck], Some(11));
        g2.add(Op::Guard { bci: 11 }, IrType::Void, vec![lb2, ne], Some(11));
    }
    let cw = g2.add(Op::ClassWordOf, IrType::Int, vec![b2], Some(12));
    let cw_eq = g2.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![cw, c99], Some(12));
    let cw_guard = g2.add(Op::Guard { bci: 12 }, IrType::Void, vec![lb2, cw_eq], Some(12));
    let load2 = g2.add(
        Op::Load(crate::ir::MemKind::Int),
        IrType::Int,
        vec![lb2, mp2, b2, f7],
        Some(14),
    );
    let inext2 = g2.add(Op::Add, IrType::Int, vec![ip2, o2], Some(16));
    let inext3 = g2.add(Op::Add, IrType::Int, vec![inext2, load2], Some(16));
    assert!(g2.push_input(r2, lb2));
    assert!(g2.push_input(mp2, load2));
    assert!(g2.push_input(ip2, inext3));
    g2.exit = g2.add(Op::Return, IrType::Void, vec![le2, ip2], Some(20));

    let _ = super::plant_null_base_load_predicates::hoist_invariant_guards_and_loads(&mut g2, false);
    assert!(
        !matches!(g2.nodes[g2.nodes[cw_guard as usize].inputs[1] as usize].op, Op::Const(1)),
        "25th guard stays in loop body"
    );
    let preheader_loads = g2
        .nodes
        .iter()
        .filter(|nd| matches!(nd.op, Op::Load(_)) && nd.inputs.first() == Some(&c2))
        .count();
    assert_eq!(
        preheader_loads, 0,
        "load must not hoist past un-hoisted ClassWordOf Cmp(Eq) guard"
    );
}

#[test]
fn hoist_invariant_loads_attaches_licensing_guard_and_keeps_users_fresh_for_null_checked_branch() {
    let mut g = Graph::empty(48);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let root = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let child = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let n_lim = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);

    let region = g.add(Op::Region, IrType::Control, vec![c0], Some(10));
    let m_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0], Some(10));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    g.push_safepoint(crate::ir::SafepointSnapshot::new(
        10,
        vec![root, child, n_lim, i_phi],
        vec![],
        vec![],
    ));
    assert!(g.set_node_frame_snapshot(region, 0));

    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n_lim], Some(10));
    let loop_if = g.add(Op::If, IrType::Control, vec![region, cond], Some(10));
    let iter_ctrl = g.add(Op::Proj(0), IrType::Control, vec![loop_if], Some(10));
    let exit_ctrl = g.add(Op::Proj(1), IrType::Control, vec![loop_if], Some(20));

    // Dominating load (`root.f0`): hoists first, allocating new nodes in `g`.
    let dom_load = g.add(
        Op::Load(crate::ir::MemKind::Int),
        IrType::Int,
        vec![iter_ctrl, m_phi, root, f0],
        Some(12),
    );
    // Conditional `if (child != null)` load (`child.f0`): MUST NOT hoist!
    let ne_child = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![child, null], Some(14));
    let null_if = g.add(Op::If, IrType::Control, vec![iter_ctrl, ne_child], Some(14));
    let non_null_arm = g.add(Op::Proj(0), IrType::Control, vec![null_if], Some(15));
    let null_arm = g.add(Op::Proj(1), IrType::Control, vec![null_if], Some(17));
    let guarded_load = g.add(
        Op::Load(crate::ir::MemKind::Int),
        IrType::Int,
        vec![non_null_arm, dom_load, child, f0],
        Some(15),
    );
    let merge = g.add(Op::Merge, IrType::Control, vec![non_null_arm, null_arm], Some(18));
    let m_join = g.add(Op::Phi, IrType::Memory, vec![merge, guarded_load, dom_load], Some(18));
    let v_join = g.add(Op::Phi, IrType::Int, vec![merge, guarded_load, zero], Some(18));
    let sum = g.add(Op::Add, IrType::Int, vec![dom_load, v_join], Some(18));
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(19));
    let i_next2 = g.add(Op::Add, IrType::Int, vec![i_next, sum], Some(19));
    assert!(g.push_input(region, merge));
    assert!(g.push_input(m_phi, m_join));
    assert!(g.push_input(i_phi, i_next2));
    g.exit = g.add(Op::Return, IrType::Void, vec![exit_ctrl, i_phi], Some(20));

    let moved = super::plant_null_base_load_predicates::hoist_invariant_guards_and_loads(&mut g, false);
    assert_eq!(moved, 1, "only dominating root.f0 should hoist; child.f0 inside if (child != null) must stay");
    assert_eq!(
        g.nodes[guarded_load as usize].inputs[0], non_null_arm,
        "child.f0 must remain anchored inside non_null_arm"
    );
    let hoisted: Vec<&crate::ir::Node> = g
        .nodes
        .iter()
        .filter(|nd| matches!(nd.op, Op::Load(_)) && nd.inputs.first() == Some(&c0))
        .collect();
    assert_eq!(hoisted.len(), 1);
    let tok = crate::ir::guard_token_of(hoisted[0]);
    assert_ne!(tok, crate::ir::NO_NODE, "hoisted load must carry preheader Guard(root != null) token");
    assert!(matches!(g.nodes[tok as usize].op, Op::Guard { .. }));
}
