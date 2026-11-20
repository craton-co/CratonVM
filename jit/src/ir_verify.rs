// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Always-on verifier for the sea-of-nodes IR.
//!
//! ## Why this exists
//!
//! The P0 "JIT correctness" lane of the C2 review
//! (`deep-research-vm-c2.md`) asks for *"a universal IR
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
//! The *structural* lane (edge validity, arity, projection indices, phi/merge
//! alignment, control integrity) always runs — it is the class of defect that produces silent
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
//! Round 11 wave 2 widened three lanes without adding a flag: the
//! memory-chain lane also requires a token's producer to be a memory state
//! (`check_memory_chain`), the frame-state lane also requires every node that
//! can deopt to have a snapshot at its resume bci (`check_deopt_coverage`) and
//! runs `Graph::verify_use_lists`, and a staged lane checks that every control
//! anchor is a block head the scheduler can map (`check_control_anchors`).
//!
//! Round 11 wave 3 added the def-use **dominance** check (`check_dominance`)
//! to that staged control-structure lane: every use is dominated by its
//! definition, evaluated over the scheduler's own CFG and schedule-early
//! placement. It runs by default in debug builds at `"post-build"`,
//! `"post-optimize"` and `"pre-lower"`, anywhere under
//! `CRATONVM_JIT_VERIFY_BRANCHES=1`, nowhere under `=0`; its frame-state half
//! only when `CRATONVM_JIT_VERIFY_FRAME_STATES=1` is set explicitly.
//!
//! Round 11 wave 6 promoted the anchor half (`check_control_anchors`) into
//! the always-on lane at `"pre-lower"`, so a default release build now runs
//! the structural lane **plus** that check in front of the scheduler; only an
//! explicit `CRATONVM_JIT_VERIFY_BRANCHES=0` removes it there.
//!
//! Round 11 wave 8 added one builder-output rule at `"post-build"`, debug
//! builds only (`check_builder_phis_have_no_undefined_input`): no data φ that
//! survived `IrBuilder::retire_untypeable_phis` merges a `NO_NODE`.
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
//!
//!   Since 2026-09-18 the front end does not build one for the case that is
//!   LEGAL — a slot javac reuses for two types, which is dead (TOP) at the
//!   join. `IrBuilder::activate_merge` leaves such a slot undefined instead
//!   of building the φ, and `IrBuilder::build` retires the loop-header φs
//!   that only turn out to conflict once their back edge arrives. What is
//!   left for this lane to find is a conflicting φ something actually READS,
//!   which is a genuine front-end typing defect.
//! * **no typed value input at all** (every value edge `NO_NODE`, out of range,
//!   removed, or `Void`) is **tolerated**. That is a slot which is dead at the
//!   merge, which verified bytecode is allowed to have; the always-on lane
//!   already accepts `NO_NODE` in a φ value slot, and the fallback's `Int`
//!   keeps it out of the oop map, which is the correct answer for a slot
//!   nothing reads.
//!
//! ## Enabling lanes
//!
//! `CRATONVM_JIT_VERIFY_TYPES`, `CRATONVM_JIT_VERIFY_FRAME_STATES`,
//! `CRATONVM_JIT_VERIFY_MEMORY_CHAIN`, `CRATONVM_JIT_VERIFY_ARENA_ORDER`, or
//! the compatibility alias `CRATONVM_JIT_VERIFY_SCHEDULE` (which covers the two
//! lanes the old combined `check_schedule` flag covered). Each is a tri-state:
//! `=1` forces the lane on, `=0` forces it off, unset takes the default.
//! [`VerifyOptions::all`] turns everything on directly.
//!
//! ## The sub-check defaults now follow the master gate (2026-09-16)
//!
//! Until 2026-09-16 the master gate [`verify_enabled`] defaulted to
//! `cfg!(debug_assertions)` while **every optional lane defaulted to `false`
//! even in a debug build**. The consequence was that the unit tests in this
//! crate, the `ir_vs_singlepass` suite and the PR difftest lane all ran with
//! the structural lane only: no type checking, no frame-state checking, no
//! memory-chain checking. The three lanes whose defects this module's history
//! is written about ran nowhere except
//! `.github/workflows/jit-differential-nightly.yml`, on a fixed seed range —
//! i.e. the checks were documented as fixed and then not exercised by the tests
//! that would have caught a regression.
//!
//! [`subchecks_follow_the_master_gate`] is the switch that changed that. All
//! four lane defaults read it; three take it directly, and the arena-order lane
//! takes it through [`ARENA_ORDER_FOLLOWS_THE_MASTER_GATE`], which is `false` —
//! see below. The explicit environment override still works in both directions:
//! `CRATONVM_JIT_VERIFY_TYPES=0` turns that one lane off in a debug build, `=1`
//! turns it on in a release build.
//!
//! ### Why arena order is exempt
//!
//! It is not a soundness property and it fires on essentially every optimized
//! graph by construction (GVN appends a replacement node *after* the users it
//! rewires to it). Defaulting it on would not surface latent bugs, it would
//! make [`VerifyOptions::for_phase`] — which propagates `env.check_arena_order`
//! unchanged at every phase — bail out of every optimized compile in every
//! debug build, which is the JIT being off rather than being checked.
//! [`ARENA_ORDER_FOLLOWS_THE_MASTER_GATE`] is the one line to flip if someone
//! wants to argue otherwise; the `check_arena_order` lane's own doc explains
//! why they should not.
//!
//! ### What turning these on is expected to surface
//!
//! This is a prediction, not a report: the change was made without a build, so
//! nothing below has been observed. It is written down so that the first person
//! to see a new failure can tell an expected shape from a surprise.
//!
//! **First, what the flip does *not* reach.** The frame-state and memory-chain
//! lanes were already on at [`PHASE_POST_OPTIMIZE`] unconditionally, and they
//! remain **off** at `"post-escape-analysis"` and `"pre-lower"` unless an
//! operator forces them with an explicit `=1`. That is deliberate and it is
//! argued at [`VerifyOptions::for_phase`]: a lane that is on only because the
//! build profile says so runs solely where the phase declares it clean, so a
//! *default* can never override [`APPLY_EA_SPLICES_MEMORY_CHAIN`] or
//! [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`]. Without that rule the flip would have
//! turned the frame-state lane on at exactly the two hooks whose declaring
//! constant says it fires on *correct* graphs — every scalar-replaced
//! allocation live across a safepoint — and the visible result would have been
//! a silent throughput regression, not a bug report.
//!
//! So the flip's real reach is **the types lane, at every hook**, and the other
//! two nowhere they were not already. In descending order of how likely each is
//! to fire:
//!
//! 1. **Types, on φ nodes.** By far the most likely, because this lane ran at
//!    no hook at all before. It demands that a non-`Memory` φ's declared type
//!    *equal* the [`join_data_type`] fold of its value inputs. Two shapes are
//!    expected. (a) A φ that took `ir::PHI_TYPE_FALLBACK` (`Int`) because its
//!    inputs did not join — reported as "joins incompatible input types", which
//!    is the violation this lane exists for and is a real find if it appears.
//!    (b) A **memory** φ that is not typed `IrType::Memory`: the lane skips a φ
//!    only on `node.ty == IrType::Memory`, and the builder deliberately reuses
//!    a value node as a memory token (`self.mem = load`, where the load is
//!    `IrType::Int`), so a token φ that inherited `Int` from such a producer is
//!    *not* skipped and is then compared against a heterogeneous join. (b) is a
//!    verifier-model gap rather than wrong code, and the fix is to type the φ
//!    `Memory`, not to loosen the lane. Since 2026-09-17 `check_types` is
//!    forced on at [`PHASE_POST_OPTIMIZE`] by [`VerifyOptions::for_phase`]
//!    exactly the way the frame-state and memory-chain lanes are, and is
//!    propagated from the environment after that — so a φ like that bails the
//!    compile at `"post-optimize"` in every build **in which that hook runs**
//!    (since round 11 wave 16 that is every build profile, gated on
//!    [`post_optimize_verify_enabled`]: a default release build reaches it
//!    unless `CRATONVM_JIT_VERIFY_POST_OPTIMIZE=0` or
//!    `CRATONVM_JIT_VERIFY_IR=0`; before that wave it ran only under
//!    [`verify_enabled`]),
//!    and still bails at `"pre-lower"` whenever the environment leaves the
//!    lane on there (that hook runs *unconditionally*, gating on
//!    [`pre_lower_verify_disabled`] rather than on [`verify_enabled`]; in a
//!    default release build the environment leaves the type lane off there,
//!    so at `"pre-lower"` a shipped build runs the structural lane — with the
//!    control-anchor check since round 11 wave 6 — and nothing else).
//! 2. **Types, on homogeneous arithmetic.** `Add`/`Sub`/`Mul`/… must have every
//!    operand typed like the result (shifts: operand 0 only, per
//!    `homogeneous_operand_count`). The expected shape is an `Int`/`Long`
//!    mix — a `Long` `Op::Add` fed by a `Const` the builder typed `Int`, which
//!    is what a hand-built test graph and a constant-folded index expression
//!    both tend to look like. Cheap to fix at the producer; the lane is right.
//! 3. **Frame states and memory chain at [`PHASE_POST_OPTIMIZE`].** Unchanged
//!    by the flip — they were already unconditional there — so a new failure in
//!    one of these is not attributable to this change and should be read as a
//!    genuine `ir_optimize` regression.
//! 4. **Frame states after `apply_ea_to_ir`, for an operator who asks.**
//!    `CRATONVM_JIT_VERIFY_FRAME_STATES=1` still reaches the post-EA hooks, and
//!    it is expected to fire at once: [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`] says
//!    in as many words that an eliminated `Op::New`'s snapshot slot
//!    deliberately keeps naming the dead node, because that slot *is* the
//!    virtual-object descriptor `ir_lower::resolve_frame_state` resolves
//!    through a `ScalarReplacementMap` the verifier is not handed. Expected
//!    message: `safepoint[i] at bci N local[k] = nM refers to a removed (Dead)
//!    node`. That is the modelling gap, reachable on demand so it can be closed
//!    rather than forgotten — not a reason to weaken the lane. (Round 11 wave
//!    18 closed it at `"pre-lower"`: that hook hands the verifier the map's
//!    keys, [`verify_graph_with_scalar_replaced`], and the lane runs there by
//!    default in debug builds. The `"post-escape-analysis"` hook still has no
//!    map, and runs before `ir_prune_unconsumable_snapshots`.)
//! 5. **Memory chain after `apply_ea_to_ir`, likewise on demand.** Least likely
//!    to have anything to say. [`APPLY_EA_SPLICES_MEMORY_CHAIN`]'s own doc says
//!    the splice is now gated on `ea_splice_feasible` and the lane "should be
//!    clean after EA"; the constant is `false` only because this module does
//!    not get to declare another module's change verified. If
//!    `CRATONVM_JIT_VERIFY_MEMORY_CHAIN=1` stays quiet across the difftest lane,
//!    that constant can be flipped to `true` on the evidence.
//!
//! If the types lane proves too disruptive, the narrow answer is
//! `CRATONVM_JIT_VERIFY_TYPES=0` and the wide one is
//! [`subchecks_follow_the_master_gate`]. Neither is the right *first* answer:
//! every shape above is either a real find or a producer that should be typing
//! its node correctly.
//!
//! The always-on structural lane is unchanged, and
//! [`subchecks_follow_the_master_gate`] is `false` in a release build, so no
//! shipped configuration becomes stricter.

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
///
/// Round 11 wave 18 took the first route for the `"pre-lower"` hook:
/// [`verify_graph_with_scalar_replaced`] carries the map's keys, and there the
/// lane runs by the debug-build default. This constant stays `false` because
/// it describes the `&Graph`-only entry point, which still cannot tell.
pub const APPLY_EA_ROUTES_ALL_SAFEPOINTS: bool = false;

/// The phase name `lib.rs` passes for the verification that runs immediately
/// after `IrBuilder::build` returns a graph, before any pass has touched it.
///
/// This module's header quotes the requirement as "run after parsing and every
/// mutating pass in stress builds", and until 2026-09-17 parsing was exactly
/// where it did not run: the first hook was [`PHASE_POST_OPTIMIZE`]. A
/// front-end defect — a dangling edge, a `NO_NODE` outside a φ, a φ whose
/// input count does not match its merge's predecessor count — was therefore
/// handed first to `ir_optimize`, which indexes `graph.nodes` by raw `NodeId`
/// and is the pass most likely to *panic* on one; and a defect that survived
/// the optimizer was reported against `"post-optimize"`, naming the wrong
/// suspect.
///
/// # Which lanes run here (and why only the structural one)
///
/// [`VerifyOptions::for_phase`] gives this phase the structural lane and
/// nothing else by default. That is not a claim that the builder's output is
/// badly typed; it is a refusal to make a claim either way in the change that
/// introduces the hook. The three optional lanes are *declarations about the
/// pipeline*, and each has a reason it cannot be declared here yet:
///
/// * **types** — [`PHASE_POST_OPTIMIZE`] runs this lane unconditionally, in
///   every build profile, so a genuine lattice conflict is already caught one
///   pass later. Turning it on here as well can only change behaviour for a
///   graph whose conflict `ir_optimize` *removes* (DCE dropping the node, GVN
///   folding it), and for that graph the change is from "compiles" to
///   "silently falls back to the single-pass tier". Fleet-wide, on a lane
///   whose findings nobody has read yet at this phase. Opt in with
///   `CRATONVM_JIT_VERIFY_TYPES=1`, read the difftest lane, then promote.
/// * **frame states** — `ir_optimize::eliminate_dead_nodes` is what roots and
///   normalises snapshot slots. Before it runs, a snapshot naming a node the
///   builder abandoned is not yet known to be a defect.
/// * **memory chain** — likewise: `kill_store_splicing_memory_chain` is what
///   closes the token chain, and it has not run.
///
/// An explicit `=1` still forces any of them on here, which is how those
/// declarations get established rather than forgotten.
pub const PHASE_POST_BUILD: &str = "post-build";

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

    /// Structural lane plus whichever optional lanes the environment enables,
    /// over the build-profile default that [`subchecks_follow_the_master_gate`]
    /// supplies.
    ///
    /// Until 2026-09-16 this was *purely* the environment: every optional lane
    /// answered `false` unless a variable said otherwise, in a debug build as
    /// much as a release one, so the whole test suite ran structural-only while
    /// [`verify_enabled`] advertised that verification was on. The four
    /// defaults now read [`subchecks_follow_the_master_gate`] — the arena-order
    /// lane through [`ARENA_ORDER_FOLLOWS_THE_MASTER_GATE`], which is `false`
    /// and says why. See the module docs for what this is expected to surface.
    ///
    /// The environment still wins in **both** directions here:
    /// `env_flag` is a tri-state, so `CRATONVM_JIT_VERIFY_TYPES=0` turns that
    /// lane off in a debug build and `=1` turns it on in a release build.
    /// (`VerifyOptions::for_phase` is the one that can only *add*; that
    /// asymmetry is deliberate and unchanged.)
    ///
    /// `CRATONVM_JIT_VERIFY_SCHEDULE` is kept as a compatibility alias for the
    /// two lanes that used to be one `check_schedule` flag, so an operator's
    /// existing incantation keeps meaning what it meant: it still forces both
    /// lanes on, and — because it is ORed rather than substituted — setting it
    /// to `0` no longer has to mean "off", which it never reliably did.
    pub fn from_env() -> Self {
        let schedule = env_flag("CRATONVM_JIT_VERIFY_SCHEDULE").unwrap_or(false);
        let gated = subchecks_follow_the_master_gate();
        VerifyOptions {
            check_types: env_flag("CRATONVM_JIT_VERIFY_TYPES").unwrap_or(gated),
            check_frame_states: env_flag("CRATONVM_JIT_VERIFY_FRAME_STATES").unwrap_or(gated),
            check_memory_chain: env_flag("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                .unwrap_or(schedule || gated),
            // NOT plain `gated`: `for_phase` propagates this value unchanged at
            // every phase, and this lane fires on essentially every optimized
            // graph by construction, so defaulting it on would bail out of every
            // debug compile rather than check it. The exemption is one constant
            // so that it is one line to revisit, and so that this lane still
            // *reads* the master gate like the other three.
            check_arena_order: env_flag("CRATONVM_JIT_VERIFY_ARENA_ORDER")
                .unwrap_or(schedule || (ARENA_ORDER_FOLLOWS_THE_MASTER_GATE && gated)),
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
    /// * [`PHASE_POST_BUILD`] runs on the builder's own output, before any
    ///   pass, so it declares nothing clean beyond the structural lane — an
    ///   explicit `=1` is the only way to add one there. That constant says
    ///   why, lane by lane.
    /// * [`PHASE_POST_OPTIMIZE`] runs between `ir_optimize::optimize` and
    ///   `apply_ea_to_ir`, so the frame-state and memory-chain lanes are on:
    ///   `eliminate_dead_nodes` roots and normalises snapshot slots, and
    ///   `kill_store_splicing_memory_chain` keeps the token chain closed. The
    ///   **type** lane is on there too, in every build profile — see the
    ///   comment on the field below for why that one is not merely a default.
    /// * every other phase (`"post-escape-analysis"`, `"pre-lower"`) runs after
    ///   `apply_ea_to_ir`, so those two lanes stay opt-in until
    ///   [`APPLY_EA_SPLICES_MEMORY_CHAIN`] and [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`]
    ///   respectively say otherwise.
    /// * the arena-order lane is never enabled here at any phase — it is a
    ///   heuristic that GVN violates by construction (module docs).
    pub fn for_phase(phase: &str) -> Self {
        let env = VerifyOptions::from_env();
        // ── PHASE_POST_BUILD: structural lane, plus whatever is FORCED ─────
        //
        // The one phase that runs before any pass, so it is the one phase
        // whose optional lanes no pass has established. Note this is an early
        // return rather than another `||` term: `env.check_types` defaults to
        // the build profile, so leaving this phase to fall through would have
        // turned the type lane on here in every debug build — the broad new
        // requirement `NOTES-irfront.md` §4 asks to be staged behind a flag,
        // arriving by default instead. See [`PHASE_POST_BUILD`] for the
        // per-lane reasoning.
        if phase == PHASE_POST_BUILD {
            let forced = |name: &str| env_flag(name) == Some(true);
            let schedule_forced = forced("CRATONVM_JIT_VERIFY_SCHEDULE");
            return VerifyOptions {
                check_types: forced("CRATONVM_JIT_VERIFY_TYPES"),
                check_frame_states: forced("CRATONVM_JIT_VERIFY_FRAME_STATES"),
                check_memory_chain: forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN") || schedule_forced,
                // The heuristic lane, which GVN violates by construction and
                // which is enabled at no phase; `env` is how an operator asks
                // for it, exactly as at every other phase.
                check_arena_order: env.check_arena_order,
            };
        }
        // `PHASE_POST_OPTIMIZE` is the only hook that runs before
        // `apply_ea_to_ir`; every other phase name is after it.
        let before_ea = phase == PHASE_POST_OPTIMIZE;
        // ── A DEFAULT may not override a phase declaration (2026-09-16) ────
        //
        // The two lanes below are declared clean at `PHASE_POST_OPTIMIZE` and
        // NOT clean after `apply_ea_to_ir` — that is what
        // [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`] and
        // [`APPLY_EA_SPLICES_MEMORY_CHAIN`] are, and both are `false`.
        //
        // This used to read `env.check_frame_states || before_ea || …`, and
        // while every lane defaulted to `false` that was the same thing as
        // "an operator asked for it". Once the defaults started following the
        // master gate (see [`subchecks_follow_the_master_gate`]), it stopped
        // being the same thing: a debug build now has `env.check_frame_states
        // == true` for nobody's reason in particular, and the OR then turned
        // the lane on at exactly the two phases whose declaring constants say
        // it is unverified there.
        //
        // That is not a theoretical concern — it fires, on a *correct* graph.
        // [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`] explains why: an eliminated
        // `Op::New`'s snapshot slot deliberately keeps naming the dead node,
        // because that slot IS the virtual-object descriptor
        // `ir_lower::resolve_frame_state` resolves through a
        // `ScalarReplacementMap` a `&Graph` does not carry. Every method whose
        // escape analysis scalar-replaces an allocation live across a
        // safepoint bailed the IR tier and fell back to single-pass — silently,
        // as a throughput regression with no failing test, which is the
        // failure mode this crate's history is most full of.
        //
        // So the rule is now explicit: **an EXPLICIT `=1` forces a lane on at
        // any phase; a lane that is on only because the build profile says so
        // runs solely where the phase declares it clean.** An operator can
        // still see the post-EA findings on demand
        // (`CRATONVM_JIT_VERIFY_FRAME_STATES=1`), which is how the modelling
        // gap gets closed rather than forgotten; nothing is weakened, because
        // the lane's reach at `PHASE_POST_OPTIMIZE` — where the flip's real
        // value is — is unchanged.
        let forced = |name: &str| env_flag(name) == Some(true);
        let schedule_forced = forced("CRATONVM_JIT_VERIFY_SCHEDULE");
        VerifyOptions {
            // ── The type lane is UNCONDITIONAL before EA (2026-09-17) ──────
            //
            // `env.check_types` alone left this lane off in every *release*
            // build, because `subchecks_follow_the_master_gate` is
            // `cfg!(debug_assertions)`. That made the module header above false
            // in the configuration that matters: it argues at length that a
            // lattice conflict "is a **violation** … a reference merge typed
            // `Int` is invisible to `ir_lower::zero_ref_phi_slots` and to the
            // oop map, so the merged oop is neither zero-initialised nor
            // reported as a GC root. The verifier names it rather than letting
            // it reach the lowerer." It did not — not in any shipped build, so
            // every "the verifier would catch this" claim about a front-end
            // typing bug was false for one.
            //
            // `before_ea` is the right scope. It is the same phase declaration
            // the two lanes below use; it runs on the graph `ir_optimize` just
            // finished, so a finding is attributable to a pass; and unlike
            // those two the type lane has no known false positive on front-end
            // output — the one shape the module header predicts, a memory φ
            // that inherited `Int` from a value node reused as a token, is a
            // producer bug the lane is right about. The cost is one linear pass
            // over the arena with no allocation.
            //
            // The asymmetry is deliberate and matches the lanes below: an
            // explicit `CRATONVM_JIT_VERIFY_TYPES=0` still turns the lane off
            // at `"post-escape-analysis"` and `"pre-lower"`, but it no longer
            // turns it off at `PHASE_POST_OPTIMIZE`. An operator who wants
            // verification off wholesale has `CRATONVM_JIT_VERIFY_IR=0`, which
            // disables the gate rather than silently narrowing it.
            //
            // ## What "every build profile" does and does not mean
            // (2026-09-18)
            //
            // It is every profile *in which the hook runs*. Until round 11
            // wave 16 that was a narrower set than the paragraphs above
            // suggest: `lib.rs` called the `PHASE_POST_OPTIMIZE` hook only
            // under [`verify_enabled`], so a default RELEASE build ran only
            // `"pre-lower"`, where this lane follows `env.check_types` —
            // `false` there — and type-checked nothing. Since wave 16 the
            // `PHASE_POST_OPTIMIZE` hook is gated on
            // [`post_optimize_verify_enabled`] and runs in release builds too
            // (kill switch `CRATONVM_JIT_VERIFY_POST_OPTIMIZE=0`); the
            // `"post-escape-analysis"` hook keeps the `verify_enabled` gate.
            // That is also why the builder now declines
            // the φs the lane would report on legal bytecode
            // (`ir::phi_inputs_conflict`): in those builds every one of them
            // was a silent fall-back to the single-pass tier.
            check_types: env.check_types || before_ea,
            check_frame_states: forced("CRATONVM_JIT_VERIFY_FRAME_STATES")
                || before_ea
                || APPLY_EA_ROUTES_ALL_SAFEPOINTS,
            check_memory_chain: forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                || schedule_forced
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
/// Read per compile, no `static` (j27 LATCHES), so a per-VM or per-thread
/// override reaches it.
pub fn verify_enabled() -> bool {
    env_flag("CRATONVM_JIT_VERIFY_IR").unwrap_or(cfg!(debug_assertions))
}

/// The **default** for the four optional lanes in [`VerifyOptions::from_env`]:
/// the same `cfg!(debug_assertions)` expression [`verify_enabled`] uses, so a
/// debug build that says it verifies actually runs the strong checks.
///
/// Before 2026-09-16 the master gate defaulted to the build profile while each
/// sub-check defaulted to `false`, so a debug build ran the structural lane and
/// nothing else. The unit tests, `jit/tests/ir_vs_singlepass.rs` and the PR
/// difftest lane therefore never type-checked a graph, never checked a frame
/// state and never checked a memory token; only
/// `.github/workflows/jit-differential-nightly.yml` did, on a fixed seed range.
///
/// # The kill switch
///
/// **Flip `FOLLOW` below to `false` to restore the pre-2026-09-16 behaviour.**
/// That is the entire revert: all four defaults read this one function, so no
/// other line has to change and no test has to be edited — the tests
/// `sub_check_defaults_follow_the_master_gate` and
/// `verify_enabled_matches_the_build_profile_when_unset` are written against
/// this function rather than against today's value of it.
///
/// It is a function and not a `pub const` on purpose: a `const` would be
/// inlined into every caller's documentation as a fixed `true`/`false`, and the
/// value is build-profile dependent.
pub fn subchecks_follow_the_master_gate() -> bool {
    // ── KILL SWITCH ── flip to `false` to restore pre-2026-09-16 behaviour.
    const FOLLOW: bool = true;
    FOLLOW && cfg!(debug_assertions)
}

/// Whether the arena-order lane joins the other three in following
/// [`subchecks_follow_the_master_gate`]. It does **not**, and this is the line
/// to change if someone decides otherwise.
///
/// The lane approximates definition-before-use by arena index, which is not a
/// property a `Graph` has: GVN appends a replacement node *after* the users it
/// rewires to it, and `apply_ea_to_ir` appends a `Const(0)` default the same
/// way, so a *correct* optimized graph violates it routinely. The other three
/// lanes were off by accident; this one is off on the merits.
///
/// The consequence of flipping it is not "more findings": it is
/// [`VerifyOptions::for_phase`] returning `check_arena_order = true` at every
/// phase in every debug build (it propagates the environment value unchanged),
/// which turns the pre-lowering gate into a bailout for essentially every
/// optimized compile. `CRATONVM_JIT_VERIFY_ARENA_ORDER=1` remains the way to
/// ask for it deliberately, on a graph you built yourself.
pub const ARENA_ORDER_FOLLOWS_THE_MASTER_GATE: bool = false;

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
    env_flag("CRATONVM_JIT_VERIFY_IR") == Some(false)
}

/// Whether `lib.rs` runs the [`PHASE_POST_OPTIMIZE`] hook (round 11 wave 16).
///
/// Until this wave that hook ran only under [`verify_enabled`], so a default
/// RELEASE build checked the structural lane (plus the control anchors) at
/// `"pre-lower"` and nothing else: the type, frame-state and memory-chain
/// lanes, which [`VerifyOptions::for_phase`] declares clean exactly at
/// `"post-optimize"`, ran in no shipped build
/// (`r11-irback-release-builds-verify-structure-only`, item 1). The released
/// checks have found real defects the tests passed over twice this round (the
/// page's waves 7 and 11), which is the argument for running the strongest
/// declared-clean hook where the probes and benchmarks actually run.
///
/// In a release build this hook runs structural + types + frame states
/// (including deopt coverage and the use-list check) + memory chain; the
/// staged control-structure and branch-completeness lanes stay debug-only
/// (they key on [`subchecks_follow_the_master_gate`]). Every one of those is a
/// linear pass over the arena or the snapshot list with hash sets, next to an
/// `ir_optimize::optimize` that is super-linear; `ea_ir_bridge::ir_verify_reject`
/// charges it to `metrics::Phase::Verify` (`CRATONVM_JIT_METRICS=1`).
///
/// A finding takes the pipeline's ordinary failure path (single-pass), is
/// counted in `bailout::bailout_counts`, and is printed under the IR stage
/// reporting flag, exactly as in a debug build.
///
/// * debug builds, or `CRATONVM_JIT_VERIFY_IR=1`: on, as before;
/// * `CRATONVM_JIT_VERIFY_IR=0`: off, like every other hook;
/// * otherwise on, unless `CRATONVM_JIT_VERIFY_POST_OPTIMIZE=0` (the kill
///   switch that restores the pre-wave-16 release behaviour).
///
/// Read per call, not latched (no new process static): the hook consults it
/// once per optimizing compile.
pub fn post_optimize_verify_enabled() -> bool {
    if verify_enabled() {
        return true;
    }
    if pre_lower_verify_disabled() {
        return false;    }
    true}

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
    verify_graph_inner(graph, phase, opts, None)
}

/// [`verify_graph`] for a graph escape analysis has run on, handed the ids of
/// the allocations it scalar-replaced (`ScalarReplacementMap::objects`' keys,
/// every round's).
///
/// This is the model change [`APPLY_EA_ROUTES_ALL_SAFEPOINTS`] asks for, in
/// the shape `r11-irback-release-builds-verify-structure-only` (wave 5) sized
/// it: an eliminated `Op::New`'s snapshot slot keeps naming the dead node on
/// purpose, because that slot IS the virtual-object descriptor
/// `ir_lower::resolve_frame_state` resolves through the map. With the set in
/// hand the frame-state lane accepts a slot naming a removed node exactly when
/// the id is in `scalar_replaced`, and reports every other one as before.
///
/// With the set, the frame-state lane also runs at `"pre-lower"` BY DEFAULT in
/// debug builds (round 11 wave 18; [`frame_state_lane_after_ea_by_default`]):
/// that is the hook in front of the lowerer, after `apply_ea_to_ir` and
/// `ir_prune_unconsumable_snapshots`, whose findings no build had read. Since
/// round 12 wave 1 it runs there in a default RELEASE build too
/// ([`frame_state_lane_after_ea_released`]; kill switch
/// `CRATONVM_JIT_VERIFY_PRE_LOWER_FRAME_STATES_RELEASE=0`);
/// `CRATONVM_JIT_VERIFY_FRAME_STATES=1` forces it anywhere and `=0` turns the
/// default off in every profile.
pub fn verify_graph_with_scalar_replaced(
    graph: &Graph,
    phase: &str,
    opts: VerifyOptions,
    scalar_replaced: &std::collections::HashSet<NodeId>,
) -> CompileResult<()> {
    verify_graph_inner(graph, phase, opts, Some(scalar_replaced))
}

/// Whether the frame-state lane runs at `phase` for a verifier that WAS handed
/// the scalar-replaced set, on top of what [`VerifyOptions::for_phase`] says.
///
/// Only at `"pre-lower"` (the one post-EA hook that runs in every build), by
/// the build-profile default ([`subchecks_follow_the_master_gate`]: debug
/// builds) or the round-12 release promotion
/// ([`frame_state_lane_after_ea_released`]), and an explicit
/// `CRATONVM_JIT_VERIFY_FRAME_STATES=0` wins. An explicit `=1` already reaches
/// the lane through `for_phase`.
fn frame_state_lane_after_ea_by_default(phase: &str) -> bool {
    phase == PHASE_PRE_LOWER
        && env_flag("CRATONVM_JIT_VERIFY_FRAME_STATES").unwrap_or_else(|| {
            subchecks_follow_the_master_gate() || frame_state_lane_after_ea_released()
        })
}

/// Whether [`frame_state_lane_after_ea_by_default`] also holds in a RELEASE
/// build (round 12 wave 1, the promotion step of
/// `r11-irback-release-builds-verify-structure-only`, the `sr_map` hand-off).
///
/// Cost: the lane that already runs in release at `"post-optimize"` (since
/// round 11 wave 16), run once more on the post-EA graph: linear in the
/// snapshot slots and the arena, plus `Graph::verify_use_lists` (a sort per
/// use list) when the lists are current. What it buys is the one hook where a
/// retained snapshot slot naming a removed node that is NOT a scalar-replaced
/// allocation is visible — a deopt that would rebuild a local from nothing,
/// which `ir_lower` cannot tell from a real value.
///
/// Default on; `CRATONVM_JIT_VERIFY_PRE_LOWER_FRAME_STATES_RELEASE=0` restores
/// the debug-only staging (`CRATONVM_JIT_VERIFY_FRAME_STATES=0` still turns the
/// post-EA lane off in every profile). Read per compile, no static.
fn frame_state_lane_after_ea_released() -> bool {
    true}

fn verify_graph_inner(
    graph: &Graph,
    phase: &str,
    opts: VerifyOptions,
    scalar_replaced: Option<&std::collections::HashSet<NodeId>>,
) -> CompileResult<()> {
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
    //
    // ## Recognised STRUCTURALLY (round 11 wave 2)
    //
    // This used to be `exit == NO_NODE && no Op::Return`, which also matches a
    // COMPLETE method that simply has no terminator: `void run() { while (true)
    // { … } }` builds a real loop and never assigns `exit`. Every lane —
    // including the unconditional pre-lower one — was skipped for exactly the
    // graphs that exercised the self-loop back-edge bug `ir_schedule` fixed in
    // round 11. An unbuilt skeleton is now "no control node beyond `Start` and
    // its projections"; a built graph with no terminator runs every lane and is
    // excused only the two "a terminator must exist" findings (`terminatorless`
    // below).
    let has_terminator = graph
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::Return | Op::Throw));
    let terminatorless = graph.exit == NO_NODE && !has_terminator;
    if terminatorless && !has_built_control(graph) {
        return Ok(());
    }

    let mut v = Violations::new();

    // Structural lane — always on.
    check_edges(graph, &mut v);
    check_arity(graph, &mut v);
    check_guard_tokens(graph, &mut v);
    check_projections(graph, &mut v);
    check_phis(graph, &mut v);
    check_control(graph, terminatorless, &mut v);
    // Staged, and keyed on the PHASE rather than on `opts`: see
    // `branch_completeness_lane_enabled`. The control-anchor check shares the
    // lane's staging (round 11 wave 2): both are statements about control
    // structure that the scheduler relies on and the always-on lane does not
    // make, and a new requirement must be read against real graphs in debug
    // builds before it can refuse a release compile.
    if branch_completeness_lane_enabled(phase) {
        check_branch_completeness(graph, &mut v);
    }
    // Round 11 wave 3: the anchor check moved to its own staging, which adds
    // the `"pre-lower"` hook in debug builds (see
    // `control_structure_lane_enabled`), and the def-use dominance check joins
    // it. The dominance check's frame-state half is heavier on assumptions
    // (it needs the resume rule and the splice map), so it runs only when the
    // frame-state lane is FORCED — never by a build-profile default.
    //
    // Round 11 wave 6: the anchor check is promoted into the always-on lane at
    // `"pre-lower"`, in every build profile — the one hook that guards the
    // scheduler, whose Step 1-3 are what drop an edge or an anchor that is not
    // a block head. Evidence: the whole JIT suite under
    // `CRATONVM_JIT_VERIFY_BRANCHES=1 CRATONVM_DBG_IR_COMPILES=1` printed no
    // `verifier rejected` line (wave 5). Only an explicit
    // `CRATONVM_JIT_VERIFY_BRANCHES=0` takes it back out, as a narrower kill
    // switch than `CRATONVM_JIT_VERIFY_IR=0`. Earlier hooks keep the staging:
    // hand-built fixtures anchor on `Start` directly and are verified there.
    let control_lane = control_structure_lane_enabled(phase);
    if control_lane
        || (phase == PHASE_PRE_LOWER && env_flag("CRATONVM_JIT_VERIFY_BRANCHES") != Some(false))
    {
        check_control_anchors(graph, &mut v);
    }
    if control_lane {
        check_dominance(
            graph,
            env_flag("CRATONVM_JIT_VERIFY_FRAME_STATES") == Some(true),
            &mut v,
        );
    }
    // Shapes the arity lane admits for hand-built fixtures but the lowerer
    // cannot read. Only at the hook that guards `ir_lower` itself.
    if phase == PHASE_PRE_LOWER {
        check_lowerable_shapes(graph, &mut v);
    }
    // t1 wave 2 (lane Y): the `int`-constant width invariant `Graph::add`
    // now establishes. Staged like every new requirement: debug builds (the
    // master gate) only, so a producer that rewrites a node in place with a
    // raw unsigned pattern is found by the test suites before it could refuse
    // a release compile.
    if subchecks_follow_the_master_gate() {
        check_int_const_width(graph, &mut v);
    }
    // Round 11 wave 8: the builder's own φ invariant, on the builder's own
    // output. Debug builds only — a new requirement is read against real
    // graphs before it can refuse a release compile (the post-build hook
    // itself runs only where `verify_enabled()` holds).
    if phase == PHASE_POST_BUILD && subchecks_follow_the_master_gate() {
        check_builder_phis_have_no_undefined_input(graph, &mut v);
    }

    if opts.check_types {
        check_types(graph, &mut v);
        // Staged: the shared operand-type table is enforced on an explicit
        // `CRATONVM_JIT_VERIFY_TYPES=1` (`operand_type_lane_enabled`), and —
        // since round 11 wave 18 — at `"post-optimize"` in debug builds
        // (`operand_type_lane_staged_at`), so the test suites read it against
        // real graphs before it may refuse a release compile.
        if operand_type_lane_enabled() || operand_type_lane_staged_at(phase) {
            check_operand_types(graph, &mut v);
        }
    }
    // Round 11 wave 18: a caller that hands over the scalar-replaced set
    // (`verify_graph_with_scalar_replaced`) removes the one reason the lane was
    // declared unclean after escape analysis, so the lane also runs where
    // `frame_state_lane_after_ea_by_default` says.
    let frame_states = opts.check_frame_states
        || (scalar_replaced.is_some() && frame_state_lane_after_ea_by_default(phase));
    if frame_states {
        check_frame_states(graph, scalar_replaced, &mut v);
    } else if phase == PHASE_PRE_LOWER && subchecks_follow_the_master_gate() {
        // Round 11 wave 3: the deopt-COVERAGE half of the frame-state lane,
        // alone, at the one hook after `ir_prune_unconsumable_snapshots`
        // (`r11-irback-deopt-site-without-frame-state-gets-an-empty-frame`).
        // The rest of the lane stays off here for the reason
        // `APPLY_EA_ROUTES_ALL_SAFEPOINTS` gives — an eliminated `Op::New`'s
        // slot keeps naming the dead node — which says nothing about whether a
        // deopt site HAS a snapshot. Debug builds only. The prune keeps the
        // resume bci of every live node `op_cannot_deopt` does not clear —
        // the same set this check asks about — so a finding here is a prune
        // or an `apply_ea_to_ir` rewrite that dropped a snapshot the
        // `"post-optimize"` run of this same check saw present, and the one
        // `ir_lower` would otherwise answer with its no-snapshot latch.
        check_deopt_coverage(graph, &mut v);
    }
    if opts.check_memory_chain {
        check_memory_chain(graph, &mut v);
    }
    if opts.check_arena_order {
        check_arena_order(graph, &mut v);
    }

    v.into_result(phase)
}

/// The phase name [`verify_graph_between_passes`] reports under.
pub const PHASE_BETWEEN_PASSES: &str = "between-passes";

/// JIT round 21 lane stress (FUZZ21-2): `CRATONVM_DBG_IR_VERIFY_EACH_PASS`.
/// Read per call, not latched (no new process static); the optimizer asks it
/// once per cleanup.
pub fn verify_each_pass_enabled() -> bool {
    false}

/// JIT round 21 lane stress (FUZZ21-2): verify a graph BETWEEN two optimizer
/// passes with the lanes [`PHASE_POST_OPTIMIZE`] declares clean -- the
/// structural lane, types, frame states (a snapshot slot naming a `Dead` node,
/// a deopt site without a frame, a token in a value slot) and the memory
/// chain. Every pass starts from a graph `"post-build"` accepted and must hand
/// on one `"post-optimize"` will accept, so a finding here names the first
/// pass that broke an invariant the end of the pipeline checks anyway. The
/// phase-staged lanes keyed on a phase name (branch completeness, control
/// anchors, the builder's φ rule) do not run under this name.
pub fn verify_graph_between_passes(graph: &Graph) -> CompileResult<()> {
    verify_graph(
        graph,
        PHASE_BETWEEN_PASSES,
        VerifyOptions::for_phase(PHASE_POST_OPTIMIZE),
    )
}

/// [`verify_graph`] with the lane selection [`VerifyOptions::for_phase`] picks
/// for `phase`. The shape a pipeline hook wants: it knows its phase name and
/// nothing else about verification policy.
pub fn verify_graph_at_phase(graph: &Graph, phase: &str) -> CompileResult<()> {
    verify_graph(graph, phase, VerifyOptions::for_phase(phase))
}

/// Does `graph` hold any control node beyond the entry skeleton — `Start` and
/// the projections taken directly off it? `false` is the shape an abandoned
/// build leaves (see the `unbuilt` guard in [`verify_graph`]); any branch,
/// join or terminator makes it a graph that claims something.
fn has_built_control(graph: &Graph) -> bool {
    graph.nodes.iter().any(|n| {
        if n.op == Op::Dead || n.op == Op::Start || !n.op.is_control() {
            return false;
        }
        if matches!(n.op, Op::Proj(_)) {
            let off_start = n
                .inputs
                .first()
                .and_then(|&p| node_at(graph, p))
                .is_some_and(|p| p.op == Op::Start);
            return !off_start;
        }
        true
    })
}

/// The phase name of the unconditional hook in front of `ir_lower`
/// (`lib.rs` passes the literal). Private: [`check_lowerable_shapes`] is the
/// one reader.
const PHASE_PRE_LOWER: &str = "pre-lower";

/// Shapes [`expected_arity`] admits because hand-built optimizer fixtures use
/// them, but that `ir_lower` cannot lower (round 11).
///
/// `Op::ArrayLength` is `(1, 4)` in the arity table so the compact `[array]`
/// fixture form verifies, but `ir_lower`'s `Op::ArrayLength` arm reads the
/// array at `inputs[2]` unconditionally — an index panic on the compile
/// thread (a permanent decline once contained) for any 1- or 2-input node
/// that reached it. `IrBuilder` only ever builds 3 or 4 inputs, so this is a
/// refusal no production graph can hit; it turns a would-be panic into the
/// verifier's ordinary bailout.
fn check_lowerable_shapes(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::ArrayLength && node.inputs.len() < 3 {
            v.add(format!(
                "{} has {} input(s); ir_lower reads the array at input[2] and needs the \
                 [ctrl, mem, array(, guard)] form",
                label(graph, idx as NodeId),
                node.inputs.len()
            ));
        }
    }
}

/// An `Op::Const` typed [`IrType::Int`] holds its value SIGN-EXTENDED to
/// 64 bits (`i32::MIN..=i32::MAX`). `Graph::add` normalises every constant it
/// is handed; this catches a node rewritten in place (`node.op =
/// Op::Const(..)`) with an unsigned 32-bit pattern, which every reader of the
/// raw `i64` (`ir_check_elim::int_const`, `range_analysis`, the
/// `offset_predicate` filters) would have to remember to truncate (t1 wave 2,
/// lane Y;
/// `docs/feature-designs/jit-ir-optimizations.md`).
fn check_int_const_width(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if let Op::Const(c) = node.op {
            if node.ty == IrType::Int && i32::try_from(c).is_err() {
                v.add(format!(
                    "{} is an int constant {c:#x} outside i32; an int constant must be \
                     stored sign-extended",
                    label(graph, idx as NodeId)
                ));
            }
        }
    }
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
        Op::Start | Op::If | Op::Switch { .. } | Op::Merge | Op::Region => true,
        Op::Proj(_) => node.ty == IrType::Control,
        _ => false,
    }
}

/// Indices of `node`'s inputs that are control edges.
///
/// A `Range`, not a `Vec` (round 11): every arm is contiguous, and the
/// always-on structural lane calls this up to three times per node, so the
/// `Vec` cost an allocation per node per call on every compile.
///
/// `pub(crate)` (round 11 wave 2) so `ea_ir_bridge::ea_control_preds` can read
/// this table instead of keeping a hand-maintained copy of it, which had
/// already lost `Op::Switch` and the monitor / type-check / unbox ops. The
/// range always lies inside `node.inputs`, so
/// `&node.inputs.as_slice()[control_input_indices(node)]` cannot panic.
pub(crate) fn control_input_indices(node: &Node) -> std::ops::Range<usize> {
    match node.op {
        // A merge takes one control edge per predecessor.
        Op::Merge | Op::Region => 0..node.inputs.len(),
        // Everything else pins control at input 0 (when it has one at all).
        Op::Return
        | Op::Throw
        | Op::If
        | Op::Switch { .. }
        | Op::Proj(_)
        | Op::Guard { .. }
        | Op::Load(_)
        | Op::Store(_)
        | Op::ArrayLoad(_)
        | Op::ArrayStore(_)
        // STRINTR21-5: the String char read, pinned below its guards.
        | Op::StrCharDecode
        | Op::New { .. }
        | Op::NewArray { .. }
        | Op::Call { .. }
        | Op::ConstString { .. }
        | Op::ConstClass { .. }
        | Op::LoadStatic { .. }
        | Op::StoreStatic { .. }
        | Op::LambdaIntToDouble
        | Op::IndyBridge { .. }
        | Op::MultiNewArray { .. }
        | Op::NewCp { .. }
        // Added 2026-09-18. All five are built `[ctrl, mem, obj(, delta)]`
        // by the front end and nowhere else, their arities are fixed (see
        // `expected_arity`), and every one of them can reach the runtime
        // (a monitor transition, a helper call that may deopt), so a control
        // slot naming a data node is precisely the "pinned to nothing"
        // defect this lane exists for. They fell into the catch-all below
        // only because they post-date the list.
        | Op::MonitorEnter
        | Op::MonitorExit
        | Op::InstanceOf { .. }
        | Op::CheckCast { .. }
        | Op::Unbox { .. }
        // Round 11 wave 14 (lane osrmerge): `[ctrl]` alone. The pin is the
        // whole point — the flag sits before its `If`, and the local and the
        // memory state inside the OSR arm (`ir_schedule::pinned_anchor`).
        | Op::OsrEntryFlag { .. }
        | Op::OsrLocal(_)
        | Op::OsrMemory
        // JIT round 19 wave 3 (lane eager3): the class-init test, pinned
        // beside the guard that reads it.
        | Op::ClassInitTest { .. }
        | Op::CurrentThreadMirror { .. }
        | Op::VthreadBoundaryTest { .. }
        // JIT round 20 wave 11 (lane pIndy): a root word read, pinned like
        // the mirror.
        | Op::IndyRootRef { .. }
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the compiled handler
        // entry's flag (before its `If`) and caught throwable (in the arm).
        | Op::CatchEntryFlag { .. }
        | Op::CaughtException
        // nio20 L2: `[ctrl, ref]`, pinned like the ops above.
        | Op::KeepAlive
        // nio21 wave 3 L2: `[ctrl, call]` / `[ctrl]`. The flag sits after its
        // call and before its `If`; the caught throwable sits in the `If`'s
        // true-edge block (`ir_lower::Lowerer::plan_call_throw_edges`).
        | Op::CallThrew { .. } => 0..node.inputs.len().min(1),
        // `Op::ArrayLength` is built with and without a control edge in-tree:
        // `[ctrl, mem, array(, guard)]` by `IrBuilder`, `[array]` by hand-built
        // optimizer fixtures. Keyed on arity the same way
        // `ir::memory_token_slot` tells the two apart (round 11): it used to
        // fall into the catch-all, so the production form's control slot
        // could name a data node and verify clean.
        Op::ArrayLength => {
            if node.inputs.len() >= 3 {
                0..1
            } else {
                0..0
            }
        }
        // `Op::Param` is defined at `Start` but that edge is a definition
        // anchor, not a control-flow edge — not checked here.
        _ => 0..0,
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
    // Delegates to the single table in `ir.rs` (`Op::memory_shape`, φ arm
    // included), which `ir_optimize::memory_token_slot` and
    // `ea_ir_bridge::ea_memory_token_slot` also read. Three hand-maintained
    // copies of this arity table existed and were only *numerically*
    // identical; a verifier that disagrees with the optimizer about what a
    // token slot is proves nothing. (Round 11 wave 4: this kept its own copy
    // of the memory-φ arm until then; `ir::is_memory_token_slot` has the same
    // one.)
    crate::ir::is_memory_token_slot(node, input_index)
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
/// and a `Merge` may have collapsed to a single predecessor. What a control
/// join may NOT have is zero predecessors, and a `Region` may not have one —
/// see the arms themselves for why that changed on 2026-09-17.
///
/// The guarded family (`Load`, `ArrayLoad`, `ArrayLength`, `Div`, `Rem`) also
/// varies by one, for the trailing `Op::Guard` token. The widths here must
/// agree with `ir::Op::guard_shape`, which is the single table the token slot
/// is defined by; `the_guarded_arities_admit_exactly_the_token_slot` in this
/// file's tests reads both and pins the agreement.
fn expected_arity(op: &Op) -> (usize, usize) {
    const ANY: usize = usize::MAX;
    match op {
        Op::Start => (0, 0),
        // [ctrl] or [ctrl, value]
        Op::Return => (1, 2),
        // [ctrl, cond]
        Op::If => (2, 2),
        // ── Predecessor minimums (2026-09-17) ─────────────────────────
        //
        // These used to be `(0, ANY)` for both, on the argument that "a
        // `Merge`/`Region` the front end created but never activated has zero
        // predecessors (an unreachable branch target) — which is not a defect,
        // just dead control". That argument is about a node the front end is
        // still holding; it is not true of a graph handed to a *verification
        // hook*. Every hook but one runs after
        // `ir_optimize::eliminate_dead_nodes`, which kills an unreferenced
        // control join; the one that does not, [`PHASE_POST_BUILD`], sees the
        // builder's output directly, and since 2026-09-18
        // `IrBuilder::build` kills a never-reached, input-less merge itself
        // (`audit_unactivated_merges`) — before that, a spliced body the walk
        // never entered left one behind and this minimum refused the whole
        // graph at `"post-build"`. Meanwhile the looseness cost real coverage:
        //
        // * `ir.rs`'s own pre-scan comment explains at length why an
        //   input-less `Op::Merge` must never exist ("an input-less control
        //   node is exactly the kind of orphan the scheduler/lowerer is not
        //   prepared for") and filters merge targets to reachable pcs for that
        //   reason — but `ensure_merge` creates the node with `vec![]` and only
        //   an activation ever calls `set_inputs`, so nothing checked it.
        // * `Op::Region` is a LOOP HEADER: entry edge plus back edge, two
        //   predecessors by construction. Leaving it at `(0, ANY)` is what made
        //   `IrBuilder::patch_loop_backedge`'s dropped control edge
        //   undetectable — the merge simply had fewer predecessors than the CFG
        //   did, and every φ on it agreed with the reduced count.
        //
        // A one-predecessor `Op::Merge` stays legal: branch folding and dead
        // arm removal legitimately collapse a two-way join into a pass-through
        // before anything re-canonicalises it. A one-predecessor `Region` does
        // not: a loop with no back edge is not a loop.
        //
        // **Round 11 correction: the `Region` minimum protects hand-built
        // graphs only.** `IrBuilder::ensure_merge` creates every join as an
        // `Op::Merge`, and `activate_loop_header` uses that same node as the
        // loop header — no production graph contains an `Op::Region` (see
        // `ir_check_elim`, "a loop header is an Op::Merge in every graph the
        // IR builder produces"). So the `patch_loop_backedge` dropped-edge
        // argument above is not caught here for real loops: a loop-header
        // `Merge` that lost its back edge has one input and passes `(1, ANY)`.
        // Telling a loop header from a forward join needs the dominator tree
        // (a back edge is an input whose block the merge dominates), which
        // this lane does not build.
        Op::Merge => (1, ANY),
        Op::Region => (2, ANY),
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
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr
        | Op::Cmp(_)
        | Op::LCmp
        | Op::FCmp { .. } => (2, 2),
        // `[a, b]` or `[a, b, guard]` — the guard token (`ir::guard_shape`).
        // SPLIT OUT of the `(2, 2)` arithmetic group above, and the split is
        // the whole point: leaving them in is the failure that shows up on no
        // unit test. A hand-built guarded `Load` is easy to write, but the
        // div-zero token exists only on graphs built from real
        // `idiv`/`ldiv`/`irem`/`lrem` bytecode, so a stale `(2, 2)` here would
        // reject exactly those graphs and nothing else.
        Op::Div | Op::Rem => (2, 3),
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
        // [ctrl, mem, base, offset] or [ctrl, mem, base, offset, guard]
        Op::Load(_) => (4, 5),
        // [ctrl, mem, base, offset, value]. No guard token: see
        // `ir::Op::guard_shape` for why the store family is excluded.
        Op::Store(_) => (5, 5),
        // Built as `[ctrl, mem, array]` by the production path and as
        // `[array]` by hand-built optimizer fixtures; `[ctrl, mem, array,
        // guard]` with the guard token.
        Op::ArrayLength => (1, 4),
        // [ctrl, mem, array, index] or [ctrl, mem, array, index, guard]
        Op::ArrayLoad(_) => (4, 5),
        // [ctrl, mem, array, index, value]
        Op::ArrayStore(_) => (5, 5),
        // [ctrl, mem, src, src_pos, dst, dst_pos, len]
        Op::ByteArrayCopy => (7, 7),
        // [ctrl, mem, value, index, coder] or [.., guard] (STRINTR21-5)
        Op::StrCharDecode => (5, 6),
        // [ctrl, mem]
        Op::New { .. } => (2, 2),
        // [ctrl, mem, length]
        Op::NewArray { .. } => (3, 3),
        // [ctrl, mem, args…]
        Op::Call { .. } => (2, ANY),
        // [ctrl, mem, args…] — a bridged `invokedynamic` (round 16, lane indy).
        Op::IndyBridge { .. } => (2, ANY),
        // [ctrl, mem, dims…] — `multianewarray`, at least one dimension
        // (round 16, lane matrix).
        Op::MultiNewArray { .. } => (3, ANY),
        // [ctrl, mem] — a deferred `new` (nio21 w15 L3); the holder and CP
        // index are baked.
        Op::NewCp { .. } => (2, 2),
        // cov-01 — [ctrl, mem]. Every operand is baked (the literal's address,
        // the CP index, the field's class and index), so there is no value edge
        // at all; what the two edges carry is the position in the control and
        // memory chains a helper call has to keep.
        Op::ConstString { .. } | Op::ConstClass { .. } | Op::LoadStatic { .. } => (2, 2),
        // [ctrl, mem, value] — `putstatic`; the class and field are baked.
        Op::StoreStatic { .. } => (3, 3),
        // [ctrl, mem, lambda, index]
        Op::LambdaIntToDouble => (4, 4),
        // [ctrl, cond]
        Op::Guard { .. } => (2, 2),
        // [ctrl, key] — the multi-way branch. Fixed at two like `Op::If`: the
        // case KEYS are not edges, they are the projection indices, so a
        // switch over 255 cases still has exactly two inputs.
        Op::Switch { .. } => (2, 2),
        // Pure arithmetic with NO control or memory edge: one or two data
        // inputs and nothing else. `ScalarOp::arity` is the single source of
        // truth, so a family added there cannot fall out of step with this.
        Op::ScalarIntrinsic(sop) => {
            let n = sop.arity();
            (n, n)
        }
        // [ctrl, mem, obj]
        Op::MonitorEnter | Op::MonitorExit => (3, 3),
        // cov-05 — [ctrl, mem, obj], same shape as `MonitorEnter` above; JIT
        // round 21, lane tcfuse (TCPERF21-3): plus the shared class word
        // `ir_optimize::typecheck_class_word` may append.
        Op::InstanceOf { .. } | Op::CheckCast { .. } => (3, 4),
        // [ctrl, mem, obj] — same shape again. It takes `ctrl` because it can
        // DEOPT (null receiver, or a subclass that overrode the accessor) and
        // `mem` because it reads the instance.
        // [ctrl, mem, receiver] plus a delta for the one family that takes
        // one. `UnboxOp::arity` is the single source of truth, so a family
        // added there cannot fall out of step with this.
        Op::Unbox { op, .. } => {
            let n = 3 + op.arity();
            // JIT round 24 B2: a cached scoped access carries the loop's
            // arena cache inputs after its own (`Graph::ffm_arena_cached`).
            if op.is_scoped() {
                (n, n + crate::ir::SCOPED_ARENA_CACHE_INPUTS)
            } else {
                (n, n)
            }
        }
        // cov-07 — [ctrl, mem, exc], a terminator like `Op::Return` above but
        // with a fixed arity: unlike a return, a throw always carries a value.
        Op::Throw => (3, 3),
        // [obj] — pure, no control or memory edge (round 11 wave 13).
        Op::ExactClassIs { .. } => (1, 1),
        // [obj] -- the same shape (JIT round 19, lane osr19).
        Op::ClassWordOf => (1, 1),
        // [ctrl] — the opaque OSR merge's three ops (round 11 wave 14).
        Op::OsrEntryFlag { .. } | Op::OsrLocal(_) | Op::OsrMemory => (1, 1),
        // [ctrl] — the class-init test (JIT round 19 wave 3, lane eager3).
        Op::ClassInitTest { .. } => (1, 1),
        // [ctrl] -- the current thread's mirror (nio21 wave 19, lane L3).
        Op::CurrentThreadMirror { .. } | Op::VthreadBoundaryTest { .. } => (1, 1),
        // [ctrl] -- a VM root word's object (JIT round 20 wave 11, lane pIndy).
        Op::IndyRootRef { .. } => (1, 1),
        // [ctrl] — the compiled handler entry (JIT round 19 wave 5, lane
        // handler5, SPLICE4-1).
        Op::CatchEntryFlag { .. } | Op::CaughtException => (1, 1),
        // [ctrl, ref] — the reachability-fence keep-alive (nio20 L2).
        Op::KeepAlive => (2, 2),
        // [ctrl, call] / [ctrl] — the call-site exception edge (nio21 w3 L2).
        Op::CallThrew { .. } => (2, 2),
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

// ── Lane: guard tokens ───────────────────────────────────────────────

/// A trailing guard-token slot must name a live `Op::Guard`.
///
/// `ir::guard_token_slot` is arity-keyed, exactly like `memory_token_slot`:
/// it answers *where the token is*, never *whether the edge is well-formed*.
/// That is deliberate — `ir::expected_input_type` and `data_input_indices`
/// both need the answer without a `Graph` in hand — and it leaves one hole
/// this lane closes. A producer that appended an ordinary VALUE to a `Load`
/// would get a node of the guarded arity whose extra operand every consumer
/// then ignores: `access_location`, `load_base` and `ea_ir_bridge` all read
/// the base and offset out of fixed slots, `data_input_indices` filters the
/// trailing slot out of the type lane, and `regalloc` skips it for want of a
/// `wants_loc`. So the operand would be *silently dropped* rather than
/// miscompiled — a lost value, invisible at every layer. Naming it here is
/// what makes that a reported violation.
///
/// Structural, so it is on in every profile and at every phase.
fn check_guard_tokens(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let Some(slot) = crate::ir::guard_token_slot(node) else {
            continue;
        };
        let id = idx as NodeId;
        let Some(&inp) = node.inputs.get(slot) else {
            continue;
        };
        // `NO_NODE` in the slot is the shape `with_guard_token` never builds
        // (it appends nothing when it has no guard), so a node at the guarded
        // arity with an undefined token is a producer defect, not a tolerated
        // "undefined here". `check_edges` already reports it under
        // `no_node_allowed`; say nothing twice.
        if inp == NO_NODE {
            continue;
        }
        match node_at(graph, inp) {
            // Out of range / removed: `check_edges` owns both findings.
            None => {}
            Some(src) if src.op == Op::Dead => {}
            Some(src) if matches!(src.op, Op::Guard { .. }) => {}
            Some(_) => v.add(format!(
                "{} input[{slot}] = {} is in the guard-token slot but is not an Op::Guard",
                label(graph, id),
                label(graph, inp)
            )),
        }
    }
}

// ── Lane: projections ────────────────────────────────────────────────

/// How many outputs a node makes available to `Op::Proj`.
///
/// Three ops in this IR are multi-output. Two produce exactly two:
/// `Op::Start` (`Proj(0)` = the initial control token, `Proj(1)` = the initial
/// memory token — `IrBuilder::new` builds that pair and every consumer assumes
/// it) and `Op::If` (`Proj(0)` = the taken edge, `Proj(1)` = the fall-through;
/// `ir_lower` and `ea_ir_bridge` both select an edge by that index). Everything
/// else produces one value under its own id and is projected from nowhere.
///
/// `Op::Switch` is the third, and the only one whose count is not a literal.
fn projection_arity(op: &Op) -> usize {
    match op {
        Op::Start | Op::If => 2,
        // The cases in key order, then the default edge.
        //
        // `ir::switch_projection_count` is `high - low + 2` computed in `i64`
        // and saturated, and both of those are load-bearing rather than
        // defensive: `high - low` **overflows `i32`** for a full-`int`-range
        // `tableswitch` (`low = i32::MIN, high = i32::MAX` is legal bytecode),
        // and an overflowed subtraction here would produce a small, plausible
        // arity that every out-of-range `Proj` of that switch then passed. A
        // producer that built such a node anyway — `ir::switch_is_dense`
        // refuses it, so `IrBuilder` cannot — has to FAIL this lane, not panic
        // inside the verifier.
        Op::Switch { low, high } => crate::ir::switch_projection_count(*low, *high),
        _ => 0,
    }
}

/// A `Proj`'s index must name an output its producer actually has, and no two
/// `Proj`s may name the same one.
///
/// Neither was checked before 2026-09-17: `Proj(7)` on an `Op::If`, or two
/// `Proj(0)`s on the same `If`, verified clean. Both are silently wrong rather
/// than loudly wrong downstream — `ir_lower` selects the taken edge by
/// *matching* `Op::Proj(0)` among an `If`'s users, so a duplicate makes "the
/// taken edge" whichever one the scan reaches first, and an out-of-range index
/// matches nothing and leaves a control edge unlowered.
///
/// # What is deliberately tolerated
///
/// A `Proj` whose producer is a `Merge`/`Region` forwards a control token that
/// [`produces_control`] already accepts, and hand-built optimizer fixtures use
/// that shape as a plain control forwarder. It is pointless but not unsound, so
/// it is not reported; the index rule above still applies to it, because an
/// index at all on a single-output producer can only be 0.
///
/// `Op::Start` is *not* required here to have both of its projections. The
/// production builder always creates the pair, but a hand-built graph that
/// needs only control legitimately creates one, and rejecting those would
/// refuse fixtures rather than find bugs.
fn check_projections(graph: &Graph, v: &mut Violations) {
    // (producer, index) pairs already seen, so a duplicate is reported once
    // against the second `Proj` rather than twice.
    //
    // A set, not a `Vec` + `contains` (round 11): this lane is structural, so
    // it runs at every hook including the unconditional `"pre-lower"` one in
    // release builds, and the list scan made it O(projections²) — two per
    // `If`, and up to `ir::SWITCH_MAX_PROJECTIONS` per `Switch`.
    let mut seen: std::collections::HashSet<(NodeId, usize)> = std::collections::HashSet::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        let which = match node.op {
            Op::Proj(w) => w as usize,
            _ => continue,
        };
        let id = idx as NodeId;
        let Some(&producer_id) = node.inputs.first() else {
            // Arity lane already reported the empty input list.
            continue;
        };
        let Some(producer) = node_at(graph, producer_id) else {
            // Edge lane already reported the bad reference.
            continue;
        };
        if producer.op == Op::Dead {
            continue;
        }
        // A control join is a one-output producer; see the doc above.
        let outputs = match &producer.op {
            Op::Merge | Op::Region => 1,
            other => projection_arity(other),
        };
        if outputs == 0 {
            v.add(format!(
                "{} projects {} which produces no projections (only Start, If and Switch are \
                 multi-output)",
                label(graph, id),
                label(graph, producer_id)
            ));
            continue;
        }
        if which >= outputs {
            v.add(format!(
                "{} selects output {which} of {}, which has {outputs}",
                label(graph, id),
                label(graph, producer_id)
            ));
            continue;
        }
        if !seen.insert((producer_id, which)) {
            v.add(format!(
                "{} is a second Proj({which}) of {} (a projection names one output, and \
                 ir_lower picks an edge by matching the index)",
                label(graph, id),
                label(graph, producer_id)
            ));
            continue;
        }
    }
}

// ── Lane: branch completeness (staged) ───────────────────────────────

/// Does the branch-completeness lane ([`check_branch_completeness`]) run at
/// `phase`?
///
/// Staged exactly as the operand-type lane was, because the structural lane
/// runs unconditionally at `"pre-lower"` in release builds and a new
/// requirement there must first be read against real graphs:
///
/// * `CRATONVM_JIT_VERIFY_BRANCHES=1` forces it on at every phase;
/// * `CRATONVM_JIT_VERIFY_BRANCHES=0` forces it off everywhere;
/// * unset, it follows [`subchecks_follow_the_master_gate`] (debug builds) at
///   [`PHASE_POST_BUILD`] and [`PHASE_POST_OPTIMIZE`] only — the two hooks
///   whose producers (the builder, and `ir_optimize` with
///   `eliminate_dead_nodes` rooting control forward-reachable from `Start`)
///   are known to keep every arm.
///
/// Promote it into the structural lane once the difftest lanes have run clean
/// with it forced on. Read live (not latched) so a test can force either arm
/// through `with_thread_overrides`; the hooks that consult it run once per
/// compile.
pub fn branch_completeness_lane_enabled(phase: &str) -> bool {
    match env_flag("CRATONVM_JIT_VERIFY_BRANCHES") {
        Some(forced) => forced,
        None => {
            subchecks_follow_the_master_gate()
                && (phase == PHASE_POST_BUILD || phase == PHASE_POST_OPTIMIZE)
        }
    }
}

/// Every output of a live `Op::If` / `Op::Switch` has a live `Proj`, and that
/// `Proj` is consumed as control by some live node.
///
/// [`check_projections`] checks the converse (a `Proj` names an output that
/// exists, once). Without this half, a two-way branch with one edge going
/// nowhere verified clean, and what `ir_lower` emits for the missing edge is
/// unspecified — plausibly a fall-through into the other arm. Two producers
/// have been seen to make that shape: `eliminate_dead_nodes` deleting the arm
/// of an effect-free infinite loop, and the speculative branch prune losing
/// the edge into a merge in its cold arm (both fixed in round 9; this lane is
/// what turns the next such producer into a bailout).
fn check_branch_completeness(graph: &Graph, v: &mut Violations) {
    let n = graph.nodes.len();
    // has_ctrl_user[c]: some live node names `c` in one of its control slots.
    let mut has_ctrl_user = vec![false; n];
    // projs[p]: (index, proj id) for each live `Proj` of producer `p`.
    let mut projs: Vec<Vec<(usize, NodeId)>> = vec![Vec::new(); n];
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        for i in control_input_indices(node) {
            if let Some(&c) = node.inputs.get(i) {
                if let Some(slot) = has_ctrl_user.get_mut(c as usize) {
                    *slot = true;
                }
            }
        }
        if let Op::Proj(w) = node.op {
            if let Some(&p) = node.inputs.first() {
                if let Some(list) = projs.get_mut(p as usize) {
                    list.push((w as usize, idx as NodeId));
                }
            }
        }
    }
    for (idx, node) in graph.nodes.iter().enumerate() {
        if !matches!(node.op, Op::If | Op::Switch { .. }) {
            continue;
        }
        let id = idx as NodeId;
        let arity = projection_arity(&node.op);
        let present = &projs[idx];
        // An arity no list of `Proj(u16)`s can meet is reported once, not
        // once per missing index (a saturated `Switch` count would otherwise
        // spin for 2^32 iterations inside the verifier).
        if present.len() < arity {
            let missing = (0..arity.min(present.len() + 1))
                .find(|k| !present.iter().any(|&(w, _)| w == *k))
                .unwrap_or(present.len());
            v.add(format!(
                "{} has {} live projection(s) of {arity}; output {missing} has none (a \
                 branch arm with no successor)",
                label(graph, id),
                present.len()
            ));
            continue;
        }
        for &(w, proj) in present {
            if !has_ctrl_user[proj as usize] {
                v.add(format!(
                    "{} (output {w} of {}) is consumed as control by no live node (a \
                     branch arm that leads nowhere)",
                    label(graph, proj),
                    label(graph, id)
                ));
            }
        }
    }
}

/// Is `id` a node `ir_schedule` opens a basic block at?
///
/// Exactly Step 1 of `ir_schedule::schedule_with_options`: a `Merge`/`Region`,
/// `Proj(0)` of `Start`, `Proj(0)`/`Proj(1)` of an `If`, and any projection of
/// a `Switch`. [`produces_control`] is deliberately wider (it accepts the `If`
/// itself, `Start`, and a `Proj` of a join), which is right for "is this a
/// control token at all" and wrong for "can the scheduler map this edge to a
/// block" — the question [`check_control_anchors`] asks.
fn is_block_head(graph: &Graph, id: NodeId) -> bool {
    let Some(n) = node_at(graph, id) else {
        return false;
    };
    match n.op {
        Op::Merge | Op::Region => true,
        Op::Proj(w) => match n.inputs.first().and_then(|&p| node_at(graph, p)) {
            Some(p) => match p.op {
                Op::Start => w == 0,
                Op::If => w <= 1,
                Op::Switch { .. } => true,
                _ => false,
            },
            None => false,
        },
        _ => false,
    }
}

/// Every control edge the scheduler turns into a CFG edge or a placement
/// anchor names a block head ([`is_block_head`]), and a join's predecessors
/// are pairwise distinct (round 11 wave 2).
///
/// What the scheduler does with each shape this refuses — none of which any
/// other lane sees:
///
/// * a `Merge`/`Region` input that is not a block head (the `If` itself,
///   `Start`, a `Proj` of a join) is skipped when the CFG edges are built, so
///   the edge is silently dropped while the merge's φs keep an input for it,
///   and `ir_lower::emit_phi_copies` never runs that input's copy;
/// * a pinned node (`Load`, `Store`, `Call`, `Guard`, a terminator …) whose
///   control input is not a block head has no block for its anchor, so
///   `find_best_block` places it by its data inputs alone — possibly in block
///   0, above the branch that guarded it;
/// * two inputs of one join naming the same token are one runtime edge with
///   two φ slots, and `ir_lower::gather_phi_copies` emits two copies into one
///   φ home on it — the last one wins.
///
/// `IrBuilder` only ever threads `Proj(0)` of `Start`, a `Proj` of an
/// `If`/`Switch` or a `Merge` as control, and the optimizer passes that rewrite
/// control (`collapse_trivial_merges`, the unrollers, LICM's preheader
/// re-anchoring) substitute one such token for another, so this is a guard
/// against a future producer. Hand-built fixtures that anchor on `Start`
/// directly are why it is staged ([`control_structure_lane_enabled`], the
/// branch lane's switch) at the earlier hooks; at `"pre-lower"` it is
/// structural in every build since round 11 wave 6 (see [`verify_graph`];
/// `CRATONVM_JIT_VERIFY_BRANCHES=0` is its kill switch).
fn check_control_anchors(graph: &Graph, v: &mut Violations) {
    // One terminator per block: the scheduler's Step 2 keeps the HIGHER-id
    // terminator of a head two of them name and never places the other, so a
    // second `If`/`Switch`/`Return`/`Throw` on one token would be dropped from
    // the emitted code without a word.
    // Indexed by the head's id (every live id is in range) rather than a hash
    // map: since round 11 wave 6 this runs on every release compile.
    let mut terminator_of: Vec<NodeId> = vec![NO_NODE; graph.nodes.len()];
    // A join's predecessors seen so far, for the duplicate test (round 11
    // wave 9). It was `inputs.iter().take(k).any(..)` per input — O(k²) per
    // join, on every release compile, and a `tableswitch` join has up to
    // `ir::SWITCH_MAX_PROJECTIONS` predecessors. Reused across joins.
    let mut preds_seen: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        let id = idx as NodeId;
        if matches!(node.op, Op::If | Op::Switch { .. } | Op::Return | Op::Throw) {
            if let Some(&head) = node.inputs.first() {
                let slot = if is_live(graph, head) {
                    terminator_of.get_mut(head as usize)
                } else {
                    None
                };
                if let Some(slot) = slot {
                    let first = std::mem::replace(slot, id);
                    if first != NO_NODE {
                        v.add(format!(
                            "{} and {} both end the block {} heads: the scheduler keeps one \
                             terminator per block and silently drops the other",
                            label(graph, first),
                            label(graph, id),
                            label(graph, head)
                        ));
                    }
                }
            }
        }
        match node.op {
            // A `Proj`'s one input is its PRODUCER, not a block anchor —
            // `check_projections` owns it.
            Op::Dead | Op::Proj(_) => {}
            Op::Merge | Op::Region => {
                preds_seen.clear();
                for (k, &c) in node.inputs.iter().enumerate() {
                    // NO_NODE / removed / out of range: the edge lane reports.
                    if !is_live(graph, c) {
                        continue;
                    }
                    if !is_block_head(graph, c) {
                        v.add(format!(
                            "{} input[{k}] = {} is not a block head (Merge/Region, Proj(0) of \
                             Start, or a Proj of an If/Switch): the scheduler drops this CFG edge \
                             while the join's phis keep an input for it",
                            label(graph, id),
                            label(graph, c)
                        ));
                    }
                    // Only live inputs are recorded, which loses nothing: an
                    // earlier input equal to a live `c` is live itself.
                    if !preds_seen.insert(c) {
                        v.add(format!(
                            "{} names {} as a predecessor twice: one runtime edge, two phi \
                             slots, and only the last copy survives",
                            label(graph, id),
                            label(graph, c)
                        ));
                    }
                }
            }
            _ => {
                for i in control_input_indices(node) {
                    let Some(&c) = node.inputs.get(i) else {
                        continue;
                    };
                    if !is_live(graph, c) {
                        continue;
                    }
                    // A non-control target is `check_control`'s finding; say
                    // nothing twice.
                    if node_at(graph, c).is_some_and(|t| !produces_control(t)) {
                        continue;
                    }
                    if !is_block_head(graph, c) {
                        v.add(format!(
                            "{} is anchored at {}, which is not a block head: the scheduler has \
                             no block for that anchor and places the node by its data inputs \
                             alone",
                            label(graph, id),
                            label(graph, c)
                        ));
                    }
                }
            }
        }
    }
}

// ── Lane: control structure (staged) ─────────────────────────────────

/// Does the control-structure lane — [`check_control_anchors`] and
/// [`check_dominance`] — run at `phase`? (round 11 wave 3)
///
/// The same switch as [`branch_completeness_lane_enabled`]:
/// `CRATONVM_JIT_VERIFY_BRANCHES=1` forces it on at every phase and `=0` off
/// everywhere, and no new flag is declared. Unset, it follows
/// [`subchecks_follow_the_master_gate`] (debug builds) at
/// [`PHASE_POST_BUILD`], [`PHASE_POST_OPTIMIZE`] **and** `"pre-lower"`. The
/// last one is what separates it from the branch lane: `"pre-lower"` is the
/// only hook after `apply_ea_to_ir`, whose load forwarding is exactly the kind
/// of rewrite that can hand a use a definition from a block that does not
/// dominate it, and a debug run is how that hook gets read against real graphs
/// before either check can be promoted into the release structural lane. The
/// anchor check was, in round 11 wave 6 (see [`verify_graph`]); the dominance
/// check's release form is `Schedule::misplaced_data_nodes`.
fn control_structure_lane_enabled(phase: &str) -> bool {
    match env_flag("CRATONVM_JIT_VERIFY_BRANCHES") {
        Some(forced) => forced,
        None => {
            subchecks_follow_the_master_gate()
                && (phase == PHASE_POST_BUILD
                    || phase == PHASE_POST_OPTIMIZE
                    || phase == PHASE_PRE_LOWER)
        }
    }
}

/// Every use is dominated by its definition (round 11 wave 3).
///
/// SSA's one structural invariant, which until now nothing in the pipeline
/// checked: `ir_lower::verify_data_locations` compares EMISSION order (a
/// definition in a sibling arm that happens to be laid out first passes) and
/// skips every φ edge. Its release form is
/// `ir_schedule::Schedule::misplaced_data_nodes`, which `lib.rs` runs on every
/// finished schedule and which covers every half of this check except the
/// frame-state one. The rule is evaluated by
/// [`crate::ir_schedule::dominance_violations`] over the scheduler's own block
/// CFG and its own schedule-early placement, so "dominates" here means exactly
/// what it will mean to the scheduler: a data node whose inputs sit in
/// incomparable blocks, a terminator or a node pinned to a control anchor
/// (every op [`control_input_indices`] gives a control slot at input 0 —
/// since round 11 wave 10, not just the side-effecting ones) reading a value
/// its block is not dominated by, and a φ input that does not dominate the
/// predecessor its edge leaves. See that function for the precise statement
/// and for what it deliberately does not claim.
///
/// `frame_states` adds the snapshot half: a value a frame state names must
/// dominate every pinned node that deopts into that frame state — otherwise
/// the deopt rebuilds the slot from a home nothing has written yet.
fn check_dominance(graph: &Graph, frame_states: bool, v: &mut Violations) {
    use crate::ir_schedule::DominanceViolation as D;
    for finding in crate::ir_schedule::dominance_violations(graph, frame_states) {
        let msg = match finding {
            D::Input { user, slot, def } => format!(
                "{} input[{slot}] = {} is defined in a block that does not dominate the block {} \
                 is fixed in — a read on a path that never computed the value",
                label(graph, user),
                label(graph, def),
                label(graph, user)
            ),
            D::PhiEdge { phi, slot, def } => format!(
                "{} value input[{slot}] = {} does not dominate the predecessor its edge leaves \
                 (merge input {}) — the edge copy reads a value that path never computed",
                label(graph, phi),
                label(graph, def),
                slot.saturating_sub(1)
            ),
            D::Incomparable { user, a, b } => format!(
                "{} reads {} and {}, which are defined in blocks neither of which dominates the \
                 other — no block can compute {} (a merge of two arms needs a phi)",
                label(graph, user),
                label(graph, a),
                label(graph, b),
                label(graph, user)
            ),
            D::FrameState {
                site,
                snapshot,
                def,
            } => format!(
                "safepoint[{snapshot}] names {}, which does not dominate {} that deopts into it — \
                 the deopt frame would read a home nothing has written yet",
                label(graph, def),
                label(graph, site)
            ),
        };
        v.add(msg);
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

/// No DATA φ in the builder's output merges an undefined value (round 11
/// wave 8, irback proposal W7-3). Run at [`PHASE_POST_BUILD`] only, in debug
/// builds (see [`verify_graph`]).
///
/// `IrBuilder::retire_untypeable_phis` runs last in `build` and retires every
/// data φ with a `NO_NODE` value input that nothing but other φs and
/// snapshot locals/stack read, then every φ that merges a retired one on the
/// same terms. So a data φ that still has a `NO_NODE` input here is one a
/// real node reads, or one a snapshot names as a HELD MONITOR — which in
/// verified bytecode cannot happen: the verifier's merge of a value with an
/// unset slot is TOP, and nothing reads TOP. It is therefore either
/// unverifiable input or a builder bookkeeping error, and it is exactly the
/// shape a later `ir_optimize` trivial-φ fold (which skips the placeholder)
/// turns into a read of a value one path never computed — the
/// `r11w6-orch-post-schedule-phi-check-refuses-hashmap` graph. The
/// always-on edge lane accepts `NO_NODE` in a φ (see [`no_node_allowed`]),
/// and must: hand-built fixtures and later passes legitimately keep a dead
/// φ's placeholder until DCE. This is the builder-output half only.
///
/// Memory and control φs are not data φs and are left alone.
fn check_builder_phis_have_no_undefined_input(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op != Op::Phi || matches!(node.ty, IrType::Memory | IrType::Control) {
            continue;
        }
        if let Some(k) = node.inputs.iter().skip(1).position(|&inp| inp == NO_NODE) {
            v.add(format!(
                "{} value input[{}] is NO_NODE and the φ survived the builder's retirement of \
                 undefined-input φs — a node or a held monitor reads a slot one path never set",
                label(graph, idx as NodeId),
                k + 1
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
///
/// `terminatorless` — `exit == NO_NODE` and no `Return`/`Throw` anywhere — is
/// a complete method none of whose paths ends (`while (true) { … }` that
/// leaves only by an exception). The two "a terminator must exist" findings
/// are dropped for it and nothing else is (round 11 wave 2; it used to skip
/// every lane, see [`verify_graph`]).
fn check_control(graph: &Graph, terminatorless: bool, v: &mut Violations) {
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

    if !terminatorless && !is_live(graph, graph.exit) {
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
        } else if !has_terminator && !terminatorless {
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
///
/// The guard token is not one of them, so the count stops in front of it. The
/// live check never reaches that slot anyway — [`data_input_indices`] filters
/// it out and the homogeneity test only visits indices that survives that
/// filter — but a count that included the token would be wrong for the next
/// caller rather than merely unused.
fn homogeneous_operand_count(op: &Op, arity: usize) -> usize {
    let arity = match crate::ir::guard_token_slot_for(op, arity) {
        Some(slot) => slot,
        None => arity,
    };
    match op {
        Op::Shl | Op::Shr | Op::UShr => 1.min(arity),
        _ => arity,
    }
}

/// Indices of `node`'s inputs that carry a *data* value (as opposed to a
/// control, memory or guard token).
///
/// # The guard token, and why it is filtered here rather than per arm
///
/// `check_types` reports "a Control or Void token used as a data value" for
/// every index this returns, and it runs in EVERY build profile at
/// `PHASE_POST_OPTIMIZE`. `Op::Guard` is `IrType::Void`, and the arms below
/// classify slots `2..` of a `Load`/`ArrayLoad` and ALL slots of a
/// `Div`/`Rem` as data — so an unfiltered guard token would be reported on
/// every graph that has one. That is not a failing test: `ir_verify_reject`
/// answers `true`, the method silently falls back to the single-pass backend,
/// and the optimizing tier goes quiet wherever a guard exists.
///
/// The filter is applied to the result rather than written into each arm so
/// there is one place that has to know, and so it reads the same
/// `ir::guard_token_slot` table `expected_arity` and `ir::expected_input_type`
/// read.
fn data_input_indices(node: &Node) -> Vec<usize> {
    let mut idx = data_input_indices_unfiltered(node);
    if let Some(slot) = crate::ir::guard_token_slot(node) {
        idx.retain(|&i| i != slot);
    }
    idx
}

/// [`data_input_indices`] before the guard-token filter.
fn data_input_indices_unfiltered(node: &Node) -> Vec<usize> {
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
        // [ctrl, cond] — and `[ctrl, key]` for the multi-way branch, whose
        // slot 1 is a value read exactly the same way.
        Op::If | Op::Switch { .. } | Op::Guard { .. } => {
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
        // [ctrl, mem, value, index, coder(, guard)] (STRINTR21-5); the guard
        // slot is filtered by `data_input_indices`.
        Op::StrCharDecode => (2..node.inputs.len()).collect(),
        // [ctrl, mem, length] / [ctrl, mem, value] (`putstatic`)
        Op::NewArray { .. } | Op::StoreStatic { .. } => (2..node.inputs.len()).collect(),
        // `[ctrl, mem, array(, guard)]` (builder) or `[array]` (fixtures) —
        // the same arity split `control_input_indices` makes. The array is the
        // one value either form reads; it used to be in no lane, so a control
        // or void token could reach `ir_lower`'s length load as the base.
        Op::ArrayLength => match node.inputs.len() {
            1 => vec![0],
            n if n >= 3 => vec![2],
            _ => Vec::new(),
        },
        // Pure arithmetic with no control or memory edge at all: EVERY input is
        // a value. Added 2026-09-17 — `ScalarIntrinsic` used to fall into the
        // catch-all below, so it had no data inputs at all from this lane's
        // point of view and a control or void token flowing into
        // `Long.rotateLeft`'s distance operand was invisible.
        //
        // This does NOT yet enforce `ScalarOp::input_ty`, which is documented
        // in `ir.rs` as "the single source of truth" for each family's operand
        // types and is what the lowerer selects its machine width from; see
        // `NOTES-irfront.md` for the shared expected-operand-type table that
        // would close the rest of the gap for `Cmp`/`LCmp`/`FCmp`, `MemKind`
        // value types and array bases/indices as well.
        Op::ScalarIntrinsic(_) => (0..node.inputs.len()).collect(),
        // [obj]: the tested reference is the one input, and a value.
        Op::ExactClassIs { .. } => (0..node.inputs.len()).collect(),
        // [obj]: likewise (JIT round 19, lane osr19).
        Op::ClassWordOf => (0..node.inputs.len()).collect(),
        // [ctrl] and nothing else: no value is read (round 11 wave 14).
        Op::OsrEntryFlag { .. } | Op::OsrLocal(_) | Op::OsrMemory => Vec::new(),
        // [ctrl] and nothing else (JIT round 19 wave 3, lane eager3).
        Op::ClassInitTest { .. } => Vec::new(),
        // [ctrl] and nothing else (nio21 wave 19, lane L3).
        Op::CurrentThreadMirror { .. } | Op::VthreadBoundaryTest { .. } => Vec::new(),
        // [ctrl] and nothing else (JIT round 20 wave 11, lane pIndy).
        Op::IndyRootRef { .. } => Vec::new(),
        // [ctrl] and nothing else (JIT round 19 wave 5, lane handler5).
        Op::CatchEntryFlag { .. } | Op::CaughtException => Vec::new(),
        // [ctrl, ref]: the referent is the one value (nio20 L2).
        Op::KeepAlive => (1..node.inputs.len()).collect(),
        // [ctrl, mem, exc] / [ctrl, mem, obj]. The exception a `Throw` carries
        // and the object a monitor locks are values, and were in no lane at
        // all: nothing required them to be anything, so a control token could
        // reach `helpers.throw_exception` as an oop.
        Op::Throw | Op::MonitorEnter | Op::MonitorExit => (2..node.inputs.len()).collect(),
        // [ctrl, mem, obj(, delta)] / [ctrl, mem, args…] / [ctrl, mem, lambda,
        // index]. Added 2026-09-18 for the same reason as the three above: the
        // operand a type check tests, the receiver an unbox reads, and every
        // argument a call marshals are values, and a control or void token
        // reaching one was in no lane. Only the token rule applies to them —
        // `is_homogeneous_arith` names none of these ops — and a call's
        // argument TYPES stay undescribed, as `ir::expected_input_type` says.
        Op::InstanceOf { .. }
        | Op::CheckCast { .. }
        | Op::Unbox { .. }
        | Op::Call { .. }
        | Op::LambdaIntToDouble
        | Op::IndyBridge { .. }
        | Op::MultiNewArray { .. } => (2..node.inputs.len()).collect(),
        // [ctrl] or [ctrl, value]: the returned value.
        Op::Return => {
            if node.inputs.len() > 1 {
                vec![1]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),    }}

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
            // `Memory` too (round 11): a store, a monitor transition, a memory
            // φ or `Start`'s memory projection has no value, and
            // `ir_lower::verify_data_locations` skips an input whose type is
            // not a value type rather than refusing it — so this lane is the
            // only place a store read as an operand is caught. Token slots,
            // φ inputs and guard slots never reach here (`data_input_indices`
            // and the φ arm above), which is what keeps the builder's
            // value-node-as-token reuse (`self.mem = load`) out of this rule:
            // that is the opposite direction.
            if matches!(ty, IrType::Control | IrType::Void | IrType::Memory) {
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

// ── Lane: expected operand types (staged) ────────────────────────────

/// Is the shared expected-operand-type table ENFORCED?
///
/// `CRATONVM_JIT_VERIFY_TYPES=1`, explicitly, in any build profile and at any
/// phase — and nothing else. Note this is a narrower reading of that variable
/// than [`VerifyOptions::from_env`] gives it: there it is a tri-state with a
/// build-profile default, and at [`PHASE_POST_OPTIMIZE`] the type lane is on
/// unconditionally. That difference is the staging, and it is deliberate.
///
/// # Why this lane is not simply part of `check_types`
///
/// `check_types` runs in every build, release included, at
/// [`PHASE_POST_OPTIMIZE`]. Folding a broad new *operand* requirement into it
/// would enforce, fleet-wide and in the same change that wrote it, a table
/// that has never been read against real graphs. The failure mode of getting
/// that wrong is not a crash and not a failing test: `ir_verify_reject`
/// answers `true`, the method falls back to the single-pass backend, and a
/// **modelling** gap — a shape the front end legitimately emits that the table
/// does not describe — becomes a silent throughput regression.
///
/// `ir.rs`'s table names two shapes it knowingly cannot describe (a call's
/// arguments, a φ's inputs) and answers `TypeReq::Any` for both. The ones it
/// does not know it cannot describe are what this flag is for: turn it on,
/// read the difftest lane, then delete this function and let `check_types`
/// call `check_operand_types` unconditionally.
///
/// Latched on first read (unlike [`verify_enabled`] since j27): the builder's debug
/// assertion consults it once per node created, and a `getenv` per IR node is
/// not a cost a debug build should pay to keep a flag re-readable.
pub fn operand_type_lane_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| env_flag("CRATONVM_JIT_VERIFY_TYPES") == Some(true))
}

/// Kill switch for [`operand_type_lane_staged_at`]: flip to `false` to restore
/// the pre-wave-18 staging (the operand-type table enforced only on an explicit
/// `CRATONVM_JIT_VERIFY_TYPES=1`).
pub const OPERAND_TYPE_LANE_STAGED_IN_DEBUG: bool = true;

/// Whether the operand-type table runs at `phase` by the build-profile default
/// (round 11 wave 18, item 2 of `r11-irback-release-builds-verify-structure-only`).
///
/// The staging step [`operand_type_lane_enabled`]'s doc asks for ("turn it on,
/// read the difftest lane, then promote"), taken the way every other new lane
/// in this module was taken: at [`PHASE_POST_OPTIMIZE`] only (the one hook
/// whose graph `ir_optimize` has finished and nothing after it has touched),
/// in DEBUG builds only ([`subchecks_follow_the_master_gate`]), so every
/// `cargo test` run of the JIT and VM suites is the read the page waited
/// sixteen waves for. Since round 12 wave 1 a release build runs it there too
/// ([`operand_type_lane_released`]; kill switch
/// `CRATONVM_JIT_VERIFY_OPERAND_TYPES_RELEASE=0`). `CRATONVM_JIT_VERIFY_TYPES=0`
/// turns it off in every profile; [`OPERAND_TYPE_LANE_STAGED_IN_DEBUG`] is the
/// compile-time switch.
///
/// Deliberately NOT folded into [`operand_type_lane_enabled`]: that predicate
/// also arms the builder's per-node debug assertion (`ir.rs`), which would turn
/// a table gap into a debug panic in the builder rather than a verifier
/// finding attributable to a phase. A finding here is the ordinary verifier
/// failure (the method falls back to the single-pass tier, counted in
/// `bailout::bailout_counts`, printed under the IR stage report), which a test
/// that asserts the IR tier compiled a method will surface.
fn operand_type_lane_staged_at(phase: &str) -> bool {
    OPERAND_TYPE_LANE_STAGED_IN_DEBUG
        && phase == PHASE_POST_OPTIMIZE
        && (subchecks_follow_the_master_gate() || operand_type_lane_released())
        && env_flag("CRATONVM_JIT_VERIFY_TYPES") != Some(false)
}

/// Whether the operand-type table's `"post-optimize"` staging also holds in a
/// RELEASE build (round 12 wave 1, the promotion step of
/// `r11-irback-release-builds-verify-structure-only` item 2).
///
/// Cost: one pass over the arena, one `ir::required_input_type` table lookup
/// per input slot, no allocation — the same order as `check_types`, which
/// already runs there in release, and small next to `ir_optimize::optimize`
/// (GVN, LICM, unroll) that the same hook follows. A finding is the ordinary
/// verifier fall-back to the single-pass tier, so a table gap costs throughput
/// on the methods it names, never a wrong answer; a mixed-width `Cmp` it
/// catches (which `ir_lower` would lower as a 64-bit compare of a 32-bit
/// operand) is a wrong answer it prevents.
///
/// Default on; `CRATONVM_JIT_VERIFY_OPERAND_TYPES_RELEASE=0` restores the
/// debug-only staging (and `CRATONVM_JIT_VERIFY_TYPES=0` still turns the table
/// off in every profile). Read per compile, no static.
fn operand_type_lane_released() -> bool {
    true}

/// Every input slot must hold the type `ir::expected_input_type` requires.
///
/// The lane `NOTES-irfront.md` §4 asks for, and the half of the type model
/// `check_types` could not express. `check_types` reports a control or void
/// token used as a value, and reports a homogeneous arithmetic node whose
/// operand disagrees with its result; it says nothing about *which* type an
/// operand should be, so `Cmp(Lt, int_node, long_node)` verified clean while
/// `ir_lower` selected a 64-bit signed `CMP` for it, and `ScalarOp::MinL` fed
/// two `Int` nodes did the same.
///
/// Reads the one table in `ir.rs` so that the lowerer, the builder's debug
/// assertion and this lane cannot hold three different opinions.
fn check_operand_types(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let id = idx as NodeId;
        // The φ lattice is `check_types`' business, not this table's.
        if node.op == Op::Phi {
            continue;
        }
        let ty_of = |n: NodeId| {
            node_at(graph, n)
                .filter(|src| src.op != Op::Dead && src.ty != IrType::Void)
                .map(|src| src.ty)
        };
        for slot in 0..node.inputs.len() {
            let Some(want) =
                crate::ir::required_input_type(&node.op, node.ty, &node.inputs, slot, &ty_of)
            else {
                continue;
            };
            let Some(&inp) = node.inputs.get(slot) else {
                continue;
            };
            // A slot naming nothing, a removed node or an untyped one is not
            // this lane's finding — `check_edges` reports the first two and
            // the φ fallback owns the third.
            let Some(have) = ty_of(inp) else {
                continue;
            };
            if have != want {
                v.add(format!(
                    "{} input[{slot}] = {} is {} where the operand table requires {} \
                     (ir::expected_input_type)",
                    label(graph, id),
                    label(graph, inp),
                    type_name(have),
                    type_name(want)
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
/// Three things are checked: that snapshot bcis are **unique** among the
/// snapshots nothing names (both consumers key on the bci and break ties by
/// position, so a duplicate silently discards one frame state; a claimed copy
/// is exempt, see the body), that slot counts are `u16`-plausible, and that every named
/// node is in range and live. See `deopt-metadata-audit.md` §5.
///
/// `scalar_replaced` (round 11 wave 18): the allocations escape analysis
/// replaced, when the caller knows them. A slot naming one of those REMOVED
/// nodes is the virtual-object descriptor, not a stranded value, and is
/// accepted; `None` accepts no removed node (the pre-EA hooks).
fn check_frame_states(
    graph: &Graph,
    scalar_replaced: Option<&std::collections::HashSet<NodeId>>,
    v: &mut Violations,
) {
    // Snapshot bcis must be unique, because both consumers key on the bci and
    // resolve ties by *position*:
    //
    //   * `ir_lower::resolve_frame_state_for_bci` does
    //     `safepoints.iter().position(|s| s.bci == bci)` — first match wins, so
    //     a duplicate silently hands the resume the OTHER snapshot's frame;
    //   * `ir_lower::build_deopt_points` maps every snapshot through
    //     `bci_native[sp.bci]`, so duplicates collide on one native offset and
    //     `dedup_by_key` drops all but the first.
    //
    // Either way one of the two frame states is silently discarded and the
    // other is used at a program point it does not describe — a *plausible*
    // deopt frame rather than the correct one, which is the failure mode with
    // no visible symptom. The front end's linear bytecode walk visits each `pc`
    // once, so this holds for anything it builds; the check is what keeps it
    // holding.
    //
    // ## Unless every duplicate is CLAIMED
    //
    // A pass that copies a region — `ir_optimize::unroll` — makes several
    // program points out of one bci on purpose, and each copy needs a frame of
    // its own. Both consumers above have a per-copy path that does not go
    // through the bci: `resolve_frame_state_for_site` prefers the snapshot the
    // trapping node NAMES (`Node::frame_snapshot`), and `build_deopt_points`
    // anchors a named snapshot at an offset inside its own copy
    // (`snapshot_native`). So a duplicate bci is safe exactly when every
    // snapshot at it is named by some node — at which point no consumer is
    // resolving it by position, and there is nothing to discard.
    //
    // An UNCLAIMED duplicate is still the original bug, and is still reported.
    // That is the case this relaxation must not swallow: it is what a
    // half-finished copy looks like, where one iteration got a snapshot and its
    // nodes were never stamped with it.
    //
    // ## One unclaimed snapshot is fine if it comes FIRST (round 11 wave 6)
    //
    // LICM's corrected pre-header frame (`ir_optimize::
    // install_preheader_deopt_frame`) pushes a COPY at the hoisted node's bci,
    // named by that node, and leaves the original — named by nobody — for the
    // other nodes at that bci. Those resolve by position, and the first
    // snapshot at a bci is the one they find (`ir_lower`'s
    // `safepoint_first_by_bci`), so the original is exactly what they need as
    // long as it precedes every copy; a copy is appended after it, so it does.
    // Two unclaimed snapshots at one bci, or an unclaimed one behind a claimed
    // one, are still ambiguous and still reported. The by-OFFSET half of that
    // story is `ir_lower`'s: a node that names its own snapshot must not move
    // `bci_native` (page `r11w5-mid-named-frame-copies-collide-with-the-by-bci-snapshot`).
    let mut claimed: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut node_bcis: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for node in &graph.nodes {
        if node.op == Op::Dead {
            continue;
        }
        if let Some(si) = node.frame_snapshot {
            claimed.insert(si);
        }
        if let Some(pc) = node.bytecode_pc {
            node_bcis.insert(pc);
        }
    }
    let mut at_bci: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for (si, sp) in graph.safepoints.iter().enumerate() {
        // A post-call chain snapshot (interpreter round i1 wave 47, lane L3) is
        // found by the call it names, never by its pc.
        if sp.post_call_only {
            continue;
        }
        at_bci.entry(sp.bci).or_default().push(si);
    }
    let mut duplicated: Vec<(usize, Vec<usize>)> = at_bci
        .into_iter()
        .filter(|(_, sis)| sis.len() > 1)
        .collect();
    duplicated.sort_unstable();
    for (bci, sis) in duplicated {
        // A bci NO live node carries emits no code, so nothing can resume
        // there: `build_deopt_points` anchors through `bci_native`, which is
        // populated only from nodes, and every guard resumes at some node's own
        // `bytecode_pc`. Duplicates at such a bci are unconsultable rather than
        // ambiguous -- and they are normal after an unroll, because the post-
        // unroll constant fold retires the very nodes that made those bcis
        // real. Only a duplicated bci the code actually EMITS is the bug this
        // check is for. See `ir_optimize::plan_copy_frames`, which draws the
        // same line on the producing side.
        if !node_bcis.contains(&bci) {
            continue;
        }
        let unclaimed: Vec<usize> = sis
            .iter()
            .copied()
            .filter(|si| !claimed.contains(&(*si as u32)))
            .collect();
        // `sis` is ascending: it was filled in snapshot order.
        let only_the_first_is_unclaimed = unclaimed.len() == 1 && unclaimed.first() == sis.first();
        if !unclaimed.is_empty() && !only_the_first_is_unclaimed {
            v.add(format!(
                "safepoint(s) {unclaimed:?} of {sis:?} describe bci {bci} without being named by \
                 any node's `frame_snapshot` — the bci-keyed consumers take the first match, so \
                 one of these frame states is silently discarded and another resumes a program \
                 point it does not describe"
            ));
        }
    }

    // A named snapshot must exist and must agree with the node about which
    // bytecode index the frame resumes at. `Graph::set_node_frame_snapshot`
    // refuses to install anything else, so a violation here means a snapshot
    // index was written past it or the snapshot list was rewritten underneath
    // one — either way the node would deopt into the wrong instruction with a
    // frame that looks entirely plausible. The bci it must agree with is the
    // node's pc, or the bci that pc resumes at out of a spliced body
    // (`Graph::set_node_frame_snapshot`, round 11 wave 5).
    let splices = crate::ir::splice_resume_map_this_build();
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        let Some(si) = node.frame_snapshot else {
            continue;
        };
        match graph.safepoints.get(si as usize) {
            None => v.add(format!(
                "node {id} names safepoint[{si}], which does not exist ({} snapshots)",
                graph.safepoints.len()
            )),
            Some(sp)
                if node.bytecode_pc != Some(sp.bci)
                    && node
                        .bytecode_pc
                        .and_then(|pc| splices.as_ref().and_then(|m| m.resume_bci(pc)))
                        != Some(sp.bci) =>
            {
                v.add(format!(
                    "node {id} at bci {:?} names safepoint[{si}], which resumes at bci {}",
                    node.bytecode_pc, sp.bci
                ));
            }
            Some(_) => {}
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
            .chain(sp.stack.iter().enumerate().map(|(i, id)| ("stack", i, *id)))
            .chain(
                sp.monitors
                    .iter()
                    .enumerate()
                    .map(|(i, id)| ("monitor", i, *id)),
            );
        for (kind, i, id) in slots {
            // `NO_NODE` means "undefined at this bci", which is legal for a
            // local or a stack slot — but not for a HELD MONITOR. A deopt
            // resumes the interpreter still owning every lock the snapshot
            // lists; an undefined entry is a lock the frame holds and cannot
            // name, so the interpreter's later `monitorexit` finds nothing to
            // release. The builder never records one (the `monitorenter` arm
            // refuses an undefined operand), so one appearing here means a
            // pass normalised a live lock's object away.
            if id == NO_NODE {
                if kind == "monitor" {
                    v.add(format!(
                        "safepoint[{si}] at bci {} monitor[{i}] is NO_NODE — a held lock the \
                         deopt frame cannot name",
                        sp.bci
                    ));
                }
                continue;
            }
            match graph.nodes.get(id as usize) {
                None => v.add(format!(
                    "safepoint[{si}] at bci {} {kind}[{i}] = n{id} is out of range",
                    sp.bci
                )),
                // A scalar-replaced allocation: the slot is its descriptor
                // (`APPLY_EA_ROUTES_ALL_SAFEPOINTS`). Before the `Dead` arm,
                // and before the token arm (`Graph::kill` types a removed
                // node `Void`).
                Some(n) if n.op == Op::Dead && scalar_replaced.is_some_and(|s| s.contains(&id)) => {
                }
                Some(n) if n.op == Op::Dead => v.add(format!(
                    "safepoint[{si}] at bci {} {kind}[{i}] = n{id} refers to a removed (Dead) \
                     node — the deopt frame would rebuild a slot from nothing",
                    sp.bci
                )),
                // A frame slot holds a VALUE. A control, memory or void token
                // there has no representation in an interpreter frame: the
                // deopt materialiser would read whatever machine word the
                // token's node left in its home (a store's, a guard's) and
                // hand it to the interpreter as a local. Added 2026-09-18.
                Some(n) if matches!(n.ty, IrType::Control | IrType::Memory | IrType::Void) => v
                    .add(format!(
                        "safepoint[{si}] at bci {} {kind}[{i}] = {} is a {} token, not a value",
                        sp.bci,
                        label(graph, id),
                        type_name(n.ty)
                    )),
                Some(_) => {}
            }
        }
    }

    check_deopt_coverage(graph, v);

    // The snapshot half of the def-use lists (round 11 wave 2). `Graph::
    // safepoints` is a public field, so a slot can be INSTALLED without
    // `sp_users` hearing of it, and a later tracked `replace_all_uses` then
    // misses that slot — a deopt reading a dead node's home. `verify_use_lists`
    // is the check for exactly that drift and had no production caller; it
    // answers `Ok` whenever the lists are not current (they make no claim).
    if let Err(why) = graph.verify_use_lists() {
        v.add(format!(
            "def-use lists disagree with the graph ({why}) — a tracked replace_all_uses would \
             miss that edge"
        ));
    }
}

/// Every node that can transfer to the interpreter has a frame state at the bci
/// it resumes at (round 11 wave 2).
///
/// `ir_lower::resolve_frame_state_for_bci` answers a bci with no snapshot with
/// an EMPTY frame (and, since round 11 wave 2, latches a bailout for it on a
/// graph that has snapshots at all); this is the same rule stated over the
/// graph, so the pass that dropped the snapshot is named instead of the
/// lowerer. The resume rule is `ir_lower`'s: a node naming its own snapshot
/// (`Node::frame_snapshot`, a per-copy unroll frame) is covered by it; any
/// other node resumes at its `bytecode_pc`, translated out of a spliced body
/// through the build's [`crate::ir::IrSpliceResumeMap`] when one was
/// published on this thread.
///
/// Silent where the question cannot be answered: a graph with no snapshots at
/// all (a hand-built fixture — `ir_lower` keeps its historical empty frame for
/// those), a node with no pc (`ir_lower::deopt_pc_of` owns that refusal), a pc
/// the splice map cannot attribute, and — with no map in hand — a pc past every
/// snapshot, which is where a spliced body's combined-buffer pcs live.
fn check_deopt_coverage(graph: &Graph, v: &mut Violations) {
    if graph.safepoints.is_empty() {
        return;
    }
    let bcis: std::collections::HashSet<usize> = graph.safepoints.iter().map(|s| s.bci).collect();
    let max_bci = graph.safepoints.iter().map(|s| s.bci).max().unwrap_or(0);
    let splices = crate::ir::splice_resume_map_this_build();
    for (idx, node) in graph.nodes.iter().enumerate() {
        if crate::ir_lower::op_cannot_deopt(&node.op) {
            continue;
        }
        // An `Op::Guard` deopts at its own `bci` field (`ir_lower`'s guard
        // arm), every other node at its `bytecode_pc` (`deopt_pc_of`).
        let deopt_pc = match node.op {
            Op::Guard { bci } => Some(bci),
            _ => node.bytecode_pc,
        };
        let Some(pc) = deopt_pc else {
            continue;
        };
        let resume = match &splices {
            Some(map) => match map.resume_bci(pc) {
                Some(r) => r,
                None => continue,
            },
            None if pc > max_bci => continue,
            None => pc,
        };
        // `resolve_frame_state_for_site`: the node's own snapshot wins when it
        // describes the RESUME bci.
        if let Some(si) = node.frame_snapshot {
            if graph
                .safepoints
                .get(si as usize)
                .is_some_and(|s| s.bci == resume)
            {
                continue;
            }
        }
        if !bcis.contains(&resume) {
            v.add(format!(
                "{} can deopt at bci {pc} (resumes at {resume}) but no safepoint snapshot \
                 describes bci {resume} — ir_lower would resume the interpreter with an empty \
                 frame",
                label(graph, idx as NodeId)
            ));
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
///
/// # The producer rule (round 11 wave 2)
///
/// A token slot naming a LIVE node was accepted whatever that node was, and
/// `TypeReq::Memory` resolves to no type requirement, so a token naming a
/// `Const`, a `Param`, an arithmetic node or a control `Proj` verified clean.
/// That is exactly what an unspliced `replace_all_uses(load, value)` leaves when
/// the load was also somebody's token (the builder's `self.mem = load`), and a
/// token that names a pure value orders nothing: the later store it was meant
/// to keep behind the read is free to move above it. So a token's producer
/// must now be something that IS a memory state: `Proj(1)` of `Start` (the
/// entry token), a `Memory`-typed φ, or an op with a memory shape
/// (`Op::memory_shape`), which covers every value-node-as-token the builder
/// uses (loads, calls, allocations, `Unbox`, the String expander's reads, the
/// `ldc` helpers). The same holds for every input of a `Memory`-typed φ.
fn check_memory_chain(graph: &Graph, v: &mut Violations) {
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.op == Op::Dead {
            continue;
        }
        for (i, &inp) in node.inputs.iter().enumerate() {
            if inp == NO_NODE || inp as usize >= graph.nodes.len() {
                continue;
            }
            if !is_memory_token_input(node, i) {
                continue;
            }
            if !is_live(graph, inp) {
                v.add(format!(
                    "{} takes its memory token from removed node n{inp} — the ordering edge to \
                     everything that wrote before it is gone",
                    label(graph, idx as NodeId)
                ));
                continue;
            }
            if !is_memory_state(graph, inp) {
                v.add(format!(
                    "{} input[{i}] takes its memory token from {}, which is not a memory state \
                     (Proj(1) of Start, a Memory phi, or an op with a memory shape) — the edge \
                     orders nothing",
                    label(graph, idx as NodeId),
                    label(graph, inp)
                ));
            }
        }
    }
}

/// Can `id` stand for a memory state in a token slot? See the producer rule on
/// [`check_memory_chain`]. `id` is known live.
fn is_memory_state(graph: &Graph, id: NodeId) -> bool {
    let Some(n) = node_at(graph, id) else {
        return false;
    };
    match n.op {
        Op::Phi => n.ty == IrType::Memory,
        // The interpreter's memory on OSR entry (round 11 wave 14): a memory
        // state with no incoming token, so no `memory_shape` either.
        Op::OsrMemory => n.ty == IrType::Memory,
        Op::Proj(1) => {
            n.ty == IrType::Memory
                && n.inputs
                    .first()
                    .and_then(|&p| node_at(graph, p))
                    .is_some_and(|p| p.op == Op::Start)
        }
        ref op => op.memory_shape().is_some(),
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
mod tests;

/// JIT round 21 lane stress (FUZZ21-2): the between-passes verifier.
#[cfg(test)]
mod r21_stress_between_passes_tests;
