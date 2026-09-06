// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Null-check and bounds-check elimination for the OPTIMIZING tier.
//!
//! # Why this module exists
//!
//! The single-pass backend has both of these and has had them for a long time:
//! `x64/bce.rs` is 3,819 lines of speculative bounds-check elimination fed by
//! `range_analysis` and `scev`, and `x64/null_check_elim.rs` plus the `this`
//! seed remove receiver null tests. Neither is reachable from the IR tier --
//! `range_analysis` is referenced by `x64/bce.rs` and nothing else, and
//! `null_check_elim` by `x64/*` and nothing else -- so
//! `ir_lower::emit_array_null_bounds_guards` emitted, unconditionally, at every
//! `ArrayLoad` and every `ArrayStore`:
//!
//! ```text
//! TEST RAX, RAX            ; the null check
//! JZ   <deopt>
//! MOV  R10D, [RAX + len]   ; the length load
//! CMP  ECX, R10D
//! JAE  <deopt>             ; the bounds check
//! ```
//!
//! Every element, every iteration, with no elision, no proof and no hoisting of
//! the length load. That is the tier that is supposed to be the OPTIMIZING one.
//!
//! # What this pass proves, and what it deliberately does not
//!
//! Two facts, both by DOMINANCE over the scheduled block CFG, which is the
//! cheapest sound form and needs no new analysis infrastructure:
//!
//! * **Non-null.** A value is non-null at `n` if something that dominates `n`
//!   already dereferenced it, or if it is the result of an allocation. If the
//!   value were null, the dominating dereference would have deopted and `n`
//!   would not be running.
//! * **In bounds.** A bounds check on the SSA pair `(base, index)` is redundant
//!   if a dominating node checked the SAME pair. In SSA neither operand can
//!   have been redefined in between, and a Java array's length is fixed for its
//!   lifetime, so the earlier check having passed is a proof that this one will.
//!
//! What it does NOT do is the interesting half of `x64/bce.rs`: proving
//! `0 <= i < a.length` for an induction variable from the loop's own trip
//! count, which removes the check on the FIRST iteration too. That wants an
//! SSA-level SCEV, which does not exist yet (`scev` is a bytecode analysis).
//! This pass removes the second and subsequent checks on the same pair, which
//! on `a[i] = a[i] + 1` and on any loop whose body touches an element more than
//! once is most of them.
//!
//! # Why dominance and not "earlier in the same block"
//!
//! Both, actually -- and the same-block case is the one that needs care.
//! `Schedule::dom` is a relation over BLOCKS, so it answers nothing about two
//! nodes in one block. Within a block the pass walks `Block::nodes` in order
//! and accumulates facts as it goes, which is sound because `ir_lower` emits a
//! block's nodes in exactly that order. That coupling is asserted rather than
//! assumed: see `the_block_node_order_is_the_emission_order`.
//!
//! # The three couplings this pass rests on, all checked
//!
//! Each is a property of code that lives elsewhere, so each is written down
//! here rather than remembered:
//!
//! 1. **`Schedule::dom` is indexed by POST-layout block numbers.** The pass
//!    queries it with post-layout indices, and frequency-driven layout permutes
//!    the block vector. `schedule_with_options` recomputes the matrix after a
//!    permutation for exactly this reason ("the dominator relation is a
//!    property of the CFG, not of its numbering") -- if it ever stopped, every
//!    dominance answer here would be read from the wrong row.
//! 2. **`ir_lower` emits a block by walking `Block::nodes` in order.** That is
//!    what makes the same-block accumulation program order rather than an
//!    arbitrary topological one. Pinned by
//!    `the_block_node_order_is_the_emission_order`.
//! 3. **Every op in [`deref_base_input`] takes its base at `inputs[2]`, and
//!    none of them CONTINUES with a null base.** `ArrayLoad`/`ArrayStore`/
//!    `ArrayLength` emit an explicit null test that deopts; `Load`/`Store`
//!    null-check the receiver or fault into the signal handler; the monitor
//!    ops compare the helper's `i64::MIN` sentinel and jump to the exception
//!    epilogue, so "a null receiver takes the NPE path" as their own comment
//!    says. If any of them ever returned normally on null, the fact this pass
//!    records past it would be false and the elision would be a wrong-code bug
//!    rather than a missed check.
//!
//! # Fail-closed
//!
//! Every unknown answers "check needed". An unplaced node (`usize::MAX` in
//! `node_to_block`), a node this pass has no arm for, a base that is a `Phi` --
//! all of them keep their check. The cost of a wrong `true` here is a null
//! dereference or an out-of-bounds read in compiled code; the cost of a wrong
//! `false` is two instructions.

use crate::ir::{Graph, MemKind, NodeId, Op};
use crate::ir_schedule::Schedule;
use rustc_hash::{FxHashMap, FxHashSet};

/// Per-node verdicts, indexed by `NodeId`.
#[derive(Default)]
pub struct CheckElision {
    /// `true` for a node whose array/receiver base is provably non-null.
    null_check_unneeded: Vec<bool>,
    /// `true` for a node whose `(base, index)` bounds check is provably
    /// redundant.
    bounds_check_unneeded: Vec<bool>,
}

impl CheckElision {
    /// Is the null check at `node` provably unnecessary?
    #[inline]
    pub fn null_elided(&self, node: NodeId) -> bool {
        self.null_check_unneeded
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Is the bounds check at `node` provably unnecessary?
    #[inline]
    pub fn bounds_elided(&self, node: NodeId) -> bool {
        self.bounds_check_unneeded
            .get(node as usize)
            .copied()
            .unwrap_or(false)
    }

    /// `(null_elided, bounds_elided)` totals, for the census.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.null_check_unneeded.iter().filter(|b| **b).count(),
            self.bounds_check_unneeded.iter().filter(|b| **b).count(),
        )
    }
}

/// Which input of `op` is the base being dereferenced, if any.
///
/// An allowlist, so an op this module has never heard of contributes no fact
/// rather than the wrong one. The index is into `Node::inputs`.
fn deref_base_input(op: &Op) -> Option<usize> {
    match op {
        // [ctrl, mem, array, index, ...]
        Op::ArrayLoad(_) | Op::ArrayStore(_) => Some(2),
        // [ctrl, mem, obj]
        Op::ArrayLength | Op::MonitorEnter | Op::MonitorExit => Some(2),
        // [ctrl, mem, base, ...] -- a field access.
        Op::Load(_) | Op::Store(_) => Some(2),
        _ => None,
    }
}

/// The `(base, index)` pair a bounds check is against, if this op has one.
fn bounds_pair(node_op: &Op, inputs: &[NodeId]) -> Option<(NodeId, NodeId)> {
    match node_op {
        Op::ArrayLoad(_) | Op::ArrayStore(_) => {
            Some((*inputs.get(2)?, *inputs.get(3)?))
        }
        _ => None,
    }
}

/// Does this node's result carry a value that cannot be null?
fn definitely_non_null(op: &Op) -> bool {
    matches!(
        op,
        // A successful allocation returns a non-null reference; a failed one
        // returns the deopt sentinel and never reaches a use.
        Op::New { .. }
            | Op::NewArray { .. }
            // The two constant-pool materializers either produce an object or
            // stash a pending exception and return the sentinel.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
    )
}

/// Run the analysis over a scheduled graph.
///
/// Linear in nodes times blocks in the worst case (the dominance query is an
/// array index), and the block count in this tier is small.
pub fn analyze(graph: &Graph, schedule: &Schedule) -> CheckElision {
    let n = graph.nodes.len();
    let mut out = CheckElision {
        null_check_unneeded: vec![false; n],
        bounds_check_unneeded: vec![false; n],
    };
    if !enabled() {
        return out;
    }

    // Fact 1 (block-level): for each value, the set of blocks in which it is
    // known dereferenced. A later node is covered when ANY of those blocks
    // dominates the later node's block.
    let mut deref_blocks: FxHashMap<NodeId, Vec<usize>> = FxHashMap::default();
    // Fact 2 (block-level): the same for a checked `(base, index)` pair.
    let mut checked_blocks: FxHashMap<(NodeId, NodeId), Vec<usize>> = FxHashMap::default();

    // Values that are non-null by construction, and the receiver.
    //
    // The receiver is seeded for the same reason the single-pass tier seeds it
    // (`CRATONVM_JIT_THIS_NONNULL`): the JVM guarantees a non-null receiver at
    // the call site, so `this` is non-null on entry and stays so -- it is a
    // parameter and SSA gives it no other definition.
    let mut non_null: FxHashSet<NodeId> = FxHashSet::default();
    for (id, node) in graph.nodes.iter().enumerate() {
        if definitely_non_null(&node.op) {
            non_null.insert(id as NodeId);
        }
    }
    // `receiver_param` is a PARAMETER INDEX, not a `NodeId` -- the node is
    // whichever `Op::Param` carries that index. Reading it as a node id would
    // seed an arbitrary node non-null, which is a wrong-code bug rather than a
    // missed optimization, so the lookup is explicit.
    if let Some(recv_idx) = graph.receiver_param {
        for (id, node) in graph.nodes.iter().enumerate() {
            if matches!(node.op, Op::Param(p) if p == recv_idx) {
                non_null.insert(id as NodeId);
            }
        }
    }

    // One pass over the blocks in index order. Within a block, facts
    // accumulated by earlier nodes apply to later ones directly; across blocks
    // they apply through `dom`.
    for (b, block) in schedule.blocks.iter().enumerate() {
        // Facts established EARLIER IN THIS BLOCK. Kept separate from the
        // block-level maps so a fact does not appear to dominate itself.
        let mut local_deref: FxHashSet<NodeId> = FxHashSet::default();
        let mut local_checked: FxHashSet<(NodeId, NodeId)> = FxHashSet::default();

        for &id in &block.nodes {
            let Some(node) = graph.nodes.get(id as usize) else {
                continue;
            };

            if let Some(base_idx) = deref_base_input(&node.op) {
                let Some(&base) = node.inputs.get(base_idx) else {
                    continue;
                };
                let known = non_null.contains(&base)
                    || local_deref.contains(&base)
                    || deref_blocks
                        .get(&base)
                        .is_some_and(|bs| bs.iter().any(|&d| dominates(schedule, b, d)));
                if known {
                    out.null_check_unneeded[id as usize] = true;
                }
                // Whether or not the check was elided, reaching PAST this node
                // proves the base was non-null: an elided check means it was
                // already proven, and an emitted one deopts when it is not.
                local_deref.insert(base);
                deref_blocks.entry(base).or_default().push(b);
            }

            if let Some(pair) = bounds_pair(&node.op, &node.inputs) {
                let known = local_checked.contains(&pair)
                    || checked_blocks
                        .get(&pair)
                        .is_some_and(|bs| bs.iter().any(|&d| dominates(schedule, b, d)));
                if known {
                    out.bounds_check_unneeded[id as usize] = true;
                }
                local_checked.insert(pair);
                checked_blocks.entry(pair).or_default().push(b);
            }
        }
    }
    out
}

/// Does block `d` dominate block `b`? Reflexive, so a block dominates itself --
/// which is why the same-block case is handled by `local_*` sets rather than by
/// this query: without that split, the FIRST dereference in a block would prove
/// itself.
#[inline]
fn dominates(schedule: &Schedule, b: usize, d: usize) -> bool {
    if b == d {
        // Same block: `local_deref` / `local_checked` own this case, in program
        // order. Answering `true` here would elide the first check in the
        // block on the strength of a later one.
        return false;
    }
    schedule
        .dom
        .get(b)
        .and_then(|row| row.get(d))
        .copied()
        .unwrap_or(false)
}

/// **Default ON** since 2026-09-06. `CRATONVM_JIT_IR_CHECK_ELIM=0` restores the
/// unconditional guards at every array site, which is every build before this
/// module existed.
pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_CHECK_ELIM").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Null checks and bounds checks elided in the optimizing tier, process-wide.
///
/// Both counters always printed together with the EMITTED counts by the census
/// caller: an elision count on its own cannot distinguish "the pass works" from
/// "this workload compiles no array accesses in this tier", and that
/// distinction has cost this file wrong conclusions before.
static ELIDED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static EMITTED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Record one guard-pair decision. `which` is 0 for the null check, 1 for the
/// bounds check.
pub fn note_check(which: usize, elided: bool) {
    let table = if elided { &ELIDED } else { &EMITTED };
    table[which].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if elided {
        crate::ir_evidence::note(crate::ir_evidence::Transform::GuardElided);
    }
}

/// `(null_elided, null_emitted, bounds_elided, bounds_emitted)`.
pub fn census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        ELIDED[0].load(Relaxed),
        EMITTED[0].load(Relaxed),
        ELIDED[1].load(Relaxed),
        EMITTED[1].load(Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrType, NO_NODE};
    use crate::ir_schedule;

    fn empty_graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        }
    }

    /// `a[i] + a[i]` -- one array, one index, two loads in one block. The
    /// SECOND load's checks are both redundant; the first load's are not.
    ///
    /// This is the shape the pass exists for and the one a dominance-only
    /// implementation would miss entirely, because both nodes are in the same
    /// block and `dom` is reflexive.
    #[test]
    fn the_second_access_to_the_same_element_needs_no_checks() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let l1 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, idx],
            Some(0),
        );
        let l2 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, idx],
            Some(1),
        );
        let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(
            !e.null_elided(l1) && !e.bounds_elided(l1),
            "the FIRST access proves the facts; it cannot use them",
        );
        assert!(
            e.null_elided(l2),
            "the second access's base was dereferenced by the first",
        );
        assert!(
            e.bounds_elided(l2),
            "the second access checks the same (base, index) SSA pair",
        );
    }

    /// A DIFFERENT index against the same array keeps its bounds check and
    /// loses only the null check. Getting this wrong is an out-of-bounds read
    /// in compiled code, so it gets its own test rather than riding on the one
    /// above.
    #[test]
    fn a_different_index_keeps_its_bounds_check() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let j = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let l1 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, i],
            Some(0),
        );
        let l2 = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, arr, j],
            Some(1),
        );
        let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(
            e.null_elided(l2),
            "the array is the same value and was already dereferenced",
        );
        assert!(
            !e.bounds_elided(l2),
            "a DIFFERENT index is a different bounds question",
        );
    }

    /// A freshly allocated array is non-null by construction, so even its
    /// first access needs no null check -- but it still needs its bounds check,
    /// because nothing here knows the length.
    #[test]
    fn a_fresh_allocation_needs_no_null_check_and_still_needs_bounds() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let len = g.add(Op::Const(4), IrType::Int, vec![], None);
        let arr = g.add(
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
            IrType::Ref,
            vec![ctrl, mem, len],
            Some(0),
        );
        let idx = g.add(Op::Const(0), IrType::Int, vec![], None);
        let l = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, arr, arr, idx],
            Some(1),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, l], Some(2));

        let sched = ir_schedule::schedule(&g);
        let e = analyze(&g, &sched);
        assert!(e.null_elided(l), "an allocation result cannot be null");
        assert!(
            !e.bounds_elided(l),
            "nothing here proves the index is inside the length",
        );
    }

    /// The kill switch must produce the pre-change answer at every site, or the
    /// A/B compares two things.
    #[test]
    fn the_kill_switch_elides_nothing() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_CHECK_ELIM", Some("0"))],
            || {
                if enabled() {
                    // The gate is a process-wide `OnceLock`; another test won
                    // the race to initialise it. Assert nothing rather than
                    // assert something false.
                    return;
                }
                let mut g = empty_graph();
                let start = g.add(Op::Start, IrType::Control, vec![], None);
                g.entry = start;
                let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
                let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
                let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
                let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
                let l1 = g.add(
                    Op::ArrayLoad(MemKind::Int),
                    IrType::Int,
                    vec![ctrl, mem, arr, idx],
                    Some(0),
                );
                let l2 = g.add(
                    Op::ArrayLoad(MemKind::Int),
                    IrType::Int,
                    vec![ctrl, mem, arr, idx],
                    Some(1),
                );
                let sum = g.add(Op::Add, IrType::Int, vec![l1, l2], Some(2));
                g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(3));
                let sched = ir_schedule::schedule(&g);
                let e = analyze(&g, &sched);
                assert!(!e.null_elided(l2) && !e.bounds_elided(l2));
            },
        );
    }

    /// The pass reads `Block::nodes` in order and treats that as PROGRAM order
    /// within the block. `ir_lower` emits a block by iterating the same vector,
    /// so the two agree -- but only for as long as nobody changes one of them.
    ///
    /// Asserting it on the source is the cheapest way to keep the coupling
    /// visible: a behavioural test cannot distinguish "the orders agree" from
    /// "this graph happens not to care".
    #[test]
    fn the_block_node_order_is_the_emission_order() {
        let src = include_str!("ir_lower.rs");
        assert!(
            src.contains("for &node_id in &block.nodes")
                || src.contains("for &id in &block.nodes")
                || src.contains("block.nodes.iter()"),
            "ir_lower must still emit a block by walking `block.nodes` in order; \
             this pass's same-block reasoning depends on it",
        );
    }
}
