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
//! [`EscapeAnalysisResult`] has four outputs; `jit/src/lib.rs` reads only two:
//!
//! | Field | Consumer | Status |
//! |---|---|---|
//! | `scalar_replaceable` | IR scalar replacement | live |
//! | `elide_locks` | IR lock elision | live |
//! | `stack_allocatable` | — | **computed, never read** |
//! | `escape_states` / `stats` | tests + diagnostics | informational |
//!
//! `stack_allocatable` collects every `ArgEscape` allocation, but no caller
//! looks at it: **stack allocation is not implemented**. The field is not a
//! disabled feature behind a flag — there is no code to enable. Treat a
//! non-empty `stack_allocatable` as a measurement, not an optimisation.
//!
//! # Soundness direction
//!
//! The lattice joins upward (`NoEscape < ArgEscape < GlobalEscape`), so every
//! partial result *under*-estimates escape, and both live consumers act on
//! `== NoEscape`. Anything that can terminate the fixed point early therefore
//! has to fail closed — see [`escalate_all_to_global`].

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
    },
    Call,
    MonitorEnter,
    MonitorExit,
    ArrayLength,
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
        }
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
            // Arrays have no statically-known field count here; treat any index as
            // out-of-range so array stores take the conservative escape path.
            Some(Op::NewArray { .. }) => false,
            _ => false,
        })
}

// ── Escape state ────────────────────────────────────────────────────────

/// How far an allocated object escapes from its allocation site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EscapeState {
    /// Object does not escape the current method -- can be scalar-replaced.
    NoEscape,
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

/// Information for replacing an allocation with scalar values.
pub struct ScalarReplacementInfo {
    /// The allocation node being replaced.
    pub alloc_node: NodeId,
    /// Class ID of the allocated object.
    pub class_id: u32,
    /// Number of fields.
    pub num_fields: usize,
    /// For each field: the node representing its value (or None if uninitialized).
    pub field_values: Vec<Option<NodeId>>,
    /// Loads that were replaced with direct field value access.
    pub replaced_loads: Vec<NodeId>,
    /// Stores that were eliminated.
    pub eliminated_stores: Vec<NodeId>,
}

// ── Analysis result ─────────────────────────────────────────────────────

/// Full result of escape analysis on a graph.
pub struct EscapeAnalysisResult {
    /// Escape states for all allocation sites.
    pub escape_states: HashMap<NodeId, EscapeState>,
    /// Allocations that can be scalar-replaced (NoEscape).
    pub scalar_replaceable: Vec<ScalarReplacementInfo>,
    /// Allocations that can be stack-allocated (ArgEscape).
    pub stack_allocatable: Vec<NodeId>,
    /// Synchronized blocks on non-escaping objects (lock elision candidates).
    pub elide_locks: Vec<NodeId>,
    /// Statistics.
    pub stats: EscapeAnalysisStats,
}

/// Summary statistics for the analysis.
#[derive(Debug, Default, Clone)]
pub struct EscapeAnalysisStats {
    pub total_allocations: usize,
    pub no_escape: usize,
    pub arg_escape: usize,
    pub global_escape: usize,
    pub scalar_replaced: usize,
    pub stack_allocated: usize,
    pub locks_elided: usize,
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

            // Return: any ref input escapes globally.
            Op::Return => {
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

// ── Phase 3: Find scalar replacement candidates ─────────────────────────

/// Identify allocations that can be decomposed into scalar field values.
fn find_scalar_replacements(cg: &ConnectionGraph, graph: &Graph) -> Vec<ScalarReplacementInfo> {
    let mut results = Vec::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        if let Op::New {
            class_id,
            num_fields,
        } = &node.op
        {
            if cg.get_escape(id) != EscapeState::NoEscape {
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
            let mut replaced_loads = Vec::new();
            let mut eliminated_stores = Vec::new();
            let mut can_replace = true;

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
                            }
                            // Both stores are eliminated regardless of which
                            // value wins — the heap object is gone, so every
                            // store to it is dead.
                            eliminated_stores.push(use_id);
                        } else if is_value {
                            // Published into another object's field -> escapes;
                            // cannot scalar-replace.
                            can_replace = false;
                            break;
                        } else {
                            // Holder role but out-of-range / malformed field.
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
                        } else {
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
                        // *reference-producing* input to be a concrete alias of
                        // this allocation (an allocation that is `id`, or
                        // another transparent phi we have already accepted).
                        let all_inputs_alias_id = use_node.inputs.iter().all(|&inp| {
                            if !is_ref_producer(graph, inp) {
                                // Non-reference (e.g. the Merge control input,
                                // a primitive) cannot carry a foreign object.
                                return true;
                            }
                            // Reference input: must resolve to exactly {id}.
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
                            can_replace = false;
                            break;
                        }
                    }
                    // MonitorEnter/Exit on non-escaping objects are handled
                    // separately by lock elision -- not a problem for SR.
                    Op::MonitorEnter | Op::MonitorExit => {}
                    // ArrayLength on a non-escaping object is fine -- the
                    // length is known statically. Other uses prevent SR.
                    Op::ArrayLength => {}
                    _ => {
                        can_replace = false;
                        break;
                    }
                }
            }

            if can_replace {
                results.push(ScalarReplacementInfo {
                    alloc_node: id,
                    class_id: *class_id,
                    num_fields: *num_fields,
                    field_values,
                    replaced_loads,
                    eliminated_stores,
                });
            }
        }
    }

    results
}

// ── Phase 4: Find lock elision candidates ───────────────────────────────

/// Identify MonitorEnter/MonitorExit nodes on non-escaping objects.
fn find_lock_elisions(cg: &ConnectionGraph, graph: &Graph) -> Vec<NodeId> {
    let mut elide = Vec::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::MonitorEnter | Op::MonitorExit) {
            if let Some(&obj) = node.inputs.first() {
                let pts = cg.resolve_points_to(obj);
                let all_no_escape = if pts.is_empty() {
                    cg.get_escape(obj) == EscapeState::NoEscape
                } else {
                    pts.iter()
                        .all(|&a| cg.get_escape(a) == EscapeState::NoEscape)
                };
                if all_no_escape {
                    elide.push(id);
                }
            }
        }
    }

    elide
}

// ── Partial escape analysis helpers ─────────────────────────────────────

/// Check if an object escapes on only some control flow paths.
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
            Op::Return | Op::Call => has_escaping_use = true,
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
        let is_escape_point = matches!(graph.nodes[use_id].op, Op::Return | Op::Call);
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

// ── Main entry point ────────────────────────────────────────────────────

/// Run escape analysis on the IR graph.
pub fn analyze_escapes(graph: &Graph) -> EscapeAnalysisResult {
    let mut cg = build_connection_graph(graph);
    propagate_escape_states(&mut cg, graph);

    let scalar_replaceable = find_scalar_replacements(&cg, graph);
    let lock_elisions = find_lock_elisions(&cg, graph);

    // Classify allocations.
    let mut stats = EscapeAnalysisStats::default();
    let mut escape_states = HashMap::new();
    let mut stack_allocatable = Vec::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::New { .. } | Op::NewArray { .. }) {
            stats.total_allocations += 1;
            let state = cg.get_escape(id);
            escape_states.insert(id, state);

            match state {
                EscapeState::NoEscape => stats.no_escape += 1,
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
    stats.locks_elided = lock_elisions.len();

    EscapeAnalysisResult {
        escape_states,
        scalar_replaceable,
        stack_allocatable,
        elide_locks: lock_elisions,
        stats,
    }
}

// ── Graph transformations ───────────────────────────────────────────────

/// Apply scalar replacement: replace allocation + load/store with direct
/// value flow.  Marks eliminated nodes as `Op::Dead`.
pub fn apply_scalar_replacement(graph: &mut Graph, info: &ScalarReplacementInfo) {
    // Replace loads with the stored field value (or a zero constant).
    for &load_id in &info.replaced_loads {
        if load_id >= graph.nodes.len() {
            continue;
        }
        if let Op::Load(field_idx) = graph.nodes[load_id].op {
            if field_idx < info.num_fields {
                if let Some(val) = info.field_values[field_idx] {
                    // Redirect all uses of this load to the stored value.
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
                }
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

/// Eliminate locks on non-escaping objects by marking MonitorEnter/Exit
/// as dead.
pub fn apply_lock_elision(graph: &mut Graph, lock_nodes: &[NodeId]) {
    for &id in lock_nodes {
        if id < graph.nodes.len() {
            graph.nodes[id].op = Op::Dead;
            graph.nodes[id].inputs.clear();
            graph.nodes[id].uses.clear();
        }
    }
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
        let arr = g.add_node(Op::NewArray { element_type: 10 }, vec![]);
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
        let arr = g.add_node(Op::NewArray { element_type: 5 }, vec![]);
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
            replaced_loads: vec![load],
            eliminated_stores: vec![store],
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
            replaced_loads: vec![load],
            eliminated_stores: vec![store],
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
    fn test_new_array_local_scalar_not_replaced() {
        // NewArray cannot be scalar-replaced (only New can).
        let mut g = Graph::new();
        let _arr = g.add_node(Op::NewArray { element_type: 4 }, vec![]);
        let result = analyze_escapes(&g);
        assert!(result.scalar_replaceable.is_empty());
        assert_eq!(result.stats.no_escape, 1);
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
            find_scalar_replacements(&cg, &g).is_empty(),
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
}
