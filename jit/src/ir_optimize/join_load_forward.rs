//! nio21 wave 16 (lane L3): forward a field read to the value an earlier read
//! or store of the SAME cell left, through forward merges
//! (`CRATONVM_JIT_IR_JOIN_LOAD_FORWARD`).
//!
//! # Why
//!
//! `UTF_8$Encoder.encodeBufferLoop` splices, per char, `src.hasRemaining()`
//! and then `src.get()` (`nextGetIndex`), and `dst.hasRemaining()` and then
//! `dst.put(b)` (`nextPutIndex`). Each pair reads `position` and `limit` of the
//! same buffer twice, and between the two reads sits the boolean join of the
//! spliced `hasRemaining` (`if_icmpge; iconst_1; goto; iconst_0`). The
//! builder's field memo clears at every join (the host census on the w15
//! binary: `field-load memo ... hits=0 ... cleared: join=13`), and
//! `ir_optimize`'s load CSE serves a read across blocks only from a
//! DOMINATING block reached through branch projections and single-input
//! joins (`control_dominates`), and never from a store. So the loop re-read
//! four fields per ASCII char that the same iteration had just read, and the
//! multi-byte arms re-read `position` right after `nextPutIndex` stored it
//! (`put(b1); put(b2)`).
//!
//! # What
//!
//! For every full-form field `Op::Load` `b` with a constant index and a
//! resolved declaring class, the memory chain is walked backwards from `b`'s
//! token:
//!
//! * a `Load` of the same cell (same base node, index, declaring class and
//!   kind) answers its own value;
//! * a `Store` to the same cell answers the value it stored (only for the
//!   kinds whose stored value IS the read value: `int`, `long`, `float`,
//!   `double`, references -- a narrow field truncates on store);
//! * a memory φ at a FORWARD merge answers a value φ at that merge whose
//!   inputs are the answers on each incoming path (one value when every path
//!   answers the same node);
//! * any other node is stepped over when it writes nothing the read may alias
//!   (`Graph::may_alias`, with field identity), is plain (no acquire,
//!   release or volatile), allocates nothing and is no safepoint a collector
//!   may run at (a deopt-only one is fine: no compiled code runs after it).
//!
//! Anything else -- a loop header's memory φ, a cycle, a call, a monitor, a
//! narrow store, a node without a token, the budget -- answers nothing and the
//! read stays.
//!
//! # Why it is sound
//!
//! * Memory is SSA. Every node on `b`'s chain executed before `b` in every
//!   execution that reaches `b`, and so did the merge whose φ the walk crosses
//!   (a definition dominates its uses). The value φ is placed AT that merge,
//!   its inputs come from the paths into it, and each input value was
//!   computed on its path before the merge.
//! * Between the answering access and `b` nothing wrote the cell (every
//!   writer on the chain was proven disjoint), and no synchronisation action
//!   lies between them. Reading what this thread last read or wrote is then a
//!   legal JMM execution for a plain field (the argument of
//!   `eliminate_hoisted_duplicate_loads` and `loop_field_forward`); volatile
//!   reads are never removed and volatile accesses stop the walk.
//! * No loop-carried value is made: the walk refuses at a loop header, so
//!   nothing here moves a read across a back edge or a safepoint poll, and an
//!   OSR entry at a header sees exactly the graph it saw before.
//! * A read that carries an exception edge (`crate::ir::exception_edge_anchors`)
//!   is never removed: its handler edge would go dead.
//! * `b`'s own deopt point disappears with it. `b` could only deopt where the
//!   earlier access to the same base did (null base, layout), and that one
//!   ran first on every path and did not.

use super::{loop_headers, FxHashMap, FxHashSet};
use crate::ir::{AliasClass, Graph, IrType, MemKind, MemOrder, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_JOIN_LOAD_FORWARD` (nio21 wave 16, lane L3) -- **default
/// ON**, `=0` is the kill switch. Read once per run of the pass.
pub(super) fn join_load_forward_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_JOIN_LOAD_FORWARD")
}

/// The most memory nodes one read's walk may visit.
const WALK_BUDGET: usize = 512;

/// One field cell: `base.#idx` declared by `decl`, at `kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cell {
    base: NodeId,
    idx: i64,
    decl: u32,
    kind: MemKind,
}

/// The cell a full-form field `Load` / `Store` addresses, or `None`.
fn cell_of(graph: &Graph, id: NodeId) -> Option<Cell> {
    let node = graph.node_opt(id)?;
    let kind = match node.op {
        Op::Load(k) | Op::Store(k) => k,
        _ => return None,
    };
    crate::ir::memory_token_slot(node)?;
    let base = node.input_opt(2)?;
    let idx = match graph.node_opt(node.input_opt(3)?)?.op {
        Op::Const(v) => v,
        _ => return None,
    };
    let decl = graph.field_decl_class.get(&id).copied()?;
    (base != NO_NODE).then_some(Cell {
        base,
        idx,
        decl,
        kind,
    })
}

/// May a stored value stand for a read of a field of this kind? A narrow
/// field (`byte`, `char`, `short`, `boolean`) truncates the `int` it is
/// given; the read sees the truncated value.
fn store_value_is_read_value(kind: MemKind) -> bool {
    matches!(
        kind,
        MemKind::Int | MemKind::Long | MemKind::Float | MemKind::Double | MemKind::Ref
    )
}

/// What [`forward_join_loads`] did.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JoinForwardCensus {
    /// Reads removed.
    pub(super) removed: usize,
    /// ... of which answered by a stored value.
    pub(super) from_stores: usize,
    /// Value φs built at forward merges.
    pub(super) phis: usize,
}

/// Run the pass. Returns its census (all zero when nothing changed).
pub(super) fn forward_join_loads(graph: &mut Graph) -> JoinForwardCensus {
    let mut census = JoinForwardCensus::default();
    // A memory op in a COMPACT layout carries no token, so the walk would
    // step over it: refuse the graph, as `redundant_loads_pass` does.
    if graph.nodes.iter().any(|n| {
        n.op != Op::Dead
            && n.op.memory_shape().is_some()
            && crate::ir::memory_token_slot(n).is_none()
    }) {
        return census;
    }
    let headers: FxHashSet<NodeId> = loop_headers(graph).into_iter().map(|l| l.0).collect();
    let anchors = crate::ir::exception_edge_anchors(graph);
    let candidates: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Load(_)) && !n.is_volatile_access())
        .map(|(i, _)| i as NodeId)
        .filter(|id| !anchors.contains(id))
        .collect();
    for b in candidates {
        let Some(node) = graph.node_opt(b) else {
            continue;
        };
        if node.op == Op::Dead {
            continue;
        }
        let ty = node.ty;
        let Some(cell) = cell_of(graph, b) else {
            continue;
        };
        let Some(token) = crate::ir::memory_token_slot(node).and_then(|t| node.input_opt(t)) else {
            continue;
        };
        let reads = graph.memory_effect(b).reads;
        let mut walk = Walk {
            target: b,
            cell,
            ty,
            reads,
            headers: &headers,
            memo: FxHashMap::default(),
            budget: WALK_BUDGET,
            plan: Vec::new(),
            from_store: false,
        };
        let Some(answer) = walk.value_at(graph, token) else {
            continue;
        };
        if answer == b || answer == NO_NODE {
            continue;
        }
        // Build the value φs the answer needs, inner ones first (they were
        // planned in post-order), then rewire.
        let mut built: FxHashMap<NodeId, NodeId> = FxHashMap::default();
        let mut ok = true;
        for step in std::mem::take(&mut walk.plan) {
            let inputs: Vec<NodeId> = step.inputs.iter().map(|v| resolve(&built, *v)).collect();
            if inputs.iter().any(|&v| v == NO_NODE) {
                ok = false;
                break;
            }
            let mut all = vec![step.ctrl];
            all.extend(inputs);
            let phi = graph.add(Op::Phi, ty, all, step.pc);
            built.insert(step.placeholder, phi);
            census.phis += 1;
        }
        if !ok {
            continue;
        }
        let value = resolve(&built, answer);
        if value == NO_NODE || value == b {
            continue;
        }
        if walk.from_store {
            census.from_stores += 1;
        }
        // Token users first: they want the chain, not the value.
        for user in graph.users_of(b) {
            let Some(un) = graph.node_opt(user) else {
                continue;
            };
            let slots: Vec<usize> = un
                .inputs
                .as_slice()
                .iter()
                .enumerate()
                .filter(|&(i, &x)| x == b && crate::ir::is_memory_token_slot(un, i))
                .map(|(i, _)| i)
                .collect();
            for slot in slots {
                graph.set_input(user, slot, token);
            }
        }
        graph.replace_all_uses(b, value);
        graph.kill(b);
        census.removed += 1;
    }
    census
}

/// A placeholder answer resolved to the φ built for it, or itself.
fn resolve(built: &FxHashMap<NodeId, NodeId>, v: NodeId) -> NodeId {
    built.get(&v).copied().unwrap_or(v)
}

/// A value φ to build at `ctrl` once the walk succeeded.
struct PhiStep {
    /// The id the walk handed out for this φ before it existed (a fresh id
    /// past every live node, never a real node).
    placeholder: NodeId,
    ctrl: NodeId,
    pc: Option<usize>,
    inputs: Vec<NodeId>,
}

struct Walk<'a> {
    /// The read being forwarded: reaching it again means a cycle the
    /// header set did not name, and answers nothing.
    target: NodeId,
    cell: Cell,
    ty: IrType,
    reads: AliasClass,
    headers: &'a FxHashSet<NodeId>,
    /// `mem -> answer`; `None` while the node is being walked (a cycle).
    memo: FxHashMap<NodeId, Option<NodeId>>,
    budget: usize,
    plan: Vec<PhiStep>,
    from_store: bool,
}

impl Walk<'_> {
    fn value_at(&mut self, graph: &Graph, mem: NodeId) -> Option<NodeId> {
        let mut path: Vec<NodeId> = Vec::new();
        let mut cur = mem;
        let answer = loop {
            if let Some(entry) = self.memo.get(&cur) {
                // `None`: a cycle back into a node being walked.
                break (*entry)?;
            }
            self.budget = self.budget.checked_sub(1)?;
            if cur == self.target {
                return None;
            }
            let node = graph.node_opt(cur)?;
            if node.op == Op::Dead {
                return None;
            }
            if node.op == Op::Phi {
                if node.ty != IrType::Memory {
                    return None;
                }
                break self.phi_value(graph, cur)?;
            }
            if node.is_volatile_access() {
                return None;
            }
            match node.op {
                Op::Load(_) if cell_of(graph, cur) == Some(self.cell) => {
                    if graph.node_opt(cur).map(|n| n.ty) != Some(self.ty) {
                        return None;
                    }
                    break cur;
                }
                Op::Store(_) if cell_of(graph, cur) == Some(self.cell) => {
                    if !store_value_is_read_value(self.cell.kind) {
                        return None;
                    }
                    let v = node.input_opt(4)?;
                    if v == NO_NODE || graph.node_opt(v).map(|n| n.ty) != Some(self.ty) {
                        return None;
                    }
                    self.from_store = true;
                    break v;
                }
                _ => {}
            }
            let eff = graph.memory_effect(cur);
            if eff.order != MemOrder::Plain
                || eff.allocates
                || (eff.safepoint && !graph.safepoint_is_deopt_only(cur))
                || graph.may_alias(eff.writes, self.reads)
            {
                return None;
            }
            let token = crate::ir::memory_token_slot(node).and_then(|t| node.input_opt(t))?;
            path.push(cur);
            cur = token;
        };
        for p in path {
            self.memo.insert(p, Some(answer));
        }
        Some(answer)
    }

    /// The value at the memory φ `phi`: a value φ over its predecessors, or
    /// the one value they all answer.
    fn phi_value(&mut self, graph: &Graph, phi: NodeId) -> Option<NodeId> {
        let node = graph.node_opt(phi)?;
        let ctrl = node.input_opt(0)?;
        if ctrl == NO_NODE || self.headers.contains(&ctrl) {
            return None;
        }
        let cn = graph.node_opt(ctrl)?;
        if !matches!(cn.op, Op::Merge | Op::Region) {
            return None;
        }
        let preds: Vec<NodeId> = node.inputs.as_slice().get(1..)?.to_vec();
        if preds.is_empty() || preds.len() != cn.inputs.len() {
            return None;
        }
        let pc = node.bytecode_pc;
        self.memo.insert(phi, None);
        let mut values: Vec<NodeId> = Vec::with_capacity(preds.len());
        for p in preds {
            values.push(self.value_at(graph, p)?);
        }
        let answer = if values.iter().all(|&v| v == values[0]) {
            values[0]
        } else {
            // A placeholder id no live node has: past the graph and past
            // every placeholder planned so far.
            let placeholder = (graph.nodes.len() + self.plan.len()) as NodeId;
            self.plan.push(PhiStep {
                placeholder,
                ctrl,
                pc,
                inputs: values,
            });
            placeholder
        };
        self.memo.insert(phi, Some(answer));
        Some(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CmpOp;

    /// `x = p.f; if (c) {} else {}; y = p.f` with the join's memory φ, and,
    /// with `store_between`, `p.g = 1` (another field) before the join or,
    /// with `store_same`, `p.f = v` on the true arm.
    struct Diamond {
        g: Graph,
        first: NodeId,
        second: NodeId,
        stored: NodeId,
    }

    fn diamond(store_other: bool, store_same: bool, volatile_second: bool) -> Diamond {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let c = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let v = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let first = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![entry, mem0, p, f0],
            Some(1),
        );
        g.field_decl_class.insert(first, 40);
        let mut mem = first;
        if store_other {
            let s = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![entry, mem, p, f1, one],
                Some(2),
            );
            g.field_decl_class.insert(s, 40);
            mem = s;
        }
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![c, zero], None);
        let iff = g.add(Op::If, IrType::Control, vec![entry, cmp], Some(3));
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let mut tmem = mem;
        if store_same {
            let s = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![t, mem, p, f0, v],
                Some(4),
            );
            g.field_decl_class.insert(s, 40);
            tmem = s;
        }
        let join = g.add(Op::Merge, IrType::Control, vec![t, f], Some(6));
        let jmem = g.add(Op::Phi, IrType::Memory, vec![join, tmem, mem], None);
        let second = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![join, jmem, p, f0],
            Some(7),
        );
        g.field_decl_class.insert(second, 40);
        if volatile_second {
            g.nodes[second as usize].volatile_access = true;
        }
        let sum = g.add(Op::Add, IrType::Int, vec![first, second], None);
        g.exit = g.add(Op::Return, IrType::Int, vec![join, sum], Some(8));
        Diamond {
            g,
            first,
            second,
            stored: v,
        }
    }

    fn run(g: &mut Graph) -> JoinForwardCensus {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_JOIN_LOAD_FORWARD", None)],
            || forward_join_loads(g),
        )
    }

    #[test]
    fn nio21w16_a_read_after_a_join_takes_the_read_before_it() {
        let mut d = diamond(false, false, false);
        let c = run(&mut d.g);
        assert_eq!(c.removed, 1);
        assert_eq!(c.phis, 0, "both paths answer the same read");
        assert_eq!(d.g.nodes[d.second as usize].op, Op::Dead);
        assert!(d
            .g
            .nodes
            .iter()
            .any(|n| n.op == Op::Add && n.inputs.as_slice() == [d.first, d.first]));
    }

    #[test]
    fn nio21w16_a_store_to_another_field_is_stepped_over() {
        let mut d = diamond(true, false, false);
        assert_eq!(run(&mut d.g).removed, 1);
        assert_eq!(d.g.nodes[d.second as usize].op, Op::Dead);
    }

    #[test]
    fn nio21w16_a_store_on_one_arm_makes_a_value_phi() {
        let mut d = diamond(false, true, false);
        let c = run(&mut d.g);
        assert_eq!((c.removed, c.phis, c.from_stores), (1, 1, 1));
        let phi =
            d.g.nodes
                .iter()
                .find(|n| n.op == Op::Phi && n.ty == IrType::Int)
                .expect("a value phi at the join");
        assert_eq!(&phi.inputs.as_slice()[1..], &[d.stored, d.first]);
    }

    #[test]
    fn nio21w16_a_volatile_read_stays_and_the_switch_turns_it_off() {
        let mut d = diamond(false, false, true);
        assert_eq!(run(&mut d.g).removed, 0);
        let mut d = diamond(false, false, false);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_JOIN_LOAD_FORWARD", Some("0"))],
            join_load_forward_enabled,
        );
        assert!(!off);
        // The pass itself does not read the switch; its caller does.
        assert_eq!(forward_join_loads(&mut d.g).removed, 1);
    }

    #[test]
    fn nio21w16_a_narrow_store_is_not_forwarded() {
        let mut d = diamond(false, true, false);
        for n in d.g.nodes.iter_mut() {
            if let Op::Load(k) | Op::Store(k) = &mut n.op {
                *k = MemKind::Byte;
            }
        }
        assert_eq!(run(&mut d.g).removed, 0);
    }

    /// A loop header's memory φ answers nothing: no value crosses a back edge.
    #[test]
    fn nio21w16_a_loop_header_stops_the_walk() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let pre = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![entry, mem0, p, f0],
            Some(1),
        );
        g.field_decl_class.insert(pre, 40);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
        let i = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let mem = g.add(Op::Phi, IrType::Memory, vec![region, pre], None);
        let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i, n], None);
        let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        let ld = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![stay, mem, p, f0],
            Some(10),
        );
        g.field_decl_class.insert(ld, 40);
        let i1 = g.add(Op::Add, IrType::Int, vec![i, ld], None);
        let i2 = g.add(Op::Add, IrType::Int, vec![i1, one], None);
        g.nodes[region as usize].inputs.push(stay);
        g.nodes[i as usize].inputs.push(i2);
        g.nodes[mem as usize].inputs.push(ld);
        g.exit = g.add(Op::Return, IrType::Int, vec![exit, i], Some(20));
        assert_eq!(run(&mut g).removed, 0);
        assert_ne!(g.nodes[ld as usize].op, Op::Dead);
    }
}
