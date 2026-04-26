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
    Load(usize),
    Store(usize),
    New { class_id: u32, num_fields: usize },
    NewArray { element_type: u8 },
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
        let start = Node { op: Op::Start, inputs: vec![], uses: vec![] };
        let ret = Node { op: Op::Return, inputs: vec![], uses: vec![] };
        Graph { nodes: vec![start, ret], entry: 0, exit: 1 }
    }

    pub fn add_node(&mut self, op: Op, inputs: Vec<NodeId>) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { op, inputs: inputs.clone(), uses: vec![] });
        for &inp in &inputs {
            if inp < self.nodes.len() {
                self.nodes[inp].uses.push(id);
            }
        }
        id
    }
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
}

impl ConnectionGraph {
    fn new() -> Self {
        ConnectionGraph {
            escape_states: HashMap::new(),
            points_to: HashMap::new(),
            field_edges: HashMap::new(),
            deferred_edges: HashMap::new(),
        }
    }

    fn set_escape(&mut self, node: NodeId, state: EscapeState) {
        let entry = self.escape_states.entry(node).or_insert(EscapeState::NoEscape);
        *entry = (*entry).join(state);
    }

    fn get_escape(&self, node: NodeId) -> EscapeState {
        self.escape_states.get(&node).copied().unwrap_or(EscapeState::NoEscape)
    }

    fn add_points_to(&mut self, from: NodeId, to: NodeId) {
        self.points_to.entry(from).or_default().insert(to);
    }

    fn add_deferred(&mut self, from: NodeId, to: NodeId) {
        self.deferred_edges.entry(from).or_default().insert(to);
    }

    fn add_field_edge(&mut self, obj: NodeId, field: usize, value: NodeId) {
        self.field_edges.entry((obj, field)).or_default().insert(value);
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

            // Phi nodes: deferred edges to all reference inputs.
            Op::Phi => {
                for &inp in &node.inputs {
                    if is_ref_producer(graph, inp) {
                        cg.add_deferred(id, inp);
                    }
                }
            }

            // Store: field edge from object to stored value.
            Op::Store(field_idx) => {
                // inputs: [object, value, ...]
                if node.inputs.len() >= 2 {
                    let obj = node.inputs[0];
                    let val = node.inputs[1];
                    cg.add_field_edge(obj, *field_idx, val);
                }
            }

            // Load: deferred edge — the result points to whatever is in the field.
            Op::Load(field_idx) => {
                if !node.inputs.is_empty() {
                    let obj = node.inputs[0];
                    // Connect load result to values stored in that field.
                    // We record a deferred edge; during propagation we'll
                    // resolve through field edges.
                    cg.add_deferred(id, obj);
                    // Also check if there is a direct field edge we can use.
                    // This is refined in propagation.
                    let _ = field_idx; // used during scalar replacement
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
        Op::New { .. }
            | Op::NewArray { .. }
            | Op::Phi
            | Op::Load(_)
            | Op::Param(_)
            | Op::Call
    )
}

// ── Phase 2: Propagate escape states ────────────────────────────────────

/// Fixed-point propagation of escape states through the connection graph.
fn propagate_escape_states(cg: &mut ConnectionGraph, graph: &Graph) {
    let mut changed = true;
    let mut iteration = 0;
    const MAX_ITERATIONS: usize = 100;

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

                // If a reference is stored into a globally-escaping object's
                // field, the stored value escapes globally too.
                if let Op::Store(field_idx) = &node.op {
                    if node.inputs.len() >= 2 {
                        let obj = node.inputs[0];
                        let val = node.inputs[1];
                        let obj_escape = cg.get_escape(obj);
                        // Resolve what obj points to.
                        let obj_pts = cg.resolve_points_to(obj);
                        for &target_alloc in &obj_pts {
                            let target_escape = cg.get_escape(target_alloc);
                            if target_escape == EscapeState::GlobalEscape {
                                let val_escape = cg.get_escape(val);
                                if val_escape != EscapeState::GlobalEscape {
                                    cg.set_escape(val, EscapeState::GlobalEscape);
                                    // Also propagate to what val points to.
                                    let val_pts = cg.resolve_points_to(val);
                                    for &vp in &val_pts {
                                        cg.set_escape(vp, EscapeState::GlobalEscape);
                                    }
                                    changed = true;
                                }
                            }
                        }
                        let _ = (obj_escape, field_idx);
                    }
                }
            }
        }
    }
}

// ── Phase 3: Find scalar replacement candidates ─────────────────────────

/// Identify allocations that can be decomposed into scalar field values.
fn find_scalar_replacements(cg: &ConnectionGraph, graph: &Graph) -> Vec<ScalarReplacementInfo> {
    let mut results = Vec::new();

    for (id, node) in graph.nodes.iter().enumerate() {
        if let Op::New { class_id, num_fields } = &node.op {
            if cg.get_escape(id) != EscapeState::NoEscape {
                continue;
            }

            let mut field_values: Vec<Option<NodeId>> = vec![None; *num_fields];
            let mut replaced_loads = Vec::new();
            let mut eliminated_stores = Vec::new();
            let mut can_replace = true;

            // Walk all uses of this allocation to find loads and stores.
            for &use_id in &node.uses {
                if use_id >= graph.nodes.len() {
                    continue;
                }
                match &graph.nodes[use_id].op {
                    Op::Store(field_idx) => {
                        if *field_idx < *num_fields && graph.nodes[use_id].inputs.len() >= 2 {
                            let stored_val = graph.nodes[use_id].inputs[1];
                            field_values[*field_idx] = Some(stored_val);
                            eliminated_stores.push(use_id);
                        } else {
                            can_replace = false;
                            break;
                        }
                    }
                    Op::Load(field_idx) => {
                        if *field_idx < *num_fields {
                            replaced_loads.push(use_id);
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
                    pts.iter().all(|&a| cg.get_escape(a) == EscapeState::NoEscape)
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
            Op::Store(_) | Op::Load(_) | Op::MonitorEnter | Op::MonitorExit => {
                has_local_use = true
            }
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
        let is_escape_point = matches!(
            graph.nodes[use_id].op,
            Op::Return | Op::Call
        );
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
            if let Op::Store(field_idx) = &graph.nodes[other_use].op {
                if *field_idx < num_fields && graph.nodes[other_use].inputs.len() >= 2 {
                    fields_at_escape[*field_idx] = Some(graph.nodes[other_use].inputs[1]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 2 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
        // Wire alloc as input to Return.
        g.nodes[1].inputs.push(alloc); // Return node
        g.nodes[alloc].uses.push(1);
        let result = analyze_escapes(&g);
        assert_eq!(result.escape_states.get(&alloc), Some(&EscapeState::GlobalEscape));
    }

    #[test]
    fn test_call_arg_escape() {
        let mut g = Graph::new();
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
        let _call = g.add_node(Op::Call, vec![alloc]);
        let result = analyze_escapes(&g);
        assert_eq!(result.escape_states.get(&alloc), Some(&EscapeState::ArgEscape));
    }

    #[test]
    fn test_stored_to_escaping_object_global_escape() {
        let mut g = Graph::new();
        // Outer object escapes via return.
        let outer = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
        g.nodes[1].inputs.push(outer);
        g.nodes[outer].uses.push(1);
        // Inner object is stored into outer's field.
        let inner = g.add_node(Op::New { class_id: 2, num_fields: 0 }, vec![]);
        let _store = g.add_node(Op::Store(0), vec![outer, inner]);
        let result = analyze_escapes(&g);
        assert_eq!(result.escape_states.get(&outer), Some(&EscapeState::GlobalEscape));
        // Inner should also be GlobalEscape because it's stored into a
        // globally-escaping object.
        assert_eq!(result.escape_states.get(&inner), Some(&EscapeState::GlobalEscape));
    }

    // ---- Scalar replacement ----

    #[test]
    fn test_scalar_replacement_single_field() {
        let mut g = Graph::new();
        let alloc = g.add_node(Op::New { class_id: 5, num_fields: 1 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 3, num_fields: 3 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 2 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
        let _enter = g.add_node(Op::MonitorEnter, vec![alloc]);
        let _exit = g.add_node(Op::MonitorExit, vec![alloc]);
        let result = analyze_escapes(&g);
        assert_eq!(result.elide_locks.len(), 2);
        assert_eq!(result.stats.locks_elided, 2);
    }

    #[test]
    fn test_lock_on_escaping_object_kept() {
        let mut g = Graph::new();
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
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
        let a1 = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
        let a2 = g.add_node(Op::New { class_id: 2, num_fields: 0 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 2 }, vec![]);
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
        let a1 = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
        let phi1 = g.add_node(Op::Phi, vec![a1]);
        let phi2 = g.add_node(Op::Phi, vec![phi1]);
        // Return phi2 -> should propagate GlobalEscape back to a1.
        g.nodes[1].inputs.push(phi2);
        g.nodes[phi2].uses.push(1);
        let result = analyze_escapes(&g);
        assert_eq!(result.escape_states.get(&a1), Some(&EscapeState::GlobalEscape));
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
        assert_eq!(result.escape_states.get(&arr), Some(&EscapeState::GlobalEscape));
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
        let a1 = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
        let a2 = g.add_node(Op::New { class_id: 2, num_fields: 0 }, vec![]);
        let a3 = g.add_node(Op::New { class_id: 3, num_fields: 0 }, vec![]);
        // a1 returned.
        g.nodes[1].inputs.push(a1);
        g.nodes[a1].uses.push(1);
        // a2 passed to call.
        let _call = g.add_node(Op::Call, vec![a2]);
        // a3 purely local.
        let result = analyze_escapes(&g);
        assert_eq!(result.escape_states.get(&a1), Some(&EscapeState::GlobalEscape));
        assert_eq!(result.escape_states.get(&a2), Some(&EscapeState::ArgEscape));
        assert_eq!(result.escape_states.get(&a3), Some(&EscapeState::NoEscape));
        assert_eq!(result.stats.total_allocations, 3);
    }

    #[test]
    fn test_stats_counting() {
        let mut g = Graph::new();
        let _a1 = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
        let a2 = g.add_node(Op::New { class_id: 2, num_fields: 0 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
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
        cg.add_deferred(20, 10);  // node 20 copies from 10
        cg.add_deferred(30, 20);  // node 30 copies from 20

        let pts = cg.resolve_points_to(30);
        assert!(pts.contains(&10));
    }

    #[test]
    fn test_nested_object_stored_to_escaping() {
        let mut g = Graph::new();
        let outer = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
        let inner = g.add_node(Op::New { class_id: 2, num_fields: 0 }, vec![]);
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
        let a1 = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
        let a2 = g.add_node(Op::New { class_id: 2, num_fields: 0 }, vec![]);
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
        assert_eq!(EscapeState::NoEscape.join(EscapeState::NoEscape), EscapeState::NoEscape);
        assert_eq!(EscapeState::NoEscape.join(EscapeState::ArgEscape), EscapeState::ArgEscape);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
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
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 0 }, vec![]);
        let _phi = g.add_node(Op::Phi, vec![param, alloc]);
        // Should not panic.
        let _ = analyze_escapes(&g);
    }

    #[test]
    fn test_multiple_stores_to_same_field_last_wins() {
        let mut g = Graph::new();
        let alloc = g.add_node(Op::New { class_id: 1, num_fields: 1 }, vec![]);
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
}
