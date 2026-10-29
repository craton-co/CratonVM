//! JIT round 25 wave 2 (lane branch-fold): branches and booleans the IR tier
//! kept although the graph already decides them. Four folds, each behind its
//! own opt-in switch, run by `ir_optimize::run_bool_branch_folds` after the
//! split-if and again after the late constant-`If` prune
//! (`docs/feature-designs/jit-ir-optimizations.md`).
//!
//! # The folds
//!
//! * **Boolean φ to its branch's condition** (`CRATONVM_JIT_IR_BOOL_PHI_FOLD`).
//!   `phi(M; 1, 0, ..)` whose every edge with a `1` is dominated by one
//!   projection of an `If(c)` and every edge with a `0` by the other is `c != 0`
//!   (or `c == 0`), wherever the φ is defined. That is javac's `iconst_1 /
//!   iconst_0` join of `return a < b` (a spliced boolean getter) or `boolean
//!   f = x != null`. The φ is replaced by `c` itself (`c` a compare, already
//!   `0`/`1`), by the negated compare, or by `Cmp(Ne|Eq, c, 0)`. The edge copies
//!   of the two constants go, and a test of the φ becomes a test of `c`.
//! * **Boolean compares** (same switch). `Cmp(cc, B, k)` with `B` known to be
//!   `0` or `1` (a compare, a `0`/`1` constant, a φ of those) and `k` an `int`
//!   constant is `B`, `!B` (a compare negated in place, never a materialised
//!   boolean compared again) or a constant -- the `ifeq` the builder puts on
//!   every boolean (`If(Cmp(Eq, B, 0))`), which lowered as `setcc; movzx;
//!   store; reload; test; jcc`.
//! * **Empty diamonds** (`CRATONVM_JIT_IR_EMPTY_DIAMOND_FOLD`). An `If` both of
//!   whose projections reach the same `Merge` through nothing but single-input
//!   merges, with every φ there equal on the two edges, decides nothing: the
//!   join takes the `If`'s own control and the `If` goes. The first fold leaves
//!   exactly this behind (its φ was the arms' only content).
//! * **Branch-implied `If`s** (`CRATONVM_JIT_IR_BRANCH_IMPLIED_IFS`). The
//!   `If` twin of `decide_branch_implied_guards`: an `If` on the same
//!   condition node as an `If` whose edge reaches it through a single-
//!   predecessor control chain, or on a compare of the same two operands that
//!   edge's compare implies or refutes (`a < b` implies `a <= b` and refutes
//!   `a > b`), gets a constant condition; `prune_constant_ifs` then cuts the
//!   dead arm.
//!
//! # Soundness
//!
//! * The φ fold changes no value: on every edge into `M` that a projection of
//!   `If(c)` dominates, `c` had the value that projection tests, and `c` is an
//!   SSA value (it does not change between the `If` and `M`). The replacement's
//!   operands are `c`'s or `c` itself, which dominate the `If` and so every use
//!   of the φ. Frame states naming the φ follow it (`replace_all_uses`
//!   rewrites snapshot slots) and name a node with the same value. No control
//!   changes. Refused for a graph with a jump-entered arm the lowering enters
//!   past its `If` (`CatchEntryFlag`, `CallThrew`): a value live into such an
//!   arm is held to rules this pass does not check. An opaque OSR arm is safe
//!   anyway (the lowering admits an arm only when its dataflow live-in set is
//!   constants, `Lowerer::emit_osr_entry_stubs`), but a replacement computed
//!   above the arm and read below it would make the lowering WITHHOLD that
//!   entry; so a φ whose deciding `If` dominates an opaque OSR arm from which
//!   the join is reachable is left alone (review R1): the fold never costs an
//!   OSR entry.
//! * The compare fold is a truth table over the two values `B` can take.
//!   Negating `Cmp(cc, x, y)` is exact for the integer / reference compare
//!   (`CmpOp::negate`); refused for a compare over `lcmp` / `fcmp` (exact, but
//!   it would cost the lowering their fused forms).
//! * An empty diamond's `If` has no effect but the choice of edge, and every
//!   node after the join sees the same values on both edges (the φs agree), so
//!   the choice is unobservable. Nothing is anchored on either arm. Its
//!   condition is a pure value (no `If` raises); a frame naming it keeps it
//!   alive through DCE. Not on a jump-entered `If` (`OsrEntryFlag` too: the
//!   OSR stub looks its arm up by that `If`).
//! * An implied `If` is decided only when every path to it took the deciding
//!   edge (a single-predecessor chain, never across a join or a back edge), and
//!   the facts compare the same SSA nodes. The walk stops at a jump-entered
//!   `If`, whose arm is entered past it. The removed arm's frame states are
//!   unreachable once the arm is gone (`prune_constant_ifs`' own rules).

use super::{
    build_users, const_node_by_scan, control_not_dominated_by, forward_control_closure,
    retreating_edge_targets, FxHashMap, FxHashSet,
};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_BOOL_PHI_FOLD` (JIT round 25 wave 2; default ON since round 26, `=0` kills): the boolean
/// φ and boolean compare folds. Read once per round.
pub(super) fn bool_phi_fold_enabled() -> bool {
    // R26-FLIP (default ON, `=0` kills).
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BOOL_PHI_FOLD")
}

/// `CRATONVM_JIT_IR_EMPTY_DIAMOND_FOLD` (JIT round 25 wave 2; default ON since round 26, `=0` kills): the
/// empty-diamond collapse.
pub(super) fn empty_diamond_fold_enabled() -> bool {
    // R26-FLIP (default ON, `=0` kills).
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_EMPTY_DIAMOND_FOLD")
}

/// `CRATONVM_JIT_IR_BRANCH_IMPLIED_IFS` (JIT round 25 wave 2; default ON since round 26, `=0` kills): `If`s
/// a dominating branch edge decides.
pub(super) fn branch_implied_ifs_enabled() -> bool {
    // R26-FLIP (default ON, `=0` kills).
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BRANCH_IMPLIED_IFS")
}

/// Is any of this module's folds on?
pub(super) fn any_enabled() -> bool {
    bool_phi_fold_enabled() || empty_diamond_fold_enabled() || branch_implied_ifs_enabled()
}

/// What the folds did to one body, for the `CRATONVM_DBG_JITC` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BoolBranchCensus {
    /// Boolean φs replaced by their branch's condition.
    pub(super) phis: usize,
    /// Compares of a boolean against a constant simplified.
    pub(super) compares: usize,
    /// Empty diamonds collapsed (their `If` removed).
    pub(super) diamonds: usize,
    /// `If`s given a constant condition by a dominating edge.
    pub(super) branches: usize,
    /// Dead arms `prune_constant_ifs` cut after a round (counted by the caller).
    pub(super) arms: usize,
    /// `And(B, 1)` of a known boolean `B` replaced by `B`.
    pub(super) masks: usize,
}

impl BoolBranchCensus {
    /// Rewrites made by the folds themselves (not the prune that follows).
    pub(super) fn rewrites(&self) -> usize {
        self.phis + self.compares + self.diamonds + self.branches + self.masks
    }

    pub(super) fn add(&mut self, other: &BoolBranchCensus) {
        self.phis += other.phis;
        self.compares += other.compares;
        self.diamonds += other.diamonds;
        self.branches += other.branches;
        self.arms += other.arms;
        self.masks += other.masks;
    }
}

/// One round of every enabled fold. The caller prunes and cleans up.
pub(super) fn fold_round(graph: &mut Graph) -> BoolBranchCensus {
    let mut census = BoolBranchCensus::default();
    if bool_phi_fold_enabled() {
        census.masks = fold_boolean_masks(graph);
        census.phis = fold_boolean_phis(graph);
        census.compares = simplify_boolean_compares(graph);
    }
    if empty_diamond_fold_enabled() {
        census.diamonds = collapse_empty_diamonds(graph);
    }
    if branch_implied_ifs_enabled() {
        census.branches = decide_branch_implied_ifs(graph);
    }
    census
}

/// How deep [`known_boolean`] follows φs.
const BOOL_DEPTH: u32 = 4;
/// `If`s on the upward walk from a join that are asked to decide its φ.
const IF_CANDIDATES: usize = 6;
/// Control nodes the upward walks visit.
const WALK_BUDGET: usize = 256;
/// Dominance walks (each one pass over the graph's control) per round.
const DOMINANCE_WALKS: usize = 64;

/// A condition whose `If`'s true edge is entered by a JUMP past the `If`
/// (the OSR stub, a compiled handler's probe, a call-site exception pad):
/// the graph's paths through it are not the only ones that reach its arm.
fn is_jump_entered_flag(op: &Op) -> bool {
    matches!(
        op,
        Op::OsrEntryFlag { .. } | Op::CatchEntryFlag { .. } | Op::CallThrew { .. }
    )
}

/// Does the graph hold an arm entered past its `If` from inside the frame (a
/// compiled handler or a call-site exception edge)? See the module notes.
fn graph_has_jump_entered_arms(graph: &Graph) -> bool {
    graph
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::CatchEntryFlag { .. } | Op::CallThrew { .. }))
}

fn int_const_of(graph: &Graph, id: NodeId) -> Option<i64> {
    let n = graph.node_opt(id)?;
    match n.op {
        Op::Const(v) if n.ty == IrType::Int => Some(v),
        _ => None,
    }
}

/// An `int` constant node of value `v`: the lowest-id existing one, or new.
fn int_const_node(graph: &mut Graph, v: i64) -> NodeId {
    match const_node_by_scan(graph, v, IrType::Int) {
        Some(id) => id,
        None => graph.add(Op::Const(v), IrType::Int, vec![], None),
    }
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

/// The orderings (`<` = 1, `==` = 2, `>` = 4) under which `cc` holds.
fn outcomes(cc: CmpOp) -> u8 {
    match cc {
        CmpOp::Lt => 1,
        CmpOp::Eq => 2,
        CmpOp::Gt => 4,
        CmpOp::Le => 3,
        CmpOp::Ge => 6,
        CmpOp::Ne => 5,
    }
}

/// Given `a held b`, does `a want b` hold (`Some(true)`), fail
/// (`Some(false)`), or is it open (`None`)?
fn cmp_implies(held: CmpOp, want: CmpOp) -> Option<bool> {
    let (h, w) = (outcomes(held), outcomes(want));
    if h & !w == 0 {
        Some(true)
    } else if h & w == 0 {
        Some(false)
    } else {
        None
    }
}

/// Is `x` an `int` that is always `0` or `1`: a compare, a `0`/`1` constant,
/// or a φ of those (up to `depth` φs deep)?
fn known_boolean(graph: &Graph, x: NodeId, depth: u32) -> bool {
    let Some(n) = graph.node_opt(x) else {
        return false;
    };
    if n.ty != IrType::Int {
        return false;
    }
    match n.op {
        Op::Cmp(_) => n.inputs.len() == 2,
        Op::Const(v) => v == 0 || v == 1,
        // `x & 1`: the `boolean` return narrowing (`ireturn` of a `Z` method).
        Op::And if n.inputs.len() == 2 => {
            n.inputs.iter().any(|&k| int_const_of(graph, k) == Some(1))
        }
        Op::Phi if depth > 0 => {
            let mut any = false;
            for v in n.phi_value_inputs() {
                let Some(v) = v else {
                    return false;
                };
                if v == x || graph.node_opt(v).is_some_and(|m| m.op == Op::Dead) {
                    continue;
                }
                if !known_boolean(graph, v, depth - 1) {
                    return false;
                }
                any = true;
            }
            any
        }
        _ => false,
    }
}

/// `Cmp(cc, a, b)` that may be negated in place: the integer / reference
/// compare, not one over an `lcmp` / `fcmp` three-way value.
fn negatable_cmp(graph: &Graph, c: NodeId) -> Option<(CmpOp, NodeId, NodeId)> {
    let n = graph.node_opt(c)?;
    let Op::Cmp(cc) = n.op else {
        return None;
    };
    if n.ty != IrType::Int || n.inputs.len() != 2 {
        return None;
    }
    let (a, b) = (n.input_opt(0)?, n.input_opt(1)?);
    for x in [a, b] {
        let xn = graph.node_opt(x)?;
        if matches!(xn.op, Op::FCmp { .. } | Op::LCmp)
            || !matches!(xn.ty, IrType::Int | IrType::Long | IrType::Ref)
        {
            return None;
        }
    }
    Some((cc, a, b))
}

/// `Proj(0)` and `Proj(1)` of `iff`, when they are its only live users.
fn if_projections(graph: &Graph, users: &[Vec<NodeId>], iff: NodeId) -> Option<[NodeId; 2]> {
    let mut projs = [NO_NODE; 2];
    for &u in users.get(iff as usize)? {
        let un = graph.node_opt(u)?;
        if un.op == Op::Dead {
            continue;
        }
        match un.op {
            Op::Proj(w) if un.ty == IrType::Control && w <= 1 && projs[w as usize] == NO_NODE => {
                projs[w as usize] = u;
            }
            _ => return None,
        }
    }
    if projs.contains(&NO_NODE) {
        None
    } else {
        Some(projs)
    }
}

// ── The boolean φ ─────────────────────────────────────────────────────

/// Replace every boolean φ a dominating `If` decides by that `If`'s
/// condition. Returns how many were replaced.
fn fold_boolean_phis(graph: &mut Graph) -> usize {
    let users = build_users(graph);
    let Some(retreating) = retreating_edge_targets(graph, &users) else {
        return 0;
    };
    let candidates: Vec<NodeId> = (0..graph.nodes.len() as NodeId)
        .filter(|&id| boolean_phi_shape(graph, &retreating, id))
        .collect();
    if candidates.is_empty() {
        return 0;
    }
    let dbg = crate::x64::dbg_jitc_enabled();
    if graph_has_jump_entered_arms(graph) {
        if dbg {
            eprintln!(
                "[cratonvm-jitc] ir-bool-phi: candidates={} refused_jump_entered_arms={}",
                candidates.len(),
                candidates.len()
            );
        }
        return 0;
    }
    // Control reachable from the entry (no node removed from the walk).
    let reach = control_not_dominated_by(graph, &users, NO_NODE);
    let arms = opaque_osr_arms(graph);
    let mut cache: FxHashMap<NodeId, FxHashSet<NodeId>> = FxHashMap::default();
    let mut forward: FxHashMap<NodeId, FxHashSet<NodeId>> = FxHashMap::default();
    let mut folded = 0usize;
    let (mut no_decider, mut osr_cross) = (0usize, 0usize);
    let total = candidates.len();
    for phi in candidates {
        // The fold only rewrites data edges, so the control-only `users`,
        // `reach` and dominance sets stay exact across the loop.
        let Some((cond, positive, projs)) = deciding_if(graph, &users, &reach, &mut cache, phi)
        else {
            no_decider += 1;
            continue;
        };
        let Some(join) = graph.node_opt(phi).and_then(|p| p.input_opt(0)) else {
            continue;
        };
        if crosses_opaque_osr_arm(graph, &users, &mut cache, &mut forward, &arms, projs, join) {
            osr_cross += 1;
            continue;
        }
        let pc = graph.node_opt(cond).and_then(|n| n.bytecode_pc);
        let replacement = boolean_of(graph, cond, positive, pc);
        if replacement == phi {
            continue;
        }
        graph.replace_all_uses(phi, replacement);
        graph.kill(phi);
        folded += 1;
    }
    if dbg {
        eprintln!(
            "[cratonvm-jitc] ir-bool-phi: candidates={total} folded={folded} no_decider={no_decider} osr_cross={osr_cross}"
        );
    }
    folded
}

/// An `int` φ at a non-header `Merge` whose values are `0`/`1` constants,
/// both present.
fn boolean_phi_shape(graph: &Graph, retreating: &FxHashSet<NodeId>, id: NodeId) -> bool {
    let Some(p) = graph.node_opt(id) else {
        return false;
    };
    if p.op != Op::Phi || p.ty != IrType::Int {
        return false;
    }
    let Some(m) = p.input_opt(0) else {
        return false;
    };
    let Some(mn) = graph.node_opt(m) else {
        return false;
    };
    if mn.op != Op::Merge
        || retreating.contains(&m)
        || mn.inputs.len() < 2
        || p.inputs.len() != mn.inputs.len() + 1
    {
        return false;
    }
    let mut seen = [false; 2];
    for v in p.phi_value_inputs() {
        match v.and_then(|v| int_const_of(graph, v)) {
            Some(0) => seen[0] = true,
            Some(1) => seen[1] = true,
            _ => return false,
        }
    }
    seen[0] && seen[1]
}

/// The `If`s met walking up from `from` along first control inputs, nearest
/// first. A dominator of `from` lies on every path, so on this one.
fn dominating_if_candidates(graph: &Graph, from: NodeId) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::new();
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut cur = from;
    for _ in 0..WALK_BUDGET {
        if !seen.insert(cur) {
            break;
        }
        let Some(n) = graph.node_opt(cur) else {
            break;
        };
        let next = match n.op {
            Op::Proj(_) if n.ty == IrType::Control => {
                let parent = n.input_opt(0);
                if let Some(pid) = parent {
                    if graph.node_opt(pid).is_some_and(|x| x.op == Op::If) {
                        out.push(pid);
                        if out.len() >= IF_CANDIDATES {
                            break;
                        }
                    }
                }
                parent
            }
            Op::If | Op::Switch { .. } | Op::Merge | Op::Region => n.input_opt(0),
            _ => None,
        };
        match next {
            Some(x) => cur = x,
            None => break,
        }
    }
    out
}

/// Which projection of an `If` (`projs`) dominates the control node `c`:
/// `Some(0)` / `Some(1)`, or `None` (neither, or the walk budget is spent).
fn dominated_side(
    graph: &Graph,
    users: &[Vec<NodeId>],
    cache: &mut FxHashMap<NodeId, FxHashSet<NodeId>>,
    projs: [NodeId; 2],
    c: NodeId,
) -> Option<usize> {
    let mut dom = [false; 2];
    for (s, &p) in projs.iter().enumerate() {
        if !cache.contains_key(&p) {
            if cache.len() >= DOMINANCE_WALKS {
                return None;
            }
            let set = control_not_dominated_by(graph, users, p);
            cache.insert(p, set);
        }
        dom[s] = !cache.get(&p)?.contains(&c);
    }
    match dom {
        [true, false] => Some(0),
        [false, true] => Some(1),
        _ => None,
    }
}

/// The condition of the `If` that decides the boolean φ `phi`, and whether
/// the φ is that condition (`true`: `1` on the true edge) or its negation.
fn deciding_if(
    graph: &Graph,
    users: &[Vec<NodeId>],
    reach: &FxHashSet<NodeId>,
    cache: &mut FxHashMap<NodeId, FxHashSet<NodeId>>,
    phi: NodeId,
) -> Option<(NodeId, bool, [NodeId; 2])> {
    let p = graph.node_opt(phi)?;
    let m = p.input_opt(0)?;
    let mn = graph.node_opt(m)?;
    let preds: Vec<NodeId> = mn.inputs.iter().copied().collect();
    let mut values: Vec<i64> = Vec::with_capacity(preds.len());
    for v in p.phi_value_inputs() {
        values.push(int_const_of(graph, v?)?);
    }
    if values.len() != preds.len() || preds.iter().any(|&c| c == NO_NODE || !reach.contains(&c)) {
        return None;
    }
    let first = *preds.first()?;
    for iff in dominating_if_candidates(graph, first) {
        let Some(n) = graph.node_opt(iff) else {
            continue;
        };
        if n.inputs.len() != 2 {
            continue;
        }
        let Some(cond) = n.input_opt(1) else {
            continue;
        };
        let Some(cn) = graph.node_opt(cond) else {
            continue;
        };
        if cn.ty != IrType::Int || is_jump_entered_flag(&cn.op) || matches!(cn.op, Op::Const(_)) {
            continue;
        }
        let Some(projs) = if_projections(graph, users, iff) else {
            continue;
        };
        let mut side_val: [Option<i64>; 2] = [None, None];
        let mut ok = true;
        for (&c, &v) in preds.iter().zip(values.iter()) {
            let Some(side) = dominated_side(graph, users, cache, projs, c) else {
                ok = false;
                break;
            };
            match side_val[side] {
                None => side_val[side] = Some(v),
                Some(w) if w == v => {}
                Some(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        match side_val {
            [Some(1), Some(0)] => return Some((cond, true, projs)),
            [Some(0), Some(1)] => return Some((cond, false, projs)),
            _ => {}
        }
    }
    None
}

/// The opaque OSR arms of the graph: each `Proj(0)` of an `If(OsrEntryFlag)`.
fn opaque_osr_arms(graph: &Graph) -> Vec<NodeId> {
    graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.op == Op::Proj(0)
                && n.ty == IrType::Control
                && n.input_opt(0)
                    .and_then(|i| graph.node_opt(i))
                    .filter(|i| i.op == Op::If)
                    .and_then(|i| i.input_opt(1))
                    .and_then(|f| graph.node_opt(f))
                    .is_some_and(|f| matches!(f.op, Op::OsrEntryFlag { .. }))
        })
        .map(|(id, _)| id as NodeId)
        .collect()
}

/// Review R1: does an opaque OSR arm lie between the deciding `If` (`projs`)
/// and the join -- dominated by one of its projections, with the join
/// forward-reachable from it? The replacement would then be live across the
/// arm's entry and the lowering would withhold it. `true` (refuse) also when
/// the dominance budget is spent.
fn crosses_opaque_osr_arm(
    graph: &Graph,
    users: &[Vec<NodeId>],
    cache: &mut FxHashMap<NodeId, FxHashSet<NodeId>>,
    forward: &mut FxHashMap<NodeId, FxHashSet<NodeId>>,
    arms: &[NodeId],
    projs: [NodeId; 2],
    join: NodeId,
) -> bool {
    for &arm in arms {
        let mut inside = false;
        for &p in &projs {
            if !cache.contains_key(&p) {
                if cache.len() >= DOMINANCE_WALKS {
                    return true;
                }
                let set = control_not_dominated_by(graph, users, p);
                cache.insert(p, set);
            }
            match cache.get(&p) {
                Some(set) if !set.contains(&arm) => inside = true,
                Some(_) => {}
                None => return true,
            }
        }
        if !inside {
            continue;
        }
        let reached = forward
            .entry(arm)
            .or_insert_with(|| forward_control_closure(graph, users, arm));
        if reached.contains(&join) {
            return true;
        }
    }
    false
}

/// The node equal to `c != 0` (`positive`) or `c == 0`, reusing `c` or
/// negating its compare where that is exact.
fn boolean_of(graph: &mut Graph, cond: NodeId, positive: bool, pc: Option<usize>) -> NodeId {
    if positive {
        if known_boolean(graph, cond, BOOL_DEPTH) {
            return cond;
        }
        let zero = int_const_node(graph, 0);
        return graph.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cond, zero], pc);
    }
    if let Some((cc, a, b)) = negatable_cmp(graph, cond) {
        return graph.add(Op::Cmp(cc.negate()), IrType::Int, vec![a, b], pc);
    }
    let zero = int_const_node(graph, 0);
    graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![cond, zero], pc)
}

// ── The boolean compare ───────────────────────────────────────────────

/// What a compare of a boolean against a constant is.
enum BoolRewrite {
    /// The boolean itself.
    Same(NodeId),
    /// The boolean compare `Cmp(cc, a, b)` (already negated).
    Negated(CmpOp, NodeId, NodeId),
    /// A constant.
    Const(i64),
}

fn boolean_compare_rewrite(graph: &Graph, id: NodeId) -> Option<BoolRewrite> {
    let n = graph.node_opt(id)?;
    let Op::Cmp(cc) = n.op else {
        return None;
    };
    if n.ty != IrType::Int || n.inputs.len() != 2 {
        return None;
    }
    let (a, b) = (n.input_opt(0)?, n.input_opt(1)?);
    let (x, cc, k) = match (int_const_of(graph, b), int_const_of(graph, a)) {
        (Some(k), None) => (a, cc, k),
        (None, Some(k)) => (b, cc.swapped(), k),
        _ => return None,
    };
    if !known_boolean(graph, x, BOOL_DEPTH) {
        return None;
    }
    let k = k as i32;
    match (cmp_holds(cc, 0, k), cmp_holds(cc, 1, k)) {
        (false, true) => Some(BoolRewrite::Same(x)),
        (true, false) => {
            let (inner, p, q) = negatable_cmp(graph, x)?;
            Some(BoolRewrite::Negated(inner.negate(), p, q))
        }
        (true, true) => Some(BoolRewrite::Const(1)),
        (false, false) => Some(BoolRewrite::Const(0)),
    }
}

/// Rewrite every `Cmp(cc, B, k)` of a known boolean `B` (see the module
/// notes). Returns how many compares were replaced.
fn simplify_boolean_compares(graph: &mut Graph) -> usize {
    let len = graph.nodes.len();
    let mut changed = 0usize;
    for id in 0..len {
        let id = id as NodeId;
        let Some(rewrite) = boolean_compare_rewrite(graph, id) else {
            continue;
        };
        let pc = graph.nodes[id as usize].bytecode_pc;
        let replacement = match rewrite {
            BoolRewrite::Same(x) => x,
            BoolRewrite::Negated(cc, a, b) => graph.add(Op::Cmp(cc), IrType::Int, vec![a, b], pc),
            BoolRewrite::Const(v) => int_const_node(graph, v),
        };
        if replacement == id {
            continue;
        }
        graph.replace_all_uses(id, replacement);
        graph.kill(id);
        changed += 1;
    }
    changed
}

/// `And(B, 1)` / `And(1, B)` of a known boolean `B` (a compare, a `0`/`1`
/// constant, a φ of those, or another such mask) is `B`: the `boolean`
/// return narrowing a spliced `ireturn` leaves on a value that is already
/// `0` or `1`. Returns how many masks were replaced. Exact: `B & 1 == B` for
/// `B` in `{0, 1}`.
fn fold_boolean_masks(graph: &mut Graph) -> usize {
    let len = graph.nodes.len();
    let mut changed = 0usize;
    for id in 0..len {
        let id = id as NodeId;
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if n.op != Op::And || n.ty != IrType::Int || n.inputs.len() != 2 {
            continue;
        }
        let (a, b) = (n.inputs[0], n.inputs[1]);
        let x = match (int_const_of(graph, a), int_const_of(graph, b)) {
            (None, Some(1)) => a,
            (Some(1), None) => b,
            _ => continue,
        };
        if x == id || !known_boolean(graph, x, BOOL_DEPTH) {
            continue;
        }
        graph.replace_all_uses(id, x);
        graph.kill(id);
        changed += 1;
    }
    changed
}

// ── The empty diamond ─────────────────────────────────────────────────

/// One empty diamond's collapse, decided before any mutation.
struct DiamondPlan {
    iff: NodeId,
    /// The `If`'s control: the join's new input.
    ctrl: NodeId,
    merge: NodeId,
    /// The join's input positions of the two arms (`j0` takes `ctrl`, `j1`
    /// goes).
    j0: usize,
    j1: usize,
    /// Both projections and the single-input merges between them and the join.
    chain: Vec<NodeId>,
}

/// From the projection `p`, follow single-input merges, each the only live
/// user of the node before it, to the multi-input `Merge` they end at:
/// `(join, the node naming it, every node on the way)`.
fn empty_arm(
    graph: &Graph,
    users: &[Vec<NodeId>],
    p: NodeId,
) -> Option<(NodeId, NodeId, Vec<NodeId>)> {
    let mut chain = vec![p];
    let mut cur = p;
    for _ in 0..8 {
        let live: Vec<NodeId> = users
            .get(cur as usize)?
            .iter()
            .copied()
            .filter(|&u| graph.node_opt(u).is_some_and(|n| n.op != Op::Dead))
            .collect();
        let [u] = live.as_slice() else {
            return None;
        };
        let u = *u;
        let un = graph.node_opt(u)?;
        if un.op != Op::Merge {
            return None;
        }
        if un.inputs.len() == 1 {
            chain.push(u);
            cur = u;
            continue;
        }
        return Some((u, cur, chain));
    }
    None
}

/// The one position of `x` in `slice`, or `None` (absent or repeated).
fn single_position(slice: &[NodeId], x: NodeId) -> Option<usize> {
    let mut found = None;
    for (j, &c) in slice.iter().enumerate() {
        if c == x {
            if found.is_some() {
                return None;
            }
            found = Some(j);
        }
    }
    found
}

fn plan_empty_diamond(
    graph: &Graph,
    users: &[Vec<NodeId>],
    retreating: &FxHashSet<NodeId>,
    iff: NodeId,
) -> Option<DiamondPlan> {
    let n = graph.node_opt(iff)?;
    if n.op != Op::If || n.inputs.len() != 2 {
        return None;
    }
    let ctrl = n.input_opt(0)?;
    let cn = graph.node_opt(ctrl)?;
    if cn.ty != IrType::Control || !matches!(cn.op, Op::Proj(_) | Op::Merge | Op::Region) {
        return None;
    }
    let cond = graph.node_opt(n.input_opt(1)?)?;
    if is_jump_entered_flag(&cond.op) {
        return None;
    }
    let projs = if_projections(graph, users, iff)?;
    let (m0, last0, mut chain) = empty_arm(graph, users, projs[0])?;
    let (m1, last1, chain1) = empty_arm(graph, users, projs[1])?;
    if m0 != m1 || retreating.contains(&m0) {
        return None;
    }
    let mn = graph.node_opt(m0)?;
    if mn.op != Op::Merge || mn.inputs.iter().any(|&c| c == ctrl) {
        return None;
    }
    let j0 = single_position(mn.inputs.as_slice(), last0)?;
    let j1 = single_position(mn.inputs.as_slice(), last1)?;
    if j0 == j1 {
        return None;
    }
    // Every φ at the join agrees on the two edges.
    for &u in users.get(m0 as usize)? {
        let Some(un) = graph.node_opt(u) else {
            continue;
        };
        if un.op != Op::Phi || un.input_opt(0) != Some(m0) {
            continue;
        }
        if un.inputs.len() != mn.inputs.len() + 1 {
            return None;
        }
        if un.inputs.get(j0 + 1) != un.inputs.get(j1 + 1) {
            return None;
        }
    }
    chain.extend(chain1);
    Some(DiamondPlan {
        iff,
        ctrl,
        merge: m0,
        j0,
        j1,
        chain,
    })
}

fn apply_empty_diamond(graph: &mut Graph, plan: DiamondPlan) {
    let Some(mn) = graph.node_opt(plan.merge) else {
        return;
    };
    let new_inputs: Vec<NodeId> = mn
        .inputs
        .iter()
        .enumerate()
        .filter(|&(j, _)| j != plan.j1)
        .map(|(j, &c)| if j == plan.j0 { plan.ctrl } else { c })
        .collect();
    let mut phis: Vec<NodeId> = graph
        .users_of(plan.merge)
        .into_iter()
        .filter(|&p| {
            graph
                .node_opt(p)
                .is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == Some(plan.merge))
        })
        .collect();
    phis.sort_unstable();
    phis.dedup();
    for p in phis {
        let Some(pn) = graph.node_opt(p) else {
            continue;
        };
        let inputs: Vec<NodeId> = pn
            .inputs
            .iter()
            .enumerate()
            .filter(|&(k, _)| k != plan.j1 + 1)
            .map(|(_, &v)| v)
            .collect();
        graph.set_inputs(p, inputs);
    }
    graph.set_inputs(plan.merge, new_inputs);
    for c in plan.chain {
        graph.kill(c);
    }
    graph.kill(plan.iff);
}

/// Collapse every empty diamond (see the module notes). Returns how many.
fn collapse_empty_diamonds(graph: &mut Graph) -> usize {
    let users = build_users(graph);
    let Some(retreating) = retreating_edge_targets(graph, &users) else {
        return 0;
    };
    let mut plans: Vec<DiamondPlan> = Vec::new();
    // One plan per join: a second would read input positions the first moves.
    let mut joins: FxHashSet<NodeId> = FxHashSet::default();
    for id in 0..graph.nodes.len() {
        if let Some(plan) = plan_empty_diamond(graph, &users, &retreating, id as NodeId) {
            if joins.insert(plan.merge) {
                plans.push(plan);
            }
        }
    }
    let n = plans.len();
    for plan in plans {
        apply_empty_diamond(graph, plan);
    }
    n
}

// ── The branch-implied `If` ───────────────────────────────────────────

/// Walk up the single-predecessor control chain from `ctrl` for an `If` edge
/// that decides `cond`: `Some(true)` / `Some(false)` for the value `cond`'s
/// `If` would test, `None` when no edge on the chain decides it.
pub(super) fn implied_by_dominating_edge(
    graph: &Graph,
    ctrl: NodeId,
    cond: NodeId,
) -> Option<bool> {
    let c = graph.node_opt(cond)?;
    let want = match c.op {
        Op::Cmp(cc) if c.inputs.len() == 2 => Some((cc, c.inputs[0], c.inputs[1])),
        _ => None,
    };
    let mut cur = ctrl;
    for _ in 0..WALK_BUDGET {
        let cn = graph.node_opt(cur)?;
        match cn.op {
            Op::Proj(k) => {
                let iff_id = cn.input_opt(0)?;
                let iff = graph.node_opt(iff_id)?;
                if iff.op != Op::If {
                    return None;
                }
                let c2_id = iff.input_opt(1)?;
                let c2 = graph.node_opt(c2_id)?;
                if is_jump_entered_flag(&c2.op) {
                    return None;
                }
                if k <= 1 {
                    if c2_id == cond {
                        return Some(k == 0);
                    }
                    if let (Some((cc, a, b)), Op::Cmp(cc2)) = (want, &c2.op) {
                        if c2.inputs.len() == 2 {
                            let held = if k == 0 { *cc2 } else { cc2.negate() };
                            let (x, y) = (c2.inputs[0], c2.inputs[1]);
                            let held_ab = if x == a && y == b {
                                Some(held)
                            } else if x == b && y == a {
                                Some(held.swapped())
                            } else {
                                None
                            };
                            if let Some(v) = held_ab.and_then(|h| cmp_implies(h, cc)) {
                                return Some(v);
                            }
                        }
                    }
                }
                cur = iff.input_opt(0)?;
            }
            Op::Merge | Op::Region if cn.inputs.len() == 1 => cur = cn.input_opt(0)?,
            _ => return None,
        }
    }
    None
}

/// Give every `If` a dominating edge decides a constant condition. Returns
/// how many.
fn decide_branch_implied_ifs(graph: &mut Graph) -> usize {
    let mut decided: Vec<(NodeId, bool)> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op != Op::If || n.inputs.len() != 2 {
            continue;
        }
        let (Some(ctrl), Some(cond)) = (n.input_opt(0), n.input_opt(1)) else {
            continue;
        };
        let Some(c) = graph.node_opt(cond) else {
            continue;
        };
        if matches!(c.op, Op::Const(_)) || is_jump_entered_flag(&c.op) {
            continue;
        }
        if let Some(v) = implied_by_dominating_edge(graph, ctrl, cond) {
            decided.push((id as NodeId, v));
        }
    }
    if decided.is_empty() {
        return 0;
    }
    let one = int_const_node(graph, 1);
    let zero = int_const_node(graph, 0);
    for &(iff, v) in &decided {
        graph.set_input(iff, 1, if v { one } else { zero });
    }
    decided.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ON: &[(&str, Option<&str>)] = &[
        ("CRATONVM_JIT_IR_BOOL_PHI_FOLD", Some("1")),
        ("CRATONVM_JIT_IR_EMPTY_DIAMOND_FOLD", Some("1")),
        ("CRATONVM_JIT_IR_BRANCH_IMPLIED_IFS", Some("1")),
    ];
    const ALL_OFF: &[(&str, Option<&str>)] = &[
        ("CRATONVM_JIT_IR_BOOL_PHI_FOLD", Some("0")),
        ("CRATONVM_JIT_IR_EMPTY_DIAMOND_FOLD", Some("0")),
        ("CRATONVM_JIT_IR_BRANCH_IMPLIED_IFS", Some("0")),
    ];

    /// The nodes of [`materialised_boolean`] a test reads back.
    struct Bool {
        g: Graph,
        entry: NodeId,
        a: NodeId,
        b: NodeId,
        zero: NodeId,
        lt: NodeId,
        iff: NodeId,
        t: NodeId,
        f: NodeId,
        join: NodeId,
        phi: NodeId,
        test: NodeId,
        test_if: NodeId,
        ret_t: NodeId,
    }

    /// `boolean r = a < b; if (r) return a; return r;` as the builder makes
    /// it: `If(a < b)` (pc 1), both arms into the join (pc 8) with
    /// `phi(1, 0)`, the `ifeq` `If(Cmp(Eq, phi, 0))` (pc 9), and the φ also
    /// read by a return (which is why the split-if would refuse it).
    /// `true_value` / `false_value` are the φ's constants on the two arms.
    fn materialised_boolean(true_value: i64, false_value: i64) -> Bool {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let konst = |v: i64| if v == 0 { zero } else { one };
        let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], Some(1));
        let iff = g.add(Op::If, IrType::Control, vec![entry, lt], Some(1));
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(1));
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(1));
        let join = g.add(Op::Merge, IrType::Control, vec![t, f], Some(8));
        let phi = g.add(
            Op::Phi,
            IrType::Int,
            vec![join, konst(true_value), konst(false_value)],
            Some(8),
        );
        let test = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![phi, zero], Some(9));
        let test_if = g.add(Op::If, IrType::Control, vec![join, test], Some(9));
        let t2 = g.add(Op::Proj(0), IrType::Control, vec![test_if], Some(9));
        let f2 = g.add(Op::Proj(1), IrType::Control, vec![test_if], Some(9));
        let ret_t = g.add(Op::Return, IrType::Void, vec![t2, phi], Some(20));
        g.exit = g.add(Op::Return, IrType::Void, vec![f2, a], Some(12));
        Bool {
            g,
            entry,
            a,
            b,
            zero,
            lt,
            iff,
            t,
            f,
            join,
            phi,
            test,
            test_if,
            ret_t,
        }
    }

    #[test]
    fn a_materialised_boolean_becomes_one_compare_and_branch() {
        let (census, d) = cratonvm_types::flags::with_thread_overrides(ALL_ON, || {
            let mut d = materialised_boolean(1, 0);
            let census = fold_round(&mut d.g);
            (census, d)
        });
        assert_eq!(
            (
                census.phis,
                census.compares,
                census.diamonds,
                census.branches
            ),
            (1, 1, 1, 0),
            "{census:?}"
        );
        // The φ is the compare itself; its `ifeq` is the negated compare.
        assert_eq!(d.g.nodes[d.phi as usize].op, Op::Dead);
        assert_eq!(d.g.nodes[d.test as usize].op, Op::Dead);
        let cond = d.g.nodes[d.test_if as usize].inputs[1];
        assert_eq!(d.g.nodes[cond as usize].op, Op::Cmp(CmpOp::Ge));
        assert_eq!(d.g.nodes[cond as usize].inputs.as_slice(), &[d.a, d.b][..]);
        // The diamond is gone: the join hangs off the entry.
        for dead in [d.iff, d.t, d.f] {
            assert_eq!(d.g.nodes[dead as usize].op, Op::Dead, "n{dead}");
        }
        assert_eq!(d.g.nodes[d.join as usize].inputs.as_slice(), &[d.entry][..]);
        // The return that read the φ reads the compare.
        assert_eq!(d.g.nodes[d.ret_t as usize].inputs[1], d.lt);
    }

    /// `phi(0, 1)` is the negation: a new `Cmp(Ge, a, b)`.
    #[test]
    fn an_inverted_boolean_is_the_negated_compare() {
        let mut d = materialised_boolean(0, 1);
        let len = d.g.nodes.len();
        assert_eq!(fold_boolean_phis(&mut d.g), 1);
        assert_eq!(d.g.nodes[d.phi as usize].op, Op::Dead);
        let r = d.g.nodes[d.test as usize].inputs[0];
        assert!(r as usize >= len, "a new node");
        assert_eq!(d.g.nodes[r as usize].op, Op::Cmp(CmpOp::Ge));
        assert_eq!(d.g.nodes[r as usize].inputs.as_slice(), &[d.a, d.b][..]);
        assert_eq!(d.g.nodes[r as usize].bytecode_pc, Some(1));
    }

    /// A third edge no projection of either `If` dominates: refused.
    #[test]
    fn an_edge_neither_side_dominates_refuses() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let outer = g.add(Op::If, IrType::Control, vec![entry, a], Some(1));
        let o_t = g.add(Op::Proj(0), IrType::Control, vec![outer], Some(1));
        let o_f = g.add(Op::Proj(1), IrType::Control, vec![outer], Some(1));
        let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], Some(4));
        let inner = g.add(Op::If, IrType::Control, vec![o_t, lt], Some(4));
        let t = g.add(Op::Proj(0), IrType::Control, vec![inner], Some(4));
        let f = g.add(Op::Proj(1), IrType::Control, vec![inner], Some(4));
        let join = g.add(Op::Merge, IrType::Control, vec![t, f, o_f], Some(9));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero, zero], Some(9));
        g.exit = g.add(Op::Return, IrType::Void, vec![join, phi], Some(10));
        assert_eq!(fold_boolean_phis(&mut g), 0);
        assert_eq!(g.nodes[phi as usize].op, Op::Phi);
    }

    /// An `If` on the opaque OSR flag never decides a φ: the stub enters its
    /// arm past it.
    #[test]
    fn a_jump_entered_if_decides_nothing() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let flag = g.add(
            Op::OsrEntryFlag { header_bci: 3 },
            IrType::Int,
            vec![entry],
            None,
        );
        let iff = g.add(Op::If, IrType::Control, vec![entry, flag], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let join = g.add(Op::Merge, IrType::Control, vec![t, f], Some(3));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero], Some(3));
        g.exit = g.add(Op::Return, IrType::Void, vec![join, phi], Some(10));
        assert_eq!(fold_boolean_phis(&mut g), 0);
        // With the arms agreeing, the diamond is still kept: the stub looks its
        // arm up by this `If`.
        assert!(g.set_input(phi, 2, one));
        let users = build_users(&g);
        let retreating = retreating_edge_targets(&g, &users).expect("a Start");
        assert!(plan_empty_diamond(&g, &users, &retreating, iff).is_none());
    }

    #[test]
    fn compares_of_a_boolean_against_a_constant_simplify() {
        let mut d = materialised_boolean(1, 0);
        let one = d.g.add(Op::Const(1), IrType::Int, vec![], None);
        let ne = d.g.add(
            Op::Cmp(CmpOp::Ne),
            IrType::Int,
            vec![d.lt, d.zero],
            Some(30),
        );
        let ge = d.g.add(
            Op::Cmp(CmpOp::Ge),
            IrType::Int,
            vec![d.lt, d.zero],
            Some(31),
        );
        let gt_left = d.g.add(
            Op::Cmp(CmpOp::Lt),
            IrType::Int,
            vec![d.zero, d.lt],
            Some(32),
        );
        let eq_one =
            d.g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![d.lt, one], Some(33));
        let not_bool =
            d.g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![d.a, d.zero], Some(34));
        let mut sinks = Vec::new();
        for c in [ne, ge, gt_left, eq_one, not_bool] {
            sinks.push(d.g.add(Op::Add, IrType::Int, vec![c, d.a], None));
        }
        // `ne`, `ge`, `gt_left`, `eq_one`, and the builder's own `test`
        // (`Cmp(Eq, phi, 0)`: the φ is boolean but no compare, so it stays).
        assert_eq!(simplify_boolean_compares(&mut d.g), 4);
        assert_eq!(d.g.nodes[sinks[0] as usize].inputs[0], d.lt);
        let k = d.g.nodes[sinks[1] as usize].inputs[0];
        assert_eq!(
            (d.g.nodes[k as usize].op.clone(), d.g.nodes[k as usize].ty),
            (Op::Const(1), IrType::Int)
        );
        assert_eq!(d.g.nodes[sinks[2] as usize].inputs[0], d.lt);
        assert_eq!(d.g.nodes[sinks[3] as usize].inputs[0], d.lt);
        assert_eq!(d.g.nodes[sinks[4] as usize].inputs[0], not_bool);
        assert_eq!(d.g.nodes[d.test as usize].op, Op::Cmp(CmpOp::Eq));
    }

    /// Two φ values that differ on the arms: the `If` decides something.
    #[test]
    fn a_diamond_whose_phi_differs_is_kept() {
        let mut d = materialised_boolean(1, 0);
        let users = build_users(&d.g);
        let retreating = retreating_edge_targets(&d.g, &users).expect("a Start");
        assert!(plan_empty_diamond(&d.g, &users, &retreating, d.iff).is_none());
        assert_eq!(collapse_empty_diamonds(&mut d.g), 0);
        assert_eq!(d.g.nodes[d.iff as usize].op, Op::If);
    }

    /// The same φ value on both arms (here through a single-input merge
    /// wrapper on one arm): collapsed, the φ keeps one edge.
    #[test]
    fn a_diamond_whose_phi_agrees_collapses_through_a_wrapper() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let iff = g.add(Op::If, IrType::Control, vec![entry, a], Some(1));
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(1));
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(1));
        let wrap = g.add(Op::Merge, IrType::Control, vec![t], Some(5));
        let join = g.add(Op::Merge, IrType::Control, vec![wrap, f], Some(8));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, a, a], Some(8));
        let ret = g.add(Op::Return, IrType::Void, vec![join, phi], Some(10));
        g.exit = ret;
        assert_eq!(collapse_empty_diamonds(&mut g), 1);
        for dead in [iff, t, f, wrap] {
            assert_eq!(g.nodes[dead as usize].op, Op::Dead, "n{dead}");
        }
        assert_eq!(g.nodes[join as usize].inputs.as_slice(), &[entry][..]);
        assert_eq!(g.nodes[phi as usize].inputs.as_slice(), &[join, a][..]);
    }

    /// `if (a < b) { if (a <= b) .. if (a < b) .. } else { if (b > a) .. }`:
    /// implied, implied (same node), refuted.
    #[test]
    fn an_if_a_dominating_edge_decides_gets_a_constant() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], Some(1));
        let top = g.add(Op::If, IrType::Control, vec![entry, lt], Some(1));
        let t = g.add(Op::Proj(0), IrType::Control, vec![top], Some(1));
        let f = g.add(Op::Proj(1), IrType::Control, vec![top], Some(1));
        let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![a, b], Some(4));
        let j1 = g.add(Op::If, IrType::Control, vec![t, le], Some(4));
        let j1t = g.add(Op::Proj(0), IrType::Control, vec![j1], Some(4));
        let j3 = g.add(Op::If, IrType::Control, vec![j1t, lt], Some(6));
        let gt_ba = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![b, a], Some(8));
        let j2 = g.add(Op::If, IrType::Control, vec![f, gt_ba], Some(8));
        g.exit = g.add(Op::Return, IrType::Void, vec![f], Some(10));
        assert_eq!(decide_branch_implied_ifs(&mut g), 3);
        let cond_of = |g: &Graph, iff: NodeId| {
            let c = g.nodes[iff as usize].inputs[1];
            g.nodes[c as usize].op.clone()
        };
        assert_eq!(cond_of(&g, j1), Op::Const(1));
        assert_eq!(cond_of(&g, j3), Op::Const(1));
        assert_eq!(cond_of(&g, j2), Op::Const(0));
        assert_eq!(cond_of(&g, top), Op::Cmp(CmpOp::Lt));
    }

    /// The walk stops at an `If` on a jump-entered flag: a fact from above it
    /// does not hold on the path the jump takes.
    #[test]
    fn an_implied_if_walk_stops_at_a_jump_entered_if() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], Some(1));
        let top = g.add(Op::If, IrType::Control, vec![entry, lt], Some(1));
        let t = g.add(Op::Proj(0), IrType::Control, vec![top], Some(1));
        let flag = g.add(
            Op::CatchEntryFlag {
                call_bci: 20,
                site: 0,
            },
            IrType::Int,
            vec![t],
            None,
        );
        let arm_if = g.add(Op::If, IrType::Control, vec![t, flag], None);
        let arm = g.add(Op::Proj(0), IrType::Control, vec![arm_if], None);
        let inner = g.add(Op::If, IrType::Control, vec![arm, lt], Some(22));
        g.exit = g.add(Op::Return, IrType::Void, vec![arm], Some(30));
        assert_eq!(decide_branch_implied_ifs(&mut g), 0);
        assert_eq!(g.nodes[inner as usize].inputs[1], lt);
    }

    /// Review R1: an opaque OSR arm inside the deciding `If`'s true region,
    /// from which the join is reachable, keeps the φ (folding it would make
    /// the lowering withhold that OSR entry). The same graph without the
    /// flag's arm folds.
    #[test]
    fn an_opaque_osr_arm_between_the_if_and_the_join_keeps_the_phi() {
        let build = |with_arm: bool| {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            g.entry = start;
            let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
            let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let one = g.add(Op::Const(1), IrType::Int, vec![], None);
            let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], Some(1));
            let iff = g.add(Op::If, IrType::Control, vec![entry, lt], Some(1));
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(1));
            let f = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(1));
            let t_side = if with_arm {
                let flag = g.add(
                    Op::OsrEntryFlag { header_bci: 5 },
                    IrType::Int,
                    vec![t],
                    None,
                );
                let flag_if = g.add(Op::If, IrType::Control, vec![t, flag], None);
                let arm = g.add(Op::Proj(0), IrType::Control, vec![flag_if], None);
                let normal = g.add(Op::Proj(1), IrType::Control, vec![flag_if], None);
                g.add(Op::Merge, IrType::Control, vec![normal, arm], Some(5))
            } else {
                t
            };
            let join = g.add(Op::Merge, IrType::Control, vec![t_side, f], Some(8));
            let phi = g.add(Op::Phi, IrType::Int, vec![join, one, zero], Some(8));
            g.exit = g.add(Op::Return, IrType::Void, vec![join, phi], Some(10));
            (g, phi)
        };
        let (mut g, phi) = build(true);
        assert_eq!(fold_boolean_phis(&mut g), 0);
        assert_eq!(g.nodes[phi as usize].op, Op::Phi);
        let (mut g, phi) = build(false);
        assert_eq!(fold_boolean_phis(&mut g), 1);
        assert_eq!(g.nodes[phi as usize].op, Op::Dead);
    }

    /// Default ON since round 26 (R26-FLIP); the `=0` arms turn every fold off.
    #[test]
    fn the_switches_are_default_on_and_zero_kills_them() {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_BOOL_PHI_FOLD", None),
                ("CRATONVM_JIT_IR_EMPTY_DIAMOND_FOLD", None),
                ("CRATONVM_JIT_IR_BRANCH_IMPLIED_IFS", None),
            ],
            || {
                assert!(bool_phi_fold_enabled());
                assert!(empty_diamond_fold_enabled());
                assert!(branch_implied_ifs_enabled());
            },
        );
        let (census, op) = cratonvm_types::flags::with_thread_overrides(ALL_OFF, || {
            let mut d = materialised_boolean(1, 0);
            let census = fold_round(&mut d.g);
            (census, d.g.nodes[d.phi as usize].op.clone())
        });
        assert_eq!(census, BoolBranchCensus::default());
        assert_eq!(op, Op::Phi);
    }

    /// `And(B, 1)` of a known boolean (a compare, or a φ of `0`/`1`) is `B`;
    /// a mask of a plain `int` is left alone.
    #[test]
    fn a_boolean_mask_folds_to_the_boolean() {
        let mut b = materialised_boolean(1, 0);
        let one = int_const_node(&mut b.g, 1);
        let m_cmp = b.g.add(Op::And, IrType::Int, vec![b.lt, one], Some(30));
        let m_phi = b.g.add(Op::And, IrType::Int, vec![one, b.phi], Some(31));
        let m_int = b.g.add(Op::And, IrType::Int, vec![b.a, one], Some(32));
        let r1 = b.g.add(Op::Add, IrType::Int, vec![m_cmp, m_phi], Some(33));
        let r2 = b.g.add(Op::Add, IrType::Int, vec![r1, m_int], Some(34));
        let folded = fold_boolean_masks(&mut b.g);
        assert_eq!(folded, 2);
        assert_eq!(b.g.nodes[r1 as usize].inputs.as_slice(), &[b.lt, b.phi][..]);
        assert_eq!(b.g.nodes[r2 as usize].inputs[1], m_int);
        assert_eq!(b.g.nodes[m_int as usize].op, Op::And);
        // And a mask is itself a known boolean.
        assert!(known_boolean(&b.g, m_int, BOOL_DEPTH));
    }
}
