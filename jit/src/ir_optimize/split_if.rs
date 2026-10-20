//! JIT round 20 wave 18 (lane xSplitIf, LOOPOPT20-1): split-if for a branch
//! on a φ of constants (`CRATONVM_JIT_IR_SPLIT_IF_CONST_PHI`, default ON,
//! `=0` is the kill switch).
//!
//! # Why
//!
//! `boolean hasRemaining() { return position < limit; }` is
//! `if_icmpge L; iconst_1; goto E; L: iconst_0; E: ireturn`. Spliced into
//! `while (buf.hasRemaining())` (or `while (it.hasNext())`), the caller's
//! `ifeq` branches on the join `E`'s value φ `phi(E; 1, 0)`. Both arms of the
//! callee's own test then reach a back edge through `E`, so the loop body has
//! no `If` edge stating `position < limit`: `mask_predicate::header_stay_edge`
//! answers `None` (the mask and monotone predicates refuse), `ir_check_elim`'s
//! edge facts and the range lattice miss the bound, and the unroller's
//! `region -> if -> {back, exit}` shape fails
//! (`docs/internal/fixed-bugs/jr20-spliced-boolean-loop-test-keeps-a-phi-join-in-the-body-CLOSED-20261012.md`).
//! HotSpot's split-if routes each predecessor of `E` straight to the successor
//! its constant selects.
//!
//! # What
//!
//! For an `If` at merge `M` whose condition is `P` or `Cmp(cc, P, k)` /
//! `Cmp(cc, k, P)` (`k` an `int` constant), `P` an `int` φ at `M` whose every
//! value is an `int` constant: each predecessor `c_j` of `M` is routed to the
//! projection `P = k_j` selects -- the projection's users are rewired to
//! `c_j` (or to a new, pc-less `Merge` of the predecessors that select it) --
//! and `M`, `P`, the compare, the `If` and both projections are killed.
//!
//! # When it refuses (the graph is left as it was)
//!
//! * the compare has any user but the `If`, `P` any user but the condition,
//!   or `M` any user but `P` and the `If` (another φ -- a memory φ included --
//!   or a node anchored at the join would need a copy per side);
//! * a predecessor of `M` has a user other than `M` (an arm that computes or
//!   anchors something; also keeps `ir_schedule`'s in-block id order out of
//!   the question), or appears twice;
//! * one side would receive no predecessor (the branch is decided; that is
//!   [`super::prune_constant_ifs`]'s job once the cleanup folds the φ);
//! * `M` is the target of a retreating control edge (a loop header, reducible
//!   or not), or the graph has no usable `Start` to tell;
//! * a safepoint snapshot naming `P` or the compare can still be consulted
//!   ([`super::pure_body_frames_unconsultable`]: a surviving node carries its
//!   bci, directly or through the splice resume map, claims it by
//!   `frame_snapshot`, or it is a post-call snapshot). The caller's flat
//!   snapshot at the `ifeq` bci names `P` on its stack, and so does the chain
//!   snapshot at the callee's `ireturn`; once the compare, the `If` and its
//!   projections (at the `ifeq` bci) and `M` and `P` (at the join's bci) are
//!   gone, nothing reaches them.
//!
//! # Why it is sound
//!
//! Control that reaches `M` through `c_j` reaches it with `P = k_j`, so the
//! `If` takes the projection `k_j` selects on every such execution: the
//! rewire only removes the join and the test, and every node after the
//! projection sees the same values (nothing at `M` but `P`, and `P` is read
//! by the condition alone) and the same memory token (no memory φ at `M`).
//! The loop structure is unchanged: `M` is no header, every header keeps its
//! inputs (a projection that was a back edge or a loop entry is replaced by
//! one control node, a predecessor or the new `Merge`), so each loop keeps one
//! header and its back edges. The frames that named `P` are unconsultable (the
//! check above); their slots are set to `NO_NODE` here, and the flat ones'
//! bcis join `UNROLL_STRANDED_BCIS`, the net `ir_lower` checks against the
//! bcis its emission transfers at (a disagreement refuses the compile). The
//! new `Merge` carries no bytecode pc, so it never sets the lowerer's carried
//! `cur_bci` to the killed join's bci.

use super::{
    build_users, pure_body_frames_unconsultable, retreating_edge_targets, FxHashSet,
    UNROLL_STRANDED_BCIS,
};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_SPLIT_IF_CONST_PHI` (JIT round 20 wave 18, lane xSplitIf,
/// LOOPOPT20-1) -- **default ON**, `=0` is the kill switch. Read once per run
/// of the pass.
pub(super) fn split_if_const_phi_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SPLIT_IF_CONST_PHI")
}

/// What one run of [`split_const_phi_ifs`] did, for the census line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SplitIfCensus {
    /// `If`s whose condition reads a constant φ at their own merge.
    pub candidates: usize,
    /// Split.
    pub split: usize,
    /// Refused because a frame naming the φ or the compare stays reachable.
    pub refused_frames: usize,
}

/// The rewrite of one `If`, decided before any mutation.
struct SplitIfPlan {
    merge: NodeId,
    phi: NodeId,
    cmp: Option<NodeId>,
    iff: NodeId,
    /// `Proj(0)` (true) and `Proj(1)` (false).
    projs: [NodeId; 2],
    /// The predecessors of `merge` routed to `projs[0]` / `projs[1]`, in
    /// their order at `merge`.
    sides: [Vec<NodeId>; 2],
    /// The flat snapshot bcis left naming a killed value (`UNROLL_STRANDED_BCIS`).
    stranded: Vec<usize>,
}

/// Why [`plan_split`] declined.
enum Refusal {
    Shape,
    Frames,
}

fn cmp_holds(cc: CmpOp, a: i32, b: i32) -> bool {
    match cc {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

fn int_const(graph: &Graph, id: NodeId) -> Option<i64> {
    let n = graph.node_opt(id)?;
    match n.op {
        Op::Const(v) if n.ty == IrType::Int => Some(v),
        _ => None,
    }
}

/// Which projection a φ value selects: the `If` on the φ itself, or on a
/// compare of it with the `int` constant `k`.
#[derive(Clone, Copy)]
enum Decider {
    /// `If(P)`: the low 32 bits decide, as in `ir_lower`.
    Truthy,
    /// `If(Cmp(cc, P, k))` (`phi_left`) or `If(Cmp(cc, k, P))`.
    Cmp { cc: CmpOp, k: i32, phi_left: bool },
}

impl Decider {
    /// The projection index (`0` true, `1` false) the φ value `v` selects.
    fn side(self, v: i64) -> usize {
        let truth = match self {
            Decider::Truthy => (v as i32) != 0,
            Decider::Cmp {
                cc,
                k,
                phi_left: true,
            } => cmp_holds(cc, v as i32, k),
            Decider::Cmp {
                cc,
                k,
                phi_left: false,
            } => cmp_holds(cc, k, v as i32),
        };
        if truth {
            0
        } else {
            1
        }
    }
}

/// The condition shape of `If` `iff`: `(merge, phi, cmp, decider)`. `None`
/// when the condition is not an `int` φ at the `If`'s own control (a
/// `Merge`), or a compare of one with an `int` constant.
fn condition_shape(
    graph: &Graph,
    iff: NodeId,
) -> Option<(NodeId, NodeId, Option<NodeId>, Decider)> {
    let n = graph.node_opt(iff)?;
    if n.op != Op::If || n.inputs.len() != 2 {
        return None;
    }
    let merge = n.input_opt(0)?;
    let cond = n.input_opt(1)?;
    if graph.node_opt(merge)?.op != Op::Merge {
        return None;
    }
    let is_phi_here = |id: NodeId| {
        graph.node_opt(id).is_some_and(|p| {
            p.op == Op::Phi && p.ty == IrType::Int && p.input_opt(0) == Some(merge)
        })
    };
    if is_phi_here(cond) {
        return Some((merge, cond, None, Decider::Truthy));
    }
    let cn = graph.node_opt(cond)?;
    let Op::Cmp(cc) = cn.op else {
        return None;
    };
    if cn.ty != IrType::Int || cn.inputs.len() != 2 {
        return None;
    }
    let (a, b) = (cn.input_opt(0)?, cn.input_opt(1)?);
    if is_phi_here(a) {
        let k = int_const(graph, b)? as i32;
        Some((
            merge,
            a,
            Some(cond),
            Decider::Cmp {
                cc,
                k,
                phi_left: true,
            },
        ))
    } else if is_phi_here(b) {
        let k = int_const(graph, a)? as i32;
        Some((
            merge,
            b,
            Some(cond),
            Decider::Cmp {
                cc,
                k,
                phi_left: false,
            },
        ))
    } else {
        None
    }
}

/// The live users of `id`, each as often as it names `id`.
fn live_users(graph: &Graph, users: &[Vec<NodeId>], id: NodeId) -> Vec<NodeId> {
    users
        .get(id as usize)
        .map(|u| {
            u.iter()
                .copied()
                .filter(|&x| graph.node_opt(x).is_some_and(|n| n.op != Op::Dead))
                .collect()
        })
        .unwrap_or_default()
}

/// Decide the split of `iff` (see the module notes), or refuse.
fn plan_split(
    graph: &Graph,
    users: &[Vec<NodeId>],
    retreating: &FxHashSet<NodeId>,
    iff: NodeId,
) -> Result<SplitIfPlan, Refusal> {
    let (merge, phi, cmp, decider) = condition_shape(graph, iff).ok_or(Refusal::Shape)?;
    let mn = graph.node_opt(merge).ok_or(Refusal::Shape)?;
    let preds: Vec<NodeId> = mn.inputs.iter().copied().collect();
    if preds.len() < 2 || preds.iter().any(|&c| c == NO_NODE) {
        return Err(Refusal::Shape);
    }
    let pn = graph.node_opt(phi).ok_or(Refusal::Shape)?;
    if pn.inputs.len() != preds.len() + 1 {
        return Err(Refusal::Shape);
    }
    let mut values = Vec::with_capacity(preds.len());
    for v in pn.phi_value_inputs() {
        values.push(int_const(graph, v.ok_or(Refusal::Shape)?).ok_or(Refusal::Shape)?);
    }
    if retreating.contains(&merge) {
        return Err(Refusal::Shape);
    }

    // The compare feeds the `If` alone; the φ feeds the condition alone.
    let cond = cmp.unwrap_or(phi);
    if let Some(c) = cmp {
        if live_users(graph, users, c) != [iff] {
            return Err(Refusal::Shape);
        }
    }
    if cmp.is_some() && live_users(graph, users, phi) != [cond] {
        return Err(Refusal::Shape);
    }
    // (A φ used directly as the condition: the `If` names it once.)
    if cmp.is_none() && live_users(graph, users, phi) != [iff] {
        return Err(Refusal::Shape);
    }
    // Nothing at the join but the φ and the `If`.
    let mut mu = live_users(graph, users, merge);
    mu.sort_unstable();
    let mut want = vec![phi, iff];
    want.sort_unstable();
    if mu != want {
        return Err(Refusal::Shape);
    }
    // Exactly the two control projections.
    let mut projs = [NO_NODE; 2];
    let iu = live_users(graph, users, iff);
    if iu.len() != 2 {
        return Err(Refusal::Shape);
    }
    for u in iu {
        let un = graph.node_opt(u).ok_or(Refusal::Shape)?;
        match un.op {
            Op::Proj(w) if un.ty == IrType::Control && w <= 1 && projs[w as usize] == NO_NODE => {
                projs[w as usize] = u;
            }
            _ => return Err(Refusal::Shape),
        }
    }
    if projs.contains(&NO_NODE) {
        return Err(Refusal::Shape);
    }
    // Empty arms: each predecessor is a control node naming the join once and
    // used by nothing else.
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut sides: [Vec<NodeId>; 2] = [Vec::new(), Vec::new()];
    for (&c, &v) in preds.iter().zip(values.iter()) {
        if !seen.insert(c) || c == iff || projs.contains(&c) {
            return Err(Refusal::Shape);
        }
        let cn = graph.node_opt(c).ok_or(Refusal::Shape)?;
        if cn.ty != IrType::Control || !matches!(cn.op, Op::Proj(_) | Op::Merge | Op::Region) {
            return Err(Refusal::Shape);
        }
        if live_users(graph, users, c) != [merge] {
            return Err(Refusal::Shape);
        }
        sides[decider.side(v)].push(c);
    }
    if sides.iter().any(Vec::is_empty) {
        return Err(Refusal::Shape);
    }

    // Frames: nothing may still consult a snapshot naming a killed value.
    let mut killed: FxHashSet<NodeId> = [merge, phi, iff, projs[0], projs[1]].into_iter().collect();
    let mut named: FxHashSet<NodeId> = FxHashSet::default();
    named.insert(phi);
    if let Some(c) = cmp {
        killed.insert(c);
        named.insert(c);
    }
    let none: FxHashSet<NodeId> = FxHashSet::default();
    if !pure_body_frames_unconsultable(graph, &killed, &named, &none) {
        return Err(Refusal::Frames);
    }
    let stranded: Vec<usize> = graph
        .safepoints
        .iter()
        .filter(|sp| {
            !sp.is_splice_state()
                && sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                    .any(|&s| s != NO_NODE && named.contains(&s))
        })
        .map(|sp| sp.bci)
        .collect();
    Ok(SplitIfPlan {
        merge,
        phi,
        cmp,
        iff,
        projs,
        sides,
        stranded,
    })
}

/// Apply a [`plan_split`] answer. Every check was made there.
fn apply_split(graph: &mut Graph, plan: SplitIfPlan) {
    for (proj, preds) in plan.projs.iter().zip(plan.sides.iter()) {
        let target = match preds.as_slice() {
            [only] => *only,
            _ => graph.add(Op::Merge, IrType::Control, preds.clone(), None),
        };
        graph.replace_all_uses(*proj, target);
    }
    graph.kill(plan.projs[0]);
    graph.kill(plan.projs[1]);
    graph.kill(plan.iff);
    if let Some(c) = plan.cmp {
        graph.kill(c);
    }
    // The only names left are snapshot slots, none of them reachable.
    graph.replace_all_uses(plan.phi, NO_NODE);
    graph.kill(plan.phi);
    graph.kill(plan.merge);
    if !plan.stranded.is_empty() {
        UNROLL_STRANDED_BCIS.with(|c| c.borrow_mut().extend(plan.stranded));
    }
}

/// Split every `If` on a constant φ the module notes admit. See there.
pub(super) fn split_const_phi_ifs(graph: &mut Graph) -> SplitIfCensus {
    let mut census = SplitIfCensus::default();
    if !split_if_const_phi_enabled() {
        return census;
    }
    let candidates: Vec<NodeId> = {
        let g: &Graph = graph;
        (0..g.nodes.len() as NodeId)
            .filter(|&id| condition_shape(g, id).is_some())
            .collect()
    };
    census.candidates = candidates.len();
    if candidates.is_empty() {
        return census;
    }
    let mut state: Option<(Vec<Vec<NodeId>>, FxHashSet<NodeId>)> = None;
    for iff in candidates {
        if state.is_none() {
            let users = build_users(graph);
            let Some(retreating) = retreating_edge_targets(graph, &users) else {
                return census;
            };
            state = Some((users, retreating));
        }
        let Some((users, retreating)) = state.as_ref() else {
            return census;
        };
        match plan_split(graph, users, retreating, iff) {
            Ok(plan) => {
                apply_split(graph, plan);
                census.split += 1;
                state = None;
            }
            Err(Refusal::Frames) => census.refused_frames += 1,
            Err(Refusal::Shape) => {}
        }
    }
    census
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::SafepointSnapshot;

    const SWITCH: &str = "CRATONVM_JIT_IR_SPLIT_IF_CONST_PHI";

    /// The nodes of [`spliced_loop`] a test reads back.
    struct Loop {
        g: Graph,
        entry: NodeId,
        header: NodeId,
        inner_t: NodeId,
        inner_f: NodeId,
        join: NodeId,
        phi: NodeId,
        cmp: NodeId,
        outer: NodeId,
        exit_proj: NodeId,
        body_proj: NodeId,
        ret: NodeId,
        iv: NodeId,
    }

    /// `while (i < n) i++; return i;` with the test spliced from a boolean
    /// getter: `If(i < n)` at the header, both arms into the join `E` with
    /// `phi(E; 1, 0)`, then the caller's `ifeq`: `If(Cmp(Eq, phi, 0))` at pc
    /// 7, whose `Proj(0)` leaves the loop and whose `Proj(1)` is the back edge.
    /// A flat snapshot at pc 7 names the φ on its stack, as the builder's does.
    fn spliced_loop() -> Loop {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let header = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(2));
        let iv = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(2));
        let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, n], Some(40));
        let inner = g.add(Op::If, IrType::Control, vec![header, lt], Some(40));
        let inner_t = g.add(Op::Proj(0), IrType::Control, vec![inner], Some(40));
        let inner_f = g.add(Op::Proj(1), IrType::Control, vec![inner], Some(40));
        let join = g.add(Op::Merge, IrType::Control, vec![inner_t, inner_f], Some(46));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero], Some(46));
        let cmp = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![phi, zero], Some(7));
        let outer = g.add(Op::If, IrType::Control, vec![join, cmp], Some(7));
        let exit_proj = g.add(Op::Proj(0), IrType::Control, vec![outer], Some(7));
        let body_proj = g.add(Op::Proj(1), IrType::Control, vec![outer], Some(7));
        let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(10));
        assert!(g.set_input(header, 1, body_proj));
        assert!(g.set_input(iv, 2, next));
        let ret = g.add(Op::Return, IrType::Void, vec![exit_proj, iv], Some(20));
        g.exit = ret;
        g.push_safepoint(SafepointSnapshot::new(2, vec![n, iv], vec![], vec![]));
        g.push_safepoint(SafepointSnapshot::new(7, vec![n, iv], vec![phi], vec![]));
        Loop {
            g,
            entry,
            header,
            inner_t,
            inner_f,
            join,
            phi,
            cmp,
            outer,
            exit_proj,
            body_proj,
            ret,
            iv,
        }
    }

    #[test]
    fn a_spliced_boolean_loop_test_routes_each_arm_to_its_successor() {
        let _ = super::super::take_unroll_stranded_bcis();
        let mut l = spliced_loop();
        let census = split_const_phi_ifs(&mut l.g);
        assert_eq!(census.split, 1, "{census:?}");
        for dead in [l.join, l.phi, l.cmp, l.outer, l.exit_proj, l.body_proj] {
            assert_eq!(l.g.nodes[dead as usize].op, Op::Dead, "n{dead}");
        }
        // `i < n` true (φ = 1, `ifeq` not taken) is the back edge now; false
        // (φ = 0) leaves the loop.
        assert_eq!(
            l.g.nodes[l.header as usize].inputs.as_slice(),
            &[l.entry, l.inner_t][..]
        );
        assert_eq!(
            l.g.nodes[l.ret as usize].inputs.as_slice(),
            &[l.inner_f, l.iv][..]
        );
        // One header, one entry, one back edge: the loop's canonical form.
        assert_eq!(
            super::super::loop_headers(&l.g),
            vec![(l.header, l.entry, vec![l.inner_t])]
        );
        // The `ifeq` frame no longer names the φ, and its bci is on the net.
        assert_eq!(l.g.safepoints[1].stack, vec![NO_NODE]);
        assert_eq!(super::super::take_unroll_stranded_bcis(), vec![7]);
    }

    /// A surviving node at the `ifeq` bci could transfer into the frame that
    /// names the φ: refused, and the graph is unchanged.
    #[test]
    fn a_reachable_frame_naming_the_phi_refuses() {
        let mut l = spliced_loop();
        let _keep = l.g.add(
            Op::Guard { bci: 7 },
            IrType::Void,
            vec![l.header, l.iv],
            Some(7),
        );
        let census = split_const_phi_ifs(&mut l.g);
        assert_eq!((census.split, census.refused_frames), (0, 1));
        assert_eq!(l.g.nodes[l.outer as usize].op, Op::If);
        assert_eq!(l.g.safepoints[1].stack, vec![l.phi]);
    }

    /// A second φ at the join (here the memory token the arms disagree on)
    /// would need a copy per side: refused.
    #[test]
    fn another_phi_at_the_join_refuses() {
        let mut l = spliced_loop();
        let m0 = l.g.add(Op::Proj(1), IrType::Memory, vec![l.g.entry], None);
        let m1 = l.g.add(Op::Proj(1), IrType::Memory, vec![l.g.entry], None);
        let _mphi =
            l.g.add(Op::Phi, IrType::Memory, vec![l.join, m0, m1], Some(46));
        assert_eq!(split_const_phi_ifs(&mut l.g).split, 0);
        assert_eq!(l.g.nodes[l.outer as usize].op, Op::If);
    }

    /// An arm that anchors a node (a load in the `true` arm) is not empty:
    /// refused.
    #[test]
    fn a_non_empty_arm_refuses() {
        let mut l = spliced_loop();
        let n = l.g.nodes[l.ret as usize].inputs[1];
        let _guard = l.g.add(
            Op::Guard { bci: 44 },
            IrType::Void,
            vec![l.inner_t, n],
            Some(44),
        );
        assert_eq!(split_const_phi_ifs(&mut l.g).split, 0);
        assert_eq!(l.g.nodes[l.join as usize].op, Op::Merge);
    }

    /// A φ value that is not a constant is no candidate.
    #[test]
    fn a_non_constant_phi_value_refuses() {
        let mut l = spliced_loop();
        assert!(l.g.set_input(l.phi, 2, l.iv));
        assert_eq!(split_const_phi_ifs(&mut l.g).split, 0);
        assert_eq!(l.g.nodes[l.outer as usize].op, Op::If);
    }

    /// The φ is also read elsewhere (stored to a local the loop carries):
    /// refused.
    #[test]
    fn a_phi_with_another_user_refuses() {
        let mut l = spliced_loop();
        let _other = l.g.add(Op::Add, IrType::Int, vec![l.phi, l.iv], Some(12));
        assert_eq!(split_const_phi_ifs(&mut l.g).split, 0);
    }

    /// A join that is itself a loop header (an edge retreats to it) is
    /// refused: splitting it would move a back edge off its header.
    #[test]
    fn a_loop_header_join_refuses() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let join = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(3));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero], Some(3));
        let iff = g.add(Op::If, IrType::Control, vec![join, phi], Some(4));
        let out = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(4));
        let back = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(4));
        assert!(g.set_input(join, 1, back));
        g.exit = g.add(Op::Return, IrType::Void, vec![out], Some(9));
        assert_eq!(split_const_phi_ifs(&mut g).split, 0);
        assert_eq!(g.nodes[iff as usize].op, Op::If);
    }

    /// `return a && b ? ...`-style joins: three edges, two of which select
    /// the false side, get a new pc-less `Merge`; the φ is the condition
    /// itself (no compare).
    #[test]
    fn several_edges_on_one_side_share_a_new_merge() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let if_a = g.add(Op::If, IrType::Control, vec![ctrl, a], Some(1));
        let a_t = g.add(Op::Proj(0), IrType::Control, vec![if_a], Some(1));
        let a_f = g.add(Op::Proj(1), IrType::Control, vec![if_a], Some(1));
        let if_b = g.add(Op::If, IrType::Control, vec![a_t, b], Some(4));
        let b_t = g.add(Op::Proj(0), IrType::Control, vec![if_b], Some(4));
        let b_f = g.add(Op::Proj(1), IrType::Control, vec![if_b], Some(4));
        let join = g.add(Op::Merge, IrType::Control, vec![b_t, a_f, b_f], Some(12));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero, zero], Some(12));
        let iff = g.add(Op::If, IrType::Control, vec![join, phi], Some(13));
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(13));
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(13));
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let r_t = g.add(Op::Return, IrType::Void, vec![t, seven], Some(16));
        let r_f = g.add(Op::Return, IrType::Void, vec![f, zero], Some(20));
        g.exit = r_f;
        let len_before = g.nodes.len();
        assert_eq!(split_const_phi_ifs(&mut g).split, 1);
        assert_eq!(g.nodes[r_t as usize].inputs.as_slice(), &[b_t, seven][..]);
        let m = g.nodes[r_f as usize].inputs[0];
        assert!(m as usize >= len_before, "a new merge");
        assert_eq!(g.nodes[m as usize].op, Op::Merge);
        assert_eq!(g.nodes[m as usize].inputs.as_slice(), &[a_f, b_f][..]);
        assert_eq!(g.nodes[m as usize].bytecode_pc, None);
        for dead in [join, phi, iff, t, f] {
            assert_eq!(g.nodes[dead as usize].op, Op::Dead, "n{dead}");
        }
    }

    /// One side would receive no edge (`phi(1, 2)` under `ifeq`): left to
    /// the constant-`If` pruning.
    #[test]
    fn a_decided_branch_is_left_alone() {
        let mut l = spliced_loop();
        let two = l.g.add(Op::Const(2), IrType::Int, vec![], None);
        assert!(l.g.set_input(l.phi, 2, two));
        assert_eq!(split_const_phi_ifs(&mut l.g).split, 0);
        assert_eq!(l.g.nodes[l.outer as usize].op, Op::If);
    }

    #[test]
    fn the_kill_switch_turns_the_pass_off() {
        let off = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("0"))], || {
            let mut l = spliced_loop();
            let c = split_const_phi_ifs(&mut l.g);
            (c.split, l.g.nodes[l.outer as usize].op.clone())
        });
        assert_eq!(off, (0, Op::If));
    }
}
