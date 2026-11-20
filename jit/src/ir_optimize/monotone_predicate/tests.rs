use super::*;
use crate::ir::MemKind;

/// The decode loop's shape (see `ir_check_elim::nio21w3_monotone_tests`),
/// with a header snapshot:
///
/// ```java
/// while (sp < sl) {
///     int b = sa[sp];
///     if (b >= 0) { if (dp >= dl) return; da[dp] = (char) b; sp++; dp++; }
///     else { if (dl - dp < 2) return; sp += 4; dp += 2; }
/// }
/// ```
/// Returns `(graph, region, entry ctrl, [sa, da, sp0, dp0, sl, dl])`.
fn decode_loop() -> (Graph, NodeId, NodeId, [NodeId; 6]) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let sa = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let da = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let sp0 = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let dp0 = g.add(Op::Param(3), IrType::Int, vec![start], None);
    let sl = g.add(Op::Param(4), IrType::Int, vec![start], None);
    let dl = g.add(Op::Param(5), IrType::Int, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(103));
    let sp = g.add(Op::Phi, IrType::Int, vec![region, sp0], None);
    let dp = g.add(Op::Phi, IrType::Int, vec![region, dp0], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let two = g.add(Op::Const(2), IrType::Int, vec![], None);
    let four = g.add(Op::Const(4), IrType::Int, vec![], None);
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![sp, sl], None);
    let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(107));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
    let b = g.add(
        Op::ArrayLoad(MemKind::Byte),
        IrType::Int,
        vec![stay, mem0, sa, sp],
        Some(113),
    );
    let neg = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![b, zero], None);
    let arm = g.add(Op::If, IrType::Control, vec![stay, neg], Some(118));
    let multi = g.add(Op::Proj(0), IrType::Control, vec![arm], None);
    let ascii = g.add(Op::Proj(1), IrType::Control, vec![arm], None);
    // ASCII: `if (dp < dl) { da[dp] = b; }` on the true edge.
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![dp, dl], None);
    let room1 = g.add(Op::If, IrType::Control, vec![ascii, lt], Some(125));
    let store_ok = g.add(Op::Proj(0), IrType::Control, vec![room1], None);
    let full1 = g.add(Op::Proj(1), IrType::Control, vec![room1], None);
    let _st = g.add(
        Op::ArrayStore(MemKind::Char),
        IrType::Void,
        vec![store_ok, mem0, da, dp, b],
        Some(151),
    );
    let sp1 = g.add(Op::Add, IrType::Int, vec![sp, one], None);
    let dp1 = g.add(Op::Add, IrType::Int, vec![dp, one], None);
    // Multi: `if (dl - dp < 2) return;`.
    let rem = g.add(Op::Sub, IrType::Int, vec![dl, dp], None);
    let short = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![rem, two], None);
    let room2 = g.add(Op::If, IrType::Control, vec![multi, short], Some(453));
    let full2 = g.add(Op::Proj(0), IrType::Control, vec![room2], None);
    let wide_ok = g.add(Op::Proj(1), IrType::Control, vec![room2], None);
    let sp4 = g.add(Op::Add, IrType::Int, vec![sp, four], None);
    let dp2 = g.add(Op::Add, IrType::Int, vec![dp1, one], None);
    let latch = g.add(
        Op::Merge,
        IrType::Control,
        vec![store_ok, wide_ok],
        Some(672),
    );
    let spn = g.add(Op::Phi, IrType::Int, vec![latch, sp1, sp4], None);
    let dpn = g.add(Op::Phi, IrType::Int, vec![latch, dp1, dp2], None);
    g.nodes[region as usize].inputs.push(latch);
    g.nodes[sp as usize].inputs.push(spn);
    g.nodes[dp as usize].inputs.push(dpn);
    let out = g.add(Op::Merge, IrType::Control, vec![exit, full1, full2], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![out], Some(675));
    // The header's own snapshot: locals `[sa, sp, sl, da, dp, dl]`.
    g.push_safepoint(SafepointSnapshot::new(
        103,
        vec![sa, sp, sl, da, dp, dl],
        vec![],
        vec![],
    ));
    (g, region, entry, [sa, da, sp0, dp0, sl, dl])
}

#[test]
fn a_decode_loop_gets_length_and_sign_guards() {
    let (g, region, entry, [sa, da, sp0, dp0, sl, dl]) = decode_loop();
    let Some((_, e, backs)) = loop_headers(&g).into_iter().find(|(r, _, _)| *r == region)
    else {
        panic!("the hand-built loop must be recognised");
    };
    assert_eq!(e, entry);
    let plan = monotone_plan(&g, region, entry, &backs).expect("a candidate loop");
    assert_eq!(plan.header_bci, 103);
    assert!(plan.bounds.contains(&(sl, vec![sa])));
    assert!(plan.bounds.contains(&(dl, vec![da])));
    // Both starts are unknown; `dl` bounds `dl - dp` and is not the header
    // test's bound, `sl` is.
    assert!(plan.non_negative.contains(&sp0));
    assert!(plan.non_negative.contains(&dp0));
    assert!(plan.non_negative.contains(&dl));
    assert!(!plan.non_negative.contains(&sl));
    // The entry frame names the φs at their entry values.
    assert!(plan.frame.locals.contains(&sp0) && plan.frame.locals.contains(&dp0));
}

#[test]
fn installing_a_plan_names_every_guard_with_the_entry_frame() {
    let (mut g, region, entry, _) = decode_loop();
    let backs = loop_headers(&g)
        .into_iter()
        .find(|(r, _, _)| *r == region)
        .map(|(_, _, b)| b)
        .expect("the loop is recognised");
    let plan = monotone_plan(&g, region, entry, &backs).expect("a candidate loop");
    let before = g.nodes.len();
    let n = install_monotone_plan(&mut g, plan, MAX_MONOTONE_GUARDS);
    // Two length guards, three sign guards.
    assert_eq!(n, 5);
    let guards: Vec<&crate::ir::Node> = g.nodes[before..]
        .iter()
        .filter(|n| matches!(n.op, Op::Guard { .. }))
        .collect();
    assert_eq!(guards.len(), 5);
    assert!(guards
        .iter()
        .all(|n| n.inputs.first() == Some(&entry) && n.frame_snapshot.is_some()));
}

#[test]
fn a_budget_of_two_plants_only_the_length_guards() {
    let (mut g, region, entry, _) = decode_loop();
    let backs = loop_headers(&g)
        .into_iter()
        .find(|(r, _, _)| *r == region)
        .map(|(_, _, b)| b)
        .expect("the loop is recognised");
    let plan = monotone_plan(&g, region, entry, &backs).expect("a candidate loop");
    assert_eq!(install_monotone_plan(&mut g, plan, 2), 2);
}

#[test]
fn a_non_monotone_index_loses_only_its_own_guard() {
    let (mut g, region, entry, _) = decode_loop();
    // `sp - 1` on the multi arm: no longer the φ plus constants.
    let Some(spn) = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Phi && n.inputs.len() == 3 && n.inputs[0] != region)
    else {
        panic!("the latch φ exists");
    };
    let minus = g.add(Op::Const(-1), IrType::Int, vec![], None);
    let sp = g.nodes[g.nodes[spn].inputs[1] as usize].inputs[0];
    let back = g.add(Op::Add, IrType::Int, vec![sp, minus], None);
    g.nodes[spn].inputs[2] = back;
    // `dp` is still monotone, so its own guard survives; `sa[sp]` loses
    // its guard.
    let backs = loop_headers(&g)
        .into_iter()
        .find(|(r, _, _)| *r == region)
        .map(|(_, _, b)| b)
        .expect("the loop is recognised");
    let plan = monotone_plan(&g, region, entry, &backs).expect("dp still plans");
    assert_eq!(plan.bounds.len(), 1);
}

/// JIT r23 wave 7b: `for (i = i0; i < a.length; i++) s += a[i]`, `i0` an
/// unknown `int` (the OSR entry merge's φ in the real body). Returns
/// `(graph, region, entry ctrl, i0, the element load)`.
fn own_length_loop(init_const: Option<i64>) -> (Graph, NodeId, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let i0 = match init_const {
        Some(c) => g.add(Op::Const(c), IrType::Int, vec![], None),
        None => g.add(Op::Param(1), IrType::Int, vec![start], None),
    };
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let len = g.add(Op::ArrayLength, IrType::Int, vec![entry, mem0, a], Some(4));
    let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(10));
    let i = g.add(Op::Phi, IrType::Int, vec![region, i0], None);
    let s = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, len], None);
    let head = g.add(Op::If, IrType::Control, vec![region, lt], Some(12));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![head], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![head], None);
    let el = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![stay, mem0, a, i],
        Some(15),
    );
    let s1 = g.add(Op::Add, IrType::Int, vec![s, el], None);
    let i1 = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[region as usize].inputs.push(stay);
    g.nodes[i as usize].inputs.push(i1);
    g.nodes[s as usize].inputs.push(s1);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, s], Some(30));
    g.push_safepoint(SafepointSnapshot::new(10, vec![a, i, s], vec![], vec![]));
    (g, region, entry, i0, el)
}

fn backs_of(g: &Graph, region: NodeId) -> Vec<NodeId> {
    loop_headers(g)
        .into_iter()
        .find(|(r, _, _)| *r == region)
        .map(|(_, _, b)| b)
        .unwrap_or_default()    }