// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 1 (lane LOOPCG; page
//! `docs/feature-designs/jit-ir-optimizations.md`,
//! "Reassociation of an unrolled accumulation"): rewrite an `int` / `long`
//! tree of adds, subtracts, negations and constant scalings as its linear form
//! `sum(k_j * leaf_j) + c`, when that form takes fewer nodes
//! (`CRATONVM_JIT_IR_LINEAR_REASSOC`, default ON, `=0` kill).
//!
//! # Why
//!
//! A loop the unroller copies four times carries `acc + i + (i+1) + (i+2) +
//! (i+3)` per main trip: seven adds, three of whose results the accumulator
//! lowering homes and reloads (`counterInt`'s body on the page, ~27
//! instructions for four iterations). Its linear form is `acc + (i << 2) + 6`,
//! three nodes. `COUNTER += 3` four times is `c + 12`. The round-17 affine
//! pass (`super::reassociate_affine`, `CRATONVM_JIT_REASSOC`, opt-in) handles
//! ONE root only and runs before the unroller, so it never sees either shape.
//! C2 reassociates the same sums (`AddNode::Ideal`'s constant pulling and its
//! loop-invariant reassociation).
//!
//! # The tree
//!
//! A *linear node* is an `int` / `long` `Add`, `Sub`, `Neg`, a `Mul` with
//! exactly one `Const` operand, or a `Shl` by a `Const` count. A linear node
//! is INTERIOR -- folded into its reader's form -- when its only reader (one
//! input slot in the whole graph) is a linear node of the same type that
//! expands that operand, and no safepoint snapshot names it. Every other
//! linear node is the TOP of its own tree; every other operand is a leaf
//! (constants fold into `c`). So the trees are disjoint, and a tree's interior
//! nodes are read by nothing outside it.
//!
//! # Why it is exact
//!
//! Java `int` / `long` `+`, `-`, unary `-` and `*` are arithmetic modulo
//! 2^32 / 2^64 (JLS 15.17.1, 15.18.2, 15.15.4), a commutative ring, and `x <<
//! s` is `x * 2^(s & 31)` (resp. `& 63`) in that ring (JLS 15.19). Every
//! coefficient and the constant are computed in the same ring (`w_add` /
//! `w_mul` / `w_neg` wrap at the type's width), so the rebuilt tree computes
//! the same value for every input. None of these ops can trap, read memory
//! or deopt, so their order and count are unobservable.
//!
//! # Why the frames stay right
//!
//! The top's readers, snapshot slots included, move to the rebuilt value
//! (`Graph::replace_all_uses`), which is the same value at every program
//! point the top was. An interior node is named by no snapshot (that is part
//! of the definition) and read by nothing outside the tree, so killing it
//! loses nothing a deopt or another node could ask for. A snapshot-named
//! intermediate -- every bytecode-level `x + 1` the builder's operand-stack
//! snapshots record -- stays a leaf and stays alive.
//!
//! # Why it never costs
//!
//! A tree is rewritten only when the rebuilt form needs strictly fewer
//! non-constant nodes than the tree's interior plus its top, so a simple
//! induction step `i + 4` (one node either way) or a one-use index `i + k` is
//! never touched, and the node-count side of the acceptance gate only ever
//! falls. Constants are interned (`ConstIntern`).

use super::{int_width, w_add, w_mul, w_neg, ConstIntern, FxHashMap};
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};
use rustc_hash::FxHashSet;

/// At most this many distinct leaves per tree; a wider tree is left alone.
const MAX_TERMS: usize = 8;
/// At most this many linear nodes per tree (top included).
const MAX_TREE_NODES: usize = 64;

/// `CRATONVM_JIT_IR_LINEAR_REASSOC` (j27 wave 1, lane LOOPCG; performance,
/// **default ON**, `=0` kill): run [`reassociate_linear_trees`] after the loop
/// transforms. No `static`; read per compile.
pub(super) fn linear_reassoc_enabled() -> bool {
    true
}

/// What [`reassociate_linear_trees`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LinearReassocCensus {
    /// Trees rewritten.
    pub(super) trees: usize,
    /// Linear nodes those trees held (killed).
    pub(super) removed: usize,
    /// Non-constant nodes their rebuilt forms added.
    pub(super) added: usize,
}

/// One linear node, by its operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lin {
    Add(NodeId, NodeId),
    Sub(NodeId, NodeId),
    Neg(NodeId),
    /// `x * k` (a `Mul` by a constant, or a `Shl` by a constant count).
    Scale(NodeId, i64),
}

impl Lin {
    /// The operands this node's form expands.
    fn operands(self) -> ([NodeId; 2], usize) {
        match self {
            Lin::Add(a, b) | Lin::Sub(a, b) => ([a, b], 2),
            Lin::Neg(a) | Lin::Scale(a, _) => ([a, NO_NODE], 1),
        }
    }

    /// The coefficient each operand of [`Self::operands`] carries when the
    /// node itself carries `k`.
    fn coefs(self, k: i64, is_int: bool) -> [i64; 2] {
        match self {
            Lin::Add(..) => [k, k],
            Lin::Sub(..) => [k, w_neg(is_int, k)],
            Lin::Neg(_) => [w_neg(is_int, k), 0],
            Lin::Scale(_, s) => [w_mul(is_int, k, s), 0],
        }
    }
}

/// The value of an integer `Const` node, wrapped to the width (`is_int`).
fn const_of(graph: &Graph, id: NodeId, is_int: bool) -> Option<i64> {
    match graph.node_opt(id)?.op {
        // Cast: an `int` constant is its low 32 bits.
        Op::Const(v) => Some(if is_int { i64::from(v as i32) } else { v }),
        _ => None,
    }
}

/// `v` wrapped to the width.
fn wrap(is_int: bool, v: i64) -> i64 {
    if is_int {
        // Cast: an `int` value is its low 32 bits.
        i64::from(v as i32)
    } else {
        v
    }
}

/// `id` as a linear node, with its width (`true` = `int`), or `None`.
fn classify(graph: &Graph, id: NodeId) -> Option<(Lin, bool)> {
    let n = graph.node_opt(id)?;
    let is_int = int_width(n.ty)?;
    let ins = n.inputs.as_slice();
    let lin = plain_lin(&n.op, ins).or_else(|| scaled_lin(graph, &n.op, ins, is_int))?;
    Some((lin, is_int))
}

/// `Add` / `Sub` / `Neg` as a linear node.
fn plain_lin(op: &Op, ins: &[NodeId]) -> Option<Lin> {
    match (op, ins) {
        (Op::Add, &[a, b]) => Some(Lin::Add(a, b)),
        (Op::Sub, &[a, b]) => Some(Lin::Sub(a, b)),
        (Op::Neg, &[a]) => Some(Lin::Neg(a)),
        _ => None,
    }
}

/// A `Mul` by a constant or a `Shl` by a constant count as a linear node.
fn scaled_lin(graph: &Graph, op: &Op, ins: &[NodeId], is_int: bool) -> Option<Lin> {
    match (op, ins) {
        (Op::Mul, &[a, b]) => mul_scale(graph, a, b, is_int),
        (Op::Shl, &[a, s]) => shl_scale(graph, a, s, is_int),
        _ => None,
    }
}

/// `a * b` with exactly one constant operand.
fn mul_scale(graph: &Graph, a: NodeId, b: NodeId, is_int: bool) -> Option<Lin> {
    match (const_of(graph, a, is_int), const_of(graph, b, is_int)) {
        (None, Some(k)) => Some(Lin::Scale(a, k)),
        (Some(k), None) => Some(Lin::Scale(b, k)),
        _ => None,
    }
}

/// `a << s` with a constant count: `a * 2^(s & mask)`. JLS 15.19: only the
/// low five (`int`) / six (`long`) bits count; the count is an `int` at both
/// widths, read raw.
fn shl_scale(graph: &Graph, a: NodeId, s: NodeId, is_int: bool) -> Option<Lin> {
    let Op::Const(s) = graph.node_opt(s)?.op else {
        return None;
    };
    let mask = if is_int { 31 } else { 63 };
    // Cast: masked to 0..=63.
    let sh = (s & mask) as u32;
    Some(Lin::Scale(a, wrap(is_int, 1i64.wrapping_shl(sh))))
}

/// Non-wrapping constant offset range `[lo_k, hi_k]` for `I2L(iv + k)` inside
/// `region`'s loop `body`, where `iv` is an `int` induction `Phi`.
struct I2lRangeProof {
    lo_k: i64,
    hi_k: i64,
    region: NodeId,
    body: FxHashSet<NodeId>,
}

/// Returns `Some(d)` when `entry_pred` is `Proj(0, If(Cmp(Lt, bound, n)))` and
/// `bound == Sub(n, Const(d))` with `d >= 0`, as `main_post_unroll_loop` builds.
fn main_post_licenced_diff(graph: &Graph, entry_pred: NodeId, bound: NodeId) -> Option<i64> {
    let ep = graph.node_opt(entry_pred)?;
    if ep.op != Op::Proj(0) {
        return None;
    }
    let iff = graph.node_opt(ep.input_opt(0)?)?;
    if iff.op != Op::If {
        return None;
    }
    let cmp = graph.node_opt(iff.input_opt(1)?)?;
    if cmp.op != Op::Cmp(CmpOp::Lt) || cmp.input_opt(0) != Some(bound) {
        return None;
    }
    let n = cmp.input_opt(1)?;
    let bn = graph.node_opt(bound)?;
    if bn.op != Op::Sub || bn.ty != IrType::Int || bn.input_opt(0) != Some(n) {
        return None;
    }
    let d = super::const_i64(graph, bn.input_opt(1)?)?;
    (d >= 0).then_some(d)
}

/// Collect `int` induction `Phi`s whose `iv + k` cannot wrap `i32` on `k in [lo_k, hi_k]`.
fn collect_iv_range_proofs(
    graph: &Graph,
    users: &[Vec<NodeId>],
) -> FxHashMap<NodeId, I2lRangeProof> {
    let mut out = FxHashMap::default();
    for (region, entry_pred, back_ctrls) in super::loop_headers(graph) {
        let Some(entry_slot) = graph
            .node_opt(region)
            .and_then(|r| r.inputs.iter().position(|&p| p == entry_pred))
            .map(|p| p + 1)
        else {
            continue;
        };
        let back_slot = if entry_slot == 1 { 2 } else { 1 };
        let body = super::loop_body_with(graph, users, region, entry_pred, &back_ctrls);
        let Some(stay_ctrl) = super::mask_predicate::header_stay_edge(graph, region, &body) else {
            continue;
        };
        let Some(stay_proj) = graph.node_opt(stay_ctrl) else {
            continue;
        };
        let Op::Proj(which) = stay_proj.op else {
            continue;
        };
        let Some(if_node) = stay_proj.input_opt(0).and_then(|id| graph.node_opt(id)) else {
            continue;
        };
        let Some(cond) = if_node.input_opt(1).and_then(|id| graph.node_opt(id)) else {
            continue;
        };
        let Op::Cmp(cmp_op) = cond.op else {
            continue;
        };
        let [ca, cb] = cond.inputs.as_slice() else {
            continue;
        };
        for cand in super::anchored_at(graph, region) {
            let Some(pn) = graph.node_opt(cand) else {
                continue;
            };
            if pn.op != Op::Phi || pn.ty != IrType::Int || pn.inputs.len() != 3 {
                continue;
            }
            let (iv_left, bound) = if *ca == cand {
                (true, *cb)
            } else if *cb == cand {
                (false, *ca)
            } else {
                continue;
            };
            if body.contains(&bound) || graph.node_opt(bound).map(|n| n.ty) != Some(IrType::Int) {
                continue;
            }
            let mut cont = if which == 0 { cmp_op } else { cmp_op.negate() };
            if !iv_left {
                cont = cont.swapped();
            }
            let (entry, back) = (pn.inputs[entry_slot], pn.inputs[back_slot]);
            let Some(bn) = graph.node_opt(back) else {
                continue;
            };
            let stride = match (bn.op.clone(), bn.inputs.as_slice()) {
                (Op::Add, &[a, b]) if a == cand => super::const_i64(graph, b),
                (Op::Add, &[a, b]) if b == cand => super::const_i64(graph, a),
                (Op::Sub, &[a, b]) if a == cand => {
                    super::const_i64(graph, b).and_then(i64::checked_neg)
                }
                _ => None,
            };
            let Some(s) = stride.filter(|&s| s != 0) else {
                continue;
            };
            let init = if graph.strip_mined_inner_headers.contains(&region) {
                graph
                    .node_opt(entry)
                    .filter(|en| en.op == Op::Phi && en.inputs.len() >= 2)
                    .map_or(entry, |en| en.inputs[1])
            } else {
                entry
            };
            let (lo_k, hi_k) = if s > 0 && cont == CmpOp::Lt {
                let lo = super::const_i64(graph, init).map_or(0, |iv0| i64::from(i32::MIN) - iv0);
                let mut hi = 1i64;
                if let Some(bv) = super::const_i64(graph, bound) {
                    hi = hi.max(i64::from(i32::MAX) - (bv - 1));
                }
                if graph.strip_mined_inner_headers.contains(&region) {
                    hi = hi.max(s);
                }
                if let Some(d) = main_post_licenced_diff(graph, entry_pred, bound) {
                    hi = hi.max(d + 1);
                }
                (lo, hi)
            } else if s < 0 && cont == CmpOp::Gt {
                let mut lo = -1i64;
                if let Some(bv) = super::const_i64(graph, bound) {
                    lo = lo.min(i64::from(i32::MIN) - (bv + 1));
                }
                let hi = super::const_i64(graph, init).map_or(0, |iv0| i64::from(i32::MAX) - iv0);
                (lo, hi)
            } else {
                continue;
            };
            out.insert(
                cand,
                I2lRangeProof {
                    lo_k,
                    hi_k,
                    region,
                    body: body.clone(),
                },
            );
        }
    }
    out
}

/// True when `top` is evaluated only inside `proof`'s loop body (so `iv` satisfies
/// the loop's entry test wherever `top` runs).
fn top_inside_loop(
    graph: &Graph,
    users: &[Vec<NodeId>],
    named: &FxHashSet<NodeId>,
    top: NodeId,
    proof: &I2lRangeProof,
) -> bool {
    let us = &users[top as usize];
    !us.is_empty()
        && !named.contains(&top)
        && us.iter().all(|&u| {
            proof.body.contains(&u)
                || graph
                    .node_opt(u)
                    .is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == Some(proof.region))
        })
}

/// Match `I2L(iv)` or non-wrapping `I2L(iv +/- Const(k))` for `top`, returning `(iv, k, kill_i2l)`.
fn match_i2l_iv_term(
    graph: &Graph,
    users: &[Vec<NodeId>],
    counts: &[u32],
    named: &FxHashSet<NodeId>,
    iv_proofs: &FxHashMap<NodeId, I2lRangeProof>,
    top: NodeId,
    id: NodeId,
) -> Option<(NodeId, i64, bool)> {
    let n = graph.node_opt(id)?;
    if n.op != Op::I2L || n.ty != IrType::Long || n.inputs.len() != 1 {
        return None;
    }
    let inner = n.inputs[0];
    if let Some(proof) = iv_proofs.get(&inner) {
        if top_inside_loop(graph, users, named, top, proof) {
            return Some((inner, 0, false));
        }
    }
    let in_node = graph.node_opt(inner)?;
    if in_node.ty != IrType::Int || in_node.inputs.len() != 2 {
        return None;
    }
    let [a, b] = in_node.inputs.as_slice() else {
        return None;
    };
    let (iv, k) = match in_node.op {
        Op::Add => {
            if let Some(kv) = const_of(graph, *b, true).filter(|_| iv_proofs.contains_key(a)) {
                (*a, kv)
            } else if let Some(kv) = const_of(graph, *a, true).filter(|_| iv_proofs.contains_key(b))
            {
                (*b, kv)
            } else {
                return None;
            }
        }
        Op::Sub => {
            let kv = const_of(graph, *b, true).filter(|_| iv_proofs.contains_key(a))?;
            (*a, -kv)
        }
        _ => return None,
    };
    let proof = &iv_proofs[&iv];
    if k < proof.lo_k || k > proof.hi_k || !top_inside_loop(graph, users, named, top, proof) {
        return None;
    }
    Some((iv, k, single_unnamed_reader(counts, named, id)))
}

/// One tree: its linear nodes (top first) and its form.
struct Tree {
    nodes: Vec<NodeId>,
    terms: Vec<(NodeId, i64)>,
    existing_i2l: FxHashMap<NodeId, NodeId>,
    c: i64,
}

/// The walk [`collect`] runs.
struct Collector<'a> {
    graph: &'a Graph,
    users: &'a [Vec<NodeId>],
    counts: &'a [u32],
    named: &'a FxHashSet<NodeId>,
    iv_proofs: &'a FxHashMap<NodeId, I2lRangeProof>,
    top: NodeId,
    is_int: bool,
    ty: IrType,
    interior: &'a [bool],
    tree: Tree,
    seen: FxHashSet<NodeId>,
    stack: Vec<(NodeId, i64)>,
}

/// Collect the tree topped by `top`, or `None` when it is too wide, holds a
/// dangling operand, or is not a tree after all.
fn collect(
    graph: &Graph,
    users: &[Vec<NodeId>],
    counts: &[u32],
    named: &FxHashSet<NodeId>,
    iv_proofs: &FxHashMap<NodeId, I2lRangeProof>,
    top: NodeId,
    is_int: bool,
    interior: &[bool],
) -> Option<Tree> {
    let mut c = Collector {
        graph,
        users,
        counts,
        named,
        iv_proofs,
        top,
        is_int,
        ty: graph.node_opt(top)?.ty,
        interior,
        tree: Tree {
            nodes: Vec::new(),
            terms: Vec::new(),
            existing_i2l: FxHashMap::default(),
            c: 0,
        },
        seen: FxHashSet::default(),
        stack: vec![(top, 1)],
    };
    while let Some((id, k)) = c.stack.pop() {
        if c.is_tree_node(id) {
            c.expand(id, k)?;
        } else {
            c.add_leaf(id, k)?;
        }
    }
    c.tree.terms.retain(|&(_, k)| k != 0);
    Some(c.tree)
}

impl Collector<'_> {
    fn is_tree_node(&self, id: NodeId) -> bool {
        id == self.top || self.interior.get(id as usize).copied().unwrap_or(false)
    }

    /// Record tree node `id` (carrying `k`) and queue its operands.
    fn expand(&mut self, id: NodeId, k: i64) -> Option<()> {
        // A tree node is reached once (its single reader); a second visit
        // means the "tree" is not one, so leave it alone.
        if !self.seen.insert(id) || self.tree.nodes.len() >= MAX_TREE_NODES {
            return None;
        }
        let (lin, w) = classify(self.graph, id)?;
        if w != self.is_int || self.graph.node_opt(id)?.ty != self.ty {
            return None;
        }
        self.tree.nodes.push(id);
        let (ops, n) = lin.operands();
        let ks = lin.coefs(k, self.is_int);
        // Pushed right-first so the left operand's leaves come first.
        for j in (0..n).rev() {
            self.stack.push((ops[j], ks[j]));
        }
        Some(())
    }

    /// Fold leaf `id` (carrying `k`) into the form.
    fn add_leaf(&mut self, id: NodeId, k: i64) -> Option<()> {
        if self.graph.node_opt(id)?.op == Op::Dead {
            return None;
        }
        let is_int = self.is_int;
        if let Some(v) = const_of(self.graph, id, is_int) {
            self.tree.c = w_add(is_int, self.tree.c, w_mul(is_int, k, v));
            return Some(());
        }
        if !is_int {
            if let Some((iv, off, kill_i2l)) = match_i2l_iv_term(
                self.graph,
                self.users,
                self.counts,
                self.named,
                self.iv_proofs,
                self.top,
                id,
            ) {
                if off == 0 {
                    self.tree.existing_i2l.entry(iv).or_insert(id);
                } else if kill_i2l && self.seen.insert(id) {
                    if self.tree.nodes.len() >= MAX_TREE_NODES {
                        return None;
                    }
                    self.tree.nodes.push(id);
                }
                self.tree.c = w_add(false, self.tree.c, w_mul(false, k, off));
                return self.add_term(iv, k);
            }
        }
        self.add_term(id, k)
    }

    /// `k * id` into the term list, merged with an earlier term of `id`.
    fn add_term(&mut self, id: NodeId, k: i64) -> Option<()> {
        let is_int = self.is_int;
        if let Some((_, coef)) = self.tree.terms.iter_mut().find(|(n, _)| *n == id) {
            *coef = w_add(is_int, *coef, k);
            return Some(());
        }
        if self.tree.terms.len() >= MAX_TERMS {
            return None;
        }
        self.tree.terms.push((id, k));
        Some(())
    }
}

/// How one term is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TermShape {
    /// The leaf itself.
    Plain,
    /// `leaf << s`.
    Shl(i64),
    /// `leaf * k`.
    Mul(i64),
}

/// A term as built: the node, its shape, and whether it is subtracted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlannedTerm {
    leaf: NodeId,
    shape: TermShape,
    negated: bool,
}

/// `Some(s)` when `k` (wrapped) is `2^s` with `1 <= s < width - 1`, i.e. a
/// positive power of two other than 1.
fn pos_pow2(is_int: bool, k: i64) -> Option<i64> {
    let k = wrap(is_int, k);
    if k > 1 && k.count_ones() == 1 {
        Some(i64::from(k.trailing_zeros()))
    } else {
        None
    }
}

/// How `k * leaf` is built: the leaf, `+/- leaf << s`, or `leaf * k`.
fn plan_term(leaf: NodeId, k: i64, is_int: bool) -> PlannedTerm {
    let neg = w_neg(is_int, k);
    let (shape, negated) = if k == 1 {
        (TermShape::Plain, false)
    } else if neg == 1 {
        (TermShape::Plain, true)
    } else if let Some(s) = pos_pow2(is_int, k) {
        (TermShape::Shl(s), false)
    } else if let Some(s) = pos_pow2(is_int, neg) {
        (TermShape::Shl(s), true)
    } else {
        (TermShape::Mul(k), false)
    };
    PlannedTerm {
        leaf,
        shape,
        negated,
    }
}

/// The `Add` / `Sub` nodes joining `terms` and the constant `c`, as
/// [`Emit::build`] emits them.
fn combine_cost(terms: &[PlannedTerm], c: i64) -> usize {
    let pos = terms.iter().filter(|t| !t.negated).count();
    let neg = terms.len() - pos;
    if pos > 0 {
        // First positive term is the accumulator; every other term one
        // `Add` / `Sub`; the constant one `Add`.
        (pos - 1) + neg + usize::from(c != 0)
    } else {
        // `c - t0 - t1 ...`: one `Sub` per term (none for a pure constant).
        neg
    }
}

/// The rebuilt form's terms, and how many non-constant nodes building it
/// adds ([`Emit::build`] adds exactly that many).
fn plan(graph: &Graph, tree: &Tree, is_int: bool) -> (Vec<PlannedTerm>, usize) {
    let out: Vec<PlannedTerm> = tree
        .terms
        .iter()
        .map(|&(leaf, k)| plan_term(leaf, k, is_int))
        .collect();
    let shaped = out.iter().filter(|t| t.shape != TermShape::Plain).count();
    let widen = out
        .iter()
        .filter(|t| {
            !is_int
                && graph.nodes[t.leaf as usize].ty == IrType::Int
                && !tree.existing_i2l.contains_key(&t.leaf)
        })
        .count();
    let cost = shaped + widen + combine_cost(&out, tree.c);
    (out, cost)
}

/// Emits a planned form at one type and bytecode pc.
struct Emit<'a> {
    graph: &'a mut Graph,
    consts: &'a mut ConstIntern,
    existing_i2l: &'a FxHashMap<NodeId, NodeId>,
    ty: IrType,
    pc: Option<usize>,
}

impl Emit<'_> {
    fn node(&mut self, op: Op, a: NodeId, b: NodeId) -> NodeId {
        self.graph.add(op, self.ty, vec![a, b], self.pc)
    }

    fn konst(&mut self, v: i64, ty: IrType) -> NodeId {
        self.consts.get_or_add(self.graph, v, ty)
    }

    fn term(&mut self, t: &PlannedTerm) -> NodeId {
        let ty = self.ty;
        let leaf = if ty == IrType::Long && self.graph.nodes[t.leaf as usize].ty == IrType::Int {
            self.existing_i2l
                .get(&t.leaf)
                .copied()
                .unwrap_or_else(|| self.graph.add(Op::I2L, IrType::Long, vec![t.leaf], self.pc))
        } else {
            t.leaf
        };
        match t.shape {
            TermShape::Plain => leaf,
            TermShape::Shl(s) => {
                // The count is an `int` at both widths (`ishl` / `lshl`).
                let sc = self.konst(s, IrType::Int);
                self.node(Op::Shl, leaf, sc)
            }
            TermShape::Mul(k) => {
                let kc = self.konst(k, ty);
                self.node(Op::Mul, leaf, kc)
            }
        }
    }

    /// The positive terms added left to right, or `None` when there are none.
    fn sum_positives(&mut self, terms: &[PlannedTerm]) -> Option<NodeId> {
        let mut acc: Option<NodeId> = None;
        for t in terms.iter().filter(|t| !t.negated) {
            let v = self.term(t);
            acc = Some(match acc {
                None => v,
                Some(a) => self.node(Op::Add, a, v),
            });
        }
        acc
    }

    /// `acc` (or the constant `c` when `None`) minus each negated term.
    fn subtract_negatives(
        &mut self,
        terms: &[PlannedTerm],
        mut acc: Option<NodeId>,
        c: i64,
    ) -> Option<NodeId> {
        let ty = self.ty;
        for t in terms.iter().filter(|t| t.negated) {
            let v = self.term(t);
            let a = match acc {
                Some(a) => a,
                None => self.konst(c, ty),
            };
            acc = Some(self.node(Op::Sub, a, v));
        }
        acc
    }

    /// Build the planned form; answers the node that computes it.
    fn build(&mut self, terms: &[PlannedTerm], c: i64) -> NodeId {
        let ty = self.ty;
        let positive = self.sum_positives(terms);
        let had_positive = positive.is_some();
        match self.subtract_negatives(terms, positive, c) {
            Some(a) if had_positive && c != 0 => {
                let cc = self.konst(c, ty);
                self.node(Op::Add, a, cc)
            }
            Some(a) => a,
            None => self.konst(c, ty),
        }
    }
}

/// Every node a safepoint snapshot names.
fn snapshot_named(graph: &Graph) -> FxHashSet<NodeId> {
    graph
        .safepoints
        .iter()
        .flat_map(|sp| {
            sp.locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
        })
        .copied()
        .filter(|&v| v != NO_NODE)
        .collect()
}

/// Is `x` read by one input slot only and named by no snapshot?
fn single_unnamed_reader(counts: &[u32], named: &FxHashSet<NodeId>, x: NodeId) -> bool {
    counts.get(x as usize).copied() == Some(1) && !named.contains(&x)
}

/// Does linear operand `x` of linear node `u` fold into `u`'s form?
fn folds_into(
    graph: &Graph,
    counts: &[u32],
    named: &FxHashSet<NodeId>,
    u: usize,
    x: NodeId,
) -> bool {
    let xi = x as usize;
    x != NO_NODE
        && xi < graph.nodes.len()
        && xi != u
        && single_unnamed_reader(counts, named, x)
        && graph.nodes[xi].ty == graph.nodes[u].ty
        && classify(graph, x).is_some()
}

/// Which linear nodes fold into their single reader's form.
fn interior_nodes(graph: &Graph, counts: &[u32], named: &FxHashSet<NodeId>) -> Vec<bool> {
    let mut interior = vec![false; graph.nodes.len()];
    for u in 0..graph.nodes.len() {
        // Cast: an arena index is a node id.
        let Some((lin, _)) = classify(graph, u as NodeId) else {
            continue;
        };
        let (ops, n) = lin.operands();
        for &x in &ops[..n] {
            if folds_into(graph, counts, named, u, x) {
                interior[x as usize] = true;
            }
        }
    }
    interior
}

/// Rewrite the tree topped by `top` when its rebuilt form is smaller;
/// answers `(nodes removed, nodes added)`.
#[allow(clippy::too_many_arguments)]
fn rewrite_tree(
    graph: &mut Graph,
    consts: &mut ConstIntern,
    users: &[Vec<NodeId>],
    counts: &[u32],
    named: &FxHashSet<NodeId>,
    iv_proofs: &FxHashMap<NodeId, I2lRangeProof>,
    top: NodeId,
    interior: &[bool],
) -> Option<(usize, usize)> {
    let (_, is_int) = classify(graph, top)?;
    let tree = collect(
        graph, users, counts, named, iv_proofs, top, is_int, interior,
    )?;
    let (terms, cost) = plan(graph, &tree, is_int);
    if cost >= tree.nodes.len() {
        return None;
    }
    let (ty, pc) = {
        let n = &graph.nodes[top as usize];
        (n.ty, n.bytecode_pc)
    };
    let new = Emit {
        graph: &mut *graph,
        consts,
        existing_i2l: &tree.existing_i2l,
        ty,
        pc,
    }
    .build(&terms, tree.c);
    if new == top {
        return None;
    }
    graph.replace_all_uses(top, new);
    for &id in &tree.nodes {
        graph.kill(id);
    }
    Some((tree.nodes.len(), cost))
}

/// The pass. See the module doc.
pub(super) fn reassociate_linear_trees(graph: &mut Graph) -> LinearReassocCensus {
    let counts = graph.use_counts();
    let named = snapshot_named(graph);
    let interior = interior_nodes(graph, &counts, &named);
    let users = super::build_users(graph);
    let iv_proofs = collect_iv_range_proofs(graph, &users);
    let mut consts = ConstIntern::new();
    let mut census = LinearReassocCensus::default();
    for (u, &is_interior) in interior.iter().enumerate() {
        if is_interior {
            continue;
        }
        // Cast: an arena index is a node id.
        if let Some((removed, added)) = rewrite_tree(
            graph,
            &mut consts,
            &users,
            &counts,
            &named,
            &iv_proofs,
            u as NodeId,
            &interior,
        ) {
            census.trees += 1;
            census.removed += removed;
            census.added += added;
        }
    }
    census
}

#[cfg(test)]
mod tests;
