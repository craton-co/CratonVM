use super::*;
use crate::ir::MemKind;

/// `encodeBufferLoop`'s ASCII arm after the field forward, reduced:
///
/// ```java
/// while (j < n) {              // header, bci 5
///     if (p < limit) {         // dst.hasRemaining()
///         hb[p + offset] = b;  // dst.put(b)
///         p++;
///     } else return;
///     j++;
/// }
/// ```
/// `hb`, `offset`, `limit`, `p0` are parameters (the forward's
/// pre-header values). Returns `(graph, region, entry, [hb, offset,
/// limit, p0], the store)`.
fn buffer_loop() -> (Graph, NodeId, NodeId, [NodeId; 4], NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let hb = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let offset = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let limit = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let p0 = g.add(Op::Param(3), IrType::Int, vec![start], None);
    let n = g.add(Op::Param(4), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
    let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
    let j = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let p = g.add(Op::Phi, IrType::Int, vec![region, p0], None);
    let mem = g.add(Op::Phi, IrType::Memory, vec![region, mem0], None);
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![j, n], None);
    let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![p, limit], None);
    let room = g.add(Op::If, IrType::Control, vec![stay, lt], Some(12));
    let ok = g.add(Op::Proj(0), IrType::Control, vec![room], None);
    let full = g.add(Op::Proj(1), IrType::Control, vec![room], None);
    let ix = g.add(Op::Add, IrType::Int, vec![p, offset], None);
    let st = g.add(
        Op::ArrayStore(MemKind::Byte),
        IrType::Void,
        vec![ok, mem, hb, ix, seven],
        Some(20),
    );
    let p1 = g.add(Op::Add, IrType::Int, vec![p, one], None);
    let j1 = g.add(Op::Add, IrType::Int, vec![j, one], None);
    g.nodes[region as usize].inputs.push(ok);
    g.nodes[j as usize].inputs.push(j1);
    g.nodes[p as usize].inputs.push(p1);
    g.nodes[mem as usize].inputs.push(st);
    let out = g.add(Op::Merge, IrType::Control, vec![exit, full], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![out], Some(30));
    g.push_safepoint(SafepointSnapshot::new(
        5,
        vec![hb, offset, limit, p, j, n],
        vec![],
        vec![],
    ));
    (g, region, entry, [hb, offset, limit, p0], st)
}

fn plan_of(g: &Graph, region: NodeId, entry: NodeId) -> Option<OffsetPlan> {
    let backs = loop_headers(g)
        .into_iter()
        .find(|(r, _, _)| *r == region)
        .map(|(_, _, b)| b)
        .expect("the loop is recognised");
    offset_plan(g, region, entry, &backs)
}

#[test]
fn nio21w6_a_buffer_store_gets_a_room_guard_and_two_sign_guards() {
    let (g, region, entry, [hb, offset, limit, p0], _) = buffer_loop();
    let plan = plan_of(&g, region, entry).expect("a candidate loop");
    assert_eq!(plan.header_bci, 5);
    assert_eq!(plan.rooms, vec![(limit, hb, offset)]);
    assert!(plan.non_negative.contains(&offset));
    assert!(plan.non_negative.contains(&p0));
    assert_eq!(plan.non_negative.len(), 2);
    // The entry frame names `p` at its entry value.
    assert!(plan.frame.locals.contains(&p0));
}

#[test]
fn nio21w6_installing_names_every_guard_with_the_entry_frame() {
    let (mut g, region, entry, [hb, offset, limit, _], _) = buffer_loop();
    let plan = plan_of(&g, region, entry).expect("a candidate loop");
    let before = g.nodes.len();
    let snaps = g.safepoints.len();
    assert_eq!(install_offset_plan(&mut g, plan, MAX_OFFSET_GUARDS), 3);
    assert_eq!(g.safepoints.len(), snaps + 1);
    let guards: Vec<&crate::ir::Node> = g.nodes[before..]
        .iter()
        .filter(|n| matches!(n.op, Op::Guard { bci: 5 }))
        .collect();
    assert_eq!(guards.len(), 3);
    assert!(guards
        .iter()
        .all(|n| n.inputs.first() == Some(&entry) && n.frame_snapshot.is_some()));
    // The room guard: `limit <= hb.length - offset`.
    let room = guards
        .iter()
        .find_map(|gd| {
            let c = &g.nodes[gd.inputs[1] as usize];
            (c.inputs.first() == Some(&limit)).then_some(c.clone())
        })
        .expect("the room guard");
    assert_eq!(room.op, Op::Cmp(CmpOp::Le));
    let sub = &g.nodes[room.inputs[1] as usize];
    assert_eq!(sub.op, Op::Sub);
    assert_eq!(sub.inputs[1], offset);
    let len = &g.nodes[sub.inputs[0] as usize];
    assert_eq!(len.op, Op::ArrayLength);
    assert_eq!(len.inputs[2], hb);
    assert!(len.frame_snapshot.is_some());
}

#[test]
fn nio21w6_a_budget_too_small_plants_nothing() {
    let (mut g, region, entry, _, _) = buffer_loop();
    let plan = plan_of(&g, region, entry).expect("a candidate loop");
    let snaps = g.safepoints.len();
    assert_eq!(install_offset_plan(&mut g, plan, 2), 0);
    assert_eq!(g.safepoints.len(), snaps);
}

#[test]
fn nio21w6_a_constant_offset_or_a_varying_one_plants_nothing() {
    // A constant offset is the existing arms' business.
    let (mut g, region, entry, [_, offset, _, _], st) = buffer_loop();
    let c = g.add(Op::Const(3), IrType::Int, vec![], None);
    let ix = g.nodes[st as usize].inputs[3];
    let k = g.nodes[ix as usize]
        .inputs
        .iter()
        .position(|&x| x == offset)
        .expect("the offset operand");
    g.nodes[ix as usize].inputs[k] = c;
    assert!(plan_of(&g, region, entry).is_none());
    // An offset computed in the loop is not invariant.
    let (mut g, region, entry, _, st) = buffer_loop();
    let ix = g.nodes[st as usize].inputs[3];
    let p = g.nodes[ix as usize].inputs[0];
    g.nodes[ix as usize].inputs[1] = p;
    assert!(plan_of(&g, region, entry).is_none());
}

#[test]
fn nio21w6_the_whole_pass_honours_the_despec_registry() {
    let (mut g, _, _, _, _) = buffer_loop();
    let despec = crate::deopt::DespecRegistry::new();
    despec.insert("T.m:()V", 5, crate::deopt::SPECULATION_ID_ANY);
    assert_eq!(plant_offset_predicates(&mut g, &despec, "T.m:()V"), 0);
    let clean = crate::deopt::DespecRegistry::new();
    assert_eq!(plant_offset_predicates(&mut g, &clean, "T.m:()V"), 3);
}


/// The proof needs every guard: drop the `0 <= offset` one and the store
/// keeps its check (`hb.length - offset` may have wrapped).
#[test]
fn nio21w6_the_store_needs_the_offset_sign_guard() {
    let (mut g, _, _, [_, offset, _, _], st) = buffer_loop();
    let clean = crate::deopt::DespecRegistry::new();
    assert_eq!(plant_offset_predicates(&mut g, &clean, "T.m:()V"), 3);
    let sign = g
        .nodes
        .iter()
        .position(|n| {
            matches!(n.op, Op::Guard { .. })
                && g.node_opt(n.inputs[1]).is_some_and(|c| {
                    c.op == Op::Cmp(CmpOp::Le) && c.inputs.get(1) == Some(&offset)
                })
        })
        .expect("the offset's sign guard");
    g.kill(sign as NodeId);
    let s = crate::ir_schedule::schedule(&g);
    let e = crate::ir_check_elim::analyze(&g, &s);
    assert!(!e.bounds_elided(st));
}

/// JIT round 20 (lane loopopt): the same loop with `p < limit` stated by
/// a trap-form `Guard` before the store (`nextPutIndex`'s throw test, an
/// ASCII-profiled `dst.hasRemaining()` pruned to a guard), the shape the
/// host build of `encodeBufferLoop` has. Returns `(graph, store)`.
fn guarded_buffer_loop() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let hb = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let offset = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let limit = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let p0 = g.add(Op::Param(3), IrType::Int, vec![start], None);
    let n = g.add(Op::Param(4), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
    let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
    let j = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let p = g.add(Op::Phi, IrType::Int, vec![region, p0], None);
    let mem = g.add(Op::Phi, IrType::Memory, vec![region, mem0], None);
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![j, n], None);
    let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![p, limit], None);
    let _guard = g.add(
        Op::Guard { bci: 12 },
        IrType::Void,
        vec![stay, lt],
        Some(12),
    );
    let ix = g.add(Op::Add, IrType::Int, vec![p, offset], None);
    let st = g.add(
        Op::ArrayStore(MemKind::Byte),
        IrType::Void,
        vec![stay, mem, hb, ix, seven],
        Some(20),
    );
    let p1 = g.add(Op::Add, IrType::Int, vec![p, one], None);
    let j1 = g.add(Op::Add, IrType::Int, vec![j, one], None);
    g.nodes[region as usize].inputs.push(stay);
    g.nodes[j as usize].inputs.push(j1);
    g.nodes[p as usize].inputs.push(p1);
    g.nodes[mem as usize].inputs.push(st);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(30));
    g.push_safepoint(SafepointSnapshot::new(
        5,
        vec![hb, offset, limit, p, j, n],
        vec![],
        vec![],
    ));
    (g, st)
}


/// JIT round 20 (lane loopopt, review): a stride-3 index from a constant
/// start is NOT proven by the variable-offset arm. `x <= hb.length - o`
/// holds at the access, not at the `p < x` edge, so on iterations that
/// skip the access `x` may be `i32::MAX` and `p + 3` wraps; the unit-stride
/// shape is asked for stride 1 only here.
#[test]
fn jr20_a_stride_three_index_keeps_its_var_offset_check() {
    let (mut g, region, _, [_, _, _, p0], st) = buffer_loop();
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let three = g.add(Op::Const(3), IrType::Int, vec![], None);
    let p = g
        .nodes
        .iter()
        .position(|n| {
            n.op == Op::Phi && n.inputs.first() == Some(&region) && n.inputs.get(1) == Some(&p0)
        })
        .expect("p") as NodeId;
    g.nodes[p as usize].inputs[1] = zero;
    let back = g.nodes[p as usize].inputs[2];
    g.nodes[back as usize].inputs[1] = three;
    let clean = crate::deopt::DespecRegistry::new();
    assert!(plant_offset_predicates(&mut g, &clean, "T.m:()V") >= 1);
    let s = crate::ir_schedule::schedule(&g);
    let e = crate::ir_check_elim::analyze(&g, &s);
    assert!(!e.bounds_range_proved(st));
}

/// A guard AFTER the access in its block states nothing about it.
#[test]
fn jr20_a_guard_after_the_store_proves_nothing() {
    let (mut g, st) = guarded_buffer_loop();
    let guard = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Guard { bci: 12 }))
        .expect("the guard") as NodeId;
    // Rebuild the guard after the store (a higher id, so a later slot of
    // the block), and drop the original.
    let (ctrl, cond) = (
        g.nodes[guard as usize].inputs[0],
        g.nodes[guard as usize].inputs[1],
    );
    // The plan's screen refuses it outright.
    let (mut late, _) = guarded_buffer_loop();
    late.kill(guard);
    let _late = late.add(
        Op::Guard { bci: 24 },
        IrType::Void,
        vec![ctrl, cond],
        Some(24),
    );
    let clean = crate::deopt::DespecRegistry::new();
    assert_eq!(plant_offset_predicates(&mut late, &clean, "T.m:()V"), 0);
    // And the analysis, given the pre-header guards, still keeps the check.
    assert_eq!(plant_offset_predicates(&mut g, &clean, "T.m:()V"), 3);
    g.kill(guard);
    let _late = g.add(
        Op::Guard { bci: 24 },
        IrType::Void,
        vec![ctrl, cond],
        Some(24),
    );
    let s = crate::ir_schedule::schedule(&g);
    let e = crate::ir_check_elim::analyze(&g, &s);
    assert!(!e.bounds_elided(st));
}

/// `buffer_loop` with the store's offset replaced by the constant `c`.
fn constant_offset_loop(c: i64) -> (Graph, NodeId, NodeId, [NodeId; 4], NodeId, NodeId) {
    let (mut g, region, entry, params, st) = buffer_loop();
    let k = g.add(Op::Const(c), IrType::Int, vec![], None);
    let ix = g.nodes[st as usize].inputs[3];
    let slot = g.nodes[ix as usize]
        .inputs
        .iter()
        .position(|&x| x == params[1])
        .expect("the offset operand");
    g.nodes[ix as usize].inputs[slot] = k;
    (g, region, entry, params, st, k)    }