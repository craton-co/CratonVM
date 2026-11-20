// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

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
    true
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
    true
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
    true
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
    let dbg = false;
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
        if !early {
            fold_loop_mask_checks(graph, region, entry_pred, &back_ctrls);
        }
    }
    planted
}

/// After a loop-entry mask guard `(base, c)` on `entry_pred` is planted or
/// verified, fold redundant in-body `Cmp(Lt, [idx, len])` and `Cmp(Ge, [idx, 0])`
/// to `Const(1)`, drop trivial guards, and splice out dead in-loop `ArrayLength(base)`
/// nodes whose only remaining users are memory-token edges.
fn fold_loop_mask_checks(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) {
    let guards: Vec<(NodeId, i64, NodeId)> = {
        let mut by_base: Vec<(NodeId, i64, NodeId)> = Vec::new();
        for n in &graph.nodes {
            if let Some((ctrl, base, c, pre_al)) = mask_guard_shape_with_len(graph, n) {
                if ctrl == entry_pred {
                    if let Some(row) = by_base.iter_mut().find(|r| r.0 == base) {
                        if c > row.1 {
                            row.1 = c;
                            row.2 = pre_al;
                        }
                    } else {
                        by_base.push((base, c, pre_al));
                    }
                }
            }
        }
        by_base
    };
    if guards.is_empty() {
        return;
    }
    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    let guards: Vec<(NodeId, i64, NodeId)> = guards
        .into_iter()
        .filter(|&(base, _, _)| !body.contains(&base))
        .collect();
    if guards.is_empty() {
        return;
    }

    let array_len_base = |id: NodeId| -> Option<NodeId> {
        let n = graph.node_opt(id)?;
        (n.op == Op::ArrayLength).then(|| n.input_opt(2)).flatten()
    };
    let mut fold_cmps: Vec<NodeId> = Vec::new();
    let mut candidate_shifts: Vec<NodeId> = Vec::new();
    let mut body_ids: Vec<NodeId> = body.iter().copied().collect();
    body_ids.sort_unstable();
    for &id in &body_ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        match n.op {
            Op::Cmp(CmpOp::Lt) => {
                let (Some(idx), Some(len)) = (n.input_opt(0), n.input_opt(1)) else {
                    continue;
                };
                let Some(k) = crate::ir_check_elim::const_mask_of(graph, idx) else {
                    continue;
                };
                let (base, is_shift) = if let Some(b) = array_len_base(len) {
                    (b, false)
                } else if let Some(ln) = graph.node_opt(len) {
                    if !matches!(ln.op, Op::Shr | Op::UShr) {
                        continue;
                    }
                    let zero_shift = ln
                        .input_opt(1)
                        .and_then(|s| graph.node_opt(s))
                        .is_some_and(|sn| sn.ty == IrType::Int && sn.op == Op::Const(0));
                    if !zero_shift {
                        continue;
                    }
                    let Some(b) = ln.input_opt(0).and_then(array_len_base) else {
                        continue;
                    };
                    (b, true)
                } else {
                    continue;
                };
                if guards.iter().any(|&(gb, c, _)| gb == base && k <= c) {
                    fold_cmps.push(id);
                    if is_shift {
                        candidate_shifts.push(len);
                    }
                }
            }
            Op::Cmp(CmpOp::Ge) => {
                let (Some(idx), Some(zero)) = (n.input_opt(0), n.input_opt(1)) else {
                    continue;
                };
                if !graph
                    .node_opt(zero)
                    .is_some_and(|zn| zn.ty == IrType::Int && zn.op == Op::Const(0))
                {
                    continue;
                }
                let Some(k) = crate::ir_check_elim::const_mask_of(graph, idx) else {
                    continue;
                };
                if guards.iter().any(|&(_, c, _)| k <= c) {
                    fold_cmps.push(id);
                }
            }
            _ => {}
        }
    }

    if !fold_cmps.is_empty() {
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        for cid in fold_cmps {
            graph.replace_all_uses(cid, one);
            graph.kill(cid);
        }
        for sh in candidate_shifts {
            let in_sp = graph.safepoints.iter().any(|sp| {
                sp.locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                    .any(|&v| v == sh)
            });
            if !in_sp && graph.users_of(sh).is_empty() {
                graph.kill(sh);
            }
        }
        super::eliminate_trivial_guards(graph);
    }

    for &id in &body_ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if n.op != Op::ArrayLength {
            continue;
        }
        let (Some(in_mem), Some(base)) = (n.input_opt(1), n.input_opt(2)) else {
            continue;
        };
        let Some(&(_, _, pre_al)) = guards.iter().find(|&&(gb, _, _)| gb == base) else {
            continue;
        };
        let mut edits: Vec<(NodeId, usize)> = Vec::new();
        let mut mem_only = true;
        for u in graph.users_of(id) {
            let Some(un) = graph.node_opt(u) else {
                continue;
            };
            if un.op == Op::Dead {
                continue;
            }
            for (slot, &x) in un.inputs.as_slice().iter().enumerate() {
                if x == id {
                    if crate::ir::is_memory_token_slot(un, slot) {
                        edits.push((u, slot));
                    } else {
                        mem_only = false;
                        break;
                    }
                }
            }
            if !mem_only {
                break;
            }
        }
        if !mem_only {
            continue;
        }
        for (u, slot) in edits {
            graph.set_input(u, slot, in_mem);
        }
        graph.replace_all_uses(id, pre_al);
        graph.kill(id);
    }
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
    true
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
    let (ctrl, base, kv, _) = mask_guard_shape_with_len(graph, n)?;
    Some((ctrl, base, kv))
}

fn mask_guard_shape_with_len(graph: &Graph, n: &Node) -> Option<(NodeId, NodeId, i64, NodeId)> {
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
    Some((ctrl, base, kv, len))
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
mod tests;
