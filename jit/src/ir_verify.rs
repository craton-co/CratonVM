// SPDX-License-Identifier: Apache-2.0
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
//!    compile at `"post-optimize"` in **every** build rather than only in a
//!    debug one, and still bails at `"pre-lower"` whenever the environment
//!    leaves the lane on there (that hook runs *unconditionally*, gating on
//!    [`pre_lower_verify_disabled`] rather than on [`verify_enabled`]).
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
//!    rather than forgotten — not a reason to weaken the lane.
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
            // It is every profile *in which the hook runs*, and that is a
            // narrower set than the paragraphs above suggest: `lib.rs` calls
            // the `PHASE_POST_OPTIMIZE` hook (and `"post-escape-analysis"`)
            // only under [`verify_enabled`], which is `cfg!(debug_assertions)`
            // unless `CRATONVM_JIT_VERIFY_IR=1`. In a default RELEASE build the
            // only hook that runs is `"pre-lower"`, where this lane follows
            // `env.check_types` — `false` there. So a shipped build type-checks
            // nothing unless an operator asks; the claim the module header
            // makes about φ conflicts holds for debug builds, the test suites
            // and the difftest lanes. That is also why the builder now declines
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
/// Latched on first read so the per-pass hook costs one atomic load.
pub fn verify_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| env_flag("CRATONVM_JIT_VERIFY_IR").unwrap_or(cfg!(debug_assertions)))
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
    check_guard_tokens(graph, &mut v);
    check_projections(graph, &mut v);
    check_phis(graph, &mut v);
    check_control(graph, &mut v);
    // Staged, and keyed on the PHASE rather than on `opts`: see
    // `branch_completeness_lane_enabled`.
    if branch_completeness_lane_enabled(phase) {
        check_branch_completeness(graph, &mut v);
    }

    if opts.check_types {
        check_types(graph, &mut v);
        // Staged: the shared operand-type table is enforced only on an
        // explicit `CRATONVM_JIT_VERIFY_TYPES=1`, never by the build profile
        // and never by a phase declaration. See `operand_type_lane_enabled`.
        if operand_type_lane_enabled() {
            check_operand_types(graph, &mut v);
        }
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
        Op::Start | Op::If | Op::Switch { .. } | Op::Merge | Op::Region => true,
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
        | Op::Switch { .. }
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
        | Op::LambdaIntToDouble
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
        | Op::Unbox { .. } => {
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
        // cov-05 — [ctrl, mem, obj], same shape as `MonitorEnter` above.
        Op::InstanceOf { .. } | Op::CheckCast { .. } => (3, 3),
        // [ctrl, mem, obj] — same shape again. It takes `ctrl` because it can
        // DEOPT (null receiver, or a subclass that overrode the accessor) and
        // `mem` because it reads the instance.
        // [ctrl, mem, receiver] plus a delta for the one family that takes
        // one. `UnboxOp::arity` is the single source of truth, so a family
        // added there cannot fall out of step with this.
        Op::Unbox { op, .. } => {
            let n = 3 + op.arity();
            (n, n)
        }
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
    let mut seen: Vec<(NodeId, usize)> = Vec::new();
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
        if seen.contains(&(producer_id, which)) {
            v.add(format!(
                "{} is a second Proj({which}) of {} (a projection names one output, and \
                 ir_lower picks an edge by matching the index)",
                label(graph, id),
                label(graph, producer_id)
            ));
            continue;
        }
        seen.push((producer_id, which));
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
        // [ctrl, mem, length]
        Op::NewArray { .. } => (2..node.inputs.len()).collect(),
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
        | Op::LambdaIntToDouble => (2..node.inputs.len()).collect(),
        // [ctrl] or [ctrl, value]: the returned value.
        Op::Return => {
            if node.inputs.len() > 1 {
                vec![1]
            } else {
                Vec::new()
            }
        }
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
/// Latched on first read, like [`verify_enabled`]: the builder's debug
/// assertion consults it once per node created, and a `getenv` per IR node is
/// not a cost a debug build should pay to keep a flag re-readable.
pub fn operand_type_lane_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| env_flag("CRATONVM_JIT_VERIFY_TYPES") == Some(true))
}

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
/// Three things are checked: that snapshot bcis are **unique** (both consumers
/// key on the bci and break ties by position, so a duplicate silently discards
/// one frame state), that slot counts are `u16`-plausible, and that every named
/// node is in range and live. See `deopt-metadata-audit.md` §5.
fn check_frame_states(graph: &Graph, v: &mut Violations) {
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
        if !unclaimed.is_empty() {
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
    // frame that looks entirely plausible.
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
            Some(sp) if node.bytecode_pc != Some(sp.bci) => v.add(format!(
                "node {id} at bci {:?} names safepoint[{si}], which resumes at bci {}",
                node.bytecode_pc, sp.bci
            )),
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

    /// A control join with no predecessors is the orphan `ir.rs`'s own pre-scan
    /// comment calls fatal ("an input-less control node is exactly the kind of
    /// orphan the scheduler/lowerer is not prepared for"). It used to verify.
    #[test]
    fn an_input_less_merge_is_rejected() {
        let mut g = diamond_graph();
        let merge = g.nodes[g.exit as usize].inputs[0];
        g.nodes[merge as usize].inputs.clear();
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("at least 1"), "{m}");
    }

    /// A `Region` is a LOOP HEADER: entry edge plus back edge. One predecessor
    /// means an edge was dropped — which is precisely what
    /// `IrBuilder::patch_loop_backedge`'s silent `None => return` produced, and
    /// why that shape was invisible to this module until 2026-09-17.
    #[test]
    fn a_loop_region_with_a_single_predecessor_is_rejected() {
        let mut g = diamond_graph();
        let merge = g.nodes[g.exit as usize].inputs[0];
        g.nodes[merge as usize].op = Op::Region;
        g.nodes[merge as usize].inputs.pop();
        // The φ is trimmed to match, so `check_phis` stays quiet and the arity
        // lane is demonstrably the one that speaks.
        let phi = g.nodes[g.exit as usize].inputs[1];
        g.nodes[phi as usize].inputs.pop();
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("at least 2"), "{m}");
        assert!(
            !m.contains("value input(s)"),
            "the phi lane must be quiet — this is an arity finding: {m}"
        );
    }

    /// A one-predecessor `Merge` stays legal: branch folding collapses a
    /// two-way join into a pass-through before anything re-canonicalises it.
    #[test]
    fn a_merge_that_collapsed_to_one_predecessor_still_verifies() {
        let mut g = diamond_graph();
        let merge = g.nodes[g.exit as usize].inputs[0];
        g.nodes[merge as usize].inputs.pop();
        let phi = g.nodes[g.exit as usize].inputs[1];
        g.nodes[phi as usize].inputs.pop();
        let r = verify_graph(&g, "test", VerifyOptions::all());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    /// `ir_lower` selects an `If`'s taken edge by *matching* `Op::Proj(0)`
    /// among its users, so an index that names no output leaves a control edge
    /// unlowered.
    #[test]
    fn a_projection_index_must_name_an_output_its_producer_has() {
        let mut g = diamond_graph();
        let t = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Proj(0) && n.ty == IrType::Control)
            .expect("a control projection");
        // Re-point it at the If it already projects, with an index that If
        // does not have.
        g.nodes[t].op = Op::Proj(7);
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("selects output 7"), "{m}");
    }

    /// …and two `Proj`s naming the same output make "the taken edge" whichever
    /// one the scan reaches first.
    #[test]
    fn two_projections_may_not_name_the_same_output() {
        let mut g = diamond_graph();
        let f = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Proj(1) && n.ty == IrType::Control)
            .expect("the fall-through projection");
        g.nodes[f].op = Op::Proj(0);
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("second Proj(0)"), "{m}");
    }

    // ── The guard token edge ─────────────────────

    /// The `Start` preamble a guarded graph needs, with nothing appended yet.
    /// Returns `(graph, start, ctrl, mem)`. The `Op::Return` is added by each
    /// test LAST, so arena order holds and `VerifyOptions::all()` — which
    /// includes the arena-order lane — is what these tests can use.
    fn guarded_head() -> (Graph, NodeId, NodeId, NodeId) {
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
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        g.entry = start;
        (g, start, ctrl, mem)
    }

    /// Close a [`guarded_head`] graph off with `Return(value)`.
    fn finish(g: &mut Graph, ctrl: NodeId, value: NodeId) {
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, value], None);
        g.exit = ret;
    }

    /// `expected_arity` admits the guarded shape of every op `ir::guard_shape`
    /// names, and admits exactly one extra input — never two.
    ///
    /// What this catches: an op added to `ir::Op::guard_shape` without the
    /// matching arity widening here (the guarded node is then rejected by the
    /// arity lane, which in production is a silent fall-back to the
    /// single-pass tier), and the reverse — an arity loosened past what the
    /// token slot explains.
    #[test]
    fn the_guarded_arities_admit_exactly_the_token_slot() {
        for op in [
            Op::Load(MemKind::Int),
            Op::ArrayLoad(MemKind::Byte),
            Op::ArrayLength,
            Op::Div,
            Op::Rem,
        ] {
            let shape = op.guard_shape().expect("a guarded op");
            let (min, max) = expected_arity(&op);
            assert!(
                min <= shape.unguarded_arity,
                "{op:?}: the unguarded form must still verify"
            );
            assert_eq!(
                max,
                shape.unguarded_arity + 1,
                "{op:?}: the arity lane must admit the token slot and nothing past it"
            );
        }
        // The store family carries no token, so its arity must NOT have room
        // for one — an extra operand there would be read as the stored value.
        for op in [Op::Store(MemKind::Int), Op::ArrayStore(MemKind::Int)] {
            assert_eq!(op.guard_shape(), None, "{op:?}");
            assert_eq!(expected_arity(&op), (5, 5), "{op:?}");
        }
    }

    /// A guarded `Load` / `ArrayLoad` / `ArrayLength` verifies clean on every
    /// lane.
    ///
    /// **What this catches, and why it is the test that matters.** Drop the
    /// guard-token filter in `data_input_indices` and this fails with "is a
    /// Void token used as a data value": `check_types` classifies slots `2..`
    /// of a memory access as data, and it runs in every build profile at
    /// `PHASE_POST_OPTIMIZE`. In production that finding is not a failing test
    /// — it makes `ir_verify_reject` true and the method quietly falls back
    /// to the single-pass backend. `NOTES-opts7.md`'s recipe for this change
    /// did not mention `data_input_indices` at all.
    #[test]
    fn a_guarded_memory_access_verifies_clean() {
        let (mut g, start, ctrl, mem) = guarded_head();
        let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let cond = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![base, null],
            None,
        );
        let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, cond], None);
        let off = g.add(Op::Const(3), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, off, guard],
            None,
        );
        let al = g.add(
            Op::ArrayLoad(MemKind::Byte),
            IrType::Int,
            vec![ctrl, load, base, off, guard],
            None,
        );
        let len = g.add(
            Op::ArrayLength,
            IrType::Int,
            vec![ctrl, al, base, guard],
            None,
        );
        finish(&mut g, ctrl, len);
        let r = verify_graph(&g, "test", VerifyOptions::all());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    /// A guarded `Div` verifies clean.
    ///
    /// What this catches: `Op::Div`/`Op::Rem` left inside the `(2, 2)`
    /// arithmetic group of `expected_arity`, or inside the homogeneous group
    /// of `ir::expected_input_type`. Both are the failure `NOTES-opts7.md`
    /// calls "the one that will not show up in a unit test" — because the
    /// div-zero token exists only on graphs built from real bytecode. This IS
    /// that unit test: it builds the shape by hand.
    #[test]
    fn a_guarded_division_verifies_clean() {
        let (mut g, _start, ctrl, _mem) = guarded_head();
        let a = g.add(Op::Const(12), IrType::Int, vec![], None);
        let b = g.add(Op::Const(4), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let nz = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![b, zero],
            None,
        );
        let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, nz], None);
        let div = g.add(Op::Div, IrType::Int, vec![a, b, guard], None);
        finish(&mut g, ctrl, div);
        let r = verify_graph(&g, "test", VerifyOptions::all());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    /// A value sitting in the guard-token slot is reported.
    ///
    /// What this catches: a producer that appends an ordinary operand to a
    /// node of the guarded family. Nothing downstream would miscompile it —
    /// every consumer reads the base and offset out of fixed slots and the
    /// type lane now skips the trailing one — so the operand would simply be
    /// dropped, invisibly, which is why the lane exists.
    #[test]
    fn a_non_guard_in_the_guard_token_slot_is_reported() {
        let (mut g, start, ctrl, mem) = guarded_head();
        let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let off = g.add(Op::Const(3), IrType::Int, vec![], None);
        let stray = g.add(Op::Const(9), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, off, stray],
            None,
        );
        finish(&mut g, ctrl, load);
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("guard-token slot"), "{m}");
    }

    /// The unguarded shapes are untouched: the token slot is identified by
    /// arity, so widening the arity range must not make the documented form
    /// look guarded — nor must it stop the compact hand-built `ArrayLength`
    /// form verifying.
    #[test]
    fn an_unguarded_access_still_verifies_and_names_no_token() {
        let (mut g, start, ctrl, mem) = guarded_head();
        let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let off = g.add(Op::Const(3), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, off],
            None,
        );
        let compact_len = g.add(Op::ArrayLength, IrType::Int, vec![base], None);
        let sum = g.add(Op::Add, IrType::Int, vec![load, compact_len], None);
        finish(&mut g, ctrl, sum);
        assert_eq!(crate::ir::guard_token_slot(&g.nodes[load as usize]), None);
        assert_eq!(
            crate::ir::guard_token_slot(&g.nodes[compact_len as usize]),
            None
        );
        let r = verify_graph(&g, "test", VerifyOptions::all());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    // ── The staged operand-type lane ─────────────────────────────────────
    //
    // Called directly rather than through `verify_graph`, because the lane is
    // gated on an explicit `CRATONVM_JIT_VERIFY_TYPES=1` that is latched on
    // first read — a test that set it would be a test that depends on which
    // other test ran first.

    fn operand_findings(g: &Graph) -> Option<String> {
        let mut v = Violations::new();
        check_operand_types(g, &mut v);
        v.into_result("test").err().map(|e| message(&e))
    }

    /// The defect `NOTES-irfront.md` §4 opens with: `Op::Cmp` is the IR's
    /// `if_icmp*` **and** its `if_acmp*`, so it has no fixed operand type and
    /// `check_types` therefore required nothing of it — while `ir_lower`
    /// selects a 64-bit signed `CMP` whenever either operand is `Ref` or
    /// `Long`. One wrongly-`Long`-typed operand inverts an `if_icmplt`,
    /// because ints are stored zero-extended and a negative int reads as a
    /// large positive 64-bit value.
    #[test]
    fn a_comparison_of_an_int_against_a_long_is_reported() {
        let mut g = linear_graph();
        let i = g.add(Op::Const(1), IrType::Int, vec![], None);
        let l = g.add(Op::Const(2), IrType::Long, vec![], None);
        let cmp = g.add(Op::Cmp(crate::ir::CmpOp::Lt), IrType::Int, vec![i, l], None);
        g.nodes[g.exit as usize].inputs[1] = cmp;
        let m = operand_findings(&g).expect("a mixed-width compare is a finding");
        assert!(m.contains("input[1]"), "{m}");
        assert!(m.contains("long"), "{m}");
    }

    /// …and the homogeneous case verifies, at either width. The requirement is
    /// sameness, not a particular type — a reference comparison is exactly as
    /// legal as an integer one.
    #[test]
    fn a_comparison_of_two_operands_of_one_type_is_clean() {
        for ty in [IrType::Int, IrType::Long, IrType::Ref] {
            let mut g = linear_graph();
            let a = g.add(Op::Const(1), ty, vec![], None);
            let b = g.add(Op::Const(2), ty, vec![], None);
            let cmp = g.add(Op::Cmp(crate::ir::CmpOp::Eq), IrType::Int, vec![a, b], None);
            g.nodes[g.exit as usize].inputs[1] = cmp;
            assert_eq!(operand_findings(&g), None, "{ty:?}");
        }
    }

    /// `ScalarOp::input_ty` is documented in `ir.rs` as "the single source of
    /// truth" for each intrinsic family's operand types, and nothing consulted
    /// it: `ScalarOp::MinL` fed two `Int` nodes verified clean and lowered to a
    /// 64-bit `CMP`/`CMOV` over two 32-bit values.
    #[test]
    fn a_long_intrinsic_fed_int_operands_is_reported() {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        let min = g.add(
            Op::ScalarIntrinsic(crate::ir::ScalarOp::MinL),
            IrType::Long,
            vec![a, b],
            None,
        );
        g.nodes[g.exit as usize].inputs[1] = min;
        let m = operand_findings(&g).expect("MinL over two ints is a finding");
        assert!(m.contains("requires long"), "{m}");
    }

    /// The distance operand of a rotate is an `int` at both widths, which is
    /// the asymmetry `ScalarOp::input_ty` exists to record: `Long.rotateLeft`
    /// takes a `long` value and an `int` distance.
    #[test]
    fn a_long_rotate_takes_a_long_value_and_an_int_distance() {
        let mut g = linear_graph();
        let v = g.add(Op::Const(1), IrType::Long, vec![], None);
        let d = g.add(Op::Const(3), IrType::Int, vec![], None);
        let rot = g.add(
            Op::ScalarIntrinsic(crate::ir::ScalarOp::RotateLeftL),
            IrType::Long,
            vec![v, d],
            None,
        );
        g.nodes[g.exit as usize].inputs[1] = rot;
        assert_eq!(operand_findings(&g), None);

        // …and the two swapped is a finding on BOTH slots.
        let mut g = linear_graph();
        let v = g.add(Op::Const(1), IrType::Int, vec![], None);
        let d = g.add(Op::Const(3), IrType::Long, vec![], None);
        let rot = g.add(
            Op::ScalarIntrinsic(crate::ir::ScalarOp::RotateLeftL),
            IrType::Long,
            vec![v, d],
            None,
        );
        g.nodes[g.exit as usize].inputs[1] = rot;
        let m = operand_findings(&g).expect("swapped operands are a finding");
        assert!(m.contains("input[0]"), "{m}");
        assert!(m.contains("input[1]"), "{m}");
    }

    /// A `Ref` where an array index belongs, and an `Int` where the base
    /// belongs. Neither was required of `Op::ArrayLoad` before the table.
    #[test]
    fn an_array_access_needs_a_reference_base_and_an_int_index() {
        let mut g = linear_graph();
        let ctrl = g.nodes[g.exit as usize].inputs[0];
        let mem = 2; // the Start's memory projection
        let base = g.add(Op::Const(0), IrType::Int, vec![], None);
        let index = g.add(Op::Const(0), IrType::Ref, vec![], None);
        let load = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, index],
            None,
        );
        g.nodes[g.exit as usize].inputs[1] = load;
        let m = operand_findings(&g).expect("a swapped base and index is a finding");
        assert!(m.contains("input[2]"), "{m}");
        assert!(m.contains("requires reference"), "{m}");
        assert!(m.contains("input[3]"), "{m}");
    }

    /// The lane makes no claim about a call's arguments (their types are the
    /// callee descriptor's, which no `&Op` carries) or about a φ's inputs (the
    /// join lattice in `check_types` is that requirement). Both are shapes the
    /// table knowingly cannot describe, and it answers `TypeReq::Any` rather
    /// than guessing — the distinction that keeps a modelling gap from
    /// becoming a fleet-wide fallback.
    #[test]
    fn the_operand_lane_makes_no_claim_about_call_arguments_or_phi_inputs() {
        let mut g = linear_graph();
        let ctrl = g.nodes[g.exit as usize].inputs[0];
        let mem = 2;
        let a = g.add(Op::Const(1), IrType::Ref, vec![], None);
        let b = g.add(Op::Const(2), IrType::Double, vec![], None);
        let call = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![ctrl, mem, a, b],
            None,
        );
        g.nodes[g.exit as usize].inputs[1] = call;
        assert_eq!(operand_findings(&g), None);
    }

    /// Only `Start` and `If` are multi-output, so a projection of anything else
    /// names an output that does not exist.
    #[test]
    fn a_projection_of_a_single_output_node_is_rejected() {
        let mut g = linear_graph();
        let k = g.nodes[g.exit as usize].inputs[1];
        let bad = g.add(Op::Proj(0), IrType::Int, vec![k], None);
        g.nodes[g.exit as usize].inputs[1] = bad;
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("produces no projections"), "{m}");
    }

    /// The branch-completeness lane, driven directly so the test does not
    /// depend on the ambient `CRATONVM_JIT_VERIFY_BRANCHES` / build profile.
    fn branch_findings(g: &Graph) -> Vec<String> {
        let mut v = Violations::new();
        check_branch_completeness(g, &mut v);
        v.list
    }

    /// A diamond and a dense switch keep every arm; a diamond whose false
    /// projection was killed (with the merge and φ trimmed to match, so every
    /// OTHER lane is quiet — the shape DCE left behind for an infinite-loop
    /// arm) is reported, as is a projection that survives but leads nowhere.
    #[test]
    fn a_branch_arm_with_no_successor_is_reported_by_the_branch_lane() {
        assert!(branch_findings(&diamond_graph()).is_empty());

        // Dense switch on a parameter: low 0, high 1 → 3 projections.
        let mut s = linear_graph();
        let ctrl = s.nodes[s.exit as usize].inputs[0];
        let key = s.add(Op::Param(0), IrType::Int, vec![], None);
        let sw = s.add(
            Op::Switch { low: 0, high: 1 },
            IrType::Control,
            vec![ctrl, key],
            None,
        );
        let arms: Vec<NodeId> = (0..3u16)
            .map(|k| s.add(Op::Proj(k), IrType::Control, vec![sw], None))
            .collect();
        let join = s.add(Op::Merge, IrType::Control, arms.clone(), None);
        s.nodes[s.exit as usize].inputs[0] = join;
        assert!(branch_findings(&s).is_empty(), "{:?}", branch_findings(&s));

        // Diamond with the false arm deleted.
        let mut g = diamond_graph();
        let merge = g.nodes[g.exit as usize].inputs[0];
        let f = g.nodes[merge as usize].inputs[1];
        g.nodes[merge as usize].inputs.pop();
        let phi = g.nodes[g.exit as usize].inputs[1];
        g.nodes[phi as usize].inputs.pop();
        assert!(
            verify_graph(&g, "test", VerifyOptions::all()).is_ok()
                || branch_completeness_lane_enabled("test"),
            "every pre-existing lane accepts the one-armed diamond — which is the gap",
        );
        let with_dangling_proj = branch_findings(&g);
        assert!(
            with_dangling_proj
                .iter()
                .any(|m| m.contains("leads nowhere")),
            "{with_dangling_proj:?}"
        );
        g.kill(f);
        let with_no_proj = branch_findings(&g);
        assert!(
            with_no_proj.iter().any(|m| m.contains("no successor")),
            "{with_no_proj:?}"
        );
    }

    /// The type lane is the one that catches a reference merged into an
    /// `Int`-typed φ — the shape that is neither zero-initialised by
    /// `ir_lower::zero_ref_phi_slots` nor reported to the collector. The module
    /// header has argued for years that this module "names it rather than
    /// letting it reach the lowerer"; until 2026-09-17 it did not, in any
    /// release build, because the lane followed `cfg!(debug_assertions)`.
    #[test]
    fn the_type_lane_runs_at_post_optimize_in_every_build_profile() {
        assert!(
            VerifyOptions::for_phase(PHASE_POST_OPTIMIZE).check_types,
            "the cheapest lane, and the only one that sees a GC-unsound φ, must \
             not depend on the build profile at the hook where the graph is \
             known clean",
        );
    }

    /// The exception a `Throw` carries is a value, and it was in no lane at
    /// all: `data_input_indices` did not list `Op::Throw`, so nothing required
    /// the operand `helpers.throw_exception` receives as an oop to be one.
    #[test]
    fn a_control_token_used_as_a_thrown_exception_is_rejected() {
        let mut g = linear_graph();
        let ctrl = g.nodes[g.exit as usize].inputs[0];
        let mem = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Proj(1)))
            .expect("memory projection") as NodeId;
        // `[ctrl, mem, exc]` with the control token standing in for the oop.
        let throw = g.add(Op::Throw, IrType::Void, vec![ctrl, mem, ctrl], None);
        g.exit = throw;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("control token used as a data value"), "{m}");
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
        // A well-typed store (`Int` offset and value), so the operand type
        // lane has nothing to say and the assertion below is about the chain.
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let val = g.add(Op::Const(1), IrType::Int, vec![], None);
        let st = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, arr, off, val],
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
            frame_snapshot: None,
            volatile_access: false,
        };
        assert!(!is_memory_token_input(&compact, 1));
        let full = Node {
            op: Op::ArrayLength,
            ty: IrType::Int,
            inputs: vec![1, 2, 3].into(),
            bytecode_pc: None,
            frame_snapshot: None,
            volatile_access: false,
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
            monitors: Vec::new(),
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
    ///
    /// The nodes carry bci 4 deliberately: a duplicated bci is only a defect
    /// when the code actually emits something there, and an UNCLAIMED
    /// duplicate is what the check is for. See
    /// [`two_claimed_snapshots_at_one_bci_are_a_copied_body`] for the other
    /// side of the rule.
    #[test]
    fn two_snapshots_at_one_bci_are_rejected() {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
        let b = g.add(Op::Const(2), IrType::Int, vec![], Some(4));
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![a],
            stack: vec![],
            monitors: Vec::new(),
        });
        // A second, DIFFERENT frame state at the same bci.
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![b],
            stack: vec![],
            monitors: Vec::new(),
        });
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("describe bci 4"), "{m}");
        assert!(m.contains("silently discarded"), "{m}");
    }

    /// Two snapshots at one bci are FINE when every one of them is named by a
    /// node — that is a copied loop body, not a collision.
    ///
    /// The relaxation this check grew for `ir_optimize::unroll`'s per-copy
    /// deopt frames, and it is paired with
    /// [`two_snapshots_at_one_bci_are_rejected`] on purpose: the two fixtures
    /// differ only in whether the nodes claim their snapshots, so neither can
    /// pass by the check having quietly stopped running.
    #[test]
    fn two_claimed_snapshots_at_one_bci_are_a_copied_body() {
        let mut g = linear_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
        let b = g.add(Op::Const(2), IrType::Int, vec![], Some(4));
        g.push_safepoint(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![a],
            stack: vec![],
            monitors: Vec::new(),
        });
        g.push_safepoint(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![b],
            stack: vec![],
            monitors: Vec::new(),
        });
        assert!(g.set_node_frame_snapshot(a, 0));
        assert!(g.set_node_frame_snapshot(b, 1));
        assert!(
            verify_graph(&g, "test", VerifyOptions::default()).is_ok(),
            "two snapshots at one bci, each claimed by the node it describes, \
             is what a per-copy unroll produces",
        );

        // Drop ONE claim and the collision is back: nothing names snapshot 1,
        // so the by-bci scan resolves that program point to snapshot 0.
        g.nodes[b as usize].frame_snapshot = None;
        let err = verify_graph(&g, "test", VerifyOptions::default()).unwrap_err();
        assert!(
            message(&err).contains("describe bci 4"),
            "{}",
            message(&err)
        );
    }

    /// A duplicated bci NO node carries is unconsultable, not ambiguous.
    ///
    /// `build_deopt_points` anchors through `bci_native`, which is populated
    /// only from nodes, and every guard resumes at some node's own
    /// `bytecode_pc` — so nothing can ever ask about a bci the code does not
    /// emit. This is the normal state of a graph after a full unroll, whose
    /// post-unroll constant fold retires the very nodes that made the body's
    /// bcis real.
    #[test]
    fn duplicate_snapshots_at_an_unemitted_bci_are_not_a_violation() {
        let mut g = linear_graph();
        let k = g.add(Op::Const(1), IrType::Int, vec![], Some(9));
        for _ in 0..2 {
            g.push_safepoint(crate::ir::SafepointSnapshot {
                bci: 4,
                locals: vec![k],
                stack: vec![],
                monitors: Vec::new(),
            });
        }
        assert!(
            verify_graph(&g, "test", VerifyOptions::default()).is_ok(),
            "no node carries bci 4, so neither snapshot can be reached",
        );
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
                monitors: Vec::new(),
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
        let k = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![k],
            stack: vec![],
            monitors: Vec::new(),
        });
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![k],
            stack: vec![],
            monitors: Vec::new(),
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
            monitors: Vec::new(),
        });
        g.kill(k);
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
        assert!(verify_graph(&g, "test", VerifyOptions::default()).is_err());
    }

    /// A frame slot holds a value; a memory token there has no interpreter
    /// representation (added 2026-09-18).
    #[test]
    fn a_frame_state_slot_naming_a_token_is_rejected() {
        let mut g = linear_graph();
        // n2 is `Proj(1)` of `Start`: the initial MEMORY token.
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![2],
            stack: vec![],
            monitors: Vec::new(),
        });
        let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
        assert!(m.contains("local[0]"), "{m}");
        assert!(m.contains("memory token"), "{m}");
        // Structural-only verification does not run the lane.
        assert!(verify_graph(&g, "test", VerifyOptions::structural()).is_ok());
    }

    /// An undefined local is legal; an undefined HELD MONITOR is not.
    #[test]
    fn an_undefined_monitor_in_a_frame_state_is_rejected() {
        let mut g = linear_graph();
        g.safepoints.push(crate::ir::SafepointSnapshot {
            bci: 4,
            locals: vec![NO_NODE],
            stack: vec![],
            monitors: vec![],
        });
        assert!(verify_graph(&g, "test", VerifyOptions::default()).is_ok());
        g.safepoints[0].monitors.push(NO_NODE);
        let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
        assert!(m.contains("monitor[0] is NO_NODE"), "{m}");
    }

    /// `MonitorEnter` pins control at input 0 like every other runtime-reaching
    /// op; a data node there is reported (added 2026-09-18).
    #[test]
    fn a_monitor_taking_control_from_a_data_node_is_rejected() {
        let mut ok = linear_graph();
        let obj = ok.add(Op::Param(0), IrType::Ref, vec![0], None);
        // n1 = control, n2 = memory.
        ok.add(Op::MonitorEnter, IrType::Memory, vec![1, 2, obj], None);
        assert!(verify_graph(&ok, "test", VerifyOptions::structural()).is_ok());

        let mut bad = linear_graph();
        let obj = bad.add(Op::Param(0), IrType::Ref, vec![0], None);
        // n3 is the `Const(7)` the return reads — not a control token.
        bad.add(Op::MonitorEnter, IrType::Memory, vec![3, 2, obj], None);
        let m = message(&verify_graph(&bad, "test", VerifyOptions::structural()).unwrap_err());
        assert!(m.contains("MonitorEnter"), "{m}");
        assert!(m.contains("produces no control token"), "{m}");
    }

    /// A call's arguments are values: a control token marshalled as one is
    /// reported by the type lane (added 2026-09-18).
    #[test]
    fn a_control_token_passed_as_a_call_argument_is_rejected() {
        let mut g = linear_graph();
        g.add(Op::Call { info_ptr: 0 }, IrType::Void, vec![1, 2, 1], None);
        let m = message(&verify_graph(&g, "test", VerifyOptions::default()).unwrap_err());
        assert!(m.contains("Call"), "{m}");
        assert!(m.contains("used as a data value"), "{m}");
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
                monitors: Vec::new(),
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

    /// With nothing set, `from_env`'s three real lanes follow the master gate
    /// and the arena-order lane does not.
    ///
    /// This replaces `options_from_env_defaults_to_structural_only`, whose name
    /// stopped describing the contract on 2026-09-16: `from_env` used to answer
    /// "structural only" in every build, which is why a debug test run never
    /// type-checked a graph. The three lanes now default to
    /// `subchecks_follow_the_master_gate()` and arena order to `false` (see
    /// `ARENA_ORDER_FOLLOWS_THE_MASTER_GATE`).
    ///
    /// Written against the *functions*, not against today's values: flipping
    /// the kill switch, or building in release, must not require editing this
    /// test. Each clause is still conditional on the variable being unset,
    /// because an operator running the suite with one of these exported is
    /// asking for a different configuration and is not a failure.
    ///
    // REVIEW-NOTE (2026-09-16): `jit/tests/no_presence_only_flag_reads.rs`'s
    // `ALLOWED` doc comment names this test by its OLD name,
    // `options_from_env_defaults_to_structural_only`. That file is outside this
    // change's write scope; its count is unchanged (this test still makes
    // exactly one presence-only read, through `unset`), so the ratchet still
    // passes — only the row's label is stale. Owner of
    // `jit/tests/no_presence_only_flag_reads.rs`: update that one row to
    // `sub_check_defaults_follow_the_master_gate`.
    #[test]
    fn sub_check_defaults_follow_the_master_gate() {
        let unset = |n: &str| cratonvm_types::flags::runtime_var_os(n).is_none();
        let o = VerifyOptions::from_env();
        let gated = subchecks_follow_the_master_gate();
        if unset("CRATONVM_JIT_VERIFY_TYPES") {
            assert_eq!(o.check_types, gated);
        }
        if unset("CRATONVM_JIT_VERIFY_FRAME_STATES") {
            assert_eq!(o.check_frame_states, gated);
        }
        if unset("CRATONVM_JIT_VERIFY_MEMORY_CHAIN") && unset("CRATONVM_JIT_VERIFY_SCHEDULE") {
            assert_eq!(o.check_memory_chain, gated);
        }
        if unset("CRATONVM_JIT_VERIFY_ARENA_ORDER") && unset("CRATONVM_JIT_VERIFY_SCHEDULE") {
            assert_eq!(
                o.check_arena_order,
                ARENA_ORDER_FOLLOWS_THE_MASTER_GATE && gated,
                "the arena-order lane is exempt on the merits — it fires on every \
                 optimized graph by construction, and `for_phase` propagates this \
                 value unchanged at every phase"
            );
        }
        // The structural lane is unconditional, so `structural()` is still the
        // floor every configuration sits on top of and is unaffected by any of
        // this.
        let floor = VerifyOptions::structural();
        assert!(
            !floor.check_types
                && !floor.check_frame_states
                && !floor.check_memory_chain
                && !floor.check_arena_order
        );
    }

    /// Was this lane turned on by an operator, rather than by the build
    /// profile?
    ///
    /// The distinction `for_phase` rests on since 2026-09-16: an explicit `=1`
    /// forces a lane on at any phase, a default-derived `true` runs only where
    /// the phase declares the lane clean. Before the sub-check defaults
    /// followed the master gate the two were the same thing, which is why the
    /// tests below used to spell this `env.check_*`.
    fn lane_forced(name: &str) -> bool {
        env_flag(name) == Some(true)
    }

    /// The builder's own output is checked structurally and nothing more:
    /// every optional lane is a claim some later pass is what establishes, and
    /// no pass has run. Staging matters here — the type lane defaults to the
    /// build profile, so a debug build would otherwise have got a broad new
    /// type requirement at the hook that introduced the phase, with a silent
    /// fall-back to the single-pass tier as the failure mode.
    #[test]
    fn the_post_build_hook_runs_the_structural_lane_and_nothing_defaulted() {
        let post_build = VerifyOptions::for_phase(PHASE_POST_BUILD);
        assert_eq!(
            post_build.check_types,
            lane_forced("CRATONVM_JIT_VERIFY_TYPES")
        );
        assert_eq!(
            post_build.check_frame_states,
            lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES")
        );
        assert_eq!(
            post_build.check_memory_chain,
            lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
        );
        // Propagated unchanged, exactly as at every other phase — the lane is
        // enabled by no phase declaration anywhere.
        assert_eq!(
            post_build.check_arena_order,
            VerifyOptions::from_env().check_arena_order
        );
    }

    /// The type lane is unconditional at `"post-optimize"` and staged behind
    /// an explicit `=1` at `"post-build"`. Stated as a relation between the
    /// two phases so that promoting the lane later is one edit here.
    #[test]
    fn the_type_lane_is_forced_after_optimize_but_only_offered_after_build() {
        assert!(VerifyOptions::for_phase(PHASE_POST_OPTIMIZE).check_types);
        if !lane_forced("CRATONVM_JIT_VERIFY_TYPES") {
            assert!(!VerifyOptions::for_phase(PHASE_POST_BUILD).check_types);
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

        for phase in ["post-escape-analysis", "pre-lower"] {
            let o = VerifyOptions::for_phase(phase);
            assert!(!o.check_arena_order, "{phase}");
            // Flipping either constant is the whole change; these state the
            // invariant either way rather than pinning today's value, so a flip
            // needs no test edit.
            // `lane_forced`, not `env.check_*`: a lane on only because this is
            // a debug build must NOT override a phase declaration. It did,
            // briefly, and the cost was that every method whose escape analysis
            // scalar-replaced an allocation live across a safepoint bailed the
            // IR tier -- a silent throughput regression, because the
            // frame-state lane's post-EA finding is an INTENTIONAL state (see
            // `APPLY_EA_ROUTES_ALL_SAFEPOINTS`).
            assert_eq!(
                o.check_frame_states,
                APPLY_EA_ROUTES_ALL_SAFEPOINTS || lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES"),
                "{phase}"
            );
            assert_eq!(
                o.check_memory_chain,
                APPLY_EA_SPLICES_MEMORY_CHAIN
                    || lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                    || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE"),
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
        let pre_lower = VerifyOptions::for_phase("pre-lower");
        assert_eq!(
            pre_lower.check_memory_chain,
            APPLY_EA_SPLICES_MEMORY_CHAIN
                || lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
        );
        assert_eq!(
            pre_lower.check_frame_states,
            APPLY_EA_ROUTES_ALL_SAFEPOINTS || lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES")
        );
        // Neither constant has any effect at the pre-EA hook: both lanes are
        // unconditionally on there.
        let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
        assert!(post_opt.check_memory_chain && post_opt.check_frame_states);
    }

    /// An operator's EXPLICIT `=1` may only *add* lanes to a phase's
    /// defaults, never subtract: turning the gate down is
    /// `CRATONVM_JIT_VERIFY_IR=0`, which disables it outright rather than
    /// silently narrowing it.
    ///
    /// # Why this is about `=1` and not about `from_env`
    ///
    /// It used to test `env.check_*`, which was the same question while every
    /// lane defaulted to `false`. Since the sub-check defaults follow the
    /// master gate, `env.check_frame_states` is `true` in a debug build for
    /// nobody in particular, and carrying THAT into a phase whose declaring
    /// constant says the lane is unverified there is not "the operator added a
    /// lane" -- it is a default overriding a declaration. The property worth
    /// keeping is the one about the operator, and it is the one tested here.
    #[test]
    fn an_explicit_lane_request_is_never_dropped_by_a_phase() {
        for phase in [PHASE_POST_OPTIMIZE, "post-escape-analysis", "pre-lower"] {
            let o = VerifyOptions::for_phase(phase);
            assert!(
                !lane_forced("CRATONVM_JIT_VERIFY_TYPES") || o.check_types,
                "{phase}"
            );
            assert!(
                !lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES") || o.check_frame_states,
                "{phase}"
            );
            assert!(
                !lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN") || o.check_memory_chain,
                "{phase}"
            );
            assert!(
                !lane_forced("CRATONVM_JIT_VERIFY_ARENA_ORDER") || o.check_arena_order,
                "{phase}"
            );
        }
    }

    /// A lane that is on only because this is a debug build does NOT run at a
    /// phase whose declaring constant says it is unverified there.
    ///
    /// The other half of the rule above, and the one with a live cost: the
    /// frame-state lane's post-EA finding is an intentional state
    /// (`APPLY_EA_ROUTES_ALL_SAFEPOINTS`), so running it there turns every
    /// scalar-replacing method into an IR-tier bailout -- silently, as lost
    /// throughput with no failing test.
    #[test]
    fn a_default_derived_lane_respects_the_phase_declaration() {
        for phase in ["post-escape-analysis", "pre-lower"] {
            let o = VerifyOptions::for_phase(phase);
            if !APPLY_EA_ROUTES_ALL_SAFEPOINTS && !lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES") {
                assert!(
                    !o.check_frame_states,
                    "{phase}: the frame-state lane is declared unverified here                      and nobody asked for it explicitly"
                );
            }
            if !APPLY_EA_SPLICES_MEMORY_CHAIN
                && !lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                && !lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
            {
                assert!(!o.check_memory_chain, "{phase}");
            }
        }
        // The pre-EA hook is unaffected: both lanes are unconditionally on
        // there, which is where the 2026-09-16 default flip's value lives.
        let post_opt = VerifyOptions::for_phase(PHASE_POST_OPTIMIZE);
        assert!(post_opt.check_frame_states && post_opt.check_memory_chain);
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

        // "the two EA constants are the switches for the post-EA phases" --
        // together with an explicit operator request, which is the only other
        // thing that reaches those phases.
        let pre_lower = VerifyOptions::for_phase("pre-lower");
        assert_eq!(
            pre_lower.check_frame_states,
            APPLY_EA_ROUTES_ALL_SAFEPOINTS || lane_forced("CRATONVM_JIT_VERIFY_FRAME_STATES")
        );
        assert_eq!(
            pre_lower.check_memory_chain,
            APPLY_EA_SPLICES_MEMORY_CHAIN
                || lane_forced("CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                || lane_forced("CRATONVM_JIT_VERIFY_SCHEDULE")
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

    /// The master gate follows the build profile when unset — and, since
    /// 2026-09-16, so do the optional lanes it advertises.
    ///
    /// The second half is the new half. `verify_enabled()` answering `true` in
    /// a debug build used to mean only "the structural lane runs at the
    /// per-pass hooks": every optional lane defaulted to `false` regardless, so
    /// a build that reported verification on was not type-checking, not
    /// checking frame states and not checking memory tokens. That is the gap
    /// this asserts is closed.
    ///
    /// Stated as an implication, not an equality, because the kill switch
    /// inside `subchecks_follow_the_master_gate` is allowed to make the
    /// sub-checks *narrower* than the master gate; what it must never produce
    /// is optional lanes on while the master gate is off.
    #[test]
    fn verify_enabled_matches_the_build_profile_when_unset() {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_VERIFY_IR").is_none() {
            assert_eq!(verify_enabled(), cfg!(debug_assertions));
            assert!(!pre_lower_verify_disabled());
            assert!(
                !subchecks_follow_the_master_gate() || verify_enabled(),
                "the optional lanes default ON while the master gate is off — a \
                 configuration nothing in this module intends"
            );
            // That the lanes actually *take* this default is
            // `sub_check_defaults_follow_the_master_gate`, which has to guard
            // each clause on its own variable being unset and so cannot be
            // folded in here.
        }
    }

    // ── Lane: projections of an `Op::Switch` ─────────────────────────

    /// A three-way switch over `[1, 3]` plus its default, verified clean, so
    /// the negative tests below are demonstrably about what they claim.
    fn switch_graph(low: i32, high: i32) -> Graph {
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
        let key = g.add(Op::Const(0), IrType::Int, vec![], None);
        let sw = g.add(
            Op::Switch { low, high },
            IrType::Control,
            vec![ctrl, key],
            None,
        );
        let outputs = crate::ir::switch_projection_count(low, high);
        let mut edges = Vec::with_capacity(outputs);
        for k in 0..outputs {
            edges.push(g.add(Op::Proj(k as u16), IrType::Control, vec![sw], None));
        }
        let merge = g.add(Op::Merge, IrType::Control, edges, None);
        let ret = g.add(Op::Return, IrType::Void, vec![merge], None);
        g.entry = start;
        g.exit = ret;
        g
    }

    #[test]
    fn a_switch_projects_one_output_per_case_plus_the_default() {
        assert_eq!(projection_arity(&Op::Switch { low: 1, high: 3 }), 4);
        assert_eq!(projection_arity(&Op::Switch { low: -3, high: -1 }), 4);
        assert_eq!(projection_arity(&Op::Switch { low: 7, high: 7 }), 2);
        let g = switch_graph(1, 3);
        let r = verify_graph(&g, "test", VerifyOptions::all());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    /// The `i64` in `projection_arity`, as a test. `high - low` OVERFLOWS
    /// `i32` for a full-`int`-range `tableswitch`: computed at that width the
    /// arity would come out as `1`, and every out-of-range `Proj` of the node
    /// would pass this lane. Saturated rather than wrapped, so the answer is
    /// implausible instead of plausible.
    #[test]
    fn a_full_range_switchs_projection_arity_does_not_overflow_into_a_small_number() {
        let op = Op::Switch {
            low: i32::MIN,
            high: i32::MAX,
        };
        assert_eq!(projection_arity(&op), u32::MAX as usize);
        assert!(projection_arity(&op) > crate::ir::SWITCH_MAX_PROJECTIONS);
    }

    /// `ir_lower` resolves a switch edge by MATCHING the projection index, so
    /// an index past the last output leaves a case edge unlowered — the same
    /// failure an out-of-range `Proj` of an `Op::If` has.
    #[test]
    fn a_projection_past_a_switchs_default_edge_is_reported() {
        let mut g = switch_graph(1, 3);
        let last = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Proj(3))
            .expect("the default projection");
        g.nodes[last].op = Op::Proj(9);
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("selects output 9"), "{m}");
    }

    /// A switch is `[ctrl, key]` and nothing else: the case keys are the
    /// projection indices, not edges, so a 255-case switch still has two
    /// inputs.
    #[test]
    fn a_switch_takes_exactly_a_control_token_and_a_key() {
        assert_eq!(expected_arity(&Op::Switch { low: 0, high: 9 }), (2, 2));
        let mut g = switch_graph(1, 3);
        let sw = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Switch { .. }))
            .expect("the switch");
        g.nodes[sw].inputs.pop();
        let err = verify_graph(&g, "test", VerifyOptions::structural()).unwrap_err();
        let m = message(&err);
        assert!(m.contains("exactly 2"), "{m}");
    }
}
