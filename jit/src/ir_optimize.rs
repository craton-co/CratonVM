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
    // Full unrolling of small constant-trip counted loops. Default-**ON**;
    // `CRATONVM_JIT_UNROLL=0` turns it off (`unroll_enabled` is
    // `map_or(true, ..)`). This comment said "Default-OFF ... while it soaks"
    // until 2026-08-04; it had outlived the soak. It matters more than a stale
    // comment usually does, because the parity audit in
    // `x64/single_pass_only.rs` decides whether the optimizing tier may take a
    // method by asking which transforms it has, and this one is the reason the
    // single-pass native unroller is NOT on that veto list. Runs after
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
    // SCEV-driven loop-invariant code motion. Default-**ON**;
    // `CRATONVM_JIT_LICM=0` turns it off (`licm_enabled` is `map_or(true, ..)`).
    // Same correction, same date, same reason as the unroll comment above: it
    // is why the single-pass `aaload`/FP hoists are not on the veto list in
    // `x64/single_pass_only.rs`. Runs once *after* the fixed-point cleanup so it sees already-folded /
    // GVN'd invariant expressions, then a final lightweight cleanup re-runs
    // GVN + DCE to dedup any anchor edges it rewrote.
    if licm_enabled() && licm(graph) {
        gvn(graph);
        eliminate_dead_nodes(graph);
    }
}

/// `true` (default) when the SCEV-driven loop-invariant code-motion pass runs.
/// Flipped default-ON after the increment-2 LICM pass soaked clean (bt10/14/16/18
/// + loop-heavy bench differential gate-ON ≡ gate-OFF == HotSpot, like the
/// `IR_CALL`/`IR_LONG`/`IR_FP` flips). `CRATONVM_JIT_LICM=0` is the opt-out that
/// restores the pre-flip (no-LICM) behaviour as the safety net.
pub fn licm_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_LICM").map_or(true, |v| v != "0")
    })
}

/// `true` when `CRATONVM_JIT_REASSOC` is set (cached). Enables the affine
/// strength-reduction pass and the matching IR-path gate relaxation.
pub fn reassoc_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_REASSOC").is_some())
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
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_NO_IR_BRANCHY").is_none())
}

// ── Affine strength reduction (reassociation) ────────────────────────
//
// An integer value is "affine in `root`" when it equals `k*root + c` for
// compile-time constants `k`, `c` (with `root == None` meaning a pure
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Affine {
    /// Root value this expression is affine in; `None` => pure constant.
    ///
    /// Deliberately an `Option<NodeId>` and *not* the [`NO_NODE`] sentinel.
    /// `NO_NODE` already carries one meaning — "this graph edge is absent" —
    /// and reusing the same bit pattern here for a second, value-domain
    /// meaning ("this expression has no root; it is a pure constant") made one
    /// `u32::MAX` stand for two incompatible things. A missed test then does
    /// not read as "absent", it reads as node `u32::MAX`, which is exactly the
    /// `nodes[u32::MAX]` panic `ir.rs`'s `NO_NODE` deprecation note describes.
    /// With an `Option` the compiler forces the case split at every read.
    root: Option<NodeId>,
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
            if a.root.is_none() {
                Affine {
                    root: b.root,
                    k: b.k,
                    c: w_add(is_int, b.c, a.c),
                }
            } else if b.root.is_none() {
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
            if b.root.is_none() {
                Affine {
                    root: a.root,
                    k: a.k,
                    c: w_sub(is_int, a.c, b.c),
                }
            } else if a.root.is_none() {
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
            if a.root.is_none() {
                if b.root.is_none() {
                    Affine {
                        root: None,
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
            } else if b.root.is_none() {
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
///
/// `root` is `None` for a pure constant (`k == 0`, value `c`). A non-zero `k`
/// with no root is not a representable value, so that combination yields
/// `None` and the caller leaves the node alone — rather than fabricating an
/// edge out of a sentinel, which is how a `Mul` with a `u32::MAX` input used to
/// become reachable.
fn build_affine(
    graph: &mut Graph,
    root: Option<NodeId>,
    k: i64,
    c: i64,
    ty: IrType,
) -> Option<NodeId> {
    if k == 0 {
        return Some(get_or_add_const(graph, c, ty));
    }
    let root = root?;
    let base = if k == 1 {
        root
    } else {
        let kc = get_or_add_const(graph, k, ty);
        graph.add(Op::Mul, ty, vec![root, kc], None)
    };
    Some(if c == 0 {
        base
    } else {
        let cc = get_or_add_const(graph, c, ty);
        graph.add(Op::Add, ty, vec![base, cc], None)
    })
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
            root: Some(id as NodeId),
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
                root: None,
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
        // Decide materialization while original inputs are intact. A form with
        // no root (`None` — a pure constant) has nothing to strength-reduce,
        // and a form rooted at the node itself (`Some(id)` — opaque) has
        // nothing to collapse; `filter` removes both without either case
        // hiding behind a sentinel comparison.
        if let Some(root) = res.root.filter(|&r| r != id as NodeId) {
            if is_int_arith(&node.op) {
                let has_arith_operand = node.inputs.iter().any(|&j| {
                    (j as usize) < id
                        && aff[j as usize].is_some_and(|fj| fj.root == Some(root))
                        && is_int_arith(&graph.nodes[j as usize].op)
                });
                materialize[id] = has_arith_operand;
            }
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
        // `None` here means the form was not materializable (a non-zero `k`
        // with no root). `materialize[id]` is only set for a form with a real
        // root, so this is unreachable today; leaving the node untouched is
        // the safe answer if that ever stops holding.
        let mat = match build_affine(graph, a.root, a.k, a.c, ty) {
            Some(m) => m,
            None => continue,
        };
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
        // Trivial-phi elimination (standard SSA simplification, Braun et al.):
        // a phi whose value inputs (slots 1..; slot 0 is the control anchor) are
        // all either the phi itself (a back-edge that re-feeds the same value —
        // i.e. a loop-carried local that is never reassigned) or a single other
        // value `v` collapses to `v`: its value cannot differ by predecessor.
        // This removes the redundant loop-header phis the IR builder inserts for
        // *invariant* locals (e.g. the receiver `b` in `for (..) acc += b.f`),
        // exposing the real `Param`/alloc base to LICM, GVN, and the alias
        // oracle — without it LICM's load hoisting is inert on production IR
        // (the base is always a region-anchored phi → judged loop-variant).
        // A phi with two distinct non-self value inputs (an induction phi
        // `φ(0, i+1)`, an accumulator, a real merge, a memory phi advanced by an
        // in-loop op) has `single` set twice → returns None, left untouched.
        Op::Phi => {
            let mut single = NO_NODE;
            for &v in inputs.iter().skip(1) {
                if v == id || v == NO_NODE {
                    continue; // self-reference (unchanged back-edge) / placeholder
                }
                if matches!(nodes[v as usize].op, Op::Dead) {
                    continue; // a stale dead input observes nothing
                }
                if single == NO_NODE {
                    single = v;
                } else if single != v {
                    return None; // genuine merge of distinct values
                }
            }
            if single != NO_NODE && single != id {
                Some(single)
            } else {
                None
            }
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
        // Reducible single-entry loop: one pre-header, >=1 back-edge.
        if entry_preds.len() == 1 && !back_ctrls.is_empty() {
            out.push((id, entry_preds[0], back_ctrls));
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

/// Control nodes `header` does **not** dominate: everything reachable from the
/// graph entry with `header` deleted.
///
/// One reachability walk, no dominator tree. It is the definition of dominance
/// read directly — `header` dominates `n` exactly when every path from the
/// entry to `n` goes through `header`, i.e. when deleting `header` makes `n`
/// unreachable — and it is what tells a NESTED INNER loop's back edge from its
/// pre-header, and its body from the enclosing method.
fn control_not_dominated_by(
    graph: &Graph,
    users: &[Vec<NodeId>],
    header: NodeId,
) -> FxHashSet<NodeId> {
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    if (graph.entry as usize) >= graph.nodes.len() {
        return seen;
    }
    seen.insert(graph.entry);
    let mut work = vec![graph.entry];
    while let Some(cur) = work.pop() {
        for &u in &users[cur as usize] {
            if u == header || seen.contains(&u) || !graph.nodes[u as usize].op.is_control() {
                continue;
            }
            seen.insert(u);
            work.push(u);
        }
    }
    seen
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
fn entry_preds_by_dominance(
    graph: &Graph,
    users: &[Vec<NodeId>],
    header: NodeId,
) -> (Vec<NodeId>, Vec<NodeId>) {
    let seen = control_not_dominated_by(graph, users, header);
    let mut entries = Vec::new();
    let mut backs = Vec::new();
    for &c in graph.nodes[header as usize].inputs.iter() {
        if c == NO_NODE {
            continue;
        }
        if seen.contains(&c) {
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
            // cov-07: `Op::Throw`, like `Op::Return`, always leaves the frame
            // and never flows control back into the loop body — exclude it
            // from the forward-reachable set for the same reason.
            if op.is_control() && !matches!(op, Op::Return | Op::Throw) {
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
    // memory input is a body node, or which is a Phi anchored at the region,
    // belongs to the body. Pure nodes are intentionally *not* pinned (they
    // float); their invariance is decided per use in `is_loop_invariant`.
    //
    // Run to a FIXED POINT rather than in one arena pass. A single pass is only
    // complete when every node's inputs have lower ids than the node itself:
    // node `X` pinned only through node `Y` is missed whenever `Y` sorts after
    // `X`. That ordering holds for the builder's forward-flowing control and
    // memory edges, but it is a *premise about the producer*, not a property of
    // the IR — a back-patched loop-carried edge breaks it, and so does any
    // future pass that rewires an input.
    //
    // The cost of being wrong is asymmetric and this is the expensive side. An
    // over-large body only loses hoists (more nodes look variant). A body that
    // MISSES a node is a body `loop_has_hard_barrier` and `loop_store_clobber`
    // never see — an unnoticed in-loop `Call` or `Store`, and a load hoisted
    // across it. Iterating removes the premise for one extra arena pass.
    loop {
        let before = body.len();
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
            // Loads / stores / calls / allocations: pinned to a control/mem
            // input, or to another node already pinned into the body.
            if node.inputs.iter().any(|&inp| body.contains(&inp)) {
                body.insert(id as NodeId);
            }
        }
        if body.len() == before {
            break;
        }
    }
    body
}

/// True if `id` produces a value that is constant across all iterations of the
/// loop with header `region` and body `body`. Conservative: any uncertainty
/// (out-of-range, Dead, a phi whose merge point is in the body, or membership
/// in the body) returns false. A phi whose control anchor is *outside* the loop
/// and whose value inputs are all invariant IS invariant (a merge decided once,
/// before the loop). `depth` bounds the recursion so a cyclic phi (e.g. from a
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
    if !graph.is_valid_id(id) {
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
        // A phi is loop-invariant iff its control anchor (the merge point at
        // input slot 0) is OUTSIDE the loop body — so the merge runs once, not
        // per iteration — AND every value input is itself invariant. A phi
        // anchored at this loop's region (induction / loop-carried) or at an
        // in-loop merge (an in-loop if/else join) has its anchor IN the body and
        // is variant. This is the cross-merge half of the alias oracle on the
        // *load* side: a base like `cond ? A : B` decided before the loop is a
        // hoistable invariant.
        Op::Phi => {
            // `input_opt` reads the control anchor as `None` when the slot is
            // missing *or* holds the placeholder — the two cases this arm
            // treats identically ("no anchor we can prove is outside").
            let anchor = match node.input_opt(0) {
                Some(a) => a,
                None => return false,
            };
            if body.contains(&anchor) {
                return false;
            }
            // Skip the control anchor (slot 0); every value input must be
            // invariant. (A self-referential value input bottoms out at the
            // depth bound → conservatively variant.)
            node.inputs.iter().skip(1).all(|&inp| {
                inp == NO_NODE || is_loop_invariant_d(graph, inp, region, body, depth + 1)
            })
        }
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

/// The memory state flowing INTO a loop, as seen at its header.
///
/// When the header carries a memory phi, that phi's input for the loop-entry
/// predecessor is the memory the pre-header ends with; when it does not, memory
/// is loop-invariant already and the caller's own invariance test answers
/// first. Used only to re-anchor a node whose alias class conflicts with no
/// store, so this is an ORDERING choice and never a claim that some other
/// memory state is equivalent.
fn loop_entry_memory(graph: &Graph, region: NodeId, entry_pred: NodeId) -> Option<NodeId> {
    let slot = graph.nodes[region as usize]
        .inputs
        .iter()
        .position(|&p| p == entry_pred)?;
    graph
        .nodes
        .iter()
        .find(|node| {
            node.op == Op::Phi
                && node.ty == IrType::Memory
                && node.inputs.first() == Some(&region)
        })
        // Phi inputs are `[region, v_for_pred0, v_for_pred1, …]`, aligned with
        // the region's own predecessor list.
        .and_then(|phi| phi.inputs.get(1 + slot).copied())
}

/// Can any node anchored at this control token raise an exception?
///
/// The question a pre-header hoist has to answer: moving a potentially-throwing
/// node into that block is only invisible if nothing already there could throw
/// first. Same classification `loop_has_hard_barrier` uses — not pure, not
/// control, not `Load`/`Store`/`Phi`/`Dead` — applied to one block instead of a
/// loop body.
///
/// Conservative in both directions that matter: a node this cannot see (one
/// scheduled into the block by `find_best_block` rather than pinned by its
/// control input) is a PURE node, which cannot throw; and a node it does see
/// and cannot classify counts as trapping.
fn preheader_may_trap(graph: &Graph, preheader: NodeId) -> bool {
    graph.nodes.iter().any(|node| {
        node.inputs.first() == Some(&preheader)
            && !node.op.is_pure()
            && !node.op.is_control()
            && !matches!(node.op, Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead)
    })
}

/// True if the loop body contains a *hard* memory barrier — a `Call`,
/// allocation (`New`/`NewArray`, which run constructor side effects), guard,
/// monitor, or any other non-pure node that is not a `Store`/`Load`/`Phi`/
/// control. A hard barrier may read or write arbitrary memory, so it
/// disqualifies ALL load hoisting for the loop.
///
/// In-loop `Store`s are deliberately NOT hard barriers: they are handled with
/// per-load alias precision by `loop_store_clobber` / `load_safe_past_clobber`
/// (a store to a local allocation distinct from a load's base cannot clobber
/// that load).
fn loop_has_hard_barrier(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    body.iter().any(|&id| {
        let op = &graph.nodes[id as usize].op;
        !op.is_pure()
            && !op.is_control()
            && !matches!(op, Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead)
    })
}

/// The same question asked about WRITES only, plus stores.
///
/// [`loop_has_hard_barrier`] disqualifies a loop from every hoist when its
/// body holds any impure non-control node outside a small allow-list. Three
/// of the nodes it therefore rejects write no memory at all:
///
/// * `Op::ArrayLoad` and `Op::ArrayLength` are **reads**. A read cannot
///   clobber the location a hoisted load reads, so it cannot make a hoist
///   value-unsafe. Each is impure only because it raises NPE on a null base
///   — a TRAP-ordering fact, not an aliasing one.
/// * `Op::Guard` carries no memory edge at all (`ir_schedule`: "its ordering
///   against a store is positional"). It transfers control to a deopt; it
///   writes nothing.
///
/// `Op::Store` is NOT exempt here, unlike in the strict predicate: the
/// restricted hoist this feeds does no alias analysis, so it wants a body
/// that writes nothing whatsoever rather than one whose writes it would have
/// to reason about. Everything else — `Op::ArrayStore`, `Op::Call`,
/// `Op::New`, `Op::NewArray`, `Op::LoadStatic`, the monitors — stays a
/// barrier for the same reason it always was.
///
/// # What this is for
///
/// The IR String-access expansion (`ir.rs::try_string_access_intrinsic`)
/// emits four `Op::Guard`s, two `Op::ArrayLoad`s and one `Op::ArrayLength`
/// into the very loop it wants its `String.value` / `String.coder` loads
/// hoisted OUT of — so it disqualified its own LICM, and `CRATONVM_DBG=licm`
/// reported `hard_barrier=true` with 0 hoisted on every candidate header of
/// `probes/CharAtCostCurve.java`. The same shape belongs to any counted loop
/// carrying a bounds-checked array read.
fn loop_writes_memory(graph: &Graph, body: &FxHashSet<NodeId>) -> bool {
    body.iter().any(|&id| {
        let op = &graph.nodes[id as usize].op;
        !op.is_pure()
            && !op.is_control()
            && !matches!(
                op,
                Op::Load(_)
                    | Op::Phi
                    | Op::Dead
                    | Op::Guard { .. }
                    | Op::ArrayLoad(_)
                    | Op::ArrayLength
            )
    })
}

/// `CRATONVM_JIT_NO_LICM_READ_HOIST=1` — turn off the restricted invariant-
/// load hoist for a loop that only its own reads and guards disqualify.
///
/// Default ON. The B arm of an in-binary A/B: with this set,
/// `probes/CharAtCostCurve.java` with the String-intrinsic pin off returns to
/// re-reading `String.value` and `String.coder` per character, and every
/// other arm is unchanged.
fn licm_read_hoist_enabled() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_LICM_READ_HOIST").is_none()
}

/// Points-to summary of a reference node, used by the LICM alias oracle to
/// reason about loads/stores whose base flows through a `Phi` (cross-merge).
struct RefPointsTo {
    /// The fresh local allocations this reference may be, or `None` if it may be
    /// a value of unknown provenance (a loaded ref, a call result, …) — which
    /// could alias anything.
    allocs: Option<FxHashSet<NodeId>>,
    /// Whether it may be a *pre-existing* reference (a method parameter /
    /// `this`) — never a fresh in-method allocation.
    has_pre: bool,
}

/// Resolve what a reference node points to, following `Phi`s transparently (the
/// "cross-merge" join). Leaves:
///   * `New`/`NewArray` → exactly that fresh allocation.
///   * `Param`           → a pre-existing reference (no allocation).
///   * anything else      → unknown provenance.
///
/// A `Phi` joins its value inputs (the control anchor at its head is skipped):
/// the allocation sets union, `has_pre` ORs, and any unknown input poisons the
/// alloc set to `None`. Recursion is depth-bounded, so a loop-carried phi cycle
/// resolves to unknown rather than hanging.
fn resolve_ref_points_to(graph: &Graph, id: NodeId, depth: u32) -> RefPointsTo {
    let unknown = RefPointsTo {
        allocs: None,
        has_pre: false,
    };
    if !graph.is_valid_id(id) || depth > 32 {
        return unknown;
    }
    match &graph.nodes[id as usize].op {
        Op::New { .. } | Op::NewArray { .. } => {
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
            let mut allocs: Option<FxHashSet<NodeId>> = Some(FxHashSet::default());
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
                let pt = resolve_ref_points_to(graph, inp, depth + 1);
                has_pre |= pt.has_pre;
                allocs = match (allocs, pt.allocs) {
                    (Some(mut a), Some(b)) => {
                        a.extend(b);
                        Some(a)
                    }
                    _ => None, // any unknown input poisons the join
                };
            }
            if !saw_value {
                return unknown;
            }
            RefPointsTo { allocs, has_pre }
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
fn loop_store_clobber(graph: &Graph, body: &FxHashSet<NodeId>) -> Option<FxHashSet<NodeId>> {
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
fn load_safe_past_clobber(graph: &Graph, load_base: NodeId, clobbered: &FxHashSet<NodeId>) -> bool {
    match resolve_ref_points_to(graph, load_base, 0).allocs {
        Some(set) => set.is_disjoint(clobbered),
        None => false,
    }
}

/// Run loop-invariant code motion. Returns `true` if it changed the graph.
///
/// See the module-level soundness model. Hoists invariant loads out of each
/// natural loop — `Op::Region` *or* a javac back-edge `Op::Merge` header (see
/// `loop_headers`) — into the loop pre-header (the structurally-classified
/// entry-predecessor control). A hard barrier (call / allocation / guard /
/// monitor) in the body blocks all hoisting; an in-loop `Store` blocks only the
/// loads it could alias (`loop_store_clobber` / `load_safe_past_clobber` — a
/// store to a distinct local allocation cannot clobber a load of another). Pure
/// invariant nodes
/// already float in Sea-of-Nodes, so no placement change is needed for them —
/// but recognising them lets the trailing GVN pass dedup loop-entry vs loop-body
/// copies. The store-alias oracle resolves bases through `Phi`s (cross-merge),
/// so a store via `(cond ? A : B)` is modelled as writing `{A, B}`.
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

    // Non-vacuity diagnostic (mirrors `CRATONVM_DBG_UNROLL`): count the loads
    // this pass actually hoists, so a live soak can confirm LICM fired rather
    // than silently bailing every loop.
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LICM").is_some();
    let mut hoisted = 0usize;
    if dbg {
        eprintln!("[DBG_LICM] {} candidate loop header(s)", headers.len());
    }

    for (region, entry_pred, back_ctrls) in headers {
        // Re-validate: the header may have been killed by an earlier hoist's
        // cleanup in this same loop set (defensive).
        if !matches!(graph.nodes[region as usize].op, Op::Region | Op::Merge) {
            continue;
        }
        let body = loop_body(graph, region, entry_pred, &back_ctrls);
        if dbg {
            let nloads = body
                .iter()
                .filter(|&&id| matches!(graph.nodes[id as usize].op, Op::Load(_)))
                .count();
            eprintln!(
                "[DBG_LICM] header {region}: body {} node(s), {} load(s), \
                 hard_barrier={} writes_memory={}",
                body.len(),
                nloads,
                loop_has_hard_barrier(graph, &body),
                loop_writes_memory(graph, &body)
            );
        }
        // The pre-header is the classified loop-entry predecessor — NOT
        // necessarily input slot 0, since a `Merge` header may carry the
        // back-edge at either slot.
        let preheader = entry_pred;
        // ── Loop-invariant `Op::ArrayLength` ────────────────────────────────
        //
        // Run BEFORE the pre-header guard and the hard-barrier bail below, and
        // deliberately so on both counts. Taking the barrier first: an
        // in-loop `ArrayLength` IS one of those barriers (it is not pure, and
        // it is not a `Load`/`Store`/`Phi`), so a javac counted loop —
        // `for (i = 0; i < a.length; i++)` — disqualified its own LICM by the
        // very node this hoists. Measured on `probes/ArrayElemLoadCost.java`
        // with `CRATONVM_DBG_LICM=1`: every candidate header reported
        // `hard_barrier=true, 0 load(s)`, so the pass did nothing at all on the
        // one loop shape it most needed to.
        //
        // What makes this hoistable when the general load hoist below is
        // blocked:
        //
        // * **The value cannot change.** Nothing writes an array's length, so
        //   `AliasClass::ArrayLength` conflicts with no store (see the alias
        //   oracle's own note in `ir.rs`). The memory token is therefore pure
        //   ordering for this node, and re-anchoring it to the loop's ENTRY
        //   memory is value-preserving rather than a claim about aliasing.
        //   Without re-anchoring the memory the hoist would be inert:
        //   `ir_schedule::find_best_block` places a data node in the deepest
        //   block dominated by ALL its input blocks, so a node still reading
        //   the loop's memory phi stays in the loop no matter where its control
        //   points.
        //
        // * **The throw point does not move.** `ArrayLength` raises NPE on a
        //   null receiver, which is why it is impure. It is only taken when its
        //   control input IS the loop header region — i.e. it executes on every
        //   entry to the header, so in particular on the first — and the
        //   pre-header runs immediately before that first execution with
        //   nothing observable in between. `preheader_may_trap` refuses the
        //   remaining case, a pre-header block that can itself throw, where
        //   hoisting could report the NPE ahead of an exception that came first.
        let al_ids: Vec<NodeId> = body
            .iter()
            .copied()
            .filter(|&id| matches!(graph.nodes[id as usize].op, Op::ArrayLength))
            .collect();
        if !al_ids.is_empty() && !preheader_may_trap(graph, preheader) {
            let entry_mem = loop_entry_memory(graph, region, entry_pred);
            for al in al_ids {
                let inputs = graph.nodes[al as usize].inputs.clone();
                // Only the full `[ctrl, mem, array]` form has a control slot to
                // repoint; the compact EA-bridge form is already schedulable by
                // invariance alone.
                if inputs.len() < 3 {
                    continue;
                }
                if inputs[0] != region {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] arraylength {al}: skip — control {} is not the header {region}",
                            inputs[0]
                        );
                    }
                    continue;
                }
                let array = inputs[2];
                if !is_loop_invariant(graph, array, region, &body) {
                    if dbg {
                        eprintln!("[DBG_LICM] arraylength {al}: skip — array {array} variant");
                    }
                    continue;
                }
                let new_mem = if is_loop_invariant(graph, inputs[1], region, &body) {
                    inputs[1]
                } else {
                    match entry_mem {
                        Some(m) => m,
                        None => {
                            if dbg {
                                eprintln!(
                                    "[DBG_LICM] arraylength {al}: skip — no loop-entry memory"
                                );
                            }
                            continue;
                        }
                    }
                };
                if dbg {
                    eprintln!(
                        "[DBG_LICM] arraylength {al} (inputs {inputs:?}): HOIST to preheader \
                         {preheader}, mem {} -> {new_mem}",
                        inputs[1]
                    );
                }
                graph.nodes[al as usize].inputs[0] = preheader;
                graph.nodes[al as usize].inputs[1] = new_mem;
                changed = true;
                hoisted += 1;
            }
        }

        // ── Restricted invariant `Op::Load` hoist ───────────────────────────
        //
        // For a loop the STRICT rule refuses and `loop_writes_memory` clears:
        // its body reads and guards, and writes nothing. Placed here, beside
        // the `Op::ArrayLength` arm and above the pre-header guard below, for
        // the same reason that arm is: for a nested INNER loop `body`
        // over-approximates and drags the enclosing loop's pre-header in with
        // it, and `loop_headers` has already established structurally that
        // `entry_pred` is outside the natural loop. Over-approximation is the
        // safe direction for every test below — a bigger `body` makes
        // `is_loop_invariant` stricter and `header_derefs` smaller, so it can
        // only refuse a hoist, never admit a wrong one.
        //
        // Three obligations, the same three the `ArrayLength` arm discharges:
        //
        // 1. **The value cannot change.** Nothing in the body writes memory
        //    (`loop_writes_memory`), and the base is loop-invariant. So the
        //    load reads the same location and the same value on every
        //    iteration, and computing it once at the pre-header is
        //    value-preserving rather than a claim about aliasing.
        //
        // 2. **The throw point does not move.** An `Op::Load` deopts (it does
        //    not fault) on a null base, and a deopt raised from the pre-header
        //    would rebuild an interpreter frame at a bci the loop never
        //    reached. So the base must already be dereferenced on entry to the
        //    header — either because this load IS control-anchored there (the
        //    `ArrayLength` arm's own condition) or because some other
        //    header-anchored load/`arraylength` reads the same base.
        //    `preheader_may_trap` refuses the remaining case, a pre-header
        //    that can itself throw.
        //
        //    For the shape this exists for that condition is exactly met: a
        //    counted `for (i = 0; i < s.length(); i++)` expands `s.length()`
        //    at the header, off the same receiver `s.charAt(i)` uses.
        //
        // 3. **The memory token is re-anchored.** Without it the hoist is
        //    INERT whenever the body threads memory through a read:
        //    `ir_schedule::find_best_block` places a data node in the deepest
        //    block dominated by all its inputs, so a load still reading the
        //    loop's memory phi stays in the loop however its control points.
        if licm_read_hoist_enabled()
            && preheader != NO_NODE
            && loop_has_hard_barrier(graph, &body)
            && !loop_writes_memory(graph, &body)
            && !preheader_may_trap(graph, preheader)
        {
            let header_derefs: FxHashSet<NodeId> = body
                .iter()
                .copied()
                .filter_map(|id| {
                    let n = &graph.nodes[id as usize];
                    if !matches!(n.op, Op::Load(_) | Op::ArrayLength) {
                        return None;
                    }
                    if n.inputs.len() < 3 || n.inputs[0] != region {
                        return None;
                    }
                    Some(n.inputs[2])
                })
                .collect();
            let entry_mem = loop_entry_memory(graph, region, entry_pred);
            let read_load_ids: Vec<NodeId> = body
                .iter()
                .copied()
                .filter(|&id| matches!(graph.nodes[id as usize].op, Op::Load(_)))
                .collect();
            for load in read_load_ids {
                let inputs = graph.nodes[load as usize].inputs.clone();
                // Full `[ctrl, mem, base, offset?]` form only: the compact
                // EA-bridge shapes carry no control or memory slot to move,
                // and this arm's whole content is moving those two.
                if inputs.len() < 3 {
                    continue;
                }
                let base = inputs[2];
                let addr = if inputs.len() >= 4 { inputs[3] } else { NO_NODE };
                if !is_loop_invariant(graph, base, region, &body) {
                    if dbg {
                        eprintln!("[DBG_LICM] read-hoist load {load}: skip — base {base} variant");
                    }
                    continue;
                }
                if addr != NO_NODE && !is_loop_invariant(graph, addr, region, &body) {
                    if dbg {
                        eprintln!("[DBG_LICM] read-hoist load {load}: skip — addr {addr} variant");
                    }
                    continue;
                }
                if inputs[0] != region && !header_derefs.contains(&base) {
                    if dbg {
                        eprintln!(
                            "[DBG_LICM] read-hoist load {load}: skip — base {base} is not \
                             dereferenced at the header"
                        );
                    }
                    continue;
                }
                let new_mem = if is_loop_invariant(graph, inputs[1], region, &body) {
                    inputs[1]
                } else {
                    match entry_mem {
                        Some(m) => m,
                        None => {
                            if dbg {
                                eprintln!(
                                    "[DBG_LICM] read-hoist load {load}: skip — no loop-entry memory"
                                );
                            }
                            continue;
                        }
                    }
                };
                if inputs[0] == preheader && inputs[1] == new_mem {
                    continue;
                }
                if dbg {
                    eprintln!(
                        "[DBG_LICM] read-hoist load {load} (inputs {inputs:?}): HOIST to \
                         preheader {preheader}, mem {} -> {new_mem}",
                        inputs[1]
                    );
                }
                graph.nodes[load as usize].inputs[0] = preheader;
                graph.nodes[load as usize].inputs[1] = new_mem;
                changed = true;
                hoisted += 1;
            }
        }

        if preheader == NO_NODE || body.contains(&preheader) {
            // No identifiable pre-header outside the loop → cannot hoist.
            //
            // Runs AFTER the `ArrayLength` hoist above, and deliberately: for a
            // nested inner loop `body` over-approximates badly (the sweep drags
            // the whole enclosing loop in behind the inner exit, pre-header
            // included), and `loop_headers` has ALREADY established that
            // `entry_pred` is outside the natural loop — by reachability, or by
            // dominance for the inner-loop case it added. This guard is a
            // belt-and-braces check against that over-approximation, not the
            // structural claim, so the arm above does not owe it. The general
            // load hoist below still does: it reasons about aliasing against a
            // body it must not under-read.
            continue;
        }

        // A hard barrier (call / allocation / guard / monitor) could read or
        // write arbitrary memory → no hoisting from this loop at all.
        if loop_has_hard_barrier(graph, &body) {
            continue;
        }
        // An in-loop store does NOT bail the whole loop: a load past it is
        // gated per-load by the alias oracle below. Summarise the stores once —
        // the set of local allocations they may write, or `None` if any store is
        // opaque (then no load hoists past them). Only paid when a store exists.
        let body_has_store = body
            .iter()
            .any(|&id| matches!(graph.nodes[id as usize].op, Op::Store(_)));
        let store_clobber = if body_has_store {
            loop_store_clobber(graph, &body)
        } else {
            None
        };

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
                if dbg {
                    eprintln!(
                        "[DBG_LICM] load {load} (inputs {inputs:?}): skip — base {base} variant ({:?})",
                        graph.nodes[base as usize].op
                    );
                }
                continue;
            }
            if addr != NO_NODE && !is_loop_invariant(graph, addr, region, &body) {
                if dbg {
                    eprintln!("[DBG_LICM] load {load}: skip — addr {addr} variant");
                }
                continue;
            }
            // If the body has in-loop stores, the load may only hoist past them
            // when it cannot alias any of them: an opaque store (`None`) blocks
            // every load, otherwise the load's points-to alloc set must be
            // disjoint from what the stores write. Without a store this check is
            // skipped (pure invariant-load loop — the historical case).
            if body_has_store {
                let safe = match &store_clobber {
                    Some(clob) => load_safe_past_clobber(graph, base, clob),
                    None => false,
                };
                if !safe {
                    if dbg {
                        eprintln!("[DBG_LICM] load {load}: skip — may alias in-loop store");
                    }
                    continue;
                }
            }
            if dbg {
                eprintln!(
                    "[DBG_LICM] load {load} (inputs {inputs:?}): HOIST base {base} addr {addr}"
                );
            }
            // The load is invariant and not clobbered by any in-loop store → safe
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
                    hoisted += 1;
                }
            } else {
                // Compact form: nothing to repoint, but mark changed so the
                // trailing GVN dedups identical hoisted loads to one.
                changed = true;
                hoisted += 1;
            }
        }
    }
    if dbg && hoisted > 0 {
        eprintln!("[DBG_LICM] hoisted {hoisted} invariant load(s) to loop pre-header(s)");
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
//   * The location key uses the *base node id*, the *index/offset node id*,
//     and the `MemKind`.  Two stores match only when all three are
//     structurally identical.  Because the base is a single SSA allocation
//     node, equal base ids are the same object; differing `MemKind`/idx are
//     different slots and never matched.
//
//     A store whose layout carries NO offset operand (`idx == NO_NODE`) does
//     not name a field at all, and is therefore not matchable in either
//     direction — see `store_matchable_location`.  `MemKind` is an access
//     *width*, not a field index: two distinct `int` fields of the same object
//     share it, so keying on it alone conflates them.  The single exception is
//     an allocation with exactly one field, where "the object's storage" IS
//     that field.  This is the may-alias/must-alias distinction: the memory
//     model calls the no-offset case `ir::AccessOffset::Absent` and defines it
//     as a MAY-alias, and a deletion needs a MUST-alias.
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
    matches!(
        graph.node_opt(id),
        Some(n) if matches!(n.op, Op::New { .. } | Op::NewArray { .. })
    )
}

/// True if `id` allocates an object with exactly **one** instance field.
///
/// This is the one shape in which a store carrying *no offset operand* still
/// names a definite cell: with a single field, "the object's storage" and
/// "field 0" are the same location, so two such stores to the same base must
/// alias. See [`store_matchable_location`].
///
/// `Op::NewArray` never qualifies — an element count is not a field count, and
/// an absent index names no element.
fn alloc_has_single_field(graph: &Graph, id: NodeId) -> bool {
    matches!(
        graph.node_opt(id),
        Some(n) if matches!(n.op, Op::New { num_fields: 1, .. })
    )
}

/// True when a store's `(base, idx)` pair names a cell precisely enough that a
/// later store to the *same key* is a proved overwrite — a **must**-alias, not
/// a may-alias.
///
/// Deleting a store needs the strong direction. The offset operand is what
/// carries the field identity, and two of the three `Op::Store` layouts
/// `store_operands` accepts do not have one:
///
/// * compact `[base, value]` (EA-bridge / hand-built), and
/// * full-without-offset `[ctrl, mem, base, value]`
///
/// both yield `idx == NO_NODE`. `ir::AccessOffset::Absent` — the memory model's
/// name for exactly this — is documented as *"the object's storage … never
/// disjoint from another access to the same base"*, i.e. a **may**-alias. Two
/// such stores into **different fields** of the same object nonetheless share
/// the location key `(base, NO_NODE, kind)` whenever their `MemKind` widths
/// agree, and `o.x = 1; o.y = 2;` would delete the live `o.x = 1`.
///
/// The production builder always emits the 5-input `[ctrl, mem, base, offset,
/// value]` form with a distinct `Const(field_index)` offset (`ir.rs`, the
/// `putfield` arm), so this refusal costs nothing today; it removes the
/// premise, which is the part that was not established.
fn store_matchable_location(graph: &Graph, base: NodeId, idx: NodeId) -> bool {
    idx != NO_NODE || alloc_has_single_field(graph, base)
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

// ── Memory-token chain surgery ───────────────────────────────────────
//
// Every memory-effecting node carries an incoming *memory token* naming the
// previous writer, and produces itself as the token for the next one. That
// chain is the only ordering relation the IR has between two writes: the
// scheduler is otherwise free to place them in either order.
//
// `Graph::kill` alone therefore cannot remove a node that sits in the chain.
// It clears the killed node's own inputs but leaves every *consumer's* token
// slot naming it, so the next memory operation's token points at an `Op::Dead`
// node and has lost the transitive dependency on everything that wrote before
// the deleted store. That is the defect `ir_verify`'s ordering lane reports as
// "takes its memory token from removed node nN"; it is a wrong-code risk (a
// hoisted store), not a tidiness complaint. The fix is to *splice*: rewire the
// consumers to the killed node's own incoming token first, then kill.

/// The input slot carrying `node`'s incoming memory token, or `None` when this
/// op does not consume one.
///
/// This must agree with `ir_verify::is_memory_token_input`, which is the
/// predicate the verifier's ordering lane uses to decide whether a slot
/// pointing at a removed node is a broken chain: a splice that disagreed would
/// leave exactly the edge the verifier checks pointing at a dead node.
///
/// The op → slot mapping is the `[ctrl, mem, …]` layout documented on
/// `ir::Op`, so the token is always slot 1. The arity guard is what
/// distinguishes that documented full form from the compact hand-built /
/// EA-bridge layouts `store_operands` and `load_base` also accept (e.g. a
/// compact `Store` is `[base, value]`, whose slot 1 is a *value*, not a
/// token); each entry is the minimum input count of the full form.
/// `Op::ArrayLength` is `[ctrl, mem, array_ref]` per `ir::Op` and is
/// recognised here even though the verifier does not classify it — being the
/// stricter of the two can only make this side *refuse* a splice, never
/// perform a wrong one.
pub(crate) fn memory_token_slot(node: &Node) -> Option<usize> {
    // Delegates to the single table (`ir::Op::memory_shape`).
    //
    // This used to be a hand-maintained copy of the arity list, and a third
    // copy lived in `lib.rs`. They were only ever *numerically* identical, and
    // the moment `Op::MonitorEnter`/`MonitorExit` joined the table the copies
    // began answering `None` for a real token slot — which would have let dead-
    // store elimination and the EA applier treat a monitor's memory edge as a
    // value and splice the chain wrongly.
    crate::ir::memory_token_slot(node)
}

/// True when input `idx` of `node` is a memory *token* — an ordering edge
/// naming the previous writer — rather than a value the node reads.
///
/// A memory-typed φ is the one op whose token edges are not a single slot:
/// *every* value input (slots 1.., past the control anchor) is a token from
/// one predecessor.
pub(crate) fn is_memory_token_slot(node: &Node, idx: usize) -> bool {
    if matches!(node.op, Op::Phi) {
        return node.ty == IrType::Memory && idx >= 1;
    }
    memory_token_slot(node) == Some(idx)
}

/// Kill `store`, first splicing it out of the memory-token chain: every
/// consumer that named it as a token is rewired to the store's *own* incoming
/// token, so the chain closes over the hole instead of pointing into it.
///
/// Returns `false` — leaving the store **alive** — when the splice cannot be
/// done safely:
///
/// * the store is in a compact (`[base, value]`) layout, so it has no incoming
///   token to hand on, yet something names it;
/// * its incoming token is absent, out of range, or itself already dead, so
///   splicing would just relocate the break;
/// * something names it in a slot that is *not* a memory token (a value input,
///   a safepoint slot). A `Store` produces no value, so such a reference is
///   already malformed — rewiring it to a memory token would replace one bug
///   with a quieter one.
///
/// A store we fail to delete is a missed optimization. A store deleted out of a
/// chain we could not repair is wrong code, so the bias is deliberate.
fn kill_store_splicing_memory_chain(graph: &mut Graph, store: NodeId) -> bool {
    let (is_store, token) = match graph.node_opt(store) {
        Some(n) => (
            matches!(n.op, Op::Store(_)),
            memory_token_slot(n).and_then(|slot| n.input_opt(slot)),
        ),
        None => return false,
    };
    if !is_store {
        return false;
    }

    // Who still names this store, and do they all name it as a token?
    let mut has_consumer = false;
    let mut spliceable = true;
    for n in &graph.nodes {
        if n.op == Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp != store {
                continue;
            }
            has_consumer = true;
            if !is_memory_token_slot(n, i) {
                spliceable = false;
            }
        }
    }
    for sp in &graph.safepoints {
        if sp.locals.iter().chain(sp.stack.iter()).any(|&v| v == store) {
            // A frame state naming a store is already nonsense — a store
            // produces no value an interpreter frame can be rebuilt from — but
            // it is not this pass's business to silently retarget it.
            has_consumer = true;
            spliceable = false;
        }
    }

    // Nothing names it: the chain does not run through this node, so the plain
    // kill is already correct (this is the common hand-built / compact case).
    if !has_consumer {
        graph.kill(store);
        return true;
    }
    if !spliceable {
        return false;
    }
    let token = match token {
        Some(t) => t,
        None => return false,
    };
    // Splicing onto an already-dead token would just move the break along the
    // chain. (Cannot happen when this pass kills a run of chained stores in
    // ascending id order — each splice rewrites the next one's token slot to a
    // live node before we get there — but the check is cheap and the invariant
    // is not local.)
    if !matches!(graph.node_opt(token), Some(n) if n.op != Op::Dead) {
        return false;
    }

    graph.replace_all_uses(store, token);
    graph.kill(store);
    true
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

                // MUST-ALIAS, NOT MAY-ALIAS. A layout with no offset operand
                // does not name a field, so two such stores to the same base
                // may be two DIFFERENT cells (see `store_matchable_location`).
                // Such a store neither matches a pending store nor becomes one
                // — but it is NOT a barrier either: a store observes nothing,
                // so it cannot make an earlier pending store live.
                if !store_matchable_location(graph, base, idx) {
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

    // Splice each dead store out of the memory-token chain before killing it —
    // a plain `Graph::kill` here would leave the *next* memory operation's
    // token slot naming an `Op::Dead` node, which is how the surviving store
    // loses its transitive ordering edges. Ascending id order so that when two
    // stores in this batch are adjacent in the chain, the earlier one is
    // spliced first and the later one's token slot already names a live node
    // by the time we reach it. `sort`/`dedup`: the pending set is popped in
    // location order, not id order, so the raw vector is neither.
    to_kill.sort_unstable();
    to_kill.dedup();
    for id in to_kill {
        kill_store_splicing_memory_chain(graph, id);
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
    // `uses[id] == 0` above already means nothing names these stores, so the
    // splice is a no-op kill — except that `use_counts` does not see safepoint
    // slots, and the splice helper does. Routing through it keeps the one
    // reference class this phase cannot count from being severed silently.
    for id in to_kill {
        kill_store_splicing_memory_chain(graph, id);
    }
}

// ── Dead Node Elimination ────────────────────────────────────────────

/// Remove nodes not reachable from the exit (Return), from any other
/// side-effecting root, or from a safepoint snapshot.
fn eliminate_dead_nodes(graph: &mut Graph) {
    if graph.exit == NO_NODE {
        return;
    }

    // ── Defensive normalisation of already-stale snapshot slots ────────
    //
    // A slot may name a node that some *earlier* pass removed without routing
    // its safepoint references — historically this pass itself, which did not
    // root snapshots at all, so every optimized graph accumulated them. Such a
    // slot cannot be rescued: the value it named is already gone, and seeding
    // it as a root below would only resurrect an `Op::Dead` node. Normalise it
    // to `NO_NODE`, which the model already defines as "undefined at this bci"
    // and which `ir_lower` resolves to `FrameValue::Undefined` — the same
    // answer deopt would have reached anyway, but reached explicitly instead
    // of by indexing a removed node.
    //
    // This is emphatically NOT the fix for the live case handled below: a
    // value that is dead except for its snapshot reference is *kept*, never
    // cleared. Clearing that one would silently lose an interpreter local.
    if !graph.safepoints.is_empty() {
        let node_is_live: Vec<bool> = graph.nodes.iter().map(|n| n.op != Op::Dead).collect();
        for sp in &mut graph.safepoints {
            for slot in sp.locals.iter_mut().chain(sp.stack.iter_mut()) {
                if *slot == NO_NODE {
                    continue;
                }
                if !node_is_live.get(*slot as usize).copied().unwrap_or(false) {
                    *slot = NO_NODE;
                }
            }
        }
    }

    let mut reachable: FxHashSet<NodeId> = FxHashSet::default();
    // Seed the worklist with EVERY `Op::Return`, not just `graph.exit`. A method
    // with multiple `return` statements (e.g. `if (c) return A; return B;`)
    // builds several `Op::Return` terminators, but `graph.exit` records only the
    // last one — each `ireturn` overwrites it in the builder. Rooting DCE solely
    // from `graph.exit` would mark every OTHER return path unreachable and delete
    // it (control, value, and the `If`'s opposite projection), collapsing the
    // conditional into a single-successor branch that always takes the surviving
    // return. Every Return is an observable program exit and must be a root.
    // Seed from every `Op::Return`, every `Op::Store`, AND every `Op::Call`. A
    // store or call is an observable side effect whose result (a memory token /
    // return value) may be consumed by no one — a pure-write `o.x = v; return
    // v;` returns the value, not the store; a void static call
    // (`Foo.sideEffect(); return;`) has no value consumer at all — so the node
    // is unreachable from any Return. Rooting it (and, transitively, the memory
    // chain it depends on, including its argument values) keeps the effect from
    // being deleted. Strictly additive: a store removed by DSE is `Op::Dead` and
    // not matched, and a live store/call always has an observable effect.
    // `Op::ArrayLoad`/`Op::ArrayStore` are also side-effecting roots: BOTH can
    // throw (NullPointerException / ArrayIndexOutOfBoundsException via the
    // lowerer's deopt guards), an observable effect, so neither may be deleted
    // even when its result/memory token has no consumer (a `float v = a[i];`
    // whose `v` is unused still performs the bounds check).
    //
    // COV-02 adds `Op::ArrayLength` for that reason and no other. It is a pure
    // read of immutable storage — but it throws NullPointerException on a null
    // array through the same deopt guard, and that throw is the observable
    // effect. `int n = a.length;` with `n` unused still has to NPE; deleting it
    // is not a faster answer, it is a dropped exception.
    let mut worklist: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            matches!(
                n.op,
                Op::Return
                    // cov-07: a throw is an observable program exit exactly
                    // like a return — see the seeding comment above `Op::
                    // Return`. Its exception-ref INPUT is what must stay
                    // reachable (deleting the throw would silently turn
                    // `throw e;` into nothing).
                    | Op::Throw
                    | Op::Store(_)
                    | Op::Call { .. }
                    // cov-01: all three are observable side effects whose value
                    // may have no consumer. `ldc <Class>` and `getstatic` can
                    // run `<clinit>` and throw; `ldc <String>` interns, which
                    // is observable through `==` on a later literal. Deleting
                    // one because nothing reads its result would drop the
                    // class initialisation Java owes at that bytecode.
                    | Op::ConstString { .. }
                    | Op::ConstClass { .. }
                    | Op::LoadStatic { .. }
                    | Op::ArrayLoad(_)
                    | Op::ArrayStore(_)
                    | Op::ArrayLength
                    // Monitors are observable side effects and must be roots.
                    // Without this, DCE deletes a `monitorenter` whose result
                    // nobody reads — lock elision by liveness sweep, with no
                    // plan, no escape proof and no balance check, which is
                    // exactly what `escape_analysis`' all-or-nothing elision
                    // exists to prevent. Deleting only one of a pair is an
                    // IllegalMonitorStateException.
                    | Op::MonitorEnter
                    | Op::MonitorExit
                    // A guard's whole purpose is the deopt it takes when its
                    // condition fails; it produces no value, so nothing else
                    // roots it. Deleting the zero-divisor guard the builder
                    // anchors at an `idiv`/`ldiv` would silently drop the
                    // ArithmeticException that division owes — see
                    // `IrBuilder::add_div_zero_guard`.
                    | Op::Guard { .. }
            )
        })
        .map(|(id, _)| id as NodeId)
        .collect();
    if worklist.is_empty() {
        worklist.push(graph.exit);
    }

    // real-frame-deopt (#5): every value a safepoint snapshot names IS a DCE
    // root. A snapshot slot is what deopt rebuilds an interpreter local or
    // operand-stack entry from, so a slot naming a node this pass removed is a
    // deoptimization correctness bug — the frame is reconstructed from a node
    // that no longer computes anything — not untidiness. "Reachable from a
    // Return" is the wrong liveness question for these values: a local that
    // the compiled code never reads again is still live to the interpreter it
    // may deopt back into.
    //
    // This deliberately reverses the previous policy ("snapshots are NOT
    // seeded as DCE roots … a value killed here resolves to `Undefined`"). The
    // cost that policy was buying is real and unchanged — the builder records
    // a snapshot at *every* bytecode boundary, so this pins essentially every
    // operand the method computes and DCE reclaims much less — but the cost of
    // the alternative is a wrong interpreter frame, and code size is not
    // tradeable against that. The way to get the reclamation back is the model
    // change the old comment already named: record snapshots only at real
    // deopt sites (guard bcis, call returns), or recompute snapshot liveness
    // after optimization. Narrowing what is *recorded* is safe; dropping what
    // is recorded but not rooted is not.
    for sp in &graph.safepoints {
        for &slot in sp.locals.iter().chain(sp.stack.iter()) {
            // Stale slots were normalised to `NO_NODE` above, so anything that
            // survives `is_valid_id` here names a real, live node.
            if graph.is_valid_id(slot) {
                worklist.push(slot);
            }
        }
    }

    // Also keep all control nodes reachable from exit
    while let Some(id) = worklist.pop() {
        if !reachable.insert(id) {
            continue;
        }
        let node = &graph.nodes[id as usize];
        for &inp in &node.inputs {
            if graph.is_valid_id(inp) {
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

/// `true` (default) when full unrolling of small constant-trip-count counted
/// loops runs. Flipped default-ON after the increment-3 unroll pass soaked clean
/// (validated live against HotSpot: bt10/14/16/18 + UnrollProbe/UnrollProbe2 +
/// loop-heavy bench differential gate-ON ≡ gate-OFF == HotSpot).
/// `CRATONVM_JIT_UNROLL=0` is the opt-out that restores the pre-flip (no-unroll)
/// behaviour as the safety net.
pub fn unroll_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_UNROLL").map_or(true, |v| v != "0")
    })
}

/// Only fully unroll loops whose constant trip count is at most this (bounds
/// code growth; larger counted loops are left rolled).
const UNROLL_MAX_TRIP: i64 = 8;
/// Skip loops whose per-iteration computation exceeds this many nodes.
const UNROLL_MAX_BODY: usize = 48;

fn const_i64(graph: &Graph, id: NodeId) -> Option<i64> {
    match graph.node_opt(id)?.op {
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
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_UNROLL").is_some();
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
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_UNROLL").is_some();
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

        // No side effects in the loop: a Store/Call/alloc/ArrayLength/Guard/array
        // element access that is loop-variant or control-pinned to the loop is a
        // side effect we do not model → bail. (`ArrayLoad`/`ArrayStore` can throw
        // NPE/AIOOBE and `ArrayStore` mutates the heap, so unrolling a loop that
        // contains one would duplicate/reorder those effects — conservatively
        // decline.)
        for id in 0..graph.nodes.len() {
            let op = &graph.nodes[id].op;
            if matches!(
                op,
                Op::Store(_)
                    | Op::Call { .. }
                    // cov-01: a helper call that may allocate, intern or run
                    // `<clinit>` is a side effect this pass does not model, so
                    // a loop containing one is not unrolled.
                    | Op::ConstString { .. }
                    | Op::ConstClass { .. }
                    | Op::LoadStatic { .. }
                    | Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::ArrayLength
                    | Op::ArrayLoad(_)
                    | Op::ArrayStore(_)
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
        let body_named_by_safepoint = graph.safepoints.iter().any(|sp| {
            sp.locals.iter().chain(sp.stack.iter()).any(|&slot| {
                slot != NO_NODE && (to_clone.contains(&slot) || carried_set.contains(&slot))
            })
        });
        if body_named_by_safepoint {
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
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_UNROLL").is_some() {
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

    /// The ops of every live node reachable from `root` through input edges —
    /// i.e. the computation the method actually performs, as distinct from
    /// what merely still occupies the arena.
    ///
    /// These are different questions now that `eliminate_dead_nodes` roots
    /// safepoint snapshots: the builder records the operand stack at every
    /// bci, so an intermediate no live computation reads can still be named by
    /// a snapshot slot and is deliberately kept alive for deopt. A whole-arena
    /// node count therefore measures "what DCE reclaimed", which is a
    /// deopt-policy question, not "what the optimizer collapsed".
    fn ops_reachable_from(graph: &Graph, root: NodeId) -> Vec<Op> {
        let mut seen: FxHashSet<NodeId> = FxHashSet::default();
        let mut stack = vec![root];
        let mut out = Vec::new();
        while let Some(id) = stack.pop() {
            if !graph.is_valid_id(id) || !seen.insert(id) {
                continue;
            }
            let n = &graph.nodes[id as usize];
            if n.op == Op::Dead {
                continue;
            }
            out.push(n.op.clone());
            for &inp in &n.inputs {
                stack.push(inp);
            }
        }
        out
    }

    /// A pure constant's affine form has no root, and that absence is `None` —
    /// not the [`NO_NODE`] sentinel doing double duty as a value-domain
    /// "nothing". The whole point of the `Option` is that the case split
    /// cannot be forgotten, and that a rootless form can never materialize an
    /// edge out of `u32::MAX`.
    #[test]
    fn test_affine_pure_constant_round_trips_without_a_sentinel() {
        let seven = Affine {
            root: None,
            k: 0,
            c: 7,
        };
        let five = Affine {
            root: None,
            k: 0,
            c: 5,
        };
        // The opaque form a real node would carry, for the `combine` calls.
        let opaque = Affine {
            root: Some(1),
            k: 1,
            c: 0,
        };

        // Combining two rootless forms stays rootless — no root is invented.
        assert_eq!(
            combine_affine(&Op::Add, true, seven, five, opaque),
            Affine {
                root: None,
                k: 0,
                c: 12
            }
        );
        assert_eq!(
            combine_affine(&Op::Sub, true, seven, five, opaque),
            Affine {
                root: None,
                k: 0,
                c: 2
            }
        );
        assert_eq!(
            combine_affine(&Op::Mul, true, seven, five, opaque),
            Affine {
                root: None,
                k: 0,
                c: 35
            }
        );

        // …and it materializes back to a single `Const`, with no operand
        // anywhere in the graph holding the placeholder.
        let mut g = probe_graph();
        let mat = build_affine(&mut g, seven.root, seven.k, seven.c, IrType::Int)
            .expect("a rootless form with k == 0 is exactly a constant");
        assert_eq!(g.nodes[mat as usize].op, Op::Const(7));
        assert!(
            g.nodes.iter().all(|n| !n.inputs.contains(&NO_NODE)),
            "materializing a pure constant must not fabricate a NO_NODE operand"
        );

        // A non-zero `k` with no root is not a representable value: refuse,
        // rather than emit `Mul(nodes[u32::MAX], k)`.
        assert!(
            build_affine(&mut g, None, 3, 0, IrType::Int).is_none(),
            "a rootless form with k != 0 has nothing to multiply"
        );
    }

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
        let ret_val = graph.nodes[graph.exit as usize].inputs[1];
        let val = &graph.nodes[ret_val as usize];
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
        // The whole intermediate chain collapsed: the returned value is
        // computed by exactly one Mul and one Add.
        //
        // Measured over the nodes the *return value* reaches, not over the
        // whole arena. The arena count stopped being the right question when
        // `eliminate_dead_nodes` began rooting safepoint snapshots: the
        // builder records the operand stack at every bci, so an intermediate
        // like the original `x*3` is still named by the snapshot at the next
        // bci and is deliberately kept for deopt even though no live
        // computation reads it. See `ops_reachable_from`.
        let reached = ops_reachable_from(&graph, ret_val);
        assert_eq!(
            reached.iter().filter(|o| **o == Op::Mul).count(),
            1,
            "the returned expression must be a single Mul: {reached:?}"
        );
        assert_eq!(
            reached.iter().filter(|o| **o == Op::Add).count(),
            1,
            "…and a single Add: {reached:?}"
        );
    }

    #[test]
    fn test_reassoc_combines_muls() {
        // x = x*181; x = x*181;  →  x*32761  (181*181, a single Mul)
        // iload_0; sipush 181; imul; sipush 181; imul; ireturn
        let code = [
            0x1a, 0x11, 0x00, 0xb5, 0x68, 0x11, 0x00, 0xb5, 0x68, 0xac, 0, 0,
        ];
        let graph = build_and_reassociate(&code, 10, 1, 1);
        let ret_val = graph.nodes[graph.exit as usize].inputs[1];
        let val = &graph.nodes[ret_val as usize];
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
        // One Mul in the returned expression. Arena-wide counting no longer
        // answers this question — the un-materialized `x*181` intermediate is
        // still named by the operand-stack snapshot at the next bci, and
        // snapshot slots are DCE roots now. See `ops_reachable_from`.
        let reached = ops_reachable_from(&graph, ret_val);
        assert_eq!(
            reached.iter().filter(|o| **o == Op::Mul).count(),
            1,
            "the returned expression must be a single Mul: {reached:?}"
        );
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
            inputs: vec![].into(),
            bytecode_pc: None,
        });
        nodes.push(Node {
            op: Op::Const(b),
            ty,
            inputs: vec![].into(),
            bytecode_pc: None,
        });
        nodes.push(Node {
            op,
            ty,
            inputs: vec![0, 1].into(),
            bytecode_pc: None,
        });
        try_fold(&nodes, 2)
    }

    fn fold_unop(ty: IrType, op: Op, a: i64) -> Option<i64> {
        let mut nodes: Vec<Node> = Vec::new();
        nodes.push(Node {
            op: Op::Const(a),
            ty,
            inputs: vec![].into(),
            bytecode_pc: None,
        });
        nodes.push(Node {
            op,
            ty,
            inputs: vec![0].into(),
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
            uses: Default::default(),
            receiver_param: None,
        };
        let start = g.add(Op::Start, IrType::Void, vec![], None);
        g.entry = start;
        g
    }

    #[test]
    fn test_dce_keeps_all_return_paths() {
        // `if (a < 0) return -1; return 1;` — two `Op::Return` terminators. The
        // builder records only the LAST in `graph.exit`, but DCE must keep BOTH
        // return paths (regression for the conditional-early-return miscompile:
        // rooting only from `graph.exit` deleted the `return -1` branch and
        // collapsed the `If` into a single-successor that always returned 1).
        let mut g = probe_graph();
        let a = g.add(Op::Param(0), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, zero], None);
        let if_node = g.add(Op::If, IrType::Control, vec![g.entry, cmp], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
        let neg1 = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let ret_t = g.add(Op::Return, IrType::Void, vec![t, neg1], None);
        let ret_f = g.add(Op::Return, IrType::Void, vec![f, one], None);
        // The builder overwrites `exit` with each return → only the last one.
        g.exit = ret_f;

        eliminate_dead_nodes(&mut g);

        assert_ne!(
            g.nodes[ret_t as usize].op,
            Op::Dead,
            "the first (graph.exit-excluded) return must survive DCE"
        );
        assert_ne!(g.nodes[ret_f as usize].op, Op::Dead);
        assert_ne!(
            g.nodes[t as usize].op,
            Op::Dead,
            "the true projection feeding the first return must survive"
        );
        assert_ne!(g.nodes[f as usize].op, Op::Dead);
        assert_ne!(
            g.nodes[neg1 as usize].op,
            Op::Dead,
            "the -1 value of the first return must survive"
        );
    }

    #[test]
    fn test_dce_keeps_value_live_only_via_safepoint_snapshot() {
        // `int y = x + 1; return x;` — `y` is dead to the *compiled* code, but
        // the safepoint snapshot names it, and deopt rebuilds interpreter local
        // 1 out of that slot. "Reachable from a Return" is the wrong liveness
        // question for a snapshot value: a local the compiled code never reads
        // again is still live to the interpreter it may deopt back into.
        // Removing it would leave the slot naming an `Op::Dead` node and the
        // reconstructed frame built from nothing.
        use crate::ir::SafepointSnapshot;
        let mut g = probe_graph();
        let x = g.add(Op::Param(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let y = g.add(Op::Add, IrType::Int, vec![x, one], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, x], None);
        g.exit = ret;
        g.safepoints.push(SafepointSnapshot {
            bci: 4,
            locals: vec![x, y],
            stack: vec![],
        });

        eliminate_dead_nodes(&mut g);

        assert_ne!(
            g.nodes[y as usize].op,
            Op::Dead,
            "a value live only through a safepoint snapshot must survive DCE"
        );
        assert_ne!(
            g.nodes[one as usize].op,
            Op::Dead,
            "and so must everything it transitively needs"
        );
        assert_eq!(
            g.safepoints[0].locals[1], y,
            "the slot must still name the value — keeping it is the fix, \
             clearing the slot would silently lose an interpreter local"
        );
    }

    #[test]
    fn test_dce_normalises_already_stale_snapshot_slot() {
        // A slot naming a node some EARLIER pass removed cannot be rescued —
        // the value it named is already gone, and seeding it as a root would
        // only resurrect an `Op::Dead` node. It normalises to `NO_NODE`
        // ("undefined at this bci"), which deopt resolves to `Undefined`:
        // the same answer, reached explicitly instead of by naming a corpse.
        use crate::ir::SafepointSnapshot;
        let mut g = probe_graph();
        let x = g.add(Op::Param(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let stale = g.add(Op::Add, IrType::Int, vec![x, one], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, x], None);
        g.exit = ret;
        g.safepoints.push(SafepointSnapshot {
            bci: 4,
            locals: vec![x, stale],
            stack: vec![NO_NODE],
        });
        // Stand in for the earlier pass that removed the node without routing
        // its safepoint references.
        g.kill(stale);

        eliminate_dead_nodes(&mut g);

        assert_eq!(
            g.safepoints[0].locals[1], NO_NODE,
            "an already-dead slot normalises to the undefined placeholder"
        );
        assert_eq!(g.safepoints[0].locals[0], x, "a live slot is untouched");
        assert_eq!(
            g.safepoints[0].stack[0], NO_NODE,
            "an already-undefined slot stays undefined"
        );
        assert_eq!(
            g.nodes[stale as usize].op,
            Op::Dead,
            "normalising the slot does not resurrect the node"
        );
    }

    #[test]
    fn test_dse_removes_overwritten_store() {
        // A local object whose field 0 is written twice with no read in
        // between: the first store is dead and must be removed; the second
        // (live) store and the allocation survive.
        //
        // The compact layout carries no offset operand, so the overwrite match
        // rests on `num_fields == 1` — with one field, "the object's storage"
        // and "field 0" are the same cell (`store_matchable_location`). Raise
        // `num_fields` and the two stores must stop matching; that is
        // `test_dse_absent_offset_stores_to_a_multi_field_object_are_not_matched`.
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
        // Different MemKind → different location key. NOTE that this is an
        // access *width* difference, not a field-identity one: it happens to
        // separate these two stores, but two `int` fields share their MemKind.
        // The width is not what makes the pass field-safe — see
        // `test_dse_absent_offset_stores_to_a_multi_field_object_are_not_matched`
        // for the same-width case.
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

    // ── DSE: memory-token chain integrity ───────────────────────────

    #[test]
    fn test_dse_overwritten_store_leaves_intact_memory_chain() {
        // Full-form chain:  m0 → alloc → s1 → s2 → load
        //
        // `s1` is overwritten by `s2` with no reader in between, so DSE removes
        // it. What this locks is *how*: a plain `Graph::kill` left `s2`'s token
        // slot naming the removed `s1`, so `s2` lost its transitive ordering
        // edge to `alloc` and to everything that wrote before it — and a
        // scheduler reading that chain is then free to hoist `s2` above them.
        // After the splice, `s2` must take its token from `s1`'s OWN incoming
        // token, and no live node anywhere may read a token from a `Dead` node.
        let mut g = probe_graph();
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![g.entry], None);
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, m0],
            None,
        );
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        // `Op::New` is both the reference and the memory token it produces, so
        // it appears in slot 1 (token) and slot 2 (base) of the first store.
        let s1 = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![ctrl, alloc, alloc, off, c1],
            None,
        );
        let s2 = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![ctrl, s1, alloc, off, c2],
            None,
        );
        // A reader keeps the allocation "read" so the write-only phase does not
        // also remove `s2` — this test is about the overwrite path only.
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, s2, alloc, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[s1 as usize].op,
            Op::Dead,
            "the overwritten store must still be eliminated"
        );
        assert_eq!(
            g.nodes[s2 as usize].op,
            Op::Store(MemKind::Int),
            "the overwriting store survives"
        );
        assert_eq!(
            g.nodes[s2 as usize].inputs[1], alloc,
            "the surviving store must be spliced onto the removed store's own \
             incoming memory token, not left naming the removed node"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[1], s2,
            "the rest of the chain is untouched"
        );

        for (id, n) in g.nodes.iter().enumerate() {
            if n.op == Op::Dead {
                continue;
            }
            for (i, &inp) in n.inputs.iter().enumerate() {
                if !is_memory_token_slot(n, i) {
                    continue;
                }
                assert_ne!(
                    g.nodes[inp as usize].op,
                    Op::Dead,
                    "n{id}:{:?} takes its memory token from removed n{inp}",
                    n.op
                );
            }
        }
    }

    #[test]
    fn test_dse_keeps_store_it_cannot_splice_out_of_the_chain() {
        // A store whose token consumer names it, but which has no incoming
        // token of its own to hand on (the compact `[base, value]` layout).
        // Deleting it would leave the consumer's token slot pointing at a
        // removed node with nothing to repair it with, so the store is KEPT: a
        // missed optimization, not a severed chain.
        let mut g = probe_graph();
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![g.entry], None);
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, m0],
            None,
        );
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        // Compact store — no token slot at all. Its location key is
        // `(alloc, no-index, Int)`.
        let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
        // …overwritten by a full-form no-index store `[ctrl, mem, base, value]`
        // (the same location key) that also names `s1` as its memory token.
        let s2 = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![ctrl, s1, alloc, c2],
            None,
        );
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, s2, alloc, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[s1 as usize].op,
            Op::Store(MemKind::Int),
            "a store that cannot be spliced out of the chain must be left alive"
        );
        assert_eq!(g.nodes[s2 as usize].inputs[1], s1, "the chain is unchanged");
    }

    // ── Alias-analysis audit (wave: alias soundness) ────────────────

    #[test]
    fn test_dse_absent_offset_stores_to_a_multi_field_object_are_not_matched() {
        // `o.x = 1; o.y = 2;` where `x` and `y` are two `int` fields of the
        // SAME object, written through a layout that carries no offset operand.
        //
        // The location key is `(base, idx, MemKind)`. Both stores have
        // `idx == NO_NODE` and `MemKind::Int`, so the key is identical — yet
        // they name different cells, and matching them deletes the live
        // `o.x = 1`. `MemKind` is an access *width*, not a field index; two
        // `int` fields share it. The memory model calls a missing offset
        // `ir::AccessOffset::Absent` and defines it as a MAY-alias, and a
        // deletion needs a MUST-alias.
        //
        // (Contrast `test_dse_removes_overwritten_store`, where the object has
        // exactly ONE field and the absence of an offset therefore still names
        // a definite cell.)
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
        let s1 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c1], None);
        let s2 = g.add(Op::Store(MemKind::Int), IrType::Void, vec![alloc, c2], None);
        // Read the object back so the write-only phase (which keys on the base
        // alone and is field-independent) does not also fire — this test is
        // about the overwrite phase.
        let load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![alloc], None);
        let ret = g.add(Op::Return, IrType::Void, vec![g.entry, load], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[s1 as usize].op,
            Op::Store(MemKind::Int),
            "nothing proves these two stores hit the same cell: with no offset \
             operand the layout cannot tell field x from field y, and the \
             matching MemKind is a width, not an identity"
        );
        assert_eq!(g.nodes[s2 as usize].op, Op::Store(MemKind::Int));
    }

    #[test]
    fn test_dse_offset_distinguished_stores_still_match_and_still_separate() {
        // The production layout `[ctrl, mem, base, offset, value]`: two stores
        // to the SAME constant offset still match (the optimization survives),
        // and two stores to DIFFERENT constant offsets still do not.
        let mut g = probe_graph();
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
        let m0 = g.add(Op::Proj(1), IrType::Memory, vec![g.entry], None);
        let alloc = g.add(
            Op::New {
                class_id: 1,
                num_fields: 2,
            },
            IrType::Ref,
            vec![ctrl, m0],
            None,
        );
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let c2 = g.add(Op::Const(2), IrType::Int, vec![], None);
        let c3 = g.add(Op::Const(3), IrType::Int, vec![], None);
        // f0 = 1 ; f1 = 2 ; f0 = 3   →  only the first is overwritten.
        let s_f0_a = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![ctrl, alloc, alloc, off0, c1],
            None,
        );
        let s_f1 = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![ctrl, s_f0_a, alloc, off1, c2],
            None,
        );
        let s_f0_b = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![ctrl, s_f1, alloc, off0, c3],
            None,
        );
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, s_f0_b, alloc, off0],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
        g.exit = ret;

        eliminate_dead_stores(&mut g);

        assert_eq!(
            g.nodes[s_f0_a as usize].op,
            Op::Dead,
            "two stores at the same constant offset are a proved overwrite"
        );
        assert_eq!(
            g.nodes[s_f1 as usize].op,
            Op::Store(MemKind::Int),
            "the store to the OTHER field is not overwritten by either"
        );
        assert_eq!(g.nodes[s_f0_b as usize].op, Op::Store(MemKind::Int));
        // …and the chain closed over the hole rather than naming a dead node.
        for (id, n) in g.nodes.iter().enumerate() {
            if n.op == Op::Dead {
                continue;
            }
            for (i, &inp) in n.inputs.iter().enumerate() {
                if !is_memory_token_slot(n, i) {
                    continue;
                }
                assert_ne!(
                    g.nodes[inp as usize].op,
                    Op::Dead,
                    "n{id}:{:?} takes its memory token from removed n{inp}",
                    n.op
                );
            }
        }
    }

    #[test]
    fn test_loop_body_pins_a_node_whose_only_link_sorts_after_it() {
        // The loop-body sweep must reach a fixed point. `call` is created
        // BEFORE the node that pins it into the body, so a single arena pass in
        // id order misses it — and a missed `Op::Call` is a hard barrier
        // `loop_has_hard_barrier` never sees, which is how a load gets hoisted
        // across a call that may write the very field it reads.
        let mut g = probe_graph();
        let pre = g.add(Op::Proj(0), IrType::Control, vec![g.entry], None);
        let region = g.add(Op::Region, IrType::Control, vec![pre, NO_NODE], None);
        // Created here, patched below: its memory operand does not exist yet.
        let call = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![pre, NO_NODE],
            None,
        );
        let back = g.add(Op::Proj(0), IrType::Control, vec![region], None);
        let mid = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![back, NO_NODE, pre],
            None,
        );
        g.nodes[region as usize].inputs[1] = back;
        g.nodes[call as usize].inputs[1] = mid;

        let body = loop_body(&g, region, pre, &[back]);

        assert!(
            body.contains(&mid),
            "the load is pinned directly to a body control node"
        );
        assert!(
            body.contains(&call),
            "the call is pinned to the body THROUGH the load, whose id sorts \
             after it — one arena pass in id order misses this"
        );
        assert!(
            loop_has_hard_barrier(&g, &body),
            "a call in the body must disqualify the loop for hoisting"
        );
        assert!(
            !body.contains(&pre),
            "the pre-header must stay out of the body"
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
            uses: Default::default(),
            receiver_param: None,
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
            uses: Default::default(),
            receiver_param: None,
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
        // Use the induction variable itself as the BASE — that is what makes
        // the load variant; the offset is an ordinary invariant constant.
        //
        // This fixture used to be built as `[ctrl, mem, iv, NO_NODE]`: the
        // full four-input form, whose slot 3 the layout dispatcher reads as a
        // real index operand, filled with the placeholder. That is not
        // padding, it is a malformed graph. `ir_verify`'s always-on structural
        // lane rejects `NO_NODE` in any slot that is not a φ value input or a
        // safepoint slot, and it is the exact shape that panics
        // `ir_lower::slot_of`, which indexes `node_slot` with `u32::MAX`. It
        // only ever went unnoticed because the base is variant, so LICM bailed
        // (`addr != NO_NODE &&` …) before it read the slot — the placeholder
        // was load-bearing for nothing. A real `Const` offset expresses the
        // same test (base variant ⇒ no hoist) with a graph that is valid. See
        // `test_licm_variant_load_fixture_has_no_placeholder_operand`.
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, iv, off],
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

    /// Regression for the fixture above: no live node may carry `NO_NODE` in a
    /// real operand slot. Only a φ value input (a local undefined on that
    /// predecessor edge) and a safepoint slot may hold the placeholder — the
    /// same rule `ir_verify::no_node_allowed` encodes for the always-on lane.
    #[test]
    fn test_licm_variant_load_fixture_has_no_placeholder_operand() {
        let (mut g, region, _preheader, iv) = loop_probe();
        let mem = 2;
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, iv, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let _ = licm(&mut g);

        for (id, n) in g.nodes.iter().enumerate() {
            if n.op == Op::Dead {
                continue;
            }
            for (i, &inp) in n.inputs.iter().enumerate() {
                let placeholder_ok = matches!(n.op, Op::Phi) && i >= 1;
                assert!(
                    inp != NO_NODE || placeholder_ok,
                    "n{id}:{:?} input[{i}] is the NO_NODE placeholder in a real \
                     operand slot — `ir_lower::slot_of` indexes node_slot with it",
                    n.op
                );
            }
        }
        // …and the load kept the full four-input shape the arity lane wants.
        assert_eq!(
            g.nodes[load as usize].inputs.len(),
            4,
            "the full load form is [ctrl, mem, base, offset]"
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

    /// `for (r…) { for (i…) { … } }` — and the inner header is the one the
    /// iterations are in.
    ///
    /// `loop_headers` classified a header's control inputs by asking which are
    /// reachable *forward from the header*. For an inner header that question
    /// has no useful answer: the walk leaves through the inner exit, goes round
    /// the OUTER back edge, and arrives back at the inner loop's own
    /// pre-header — so both inputs read as back edges, the loop yields zero
    /// pre-headers, and it was dropped. Every nested loop in the tree offered
    /// LICM only its outer header.
    #[test]
    fn test_loop_headers_finds_the_inner_header_of_a_nested_loop() {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);

        // Outer header; its back edge is wired below.
        let outer = g.add(Op::Merge, IrType::Control, vec![c0], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let r = g.add(Op::Phi, IrType::Int, vec![outer, zero], None);
        let ten = g.add(Op::Const(10), IrType::Int, vec![], None);
        let ocond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![r, ten], None);
        let oif = g.add(Op::If, IrType::Control, vec![outer, ocond], None);
        let obody = g.add(Op::Proj(0), IrType::Control, vec![oif], None); // inner pre-header
        let oexit = g.add(Op::Proj(1), IrType::Control, vec![oif], None);

        // Inner header, entered from the outer body.
        let inner = g.add(Op::Merge, IrType::Control, vec![obody], None);
        let i = g.add(Op::Phi, IrType::Int, vec![inner, zero], None);
        let icond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, ten], None);
        let iif = g.add(Op::If, IrType::Control, vec![inner, icond], None);
        let ibody = g.add(Op::Proj(0), IrType::Control, vec![iif], None);
        let iexit = g.add(Op::Proj(1), IrType::Control, vec![iif], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let inext = g.add(Op::Add, IrType::Int, vec![i, one], None);
        g.nodes[i as usize].inputs.push(inext);
        g.nodes[inner as usize].inputs.push(ibody); // inner back edge

        // Inner exit closes the outer back edge.
        let rnext = g.add(Op::Add, IrType::Int, vec![r, one], None);
        g.nodes[r as usize].inputs.push(rnext);
        g.nodes[outer as usize].inputs.push(iexit); // outer back edge
        let ret = g.add(Op::Return, IrType::Void, vec![oexit, r], None);
        g.exit = ret;

        let headers = loop_headers(&g);
        let inner_entry = headers
            .iter()
            .find(|&&(h, _, _)| h == inner)
            .map(|&(_, e, _)| e);
        assert_eq!(
            inner_entry,
            Some(obody),
            "the inner header's pre-header is the outer body's projection, got \
             headers {:?}",
            headers.iter().map(|&(h, e, _)| (h, e)).collect::<Vec<_>>()
        );
        let outer_entry = headers
            .iter()
            .find(|&&(h, _, _)| h == outer)
            .map(|&(_, e, _)| e);
        assert_eq!(
            outer_entry,
            Some(c0),
            "the outer header's classification must be unchanged"
        );
    }

    /// javac's counted loop — `for (i = 0; i < a.length; i++)` — re-evaluates
    /// `a.length` at the top of every iteration, and the resulting in-loop
    /// `Op::ArrayLength` is itself one of the hard barriers that disqualified
    /// this pass from hoisting anything at all. Measured on
    /// `probes/ArrayElemLoadCost.java` before the fix: every candidate header
    /// reported `hard_barrier=true, 0 load(s)`.
    ///
    /// Both edges have to move. Re-anchoring only the control leaves the node
    /// reading the loop's memory phi, and `ir_schedule::find_best_block` places
    /// a data node in the deepest block dominated by ALL its input blocks — so
    /// the "hoisted" node would schedule straight back into the loop. This test
    /// pins both.
    #[test]
    fn test_licm_hoists_invariant_arraylength() {
        let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
        let entry_mem = 2; // the Start's memory projection
        let arr = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        // The loop's memory phi, exactly as the builder emits it:
        // [region, entry_memory, back_edge_memory]. Its own back edge is
        // itself here — nothing in this probe writes memory.
        let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
        g.nodes[mem_phi as usize].inputs.push(mem_phi);
        let len = g.add(
            Op::ArrayLength,
            IrType::Int,
            vec![region, mem_phi, arr],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, len], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(changed, "an invariant arraylength must hoist");
        assert_eq!(
            g.nodes[len as usize].inputs[0], preheader,
            "control must be re-anchored to the pre-header"
        );
        assert_eq!(
            g.nodes[len as usize].inputs[1], entry_mem,
            "the memory token must be re-anchored to the loop's ENTRY memory — \
             leaving it on the loop's memory phi schedules the node back into \
             the loop and the hoist is inert"
        );
    }

    /// The receiver decides. An `arraylength` of something the loop itself
    /// produces is not invariant, and hoisting it would read the length of a
    /// different array (or of nothing yet).
    #[test]
    fn test_licm_does_not_hoist_variant_arraylength() {
        let (mut g, region, _preheader, iv) = loop_probe_header(Op::Merge);
        let entry_mem = 2;
        let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
        g.nodes[mem_phi as usize].inputs.push(mem_phi);
        // A "receiver" that varies with the induction variable.
        let variant_ref = g.add(Op::Phi, IrType::Ref, vec![region, iv], None);
        g.nodes[variant_ref as usize].inputs.push(variant_ref);
        let len = g.add(
            Op::ArrayLength,
            IrType::Int,
            vec![region, mem_phi, variant_ref],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, len], None);
        g.exit = ret;

        licm(&mut g);

        assert_eq!(
            g.nodes[len as usize].inputs[0], region,
            "a variant receiver's arraylength must stay in the loop"
        );
    }

    /// The hoist moves a node that can raise NPE, so it is only taken when the
    /// `arraylength` runs on EVERY entry to the header — i.e. its control input
    /// is the header itself. One behind a conditional inside the body may never
    /// run at all in the original program, and moving it into the pre-header
    /// would raise an exception the program never raised.
    #[test]
    fn test_licm_does_not_hoist_conditional_arraylength() {
        let (mut g, region, _preheader, iv) = loop_probe_header(Op::Merge);
        let entry_mem = 2;
        let arr = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, entry_mem], None);
        g.nodes[mem_phi as usize].inputs.push(mem_phi);
        // An in-body branch, with the arraylength on one arm only.
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![iv, zero], None);
        let inner_if = g.add(Op::If, IrType::Control, vec![region, cond], None);
        let taken = g.add(Op::Proj(0), IrType::Control, vec![inner_if], None);
        let len = g.add(
            Op::ArrayLength,
            IrType::Int,
            vec![taken, mem_phi, arr],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![taken, len], None);
        g.exit = ret;

        licm(&mut g);

        assert_eq!(
            g.nodes[len as usize].inputs[0], taken,
            "a conditionally-executed arraylength must stay where it is"
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
    fn test_trivial_phi_collapses_to_single_value() {
        // φ(ctrl; v, self) collapses to v (trivial loop phi for an unmodified
        // local); φ(ctrl; a, b) with two distinct values is a genuine merge and
        // is left untouched.
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Region, IrType::Control, vec![start], None);
        let v = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        // Trivial loop phi: value inputs = [v, self].
        let triv = g.add(Op::Phi, IrType::Ref, vec![ctrl, v], None);
        g.nodes[triv as usize].inputs.push(triv); // self back-edge
        let user = g.add(Op::ArrayLength, IrType::Int, vec![triv], None);
        // Genuine merge phi: two distinct value inputs.
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        let merge = g.add(Op::Phi, IrType::Int, vec![ctrl, a, b], None);
        let user2 = g.add(Op::Neg, IrType::Int, vec![merge], None);

        algebraic_simplify(&mut g);

        assert_eq!(
            g.nodes[user as usize].inputs[0], v,
            "the trivial phi must collapse to its single value v"
        );
        assert_eq!(
            g.nodes[triv as usize].op,
            Op::Dead,
            "the collapsed trivial phi must be killed"
        );
        assert_eq!(
            g.nodes[user2 as usize].inputs[0], merge,
            "a genuine merge phi (distinct inputs) must NOT be collapsed"
        );
        assert_ne!(g.nodes[merge as usize].op, Op::Dead);
    }

    #[test]
    fn test_licm_hoists_load_over_trivial_phi_base() {
        // The production case: a getfield whose base is a loop-invariant local
        // arrives with the base wrapped in a trivial loop-header phi `φ(p, self)`
        // (the IR builder inserts a phi for every live local). Trivial-phi
        // elimination collapses it to the Param, after which LICM hoists the
        // load — the integration that makes LICM non-inert on real bytecode.
        let (mut g, region, preheader, _iv) = loop_probe_header(Op::Merge);
        let mem = 2;
        let param = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        // Trivial loop phi over the invariant base, anchored at the loop header.
        let base_phi = g.add(Op::Phi, IrType::Ref, vec![region, param], None);
        g.nodes[base_phi as usize].inputs.push(base_phi); // unchanged back-edge
        let off = g.add(Op::Const(4), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, base_phi, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        // Precondition: the load's base is the region-anchored loop phi, which
        // LICM judges loop-variant — so without the collapse LICM is inert here.
        assert_eq!(g.nodes[load as usize].inputs[2], base_phi);
        assert_eq!(g.nodes[base_phi as usize].inputs[0], region);

        // Collapse the trivial phi, then hoist.
        algebraic_simplify(&mut g);
        assert_eq!(
            g.nodes[load as usize].inputs[2], param,
            "trivial-phi elimination must expose the Param base to the load"
        );
        let changed = licm(&mut g);
        assert!(
            changed,
            "LICM must hoist once the base is the invariant Param"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the hoisted load's control must be re-anchored to the pre-header"
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
    fn test_licm_hoists_load_past_nonaliasing_local_store() {
        // An invariant load of local allocation A.f with an in-loop store to a
        // DISTINCT local allocation B.f must still hoist: two distinct local
        // allocations never alias, so the store cannot clobber the load.
        let (mut g, region, preheader, _iv) = loop_probe();
        let mem = 2;
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let b = g.add(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, a, off],
            None,
        );
        // In-loop store to a DIFFERENT local allocation — not a hard barrier and
        // provably non-aliasing with the load of A.
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, b, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(
            changed,
            "the load of A must hoist past the non-aliasing store to B"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the hoisted load's control must be re-anchored to the pre-header"
        );
    }

    #[test]
    fn test_licm_keeps_load_when_store_to_same_alloc() {
        // An in-loop store to the SAME allocation the load reads may alias it
        // (same object) — the load must NOT be hoisted.
        let (mut g, region, preheader, _iv) = loop_probe();
        let mem = 2;
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, a, off],
            None,
        );
        // Store to the SAME allocation A → may alias the load.
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, a, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let _ = licm(&mut g);

        assert_eq!(
            g.nodes[load as usize].inputs[0], region,
            "a load aliased by an in-loop store to the same allocation must stay in the loop"
        );
    }

    #[test]
    fn test_licm_keeps_load_when_store_base_non_local() {
        // The alias oracle is conservative: a store through a non-local base (a
        // Param) is not provably non-aliasing, so the load stays pinned even
        // though the store actually writes a different object.
        let (mut g, region, preheader, _iv) = loop_probe();
        let mem = 2;
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let p = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, a, off],
            None,
        );
        // Store through a Param base — not a provable local allocation.
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, p, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let _ = licm(&mut g);

        assert_eq!(
            g.nodes[load as usize].inputs[0], region,
            "a non-local store base is not provably non-aliasing → load stays pinned"
        );
    }

    #[test]
    fn test_licm_hoists_param_load_past_local_store() {
        // A load from a method parameter P.f, with an in-loop store to a local
        // allocation B.f, hoists: a freshly-allocated object is never the
        // pre-existing parameter object, so the store cannot clobber the load.
        let (mut g, region, preheader, _iv) = loop_probe();
        let mem = 2;
        let p = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let b = g.add(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, p, off],
            None,
        );
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, b, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(
            changed,
            "the load of param P must hoist past the store to local B"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the hoisted load's control must be re-anchored to the pre-header"
        );
    }

    #[test]
    fn test_licm_keeps_param_load_when_store_base_is_param() {
        // The store base is NOT widened to Param: two parameters may be the same
        // object (`foo(x, x)`), so a load from P0.f must NOT hoist past a store
        // through P1 — they may alias.
        let (mut g, region, _preheader, _iv) = loop_probe();
        let mem = 2;
        let p0 = g.add(Op::Param(0), IrType::Ref, vec![g.entry], None);
        let p1 = g.add(Op::Param(1), IrType::Ref, vec![g.entry], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, p0, off],
            None,
        );
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, p1, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let _ = licm(&mut g);

        assert_eq!(
            g.nodes[load as usize].inputs[0], region,
            "a store through a Param base may alias another Param load → stays pinned"
        );
    }

    #[test]
    fn test_licm_hoists_load_past_phi_store_of_distinct_allocs() {
        // Cross-merge: the in-loop store's base flows through a Phi merging two
        // local allocations A and B. The oracle resolves it to {A, B}, so a load
        // from a DISTINCT allocation C (disjoint) still hoists past the store.
        let (mut g, region, preheader, _iv) = loop_probe();
        let mem = 2;
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let b = g.add(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let c = g.add(
            Op::New {
                class_id: 3,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        // Ref phi merging A and B (anchor at the region control in slot 0).
        let phi = g.add(Op::Phi, IrType::Ref, vec![region, a, b], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
        // Load of C (distinct), and a store through the phi (writes A or B).
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, c, off],
            None,
        );
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, phi, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(
            changed,
            "the load of C must hoist past a phi-store of {{A, B}} (C is disjoint)"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the hoisted load's control must be re-anchored to the pre-header"
        );
    }

    #[test]
    fn test_licm_keeps_load_when_phi_store_includes_its_alloc() {
        // The phi-store resolves to {A, B}; a load from A IS in that set, so it
        // may alias and must stay pinned.
        let (mut g, region, preheader, _iv) = loop_probe();
        let mem = 2;
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let b = g.add(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let phi = g.add(Op::Phi, IrType::Ref, vec![region, a, b], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cval = g.add(Op::Const(7), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, a, off],
            None,
        );
        let _st = g.add(
            Op::Store(MemKind::Int),
            IrType::Void,
            vec![region, mem, phi, cval],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let _ = licm(&mut g);

        assert_eq!(
            g.nodes[load as usize].inputs[0], region,
            "a load of A may be aliased by a phi-store that writes {{A, B}} → stays pinned"
        );
    }

    #[test]
    fn test_licm_hoists_load_with_invariant_phi_base() {
        // The cross-merge oracle on the LOAD side: a load whose base is a phi
        // merged BEFORE the loop (anchor outside the body) over invariant
        // allocations is itself invariant and hoists. (`x = cond ? new A() :
        // new B(); for (...) ... x.f ...`)
        let (mut g, region, preheader, _iv) = loop_probe();
        let mem = 2;
        let a = g.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        let b = g.add(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            IrType::Ref,
            vec![preheader],
            None,
        );
        // Phi anchored at the pre-loop control (slot 0 = preheader, OUTSIDE the
        // loop body) merging two invariant allocations.
        let phi = g.add(Op::Phi, IrType::Ref, vec![preheader, a, b], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![region, mem, phi, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![region, load], None);
        g.exit = ret;

        let changed = licm(&mut g);

        assert!(
            changed,
            "a load over a pre-loop invariant phi base must hoist"
        );
        assert_eq!(
            g.nodes[load as usize].inputs[0], preheader,
            "the hoisted load's control must be re-anchored to the pre-header"
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
