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
//! builds. Four further lanes are selected by [`VerifyOptions`]; three of them
//! were opt-in because of defects that have since been fixed on this branch,
//! and this section is the record of what changed.
//!
//! * **types** ([`VerifyOptions::check_types`]) — the lattice is
//!   [`crate::ir::join_data_type`], *not* a private one. It used to be a
//!   private coarse "category" join in this file, in which `Int` and `Long`
//!   were the same category; that was written when `IrBuilder::phi_data_type`
//!   really did answer `Int` for every φ that was not `Long`/`Double`, so a
//!   finer lattice would have rejected graphs the compiler handled. That is no
//!   longer true: `phi_data_type` now folds `join_data_type` over the φ's value
//!   inputs (`Graph::phi_data_type_checked`), and `join_data_type` *rejects*
//!   an `Int`/`Long` merge. A verifier running a coarser lattice than the
//!   compiler proves nothing about the compiler, so the two are now the same
//!   function. See "The φ fallback" below.
//! * **frame states** ([`VerifyOptions::check_frame_states`]) —
//!   `ir_optimize::eliminate_dead_nodes` used to document safepoint snapshots
//!   as deliberately *not* DCE roots ("a value killed here resolves to
//!   `Undefined`"), so every optimized graph accumulated snapshot slots
//!   pointing at removed nodes. That policy is reversed: every value a snapshot
//!   names is now a DCE root, and a slot that some *earlier* pass already
//!   stranded is normalised to `NO_NODE` before the mark phase. This lane is
//!   therefore clean after `ir_optimize::optimize` and is **on by default at
//!   the `"post-optimize"` hook** — see [`VerifyOptions::for_phase`].
//! * **memory chain** ([`VerifyOptions::check_memory_chain`]) — a memory
//!   operation whose incoming token names a removed node has lost the
//!   transitive dependency on everything that wrote before it, so the scheduler
//!   may hoist it above those writes. `ir_optimize::eliminate_dead_stores` used
//!   to produce exactly that, killing an overwritten store with a plain
//!   `Graph::kill` and no `replace_all_uses`. It now routes every store
//!   deletion through `kill_store_splicing_memory_chain`, which rewires the
//!   consumers to the killed store's own incoming token first and *declines the
//!   deletion* when it cannot. So this lane, too, is clean after
//!   `ir_optimize::optimize` and on by default at `"post-optimize"`.
//! * **arena order** ([`VerifyOptions::check_arena_order`]) — definition
//!   -before-use approximated by arena index. This one is opt-in **and stays
//!   that way**, because it is not a soundness property: a `Graph` carries no
//!   schedule, and GVN legitimately appends a replacement node *after* the
//!   users it rewires to it, so the lane fires on essentially every optimized
//!   graph by construction. It is a debugging aid for hand-built graphs and
//!   front-end output, which is why it was split out of the old combined
//!   `check_schedule` lane rather than left riding along with the memory-chain
//!   check it has nothing in common with.
//!
//! What is still *not* on by default at `"pre-lower"` is the pair of lanes
//! above that `ir_optimize` fixed, because a second mutating pass runs after
//! it: `apply_ea_to_ir` (in `lib.rs`) marks the scalar-replaced allocation, its
//! stores and its loads `Op::Dead` while rewiring only `graph.nodes` — it never
//! touches `graph.safepoints`, and it does not splice the memory chain. Until
//! it does, both lanes have real false positives after escape analysis. The
//! single switch is [`APPLY_EA_ROUTES_SAFEPOINTS_AND_MEMORY`].
//!
//! ## The φ fallback
//!
//! `ir::PHI_TYPE_FALLBACK` (`Int`) is what `IrBuilder::phi_data_type` answers
//! when `phi_data_type_checked` proves no type — it is infallible by signature,
//! so it cannot decline. It has two triggers, and the verifier treats them
//! differently on purpose:
//!
//! * **a category conflict** (`Ref` merging with `Int`, `Int` with `Long`, …)
//!   is a **violation**. It is a real type conflict, and the fallback is the
//!   unsound-in-the-quiet-direction answer: a reference merge typed `Int` is
//!   invisible to `ir_lower::zero_ref_phi_slots` and to the oop map, so the
//!   merged oop is neither zero-initialised nor reported as a GC root. The
//!   verifier names it rather than letting it reach the lowerer.
//! * **no typed value input at all** (every value edge `NO_NODE`, out of range,
//!   removed, or `Void`) is **tolerated**. That is a slot which is dead at the
//!   merge, which verified bytecode is allowed to have; the always-on lane
//!   already accepts `NO_NODE` in a φ value slot, and the fallback's `Int`
//!   keeps it out of the oop map, which is the correct answer for a slot
//!   nothing reads.
//!
//! ## Enabling lanes
//!
//! `CRATONVM_JIT_VERIFY_TYPES=1`, `CRATONVM_JIT_VERIFY_FRAME_STATES=1`,
//! `CRATONVM_JIT_VERIFY_MEMORY_CHAIN=1`, `CRATONVM_JIT_VERIFY_ARENA_ORDER=1`,
//! or the compatibility alias `CRATONVM_JIT_VERIFY_SCHEDULE=1` (which enables
//! the two lanes the old combined name covered). [`VerifyOptions::all`] turns
//! everything on directly.

use crate::bailout::{Bailout, BailoutReason, CompileResult};
use crate::ir::{join_data_type, Graph, IrType, Node, NodeId, Op, NO_NODE};

// ── Options ──────────────────────────────────────────────────────────

/// Whether `apply_ea_to_ir` (`jit/src/lib.rs`) keeps `graph.safepoints` and the
/// memory-token chain consistent when it deletes a scalar-replaced allocation.
///
/// **This constant is the entire "turn the remaining lanes on at `pre-lower`"
/// change.** Flip it to `true` once `apply_ea_to_ir`, in addition to rewiring
/// `graph.nodes`, (a) retargets or clears every `graph.safepoints` slot naming
/// a node it marks `Op::Dead`, and (b) splices each killed store/allocation out
/// of the memory-token chain the way `ir_optimize::kill_store_splicing_memory_chain`
/// does. Nothing else needs to change: [`VerifyOptions::for_phase`] reads it.
///
/// It is `false` today because `apply_ea_to_ir` does neither — it marks the
/// allocation, its eliminated stores and its replaced loads `Op::Dead` and
/// clears their inputs, and its "replace all references" loop iterates
/// `ir_graph.nodes` only. So after escape analysis a snapshot slot can name a
/// removed node (frame-state lane) and a surviving memory op can take its token
/// from one (memory-chain lane), and both lanes would reject correct-enough
/// graphs at `"post-escape-analysis"` and `"pre-lower"`.
pub const APPLY_EA_ROUTES_SAFEPOINTS_AND_MEMORY: bool = false;

/// The phase name `lib.rs` passes for the verification that runs immediately
/// after `ir_optimize::optimize` and *before* `apply_ea_to_ir`. This is the one
/// hook at which every lane `ir_optimize` fixed is known clean.
pub const PHASE_POST_OPTIMIZE: &str = "post-optimize";

/// Which optional verification lanes to run. The structural lane is not
/// listed because it is unconditional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Check the type lattice ([`crate::ir::join_data_type`]): φ joins, and
    /// reference/float/integer flow into arithmetic.
    pub check_types: bool,
    /// Check safepoint snapshots (deopt frame states).
    pub check_frame_states: bool,
    /// Check memory-token chain integrity: no memory operation may take its
    /// incoming token from a removed node.
    pub check_memory_chain: bool,
    /// Check definition-before-use in *arena* order.
    ///
    /// A heuristic, not a soundness property — a `Graph` carries no schedule,
    /// and GVN appends a replacement node after the users it rewires to it — so
    /// this fires on essentially every optimized graph and is never enabled by
    /// [`VerifyOptions::for_phase`]. See the module docs.
    pub check_arena_order: bool,
}

impl Default for VerifyOptions {
    /// Types, frame states and the memory chain on; arena order off. This is
    /// the "I built this graph and expect it to be clean" setting used by tests
    /// and by explicit stress runs — *not* what the production hooks use (those
    /// use [`VerifyOptions::for_phase`]).
    fn default() -> Self {
        VerifyOptions {
            check_types: true,
            check_frame_states: true,
            check_memory_chain: true,
            check_arena_order: false,
        }
    }
}

impl VerifyOptions {
    /// Structural lane only.
    pub const fn structural() -> Self {
        VerifyOptions {
            check_types: false,
            check_frame_states: false,
            check_memory_chain: false,
            check_arena_order: false,
        }
    }

    /// Every lane, including the heuristic arena-order one.
    pub const fn all() -> Self {
        VerifyOptions {
            check_types: true,
            check_frame_states: true,
            check_memory_chain: true,
            check_arena_order: true,
        }
    }

    /// Structural lane plus whichever optional lanes the environment enables.
    ///
    /// Purely the environment: no lane is on unless a flag says so. Production
    /// hooks want [`VerifyOptions::for_phase`], which layers the
    /// known-clean-at-this-phase defaults on top of this.
    ///
    /// `CRATONVM_JIT_VERIFY_SCHEDULE` is kept as a compatibility alias for the
    /// two lanes that used to be one `check_schedule` flag, so an operator's
    /// existing incantation keeps meaning what it meant.
    pub fn from_env() -> Self {
        let schedule = env_flag("CRATONVM_JIT_VERIFY_SCHEDULE").unwrap_or(false);
        VerifyOptions {
            check_types: env_flag("CRATONVM_JIT_VERIFY_TYPES").unwrap_or(false),
            check_frame_states: env_flag("CRATONVM_JIT_VERIFY_FRAME_STATES").unwrap_or(false),
            check_memory_chain: env_flag("CRATONVM_JIT_VERIFY_MEMORY_CHAIN").unwrap_or(schedule),
            check_arena_order: env_flag("CRATONVM_JIT_VERIFY_ARENA_ORDER").unwrap_or(schedule),
        }
    }

    /// The lanes to run at `phase`: [`VerifyOptions::from_env`] plus every lane
    /// that is known clean *at that point in the pipeline*.
    ///
    /// This is what the compiler's hooks should use. The environment can only
    /// add lanes here, never remove them — an operator who needs a lane off
    /// wholesale has `CRATONVM_JIT_VERIFY_IR=0`, which disables the gate rather
    /// than silently narrowing it.
    ///
    /// Phase model:
    ///
    /// * [`PHASE_POST_OPTIMIZE`] runs between `ir_optimize::optimize` and
    ///   `apply_ea_to_ir`, so the frame-state and memory-chain lanes are on:
    ///   `eliminate_dead_nodes` roots and normalises snapshot slots, and
    ///   `kill_store_splicing_memory_chain` keeps the token chain closed.
    /// * every other phase (`"post-escape-analysis"`, `"pre-lower"`) runs after
    ///   `apply_ea_to_ir`, which does neither, so those two lanes stay opt-in
    ///   until [`APPLY_EA_ROUTES_SAFEPOINTS_AND_MEMORY`] says otherwise.
    /// * the arena-order lane is never enabled here at any phase — it is a
    ///   heuristic that GVN violates by construction (module docs).
    pub fn for_phase(phase: &str) -> Self {
        let env = VerifyOptions::from_env();
        // Before EA, or after an EA that has learnt to route both.
        let ea_clean = phase == PHASE_POST_OPTIMIZE || APPLY_EA_ROUTES_SAFEPOINTS_AND_MEMORY;
        VerifyOptions {
            check_types: env.check_types,
            check_frame_states: env.check_frame_states || ea_clean,
            check_memory_chain: env.check_memory_chain || ea_clean,
            check_arena_order: env.check_arena_order,
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
    if opts.check_memory_chain {
        check_memory_chain(graph, &mut v);
    }
    if opts.check_arena_order {
        check_arena_order(graph, &mut v);
    }

    v.into_result(phase)
}

/// [`verify_graph`] with the lane selection [`VerifyOptions::for_phase`] picks
/// for `phase`. The shape a pipeline hook wants: it knows its phase name and
/// nothing else about verification policy.
pub fn verify_graph_at_phase(graph: &Graph, phase: &str) -> CompileResult<()> {
    verify_graph(graph, phase, VerifyOptions::for_phase(phase))
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
/// broken ordering edge (the memory-chain lane's business), while a *data*
/// input pointing at a removed node is a guaranteed wrong-value read — a hard
/// structural violation reported by the always-on edge lane. Collapsing the two
/// would either lose the wrong-value finding or reject graphs on an ordering
/// complaint from the lane that may not have false positives.
///
/// **This must agree with `ir_optimize::memory_token_slot`**, which is the
/// predicate the *producer* side uses when it splices a killed store out of the
/// chain: a splice that disagreed with this would leave exactly the edge this
/// checks pointing at a dead node. The two differed in two ways and both are
/// reconciled here:
///
/// * `Op::ArrayLength` is `[ctrl, mem, array_ref]` per `ir::Op` and
///   `memory_token_slot` classifies it; this function used to omit it, so a
///   removed token in an `ArrayLength` was reported by the *structural* lane as
///   a wrong-value read. It is now classified.
/// * `memory_token_slot` guards each op with the minimum arity of its
///   documented `[ctrl, mem, …]` form, because `store_operands`/`load_base`
///   also accept compact hand-built and EA-bridge layouts whose slot 1 is a
///   *value* (a compact `Store` is `[base, value]`). The same guard is applied
///   here, with the same numbers. For every op but `ArrayLength` this is a
///   no-op on graphs that pass the arity lane; for `ArrayLength`, whose arity
///   range is deliberately `1..=3`, it is what distinguishes the production
///   `[ctrl, mem, array]` form from the hand-built `[array]` one.
fn is_memory_token_input(node: &Node, input_index: usize) -> bool {
    // A memory phi's value inputs are all tokens, one per predecessor.
    if matches!(node.op, Op::Phi) {
        return node.ty == IrType::Memory && input_index >= 1;
    }
    // Minimum input count of the documented full `[ctrl, mem, …]` form. Kept
    // numerically identical to `ir_optimize::memory_token_slot`.
    let min_full_arity = match node.op {
        Op::Load(_) => 3,           // [ctrl, mem, base]
        Op::Store(_) => 4,          // [ctrl, mem, base, value]
        Op::ArrayLoad(_) => 4,      // [ctrl, mem, array, index]
        Op::ArrayStore(_) => 5,     // [ctrl, mem, array, index, value]
        Op::ArrayLength => 3,       // [ctrl, mem, array_ref]
        Op::New { .. } => 2,        // [ctrl, mem]
        Op::NewArray { .. } => 3,   // [ctrl, mem, length]
        Op::Call { .. } => 2,       // [ctrl, mem, args…]
        Op::LambdaIntToDouble => 4, // [ctrl, mem, lambda, index]
        _ => return false,
    };
    input_index == 1 && node.inputs.len() >= min_full_arity
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
                // the memory-chain lane; see `is_memory_token_input`.
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

/// Human name for an [`IrType`], for violation messages.
///
/// There is deliberately no *category* coarsening here any more. This lane used
/// to fold `Int`/`Long` into one "integer" category and `Float`/`Double` into
/// one "floating-point" category, and join within a category. That private
/// lattice is gone: [`crate::ir::join_data_type`] is the lattice the compiler
/// itself folds over a φ's inputs, it rejects `Int ⊔ Long` and `Float ⊔ Double`,
/// and a verifier that accepted joins the compiler rejects would prove nothing
/// about the compiler. Per-type names keep the messages as specific as the
/// lattice now is.
fn type_name(t: IrType) -> &'static str {
    match t {
        IrType::Int => "int",
        IrType::Long => "long",
        IrType::Float => "float",
        IrType::Double => "double",
        IrType::Ref => "reference",
        IrType::Memory => "memory",
        IrType::Control => "control",
        IrType::Void => "void",
    }
}

/// Ops whose result type must equal every operand's type.
///
/// Comparisons (`Cmp`/`LCmp`/`FCmp`) and conversions (`I2L`, `D2I`, …) are
/// excluded by construction: crossing types is what they are *for*.
/// Shifts are included but only for operand 0 — `lshl` shifts a `Long` by an
/// `Int`, which under the unified lattice is a genuine `Int`/`Long` non-join
/// and would otherwise be reported.
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

/// Number of leading operands of `op` whose type must match the result.
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

/// Fold [`crate::ir::join_data_type`] over a φ's value inputs exactly the way
/// `Graph::phi_data_type_checked` does, and report the *first* conflict.
///
/// `Ok(None)` means "no value input carried a type at all" — the tolerated
/// half of the φ fallback (see the module docs); `Ok(Some(t))` is the proven
/// join; `Err((prev, ty))` is a genuine non-join, the half that is a violation.
///
/// Kept structurally parallel to `phi_data_type_checked` on purpose: the whole
/// point of the unification is that the verifier answers the same question the
/// builder did, so if that function's skip rules change this one must follow.
fn phi_join(graph: &Graph, node: &Node) -> Result<Option<IrType>, (IrType, IrType)> {
    let mut joined: Option<IrType> = None;
    for &inp in node.inputs.iter().skip(1) {
        // `NO_NODE`, out of range, removed, or `Void`: no information, and not
        // on its own a conflict — same rule as `phi_data_type_checked`.
        let ty = match node_at(graph, inp) {
            Some(src) if src.op != Op::Dead && src.ty != IrType::Void => src.ty,
            _ => continue,
        };
        joined = Some(match joined {
            None => ty,
            Some(prev) => join_data_type(prev, ty).ok_or((prev, ty))?,
        });
    }
    Ok(joined)
}

fn check_types(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let id = idx as NodeId;

        // Phi: the declared type must be the join of the value inputs.
        if node.op == Op::Phi {
            // A *memory* phi joins scheduling tokens, not values, and the
            // builder deliberately reuses a value node as a token
            // (`self.mem = load`, where the load is `IrType::Int`). Its inputs
            // are therefore heterogeneous by design.
            if node.ty == IrType::Memory {
                continue;
            }
            match phi_join(graph, node) {
                Err((prev, ty)) => v.add(format!(
                    "{} joins incompatible input types ({} and {}) — ir::join_data_type has no \
                     join for them, so ir::PHI_TYPE_FALLBACK ({}) is what the builder recorded",
                    label(graph, id),
                    type_name(prev),
                    type_name(ty),
                    type_name(crate::ir::PHI_TYPE_FALLBACK)
                )),
                // No typed value input: a slot that is dead at this merge. The
                // tolerated half of the φ fallback — see the module docs.
                Ok(None) => {}
                Ok(Some(j)) => {
                    if j != node.ty {
                        v.add(format!(
                            "{} is typed {} but joins {} inputs",
                            label(graph, id),
                            type_name(node.ty),
                            type_name(j)
                        ));
                    }
                }
            }
            continue;
        }

        // Non-phi: control/void tokens may not flow into a data use, and a
        // homogeneous arithmetic node's operands must share its type.
        let data_idx = data_input_indices(node);
        let result_ty = node.ty;
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
            let ty = src.ty;
            if matches!(ty, IrType::Control | IrType::Void) {
                v.add(format!(
                    "{} input[{i}] = {} is a {} token used as a data value",
                    label(graph, id),
                    label(graph, inp),
                    type_name(ty)
                ));
                continue;
            }
            if homogeneous && i < homogeneous_upto && ty != result_ty {
                v.add(format!(
                    "{} produces {} but input[{i}] = {} is {}",
                    label(graph, id),
                    type_name(result_ty),
                    label(graph, inp),
                    type_name(ty)
                ));
            }
        }
    }
}

// ── Lane: frame states ───────────────────────────────────────────────

/// Safepoint snapshots must reference live nodes and describe a plausible
/// interpreter frame.
///
/// `ir_optimize::eliminate_dead_nodes` now roots every value a snapshot names
/// and normalises an already-stranded slot to `NO_NODE`, so this lane is clean
/// after `ir_optimize::optimize` and [`VerifyOptions::for_phase`] enables it at
/// [`PHASE_POST_OPTIMIZE`]. It is *not* yet clean after `apply_ea_to_ir`, which
/// marks nodes `Op::Dead` without touching `graph.safepoints` — see
/// [`APPLY_EA_ROUTES_SAFEPOINTS_AND_MEMORY`].
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

// ── Lane: memory chain ───────────────────────────────────────────────

/// Memory-token chain integrity: no live memory operation may take its incoming
/// token from a removed node.
///
/// A token names the previous writer, and it is the only ordering relation the
/// IR has between two writes. A token pointing at an `Op::Dead` node has lost
/// the transitive dependency on everything that wrote before it, so the
/// scheduler is free to hoist the operation above those writes — a wrong-code
/// risk, not untidiness.
///
/// This was the *second half* of a combined `check_schedule` lane. It is split
/// out because the two halves answer unrelated questions and have opposite
/// readiness: this one is a soundness property that `ir_optimize` now upholds
/// (`kill_store_splicing_memory_chain` rewires consumers to the killed store's
/// own token, and declines the deletion when it cannot), so
/// [`VerifyOptions::for_phase`] enables it at [`PHASE_POST_OPTIMIZE`]. Its old
/// bunkmate, [`check_arena_order`], is a heuristic that fires on every optimized
/// graph.
fn check_memory_chain(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        for (i, &inp) in node.inputs.iter().enumerate() {
            if inp == NO_NODE || inp as usize >= graph.nodes.len() {
                continue;
            }
            if is_memory_token_input(node, i) && !is_live(graph, inp) {
                v.add(format!(
                    "{} takes its memory token from removed node n{inp} — the ordering edge to \
                     everything that wrote before it is gone",
                    label(graph, idx as NodeId)
                ));
            }
        }
    }
}

// ── Lane: arena order ────────────────────────────────────────────────

/// Definition-before-use approximated by *arena* order.
///
/// A `Graph` carries no schedule, so there is no real definition-before-use
/// question to ask: this lane substitutes arena index, on the observation that
/// `Graph::add` appends, so a node the front end built always has
/// smaller-numbered inputs. Phis (loop back-edges) and merges/regions
/// (back-edge control) are exempt by construction.
///
/// **This is a heuristic and it stays opt-in.** GVN rewires a user to a
/// replacement node it appended *after* that user, and `apply_ea_to_ir` appends
/// the `Const(0)` default for a never-stored scalar-replaced field the same way,
/// so a correct optimized graph violates it as a matter of routine.
/// [`VerifyOptions::for_phase`] therefore never enables it at any phase; it is
/// for hand-built graphs, front-end output, and a human asking "did something
/// rewire this backwards?". Dropping it outright would lose the one cheap check
/// that catches a front end emitting a use before its definition, which is why
/// it survives as its own flag rather than being deleted.
fn check_arena_order(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        if matches!(node.op, Op::Phi | Op::Merge | Op::Region) {
            continue;
        }
        let id = idx as NodeId;
        for (i, &inp) in node.inputs.iter().enumerate() {
            if inp == NO_NODE || inp as usize >= graph.nodes.len() {
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
        assert!(m.contains("incompatible input types"), "{m}");
        assert!(m.contains("reference"), "{m}");
        // The message names the fallback the builder would have recorded, so a
        // reader does not have to know `ir::PHI_TYPE_FALLBACK` by heart.
        assert!(m.contains(type_name(crate::ir::PHI_TYPE_FALLBACK)), "{m}");
        // …and the structural lane alone must NOT reject it: this is a type
        // defect, not a shape defect.
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    }

    /// The unified lattice's headline consequence: `ir::join_data_type` rejects
    /// an `Int`/`Long` merge, and the verifier — which now folds that exact
    /// function — must reject it too. Under the old private `Cat` lattice both
    /// were the one `Integer` category and this graph verified clean, so the
    /// type lane was proving something weaker than the compiler enforces.
    #[test]
    fn int_and_long_no_longer_share_a_category() {
        assert!(
            crate::ir::join_data_type(IrType::Int, IrType::Long).is_none(),
            "the compiler's lattice must reject int/long for this lane to"
        );
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        let a = g.nodes[phi as usize].inputs[1];
        g.nodes[a as usize].ty = IrType::Long;
        let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("incompatible input types"), "{m}");
        assert!(m.contains("long"), "{m}");
        assert!(m.contains("int"), "{m}");
    }

    /// The other half of the same unification: `Float`/`Double` were one
    /// "floating-point" category and are now distinct.
    #[test]
    fn float_and_double_no_longer_share_a_category() {
        assert!(crate::ir::join_data_type(IrType::Float, IrType::Double).is_none());
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        g.nodes[phi as usize].ty = IrType::Float;
        let a = g.nodes[phi as usize].inputs[1];
        let b = g.nodes[phi as usize].inputs[2];
        g.nodes[a as usize].ty = IrType::Float;
        g.nodes[b as usize].ty = IrType::Double;
        let err = verify_graph(&g, "test", VerifyOptions::all()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("incompatible input types"), "{m}");
        assert!(m.contains("double"), "{m}");
    }

    /// The φ fallback, conflict half: a real type conflict IS a violation, and
    /// the always-on lane must still accept the shape. See the module docs.
    #[test]
    fn phi_type_fallback_on_a_conflict_is_a_violation() {
        assert_eq!(crate::ir::PHI_TYPE_FALLBACK, IrType::Int);
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        // Exactly the shape `phi_data_type` cannot type: a Ref and an Int
        // merging, with the φ carrying the `Int` fallback the builder recorded.
        let a = g.nodes[phi as usize].inputs[1];
        g.nodes[a as usize].ty = IrType::Ref;
        assert_eq!(g.nodes[phi as usize].ty, crate::ir::PHI_TYPE_FALLBACK);
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
        assert!(verify_graph(&g, "test", VerifyOptions::default()).is_err());
    }

    /// The φ fallback, degenerate half: a φ no input types is a slot that is
    /// dead at the merge, which verified bytecode may have. Tolerated — the
    /// fallback's `Int` keeps it out of the oop map, the right answer for a slot
    /// nothing reads.
    #[test]
    fn phi_with_no_typed_input_is_tolerated() {
        let mut g = diamond_graph();
        let phi = g.nodes.iter().position(|n| n.op == Op::Phi).expect("phi") as NodeId;
        let n = g.nodes[phi as usize].inputs.len();
        for i in 1..n {
            g.nodes[phi as usize].inputs[i] = NO_NODE;
        }
        assert_eq!(g.nodes[phi as usize].ty, crate::ir::PHI_TYPE_FALLBACK);
        let r = verify_graph(&g, "test", VerifyOptions::all());
        assert!(r.is_ok(), "{}", r.unwrap_err());
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
        assert!(m.contains("typed int but joins reference"), "{m}");
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
        assert!(m.contains("produces int"), "{m}");
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

    /// A store killed without splicing the memory chain leaves the surviving
    /// store's token slot naming a removed node. That is an *ordering* defect,
    /// not a wrong-value one: the always-on structural lane must not reject it
    /// (see `is_memory_token_input`), and the memory-chain lane must.
    ///
    /// `ir_optimize::eliminate_dead_stores` no longer produces this — it routes
    /// every deletion through `kill_store_splicing_memory_chain` — which is
    /// exactly why the lane is now safe to enable at `"post-optimize"`. The
    /// shape is built by hand here because the pass that used to emit it does
    /// not any more.
    #[test]
    fn removed_memory_token_is_a_chain_finding_not_a_structural_one() {
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
        let lanes_without_the_chain = VerifyOptions {
            check_memory_chain: false,
            ..VerifyOptions::default()
        };
        let r = verify_graph(&g, "test", lanes_without_the_chain);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        // Memory-chain lane: names it.
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("memory token"), "{m}");
        assert!(m.contains(&format!("n{st1}")), "{m}");
        let _ = st2;
    }

    /// `Op::ArrayLength` is `[ctrl, mem, array_ref]` per `ir::Op`, so its input
    /// 1 is a memory token. `ir_optimize::memory_token_slot` has always said so;
    /// this lane used to omit it, which meant a removed token in an
    /// `ArrayLength` was reported by the *structural* lane as a wrong-value read
    /// — the one classification the structural lane may not get wrong, because
    /// it is the lane with no opt-out.
    #[test]
    fn array_length_input_one_is_a_memory_token() {
        let mut g = linear_graph();
        let ctrl = g.nodes[g.exit as usize].inputs[0];
        let mem = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Proj(1)))
            .expect("memory projection") as NodeId;
        let arr = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let st = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, arr, arr, arr],
            None,
        );
        // Deliberately not rewired into the Return: `Graph::add` appends, so a
        // Return consuming it would trip the (heuristic) arena-order lane and
        // muddy what this test is about.
        let _len = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, st, arr], None);
        assert!(verify_graph(&g, "test", VerifyOptions::all()).is_ok());

        g.kill(st);
        // Structural lane stays quiet: this is an ordering edge, not a value.
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
        // The chain lane names it.
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("memory token"), "{m}");
        assert!(m.contains("ArrayLength"), "{m}");
    }

    /// The compact hand-built `[array]` form has no token to lose, and the
    /// arity lane deliberately admits it (`ArrayLength` is `1..=3`). The
    /// min-arity guard borrowed from `ir_optimize::memory_token_slot` is what
    /// keeps the two forms apart.
    #[test]
    fn compact_array_length_has_no_memory_token() {
        let compact = Node {
            op: Op::ArrayLength,
            ty: IrType::Int,
            inputs: vec![7],
            bytecode_pc: None,
        };
        assert!(!is_memory_token_input(&compact, 1));
        let full = Node {
            op: Op::ArrayLength,
            ty: IrType::Int,
            inputs: vec![1, 2, 3],
            bytecode_pc: None,
        };
        assert!(is_memory_token_input(&full, 1));
        assert!(!is_memory_token_input(&full, 0));
        assert!(!is_memory_token_input(&full, 2));
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
