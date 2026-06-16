// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Optimization passes on the Sea-of-Nodes IR graph.
//!
//! Each pass takes `&mut Graph` and transforms it in place.
//! Passes are safe to compose in any order (idempotent).

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use super::ir::{CmpOp, Graph, IrType, Node, NodeId, Op, NO_NODE};

// ── Public API ───────────────────────────────────────────────────────

/// Run all optimization passes on the graph.
pub fn optimize(graph: &mut Graph) {
    // Affine strength-reduction (reassociation). Collapses an unrolled affine
    // recurrence such as `x = x*c1 + c2` (×N) into a single `k*root + c` — the
    // optimization C2 performs via Mul/Add reassociation, which dominated the
    // CratonVM-vs-HotSpot CPU gap on compute kernels. Gated default-OFF behind
    // `CRATONVM_JIT_REASSOC` while it soaks (the IR path it feeds is also
    // gate-relaxed for branchy integer loops in `lib.rs`).
    if reassoc_enabled() {
        reassociate_affine(graph);
    }
    // Run passes in a fixed-point loop until no more changes.
    for _ in 0..8 {
        let before = graph.live_count();
        fold_constants(graph);
        algebraic_simplify(graph);
        gvn(graph);
        eliminate_dead_nodes(graph);
        if graph.live_count() == before {
            break;
        }
    }
}

/// `true` when `CRATONVM_JIT_REASSOC` is set (cached). Enables the affine
/// strength-reduction pass and the matching IR-path gate relaxation.
pub fn reassoc_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_JIT_REASSOC").is_some())
}

// ── Affine strength reduction (reassociation) ────────────────────────
//
// An integer value is "affine in `root`" when it equals `k*root + c` for
// compile-time constants `k`, `c` (with `root == NO_NODE` meaning a pure
// constant `c`). The pass computes this form for every integer Add/Sub/Mul/Neg
// node in one forward pass (SSA data inputs precede their users in id order;
// loop-carried φ back-edges read as opaque, which is conservative), then
// rewrites each chain node into the canonical `k*root + c`. Dead chain nodes
// are removed by the following DCE pass; duplicate const/Mul/Add nodes are
// merged by GVN.
//
// Correctness: Java `int`/`long` arithmetic is two's-complement and wraps mod
// 2^n, where +, -, * are associative, commutative, and distributive, so the
// reassociation is exact. Floating point (non-associative) is never touched.

#[derive(Clone, Copy)]
struct Affine {
    /// Root value this expression is affine in; `NO_NODE` => pure constant.
    root: NodeId,
    k: i64,
    c: i64,
}

#[inline]
fn w_mul(is_int: bool, a: i64, b: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_mul(b as i32)) as i64
    } else {
        a.wrapping_mul(b)
    }
}
#[inline]
fn w_add(is_int: bool, a: i64, b: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_add(b as i32)) as i64
    } else {
        a.wrapping_add(b)
    }
}
#[inline]
fn w_sub(is_int: bool, a: i64, b: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_sub(b as i32)) as i64
    } else {
        a.wrapping_sub(b)
    }
}
#[inline]
fn w_neg(is_int: bool, a: i64) -> i64 {
    if is_int {
        ((a as i32).wrapping_neg()) as i64
    } else {
        a.wrapping_neg()
    }
}

/// `Some(true)` for `int`, `Some(false)` for `long`, `None` otherwise.
fn int_width(ty: IrType) -> Option<bool> {
    match ty {
        IrType::Int => Some(true),
        IrType::Long => Some(false),
        _ => None,
    }
}

fn is_int_arith(op: &Op) -> bool {
    matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Neg)
}

/// Combine the affine forms of a binary op's two operands.
fn combine_affine(op: &Op, is_int: bool, a: Affine, b: Affine, opaque: Affine) -> Affine {
    match op {
        Op::Add => {
            if a.root == NO_NODE {
                Affine { root: b.root, k: b.k, c: w_add(is_int, b.c, a.c) }
            } else if b.root == NO_NODE {
                Affine { root: a.root, k: a.k, c: w_add(is_int, a.c, b.c) }
            } else if a.root == b.root {
                Affine { root: a.root, k: w_add(is_int, a.k, b.k), c: w_add(is_int, a.c, b.c) }
            } else {
                opaque
            }
        }
        Op::Sub => {
            if b.root == NO_NODE {
                Affine { root: a.root, k: a.k, c: w_sub(is_int, a.c, b.c) }
            } else if a.root == NO_NODE {
                Affine { root: b.root, k: w_neg(is_int, b.k), c: w_sub(is_int, a.c, b.c) }
            } else if a.root == b.root {
                Affine { root: a.root, k: w_sub(is_int, a.k, b.k), c: w_sub(is_int, a.c, b.c) }
            } else {
                opaque
            }
        }
        Op::Mul => {
            if a.root == NO_NODE {
                if b.root == NO_NODE {
                    Affine { root: NO_NODE, k: 0, c: w_mul(is_int, a.c, b.c) }
                } else {
                    Affine { root: b.root, k: w_mul(is_int, b.k, a.c), c: w_mul(is_int, b.c, a.c) }
                }
            } else if b.root == NO_NODE {
                Affine { root: a.root, k: w_mul(is_int, a.k, b.c), c: w_mul(is_int, a.c, b.c) }
            } else {
                opaque
            }
        }
        _ => opaque,
    }
}

/// Get an existing `Const(val)` node of type `ty`, or create one.
fn get_or_add_const(graph: &mut Graph, val: i64, ty: IrType) -> NodeId {
    if let Some(i) = graph
        .nodes
        .iter()
        .position(|n| n.op == Op::Const(val) && n.ty == ty)
    {
        return i as NodeId;
    }
    graph.add(Op::Const(val), ty, vec![], None)
}

/// Materialize `k*root + c` as IR nodes, reusing `root` directly when possible.
fn build_affine(graph: &mut Graph, root: NodeId, k: i64, c: i64, ty: IrType) -> NodeId {
    if k == 0 {
        return get_or_add_const(graph, c, ty);
    }
    let base = if k == 1 {
        root
    } else {
        let kc = get_or_add_const(graph, k, ty);
        graph.add(Op::Mul, ty, vec![root, kc], None)
    };
    if c == 0 {
        base
    } else {
        let cc = get_or_add_const(graph, c, ty);
        graph.add(Op::Add, ty, vec![base, cc], None)
    }
}

fn reassociate_affine(graph: &mut Graph) {
    let len = graph.nodes.len();
    let mut aff: Vec<Option<Affine>> = vec![None; len];
    // `true` for nodes we will rewrite: an integer arith node that is affine in
    // a single real root AND has an operand that is itself an affine arith node
    // over the same root (i.e. a genuine chain to collapse). Decided in the
    // forward pass from *original* inputs so later input-rewrites cannot
    // perturb the decision.
    let mut materialize: Vec<bool> = vec![false; len];

    for id in 0..len {
        let node = &graph.nodes[id];
        if node.op == Op::Dead {
            continue;
        }
        let opaque = Affine { root: id as NodeId, k: 1, c: 0 };
        let is_int = match int_width(node.ty) {
            Some(w) => w,
            None => {
                aff[id] = Some(opaque);
                continue;
            }
        };
        // Read an operand's already-computed affine form; only inputs with a
        // strictly-lower id are available (φ back-edges read as None → opaque).
        let geta = |slot: usize| -> Option<Affine> {
            node.inputs
                .get(slot)
                .and_then(|&i| if (i as usize) < id { aff[i as usize] } else { None })
        };
        let res = match &node.op {
            Op::Const(v) => Affine {
                root: NO_NODE,
                k: 0,
                c: if is_int { (*v as i32) as i64 } else { *v },
            },
            Op::Neg => match geta(0) {
                Some(a) => Affine { root: a.root, k: w_neg(is_int, a.k), c: w_neg(is_int, a.c) },
                None => opaque,
            },
            Op::Add | Op::Sub | Op::Mul => match (geta(0), geta(1)) {
                (Some(a), Some(b)) => combine_affine(&node.op, is_int, a, b, opaque),
                _ => opaque,
            },
            _ => opaque,
        };
        // Decide materialization while original inputs are intact.
        if is_int_arith(&node.op) && res.root != NO_NODE && res.root != id as NodeId {
            let has_arith_operand = node.inputs.iter().any(|&j| {
                (j as usize) < id
                    && aff[j as usize].is_some_and(|fj| fj.root == res.root && fj.root != NO_NODE)
                    && is_int_arith(&graph.nodes[j as usize].op)
            });
            materialize[id] = has_arith_operand;
        }
        aff[id] = Some(res);
    }

    for id in 0..len {
        if !materialize[id] {
            continue;
        }
        let ty = graph.nodes[id].ty;
        let a = match aff[id] {
            Some(a) => a,
            None => continue,
        };
        let mat = build_affine(graph, a.root, a.k, a.c, ty);
        if mat != id as NodeId {
            graph.replace_all_uses(id as NodeId, mat);
            graph.kill(id as NodeId);
        }
    }
}

// ── Constant Folding ─────────────────────────────────────────────────

/// Evaluate nodes whose inputs are all constants.
fn fold_constants(graph: &mut Graph) {
    let len = graph.nodes.len();
    for id in 0..len {
        if graph.nodes[id].op == Op::Dead {
            continue;
        }
        if let Some(folded) = try_fold(&graph.nodes, id as NodeId) {
            let ty = graph.nodes[id].ty;
            let pc = graph.nodes[id].bytecode_pc;
            // Replace with constant
            graph.nodes[id].op = Op::Const(folded);
            graph.nodes[id].ty = ty;
            graph.nodes[id].inputs.clear();
            graph.nodes[id].bytecode_pc = pc;
        }
    }
}

fn try_fold(nodes: &[Node], id: NodeId) -> Option<i64> {
    let node = &nodes[id as usize];
    let is_int = node.ty == IrType::Int;
    match &node.op {
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::And | Op::Or | Op::Xor
        | Op::Shl | Op::Shr | Op::UShr => {
            let a = const_value(nodes, node.inputs[0])?;
            let b = const_value(nodes, node.inputs[1])?;
            let raw = match &node.op {
                Op::Add => {
                    if is_int {
                        ((a as i32).wrapping_add(b as i32)) as i64
                    } else {
                        a.wrapping_add(b)
                    }
                }
                Op::Sub => {
                    if is_int {
                        ((a as i32).wrapping_sub(b as i32)) as i64
                    } else {
                        a.wrapping_sub(b)
                    }
                }
                Op::Mul => {
                    if is_int {
                        ((a as i32).wrapping_mul(b as i32)) as i64
                    } else {
                        a.wrapping_mul(b)
                    }
                }
                Op::Div => {
                    if is_int {
                        let bi = b as i32;
                        if bi == 0 {
                            return None;
                        }
                        let ai = a as i32;
                        // Java semantics: INT_MIN / -1 == INT_MIN (no exception).
                        // i32::wrapping_div handles this correctly.
                        if ai == i32::MIN && bi == -1 {
                            i32::MIN as i64
                        } else {
                            ai.wrapping_div(bi) as i64
                        }
                    } else {
                        if b == 0 {
                            return None;
                        }
                        // Java semantics: LONG_MIN / -1 == LONG_MIN (no exception).
                        if a == i64::MIN && b == -1 {
                            i64::MIN
                        } else {
                            a.wrapping_div(b)
                        }
                    }
                }
                Op::Rem => {
                    if is_int {
                        let bi = b as i32;
                        if bi == 0 {
                            return None;
                        }
                        let ai = a as i32;
                        // Java semantics: INT_MIN % -1 == 0 (no exception).
                        if ai == i32::MIN && bi == -1 {
                            0
                        } else {
                            ai.wrapping_rem(bi) as i64
                        }
                    } else {
                        if b == 0 {
                            return None;
                        }
                        // Java semantics: LONG_MIN % -1 == 0 (no exception).
                        if a == i64::MIN && b == -1 {
                            0
                        } else {
                            a.wrapping_rem(b)
                        }
                    }
                }
                Op::And => a & b,
                Op::Or => a | b,
                Op::Xor => a ^ b,
                Op::Shl => {
                    if is_int {
                        // Java: only low 5 bits of shift count for int.
                        ((a as i32).wrapping_shl((b as u32) & 0x1f)) as i64
                    } else {
                        // Java: only low 6 bits of shift count for long.
                        a.wrapping_shl((b as u32) & 0x3f)
                    }
                }
                Op::Shr => {
                    if is_int {
                        ((a as i32).wrapping_shr((b as u32) & 0x1f)) as i64
                    } else {
                        a.wrapping_shr((b as u32) & 0x3f)
                    }
                }
                Op::UShr => {
                    if is_int {
                        ((a as u32).wrapping_shr((b as u32) & 0x1f)) as i64
                    } else {
                        (a as u64).wrapping_shr((b as u32) & 0x3f) as i64
                    }
                }
                _ => unreachable!(),
            };
            // Defensive truncation: for i32 ops, ensure the high 32 bits are
            // a sign-extension of the low 32 bits. (Most paths above already
            // satisfy this, but truncating once at the end makes the
            // invariant robust to future edits.)
            let result = if is_int { (raw as i32) as i64 } else { raw };
            Some(result)
        }
        Op::Neg => {
            let a = const_value(nodes, node.inputs[0])?;
            let raw = if is_int {
                ((a as i32).wrapping_neg()) as i64
            } else {
                a.wrapping_neg()
            };
            let result = if is_int { (raw as i32) as i64 } else { raw };
            Some(result)
        }
        Op::Cmp(cc) => {
            let a = const_value(nodes, node.inputs[0])?;
            let b = const_value(nodes, node.inputs[1])?;
            // Compare in the proper width so that two i32-typed constants
            // whose i64 sign-extensions agree compare identically.
            let result = if is_int {
                let ai = a as i32;
                let bi = b as i32;
                match cc {
                    CmpOp::Eq => ai == bi,
                    CmpOp::Ne => ai != bi,
                    CmpOp::Lt => ai < bi,
                    CmpOp::Le => ai <= bi,
                    CmpOp::Gt => ai > bi,
                    CmpOp::Ge => ai >= bi,
                }
            } else {
                match cc {
                    CmpOp::Eq => a == b,
                    CmpOp::Ne => a != b,
                    CmpOp::Lt => a < b,
                    CmpOp::Le => a <= b,
                    CmpOp::Gt => a > b,
                    CmpOp::Ge => a >= b,
                }
            };
            Some(if result { 1 } else { 0 })
        }
        _ => None,
    }
}

fn const_value(nodes: &[Node], id: NodeId) -> Option<i64> {
    if let Op::Const(v) = nodes[id as usize].op {
        Some(v)
    } else {
        None
    }
}

// ── Algebraic Simplification ─────────────────────────────────────────

/// Simplify expressions using algebraic identities.
fn algebraic_simplify(graph: &mut Graph) {
    // Ensure a zero constant exists for identity replacements.
    let zero = find_const(&graph.nodes, 0)
        .unwrap_or_else(|| graph.add(Op::Const(0), IrType::Int, vec![], None));
    let len = graph.nodes.len();
    for id in 0..len {
        if graph.nodes[id].op == Op::Dead {
            continue;
        }
        if let Some(replacement) = try_simplify(&graph.nodes, id as NodeId, zero) {
            graph.replace_all_uses(id as NodeId, replacement);
            graph.kill(id as NodeId);
        }
    }
}

fn try_simplify(nodes: &[Node], id: NodeId, zero: NodeId) -> Option<NodeId> {
    let node = &nodes[id as usize];
    let inputs = &node.inputs;
    match &node.op {
        // x + 0 → x
        Op::Add => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(inputs[0]);
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(inputs[1]);
            }
            None
        }
        // x - 0 → x, x - x → 0
        Op::Sub => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(inputs[0]);
            }
            if inputs[0] == inputs[1] {
                return Some(zero);
            }
            None
        }
        // x * 1 → x, x * 0 → 0
        Op::Mul => {
            if is_const_val(nodes, inputs[1], 1) {
                return Some(inputs[0]);
            }
            if is_const_val(nodes, inputs[0], 1) {
                return Some(inputs[1]);
            }
            if is_const_val(nodes, inputs[1], 0) {
                return Some(inputs[1]); // the zero const
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(inputs[0]); // the zero const
            }
            None
        }
        // x & 0 → 0, x & -1 → x
        Op::And => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(inputs[1]);
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(inputs[0]);
            }
            if is_const_val(nodes, inputs[1], -1) {
                return Some(inputs[0]);
            }
            if is_const_val(nodes, inputs[0], -1) {
                return Some(inputs[1]);
            }
            None
        }
        // x | 0 → x
        Op::Or => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(inputs[0]);
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(inputs[1]);
            }
            None
        }
        // x ^ 0 → x, x ^ x → 0
        Op::Xor => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(inputs[0]);
            }
            if is_const_val(nodes, inputs[0], 0) {
                return Some(inputs[1]);
            }
            if inputs[0] == inputs[1] {
                return Some(zero);
            }
            None
        }
        // x << 0 → x, x >> 0 → x, x >>> 0 → x
        Op::Shl | Op::Shr | Op::UShr => {
            if is_const_val(nodes, inputs[1], 0) {
                return Some(inputs[0]);
            }
            None
        }
        _ => None,
    }
}

fn is_const_val(nodes: &[Node], id: NodeId, val: i64) -> bool {
    matches!(nodes[id as usize].op, Op::Const(v) if v == val)
}

fn find_const(nodes: &[Node], val: i64) -> Option<NodeId> {
    nodes
        .iter()
        .position(|n| n.op == Op::Const(val))
        .map(|i| i as NodeId)
}

// ── Global Value Numbering (GVN) ─────────────────────────────────────

/// Deduplicate nodes that compute the same value.
///
/// Uses a pre-computed hash of (op, ty, inputs) as the map key to avoid
/// cloning the inputs Vec on every lookup.
fn gvn(graph: &mut Graph) {
    // Hot-path: use FxHashMap (rustc_hash) instead of the std SipHash-backed
    // HashMap. The keys are 64-bit value-number hashes (no adversarial input),
    // so the faster FxHasher gives ≈2x lookup throughput with the same
    // collision behavior in practice.
    let mut value_map: FxHashMap<u64, NodeId> = FxHashMap::default();
    let len = graph.nodes.len();
    for id in 0..len {
        let node = &graph.nodes[id];
        if node.op == Op::Dead || !node.op.is_pure() {
            continue;
        }
        let hash = gvn_hash(&node.op, node.ty, &node.inputs);
        if let Some(&existing) = value_map.get(&hash) {
            // Verify it is actually the same node (hash collision check).
            let existing_node = &graph.nodes[existing as usize];
            if existing != id as NodeId
                && existing_node.op == node.op
                && existing_node.ty == node.ty
                && existing_node.inputs == node.inputs
            {
                graph.replace_all_uses(id as NodeId, existing);
                graph.kill(id as NodeId);
            }
        } else {
            value_map.insert(hash, id as NodeId);
        }
    }
}

/// Compute a hash of (op, ty, inputs) for GVN without cloning the inputs Vec.
///
/// Uses `FxHasher` rather than the std default (SipHash) — GVN runs on every
/// JIT compile and the values are non-adversarial node identifiers, so the
/// 2-3x faster Fx hash is a strict win.
fn gvn_hash(op: &Op, ty: IrType, inputs: &[NodeId]) -> u64 {
    let mut hasher = FxHasher::default();
    op.hash(&mut hasher);
    ty.hash(&mut hasher);
    inputs.hash(&mut hasher);
    hasher.finish()
}

// ── Dead Node Elimination ────────────────────────────────────────────

/// Remove nodes not reachable from the exit (Return).
fn eliminate_dead_nodes(graph: &mut Graph) {
    if graph.exit == NO_NODE {
        return;
    }

    let mut reachable: FxHashSet<NodeId> = FxHashSet::default();
    let mut worklist = vec![graph.exit];

    // Also keep all control nodes reachable from exit
    while let Some(id) = worklist.pop() {
        if !reachable.insert(id) {
            continue;
        }
        let node = &graph.nodes[id as usize];
        for &inp in &node.inputs {
            if inp != NO_NODE && (inp as usize) < graph.nodes.len() {
                worklist.push(inp);
            }
        }
    }

    // Kill unreachable nodes (except Start which is always kept)
    for id in 0..graph.nodes.len() {
        if !reachable.contains(&(id as NodeId)) && graph.nodes[id].op != Op::Dead {
            // Keep Start alive always
            if id as NodeId == graph.entry {
                continue;
            }
            graph.kill(id as NodeId);
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::IrBuilder;

    fn build_and_optimize(code: &[u8], code_len: usize, num_params: usize, num_locals: usize) -> Graph {
        let builder = IrBuilder::new(num_params, num_locals);
        let mut graph = builder.build(code, code_len).expect("build failed");
        optimize(&mut graph);
        graph
    }

    #[test]
    fn test_constant_folding_add() {
        // return 3 + 4  →  should fold to return 7
        let code = [0x06, 0x07, 0x60, 0xac, 0, 0]; // iconst_3; iconst_4; iadd; ireturn
        let graph = build_and_optimize(&code, 4, 0, 0);
        let ret = &graph.nodes[graph.exit as usize];
        let val_id = ret.inputs[1];
        // After folding, the return value should be Const(7)
        assert_eq!(graph.nodes[val_id as usize].op, Op::Const(7));
    }

    #[test]
    fn test_constant_folding_mul() {
        // return 5 * 6  →  should fold to 30
        let code = [0x08, 0x06, 0x68, 0xac, 0, 0]; // iconst_5; iconst_3; imul; ireturn
        let graph = build_and_optimize(&code, 4, 0, 0);
        let ret = &graph.nodes[graph.exit as usize];
        let val_id = ret.inputs[1];
        assert_eq!(graph.nodes[val_id as usize].op, Op::Const(15));
    }

    #[test]
    fn test_algebraic_add_zero() {
        // return x + 0  →  should simplify to return x
        // iload_0; iconst_0; iadd; ireturn
        let code = [0x1a, 0x03, 0x60, 0xac, 0, 0];
        let graph = build_and_optimize(&code, 4, 1, 1);
        let ret = &graph.nodes[graph.exit as usize];
        let val_id = ret.inputs[1];
        // After simplification, return value should be Param(0) directly
        assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
    }

    #[test]
    fn test_algebraic_mul_one() {
        // return x * 1  →  should simplify to return x
        // iload_0; iconst_1; imul; ireturn
        let code = [0x1a, 0x04, 0x68, 0xac, 0, 0];
        let graph = build_and_optimize(&code, 4, 1, 1);
        let ret = &graph.nodes[graph.exit as usize];
        let val_id = ret.inputs[1];
        assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
    }

    #[test]
    fn test_algebraic_sub_self() {
        // return x - x  →  should simplify to return 0
        // iload_0; iload_0; isub; ireturn
        let code = [0x1a, 0x1a, 0x64, 0xac, 0, 0];
        let graph = build_and_optimize(&code, 4, 1, 1);
        let ret = &graph.nodes[graph.exit as usize];
        let val_id = ret.inputs[1];
        assert_eq!(graph.nodes[val_id as usize].op, Op::Const(0));
    }

    #[test]
    fn test_gvn_deduplicates() {
        // return (x + y) + (x + y) → should GVN the two (x+y) computations
        // iload_0; iload_1; iadd; iload_0; iload_1; iadd; iadd; ireturn
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x1b, 0x60, 0x60, 0xac, 0, 0];
        let graph = build_and_optimize(&code, 8, 2, 2);
        // Count Add nodes — should be 2 (one for x+y, one for (x+y)+(x+y))
        // not 3 (the duplicate x+y should be eliminated)
        let add_count = graph.nodes.iter().filter(|n| n.op == Op::Add).count();
        assert_eq!(add_count, 2, "GVN should deduplicate the duplicate x+y");
    }

    #[test]
    fn test_dead_node_elimination() {
        // int f(int x) { int y = x + 1; return x; }
        // iload_0; iconst_1; iadd; istore_1; iload_0; ireturn
        let code = [0x1a, 0x04, 0x60, 0x3c, 0x1a, 0xac, 0, 0];
        let graph = build_and_optimize(&code, 6, 1, 2);
        // The x+1 computation is dead (result stored but never used in return)
        // After DCE, the Add node should be killed
        let ret = &graph.nodes[graph.exit as usize];
        let val_id = ret.inputs[1];
        assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
    }

    #[test]
    fn test_chained_constant_folding() {
        // return (2 + 3) * 4  →  should fold to 20
        // iconst_2; iconst_3; iadd; iconst_4; imul; ireturn
        let code = [0x05, 0x06, 0x60, 0x07, 0x68, 0xac, 0, 0];
        let graph = build_and_optimize(&code, 6, 0, 0);
        let ret = &graph.nodes[graph.exit as usize];
        let val_id = ret.inputs[1];
        assert_eq!(graph.nodes[val_id as usize].op, Op::Const(20));
    }

    // ── Affine strength reduction (reassociation) ───────────────────

    fn build_and_reassociate(code: &[u8], code_len: usize, num_params: usize, num_locals: usize) -> Graph {
        let builder = IrBuilder::new(num_params, num_locals);
        let mut graph = builder.build(code, code_len).expect("build failed");
        reassociate_affine(&mut graph);
        // Clean-up passes that normally follow in `optimize`.
        for _ in 0..8 {
            let before = graph.live_count();
            fold_constants(&mut graph);
            algebraic_simplify(&mut graph);
            gvn(&mut graph);
            eliminate_dead_nodes(&mut graph);
            if graph.live_count() == before {
                break;
            }
        }
        graph
    }

    #[test]
    fn test_reassoc_affine_chain_folds() {
        // int f(int x){ x = x*3+5; x = x*2+1; return x; }  →  x*6 + 11
        // iload_0; iconst_3; imul; iconst_5; iadd; iconst_2; imul; iconst_1; iadd; ireturn
        let code = [
            0x1a, 0x06, 0x68, 0x08, 0x60, 0x05, 0x68, 0x04, 0x60, 0xac, 0, 0,
        ];
        let graph = build_and_reassociate(&code, 10, 1, 1);
        let ret = &graph.nodes[graph.exit as usize];
        let val = &graph.nodes[ret.inputs[1] as usize];
        assert_eq!(val.op, Op::Add, "top of folded chain should be Add(k*x, c)");
        // One input is the constant c=11, the other is Mul(x, 6).
        let (mul_id, c_id) = if graph.nodes[val.inputs[0] as usize].op == Op::Mul {
            (val.inputs[0], val.inputs[1])
        } else {
            (val.inputs[1], val.inputs[0])
        };
        assert_eq!(graph.nodes[c_id as usize].op, Op::Const(11), "additive constant");
        let mul = &graph.nodes[mul_id as usize];
        assert_eq!(mul.op, Op::Mul);
        let (px, k_id) = if graph.nodes[mul.inputs[0] as usize].op == Op::Param(0) {
            (mul.inputs[0], mul.inputs[1])
        } else {
            (mul.inputs[1], mul.inputs[0])
        };
        assert_eq!(graph.nodes[px as usize].op, Op::Param(0));
        assert_eq!(graph.nodes[k_id as usize].op, Op::Const(6), "multiplicative constant");
        // The whole intermediate chain collapsed: exactly one Mul + one Add remain.
        assert_eq!(graph.nodes.iter().filter(|n| n.op == Op::Mul).count(), 1);
        assert_eq!(graph.nodes.iter().filter(|n| n.op == Op::Add).count(), 1);
    }

    #[test]
    fn test_reassoc_combines_muls() {
        // x = x*181; x = x*181;  →  x*32761  (181*181, a single Mul)
        // iload_0; sipush 181; imul; sipush 181; imul; ireturn
        let code = [
            0x1a, 0x11, 0x00, 0xb5, 0x68, 0x11, 0x00, 0xb5, 0x68, 0xac, 0, 0,
        ];
        let graph = build_and_reassociate(&code, 10, 1, 1);
        let ret = &graph.nodes[graph.exit as usize];
        let val = &graph.nodes[ret.inputs[1] as usize];
        assert_eq!(val.op, Op::Mul);
        let k_id = if graph.nodes[val.inputs[0] as usize].op == Op::Param(0) {
            val.inputs[1]
        } else {
            val.inputs[0]
        };
        assert_eq!(graph.nodes[k_id as usize].op, Op::Const(32761), "181*181 folds to 32761");
        assert_eq!(graph.nodes.iter().filter(|n| n.op == Op::Mul).count(), 1);
    }

    #[test]
    fn test_reassoc_leaves_nonlinear_alone() {
        // x*x is not affine (variable * variable) — must not be folded.
        // iload_0; iload_0; imul; ireturn
        let code = [0x1a, 0x1a, 0x68, 0xac, 0, 0];
        let graph = build_and_reassociate(&code, 4, 1, 1);
        let ret = &graph.nodes[graph.exit as usize];
        let val = &graph.nodes[ret.inputs[1] as usize];
        assert_eq!(val.op, Op::Mul);
        assert_eq!(graph.nodes[val.inputs[0] as usize].op, Op::Param(0));
        assert_eq!(graph.nodes[val.inputs[1] as usize].op, Op::Param(0));
    }

    // ── Unit tests for try_fold i32-truncation correctness ──────────

    /// Build a 3-node graph: Const(a), Const(b), Op(a, b) of type `ty`.
    /// Returns the binary node's id and the node arena, ready to pass to try_fold.
    fn fold_binop(ty: IrType, op: Op, a: i64, b: i64) -> Option<i64> {
        let mut nodes: Vec<Node> = Vec::new();
        nodes.push(Node {
            op: Op::Const(a),
            ty,
            inputs: vec![],
            bytecode_pc: None,
        });
        nodes.push(Node {
            op: Op::Const(b),
            ty,
            inputs: vec![],
            bytecode_pc: None,
        });
        nodes.push(Node {
            op,
            ty,
            inputs: vec![0, 1],
            bytecode_pc: None,
        });
        try_fold(&nodes, 2)
    }

    fn fold_unop(ty: IrType, op: Op, a: i64) -> Option<i64> {
        let mut nodes: Vec<Node> = Vec::new();
        nodes.push(Node {
            op: Op::Const(a),
            ty,
            inputs: vec![],
            bytecode_pc: None,
        });
        nodes.push(Node {
            op,
            ty,
            inputs: vec![0],
            bytecode_pc: None,
        });
        try_fold(&nodes, 1)
    }

    #[test]
    fn test_fold_i32_add_overflow_wraps() {
        // Java: Integer.MIN_VALUE + (-1) == Integer.MAX_VALUE (0x7FFF_FFFF)
        let r = fold_binop(IrType::Int, Op::Add, i32::MIN as i64, -1).unwrap();
        assert_eq!(r, i32::MAX as i64, "INT_MIN + (-1) must wrap to INT_MAX, got {:#x}", r);
        // High 32 bits must be sign-extension of low 32 bits (i.e. 0 here).
        assert_eq!(r as i32 as i64, r, "result must be a clean i32 sign-extension");
    }

    #[test]
    fn test_fold_i32_sub_overflow_wraps() {
        // Java: Integer.MIN_VALUE - 1 == Integer.MAX_VALUE
        let r = fold_binop(IrType::Int, Op::Sub, i32::MIN as i64, 1).unwrap();
        assert_eq!(r, i32::MAX as i64);
    }

    #[test]
    fn test_fold_i32_mul_overflow_wraps() {
        // Java: 0x10000 * 0x10000 (as int) == 0 (low 32 bits of 2^32)
        let r = fold_binop(IrType::Int, Op::Mul, 0x10000, 0x10000).unwrap();
        assert_eq!(r, 0);
    }

    #[test]
    fn test_fold_i32_div_intmin_neg1() {
        // Java: Integer.MIN_VALUE / -1 == Integer.MIN_VALUE (no exception)
        let r = fold_binop(IrType::Int, Op::Div, i32::MIN as i64, -1).unwrap();
        assert_eq!(r, i32::MIN as i64);
    }

    #[test]
    fn test_fold_i32_rem_intmin_neg1() {
        // Java: Integer.MIN_VALUE % -1 == 0 (no exception)
        let r = fold_binop(IrType::Int, Op::Rem, i32::MIN as i64, -1).unwrap();
        assert_eq!(r, 0);
    }

    #[test]
    fn test_fold_i32_div_by_zero_returns_none() {
        // Cannot fold; runtime must throw ArithmeticException.
        assert_eq!(fold_binop(IrType::Int, Op::Div, 5, 0), None);
        assert_eq!(fold_binop(IrType::Int, Op::Rem, 5, 0), None);
    }

    #[test]
    fn test_fold_i64_div_longmin_neg1() {
        // Java: Long.MIN_VALUE / -1 == Long.MIN_VALUE (no exception)
        let r = fold_binop(IrType::Long, Op::Div, i64::MIN, -1).unwrap();
        assert_eq!(r, i64::MIN);
        let r = fold_binop(IrType::Long, Op::Rem, i64::MIN, -1).unwrap();
        assert_eq!(r, 0);
    }

    #[test]
    fn test_fold_i64_div_by_zero_returns_none() {
        assert_eq!(fold_binop(IrType::Long, Op::Div, 5, 0), None);
        assert_eq!(fold_binop(IrType::Long, Op::Rem, 5, 0), None);
    }

    #[test]
    fn test_fold_i32_shl_masks_to_5_bits() {
        // Java: 1 << 33 == 1 << 1 == 2 (shift count masked to low 5 bits)
        let r = fold_binop(IrType::Int, Op::Shl, 1, 33).unwrap();
        assert_eq!(r, 2);
        // 1 << 31 == INT_MIN (sign-extended to i64)
        let r = fold_binop(IrType::Int, Op::Shl, 1, 31).unwrap();
        assert_eq!(r, i32::MIN as i64);
    }

    #[test]
    fn test_fold_i64_shl_masks_to_6_bits() {
        // Java: 1L << 65 == 1L << 1 == 2
        let r = fold_binop(IrType::Long, Op::Shl, 1, 65).unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_fold_i32_ushr_does_not_leak_high_bits() {
        // Java: (-1) >>> 1 == 0x7FFF_FFFF (as int), sign-extended → 0x7FFF_FFFF
        let r = fold_binop(IrType::Int, Op::UShr, -1i64, 1).unwrap();
        assert_eq!(r, 0x7FFF_FFFFi64);
    }

    #[test]
    fn test_fold_i32_and_truncates() {
        // If either operand somehow had stale high bits, AND result must
        // still be a clean i32 sign-extension.
        let r = fold_binop(IrType::Int, Op::And, 0xFF, 0x0F).unwrap();
        assert_eq!(r, 0x0F);
        assert_eq!(r as i32 as i64, r);
    }

    #[test]
    fn test_fold_i32_neg_intmin() {
        // Java: -Integer.MIN_VALUE == Integer.MIN_VALUE (wraps)
        let r = fold_unop(IrType::Int, Op::Neg, i32::MIN as i64).unwrap();
        assert_eq!(r, i32::MIN as i64);
    }

    #[test]
    fn test_fold_i64_neg_longmin() {
        let r = fold_unop(IrType::Long, Op::Neg, i64::MIN).unwrap();
        assert_eq!(r, i64::MIN);
    }
}
