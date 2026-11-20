// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane REMAT (`CRATONVM_JIT_IR_LS_REMAT_ADD`, opt-in): which
//! values can be recomputed as `base + addend` at a split reload.
//!
//! [`super::remat_of`] answers for the values the ALLOCATOR may treat as free to
//! evict (constants and parameters: no store, scored first). `x + c` is not one
//! of those: it is recomputable only while `x` is in a register, which is a
//! fact of the emission point and not of the graph, so the allocator still
//! stores it and plans a `Load`. This module only classifies the shape; the
//! consumer (`ir_lower::ls_remat_add`) turns a planned `Load` of such a value
//! into `lea dst, [base + addend]` when, at that reload, the base is resident.
//!
//! The classification is the GC-map rule as well: only `int` and `long`, with a
//! base of the same type. A `Ref` is never recomputed (an interior pointer
//! built by `lea` would be a word no oop map names).

use crate::ir::{Graph, IrType, NodeId, Op};

/// `node = base + addend`, wrapped at the node's width.
///
/// `addend` is already normalised: a `Sub(x, c)` is `x + (-c)` with the
/// negation wrapped at the width (`int` `x - MIN` is `x + MIN` modulo 2^32;
/// `long` likewise modulo 2^64), and an `int` addend is the constant's low 32
/// bits sign-extended, which is what Java's `int` arithmetic reads of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RematAdd {
    /// The register operand the reload reads instead of the home word.
    pub base: NodeId,
    /// What is added to it.
    pub addend: i64,
    /// `long` (64-bit add) rather than `int` (32-bit add, zero-extended).
    pub wide: bool,
}

/// `Some` when `node` is an `int`/`long` `Add` with a constant operand (either
/// side: `Add` commutes) or a `Sub` with a constant SECOND operand, and the
/// other operand has the node's own type.
pub fn remat_add_of(graph: &Graph, node: NodeId) -> Option<RematAdd> {
    let n = graph.nodes.get(node as usize)?;
    let wide = match n.ty {
        IrType::Int => false,
        IrType::Long => true,
        _ => return None,
    };
    let (&a, &b) = (n.inputs.first()?, n.inputs.get(1)?);
    let (base, k, sub) = match n.op {
        Op::Add => match (const_of(graph, b), const_of(graph, a)) {
            (Some(k), _) => (a, k, false),
            (None, Some(k)) => (b, k, false),
            (None, None) => return None,
        },
        Op::Sub => (a, const_of(graph, b)?, true),
        _ => return None,
    };
    if graph.nodes.get(base as usize)?.ty != n.ty {
        return None;
    }
    Some(RematAdd {
        base,
        addend: normalised_addend(k, sub, wide),
        wide,
    })
}

fn const_of(graph: &Graph, id: NodeId) -> Option<i64> {
    match graph.nodes.get(id as usize)?.op {
        Op::Const(v) => Some(v),
        _ => None,
    }
}

/// `+k` or `-k`, wrapped at the width. Cast: `int` arithmetic reads the low 32
/// bits of the constant, so the truncation is Java's own.
fn normalised_addend(k: i64, sub: bool, wide: bool) -> i64 {
    if wide {
        if sub {
            k.wrapping_neg()
        } else {
            k
        }
    } else {
        let k = k as i32;
        i64::from(if sub { k.wrapping_neg() } else { k })
    }
}

#[cfg(test)]
mod tests;
