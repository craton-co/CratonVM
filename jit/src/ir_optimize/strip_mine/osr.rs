// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane STRIPOSR2; proposal
//! `docs/known-issues/jit/j27-stripmine-osr-and-array-bodies-proposal-20261028.md`
//! item 1, `CRATONVM_JIT_IR_STRIP_MINE_OSR`, opt-in): strip-mine a counted
//! loop whose header carries a bci -- the loop an OSR door enters (the post
//! loop of a main/post unroll, or the only loop of an OSR-door graph).
//!
//! # What
//!
//! ```text
//! R: i = phi(i0, i + S) ; if (i < lim) { BODY ; goto R }   X: ...      // R at bci b
//! ```
//!
//! becomes
//!
//! ```text
//! O: q = phi(i0, i)                                  // bci b: the OSR landing; polls
//!    d = (int) min(max((long) lim - (long) q, 0), span)   // span = min(N*S, 2^30)
//!    R: i = phi(q, i + S) ; if (i - q < d) { BODY ; goto R }   // no bci, no poll
//!    X: if (i < lim) goto O                          // the ORIGINAL compare node
//!    OX: ...
//! ```
//!
//! The header snapshot at `b` (the one the OSR stub and the back-edge poll's
//! mode exit read, `safepoint_first_by_bci`) is renamed to name `O`'s phis
//! where it named `R`'s. So the stub lands at `O` with the interpreter's
//! locals and computes `d` there; the outer poll's mode exit names the
//! header state with each `q` read from its edge input, `R`'s phi
//! (`back_edge_mode_exit_state`'s on-edge rule); `R` carries no bci, so there
//! is no second entry. Every body node and every body frame is unchanged.
//!
//! # Why a trip distance and not `i < slim`
//!
//! Such a loop has no post loop to finish a strip stopped early at a cap, so
//! the main-loop form (`lim2 = min(lim, MAX - S + 1)`) does not apply, and
//! without the cap `i < slim` keeps running a wrapped `i` (negative, below
//! `slim`) for about `2^32 / S` trips without a poll. The trip distance `i -
//! q` does not: it counts the strip's own iterations, wrap or not.
//!
//! # Why it is exact
//!
//! Let `i = q + kS` (mathematically) on the inner test of trip `k`.
//!
//! * `kS < 2^31`: trip `k` runs only after `(k-1)S < d <= 2^30`, so `kS <
//!   2^30 + 2^16`. The `int` difference `i - q` is therefore exactly `kS`,
//!   whether or not `i` itself wrapped.
//! * While `kS < lim - q`, `q + kS < lim <= MAX` has not wrapped and `i < lim`
//!   holds: the body runs when `kS < d = min(lim - q, span)`, exactly when
//!   the original test holds and the strip is not used up; a used-up strip
//!   reaches the outer test with `i < lim` and starts the next strip at `q' =
//!   i`.
//! * Once `kS >= lim - q` (so `kS >= d`), the inner test fails, and the outer
//!   test is the original compare on the same `i` (wrapped or not): it exits
//!   exactly when the original loop exits and otherwise re-enters `O` with
//!   that `i`, where the original would have run the body with it.
//! * `q >= lim` gives `d = 0`: zero trips, and the outer test exits. `q < lim`
//!   gives `d >= 1`: every strip runs a trip, the outer loop progresses.
//! * At most `ceil(span / S) <= N` inner trips between two outer polls.
//!
//! # Refused (the loop keeps its per-trip poll)
//!
//! The main loop's purity rules ([`super::scan_body`], over the nodes pinned
//! inside the loop: the exit's code is not the body); a `long` induction with
//! `CRATONVM_JIT_IR_STRIP_MINE_LONG` off (the switch is opt-in; with it
//! set, j27 wave 8 admits the loop through the saturating `long` trip
//! distance, `long_strip_distance`); an array read (not built for
//! this form); silent promoted stores (a body
//! with a deopt point: `Graph::promoted_sunk_stores`); an opaque OSR arm at
//! the header bci; a header snapshot that is not the header's own frame, or
//! is named by a node other than a promoted cell's exit write-back, or an
//! unclaimed second snapshot or a chain snapshot at its bci; a node outside
//! the loop that may deopt at the header bci with no frame of its own (it
//! would resume through the renamed snapshot, [`resumes_by_pc_at`]); any other
//! snapshot naming a loop value that a deopt or an entry can consult
//! ([`consulted_bcis`], `ea_ir_bridge::ir_consumable_snapshot_bcis`'s rule).
//!
//! # Promoted field cells (`counterInt`, `twoCells`)
//!
//! `loop_field_forward` keeps a cell in a header phi of `R`, deletes the
//! in-loop stores (no deopt point inside: none here, the body is pure), and
//! writes the cell back on the exit edge (`if (v != init) f = v`, a store
//! naming the header snapshot) and, through its records keyed by the header
//! NODE (`Graph::promoted_cell_writebacks`), at the header poll's mode exit.
//! Strip-mined: the records move to `O` ([`rekey_cell_writebacks`]), whose
//! poll is the loop's only one, and the lowering's `current` reads the cell
//! (`R`'s phi) through its outer twin (`strip_outer_twin`), so the pad writes
//! the value after the same iterations as the original poll did; the exit
//! stores keep an unrenamed copy of the header snapshot
//! ([`keep_exit_frame`]). No poll runs inside the inner loop, so no GC or
//! other thread can observe the cell there; the exit write-back runs before
//! any code after the loop, as before.

use super::{
    build_users, constant, header_phis, induction, loop_control, min_long, names_any_of, read_only_inside, scan_body,
    snapshot_namers, BodyScan, MainLoop, FxHashSet,
};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, ScalarOp, NO_NODE};

/// Largest strip span `N * S` this form takes: `kS < 2^30 + 2^16` keeps the
/// `int` trip distance exact.
const MAX_SPAN: i64 = 1 << 30;

/// Strip-mine every loop of `graph` whose header carries a bci and this
/// module's doc accepts. Returns how many were.
pub(super) fn strip_mine_bci_headers(graph: &mut Graph, iter: i64, dbg: bool) -> usize {
    let mut done = 0usize;
    for h in candidates(graph) {
        match recognise(graph, h) {
            Ok((m, header_sp)) => {
                rewrite(graph, &m, header_sp, iter);
                done += 1;
                if dbg {
                    eprintln!("[DBG_UNROLL] region {h}: strip-mined at its bci (OSR form, step {}, {iter} trip(s) per poll)", m.step);
                }
            }
            Err(why) => {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {h}: OSR-form strip mine refused -- {why}");
                }
            }
        }
    }
    done
}

/// Headers with a bci whose back edge is the true edge of a test on them
/// (cheap; [`loop_control`] checks the rest), neither a strip-mined inner
/// header nor the outer header of one.
fn candidates(graph: &Graph) -> Vec<NodeId> {
    let outers: FxHashSet<NodeId> = graph
        .strip_mined_inner_headers
        .iter()
        .filter_map(|&h| graph.node_opt(h).and_then(|n| n.input_opt(0)))
        .collect();
    (0..graph.nodes.len())
        // Cast: arena index to node id.
        .map(|i| i as NodeId)
        .filter(|&h| looks_like_bci_loop(graph, h))
        .filter(|h| !graph.strip_mined_inner_headers.contains(h) && !outers.contains(h))
        .collect()
}

fn looks_like_bci_loop(graph: &Graph, h: NodeId) -> bool {
    let Some(n) = graph.node_opt(h) else {
        return false;
    };
    if !matches!(n.op, Op::Merge | Op::Region) || n.bytecode_pc.is_none() || n.inputs.len() != 2 {
        return false;
    }
    let test = graph.node_opt(n.inputs[1]).filter(|b| b.op == Op::Proj(0)).and_then(|b| b.input_opt(0));
    test.and_then(|t| graph.node_opt(t))
        .is_some_and(|t| t.op == Op::If && t.input_opt(0) == Some(h))
}

/// The loop at `h` and its header snapshot's index, or why not.
fn recognise(graph: &Graph, h: NodeId) -> Result<(MainLoop, Option<usize>), &'static str> {
    let users = build_users(graph);
    let (entry, back, test, exit) = loop_control(graph, &users, h)?;
    let phis = header_phis(graph, &users, h)?;
    // `Int`, or `Long` under `CRATONVM_JIT_IR_STRIP_MINE_LONG` (j27 wave 8,
    // lane W8OSR: the saturating trip distance, [`long_strip_distance`]);
    // `induction` refuses a long one with that switch off.
    let (iv, step, lim) = induction(graph, test, &phis)?;
    let ty = graph.nodes[iv as usize].ty;
    let body = scan_body(graph, &users, h, back, &phis, Some(&[h, test, back][..]))?;
    if !body.accesses.is_empty() {
        return Err("an array read at a header with a bci");
    }
    if body.variant.contains(&lim) || phis.contains(&lim) {
        return Err("the bound varies in the loop");
    }
    let bci = graph.nodes[h as usize].bytecode_pc.ok_or("no bci")?;
    header_tables_clear(graph, h, bci)?;
    let header_sp = header_snapshot(graph, h, bci, &phis, &body.variant)?;
    if header_sp.is_some() && resumes_by_pc_at(graph, bci, &body.variant) {
        return Err("a node outside the loop resumes by pc at the header bci");
    }
    if consulted_frame_names_loop_value(graph, header_sp, &body) {
        return Err("a frame a deopt or an entry can consult names a loop value");
    }
    let m = MainLoop {
        header: h,
        entry,
        test,
        exit,
        phis,
        iv,
        step,
        lim,
        ty,
        accesses: Vec::new(),
    };
    Ok((m, header_sp))
}

/// The exit write-back stores the loop field promotion recorded for header
/// `h` (`Graph::promoted_cell_writebacks`).
fn cell_writebacks(graph: &Graph, h: NodeId) -> Vec<NodeId> {
    graph
        .promoted_cell_writebacks
        .iter()
        .filter(|&&(r, _)| r == h)
        .map(|&(_, s)| s)
        .collect()
}

/// The per-header tables this rewrite cannot carry over: SILENT promoted
/// stores (`Graph::promoted_sunk_stores`, kept in the body for its deopt pads,
/// whose plan `ir_lower::plan_promoted_pending` walks the header's own loop
/// -- a body with a store is refused by [`scan_body`] anyway, this names it),
/// and an opaque OSR arm. The promoted cells' write-back records follow the
/// outer header ([`rekey_cell_writebacks`]).
fn header_tables_clear(graph: &Graph, h: NodeId, bci: usize) -> Result<(), &'static str> {
    let records = cell_writebacks(graph, h);
    if graph.promoted_sunk_stores.iter().any(|(exit, _)| records.contains(exit)) {
        return Err("silent promoted stores in the loop (pending deopt pads)");
    }
    let opaque = graph
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::OsrEntryFlag { header_bci } if header_bci == bci));
    if opaque {
        return Err("an opaque OSR arm at the header bci");
    }
    Ok(())
}

/// The header snapshot: the FIRST non-chain snapshot at the header bci, the
/// one `ir_lower`'s `safepoint_first_by_bci` hands the OSR stub and the poll
/// (`None` when there is none). It must be the header's own frame (not a copy
/// or post-call frame), named by no node but the promoted cells' exit
/// write-back stores (which keep a copy of it, [`keep_exit_frame`]), and name
/// no loop value but `H`'s phis. Every other snapshot at the bci must be a
/// claimed copy (a pre-header read's, `loop_field_forward::preheader_snapshot`),
/// and a chain snapshot there (a spliced loop) refuses.
fn header_snapshot(
    graph: &Graph,
    h: NodeId,
    bci: usize,
    phis: &[NodeId],
    variant: &FxHashSet<NodeId>,
) -> Result<Option<usize>, &'static str> {
    let at: Vec<usize> = (0..graph.safepoints.len()).filter(|&si| graph.safepoints[si].bci == bci).collect();
    if at.iter().any(|&si| graph.safepoints[si].is_splice_state()) {
        return Err("a chain snapshot at the header bci");
    }
    let Some((&si, others)) = at.split_first() else {
        return Ok(None);
    };
    let namers = snapshot_namers(graph);
    if others.iter().any(|o| !namers.contains_key(o)) {
        return Err("a second unclaimed snapshot at the header bci");
    }
    let sp = &graph.safepoints[si];
    if sp.copy_frame || sp.post_call_only || sp.post_call_of != NO_NODE {
        return Err("the header snapshot is not the header's own frame");
    }
    let writebacks = cell_writebacks(graph, h);
    let claimers = namers.get(&si).map_or(&[][..], Vec::as_slice);
    if claimers.iter().any(|c| !writebacks.contains(c)) {
        return Err("a node other than a cell write-back names the header snapshot");
    }
    let body_value = sp
        .locals
        .iter()
        .chain(sp.stack.iter())
        .chain(sp.monitors.iter())
        .any(|v| variant.contains(v) && !phis.contains(v));
    if body_value {
        return Err("the header snapshot names a body value");
    }
    Ok(Some(si))
}

/// j27 wave 5 (lane REVIEW8): may a node outside the loop (`inner` left out)
/// transfer at `bci` through the first snapshot there, by pc rather than by a
/// frame of its own? The rewrite renames that snapshot to the outer phis,
/// which past the loop hold the last strip's start, not the exit state such a
/// node's frame needs (the cell write-backs name a frame of their own,
/// [`keep_exit_frame`]).
fn resumes_by_pc_at(graph: &Graph, bci: usize, inner: &FxHashSet<NodeId>) -> bool {
    graph.nodes.iter().enumerate().any(|(id, n)| {
        let at = n.bytecode_pc == Some(bci) || matches!(n.op, Op::Guard { bci: b } if b == bci);
        // Cast: arena index to node id.
        let outside = !inner.contains(&(id as NodeId));
        at && outside && n.frame_snapshot.is_none() && !crate::ir_lower::op_cannot_deopt(&n.op)
    })
}

/// Does a snapshot other than the header's name a loop value where something
/// may consult it? A per-copy frame only inner nodes name is never consulted
/// ([`read_only_inside`]); nor is a by-pc frame no outside node names whose
/// bci no outside deopt and no entry ([`consulted_bcis`]) resumes at.
fn consulted_frame_names_loop_value(graph: &Graph, header_sp: Option<usize>, body: &BodyScan) -> bool {
    let inner = &body.variant;
    let namers = snapshot_namers(graph);
    let consulted = consulted_bcis(graph, inner);
    graph.safepoints.iter().enumerate().any(|(si, sp)| {
        if Some(si) == header_sp || !names_any_of(sp, inner) {
            return false;
        }
        let ns = namers.get(&si).map(Vec::as_slice);
        if read_only_inside(sp, ns, inner) {
            return false;
        }
        let named_inside = ns.is_none_or(|ns| ns.iter().all(|n| inner.contains(n)));
        let plain = !sp.is_splice_state() && !sp.post_call_only && sp.post_call_of == NO_NODE;
        let unread = consulted.as_ref().is_some_and(|c| !c.contains(&sp.bci));
        !(named_inside && plain && unread)
    })
}

/// The bcis at which a deopt or an entry may consult a by-pc snapshot, the
/// `inner` nodes left out (none of them can deopt: [`scan_body`]): every
/// `Merge` / `Region` bci, every bci an outside node that may deopt carries,
/// and every `Guard`'s own bci. `None` (everything consultable) when such a
/// node carries no bci. `ea_ir_bridge::ir_consumable_snapshot_bcis`'s rule
/// without the splice mapping: a spliced deopting node resuming at a body bci
/// would sit in the one-block body, which [`scan_body`] refuses.
fn consulted_bcis(graph: &Graph, inner: &FxHashSet<NodeId>) -> Option<FxHashSet<usize>> {
    let mut out: FxHashSet<usize> = FxHashSet::default();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: arena index to node id.
        if n.op == Op::Dead || inner.contains(&(id as NodeId)) {
            continue;
        }
        if matches!(n.op, Op::Merge | Op::Region) {
            out.extend(n.bytecode_pc);
            continue;
        }
        if crate::ir_lower::op_cannot_deopt(&n.op) {
            continue;
        }
        out.insert(n.bytecode_pc?);
        if let Op::Guard { bci } = n.op {
            out.insert(bci);
        }
    }
    Some(out)
}

/// The strip distance `d` at the loop's width: [`int_strip_distance`] or
/// [`long_strip_distance`].
fn strip_distance(graph: &mut Graph, m: &MainLoop, q: NodeId, iter: i64) -> NodeId {
    if m.ty == IrType::Long {
        long_strip_distance(graph, m, q, iter)
    } else {
        int_strip_distance(graph, m, q, iter)
    }
}

/// j27 wave 8 (lane W8OSR): `d = min(max(lim, q), min(q, MAX - span) + span)
/// - q` at `long` width, `span = min(N*S, 2^30)`. `lim - q` itself can wrap a
/// `long`; this cannot. `hi = min(q, MAX - span) + span` does not overflow
/// and `hi >= q` (it is `q + span`, or `MAX` when `q > MAX - span`), so `t =
/// min(max(lim, q), hi)` lies in `[q, hi]` and `t - q` is exact, in `[0,
/// span]`. It equals `min(lim - q, span)` for `lim > q` (when `hi = MAX`,
/// `lim - q <= MAX - q < span`) and `0` for `lim <= q`: the `int` form's `d`,
/// so the module doc's argument holds with `long` in place of `int` (`kS <
/// 2^30 + 2^16` keeps the `long` trip distance `i - q` exact too).
fn long_strip_distance(graph: &mut Graph, m: &MainLoop, q: NodeId, iter: i64) -> NodeId {
    let long = IrType::Long;
    // `iter <= 2^20` and `step <= 2^16`: no overflow before the clamp.
    let span = (iter * m.step).min(MAX_SPAN);
    let q_cap = constant(graph, i64::MAX - span, long);
    let q_low = min_long(graph, q, q_cap);
    let span_c = constant(graph, span, long);
    let hi = graph.add(Op::Add, long, vec![q_low, span_c], None);
    let floor = graph.add(Op::ScalarIntrinsic(ScalarOp::MaxL), long, vec![m.lim, q], None);
    let top = min_long(graph, floor, hi);
    graph.add(Op::Sub, long, vec![top, q], None)
}

/// `d = (int) min(max((long) lim - (long) q, 0), span)`.
fn int_strip_distance(graph: &mut Graph, m: &MainLoop, q: NodeId, iter: i64) -> NodeId {
    let long = IrType::Long;
    let wide_lim = graph.add(Op::I2L, long, vec![m.lim], None);
    let wide_q = graph.add(Op::I2L, long, vec![q], None);
    let left = graph.add(Op::Sub, long, vec![wide_lim, wide_q], None);
    let zero = constant(graph, 0, long);
    let left = graph.add(Op::ScalarIntrinsic(ScalarOp::MaxL), long, vec![left, zero], None);
    // `iter <= 2^20` and `step <= 2^16`: no overflow before the clamp.
    let span = constant(graph, (iter * m.step).min(MAX_SPAN), long);
    let d = min_long(graph, left, span);
    graph.add(Op::L2I, IrType::Int, vec![d], None)
}

/// The edit (nothing below refuses): the outer header takes the bci and the
/// header snapshot, the inner test the trip distance, the outer test the
/// original compare.
fn rewrite(graph: &mut Graph, m: &MainLoop, header_sp: Option<usize>, iter: i64) {
    let original_cond = graph.nodes[m.test as usize].inputs[1];
    let outer = super::open_outer_loop(graph, m);
    move_bci(graph, m.header, outer.header);
    for (&p, &q) in m.phis.iter().zip(outer.phis.iter()) {
        move_bci(graph, p, q);
    }
    let d = strip_distance(graph, m, outer.iv, iter);
    // At the loop's width: `d` is a `long` for a `long` induction.
    let trips = graph.add(Op::Sub, m.ty, vec![m.iv, outer.iv], None);
    let inner_cond = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![trips, d], None);
    graph.set_input(m.test, 1, inner_cond);
    super::close_outer_loop(graph, m, &outer, original_cond);
    if let Some(si) = header_sp {
        keep_exit_frame(graph, si);
        rename_in_snapshot(graph, si, &m.phis, &outer.phis);
    }
    rekey_cell_writebacks(graph, m.header, outer.header);
}

/// The nodes naming snapshot `si` (the promoted cells' exit write-back stores,
/// [`header_snapshot`]) name an unrenamed copy of it instead: they run after
/// the loop, where `H`'s phis hold the exit state the snapshot describes now
/// (the outer phis would hold the last strip's start). The copy is claimed
/// and comes after `si`, which stays the first at its bci and unclaimed
/// (`ir_verify::check_frame_states`'s rule).
fn keep_exit_frame(graph: &mut Graph, si: usize) {
    // Cast: a snapshot index is a `u32` on a node.
    let claimers: Vec<NodeId> = (0..graph.nodes.len())
        .filter(|&id| graph.nodes[id].op != Op::Dead && graph.nodes[id].frame_snapshot == Some(si as u32))
        // Cast: arena index to node id.
        .map(|id| id as NodeId)
        .collect();
    if claimers.is_empty() {
        return;
    }
    let copy = graph.safepoints[si].clone();
    graph.push_safepoint(copy);
    // Cast: a snapshot index is a `u32` on a node.
    let ci = (graph.safepoints.len() - 1) as u32;
    for c in claimers {
        // Same bci (the copy's is `si`'s, which the node already agreed with).
        let moved = graph.set_node_frame_snapshot(c, ci);
        debug_assert!(moved, "n{c} keeps a frame at its bci");
    }
}

/// The promoted cells' write-back records of `inner` belong to `outer`, whose
/// back-edge poll now gives the header's mode exit
/// (`Lowerer::promoted_cell_poll_writebacks`, keyed by the header node; its
/// `current` reads an inner-header cell through [`super::strip_outer_twin`]).
fn rekey_cell_writebacks(graph: &mut Graph, inner: NodeId, outer: NodeId) {
    for rec in graph.promoted_cell_writebacks.iter_mut() {
        if rec.0 == inner {
            rec.0 = outer;
        }
    }
}

/// Move node `from`'s bci to `to`.
fn move_bci(graph: &mut Graph, from: NodeId, to: NodeId) {
    let pc = graph.node_opt_mut(from).and_then(|n| n.bytecode_pc.take());
    if let Some(n) = graph.node_opt_mut(to) {
        n.bytecode_pc = pc;
    }
}

/// In snapshot `si`, name `twins[k]` wherever it named `phis[k]`, through
/// `Graph::set_safepoint_slot` (a direct write would install a reference the
/// def-use lists never see).
fn rename_in_snapshot(graph: &mut Graph, si: usize, phis: &[NodeId], twins: &[NodeId]) {
    use crate::ir::SafepointSlotKind as K;
    let Some(sp) = graph.safepoints.get(si) else {
        return;
    };
    let mut edits: Vec<(K, usize, NodeId)> = Vec::new();
    for (kind, vals) in [(K::Local, &sp.locals), (K::Stack, &sp.stack), (K::Monitor, &sp.monitors)] {
        for (slot, v) in vals.iter().enumerate() {
            if let Some(k) = phis.iter().position(|p| p == v) {
                edits.push((kind, slot, twins[k]));
            }
        }
    }
    for (kind, slot, new) in edits {
        graph.set_safepoint_slot(si, kind, slot, new);
    }
}
