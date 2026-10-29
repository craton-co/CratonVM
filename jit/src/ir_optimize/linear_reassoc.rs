// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 1 (lane LOOPCG; page
//! `docs/known-issues/jit/jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md`,
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

use super::{int_width, w_add, w_mul, w_neg, ConstIntern};
use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};
use rustc_hash::FxHashSet;

/// At most this many distinct leaves per tree; a wider tree is left alone.
const MAX_TERMS: usize = 8;
/// At most this many linear nodes per tree (top included).
const MAX_TREE_NODES: usize = 64;

/// `CRATONVM_JIT_IR_LINEAR_REASSOC` (j27 wave 1, lane LOOPCG; performance,
/// **default ON**, `=0` kill): run [`reassociate_linear_trees`] after the loop
/// transforms. No `static`; read per compile.
pub(super) fn linear_reassoc_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LINEAR_REASSOC")
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

/// One tree: its linear nodes (top first) and its form.
struct Tree {
    nodes: Vec<NodeId>,
    terms: Vec<(NodeId, i64)>,
    c: i64,
}

/// The walk [`collect`] runs.
struct Collector<'a> {
    graph: &'a Graph,
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
fn collect(graph: &Graph, top: NodeId, is_int: bool, interior: &[bool]) -> Option<Tree> {
    let mut c = Collector {
        graph,
        top,
        is_int,
        ty: graph.node_opt(top)?.ty,
        interior,
        tree: Tree {
            nodes: Vec::new(),
            terms: Vec::new(),
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
fn plan(tree: &Tree, is_int: bool) -> (Vec<PlannedTerm>, usize) {
    let out: Vec<PlannedTerm> = tree
        .terms
        .iter()
        .map(|&(leaf, k)| plan_term(leaf, k, is_int))
        .collect();
    let shaped = out.iter().filter(|t| t.shape != TermShape::Plain).count();
    let cost = shaped + combine_cost(&out, tree.c);
    (out, cost)
}

/// Emits a planned form at one type and bytecode pc.
struct Emit<'a> {
    graph: &'a mut Graph,
    consts: &'a mut ConstIntern,
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
        match t.shape {
            TermShape::Plain => t.leaf,
            TermShape::Shl(s) => {
                // The count is an `int` at both widths (`ishl` / `lshl`).
                let sc = self.konst(s, IrType::Int);
                self.node(Op::Shl, t.leaf, sc)
            }
            TermShape::Mul(k) => {
                let kc = self.konst(k, ty);
                self.node(Op::Mul, t.leaf, kc)
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
fn interior_nodes(graph: &Graph) -> Vec<bool> {
    let counts = graph.use_counts();
    let named = snapshot_named(graph);
    let mut interior = vec![false; graph.nodes.len()];
    for u in 0..graph.nodes.len() {
        // Cast: an arena index is a node id.
        let Some((lin, _)) = classify(graph, u as NodeId) else {
            continue;
        };
        let (ops, n) = lin.operands();
        for &x in &ops[..n] {
            if folds_into(graph, &counts, &named, u, x) {
                interior[x as usize] = true;
            }
        }
    }
    interior
}

/// Rewrite the tree topped by `top` when its rebuilt form is smaller;
/// answers `(nodes removed, nodes added)`.
fn rewrite_tree(
    graph: &mut Graph,
    consts: &mut ConstIntern,
    top: NodeId,
    interior: &[bool],
) -> Option<(usize, usize)> {
    let (_, is_int) = classify(graph, top)?;
    let tree = collect(graph, top, is_int, interior)?;
    let (terms, cost) = plan(&tree, is_int);
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
    let interior = interior_nodes(graph);
    let mut consts = ConstIntern::new();
    let mut census = LinearReassocCensus::default();
    for (u, &is_interior) in interior.iter().enumerate() {
        if is_interior {
            continue;
        }
        // Cast: an arena index is a node id.
        if let Some((removed, added)) = rewrite_tree(graph, &mut consts, u as NodeId, &interior) {
            census.trees += 1;
            census.removed += removed;
            census.added += added;
        }
    }
    census
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::IrBuilder;

    /// `int f(int a, int b) { return a + b; }`: a graph with two `int`
    /// parameters whose returned value the tests replace.
    fn two_param_graph() -> (Graph, NodeId, NodeId, NodeId) {
        // iload_0; iload_1; iadd; ireturn
        let code = [0x1a, 0x1b, 0x60, 0xac];
        let g = IrBuilder::new(2, 2).build(&code, code.len()).expect("build");
        let ret = g.nodes[g.exit as usize].inputs[1];
        let add = &g.nodes[ret as usize];
        assert_eq!(add.op, Op::Add);
        let (a, b) = (add.inputs[0], add.inputs[1]);
        (g, ret, a, b)
    }

    fn k(g: &mut Graph, v: i64, ty: IrType) -> NodeId {
        g.add(Op::Const(v), ty, vec![], None)
    }

    fn bin(g: &mut Graph, op: Op, ty: IrType, a: NodeId, b: NodeId) -> NodeId {
        g.add(op, ty, vec![a, b], None)
    }

    /// Evaluate `id` over the two parameters, Java `int` semantics.
    fn eval_int(g: &Graph, id: NodeId, a: NodeId, b: NodeId, va: i32, vb: i32) -> i32 {
        if id == a {
            return va;
        }
        if id == b {
            return vb;
        }
        let n = &g.nodes[id as usize];
        let x = |s: usize| eval_int(g, n.inputs[s], a, b, va, vb);
        match n.op {
            // Cast: an `int` constant is its low 32 bits.
            Op::Const(v) => v as i32,
            Op::Add => x(0).wrapping_add(x(1)),
            Op::Sub => x(0).wrapping_sub(x(1)),
            Op::Mul => x(0).wrapping_mul(x(1)),
            Op::Neg => x(0).wrapping_neg(),
            // Cast: JLS 15.19 masks the count.
            Op::Shl => x(0).wrapping_shl((x(1) & 31) as u32),
            ref op => panic!("unexpected {op:?} at n{id}"),
        }
    }

    /// Non-constant nodes the returned value reaches, parameters excluded.
    fn reached_ops(g: &Graph, id: NodeId, a: NodeId, b: NodeId) -> usize {
        if id == a || id == b {
            return 0;
        }
        let n = &g.nodes[id as usize];
        if matches!(n.op, Op::Const(_)) {
            return 0;
        }
        1 + n
            .inputs
            .as_slice()
            .iter()
            .map(|&i| reached_ops(g, i, a, b))
            .sum::<usize>()
    }

    fn ret_of(g: &Graph) -> NodeId {
        g.nodes[g.exit as usize].inputs[1]
    }

    const VALUES: [i32; 7] = [0, 1, -1, 7, i32::MAX, i32::MIN, 0x1234_5678];

    /// `acc + i + (i+1) + (i+2) + (i+3)` (the unrolled accumulation of the
    /// page) becomes `acc + (i << 2) + 6`: three nodes for seven, same value
    /// at every input, wrap-around included.
    #[test]
    fn j27_loopcg_an_unrolled_accumulation_becomes_acc_plus_4i_plus_6() {
        let (mut g, ret, acc, i) = two_param_graph();
        let ty = IrType::Int;
        let mut top = bin(&mut g, Op::Add, ty, acc, i);
        for c in 1..=3 {
            let kc = k(&mut g, c, ty);
            let ic = bin(&mut g, Op::Add, ty, i, kc);
            top = bin(&mut g, Op::Add, ty, top, ic);
        }
        g.replace_all_uses(ret, top);
        g.kill(ret);
        let before: Vec<i32> = VALUES
            .iter()
            .flat_map(|&x| VALUES.iter().map(move |&y| (x, y)))
            .map(|(x, y)| eval_int(&g, ret_of(&g), acc, i, x, y))
            .collect();
        assert_eq!(reached_ops(&g, ret_of(&g), acc, i), 7);
        let census = reassociate_linear_trees(&mut g);
        assert_eq!(census.trees, 1, "{census:?}");
        assert_eq!(census.removed, 7);
        assert_eq!(census.added, 3);
        assert_eq!(reached_ops(&g, ret_of(&g), acc, i), 3);
        let after: Vec<i32> = VALUES
            .iter()
            .flat_map(|&x| VALUES.iter().map(move |&y| (x, y)))
            .map(|(x, y)| eval_int(&g, ret_of(&g), acc, i, x, y))
            .collect();
        assert_eq!(before, after);
        let top = &g.nodes[ret_of(&g) as usize];
        assert_eq!(top.op, Op::Add);
        assert_eq!(
            g.nodes[top.inputs[1] as usize].op,
            Op::Const(6),
            "the constant is pulled outermost"
        );
    }

    /// `((x + 3) + 3) + 3) + 3` is `x + 12`.
    #[test]
    fn j27_loopcg_a_constant_chain_folds_to_one_add() {
        let (mut g, ret, x, y) = two_param_graph();
        let ty = IrType::Int;
        let mut top = x;
        for _ in 0..4 {
            let kc = k(&mut g, 3, ty);
            top = bin(&mut g, Op::Add, ty, top, kc);
        }
        g.replace_all_uses(ret, top);
        g.kill(ret);
        let census = reassociate_linear_trees(&mut g);
        assert_eq!(census.trees, 1);
        let r = &g.nodes[ret_of(&g) as usize];
        assert_eq!(r.op, Op::Add);
        assert_eq!(r.inputs[0], x);
        assert_eq!(g.nodes[r.inputs[1] as usize].op, Op::Const(12));
        for &v in &VALUES {
            assert_eq!(eval_int(&g, ret_of(&g), x, y, v, 0), v.wrapping_add(12));
        }
    }

    /// Subtractions, negations, constant multiplies and shifts: the rebuilt
    /// value equals the tree's at every input.
    #[test]
    fn j27_loopcg_mixed_linear_trees_keep_their_value() {
        let (mut g, ret, a, b) = two_param_graph();
        let ty = IrType::Int;
        // t = ((a * 5 - b) + -(a << 3)) - (b * -4) + (7 - a)
        let c5 = k(&mut g, 5, ty);
        let a5 = bin(&mut g, Op::Mul, ty, a, c5);
        let t1 = bin(&mut g, Op::Sub, ty, a5, b);
        let c3 = k(&mut g, 3, IrType::Int);
        let a8 = bin(&mut g, Op::Shl, ty, a, c3);
        let na8 = g.add(Op::Neg, ty, vec![a8], None);
        let t2 = bin(&mut g, Op::Add, ty, t1, na8);
        let cm4 = k(&mut g, -4, ty);
        let bm4 = bin(&mut g, Op::Mul, ty, b, cm4);
        let t3 = bin(&mut g, Op::Sub, ty, t2, bm4);
        let c7 = k(&mut g, 7, ty);
        let sa = bin(&mut g, Op::Sub, ty, c7, a);
        let top = bin(&mut g, Op::Add, ty, t3, sa);
        g.replace_all_uses(ret, top);
        g.kill(ret);
        let mut before = Vec::new();
        for &x in &VALUES {
            for &y in &VALUES {
                before.push(eval_int(&g, ret_of(&g), a, b, x, y));
            }
        }
        // a*(5 - 8 - 1) + b*(-1 + 4) + 7 = -4a + 3b + 7.
        let census = reassociate_linear_trees(&mut g);
        assert_eq!(census.trees, 1, "{census:?}");
        let mut after = Vec::new();
        for &x in &VALUES {
            for &y in &VALUES {
                after.push(eval_int(&g, ret_of(&g), a, b, x, y));
            }
        }
        assert_eq!(before, after);
        assert!(reached_ops(&g, ret_of(&g), a, b) < 9);
    }

    /// An intermediate read twice is a leaf of both readers, and a tree no
    /// smaller rebuilt is left exactly as it was.
    #[test]
    fn j27_loopcg_a_shared_intermediate_and_a_one_node_step_are_left_alone() {
        let (mut g, ret, a, b) = two_param_graph();
        let ty = IrType::Int;
        let c1 = k(&mut g, 1, ty);
        let shared = bin(&mut g, Op::Add, ty, a, c1);
        // top = shared * shared is not linear; each of its operands is the
        // top of a one-node tree `a + 1`, which nothing shrinks.
        let top = bin(&mut g, Op::Mul, ty, shared, shared);
        g.replace_all_uses(ret, top);
        g.kill(ret);
        let census = reassociate_linear_trees(&mut g);
        assert_eq!(census, LinearReassocCensus::default());
        assert_eq!(g.nodes[ret_of(&g) as usize].inputs.as_slice(), &[shared, shared]);
        let _ = b;
    }

    /// A snapshot-named intermediate stays a leaf (and alive): the builder's
    /// own `iadd` is on the operand stack of no later bci here, so name it.
    #[test]
    fn j27_loopcg_a_snapshot_named_intermediate_is_a_leaf() {
        let (mut g, ret, a, b) = two_param_graph();
        let ty = IrType::Int;
        let c1 = k(&mut g, 1, ty);
        let inner = bin(&mut g, Op::Add, ty, a, c1);
        let c2 = k(&mut g, 2, ty);
        let top = bin(&mut g, Op::Add, ty, inner, c2);
        g.replace_all_uses(ret, top);
        g.kill(ret);
        g.push_safepoint(crate::ir::SafepointSnapshot::new(0, vec![inner], vec![], vec![]));
        let census = reassociate_linear_trees(&mut g);
        assert_eq!(census.trees, 0, "{census:?}");
        assert_eq!(g.nodes[inner as usize].op, Op::Add);
        let _ = b;
    }

    /// `long`: the coefficient of `x << 63` wraps to `Long.MIN_VALUE`.
    #[test]
    fn j27_loopcg_long_shift_coefficients_wrap_at_64_bits() {
        let (mut g, ret, a, _b) = two_param_graph();
        let ty = IrType::Long;
        let x = g.add(Op::I2L, ty, vec![a], None);
        let c63 = k(&mut g, 63, IrType::Int);
        let s = bin(&mut g, Op::Shl, ty, x, c63);
        let t = bin(&mut g, Op::Add, ty, s, s);
        let c9 = k(&mut g, 9, ty);
        let top = bin(&mut g, Op::Add, ty, t, c9);
        let l2i = g.add(Op::L2I, IrType::Int, vec![top], None);
        g.replace_all_uses(ret, l2i);
        g.kill(ret);
        // `s` is read twice, so it is a leaf; `t + 9` holds two linear nodes
        // and rebuilds as `(s << 1) + 9`, two nodes: no gain, untouched.
        let census = reassociate_linear_trees(&mut g);
        assert_eq!(census.trees, 0, "{census:?}");
        assert_eq!(classify(&g, s).map(|(l, _)| l), Some(Lin::Scale(x, i64::MIN)));
    }

    #[test]
    fn j27_loopcg_linear_reassoc_is_default_on_with_a_zero_kill() {
        const KEY: &str = "CRATONVM_JIT_IR_LINEAR_REASSOC";
        assert!(cratonvm_types::flags::with_thread_overrides(
            &[(KEY, None)],
            linear_reassoc_enabled
        ));
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[(KEY, Some("0"))],
            linear_reassoc_enabled
        ));
    }
}
