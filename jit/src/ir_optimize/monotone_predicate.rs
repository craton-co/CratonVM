//! nio21 wave 3 (lane L5): loop-entry predicates for a MONOTONE, non-counted
//! loop index (`CRATONVM_JIT_IR_MONOTONE_PREDICATE`).
//!
//! # What
//!
//! `UTF_8$Decoder.decodeArrayLoop` (JDK 25) runs `while (sp < sl)` with `sp`
//! and `dp` advanced by 1..4 in several arms and starting from computed values
//! (`soff + position + n`). Neither is a counted induction variable, so
//! [`super::plant_range_check_predicates`] plants nothing and every `sa[sp]`
//! and `da[dp]` keeps its null and bounds checks, per byte. HotSpot predicates
//! the loop. This plants, in the pre-header, deopting to the LOOP ENTRY with
//! the loop-entry frame (the header snapshot with every header φ at its entry
//! value; [`super::install_range_predicates`]' nodes, frame and de-spec
//! discipline):
//!
//! * `x <= a.length` for each invariant array `a` the body indexes as `a[p]`
//!   directly on an edge where `p < x` holds (`sa[sp]` on the header's stay
//!   edge with `x = sl`; `da[dp]` past `if (dp >= dl) return ..` with
//!   `x = dl`), `p` a header φ whose every back value is the φ plus constants
//!   (through forward merges);
//! * `0 <= init` for such a φ's entry value, unless it is a non-negative
//!   constant or a length;
//! * `0 <= x` for each invariant `x` of an in-loop `x - p < k` test on such a
//!   φ (`dl - dp < 2`), unless `x` is the header test's own bound (which the
//!   header test already proves `>= 1` inside the loop).
//!
//! `ir_check_elim` then proves both halves: `p < x <= a.length` from the
//! guard and the dominating edge (`upper_bounds`), and `p >= 0` from its
//! monotone-induction rule (`monotone_non_negative_phis`), which reads the
//! `0 <= init` / `0 <= x` guards. The pre-header `ArrayLength` is a
//! dereference of `a`, so the null check goes too.
//!
//! # Why it is sound
//!
//! Every guard only DEOPTS: a failing one resumes the interpreter at the
//! loop's header with the state the loop is entered with; nothing has run
//! between the pre-header and that point, so the interpreter runs the loop
//! itself, from its first test, and raises whatever the bytecode raises where
//! it raises it. A guard that keeps failing is withdrawn by the de-spec
//! registry (the header bci), after which the method keeps its checks. The
//! proofs that REMOVE checks are `ir_check_elim`'s, and fail closed.
//!
//! # What it does not plant for
//!
//! A loop with no top test (every predicate pass's restriction), an access
//! not hanging directly (through single-input merges) off a `p < x` edge, a
//! base allocated in this compile or computed in the loop, a loop of a spliced
//! callee (no header snapshot of its own), and the optimizing OSR route's
//! compile (its door admits only exits it resumes exactly).

use super::{
    build_users, install_range_predicates, is_loop_invariant, loop_body_with, loop_entry_frame,
    loop_entry_memory, loop_headers, mask_predicate::header_stay_edge, RangePredicatePlan,
};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, SafepointSnapshot, NO_NODE};

// j27 wave 2 (lane TIER1PROP): `a[p - c]` accesses and their `c <= init`
// floors (`CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET`, opt-in).
mod neg_offset;

/// At most this many guards per compile.
const MAX_MONOTONE_GUARDS: usize = 8;

/// How many forward merges a back value may pass through (the same bound as
/// `ir_check_elim`'s rule, which re-checks the shape with dominance).
const MONOTONE_SHAPE_DEPTH: u32 = 4;

/// `CRATONVM_JIT_IR_MONOTONE_PREDICATE` (nio21 wave 3, lane L5) -- **default
/// ON**, `=0` is the kill switch. Read once per compile.
/// `CRATONVM_JIT_IR_BCE_MONOTONE=0` (the `ir_check_elim` arm that reads the
/// guards) makes them pointless but does not stop them.
pub(super) fn monotone_predicate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_MONOTONE_PREDICATE")
}

/// JIT round 23 wave 7b (lane L1, item 1 of L3's wave-7 verdict on
/// `r23-w2c-typecheck-loops-loads-stay-in-the-loop-FIXED-20261024.md`):
/// `CRATONVM_JIT_IR_MONOTONE_PREDICATE_OWN_LENGTH` -- **opt-in** (`=1`; neutral
/// on the n23-w7 A/B, nocast 2.10 ns/elem both arms). Read once per compile, no `static`.
///
/// `for (int i = 0; i < a.length; i++) .. a[i] ..` entered by OSR kept the
/// bounds check of `a[i]` in the loop (`cmp ecx,[rax+8]; jae` in
/// `W2cTypecheckLoops.nocastRow`'s OSR body). Two gaps, both closed here:
///
/// * The planner skipped an access whose bound is the array's OWN length
///   (`ir_check_elim` needs no length guard for it), and with no other access
///   the loop got no plan at all, so no `0 <= init` guard either. In a
///   method-entry compile `init` is the constant `0` and nothing is missing.
///   In an OSR body `init` is the opaque entry merge's φ
///   (`[pre; 0, OsrLocal(i)]`), which no rule proves `>= 0`, so the
///   unit-stride rule (constant `init` only) and the monotone rule (a guard
///   or an edge fact on `init`) both declined. Under this switch such an
///   access contributes its φ, whose entry value gets the `0 <= init` guard
///   alone.
/// * The planter did not run on the optimizing OSR route. Under this switch
///   it runs there for a loop every entry reaches through its pre-header
///   (`loop_entered_only_through_its_pre_header`, the null-base predicate's
///   OSR rule), so the guard runs on the OSR entry too, before the header.
///   It needs the same OSR admission as the other arm predicates
///   (`osr_route_arm_predicates_enabled`).
///
/// Why the n23-w7 A/B saw nothing (JIT r23 wave 8, lane W8-L7, from the n23-w6
/// `CRATONVM_DBG=licm` log and disassembly): `W2cTypecheckLoops`' timed loop is
/// not the row's OSR body but the lambda's method-entry compile with the row
/// spliced in (`lambda$main$2`, `full/ir`), where `i` starts at the constant
/// `0` and the unit-stride rule already drops `a[i]`'s check. The OSR body
/// runs only for the first calls. Kept opt-in; it serves a real OSR-resident
/// own-length loop (one whose method is entered once).
///
/// Sound as the module notes say: the guard only deopts, to the loop entry,
/// with the loop-entry frame. `ir_check_elim`'s monotone rule then proves
/// `i >= 0` (entry `>= 0` by the guard, each back value `i + 1` under the
/// header's `i < a.length`, so no wrap), and its `If`-edge fact proves
/// `i < a.length`.
pub(super) fn monotone_predicate_own_length_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_MONOTONE_PREDICATE_OWN_LENGTH")
}

/// One loop's guards, decided without touching the graph.
struct MonotonePlan {
    header_bci: usize,
    entry_pred: NodeId,
    entry_mem: NodeId,
    /// `(x, arrays)`: `Guard(x <= a.length)` for each `a`.
    bounds: Vec<(NodeId, Vec<NodeId>)>,
    /// `Guard(0 <= v)` for each `v`.
    non_negative: Vec<NodeId>,
    /// JIT r23 wave 7b: some `0 <= v` guard serves an own-length access
    /// (`a[p]` under `p < a.length`), so it is planted even with no length
    /// guard (`monotone_predicate_own_length_enabled`). j27 wave 2: or a
    /// `c <= v` floor serves an own-length `a[p - c]`.
    non_negative_alone: bool,
    /// j27 wave 2 (lane TIER1PROP): `Guard(c <= v)` for each `(v, c)`, the
    /// entry value of a φ indexed as `a[p - c]` (`neg_offset`).
    floors: Vec<(NodeId, i64)>,
    frame: SafepointSnapshot,
}

/// Plant the guards the module notes describe, unless `despec` withdrew a
/// speculation at the loop's header. Returns the number of guards planted.
pub(super) fn plant_monotone_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    osr_route: bool,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LICM");
    let mut planted = 0usize;
    // JIT r23 wave 7b: on the optimizing OSR route (the caller admits it only
    // under `monotone_predicate_own_length_enabled`), a loop only when every
    // entry passes its pre-header.
    let osr_route = osr_route || graph.optimize_osr_route;
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if planted >= MAX_MONOTONE_GUARDS {
            break;
        }
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) {
            continue;
        }
        if osr_route {
            let users = build_users(graph);
            let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
            if !super::loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body) {
                if dbg {
                    eprintln!(
                        "[ir-licm] monotone predicate: header {region} skipped -- an OSR entry \
                         may bypass its pre-header"
                    );
                }
                continue;
            }
        }
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
            crate::deopt::PredicateKind::Monotone,
        ) {
            if dbg {
                eprintln!(
                    "[ir-licm] monotone predicate: header {region} bci {header_bci} \
                     de-specialised, not planted"
                );
            }
            continue;
        }
        let Some(plan) = monotone_plan(graph, region, entry_pred, &back_ctrls) else {
            continue;
        };
        let n = install_monotone_plan(graph, plan, MAX_MONOTONE_GUARDS - planted);
        planted += n;
        if dbg && n > 0 {
            eprintln!(
                "[ir-licm] monotone predicate: header {region} bci {header_bci} planted {n} \
                 (total {planted})"
            );
        }
    }
    planted
}

/// Is `v`, a back value of the header φ `phi`, the φ itself, the φ plus
/// constants, or a φ of a (non-header) merge whose inputs all are? A shape
/// screen only: `ir_check_elim` re-checks it with dominance and the facts.
fn monotone_shape(graph: &Graph, phi: NodeId, region: NodeId, v: NodeId, depth: u32) -> bool {
    if v == phi {
        return true;
    }
    let Some(n) = graph.node_opt(v) else {
        return false;
    };
    if n.op == Op::Phi {
        if depth >= MONOTONE_SHAPE_DEPTH {
            return false;
        }
        let Some((&m, vals)) = n.inputs.split_first() else {
            return false;
        };
        if m == region
            || vals.is_empty()
            || !matches!(
                graph.node_opt(m).map(|x| &x.op),
                Some(Op::Merge | Op::Region)
            )
        {
            return false;
        }
        return vals
            .iter()
            .all(|&x| monotone_shape(graph, phi, region, x, depth + 1));
    }
    crate::ir_check_elim::constant_step(graph, v).is_some_and(|(root, _)| root == phi)
}

/// The `If` edge control `ctrl` hangs off, through single-input merges:
/// `(if, projection)`.
fn edge_of(graph: &Graph, ctrl: NodeId) -> Option<(NodeId, u16)> {
    let mut c = ctrl;
    for _ in 0..5 {
        let n = graph.node_opt(c)?;
        match n.op {
            Op::Proj(which) if which <= 1 => {
                let owner = *n.inputs.first()?;
                return (graph.node_opt(owner)?.op == Op::If).then_some((owner, which));
            }
            Op::Merge if n.inputs.len() == 1 => c = *n.inputs.first()?,
            _ => return None,
        }
    }
    None
}

/// Is `v` a non-negative `int` constant or an array length (owing no guard)?
fn non_negative_by_shape(graph: &Graph, v: NodeId) -> bool {
    graph.node_opt(v).is_some_and(|n| match n.op {
        Op::Const(c) => n.ty == IrType::Int && (0..=i64::from(i32::MAX)).contains(&c),
        Op::ArrayLength => true,
        _ => false,
    })
}

/// Decide the guards for the loop at `region`, or `None` when it is not a
/// candidate (see the module notes).
fn monotone_plan(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> Option<MonotonePlan> {
    let users = build_users(graph);
    let body = loop_body_with(graph, &users, region, entry_pred, back_ctrls);
    let region_node = graph.node_opt(region)?;
    let arity = region_node.inputs.len();
    let entry_slot = region_node.inputs.iter().position(|&p| p == entry_pred)? + 1;
    // A top-tested loop only, so a failing guard stands for a loop that runs.
    let stay = header_stay_edge(graph, region, &body)?;
    let (head_if, _) = edge_of(graph, stay)?;
    let head_cond = graph.node_opt(head_if)?.input_opt(1)?;

    // The header's monotone `int` φs.
    let mut phis: Vec<NodeId> = Vec::new();
    for &u in users.get(region as usize)? {
        let Some(n) = graph.node_opt(u) else {
            continue;
        };
        if n.op != Op::Phi
            || n.ty != IrType::Int
            || n.inputs.first() != Some(&region)
            || n.inputs.len() != arity + 1
            || phis.contains(&u)
        {
            continue;
        }
        let shaped = n
            .inputs
            .iter()
            .enumerate()
            .skip(1)
            .all(|(slot, &v)| slot == entry_slot || monotone_shape(graph, u, region, v, 0));
        if shaped {
            phis.push(u);
        }
    }
    if phis.is_empty() {
        return None;
    }
    let invariant = |x: NodeId| {
        x != NO_NODE && !body.contains(&x) && is_loop_invariant(graph, x, region, &body)
    };

    // nio21 wave 6 (lane L2): no guard over an array an exception-edge anchor
    // of the body indexes (its checks would be decided in the pre-header, and
    // a throw would deopt there instead of entering its handler).
    let anchors = crate::ir::exception_edge_anchors(graph);
    let anchor_arrays =
        crate::ir::exception_edge_anchor_arrays(graph, &anchors, &|id: NodeId| body.contains(&id));

    // JIT r23 wave 7b: an own-length access's φ gets its `0 <= init` guard
    // (`monotone_predicate_own_length_enabled`). In an OSR graph only for a
    // loop every entry reaches through the pre-header, where the guard runs.
    let pre_header_screen = || {
        !(graph.osr_compile || graph.optimize_osr_route)
            || super::loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body)
    };
    let own_length_on = monotone_predicate_own_length_enabled() && pre_header_screen();
    // j27 wave 2 (lane TIER1PROP, `CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET`):
    // `a[p - c]` accesses too, each owing `c <= init`; a lone floor (an
    // own-length access) under the own-length rule's OSR screen.
    let neg_on = crate::ir_check_elim::monotone_neg_offset_enabled();
    let neg_alone_on = neg_on && pre_header_screen();
    let mut floor_uses = neg_offset::FloorUses::default();
    let mut own_used: Vec<NodeId> = Vec::new();
    // `a[p]` hanging off a `p < x` edge: `(x, arrays)`, and the φs used.
    let mut bounds: Vec<(NodeId, Vec<NodeId>)> = Vec::new();
    let mut used: Vec<NodeId> = Vec::new();
    let mut access_mem: Option<NodeId> = None;
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    for &id in &ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if !matches!(n.op, Op::ArrayLoad(_) | Op::ArrayStore(_)) {
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
        // `a[p]`, or `a[p - c]` (`c >= 1`) under the j27 switch.
        let Some((p, c)) = neg_offset::indexed_phi(graph, idx, &phis, neg_on) else {
            continue;
        };
        let Some((iff, which)) = edge_of(graph, ctrl) else {
            continue;
        };
        if !body.contains(&iff) {
            continue;
        }
        let Some(cond) = graph.node_opt(iff).and_then(|i| i.input_opt(1)) else {
            continue;
        };
        let Some((v, x)) = crate::ir_check_elim::strictly_below(graph, cond, which == 0) else {
            continue;
        };
        if v != p || !invariant(x) {
            continue;
        }
        let Some(base_node) = graph.node_opt(base) else {
            continue;
        };
        if base_node.ty != IrType::Ref
            || matches!(
                base_node.op,
                Op::New { .. } | Op::NewArray { .. } | Op::MultiNewArray { .. } | Op::NewCp { .. }
            )
            || !invariant(base)
            || anchor_arrays.contains(&base)
        {
            continue;
        }
        // j27 wave 2: what `a[p - c]` owes its φ's entry value; a start no
        // guard can raise to `c` keeps the access's check.
        let floor = match c {
            0 => None,
            _ => match neg_offset::floor_of(graph, p, entry_slot, c, &|i: NodeId| body.contains(&i))
            {
                neg_offset::Floor::Never => continue,
                f => Some(f),
            },
        };
        // `p < a.length` itself: `ir_check_elim` needs no length guard for
        // it, only `p >= 0` (r23 wave 7b: its φ's `0 <= init`), or, for
        // `a[p - c]`, `p >= c` (j27 wave 2: its φ's `c <= init`).
        if graph
            .node_opt(x)
            .is_some_and(|xn| xn.op == Op::ArrayLength && xn.input_opt(2) == Some(base))
        {
            if let Some(f) = floor {
                if neg_alone_on {
                    floor_uses.note(p, f, c, true);
                    if access_mem.is_none() {
                        access_mem = Some(mem);
                    }
                }
                continue;
            }
            if own_length_on && !own_used.contains(&idx) {
                own_used.push(idx);
                if access_mem.is_none() {
                    access_mem = Some(mem);
                }
            }
            continue;
        }
        match bounds.iter_mut().find(|(bx, _)| *bx == x) {
            Some((_, arrays)) => {
                if !arrays.contains(&base) {
                    arrays.push(base);
                }
            }
            None => bounds.push((x, vec![base])),
        }
        match floor {
            Some(f) => floor_uses.note(p, f, c, false),
            None if !used.contains(&p) => used.push(p),
            None => {}
        }
        if access_mem.is_none() {
            access_mem = Some(mem);
        }
    }
    if bounds.is_empty() && own_used.is_empty() && !floor_uses.alone {
        return None;
    }

    // `0 <= init` of each φ used.
    let mut non_negative: Vec<NodeId> = Vec::new();
    for &p in &used {
        let init = graph.node_opt(p)?.input_opt(entry_slot)?;
        if init == NO_NODE || body.contains(&init) {
            return None;
        }
        if !non_negative_by_shape(graph, init) && !non_negative.contains(&init) {
            non_negative.push(init);
        }
    }
    // r23 wave 7b: and of each own-length access's φ. A φ whose entry value
    // is defined in the loop, or missing, is skipped (no guard), not the
    // whole plan. j27 wave 2: a lone `c <= init` floor counts too.
    let mut non_negative_alone = floor_uses.alone;
    for &p in &own_used {
        let Some(init) = graph.node_opt(p).and_then(|n| n.input_opt(entry_slot)) else {
            continue;
        };
        if init == NO_NODE || body.contains(&init) || !invariant(init) {
            continue;
        }
        if non_negative_by_shape(graph, init) {
            continue;
        }
        if !non_negative.contains(&init) {
            non_negative.push(init);
        }
        non_negative_alone = true;
    }
    if bounds.is_empty() && !non_negative_alone {
        return None;
    }
    // `0 <= x` of each in-loop `x - p < k` test on a φ used, unless `x` is
    // the header test's bound for that φ.
    let header_bound = |p: NodeId| -> Option<NodeId> {
        [true, false].into_iter().find_map(|holds| {
            crate::ir_check_elim::strictly_below(graph, head_cond, holds)
                .filter(|&(v, _)| v == p)
                .map(|(_, x)| x)
        })
    };
    for &id in &ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if n.op != Op::If {
            continue;
        }
        let Some(cond) = n.input_opt(1) else {
            continue;
        };
        for holds in [true, false] {
            let Some((v, x, _)) = crate::ir_check_elim::room_at_least(graph, cond, holds) else {
                continue;
            };
            if (!used.contains(&v) && !floor_uses.phis.contains(&v))
                || !invariant(x)
                || non_negative_by_shape(graph, x)
                || header_bound(v) == Some(x)
                || non_negative.contains(&x)
            {
                continue;
            }
            non_negative.push(x);
        }
    }

    let entry_mem = loop_entry_memory(graph, region, entry_pred)
        .or_else(|| access_mem.filter(|m| !body.contains(m)))?;
    graph.node_opt(entry_mem)?;
    let (header_bci, frame) = loop_entry_frame(graph, region, entry_slot, &body)?;
    // j27 wave 2: a `c <= v` floor (`c >= 1`) states `0 <= v` too.
    let floors = floor_uses.floors;
    non_negative.retain(|v| !floors.iter().any(|(f, _)| f == v));
    Some(MonotonePlan {
        header_bci,
        entry_pred,
        entry_mem,
        bounds,
        non_negative,
        non_negative_alone,
        floors,
        frame,
    })
}

/// Add one plan's guards, at most `budget` of them. The length guards first:
/// without one the non-negative guards buy nothing and are not planted.
fn install_monotone_plan(graph: &mut Graph, plan: MonotonePlan, budget: usize) -> usize {
    let MonotonePlan {
        header_bci,
        entry_pred,
        entry_mem,
        bounds,
        non_negative,
        non_negative_alone,
        floors,
        frame,
    } = plan;
    let mut added = 0usize;
    for (bound, arrays) in bounds {
        if added >= budget {
            break;
        }
        let arrays: Vec<NodeId> = arrays.into_iter().take(budget - added).collect();
        added += install_range_predicates(
            graph,
            RangePredicatePlan {
                header_bci,
                entry_pred,
                entry_mem,
                bound,
                cmp: CmpOp::Le,
                arrays,
                frame: frame.clone(),
            },
        );
    }
    // r23 wave 7b: an own-length access's `0 <= init` is worth planting alone.
    if (added == 0 && !non_negative_alone)
        || added >= budget
        || (non_negative.is_empty() && floors.is_empty())
    {
        return added;
    }
    // One frame copy for every `0 <= v` guard; it resumes at the header like
    // the length guards' (see `install_range_predicates` for why it must be
    // named, and taken back out when nothing names it).
    let pc = Some(header_bci);
    graph.push_safepoint(frame);
    let si = (graph.safepoints.len() - 1) as u32;
    let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
    let mut named = 0usize;
    let mut naming_failed = false;
    for v in non_negative {
        if added >= budget {
            break;
        }
        let cond = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![zero, v], pc);
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            pc,
        );
        if !graph.set_node_frame_snapshot(guard, si) {
            graph.kill(guard);
            graph.kill(cond);
            naming_failed = true;
            break;
        }
        named += 1;
        added += 1;
    }
    // j27 wave 2 (lane TIER1PROP): the `c <= v` floors, named by the same copy.
    if !naming_failed && added < budget {
        let n = neg_offset::install_floor_guards(
            graph,
            &floors,
            neg_offset::FloorSite {
                header_bci,
                entry_pred,
                snapshot: si,
            },
            budget - added,
        );
        named += n;
        added += n;
    }
    if named == 0 {
        let keep: Vec<bool> = (0..graph.safepoints.len())
            .map(|i| i != si as usize)
            .collect();
        let _ = graph.retain_safepoints(&keep);
    }
    added
}

#[cfg(test)]
mod tests {
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
            .unwrap_or_default()
    }

    /// The own-length access plans a lone `0 <= i0` guard (none with the
    /// switch off, none for a constant start), and with it planted
    /// `ir_check_elim` drops the bounds check of `a[i]`.
    #[test]
    fn r23_l1_an_own_length_loop_gets_its_init_guard_and_loses_the_bounds_check() {
        const SW: &str = "CRATONVM_JIT_IR_MONOTONE_PREDICATE_OWN_LENGTH";
        let on = |f: &dyn Fn() -> bool| {
            cratonvm_types::flags::with_thread_overrides(&[(SW, Some("1"))], || f())
        };
        // Switch off: no plan, as before.
        let (g, region, entry, _, _) = own_length_loop(None);
        let backs = backs_of(&g, region);
        let off = cratonvm_types::flags::with_thread_overrides(&[(SW, Some("0"))], || {
            monotone_plan(&g, region, entry, &backs).is_none()
        });
        assert!(off, "=0: an own-length loop plans nothing");
        // A constant start needs no guard.
        let (g, region, entry, _, _) = own_length_loop(Some(0));
        let backs = backs_of(&g, region);
        assert!(on(&|| monotone_plan(&g, region, entry, &backs).is_none()));
        // An unknown start: one lone guard, and the check goes.
        let (mut g, region, entry, i0, el) = own_length_loop(None);
        let backs = backs_of(&g, region);
        let before_elided = {
            let schedule = crate::ir_schedule::schedule(&g);
            crate::ir_check_elim::analyze(&g, &schedule).bounds_elided(el)
        };
        assert!(!before_elided, "without the guard nothing proves i >= 0");
        let planted = cratonvm_types::flags::with_thread_overrides(&[(SW, Some("1"))], || {
            let plan = monotone_plan(&g, region, entry, &backs).expect("an own-length plan");
            assert!(plan.bounds.is_empty());
            assert_eq!(plan.non_negative, vec![i0]);
            assert!(plan.non_negative_alone);
            install_monotone_plan(&mut g, plan, MAX_MONOTONE_GUARDS)
        });
        assert_eq!(planted, 1);
        let schedule = crate::ir_schedule::schedule(&g);
        assert!(
            crate::ir_check_elim::analyze(&g, &schedule).bounds_elided(el),
            "0 <= i0 and i < a.length: a[i] is in range"
        );
    }
}
