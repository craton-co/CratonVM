#[allow(unused_imports)]
use super::*;

/// Round nio20 wave 5 (lane L3): `CRATONVM_JIT_IR_UNROLL_PURE_BODY`, **default
/// ON** (`0` restores the old refusal). Lets the FULL unroller take a loop a
/// safepoint snapshot names when the loop ITSELF cannot deopt, instead of
/// asking whether the whole METHOD can (`graph_cannot_deopt`, which one call or
/// array access anywhere in the method makes false -- the `graph trap-free:
/// false` that `CRATONVM_DBG_UNROLL` printed for every graph of
/// `Nio20FenceRow`). See [`pure_body_frames_unconsultable`] for the argument and
/// [`take_unroll_stranded_bcis`] for the lowering's check of it.
pub(in crate::ir_optimize) fn unroll_pure_body_enabled() -> bool {
    true
}

thread_local! {
    /// The flat bcis whose snapshots a pure-body full unroll left naming killed
    /// nodes (round nio20 wave 5, lane L3). The NET behind
    /// [`pure_body_frames_unconsultable`]'s prediction that nothing transfers
    /// at them: `ir_lower` refuses the compile if its emission recorded a
    /// transfer at one. Cleared at the top of [`optimize_with`], appended when
    /// such an unroll commits.
    pub(in crate::ir_optimize) static UNROLL_STRANDED_BCIS: std::cell::RefCell<StrandedFrames> =
        const { std::cell::RefCell::new(StrandedFrames::new()) };
}

/// What [`UNROLL_STRANDED_BCIS`] holds for one compile: the FLAT bcis a
/// rewrite left naming killed nodes, and (LOOPS22-2, a retired SPLICED loop)
/// the combined pcs of the CHAIN snapshots it left so. One channel, two key
/// spaces: a flat snapshot is consulted by its resume bci, a chain snapshot by
/// the raw combined pc of the site that publishes it
/// (`ir_lower`'s `splice_chain_for_site`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StrandedFrames {
    /// Flat snapshot bcis (`by_bci_transfer_bcis` is the lowering's check).
    pub flat: Vec<usize>,
    /// Chain snapshot combined pcs (`chain_transfer_pcs` is the check).
    pub chain: Vec<usize>,
}

impl StrandedFrames {
    pub(in crate::ir_optimize) const fn new() -> Self {
        Self {
            flat: Vec::new(),
            chain: Vec::new(),
        }
    }
}

/// Read and clear [`UNROLL_STRANDED_BCIS`], both key spaces.
pub fn take_unroll_stranded_frames() -> StrandedFrames {
    UNROLL_STRANDED_BCIS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// Read and clear [`UNROLL_STRANDED_BCIS`]; the flat bcis.
pub fn take_unroll_stranded_bcis() -> Vec<usize> {
    take_unroll_stranded_frames().flat
}

/// `CRATONVM_JIT_IR_UNROLL_NAMED_FRAME_PCS` (JIT round 20 wave 19, lane
/// yLoopPred, LOOPOPT20-6 of `docs/feature-designs/jit-ir-optimizations.md`;
/// `jr20-fence-row-pure-body-unroll-refused-by-header-bci-frames`) -- **default
/// ON**, `=0` restores the old rule and the old net. The pure-body escape
/// ([`pure_body_frames_unconsultable`]) stops counting as a reach of the FLAT
/// snapshot at a pc the pc of a node that never reads that snapshot
/// ([`pc_reads_no_flat_frame`]): a loop-entry predicate (`Op::Guard` and its
/// `ArrayLength`, planted at the header bci under their own named loop-entry
/// frame) and a direct `getstatic` (no transfer at all), so `fenceRow`'s
/// early mask guard and its hoisted `PLAIN` read no longer keep the header /
/// `getstatic` snapshots (which name the carried `k`, `h`) reachable. Asked
/// only where the old rule refuses ([`pure_body_frames_verdict`]); an unroll
/// it admits also strands the flat bcis whose snapshot names a carried phi
/// inside the body's pc range (rule (3)'s population), and the lowering's net
/// refuses the compile if anything transfers at a stranded bci through a
/// FLAT frame -- a call or pad exit at the carried `cur_bci`, an unnamed
/// guard, a check whose node names no frame there
/// (`ir_lower::lower_inner`, `Lowerer::by_bci_transfer_bcis`). A transfer
/// through the node's own named frame reads that frame, never the flat one
/// (`Lowerer::resolve_frame_state_for_site`), which is what lets the net
/// ignore it. Read per loop (no `static`).
pub(crate) fn unroll_named_frame_pcs_enabled() -> bool {
    true
}

/// LOOPOPT20-6: does node `n` -- one the unroll keeps -- never read the flat
/// snapshot at its own pc? Three shapes:
///
/// * a DATA node that cannot deopt at all (`ir_lower::node_cannot_deopt`, the
///   predicate the emission reads): the early guard's `Cmp`, which
///   `install_range_predicates` also stamps with the header bci, and every
///   other pure node with a pc. Its pc is a transfer point for nothing but the
///   pc-less nodes after it that inherit it as `cur_bci`, which
///   [`pure_body_frames_unconsultable`] refuses separately;
/// * a loop-entry predicate's `Op::Guard { bci }` or `Op::ArrayLength` whose
///   pc is `bci` and which names its own snapshot at that pc
///   (`Node::frame_snapshot`, the frame `install_range_predicates` gives
///   both): the guard boxes that frame, and the length's null check resolves
///   it through `cur_node_frame` before the flat scan;
/// * a non-volatile full-form `Op::LoadStatic` the lowering reads directly
///   ([`static_read_is_direct`]): two plain loads, no `<clinit>`, no call, no
///   deopt, so no transfer at all.
///
/// The claim is a prediction; the lowering's by-bci transfer net is its check.
pub(in crate::ir_optimize) fn pc_reads_no_flat_frame(graph: &Graph, n: &Node) -> bool {
    let Some(pc) = n.bytecode_pc else {
        return false;
    };
    // Never a CONTROL node: a block's control pc is where an ordinary OSR
    // entry stub seeds the frame from the flat snapshot
    // (`ir_lower::Lowerer::emit_osr_entry_stubs`), which is no transfer and
    // so no part of the lowering's net.
    if !n.op.is_control() && crate::ir_lower::node_cannot_deopt(graph, n) {
        return true;
    }
    let names_own_frame_here = || {
        n.frame_snapshot
            .and_then(|si| graph.safepoints.get(si as usize))
            .is_some_and(|sp| sp.bci == pc && !sp.is_splice_state())
    };
    match n.op {
        Op::Guard { bci } => bci == pc && names_own_frame_here(),
        Op::ArrayLength => names_own_frame_here(),
        Op::LoadStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        } => {
            static_read_is_direct(graph, class_id, field_index)
                && crate::ir::memory_token_slot(n).is_some()
        }
        _ => false,
    }
}

/// May a FULL unroll of a loop whose body a safepoint snapshot names proceed
/// without per-copy frames, because no snapshot it would leave describing a
/// killed node can ever be consulted? Round nio20 wave 5 (lane L3).
///
/// `killed` is every node the transform retires (the clone set, the carried
/// phis, the header, its test and both projections). The copies are emitted
/// with NO bytecode pc when this answers `true`, like the main/post unroller's
/// copies, so after the transform a snapshot at a body bci is reachable only
/// through a node that is NOT killed. The answer is `true` exactly when:
///
/// 1. every node of the clone set cannot deopt (`ir_lower::node_cannot_deopt`,
///    the predicate the emission reads): a copy is never a transfer site, and
///    a pc-less copy never sets the lowerer's carried `cur_bci`;
/// 2. no snapshot that names a CLONE is reachable from a surviving node: none
///    carries its bci as `bytecode_pc` or as an `Op::Guard` bci (directly or
///    through the splice resume map), claims it by `frame_snapshot`, and it is
///    not a post-call chain snapshot. A pc-less surviving node transfers at the
///    carried `cur_bci`, which is always a surviving node's pc, so (2) covers
///    it too;
/// 3. a snapshot that names a CARRIED phi (repaired by the teardown to the
///    loop's final value -- right after the loop, wrong inside it) and is
///    reachable as in (2) lies outside the body's pc range.
///
/// Conservative in every direction it can be: an unresolvable spliced pc
/// answers `false`, and a reachable snapshot naming a clone is refused even if
/// the reaching node could not deopt.
///
/// JIT r20 wave 19 (LOOPOPT20-6, [`unroll_named_frame_pcs_enabled`]): in (2),
/// the pc of a node [`pc_reads_no_flat_frame`] answers for is not a reach --
/// provided no surviving pc-less node can deopt (it would inherit that pc as
/// the lowerer's `cur_bci`).
pub(in crate::ir_optimize) fn pure_body_frames_unconsultable(
    graph: &Graph,
    killed: &FxHashSet<NodeId>,
    to_clone: &FxHashSet<NodeId>,
    carried_set: &FxHashSet<NodeId>,
) -> bool {
    // LOOPOPT20-6: the refined rule is asked only where the old one refuses,
    // so a loop the old rule admitted keeps exactly its old answer (the
    // refinement's own proviso -- no pc-less node may deopt -- is a new
    // condition the old rule did not have).
    pure_body_frames_verdict(graph, killed, to_clone, carried_set).is_some()
}

/// [`pure_body_frames_unconsultable`] with the rule that answered:
/// `Some(false)` the old rule, `Some(true)` only the LOOPOPT20-6 refinement
/// (the unroll then strands rule (3)'s population too), `None` refused.
pub(in crate::ir_optimize) fn pure_body_frames_verdict(
    graph: &Graph,
    killed: &FxHashSet<NodeId>,
    to_clone: &FxHashSet<NodeId>,
    carried_set: &FxHashSet<NodeId>,
) -> Option<bool> {
    if pure_body_frames_check(graph, killed, to_clone, carried_set, false) {
        return Some(false);
    }
    (unroll_named_frame_pcs_enabled()
        && pure_body_frames_check(graph, killed, to_clone, carried_set, true))
    .then_some(true)
}

/// [`pure_body_frames_unconsultable`]'s rules, with (`named_frame_pcs`) or
/// without the LOOPOPT20-6 refinement of rule (2).
pub(in crate::ir_optimize) fn pure_body_frames_check(
    graph: &Graph,
    killed: &FxHashSet<NodeId>,
    to_clone: &FxHashSet<NodeId>,
    carried_set: &FxHashSet<NodeId>,
    named_frame_pcs: bool,
) -> bool {
    for &c in to_clone {
        match graph.nodes.get(c as usize) {
            Some(n) if crate::ir_lower::node_cannot_deopt(graph, n) => {}
            _ => return false,
        }
    }
    // The body's pc range: every pc a retired node carries.
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    for &k in killed {
        if let Some(pc) = graph.nodes.get(k as usize).and_then(|n| n.bytecode_pc) {
            lo = lo.min(pc);
            hi = hi.max(pc);
        }
    }
    let splices = crate::ir::splice_resume_map_this_build();
    let mut reached: FxHashSet<usize> = FxHashSet::default();
    let mut claimed: FxHashSet<u32> = FxHashSet::default();
    // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-6): see
    // [`unroll_named_frame_pcs_enabled`].
    let mut skipped_a_pc = false;
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Dead || killed.contains(&(id as NodeId)) {
            continue;
        }
        if let Some(si) = n.frame_snapshot {
            claimed.insert(si);
        }
        if named_frame_pcs && pc_reads_no_flat_frame(graph, n) {
            skipped_a_pc = true;
            continue;
        }
        let mut pcs: [Option<usize>; 2] = [n.bytecode_pc, None];
        if let Op::Guard { bci } = n.op {
            pcs[1] = Some(bci);
        }
        for pc in pcs.into_iter().flatten() {
            reached.insert(pc);
            if let Some(map) = splices.as_ref() {
                match map.resume_bci(pc) {
                    Some(r) => {
                        reached.insert(r);
                    }
                    None => return false,
                }
            }
        }
    }
    // LOOPOPT20-6: a pc a skipped node carries is still the lowerer's carried
    // `cur_bci` for the pc-less nodes emitted after it, so none of those may
    // transfer (rule (2)'s own premise, restated for the skipped pcs). The
    // lowering's net checks the same thing against the emission
    // (`take_unroll_stranded_bcis`, the by-bci transfers).
    if skipped_a_pc
        && graph.nodes.iter().enumerate().any(|(id, n)| {
            n.op != Op::Dead
                && n.bytecode_pc.is_none()
                && !killed.contains(&(id as NodeId))
                && !crate::ir_lower::node_cannot_deopt(graph, n)
        })
    {
        return false;
    }
    for (si, sp) in graph.safepoints.iter().enumerate() {
        let names = |set: &FxHashSet<NodeId>| {
            sp.locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .any(|&s| s != NO_NODE && set.contains(&s))
        };
        let names_clone = names(to_clone);
        let names_carried = names(carried_set);
        if !names_clone && !names_carried {
            continue;
        }
        let reachable = reached.contains(&sp.bci)
            || claimed.contains(&(si as u32))
            || sp.post_call_of != NO_NODE;
        if !reachable {
            continue;
        }
        if names_clone {
            return false;
        }
        if lo <= hi && (lo..=hi).contains(&sp.bci) {
            return false;
        }
    }
    true
}

pub(in crate::ir_optimize) fn const_i64(graph: &Graph, id: NodeId) -> Option<i64> {
    match graph.node_opt(id)?.op {
        Op::Const(c) => Some(c),
        _ => None,
    }
}

pub(in crate::ir_optimize) fn eval_cmp_i64(op: CmpOp, x: i64, y: i64) -> bool {
    match op {
        CmpOp::Eq => x == y,
        CmpOp::Ne => x != y,
        CmpOp::Lt => x < y,
        CmpOp::Le => x <= y,
        CmpOp::Gt => x > y,
        CmpOp::Ge => x >= y,
    }
}

/// Why [`analyze_counted_loop`] declined, when it did.
///
/// `RuntimeBound` is split out from every other refusal because it is the only
/// one that is a MISSING FEATURE rather than a shape this pass does not model:
/// the loop is a textbook counted loop in every respect except that its bound
/// is a value. Full unrolling can never serve it — the trip count is not known
/// until the loop runs — and partial unrolling is what would. Counting the two
/// together would hide the entire partial-unroll population inside "not a
/// counted loop", which is how a census reports a prize as a non-event.
pub(in crate::ir_optimize) enum NotCounted {
    /// No constant-stride induction phi, no single exit test, or a shape this
    /// pass does not model.
    Shape,
    /// A counted loop against a NON-CONSTANT bound.
    ///
    /// Carries the shape a PARTIAL unroller needs, which is everything
    /// [`CountedLoop`] holds except the three facts that depend on the bound
    /// being constant (`iv_init`, `iv_stride`, `trip`). Partial unrolling never
    /// evaluates the induction variable — it keeps the loop test in every copy
    /// and lets the hardware decide — so it needs none of them.
    RuntimeBound(RuntimeBoundLoop),
    /// A counted loop whose constant trip count exceeds `UNROLL_MAX_TRIP`
    /// (`UNROLL_MAX_TRIP_WIDE` under `CRATONVM_JIT_IR_UNROLL_WIDE_COUNTED`).
    ///
    /// Carries the same shape as [`Self::RuntimeBound`] (round nio20 wave 5,
    /// lane L3): a constant bound is a loop-invariant one, so the main/post
    /// partial unroller can serve the loop the full unroller cannot.
    TripOverCap(RuntimeBoundLoop),
}

/// A counted loop whose bound is a runtime value — the population
/// [`partial_unroll_loop`] serves, and the one the census on
/// `c2-unrolling-is-a-deopt-metadata-problem-20260911.md` found to be **every**
/// counted loop in both benchmark suites.
///
/// Recognised to exactly the same standard as [`CountedLoop`]: one
/// constant-stride induction phi, one loop-exit `If`, and a back edge that is
/// one of that `If`'s two projections. The only thing missing is a trip count,
/// and the transform that consumes this does not want one.
pub(in crate::ir_optimize) struct RuntimeBoundLoop {
    pub(in crate::ir_optimize) if_node: NodeId,
    pub(in crate::ir_optimize) exit_ctrl: NodeId,
}

/// A recognised constant-trip counted loop suitable for full unrolling.
pub(in crate::ir_optimize) struct CountedLoop {
    pub(in crate::ir_optimize) iv_phi: NodeId,
    pub(in crate::ir_optimize) iv_init: i64,
    pub(in crate::ir_optimize) iv_stride: i64,
    pub(in crate::ir_optimize) trip: i64,
    pub(in crate::ir_optimize) if_node: NodeId,
    pub(in crate::ir_optimize) exit_ctrl: NodeId,
}

/// The single `Op::Proj(which)` user of `node`, or `NO_NODE`.
pub(in crate::ir_optimize) fn proj_user(graph: &Graph, node: NodeId, which: u16) -> NodeId {
    anchored_at(graph, node)
        .into_iter()
        .find(|&id| graph.node_opt(id).is_some_and(|n| n.op == Op::Proj(which)))
        .unwrap_or(NO_NODE)
}

/// Past this many users of a control node, [`anchored_at`] scans the arena
/// instead: `Graph::users_of` de-duplicates in O(users²).
pub(in crate::ir_optimize) const ANCHOR_INDEX_MAX_USERS: u32 = 256;

/// The live nodes whose slot 0 (the control anchor) is `ctrl`, ascending by
/// id — exactly what the whole-arena scans `n.inputs.first() == Some(&ctrl)`
/// answered, in the same order.
///
/// Round 12 wave 3 (iropt proposal W2-3, first slice): the per-header queries
/// (`proj_user`, `preheader_may_trap`, `control_token_has_guard`,
/// `LoopGuards::at_control`, `loop_entry_memory`, the header scans of
/// `analyze_counted_loop` and `range_predicate_plan`) each walked every node,
/// once or twice per loop header per LICM round. When the graph's maintained
/// def-use lists are current they are read instead, O(users of `ctrl`); stale
/// lists (or a hub with very many users) take the old scan, so the answer never
/// depends on which path ran.
pub(in crate::ir_optimize) fn anchored_at(graph: &Graph, ctrl: NodeId) -> Vec<NodeId> {
    if !graph.is_valid_id(ctrl) {
        return Vec::new();
    }
    let anchored = |id: NodeId| {
        graph
            .node_opt(id)
            .is_some_and(|n| n.op != Op::Dead && n.inputs.first() == Some(&ctrl))
    };
    let indexed = graph.use_lists_valid() && graph.use_count(ctrl) <= ANCHOR_INDEX_MAX_USERS;
    let mut out: Vec<NodeId> = if indexed {
        let mut users = graph.users_of(ctrl);
        users.retain(|&u| anchored(u));
        users.sort_unstable();
        users
    } else {
        (0..graph.nodes.len() as NodeId)
            .filter(|&id| anchored(id))
            .collect()
    };
    out.dedup();
    debug_assert_eq!(
        out,
        (0..graph.nodes.len() as NodeId)
            .filter(|&id| anchored(id))
            .collect::<Vec<_>>(),
        "anchored_at: the use-list answer diverged from the arena scan"
    );
    out
}

/// Users map: `users[id]` = nodes that reference `id` as an input.
pub(crate) fn build_users(graph: &Graph) -> Vec<Vec<NodeId>> {
    let mut users: Vec<Vec<NodeId>> = vec![Vec::new(); graph.nodes.len()];
    for (id, node) in graph.nodes.iter().enumerate() {
        for &inp in &node.inputs {
            if (inp as usize) < users.len() {
                users[inp as usize].push(id as NodeId);
            }
        }
    }
    users
}

/// Recognise a constant-trip counted loop at `region` (single back-edge
/// `back_ctrl`). Conservative: `None` on any shape we don't fully model
/// (non-constant init/stride/bound, induction tested against a non-constant,
/// multiple header tests, …).
pub(in crate::ir_optimize) fn analyze_counted_loop(
    graph: &Graph,
    region: NodeId,
    back_ctrl: NodeId,
) -> Result<CountedLoop, NotCounted> {
    let dbg = false;
    let n = graph.nodes.len();
    // Induction phi candidates: a Phi anchored at `region` with inputs
    // `[region, init(Const), next]` where `next = Add(self, Const)` or
    // `Add(Const, self)`.
    //
    // ALL of them, not the first one found.
    //
    // This loop used to `break` on its first hit and the exit test below then
    // had to name THAT phi or the whole loop was reported `NotCounted::Shape`.
    // Every shape with a second loop-carried counter lost both full and partial
    // unrolling to it: `for (int i = 0, j = 0; i < 8; i++, j++)` is enough, and
    // so is a plain accumulator `sum += 1`, or a strength-reduced pointer —
    // any phi with a constant init and a `phi + const` back edge qualifies.
    // Worse, WHICH one won was decided by node-id order, so the same source
    // could unroll or not depending on the order the builder happened to emit
    // its phis in.
    //
    // The choice is deferred to the exit test, which is the thing that decides
    // the trip count and therefore the only candidate that can be *the*
    // induction variable for unrolling purposes.
    //
    // Round nio20 wave 5 (lane L3), under `unroll_wide_counted_enabled`: the
    // entry / back-edge slots are DERIVED from the header's predecessor order
    // (the back edge is `back_ctrl`, the other input the entry), the init may
    // be any value (recorded as `None`: such a loop has no trip count and
    // leaves as runtime-bound below), and `phi - c` is a stride of `-c`. With
    // the flag off this is the old recogniser: slots 1 / 2, a constant init,
    // `Add` only.
    let wide = unroll_wide_counted_enabled();
    let (entry_slot, back_slot) = if wide {
        let rin = &graph.nodes[region as usize].inputs;
        match rin.iter().position(|&p| p == back_ctrl) {
            Some(b) if rin.len() == 2 => (2 - b, 1 + b),
            _ => (1, 2),
        }
    } else {
        (1, 2)
    };
    let mut iv_candidates: Vec<(NodeId, Option<i64>, i64)> = Vec::new();
    // The header's anchored nodes, not the arena (round 12 wave 3, W2-3).
    for id in anchored_at(graph, region) {
        let id = id as usize;
        let node = &graph.nodes[id];
        if node.op != Op::Phi
            || node.inputs.first().copied() != Some(region)
            || node.inputs.len() < 3
        {
            continue;
        }
        let init = match const_i64(graph, node.inputs[entry_slot]) {
            Some(c) => Some(c),
            None if wide => None,
            None => continue,
        };
        let nxt = node.inputs[back_slot];
        if nxt == NO_NODE || nxt as usize >= n {
            continue;
        }
        let is_sub = match graph.nodes[nxt as usize].op {
            Op::Add => false,
            Op::Sub if wide => true,
            _ => continue,
        };
        let ni = &graph.nodes[nxt as usize].inputs;
        if ni.len() != 2 {
            continue;
        }
        let stride = if ni[0] == id as NodeId {
            const_i64(graph, ni[1]).and_then(|c| if is_sub { c.checked_neg() } else { Some(c) })
        } else if ni[1] == id as NodeId && !is_sub {
            const_i64(graph, ni[0])
        } else {
            None
        };
        if let Some(s) = stride {
            if s != 0 {
                iv_candidates.push((id as NodeId, init, s));
            }
        }
    }
    if iv_candidates.is_empty() {
        if dbg {
            eprintln!("[DBG_UNROLL] region {region}: bail — no constant-stride induction phi");
        }
        return Err(NotCounted::Shape);
    }

    // Loop exit test: the single `If` controlled by a node inside the loop. We
    // accept the header test (`If.ctrl == region`) and the common bottom test
    // (`If.ctrl` chains back to the region via the back-edge control).
    let mut if_node = NO_NODE;
    let mut tests = anchored_at(graph, region);
    if back_ctrl != region {
        tests.extend(anchored_at(graph, back_ctrl));
    }
    for id in tests {
        let id = id as usize;
        let node = &graph.nodes[id];
        if node.op != Op::If {
            continue;
        }
        let c = node.inputs.first().copied().unwrap_or(NO_NODE);
        let in_loop = c == region || c == back_ctrl;
        if in_loop {
            if if_node != NO_NODE {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — multiple loop tests");
                }
                return Err(NotCounted::Shape);
            }
            if_node = id as NodeId;
        }
    }
    if if_node == NO_NODE {
        if dbg {
            eprintln!(
                "[DBG_UNROLL] region {region}: bail — no loop exit If (ctrl==region/back_ctrl)"
            );
        }
        return Err(NotCounted::Shape);
    }
    let cond = *graph.nodes[if_node as usize]
        .inputs
        .get(1)
        .ok_or(NotCounted::Shape)?;
    let op = match graph.nodes.get(cond as usize).ok_or(NotCounted::Shape)?.op {
        Op::Cmp(o) => o,
        _ => return Err(NotCounted::Shape),
    };
    let ci_raw = &graph.nodes[cond as usize].inputs;
    if ci_raw.len() != 2 {
        return Err(NotCounted::Shape);
    }
    // Round 9 wave 10 (irl10), `perf-long-counted-loops-lose-the-optimizing-osr-body`:
    // a `long` loop's test is `Cmp(cc, LCmp(i, n), 0)` -- javac emits
    // `lload; lload; lcmp; if<cc>` -- and until now it was reported as
    // "the exit test names none of the constant-stride phis", so no `long`
    // counted loop was ever recognised, for full or partial unrolling. Read
    // through the `lcmp`: `lcmp(a, b)` is exactly the sign of `a - b` (no
    // overflow), so `lcmp(a, b) cc 0` is `a cc b` and `0 cc lcmp(a, b)` is
    // `b cc a`, for all six (signed) `CmpOp`s. The `If` and its condition
    // are not rewritten; only the recognition (and the trip simulation,
    // which then evaluates `a cc b` at the induction variable's own `long`
    // width) reads the two long operands.
    let ci: [NodeId; 2] =
        lcmp_operands_of_zero_test(graph, ci_raw[0], ci_raw[1]).unwrap_or([ci_raw[0], ci_raw[1]]);
    // Compare the induction phi against a bound. A NON-CONSTANT bound is a
    // counted loop FULL unrolling cannot serve -- the trip count is not known
    // until the loop runs -- and is reported as such rather than as "not a
    // counted loop", because the two call for different work.
    // Now pick the induction variable: the candidate the exit test names. The
    // test is what bounds the loop, so a phi it does not mention cannot be the
    // one whose trip count the unroller needs, however constant its stride.
    //
    // BOTH operands naming a candidate — `for (i = 0, j = 3; i < j; i++, j++)`
    // — is the residue the "collect them all" fix left behind: `find` settled
    // that by node-id order, which is the very order-dependence collecting the
    // candidates was meant to remove. `iv_on_left` and the trip simulation both
    // read the winner, so a full unroll of an ambiguous test would be a trip
    // count decided by emission order.
    //
    // It is unreachable today — `other` is then a phi, `const_i64` answers
    // `None`, and the loop leaves as `NotCounted::RuntimeBound` before any of
    // that runs — but unreachable BY COINCIDENCE, through a property of a
    // different function. `ambiguous_iv` makes it unreachable by construction:
    // the full-unroll path is closed explicitly, so a `const_i64` that learns
    // to fold a phi (a single-input one, or a phi of identical constants —
    // both shapes GVN can produce) cannot quietly open it.
    //
    // NOT a refusal of the loop. The partial unroller reads neither the
    // induction variable nor the bound (it takes `if_node` and `exit_ctrl`
    // only, and `unroll` passes it `iv_phi: NO_NODE`), so the ambiguity does
    // not reach it and declining the shape outright would cost a transform for
    // nothing.
    let named: Vec<(NodeId, Option<i64>, i64)> = iv_candidates
        .iter()
        .copied()
        .filter(|&(p, _, _)| p == ci[0] || p == ci[1])
        .collect();
    let ambiguous_iv = named.len() > 1;
    if ambiguous_iv && dbg {
        eprintln!(
            "[DBG_UNROLL] region {region}: the exit test names {} constant-stride \
             phis — no full unroll, the bound is not a constant",
            named.len()
        );
    }
    let (iv_phi, iv_init, iv_stride) = match named.first().copied() {
        Some(v) => v,
        None => {
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: bail — the exit test names none of the \
                     {} constant-stride phi(s)",
                    iv_candidates.len()
                );
            }
            return Err(NotCounted::Shape);
        }
    };
    let (iv_on_left, other) = if ci[0] == iv_phi {
        (true, ci[1])
    } else {
        (false, ci[0])
    };
    // Read, but not yet acted on. The runtime-bound refusal is deferred to
    // below the exit-shape checks, because `NotCounted::RuntimeBound` now
    // carries that shape to the partial unroller: a loop reported as
    // "runtime bound" must have been recognised as completely as one reported
    // as counted, or the census counts a loop the transform would then refuse
    // for a second reason it never named.
    let bound_if_const = if ambiguous_iv {
        None
    } else {
        const_i64(graph, other)
    };

    // Which If projection re-enters the loop (the "continue" edge)?
    if graph
        .nodes
        .get(back_ctrl as usize)
        .ok_or(NotCounted::Shape)?
        .inputs
        .first()
        .copied()
        != Some(if_node)
    {
        return Err(NotCounted::Shape);
    }
    let back_is_true = match graph.nodes[back_ctrl as usize].op {
        Op::Proj(0) => true,
        Op::Proj(1) => false,
        _ => return Err(NotCounted::Shape),
    };
    let exit_ctrl = proj_user(graph, if_node, if back_is_true { 1 } else { 0 });
    if exit_ctrl == NO_NODE {
        return Err(NotCounted::Shape);
    }

    // The deferred refusal, now that the shape a partial unroll needs is known.
    let bound = match bound_if_const {
        Some(b) => b,
        None => {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — bound is a runtime value");
            }
            return Err(NotCounted::RuntimeBound(RuntimeBoundLoop {
                if_node,
                exit_ctrl,
            }));
        }
    };
    // Round nio20 wave 5 (lane L3): a non-constant ENTRY value (only ever
    // admitted under `unroll_wide_counted_enabled`) leaves the trip count as
    // unknown as a non-constant bound does, and goes the same way.
    let iv_init = match iv_init {
        Some(v) => v,
        None => {
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: bail — the induction variable's entry \
                     value is a runtime value"
                );
            }
            return Err(NotCounted::RuntimeBound(RuntimeBoundLoop {
                if_node,
                exit_ctrl,
            }));
        }
    };
    let trip_cap = if wide {
        UNROLL_MAX_TRIP_WIDE
    } else {
        UNROLL_MAX_TRIP
    };

    // Concrete trip count by simulation (init/stride/bound all constant).
    //
    // At the induction variable's own width. A Java `int` IV wraps at 2^31,
    // so simulating it in i64 gave the wrong count for a loop that steps past
    // `Integer.MAX_VALUE` (`for (int i = 0x7FFFFFE8; i < 0x7FFFFFF0; i += 0x18)`
    // runs 178956971 times in Java — it wraps negative and keeps going — and
    // was unrolled to ONE trip), and the replacement constants materialised
    // below lay outside the int range. A wrapping int run is refused outright:
    // the only loops worth unrolling have small, non-wrapping trip counts.
    let iv_is_int = matches!(graph.nodes[iv_phi as usize].ty, IrType::Int);
    if iv_is_int
        && [iv_init, iv_stride, bound]
            .iter()
            .any(|&v| i64::from(v as i32) != v)
    {
        return Err(NotCounted::Shape);
    }
    let mut i = iv_init;
    let mut trip = 0i64;
    loop {
        let raw = if iv_on_left {
            eval_cmp_i64(op, i, bound)
        } else {
            eval_cmp_i64(op, bound, i)
        };
        let cont = if back_is_true { raw } else { !raw };
        if !cont {
            break;
        }
        i = if iv_is_int {
            // Cast: all three operands checked to be exact i32 values above
            match (i as i32).checked_add(iv_stride as i32) {
                Some(next) => i64::from(next),
                None => return Err(NotCounted::Shape),
            }
        } else {
            match i.checked_add(iv_stride) {
                Some(next) => next,
                None => return Err(NotCounted::Shape),
            }
        };
        trip += 1;
        if trip > trip_cap {
            // Too large, or non-terminating within the cap.
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: bail — constant trip count over {trip_cap}"
                );
            }
            return Err(NotCounted::TripOverCap(RuntimeBoundLoop {
                if_node,
                exit_ctrl,
            }));
        }
    }
    Ok(CountedLoop {
        iv_phi,
        iv_init,
        iv_stride,
        trip,
        if_node,
        exit_ctrl,
    })
}

/// `Some([a, b])` when a compare `x cc y` is the zero test of a `long`
/// three-way compare, i.e. `x = LCmp(a, b)` and `y = 0` (answers `[a, b]`), or
/// `x = 0` and `y = LCmp(a, b)` (answers `[b, a]`): in both cases `x cc y` holds
/// exactly when `first cc second` does, for every signed `CmpOp`, because
/// `lcmp` is the sign of `a - b` computed without overflow. `None` for any
/// other shape (an `FCmp` is NOT read through: its NaN bias makes the
/// three-way value differ from a plain compare). Round 9 wave 10 (irl10).
pub(in crate::ir_optimize) fn lcmp_operands_of_zero_test(
    graph: &Graph,
    x: NodeId,
    y: NodeId,
) -> Option<[NodeId; 2]> {
    let lcmp_ops = |id: NodeId| -> Option<[NodeId; 2]> {
        let n = graph.nodes.get(id as usize)?;
        if n.op != Op::LCmp || n.inputs.len() != 2 {
            return None;
        }
        Some([n.inputs[0], n.inputs[1]])
    };
    if const_i64(graph, y) == Some(0) {
        if let Some([a, b]) = lcmp_ops(x) {
            return Some([a, b]);
        }
    }
    if const_i64(graph, x) == Some(0) {
        if let Some([a, b]) = lcmp_ops(y) {
            return Some([b, a]);
        }
    }
    None
}

/// Collapse trivial single-input `Op::Merge` nodes (control pass-throughs the
/// builder emits around branch projections) by forwarding their users to the
/// single input. Run before loop detection so a loop header and its back-edge
/// sit adjacent to the `If`/`Proj` nodes (the javac loop shape wraps the If's
/// projections in single-input Merges).
/// True when collapsing a successor control node `succ` into its single
/// predecessor `pred` (`replace_all_uses(succ, pred)`) would invert the
/// intra-block order of anchored side effects.
///
/// Within a single basic block (`ctrl`), `ir_schedule` chains impure nodes
/// (`!op.is_pure() && !op.is_control() && op != Op::Phi && op != Op::Dead` with
/// `input_opt(0) == Some(ctrl)`) in ascending `NodeId` order. When a later pass
/// (such as loop-predicate planting, LICM, or loop unrolling) allocates a new
/// effect node with a higher `NodeId` on `pred`, and subsequent pruning or
/// unrolling leaves `succ` with `pred` as its sole predecessor, forwarding
/// `succ`'s lower-`NodeId` effects onto `pred` places them ahead of `pred`'s
/// higher-`NodeId` effects in `ir_schedule`.
pub(crate) fn collapse_would_invert_effects(graph: &Graph, pred: NodeId, succ: NodeId) -> bool {
    if pred == NO_NODE || succ == NO_NODE || pred == succ {
        return false;
    }
    let mut max_pred: Option<NodeId> = None;
    let mut min_succ: Option<NodeId> = None;
    for (idx, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Dead || n.op == Op::Phi || n.op.is_control() || n.op.is_pure() {
            continue;
        }
        let Some(ctrl) = n.input_opt(0) else {
            continue;
        };
        let id = idx as NodeId;
        if ctrl == pred {
            max_pred = Some(max_pred.map_or(id, |m| m.max(id)));
        } else if ctrl == succ {
            min_succ = Some(min_succ.map_or(id, |m| m.min(id)));
        }
    }
    matches!((max_pred, min_succ), (Some(mp), Some(ms)) if mp > ms)
}

pub(in crate::ir_optimize) fn collapse_trivial_merges(graph: &mut Graph) {
    let n_len = graph.nodes.len();
    let mut eff_bounds = vec![(NodeId::MAX, 0 as NodeId); n_len];
    for (idx, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Dead || n.op == Op::Phi || n.op.is_control() || n.op.is_pure() {
            continue;
        }
        if let Some(ctrl) = n.input_opt(0) {
            if let Some(slot) = eff_bounds.get_mut(ctrl as usize) {
                let id = idx as NodeId;
                slot.0 = slot.0.min(id);
                slot.1 = slot.1.max(id);
            }
        }
    }
    loop {
        let mut changed = false;
        for id in 0..n_len {
            if graph.nodes[id].op == Op::Merge && graph.nodes[id].inputs.len() == 1 {
                let inp = graph.nodes[id].inputs[0];
                let inp_idx = inp as usize;
                if inp != NO_NODE && inp_idx != id {
                    let (pred_min, pred_max) =
                        eff_bounds.get(inp_idx).copied().unwrap_or((NodeId::MAX, 0));
                    let (succ_min, succ_max) =
                        eff_bounds.get(id).copied().unwrap_or((NodeId::MAX, 0));
                    let pred_has_eff = pred_min <= pred_max;
                    let succ_has_eff = succ_min <= succ_max;
                    if pred_has_eff && succ_has_eff && pred_max > succ_min {
                        continue;
                    }
                    graph.replace_all_uses(id as NodeId, inp);
                    graph.kill(id as NodeId);
                    if succ_has_eff {
                        if let Some(slot) = eff_bounds.get_mut(inp_idx) {
                            slot.0 = slot.0.min(succ_min);
                            slot.1 = slot.1.max(succ_max);
                        }
                        if let Some(slot) = eff_bounds.get_mut(id) {
                            *slot = (NodeId::MAX, 0);
                        }
                    }
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Control nodes forward-reachable from `start` (following control-typed users).
pub(in crate::ir_optimize) fn forward_control_closure(
    graph: &Graph,
    users: &[Vec<NodeId>],
    start: NodeId,
) -> FxHashSet<NodeId> {
    let mut set = FxHashSet::default();
    let mut work = vec![start];
    while let Some(c) = work.pop() {
        for &u in &users[c as usize] {
            if graph.nodes[u as usize].op.is_control() && set.insert(u) {
                work.push(u);
            }
        }
    }
    set
}

/// Would a partial unroller be able to clone this loop's body, and would the
/// clones be DEOPT-FREE?
///
/// # Census only — it transforms nothing
///
/// `unroll` answers this question for a loop it is about to unroll, as a side
/// effect of building the clone set. A loop refused earlier for
/// [`NotCounted::RuntimeBound`] never reaches that code, so the population a
/// PARTIAL unroller would serve is invisible in the census. This mirrors the
/// same walk to size it.
///
/// # The gate that matters is per-BODY, not per-graph
///
/// `unroll`'s existing escape from the safepoint refusal asks
/// `graph_cannot_deopt` — a WHOLE-METHOD property, false for any method
/// containing a single call, field access or array access anywhere. Measured
/// across CratonBench, CratonBenchC2 and the probe set, it is true for **zero**
/// of the counted loops found, so a partial unroller gated on it would fire on
/// nothing.
///
/// The obligation it stands in for is narrower. Cloning a body duplicates its
/// bcis, and `build_deopt_points` anchors a point at `bci_native[bci]`, which
/// keeps only the EARLIEST native offset for a bci — so copy 1..U-1 get no
/// deopt point at all, and a trap inside them has no frame to reconstruct.
/// That objection is about the CLONED NODES. If none of them can deopt, no
/// copy needs a point and the obligation is discharged whatever the rest of
/// the method does.
///
/// So this asks the narrow question, and [`UnrollCensus::runtime_bound_pure_body`]
/// is the answer.
pub(in crate::ir_optimize) fn partial_unroll_body_is_pure(
    graph: &Graph,
    users: &[Vec<NodeId>],
    region: NodeId,
    back_ctrl: NodeId,
    if_node: NodeId,
) -> bool {
    let mut carried: Vec<NodeId> = Vec::new();
    for id in 0..graph.nodes.len() {
        let node = &graph.nodes[id];
        if node.op == Op::Phi && node.inputs.first().copied() == Some(region) {
            if node.inputs.len() < 3 {
                return false;
            }
            carried.push(id as NodeId);
        }
    }
    if carried.is_empty() {
        return false;
    }
    let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();
    let mut variant: FxHashSet<NodeId> = carried_set.clone();
    let mut vw: Vec<NodeId> = carried.clone();
    while let Some(c) = vw.pop() {
        for &u in &users[c as usize] {
            if variant.insert(u) {
                vw.push(u);
            }
        }
    }
    // The same seeds the transform uses: the loop condition and each carried
    // phi's back-edge value.
    let cond = match graph.nodes[if_node as usize].inputs.get(1) {
        Some(&c) => c,
        None => return false,
    };
    let mut work: Vec<NodeId> = vec![cond];
    for &c in &carried {
        work.push(graph.nodes[c as usize].inputs[2]);
    }
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    while let Some(c) = work.pop() {
        if c == NO_NODE
            || c as usize >= graph.nodes.len()
            || carried_set.contains(&c)
            || !variant.contains(&c)
        {
            continue;
        }
        if graph.nodes[c as usize].op.is_control() || !seen.insert(c) {
            continue;
        }
        // PURE ONLY. `unroll` also admits `Op::Load`, because a full unroll
        // deletes the loop and its bcis with it; a PARTIAL unroll keeps them
        // and a cloned `Op::Load` can NPE at a bci whose only deopt point
        // belongs to copy 0.
        if !graph.nodes[c as usize].op.is_pure() {
            return false;
        }
        for &inp in &graph.nodes[c as usize].inputs {
            work.push(inp);
        }
    }
    !seen.is_empty() && seen.len() <= UNROLL_MAX_BODY
}

/// Fully unroll small constant-trip counted loops (gated by [`unroll_enabled`]).
/// Returns `true` if any loop was unrolled.
///
/// Soundness model: only a single-back-edge, single-block (no internal control),
/// side-effect-free (no Store/Call/alloc/Guard; loads only if loop-variant)
/// counted loop with a constant trip count `<= UNROLL_MAX_TRIP` is transformed.
/// The per-iteration computation is the set backward-reachable from each carried
/// phi's back-edge value and the loop condition — this is exactly the
/// loop-carried work and *excludes* post-loop uses. We clone that set once per
/// iteration with the induction variable substituted by its concrete constant
/// and each carried phi by its running value, then redirect post-loop uses of
/// each carried phi to its final value and straight-line the control. Anything
/// not matching this shape is left untouched.
/// May the unroller ignore a safepoint snapshot that names the loop body, when
/// the graph is trap-free and so no snapshot can be consulted? **Default ON**
/// since 2026-09-06; `CRATONVM_JIT_IR_UNROLL_UNREACHABLE_FRAMES=0` restores the
/// blanket refusal.
thread_local! {
    /// Set when [`unroll`] transformed a loop whose body IS named by a
    /// safepoint snapshot, on the strength of the graph being trap-free.
    ///
    /// This is a NET, not a diagnostic. The relaxation is a prediction, and the
    /// prediction is only sound while `build_deopt_points` actually skips those
    /// points -- which it does only when the EMISSION agrees the body is
    /// trap-free. If emission disagrees, every point is built and the slots the
    /// unroller killed resolve to `FrameValue::Undefined`, which is a deopt
    /// that silently loses interpreter locals rather than one that fails.
    ///
    /// `lower_inner` reads this and refuses the compile in that case. The
    /// method then takes the single-pass backend, which is what it did before
    /// the relaxation existed.
    pub(crate) static UNROLL_USED_UNREACHABLE_FRAMES: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Read and clear the net above.
pub fn take_unroll_used_unreachable_frames() -> bool {
    UNROLL_USED_UNREACHABLE_FRAMES.with(|c| c.replace(false))
}

pub(crate) fn unroll_over_unreachable_frames() -> bool {
    true
}

/// May the unroller give each copy of a cloned body its **own** deopt frames,
/// instead of refusing a loop a safepoint snapshot names? **Default ON** since
/// 2026-10-31; `CRATONVM_JIT_IR_PER_COPY_FRAMES=0` disables it.
///
/// # What it turns on
///
/// A cloned node keeps the original's `bytecode_pc`, so after an unroll there
/// are `trip` nodes at each body bci. `ir_lower` resolves a trapping node's
/// deopt frame by scanning `graph.safepoints` for the first snapshot with that
/// bci, so all `trip` copies resolve to copy 0's frame — which names copy 0's
/// values. Copies `1..trip` would then deopt into an interpreter frame holding
/// another iteration's locals.
///
/// With this on, [`install_copy_frames`] gives each copy a substituted
/// snapshot of its own and stamps the copy's nodes with
/// [`Node::frame_snapshot`], which `ir_lower` prefers over the scan.
///
/// # Why it is on
///
/// Per-copy frames passed the maintained deterministic checksum soak in both
/// runtime modes under Generational, G1, and ZGC, with no divergences across
/// 41 deterministic workloads per run. The LOOP27-1 probe also preserved its
/// checksum while the cloned volatile-store loop improved. `=0` remains the
/// fail-closed kill switch for differential checks.
pub(in crate::ir_optimize) fn ir_per_copy_frames_enabled() -> bool {
    // Read live rather than cached in a `OnceLock`, like
    // `ir_lower::ir_drop_unreachable_homes_enabled` and unlike its
    // `unroll_over_unreachable_frames` neighbour. Two reasons, and the second
    // is the one that matters: it is read once per candidate loop header, so
    // the cache buys nothing; and a process-wide cache makes the OFF arm
    // untestable in the same process as the ON arm, which is precisely the
    // differential this mechanism has to be held to.
    true
}

/// May the unroller PARTIALLY unroll a counted loop whose bound is a runtime
/// value? **Default OFF**; `CRATONVM_JIT_IR_PARTIAL_UNROLL=1` arms it.
///
/// Off for the same reason [`ir_per_copy_frames_enabled`] is, and it is the
/// same reason: this transform's correctness rests on that one, which has not
/// yet earned its default on a soak. A partial unroll that needs per-copy
/// frames and cannot have them is refused, so the two flags compose by
/// refusing rather than by miscompiling — but the population that needs them is
/// the population worth having, so arming this one alone buys little.
pub(in crate::ir_optimize) fn partial_unroll_enabled() -> bool {
    // Read live rather than cached, for [`ir_per_copy_frames_enabled`]'s second
    // reason: a process-wide cache makes the OFF arm untestable in the same
    // process as the ON arm, and the ON/OFF differential is how this is held.
    matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// How many copies of the body a partial unroll emits, counting the original.
///
/// 4 is the factor `c2-unrolling-is-a-deopt-metadata-problem-20260911.md` §3
/// priced: it removes `(U−1)/U × (poll 2 + back edge 1)` per iteration, which
/// at `U = 4` is 2.25 instructions of `FieldLoop.sum`'s 23. Past 4 the series
/// `(U−1)/U` flattens while code size keeps growing linearly, so the knob
/// exists to measure that rather than to be turned up.
///
/// Clamped to `2..=8`: 1 is not an unroll, and 8 copies of a body already
/// exceeds what [`UNROLL_MAX_BODY`] admits for anything but a tiny one.
pub(in crate::ir_optimize) fn partial_unroll_factor() -> usize {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(2, 8)
}

/// Which snapshots need a per-copy version, and what they looked like BEFORE
/// the unroll touched them. `Err` when this loop cannot have per-copy frames at
/// all.
///
/// # Three kinds of snapshot, and only one of them is a refusal
///
/// A copy's frame has to hang on a program point the copy actually has, and the
/// only program points a copy has are the bcis its cloned nodes carry. So:
///
///   * **at a cloned bci** — copied, one per iteration. This is the mechanism.
///   * **at a bci NO live node in the graph carries** — left exactly as it is,
///     and this is the common case rather than an edge one: `IrBuilder` records
///     a snapshot at *every* bytecode index, including the `istore` and `goto`
///     that close a javac loop body, and neither of those produces a node. Such
///     a snapshot cannot be consulted by anything. `build_deopt_points` anchors
///     through `bci_native`, which is populated only from nodes, so it is
///     skipped; and every deopt this file emits resumes at some node's own
///     `bytecode_pc`, so no guard names it either. Its slots are left to be
///     stranded and normalised to `NO_NODE` by `eliminate_dead_nodes`, which is
///     what already happens to every unconsultable snapshot on every compile.
///     Copying it instead would be worse than useless: safepoint slots are DCE
///     ROOTS, so `trip` copies of a frame nothing reads would keep every
///     iteration's intermediates alive to describe a program point with no
///     code.
///   * **at a bci some OTHER node carries** — refused
///     ([`UnrollCensus::frame_uncopyable`]). Here the code really is emitted,
///     really can be resumed at, and the frame names a body value with no
///     iteration to belong to. There is no substitution that is right, so the
///     loop is declined rather than given a frame picked from whichever
///     iteration was convenient.
///
/// A snapshot naming only CARRIED phis, at any bci, is fine and is not refused:
/// `unroll`'s teardown rewrites every use of a carried phi to its post-loop
/// value, and `Graph::replace_all_uses` routes safepoint slots, so those slots
/// are repaired by the transform itself.
///
/// # Why the snapshots are returned by value
///
/// Iteration 0 rewrites its snapshot IN PLACE (see [`install_copy_frames`]), so
/// by iteration 1 the entry in `graph.safepoints` no longer names the original
/// nodes the substitution has to start from. The pre-unroll copy returned here
/// is that starting point, and taking it once is also what keeps the borrow of
/// `graph` out of the mutating loop.
pub(crate) fn plan_copy_frames(
    graph: &Graph,
    to_clone: &FxHashSet<NodeId>,
) -> Result<Vec<(u32, SafepointSnapshot)>, ()> {
    plan_copy_frames_in(graph, to_clone, None)
}

/// `i2l(phi)`, the widening a narrowed `long` induction hands to its readers
/// (`long_induction_narrow`).
pub(in crate::ir_optimize) fn is_i2l_of_phi(graph: &Graph, id: NodeId) -> bool {
    graph.node_opt(id).is_some_and(|n| {
        n.op == Op::I2L
            && n.inputs
                .first()
                .and_then(|&p| graph.node_opt(p))
                .is_some_and(|p| p.op == Op::Phi)
    })
}

/// [`plan_copy_frames`], with the refusal's "some OTHER live node carries the
/// bci" asked only of the nodes in `in_loop` when given (JIT r23 w2c close,
/// the main/post deopt form). A node OUTSIDE the loop that carries a body bci
/// -- a loop-entry predicate's pre-header copy of a body read keeps the read's
/// pc for its field row -- is not a program point of any copy: the unroll
/// leaves the snapshots it would look up by that bci exactly as they were,
/// so it reads the same frame it read before the transform.
pub(in crate::ir_optimize) fn plan_copy_frames_in(
    graph: &Graph,
    to_clone: &FxHashSet<NodeId>,
    in_loop: Option<&FxHashSet<NodeId>>,
) -> Result<Vec<(u32, SafepointSnapshot)>, ()> {
    let mut cloned_bcis: FxHashSet<usize> = FxHashSet::default();
    for &d in to_clone {
        if let Some(pc) = graph.nodes[d as usize].bytecode_pc {
            cloned_bcis.insert(pc);
        }
    }
    // Every bci the emitted code will actually carry. Read from live nodes
    // only: a node the optimizer already killed emits nothing, so a bci carried
    // solely by one is as unreachable as a bci carried by nothing.
    let mut node_bcis: FxHashSet<usize> = FxHashSet::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        // Cast: arena index to node id.
        if in_loop.is_some_and(|s| !s.contains(&(id as NodeId))) {
            continue;
        }
        if let Some(pc) = node.bytecode_pc {
            node_bcis.insert(pc);
        }
    }
    // Round 13 wave 3 (lane framestate): the bcis a cloned node of a SPLICED
    // body resumes at when it has no chain frame at its own pc -- its outermost
    // `invoke` (`ir_lower::resume_bci`). That flat frame is a program point of
    // every copy too: a trap in copy k's spliced load resumes there, and before
    // this it was left unplanned, so copy k resumed with copy 0's (or, after
    // the teardown, the post-loop) values and a stranded `NO_NODE` for every
    // body value it named. Planned only when no UNCLONED live node carries the
    // bci (that node would then read a copy's frame); such a bci keeps the old
    // answer below.
    let splices = crate::ir::splice_resume_map_this_build();
    let mut cloned_resume_bcis: FxHashSet<usize> = FxHashSet::default();
    if let Some(map) = splices.as_ref() {
        for &d in to_clone {
            if let Some(pc) = graph.nodes[d as usize].bytecode_pc {
                match map.resume_bci(pc) {
                    Some(r) if r != pc => {
                        cloned_resume_bcis.insert(r);
                    }
                    _ => {}
                }
            }
        }
    }
    let mut uncloned_node_bcis: FxHashSet<usize> = FxHashSet::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead || to_clone.contains(&(id as NodeId)) {
            continue;
        }
        if let Some(pc) = node.bytecode_pc {
            uncloned_node_bcis.insert(pc);
        }
    }
    let mut first_flat_at: FxHashMap<usize, usize> = FxHashMap::default();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if !sp.is_splice_state() {
            first_flat_at.entry(sp.bci).or_insert(si);
        }
    }
    let mut plan: Vec<(u32, SafepointSnapshot)> = Vec::new();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if cloned_bcis.contains(&sp.bci) {
            plan.push((si as u32, sp.clone()));
            continue;
        }
        if cloned_resume_bcis.contains(&sp.bci)
            && first_flat_at.get(&sp.bci) == Some(&si)
            && !uncloned_node_bcis.contains(&sp.bci)
        {
            plan.push((si as u32, sp.clone()));
            continue;
        }
        let names_body = sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .any(|slot| {
                to_clone.contains(slot)
                    // JIT r27 LOOP27-2: a narrowed `long` induction's frames name
                    // `i2l(phi)`. At a bci only the post loop carries (main/post,
                    // `in_loop` given) that is the original node on the original
                    // loop; the copies' own frames rename it through `subst`.
                    && !(in_loop.is_some() && is_i2l_of_phi(graph, *slot))
            });
        if names_body && node_bcis.contains(&sp.bci) {
            return Err(());
        }
    }
    Ok(plan)
}

/// Give iteration `t` its own copy of every body snapshot, and stamp that
/// iteration's clones with it.
///
/// `subst` is iteration `t`'s substitution — body node → this iteration's
/// clone, carried phi → the value entering this iteration — which is exactly
/// the rename a frame at a body bci needs. A slot naming something outside the
/// loop is left alone, because a loop-invariant value is the same in every
/// copy.
///
/// # Iteration 0 rewrites in place; the rest append
///
/// Not a micro-optimization. `bci_native` anchors a bci at the EARLIEST native
/// offset emitted for it, which is copy 0's, and `build_deopt_points` falls
/// back to that anchor for any snapshot no node claims. If copy 0 got a fresh
/// appended snapshot and the original were left naming dead nodes, the original
/// would keep the copy-0 anchor and the table's point at that offset would
/// describe an iteration that no longer exists. Making the original BE copy 0's
/// frame keeps the anchor and its frame describing the same code.
///
/// # Ordering
///
/// Call this after iteration `t`'s clone loop and before the carried phis are
/// advanced: the substitution must be complete (a snapshot at bci B may name
/// any value live at B, which is any body node cloned at or before B), and it
/// must not yet contain iteration `t+1`'s induction constant.
pub(crate) fn install_copy_frames(
    graph: &mut Graph,
    plan: &[(u32, SafepointSnapshot)],
    subst: &FxHashMap<NodeId, NodeId>,
    iter_clones: &[NodeId],
    first_iteration: bool,
) {
    let marks = true;
    let per_copy = marks && unroll_post_call_per_copy_enabled();
    let mut snap_for_bci: FxHashMap<usize, u32> = FxHashMap::default();
    for (orig_si, orig) in plan {
        let rename = |n: &NodeId| subst.get(n).copied().unwrap_or(*n);
        let locals: Vec<NodeId> = orig.locals.iter().map(rename).collect();
        let stack: Vec<NodeId> = orig.stack.iter().map(rename).collect();
        let monitors: Vec<NodeId> = orig.monitors.iter().map(rename).collect();
        let si = if first_iteration {
            // Through `set_safepoint_slot`, not a direct write: installing a
            // reference is the one snapshot edit the def-use edges cannot
            // notice, and a stale use list is how a value a frame still names
            // gets collected.
            for (k, &v) in locals.iter().enumerate() {
                graph.set_safepoint_slot(*orig_si as usize, SafepointSlotKind::Local, k, v);
            }
            for (k, &v) in stack.iter().enumerate() {
                graph.set_safepoint_slot(*orig_si as usize, SafepointSlotKind::Stack, k, v);
            }
            for (k, &v) in monitors.iter().enumerate() {
                graph.set_safepoint_slot(*orig_si as usize, SafepointSlotKind::Monitor, k, v);
            }
            *orig_si
        } else {
            let si = graph.safepoints.len() as u32;
            let mut copy = SafepointSnapshot::new(orig.bci, locals, stack, monitors);
            // The copy is the same program point in another iteration: it
            // keeps the original's per-entry nested-lock marks (positional, so
            // valid for the renamed slots) and, for a snapshot inside a spliced
            // body, its scope chain (round 13 wave 3, lane framestate) -- a
            // chain snapshot copied without it would read as one flat frame of
            // the compiling method at a combined-buffer pc.
            copy.monitor_elided_mask = orig.monitor_elided_mask;
            copy.splice_scope = orig.splice_scope.clone();
            // JIT r23 w2c close: a frame of one copy alone, dropped once no
            // node of that copy is left (`drop_unnamed_copy_frames`).
            copy.copy_frame = true;
            // JIT round 19 wave 12, lane tail12
            // (`r19-unrolled-copies-hand-guards-the-post-call-chain-snapshot-20261007`):
            // a copy of a post-call-only snapshot stays post-call-only, so no
            // guard, fence or by-pc lookup reads it as a chain. Its
            // `post_call_of` is left `NO_NODE`: the copy names no call, so
            // the iteration's cloned call gets no chain post-call exit (as
            // before), and the ORIGINAL call -- copy 0, whose snapshot was
            // rewritten in place above -- keeps its one.
            if marks {
                copy.post_call_only = orig.post_call_only;
            }
            // JIT round 19 wave 14 (TAIL12-2, opt-in
            // `CRATONVM_JIT_IR_UNROLL_POST_CALL_PER_COPY`): the copy names
            // ITS iteration's clone of the call, so each clone gets its own
            // chain post-call exit instead of the fallback. Only when the call
            // was cloned (a distinct node): two snapshots naming one call get
            // none (`post_call_splice_states_by_call`).
            if per_copy && orig.post_call_of != NO_NODE {
                if let Some(&clone) = subst.get(&orig.post_call_of) {
                    if clone != orig.post_call_of {
                        copy.post_call_of = clone;
                    }
                }
            }
            graph.push_safepoint(copy);
            si
        };
        // The guard snapshot wins its pc: the builder pushes it BEFORE the
        // post-call-only one at a spliced call's successor, and the old
        // last-write-wins insert handed every node there (guards included)
        // the post-call snapshot. A post-call-only snapshot still answers a
        // pc nothing else describes, as before.
        if marks && orig.post_call_only {
            snap_for_bci.entry(orig.bci).or_insert(si);
        } else {
            snap_for_bci.insert(orig.bci, si);
        }
    }
    // A clone of a spliced body with no frame at its own pc resumes at its
    // outermost `invoke`, whose flat frame `plan_copy_frames` planned for
    // exactly that (round 13 wave 3, lane framestate).
    let splices = crate::ir::splice_resume_map_this_build();
    for &clone in iter_clones {
        let Some(pc) = graph.nodes[clone as usize].bytecode_pc else {
            continue;
        };
        let own = snap_for_bci.get(&pc).copied();
        let resumed = || {
            splices
                .as_ref()
                .and_then(|m| m.resume_bci(pc))
                .filter(|&r| r != pc)
                .and_then(|r| snap_for_bci.get(&r).copied())
        };
        let Some(si) = own.or_else(resumed) else {
            continue;
        };
        let bound = graph.set_node_frame_snapshot(clone, si);
        debug_assert!(
            bound,
            "per-copy frame {si} refused for clone {clone} at bci {pc} -- \
             `plan_copy_frames` and `set_node_frame_snapshot` disagree about \
             which snapshot describes this node",
        );
    }
}

/// `CRATONVM_JIT_IR_UNROLL_POST_CALL_PER_COPY` (JIT round 19 wave 14,
/// TAIL12-2; opt-in, needs the marks switch above): an unrolled copy's
/// post-call snapshot names its own cloned call.
pub(in crate::ir_optimize) fn unroll_post_call_per_copy_enabled() -> bool {
    false
}

/// The loop skeleton [`partial_unroll_loop`] rewrites, as `unroll` found it.
pub(in crate::ir_optimize) struct PartialUnroll {
    /// The loop header. Its inputs are `[entry_pred, back_ctrl]` — the shared
    /// checks above have already established the arity, and this transform
    /// re-checks the ORDER, which they do not.
    pub(in crate::ir_optimize) region: NodeId,
    /// The `If` projection that re-enters the loop.
    pub(in crate::ir_optimize) back_ctrl: NodeId,
    /// The single loop-exit test.
    pub(in crate::ir_optimize) if_node: NodeId,
    /// Copies of the body to end up with, counting the original as one.
    pub(in crate::ir_optimize) factor: usize,
}

/// Partially unroll a counted loop whose bound is a runtime value.
///
/// # The shape, and why this one
///
/// Read off the single-pass tier's own disassembly rather than assumed
/// (`c2-unrolling-is-a-deopt-metadata-problem-20260911.md` §3): **the loop test
/// is kept in every copy**, and only the back edge and its safepoint poll are
/// amortised.
///
/// ```text
/// header:
///   cmp i, n ; jge exit      <- every copy keeps this
///   BODY(i)  ; i += s
///   cmp i, n ; jge exit
///   BODY(i)  ; i += s        <- `factor` copies
///   ...
///   poll ; jmp header        <- paid once per `factor`
/// ```
///
/// That needs **no trip-count arithmetic**, which is where the soundness risk
/// in the textbook main-loop-plus-remainder form lives: the range-BCE closeout
/// records that `i + (U-1)*s` overflows at the top of the `int` range and that
/// the wrapped value passes a signed test. This form never computes that
/// expression, so it never has to answer for it.
///
/// It also introduces **no speculation**. Copy `k` runs only if copy `k`'s own
/// test passed, exactly as iteration `k` did before — which is what makes
/// cloning a body containing a trapping `Op::Load` no different here from
/// leaving it where it was.
///
/// # Where the early exits go, and why there is no exit merge
///
/// The textbook rendering of the diagram above gives each copy's failing test
/// its own edge OUT of the loop, which needs a `factor`-way exit merge and, at
/// it, an exit phi per carried value — and then every post-loop use of a
/// carried phi, in node inputs AND in safepoint slots, has to be rewritten to
/// the exit phi while every in-loop use is left alone. That rewrite is a
/// partition of a use list by "is this inside the loop", and getting it wrong
/// is not a crash: it is a frame slot naming the wrong iteration, which is the
/// defect class `internal/fixed-bugs/` records as an H2 `GROUP BY` returning 3
/// rows of 5.
///
/// So the early exits do not leave the loop. **Copy `k`'s failing test branches
/// back to the header**, carrying that copy's values on a new header
/// predecessor; the header's own test — copy 0's, which is the original `If`,
/// untouched — then fails on those same values and leaves through the original
/// exit.
///
/// The cost is one redundant header test, once, on the way out of the loop. The
/// benefit is that **nothing outside the loop changes at all**: `exit_ctrl`
/// keeps its users, the carried phis keep their identities and therefore their
/// post-loop uses, and no safepoint slot anywhere in the method needs a
/// rewrite. The transform is closed under the loop it is given.
///
/// # What it mutates
///
///  * appends `factor - 1` `If`/`Proj`/`Proj` triples, chained through the true
///    edges, plus `factor - 1` clones of the body;
///  * repoints the header's back edge at the LAST copy's true projection, and
///    each carried phi's back-edge value at that copy's output;
///  * appends the `factor - 1` early-exit projections as extra header
///    predecessors, each with its copy's values as the matching phi inputs.
///
/// # Errors
///
/// The bucket of [`unroll_bucket`] to charge the refusal to. Every one of them
/// is a shape this function declines to rewrite, never a partial rewrite
/// abandoned halfway: each check runs before the first node is added.
pub(in crate::ir_optimize) fn partial_unroll_loop(
    graph: &mut Graph,
    loop_: &PartialUnroll,
    carried: &[NodeId],
    order: &[NodeId],
    per_copy: Option<&[(u32, SafepointSnapshot)]>,
    dbg: bool,
) -> Result<(), usize> {
    let PartialUnroll {
        region,
        back_ctrl,
        if_node,
        factor,
    } = *loop_;

    // ── Everything that can refuse, before anything is added ─────────

    // The back edge must be the header's input 1. The shared analysis found
    // `back_ctrl` among the header's inputs without caring which slot it sat
    // in, and `analyze_counted_loop` then read each carried phi's entry value
    // from `inputs[1]` and its back-edge value from `inputs[2]` — which is only
    // those two things if the header's predecessors are in that order. A
    // reversed header is rare enough to decline and too quiet to assume.
    let rin = graph.nodes[region as usize].inputs.as_slice().to_vec();
    if rin.len() != 2 || rin[1] != back_ctrl {
        if dbg {
            eprintln!(
                "[DBG_UNROLL] region {region}: partial bail — back edge is not header input 1"
            );
        }
        return Err(unroll_bucket::CONTROL_SHAPE);
    }
    if rin[0] == NO_NODE || rin[0] == back_ctrl {
        return Err(unroll_bucket::CONTROL_SHAPE);
    }
    // Each carried phi must have exactly the two values those two predecessors
    // call for, so that appending one value per new predecessor keeps the
    // positional pairing `ir_verify::check_phis` enforces.
    for &p in carried {
        if graph.nodes[p as usize].inputs.len() != 3 {
            return Err(unroll_bucket::CONTROL_SHAPE);
        }
    }

    // The condition has to be one of the nodes being cloned: every copy needs
    // its OWN test, and a condition the clone set does not contain is one whose
    // value does not change per copy — so every copy's test would read copy 0's
    // induction variable and the loop would run forever.
    let cond = graph.nodes[if_node as usize].inputs[1];
    if !order.contains(&cond) {
        if dbg {
            eprintln!(
                "[DBG_UNROLL] region {region}: partial bail — loop condition {cond} is not in the clone set"
            );
        }
        return Err(unroll_bucket::BODY_UNCLONABLE);
    }

    // Which projection is which, read from the node rather than assumed.
    let back_which = match graph.nodes[back_ctrl as usize].op {
        Op::Proj(w) if w <= 1 => w,
        _ => return Err(unroll_bucket::CONTROL_SHAPE),
    };
    let if_pc = graph.nodes[if_node as usize].bytecode_pc;

    // ── From here the graph is edited; nothing below may refuse ──────

    // Copy 0 is the ORIGINAL body and needs no clone — but it does need its
    // nodes STAMPED with the original snapshot, because after this there are
    // `factor` snapshots at each body bci and `ir_verify::check_frame_states`
    // permits a duplicated bci only when every snapshot at it is CLAIMED. The
    // substitution is empty, so `install_copy_frames`' in-place rewrite writes
    // each slot its own current value and the frame is unchanged.
    if let Some(plan) = per_copy {
        let identity: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        install_copy_frames(graph, plan, &identity, order, true);
    }

    // The back-edge value of each carried phi, read before any of them is
    // repointed: this is the ORIGINAL body's output, i.e. the values copy 1 is
    // entered with.
    let back_val: Vec<NodeId> = carried
        .iter()
        .map(|&p| graph.nodes[p as usize].inputs[2])
        .collect();

    let mut cur: FxHashMap<NodeId, NodeId> = carried
        .iter()
        .copied()
        .zip(back_val.iter().copied())
        .collect();

    // The header predecessors this transform appends, in the order their phi
    // inputs must be appended too: `(early_exit_proj, values entering that
    // copy)`.
    let mut extra_preds: Vec<(NodeId, Vec<NodeId>)> = Vec::with_capacity(factor - 1);
    // The true edge the next copy hangs off. Copy 1 hangs off copy 0's.
    let mut prev_true = back_ctrl;

    for _k in 1..factor {
        // The test first, so the copy's body has a control node to be anchored
        // at. Its condition input is seeded with the ORIGINAL condition rather
        // than a placeholder — a real edge keeps the use lists valid across the
        // clone loop — and is repointed at this copy's own clone below.
        let if_k = graph.add(Op::If, IrType::Control, vec![prev_true, cond], if_pc);
        // `Proj(0)` before `Proj(1)`, which is the order `IrBuilder` adds them
        // in and therefore the order every graph in this tree had until a
        // transform cloned an `If`. `ir_schedule` no longer depends on it — it
        // sorts an `If`'s successors by projection index precisely because this
        // code broke that assumption once — but emitting the conventional order
        // costs nothing and keeps a cloned loop reading like a built one.
        let proj_lo = graph.add(Op::Proj(0), IrType::Control, vec![if_k], if_pc);
        let proj_hi = graph.add(Op::Proj(1), IrType::Control, vec![if_k], if_pc);
        let (t_k, x_k) = if back_which == 0 {
            (proj_lo, proj_hi)
        } else {
            (proj_hi, proj_lo)
        };

        // The rename this copy is built under.
        //
        // The two control anchors are the two the shared side-effect scan
        // spells (`ctrl == region || ctrl == back_ctrl`), and they map to
        // DIFFERENT blocks, which is the whole of why a copy lands where it
        // should:
        //
        //   * `region` — the header, i.e. before this copy's test — maps to
        //     `prev_true`, the block that runs just before `if_k`;
        //   * `back_ctrl` — after this copy's test — maps to `t_k`.
        let mut subst: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        subst.insert(region, prev_true);
        subst.insert(back_ctrl, t_k);
        for (&p, &v) in &cur {
            subst.insert(p, v);
        }

        let mut iter_clones: Vec<NodeId> = Vec::with_capacity(order.len());
        for &d in order {
            let (op, ty, pc, volatile) = {
                let nd = &graph.nodes[d as usize];
                (nd.op.clone(), nd.ty, nd.bytecode_pc, nd.volatile_access)
            };
            let new_inputs: Vec<NodeId> = graph.nodes[d as usize]
                .inputs
                .iter()
                .map(|&inp| subst.get(&inp).copied().unwrap_or(inp))
                .collect();
            let clone = graph.add(op, ty, new_inputs, pc);
            // A copy of a volatile access is a volatile access (round 9 wave
            // 6): `Graph::add` makes every node plain.
            if volatile {
                graph.set_volatile_access(clone);
            }
            subst.insert(d, clone);
            iter_clones.push(clone);
        }

        // This copy's own test, now that its clone exists. `cond` is in `order`
        // (checked above), so the lookup cannot miss.
        let cond_k = subst[&cond];
        graph.set_input(if_k, 1, cond_k);

        // Per-copy deopt metadata, while `subst` still describes exactly this
        // copy — the same ordering constraint `install_copy_frames` documents
        // for the full unroller, and for the same reason.
        if let Some(plan) = per_copy {
            install_copy_frames(graph, plan, &subst, &iter_clones, false);
        }

        // This copy's test fails on the values the copy was ENTERED with, so
        // those are what its early exit carries back to the header.
        extra_preds.push((x_k, carried.iter().map(|p| cur[p]).collect::<Vec<NodeId>>()));

        // Advance to the values the next copy is entered with.
        cur = carried
            .iter()
            .copied()
            .zip(
                back_val
                    .iter()
                    .map(|bv| subst.get(bv).copied().unwrap_or(*bv)),
            )
            .collect();
        prev_true = t_k;
    }

    // The real back edge is now the LAST copy's true projection, and each
    // carried phi's back-edge value is that copy's output.
    graph.set_input(region, 1, prev_true);
    for (i, &p) in carried.iter().enumerate() {
        let fin = cur[&p];
        if fin != back_val[i] {
            graph.set_input(p, 2, fin);
        }
    }

    // The early exits, appended as extra header predecessors. Predecessor `j`
    // of the header pairs with phi input `j + 1`, so the two pushes have to
    // stay in lockstep — which is why each copy's values were captured
    // alongside its projection rather than recomputed here.
    for (x_k, vals) in extra_preds {
        graph.push_input(region, x_k);
        for (&p, &v) in carried.iter().zip(vals.iter()) {
            graph.push_input(p, v);
        }
    }

    if dbg {
        eprintln!(
            "[DBG_UNROLL] partially unrolled counted loop (region {region}, factor {factor}, \
             {} cloned nodes/copy, per-copy frames: {})",
            order.len(),
            per_copy.is_some(),
        );
    }
    Ok(())
}

/// `CRATONVM_JIT_IR_UNROLL_MAIN_POST` (round 17;
/// `r16-codegen-loop-and-straight-line-anatomy-FIXED-20261005.md`): partially unroll
/// a runtime-bound counted loop as a MAIN loop that tests once per `factor`
/// copies plus the original loop as the POST loop
/// ([`main_post_unroll_loop`]), instead of the per-copy-test form. Independent
/// of `CRATONVM_JIT_IR_PARTIAL_UNROLL`, which only arms the per-copy-test
/// form as the fallback for a loop this one declines. **Default ON** since
/// round 17 (`0` turns it off): `R16CodegenLoops` intLoop 0.73 -> 0.60 ns.
/// Read per loop.
pub(in crate::ir_optimize) fn main_post_unroll_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT` (JIT r23 w2c close, the page's
/// item (B); perf, **default ON**, `=0` kill): the main/post form
/// ([`main_post_unroll_loop`]) also takes a counted loop whose body reads
/// fields and array elements -- nodes that can deopt (a null base, an index
/// out of range, the field row's helper) -- by giving every copy its own
/// frames ([`main_post_copy_frames`]). Before, the unroller's side-effect
/// screen refused every in-loop `ArrayLoad` and the main/post form every body
/// node that can deopt, so `for (i ..) s += a[i].v` kept one exit test, one
/// poll and two phi publishes per element. Read per loop.
pub(in crate::ir_optimize) fn unroll_main_post_deopt_enabled() -> bool {
    true
}

/// The per-copy frame plan for a main/post unroll of a body that can deopt,
/// or `None` when the copies cannot be given exact frames:
///
/// * [`plan_copy_frames`] refuses (a snapshot names the body at a bci no clone
///   carries), or a planned snapshot names a loop-variant value no copy
///   renames (a dead local the builder still records);
/// * the body's deopt points are not totally ordered ACROSS copies. Copy
///   `k + 1` must not fault before copy `k` has run every one of its own: a
///   fault resumes the interpreter at that copy's iteration, so an earlier
///   iteration's work (and its own fault) would be skipped. Within one copy
///   the original body's order holds; across copies the order is carried by
///   the loop's memory chain -- the builder threads every read through memory
///   (`self.mem = load`), so copy `k + 1` reads the memory copy `k` ended on.
///   That holds only if every node that can deopt is ON the chain from the
///   carried memory phi to its back-edge value, which is what is checked: a
///   deopting node off the chain (a division, a read the builder did not
///   thread) could be scheduled ahead of the previous copy's faults.
pub(in crate::ir_optimize) fn main_post_copy_frames(
    graph: &Graph,
    to_clone: &FxHashSet<NodeId>,
    carried: &[NodeId],
    variant: &FxHashSet<NodeId>,
    region: NodeId,
    dbg: bool,
) -> Option<Vec<(u32, SafepointSnapshot)>> {
    let bail = |why: &str| {
        if dbg {
            eprintln!("[DBG_UNROLL] region {region}: main/post frames refused -- {why}");
        }
        None
    };
    let Ok(plan) = plan_copy_frames_in(graph, to_clone, Some(variant)) else {
        return bail("a snapshot names the body at a bci no clone carries");
    };
    let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();
    let stale = plan.iter().any(|(_, sp)| {
        sp.locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .any(|&v| {
                v != NO_NODE
                    && variant.contains(&v)
                    && !to_clone.contains(&v)
                    && !carried_set.contains(&v)
            })
    });
    if stale {
        if dbg {
            for (_, sp) in plan.iter() {
                for &v in sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                {
                    if v != NO_NODE
                        && variant.contains(&v)
                        && !to_clone.contains(&v)
                        && !carried_set.contains(&v)
                    {
                        eprintln!(
                            "[DBG_UNROLL] region {region}: stale slot n{v} {:?} (pc {:?}) in the frame at bci {}",
                            graph.nodes[v as usize].op, graph.nodes[v as usize].bytecode_pc, sp.bci
                        );
                    }
                }
            }
        }
        return bail("a snapshot names a loop-variant node no copy renames");
    }
    let deopting: Vec<NodeId> = to_clone
        .iter()
        .copied()
        .filter(|&c| {
            graph
                .node_opt(c)
                .is_some_and(|n| !crate::ir_lower::node_cannot_deopt(graph, n))
        })
        .collect();
    let mem_phis: Vec<NodeId> = carried
        .iter()
        .copied()
        .filter(|&p| graph.node_opt(p).is_some_and(|n| n.ty == IrType::Memory))
        .collect();
    let &[mem_phi] = mem_phis.as_slice() else {
        return bail("not exactly one carried memory phi");
    };
    let Some(&back) = graph.node_opt(mem_phi).and_then(|n| n.inputs.get(2)) else {
        return bail("the memory phi has no back-edge value");
    };
    let mut chain: FxHashSet<NodeId> = FxHashSet::default();
    let mut cur = back;
    while cur != mem_phi {
        if !to_clone.contains(&cur) || !chain.insert(cur) {
            return bail("the memory chain leaves the body");
        }
        let Some(n) = graph.node_opt(cur) else {
            return bail("a dangling memory token");
        };
        let Some(slot) = crate::ir::memory_token_slot(n) else {
            return bail("a chain node has no memory token");
        };
        cur = n.inputs.get(slot).copied().unwrap_or(NO_NODE);
    }
    if deopting.iter().any(|d| !chain.contains(d)) {
        return bail("a node that can deopt is off the memory chain");
    }
    Some(plan)
}

/// The main-loop + post-loop partial unroll, HotSpot's shape for a
/// runtime-bound counted loop: `for (i = init; i < n; i += s) BODY` becomes
///
/// ```text
/// if (n - d < n) {                       // d = (factor-1)*s does not wrap n
///   for (i' = init; i' < n - d; ) {       // MAIN: one test per factor copies
///     BODY(i'); BODY(i'+s); ... ; i' += factor*s
///   }
/// }
/// for (i = <where main stopped>; i < n; i += s) BODY   // POST: the original
/// ```
///
/// The per-copy-test form ([`partial_unroll_loop`]) keeps every copy's exit
/// test and sends each early exit back through the header, so its header
/// phis take `factor` inputs, the copies' compares cannot fuse with their
/// branches, and the residency file refuses every phi (`R16CodegenLoops`
/// intLoop ran SLOWER unrolled, 1.00 against 0.69 ns). Here the copies are
/// one straight-line block with no test between them, the main header keeps
/// two predecessors, and the original loop is not touched beyond its entry.
///
/// # Why it is exact
///
/// * The main test `i' < n - d` holds only when `i' + d < n`, i.e. when the
///   original loop would run all `factor` iterations `i', i'+s, ..., i'+d`
///   (each `< n`, as `s > 0`). `n - d` is the true difference: the pre-header
///   test `n - d < n` is false exactly when it wraps (`s`, `d` > 0), and then
///   the main loop is skipped.
/// * A wrap of the induction variable inside a trip (`n` near `MAX`) happens
///   in the original loop too, at the same iteration: both compute the same
///   sequence of `i` values and run the body on each, and the original keeps
///   looping on a wrapped (negative, so `< n`) value exactly as the main loop
///   does on `< n - d`.
/// * When the main loop stops, the post loop resumes with the carried values
///   it ended on and runs the remaining iterations under the original test.
///   The post loop's phis keep their identities, so every use after the loop
///   (and every snapshot slot naming one) still reads the final values.
/// * Only a PURE body reaches this (the shared checks refuse a side effect, a
///   trap, a load pinned to the header and a value escaping the loop), so no
///   cloned node is a deopt point and the copies need no frames of their own.
///   The main header carries no bci: it gets no OSR entry, and its back-edge
///   poll no mode exit; both stay with the original header.
///
/// Applies to an `int` induction `i = phi(init, i + s)`, `0 < s <= 64`, under
/// a stay condition that reads `i < n` with `n` loop-invariant; anything else
/// returns `false` and falls back to the per-copy-test form. Since JIT round
/// 20 wave 12 (W5L3-3, [`main_post_long_iv_enabled`]) a `long` induction too,
/// its test read through `lcmp`; every argument above holds at 64 bits.
pub(in crate::ir_optimize) fn main_post_unroll_loop(
    graph: &mut Graph,
    loop_: &PartialUnroll,
    carried: &[NodeId],
    order: &[NodeId],
    variant: &FxHashSet<NodeId>,
    // JIT r23 w2c close (`CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT`): the
    // per-copy frame plan ([`plan_copy_frames`]) for a body whose reads can
    // deopt. `None`: every copy must be unable to deopt, as before.
    frames: Option<&[(u32, SafepointSnapshot)]>,
    dbg: bool,
) -> bool {
    let PartialUnroll {
        region,
        back_ctrl,
        if_node,
        factor,
    } = *loop_;
    let bail = |why: &str| {
        if dbg {
            eprintln!("[DBG_UNROLL] region {region}: main/post bail -- {why}");
        }
        false
    };
    let rin = graph.nodes[region as usize].inputs.as_slice().to_vec();
    if rin.len() != 2 || rin[1] != back_ctrl || rin[0] == NO_NODE || rin[0] == back_ctrl {
        return bail("header inputs are not [entry, back]");
    }
    let entry_pred = rin[0];
    if carried
        .iter()
        .any(|&p| graph.nodes[p as usize].inputs.len() != 3)
    {
        return bail("a carried phi is not [region, entry, back]");
    }
    // The stay condition as `iv < n`.
    let back_which = match graph.nodes[back_ctrl as usize].op {
        Op::Proj(w) if w <= 1 => w,
        _ => return bail("the back edge is not a projection of the test"),
    };
    let cond = graph.nodes[if_node as usize].inputs[1];
    let Some(cn) = graph.node_opt(cond) else {
        return bail("no condition");
    };
    let Op::Cmp(op) = cn.op else {
        return bail("the condition is not an int compare");
    };
    let &[l, r] = cn.inputs.as_slice() else {
        return bail("compare arity");
    };
    // JIT round 20 wave 12 (lane qLoop, W5L3-3): a `long` loop's test is
    // `Cmp(cc, LCmp(i, n), 0)` (`lload; lload; lcmp; if<cc>`), and `lcmp(a, b)
    // cc 0` is exactly `a cc b` as signed longs (`lcmp_operands_of_zero_test`,
    // the reading `analyze_counted_loop` already makes). Off: the compare's own
    // operands, as before.
    let long_iv = main_post_long_iv_enabled();
    let through_lcmp = if long_iv {
        lcmp_operands_of_zero_test(graph, l, r)
    } else {
        None
    };
    let (l, r) = match through_lcmp {
        Some([a, b]) => (a, b),
        None => (l, r),
    };
    // Proj(0) is the edge taken when the condition holds.
    let stay_holds = back_which == 0;
    let (iv, n) = match (op, stay_holds) {
        (CmpOp::Lt, true) | (CmpOp::Ge, false) => (l, r),
        (CmpOp::Gt, true) | (CmpOp::Le, false) => (r, l),
        _ => return bail("the stay condition is not `iv < n`"),
    };
    // The induction variable's width: `int`, or (W5L3-3) `long`. Every node
    // the transform adds below is built at this width.
    let iv_ty = graph.node_opt(iv).map(|x| x.ty);
    let wide = long_iv && iv_ty == Some(IrType::Long);
    if !carried.contains(&iv) || (iv_ty != Some(IrType::Int) && !wide) {
        return bail("the compared value is not an int phi of this header");
    }
    let iv_ty = if wide { IrType::Long } else { IrType::Int };
    if variant.contains(&n) || graph.node_opt(n).map(|x| x.ty) != Some(iv_ty) {
        return bail("the bound varies in the loop");
    }
    let next = graph.nodes[iv as usize].inputs[2];
    let stride = match graph.node_opt(next) {
        Some(nn) if nn.op == Op::Add && nn.inputs.len() == 2 => {
            if nn.inputs[0] == iv {
                const_value(&graph.nodes, nn.inputs[1])
            } else if nn.inputs[1] == iv {
                const_value(&graph.nodes, nn.inputs[0])
            } else {
                None
            }
        }
        _ => None,
    };
    // Cast: an `int` constant is its low 32 bits; a `long` one is itself.
    let Some(s) = stride.map(|v| if wide { v } else { i64::from(v as i32) }) else {
        return bail("no constant stride");
    };
    if !(1..=64).contains(&s) || factor < 2 {
        return bail("stride or factor out of range");
    }
    // Round nio20 wave 5 (lane L3, review): the doc above says "only a PURE
    // body reaches this", and the shared checks did not make it so -- the clone
    // predicate admits `Op::Load` (`unroll_body_node_clonable`), so a field
    // read of a loop-carried object (`s += p.v; p = p.next` under an `i < n`
    // counter) reached here. Its copies carry no pc, and a pc-less node that
    // deopts transfers at the lowerer's CARRIED bci -- whichever node was
    // lowered before it, possibly a call or store ahead of the loop, which the
    // resume would then run again. Every copy must be a node that cannot deopt.
    // JIT r23 w2c close: unless every copy gets a frame of its own -- then
    // each copy keeps its pc and names its iteration's snapshot, as the
    // per-copy-test form's copies do (`install_copy_frames`). The original
    // body stays the post loop with its frames unchanged, so every copy's
    // snapshot is APPENDED (`first_iteration == false`).
    if frames.is_none()
        && order.iter().any(|&c| {
            graph
                .nodes
                .get(c as usize)
                .is_none_or(|n| !crate::ir_lower::node_cannot_deopt(graph, n))
        })
    {
        return bail("a body node can deopt; its pc-less copies would have no frame");
    }
    // factor <= 16 and s <= 64, so the displacement fits an int.
    let d = (factor as i64 - 1) * s;

    // ── From here the graph is edited; nothing below may refuse ──────

    // With per-copy frames, the ORIGINAL body (the post loop) claims its own
    // snapshots first: after the copies append theirs, each body bci has
    // several, and `ir_verify::check_frame_states` admits a duplicated bci
    // only when every snapshot at it is claimed (the per-copy-test form's
    // copy 0 does the same).
    if let Some(plan) = frames {
        let identity: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        install_copy_frames(graph, plan, &identity, order, true);
    }

    // Pre-header: `lim = n - d; if (lim < n)` -- the main loop's licence.
    // At the induction variable's width (W5L3-3): a `long` `Sub` wraps at
    // 2^64 exactly as `lsub` does, and a `Cmp` of two `long` operands is the
    // signed 64-bit compare (`ir_lower` keys the width on the operand types),
    // so the licence is false exactly when `n - d` wraps, as for `int`.
    let dc = graph.add(Op::Const(d), iv_ty, vec![], None);
    let lim = graph.add(Op::Sub, iv_ty, vec![n, dc], None);
    let ok = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![lim, n], None);
    let if_pre = graph.add(Op::If, IrType::Control, vec![entry_pred, ok], None);
    let into_main = graph.add(Op::Proj(0), IrType::Control, vec![if_pre], None);
    let skip_main = graph.add(Op::Proj(1), IrType::Control, vec![if_pre], None);

    // The main header and its phis; back-edge inputs appended below.
    let main = graph.add(Op::Merge, IrType::Control, vec![into_main], None);
    let main_phis: Vec<NodeId> = carried
        .iter()
        .map(|&p| {
            let (ty, init) = (
                graph.nodes[p as usize].ty,
                graph.nodes[p as usize].inputs[1],
            );
            graph.add(Op::Phi, ty, vec![main, init], None)
        })
        .collect();
    let iv_main = main_phis[carried.iter().position(|&p| p == iv).unwrap_or(0)];
    let main_cond = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv_main, lim], None);
    let main_if = graph.add(Op::If, IrType::Control, vec![main, main_cond], None);
    let main_stay = graph.add(Op::Proj(0), IrType::Control, vec![main_if], None);
    let main_exit = graph.add(Op::Proj(1), IrType::Control, vec![main_if], None);

    // `factor` copies of the body, chained through the carried values.
    let back_val: Vec<NodeId> = carried
        .iter()
        .map(|&p| graph.nodes[p as usize].inputs[2])
        .collect();
    let mut cur: Vec<NodeId> = main_phis.clone();
    // JIT r23 w2c close (`CRATONVM_JIT_IR_UNROLL_INDUCTION_OFFSETS`): copy `k`
    // enters with `i' + k*s`, not with copy `k - 1`'s `+ s`. The same value
    // (both wrap at the induction's width exactly as the original loop's
    // `iinc`s do), without a `k`-long chain of adds, and in the `i' + j` shape
    // `ir_check_elim`'s offset rule reads against the main test `i' < n - d`.
    let iv_pos = carried.iter().position(|&p| p == iv).unwrap_or(0);
    let offsets = true;
    for k in 0..factor {
        let mut subst: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        subst.insert(region, main);
        subst.insert(back_ctrl, main_stay);
        for (&p, &v) in carried.iter().zip(cur.iter()) {
            subst.insert(p, v);
        }
        let mut iter_clones: Vec<NodeId> = Vec::with_capacity(order.len());
        for &node in order {
            let (op, ty, pc, volatile) = {
                let nd = &graph.nodes[node as usize];
                (nd.op.clone(), nd.ty, nd.bytecode_pc, nd.volatile_access)
            };
            let inputs: Vec<NodeId> = graph.nodes[node as usize]
                .inputs
                .iter()
                .map(|&i| subst.get(&i).copied().unwrap_or(i))
                .collect();
            // No bci: no snapshot describes a copy (see the doc comment) --
            // unless each copy gets its own frames, which hang on the copy's
            // pcs (and a field read's row is keyed by its pc).
            let clone = graph.add(op, ty, inputs, if frames.is_some() { pc } else { None });
            if volatile {
                graph.set_volatile_access(clone);
            }
            if frames.is_some() {
                if let Some(&decl) = graph.field_decl_class.get(&node) {
                    graph.field_decl_class.insert(clone, decl);
                }
                if let Some(&row) = graph.field_rows.get(&node) {
                    graph.field_rows.insert(clone, row);
                }
                if let Some(&row_pc) = graph.field_row_pcs.get(&node) {
                    graph.field_row_pcs.insert(clone, row_pc);
                }
            }
            subst.insert(node, clone);
            iter_clones.push(clone);
        }
        // This copy's frames, while `subst` describes exactly this copy (the
        // carried phis at the values entering it, the body at its clones).
        if let Some(plan) = frames {
            install_copy_frames(graph, plan, &subst, &iter_clones, false);
        }
        cur = back_val
            .iter()
            .map(|bv| subst.get(bv).copied().unwrap_or(*bv))
            .collect();
        if offsets {
            // factor <= 16 and s <= 64, so the displacement fits an int.
            let step = graph.add(Op::Const((k as i64 + 1) * s), iv_ty, vec![], None);
            cur[iv_pos] = graph.add(Op::Add, iv_ty, vec![iv_main, step], None);
        }
    }
    graph.push_input(main, main_stay);
    for (&mp, &v) in main_phis.iter().zip(cur.iter()) {
        graph.push_input(mp, v);
    }

    // The join in front of the post loop: from the main loop's exit (its
    // phis) or from the skipped licence (the entry values).
    let join = graph.add(Op::Merge, IrType::Control, vec![main_exit, skip_main], None);
    let joined: Vec<NodeId> = carried
        .iter()
        .zip(main_phis.iter())
        .map(|(&p, &mp)| {
            let (ty, init) = (
                graph.nodes[p as usize].ty,
                graph.nodes[p as usize].inputs[1],
            );
            graph.add(Op::Phi, ty, vec![join, mp, init], None)
        })
        .collect();
    // The original loop is the post loop: entered from the join.
    graph.set_input(region, 0, join);
    for (&p, &j) in carried.iter().zip(joined.iter()) {
        graph.set_input(p, 1, j);
    }
    graph.partially_unrolled_headers.extend([region, main]);
    if dbg {
        eprintln!(
            "[DBG_UNROLL] main/post unrolled counted loop (region {region}, factor {factor}, \
             stride {s}, {} cloned nodes/copy)",
            order.len()
        );
        // W5L3-3 positive control: the `long` induction route engaged.
        if wide {
            eprintln!("[DBG_UNROLL] main/post long induction (region {region}, stride {s})");
        }
    }
    true
}

/// `CRATONVM_JIT_IR_UNROLL_MAIN_POST_LONG_IV` (JIT round 20 wave 12, lane
/// qLoop, W5L3-3 of `jit-r20-nio-proposals-w5-L3.md`): let
/// [`main_post_unroll_loop`] take a `long` induction variable (the
/// `MemorySegment` / `Buffer` loops whose offsets are `long`), reading the
/// `lcmp` test through and building the licence and the main test at `long`
/// width. Default ON; `0` keeps the `int`-only form. Read per loop.
pub(in crate::ir_optimize) fn main_post_long_iv_enabled() -> bool {
    true
}

/// May `c` be cloned into an unrolled body (full or partial)?
///
/// Loads and pure arithmetic, as always -- and, since round 9 wave 11 (irl11,
/// `perf-long-counted-loops-lose-the-optimizing-osr-body`), an integer
/// division or remainder by a non-zero constant. That one cannot trap: it is
/// exactly `ir_lower::int_division_cannot_trap`, the prediction
/// `graph_cannot_deopt` already makes for it and the lowering's guard-free
/// constant-divisor path already honours (no zero test, and `MIN / -1`
/// materialised inline). So it is as clonable as the multiply it lowers to.
/// Before this, `s += i * 3 - i / 2` bailed with "unclonable body node Div",
/// the one thing between the page's `div`/`rem`/`full` loops and the unroller
/// `mul` already reaches. Two edges exactly: a division still carrying a guard
/// token is left alone.
pub(in crate::ir_optimize) fn unroll_body_node_clonable(graph: &Graph, c: NodeId) -> bool {
    let Some(n) = graph.nodes.get(c as usize) else {
        return false;
    };
    matches!(n.op, Op::Load(_))
        || n.op.is_pure()
        || (n.inputs.len() == 2 && crate::ir_lower::int_division_cannot_trap(graph, n))
}

// ── Fenced loads a full unroll may drop (nio21 wave 6, lane L5, L5W5-2) ──
//
// `Nio20FenceRow own`: `Object o = PLAIN[(i + k) & 7]; h += k ^ i;
// Reference.reachabilityFence(o);` in a 16-trip counted loop. With the early
// mask predicate (`CRATONVM_JIT_IR_MASK_PREDICATE_EARLY`) and the
// `node_cannot_deopt` clause (`CRATONVM_JIT_IR_MASK_GUARD_ELIDE`) the element
// load cannot deopt, but the unroller's side-effect screen still lists every
// `ArrayLoad`. The load is not in the clone set either: it feeds only the
// fence's `Op::KeepAlive`, not a carried value.
//
// Under `CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD` (default ON since nio21 w7) a FULL unroll
// whose copies take the pure-body escape (pc-less, and every copy unable to
// deopt: `pure_body_frames_unconsultable`) retires such a load with its
// `KeepAlive`s and its private index computation instead of cloning them.
//
// # Why dropping is sound
//
// * The load: it cannot trap (`masked_array_load_cannot_deopt`, the predicate
//   the lowering's elision reads), reads memory no one writes in a loop the
//   screen admitted (no store, call or allocation in it), and its value has no
//   user but the fences. Not performing it is unobservable.
// * The fence: `Reference.reachabilityFence(o)` obliges the collector to treat
//   `o` as reachable until the fence's program point. A collector acts only at
//   a GC point (a safepoint poll, a call, an allocation slow path). In a full
//   pure-body unroll there is none between where iteration `t`'s load would
//   have run and where its fence would have run: the copies are pure
//   arithmetic and field loads that cannot deopt, with no poll (the back edge
//   is gone) and no pc. So no collection can fall inside any fence's
//   obligation window, and no observer can tell the fence was dropped. (The
//   JMM gives the fence no ordering effect of its own.) This is the reviewer
//   sign-off item (d) of L5W1-4 asked for; the argument is recorded here and
//   on `r20-masked-index-loop-keeps-its-load-and-checks-CLOSED-20261012.md`.
// * Frames: every snapshot naming a dropped node must be unreachable after
//   the transform, which `pure_body_frames_unconsultable` checks with the
//   dropped nodes counted as clones.

/// `CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD` (nio21 wave 6, lane L5) --
/// **default ON since nio21 wave 7** (`=0` is the kill switch): see the
/// section notes. Read per candidate load.
///
/// Why ON (nio21 wave 7): the IR's `Op::KeepAlive` emits no code and is not a
/// safepoint; its only effect on the emitted body is to keep its referent in
/// the oop maps and frames of the GC points between the load and the fence.
/// The drop is taken only when no such point exists (every copy pc-less, no
/// poll, no call, no allocation), so the body with the fence and the body
/// without it are the same machine code minus one load that cannot trap and
/// whose value nothing else reads. HotSpot 25 drops the same load (its C2 has
/// no reachability-fence node).
pub(crate) fn unroll_drop_fenced_load_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD` (JIT round 20 wave 19, lane
/// yLoopPred, LOOPOPT20-2 / L5W6-2 of `docs/feature-designs/jit-ir-optimizations.md`)
/// -- **default ON**, `=0` restores the old refusal. A FULL pure-body unroll
/// CLONES an in-loop masked `ArrayLoad` whose value feeds the loop (`h = h *
/// 31 + t[(i + k) & 15]`, `crc = T[(crc ^ b) & 0xff] ^ ..`) when the loop's
/// own entry carries the mask guard that proves it
/// (`mask_predicate::entry_mask_guard_for`, planted early: the early plant
/// takes such a loop under this switch). Each copy is pc-less, anchored on the
/// loop's entry control (the guard's own control), and names that guard as
/// its guard TOKEN (`ir::guard_token_slot`): the token is an input, so the
/// scheduler emits the guard first in that block, and the masked-access proof
/// for a copy outside the loop is restated on it
/// (`mask_predicate::MaskGuardProofs::proves`'s token arm): the copy runs
/// after the guard passed, so its base is non-null and `p & m <= m < len`.
/// The lowering then emits no check for it (`node_cannot_deopt` and the
/// elision read the same proof), and a pc-less copy owes no resume pc. If a
/// later pass strips the token (`eliminate_trivial_guards`), the lowering
/// emits the checks with no pc and refuses the compile (`deopt_pc_of`), the
/// safe direction. Read per candidate load (no `static`).
pub(crate) fn unroll_clone_masked_load_enabled() -> bool {
    true
}

/// Is `id` an `ArrayLoad` the lowering emits with no check
/// (`ir_lower::masked_array_load_cannot_deopt`), with no guard token, whose
/// every user is an `Op::KeepAlive` of it pinned to one of `loop_ctrls`?
pub(in crate::ir_optimize) fn fenced_load_droppable(
    graph: &Graph,
    users: &[Vec<NodeId>],
    id: NodeId,
    loop_ctrls: &[NodeId],
) -> bool {
    let Some(n) = graph.nodes.get(id as usize) else {
        return false;
    };
    if !matches!(n.op, Op::ArrayLoad(_))
        || n.inputs.len() != 4
        || !crate::ir_lower::masked_array_load_cannot_deopt(graph, n)
    {
        return false;
    }
    let us = users.get(id as usize).map(Vec::as_slice).unwrap_or(&[]);
    // JIT round 20 wave 4 (lane loopopt): no user at all is a dead load, kept
    // only as a trap point the guard proves cannot fire: droppable too.
    us.iter().all(|&u| {
        graph.nodes.get(u as usize).is_some_and(|un| {
            un.op == Op::KeepAlive
                && un.inputs.len() == 2
                && un.inputs[1] == id
                && loop_ctrls.contains(&un.inputs[0])
        })
    })
}

/// The nodes a full unroll retires with the fenced `loads`: the loads, their
/// `KeepAlive`s, and the pure loop-variant index nodes only they use
/// (transitively). `None` when an index node of the loop is shared with
/// anything else or is not pure (it would survive naming a killed φ).
pub(in crate::ir_optimize) fn fenced_load_drop_set(
    graph: &Graph,
    users: &[Vec<NodeId>],
    loads: &[NodeId],
    to_clone: &FxHashSet<NodeId>,
    carried: &FxHashSet<NodeId>,
    variant: &FxHashSet<NodeId>,
) -> Option<FxHashSet<NodeId>> {
    let mut set: FxHashSet<NodeId> = FxHashSet::default();
    let mut work: Vec<NodeId> = Vec::new();
    for &l in loads {
        set.insert(l);
        set.extend(users.get(l as usize).into_iter().flatten().copied());
        work.push(*graph.nodes.get(l as usize)?.inputs.get(3)?);
    }
    let mut guard = 0usize;
    while let Some(x) = work.pop() {
        guard += 1;
        if guard > 256 {
            return None;
        }
        if x == NO_NODE
            || set.contains(&x)
            || carried.contains(&x)
            || to_clone.contains(&x)
            || !variant.contains(&x)
        {
            continue;
        }
        let n = graph.nodes.get(x as usize)?;
        if !n.op.is_pure() || n.op.is_control() {
            return None;
        }
        let shared = users
            .get(x as usize)
            .into_iter()
            .flatten()
            .any(|u| !set.contains(u));
        if shared {
            return None;
        }
        set.insert(x);
        work.extend(n.inputs.iter().copied());
    }
    Some(set)
}

pub(in crate::ir_optimize) fn unroll(graph: &mut Graph) -> bool {
    unroll_in(graph, None, false)
}

/// Is the control-flow full unroll ([`cfg_unroll`]) to run? Under the
/// unroller's own switch as well as its own.
pub(in crate::ir_optimize) fn may_unroll_cfg_body() -> bool {
    unroll_enabled() && cfg_unroll::cfg_unroll_enabled()
}

/// `CRATONVM_JIT_IR_UNROLL_AFTER_PREDICATES` (JIT r23 w2c close; perf): a
/// second [`unroll_in`] round, after the loop-entry checkcast and null-base
/// predicates, over the loops they moved something out of. The loop rounds
/// (and so the unroller) run BEFORE those predicates, so a counted loop whose
/// body was `s += o.v + h.k + i` -- impure only through the two field reads the
/// predicates then take to the pre-header -- was never offered to the
/// main/post form once it had become pure (`W2cTypecheckLoops` plainfixed /
/// castfixed, whose timed loop is spliced into `time()`'s OSR body). Loops a
/// partial unroll already took this compile are skipped
/// (`Graph::partially_unrolled_headers`). Read per compile.
pub(crate) fn unroll_after_predicates_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_UNROLL_YIELDS_TO_VECTOR` (the r23 L8 close, L8-1's
/// residual; perf, **default ON**, `=0` kill): [`unroll_in`] leaves a counted
/// loop the vector pre-loop admits
/// (`ir_vector_loop::vector_loop_headers`) to the pre-loop. Since
/// `CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT` (r23 w2c) the main/post form also
/// takes bodies that read array elements, so a reduction -- `m =
/// Math.max(m, a[j])`, `s += a[j]` -- was unrolled four times first and the
/// planner then found only the post loop (the main loop steps by four): the
/// VEC21-5 rows ran at scalar speed with the switch on or off. Read per
/// unroll pass.
pub(in crate::ir_optimize) fn unroll_yields_to_vector_enabled() -> bool {
    true
}

/// The loop headers whose pre-header holds a `Guard` created at or after node
/// `from` -- the loops a predicate planter used since then.
pub(in crate::ir_optimize) fn loops_predicated_since(
    graph: &Graph,
    from: usize,
) -> FxHashSet<NodeId> {
    let guarded: FxHashSet<NodeId> = graph.nodes[from.min(graph.nodes.len())..]
        .iter()
        .filter(|n| matches!(n.op, Op::Guard { .. }))
        .filter_map(|n| n.inputs.first().copied())
        .collect();
    loop_headers(graph)
        .into_iter()
        .filter(|&(_, entry_pred, _)| guarded.contains(&entry_pred))
        .map(|(region, _, _)| region)
        .collect()
}

/// [`unroll`] restricted to the loop headers in `only` (every header when
/// `None`), and never a header a partial unroll already took.
pub(in crate::ir_optimize) fn unroll_in(
    graph: &mut Graph,
    only: Option<&FxHashSet<NodeId>>,
    late: bool,
) -> bool {
    let dbg = false;
    // Per-call, in `UnrollCensus` field order. See `publish_unroll_census`.
    let mut census = [0usize; 17];
    // Computed ONCE for the whole graph, before any header is transformed:
    // unrolling only clones pure nodes and `Op::Load`s, so it cannot introduce
    // a trapping op, and re-deriving it per header would give the same answer
    // at N times the cost.
    let trap_free = crate::ir_lower::graph_cannot_deopt(graph);
    if dbg {
        eprintln!("[DBG_UNROLL] graph trap-free: {trap_free}");
    }
    // Normalize away the single-input Merge wrappers the builder puts around
    // branch projections, so loop headers/back-edges sit next to their If/Proj.
    collapse_trivial_merges(graph);
    // The r23 L8 close: the loops the vector pre-loop will serve stay as they
    // are (`unroll_yields_to_vector_enabled`). Planned once, before any
    // header is transformed; only a graph with an element read can have one.
    let vector_headers: Vec<NodeId> = if unroll_yields_to_vector_enabled()
        && graph.nodes.iter().any(|n| matches!(n.op, Op::ArrayLoad(_)))
    {
        crate::ir_vector_loop::vector_loop_headers(graph)
    } else {
        Vec::new()
    };
    // Loop headers: a `Region`, or a `Merge` with a back-edge — the builder emits
    // `Merge` headers for javac loops. Two control inputs (entry + back-edge).
    let headers: Vec<NodeId> = (0..graph.nodes.len() as NodeId)
        .filter(|&i| {
            matches!(graph.nodes[i as usize].op, Op::Region | Op::Merge)
                && graph.nodes[i as usize].inputs.len() >= 2
        })
        .collect();
    if dbg {
        eprintln!("[DBG_UNROLL] {} candidate loop header(s)", headers.len());
    }
    // Which of those merges an edge retreats to — the only ones that can be
    // loop headers (see `retreating_edge_targets`). Every other merge used to
    // pay a users-map rebuild and a forward closure below, O(nodes) each, only
    // to land in `not_single_backedge` — where it still lands, so the census
    // reads exactly as before. Computed once: unrolling a loop turns no
    // if/else join into a loop header.
    //
    // One users map for the whole pass, rebuilt only after a transform has
    // edited the graph (round 12 wave 2). It used to be rebuilt for every loop
    // header, one vector per arena node each time. Every check between two
    // transforms only READS the graph (`partial_unroll_loop` refuses before its
    // first edit), so the map a header reads is exactly the one it built.
    let mut users = build_users(graph);
    let loop_candidates = retreating_edge_targets(graph, &users);
    let mut users_stale = false;
    let mut changed = false;
    for region in headers {
        if !matches!(graph.nodes[region as usize].op, Op::Region | Op::Merge) {
            continue; // killed by an earlier unroll in this pass
        }
        if only.is_some_and(|s| !s.contains(&region))
            || graph.partially_unrolled_headers.contains(&region)
        {
            continue;
        }
        if vector_headers.contains(&region) {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: left to the vector pre-loop");
            }
            continue;
        }
        // The LATE round leaves the compiling method's own loops of an OSR
        // graph alone: their headers are OSR entries, whose stubs re-run the
        // pre-header's hoisted reads and predicates and refuse an entry the
        // main/post rewrite of its pre-header no longer describes (the r16
        // matrix OSR tests lost their entry). A loop spliced from a callee is
        // never an OSR entry (`spliced_loop_entered_only_through_its_pre_header`).
        if late
            && (graph.optimize_osr_route || graph.osr_compile)
            && !header_is_spliced(graph, region)
        {
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: late round leaves an OSR graph's own loop"
                );
            }
            continue;
        }
        census[unroll_bucket::HEADERS] += 1;
        let rin = graph.nodes[region as usize].inputs.clone();
        if rin.len() != 2 {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — not_single_backedge");
            }
            census[unroll_bucket::NOT_SINGLE_BACKEDGE] += 1;
            continue; // single back-edge only
        }
        if loop_candidates
            .as_ref()
            .is_some_and(|c| !c.contains(&region))
        {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — not_single_backedge");
            }
            census[unroll_bucket::NOT_SINGLE_BACKEDGE] += 1;
            continue; // no edge retreats to it: not a loop header
        }
        if users_stale {
            users = build_users(graph);
            users_stale = false;
        }
        // Identify the back-edge input (control-reachable from the header) vs the
        // loop-entry predecessor.
        let reach = forward_control_closure(graph, &users, region);
        let mut back_inputs: Vec<NodeId> = rin
            .iter()
            .copied()
            .filter(|&i| i != NO_NODE && reach.contains(&i))
            .collect();
        // A NESTED INNER header has both inputs reachable from itself: the
        // walk leaves through the inner exit, goes round the outer back edge
        // and comes back to the inner pre-header. `loop_headers` solved the
        // same blind spot with dominance; this used to count every inner loop
        // as `not_single_backedge` (round 11 wave 2). Consulted only when
        // reachability did not give exactly one back edge, so a loop it
        // already classifies keeps its answer. The `variant` closure below is
        // what makes an inner loop safe to unroll once it is found.
        if back_inputs.len() != 1 {
            let (entries, backs) = entry_preds_by_dominance(graph, &users, region);
            if entries.len() == 1 && backs.len() == 1 {
                back_inputs = backs;
            }
        } else if !back_edges_dominated_by(graph, &users, region, &back_inputs) {
            // A latch the method entry reaches without passing the header: a
            // second entry into the cycle (irreducible CFG). Cloning its body
            // would duplicate an iteration another entry joins mid-way. Same
            // rule as `loop_headers`; declines nothing in a reducible graph.
            back_inputs.clear();
        }
        if back_inputs.len() != 1 {
            if dbg {
                eprintln!("[DBG_UNROLL] header {region}: bail — not a single-back-edge loop (back={back_inputs:?})");
            }
            census[unroll_bucket::NOT_SINGLE_BACKEDGE] += 1;
            continue;
        }
        let back_ctrl = back_inputs[0];
        let entry_pred = rin
            .iter()
            .copied()
            .find(|&i| i != back_ctrl)
            .unwrap_or(NO_NODE);
        if entry_pred == NO_NODE {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — not_single_backedge");
            }
            census[unroll_bucket::NOT_SINGLE_BACKEDGE] += 1;
            continue;
        }
        // ── The phi slot the entry value lives in, DERIVED and not assumed ──
        //
        // A `Phi`'s value inputs are `[anchor, v_for_pred0, v_for_pred1, …]`,
        // aligned with the header's own predecessor list — which this function
        // has just classified STRUCTURALLY, because `loop_headers` records that
        // a `Merge` header's slot order is not fixed by the builder.
        //
        // The full-unroll emitter below nonetheless read `inputs[1]` as the
        // pre-header value and `inputs[2]` as the back-edge value. On a header
        // whose back edge sits at slot 0 those are the wrong way round, and the
        // emitter would seed iteration 0 with an in-loop value and advance with
        // the pre-header one: wrong code, not a lost transform.
        //
        // It has never fired, because `analyze_counted_loop` makes the SAME
        // undeclared assumption one step earlier (it reads `inputs[1]` as the
        // induction variable's constant init, finds the `Add` there instead and
        // reports `NotCounted::Shape`), so a flipped header leaves as
        // "not counted" before the emitter is reached. That is a correctness
        // argument resting on a coincidence in a different function, and the
        // natural next improvement to the recogniser — making IT slot-aware —
        // removes the coincidence without touching the emitter.
        //
        // So the slots are derived here and the emitter reads them. `rin.len()`
        // is 2 at this point, so the two are 1 and 2 in some order; anything
        // this cannot resolve bails on the control shape.
        let (entry_slot, back_slot) = match (
            rin.iter().position(|&p| p == entry_pred),
            rin.iter().position(|&p| p == back_ctrl),
        ) {
            (Some(e), Some(bk)) if e != bk => (1 + e, 1 + bk),
            _ => {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — control_shape");
                }
                census[unroll_bucket::CONTROL_SHAPE] += 1;
                continue;
            }
        };
        // `partial` selects the transform at the bottom of this body. Every
        // check between here and there is shared: a partial unroll clones the
        // same node set, under the same control-shape, side-effect, escape and
        // frame obligations. The two differ only in what they emit — a full
        // unroll retires the loop, a partial one keeps it — so sharing the
        // analysis is what keeps a widening of either from silently applying to
        // the other with no reasoning behind it.
        let mut partial = false;
        // A constant-trip loop over the full-unroll cap, handed to the
        // main/post form (round nio20 wave 5, lane L3). Its refusals there are
        // `trip_over_cap`, the bucket it landed in before.
        let mut over_cap_partial = false;
        let info = match analyze_counted_loop(graph, region, back_ctrl) {
            Ok(i) => i,
            Err(NotCounted::Shape) => {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — not_counted");
                }
                census[unroll_bucket::NOT_COUNTED] += 1;
                continue;
            }
            Err(NotCounted::RuntimeBound(rb)) => {
                census[unroll_bucket::RUNTIME_BOUND] += 1;
                if trap_free {
                    census[unroll_bucket::RUNTIME_BOUND_TRAP_FREE] += 1;
                }
                if partial_unroll_body_is_pure(graph, &users, region, back_ctrl, rb.if_node) {
                    census[unroll_bucket::RUNTIME_BOUND_PURE_BODY] += 1;
                }
                // Round 17: the main/post form has its own switch and does not
                // need the per-copy-test form's (`main_post_unroll_enabled`).
                if !partial_unroll_enabled() && !main_post_unroll_enabled() {
                    // Terminal here, and only here: with the flag off, "the
                    // bound is a value" is the whole disposition.
                    if dbg {
                        eprintln!("[DBG_UNROLL] region {region}: bail — runtime_bound_refused");
                    }
                    census[unroll_bucket::RUNTIME_BOUND_REFUSED] += 1;
                    continue;
                }
                partial = true;
                // The three bound-dependent fields are not merely unknown here,
                // they are unused: `partial_unroll_loop` never evaluates the
                // induction variable. `iv_phi` is `NO_NODE` so that the one
                // place the full-unroll emitter special-cases it
                // (`p == info.iv_phi`, which substitutes a concrete constant)
                // cannot match a real phi on this path even if the two emitters
                // are ever merged.
                CountedLoop {
                    iv_phi: NO_NODE,
                    iv_init: 0,
                    iv_stride: 0,
                    trip: 0,
                    if_node: rb.if_node,
                    exit_ctrl: rb.exit_ctrl,
                }
            }
            // Round nio20 wave 5 (lane L3): a constant bound is a loop-invariant
            // one, so the main/post form serves a loop too long to retire.
            // Only that form: the per-copy-test fallback keeps its own scope.
            Err(NotCounted::TripOverCap(rb))
                if unroll_wide_counted_enabled() && main_post_unroll_enabled() =>
            {
                partial = true;
                over_cap_partial = true;
                CountedLoop {
                    iv_phi: NO_NODE,
                    iv_init: 0,
                    iv_stride: 0,
                    trip: 0,
                    if_node: rb.if_node,
                    exit_ctrl: rb.exit_ctrl,
                }
            }
            Err(NotCounted::TripOverCap(_)) => {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — trip_over_cap");
                }
                census[unroll_bucket::TRIP_OVER_CAP] += 1;
                continue;
            }
        };

        // Control simplicity: the loop must be a single block —
        //   region → if_node → { back_ctrl (re-enter), exit_ctrl (leave) }.
        // (loop_body is unsuitable here: its pinned sweep cascades control
        // Projs/Returns that merely *reference* a body node into the set.)
        let region_ctrl_users: Vec<NodeId> = users[region as usize]
            .iter()
            .copied()
            .filter(|&u| graph.nodes[u as usize].op.is_control())
            .collect();
        if region_ctrl_users != [info.if_node] {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — region control users {region_ctrl_users:?} != [if {}]", info.if_node);
            }
            census[unroll_bucket::CONTROL_SHAPE] += 1;
            continue;
        }
        let mut if_users = users[info.if_node as usize].clone();
        if_users.sort_unstable();
        let mut expect_projs = vec![back_ctrl, info.exit_ctrl];
        expect_projs.sort_unstable();
        if if_users != expect_projs {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — if users {if_users:?} != projs {expect_projs:?}");
            }
            census[unroll_bucket::CONTROL_SHAPE] += 1;
            continue;
        }
        let back_ctrl_users: Vec<NodeId> = users[back_ctrl as usize]
            .iter()
            .copied()
            .filter(|&u| graph.nodes[u as usize].op.is_control())
            .collect();
        if back_ctrl_users != [region] {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — back_ctrl control users {back_ctrl_users:?} != [region {region}]");
            }
            census[unroll_bucket::CONTROL_SHAPE] += 1;
            continue;
        }

        // Carried phis (anchored at the region), each needs a back-edge value.
        let mut ok = true;
        let mut carried: Vec<NodeId> = Vec::new();
        for id in 0..graph.nodes.len() {
            let node = &graph.nodes[id];
            if node.op == Op::Phi && node.inputs.first().copied() == Some(region) {
                if node.inputs.len() < 3 {
                    ok = false;
                    break;
                }
                carried.push(id as NodeId);
            }
        }
        if !ok {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — control_shape");
            }
            census[unroll_bucket::CONTROL_SHAPE] += 1;
            continue;
        }
        let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();

        // Variance: nodes transitively using a carried phi (forward propagation).
        //
        // The closure does not pass through a phi of ANOTHER merge (round 11
        // wave 2). The control checks above have proved the loop is the single
        // block `region → if → back_ctrl`, so every merge other than `region`
        // is outside it and its phis hold one value for the loop's whole run.
        // Walking through them leaked out of an inner loop: exit, the outer
        // header's phi (whose back value is this loop's final value), then
        // every outer-loop value -- which the clone walk below then tried to
        // clone per iteration, a φ it cannot clone. For a top-level loop such a
        // phi is after the loop and nothing in the loop reads it.
        //
        // JIT r23 w2c close (under `CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT`, for
        // the partial forms): nor through the loop's EXIT projection. Control
        // reached through it is after the loop; for an inner loop it runs on
        // round the outer loop and back into THIS loop's pre-header, which
        // then read as variant -- a pre-header predicate guard and its hoisted
        // reads became "body nodes" the clone walk refused
        // (`W2cTypecheckLoops` nocast in `time()`'s OSR body).
        let exit_stops_variance = late && partial && unroll_main_post_deopt_enabled();
        let mut variant: FxHashSet<NodeId> = carried_set.clone();
        let mut vw: Vec<NodeId> = carried.clone();
        while let Some(c) = vw.pop() {
            for &u in &users[c as usize] {
                if graph.nodes[u as usize].op == Op::Phi && !carried_set.contains(&u) {
                    continue;
                }
                if exit_stops_variance && u == info.exit_ctrl {
                    continue;
                }
                if variant.insert(u) {
                    vw.push(u);
                }
            }
        }

        // No side effects in the loop: a Store/Call/alloc/ArrayLength/Guard/array
        // element access that is control-pinned to the loop, or floating and
        // loop-variant, is a side effect we do not model → bail. A node pinned
        // to control OUTSIDE the loop runs after it, however much it depends on
        // the carried values: `for (…) acc += i; println(acc);` is not a loop
        // with a side effect. (Reading plain variance used to refuse it.) (`ArrayLoad`/`ArrayStore` can throw
        // NPE/AIOOBE and `ArrayStore` mutates the heap, so unrolling a loop that
        // contains one would duplicate/reorder those effects — conservatively
        // decline.)
        //
        // nio21 wave 6 (lane L5, L5W5-2): except, under
        // `CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD` (default OFF), an
        // `ArrayLoad` that cannot deopt (`ir_lower::masked_array_load_cannot_deopt`)
        // whose only users are in-loop `KeepAlive`s: a full pure-body unroll
        // DROPS it with its fences (see `fenced_load_drop_set`).
        let mut fenced_loads: Vec<NodeId> = Vec::new();
        // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-2): the in-loop masked
        // loads a FULL unroll clones under
        // `CRATONVM_JIT_IR_UNROLL_CLONE_MASKED_LOAD`, each with the mask guard
        // on this loop's entry that its copies name as their guard token.
        let mut cloned_masked: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        // JIT r23 w2c close (`CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT`): the
        // in-loop array reads a main/post unroll clones with per-copy frames.
        // Each must end up in the clone set, and on the loop's memory chain
        // (both checked below, before the transform).
        // Only in the LATE round (after the loop-entry predicates): offered
        // earlier, a body whose reads a predicate would take to the pre-header
        // was copied four times with the reads still in it.
        let deopt_mpu =
            late && partial && main_post_unroll_enabled() && unroll_main_post_deopt_enabled();
        let edge_anchors: FxHashSet<NodeId> = crate::ir::exception_edge_anchors(graph)
            .into_iter()
            .collect();
        let mut deopt_reads: Vec<NodeId> = Vec::new();
        let mut static_read: Option<NodeId> = None;
        for id in 0..graph.nodes.len() {
            let op = &graph.nodes[id].op;
            if matches!(
                op,
                Op::Store(_)
                    | Op::Call { .. }
                    // Round 16 (lane indy): a bridged `invokedynamic`, the same.
                    | Op::IndyBridge { .. }
                    // Round 16 (lane matrix): `multianewarray`, the same.
                    | Op::MultiNewArray { .. }
                    // nio21 w15 L3: a deferred `new`, the same.
                    | Op::NewCp { .. }
                    // cov-01: a helper call that may allocate, intern or run
                    // `<clinit>` is a side effect this pass does not model, so
                    // a loop containing one is not unrolled.
                    | Op::ConstString { .. }
                    | Op::ConstClass { .. }
                    | Op::LoadStatic { .. }
                    | Op::StoreStatic { .. }
                    | Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::ArrayLength
                    | Op::ArrayLoad(_)
                    | Op::ArrayStore(_)
        | Op::ByteArrayCopy
                    | Op::Guard { .. }
            ) {
                let inputs = &graph.nodes[id].inputs;
                // The documented `[ctrl, …]` form; a compact hand-built node has
                // no control input and floats.
                let pinned = match op.memory_shape() {
                    Some(shape) => inputs.len() >= shape.min_full_arity,
                    None => matches!(op, Op::Guard { .. }),
                };
                let ctrl = if pinned {
                    inputs.first().copied().unwrap_or(NO_NODE)
                } else {
                    NO_NODE
                };
                let in_loop = if ctrl == NO_NODE {
                    variant.contains(&(id as NodeId))
                } else {
                    ctrl == region || ctrl == back_ctrl || ctrl == info.if_node
                };
                if in_loop
                    && !partial
                    && unroll_drop_fenced_load_enabled()
                    && fenced_load_droppable(
                        graph,
                        &users,
                        id as NodeId,
                        &[region, back_ctrl, info.if_node],
                    )
                {
                    fenced_loads.push(id as NodeId);
                    continue;
                }
                // LOOPOPT20-2: a load the guard on THIS loop's entry proves
                // (so a copy anchored there can name it), in the documented
                // four-input form; whether it is really cloned (its value
                // feeds the loop) is the clone walk's question, and the
                // pure-body escape below is required for it.
                if in_loop
                    && !partial
                    && matches!(op, Op::ArrayLoad(_))
                    && graph.nodes[id].inputs.len() == 4
                    && unroll_clone_masked_load_enabled()
                    && crate::ir_lower::masked_array_load_cannot_deopt(graph, &graph.nodes[id])
                {
                    if let Some(g) =
                        mask_predicate::entry_mask_guard_for(graph, entry_pred, &graph.nodes[id])
                    {
                        cloned_masked.insert(id as NodeId, g);
                        continue;
                    }
                }
                if in_loop
                    && deopt_mpu
                    && matches!(op, Op::ArrayLoad(_) | Op::ArrayLength | Op::StrCharDecode)
                    && pinned
                    && !graph.nodes[id].is_volatile_access()
                    && !edge_anchors.contains(&(id as NodeId))
                {
                    deopt_reads.push(id as NodeId);
                    continue;
                }
                if in_loop {
                    // LOOPSTATIC21-1 census: a direct static read only bails
                    // the loop if nothing else in it does, so keep scanning.
                    if matches!(op, Op::LoadStatic { .. }) {
                        static_read.get_or_insert(id as NodeId);
                        continue;
                    }
                    if dbg {
                        eprintln!("[DBG_UNROLL] region {region}: bail — loop side effect {:?} (node {id})", graph.nodes[id].op);
                    }
                    ok = false;
                    break;
                }
            } else if matches!(op, Op::Load(_)) {
                let inputs = &graph.nodes[id].inputs;
                let pinned = op
                    .memory_shape()
                    .is_some_and(|shape| inputs.len() >= shape.min_full_arity);
                let ctrl = if pinned {
                    inputs.first().copied().unwrap_or(NO_NODE)
                } else {
                    NO_NODE
                };
                let in_loop = if ctrl == NO_NODE {
                    variant.contains(&(id as NodeId))
                } else {
                    ctrl == region || ctrl == back_ctrl || ctrl == info.if_node
                };
                if in_loop {
                    if graph.nodes[id].is_volatile_access()
                        || edge_anchors.contains(&(id as NodeId))
                    {
                        if dbg {
                            eprintln!(
                                "[DBG_UNROLL] region {region}: bail — volatile or catch-anchored Load (node {id})"
                            );
                        }
                        ok = false;
                        break;
                    }
                    if partial && !crate::ir_lower::node_cannot_deopt(graph, &graph.nodes[id]) {
                        if deopt_mpu && pinned {
                            deopt_reads.push(id as NodeId);
                        } else {
                            if dbg {
                                eprintln!(
                                    "[DBG_UNROLL] region {region}: bail — deopting Load awaits late deopt_mpu (node {id})"
                                );
                            }
                            ok = false;
                            break;
                        }
                    }
                }
            }
        }
        if let (true, Some(id)) = (ok, static_read) {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — loop side effect LoadStatic is the only one (node {id})");
            }
            ok = false;
        }
        if !ok {
            census[unroll_bucket::SIDE_EFFECT] += 1;
            continue;
        }

        // Per-iteration computation = variant nodes backward-reachable from each
        // carried phi's back-edge value and the loop condition; stops at carried
        // phis (substituted) and invariant nodes (shared). Excludes post-loop uses.
        let cond = graph.nodes[info.if_node as usize].inputs[1];
        let mut seeds: Vec<NodeId> = vec![cond];
        for &p in &carried {
            // `back_slot`, not a literal 2 — see the derivation above.
            seeds.push(graph.nodes[p as usize].inputs[back_slot]);
        }
        // JIT r27 LOOP27-2: a narrowed `long` induction's frames name `i2l` of
        // the induction and of its step and the old `lcmp`, which no node reads. Each copy's frame
        // needs its own, so they are cloned like the body.
        for sp in graph.safepoints.iter() {
            for &slot in sp.locals.iter().chain(sp.stack.iter()) {
                if slot != NO_NODE
                    && variant.contains(&slot)
                    && !carried_set.contains(&slot)
                    && matches!(graph.nodes[slot as usize].op, Op::I2L | Op::LCmp)
                    && crate::ir_vector_loop::frame_only_wide(graph, &users, slot)
                {
                    seeds.push(slot);
                }
            }
        }
        let mut to_clone: FxHashSet<NodeId> = FxHashSet::default();
        let mut cw = seeds;
        // The nodes compiled catch arms hang on (`exception_edge_anchors`): a
        // cast among them keeps the loop out of the deopt main/post form.
        let cast_anchors: FxHashSet<NodeId> = if deopt_mpu {
            crate::ir::exception_edge_anchors(graph)
                .into_iter()
                .collect()
        } else {
            FxHashSet::default()
        };
        // Under `CRATONVM_DBG_UNROLL`: who reached each node, for the bail line.
        let mut reached_from: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        while let Some(c) = cw.pop() {
            if c == NO_NODE
                || c as usize >= graph.nodes.len()
                || carried_set.contains(&c)
                || !variant.contains(&c)
            {
                continue; // phi (substituted) or invariant (shared)
            }
            if graph.nodes[c as usize].op.is_control() {
                continue;
            }
            if to_clone.contains(&c) {
                continue;
            }
            // Only loads, pure arithmetic and (round 9 wave 11) a division by
            // a non-zero constant are clonable; see `unroll_body_node_clonable`.
            // LOOPOPT20-2: or a masked load the loop's entry guard proves.
            // JIT r23 w2c close: and, in the late deopt main/post form, a
            // resolved `checkcast` with no compiled catch arm -- its CCE exit
            // resumes through the node's own frame, which each copy gets.
            let deopt_cast = deopt_mpu
                && matches!(graph.nodes[c as usize].op, Op::CheckCast { name_ptr, .. } if name_ptr != 0)
                && !cast_anchors.contains(&c);
            if !unroll_body_node_clonable(graph, c)
                && !cloned_masked.contains_key(&c)
                && !deopt_reads.contains(&c)
                && !deopt_cast
            {
                let opc = &graph.nodes[c as usize].op;
                if dbg {
                    let mut path = vec![c];
                    let mut at = c;
                    while let Some(&p) = reached_from.get(&at) {
                        if path.len() > 16 {
                            break;
                        }
                        path.push(p);
                        at = p;
                    }
                    eprintln!(
                        "[DBG_UNROLL] region {region}: bail — unclonable body node {opc:?} (node {c}; \
                         reached from {path:?})"
                    );
                }
                ok = false;
                break;
            }
            to_clone.insert(c);
            for &inp in &graph.nodes[c as usize].inputs {
                if dbg {
                    reached_from.entry(inp).or_insert(c);
                }
                cw.push(inp);
            }
        }
        if !ok {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — body_unclonable");
            }
            census[unroll_bucket::BODY_UNCLONABLE] += 1;
            continue;
        }
        if to_clone.len() > UNROLL_MAX_BODY {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — body_unclonable");
            }
            census[unroll_bucket::BODY_UNCLONABLE] += 1;
            continue;
        }
        // JIT r23 w2c close: an admitted in-loop read the clone walk did not
        // reach would run in the post loop only -- the main loop would skip
        // its fault. Refuse.
        if deopt_reads.iter().any(|r| !to_clone.contains(r)) {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — an in-loop read is not cloned");
            }
            census[unroll_bucket::SIDE_EFFECT] += 1;
            continue;
        }
        // Round nio20 wave 5 (lane L3): a trip past `UNROLL_MAX_TRIP` (only
        // recognised under `CRATONVM_JIT_IR_UNROLL_WIDE_COUNTED`) is retired
        // only over a small body. Same bucket as an over-cap trip count.
        if !partial
            && info.trip > UNROLL_MAX_TRIP
            && usize::try_from(info.trip)
                .unwrap_or(usize::MAX)
                .saturating_mul(to_clone.len())
                > UNROLL_MAX_TOTAL_WIDE
        {
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: bail — trip {} x {} body nodes over the \
                     wide budget {UNROLL_MAX_TOTAL_WIDE}",
                    info.trip,
                    to_clone.len()
                );
            }
            census[unroll_bucket::TRIP_OVER_CAP] += 1;
            continue;
        }
        // A partial unroll emits `factor` copies of the body and KEEPS them —
        // a full unroll's copies mostly fold away against concrete induction
        // constants, and these cannot, because there is no trip count to make
        // them concrete. So the budget applies to the emitted total, not to one
        // copy. An empty body is refused for the separate reason that
        // unrolling one buys nothing and still adds `factor - 1` tests.
        let factor = partial_unroll_factor();
        if partial && (to_clone.is_empty() || to_clone.len() * factor > UNROLL_MAX_BODY) {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — body_unclonable");
            }
            census[unroll_bucket::BODY_UNCLONABLE] += 1;
            continue;
        }
        // Refuse a loop whose body (or carried phi) is named by a safepoint
        // snapshot.
        //
        // Unrolling kills every `to_clone` node and every carried phi below.
        // Those kills do not route `graph.safepoints`, so `eliminate_dead_nodes`'
        // defensive normalisation later rewrites the stranded slots to
        // `NO_NODE`, which `ir_lower::frame_value_for` resolves to
        // `FrameValue::Undefined` — a deopt into an unrolled loop silently
        // loses those interpreter locals.
        //
        // Refusing is the correct minimal fix rather than cloning the
        // snapshots: `SafepointSnapshot` is keyed by a single `bci`, so a loop
        // unrolled `trip` times has `trip` copies of each body bci and cannot
        // represent per-iteration frames with one snapshot. Representing them
        // needs a per-iteration snapshot index, which is a lowerer change.
        // nio21 wave 6 (lane L5): the fenced loads the screen admitted, their
        // `KeepAlive`s and their private index computation, retired with the
        // loop. Empty unless `CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD`.
        let dropped: FxHashSet<NodeId> = if fenced_loads.is_empty() {
            FxHashSet::default()
        } else {
            match fenced_load_drop_set(
                graph,
                &users,
                &fenced_loads,
                &to_clone,
                &carried_set,
                &variant,
            ) {
                Some(d) => d,
                None => {
                    if dbg {
                        eprintln!(
                            "[DBG_UNROLL] region {region}: bail — a fenced load's index is shared"
                        );
                    }
                    census[unroll_bucket::SIDE_EFFECT] += 1;
                    continue;
                }
            }
        };
        let body_named_by_safepoint = graph.safepoints.iter().any(|sp| {
            sp.locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .any(|&slot| {
                    slot != NO_NODE
                        && (to_clone.contains(&slot)
                            || carried_set.contains(&slot)
                            || dropped.contains(&slot))
                })
        });
        // ...unless NOTHING in this graph can transfer to the interpreter, in
        // which case no snapshot can ever be consulted and the objection does
        // not apply.
        //
        // The refusal above is correct and its reasoning is unchanged: a
        // `SafepointSnapshot` is keyed by one `bci`, and `trip` copies of a
        // body bci cannot be represented by one snapshot. What it did not ask
        // is whether any of those snapshots is REACHABLE. On a trap-free graph
        // none is -- the same fact `lower_inner`'s `graph_trap_free` already
        // computes and acts on when it skips building unreachable deopt points,
        // now asked one pass earlier, where the unroller can use it.
        //
        // This is a PREDICTION, and it is backed the same way that one is: if
        // the emission disagrees, `build_deopt_points` builds every point,
        // `ir_lower::frame_value_for` meets a slot it cannot describe, and the compile is
        // refused. Nothing is taken away; a case is added in which the net is
        // provably not needed.
        //
        // Measured reach: the unroller previously fired only on loops whose
        // body no snapshot names, which on javac output is almost none -- a
        // snapshot records the full operand stack at every bci, so any loop
        // whose body leaves a value on the stack was refused.
        //
        // ...or unless each copy gets a frame of its OWN, which is what the
        // comment above calls "a per-iteration snapshot index" and what
        // `ir_per_copy_frames_enabled` turns on. That path does not rest on any
        // prediction about reachability: it answers the objection rather than
        // arguing the objection cannot be reached, so it needs neither the
        // trap-free fact nor the net below.
        let mut per_copy: Option<Vec<(u32, SafepointSnapshot)>> = None;
        // Did THIS loop's admission rest on the unreachable-frames relaxation?
        // Published to `UNROLL_USED_UNREACHABLE_FRAMES` only after commit.
        let mut uses_unreachable_frames = false;
        let mut mpu_only = false;
        // Round nio20 wave 5 (lane L3): ...or unless the LOOP cannot deopt and
        // no snapshot it would strand is reachable from a node the transform
        // keeps (`pure_body_frames_unconsultable`). The copies are then emitted
        // with no bytecode pc, as the main/post form emits its copies, so no
        // deopt point is ever anchored at them. Asked per loop, where the
        // trap-free escape asks per METHOD -- which one call anywhere answers
        // `false`. Tried before the per-copy frames, which it makes
        // unnecessary.
        let mut strip_copy_pcs = false;
        // LOOPOPT20-6: the escape held only by its refinement
        // (`pure_body_frames_verdict` answered `Some(true)`).
        let mut frames_refined = false;
        // nio21 wave 6 (lane L5): a drop is sound only in a pc-less pure-body
        // copy sequence (no deopt, no poll, no call between a load and its
        // fence), so it asks for that escape whether or not a snapshot names
        // the body, and treats the dropped nodes as clones in its frame test.
        // LOOPOPT20-2: so is a clone of a masked load (its copies are proven
        // only pc-less, by their token, and a deopt there would need a frame).
        let clones_masked = to_clone.iter().any(|c| cloned_masked.contains_key(c));
        let must_strip = !dropped.is_empty() || clones_masked;
        if (body_named_by_safepoint || must_strip) && !partial && unroll_pure_body_enabled() {
            let mut killed: FxHashSet<NodeId> = to_clone.clone();
            killed.extend(carried.iter().copied());
            killed.extend([region, info.if_node, back_ctrl, info.exit_ctrl]);
            killed.extend(dropped.iter().copied());
            let gone: std::borrow::Cow<'_, FxHashSet<NodeId>> = if must_strip {
                let mut g = to_clone.clone();
                g.extend(dropped.iter().copied());
                std::borrow::Cow::Owned(g)
            } else {
                std::borrow::Cow::Borrowed(&to_clone)
            };
            if let Some(refined) = pure_body_frames_verdict(graph, &killed, &gone, &carried_set) {
                strip_copy_pcs = true;
                frames_refined = refined;
                if dbg && refined {
                    eprintln!(
                        "[DBG_UNROLL] region {region}: pure-body escape by the named-frame \
                         rule (LOOPOPT20-6)"
                    );
                }
            } else if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: pure-body escape declined (a clone can \
                     deopt, or a stranded frame is reachable)"
                );
            }
        }
        if must_strip && !strip_copy_pcs {
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: bail — a fenced load needs the pure-body \
                     escape"
                );
            }
            census[unroll_bucket::SIDE_EFFECT] += 1;
            continue;
        }
        if body_named_by_safepoint
            && !strip_copy_pcs
            && ir_per_copy_frames_enabled()
            && !(partial && main_post_unroll_enabled())
        {
            // JIT round 20 wave 3 (lane loopopt): the copies' frames rename
            // only clones and carried phis. A snapshot naming another
            // loop-variant node (a dead local `t = i * 3` the builder still
            // records) would keep the original, which after the transform
            // holds another iteration's value: refuse rather than describe
            // a copy with it.
            // Only the snapshots the copies take -- exactly the ones
            // `plan_copy_frames` plans; a frame after the loop names post-loop
            // values the teardown repairs.
            // JIT round 20 wave 7 (lane revIR; correctness, unconditional):
            // asked of the PLAN, not of the flat snapshots at a clone's own
            // bci. The plan also takes every CHAIN snapshot at a spliced
            // clone's combined pc (whose outer scope names the compiling
            // method's locals) and the flat frame at a spliced clone's
            // outermost `invoke` (`cloned_resume_bcis`); a copy resumes
            // through one of those when a field load of the spliced body
            // traps. Both were skipped, so a dead local of the caller came
            // back from such a deopt holding the original node's value
            // (`R20RevIRUnrollSpliceStaleLocal`).
            match plan_copy_frames(graph, &to_clone) {
                Ok(plan) => {
                    let stale_variant = plan.iter().any(|(_, sp)| {
                        sp.locals
                            .iter()
                            .chain(sp.stack.iter())
                            .chain(sp.monitors.iter())
                            .any(|&v| {
                                v != NO_NODE
                                    && variant.contains(&v)
                                    && !to_clone.contains(&v)
                                    && !carried_set.contains(&v)
                                    && !dropped.contains(&v)
                            })
                    });
                    if stale_variant {
                        if dbg {
                            for (_, sp) in plan.iter() {
                                for &v in sp
                                    .locals
                                    .iter()
                                    .chain(sp.stack.iter())
                                    .chain(sp.monitors.iter())
                                {
                                    if v != NO_NODE
                                        && variant.contains(&v)
                                        && !to_clone.contains(&v)
                                        && !carried_set.contains(&v)
                                        && !dropped.contains(&v)
                                    {
                                        eprintln!(
                                            "[DBG_UNROLL] region {region}: stale slot n{v} {:?} (pc {:?}) in the frame at bci {}",
                                            graph.nodes[v as usize].op, graph.nodes[v as usize].bytecode_pc, sp.bci
                                        );
                                    }
                                }
                            }
                            eprintln!(
                                "[DBG_UNROLL] region {region}: bail — a snapshot names a \
                                 loop-variant node no copy renames"
                            );
                        }
                        census[unroll_bucket::FRAME_UNCOPYABLE] += 1;
                        continue;
                    }
                    per_copy = Some(plan);
                }
                Err(()) => {
                    if dbg {
                        eprintln!("[DBG_UNROLL] region {region}: bail — a snapshot names the body at a bci no clone carries");
                    }
                    census[unroll_bucket::FRAME_UNCOPYABLE] += 1;
                    continue;
                }
            }
        }
        if body_named_by_safepoint && per_copy.is_none() && !strip_copy_pcs {
            // A PARTIAL unroll may not take the unreachable-frames escape, and
            // the reason is not caution — the escape does not apply to it.
            //
            // It rests on `build_deopt_points` skipping every point in a
            // trap-free graph, which makes the collapsed frames unconsultable.
            // For a full unroll that is the whole story, because the loop is
            // gone and there is no other reader. A partial unroll KEEPS the
            // loop, and the safepoint POLL on its back edge is emitted whether
            // or not the graph can deopt — so the snapshots at the body's bcis
            // remain live oop-map material describing copy 0 while copies
            // `1..factor` execute. Per-copy frames are not an optimization
            // here; they are the transform's precondition.
            //
            // Round 17: except for the main/post form, which keeps the
            // original body (copy 0's frames describe it, unchanged, in the
            // post loop) and clones the copies with no bci, under a header
            // with no bci -- no snapshot can describe them, and none is asked
            // to (a pure body has no deopt point; the main poll has no mode
            // exit). It alone may proceed; see `mpu_only` below.
            if partial && main_post_unroll_enabled() {
                mpu_only = true;
            } else if partial {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — safepoint_named");
                }
                census[unroll_bucket::SAFEPOINT_NAMED] += 1;
                continue;
            }
            // The relaxation additionally requires the CONSUMER of the
            // trap-free fact to be available: if `build_deopt_points` is not
            // going to skip the points, the snapshots are live and the original
            // refusal stands.
            if !mpu_only
                && !(unroll_over_unreachable_frames()
                    && trap_free
                    && crate::ir_lower::ir_drop_unreachable_homes_enabled())
            {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — safepoint_named");
                }
                census[unroll_bucket::SAFEPOINT_NAMED] += 1;
                continue;
            }
            // Recorded here, published only once the transform COMMITS (the
            // full-unroll tail below). Setting the net at this point -- as it
            // was until round 9 wave 2 -- left it set by a loop the later
            // `ESCAPES_OR_PINNED` refusals then declined, and `lower_inner`
            // refused a method whose graph the relaxation never touched.
            uses_unreachable_frames = !mpu_only;
        }
        // Bail on invariant loads pinned to the loop header (we'd have to
        // re-anchor them); milestone-1 only handles variant (cloned) loads.
        let mut pinned_invariant_load = false;
        for id in 0..graph.nodes.len() {
            // Both body anchors, for the reason the `subst` above now covers
            // both: an invariant load pinned to the BACK EDGE is left naming a
            // killed control node just as surely as one pinned to the header,
            // and this check is what keeps it from getting there.
            if matches!(graph.nodes[id].op, Op::Load(_))
                && matches!(
                    graph.nodes[id].inputs.first().copied(),
                    Some(c) if c == region || c == back_ctrl
                )
                && !to_clone.contains(&(id as NodeId))
            {
                pinned_invariant_load = true;
                break;
            }
        }
        if pinned_invariant_load {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — invariant load pinned to header");
            }
            census[unroll_bucket::ESCAPES_OR_PINNED] += 1;
            continue;
        }
        // JIT round 20 wave 3 (lane loopopt; correctness, unconditional): any
        // OTHER node anchored on the loop's control -- a `KeepAlive` whose
        // referent is not a dropped fenced load (`reachabilityFence(this)` in
        // a pure counted loop), a node pinned to the test -- is neither cloned
        // nor retired, and would keep a control input the teardown kills.
        // Clones are re-anchored through `subst`, carried phis and the test's
        // projections are retired, dropped nodes are killed.
        // A partial unroll keeps the loop and its anchors, so only a full one asks.
        let stray_pinned = !partial
            && (0..graph.nodes.len()).any(|id| {
                let nid = id as NodeId;
                let n = &graph.nodes[id];
                n.op != Op::Dead
                    && !n.op.is_control()
                    && matches!(
                        n.inputs.first().copied(),
                        Some(c) if c == region || c == back_ctrl || c == info.if_node
                    )
                    && !to_clone.contains(&nid)
                    && !carried_set.contains(&nid)
                    && !dropped.contains(&nid)
            });
        if stray_pinned {
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: bail — a node pinned to the loop is neither \
                     cloned nor retired"
                );
            }
            census[unroll_bucket::ESCAPES_OR_PINNED] += 1;
            continue;
        }

        // Safety: every user of a to-clone node must be another to-clone node, a
        // carried phi, or the header `If` — i.e. the computation feeds only the
        // loop (its results escape solely via the carried phis we redirect).
        let mut escapes = false;
        for &d in &to_clone {
            for &u in &users[d as usize] {
                if !(to_clone.contains(&u) || carried_set.contains(&u) || u == info.if_node) {
                    escapes = true;
                    break;
                }
            }
            if escapes {
                break;
            }
        }
        if escapes {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — a body value escapes the loop");
            }
            census[unroll_bucket::ESCAPES_OR_PINNED] += 1;
            continue;
        }

        // Topological order of the clone set (DAG — carried phis broke cycles).
        let mut order: Vec<NodeId> = Vec::with_capacity(to_clone.len());
        let mut state: FxHashMap<NodeId, u8> = FxHashMap::default();
        let mut stack: Vec<(NodeId, usize)> = Vec::new();
        for &root in &to_clone {
            if state.get(&root).copied().unwrap_or(0) == 2 {
                continue;
            }
            stack.push((root, 0));
            // `last_mut()` rather than a `last()` read followed by a second
            // `last_mut().unwrap()`: the cursor bump is the only mutation, so
            // taking the mutable borrow once removes the unwrap entirely. A
            // panic here would land on a background compiler thread, where it
            // is a VM crash rather than a failed compile.
            while let Some(top) = stack.last_mut() {
                let (node, idx) = (top.0, top.1);
                if idx < graph.nodes[node as usize].inputs.len() {
                    top.1 += 1;
                }
                state.insert(node, 1);
                let inputs = &graph.nodes[node as usize].inputs;
                if idx < inputs.len() {
                    let inp = inputs[idx];
                    if to_clone.contains(&inp) && state.get(&inp).copied().unwrap_or(0) == 0 {
                        stack.push((inp, 0));
                    }
                } else {
                    state.insert(node, 2);
                    order.push(node);
                    stack.pop();
                }
            }
        }

        // Round 17: the main-loop + post-loop form first, when it applies;
        // the per-copy-test form below is the fallback.
        if partial && main_post_unroll_enabled() {
            // JIT r23 w2c close: a body that can deopt gets per-copy frames,
            // when its deopt points are ordered across copies.
            let mpu_frames = if deopt_mpu
                && order.iter().any(|&c| {
                    graph
                        .node_opt(c)
                        .is_some_and(|n| !crate::ir_lower::node_cannot_deopt(graph, n))
                }) {
                main_post_copy_frames(graph, &to_clone, &carried, &variant, region, dbg)
            } else {
                None
            };
            // j27 wave 8 (lane W8LOOP, `CRATONVM_JIT_IR_UNROLL_WIDE_PURE`,
            // opt-in): a body no node of which can deopt may take more copies.
            let pure_body = order.iter().all(|&c| {
                graph
                    .node_opt(c)
                    .is_some_and(|n| crate::ir_lower::node_cannot_deopt(graph, n))
            });
            let mpu_factor = unroll_wide_pure::main_post_factor(
                factor,
                order.len(),
                pure_body,
                UNROLL_MAX_TOTAL_WIDE,
            );
            if dbg && mpu_factor != factor {
                eprintln!(
                    "[DBG_UNROLL] region {region}: main/post factor {factor} -> {mpu_factor} \
                     (wide pure)"
                );
            }
            if main_post_unroll_loop(
                graph,
                &PartialUnroll {
                    region,
                    back_ctrl,
                    if_node: info.if_node,
                    factor: mpu_factor,
                },
                &carried,
                &order,
                &variant,
                mpu_frames.as_deref(),
                dbg,
            ) {
                census[unroll_bucket::PARTIALLY_UNROLLED] += 1;
                crate::ir_evidence::note(crate::ir_evidence::Transform::MainPostUnrolled);
                changed = true;
                users_stale = true;
                continue;
            }
            // An over-cap constant-trip loop declined by the main/post form is
            // refused for its trip count, as it was before it was offered
            // (round nio20 wave 5, lane L3); the per-copy-test fallback never
            // sees one.
            if over_cap_partial {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — trip_over_cap");
                }
                census[unroll_bucket::TRIP_OVER_CAP] += 1;
                continue;
            }
            if mpu_only {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — safepoint_named");
                }
                census[unroll_bucket::SAFEPOINT_NAMED] += 1;
                continue;
            }
            // The per-copy-test fallback only under its own switch.
            if !partial_unroll_enabled() {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — runtime_bound_refused");
                }
                census[unroll_bucket::RUNTIME_BOUND_REFUSED] += 1;
                continue;
            }
        }
        if over_cap_partial {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — trip_over_cap");
            }
            census[unroll_bucket::TRIP_OVER_CAP] += 1;
            continue;
        }
        if partial {
            match partial_unroll_loop(
                graph,
                &PartialUnroll {
                    region,
                    back_ctrl,
                    if_node: info.if_node,
                    factor,
                },
                &carried,
                &order,
                per_copy.as_deref(),
                dbg,
            ) {
                Ok(()) => graph.partially_unrolled_headers.push(region),
                Err(bucket) => {
                    if dbg {
                        eprintln!(
                            "[DBG_UNROLL] region {region}: partial bail -- census bucket {bucket}"
                        );
                    }
                    census[bucket] += 1;
                    continue;
                }
            }
            census[unroll_bucket::PARTIALLY_UNROLLED] += 1;
            if per_copy.is_some() {
                census[unroll_bucket::PER_COPY_FRAMES] += 1;
            }
            changed = true;
            users_stale = true;
            continue;
        }

        // Running value of each carried phi entering the current iteration,
        // seeded with its preheader (entry) value.
        let mut cur: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        for &p in &carried {
            // `entry_slot`, not a literal 1 — see the derivation above.
            cur.insert(p, graph.nodes[p as usize].inputs[entry_slot]);
        }

        for t in 0..info.trip {
            // Substitution for iteration t: region control → preheader; each
            // carried phi → its running value (the iv's running value is the
            // concrete constant init + t*stride).
            let mut subst: FxHashMap<NodeId, NodeId> = FxHashMap::default();
            subst.insert(region, entry_pred);
            // ...and the OTHER control node a body value can be anchored to.
            //
            // This pass's own side-effect scan spells the body's control
            // anchors `ctrl == region || ctrl == back_ctrl`, and `subst` covered
            // only the first. A cloned `Op::Load` pinned to the back edge
            // therefore kept `back_ctrl` as its control input across the
            // teardown's `graph.kill(back_ctrl)`, and every copy pointed at a
            // `Op::Dead`. `ir_verify`'s structural lane caught it -- "n24:
            // Load(Int) input[0] = n16 refers to a removed (Dead) node", ten
            // violations for five copies of a two-load body -- and the compile
            // was refused, so the shape never reached the code. It was
            // unreachable before per-copy frames only because a loop with a
            // loop-carried receiver is one a safepoint snapshot names.
            //
            // `entry_pred` is right for both: a fully unrolled loop runs every
            // iteration unconditionally in the preheader, so every body anchor
            // becomes the preheader.
            subst.insert(back_ctrl, entry_pred);
            // JIT round 20 wave 3 (lane loopopt): and a clone anchored on the
            // test itself.
            subst.insert(info.if_node, entry_pred);
            for (&p, &v) in &cur {
                subst.insert(p, v);
            }
            // Clone the per-iteration computation in topo order.
            let mut iter_clones: Vec<NodeId> = Vec::with_capacity(order.len());
            for &d in &order {
                let (op, ty, pc, volatile) = {
                    let nd = &graph.nodes[d as usize];
                    (nd.op.clone(), nd.ty, nd.bytecode_pc, nd.volatile_access)
                };
                let mut new_inputs: Vec<NodeId> = graph.nodes[d as usize]
                    .inputs
                    .iter()
                    .map(|&inp| subst.get(&inp).copied().unwrap_or(inp))
                    .collect();
                // LOOPOPT20-2: a masked load's copy names the entry guard that
                // proves it as its guard token (`ir::guard_token_slot`, the
                // fifth input), which orders it after that guard in the
                // pre-header block and is what `MaskGuardProofs` reads for a
                // copy outside the loop. Its control is `entry_pred` (`subst`).
                if let Some(&g) = cloned_masked.get(&d) {
                    new_inputs.push(g);
                }
                // A pure-body unroll's copies carry no pc (nio20 wave 5, L3):
                // no snapshot describes them and none may be anchored at them.
                let clone = graph.add(op, ty, new_inputs, if strip_copy_pcs { None } else { pc });
                // See the partial unroller: a copy of a volatile access stays
                // volatile (round 9 wave 6).
                if volatile {
                    graph.set_volatile_access(clone);
                }
                subst.insert(d, clone);
                iter_clones.push(clone);
            }
            // Per-copy deopt metadata, while `subst` still describes exactly
            // this iteration. See `install_copy_frames` for why the order
            // matters.
            if let Some(plan) = per_copy.as_ref() {
                install_copy_frames(graph, plan, &subst, &iter_clones, t == 0);
            }
            // Advance carried phis to their next-iteration values.
            let mut next: FxHashMap<NodeId, NodeId> = FxHashMap::default();
            for &p in &carried {
                if p == info.iv_phi {
                    let v = info
                        .iv_init
                        .wrapping_add((t + 1).wrapping_mul(info.iv_stride));
                    // The IV φ's own type, not a hard-coded `Int`: a `long`
                    // induction variable unrolled to an `Int`-typed constant
                    // would be lowered and oop-mapped at the wrong width.
                    // `analyze_counted_loop` already refused any run whose
                    // steps leave that type's range.
                    let iv_ty = graph.nodes[p as usize].ty;
                    let c = graph.add(Op::Const(v), iv_ty, vec![], None);
                    next.insert(p, c);
                } else {
                    let back_val = graph.nodes[p as usize].inputs[back_slot];
                    next.insert(p, subst.get(&back_val).copied().unwrap_or(back_val));
                }
            }
            cur = next;
        }

        // The flat frames a pure-body unroll strands (their slots still name
        // the originals here), for the lowering's net. See
        // `UNROLL_STRANDED_BCIS`.
        // JIT r20 wave 19 (LOOPOPT20-6, `CRATONVM_JIT_IR_UNROLL_NAMED_FRAME_PCS`):
        // when the escape held only by the refined rule, also every flat frame
        // inside the body's pc range that names a CARRIED phi (rule (3) of
        // `pure_body_frames_unconsultable`, whose reaches that rule no longer
        // counts for a named-frame, pure or direct-read node).
        let carried_range: Option<(usize, usize)> = if strip_copy_pcs && frames_refined {
            let pcs = to_clone
                .iter()
                .chain(carried.iter())
                .chain(dropped.iter())
                .copied()
                .chain([region, info.if_node, back_ctrl, info.exit_ctrl])
                .filter_map(|k| graph.nodes.get(k as usize).and_then(|n| n.bytecode_pc));
            pcs.fold(None, |acc: Option<(usize, usize)>, pc| match acc {
                Some((lo, hi)) => Some((lo.min(pc), hi.max(pc))),
                None => Some((pc, pc)),
            })
        } else {
            None
        };
        let stranded_bcis: Vec<usize> = if strip_copy_pcs {
            graph
                .safepoints
                .iter()
                .filter(|sp| {
                    !sp.is_splice_state()
                        && sp
                            .locals
                            .iter()
                            .chain(sp.stack.iter())
                            .chain(sp.monitors.iter())
                            .any(|&s| {
                                s != NO_NODE
                                    && (to_clone.contains(&s)
                                        || dropped.contains(&s)
                                        || (carried_set.contains(&s)
                                            && carried_range.is_some_and(|(lo, hi)| {
                                                (lo..=hi).contains(&sp.bci)
                                            })))
                            })
                })
                .map(|sp| sp.bci)
                .collect()
        } else {
            Vec::new()
        };
        // Redirect post-loop uses of each carried phi to its final value, then
        // straight-line the control and retire the loop skeleton + carried phis.
        // (`cur` now holds the post-loop value of each phi; for trip == 0 these
        // are the entry values.)
        for &p in &carried {
            let fin = cur.get(&p).copied().unwrap_or(NO_NODE);
            if fin != NO_NODE && fin != p {
                graph.replace_all_uses(p, fin);
            }
        }
        let keep_exit_as_merge = collapse_would_invert_effects(graph, entry_pred, info.exit_ctrl);
        if keep_exit_as_merge {
            if let Some(exit_node) = graph.nodes.get_mut(info.exit_ctrl as usize) {
                exit_node.op = Op::Merge;
            }
            graph.set_inputs(info.exit_ctrl, vec![entry_pred]);
        } else {
            graph.replace_all_uses(info.exit_ctrl, entry_pred);
        }
        graph.kill(region);
        graph.kill(info.if_node);
        graph.kill(back_ctrl);
        if !keep_exit_as_merge {
            graph.kill(info.exit_ctrl);
        }
        for &p in &carried {
            graph.kill(p);
        }
        for &d in &to_clone {
            graph.kill(d);
        }
        // nio21 wave 6 (lane L5): the fenced loads, their fences and their
        // private index nodes (`fenced_load_drop_set`).
        for &d in &dropped {
            graph.kill(d);
        }
        if false {
            eprintln!(
                "[DBG_UNROLL] fully unrolled counted loop (region {region}, trip {}, {} cloned nodes/iter)",
                info.trip,
                to_clone.len()
            );
        }
        census[unroll_bucket::UNROLLED] += 1;
        // JIT round 17 wave 3 (lane irlower, IRFEED17-2): a loop RETIRED, which
        // the single-pass 4x unroll never does. Recorded apart from
        // `Transform::Unrolled` (noted by the caller for both kinds); evidence
        // unless `CRATONVM_JIT_IR_EVIDENCE_FULL_UNROLL=0`.
        crate::ir_evidence::note_full_unroll();
        if per_copy.is_some() {
            census[unroll_bucket::PER_COPY_FRAMES] += 1;
        }
        if uses_unreachable_frames {
            UNROLL_USED_UNREACHABLE_FRAMES.with(|c| c.set(true));
        }
        if strip_copy_pcs {
            if dbg {
                eprintln!(
                    "[DBG_UNROLL] region {region}: pure-body unroll, {} stranded frame bci(s)",
                    stranded_bcis.len()
                );
            }
            UNROLL_STRANDED_BCIS.with(|c| c.borrow_mut().flat.extend(stranded_bcis));
        }
        changed = true;
        users_stale = true;
    }
    publish_unroll_census(&census);
    changed
}

fn low_bit_mask_candidate(graph: &Graph, id: NodeId) -> Option<(NodeId, i64)> {
    let node = graph.node_opt(id)?;
    if node.op != Op::And
        || !matches!(node.ty, IrType::Int | IrType::Long)
        || node.inputs.len() != 2
    {
        return None;
    }
    let ty = node.ty;
    let is_valid_mask = |cid: NodeId| -> Option<i64> {
        let cn = graph.node_opt(cid)?;
        let Op::Const(m) = cn.op else {
            return None;
        };
        if cn.ty != ty || m <= 0 {
            return None;
        }
        if ty == IrType::Int && m > i64::from(i32::MAX) {
            return None;
        }
        let u = m as u64;
        u.checked_add(1)?.is_power_of_two().then_some(m)
    };
    let (a, b) = (node.inputs[0], node.inputs[1]);
    let (x, mask) = match (is_valid_mask(a), is_valid_mask(b)) {
        (_, Some(m)) => (a, m),
        (Some(m), None) => (b, m),
        _ => return None,
    };
    if x == id {
        return None;
    }
    let xn = graph.node_opt(x)?;
    if xn.ty != ty || xn.op == Op::Dead {
        return None;
    }
    Some((x, mask))
}

/// Fold `Op::And(x, Const(mask))` (and `Const(mask), x`) to `x` when `mask`
/// is a positive contiguous low-bit mask `(1 << k) - 1` and `GraphRanges`
/// proves `0 <= lo(x)` and `hi(x) <= mask`.
pub(in crate::ir_optimize) fn fold_range_redundant_masks(graph: &mut Graph) -> bool {
    let has_candidate =
        (0..graph.nodes.len() as NodeId).any(|id| low_bit_mask_candidate(graph, id).is_some());
    if !has_candidate {
        return false;
    }
    let ranges = crate::range_analysis::analyze_graph(graph);
    if !ranges.converged() {
        return false;
    }
    let mut folded: Vec<(NodeId, NodeId)> = Vec::new();
    for id in 0..graph.nodes.len() as NodeId {
        let Some((x, mask)) = low_bit_mask_candidate(graph, id) else {
            continue;
        };
        let rx = ranges.of(x);
        if rx.is_empty() {
            continue;
        }
        let (Some(lo), Some(hi)) = (rx.lo(), rx.hi()) else {
            continue;
        };
        if lo >= 0 && hi <= mask {
            folded.push((id, x));
        }
    }
    if folded.is_empty() {
        return false;
    }
    let mut map: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    for &(id, x) in &folded {
        map.insert(id, x);
    }
    for (id, x) in folded {
        let mut target = x;
        let mut hops = 0usize;
        while let Some(&next) = map.get(&target) {
            if next == target || hops >= map.len() {
                break;
            }
            target = next;
            hops += 1;
        }
        if target != id && graph.node_opt(target).is_some_and(|n| n.op != Op::Dead) {
            graph.replace_all_uses(id, target);
            graph.kill(id);
        }
    }
    true
}
