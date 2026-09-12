// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Escape analysis and scalar replacement for the JIT compiler.
//!
//! Determines whether heap-allocated objects escape the current method,
//! enabling three key optimizations:
//!
//! - **Scalar replacement**: objects that never escape (`NoEscape`) are
//!   decomposed into individual local values, eliminating the allocation
//!   entirely.
//! - **Stack allocation**: objects that only escape to callees (`ArgEscape`)
//!   can be allocated on the stack instead of the GC heap.
//! - **Lock elision**: synchronized blocks on non-escaping objects are
//!   removed since no other thread can observe them.
//!
//! The analysis builds a *connection graph* that tracks how object
//! references flow through the IR, then propagates escape states to a
//! fixed point.
//!
//! # Which escape analysis is this?
//!
//! There are **two** in the JIT and they share a function name. Do not confuse
//! them:
//!
//! * **This module** operates on the sea-of-nodes IR ([`Graph`], [`Op`]) and is
//!   reached only from the tier-2 IR pipeline in `jit/src/lib.rs`
//!   (`escape_analysis_from_ir` builds the graph; `analyze_escapes(&ea_graph)`
//!   runs it). Its results drive IR-level scalar replacement and lock elision.
//! * **The single-pass x64 backend has its own**, private, *bytecode-level*
//!   `analyze_escapes(code, code_len, &invokespecial_shapes)` in
//!   `jit/src/x64.rs`. That is the one that runs for the overwhelming majority
//!   of compiled methods, and it is where the known varargs-constructor
//!   receiver-null defect (BUG-05) lives — the shape map keyed by
//!   `InvokeSpecialShape { arg_slots, is_trivial_void_init }` is that
//!   analysis's, not this one's. Fixing this module does not fix that one.
//!
//! # What is actually consumed
//!
//! [`EscapeAnalysisResult`] has several outputs; production code in
//! `jit/src/lib.rs` (`apply_ea_to_ir_pinned`) reads only two:
//!
//! | Field | Consumer | Status |
//! |---|---|---|
//! | `scalar_replaceable` | IR scalar replacement (`plan_scalar_replacement`) | live |
//! | `lock_elisions` | IR lock elision, applied all-or-nothing per object | live |
//! | `elide_locks` | tests + diagnostics | the flat view of `lock_elisions`; no production reader |
//! | `lock_coarsening` | — | offered, no consumer yet |
//! | `lock_refusals` | tests + diagnostics | informational |
//! | `stack_allocatable` | — | **computed, never read** |
//! | `escape_states` / `stats` | tests + diagnostics | informational |
//!
//! # Lock elimination and coarsening
//!
//! Two transforms, both fenced on `EscapeState::NoEscape`, both with their JMM
//! argument written out at the "Phase 4" section comment below and in
//! `docs/jit/lock-elimination.md`:
//!
//! * **elision** removes every monitor operation on a confined object. It is
//!   offered per object ([`LockElisionPlan`]) because removing a strict subset
//!   of a balanced monitor sequence is wrong code. The production consumer
//!   (`apply_ea_to_ir_pinned` in `jit/src/lib.rs`) reads those per-object plans
//!   and refuses a whole object when any one of its monitors cannot be removed;
//!   the flat [`EscapeAnalysisResult::elide_locks`] list is kept for tests and
//!   diagnostics. A method that had monitors before elision is marked unable to
//!   deopt-resume precisely (`had_monitors`, see
//!   `docs/jit/lock-elimination.md` §8).
//! * **coarsening** merges two adjacent lock regions on a confined object by
//!   deleting the inner `monitorexit`/`monitorenter` pair
//!   ([`LockCoarseningPlan`]). It depends on no elision having landed, so it is
//!   safe under that partial application.
//!
//! Both refuse an object whose monitor is `wait`ed on, both refuse an object
//! whose monitors cannot be attributed, and coarsening additionally refuses any
//! gap that can throw, can deopt, or can observe the unlocked state. Every
//! refusal is recorded in [`EscapeAnalysisResult::lock_refusals`].
//!
//! `stack_allocatable` collects every `ArgEscape` allocation, but no caller
//! looks at it: **stack allocation is not implemented**. The field is not a
//! disabled feature behind a flag — there is no code to enable. Treat a
//! non-empty `stack_allocatable` as a measurement, not an optimisation.
//!
//! # Soundness direction
//!
//! The lattice joins upward (`NoEscape < PartialEscape < ArgEscape <
//! GlobalEscape`), so every partial result *under*-estimates escape, and both
//! live consumers act on `== NoEscape`. Anything that can terminate the fixed
//! point early therefore has to fail closed — see [`escalate_all_to_global`].
//!
//! # The three things this module proves before an object may be replaced
//!
//! Scalar replacement deletes a *heap object*. Three separate facts have to
//! hold, and each is checked by its own machinery here:
//!
//! 1. **Reachability** — nobody outside this frame can reach the object.
//!    That is [`EscapeState`], computed by [`propagate_escape_states`].
//! 2. **Value** — every surviving read of a field must be answerable with the
//!    value the object actually held *at that point*. That is the positional
//!    store record ([`ScalarReplacementInfo::field_stores`]) and the per-load
//!    answer ([`LoadResolution`]). A last-write-wins field map is **not**
//!    enough: `Foo o = new Foo(); int a = o.x; o.x = 42;` must fold `a` to the
//!    zero default, never to `42`. See [`ScalarReplacementInfo::load_values`].
//! 3. **Identity** — nothing may observe the object's *address*. `==` on
//!    references, an identity hash, and `monitorenter`/`monitorexit` all do.
//!    An object with a live identity observation is refused outright; see
//!    [`Op::RefCompare`], [`Op::IdentityHash`] and
//!    [`EscapeAnalysisResult::identity_observations`].
//!
//! Facts 2 and 3 are *independent* of fact 1. An object can be perfectly
//! `NoEscape` and still be unreplaceable because a load cannot be answered or
//! its identity is observed. Conflating them is how the last miscompilation on
//! this branch happened.
//!
//! # Why `dominance_proved` exists
//!
//! Answering "which store does this load see?" is a dominance question, and the
//! EA graph cannot ask it: the bridge in `jit/src/lib.rs`
//! (`escape_analysis_from_ir`) rewrites the production `Op::Load` / `Op::Store`
//! into the compact `[holder, value]` layout, **dropping the control and memory
//! edges**. Without them there is no CFG here to build a dominator tree from.
//!
//! So the analysis uses program order — ascending [`NodeId`], which is creation
//! order — as a stand-in, and gates that stand-in per candidate on either
//! [`program_order_proves_dominance`] (a graph with no branch, no join and no
//! multi-input φ) or `one_block_proves_dominance` (the allocation and every
//! access it folds share one basic block, as recorded in `Graph::blocks`, with
//! no φ alias). Anything else with a store to a loaded field resolves to
//! [`LoadResolution::Unknown`] and the object is refused. Recovering more
//! precision means giving the EA graph real control edges — see
//! `docs/jit/escape-analysis.md`.

use std::collections::{HashMap, HashSet, VecDeque};

// ── Local type definitions (mirrors ir.rs for standalone compilation) ──

pub type NodeId = usize;

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Start,
    Return,
    If,
    Merge,
    Phi,
    Const(i64),
    Param(u16),
    Add,
    Sub,
    Mul,
    /// Field/array **load**.  The `usize` payload is a *field index* relative
    /// to the holder allocation (NOT a `MemKind` / type tag — see the layout
    /// contract below).  Canonical EA input layout: `[holder, (index)?]`
    /// (`STORE_LOAD_HOLDER_INPUT == 0`).
    Load(usize),
    /// Field/array **store**.  The `usize` payload is a *field index* relative
    /// to the holder allocation.  Canonical EA input layout:
    /// `[holder, value, (index)?]` (holder at 0, value at 1).
    Store(usize),
    New {
        class_id: u32,
        num_fields: usize,
    },
    NewArray {
        element_type: u8,
        /// The array's length, when the producer proved it a compile-time
        /// constant; `None` otherwise.
        ///
        /// This is what makes an array a scalar-replacement candidate at all:
        /// an array of constant length N maps onto
        /// [`ScalarReplacementInfo::field_values`] exactly the way an N-field
        /// object does, and a constant index is exactly a field index. Without
        /// a length there is no slot count, so there is nothing to map onto.
        ///
        /// `None` is the fail-closed default: a hand-built or future-produced
        /// node that does not fill it is simply never a candidate.
        length: Option<usize>,
    },
    Call,
    MonitorEnter,
    MonitorExit,
    /// `Object.wait` / `wait(long)`. Input `[obj]`.
    ///
    /// Semantically a *monitor* operation, not a call: it requires the monitor
    /// to be held (else `IllegalMonitorStateException`), releases it to the
    /// recorded depth, blocks, and reacquires it at that same depth. Every one
    /// of those three facts is destroyed by eliding the monitor, so an object
    /// with a live `MonitorWait` is refused both lock transforms outright — see
    /// `find_lock_elision_plans`.
    ///
    /// NOTE: no producer yet. `ir_op_to_ea_op` maps the underlying
    /// `invokevirtual` to [`Op::Call`], which arg-escapes the receiver and
    /// therefore already refuses the elision. This variant makes that refusal
    /// *intentional and reportable* rather than a side effect of the call rule,
    /// and it keeps the refusal alive if `Op::Call` ever gains an
    /// "inlined intrinsic, does not escape" arm.
    MonitorWait,
    /// `Object.notify` / `notifyAll`. Input `[obj]`.
    ///
    /// Same status and same treatment as [`Op::MonitorWait`]: it requires the
    /// monitor to be held, and the thread it wakes is a thread that must have
    /// been able to reach the object — so a `notify` on an object the analysis
    /// believes is confined is a contradiction the analysis fails closed on
    /// rather than resolves.
    MonitorNotify,
    /// A safepoint / deoptimization point. Inputs: the references the frame
    /// state names (may be empty).
    ///
    /// Present so lock **coarsening** can ask "does a deopt land between these
    /// two regions?". A deopt in a coarsened gap would reconstruct an
    /// interpreter frame whose monitor set says the object is *unlocked* while
    /// the compiled frame holds it — see `find_lock_coarsening_plans`.
    ///
    /// NOTE: no producer yet. `ir::Graph` carries safepoints in a side table
    /// (`ir_graph.safepoints`), not as nodes, and `escape_analysis_from_ir`
    /// does not bridge them. Until it does, [`LockRefusal::SafepointInGap`] is
    /// unreachable from production IR and coarsening relies on the gap
    /// allowlist alone — which admits no node that can be a safepoint.
    Safepoint,
    /// `athrow`. Inputs: the thrown reference.
    ///
    /// Throwing publishes the reference out of the frame (same rule as
    /// [`Op::Return`]) and unwinds every monitor the frame holds.
    ///
    /// NOTE: no producer yet — `ir_op_to_ea_op` has no `athrow` arm.
    Throw,
    ArrayLength,
    /// Reference **identity comparison** (`if_acmpeq` / `if_acmpne`, and the
    /// `Objects.isNull`-style folds that lower to one).  Inputs: the compared
    /// references, in any order.
    ///
    /// This is an *identity observation*, not an escape: the compared object
    /// stays unreachable from outside the frame, but its address becomes
    /// observable, so it may not be scalar-replaced while the comparison is
    /// live.  See [`find_identity_observations`].
    ///
    /// NOTE: no producer emits this yet — `ir_op_to_ea_op` in `jit/src/lib.rs`
    /// maps `ir::Op::Cmp(_)` to [`Op::Other`].  Until it is taught to emit
    /// `RefCompare` for a `Cmp` on two `IrType::Ref` operands, an `acmp` on a
    /// scalar-replacement candidate is invisible here.  It is *currently*
    /// harmless only because `Op::Other` hits `find_scalar_replacements`'
    /// catch-all and refuses the object anyway; this variant makes the refusal
    /// intentional and reportable rather than incidental.
    RefCompare,
    /// Identity-hash observation (`System.identityHashCode`, or an
    /// `Object.hashCode` that is not overridden).  Input `[obj]`.
    ///
    /// Same status as [`Op::RefCompare`]: an identity observation that blocks
    /// scalar replacement, with no producer in the bridge yet.
    IdentityHash,
    Dead,
    Other,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub op: Op,
    pub inputs: Vec<NodeId>,
    pub uses: Vec<NodeId>,
}

#[derive(Debug)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub entry: NodeId,
    pub exit: NodeId,
    /// Nodes the *producer* knows execute on a cold path — an exceptional or
    /// profiled-never-taken branch, a slow-path helper call, a bail.
    ///
    /// This is the only cold-path input the analysis has, and it is **empty by
    /// default**: with no producer filling it, no allocation is ever classified
    /// [`EscapeState::PartialEscape`] and behaviour is exactly what it was
    /// before partial escape existed. That is deliberate — a wrong guess about
    /// coldness would report an object as "escapes only rarely" when it escapes
    /// on every call, so the fail-closed default is "nothing is cold".
    ///
    /// A partial-escape *classification* is informational: it never enables an
    /// optimisation on its own (see [`EscapeState::PartialEscape`]). It names
    /// the objects a future partial-escape applier could sink past their cold
    /// escape point, together with the materialization sites it would need
    /// ([`PartialEscapeInfo::escape_sites`]).
    pub cold_nodes: HashSet<NodeId>,
    /// For each memory-accessing node, the **basic block** it executes in,
    /// named by the IR control node it is pinned to (mapped into EA ids).
    ///
    /// # Why the analysis needs this
    ///
    /// The bridge drops the control and memory edges (see the module header),
    /// so [`program_order_proves_dominance`] can only answer the *whole graph*
    /// question — "is there any branch anywhere?" — and a hot loop always
    /// answers no. That refused every allocation in every loop body, including
    /// ones whose allocation, stores and loads sit next to each other in one
    /// straight-line block.
    ///
    /// This carries back the one fact that makes the local question decidable,
    /// and nothing else: which block each access is in. Ascending [`NodeId`] is
    /// execution order *within* a block, because a block is straight-line by
    /// definition.
    ///
    /// **Empty by default.** A graph with no entries behaves exactly as it did
    /// before this existed: only the graph-wide stand-in is available, and an
    /// allocation in a branchy graph is refused. Fail-closed in the direction
    /// that matters — a missing entry costs an optimisation, never correctness.
    ///
    /// Only nodes whose IR op pins control at input 0 *and* whose control input
    /// is a real control node get an entry. In particular a `Guard` is
    /// deliberately NOT a block boundary: it produces no control token
    /// ([`ir::Op::is_control`] excludes it), so a bounds or null check leaves
    /// its block intact and the accesses either side of it still compare equal.
    pub blocks: HashMap<NodeId, NodeId>,
}

impl Graph {
    /// Create an empty graph with a Start and a Return node.
    pub fn new() -> Self {
        let start = Node {
            op: Op::Start,
            inputs: vec![],
            uses: vec![],
        };
        let ret = Node {
            op: Op::Return,
            inputs: vec![],
            uses: vec![],
        };
        Graph {
            nodes: vec![start, ret],
            entry: 0,
            exit: 1,
            cold_nodes: HashSet::new(),
            blocks: HashMap::new(),
        }
    }

    /// Record that `node` executes only on a cold path.
    ///
    /// Additive and monotone: marking more nodes cold can only ever move an
    /// allocation from `ArgEscape`/`GlobalEscape` to the *reported*
    /// `PartialEscape`, which no consumer acts on.
    pub fn mark_cold(&mut self, node: NodeId) {
        self.cold_nodes.insert(node);
    }

    pub fn add_node(&mut self, op: Op, inputs: Vec<NodeId>) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            op,
            inputs: inputs.clone(),
            uses: vec![],
        });
        for &inp in &inputs {
            if inp < self.nodes.len() {
                self.nodes[inp].uses.push(id);
            }
        }
        id
    }
}

// ── Load/Store node layout contract ─────────────────────────────────────
//
// PINNED SEMANTICS (do not change without updating `ir_lower`/the builder):
//
// The escape-analysis IR is a *simplified* dataflow graph that is distinct
// from the JIT's full `ir::Graph`.  In the EA graph, memory-access nodes use
// a compact, control-/memory-edge-free layout so that the connection-graph
// builder can read operands positionally:
//
//   Op::Store(field_idx)  inputs = [holder, value]   (extra inputs ignored)
//   Op::Load(field_idx)   inputs = [holder]          (extra inputs ignored)
//
// where:
//   * `holder` is the reference node whose field is being accessed, and
//   * `value`  (Store only) is the reference/primitive node being written.
//   * `field_idx` is the **field index** within the holder object, used to
//     key field edges and to drive scalar replacement.
//
// IMPORTANT (the historical landmine this contract closes): the full
// `ir::Op::{Load,Store}` variants carry a `MemKind` *type tag*, not a field
// index, and their inputs are `[ctrl, mem, base, index/offset, (value)]`.
// A naive lowering that forwarded those verbatim would (a) place the holder
// at input index 2 (not 0) and the value at index 4 (not 1), and (b) pass a
// `MemKind` discriminant where a field index is expected.  Either mistake
// silently corrupts the escape lattice (e.g. a store's value would be read
// from the *memory* edge, and field arrays would be indexed by a type tag).
//
// To keep the lattice sound regardless of how the lowering evolves, the
// helpers below are the *single* place that interprets Load/Store operands
// and field indices.  `store_holder`/`store_value`/`load_holder` return
// `None` when the layout invariant is violated, and every caller treats a
// `None` (or an out-of-range field index) **conservatively** — i.e. it may
// only *add* escape, never remove it.

/// Input position of the holder reference for `Load`/`Store` nodes.
const MEM_HOLDER_INPUT: usize = 0;
/// Input position of the stored value for `Store` nodes.
const STORE_VALUE_INPUT: usize = 1;

/// Holder reference of a `Store` node, or `None` if the layout is malformed.
fn store_holder(node: &Node) -> Option<NodeId> {
    debug_assert!(matches!(node.op, Op::Store(_)));
    node.inputs.get(MEM_HOLDER_INPUT).copied()
}

/// Stored value of a `Store` node, or `None` if the layout is malformed.
fn store_value(node: &Node) -> Option<NodeId> {
    debug_assert!(matches!(node.op, Op::Store(_)));
    node.inputs.get(STORE_VALUE_INPUT).copied()
}

/// Holder reference of a `Load` node, or `None` if the layout is malformed.
fn load_holder(node: &Node) -> Option<NodeId> {
    debug_assert!(matches!(node.op, Op::Load(_)));
    node.inputs.get(MEM_HOLDER_INPUT).copied()
}

/// True if `field` is a valid field index for *every* allocation a holder may
/// resolve to.  An empty/unknown holder set, or any allocation whose
/// `num_fields` does not cover `field`, returns `false` so the caller can
/// fall back to the conservative (escape-everything) path.  This prevents a
/// `MemKind`-as-field-index mix-up (or a genuinely out-of-range index) from
/// silently indexing the wrong field.
fn field_in_range(graph: &Graph, holder_allocs: &HashSet<NodeId>, field: usize) -> bool {
    if holder_allocs.is_empty() {
        return false;
    }
    holder_allocs
        .iter()
        .all(|&a| match graph.nodes.get(a).map(|n| &n.op) {
            Some(Op::New { num_fields, .. }) => field < *num_fields,
            // An array's "fields" are its elements, so a constant length is a
            // slot count and a constant index is a field index. An array whose
            // length the producer could not prove constant still answers
            // `false`, which keeps its accesses on the conservative path.
            Some(Op::NewArray { length, .. }) => length.is_some_and(|n| field < n),
            _ => false,
        })
}

// ── Program order as a dominance stand-in ───────────────────────────────

/// Whether ascending [`NodeId`] is a sound stand-in for dominance *for one
/// allocation*, on the strength of every access to it being in one basic block.
///
/// [`program_order_proves_dominance`] asks the whole-graph question and a hot
/// loop always fails it: the loop's own back-edge is a `Merge`. But the object
/// this analysis wants to delete is usually allocated, filled and read inside a
/// single straight-line block of that loop, and for such an object the ordering
/// question is decidable without any dominator tree.
///
/// # The argument
///
/// Let *B* be the block. Require the allocation and **every** field access this
/// candidate folds — every store in `stores`, every load in `loads` — to be in
/// *B*, and require the reference to have no φ alias (`transparent` is just the
/// allocation itself, so no copy of it leaves *B*).
///
/// 1. A block is straight-line, so within *B* ascending `NodeId` — which is
///    creation order, which is program order — **is** execution order.
/// 2. The allocation is itself in *B*, so each execution of *B* produces a
///    *fresh* object. A store executed in an earlier execution of *B* (an
///    earlier loop iteration) wrote an earlier object, not this one. This is
///    the clause that makes the rule safe in a loop, and it is exactly what
///    fails when the allocation is hoisted out of the loop: then one object is
///    reused across iterations and a load can see the previous iteration's
///    store.
/// 3. The use walk in [`find_scalar_replacements`] is exhaustive — it visits
///    every use of the allocation and refuses the object outright at anything
///    it cannot classify — so `stores` is *every* store to this object's
///    fields. With (1) and (2), the latest store preceding a load in *B* is the
///    value that load reads.
///
/// Returns `false` whenever any access has no recorded block (see
/// [`Graph::blocks`]) or the blocks are not all equal — a missing fact is never
/// read as agreement.
fn one_block_proves_dominance(
    graph: &Graph,
    alloc: NodeId,
    transparent: &HashSet<NodeId>,
    stores: &[Vec<FieldStore>],
    loads: &[NodeId],
) -> bool {
    // A φ copy carries the reference to a merge, which is by construction not
    // this block. Refuse rather than reason about it.
    if transparent.len() != 1 {
        return false;
    }
    let block = match graph.blocks.get(&alloc) {
        Some(&b) => b,
        None => return false,
    };
    let same = |n: &NodeId| graph.blocks.get(n) == Some(&block);
    stores.iter().flatten().all(|fs| same(&fs.store)) && loads.iter().all(same)
}

/// Whether ascending [`NodeId`] is a sound stand-in for *dominance* in this
/// graph — i.e. whether "store id < load id" really means "the store executed
/// before the load, on every path that reaches the load".
///
/// # Why this is needed at all
///
/// The value-forwarding question ("which store does this load see?") is a
/// dominance question. The EA graph cannot answer it directly: the bridge
/// (`escape_analysis_from_ir` in `jit/src/lib.rs`) rewrites `Op::Load` /
/// `Op::Store` into the compact `[holder, value]` / `[holder]` layout and drops
/// input 0 (control) and input 1 (memory). There is therefore no CFG in this
/// graph to dominate over.
///
/// Node ids are assigned by `Graph::add_node` in creation order, which the
/// bridge derives from `ir::Graph` node order, which the builder derives from
/// bytecode order. In a graph with **no control flow at all** that ordering is
/// exactly execution order, so it *is* dominance. Add one branch and it is not:
///
/// ```text
///   Foo o = new Foo();
///   if (c) o.x = 1; else o.x = 2;   // ids 10 and 12
///   int a = o.x;                    // id 15 — sees 1 or 2, unknowably
/// ```
///
/// Both stores precede the load in id order and neither dominates it. A
/// last-write-wins field map answers `2` and miscompiles the `c` path.
///
/// # The test
///
/// Refuse the moment any of these appears on a live node:
///
/// * [`Op::If`] — the only way control can diverge, and therefore also the only
///   way a loop can be built (a loop with no exit branch does not terminate).
///   Catching `If` is what closes the back-edge case, where a *higher*-id store
///   in the loop body runs before a *lower*-id load on the second iteration.
/// * [`Op::Merge`] — a forward join. Implied by `If` in a well-formed graph,
///   checked separately because the bridge maps `ir::Op::Region` (the loop
///   header) to [`Op::Other`], so `Merge` is the only join op that survives
///   translation and a graph could in principle carry one without an `If`.
/// * A [`Op::Phi`] with more than one input. A φ merging two or more values can
///   only exist at a join. A *single*-input φ is a degenerate copy (the
///   transparent-alias shape `find_scalar_replacements` accepts) and does not
///   imply divergence.
///
/// # What is deliberately *not* on the list
///
/// **Exception edges.** A `catch`/`finally` handler body is never built into
/// the IR at all: `ir::IrBuilder::build` skips handler bytecode outright
/// (the "STUB-S8" skip in `ir::IrBuilder::build`), because a JIT frame never
/// takes an exception edge —
/// an exception makes the compiled body return the `i64::MIN` sentinel and the
/// runtime re-runs or resumes the method in the interpreter. So a throw inside
/// a compiled body *leaves the frame*; it cannot branch to a lower-id load.
/// If that ever changes, this predicate must gain a `may_throw` term.
///
/// # Failure mode
///
/// `false` is not a bug, it is the fail-closed answer: every field that has at
/// least one store then resolves to [`LoadResolution::Unknown`] and the object
/// is refused. Fields with *no* store still resolve (to the zero default,
/// which no control flow can change), and an object with stores but no loads is
/// still replaceable — nothing has to be answered.
pub fn program_order_proves_dominance(graph: &Graph) -> bool {
    !graph.nodes.iter().any(|n| match &n.op {
        Op::If | Op::Merge => true,
        Op::Phi => n.inputs.len() > 1,
        _ => false,
    })
}

// ── Escape state ────────────────────────────────────────────────────────

/// How far an allocated object escapes from its allocation site.
///
/// The order of the variants **is** the lattice order, and [`join`] is `max`:
///
/// ```text
///   NoEscape  <  PartialEscape  <  ArgEscape  <  GlobalEscape
///   ▲                                                       ▲
///   bottom (optimisable)                       top (fail-closed answer)
/// ```
///
/// # `PartialEscape` is not produced by the fixed point
///
/// The other three states are computed by [`propagate_escape_states`], which
/// only ever joins *upward*, so every intermediate value is an under-estimate
/// and the final value is a sound over-approximation of reality.
///
/// `PartialEscape` is different: it is a **path-sensitive refinement applied
/// after the fixed point**, in [`analyze_escapes`], and it *lowers* an
/// allocation's reported state from `ArgEscape`/`GlobalEscape` when every site
/// at which the object escapes is in [`Graph::cold_nodes`]. Lowering a state is
/// unsound in general, which is why it is fenced three ways:
///
/// * it is never written into the [`ConnectionGraph`], so it can never feed
///   back into the join and weaken another node's state;
/// * it only ever appears in [`EscapeAnalysisResult::escape_states`] and
///   [`EscapeAnalysisStats`], both of which are informational;
/// * every consumer that *acts* (scalar replacement, lock elision) tests
///   `== NoEscape` against the connection graph, so a `PartialEscape` object is
///   treated exactly like the `ArgEscape`/`GlobalEscape` object it really is
///   until somebody writes a partial-escape applier that sinks the allocation
///   past its cold escape point.
///
/// It sits *below* `ArgEscape` in the order because that is the direction a
/// future applier would move it in, and because a consumer that (wrongly) tests
/// `>= ArgEscape` for "escapes" is then the one that has to be fixed — a
/// consumer that tests `!= NoEscape`, which is the documented predicate
/// ([`EscapeState::may_escape`]), stays correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EscapeState {
    /// Object does not escape the current method -- can be scalar-replaced.
    NoEscape,
    /// Object escapes on *some* path only, and every such path is cold.
    ///
    /// Reported, never propagated — see the type-level documentation. Today it
    /// enables nothing; it identifies the objects a partial-escape applier
    /// would target, and it is *not* stack-allocatable (the hot path would pay
    /// for a frame slot the cold path then has to copy to the heap anyway).
    PartialEscape,
    /// Object escapes to a callee but not globally -- can be stack-allocated.
    ArgEscape,
    /// Object escapes globally -- must be heap-allocated.
    GlobalEscape,
}

impl EscapeState {
    /// Join two escape states (lattice meet = max).
    fn join(self, other: EscapeState) -> EscapeState {
        std::cmp::max(self, other)
    }

    /// True when the object may be observed outside the allocating frame on at
    /// least one path.
    ///
    /// **This, not `>= ArgEscape`, is the predicate to test for "escapes".**
    /// `PartialEscape` escapes — rarely, but it escapes — and it orders below
    /// `ArgEscape`.
    pub fn may_escape(self) -> bool {
        self != EscapeState::NoEscape
    }

    /// True when the object is provably confined to this frame on **every**
    /// path, which is the precondition for scalar replacement and lock elision.
    pub fn is_confined(self) -> bool {
        self == EscapeState::NoEscape
    }
}

// ── Connection graph ────────────────────────────────────────────────────

/// Connection graph for escape analysis.
/// Tracks which nodes point to which allocations and how objects flow.
pub struct ConnectionGraph {
    /// Escape state per node.
    pub escape_states: HashMap<NodeId, EscapeState>,
    /// Points-to set: node -> set of allocation sites it may point to.
    pub points_to: HashMap<NodeId, HashSet<NodeId>>,
    /// Field edges: (object_node, field_index) -> set of nodes stored there.
    pub field_edges: HashMap<(NodeId, usize), HashSet<NodeId>>,
    /// Deferred edges: node -> set of nodes it copies from.
    pub deferred_edges: HashMap<NodeId, HashSet<NodeId>>,
    /// Field loads recorded during graph construction:
    /// `(load_node, holder_node, field_index)`.  Resolved field-sensitively
    /// during propagation — a load points to whatever was stored into that
    /// field of any allocation the holder may resolve to.
    pub field_loads: Vec<(NodeId, NodeId, usize)>,
}

impl ConnectionGraph {
    fn new() -> Self {
        ConnectionGraph {
            escape_states: HashMap::new(),
            points_to: HashMap::new(),
            field_edges: HashMap::new(),
            deferred_edges: HashMap::new(),
            field_loads: Vec::new(),
        }
    }

    fn set_escape(&mut self, node: NodeId, state: EscapeState) {
        let entry = self
            .escape_states
            .entry(node)
            .or_insert(EscapeState::NoEscape);
        *entry = (*entry).join(state);
    }

    fn get_escape(&self, node: NodeId) -> EscapeState {
        self.escape_states
            .get(&node)
            .copied()
            .unwrap_or(EscapeState::NoEscape)
    }

    fn add_points_to(&mut self, from: NodeId, to: NodeId) {
        self.points_to.entry(from).or_default().insert(to);
    }

    fn add_deferred(&mut self, from: NodeId, to: NodeId) {
        self.deferred_edges.entry(from).or_default().insert(to);
    }

    fn add_field_edge(&mut self, obj: NodeId, field: usize, value: NodeId) {
        self.field_edges
            .entry((obj, field))
            .or_default()
            .insert(value);
    }

    /// Resolve all deferred edges to compute the full points-to set for a node.
    fn resolve_points_to(&self, node: NodeId) -> HashSet<NodeId> {
        let mut result = HashSet::new();
        let mut worklist = VecDeque::new();
        let mut visited = HashSet::new();
        worklist.push_back(node);
        visited.insert(node);

        while let Some(n) = worklist.pop_front() {
            if let Some(pts) = self.points_to.get(&n) {
                result.extend(pts);
            }
            if let Some(deferred) = self.deferred_edges.get(&n) {
                for &d in deferred {
                    if visited.insert(d) {
                        worklist.push_back(d);
                    }
                }
            }
        }
        result
    }
}

// ── Scalar replacement info ─────────────────────────────────────────────

/// One store into a scalar-replaced object's field, kept **positionally**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldStore {
    /// The `Op::Store` node. Its id is its position in program order.
    pub store: NodeId,
    /// The value node written by this store.
    pub value: NodeId,
}

/// What a replaced field load reads.
///
/// The whole point of this type is that it distinguishes *"reads the zero
/// default"* from *"cannot be answered"*. Collapsing those two into one
/// `Option<NodeId>` is how a load-before-store gets a value from its own
/// future.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadResolution {
    /// The load reads the value produced by this node.
    Value(NodeId),
    /// No store to this field dominates the load, so it reads the freshly
    /// allocated object's zero default.
    ///
    /// Sound because the object is genuinely zero-initialised at allocation:
    /// `Op::New` names a class whose instance fields start at `0`/`null`, and
    /// the front end only admits allocations whose constructor sets no non-zero
    /// field before the analysis sees it.
    ZeroDefault,
    /// Not provable. The caller **must** keep the load (and, since the object
    /// it reads must then still exist, must not elide the allocation either).
    Unknown,
}

/// Information for replacing an allocation with scalar values.
pub struct ScalarReplacementInfo {
    /// The allocation node being replaced.
    pub alloc_node: NodeId,
    /// Class ID of the allocated object.
    pub class_id: u32,
    /// Number of fields.
    pub num_fields: usize,
    /// For each field: the value of the **last** store to it in program order
    /// (or `None` if the field is never stored).
    ///
    /// # This field alone is not a correct forwarding source
    ///
    /// It answers "what does the object hold when the method ends?", which is
    /// the right question for a deopt recipe at the *end* of the object's live
    /// range and the wrong question for a load in the middle of it. Forwarding
    /// every replaced load to `field_values[f]` folded
    /// `Foo o = new Foo(); int a = o.x; o.x = 42;` to `a == 42` — the
    /// miscompilation this branch already paid for once.
    ///
    /// It is kept because it is the shape `ir_lower::VirtualObjectInfo` is built
    /// from today (via `jit/src/lib.rs::virtual_object_info_for`). **For load
    /// forwarding use [`Self::load_values`] / [`Self::load_value`].**
    pub field_values: Vec<Option<NodeId>>,
    /// Per field, **every** store to it, ascending by node id = program order.
    ///
    /// This is the positional record that makes per-load resolution possible,
    /// and it is also what a deopt producer needs to pick the store that
    /// dominates a *particular* safepoint rather than bailing on "unproven
    /// dominance" — see `docs/jit/escape-analysis.md`.
    pub field_stores: Vec<Vec<FieldStore>>,
    /// Loads that were replaced with direct field value access.
    pub replaced_loads: Vec<NodeId>,
    /// For each entry of [`Self::replaced_loads`], in the same order, the value
    /// that load actually reads.
    ///
    /// **Invariant:** a candidate that reaches [`EscapeAnalysisResult`] never
    /// contains [`LoadResolution::Unknown`] here —
    /// [`find_scalar_replacements`] refuses the whole object instead. A
    /// consumer may therefore treat `Unknown` as unreachable, but should still
    /// fail closed on it rather than assume.
    pub load_values: Vec<(NodeId, LoadResolution)>,
    /// Stores that were eliminated.
    pub eliminated_stores: Vec<NodeId>,
    /// `Some(atype)` when the replaced allocation is an [`Op::NewArray`], and
    /// the elements rather than fields are what `field_values` describes;
    /// `None` for an ordinary object.
    ///
    /// The IR-side applier reads this to know whether it is looking for
    /// `Op::Load`/`Op::Store` or `Op::ArrayLoad`/`Op::ArrayStore`, and the
    /// deopt-descriptor producer reads it to know it cannot describe the
    /// allocation with a `class_id`.
    pub array_element_type: Option<u8>,
    /// Whether program order was a sound dominance stand-in **for this
    /// candidate** — either [`program_order_proves_dominance`] for the whole
    /// graph, or [`one_block_proves_dominance`] for this object alone.
    /// Recorded per candidate so a consumer can see *why* a field with stores
    /// resolved to `Unknown`.
    pub dominance_proved: bool,
}

impl ScalarReplacementInfo {
    /// The value `load` reads, or [`LoadResolution::Unknown`] if this candidate
    /// does not describe that load at all.
    ///
    /// Fails closed on an unknown node: a caller that asks about a load this
    /// object never owned gets `Unknown`, not a plausible-looking wrong value.
    pub fn load_value(&self, load: NodeId) -> LoadResolution {
        self.load_values
            .iter()
            .find(|&&(l, _)| l == load)
            .map(|&(_, r)| r)
            .unwrap_or(LoadResolution::Unknown)
    }
}

// ── Analysis result ─────────────────────────────────────────────────────

/// An allocation that escapes, but only on cold paths.
#[derive(Debug, Clone)]
pub struct PartialEscapeInfo {
    /// The allocation node.
    pub alloc_node: NodeId,
    /// The state the fixed point computed, before the cold-path refinement.
    /// This is the state every *acting* consumer still sees, because the
    /// refinement never enters the connection graph.
    pub without_refinement: EscapeState,
    /// The cold nodes at which the object becomes visible outside the frame.
    /// A partial-escape applier must materialize the object on the heap before
    /// each of these, with the field values that hold at that point.
    pub escape_sites: Vec<NodeId>,
}

/// Full result of escape analysis on a graph.
/// The largest constant array length scalar replacement will take on.
///
/// Replacing an array trades one allocation for one live value per element, and
/// the register allocator pays for every one of them. Small wrapper arrays --
/// the `short[2]` behind a `Short2`, a `float[3]` behind a vector type -- are
/// the shape this exists for, and they are the shape where the trade is
/// obviously good. A long array turns a single allocation into dozens of
/// simultaneously-live values and spills, which is a pessimisation dressed as
/// an optimisation.
///
/// Eight is chosen to cover the vector/tuple wrappers (2, 3, 4, 8 elements) and
/// stop well short of anything a loop would index.
pub const MAX_SCALAR_ARRAY_LEN: usize = 8;

/// Why [`find_scalar_replacements`] refused one allocation.
///
/// # Why this exists
///
/// `scalar-replaced 0/2` is a count, and a count cannot be acted on. The
/// per-voxel `Short2` page spent three rounds guessing at a `0/N` — twice
/// wrongly — because the analysis reported how many objects it kept and never
/// which fact stopped it. Each variant below is one `continue`/`break` in
/// `find_scalar_replacements`, so the report and the control flow cannot drift.
///
/// Purely informational: nothing acts on a refusal, and producing one is not a
/// decision. It is emitted under `CRATONVM_DBG_SCALAR_NEW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScalarRefusal {
    /// Not `NoEscape` — the reachability fact (fact 1) does not hold.
    Escapes(EscapeState),
    /// A live node observes the object's address (fact 3).
    IdentityObserved,
    /// The object is published into another object's field, so it escapes
    /// through the heap.
    StoredIntoAnotherObject,
    /// A field op names this allocation as its holder with an index that is not
    /// a field of it (or a malformed operand layout).
    FieldIndexOutOfRange,
    /// A φ carrying this reference could not be proved to be a transparent copy
    /// of exactly this allocation.
    PhiNotTransparentCopy,
    /// A monitor operation names the object.
    MonitorOperation,
    /// A use this walk has no rule for — a `Call` argument, a `Return`, a
    /// `Throw`, an `Op::Other`. The catch-all.
    OpaqueUse,
    /// A value written into one of the object's own fields may BE the object,
    /// so a reference to it can be recovered through a load this walk never
    /// visits.
    SelfReference,
    /// A surviving load of a field that HAS stores, in a graph where program
    /// order is not dominance (`program_order_proves_dominance` is false —
    /// any branch, join or multi-input φ). Fact 2 is unprovable, not false.
    ///
    /// This is the refusal a hot loop hits: the loop body's own branches deny
    /// the whole graph the ordering stand-in, even for a store and a load that
    /// sit next to each other in one block.
    LoadNotAnswerable,
    /// `Op::NewArray`. Array scalar replacement handles a constant-length
    /// array whose every index is a constant in range; anything else — a
    /// non-constant length, a non-constant or out-of-range index — is refused
    /// here rather than guessed at.
    ArrayNotReplaceable(ArrayRefusal),
}

/// Why an `Op::NewArray` was not scalar-replaced. See
/// [`ScalarRefusal::ArrayNotReplaceable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArrayRefusal {
    /// The length operand is not a compile-time constant, so the array has no
    /// statically-known number of slots to map onto scalars.
    LengthNotConstant,
    /// The constant length is larger than [`MAX_SCALAR_ARRAY_LEN`]. Replacing a
    /// long array trades one allocation for a large number of live values, and
    /// the register allocator pays for every one of them.
    LengthTooLarge(usize),
    /// An element access whose index is not a compile-time constant. Which slot
    /// it names is unknown, so neither a load nor a store can be attributed.
    IndexNotConstant,
    /// A constant index outside `0..len`. The access throws
    /// `ArrayIndexOutOfBoundsException` at run time, and deleting the array
    /// deletes the throw.
    IndexOutOfRange,
    /// The array's elements are references (`anewarray`). See the refusal
    /// site: the applier has no spelling for a `null` zero default.
    ReferenceElements,
    /// `Op::ArrayLength` names this array. The length IS a compile-time
    /// constant and could be folded to it, but no applier arm does that today,
    /// so replacing the array would leave a live node reading a deleted one.
    /// Refused rather than half-applied.
    LengthReadNotFolded,
}

pub struct EscapeAnalysisResult {
    /// Escape states for all allocation sites.
    ///
    /// May contain [`EscapeState::PartialEscape`], which the connection graph
    /// never does — see that variant's documentation.
    pub escape_states: HashMap<NodeId, EscapeState>,
    /// Allocations that can be scalar-replaced (NoEscape).
    pub scalar_replaceable: Vec<ScalarReplacementInfo>,
    /// Allocations that can be stack-allocated (ArgEscape).
    pub stack_allocatable: Vec<NodeId>,
    /// Synchronized blocks on non-escaping objects (lock elision candidates),
    /// as a flat, ascending node list.
    ///
    /// **This is the flattened union of [`Self::lock_elisions`] and it is only
    /// sound applied a whole plan at a time.** Kept in this shape because it is
    /// what `apply_ea_to_ir` reads.
    pub elide_locks: Vec<NodeId>,
    /// The same offers, grouped per object, which is the granularity at which
    /// they are correct.
    ///
    /// # The partial-application hazard
    ///
    /// This module *offers* elisions; `apply_ea_to_ir` in `jit/src/lib.rs`
    /// independently refuses individual ones — for a monitor a safepoint slot
    /// names, one whose memory-token chain cannot be spliced, or one whose value
    /// is still read. Each refusal drops **one node** out of a balanced monitor
    /// sequence, and a `monitorexit` whose `monitorenter` was elided throws
    /// `IllegalMonitorStateException`; the reverse leaks a monitor past the end
    /// of the frame.
    ///
    /// A per-node list cannot express "these four go together", so this field
    /// does. [`apply_lock_elision_plan`] applies one plan atomically, and
    /// [`apply_lock_elision`] refuses any request that is not balance-preserving.
    /// The consumer in `jit/src/lib.rs` still reads the flat list; the required
    /// edit is recorded in `docs/jit/lock-elimination.md` §6.
    pub lock_elisions: Vec<LockElisionPlan>,
    /// Adjacent lock regions on the same confined object that may be merged.
    ///
    /// Independent of [`Self::lock_elisions`]: a plan deletes only its own two
    /// monitor nodes and leaves a balanced structure whether or not any elision
    /// landed. Apply with [`apply_lock_coarsening`], which re-verifies.
    pub lock_coarsening: Vec<LockCoarseningPlan>,
    /// Every lock transform this analysis refused, with the reason.
    ///
    /// The `NodeId` is the object, except for
    /// [`LockRefusal::AmbiguousMonitorOperand`] where it is the monitor node
    /// that could not be attributed. Sorted and deduplicated. Purely
    /// informational — it exists so a fail-closed answer is *reportable* rather
    /// than invisible.
    pub lock_refusals: Vec<(NodeId, LockRefusal)>,
    /// Every allocation [`find_scalar_replacements`] refused, with the reason.
    ///
    /// Sorted by node id and deduplicated; one entry per refused allocation
    /// (the walk stops at the first refusal, so an object has one reason).
    /// Informational only — see [`ScalarRefusal`].
    pub scalar_refusals: Vec<(NodeId, ScalarRefusal)>,
    /// Allocations whose every escape site is cold, with those sites.
    /// Informational: nothing acts on it yet.
    pub partial_escapes: Vec<PartialEscapeInfo>,
    /// `(allocation, observing node)` pairs for every **live** observation of an
    /// object's identity: `Op::RefCompare`, `Op::IdentityHash`,
    /// `Op::MonitorEnter`, `Op::MonitorExit`, `Op::MonitorWait`,
    /// `Op::MonitorNotify`. Sorted and deduplicated.
    ///
    /// Every allocation named here is excluded from
    /// [`Self::scalar_replaceable`], whatever its escape state. Removing the
    /// observation (marking the node `Op::Dead` — e.g. by applying lock elision)
    /// and re-running the analysis makes the object eligible again; that
    /// two-phase story is the *only* supported way to scalar-replace an object
    /// whose identity is observed.
    pub identity_observations: Vec<(NodeId, NodeId)>,
    /// Statistics.
    pub stats: EscapeAnalysisStats,
}

/// Summary statistics for the analysis.
#[derive(Debug, Default, Clone)]
pub struct EscapeAnalysisStats {
    pub total_allocations: usize,
    pub no_escape: usize,
    /// Allocations reported [`EscapeState::PartialEscape`]. These are *not*
    /// counted in `arg_escape`/`global_escape`, so the four state counters still
    /// sum to `total_allocations`.
    pub partial_escape: usize,
    pub arg_escape: usize,
    pub global_escape: usize,
    pub scalar_replaced: usize,
    pub stack_allocated: usize,
    /// Monitor nodes offered for elision (the length of `elide_locks`).
    pub locks_elided: usize,
    /// Objects with at least one complete elision plan.
    pub lock_objects_elided: usize,
    /// Adjacent region pairs offered for coarsening. Each plan removes two
    /// monitor nodes.
    pub locks_coarsened: usize,
    /// Lock transforms refused, for any reason. The counterpart of
    /// `identity_blocked`: it says what failing closed on locks costs.
    pub locks_refused: usize,
    /// `NoEscape` allocations refused scalar replacement *only* because their
    /// identity is observed. The measurement that says how much an
    /// identity-observation folder (acmp on a fresh object, elided monitors)
    /// would be worth.
    pub identity_blocked: usize,
}

// ── Materialization (partial escape analysis) ───────────────────────────

/// A point where a partially-escaping object must be materialized on the heap.
pub struct MaterializationPoint {
    /// The allocation node being materialized.
    pub alloc_node: NodeId,
    /// The node where the object escapes.
    pub escape_point: NodeId,
    /// Field values at the moment of materialization.
    pub fields_at_escape: Vec<Option<NodeId>>,
}

// ── Phase 1: Build connection graph ─────────────────────────────────────

/// Build the initial connection graph by scanning all nodes in the IR.
fn build_connection_graph(graph: &Graph) -> ConnectionGraph {
    let mut cg = ConnectionGraph::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        match &node.op {
            // Allocation sites: point to themselves, start as NoEscape.
            Op::New { .. } | Op::NewArray { .. } => {
                cg.add_points_to(id, id);
                cg.set_escape(id, EscapeState::NoEscape);
            }

            // Parameters: a reference passed in from the caller is, by
            // definition, already reachable from outside this method.  Anything
            // published into a parameter's field — the classic lazy-init
            // `this.field = new ...; return this.field;` getter — is visible to
            // the caller after we return, so a Param holder must carry an
            // escaping state.  Without this, `this`/argument holders defaulted
            // to `NoEscape` and the store-publish rule below never fired for
            // them, letting a value stored into `this.field` be wrongly
            // scalar-replaced (which would elide the `putfield` and leave the
            // field null).  This was first investigated while chasing the
            // keycloak CredentialModel lazy-init failure; that specific failure
            // was later shown not to use this scalar-replacement path, but this
            // invariant is still required for any IR-compiled lazy-init getter.
            // Marking a primitive param is harmless: primitives are never
            // allocations or field holders.
            Op::Param(_) => {
                cg.set_escape(id, EscapeState::GlobalEscape);
            }

            // Phi nodes: deferred edges to all reference inputs.
            Op::Phi => {
                for &inp in &node.inputs {
                    if is_ref_producer(graph, inp) {
                        cg.add_deferred(id, inp);
                    }
                }
            }

            // Store: field edge from holder to stored value.
            // Layout: inputs = [holder, value] (see MEM_HOLDER_INPUT /
            // STORE_VALUE_INPUT).  `field_idx` is a field index, NOT a
            // MemKind tag.
            Op::Store(field_idx) => {
                if let (Some(obj), Some(val)) = (store_holder(node), store_value(node)) {
                    cg.add_field_edge(obj, *field_idx, val);
                }
                // A malformed store (missing holder/value) is conservatively
                // handled in propagation, where any unresolved holder forces
                // the stored value to escape.
            }

            // Load: the result reads a *field* of the holder, so its
            // provenance is whatever was stored INTO that field, NOT the
            // holder object itself.
            //
            // BUGFIX / landmine closed: the previous implementation added a
            // deferred edge `load -> holder`, which made the load alias the
            // holder allocation.  That is unsound — it conflates "I read a
            // field of X" with "I am X", so escaping the loaded value would
            // wrongly escape the holder (and vice versa), and a load of a
            // primitive field would spuriously create a reference edge.
            //
            // We instead record a *field load* and resolve it through
            // `field_edges[(holder_alloc, field_idx)]` during propagation,
            // making loads field-sensitive.  If the field's contents are
            // unknown (no matching store, or an unresolvable holder), the
            // load conservatively points to nothing here and is treated as a
            // fresh unknown reference by escape propagation.
            Op::Load(field_idx) => {
                if let Some(obj) = load_holder(node) {
                    cg.field_loads.push((id, obj, *field_idx));
                }
            }

            // Return / Throw: any ref input escapes globally.  A thrown
            // reference leaves the frame exactly as a returned one does — the
            // handler that catches it is, from this method's point of view,
            // outside.
            Op::Return | Op::Throw => {
                for &inp in &node.inputs {
                    if is_ref_producer(graph, inp) {
                        cg.set_escape(inp, EscapeState::GlobalEscape);
                    }
                }
            }

            // Call: reference arguments escape to callee (ArgEscape at minimum).
            Op::Call => {
                for &inp in &node.inputs {
                    if is_ref_producer(graph, inp) {
                        cg.set_escape(inp, EscapeState::ArgEscape);
                    }
                }
            }

            // UNMODELLED NODE ⇒ its reference operands escape globally.
            //
            // `Op::Other` is not "a node that does nothing"; it is the bridge's
            // catch-all. `jit/src/lib.rs::ir_op_to_ea_op` funnels every
            // `ir::Op` this module has no variant for into it, and two of those
            // publish or republish a reference:
            //
            //   * `ir::Op::LambdaIntToDouble` — `MemAccess::Opaque`,
            //     `MemEffect::OPAQUE`, a safepoint, and it *invokes the lambda
            //     it is handed*. It is a call in everything but name.
            //   * `ir::Op::Guard` — a deoptimization point. Taking it rebuilds
            //     an interpreter frame from the references the frame state
            //     names, which republishes them outside the compiled frame.
            //
            // Leaving these contributing nothing was fail-OPEN, and the two
            // consumers disagreed about it. `find_scalar_replacements` walks
            // the allocation's uses and refuses an `Op::Other` use at its
            // catch-all arm, so scalar replacement was already safe. **Lock
            // elision walks no uses at all** — `find_lock_elision_plans`' E2 is
            // `cg.get_escape(object).is_confined()` — so an object handed to an
            // unmodelled node still looked confined and had its monitors
            // deleted, dropping a `hb` edge the JMM argument in §4 assumes
            // cannot exist.
            //
            // `GlobalEscape` rather than `ArgEscape`: we do not know that the
            // destination is a callee, and `stack_allocatable` (which collects
            // `ArgEscape`) should not fill up with objects whose fate is simply
            // unknown. This costs scalar replacement nothing — the use walk
            // refused these objects already — and costs lock elision only where
            // it was unsound.
            Op::Other => {
                for &inp in &node.inputs {
                    if is_ref_producer(graph, inp) {
                        cg.set_escape(inp, EscapeState::GlobalEscape);
                    }
                }
            }

            _ => {}
        }
    }

    cg
}

/// Heuristic: does this node produce a reference that could be an object?
fn is_ref_producer(graph: &Graph, node: NodeId) -> bool {
    if node >= graph.nodes.len() {
        return false;
    }
    matches!(
        graph.nodes[node].op,
        Op::New { .. } | Op::NewArray { .. } | Op::Phi | Op::Load(_) | Op::Param(_) | Op::Call
    )
}

// ── Phase 2: Propagate escape states ────────────────────────────────────

/// Iteration bound on the escape fixed point. Reaching it means the lattice
/// did NOT converge; see [`escalate_all_to_global`] for why that must fail
/// closed rather than be silently accepted.
const MAX_ESCAPE_ITERATIONS: usize = 100;

/// Force every node in `graph` to [`EscapeState::GlobalEscape`].
///
/// The fail-closed answer: `GlobalEscape` is the top of the lattice, so no
/// consumer can act on it (`find_scalar_replacements` and `find_lock_elisions`
/// both require `== NoEscape`). Every node id in `0..graph.nodes.len()` is
/// written explicitly, because [`ConnectionGraph::get_escape`] reports an
/// *absent* node as `NoEscape` — leaving the map untouched would keep exactly
/// the nodes that were never reached looking optimisable.
fn escalate_all_to_global(cg: &mut ConnectionGraph, graph: &Graph) {
    for id in 0..graph.nodes.len() {
        cg.set_escape(id, EscapeState::GlobalEscape);
    }
}

/// Fixed-point propagation of escape states through the connection graph.
fn propagate_escape_states(cg: &mut ConnectionGraph, graph: &Graph) {
    let mut changed = true;
    let mut iteration = 0;
    const MAX_ITERATIONS: usize = MAX_ESCAPE_ITERATIONS;

    while changed && iteration < MAX_ITERATIONS {
        changed = false;
        iteration += 1;

        // Propagate through deferred edges: if A defers to B, A's
        // escape state must be at least B's.
        let deferred_snapshot: Vec<(NodeId, Vec<NodeId>)> = cg
            .deferred_edges
            .iter()
            .map(|(&k, v)| (k, v.iter().copied().collect()))
            .collect();

        for (node, targets) in &deferred_snapshot {
            for &target in targets {
                let target_state = cg.get_escape(target);
                let my_state = cg.get_escape(*node);
                let joined = my_state.join(target_state);
                if joined != my_state {
                    cg.set_escape(*node, joined);
                    changed = true;
                }
            }
        }

        // Resolve field loads field-sensitively: a `Load(field)` of `holder`
        // points to whatever was stored into that field of any allocation the
        // holder may resolve to.  We materialise this as deferred edges
        // `load -> stored_value` (one per matching store), so the load
        // inherits both the points-to set and the escape state of the field
        // contents on the next deferred-propagation pass.  Adding edges that
        // already exist is a no-op (HashSet insert), so this is monotone and
        // converges with the rest of the fixed point.
        let field_loads = cg.field_loads.clone();
        for (load, holder, field) in field_loads {
            let holder_allocs = cg.resolve_points_to(holder);
            // Only resolve field-sensitively when the index validly addresses
            // the holder's allocations.  An out-of-range index (e.g. a leaked
            // MemKind tag) leaves the load as a conservative unknown rather
            // than aliasing an unrelated field edge that happens to share the
            // numeric key.
            if !field_in_range(graph, &holder_allocs, field) {
                continue;
            }
            for &alloc in &holder_allocs {
                if let Some(stored) = cg.field_edges.get(&(alloc, field)).cloned() {
                    for &val in &stored {
                        if cg
                            .deferred_edges
                            .get(&load)
                            .map_or(true, |s| !s.contains(&val))
                        {
                            cg.add_deferred(load, val);
                            changed = true;
                        }
                    }
                }
            }
        }

        // Propagate through points-to sets: if node N points to alloc A,
        // and N escapes, then A escapes at least as much.
        let alloc_ids: Vec<NodeId> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::New { .. } | Op::NewArray { .. }))
            .map(|(id, _)| id)
            .collect();

        for &alloc in &alloc_ids {
            // Check all nodes that might point to this allocation.
            for (id, node) in graph.nodes.iter().enumerate() {
                if id == alloc {
                    continue;
                }
                let pts = cg.resolve_points_to(id);
                if pts.contains(&alloc) {
                    let node_escape = cg.get_escape(id);
                    let alloc_escape = cg.get_escape(alloc);
                    let joined = alloc_escape.join(node_escape);
                    if joined != alloc_escape {
                        cg.set_escape(alloc, joined);
                        changed = true;
                    }
                }

                // Store escape rule (the soundness core of the lattice):
                // a value stored into a field of a holder is reachable by
                // anyone who can reach the holder.  Therefore the stored
                // value must escape *at least as much as* the holder.
                //
                // This must fire for BOTH GlobalEscape (the holder is visible
                // to other threads / the heap) AND ArgEscape (the holder is
                // passed to a callee, which can then reach the stored value):
                // the previous code only handled GlobalEscape, leaving values
                // stored into Arg-escaping holders spuriously NoEscape, which
                // is unsound for stack-allocation / lock-elision decisions.
                if let Op::Store(field_idx) = &node.op {
                    if let (Some(obj), Some(val)) = (store_holder(node), store_value(node)) {
                        // The holder's "reach" is the max escape of any
                        // allocation it may resolve to, joined with the
                        // holder node's own escape (covers Param/Call holders
                        // that have no concrete allocation here).
                        //
                        // NOTE: whole-object escape is intentionally
                        // field-INsensitive — a value stored into *any* field
                        // of an escaping holder escapes, regardless of which
                        // field index it is, so we do not gate this on
                        // `field_in_range` here.  Field validity only matters
                        // for field-sensitive *reads* and scalar replacement.
                        let obj_pts = cg.resolve_points_to(obj);
                        let mut holder_escape = cg.get_escape(obj);
                        for &a in &obj_pts {
                            holder_escape = holder_escape.join(cg.get_escape(a));
                        }
                        // UNKNOWN DESTINATION ⇒ GLOBAL ESCAPE.
                        //
                        // A holder that resolves to no allocation at all is a
                        // write to somewhere this analysis cannot name: the
                        // classic case is `putstatic`, whose base is a class /
                        // static-area node, but it also covers a bridge gap
                        // (a holder that mapped to `usize::MAX`) and any
                        // opaque reference with no points-to entry.
                        //
                        // `get_escape` reports an unseen node as `NoEscape`, so
                        // before this the rule below simply did not fire and a
                        // value written into a *static field* stayed `NoEscape`
                        // — scalar-replaceable, lock-elidable, and reachable by
                        // every other thread in the VM. `Op::Param` holders were
                        // already covered (the build phase marks them
                        // `GlobalEscape`); nothing covered the rest.
                        //
                        // Fail closed: an unnameable destination is the global
                        // heap until proven otherwise.
                        if obj_pts.is_empty() {
                            holder_escape = holder_escape.join(EscapeState::GlobalEscape);
                        }

                        if holder_escape > EscapeState::NoEscape {
                            let val_escape = cg.get_escape(val);
                            let joined = val_escape.join(holder_escape);
                            if joined != val_escape {
                                cg.set_escape(val, joined);
                                // Propagate to allocations the value points to.
                                let val_pts = cg.resolve_points_to(val);
                                for &vp in &val_pts {
                                    let vp_escape = cg.get_escape(vp);
                                    let vp_joined = vp_escape.join(holder_escape);
                                    if vp_joined != vp_escape {
                                        cg.set_escape(vp, vp_joined);
                                    }
                                }
                                changed = true;
                            }
                        }
                        let _ = field_idx;
                    } else if let Some(val) = store_value(node) {
                        // Malformed store: a value with no resolvable holder.
                        // We cannot know where it was written, so be
                        // conservative and let it escape globally rather than
                        // silently keeping it NoEscape.
                        if cg.get_escape(val) != EscapeState::GlobalEscape {
                            cg.set_escape(val, EscapeState::GlobalEscape);
                            changed = true;
                        }
                    }
                }
            }
        }
    }

    // Non-convergence must FAIL CLOSED.
    //
    // This lattice only ever joins UPWARD (`NoEscape < ArgEscape <
    // GlobalEscape`; `set_escape` itself joins), so every intermediate state
    // is an UNDER-estimate of escape. Stopping at the iteration cap therefore
    // leaves states too LOW, and both consumers — `find_scalar_replacements`
    // and `find_lock_elisions` — act on `== NoEscape`. An unconverged run
    // would hand them objects that actually escape: scalar replacement then
    // deletes stores another party can observe, and lock elision drops a
    // monitor another thread contends on. Both are silent wrong-answer bugs,
    // not crashes.
    //
    // `changed` is still true exactly when the `while` exited on the iteration
    // bound rather than on a fixed point. In that case escalate EVERY node to
    // `GlobalEscape`, which disables every downstream optimisation for this
    // method and leaves the ordinary heap-allocating, fully-locked code.
    // Slower, always correct.
    if changed {
        escalate_all_to_global(cg, graph);
    }
}

// ── Phase 3a: Identity observations ─────────────────────────────────────

/// Every **live** observation of an object's *identity*, as
/// `(allocation, observing node)` pairs, sorted and deduplicated.
///
/// # Why identity is a separate question from escape
///
/// Scalar replacement deletes the object's address. Three JVM operations can
/// see that address without the object escaping anywhere:
///
/// * `if_acmpeq` / `if_acmpne` — reference `==`. Two scalar-replaced objects
///   with equal fields are indistinguishable; two heap objects are not.
/// * `System.identityHashCode` (and a non-overridden `Object.hashCode`) — a
///   stable per-object value derived from the header, which a bag of scalars
///   does not have.
/// * `monitorenter` / `monitorexit` — the monitor *is* the object header.
///
/// An object with any of these live cannot be scalar-replaced. The
/// qualification in the review item — *"unless the observation is itself
/// eliminated"* — is taken literally here: the observing node must be
/// `Op::Dead`, i.e. some pass has already removed it. Anything weaker requires
/// this module and its consumer to agree about a *future* elimination, and the
/// consumer (`apply_ea_to_ir` in `jit/src/lib.rs`) can and does refuse elisions
/// this module offers — for a lock naming a safepoint slot, for one whose
/// memory chain cannot be spliced, for one whose value is still read. A monitor
/// that survives on a scalar-replaced object is a `monitorenter` on a deleted
/// reference.
///
/// So the supported way to scalar-replace a synchronized object is two-phase:
/// run the analysis, [`apply_lock_elision`] the monitors it offers (they become
/// `Op::Dead`), then run the analysis **again** on the mutated graph. The
/// second run sees no live observation and replaces the object. This costs one
/// extra pass and, unlike the coupled version, cannot desynchronise.
///
/// # Producer status
///
/// [`Op::RefCompare`] and [`Op::IdentityHash`] have no producer yet: the bridge
/// maps `ir::Op::Cmp` to [`Op::Other`], and an identity-hash call to
/// [`Op::Call`]. Both currently reach [`find_scalar_replacements`]' catch-all
/// (`Other`) or escape the object (`Call`), so the *outcome* is already
/// fail-closed — but it is incidental, unmeasurable and would silently become
/// wrong if `Op::Other` were ever relaxed. Wiring the two variants is the
/// out-of-scope edit recorded in `docs/jit/escape-analysis.md`.
pub fn find_identity_observations(cg: &ConnectionGraph, graph: &Graph) -> Vec<(NodeId, NodeId)> {
    let mut pairs: Vec<(NodeId, NodeId)> = Vec::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        // The operands whose identity this node observes. `None` = not an
        // identity observation at all.
        let observed: &[NodeId] = match &node.op {
            // The monitor is the object header of input 0 — the same operand
            // `find_lock_elisions` reads, so the two agree about which object a
            // monitor names.  `wait`/`notify` are monitor operations too: they
            // read the header AND require the monitor to be held.
            Op::MonitorEnter
            | Op::MonitorExit
            | Op::MonitorWait
            | Op::MonitorNotify
            | Op::IdentityHash => match node.inputs.first() {
                Some(first) => std::slice::from_ref(first),
                None => continue,
            },
            // Both sides of an `acmp` have their addresses compared.
            Op::RefCompare => node.inputs.as_slice(),
            _ => continue,
        };
        for &operand in observed {
            // The operand itself may be the allocation (direct use), or a copy
            // (φ / field load) that resolves to it.
            let mut allocs = cg.resolve_points_to(operand);
            if matches!(
                graph.nodes.get(operand).map(|n| &n.op),
                Some(Op::New { .. }) | Some(Op::NewArray { .. })
            ) {
                allocs.insert(operand);
            }
            for alloc in allocs {
                pairs.push((alloc, id));
            }
        }
    }

    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

// ── Phase 3b: Find scalar replacement candidates ────────────────────────

/// Identify allocations that can be decomposed into scalar field values.
fn find_scalar_replacements(
    cg: &ConnectionGraph,
    graph: &Graph,
) -> (Vec<ScalarReplacementInfo>, Vec<(NodeId, ScalarRefusal)>) {
    let mut results = Vec::new();
    // One reason per refused allocation. The walk below stops at its
    // first refusal, so an object never accumulates two.
    let mut refusals: Vec<(NodeId, ScalarRefusal)> = Vec::new();

    // Program order is only dominance in a branch-free graph; see
    // `program_order_proves_dominance`. Computed once — it is a property of the
    // graph, not of an object.
    let dominance_proved = program_order_proves_dominance(graph);
    // An allocation whose address is observed by a LIVE node may not be
    // replaced, whatever its escape state.
    let identity_observed: HashSet<NodeId> = find_identity_observations(cg, graph)
        .into_iter()
        .filter(|&(_, observer)| {
            !matches!(graph.nodes.get(observer).map(|n| &n.op), Some(Op::Dead))
        })
        .map(|(alloc, _)| alloc)
        .collect();

    for (id, node) in graph.nodes.iter().enumerate() {
        // An object contributes its field count; an array of proven constant
        // length contributes its element count, which is the same thing for
        // every question below. `class_id` is meaningless for an array and is
        // never read for one -- `array_element_type` is what the consumers ask.
        let shape: Option<(u32, usize, Option<u8>)> = match &node.op {
            Op::New {
                class_id,
                num_fields,
            } => Some((*class_id, *num_fields, None)),
            Op::NewArray {
                element_type,
                length,
            } if *element_type == 0 => {
                // `anewarray`: a REFERENCE array (the bridge writes
                // `element_type = 0` for it; JVM atypes start at 4). Refused,
                // and not for want of a length. A never-stored slot of one has
                // to be answered with `null`, and the applier's zero default is
                // a numeric constant -- an `Int`-typed zero forwarded where a
                // reference belongs is the silent-null shape this codebase has
                // paid for more than once. Primitive element types have an
                // honest zero; a reference does not, here.
                let _ = length;
                refusals.push((
                    id,
                    ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::ReferenceElements),
                ));
                None
            }
            Op::NewArray {
                element_type,
                length,
            } => match length {
                Some(n) if *n <= MAX_SCALAR_ARRAY_LEN => Some((0, *n, Some(*element_type))),
                Some(n) => {
                    refusals.push((
                        id,
                        ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::LengthTooLarge(*n)),
                    ));
                    None
                }
                None => {
                    refusals.push((
                        id,
                        ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::LengthNotConstant),
                    ));
                    None
                }
            },
            _ => None,
        };
        if let Some((class_id, num_fields, array_element_type)) = shape {
            let (class_id, num_fields) = (&class_id, &num_fields);
            let state = cg.get_escape(id);
            if state != EscapeState::NoEscape {
                refusals.push((id, ScalarRefusal::Escapes(state)));
                continue;
            }
            // IDENTITY GATE. Checked before anything else so a `synchronized`
            // or `==`-compared object is refused whole, not per-use — an
            // observation reached through an alias this loop does not classify
            // as transparent would otherwise slip past the use walk below.
            if identity_observed.contains(&id) {
                refusals.push((id, ScalarRefusal::IdentityObserved));
                continue;
            }

            let mut field_values: Vec<Option<NodeId>> = vec![None; *num_fields];
            // Per field, the id of the `Store` node that set `field_values`.
            // Used to enforce program-order "last write wins" independent of the
            // order in which the worklist visits the allocation's stores (it
            // pops LIFO, so two stores to the same field can be seen in either
            // order). `add_node` assigns ids in creation = program order, so a
            // larger store-node id is a later store.
            let mut field_value_store: Vec<Option<NodeId>> = vec![None; *num_fields];
            // POSITIONAL RECORD: every store to each field, not just the last.
            // Sorted by node id after the walk, because the worklist pops LIFO.
            let mut field_stores: Vec<Vec<FieldStore>> = vec![Vec::new(); *num_fields];
            let mut replaced_loads = Vec::new();
            // Parallel to `replaced_loads`: the field each one reads, kept so
            // the per-load resolution below does not have to re-derive it.
            let mut load_fields: Vec<usize> = Vec::new();
            let mut eliminated_stores = Vec::new();
            let mut can_replace = true;
            // Set beside every `can_replace = false` so the two cannot drift.
            let mut refusal: Option<ScalarRefusal> = None;

            // WIDENED ACCEPTED SHAPE (increment 1):
            //
            // The narrow pre-widening shape accepted scalar replacement only
            // when *every* direct use of the allocation was a field
            // load/store/monitor/length on the allocation itself.  Two
            // provably-sound relaxations broaden the set of objects that
            // scalar-replace without weakening the escape guarantees:
            //
            //   1. `Op::Dead` uses are *skipped*, not rejected.  A use list
            //      may still reference a node that an earlier pass marked
            //      dead; a dead node observes nothing, so it can never make a
            //      NoEscape object escape.  Previously such a stale edge hit
            //      the catch-all and spuriously blocked SR.
            //
            //   2. A `NoEscape` `Op::Phi` use is treated as a *transparent
            //      copy*: the allocation may flow through a phi that merges it
            //      with another reference (e.g. `o = cond ? new A() : a2`)
            //      yet never escapes.  We accept the allocation as long as the
            //      phi itself is NoEscape (so propagation already proved no
            //      use beyond the phi escapes) AND the phi's own uses are only
            //      field loads/stores addressing valid fields of THIS
            //      allocation — i.e. the phi forwards the reference to more
            //      in-method field accesses that we also fold in.  A phi whose
            //      uses include anything else, or that is not NoEscape, still
            //      bails.  Loads/stores reached through such a phi are
            //      collected for replacement just like direct ones.
            //
            // Soundness anchor: we only ever fold loads/stores whose holder
            // resolves (directly, or via a NoEscape transparent phi) to THIS
            // allocation, and the object's NoEscape state — computed by the
            // sound propagation in `propagate_escape_states`, which escapes
            // any value reaching a call arg / field store / return / throw —
            // is the precondition for entering this loop at all.

            // Work list of nodes (uses) to validate. We seed it with the
            // allocation's direct uses and may push a transparent phi's uses
            // when we accept it.  `transparent` is the set of nodes that are
            // provably aliases of THIS allocation and nothing else (the
            // allocation itself plus any accepted singleton-phi); a field
            // op's holder is accepted iff it is in this set.
            let mut worklist: Vec<NodeId> = node.uses.clone();
            let mut visited: HashSet<NodeId> = HashSet::new();
            let mut transparent: HashSet<NodeId> = HashSet::new();
            transparent.insert(id);

            while let Some(use_id) = worklist.pop() {
                if use_id >= graph.nodes.len() || !visited.insert(use_id) {
                    continue;
                }
                let use_node = &graph.nodes[use_id];
                match &use_node.op {
                    // Stale dead edge — observes nothing, skip it.
                    Op::Dead => {}
                    Op::Store(field_idx) => {
                        // This allocation may appear in a Store either as the
                        // HOLDER (storing into our field — recordable) or as
                        // the VALUE (we are being published into another
                        // object — that escapes us, so bail).  Distinguish by
                        // role rather than assuming holder.  A holder that is
                        // a transparent phi alias of this allocation counts as
                        // a holder edge too.
                        let holder = store_holder(use_node);
                        let is_holder = holder.is_some_and(|h| transparent.contains(&h));
                        let is_value = store_value(use_node) == Some(id);
                        if is_holder && *field_idx < *num_fields {
                            // Record the value written into our field.  If the
                            // value operand is malformed, leave the field as
                            // None (uninitialised) rather than guessing.
                            if let Some(stored_val) = store_value(use_node) {
                                // LAST WRITE WINS (program order). The worklist
                                // pops uses LIFO, so the same field's stores can
                                // be visited out of program order; only let a
                                // store overwrite a previously-recorded value
                                // when it is the same-or-later store (larger
                                // node id). Without this, an EARLIER store could
                                // clobber a LATER one's value and a scalar-
                                // replaced object would forward the stale first
                                // value to post-store loads
                                // (`test_multiple_stores_to_same_field_last_wins`).
                                let supersedes = field_value_store[*field_idx]
                                    .map_or(true, |prev| use_id >= prev);
                                if supersedes {
                                    field_values[*field_idx] = Some(stored_val);
                                    field_value_store[*field_idx] = Some(use_id);
                                }
                                // …and the POSITIONAL record keeps every store,
                                // which is what lets a load see the store that
                                // precedes *it* rather than the last one in the
                                // method.
                                field_stores[*field_idx].push(FieldStore {
                                    store: use_id,
                                    value: stored_val,
                                });
                            }
                            // Both stores are eliminated regardless of which
                            // value wins — the heap object is gone, so every
                            // store to it is dead.
                            eliminated_stores.push(use_id);
                        } else if is_value {
                            // Published into another object's field -> escapes;
                            // cannot scalar-replace.
                            refusal = Some(ScalarRefusal::StoredIntoAnotherObject);
                            can_replace = false;
                            break;
                        } else {
                            // Holder role but out-of-range / malformed field.
                            refusal = Some(ScalarRefusal::FieldIndexOutOfRange);
                            can_replace = false;
                            break;
                        }
                    }
                    Op::Load(field_idx) => {
                        // Only a load OF this object's field (directly or via
                        // a transparent phi alias) is replaceable.
                        let holder = load_holder(use_node);
                        let is_holder = holder.is_some_and(|h| transparent.contains(&h));
                        if is_holder && *field_idx < *num_fields {
                            replaced_loads.push(use_id);
                            load_fields.push(*field_idx);
                        } else {
                            refusal = Some(ScalarRefusal::FieldIndexOutOfRange);
                            can_replace = false;
                            break;
                        }
                    }
                    // A NoEscape phi is a transparent copy of the reference,
                    // but ONLY when it provably resolves to *exactly* this
                    // allocation and nothing else.  If the phi merged this
                    // allocation with a different reference `a2`, a load
                    // reached through it could be reading `a2`'s field on the
                    // `a2` control path; folding that load to THIS object's
                    // field value would be a miscompile (the kafka bug-25
                    // class of missed-alias error).  We therefore require the
                    // phi's resolved points-to set to be the singleton {id}.
                    // We also require it to be NoEscape (a redundant but cheap
                    // guard, since a phi that escapes would already have
                    // forced `id` to escape and we would not be in this loop).
                    Op::Phi => {
                        let phi_no_escape = cg.get_escape(use_id) == EscapeState::NoEscape;
                        let pts = cg.resolve_points_to(use_id);
                        let only_this_alloc = pts.len() == 1 && pts.contains(&id);
                        // CRITICAL soundness guard: a points-to set of exactly
                        // {id} is necessary but NOT sufficient.  A reference
                        // input with *unknown* provenance — a `Param`, `Call`
                        // result, or field `Load` — contributes no entry to
                        // the points-to set, so a phi merging `id` with such an
                        // input would still resolve to the singleton {id} while
                        // genuinely also carrying that other object.  Folding a
                        // load through it would miscompile the path on which
                        // the phi takes the unknown reference.  Require every
                        // *value* input to be a concrete alias of this
                        // allocation (an allocation that is `id`, or another
                        // transparent phi we have already accepted).
                        //
                        // Only the control anchor may be skipped. This used to
                        // skip anything `is_ref_producer` did not list, on the
                        // theory that such an input "cannot carry a foreign
                        // object" — but the EA graph is untyped, the bridge maps
                        // the `aconst_null` literal to `Const(0)` and a
                        // `checkcast` to `Other`, and both ARE references.
                        // `p = c ? new Foo() : null; p.x` folded to 0 instead of
                        // throwing, and `c ? new Foo() : (Foo) o` read the
                        // allocation's default instead of `o.x`. A φ that uses
                        // the allocation is a reference φ, so every value input
                        // is a reference; a primitive or unmapped input simply
                        // fails the singleton test and refuses.
                        let all_inputs_alias_id = use_node.inputs.iter().all(|&inp| {
                            let Some(input) = graph.nodes.get(inp) else {
                                return false;
                            };
                            if matches!(input.op, Op::Start | Op::Merge | Op::If) {
                                return true;
                            }
                            let ip = cg.resolve_points_to(inp);
                            ip.len() == 1 && ip.contains(&id)
                        });
                        if phi_no_escape && only_this_alloc && all_inputs_alias_id {
                            // The phi is a sound alias of this allocation:
                            // record it as transparent and follow its uses.
                            transparent.insert(use_id);
                            for &u in &use_node.uses {
                                worklist.push(u);
                            }
                        } else {
                            refusal = Some(ScalarRefusal::PhiNotTransparentCopy);
                            can_replace = false;
                            break;
                        }
                    }
                    // IDENTITY OBSERVATIONS.
                    //
                    // Previously `MonitorEnter`/`MonitorExit` were accepted
                    // unconditionally, on the reasoning that lock elision would
                    // remove them anyway. That coupling does not hold: this
                    // module *offers* an elision, and `apply_ea_to_ir`
                    // independently REFUSES it when a safepoint slot names the
                    // monitor, when its memory chain cannot be spliced, or when
                    // its value is still read (the `elide_locks` loop in
                    // `jit/src/lib.rs::apply_ea_to_ir`). Each
                    // refusal leaves a live `monitorenter` on an object this
                    // pass just deleted.
                    //
                    // The identity gate above has already rejected the object,
                    // so reaching here means the observation is `Op::Dead`
                    // (handled by the `Op::Dead` arm) or the gate and this walk
                    // disagree. Refuse either way — see
                    // `find_identity_observations` for the two-phase story that
                    // makes a synchronized object replaceable.
                    Op::MonitorEnter | Op::MonitorExit | Op::MonitorWait | Op::MonitorNotify => {
                        refusal = Some(ScalarRefusal::MonitorOperation);
                        can_replace = false;
                        break;
                    }
                    Op::RefCompare | Op::IdentityHash => {
                        refusal = Some(ScalarRefusal::IdentityObserved);
                        can_replace = false;
                        break;
                    }
                    // `arraylength` cannot name an ordinary object, so for one
                    // this arm is unreachable and skipping it is harmless. For
                    // an ARRAY candidate it is a live node that would be left
                    // reading the array this pass is about to delete, and
                    // nothing folds it to the constant length -- so refuse.
                    Op::ArrayLength => {
                        if array_element_type.is_some() {
                            refusal = Some(ScalarRefusal::ArrayNotReplaceable(
                                ArrayRefusal::LengthReadNotFolded,
                            ));
                            can_replace = false;
                            break;
                        }
                    }
                    _ => {
                        refusal = Some(ScalarRefusal::OpaqueUse);
                        can_replace = false;
                        break;
                    }
                }
            }

            // ── SELF-REFERENCE GATE ──────────────────────────────────
            //
            // The use walk above only ever visits uses of the allocation itself
            // and of the transparent φ copies it accepted. A reference to the
            // object that is recovered by **loading it back out of its own
            // field** is reachable through neither, so every use made through
            // that recovered reference is invisible to this loop:
            //
            //     o.next = o;          // recorded as a field store, fine
            //     Foo p = o.next;      // `p` IS `o`, but `p` is a Load node
            //     p.x = 5;             // holder is the LOAD — never visited
            //
            // `can_replace` stays true, `apply_scalar_replacement` deletes the
            // allocation, and `p.x = 5` is left writing through an `Op::Dead`
            // reference. That is the same *class* of defect as forwarding a
            // load to a later store: an alias the analysis never proved absent.
            //
            // Refuse whenever a value written into one of our fields may BE
            // this allocation. Asking `resolve_points_to` rather than testing
            // `value == id` also catches the φ-aliased spelling
            // (`o.next = (c ? o : o)`), and it is order-independent — unlike
            // the `is_value` role test in the walk above, which the `is_holder`
            // arm wins in exactly the self-store case.
            if can_replace {
                'self_ref: for stores in field_stores.iter() {
                    for fs in stores {
                        if fs.value == id || cg.resolve_points_to(fs.value).contains(&id) {
                            refusal = Some(ScalarRefusal::SelfReference);
                            can_replace = false;
                            break 'self_ref;
                        }
                    }
                }
            }

            // Program order is dominance for this object either because the
            // whole graph is branch-free, or because every access to it is in
            // one basic block — see `one_block_proves_dominance`. The second
            // is what a hot loop needs; the first cannot be true inside one.
            let dominance_proved = dominance_proved
                || one_block_proves_dominance(
                    graph,
                    id,
                    &transparent,
                    &field_stores,
                    &replaced_loads,
                );

            // ── Positional load resolution ───────────────────────────
            //
            // Now that every store is recorded with its position, answer each
            // replaced load with the store that actually dominates it. A load we
            // cannot answer refuses the WHOLE object: leaving it in the graph
            // while eliding the allocation would make it read a deleted object,
            // and leaving both means there was nothing to gain.
            for stores in field_stores.iter_mut() {
                stores.sort_unstable_by_key(|s| s.store);
            }
            let mut load_values: Vec<(NodeId, LoadResolution)> =
                Vec::with_capacity(replaced_loads.len());
            if can_replace {
                for (&load, &field) in replaced_loads.iter().zip(load_fields.iter()) {
                    let resolution =
                        resolve_field_load(&field_stores[field], load, dominance_proved);
                    if resolution == LoadResolution::Unknown {
                        refusal = Some(ScalarRefusal::LoadNotAnswerable);
                        can_replace = false;
                        break;
                    }
                    load_values.push((load, resolution));
                }
            }

            if can_replace {
                results.push(ScalarReplacementInfo {
                    alloc_node: id,
                    class_id: *class_id,
                    num_fields: *num_fields,
                    field_values,
                    field_stores,
                    replaced_loads,
                    load_values,
                    eliminated_stores,
                    array_element_type,
                    dominance_proved,
                });
            } else {
                // Every `can_replace = false` above sets `refusal`; the
                // `unwrap_or` is the belt-and-braces answer for a future arm
                // that forgets, and is deliberately the least specific reason
                // rather than a silent omission.
                refusals.push((id, refusal.unwrap_or(ScalarRefusal::OpaqueUse)));
            }
        }
    }

    refusals.sort_unstable();
    refusals.dedup();
    (results, refusals)
}

/// Resolve one field load against the field's positional store record.
///
/// `stores` must be sorted ascending by [`FieldStore::store`].
/// `dominance_proved` is [`program_order_proves_dominance`] for the graph.
///
/// The three answers, and why each is sound:
///
/// * **no store to this field anywhere** ⇒ [`LoadResolution::ZeroDefault`].
///   Sound *regardless of control flow*: if nothing in the method writes the
///   field, every read of it sees the allocation's zero-initialised value on
///   every path. This is the one case that survives a branchy graph.
/// * **program order is dominance, and some store precedes the load** ⇒ the
///   value of the latest such store. In a branch-free graph "precedes" is
///   "executed before", and the latest one is the one still in the field.
/// * **program order is dominance, and no store precedes the load** ⇒
///   `ZeroDefault`. This is the load-before-store case
///   (`Foo o = new Foo(); int a = o.x; o.x = 42;`): the later store is in the
///   load's *future* and must not be forwarded to it. Answering `42` here was
///   the miscompilation.
///
/// Everything else — a field with stores in a graph whose control flow we
/// cannot see — is [`LoadResolution::Unknown`].
fn resolve_field_load(
    stores: &[FieldStore],
    load: NodeId,
    dominance_proved: bool,
) -> LoadResolution {
    if stores.is_empty() {
        return LoadResolution::ZeroDefault;
    }
    if !dominance_proved {
        return LoadResolution::Unknown;
    }
    match stores.iter().rev().find(|s| s.store < load) {
        Some(s) => LoadResolution::Value(s.value),
        None => LoadResolution::ZeroDefault,
    }
}

// ── Phase 4: Lock elimination and coarsening ────────────────────────────
//
// Two transforms live here. They are independent: neither reads the other's
// result, and applying one does not make the other unsound.
//
// ════════════════════════════════════════════════════════════════════════
// (a) LOCK ELISION — delete every monitor operation on a confined object
// ════════════════════════════════════════════════════════════════════════
//
// Preconditions, all of which must hold (`find_lock_elision_plans`):
//
//   E1. Every live monitor-family node in the WHOLE graph is attributable —
//       `monitor_object` resolves its operand to exactly one allocation
//       through φ copies only. One unattributable monitor refuses every lock
//       plan in the method; see the note on [`LockRefusal::AmbiguousMonitorOperand`].
//   E2. The object is [`EscapeState::NoEscape`] (`is_confined`).
//   E3. No live `Object.wait`/`notify`/`notifyAll` names the object.
//   E4. The offer names EVERY monitor on the object, and it is ALL-OR-NOTHING.
//
// # JMM argument
//
// JLS 17.4.4 gives a *lock action* on monitor m a synchronizes-with edge to
// the *unlock action* on m that immediately precedes it in the synchronization
// order. The edge is only observable when the two actions belong to different
// threads: two synchronization actions of the *same* thread are already
// ordered by program order, and `hb` is transitively closed over program
// order, so an intra-thread lock/unlock pair on m contributes no `hb` edge
// that program order did not already contribute.
//
// E2 proves no reference to the object is reachable from any other thread for
// the object's whole lifetime, so no other thread can ever execute a
// synchronization action on m. Every action on m is therefore this thread's,
// and deleting all of them deletes no `hb` edge that constrains any legal
// execution. This is the JSR-133 "synchronization on a thread-local object is
// a no-op" argument; it is why the transform removes the implied fences too.
//
// E4 is what keeps the *lock state* consistent rather than the memory model:
// removing a strict subset of a balanced monitor sequence leaves a
// `monitorexit` with no matching `monitorenter` (immediate
// `IllegalMonitorStateException`) or the reverse (a monitor held past the end
// of the frame). This is exactly the hazard the partial-application note
// below is about.
//
// E3 is separate from the memory model: `wait` is the one monitor operation
// whose *semantics* — not just its ordering — depend on the monitor being
// held. It throws `IllegalMonitorStateException` when it is not, releases the
// monitor to its recorded reentry depth, and reacquires at that depth on
// wake. None of that survives elision, and none of it is expressible once the
// monitor is gone.
//
// ════════════════════════════════════════════════════════════════════════
// (b) LOCK COARSENING — merge two adjacent regions on the same object
// ════════════════════════════════════════════════════════════════════════
//
//     monitorenter o;  A  monitorexit o;   B   monitorenter o;  C  monitorexit o
//  ⇒  monitorenter o;  A                   B                    C  monitorexit o
//
// It deletes exactly two nodes — the inner `monitorexit`/`monitorenter` pair —
// and moves the gap `B` inside the critical section.
//
// Preconditions (`find_lock_coarsening_plans`):
//
//   C1. E1 (attributability) and E2 (confined) and E3 (no wait/notify).
//   C2. [`program_order_proves_dominance`] — without it "adjacent" and
//       "between" have no meaning in this graph, which has no CFG.
//   C3. The object's monitor sequence is balanced and properly nested, and the
//       two regions are both OUTERMOST (`depth == 0`).
//   C4. Every node strictly between `first.exit` and `second.enter` is on the
//       gap allowlist (`gap_node_refusal`): pure arithmetic, constants,
//       single-input φ copies, and field accesses on confined allocations of
//       this graph. Nothing else.
//
// # JMM argument
//
// Coarsening only ever *adds* synchronization: the gap `B` acquires an
// enclosing lock region it did not have. Adding a lock region can only add
// `hb` edges, and adding `hb` edges can only *remove* legal executions — every
// execution of the coarsened program is an execution of the original. So no
// new observable behaviour is introduced by the memory model itself.
//
// The two things adding synchronization *can* break are not memory-model
// facts, and each is closed by a precondition:
//
//   * **Liveness.** Extending a critical section can starve or deadlock a
//     thread that wanted the monitor during the gap. C1/E2 make that
//     impossible: no other thread can reach the object, so no thread can ever
//     block on m.
//   * **Observing the unlocked state.** The JVM offers exactly three ways to
//     see that m is free: `wait`/`notify` on m (refused by E3),
//     `Thread.holdsLock(m)` (an `Op::Call` — not on the C4 allowlist), and
//     another lock/unlock of m (a monitor op — not on the C4 allowlist).
//
// The removed `monitorexit; monitorenter` pair is itself a release/acquire
// pair on m, and by the elision argument above it pairs with nothing.
//
// ## Exceptions
//
// The requirement is *exact* unlock-on-throw behaviour, and the proof is by
// exclusion rather than by reasoning about unwinding: C4's allowlist admits no
// node that can raise a throwable. `Op::Call`, `Op::New`, `Op::NewArray`,
// `Op::ArrayLength`, `Op::Throw` and `Op::Other` are all refused, and the only
// memory accesses admitted are on allocations of this graph, which are
// non-null by construction and so cannot raise `NullPointerException`. A gap
// that cannot throw cannot unwind a monitor, so the unwind behaviour of the
// coarsened program is trivially identical to the original's.
//
// This is deliberately stronger than the reasoning a `finally`-based argument
// would need. It has to be: `ir::IrBuilder::build` does not compile handler
// bodies at all (the "STUB-S8" skip), an exception makes the compiled body
// return the `i64::MIN` sentinel, and whether the sentinel path unwinds a
// monitor the *compiled* frame holds is a runtime property this module cannot
// prove. Refusing to coarsen across anything that can throw means we never
// have to.
//
// ## Deopt relocking
//
// If a deopt lands in the gap, the interpreter frame must be reconstructed
// with the monitor set the *original* program held there — which does NOT
// include m, because the original had already run `first.exit`. The coarsened
// compiled frame DOES hold m. `deopt::MonitorInfo { object, lock_depth }` can
// describe holding m, but the reconstruction would still be wrong: the
// interpreter resumes at a gap bci and goes on to execute the original
// `second.enter`, reaching depth 2 with only one `monitorexit` left to run, so
// m is never fully released.
//
// That reconstruction is not merely unproven, it is provably wrong, so
// coarsening **refuses** rather than tries to repair it: `Op::Safepoint` is not
// on the C4 allowlist ([`LockRefusal::SafepointInGap`]). Safepoints *inside*
// either region are unaffected — the monitor set there is identical before and
// after, because coarsening changes the held-monitor set only in the gap.

/// One properly-nested `monitorenter`/`monitorexit` pair on a single object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockRegion {
    /// The `Op::MonitorEnter` node.
    pub enter: NodeId,
    /// The `Op::MonitorExit` node that matches it.
    pub exit: NodeId,
    /// Reentry depth of `enter`: `0` for an outermost region on this object,
    /// `1` for the first recursive re-entry inside it, and so on.
    pub depth: u32,
}

/// Why a lock transform was refused.
///
/// Recorded rather than dropped so a refusal is *reportable*: the whole point
/// of this module's identity machinery is that a fail-closed answer should be
/// intentional and measurable rather than incidental.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockRefusal {
    /// A live monitor-family node's operand is `OperandOrigin::Unknown` — it
    /// neither provably names one allocation of this graph nor provably names
    /// something outside it.
    ///
    /// This refuses **every** lock plan in the graph, not just one object's.
    /// An unattributable monitor locks *something* at run time; if that
    /// something is an object whose other monitors we did elide, the elision
    /// leaves an unbalanced sequence. There is no way to tell which object it
    /// is, so no object is safe. The accompanying `NodeId` is the offending
    /// **monitor node**, not an object — the one case where it is not an
    /// allocation.
    ///
    /// A monitor on a *caller-supplied* reference (`Op::Param`, or a φ of
    /// them) is NOT this: it provably locks something no allocation of this
    /// frame can be, so `synchronized (local) {} … this.wait();` still
    /// optimises the local.
    AmbiguousMonitorOperand,
    /// The object may be reachable from another thread (`may_escape`).
    ObjectEscapes,
    /// `Object.wait`/`notify`/`notifyAll` is performed on this monitor. The
    /// monitor must actually be held for those to work at all.
    WaitOrNotify,
    /// The object's monitor operations are not a balanced, properly-nested
    /// sequence in program order.
    UnbalancedMonitors,
    /// Program order is not a sound stand-in for execution order in this graph
    /// ([`program_order_proves_dominance`]), so "adjacent regions" and
    /// "between two regions" are not answerable.
    NoDominanceProof,
    /// A node between two regions can raise a throwable, so coarsening cannot
    /// be shown to preserve unlock-on-throw behaviour.
    MayThrowInGap,
    /// A safepoint / deopt point lies between two regions. The reconstructed
    /// interpreter frame would disagree with the compiled frame about whether
    /// the monitor is held.
    SafepointInGap,
    /// A node between two regions can observe the unlocked state, or is an
    /// access this analysis cannot prove is confined and non-faulting.
    ObservableGap,
}

/// A complete, **all-or-nothing** lock-elision offer for one object.
///
/// [`EscapeAnalysisResult::elide_locks`] is the flattened union of these
/// plans, kept for the existing consumer. **The flat list is only sound when
/// applied a whole plan at a time** — see the partial-application note on
/// [`EscapeAnalysisResult::lock_elisions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockElisionPlan {
    /// The allocation whose monitor is elided.
    pub object: NodeId,
    /// EVERY live `Op::MonitorEnter`/`Op::MonitorExit` naming `object`,
    /// ascending by node id. Applying a strict subset is unsound.
    pub monitors: Vec<NodeId>,
}

/// Merging two adjacent outermost lock regions on one object.
///
/// Applying it deletes [`Self::removed_exit`] and [`Self::removed_enter`] and
/// nothing else, which is why it is safe under partial application of *lock
/// elision*: the surviving structure is a single balanced region either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockCoarseningPlan {
    /// The allocation being locked.
    pub object: NodeId,
    /// The earlier region. Its `enter` survives.
    pub first: LockRegion,
    /// The later region. Its `exit` survives.
    pub second: LockRegion,
}

impl LockCoarseningPlan {
    /// The `monitorexit` this plan deletes.
    pub fn removed_exit(&self) -> NodeId {
        self.first.exit
    }

    /// The `monitorenter` this plan deletes.
    pub fn removed_enter(&self) -> NodeId {
        self.second.enter
    }

    /// The `monitorenter` that becomes the merged region's entry.
    pub fn surviving_enter(&self) -> NodeId {
        self.first.enter
    }

    /// The `monitorexit` that becomes the merged region's exit.
    pub fn surviving_exit(&self) -> NodeId {
        self.second.exit
    }
}

/// True for the four monitor operations. `wait`/`notify` are monitor
/// operations: they read the header and require the monitor to be held.
fn is_monitor_family(op: &Op) -> bool {
    matches!(
        op,
        Op::MonitorEnter | Op::MonitorExit | Op::MonitorWait | Op::MonitorNotify
    )
}

/// True when `op` provably produces no reference value at all, so a φ input of
/// this shape cannot carry an object.
///
/// The complement — `New`, `NewArray`, `Phi`, `Load`, `Param`, `Call` and
/// **`Other`** — is what `ref_operand_origin` has to account for. `Op::Other`
/// is deliberately on the *may be a reference* side even though
/// [`is_ref_producer`] excludes it: that heuristic is allowed to be optimistic
/// because its consumers only ever *add* escape, whereas here an unexamined φ
/// input is the difference between eliding this object's monitor and eliding a
/// monitor some other thread contends on.
fn produces_no_reference(op: &Op) -> bool {
    matches!(
        op,
        Op::Start
            | Op::Return
            | Op::If
            | Op::Merge
            // NOT `Op::Const`: the bridge maps the `aconst_null` literal to a
            // `Const(0)`, and `synchronized (c ? new Object() : null)` merged
            // that with the allocation — treating the null as "no reference"
            // let the lock be elided and dropped the NPE on the null path.
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Store(_)
            | Op::MonitorEnter
            | Op::MonitorExit
            | Op::MonitorWait
            | Op::MonitorNotify
            | Op::Safepoint
            | Op::Throw
            | Op::ArrayLength
            | Op::RefCompare
            | Op::IdentityHash
            | Op::Dead
    )
}

/// What a reference operand provably names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperandOrigin {
    /// Provably this allocation of this graph, on every path.
    Allocation(NodeId),
    /// Provably **not** any allocation of this graph: every path supplies a
    /// caller-provided reference (`Op::Param`), and an allocation this frame
    /// never published is by definition unreachable from the caller.
    ///
    /// This distinction is what keeps `synchronized (local) {} … this.wait();`
    /// optimisable: the `wait` names a reference that cannot be `local`.
    Foreign,
    /// Anything else. Not "some other object" — *unknown*.
    Unknown,
}

/// What a *reference operand* provably names.
///
/// "Provably" is deliberately narrow: the walk follows [`Op::Phi`] copies and
/// stops at an allocation or a parameter. A `Call` result, a field `Load`, an
/// `Op::Other` or a φ mixing the two leaf kinds all answer
/// `OperandOrigin::Unknown`.
///
/// # Why not `ConnectionGraph::resolve_points_to`
///
/// Because a singleton points-to set is *not* a proof of provenance, and this
/// is the same trap `find_scalar_replacements` documents at its φ arm: a
/// reference input with unknown provenance (a `Param`, a `Call` result, a
/// field `Load`) contributes **no** entry to the points-to set, so a φ merging
/// this allocation with such an input still resolves to the singleton
/// `{alloc}` while genuinely carrying the other object on one path.
///
/// For a lock that is a fatal difference. `monitorenter` on such a φ would be
/// attributed to the confined allocation and elided, while at run time it
/// locks the *other* — escaping — object, and its matching `monitorexit`
/// disappears with it. This walk cannot make that mistake: an input it cannot
/// name is an immediate `Unknown`.
fn ref_operand_origin(graph: &Graph, operand: NodeId) -> OperandOrigin {
    let mut work = vec![operand];
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut alloc: Option<NodeId> = None;
    let mut saw_param = false;

    while let Some(n) = work.pop() {
        if !seen.insert(n) {
            continue;
        }
        let node = match graph.nodes.get(n) {
            Some(node) => node,
            None => return OperandOrigin::Unknown,
        };
        match &node.op {
            Op::New { .. } | Op::NewArray { .. } => match alloc {
                None => alloc = Some(n),
                // Two *different* allocations reach this operand: it does not
                // name one object, so it names none we can act on.
                Some(l) if l != n => return OperandOrigin::Unknown,
                Some(_) => {}
            },
            Op::Param(_) => saw_param = true,
            Op::Phi => {
                let mut any_ref = false;
                for &inp in &node.inputs {
                    let inp_op = match graph.nodes.get(inp) {
                        Some(x) => &x.op,
                        None => return OperandOrigin::Unknown,
                    };
                    // Only a *provably* value-less input may be skipped — the
                    // Merge control edge, a primitive. Everything else is
                    // walked, and the walk answers `Unknown` for anything it
                    // cannot name.
                    if produces_no_reference(inp_op) {
                        continue;
                    }
                    any_ref = true;
                    work.push(inp);
                }
                // A φ with no reference input cannot be a reference at all;
                // treat the operand as unnameable rather than assume.
                if !any_ref {
                    return OperandOrigin::Unknown;
                }
            }
            _ => return OperandOrigin::Unknown,
        }
    }

    match (alloc, saw_param) {
        // A φ that merges a local allocation with a parameter is neither: on
        // one path it IS the allocation, so it cannot be dismissed as foreign,
        // and on the other it is not, so it cannot be attributed.
        (Some(_), true) => OperandOrigin::Unknown,
        (Some(a), false) => OperandOrigin::Allocation(a),
        (None, true) => OperandOrigin::Foreign,
        (None, false) => OperandOrigin::Unknown,
    }
}

/// The allocation a reference operand provably names, or `None`.
fn ref_operand_allocation(graph: &Graph, operand: NodeId) -> Option<NodeId> {
    match ref_operand_origin(graph, operand) {
        OperandOrigin::Allocation(a) => Some(a),
        _ => None,
    }
}

/// The allocation whose monitor `monitor` operates on, or `None` when the
/// operand is foreign or its provenance cannot be proved.
///
/// A `None` on an *unknown* operand is not a local refusal — see
/// [`LockRefusal::AmbiguousMonitorOperand`]. A `None` on a *foreign* operand
/// is simply "not one of ours", and is harmless.
pub fn monitor_object(graph: &Graph, monitor: NodeId) -> Option<NodeId> {
    let node = graph.nodes.get(monitor)?;
    if !is_monitor_family(&node.op) {
        return None;
    }
    ref_operand_allocation(graph, *node.inputs.first()?)
}

/// Every live monitor-family node in the graph, ascending.
fn live_monitor_nodes(graph: &Graph) -> Vec<NodeId> {
    graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| is_monitor_family(&n.op))
        .map(|(id, _)| id)
        .collect()
}

/// The monitor nodes whose object is `OperandOrigin::Unknown`.
///
/// Non-empty means **no** lock transform may be applied anywhere in the
/// method; see [`LockRefusal::AmbiguousMonitorOperand`]. A monitor on a
/// *foreign* reference is not listed — it provably locks something no
/// transform here touches.
fn unattributable_monitors(graph: &Graph) -> Vec<NodeId> {
    live_monitor_nodes(graph)
        .into_iter()
        .filter(|&m| {
            graph.nodes[m].inputs.first().map_or(true, |&operand| {
                ref_operand_origin(graph, operand) == OperandOrigin::Unknown
            })
        })
        .collect()
}

/// True when a live `Object.wait`/`notify`/`notifyAll` names `object`.
///
/// Exact rather than conservative, because it is only ever called after the
/// `unattributable_monitors` guard has already rejected the graph if any
/// monitor-family operand was `OperandOrigin::Unknown`. What is left is
/// provably this allocation or provably foreign.
fn waits_or_notifies_on(graph: &Graph, object: NodeId) -> bool {
    graph.nodes.iter().enumerate().any(|(id, n)| {
        matches!(n.op, Op::MonitorWait | Op::MonitorNotify)
            && monitor_object(graph, id) == Some(object)
    })
}

/// The `monitorenter`/`monitorexit` regions of `object`, in program order.
///
/// `Err` is the fail-closed answer and carries the reason. Pairing enters with
/// exits is only meaningful when program order is execution order, so this
/// requires [`program_order_proves_dominance`] — unlike *elision*, which
/// removes every monitor on the object and therefore needs no ordering at all.
pub fn lock_regions(graph: &Graph, object: NodeId) -> Result<Vec<LockRegion>, LockRefusal> {
    if !program_order_proves_dominance(graph) {
        return Err(LockRefusal::NoDominanceProof);
    }

    let mut open: Vec<(NodeId, u32)> = Vec::new();
    let mut regions: Vec<LockRegion> = Vec::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.op, Op::MonitorEnter | Op::MonitorExit) {
            continue;
        }
        if monitor_object(graph, id) != Some(object) {
            continue;
        }
        match node.op {
            Op::MonitorEnter => {
                let depth = open.len() as u32;
                open.push((id, depth));
            }
            Op::MonitorExit => match open.pop() {
                Some((enter, depth)) => regions.push(LockRegion {
                    enter,
                    exit: id,
                    depth,
                }),
                // An exit with no matching enter: the sequence is not a
                // sequence we understand.
                None => return Err(LockRefusal::UnbalancedMonitors),
            },
            _ => unreachable!("filtered above"),
        }
    }

    if !open.is_empty() {
        return Err(LockRefusal::UnbalancedMonitors);
    }

    regions.sort_unstable_by_key(|r| r.enter);
    Ok(regions)
}

/// Whether `id` may appear in a coarsening gap, and why not if it may not.
///
/// The allowlist is the whole safety argument for (b); see the section comment
/// above. It is an **allowlist**, not a denylist: an op nobody has thought
/// about is refused, and adding an `Op` variant without an arm here is a
/// compile error rather than a silent admission.
fn gap_node_refusal(cg: &ConnectionGraph, graph: &Graph, id: NodeId) -> Option<LockRefusal> {
    let node = match graph.nodes.get(id) {
        Some(n) => n,
        None => return Some(LockRefusal::ObservableGap),
    };
    match &node.op {
        // Removed by an earlier pass: observes nothing, runs nothing.
        Op::Dead => None,
        // Pure, total, no safepoint, no throw.
        Op::Start | Op::Const(_) | Op::Param(_) | Op::Add | Op::Sub | Op::Mul => None,
        // A single-input φ is a degenerate copy. A multi-input φ cannot occur
        // (C2 already required `program_order_proves_dominance`), but if one
        // does, it is a join and the gap is not a straight line.
        Op::Phi => {
            if node.inputs.len() <= 1 {
                None
            } else {
                Some(LockRefusal::ObservableGap)
            }
        }
        // Field access. Admitted only when the holder is provably an
        // allocation of THIS graph that does not escape:
        //   * non-null by construction, so it cannot raise NPE — which is what
        //     makes the "the gap cannot throw" claim hold;
        //   * confined, so no other thread can observe the access and no
        //     other thread's view of the gap changes.
        // A holder that is a Param, a call result or an unresolvable reference
        // fails both halves and is refused.
        Op::Load(_) | Op::Store(_) => {
            let holder = match node.op {
                Op::Load(_) => load_holder(node),
                _ => store_holder(node),
            };
            match holder.and_then(|h| ref_operand_allocation(graph, h)) {
                Some(a)
                    if matches!(graph.nodes.get(a).map(|n| &n.op), Some(Op::New { .. }))
                        && cg.get_escape(a).is_confined() =>
                {
                    None
                }
                _ => Some(LockRefusal::ObservableGap),
            }
        }
        // A deopt point whose recorded monitor set would be wrong.
        Op::Safepoint => Some(LockRefusal::SafepointInGap),
        // Can raise a throwable, and (for Call/New/NewArray) is a safepoint.
        Op::Call | Op::New { .. } | Op::NewArray { .. } | Op::ArrayLength | Op::Throw => {
            Some(LockRefusal::MayThrowInGap)
        }
        // Observes the lock state or the object's identity.
        Op::MonitorEnter
        | Op::MonitorExit
        | Op::MonitorWait
        | Op::MonitorNotify
        | Op::RefCompare
        | Op::IdentityHash => Some(LockRefusal::ObservableGap),
        // Control flow inside the gap contradicts the dominance proof, and
        // `Op::Other` is by definition unmodelled.
        Op::If | Op::Merge | Op::Return | Op::Other => Some(LockRefusal::ObservableGap),
    }
}

/// Lock-elision offers, one per object, plus the refusals.
///
/// Sound on its own terms: no other thread can reach a `NoEscape` object, so
/// every action on its monitor is this thread's and the whole balanced set is
/// a no-op. See the section comment above for the JMM argument.
///
/// **This offer does not license scalar replacement of the locked object.** A
/// live monitor is an identity observation, and the consumer may refuse the
/// elision for reasons this module cannot see (`apply_ea_to_ir` refuses one
/// whose safepoint slot or memory chain it cannot repair). The object is
/// therefore excluded from `scalar_replaceable` while the monitor is live —
/// see [`find_identity_observations`].
fn find_lock_elision_plans(
    cg: &ConnectionGraph,
    graph: &Graph,
) -> (Vec<LockElisionPlan>, Vec<(NodeId, LockRefusal)>) {
    let mut refusals: Vec<(NodeId, LockRefusal)> = Vec::new();

    // E1, and it is global. One monitor we cannot attribute poisons the whole
    // method: eliding any object's monitors could leave that one unbalanced,
    // and we cannot tell which object it locks.
    let unattributable = unattributable_monitors(graph);
    if !unattributable.is_empty() {
        for m in unattributable {
            refusals.push((m, LockRefusal::AmbiguousMonitorOperand));
        }
        return (Vec::new(), refusals);
    }

    // Group the attributable enter/exit nodes by object, ascending.
    let mut by_object: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for m in live_monitor_nodes(graph) {
        if !matches!(graph.nodes[m].op, Op::MonitorEnter | Op::MonitorExit) {
            continue;
        }
        if let Some(obj) = monitor_object(graph, m) {
            by_object.entry(obj).or_default().push(m);
        }
    }

    let mut objects: Vec<NodeId> = by_object.keys().copied().collect();
    objects.sort_unstable();

    let mut plans = Vec::new();
    for object in objects {
        let mut monitors = by_object.remove(&object).unwrap_or_default();
        monitors.sort_unstable();

        // E2.
        if cg.get_escape(object).may_escape() {
            refusals.push((object, LockRefusal::ObjectEscapes));
            continue;
        }
        // E3.
        if waits_or_notifies_on(graph, object) {
            refusals.push((object, LockRefusal::WaitOrNotify));
            continue;
        }
        // E4 is structural: `monitors` is every enter/exit on the object, and
        // `LockElisionPlan` is documented all-or-nothing. Removing *all* of a
        // balanced sequence is balance-preserving on every path, which is why
        // elision — unlike coarsening — needs no ordering proof.
        plans.push(LockElisionPlan { object, monitors });
    }

    (plans, refusals)
}

/// Backwards-compatible flat view of `find_lock_elision_plans`: every monitor
/// node of every complete plan, ascending.
///
/// Kept because it is the shape [`EscapeAnalysisResult::elide_locks`] has and
/// the shape `apply_ea_to_ir` reads. It loses the grouping, and the grouping is
/// load-bearing — see [`EscapeAnalysisResult::lock_elisions`].
pub fn find_lock_elisions(cg: &ConnectionGraph, graph: &Graph) -> Vec<NodeId> {
    let (plans, _) = find_lock_elision_plans(cg, graph);
    let mut nodes: Vec<NodeId> = plans.into_iter().flat_map(|p| p.monitors).collect();
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}

/// Lock-coarsening offers, plus the refusals.
///
/// Each plan merges two *adjacent outermost* regions on the same object by
/// deleting the inner `monitorexit`/`monitorenter` pair. See the section
/// comment above for the JMM, exception and deopt arguments.
///
/// # Independence from lock elision
///
/// A plan names only its own two victims and is correct whether or not any
/// elision was applied — which is the answer to the partial-application
/// hazard: coarsening depends on no elision landing.
///
/// [`apply_lock_coarsening`] re-verifies its four nodes before mutating, so
/// applying any subset of the offered plans, in any order, leaves a balanced
/// monitor structure. Chained plans (three adjacent regions ⇒ two plans) share
/// a region, so the second one applied is *refused* rather than compounded;
/// re-running the analysis on the mutated graph offers the now-adjacent pair,
/// the same iterate-to-fixpoint story `find_identity_observations` documents
/// for synchronized objects.
fn find_lock_coarsening_plans(
    cg: &ConnectionGraph,
    graph: &Graph,
) -> (Vec<LockCoarseningPlan>, Vec<(NodeId, LockRefusal)>) {
    let mut refusals: Vec<(NodeId, LockRefusal)> = Vec::new();

    // C1 (attributability), global for the same reason as in elision: an
    // unattributable monitor could be an inner region on the object we are
    // about to coarsen, and then "adjacent" is a lie.
    let unattributable = unattributable_monitors(graph);
    if !unattributable.is_empty() {
        for m in unattributable {
            refusals.push((m, LockRefusal::AmbiguousMonitorOperand));
        }
        return (Vec::new(), refusals);
    }

    let mut objects: Vec<NodeId> = live_monitor_nodes(graph)
        .into_iter()
        .filter_map(|m| monitor_object(graph, m))
        .collect();
    objects.sort_unstable();
    objects.dedup();

    let mut plans = Vec::new();
    for object in objects {
        // C1: confined.
        if cg.get_escape(object).may_escape() {
            refusals.push((object, LockRefusal::ObjectEscapes));
            continue;
        }
        // C1: no wait/notify.
        if waits_or_notifies_on(graph, object) {
            refusals.push((object, LockRefusal::WaitOrNotify));
            continue;
        }
        // C2 + C3.
        let regions = match lock_regions(graph, object) {
            Ok(r) => r,
            Err(reason) => {
                refusals.push((object, reason));
                continue;
            }
        };
        let outer: Vec<LockRegion> = regions.into_iter().filter(|r| r.depth == 0).collect();

        // C4, pairwise over adjacent outermost regions.
        for pair in outer.windows(2) {
            let (first, second) = (pair[0], pair[1]);
            let mut refusal = None;
            for id in (first.exit + 1)..second.enter {
                if let Some(r) = gap_node_refusal(cg, graph, id) {
                    refusal = Some(r);
                    break;
                }
            }
            match refusal {
                Some(r) => refusals.push((object, r)),
                None => plans.push(LockCoarseningPlan {
                    object,
                    first,
                    second,
                }),
            }
        }
    }

    (plans, refusals)
}

// ── Partial escape analysis helpers ─────────────────────────────────────

/// Check if an object escapes on only some control flow paths.
///
/// This is the *structural* heuristic — "some direct use escapes, some does
/// not" — and it says nothing about how often the escaping path runs. It is
/// kept because [`find_materialization_points`] is written against it.
///
/// The lattice-level answer is [`EscapeState::PartialEscape`], produced by
/// [`analyze_escapes`] from [`Graph::cold_nodes`]. The two differ deliberately:
/// this one is true for `new Foo(); o.x = 1; f(o);` (a local use and an
/// escaping use) even though the object escapes on *every* execution, whereas
/// the lattice classification requires the escape site itself to be cold. Only
/// the latter is a basis for sinking an allocation.
pub fn is_partial_escape(cg: &ConnectionGraph, graph: &Graph, alloc: NodeId) -> bool {
    let state = cg.get_escape(alloc);
    if state == EscapeState::NoEscape {
        return false; // fully non-escaping, not partial
    }

    // Check if some uses escape and others don't.
    if alloc >= graph.nodes.len() {
        return false;
    }
    let node = &graph.nodes[alloc];
    let mut has_escaping_use = false;
    let mut has_local_use = false;

    for &use_id in &node.uses {
        if use_id >= graph.nodes.len() {
            continue;
        }
        match &graph.nodes[use_id].op {
            // cov-07: a thrown reference escapes exactly like a returned one.
            Op::Return | Op::Throw | Op::Call => has_escaping_use = true,
            Op::Store(_) | Op::Load(_) | Op::MonitorEnter | Op::MonitorExit => has_local_use = true,
            Op::Phi => {
                // Phi merges paths -- check if any successor escapes.
                if cg.get_escape(use_id) >= EscapeState::ArgEscape {
                    has_escaping_use = true;
                } else {
                    has_local_use = true;
                }
            }
            _ => {}
        }
    }

    has_escaping_use && has_local_use
}

/// Find the points where a partially-escaping object must be materialized.
pub fn find_materialization_points(
    cg: &ConnectionGraph,
    graph: &Graph,
    alloc: NodeId,
) -> Vec<MaterializationPoint> {
    let mut points = Vec::new();

    if alloc >= graph.nodes.len() {
        return points;
    }
    let node = &graph.nodes[alloc];
    let num_fields = match &node.op {
        Op::New { num_fields, .. } => *num_fields,
        Op::NewArray { .. } => 0,
        _ => return points,
    };

    for &use_id in &node.uses {
        if use_id >= graph.nodes.len() {
            continue;
        }
        let is_escape_point = matches!(graph.nodes[use_id].op, Op::Return | Op::Throw | Op::Call);
        if !is_escape_point {
            if let Op::Phi = &graph.nodes[use_id].op {
                if cg.get_escape(use_id) >= EscapeState::ArgEscape {
                    // Phi that leads to an escape.
                } else {
                    continue;
                }
            } else {
                continue;
            }
        }

        // Collect field values known before this escape point.
        let mut fields_at_escape: Vec<Option<NodeId>> = vec![None; num_fields];
        for &other_use in &node.uses {
            if other_use >= graph.nodes.len() || other_use == use_id {
                continue;
            }
            let other_node = &graph.nodes[other_use];
            if let Op::Store(field_idx) = &other_node.op {
                // Only stores whose HOLDER is this allocation describe its
                // field state; a store that merely uses this alloc as a value
                // is irrelevant here.
                if store_holder(other_node) == Some(alloc) && *field_idx < num_fields {
                    if let Some(stored_val) = store_value(other_node) {
                        fields_at_escape[*field_idx] = Some(stored_val);
                    }
                }
            }
        }

        points.push(MaterializationPoint {
            alloc_node: alloc,
            escape_point: use_id,
            fields_at_escape,
        });
    }

    points
}

/// Every live node at which `alloc` becomes visible outside the allocating
/// frame.
///
/// This is the *attribution* half of partial escape: [`EscapeState`] says an
/// object escapes, this says **where**. An empty result for an escaping object
/// means the escape could not be attributed to a site (it came from the
/// fail-closed [`escalate_all_to_global`] path, or through an edge shape this
/// walk does not model), and the caller must then keep the unrefined state.
fn escape_sites_for(cg: &ConnectionGraph, graph: &Graph, alloc: NodeId) -> Vec<NodeId> {
    let mut sites = Vec::new();
    // Does `r` (a node used as a reference) carry `alloc`?
    let carries = |cg: &ConnectionGraph, r: NodeId| -> bool {
        r == alloc || cg.resolve_points_to(r).contains(&alloc)
    };

    for (id, node) in graph.nodes.iter().enumerate() {
        match &node.op {
            Op::Dead => {}
            // Returning, throwing or passing the reference publishes it. So
            // does handing it to a node this module cannot model: `Op::Other`
            // is the bridge's catch-all, and `build_connection_graph` escapes
            // its reference operands for the reasons recorded there. Naming the
            // site here keeps the attribution and the lattice agreeing — an
            // unattributed escape would otherwise look like the fail-closed
            // `escalate_all_to_global` path.
            Op::Return | Op::Throw | Op::Call | Op::Other => {
                if node
                    .inputs
                    .iter()
                    .any(|&inp| is_ref_producer(graph, inp) && carries(cg, inp))
                {
                    sites.push(id);
                }
            }
            // Writing the reference into a field publishes it exactly when the
            // holder is itself reachable from outside — the same rule the
            // propagation uses, including "an unnameable holder is the global
            // heap".
            Op::Store(_) => {
                let (obj, val) = match (store_holder(node), store_value(node)) {
                    (Some(o), Some(v)) => (o, v),
                    // A malformed store is an unattributable publication of its
                    // value; propagation escalates it to GlobalEscape, so name
                    // it as a site rather than pretend it is not one.
                    (None, Some(v)) if carries(cg, v) => {
                        sites.push(id);
                        continue;
                    }
                    _ => continue,
                };
                if !carries(cg, val) {
                    continue;
                }
                let obj_pts = cg.resolve_points_to(obj);
                let mut holder_escape = cg.get_escape(obj);
                for &a in &obj_pts {
                    holder_escape = holder_escape.join(cg.get_escape(a));
                }
                if obj_pts.is_empty() || holder_escape.may_escape() {
                    sites.push(id);
                }
            }
            _ => {}
        }
    }

    sites
}

// ── Main entry point ────────────────────────────────────────────────────

/// Run escape analysis on the IR graph.
pub fn analyze_escapes(graph: &Graph) -> EscapeAnalysisResult {
    let mut cg = build_connection_graph(graph);
    propagate_escape_states(&mut cg, graph);

    let (scalar_replaceable, scalar_refusals) = find_scalar_replacements(&cg, graph);
    let (lock_elisions, mut lock_refusals) = find_lock_elision_plans(&cg, graph);
    let (lock_coarsening, coarsening_refusals) = find_lock_coarsening_plans(&cg, graph);
    lock_refusals.extend(coarsening_refusals);
    lock_refusals.sort_unstable();
    lock_refusals.dedup();
    // The flat view the existing consumer reads. Derived from the plans rather
    // than recomputed, so the two can never disagree about which monitors are
    // offered.
    let mut elide_lock_nodes: Vec<NodeId> = lock_elisions
        .iter()
        .flat_map(|p| p.monitors.iter().copied())
        .collect();
    elide_lock_nodes.sort_unstable();
    elide_lock_nodes.dedup();
    let identity_observations = find_identity_observations(&cg, graph);
    // Allocations whose identity a LIVE node observes. Used only to attribute
    // the "would have been replaced but for its identity" statistic — the
    // refusal itself already happened inside `find_scalar_replacements`.
    let identity_live: HashSet<NodeId> = identity_observations
        .iter()
        .filter(|&&(_, observer)| {
            !matches!(graph.nodes.get(observer).map(|n| &n.op), Some(Op::Dead))
        })
        .map(|&(alloc, _)| alloc)
        .collect();

    // Classify allocations.
    let mut stats = EscapeAnalysisStats::default();
    let mut escape_states = HashMap::new();
    let mut stack_allocatable = Vec::new();
    let mut partial_escapes = Vec::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::New { .. } | Op::NewArray { .. }) {
            stats.total_allocations += 1;
            let raw = cg.get_escape(id);
            if raw.is_confined() && identity_live.contains(&id) {
                stats.identity_blocked += 1;
            }

            // COLD-PATH REFINEMENT (reported only — see `EscapeState`).
            // An escaping object whose every escape site is cold is reported
            // `PartialEscape`. Both conditions fail closed: an unattributable
            // escape (empty site list) and any hot site keep the raw state.
            let mut state = raw;
            if raw.may_escape() {
                let sites = escape_sites_for(&cg, graph, id);
                if !sites.is_empty() && sites.iter().all(|s| graph.cold_nodes.contains(s)) {
                    partial_escapes.push(PartialEscapeInfo {
                        alloc_node: id,
                        without_refinement: raw,
                        escape_sites: sites,
                    });
                    state = EscapeState::PartialEscape;
                }
            }
            escape_states.insert(id, state);

            match state {
                EscapeState::NoEscape => stats.no_escape += 1,
                EscapeState::PartialEscape => stats.partial_escape += 1,
                EscapeState::ArgEscape => {
                    stats.arg_escape += 1;
                    stack_allocatable.push(id);
                }
                EscapeState::GlobalEscape => stats.global_escape += 1,
            }
        }
    }

    stats.scalar_replaced = scalar_replaceable.len();
    stats.stack_allocated = stack_allocatable.len();
    stats.locks_elided = elide_lock_nodes.len();
    stats.lock_objects_elided = lock_elisions.len();
    stats.locks_coarsened = lock_coarsening.len();
    stats.locks_refused = lock_refusals.len();

    EscapeAnalysisResult {
        escape_states,
        scalar_replaceable,
        stack_allocatable,
        elide_locks: elide_lock_nodes,
        lock_elisions,
        lock_coarsening,
        lock_refusals,
        scalar_refusals,
        partial_escapes,
        identity_observations,
        stats,
    }
}

// ── Graph transformations ───────────────────────────────────────────────

/// Apply scalar replacement: replace allocation + load/store with direct
/// value flow.  Marks eliminated nodes as `Op::Dead`.
///
/// # Which value each load gets
///
/// [`ScalarReplacementInfo::load_values`], **not** `field_values`. This
/// function used to forward every load of field `f` to `field_values[f]`, the
/// value of the *last* store to `f` anywhere in the method, which is a value
/// from the load's future whenever a store to the same field follows it. That
/// is the `Foo o = new Foo(); int a = o.x; o.x = 42;` ⇒ `a == 42`
/// miscompilation, in this module rather than in `apply_ea_to_ir`.
///
/// # All-or-nothing
///
/// If any load cannot be resolved the graph is left **completely untouched**.
/// A half-applied object — allocation killed, an unanswerable load still
/// reading it — is worse than no optimisation.
pub fn apply_scalar_replacement(graph: &mut Graph, info: &ScalarReplacementInfo) {
    // The strongest single piece of evidence the optimizing tier can produce:
    // an allocation that no longer happens. See `ir_evidence`.
    crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarReplacement);
    // Refuse before mutating anything. `find_scalar_replacements` already
    // guarantees no `Unknown` survives into a reported candidate, so this is the
    // belt-and-braces check for a hand-built or future-produced info.
    if info
        .replaced_loads
        .iter()
        .any(|&l| info.load_value(l) == LoadResolution::Unknown)
    {
        return;
    }

    // Replace loads with the value that load actually reads.
    let mut zero_default: Option<NodeId> = None;
    for &load_id in &info.replaced_loads {
        if load_id >= graph.nodes.len() {
            continue;
        }
        // A never-written field reads the allocation's zero value; materialise
        // ONE shared `Const(0)` for all of them.
        let val = match info.load_value(load_id) {
            LoadResolution::Value(v) => Some(v),
            LoadResolution::ZeroDefault => Some(match zero_default {
                Some(z) => z,
                None => {
                    let z = graph.add_node(Op::Const(0), vec![]);
                    zero_default = Some(z);
                    z
                }
            }),
            // Unreachable: the guard above returned. Keep the load alive rather
            // than kill it with no replacement.
            LoadResolution::Unknown => None,
        };
        let val = match val {
            Some(v) if v < graph.nodes.len() => v,
            _ => continue,
        };
        // Redirect all uses of this load to that value.
        let load_uses: Vec<NodeId> = graph.nodes[load_id].uses.clone();
        for &u in &load_uses {
            if u < graph.nodes.len() {
                for inp in graph.nodes[u].inputs.iter_mut() {
                    if *inp == load_id {
                        *inp = val;
                    }
                }
                graph.nodes[val].uses.push(u);
            }
        }
        graph.nodes[load_id].op = Op::Dead;
        graph.nodes[load_id].inputs.clear();
        graph.nodes[load_id].uses.clear();
    }

    // Mark stores as dead.
    for &store_id in &info.eliminated_stores {
        if store_id < graph.nodes.len() {
            graph.nodes[store_id].op = Op::Dead;
            graph.nodes[store_id].inputs.clear();
            graph.nodes[store_id].uses.clear();
        }
    }

    // Mark the allocation itself as dead.
    if info.alloc_node < graph.nodes.len() {
        graph.nodes[info.alloc_node].op = Op::Dead;
        graph.nodes[info.alloc_node].inputs.clear();
        graph.nodes[info.alloc_node].uses.clear();
    }
}

/// True when `id` is a live `Op::MonitorEnter` (`want_enter`) or a live
/// `Op::MonitorExit` (`!want_enter`).
fn is_monitor_kind(graph: &Graph, id: NodeId, want_enter: bool) -> bool {
    match graph.nodes.get(id).map(|n| &n.op) {
        Some(Op::MonitorEnter) => want_enter,
        Some(Op::MonitorExit) => !want_enter,
        _ => false,
    }
}

/// Kill one monitor node outright. Callers must have proved the *set* they are
/// killing is balance-preserving; this does no checking of its own.
fn kill_node(graph: &mut Graph, id: NodeId) {
    if id < graph.nodes.len() {
        graph.nodes[id].op = Op::Dead;
        graph.nodes[id].inputs.clear();
        graph.nodes[id].uses.clear();
    }
}

/// Eliminate locks on non-escaping objects by marking MonitorEnter/Exit as
/// dead — **all or nothing**.
///
/// # Why this validates instead of just killing
///
/// It used to kill whatever it was handed. A monitor sequence is only correct
/// as a whole: dropping one `monitorenter` out of a balanced pair leaves a
/// `monitorexit` on a monitor that was never entered
/// (`IllegalMonitorStateException`), and dropping one `monitorexit` leaks the
/// monitor past the end of the frame. Since the caller of record —
/// `apply_ea_to_ir` — filters [`EscapeAnalysisResult::elide_locks`] node by
/// node and can hand back a strict subset, "kill whatever I am given" is one
/// consumer-side refusal away from wrong code.
///
/// Accepted when, **per object**, either
///
/// * the request removes *every* live `monitorenter`/`monitorexit` on that
///   object — balance-preserving on every path, no ordering needed; or
/// * [`lock_regions`] can pair the object's monitors, and every region is
///   *wholly* in or *wholly* out of the request. This is the nested-region
///   case: removing the inner pair of `enter o; enter o; exit o; exit o` is
///   legal and useful. Removing `e2` and `x2` from that sequence is not — the
///   two are not a pair, and the result would silently *narrow* the outer
///   region rather than eliminate a level of reentry.
///
/// Anything else — including a monitor whose object cannot be attributed, or a
/// request that names a node that is not a monitor — leaves the graph
/// **completely untouched**. Returns whether the request was applied.
///
/// # What this does NOT check
///
/// Confinement. This function sees no [`ConnectionGraph`], so it validates the
/// monitor *structure* and trusts the caller for the escape state. Its caller
/// of record is [`apply_lock_elision_plan`], whose plans come from
/// `find_lock_elision_plans` and are confined by construction.
pub fn apply_lock_elision(graph: &mut Graph, lock_nodes: &[NodeId]) -> bool {
    let mut requested: Vec<NodeId> = lock_nodes
        .iter()
        .copied()
        .filter(|&id| {
            graph
                .nodes
                .get(id)
                .is_some_and(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
        })
        .collect();
    requested.sort_unstable();
    requested.dedup();
    if requested.is_empty() {
        // Nothing live to do (a request of only dead/absent nodes is vacuous,
        // not a violation).
        return true;
    }

    // Group both the request and the graph's monitors by object.
    let mut requested_by_object: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for &id in &requested {
        match monitor_object(graph, id) {
            Some(obj) => requested_by_object.entry(obj).or_default().push(id),
            // Unattributable: we cannot prove anything about the sequence it
            // belongs to. Fail closed for the whole request.
            None => return false,
        }
    }

    for (&object, removed) in requested_by_object.iter() {
        let all: Vec<NodeId> = (0..graph.nodes.len())
            .filter(|&id| matches!(graph.nodes[id].op, Op::MonitorEnter | Op::MonitorExit))
            .filter(|&id| monitor_object(graph, id) == Some(object))
            .collect();
        let removed_set: HashSet<NodeId> = removed.iter().copied().collect();
        if removed_set.len() == all.len() {
            // Whole-object elision: correct without any ordering proof.
            continue;
        }
        // Partial removal. Every region must go whole or stay whole.
        let regions = match lock_regions(graph, object) {
            Ok(r) => r,
            Err(_) => return false,
        };
        for region in &regions {
            if removed_set.contains(&region.enter) != removed_set.contains(&region.exit) {
                return false;
            }
        }
    }

    for id in requested {
        kill_node(graph, id);
    }
    true
}

/// Apply one complete [`LockElisionPlan`] atomically.
///
/// The plan already names every monitor on its object, so this is the
/// whole-object case of [`apply_lock_elision`] and cannot be partially applied
/// by construction. Returns whether it was applied — `false` when the graph has
/// drifted from the plan (a monitor is no longer live, or the object grew one
/// the plan does not name).
pub fn apply_lock_elision_plan(graph: &mut Graph, plan: &LockElisionPlan) -> bool {
    let still_complete = (0..graph.nodes.len())
        .filter(|&id| matches!(graph.nodes[id].op, Op::MonitorEnter | Op::MonitorExit))
        .filter(|&id| monitor_object(graph, id) == Some(plan.object))
        .all(|id| plan.monitors.contains(&id));
    if !still_complete {
        return false;
    }
    apply_lock_elision(graph, &plan.monitors)
}

/// Apply one [`LockCoarseningPlan`]: delete the inner `monitorexit` and
/// `monitorenter`, merging the two regions into one.
///
/// # Re-verification, and why it is not paranoia
///
/// The plan was computed on a graph that may since have been mutated — by lock
/// elision, by scalar replacement, or by a partially-applied version of either.
/// Node ids are stable and killing a node clears its inputs, so "all four
/// monitors are still live and still the right kind" is exactly the statement
/// that the structure this plan proved still exists. If it does not, the plan
/// is dropped whole:
///
/// * an elision that already removed the pair ⇒ no-op, not a double kill;
/// * an elision that removed the *outer* enter or exit ⇒ refused, so
///   coarsening never compounds an unbalanced sequence it did not create.
///
/// Returns whether it was applied.
pub fn apply_lock_coarsening(graph: &mut Graph, plan: &LockCoarseningPlan) -> bool {
    if !is_monitor_kind(graph, plan.surviving_enter(), true)
        || !is_monitor_kind(graph, plan.removed_exit(), false)
        || !is_monitor_kind(graph, plan.removed_enter(), true)
        || !is_monitor_kind(graph, plan.surviving_exit(), false)
    {
        return false;
    }
    // All four must still name the same object, or the "same monitor" premise
    // is gone.
    for id in [
        plan.surviving_enter(),
        plan.removed_exit(),
        plan.removed_enter(),
        plan.surviving_exit(),
    ] {
        if monitor_object(graph, id) != Some(plan.object) {
            return false;
        }
    }

    kill_node(graph, plan.removed_exit());
    kill_node(graph, plan.removed_enter());
    true
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a trivial graph with a local allocation that is only
    /// loaded/stored but never returned or passed to a call.
    fn graph_local_alloc() -> Graph {
        let mut g = Graph::new();
        // node 2: New { class_id: 1, num_fields: 2 }
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 2,
            },
            vec![],
        );
        // node 3: Const(42)
        let c42 = g.add_node(Op::Const(42), vec![]);
        // node 4: Store field 0 <- 42
        let _store = g.add_node(Op::Store(0), vec![alloc, c42]);
        // node 5: Load field 0
        let _load = g.add_node(Op::Load(0), vec![alloc]);
        g
    }

    // ---- Basic escape classification ----

    #[test]
    fn test_local_alloc_no_escape() {
        let g = graph_local_alloc();
        let result = analyze_escapes(&g);
        assert_eq!(result.escape_states.get(&2), Some(&EscapeState::NoEscape));
    }

    #[test]
    fn test_returned_alloc_global_escape() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        // Wire alloc as input to Return.
        g.nodes[1].inputs.push(alloc); // Return node
        g.nodes[alloc].uses.push(1);
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::GlobalEscape)
        );
    }

    #[test]
    fn test_call_arg_escape() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let _call = g.add_node(Op::Call, vec![alloc]);
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::ArgEscape)
        );
    }

    #[test]
    fn test_stored_to_escaping_object_global_escape() {
        let mut g = Graph::new();
        // Outer object escapes via return.
        let outer = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        g.nodes[1].inputs.push(outer);
        g.nodes[outer].uses.push(1);
        // Inner object is stored into outer's field.
        let inner = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let _store = g.add_node(Op::Store(0), vec![outer, inner]);
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&outer),
            Some(&EscapeState::GlobalEscape)
        );
        // Inner should also be GlobalEscape because it's stored into a
        // globally-escaping object.
        assert_eq!(
            result.escape_states.get(&inner),
            Some(&EscapeState::GlobalEscape)
        );
    }

    #[test]
    fn test_value_stored_into_param_field_escapes() {
        // Regression for a lazy-init getter shape first audited during the
        // keycloak `CredentialModelTest` investigation:
        //
        //   Map getAdditionalParameters() {
        //       if (additionalParameters == null)
        //           additionalParameters = new MultivaluedHashMap<>();
        //       return additionalParameters;
        //   }
        //
        // `this` is `Param(0)`.  The freshly allocated map is published into a
        // field of `this`, so the caller can reach it after the method returns.
        // It MUST be treated as escaping and MUST NOT be scalar-replaced —
        // scalar-replacing it would elide the `putfield`, leaving the instance
        // field null and the getter returning `null` (a value that can never
        // legitimately be null on a correct JVM). The real keycloak failure
        // later proved to be outside this IR scalar-replacement path, but the
        // invariant remains valid.
        let mut g = Graph::new();
        let this = g.add_node(Op::Param(0), vec![]);
        let map = g.add_node(
            Op::New {
                class_id: 7,
                num_fields: 0,
            },
            vec![],
        );
        let _store = g.add_node(Op::Store(0), vec![this, map]);
        // Reload the field and return it, as the real getter does.
        let load = g.add_node(Op::Load(0), vec![this]);
        g.nodes[1].inputs.push(load); // Return node
        g.nodes[load].uses.push(1);

        let result = analyze_escapes(&g);
        assert_ne!(
            result.escape_states.get(&map),
            Some(&EscapeState::NoEscape),
            "object published into a parameter's field must escape"
        );
        assert!(
            result.scalar_replaceable.is_empty(),
            "a published object must not be scalar-replaced (would drop the putfield)"
        );
    }

    // ---- Scalar replacement ----

    #[test]
    fn test_scalar_replacement_single_field() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 5,
                num_fields: 1,
            },
            vec![],
        );
        let c10 = g.add_node(Op::Const(10), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c10]);
        let _load = g.add_node(Op::Load(0), vec![alloc]);
        let result = analyze_escapes(&g);
        assert_eq!(result.scalar_replaceable.len(), 1);
        assert_eq!(result.scalar_replaceable[0].class_id, 5);
        assert_eq!(result.scalar_replaceable[0].field_values[0], Some(c10));
    }

    #[test]
    fn test_scalar_replacement_multi_field() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 3,
                num_fields: 3,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let c3 = g.add_node(Op::Const(3), vec![]);
        let _s0 = g.add_node(Op::Store(0), vec![alloc, c1]);
        let _s1 = g.add_node(Op::Store(1), vec![alloc, c2]);
        let _s2 = g.add_node(Op::Store(2), vec![alloc, c3]);
        let _l1 = g.add_node(Op::Load(1), vec![alloc]);
        let result = analyze_escapes(&g);
        assert_eq!(result.scalar_replaceable.len(), 1);
        let sr = &result.scalar_replaceable[0];
        assert_eq!(sr.num_fields, 3);
        assert_eq!(sr.field_values, vec![Some(c1), Some(c2), Some(c3)]);
        assert_eq!(sr.replaced_loads.len(), 1);
        assert_eq!(sr.eliminated_stores.len(), 3);
    }

    #[test]
    fn test_load_after_store_returns_stored_value() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c99 = g.add_node(Op::Const(99), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c99]);
        let _load = g.add_node(Op::Load(0), vec![alloc]);
        let result = analyze_escapes(&g);
        let sr = &result.scalar_replaceable[0];
        assert_eq!(sr.field_values[0], Some(c99));
    }

    #[test]
    fn test_uninitialized_field_returns_none() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 2,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        // Field 1 is never stored to.
        let _load = g.add_node(Op::Load(1), vec![alloc]);
        let result = analyze_escapes(&g);
        let sr = &result.scalar_replaceable[0];
        assert_eq!(sr.field_values[1], None);
    }

    // ---- Lock elision ----

    #[test]
    fn test_lock_on_non_escaping_elided() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let _enter = g.add_node(Op::MonitorEnter, vec![alloc]);
        let _exit = g.add_node(Op::MonitorExit, vec![alloc]);
        let result = analyze_escapes(&g);
        assert_eq!(result.elide_locks.len(), 2);
        assert_eq!(result.stats.locks_elided, 2);
    }

    #[test]
    fn test_lock_on_escaping_object_kept() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        g.nodes[1].inputs.push(alloc); // Return
        g.nodes[alloc].uses.push(1);
        let _enter = g.add_node(Op::MonitorEnter, vec![alloc]);
        let result = analyze_escapes(&g);
        assert!(result.elide_locks.is_empty());
    }

    // ---- Partial escape ----

    #[test]
    fn test_partial_escape_detection() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        // Also passed to a call (ArgEscape).
        let _call = g.add_node(Op::Call, vec![alloc]);
        let cg = build_connection_graph(&g);
        assert!(is_partial_escape(&cg, &g, alloc));
    }

    #[test]
    fn test_materialization_point_found() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c7 = g.add_node(Op::Const(7), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c7]);
        let _call = g.add_node(Op::Call, vec![alloc]);

        let cg = build_connection_graph(&g);
        let mps = find_materialization_points(&cg, &g, alloc);
        assert_eq!(mps.len(), 1);
        assert_eq!(mps[0].escape_point, _call);
        assert_eq!(mps[0].fields_at_escape[0], Some(c7));
    }

    // ---- Connection graph internals ----

    #[test]
    fn test_phi_deferred_edges() {
        let mut g = Graph::new();
        let a1 = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let a2 = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let phi = g.add_node(Op::Phi, vec![a1, a2]);
        let _ = phi; // just used for CG building
        let cg = build_connection_graph(&g);
        let pts = cg.resolve_points_to(phi);
        assert!(pts.contains(&a1));
        assert!(pts.contains(&a2));
    }

    #[test]
    fn test_points_to_propagation_through_copies() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        // Phi acts as a "copy".
        let phi1 = g.add_node(Op::Phi, vec![alloc]);
        let phi2 = g.add_node(Op::Phi, vec![phi1]);
        let cg = build_connection_graph(&g);
        let pts = cg.resolve_points_to(phi2);
        assert!(pts.contains(&alloc));
    }

    #[test]
    fn test_field_edge_tracking() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 2,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let _s0 = g.add_node(Op::Store(0), vec![alloc, c1]);
        let _s1 = g.add_node(Op::Store(1), vec![alloc, c2]);
        let cg = build_connection_graph(&g);
        assert!(cg.field_edges.get(&(alloc, 0)).unwrap().contains(&c1));
        assert!(cg.field_edges.get(&(alloc, 1)).unwrap().contains(&c2));
    }

    #[test]
    fn test_fixed_point_convergence() {
        // Build a graph where propagation needs multiple iterations.
        let mut g = Graph::new();
        let a1 = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let phi1 = g.add_node(Op::Phi, vec![a1]);
        let phi2 = g.add_node(Op::Phi, vec![phi1]);
        // Return phi2 -> should propagate GlobalEscape back to a1.
        g.nodes[1].inputs.push(phi2);
        g.nodes[phi2].uses.push(1);
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&a1),
            Some(&EscapeState::GlobalEscape)
        );
    }

    // ---- Empty / edge cases ----

    #[test]
    fn test_no_allocations_empty_result() {
        let mut g = Graph::new();
        let _c = g.add_node(Op::Const(1), vec![]);
        let _add = g.add_node(Op::Add, vec![]);
        let result = analyze_escapes(&g);
        assert!(result.escape_states.is_empty());
        assert_eq!(result.stats.total_allocations, 0);
    }

    #[test]
    fn test_array_returned_global_escape() {
        let mut g = Graph::new();
        let arr = g.add_node(
            Op::NewArray {
                element_type: 10,
                length: None,
            },
            vec![],
        );
        g.nodes[1].inputs.push(arr);
        g.nodes[arr].uses.push(1);
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&arr),
            Some(&EscapeState::GlobalEscape)
        );
    }

    #[test]
    fn test_array_local_no_escape() {
        let mut g = Graph::new();
        let arr = g.add_node(
            Op::NewArray {
                element_type: 5,
                length: None,
            },
            vec![],
        );
        let _len = g.add_node(Op::ArrayLength, vec![arr]);
        let result = analyze_escapes(&g);
        assert_eq!(result.escape_states.get(&arr), Some(&EscapeState::NoEscape));
    }

    #[test]
    fn test_multiple_allocations() {
        let mut g = Graph::new();
        let a1 = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let a2 = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let a3 = g.add_node(
            Op::New {
                class_id: 3,
                num_fields: 0,
            },
            vec![],
        );
        // a1 returned.
        g.nodes[1].inputs.push(a1);
        g.nodes[a1].uses.push(1);
        // a2 passed to call.
        let _call = g.add_node(Op::Call, vec![a2]);
        // a3 purely local.
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&a1),
            Some(&EscapeState::GlobalEscape)
        );
        assert_eq!(result.escape_states.get(&a2), Some(&EscapeState::ArgEscape));
        assert_eq!(result.escape_states.get(&a3), Some(&EscapeState::NoEscape));
        assert_eq!(result.stats.total_allocations, 3);
    }

    #[test]
    fn test_stats_counting() {
        let mut g = Graph::new();
        let _a1 = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let a2 = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let _call = g.add_node(Op::Call, vec![a2]);
        // a1 is local -> scalar replaceable, a2 is ArgEscape -> stack alloc.
        let result = analyze_escapes(&g);
        assert_eq!(result.stats.total_allocations, 2);
        assert_eq!(result.stats.no_escape, 1);
        assert_eq!(result.stats.arg_escape, 1);
        assert_eq!(result.stats.scalar_replaced, 1);
        assert_eq!(result.stats.stack_allocated, 1);
    }

    // ---- Graph transformations ----

    #[test]
    fn test_apply_scalar_replacement_marks_dead() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c10 = g.add_node(Op::Const(10), vec![]);
        let store = g.add_node(Op::Store(0), vec![alloc, c10]);
        let load = g.add_node(Op::Load(0), vec![alloc]);

        let info = ScalarReplacementInfo {
            alloc_node: alloc,
            class_id: 1,
            num_fields: 1,
            field_values: vec![Some(c10)],
            field_stores: vec![vec![FieldStore { store, value: c10 }]],
            replaced_loads: vec![load],
            load_values: vec![(load, LoadResolution::Value(c10))],
            eliminated_stores: vec![store],
            array_element_type: None,
            dominance_proved: true,
        };

        apply_scalar_replacement(&mut g, &info);
        assert_eq!(g.nodes[alloc].op, Op::Dead);
        assert_eq!(g.nodes[store].op, Op::Dead);
        assert_eq!(g.nodes[load].op, Op::Dead);
    }

    #[test]
    fn test_apply_lock_elision_marks_dead() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let enter = g.add_node(Op::MonitorEnter, vec![alloc]);
        let exit = g.add_node(Op::MonitorExit, vec![alloc]);

        apply_lock_elision(&mut g, &[enter, exit]);
        assert_eq!(g.nodes[enter].op, Op::Dead);
        assert_eq!(g.nodes[exit].op, Op::Dead);
    }

    #[test]
    fn test_deferred_edge_propagation() {
        let mut cg = ConnectionGraph::new();
        cg.add_points_to(10, 10); // alloc 10 points to itself
        cg.add_deferred(20, 10); // node 20 copies from 10
        cg.add_deferred(30, 20); // node 30 copies from 20

        let pts = cg.resolve_points_to(30);
        assert!(pts.contains(&10));
    }

    #[test]
    fn test_nested_object_stored_to_escaping() {
        let mut g = Graph::new();
        let outer = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let inner = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let _store = g.add_node(Op::Store(0), vec![outer, inner]);
        // Outer escapes via call.
        let _call = g.add_node(Op::Call, vec![outer]);
        let result = analyze_escapes(&g);
        // Outer: ArgEscape (passed to call).
        assert!(result.escape_states.get(&outer).unwrap() >= &EscapeState::ArgEscape);
        // Inner: stored into outer which escapes, so inner escapes too.
        // At minimum ArgEscape via propagation.
        let inner_state = result.escape_states.get(&inner);
        assert!(inner_state.is_some());
    }

    #[test]
    fn test_connection_graph_empty_for_arithmetic() {
        let mut g = Graph::new();
        let c1 = g.add_node(Op::Const(1), vec![]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let _add = g.add_node(Op::Add, vec![c1, c2]);
        let cg = build_connection_graph(&g);
        assert!(cg.points_to.is_empty());
        assert!(cg.field_edges.is_empty());
        assert!(cg.deferred_edges.is_empty());
    }

    #[test]
    fn test_merge_point_escape_states_join_is_max() {
        // Two allocations, one NoEscape and one GlobalEscape, merged
        // through a phi. The phi should be GlobalEscape.
        let mut g = Graph::new();
        let a1 = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let a2 = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        // a2 escapes globally.
        g.nodes[1].inputs.push(a2);
        g.nodes[a2].uses.push(1);
        let phi = g.add_node(Op::Phi, vec![a1, a2]);

        let mut cg = build_connection_graph(&g);
        propagate_escape_states(&mut cg, &g);

        // The phi merges a NoEscape and a GlobalEscape.
        // Since a2 is GlobalEscape, the phi should be at least GlobalEscape.
        let phi_state = cg.get_escape(phi);
        assert!(phi_state >= EscapeState::GlobalEscape);
    }

    #[test]
    fn test_escape_state_join_lattice() {
        assert_eq!(
            EscapeState::NoEscape.join(EscapeState::NoEscape),
            EscapeState::NoEscape
        );
        assert_eq!(
            EscapeState::NoEscape.join(EscapeState::ArgEscape),
            EscapeState::ArgEscape
        );
        assert_eq!(
            EscapeState::ArgEscape.join(EscapeState::GlobalEscape),
            EscapeState::GlobalEscape
        );
        assert_eq!(
            EscapeState::NoEscape.join(EscapeState::GlobalEscape),
            EscapeState::GlobalEscape
        );
    }

    #[test]
    fn test_scalar_replacement_not_applied_to_escaping() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        // Return the allocation -> GlobalEscape.
        g.nodes[1].inputs.push(alloc);
        g.nodes[alloc].uses.push(1);
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
    }

    #[test]
    fn test_apply_scalar_replacement_redirects_uses() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c42 = g.add_node(Op::Const(42), vec![]);
        let store = g.add_node(Op::Store(0), vec![alloc, c42]);
        let load = g.add_node(Op::Load(0), vec![alloc]);
        // An Add that uses the load result.
        let add = g.add_node(Op::Add, vec![load, load]);

        let info = ScalarReplacementInfo {
            alloc_node: alloc,
            class_id: 1,
            num_fields: 1,
            field_values: vec![Some(c42)],
            field_stores: vec![vec![FieldStore { store, value: c42 }]],
            replaced_loads: vec![load],
            load_values: vec![(load, LoadResolution::Value(c42))],
            eliminated_stores: vec![store],
            array_element_type: None,
            dominance_proved: true,
        };

        apply_scalar_replacement(&mut g, &info);
        // The add should now reference c42 instead of the dead load.
        assert_eq!(g.nodes[add].inputs, vec![c42, c42]);
    }

    #[test]
    fn test_no_escape_state_for_non_alloc_node() {
        let g = graph_local_alloc();
        let result = analyze_escapes(&g);
        // Const node (id=3) should not appear in escape_states.
        assert!(!result.escape_states.contains_key(&3));
    }

    #[test]
    fn test_param_ref_in_phi_does_not_crash() {
        let mut g = Graph::new();
        let param = g.add_node(Op::Param(0), vec![]);
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let _phi = g.add_node(Op::Phi, vec![param, alloc]);
        // Should not panic.
        let _ = analyze_escapes(&g);
    }

    #[test]
    fn test_multiple_stores_to_same_field_last_wins() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let _s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
        let _s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
        let result = analyze_escapes(&g);
        // The last store wins in the field_values vector.
        let sr = &result.scalar_replaceable[0];
        assert_eq!(sr.field_values[0], Some(c2));
    }

    #[test]
    fn test_new_array_without_constant_length_not_replaced() {
        // A `newarray` whose length the producer could not prove constant has no
        // slot count, so there is nothing to map onto `field_values`. It is
        // still NoEscape -- the refusal is about VALUE, not reachability.
        let mut g = Graph::new();
        let _arr = g.add_node(
            Op::NewArray {
                element_type: 4,
                length: None,
            },
            vec![],
        );
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
        assert_eq!(result.stats.no_escape, 1);
        assert_eq!(
            result.scalar_refusals,
            vec![(
                2,
                ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::LengthNotConstant)
            )],
            "the refusal must name the missing length, not a generic bail"
        );
    }

    /// The `Short2` shape: a `short[2]`, both slots stored then both read.
    ///
    /// This is the half of the per-voxel wrapper that `Op::New` scalar
    /// replacement could never reach -- `docs/known-issues/jit/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-20260827.md`
    /// recorded it as `test_new_array_local_scalar_not_replaced`.
    #[test]
    fn test_constant_length_array_is_scalar_replaced() {
        let mut g = Graph::new();
        let arr = g.add_node(
            Op::NewArray {
                element_type: 9, // short
                length: Some(2),
            },
            vec![],
        );
        let v0 = g.add_node(Op::Const(7), vec![]);
        let v1 = g.add_node(Op::Const(9), vec![]);
        g.add_node(Op::Store(0), vec![arr, v0]);
        g.add_node(Op::Store(1), vec![arr, v1]);
        let l0 = g.add_node(Op::Load(0), vec![arr]);
        let l1 = g.add_node(Op::Load(1), vec![arr]);
        g.add_node(Op::Add, vec![l0, l1]);

        let result = analyze_escapes(&g);
        let info = result
            .scalar_replaceable
            .iter()
            .find(|i| i.alloc_node == arr)
            .expect("a two-slot array with constant indices is replaceable");
        assert_eq!(info.array_element_type, Some(9));
        assert_eq!(info.num_fields, 2);
        assert_eq!(info.field_values, vec![Some(v0), Some(v1)]);
        assert_eq!(info.load_value(l0), LoadResolution::Value(v0));
        assert_eq!(info.load_value(l1), LoadResolution::Value(v1));
    }

    #[test]
    fn test_array_longer_than_the_cap_is_refused_by_length() {
        let mut g = Graph::new();
        let arr = g.add_node(
            Op::NewArray {
                element_type: 10,
                length: Some(MAX_SCALAR_ARRAY_LEN + 1),
            },
            vec![],
        );
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
        assert_eq!(
            result.scalar_refusals,
            vec![(
                arr,
                ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::LengthTooLarge(
                    MAX_SCALAR_ARRAY_LEN + 1
                ))
            )],
        );
    }

    #[test]
    fn test_reference_array_is_refused() {
        // `anewarray` -> element_type 0. A never-stored slot would have to be
        // answered with `null`, and the applier's zero default is numeric.
        let mut g = Graph::new();
        let arr = g.add_node(
            Op::NewArray {
                element_type: 0,
                length: Some(2),
            },
            vec![],
        );
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
        assert_eq!(
            result.scalar_refusals,
            vec![(
                arr,
                ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::ReferenceElements)
            )],
        );
    }

    #[test]
    fn test_array_index_out_of_range_refuses_the_array() {
        // A constant index outside `0..len` throws `ArrayIndexOutOfBounds` at
        // run time; deleting the array would delete the throw.
        let mut g = Graph::new();
        let arr = g.add_node(
            Op::NewArray {
                element_type: 9,
                length: Some(2),
            },
            vec![],
        );
        let v = g.add_node(Op::Const(1), vec![]);
        g.add_node(Op::Store(5), vec![arr, v]);
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
        assert_eq!(
            result.scalar_refusals,
            vec![(arr, ScalarRefusal::FieldIndexOutOfRange)],
        );
    }

    #[test]
    fn test_arraylength_on_a_replaced_array_is_refused() {
        // Nothing folds `arraylength` to the constant, so replacing the array
        // would leave a live node reading a deleted one.
        let mut g = Graph::new();
        let arr = g.add_node(
            Op::NewArray {
                element_type: 9,
                length: Some(2),
            },
            vec![],
        );
        g.add_node(Op::ArrayLength, vec![arr]);
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
        assert_eq!(
            result.scalar_refusals,
            vec![(
                arr,
                ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::LengthReadNotFolded)
            )],
        );
    }

    /// The refusal instrument is only worth having if it names the fact that
    /// actually stopped the object. This is the shape the per-voxel page hit:
    /// a graph with a branch, an object whose accesses are all in one block.
    #[test]
    fn test_one_block_dominance_answers_a_load_a_branchy_graph_could_not() {
        // Build a graph with a branch somewhere else entirely, so
        // `program_order_proves_dominance` is false for the whole graph.
        let build = |same_block: bool| {
            let mut g = Graph::new();
            let block = g.add_node(Op::Merge, vec![]);
            let other = g.add_node(Op::Merge, vec![]);
            let obj = g.add_node(
                Op::New {
                    class_id: 1,
                    num_fields: 1,
                },
                vec![],
            );
            let v = g.add_node(Op::Const(42), vec![]);
            let st = g.add_node(Op::Store(0), vec![obj, v]);
            let ld = g.add_node(Op::Load(0), vec![obj]);
            g.add_node(Op::Add, vec![ld, ld]);
            g.blocks.insert(obj, block);
            g.blocks.insert(st, block);
            g.blocks.insert(ld, if same_block { block } else { other });
            (g, obj, v, ld)
        };

        let (g, obj, v, ld) = build(true);
        assert!(
            !program_order_proves_dominance(&g),
            "precondition: the graph has a Merge, so the whole-graph stand-in fails"
        );
        let result = analyze_escapes(&g);
        let info = result
            .scalar_replaceable
            .iter()
            .find(|i| i.alloc_node == obj)
            .expect("every access is in one block, so the load is answerable");
        assert!(info.dominance_proved);
        assert_eq!(info.load_value(ld), LoadResolution::Value(v));

        // Move the load to another block and the proof must fail closed.
        let (g, obj, _, _) = build(false);
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
        assert_eq!(
            result.scalar_refusals,
            vec![(obj, ScalarRefusal::LoadNotAnswerable)],
            "a load in another block is not ordered by program order"
        );
    }

    #[test]
    fn test_partial_escape_false_for_no_escape() {
        let g = graph_local_alloc();
        let cg = build_connection_graph(&g);
        // Alloc is node 2 and fully NoEscape.
        assert!(!is_partial_escape(&cg, &g, 2));
    }

    #[test]
    fn test_materialization_empty_for_no_escape() {
        let g = graph_local_alloc();
        let cg = build_connection_graph(&g);
        let mps = find_materialization_points(&cg, &g, 2);
        assert!(mps.is_empty());
    }

    #[test]
    fn test_connection_graph_resolve_cycle() {
        // Ensure cycles in deferred edges don't cause infinite loops.
        let mut cg = ConnectionGraph::new();
        cg.add_points_to(10, 10);
        cg.add_deferred(20, 10);
        cg.add_deferred(10, 20); // cycle!
        let pts = cg.resolve_points_to(20);
        assert!(pts.contains(&10));
    }

    #[test]
    fn test_graph_new_helper() {
        let g = Graph::new();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.entry, 0);
        assert_eq!(g.exit, 1);
        assert_eq!(g.nodes[0].op, Op::Start);
        assert_eq!(g.nodes[1].op, Op::Return);
    }

    // ---- Load/Store input + field-index modeling (landmine pins) ----
    //
    // These tests pin the CANONICAL EA-internal Load/Store layout
    // (`Store: [holder, value]`, `Load: [holder]`, payload = FIELD INDEX) so a
    // future builder/lowering change that emits these nodes cannot silently
    // miscompile the escape lattice.  They are the regression fence for the
    // bugs described in the layout-contract comment near `Op::Load`/`Op::Store`.

    /// Helper accessors must read holder/value from the documented positions.
    #[test]
    fn test_store_load_layout_accessors() {
        let mut g = Graph::new();
        let holder = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let value = g.add_node(Op::Const(7), vec![]);
        let store = g.add_node(Op::Store(0), vec![holder, value]);
        let load = g.add_node(Op::Load(0), vec![holder]);

        assert_eq!(store_holder(&g.nodes[store]), Some(holder));
        assert_eq!(store_value(&g.nodes[store]), Some(value));
        assert_eq!(load_holder(&g.nodes[load]), Some(holder));
    }

    /// A malformed store (holder only, no value) must NOT fabricate a value.
    #[test]
    fn test_store_missing_value_is_none() {
        let mut g = Graph::new();
        let holder = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let store = g.add_node(Op::Store(0), vec![holder]); // no value operand
        assert_eq!(store_value(&g.nodes[store]), None);
        // Build must not panic and must record no field edge for the missing value.
        let cg = build_connection_graph(&g);
        assert!(cg.field_edges.get(&(holder, 0)).is_none());
    }

    /// SOUNDNESS: a value stored into an *ArgEscape* holder must escape too.
    /// (The pre-fix code only handled GlobalEscape, leaving this value
    /// spuriously NoEscape — unsound for stack-allocation / lock elision.)
    #[test]
    fn test_value_stored_into_arg_escaping_holder_escapes() {
        let mut g = Graph::new();
        let holder = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let inner = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        // inner is stored into holder's field 0 ...
        let _store = g.add_node(Op::Store(0), vec![holder, inner]);
        // ... and holder is passed to a call (ArgEscape, NOT GlobalEscape).
        let _call = g.add_node(Op::Call, vec![holder]);

        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&holder),
            Some(&EscapeState::ArgEscape)
        );
        // inner is reachable through holder by the callee, so it must escape
        // at least as much as holder.
        let inner_state = *result.escape_states.get(&inner).unwrap();
        assert!(
            inner_state >= EscapeState::ArgEscape,
            "value stored into ArgEscape holder must escape >= ArgEscape, got {:?}",
            inner_state
        );
    }

    /// A load reads the FIELD's contents, not the holder: the load's
    /// provenance/escape comes from the stored value, and the load must NOT
    /// alias the holder allocation (the old `load -> holder` deferred edge).
    #[test]
    fn test_load_is_field_sensitive_not_holder_alias() {
        let mut g = Graph::new();
        let holder = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let stored = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let _store = g.add_node(Op::Store(0), vec![holder, stored]);
        let load = g.add_node(Op::Load(0), vec![holder]);

        let mut cg = build_connection_graph(&g);
        propagate_escape_states(&mut cg, &g);

        let load_pts = cg.resolve_points_to(load);
        // The load points to what was stored into the field ...
        assert!(
            load_pts.contains(&stored),
            "field-sensitive load must point to the stored value"
        );
        // ... and crucially NOT to the holder allocation itself.
        assert!(
            !load_pts.contains(&holder),
            "load must not alias its holder object"
        );
    }

    /// Escaping the loaded value must escape the STORED value (through the
    /// field), but must NOT, by itself, escape the holder via aliasing.
    #[test]
    fn test_returned_load_escapes_stored_value_only() {
        let mut g = Graph::new();
        let holder = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let stored = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let _store = g.add_node(Op::Store(0), vec![holder, stored]);
        let load = g.add_node(Op::Load(0), vec![holder]);
        // Return the loaded value -> GlobalEscape flows to the stored value.
        g.nodes[1].inputs.push(load);
        g.nodes[load].uses.push(1);

        let result = analyze_escapes(&g);
        // The value read out of the field and returned escapes globally.
        assert_eq!(
            result.escape_states.get(&stored),
            Some(&EscapeState::GlobalEscape)
        );
    }

    /// An out-of-range field index (e.g. a `MemKind` tag mistakenly used as a
    /// field index) must NOT mis-index a field: scalar replacement bails, and
    /// the value still escapes conservatively if the holder escapes.
    #[test]
    fn test_out_of_range_field_index_is_conservative() {
        let mut g = Graph::new();
        let holder = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1, // only field 0 exists
            },
            vec![],
        );
        let value = g.add_node(Op::Const(3), vec![]);
        // field index 4 is out of range (mimics a leaked MemKind::Ref tag).
        let _store = g.add_node(Op::Store(4), vec![holder, value]);
        let _load = g.add_node(Op::Load(4), vec![holder]);

        let result = analyze_escapes(&g);
        // Holder cannot be scalar-replaced because of the out-of-range access.
        assert!(
            result.scalar_replaceable.is_empty(),
            "out-of-range field access must block scalar replacement"
        );
        // field_in_range rejects index 4 for a 1-field object.
        let allocs: HashSet<NodeId> = std::iter::once(holder).collect();
        assert!(!field_in_range(&g, &allocs, 4));
        assert!(field_in_range(&g, &allocs, 0));
    }

    /// A NoEscape holder must NOT escape values stored into it.
    #[test]
    fn test_value_stored_into_local_holder_does_not_escape() {
        let mut g = Graph::new();
        let holder = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let inner = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let _store = g.add_node(Op::Store(0), vec![holder, inner]);
        // Neither holder nor inner escapes anywhere.
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&holder),
            Some(&EscapeState::NoEscape)
        );
        assert_eq!(
            result.escape_states.get(&inner),
            Some(&EscapeState::NoEscape)
        );
    }

    // ---- Widened scalar-replacement shapes (increment 1) ----

    /// WIDENING: an allocation whose field load is reached *through* a
    /// NoEscape phi that resolves solely to this allocation must now be
    /// scalar-replaced.  The pre-widening shape rejected ANY phi use via the
    /// catch-all, so this object stayed heap-allocated.
    #[test]
    fn test_scalar_replacement_through_transparent_phi() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            vec![],
        );
        let c5 = g.add_node(Op::Const(5), vec![]);
        // Store 5 into field 0 of the allocation.
        let _store = g.add_node(Op::Store(0), vec![alloc, c5]);
        // A phi that carries ONLY this allocation (e.g. `o = o` across a
        // merge): points-to set is the singleton {alloc}.
        let phi = g.add_node(Op::Phi, vec![alloc]);
        // Load field 0 THROUGH the phi (holder = phi, not alloc directly).
        let _load = g.add_node(Op::Load(0), vec![phi]);

        let result = analyze_escapes(&g);
        // The allocation does not escape ...
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::NoEscape)
        );
        // ... and is now scalar-replaceable despite the intervening phi.
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("allocation reached only through a transparent phi must scalar-replace");
        assert_eq!(sr.field_values[0], Some(c5));
        // The load through the phi is recorded for replacement.
        assert!(sr.replaced_loads.contains(&_load));
    }

    /// SOUNDNESS GUARD on the widening: a phi that merges this allocation
    /// with a DIFFERENT allocation must NOT be treated as transparent — a
    /// load through it could be reading the other object's field. Scalar
    /// replacement must bail for both allocations.
    #[test]
    fn test_phi_merging_two_allocs_blocks_scalar_replacement() {
        let mut g = Graph::new();
        let a1 = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let a2 = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let _s1 = g.add_node(Op::Store(0), vec![a1, c1]);
        let _s2 = g.add_node(Op::Store(0), vec![a2, c2]);
        // phi merges two DISTINCT allocations.
        let phi = g.add_node(Op::Phi, vec![a1, a2]);
        // Load field 0 through the ambiguous phi.
        let _load = g.add_node(Op::Load(0), vec![phi]);

        let result = analyze_escapes(&g);
        // Neither allocation may be scalar-replaced: folding the load to one
        // object's field value would miscompile the other control path.
        assert!(
            result.scalar_replaceable.iter().all(|s| s.alloc_node != a1),
            "a1 must not scalar-replace through an ambiguous phi"
        );
        assert!(
            result.scalar_replaceable.iter().all(|s| s.alloc_node != a2),
            "a2 must not scalar-replace through an ambiguous phi"
        );
    }

    /// SOUNDNESS GUARD: a phi merging this allocation with a `Param` (unknown
    /// provenance) resolves to the singleton {alloc} in the points-to set —
    /// because the Param contributes no points-to entry — yet it genuinely
    /// carries a foreign object on the Param path.  The widening must NOT
    /// treat it as transparent; scalar replacement must bail.
    #[test]
    fn test_phi_merging_alloc_and_param_blocks_scalar_replacement() {
        let mut g = Graph::new();
        let param = g.add_node(Op::Param(0), vec![]);
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        // phi merges the allocation with an opaque parameter reference.
        let phi = g.add_node(Op::Phi, vec![alloc, param]);
        let _load = g.add_node(Op::Load(0), vec![phi]);

        let result = analyze_escapes(&g);
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "a phi merging the allocation with a Param must block scalar replacement"
        );
    }

    /// WIDENING: a stale `Op::Dead` node left in the allocation's use list
    /// must be skipped, not treated as an unknown use that blocks SR.
    #[test]
    fn test_scalar_replacement_skips_dead_use() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c9 = g.add_node(Op::Const(9), vec![]);
        let store = g.add_node(Op::Store(0), vec![alloc, c9]);
        let _load = g.add_node(Op::Load(0), vec![alloc]);
        // Simulate an earlier pass having killed a former user of `alloc`
        // while leaving the use-edge dangling.
        let dead = g.add_node(Op::Other, vec![alloc]);
        g.nodes[dead].op = Op::Dead;

        let result = analyze_escapes(&g);
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("a stale Dead use must not block scalar replacement");
        assert_eq!(sr.field_values[0], Some(c9));
        assert!(sr.eliminated_stores.contains(&store));
    }

    /// An allocation used as a stored VALUE (published into another object's
    /// field) must not be scalar-replaced.
    #[test]
    fn test_published_value_not_scalar_replaced() {
        let mut g = Graph::new();
        let outer = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let inner = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let _store = g.add_node(Op::Store(0), vec![outer, inner]);
        // outer escapes via return -> inner is published into an escaping field.
        g.nodes[1].inputs.push(outer);
        g.nodes[outer].uses.push(1);

        let result = analyze_escapes(&g);
        // inner must not be in the scalar-replacement set.
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|sr| sr.alloc_node != inner),
            "a value published into an escaping object's field is not SR-eligible"
        );
    }

    // -- non-convergence must fail closed ---------------------------------

    /// `escalate_all_to_global` has to cover nodes that were never entered in
    /// the escape map at all: `get_escape` reports an absent node as
    /// `NoEscape`, so leaving the map alone would keep exactly the unreached
    /// nodes looking optimisable.
    #[test]
    fn escalation_covers_nodes_with_no_map_entry() {
        let mut g = Graph::new();
        let a = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let b = g.add_node(Op::Const(7), vec![]);
        let mut cg = ConnectionGraph::new();
        // Deliberately populate nothing: every node reads back as NoEscape.
        assert_eq!(cg.get_escape(a), EscapeState::NoEscape);
        assert_eq!(cg.get_escape(b), EscapeState::NoEscape);

        escalate_all_to_global(&mut cg, &g);

        for id in 0..g.nodes.len() {
            assert_eq!(
                cg.get_escape(id),
                EscapeState::GlobalEscape,
                "node {id} must be escalated"
            );
        }
    }

    /// After escalation neither live consumer may offer a candidate — that is
    /// the whole point of failing closed.
    #[test]
    fn escalation_disables_scalar_replacement_and_lock_elision() {
        let mut g = Graph::new();
        let obj = g.add_node(
            Op::New {
                class_id: 3,
                num_fields: 1,
            },
            vec![],
        );
        let c = g.add_node(Op::Const(1), vec![]);
        let _st = g.add_node(Op::Store(0), vec![obj, c]);
        let _me = g.add_node(Op::MonitorEnter, vec![obj]);
        let _mx = g.add_node(Op::MonitorExit, vec![obj]);

        // Baseline: this shape IS optimisable when the fixed point converges.
        let converged = analyze_escapes(&g);
        assert!(
            !converged.scalar_replaceable.is_empty() || !converged.elide_locks.is_empty(),
            "fixture must be optimisable before escalation, else the test proves nothing"
        );

        // Now the failed-fixed-point answer.
        let mut cg = build_connection_graph(&g);
        escalate_all_to_global(&mut cg, &g);
        assert!(
            find_scalar_replacements(&cg, &g).0.is_empty(),
            "no allocation may be scalar-replaced after escalation"
        );
        assert!(
            find_lock_elisions(&cg, &g).is_empty(),
            "no monitor may be elided after escalation"
        );
    }

    /// The escalation must NOT fire on an ordinary graph — a fixed point that
    /// converges keeps its precise answer.
    #[test]
    fn converging_graph_is_not_escalated() {
        let mut g = Graph::new();
        let obj = g.add_node(
            Op::New {
                class_id: 4,
                num_fields: 1,
            },
            vec![],
        );
        let c = g.add_node(Op::Const(42), vec![]);
        let _st = g.add_node(Op::Store(0), vec![obj, c]);
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&obj).copied(),
            Some(EscapeState::NoEscape),
            "a purely local allocation must stay NoEscape"
        );
        assert!(!result.scalar_replaceable.is_empty());
    }

    /// Documented, deliberately unwired output: `stack_allocatable` is
    /// computed but no caller reads it. Pin the shape so a future consumer
    /// knows what it is getting (ArgEscape allocations only — never NoEscape,
    /// which goes to `scalar_replaceable`, and never GlobalEscape).
    #[test]
    fn stack_allocatable_holds_only_arg_escaping_allocations() {
        let mut g = Graph::new();
        let obj = g.add_node(
            Op::New {
                class_id: 5,
                num_fields: 0,
            },
            vec![],
        );
        // Passing the object to a call is ArgEscape, not GlobalEscape.
        let _call = g.add_node(Op::Call, vec![obj]);
        let result = analyze_escapes(&g);
        for &id in &result.stack_allocatable {
            assert_eq!(
                result.escape_states.get(&id).copied(),
                Some(EscapeState::ArgEscape),
                "stack_allocatable must contain exactly the ArgEscape allocations"
            );
        }
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|sr| sr.alloc_node != obj),
            "an ArgEscape allocation is not scalar-replaceable"
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Positional (dominance-aware) store tracking
    // ══════════════════════════════════════════════════════════════════
    //
    // The defect these fence: `field_values` records only the LAST store per
    // field, and forwarding every load to it hands a load a value from its own
    // future. `apply_ea_to_ir` currently *refuses* such an object; the analysis
    // now answers it instead.

    /// THE REGRESSION TEST. `Foo o = new Foo(); int a = o.x; o.x = 42;` must
    /// fold `a` to the zero default. Folding it to `42` was the miscompilation.
    #[test]
    fn load_before_store_reads_the_zero_default_not_the_later_store() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        // `int a = o.x;` — BEFORE any store.
        let load = g.add_node(Op::Load(0), vec![alloc]);
        let c42 = g.add_node(Op::Const(42), vec![]);
        // `o.x = 42;` — after the read.
        let store = g.add_node(Op::Store(0), vec![alloc, c42]);

        let result = analyze_escapes(&g);
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("a purely local object is still replaceable");

        assert_eq!(
            sr.load_value(load),
            LoadResolution::ZeroDefault,
            "a load that precedes every store to its field reads the freshly \
             allocated object's zero value"
        );
        assert_ne!(
            sr.load_value(load),
            LoadResolution::Value(c42),
            "forwarding the load to a store in its own future is THE \
             miscompilation this test exists for"
        );
        // The last-write-wins map still says 42 — which is precisely why it is
        // the wrong forwarding source and `load_values` exists.
        assert_eq!(sr.field_values[0], Some(c42));
        assert_eq!(sr.field_stores[0], vec![FieldStore { store, value: c42 }]);
    }

    /// The paired POSITIVE: a load after the store reads that store.
    #[test]
    fn load_after_store_reads_that_store() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c42 = g.add_node(Op::Const(42), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c42]);
        let load = g.add_node(Op::Load(0), vec![alloc]);

        let result = analyze_escapes(&g);
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("must replace");
        assert_eq!(sr.load_value(load), LoadResolution::Value(c42));
        assert!(sr.dominance_proved);
    }

    /// Two stores straddling two loads: each load keeps the value that was in
    /// the field when *it* ran. A single last-write-wins slot cannot express
    /// this at all.
    #[test]
    fn interleaved_stores_and_loads_each_resolve_positionally() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
        let load_a = g.add_node(Op::Load(0), vec![alloc]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
        let load_b = g.add_node(Op::Load(0), vec![alloc]);

        let result = analyze_escapes(&g);
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("must replace");
        assert_eq!(sr.load_value(load_a), LoadResolution::Value(c1));
        assert_eq!(sr.load_value(load_b), LoadResolution::Value(c2));
        assert_eq!(
            sr.field_stores[0],
            vec![
                FieldStore {
                    store: s1,
                    value: c1
                },
                FieldStore {
                    store: s2,
                    value: c2
                }
            ],
            "the positional record keeps BOTH stores, in program order"
        );
        assert_eq!(
            sr.field_values[0],
            Some(c2),
            "the legacy last-write-wins map is unchanged for its remaining \
             consumer (the deopt recipe builder)"
        );
    }

    /// End-to-end through the in-module applier: the rewritten graph must give
    /// each consumer the value its own load read.
    #[test]
    fn apply_scalar_replacement_forwards_each_load_to_its_own_store() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
        let load_a = g.add_node(Op::Load(0), vec![alloc]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let _s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
        let load_b = g.add_node(Op::Load(0), vec![alloc]);
        // `a + b` — the consumer that would observe the wrong fold.
        let add = g.add_node(Op::Add, vec![load_a, load_b]);

        let result = analyze_escapes(&g);
        let info = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("must replace");
        apply_scalar_replacement(&mut g, info);

        assert_eq!(
            g.nodes[add].inputs,
            vec![c1, c2],
            "forwarding both loads to `field_values[0]` would make this \
             `[c2, c2]` — i.e. `a` folded to 2"
        );
    }

    /// An unresolvable load must leave the graph completely untouched — no
    /// half-applied object.
    #[test]
    fn apply_scalar_replacement_refuses_an_unresolvable_load() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let load = g.add_node(Op::Load(0), vec![alloc]);

        let info = ScalarReplacementInfo {
            alloc_node: alloc,
            class_id: 1,
            num_fields: 1,
            field_values: vec![Some(c1)],
            field_stores: vec![vec![FieldStore { store, value: c1 }]],
            replaced_loads: vec![load],
            load_values: vec![(load, LoadResolution::Unknown)],
            eliminated_stores: vec![store],
            array_element_type: None,
            dominance_proved: false,
        };
        apply_scalar_replacement(&mut g, &info);

        assert!(
            matches!(g.nodes[alloc].op, Op::New { .. }),
            "the allocation must survive an unresolvable load"
        );
        assert!(matches!(g.nodes[load].op, Op::Load(_)));
        assert!(matches!(g.nodes[store].op, Op::Store(_)));
    }

    // ── The dominance gate ────────────────────────────────────────────

    /// `if (c) o.x = 1; else o.x = 2; int a = o.x;` — both stores precede the
    /// load in program order and NEITHER dominates it. Program order is not
    /// dominance here, so the object must be refused rather than folded to the
    /// textually-last store.
    #[test]
    fn a_branch_makes_program_order_stop_proving_dominance() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let cond = g.add_node(Op::Const(0), vec![]);
        let _iff = g.add_node(Op::If, vec![cond]);
        let c1 = g.add_node(Op::Const(1), vec![]);
        let c2 = g.add_node(Op::Const(2), vec![]);
        let _s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
        let _s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
        let _load = g.add_node(Op::Load(0), vec![alloc]);

        assert!(!program_order_proves_dominance(&g));
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::NoEscape),
            "the object still does not escape — this is a VALUE question, not \
             a reachability one"
        );
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "a load that could read either of two conditional stores must not \
             be folded to one of them"
        );
    }

    /// The paired POSITIVE for the gate: a field with **no** store anywhere
    /// resolves even in a branchy graph — no control flow can change what a
    /// never-written field holds.
    #[test]
    fn a_never_stored_field_resolves_even_without_dominance() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 2,
            },
            vec![],
        );
        let cond = g.add_node(Op::Const(0), vec![]);
        let _iff = g.add_node(Op::If, vec![cond]);
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _s0 = g.add_node(Op::Store(0), vec![alloc, c1]);
        // Field 1 is never stored, anywhere.
        let load = g.add_node(Op::Load(1), vec![alloc]);

        let result = analyze_escapes(&g);
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("a never-stored field is answerable with or without a CFG");
        assert!(!sr.dominance_proved);
        assert_eq!(sr.load_value(load), LoadResolution::ZeroDefault);
    }

    /// The three shapes that retract the dominance stand-in, and the one that
    /// does not (a single-input φ is a copy, not a join).
    #[test]
    fn program_order_dominance_predicate_shapes() {
        let mut branch = Graph::new();
        let c = branch.add_node(Op::Const(0), vec![]);
        branch.add_node(Op::If, vec![c]);
        assert!(!program_order_proves_dominance(&branch));

        let mut join = Graph::new();
        join.add_node(Op::Merge, vec![]);
        assert!(!program_order_proves_dominance(&join));

        let mut merge_phi = Graph::new();
        let a = merge_phi.add_node(Op::Const(1), vec![]);
        let b = merge_phi.add_node(Op::Const(2), vec![]);
        merge_phi.add_node(Op::Phi, vec![a, b]);
        assert!(!program_order_proves_dominance(&merge_phi));

        let mut copy_phi = Graph::new();
        let a = copy_phi.add_node(Op::Const(1), vec![]);
        copy_phi.add_node(Op::Phi, vec![a]);
        assert!(
            program_order_proves_dominance(&copy_phi),
            "a single-input φ is a degenerate copy and implies no divergence"
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Identity sensitivity
    // ══════════════════════════════════════════════════════════════════

    /// An object whose reference is compared with `==` has its ADDRESS
    /// observed. It does not escape, and it still may not be replaced.
    #[test]
    fn object_compared_by_identity_is_not_scalar_replaced() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let other = g.add_node(Op::Param(0), vec![]);
        let cmp = g.add_node(Op::RefCompare, vec![alloc, other]);

        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::NoEscape),
            "an acmp does not publish the object — escape and identity are \
             different questions"
        );
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "an object with a live identity comparison must not be replaced: \
             a bag of scalars has no address to compare"
        );
        assert!(result.identity_observations.contains(&(alloc, cmp)));
        assert_eq!(result.stats.identity_blocked, 1);
    }

    /// The paired POSITIVE: the same object without the comparison replaces.
    #[test]
    fn object_not_compared_by_identity_is_scalar_replaced() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let load = g.add_node(Op::Load(0), vec![alloc]);

        let result = analyze_escapes(&g);
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("must replace");
        assert_eq!(sr.load_value(load), LoadResolution::Value(c1));
        assert!(result.identity_observations.is_empty());
        assert_eq!(result.stats.identity_blocked, 0);
    }

    /// An identity hash is a header read: same rule as `==`.
    #[test]
    fn identity_hash_blocks_scalar_replacement() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let ihash = g.add_node(Op::IdentityHash, vec![alloc]);

        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::NoEscape)
        );
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "a scalar-replaced object has no header to hash"
        );
        assert!(result.identity_observations.contains(&(alloc, ihash)));
    }

    /// An identity observation reached through a transparent φ still counts —
    /// the alias is the same address.
    #[test]
    fn identity_observed_through_a_phi_alias_blocks_replacement() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let phi = g.add_node(Op::Phi, vec![alloc]);
        let _cmp = g.add_node(Op::RefCompare, vec![phi]);

        let result = analyze_escapes(&g);
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "the φ resolves to this allocation, so comparing the φ compares \
             this object's address"
        );
    }

    /// SYNCHRONIZATION is an identity observation: the monitor *is* the object
    /// header. A live `monitorenter` blocks replacement even though the lock is
    /// simultaneously offered for elision — because the consumer may refuse
    /// that elision (`apply_ea_to_ir` does, for a monitor a safepoint names).
    ///
    /// The paired POSITIVE is in the same test: once the monitors are actually
    /// eliminated, a second run replaces the object. That two-phase order is
    /// the supported way to get both.
    #[test]
    fn synchronized_object_is_not_replaced_until_the_monitor_is_eliminated() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 1,
            },
            vec![],
        );
        let c7 = g.add_node(Op::Const(7), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c7]);
        let _enter = g.add_node(Op::MonitorEnter, vec![alloc]);
        let _exit = g.add_node(Op::MonitorExit, vec![alloc]);
        let load = g.add_node(Op::Load(0), vec![alloc]);

        let first = analyze_escapes(&g);
        assert_eq!(
            first.escape_states.get(&alloc),
            Some(&EscapeState::NoEscape),
            "locking a local object does not make it escape"
        );
        assert!(
            first
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "MUST NOT: a live monitor observes the object's identity"
        );
        assert_eq!(
            first.elide_locks.len(),
            2,
            "the elision is still OFFERED — it is sound on its own terms"
        );
        assert_eq!(first.stats.identity_blocked, 1);

        // Phase two: actually eliminate the observation, then re-analyse.
        apply_lock_elision(&mut g, &first.elide_locks);
        let second = analyze_escapes(&g);
        let sr = second
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("MUST: with the monitors dead, nothing observes the identity");
        assert_eq!(sr.load_value(load), LoadResolution::Value(c7));
        assert_eq!(second.stats.identity_blocked, 0);
    }

    // ══════════════════════════════════════════════════════════════════
    // The lattice: partial escape and the global-escape rules
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn lattice_order_places_partial_between_no_escape_and_arg_escape() {
        assert!(EscapeState::NoEscape < EscapeState::PartialEscape);
        assert!(EscapeState::PartialEscape < EscapeState::ArgEscape);
        assert!(EscapeState::ArgEscape < EscapeState::GlobalEscape);
        // `may_escape`, not `>= ArgEscape`, is the "escapes" predicate.
        assert!(!EscapeState::NoEscape.may_escape());
        assert!(EscapeState::PartialEscape.may_escape());
        assert!(EscapeState::NoEscape.is_confined());
        assert!(!EscapeState::PartialEscape.is_confined());
    }

    /// An object that escapes only through a node the producer marked cold is
    /// classified `PartialEscape` — and that classification licenses nothing.
    #[test]
    fn object_escaping_only_on_a_cold_path_is_classified_partial() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 3,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        // The only escape: a slow-path helper the producer knows is cold.
        let cold_call = g.add_node(Op::Call, vec![alloc]);
        g.mark_cold(cold_call);

        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::PartialEscape)
        );
        let pe = result
            .partial_escapes
            .iter()
            .find(|p| p.alloc_node == alloc)
            .expect("the refinement records what it refined");
        assert_eq!(pe.without_refinement, EscapeState::ArgEscape);
        assert_eq!(pe.escape_sites, vec![cold_call]);
        assert_eq!(result.stats.partial_escape, 1);
        assert_eq!(result.stats.arg_escape, 0);

        // A refinement is NOT a licence. Nothing may act on it.
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "a partially-escaping object is still not scalar-replaceable"
        );
        assert!(
            !result.stack_allocatable.contains(&alloc),
            "nor stack-allocatable — the cold path would have to copy it out"
        );
    }

    /// The paired NEGATIVE control: the same escape on a HOT path stays
    /// `ArgEscape`, and one hot site among cold ones is enough to keep it.
    #[test]
    fn a_single_hot_escape_site_defeats_the_partial_classification() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 3,
                num_fields: 0,
            },
            vec![],
        );
        let cold_call = g.add_node(Op::Call, vec![alloc]);
        let _hot_call = g.add_node(Op::Call, vec![alloc]);
        g.mark_cold(cold_call);

        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::ArgEscape),
            "one escape site that is not cold means the object escapes on the \
             hot path"
        );
        assert!(result.partial_escapes.is_empty());
        assert_eq!(result.stats.partial_escape, 0);
        assert_eq!(result.stats.arg_escape, 1);
    }

    /// With no cold information at all — the default — the classification is
    /// exactly what it was before partial escape existed.
    #[test]
    fn no_cold_information_means_no_partial_classification() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 3,
                num_fields: 0,
            },
            vec![],
        );
        let _call = g.add_node(Op::Call, vec![alloc]);
        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&alloc),
            Some(&EscapeState::ArgEscape)
        );
        assert!(result.partial_escapes.is_empty());
    }

    /// A value written into a holder this analysis cannot NAME — a `putstatic`
    /// base, or any opaque reference with no points-to entry — must escape
    /// globally.
    ///
    /// Before this rule the store-escape check read the holder's state with
    /// `get_escape`, which reports an unseen node as `NoEscape`, so the rule
    /// simply did not fire: an object published into a **static field** stayed
    /// `NoEscape`, hence scalar-replaceable and lock-elidable, while being
    /// reachable from every thread in the VM.
    #[test]
    fn object_stored_into_an_unnameable_holder_is_global_escape() {
        let mut g = Graph::new();
        // The static-area base: not an allocation, not a Param — opaque.
        let statics = g.add_node(Op::Other, vec![]);
        let obj = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let _putstatic = g.add_node(Op::Store(0), vec![statics, obj]);

        let result = analyze_escapes(&g);
        assert_eq!(
            result.escape_states.get(&obj),
            Some(&EscapeState::GlobalEscape),
            "a write to an unnameable destination is a write to the global heap"
        );
        assert!(result.scalar_replaceable.is_empty());
        assert!(result.elide_locks.is_empty());
    }

    /// An object passed to an unknown call escapes. It is `ArgEscape`, not
    /// `GlobalEscape` — that is what the middle lattice element is FOR (the
    /// callee can reach it; the global heap cannot, unless the callee publishes
    /// it, which is the callee's own analysis). Either way it is not
    /// replaceable.
    #[test]
    fn object_passed_to_an_unknown_call_escapes_and_is_not_replaced() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let _call = g.add_node(Op::Call, vec![alloc]);

        let result = analyze_escapes(&g);
        let state = *result.escape_states.get(&alloc).expect("classified");
        assert!(state.may_escape());
        assert_eq!(state, EscapeState::ArgEscape);
        assert!(result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc));
    }

    /// The paired POSITIVE for both escape rules: an object that is neither
    /// published nor passed anywhere stays confined and IS replaced.
    #[test]
    fn a_purely_local_object_stays_confined_and_is_replaced() {
        let mut g = Graph::new();
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let load = g.add_node(Op::Load(0), vec![alloc]);

        let result = analyze_escapes(&g);
        assert!(result.escape_states[&alloc].is_confined());
        let sr = result
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == alloc)
            .expect("MUST replace");
        assert_eq!(sr.load_value(load), LoadResolution::Value(c1));
    }

    /// The four state counters partition the allocations — a `PartialEscape`
    /// object is counted once, in its own bucket.
    #[test]
    fn state_counters_partition_the_allocations() {
        let mut g = Graph::new();
        let local = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            vec![],
        );
        let arg = g.add_node(
            Op::New {
                class_id: 2,
                num_fields: 0,
            },
            vec![],
        );
        let cold = g.add_node(
            Op::New {
                class_id: 3,
                num_fields: 0,
            },
            vec![],
        );
        let global = g.add_node(
            Op::New {
                class_id: 4,
                num_fields: 0,
            },
            vec![],
        );
        let _ = local;
        let _arg_call = g.add_node(Op::Call, vec![arg]);
        let cold_call = g.add_node(Op::Call, vec![cold]);
        g.mark_cold(cold_call);
        g.nodes[1].inputs.push(global); // Return
        g.nodes[global].uses.push(1);

        let s = analyze_escapes(&g).stats;
        assert_eq!(s.total_allocations, 4);
        assert_eq!(s.no_escape, 1);
        assert_eq!(s.partial_escape, 1);
        assert_eq!(s.arg_escape, 1);
        assert_eq!(s.global_escape, 1);
        assert_eq!(
            s.no_escape + s.partial_escape + s.arg_escape + s.global_escape,
            s.total_allocations
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Lock elimination and coarsening
    // ══════════════════════════════════════════════════════════════════
    //
    // Every "MUST NOT" below is paired with the "MUST" that proves the refusal
    // is a refusal and not an inability.

    /// A fresh confined object with `num_fields` fields.
    fn alloc(g: &mut Graph, class_id: u32, num_fields: usize) -> NodeId {
        g.add_node(
            Op::New {
                class_id,
                num_fields,
            },
            vec![],
        )
    }

    /// The monitor nodes of `object`, ascending — what the graph *still* holds
    /// after a transform, independent of any plan.
    fn live_monitors_of(g: &Graph, object: NodeId) -> Vec<NodeId> {
        (0..g.nodes.len())
            .filter(|&id| matches!(g.nodes[id].op, Op::MonitorEnter | Op::MonitorExit))
            .filter(|&id| monitor_object(g, id) == Some(object))
            .collect()
    }

    // ── (a) Lock elision ──────────────────────────────────────────────

    /// MUST: a confined object's monitor is elided, and it is offered as ONE
    /// all-or-nothing plan naming every monitor on the object.
    #[test]
    fn confined_object_lock_is_elided_as_one_complete_plan() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 1);
        let c = g.add_node(Op::Const(1), vec![]);
        let enter = g.add_node(Op::MonitorEnter, vec![o]);
        let _st = g.add_node(Op::Store(0), vec![o, c]);
        let exit = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert_eq!(r.escape_states.get(&o), Some(&EscapeState::NoEscape));
        assert_eq!(r.lock_elisions.len(), 1);
        assert_eq!(r.lock_elisions[0].object, o);
        assert_eq!(
            r.lock_elisions[0].monitors,
            vec![enter, exit],
            "the plan names EVERY monitor on the object — that is what makes \
             it all-or-nothing"
        );
        assert_eq!(r.elide_locks, vec![enter, exit]);
        assert_eq!(r.stats.locks_elided, 2);
        assert_eq!(r.stats.lock_objects_elided, 1);
        assert!(r.lock_refusals.is_empty());
    }

    /// MUST NOT: `wait()` requires the monitor to actually be held. Eliding it
    /// turns a legal wait into `IllegalMonitorStateException`, and there is no
    /// depth left to release to or reacquire at.
    ///
    /// Paired MUST: the identical graph without the `wait` elides.
    #[test]
    fn object_with_wait_is_never_elided() {
        let build = |with_wait: bool| {
            let mut g = Graph::new();
            let o = alloc(&mut g, 1, 0);
            let _enter = g.add_node(Op::MonitorEnter, vec![o]);
            if with_wait {
                g.add_node(Op::MonitorWait, vec![o]);
            }
            let _exit = g.add_node(Op::MonitorExit, vec![o]);
            (g, o)
        };

        let (g, o) = build(true);
        let r = analyze_escapes(&g);
        assert_eq!(
            r.escape_states.get(&o),
            Some(&EscapeState::NoEscape),
            "waiting on a local object does not publish it — this is not an \
             escape question"
        );
        assert!(
            r.elide_locks.is_empty(),
            "MUST NOT elide a waited-on monitor"
        );
        assert!(r.lock_elisions.is_empty());
        assert!(r.lock_coarsening.is_empty());
        assert!(r.lock_refusals.contains(&(o, LockRefusal::WaitOrNotify)));

        let (g, o) = build(false);
        let r = analyze_escapes(&g);
        assert_eq!(
            r.lock_elisions.len(),
            1,
            "MUST elide once nothing waits on the monitor"
        );
        assert_eq!(r.lock_elisions[0].object, o);
    }

    /// `notify`/`notifyAll` are the same rule: they too throw when the monitor
    /// is not held, and a thread they could wake is a thread that reached the
    /// object.
    #[test]
    fn object_with_notify_is_never_elided() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let _enter = g.add_node(Op::MonitorEnter, vec![o]);
        let _notify = g.add_node(Op::MonitorNotify, vec![o]);
        let _exit = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert!(r.elide_locks.is_empty());
        assert!(r.lock_refusals.contains(&(o, LockRefusal::WaitOrNotify)));
    }

    /// A `wait` on a **caller-supplied** reference cannot be a wait on an
    /// object this frame allocated and never published, so it must not block
    /// that object's elision. The `Foreign` half of `OperandOrigin` is what
    /// keeps `synchronized (local) {} … this.wait();` optimisable.
    #[test]
    fn a_wait_on_a_foreign_reference_does_not_block_a_local_object() {
        let mut g = Graph::new();
        let this = g.add_node(Op::Param(0), vec![]);
        let o = alloc(&mut g, 1, 0);
        let enter = g.add_node(Op::MonitorEnter, vec![o]);
        let exit = g.add_node(Op::MonitorExit, vec![o]);
        let _wait = g.add_node(Op::MonitorWait, vec![this]);

        let r = analyze_escapes(&g);
        assert_eq!(r.elide_locks, vec![enter, exit]);
        assert!(!r
            .lock_refusals
            .iter()
            .any(|&(_, why)| why == LockRefusal::WaitOrNotify));
    }

    /// MUST NOT: a monitor whose operand may be either a local allocation or a
    /// caller-supplied reference is attributable to neither, and it poisons
    /// **every** lock plan in the method — including the unrelated object's,
    /// whose monitors would otherwise be elided.
    ///
    /// A singleton points-to set would have said "this is `a`" and elided a
    /// lock that, on the parameter path, guards an object other threads share.
    ///
    /// Paired MUST: replace the parameter input with the allocation itself and
    /// both objects elide.
    #[test]
    fn an_unattributable_monitor_operand_poisons_every_lock_plan() {
        let build = |mix_in_param: bool| {
            let mut g = Graph::new();
            let p = g.add_node(Op::Param(0), vec![]);
            let a = alloc(&mut g, 1, 0);
            let b = alloc(&mut g, 2, 0);
            let other = if mix_in_param { p } else { a };
            let phi = g.add_node(Op::Phi, vec![a, other]);
            let amb = g.add_node(Op::MonitorEnter, vec![phi]);
            let _ambx = g.add_node(Op::MonitorExit, vec![phi]);
            let _be = g.add_node(Op::MonitorEnter, vec![b]);
            let _bx = g.add_node(Op::MonitorExit, vec![b]);
            (g, a, b, amb)
        };

        let (g, _a, b, amb) = build(true);
        let r = analyze_escapes(&g);
        assert!(
            r.elide_locks.is_empty(),
            "MUST NOT elide anything while one monitor is unattributable — \
             including object {b}'s own, perfectly confined, lock"
        );
        assert!(r.lock_coarsening.is_empty());
        assert!(r
            .lock_refusals
            .contains(&(amb, LockRefusal::AmbiguousMonitorOperand)));

        let (g, a, b, _) = build(false);
        let r = analyze_escapes(&g);
        let objects: Vec<NodeId> = r.lock_elisions.iter().map(|p| p.object).collect();
        assert!(
            objects.contains(&a) && objects.contains(&b),
            "MUST elide both once every operand names exactly one allocation"
        );
    }

    /// MUST NOT: an escaping object's monitor is real. Another thread can
    /// contend on it, so both transforms are refused.
    #[test]
    fn escaping_object_lock_is_neither_elided_nor_coarsened() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        g.nodes[1].inputs.push(o); // Return
        g.nodes[o].uses.push(1);
        let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _c = g.add_node(Op::Const(0), vec![]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert_eq!(r.escape_states.get(&o), Some(&EscapeState::GlobalEscape));
        assert!(r.elide_locks.is_empty());
        assert!(
            r.lock_coarsening.is_empty(),
            "MUST NOT coarsen an escaping object: extending its critical \
             section can block a thread that wanted the monitor in the gap"
        );
        assert!(r.lock_refusals.contains(&(o, LockRefusal::ObjectEscapes)));
    }

    // ── Recursive locking ─────────────────────────────────────────────

    /// A reentrant `synchronized (o) { synchronized (o) { … } }` round-trips:
    /// the regions pair at the right depths, the elision offer names all four
    /// monitors, and applying it leaves an object with no monitors at all.
    #[test]
    fn recursive_locking_round_trips() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let x1 = g.add_node(Op::MonitorExit, vec![o]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);

        let regions = lock_regions(&g, o).expect("a balanced nest pairs up");
        assert_eq!(regions.len(), 2);
        assert_eq!(
            regions[0],
            LockRegion {
                enter: e1,
                exit: x2,
                depth: 0
            },
            "the OUTER region is e1..x2"
        );
        assert_eq!(
            regions[1],
            LockRegion {
                enter: e2,
                exit: x1,
                depth: 1
            },
            "the INNER region is e2..x1 — pairing by nesting, not by order"
        );

        let r = analyze_escapes(&g);
        assert_eq!(r.lock_elisions.len(), 1);
        assert_eq!(r.lock_elisions[0].monitors, vec![e1, e2, x1, x2]);

        let plan = r.lock_elisions[0].clone();
        assert!(apply_lock_elision_plan(&mut g, &plan));
        assert!(
            live_monitors_of(&g, o).is_empty(),
            "the whole nest goes, so the depth is 0 before and after"
        );
        assert!(lock_regions(&g, o).expect("still balanced").is_empty());
    }

    /// Removing the INNER pair of a reentrant nest is legal (one level of
    /// reentry disappears, the outer region is untouched). Removing `e2` and
    /// `x2` — which are not a pair — is not: it would silently narrow the outer
    /// region. Both requests are "balanced" by a naive depth count; only the
    /// region-pairing check tells them apart.
    #[test]
    fn partial_elision_is_accepted_only_when_whole_regions_go() {
        let build = || {
            let mut g = Graph::new();
            let o = alloc(&mut g, 1, 0);
            let e1 = g.add_node(Op::MonitorEnter, vec![o]);
            let e2 = g.add_node(Op::MonitorEnter, vec![o]);
            let x1 = g.add_node(Op::MonitorExit, vec![o]);
            let x2 = g.add_node(Op::MonitorExit, vec![o]);
            (g, o, e1, e2, x1, x2)
        };

        let (mut g, o, e1, e2, x1, x2) = build();
        assert!(
            apply_lock_elision(&mut g, &[e2, x1]),
            "MUST: the inner region is a whole region"
        );
        assert_eq!(live_monitors_of(&g, o), vec![e1, x2]);

        let (mut g, o, e1, e2, x1, x2) = build();
        assert!(
            !apply_lock_elision(&mut g, &[e2, x2]),
            "MUST NOT: e2 pairs with x1, not x2"
        );
        assert_eq!(
            live_monitors_of(&g, o),
            vec![e1, e2, x1, x2],
            "a refused request leaves the graph completely untouched"
        );
    }

    /// THE PARTIAL-APPLICATION GUARD. `apply_ea_to_ir` filters `elide_locks`
    /// node by node and can hand back a strict subset; a lone `monitorexit`
    /// throws `IllegalMonitorStateException` and a lone `monitorenter` leaks the
    /// monitor. The applier refuses rather than obeys.
    #[test]
    fn a_half_applied_elision_request_is_refused_whole() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let enter = g.add_node(Op::MonitorEnter, vec![o]);
        let exit = g.add_node(Op::MonitorExit, vec![o]);

        assert!(
            !apply_lock_elision(&mut g, &[enter]),
            "MUST NOT drop the enter and keep the exit"
        );
        assert!(!apply_lock_elision(&mut g, &[exit]));
        assert_eq!(g.nodes[enter].op, Op::MonitorEnter);
        assert_eq!(g.nodes[exit].op, Op::MonitorExit);

        assert!(
            apply_lock_elision(&mut g, &[enter, exit]),
            "MUST apply the complete pair"
        );
        assert_eq!(g.nodes[enter].op, Op::Dead);
        assert_eq!(g.nodes[exit].op, Op::Dead);
    }

    /// An unbalanced monitor sequence is not a sequence this analysis
    /// understands, and `lock_regions` says so rather than guessing.
    #[test]
    fn an_unbalanced_monitor_sequence_is_refused() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let _e = g.add_node(Op::MonitorEnter, vec![o]);
        assert_eq!(lock_regions(&g, o), Err(LockRefusal::UnbalancedMonitors));

        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let _x = g.add_node(Op::MonitorExit, vec![o]);
        assert_eq!(lock_regions(&g, o), Err(LockRefusal::UnbalancedMonitors));
    }

    // ── (b) Lock coarsening ───────────────────────────────────────────

    /// MUST: two adjacent regions on a confined object, separated by pure
    /// arithmetic, merge — and applying the plan leaves exactly one balanced
    /// region.
    #[test]
    fn two_adjacent_regions_on_a_confined_object_are_coarsened() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 1);
        let c1 = g.add_node(Op::Const(1), vec![]);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _s1 = g.add_node(Op::Store(0), vec![o, c1]);
        let x1 = g.add_node(Op::MonitorExit, vec![o]);
        // The gap: pure, total, no safepoint, no throw.
        let c2 = g.add_node(Op::Const(2), vec![]);
        let sum = g.add_node(Op::Add, vec![c1, c2]);
        let e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _s2 = g.add_node(Op::Store(0), vec![o, sum]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert_eq!(r.lock_coarsening.len(), 1);
        assert_eq!(r.stats.locks_coarsened, 1);
        let plan = r.lock_coarsening[0];
        assert_eq!(plan.object, o);
        assert_eq!(plan.surviving_enter(), e1);
        assert_eq!(plan.removed_exit(), x1);
        assert_eq!(plan.removed_enter(), e2);
        assert_eq!(plan.surviving_exit(), x2);

        assert!(apply_lock_coarsening(&mut g, &plan));
        assert_eq!(g.nodes[x1].op, Op::Dead);
        assert_eq!(g.nodes[e2].op, Op::Dead);
        assert_eq!(
            lock_regions(&g, o).expect("still balanced"),
            vec![LockRegion {
                enter: e1,
                exit: x2,
                depth: 0
            }],
            "one region, entered once and exited once"
        );
    }

    /// MUST NOT: a throw between the two regions. In the source program the
    /// monitor is NOT held at the throw, so unwinding releases nothing;
    /// coarsening would make the compiled frame hold it there.
    ///
    /// The preservation argument is by exclusion, and this test states it as
    /// such: the plan is refused, the two regions survive untouched, and the
    /// throw still lies in the unlocked window between them.
    ///
    /// Paired MUST: replace the throw with an `Add` and the merge happens.
    #[test]
    fn a_throw_between_two_regions_refuses_the_merge_and_still_unlocks() {
        let build = |throwing: bool| {
            let mut g = Graph::new();
            let p = g.add_node(Op::Param(0), vec![]);
            let o = alloc(&mut g, 1, 0);
            let e1 = g.add_node(Op::MonitorEnter, vec![o]);
            let x1 = g.add_node(Op::MonitorExit, vec![o]);
            let gap = if throwing {
                g.add_node(Op::Throw, vec![p])
            } else {
                g.add_node(Op::Add, vec![p, p])
            };
            let e2 = g.add_node(Op::MonitorEnter, vec![o]);
            let x2 = g.add_node(Op::MonitorExit, vec![o]);
            (g, o, e1, x1, gap, e2, x2)
        };

        let (mut g, o, e1, x1, throw, e2, x2) = build(true);
        let r = analyze_escapes(&g);
        assert!(
            r.lock_coarsening.is_empty(),
            "MUST NOT coarsen across a node that can unwind monitors"
        );
        assert!(r.lock_refusals.contains(&(o, LockRefusal::MayThrowInGap)));

        // Nothing to apply, so unlock-on-throw is bit-for-bit the original's:
        // the throw sits strictly inside the window where `o` is unlocked.
        for plan in &r.lock_coarsening {
            apply_lock_coarsening(&mut g, plan);
        }
        let regions = lock_regions(&g, o).expect("balanced");
        assert_eq!(
            regions,
            vec![
                LockRegion {
                    enter: e1,
                    exit: x1,
                    depth: 0
                },
                LockRegion {
                    enter: e2,
                    exit: x2,
                    depth: 0
                }
            ]
        );
        assert!(
            throw > regions[0].exit && throw < regions[1].enter,
            "the throw is in the unlocked gap, exactly where the source put it"
        );

        let (g, _o, _e1, _x1, _gap, _e2, _x2) = build(false);
        let r = analyze_escapes(&g);
        assert_eq!(
            r.lock_coarsening.len(),
            1,
            "MUST coarsen once the gap cannot throw"
        );
    }

    /// MUST NOT: a deopt point in the gap. The interpreter frame rebuilt there
    /// must NOT hold the monitor (the original had already run the first
    /// `monitorexit`), while the coarsened compiled frame does. That
    /// reconstruction is provably wrong, not merely unproven, so the merge is
    /// refused outright rather than repaired.
    ///
    /// Paired MUST: safepoints *inside* either region are fine — coarsening
    /// changes the held-monitor set only in the gap, so those frames
    /// reconstruct exactly the monitor set they always did.
    #[test]
    fn a_deopt_point_in_the_gap_refuses_the_merge_but_one_inside_a_region_does_not() {
        // Safepoint in the GAP.
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _sp = g.add_node(Op::Safepoint, vec![]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert!(
            r.lock_coarsening.is_empty(),
            "MUST NOT coarsen across a deopt point whose monitor set would be \
             wrong after the merge"
        );
        assert!(r.lock_refusals.contains(&(o, LockRefusal::SafepointInGap)));

        // Safepoints INSIDE both regions, gap clean.
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _sp1 = g.add_node(Op::Safepoint, vec![]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _c = g.add_node(Op::Const(0), vec![]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _sp2 = g.add_node(Op::Safepoint, vec![]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert_eq!(
            r.lock_coarsening.len(),
            1,
            "MUST coarsen: a deopt inside a region holds the same monitor set \
             before and after the merge"
        );
        assert_eq!(r.lock_coarsening[0].surviving_enter(), e1);
        assert_eq!(r.lock_coarsening[0].surviving_exit(), x2);
    }

    /// MUST NOT: a call in the gap. It is a safepoint, it can throw, and it is
    /// how `Thread.holdsLock` and every blocking operation reach the gap — the
    /// three ways the unlocked state is observable.
    #[test]
    fn a_call_in_the_gap_refuses_the_merge() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _call = g.add_node(Op::Call, vec![]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert!(r.lock_coarsening.is_empty());
        assert!(r.lock_refusals.contains(&(o, LockRefusal::MayThrowInGap)));
    }

    /// A `monitorexit`/`monitorenter` on a DIFFERENT object in the gap is an
    /// observation of the lock state, and it is also the shape where a naive
    /// merge could invert two lock orders. Refused.
    #[test]
    fn a_foreign_monitor_in_the_gap_refuses_the_merge() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let p = alloc(&mut g, 2, 0);
        let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _pe = g.add_node(Op::MonitorEnter, vec![p]);
        let _px = g.add_node(Op::MonitorExit, vec![p]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert!(r.lock_coarsening.iter().all(|plan| plan.object != o));
        assert!(r.lock_refusals.contains(&(o, LockRefusal::ObservableGap)));
    }

    /// Without a dominance proof "adjacent" and "between" are not answerable,
    /// so coarsening is refused wholesale — while elision, which removes every
    /// monitor and therefore needs no ordering, still fires.
    #[test]
    fn a_branch_refuses_coarsening_but_not_elision() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let cond = g.add_node(Op::Const(0), vec![]);
        let _iff = g.add_node(Op::If, vec![cond]);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let x1 = g.add_node(Op::MonitorExit, vec![o]);
        let e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);

        assert!(!program_order_proves_dominance(&g));
        let r = analyze_escapes(&g);
        assert!(r.lock_coarsening.is_empty());
        assert!(r
            .lock_refusals
            .contains(&(o, LockRefusal::NoDominanceProof)));
        assert_eq!(
            r.elide_locks,
            vec![e1, x1, e2, x2],
            "removing EVERY monitor is balance-preserving on every path, so \
             elision needs no ordering proof"
        );
    }

    /// Coarsening merges only *outermost* regions. An inner recursive region is
    /// not adjacent to anything: merging across it would change the reentry
    /// depth the interpreter has to rebuild.
    #[test]
    fn coarsening_pairs_only_outermost_regions() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]); // depth 1
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let xo = g.add_node(Op::MonitorExit, vec![o]);
        let _c = g.add_node(Op::Const(0), vec![]);
        let e3 = g.add_node(Op::MonitorEnter, vec![o]);
        let x3 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert_eq!(r.lock_coarsening.len(), 1);
        let plan = r.lock_coarsening[0];
        assert_eq!(plan.surviving_enter(), e1);
        assert_eq!(plan.removed_exit(), xo, "the OUTER exit, not the inner one");
        assert_eq!(plan.removed_enter(), e3);
        assert_eq!(plan.surviving_exit(), x3);
    }

    /// Three adjacent regions produce two chained plans that SHARE a region.
    /// Each plan is individually balance-preserving, and the re-verification in
    /// [`apply_lock_coarsening`] makes overlapping plans mutually exclusive
    /// rather than compounding: whichever runs first wins, the other is dropped.
    /// The graph is balanced after every subset, in every order.
    ///
    /// Merging all three is reached the same way a synchronized object reaches
    /// scalar replacement — by re-running the analysis on the mutated graph.
    #[test]
    fn chained_coarsening_plans_are_safe_in_any_subset() {
        let build = || {
            let mut g = Graph::new();
            let o = alloc(&mut g, 1, 0);
            for _ in 0..3 {
                g.add_node(Op::MonitorEnter, vec![o]);
                g.add_node(Op::MonitorExit, vec![o]);
                g.add_node(Op::Const(0), vec![]);
            }
            (g, o)
        };

        let (g, _o) = build();
        let plans = analyze_escapes(&g).lock_coarsening;
        assert_eq!(plans.len(), 2, "three regions ⇒ two adjacent pairs");

        // Either plan alone: three regions become two.
        for which in 0..2 {
            let (mut g, o) = build();
            assert!(apply_lock_coarsening(&mut g, &plans[which]));
            assert_eq!(
                lock_regions(&g, o).expect("balanced").len(),
                2,
                "applying one of two plans leaves two balanced regions"
            );
        }

        // Both, in either order: the second overlaps the first and is dropped.
        for order in [[0usize, 1usize], [1, 0]] {
            let (mut g, o) = build();
            assert!(apply_lock_coarsening(&mut g, &plans[order[0]]));
            assert!(
                !apply_lock_coarsening(&mut g, &plans[order[1]]),
                "the two plans share a region; the second no longer describes \
                 the graph and is refused rather than half-applied"
            );
            let regions = lock_regions(&g, o).expect("balanced");
            assert_eq!(regions.len(), 2);

            // Re-analysing the mutated graph offers the now-adjacent pair.
            let again = analyze_escapes(&g).lock_coarsening;
            assert_eq!(again.len(), 1);
            assert!(apply_lock_coarsening(&mut g, &again[0]));
            assert_eq!(
                lock_regions(&g, o).expect("balanced").len(),
                1,
                "one region, entered once and exited once"
            );
        }
    }

    /// Coarsening depends on NO elision having landed, and re-verifies before
    /// mutating. A plan whose pair an elision already removed is a no-op; a
    /// plan whose *outer* monitor a partial elision removed is refused, so
    /// coarsening never compounds an imbalance it did not create.
    #[test]
    fn coarsening_is_safe_under_partial_application_of_elision() {
        let build = || {
            let mut g = Graph::new();
            let o = alloc(&mut g, 1, 0);
            let e1 = g.add_node(Op::MonitorEnter, vec![o]);
            let x1 = g.add_node(Op::MonitorExit, vec![o]);
            let _c = g.add_node(Op::Const(0), vec![]);
            let e2 = g.add_node(Op::MonitorEnter, vec![o]);
            let x2 = g.add_node(Op::MonitorExit, vec![o]);
            (g, o, e1, x1, e2, x2)
        };

        let (g, _o, _e1, _x1, _e2, _x2) = build();
        let plan = analyze_escapes(&g).lock_coarsening[0];

        // The elision landed in full: the pair is already gone.
        let (mut g, o, _e1, _x1, _e2, _x2) = build();
        let r = analyze_escapes(&g);
        assert!(apply_lock_elision_plan(&mut g, &r.lock_elisions[0]));
        assert!(
            !apply_lock_coarsening(&mut g, &plan),
            "no monitors left: the plan is dropped, not applied twice"
        );
        assert!(live_monitors_of(&g, o).is_empty());

        // A partial elision took the OUTER enter only — the hazard case.
        let (mut g, _o, e1, x1, e2, x2) = build();
        g.nodes[e1].op = Op::Dead;
        g.nodes[e1].inputs.clear();
        assert!(
            !apply_lock_coarsening(&mut g, &plan),
            "MUST NOT merge onto an enter that is no longer there"
        );
        assert_eq!(g.nodes[x1].op, Op::MonitorExit);
        assert_eq!(g.nodes[e2].op, Op::MonitorEnter);
        assert_eq!(g.nodes[x2].op, Op::MonitorExit);
    }

    /// Field accesses on confined allocations of this graph ARE admitted in a
    /// gap: they cannot fault (the holder is a `New` of this frame, so never
    /// null) and no other thread can observe them.
    #[test]
    fn a_gap_of_confined_field_accesses_is_transparent() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 1);
        let other = alloc(&mut g, 2, 1);
        let c = g.add_node(Op::Const(7), vec![]);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _st = g.add_node(Op::Store(0), vec![other, c]);
        let _ld = g.add_node(Op::Load(0), vec![other]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        let plan = r
            .lock_coarsening
            .iter()
            .find(|p| p.object == o)
            .expect("a confined-only gap is transparent");
        assert_eq!(plan.surviving_enter(), e1);
        assert_eq!(plan.surviving_exit(), x2);
    }

    /// …but a field access on a **caller-supplied** holder is not: it can raise
    /// `NullPointerException`, which is a monitor-unwinding event, and other
    /// threads can see the write.
    #[test]
    fn a_gap_touching_a_foreign_holder_is_not_transparent() {
        let mut g = Graph::new();
        let this = g.add_node(Op::Param(0), vec![]);
        let o = alloc(&mut g, 1, 0);
        let c = g.add_node(Op::Const(7), vec![]);
        let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _st = g.add_node(Op::Store(0), vec![this, c]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x2 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert!(r.lock_coarsening.is_empty());
        assert!(r.lock_refusals.contains(&(o, LockRefusal::ObservableGap)));
    }

    /// Coarsening does not disturb the identity rule: a live monitor is still
    /// an identity observation, so the object is not scalar-replaced until the
    /// monitors are actually gone. The documented two-phase route still works
    /// through the coarsened graph.
    #[test]
    fn coarsening_then_elision_still_unlocks_scalar_replacement() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 1);
        let c = g.add_node(Op::Const(9), vec![]);
        let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _st = g.add_node(Op::Store(0), vec![o, c]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _k = g.add_node(Op::Const(0), vec![]);
        let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x2 = g.add_node(Op::MonitorExit, vec![o]);
        let load = g.add_node(Op::Load(0), vec![o]);

        let first = analyze_escapes(&g);
        assert!(
            first.scalar_replaceable.iter().all(|s| s.alloc_node != o),
            "MUST NOT replace while a monitor observes the identity"
        );
        assert_eq!(first.stats.identity_blocked, 1);

        let plan = first.lock_coarsening[0];
        assert!(apply_lock_coarsening(&mut g, &plan));
        let second = analyze_escapes(&g);
        assert!(
            second.scalar_replaceable.iter().all(|s| s.alloc_node != o),
            "two monitors survive the merge — still observed"
        );

        assert!(apply_lock_elision_plan(&mut g, &second.lock_elisions[0]));
        let third = analyze_escapes(&g);
        let sr = third
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == o)
            .expect("MUST replace once every monitor is dead");
        assert_eq!(sr.load_value(load), LoadResolution::Value(c));
    }

    /// The refusal counters are the measurement that says what failing closed
    /// costs, exactly as `identity_blocked` does for scalar replacement.
    #[test]
    fn lock_refusals_are_counted_and_deduplicated() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        g.nodes[1].inputs.push(o); // Return: escapes
        g.nodes[o].uses.push(1);
        let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let _x1 = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert_eq!(
            r.lock_refusals,
            vec![(o, LockRefusal::ObjectEscapes)],
            "the elision and coarsening finders both refuse it; the record is \
             deduplicated"
        );
        assert_eq!(r.stats.locks_refused, 1);
        assert_eq!(r.stats.locks_elided, 0);
        assert_eq!(r.stats.locks_coarsened, 0);
    }

    // ── Alias-analysis audit (wave: alias soundness) ──────────────────

    /// SELF-REFERENCE. `o.f0 = o;` makes the allocation recoverable by loading
    /// its own field, and the use walk in `find_scalar_replacements` never
    /// visits uses made through that recovered reference — it only walks the
    /// allocation's own uses and the transparent φ copies it accepts.
    ///
    /// Before the self-reference gate this object was reported replaceable:
    /// `apply_scalar_replacement` would delete the allocation and leave
    /// `p.f1 = 7` writing through an `Op::Dead` holder.
    #[test]
    fn an_object_stored_into_its_own_field_is_not_scalar_replaced() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 2);
        let c7 = g.add_node(Op::Const(7), vec![]);
        // o.f0 = o
        let _self_store = g.add_node(Op::Store(0), vec![o, o]);
        // p = o.f0   — `p` is `o`, but it is a Load node, not a use of `o`
        let p = g.add_node(Op::Load(0), vec![o]);
        // p.f1 = 7   — a real store into the object, invisible to the walk
        let _via_p = g.add_node(Op::Store(1), vec![p, c7]);

        let r = analyze_escapes(&g);
        assert_eq!(
            r.escape_states.get(&o),
            Some(&EscapeState::NoEscape),
            "the object genuinely does not escape — this is an ALIAS question, \
             not a reachability one, which is why the escape lattice alone \
             cannot refuse it"
        );
        assert!(
            r.scalar_replaceable.iter().all(|s| s.alloc_node != o),
            "a reference to the object is recoverable from its own field, so \
             there are uses of it this analysis never saw"
        );
    }

    /// The paired NEGATIVE control: an allocation stored into a *different*
    /// object's field is still refused (it is published), and an allocation
    /// whose fields hold only non-references is still replaced. The gate must
    /// not have swallowed the ordinary case.
    #[test]
    fn the_self_reference_gate_does_not_refuse_an_ordinary_object() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 1);
        let c9 = g.add_node(Op::Const(9), vec![]);
        let _st = g.add_node(Op::Store(0), vec![o, c9]);
        let load = g.add_node(Op::Load(0), vec![o]);

        let r = analyze_escapes(&g);
        let sr = r
            .scalar_replaceable
            .iter()
            .find(|s| s.alloc_node == o)
            .expect("a field holding a constant is not a self-reference");
        assert_eq!(sr.load_value(load), LoadResolution::Value(c9));
    }

    /// UNMODELLED USE. `Op::Other` is the bridge's catch-all, and two of the
    /// `ir::Op`s that land in it — `LambdaIntToDouble` and `Guard` — publish or
    /// republish the reference they are handed.
    ///
    /// `find_scalar_replacements` refuses such an object at its use-walk
    /// catch-all, but `find_lock_elision_plans` walks no uses: its E2 test is
    /// `get_escape(object).is_confined()`. Before this rule the object looked
    /// confined and both monitors were elided.
    #[test]
    fn an_unmodelled_use_blocks_lock_elision() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let _opaque = g.add_node(Op::Other, vec![o]);
        let _enter = g.add_node(Op::MonitorEnter, vec![o]);
        let _exit = g.add_node(Op::MonitorExit, vec![o]);

        let r = analyze_escapes(&g);
        assert!(
            r.escape_states
                .get(&o)
                .copied()
                .unwrap_or(EscapeState::NoEscape)
                .may_escape(),
            "a reference handed to a node this module cannot model must not be \
             reported confined"
        );
        assert!(
            r.elide_locks.is_empty(),
            "eliding every monitor on an object an unmodelled node may have \
             published drops a happens-before edge the JMM argument assumes \
             cannot exist"
        );
        assert!(r.scalar_replaceable.iter().all(|s| s.alloc_node != o));
    }

    /// The `Op::Other` rule must key off the *reference* operands only: an
    /// unmodelled node with no reference input (the static-area base shape) is
    /// unchanged, and a node an earlier pass already killed observes nothing.
    #[test]
    fn the_unmodelled_use_rule_ignores_dead_and_referenceless_nodes() {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 1);
        let c = g.add_node(Op::Const(3), vec![]);
        let _st = g.add_node(Op::Store(0), vec![o, c]);
        // A referenceless unmodelled node, and a stale dead use-edge.
        let _bare = g.add_node(Op::Other, vec![c]);
        let stale = g.add_node(Op::Other, vec![o]);
        g.nodes[stale].op = Op::Dead;

        let r = analyze_escapes(&g);
        assert_eq!(r.escape_states.get(&o), Some(&EscapeState::NoEscape));
        assert!(r.scalar_replaceable.iter().any(|s| s.alloc_node == o));
    }
}

/// Regressions from the 2026-09-12 JIT review: a φ that merges the allocation
/// with a reference the untyped EA graph does not list as a "reference
/// producer" must still refuse to be a transparent copy.
#[cfg(test)]
mod phi_foreign_reference_tests {
    use super::*;

    /// `Foo p = c ? new Foo() : null; return p.x;` — the null literal reaches
    /// the EA graph as `Const(0)`. Treating it as "not a reference" folded the
    /// load to the field default (0) instead of throwing NPE.
    #[test]
    fn a_phi_merging_the_allocation_with_null_blocks_scalar_replacement() {
        let mut g = Graph::new();
        let null = g.add_node(Op::Const(0), vec![]);
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let phi = g.add_node(Op::Phi, vec![alloc, null]);
        let _load = g.add_node(Op::Load(0), vec![phi]);

        let result = analyze_escapes(&g);
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "a phi merging the allocation with the null literal must block scalar replacement"
        );
    }

    /// `Foo p = c ? new Foo() : (Foo) o; return p.x;` — the `checkcast` reaches
    /// the EA graph as `Other`.
    #[test]
    fn a_phi_merging_the_allocation_with_a_checkcast_blocks_scalar_replacement() {
        let mut g = Graph::new();
        let param = g.add_node(Op::Param(0), vec![]);
        let cast = g.add_node(Op::Other, vec![param]);
        let alloc = g.add_node(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            vec![],
        );
        let c1 = g.add_node(Op::Const(1), vec![]);
        let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
        let phi = g.add_node(Op::Phi, vec![alloc, cast]);
        let _load = g.add_node(Op::Load(0), vec![phi]);

        let result = analyze_escapes(&g);
        assert!(
            result
                .scalar_replaceable
                .iter()
                .all(|s| s.alloc_node != alloc),
            "a phi merging the allocation with a checkcast result must block scalar replacement"
        );
    }

    /// The null literal merged into a lock operand is a reference too.
    #[test]
    fn a_constant_is_not_assumed_to_produce_no_reference() {
        assert!(!produces_no_reference(&Op::Const(0)));
    }
}
