#[allow(unused_imports)]
use super::*;

/// The body of [`eliminate_redundant_loads`]; `same_block_reads_only`
/// restricts it to [`eliminate_hoisted_duplicate_loads`]' rule.
pub(in crate::ir_optimize) fn redundant_loads_pass(graph: &mut Graph, same_block_reads_only: bool) -> bool {
    let dbg = false;
    // A memory op in a COMPACT layout carries no token, so it is not on the
    // chain and the walk cannot see it. One such `Store` between two loads of
    // the cell it writes would be a write this pass steps straight over. Today
    // no such graph reaches here — the compact forms come from the EA bridge,
    // and `apply_ea_to_ir` runs AFTER `optimize` — but "a pass that runs later
    // does not build them yet" is a fact about pass order, not a property of
    // this pass, and pass order is exactly the thing this file has been
    // rearranging. So the whole graph is refused instead, which costs nothing
    // in production and closes the argument.
    if graph.nodes.iter().any(|n| {
        n.op != Op::Dead
            && n.op.memory_shape().is_some()
            && crate::ir::memory_token_slot(n).is_none()
    }) {
        if dbg {
            eprintln!(
                "[DBG_LOAD_CSE] graph refused: it holds a memory op in a compact \
                 layout, which carries no token this pass can follow"
            );
        }
        return false;
    }
    // One bucket per address-key. Buckets hold every surviving read with that
    // key, newest last, because the newest is the one whose memory state is
    // closest to the candidate's and therefore the cheapest walk.
    let mut buckets: FxHashMap<u64, Vec<NodeId>> = FxHashMap::default();
    let mut removed = 0usize;
    for id in 0..graph.nodes.len() {
        let b = id as NodeId;
        let (token_slot, b_mem, b_ctrl, key) = {
            let node = &graph.nodes[id];
            if node.op == Op::Dead || !is_pure_memory_read(&node.op) {
                continue;
            }
            // A VOLATILE read is never merged, in either role: removing it
            // drops an acquire (and a re-read the program asked for — the spin
            // loop), and serving a later read from it is at best a JMM-legal
            // corner not worth an argument. Skipping it here keeps it out of
            // the buckets too, so it is never a `found` (round 9 wave 6).
            if node.is_volatile_access() {
                continue;
            }
            // Below the documented full arity the node is in a compact
            // hand-built / EA-bridge layout and carries no memory token at
            // all, so there is no heap state to compare and nothing this pass
            // can say about it.
            let Some(token_slot) = crate::ir::memory_token_slot(node) else {
                continue;
            };
            let (Some(mem), Some(ctrl)) = (node.input_opt(token_slot), node.input_opt(0)) else {
                continue;
            };
            (token_slot, mem, ctrl, load_cse_hash(node, token_slot))
        };
        let mut found = NO_NODE;
        if let Some(bucket) = buckets.get(&key) {
            for &a in bucket.iter().rev() {
                let (Some(an), Some(bn)) = (graph.node_opt(a), graph.node_opt(b)) else {
                    continue;
                };
                if !redundant_load_matches(an, bn, token_slot) {
                    continue;
                }
                // Same block, or -- `load_cse_dominance_enabled` -- `a`'s block
                // dominates `b`'s, so `a` has run on every path that reaches
                // `b`. That is the property the within-block rule stood in for
                // (it forbids merging reads on sibling paths, where neither
                // dominates), and it is what lets `if (n.left == null) ...;
                // f(n.left)` read the field once.
                let a_ctrl = an.input_opt(0);
                let cross_block = a_ctrl != Some(b_ctrl);
                if same_block_reads_only && cross_block {
                    continue;
                }
                if cross_block
                    && !(load_cse_dominance_enabled()
                        && a_ctrl.is_some_and(|ac| control_dominates(graph, ac, b_ctrl)))
                {
                    continue;
                }
                let Some(a_mem) = an.input_opt(token_slot) else {
                    continue;
                };
                // The cell `b` reads, as the alias model describes it. A
                // read's `reads` class IS its address, so this needs no second
                // spelling of the layout rules.
                let reads = graph.memory_effect(b).reads;
                // The restricted form walks the pure-read rule only, whatever
                // `CRATONVM_JIT_IR_LOAD_CSE_ALIAS` says: `cross_block = true`
                // is the argument that turns the alias-aware widening off.
                let pure_reads_only = cross_block || same_block_reads_only;
                if memory_chain_reaches(graph, b_mem, a_mem, b_ctrl, reads, pure_reads_only) {
                    found = a;
                    break;
                }
            }
        }
        if found == NO_NODE {
            buckets.entry(key).or_default().push(b);
            continue;
        }
        // Token users first: they want the chain, not the value. Splicing them
        // to `b`'s own incoming token keeps every ordering `b` provided except
        // `b`'s own read, which is about to stop existing.
        for user in graph.users_of(b) {
            let Some(node) = graph.node_opt(user) else {
                continue;
            };
            let slots: Vec<usize> = node
                .inputs
                .as_slice()
                .iter()
                .enumerate()
                .filter(|&(i, &x)| x == b && crate::ir::is_memory_token_slot(node, i))
                .map(|(i, _)| i)
                .collect();
            for slot in slots {
                graph.set_input(user, slot, b_mem);
            }
        }
        // Everything still naming `b` is naming its VALUE, and that value is
        // `found`'s. `replace_all_uses` carries the safepoint slots too, so a
        // deopt frame that named the removed read follows it.
        graph.replace_all_uses(b, found);
        graph.kill(b);
        removed += 1;
        if dbg {
            eprintln!("[DBG_LOAD_CSE] read {b} is redundant with {found}");
        }
    }
    if removed > 0 {
        IR_LOADS_CSE.fetch_add(removed as u64, std::sync::atomic::Ordering::Relaxed);
        IR_LOADS_CSE_HERE.with(|c| c.set(c.get() + removed as u64));
        if dbg {
            eprintln!("[DBG_LOAD_CSE] removed {removed} redundant read(s)");
        }
    }
    removed > 0
}

/// Reads this process removed as redundant, since it started.
///
/// Process-global and monotone, so a caller reads a DELTA around the compile it
/// cares about. The closing identity: every read counted here had a surviving
/// read with the same key, so `reads_after + this == reads_before`.
pub(in crate::ir_optimize) static IR_LOADS_CSE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Total redundant reads eliminated by [`eliminate_redundant_loads`].
pub fn ir_load_cse_census() -> u64 {
    IR_LOADS_CSE.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    /// The same count, for THIS thread only.
    ///
    /// Both exist and neither is redundant. The process-global one answers
    /// "what did this workload do", which is the question the `[c2-supersede]`
    /// line is asked and the only one a cross-thread compile queue can answer.
    /// It is also unreadable as a delta from a test: `cargo test` runs the
    /// harness in parallel, `with_thread_overrides` is thread-local, so a
    /// sibling test with the switch on inflates it by an amount that depends on
    /// scheduling. That is not hypothetical — it is how
    /// `the_census_counts_the_reads_this_pass_removed` first failed, reading 2
    /// for its own 1.
    ///
    /// A test asserting "the pass removed exactly one read" has to read a
    /// counter only its own thread can raise.
    pub(in crate::ir_optimize) static IR_LOADS_CSE_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Redundant reads eliminated **on this thread**, since it started.
pub fn ir_load_cse_census_here() -> u64 {
    IR_LOADS_CSE_HERE.with(|c| c.get())
}

/// `CRATONVM_JIT_IR_LOAD_CSE_ALIAS` — default ON since round 13 (the opt-in
/// triage, `docs/feature-designs/jit-ir-optimizations.md`:
/// the R11 + R12 battery reads 335/336 with it, identical to the default arm);
/// `0` is the kill switch. Widens the chain walk from "only reads may sit
/// between" to "nothing that may alias may sit between", which is a widening of
/// a SOUNDNESS argument and so gets its own switch on top of the pass's.
pub(in crate::ir_optimize) fn load_cse_alias_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// Does control node `a` dominate control node `b`?
///
/// Answered only for the easy shape and conservatively everywhere else: `b`
/// is reached from `a` through branch projections (`Proj` of an `If`, the `If`
/// itself) and single-predecessor joins. Every such step has exactly one
/// predecessor, so any path to `b` passes through `a`. A join of two or more
/// edges, a loop header, `Start`, or anything else answers `false`.
pub(crate) fn control_dominates(graph: &Graph, a: NodeId, b: NodeId) -> bool {
    let mut cur = b;
    for _ in 0..LOAD_CSE_CHAIN_BUDGET {
        if cur == a {
            return true;
        }
        let Some(n) = graph.node_opt(cur) else {
            return false;
        };
        match n.op {
            Op::Proj(_) | Op::If => match n.input_opt(0) {
                Some(p) => cur = p,
                None => return false,
            },
            // A join with ONE predecessor -- the builder's merge at a branch
            // target only one edge reaches, before cleanup folds it -- has the
            // same dominator as that edge.
            Op::Merge | Op::Region if n.inputs.len() == 1 => cur = n.inputs.as_slice()[0],
            _ => return false,
        }
    }
    false
}

/// `CRATONVM_JIT_IR_LOAD_CSE_DOMINANCE` — default ON under
/// `CRATONVM_JIT_IR_LOAD_CSE`. Lets [`eliminate_redundant_loads`] serve a
/// read from an earlier one in a DOMINATING block (see
/// [`control_dominates`]), with only pure reads between them.
pub(in crate::ir_optimize) fn load_cse_dominance_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOAD_CSE` — default ON since round 13 (measured: see the
/// opt-in triage record named at [`load_cse_alias_enabled`]); `0` disables.
pub(in crate::ir_optimize) fn load_cse_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

// ── SCEV-driven Loop-Invariant Code Motion (LICM) ────────────────────
//
// Hoists provably loop-invariant *pure* computations and *invariant loads*
// out of a loop into its pre-header.  Increment 2 of activate-ir-optimizer.
//
// RELATIONSHIP TO `scev` / `loop_analysis`:
//
//   `scev::CountedLoop` / `loop_analysis::analyze_counted_loop_at_with_handlers`
//   are *bytecode* analyses — they consume the raw `&[u8]` method body and
//   return bytecode-PC keyed facts (induction-variable locals, trip counts).
//   They are not threaded through `optimize(&mut Graph)` (which only sees
//   the post-build SSA graph, no bytecode), and their PC keys do not map
//   onto SSA `NodeId`s, so they cannot *drive* node-level hoisting directly.
//   This pass therefore makes its hoisting decisions *structurally on the
//   SSA graph* — the only sound basis available here — and uses the
//   SCEV/loop-analysis vocabulary (loop header = an `Op::Merge` or
//   `Op::Region` that `loop_headers` accepts, induction variable = the
//   loop-carried `Op::Phi` anchored at that header,
//   trip-count-bounded body) to reason about invariance.
//
//   (Round 9 wave 3: the uncalled bytecode "corroboration" hook
//   `licm_scev_corroborates`, and the legacy `scev::InductionVar` /
//   `loop_analysis::find_invariant_loads` API it was the only caller of,
//   were deleted. A future bytecode-threaded gate should be built on
//   `analyze_counted_loop_at_with_handlers` + `body_is_heap_stable`; the
//   handler-blind `analyze_counted_loop_at` is test-only since round 11
//   wave 4.)
//
// LOOP MODEL (Sea-of-Nodes):
//
//   A loop header is an `Op::Region` node (hand-built / EA-bridge loops) OR an
//   `Op::Merge` node with a back-edge — the shape the production bytecode→IR
//   builder emits for a javac loop. Its control inputs are an entry predecessor
//   and one (or more) back-edges; for a `Region` the order is fixed
//   `[ctrl_entry, ctrl_backedge, …]` (`ir.rs` `activate_loop_header`), but for a
//   `Merge` header the slot order is NOT fixed, so `loop_headers` classifies
//   entry vs back-edge structurally (a back-edge is control-reachable forward
//   from the header itself; the remaining input is the pre-header).
//   The *loop body* is the set of control nodes reachable from the header
//   along control edges without leaving through the entry predecessor, up to
//   and including the node(s) that close the back-edge.  A value is
//   *loop-variant* if it is a `Phi` anchored at this header (the loop-carried /
//   induction values) or transitively depends on one; everything else (params,
//   constants, and pure expressions over non-variant inputs) is
//   *loop-invariant*.
//
// SOUNDNESS MODEL (deliberately conservative — a hoist that changes
// observable behaviour is a miscompile):
//
//   * We hoist only:
//       (a) PURE nodes (`Op::is_pure()`), which carry no control / memory
//           edge.  In Sea-of-Nodes these already float, so "hoisting" is a
//           no-op on placement; the value of this pass is the *gated
//           invariant-load* case (b) plus making the invariance explicit so
//           GVN can dedup loop-entry vs loop-body copies.
//       (b) `Op::Load` whose base AND address operands are loop-invariant
//           AND that is provably not clobbered by any `Store` / `Call` /
//           barrier inside the loop body (a real reordering across the loop
//           back-edge).
//   * Loop-invariance of an operand is decided by `is_loop_invariant`, which
//     bails (returns false → conservative) on:
//       - any node inside the loop body,
//       - any `Phi` anchored at the loop Region,
//       - any node it cannot resolve (out-of-range id, `Dead`).
//   * A *hard* barrier — a `Call`, a `New`/`NewArray` (constructor side
//     effects), a guard, a monitor, or any other non-pure non-load non-store
//     node — may touch arbitrary memory and disqualifies ALL loads in the loop
//     (`loop_has_hard_barrier`, give up — conservative).
//   * An in-loop `Store` no longer disqualifies the whole loop. An alias oracle
//     resolves each load/store base to a points-to summary (`resolve_ref_points_
//     to`), following `Phi`s transparently (cross-merge). EVERY store must be
//     *tame* — its base resolves to a definite set of local `New`/`NewArray`
//     allocations and nothing pre-existing (`loop_store_clobber`); one opaque
//     store blocks all hoisting. A load then hoists past the tame stores when its
//     possible-allocation set is disjoint from what they write
//     (`load_safe_past_clobber`); its pre-existing (`Param`) component is always
//     safe — a fresh in-method allocation is never an already-existing object.
//     The store base is NOT widened to `Param` (two parameters can be the same
//     object, `foo(x, x)`). An unknown-provenance base keeps the load pinned. The
//     common read-only invariant-load loop (no store at all) fires unchanged.
//   * Hoisting never moves a node past an exception edge that should observe
//     the pre-loop value: pure nodes raise nothing, and a load is only
//     hoisted when the body contains no barrier (so no observable ordering
//     relative to a side effect exists to violate).
//   * Irreducible / multi-entry loops are excluded: we only treat a `Region`
//     or back-edge `Merge` with at least 2 control inputs (exactly one entry,
//     one or more back-edges) as a loop, and we require the entry predecessor
//     (classified structurally, not by slot) to be outside the body (the hoist
//     target / pre-header anchor).
//
// This pass is monotone in observable behaviour (it only re-anchors invariant
// pure nodes — already-floating — and rewrites a hoisted load's control /
// memory inputs to the pre-header) and is idempotent, so re-running it finds
// no new work.

/// Identify loop headers with their loop-entry predecessor and back-edge(s).
///
/// A header is an `Op::Region` (hand-built / EA-bridge loops) OR an `Op::Merge`
/// with a back-edge — the shape the production bytecode→IR builder emits for a
/// javac loop. Of a header's control inputs, the back-edge(s) are control-
/// reachable *forward* from the header itself (`forward_control_closure`), and
/// the remaining input is the pre-header / loop entry. Resolving entry vs
/// back-edge structurally — rather than assuming the entry sits at input slot 0
/// — is what lets LICM fire on `Merge` headers, whose slot order the builder
/// does not fix (mirrors the unroll pass's loop discovery).
///
/// Only reducible single-entry loops are returned: exactly one pre-header and
/// at least one back-edge, every back-edge source dominated by the header
/// (`back_edges_dominated_by`). Returns `(header, entry_pred, back_ctrls)`.
pub(crate) fn loop_headers(graph: &Graph) -> Vec<(NodeId, NodeId, Vec<NodeId>)> {
    if !graph
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::Region | Op::Merge) && n.inputs.len() >= 2)
    {
        return Vec::new();
    }
    let users = build_users(graph);
    // Only a merge some control edge RETREATS to can be a reducible loop
    // header (see [`retreating_edge_targets`]). Every other merge — every
    // if/else join, which is most of them — used to pay a full forward closure
    // and, when it sat inside a loop, a second full dominance walk, to be told
    // it is not a header: O(merges × nodes) per LICM visit.
    let candidates = retreating_edge_targets(graph, &users);
    let mut out = Vec::new();
    for id in 0..graph.nodes.len() as NodeId {
        if !matches!(graph.nodes[id as usize].op, Op::Region | Op::Merge) {
            continue;
        }
        if candidates.as_ref().is_some_and(|c| !c.contains(&id)) {
            continue;
        }
        let inputs: Vec<NodeId> = graph.nodes[id as usize]
            .inputs
            .iter()
            .copied()
            .filter(|&c| c != NO_NODE)
            .collect();
        if inputs.len() < 2 {
            continue;
        }
        let reach = forward_control_closure(graph, &users, id);
        let mut back_ctrls = Vec::new();
        let mut entry_preds = Vec::new();
        for &c in &inputs {
            if reach.contains(&c) {
                back_ctrls.push(c);
            } else {
                entry_preds.push(c);
            }
        }
        // Reducible single-entry loop: one pre-header, >=1 back-edge.
        //
        // Reachability alone does not prove "single entry": a latch reachable
        // from the header can ALSO be reachable from the method entry without
        // passing the header — a second entry into the cycle (an irreducible
        // CFG, `r11w3-ircore-irreducible-cfgs-reached-natural-loop-passes`).
        // A pre-header hoist would then run on one of the two entries only.
        // So every back edge must also be dominated by the header. In a
        // reducible graph a latch the reachability test accepts always is
        // (a latch it is not would make the pre-header reachable from the
        // header too, so there would be no single entry), and this declines
        // nothing there; it is what let `IrBuilder::build` stop refusing an
        // irreducible method (round 11 wave 5). Without a usable `Op::Start` there is no
        // dominance answer (some hand-built graphs), and the reachability
        // answer stands, as in `loop_body`.
        if entry_preds.len() == 1 && !back_ctrls.is_empty() {
            if back_edges_dominated_by(graph, &users, id, &back_ctrls) {
                out.push((id, entry_preds[0], back_ctrls));
            }
            continue;
        }
        // Zero pre-headers is the NESTED INNER LOOP case, and it used to end
        // here as "safe, just not optimized". It is not a corner: the walk
        // forward from an inner header leaves through the inner exit, goes
        // round the OUTER back edge and arrives at the inner loop's own
        // pre-header, so every input reads as a back edge. Every `for (r…)
        // for (i…)` in the tree — which is every rep-counted probe and most
        // real scan loops — offered LICM only its outer header, never the loop
        // doing the work.
        //
        // Dominance answers what reachability cannot; see
        // `entry_preds_by_dominance`. Applied ONLY here, so a loop the test
        // above already classifies keeps exactly the answer it had and this
        // can only ADD loops the pass previously declined.
        if entry_preds.is_empty() {
            let (dom_entries, dom_backs) = entry_preds_by_dominance(graph, &users, id);
            if dom_entries.len() == 1 && !dom_backs.is_empty() {
                out.push((id, dom_entries[0], dom_backs));
            }
        }
    }
    out
}

/// The control nodes some edge RETREATS to in one depth-first walk of the
/// control graph from `graph.entry` — the targets of edges whose source is a
/// DFS descendant of their target.
///
/// In a reducible CFG every loop header is one: a back edge `n -> h` has `n`
/// dominated by `h`, so `n` is visited inside `h`'s DFS subtree while `h` is
/// still on the stack, whichever order the walk takes. [`loop_headers`] only
/// ever accepts a header that dominates every back-edge source (checked there
/// since round 11 wave 5, which is what let `IrBuilder::build` stop refusing
/// an irreducible CFG), and such a header is a retreating-edge target in ANY
/// depth-first walk, reducible or not (a dominator is an ancestor of every
/// node it dominates in the DFS tree), so filtering its candidates
/// through this set changes no answer; it only stops every if/else join from
/// paying for two whole-graph walks to learn it is not a loop.
///
/// `None` when the graph has no usable `Op::Start` at `graph.entry` (some
/// hand-built graphs): the caller then treats every merge as a candidate,
/// exactly as before.
pub(in crate::ir_optimize) fn retreating_edge_targets(graph: &Graph, users: &[Vec<NodeId>]) -> Option<FxHashSet<NodeId>> {
    let n = graph.nodes.len();
    if !matches!(graph.node_opt(graph.entry), Some(s) if s.op == Op::Start) || users.len() != n {
        return None;
    }
    let mut out: FxHashSet<NodeId> = FxHashSet::default();
    // 0 = unvisited, 1 = on the DFS stack, 2 = finished.
    let mut state = vec![0u8; n];
    state[graph.entry as usize] = 1;
    let mut stack: Vec<(NodeId, usize)> = vec![(graph.entry, 0)];
    while let Some(top) = stack.last_mut() {
        let cur = top.0;
        let list = &users[cur as usize];
        if top.1 < list.len() {
            let u = list[top.1];
            top.1 += 1;
            if (u as usize) >= n || !graph.nodes[u as usize].op.is_control() {
                continue;
            }
            match state[u as usize] {
                0 => {
                    state[u as usize] = 1;
                    stack.push((u, 0));
                }
                1 => {
                    out.insert(u);
                }
                _ => {}
            }
        } else {
            state[cur as usize] = 2;
            stack.pop();
        }
    }
    Some(out)
}

/// Control nodes `header` does **not** dominate: everything reachable from the
/// graph entry with `header` deleted.
///
/// One reachability walk, no dominator tree. It is the definition of dominance
/// read directly — `header` dominates `n` exactly when every path from the
/// entry to `n` goes through `header`, i.e. when deleting `header` makes `n`
/// unreachable — and it is what tells a NESTED INNER loop's back edge from its
/// pre-header, and its body from the enclosing method.
#[inline]
pub(in crate::ir_optimize) fn node_bit_contains(bits: &[u64], id: NodeId) -> bool {
    let idx = id as usize;
    bits.get(idx >> 6)
        .is_some_and(|&w| (w & (1u64 << (idx & 63))) != 0)
}

#[inline]
pub(in crate::ir_optimize) fn node_bit_insert(bits: &mut [u64], id: NodeId) -> bool {
    let idx = id as usize;
    let Some(w) = bits.get_mut(idx >> 6) else {
        return false;
    };
    let mask = 1u64 << (idx & 63);
    let new = (*w & mask) == 0;
    *w |= mask;
    new
}

pub(in crate::ir_optimize) fn control_not_dominated_by_bits(
    graph: &Graph,
    users: &[Vec<NodeId>],
    header: NodeId,
) -> Vec<u64> {
    let n = graph.nodes.len();
    let mut seen = vec![0u64; (n + 63) >> 6];
    if (graph.entry as usize) >= n {
        return seen;
    }
    node_bit_insert(&mut seen, graph.entry);
    let mut work = vec![graph.entry];
    while let Some(cur) = work.pop() {
        for &u in &users[cur as usize] {
            if u == header || node_bit_contains(&seen, u) || !graph.nodes[u as usize].op.is_control() {
                continue;
            }
            node_bit_insert(&mut seen, u);
            work.push(u);
        }
    }
    seen
}

#[allow(dead_code)]
pub(crate) fn control_not_dominated_by(
    graph: &Graph,
    users: &[Vec<NodeId>],
    header: NodeId,
) -> FxHashSet<NodeId> {
    let bits = control_not_dominated_by_bits(graph, users, header);
    let mut out = FxHashSet::default();
    for (wi, &w) in bits.iter().enumerate() {
        let mut word = w;
        while word != 0 {
            let b = word.trailing_zeros() as usize;
            out.insert(((wi << 6) | b) as NodeId);
            word &= word - 1;
        }
    }
    out
}

/// Does `header` dominate every one of `back_ctrls`? The check that turns a
/// reachability-classified back edge into a real one: a latch the method
/// entry reaches without passing `header` is a second entry into the cycle,
/// and a loop pass that trusts the classification would hoist to (or clone
/// from) one entry of a two-entry cycle.
///
/// `true` when the graph has no usable `Op::Start` at `graph.entry`: there is
/// no dominance answer, and the caller keeps its reachability answer, as it
/// always did.
pub(in crate::ir_optimize) fn back_edges_dominated_by(
    graph: &Graph,
    users: &[Vec<NodeId>],
    header: NodeId,
    back_ctrls: &[NodeId],
) -> bool {
    if !matches!(graph.node_opt(graph.entry), Some(s) if s.op == Op::Start)
        || users.len() != graph.nodes.len()
    {
        return true;
    }
    let not_dominated = control_not_dominated_by_bits(graph, users, header);
    back_ctrls.iter().all(|&c| !node_bit_contains(&not_dominated, c))
}

/// Which of `header`'s control inputs are loop ENTRIES, decided by dominance
/// rather than by reachability.
///
/// A control node is reachable from the graph entry *without passing through
/// `header`* exactly when `header` does not dominate it — which is the textbook
/// definition of "not a back edge". Reachability-from-the-header, which
/// [`loop_headers`] uses first, cannot answer this for a **nested inner loop**:
/// the walk forward from the inner header leaves through the inner loop's exit,
/// goes round the OUTER back edge, and arrives at the inner loop's own
/// pre-header — so every input looks like a back edge, no pre-header is found,
/// and the inner loop is skipped entirely.
///
/// That skip is the documented limitation in `loop_headers`, and it is not a
/// corner: an inner loop is where the iterations are. Measured on
/// `probes/ArrayElemLoadCost.java`, whose `for (r…) for (i…)` shape is the same
/// one every rep-counted probe and most real scan loops have, LICM reported
/// exactly `1 candidate loop header` per method — the OUTER one — and never saw
/// the loop doing the work.
///
/// Returns `(entry_preds, back_ctrls)`.
pub(in crate::ir_optimize) fn entry_preds_by_dominance(
    graph: &Graph,
    users: &[Vec<NodeId>],
    header: NodeId,
) -> (Vec<NodeId>, Vec<NodeId>) {
    let seen = control_not_dominated_by_bits(graph, users, header);
    let mut entries = Vec::new();
    let mut backs = Vec::new();
    for &c in graph.nodes[header as usize].inputs.iter() {
        if c == NO_NODE {
            continue;
        }
        if node_bit_contains(&seen, c) {
            entries.push(c);
        } else {
            backs.push(c);
        }
    }
    (entries, backs)
}

/// Compute the set of nodes that belong to the loop whose header is `region`.
///
/// Membership = control-reachable from `region` along control / projection
/// edges, plus the data / memory nodes pinned to those control nodes, without
/// passing back *out* through `region`'s entry predecessor.  We approximate
/// the body with a forward control walk seeded at `region` and at the control
/// projections of `If` nodes inside it, stopping at the graph exit and at any
/// control edge that leaves to a node dominating `region`.  Because `region`
/// dominates every back-edge source (`loop_headers` checks it), this
/// forward-from-header reachability over-approximates the body safely: an
/// over-large body only *loses* hoisting opportunities (more nodes look
/// variant), never produces an unsound hoist.
pub(in crate::ir_optimize) fn loop_body(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> FxHashSet<NodeId> {
    loop_body_with(graph, &build_users(graph), region, entry_pred, back_ctrls)
}

/// [`loop_body`] over a users map the caller already holds (round 12 wave 2).
///
/// The map is read for CONTROL adjacency only: both walks below (the forward
/// control set and [`control_not_dominated_by`]) skip every user that is not a
/// control node. So a map built once per LICM visit stays exact across that
/// visit's hoists, which re-anchor loads and `ArrayLength`s (never control
/// nodes) and add no node. It used to be rebuilt per loop header, one users
/// vector per arena node each time: O(headers × nodes) allocation per visit.
/// A map whose length disagrees with the arena (a node was added since) is not
/// trusted, and a fresh one is built instead.
pub(crate) fn loop_body_with(
    graph: &Graph,
    users: &[Vec<NodeId>],
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> FxHashSet<NodeId> {
    // Control nodes whose nearest enclosing loop header is `region`. We seed
    // with `region` itself and walk *users* (forward control flow).
    let rebuilt: Vec<Vec<NodeId>>;
    let users: &[Vec<NodeId>] = if users.len() == graph.nodes.len() {
        users
    } else {
        rebuilt = build_users(graph);
        &rebuilt
    };

    // `entry_pred` (the pre-header control) and `back_ctrls` (the back-edge
    // control nodes) are classified structurally by the caller (`loop_headers`),
    // so this works for `Merge` headers whose slot order is not fixed.

    // (1) Forward control set: control nodes reachable from `region` along
    // control / projection successor edges, without leaving through the entry
    // predecessor. We intentionally DO NOT stop at nested loop headers: an
    // over-large forward set is trimmed back to the natural loop by the
    // backward intersection in (2), so any nested-loop control (and its
    // barriers — stores/calls) stays inside the body. This is the SAFE
    // direction: an over-large body only loses hoists, never enables an unsound
    // one, whereas under-approximating could hide a nested-loop store from the
    // barrier check.
    let n = graph.nodes.len();
    let words = (n + 63) >> 6;
    let mut forward_bits = vec![0u64; words];
    node_bit_insert(&mut forward_bits, region);
    let mut work = vec![region];
    while let Some(cur) = work.pop() {
        for &u in &users[cur as usize] {
            if u == entry_pred || node_bit_contains(&forward_bits, u) {
                continue;
            }
            let op = &graph.nodes[u as usize].op;
            if op.is_control() && !matches!(op, Op::Return | Op::Throw) {
                if node_bit_insert(&mut forward_bits, u) {
                    work.push(u);
                }
            }
        }
    }

    let mut body_bits = vec![0u64; words];
    let mut body_count = usize::from(node_bit_insert(&mut body_bits, region));
    let mut back: Vec<NodeId> = back_ctrls.to_vec();
    while let Some(cur) = back.pop() {
        if cur == NO_NODE || cur == region || node_bit_contains(&body_bits, cur) {
            continue;
        }
        if !node_bit_contains(&forward_bits, cur) {
            continue;
        }
        if node_bit_insert(&mut body_bits, cur) {
            body_count += 1;
        }
        for &inp in &graph.nodes[cur as usize].inputs {
            if inp != region && node_bit_contains(&forward_bits, inp) {
                back.push(inp);
            }
        }
    }

    let not_dominated: Option<Vec<u64>> =
        if matches!(graph.node_opt(graph.entry), Some(s) if s.op == Op::Start) {
            Some(control_not_dominated_by_bits(graph, users, region))
        } else {
            None
        };
    loop {
        let before = body_count;
        for (id, node) in graph.nodes.iter().enumerate() {
            if (body_bits[id >> 6] & (1u64 << (id & 63))) != 0 {
                continue;
            }
            if node.op == Op::Dead || node.op.is_pure() {
                continue;
            }
            if node.op == Op::Phi {
                if node.inputs.first().copied() == Some(region) {
                    body_bits[id >> 6] |= 1u64 << (id & 63);
                    body_count += 1;
                }
                continue;
            }
            if not_dominated.is_some() && node.ty == IrType::Control && node.op.is_control() {
                continue;
            }
            let ctrl = node
                .input_opt(0)
                .filter(|&c| matches!(graph.node_opt(c), Some(cn) if cn.ty == IrType::Control));
            let member = match ctrl {
                Some(c) if node_bit_contains(&body_bits, c) => true,
                Some(c) if not_dominated.as_ref().is_some_and(|nd| !node_bit_contains(nd, c)) => {
                    false
                }
                _ => node.inputs.iter().any(|&inp| node_bit_contains(&body_bits, inp)),
            };
            if member {
                body_bits[id >> 6] |= 1u64 << (id & 63);
                body_count += 1;
            }
        }
        if body_count == before {
            break;
        }
    }
    let mut body = FxHashSet::with_capacity_and_hasher(body_count, Default::default());
    for (wi, &w) in body_bits.iter().enumerate() {
        let mut word = w;
        while word != 0 {
            let b = word.trailing_zeros() as usize;
            body.insert(((wi << 6) | b) as NodeId);
            word &= word - 1;
        }
    }
    body
}

/// True if `id` produces a value that is constant across all iterations of the
/// loop with header `region` and body `body`. Conservative: any uncertainty
/// (out-of-range, Dead, a phi whose merge point is in the body, or membership
/// in the body) returns false. A phi whose control anchor is *outside* the loop
/// IS invariant (a merge decided outside it — before it, or in an enclosing
/// loop). A depth bound caps the recursion (pure expression chains only, since
/// both φ arms answer without recursing) — exceeding it is treated
/// conservatively as variant.
pub(crate) fn is_loop_invariant(graph: &Graph, id: NodeId, region: NodeId, body: &FxHashSet<NodeId>) -> bool {
    let mut memo: FxHashMap<NodeId, bool> = FxHashMap::default();
    is_loop_invariant_d(graph, id, region, body, 0, &mut memo)
}

/// Is `node` pinned to a control node OUTSIDE the loop body — an impure node
/// whose slot 0 is a control-typed node that `body` does not contain?
///
/// Such a node executes where its control puts it, and `loop_body`'s pinned
/// sweep has already put every impure node with ANY input in the body into the
/// body, so a pinned node outside it runs outside the loop: once before the
/// loop (the only place a value used inside the loop can come from, since the
/// value must dominate its use), or on an exit path (whose values reach the
/// loop again only through a φ anchored outside it). Either way it holds one
/// value for the whole run of the loop.
pub(in crate::ir_optimize) fn pinned_outside_body(graph: &Graph, node: &Node, body: &FxHashSet<NodeId>) -> bool {
    if node.op.is_pure() || node.op == Op::Phi {
        return false;
    }
    match node
        .input_opt(0)
        .and_then(|c| graph.node_opt(c).map(|n| (c, n)))
    {
        Some((c, cn)) => cn.ty == IrType::Control && cn.op != Op::Dead && !body.contains(&c),
        None => false,
    }
}

/// The memoised body of [`is_loop_invariant`].
///
/// Every answer is memoised, including a `false` the depth bound produced. That
/// can only make a later visit of the same node at a shallower depth answer
/// `false` where the unmemoised walk would have said `true` — a refused hoist,
/// never a wrong one — and it keeps the worst case linear, which caching only
/// "exact" answers would not.
///
/// # Why a memo (round 11)
///
/// The walk used to re-descend every shared input: a value DAG with sharing
/// (`a ^= a << 13; a ^= a >>> 7; …`) or a chain of `if`/`else` diamonds above
/// a pre-loop load (reached through its control and memory inputs) made it
/// EXPONENTIAL in the number of diamonds within the depth bound — 2^k visits
/// per query, and it is asked several times per candidate load. And because
/// the walk went up the whole dominating control chain of a pre-loop load, any
/// such load more than ~64 levels below `Start` read as VARIANT, so a loop late
/// in a long method could not hoist `a.length` of a field-loaded `a`.
///
/// # The two short-circuits
///
/// * A φ anchored outside the body is invariant, full stop: its value changes
///   only when control passes its anchor, and no iteration of this loop does.
///   The old rule also demanded invariant value INPUTS, which an enclosing
///   loop's induction φ never has (its back-edge input is `j + 1`, which leads
///   back to the φ, which bottomed out at the depth bound as variant). That is
///   what kept `row.length` / `node.val` of an OUTER induction value from
///   hoisting out of every inner loop.
/// * An impure node pinned outside the body — see [`pinned_outside_body`].
pub(in crate::ir_optimize) fn is_loop_invariant_d(
    graph: &Graph,
    id: NodeId,
    region: NodeId,
    body: &FxHashSet<NodeId>,
    depth: u32,
    memo: &mut FxHashMap<NodeId, bool>,
) -> bool {
    if !graph.is_valid_id(id) {
        return false;
    }
    if let Some(&v) = memo.get(&id) {
        return v;
    }
    if depth > 64 {
        // Recursion bound hit: give up conservatively.
        return false;
    }
    let node = &graph.nodes[id as usize];
    let answer = match node.op {
        Op::Dead => false,
        // Constants / params are defined at Start — always invariant.
        Op::Const(_) | Op::ConstF(_) | Op::Param(_) => true,
        // A phi is loop-invariant iff its control anchor (the merge point at
        // input slot 0) is OUTSIDE the loop body — so the merge runs once, not
        // per iteration. A phi anchored at this loop's region (induction /
        // loop-carried) or at an in-loop merge (an in-loop if/else join) has
        // its anchor IN the body and is variant. This is the cross-merge half
        // of the alias oracle on the *load* side: a base like `cond ? A : B`
        // decided before the loop is a hoistable invariant, and so is an
        // enclosing loop's induction value.
        Op::Phi => {
            // `input_opt` reads the control anchor as `None` when the slot is
            // missing *or* holds the placeholder — the two cases this arm
            // treats identically ("no anchor we can prove is outside").
            match node.input_opt(0) {
                Some(anchor) if graph.is_valid_id(anchor) && !body.contains(&anchor) => {
                    graph.nodes[anchor as usize].op != Op::Dead
                }
                _ => false,
            }
        }
        _ => {
            if body.contains(&id) {
                // Anything physically in the loop body is variant.
                false
            } else if pinned_outside_body(graph, node, body) {
                true
            } else {
                // Otherwise invariant iff every input is invariant. (Pure nodes
                // have no control/memory inputs; a floating impure node such
                // as `Div` is judged by its operands, and the caller's barrier
                // checks decide whether it may move.)
                node.inputs.iter().all(|&inp| {
                    inp == NO_NODE || is_loop_invariant_d(graph, inp, region, body, depth + 1, memo)
                })
            }
        }
    };
    memo.insert(id, answer);
    answer
}

/// The memory state flowing INTO a loop, as seen at its header.
///
/// When the header carries a memory phi, that phi's input for the loop-entry
/// predecessor is the memory the pre-header ends with; when it does not, memory
/// is loop-invariant already and the caller's own invariance test answers
/// first. Used only to re-anchor a node whose alias class conflicts with no
/// store, so this is an ORDERING choice and never a claim that some other
/// memory state is equivalent.
pub(in crate::ir_optimize) fn loop_entry_memory(graph: &Graph, region: NodeId, entry_pred: NodeId) -> Option<NodeId> {
    let slot = graph.nodes[region as usize]
        .inputs
        .iter()
        .position(|&p| p == entry_pred)?;
    anchored_at(graph, region)
        .into_iter()
        .filter_map(|id| graph.node_opt(id))
        .find(|node| node.op == Op::Phi && node.ty == IrType::Memory)
        // Phi inputs are `[region, v_for_pred0, v_for_pred1, …]`, aligned with
        // the region's own predecessor list.
        .and_then(|phi| phi.inputs.get(1 + slot).copied())
}

/// Can any node anchored at this control token raise an exception?
///
/// The question a pre-header hoist has to answer: moving a potentially-throwing
/// node into that block is only invisible if nothing already there could throw
/// first.
///
/// This is the **trap** question, not the memory question, and it is the one
/// site of the four where the two pull in opposite directions: `Op::Div` and
/// `Op::Rem` touch no memory ([`memory_shape_of`] answers
/// [`MemoryShape::None`] for both) and are nonetheless exactly what this
/// predicate is looking for, because a zero divisor raises
/// `ArithmeticException`. So it reads [`may_raise`], which is that
/// question and only that question. `Op::LCmp` / `Op::FCmp` fall out for free:
/// they raise nothing, and no list here has to remember them.
///
/// Conservative in both directions that matter: a node this cannot see (one
/// scheduled into the block by `find_best_block` rather than pinned by its
/// control input) cannot raise — [`may_raise`] is false for every floating
/// node — and a node it does see and cannot classify counts as trapping.
///
/// # The two exemptions, and why they are stated here rather than in the model
///
/// [`may_raise`] says a `Load` or `Store` may raise, which is true: a null
/// base faults. This predicate exempts them anyway, and deliberately, because
/// of what it is clearing a path for — an `ArrayLength` or a guarded load
/// anchored at the loop header, whose own null check is the guard
/// `control_token_has_guard` separately insists on. A pre-header full of the
/// caller's own field loads is the normal case, and counting those as "the
/// pre-header can throw first" would refuse every hoist this arm exists to
/// make. `Op::Phi` and `Op::Dead` are exempt for the ordinary reasons: a φ is
/// a merge, and a removed node emits nothing.
///
/// The exemption lives at this call site because its justification is a
/// property of *this analysis*, not of the op. Putting it in [`may_raise`]
/// would make that function state something false about a `Load`.
pub(in crate::ir_optimize) fn preheader_may_trap(graph: &Graph, preheader: NodeId) -> bool {
    anchored_at(graph, preheader).into_iter().any(|id| {
        graph.node_opt(id).is_some_and(|node| {
            !node.op.is_control()
                && !matches!(node.op, Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead)
                && !direct_static_read_cannot_raise(graph, node)
                // JIT r23 wave 8 (lane W8-L4): a segment-field read the FFM
                // hoist placed here raises nothing; it only deopts to this
                // loop's entry, where the interpreter re-runs the loop.
                && !ffm_field_hoist::resumes_at_its_loop_entry(graph, id)
                && may_raise(&node.op)
        })
    })
}

/// [`preheader_may_trap`] for the `ArrayLength` arm, which may also step over
/// a pre-header node that RESUMES AT THIS LOOP'S HEADER: the copy
/// [`hoist_invariant_array_loads`] plants for `c[i]` (MX16-2, round 16
/// matrix). Such a node's fault deopts to the header's own entry frame, and so
/// does a hoisted `ArrayLength`'s (`install_preheader_deopt_frame`). Whichever
/// of the two faults, the interpreter re-runs the header from its first
/// bytecode with the same values and raises the exception bytecode order
/// raises first -- the order of the two in the pre-header is not observable.
/// Any other trapping pre-header node still refuses, as before.
pub(in crate::ir_optimize) fn preheader_may_trap_except_header_resumes(
    graph: &Graph,
    preheader: NodeId,
    region: NodeId,
) -> bool {
    let header_bci = graph.node_opt(region).and_then(|r| r.bytecode_pc);
    anchored_at(graph, preheader).into_iter().any(|id| {
        graph.node_opt(id).is_some_and(|node| {
            let resumes_at_header = header_bci.is_some()
                && matches!(node.op, Op::ArrayLoad(_))
                && node
                    .frame_snapshot
                    .and_then(|si| graph.safepoints.get(si as usize))
                    .is_some_and(|sp| Some(sp.bci) == header_bci && !sp.is_splice_state());
            !node.op.is_control()
                && !resumes_at_header
                && !matches!(node.op, Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead)
                && !direct_static_read_cannot_raise(graph, node)
                && may_raise(&node.op)
        })
    })
}

/// `CRATONVM_JIT_IR_LICM_DIRECT_STATIC`, **default OFF** (opt-in) -- JIT
/// round 19, lane irops19, IRLOWER17-1 (= OI-3) of
/// `jit-r17-irlower-proposals-RETIRED-20261006.md`: LICM hoists a plain
/// `getstatic` the lowering reads DIRECTLY ([`hoist_direct_static_reads`]).
/// `lib.rs` feeds `Graph::direct_static_fields` while it is on. Read live,
/// once per loop visit (no `static`: `process_global_statics_ratchet`).
pub(crate) fn ir_licm_direct_static_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_LICM_DIRECT_STATIC_OSR_ONLY` (JIT round 19 wave 12, lane
/// hoist12, HOIST11-3) -- **default on** (default on; inert unless
/// `CRATONVM_JIT_IR_LICM_DIRECT_STATIC` is on): the direct-static LICM arm runs
/// only on the optimizing OSR route's compile (`Graph::optimize_osr_route`).
/// A method-entry compile is then served by the pre-loop static-read hoist
/// alone (`loop_static_hoist`, with HOIST11-1 / HOIST11-2 covering the LICM
/// arm's stores and its OSR re-read). Nothing is deleted. No `static`.
pub(crate) fn ir_licm_direct_static_osr_only_enabled() -> bool {
    true
}

/// Is `node` a NON-volatile `getstatic` this compile lowers as the direct
/// read ([`static_read_is_direct`]: inline route on, field in
/// `Graph::direct_static_fields`, i.e. its class initialised at compile time),
/// in the full `[ctrl, mem]` form?
///
/// Such a read runs no `<clinit>`, makes no call, emits no safepoint map and
/// cannot trap: `ir_lower::emit_inline_getstatic` is two `MOV`s through the
/// class's baked statics-base cell. It is still `MemAccess::Opaque` in the
/// effect model (and advances the memory token in the builder), because
/// `effect_of_node` sees one node and not the compile's direct-field list.
pub(crate) fn is_direct_static_read(graph: &Graph, node: &Node) -> bool {
    match node.op {
        Op::LoadStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        } => node.inputs.len() >= 2 && static_read_is_direct(graph, class_id, field_index),
        _ => false,
    }
}

/// The pre-header trap exemption for a direct static read hoisted by
/// [`hoist_direct_static_reads`]: it cannot raise, so it must not stop a
/// later arm from hoisting a load into the same pre-header. `false` with the
/// switch off, which keeps both trap predicates byte-identical to before.
///
/// JIT round 19 wave 12 (lane hoist12, HOIST12-1): the exemption was keyed on
/// the LICM arm's switch, so a read the default-ON pre-loop hoist
/// (`loop_static_hoist`) parked in the same pre-header counted as a TRAPPING
/// node there and refused every later arm (the `ArrayLength` hoist, the load
/// hoists) for exactly the loops that pass serves -- the `read-only` row's
/// LICM-only gain in the i22 A/B. Under
/// `CRATONVM_JIT_IR_DIRECT_STATIC_PREHEADER_NO_TRAP` (opt-in) the exemption
/// holds for any direct read, whichever pass put it there: the argument
/// (two plain loads, no `<clinit>`, no call) is about the read, not the pass.
pub(in crate::ir_optimize) fn direct_static_read_cannot_raise(graph: &Graph, node: &Node) -> bool {
    (ir_licm_direct_static_enabled()
        || loop_static_hoist::direct_static_preheader_no_trap_enabled())
        && is_direct_static_read(graph, node)
}

/// Does body node `id` forbid hoisting a direct static read out of its loop?
///
/// The read's VALUE is loop-invariant iff nothing in the loop can write a
/// static cell or run Java code that could: so every `Op::StoreStatic` (any
/// cell -- it is a helper call that can run `<clinit>`), every call, every
/// non-direct `getstatic` (the helper route runs `<clinit>`), allocation (a
/// `new` of an uninitialised class runs `<clinit>`), monitors (an acquire
/// orders later reads after it) and every volatile access (same) refuse. What
/// is admitted cannot write a static cell:
///
/// * `Op::Load` / `Op::ArrayLoad` / `Op::ArrayLength` read;
/// * `Op::Store` / `Op::ArrayStore` write INSTANCE / ELEMENT storage, and a
///   class's statics block is not any object's instance storage (the premise
///   `Graph::may_alias` records for `AliasClass::Static`);
/// * `Op::Guard` leaves the body without writing, and the direct read cannot
///   trap, so hoisting it above a guard reads a value nothing uses on the
///   deopt path;
/// * a memory `Op::Phi`, `Op::Dead`, `Op::OsrMemory` (as for
///   `licm_hoist_barrier`) and control.
///
/// A plain read hoisted past a loop that never synchronises is exactly what
/// the JMM permits and what HotSpot's C2 does for a non-volatile static
/// (the classic `while (!flag) {}` that never terminates).
pub(in crate::ir_optimize) fn blocks_direct_static_hoist(graph: &Graph, id: NodeId) -> bool {
    let Some(node) = graph.node_opt(id) else {
        return true;
    };
    if node.is_volatile_access() {
        return true;
    }
    match &node.op {
        Op::LoadStatic { .. } => !is_direct_static_read(graph, node),
        Op::Phi
        | Op::Dead
        | Op::OsrMemory
        | Op::Guard { .. }
        | Op::Load(_)
        | Op::Store(_)
        | Op::ArrayLoad(_)
        | Op::ArrayStore(_)
        | Op::ByteArrayCopy
        | Op::ArrayLength
        // JIT round 21 (lane charat, STRINTR21-5): the String char read, a
        // byte-array read like the two `ArrayLoad`s it replaced.
        | Op::StrCharDecode => false,
        op if op.is_control() => false,
        op => memory_shape_of(op).touches_memory(),
    }
}

/// JIT round 19 (lane irops19, IRLOWER17-1 / OI-3): hoist every direct,
/// non-volatile `getstatic` of this loop to its pre-header, when nothing in
/// the loop can change a static ([`blocks_direct_static_hoist`]).
///
/// The three obligations of the other hoist arms, discharged:
///
/// 1. **The value cannot change** -- the body scan above, all or nothing per
///    loop (the reads are Opaque in the effect model, so per-cell alias
///    precision is not available and not attempted).
/// 2. **No throw point, call or `<clinit>` moves** -- the direct read has
///    none ([`is_direct_static_read`]); it is also why a hoist from under a
///    branch the loop may never take is harmless.
/// 3. **The memory chain is re-anchored.** The builder threads the token
///    through every `getstatic` (`self.mem = load`), so the read's TOKEN users
///    are rewired to its own incoming token first, then the read takes the
///    pre-header as control and the loop's entry memory as its token. The read
///    writes nothing, so dropping it from the chain loses no ordering edge a
///    write needed. A loop whose header has no memory φ has no entry token to
///    name and is skipped.
///
/// A hoisted read is removed from `body`, so the arms after this one see a
/// loop without it. Returns the number hoisted.
pub(in crate::ir_optimize) fn hoist_direct_static_reads(
    graph: &mut Graph,
    region: NodeId,
    entry_pred: NodeId,
    body: &mut FxHashSet<NodeId>,
    dbg: bool,
) -> usize {
    let mut candidates: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            graph
                .node_opt(id)
                .is_some_and(|n| is_direct_static_read(graph, n))
        })
        .collect();
    if candidates.is_empty() {
        return 0;
    }
    // JIT round 19 wave 2 (lane irops2, IROPS19-1), under
    // `CRATONVM_JIT_IR_STATIC_ALIAS_CLASS` (`Graph::static_alias_class`): a
    // DIRECT primitive `putstatic` (`Graph::direct_static_access`) writes
    // exactly its own cell -- an inline store into an initialised class's
    // statics block, no call, no `<clinit>` -- so it refuses only the reads
    // of THAT cell instead of every read of the loop. Collected while the
    // blocker scan runs; complete whenever the scan finds no blocker, which is
    // the only case it is read in.
    let per_cell = graph.static_alias_class;
    let mut written: Vec<crate::ir::AliasClass> = Vec::new();
    // HOIST11-5 (wave 12, opt-in `CRATONVM_JIT_IR_STATIC_HOIST_SHARED_SCAN`):
    // the one classifier both direct-static hoists share, in this arm's mode.
    let shared = loop_static_hoist::static_hoist_shared_scan_enabled();
    let mode = loop_static_hoist::StaticHoistScanMode {
        heap_stores: true,
        per_cell,
        osr_memory: true,
    };
    if let Some(blocker) = body.iter().copied().find(|&id| {
        if shared {
            return match graph
                .node_opt(id)
                .map(|n| loop_static_hoist::classify_for_static_hoist(graph, n, mode))
            {
                Some(loop_static_hoist::StaticHoistNode::CellWrite(cell)) => {
                    written.push(cell);
                    false
                }
                Some(loop_static_hoist::StaticHoistNode::Refuse) | None => true,
                Some(_) => false,
            };
        }
        if per_cell {
            if let Some((cell, true)) = graph
                .node_opt(id)
                .and_then(|n| graph.direct_static_access(n))
            {
                written.push(cell);
                return false;
            }
            // JIT round 19 wave 4 (lane irops3, IROPS2-5), under
            // `Graph::static_ref_store_effect`: a reference `putstatic` of
            // an initialised class runs no `<clinit>` and writes only its
            // own cell, so it too refuses only the reads of that cell.
            if let Some(cell) = graph
                .node_opt(id)
                .and_then(|n| graph.direct_static_ref_store(n))
            {
                written.push(cell);
                return false;
            }
        }
        blocks_direct_static_hoist(graph, id)
    }) {
        if dbg {
            eprintln!(
                "[DBG_LICM] direct-static header {region}: skip {} read(s) -- n{blocker} ({:?}) \
                 may change a static",
                candidates.len(),
                graph.node_opt(blocker).map(|n| &n.op)
            );
        }
        return 0;
    }
    let Some(entry_mem) = loop_entry_memory(graph, region, entry_pred) else {
        if dbg {
            eprintln!("[DBG_LICM] direct-static header {region}: skip -- no loop-entry memory");
        }
        return 0;
    };
    if !written.is_empty() {
        candidates.retain(|&read| {
            let keep = !graph
                .node_opt(read)
                .and_then(|n| graph.direct_static_access(n))
                .is_some_and(|(cell, _)| written.contains(&cell));
            if !keep && dbg {
                eprintln!("[DBG_LICM] direct-static read {read}: skip -- the loop stores its cell");
            }
            keep
        });
    }
    candidates.sort_unstable();
    let mut moved = 0usize;
    for read in candidates {
        let Some(&mem_in) = graph.nodes[read as usize].inputs.get(1) else {
            continue;
        };
        // Token users of the read take the read's own incoming token.
        let token_users: Vec<(NodeId, usize)> = graph
            .nodes
            .iter()
            .enumerate()
            .flat_map(|(u, node)| {
                node.inputs
                    .iter()
                    .enumerate()
                    .filter(move |&(k, &inp)| {
                        inp == read && crate::ir::is_memory_token_slot(node, k)
                    })
                    .map(move |(k, _)| (u as NodeId, k)) // Cast: node index -> NodeId
            })
            .collect();
        for (u, k) in token_users {
            graph.set_input(u, k, mem_in);
        }
        if dbg {
            eprintln!(
                "[DBG_LICM] direct-static read {read}: HOIST to preheader {entry_pred}, \
                 mem {mem_in} -> {entry_mem}"
            );
        }
        graph.set_input(read, 0, entry_pred);
        graph.set_input(read, 1, entry_mem);
        body.remove(&read);
        // JIT round 19 wave 2 (lane irops2, IROPS19-7): the OSR entry stub
        // may re-read exactly these (`ir_lower::osr_remat_compile`): nothing
        // between the pre-header and any entry inside this loop writes the
        // cell, which is the precondition just checked.
        if !graph.licm_hoisted_static_reads.contains(&read) {
            graph.licm_hoisted_static_reads.push(read);
        }
        moved += 1;
    }
    moved
}

/// Does an `Op::Guard` hang off this control token?
///
/// `Op::Guard` carries `[ctrl, cond]` and shares its control input with the
/// nodes it protects: "the guard runs before the load" is a fact about the
/// ORDER INSIDE one block (`ir.rs`: the scheduler pins the guard into the
/// block and its ordering against everything else there is positional), and no
/// edge in the graph expresses it. A pass that re-anchors a node to a
/// different block therefore cannot preserve a relationship it cannot see.
///
/// That is the whole reason this exists. The IR String-access expansion emits
///
/// ```text
/// Guard(recv != null)        ctrl = region
/// Load(Int, recv.coder)      ctrl = region
/// Load(Ref, recv.value)      ctrl = region
/// Guard(value != null)       ctrl = region
/// ArrayLength(value)         ctrl = region
/// ```
///
/// where every node after the first is legal only *because* the guard deopted
/// on the null case first. Hoisting one of them to the pre-header runs it on a
/// receiver the guard was going to reject, which for a null base is a deopt
/// raised from a pre-header frame at a bci the loop never reached, and for a
/// guard that establishes anything about an object's SHAPE is a read at an
/// offset that object does not have.
///
/// So the answer is deliberately coarse: ANY guard at the token blocks every
/// positional hoist out of it, whether or not that particular guard is the one
/// protecting that particular node. Refusing costs hoists in guarded loops;
/// admitting one wrongly costs a wild read.
///
/// [`LoopGuards`] is the finer question this one cannot answer, and the arms
/// consult that instead. This predicate survives because it is the honest
/// spelling of "is this header guarded at all", which the diagnostics and the
/// tests still want.
pub(in crate::ir_optimize) fn control_token_has_guard(graph: &Graph, ctrl: NodeId) -> bool {
    if ctrl == NO_NODE {
        return false;
    }
    anchored_at(graph, ctrl).into_iter().any(|id| {
        graph
            .node_opt(id)
            .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
    })
}

/// `true` unless `CRATONVM_JIT_IR_LICM_GUARD_ATTRIBUTION` is turned off: the
/// kill switch for [`LoopGuards::permits_hoist`]'s attribution arm.
///
/// Default **ON** and not default-off, which is the opposite of this file's
/// usual convention for a widening, because of what the arm does and does not
/// do. It moves no guard, so it moves no deopt point (see [`LoopGuards`] for
/// why that distinction is the whole design), and everything it admits has to
/// clear a permission STRICTER than the unguarded path's — a base that is
/// non-null by construction, not merely a base at the header. Setting this to
/// `0` restores the blanket refusal `control_token_has_guard` describes, which
/// is what to bisect with if a guarded loop starts misbehaving.
pub(in crate::ir_optimize) fn licm_guard_attribution_enabled() -> bool {
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// How many nodes a cone walk may visit before it gives up and fails closed.
///
/// A cone is walked once per loop for the guards and once per hoist candidate,
/// so the bound is on a hot-ish path. It is generous relative to the shapes
/// that matter (a String expansion's condition cone is under ten nodes) and the
/// failure mode is a refused hoist, never a wrong one.
pub(in crate::ir_optimize) const GUARD_CONE_BUDGET: usize = 512;

/// The transitive INPUT cone of `root`, accumulated into `out`.
///
/// Returns `false` when the budget ran out, in which case `out` is incomplete
/// and every caller must treat the question as unanswerable — an incomplete
/// cone makes the disjointness test in [`LoopGuards::permits_hoist`] answer
/// "unrelated" for a node that is in fact related, which is the direction that
/// produces a wild read.
///
/// Three kinds of node are deliberately NOT members, and are not walked
/// through either. Each is a *universal connector* — something almost every
/// node in a method reaches — so including it would make almost every cone
/// intersect almost every other one and the analysis would degenerate back into
/// the blanket refusal. Correct, but pointlessly so. In each case the exclusion
/// is also the right answer on the merits:
///
/// * **constants** (`Op::Const` / `Op::ConstF`). A guard cannot establish
///   anything about a literal that was not already true of it, so the
///   ubiquitous `0` appearing in both a guard's condition and a load's address
///   is not evidence the two are related.
/// * **control nodes.** A guard's condition is a value; control carries none.
///   Every `Op::Param` in a method names `Op::Start`, and every node at a loop
///   header names the header — sharing either says nothing about who depends on
///   whose proof. (The guard's own control input is skipped at the call site
///   for the same reason.)
/// * **memory-typed nodes.** The memory token is an ORDERING edge, not an
///   address: every load in a loop threads the same memory phi. Excluding it
///   costs nothing this analysis needs, because the relation being modelled is
///   "the guard proves a property of a value the candidate's ADDRESS is
///   computed from", and a memory token is never part of an address. The
///   String expansion's second guard is the case to check against — `value =
///   Load(recv.value)`, `Guard(value != null)`, `ArrayLength(value)` — and it
///   still intersects, on `value` and on `recv`, which are values.
pub(in crate::ir_optimize) fn value_cone(
    graph: &Graph,
    root: NodeId,
    out: &mut FxHashSet<NodeId>,
    budget: &mut usize,
) -> bool {
    value_cone_with(graph, root, out, budget, false)
}

/// `CRATONVM_JIT_IR_LICM_GUARD_CONE_FIELD_VALUE` (the r23 L8 heap-buffer
/// close; perf, **default ON**, `=0` restores the full walk): a GUARD's
/// condition cone ([`LoopGuards::over`]) holds a field read `Load(base.f)`
/// itself but not what it was read from.
///
/// Why sound: a guard on the VALUE of `base.f` proves something about that
/// value and about everything computed from it -- both still in the cone, so
/// `base.f.g` (its own cone holds the `Load`) is refused as before -- and
/// nothing about `base`: whether `base.g` exists is a fact of `base`'s class,
/// which only a test OF `base` establishes (`ExactClassIs(base)`, a null test,
/// a class word compare -- none of them a `Load`, so the walk still reaches
/// `base` through them). The read's other obligation, that it cannot deopt
/// where it lands, is unchanged (`base_non_null_on_entry` only).
///
/// Why: `absGet`'s spliced loop guards `segment != null`
/// (`Buffer.session()`) and `k << 2 < limit - 3` (`checkIndex`), both on
/// values READ from the buffer, so the full walk put the buffer itself in the
/// guard cone and LICM refused every field read of it ("a guard may be what
/// makes the read legal"): five reloads per access. Read once per loop.
pub(in crate::ir_optimize) fn licm_guard_cone_field_value_enabled() -> bool {
    true
}

/// [`value_cone`], stopping at a field `Op::Load` (kept, its inputs not
/// walked) when `stop_at_field_reads` -- a guard's condition cone under
/// [`licm_guard_cone_field_value_enabled`].
pub(in crate::ir_optimize) fn value_cone_with(
    graph: &Graph,
    root: NodeId,
    out: &mut FxHashSet<NodeId>,
    budget: &mut usize,
    stop_at_field_reads: bool,
) -> bool {
    let mut work = vec![root];
    while let Some(cur) = work.pop() {
        if !graph.is_valid_id(cur) {
            continue;
        }
        let node = &graph.nodes[cur as usize];
        if matches!(node.op, Op::Const(_) | Op::ConstF(_))
            || node.op.is_control()
            || node.ty == IrType::Memory
        {
            continue;
        }
        if !out.insert(cur) {
            continue;
        }
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if stop_at_field_reads && matches!(node.op, Op::Load(_)) {
            continue;
        }
        for &inp in &node.inputs {
            work.push(inp);
        }
    }
    true
}

/// What the `Op::Guard`s covering a hoist candidate allow.
///
/// # Why a guard blocks a hoist at all
///
/// `Op::Guard` carries `[ctrl, cond]` and shares its control input with the
/// nodes it protects, so "the guard ran first" is a fact about the order inside
/// one block and no edge expresses it (`control_token_has_guard` tells that
/// story at length). Two separate things go wrong when a node moves above a
/// guard:
///
/// 1. **The node may be relying on what the guard established.** A guard that
///    proves an object's SHAPE makes a read behind it a read at an offset the
///    object may not have; a guard that proves an index in range makes the
///    access behind it an in-bounds one. Move the read, and the proof is gone.
/// 2. **The node may deopt where the guard would have deopted first.** Both a
///    guard and a null-based `Op::Load` leave for the interpreter, and each
///    names its own `bci` as the resume point. Running the load first resumes
///    the interpreter at the LOAD's bci when the correct answer was the
///    guard's, which skips every bytecode between them.
///
/// # What this answers instead of refusing outright
///
/// (1) is a question about VALUES and the graph does express it: a guard
/// constrains the values its condition is computed from, and a node computed
/// from an entirely disjoint set of values cannot be relying on it. So the
/// union of every covering guard's condition cone is computed once per loop
/// ([`value_cone`]), and a candidate whose own cone is disjoint from it is not
/// one of that guard's dependents. When the ir.rs guard-token edge lands (see
/// `NOTES-opts7.md`) this needs no change at all: the guard node itself is a
/// member of its own cone, so a node naming it as an input intersects and is
/// refused, and the licence becomes an explicit graph fact rather than an
/// inference about condition operands.
///
/// (2) is NOT answered, and is instead made impossible. Under any guard the
/// only admissible permission is `base_non_null_on_entry` — a receiver
/// parameter, a fresh allocation, a materialized constant — which says the
/// hoisted read cannot deopt at all. The permissions the unguarded path
/// accepts, `inputs[0] == region` and `anchored_on_every_iteration`, are both
/// POSITIONAL claims of the form "this node runs on the first pass through the
/// header", and a guard at that header is exactly the thing that falsifies
/// them. They are refused here however invariant the base looks.
///
/// # Why this never moves a guard, and what that buys
///
/// `NOTES-misc.md` §4 option A would hoist the guard itself into the pre-header
/// and let the loads follow. That moves a DEOPT POINT, and the obligations it
/// takes on are not the ones the original sketch listed:
///
/// * the pre-header must perform no observable work, because the frame the
///   deopt rebuilds is keyed on the guard's own `bci` — inside the loop — so
///   the interpreter resumes at the header and never re-executes a pre-header
///   store that had not run yet. `preheader_may_trap` asks whether the
///   pre-header can THROW, not whether it can WRITE, and answers `false` for
///   `Op::Store` by name;
/// * and, worse, the frame state itself: `ir_lower` resolves a guard's frame
///   from the safepoint snapshot for its `bci`, whose locals include the
///   loop-carried phis. A phi's slot is written by `emit_phi_copies` at the
///   PREDECESSOR'S TERMINATOR — i.e. at the very end of the pre-header block —
///   so a guard lowered into that block reads the phi slot before anything has
///   written it and reconstructs an interpreter frame holding a stale
///   induction variable. That is a wrong answer, not a lost optimization, and
///   no predicate available to this pass can see it.
///
/// Nothing here moves a guard, so neither obligation arises. The tests
/// `licm_never_re_anchors_a_guard_out_of_its_loop` and
/// `a_pre_header_store_cannot_be_stranded_by_a_guarded_header` pin that.
pub(in crate::ir_optimize) enum LoopGuards {
    /// No `Op::Guard` covers the candidate — the classic unguarded loop, where
    /// the arms keep every permission they had.
    None,
    /// Guards cover the candidate and the union of their condition cones was
    /// computed in full.
    Attributed(FxHashSet<NodeId>),
    /// Guards cover the candidate and at least one cone overflowed the budget.
    /// Indistinguishable from "related to everything", and treated as such.
    Opaque,
}

impl LoopGuards {
    /// The guards whose control input is `ctrl`, summarised.
    ///
    /// Used for a candidate anchored AT the loop header: a guard deeper in the
    /// body runs after the header in the same iteration, so it cannot be the
    /// thing that licensed a node the header itself anchors. (Across
    /// iterations the question does not arise either — a guard is re-executed
    /// every trip, so its licence is per-iteration, and the pre-header runs
    /// before any of them.)
    pub(in crate::ir_optimize) fn at_control(graph: &Graph, ctrl: NodeId) -> LoopGuards {
        LoopGuards::over(
            graph,
            anchored_at(graph, ctrl).into_iter().filter(|&id| {
                graph
                    .node_opt(id)
                    .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
            }),
        )
    }

    /// Every guard in the loop body, summarised. Used for a candidate anchored
    /// anywhere else in the loop, where any of them may be the one in front.
    pub(in crate::ir_optimize) fn in_body(graph: &Graph, body: &FxHashSet<NodeId>) -> LoopGuards {
        LoopGuards::over(
            graph,
            body.iter()
                .copied()
                .filter(|&id| matches!(graph.nodes[id as usize].op, Op::Guard { .. })),
        )
    }

    pub(in crate::ir_optimize) fn over(graph: &Graph, guards: impl Iterator<Item = NodeId>) -> LoopGuards {
        let mut cone: FxHashSet<NodeId> = FxHashSet::default();
        let mut budget = GUARD_CONE_BUDGET;
        let mut any = false;
        let field_value = licm_guard_cone_field_value_enabled();
        for g in guards {
            any = true;
            // The guard node ITSELF is a member of its own cone. What reads it
            // is `permits_hoist`, which seeds the candidate's OWN cone with the
            // guard the candidate names as its token (`ir::guard_token_of`) —
            // so a licensed node intersects here and is refused, and the
            // licence is an explicit graph fact rather than an inference about
            // condition operands.
            //
            // `NOTES-opts7.md` claimed this would need "no other line of code",
            // on the reading that `permits_hoist` builds the candidate's cone
            // from the candidate NODE. It does not — it builds it from
            // `(base, addr)`, neither of which names the guard — so the seeding
            // in `permits_hoist` is what makes this line load-bearing. It is
            // inert without the token edge either way.
            cone.insert(g);
            // Slot 0 is the control token, which every node at this header
            // shares and which therefore carries no information about who
            // depends on whom. Everything from slot 1 on is the condition.
            let inputs = graph.nodes[g as usize].inputs.clone();
            for &cond in inputs.iter().skip(1) {
                if !value_cone_with(graph, cond, &mut cone, &mut budget, field_value) {
                    return LoopGuards::Opaque;
                }
            }
        }
        if any {
            LoopGuards::Attributed(cone)
        } else {
            LoopGuards::None
        }
    }

    /// May the read `node` — of `base`, at `addr`, or [`NO_NODE`] when the op
    /// has no address operand — be re-anchored out of the loop past these
    /// guards?
    ///
    /// A permission, not an analysis: every uncertain case answers `false`.
    ///
    /// # What `node` is for
    ///
    /// The address cone below is an INFERENCE: "this read is computed from
    /// values no guard constrains, therefore no guard licensed it". The guard
    /// token edge (`ir::guard_token_of`) is the same claim stated directly, so
    /// when the candidate carries one it is seeded into the candidate's own
    /// cone and the disjointness test refuses without appealing to the
    /// inference at all. A candidate with no token is unchanged, which is
    /// every candidate while `ir::guard_token_edges_enabled` is off.
    pub(in crate::ir_optimize) fn permits_hoist(&self, graph: &Graph, node: NodeId, base: NodeId, addr: NodeId) -> bool {
        self.permits_hoist_proven(graph, node, base, addr, base_non_null_on_entry(graph, base))
    }

    /// [`Self::permits_hoist`] with obligation (2) answered by the caller:
    /// `base_non_null` is `true` when the read cannot deopt where it lands --
    /// its base is non-null on entry, or (JIT round 23 wave 2d, lane L1,
    /// TCL23-1) a pre-header null predicate that resumes at the loop entry runs
    /// before it ([`plant_null_base_load_predicates`]).
    pub(in crate::ir_optimize) fn permits_hoist_proven(
        &self,
        graph: &Graph,
        node: NodeId,
        base: NodeId,
        addr: NodeId,
        base_non_null: bool,
    ) -> bool {
        // The candidate's own guard token, read BEFORE the match so the
        // `LoopGuards::None` arm cannot answer `true` over the top of it.
        // `None` means "no guard is anchored at the control this candidate is
        // anchored to"; a token means "a guard licensed this node". The two
        // disagree only for a shape no producer builds — a node licensed by a
        // guard it does not share a control token with — and refusing is the
        // conservative reading of the disagreement.
        let own_token = graph
            .node_opt(node)
            .map_or(NO_NODE, crate::ir::guard_token_of);
        let cone = match self {
            LoopGuards::None => return own_token == NO_NODE,
            LoopGuards::Opaque => return false,
            LoopGuards::Attributed(c) => c,
        };
        if !licm_guard_attribution_enabled() {
            return false;
        }
        // Obligation (2): the read must not be able to deopt, so that moving it
        // cannot move a deopt point. Nothing weaker is accepted under a guard —
        // in particular not "it is anchored at the header", which is the claim
        // the guard falsifies.
        if !base_non_null {
            return false;
        }
        // Obligation (1): the read's address must be computed from values no
        // guard here constrains.
        let mut own: FxHashSet<NodeId> = FxHashSet::default();
        let mut budget = GUARD_CONE_BUDGET;
        for root in [base, addr] {
            if root == NO_NODE {
                continue;
            }
            if !value_cone(graph, root, &mut own, &mut budget) {
                return false;
            }
        }
        // The explicit half of obligation (1). Inserted rather than walked:
        // `LoopGuards::over` puts each guard into its own cone under its own
        // id, so membership is the whole test, and walking the guard's inputs
        // would only re-add the condition cone that is already there.
        if own_token != NO_NODE {
            own.insert(own_token);
        }
        own.is_disjoint(cone)
    }
}

/// Does this op block hoisting a load OUT of the loop that contains it?
///
/// [`Op::is_pure`] is the wrong question, and was the one being asked here.
/// `is_pure()` answers *"may this node be freely moved, duplicated and
/// GVN'd"*, so it excludes `Op::Div` / `Op::Rem` — which trap on a zero
/// divisor — and, purely because that allow-list was never extended when the
/// ops were added, also `Op::LCmp` and `Op::FCmp`, which trap on nothing at
/// all. None of the four touches memory, and this predicate now asks
/// [`memory_shape_of`], which says so.
///
/// The cost of conflating the two questions was total rather than marginal.
/// One `idiv`, one `lcmp` or one `dcmpg` anywhere in a loop body made
/// [`loop_has_hard_barrier`] true AND [`loop_writes_memory`] true, which
/// disqualifies the loop from BOTH LICM arms — so
/// `for (int i = 0; i < n; i++) if (this.lo < x[i]) …` on `double` lost every
/// hoist to the compare the loop is written around, and any loop doing integer
/// division lost them to the `idiv`. Extending the `is_pure` allow-list fixed
/// that instance; asking a different question is what stops the next
/// pure-but-unlisted op from reintroducing it silently, which is the whole
/// point of `ir::memory_shape_of` having no `_` arm.
///
/// Trapping stays a SEPARATE question and is not relaxed anywhere by this
/// predicate. It says nothing about moving `Div`/`Rem` themselves — neither
/// arm hoists arithmetic, and [`preheader_may_trap`] still counts a `Div` in
/// the pre-header as a trap. All it says is that these four cannot CLOBBER a
/// read some other node wants hoisted.
///
/// # Why `Op::Guard` is a barrier although it touches no memory
///
/// It is the one member of this predicate that is not a memory fact at all.
/// A guard leaves the compiled body when its condition fails, so a load moved
/// above one runs on a value the guard was going to reject — for a null base,
/// a wild read. `control_token_has_guard` makes the same refusal for a guard
/// at the header, where it can be attributed to a control token; this arm
/// covers a guard anywhere in the body, where it cannot. Both are refusals
/// rather than analyses, for the reason that function gives at length: the IR
/// gives a guard the same control input as the nodes it protects, so
/// "the guard ran first" is positional and no edge expresses it.
///
/// `Op::Load` / `Op::Store` / `Op::Phi` / `Op::Dead` keep their exemptions:
/// in-loop stores are handled with per-load alias precision by
/// `loop_store_clobber` / `load_safe_past_clobber`, loads and φs cannot
/// clobber anything, and a removed node emits nothing.
///
/// # `Op::OsrMemory` is not a barrier (round 11 wave 16, lane irback)
///
/// An opaque OSR arm (`IrBuilder::build_opaque_osr_merge`, only under
/// `CRATONVM_JIT_OSR_OPAQUE_ENTRY`) sits in front of an INNER loop's header,
/// i.e. inside every enclosing loop's body, and its `OsrMemory` is
/// `MemoryShape::Opaque`. Counted as a barrier it refused every load hoist in
/// every loop that contains a nested loop, on the method-entry path, where
/// the arm never runs (`r11w11-irexc-osr-entry-as-a-real-predecessor`,
/// wave-15 blocker 2). It writes nothing: it stands for memory the
/// interpreter already wrote BEFORE the entry. The only path on which it
/// matters is an OSR entry through that arm, and a value hoisted into an
/// enclosing loop's pre-header is not computed on that path; it is used in
/// the enclosing body, so it is live into the arm (the arm reaches that body
/// through the inner loop's exit or the outer back edge, and nothing between
/// redefines the hoisted value), and `ir_lower::emit_osr_entry_stubs` admits an
/// opaque arm only when every value live into it is a constant
/// (`LiveModel::live_in_at`). So the hoist costs that one entry and never
/// produces a wrong value; unhoisted, it cost every run of the method.
pub(in crate::ir_optimize) fn licm_hoist_barrier(op: &Op) -> bool {
    if matches!(
        op,
        Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead | Op::OsrMemory
    ) || op.is_control()
    {
        return false;
    }
    memory_shape_of(op).touches_memory() || matches!(op, Op::Guard { .. })
}

/// True if the loop body contains a *hard* barrier — a `Call`, allocation
/// (`New`/`NewArray`, which run constructor side effects), guard, monitor, or
/// any other node that [`licm_hoist_barrier`] classifies. Such a node may read
/// or write arbitrary memory (or, for a guard, leave the body), so it
/// disqualifies ALL load hoisting for the loop.
///
/// In-loop `Store`s are deliberately NOT hard barriers: they are handled with
/// per-load alias precision by `loop_store_clobber` / `load_safe_past_clobber`
/// (a store to a local allocation distinct from a load's base cannot clobber
/// that load).
///
/// A VOLATILE `Load` / `Store` ([`Node::is_volatile_access`]) IS a hard
/// barrier (round 9 wave 6): a volatile read is an acquire, so no later read
/// may be hoisted above it, and re-reading it every iteration is the whole
/// point of `while (!this.stop) {}`. The op-level predicate cannot see the
/// mark, hence the node-level test here.
pub(in crate::ir_optimize) fn loop_has_hard_barrier(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    body.iter()
        .any(|&id| node_is_hard_barrier(graph, &graph.nodes[id as usize]))
}

/// One node's answer to [`loop_has_hard_barrier`].
pub(in crate::ir_optimize) fn node_is_hard_barrier(graph: &Graph, node: &Node) -> bool {
    (licm_hoist_barrier(&node.op)
        && !is_classified_direct_static(graph, node)
        && !is_unbarred_direct_static_read(graph, node))
        || node.is_volatile_access()
}

/// `CRATONVM_JIT_IR_LICM_STATIC_READ_NOT_BARRIER` (JIT round 20 wave 11,
/// lane pLoop; LOOPOPT20-4 of `docs/feature-designs/jit-ir-optimizations.md`
/// = L5W7-3 of `docs/feature-designs/jit-ir-optimizations.md`):
/// **default ON**, `=0` restores the op-level barrier exactly. Read live, only
/// for a body `getstatic` (no `static`: `process_global_statics_ratchet`).
pub(crate) fn licm_static_read_not_barrier_enabled() -> bool {
    true
}

/// JIT round 20 wave 11 (lane pLoop, LOOPOPT20-4 / L5W7-3): is `node` a
/// DIRECT, non-volatile `getstatic` ([`Graph::direct_static_access`] answers
/// a read) under [`licm_static_read_not_barrier_enabled`]? The read half of
/// [`is_classified_direct_static`], without `Graph::static_alias_class`.
///
/// Such a read is no hard barrier for the field / array load hoist: it is two
/// `MOV`s through the class's baked statics-base cell (the class was
/// initialised when the compile resolved it -- the lowering re-asks the same
/// resolver), so it makes no call, runs no `<clinit>`, publishes no safepoint
/// map, cannot trap and is no synchronisation action; it WRITES nothing, so no
/// load hoisted past it can read a different value. Its op-level
/// `MemAccess::Opaque` is the helper route's, which a direct read never takes.
/// The stores of the loop stay with `loop_store_clobber`, and every other
/// barrier (a helper-routed or volatile static, a `putstatic`, a call, a
/// guard) is unchanged; [`loop_writes_memory`] still counts the read (the
/// restricted arm does no alias reasoning).
pub(in crate::ir_optimize) fn is_unbarred_direct_static_read(graph: &Graph, node: &Node) -> bool {
    matches!(
        node.op,
        Op::LoadStatic {
            is_volatile: false,
            ..
        }
    ) && graph
        .direct_static_access(node)
        .is_some_and(|(_, is_write)| !is_write)
        && licm_static_read_not_barrier_enabled()
}

/// JIT round 19 wave 2 (lane irops2, IROPS19-1): is `node` a direct static
/// access the effect model classifies as a plain read / write of its own
/// [`crate::ir::AliasClass::Static`] cell ([`Graph::direct_static_access`],
/// only under [`Graph::static_alias_class`])?
///
/// Such a node is no LICM barrier for the FIELD / ARRAY load hoists: a
/// class's static cell is not any object's instance or element storage (the
/// premise `Graph::may_alias` records), the direct read and the inline
/// primitive store make no call, run no `<clinit>`, publish no safepoint map
/// and cannot trap, and neither is volatile. The loads those arms hoist
/// therefore read the same value whether the static access runs before or
/// after them. `false` with the switch off, which keeps both predicates
/// byte-identical to before.
pub(in crate::ir_optimize) fn is_classified_direct_static(graph: &Graph, node: &Node) -> bool {
    graph.static_alias_class
        && (graph.direct_static_access(node).is_some()
            // JIT round 19 wave 4 (lane irops3, IROPS2-5): a reference
            // `putstatic` of an initialised class (`Graph::direct_static_ref_store`,
            // only under `Graph::static_ref_store_effect`). It is a helper
            // call, but one that writes only its static cell, runs no Java
            // code, allocates nothing and cannot deopt this frame, so a field /
            // array load hoisted past it reads the value it would have read;
            // the values the hoist keeps live across its safepoint map are
            // named by that map exactly as across the loop's own back-edge
            // poll.
            || graph.direct_static_ref_store(node).is_some())
}

/// MON12-2 (JIT round 19 wave 14): under `CRATONVM_DBG_JITC`, the op a loop
/// lock-coarsening plan refused (`plan` names which: the per-iteration `loop`
/// plan or the `block` plan). The census that decides what to admit next:
/// `grep 'lock coarsening refusing op'` over a battery run. Diagnostic only.
pub(in crate::ir_optimize) fn note_lock_coarsen_refusing_op(plan: &str, id: NodeId, op: &Op) {
    if false {
        eprintln!(
            "[cratonvm-jitc] ir loop lock coarsening refusing op ({plan} plan) n{id}: {op:?}"
        );
    }
}

// The same question asked about WRITES only, plus stores.
//
// [`loop_has_hard_barrier`] disqualifies a loop from every hoist when its
// body holds any impure non-control node outside a small allow-list. Three
// of the nodes it therefore rejects write no memory at all:
//
// * `Op::ArrayLoad` and `Op::ArrayLength` are **reads**. A read cannot
//   clobber the location a hoisted load reads, so it cannot make a hoist
//   value-unsafe. Each is impure only because it raises NPE on a null base
//   — a TRAP-ordering fact, not an aliasing one.
// * `Op::Guard` carries no memory edge at all (`ir_schedule`: "its ordering
//   against a store is positional"). It transfers control to a deopt; it
//   writes nothing.
//
// A fourth group — `Op::Div`, `Op::Rem`, `Op::LCmp`, `Op::FCmp` — was being
// rejected by BOTH this predicate and the strict one, and now falls out of
// [`memory_shape_of`]: none of the four touches memory, so none of them can
// write. That is the whole predicate below — [`MemoryShape::may_write`] —
// rather than a hand-maintained list of the ops that do not, which is what
// the three bullets above had become.
//
// `Op::Store` is NOT exempt here, unlike in the strict predicate: the
// restricted hoist this feeds does no alias analysis, so it wants a body
// that writes nothing whatsoever rather than one whose writes it would have
// to reason about. Everything else — `Op::ArrayStore`, `Op::Call`,
// `Op::New`, `Op::NewArray`, `Op::LoadStatic`, the monitors — stays a
// barrier for the same reason it always was.
//
// # What this is for
//
// The IR String-access expansion (`ir.rs::try_string_access_intrinsic`)
// emits four `Op::Guard`s, two `Op::ArrayLoad`s and one `Op::ArrayLength`
// into the very loop it wants its `String.value` / `String.coder` loads
// hoisted OUT of — so it disqualified its own LICM, and `CRATONVM_DBG=licm`
// reported `hard_barrier=true` with 0 hoisted on every candidate header of
// `probes/CharAtCostCurve.java`. The same shape belongs to any counted loop
// carrying a bounds-checked array read.
//
// (JIT round 21 wave 1, lane loops, review: this block documents
// `loop_writes_memory` below; MON12-2's `note_lock_coarsen_refusing_op` had
// been inserted between the two, so rustdoc attached this text to it. The
// function now sits above the block; no code changed.)
/// Primitive element stores cannot modify any instance field. This narrow
/// admission extends the restricted field-read hoist without admitting field
/// writers, reference stores, calls, allocations, atomics or ordered accesses.
pub(in crate::ir_optimize) fn primitive_array_field_hoist(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    if !crate::ir_vector_loop::avx512_vector_loop_enabled()
    {
        return false;
    }
    body.iter().all(|&id| {
        let n = &graph.nodes[id as usize];
        if n.is_volatile_access() {
            return false;
        }
        if n.op.is_control() || matches!(n.op, Op::Phi | Op::Dead | Op::OsrMemory) {
            return true;
        }
        let effect = graph.memory_effect(id);
        if effect.allocates || effect.order != crate::ir::MemOrder::Plain {
            return false;
        }
        if matches!(n.op,Op::ArrayStore(k) if k!=MemKind::Ref) {
            return true;
        }
        !memory_shape_of(&n.op).may_write()
    })
}

/// `CRATONVM_JIT_IR_LICM_FIELD_DISJOINT`: like [`primitive_array_field_hoist`],
/// but the body may also store to instance fields (plain, non-volatile). The
/// read hoist then keeps only the reads no body write may alias
/// ([`body_write_may_alias`]); calls, allocations and ordered accesses still refuse.
pub(in crate::ir_optimize) fn field_store_disjoint_hoist(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    let typechecks_read_only = licm_typecheck_not_write_enabled();
    let dbg = graph.late_licm && false;
    body.iter().all(|&id| {
        let n = &graph.nodes[id as usize];
        let ok = if n.is_volatile_access() {
            false
        } else if !node_writes_memory(graph, id, typechecks_read_only) {
            true
        } else {
            let effect = graph.memory_effect(id);
            !effect.allocates
                && effect.order == crate::ir::MemOrder::Plain
                && match n.op {
                    Op::Store(_) => !matches!(effect.writes, crate::ir::AliasClass::Any),
                    Op::ArrayStore(k) => k != MemKind::Ref,
                    _ => false,
                }
        };
        if !ok && dbg {
            eprintln!("[DBG_LICM] field-disjoint refused by n{id} {:?}", n.op);
        }
        ok
    })
}

/// Late LICM: is `base` non-null wherever `preheader` runs? [`base_non_null_on_entry`],
/// or a dominating node that leaves on a null `base` first: a `Guard(ExactClassIs(base, C))`,
/// a field store to it, or a used field read of it.
pub(in crate::ir_optimize) fn late_base_non_null(graph: &Graph, base: NodeId, preheader: NodeId) -> bool {
    if base_non_null_on_entry(graph, base) {
        return true;
    }
    graph.users_of(base).into_iter().any(|u| {
        let Some(n) = graph.node_opt(u) else {
            return false;
        };
        let anchored_before = || n.inputs.first().is_some_and(|&c| control_dominates(graph, c, preheader));
        match n.op {
            Op::Store(_) => n.inputs.get(2) == Some(&base) && anchored_before(),
            Op::Load(_) => {
                n.inputs.get(2) == Some(&base) && !graph.users_of(u).is_empty() && anchored_before()
            }
            Op::ExactClassIs { class_id } => class_id != 0 && class_guard_dominates(graph, u, preheader),
            _ => false,
        }
    })
}

/// `ExactClassIs(base, C != 0)` nodes are a null proof only through a guard on them.
pub(in crate::ir_optimize) fn class_guard_dominates(graph: &Graph, test: NodeId, preheader: NodeId) -> bool {
    graph.users_of(test).into_iter().any(|g| {
        graph.node_opt(g).is_some_and(|gn| {
            matches!(gn.op, Op::Guard { .. })
                && gn.inputs.get(1) == Some(&test)
                && control_dominates(graph, gn.inputs[0], preheader)
        })
    })
}

/// Guard at `preheader` that licenses a late-hoisted read of `base` when no
/// earlier proof already guarantees `base != null`, preventing the scheduler
/// from placing the hoisted read before the guard.
pub(in crate::ir_optimize) fn late_base_licensing_guard(
    graph: &Graph,
    base: NodeId,
    preheader: NodeId,
) -> NodeId {
    if base_non_null_on_entry(graph, base) {
        return NO_NODE;
    }
    let non_guard_proves = graph.users_of(base).into_iter().any(|u| {
        let Some(n) = graph.node_opt(u) else {
            return false;
        };
        let anchored_before = || n.inputs.first().is_some_and(|&c| c != preheader && control_dominates(graph, c, preheader));
        match n.op {
            Op::Store(_) => n.inputs.get(2) == Some(&base) && anchored_before(),
            Op::Load(_) => {
                n.inputs.get(2) == Some(&base) && !graph.users_of(u).is_empty() && anchored_before()
            }
            _ => false,
        }
    });
    if non_guard_proves {
        return NO_NODE;
    }
    for u in graph.users_of(base) {
        let Some(n) = graph.node_opt(u) else { continue; };
        if let Op::ExactClassIs { class_id } = n.op {
            if class_id == 0 { continue; }
            for g in graph.users_of(u) {
                if graph.node_opt(g).is_some_and(|gn| {
                    matches!(gn.op, Op::Guard { .. })
                        && gn.inputs.get(1) == Some(&u)
                        && gn.inputs.first() == Some(&preheader)
                }) {
                    return g;
                }
            }
        }
    }
    NO_NODE
}

/// Late LICM: replace `load` by a copy anchored at `preheader` reading `mem`.
/// The copy keeps the pc and the id-keyed field facts; the load's memory-token
/// users read its own memory input instead (a read writes nothing), every other
/// use (snapshots included) the copy.
pub(in crate::ir_optimize) fn late_hoist_copy(graph: &mut Graph, load: NodeId, preheader: NodeId, mem: NodeId) -> NodeId {
    late_hoist_copy_guarded(graph, load, preheader, mem, NO_NODE)
}

pub(in crate::ir_optimize) fn late_hoist_copy_guarded(
    graph: &mut Graph,
    load: NodeId,
    preheader: NodeId,
    mem: NodeId,
    licensing_guard: NodeId,
) -> NodeId {
    let (op, ty, pc, mut inputs) = {
        let n = &graph.nodes[load as usize];
        (n.op.clone(), n.ty, n.bytecode_pc, n.inputs.as_slice().to_vec())
    };
    let old_mem = inputs[1];
    inputs[0] = preheader;
    inputs[1] = mem;
    if let Some(slot) = crate::ir::guard_token_slot_for(&op, inputs.len()) {
        let old_guard = inputs[slot];
        let dominates_preheader = graph
            .node_opt(old_guard)
            .and_then(|gn| gn.inputs.first().copied())
            .is_some_and(|gc| control_dominates(graph, gc, preheader));
        if !dominates_preheader {
            inputs.truncate(slot);
        }
    }
    if licensing_guard != NO_NODE {
        if let Some(shape) = op.guard_shape() {
            if inputs.len() >= shape.unguarded_arity {
                inputs.truncate(shape.unguarded_arity);
                inputs.push(licensing_guard);
            }
        }
    }
    let copy = graph.add(op, ty, inputs, pc);
    if let Some(&d) = graph.field_decl_class.get(&load) {
        graph.field_decl_class.insert(copy, d);
    }
    if let Some(&r) = graph.field_rows.get(&load) {
        graph.field_rows.insert(copy, r);
    }
    if let Some(&p) = graph.field_row_pcs.get(&load) {
        graph.field_row_pcs.insert(copy, p);
    }
    if graph.tl_trusted_loads.contains(&load) {
        graph.tl_trusted_loads.insert(copy);
    }
    if graph.ffm_final_field_reads.contains(&load) {
        graph.ffm_final_field_reads.insert(copy);
    }
    for user in graph.users_of(load) {
        let slots: Vec<usize> = graph.nodes[user as usize]
            .inputs
            .as_slice()
            .iter()
            .enumerate()
            .filter(|&(i, &x)| x == load && crate::ir::is_memory_token_slot(&graph.nodes[user as usize], i))
            .map(|(i, _)| i)
            .collect();
        for slot in slots {
            graph.set_input(user, slot, old_mem);
        }
    }
    graph.replace_all_uses(load, copy);
    graph.kill(load);
    copy
}

/// May any writer of `body` (as [`loop_writes_memory`] counts them) touch the cell `load` reads?
pub(in crate::ir_optimize) fn body_write_may_alias(graph: &Graph, body: &FxHashSet<NodeId>, load: NodeId) -> bool {
    let reads = graph.memory_effect(load).reads;
    let typechecks_read_only = licm_typecheck_not_write_enabled();
    body.iter().any(|&id| {
        node_writes_memory(graph, id, typechecks_read_only)
            && graph.may_alias(graph.memory_effect(id).writes, reads)
    })
}

pub(in crate::ir_optimize) fn loop_writes_memory(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    let typechecks_read_only = licm_typecheck_not_write_enabled();
    body.iter().any(|&id| node_writes_memory(graph, id, typechecks_read_only))
}

pub(in crate::ir_optimize) fn node_writes_memory(graph: &Graph, id: NodeId, typechecks_read_only: bool) -> bool {
    {
        // A VOLATILE access counts as a write here although a volatile READ
        // writes nothing: the restricted read hoist this feeds reasons only
        // about clobbering, and an acquire forbids the hoist for an ordering
        // reason that argument does not cover (round 9 wave 6). See
        // [`loop_has_hard_barrier`].
        if graph.nodes[id as usize].is_volatile_access() {
            return true;
        }
        let op = &graph.nodes[id as usize].op;
        // `Op::Phi` and `Op::Dead` keep their explicit exemptions: a
        // `Memory`-typed φ is a token carrier that writes nothing itself (the
        // writes it merges are their own nodes, and they are in this body if
        // they are anywhere), and a removed node emits nothing. `ir::`'s
        // op-level classifier cannot see either fact from an `&Op` and
        // answers conservatively, which is the right default there and the
        // wrong answer here.
        // `Op::OsrMemory` (round 11 wave 16): an opaque OSR arm's incoming
        // memory writes nothing in the body; see `licm_hoist_barrier`.
        //
        // Round nio20 wave 4 (lane L1, W3L4-1): a scoped RAW READ
        // (`ScopedMemoryAccess.get*`, `ir::scoped_raw_effect`) writes nothing
        // either, which the op-level table (it calls every `Op::Unbox`
        // `Opaque`) cannot see; under `Graph::scoped_raw_alias` the graph-
        // aware effect answers. It stays a HARD barrier
        // (`licm_hoist_barrier`), so only the restricted read hoist above,
        // which asks exactly "does the body write", is widened by it.
        !matches!(op, Op::Phi | Op::Dead | Op::OsrMemory)
            && !op.is_control()
            && memory_shape_of(op).may_write()
            // JIT round 19 wave 2 (lane irops2, IROPS19-1): a direct static
            // read writes nothing, and a direct primitive static store writes
            // only a static cell, which no field / array load reads.
            && !is_classified_direct_static(graph, &graph.nodes[id as usize])
            && !(matches!(op, Op::Unbox { .. })
                && graph.scoped_raw_alias
                && crate::ir::scoped_raw_effect(&graph.nodes[id as usize])
                    .is_some_and(|e| !e.is_write()))
            // JIT round 23 wave 2d (lane L1, TCL23-2;
            // `CRATONVM_JIT_IR_LICM_TYPECHECK_NOT_WRITE`): a type check writes
            // no Java-visible memory (see the switch).
            && !(typechecks_read_only && matches!(op, Op::CheckCast { .. } | Op::InstanceOf { .. }))
    }
}

/// `CRATONVM_JIT_NO_LICM_READ_HOIST=1` — turn off the restricted invariant-
/// load hoist for a loop that only its own reads and guards disqualify.
///
/// Default ON. The B arm of an in-binary A/B: with this set,
/// `probes/CharAtCostCurve.java` with the String-intrinsic pin off returns to
/// re-reading `String.value` and `String.coder` per character, and every
/// other arm is unchanged.
/// `CRATONVM_JIT_IR_LICM_TYPECHECK_NOT_WRITE` (JIT round 23 wave 2d, lane L1,
/// TCL23-2; perf, **default ON**, `=0` kill): [`loop_writes_memory`] does not
/// count an `Op::CheckCast` or `Op::InstanceOf` as a write, so the restricted
/// read hoist (and the null-base load predicates) serve a loop whose body
/// type-tests or casts. Their op-level `MemAccess::Opaque` is the helper's
/// route; that helper (`jit_checkcast` / `jit_instanceof`) runs no Java code
/// for the compile-time-resolved targets this tier admits, writes nothing
/// Java-visible (the site's seen-class ways and the receiver's display are
/// VM-private), and may collect (a `Class` mirror), which a value hoisted out
/// of the loop survives like any value live across the back-edge poll. A
/// `CheckCast` that THROWS leaves the loop; a read hoisted above it read a
/// value nothing wrote. They stay hard barriers ([`licm_hoist_barrier`]) and
/// scheduling barriers; only the "does the body write" answer changes.
/// Read once per question, no static.
pub(in crate::ir_optimize) fn licm_typecheck_not_write_enabled() -> bool {
    true
}

pub(in crate::ir_optimize) fn licm_read_hoist_enabled() -> bool {
    !false
}

/// Points-to summary of a reference node, used by the LICM alias oracle to
/// reason about loads/stores whose base flows through a `Phi` (cross-merge).
#[derive(Clone)]
pub(in crate::ir_optimize) struct RefPointsTo {
    /// The fresh local allocations this reference may be, or `None` if it may be
    /// a value of unknown provenance (a loaded ref, a call result, …) — which
    /// could alias anything.
    pub(in crate::ir_optimize) allocs: Option<FxHashSet<NodeId>>,
    /// Whether it may be a *pre-existing* reference (a method parameter /
    /// `this`) — never a fresh in-method allocation.
    pub(in crate::ir_optimize) has_pre: bool,
}

/// Resolve what a reference node points to, following `Phi`s transparently (the
/// "cross-merge" join). Leaves:
///   * `New`/`NewArray` → exactly that fresh allocation.
///   * `Param`           → a pre-existing reference (no allocation).
///   * anything else      → unknown provenance.
///
/// A `Phi` joins its value inputs (the control anchor at its head is skipped):
/// the allocation sets union, `has_pre` ORs, and any unknown input poisons the
/// alloc set to `None` (and ends the walk). Recursion is depth-bounded, so a
/// loop-carried phi cycle resolves to unknown rather than hanging.
pub(in crate::ir_optimize) fn resolve_ref_points_to(graph: &Graph, id: NodeId, depth: u32) -> RefPointsTo {
    PointsToWalk::memoised().resolve(graph, id, depth)
}

/// One points-to query (j27 wave 5, lane CSEHUNT).
///
/// The memo is keyed by `(node, depth)`, not by node. The walk is a pure
/// function of `(graph, node, depth)` (the graph is borrowed shared for the
/// whole query), so a memo on its full argument list returns exactly what the
/// uncached walk returns, for every caller. A memo keyed by node alone would
/// not: the depth cap makes the answer depend on the depth (a node 30 phis
/// tall is known one level down and cut off three levels down), so reusing a
/// shallow answer at a deeper visit would turn some unknown roots known.
/// Each `(node, depth)` is computed once and the depth is at most 33, so a
/// query makes at most `1 + 34 * (phi value inputs)` calls instead of about
/// `1.6^32` on a shared-input phi DAG. No switch: the answer is unchanged.
pub(in crate::ir_optimize) struct PointsToWalk {
    /// `None` only in tests, as the reference the memo is checked against.
    pub(in crate::ir_optimize) memo: Option<FxHashMap<(NodeId, u32), RefPointsTo>>,
    /// Calls to [`PointsToWalk::resolve`], memo hits included (read by the
    /// tests' call-count bound).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::ir_optimize) calls: u64,
}

impl PointsToWalk {
    pub(in crate::ir_optimize) fn memoised() -> Self {
        PointsToWalk {
            memo: Some(FxHashMap::default()),
            calls: 0,
        }
    }

    #[cfg(test)]
    pub(in crate::ir_optimize) fn uncached() -> Self {
        PointsToWalk {
            memo: None,
            calls: 0,
        }
    }

    pub(in crate::ir_optimize) fn resolve(&mut self, graph: &Graph, id: NodeId, depth: u32) -> RefPointsTo {
        self.calls += 1;
        if let Some(hit) = self.memo.as_ref().and_then(|m| m.get(&(id, depth))) {
            return hit.clone();
        }
        let pt = resolve_ref_points_to_step(self, graph, id, depth);
        if let Some(m) = self.memo.as_mut() {
            m.insert((id, depth), pt.clone());
        }
        pt
    }
}

/// The walk itself; recursion goes back through [`PointsToWalk::resolve`].
pub(in crate::ir_optimize) fn resolve_ref_points_to_step(
    walk: &mut PointsToWalk,
    graph: &Graph,
    id: NodeId,
    depth: u32,
) -> RefPointsTo {
    let unknown = RefPointsTo {
        allocs: None,
        has_pre: false,
    };
    if !graph.is_valid_id(id) || depth > 32 {
        return unknown;
    }
    match &graph.nodes[id as usize].op {
        // MX16-3: a `multianewarray`'s OUTER array is one fresh object too.
        // Its rows are reached through `aaload`s, which stay unknown.
        Op::New { .. } | Op::NewArray { .. } | Op::MultiNewArray { .. } => {
            let mut s = FxHashSet::default();
            s.insert(id);
            RefPointsTo {
                allocs: Some(s),
                has_pre: false,
            }
        }
        Op::Param(_) => RefPointsTo {
            allocs: Some(FxHashSet::default()),
            has_pre: true,
        },
        Op::Phi => {
            let mut allocs: FxHashSet<NodeId> = FxHashSet::default();
            let mut has_pre = false;
            let mut saw_value = false;
            // inputs are [control_anchor, value0, value1, …]; skip control.
            for &inp in &graph.nodes[id as usize].inputs {
                if !graph.is_valid_id(inp) {
                    continue;
                }
                if graph.nodes[inp as usize].op.is_control() {
                    continue; // the phi's region/merge anchor, not a value
                }
                saw_value = true;
                let pt = walk.resolve(graph, inp, depth + 1);
                // j27 wave 5 (lane OPTHUNT): any unknown input poisons the
                // join, so stop at once. Every caller reads `has_pre` only
                // beside `Some` allocs, so the answer is unchanged; what
                // changes is the cost. Without the stop, a phi cycle in which
                // each phi has two phi inputs (refs swapped under a branch in
                // a loop) walked a full binary tree to the depth cap,
                // about 2^33 calls for one load or store base.
                let Some(b) = pt.allocs else {
                    return unknown;
                };
                has_pre |= pt.has_pre;
                allocs.extend(b);
            }
            if !saw_value {
                return unknown;
            }
            RefPointsTo {
                allocs: Some(allocs),
                has_pre,
            }
        }
        _ => unknown,
    }
}

/// The set of local allocations the in-loop stores may write, or `None` if any
/// store has an **opaque** base — one that may write a pre-existing object or a
/// value of unknown provenance, which could alias anything. When `None`, no load
/// may be hoisted past the loop's stores.
///
/// A store is *tame* only if its base resolves (cross-merge) to a definite set
/// of local allocations and nothing pre-existing. The store base category is NOT
/// widened to `Param`: two parameters can be the same object (`foo(x, x)`), so a
/// store through one is not provably non-aliasing.
pub(in crate::ir_optimize) fn loop_store_clobber(graph: &Graph, body: &FxHashSet<NodeId>) -> Option<FxHashSet<NodeId>> {
    let mut clobbered = FxHashSet::default();
    for &id in body {
        if !matches!(graph.nodes[id as usize].op, Op::Store(_)) {
            continue;
        }
        let sbase = match store_operands(graph, id) {
            Some((b, _, _)) => b,
            None => return None, // unreadable layout — opaque
        };
        let pt = resolve_ref_points_to(graph, sbase, 0);
        match pt.allocs {
            Some(set) if !pt.has_pre => clobbered.extend(set),
            _ => return None, // may write a pre-existing or unknown object
        }
    }
    Some(clobbered)
}

/// True if a load with base `load_base` cannot read any allocation in
/// `clobbered`, so it may be hoisted past the loop's (tame) stores.
///
/// The load's pre-existing (parameter) component is always safe — no store to a
/// fresh local allocation can clobber a pre-existing object (object identity is
/// fixed at allocation, even under escape) — so only its possible-allocation set
/// must be disjoint from `clobbered`. An unknown-provenance base is never safe.
pub(in crate::ir_optimize) fn load_safe_past_clobber(graph: &Graph, load_base: NodeId, clobbered: &FxHashSet<NodeId>) -> bool {
    match resolve_ref_points_to(graph, load_base, 0).allocs {
        Some(set) => set.is_disjoint(clobbered),
        None => false,
    }
}

/// Whether control node `ctrl` executes on every iteration of the loop headed
/// by `region`: it IS the header, or it is a projection of the header's own
/// exit test (the `If` anchored directly at the header), reached through at
/// most a trivial single-input `Merge`.
///
/// Anything else — a projection of any other `If` in the body (a condition, a
/// `break`), a real merge — is refused: the node may be skipped on some
/// iteration, including the first. Deliberately a syntactic walk rather than a
/// dominance query; it answers `false` whenever it is unsure, and a `false`
/// only costs a hoist.
pub(in crate::ir_optimize) fn anchored_on_every_iteration(
    graph: &Graph,
    ctrl: NodeId,
    region: NodeId,
    body: &FxHashSet<NodeId>,
) -> bool {
    let mut c = ctrl;
    for _ in 0..4 {
        if c == region {
            return true;
        }
        let Some(node) = graph.nodes.get(c as usize) else {
            return false;
        };
        match node.op {
            Op::Proj(_) => {
                let Some(&if_id) = node.inputs.first() else {
                    return false;
                };
                let Some(if_node) = graph.nodes.get(if_id as usize) else {
                    return false;
                };
                if !matches!(if_node.op, Op::If) || if_node.inputs.first().copied() != Some(region)
                {
                    return false;
                }
                // The other edge must leave the loop: if both stay in it, this
                // projection is one arm of an in-body branch and may be skipped.
                if body.iter().any(|&b| {
                    b != c
                        && graph.nodes.get(b as usize).is_some_and(|x| {
                            matches!(x.op, Op::Proj(_)) && x.inputs.first() == Some(&if_id)
                        })
                }) {
                    return false;
                }
                c = region;
            }
            Op::Merge if node.inputs.len() == 1 => c = node.inputs[0],
            _ => return false,
        }
    }
    false
}

/// What one LICM visit did, as two facts rather than one.
///
/// The distinction exists because [`optimize`]'s outer round needs a
/// CONVERGENCE signal and this pass has two different things to report:
///
/// * `moved` — an input edge was rewritten. A hoist is monotone (a load
///   re-anchored to the pre-header with the loop's entry memory is skipped by
///   the `continue` on the next visit), so a visit that reports `false` here
///   will report `false` forever and the outer round may stop.
/// * `recognized` — an invariant load in the COMPACT EA-bridge form
///   (`[base]` / `[base, index]`) was seen. There is no control or memory slot
///   on those, so nothing moved and nothing can: the hoist is invariance alone,
///   which scheduling already honours. The trailing GVN still wants to run once
///   so the duplicate reads dedup, but the answer is the same on every visit,
///   which is exactly why it must not be fed to a fixpoint.
///
/// Reporting both through one `bool` is what pinned [`optimize`]'s loop at a
/// two-round BUDGET: `licm()` answered "changed" for a compact-form load every
/// time it saw one, so a loop driven on that answer would never terminate.
pub(in crate::ir_optimize) struct LicmOutcome {
    pub(in crate::ir_optimize) moved: bool,
    pub(in crate::ir_optimize) recognized: bool,
}

impl LicmOutcome {
    /// Did this visit do anything the trailing GVN/DCE cleanup should see?
    pub(in crate::ir_optimize) fn any_work(&self) -> bool {
        self.moved || self.recognized
    }
}

/// Is `graph.safepoints[si]` (at `bci`) consulted by `node` alone?
///
/// `true` only when it is the one snapshot at `bci`, no other node names it,
/// and no other live node deopts at `bci` or carries a pc that resumes there —
/// a pure or control node counts too, because `ir_lower`'s `bci_native`
/// anchors the snapshot's deopt-table point at the earliest node of that bci.
/// A pc the splice map cannot place counts as resuming there. Anything unsure
/// answers `false`, which only keeps the copy-and-name path.
pub(in crate::ir_optimize) fn snapshot_serves_only(
    graph: &Graph,
    node: NodeId,
    si: usize,
    bci: usize,
    splices: Option<&crate::ir::IrSpliceResumeMap>,
) -> bool {
    if graph.safepoints.iter().filter(|s| s.bci == bci).count() != 1 {
        return false;
    }
    let resumes_at = |p: usize| -> bool {
        if p == bci {
            return true;
        }
        match splices {
            Some(map) => map.resume_bci(p).is_none_or(|r| r == bci),
            None => false,
        }
    };
    graph.nodes.iter().enumerate().all(|(id, m)| {
        if id as NodeId == node || m.op == Op::Dead {
            return true;
        }
        if m.frame_snapshot == Some(si as u32) {
            return false;
        }
        if let Op::Guard { bci: g } = m.op {
            if resumes_at(g) {
                return false;
            }
        }
        !m.bytecode_pc.is_some_and(resumes_at)
    })
}

/// A hoisted read whose base is non-null on entry cannot fail at the pre-header on the
/// null path, so the hoist never required a first-iteration frame. It can still be a
/// deopt or exception site the lowering emits (a compactness guard, the `jit_getfield`
/// helper's exception route), and left unnamed it resolves the in-loop snapshot, which
/// names the header phis: the by-bci table point it anchors in the pre-header then
/// describes values nothing has computed (`ir_lower::undominated_anchor`, a refused
/// compile). So the frame is installed when it can be described, and the hoist goes
/// ahead either way (`install_preheader_deopt_frame` changes nothing when it says no).
pub(in crate::ir_optimize) fn name_preheader_frame_best_effort(
    graph: &mut Graph,
    node: NodeId,
    region: NodeId,
    entry_pred: NodeId,
    body: &FxHashSet<NodeId>,
    body_runs: bool,
) {
    let _ = install_preheader_deopt_frame(graph, node, region, entry_pred, body, body_runs);
}

/// May `node` — anchored at loop header `region` and about to move to the
/// pre-header, where it can still deopt (its base may be null) — move, and if
/// so, which frame does its deopt rebuild? Round 11 wave 2
/// (`r11-iropt-licm-preheader-deopt-reads-unwritten-phi-slots`).
///
/// The hoist keeps the node's own `bytecode_pc`, so its deopt resumes the
/// interpreter at that in-loop bci with the snapshot recorded there. That
/// snapshot names the header's φs (the induction variable, an accumulator), and
/// a φ's home is written by the phi copies at the END of the pre-header, after
/// the hoisted node: for a loop entered a second time (an inner loop) the frame
/// carried the previous run's final values, which an in-method `catch` reads.
///
/// What this does instead:
///
/// * The frame is a copy of that snapshot with each φ of `region` replaced by
///   its pre-header value — exactly the state at that bci on the first
///   iteration — installed as the node's own ([`Graph::set_node_frame_snapshot`];
///   `ir_lower::resolve_frame_state_for_site` and the scheduler's anchor check
///   both honour it). A slot naming any other value computed inside the loop
///   cannot be described at the pre-header: refuse.
/// * Resuming at that bci skips whatever the first iteration runs before it.
///   So a non-pure node that runs earlier (a store, a call, a guard, another
///   trapping read) refuses the hoist. A bytecode whose IR nodes share the
///   node's own bci is not skipped: the interpreter re-executes all of it.
/// * A node anchored at the header is preceded by the header nodes at an
///   earlier bci.
/// * A node anchored in the BODY (round 11 wave 3, lane mid) is admitted only
///   when `body_runs` (the caller's `loop_body_definitely_runs`) and its control
///   is the header exit test's stay projection, possibly behind trivial
///   `Merge`s. `analyze_counted_loop` only answers for a loop whose back edge
///   is that projection, so the body is straight-line code after the test, and
///   on the first iteration the test falls into it. It is then preceded by
///   every non-pure node the header anchors, every one anchored on the spine
///   between the test and the node, and those anchored beside it at an earlier
///   bci. With none of those, the first iteration reaches the node having
///   changed nothing but the values the frame already names, so a resume at
///   its bci with the header φs at their entry values IS the first iteration.
///   A pre-header that can itself trap is refused as well, as the other arms
///   do.
///
/// "Earlier" is decided by bci, which is execution order inside one block of
/// the compiling method. A node of a spliced callee body carries a relocated
/// pc that sorts after every caller bci; it is placed at its outermost invoke
/// through the build's splice resume map, and a node that cannot be placed at
/// all (no pc, or a pc the map cannot resolve) counts as earlier.
///
/// `true` means "hoist": no frame was needed, or one was installed. A graph
/// with no snapshots (a hand-built fixture) has no frame to get wrong. A
/// header-anchored node with no pc keeps the by-bci frame. A header-anchored
/// node of a spliced callee body (a relocated pc with no snapshot) deopts at
/// its enclosing invoke, so since round 11 wave 5 its frame is the one at
/// that RESUME bci, corrected the same way; a body-anchored one is refused.
///
/// Where the frame goes (wave 5): when nothing but this node consults the
/// recorded snapshot (`snapshot_serves_only`), it is corrected in place, so
/// no second snapshot at the same bci exists. Otherwise a corrected copy is
/// pushed and named by the node. For a resume bci that relies on
/// `Graph::set_node_frame_snapshot` reading the splice map
/// (`r11w5-mid-ircore-resume-bci-node-frames-patch-FIXED-20260924.md`, applied
/// in wave 5). A pc the map cannot place gets no frame here, and the lowerer
/// then refuses the compile for want of one at its resume bci.
pub(in crate::ir_optimize) fn install_preheader_deopt_frame(
    graph: &mut Graph,
    node: NodeId,
    region: NodeId,
    entry_pred: NodeId,
    body: &FxHashSet<NodeId>,
    body_runs: bool,
) -> bool {
    if graph.safepoints.is_empty() {
        return true;
    }
    let Some(n) = graph.node_opt(node) else {
        return false;
    };
    let Some(&ctrl) = n.inputs.first() else {
        return false;
    };
    // `spine`: the controls between the header test and the node, the node's
    // own control first. Empty for a header-anchored node.
    let mut spine: Vec<NodeId> = Vec::new();
    if ctrl != region {
        if !body_runs {
            return false;
        }
        let mut c = ctrl;
        let mut reached_test = false;
        for _ in 0..4 {
            let Some(cn) = graph.node_opt(c) else {
                return false;
            };
            // The stay edge, never the exit: `loop_body` holds only controls
            // on a path back to the header.
            if !body.contains(&c) {
                return false;
            }
            spine.push(c);
            match cn.op {
                Op::Proj(_) => {
                    let test_at_header = cn
                        .inputs
                        .first()
                        .and_then(|&t| graph.node_opt(t))
                        .is_some_and(|t| t.op == Op::If && t.inputs.first() == Some(&region));
                    if !test_at_header {
                        return false;
                    }
                    reached_test = true;
                    break;
                }
                Op::Merge if cn.inputs.len() == 1 => c = cn.inputs[0],
                _ => return false,
            }
        }
        if !reached_test || preheader_may_trap(graph, entry_pred) {
            return false;
        }
    }
    let body_anchored = !spine.is_empty();
    let Some(pc) = n.bytecode_pc else {
        return !body_anchored;
    };
    // Frames of the compiling method only: a chain snapshot recorded inside a
    // spliced body (round 13 wave 3, `ir::SpliceScopeChain`) is not a by-bci
    // frame, and counting it would move `resume_pc` and the `earlier` order
    // below off the rule `ir_lower::resume_bci` applies.
    let snapshot_bcis: FxHashSet<usize> = graph
        .safepoints
        .iter()
        .filter(|s| !s.is_splice_state())
        .map(|s| s.bci)
        .collect();
    // Round 13 wave 3 (lane framestate): does a chain snapshot describe this
    // node's own pc? Then `ir_lower` would resolve an UNNAMED node's deopt
    // from it, and that frame is the node's state at its place in the loop,
    // not at the pre-header. The hoist must leave the node naming a frame
    // corrected here (the flat one), or not happen.
    let has_chain_frame = graph
        .safepoints
        .iter()
        .any(|s| s.is_guard_splice_state() && s.bci == pc);
    // W5-3 stage 3: when the graph relies on its chains
    // (`Graph::splice_chains_required`) the corrected FLAT frame below would
    // re-run what the spliced body committed, so a node a chain describes
    // stays where it is.
    if has_chain_frame && graph.splice_chains_required {
        return false;
    }
    let splices = crate::ir::splice_resume_map_this_build();
    // The bci this node's deopt resumes at: its own, or, for a node of a
    // spliced callee body (a relocated pc with no snapshot), the enclosing
    // invoke's — `ir_lower::resume_bci`, which then resolves the frame there.
    // That caller frame names the header φs exactly like an in-loop bci's
    // does, so it is corrected the same way (round 11 wave 5).
    let resume_pc = if snapshot_bcis.contains(&pc) {
        pc
    } else if body_anchored {
        return false;
    } else {
        match splices.as_ref().and_then(|m| m.resume_bci(pc)) {
            Some(rb) if snapshot_bcis.contains(&rb) => rb,
            // No frame to correct; the lowerer refuses a deopt with no frame
            // state at its resume bci on its own -- unless a chain snapshot
            // would answer it uncorrected (see `has_chain_frame`).
            _ => return !has_chain_frame,
        }
    };
    let own = n.frame_snapshot.filter(|&si| {
        graph
            .safepoints
            .get(si as usize)
            .is_some_and(|s| s.bci == resume_pc)
    });
    // Does `m` run before the bytecode at `pc` in its block?
    let earlier = |m: &Node| -> bool {
        let Some(mp) = m.bytecode_pc else {
            return true;
        };
        let at = if snapshot_bcis.contains(&mp) {
            Some(mp)
        } else {
            match &splices {
                // Nothing was spliced: every pc is the method's own.
                None => Some(mp),
                Some(map) => map.resume_bci(mp),
            }
        };
        // Against the RESUME bci: a resume at the enclosing invoke re-runs
        // every node that resolves to it, the node's own callee body included.
        match at {
            Some(at) => at < resume_pc,
            None => true,
        }
    };
    // Anything the first iteration runs before this bci.
    let skips_work = graph.nodes.iter().enumerate().any(|(id, m)| {
        if id as NodeId == node
            || m.op == Op::Dead
            || m.op == Op::Phi
            || m.op.is_pure()
            || m.op.is_control()
        {
            return false;
        }
        let Some(&anchor) = m.inputs.first() else {
            return false;
        };
        if anchor == region {
            // The whole header block precedes a body node.
            body_anchored || earlier(m)
        } else if anchor == ctrl {
            body_anchored && earlier(m)
        } else {
            spine.contains(&anchor)
        }
    });
    if skips_work {
        return false;
    }
    let source = match own {
        Some(si) => si as usize,
        None => match graph
            .safepoints
            .iter()
            .position(|s| s.bci == resume_pc && !s.is_splice_state())
        {
            Some(si) => si,
            None => return !body_anchored && !has_chain_frame,
        },
    };
    let Some(entry_slot) = graph
        .node_opt(region)
        .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
        .map(|k| k + 1)
    else {
        return false;
    };
    let Some(mut frame) = graph.safepoints.get(source).cloned() else {
        return false;
    };
    let mut substituted = false;
    for slot in frame
        .locals
        .iter_mut()
        .chain(frame.stack.iter_mut())
        .chain(frame.monitors.iter_mut())
    {
        let v = *slot;
        if v == NO_NODE {
            continue;
        }
        let Some(vn) = graph.node_opt(v) else {
            return false;
        };
        // JIT r27 LOOP27-2: `i2l` of a narrowed `long` induction.
        if let Some(wide) = wide_entry_value(graph, v, region) {
            *slot = wide;
            substituted = true;
            continue;
        }
        if vn.op == Op::Phi && vn.inputs.first().copied() == Some(region) {
            *slot = vn.inputs.get(entry_slot).copied().unwrap_or(NO_NODE);
            substituted = true;
            continue;
        }
        if body.contains(&v) || !is_loop_invariant(graph, v, region, body) {
            return false;
        }
    }
    if !substituted {
        // The recorded frame names nothing the loop defines: it is already
        // the pre-header state. A node a chain snapshot also describes is
        // bound to it explicitly, so the lowerer takes this frame rather than
        // the chain one (round 13 wave 3; see `has_chain_frame`).
        if has_chain_frame && own.is_none() {
            return graph.set_node_frame_snapshot(node, source as u32);
        }
        return true;
    }
    if own.is_none() && snapshot_serves_only(graph, node, source, resume_pc, splices.as_ref()) {
        // Nothing but this node resolves the recorded frame (and the one
        // deopt-table point anchored at its bci sits at this node, now in the
        // pre-header), so correct it IN PLACE rather than add a second
        // snapshot at the same bci: an unnamed duplicate is exactly what
        // `ir_verify::check_frame_states` rejects, and `build_deopt_points`
        // keeps the first of two points at one offset — the uncorrected one
        // (round 11 wave 5). Every slot exists: `frame` is a clone of it.
        let Some(orig) = graph.safepoints.get(source).cloned() else {
            return false;
        };
        let rewrites = [
            (SafepointSlotKind::Local, &orig.locals, &frame.locals),
            (SafepointSlotKind::Stack, &orig.stack, &frame.stack),
            (SafepointSlotKind::Monitor, &orig.monitors, &frame.monitors),
        ];
        for (kind, old, new) in rewrites {
            for (k, (&o, &v)) in old.iter().zip(new.iter()).enumerate() {
                if o != v {
                    let _ = graph.set_safepoint_slot(source, kind, k, v);
                }
            }
        }
        // Name it (its own bci, or the resume bci the splice map gives).
        // Unnamed, the lowerer's by-bci resolution finds the same, now
        // corrected, frame.
        let _ = graph.set_node_frame_snapshot(node, source as u32);
        return true;
    }
    if resume_pc != pc {
        // `Graph::set_node_frame_snapshot` accepts a frame at the resume bci
        // of a relocated pc only when the splice map resolves that pc there.
        // Probe with the recorded frame — the one the lowerer resolves anyway —
        // before adding a snapshot nothing could name: without it the hoist
        // would deopt with the header φ homes unwritten, so it is refused.
        if !graph.set_node_frame_snapshot(node, source as u32) {
            return false;
        }
    }
    graph.push_safepoint(frame);
    let si = (graph.safepoints.len() - 1) as u32;
    graph.set_node_frame_snapshot(node, si)
}

// ── Range-check predication (round 11 wave 7, lane mid) ──────────────
//
// `for (int i = 0; i < n; i++) … a[i] …` with `n` not `a.length` keeps one
// bounds check per iteration: `ir_check_elim` can prove `i < a.length` only
// from a dominating fact `n <= a.length`, and the loop has none. This pass
// plants one, the way C2's loop predication does: in the pre-header,
//
//     len = ArrayLength[entry_pred, entry_mem, a]
//     Guard{header_bci}[entry_pred, Cmp(Le)[n, len]]
//
// and `ir_check_elim::upper_bounds` (wave 6) reads the guard as `n <=
// a.length` for every block it strictly dominates, so the body's check goes.
//
// # Why it is sound
//
// * **The deopt is exact.** Both new nodes carry the header's bci and name a
//   COPY of the header snapshot with every header φ replaced by its pre-header
//   value (and every other slot a value defined outside the loop, or the
//   predicate is not planted). A resume there re-runs the loop from its first
//   test with exactly the state the compiled code had on entry, whatever the
//   loop then does: the interpreter checks every access itself. So a failing
//   guard (or a null `a` at the `ArrayLength`, which deopts rather than
//   throwing) costs a deopt, never a wrong result.
// * **It runs after everything the pre-header does.** The new nodes are the
//   newest in the graph and nothing names them but each other, so the
//   scheduler (id-order placement, a post-order topological sort from the
//   roots in that order, and a priority pass that keeps every impure node's
//   relative order) emits them after every node already in the pre-header
//   block; and `len` reads the loop's ENTRY memory, which orders it after every
//   store, call and monitor before the loop. A resume at the header therefore
//   skips nothing the pre-header had still to do.
// * **The fact is true where it is read.** `ir_check_elim` still proves
//   `i >= 0` and the unit-stride / back-edge argument itself (for a down
//   count: the decreasing-φ rule and the header test's `i >= 0` edge); the
//   guard only supplies `n <= a.length` (or the strict `x < a.length`), which
//   holds after it because it deopts otherwise.
// * **No OSR entry skips it — by a rule that lives elsewhere.** An ordinary
//   optimizing OSR entry stub (`ir_lower::emit_osr_entry_stubs`) jumps to the
//   loop HEADER, past this pre-header, and the fact would then license the
//   elided check on a bound nobody tested (`n > a.length` runs off the array
//   instead of throwing). Today the optimizing OSR door enters no body holding
//   an `Op::Guard` at all (`ir_osr_sentinel_free`, and
//   `osr_exit::guard_exit_fact` counts every `Op::Guard` as speculative), and
//   an opaque entry (`CRATONVM_JIT_OSR_OPAQUE_ENTRY`) passes through the
//   pre-header merge this guard is anchored at. Round 12 wave 2 page
//   `r12w2-iropt-osr-entry-skips-range-predicate`: relaxing either rule
//   needs the lowerer to refuse a plain entry at such a header first.
//
// # Why it is gated on the de-spec registry
//
// A method that runs off the end on purpose (`n == a.length + 1`, AIOOBE
// caught) fails the guard on every call. The failure is charged as an
// `UncommonTrap` at the header bci, and after `PER_BCI_DESPEC_LIMIT` traps the
// VM records the header bci in its de-spec registry
// (`vm/src/runtime/interpreter/deopt_resume.rs`). This pass then plants
// nothing at that header on the next compile, and the method keeps the
// optimizing tier with its per-iteration check, as before. Without a registry
// (`OptimizeContext::despec == None`) it plants nothing at all.
//
// # What it plants for
//
// Only the shape whose check `ir_check_elim` can then remove, and only an
// access every iteration makes, so that a failing guard almost always stands
// for an access that was going to fault anyway: a top-tested loop with one
// back edge, a header `If` on `iv < n` (either polarity), `iv` an `int` φ of
// the header starting at a non-negative constant and stepping by one (by one
// to three since round 12 wave 3; `ir_check_elim::MAX_PROVEN_STRIDE`),
// `n` an `int` invariant, and an `ArrayLoad`/`ArrayStore` of `a[iv]` anchored
// on the stay edge itself. Wave 8 (proposal W7-2) adds two shapes with a
// STRICT guard `x < a.length`, read by `ir_check_elim` as a strict fact:
// `iv <= n` with the same `iv` (`x = n`: `iv <= n < a.length`), and a down
// count `iv >= c`, `c >= 0`, with `iv` stepping by a negative constant from
// an invariant `init` (`x = init`: no value of `iv` exceeds it). In both, a
// failing guard is again an access that faults: `iv` reaches `n` (or starts
// at `init >= a.length`, which the first iteration indexes) unless the loop
// does not run at all, which costs a deopt, never a wrong result. The rest is
// as before: a guard per `a` of an `a[iv]` access anchored
// on the stay edge itself (through single-input merges), with `a` an
// invariant that is not a fresh allocation (EA's to reason about, and
// `new T[n]` is already proven by the allocation rule). The loop header must
// carry its own snapshot, so a loop of a spliced callee body is left alone,
// and that snapshot must name no allocation (`ir_lower` describes a
// scalar-replaced object per bci, against the block of that bci's deopt
// sites, which for the header's own snapshot would become the pre-header;
// `r11w7-mid-sr-deopt-block-is-chosen-per-bci-not-per-snapshot-FIXED-20260924.md`).

/// At most this many predicates per compile. Each is two nodes and a snapshot
/// copy; a method with more loops than this keeps the per-iteration checks in
/// the rest.
pub(in crate::ir_optimize) const MAX_RANGE_PREDICATES: usize = 8;



/// `CRATONVM_JIT_IR_OSR_ROUTE_NO_PREDICATE=1` (round 16, lane matrix): plant
/// no range predicate in the optimizing OSR route's compile
/// (`OptimizeContext::osr_route`). **default on** until measured.
///
/// A predicate is an `Op::Guard` (`UncommonTrap`) and the OSR door admits no
/// body holding a speculative guard exit: `osr_exit::guard_exit_fact` answers
/// `None` and the door refuses the WHOLE body ("the body has exits and the door
/// admits none of them"), at every header. So one predicate-able loop anywhere
/// in a method -- `R16Bench.matrix`'s closing `c[i][i]` sum, once the method
/// is admitted at all -- sends every OSR entry of that method to the
/// single-pass body. Measured on `R16MatrixOsrPred.sum` (base binary,
/// 2026-10-01): `non_exact=["14@UncommonTrap", …]` and a method-wide refusal
/// with the predicate, `guard_exits=Some([24])` without it. The method-entry
/// compile is a separate compile and keeps its predicates.
pub(in crate::ir_optimize) fn osr_route_withholds_range_predicates(osr_route: bool) -> bool {
    osr_route && true
}

/// One loop's predicates, decided without touching the graph.
pub(in crate::ir_optimize) struct RangePredicatePlan {
    /// The header merge's bci: the guards' resume point.
    pub(in crate::ir_optimize) header_bci: usize,
    /// The loop-entry control, where the new nodes are anchored.
    pub(in crate::ir_optimize) entry_pred: NodeId,
    /// The memory state flowing into the loop.
    pub(in crate::ir_optimize) entry_mem: NodeId,
    /// `x` in the planted `x <= a.length` (or `x < a.length`): `n` in
    /// `iv < n` / `iv <= n`, or a down count's start.
    pub(in crate::ir_optimize) bound: NodeId,
    /// `Le`, or `Lt` for the two shapes whose fact must be strict.
    pub(in crate::ir_optimize) cmp: CmpOp,
    /// Each distinct `a` indexed by `iv` on the stay edge.
    pub(in crate::ir_optimize) arrays: Vec<NodeId>,
    /// The header snapshot with its φs at their entry values.
    pub(in crate::ir_optimize) frame: SafepointSnapshot,
}

/// Is `ctrl` the control node `target`, possibly behind single-input `Merge`
/// pass-throughs?
pub(in crate::ir_optimize) fn reaches_through_pass_throughs(graph: &Graph, ctrl: NodeId, target: NodeId) -> bool {
    let mut c = ctrl;
    for _ in 0..4 {
        if c == target {
            return true;
        }
        let Some(n) = graph.node_opt(c) else {
            return false;
        };
        if n.op != Op::Merge || n.inputs.len() != 1 {
            return false;
        }
        match n.inputs.first() {
            Some(&p) => c = p,
            None => return false,
        }
    }
    c == target
}

/// Decide the predicates for the loop at `region`, or `None` when it is not
/// the shape the section notes above describe. The second half is the value
/// `s` of a `0 <= s` sign guard to plant beside the plan's length guards (t1
/// wave 6, lane B6: a `!=` loop's non-constant start), or `None`.
pub(in crate::ir_optimize) fn range_predicate_plan(
    graph: &Graph,
    users: &[Vec<NodeId>],
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> Option<(RangePredicatePlan, Option<NodeId>)> {
    let [back] = back_ctrls else {
        return None;
    };
    let region_node = graph.node_opt(region)?;
    let header_bci = region_node.bytecode_pc?;
    // The header's own snapshot, the first at its bci (the one a by-bci
    // consumer finds). No snapshot there: a spliced body, or a fixture. A
    // chain snapshot (round 13 wave 3, `ir::SpliceScopeChain`) is a spliced
    // body's too, and is not a frame this pass may copy.
    let source = graph
        .safepoints
        .iter()
        .position(|s| s.bci == header_bci && !s.is_splice_state())?;
    let entry_slot = region_node.inputs.iter().position(|&p| p == entry_pred)? + 1;
    let back_slot = region_node.inputs.iter().position(|&p| p == *back)? + 1;
    let region_arity = region_node.inputs.len();
    // `ir_check_elim::is_unit_stride_induction` reads the φ as exactly
    // `[region, init, next]`; a header of any other shape would get a guard
    // whose fact nothing can use.
    if region_arity != 2 || entry_slot != 1 {
        return None;
    }

    // The header's one control successor must be the exit test.
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
    let cond = graph.node_opt(test)?.input_opt(1)?;
    let cond_node = graph.node_opt(cond)?;
    let Op::Cmp(op) = cond_node.op else {
        return None;
    };
    let &[lhs, rhs] = cond_node.inputs.as_slice() else {
        return None;
    };

    let body = loop_body_with(graph, users, region, entry_pred, back_ctrls);
    // Which projection stays in the loop? Exactly one must.
    let p0 = proj_user(graph, test, 0);
    let p1 = proj_user(graph, test, 1);
    let stays = |p: NodeId| p != NO_NODE && body.contains(&p);
    let (stay, holds) = match (stays(p0), stays(p1)) {
        (true, false) => (p0, true),
        (false, true) => (p1, false),
        _ => return None,
    };
    // The compare as it holds on the stay edge.
    let op = if holds { op } else { op.negate() };
    // `iv = φ(region; init, iv + k)`, an `int` φ of this header with a
    // constant step: `(init, k)`.
    let induction = |iv: NodeId| -> Option<(NodeId, i64)> {
        let iv_node = graph.node_opt(iv)?;
        if iv_node.op != Op::Phi
            || iv_node.ty != IrType::Int
            || iv_node.inputs.first() != Some(&region)
            || iv_node.inputs.len() != region_arity + 1
        {
            return None;
        }
        let init = iv_node.input_opt(entry_slot)?;
        let next_node = graph.node_opt(iv_node.input_opt(back_slot)?)?;
        let &[l, r] = next_node.inputs.as_slice() else {
            return None;
        };
        let step = match (next_node.op == Op::Add, l == iv, r == iv) {
            (true, true, false) => r,
            (true, false, true) => l,
            _ => return None,
        };
        let step_node = graph.node_opt(step)?;
        match step_node.op {
            Op::Const(k) if step_node.ty == IrType::Int => Some((init, k)),
            _ => None,
        }
    };
    let non_negative_const = |n: NodeId| {
        matches!(
            graph.node_opt(n).map(|x| &x.op),
            Some(Op::Const(c)) if (0..=i64::from(i32::MAX)).contains(c)
        )
    };
    // Three shapes (wave 8 added the last two), each giving the value `x`
    // the guard compares and whether the guard must be strict:
    //
    // * `iv < n`, `iv` from a constant `>= 0` by `+1`: `n <= a.length`;
    // * `iv <= n`, the same `iv`: `n < a.length`;
    // * `iv >= c` with `c >= 0`, `iv` from `init` by a negative constant:
    //   `init < a.length` (every value of `iv` is at most `init`).
    let up = match op {
        CmpOp::Lt => Some((lhs, rhs, false)),
        CmpOp::Gt => Some((rhs, lhs, false)),
        CmpOp::Le => Some((lhs, rhs, true)),
        CmpOp::Ge => Some((rhs, lhs, true)),
        CmpOp::Eq | CmpOp::Ne => None,
    };
    // The r23 L8 close (L8-5, BCE21-5 proper): `iv != b.length` on the stay
    // edge, `iv` from a constant `c >= 1` by `+1`, `b` invariant. The guard is
    // `c <= b.length`: by induction over the trips `c <= iv <= b.length` at the
    // header (an increment happens only after `iv != b.length`), so the stay
    // edge has `iv < b.length`, which `ir_check_elim::ne_exit_bound` then
    // reads off this guard. A zero start needs no guard (`0 <= b.length`).
    //
    // t1 wave 6 (lane B6, `CRATONVM_JIT_IR_BCE_NE_EXIT_VAR_START`, default ON since j27): a
    // NON-constant start `s`, an `int` defined outside the loop, gets TWO
    // guards: `s <= b.length` (this plan's length guard) and `0 <= s` (the sign
    // guard returned beside the plan; none when `s` is a length). With both,
    // `s <= iv <= b.length` at the header by the same induction, and `iv >= s
    // >= 0`; `ir_check_elim::ne_exit_var_start_bound` reads the pair (its
    // section notes carry the argument). A negative `s` or one past
    // `b.length` fails a guard and deopts to the loop entry, where the
    // interpreter raises the AIOOBE itself. On an OSR graph only for a loop
    // every entry reaches through its pre-header: the sign guard is not a
    // range predicate's shape, so no OSR entry stub re-checks it (the
    // monotone predicate's own-length rule).
    let ne_start_on = crate::ir_check_elim::bce_ne_exit_start_enabled();
    let ne_var_start_on = crate::ir_check_elim::bce_ne_exit_var_start_enabled()
        && (!(graph.osr_compile || graph.optimize_osr_route)
            || loop_entered_only_through_its_pre_header(graph, region, entry_pred, &body));
    let ne_start: Option<(NodeId, NodeId, NodeId, Option<NodeId>)> = (op == CmpOp::Ne
        && (ne_start_on || ne_var_start_on))
    .then(|| {
        [(lhs, rhs), (rhs, lhs)].into_iter().find_map(|(iv, n)| {
            let (init, k) = induction(iv)?;
            if k != 1 {
                return None;
            }
            let init_node = graph.node_opt(init)?;
            let sign = match init_node.op {
                Op::Const(c) if ne_start_on && (1..=i64::from(i32::MAX)).contains(&c) => None,
                Op::Const(_) => return None,
                Op::ArrayLength if ne_var_start_on => None,
                _ if ne_var_start_on
                    && init_node.ty == IrType::Int
                    && !body.contains(&init)
                    && is_loop_invariant(graph, init, region, &body) =>
                {
                    Some(init)
                }
                _ => return None,
            };
            let len = graph.node_opt(n).filter(|x| x.op == Op::ArrayLength)?;
            let b = len.input_opt(2)?;
            Some((init, b, len.input_opt(1)?, sign))
        })
    })
    .flatten();
    if let Some((init, b, len_mem, sign)) = ne_start {
        let invariant_ref = graph.node_opt(b).is_some_and(|bn| {
            bn.ty == IrType::Ref
                && !matches!(bn.op, Op::NewArray { .. })
                && !body.contains(&b)
                && is_loop_invariant(graph, b, region, &body)
        });
        let anchors = crate::ir::exception_edge_anchors(graph);
        let anchor_arrays =
            crate::ir::exception_edge_anchor_arrays(graph, &anchors, &|id: NodeId| {
                body.contains(&id)
            });
        if !invariant_ref || anchor_arrays.contains(&b) {
            return None;
        }
        let entry_mem = match loop_entry_memory(graph, region, entry_pred) {
            Some(m) => m,
            None => Some(len_mem).filter(|m| !body.contains(m))?,
        };
        graph.node_opt(entry_mem)?;
        let frame = range_predicate_entry_frame(graph, source, region, entry_slot, &body)?;
        return Some((
            RangePredicatePlan {
                header_bci,
                entry_pred,
                entry_mem,
                bound: init,
                cmp: CmpOp::Le,
                arrays: vec![b],
                frame,
            },
            sign,
        ));
    }
    let down = match op {
        CmpOp::Ge => Some((lhs, rhs, 0i64)),
        CmpOp::Gt => Some((lhs, rhs, 1i64)),
        CmpOp::Le => Some((rhs, lhs, 0i64)),
        CmpOp::Lt => Some((rhs, lhs, 1i64)),
        CmpOp::Eq | CmpOp::Ne => None,
    };
    // Round 12 wave 3 (W2-1): a step up to `ir_check_elim`'s proven stride
    // (3 under the VM's array-length cap), since that is what the pass reading
    // the guard's fact accepts; `CRATONVM_JIT_IR_BCE_STRIDE3=0` pins it to 1.
    let counted_up = up.and_then(|(iv, n, inclusive)| match induction(iv)? {
        (init, k)
            if (1..=crate::ir_check_elim::induction_max_stride()).contains(&k)
                && non_negative_const(init) =>
        {
            Some((iv, n, inclusive))
        }
        _ => None,
    });
    let counted_down = || -> Option<(NodeId, NodeId)> {
        let (iv, lo, adjust) = down?;
        let lo_node = graph.node_opt(lo)?;
        let Op::Const(c) = lo_node.op else {
            return None;
        };
        if lo_node.ty != IrType::Int || c.checked_add(adjust)? < 0 {
            return None;
        }
        match induction(iv)? {
            (init, k) if (i64::from(i32::MIN)..=-1).contains(&k) => Some((iv, init)),
            _ => None,
        }
    };
    // JIT round 21 wave 4 (lane bce, BCE21-2,
    // `CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE`, opt-in): an up count's
    // constant entry value, for the `a[iv - c]` accesses below.
    let up_init: Option<i64> = counted_up
        .and_then(|(iv, _, _)| induction(iv))
        .and_then(|(init, _)| match graph.node_opt(init).map(|n| &n.op) {
            Some(Op::Const(c)) => Some(*c),
            _ => None,
        });
    let const_offset = up_init.is_some() && crate::ir_check_elim::const_offset_predicate_enabled();
    let (iv, bound, cmp) = match counted_up {
        Some((iv, n, false)) => (iv, n, CmpOp::Le),
        Some((iv, n, true)) => (iv, n, CmpOp::Lt),
        None => {
            let (iv, init) = counted_down()?;
            (iv, init, CmpOp::Lt)
        }
    };
    // `a[iv]`, or under BCE21-2 `a[iv - c]` with `c` at most the entry value
    // (`ir_check_elim`'s negative-offset arm reads the same guard: `iv < x <=
    // a.length` and `iv >= init >= c`). A `c` past the entry value indexes
    // below zero on the first iteration: no guard, the check stays.
    let indexes_iv = |idx: NodeId| -> bool {
        idx == iv
            || (const_offset
                && crate::ir_check_elim::negative_offset_index(graph, idx)
                    .is_some_and(|(v, c)| v == iv && up_init.is_some_and(|k| c <= k)))
    };

    // `x`: an `int` defined outside the loop.
    if graph.node_opt(bound)?.ty != IrType::Int
        || body.contains(&bound)
        || !is_loop_invariant(graph, bound, region, &body)
    {
        return None;
    }
    // `x` is `b.length` or `b.length - k`: `a[i]` over `b` itself is either
    // already proven (`ir_check_elim` reads both shapes) or, for `iv <= b.length`
    // and a down count from `b.length`, faults on every entry. No guard for `b`.
    let bound_length_of = graph
        .node_opt(bound)
        .filter(|b| b.op == Op::ArrayLength)
        .and_then(|b| b.input_opt(2))
        .or_else(|| crate::ir_check_elim::below_own_length(graph, bound));

    // The accesses `a[iv]` every iteration makes.
    let mut arrays: Vec<NodeId> = Vec::new();
    let mut access_mem: Option<NodeId> = None;
    // nio21 wave 6 L2: no guard for an array an exception-edge anchor of the
    // body indexes. The guard's `a.length` and its `iv < a.length` fact would
    // decide that anchor's null and bounds checks in the pre-header, so a
    // throw would deopt there instead of entering its handler.
    let anchors = crate::ir::exception_edge_anchors(graph);
    let anchor_arrays =
        crate::ir::exception_edge_anchor_arrays(graph, &anchors, &|id: NodeId| body.contains(&id));
    for (id, n) in graph.nodes.iter().enumerate() {
        if !matches!(n.op, Op::ArrayLoad(_) | Op::ArrayStore(_)) || !body.contains(&(id as NodeId))
        {
            continue;
        }
        let (Some(ctrl), Some(mem)) = (n.input_opt(0), n.input_opt(1)) else {
            continue;
        };
        let (Some(base), Some(idx)) = (n.input_opt(2), n.input_opt(3)) else {
            continue;
        };
        if !indexes_iv(idx)
            || !reaches_through_pass_throughs(graph, ctrl, stay)
            || arrays.contains(&base)
            || anchor_arrays.contains(&base)
            || Some(base) == bound_length_of
        {
            continue;
        }
        let Some(base_node) = graph.node_opt(base) else {
            continue;
        };
        if base_node.ty != IrType::Ref
            || matches!(base_node.op, Op::NewArray { .. })
            || body.contains(&base)
            || !is_loop_invariant(graph, base, region, &body)
        {
            continue;
        }
        if access_mem.is_none() {
            access_mem = Some(mem);
        }
        arrays.push(base);
    }
    if arrays.is_empty() {
        return None;
    }

    // The memory flowing into the loop: the header memory φ's entry input, or,
    // for a loop that writes nothing (no memory φ), the invariant state the
    // accesses read.
    let entry_mem = match loop_entry_memory(graph, region, entry_pred) {
        Some(m) => m,
        None => access_mem.filter(|m| !body.contains(m))?,
    };
    graph.node_opt(entry_mem)?;

    let frame = range_predicate_entry_frame(graph, source, region, entry_slot, &body)?;

    Some((
        RangePredicatePlan {
            header_bci,
            entry_pred,
            entry_mem,
            bound,
            cmp,
            arrays,
            frame,
        },
        None,
    ))
}

/// The loop-entry frame of a range predicate: the header snapshot `source`
/// with each header φ at its entry value. Any other slot must be a value
/// defined outside the loop.
pub(in crate::ir_optimize) fn range_predicate_entry_frame(
    graph: &Graph,
    source: usize,
    region: NodeId,
    entry_slot: usize,
    body: &FxHashSet<NodeId>,
) -> Option<SafepointSnapshot> {
    let mut frame = graph.safepoints.get(source)?.clone();
    loop_entry_values(graph, &mut frame, region, entry_slot, body)?;
    Some(frame)
}

/// Plant `Guard(n <= a.length)` in the pre-header of each loop the section
/// notes above describe, unless `despec` has withdrawn a speculation at that
/// loop's header for `method_key`. Returns the number of guards planted.
pub(in crate::ir_optimize) fn plant_range_check_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = false;
    let mut planted = 0usize;
    let headers = loop_headers(graph);
    // One users map for the plans, rebuilt after each install (which adds
    // nodes) rather than once per loop header; see `loop_body_with`.
    let mut users = build_users(graph);
    for (region, entry_pred, back_ctrls) in headers {
        if planted >= MAX_RANGE_PREDICATES {
            break;
        }
        let Some((plan, sign)) =
            range_predicate_plan(graph, &users, region, entry_pred, &back_ctrls)
        else {
            continue;
        };
        // The coarse per-bci question: whatever was withdrawn at the header
        // (this guard's `UncommonTrap`, or the `ArrayLength`'s `NullCheck`,
        // both of which resume there) withdraws the predicate.
        let Ok(bci32) = u32::try_from(plan.header_bci) else {
            continue;
        };
        if crate::deopt::predicate_withdrawn(
            despec,
            method_key,
            bci32,
            crate::deopt::PredicateKind::Range,
        ) {
            if dbg {
                eprintln!(
                    "[ir-licm] range predicate: header {region} bci {} de-specialised, not planted",
                    plan.header_bci
                );
            }
            continue;
        }
        // t1 wave 6 (lane B6): a `!=` loop's non-constant start also needs
        // `0 <= s`, planted only beside a length guard that went in.
        let sign_site = sign.map(|s| (s, plan.header_bci, plan.entry_pred, plan.frame.clone()));
        let added = install_range_predicates(graph, plan);
        planted += added;
        if added > 0 {
            if let Some((s, bci, pred, frame)) = sign_site {
                planted += install_sign_predicate(graph, s, bci, pred, frame);
            }
        }
        users = build_users(graph);
        if dbg {
            eprintln!("[ir-licm] range predicate: header {region} planted (total {planted})");
        }
    }
    planted
}

/// Add one plan's nodes and its frame. Returns the number of guards added.
pub(in crate::ir_optimize) fn install_range_predicates(graph: &mut Graph, plan: RangePredicatePlan) -> usize {
    let RangePredicatePlan {
        header_bci,
        entry_pred,
        entry_mem,
        bound,
        cmp,
        arrays,
        frame,
    } = plan;
    let pc = Some(header_bci);
    graph.push_safepoint(frame);
    let si = (graph.safepoints.len() - 1) as u32;
    let mut added = 0usize;
    for a in arrays {
        if added >= MAX_RANGE_PREDICATES {
            break;
        }
        let len = graph.add(
            Op::ArrayLength,
            IrType::Int,
            vec![entry_pred, entry_mem, a],
            pc,
        );
        let cond = graph.add(Op::Cmp(cmp), IrType::Int, vec![bound, len], pc);
        let guard = graph.add(
            Op::Guard { bci: header_bci },
            IrType::Void,
            vec![entry_pred, cond],
            pc,
        );
        // Both deopt at the header and MUST resume with the corrected frame:
        // unnamed, they would resolve the header's own snapshot, whose φ homes
        // the pre-header has not written yet. Naming cannot fail (the snapshot
        // is at their own pc), but if it did, take the nodes back out, and the
        // copy too when nothing names it: an unnamed second snapshot at the
        // header bci is what `ir_verify::check_frame_states` rejects.
        if !(graph.set_node_frame_snapshot(len, si) && graph.set_node_frame_snapshot(guard, si)) {
            graph.kill(guard);
            graph.kill(cond);
            graph.kill(len);
            if added == 0 {
                let keep: Vec<bool> = (0..graph.safepoints.len())
                    .map(|i| i != si as usize)
                    .collect();
                let _ = graph.retain_safepoints(&keep);
            }
            break;
        }
        added += 1;
    }
    added
}

/// t1 wave 6 (lane B6): add `Guard{header_bci}(0 <= s)` at `entry_pred`, named
/// by its own copy of the loop-entry `frame` -- the construction
/// `install_range_predicates` uses for a length guard, and the monotone
/// predicate's for its sign guards. Returns the number of guards added (the
/// copy is taken back out when the guard cannot name it).
pub(in crate::ir_optimize) fn install_sign_predicate(
    graph: &mut Graph,
    s: NodeId,
    header_bci: usize,
    entry_pred: NodeId,
    frame: SafepointSnapshot,
) -> usize {
    let pc = Some(header_bci);
    graph.push_safepoint(frame);
    let si = (graph.safepoints.len() - 1) as u32;
    let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![zero, s], pc);
    let guard = graph.add(
        Op::Guard { bci: header_bci },
        IrType::Void,
        vec![entry_pred, cond],
        pc,
    );
    if graph.set_node_frame_snapshot(guard, si) {
        return 1;
    }
    graph.kill(guard);
    graph.kill(cond);
    let keep: Vec<bool> = (0..graph.safepoints.len())
        .map(|i| i != si as usize)
        .collect();
    let _ = graph.retain_safepoints(&keep);
    0
}

// ── Clamp length predicates (JIT round 19 wave 5, lane osr5, IROPS3-1) ──
//
// `for (..) s += a[max(0, min(i + k, a.length - 1))]`: the clamp arm of
// `ir_check_elim` (IROPS2-7) drops the check of such an access only where
// `a.length >= 1` is known, because on an EMPTY array the clamp yields `0` (or
// `-1`) and the check is what throws. The first clamped access of every
// iteration therefore kept its check. This plants, in the pre-header,
//
//     len = ArrayLength[entry_pred, entry_mem, a]
//     Guard{header_bci}[entry_pred, Cmp(Lt)[0, len]]
//
// named by the loop-entry frame, exactly the range predicates' construction
// (`install_range_predicates`, whose soundness notes apply word for word: the
// new nodes are the newest, read the loop's entry memory, and resume at the
// header with each header phi at its entry value). The guard is a range
// predicate's shape (`Cmp(Lt)[x, len]`, `len` an `ArrayLength` at the header
// bci), so `ir_lower::op_guard_is_range_predicate` recognises it: the OSR
// entry stubs re-check it and its exit is resumable.
//
// A failing guard means an empty `a`: either the loop runs, and its first
// clamped access faults anyway, or it does not, and the deopt costs one
// interpreted zero-trip loop -- never a wrong result. A method that keeps
// failing it de-specialises its header (the registry the range predicates
// share), and the next compile plants nothing there.

/// Plant `Guard(0 < a.length)` at the entry of each loop whose body indexes an
/// invariant array `a` by a clamp needing `a.length >= 1`
/// (`ir_check_elim::clamp_needs_a_non_empty_array`), unless `despec` withdrew
/// a speculation at that header. Returns the number of guards planted.
pub(in crate::ir_optimize) fn plant_clamp_length_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = false;
    let mut planted = 0usize;
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if planted >= MAX_RANGE_PREDICATES {
            break;
        }
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) {
            continue;
        }
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        let mut members: Vec<NodeId> = body.iter().copied().collect();
        members.sort_unstable();
        // `(array, memory its first clamped access reads)`.
        let mut arrays: Vec<(NodeId, NodeId)> = Vec::new();
        for id in members {
            let Some(n) = graph.node_opt(id) else {
                continue;
            };
            if !matches!(n.op, Op::ArrayLoad(_) | Op::ArrayStore(_)) {
                continue;
            }
            let (Some(mem), Some(base), Some(idx)) =
                (n.input_opt(1), n.input_opt(2), n.input_opt(3))
            else {
                continue;
            };
            if arrays.iter().any(|&(a, _)| a == base)
                || !crate::ir_check_elim::clamp_needs_a_non_empty_array(graph, idx, base)
            {
                continue;
            }
            let Some(bn) = graph.node_opt(base) else {
                continue;
            };
            if bn.ty != IrType::Ref
                || matches!(bn.op, Op::NewArray { .. })
                || body.contains(&base)
                || !is_loop_invariant(graph, base, region, &body)
            {
                continue;
            }
            arrays.push((base, mem));
        }
        if arrays.is_empty() {
            continue;
        }
        if preheader_may_trap(graph, entry_pred) {
            continue;
        }
        let Some((header_bci, frame)) = loop_entry_frame(graph, region, entry_slot, &body) else {
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        if crate::deopt::predicate_withdrawn(
            despec,
            method_key,
            bci32,
            crate::deopt::PredicateKind::ClampLength,
        ) {
            if dbg {
                eprintln!(
                    "[ir-licm] clamp predicate: header {region} bci {header_bci} de-specialised"
                );
            }
            continue;
        }
        let entry_mem = loop_entry_memory(graph, region, entry_pred);
        let pc = Some(header_bci);
        let mut si: Option<u32> = None;
        let mut added_here = 0usize;
        for (a, mem) in arrays {
            if planted >= MAX_RANGE_PREDICATES {
                break;
            }
            // The loop's entry state; for a loop that writes nothing (no
            // memory phi), the invariant state its access reads.
            let Some(mem) = entry_mem.or_else(|| (!body.contains(&mem)).then_some(mem)) else {
                continue;
            };
            let snapshot = match si {
                Some(s) => s,
                None => {
                    graph.push_safepoint(frame.clone());
                    // Cast: a snapshot index; `push_safepoint` just grew the list.
                    let s = (graph.safepoints.len() - 1) as u32;
                    si = Some(s);
                    s
                }
            };
            let len = graph.add(Op::ArrayLength, IrType::Int, vec![entry_pred, mem, a], pc);
            let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
            let cond = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![zero, len], pc);
            let guard = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, cond],
                pc,
            );
            if !(graph.set_node_frame_snapshot(len, snapshot)
                && graph.set_node_frame_snapshot(guard, snapshot))
            {
                graph.kill(guard);
                graph.kill(cond);
                graph.kill(len);
                // An unnamed second snapshot at the header bci is what
                // `ir_verify::check_frame_states` rejects.
                if added_here == 0 {
                    let keep: Vec<bool> = (0..graph.safepoints.len())
                        .map(|i| i != snapshot as usize)
                        .collect();
                    let _ = graph.retain_safepoints(&keep);
                }
                break;
            }
            added_here += 1;
            planted += 1;
            if dbg {
                eprintln!(
                    "[ir-licm] clamp predicate: header {region} bci {header_bci}: 0 < n{a}.length"
                );
            }
        }
    }
    planted
}

// ── Loop-predicated array lengths (round 17, CG16P-2) ────────────────
//
// `for (i = from; i < from + len; i++) s += a[(i * 7 + (i >>> 5)) % a.length];`
// reads `a.length` (a null check and a load) on every iteration. LICM's
// `ArrayLength` arm takes only a node anchored at the header, and the body
// of this loop may run zero times, so a node anchored past the exit test
// cannot leave: `install_preheader_deopt_frame` resumes a hoisted node's
// deopt at its OWN bci, which on a loop that would not have been entered
// runs a body the interpreter never reaches.
//
// HotSpot's loop predication answers it by resuming at the LOOP ENTRY
// instead. That is what this does: a fresh `ArrayLength` of the invariant
// array in the pre-header, named by the loop-entry frame (the header
// snapshot with each header φ at its entry value, the frame
// `range_predicate_plan` builds), and every use of the in-body nodes moved to
// it. A null array then deopts BEFORE the header, and the interpreter runs
// the loop from its first test: zero trips and no exception if the body
// would not have run, the same NPE at the same bci if it would. Nothing has
// run between the pre-header and that test, so the resume IS the loop entry.



/// JIT r27 LOOP27-2: the entry value of `v` when it is the `i2l` of a header
/// phi of `region` that a narrowed `long` induction hands its frames
/// (`Graph::wide_entry`).
pub(in crate::ir_optimize) fn wide_entry_value(graph: &Graph, v: NodeId, region: NodeId) -> Option<NodeId> {
    let n = graph.node_opt(v).filter(|n| n.op == Op::I2L)?;
    let p = graph.node_opt(*n.inputs.first()?)?;
    (p.op == Op::Phi && p.inputs.first() == Some(&region))
        .then(|| graph.wide_entry.get(&v).copied())
        .flatten()
}

/// The header snapshot of `region` with each header φ at its entry value, or
/// `None` when a slot names any other value computed in the loop (or an
/// allocation defined inside the loop). Admitted allocations remain concrete
/// through the moved snapshot. The frame
/// `range_predicate_plan` builds, for a header of any arity.
pub(in crate::ir_optimize) fn loop_entry_frame(
    graph: &Graph,
    region: NodeId,
    entry_slot: usize,
    body: &FxHashSet<NodeId>,
) -> Option<(usize, SafepointSnapshot)> {
    let header = graph.node_opt(region)?;
    let header_bci = header.bytecode_pc?;
    let at_header = |s: &&SafepointSnapshot| s.bci == header_bci && !s.is_splice_state();
    let mut frame = header
        .frame_snapshot
        .and_then(|si| graph.safepoints.get(si as usize))
        .filter(at_header)
        .or_else(|| graph.safepoints.iter().find(at_header))?
        .clone();
    loop_entry_values(graph, &mut frame, region, entry_slot, body)?;
    Some((header_bci, frame))
}

/// JIT round 23 wave 4 (lane L1): [`loop_entry_frame`] for a loop SPLICED from
/// a callee, whose header has no frame of the compiling method -- only the
/// inlined CHAIN snapshot the builder recorded at the header's combined pc
/// (callee frame at its own loop header, every caller parked at its invoke).
/// The same substitution over every slot of the chain: the callee's header
/// φs take their entry values, and every other slot (the callee's other
/// locals, each caller's locals and operand stack, the compiling method's
/// monitors) must already be loop-invariant, as it is for a flat frame.
/// `None` unless exactly one guard-usable chain snapshot sits at the header's
/// pc (an unrolled outer loop's copies would make a by-pc choice ambiguous)
/// and its layout splits ([`crate::ir::SpliceScopeChain::split`]).
pub(in crate::ir_optimize) fn loop_entry_chain_frame(
    graph: &Graph,
    region: NodeId,
    entry_slot: usize,
    body: &FxHashSet<NodeId>,
) -> Option<(usize, SafepointSnapshot)> {
    let header_bci = graph.node_opt(region)?.bytecode_pc?;
    let mut sources = graph
        .safepoints
        .iter()
        .enumerate()
        .filter(|(_, s)| s.bci == header_bci && s.is_guard_splice_state());
    let (source, _) = sources.next()?;
    if sources.next().is_some()
        || graph
            .safepoints
            .iter()
            .any(|s| s.bci == header_bci && !s.is_splice_state())
    {
        return None;
    }
    let mut frame = graph.safepoints.get(source)?.clone();
    let chain = frame.splice_scope.as_deref()?;
    if chain.scopes.len() < 2 || chain.split(&frame.locals, &frame.stack).is_none() {
        return None;
    }
    loop_entry_values(graph, &mut frame, region, entry_slot, body)?;
    Some((header_bci, frame))
}

/// The slot rewrite of [`loop_entry_frame`] / [`loop_entry_chain_frame`]: each
/// header φ of `region` at its entry value; `None` when a slot names any
/// other value computed in the loop. Allocations defined before the loop are
/// retained by escape analysis through the moved snapshot.
pub(in crate::ir_optimize) fn loop_entry_values(
    graph: &Graph,
    frame: &mut SafepointSnapshot,
    region: NodeId,
    entry_slot: usize,
    body: &FxHashSet<NodeId>,
) -> Option<()> {
    let mut preserve_allocations = false;
    for slot in frame
        .locals
        .iter_mut()
        .chain(frame.stack.iter_mut())
        .chain(frame.monitors.iter_mut())
    {
        let v = *slot;
        if v == NO_NODE {
            continue;
        }
        let vn = graph.node_opt(v)?;
        // JIT r27 LOOP27-2: `i2l` of a narrowed `long` induction.
        if let Some(wide) = wide_entry_value(graph, v, region) {
            *slot = wide;
            continue;
        }
        let value = if vn.op == Op::Phi && vn.inputs.first() == Some(&region) {
            vn.inputs.get(entry_slot).copied()?
        } else {
            v
        };
        if value == NO_NODE {
            *slot = value;
            continue;
        }
        if body.contains(&value) || !is_loop_invariant(graph, value, region, body) {
            return None;
        }
        preserve_allocations |= matches!(
            graph.node_opt(value)?.op,
            Op::New { .. } | Op::NewArray { .. }
        );
        *slot = value;
    }
    frame.preserve_allocations |= preserve_allocations;
    Some(())
}



pub(in crate::ir_optimize) fn plant_length_predicates(
    graph: &mut Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> usize {
    if graph.safepoints.is_empty() {
        return 0;
    }
    let dbg = false;
    let mut replaced = 0usize;
    // nio21 wave 6 L2: the nodes exception edges hang on. Their guards stay
    // in the body, and no length of an array they index is decided ahead of
    // the loop (`crate::ir::exception_edge_anchors`).
    let anchors = crate::ir::exception_edge_anchors(graph);
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) {
            continue;
        }
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let anchor_arrays =
            crate::ir::exception_edge_anchor_arrays(graph, &anchors, &|id: NodeId| {
                body.contains(&id)
            });
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        // `(in-body node, array, memory it reads)`: the full `[ctrl, mem,
        // array]` form, anchored past the header (LICM's arm serves the
        // header's own), of a loop-invariant array defined outside the loop.
        let candidates: Vec<(NodeId, NodeId, NodeId)> = body
            .iter()
            .copied()
            .filter_map(|id| {
                let n = graph.node_opt(id)?;
                if n.op != Op::ArrayLength || n.inputs.len() < 3 || n.inputs[0] == region {
                    return None;
                }
                let array = n.inputs[2];
                let an = graph.node_opt(array)?;
                if an.ty != IrType::Ref
                    || matches!(an.op, Op::NewArray { .. })
                    || body.contains(&array)
                    || anchor_arrays.contains(&array)
                    || !is_loop_invariant(graph, array, region, &body)
                {
                    return None;
                }
                Some((id, array, n.inputs[1]))
            })
            .collect();
        // Round 17: and every speculative guard of the body whose condition is
        // computed outside the loop -- `len != 0` for a `%` by an invariant
        // array's length, once the length itself is planted above. Same
        // argument: failing in the pre-header resumes at the loop entry.
        let any_guard = body.iter().any(|&id| {
            graph
                .node_opt(id)
                .is_some_and(|n| matches!(n.op, Op::Guard { .. }))
        });
        if candidates.is_empty() && !any_guard {
            continue;
        }
        if preheader_may_trap(graph, entry_pred) {
            if dbg {
                eprintln!("[ir-licm] length predicate: header {region} pre-header may trap");
            }
            continue;
        }
        // The r23 L8 heap-buffer close (`CRATONVM_JIT_IR_LENGTH_PREDICATE_CHAIN`):
        // a loop spliced from a callee moves its invariant GUARDS through the
        // loop-entry chain, the null-base predicate's discipline. Not its
        // `ArrayLength`s: a node that is no listed chain guard would be
        // charged at the call site under the wildcard, which drops the splice.
        let spliced = header_is_spliced(graph, region)
            && (!(graph.osr_compile || graph.optimize_osr_route)
                || spliced_loop_entered_only_through_its_pre_header(graph, region, &body));
        let header_pc = graph.node_opt(region).and_then(|r| r.bytecode_pc);
        let reused = match header_pc {
            Some(pc) if spliced => chain_predicate_guard_frame(graph, entry_pred, pc),
            _ => None,
        };
        let entry_frame = match reused {
            Some(si) => header_pc.zip(graph.safepoints.get(si as usize).cloned()),
            None if spliced => loop_entry_chain_frame(graph, region, entry_slot, &body),
            None => loop_entry_frame(graph, region, entry_slot, &body),
        };
        let Some((header_bci, frame)) = entry_frame else {
            if dbg {
                eprintln!("[ir-licm] length predicate: header {region} has no entry frame");
            }
            continue;
        };
        let Ok(bci32) = u32::try_from(header_bci) else {
            continue;
        };
        // A `NullCheck` withdrawn at the header (this node's own deopt resumes
        // there) withdraws the predicate. A spliced loop's is asked at the
        // compiling method's call site, under its header's chain id.
        let withdrawn = match frame.splice_scope.as_deref() {
            Some(chain) => match (chain.scopes.first(), chain.scopes.last()) {
                (Some(inner), Some(outer)) => crate::deopt::chain_predicate_withdrawn(
                    despec,
                    method_key,
                    outer.bci,
                    &inner.method_key,
                    inner.bci,
                    crate::deopt::PredicateKind::Length,
                ),
                _ => true,
            },
            None => crate::deopt::predicate_withdrawn(
                despec,
                method_key,
                bci32,
                crate::deopt::PredicateKind::Length,
            ),
        };
        if withdrawn {
            if dbg {
                eprintln!(
                    "[ir-licm] length predicate: header {region} bci {header_bci} de-specialised"
                );
            }
            continue;
        }
        let candidates = if spliced { Vec::new() } else { candidates };
        let entry_mem = loop_entry_memory(graph, region, entry_pred);
        let si = match reused {
            Some(si) => si,
            None => {
                graph.push_safepoint(frame);
                // Cast: a snapshot index; `push_safepoint` just grew the list.
                (graph.safepoints.len() - 1) as u32
            }
        };
        let mut planted: Vec<(NodeId, NodeId)> = Vec::new();
        // JIT r23 L1W3-2: the frames the moved nodes named, asked again once
        // they are gone (`orphaned_predicate_frames`).
        let mut left_frames: Vec<u32> = Vec::new();
        for (al, array, mem) in candidates {
            let new = match planted.iter().find(|&&(a, _)| a == array) {
                Some(&(_, new)) => new,
                None => {
                    // `ArrayLength` conflicts with no store, so the memory is
                    // pure ordering: the loop's entry state keeps the new node
                    // after every pre-header store, which is what lets the
                    // resume skip nothing.
                    let new_mem = if is_loop_invariant(graph, mem, region, &body) {
                        mem
                    } else {
                        match entry_mem {
                            Some(m) => m,
                            None => continue,
                        }
                    };
                    let new = graph.add(
                        Op::ArrayLength,
                        IrType::Int,
                        vec![entry_pred, new_mem, array],
                        Some(header_bci),
                    );
                    if !graph.set_node_frame_snapshot(new, si) {
                        graph.kill(new);
                        continue;
                    }
                    planted.push((array, new));
                    new
                }
            };
            left_frames.extend(graph.nodes[al as usize].frame_snapshot);
            graph.replace_all_uses(al, new);
            graph.kill(al);
            replaced += 1;
            if dbg {
                eprintln!(
                    "[ir-licm] length predicate: header {region} bci {header_bci}: n{al} -> n{new} \
                     (array n{array})"
                );
            }
        }
        // The guards, asked of the graph as it stands now: a planted length
        // makes `len != 0` invariant only once its in-body read is gone.
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        // nio21 wave 7 (lane L5; `r21-charat-trap-reads-a-length-predicate-class-guard`):
        // a pre-header `Guard{bci: header}(ExactClassIs(..))` is the shape that
        // licenses the `charAt` trap (`charat_trap::seq_charat_traps_on_miss`),
        // which only `class_predicate` may plant (after its frame and de-spec
        // screen). So no class guard leaves a loop that holds such a call.
        let keeps_class_guards = body.iter().any(|&id| {
                graph
                    .node_opt(id)
                    .is_some_and(charat_trap::is_seq_charat_call)
            });
        // JIT round 20 wave 5 (lane loopopt; correctness, unconditional;
        // `R20FuzzLicm1` div.zero.trip0 / div.guarded / rem.* / alt.loop): a
        // division's zero guard stays at its division's bci. With the guard
        // token off (the default) the `Div` / `Rem` floats freely -- its
        // inputs are invariant, so it is scheduled ahead of the loop -- and
        // the lowering decides its own zero test by whether a guard is
        // anchored at the division's bci (`Lowerer::emit_div_zero_guard`):
        // anchored, a zero divisor yields a placeholder the guard keeps from
        // being used; NOT anchored, the division deopts at its OWN bci, which
        // resumes the interpreter inside the loop body and throws
        // `ArithmeticException` although the loop never runs (or the arm
        // holding the division is never taken). Moving the guard to the
        // header bci is what un-anchored it.
        let division_bcis: FxHashSet<usize> = graph
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Div | Op::Rem))
            .filter_map(|n| n.bytecode_pc)
            .collect();
        let guards: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| {
                graph.node_opt(id).is_some_and(|n| {
                    // With the guard token on, the division names the guard
                    // and is ordered after it wherever it moves, so its own
                    // zero test is unreachable and the move is safe.
                    if let Op::Guard { bci } = n.op {
                        if division_bcis.contains(&bci) && !crate::ir::guard_token_edges_enabled() {
                            return false;
                        }
                    }
                    if keeps_class_guards
                        && n.inputs.len() == 2
                        && graph
                            .node_opt(n.inputs[1])
                            .is_some_and(|c| matches!(c.op, Op::ExactClassIs { .. }))
                    {
                        return false;
                    }
                    // Exactly `[ctrl, cond]`: a guard carrying another
                    // guard's token is ordered after it, and stays.
                    //
                    // Round nio20 wave 4 (lane L1): never a CONSTANT
                    // condition. A `Guard(Const(0))` is a site trap (or a
                    // guard that folded false) on whatever arm holds it --
                    // usually a cold one; in the pre-header it would deopt
                    // EVERY entry to the loop, charged at the header bci
                    // rather than the site's, until the de-spec registry
                    // withdrew the whole header. A non-zero constant never
                    // fails, so moving it buys nothing but a snapshot.
                    matches!(n.op, Op::Guard { .. })
                        && n.inputs.len() == 2
                        && !anchors.contains(&id)
                        && !body.contains(&n.inputs[1])
                        && !matches!(
                            graph.node_opt(n.inputs[1]).map(|c| &c.op),
                            Some(Op::Const(_))
                        )
                        && is_loop_invariant(graph, n.inputs[1], region, &body)
                })
            })
            .collect();
        let mut guards_planted = 0usize;
        for g in guards {
            let Some(cond) = graph.node_opt(g).and_then(|n| n.inputs.get(1).copied()) else {
                continue;
            };
            let new = graph.add(
                Op::Guard { bci: header_bci },
                IrType::Void,
                vec![entry_pred, cond],
                Some(header_bci),
            );
            if !graph.set_node_frame_snapshot(new, si) {
                graph.kill(new);
                continue;
            }
            if spliced {
                graph
                    .chain_predicate_guards
                    .push((new, crate::deopt::PredicateKind::Length as u8));
            }
            // The licensed nodes name the guard as a trailing token input
            // (`Op::guard_shape`): they move to the pre-header guard, which
            // dominates them.
            left_frames.extend(graph.nodes[g as usize].frame_snapshot);
            graph.replace_all_uses(g, new);
            graph.kill(g);
            guards_planted += 1;
            replaced += 1;
            if dbg {
                eprintln!(
                    "[ir-licm] length predicate: header {region} bci {header_bci}: guard n{g} -> \
                     n{new} (cond n{cond})"
                );
            }
        }
        guards_planted += super::charat_trap::hoist_invariant_string_charat(
            graph,
            region,
            entry_pred,
            &back_ctrls,
            entry_mem,
            header_bci,
            si,
            spliced,
            &mut left_frames,
            &mut replaced,
        );
        guards_planted += super::charat_trap::hoist_invariant_arraylist_get(
            graph,
            region,
            entry_pred,
            &back_ctrls,
            entry_mem,
            header_bci,
            si,
            spliced,
            &mut left_frames,
            &mut replaced,
        );
        let mut keep: Vec<bool> = vec![true; graph.safepoints.len()];
        if planted.is_empty() && guards_planted == 0 && reused.is_none() {
            // Nothing names the copy: an unnamed second snapshot at the header
            // bci is what `ir_verify::check_frame_states` rejects.
            keep[si as usize] = false;
        }
        {
            for orphan in orphaned_predicate_frames(graph, &left_frames) {
                keep[orphan] = false;
            }
        }
        let _ = graph.retain_safepoints(&keep);
    }
    replaced
}



/// The snapshots among `left` (the frames of nodes a predicate just moved out
/// of a loop and killed) that nothing can consult any more: no live node
/// names one, and an EARLIER snapshot at the same bci answers every lookup by
/// position (`ir_lower::safepoint_first_by_bci`). Ascending, without repeats.
///
/// The case it exists for is a nested loop (JIT r23 L1W3-2,
/// `L1W3FpCarried.nested`). The range predicate plants
/// `Guard(n <= a.length)` in the INNER loop's pre-header, named by a copy of
/// the inner header's frame -- a second snapshot at the inner header bci.
/// The same round's length predicate then finds that guard invariant in the
/// OUTER loop and moves it, with its `ArrayLength`, to the outer pre-header
/// under the outer entry frame. Both nodes that named the inner copy are
/// gone, and the copy, left behind with no node naming it, is the
/// "unclaimed duplicate" `ir_verify::check_frame_states` refuses: every
/// method-entry compile of such a loop nest failed verification and stayed
/// on the single-pass tier.
///
/// Dropping the copy changes no deopt. Every bci-keyed consumer takes the
/// first snapshot at a bci, which this never drops, and a node that named
/// the copy would have kept it. The builder's own frame (the first at its
/// bci) stays even when nothing names it.
pub(in crate::ir_optimize) fn orphaned_predicate_frames(graph: &Graph, left: &[u32]) -> Vec<usize> {
    if left.is_empty() {
        return Vec::new();
    }
    let mut named = vec![false; graph.safepoints.len()];
    for n in graph.nodes.iter() {
        if n.op == Op::Dead {
            continue;
        }
        if let Some(slot) = n.frame_snapshot.and_then(|si| named.get_mut(si as usize)) {
            *slot = true;
        }
    }
    let mut out: Vec<usize> = left
        .iter()
        .map(|&si| si as usize)
        .filter(|&si| {
            let Some(sp) = graph.safepoints.get(si) else {
                return false;
            };
            // A post-call-only snapshot is found by the call it names, never
            // by its bci (`ir_verify::check_frame_states` skips it too).
            !named[si]
                && !sp.post_call_only
                && graph
                    .safepoints
                    .iter()
                    .take(si)
                    .any(|e| !e.post_call_only && e.bci == sp.bci)
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}



// ── Null-base load predicates (JIT round 23 wave 2d, lane L1, TCL23-1) ──
//
// `for (i = 0; i < n; i++) s += h.k;` with `h` a parameter: LICM refuses the
// read (`load NN: NOT hoisted -- base may be null at the pre-header and the
// body may not run`). Hoisted as it stands, a null `h` would fault in the
// pre-header of a loop that may run zero times, and a resume at the read's own
// bci would run a body the interpreter never enters. HotSpot hoists it behind a
// loop predicate. So does this: a pre-header `Guard(h != null)` named by the
// LOOP-ENTRY frame (the header snapshot with each header φ at its entry value,
// `loop_entry_frame`, as the length predicate's), then a copy of each such read
// after it. A null `h` deopts BEFORE the header and the interpreter runs the
// loop from its first test: zero trips and no exception if the body would not
// have run, the same NPE at the same bci if it would
// (`docs/feature-designs/jit-ir-optimizations.md`).
//
// Admitted reads, each obligation the general arm's:
// * a non-volatile field `Op::Load` anchored on every iteration's spine
//   (`anchored_on_every_iteration`), so the loop runs it on its first trip;
// * its base and address loop-invariant and defined outside the loop, the base
//   a reference that is not an allocation of this graph;
// * a body that writes nothing (`loop_writes_memory`), so the value read at the
//   pre-header is the value every iteration reads;
// * no body guard licenses it (`LoopGuards::permits_hoist_proven`, with the
//   predicate standing in for "the base is non-null");
// * no exception edge hangs on it.
// The guard is created before the copies, so the copies follow it in the
// pre-header (node order is block order there, as `class_predicate` notes).

/// `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE` (JIT round 23 wave 2d, lane L1,
/// TCL23-1; perf, **default ON**, `=0` kill): plant the null-base load
/// predicates above. Read per compile.
pub(crate) fn null_base_load_predicate_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_OSR` (JIT round 23 wave 3c, lane L1;
/// perf, **default ON**, `=0` kill): the null-base load predicates on the
/// optimizing OSR route and in an OSR-door graph too, where the hot loops of a
/// method run once live. Only for a loop whose pre-header is the opaque OSR
/// entry merge of its own header, and whose body holds no other OSR entry
/// ([`loop_entered_only_through_its_pre_header`]). Every entry, normal or OSR,
/// then passes the guard. The guard's frame is the loop-entry frame (each
/// header phi at its value on the merge's edge, which on the OSR arm is the
/// interpreter's local). Its failure is an `UncommonTrap` guard exit at the
/// header, which the door resumes in the live frame like every other guard
/// exit of an opaque-entry body (`osr_optimizing_guard_exits_admissible`). The
/// interpreter then runs the loop from its first test. Read per compile.
pub(crate) fn null_base_load_predicate_osr_enabled() -> bool {
    true
}

/// In an OSR graph, is every way into the loop at `region` through
/// `entry_pred`, so a node placed there runs before any iteration
/// (JIT round 23 wave 3c, lane L1)?
/// - `entry_pred` must be the opaque OSR entry merge of this header: a `Merge`
///   one of whose inputs projects an `If` on `OsrEntryFlag { header_bci }` of
///   the region's own bci. The OSR stub then enters through that merge, not
///   straight into the header.
/// - The body must import nothing from an OSR arm (no input is an `OsrLocal` /
///   `OsrMemory`), and no body node may test an `OsrEntryFlag`, so no inner
///   header's entry jumps into the loop past the pre-header.
///
/// The flag test is the decisive one (r23 wave 3 review): `OsrLocal` and
/// `OsrEntryFlag` are pure, so `loop_body` never admits them, and an inner
/// arm's `OsrMemory` is in the body only while it survives; the inner arm's
/// `If(OsrEntryFlag)` is body control as long as the arm reaches the back edge.
///
/// `false` whenever unsure.
pub(in crate::ir_optimize) fn loop_entered_only_through_its_pre_header(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    body: &FxHashSet<NodeId>,
) -> bool {
    let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
        return false;
    };
    let Some(merge) = graph.node_opt(entry_pred) else {
        return false;
    };
    if merge.op != Op::Merge {
        return false;
    }
    let osr_arm = merge.inputs.iter().any(|&p| {
        graph
            .node_opt(p)
            .filter(|pn| matches!(pn.op, Op::Proj(_)))
            .and_then(|pn| pn.inputs.first().copied())
            .and_then(|iff| graph.node_opt(iff))
            .filter(|ifn| ifn.op == Op::If)
            .and_then(|ifn| ifn.inputs.get(1).copied())
            .and_then(|c| graph.node_opt(c))
            .is_some_and(|cn| matches!(cn.op, Op::OsrEntryFlag { header_bci: h } if h == header_bci))
    });
    if !osr_arm {
        return false;
    }
    body_imports_no_osr_entry(graph, body)
}

/// No node of `body` is, or reads, an OSR arm's `OsrLocal` / `OsrMemory` /
/// `OsrEntryFlag`: no OSR entry jumps into the loop past its pre-header. The
/// body half of [`loop_entered_only_through_its_pre_header`] and
/// [`spliced_loop_entered_only_through_its_pre_header`].
pub(in crate::ir_optimize) fn body_imports_no_osr_entry(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    !body.iter().any(|&id| {
        graph.node_opt(id).is_some_and(|n| {
            matches!(n.op, Op::OsrLocal(_) | Op::OsrMemory | Op::OsrEntryFlag { .. })
                || n.inputs.iter().any(|&i| {
                    graph
                        .node_opt(i)
                        .is_some_and(|inp| {
                            matches!(
                                inp.op,
                                Op::OsrLocal(_) | Op::OsrMemory | Op::OsrEntryFlag { .. }
                            )
                        })
                })
        })
    })
}

/// JIT round 23 wave 4 (lane L1): [`loop_entered_only_through_its_pre_header`]
/// for a loop SPLICED from a callee, in an OSR graph. Such a header is never
/// an OSR entry -- the door enters only the compiling method's own headers,
/// and the builder plants no opaque merge inside a splice
/// (`IrBuilder::build_opaque_osr_merge`) -- so every way into the loop is its
/// pre-header or a back edge, provided:
/// - no `OsrEntryFlag` anywhere in the graph names this header's pc;
/// - the body imports nothing from an OSR arm ([`body_imports_no_osr_entry`]);
/// - the graph's entries are OPAQUE merges (some `OsrEntryFlag` exists): the
///   predicate guard is speculative, and the door admits a speculative guard
///   exit only from a body every one of whose entries is opaque
///   (`osr_exit::guard_exit_fact_opaque`). A body whose entries are stubs
///   would lose its whole OSR admission to the guard.
pub(in crate::ir_optimize) fn spliced_loop_entered_only_through_its_pre_header(
    graph: &Graph,
    region: NodeId,
    body: &FxHashSet<NodeId>,
) -> bool {
    let Some(header_bci) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
        return false;
    };
    let mut opaque = false;
    for n in graph.nodes.iter() {
        if let Op::OsrEntryFlag { header_bci: h } = n.op {
            if h == header_bci {
                return false;
            }
            opaque = true;
        }
    }
    opaque && body_imports_no_osr_entry(graph, body)
}

/// Does a pre-header node that may trap stand in the way of a loop-entry
/// predicate of the loop at `header_bci`? Not when its own deopt names a frame
/// of this method at or past the header (a hoisted read with its corrected
/// first-iteration frame, another predicate): whichever of the two fails first,
/// the interpreter resumes at or before the other's bytecode with the values
/// the loop entered with, and raises what bytecode order raises first. Any
/// other trapping pre-header node refuses.
pub(in crate::ir_optimize) fn preheader_traps_block_entry_predicate(
    graph: &Graph,
    preheader: NodeId,
    header_bci: usize,
) -> bool {
    // JIT round 23 wave 8 (lane W8-L7,
    // `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_SPLICED_HOIST`): with the
    // build's splice map, LICM's copy of a SPLICED loop's own length / element
    // read is stepped over too (`spliced_hoist_trap`).
    let splices = if spliced_hoist_trap::spliced_hoist_trap_exempt_enabled() {
        crate::ir::splice_resume_map_this_build()
    } else {
        None
    };
    preheader_traps_block_entry_predicate_with(graph, preheader, header_bci, splices.as_ref())
}

/// [`preheader_traps_block_entry_predicate`] with the splice map handed in
/// (`None`: the rule before JIT round 23 wave 8).
pub(in crate::ir_optimize) fn preheader_traps_block_entry_predicate_with(
    graph: &Graph,
    preheader: NodeId,
    header_bci: usize,
    splices: Option<&crate::ir::IrSpliceResumeMap>,
) -> bool {
    anchored_at(graph, preheader).into_iter().any(|id| {
        graph.node_opt(id).is_some_and(|node| {
            let resumes_in_loop = node
                .frame_snapshot
                .and_then(|si| graph.safepoints.get(si as usize))
                .is_some_and(|sp| sp.bci >= header_bci && !sp.is_splice_state());
            // JIT r23 w2c close: another loop-entry predicate of a SPLICED
            // loop (`Graph::chain_predicate_guards`) resumes at that loop's
            // entry too -- through its chain frame at the header's combined
            // pc, the frame this predicate would name.
            // The r23 L8 heap-buffer close (`CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_SPLICED_EXACT`):
            // any node whose frame IS that loop-entry chain (the field
            // forward's null guards on a spliced loop name it through
            // `loop_field_forward::preheader_snapshot`) resumes there too.
            let at_entry_chain = node
                .frame_snapshot
                .and_then(|si| graph.safepoints.get(si as usize))
                .is_some_and(|sp| sp.bci == header_bci && sp.is_guard_splice_state());
            let resumes_in_loop = resumes_in_loop
                || (at_entry_chain
                    && (graph.chain_predicate_guards.iter().any(|&(g, _)| g == id)
                        || class_predicate::spliced_exact_forward_enabled()));
            let spliced_hoist = splices.is_some_and(|m| {
                spliced_hoist_trap::hoisted_from_spliced_loop(graph, node, header_bci, m)
            });
            !node.op.is_control()
                && !resumes_in_loop
                && !spliced_hoist
                && !matches!(node.op, Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead)
                && !direct_static_read_cannot_raise(graph, node)
                && may_raise(&node.op)
        })
    })
}

/// `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_CHAIN` (JIT round 23 wave 4, lane
/// L1; perf, **default ON**, `=0` kill): the null-base load predicates for a
/// loop SPLICED from a callee too -- PF's `plain` inside `main`'s OSR body, the
/// shape every lambda-driven row of the type-check probe has. The guard's
/// frame is the loop-entry CHAIN ([`loop_entry_chain_frame`]): the callee at
/// its own loop header with the entry values, every caller parked at its
/// invoke. A failure resumes through that chain (an OSR body's planless chain
/// transfer, a method-entry body's chain deopt), and the interpreter runs the
/// callee's loop from its first test. In an OSR graph only for a loop no OSR
/// entry can bypass ([`spliced_loop_entered_only_through_its_pre_header`]).
/// Its withdrawal is the call site's chain id
/// ([`crate::deopt::chain_predicate_withdrawn`]). Read per compile.
pub(crate) fn null_base_load_predicate_chain_enabled() -> bool {
    true
}

/// Is `region` a loop header spliced from a callee: its pc has a guard-usable
/// chain snapshot and no frame of the compiling method?
pub(in crate::ir_optimize) fn header_is_spliced(graph: &Graph, region: NodeId) -> bool {
    let Some(pc) = graph.node_opt(region).and_then(|r| r.bytecode_pc) else {
        return false;
    };
    let mut chain = false;
    for s in &graph.safepoints {
        if s.bci != pc {
            continue;
        }
        if !s.is_splice_state() {
            return false;
        }
        chain |= s.is_guard_splice_state();
    }
    chain
}

/// The snapshot index of the loop-entry chain frame an earlier chain predicate
/// guard of the loop at combined pc `header_bci` names: a live
/// `Guard { bci: header_bci }` anchored at `entry_pred`, listed in
/// `Graph::chain_predicate_guards`, whose frame is a guard-usable chain
/// snapshot at that pc. Ungated (the frame is the same one
/// [`loop_entry_chain_frame`] would build: it was built by it).
pub(in crate::ir_optimize) fn chain_predicate_guard_frame(graph: &Graph, entry_pred: NodeId, header_bci: usize) -> Option<u32> {
    graph.chain_predicate_guards.iter().find_map(|&(g, _)| {
        let n = graph.node_opt(g)?;
        if !matches!(n.op, Op::Guard { bci } if bci == header_bci)
            || n.inputs.first() != Some(&entry_pred)
        {
            return None;
        }
        let si = n.frame_snapshot?;
        let sp = graph.safepoints.get(si as usize)?;
        (sp.is_guard_splice_state() && sp.bci == header_bci).then_some(si)
    })
}

