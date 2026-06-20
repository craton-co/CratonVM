// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Optimization passes on the Sea-of-Nodes IR graph.
//!
//! Each pass takes `&mut Graph` and transforms it in place.
//! Passes are safe to compose in any order (idempotent).

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use super::ir::{CmpOp, Graph, IrType, MemKind, Node, NodeId, Op, NO_NODE};

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
        eliminate_dead_stores(graph);
        eliminate_dead_nodes(graph);
        if graph.live_count() == before {
            break;
        }
    }
    // Full unrolling of small constant-trip counted loops. Default-OFF behind
    // `CRATONVM_JIT_UNROLL` (Increment 3 of activate-ir-optimizer). Runs after
    // the fixed-point cleanup so init/stride/bound are already folded to
    // constants; re-runs the cleanup so the unrolled straight-line code folds
    // (the concrete induction values collapse the per-iteration computation)
    // and the retired loop nodes are reclaimed.
    if unroll_enabled() && unroll(graph) {
        for _ in 0..8 {
            let before = graph.live_count();
            fold_constants(graph);
            algebraic_simplify(graph);
            gvn(graph);
            eliminate_dead_stores(graph);
            eliminate_dead_nodes(graph);
            if graph.live_count() == before {
                break;
            }
        }
    }
    // SCEV-driven loop-invariant code motion. Default-OFF behind
    // `CRATONVM_JIT_LICM` while it soaks (Increment 2 of activate-ir-optimizer).
    // Runs once *after* the fixed-point cleanup so it sees already-folded /
    // GVN'd invariant expressions, then a final lightweight cleanup re-runs
    // GVN + DCE to dedup any anchor edges it rewrote.
    if licm_enabled() && licm(graph) {
        gvn(graph);
        eliminate_dead_nodes(graph);
    }
}

/// `true` when `CRATONVM_JIT_LICM` is set (cached). Enables the SCEV-driven
/// loop-invariant code-motion pass. Default-OFF: the pass is the experimental
/// Increment-2 slice and must soak against the differential gauntlet before
/// the default flips.
pub fn licm_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_JIT_LICM").is_some())
}

/// `true` when `CRATONVM_JIT_REASSOC` is set (cached). Enables the affine
/// strength-reduction pass and the matching IR-path gate relaxation.
pub fn reassoc_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_JIT_REASSOC").is_some())
}

/// `true` (default) when pure, call-free *branchy* integer methods may take the
/// IR pipeline. The φ/branch SIGSEGV (missing SETcc ModRM byte) and the
/// loop-carried-phi miscompile that originally gated these methods are fixed,
/// so the IR path now compiles if/else and loops correctly. `CRATONVM_NO_IR_BRANCHY`
/// is the emergency opt-out that restores the single-pass-only routing for the
/// branchy shape (e.g. to bisect a suspected IR-path regression).
pub fn ir_branchy_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_NO_IR_BRANCHY").is_none())
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
                Affine {
                    root: b.root,
                    k: b.k,
                    c: w_add(is_int, b.c, a.c),
                }
            } else if b.root == NO_NODE {
                Affine {
                    root: a.root,
                    k: a.k,
                    c: w_add(is_int, a.c, b.c),
                }
            } else if a.root == b.root {
                Affine {
                    root: a.root,
                    k: w_add(is_int, a.k, b.k),
                    c: w_add(is_int, a.c, b.c),
                }
            } else {
                opaque
            }
        }
        Op::Sub => {
            if b.root == NO_NODE {
                Affine {
                    root: a.root,
                    k: a.k,
                    c: w_sub(is_int, a.c, b.c),
                }
            } else if a.root == NO_NODE {
                Affine {
                    root: b.root,
                    k: w_neg(is_int, b.k),
                    c: w_sub(is_int, a.c, b.c),
                }
            } else if a.root == b.root {
                Affine {
                    root: a.root,
                    k: w_sub(is_int, a.k, b.k),
                    c: w_sub(is_int, a.c, b.c),
                }
            } else {
                opaque
            }
        }
        Op::Mul => {
            if a.root == NO_NODE {
                if b.root == NO_NODE {
                    Affine {
                        root: NO_NODE,
                        k: 0,
                        c: w_mul(is_int, a.c, b.c),
                    }
                } else {
                    Affine {
                        root: b.root,
                        k: w_mul(is_int, b.k, a.c),
                        c: w_mul(is_int, b.c, a.c),
                    }
                }
            } else if b.root == NO_NODE {
                Affine {
                    root: a.root,
                    k: w_mul(is_int, a.k, b.c),
                    c: w_mul(is_int, a.c, b.c),
                }
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
        let opaque = Affine {
            root: id as NodeId,
            k: 1,
            c: 0,
        };
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
            node.inputs.get(slot).and_then(|&i| {
                if (i as usize) < id {
                    aff[i as usize]
                } else {
                    None
                }
            })
        };
        let res = match &node.op {
            Op::Const(v) => Affine {
                root: NO_NODE,
                k: 0,
                c: if is_int { (*v as i32) as i64 } else { *v },
            },
            Op::Neg => match geta(0) {
                Some(a) => Affine {
                    root: a.root,
                    k: w_neg(is_int, a.k),
                    c: w_neg(is_int, a.c),
                },
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
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr => {
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

// ── SCEV-driven Loop-Invariant Code Motion (LICM) ────────────────────
//
// Hoists provably loop-invariant *pure* computations and *invariant loads*
// out of a loop into its pre-header.  Increment 2 of activate-ir-optimizer.
//
// RELATIONSHIP TO `scev` / `loop_analysis`:
//
//   `scev::analyze_induction_variables` / `InductionVar::trip_count` and
//   `loop_analysis::detect_loops` / `find_invariant_loads` are *bytecode*
//   analyses — they consume the raw `&[u8]` method body and return
//   bytecode-PC keyed facts (induction-variable locals, trip counts,
//   getfield/getstatic invariant-load sites).  They are not threaded through
//   `optimize(&mut Graph)` (which only sees the post-build SSA graph, no
//   bytecode), and their PC keys do not map onto SSA `NodeId`s, so they
//   cannot *drive* node-level hoisting directly.  This pass therefore makes
//   its hoisting decisions *structurally on the SSA graph* — the only sound
//   basis available here — and uses the SCEV/loop-analysis vocabulary
//   (loop header = `Op::Region`, induction variable = the loop-carried
//   `Op::Phi` anchored at that Region, trip-count-bounded body) to reason
//   about invariance.  `licm_scev_corroborates` bridges back to
//   `scev`/`loop_analysis` for graphs that retain `bytecode_pc` annotations,
//   keeping those modules exercised and giving a future bytecode-threaded
//   caller a ready hook.
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
//   * A load is hoist-eligible only if NO node in the loop body is a
//     `Store` of a possibly-aliasing `MemKind`, a `Call`, a `New`/`NewArray`
//     (constructor side effects), or any other non-pure non-load barrier.
//     We do not run an alias oracle: ANY store / call in the body
//     disqualifies ALL loads (give up — conservative).  This still fires on
//     the common read-only invariant-load loop.
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
/// at least one back-edge. Returns `(header, entry_pred, back_ctrls)`.
fn loop_headers(graph: &Graph) -> Vec<(NodeId, NodeId, Vec<NodeId>)> {
    let users = build_users(graph);
    let mut out = Vec::new();
    for id in 0..graph.nodes.len() as NodeId {
        if !matches!(graph.nodes[id as usize].op, Op::Region | Op::Merge) {
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
        // Reducible single-entry loop: one pre-header, ≥1 back-edge. A nested
        // inner header whose entry is itself control-reachable (both inputs in
        // `reach`) yields zero pre-headers and is skipped — safe, just not
        // optimized (same limitation as the unroll pass).
        if entry_preds.len() == 1 && !back_ctrls.is_empty() {
            out.push((id, entry_preds[0], back_ctrls));
        }
    }
    out
}

/// Compute the set of nodes that belong to the loop whose header is `region`.
///
/// Membership = control-reachable from `region` along control / projection
/// edges, plus the data / memory nodes pinned to those control nodes, without
/// passing back *out* through `region`'s entry predecessor.  We approximate
/// the body with a forward control walk seeded at `region` and at the control
/// projections of `If` nodes inside it, stopping at the graph exit and at any
/// control edge that leaves to a node dominating `region`.  Because the IR is
/// reducible (the builder only creates a Region for a genuine back-edge), this
/// forward-from-header reachability over-approximates the body safely: an
/// over-large body only *loses* hoisting opportunities (more nodes look
/// variant), never produces an unsound hoist.
fn loop_body(
    graph: &Graph,
    region: NodeId,
    entry_pred: NodeId,
    back_ctrls: &[NodeId],
) -> FxHashSet<NodeId> {
    // Control nodes whose nearest enclosing loop header is `region`. We seed
    // with `region` itself and walk *users* (forward control flow) — but the
    // arena stores inputs, not users, so build a users map once.
    let mut users: Vec<Vec<NodeId>> = vec![Vec::new(); graph.nodes.len()];
    for (id, node) in graph.nodes.iter().enumerate() {
        for &inp in &node.inputs {
            if (inp as usize) < users.len() {
                users[inp as usize].push(id as NodeId);
            }
        }
    }

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
    let mut forward: FxHashSet<NodeId> = FxHashSet::default();
    forward.insert(region);
    let mut work = vec![region];
    while let Some(cur) = work.pop() {
        for &u in &users[cur as usize] {
            if u == entry_pred || forward.contains(&u) {
                continue;
            }
            let op = &graph.nodes[u as usize].op;
            if op.is_control() && !matches!(op, Op::Return) {
                if forward.insert(u) {
                    work.push(u);
                }
            }
        }
    }

    // (2) "Can reach a back-edge" set: control nodes from which control flows
    // back to `region` (i.e. that reach one of the back-edge control nodes by
    // following input → producer edges backward from each back-edge ctrl,
    // staying within the forward set). The natural-loop body is the
    // intersection: forward-reachable AND on a path back to the header. This
    // precisely excludes the loop-exit projection and all post-loop code.
    let mut body: FxHashSet<NodeId> = FxHashSet::default();
    body.insert(region);
    let mut back: Vec<NodeId> = back_ctrls.to_vec();
    while let Some(cur) = back.pop() {
        if cur == NO_NODE || cur == region || body.contains(&cur) {
            continue;
        }
        if !forward.contains(&cur) {
            // A back-edge control outside the forward set (irreducible / odd
            // shape): skip it conservatively rather than pulling in foreign
            // control.
            continue;
        }
        body.insert(cur);
        // Walk backward through this control node's control inputs.
        for &inp in &graph.nodes[cur as usize].inputs {
            if inp != region && forward.contains(&inp) {
                back.push(inp);
            }
        }
    }

    // Pinned-node sweep: any non-pure data / memory node whose control or
    // memory input is a body control node, or which is a Phi anchored at the
    // region, belongs to the body. Pure nodes are intentionally *not* pinned
    // (they float); their invariance is decided per use in `is_loop_invariant`.
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead || node.op.is_pure() {
            continue;
        }
        if node.op == Op::Phi {
            // Loop-carried phi: anchored at the region (input slot 0).
            if node.inputs.first().copied() == Some(region) {
                body.insert(id as NodeId);
            }
            continue;
        }
        // Loads / stores / calls / allocations: pinned to a control/mem input.
        if node.inputs.iter().any(|&inp| body.contains(&inp)) {
            body.insert(id as NodeId);
        }
    }
    body
}

/// True if `id` produces a value that is constant across all iterations of the
/// loop with header `region` and body `body`. Conservative: any uncertainty
/// (out-of-range, Dead, a loop-carried phi, or membership in the body)
/// returns false. `depth` bounds the recursion so a cyclic phi (e.g. from a
/// *different* loop nest) cannot cause unbounded recursion — exceeding the
/// bound is treated conservatively as variant.
fn is_loop_invariant(graph: &Graph, id: NodeId, region: NodeId, body: &FxHashSet<NodeId>) -> bool {
    is_loop_invariant_d(graph, id, region, body, 0)
}

fn is_loop_invariant_d(
    graph: &Graph,
    id: NodeId,
    region: NodeId,
    body: &FxHashSet<NodeId>,
    depth: u32,
) -> bool {
    if id == NO_NODE || (id as usize) >= graph.nodes.len() {
        return false;
    }
    if depth > 64 {
        // Recursion bound hit (possible phi cycle in another loop nest):
        // give up conservatively.
        return false;
    }
    let node = &graph.nodes[id as usize];
    match node.op {
        Op::Dead => false,
        // Constants / params are defined at Start — always invariant.
        Op::Const(_) | Op::ConstF(_) | Op::Param(_) => true,
        // Any phi is a merge value. A phi anchored at *this* loop's region is
        // the induction / loop-carried value: variant by construction. A phi
        // anchored elsewhere is treated as variant too (conservative — we do
        // not prove cross-merge invariance here).
        Op::Phi => false,
        _ => {
            // Anything physically in the loop body is variant.
            if body.contains(&id) {
                return false;
            }
            // Otherwise invariant iff every data input is invariant. We skip
            // control (ctrl/mem) inputs for pure nodes (they have none) and,
            // for impure nodes, defer to the caller's barrier check.
            node.inputs.iter().all(|&inp| {
                inp == NO_NODE || is_loop_invariant_d(graph, inp, region, body, depth + 1)
            })
        }
    }
}

/// True if the loop body contains any memory barrier (store, call, allocation,
/// guard, monitor) that could clobber a hoisted load. Conservative: a single
/// barrier disqualifies all load hoisting for this loop.
fn loop_has_memory_barrier(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    body.iter().any(|&id| {
        let op = &graph.nodes[id as usize].op;
        !op.is_pure() && !op.is_control() && !matches!(op, Op::Load(_) | Op::Phi | Op::Dead)
    })
}

/// Run loop-invariant code motion. Returns `true` if it changed the graph.
///
/// See the module-level soundness model. Hoists invariant loads out of each
/// natural loop — `Op::Region` *or* a javac back-edge `Op::Merge` header (see
/// `loop_headers`) — into the loop pre-header (the structurally-classified
/// entry-predecessor control), provided the loop body has no memory barrier
/// that could clobber the load. Pure invariant nodes already float in
/// Sea-of-Nodes, so no placement change is needed for them — but recognising
/// them lets the trailing GVN pass dedup loop-entry vs loop-body copies.
fn licm(graph: &mut Graph) -> bool {
    // Normalize the single-input `Op::Merge` control pass-throughs the builder
    // wraps around branch projections, so a javac loop header and its back-edge
    // sit adjacent to their If/Proj (mirrors the unroll pass). Without this,
    // real `Merge`-header loops are not recognised. Idempotent — a no-op when
    // unroll already ran it (or on hand-built `Region` graphs with no trivial
    // merges). Fold any structural change into `changed` so `optimize()` runs
    // its trailing GVN/DCE cleanup.
    let live_before = graph.live_count();
    collapse_trivial_merges(graph);
    let mut changed = graph.live_count() != live_before;

    let headers = loop_headers(graph);
    if headers.is_empty() {
        return changed;
    }

    for (region, entry_pred, back_ctrls) in headers {
        // Re-validate: the header may have been killed by an earlier hoist's
        // cleanup in this same loop set (defensive).
        if !matches!(graph.nodes[region as usize].op, Op::Region | Op::Merge) {
            continue;
        }
        let body = loop_body(graph, region, entry_pred, &back_ctrls);
        // The pre-header is the classified loop-entry predecessor — NOT
        // necessarily input slot 0, since a `Merge` header may carry the
        // back-edge at either slot.
        let preheader = entry_pred;
        if preheader == NO_NODE || body.contains(&preheader) {
            // No identifiable pre-header outside the loop → cannot hoist.
            continue;
        }

        // Load hoisting requires a clobber-free body.
        if loop_has_memory_barrier(graph, &body) {
            continue;
        }

        // Collect hoistable loads: in the body, with invariant base/address,
        // typed as a real Load. We snapshot ids first (we mutate inputs after).
        let load_ids: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| matches!(graph.nodes[id as usize].op, Op::Load(_)))
            .collect();

        // The memory token to use at the pre-header: the region's *entry*
        // memory phi input if a memory phi exists, else any invariant memory.
        // For loads in the EA-bridge/hand-built compact form (`[base]` or
        // `[base, index]`) there is no control/mem operand to repoint, so the
        // hoist is purely a re-anchor that GVN/scheduling already honour; we
        // still record `changed` so the trailing cleanup runs.
        for load in load_ids {
            let inputs = graph.nodes[load as usize].inputs.clone();
            // Identify base / address operands across both layouts:
            //   compact:  [base] | [base, index]
            //   full:     [ctrl, mem, base, index?]
            let (base, addr) = match inputs.len() {
                1 => (inputs[0], NO_NODE),
                2 => {
                    // Disambiguate compact [base, index] from a 2-input full
                    // form by the type of slot 0: a Memory-typed slot 0 means
                    // the full form is malformed here (we only see compact),
                    // so treat slot 0 as base.
                    (inputs[0], inputs[1])
                }
                3 => (inputs[2], NO_NODE),             // [ctrl, mem, base]
                n if n >= 4 => (inputs[2], inputs[3]), // [ctrl, mem, base, index]
                _ => (NO_NODE, NO_NODE),
            };
            if !is_loop_invariant(graph, base, region, &body) {
                continue;
            }
            if addr != NO_NODE && !is_loop_invariant(graph, addr, region, &body) {
                continue;
            }
            // The load is invariant and the body is barrier-free → it is safe
            // to compute once at the pre-header. Remove it from the body by
            // repointing its control input (slot 0 of the full form) to the
            // pre-header so scheduling/lowering place it before the loop. For
            // the compact form there is no control slot, so invariance alone
            // (already established) makes it loop-entry schedulable.
            if inputs.len() >= 3 {
                // Full form: repoint ctrl (slot 0) to the pre-header control.
                if graph.nodes[load as usize].inputs[0] != preheader {
                    graph.nodes[load as usize].inputs[0] = preheader;
                    changed = true;
                }
            } else {
                // Compact form: nothing to repoint, but mark changed so the
                // trailing GVN dedups identical hoisted loads to one.
                changed = true;
            }
        }
    }
    changed
}

/// Bridge to the bytecode-level `scev` / `loop_analysis` passes: given the
/// method bytecode, corroborate that a loop with the given `(header_pc,
/// back_edge_pc)` is a recognised counted loop with at least one invariant
/// load site. Returns `true` when SCEV agrees there is a counted induction
/// variable OR `loop_analysis` finds an invariant getfield/getstatic in the
/// body — i.e. when bytecode-level analysis independently supports hoisting.
///
/// Not currently called from `optimize()` (which has no bytecode in scope);
/// it is the hook a future bytecode-threaded caller uses to gate the
/// graph-structural `licm()` against the bytecode analyses, and it keeps
/// `scev` / `loop_analysis` exercised from this module.
#[allow(dead_code)]
pub fn licm_scev_corroborates(code: &[u8], code_len: usize) -> bool {
    let loops = crate::loop_analysis::detect_loops(code, code_len);
    if loops.is_empty() {
        return false;
    }
    let pairs: Vec<(usize, usize)> = loops
        .iter()
        .filter_map(|li| {
            li.back_edges
                .iter()
                .copied()
                .max()
                .map(|be| (li.header_pc, be))
        })
        .collect();
    let ivs = crate::scev::analyze_induction_variables(code, code_len, &pairs);
    let has_counted_iv = ivs.iter().any(|iv| iv.stride != 0);
    let has_invariant_load = loops
        .iter()
        .any(|li| !crate::loop_analysis::find_invariant_loads(li, code).is_empty());
    has_counted_iv || has_invariant_load
}

// ── Dead Store Elimination (DSE) ─────────────────────────────────────
//
// `eliminate_dead_nodes` (below) is a *value* DCE: it removes nodes whose
// result no node reads.  It cannot remove a store, because a store produces
// no value its consumers depend on — its effect is on memory, and DCE keeps
// any node transitively reachable from the exit through the memory-token /
// input chain.  DSE is the complementary pass that targets the *side
// effect*: a `Store` whose written location is provably overwritten by a
// later store before any read, or that is never read at all, performs no
// observable work and can be deleted.
//
// SOUNDNESS MODEL (deliberately conservative — removing a store that *was*
// observed is silent data loss):
//
//   * We only ever remove a store to a **provably-local, non-escaping
//     allocation** (`New`/`NewArray` base).  A store through a Param, a
//     loaded reference, a phi, a call result, or any base we cannot resolve
//     to a fresh allocation in this method is left untouched — another
//     thread, the caller, or a callee might observe it.
//
//   * "Overwritten before any read" is decided by an ordered scan in
//     node-id order.  A store `S` to location `L = (base, idx_key, kind)` is
//     dead iff a *later* store `S'` writes the same `L` with **no** node
//     between them that could read or alias memory.  The instant we see *any*
//     non-pure, non-store node — `Load`, `Call`, `Return`, `New`/`NewArray`
//     (constructor effects), `Guard`, `ArrayLength`, a monitor op, OR any
//     control / merge / phi / projection node — we flush every pending store
//     (give up — conservative).
//
//     WHY THIS IS SOUND DESPITE NODE-ID ORDER NOT BEING GLOBAL PROGRAM ORDER:
//     in this Sea-of-Nodes IR, node id is creation order, which is a faithful
//     linearisation only *within a single straight-line region*.  Across a
//     branch or join the order is meaningless.  But every control node
//     (`If`, `Merge`, `Region`, `Proj`, `Start`, `Return`) and every `Phi`
//     is non-pure, so it is a barrier that flushes the pending set.  Two
//     stores therefore only ever match when no control node lies between
//     them — i.e. they are in the same straight-line region where node-id
//     order *is* a valid before/after relationship.  This is intentionally
//     coarse: it fires on the common straight-line "init then overwrite" and
//     "write-only scratch field" shapes without needing a real alias oracle.
//
//   * The location key uses the *base node id*, the *index/offset node id*
//     (or a sentinel for plain field stores with no index operand), and the
//     `MemKind`.  Two stores match only when all three are structurally
//     identical.  Because the base is a single SSA allocation node, equal
//     base ids are the same object; differing `MemKind`/idx are different
//     slots and never matched.
//
// WIDENED CASE — WRITE-ONLY, NEVER-READ (Increment 2):
//
//   The straight-line "overwritten before any read" phase above only fires
//   inside one barrier-free region.  The second phase catches the
//   complementary shape: a store to a local `New`/`NewArray` allocation that
//   is *never read on any path* and *never escapes*, so every store into it
//   is dead regardless of control flow.
//
//   SOUNDNESS: a store to allocation `A` is write-only-dead iff
//     (1) `A` is a fresh local `New`/`NewArray` (same provenance guard), AND
//     (2) NO `Load` node anywhere in the graph reads from `A`
//         (`load_base(load) == A`), AND
//     (3) `A` does not escape — it never flows, *other than as the base of a
//         Store*, into any node that could publish or read it: a `Call` arg,
//         a `Return`, a `Store` *value* slot (storing the ref into another
//         object), an `ArrayLength`, or any base we cannot classify.  A
//         `Phi`/`Proj` use is treated as escaping (conservative — we do not
//         chase the ref through merges here, the increment-1 EA does).
//   When all three hold, *every* Store whose base is `A` is observably inert
//   (nothing ever loads what was written, and no external code sees `A`), so
//   each such Store is removed.  This is path-insensitive and needs no
//   ordering relation, because the predicate "no load of A exists" is a whole-
//   graph property.
//
//   This deliberately does NOT extend the location key to array-element
//   stores for the *overwrite* phase: the production bytecode→IR builder
//   (`ir.rs`) never emits `Op::Store`/`Op::NewArray` at all (it bails on those
//   opcodes), so the only array stores that exist are EA-bridge / hand-built
//   compact-form `[base, value]` nodes with no distinct element index operand
//   to key on.  Until a real array `Store` operand layout with a resolvable
//   element index lands, array-element overwrite matching is left out; the
//   write-only phase above is layout-agnostic (it keys on the base only) and
//   so already covers write-only dead array stores.
//
// This pass is monotone (only ever marks nodes `Dead`) and idempotent, so it
// composes safely inside the `optimize()` fixed-point loop.

/// A memory location written by a store, used to match an overwriting store.
/// Equality is structural over (base, index, kind).
#[derive(Clone, Copy, PartialEq, Eq)]
struct StoreLoc {
    /// The base reference node (a non-escaping allocation).
    base: NodeId,
    /// The index/offset operand node, or `NO_NODE` for a plain field store.
    idx: NodeId,
    /// The access width / kind.
    kind: MemKind,
}

/// Read `(base, idx, value)` from a `Store` node, tolerating both the compact
/// `[base, value]` layout used by the EA bridge / hand-built test graphs and
/// the full `[ctrl, mem, base, index/offset, value]` layout documented on
/// `Op::Store`.  Returns `None` when the layout is too short to interpret,
/// so the caller treats the store conservatively (never removed).
///
/// Disambiguation: the documented full form always carries a `Memory`-typed
/// token at input slot 1, whereas the compact form puts the base reference
/// there.  We key off the type of slot 1's producer.
fn store_operands(graph: &Graph, store_id: NodeId) -> Option<(NodeId, NodeId, NodeId)> {
    let n = &graph.nodes[store_id as usize];
    debug_assert!(matches!(n.op, Op::Store(_)));
    let inputs = &n.inputs;
    match inputs.len() {
        // Compact: [base, value]
        2 => Some((inputs[0], NO_NODE, inputs[1])),
        // Full store with no explicit index: [ctrl, mem, base, value]
        4 => Some((inputs[2], NO_NODE, inputs[3])),
        // Full store with index/offset: [ctrl, mem, base, index, value]
        n if n >= 5 => Some((inputs[2], inputs[3], inputs[4])),
        _ => None,
    }
}

/// Read the base reference a `Load` node addresses, tolerating both the
/// compact (`[base]` / `[base, index]`) and full (`[ctrl, mem, base, index?]`)
/// layouts — mirrors `store_operands`. Returns `None` for an uninterpretable
/// layout (caller treats it conservatively as a potential reader of anything).
fn load_base(graph: &Graph, load_id: NodeId) -> Option<NodeId> {
    let n = &graph.nodes[load_id as usize];
    debug_assert!(matches!(n.op, Op::Load(_)));
    let inputs = &n.inputs;
    match inputs.len() {
        // Compact: [base] or [base, index] — slot 0 is the base.
        1 | 2 => Some(inputs[0]),
        // Full with no index: [ctrl, mem, base].
        3 => Some(inputs[2]),
        // Full with index: [ctrl, mem, base, index].
        n if n >= 4 => Some(inputs[2]),
        _ => None,
    }
}

/// True if `id` is a fresh allocation node in this method (provably local
/// provenance — the only base we are willing to delete a store to).
fn is_local_alloc(graph: &Graph, id: NodeId) -> bool {
    if id == NO_NODE || (id as usize) >= graph.nodes.len() {
        return false;
    }
    matches!(
        graph.nodes[id as usize].op,
        Op::New { .. } | Op::NewArray { .. }
    )
}

/// True if a node could read memory or otherwise observe a pending store, so
/// encountering it forces DSE to discard all pending (not-yet-overwritten)
/// stores.  This is the conservative "alias barrier": anything that is not a
/// pure data computation is treated as a potential reader.
///
/// `Op::Store` is handled explicitly by the scan (it drives the overwrite
/// match), and `Op::Load` is intercepted by an earlier explicit arm in
/// `eliminate_dead_stores` that resolves it against the pending set with
/// allocation-level alias precision (a load of a distinct local allocation
/// observes no other local's stores). A load never reaches this function;
/// keeping `Load` classified as a barrier here is the SAFE fallback if that
/// explicit arm is ever removed. Every other impure node (calls, returns,
/// allocations, guards, monitors, control joins, projections, memory phis, …)
/// is an unconditional barrier.
fn is_memory_barrier(op: &Op) -> bool {
    if op.is_pure() {
        return false;
    }
    !matches!(op, Op::Store(_))
}

/// Dead-store elimination: remove stores whose effect is never observed.
///
/// See the module-level soundness model above.  Walks nodes in id order,
/// tracking the most recent still-live store to each location written to a
/// local allocation; an overwriting store to the same location kills the
/// earlier one, and any memory barrier flushes the pending set.
///
/// Alias refinement (DSE-WIDEN): an intervening `Load` is not an unconditional
/// barrier. Because every pending store targets a local allocation and two
/// distinct local allocations never alias, a load of a *different* local
/// allocation flushes only its own base; only a load of the same base, or of a
/// non-local/unreadable base, flushes (the latter, everything). This lets a
/// store survive an intervening read of an unrelated local object.
fn eliminate_dead_stores(graph: &mut Graph) {
    // The location each currently-pending store writes, in node order. A
    // pending store is one we have seen but not yet proven observed; a later
    // store to the same location proves it dead.
    let mut pending: Vec<(NodeId, StoreLoc)> = Vec::new();
    let mut to_kill: Vec<NodeId> = Vec::new();

    let len = graph.nodes.len();
    for id in 0..len {
        let op = graph.nodes[id].op.clone();
        match op {
            Op::Dead => continue,
            Op::Store(kind) => {
                let store_id = id as NodeId;
                let (base, idx, _val) = match store_operands(graph, store_id) {
                    Some(t) => t,
                    None => {
                        // Unreadable layout — treat as a barrier so we never
                        // delete around a store we cannot understand.
                        pending.clear();
                        continue;
                    }
                };

                // Only stores to a fresh local allocation are eligible: any
                // other base may be observed outside this method.
                if !is_local_alloc(graph, base) {
                    // Non-local store: it both may be observed AND may alias
                    // pending locals (we cannot prove otherwise), so flush.
                    pending.clear();
                    continue;
                }

                let loc = StoreLoc { base, idx, kind };

                // If an earlier pending store wrote the exact same location,
                // it is overwritten here with no intervening reader → dead.
                if let Some(pos) = pending.iter().position(|(_, l)| *l == loc) {
                    let (dead_store, _) = pending.remove(pos);
                    to_kill.push(dead_store);
                }
                // This store is now the live writer of `loc`.
                pending.push((store_id, loc));
            }
            Op::Load(_) => {
                // A load is only a barrier for stores it could OBSERVE. Every
                // pending store writes a local `New`/`NewArray` allocation, and
                // two distinct local allocations never alias (the same fact the
                // location key relies on). So:
                //   * a load whose base is a DIFFERENT local allocation cannot
                //     read any other local's fields — it only flushes pending
                //     stores to its own base (which it genuinely could observe);
                //   * a load from a non-local / unreadable base may alias any
                //     escaped local, so it conservatively flushes everything.
                // This lets an overwritten store stay dead-eligible across an
                // intervening read of an unrelated local object (DSE-WIDEN),
                // while remaining sound: the only way to read allocation A's
                // field is a load whose base resolves to A (flushes A) or to a
                // non-local node such as a load/phi result (flushes all).
                match load_base(graph, id as NodeId) {
                    Some(b) if is_local_alloc(graph, b) => {
                        pending.retain(|(_, loc)| loc.base != b);
                    }
                    _ => pending.clear(),
                }
            }
            other => {
                // Any non-pure, non-store node may observe memory: give up on
                // every pending store (we cannot prove they are unread past
                // this point).
                if is_memory_barrier(&other) {
                    pending.clear();
                }
            }
        }
    }

    for id in to_kill {
        graph.kill(id);
    }

    // ── Phase 2: write-only, never-read local allocations ──────────────
    // Path-insensitive: a store to a local alloc that is never loaded and
    // never escapes is dead regardless of control flow. See the module-level
    // "WIDENED CASE" note for the soundness argument.
    eliminate_write_only_stores(graph);
}

/// Remove every `Store` into a local `New`/`NewArray` allocation that is never
/// read (no `Load` addresses it) and never escapes the method. Path-
/// insensitive whole-graph analysis; see the DSE module note.
fn eliminate_write_only_stores(graph: &mut Graph) {
    use std::collections::hash_map::Entry;

    // 1. Candidate allocations: every fresh local alloc node.
    let mut candidate: FxHashMap<NodeId, bool> = FxHashMap::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::New { .. } | Op::NewArray { .. }) {
            candidate.insert(id as NodeId, true); // true = still write-only
        }
    }
    if candidate.is_empty() {
        return;
    }

    // 2. Disqualify any allocation that is read or escapes. We scan every use
    //    of every candidate. A use disqualifies the candidate unless it is the
    //    *base* slot of a Store (the only inert use of a write-only alloc).
    for (id, node) in graph.nodes.iter().enumerate() {
        let id = id as NodeId;
        match &node.op {
            Op::Dead => continue,
            Op::Load(_) => {
                // Reading any candidate disqualifies it.
                if let Some(base) = load_base(graph, id) {
                    if let Entry::Occupied(mut e) = candidate.entry(base) {
                        *e.get_mut() = false;
                    }
                } else {
                    // Unreadable load layout: conservatively disqualify every
                    // candidate it could touch (all of them).
                    for v in candidate.values_mut() {
                        *v = false;
                    }
                }
            }
            Op::Store(_) => {
                // The base slot is the inert use; the *value* slot escaping the
                // ref (storing a candidate ref into another object) disqualifies
                // it. `idx` operand referencing a candidate would be nonsensical
                // for an alloc ref but is treated as escaping if it occurs.
                if let Some((base, idx, val)) = store_operands(graph, id) {
                    // value or index referencing a candidate ⇒ escapes.
                    for &slot in &[val, idx] {
                        if slot != NO_NODE {
                            if let Entry::Occupied(mut e) = candidate.entry(slot) {
                                *e.get_mut() = false;
                            }
                        }
                    }
                    // base is the inert use — does not disqualify.
                    let _ = base;
                } else {
                    // Unreadable store layout: disqualify everything it touches.
                    for &inp in &node.inputs {
                        if let Entry::Occupied(mut e) = candidate.entry(inp) {
                            *e.get_mut() = false;
                        }
                    }
                }
            }
            // Any OTHER node (Call arg, Return value, ArrayLength, Phi, Proj,
            // a second New's input, Guard, …) that references a candidate
            // publishes or observes the ref ⇒ escapes / read ⇒ disqualify.
            _ => {
                for &inp in &node.inputs {
                    if let Entry::Occupied(mut e) = candidate.entry(inp) {
                        *e.get_mut() = false;
                    }
                }
            }
        }
    }

    // 3. Kill every Store whose base is a surviving write-only candidate AND
    //    whose own node is not consumed as an input elsewhere. A Store
    //    referenced by another node (a memory-token chain, a Return, …) is
    //    being kept alive as an observed effect; we only remove the store when
    //    nothing depends on the store node itself, so this never severs a
    //    memory/effect edge a consumer relies on.
    let uses = graph.use_counts();
    let mut to_kill: Vec<NodeId> = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        if let Op::Store(_) = node.op {
            if uses[id] != 0 {
                continue; // store node is depended upon — keep it
            }
            if let Some((base, _, _)) = store_operands(graph, id as NodeId) {
                if candidate.get(&base).copied() == Some(true) {
                    to_kill.push(id as NodeId);
                }
            }
        }
    }
    for id in to_kill {
        graph.kill(id);
    }
}

// ── Dead Node Elimination ────────────────────────────────────────────

/// Remove nodes not reachable from the exit (Return).
fn eliminate_dead_nodes(graph: &mut Graph) {
    if graph.exit == NO_NODE {
        return;
    }

    let mut reachable: FxHashSet<NodeId> = FxHashSet::default();
    let mut worklist = vec![graph.exit];

    // real-frame-deopt (#5): safepoint snapshots are NOT seeded as DCE roots.
    // The builder records a snapshot at *every* bytecode boundary, so pinning
    // every safepoint-referenced value would keep every transient operand
    // alive and defeat DCE/reassociation/folding. Making safepoints DCE-safe
    // needs the model to change first — record/pin only at real deopt sites
    // (guard bcis, call returns), or recompute safepoint liveness AFTER
    // optimization — rather than this build-time every-bci snapshot. Until
    // then the deopt path is exercised on the un-optimized graph (see
    // `ir_lower` tests); a value killed here resolves to `Undefined`.

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

// ── Loop unrolling (activate-ir-optimizer increment 3) ───────────────────

/// `true` when `CRATONVM_JIT_UNROLL` is set (cached). Enables full unrolling of
/// small constant-trip-count counted loops. Default-OFF: experimental — it
/// soaks behind the gate and is validated live against HotSpot before any flip.
pub fn unroll_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_JIT_UNROLL").is_some())
}

/// Only fully unroll loops whose constant trip count is at most this (bounds
/// code growth; larger counted loops are left rolled).
const UNROLL_MAX_TRIP: i64 = 8;
/// Skip loops whose per-iteration computation exceeds this many nodes.
const UNROLL_MAX_BODY: usize = 48;

fn const_i64(graph: &Graph, id: NodeId) -> Option<i64> {
    if id == NO_NODE || id as usize >= graph.nodes.len() {
        return None;
    }
    match graph.nodes[id as usize].op {
        Op::Const(c) => Some(c),
        _ => None,
    }
}

fn eval_cmp_i64(op: CmpOp, x: i64, y: i64) -> bool {
    match op {
        CmpOp::Eq => x == y,
        CmpOp::Ne => x != y,
        CmpOp::Lt => x < y,
        CmpOp::Le => x <= y,
        CmpOp::Gt => x > y,
        CmpOp::Ge => x >= y,
    }
}

/// A recognised constant-trip counted loop suitable for full unrolling.
struct CountedLoop {
    iv_phi: NodeId,
    iv_init: i64,
    iv_stride: i64,
    trip: i64,
    if_node: NodeId,
    exit_ctrl: NodeId,
}

/// The single `Op::Proj(which)` user of `node`, or `NO_NODE`.
fn proj_user(graph: &Graph, node: NodeId, which: u8) -> NodeId {
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Proj(which) && n.inputs.first().copied() == Some(node) {
            return id as NodeId;
        }
    }
    NO_NODE
}

/// Users map: `users[id]` = nodes that reference `id` as an input.
fn build_users(graph: &Graph) -> Vec<Vec<NodeId>> {
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
fn analyze_counted_loop(graph: &Graph, region: NodeId, back_ctrl: NodeId) -> Option<CountedLoop> {
    let dbg = std::env::var_os("CRATONVM_DBG_UNROLL").is_some();
    let n = graph.nodes.len();
    // Induction phi: Phi anchored at `region`, inputs [region, init(Const), next],
    // next = Add(self, Const) or Add(Const, self).
    let mut iv: Option<(NodeId, i64, i64)> = None;
    for id in 0..n {
        let node = &graph.nodes[id];
        if node.op != Op::Phi
            || node.inputs.first().copied() != Some(region)
            || node.inputs.len() < 3
        {
            continue;
        }
        let init = match const_i64(graph, node.inputs[1]) {
            Some(c) => c,
            None => continue,
        };
        let nxt = node.inputs[2];
        if nxt == NO_NODE || nxt as usize >= n || graph.nodes[nxt as usize].op != Op::Add {
            continue;
        }
        let ni = &graph.nodes[nxt as usize].inputs;
        if ni.len() != 2 {
            continue;
        }
        let stride = if ni[0] == id as NodeId {
            const_i64(graph, ni[1])
        } else if ni[1] == id as NodeId {
            const_i64(graph, ni[0])
        } else {
            None
        };
        if let Some(s) = stride {
            if s != 0 {
                iv = Some((id as NodeId, init, s));
                break;
            }
        }
    }
    let (iv_phi, iv_init, iv_stride) = match iv {
        Some(v) => v,
        None => {
            if dbg {
                eprintln!("[DBG_UNROLL] region {region}: bail — no constant-stride induction phi");
            }
            return None;
        }
    };

    // Loop exit test: the single `If` controlled by a node inside the loop. We
    // accept the header test (`If.ctrl == region`) and the common bottom test
    // (`If.ctrl` chains back to the region via the back-edge control).
    let mut if_node = NO_NODE;
    for id in 0..n {
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
                return None;
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
        return None;
    }
    let cond = *graph.nodes[if_node as usize].inputs.get(1)?;
    let op = match graph.nodes.get(cond as usize)?.op {
        Op::Cmp(o) => o,
        _ => return None,
    };
    let ci = &graph.nodes[cond as usize].inputs;
    if ci.len() != 2 {
        return None;
    }
    // Compare the induction phi against a constant bound.
    let (iv_on_left, bound) = if ci[0] == iv_phi {
        (true, const_i64(graph, ci[1])?)
    } else if ci[1] == iv_phi {
        (false, const_i64(graph, ci[0])?)
    } else {
        return None;
    };

    // Which If projection re-enters the loop (the "continue" edge)?
    if graph.nodes.get(back_ctrl as usize)?.inputs.first().copied() != Some(if_node) {
        return None;
    }
    let back_is_true = match graph.nodes[back_ctrl as usize].op {
        Op::Proj(0) => true,
        Op::Proj(1) => false,
        _ => return None,
    };
    let exit_ctrl = proj_user(graph, if_node, if back_is_true { 1 } else { 0 });
    if exit_ctrl == NO_NODE {
        return None;
    }

    // Concrete trip count by simulation (init/stride/bound all constant).
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
        i = i.wrapping_add(iv_stride);
        trip += 1;
        if trip > UNROLL_MAX_TRIP {
            return None; // too large / non-terminating within the cap
        }
    }
    Some(CountedLoop {
        iv_phi,
        iv_init,
        iv_stride,
        trip,
        if_node,
        exit_ctrl,
    })
}

/// Collapse trivial single-input `Op::Merge` nodes (control pass-throughs the
/// builder emits around branch projections) by forwarding their users to the
/// single input. Run before loop detection so a loop header and its back-edge
/// sit adjacent to the `If`/`Proj` nodes (the javac loop shape wraps the If's
/// projections in single-input Merges).
fn collapse_trivial_merges(graph: &mut Graph) {
    loop {
        let mut changed = false;
        for id in 0..graph.nodes.len() {
            if graph.nodes[id].op == Op::Merge && graph.nodes[id].inputs.len() == 1 {
                let inp = graph.nodes[id].inputs[0];
                if inp != NO_NODE && inp != id as NodeId {
                    graph.replace_all_uses(id as NodeId, inp);
                    graph.kill(id as NodeId);
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
fn forward_control_closure(
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
fn unroll(graph: &mut Graph) -> bool {
    let dbg = std::env::var_os("CRATONVM_DBG_UNROLL").is_some();
    // Normalize away the single-input Merge wrappers the builder puts around
    // branch projections, so loop headers/back-edges sit next to their If/Proj.
    collapse_trivial_merges(graph);
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
    let mut changed = false;
    for region in headers {
        if !matches!(graph.nodes[region as usize].op, Op::Region | Op::Merge) {
            continue; // killed by an earlier unroll in this pass
        }
        let rin = graph.nodes[region as usize].inputs.clone();
        if rin.len() != 2 {
            continue; // single back-edge only
        }
        let users = build_users(graph);
        // Identify the back-edge input (control-reachable from the header) vs the
        // loop-entry predecessor.
        let reach = forward_control_closure(graph, &users, region);
        let back_inputs: Vec<NodeId> = rin
            .iter()
            .copied()
            .filter(|&i| i != NO_NODE && reach.contains(&i))
            .collect();
        if back_inputs.len() != 1 {
            if dbg {
                eprintln!("[DBG_UNROLL] header {region}: bail — not a single-back-edge loop (back={back_inputs:?})");
            }
            continue;
        }
        let back_ctrl = back_inputs[0];
        let entry_pred = rin
            .iter()
            .copied()
            .find(|&i| i != back_ctrl)
            .unwrap_or(NO_NODE);
        if entry_pred == NO_NODE {
            continue;
        }
        let info = match analyze_counted_loop(graph, region, back_ctrl) {
            Some(i) => i,
            None => continue,
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
            continue;
        }
        let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();

        // Variance: nodes transitively using a carried phi (forward propagation).
        let mut variant: FxHashSet<NodeId> = carried_set.clone();
        let mut vw: Vec<NodeId> = carried.clone();
        while let Some(c) = vw.pop() {
            for &u in &users[c as usize] {
                if variant.insert(u) {
                    vw.push(u);
                }
            }
        }

        // No side effects in the loop: a Store/Call/alloc/ArrayLength/Guard that
        // is loop-variant or control-pinned to the loop is a side effect we do
        // not model → bail.
        for id in 0..graph.nodes.len() {
            let op = &graph.nodes[id].op;
            if matches!(
                op,
                Op::Store(_)
                    | Op::Call
                    | Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::ArrayLength
                    | Op::Guard { .. }
            ) {
                let ctrl = graph.nodes[id].inputs.first().copied().unwrap_or(NO_NODE);
                if variant.contains(&(id as NodeId)) || ctrl == region || ctrl == back_ctrl {
                    if dbg {
                        eprintln!("[DBG_UNROLL] region {region}: bail — loop side effect {:?} (node {id})", graph.nodes[id].op);
                    }
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }

        // Per-iteration computation = variant nodes backward-reachable from each
        // carried phi's back-edge value and the loop condition; stops at carried
        // phis (substituted) and invariant nodes (shared). Excludes post-loop uses.
        let cond = graph.nodes[info.if_node as usize].inputs[1];
        let mut seeds: Vec<NodeId> = vec![cond];
        for &p in &carried {
            seeds.push(graph.nodes[p as usize].inputs[2]);
        }
        let mut to_clone: FxHashSet<NodeId> = FxHashSet::default();
        let mut cw = seeds;
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
            // Only loads and pure arithmetic are clonable.
            let opc = &graph.nodes[c as usize].op;
            if !matches!(opc, Op::Load(_)) && !opc.is_pure() {
                if dbg {
                    eprintln!("[DBG_UNROLL] region {region}: bail — unclonable body node {opc:?} (node {c})");
                }
                ok = false;
                break;
            }
            to_clone.insert(c);
            for &inp in &graph.nodes[c as usize].inputs {
                cw.push(inp);
            }
        }
        if !ok {
            continue;
        }
        if to_clone.len() > UNROLL_MAX_BODY {
            continue;
        }
        // Bail on invariant loads pinned to the loop header (we'd have to
        // re-anchor them); milestone-1 only handles variant (cloned) loads.
        let mut pinned_invariant_load = false;
        for id in 0..graph.nodes.len() {
            if matches!(graph.nodes[id].op, Op::Load(_))
                && graph.nodes[id].inputs.first().copied() == Some(region)
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
            while let Some(&(node, idx)) = stack.last() {
                state.insert(node, 1);
                let inputs = &graph.nodes[node as usize].inputs;
                if idx < inputs.len() {
                    let inp = inputs[idx];
                    stack.last_mut().unwrap().1 += 1;
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

        // Running value of each carried phi entering the current iteration,
        // seeded with its preheader (entry) value.
        let mut cur: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        for &p in &carried {
            cur.insert(p, graph.nodes[p as usize].inputs[1]);
        }

        for t in 0..info.trip {
            // Substitution for iteration t: region control → preheader; each
            // carried phi → its running value (the iv's running value is the
            // concrete constant init + t*stride).
            let mut subst: FxHashMap<NodeId, NodeId> = FxHashMap::default();
            subst.insert(region, entry_pred);
            for (&p, &v) in &cur {
                subst.insert(p, v);
            }
            // Clone the per-iteration computation in topo order.
            for &d in &order {
                let (op, ty, pc) = {
                    let nd = &graph.nodes[d as usize];
                    (nd.op.clone(), nd.ty, nd.bytecode_pc)
                };
                let new_inputs: Vec<NodeId> = graph.nodes[d as usize]
                    .inputs
                    .iter()
                    .map(|&inp| subst.get(&inp).copied().unwrap_or(inp))
                    .collect();
                let clone = graph.add(op, ty, new_inputs, pc);
                subst.insert(d, clone);
            }
            // Advance carried phis to their next-iteration values.
            let mut next: FxHashMap<NodeId, NodeId> = FxHashMap::default();
            for &p in &carried {
                if p == info.iv_phi {
                    let v = info
                        .iv_init
                        .wrapping_add((t + 1).wrapping_mul(info.iv_stride));
                    let c = graph.add(Op::Const(v), IrType::Int, vec![], None);
                    next.insert(p, c);
                } else {
                    let back_val = graph.nodes[p as usize].inputs[2];
                    next.insert(p, subst.get(&back_val).copied().unwrap_or(back_val));
                }
            }
            cur = next;
        }

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
        graph.replace_all_uses(info.exit_ctrl, entry_pred);
        graph.kill(region);
        graph.kill(info.if_node);
        graph.kill(back_ctrl);
        graph.kill(info.exit_ctrl);
        for &p in &carried {
            graph.kill(p);
        }
        for &d in &to_clone {
            graph.kill(d);
        }
        if std::env::var_os("CRATONVM_DBG_UNROLL").is_some() {
            eprintln!(
                "[DBG_UNROLL] fully unrolled counted loop (region {region}, trip {}, {} cloned nodes/iter)",
                info.trip,
                to_clone.len()
            );
        }
        changed = true;
    }
    changed
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrBuilder, MemKind};

    fn build_and_optimize(
        code: &[u8],
        code_len: usize,
        num_params: usize,
        num_locals: usize,
    ) -> Graph {
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

    fn build_and_reassociate(
        code: &[u8],
        code_len: usize,
        num_params: usize,
        num_locals: usize,
    ) -> Graph {
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
        assert_eq!(
            graph.nodes[c_id as usize].op,
            Op::Const(11),
            "additive constant"
        );
        let mul = &graph.nodes[mul_id as usize];
        assert_eq!(mul.op, Op::Mul);
        let (px, k_id) = if graph.nodes[mul.inputs[0] as usize].op == Op::Param(0) {
            (mul.inputs[0], mul.inputs[1])
        } else {
            (mul.inputs[1], mul.inputs[0])
        };
        assert_eq!(graph.nodes[px as usize].op, Op::Param(0));
        assert_eq!(
            graph.nodes[k_id as usize].op,
            Op::Const(6),
            "multiplicative constant"
        );
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
        assert_eq!(
            graph.nodes[k_id as usize].op,
            Op::Const(32761),
            "181*181 folds to 32761"
        );
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
        assert_eq!(
            r,
            i32::MAX as i64,
            "INT_MIN + (-1) must wrap to INT_MAX, got {:#x}",
            r
        );
        // High 32 bits must be sign-extension of low 32 bits (i.e. 0 here).
        assert_eq!(
            r as i32 as i64, r,
            "result must be a clean i32 sign-extension"
        );
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

    // ── Dead Store Elimination (DSE) ────────────────────────────────

    /// Hand-build a minimal `ir::Graph` (the `optimize()` passes run on the
    /// real `ir::Graph`, distinct from the EA graph). Returns a graph with a
    /// Start node already in place.
    fn probe_graph() -> Graph {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Void, vec![], None);
        g.entry = start;
        g
    }

    #[test]
    fn test_dse_removes_overwritten_store() {
        // A local object whose field 0 is written twice with no read in
        // between: the first store is dead and must be removed; the second
        // (live) store and the allocation survive.
        let mut g = probe_graph();
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        // Compact [base, value] layout (as used by the EA bridge/tests).
        let dead_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
        let live_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c2], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, live_store], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[dead_store as usize].op,
            Op::Dead,
            "the overwritten store must be eliminated"
        );
        assert_eq!(
            g.nodes[live_store as usize].op,
            Op::Store(MemKind::Int),
            "the surviving (overwriting) store must be kept"
        );
        assert!(
            !matches!(g.nodes[alloc as usize].op, Op::Dead),
            "DSE must not touch the allocation"
        );
    }

    #[test]
    fn test_dse_keeps_store_with_intervening_load() {
        // store; load(same obj); store  — the first store IS observed by the
        // load, so it must NOT be removed.
        let mut g = probe_graph();
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
        // A load is a memory barrier — flushes the pending store.
        let _load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![alloc], None);
        let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c2], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, s2], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[s1 as usize].op,
            Op::Store(MemKind::Int),
            "a store observed by an intervening load must be kept"
        );
    }

    #[test]
    fn test_dse_keeps_store_to_non_local_base() {
        // Store through a Param (could be observed by the caller / other
        // threads): even an immediate overwrite must NOT remove the first.
        let mut g = probe_graph();
        let p0 = g.add(Op::Param(0), IrType::Ref, vec![], None);
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![p0, c1], None);
        let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![p0, c2], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, s2], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[s1 as usize].op,
            Op::Store(MemKind::Int),
            "a store to a non-local (Param) base may be observed externally and must be kept"
        );
    }

    #[test]
    fn test_dse_distinct_fields_not_matched() {
        // Two stores to *different* fields of the same object: neither
        // overwrites the other, so the overwrite phase must not match them.
        // A trailing load of the object makes it *read* (so the widened
        // write-only phase does not apply here) — this test isolates the
        // distinct-field non-conflation property of the overwrite phase.
        let mut g = probe_graph();
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 2,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        // Different MemKind → different location key (Int vs Long slot).
        let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
        let s2 = g.add(
            Op::Store(MemKind::Long),
            IrType::Void,
            vec![alloc, c2],
            None,
        );
        // Read the object back so it is observed (write-only phase off).
        let load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![alloc], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(g.nodes[s1 as usize].op, Op::Store(MemKind::Int));
        assert_eq!(g.nodes[s2 as usize].op, Op::Store(MemKind::Long));
    }

    // ── Widened DSE: load-alias refinement (DSE-WIDEN) ──────────────

    #[test]
    fn test_dse_removes_overwrite_across_unrelated_local_load() {
        // store A.f = 1; load B.f (B a DISTINCT local allocation); store A.f = 2
        // The intervening load reads a provably-different object, so it does not
        // observe A's first store — which is therefore still dead. A trailing
        // load of A keeps A "read" so the write-only phase doesn't also remove
        // the live store, isolating the overwrite property.
        let mut g = probe_graph();
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let b = g.add(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        let dead_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c1], None);
        // Load of the UNRELATED local allocation B — not a barrier for A.
        let _load_b = g.add(Op::Load(MemKind::Int), IrType::Int, vec![b], None);
        let live_store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c2], None);
        // Read A so the write-only phase keeps the live store.
        let load_a = g.add(Op::Load(MemKind::Int), IrType::Int, vec![a], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load_a], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[dead_store as usize].op,
            Op::Dead,
            "the first store to A is dead: the intervening load reads a distinct \
             local allocation B that cannot alias A"
        );
        assert_eq!(
            g.nodes[live_store as usize].op,
            Op::Store(MemKind::Int),
            "the overwriting (live) store to A must be kept"
        );
    }

    #[test]
    fn test_dse_keeps_overwrite_across_nonlocal_load() {
        // store A.f = 1; load P.f (P a Param — non-local, unknown provenance);
        // store A.f = 2. The load's base could alias A if A had escaped, so the
        // conservative path flushes all pending stores: the first store to A
        // must be KEPT. (Locks the non-local-load fallback of the refinement.)
        let mut g = probe_graph();
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let p = g.add(Op::Param(0), IrType::Ref, vec![], None);
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c1], None);
        // Load from a non-local base — conservatively a full barrier.
        let _load_p = g.add(Op::Load(MemKind::Int), IrType::Int, vec![p], None);
        let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![a, c2], None);
        let load_a = g.add(Op::Load(MemKind::Int), IrType::Int, vec![a], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load_a], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[s1 as usize].op,
            Op::Store(MemKind::Int),
            "a load from a non-local base may alias A, so the first store is kept"
        );
        assert_eq!(g.nodes[s2 as usize].op, Op::Store(MemKind::Int));
    }

    // ── Widened DSE: write-only, never-read local allocation ────────

    #[test]
    fn test_dse_removes_write_only_never_read_store() {
        // A local object whose field is written once and never read, with
        // nothing depending on the store, and the object never escaping:
        // the store is observably inert and must be removed. (Distinct from
        // the overwrite case: there is only ONE store, so the straight-line
        // overwrite phase cannot fire — only the write-only phase can.)
        let mut g = probe_graph();
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let c1 = g.add(Op::Const(7), IrType::Int, vec![], None);
        let store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
        // The method returns void; the alloc never escapes and is never read.
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[store as usize].op,
            Op::Dead,
            "a write-only, never-read store to a local alloc must be removed"
        );
    }

    #[test]
    fn test_dse_keeps_write_only_store_when_alloc_escapes() {
        // Same write-only shape, but the allocation escapes (returned to the
        // caller). The caller may read the field, so the store is observable
        // and must be kept — guards the kafka bug-25-class escape hole.
        let mut g = probe_graph();
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![g.entry],
            None,
        );
        let c1 = g.add(Op::Const(7), IrType::Int, vec![], None);
        let store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
        // The allocation is RETURNED → escapes.
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, alloc], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[store as usize].op,
            Op::Store(MemKind::Int),
            "a store into an escaping allocation may be observed externally and must be kept"
        );
    }

    // ── SCEV-driven LICM ────────────────────────────────────────────

    /// Hand-build a minimal counted-loop graph and return
    /// `(graph, region, preheader, iv_phi)`:
    ///
    /// ```text
    ///   start ─ c0(Proj0) ─ m0(Proj1)
    ///   region = Region[c0, back_ctrl]          (loop header)
    ///   iv     = Phi[region, init=Const0, back] (induction var, variant)
    ///   if     = If[region, cond]               (loop spine)
    ///   t/f    = Proj0/Proj1[if]
    ///   back_ctrl = Proj0 (loops to region)     (the back-edge control)
    /// ```
    ///
    /// The caller adds the node under test (an invariant or variant load)
    /// pinned into the body via a control input, then runs `licm`.
    fn loop_probe() -> (Graph, NodeId, NodeId, NodeId) {
        loop_probe_header(Op::Region)
    }

    /// As [`loop_probe`] but with a caller-chosen header op (`Op::Region` for
    /// the hand-built shape, `Op::Merge` for the real javac loop shape). The
    /// header's inputs are `[entry_pred=c0, back_edge_ctrl]`.
    fn loop_probe_header(header: Op) -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None); // preheader ctrl
        let _m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        // Header: [entry_pred=c0, back_edge_ctrl] — fill back-edge below.
        let region = g.add(header, IrType::Control, vec![c0], None);
        let init = g.add(Op::Const(0), IrType::Int, vec![], None);
        // Induction phi (loop-carried): [region, init, back_val]. back_val
        // filled after the increment exists.
        let iv = g.add(Op::Phi, IrType::Int, vec![region, init], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let iv_next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
        // Close the phi's back-edge value.
        g.nodes[iv as usize].inputs.push(iv_next);
        // Loop spine: If on a cond, with a back-edge Proj re-entering region.
        let bound = g.add(Op::Const(10), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], None);
        let if_node = g.add(Op::If, IrType::Control, vec![region, cond], None);
        let back_ctrl = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
        let _exit_ctrl = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
        // Wire the back-edge control into the region.
        g.nodes[region as usize].inputs.push(back_ctrl);
        (g, region, c0, iv)
    }

    /// Build a counted reduction loop `for (i=init; i<bound; i+=stride) acc += i;`
    /// Returns (graph, return_node); the Return's value input is the accumulator
    /// phi (its post-loop value), its control is the loop-exit projection.
    fn reduction_loop(init: i64, bound: i64, stride: i64) -> (Graph, NodeId) {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let region = g.add(Op::Region, IrType::Control, vec![c0], None);
        let i_init = g.add(Op::Const(init), IrType::Int, vec![], None);
        let iv = g.add(Op::Phi, IrType::Int, vec![region, i_init], None);
        let stride_c = g.add(Op::Const(stride), IrType::Int, vec![], None);
        let iv_next = g.add(Op::Add, IrType::Int, vec![iv, stride_c], None);
        g.nodes[iv as usize].inputs.push(iv_next);
        let s_init = g.add(Op::Const(0), IrType::Int, vec![], None);
        let acc = g.add(Op::Phi, IrType::Int, vec![region, s_init], None);
        let acc_next = g.add(Op::Add, IrType::Int, vec![acc, iv], None);
        g.nodes[acc as usize].inputs.push(acc_next);
        let bound_c = g.add(Op::Const(bound), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound_c], None);
        let if_node = g.add(Op::If, IrType::Control, vec![region, cond], None);
        let back = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
        g.nodes[region as usize].inputs.push(back);
        let ret = g.add(Op::Return, IrType::Void, vec![exit, acc], None);
        g.exit = ret;
        (g, ret)
    }

    fn fold_pipeline(g: &mut Graph) {
        for _ in 0..8 {
            let before = g.live_count();
            fold_constants(g);
            algebraic_simplify(g);
            gvn(g);
            eliminate_dead_nodes(g);
            if g.live_count() == before {
                break;
            }
        }
    }

    #[test]
    fn test_unroll_counted_reduction_folds_to_constant() {
        // for (i=0; i<5; i++) acc += i;  =>  acc == 0+1+2+3+4 == 10
        let (mut g, ret) = reduction_loop(0, 5, 1);
        assert!(
            unroll(&mut g),
            "a constant-trip counted reduction must unroll"
        );
        fold_pipeline(&mut g);
        let val = g.nodes[ret as usize].inputs[1];
        assert_eq!(
            g.nodes[val as usize].op,
            Op::Const(10),
            "unrolled sum of 0..5 must fold to the constant 10"
        );
        assert!(
            !g.nodes.iter().any(|n| n.op == Op::Region),
            "no loop region should remain after a full unroll"
        );
    }

    #[test]
    fn test_unroll_respects_trip_cap() {
        // Trip count 100 exceeds UNROLL_MAX_TRIP → must NOT unroll.
        let (mut g, _ret) = reduction_loop(0, 100, 1);
        assert!(!unroll(&mut g), "a loop above the trip cap must not unroll");
        assert!(
            g.nodes.iter().any(|n| n.op == Op::Region),
            "the loop region must remain when unroll bails"
        );
    }

    #[test]
    fn test_unroll_nonzero_init_and_stride() {
        // for (i=3; i<11; i+=2) acc += i;  =>  i ∈ {3,5,7,9}  =>  sum == 24
        let (mut g, ret) = reduction_loop(3, 11, 2);
        assert!(
            unroll(&mut g),
            "non-zero init/stride counted loop must unroll"
        );
        fold_pipeline(&mut g);
        let val = g.nodes[ret as usize].inputs[1];
        assert_eq!(g.nodes[val as usize].op, Op::Const(24), "3+5+7+9 == 24");
    }

    #[test]
    fn test_licm_hoists_invariant_load() {
        // An invariant load (base + address defined OUTSIDE the loop) pinned
        // into a barrier-free loop body must be re-anchored to the pre-header.
        let (mut g, region, preheader, _iv) = loop_probe();
        // Memory token and base are defined outside the loop (invariant).
        let mem = 2; // _m0 = Proj(1) of Start
        let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let off = g.add(Op::Const(4), IrType::Int, vec![], None);
        // Full-form load pinned into the body: ctrl = region (loop control).
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, base, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(
            changed,
            "an invariant load in a barrier-free loop must hoist"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the hoisted load's control input must be re-anchored to the pre-header"
        );
    }

    #[test]
    fn test_licm_does_not_hoist_variant_load() {
        // A load whose base is the loop-carried induction phi is loop-VARIANT
        // and must NOT be hoisted (its control input stays at the region).
        let (mut g, region, _preheader, iv) = loop_probe();
        let mem = 2;
        // Use the induction variable itself as the address — variant.
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, iv, NO_NODE],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let _ = licm(&mut g);

        assert_eq!(
            g.nodes[load as usize].inputs[0], region,
            "a loop-variant load must NOT be hoisted (control stays in the loop)"
        );
    }

    #[test]
    fn test_licm_bails_on_barrier_in_loop() {
        // A store inside the loop body is a memory barrier: even an otherwise
        // invariant load must NOT be hoisted (we have no alias oracle).
        let (mut g, region, _preheader, _iv) = loop_probe();
        let mem = 2;
        let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let off = g.add(Op::Const(4), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, base, off],
            None,
        );
        // A store pinned into the body (ctrl = region) is the barrier. Its
        // base is a *different* local alloc so DSE-style reasoning is moot;
        // for LICM any in-body store disqualifies hoisting.
        let other = g.add(
            Op::New {
                class_id: 9,
                num_fields: 1,
            },
            IrType::Ref,
            vec![region],
            None,
        );
        let cval = g.add(Op::Const(5), IrType::Int, vec![], None);
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, other, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(
            !changed,
            "a memory barrier in the loop body must block all load hoisting"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], region,
            "the load must stay in the loop when the body has a barrier"
        );
    }

    #[test]
    fn test_licm_hoists_invariant_load_merge_header() {
        // The production bytecode→IR builder emits javac loops with an
        // `Op::Merge` header (not `Op::Region`). LICM must recognise it and
        // hoist an invariant load just the same.
        let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
        let mem = 2;
        let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let off = g.add(Op::Const(4), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, base, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(
            changed,
            "an invariant load in a barrier-free Merge-header loop must hoist"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the hoisted load's control input must be re-anchored to the pre-header"
        );
    }

    #[test]
    fn test_licm_merge_header_classifies_entry_regardless_of_slot_order() {
        // A `Merge` header does not fix entry vs back-edge slot order. With the
        // back-edge moved to slot 0 and the entry to slot 1, LICM must still
        // classify the pre-header structurally (by control-reachability) and
        // hoist to the real entry — never to the back-edge at slot 0.
        let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
        // Swap the header's control inputs: [c0, back] -> [back, c0].
        g.nodes[region as usize].inputs.swap(0, 1);
        assert_ne!(
            g.nodes[region as usize].inputs[0], preheader,
            "precondition: the entry predecessor is no longer at input slot 0"
        );
        let mem = 2;
        let base = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let off = g.add(Op::Const(4), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, base, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(changed, "the invariant load must still hoist");
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the load must re-anchor to the structurally-classified entry (c0), \
             not to whatever sits at input slot 0 (the back-edge)"
        );
    }

    #[test]
    fn test_licm_scev_corroborates_counted_loop() {
        // Bridge sanity: the bytecode-level SCEV/loop-analysis corroboration
        // recognises a simple counted loop (`for i in 0..10`).
        // iconst_0; istore_1; [h:] iload_1; bipush 10; if_icmpge exit;
        // iinc 1,1; goto h; [exit:] return
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1            (header)
            0x10, 0x0a, // 3: bipush 10
            0xa2, 0x00, 0x08, // 5: if_icmpge +8 → 13
            0x84, 0x01, 0x01, // 8: iinc 1, 1
            0xa7, 0xff, 0xf7, // 11: goto -9 → 2
            0xb1, // 14: return
        ];
        assert!(
            licm_scev_corroborates(&code, code.len()),
            "SCEV must corroborate a simple counted loop"
        );
    }
}
