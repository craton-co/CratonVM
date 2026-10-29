// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane CSEHUNT): the points-to walk's `(node, depth)` memo
//! answers exactly what the uncached walk answers, and bounds its cost.

use super::{PointsToWalk, RefPointsTo};
use crate::ir::{Graph, IrType, MemKind, NodeId, Op};

/// The comparable form of an answer: sorted allocation ids (or none) and the
/// pre-existing bit.
fn key(pt: &RefPointsTo) -> (Option<Vec<NodeId>>, bool) {
    let allocs = pt.allocs.as_ref().map(|s| {
        let mut v: Vec<NodeId> = s.iter().copied().collect();
        v.sort_unstable();
        v
    });
    (allocs, pt.has_pre)
}

/// Graph scaffold: start, its control and memory projections, a join anchor
/// for phis, a parameter and one fresh allocation.
struct Base {
    g: Graph,
    join: NodeId,
    param: NodeId,
    fresh: NodeId,
    loaded: NodeId,
}

fn base() -> Base {
    let mut g = Graph::empty(64);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let param = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let fresh = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![entry, mem],
        Some(1),
    );
    let field = g.add(Op::Const(0), IrType::Int, vec![], None);
    let loaded = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![entry, mem, param, field],
        Some(3),
    );
    let join = g.add(Op::Merge, IrType::Control, vec![entry, entry], Some(5));
    Base {
        g,
        join,
        param,
        fresh,
        loaded,
    }
}

fn phi(b: &mut Base, x: NodeId, y: NodeId) -> NodeId {
    let join = b.join;
    b.g.add(Op::Phi, IrType::Ref, vec![join, x, y], None)
}

/// `v[0] = param`, `v[1] = new`, `v[k] = phi(v[k-1], v[k-2])` up to `v[n]`:
/// `x3 = c ? x2 : x1; x4 = c ? x3 : x2; …`. The uncached path count grows
/// like Fibonacci; `v[k]` is `k - 1` phis tall.
fn fib_dag(n: usize) -> (Graph, Vec<NodeId>) {
    let mut b = base();
    let mut v = vec![b.param, b.fresh];
    for k in 2..=n {
        let p = phi(&mut b, v[k - 1], v[k - 2]);
        v.push(p);
    }
    (b.g, v)
}

/// A straight chain of `len` single-value phis over `leaf`.
fn chain(b: &mut Base, leaf: NodeId, len: usize) -> NodeId {
    let mut cur = leaf;
    for _ in 0..len {
        let join = b.join;
        cur = b.g.add(Op::Phi, IrType::Ref, vec![join, cur], None);
    }
    cur
}

fn assert_memo_matches_uncached(g: &Graph) {
    for id in 0..g.nodes.len() as NodeId {
        for depth in 0..=34 {
            let cached = PointsToWalk::memoised().resolve(g, id, depth);
            let plain = PointsToWalk::uncached().resolve(g, id, depth);
            assert_eq!(key(&cached), key(&plain), "node {id} depth {depth}");
        }
    }
}

/// The trap a memo keyed by node alone falls into: `x` is 30 phis tall, so
/// it is known one level below the root and unknown three levels below it
/// (the depth cap cuts it). The root reaches it both ways, so the uncached
/// walk answers unknown; a memo that reused `x`'s shallow answer would say
/// "the parameter" instead.
#[test]
fn j27_csehunt_a_node_reached_at_two_depths_keeps_the_depth_capped_answer() {
    let mut b = base();
    let param = b.param;
    let x = chain(&mut b, param, 30);
    let y = chain(&mut b, x, 2);
    let root = phi(&mut b, x, y);
    let g = b.g;
    assert!(PointsToWalk::uncached().resolve(&g, x, 1).allocs.is_some());
    assert!(PointsToWalk::uncached().resolve(&g, x, 3).allocs.is_none());
    for walk in [PointsToWalk::memoised(), PointsToWalk::uncached()] {
        let mut walk = walk;
        assert!(walk.resolve(&g, root, 0).allocs.is_none());
    }
    assert_memo_matches_uncached(&g);
}

/// Every node at every depth, over fixtures that touch each arm: the depth
/// cap boundary, a shared-input DAG, a cycle that reaches a leaf, and an
/// unknown leaf at either input.
#[test]
fn j27_csehunt_memo_answers_equal_the_uncached_walk_on_every_node() {
    let (g, _) = fib_dag(14);
    assert_memo_matches_uncached(&g);

    let mut b = base();
    let (param, fresh, loaded) = (b.param, b.fresh, b.loaded);
    for len in [31, 32, 33] {
        chain(&mut b, fresh, len);
    }
    let l = phi(&mut b, loaded, fresh);
    phi(&mut b, fresh, l);
    let cyc_a = phi(&mut b, param, fresh);
    let cyc_b = phi(&mut b, cyc_a, fresh);
    assert!(b.g.set_input(cyc_a, 1, cyc_b));
    assert_memo_matches_uncached(&b.g);
}

/// The cost: the uncached walk on `v[24]` makes `C(k) = 1 + C(k-1) +
/// C(k-2)` calls (about 150 000); the memoised walk computes each
/// `(node, depth)` once, so it makes at most one call per input of each such
/// entry plus the root call, and it reaches the tallest DAG the depth cap
/// admits (`v[33]`, 32 phis) with the same answer.
#[test]
fn j27_csehunt_memo_bounds_the_fibonacci_dag_walk() {
    let (g, v) = fib_dag(33);
    let mut expected = vec![1u64, 1];
    for k in 2..=24 {
        expected.push(1 + expected[k - 1] + expected[k - 2]);
    }
    let mut plain = PointsToWalk::uncached();
    let want = key(&plain.resolve(&g, v[24], 0));
    assert_eq!(plain.calls, expected[24]);
    assert!(plain.calls > 100_000);

    let mut cached = PointsToWalk::memoised();
    assert_eq!(key(&cached.resolve(&g, v[24], 0)), want);
    assert!(cached.calls <= 1 + 2 * 34 * 25, "calls {}", cached.calls);

    let mut tall = PointsToWalk::memoised();
    let (allocs, has_pre) = key(&tall.resolve(&g, v[33], 0));
    assert_eq!(allocs, Some(vec![v[1]]));
    assert!(has_pre);
    assert!(tall.calls <= 1 + 2 * 34 * 34, "calls {}", tall.calls);
    assert!(PointsToWalk::memoised().resolve(&g, v[33], 1).allocs.is_none());
}

/// EAHUNT's question, answered for the redundant-load pass: the shapes of
/// `tools/probes/jit/j27/J27EaBoundsRows.java` rows `bce-2d` and
/// `bce-field-reload`, built by hand in one block (the alias-aware arm).
mod load_cse_across_writes {
    use super::super::redundant_loads_pass;
    use crate::ir::{Graph, IrType, MemKind, NodeId, Op};

    /// What sits on the memory chain between the two reads.
    #[derive(Clone, Copy)]
    enum Between {
        Nothing,
        /// `m[i] = r` (the same element).
        SameRow,
        /// `m[j] = r` (`j` may equal `i`).
        OtherRow,
        /// `this.arr = other` (a field of another kind of storage).
        FieldStore,
    }

    struct Rows {
        g: Graph,
        second: NodeId,
    }

    /// The read done twice.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Read {
        /// `m[i]`.
        Row,
        /// `m.length`.
        Length,
        /// `this.arr` (field 0, not final).
        Field,
    }

    /// `this` is param 0, `m` param 1, `i` param 2, `j` param 3, `r` param 4;
    /// `what` is read twice with `between` between.
    fn rows(between: Between, what: Read) -> Rows {
        let mut g = Graph::empty(32);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let this = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let m = g.add(Op::Param(1), IrType::Ref, vec![start], None);
        let i = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let j = g.add(Op::Param(3), IrType::Int, vec![start], None);
        let r = g.add(Op::Param(4), IrType::Ref, vec![start], None);
        let field = g.add(Op::Const(0), IrType::Int, vec![], None);
        let read = |g: &mut Graph, mem: NodeId, pc: usize| match what {
            Read::Length => g.add(Op::ArrayLength, IrType::Int, vec![ctrl, mem, m], Some(pc)),
            Read::Row => g.add(
                Op::ArrayLoad(MemKind::Ref),
                IrType::Ref,
                vec![ctrl, mem, m, i],
                Some(pc),
            ),
            Read::Field => g.add(
                Op::Load(MemKind::Ref),
                IrType::Ref,
                vec![ctrl, mem, this, field],
                Some(pc),
            ),
        };
        let first = read(&mut g, mem0, 1);
        let mid = match between {
            Between::Nothing => first,
            Between::SameRow | Between::OtherRow => {
                let at = if matches!(between, Between::SameRow) { i } else { j };
                g.add(
                    Op::ArrayStore(MemKind::Ref),
                    IrType::Memory,
                    vec![ctrl, first, m, at, r],
                    Some(2),
                )
            }
            Between::FieldStore => g.add(
                Op::Store(MemKind::Ref),
                IrType::Memory,
                vec![ctrl, first, this, field, r],
                Some(2),
            ),
        };
        let second = read(&mut g, mid, 3);
        let _ = g.add(Op::Return, IrType::Control, vec![ctrl, second], Some(4));
        Rows { g, second }
    }

    fn second_survives(between: Between, what: Read) -> bool {
        let mut rows = rows(between, what);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_LOAD_CSE_ALIAS", Some("1"))],
            || redundant_loads_pass(&mut rows.g, false),
        );
        rows.g.nodes[rows.second as usize].op != Op::Dead
    }

    /// The control: with nothing between, the pass does merge each read, so
    /// the refusals below are the pass's, not the fixture's.
    #[test]
    fn j27_csehunt_two_reads_with_nothing_between_merge() {
        for what in [Read::Row, Read::Length, Read::Field] {
            assert!(!second_survives(Between::Nothing, what));
        }
    }

    /// `m[i]; m[i] = r; m[i]` and `m[i]; m[j] = r; m[i]`: the store may write
    /// the row the second read reads (a dynamic index proves nothing), so
    /// the second read stays.
    #[test]
    fn j27_csehunt_a_row_store_keeps_the_second_row_read() {
        assert!(second_survives(Between::SameRow, Read::Row));
        assert!(second_survives(Between::OtherRow, Read::Row));
    }

    /// `a = this.arr; this.arr = r; a2 = this.arr`: the second read of the
    /// non-final field stays.
    #[test]
    fn j27_csehunt_a_field_store_keeps_the_field_reload() {
        assert!(second_survives(Between::FieldStore, Read::Field));
    }

    /// `m.length` read twice around `this.arr = r` merges, which is right:
    /// the length belongs to the array `m` names, and no store writes a
    /// length word. (The length of a RE-READ `this.arr` has another base
    /// node, so it never shares a key with the first one's.)
    #[test]
    fn j27_csehunt_a_length_merges_across_a_field_store() {
        assert!(!second_survives(Between::FieldStore, Read::Length));
    }
}
