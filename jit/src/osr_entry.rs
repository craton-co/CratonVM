// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Validated OSR entry: the contract a compiled body publishes for on-stack
//! replacement, and the trampoline that enters it.
//!
//! Split out of `lib.rs` on 2026-09-16, the same way and for the same reason as
//! `jfr_compile_decision`: `lib.rs` had grown past 46,000 lines, and at that
//! size a file stops being a module and becomes a place where unrelated
//! concerns happen to sit next to each other. This section is not one of those
//! accidents -- it is a closed loop, and the loop is the boundary.
//!
//! The loop: a compiled artifact does not simply "support OSR". It publishes an
//! entry table, and `CompiledMethod::validate_osr_entry` turns a requested
//! back-edge pc plus the interpreter's live frame into either an
//! [`OsrEntryPlan`] -- a seeded, register-assigned, slot-typed description of
//! exactly how to land in that body -- or a refusal. The only thing anyone can
//! do with a plan is hand it to `osr_trampoline`, which builds the native frame
//! around the body's OSR entry and jumps in. Validation with no trampoline is a
//! verdict nobody can act on; a trampoline with no validation is a jump into a
//! frame whose shape was never checked. Neither half is independently useful,
//! and nothing else in the JIT constructs either one.
//!
//! Everything in between is that loop's private vocabulary: [`OsrSlotType`] and
//! [`OsrSlotExpectation`] (what the interpreter has in a slot versus what the
//! compiled body was built to find there), and the `OSR_REFUSE_*` tag set with
//! its permanence and compile-state classifications. Callers outside see a plan
//! or a `bailout::Bailout` carrying one of those tags; they never build one.
//!
//! WHAT DELIBERATELY DID NOT MOVE, and why -- the split reads as arbitrary
//! otherwise:
//!
//!   * The per-(method, entry_pc) reject memo (`is_osr_entry_rejected` /
//!     `mark_osr_entry_rejected`) stayed in `lib.rs`. It is storage, not
//!     policy: it lives in `JIT_VERDICTS` beside the method's other verdicts so
//!     that it expires with the bytecode it was measured on. Moving it here
//!     would have dragged the whole verdict table along with it.
//!   * `CompiledMethod::can_osr_enter` and `osr_enter` stayed with the rest of
//!     `CompiledMethod` in `lib.rs`. They are the cheap pre-checks every
//!     back-edge runs; this module is what happens once one of them says yes.
//!     That is also why the two switches those pre-checks read --
//!     `osr_dead_local_entry_allowed` and `osr_single_pc_entry_only` -- are
//!     `pub(crate)` here rather than private: the policy is defined beside the
//!     validation it constrains, and read from the caller that has to obey it.
//!
//! Glob-re-exported from the crate root, so every path a caller used before the
//! split still resolves -- the code moved, the API did not.

use std::sync::Arc;

use rustc_hash::FxHashMap;

// `sp_id_slot_init_enabled` and `validate_code_ptr` are deliberately NOT
// imported here: both are reachable only from the `#[cfg(target_arch =
// "x86_64")]` half of this file, so importing them would raise an
// `unused_imports` warning on every other target. They are spelled
// `crate::`-qualified at their two call sites instead — the same way this file
// already spells `crate::x64::` and `crate::code_events::`.
use crate::{bailout, deopt, metrics, osr_exit, CompiledMethod, ExecutableBuffer};

// ---------------------------------------------------------------------------
// Validated OSR entry (C2 review P1 — "Add on-stack replacement")
// ---------------------------------------------------------------------------
//
// What was already here before this section, and is NOT re-implemented:
//
//   * `CompiledMethod::osr_pc_to_native` — the per-bci entry table, built from
//     `x64::Compiler::osr_entry_native` (which points *before* a LICM-hoisted
//     preheader, and `-1` for a pc strictly inside a hoisted body).
//   * `can_osr_enter` / `can_osr_enter_with` — "is there a native offset here,
//     and is the dead-local mask acceptable".
//   * `osr_enter` + `osr_trampoline` — the machine-level transition: seed
//     locals into their register/frame homes, build the frame, jump.
//   * `osr_dead_mask` + `osr_dead_local_entry_allowed` — the coalesced-register
//     hazard and its kill switch.
//   * `is_osr_entry_rejected` / `mark_osr_entry_rejected` — the per-(method,pc)
//     negative memo so a permanent refusal is decided once.
//   * `deopt_points` / `osr_exit_points` / `can_osr_exit` — the OSR-*exit*
//     snapshots the VM's in-place transfer consumes.
//
// What this section adds is the part the acceptance criterion ("long-running
// loops tier up without restarting and survive forced deopt") needs and that
// none of the above does: a **typed admission check**. `osr_enter` takes
// `jit_locals: &[i64]` — raw words with no types attached — so nothing ever
// compared the interpreter's idea of a slot against the compiled entry's. A
// `double` local seeded into a GPR home, or a `long` seeded where the compiled
// body reads a reference, is a silent miscompile, not a refusal.
//
// The three rules this section is built around:
//
// 1. **Refuse, never guess.** Every rejection is a `bailout::Bailout` carrying
//    a `BailoutReason::UnsupportedShape` tag from the closed taxonomy below,
//    counted through `bailout::record_bailout`. No panics, no `Option::None`
//    with the reason thrown away.
//
// 2. **The resume bci is exact, or there is no entry.** `OsrEntryPlan` is
//    derived from `OsrEntryState::pc` — the interpreter's *current* pc, which
//    at a taken back-edge is the loop header with zero bytes of the new
//    iteration executed. There is no separate "entry pc" argument that could
//    disagree with it. See [`OsrEntryPlan::resume_bci`].
//
// 3. **Only a validation that ran BEFORE entry may resume at the entry bci.**
//    That is the whole of the known "an OSR bail re-runs loop iterations"
//    defect: once compiled code has committed iterations, resuming the
//    interpreter at the header replays them. `validate_osr_entry` failing is
//    the only path that leaves the interpreter at `entry_pc`; after entry the
//    only sanctioned resume is [`OsrEntryPlan::resume_after_exit`], which
//    returns the *reconstructed frame's own* bci or refuses outright.

/// The JVM value kind of one interpreter slot, as the OSR entry contract sees
/// it.
///
/// Deliberately coarser than `deopt::FrameValue` (which also encodes *where*
/// the value lives) and coarser than the verifier's type lattice (no class
/// identity): the only question an OSR entry has to answer is "will the
/// compiled body read this slot's 64 bits as the same kind of thing the
/// interpreter wrote into it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsrSlotType {
    /// Cat-1 `int` family (`int`/`short`/`char`/`byte`/`boolean`).
    Int,
    /// Cat-2 `long`.
    Long,
    /// Cat-1 `float`.
    Float,
    /// Cat-2 `double`.
    Double,
    /// Object or array reference (including `null`).
    Ref,
    /// No value: an uninitialized local, or the reserved upper half of a cat-2
    /// pair. The JVM verification lattice's `top`.
    Top,
}

impl OsrSlotType {
    /// Stable short name, used in refusal context strings and test assertions.
    pub fn name(self) -> &'static str {
        match self {
            OsrSlotType::Int => "int",
            OsrSlotType::Long => "long",
            OsrSlotType::Float => "float",
            OsrSlotType::Double => "double",
            OsrSlotType::Ref => "ref",
            OsrSlotType::Top => "top",
        }
    }

    /// The interpreter's side: map a `cratonvm_types` compact value tag.
    ///
    /// `None` for `VTAG_RETADDR` (a `jsr` return address, which has no JIT
    /// representation at all) and for any tag this build does not know — both
    /// route to a refusal rather than to a guess.
    pub fn from_vtag(tag: u8) -> Option<OsrSlotType> {
        Some(match tag {
            cratonvm_types::VTAG_INT => OsrSlotType::Int,
            cratonvm_types::VTAG_LONG => OsrSlotType::Long,
            cratonvm_types::VTAG_FLOAT => OsrSlotType::Float,
            cratonvm_types::VTAG_DOUBLE => OsrSlotType::Double,
            // A `null` slot is still reference-*typed*; the compiled body will
            // read it as a pointer, and 0 is the correct pointer.
            cratonvm_types::VTAG_OBJECT | cratonvm_types::VTAG_NULL => OsrSlotType::Ref,
            cratonvm_types::VTAG_UNINIT => OsrSlotType::Top,
            _ => return None,
        })
    }

    /// The compiled side: map a deopt-metadata `FrameValue`.
    ///
    /// `None` means **undescribable** — the artifact says something lives in
    /// this slot but cannot say what. That covers
    /// [`deopt::FrameValue::Unsupported`] (unknown width),
    /// [`deopt::FrameValue::MaterializationRequired`] (an optimization deleted
    /// the value and left no rebuild recipe), and the scalar-replacement
    /// variants (which would need a heap allocation this crate cannot perform).
    // The trailing `_` arm is redundant *today* — the arms before it name every
    // `FrameValue` variant — and is kept deliberately so that a variant added to
    // `deopt.rs` (a file this change does not own) lands on "undescribable ⇒
    // refuse" instead of breaking this crate's build.
    #[allow(unreachable_patterns)]
    pub fn from_frame_value(v: &deopt::FrameValue) -> Option<OsrSlotType> {
        use deopt::FrameValue as FV;
        Some(match v {
            FV::Int(_) | FV::Register(_) | FV::StackSlot(_) => OsrSlotType::Int,
            FV::Long(_) | FV::RegisterLong(_) | FV::StackSlotLong(_) => OsrSlotType::Long,
            FV::Float(_) | FV::XmmFloat(_) | FV::StackSlotFloat(_) => OsrSlotType::Float,
            FV::Double(_) | FV::XmmDouble(_) | FV::StackSlotDouble(_) => OsrSlotType::Double,
            FV::Object(_) | FV::RegisterRef(_) | FV::StackSlotRef(_) => OsrSlotType::Ref,
            FV::Undefined => OsrSlotType::Top,
            FV::Unsupported
            | FV::MaterializationRequired(_)
            | FV::VirtualObject(_)
            | FV::VirtualObjectRef(_) => return None,
            _ => return None,
        })
    }
}

impl std::fmt::Display for OsrSlotType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What the compiled OSR entry expects to find in one slot.
///
/// Two contract strengths, because two very different artifacts exist. An
/// artifact compiled with `CRATONVM_DEOPT_REAL` carries a `FrameState` at the
/// loop bci and therefore knows each slot's exact JVM type — that is
/// [`OsrSlotExpectation::Exact`]. A production artifact carries no deopt
/// metadata at all, and the only per-slot typing left is the register-home
/// map: an XMM home can only ever hold FP, a GPR home can only ever hold an
/// integral/reference word. Those are [`OsrSlotExpectation::FloatingPoint`]
/// and [`OsrSlotExpectation::Integral`], and they are still enough to catch the
/// class of mismatch that silently corrupts a frame (an FP value seeded into a
/// GPR home is read back as an integer, and vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsrSlotExpectation {
    /// Exactly this type, from a precise `FrameState` at the entry bci.
    Exact(OsrSlotType),
    /// A general-purpose register home: `int`, `long` or a reference.
    Integral,
    /// An XMM register home: `float` or `double`.
    FloatingPoint,
    /// Memory-homed with no precise type available — nothing to check.
    Unconstrained,
    /// The trampoline does not seed this slot at all (its `osr_dead_mask` bit
    /// is set, so loading it would clobber the live owner of a coalesced
    /// register). Whatever the interpreter holds is irrelevant.
    NotSeeded,
}

impl OsrSlotExpectation {
    /// Would seeding a slot of type `got` into this expectation be sound?
    ///
    /// `Exact(Top)` accepts anything: the precise contract says nothing reads
    /// the slot at this bci. The converse is *not* true — an incoming `Top`
    /// against an `Exact` live type is a refusal, because the compiled body
    /// will read a slot the interpreter says has never been written.
    ///
    /// Under the inferred (register-home) contract an incoming `Top` **is**
    /// accepted: the dead mask only names the *hazardous* dead locals (those
    /// sharing a register with a live one), so a harmlessly-dead local reaches
    /// here unmasked and legitimately uninitialized. Refusing those is what the
    /// pre-2026-07-27 blanket dead-mask refusal did, and it cost H2's hottest
    /// method every one of its OSR entries.
    pub fn accepts(self, got: OsrSlotType) -> bool {
        match self {
            OsrSlotExpectation::Exact(OsrSlotType::Top) => true,
            OsrSlotExpectation::Exact(want) => want == got,
            OsrSlotExpectation::Integral => matches!(
                got,
                OsrSlotType::Int | OsrSlotType::Long | OsrSlotType::Ref | OsrSlotType::Top
            ),
            OsrSlotExpectation::FloatingPoint => matches!(
                got,
                OsrSlotType::Float | OsrSlotType::Double | OsrSlotType::Top
            ),
            OsrSlotExpectation::Unconstrained | OsrSlotExpectation::NotSeeded => true,
        }
    }
}

impl std::fmt::Display for OsrSlotExpectation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OsrSlotExpectation::Exact(t) => write!(f, "exactly {t}"),
            OsrSlotExpectation::Integral => f.write_str("a gpr-homed int/long/ref"),
            OsrSlotExpectation::FloatingPoint => f.write_str("an xmm-homed float/double"),
            OsrSlotExpectation::Unconstrained => f.write_str("anything (memory-homed)"),
            OsrSlotExpectation::NotSeeded => f.write_str("nothing (not seeded)"),
        }
    }
}

// ── Refusal taxonomy ─────────────────────────────────────────────────
//
// Each tag is the `&'static str` payload of a
// `bailout::BailoutReason::UnsupportedShape`, so every OSR refusal lands in the
// existing `unsupported_shape` bailout counter and prints through the existing
// `Bailout: Display`. The strings are an external contract (tests and log greps
// key on them) and must not be renamed with the code that raises them.

/// The artifact publishes no OSR entry table at all (`osr_pc_to_native` is
/// `None`) — it was never compiled for trampoline entry.
pub const OSR_REFUSE_NO_ENTRY_TABLE: &str = "osr-entry-no-table";
/// The table has no enterable native offset for this bci: past the end, or the
/// `-1` the codegen writes for a pc strictly inside a LICM-hoisted loop body
/// (whose preheader an entry here would skip).
pub const OSR_REFUSE_PC_NOT_AN_ENTRY: &str = "osr-entry-pc-not-an-entry";
/// A non-zero `osr_dead_mask` at this bci with `CRATONVM_JIT_OSR_DEAD_LOCALS=0`
/// (the kill switch that restores the historical blanket refusal).
pub const OSR_REFUSE_DEAD_LOCAL_MASK: &str = "osr-entry-dead-local-mask";
/// The interpreter's local/tag (or stack/tag) slices disagree in length, or the
/// local count does not match the compiled frame's `osr_num_locals`.
pub const OSR_REFUSE_LOCAL_COUNT: &str = "osr-entry-local-count";
/// The incoming operand stack is non-empty, or does not match the depth the
/// compiled entry expects. The trampoline seeds **locals only**, so a
/// non-empty stack would be silently dropped.
pub const OSR_REFUSE_OPERAND_STACK: &str = "osr-entry-operand-stack";
/// An incoming slot's JVM type is not one the compiled entry can accept there.
pub const OSR_REFUSE_SLOT_TYPE: &str = "osr-entry-slot-type-mismatch";
/// The compiled entry's own contract cannot describe a slot: `Unsupported`,
/// `MaterializationRequired`, or a scalar-replaced object.
pub const OSR_REFUSE_UNDESCRIBABLE_SLOT: &str = "osr-entry-undescribable-slot";
/// An incoming slot holds a `jsr` return address, which has no JIT
/// representation.
pub const OSR_REFUSE_RETURNADDRESS: &str = "osr-entry-returnaddress-slot";
/// The entry could land in — or exit from — an inlined scope that the deopt
/// metadata cannot describe (`FrameState::caller` is still `None` at every
/// producer).
pub const OSR_REFUSE_INLINED_SCOPE: &str = "osr-entry-inlined-scope";
/// The artifact contains an unconditional uncommon trap (`has_indy_trap`): it
/// bails on every execution reaching that site, so entering it buys nothing and
/// costs a bail whose resume may not be describable.
pub const OSR_REFUSE_UNCONDITIONAL_TRAP: &str = "osr-entry-unconditional-trap";
/// The artifact can take a frame-deopt exit whose reconstructed state is not
/// resumable. Entering would commit loop iterations that a bail could then only
/// discard — the replay this whole section exists to prevent.
pub const OSR_REFUSE_UNRESUMABLE_EXIT: &str = "osr-entry-unresumable-exit";
/// The artifact records more than one resume image at some bci, and they
/// disagree on what the by-bci resume lookups read (`semantics`, `reason`), so
/// the pick would be arbitrary — the lane's "what to refuse", applied where it
/// is free to apply it.
///
/// At ADMISSION, not at exit. Refusing at exit is not the mirror of refusing at
/// entry: by then the body has committed iterations, and the caller's only
/// remaining move is the safe reject, which re-runs every one of them. See
/// [`osr_exit`] for the argument in full.
pub const OSR_REFUSE_AMBIGUOUS_EXIT_IMAGE: &str = "osr-entry-ambiguous-exit-image";
/// Post-entry: the reconstructed frame cannot name an exact resume point, so
/// the interpreter must NOT be resumed (least of all at the entry bci).
pub const OSR_REFUSE_EXIT_REPLAY: &str = "osr-exit-replay-refused";
/// The artifact's **two views of its own frame** disagree about a slot: the
/// precise `FrameState` at the entry bci says one register file, the register
/// homes the trampoline actually seeds through say the other.
///
/// `osr-01` item 4 — "`osr_entry_frame_state` and the deopt frame state are two
/// views of the same thing and are not checked against each other". The two
/// views are produced by the same compile from the same allocator state, so
/// they cannot legitimately differ; when they do, the entry that
/// [`CompiledMethod::validate_osr_entry`] type-checks is not the entry
/// [`osr_trampoline`] performs. See [`CompiledMethod::osr_home_disagreement`].
pub const OSR_REFUSE_CONTRACT_DISAGREEMENT: &str = "osr-entry-contract-disagreement";

/// Every refusal tag, in taxonomy order. A new refusal must be added here; the
/// tests assert the list is complete and duplicate-free.
pub const OSR_REFUSAL_TAGS: [&str; 14] = [
    OSR_REFUSE_NO_ENTRY_TABLE,
    OSR_REFUSE_PC_NOT_AN_ENTRY,
    OSR_REFUSE_DEAD_LOCAL_MASK,
    OSR_REFUSE_LOCAL_COUNT,
    OSR_REFUSE_OPERAND_STACK,
    OSR_REFUSE_SLOT_TYPE,
    OSR_REFUSE_UNDESCRIBABLE_SLOT,
    OSR_REFUSE_RETURNADDRESS,
    OSR_REFUSE_INLINED_SCOPE,
    OSR_REFUSE_UNCONDITIONAL_TRAP,
    OSR_REFUSE_UNRESUMABLE_EXIT,
    OSR_REFUSE_AMBIGUOUS_EXIT_IMAGE,
    OSR_REFUSE_EXIT_REPLAY,
    OSR_REFUSE_CONTRACT_DISAGREEMENT,
];

/// Build (and count) an OSR refusal.
///
/// Counting here rather than at each call site is deliberate: a refusal that is
/// not counted is invisible to the compiler report the review asks for, and
/// there is exactly one constructor so none can be missed.
pub(crate) fn osr_refusal(tag: &'static str, context: impl Into<String>) -> bailout::Bailout {
    let b = bailout::Bailout::with_context(bailout::BailoutReason::UnsupportedShape(tag), context);
    bailout::record_bailout(&b);
    b
}

/// The subset of [`OSR_REFUSAL_TAGS`] whose answer is a pure function of the
/// *artifact*, and therefore reproduces for every future back-edge over the
/// same pc. See [`osr_refusal_is_permanent`].
pub const OSR_PERMANENT_REFUSAL_TAGS: [&str; 9] = [
    OSR_REFUSE_NO_ENTRY_TABLE,
    OSR_REFUSE_PC_NOT_AN_ENTRY,
    OSR_REFUSE_DEAD_LOCAL_MASK,
    OSR_REFUSE_UNDESCRIBABLE_SLOT,
    OSR_REFUSE_INLINED_SCOPE,
    OSR_REFUSE_UNCONDITIONAL_TRAP,
    OSR_REFUSE_UNRESUMABLE_EXIT,
    // A pure function of the artifact's own point list: the same two
    // disagreeing points are there on every future back edge over this pc.
    OSR_REFUSE_AMBIGUOUS_EXIT_IMAGE,
    // Artifact-level: both views come from this compile and neither depends on
    // the offered locals, so the answer reproduces for every future back-edge
    // over this pc. Memoing it is the difference between one wasted pipeline
    // and one per trip.
    OSR_REFUSE_CONTRACT_DISAGREEMENT,
];

/// Is this refusal a pure function of the *artifact* (as opposed to the
/// incoming interpreter state)?
///
/// A permanent refusal will reproduce for every future back-edge over the same
/// pc, so the caller should memo it through [`mark_osr_entry_rejected`] instead
/// of re-running the whole pipeline. A state-dependent one must not be memoed:
/// the next trip over the back-edge carries different locals.
pub fn osr_refusal_is_permanent(b: &bailout::Bailout) -> bool {
    match &b.reason {
        bailout::BailoutReason::UnsupportedShape(tag) => {
            OSR_PERMANENT_REFUSAL_TAGS.iter().any(|t| *t == *tag)
        }
        _ => false,
    }
}

/// The memoable refusals whose answer depends on the state the compile ran
/// under, rather than on the bytecode alone.
///
/// An unconditional trap, an unresumable exit, an ambiguous exit image and a
/// contract disagreement are all read off the artifact's deopt metadata, and
/// what metadata a compile records depends on what it saw: `CRATONVM_DEOPT_REAL`,
/// a class that has loaded since, a speculation the profile no longer supports.
/// Memoing them against the method for the life of the process refused OSR to a
/// recompile that would have produced a different artifact.
/// [`mark_osr_entry_rejected_by`] stamps these with the JIT install epoch, so
/// the memo expires when a redefinition or a code-cache flush moves it.
///
/// **`OSR_REFUSE_INLINED_SCOPE` and `OSR_REFUSE_UNDESCRIBABLE_SLOT` belong here
/// too (2026-09-21 round-10 `deopt2` sweep), and were missing.** Both refusals
/// are reachable ONLY through `self.deopt_points` / `osr_entry_frame_state`,
/// exactly like the four above:
///
///  * `OSR_REFUSE_INLINED_SCOPE`'s guard is `!self.inlined_methods.is_empty()
///    && !self.deopt_points.is_empty() && ...` — vacuously false whenever
///    `deopt_points` is empty, so it can only fire under the same
///    `deopt_real_enabled()`-gated, profile-shaped metadata as the other four.
///    Which methods got inlined, and whether their deopt points carry a caller
///    chain, is exactly "a speculation the profile no longer supports": a
///    later recompile with different call-site heat can inline a different
///    set of callees, or none, and stop carrying this artifact's problem.
///  * `OSR_REFUSE_UNDESCRIBABLE_SLOT` only fires when `osr_entry_frame_state`
///    returns `Some` — again conditioned on a recorded deopt point — and its
///    verdict (whether a slot's `FrameValue` names a describable JVM type) is
///    a function of THIS compile's register allocation and per-slot
///    classification, which a recompile with different speculation can change
///    independently of the bytecode.
///
/// Before this fix both tags were classified as artifact-pure-but-not-
/// install-epoch-stamped (`OSR_PERMANENT_REFUSAL_TAGS` without
/// `OSR_COMPILE_STATE_REFUSAL_TAGS`), so `mark_osr_entry_rejected_by` recorded
/// them with `install_epoch: None` — a memo that survives a code-cache flush
/// forever. A method rejected for `OSR_REFUSE_INLINED_SCOPE` under one
/// inlining decision would stay refused at that (method, pc) for the life of
/// the process even after a flush produced a fresh artifact that inlined
/// nothing and would have been admitted.
pub const OSR_COMPILE_STATE_REFUSAL_TAGS: [&str; 6] = [
    OSR_REFUSE_UNCONDITIONAL_TRAP,
    OSR_REFUSE_UNRESUMABLE_EXIT,
    OSR_REFUSE_AMBIGUOUS_EXIT_IMAGE,
    OSR_REFUSE_CONTRACT_DISAGREEMENT,
    OSR_REFUSE_INLINED_SCOPE,
    OSR_REFUSE_UNDESCRIBABLE_SLOT,
];

/// Whether a (memoable) refusal depends on compile-time state; see
/// [`OSR_COMPILE_STATE_REFUSAL_TAGS`].
pub fn osr_refusal_depends_on_compile_state(b: &bailout::Bailout) -> bool {
    match &b.reason {
        bailout::BailoutReason::UnsupportedShape(tag) => {
            OSR_COMPILE_STATE_REFUSAL_TAGS.iter().any(|t| *t == *tag)
        }
        _ => false,
    }
}

/// Where an [`OsrEntryPlan`]'s per-slot expectations came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsrContractSource {
    /// A `FrameState` recorded at the entry bci (an `OsrExit`-tagged deopt
    /// point, or any deopt point there): every slot's exact JVM type is known.
    PreciseFrameState,
    /// No deopt metadata at the entry bci — the production case. Expectations
    /// are inferred from `osr_local_assignments` / `osr_xmm_assignments`.
    RegisterHomes,
}

/// What a mid-loop bail out of the OSR'd body is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsrExitPolicy {
    /// Every frame-deopt exit this artifact can take reconstructs a resumable
    /// frame in this method's own scope, so a bail transfers the JIT-advanced
    /// loop state into the live frame and resumes at the bail's own bci.
    ExactTransfer,
    /// The artifact has no frame-deopt exits at all (the production case:
    /// `deopt_points` is empty unless `CRATONVM_DEOPT_REAL` was on at compile).
    /// Control can only leave the body by returning or by the exception routes,
    /// which propagate out of the frame. The interpreter must never "resume"
    /// this frame at the entry bci.
    PropagateOnly,
}

/// The interpreter state offered to an OSR entry.
///
/// `locals` are the raw 64-bit words in JVM local-slot order (`Frame::
/// get_local_raw`), `local_tags` the matching compact value tags (`Frame::
/// get_local_tag`, which exists precisely "for JIT/OSR interop"). The two
/// slices must be the same length.
///
/// `pc` is the interpreter's **current** pc. At a taken back-edge that is the
/// loop header with zero bytes of the new iteration executed, which is exactly
/// the invariant the exact-resume guarantee rests on — see
/// [`OsrEntryPlan::resume_bci`]. There is no separate `entry_pc` parameter to
/// disagree with it.
#[derive(Debug, Clone, Copy)]
pub struct OsrEntryState<'a> {
    /// The interpreter's current pc; both the entry bci and the resume bci.
    pub pc: usize,
    /// Raw local words, JVM-slot-indexed.
    pub locals: &'a [i64],
    /// Compact value tag per local slot (`cratonvm_types::VTAG_*`).
    pub local_tags: &'a [u8],
    /// Raw operand-stack words, bottom-first.
    pub stack: &'a [i64],
    /// Compact value tag per operand-stack entry.
    pub stack_tags: &'a [u8],
}

impl<'a> OsrEntryState<'a> {
    /// The common back-edge shape: a loop header with an empty operand stack.
    pub fn at(pc: usize, locals: &'a [i64], local_tags: &'a [u8]) -> Self {
        OsrEntryState {
            pc,
            locals,
            local_tags,
            stack: &[],
            stack_tags: &[],
        }
    }
}

/// A validated OSR entry: everything the transition needs, and nothing that
/// could still be refused.
///
/// Obtained only from [`CompiledMethod::validate_osr_entry`]. Holding one is
/// the proof that every incoming slot was type-checked against the compiled
/// entry's contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsrEntryPlan {
    /// The bci the compiled body starts executing at.
    pub entry_pc: usize,
    /// The bci the interpreter resumes at **if the entry never happens**.
    ///
    /// Equal to `entry_pc` by construction, and that equality is the exact-
    /// resume guarantee: `entry_pc` is `OsrEntryState::pc`, the pc at which the
    /// interpreter has executed nothing, so falling back to it repeats no work.
    /// After the transition this field is *not* a resume point — use
    /// [`resume_after_exit`](Self::resume_after_exit).
    pub resume_bci: usize,
    /// Offset of the entry point within the artifact's code buffer.
    pub native_offset: u32,
    /// The `osr_dead_mask` bits in force at `entry_pc`.
    pub dead_mask: u64,
    /// How the per-slot expectations were derived.
    pub contract: OsrContractSource,
    /// What a mid-loop bail may do.
    pub exit_policy: OsrExitPolicy,
    /// The validated per-local expectation, indexed by JVM local slot.
    pub expected_locals: Vec<OsrSlotExpectation>,
}

impl OsrEntryPlan {
    /// The exact bci to resume the (already mutated, already advanced)
    /// interpreter frame at after the OSR'd body bailed with `rframe`.
    ///
    /// **This is the only sanctioned resume point once compiled code has run.**
    /// It returns the reconstructed frame's own bci — never `entry_pc` unless
    /// the bail genuinely landed back on the loop header having completed a
    /// whole number of iterations, which is what a loop-boundary OSR-exit map
    /// means. An `Err` means "this frame cannot name where it is": the caller
    /// must propagate/unwind, and must **not** fall back to `entry_pc`, because
    /// every iteration the compiled body committed would then run a second
    /// time (the recorded `jit-osr-bail-reruns-loop-iterations` defect).
    ///
    /// The checks, in order:
    ///
    ///  * `caller_frames` non-empty → the bail is inside an inlined callee and
    ///    `FrameState::caller` cannot describe the outer scope. Refuse.
    ///  * held monitors → the in-place transfer has no re-lock path. Refuse.
    ///  * `bci` must be one this artifact actually recorded a deopt point for.
    ///    A bci from nowhere is a mis-routed stash, not a resume point.
    ///  * that point's [`deopt::ResumeSemantics`] must be `REEXECUTE`. See
    ///    below — this is the check that makes the returned bci *exact*.
    ///  * every operand-stack slot must be describable.
    ///  * every local must be describable, with one deliberate exception,
    ///    below.
    ///
    /// **The returned bci is always a re-execute point.** `ResumeSemantics`
    /// distinguishes three things a snapshot bci can mean, and only one of them
    /// is a bci the interpreter may be parked at:
    ///
    ///  * `REEXECUTE` — the bytecode at `bci` has not taken effect. Setting
    ///    `frame.pc = bci` runs it, which is exactly right. An `OsrExit` at a
    ///    loop header is this: the header iteration has not run.
    ///  * `RESUME` — the bytecode at `bci` has *already* taken effect and the
    ///    interpreter must continue *after* it. Handing that bci back would
    ///    re-execute it — the same double-execution this API exists to prevent,
    ///    one bytecode instead of one loop iteration. Computing the successor
    ///    bci needs the method's bytecode, which this crate does not have, so
    ///    such a point is refused at *admission* (`osr_exit_policy`) and again
    ///    here.
    ///  * `RETHROW` — not a resume point at all; the bci names a throwing
    ///    instruction to be routed through the exception table. Refused.
    ///
    /// **`Unsupported` vs `MaterializationRequired` in a local.** An
    /// `Unsupported` local is tolerated, and the live frame's current value is
    /// left in place. Refusing here is not the safe direction: it discards the
    /// exit's committed side effects and replays them from pre-entry state,
    /// which is the defect recorded in
    /// `jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`.
    ///
    /// The tolerance is sound because liveness is decided at COMPILE time,
    /// where the deopt point is published, and not here:
    ///
    ///  * a local that is not live-in at the point's bci is published
    ///    `Undefined`, whatever its whole-method kind
    ///    (`x64::deopt_stubs::build_frame_state_at`, handler-aware
    ///    `regalloc::live_locals_per_pc_all`);
    ///  * a live local is described through its per-bci kind where the
    ///    whole-method kind is `Ambiguous`, and published `Unsupported` only when
    ///    it cannot be;
    ///  * any `Unsupported` local in any deopt point makes
    ///    [`CompiledMethod::validate_osr_entry`] refuse the entry
    ///    (`osr_exit_policy`, `OSR_REFUSE_UNRESUMABLE_EXIT`) before the body
    ///    runs.
    ///
    /// So an admitted artifact never carries a live `Unsupported` local.
    /// `deopt::resolve_value` can still produce one at exit time from a
    /// metadata defect (a register index outside the spilled file), and that
    /// is left in place too rather than refused after side effects. See
    /// `jit-resume-tolerates-unsupported-locals-without-liveness-FIXED-20260912.md`.
    ///
    /// `MaterializationRequired` is **not** tolerated: it means a value that
    /// *was* live got deleted by an optimization with no rebuild recipe, so the
    /// live frame's stale pre-entry word is genuinely wrong, not merely
    /// unread. Guessing it is exactly what `FrameValue::MaterializationRequired`
    /// was introduced to make impossible.
    pub fn resume_after_exit(
        &self,
        artifact: &CompiledMethod,
        rframe: &deopt::ReconstructedFrame,
    ) -> Result<usize, bailout::Bailout> {
        use deopt::FrameValue as FV;
        if !rframe.caller_frames.is_empty() {
            return Err(osr_refusal(
                OSR_REFUSE_INLINED_SCOPE,
                format!(
                    "exit at bci {} carries {} inlined caller frame(s)",
                    rframe.bci,
                    rframe.caller_frames.len()
                ),
            ));
        }
        // Only a monitor the resume would have to ACQUIRE refuses: the in-place
        // transfer has no re-lock path. A lock the compiled code took itself
        // (`relock == false`) is still held by this thread and the live frame
        // releases it with its own `monitorexit`, exactly as before IR frame
        // states recorded monitors at all.
        if rframe.monitors.iter().any(|m| m.relock) {
            return Err(osr_refusal(
                OSR_REFUSE_EXIT_REPLAY,
                format!(
                    "exit at bci {} holds {} monitor(s) that must be re-acquired",
                    rframe.bci,
                    rframe.monitors.iter().filter(|m| m.relock).count()
                ),
            ));
        }
        // "More than one possible native image, or none" — both answered here,
        // by the same lookup, instead of by a `find` that silently takes the
        // first of however many there are.
        //
        // The ambiguous arm is an ASSERTION, not a new policy: `osr_exit_policy`
        // walked this artifact's whole point list at admission and refused the
        // entry (`osr-entry-ambiguous-exit-image`) if any bci named two
        // disagreeing images, so an admitted plan cannot reach it. It is here
        // because the alternative — reaching it and picking arbitrarily — is a
        // wrong-code bug, and because the check costs one scan of a list with
        // tens of entries on a path that has already taken a deopt.
        let point = match osr_exit::resume_image(&artifact.deopt_points, rframe.bci) {
            osr_exit::ResumeImage::Unique { index, .. } => &artifact.deopt_points[index],
            osr_exit::ResumeImage::None => {
                // Two different states share this arm, and saying so matters:
                // `resume_image` skips `rethrow_exception` points (they are not
                // resume images — see its doc), so a bci whose ONLY point is a
                // reason-9 exceptional frame answers `None` here. That is not a
                // mis-routed stash, it is a frame arriving at the wrong sink:
                // its bci names a THROWING instruction and it belongs to
                // `take_exceptional_frame`, not to a resume.
                let rethrow_here = artifact
                    .deopt_points
                    .iter()
                    .any(|p| p.bci == rframe.bci && p.semantics.rethrow_exception);
                return Err(osr_refusal(
                    OSR_REFUSE_EXIT_REPLAY,
                    if rethrow_here {
                        format!(
                            "exit bci {} names only a RETHROW point, which is not a resume \
                             point at all — it must be routed through the exception table \
                             (REEXECUTE semantics are required here)",
                            rframe.bci
                        )
                    } else {
                        format!(
                            "exit bci {} is not a recorded deopt point of this artifact",
                            rframe.bci
                        )
                    },
                ));
            }
            osr_exit::ResumeImage::Ambiguous { first, second } => {
                let (a, b) = (
                    &artifact.deopt_points[first],
                    &artifact.deopt_points[second],
                );
                return Err(osr_refusal(
                    OSR_REFUSE_EXIT_REPLAY,
                    format!(
                        "exit bci {} names two resume images that disagree: +{:#x} ({:?}, {}) \
                         vs +{:#x} ({:?}, {}) — admission should have refused this artifact",
                        rframe.bci,
                        a.native_offset,
                        a.reason,
                        a.semantics,
                        b.native_offset,
                        b.reason,
                        b.semantics
                    ),
                ));
            }
        };
        if point.semantics != deopt::ResumeSemantics::REEXECUTE {
            return Err(osr_refusal(
                OSR_REFUSE_EXIT_REPLAY,
                format!(
                    "exit at bci {} has {:?}: the bci is not one the interpreter may be \
                     parked at (only REEXECUTE points are exact resume points)",
                    rframe.bci, point.semantics
                ),
            ));
        }
        for (i, v) in rframe.stack.iter().enumerate() {
            if OsrSlotType::from_frame_value(v).is_none() {
                return Err(osr_refusal(
                    OSR_REFUSE_EXIT_REPLAY,
                    format!("exit at bci {}: stack slot {i} is {v:?}", rframe.bci),
                ));
            }
        }
        for (i, v) in rframe.locals.iter().enumerate() {
            match v {
                // See the doc comment: never a live slot of an admitted
                // artifact (dead slots are published `Undefined`, live
                // undescribable ones refuse the entry), so the live frame's
                // current value is left in place.
                FV::Unsupported => {}
                FV::MaterializationRequired(ev) => {
                    return Err(osr_refusal(
                        OSR_REFUSE_EXIT_REPLAY,
                        format!(
                            "exit at bci {}: local {i} needs materialization ({ev})",
                            rframe.bci
                        ),
                    ));
                }
                other => {
                    if OsrSlotType::from_frame_value(other).is_none() {
                        return Err(osr_refusal(
                            OSR_REFUSE_EXIT_REPLAY,
                            format!("exit at bci {}: local {i} is {other:?}", rframe.bci),
                        ));
                    }
                }
            }
        }
        Ok(rframe.bci as usize)
    }
}

impl CompiledMethod {
    /// Stamp `key` (`"<class>.<method>:<descriptor>"`) into every deopt
    /// snapshot this artifact publishes that does not already carry one.
    ///
    /// The optimizing IR lowerer cannot name the method it is compiling —
    /// `ir_lower::resolve_frame_state` records `method_key: String::new()` and
    /// its doc comment says the VM caller fills it in. Nothing ever did, and an
    /// identity-less snapshot is not merely untidy: `try_resume_trapped_callee`
    /// refuses it outright (*"no usable stash identity"*), so the `i64::MIN`
    /// deopt sentinel keeps travelling up through the compiled callers until
    /// some **unrelated** outer method's first-call tier-up sink consumes the
    /// foreign frame, fails its own `deopt_frame_matches_method` check and
    /// raises `precise deoptimization unavailable … refusing side-effecting
    /// replay` — naming a bci that does not exist in the method it blames. That
    /// is the `org/h2/mvstore/MVMap.evaluateMemoryForKey … at bci 123` failure
    /// (bci 123 is the `ldiv` in `org.h2.util.MemoryEstimator.estimateMemory`,
    /// which is 31 bytes long in `evaluateMemoryForKey`).
    ///
    /// Only empty keys are filled, so a backend that already stamps its own
    /// (the single-pass x64 emitter) and the per-scope keys an inlined
    /// `caller` chain carries are both left exactly as they were.
    ///
    /// Mutating the `String` inside a `Box<DeoptimizationPoint>` does not move
    /// the box, so the `imm64` payload addresses already baked into the emitted
    /// deopt stubs stay valid. Both lists are stamped: the boxes are what the
    /// stubs hand to `ir_deopt_entry` (IR tier; a single-pass artifact
    /// publishes no boxes and its stubs hand the `deopt_points` elements
    /// themselves to `x64_deopt_entry`), and the by-value `deopt_points` are what
    /// the VM's resume sinks search for the trap's reason.
    pub fn stamp_deopt_method_key(&mut self, key: &str) {
        if key.is_empty() {
            return;
        }
        for p in &mut self.deopt_points {
            if p.frame_state.method_key.is_empty() {
                p.frame_state.method_key.push_str(key);
            }
        }
        for p in &mut self._deopt_point_boxes {
            if p.frame_state.method_key.is_empty() {
                p.frame_state.method_key.push_str(key);
            }
        }
    }

    /// The precise entry contract at `entry_pc`, if this artifact has one.
    ///
    /// Prefers the `OsrExit`-tagged point (that IS the loop-boundary snapshot
    /// for this header) and falls back to any deopt point at the same bci. Both
    /// describe the same program point's live state, which is what makes the
    /// *exit* map usable as the *entry* contract — the symmetry the acceptance
    /// criterion ("survive forced deopt") is asking for.
    ///
    /// **A `RETHROW` point is not one of them and is skipped**, exactly as
    /// [`osr_exit::resume_image`] and [`osr_exit::deopt_reason_at_bci`] skip it,
    /// and for the same reason: its `bci` names a THROWING instruction, and its
    /// recorded operand stack is the *post-pop* state of the call that threw
    /// (`deopt::DeoptReason::PendingException`'s own doc says so). That is not a
    /// description of the frame an OSR entry would land in, so it cannot be this
    /// entry's contract.
    ///
    /// Reachable, not hypothetical. An OSR entry pc is any instruction boundary
    /// with an empty abstract stack (`x64::osr::osr_empty_stack_entry_enabled`),
    /// which is far more pcs than the loop headers `emit_osr_exit_map_at` covers
    /// — so a no-argument call inside a `try` block can be an entry pc whose
    /// ONLY deopt point is the RBC.6b `PendingException` frame. This used to
    /// hand that frame to the slot loop below as an `Exact` per-slot contract
    /// and to `osr_home_disagreement` as "the artifact's own view of its frame",
    /// and to report the resulting stack-depth disagreement as
    /// [`OSR_REFUSE_OPERAND_STACK`] — a tag that is deliberately NOT memoable,
    /// so the whole validation re-ran and re-refused on every trip over that pc.
    /// Skipping it falls back to [`OsrContractSource::RegisterHomes`], which is
    /// the production path and is what a bci with no snapshot has always used.
    fn osr_entry_frame_state(&self, entry_pc: usize) -> Option<&deopt::FrameState> {
        let bci = u32::try_from(entry_pc).ok()?;
        let describes_entry =
            |p: &&deopt::DeoptimizationPoint| p.bci == bci && !p.semantics.rethrow_exception;
        self.deopt_points
            .iter()
            .find(|p| describes_entry(p) && p.reason == deopt::DeoptReason::OsrExit)
            .or_else(|| self.deopt_points.iter().find(describes_entry))
            .map(|p| &p.frame_state)
    }

    /// Where an exit taken at `bci` landed, relative to the loop-boundary exit
    /// maps this artifact recorded.
    ///
    /// The lane's step 4: `osr_exit_points` is populated and nothing compared
    /// it with the exits that actually happen. This is that comparison, and it
    /// is a **classification, not a verdict** — see [`osr_exit::OsrExitSite`].
    /// The caller counts the answer; only `Unrecorded` is a defect.
    pub fn classify_osr_exit_site(&self, bci: u32) -> osr_exit::OsrExitSite {
        osr_exit::classify_exit_site(&self.osr_exit_points, &self.deopt_points, bci)
    }

    /// Classify what a mid-loop bail out of this artifact may do, refusing the
    /// entry outright when some reachable exit could not be resumed.
    ///
    /// This is the "make the re-entry point exact, or refuse" rule applied at
    /// admission time. Checking it *after* entering is useless: by then the
    /// compiled body has committed iterations, and the only remaining options
    /// are to replay them or to lose them.
    pub(crate) fn osr_exit_policy(&self) -> Result<OsrExitPolicy, bailout::Bailout> {
        // Memoised: the verdict is a pure function of `deopt_points`, which is
        // immutable once the artifact is published, and this is asked once per
        // OSR ENTRY rather than once per compile. See `osr_exit_policy_memo`.
        self.osr_exit_policy_memo
            .get_or_init(|| self.osr_exit_policy_uncached())
            .clone()
    }

    /// The real walk behind [`Self::osr_exit_policy`]'s memo.
    fn osr_exit_policy_uncached(&self) -> Result<OsrExitPolicy, bailout::Bailout> {
        if self.deopt_points.is_empty() {
            return Ok(OsrExitPolicy::PropagateOnly);
        }
        for p in &self.deopt_points {
            let fs = &p.frame_state;
            // A recorded caller chain is *describable* — the good case — but
            // the VM's in-place OSR-exit transfer is single-frame
            // (`transfer_osr_exit_into_live_frame` bails on "inlined caller
            // chain"), so it still cannot be resumed. Refuse at admission
            // rather than after committing iterations. Lift this the same day
            // that transfer grows a multi-frame path.
            // An inlined caller scope used to refuse outright, because the
            // VM's in-place OSR-exit transfer was single-frame. It is not any
            // more (2026-08-18): `transfer_osr_exit_chain_into_live_frame`
            // writes the outermost scope into the live frame and PUSHES the
            // rest, so a chain is resumable up to the budget both sides share.
            //
            // The budget is still a refusal, and it is deliberately the VM's:
            // admitting a chain the transfer would decline spends a whole OSR
            // entry to reach a safe reject. `MAX_OSR_INLINE_RESUME_DEPTH` is
            // defined once and consumed by both.
            //
            // Everything else about a chain is already covered without a
            // special case: `first_unresumable_slot` below walks EVERY scope,
            // so an undescribable caller slot refuses exactly as an
            // undescribable innermost one does.
            let chain_depth = deopt::caller_chain_depth(fs);
            if chain_depth > deopt::MAX_OSR_INLINE_RESUME_DEPTH {
                return Err(osr_refusal(
                    OSR_REFUSE_INLINED_SCOPE,
                    format!(
                        "deopt point at bci {} carries {chain_depth} inlined caller scope(s), \
                         past the {}-frame budget the VM's OSR-exit transfer materialises \
                         atomically",
                        p.bci,
                        deopt::MAX_OSR_INLINE_RESUME_DEPTH
                    ),
                ));
            }
            // A RETHROW point naming a scalar-replaced object, and why this is
            // narrower than the `first_unresumable_local` veto below.
            //
            // `VirtualObject` is deliberately NOT "unresumable":
            // `build_deopt_frame_inner` materializes one, so an ordinary guard
            // deopt on such a frame resumes fine and refusing it here would cost
            // every scalar-replacing method its OSR entry for nothing.
            //
            // The OSR-EXCEPTION exit is the exception to that, in both senses.
            // `deopt_resume::transfer_osr_exception_exit_into_live_frame`
            // rewrites a LIVE frame in place and has no materialization path, so
            // it answers `Err("virtual-object local")` — and
            // `jit_bridge::route_osr_exception_out_of_artifact` turns that into
            // `OsrExceptionExit::Propagate`. Propagating is fail-closed against
            // resuming on locals nobody could map, but it is still an exception
            // escaping a handler that matched it, which is a wrong answer rather
            // than a slow one. That function's own comment calls the case
            // "unreachable by admission" and points here; this is the admission
            // that makes the claim true.
            //
            // Vacuous before 2026-09-22 — `x64/driver.rs` refused to scalar-
            // replace under `precise_exception_frames`, and a rethrow point only
            // exists when they are on, so no artifact could reach it. Written
            // out because that is no longer so.
            if p.semantics.rethrow_exception {
                if let Some((i, v)) = fs.locals.iter().enumerate().find(|(_, v)| {
                    matches!(
                        v,
                        deopt::FrameValue::VirtualObject(_)
                            | deopt::FrameValue::VirtualObjectRef(_)
                    )
                }) {
                    return Err(osr_refusal(
                        OSR_REFUSE_UNRESUMABLE_EXIT,
                        format!(
                            "rethrow point at bci {} names a scalar-replaced object in local \
                             {i} ({v:?}); the in-place OSR-exception transfer cannot \
                             materialize one and its refusal propagates",
                            p.bci
                        ),
                    ));
                }
            }
            // See `resume_after_exit`: only an elided (`relock`) monitor has no
            // path through the in-place transfer.
            if fs.monitors.iter().any(|m| m.relock) {
                return Err(osr_refusal(
                    OSR_REFUSE_UNRESUMABLE_EXIT,
                    format!(
                        "deopt point at bci {} holds monitors that must be re-acquired",
                        p.bci
                    ),
                ));
            }
            // A RETHROW point is asked a NARROWER question, because it is
            // consumed differently: `take_exceptional_frame` hands its `bci`
            // and `locals` to the handler-frame builder, which then pushes
            // `[exception]` as the operand stack by JVMS §2.10. The recorded
            // stack is never read, so an entry in it that could not be typed
            // describes nothing that will be reconstructed — and this veto is
            // ARTIFACT-WIDE, so one such entry at one throwing bci costs the
            // whole method its OSR entry. See `first_unresumable_local`.
            let unresumable = if p.semantics.rethrow_exception {
                deopt::first_unresumable_local(fs)
            } else {
                deopt::first_unresumable_slot(fs)
            };
            if let Some(slot) = unresumable {
                return Err(osr_refusal(
                    OSR_REFUSE_UNRESUMABLE_EXIT,
                    format!(
                        "deopt point at bci {} ({:?}) reconstructs an unresumable frame: {slot}",
                        p.bci, p.reason
                    ),
                ));
            }
            // `RESUME` semantics mean "the bytecode at this bci already took
            // effect; continue AFTER it". Parking the interpreter at that bci
            // would re-execute it, and computing the successor bci needs the
            // method's bytecode, which this crate does not have. `RETHROW`
            // points are fine to *have* — they are stashed separately
            // (`take_exceptional_frame`) and never routed to a resume — so only
            // the plain `RESUME` shape disqualifies the entry.
            if !p.semantics.reexecute && !p.semantics.rethrow_exception {
                return Err(osr_refusal(
                    OSR_REFUSE_UNRESUMABLE_EXIT,
                    format!(
                        "deopt point at bci {} ({:?}) has RESUME semantics: its successor \
                         bci is not computable here",
                        p.bci, p.reason
                    ),
                ));
            }
        }
        // The lane's "what to refuse", at the only moment refusing is free.
        //
        // `resume_after_exit` finds its point BY BCI and takes the first match,
        // reading its `semantics` — the field that decides whether the bci is a
        // place the interpreter may be parked at. Two points at one bci that
        // disagree there make the resume bci itself arbitrary, and "picking the
        // wrong image is a wrong-code bug rather than a missed optimisation".
        //
        // Two things are deliberately NOT refused here, both because refusing
        // them costs OSR and buys nothing:
        //
        //  * Copies that agree. Several native images of one bytecode is
        //    exactly what a loop transform produces, and every consumer that
        //    RECONSTRUCTS a frame finds its point by native offset or through
        //    the copy's own baked box.
        //  * Images that differ only in `reason`. That is the ordinary shape of
        //    a compiled counted loop — the loop-boundary exit map and the
        //    speculative-BCE range guard on the same header bci — and it was
        //    measured: refusing it took 10 of CratonBench's 11 OSR refusals and
        //    cost `matrixKernel` its OSR permanently. It is counted instead;
        //    `osr_exit`'s module note carries the full argument.
        if osr_exit::has_reason_ambiguous_bci(&self.deopt_points) {
            metrics::record_osr_event("osr_entry_reason_ambiguous_image");
        }
        if let Some((bci, i, j)) = osr_exit::first_ambiguous_resume_bci(&self.deopt_points) {
            metrics::record_osr_event("osr_entry_refused_ambiguous_image");
            let (a, b) = (&self.deopt_points[i], &self.deopt_points[j]);
            return Err(osr_refusal(
                OSR_REFUSE_AMBIGUOUS_EXIT_IMAGE,
                format!(
                    "bci {bci} names two resume images whose ResumeSemantics disagree, so the \
                     resume bci itself is arbitrary: +{:#x} ({:?}, {}) vs +{:#x} ({:?}, {})",
                    a.native_offset, a.reason, a.semantics, b.native_offset, b.reason, b.semantics
                ),
            ));
        }
        Ok(OsrExitPolicy::ExactTransfer)
    }

    /// The register-home-inferred expectation for local `i`, used when the
    /// artifact carries no precise `FrameState` at the entry bci.
    fn osr_inferred_local_expectation(&self, i: usize, dead_mask: u64) -> OsrSlotExpectation {
        if i < 64 && (dead_mask >> i) & 1 == 1 {
            return OsrSlotExpectation::NotSeeded;
        }
        if self
            .osr_xmm_assignments
            .as_ref()
            .and_then(|m| m.get(i))
            .is_some_and(|a| a.is_some())
        {
            return OsrSlotExpectation::FloatingPoint;
        }
        if self
            .osr_local_assignments
            .as_ref()
            .and_then(|m| m.get(i))
            .is_some_and(|a| a.is_some())
        {
            return OsrSlotExpectation::Integral;
        }
        OsrSlotExpectation::Unconstrained
    }

    /// Does the register file a local's homes imply admit `want`?
    ///
    /// No home at all means memory-homed: the trampoline stores the word to the
    /// frame slot and every type is admissible.
    ///
    /// # A slot has at most ONE home, and this function does not rely on it
    ///
    /// This doc used to claim the opposite — that a slot reused across disjoint
    /// live ranges (`java.util.DualPivotQuicksort.mixedInsertionSort` has slot 7
    /// as a `long`'s high half in one region and as an `int` loop counter in the
    /// others) "can legitimately carry BOTH a GPR and an XMM home", and that
    /// [`osr_trampoline`] seeds both. The second half is true; the first is not.
    /// `regalloc::regalloc_invariants_hold`'s first invariant is exactly
    /// "GPR × XMM non-overlap", and a violation discards the whole allocation
    /// (every local memory-homed) rather than being published — and
    /// `allocate_registers_with` additionally withholds the XMM home from any
    /// float-masked slot javac also uses as an int/long/ref (`dual_category_mask`),
    /// which is that very slot-7 shape. The OSR copies only ever NULL entries
    /// (the pure-high-half strip in `x64::osr::publish_entry_metadata`), so they
    /// cannot introduce a second home either.
    ///
    /// It matters because the sibling
    /// [`Self::osr_inferred_local_expectation`] answers `FloatingPoint` for a
    /// both-homed slot — which would refuse an incoming `int` outright — and a
    /// reader comparing the two would have to conclude one of them is wrong.
    /// Neither is: the input the disagreement needs cannot be produced.
    ///
    /// The `match` below is still written as a *set* rather than a single
    /// answer, deliberately. It is the artifact-level cross-check
    /// ([`Self::osr_home_disagreement`]) and its whole job is to be correct
    /// about metadata that has gone wrong somewhere else; encoding
    /// "at most one home" as an assumption here would make the one shape it is
    /// supposed to catch answer `true` by construction.
    fn osr_homes_admit(&self, i: usize, want: OsrSlotType) -> bool {
        let has = |m: &Option<Vec<Option<u8>>>| {
            m.as_ref()
                .and_then(|v| v.get(i))
                .is_some_and(|a| a.is_some())
        };
        let gpr = has(&self.osr_local_assignments);
        let xmm = has(&self.osr_xmm_assignments);
        if !gpr && !xmm {
            return true;
        }
        match want {
            OsrSlotType::Float | OsrSlotType::Double => xmm,
            OsrSlotType::Int | OsrSlotType::Long | OsrSlotType::Ref => gpr,
            // The precise contract says nothing lives here, so it constrains
            // nothing. (`Exact(Top)` accepts any incoming value too — see
            // `OsrSlotExpectation::accepts`.)
            OsrSlotType::Top => true,
        }
    }

    /// `osr-01` item 4: check this artifact's **two views of its own frame**
    /// against each other, and name the first slot where they disagree.
    ///
    /// The two views are:
    ///
    /// * the precise `FrameState` at the entry bci — what
    ///   [`CompiledMethod::validate_osr_entry`] type-checks the interpreter's
    ///   offer against; and
    /// * `osr_local_assignments` / `osr_xmm_assignments` — the register homes
    ///   [`osr_trampoline`] actually seeds through.
    ///
    /// They are produced by one compile from one allocator state, so they
    /// cannot legitimately differ. When they do, the entry that was *validated*
    /// is not the entry that is *performed*: a `double` whose only home is a
    /// GPR has its bits moved into a register the body reads as an integer, and
    /// — worse — a reference whose only home is an XMM register is seeded into
    /// the FP file with the frame-slot store elided, so the GC cannot see it
    /// and the body reads a stale word.
    ///
    /// Deliberately narrow. It does **not** assert that a slot the frame state
    /// calls live has a register home (memory-homed locals are ordinary), nor
    /// that a slot with a home is described by the frame state (a snapshot
    /// shorter than the compiled frame simply does not describe its tail), nor
    /// anything about the dead mask (see `osr_contract`'s note on the
    /// invariant-that-is-not-one). Only the *register file* is cross-checked,
    /// because that is the only place the two views make a claim that can
    /// contradict — and it is the claim the trampoline acts on.
    ///
    /// Masked-dead slots are skipped: the trampoline does not seed them at all,
    /// so their homes describe nothing that happens at this entry.
    fn osr_home_disagreement(
        &self,
        frame_state: &deopt::FrameState,
        dead_mask: u64,
    ) -> Option<(usize, OsrSlotType)> {
        for i in 0..self.osr_num_locals {
            if i < 64 && (dead_mask >> i) & 1 == 1 {
                continue;
            }
            let Some(v) = frame_state.locals.get(i) else {
                break; // the snapshot describes a prefix; the tail is untyped
            };
            // An undescribable slot is `validate_osr_entry`'s refusal, not
            // this one's — reporting it here would blame the wrong thing.
            let Some(want) = OsrSlotType::from_frame_value(v) else {
                continue;
            };
            if !self.osr_homes_admit(i, want) {
                return Some((i, want));
            }
        }
        None
    }

    /// Type-check an offered interpreter state against this artifact's OSR
    /// entry contract at `state.pc`, producing an [`OsrEntryPlan`] or a
    /// structured refusal.
    ///
    /// Pure: it inspects metadata and the caller's slices only, allocates one
    /// `Vec` for the plan, and never enters compiled code. That is what makes
    /// a refusal free of side effects — the interpreter continues at
    /// `state.pc` having executed nothing, so nothing can be replayed.
    ///
    /// Refusals are ordered cheapest-and-most-permanent first, so the memoable
    /// artifact-level answers ([`osr_refusal_is_permanent`]) are reached before
    /// the per-entry state checks.
    #[must_use = "an OSR entry must not be taken without its validated plan"]
    pub fn validate_osr_entry(
        &self,
        state: &OsrEntryState<'_>,
    ) -> Result<OsrEntryPlan, bailout::Bailout> {
        let entry_pc = state.pc;
        let label = || {
            if self.method_label.is_empty() {
                format!("bci {entry_pc}")
            } else {
                format!("{} bci {entry_pc}", self.method_label)
            }
        };

        // ── 1. Is there an entry here at all? ──────────────────────
        let Some(table) = self.osr_pc_to_native.as_ref() else {
            return Err(osr_refusal(OSR_REFUSE_NO_ENTRY_TABLE, label()));
        };
        let native_offset = match table.get(entry_pc) {
            Some(&off) if off >= 0 => off as u32,
            Some(_) => {
                return Err(osr_refusal(
                    OSR_REFUSE_PC_NOT_AN_ENTRY,
                    format!("{}: native offset is -1", label()),
                ))
            }
            None => {
                return Err(osr_refusal(
                    OSR_REFUSE_PC_NOT_AN_ENTRY,
                    format!("{}: past the end of a {}-entry table", label(), table.len()),
                ))
            }
        };

        // ── 2. Coalesced-register hazard (and its kill switch) ─────
        let dead_mask = self
            .osr_dead_mask
            .as_ref()
            .and_then(|m| m.get(entry_pc).copied())
            .unwrap_or(0);
        if dead_mask != 0 && !osr_dead_local_entry_allowed() {
            return Err(osr_refusal(
                OSR_REFUSE_DEAD_LOCAL_MASK,
                format!(
                    "{}: mask {dead_mask:#x} with CRATONVM_JIT_OSR_DEAD_LOCALS=0",
                    label()
                ),
            ));
        }

        // ── 3. Artifact-level disqualifications ────────────────────
        if self.has_indy_trap {
            return Err(osr_refusal(
                OSR_REFUSE_UNCONDITIONAL_TRAP,
                format!(
                    "{}: artifact carries an unconditional uncommon trap",
                    label()
                ),
            ));
        }
        // An artifact that inlined something and can also take a frame-deopt
        // exit must be able to say which scope an exit belongs to. When no
        // deopt point carries a caller chain, it cannot: an exit inside an
        // inlined callee arrives under the OUTER method's key at the CALLEE's
        // bci, indistinguishable from an outer-scope exit, and resuming it
        // lands the interpreter at a bci that means something else entirely.
        //
        // (Entry itself is safe — `osr_pc_to_native` is indexed by the outer
        // method's code array, so an entry pc is always an outer-scope block
        // start. The hazard is the exit.)
        //
        // Written as "inlined AND no scope recorded anywhere" rather than
        // "inlined AND has deopt points" so it relaxes on its own as producers
        // start populating `FrameState::caller` (they do not yet — see
        // `docs/jit/deopt-inline-scopes.md`). A chain that IS recorded is
        // handled by `osr_exit_policy` below, which refuses it for a different
        // and narrower reason: the VM's in-place transfer has no multi-frame
        // path.
        if !self.inlined_methods.is_empty()
            && !self.deopt_points.is_empty()
            && !self
                .deopt_points
                .iter()
                .any(|p| p.frame_state.caller.is_some())
        {
            return Err(osr_refusal(
                OSR_REFUSE_INLINED_SCOPE,
                format!(
                    "{}: {} inlined method(s) with {} deopt point(s) and no caller chain",
                    label(),
                    self.inlined_methods.len(),
                    self.deopt_points.len()
                ),
            ));
        }

        // ── 4. Shape of the offered state ──────────────────────────
        if state.locals.len() != state.local_tags.len() {
            return Err(osr_refusal(
                OSR_REFUSE_LOCAL_COUNT,
                format!(
                    "{}: {} local words but {} tags",
                    label(),
                    state.locals.len(),
                    state.local_tags.len()
                ),
            ));
        }
        if state.stack.len() != state.stack_tags.len() {
            return Err(osr_refusal(
                OSR_REFUSE_OPERAND_STACK,
                format!(
                    "{}: {} stack words but {} tags",
                    label(),
                    state.stack.len(),
                    state.stack_tags.len()
                ),
            ));
        }
        if state.locals.len() != self.osr_num_locals {
            return Err(osr_refusal(
                OSR_REFUSE_LOCAL_COUNT,
                format!(
                    "{}: interpreter offers {} locals, compiled frame has {}",
                    label(),
                    state.locals.len(),
                    self.osr_num_locals
                ),
            ));
        }

        // ── 5. Build the contract, then check every slot ───────────
        let frame_state = self.osr_entry_frame_state(entry_pc);
        if let Some(fs) = frame_state {
            if fs.caller.is_some() {
                return Err(osr_refusal(
                    OSR_REFUSE_INLINED_SCOPE,
                    format!("{}: entry contract has an inlined caller scope", label()),
                ));
            }
            // A lock the compiled body takes itself is taken by the
            // interpreter before the entry and released by the body's own
            // `monitorexit`. An ELIDED one (`relock`) would be held by the
            // interpreter and released by nobody: the body has no monitor op
            // for it. Refuse only that.
            if fs.monitors.iter().any(|m| m.relock) {
                return Err(osr_refusal(
                    OSR_REFUSE_UNRESUMABLE_EXIT,
                    format!("{}: entry contract holds an elided monitor", label()),
                ));
            }
            // `osr-01` item 4. The precise contract is what the loop below
            // type-checks the interpreter's offer against; the register homes
            // are what the trampoline seeds through. Both come from this one
            // compile, so a disagreement is a compiler bug — and it is the kind
            // that validates one entry and performs another. Checked BEFORE the
            // per-slot loop so a disagreement is reported as itself rather than
            // surfacing later as a slot-type mismatch against the offer, which
            // would name the interpreter for the compiler's error.
            if let Some((i, want)) = self.osr_home_disagreement(fs, dead_mask) {
                return Err(osr_refusal(
                    OSR_REFUSE_CONTRACT_DISAGREEMENT,
                    format!(
                        "{}: the entry contract says local {i} is {want}, but its register \
                         homes are gpr={:?} xmm={:?} — the trampoline seeds through the \
                         homes, so the validated entry is not the entry performed",
                        label(),
                        self.osr_local_assignments
                            .as_ref()
                            .and_then(|m| m.get(i).copied())
                            .flatten(),
                        self.osr_xmm_assignments
                            .as_ref()
                            .and_then(|m| m.get(i).copied())
                            .flatten(),
                    ),
                ));
            }
        }
        let contract = match frame_state {
            Some(_) => OsrContractSource::PreciseFrameState,
            None => OsrContractSource::RegisterHomes,
        };

        let mut expected_locals = Vec::with_capacity(self.osr_num_locals);
        for i in 0..self.osr_num_locals {
            let expectation = if i < 64 && (dead_mask >> i) & 1 == 1 {
                // The trampoline skips this slot entirely; nothing to check.
                OsrSlotExpectation::NotSeeded
            } else {
                match frame_state.and_then(|fs| fs.locals.get(i)) {
                    Some(v) => match OsrSlotType::from_frame_value(v) {
                        Some(t) => OsrSlotExpectation::Exact(t),
                        None => {
                            return Err(osr_refusal(
                                OSR_REFUSE_UNDESCRIBABLE_SLOT,
                                format!("{}: local {i} is {v:?}", label()),
                            ))
                        }
                    },
                    // Either there is no precise contract (production), or the
                    // snapshot is shorter than the compiled frame — in which
                    // case the trailing slots are simply not described.
                    None => self.osr_inferred_local_expectation(i, dead_mask),
                }
            };
            let tag = state.local_tags[i];
            let Some(got) = OsrSlotType::from_vtag(tag) else {
                return Err(osr_refusal(
                    OSR_REFUSE_RETURNADDRESS,
                    format!("{}: local {i} carries vtag {tag}", label()),
                ));
            };
            if !expectation.accepts(got) {
                return Err(osr_refusal(
                    OSR_REFUSE_SLOT_TYPE,
                    format!(
                        "{}: local {i} — compiled entry expects {expectation}, interpreter has {got}",
                        label()
                    ),
                ));
            }
            expected_locals.push(expectation);
        }

        // ── 6. Operand stack ───────────────────────────────────────
        // Type-check the described overlap first, so a genuine type error is
        // reported as one rather than as the blanket "stack not seeded" below.
        let expected_stack = frame_state.map(|fs| fs.stack.as_slice()).unwrap_or(&[]);
        for (i, want) in expected_stack.iter().enumerate() {
            let Some(&tag) = state.stack_tags.get(i) else {
                break;
            };
            let Some(want) = OsrSlotType::from_frame_value(want) else {
                return Err(osr_refusal(
                    OSR_REFUSE_UNDESCRIBABLE_SLOT,
                    format!("{}: stack slot {i} is {:?}", label(), expected_stack[i]),
                ));
            };
            let Some(got) = OsrSlotType::from_vtag(tag) else {
                return Err(osr_refusal(
                    OSR_REFUSE_RETURNADDRESS,
                    format!("{}: stack slot {i} carries vtag {tag}", label()),
                ));
            };
            if want != got && want != OsrSlotType::Top {
                return Err(osr_refusal(
                    OSR_REFUSE_SLOT_TYPE,
                    format!(
                        "{}: stack slot {i} — compiled entry expects {want}, interpreter has {got}",
                        label()
                    ),
                ));
            }
        }
        if state.stack.len() != expected_stack.len() {
            return Err(osr_refusal(
                OSR_REFUSE_OPERAND_STACK,
                format!(
                    "{}: interpreter offers {} operand(s), entry contract describes {}",
                    label(),
                    state.stack.len(),
                    expected_stack.len()
                ),
            ));
        }
        // The trampoline (`osr_trampoline`) seeds locals into their register /
        // frame homes and nothing else — it has no operand-stack seeding path.
        // A non-empty stack would therefore be silently dropped. Back-edge
        // entries are the empty-stack case by construction (the branch already
        // consumed its operands), so this costs nothing today and fails closed
        // if a future trigger fires somewhere else.
        if !state.stack.is_empty() {
            return Err(osr_refusal(
                OSR_REFUSE_OPERAND_STACK,
                format!(
                    "{}: {} operand(s) live; the OSR trampoline seeds locals only",
                    label(),
                    state.stack.len()
                ),
            ));
        }

        // ── 7. Can every exit this body may take name its own bci? ─
        // Deliberately last of the artifact-level checks: an undescribable slot
        // in the *entry* contract also makes that point's frame unresumable, and
        // the per-slot refusal above names the offending slot, which is strictly
        // more actionable than the whole-artifact answer here. Reaching this
        // means the entry contract itself is clean and some *other* deopt point
        // is the problem.
        let exit_policy = self.osr_exit_policy()?;

        Ok(OsrEntryPlan {
            entry_pc,
            resume_bci: entry_pc,
            native_offset,
            dead_mask,
            contract,
            exit_policy,
            expected_locals,
        })
    }

    /// Take a validated OSR entry.
    ///
    /// A thin, deliberate wrapper over the existing [`osr_enter`](Self::
    /// osr_enter): the plan carries the proof that every incoming slot was
    /// type-checked, and this is the only place that proof is spent. Returns
    /// what `osr_enter` returns (`None` when the machine-level entry itself
    /// declined; `Some(word)` — possibly the `i64::MIN` deopt sentinel — when
    /// the body ran).
    ///
    /// # Safety
    /// Same requirements as [`osr_enter`](Self::osr_enter), and additionally
    /// `plan` must have come from [`validate_osr_entry`](Self::
    /// validate_osr_entry) on **this** artifact with **this** `state`.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    pub unsafe fn osr_enter_planned(
        &self,
        vm_ptr: i64,
        state: &OsrEntryState<'_>,
        plan: &OsrEntryPlan,
        thread_ptr: i64,
    ) -> Option<i64> {
        self.osr_enter(vm_ptr, state.locals, plan.entry_pc, thread_ptr)
    }
}

// ---------------------------------------------------------------------------
// OSR (On-Stack Replacement) trampoline
// ---------------------------------------------------------------------------

/// Whether OSR may enter at a pc whose `osr_dead_mask` is non-zero, relying on
/// the trampoline to skip seeding the masked locals.
///
/// **DEFAULT ON since 2026-07-31.** **Kill switch:
/// `CRATONVM_JIT_OSR_DEAD_LOCALS=0`** (also `off` / `false` / `no`) restores
/// the historical blanket refusal with no rebuild. If a run turns up a wrong
/// result, a spurious NPE/SIGSEGV or a mis-sorted array, set that and re-run
/// before doing anything else — it is the fastest attribution test for this
/// change and separates it cleanly from everything else in the same binary.
///
/// ## What the mask means
///
/// `osr_dead_mask[pc]` bit `i` is set iff, at the block-start pc `pc`, local
/// `i` is **dead** (not in that block's `live_in`), is **register-resident**,
/// and its home register is **also the home of a local that IS live there**.
/// Graph-colouring coalesced the two once local `i`'s range ended. Loading `i`
/// at entry would drop the interpreter's stale value on top of the live
/// owner's register.
///
/// ## Why skipping the load is sufficient
///
/// The historical refusal (`3415d052b`, 2026-07-03) called the skip "a
/// coalesced state transition that was not proven safe". It is provable, from
/// the allocator's own invariant:
///
/// 1. **A local has exactly one home for the whole method.** `local_assignments`
///    is a `Vec<Option<u8>>` indexed by local; `x64::Compiler::reg_for_local`
///    is the single reader and there is no live-range splitting, so there is no
///    "at this pc register R belongs to someone else" state to express. The
///    OSR copy only nulls entries (category-2 high halves), never re-points
///    them.
/// 2. **Two locals sharing a register are never both live-in at the same
///    block start.** `regalloc::build_interference` unions, for every block,
///    `live_in` against itself — every pair simultaneously live-in at a block
///    boundary is marked interfering — and `regalloc_invariants_hold` fails the
///    whole allocation (falling back to no register homes) if any interfering
///    pair got the same colour. `RegAllocResult::block_live_in`, which the mask
///    is computed from, is *the same* `blocks[i].live_in`.
/// 3. Therefore at an OSR entry pc, of the locals homed to register `R`, **at
///    most one is live**. The trampoline loads every non-masked local, which is
///    exactly that one, so `R` ends up holding its correct value.
/// 4. A masked local needs no value: dead means every path from `pc` redefines
///    it before reading it. Its frame slot is not written either — but the
///    trampoline already elides the frame-slot store for *every* register-homed
///    local (dead or live), so that is not a new hole.
///
/// The refusal was introduced alongside the fix that actually closed the
/// Hibernate regression it cites: the same commit threaded
/// `compute_param_jvm_slots` / `param_slot_span` into the OSR compile so
/// category-2 parameters (the `long limitRows` in that very
/// `org.h2.command.query.Select.queryFlat` frame) land in the slots the body
/// reads. That mismatch, not the dead-local skip, is what produced the NPE.
///
/// ## What this unblocks
///
/// The refusal is the second of the two gates behind Tomcat known-issue 30:
/// once RBC.7 stopped refusing `invokedynamic` methods outright, every
/// `loop … then System.out.println("…" + x)` shape compiled its OSR body
/// successfully and was then turned away at the door, because the harness
/// method's own parameter (`String[] args`, local 0) is dead at the loop head.
/// A once-invoked method with its hot loop inline has no other route into
/// compiled code.
/// `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS` — make the OSR trampoline write every
/// seeded local to its frame slot as well as its register home, instead of
/// eliding the store for register-resident locals.
///
/// Diagnosis lever for the `mixedInsertionSort` OSR miscompile
/// (`docs/known-issues/jit/arrays-sort-long-osr-miscompile-20260803.md`). The
/// elision assumes the compiled body re-establishes a local's frame slot
/// before any operation that needs a memory operand; if that is not true on
/// every path reachable from an OSR entry, the slot holds whatever the
/// trampoline's own frame left there.
fn osr_always_seed_frame_slot() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_SEED_FRAME_SLOTS") {
            Ok(v) => matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes"
            ),
            Err(_) => false,
        }
    })
}

/// `CRATONVM_JIT_OSR_SINGLE_PC` — see [`CompiledMethod::osr_compiled_entry_pc`].
pub(crate) fn osr_single_pc_entry_only() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_SINGLE_PC") {
            Ok(v) => matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes"
            ),
            Err(_) => false,
        },
    )
}

pub(crate) fn osr_dead_local_entry_allowed() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_DEAD_LOCALS") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            ),
            // Default: the trampoline's skip-the-load handles these entries.
            Err(_) => true,
        },
    )
}

/// Emitted OSR trampolines, keyed by `(target_addr, dead_mask)`.
///
/// NOT by `target_addr` alone: the body bakes in the per-pc dead-local skip
/// set, and two OSR pcs share a native target whenever the instruction between
/// them emits no bytes. A cache hit for the second then reused the first pc's
/// skip set — skipping a live local, or seeding a dead one over a coalesced
/// live register.
///
/// Runtime values (`vm_ptr`, `locals_ptr`) arrive in argument registers, so one
/// trampoline serves every entry at its key. Each buffer sits behind an `Arc`,
/// so an entry already under way keeps it mapped, and `CompiledMethod`'s `Drop`
/// purges every key whose target lies inside the retiring body.
#[allow(clippy::type_complexity)]
pub(crate) fn osr_trampoline_cache(
) -> &'static parking_lot::Mutex<FxHashMap<(usize, u64), Arc<ExecutableBuffer>>> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<FxHashMap<(usize, u64), Arc<ExecutableBuffer>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Emit a fresh OSR trampoline body for the given destination + frame layout.
///
/// The emitted code expects three arguments via the platform C ABI:
///   * arg0 (RCX on Windows / RDI on SysV) = `locals_ptr: *const i64`
///   * arg1 (RDX on Windows / RSI on SysV) = `vm_ptr: i64` (only read when `needs_context`)
///   * arg2 (R8 on Windows / RDX on SysV) = `thread_ptr: i64` (read when shadow
///     slots exist; tests may pass 0, which disables tracking via null guards)
///
/// It saves callee-saved registers used for locals, optionally stores `vm_ptr`
/// into the heap-local slot, sets up shadow-stack tracking for the OSR frame,
/// copies each incoming local into its register/XMM/frame slot, then jumps to
/// `target_addr`.
#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
unsafe fn emit_osr_trampoline(
    target_addr: usize,
    num_locals: usize,
    num_reg_locals: usize,
    local_assignments: Option<&[Option<u8>]>,
    xmm_assignments: Option<&[Option<u8>]>,
    frame_size: i32,
    callee_saved_base: i32,
    callee_saved_regs: Option<&[u8]>,
    callee_saved_xmms: Option<&[u8]>,
    xmm_saved_base: i32,
    heap_local_offset: i32,
    jit_thread_slot_off: i32,
    stack_floor_slot_off: i32,
    frame_record: usize,
    needs_context: bool,
    dead_mask: u64,
    shadow_thread_slot_off: i32,
    shadow_savetop_slot_off: i32,
    shadow_off_in_thread: i32,
    compile_id: u32,
    sp_id_slot_off: i32,
) -> Option<ExecutableBuffer> {
    use crate::x64::LOCAL_REGS;

    // Platform C-ABI argument register numbers.
    // arg0 carries `locals_ptr`, arg1 carries `vm_ptr`, arg2 carries `thread_ptr`
    // (the shadow-stack thread pointer). All three are caller-saved
    // on both ABIs, and none overlaps any register in `LOCAL_REGS`, so saving
    // arg0 into R10 first cannot clobber a callee-saved local target before
    // we've spilled it, and arg2 survives to the shadow-track block below
    // (the spills write memory, not arg2's register).
    #[cfg(target_os = "windows")]
    let arg0_reg: u8 = 1; // RCX
    #[cfg(target_os = "windows")]
    let arg1_reg: u8 = 2; // RDX
    #[cfg(target_os = "windows")]
    let arg2_reg: u8 = 8; // R8
    #[cfg(not(target_os = "windows"))]
    let arg0_reg: u8 = 7; // RDI
    #[cfg(not(target_os = "windows"))]
    let arg1_reg: u8 = 6; // RSI
    #[cfg(not(target_os = "windows"))]
    let arg2_reg: u8 = 2; // RDX

    let trampoline_size = 1024 + num_locals * 32;
    let mut tramp = ExecutableBuffer::new(trampoline_size)?;
    tramp.set_tag("osr-trampoline");

    // === JIT Prologue ===
    //
    // Every step `x64::Compiler::emit_prologue` performs that the body entered
    // past it relies on must be mirrored here. Three were not: the stack bang,
    // the safepoint-id sentinel, and the compile-id half of the TLS frame
    // record. See each below.
    tramp.emit_byte(0x55); // push rbp
    tramp.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
                                     // Stack bang: probe every page the frame crosses before moving RSP. An OSR
                                     // entry near the end of the stack otherwise skipped the guard page, and its
                                     // first writes access-violated instead of raising StackOverflowError.
                                     // MOV EAX, [RSP + disp32]  (8B 84 24 disp32); RAX is not an argument.
    let bang = crate::x64::jit_stack_bang_enabled();
    if bang {
        for disp in crate::x64::stack_bang_frame_probe_disps(frame_size)? {
            tramp.emit(&[0x8B, 0x84, 0x24]);
            tramp.emit(&disp.to_le_bytes());
        }
    }
    tramp.emit(&[0x48, 0x81, 0xEC]); // sub rsp, imm32
    tramp.emit(&frame_size.to_le_bytes());
    if bang {
        // The one-page headroom probe below the final RSP.
        tramp.emit(&[0x8B, 0x84, 0x24]);
        tramp.emit(&(-crate::x64::STACK_BANG_PAGE_SIZE).to_le_bytes());
    }
    // Safepoint-id sentinel: until the body's first safepoint the slot held
    // whatever the previous frame at this depth left, and a stack walk could
    // match a real oop map on it. MOV RAX, imm32 (sx); MOV [rbp - off], RAX.
    if sp_id_slot_off != 0 && crate::sp_id_slot_init_enabled() {
        tramp.emit(&[0x48, 0xC7, 0xC0]);
        // Cast: the sentinel is `u32::MAX - 1`; see the prologue's store.
        tramp.emit(&(crate::x64::safepoint::SP_ID_UNSET_BC_PC as u32 as i32).to_le_bytes());
        tramp.emit(&[0x48, 0x89, 0x85]);
        tramp.emit(&(-sp_id_slot_off).to_le_bytes());
    }

    // Stash arg0 (locals_ptr) into R10 immediately, before any subsequent emission
    // could clobber the caller-saved arg register. R10 is itself caller-saved and
    // not part of LOCAL_REGS on either platform, so it remains live through the
    // local-copy loop below.
    // Encoding: MOV r10, arg0_reg  =>  REX.W|REX.B 89 (mod=11 reg=arg0 rm=R10&7=2)
    tramp.emit_byte(0x49); // REX.W + REX.B (dest extended)
    tramp.emit_byte(0x89);
    tramp.emit_byte(0xC0 | ((arg0_reg & 7) << 3) | 2);

    // CRITICAL: the callee-saved registers MUST be spilled to the exact
    // frame slots the compiled method's epilogue restores them from.
    //
    // The epilogue (`x64::Compiler::emit_epilogue`) iterates
    // `alloc_used_regs` — which is `RegAllocResult::used_callee_saved` —
    // and restores register `i` from `[rbp - (callee_saved_base + i*8)]`.
    // `used_callee_saved` is produced by filtering the fixed `LOCAL_REGS`
    // priority list, so it is always in `LOCAL_REGS` order.
    //
    // If the trampoline spills the registers in any other order (e.g.
    // local-slot first-appearance order, which can differ when the
    // allocator assigns a higher-priority register to a higher-numbered
    // local), then register X's value lands in the slot the epilogue
    // reads register Y from. After OSR return, the method's epilogue
    // restores the callee-saved registers SWAPPED — and since these hold
    // the caller's live (often pointer-typed) values, the caller resumes
    // with corrupted registers and segfaults on the next dereference.
    //
    // Therefore: always emit the spill set in `LOCAL_REGS` order.
    // HIB-CV-20: spill EXACTLY the callee-saved GPR set the method's epilogue
    // restores, at the SAME slot index. `callee_saved_regs` is the compiler's
    // `alloc_used_regs` (every callee-saved register the allocator used — for
    // locals AND for operand-stack temporaries / spilled values), in the order
    // the epilogue reads `[rbp-(callee_saved_base+i*8)]`. Spilling only the
    // local_assignments-derived subset (the historical fallback below) drops any
    // non-local callee-saved register, so the epilogue restores it (and every
    // later one) from the wrong slot — silently corrupting the OSR caller's live
    // registers on return. Prefer the exact set; keep the subset derivation only
    // for artifacts compiled before this metadata existed.
    let used_regs: Vec<u8> = if let Some(regs) = callee_saved_regs {
        regs.to_vec()
    } else if let Some(assignments) = local_assignments {
        let mut used = [false; 16];
        for &a in assignments {
            if let Some(reg) = a {
                used[(reg & 0x0F) as usize] = true;
            }
        }
        LOCAL_REGS
            .iter()
            .copied()
            .filter(|&r| used[(r & 0x0F) as usize])
            .collect()
    } else {
        LOCAL_REGS.iter().copied().take(num_reg_locals).collect()
    };

    for (i, &reg) in used_regs.iter().enumerate() {
        let neg_off = -(callee_saved_base + i as i32 * 8);
        let rex = 0x48 | if reg >= 8 { 0x04 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0x85 | ((reg & 7) << 3));
        tramp.emit(&neg_off.to_le_bytes());
    }

    // HIB-CV-20: spill the caller's callee-saved XMM registers to the same slots
    // the epilogue restores them from (`x64::xmm_save_slot_offset`, matching
    // `emit_movups_mem_rbp_from_xmm`). The old trampoline skipped XMM spills
    // entirely, so on Windows (where XMM6–XMM15 are callee-saved) a method that
    // used a callee-saved XMM had the caller's value restored from an
    // uninitialised slot. All 128 bits, in 16-byte slots: a 64-bit save paired
    // with the zero-extending restore cleared the caller's upper half.
    // Encoding: [REX.R] 0F 11 /r (MOVUPS m128, xmm) with a disp32 [rbp-off].
    if let Some(xmms) = callee_saved_xmms {
        for (i, &xmm) in xmms.iter().enumerate() {
            let neg_off = -crate::x64::xmm_save_slot_offset(xmm_saved_base, i);
            if xmm >= 8 {
                tramp.emit_byte(0x44); // REX.R (base RBP needs no REX.B)
            }
            tramp.emit_byte(0x0F);
            tramp.emit_byte(0x11);
            tramp.emit_byte(0x85 | ((xmm & 7) << 3)); // mod=10, reg=xmm&7, rm=rbp(5)
            tramp.emit(&neg_off.to_le_bytes());
        }
    }

    if needs_context {
        // Spill vm_ptr (already in arg1_reg, both arg1 candidates are low regs)
        // directly to the heap-local slot. No immediate, no scratch needed.
        let neg_off = -heap_local_offset;
        tramp.emit_byte(0x48); // REX.W
        tramp.emit_byte(0x89); // MOV r/m64, r64
        tramp.emit_byte(0x85 | ((arg1_reg & 7) << 3));
        tramp.emit(&neg_off.to_le_bytes());
    }

    if jit_thread_slot_off != 0 {
        let neg_off = -jit_thread_slot_off;
        let rex = 0x48 | if arg2_reg >= 8 { 0x04 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0x85 | ((arg2_reg & 7) << 3));
        tramp.emit(&neg_off.to_le_bytes());
    }

    // Inline self-recursion check: OSR bypasses the compiled prologue that
    // caches the native-stack floor, so initialise the slot to usize::MAX
    // (`MOV qword [rbp - off], -1` -- imm32 sign-extends). `RSP > MAX` is
    // unsatisfiable, so every self-call site in an OSR-entered frame takes
    // the out-of-line guard helper (safe, merely slower). Leaving the slot
    // uninitialised could skip the guard on garbage and miss a
    // StackOverflowError.
    if stack_floor_slot_off != 0 {
        let neg_off = -stack_floor_slot_off;
        tramp.emit_byte(0x48); // REX.W
        tramp.emit_byte(0xC7); // MOV r/m64, imm32 (sign-extended)
        tramp.emit_byte(0x85); // mod=10, reg=/0, rm=rbp
        tramp.emit(&neg_off.to_le_bytes());
        tramp.emit(&(-1i32).to_le_bytes());
    }

    // OSR bypasses the compiled method's normal prologue, including the exact
    // RBP publication used by precise JIT maps. Mirror the prologue here after
    // live ABI arguments have been saved to their frame homes: prefer the
    // default inline TLS store when available; fall back to the helper-table
    // callback on an unsupported target / inline opt-out. The helper call can
    // clobber caller-saved registers, so preserve the incoming locals/thread
    // pointers in the frame's reserved helper-call stack-arg area.
    let inline_rbp_disp = if frame_record != 0 {
        crate::x64::inline_rbp_tls_disp()
    } else {
        0
    };
    if inline_rbp_disp != 0 {
        // MOV qword ptr <gs|fs>:[disp32], RBP
        tramp.emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        tramp.emit_byte(0x48);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0x2C);
        tramp.emit_byte(0x25);
        tramp.emit(&(inline_rbp_disp as u32).to_le_bytes());
        // …and the identity half of the pair, as the prologue writes it. Only
        // RBP was published, so the id read 0 until the body's first compiled
        // call returned; the collector then fell back to stack decode, which
        // fails for a frame whose return address is this trampoline.
        // MOV dword ptr <gs|fs>:[disp32], imm32
        let cm_disp = crate::x64::inline_cm_tls_disp();
        if compile_id != 0 && cm_disp != 0 {
            tramp.emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
            tramp.emit(&[0xC7, 0x04, 0x25]);
            tramp.emit(&(cm_disp as u32).to_le_bytes());
            tramp.emit(&compile_id.to_le_bytes());
        }
    }
    let call_frame_record = frame_record != 0
        && (inline_rbp_disp == 0 || crate::x64::verify_inline_frame_record_enabled());
    if call_frame_record {
        // MOV [rsp + 32], R10
        tramp.emit(&[0x4C, 0x89, 0x54, 0x24, 32]);
        // MOV R11, arg2_reg
        let rex = 0x48 | 0x01 | if arg2_reg >= 8 { 0x04 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0xC0 | ((arg2_reg & 7) << 3) | 3);
        // MOV [rsp + 40], R11
        tramp.emit(&[0x4C, 0x89, 0x5C, 0x24, 40]);
        // MOV arg0_reg, RBP
        let rex = 0x48 | if arg0_reg >= 8 { 0x01 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0xC0 | (5 << 3) | (arg0_reg & 7));
        // CALL frame_record via RAX.
        tramp.emit(&[0x48, 0xB8]);
        tramp.emit(&(frame_record as i64).to_le_bytes());
        tramp.emit(&[0xFF, 0xD0]);
        // MOV R10, [rsp + 32]
        tramp.emit(&[0x4C, 0x8B, 0x54, 0x24, 32]);
        // MOV R11, [rsp + 40]
        tramp.emit(&[0x4C, 0x8B, 0x5C, 0x24, 40]);
        // MOV arg2_reg, R11
        let rex = 0x48 | 0x04 | if arg2_reg >= 8 { 0x01 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0xC0 | (3 << 3) | (arg2_reg & 7));
    }

    // Shadow-stack OSR-frame handling. An OSR entry bypasses the prologue's
    // `get_current_thread` sequence, so the trampoline must initialize the same
    // cached-thread and saved-watermark slots. A null `thread_ptr` (unit tests)
    // is stored as null and guarded exactly like the normal prologue path.
    if shadow_thread_slot_off != 0 && shadow_savetop_slot_off != 0 {
        // MOV [rbp - shadow_thread_slot_off], arg2   (cache the thread pointer)
        let neg_thr = -shadow_thread_slot_off;
        let rex = 0x48 | if arg2_reg >= 8 { 0x04 } else { 0x00 }; // REX.W (+R if arg2 extended)
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89); // MOV r/m64, r64
        tramp.emit_byte(0x85 | ((arg2_reg & 7) << 3)); // mod=10, reg=arg2, rm=rbp(5)
        tramp.emit(&neg_thr.to_le_bytes());

        // TEST arg2, arg2; JE skip_savetop
        let rex = 0x48
            | if arg2_reg >= 8 { 0x04 } else { 0x00 }
            | if arg2_reg >= 8 { 0x01 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x85);
        tramp.emit_byte(0xC0 | ((arg2_reg & 7) << 3) | (arg2_reg & 7));
        tramp.emit(&[0x0F, 0x84]);
        let skip_savetop = tramp.pos();
        tramp.emit(&0i32.to_le_bytes());

        // R11 = [arg2 + shadow_off_in_thread]   (shadow `top`, ShadowStack TOP=0)
        let rex = 0x48 | 0x04 | if arg2_reg >= 8 { 0x01 } else { 0x00 }; // REX.W + R(r11) (+B if arg2 extended)
        tramp.emit_byte(rex);
        tramp.emit_byte(0x8B); // MOV r64, r/m64
        tramp.emit_byte(0x80 | (3 << 3) | (arg2_reg & 7)); // mod=10, reg=R11&7=3, rm=arg2&7
        tramp.emit(&shadow_off_in_thread.to_le_bytes());

        // MOV [rbp - shadow_savetop_slot_off], R11   (save the entry watermark)
        let neg_sav = -shadow_savetop_slot_off;
        tramp.emit_byte(0x4C); // REX.W + REX.R (R11)
        tramp.emit_byte(0x89); // MOV r/m64, r64
        tramp.emit_byte(0x80 | (3 << 3) | 5); // mod=10, reg=R11&7=3, rm=rbp(5) → 0x9D
        tramp.emit(&neg_sav.to_le_bytes());
        let rel = (tramp.pos() as i64) - (skip_savetop as i64 + 4);
        tramp.try_patch_i32(skip_savetop, rel as i32).ok();
    } else if shadow_thread_slot_off != 0 {
        // Defensive partial-layout fallback: zero the cached thread slot so the
        // push/reload/epilogue null guards skip rather than reading stale stack.
        let neg_off = -shadow_thread_slot_off;
        tramp.emit_byte(0x48); // REX.W
        tramp.emit_byte(0xC7); // MOV r/m64, imm32 (sign-extended)
        tramp.emit_byte(0x85); // mod=10, reg=/0, rm=rbp(5) → [rbp + disp32]
        tramp.emit(&neg_off.to_le_bytes());
        tramp.emit(&0i32.to_le_bytes());
    }

    #[allow(clippy::needless_range_loop)]
    for i in 0..num_locals {
        // Skip locals dead at this OSR entry PC: they are register-resident and
        // their register may be shared (graph-colouring coalescing) with a live
        // local. Loading the dead local here would overwrite the live owner's
        // value. The dead local needs no value (it is dead until its own loop
        // re-defines it), so skipping the load entirely is correct.
        if i < 64 && (dead_mask >> i) & 1 == 1 {
            continue;
        }
        let src_disp = (i as i32) * 8;
        if src_disp == 0 {
            tramp.emit(&[0x49, 0x8B, 0x02]);
        } else {
            tramp.emit(&[0x49, 0x8B, 0x82]);
            tramp.emit(&src_disp.to_le_bytes());
        }

        // `LOCAL_REGS.get(i)`, NOT `LOCAL_REGS[i]`. `num_reg_locals` is
        // `local_assignments.count(Some) + xmm_assignments.count(Some)`
        // (`x64.rs`), so it is a count of HOMES across two register files and
        // bounds nothing about `LOCAL_REGS` — which holds 7 entries on System V
        // and 5 on Windows. A method with 5 GPR-homed and 4 XMM-homed locals
        // makes `num_reg_locals == 9`, and `i == 7` then indexed past the end
        // and panicked inside an unsafe trampoline emitter. Unreachable today
        // only because `osr_enter` returns before this whenever
        // `osr_pc_to_native` is `None`, and `publish_entry_metadata` clears the
        // entry table and the assignment vectors together — an invariant of a
        // different file. `None` is the same answer the `else` arm gives
        // (memory-homed: the frame-slot store below is not elided).
        let dst_reg_opt = if let Some(assignments) = local_assignments {
            assignments.get(i).copied().flatten()
        } else if i < num_reg_locals {
            LOCAL_REGS.get(i).copied()
        } else {
            None
        };

        // round-8 perf (round-4 #11 / round-5 #8): elide the frame-slot
        // store when the local has a canonical register home. The compiled
        // method body reads from `dst_reg` (or `xmm` below) directly; the
        // frame slot is only used as a spill backing store, which the JIT
        // re-establishes lazily before any operation that needs a memory
        // operand. Skipping this MOV saves 7 bytes + one L1d store per
        // register-resident local on each OSR entry.
        let xmm_opt = if let Some(xmm_asgn) = xmm_assignments {
            xmm_asgn.get(i).copied().flatten()
        } else {
            None
        };
        let has_register_home = dst_reg_opt.is_some() || xmm_opt.is_some();
        if !has_register_home || osr_always_seed_frame_slot() {
            let frame_neg_off = -((i as i32 + 1) * 8);
            tramp.emit(&[0x48, 0x89, 0x85]);
            tramp.emit(&frame_neg_off.to_le_bytes());
        }

        if let Some(dst_reg) = dst_reg_opt {
            let rex = 0x48 | if dst_reg >= 8 { 0x04 } else { 0x00 };
            tramp.emit_byte(rex);
            tramp.emit_byte(0x8B);
            tramp.emit_byte(0xC0 | ((dst_reg & 7) << 3));
        }

        // If this local has an XMM assignment, load the value into the XMM register.
        // RAX already contains the local's value (from the MOV above).
        // Emit: MOVQ XMMn, RAX  (66 48|4C 0F 6E /r)
        if let Some(xmm) = xmm_opt {
            let rex_r = if xmm >= 8 { 0x04u8 } else { 0u8 };
            tramp.emit_byte(0x66);
            tramp.emit_byte(0x48 | rex_r); // REX.W + optional REX.R
            tramp.emit_byte(0x0F);
            tramp.emit_byte(0x6E);
            tramp.emit_byte(0xC0 | ((xmm & 7) << 3)); // ModRM: XMMn, RAX
        }
    }

    tramp.emit(&[0x48, 0xB8]);
    tramp.emit(&(target_addr as i64).to_le_bytes());
    tramp.emit(&[0xFF, 0xE0]);

    // Defensive: if the trampoline somehow exceeded its sizing heuristic the
    // emitted code is truncated and unsafe to run — discard it.
    if tramp.overflowed() {
        return None;
    }

    // Transition trampoline buffer from writable to executable. A refused
    // RW->RX flip declines the OSR entry (the loop keeps interpreting) instead
    // of aborting the process -- round 9 wave 3,
    // `executable-buffer-finalize-aborts-the-process-on-protect-failure-20260918.md`.
    // The refused buffer is still RW and unpublished, so dropping it is an
    // ordinary unmap.
    if let Err(e) = tramp.try_finalize() {
        crate::note_code_buffer_protect_failure(&e);
        return None;
    }

    Some(tramp)
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(crate) unsafe fn osr_trampoline(
    target_addr: usize,
    vm_ptr: i64,
    locals_ptr: *const i64,
    num_locals: usize,
    num_reg_locals: usize,
    local_assignments: Option<&[Option<u8>]>,
    xmm_assignments: Option<&[Option<u8>]>,
    frame_size: i32,
    callee_saved_base: i32,
    callee_saved_regs: Option<&[u8]>,
    callee_saved_xmms: Option<&[u8]>,
    xmm_saved_base: i32,
    heap_local_offset: i32,
    jit_thread_slot_off: i32,
    stack_floor_slot_off: i32,
    frame_record: usize,
    needs_context: bool,
    dead_mask: u64,
    shadow_thread_slot_off: i32,
    shadow_savetop_slot_off: i32,
    shadow_off_in_thread: i32,
    compile_id: u32,
    sp_id_slot_off: i32,
    thread_ptr: i64,
) -> Option<i64> {
    // Look up (or emit and insert) the cached trampoline body for this target.
    // `target_addr` encodes the compiled method and native offset, both stable
    // for the method's life, and the frame layout, register assignments and
    // compile id are functions of the method. `dead_mask` is NOT a function of
    // `target_addr` — two OSR pcs can share a native offset — so it is part of
    // the key; see `osr_trampoline_cache`.
    let cache_key = (target_addr, dead_mask);
    let tramp_arc: Arc<ExecutableBuffer> = {
        let cache = osr_trampoline_cache();
        // Fast path: read-only lookup. The result is bound to a local so the
        // `MutexGuard` temporary is dropped at the end of this statement.
        // Under edition 2021 a guard created directly in an `if let`
        // scrutinee stays live through the `else` arm, so the `cache.lock()`
        // in the slow path below would re-lock the same non-reentrant
        // `parking_lot::Mutex` on this thread and deadlock.
        let existing = cache.lock().get(&cache_key).cloned();
        if let Some(existing) = existing {
            existing
        } else {
            // Slow path: emit outside the lock, then insert under it. If another
            // thread raced us, prefer their entry and let our buffer drop.
            let fresh = emit_osr_trampoline(
                target_addr,
                num_locals,
                num_reg_locals,
                local_assignments,
                xmm_assignments,
                frame_size,
                callee_saved_base,
                callee_saved_regs,
                callee_saved_xmms,
                xmm_saved_base,
                heap_local_offset,
                jit_thread_slot_off,
                stack_floor_slot_off,
                frame_record,
                needs_context,
                dead_mask,
                shadow_thread_slot_off,
                shadow_savetop_slot_off,
                shadow_off_in_thread,
                compile_id,
                sp_id_slot_off,
            )?;
            let fresh_arc = Arc::new(fresh);
            // Profiler/debugger symbols; a racing loser is retired on drop.
            crate::code_events::publish(fresh_arc.as_ptr() as usize, fresh_arc.pos(), || {
                let label = format!("osr-trampoline->{target_addr:#x}");
                crate::code_events::with_tier_suffix(
                    &label,
                    crate::code_events::CodeTier::Stub("osr-trampoline"),
                )
            });
            let mut guard = cache.lock();
            guard
                .entry(cache_key)
                .or_insert_with(|| fresh_arc.clone())
                .clone()
        }
    };

    let code_ptr = tramp_arc.as_ptr();
    // Non-panicking validation: if the freshly-emitted trampoline pointer
    // somehow isn't in a registered code region, log and bail rather than
    // aborting the process. The caller (`osr_enter`) treats `None` as
    // "skip OSR, fall back to interpreter".
    if let Err(reason) = crate::validate_code_ptr(code_ptr) {
        tracing::warn!(
            reason = reason,
            "JIT osr_trampoline: invalid code pointer; skipping OSR"
        );
        return None;
    }

    // The cached trampoline takes (locals_ptr, vm_ptr, thread_ptr) via the
    // platform C ABI. `vm_ptr` is only read when `needs_context`, and
    // `thread_ptr` only when shadow-stack frame slots exist; both are passed
    // unconditionally (caller-saved registers, ignored if unused).
    let tramp_fn: unsafe extern "C" fn(*const i64, i64, i64) -> i64 = std::mem::transmute(code_ptr);

    // SAFETY: `tramp_arc` holds an Arc clone of the cached buffer, keeping the
    // executable memory alive for the duration of the call. `locals_ptr` is
    // borrowed from the caller's `jit_locals: &[i64]` slice, which is live
    // across `osr_enter` (and therefore across this call). The fences and
    // black_box prevent the optimizer from reordering the Arc drop above the
    // call or otherwise invalidating the live region.
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    let result = tramp_fn(locals_ptr, vm_ptr, thread_ptr);
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    std::hint::black_box(code_ptr);
    std::hint::black_box(&tramp_arc);

    drop(tramp_arc);

    Some(result)
}
