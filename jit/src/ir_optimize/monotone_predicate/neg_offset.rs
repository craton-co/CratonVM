//! j27 wave 2 (lane TIER1PROP, `CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET`, opt-in):
//! the planter half of BCE21-2's non-constant start.
//!
//! # What
//!
//! `for (int i = from; i < to; i++) d[i] = s[i - 1];` keeps the bounds check of
//! `s[i - 1]` on every iteration: BCE21-2 (`ir_check_elim`'s `a[i - c]` arm,
//! `CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE`) proves it only for a unit-stride
//! `i` from a CONSTANT start `>= c`, and the monotone predicate planted guards
//! for `a[p]` only. HotSpot's range-check elimination handles any affine
//! `i - c` with a pre-loop predicate on the start. Under the switch the
//! monotone planter ([`super::monotone_plan`]) also takes `a[p - c]`, `c` in
//! `1..=i32::MAX`, hanging off a `p < x` edge, and plants, in the pre-header,
//! deopting to the loop entry with the loop-entry frame:
//!
//! * `x <= a.length`, as for `a[p]` (none when `x` is `a.length` itself);
//! * `c <= init` for the φ's entry value `init`, unless `init` is an `int`
//!   constant `>= c` (then nothing is owed). A constant start below `c` faults
//!   on the first iteration, so no guard is planted and the check stays.
//!
//! `ir_check_elim::monotone_phi_at_least` then proves `c <= p < a.length` at
//! the access (its section notes carry the soundness argument). A floor also
//! states `0 <= init`, so it replaces that φ's `0 <= init` guard.
//!
//! # Why it is sound
//!
//! Every guard only deopts to the loop entry with the entry frame, as
//! [`super::install_range_predicates`]' guards do: nothing has run between the
//! pre-header and the header, so the interpreter runs the loop itself and
//! raises whatever the bytecode raises where it raises it. A guard that keeps
//! failing is withdrawn by the de-spec registry at the header bci
//! (`PredicateKind::Monotone`, the module's existing key), after which the
//! method keeps its checks.

use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};

/// `(p, c)` when the access index `idx` is a monotone header φ `p` of `phis`
/// (`c == 0`), or, with `neg_on`, `p - c` / `p + (-c)` for such a `p` and `c`
/// in `1..=i32::MAX` (`ir_check_elim::negative_offset_index`).
pub(super) fn indexed_phi(
    graph: &Graph,
    idx: NodeId,
    phis: &[NodeId],
    neg_on: bool,
) -> Option<(NodeId, i64)> {
    if phis.contains(&idx) {
        return Some((idx, 0));
    }
    if !neg_on {
        return None;
    }
    crate::ir_check_elim::negative_offset_index(graph, idx).filter(|(p, _)| phis.contains(p))
}

/// What an `a[p - c]` access owes the entry value of its φ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Floor {
    /// The entry value is an `int` constant `>= c`: nothing to plant.
    Holds,
    /// Plant `Guard(c <= v)` for the entry value `v`.
    Guard(NodeId),
    /// No guard can help (a constant below `c`, a missing or non-`int` entry
    /// value, or one computed in the loop): the access keeps its check.
    Never,
}

/// The [`Floor`] of the header φ `p` (entry input at `entry_slot`) for an
/// offset `c >= 1`.
pub(super) fn floor_of(
    graph: &Graph,
    p: NodeId,
    entry_slot: usize,
    c: i64,
    in_body: &dyn Fn(NodeId) -> bool,
) -> Floor {
    let Some(init) = graph.node_opt(p).and_then(|n| n.input_opt(entry_slot)) else {
        return Floor::Never;
    };
    if init == NO_NODE || in_body(init) {
        return Floor::Never;
    }
    let Some(n) = graph.node_opt(init) else {
        return Floor::Never;
    };
    if n.ty != IrType::Int {
        return Floor::Never;
    }
    match n.op {
        // Read at `int` width, as `ir_check_elim::int_const` reads it.
        Op::Const(k) if i64::from(k as i32) >= c => Floor::Holds,
        Op::Const(_) => Floor::Never,
        _ => Floor::Guard(init),
    }
}

/// The floors one loop's `a[p - c]` accesses owe.
#[derive(Debug, Default)]
pub(super) struct FloorUses {
    /// `(v, c)`: `Guard(c <= v)`, the largest `c` asked of `v`.
    pub(super) floors: Vec<(NodeId, i64)>,
    /// The φs indexed with an offset (their room tests' `0 <= x` guards are
    /// owed as for a plain `a[p]`).
    pub(super) phis: Vec<NodeId>,
    /// Some floor guard serves an own-length access, so it is worth planting
    /// with no length guard.
    pub(super) alone: bool,
}

impl FloorUses {
    /// Record that an access `a[p - c]` was kept with `floor`; `own_length`
    /// when its bound is `a.length` itself (no length guard).
    pub(super) fn note(&mut self, p: NodeId, floor: Floor, c: i64, own_length: bool) {
        if !self.phis.contains(&p) {
            self.phis.push(p);
        }
        let Floor::Guard(v) = floor else {
            return;
        };
        self.alone |= own_length;
        match self.floors.iter_mut().find(|(x, _)| *x == v) {
            Some((_, have)) => *have = (*have).max(c),
            None => self.floors.push((v, c)),
        }
    }
}

/// Where a loop's floor guards go: the header bci they resume at, the
/// loop-entry control they hang off, and the loop-entry frame copy that names
/// them.
pub(super) struct FloorSite {
    pub(super) header_bci: usize,
    pub(super) entry_pred: NodeId,
    pub(super) snapshot: u32,
}

/// Plant `Guard{header_bci}(c <= v)` for each floor, at most `budget` of them,
/// each named by `site.snapshot`. Stops at the first guard that cannot be
/// named (its nodes are taken back out). Returns the number planted.
pub(super) fn install_floor_guards(
    graph: &mut Graph,
    floors: &[(NodeId, i64)],
    site: FloorSite,
    budget: usize,
) -> usize {
    let pc = Some(site.header_bci);
    let mut added = 0usize;
    for &(v, c) in floors.iter().take(budget) {
        let lo = graph.add(Op::Const(c), IrType::Int, vec![], None);
        let cond = graph.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![lo, v], pc);
        let guard = graph.add(
            Op::Guard {
                bci: site.header_bci,
            },
            IrType::Void,
            vec![site.entry_pred, cond],
            pc,
        );
        if !graph.set_node_frame_snapshot(guard, site.snapshot) {
            graph.kill(guard);
            graph.kill(cond);
            graph.kill(lo);
            break;
        }
        added += 1;
    }
    added
}

#[cfg(test)]
mod j27_tier1prop_neg_offset_tests {
    use super::super::{install_monotone_plan, loop_headers, monotone_plan, MAX_MONOTONE_GUARDS};
    use super::*;
    use crate::ir::{MemKind, SafepointSnapshot};

    const SW: &str = "CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET";

    fn with_switch<T>(v: &str, f: impl FnOnce() -> T) -> T {
        cratonvm_types::flags::with_thread_overrides(&[(SW, Some(v))], f)
    }

    /// The loop's start value.
    #[derive(Clone, Copy)]
    enum Start {
        Param,
        Const(i64),
    }

    struct Loop {
        g: Graph,
        region: NodeId,
        entry: NodeId,
        /// The φ's entry value.
        i0: NodeId,
        /// The bound `n` of `i < n` (`a.length` for an own-length loop).
        n: NodeId,
        a: NodeId,
        /// The element load `a[i - c]`.
        el: NodeId,
    }

    /// `for (i = i0; i < n; i += step) s += a[i - c];` with `n` a parameter,
    /// or `a.length` when `own_length`. Header snapshot at bci 10.
    fn neg_loop(start: Start, own_length: bool, c: i64, step: i64) -> Loop {
        let mut g = Graph::empty(0);
        let st = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = st;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![st], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![st], None);
        let a = g.add(Op::Param(0), IrType::Ref, vec![st], None);
        let i0 = match start {
            Start::Param => g.add(Op::Param(1), IrType::Int, vec![st], None),
            Start::Const(k) => g.add(Op::Const(k), IrType::Int, vec![], None),
        };
        let n = if own_length {
            g.add(Op::ArrayLength, IrType::Int, vec![entry, mem0, a], Some(4))
        } else {
            g.add(Op::Param(2), IrType::Int, vec![st], None)
        };
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let stepc = g.add(Op::Const(step), IrType::Int, vec![], None);
        let cc = g.add(Op::Const(c), IrType::Int, vec![], None);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(10));
        let i = g.add(Op::Phi, IrType::Int, vec![region, i0], None);
        let s = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i, n], None);
        let head = g.add(Op::If, IrType::Control, vec![region, lt], Some(12));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        let idx = g.add(Op::Sub, IrType::Int, vec![i, cc], None);
        let el = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![stay, mem0, a, idx],
            Some(15),
        );
        let s1 = g.add(Op::Add, IrType::Int, vec![s, el], None);
        let i1 = g.add(Op::Add, IrType::Int, vec![i, stepc], None);
        g.nodes[region as usize].inputs.push(stay);
        g.nodes[i as usize].inputs.push(i1);
        g.nodes[s as usize].inputs.push(s1);
        g.exit = g.add(Op::Return, IrType::Void, vec![exit, s], Some(30));
        g.push_safepoint(SafepointSnapshot::new(10, vec![a, i, s, n], vec![], vec![]));
        Loop {
            g,
            region,
            entry,
            i0,
            n,
            a,
            el,
        }
    }

    fn backs(l: &Loop) -> Vec<NodeId> {
        loop_headers(&l.g)
            .into_iter()
            .find(|(r, _, _)| *r == l.region)
            .map(|(_, _, b)| b)
            .unwrap_or_default()
    }

    fn elided(g: &Graph, el: NodeId) -> bool {
        let schedule = crate::ir_schedule::schedule(g);
        crate::ir_check_elim::analyze(g, &schedule).bounds_elided(el)
    }

    /// Plan and install under the switch; the number of guards planted, or
    /// `None` when nothing is planned.
    fn plant(l: &mut Loop) -> Option<usize> {
        let b = backs(l);
        let plan = monotone_plan(&l.g, l.region, l.entry, &b)?;
        Some(install_monotone_plan(&mut l.g, plan, MAX_MONOTONE_GUARDS))
    }

    #[test]
    fn switch_off_plans_nothing_and_the_check_stays() {
        let mut l = neg_loop(Start::Param, false, 1, 1);
        with_switch("0", || {
            assert_eq!(plant(&mut l), None, "=0: a[i - 1] is no candidate");
            assert!(!elided(&l.g, l.el));
        });
    }

    #[test]
    fn a_parameter_start_gets_a_length_and_a_floor_guard_and_loses_the_check() {
        let mut l = neg_loop(Start::Param, false, 1, 1);
        with_switch("1", || {
            assert!(!elided(&l.g, l.el), "no guard yet: nothing proves i >= 1");
            let b = backs(&l);
            let plan = monotone_plan(&l.g, l.region, l.entry, &b).expect("a[i - 1] plans");
            assert_eq!(plan.bounds, vec![(l.n, vec![l.a])]);
            assert_eq!(plan.floors, vec![(l.i0, 1)]);
            assert!(plan.non_negative.is_empty(), "the floor states 0 <= i0 too");
            let before = l.g.nodes.len();
            assert_eq!(install_monotone_plan(&mut l.g, plan, MAX_MONOTONE_GUARDS), 2);
            // Both guards resume at the header with the loop-entry frame.
            let guards: Vec<&crate::ir::Node> = l.g.nodes[before..]
                .iter()
                .filter(|n| matches!(n.op, Op::Guard { bci: 10 }))
                .collect();
            assert_eq!(guards.len(), 2);
            assert!(guards
                .iter()
                .all(|n| n.inputs.first() == Some(&l.entry) && n.frame_snapshot.is_some()));
            assert!(elided(&l.g, l.el), "1 <= i < n <= a.length: a[i - 1] is in range");
        });
    }

    #[test]
    fn an_own_length_access_plants_a_lone_floor() {
        let mut l = neg_loop(Start::Param, true, 2, 1);
        with_switch("1", || {
            let b = backs(&l);
            let plan = monotone_plan(&l.g, l.region, l.entry, &b).expect("a[i - 2] plans");
            assert!(plan.bounds.is_empty(), "i < a.length needs no length guard");
            assert_eq!(plan.floors, vec![(l.i0, 2)]);
            assert!(plan.non_negative_alone);
            assert_eq!(install_monotone_plan(&mut l.g, plan, MAX_MONOTONE_GUARDS), 1);
            assert!(elided(&l.g, l.el));
        });
    }

    #[test]
    fn a_constant_start_below_the_offset_keeps_the_check() {
        // `a[i - 1]` from `i = 0` faults on the first trip: no guard can help.
        let mut l = neg_loop(Start::Const(0), false, 1, 1);
        with_switch("1", || {
            assert_eq!(plant(&mut l), None);
            assert!(!elided(&l.g, l.el));
        });
    }

    #[test]
    fn a_constant_start_at_the_offset_owes_no_floor_and_is_proved() {
        let mut l = neg_loop(Start::Const(2), false, 2, 1);
        with_switch("1", || {
            let b = backs(&l);
            let plan = monotone_plan(&l.g, l.region, l.entry, &b).expect("a[i - 2] plans");
            assert!(plan.floors.is_empty(), "2 >= 2 by construction");
            assert_eq!(install_monotone_plan(&mut l.g, plan, MAX_MONOTONE_GUARDS), 1);
            assert!(elided(&l.g, l.el));
        });
        // The reader arm is the switch's too.
        with_switch("0", || assert!(!elided(&l.g, l.el)));
    }

    #[test]
    fn a_floor_below_the_offset_does_not_prove_the_access() {
        let mut l = neg_loop(Start::Param, false, 2, 1);
        with_switch("1", || {
            let before = l.g.nodes.len();
            assert_eq!(plant(&mut l), Some(2));
            assert!(elided(&l.g, l.el));
            // Lower the planted floor to `1 <= i0`: `a[i - 2]` at `i == 1`
            // would read index -1, so the check must come back.
            let lo = (before..l.g.nodes.len())
                .find(|&id| matches!(l.g.nodes[id].op, Op::Const(2)))
                .expect("the floor's constant");
            l.g.nodes[lo].op = Op::Const(1);
            assert!(!elided(&l.g, l.el));
        });
    }

    #[test]
    fn a_stride_of_three_keeps_the_check_and_a_decreasing_index_plans_nothing() {
        // `i += 3` passes the planter's shape screen, but the `i < n` edge
        // alone bounds only a unit step against wrapping (the monotone rule's
        // step (a)); with no room test the reader keeps the check (fail
        // closed), although the guards went in.
        let mut l = neg_loop(Start::Param, false, 1, 3);
        with_switch("1", || {
            assert_eq!(plant(&mut l), Some(2));
            assert!(!elided(&l.g, l.el));
        });
        // `i += -1` is no monotone shape: nothing is planned.
        let mut l = neg_loop(Start::Param, false, 1, -1);
        with_switch("1", || assert_eq!(plant(&mut l), None));
    }

    #[test]
    fn floor_uses_keep_the_largest_offset_per_value() {
        let mut u = FloorUses::default();
        u.note(7, Floor::Guard(3), 1, false);
        u.note(7, Floor::Guard(3), 4, false);
        u.note(8, Floor::Holds, 2, true);
        assert_eq!(u.floors, vec![(3, 4)]);
        assert_eq!(u.phis, vec![7, 8]);
        assert!(!u.alone, "a Holds floor plants nothing, alone or not");
        u.note(9, Floor::Guard(5), 1, true);
        assert!(u.alone);
    }

    // j27 wave 6 (lane T1BCE): the reader's two floor hazards. Each starts
    // from a proved loop and breaks one premise of `monotone_phi_at_least`;
    // a reader that took any `Guard(c <= v)` in the graph as a fact about the
    // entry value would keep the check elided.

    /// The planted `Guard(c <= v)` among the nodes from `from` on.
    fn floor_guard(g: &Graph, from: usize, v: NodeId, c: i64) -> usize {
        (from..g.nodes.len())
            .find(|&id| {
                matches!(g.nodes[id].op, Op::Guard { .. })
                    && g.nodes[id].inputs.get(1).is_some_and(|&cond| {
                        let cn = &g.nodes[cond as usize];
                        cn.op == Op::Cmp(CmpOp::Le)
                            && cn.inputs.get(1) == Some(&v)
                            && cn
                                .inputs
                                .first()
                                .is_some_and(|&k| g.nodes[k as usize].op == Op::Const(c))
                    })
            })
            .expect("the floor guard")
    }

    #[test]
    fn a_floor_guard_below_the_entry_edge_does_not_floor_the_start() {
        let mut l = neg_loop(Start::Param, false, 1, 1);
        with_switch("1", || {
            let before = l.g.nodes.len();
            assert_eq!(plant(&mut l), Some(2));
            assert!(elided(&l.g, l.el));
            // Hang the floor off the loop's stay edge: it no longer runs
            // before the entry edge, so nothing floors `i0` there (the first
            // trip would read `a[i0 - 1]` unguarded).
            let stay = l.g.nodes[l.el as usize].inputs[0];
            let guard = floor_guard(&l.g, before, l.i0, 1);
            l.g.nodes[guard].inputs[0] = stay;
            assert!(!elided(&l.g, l.el));
        });
    }

    #[test]
    fn a_floor_on_another_value_does_not_floor_the_start() {
        let mut l = neg_loop(Start::Param, false, 1, 1);
        with_switch("1", || {
            let before = l.g.nodes.len();
            assert_eq!(plant(&mut l), Some(2));
            assert!(elided(&l.g, l.el));
            // `1 <= n` says nothing about `i0`.
            let guard = floor_guard(&l.g, before, l.i0, 1);
            let cond = l.g.nodes[guard].inputs[1] as usize;
            l.g.nodes[cond].inputs[1] = l.n;
            assert!(!elided(&l.g, l.el));
        });
    }
}
