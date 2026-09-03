// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Always-on verifier for the sea-of-nodes IR.
//!
//! ## Why this exists
//!
//! The P0 "JIT correctness" lane of the C2 review
//! (`feature-designs/c2/deep-research-vm-c2.md`) asks for *"a universal IR
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
//! * **arena order** ([`VerifyOptions::check_arena_order`]) —
//!   definition-before-use approximated by arena index. This one is opt-in **and stays
//!   that way**, because it is not a soundness property: a `Graph` carries no
//!   schedule, and GVN legitimately appends a replacement node *after* the
//!   users it rewires to it, so the lane fires on essentially every optimized
//!   graph by construction. It is a debugging aid for hand-built graphs and
//!   front-end output, which is why it was split out of the old combined
//!   `check_schedule` lane rather than left riding along with the memory-chain
//!   check it has nothing in common with.
//!
//! The frame-state and memory-chain lanes are on at `"post-optimize"` and
//! **not** at `"post-escape-analysis"` / `"pre-lower"`, because a second pass
//! runs in between: `apply_ea_to_ir` (in `lib.rs`). It used to mark the
//! scalar-replaced allocation, its stores and its loads `Op::Dead` while
//! rewiring only `graph.nodes`, breaking both. Its two remaining stories are
//! *not* the same, which is why they are two constants and not one:
//!
//! * [`APPLY_EA_SPLICES_MEMORY_CHAIN`] — a plain "not verified yet" switch.
//!   `apply_ea_to_ir` now gates every victim on a feasible splice and rewires
//!   its token consumers, so the lane should be clean after EA; the constant is
//!   the one-line flip once that lands and passes.
//! * [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`] — not a bug, a modelling gap. An
//!   eliminated `Op::New`'s snapshot slot deliberately keeps naming the dead
//!   node, because that slot *is* the virtual-object descriptor. A `&Graph`
//!   carries no `ScalarReplacementMap`, so this module cannot tell that
//!   intentional state from the silent-null it exists to catch.
//!
//! ## The φ fallback
//!
//! `ir::PHI_TYPE_FALLBACK` (`Int`) is what `IrBuilder::phi_data_type` answers
//! when `phi_data_type_checked` proves no type — it is infallible by signature,
//! so it cannot decline. It has two triggers, and the verifier treats them
//! differently on purpose:
//!
//! * **a lattice conflict** (`Ref` merging with `Int`, `Int` with `Long`, …)
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

/// Whether `apply_ea_to_ir` (`jit/src/lib.rs`) splices every node it retires out
/// of the memory-token chain, the way
/// `ir_optimize::kill_store_splicing_memory_chain` does.
///
/// **Flipping this one constant to `true` is the entire "run the memory-chain
/// lane at `"post-escape-analysis"` and `"pre-lower"` too" change** —
/// [`VerifyOptions::for_phase`] is the only reader.
///
/// It was `false` because `apply_ea_to_ir` marked the allocation, its
/// eliminated stores and its replaced loads `Op::Dead` while rewiring only
/// `ir_graph.nodes`, leaving a surviving memory operation's token pointing into
/// the hole. That is no longer the code: `plan_scalar_replacement` now gates
/// every victim on `ea_splice_feasible`, and the kill loop follows each
/// victim's incoming token transitively before rewiring its token consumers to
/// it. The constant is still `false` only because that fix is landing
/// concurrently and this module does not get to declare another module's change
/// verified; flip it once the pipeline builds and its tests pass.
pub const APPLY_EA_SPLICES_MEMORY_CHAIN: bool = false;

/// Whether *every* `graph.safepoints` slot names a live node after
/// `apply_ea_to_ir`.
///
/// Unlike [`APPLY_EA_SPLICES_MEMORY_CHAIN`] this is **not** simply waiting on a
/// fix, and it is why the two were split apart rather than left as one switch.
/// `apply_ea_to_ir` retargets a snapshot slot that names a forwarded load, but
/// a slot naming an *eliminated* `Op::New` is deliberately left pointing at the
/// dead node: that slot **is** the "materialise this virtual object" descriptor
/// that `ir_lower::resolve_frame_state` resolves through
/// `ir_lower::ScalarReplacementMap` into a `FrameValue::VirtualObject`.
/// `plan_scalar_replacement` refuses the elision unless such a descriptor will
/// exist, so the state is intentional and correct — but it is indistinguishable
/// *from this module* from the silent-null case the lane exists to catch,
/// because a `&Graph` carries no `ScalarReplacementMap`.
///
/// So enabling the frame-state lane after escape analysis needs a model change,
/// not a bug fix: either the verifier is handed the scalar-replacement map, or
/// `SafepointSnapshot` grows a way to spell "eliminated, described elsewhere"
/// rather than reusing a stale `NodeId`. Until then the lane runs where it is
/// unambiguous — at [`PHASE_POST_OPTIMIZE`], before EA — and is opt-in after.
pub const APPLY_EA_ROUTES_ALL_SAFEPOINTS: bool = false;

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
    ///   `apply_ea_to_ir`, so those two lanes stay opt-in until
    ///   [`APPLY_EA_SPLICES_MEMORY_CHAIN`] and [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`]
    ///   respectively say otherwise.
    /// * the arena-order lane is never enabled here at any phase — it is a
    ///   heuristic that GVN violates by construction (module docs).
    pub fn for_phase(phase: &str) -> Self {
        let env = VerifyOptions::from_env();
        // `PHASE_POST_OPTIMIZE` is the only hook that runs before
        // `apply_ea_to_ir`; every other phase name is after it.
        let before_ea = phase == PHASE_POST_OPTIMIZE;
        VerifyOptions {
            check_types: env.check_types,
            check_frame_states: env.check_frame_states
                || before_ea
                || APPLY_EA_ROUTES_ALL_SAFEPOINTS,
            check_memory_chain: env.check_memory_chain
                || before_ea
                || APPLY_EA_SPLICES_MEMORY_CHAIN,
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
    // An UNBUILT graph makes no claims, so it cannot violate any.
    //
    // `ir_build` can abandon a method partway (an unsupported opcode, a shape
    // the builder declines) and leave behind the entry skeleton alone — `Start`,
    // its projections, the parameters, and any constants already interned — with
    // no terminator and `exit` never assigned. That is not a severed graph; it
    // is a graph that was never finished, and the pipeline discards it through
    // its own bail path.
    //
    // Verifying it anyway is actively harmful: the caller accumulates the
    // verdict with `ir_verify_bail |= …`, so a stub from an abandoned attempt
    // poisons a LATER successful build of the same method and silently drops it
    // out of the optimizing tier. That regression is what this guard prevents.
    //
    // A genuinely severed terminator is still caught: `check_control` reports
    // "no Op::Return is reachable from the entry" whenever a `Return` exists but
    // cannot be reached, and that case does not land here.
    let unbuilt =
        graph.exit == crate::ir::NO_NODE && !graph.nodes.iter().any(|n| matches!(n.op, Op::Return));
    if unbuilt {
        return Ok(());
    }

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
        | Op::Throw
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
        | Op::ConstString { .. }
        | Op::ConstClass { .. }
        | Op::LoadStatic { .. }
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
    // Delegates to the single table in `ir.rs` (`Op::memory_shape`), which is
    // what `ir_optimize::memory_token_slot` and `lib.rs::ea_memory_token_slot`
    // also read. Three hand-maintained copies of this arity table existed and
    // were only *numerically* identical; a fourth op gaining a memory edge
    // would have had to be added to all three, and a verifier that disagrees
    // with the optimizer about what a token slot is proves nothing.
    crate::ir::memory_token_slot(node) == Some(input_index)
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
        // cov-01 — [ctrl, mem]. Every operand is baked (the literal's address,
        // the CP index, the field's class and index), so there is no value edge
        // at all; what the two edges carry is the position in the control and
        // memory chains a helper call has to keep.
        Op::ConstString { .. } | Op::ConstClass { .. } | Op::LoadStatic { .. } => (2, 2),
        // [ctrl, mem, lambda, index]
        Op::LambdaIntToDouble => (4, 4),
        // [ctrl, cond]
        Op::Guard { .. } => (2, 2),
        // [ctrl, mem, obj]
        Op::MonitorEnter | Op::MonitorExit => (3, 3),
        // cov-05 — [ctrl, mem, obj], same shape as `MonitorEnter` above.
        Op::InstanceOf { .. } | Op::CheckCast { .. } => (3, 3),
        // cov-07 — [ctrl, mem, exc], a terminator like `Op::Return` above but
        // with a fixed arity: unlike a return, a throw always carries a value.
        Op::Throw => (3, 3),
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
        // cov-07: `Op::Throw` is as valid a terminator as `Op::Return` — a
        // method that unconditionally throws (`void fail() { throw new
        // IllegalStateException(); }`) builds no `Op::Return` at all, and
        // that is not a severed graph. `Op::Return`'s own reachability is
        // NOT weakened by this: a graph with a live `Op::Return` still needs
        // it reachable, this only ADDS `Op::Throw` as an equally acceptable
        // way for a control path to end.
        let mut found_terminator = false;
        while let Some(cur) = stack.pop() {
            if matches!(node_at(graph, cur), Some(n) if matches!(n.op, Op::Return | Op::Throw)) {
                found_terminator = true;
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
        let has_terminator = graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Return | Op::Throw));
        if has_terminator && !found_terminator {
            v.add(
                "no Op::Return/Op::Throw terminator is reachable from the entry over control \
                 edges (the graph's terminator was severed)"
                    .to_string(),
            );
        } else if !has_terminator {
            v.add("graph has no live Op::Return/Op::Throw terminator".to_string());
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
/// [`PHASE_POST_OPTIMIZE`]. It is *not* clean after `apply_ea_to_ir`, which
/// deliberately leaves an eliminated `Op::New`'s snapshot slot naming the dead
/// node as the virtual-object descriptor — see [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`]
/// for why that needs a model change rather than a fix.
///
/// Three things are checked: that snapshot bcis are **unique** (both consumers
/// key on the bci and break ties by position, so a duplicate silently discards
/// one frame state), that slot counts are `u16`-plausible, and that every named
/// node is in range and live. See `audits/deopt-metadata-audit.md` §5.
fn check_frame_states(graph: &Graph, v: &mut Violations) {
    // Snapshot bcis must be unique, because both consumers key on the bci and
    // resolve ties by *position*:
    //
    //   * `ir_lower::resolve_frame_state_for_bci` does
    //     `safepoints.iter().position(|s| s.bci == bci)` — first match wins, and
    //     that index is also what binds the frame to its inlined scope
    //     (`InlineScopeTable::snapshot_scope`), so a duplicate binds the frame
    //     to another snapshot's scope;
    //   * `ir_lower::build_deopt_points` maps every snapshot through
    //     `bci_native[sp.bci]`, so duplicates collide on one native offset and
    //     `dedup_by_key` drops all but the first.
    //
    // Either way one of the two frame states is silently discarded and the
    // other is used at a program point it does not describe — a *plausible*
    // deopt frame rather than the correct one, which is the failure mode with
    // no visible symptom. The front end's linear bytecode walk visits each `pc`
    // once, so this holds today; the check is what keeps it holding.
    let mut first_at: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        if let Some(&prev) = first_at.get(&sp.bci) {
            v.add(format!(
                "safepoint[{si}] and safepoint[{prev}] both describe bci {} — the bci-keyed \
                 consumers take the first match, so one of the two frame states is silently \
                 discarded and the other resumes a program point it does not describe",
                sp.bci
            ));
        } else {
            first_at.insert(sp.bci, si);
        }
    }

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
            uses: Default::default(),
            receiver_param: None,
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
            uses: Default::default(),
            receiver_param: None,
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

    /// The φ fallback, degenerate half: a φ no input of which carries a type is
    /// a slot that is dead at the merge, which verified bytecode may have — the
    /// always-on lane already accepts `NO_NODE` there. Tolerated: the
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
        // which is exactly what the (opt-in, heuristic) arena-order lane
        // reports. `Shl` is checked only on operand 0, so the `Int` shift
        // amount against a `Long` result is not an Int/Long non-join finding.
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
            inputs: vec![7].into(),
            bytecode_pc: None,
        };
        assert!(!is_memory_token_input(&compact, 1));
        let full = Node {
            op: Op::ArrayLength,
            ty: IrType::Int,
            inputs: vec![1, 2, 3].into(),
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

    /// Both snapshot consumers key on the bci and resolve ties by position, so
    /// two snapshots at one bci silently discard one of the two frame states.
    /// The survivor then describes a program point that is not the one it
    /// resumes at — a wrong frame with no symptom at the point of the defect.
    #[test]
    fn two_snapshots_at_one_bci_are_rejected() {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![a],
            stack: vec![],
        });
        // A second, DIFFERENT frame state at the same bci.
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![b],
            stack: vec![],
        });
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("both describe bci 4"), "{m}");
        assert!(m.contains("silently discarded"), "{m}");
    }

    /// Distinct bcis are the normal case and must stay clean, including when
    /// they name the same node.
    #[test]
    fn distinct_snapshot_bcis_are_accepted() {
        let mut g = linear_graph();
        let k = g.add(Op::Const(1), IrType::Int, vec![], None);
        for bci in [2usize, 4, 7] {
            g.safepoints.push(crate::ir::SafepointSnapshot {
                bci,
                locals: vec![k],
                stack: vec![],
            });
        }
        assert!(verify_graph(&g, "test", VerifyOptions::default()).is_ok());
    }

    /// The duplicate-bci check lives in the frame-state lane, so it follows
    /// that lane's opt-in rules rather than silently becoming an always-on
    /// structural check.
    #[test]
    fn the_duplicate_bci_check_is_part_of_the_frame_state_lane() {
        let mut g = linear_graph();
        let k = g.add(Op::Const(1), IrType::Int, vec![], None);
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![k],
            stack: vec![],
        });
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![k],
            stack: vec![],
        });
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
        assert!(verify_graph(&g, "test", VerifyOptions::default()).is_err());
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
            uses: Default::default(),
            receiver_param: None,
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
            uses: Default::default(),
            receiver_param: None,
        };
        assert!(verify_graph(&g, "empty", VerifyOptions::all()).is_err());
    }

    #[test]
    fn arena_order_lane_flags_a_use_before_its_definition() {
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

    /// The split is the point: an arena-order finding must not drag the
    /// memory-chain lane in with it, and vice versa. They used to be one
    /// `check_schedule` flag, which meant the soundness half could not be
    /// enabled without the heuristic half's guaranteed false positives.
    #[test]
    fn the_two_ordering_lanes_are_independent() {
        let mut g = linear_graph();
        let later = g.add(Op::Const(5), IrType::Int, vec![], None);
        g.nodes[g.exit as usize].inputs[1] = later;

        let arena_only = VerifyOptions {
            check_arena_order: true,
            check_memory_chain: false,
            ..VerifyOptions::structural()
        };
        let chain_only = VerifyOptions {
            check_arena_order: false,
            check_memory_chain: true,
            ..VerifyOptions::structural()
        };
        // An arena-order violation is invisible to the chain lane…
        let r = verify_graph(&g, "test", chain_only);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert!(message(&verify_graph(&g, "test", arena_only).unwrap_err())
            .contains("defined after its use"));

        // …and a broken memory token is invisible to the arena-order lane.
        let ctrl = g.nodes[g.exit as usize].inputs[0];
        let mem = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Proj(1)))
            .expect("memory projection") as NodeId;
        let st1 = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, later, later, later],
            None,
        );
        let _st2 = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, st1, later, later, later],
            None,
        );
        g.kill(st1);
        assert!(
            message(&verify_graph(&g, "test", chain_only).unwrap_err()).contains("memory token")
        );
        let arena_msg = message(&verify_graph(&g, "test", arena_only).unwrap_err());
        assert!(arena_msg.contains("defined after its use"), "{arena_msg}");
        assert!(!arena_msg.contains("memory token"), "{arena_msg}");
    }

    #[test]
    fn options_from_env_defaults_to_structural_only() {
        // The env vars are not set in the test harness, so `from_env` must be
        // the structural-only configuration. `from_env` is deliberately *pure*
        // environment: the phase-aware defaults live in `for_phase`.
        let unset = |n: &str| cratonvm_types::flags::runtime_var_os(n).is_none();
        let o = VerifyOptions::from_env();
        if unset("CRATONVM_JIT_VERIFY_TYPES") {
            assert!(!o.check_types);
        }
        if unset("CRATONVM_JIT_VERIFY_FRAME_STATES") {
            assert!(!o.check_frame_states);
        }
        if unset("CRATONVM_JIT_VERIFY_MEMORY_CHAIN") && unset("CRATONVM_JIT_VERIFY_SCHEDULE") {
            assert!(!o.check_memory_chain);
        }
        if unset("CRATONVM_JIT_VERIFY_ARENA_ORDER") && unset("CRATONVM_JIT_VERIFY_SCHEDULE") {
            assert!(!o.check_arena_order);
        }
    }

    /// The lanes `ir_optimize`'s fixes made safe are on at `"post-optimize"`,
    /// and only there until `apply_ea_to_ir` catches up. The heuristic
    /// arena-order lane is on at no phase.
    #[test]
    fn for_phase_enables_the_ea_blocked_lanes_only_before_escape_analysis() {
        let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
        assert!(post_opt.check_frame_states);
        assert!(post_opt.check_memory_chain);
        assert!(!post_opt.check_arena_order);

        let env = VerifyOptions::from_env();
        for phase in ["post-escape-analysis", "pre-lower"] {
            let o = VerifyOptions::for_phase(phase);
            assert!(!o.check_arena_order, "{phase}");
            // Flipping either constant is the whole change; these state the
            // invariant either way rather than pinning today's value, so a flip
            // needs no test edit.
            assert_eq!(
                o.check_frame_states,
                APPLY_EA_ROUTES_ALL_SAFEPOINTS || env.check_frame_states,
                "{phase}"
            );
            assert_eq!(
                o.check_memory_chain,
                APPLY_EA_SPLICES_MEMORY_CHAIN || env.check_memory_chain,
                "{phase}"
            );
        }
    }

    /// The two EA constants gate *different* lanes. They were one switch until
    /// it became clear they are waiting on different things — a fix versus a
    /// model change — and a test that could not tell them apart would let them
    /// silently fuse back together.
    #[test]
    fn the_two_ea_constants_gate_different_lanes() {
        let env = VerifyOptions::from_env();
        let pre_lower = VerifyOptions::for_phase("pre-lower");
        assert_eq!(
            pre_lower.check_memory_chain,
            APPLY_EA_SPLICES_MEMORY_CHAIN || env.check_memory_chain
        );
        assert_eq!(
            pre_lower.check_frame_states,
            APPLY_EA_ROUTES_ALL_SAFEPOINTS || env.check_frame_states
        );
        // Neither constant has any effect at the pre-EA hook: both lanes are
        // unconditionally on there.
        let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
        assert!(post_opt.check_memory_chain && post_opt.check_frame_states);
    }

    /// The environment may only *add* lanes to a phase's defaults, never
    /// subtract: an operator turning the gate down uses `CRATONVM_JIT_VERIFY_IR=0`,
    /// which disables it outright rather than silently narrowing it.
    #[test]
    fn for_phase_never_drops_a_phase_default() {
        let env = VerifyOptions::from_env();
        for phase in [PHASE_POST_OPTIMIZE, "post-escape-analysis", "pre-lower"] {
            let o = VerifyOptions::for_phase(phase);
            assert!(!env.check_types || o.check_types, "{phase}");
            assert!(!env.check_frame_states || o.check_frame_states, "{phase}");
            assert!(!env.check_memory_chain || o.check_memory_chain, "{phase}");
            assert!(!env.check_arena_order || o.check_arena_order, "{phase}");
        }
    }

    #[test]
    fn verify_graph_at_phase_matches_verify_graph_with_for_phase() {
        let g = diamond_graph();
        for phase in [PHASE_POST_OPTIMIZE, "pre-lower"] {
            let a = verify_graph_at_phase(&g, phase);
            let b = verify_graph(&g, phase, VerifyOptions::for_phase(phase));
            assert_eq!(a.is_ok(), b.is_ok(), "{phase}");
        }
    }

    /// The module doc makes claims about the code it verifies. The ones that are
    /// checkable from here are checked here, so the doc cannot rot back into the
    /// state this consolidation found it in.
    #[test]
    fn module_doc_claims_match_observable_behaviour() {
        // "the lattice is `ir::join_data_type`, and it rejects an Int/Long
        // merge" — the claim that retired the private `Cat` lattice.
        assert!(crate::ir::join_data_type(IrType::Int, IrType::Long).is_none());
        assert!(crate::ir::join_data_type(IrType::Float, IrType::Double).is_none());
        assert_eq!(
            crate::ir::join_data_type(IrType::Ref, IrType::Ref),
            Some(IrType::Ref)
        );
        assert!(crate::ir::join_data_type(IrType::Void, IrType::Void).is_none());

        // "`ir::PHI_TYPE_FALLBACK` is `Int`, and it is not a GC root type."
        assert_eq!(crate::ir::PHI_TYPE_FALLBACK, IrType::Int);
        assert_ne!(crate::ir::PHI_TYPE_FALLBACK, IrType::Ref);

        // "the frame-state and memory-chain lanes are on at post-optimize, the
        // arena-order lane at no phase."
        let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
        assert!(post_opt.check_frame_states && post_opt.check_memory_chain);
        assert!(!post_opt.check_arena_order);
        assert!(!VerifyOptions::for_phase("pre-lower").check_arena_order);

        // "the two EA constants are the switches for the post-EA phases."
        let env = VerifyOptions::from_env();
        let pre_lower = VerifyOptions::for_phase("pre-lower");
        assert_eq!(
            pre_lower.check_frame_states,
            APPLY_EA_ROUTES_ALL_SAFEPOINTS || env.check_frame_states
        );
        assert_eq!(
            pre_lower.check_memory_chain,
            APPLY_EA_SPLICES_MEMORY_CHAIN || env.check_memory_chain
        );

        // "`VerifyOptions::all` turns everything on."
        let all = VerifyOptions::all();
        assert!(
            all.check_types
                && all.check_frame_states
                && all.check_memory_chain
                && all.check_arena_order
        );
        let none = VerifyOptions::structural();
        assert!(
            !none.check_types
                && !none.check_frame_states
                && !none.check_memory_chain
                && !none.check_arena_order
        );
    }

    #[test]
    fn verify_enabled_matches_the_build_profile_when_unset() {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_VERIFY_IR").is_none() {
            assert_eq!(verify_enabled(), cfg!(debug_assertions));
            assert!(!pre_lower_verify_disabled());
        }
    }
}
