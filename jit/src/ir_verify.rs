// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Always-on verifier for the sea-of-nodes IR.
//!
//! ## Why this exists
//!
//! The P0 "JIT correctness" lane of the C2 review
//! (`docs/known-issues/deep-research-vm-c2.md`) asks for *"a universal IR
//! verifier … run after parsing and every mutating pass in stress builds; run
//! before lowering in all builds"*, with the exit criterion that *"invalid IR
//! or ABI state causes a deterministic compilation bailout, never silent wrong
//! code, panic, or native crash"*.
//!
//! Every historical miscompile in this pipeline that reached machine code had
//! a graph-level signature that a verifier would have named:
//! a phi whose value-input count no longer matched its merge's predecessor
//! count (dropping a parallel copy — `emit_phi_copies` pairs
//! `phi.inputs[k + 1]` with `merge.inputs[k]` positionally); a live node whose
//! input pointed at an `Op::Dead` node (reading a slot nothing writes); a
//! `NO_NODE` reaching `ir_lower::slot_of`, which indexes `node_slot` with
//! `u32::MAX` and panics. This module turns each of those into a
//! [`BailoutReason::IrVerification`] *before* the lowerer sees the graph.
//!
//! ## Contract
//!
//! * **Never panics.** Every index goes through `get`, so a graph that is
//!   arbitrarily malformed still produces a `Bailout`, not an abort. That is
//!   the whole point: the verifier is the component that must survive input it
//!   was not designed for.
//! * **Never mutates.** `&Graph` in, `Result` out.
//! * **Collects, does not short-circuit.** One compile reports every violation
//!   it can find (up to [`MAX_REPORTED_VIOLATIONS`]), because the second
//!   violation is usually the one that explains the first.
//!
//! ## Lanes
//!
//! The *structural* lane (edge validity, arity, phi/merge alignment, control
//! integrity) always runs — it is the class of defect that produces silent
//! wrong code, and it has no false positives on graphs the current front end
//! builds. The *type*, *frame-state* and *schedule* lanes are opt-in through
//! [`VerifyOptions`], because:
//!
//! * **types** — `IrBuilder::phi_data_type` currently returns `Int` for a phi
//!   whose inputs are references or floats (the review's "Make node types
//!   complete" item). Until that lands, the type lane would reject graphs the
//!   compiler handles correctly today, so enabling it by default would trade
//!   real coverage for a finding we already have written down.
//! * **frame states** — `ir_optimize::eliminate_dead_nodes` documents that
//!   safepoint snapshots are deliberately *not* DCE roots ("a value killed
//!   here resolves to `Undefined`"), so after optimization essentially every
//!   graph has snapshot slots pointing at removed nodes. That is a known,
//!   written-down modelling gap, not something to lose coverage over.
//! * **schedule** — this lane covers *ordering*: arena-order
//!   definition-before-use (a heuristic — a `Graph` carries no schedule, and
//!   GVN and `apply_ea_to_ir` append replacement nodes *after* their users)
//!   and memory-token chain integrity. The latter is off by default because
//!   `ir_optimize::eliminate_dead_stores` kills an overwritten store with a
//!   plain `Graph::kill` and no `replace_all_uses`, leaving the *next* memory
//!   operation's token slot pointing at a removed node. That is a real defect
//!   — the surviving store loses its transitive ordering edges — but it is a
//!   defect in a pass this module does not own, and rejecting every method
//!   that hits it would trade a large amount of optimized code for a finding
//!   that one grep already establishes.
//!
//! Enable them with `CRATONVM_JIT_VERIFY_TYPES=1`,
//! `CRATONVM_JIT_VERIFY_FRAME_STATES=1`, `CRATONVM_JIT_VERIFY_SCHEDULE=1`, or
//! by passing [`VerifyOptions::all`] directly.

use crate::bailout::{Bailout, BailoutReason, CompileResult};
use crate::ir::{Graph, IrType, Node, NodeId, Op, NO_NODE};

// ── Options ──────────────────────────────────────────────────────────

/// Which optional verification lanes to run. The structural lane is not
/// listed because it is unconditional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Check the type lattice: phi joins, and reference/float/integer flow.
    pub check_types: bool,
    /// Check safepoint snapshots (deopt frame states).
    pub check_frame_states: bool,
    /// Check ordering: definition-before-use in arena order (a heuristic — see
    /// the module docs) and memory-token chain integrity.
    pub check_schedule: bool,
}

impl Default for VerifyOptions {
    /// Types and frame states on, schedule off. This is the "I built this
    /// graph and expect it to be clean" setting used by tests and by explicit
    /// stress runs — *not* what the production pre-lowering hook uses (that
    /// one is [`VerifyOptions::from_env`]).
    fn default() -> Self {
        VerifyOptions {
            check_types: true,
            check_frame_states: true,
            check_schedule: false,
        }
    }
}

impl VerifyOptions {
    /// Structural lane only.
    pub const fn structural() -> Self {
        VerifyOptions {
            check_types: false,
            check_frame_states: false,
            check_schedule: false,
        }
    }

    /// Every lane.
    pub const fn all() -> Self {
        VerifyOptions {
            check_types: true,
            check_frame_states: true,
            check_schedule: true,
        }
    }

    /// Structural lane plus whichever optional lanes the environment enables.
    ///
    /// This is what the compiler itself uses, so the default production
    /// behaviour is "structural only" — see the module docs for why each
    /// optional lane is opt-in.
    pub fn from_env() -> Self {
        VerifyOptions {
            check_types: env_flag("CRATONVM_JIT_VERIFY_TYPES").unwrap_or(false),
            check_frame_states: env_flag("CRATONVM_JIT_VERIFY_FRAME_STATES").unwrap_or(false),
            check_schedule: env_flag("CRATONVM_JIT_VERIFY_SCHEDULE").unwrap_or(false),
        }
    }
}

// ── Enablement ───────────────────────────────────────────────────────

/// Parse a boolean-ish environment flag. `None` = unset / unparseable.
fn env_flag(name: &str) -> Option<bool> {
    let raw = cratonvm_types::flags::runtime_var_os(name)?;
    let s = raw.to_str()?.trim().to_ascii_lowercase();
    match s.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Whether the *optional* per-pass verification runs.
///
/// True in debug builds (where an extra linear pass per optimization is
/// irrelevant next to an unoptimized compiler), or when
/// `CRATONVM_JIT_VERIFY_IR=1` explicitly enables it in a release build.
/// Latched on first read so the per-pass hook costs one atomic load.
pub fn verify_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| env_flag("CRATONVM_JIT_VERIFY_IR").unwrap_or(cfg!(debug_assertions)))
}

/// Kill switch for the *unconditional* pre-lowering verification.
///
/// `CRATONVM_JIT_VERIFY_IR=0` turns the pre-lowering check off entirely. This
/// exists so a verifier false positive can never be the reason a deployment
/// loses optimized code: the operator can disable the gate without a rebuild,
/// and the bailout counters (`bailout::bailout_counts`) tell them whether it
/// was firing. It is deliberately *not* the same predicate as
/// [`verify_enabled`] — that one defaults to the build profile, this one only
/// ever responds to an explicit `0`.
pub fn pre_lower_verify_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| env_flag("CRATONVM_JIT_VERIFY_IR") == Some(false))
}

// ── Violation accumulation ───────────────────────────────────────────

/// Upper bound on how many violations one report lists. A graph that is
/// structurally shredded can produce one violation per node; the message is
/// going into a log line and a `Bailout`, not a file.
pub const MAX_REPORTED_VIOLATIONS: usize = 32;

/// Largest plausible local/stack slot count in a safepoint snapshot. The JVM
/// class-file format stores `max_locals` and `max_stack` as `u16`, so anything
/// above this cannot have come from a real method.
const MAX_JVM_FRAME_SLOTS: usize = u16::MAX as usize;

struct Violations {
    list: Vec<String>,
    total: usize,
}

impl Violations {
    fn new() -> Self {
        Violations {
            list: Vec::new(),
            total: 0,
        }
    }

    fn add(&mut self, msg: String) {
        self.total += 1;
        if self.list.len() < MAX_REPORTED_VIOLATIONS {
            self.list.push(msg);
        }
    }

    fn is_empty(&self) -> bool {
        self.total == 0
    }

    fn into_result(self, phase: &str) -> CompileResult<()> {
        if self.is_empty() {
            return Ok(());
        }
        let mut msg = format!("{} violation(s): ", self.total);
        msg.push_str(&self.list.join("; "));
        if self.total > self.list.len() {
            msg.push_str(&format!(" … and {} more", self.total - self.list.len()));
        }
        Err(Bailout::with_context(
            BailoutReason::IrVerification(msg),
            format!("phase={phase}"),
        ))
    }
}

// ── Entry point ──────────────────────────────────────────────────────

/// Verify `graph`, attributing any failure to `phase` (a pass name such as
/// `"post-optimize"` or `"pre-lower"`).
///
/// Returns `Err(Bailout { reason: BailoutReason::IrVerification(..), .. })`
/// listing every violation found. Never panics, never mutates.
pub fn verify_graph(graph: &Graph, phase: &str, opts: VerifyOptions) -> CompileResult<()> {
    let mut v = Violations::new();

    // Structural lane — always on.
    check_edges(graph, &mut v);
    check_arity(graph, &mut v);
    check_phis(graph, &mut v);
    check_control(graph, &mut v);

    if opts.check_types {
        check_types(graph, &mut v);
    }
    if opts.check_frame_states {
        check_frame_states(graph, &mut v);
    }
    if opts.check_schedule {
        check_schedule(graph, &mut v);
    }

    v.into_result(phase)
}

// ── Shared helpers ───────────────────────────────────────────────────

fn node_at(graph: &Graph, id: NodeId) -> Option<&Node> {
    if id == NO_NODE {
        return None;
    }
    graph.nodes.get(id as usize)
}

/// A node exists and has not been removed (`Op::Dead`).
fn is_live(graph: &Graph, id: NodeId) -> bool {
    matches!(node_at(graph, id), Some(n) if n.op != Op::Dead)
}

/// Short human label for a node, e.g. `n7:Add`.
fn label(graph: &Graph, id: NodeId) -> String {
    match node_at(graph, id) {
        Some(n) => format!("n{id}:{:?}", n.op),
        None if id == NO_NODE => "NO_NODE".to_string(),
        None => format!("n{id}:<out-of-range>"),
    }
}

/// True if the node hands a control token to its successors. `Op::Return`
/// consumes control and produces none, and `Op::Proj` produces control only
/// for the control projection (`Proj(1)` off `Start` is the memory token).
fn produces_control(node: &Node) -> bool {
    match node.op {
        Op::Start | Op::If | Op::Merge | Op::Region => true,
        Op::Proj(_) => node.ty == IrType::Control,
        _ => false,
    }
}

/// Indices of `node`'s inputs that are control edges.
fn control_input_indices(node: &Node) -> Vec<usize> {
    match node.op {
        // A merge takes one control edge per predecessor.
        Op::Merge | Op::Region => (0..node.inputs.len()).collect(),
        // Everything else pins control at input 0 (when it has one at all).
        Op::Return
        | Op::If
        | Op::Proj(_)
        | Op::Guard { .. }
        | Op::Load(_)
        | Op::Store(_)
        | Op::ArrayLoad(_)
        | Op::ArrayStore(_)
        | Op::New { .. }
        | Op::NewArray { .. }
        | Op::Call { .. }
        | Op::LambdaIntToDouble => {
            if node.inputs.is_empty() {
                Vec::new()
            } else {
                vec![0]
            }
        }
        // `Op::Param` is defined at `Start` but that edge is a definition
        // anchor, not a control-flow edge, and `Op::ArrayLength` is built with
        // and without a control edge in-tree — neither is checked here.
        _ => Vec::new(),
    }
}

/// `NO_NODE` is a legitimate value in exactly two places: a phi value input
/// (a local that is undefined on that predecessor edge) and a safepoint slot
/// (a local/stack slot that is undefined at that bci). Anywhere else it is the
/// bug that panics `ir_lower::slot_of`.
fn no_node_allowed(node: &Node, input_index: usize) -> bool {
    matches!(node.op, Op::Phi) && input_index >= 1
}

/// True if `input_index` of `node` is a memory *token* — a scheduling edge
/// naming the previous writer, not a value anything reads.
///
/// The distinction matters because a token pointing at a removed node is a
/// broken ordering edge (the ordering lane's business, and something
/// `eliminate_dead_stores` currently produces), while a *data* input pointing
/// at a removed node is a guaranteed wrong-value read — a hard structural
/// violation. Collapsing the two would make the always-on lane reject graphs
/// the compiler handles today.
fn is_memory_token_input(node: &Node, input_index: usize) -> bool {
    match node.op {
        Op::Load(_)
        | Op::Store(_)
        | Op::ArrayLoad(_)
        | Op::ArrayStore(_)
        | Op::New { .. }
        | Op::NewArray { .. }
        | Op::Call { .. }
        | Op::LambdaIntToDouble => input_index == 1,
        // A memory phi's value inputs are all tokens.
        Op::Phi => node.ty == IrType::Memory && input_index >= 1,
        _ => false,
    }
}

// ── Lane: edges ──────────────────────────────────────────────────────

/// Every `NodeId` referenced by a live node's input list is in range and
/// refers to a live node.
fn check_edges(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let id = idx as NodeId;
        for (i, &inp) in node.inputs.iter().enumerate() {
            if inp == NO_NODE {
                if !no_node_allowed(node, i) {
                    v.add(format!(
                        "{} input[{i}] is NO_NODE (undefined value reaching a real use)",
                        label(graph, id)
                    ));
                }
                continue;
            }
            match graph.nodes.get(inp as usize) {
                None => v.add(format!(
                    "{} input[{i}] = n{inp} is out of range (graph has {} nodes)",
                    label(graph, id),
                    graph.nodes.len()
                )),
                // A removed *memory token* is an ordering defect, reported by
                // the ordering lane; see `is_memory_token_input`.
                Some(target) if target.op == Op::Dead && !is_memory_token_input(node, i) => {
                    v.add(format!(
                        "{} input[{i}] = n{inp} refers to a removed (Dead) node",
                        label(graph, id)
                    ))
                }
                Some(_) => {}
            }
        }
    }
}

// ── Lane: arity ──────────────────────────────────────────────────────

/// Permitted input count for an op, as an inclusive `(min, max)` range.
///
/// Ranges rather than exact counts because a few ops legitimately vary:
/// `Op::Return` carries a value or does not, `Op::Call` carries its arguments,
/// and a `Merge`/`Region` the front end created but never activated has zero
/// predecessors (an unreachable branch target) — which is not a defect, just
/// dead control.
fn expected_arity(op: &Op) -> (usize, usize) {
    const ANY: usize = usize::MAX;
    match op {
        Op::Start => (0, 0),
        // [ctrl] or [ctrl, value]
        Op::Return => (1, 2),
        // [ctrl, cond]
        Op::If => (2, 2),
        Op::Merge | Op::Region => (0, ANY),
        Op::Proj(_) => (1, 1),
        Op::Const(_) | Op::ConstF(_) => (0, 0),
        // The builder anchors a parameter at `Start`; a hand-built graph need
        // not.
        Op::Param(_) => (0, 1),
        // [merge, val_0, …]
        Op::Phi => (2, ANY),
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
        | Op::UShr
        | Op::Cmp(_)
        | Op::LCmp
        | Op::FCmp { .. } => (2, 2),
        Op::Neg => (1, 1),
        Op::I2L
        | Op::L2I
        | Op::I2F
        | Op::I2D
        | Op::L2F
        | Op::L2D
        | Op::F2I
        | Op::F2L
        | Op::F2D
        | Op::D2I
        | Op::D2L
        | Op::D2F
        | Op::I2B
        | Op::I2C
        | Op::I2S => (1, 1),
        // [ctrl, mem, base, offset]
        Op::Load(_) => (4, 4),
        // [ctrl, mem, base, offset, value]
        Op::Store(_) => (5, 5),
        // Built as `[ctrl, mem, array]` by the production path and as
        // `[array]` by hand-built optimizer fixtures.
        Op::ArrayLength => (1, 3),
        // [ctrl, mem, array, index]
        Op::ArrayLoad(_) => (4, 4),
        // [ctrl, mem, array, index, value]
        Op::ArrayStore(_) => (5, 5),
        // [ctrl, mem]
        Op::New { .. } => (2, 2),
        // [ctrl, mem, length]
        Op::NewArray { .. } => (3, 3),
        // [ctrl, mem, args…]
        Op::Call { .. } => (2, ANY),
        // [ctrl, mem, lambda, index]
        Op::LambdaIntToDouble => (4, 4),
        // [ctrl, cond]
        Op::Guard { .. } => (2, 2),
        Op::Dead => (0, 0),
    }
}

fn check_arity(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let (min, max) = expected_arity(&node.op);
        let n = node.inputs.len();
        if n < min || n > max {
            let bound = if max == usize::MAX {
                format!("at least {min}")
            } else if min == max {
                format!("exactly {min}")
            } else {
                format!("{min}..={max}")
            };
            v.add(format!(
                "{} has {n} input(s) but its opcode requires {bound}",
                label(graph, idx as NodeId)
            ));
        }
    }
}

// ── Lane: phis ───────────────────────────────────────────────────────

/// `phi.inputs = [merge, val_0, …, val_{k-1}]` and
/// `merge.inputs = [ctrl_0, …, ctrl_{k-1}]`, paired **positionally**:
/// `ir_lower::emit_phi_copies` emits the copy for `phi.inputs[k + 1]` on the
/// edge from `merge.inputs[k]`. A count mismatch therefore silently drops a
/// parallel copy on one edge, which is a wrong-value miscompile rather than a
/// crash — exactly the class the review's exit criterion names.
fn check_phis(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op != Op::Phi {
            continue;
        }
        let id = idx as NodeId;
        let Some(&merge_id) = node.inputs.first() else {
            // Arity lane already reported the empty input list.
            continue;
        };
        let Some(merge) = node_at(graph, merge_id) else {
            // Edge lane already reported the bad reference.
            continue;
        };
        if !matches!(merge.op, Op::Merge | Op::Region) {
            v.add(format!(
                "{} input[0] is {} but a phi must be anchored at a Merge/Region",
                label(graph, id),
                label(graph, merge_id)
            ));
            continue;
        }
        let preds = merge.inputs.len();
        let vals = node.inputs.len() - 1;
        if vals != preds {
            v.add(format!(
                "{} has {vals} value input(s) but its merge {} has {preds} predecessor(s) \
                 (phi value k pairs with merge predecessor k)",
                label(graph, id),
                label(graph, merge_id)
            ));
        }
        // A phi may not take itself as a value input on a *forward* merge; on
        // a loop `Region` the back-edge value legitimately can be the phi
        // (`i = phi(0, i)` never happens in practice, but `i = phi(0, i + 1)`
        // routes through an Add, so a direct self-reference on the entry edge
        // is always wrong).
        if node.inputs.get(1).copied() == Some(id) {
            v.add(format!(
                "{} takes itself as its entry-edge value",
                label(graph, id)
            ));
        }
    }
}

// ── Lane: control ────────────────────────────────────────────────────

/// Single entry, no dangling or removed control targets, and at least one
/// terminator reachable from the entry.
///
/// Deliberately *not* "every control node reaches a terminator": an endless
/// `Region` loop (`while (true) { … }` with the exit inside a call) has no
/// path from its header to a `Return`, and that is valid Java. The check is
/// therefore "the graph has a live `Return` that the entry can reach", which
/// catches a graph whose exit was severed without rejecting a legal one.
fn check_control(graph: &Graph, v: &mut Violations) {
    // Single entry.
    let starts: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.op == Op::Start)
        .map(|(i, _)| i as NodeId)
        .collect();
    match starts.len() {
        0 => v.add("graph has no Op::Start node".to_string()),
        1 => {
            if graph.entry != starts[0] {
                v.add(format!(
                    "graph.entry = {} but the only Op::Start is {}",
                    label(graph, graph.entry),
                    label(graph, starts[0])
                ));
            }
        }
        n => v.add(format!(
            "graph has {n} Op::Start nodes (control flow must have a single entry)"
        )),
    }

    if !is_live(graph, graph.exit) {
        v.add(format!(
            "graph.exit = {} is not a live node",
            label(graph, graph.exit)
        ));
    }

    // Control edges must target live control producers.
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        for i in control_input_indices(node) {
            let Some(&ctrl) = node.inputs.get(i) else {
                continue;
            };
            if ctrl == NO_NODE {
                // Reported by the edge lane.
                continue;
            }
            match node_at(graph, ctrl) {
                None => { /* reported by the edge lane */ }
                Some(target) if target.op == Op::Dead => { /* reported by the edge lane */ }
                Some(target) if !produces_control(target) => v.add(format!(
                    "{} takes control from {}, which produces no control token",
                    label(graph, idx as NodeId),
                    label(graph, ctrl)
                )),
                Some(_) => {}
            }
        }
    }

    // Forward reachability from the entry over control edges only.
    if starts.len() == 1 && is_live(graph, graph.entry) {
        let mut reachable = vec![false; graph.nodes.len()];
        // succ[c] = control nodes that consume c's control token.
        let mut succ: Vec<Vec<NodeId>> = vec![Vec::new(); graph.nodes.len()];
        for (idx, node) in graph.nodes.iter().enumerate() {
            if node.op == Op::Dead {
                continue;
            }
            for i in control_input_indices(node) {
                if let Some(&ctrl) = node.inputs.get(i) {
                    if let Some(slot) = succ.get_mut(ctrl as usize) {
                        slot.push(idx as NodeId);
                    }
                }
            }
        }
        let mut stack = vec![graph.entry];
        if let Some(seen) = reachable.get_mut(graph.entry as usize) {
            *seen = true;
        }
        let mut found_return = false;
        while let Some(cur) = stack.pop() {
            if matches!(node_at(graph, cur), Some(n) if n.op == Op::Return) {
                found_return = true;
            }
            let Some(succs) = succ.get(cur as usize) else {
                continue;
            };
            for &s in succs {
                match reachable.get_mut(s as usize) {
                    Some(seen) if !*seen => {
                        *seen = true;
                        stack.push(s);
                    }
                    _ => {}
                }
            }
        }
        let has_return = graph.nodes.iter().any(|n| matches!(n.op, Op::Return));
        if has_return && !found_return {
            v.add(
                "no Op::Return is reachable from the entry over control edges (the graph's \
                 terminator was severed)"
                    .to_string(),
            );
        } else if !has_return {
            v.add("graph has no live Op::Return terminator".to_string());
        }
    }
}

// ── Lane: types ──────────────────────────────────────────────────────

/// Coarse type category. The lattice join is defined *within* a category
/// (`Int ⊔ Long = Long`); across categories it is undefined, which is exactly
/// the defect this lane looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cat {
    Integer,
    Float,
    Ref,
    Memory,
    Control,
    Void,
}

fn category(t: IrType) -> Cat {
    match t {
        IrType::Int | IrType::Long => Cat::Integer,
        IrType::Float | IrType::Double => Cat::Float,
        IrType::Ref => Cat::Ref,
        IrType::Memory => Cat::Memory,
        IrType::Control => Cat::Control,
        IrType::Void => Cat::Void,
    }
}

fn cat_name(c: Cat) -> &'static str {
    match c {
        Cat::Integer => "integer",
        Cat::Float => "floating-point",
        Cat::Ref => "reference",
        Cat::Memory => "memory",
        Cat::Control => "control",
        Cat::Void => "void",
    }
}

/// Ops whose result category must equal every operand's category.
///
/// Comparisons (`Cmp`/`LCmp`/`FCmp`) and conversions (`I2L`, `D2I`, …) are
/// excluded by construction: crossing categories is what they are *for*.
/// Shifts are included but only for operand 0 — `lshl` shifts a `Long` by an
/// `Int`.
fn is_homogeneous_arith(op: &Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
    )
}

/// Number of leading operands of `op` whose category must match the result.
fn homogeneous_operand_count(op: &Op, arity: usize) -> usize {
    match op {
        Op::Shl | Op::Shr | Op::UShr => 1.min(arity),
        _ => arity,
    }
}

/// Indices of `node`'s inputs that carry a *data* value (as opposed to a
/// control or memory token).
fn data_input_indices(node: &Node) -> Vec<usize> {
    match node.op {
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr
        | Op::Cmp(_)
        | Op::LCmp
        | Op::FCmp { .. }
        | Op::I2L
        | Op::L2I
        | Op::I2F
        | Op::I2D
        | Op::L2F
        | Op::L2D
        | Op::F2I
        | Op::F2L
        | Op::F2D
        | Op::D2I
        | Op::D2L
        | Op::D2F
        | Op::I2B
        | Op::I2C
        | Op::I2S => (0..node.inputs.len()).collect(),
        // [ctrl, cond]
        Op::If | Op::Guard { .. } => {
            if node.inputs.len() > 1 {
                vec![1]
            } else {
                Vec::new()
            }
        }
        // [ctrl, mem, base, offset(, value)]
        Op::Load(_) | Op::Store(_) | Op::ArrayLoad(_) | Op::ArrayStore(_) => {
            (2..node.inputs.len()).collect()
        }
        // [ctrl, mem, length]
        Op::NewArray { .. } => (2..node.inputs.len()).collect(),
        _ => Vec::new(),
    }
}

fn check_types(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let id = idx as NodeId;

        // Phi: the declared type must be a valid join of the value inputs.
        if node.op == Op::Phi {
            // A *memory* phi joins scheduling tokens, not values, and the
            // builder deliberately reuses a value node as a token
            // (`self.mem = load`, where the load is `IrType::Int`). Its inputs
            // are therefore heterogeneous by design.
            if node.ty == IrType::Memory {
                continue;
            }
            let mut join: Option<Cat> = None;
            let mut mixed = false;
            for &inp in node.inputs.iter().skip(1) {
                if inp == NO_NODE {
                    continue;
                }
                let Some(src) = node_at(graph, inp) else {
                    continue;
                };
                if src.op == Op::Dead {
                    continue;
                }
                let c = category(src.ty);
                match join {
                    None => join = Some(c),
                    Some(prev) if prev != c => {
                        if !mixed {
                            mixed = true;
                            v.add(format!(
                                "{} joins incompatible input categories ({} and {}) — there is \
                                 no lattice join for them",
                                label(graph, id),
                                cat_name(prev),
                                cat_name(c)
                            ));
                        }
                    }
                    Some(_) => {}
                }
            }
            if let Some(j) = join {
                if !mixed && j != category(node.ty) {
                    v.add(format!(
                        "{} is typed {} but joins {} inputs",
                        label(graph, id),
                        cat_name(category(node.ty)),
                        cat_name(j)
                    ));
                }
            }
            continue;
        }

        // Non-phi: control/void tokens may not flow into a data use, and a
        // homogeneous arithmetic node's operands must share its category.
        let data_idx = data_input_indices(node);
        let result_cat = category(node.ty);
        let homogeneous = is_homogeneous_arith(&node.op);
        let homogeneous_upto = homogeneous_operand_count(&node.op, node.inputs.len());
        for i in data_idx {
            let Some(&inp) = node.inputs.get(i) else {
                continue;
            };
            let Some(src) = node_at(graph, inp) else {
                continue;
            };
            if src.op == Op::Dead {
                continue;
            }
            let c = category(src.ty);
            if matches!(c, Cat::Control | Cat::Void) {
                v.add(format!(
                    "{} input[{i}] = {} is a {} token used as a data value",
                    label(graph, id),
                    label(graph, inp),
                    cat_name(c)
                ));
                continue;
            }
            if homogeneous && i < homogeneous_upto && c != result_cat {
                v.add(format!(
                    "{} produces {} but input[{i}] = {} is {}",
                    label(graph, id),
                    cat_name(result_cat),
                    label(graph, inp),
                    cat_name(c)
                ));
            }
        }
    }
}

// ── Lane: frame states ───────────────────────────────────────────────

/// Safepoint snapshots must reference live nodes and describe a plausible
/// interpreter frame.
fn check_frame_states(graph: &Graph, v: &mut Violations) {
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if sp.locals.len() > MAX_JVM_FRAME_SLOTS {
            v.add(format!(
                "safepoint[{si}] at bci {} declares {} locals (max_locals is a u16)",
                sp.bci,
                sp.locals.len()
            ));
        }
        if sp.stack.len() > MAX_JVM_FRAME_SLOTS {
            v.add(format!(
                "safepoint[{si}] at bci {} declares {} stack slots (max_stack is a u16)",
                sp.bci,
                sp.stack.len()
            ));
        }
        let slots = sp
            .locals
            .iter()
            .enumerate()
            .map(|(i, id)| ("local", i, *id))
            .chain(sp.stack.iter().enumerate().map(|(i, id)| ("stack", i, *id)));
        for (kind, i, id) in slots {
            // `NO_NODE` means "undefined at this bci", which is legal.
            if id == NO_NODE {
                continue;
            }
            match graph.nodes.get(id as usize) {
                None => v.add(format!(
                    "safepoint[{si}] at bci {} {kind}[{i}] = n{id} is out of range",
                    sp.bci
                )),
                Some(n) if n.op == Op::Dead => v.add(format!(
                    "safepoint[{si}] at bci {} {kind}[{i}] = n{id} refers to a removed (Dead) \
                     node — the deopt frame would rebuild a slot from nothing",
                    sp.bci
                )),
                Some(_) => {}
            }
        }
    }
}

// ── Lane: schedule ───────────────────────────────────────────────────

/// Ordering: arena-order definition-before-use, plus memory-token chain
/// integrity.
///
/// A `Graph` carries no schedule, so definition-before-use is approximated by
/// arena order: `Graph::add` appends, so a node built by the front end always
/// has smaller-numbered inputs. Phis (loop back-edges), merges (back-edge
/// control) and any node the optimizer rewired to a later-appended replacement
/// are exempt.
///
/// The memory-chain check is the other half of ordering: a memory operation
/// whose incoming token names a removed node has lost the transitive
/// dependency on everything that wrote before it, so the scheduler is free to
/// hoist it above those writes.
fn check_schedule(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let id = idx as NodeId;

        for (i, &inp) in node.inputs.iter().enumerate() {
            if inp == NO_NODE || inp as usize >= graph.nodes.len() {
                continue;
            }
            if is_memory_token_input(node, i) && !is_live(graph, inp) {
                v.add(format!(
                    "{} takes its memory token from removed node n{inp} — the ordering edge to \
                     everything that wrote before it is gone",
                    label(graph, id)
                ));
            }
            if matches!(node.op, Op::Phi | Op::Merge | Op::Region) {
                continue;
            }
            if inp > id {
                v.add(format!(
                    "{} input[{i}] = {} is defined after its use in arena order",
                    label(graph, id),
                    label(graph, inp)
                ));
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Graph, IrType, MemKind, Op};

    /// A minimal well-formed graph: `Start → Proj(0) → Return(Const)`.
    fn linear_graph() -> Graph {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let k = g.add(Op::Const(7), IrType::Int, vec![], None);
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, k], None);
        g.entry = start;
        g.exit = ret;
        g
    }

    /// `Start → If → (Proj, Proj) → Merge → Phi → Return(phi)`.
    fn diamond_graph() -> Graph {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
        let if_node = g.add(Op::If, IrType::Control, vec![ctrl, cond], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        let merge = g.add(Op::Merge, IrType::Control, vec![t, f], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, a, b], None);
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], None);
        g.entry = start;
        g.exit = ret;
        g
    }

    fn message(err: &Bailout) -> String {
        match &err.reason {
            BailoutReason::IrVerification(m) => m.clone(),
            other => panic!("expected IrVerification, got {other:?}"),
        }
    }

    #[test]
    fn valid_linear_graph_verifies_clean() {
        let g = linear_graph();
        assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
    }

    #[test]
    fn valid_diamond_graph_verifies_clean() {
        let g = diamond_graph();
        let r = verify_graph(&g, "test", VerifyOptions::all());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn dangling_input_is_rejected() {
        let mut g = linear_graph();
        // Point the Return's value at a node that does not exist.
        let ret = g.exit as usize;
        g.nodes[ret].inputs[1] = 999;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("out of range"), "{m}");
        assert!(m.contains("n999"), "{m}");
        assert_eq!(err.category(), "ir_verification");
        assert_eq!(err.context.as_deref(), Some("phase=test"));
    }

    #[test]
    fn input_referring_to_a_removed_node_is_rejected() {
        let mut g = linear_graph();
        let ret = g.exit;
        let val = g.nodes[ret as usize].inputs[1];
        g.kill(val);
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("removed (Dead) node"), "{m}");
    }

    #[test]
    fn no_node_outside_a_phi_is_rejected() {
        let mut g = linear_graph();
        let ret = g.exit as usize;
        g.nodes[ret].inputs[1] = NO_NODE;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        assert!(message(&err).contains("NO_NODE"), "{}", message(&err));
    }

    #[test]
    fn no_node_inside_a_phi_is_allowed() {
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        g.nodes[phi as usize].inputs[1] = NO_NODE;
        // Structural + frame-state lanes must accept an undefined predecessor
        // value; only the type lane has an opinion, and it skips NO_NODE too.
        assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
    }

    #[test]
    fn phi_arity_mismatch_is_rejected() {
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        // Drop one value input: the merge still has two predecessors, so the
        // second edge would emit no parallel copy.
        g.nodes[phi as usize].inputs.pop();
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("value input"), "{m}");
        assert!(m.contains("predecessor"), "{m}");
    }

    #[test]
    fn phi_not_anchored_at_a_merge_is_rejected() {
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        g.nodes[phi as usize].inputs[0] = g.entry;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        assert!(
            message(&err).contains("anchored at a Merge/Region"),
            "{}",
            message(&err)
        );
    }

    #[test]
    fn type_inconsistent_phi_is_rejected() {
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        // Make one incoming value a reference while the other stays an int.
        let a = g.nodes[phi as usize].inputs[1];
        g.nodes[a as usize].ty = IrType::Ref;
        let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("incompatible input categories"), "{m}");
        assert!(m.contains("reference"), "{m}");
        // …and the structural lane alone must NOT reject it, which is what
        // makes the type lane safe to keep opt-in for now.
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    }

    #[test]
    fn phi_typed_against_its_join_is_rejected() {
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        // Both inputs are references, but the phi claims to be an int — the
        // `phi_data_type` defect the review names.
        for i in 1..g.nodes[phi as usize].inputs.len() {
            let src = g.nodes[phi as usize].inputs[i];
            g.nodes[src as usize].ty = IrType::Ref;
        }
        let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("typed integer but joins reference"), "{m}");
    }

    #[test]
    fn reference_flowing_into_integer_arithmetic_is_rejected() {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Ref, vec![], None);
        let sum = g.add(Op::Add, IrType::Int, vec![a, b], None);
        let ret = g.exit as usize;
        g.nodes[ret].inputs[1] = sum;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("produces integer"), "{m}");
        assert!(m.contains("reference"), "{m}");
        // Structural-only must accept it: nothing about the *shape* is wrong.
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    }

    #[test]
    fn shift_amount_may_be_narrower_than_the_shifted_value() {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Long, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        let sh = g.add(Op::Shl, IrType::Long, vec![a, b], None);
        let ret = g.exit as usize;
        g.nodes[ret].inputs[1] = sh;
        // Not `all()`: the appended nodes outrank the Return in arena order,
        // which is exactly what the (opt-in, heuristic) schedule lane reports.
        let r = verify_graph(&g, "test", VerifyOptions::default());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn wrong_arity_is_rejected() {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let bad = g.add(Op::Add, IrType::Int, vec![a], None);
        let ret = g.exit as usize;
        g.nodes[ret].inputs[1] = bad;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("exactly 2"), "{m}");
    }

    #[test]
    fn two_start_nodes_are_rejected() {
        let mut g = linear_graph();
        g.add(Op::Start, IrType::Control, vec![], None);
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        assert!(message(&err).contains("single entry"), "{}", message(&err));
    }

    #[test]
    fn control_taken_from_a_data_node_is_rejected() {
        let mut g = linear_graph();
        let k = g.add(Op::Const(3), IrType::Int, vec![], None);
        let ret = g.exit as usize;
        g.nodes[ret].inputs[0] = k;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        assert!(
            message(&err).contains("produces no control token"),
            "{}",
            message(&err)
        );
    }

    #[test]
    fn severed_terminator_is_rejected() {
        let mut g = linear_graph();
        // Re-anchor the Return on a Merge that nothing reaches.
        let orphan = g.add(Op::Merge, IrType::Control, vec![], None);
        let ret = g.exit as usize;
        g.nodes[ret].inputs[0] = orphan;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        assert!(
            message(&err).contains("reachable from the entry"),
            "{}",
            message(&err)
        );
    }

    /// `ir_optimize::eliminate_dead_stores` kills an overwritten store without
    /// rewiring the memory chain, so the surviving store's token slot names a
    /// removed node. That is an ordering defect, not a wrong-value one, and
    /// the always-on structural lane must not reject it — see
    /// `is_memory_token_input`.
    #[test]
    fn removed_memory_token_is_an_ordering_finding_not_a_structural_one() {
        let mut g = linear_graph();
        let ctrl = g.nodes[g.exit as usize].inputs[0];
        let mem = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Proj(1)))
            .expect("memory projection") as NodeId;
        let base = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let val = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st1 = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, base, off, val],
            None,
        );
        let st2 = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, st1, base, off, val],
            None,
        );
        assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
        g.kill(st1);
        // Structural / type / frame-state lanes: unchanged verdict.
        let r = verify_graph(&g, "test", VerifyOptions::default());
        assert!(r.is_ok(), "{}", r.unwrap_err());
        // Ordering lane: names it.
        let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("memory token"), "{m}");
        assert!(m.contains(&format!("n{st1}")), "{m}");
        let _ = st2;
    }

    #[test]
    fn frame_state_referring_to_a_removed_node_is_rejected() {
        let mut g = linear_graph();
        let k = g.add(Op::Const(9), IrType::Int, vec![], None);
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![k, NO_NODE],
            stack: vec![],
        });
        // Clean while the slot is live…
        assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());
        // …and rejected once it is not.
        g.kill(k);
        let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("local[0]"), "{m}");
        assert!(m.contains("removed (Dead)"), "{m}");
    }

    #[test]
    fn frame_state_lane_is_opt_in() {
        let mut g = linear_graph();
        let k = g.add(Op::Const(9), IrType::Int, vec![], None);
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![k],
            stack: vec![],
        });
        g.kill(k);
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
        assert!(verify_graph(&g, "test", VerifyOptions::default()).is_err());
    }

    #[test]
    fn every_violation_is_collected_not_just_the_first() {
        let mut g = linear_graph();
        let ret = g.exit as usize;
        g.nodes[ret].inputs[0] = 500;
        g.nodes[ret].inputs[1] = 501;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("n500"), "{m}");
        assert!(m.contains("n501"), "{m}");
        assert!(m.contains("violation(s)"), "{m}");
    }

    #[test]
    fn violation_list_is_capped_but_the_total_is_not() {
        let mut g = linear_graph();
        // Many broken nodes: one violation each.
        for _ in 0..(MAX_REPORTED_VIOLATIONS + 10) {
            g.add(Op::Add, IrType::Int, vec![9999, 9999], None);
        }
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("and"), "{m}");
        assert!(m.contains("more"), "{m}");
    }

    #[test]
    fn verifier_never_panics_on_a_shredded_graph() {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 42,
            exit: 77,
            safepoints: vec![crate::ir::SafepointSnapshot {
                bci: 0,
                locals: vec![5, NO_NODE, 900],
                stack: vec![1234],
            }],
        };
        g.add(Op::Phi, IrType::Int, vec![NO_NODE, 3], None);
        g.add(Op::Return, IrType::Void, vec![u32::MAX - 1], None);
        // No assertion on the contents — the contract under test is "returns
        // an Err instead of panicking".
        assert!(verify_graph(&g, "shredded", VerifyOptions::all()).is_err());
    }

    #[test]
    fn empty_graph_is_rejected_without_panicking() {
        let g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        assert!(verify_graph(&g, "empty", VerifyOptions::all()).is_err());
    }

    #[test]
    fn schedule_lane_flags_a_use_before_its_definition() {
        let mut g = linear_graph();
        // Append a constant *after* the Return, then make the Return consume
        // it: valid as a graph, impossible as an arena-ordered schedule.
        let later = g.add(Op::Const(5), IrType::Int, vec![], None);
        let ret = g.exit as usize;
        g.nodes[ret].inputs[1] = later;
        assert!(verify_graph(&g, "test", VerifyOptions::default()).is_ok());
        let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
        assert!(
            message(&err).contains("defined after its use"),
            "{}",
            message(&err)
        );
    }

    #[test]
    fn options_from_env_defaults_to_structural_only() {
        // The env vars are not set in the test harness, so `from_env` must be
        // the structural-only configuration.
        let o = VerifyOptions::from_env();
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_VERIFY_TYPES").is_none() {
            assert!(!o.check_types);
        }
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_VERIFY_FRAME_STATES").is_none() {
            assert!(!o.check_frame_states);
        }
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_VERIFY_SCHEDULE").is_none() {
            assert!(!o.check_schedule);
        }
    }

    #[test]
    fn verify_enabled_matches_the_build_profile_when_unset() {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_VERIFY_IR").is_none() {
            assert_eq!(verify_enabled(), cfg!(debug_assertions));
            assert!(!pre_lower_verify_disabled());
        }
    }
}
