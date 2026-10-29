//! nio20 wave 6 (lane L4): loop-entry predicates for a constant-masked index
//! (`CRATONVM_JIT_IR_MASK_PREDICATE`).
//!
//! # What
//!
//! `TABLE[(i + k) & 7]`, `DIGITS[b & 0xf]`, `crcTable[(c ^ b) & 0xff]`: an
//! array access whose index is `p & m`, `m` a non-negative `int` constant,
//! inside a loop, over an array `a` that is the same on every iteration. The
//! index is in `[0, m]` whatever `p` is, so ONE test `m < a.length` -- which
//! also dereferences `a` -- proves every such access of the loop in range and
//! its base non-null. This plants that test in the pre-header, as an
//! `Op::Guard` that deopts to the LOOP ENTRY with the loop-entry frame (the
//! header snapshot with every header φ at its entry value): the same shape,
//! frame and de-spec discipline as [`super::plant_range_check_predicates`],
//! whose installer it reuses. `ir_check_elim` then reads the guard as the
//! strict fact `m < a.length` (its constant-mask arm) and the pre-header
//! `ArrayLength` as a dereference of `a`, and drops both checks of every
//! access it dominates.
//!
//! # Why it is sound
//!
//! A failing guard resumes the interpreter at the loop's header with the
//! state the loop is entered with; nothing has run between the pre-header and
//! that point, so the interpreter runs the loop itself, from its first test,
//! and raises whatever the bytecode raises where it raises it (or nothing,
//! when no access actually goes out of range). A null `a` deopts at the
//! pre-header `ArrayLength` the same way. So a failing guard costs a deopt,
//! never a wrong result; and a header whose guard keeps failing is withdrawn
//! by the de-spec registry (`OptimizeContext::despec`), after which the
//! method keeps its per-access checks.
//!
//! # What it does not plant for
//!
//! An access not anchored on the header test's stay edge (one that not every
//! iteration makes: a failing guard would then stand for nothing), a loop
//! with no top test, a base allocated in the loop or in this compile (`NewArray`: escape
//! analysis and the allocation rule own it, and `ir_check_elim` proves a
//! constant-length one directly), a base the loop computes, a loop of a
//! spliced callee (no own header snapshot), and, in the optimizing OSR route's
//! compile, a loop no opaque OSR arm enters through its pre-header (JIT r20
//! wave 19, LOOPOPT20-3, `CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES`: before it,
//! that whole compile).

use super::{
    anchored_at, build_users, install_range_predicates, is_loop_invariant, loop_body_with,
    loop_entry_frame, loop_entry_memory, loop_headers, proj_user, reaches_through_pass_throughs,
    FxHashSet, RangePredicatePlan, MAX_RANGE_PREDICATES,
};
use crate::ir::{CmpOp, Graph, IrType, Node, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_MASK_PREDICATE` (nio20 wave 6, lane L4) -- **default
/// ON**, `=0` is the kill switch. Read without a `static`, once per compile.
/// `CRATONVM_JIT_IR_BCE_CONST_MASK=0` (the `ir_check_elim` arm that reads the
/// guard) makes the predicates pointless but does not stop them.
pub(super) fn mask_predicate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_MASK_PREDICATE")
}

/// `CRATONVM_JIT_IR_MASK_PREDICATE_EARLY` (nio21 wave 5, lane L5) -- **default
/// ON since nio21 wave 7** (`=0` is the kill switch): plant the mask predicate
/// BEFORE the loop rounds (`ir_optimize::optimize_passes`) for the loops the
/// full unroller can then retire (see [`early_plant_verdict`]), so it sees
/// the guard `ir_lower::node_cannot_deopt`'s masked-load clause reads. Every
/// other loop keeps the late plant (`optimize_with`, after the loop rounds),
/// which skips an array an early guard already covers. Wave 5 planted EVERY
/// loop's predicate early and skipped the late plant, which cost the loop's
/// own LICM read hoist (`preheader_may_trap`) and an enclosing loop's LICM (a
/// guard in its body is a hard barrier); the candidate rule leaves both
/// untouched for any loop the unroller does not retire. Read once per compile.
pub(super) fn early_mask_predicate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_MASK_PREDICATE_EARLY")
}

/// nio21 wave 7 (lane L5): is the loop at `region` one the early plant is
/// FOR -- a loop the full unroller retires once the guard exists? All of:
///
/// * the two switches that make the guard useful to the unroller are on
///   (`CRATONVM_JIT_IR_MASK_GUARD_ELIDE` for `node_cannot_deopt`'s clause,
///   `CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD` for the screen's admission);
/// * one back edge, and a constant trip within the full-unroll cap
///   (`analyze_counted_loop` answers `Ok`);
/// * not nested in another loop (a guard in an outer body is a hard barrier to
///   that loop's LICM, which runs in the same rounds as the unroller);
/// * at least one `ArrayLoad` `a[x & m]` over an invariant array whose every
///   user is an `Op::KeepAlive` (the fenced load the unroller drops), and no
///   other node of the unroller's side-effect screen pinned in the body (a
///   store, call, allocation, static access, `ArrayLength`, guard, or any
///   other array access): such a loop would be refused anyway, and would then
///   keep a pre-header guard for nothing.
///
/// A loop that passes and is still not unrolled (the body budget, a frame the
/// pure-body escape cannot strand) keeps its early guard; it loses only its
/// own LICM read hoist, and it has nothing to hoist but the fenced loads.
fn early_plant_verdict(
    graph: &Graph,
    users: &[Vec<NodeId>],
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    all_loops: &[(NodeId, NodeId, Vec<NodeId>)],
) -> Result<(), String> {
    // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-2): a masked load whose
    // value feeds the loop is a candidate too, under
    // `CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD` (the unroller clones it).
    let drop = super::unroll_drop_fenced_load_enabled();
    let clone = super::unroll_clone_masked_load_enabled();
    if !mask_guard_elide_enabled() || !(drop || clone) {
        return Err("the elide / drop switches are off".into());
    }
    let [back] = back_ctrls else {
        return Err("not one back edge".into());
    };
    if super::analyze_counted_loop(graph, region, *back).is_err() {
        return Err("not a constant-trip counted loop".into());
    }
    let body = loop_body_with(graph, users, region, entry_pred, back_ctrls);
    let mut fenced = 0usize;
    for &id in &body {
        let Some(n) = graph.node_opt(id) else {
            return Err(format!("n{id} out of range"));
        };
        match n.op {
            Op::ArrayLoad(_) => {
                let (Some(base), Some(idx)) = (n.input_opt(2), n.input_opt(3)) else {
                    return Err(format!("n{id} ArrayLoad without base / index"));
                };
                // JIT round 20 (lane loopopt): the builder threads the memory
                // token through every load (`self.mem = load`), so the
                // header's memory φ (or the next read) names the load as its
                // TOKEN. That is an ordering edge, not a use of the value:
                // with no write in the loop (the screen below) it orders
                // nothing, and [`unthread_fenced_loads`] removes it once the
                // guard is planted. A value user other than a fence still
                // refuses.
                let mut fences = 0usize;
                // LOOPOPT20-2: value users (not fences, not memory tokens).
                let mut values = 0usize;
                let users_ok = users.get(id as usize).is_some_and(|us| {
                    us.iter().all(|&u| {
                        graph.node_opt(u).is_some_and(|un| {
                            if un.op == Op::KeepAlive {
                                fences += 1;
                                return un.inputs.len() == 2 && un.inputs[1] == id;
                            }
                            let token_only =
                                un.inputs.as_slice().iter().enumerate().all(|(slot, &x)| {
                                    x != id || crate::ir::is_memory_token_slot(un, slot)
                                });
                            if !token_only {
                                values += 1;
                            }
                            token_only || clone
                        })
                    })
                });
                // JIT round 20 wave 4: zero fences is a DEAD load (its fence
                // spliced away, or never written): kept only as a trap point,
                // which the guard this plant adds proves cannot fire, so the
                // unroller drops it like a fenced one.
                // LOOPOPT20-2: a load is either dropped (fences only, or dead:
                // the drop switch) or cloned (values only: the clone switch);
                // one with both kinds of user is neither.
                if values > 0 && fences > 0 {
                    return Err(format!("n{id} ArrayLoad has both a fence and a value user"));
                }
                if values == 0 && !drop {
                    return Err(format!("n{id} fenced ArrayLoad and the drop switch is off"));
                }
                if crate::ir_check_elim::const_mask_of(graph, idx).is_none() {
                    return Err(format!("n{id} ArrayLoad index is not a constant mask"));
                }
                if body.contains(&base) || !is_loop_invariant(graph, base, region, &body) {
                    return Err(format!(
                        "n{id} ArrayLoad base n{base} is not loop-invariant"
                    ));
                }
                if !users_ok {
                    return Err(format!(
                        "n{id} ArrayLoad has a value user other than a fence"
                    ));
                }
                fenced += 1;
            }
            Op::Store(_)
            | Op::Call { .. }
            | Op::IndyBridge { .. }
            | Op::MultiNewArray { .. }
            | Op::NewCp { .. }
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::StoreStatic { .. }
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::ArrayLength
            | Op::ArrayStore(_)
            | Op::Guard { .. } => {
                return Err(format!("a side effect in the body: n{id} {:?}", n.op));
            }
            _ => {}
        }
    }
    if fenced == 0 {
        return Err("no masked load".into());
    }
    // Not nested: no other loop's body holds this header -- unless
    // `CRATONVM_JIT_IR_MASK_PREDICATE_EARLY_NESTED` (JIT round 24).
    if !early_nested_enabled()
        && all_loops.iter().any(|(r, e, b)| {
            *r != region && loop_body_with(graph, users, *r, *e, b).contains(&region)
        })
    {
        return Err("nested in another loop".into());
    }
    Ok(())
}

/// `CRATONVM_JIT_IR_MASK_PREDICATE_EARLY_NESTED` (JIT round 24, the
/// LOOPOPT20-6 close; perf, **default ON** since the round-24 close, `=0`
/// kills: `Nio21MaskedFence nested` 0.53-0.61x, `Nio20FenceRow inline`
/// 0.37-0.44x and `shared` 0.72-0.83x, both modes; the `own` rows 1.04-1.17x
/// at 18-50 ns; facts equal HotSpot's): the early plant takes a
/// candidate loop NESTED in another loop too. `Nio20FenceRow own`'s fence
/// loop is spliced into its driver's loop, so the default rule refused it
/// ("nested in another loop"), its masked load kept its trap and the full
/// unroller bailed on it as a side effect: 12-46 ns per call against
/// HotSpot's 3. The cost the rule avoids is the enclosing loop's LICM, for
/// which the planted guard is a barrier; the switch measures that trade.
/// Read per candidate loop; no `static`.
pub(super) fn early_nested_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_MASK_PREDICATE_EARLY_NESTED")
}

/// [`early_plant_verdict`] as a yes / no, for the tests.
#[cfg(test)]
fn early_plant_candidate(
    graph: &Graph,
    users: &[Vec<NodeId>],
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
    all_loops: &[(NodeId, NodeId, Vec<NodeId>)],
) -> bool {
    early_plant_verdict(graph, users, region, entry_pred, back_ctrls, all_loops).is_ok()
}

/// Plant the predicates the module notes describe, unless `despec` withdrew
/// a speculation at the loop's header. Returns the number of guards planted.
/// `early` (nio21 wave 7): the pre-loop-rounds plant, for
/// [`early_plant_verdict`] loops only; the late plant takes every loop and
/// skips an array an existing mask guard on the loop's entry already covers.
pub(super) fn plant_mask_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    early: bool,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LICM");
    let mut planted = 0usize;
    let all_loops = loop_headers(graph);
    // The early plant's candidate test reads the graph as it was before this
    // pass planted anything (a plant adds only pre-header nodes, which change
    // no loop's body or nesting).
    let early_users = if early {
        build_users(graph)
    } else {
        Vec::new()
    };
    for (region, entry_pred, back_ctrls) in all_loops.clone() {
        if planted >= MAX_RANGE_PREDICATES {
            break;
        }
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) {
            continue;
        }
        // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-3): on the optimizing OSR
        // route (`ir_optimize::optimize_with` plants there only under
        // `CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES`), only a loop whose entry
        // is the opaque OSR merge: an entry at its header falls into that
        // merge, so it runs the guard planted there.
        if graph.optimize_osr_route && !super::entry_is_opaque_osr_arm(graph, entry_pred) {
            if dbg {
                eprintln!(
                    "[ir-licm] mask predicate: header {region} on the OSR route has no opaque \
                     arm in front of it, not planted"
                );
            }
            continue;
        }
        // The coarse per-bci question, asked before any node is built: a
        // speculation withdrawn at the header (this guard's `UncommonTrap`,
        // an iv predicate's, or the `ArrayLength`'s `NullCheck`, all of which
        // resume there) withdraws this one too.
        let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        if crate::deopt::predicate_withdrawn(
            despec,
            method_key,
            bci32,
            crate::deopt::PredicateKind::Mask,
        ) {
            if dbg {
                eprintln!(
                    "[ir-licm] mask predicate: header {region} bci {header_bci} \
                     de-specialised, not planted"
                );
            }
            continue;
        }
        if early {
            // JIT round 20 wave 5 (lane loopopt): name the refusal, so a host
            // trace says why a loop is not an early unroll candidate.
            if let Err(why) = early_plant_verdict(
                graph,
                &early_users,
                region,
                entry_pred,
                &back_ctrls,
                &all_loops,
            ) {
                if dbg {
                    eprintln!(
                        "[ir-licm] mask predicate: header {region} bci {header_bci} not an early \
                         candidate: {why}"
                    );
                }
                continue;
            }
        }
        let plans = mask_predicate_plans(graph, region, entry_pred, &back_ctrls);
        if dbg && early && !plans.is_empty() {
            eprintln!(
                "[ir-licm] mask predicate: header {region} bci {header_bci} early \
                 (unroll candidate)"
            );
        }
        let mut planted_here = 0usize;
        for plan in plans {
            if planted >= MAX_RANGE_PREDICATES {
                break;
            }
            let n = install_range_predicates(graph, plan);
            planted += n;
            planted_here += n;
            if dbg && n > 0 {
                eprintln!("[ir-licm] mask predicate: header {region} planted (total {planted})");
            }
        }
        // JIT round 20 (lane loopopt): the candidate's fenced loads leave the
        // memory chain, so the unroller sees no memory φ carrying one of them
        // (an `ArrayLoad` in its clone walk is an "unclonable body node").
        if early && planted_here > 0 {
            let n = unthread_fenced_loads(graph, region, entry_pred, &back_ctrls);
            if dbg && n > 0 {
                eprintln!(
                    "[ir-licm] mask predicate: header {region}: {n} fenced load(s) off the \
                     memory chain"
                );
            }
        }
    }
    planted
}

/// JIT round 20 (lane loopopt): take every `ArrayLoad` of the loop at
/// `region` -- an early-plant candidate ([`early_plant_verdict`]), so each one is a masked load of
/// an invariant array whose only VALUE users are `Op::KeepAlive`s, in a body
/// with no store, call, allocation, static access or other side effect --
/// off the memory chain: every node naming it as a memory TOKEN names its
/// incoming token instead. Returns how many loads moved.
///
/// Sound because the builder threads a read through the token only to order
/// it before a LATER WRITE that may alias it (WAR), and this loop writes
/// nothing; a write after the loop is ordered after the load by control (the
/// load stays pinned to its body block). Nothing reads the load's value but
/// the fences, so which write it could observe is moot as well. The load keeps
/// its own token, so where it reads from is unchanged.
fn unthread_fenced_loads(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> usize {
    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    let mut loads: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            graph
                .node_opt(id)
                .is_some_and(|n| matches!(n.op, Op::ArrayLoad(_)))
        })
        .collect();
    loads.sort_unstable();
    let mut moved = 0usize;
    for id in loads {
        let Some(mem) = graph.node_opt(id).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        let mut edits: Vec<(NodeId, usize)> = Vec::new();
        for u in graph.users_of(id) {
            let Some(un) = graph.node_opt(u) else {
                continue;
            };
            if un.op == Op::Dead {
                continue;
            }
            for (slot, &x) in un.inputs.as_slice().iter().enumerate() {
                if x == id && crate::ir::is_memory_token_slot(un, slot) {
                    edits.push((u, slot));
                }
            }
        }
        if edits.is_empty() {
            continue;
        }
        for (u, slot) in edits {
            graph.set_input(u, slot, mem);
        }
        moved += 1;
    }
    moved
}

/// The plans for one loop: one per distinct mask constant, naming every
/// invariant array a `x & m` access of the body reads or writes with that
/// mask as its LARGEST. Empty when the loop is not a candidate.
fn mask_predicate_plans(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> Vec<RangePredicatePlan> {
    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    let Some(entry_slot) = graph
        .node_opt(region)
        .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
        .map(|p| p + 1)
    else {
        return Vec::new();
    };
    // Only an access every iteration makes -- one anchored on the header
    // test's stay edge (through single-input merges), as the iv predicate
    // requires -- so a failing guard almost always stands for an access that
    // was going to run (an access under an in-loop `if (a != null)` would
    // otherwise deopt a null `a` at every entry until the de-spec registry
    // withdrew it).
    let Some(stay) = header_stay_edge(graph, region, &body) else {
        return Vec::new();
    };
    // nio21 wave 6 (lane L2): no predicate over an array an exception-edge
    // anchor of the body indexes -- its pre-header `a.length` and `m <
    // a.length` would decide that anchor's checks ahead of the loop, and a
    // throw would deopt there instead of entering its handler.
    let anchors = crate::ir::exception_edge_anchors(graph);
    let anchor_arrays =
        crate::ir::exception_edge_anchor_arrays(graph, &anchors, &|id: NodeId| body.contains(&id));
    // `(array, largest mask, memory the access reads)`.
    let mut found: Vec<(NodeId, i64, NodeId)> = Vec::new();
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if !matches!(n.op, Op::ArrayLoad(_) | Op::ArrayStore(_))
            || crate::ir::memory_token_slot(n).is_none()
        {
            continue;
        }
        let (Some(ctrl), Some(mem), Some(base), Some(idx)) = (
            n.input_opt(0),
            n.input_opt(1),
            n.input_opt(2),
            n.input_opt(3),
        ) else {
            continue;
        };
        if !reaches_through_pass_throughs(graph, ctrl, stay) {
            continue;
        }
        let Some(m) = crate::ir_check_elim::const_mask_of(graph, idx) else {
            continue;
        };
        let Some(base_node) = graph.node_opt(base) else {
            continue;
        };
        if base_node.ty != IrType::Ref
            || matches!(base_node.op, Op::NewArray { .. } | Op::MultiNewArray { .. })
            || body.contains(&base)
            || anchor_arrays.contains(&base)
            || !is_loop_invariant(graph, base, region, &body)
        {
            continue;
        }
        match found.iter_mut().find(|(b, _, _)| *b == base) {
            Some(row) => row.1 = row.1.max(m),
            None => found.push((base, m, mem)),
        }
    }
    // nio21 wave 7 (lane L5): an array a mask guard on this loop's entry
    // already covers (the early plant's, when the unroller then kept the loop)
    // gets no second guard from the late plant.
    if !found.is_empty() {
        let existing: Vec<(NodeId, i64)> = mask_guards(graph)
            .into_iter()
            .filter(|&(p, _, _)| p == entry_pred)
            .map(|(_, b, c)| (b, c))
            .collect();
        found.retain(|&(b, m, _)| !existing.iter().any(|&(eb, c)| eb == b && c >= m));
    }
    if found.is_empty() {
        return Vec::new();
    }
    let Some(entry_mem) = loop_entry_memory(graph, region, entry_pred)
        .or_else(|| found.iter().map(|&(_, _, m)| m).find(|m| !body.contains(m)))
    else {
        return Vec::new();
    };
    if graph.node_opt(entry_mem).is_none() {
        return Vec::new();
    }
    let Some((header_bci, frame)) = loop_entry_frame(graph, region, entry_slot, &body) else {
        return Vec::new();
    };
    let mut masks: Vec<i64> = found.iter().map(|&(_, m, _)| m).collect();
    masks.sort_unstable();
    masks.dedup();
    let mut plans = Vec::with_capacity(masks.len());
    for m in masks {
        let arrays: Vec<NodeId> = found
            .iter()
            .filter(|&&(_, mm, _)| mm == m)
            .map(|&(b, _, _)| b)
            .collect();
        let bound = graph.add(Op::Const(m), IrType::Int, vec![], None);
        plans.push(RangePredicatePlan {
            header_bci,
            entry_pred,
            entry_mem,
            bound,
            // `m < a.length`: the strict fact the constant-mask arm reads.
            cmp: CmpOp::Lt,
            arrays,
            frame: frame.clone(),
        });
    }
    plans
}

/// The projection of the header's one exit test that stays in the loop, or
/// `None` when the header is not `region -> If` with exactly one projection in
/// `body` (a bottom-tested loop, or a header with more than one test). The
/// header shape `range_predicate_plan` reads.
pub(super) fn header_stay_edge(
    graph: &Graph,
    region: NodeId,
    body: &FxHashSet<NodeId>,
) -> Option<NodeId> {
    let mut test: Option<NodeId> = None;
    for id in anchored_at(graph, region) {
        let n = graph.node_opt(id)?;
        if n.op == Op::Dead || !n.op.is_control() || n.inputs.first() != Some(&region) {
            continue;
        }
        if n.op != Op::If || test.is_some() {
            return None;
        }
        test = Some(id);
    }
    let test = test?;
    let stays = |p: NodeId| p != NO_NODE && body.contains(&p);
    let (p0, p1) = (proj_user(graph, test, 0), proj_user(graph, test, 1));
    match (stays(p0), stays(p1)) {
        (true, false) => Some(p0),
        (false, true) => Some(p1),
        _ => None,
    }
}

// ── nio21 wave 3 (lane L5): a masked access proven by graph shape alone ──
//
// The first step of reason 3 of
// `r20-masked-index-loop-keeps-its-load-and-checks-CLOSED-20261012.md`: the unroller
// cannot clone an `ArrayLoad` it must treat as trapping, and the only proof
// that a masked access cannot trap, `ir_check_elim`'s constant-mask arm, needs
// a schedule the unroller runs before. This is the same proof read off the
// GRAPH, so the unroller's `ir_lower::node_cannot_deopt` and the lowering's
// check elision can one day ask the same predicate (the store-clause
// pattern). Today only the lowering asks it, behind
// `CRATONVM_JIT_IR_MASK_GUARD_ELIDE` (default OFF); `node_cannot_deopt` is
// not wired, because with the predicates planted AFTER the loop rounds the
// unroller never sees the guard, and claiming "cannot deopt" before every
// `ArrayLoad` route is shown to consult this chokepoint would strip a frame a
// deopt still reads.
//
// # The proof
//
// `a[p & m]` (`m` a constant in `0..=i32::MAX`, `ir_check_elim::const_mask_of`)
// is in range and `a` is non-null when a live `Guard(c < ArrayLength(a))`,
// `c >= m` a constant, with the guard AND its `ArrayLength` anchored on a
// loop's single ENTRY control `P` (the shape `install_range_predicates`
// plants), and the access anchored on a control node of that loop's body. The
// guard is pinned to `P`'s block, which control passes on every way into the
// loop (`loop_headers` admits only a single-entry loop whose back edges the
// header dominates, and every body control node is reached through the
// header), so it ran, and passed, before any execution of the access: `a` was
// dereferenced (non-null) and `0 <= p & m <= m <= c < a.length`. `a` is one
// SSA value, defined before the pre-header, and an array's length never
// changes.

/// `CRATONVM_JIT_IR_MASK_GUARD_ELIDE` (nio21 wave 3, lane L5) -- **default
/// ON since nio21 wave 7** (`=0` is the kill switch): the lowering drops the
/// null and bounds checks of an access [`mask_guard_proves_access`] proves,
/// and `ir_lower::node_cannot_deopt` claims such a load. The proof is the one
/// `ir_check_elim`'s constant-mask arm (`CRATONVM_JIT_IR_BCE_CONST_MASK`,
/// default ON since nio20 w6) already acts on from the schedule; the OSR entry
/// that could skip the pre-header is refused or re-checks the guard
/// (`CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL`) for both. The argument is on
/// `r20-masked-index-loop-keeps-its-load-and-checks-CLOSED-20261012.md`, "nio21 wave
/// 7 L5". The lowering reads it once per compile (`MaskGuardProofs`).
pub(crate) fn mask_guard_elide_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_MASK_GUARD_ELIDE")
}

/// `(entry control, array, c)` of every live `Guard(c < ArrayLength(a))`,
/// `c` a non-negative `int` constant, whose `ArrayLength` is anchored on the
/// guard's own control.
fn mask_guards(graph: &Graph) -> Vec<(NodeId, NodeId, i64)> {
    graph
        .nodes
        .iter()
        .filter_map(|n| mask_guard_shape(graph, n))
        .collect()
}

/// `(entry control, array, c)` when `n` is a live `Guard(c < ArrayLength(a))`,
/// `c` a non-negative `int` constant, whose `ArrayLength` is anchored on the
/// guard's own control -- one row of [`mask_guards`].
fn mask_guard_shape(graph: &Graph, n: &Node) -> Option<(NodeId, NodeId, i64)> {
    if !matches!(n.op, Op::Guard { .. }) {
        return None;
    }
    let (ctrl, cond) = (n.input_opt(0)?, n.input_opt(1)?);
    let c = graph.node_opt(cond)?;
    if c.op != Op::Cmp(CmpOp::Lt) {
        return None;
    }
    let (k, len) = (c.input_opt(0)?, c.input_opt(1)?);
    let kn = graph.node_opt(k)?;
    let Op::Const(kv) = kn.op else {
        return None;
    };
    if kn.ty != IrType::Int || !(0..=i64::from(i32::MAX)).contains(&kv) {
        return None;
    }
    let ln = graph.node_opt(len)?;
    if ln.op != Op::ArrayLength || ln.input_opt(0) != Some(ctrl) {
        return None;
    }
    let base = ln.input_opt(2)?;
    Some((ctrl, base, kv))
}

/// JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-2): the mask guard anchored on
/// `entry_pred` -- a loop's single entry control -- that covers `access`
/// (`a[p & m]`: the guard's array is `a` and its constant is at least `m`), or
/// `None`. The guard a full unroll's copy of `access` names as its token.
pub(super) fn entry_mask_guard_for(
    graph: &Graph,
    entry_pred: NodeId,
    access: &Node,
) -> Option<NodeId> {
    let (base, idx) = (access.input_opt(2)?, access.input_opt(3)?);
    let m = crate::ir_check_elim::const_mask_of(graph, idx)?;
    graph.nodes.iter().enumerate().find_map(|(id, n)| {
        let (ctrl, b, c) = mask_guard_shape(graph, n)?;
        (ctrl == entry_pred && b == base && c >= m).then_some(id as NodeId)
    })
}

/// JIT r20 wave 19 (LOOPOPT20-2, `CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD`):
/// the masked-access proof for an access OUTSIDE any loop -- a full unroll's
/// copy: `access` (`a[p & m]`) names as its guard TOKEN a mask guard on `a`
/// with `c >= m`, and is anchored on that guard's own control. The token is an
/// input of `access`, so the scheduler emits the guard before it in that one
/// block (`ir_schedule`'s topological order, and the impure chain keeps it);
/// a guard deopts on failure and never falls through. So when `access` runs,
/// `a` was dereferenced (non-null) and `0 <= p & m <= m <= c < a.length`.
fn token_guard_proves(graph: &Graph, access: &Node, m: i64) -> bool {
    if !super::unroll_clone_masked_load_enabled() {
        return false;
    }
    let tok = crate::ir::guard_token_of(access);
    if tok == NO_NODE {
        return false;
    }
    let (Some(ctrl), Some(base), Some(g)) = (
        access.input_opt(0),
        access.input_opt(2),
        graph.node_opt(tok),
    ) else {
        return false;
    };
    matches!(
        mask_guard_shape(graph, g),
        Some((gc, gb, c)) if gc == ctrl && gb == base && c >= m
    )
}

/// nio21 wave 7 (lane L5, step 3 of
/// `r20-masked-index-loop-keeps-its-load-and-checks-CLOSED-20261012.md`): every
/// proof [`mask_guard_proves_access`] can give, computed ONCE for a graph that
/// no longer changes (the lowering's), so a per-access question costs a scan
/// of the few guarded loops instead of a node scan, a users map and a loop
/// forest. `rows` holds, per loop whose single entry control carries at least
/// one mask guard, that loop's body and the `(array, c)` of each such guard.
/// [`MaskGuardProofs::proves`] answers exactly what
/// [`mask_guard_proves_access`] answers on the same graph (the test
/// `nio21w7_the_cached_proofs_agree_with_the_per_access_proof` pins it).
#[derive(Default)]
pub(crate) struct MaskGuardProofs {
    rows: Vec<(FxHashSet<NodeId>, Vec<(NodeId, i64)>)>,
}

impl MaskGuardProofs {
    /// The proofs of `graph`. Cheap when the graph has no mask guard (one
    /// node scan); otherwise one users map and one loop forest.
    pub(crate) fn of(graph: &Graph) -> Self {
        let guards = mask_guards(graph);
        if guards.is_empty() {
            return Self::default();
        }
        let users = build_users(graph);
        let mut rows = Vec::new();
        for (region, entry_pred, back_ctrls) in loop_headers(graph) {
            let covered: Vec<(NodeId, i64)> = guards
                .iter()
                .filter(|&&(p, _, _)| p == entry_pred)
                .map(|&(_, b, c)| (b, c))
                .collect();
            if covered.is_empty() {
                continue;
            }
            let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
            rows.push((body, covered));
        }
        Self { rows }
    }

    /// Does one of the cached guards prove `access`? The same question as
    /// [`mask_guard_proves_access`]: some loop whose entry carries
    /// `Guard(c < a.length)`, `c >= m`, holds the access's control in its body
    /// and does not define `a`.
    pub(crate) fn proves(&self, graph: &Graph, access: &Node) -> bool {
        // LOOPOPT20-2: an access that names a guard token may be a full
        // unroll's copy, proven by that token alone ([`token_guard_proves`]),
        // whatever `rows` holds. Every other access keeps the old early out.
        let tokened = crate::ir::guard_token_of(access) != NO_NODE;
        if (self.rows.is_empty() && !tokened)
            || !matches!(access.op, Op::ArrayLoad(_) | Op::ArrayStore(_))
        {
            return false;
        }
        let (Some(ctrl), Some(base), Some(idx)) = (
            access.input_opt(0),
            access.input_opt(2),
            access.input_opt(3),
        ) else {
            return false;
        };
        let Some(m) = crate::ir_check_elim::const_mask_of(graph, idx) else {
            return false;
        };
        if tokened && matches!(access.op, Op::ArrayLoad(_)) && token_guard_proves(graph, access, m)
        {
            return true;
        }
        self.rows.iter().any(|(body, covered)| {
            covered.iter().any(|&(b, c)| b == base && c >= m)
                && body.contains(&ctrl)
                && !body.contains(&base)
        })
    }
}

/// Does a pre-header mask guard prove `access` (an `ArrayLoad` /
/// `ArrayStore`) in range with a non-null base? See the notes above. Graph
/// only: no schedule, so the optimizer and the lowering can both ask it. The
/// lowering asks the cached form ([`MaskGuardProofs`]) instead.
pub(crate) fn mask_guard_proves_access(graph: &Graph, access: &Node) -> bool {
    if !matches!(access.op, Op::ArrayLoad(_) | Op::ArrayStore(_)) {
        return false;
    }
    let (Some(ctrl), Some(base), Some(idx)) = (
        access.input_opt(0),
        access.input_opt(2),
        access.input_opt(3),
    ) else {
        return false;
    };
    let Some(m) = crate::ir_check_elim::const_mask_of(graph, idx) else {
        return false;
    };
    // LOOPOPT20-2: a full unroll's copy, proven by its guard token.
    if matches!(access.op, Op::ArrayLoad(_))
        && crate::ir::guard_token_of(access) != NO_NODE
        && token_guard_proves(graph, access, m)
    {
        return true;
    }
    let entries: Vec<NodeId> = mask_guards(graph)
        .into_iter()
        .filter(|&(_, b, c)| b == base && c >= m)
        .map(|(p, _, _)| p)
        .collect();
    if entries.is_empty() {
        return false;
    }
    let users = build_users(graph);
    loop_headers(graph)
        .into_iter()
        .any(|(region, entry_pred, back_ctrls)| {
            if !entries.contains(&entry_pred) {
                return false;
            }
            let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
            body.contains(&ctrl) && !body.contains(&base)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{MemKind, SafepointSnapshot};

    /// `for (k = 0; k < 16; k++) o = a[(i + k) & 7]` over the parameter `a`:
    /// returns `(graph, region, entry ctrl, array param, element read)`.
    fn masked_loop(mask: i64) -> (Graph, NodeId, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(3));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let k = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let next = g.add(Op::Add, IrType::Int, vec![k, one], None);
        g.nodes[k as usize].inputs.push(next);
        let sixteen = g.add(Op::Const(16), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, sixteen], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(5));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let sum = g.add(Op::Add, IrType::Int, vec![i, k], None);
        let m = g.add(Op::Const(mask), IrType::Int, vec![], None);
        let idx = g.add(Op::And, IrType::Int, vec![sum, m], None);
        let el = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![stay, mem0, a, idx],
            Some(9),
        );
        let _ka = g.add(Op::KeepAlive, IrType::Void, vec![stay, el], Some(12));
        g.nodes[region as usize].inputs.push(stay);
        let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
        g.exit = ret;
        // The header's own snapshot: locals `[a, i, k]`.
        g.push_safepoint(SafepointSnapshot::new(3, vec![a, i, k], vec![], vec![]));
        (g, region, entry, a, el)
    }

    #[test]
    fn a_constant_masked_access_over_an_invariant_array_gets_one_entry_guard() {
        let (mut g, region, entry, a, _) = masked_loop(7);
        // `loop_headers` classifies the back edge; ask it rather than guess.
        let headers = loop_headers(&g);
        let Some((_, e, backs)) = headers.iter().find(|(r, _, _)| *r == region).cloned() else {
            panic!("the hand-built loop must be recognised");
        };
        assert_eq!(e, entry);
        let plans = mask_predicate_plans(&mut g, region, entry, &backs);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.arrays, vec![a]);
        assert_eq!(plan.cmp, CmpOp::Lt);
        assert_eq!(g.nodes[plan.bound as usize].op, Op::Const(7));
        // The entry frame names `k` at its entry value.
        assert!(plan.frame.locals.contains(&a));
        assert!(!plan
            .frame
            .locals
            .iter()
            .any(|&v| g.nodes[v as usize].op == Op::Phi));
    }

    #[test]
    fn a_negative_mask_proves_nothing_and_plants_nothing() {
        let (mut g, region, entry, _, _) = masked_loop(-1);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        assert!(mask_predicate_plans(&mut g, region, entry, &backs).is_empty());
    }

    /// nio21 wave 3 (lane L5): the planted guard proves the masked access by
    /// graph shape alone, and only an access whose mask it covers.
    #[test]
    fn the_planted_guard_proves_the_masked_access_by_shape() {
        let (mut g, region, entry, _, el) = masked_loop(7);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        // No guard yet: nothing proves it.
        assert!(!mask_guard_proves_access(&g, &g.nodes[el as usize]));
        let plans = mask_predicate_plans(&mut g, region, entry, &backs);
        assert_eq!(plans.len(), 1);
        for plan in plans {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        assert!(mask_guard_proves_access(&g, &g.nodes[el as usize]));
        // Widen the access's mask past the guard's constant: no longer proven.
        let idx = g.nodes[el as usize].inputs[3];
        let m = g.nodes[idx as usize].inputs[1];
        g.nodes[m as usize].op = Op::Const(15);
        assert!(!mask_guard_proves_access(&g, &g.nodes[el as usize]));
    }

    /// nio21 wave 5 (lane L5): `node_cannot_deopt` claims the proven masked
    /// load exactly when the emission's elision switch is on, and never
    /// before the guard exists or for an access the guard does not cover.
    #[test]
    fn nio21w5_node_cannot_deopt_claims_the_proven_masked_load_under_the_switch() {
        let elide = |on: bool, g: &Graph, el: NodeId| {
            cratonvm_types::flags::with_thread_overrides(
                &[(
                    "CRATONVM_JIT_IR_MASK_GUARD_ELIDE",
                    if on { Some("1") } else { Some("0") },
                )],
                || crate::ir_lower::node_cannot_deopt(g, &g.nodes[el as usize]),
            )
        };
        let (mut g, region, entry, _, el) = masked_loop(7);
        assert!(!elide(true, &g, el), "no guard yet");
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        assert!(elide(true, &g, el));
        assert!(
            !elide(false, &g, el),
            "the switch off keeps the checks, so the claim"
        );
        let idx = g.nodes[el as usize].inputs[3];
        let m = g.nodes[idx as usize].inputs[1];
        g.nodes[m as usize].op = Op::Const(15);
        assert!(!elide(true, &g, el), "a mask past the guard's constant");
    }

    /// nio21 wave 7 (lane L5): the early plant is default ON (for the unroll
    /// candidates only), `=0` the kill switch; so are the two switches it
    /// serves.
    #[test]
    fn nio21w7_the_early_mask_predicate_is_default_on() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_MASK_PREDICATE_EARLY", v)],
                early_mask_predicate_enabled,
            )
        };
        assert!(read(None));
        assert!(read(Some("1")));
        assert!(!read(Some("0")));
        let elide = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_MASK_GUARD_ELIDE", v)],
                mask_guard_elide_enabled,
            )
        };
        assert!(elide(None));
        assert!(!elide(Some("0")));
        let drop = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD", v)],
                super::super::unroll_drop_fenced_load_enabled,
            )
        };
        assert!(drop(None));
        assert!(!drop(Some("0")));
    }

    /// nio21 wave 7 (lane L5, step 3): the lowering's cached proofs answer
    /// exactly what the per-access proof answers, before and after the plant
    /// and for a mask the guard does not cover.
    #[test]
    fn nio21w7_the_cached_proofs_agree_with_the_per_access_proof() {
        let (mut g, region, entry, _, el) = masked_loop(7);
        let agree = |g: &Graph, want: bool| {
            let n = &g.nodes[el as usize];
            assert_eq!(mask_guard_proves_access(g, n), want);
            assert_eq!(MaskGuardProofs::of(g).proves(g, n), want);
        };
        agree(&g, false);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        agree(&g, true);
        let idx = g.nodes[el as usize].inputs[3];
        let m = g.nodes[idx as usize].inputs[1];
        g.nodes[m as usize].op = Op::Const(15);
        agree(&g, false);
    }

    /// nio21 wave 7 (lane L5): a second plant (the late one, after an early
    /// guard the unroller did not consume) adds nothing for an array the
    /// existing guard already covers.
    #[test]
    fn nio21w7_the_late_plant_skips_an_array_an_early_guard_covers() {
        let (mut g, region, entry, _, _) = masked_loop(7);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        assert!(mask_predicate_plans(&mut g, region, entry, &backs).is_empty());
    }

    /// nio21 wave 7 (lane L5): the early plant takes the fence row's shape --
    /// a short constant-trip top-level loop whose only array access is a
    /// masked load feeding a `KeepAlive` -- and nothing with another side
    /// effect in the body.
    #[test]
    fn nio21w7_the_early_plant_takes_only_an_unroll_candidate() {
        let (mut g, region, entry, _, el) = masked_loop(7);
        // A trip under every full-unroll cap, whatever the wide switch says.
        let bound = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Const(16))
            .expect("the bound") as NodeId;
        g.nodes[bound as usize].op = Op::Const(4);
        let candidate = |g: &Graph| {
            let users = build_users(g);
            let loops = loop_headers(g);
            let backs = loops
                .iter()
                .find(|(r, _, _)| *r == region)
                .map(|(_, _, b)| b.clone())
                .expect("the loop is recognised");
            early_plant_candidate(g, &users, region, entry, &backs, &loops)
        };
        assert!(candidate(&g));
        assert!(
            !cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD", Some("0"))],
                || candidate(&g),
            ),
            "no drop, no reason to plant early"
        );
        // A guard pinned in the body: the unroller refuses the loop anyway.
        let stay = g.nodes[el as usize].inputs[0];
        let cond = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Cmp(CmpOp::Lt))
            .expect("the test") as NodeId;
        let guard = g.add(
            Op::Guard { bci: 9 },
            IrType::Void,
            vec![stay, cond],
            Some(9),
        );
        assert!(!candidate(&g));
        g.nodes[guard as usize].op = Op::Dead;
        assert!(candidate(&g));
        // A second user of the element: not a fenced load.
        let _use = g.add(Op::KeepAlive, IrType::Void, vec![stay, el], Some(12));
        assert!(candidate(&g), "two fences are still only fences");
        let (mem, a, idx) = {
            let e = &g.nodes[el as usize];
            (e.inputs[1], e.inputs[2], e.inputs[3])
        };
        let other = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![stay, mem, a, idx],
            Some(10),
        );
        // JIT round 20 wave 4: a DEAD load (no user) is a candidate too.
        assert!(
            candidate(&g),
            "a dead masked load is dropped like a fenced one"
        );
        let _use = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![other, other], None);
        let no_clone = |g: &Graph| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD", Some("0"))],
                || candidate(g),
            )
        };
        assert!(
            !no_clone(&g),
            "a load whose value something uses is not dropped"
        );
        // LOOPOPT20-2: under the clone switch (default ON) it is cloned.
        assert!(candidate(&g), "a load whose value the loop uses is cloned");
        // ...but not one that is both fenced and used.
        let _fence = g.add(Op::KeepAlive, IrType::Void, vec![stay, other], Some(12));
        assert!(
            !candidate(&g),
            "a fenced and used load is neither dropped nor cloned"
        );
    }

    /// JIT round 20 (lane loopopt): the builder threads the memory token
    /// through the load, so the header's memory φ names it. The early plant
    /// still takes the loop, and takes the load off the chain once its guard
    /// is planted (the φ then carries nothing).
    #[test]
    fn jr20_a_fenced_load_on_the_memory_chain_is_a_candidate_and_leaves_it() {
        let (mut g, region, entry, _, el) = masked_loop(7);
        let bound = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Const(16))
            .expect("the bound") as NodeId;
        g.nodes[bound as usize].op = Op::Const(4);
        let mem0 = g.nodes[el as usize].inputs[1];
        let mphi = g.add(Op::Phi, IrType::Memory, vec![region, mem0], None);
        g.nodes[mphi as usize].inputs.push(el);
        g.nodes[el as usize].inputs[1] = mphi;
        let candidate = |g: &Graph| {
            let users = build_users(g);
            let loops = loop_headers(g);
            let backs = loops
                .iter()
                .find(|(r, _, _)| *r == region)
                .map(|(_, _, b)| b.clone())
                .expect("the loop is recognised");
            early_plant_candidate(g, &users, region, entry, &backs, &loops)
        };
        assert!(candidate(&g), "a token user is not a value user");
        let despec = crate::deopt::DespecRegistry::new();
        assert!(plant_mask_predicates(&mut g, &despec, "T.m:()V", true) >= 1);
        assert_eq!(
            g.nodes[mphi as usize].inputs[2], mphi,
            "the φ carries nothing now"
        );
        assert_eq!(
            g.nodes[el as usize].inputs[1], mphi,
            "the load keeps its own token"
        );
        // A value user other than a fence still refuses.
        let (mut h, region2, entry2, _, el2) = masked_loop(7);
        let b2 = h
            .nodes
            .iter()
            .position(|n| n.op == Op::Const(16))
            .expect("the bound") as NodeId;
        h.nodes[b2 as usize].op = Op::Const(4);
        let _use = h.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![el2, el2], None);
        let users = build_users(&h);
        let loops = loop_headers(&h);
        let backs = loops
            .iter()
            .find(|(r, _, _)| *r == region2)
            .map(|(_, _, b)| b.clone())
            .expect("the loop is recognised");
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD", Some("0"))],
            || early_plant_candidate(&h, &users, region2, entry2, &backs, &loops),
        ));
    }

    /// nio21 wave 6 (lane L5, L5W5-2): once the guard proves it, the fence
    /// row's element load is one the full unroll may drop, together with its
    /// `KeepAlive` and its private index (`(i + k) & 7`), and nothing the
    /// clone set or the carried `k` needs.
    #[test]
    fn nio21w6_a_proven_fenced_load_drops_with_its_fence_and_index() {
        let (mut g, region, entry, _, el) = masked_loop(7);
        let stay = g.nodes[el as usize].inputs[0];
        let droppable = |g: &Graph| {
            let users = build_users(g);
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_MASK_GUARD_ELIDE", Some("1"))],
                || super::super::fenced_load_droppable(g, &users, el, &[region, stay]),
            )
        };
        assert!(!droppable(&g), "no guard yet");
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        assert!(droppable(&g));
        // The unroller's sets for this loop: `k` carried, the clone set the
        // test and the step, the variance everything `k` reaches.
        let k = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Phi && n.inputs.first() == Some(&region))
            .expect("k") as NodeId;
        let users = build_users(&g);
        let carried: FxHashSet<NodeId> = [k].into_iter().collect();
        let mut variant: FxHashSet<NodeId> = carried.clone();
        let mut work = vec![k];
        while let Some(c) = work.pop() {
            for &u in &users[c as usize] {
                if variant.insert(u) {
                    work.push(u);
                }
            }
        }
        let next = g.nodes[k as usize].inputs[2];
        let cond = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Cmp(CmpOp::Lt) && n.inputs.first() == Some(&k))
            .expect("the test") as NodeId;
        let to_clone: FxHashSet<NodeId> = [next, cond].into_iter().collect();
        let set =
            super::super::fenced_load_drop_set(&g, &users, &[el], &to_clone, &carried, &variant)
                .expect("the index is private");
        let idx = g.nodes[el as usize].inputs[3];
        let sum = g.nodes[idx as usize].inputs[0];
        assert!(set.contains(&el) && set.contains(&idx) && set.contains(&sum));
        assert!(set.iter().any(|&d| g.nodes[d as usize].op == Op::KeepAlive));
        assert!(!set.contains(&k) && !set.contains(&next) && !set.contains(&cond));
        assert_eq!(set.len(), 4);
        // An index the loop also uses elsewhere is not dropped.
        let other = g.add(Op::Add, IrType::Int, vec![sum, sum], None);
        let users = build_users(&g);
        let _ = other;
        assert!(super::super::fenced_load_drop_set(
            &g,
            &users,
            &[el],
            &to_clone,
            &carried,
            &variant
        )
        .is_none());
    }

    // ── JIT r20 wave 19, lane yLoopPred (LOOPOPT20-2 / -3 / -6) ──

    /// The three switches are default ON, `=0` the kill switch each.
    #[test]
    fn yloop_the_three_switches_are_default_on() {
        let read = |key: &str, v: Option<&str>, f: fn() -> bool| {
            cratonvm_types::flags::with_thread_overrides(&[(key, v)], f)
        };
        for (key, f) in [
            (
                "CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES",
                super::super::osr_route_arm_predicates_enabled as fn() -> bool,
            ),
            (
                "CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD",
                super::super::unroll_clone_masked_load_enabled as fn() -> bool,
            ),
            (
                "CRATONVM_JIT_IR_UNROLL_NAMED_FRAME_PCS",
                super::super::unroll_named_frame_pcs_enabled as fn() -> bool,
            ),
        ] {
            assert!(read(key, None, f), "{key} is default ON");
            assert!(!read(key, Some("0"), f), "{key}=0 is the kill switch");
        }
        // The OSR-route arm predicates also need the entry stubs' re-check.
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_OSR_REMAT_ARRAY_LOAD", Some("0"))],
            super::super::osr_route_arm_predicates_enabled,
        ));
    }

    /// [`masked_loop`] with the opaque OSR merge the OSR door's build puts in
    /// front of every covered header (`IrBuilder::build_opaque_osr_merge`):
    /// `If(OsrEntryFlag)` on the entry, its arm and entry side joined by a
    /// `Merge` at the header bci, which is the header's forward input.
    /// Returns `(graph, region, that merge, element read)`.
    fn masked_loop_behind_arm(mask: i64) -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let flag = g.add(
            Op::OsrEntryFlag { header_bci: 3 },
            IrType::Int,
            vec![entry],
            None,
        );
        let gate = g.add(Op::If, IrType::Control, vec![entry, flag], None);
        let from_entry = g.add(Op::Proj(1), IrType::Control, vec![gate], None);
        let arm = g.add(Op::Proj(0), IrType::Control, vec![gate], None);
        let pre = g.add(Op::Merge, IrType::Control, vec![from_entry, arm], Some(3));
        let region = g.add(Op::Merge, IrType::Control, vec![pre], Some(3));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let k = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let next = g.add(Op::Add, IrType::Int, vec![k, one], None);
        g.nodes[k as usize].inputs.push(next);
        let sixteen = g.add(Op::Const(16), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, sixteen], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(5));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let sum = g.add(Op::Add, IrType::Int, vec![i, k], None);
        let m = g.add(Op::Const(mask), IrType::Int, vec![], None);
        let idx = g.add(Op::And, IrType::Int, vec![sum, m], None);
        let el = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![stay, mem0, a, idx],
            Some(9),
        );
        let _ka = g.add(Op::KeepAlive, IrType::Void, vec![stay, el], Some(12));
        g.nodes[region as usize].inputs.push(stay);
        let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
        g.exit = ret;
        g.push_safepoint(SafepointSnapshot::new(3, vec![a, i, k], vec![], vec![]));
        (g, region, pre, el)
    }

    /// LOOPOPT20-3: on the optimizing OSR route the late plant takes a loop an
    /// opaque arm enters through its pre-header, and anchors the guard on that
    /// merge (which the arm falls into); a loop with no arm gets nothing there.
    #[test]
    fn yloop_loopopt20_3_the_osr_route_plants_only_behind_an_opaque_arm() {
        let despec = crate::deopt::DespecRegistry::new();
        let (mut g, region, pre, _) = masked_loop_behind_arm(7);
        assert!(super::super::entry_is_opaque_osr_arm(&g, pre));
        assert!(!super::super::entry_is_opaque_osr_arm(&g, region));
        g.optimize_osr_route = true;
        assert_eq!(plant_mask_predicates(&mut g, &despec, "T.m:()V", false), 1);
        assert!(
            mask_guards(&g).iter().any(|&(c, _, k)| c == pre && k == 7),
            "the guard is anchored on the merge the arm falls into"
        );
        let (mut h, _, _, _, _) = masked_loop(7);
        assert!(!super::super::entry_is_opaque_osr_arm(&h, h.entry));
        h.optimize_osr_route = true;
        assert_eq!(
            plant_mask_predicates(&mut h, &despec, "T.m:()V", false),
            0,
            "no arm: an OSR entry at the header would skip the guard"
        );
        let (mut h, _, entry, _, _) = masked_loop(7);
        assert_eq!(plant_mask_predicates(&mut h, &despec, "T.m:()V", false), 1);
        assert!(mask_guards(&h).iter().any(|&(c, _, _)| c == entry));
    }

    /// LOOPOPT20-2: a full unroll's copy -- anchored on the loop entry, the
    /// entry guard as its token -- is proven outside the loop, by both forms
    /// of the proof; not without the token, not on another control, not for a
    /// wider mask, and not under the kill switch.
    #[test]
    fn yloop_loopopt20_2_a_token_licensed_copy_is_proven_outside_the_loop() {
        let (mut g, region, entry, a, el) = masked_loop(7);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        let guard = entry_mask_guard_for(&g, entry, &g.nodes[el as usize])
            .expect("the entry guard covers the load");
        assert!(matches!(g.nodes[guard as usize].op, Op::Guard { .. }));
        let (mem, idx) = (
            g.nodes[el as usize].inputs[1],
            g.nodes[el as usize].inputs[3],
        );
        let copy = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![entry, mem, a, idx, guard],
            None,
        );
        let proven = |g: &Graph, id: NodeId, on: bool| {
            cratonvm_types::flags::with_thread_overrides(
                &[(
                    "CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD",
                    if on { None } else { Some("0") },
                )],
                || {
                    let n = &g.nodes[id as usize];
                    let one = mask_guard_proves_access(g, n);
                    assert_eq!(
                        one,
                        MaskGuardProofs::of(g).proves(g, n),
                        "the two forms agree"
                    );
                    one
                },
            )
        };
        assert!(proven(&g, copy, true));
        assert!(
            !proven(&g, copy, false),
            "the kill switch withdraws the token proof"
        );
        let bare = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![entry, mem, a, idx],
            None,
        );
        assert!(
            !proven(&g, bare, true),
            "no token, outside the loop: unproven"
        );
        let start = g.entry;
        let elsewhere = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![start, mem, a, idx, guard],
            None,
        );
        assert!(
            !proven(&g, elsewhere, true),
            "not on the guard's own control"
        );
        let sum = g.nodes[idx as usize].inputs[0];
        let fifteen = g.add(Op::Const(15), IrType::Int, vec![], None);
        let wide_idx = g.add(Op::And, IrType::Int, vec![sum, fifteen], None);
        let wider = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![entry, mem, a, wide_idx, guard],
            None,
        );
        assert!(!proven(&g, wider, true), "a mask past the guard's constant");
    }

    /// LOOPOPT20-6: the pure-body escape stops counting the pcs of the
    /// planted predicate (its guard and length name their own loop-entry
    /// frame) and of a pure node; a control node and an unnamed guard still
    /// count.
    #[test]
    fn yloop_loopopt20_6_a_named_predicate_and_a_pure_node_read_no_flat_frame() {
        let (mut g, region, entry, _, _) = masked_loop(7);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        let guard = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Guard { .. }))
            .expect("the planted guard") as NodeId;
        let cond = g.nodes[guard as usize].inputs[1];
        let len = g.nodes[cond as usize].inputs[1];
        assert_eq!(g.nodes[len as usize].op, Op::ArrayLength);
        let reads_none =
            |g: &Graph, id: NodeId| super::super::pc_reads_no_flat_frame(g, &g.nodes[id as usize]);
        assert!(
            reads_none(&g, guard),
            "the guard boxes its own loop-entry frame"
        );
        assert!(
            reads_none(&g, len),
            "the length's null check resolves its own frame"
        );
        assert!(reads_none(&g, cond), "a compare cannot deopt");
        assert!(
            !reads_none(&g, region),
            "a control node: an OSR entry stub seeds from the flat frame there"
        );
        let bare = g.add(
            Op::Guard { bci: 3 },
            IrType::Void,
            vec![entry, cond],
            Some(3),
        );
        assert!(
            !reads_none(&g, bare),
            "an unnamed guard boxes the flat frame"
        );
    }

    /// Build `code` (a static `(int[] t, int i) -> int`) and run the whole
    /// optimizing pipeline with a de-spec registry, under `overrides`.
    fn yloop_build_and_optimize(
        code: &[u8],
        code_len: usize,
        num_locals: usize,
        overrides: &[(&str, Option<&str>)],
    ) -> Graph {
        cratonvm_types::flags::with_thread_overrides(overrides, || {
            let mut builder = crate::ir::IrBuilder::new(2, num_locals);
            builder.set_param_types(&[IrType::Ref, IrType::Int]);
            let mut g = builder.build(code, code_len).expect("IR build");
            let despec = crate::deopt::DespecRegistry::new();
            super::super::optimize_with(
                &mut g,
                super::super::OptimizeContext {
                    despec: Some(&despec),
                    method_key: "R20YLoopPred.f:([II)I",
                    osr_route: false,
                },
            );
            g
        })
    }

    /// The live `ArrayLoad`s of `g`.
    fn yloop_array_loads(g: &Graph) -> Vec<NodeId> {
        g.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::ArrayLoad(_)))
            .map(|(id, _)| id as NodeId)
            .collect()
    }

    /// LOOPOPT20-2 (with -6's early-plant normalisation): `int h = 0; for (int
    /// k = 0; k < 16; k++) h = h * 31 + t[(i + k) & 15]; return h;` -- the
    /// `Nio21MaskedFence sum` kernel over a parameter -- is fully unrolled:
    /// no loop is left, every element read is a copy licensed by the entry
    /// guard as its token, and the body lowers. `=0` keeps the loop.
    #[test]
    fn yloop_loopopt20_2_a_carried_masked_load_loop_unrolls_and_lowers() {
        //  0: iconst_0   1: istore_2   2: iconst_0   3: istore_3
        //  4: iload_3    5: bipush 16  7: if_icmpge 30
        // 10: iload_2   11: bipush 31 13: imul      14: aload_0   15: iload_1
        // 16: iload_3   17: iadd      18: bipush 15 20: iand      21: iaload
        // 22: iadd      23: istore_2  24: iinc 3,1  27: goto 4
        // 30: iload_2   31: ireturn
        let code = [
            0x03, 0x3d, 0x03, 0x3e, 0x1d, 0x10, 0x10, 0xa2, 0x00, 0x17, 0x1c, 0x10, 0x1f, 0x68,
            0x2a, 0x1b, 0x1d, 0x60, 0x10, 0x0f, 0x7e, 0x2e, 0x60, 0x3d, 0x84, 0x03, 0x01, 0xa7,
            0xff, 0xe9, 0x1c, 0xac, 0, 0,
        ];
        let g = yloop_build_and_optimize(&code, 32, 4, &[]);
        assert!(loop_headers(&g).is_empty(), "the 16-trip loop is retired");
        let loads = yloop_array_loads(&g);
        assert!(
            !loads.is_empty(),
            "the copies are kept (their values feed `h`)"
        );
        for &l in &loads {
            let n = &g.nodes[l as usize];
            assert_eq!(n.inputs.len(), 5, "n{l}: a copy names its licensing guard");
            assert!(n.bytecode_pc.is_none(), "n{l}: a pure-body copy has no pc");
            let tok = crate::ir::guard_token_of(n);
            assert!(
                matches!(g.nodes[tok as usize].op, Op::Guard { .. }),
                "n{l}: the token is the entry guard"
            );
            assert!(
                MaskGuardProofs::of(&g).proves(&g, n),
                "n{l}: proven, so no check"
            );
        }
        let schedule = crate::ir_schedule::schedule(&g);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the body calls no helper and is never run.
        let helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        assert!(
            crate::ir_lower::lower(&g, &schedule, 2, 4, &helpers).is_some(),
            "the unrolled body lowers (no pc-less check, no stranded-frame transfer)"
        );
        let kept = yloop_build_and_optimize(
            &code,
            32,
            4,
            &[("CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD", Some("0"))],
        );
        assert!(!loop_headers(&kept).is_empty(), "=0: the loop stays rolled");
    }

    /// LOOPOPT20-6: `for (k < 16) { int o = t[(i + k) & 7]; h += k ^ i; }` --
    /// the fence row's `param` shape with the fence's referent dead -- is
    /// retired: the early guard is planted (the merge wrappers normalised
    /// first), the dead masked read is dropped, and the pure-body escape no
    /// longer counts the guard's and the compare's header pcs. `=0` keeps the
    /// loop.
    #[test]
    fn yloop_loopopt20_6_a_dead_masked_load_loop_unrolls_and_lowers() {
        //  0: iconst_0   1: istore_2   2: iconst_0   3: istore_3
        //  4: iload_3    5: bipush 16  7: if_icmpge 32
        // 10: aload_0   11: iload_1   12: iload_3   13: iadd      14: bipush 7
        // 16: iand      17: iaload    18: istore 4  20: iload_2   21: iload_3
        // 22: iload_1   23: ixor      24: iadd      25: istore_2  26: iinc 3,1
        // 29: goto 4    32: iload_2   33: ireturn
        let code = [
            0x03, 0x3d, 0x03, 0x3e, 0x1d, 0x10, 0x10, 0xa2, 0x00, 0x19, 0x2a, 0x1b, 0x1d, 0x60,
            0x10, 0x07, 0x7e, 0x2e, 0x36, 0x04, 0x1c, 0x1d, 0x1b, 0x82, 0x60, 0x3d, 0x84, 0x03,
            0x01, 0xa7, 0xff, 0xe7, 0x1c, 0xac, 0, 0,
        ];
        let g = yloop_build_and_optimize(&code, 34, 5, &[]);
        assert!(loop_headers(&g).is_empty(), "the 16-trip loop is retired");
        assert!(
            yloop_array_loads(&g).is_empty(),
            "the dead masked read is dropped"
        );
        let schedule = crate::ir_schedule::schedule(&g);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the body calls no helper and is never run.
        let helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        assert!(
            crate::ir_lower::lower(&g, &schedule, 2, 5, &helpers).is_some(),
            "the unrolled body lowers"
        );
        let kept = yloop_build_and_optimize(
            &code,
            34,
            5,
            &[("CRATONVM_JIT_IR_UNROLL_NAMED_FRAME_PCS", Some("0"))],
        );
        assert!(!loop_headers(&kept).is_empty(), "=0: the loop stays rolled");
    }
}
