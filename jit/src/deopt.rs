// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Deoptimization framework for the JIT compiler.
//!
//! When speculative optimizations turn out to be invalid at runtime, the JIT
//! must transfer execution back to the interpreter. This module provides:
//!
//! - Metadata embedded in compiled code (`DeoptimizationPoint`, `FrameState`)
//!   that describes how to reconstruct an interpreter frame at each deopt site.
//! - A `DeoptimizationLog` that records deopt events and drives adaptive
//!   recompilation decisions.
//! - A [`DeoptVerifier`] that checks emitted deopt metadata *before* the
//!   artifact is installed, so a frame that could not be reconstructed
//!   byte-for-byte bails the compile instead of becoming live code.
//!
//! ## Eliminated vs. undefined
//!
//! Two states that look identical in a snapshot are semantically opposite, and
//! conflating them is how a scalar-replaced object silently reconstructs as
//! `null`:
//!
//! * [`FrameValue::Undefined`] — the slot genuinely holds nothing at this bci
//!   (never stored, or the reserved upper half of a cat-2 value). The resume
//!   sink maps it to `Value::Int(0)`, which is correct *because the interpreter
//!   never reads it*.
//! * [`FrameValue::MaterializationRequired`] — the slot held a value the
//!   optimizer **deleted** (a scalar-replaced allocation, an elided lock, an
//!   eliminated store), and the emitter could not describe how to rebuild it.
//!   The interpreter *will* read this slot. Resuming it as `Int(0)` hands Java
//!   code a null where a live object was.
//!
//! Producers must never spell the second case as the first. See
//! `docs/jit/deopt-metadata.md` for the producer-by-producer status.

// `DefaultHasher`, `hash::{Hash, Hasher}` and `sync::Arc` were imported for the
// interned frame-state representation retired on 2026-09-21 (see the note above
// `MAX_SCOPE_CHAIN`): its structural hashing and its `Arc<[FrameValue]>` chunk
// store were the only users. `speculation_id` brings its own `Hasher` into scope
// locally, next to the `FxHasher` it needs it for.
use std::{collections::VecDeque, fmt};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::bailout::{Bailout, BailoutReason, CompileResult};

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Why a deoptimization was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeoptReason {
    /// Null check failed (optimized away but encountered null).
    NullCheck,
    /// Type check failed (instanceof/checkcast assumption violated).
    ClassCheck,
    /// Array bounds check failed.
    BoundsCheck,
    /// Division by zero.
    DivByZero,
    /// Receiver type changed (inline cache miss / polymorphic dispatch).
    ReceiverTypeChanged,
    /// Class loading invalidated an assumption (e.g., new subclass loaded).
    ClassLoading,
    /// Uninitialized field access (escape analysis assumption violated).
    UninitializedAccess,
    /// Transfer to interpreter requested (e.g., debug breakpoint).
    TransferToInterpreter,
    /// Uncommon trap — rare branch taken.
    UncommonTrap,
    /// Speculative optimization failed.
    SpeculationFailed,
    /// Not compiled (method too complex).
    NotCompiled,
    /// Unreached code executed.
    UnreachedCode,
    /// OSR-exit — a running JIT/OSR frame bailed mid-loop back to the
    /// interpreter at a loop bci (not a guard bci). Distinct from
    /// `UncommonTrap` so OSR-exit events are countable separately in the
    /// `DeoptimizationLog`; for `recommend_action` policy it currently falls
    /// through to the count-based default, exactly like `UncommonTrap`
    /// (see `docs/feature-designs/deopt-osr.md`, scaffolding).
    OsrExit,
    /// A Java exception is pending in a compiled method whose handler reads
    /// non-parameter locals (the RBC.6 precise-handler-frame relaxation).
    ///
    /// The frame this reason stamps exists for exactly ONE purpose: to hand the
    /// interpreter the throwing bci and the live locals so the exception can be
    /// routed through that method's own exception table. It is **not** a resume
    /// point — its `bci` names the throwing instruction (not a successor to
    /// continue at) and its operand stack is the post-pop state of the call that
    /// threw. Resuming it as an ordinary deopt executes the instruction after a
    /// call that never returned, with the result missing from the stack. That is
    /// why such a frame is stashed separately ([`take_exceptional_frame`]) and
    /// never lands in `LAST_DEOPT`.
    PendingException,
    /// JIT r20 wave 11, lane pProf (W17L4-5 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): the
    /// guard of a never-taken-pruned conditional branch
    /// (`IrBuilder::prune_never_taken_branch`, listed in
    /// `Graph::unstable_if_trap_pcs`) failed -- the branch was taken. HotSpot's
    /// `Reason_unstable_if`: the profile that pruned the edge is wrong, so
    /// [`DeoptimizationLog::recommend_action`] recompiles at once and the VM
    /// withdraws the prune at that bci after ONE charge (the per-bci limit is
    /// 1 for this reason), so the first real taken edge costs one deopt and
    /// one recompile. Re-executes like [`Self::UncommonTrap`]. Added last so
    /// every other variant keeps its discriminant (`speculation_id` hashes
    /// it). `CRATONVM_JIT_PROF_UNSTABLE_IF_REASON=0` stamps `UncommonTrap`.
    UnstableIf,
}

/// What the runtime should do after a deopt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptAction {
    /// Continue in the interpreter at the current bci.
    Reinterpret,
    /// Invalidate compiled code and recompile with updated profile.
    RecompileAndReinterpret,
    /// Mark compiled code as invalid but don't recompile yet.
    MakeNotEntrant,
    /// Give up on compiling this method entirely.
    MakeNotCompilable,
}

// ---------------------------------------------------------------------------
// Frame reconstruction types
// ---------------------------------------------------------------------------

/// A single value in an interpreter frame (local or stack slot).
#[derive(Debug, Clone, PartialEq)]
pub enum FrameValue {
    /// Integer constant (cat-1: JVM `int`/`boolean`/`byte`/`char`/`short`).
    Int(i64),
    /// Category-2 `long` constant. Distinct from [`FrameValue::Int`] so the
    /// resume builds a `Value::Long` (occupying one compact operand-stack slot
    /// and two JVM local slots) rather than a truncated `Value::Int`.
    Long(i64),
    /// Cat-1 `float` constant, stored as its raw 32-bit IEEE-754 pattern in the
    /// low bits (upper bits zero). The resume builds a `Value::Float`
    /// (`f32::from_bits(bits as u32)`) occupying one local/stack slot.
    Float(u64),
    /// Category-2 `double` constant, stored as its raw 64-bit IEEE-754 pattern.
    /// Distinct from [`FrameValue::Float`] (cat-1, 32-bit) so the resume builds a
    /// `Value::Double` (`f64::from_bits(bits)`) with correct cat-2 two-slot local
    /// placement, and from [`FrameValue::Long`] so the bits are reinterpreted as
    /// a double rather than a long.
    Double(u64),
    /// Object reference (heap address, 0 for null).
    Object(u64),
    /// Cat-1 `int` currently in a general-purpose machine register. Resolves to a
    /// (truncated-to-32-bit) [`FrameValue::Int`] from `gpr[n]`, so it is for
    /// `int`s only — a register-resident `long` uses [`FrameValue::RegisterLong`]
    /// and a register-resident object reference [`FrameValue::RegisterRef`].
    Register(u8),
    /// Cat-2 `long` currently live in general-purpose register `n`. Resolves to
    /// [`FrameValue::Long`] from the full 64 bits of `gpr[n]`. Distinct from
    /// [`FrameValue::Register`] (which resolves to a cat-1 `Int`, truncated to 32
    /// bits on resume) so a register-resident `long` keeps all 64 bits.
    RegisterLong(u8),
    /// Object reference currently live in general-purpose register `n`. Resolves
    /// to [`FrameValue::Object`] from the full 64 bits of `gpr[n]` — the register
    /// holds the raw heap pointer (0 == null), exactly as
    /// [`FrameValue::StackSlotRef`] does for a spilled ref. Distinct from
    /// [`FrameValue::Register`] so the resume builds a `Value::Object` (GC-tracked)
    /// instead of a truncated `Value::Int` that would also drop the oop from the
    /// GC root scan.
    RegisterRef(u8),
    /// Cat-1 `float` currently live in XMM register `n`. Resolves to
    /// [`FrameValue::Float`] from the low 32 bits of the spilled `xmm[n]`
    /// ([`SavedRegisters::xmm`]). The JIT FP value tier keeps a `float` in an XMM
    /// across a guard; the deopt stub spills all 16 XMM regs so this resolves.
    XmmFloat(u8),
    /// Cat-2 `double` currently live in XMM register `n`. Resolves to
    /// [`FrameValue::Double`] from the full 64 bits of the spilled `xmm[n]`.
    XmmDouble(u8),
    /// Value at a native stack slot offset, holding a cat-1 **int** (JVM
    /// `int`/`boolean`/`byte`/`char`/`short`). Resolves to [`FrameValue::Int`].
    StackSlot(i32),
    /// Value at a native stack slot offset, holding an **object reference**
    /// (`real-frame-deopt` type source). Resolves to [`FrameValue::Object`] —
    /// the raw word read from the slot IS the heap pointer. Distinct from
    /// `StackSlot` so the resume builds a `Value::Object` (not a truncated
    /// `Value::Int`) for ref-typed locals/stack slots such as an instance
    /// method's `this`.
    StackSlotRef(i32),
    /// Value at a native stack slot offset, holding a category-2 **long** (the
    /// lowerer spills the full 64-bit value). Resolves to [`FrameValue::Long`] —
    /// the raw 64-bit word read from the slot IS the long value. Distinct from
    /// `StackSlot` (which is a cat-1 `int`) so the resume builds a `Value::Long`
    /// with correct cat-2 two-slot local placement (`real-frame-deopt` cat-2).
    StackSlotLong(i32),
    /// Value at a native stack slot offset, holding a cat-1 `float` (the lowerer
    /// spills the 32-bit IEEE bit pattern in the low word). Resolves to
    /// [`FrameValue::Float`] — the low 32 bits ARE the float bits. Distinct from
    /// `StackSlot` (a cat-1 `int`) so the resume builds a `Value::Float`.
    StackSlotFloat(i32),
    /// Value at a native stack slot offset, holding a category-2 `double` (the
    /// lowerer spills the full 64-bit IEEE bit pattern). Resolves to
    /// [`FrameValue::Double`] — the raw 64-bit word IS the double bits. Distinct
    /// from `StackSlotLong` so the resume builds a `Value::Double` (cat-2).
    StackSlotDouble(i32),
    /// Scalar-replaced object that must be re-materialized.
    VirtualObject(VirtualObjectState),
    /// A reference to another scalar-replaced object in the same deopt frame, by
    /// its [`VirtualObjectState::id`]. Represents shared references and cycles:
    /// each object is *defined* exactly once by its `VirtualObject(state)`
    /// occurrence, and every other edge to it (including a back-edge that would
    /// otherwise nest infinitely) is a `VirtualObjectRef(id)`. Materialization
    /// resolves it to the shell allocated for that id in Phase 1; it never
    /// resolves to a machine value, so `resolve_value` passes it through.
    VirtualObjectRef(usize),
    /// Null-guarded scalar-replaced object: evaluates `cond` (`Int`) at deopt
    /// time; `0` resolves to `Object(0)` (`null`), non-zero resolves to
    /// `VirtualObject(state)`.
    ConditionalVirtualObject(Box<(FrameValue, VirtualObjectState)>),
    /// **The value that belonged in this slot was deleted by an optimization
    /// and must be materialised — but the emitter could not describe how.**
    ///
    /// Distinct from [`FrameValue::Undefined`] on purpose, and the distinction
    /// is a correctness one, not a diagnostic nicety. `Undefined` means "the
    /// interpreter never reads this slot", and every resume sink maps it to
    /// `Value::Int(0)` on that basis. A scalar-replaced object recorded as
    /// `Undefined` therefore reconstructs as `0` — i.e. `null` for a
    /// reference-typed local — with no error anywhere: the exact silent
    /// wrong-reconstruction this variant exists to make impossible.
    ///
    /// Semantics on resume: **unresumable**. [`frame_state_is_resumable`]
    /// returns `false` for a frame containing one, and the VM sinks' catch-all
    /// arms (`fv_to_value` → `None`, `field_value_to_value` → `Err`) already
    /// refuse it, so the method takes the safe whole-method re-run instead of a
    /// precise resume. It is strictly better than `Undefined` (wrong value,
    /// silently) and strictly better than [`FrameValue::Unsupported`] (right
    /// outcome, but it says "unknown width" and so misattributes the cause).
    ///
    /// A slot whose eliminated value the emitter *can* rebuild is
    /// [`FrameValue::VirtualObject`] / [`FrameValue::VirtualObjectRef`], not
    /// this.
    MaterializationRequired(EliminatedValue),
    /// Genuinely undefined / uninitialized at this bci: a local never stored on
    /// any path reaching here, or the reserved upper half of a cat-2
    /// `long`/`double`. Resume sinks map it to `Value::Int(0)`, which is sound
    /// **only** because the bytecode verifier guarantees the interpreter cannot
    /// read such a slot before something writes it.
    ///
    /// This must NOT be used for a value that was optimized away — that is
    /// [`FrameValue::MaterializationRequired`].
    Undefined,
    /// A live slot whose precise value can't be reconstructed for resume. With
    /// cat-2 (`Long`/`Double`/`StackSlotLong`/`StackSlotDouble`) and FP
    /// (`Float`/`XmmFloat`/`XmmDouble`/`StackSlotFloat`) now representable, this
    /// is reserved for slots whose JVM width/type the snapshot emitter cannot
    /// determine with certainty at the guard bci (the single-pass backend has no
    /// global per-slot type oracle). The resume treats it as "fall back to the
    /// safe re-run path" rather than fabricate a value, so a method with such a
    /// slot live at a guard is never resumed with garbage.
    Unsupported,
}

/// Why a slot's value is gone from the machine state, for
/// [`FrameValue::MaterializationRequired`].
///
/// Carried so the compiler report can name the *pass* that deleted the value
/// rather than reporting an anonymous "cannot resume". Every variant means the
/// same thing to the resume path (refuse), so a producer that cannot classify
/// precisely should use [`EliminationCause::Unclassified`] rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EliminationCause {
    /// Escape analysis scalar-replaced the allocation, but the emitter could
    /// not prove the object's fields were all stored before this deopt point
    /// (or could not resolve one of them), so no `VirtualObject` recipe exists.
    ScalarReplacedObject,
    /// The object is scalar-replaced and one of its fields is *itself* a
    /// scalar-replaced object — nested virtual graphs are not emitted yet.
    NestedVirtualObject,
    /// A store whose value this slot names was deleted by dead-store
    /// elimination, so the slot's value has no producer left in the graph.
    EliminatedStore,
    /// The monitor this slot describes was elided by lock elision, so the
    /// resume has no object to re-lock.
    ElidedLock,
    /// The producing node was removed and the emitter has no better
    /// attribution. Prefer a specific cause when one is known.
    Unclassified,
}

impl fmt::Display for EliminationCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::ScalarReplacedObject => "scalar-replaced object",
            Self::NestedVirtualObject => "nested virtual object",
            Self::EliminatedStore => "eliminated store",
            Self::ElidedLock => "elided lock",
            Self::Unclassified => "unclassified elimination",
        };
        f.write_str(s)
    }
}

/// The provenance of a value an optimization deleted, carried by
/// [`FrameValue::MaterializationRequired`].
///
/// `producer` is the IR `NodeId` of the node that used to compute the value
/// (`u32::MAX` when the producer is unknown), which is what makes an
/// unreconstructable slot *actionable*: the compiler report names the node the
/// pass removed, and [`DeoptVerifier`] can cross-check it against the set of
/// nodes the optimizer actually retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EliminatedValue {
    /// IR node id of the deleted producer, or `u32::MAX` when unknown.
    pub producer: u32,
    /// Class id of the eliminated object when it was an allocation, else `0`.
    pub class_id: u32,
    /// Which pass deleted it.
    pub cause: EliminationCause,
}

impl EliminatedValue {
    /// An eliminated value with a known producer node and cause.
    pub fn new(producer: u32, cause: EliminationCause) -> Self {
        Self {
            producer,
            class_id: 0,
            cause,
        }
    }

    /// An eliminated *allocation* — producer node plus the class it allocated.
    pub fn allocation(producer: u32, class_id: u32, cause: EliminationCause) -> Self {
        Self {
            producer,
            class_id,
            cause,
        }
    }

    /// An eliminated value whose producer node id is not known to the emitter.
    pub fn unknown(cause: EliminationCause) -> Self {
        Self {
            producer: u32::MAX,
            class_id: 0,
            cause,
        }
    }
}

impl fmt::Display for EliminatedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.cause)?;
        if self.producer != u32::MAX {
            write!(f, " (producer n{})", self.producer)?;
        }
        if self.class_id != 0 {
            write!(f, " class_id={}", self.class_id)?;
        }
        Ok(())
    }
}

/// State of a scalar-replaced object that needs heap materialization.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualObjectState {
    /// `Some(atype)` when this describes a scalar-replaced **array** rather
    /// than an object: the JVM `newarray` atype of its elements, and
    /// [`Self::num_fields`] is its LENGTH. `None` for an ordinary object.
    ///
    /// The materializer needs the distinction because the two are allocated by
    /// different calls -- an object by class id and field count, an array by
    /// element type and length -- and because an array's slots are elements,
    /// stored through the array accessor rather than the field accessor.
    /// [`Self::class_id`] is meaningless when this is `Some` and is not read.
    ///
    /// Only primitive element types reach here (atype 4..=11); a reference
    /// array is refused upstream in `escape_analysis`, so a materialized
    /// element never needs a reference store barrier.
    pub array_element_type: Option<u8>,
    /// Identity of this scalar-replaced object within its deopt frame. Distinct
    /// objects have distinct ids; `FrameValue::VirtualObjectRef(id)` edges (and
    /// the materializer's shell map) resolve against it, which is what lets the
    /// two-phase materialization rebuild shared references and cycles.
    pub id: usize,
    pub class_id: u32,
    pub num_fields: usize,
    pub field_values: Vec<FrameValue>,
}

/// JIT round 21 wave 3 (lane ea, EA21-2): a [`VirtualObjectState::array_element_type`]
/// marker (outside the JVM atypes 4..=11) saying the state is an AUTO-BOX the
/// optimizing tier removed, `Integer.valueOf(field_values[0])`. The
/// materialiser answers it with the JDK's own `Integer.valueOf`, so a value in
/// the box cache comes back as THE cached instance (HotSpot's
/// `Deoptimization::get_cached_box`), not a fresh object. `class_id` is not
/// read; `num_fields` is 1. Produced by `ea_ir_bridge::ir_fold_box_phi_webs`
/// (`CRATONVM_JIT_IR_BOX_PHI_WEB_FOLD`).
pub const AUTO_BOX_INTEGER_ATYPE: u8 = 0xB1;
/// [`AUTO_BOX_INTEGER_ATYPE`]'s `Long.valueOf(field_values[0])` twin.
pub const AUTO_BOX_LONG_ATYPE: u8 = 0xB2;
/// Sentinel `array_element_type` for a virtual `java/lang/StringBuilder`
/// accumulated across fused `append` steps when a call operand deopts mid-chain.
///
/// `field_values` stores alternating `(tag, payload)` pairs:
/// - `(SB_VIRTUAL_TAG_LITERAL, Constant(int literal_idx))`
/// - `(SB_VIRTUAL_TAG_STRING..=SB_VIRTUAL_TAG_DOUBLE, value)`
pub const VIRTUAL_STRING_BUILDER_ATYPE: u8 = 0xB3;
pub const SB_VIRTUAL_TAG_LITERAL: i64 = 0;
pub const SB_VIRTUAL_TAG_STRING: i64 = 1;
pub const SB_VIRTUAL_TAG_CHAR: i64 = 2;
pub const SB_VIRTUAL_TAG_INT: i64 = 3;
pub const SB_VIRTUAL_TAG_LONG: i64 = 4;
pub const SB_VIRTUAL_TAG_BOOLEAN: i64 = 5;
pub const SB_VIRTUAL_TAG_FLOAT: i64 = 6;
pub const SB_VIRTUAL_TAG_DOUBLE: i64 = 7;

#[inline]
pub const fn is_virtual_string_builder_atype(atype: u8) -> bool {
    atype == VIRTUAL_STRING_BUILDER_ATYPE
}

/// The `(wrapper class, valueOf descriptor)` an auto-box marker names, or
/// `None` for any other atype (an ordinary array, or an object).
pub fn auto_box_value_of(atype: u8) -> Option<(&'static str, &'static str)> {
    match atype {
        AUTO_BOX_INTEGER_ATYPE => Some(("java/lang/Integer", "(I)Ljava/lang/Integer;")),
        AUTO_BOX_LONG_ATYPE => Some(("java/lang/Long", "(J)Ljava/lang/Long;")),
        _ => None,
    }
}

/// How deep an inlined caller chain an OSR exit may carry.
///
/// This is a COUPLING, not a tuning knob: it is the VM's
/// `deopt_resume::MAX_INLINE_RESUME_DEPTH`, which is that side's budget for
/// materialising a chain atomically, and `CompiledMethod::osr_exit_policy`
/// refuses at admission anything the transfer would refuse at the exit. The VM
/// constant is defined as this one so the two cannot drift — a compile that
/// admits a chain the VM then refuses spends a whole OSR entry to reach a safe
/// reject, which is the shape admission-time checking exists to avoid.
///
/// 9 is our legacy resume budget, not HotSpot's high-tier inline limit.
/// `ir::MAX_INLINE_SCOPE_DEPTH` (64) and `MAX_SCOPE_CHAIN` (256) bound
/// different things — what may be recorded, and what may be walked.
pub const MAX_OSR_INLINE_RESUME_DEPTH: usize = 9;

/// The resume budget in force: [`MAX_OSR_INLINE_RESUME_DEPTH`], or
/// [`crate::rx27_deep_spine::RESUME_DEPTH`] (13) under
/// `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` (round rx27 wave 4; default ON since 2026-10-29), which
/// lets the builder open 12 splice frames ([`crate::ir::ir_max_splice_depth`]).
/// Every admission and transfer check reads this one function -- the jit's
/// OSR-exit policy, the chain snapshot publication, the in-place transfer
/// walk and the VM's `deopt_resume` sinks -- so the two sides cannot drift.
/// The sinks build the chain in a `Vec` and refuse before pushing anything,
/// so the budget bounds work, not storage. Read per check, no `static`.
pub fn max_osr_inline_resume_depth() -> usize {
    if crate::rx27_deep_spine::enabled() {
        crate::rx27_deep_spine::RESUME_DEPTH
    } else {
        MAX_OSR_INLINE_RESUME_DEPTH
    }
}

/// How many caller scopes `fs` carries. Bounded by [`MAX_SCOPE_CHAIN`], so a
/// cyclic or absurd chain answers the cap rather than looping.
pub fn caller_chain_depth(fs: &FrameState) -> usize {
    let mut depth = 0usize;
    let mut scope = fs.caller.as_deref();
    while let Some(f) = scope {
        depth += 1;
        if depth >= MAX_SCOPE_CHAIN {
            return depth;
        }
        scope = f.caller.as_deref();
    }
    depth
}

/// Can a [`FrameState`] be turned back into an interpreter frame?
///
/// `false` as soon as any live local or operand-stack slot is
/// [`FrameValue::Unsupported`] — the marker the snapshot emitter writes when it
/// cannot determine a slot JVM width/type — or
/// [`FrameValue::MaterializationRequired`], the marker for a slot whose value an
/// optimization deleted without leaving a rebuild recipe. The VM resume sink
/// (`build_deopt_frame_inner`) maps both to `None` and refuses the resume, so a
/// deopt point carrying one is unresumable by construction.
///
/// The scan reaches into [`FrameValue::VirtualObject`] field graphs for the
/// eliminated marker only: materializing an object whose field held a deleted
/// value would store a `null` into a field that held a live reference.
/// `Unsupported` *inside* a virtual object is deliberately left as it always
/// was (not propagated) — the producers already refuse to emit such a field
/// (`ir_lower::frame_value_for_object`), and widening the predicate there would
/// silently change which methods the `invokedynamic` trap bail rejects.
/// Recursion terminates because a `VirtualObjectRef` edge is *not* followed —
/// it is the cycle/sharing terminator.
///
/// Callers that emit an UNCONDITIONAL trap (the `invokedynamic` uncommon trap)
/// use this to bail the whole compile: an artifact that always traps at a point
/// it can never resume from fails on its first compiled call with
/// `InternalError: precise deoptimization unavailable ... refusing
/// side-effecting replay`, which is strictly worse than staying interpreted.
///
/// ## The whole chain, not just the innermost scope
///
/// The scan follows `caller`. It has to: a deopt inside an inlined callee
/// rebuilds *every* frame in the chain, so a caller scope holding a value no
/// one can describe is exactly as unresumable as the trapping scope holding
/// one — and it is the caller's locals that a resume would silently fill with
/// `Value::Int(0)`.
///
/// It used to stop at the innermost scope, which was correct only because no
/// producer built a chain. The callers are the two **compile-time** admission
/// gates — `jit/src/x64.rs`'s unresumable-`invokedynamic`-trap bail and
/// `CompiledMethod::osr_exit_policy` — and both are deciding "may this artifact
/// ever be entered". An artifact whose only trap sits under an undescribable
/// caller scope must fail that question, not pass it because the innermost
/// frame happens to be clean.
///
/// (The VM's *resume* sinks are a separate, stricter gate. They materialise a
/// chain since the N2 work (`deopt_resume::materialise_inlined_chain`), but
/// only one whose every value is a constant or a machine word: see
/// [`inlined_chain_refusal`], which is what the optimizing tier asks before it
/// publishes one (round 13 wave 3). This predicate is what decides whether
/// such a chain gets compiled in the first place.)
///
/// A one-scope chain is exactly the old predicate. (The single-pass backend
/// does build caller chains inside a splice — `Compiler::inline_caller_chain`
/// — but `x64/inlining.rs`'s splice postcondition rolls back any splice that
/// published a point, so no finished artifact carries one yet.) This is now the
/// only resumability predicate: the handle-side pair that used to shadow it
/// (a scope-local `is_resumable` and a whole-chain `chain_is_resumable` on the
/// interner) went with the interned representation on 2026-09-21 — see the
/// retirement note above [`MAX_SCOPE_CHAIN`].
///
/// The walk is bounded by [`MAX_SCOPE_CHAIN`]: a chain longer than that is
/// treated as unresumable rather than walked further, because a chain that deep
/// is a metadata defect and refusing costs only a whole-method re-run.
pub fn frame_state_is_resumable(fs: &FrameState) -> bool {
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        if f.locals
            .iter()
            .chain(f.stack.iter())
            .any(value_blocks_resume)
        {
            return false;
        }
        seen += 1;
        if seen >= MAX_SCOPE_CHAIN {
            // Deeper than any real inliner produces: refuse rather than keep
            // walking a chain that is already known to be malformed.
            return f.caller.is_none();
        }
        scope = f.caller.as_deref();
    }
    true
}

/// Why the VM could NOT materialise `fs` as an inlined chain of interpreter
/// frames, or `None` when it can -- the compile-time mirror of
/// `vm/src/runtime/interpreter/deopt_resume.rs`'s `materialise_inlined_chain`
/// and `caller_frame_values` (round 13 wave 3, lane framestate).
///
/// Stricter than [`frame_state_is_resumable`] on purpose, and it has to be:
/// the chain sinks rebuild EVERY scope through `fv_to_value`, which maps only
/// constants, resolved machine words and `Undefined`. A scalar-replaced object
/// (`VirtualObject` / `VirtualObjectRef`) that the single-frame sink would
/// materialise is refused there, and so is a monitor the resume would have to
/// re-acquire (`relock`). A chain the VM refuses at run time does not fall back
/// to the flat frame the producer could have published instead: it costs a
/// whole-method re-run, which is exactly what precise resume exists to avoid.
/// So `ir_lower` asks this before publishing a chain, and publishes the flat
/// frame when it answers `Some`.
///
/// Also refused: a chain deeper than [`MAX_OSR_INLINE_RESUME_DEPTH`] (the VM's
/// resume budget), a register outside the spilled files, a held monitor whose
/// object is not a machine word the stub resolves to a reference, and a monitor
/// in any scope but the outermost (no spliced body holds one; a chain that says
/// otherwise is malformed). `fs` without a caller is not a chain: `Some`.
///
/// **Scalar-replaced objects** (round 13 wave 5, lane resume): with
/// [`chain_virtuals_enabled`] a chain may name `VirtualObject` /
/// `VirtualObjectRef` values, which the deopt-exit chain sinks re-allocate
/// CHAIN-WIDE (`deopt_resume::materialise_chain_virtuals`): the producer
/// numbers them with one id space for the whole chain
/// (`ir_lower::splice_chain_frame_state` resolves every scope with one
/// `emitted` set), so an object two scopes name is defined in one and
/// referenced from the other, and must become ONE heap object. Such a chain is
/// still refused when it holds any monitor (an abandoned activation's locks
/// would have no release path on a failed re-allocation), when a reference
/// has no definition in the chain or an id is defined twice, when a field is
/// a value the materialiser cannot store, and always with the switch off.
pub fn inlined_chain_refusal(fs: &FrameState) -> Option<&'static str> {
    inlined_chain_refusal_with(fs, chain_virtuals_enabled)
}

/// `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS`, **default ON** since 2026-10-06 (round
/// 13 wave 5, lane resume; flipped by the ealc round, `=0` is the kill switch):
/// may an optimizing-tier inlined caller chain name a scalar-replaced object
/// ([`inlined_chain_refusal`])? Read by both ends -- the producer's publish
/// check and the VM's chain builder (`deopt_resume::build_deopt_frame_chain`)
/// -- so they agree either way. Only reached for a chain that names one (and
/// no chain names one unless `ea_ir_bridge` lets escape analysis remove an
/// object a chain snapshot names). It is what lets a for-each iterator whose
/// `hasNext`/`next` are spliced be deleted: every chain inside those bodies
/// names it as `this`. Not memoised (no new static).
pub fn chain_virtuals_enabled() -> bool {
    true
}

/// The class id a `CompiledMethod::splice_scope_class_ids` row carries for a
/// method key two splices of ONE compile resolved to different classes
/// (two loaders' same-named classes): no id can be trusted for that key, and
/// the name rule would pick one of them blindly (round 13 wave 6, lane chain2).
pub const SPLICE_SCOPE_CLASS_AMBIGUOUS: u32 = u32::MAX;

/// What an artifact's scope class table says about one inlined scope of a
/// deopt chain ([`splice_scope_class_hint`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpliceScopeClass {
    /// No row (an artifact without the table, a chain the single-pass tier
    /// built, a site the resolver gave no id): resolve by name, as before.
    Unknown,
    /// The class id the compile spliced this scope's body from.
    Exact(u32),
    /// Two different classes in one compile: refuse the scope.
    Ambiguous,
}

/// Look `method_key` up in `hints`, an artifact's
/// `CompiledMethod::splice_scope_class_ids` (sorted by key, one row per key,
/// as `ir::splice_scope_class_ids` builds it). The VM's chain builder resolves
/// an inner scope through this before the by-name rule
/// (`r13w5-resume-chain-scope-class-ids-patch`).
pub fn splice_scope_class_hint(hints: &[(String, u32)], method_key: &str) -> SpliceScopeClass {
    match hints.binary_search_by(|(key, _)| key.as_str().cmp(method_key)) {
        Ok(i) => match hints[i].1 {
            0 => SpliceScopeClass::Unknown,
            SPLICE_SCOPE_CLASS_AMBIGUOUS => SpliceScopeClass::Ambiguous,
            id => SpliceScopeClass::Exact(id),
        },
        Err(_) => SpliceScopeClass::Unknown,
    }
}

/// Is `v` a scalar-replaced object inside an inlined caller CHAIN (`chain`)?
/// The in-place OSR transfers (`transfer_osr_exit_chain_into_live_frame`,
/// the planless guard exit) have no materialisation path, so OSR admission
/// (`osr_exit_policy`, through [`first_unresumable_slot`] /
/// [`first_unresumable_local`], whose other callers refuse every chain
/// anyway) refuses such a point rather than reaching a refusal after the body
/// committed iterations. [`frame_state_is_resumable`] is NOT narrowed: it
/// answers for the method-entry sinks, whose chain builder re-allocates such
/// an object. Vacuous unless [`chain_virtuals_enabled`] let one be published
/// (round 13 wave 5, lane resume).
fn chain_value_blocks_osr(chain: bool, v: &FrameValue) -> bool {
    chain
        && matches!(
            v,
            FrameValue::VirtualObject(_)
                | FrameValue::VirtualObjectRef(_)
                | FrameValue::ConditionalVirtualObject(_)
        )
}

/// How the chain sinks treat one value of an inlined caller chain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChainSlot {
    /// A constant or a resolved machine word: mapped directly.
    Plain,
    /// A scalar-replaced object (or a reference to one) the chain-wide
    /// materialiser can rebuild.
    Virtual,
    /// Nothing can rebuild it.
    Refused,
}

/// j27 wave 5 (lane STRPIN): does [`inlined_chain_refusal`] refuse `v` as
/// "a value the chain sinks cannot rebuild"? For the lowerer's debug line
/// naming the slot that made a required chain unpublishable.
pub(crate) fn chain_value_unrebuildable(v: &FrameValue) -> bool {
    chain_slot(v) == ChainSlot::Refused
}

/// [`ChainSlot`] of `v`; a virtual object's fields must each be a value the
/// VM materialiser stores (`deopt_materialize::validate_virtual_graph`: a
/// constant, a resolved word, `Undefined`, or another virtual object), and an
/// array must have a primitive element type.
fn chain_slot(v: &FrameValue) -> ChainSlot {
    match v {
        FrameValue::Int(_)
        | FrameValue::Long(_)
        | FrameValue::Float(_)
        | FrameValue::Double(_)
        | FrameValue::Object(_)
        | FrameValue::StackSlot(_)
        | FrameValue::StackSlotRef(_)
        | FrameValue::StackSlotLong(_)
        | FrameValue::StackSlotFloat(_)
        | FrameValue::StackSlotDouble(_)
        | FrameValue::Undefined => ChainSlot::Plain,
        FrameValue::Register(_)
        | FrameValue::RegisterLong(_)
        | FrameValue::RegisterRef(_)
        | FrameValue::XmmFloat(_)
        | FrameValue::XmmDouble(_) => {
            if names_unspilled_register(v) {
                ChainSlot::Refused
            } else {
                ChainSlot::Plain
            }
        }
        FrameValue::VirtualObject(state) => {
            // A primitive array, a reference array (atype 0, its component
            // class in `class_id`), or an auto-box: each one the VM
            // materialiser rebuilds (`deopt_materialize::validate_virtual_graph`).
            let shape_ok = state.num_fields == state.field_values.len()
                && state.array_element_type.is_none_or(|atype| {
                    atype == 0 || (4..=11).contains(&atype) || auto_box_value_of(atype).is_some()
                });
            if shape_ok
                && state
                    .field_values
                    .iter()
                    .all(|f| chain_slot(f) != ChainSlot::Refused)
            {
                ChainSlot::Virtual
            } else {
                ChainSlot::Refused
            }
        }
        FrameValue::ConditionalVirtualObject(pair) => {
            let (cond, state) = pair.as_ref();
            let shape_ok = state.num_fields == state.field_values.len()
                && state.array_element_type.is_none_or(|atype| {
                    atype == 0 || (4..=11).contains(&atype) || auto_box_value_of(atype).is_some()
                });
            if chain_slot(cond) == ChainSlot::Plain
                && shape_ok
                && state
                    .field_values
                    .iter()
                    .all(|f| chain_slot(f) != ChainSlot::Refused)
            {
                ChainSlot::Virtual
            } else {
                ChainSlot::Refused
            }
        }
        FrameValue::VirtualObjectRef(_) => ChainSlot::Virtual,
        FrameValue::MaterializationRequired(_) | FrameValue::Unsupported => ChainSlot::Refused,
    }
}

/// Why the chain's virtual-object ids cannot be materialised as one graph:
/// an id defined twice, or a `VirtualObjectRef` (top level or in a field) to
/// an id no scope defines. Ids are CHAIN-wide (see [`inlined_chain_refusal`]).
fn chain_virtual_ids_refusal(fs: &FrameState) -> Option<&'static str> {
    fn walk(v: &FrameValue, defined: &mut Vec<usize>, referenced: &mut Vec<usize>) -> bool {
        match v {
            FrameValue::VirtualObject(state) => {
                if defined.contains(&state.id) {
                    return true;
                }
                defined.push(state.id);
                state
                    .field_values
                    .iter()
                    .any(|f| walk(f, defined, referenced))
            }
            FrameValue::ConditionalVirtualObject(pair) => {
                let (_, state) = pair.as_ref();
                if defined.contains(&state.id) {
                    return true;
                }
                defined.push(state.id);
                state
                    .field_values
                    .iter()
                    .any(|f| walk(f, defined, referenced))
            }
            FrameValue::VirtualObjectRef(id) => {
                referenced.push(*id);
                false
            }
            _ => false,
        }
    }
    let mut defined: Vec<usize> = Vec::new();
    let mut referenced: Vec<usize> = Vec::new();
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        for v in f.locals.iter().chain(f.stack.iter()) {
            if walk(v, &mut defined, &mut referenced) {
                return Some("a chain defines one scalar-replaced object twice");
            }
        }
        seen += 1;
        if seen >= MAX_SCOPE_CHAIN {
            break;
        }
        scope = f.caller.as_deref();
    }
    if referenced.iter().any(|id| !defined.contains(id)) {
        return Some("a chain references a scalar-replaced object it does not define");
    }
    None
}

/// [`inlined_chain_refusal`] with the switch as a parameter, asked only of a
/// chain that names a scalar-replaced object (so the tests need no
/// environment).
fn inlined_chain_refusal_with(
    fs: &FrameState,
    virtuals_enabled: impl FnOnce() -> bool,
) -> Option<&'static str> {
    if fs.caller.is_none() {
        return Some("no caller scope");
    }
    let budget = max_osr_inline_resume_depth();
    if caller_chain_depth(fs) > budget {
        return Some("deeper than the VM's inline resume budget");
    }
    let mut names_virtual = false;
    let mut holds_monitor = false;
    let mut scope = Some(fs);
    let mut depth = 0usize;
    while let Some(f) = scope {
        for v in f.locals.iter().chain(f.stack.iter()) {
            match chain_slot(v) {
                ChainSlot::Plain => {}
                ChainSlot::Virtual => names_virtual = true,
                ChainSlot::Refused => {
                    return Some("a scope holds a value the chain sinks cannot rebuild")
                }
            }
        }
        holds_monitor |= !f.monitors.is_empty();
        let outermost = f.caller.is_none();
        if !f.monitors.is_empty() && !outermost {
            return Some("an inlined scope holds a monitor");
        }
        for m in &f.monitors {
            if m.relock || m.lock_depth == 0 {
                return Some("a held monitor would have to be re-acquired");
            }
            if !matches!(
                m.object,
                FrameValue::StackSlotRef(_) | FrameValue::RegisterRef(_)
            ) || names_unspilled_register(&m.object)
            {
                return Some("a held monitor's object is not a reference word");
            }
        }
        depth += 1;
        if depth > budget + 1 {
            return Some("deeper than the VM's inline resume budget");
        }
        scope = f.caller.as_deref();
    }
    if names_virtual {
        if !virtuals_enabled() {
            return Some("a scope holds a scalar-replaced object (CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS is off)");
        }
        if holds_monitor {
            return Some("a chain naming a scalar-replaced object holds a monitor");
        }
        if let Some(why) = chain_virtual_ids_refusal(fs) {
            return Some(why);
        }
    }
    None
}

/// `CRATONVM_JIT_OSR_GUARD_EXIT_VIRTUALS`, **default ON** since 2026-10-06 (the
/// for-each residual round; `=0` is the kill switch): may an optimizing OSR
/// body's guard or poll exit name a scalar-replaced object? The VM's planless
/// in-place transfer re-allocates such objects before it writes the live frame
/// (`deopt_resume::transfer_osr_guard_exit_into_live_frame`, flat and chain), so
/// `osr_exit::guard_exit_fact` admits the frame
/// ([`osr_in_place_virtuals_refusal`]) and escape analysis deletes a
/// loop-carried object in an OSR body as it does in a method-entry body
/// (`ea_ir_bridge`). Read by both ends, so they agree either way. Not memoised
/// (no new static): asked per box at compile time and once per exit taken.
pub fn osr_guard_exit_virtuals_enabled() -> bool {
    true
}

/// Why the in-place OSR exit transfer cannot re-allocate the scalar-replaced
/// objects `fs` names, or `None` when it names none or can: the switch
/// ([`osr_guard_exit_virtuals_enabled`]) is on, no scope holds a monitor (a
/// re-allocation that fails propagates `OutOfMemoryError` out of the
/// activation, as HotSpot pops the frames of a failed reallocation, and the
/// transfer has no path that releases a lock for it), every such object is one
/// the VM materialiser stores ([`chain_slot`]), and the ids form one graph
/// ([`chain_virtual_ids_refusal`]; a flat frame is a chain of one scope).
/// Every other value is [`frame_state_is_resumable`]'s question.
pub fn osr_in_place_virtuals_refusal(fs: &FrameState) -> Option<&'static str> {
    osr_in_place_virtuals_refusal_with(fs, osr_guard_exit_virtuals_enabled)
}

/// [`osr_in_place_virtuals_refusal`] with the switch as a parameter, asked
/// only of a frame that names a scalar-replaced object.
fn osr_in_place_virtuals_refusal_with(
    fs: &FrameState,
    enabled: impl FnOnce() -> bool,
) -> Option<&'static str> {
    let mut names_virtual = false;
    let mut holds_monitor = false;
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        for v in f.locals.iter().chain(f.stack.iter()) {
            if matches!(
                v,
                FrameValue::VirtualObject(_)
                    | FrameValue::VirtualObjectRef(_)
                    | FrameValue::ConditionalVirtualObject(_)
            ) {
                names_virtual = true;
                if chain_slot(v) == ChainSlot::Refused {
                    return Some(
                        "a scalar-replaced object has a field the materialiser cannot store",
                    );
                }
            }
        }
        holds_monitor |= !f.monitors.is_empty();
        seen += 1;
        if seen >= MAX_SCOPE_CHAIN {
            return Some("scope chain too deep");
        }
        scope = f.caller.as_deref();
    }
    if !names_virtual {
        return None;
    }
    if !enabled() {
        return Some("a scalar-replaced object (CRATONVM_JIT_OSR_GUARD_EXIT_VIRTUALS is off)");
    }
    if holds_monitor {
        return Some("a frame naming a scalar-replaced object holds a monitor");
    }
    chain_virtual_ids_refusal(fs)
}

/// `CRATONVM_JIT_OSR_EXCEPTION_EXIT_VIRTUALS` (OSR round, 2026-10-06;
/// **default ON**, `=0` is the kill switch): may an optimizing OSR body's
/// reason-9 rethrow pad name a scalar-replaced object in a local? The OSR
/// exception route's in-place transfer then re-allocates it before it enters
/// the method's handler (`deopt_resume::transfer_osr_exception_exit_into_live_frame`),
/// as the guard-exit transfer does under [`osr_guard_exit_virtuals_enabled`].
/// Only an OSR-door body's pads take that route; a method-entry body's are
/// read by `route_jit_signal_exception`, which re-allocates nothing, so the
/// lowerer asks this only of an OSR compile. Read by both ends, uncached.
pub fn osr_exception_exit_virtuals_enabled() -> bool {
    osr_guard_exit_virtuals_enabled()
        && true
}

/// Why the OSR exception route's in-place transfer cannot re-allocate the
/// scalar-replaced objects a reason-9 frame `fs` names, or `None` when it
/// names none or can: one scope, no monitor, every object one the materialiser
/// stores, and one id graph ([`osr_in_place_virtuals_refusal_with`] asked with
/// [`osr_exception_exit_virtuals_enabled`]).
pub fn osr_exception_exit_virtuals_refusal(fs: &FrameState) -> Option<&'static str> {
    if fs.caller.is_some() {
        return Some("an inlined caller scope");
    }
    osr_in_place_virtuals_refusal_with(fs, osr_exception_exit_virtuals_enabled)
}

/// Does any scope of `fs` name a scalar-replaced object in a local or on the
/// operand stack?
pub fn frame_state_names_virtual(fs: &FrameState) -> bool {
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        if f.locals.iter().chain(f.stack.iter()).any(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_)
                    | FrameValue::VirtualObjectRef(_)
                    | FrameValue::ConditionalVirtualObject(_)
            )
        }) {
            return true;
        }
        seen += 1;
        if seen >= MAX_SCOPE_CHAIN {
            return false;
        }
        scope = f.caller.as_deref();
    }
    false
}

/// Which slot makes [`frame_state_is_resumable`] answer `false` — `"local 3
/// (Unsupported)"`, `"stack 1 (MaterializationRequired)"`, `depth`-prefixed
/// when the offender is in an inlined caller scope. `None` iff the state is
/// resumable, with one OSR-admission addition (round 13 wave 5): a
/// scalar-replaced object anywhere in an inlined CHAIN is named too
/// ([`chain_value_blocks_osr`]).
///
/// Exists because the refusal it feeds used to read
/// `"reconstructs an unresumable frame"` and stop there. That names the
/// symptom and hides the cause: a frame is unresumable because ONE value has
/// no interpreter encoding, and which one it is decides the whole diagnosis —
/// an `Unsupported` **stack** entry means the operand stack had no width
/// source at that bci (the `uses_long_float_double` fallback in
/// `build_and_record_deopt_point`), while an `Unsupported` **local** means the
/// local's kind or liveness could not be established. Those are different
/// defects with different fixes, and the message could not tell them apart.
pub fn first_unresumable_slot(fs: &FrameState) -> Option<String> {
    let chain = fs.caller.is_some();
    let mut scope = Some(fs);
    let mut depth = 0usize;
    while let Some(f) = scope {
        let at = |depth: usize, what: &str, i: usize, v: &FrameValue| {
            let scope_tag = if depth == 0 {
                String::new()
            } else {
                format!("caller-scope-{depth} ")
            };
            format!("{scope_tag}{what} {i} ({v:?})")
        };
        for (i, v) in f.locals.iter().enumerate() {
            if value_blocks_resume(v) || chain_value_blocks_osr(chain, v) {
                return Some(at(depth, "local", i, v));
            }
        }
        for (i, v) in f.stack.iter().enumerate() {
            if value_blocks_resume(v) || chain_value_blocks_osr(chain, v) {
                // Depth matters as much as the index: "stack 0 of 1" is a call
                // whose own argument could not be typed, "stack 0 of 3" is a
                // value sitting UNDER the arguments, and the two want different
                // fixes.
                return Some(format!("{} of {}", at(depth, "stack", i, v), f.stack.len()));
            }
        }
        depth += 1;
        if depth >= MAX_SCOPE_CHAIN {
            return if f.caller.is_none() {
                None
            } else {
                Some(format!("scope chain deeper than {MAX_SCOPE_CHAIN}"))
            };
        }
        scope = f.caller.as_deref();
    }
    None
}

/// Every machine register `fs` names as the home of a value — GPRs as
/// `(n, false)`, XMMs as `(n, true)` — across the whole caller chain.
///
/// The reason-9 exceptional-frame stub reconstructs those homes by spilling the
/// live register file AT THE STUB and indexing it, so anything that runs
/// between the trapping instruction and that stub has to preserve every
/// register listed here. Compiled local handlers put a helper `CALL` on exactly
/// that edge, and this is how that emitter proves the call cannot disturb a
/// frame it may still have to fall through to.
///
/// **Monitor objects count, and used not to.** `MonitorInfo::object` is a
/// `FrameValue` like any other and `resolve_frame_state_machine` resolves it
/// against the same spilled register file (`monitors_resolve_register_homed_objects`
/// pins that), so a monitor held in a caller-saved register would be read back
/// as whatever the helper left there — a lock re-acquired on the wrong object,
/// which is the same defect as a mistyped local and a quieter one. Today no
/// producer puts a monitor in a register (the single-pass backend's
/// `build_frame_state_at` records `VirtualObjectRef` for every held monitor),
/// so this widening changes no answer; it stops the ONE consumer's proof from
/// resting on a set that does not cover every slot the resolver reads.
pub fn frame_state_register_homes(fs: &FrameState) -> Vec<(u8, bool)> {
    let mut out = Vec::new();
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        let monitor_objects = f.monitors.iter().map(|m| &m.object);
        for v in f.locals.iter().chain(f.stack.iter()).chain(monitor_objects) {
            match v {
                FrameValue::Register(r)
                | FrameValue::RegisterLong(r)
                | FrameValue::RegisterRef(r) => out.push((*r, false)),
                FrameValue::XmmFloat(x) | FrameValue::XmmDouble(x) => out.push((*x, true)),
                _ => {}
            }
        }
        seen += 1;
        if seen >= MAX_SCOPE_CHAIN {
            break;
        }
        scope = f.caller.as_deref();
    }
    out
}

/// [`first_unresumable_slot`], restricted to what a **RETHROW** point's
/// consumer actually reads.
///
/// A `PendingException` (reason-9) frame is not a resume image — this crate
/// says so in three places already — and it is not consumed like one. It is
/// stashed by [`take_exceptional_frame`] and claimed by
/// `route_jit_signal_exception` / `run_jit_callee_handler`, which read its
/// `bci` and its `locals` and then build a handler frame whose operand stack is
/// `[exception]` by JVMS §2.10. **The recorded operand stack is never read at
/// all**, so an entry in it that could not be typed describes nothing that will
/// ever be reconstructed.
///
/// Vetoing on it is not conservative, it is just wrong-sized, and it costs a
/// whole artifact its OSR entry: `osr_exit_policy` is artifact-wide, so ONE
/// untypeable stack entry at ONE throwing bci refuses entry at EVERY pc of the
/// method. The population is not exotic — any `catch` block containing a call,
/// in any method that touches a `long`/`float`/`double` (which makes every
/// non-oop operand `Unsupported` for want of a per-entry width source), and
/// `HttpHeaderValidationUtilTest`'s two exhaustive loops are exactly that.
///
/// Locals still veto, and must: the handler reads them, and entering one with
/// zeroed non-parameter locals is the silent miscompile
/// `route_jit_signal_exception` fails closed against.
pub fn first_unresumable_local(fs: &FrameState) -> Option<String> {
    let chain = fs.caller.is_some();
    let mut scope = Some(fs);
    let mut depth = 0usize;
    while let Some(f) = scope {
        for (i, v) in f.locals.iter().enumerate() {
            if value_blocks_resume(v) || chain_value_blocks_osr(chain, v) {
                let scope_tag = if depth == 0 {
                    String::new()
                } else {
                    format!("caller-scope-{depth} ")
                };
                return Some(format!("{scope_tag}local {i} ({v:?})"));
            }
        }
        depth += 1;
        if depth >= MAX_SCOPE_CHAIN {
            return if f.caller.is_none() {
                None
            } else {
                Some(format!("scope chain deeper than {MAX_SCOPE_CHAIN}"))
            };
        }
        scope = f.caller.as_deref();
    }
    None
}

/// Entries in each register file the deopt stub spills
/// ([`SavedRegisters::gpr`] and [`SavedRegisters::xmm`]).
const SPILLED_REGISTER_FILE_LEN: usize = 16;

/// `true` when `v` names a register (transitively, through virtual-object
/// fields) outside the files the deopt stub spills. [`try_resolve_value`]
/// would report such a value as `RegisterFileIndexOutOfRange` at exit time
/// and it would resume as `Unsupported`. Refusing it where resumability is
/// decided makes that exit-time fallback unreachable for an admitted frame
/// (review #88).
fn names_unspilled_register(v: &FrameValue) -> bool {
    match v {
        FrameValue::Register(r) | FrameValue::RegisterLong(r) | FrameValue::RegisterRef(r) => {
            usize::from(*r) >= SPILLED_REGISTER_FILE_LEN
        }
        FrameValue::XmmFloat(n) | FrameValue::XmmDouble(n) => {
            usize::from(*n) >= SPILLED_REGISTER_FILE_LEN
        }
        FrameValue::VirtualObject(state) => state.field_values.iter().any(names_unspilled_register),
        FrameValue::ConditionalVirtualObject(pair) => {
            names_unspilled_register(&pair.0)
                || pair.1.field_values.iter().any(names_unspilled_register)
        }
        _ => false,
    }
}

/// `true` when `v` cannot be turned into an interpreter value: either it is
/// itself unreconstructable, it names a register the deopt stub does not
/// spill, or it is a virtual object one of whose fields (transitively, without
/// crossing a `VirtualObjectRef` edge) names a value an optimization deleted.
fn value_blocks_resume(v: &FrameValue) -> bool {
    match v {
        FrameValue::Unsupported | FrameValue::MaterializationRequired(_) => true,
        FrameValue::VirtualObject(state) => {
            state
                .field_values
                .iter()
                .any(contains_materialization_required)
                || names_unspilled_register(v)
        }
        FrameValue::ConditionalVirtualObject(pair) => {
            value_blocks_resume(&pair.0)
                || pair
                    .1
                    .field_values
                    .iter()
                    .any(contains_materialization_required)
                || names_unspilled_register(v)
        }
        other => names_unspilled_register(other),
    }
}

/// `true` when `v` is — or transitively contains as a virtual-object field —
/// a [`FrameValue::MaterializationRequired`].
fn contains_materialization_required(v: &FrameValue) -> bool {
    match v {
        FrameValue::MaterializationRequired(_) => true,
        FrameValue::VirtualObject(state) => state
            .field_values
            .iter()
            .any(contains_materialization_required),
        FrameValue::ConditionalVirtualObject(pair) => {
            contains_materialization_required(&pair.0)
                || pair
                    .1
                    .field_values
                    .iter()
                    .any(contains_materialization_required)
        }
        _ => false,
    }
}

/// How many slots of `fs` name a value an optimization deleted without leaving
/// a materialization recipe ([`FrameValue::MaterializationRequired`]).
///
/// The compiler-report counterpart of [`count_virtual_objects`]: that one
/// counts eliminations the deopt path *can* undo, this one counts the ones it
/// cannot. A non-zero count means every deopt at this point costs a
/// whole-method re-run, which is the signal that a producer needs to start
/// emitting a `VirtualObject` for that shape.
pub fn count_materialization_required(fs: &FrameState) -> usize {
    fn count_in(values: &[FrameValue]) -> usize {
        values
            .iter()
            .map(|v| match v {
                FrameValue::MaterializationRequired(_) => 1,
                FrameValue::VirtualObject(state) => count_in(&state.field_values),
                FrameValue::ConditionalVirtualObject(pair) => count_in(&pair.1.field_values),
                _ => 0,
            })
            .sum()
    }
    count_in(&fs.locals) + count_in(&fs.stack)
}

/// Lock/monitor state for a single object.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub object: FrameValue,
    pub lock_depth: u32,
    /// Must the resume ACQUIRE this monitor (`lock_depth` times) on the
    /// resuming thread?
    ///
    /// `true` for a lock the compiled code never took: escape analysis elided
    /// its `monitorenter`/`monitorexit` (typically over a scalar-replaced
    /// object, which the resume materializes first). The interpreter frame
    /// will run the matching `monitorexit`, so the lock has to exist — this is
    /// HotSpot's relock-eliminated-locks-on-deopt. Every producer before
    /// 2026-09-12 (the single-pass backend's scalar-monitor snapshots) emitted
    /// only such locks, and they carry `true`.
    ///
    /// `false` for a lock the compiled code took through the monitor helper:
    /// the thread still holds it when the frame deoptimizes, and acquiring it
    /// again would leave it held one level too deep after the interpreter's
    /// `monitorexit`. The optimizing tier's frame states name these since they
    /// began recording the builder's monitor stack.
    pub relock: bool,
}

/// Complete interpreter frame state at a deopt point.
#[derive(Debug, Clone)]
pub struct FrameState {
    /// `ClassId` of the class `method_key` names, or `0` when unknown/root.
    pub class_id: u32,
    /// Fully-qualified method key (class + name + descriptor).
    pub method_key: String,
    /// Bytecode index to resume at.
    pub bci: u32,
    /// Local variable values.
    pub locals: Vec<FrameValue>,
    /// Operand stack values.
    pub stack: Vec<FrameValue>,
    /// Held monitors.
    pub monitors: Vec<MonitorInfo>,
    /// Caller frame (for inlined methods).
    pub caller: Option<Box<FrameState>>,
}

// ---------------------------------------------------------------------------
// Per-bci de-speculation registry (deopt-osr Step 9 follow-up c)
// ---------------------------------------------------------------------------

/// One VM's set of `(method_key, bci, speculation_id)` speculation sites that have deopted past
/// the per-bci give-up threshold and must NOT be re-speculated on the next
/// compilation — "only de-spec the speculation that failed instead of
/// whole-method eviction." The VM's real-frame-deopt de-spec path
/// (`vm/src/runtime/interpreter/deopt_resume.rs`) inserts into it; the compiler
/// reads it when deciding whether to emit a speculative guard (loop-header BCE
/// guards and LICM hoists in `x64::compile_with_param_slots`, the arraycopy
/// intrinsic in `x64::bytecode_walk`, and the guarded String receiver
/// intrinsics in `try_compile_inner` and the VM's OSR door).
///
/// # Per VM, not per process
///
/// This was a process-global `static` until 2026-09-12. A despeculation is a
/// verdict about what ONE VM's program did at a bci, so a second VM in the same
/// process (an embedded VM, or a test building two) inherited verdicts it never
/// earned and compiled without speculations its own profile supported. The VM
/// now owns one registry (`JitRealm::despec_registry`, behind an `Arc` so a
/// compile can hold it for its whole duration) and threads it into every
/// compile request as `Option<&Arc<DespecRegistry>>`. `None` — the legacy
/// `try_compile` / `x64::compile` wrappers and crate fixtures with no VM —
/// consults nothing, which is what the empty process registry answered for
/// them. See `jit-compatibility-and-despec-state-per-vm-FIXED.md`.
///
/// # Keyed by speculation, not by bci alone — `NOTES-deopt2.md` M4
///
/// One bci can carry TWO speculations. `jit/src/osr_exit.rs` documents that as
/// the ordinary shape of a protected invoke, not an exotic one: a
/// `ReceiverTypeChanged` guard (`reexecute`) and a `PendingException` frame
/// (`rethrow`) are published at the same bytecode index. With a `(method, bci)`
/// key, four traps on either of them withdrew BOTH — so an exceptional frame
/// that kept firing also silently stripped a receiver guard that had never
/// failed, and vice versa.
///
/// The third key component is [`DeoptimizationPoint::speculation_id`], derived
/// from `(bci, reason)` by [`speculation_id`]. `0` is reserved and means "the
/// producer could not say which speculation this was": it is stored as an
/// entry like any other and is treated as a WILDCARD by
/// [`Self::contains_speculation`], so a mixed tree stays well defined rather
/// than merely tolerated. That direction is the safe one: a wildcard
/// suppresses too much, which costs an optimisation, and never too little,
/// which would leave a guard trapping forever.
///
/// Every producer that knows its own `(bci, reason)` now derives an id: the
/// single-pass backend (`x64/deopt_stubs.rs`), all three IR-tier producers
/// (`ir_lower.rs` — the `Op::Guard` arm, `emit_deopt_unless`, and the
/// safepoint resume points) and the VM's `DeoptEvent`
/// (`vm/src/jit/helpers.rs`). The wildcard is written only where the pair
/// genuinely cannot be recovered — the `Ambiguous` / `None` arms of the
/// de-spec recording in `vm/src/runtime/interpreter/deopt_resume.rs`, a
/// consumer-side insert with no point in hand — and in test fixtures that do
/// not exercise this field.
///
/// # The data structure
///
/// One `std::sync::RwLock` over a two-level map, method key → bci → the
/// speculation ids recorded there; a poisoned lock reads as "not de-spec'd".
/// Keyed by the same `"<class>.<method>:<descriptor>"` string the deopt log /
/// `method_epochs` use.
///
/// Two levels rather than one flat `FxHashSet<(String, u32, u32)>` (the shape
/// until r9): the flat set could not be probed by `(&str, bci)` without
/// building an owned key, so every lookup was a linear scan of EVERY entry
/// the VM had ever recorded, for every method — and the emitters ask once per
/// speculative guard they consider. With the method key as the outer map key
/// a lookup is one `&str`-borrowed hash probe plus a probe by bci, whatever
/// the registry's size, and still allocation-free.
#[derive(Debug, Default)]
pub struct DespecRegistry {
    map: std::sync::RwLock<FxHashMap<String, FxHashMap<u32, Vec<u32>>>>,
    /// JIT round 20 wave 12, lane qNull (L1W19-2,
    /// `CRATONVM_JIT_NULL_TRAP_RECOMPILE`): translated implicit-null faults
    /// per `(method_key, bci)` that have not yet reached the per-bci trap
    /// limit -- the counting half of [`Self::note_implicit_null_trap`]. Same
    /// two-level shape and key as `map`, same owner (this VM), same unload
    /// eviction ([`Self::forget_classes`]); empty unless a single-pass
    /// implicit-null site's receiver was null.
    implicit_null_traps: std::sync::Mutex<FxHashMap<String, FxHashMap<u32, u32>>>,
}

impl DespecRegistry {
    /// An empty registry: nothing is de-spec'd.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `(method_key, bci, speculation_id)` as a failed speculation site
    /// that must not be re-speculated. Idempotent.
    ///
    /// `speculation_id` is [`SPECULATION_ID_ANY`] when the caller could not
    /// recover WHICH speculation at `bci` failed — an ambiguous reason lookup,
    /// or a producer that has not been taught the derivation. See the type doc
    /// for why that is the conservative direction.
    pub fn insert_if_absent(&self, method_key: &str, bci: u32, speculation_id: u32) -> bool {
        if method_key.is_empty() {
            return false;
        }
        if let Ok(mut guard) = self.map.write() {
            let entries = guard
                .entry(method_key.to_string())
                .or_default()
                .entry(bci)
                .or_default();
            if entries.contains(&speculation_id) || entries.contains(&SPECULATION_ID_ANY) {
                return false;
            }
            entries.push(speculation_id);
            return true;
        }
        false
    }

    pub fn insert(&self, method_key: &str, bci: u32, speculation_id: u32) {
        if let Ok(mut m) = self.map.write() {
            // Probe before inserting so the owned key is built only for a
            // method's first entry. Two lookups rather than a `match` on
            // `get_mut` because that shape needs a borrow the checker cannot
            // see is disjoint (the same reason `DeoptimizationLog::record_deopt`
            // is written this way).
            if !m.contains_key(method_key) {
                m.insert(method_key.to_string(), FxHashMap::default());
            }
            if let Some(per_bci) = m.get_mut(method_key) {
                let ids = per_bci.entry(bci).or_default();
                // Idempotent, as the flat set was: one entry per id.
                if !ids.contains(&speculation_id) {
                    ids.push(speculation_id);
                }
            }
        }
    }

    /// `true` if ANY speculation at `(method_key, bci)` was recorded as failed.
    ///
    /// The coarse question, and the right one for a consumer that is reasoning
    /// about the whole bytecode index rather than about one guard — the two
    /// loop-hoist gates in `x64/driver.rs` (a de-spec'd loop header means the
    /// hoist is withdrawn whatever failed there) and
    /// `DeoptimizationLog::recommend_action_at_bci` (which asks whether the
    /// trapping site has already been given up on at all). A consumer deciding
    /// whether to emit ONE guard must ask [`Self::contains_speculation`]
    /// instead.
    ///
    /// An empty `method_key` never matches (the `x64::compile()` legacy/test
    /// wrapper passes `""`).
    ///
    /// Fast path: when the registry is empty (nothing ever de-spec'd) this
    /// returns `false` after a cheap `is_empty` check, WITHOUT the
    /// `method_key.to_string()` lookup allocation, so consulting it per
    /// speculative guard during normal compilation is allocation-free.
    pub fn contains(&self, method_key: &str, bci: u32) -> bool {
        self.with_entries_at(method_key, bci, |mut entries| entries.next().is_some())
    }

    /// `true` if THIS speculation at `(method_key, bci)` was recorded as
    /// failed, or if a wildcard ([`SPECULATION_ID_ANY`]) entry was recorded
    /// there.
    ///
    /// This is the question an emitter asks before putting one guard down, and
    /// the whole point of M4: at a protected invoke, four traps on the
    /// exceptional frame must not take the receiver guard with them.
    pub fn contains_speculation(&self, method_key: &str, bci: u32, speculation_id: u32) -> bool {
        self.with_entries_at(method_key, bci, |entries| {
            // A plain loop, not `.any(..)`: `entries` is a `&mut dyn Iterator`,
            // and `Iterator::any` carries a `Self: Sized` bound that the
            // unsized receiver cannot satisfy. Iterating is equivalent and
            // short-circuits the same way.
            for id in entries {
                if id == speculation_id || id == SPECULATION_ID_ANY {
                    return true;
                }
            }
            false
        })
    }

    /// Run `f` over the speculation ids recorded at `(method_key, bci)`.
    ///
    /// Factored out so the empty-registry fast path and the empty-key rule are
    /// stated once: both `contains` and `contains_speculation` are consulted
    /// per speculative guard during codegen, and neither may allocate on the
    /// overwhelmingly common "nothing has ever de-spec'd" path.
    fn with_entries_at<R>(
        &self,
        method_key: &str,
        bci: u32,
        f: impl FnOnce(&mut dyn Iterator<Item = u32>) -> R,
    ) -> R
    where
        R: Default,
    {
        if method_key.is_empty() {
            return R::default();
        }
        let map = match self.map.read() {
            Ok(m) => m,
            Err(_) => return R::default(),
        };
        if map.is_empty() {
            return R::default();
        }
        let ids: &[u32] = match map.get(method_key).and_then(|per_bci| per_bci.get(&bci)) {
            Some(ids) => ids.as_slice(),
            None => return R::default(),
        };
        let mut ids = ids.iter().copied();
        f(&mut ids)
    }

    /// Every `(bci, speculation id)` recorded for `method_key`, in no
    /// particular order (JIT round 19 wave 3, lane splice3, SPLICE2-2: the
    /// planner's expected-throw rows, `ir::splice_expected_throw_rows`, which
    /// match EXACT ids and so must not read a wildcard as a match). Empty for
    /// an unknown key, an empty key or a poisoned lock.
    pub fn entries_for(&self, method_key: &str) -> Vec<(u32, u32)> {
        if method_key.is_empty() {
            return Vec::new();
        }
        self.map
            .read()
            .map(|m| {
                m.get(method_key).map_or_else(Vec::new, |per_bci| {
                    per_bci
                        .iter()
                        .flat_map(|(&bci, ids)| ids.iter().map(move |&id| (bci, id)))
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// Number of recorded de-spec entries for `method_key` (diagnostics /
    /// tests). Two speculations at one bci count as two.
    pub fn count_for(&self, method_key: &str) -> usize {
        self.map
            .read()
            .map(|m| {
                m.get(method_key)
                    .map_or(0, |per_bci| per_bci.values().map(Vec::len).sum())
            })
            .unwrap_or(0)
    }

    /// Forget every de-speculation recorded for a method of the classes named
    /// in `class_names`, in one pass; returns how many methods were dropped.
    /// Called by the VM's class-unload transaction with the unloaded set
    /// (gc-common w36-c).
    ///
    /// The registry had no unload eviction, so it kept one row per distinct
    /// de-spec'd method of every unloaded class for the life of the VM, and a
    /// class redeployed under the same name inherited the old bytecode's
    /// per-bci verdicts. Keyed by name, like [`DeoptimizationLog::clear_class`]
    /// (and matched on the key's class field the same way): another loader's
    /// live class of the same name loses its rows too, which costs it at most
    /// one more trap per site before it records them again -- forgetting a
    /// de-speculation only re-enables a guard, it never skips one.
    pub fn forget_classes(&self, class_names: &FxHashSet<&str>) -> usize {
        if class_names.is_empty() {
            return 0;
        }
        let Ok(mut m) = self.map.write() else {
            return 0;
        };
        let before = m.len();
        m.retain(|method, _| {
            !method_key_class(method).is_some_and(|class| class_names.contains(class))
        });
        // L1W19-2: the pending implicit-null fault counts go with the class,
        // on the same key rule (a redeployed class starts counting afresh).
        if let Ok(mut traps) = self.implicit_null_traps.lock() {
            traps.retain(|method, _| {
                !method_key_class(method).is_some_and(|class| class_names.contains(class))
            });
        }
        before - m.len()
    }

    /// JIT round 20 wave 12, lane qNull (L1W19-2,
    /// `CRATONVM_JIT_NULL_TRAP_RECOMPILE`): count one translated implicit-null
    /// fault at `(method_key, bci)` and, on the `limit`-th, record
    /// [`implicit_null_trap_speculation_id`] there, so the method's next
    /// single-pass compile keeps an explicit `TEST`/`JZ` at that `getfield`
    /// (`x64::Compiler::null_check_explicit_after_traps`). HotSpot's
    /// `too_many_traps(Reason_null_check)` at `PerBytecodeTrapLimit`.
    ///
    /// Returns `true` exactly once per site -- on the call that recorded the
    /// entry -- which is when the caller asks for the recompile. Every other
    /// call (below the limit, or the site already recorded because faults keep
    /// arriving from the old body until it is replaced) returns `false`. An
    /// empty key or a poisoned lock never records.
    pub fn note_implicit_null_trap(&self, method_key: &str, bci: u32, limit: u32) -> bool {
        if method_key.is_empty() {
            return false;
        }
        let id = implicit_null_trap_speculation_id(bci);
        if self.contains_exact(method_key, bci, id) {
            return false;
        }
        let reached = {
            let Ok(mut traps) = self.implicit_null_traps.lock() else {
                return false;
            };
            if !traps.contains_key(method_key) {
                traps.insert(method_key.to_string(), FxHashMap::default());
            }
            let Some(per_bci) = traps.get_mut(method_key) else {
                return false;
            };
            let count = per_bci.entry(bci).or_insert(0);
            *count = count.saturating_add(1);
            if *count < limit.max(1) {
                return false;
            }
            // The verdict now lives in `map`; the counter has done its job.
            per_bci.remove(&bci);
            if per_bci.is_empty() {
                traps.remove(method_key);
            }
            true
        };
        if reached {
            self.insert(method_key, bci, id);
        }
        reached
    }

    /// Has [`Self::note_implicit_null_trap`] recorded `(method_key, bci)`?
    /// The emitter's question: an EXACT id match, so neither a wildcard nor a
    /// guard's own id at the same bci turns an implicit check explicit.
    pub fn implicit_null_trapped(&self, method_key: &str, bci: u32) -> bool {
        self.contains_exact(method_key, bci, implicit_null_trap_speculation_id(bci))
    }

    /// `true` when exactly `speculation_id` is recorded at `(method_key, bci)`
    /// -- no wildcard reading, unlike [`Self::contains_speculation`].
    fn contains_exact(&self, method_key: &str, bci: u32, speculation_id: u32) -> bool {
        self.with_entries_at(method_key, bci, |entries| {
            for id in entries {
                if id == speculation_id {
                    return true;
                }
            }
            false
        })
    }
}

/// Reserved [`DeoptimizationPoint::speculation_id`] meaning "no id assigned":
/// the producer of this point could not say which of the speculations at its
/// bci it describes.
///
/// No compiler producer writes it any more: every one of them holds the
/// `(bci, reason)` its point publishes and stamps [`speculation_id`] instead.
/// It survives as the value a CONSUMER records when it cannot recover which
/// point trapped — `vm/src/runtime/interpreter/deopt_resume.rs`'s per-bci
/// de-spec, whose reason lookup can answer `Ambiguous` or `None` — and as the
/// don't-care in test fixtures. A mixed tree is therefore well defined rather
/// than merely tolerated: [`DespecRegistry::contains_speculation`] treats a
/// stored `0` as a wildcard, which is the coarse per-bci behaviour the whole
/// registry had before M4.
pub const SPECULATION_ID_ANY: u32 = 0;

/// The `(bci, reason)` derivation of [`DeoptimizationPoint::speculation_id`].
///
/// Lives here rather than in the backend that stamps it
/// (`x64/deopt_stubs.rs::speculation_id_for`, which now delegates) because the
/// CONSUMER has to derive the same value: the VM records a de-spec from the
/// reason it recovered at the trapping bci, and the emitter asks about the
/// speculation it is about to put down. Two copies of this function would be
/// two ways to spell one id.
///
/// Deliberately `(bci, reason)` and **not** a counter. A loop rewrite emits the
/// loop body more than once, and `x64/loop_rewrite.rs::deopt_point_difference`
/// treats a `speculation_id` mismatch between two images of one bytecode as
/// `PointDifference::Fatal` — an index-derived id would refuse every loop
/// rewrite in the tree. `(bci, reason)` is also the key that function's caller
/// already groups by, so two points that reach the comparison agree on the
/// field by construction.
///
/// Never returns [`SPECULATION_ID_ANY`]: a real derivation that hashed to `0`
/// would read as "no id assigned" and suppress every other speculation at its
/// bci, so it is nudged to `1`.
pub fn speculation_id(bci: u32, reason: DeoptReason) -> u32 {
    // `DeoptReason` is a C-like enum and is cast to an integer elsewhere
    // (`jit/tests/x64_artifact_corpus.rs` hashes `d.reason as i64`), so the
    // discriminant is the stable spelling of "which kind of speculation".
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write_u32(bci);
    h.write_u32(reason as u32);
    // Truncating: a 64-bit hash to the field's 32 bits.
    let id = h.finish() as u32;
    if id == SPECULATION_ID_ANY {
        1
    } else {
        id
    }
}

/// `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION` (JIT round 21 wave 1, lane
/// loops, LOOPS21-2) -- perf, **default OFF** (opt-in `=1`). Every
/// loop-entry predicate planter asked the COARSE de-spec question
/// ([`DespecRegistry::contains`] at the loop header's bci), and every guard
/// it planted was an `Op::Guard { bci: header }` whose point carried the
/// shared `(header, UncommonTrap)` id, so four traps of ANY one of them (a
/// heap-profiled buffer class guard meeting a direct buffer, say) withdrew
/// every predicate at that header from the next compile. Under the switch
/// each planter's guard carries [`predicate_speculation_id`] of its own
/// [`PredicateKind`] (`ir_lower`'s `Op::Guard` arm, from
/// `Graph::predicate_guard_kinds`), the VM records that id when the guard
/// traps (the own-stash sinks read it from the point; the trapped-callee
/// sink names it explicitly), and each planter is withdrawn by its own id
/// or by any id that is no planter's ([`predicate_withdrawn`]); only another
/// planter's own id leaves it standing. A wildcard entry still withdraws
/// everything.
/// Read once per planter call; no `static`.
pub fn predicate_own_speculation_enabled() -> bool {
    false
}

/// LOOPS21-2: which loop-entry planter put a predicate guard down. The
/// discriminant is the stable spelling stored in
/// `Graph::predicate_guard_kinds` and hashed into
/// [`predicate_speculation_id`]; never renumber a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PredicateKind {
    /// `ir_optimize::plant_range_check_predicates` (counted-loop range
    /// predicates; `install_range_predicates` from that planter).
    Range = 1,
    /// `ir_optimize::plant_clamp_length_predicates`.
    ClampLength = 2,
    /// `ir_optimize::plant_length_predicates` (and the receiver test it moves).
    Length = 3,
    /// `ir_optimize::mask_predicate` (early and late plants).
    Mask = 4,
    /// `ir_optimize::monotone_predicate`.
    Monotone = 5,
    /// `ir_optimize::offset_predicate`.
    Offset = 6,
    /// `ir_optimize::class_predicate`.
    Class = 7,
    /// `ir_optimize::plant_null_base_load_predicates` (JIT round 23 wave 2d,
    /// lane L1, TCL23-1).
    NullBase = 8,
    /// JIT round 23 wave 4 (lane L1, CF23-2): a never-taken branch pruned
    /// INSIDE a spliced callee (`IrBuilder::prune_never_taken_branch`,
    /// `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`). Not a loop-entry predicate; it
    /// shares the chain id machinery ([`chain_predicate_speculation_id`]) that
    /// charges a spliced guard at its call site.
    UnstableIf = 9,
    /// JIT round 23 wave 5 (lane L1, TCL23-3):
    /// `ir_optimize::plant_checkcast_predicates`.
    CheckCast = 10,
    /// TLGET21-3: `ir_optimize::tl_chain_predicate` (a `ThreadLocal.get`
    /// chain moved to its loop's pre-header).
    ThreadLocal = 11,
}

impl PredicateKind {
    /// Every kind, for [`is_predicate_speculation_id`].
    pub const ALL: [PredicateKind; 11] = [
        PredicateKind::Range,
        PredicateKind::ClampLength,
        PredicateKind::Length,
        PredicateKind::Mask,
        PredicateKind::Monotone,
        PredicateKind::Offset,
        PredicateKind::Class,
        PredicateKind::NullBase,
        PredicateKind::UnstableIf,
        PredicateKind::CheckCast,
        PredicateKind::ThreadLocal,
    ];

    /// The kind stored as `raw` in `Graph::predicate_guard_kinds`.
    pub fn from_u8(raw: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|k| *k as u8 == raw)
    }
}

/// LOOPS21-2: the speculation id of planter `kind`'s guard at `bci`. A hash
/// of `(bci, a tag no `DeoptReason` discriminant reaches, kind)`, so it never
/// equals a [`speculation_id`] of the same bci by construction of the input
/// (only by a 32-bit collision), and never [`SPECULATION_ID_ANY`].
pub fn predicate_speculation_id(bci: u32, kind: PredicateKind) -> u32 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write_u32(bci);
    // A reason slot no `DeoptReason as u32` takes, then the kind.
    h.write_u32(0x5052_4544); // "PRED"
    h.write_u32(kind as u32);
    // Truncating: a 64-bit hash to the field's 32 bits.
    let id = h.finish() as u32;
    if id == SPECULATION_ID_ANY {
        1
    } else {
        id
    }
}

/// LOOPS21-2: is `id` some planter's [`predicate_speculation_id`] at `bci`?
/// The VM's trapped-callee sink asks it to record a named id rather than the
/// wildcard it records for an unnamed `UncommonTrap`.
pub fn is_predicate_speculation_id(bci: u32, id: u32) -> bool {
    id != SPECULATION_ID_ANY
        && PredicateKind::ALL
            .into_iter()
            .any(|k| predicate_speculation_id(bci, k) == id)
}

/// JIT round 23 wave 4 (lane L1): the speculation id of planter `kind`'s
/// loop-entry guard for a loop SPLICED from a callee -- a guard whose frame is
/// an inlined chain whose innermost scope is `scope_key` (the callee's
/// `"class.name:descriptor"`) parked at its own loop header `bci`. The id the
/// lowering gives such a chain point (`ir_lower`'s `Op::Guard` arm, from
/// `Graph::chain_predicate_guards`), the VM's chain sinks record at the
/// outermost call site (`deopt_resume::chain_predicate_charge_id`), and the
/// planter asks at that call site ([`chain_predicate_withdrawn`]). A
/// domain-tagged FxHash over the pair and the kind, deterministic across runs;
/// never [`SPECULATION_ID_ANY`] and never a [`predicate_speculation_id`] or
/// [`speculation_id`] by construction of the input (only by a 32-bit
/// collision).
pub fn chain_predicate_speculation_id(scope_key: &str, bci: u32, kind: PredicateKind) -> u32 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write(b"chain-predicate\0");
    h.write(scope_key.as_bytes());
    h.write_u32(bci);
    h.write_u32(kind as u32);
    // Truncating: a 64-bit hash to the field's 32 bits.
    let id = h.finish() as u32;
    if id == SPECULATION_ID_ANY {
        1
    } else {
        id
    }
}

/// Is `id` some planter's [`chain_predicate_speculation_id`] for the scope
/// `scope_key` at `bci`?
pub fn is_chain_predicate_speculation_id(scope_key: &str, bci: u32, id: u32) -> bool {
    id != SPECULATION_ID_ANY
        && !scope_key.is_empty()
        && PredicateKind::ALL
            .into_iter()
            .any(|k| chain_predicate_speculation_id(scope_key, bci, k) == id)
}

/// JIT round 25 (item 4 residual 1, spliced withdrawal): the speculation id
/// of the deopting compactness guard (`ir_lower`'s
/// `emit_compact_receiver_trap`, `DeoptReason::SpeculationFailed`) at bci
/// `bci` of the spliced body `scope_key` (the chain's innermost scope,
/// `"class.name:descriptor"`). A chain trap of that guard is charged at the
/// OUTERMOST call site under this id (`deopt_resume::chain_named_charge_id`),
/// and the next compile keeps the full field sequence at that one spliced
/// site (`ir_optimize::proven_compact_withdrawn_pcs`) instead of losing the
/// whole splice to the call site's wildcard. A domain-tagged FxHash,
/// deterministic across runs; never [`SPECULATION_ID_ANY`].
pub fn chain_compact_speculation_id(scope_key: &str, bci: u32) -> u32 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write(b"chain-compact\0");
    h.write(scope_key.as_bytes());
    h.write_u32(bci);
    // Truncating: a 64-bit hash to the field's 32 bits.
    let id = h.finish() as u32;
    if id == SPECULATION_ID_ANY {
        1
    } else {
        id
    }
}

/// `CRATONVM_JIT_PROVEN_COMPACT_ONE_TRAP` (the r23 L8 heap-buffer close;
/// **default ON**, `=0` is the kill switch): a deopting compactness guard
/// (`DeoptReason::SpeculationFailed`, the only reason compiled code stamps
/// for it) is withdrawn after ONE charged trap at its site instead of
/// `PER_BCI_DESPEC_LIMIT`, and the recompile withdraws it at every field
/// site on the same receiver node (`ir_optimize::proven_compact_withdrawn_pcs`).
///
/// Why: compactness is fixed at allocation, so a legacy object that failed
/// the guard once fails it every time, at every site on it. Under the
/// per-site, four-trap rule a legacy receiver walked the guard from field to
/// field, four traps at each, until the method's charged count reached
/// `MakeNotCompilable` -- `HeapByteBuffer.getInt()` then ran interpreted for
/// the rest of the run (`R23W2L8HeapBB heapbb`, 5 us -> 120 us per call).
/// Read per trap and once per compile; no static.
pub fn proven_compact_one_trap_enabled() -> bool {
    true
}

/// Item 4 residual 1: is `id` the compactness guard's chain id
/// ([`chain_compact_speculation_id`]) of the scope `scope_key` at `bci`?
pub fn is_chain_compact_speculation_id(scope_key: &str, bci: u32, id: u32) -> bool {
    id != SPECULATION_ID_ANY
        && !scope_key.is_empty()
        && chain_compact_speculation_id(scope_key, bci) == id
}

/// Item 4 residual 1: has the registry withdrawn the compactness guard at bci
/// `bci` of the spliced scope `scope_key`, as recorded at the outermost call
/// site `call_site_bci` of `method_key`? Its own chain id, or the call site's
/// wildcard (which normally drops the whole splice in the planner,
/// `ir_splice_site_withdrawn`; with that planner rule switched off the splice
/// stays and its guards must not be planted again).
pub fn chain_compact_withdrawn(
    despec: &DespecRegistry,
    method_key: &str,
    call_site_bci: u32,
    scope_key: &str,
    bci: u32,
) -> bool {
    despec.contains_speculation(
        method_key,
        call_site_bci,
        chain_compact_speculation_id(scope_key, bci),
    )
}

/// Has the registry withdrawn planter `kind`'s loop-entry predicate of a
/// SPLICED loop (innermost scope `scope_key` at its header `bci`), as recorded
/// at the outermost call site `call_site_bci` of `method_key` (the compiling
/// method)? Its own named id ([`chain_predicate_speculation_id`]) or the
/// wildcard, the call site's whole-splice withdrawal. Unlike
/// [`predicate_withdrawn`], another speculation recorded at the call site
/// (a nested trap's named id, an expected throw) leaves it standing: those
/// name other guards of the same splice.
pub fn chain_predicate_withdrawn(
    despec: &DespecRegistry,
    method_key: &str,
    call_site_bci: u32,
    scope_key: &str,
    bci: u32,
    kind: PredicateKind,
) -> bool {
    let own = chain_predicate_speculation_id(scope_key, bci, kind);
    despec.with_entries_at(method_key, call_site_bci, |entries| {
        // A plain loop: `Iterator::any` needs `Self: Sized` (see
        // `contains_speculation`).
        for id in entries {
            if id == own || id == SPECULATION_ID_ANY {
                return true;
            }
        }
        false
    })
}

/// LOOPS21-2: has the registry withdrawn planter `kind`'s predicate at
/// `(method_key, bci)`? With [`predicate_own_speculation_enabled`] off, the
/// coarse [`DespecRegistry::contains`] (any speculation at the bci), which is
/// every planter's question before the switch; on, this planter's own id, a
/// wildcard, or any id that is NOT another planter's predicate id.
///
/// JIT round 21 wave 5 (review lane rev-ir): the last clause. A planter's
/// pre-header holds more than its `Op::Guard`s: the `a.length` read every
/// length-based planter puts there deopts at the header bci on a null array
/// with its own `(bci, NullCheck)` (or implicit-null) id, which no planter's
/// predicate id names. Asking only `contains_speculation(own id)` left such a
/// trap unseen, so the next compile planted the same read again and trapped
/// again (`plant_length_predicates`' "a NullCheck withdrawn at the header
/// withdraws the predicate" no longer held). Only ANOTHER planter's own id
/// leaves this planter standing, which is the switch's whole point.
pub fn predicate_withdrawn(
    despec: &DespecRegistry,
    method_key: &str,
    bci: u32,
    kind: PredicateKind,
) -> bool {
    if predicate_own_speculation_enabled() {
        let own = predicate_speculation_id(bci, kind);
        despec.with_entries_at(method_key, bci, |entries| {
            for id in entries {
                if id == own || !is_predicate_speculation_id(bci, id) {
                    return true;
                }
            }
            false
        })
    } else {
        despec.contains(method_key, bci)
    }
}

/// JIT round 19 wave 3 (lane splice3, SPLICE2-2 = TF15-1): the reasons whose
/// guard, when it fails, makes the re-executed bytecode THROW (a null
/// dereference, an index out of range, a zero divisor) -- the guards a spliced
/// callee may keep failing on an expected exception path.
pub const EXPECTED_THROW_REASONS: [DeoptReason; 3] = [
    DeoptReason::NullCheck,
    DeoptReason::BoundsCheck,
    DeoptReason::DivByZero,
];

/// Is `reason` one of [`EXPECTED_THROW_REASONS`]?
pub fn is_expected_throw_reason(reason: DeoptReason) -> bool {
    EXPECTED_THROW_REASONS.contains(&reason)
}

/// SPLICE2-2: the de-spec registry id that names "the guard of `reason` at
/// `bci` of this (spliced callee's) method is an EXPECTED throw": recorded by
/// the VM under the CALLEE's key at its own bci, read by the IR planner
/// (`ir::splice_expected_throw_rows`). Salted so it is never the
/// [`speculation_id`] of the same `(bci, reason)`: the callee's own compiles
/// ask `contains_speculation` with that id and must not see this entry as a
/// withdrawal of their own guard. Never [`SPECULATION_ID_ANY`].
pub fn expected_throw_speculation_id(bci: u32, reason: DeoptReason) -> u32 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    // A fixed salt word ahead of the pair `speculation_id` hashes.
    h.write_u32(0x5E7E_C7ED);
    h.write_u32(bci);
    h.write_u32(reason as u32);
    let id = h.finish() as u32;
    if id == SPECULATION_ID_ANY || id == speculation_id(bci, reason) {
        2
    } else {
        id
    }
}

/// JIT round 20 wave 12, lane qNull (L1W19-2): the de-spec registry id that
/// names "the implicit null check of the single-pass `getfield` at `bci` has
/// faulted past the per-bci trap limit" ([`DespecRegistry::note_implicit_null_trap`]).
/// Salted apart from [`speculation_id`] and [`expected_throw_speculation_id`]
/// of the same bci under [`DeoptReason::NullCheck`], so no guard's own
/// `contains_speculation` question and no expected-throw row ever reads it as
/// theirs. Never [`SPECULATION_ID_ANY`]. Only a COARSE per-bci consumer
/// ([`DespecRegistry::contains`]) sees it, and every one of those withdraws an
/// optimisation, never a check.
pub fn implicit_null_trap_speculation_id(bci: u32) -> u32 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write_u32(0x1A11_7E55);
    h.write_u32(bci);
    let taken = |v: u32| {
        v == SPECULATION_ID_ANY
            || v == speculation_id(bci, DeoptReason::NullCheck)
            || v == expected_throw_speculation_id(bci, DeoptReason::NullCheck)
    };
    let mut id = h.finish() as u32;
    // A collision is ~2^-31; step to the next free small value.
    let mut nudge = 3u32;
    while taken(id) {
        id = nudge;
        nudge += 1;
    }
    id
}

/// SPLICE2-2: does the bytecode `op` raise `reason`'s exception when that
/// guard fails, i.e. is the re-executed instruction the throw? `false` for
/// every other opcode, so a guard that names the reason without an exception
/// at its bci (a hoisted range check at a loop header) is never an expected
/// throw.
pub fn opcode_throws_for_reason(op: u8, reason: DeoptReason) -> bool {
    match reason {
        // xaload / xastore.
        DeoptReason::BoundsCheck => matches!(op, 0x2e..=0x35 | 0x4f..=0x56),
        // idiv, ldiv, irem, lrem.
        DeoptReason::DivByZero => matches!(op, 0x6c | 0x6d | 0x70 | 0x71),
        // xaload / xastore, arraylength, athrow, getfield, putfield,
        // invokevirtual, invokespecial, invokeinterface, monitorenter / exit.
        DeoptReason::NullCheck => matches!(
            op,
            0x2e..=0x35 | 0x4f..=0x56 | 0xbe | 0xbf | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb9 | 0xc2 | 0xc3
        ),
        _ => false,
    }
}

/// How many times `max_deopts_per_method` a de-spec'd method may keep
/// recompiling before the whole-method blacklist fires anyway
/// (`CRATONVM_JIT_DESPEC_SPARE_FACTOR`, default 2). The backstop exists because
/// "this bci is de-spec'd" is a claim about the NEXT compile: a site that keeps
/// trapping past the factor is evidence the claim is wrong, and an unbounded
/// spare would recompile forever. Tunable so the factor can be swept against
/// the deopt count without a rebuild.
pub fn despec_spare_factor() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&f| f > 0)
            .unwrap_or(2)
    })
}

// ---------------------------------------------------------------------------
// Epoch staleness guard (deopt-osr Step 9 follow-up a)
// ---------------------------------------------------------------------------

/// A small, **process-lifetime-retained** cell baked (by raw pointer) into the
/// x64 frame-deopt stub *alongside* the `DeoptimizationPoint` box, so the deopt
/// trampoline can decide — **before dereferencing the box** — whether the
/// speculation it bakes has been superseded by a later invalidation.
///
/// Why a separate cell rather than a field on the box: it was built for a
/// `CRATONVM_JIT_FREE_CODE=1` A/B mode in which an evicted artifact's
/// `DeoptimizationPoint` boxes could be freed under a running frame, so reading
/// the epoch *from* the box would itself have been the use-after-free. That mode
/// is gone (an executing artifact owns its boxes until it returns), and
/// `x64_deopt_entry` no longer short-circuits on this guard. It is still leaked
/// independently of the artifact and still stamped, so the stub ABI and the
/// VM-side staleness check keep a stable cell to read.
///
/// Stamped once by the VM at install time (`SharedVm`/`stamp_compilation_epoch`)
/// under `deopt_real_enabled()`; on every production artifact it stays
/// `{ live_epoch_cell: null, creation_epoch: 0 }` and the in-entry check is a
/// no-op (gate-off byte-identical). Both fields are atomic so the VM's
/// single install-time write is visible to the lock-free in-stub read.
#[derive(Debug)]
pub struct DeoptEpochGuard {
    /// The artifact's compilation epoch, stamped at install. Compared against
    /// `*live_epoch_cell`: when the live epoch has advanced past it, every
    /// speculation this artifact baked is superseded.
    pub creation_epoch: std::sync::atomic::AtomicU64,
    /// Stable pointer to the owning method's live compilation-epoch cell
    /// (`SharedVm::method_epochs`, itself an `AtomicU64` kept boxed so its
    /// address is stable for the process lifetime). Null until the VM stamps it,
    /// and on every production artifact.
    pub live_epoch_cell: std::sync::atomic::AtomicPtr<std::sync::atomic::AtomicU64>,
    /// The compile id (`crate::CompileIdReservation`) of the compilation
    /// whose stubs bake this guard, written by the stub emitter; 0 = none.
    /// Round 14 wave 2 (lane deopt; proposal R13RP6-1): [`x64_deopt_entry`]
    /// resolves it ([`crate::lookup_compile_id`]) to the trapping artifact,
    /// checks that the artifact owns THIS guard, and stashes the bytecode the
    /// body was compiled from beside the frame ([`peek_last_deopt_trap_source`]),
    /// so a call-site service can resume a SUPERSEDED body of a redefined
    /// class in its own code instead of re-running it from entry in the new
    /// one. An OSR body is never bound to its id, so it resolves to nothing.
    pub compile_id: std::sync::atomic::AtomicU32,
}

impl DeoptEpochGuard {
    /// A fresh, unstamped guard (null cell, epoch 0) — the in-entry check is a
    /// no-op until the VM stamps it.
    pub fn new() -> Self {
        Self::for_compile(0)
    }

    /// [`Self::new`] for the compilation holding compile id `compile_id`
    /// (0 = none); see [`Self::compile_id`].
    pub fn for_compile(compile_id: u32) -> Self {
        Self {
            creation_epoch: std::sync::atomic::AtomicU64::new(0),
            live_epoch_cell: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            compile_id: std::sync::atomic::AtomicU32::new(compile_id),
        }
    }

    /// `true` when the artifact has been superseded: a non-null live cell whose
    /// epoch has advanced past this artifact's creation epoch. Lock-free; safe
    /// on a guard whose `live_epoch_cell` is null (returns `false`).
    #[inline]
    pub fn is_superseded(&self) -> bool {
        use std::sync::atomic::Ordering;
        let cell = self.live_epoch_cell.load(Ordering::Acquire);
        if cell.is_null() {
            return false;
        }
        // SAFETY: a non-null `live_epoch_cell` is a stable, retained
        // `AtomicU64` address handed out by `SharedVm::method_epochs` (never
        // freed for the process lifetime).
        let live = unsafe { (*cell).load(Ordering::Relaxed) };
        live > self.creation_epoch.load(Ordering::Relaxed)
    }
}

impl Default for DeoptEpochGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Deopt point (embedded in compiled code metadata)
// ---------------------------------------------------------------------------

/// Metadata attached to a specific native code offset that enables deopt.
#[derive(Debug, Clone)]
pub struct DeoptimizationPoint {
    /// Offset in native code where this deopt point lives.
    pub native_offset: u32,
    /// Corresponding bytecode index in the original method.
    pub bci: u32,
    /// Reason this deopt point exists.
    pub reason: DeoptReason,
    /// What to do when deopt is triggered.
    pub action: DeoptAction,
    /// Speculation ID (tracks which speculation failed).
    pub speculation_id: u32,
    /// How to reconstruct the interpreter frame.
    pub frame_state: FrameState,
    /// What the interpreter must do with the bytecode at `frame_state.bci`:
    /// re-execute it, continue after it, or route a pending exception through
    /// the method's exception table.
    ///
    /// This used to be a **prose convention** derived from `reason` by each
    /// consumer independently — `docs/jit/deopt-metadata.md` records the
    /// missing field as an outright gap, and "a consumer that gets the
    /// convention wrong executes the instruction after a call that never
    /// returned". Every producer stamps [`ResumeSemantics::for_reason`], which
    /// *is* that convention, so recording it changes no behaviour; what changes
    /// is that a producer which knows better — an inlined caller scope
    /// ([`ResumeSemantics::for_caller_scope`]), a resume point after a call
    /// that did return — can now say so.
    ///
    /// **Still emitted, not yet read on the resume side.** The verifier's
    /// exception-state-agreement lane checks it against `reason`
    /// ([`DeoptMetadataError::ResumeSemanticsMismatch`]), so a producer cannot
    /// contradict itself; but `vm/src/runtime/interpreter.rs`'s resume sink
    /// still infers re-execute-vs-resume from `reason` rather than reading this
    /// field. That one consumer edit is what `docs/feature-designs/deopt-osr.md`
    /// §5.2 is still open on.
    pub semantics: ResumeSemantics,
}

impl DeoptimizationPoint {
    /// Does this point's [`Self::bci`] name a bytecode of the artifact's ROOT
    /// method? `false` for a single-pass chain point: a guard of an outermost
    /// splice (`push_inline_scope` / `inline_caller_chain`) or a post-call exit
    /// after a real call inside a splice, recorded with the spliced CALLEE's
    /// bci (`resume_bci_for(pc)` answers the callee pc inside a splice) and
    /// carrying the caller chain in `frame_state.caller`. Such a bci lives in
    /// another method's key space, so a by-bci reader that asks about a root
    /// bci must not count it (JIT round 19 proposal wave 10, lane deopt10;
    /// `i62-L2-proposal-single-pass-chain-points-leave-the-roots-bci-key-space`,
    /// behind [`crate::osr_exit::chain_points_off_root_bcis_enabled`]). The
    /// stash sinks resolve a chain point by its baked address and the OSR
    /// door's `RecordedBci` provenance check asks the raw list, so neither
    /// needs this.
    pub fn names_a_root_bci(&self) -> bool {
        self.frame_state.caller.is_none()
    }
}

// ---------------------------------------------------------------------------
// Deopt event log
// ---------------------------------------------------------------------------

/// A single recorded deoptimization event.
#[derive(Debug, Clone)]
pub struct DeoptEvent {
    pub reason: DeoptReason,
    pub action: DeoptAction,
    pub bci: u32,
    pub timestamp_ms: u64,
    pub speculation_id: u32,
}

/// The class-name field of a `"<class>.<method>:<descriptor>"` method key.
///
/// `None` for a key that is not in that shape (the empty key legacy/test
/// wrappers bake, or anything a future producer invents), which callers must
/// treat as "matches no class" — the fail-closed direction for a function whose
/// only caller decides what to forget.
fn method_key_class(method_key: &str) -> Option<&str> {
    let (rest, _descriptor) = method_key.rsplit_once(':')?;
    let (class, _method) = rest.rsplit_once('.')?;
    Some(class)
}

/// How many `(reason, bci)` trap sites one method's ledger tracks.
///
/// A method with more distinct failing speculation sites than this is already
/// past every escalation threshold there is, so the tail carries no decision —
/// it is pure memory. The cap is what makes [`MethodTrapLedger`] bounded per
/// method as well as bounded overall; the previous `Vec<DeoptEvent>` was
/// neither.
const MAX_TRAP_SITES_PER_METHOD: usize = 64;

/// How many recent [`DeoptEvent`]s one method keeps for diagnostics.
///
/// The ring is the ONLY place raw events are kept now, and nothing reads it to
/// make a decision — `CRATONVM_DBG_DEOPT` dumps and tests read it. 32 is enough
/// to see the shape of a thrash without keeping one allocation per exception
/// thrown for the life of the process.
const TRAP_HISTORY_RING: usize = 32;

/// Charged traps after which a method is made not compilable whatever the
/// decayed rate says: HotSpot's `PerMethodRecompilationCutoff` default. Every
/// escalation count halves each `TRAP_DECAY_MS`, so a method trapping fewer
/// than ~10 times a minute would otherwise be recompiled forever
/// (r11w5-tier-deopt-lifetime-recompile-cutoff-patch).
const PER_METHOD_TRAP_CUTOFF: u64 = 400;

/// The trap ledger for one method: bounded, per-`(reason, bci)`, and decayed.
///
/// # Why this replaced an unbounded `Vec<DeoptEvent>`
///
/// The log used to keep every event ever recorded for a method, forever, behind
/// a global mutex, and answer every policy question with an O(n) scan over it.
/// Three separate defects fell out of that one representation:
///
///  * **It never decayed.** `recommend_action` escalated on
///    `history[method].len()`, and the only thing that ever removed entries was
///    a class unload. A long-lived server that took one ordinary polymorphic
///    `ClassCheck` deopt every few hours permanently blacklisted the method
///    after twenty of them — a trap rate of one per hour, retiring a hot method
///    to the interpreter for the rest of the process. The tiered manager
///    already decayed its own counts ([`crate::tiered::decay_traps`]); the
///    ledger that actually produces `MakeNotCompilable` did not, so the two
///    ledgers permanently disagreed about the same method.
///  * **It mixed reasons.** `PendingException`, `TransferToInterpreter` and a
///    soft `OsrExit` never escalate *themselves*, but every one of them was
///    recorded and therefore inflated the count that escalates everything else.
///    Ten exceptions out of a hot `try`/`catch` made the very next genuine
///    `UncommonTrap` come back `MakeNotEntrant`.
///  * **It was unbounded.** `PendingException` never escalates, so it can recur
///    forever on a still-compiled method: roughly twenty-four bytes per
///    exception thrown out of compiled code, retained for the life of the
///    process.
///
/// The shape here is deliberately the same one
/// [`crate::tiered::MethodState::trap_counts`] uses — per-`(reason, bci)`,
/// halved on the same schedule — because having two trap ledgers with
/// different half-lives driving one decision is the root problem, not a
/// symptom of it. What is charged is decided by exactly one predicate,
/// [`crate::tiered::deopt_reason_is_charged`], which both ledgers call.
#[derive(Debug, Default)]
struct MethodTrapLedger {
    /// Charged traps per `(reason, bci)`. Bounded by
    /// [`MAX_TRAP_SITES_PER_METHOD`].
    charged: FxHashMap<(DeoptReason, u32), u32>,
    /// Sum of [`Self::charged`]'s values, maintained incrementally so the
    /// escalation policy is O(1) rather than a scan.
    charged_total: u32,
    /// Charges that arrived at a NEW site after [`Self::charged`] was full:
    /// part of [`Self::charged_total`], but in no per-site row. Kept so
    /// [`Self::decay`] halves them instead of recomputing the total from the
    /// rows alone, which erased every one of them on the first decay (r11-tier).
    overflow_charged: u32,
    /// The most recent events, whatever their reason — diagnostics only.
    recent: VecDeque<DeoptEvent>,
    /// Every event ever recorded for this method, charged or not. Never
    /// decayed, never read by a policy: the "how often does this method deopt
    /// at all" number that the escalation count used to be conflated with.
    lifetime_events: u64,
    /// Charged traps ever recorded for this method: never decayed. The input
    /// to [`PER_METHOD_TRAP_CUTOFF`], the one policy that must not forget
    /// (HotSpot's `PerMethodRecompilationCutoff`).
    lifetime_charged: u64,
    /// [`crate::tiered::process_uptime_ms`] at the last decay.
    decay_ms: u64,
}

impl MethodTrapLedger {
    fn new(now_ms: u64) -> Self {
        Self {
            decay_ms: now_ms,
            ..Self::default()
        }
    }

    /// How many halvings this ledger owes: one per whole
    /// [`crate::tiered::TRAP_DECAY_MS`] elapsed since the last applied decay.
    ///
    /// Time only, where the tiered manager also decays on invocations: this
    /// ledger never sees an invocation. The time arm is the one that matters
    /// for the failure it exists to prevent anyway — a method recompiled
    /// immediately after each trap stops reaching the interpreter's invocation
    /// hook, which is exactly why [`crate::tiered::TRAP_DECAY_MS`] was added
    /// there too.
    ///
    /// Capped at 31 so the shift it feeds is always defined, which is also all
    /// it needs to be: a `u32` count is zero after 32 halvings.
    fn halvings_due(&self, now_ms: u64) -> u32 {
        self.periods_due(now_ms).min(31) as u32
    }

    /// Whole decay periods elapsed since the last applied decay, uncapped —
    /// what [`Self::decay`] advances `decay_ms` by, so that a ledger idle for
    /// hours does not stay permanently "31 halvings behind" and read every
    /// later charge as zero.
    fn periods_due(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.decay_ms) / crate::tiered::TRAP_DECAY_MS
    }

    /// The charged total as of `now_ms`, **without** mutating: the decay owed
    /// applied to the stored value.
    ///
    /// Reads have to see the decay too. `recommend_action` is asked BEFORE
    /// `record_deopt` runs (that is the order `SharedVm::record_deoptimization`
    /// uses, and it has to be — the action it returns is what the event
    /// records), so a policy that only saw decay applied by the *write* would
    /// read a stale count on exactly the event that matters. For a method
    /// trapping once an hour that is the difference between a count that stays
    /// at one and a count that climbs to the blacklist.
    fn decayed_total(&self, now_ms: u64) -> u32 {
        self.charged_total >> self.halvings_due(now_ms)
    }

    /// The count at one `(reason, bci)` site as of `now_ms`, same projection.
    fn decayed_site(&self, key: &(DeoptReason, u32), now_ms: u64) -> u32 {
        self.charged.get(key).copied().unwrap_or(0) >> self.halvings_due(now_ms)
    }

    /// Apply the halvings owed to the stored counts, dropping sites that reach
    /// zero.
    fn decay(&mut self, now_ms: u64) {
        let periods = self.periods_due(now_ms);
        if periods == 0 {
            return;
        }
        let halvings = periods.min(31) as u32;
        // Advance by whole periods only, so the remainder of the current period
        // is not silently forgiven on every write — but by ALL of them, so the
        // ledger is never left owing more halvings than it can apply.
        self.decay_ms = self
            .decay_ms
            .saturating_add(periods.saturating_mul(crate::tiered::TRAP_DECAY_MS));
        self.overflow_charged >>= halvings;
        let mut total = self.overflow_charged;
        self.charged.retain(|_, n| {
            *n >>= halvings;
            total = total.saturating_add(*n);
            *n > 0
        });
        self.charged_total = total;
    }

    fn charge(&mut self, reason: DeoptReason, bci: u32) {
        let key = (reason, bci);
        // A new site is admitted only while there is room. At the cap the
        // method is far past every threshold, so refusing the new site changes
        // no decision — but the *total* is still bumped, so a method whose
        // traps spread over thousands of bcis cannot use that spread to stay
        // under the escalation count forever.
        if self.charged.len() >= MAX_TRAP_SITES_PER_METHOD && !self.charged.contains_key(&key) {
            self.overflow_charged = self.overflow_charged.saturating_add(1);
            self.charged_total = self.charged_total.saturating_add(1);
            return;
        }
        let n = self.charged.entry(key).or_insert(0);
        *n = n.saturating_add(1);
        self.charged_total = self.charged_total.saturating_add(1);
    }
}

/// Tracks deopt history per method and drives adaptive recompilation.
/// T10.9.B: FxHashMap — keyed on internal method names from loaded class files.
///
/// See [`MethodTrapLedger`] for what one method's entry holds and why it is
/// shaped the way it is.
pub struct DeoptimizationLog {
    methods: FxHashMap<String, MethodTrapLedger>,
    total_deopts: u64,
    max_deopts_per_method: u32,
}

impl DeoptimizationLog {
    /// Create a new log with default threshold (20 charged traps before giving
    /// up).
    pub fn new() -> Self {
        Self::new_with_threshold(20)
    }

    /// Create a new log with a custom threshold.
    pub fn new_with_threshold(max_deopts: u32) -> Self {
        Self {
            methods: FxHashMap::default(),
            total_deopts: 0,
            max_deopts_per_method: max_deopts,
        }
    }

    /// "Now" for the decay schedule — the same monotonic process clock the
    /// tiered manager's trap decay uses, so the two ledgers cannot drift apart
    /// on the clock as well as on the policy.
    ///
    /// Deliberately NOT `DeoptEvent::timestamp_ms`: that is wall-clock
    /// milliseconds since the Unix epoch, supplied by the caller, and a ledger
    /// whose half-life depends on a value a caller fills in is one that will
    /// eventually be filled in wrong.
    fn now_ms() -> u64 {
        crate::tiered::process_uptime_ms()
    }

    /// Record a deoptimization event for a method.
    ///
    /// PERF-P5 (T10.9.C): avoid the per-call `String::from(method)` that
    /// `HashMap::entry` requires by trying a `get_mut` first. The owned-key
    /// allocation only happens on the first deopt for a given method name
    /// (the miss path).
    ///
    /// TODO(PERF-P5): take `&Arc<str>` once upstream call sites in
    /// `vm/src/runtime/jit_integration.rs` and `vm/src/vm/vm_init.rs`
    /// thread the standard `Arc<str>` method-name carrier through.
    pub fn record_deopt(&mut self, method: &str, event: DeoptEvent) {
        self.total_deopts += 1;
        let now = Self::now_ms();
        // Probe before inserting so the owned-key allocation only happens on
        // the first deopt for a given method name. Two lookups on the miss
        // path, one on the hit path; written this way rather than as
        // `get_mut(..).unwrap_or_else(insert)` because that shape needs a
        // borrow the checker cannot see is disjoint.
        if !self.methods.contains_key(method) {
            self.methods
                .insert(method.to_string(), MethodTrapLedger::new(now));
        }
        let Some(ledger) = self.methods.get_mut(method) else {
            return;
        };
        ledger.decay(now);
        ledger.lifetime_events = ledger.lifetime_events.saturating_add(1);
        if crate::tiered::deopt_reason_is_charged(event.reason) {
            ledger.charge(event.reason, event.bci);
            ledger.lifetime_charged = ledger.lifetime_charged.saturating_add(1);
        }
        if ledger.recent.len() == TRAP_HISTORY_RING {
            ledger.recent.pop_front();
        }
        ledger.recent.push_back(event);
    }

    /// Charged traps recorded for `method`, as of the last decay.
    ///
    /// This is the escalation count, and it is **not** "how many times this
    /// method deopted" — an exception routed out of compiled code is not a
    /// failed speculation and does not appear here. Use
    /// [`Self::lifetime_deopt_count`] for the raw rate.
    pub fn deopt_count(&self, method: &str) -> usize {
        let now = Self::now_ms();
        self.methods
            .get(method)
            .map_or(0, |l| l.decayed_total(now) as usize)
    }

    /// Every deopt ever recorded for `method`, charged or not, undecayed.
    /// Diagnostics only — no policy reads it.
    pub fn lifetime_deopt_count(&self, method: &str) -> u64 {
        self.methods.get(method).map_or(0, |l| l.lifetime_events)
    }

    /// Charged traps recorded for `method` at the specific bytecode index
    /// `bci`. Drives per-bci de-speculation (Step 9 follow-up c): a single
    /// speculation site that fails repeatedly is de-spec'd on its own (its guard
    /// suppressed on the next compile) rather than escalating to a whole-method
    /// blacklist once the *aggregate* per-method count crosses the threshold.
    ///
    /// Uncharged reasons are excluded here too, and that matters: a protected
    /// invoke carries a `PendingException` point at the same bci as its
    /// receiver guard, so counting exceptions here would de-spec a guard for a
    /// program that only ever threw.
    pub fn deopt_count_at_bci(&self, method: &str, bci: u32) -> usize {
        let now = Self::now_ms();
        self.methods.get(method).map_or(0, |l| {
            let shift = l.halvings_due(now);
            l.charged
                .iter()
                .filter(|((_, b), _)| *b == bci)
                .map(|(_, n)| (*n >> shift) as usize)
                .sum()
        })
    }

    /// Charged traps recorded for `method` at one `(reason, bci)` site — the
    /// finest grain the ledger keeps, and the one HotSpot's
    /// `PerBytecodeTrapLimit` is expressed in.
    pub fn deopt_count_at_site(&self, method: &str, reason: DeoptReason, bci: u32) -> usize {
        let now = Self::now_ms();
        self.methods
            .get(method)
            .map_or(0, |l| l.decayed_site(&(reason, bci), now) as usize)
    }

    /// Returns `true` when the method has exceeded the charged-trap threshold.
    pub fn should_give_up(&self, method: &str) -> bool {
        self.deopt_count(method) >= self.max_deopts_per_method as usize
    }

    /// The deopt reason that has occurred most often for `method`, over the
    /// retained ring. Diagnostic: the ring is bounded, so this reports the
    /// recent shape rather than a lifetime tally.
    pub fn most_common_reason(&self, method: &str) -> Option<DeoptReason> {
        let ledger = self.methods.get(method)?;
        let mut counts: FxHashMap<DeoptReason, usize> = FxHashMap::default();
        for e in &ledger.recent {
            *counts.entry(e.reason).or_default() += 1;
        }
        counts.into_iter().max_by_key(|&(_, c)| c).map(|(r, _)| r)
    }

    /// The retained recent-event ring for a method, oldest first (empty when
    /// the method has never deopted).
    ///
    /// Bounded by [`TRAP_HISTORY_RING`]; see [`MethodTrapLedger`] for why an
    /// unbounded history was a leak rather than a feature.
    ///
    /// Returns an owned `Vec` rather than a slice because the ring is a
    /// `VecDeque` and has no single contiguous slice. Every caller is a
    /// diagnostic or a test — no policy reads raw events any more — so the
    /// copy is not on any hot path.
    pub fn history(&self, method: &str) -> Vec<DeoptEvent> {
        self.methods
            .get(method)
            .map(|l| l.recent.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Total deopts across all methods.
    pub fn total_deopts(&self) -> u64 {
        self.total_deopts
    }

    /// Clear history for a method (e.g., after successful recompilation).
    pub fn clear_history(&mut self, method: &str) {
        // Do not decrement total_deopts — it is a lifetime counter.
        self.methods.remove(method);
    }

    // Release deoptimization history for methods owned by an unloaded class.
    //
    // Method keys are `"<class>.<method>:<descriptor>"`, so the class name is
    // everything before the last `.` that precedes the last `:` — exactly the
    // parse `deopt_frame_matches_method` performs on the VM side. This used to
    // be `starts_with(&format!("{class_name}."))`, which is a statement about
    // the shape of the *whole* key rather than about its class field: it
    // dropped the history of any method whose key merely began with those
    // characters. Harmless today (internal class names use `/`, and `.` is not
    // a legal class-name character, so no two loaded classes can collide) —
    // but it was a string-shape assumption with no test, in a function whose
    // job is to forget the right method's traps.
    // gc-common w36: production unloads go through the batch form
    // (`clear_classes`, one sweep per unload), so the single
    // form is kept for the equivalence and inventory tests only.
    #[cfg(test)]
    fn clear_class(&mut self, class_name: &str) {
        self.methods
            .retain(|method, _| method_key_class(method) != Some(class_name));
    }

    /// Batch form of [`Self::clear_class`] for a whole unloaded set: ONE
    /// `retain` instead of one per class (gc-common w36-c). Equivalent to
    /// calling `clear_class` for each name
    /// (`clear_classes_matches_a_loop_of_clear_class`).
    pub fn clear_classes(&mut self, class_names: &rustc_hash::FxHashSet<&str>) {
        if class_names.is_empty() {
            return;
        }
        self.methods.retain(|method, _| {
            !method_key_class(method).is_some_and(|class| class_names.contains(class))
        });
    }

    /// [`Self::recommend_action`], but told WHICH bci trapped -- so the
    /// whole-method `MakeNotCompilable` escalation can be withheld from a
    /// method whose only failing speculation has already been withdrawn.
    ///
    /// `ReceiverTypeChanged` / `ClassCheck` escalate on the AGGREGATE per-method
    /// deopt count, which is right when the type profile is merely unstable and
    /// wrong when one call site speculates on a receiver class the program never
    /// produces. The second case is not fixed by recompiling and is not the
    /// method's fault: the per-bci de-spec registry exists precisely to drop
    /// that ONE guard on the next compile (see [`DespecRegistry::insert`]'s
    /// caller, `real_frame_deopt_resume_and_despeculate`), and since the emitter now
    /// honours it (`x64::bytecode_walk`'s invoke ladder) the recompile really
    /// does come back without the guard. Blacklisting the method anyway retires
    /// it to the interpreter for a speculation that no longer exists.
    ///
    /// So: when the trapping bci is already de-spec'd, recompile instead of
    /// blacklisting. The per-method backstop is kept rather than removed -- at
    /// twice `max_deopts_per_method` the escalation happens regardless, because
    /// "de-spec'd" is a claim about the NEXT compile and a site that keeps
    /// trapping past that point is evidence the claim is wrong. Deopts at any
    /// OTHER bci are untouched and still escalate on the ordinary schedule.
    ///
    /// `despec` is the VM's own registry (`JitRealm::despec_registry`) — the
    /// same one its compiles consult, so "already de-spec'd" means de-spec'd for
    /// THIS VM's next compile.
    pub fn recommend_action_at_bci(
        &self,
        method: &str,
        reason: DeoptReason,
        bci: u32,
        despec: &DespecRegistry,
    ) -> DeoptAction {
        if matches!(reason, DeoptReason::OsrExit) {
            return self.recommend_osr_exit_at_site(method, bci);
        }
        let action = self.recommend_action(method, reason);
        if action != DeoptAction::MakeNotCompilable
            || !matches!(
                reason,
                DeoptReason::ReceiverTypeChanged | DeoptReason::ClassCheck
            )
            || !crate::receiver_despec_enabled()
            || !despec.contains(method, bci)
            || self.deopt_count(method)
                >= despec_spare_factor() * self.max_deopts_per_method as usize
            || self.past_lifetime_cutoff(method)
        {
            return action;
        }
        crate::metrics::note_despec_escalation_spared();
        DeoptAction::RecompileAndReinterpret
    }

    /// The `OsrExit` escalation, keyed on the exiting SITE (r9 wave 2).
    ///
    /// [`Self::recommend_action`]'s `OsrExit` arm argues per-site ("a bci that
    /// keeps exiting is a structural property of the loop … tolerate the first
    /// `half` occurrences") but, having no bci, tests the method's AGGREGATE
    /// charged count: a method that took `half` ordinary `UncommonTrap`s
    /// anywhere — each answered "fine, recompile" — was made permanently
    /// not-compilable (OSR door included) by its FIRST loop exit.
    ///
    /// Here the rule is what the comment says: `MakeNotCompilable` once THIS
    /// `(OsrExit, bci)` site has exited `half` times (decayed, like every other
    /// count). The structurally-always-exiting bci that arm was written for
    /// (Tomcat `Response.toAbsolute()`) charges one site, whose count grows
    /// exactly as fast as the aggregate would have, so it retires on the same
    /// schedule. The aggregate stays as a backstop at `full` — a method whose
    /// loops exit at many bcis, none of them `half` times, still pays the
    /// reconstruct-and-resume tax the arm exists to cap, just at twice the
    /// count. Never MORE aggressive than the aggregate rule: the site count is
    /// a subset of the aggregate.
    ///
    /// `docs/feature-designs/jit-osr-exit-and-recompile.md`.
    fn recommend_osr_exit_at_site(&self, method: &str, bci: u32) -> DeoptAction {
        let half = (self.max_deopts_per_method as usize) / 2;
        let full = self.max_deopts_per_method as usize;
        if self.deopt_count_at_site(method, DeoptReason::OsrExit, bci) >= half
            || self.deopt_count(method) >= full
        {
            DeoptAction::MakeNotCompilable
        } else {
            DeoptAction::Reinterpret
        }
    }

    /// Has `method` been charged [`PER_METHOD_TRAP_CUTOFF`] traps over its
    /// whole life? The de-spec spare in [`Self::recommend_action_at_bci`] must
    /// not undo that answer.
    fn past_lifetime_cutoff(&self, method: &str) -> bool {
        self.methods
            .get(method)
            .is_some_and(|l| l.lifetime_charged >= PER_METHOD_TRAP_CUTOFF)
    }

    /// Recommend a deopt action from the method's CHARGED trap count and the
    /// triggering reason.
    ///
    /// The `reason` parameter influences the recommended action:
    /// - `ReceiverTypeChanged`, `ClassCheck` → aggressive recompile (the type profile changed)
    /// - `NotCompiled`, `UnreachedCode` → give up immediately
    /// - `SpeculationFailed`, `ClassLoading` → recompile with updated assumptions
    /// - `TransferToInterpreter`, `PendingException` → never escalate
    /// - Other reasons use the count-based policy:
    ///   - First occurrence              → Reinterpret
    ///   - 2..threshold/2                → RecompileAndReinterpret
    ///   - threshold/2..threshold        → MakeNotEntrant
    ///   - >= threshold                  → MakeNotCompilable
    ///
    /// # The count is charged-and-decayed, and both halves are load-bearing
    ///
    /// `count` used to be `history[method].len()`: every event ever recorded,
    /// whatever its reason, never trimmed. Two failures followed from that, and
    /// both of them ended in `MakeNotCompilable` — which the VM turns into a
    /// permanent, whole-method compile ban that also closes the OSR door.
    ///
    /// **Soft reasons pushed real ones over the cliff.** `PendingException`,
    /// `TransferToInterpreter` and a soft `OsrExit` each return early below
    /// without escalating *themselves* — but each was recorded, so each
    /// inflated the count that escalates everything else. A hot `try`/`catch`
    /// in a loop accrued ten exceptions, and the very first genuine
    /// `UncommonTrap` after that came back `MakeNotEntrant` instead of
    /// `Reinterpret`. [`crate::tiered::deopt_reason_is_charged`] — the same
    /// predicate the tiered manager's own trap limits use — now decides what
    /// counts, so the two policies cannot drift.
    ///
    /// **Nothing decayed.** Twenty ordinary polymorphic `ClassCheck` deopts
    /// spread over *days* read exactly like twenty in a second, so a long-lived
    /// server eventually blacklisted every hot method it had. The ledger halves
    /// on [`crate::tiered::TRAP_DECAY_MS`] now, so the count expresses a trap
    /// *rate* rather than a lifetime total, and escalation means "this method
    /// is still failing" rather than "this method has ever failed enough".
    pub fn recommend_action(&self, method: &str, reason: DeoptReason) -> DeoptAction {
        // Asked before `record_deopt`, as every call is, so the 401st charged
        // trap is the one refused. Uncharged reasons keep their own arms:
        // `PendingException` must never retire a method.
        if crate::tiered::deopt_reason_is_charged(reason) && self.past_lifetime_cutoff(method) {
            return DeoptAction::MakeNotCompilable;
        }
        let count = self.deopt_count(method);

        let half = (self.max_deopts_per_method as usize) / 2;
        let full = self.max_deopts_per_method as usize;

        // Certain reasons override the count-based policy.
        match reason {
            // Type-related failures benefit from immediate recompile with new profile.
            DeoptReason::ReceiverTypeChanged | DeoptReason::ClassCheck => {
                if count >= self.max_deopts_per_method as usize {
                    return DeoptAction::MakeNotCompilable;
                }
                return DeoptAction::RecompileAndReinterpret;
            }
            // Method was never compiled or dead code was hit — do not retry.
            DeoptReason::NotCompiled | DeoptReason::UnreachedCode => {
                return DeoptAction::MakeNotCompilable;
            }
            // Speculation/class hierarchy change — recompile with updated assumptions.
            // W17L4-5: a pruned branch that was taken is a failed profile
            // speculation too (HotSpot `Reason_unstable_if`): recompile now;
            // the VM withdraws the prune at that bci after this one charge.
            DeoptReason::SpeculationFailed
            | DeoptReason::ClassLoading
            | DeoptReason::UnstableIf => {
                if count >= self.max_deopts_per_method as usize {
                    return DeoptAction::MakeNotCompilable;
                }
                return DeoptAction::RecompileAndReinterpret;
            }
            // Transfer to interpreter is a soft deopt — just reinterpret.
            DeoptReason::TransferToInterpreter => {
                return DeoptAction::Reinterpret;
            }
            // A caught Java exception is ordinary control flow, not a failed
            // speculation: recompiling changes nothing and blacklisting the
            // method for throwing would permanently interpret every hot
            // try/catch. Never escalate.
            DeoptReason::PendingException => {
                return DeoptAction::Reinterpret;
            }
            // OSR-exit is a real, EXPECTED control-flow event -- "a running
            // JIT/OSR frame bailed mid-loop back to the interpreter at a loop
            // bci (not a guard bci)" (see this enum's own doc comment). It is
            // not evidence the compiled artifact mis-speculated, so recompiling
            // cannot fix it: the same loop boundary will exit the same way on
            // the next compile too. Routing it through the generic count-based
            // policy (RecompileAndReinterpret then MakeNotEntrant) made a hot
            // method with a structurally-always-taken OSR-exit (e.g. Tomcat's
            // `Response.toAbsolute()`, docs/feature-designs/gpu-offload.md) get evicted and eagerly recompiled
            // dozens of times over a single benchmark for zero benefit.
            //
            // But ALWAYS reinterpreting (never evicting) is also wrong: a
            // structurally-always-taken OSR-exit pays the reconstruct-and-
            // resume tax on literally EVERY call while staying "compiled",
            // which measured net SLOWER than plain interpretation (347s/round
            // vs the 63-72s/round fully-interpreted baseline for the same
            // benchmark -- confirmed empirically, not assumed). A handful of
            // genuinely rare OSR-exits are cheap and worth tolerating to keep
            // the rest of the method's hot path compiled; a bci that keeps
            // exiting is a structural property of the loop, not noise, and no
            // amount of waiting fixes it. So: tolerate the first `half`
            // occurrences as a soft deopt (Reinterpret, artifact stays live,
            // matching `TransferToInterpreter` above), then permanently give
            // up compiling this method (skip straight to MakeNotCompilable --
            // recompiling is pointless here, so there is no reason to pass
            // through MakeNotEntrant's "retryable" state first).
            //
            // This arm has no bci, so it can only test the AGGREGATE. The VM
            // asks through `recommend_action_at_bci`, which answers `OsrExit`
            // per site instead (`recommend_osr_exit_at_site`); this is the
            // bci-less fallback.
            DeoptReason::OsrExit => {
                if count >= half {
                    return DeoptAction::MakeNotCompilable;
                }
                return DeoptAction::Reinterpret;
            }
            // All other reasons use count-based policy.
            _ => {}
        }

        if count == 0 {
            DeoptAction::Reinterpret
        } else if count < half {
            DeoptAction::RecompileAndReinterpret
        } else if count < full {
            DeoptAction::MakeNotEntrant
        } else {
            DeoptAction::MakeNotCompilable
        }
    }
}

// ---------------------------------------------------------------------------
// Frame reconstruction helpers
// ---------------------------------------------------------------------------

/// A fully reconstructed interpreter frame ready for the interpreter to
/// resume execution.
///
/// `Clone` is needed by the vm-crate resume sink: virtual-object
/// re-materialization (`deopt_materialize::materialize_virtual_objects`) rewrites
/// slots in place, so the sink clones the immutable reconstructed frame into a
/// mutable copy before materializing.
#[derive(Debug, Clone, Default)]
pub struct ReconstructedFrame {
    pub class_id: u32,
    pub method_key: String,
    pub bci: u32,
    pub locals: Vec<FrameValue>,
    pub stack: Vec<FrameValue>,
    pub monitors: Vec<MonitorInfo>,
    /// What the interpreter must do with the bytecode at `bci` — **per scope**.
    ///
    /// Round 10 wave 9. [`FrameState`] has no per-scope semantics field, so a
    /// [`DeoptimizationPoint`] carries exactly one [`ResumeSemantics`]: the
    /// INNERMOST scope's. That was the half of
    /// `docs/feature-designs/deopt-osr.md` §5.2 the interner's retirement
    /// left open — the interned form carried them per scope and applied
    /// [`ResumeSemantics::for_caller_scope`] automatically to every linked
    /// caller, and the owned form could not.
    ///
    /// It is answered HERE instead, at the one place the chain is unwound:
    /// [`reconstruct_frame_from_machine_state`] stamps the trapping frame with the point's own
    /// semantics and every caller frame with `for_caller_scope()`, which is not
    /// a guess — a caller scope's `bci` names an `invoke` that is by
    /// construction already in progress. Consumers therefore read a field
    /// rather than re-deriving the rule from a frame's position in the chain
    /// (`deopt_resume::caller_resume_pc` is where the VM turns `RESUME` into
    /// the successor bci, which needs bytecode this crate does not have).
    ///
    /// A producer that eventually publishes real caller chains still has to
    /// carry per-scope semantics on the METADATA if a caller scope is ever
    /// anything but `RESUME`; nothing in either backend can express that today,
    /// and the default here is the answer for every chain they can build.
    pub semantics: ResumeSemantics,
    /// Outer frames when the deopt point was inside inlined code.
    pub caller_frames: Vec<ReconstructedFrame>,
}

/// Which deopt point fired: the `reason` and `speculation_id` of the
/// [`DeoptimizationPoint`] a trampoline was handed, carried BESIDE a stashed
/// [`ReconstructedFrame`] rather than inside it
/// (r11-tier-deopt-stash-drops-the-trap-reason).
///
/// Why a side value and not two new frame fields: `ReconstructedFrame` is
/// built by struct literal in a dozen test and VM files, and every one of them
/// would have to change in the same commit. The stash is the only place that
/// knows the point, so it keeps the cause next to each entry, and a sink that
/// wants it asks with [`take_last_deopt_with_cause`] /
/// [`peek_last_deopt_cause`] instead of re-deriving the reason from the bci —
/// which answers `Ambiguous` wherever two points share a bci (every unelided
/// IR array access plants a `NullCheck` and a `BoundsCheck` at one bci).
///
/// `None` from those accessors means "no point to describe": the identity-less
/// re-run sentinel of a null point, or a frame put back through the plain
/// [`restash_last_deopt`]. A sink then falls back to what it did before. (A
/// point that [`ordinary_stash_frame`] turned into the re-run sentinel keeps
/// its real cause; the frame's `bci == u32::MAX` is what every sink refuses
/// on, as before.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeoptCause {
    pub reason: DeoptReason,
    pub speculation_id: u32,
}

impl DeoptCause {
    /// The cause recorded by the point that fired.
    pub fn of_point(point: &DeoptimizationPoint) -> Self {
        Self {
            reason: point.reason,
            speculation_id: point.speculation_id,
        }
    }
}

/// One stash entry: the frame and, when a point described it, the cause.
struct StashEntry {
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    /// Address of the [`DeoptimizationPoint`] that fired, 0 when none did (the
    /// re-run sentinel, a plain restash). Kept here rather than in
    /// [`DeoptCause`] for the reason `DeoptCause` is kept out of the frame:
    /// that struct is built by literal in VM files. It names the ARTIFACT that
    /// trapped — see [`crate::CompiledMethod::owns_deopt_point`] — and is only
    /// ever compared for equality, never dereferenced.
    point_addr: usize,
    /// The bytecode the trapping body was compiled from and its constant-pool
    /// generation, when the trampoline could name the body (the x64 framed
    /// trap of a published body whose publish site stamped its source; see
    /// [`DeoptEpochGuard::compile_id`]). `None` everywhere else, including
    /// every restash.
    source: Option<TrapSource>,
    /// The forced-trip mark ([`FORCED_TRIP_FORCED`] and its census bits):
    /// set by [`ir_deopt_entry`] when the forced block of a chain point
    /// (`CRATONVM_DEOPT_EAGER_CHAINS`) sent the trap there through its own
    /// tagged pad, and carried through a take / restash of the same point
    /// ([`TAKEN_FORCED_MARK`]). Round 16 lane chain (CH5W4-1). 0 for every
    /// natural trap, and for everything with the soak switch off.
    forced: u8,
    /// The body that trapped, named at the trap while its frame was live
    /// (interpreter round i2 wave 60, lane L2, behind
    /// [`stash_names_trapping_body_enabled`]): the x64 entry through its epoch
    /// guard's compile id, the IR entry through the compile id its stub bakes.
    /// Carried through a take / restash of the same point
    /// ([`TAKEN_FORCED_MARK`]); `None` for the re-run sentinel, the
    /// exceptional stash, a body bound without an `Arc` and with the switch
    /// off. A sink asks [`taken_trapping_body`] before it looks the artifact
    /// up by name.
    body: Option<std::sync::Weak<crate::CompiledMethod>>,
    /// This thread's push number of the entry (HANDLER11-1, JIT round 19
    /// wave 11, lane handler11): every fresh push takes the next number of
    /// [`TakenEntry::pushes`]; a restash keeps the number the entry had when
    /// it was taken ([`TakenEntry::seq`]). A door that entered a body after
    /// [`deopt_stash_push_mark`] answered `m` attributes to that body only an
    /// entry numbered above `m` ([`take_last_deopt_with_point_pushed_after`]).
    seq: u64,
    /// HANDLER11-3 (JIT round 19 wave 12, lane handler12): the consumer that
    /// COMPLETED the call this entry's trap belonged to and left the entry on
    /// the stash ([`note_stash_left_by`]), i.e. the birth of an orphan. Named
    /// by every later discard under `CRATONVM_DBG_DEOPT`. `None` for an entry
    /// no completing consumer passed over (and after a restash).
    left_by: Option<&'static str>,
    /// The trapping frame's base (`rbp`) the trampoline reconstructed the
    /// frame against (interpreter round i6 wave 123, lane L2; slice 4 of
    /// stage 3b of `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
    /// the key a debugger's pending write to a compiled activation is matched
    /// by when the activation's post-call exit rebuilds it
    /// ([`peek_last_deopt_frame_rbp`], [`taken_frame_rbp`]). 0 when no framed
    /// trampoline pushed the entry (the re-run sentinel, the exceptional
    /// stash); a restash of the same point keeps it ([`TakenEntry::frame_rbp`]).
    /// Only ever compared, never dereferenced.
    frame_rbp: u64,
}

/// `CRATONVM_DEOPT_STASH_NAMES_TRAPPING_BODY` -- interpreter round i2 wave
/// 60, lane L2 (`i59-L2-proposal-every-stash-names-its-trapping-body`): the
/// deopt trampolines stash a weak reference to the body that trapped beside
/// the frame, and the call-site service's chain builder
/// (`vm::...::deopt_resume::build_deopt_frame_chain_for_point`) asks it before
/// the JIT cache and the code-range walk. Off: nothing is stashed, and the
/// IR deopt stub calls [`ir_deopt_entry`] as before. **Default ON** since the
/// wave-60 lane's switch-on commit (revert that commit to turn it off by
/// default). Read per trap (cold) and per IR compile with a deopt stub; not
/// cached in a static.
pub fn stash_names_trapping_body_enabled() -> bool {
    true
}

/// The body that trapped at `point_addr`, when the entry this thread last
/// took from the ordinary stash was that point's and its trampoline named the
/// body ([`StashEntry::body`]), and the body is still alive and owns the
/// point. `None` otherwise (point 0, another point, the switch off at the
/// trap, a body already reclaimed). Interpreter round i2 wave 60, lane L2.
pub fn taken_trapping_body(point_addr: usize) -> Option<std::sync::Arc<crate::CompiledMethod>> {
    if point_addr == 0 {
        return None;
    }
    TAKEN_FORCED_MARK.with(|c| {
        let taken = c.borrow();
        if taken.point != point_addr {
            return None;
        }
        taken
            .body
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .filter(|body| body.owns_deopt_point(point_addr))
    })
}

/// [`StashEntry::forced`]: the trap was a forced chain exit (round 16 lane
/// chain, CH5W4-1 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`).
const FORCED_TRIP_FORCED: u8 = 1;
/// ... and a sink already counted it in its VM's census (`forced=`).
const FORCED_TRIP_CLAIMED: u8 = 2;
/// ... and a sink already left it uncharged (`uncharged=`).
const FORCED_TRIP_UNCHARGED: u8 = 4;
/// ... and a sink already resumed its chain (the per-point `resumed` count;
/// round 16 wave 14 lane framestate, CH16-2 of `jit-r16-chain-proposals-RETIRED-20261006.md`).
const FORCED_TRIP_RESUMED: u8 = 8;

/// The tag a chain point's FORCED pad sets in the `DeoptimizationPoint`
/// pointer it hands the shared deopt stub
/// (`ir_lower::Lowerer::emit_forced_chain_deopt`): bit 0, which a boxed point
/// (8-aligned) never has. [`ir_deopt_entry`] strips it and marks the stash
/// entry forced, so the forced bit travels with the trap itself and no sink
/// needs the trapping body to learn it. The tag is a compile-time property of
/// the pad (its own `MOV imm64`), never a runtime branch in the stub. Round 16
/// lane chain (CH5W4-1).
pub const FORCED_CHAIN_PAD_TAG: u64 = 1;

const _: () = assert!(std::mem::align_of::<DeoptimizationPoint>() >= 2);

thread_local! {
    /// The point address and forced mark of the entry this thread last took
    /// from a deopt stash ([`pop_stash_entry`]): a restash of the same point
    /// keeps the mark, and a sink's claim reads it
    /// ([`claim_forced_trip_mark`]). Round 16 lane chain (CH5W4-1): the take /
    /// restash API keeps its tuple shape (84 VM call sites); the mark rides
    /// beside it, per thread like the stash itself. Interpreter round i2 wave
    /// 60 (lane L2): the entry's trapping body rides here too
    /// ([`TakenEntry::body`], [`taken_trapping_body`]).
    static TAKEN_FORCED_MARK: std::cell::RefCell<TakenEntry> =
        const {
            std::cell::RefCell::new(TakenEntry {
                point: 0,
                mark: 0,
                body: None,
                seq: 0,
                pushes: 0,
                trap_site: None,
                frame_rbp: 0,
                last_frame: (0, None, 0),
            })
        };
}

/// What this thread last took from the ordinary stash ([`TAKEN_FORCED_MARK`]).
struct TakenEntry {
    /// The point address (0 when none fired).
    point: usize,
    /// The forced mark ([`FORCED_TRIP_FORCED`] and its census bits).
    mark: u8,
    /// The trapping body the trampoline named ([`StashEntry::body`]).
    body: Option<std::sync::Weak<crate::CompiledMethod>>,
    /// The push number of the entry this thread last took from the ordinary
    /// stash ([`StashEntry::seq`]), which a restash puts back (HANDLER11-1).
    seq: u64,
    /// How many fresh pushes this thread made onto a deopt stash, i.e. the
    /// number the last fresh push took (HANDLER11-1). Kept here rather than
    /// in a `thread_local!` of its own: it is per thread like the stash, and
    /// the crate's static count is ratcheted (`process_global_statics_ratchet`).
    pushes: u64,
    /// The trapping method (`ReconstructedFrame::method_key`), bci and body of
    /// the entry this thread last popped from the ordinary stash, until
    /// [`take_trap_site_if`] claims it. `OmitStackTraceInFastThrow` (the VM's
    /// `runtime::exceptions`): the implicit exception the interpreter raises
    /// when it re-executes that bytecode is the one the trapping body would
    /// have thrown in place.
    trap_site: Option<(String, u32, Option<std::sync::Weak<crate::CompiledMethod>>)>,
    /// The trapping frame's base of the entry this thread last took with its
    /// point ([`StashEntry::frame_rbp`]), which a restash of the same point
    /// puts back. Interpreter round i6 wave 123, lane L2.
    frame_rbp: u64,
    /// The frame base, trapping body and bci of the entry this thread last
    /// popped from the ordinary stash by ANY take ([`pop_stash_entry`]),
    /// for [`last_taken_frame`]. Interpreter round i6 wave 123, lane L2.
    last_frame: (u64, Option<std::sync::Weak<crate::CompiledMethod>>, u32),
}

/// The body that trapped at `method_key`'s `bci`, when that is the trap this
/// thread last took from the ordinary stash and nothing claimed it yet:
/// `Some(body)` claims it (`body` is `None` when the trampoline did not name
/// the body, or it has been reclaimed), `None` leaves it. The VM asks this
/// when the interpreter raises an implicit exception, to tell the re-execution
/// of a trapping bytecode (a compiled throw) from an ordinary interpreted one.
pub fn take_trap_site_if(
    method_key: &str,
    bci: u32,
) -> Option<Option<std::sync::Arc<crate::CompiledMethod>>> {
    TAKEN_FORCED_MARK.with(|c| {
        let mut taken = c.try_borrow_mut().ok()?;
        let matches = taken
            .trap_site
            .as_ref()
            .is_some_and(|(key, at, _)| *at == bci && key == method_key);
        if !matches {
            return None;
        }
        let (_, _, body) = taken.trap_site.take()?;
        Some(body.as_ref().and_then(std::sync::Weak::upgrade))
    })
}

/// What [`claim_forced_trip_mark`] learned about the trap a sink holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForcedTripMark {
    /// The trap at that point was a forced chain exit.
    pub forced: bool,
    /// ... and this is its first claim (count it once in the census: a
    /// restashed forced trip a later sink claims again is not recounted).
    pub first_claim: bool,
}

/// Round 16 lane chain (CH5W4-1): was the trap this thread last took at
/// `point_addr` a forced chain exit? Read from the stash mark the trampoline
/// set ([`FORCED_CHAIN_PAD_TAG`]), so it is exact at every sink -- a
/// superseded body, a sink that holds no artifact, a restashed refusal --
/// with no body lookup. Marks the trap claimed: only the first claim is
/// `first_claim`. `point_addr == 0` and any other point answer not forced.
pub fn claim_forced_trip_mark(point_addr: usize) -> ForcedTripMark {
    if point_addr == 0 {
        return ForcedTripMark::default();
    }
    TAKEN_FORCED_MARK.with(|c| {
        let mut taken = c.borrow_mut();
        let (addr, mark) = (taken.point, taken.mark);
        if addr != point_addr || mark & FORCED_TRIP_FORCED == 0 {
            return ForcedTripMark::default();
        }
        taken.mark = mark | FORCED_TRIP_CLAIMED;
        ForcedTripMark {
            forced: true,
            first_claim: mark & FORCED_TRIP_CLAIMED == 0,
        }
    })
}

/// Round 16 lane chain (CH5W4-1): the forced trap this thread last took is
/// being left uncharged; `true` only the first time for that trap, so the
/// census counts `uncharged=` once per trip however many sinks decide. (The
/// sinks' charge decision carries the claim's answer, not the point, hence
/// the taken entry; `false` when that entry was not forced.)
pub fn mark_taken_forced_trip_uncharged() -> bool {
    TAKEN_FORCED_MARK.with(|c| {
        let mut taken = c.borrow_mut();
        let (addr, mark) = (taken.point, taken.mark);
        if addr == 0 || mark & FORCED_TRIP_FORCED == 0 {
            return false;
        }
        taken.mark = mark | FORCED_TRIP_UNCHARGED;
        mark & FORCED_TRIP_UNCHARGED == 0
    })
}

/// Round 16 wave 14 (lane framestate, CH16-2 of `jit-r16-chain-proposals-RETIRED-20261006.md`):
/// the forced trap this thread last took had its chain resumed. `Some(point)`
/// only the first time for that trap, so the census credits each trip's
/// resume to its point once however many sinks report it; `None` when the
/// taken entry was not forced (a natural trap: the census does not track it).
pub fn mark_taken_forced_trip_resumed() -> Option<usize> {
    TAKEN_FORCED_MARK.with(|c| {
        let mut taken = c.borrow_mut();
        let (addr, mark) = (taken.point, taken.mark);
        if addr == 0 || mark & FORCED_TRIP_FORCED == 0 || mark & FORCED_TRIP_RESUMED != 0 {
            return None;
        }
        taken.mark = mark | FORCED_TRIP_RESUMED;
        Some(addr)
    })
}

/// What [`peek_last_deopt_trap_source`] answers: the bytecode a trapping body
/// was compiled from (`CompiledMethod::compiled_source`) and its constant-pool
/// generation (`CompiledMethod::compile_cp_stamp`).
pub type TrapSource = (
    std::sync::Arc<cratonvm_jit_api::CachedBytecodeMethod>,
    Option<u64>,
);

// ---------------------------------------------------------------------------
// Machine-state frame reconstruction (real-frame-deopt step 3)
// ---------------------------------------------------------------------------

/// The register file spilled by the deopt trampoline. `gpr` is indexed by
/// x86-64 GPR number (0 = RAX, 1 = RCX, … 15 = R15); `xmm` by XMM number
/// (0 = XMM0 … 15 = XMM15), each holding the low 64 bits of the vector register
/// (`movq`), which is all a scalar `float`/`double` occupies.
///
/// `FrameValue::Register(r)` resolves against `gpr[r]`; `XmmFloat(n)` /
/// `XmmDouble(n)` against `xmm[n]`. The naive IR lowerer spills every value to a
/// frame slot, so on that path this is unused (all `FrameValue`s are
/// `StackSlot`/constant); it exists so the resolver is complete for backends
/// that keep live values in registers at a safepoint.
///
/// `#[repr(C)]` fixes the field order: the x64 deopt stub spills the 16 GPRs
/// into the first 128 bytes and the 16 XMMs into the next 128 (256-byte region),
/// and `&gpr[0]` is the struct base — so the spill layout and this struct must
/// stay in lockstep (see `emit_deopt_stubs` in `x64.rs`).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SavedRegisters {
    pub gpr: [u64; 16],
    pub xmm: [u64; 16],
}

impl Default for SavedRegisters {
    fn default() -> Self {
        Self {
            gpr: [0; 16],
            xmm: [0; 16],
        }
    }
}

/// Resolve one `FrameValue` against live machine state.
///
/// Constants (`Int`/`Long`/`Float`/`Double`/`Object`/`Undefined`) and
/// not-yet-materialized `VirtualObject`s pass through unchanged. The machine
/// forms resolve against the spilled state: `Register(r)` → `gpr[r]` (as `Int`);
/// `XmmFloat(n)`/`XmmDouble(n)` → the low 32 / full 64 bits of `xmm[n]` (as
/// `Float`/`Double`); `StackSlot*(off)` reads `*(rbp + off)` from the live native
/// frame and tags it `Int`/`Object`/`Long`/`Float`/`Double` per the slot's typed
/// variant. The plain `StackSlot`/`Register` int case carries no per-slot ref/int
/// tag, so the snapshot emitter chooses the typed variant up front (oop mask, XMM
/// provenance, cat-2 width); see `real-frame-deopt.md`.
///
/// # Safety
/// `rbp` must be the still-live frame base for which `off` was computed, and
/// `off` must address a word inside that frame. The deopt trampoline calls
/// this *before* tearing the frame down, satisfying that invariant.
fn resolve_value(v: &FrameValue, regs: &SavedRegisters, rbp: u64) -> FrameValue {
    // A metadata defect must not take the VM down from inside a deopt stub, and
    // it must not silently yield a plausible-looking wrong value either. Both
    // are avoided by mapping the structured error to `Unsupported`, which every
    // resume sink already refuses (→ safe whole-method re-run). The error itself
    // is surfaced at compile time by `DeoptVerifier`, which is where it can
    // still be acted on.
    try_resolve_value(v, regs, rbp).unwrap_or(FrameValue::Unsupported)
}

/// [`resolve_value`], but reporting a structured error instead of falling back.
///
/// The only failure mode is a machine-location descriptor whose register number
/// is outside the 16-entry GPR/XMM files the deopt stub spills — a
/// [`FrameValue::Register`]`(17)` would otherwise index `gpr[17]` and panic
/// inside the trampoline. Frame-slot reads are unchecked by construction (the
/// `rbp`/offset contract is the caller's, documented on [`resolve_value`]);
/// the compile-time cross-check that an offset is inside the frame and inside
/// the oop map is [`DeoptVerifier`]'s job.
fn try_resolve_value(
    v: &FrameValue,
    regs: &SavedRegisters,
    rbp: u64,
) -> Result<FrameValue, DeoptMetadataError> {
    /// Checked read of GPR `r` from the spilled register file.
    fn gpr(regs: &SavedRegisters, r: u8) -> Result<u64, DeoptMetadataError> {
        regs.gpr
            .get(r as usize)
            .copied()
            .ok_or(DeoptMetadataError::RegisterFileIndexOutOfRange {
                bank: "gpr",
                index: r,
            })
    }
    /// Checked read of XMM `n` from the spilled register file.
    fn xmm(regs: &SavedRegisters, n: u8) -> Result<u64, DeoptMetadataError> {
        regs.xmm
            .get(n as usize)
            .copied()
            .ok_or(DeoptMetadataError::RegisterFileIndexOutOfRange {
                bank: "xmm",
                index: n,
            })
    }

    Ok(match v {
        FrameValue::Register(r) => FrameValue::Int(gpr(regs, *r)? as i64),
        FrameValue::RegisterLong(r) => FrameValue::Long(gpr(regs, *r)? as i64),
        // The register holds the raw heap pointer (0 == null) captured in-stub at
        // the guard — same as `StackSlotRef` but read from the spilled GPR file.
        FrameValue::RegisterRef(r) => FrameValue::Object(gpr(regs, *r)?),
        FrameValue::XmmFloat(n) => {
            // Low 32 bits of the spilled XMM ARE the IEEE-754 float pattern.
            FrameValue::Float(xmm(regs, *n)? & 0xFFFF_FFFF)
        }
        FrameValue::XmmDouble(n) => {
            // Full 64 bits of the spilled XMM ARE the IEEE-754 double pattern.
            FrameValue::Double(xmm(regs, *n)?)
        }
        FrameValue::StackSlot(off) => {
            let addr = (rbp as i64 + *off as i64) as u64 as *const i64;
            // SAFETY: see function-level contract — frame is live, slot in-frame.
            FrameValue::Int(unsafe { addr.read_unaligned() })
        }
        FrameValue::StackSlotRef(off) => {
            let addr = (rbp as i64 + *off as i64) as u64 as *const u64;
            // SAFETY: see function-level contract — frame is live, slot in-frame.
            // The word IS the object pointer (0 == null); the resume builds a
            // `Value::Object` from it. Reading it here (synchronously, before any
            // Java-heap allocation) keeps the oop current — no GC has run since
            // the guard captured it.
            FrameValue::Object(unsafe { addr.read_unaligned() })
        }
        FrameValue::StackSlotLong(off) => {
            let addr = (rbp as i64 + *off as i64) as u64 as *const i64;
            // SAFETY: see function-level contract — frame is live, slot in-frame.
            // The lowerer spills the full 64-bit `long`, so the raw word IS the
            // value; the resume builds a `Value::Long` (cat-2) from it.
            FrameValue::Long(unsafe { addr.read_unaligned() })
        }
        FrameValue::StackSlotFloat(off) => {
            let addr = (rbp as i64 + *off as i64) as u64 as *const u32;
            // SAFETY: see function-level contract — frame is live, slot in-frame.
            // The low 32 bits ARE the IEEE-754 float pattern; resume builds a
            // `Value::Float`.
            FrameValue::Float(unsafe { addr.read_unaligned() } as u64)
        }
        FrameValue::StackSlotDouble(off) => {
            let addr = (rbp as i64 + *off as i64) as u64 as *const u64;
            // SAFETY: see function-level contract — frame is live, slot in-frame.
            // The raw 64-bit word IS the IEEE-754 double pattern; resume builds a
            // `Value::Double` (cat-2).
            FrameValue::Double(unsafe { addr.read_unaligned() })
        }
        // A scalar-replaced object whose fields are still in *machine* form
        // (the guard-surviving SR producer emits `field_values` as the live
        // `StackSlot*`/`Const` of each stored field). Resolve each field NOW —
        // synchronously, before the frame is torn down and before any Java-heap
        // allocation — so the VM materializer (`field_value_to_value`) receives
        // concrete `Object`/`Int`/`Long`/`Float`/`Double` values it accepts.
        // A `VirtualObjectRef` field is an intra-frame id edge with no machine
        // location, so it passes through unchanged. Recursion handles nested
        // virtual objects (none are emitted in v1, but the resolver is general).
        FrameValue::VirtualObject(state) => {
            let mut resolved = state.clone();
            for fv in resolved.field_values.iter_mut() {
                *fv = try_resolve_value(fv, regs, rbp)?;
            }
            FrameValue::VirtualObject(resolved)
        }
        FrameValue::ConditionalVirtualObject(pair) => {
            let cond = try_resolve_value(&pair.0, regs, rbp)?;
            if matches!(cond, FrameValue::Int(0)) {
                FrameValue::Object(0)
            } else {
                let mut resolved = pair.1.clone();
                for fv in resolved.field_values.iter_mut() {
                    *fv = try_resolve_value(fv, regs, rbp)?;
                }
                FrameValue::VirtualObject(resolved)
            }
        }
        // Constants, `Undefined`, `Unsupported`, `VirtualObjectRef` and
        // `MaterializationRequired` carry no machine location: they pass through
        // to the resume sink exactly as the emitter wrote them. In particular a
        // `MaterializationRequired` must NOT be softened to `Undefined` here —
        // that would restore the silent-null reconstruction it exists to stop.
        other => other.clone(),
    })
}

fn resolve_frame_state_machine(
    state: &FrameState,
    regs: &SavedRegisters,
    rbp: u64,
    semantics: ResumeSemantics,
) -> ReconstructedFrame {
    let monitors = state
        .monitors
        .iter()
        .map(|m| MonitorInfo {
            object: resolve_value(&m.object, regs, rbp),
            lock_depth: m.lock_depth,
            relock: m.relock,
        })
        .collect();
    ReconstructedFrame {
        class_id: state.class_id,
        method_key: state.method_key.clone(),
        bci: state.bci,
        locals: state
            .locals
            .iter()
            .map(|v| resolve_value(v, regs, rbp))
            .collect(),
        stack: state
            .stack
            .iter()
            .map(|v| resolve_value(v, regs, rbp))
            .collect(),
        monitors,
        semantics,
        caller_frames: Vec::new(),
    }
}

/// Reconstruct a precise interpreter frame from a `DeoptimizationPoint` and
/// the live machine state captured at the trapping site.
///
/// The ONE frame reconstructor (round 11 wave 3 deleted the constant-only
/// `reconstruct_frame`, which no trampoline called): it reads
/// `Register`/`StackSlot` values out of the spilled register file and the
/// native stack, and passes constants through unchanged. The inlined caller
/// chain (`FrameState.caller`) is flattened into `caller_frames`, each resolved
/// against the same machine state (inlined frames share the physical frame).
///
/// Virtual (scalar-replaced) objects are NOT materialized here — that needs
/// GC-backed allocation and is the VM's job
/// (`vm/src/runtime/deopt_materialize.rs::materialize_virtual_objects`); they
/// pass through as `VirtualObject` for the resume sink to realize.
pub fn reconstruct_frame_from_machine_state(
    deopt: &DeoptimizationPoint,
    regs: &SavedRegisters,
    rbp: u64,
) -> ReconstructedFrame {
    let mut frame = resolve_frame_state_machine(&deopt.frame_state, regs, rbp, deopt.semantics);
    let mut callers = Vec::new();
    let mut next = deopt.frame_state.caller.as_deref();
    while let Some(c) = next {
        // Every linked caller is parked mid-`invoke`; see
        // `ReconstructedFrame::semantics`.
        callers.push(resolve_frame_state_machine(
            c,
            regs,
            rbp,
            ResumeSemantics::for_caller_scope(),
        ));
        next = c.caller.as_deref();
    }
    frame.caller_frames = callers;
    report_dead_home_poison(&frame);
    frame
}

/// JIT round 21 lane stress (FUZZ21-3): the scope and slot of the first
/// reference value in `frame` (its callers included) that reads
/// [`cratonvm_types::jit_activation::DEAD_HOME_POISON`] -- a frame word the
/// optimizing tier cleared as dead under `CRATONVM_DBG_DEAD_HOME_POISON` that
/// this frame state nevertheless names. `None` for every frame the poison
/// never reached, which is every frame with the switch off.
pub(crate) fn dead_home_poison_slot(
    frame: &ReconstructedFrame,
) -> Option<(String, u32, &'static str, usize)> {
    fn poisoned(v: &FrameValue) -> bool {
        match v {
            FrameValue::Object(w) => *w == cratonvm_types::jit_activation::DEAD_HOME_POISON,
            FrameValue::VirtualObject(state) => state.field_values.iter().any(poisoned),
            FrameValue::ConditionalVirtualObject(pair) => {
                pair.1.field_values.iter().any(poisoned)
            }
            _ => false,
        }
    }
    std::iter::once(frame)
        .chain(frame.caller_frames.iter())
        .find_map(|f| {
            let hit = f
                .locals
                .iter()
                .position(poisoned)
                .map(|i| ("local", i))
                .or_else(|| f.stack.iter().position(poisoned).map(|i| ("stack", i)))
                .or_else(|| {
                    f.monitors
                        .iter()
                        .position(|m| poisoned(&m.object))
                        .map(|i| ("monitor", i))
                });
            hit.map(|(kind, i)| (f.method_key.clone(), f.bci, kind, i))
        })
}

/// FUZZ21-3: a deopt about to hand the interpreter a dead-home-poisoned word
/// as a reference is a premature dead-home clear (or a frame state naming a
/// value its compiled code no longer holds) caught at the exit. With zero
/// clears the slot would have read as `null` and the program would have gone
/// on, wrong; here the run stops, naming the frame. Only the debug switch
/// writes the poison, so this never fires without it.
fn report_dead_home_poison(frame: &ReconstructedFrame) {
    // JIT round 21, lane revjit2: walk only under the switch that writes the
    // poison -- not a full frame walk (and an abort path) on every deopt.
    if !crate::ir_lower::dbg_dead_home_poison_enabled() {
        return;
    }
    if let Some((method, bci, kind, i)) = dead_home_poison_slot(frame) {
        eprintln!(
            "[cratonvm-deopt] FUZZ21-3: the deopt frame of {method} at bci {bci} reads the \
             dead-home poison in {kind}[{i}]: a home the optimizing tier cleared as dead is \
             named by this frame state (CRATONVM_DBG_DEAD_HOME_POISON); aborting"
        );
        std::process::abort();
    }
}

/// How deep a nested stash is allowed to get before the OUTERMOST entry is
/// dropped to make room.
///
/// Bounds a thread-local that a trampoline can push to without any consumer
/// running in between — the failure shape a plain `Vec` would turn from "one
/// frame silently lost" into "unbounded growth". Compiled-to-compiled call
/// nesting that reaches a trap on every level past eight is not a shape this VM
/// produces; if it ever is, the drop is counted rather than silent.
const MAX_STASH_DEPTH: usize = 8;

/// The depth cap of [`LAST_EXCEPTIONAL`] under
/// [`exceptional_stash_deep_cap_enabled`] (JIT round 17 wave 6, lane stashcap;
/// `r17-stashdrop-exceptional-stash-cap-overflow-drops-holds-FIXED-20261005.md`).
///
/// [`MAX_STASH_DEPTH`] exists for [`LAST_DEOPT`]'s leftovers. An exceptional
/// frame is different: it may name holds its compiled code kept for the sink
/// (block locks with `relock == false`, and under the exceptional hand-over a
/// self-locking body's method monitor), and an exception unwinding through N
/// compiled activations publishes N frames, callee first, before any consumer
/// takes the top. Dropping the outermost entry at nine (a recursion nine
/// compiled levels deep) loses a frame from INSIDE the jit crate, which has no
/// VM to release its holds with, so the hold leaks for good. Under this cap the
/// frame stays in the stash until the take that discards it, where the VM
/// releases it (`deopt_resume.rs` `take_exceptional_frame_releasing_discards`)
/// exactly as it releases every other discard. The cap stays finite so a
/// consumer that never takes cannot grow the thread-local (and its GC roots)
/// without bound; a drop past it is traced under `CRATONVM_DBG_DEOPT` with the
/// holds the frame names.
const MAX_EXCEPTIONAL_STASH_DEPTH: usize = 1024;



/// The depth cap for `cell`: [`MAX_EXCEPTIONAL_STASH_DEPTH`] for
/// [`LAST_EXCEPTIONAL`] under [`exceptional_stash_deep_cap_enabled`],
/// [`MAX_STASH_DEPTH`] otherwise. Called only once `cell` holds
/// [`MAX_STASH_DEPTH`] entries. Identifies the stash by address, so it takes
/// no borrow of it (the caller holds one).
fn stash_depth_cap(cell: &std::cell::RefCell<Vec<StashEntry>>) -> usize {
    let exceptional = LAST_EXCEPTIONAL
        .try_with(|c| std::ptr::eq(c, cell))
        .unwrap_or(false);
    if exceptional {
        MAX_EXCEPTIONAL_STASH_DEPTH
    } else {
        MAX_STASH_DEPTH
    }
}

/// How many compiled lock holds (`relock == false`, counted by depth) a
/// stashed frame names across its scopes: what a discard or a drop of it would
/// leave held unless a VM-side taker releases it.
fn stashed_frame_compiled_holds(frame: &ReconstructedFrame) -> u32 {
    std::iter::once(frame)
        .chain(frame.caller_frames.iter())
        .flat_map(|scope| scope.monitors.iter())
        .filter(|m| !m.relock)
        .map(|m| m.lock_depth)
        .sum()
}

/// Frames stashed on top of a stash that was already occupied — a trap arrived
/// while an earlier trap's frame was still unclaimed. See
/// [`deopt_stash_nesting_counts`].
static STASH_NESTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Frames discarded unclaimed: a consumer took the frame of the trap it was
/// servicing, and the older ones underneath it belonged to no live frame.
static STASH_DROPPED_UNCLAIMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Points refused entry to [`LAST_DEOPT`] because they were not re-execute
/// points. See [`ordinary_stash_frame`].
static STASH_NON_REEXECUTE_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(stashed over a non-empty stash, discarded unclaimed)`.
///
/// # What a frame UNDER the top of the stash is
///
/// Never a caller or a callee of the frame above it — it is a leftover. That
/// follows from what may enter [`LAST_DEOPT`] at all, which
/// [`ordinary_stash_frame`] enforces rather than assumes:
///
/// * Only a **re-execute** point: a guard that fires BEFORE the bytecode it
///   protects. A trap at an invoke bci therefore means the invoke has not run,
///   so there is no callee of it anywhere, stashed or otherwise.
/// * A frame reaching a guard at all means every call it made before that
///   point RETURNED. A callee that returned the `i64::MIN` sentinel does not
///   let its caller continue to a later guard: both backends branch straight
///   to their call-exception exit, which returns the sentinel WITHOUT stashing
///   the caller's frame (`ir_lower.rs` `emit_call_exc_stub`, the single-pass
///   `emit_post_invoke_exception_check`; the only frame either publishes there
///   is a `PendingException` one, which goes to [`LAST_EXCEPTIONAL`]).
///
/// So the chain "B trapped, nobody claimed it, then its caller A trapped too"
/// cannot be produced — A never stashes on B's sentinel. What CAN happen is a
/// frame whose trap was handled by a consumer that never took it (the
/// first-call tier-up sink used to be one; see its note in `interpreter.rs`),
/// followed later by an unrelated trap on the same thread. The stack keeps
/// the older frame's oops visible to the collector
/// ([`for_each_stashed_deopt_object`]) until then, and the take discards it —
/// correctly, since resuming it would run a frame that has already been dealt
/// with.
///
/// That is why the stack is not resumed as a chain (the `NOTES-deopt2.md` M3
/// proposal): pushing the older frame as a "caller" parked after its invoke
/// would resume a stale frame. It is also why a take that discards does NOT
/// forbid resuming the frame it returns: that frame is a re-execute point
/// whose invoke, if its bci holds one, has not run.
///
/// Expected to stay at zero. Non-zero means some consumer handled a trap
/// without taking its frame, and `vm-cli` names that at exit.
pub fn deopt_stash_nesting_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering;
    (
        STASH_NESTED.load(Ordering::Relaxed),
        STASH_DROPPED_UNCLAIMED.load(Ordering::Relaxed),
    )
}

/// Push `frame` onto `cell`, counting the nesting and enforcing
/// [`MAX_STASH_DEPTH`].
fn push_stash(
    cell: &std::cell::RefCell<Vec<StashEntry>>,
    frame: ReconstructedFrame,
    restash: bool,
) {
    push_stash_with_cause(cell, frame, None, 0, restash);
}

/// [`push_stash`] with the cause of the point that fired, and that point's
/// address, kept beside the frame (see [`DeoptCause`], `StashEntry::point_addr`).
fn push_stash_with_cause(
    cell: &std::cell::RefCell<Vec<StashEntry>>,
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    point_addr: usize,
    restash: bool,
) {
    push_stash_with_source(cell, frame, cause, point_addr, None, restash);
}

/// [`push_stash_with_cause`], plus the trapping body's own bytecode
/// (`StashEntry::source`).
fn push_stash_with_source(
    cell: &std::cell::RefCell<Vec<StashEntry>>,
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    point_addr: usize,
    source: Option<TrapSource>,
    restash: bool,
) {
    // Round 16 lane chain (CH5W4-1): a restash puts back what this thread just
    // took, so the same point's forced mark goes back with it; a fresh trap's
    // mark is the trampoline's ([`push_stash_forced_trap`]).
    // Wave 60 (lane L2): the trapping body goes back the same way.
    let (forced, body) = if restash && point_addr != 0 {
        TAKEN_FORCED_MARK.with(|c| {
            let taken = c.borrow();
            if taken.point == point_addr {
                (taken.mark, taken.body.clone())
            } else {
                (0, None)
            }
        })
    } else {
        (0, None)
    };
    push_stash_with_mark(
        cell, frame, cause, point_addr, source, restash, forced, body,
    );
}

/// [`push_stash_with_source`] for a FRESH trap that a chain point's forced pad
/// sent ([`FORCED_CHAIN_PAD_TAG`]): the entry carries the forced mark. Round
/// 16 lane chain (CH5W4-1).
fn push_stash_forced_trap(
    cell: &std::cell::RefCell<Vec<StashEntry>>,
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    point_addr: usize,
    body: Option<std::sync::Weak<crate::CompiledMethod>>,
) {
    push_stash_with_mark(
        cell,
        frame,
        cause,
        point_addr,
        None,
        false,
        FORCED_TRIP_FORCED,
        body,
    );
}

/// The one push every stash path ends in.
#[allow(clippy::too_many_arguments)]
fn push_stash_with_mark(
    cell: &std::cell::RefCell<Vec<StashEntry>>,
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    point_addr: usize,
    source: Option<TrapSource>,
    restash: bool,
    forced: u8,
    body: Option<std::sync::Weak<crate::CompiledMethod>>,
) {
    use std::sync::atomic::Ordering;
    let mut slot = cell.borrow_mut();
    // A restash is the second half of take-inspect-restash and is putting back
    // what it just took, so it is not nesting even when the stack is non-empty.
    if !restash && !slot.is_empty() {
        STASH_NESTED.fetch_add(1, Ordering::Relaxed);
    }
    if slot.len() >= MAX_STASH_DEPTH && slot.len() >= stash_depth_cap(cell) {
        // Drop the OUTERMOST, not the innermost: the innermost is the frame the
        // very next consumer will be asked about.
        let dropped = slot.remove(0);
        STASH_DROPPED_UNCLAIMED.fetch_add(1, Ordering::Relaxed);
        // Round 17 wave 6, lane stashcap: the drop happens here, in the jit
        // crate, so nothing can release what the frame names; say so, as
        // `pop_stash_entry` does for a discard.
        if false {
            let held = stashed_frame_compiled_holds(&dropped.frame);
            if held > 0 {
                eprintln!(
                    "[cratonvm-deopt] the stash depth cap dropped a stashed frame {} bci={} \
                     that names {held} compiled lock hold(s); nothing releases them",
                    dropped.frame.method_key, dropped.frame.bci,
                );
            }
        }
    }
    // HANDLER11-1: a fresh push takes the next number; a restash puts back
    // what this thread just took, with the number it had.
    // Interpreter round i6 wave 123, lane L2: and a restash of the same point
    // its frame base; a fresh push gets its base from the trampoline
    // ([`note_pushed_frame_rbp`]).
    let (seq, frame_rbp) = TAKEN_FORCED_MARK.with(|c| {
        let mut taken = c.borrow_mut();
        if restash {
            let rbp = if point_addr != 0 && taken.point == point_addr {
                taken.frame_rbp
            } else {
                0
            };
            (taken.seq, rbp)
        } else {
            taken.pushes += 1;
            (taken.pushes, 0)
        }
    });
    slot.push(StashEntry {
        frame,
        cause,
        point_addr,
        source,
        forced,
        body,
        seq,
        left_by: None,
        frame_rbp,
    });
}

/// Record `rbp`, the frame base a trampoline just reconstructed against,
/// beside the entry it just pushed onto [`LAST_DEOPT`] (interpreter round i6
/// wave 123, lane L2; [`StashEntry::frame_rbp`]). Called right after the push,
/// on the same thread, so the innermost entry is that one. Cold: one store
/// per trap.
fn note_pushed_frame_rbp(rbp: u64) {
    LAST_DEOPT.with(|c| {
        if let Some(top) = c.borrow_mut().last_mut() {
            top.frame_rbp = rbp;
        }
    });
}

/// [`last_taken_frame`], consumed: the record is cleared as it is read, so a
/// later frame build on this thread that popped nothing reads a frame base of
/// 0 and matches nothing (interpreter round i6 wave 124, lane L2; the stale
/// take of `i124-L9-compiled-home-write-matches-a-stale-take`). The VM's
/// apply of a debugger's pending compiled-home write reads it through this.
pub fn take_last_taken_frame() -> (u64, Option<std::sync::Arc<crate::CompiledMethod>>, u32) {
    TAKEN_FORCED_MARK.with(|c| match c.try_borrow_mut() {
        Ok(mut taken) => {
            let (rbp, body, bci) = std::mem::take(&mut taken.last_frame);
            (rbp, body.as_ref().and_then(std::sync::Weak::upgrade), bci)
        }
        Err(_) => (0, None, 0),
    })
}

/// Forget what this thread last popped from a deopt stash
/// ([`last_taken_frame`]): the VM calls it when a debugger records a write to
/// one of this thread's compiled activations, so a pop made before the
/// record can never be matched to it (interpreter round i6 wave 124, lane
/// L2).
pub fn clear_last_taken_frame() {
    let _ = take_last_taken_frame();
}

/// Pop the innermost frame from `cell` and discard whatever is underneath it.
///
/// The discard is what the single slot used to do implicitly, and it has to
/// keep happening: a consumer that takes the innermost frame is the sink for
/// the trap that just arrived, and the frames below it are leftovers of traps
/// already handled (see [`deopt_stash_nesting_counts`] for why they cannot be
/// anything else). Leaving them stashed would let an unrelated later call site
/// find one and mistake it for its own trap — the exact identity confusion
/// `deopt_frame_matches_method` exists to catch, but arriving from a stale
/// stash rather than a live one. Counted, not silent.
fn pop_stash(cell: &std::cell::RefCell<Vec<StashEntry>>) -> Option<ReconstructedFrame> {
    pop_stash_entry(cell).map(|e| e.frame)
}

/// [`pop_stash`], keeping the entry's [`DeoptCause`].
///
/// # A discarded frame's locks are not released here
///
/// A discarded frame may name a lock its compiled code took
/// ([`MonitorInfo::relock`] `== false`). Nothing is released for it,
/// deliberately (interpreter round i1 wave 24, lane L2): the frame is a
/// leftover of a trap some consumer already handled without taking it, and
/// that consumer decided the activation's fate when it ran. If it re-ran the
/// method from entry, the hold leaked THEN, and whether it is still there now
/// depends on everything the thread did since (another frame may have
/// entered the same object legitimately); releasing it here, at an unrelated
/// later take, could release someone else's hold. The fix for such a leak
/// belongs at the consumer that did not take its frame, which
/// [`deopt_stash_nesting_counts`] names; `CRATONVM_DBG_DEOPT` also says when a
/// discarded frame named such a lock.
///
/// JIT round 17 wave 4, lane stashdrop: an exceptional-stash taker that can
/// release AT its own take uses
/// [`take_exceptional_frame_with_point_and_discards`] instead, which hands the
/// discarded frames back rather than dropping them here.
fn pop_stash_entry(cell: &std::cell::RefCell<Vec<StashEntry>>) -> Option<StashEntry> {
    use std::sync::atomic::Ordering;
    let mut slot = cell.borrow_mut();
    let entry = slot.pop()?;
    // [`take_trap_site_if`]: the trap a sink now holds, whichever sink.
    TAKEN_FORCED_MARK.with(|c| {
        if let Ok(mut taken) = c.try_borrow_mut() {
            taken.trap_site = Some((
                entry.frame.method_key.clone(),
                entry.frame.bci,
                entry.body.clone(),
            ));
            // Interpreter round i6 wave 123, lane L2 ([`last_taken_frame`]).
            taken.last_frame = (entry.frame_rbp, entry.body.clone(), entry.frame.bci);
        }
    });
    let discarded = slot.len();
    if discarded > 0 {
        STASH_DROPPED_UNCLAIMED.fetch_add(discarded as u64, Ordering::Relaxed);
        if false {
            for stale in slot.iter() {
                let held = stashed_frame_compiled_holds(&stale.frame);
                if held > 0 {
                    eprintln!(
                        "[cratonvm-deopt] discarded an unclaimed stashed frame {} bci={} that \
                         names {held} compiled lock hold(s); its trap's consumer never took it \
                         (left by {})",
                        stale.frame.method_key,
                        stale.frame.bci,
                        stale.left_by.unwrap_or("?"),
                    );
                }
            }
        }
        slot.clear();
    }
    Some(entry)
}

/// The frame a trampoline may push onto [`LAST_DEOPT`] for `point`.
///
/// **The invariant [`deopt_stash_nesting_counts`] rests on, enforced here.**
/// Every consumer of the ordinary stash resumes AT the frame's bci, and the
/// argument that a stale frame under the top can never be a live callee needs
/// every stashed frame to be a re-execute point. Both backends stamp
/// [`ResumeSemantics::for_reason`] today, which is `REEXECUTE` for every reason
/// but `PendingException` — and a `rethrow_exception` point is routed to
/// [`LAST_EXCEPTIONAL`] by both trampoline entries before this is reached, so
/// in practice the only shape this gate still turns away is a plain `RESUME`.
/// A producer that later stashes a resume-AFTER point (a caller parked
/// mid-invoke, a point after a returning call) would make that argument false
/// silently; this makes it loud and safe instead. The `RETHROW` arm is kept
/// rather than debug-asserted away: it costs one comparison on a path that has
/// just rebuilt a frame, and being unreachable by routing is not the same as
/// being unreachable.
///
/// Such a frame is replaced by the identity-less `bci == u32::MAX` re-run
/// sentinel every sink already refuses, so the trap takes the whole-method
/// re-run and its side-effect gate — never a resume at the wrong side of a
/// call. Counted ([`deopt_stash_non_reexecute_refused`]) and traced under
/// `CRATONVM_DBG_DEOPT`.
fn ordinary_stash_frame(
    point: &DeoptimizationPoint,
    frame: ReconstructedFrame,
) -> ReconstructedFrame {
    // `resume_kind`: a site trap's `first_arrival` fact is not a different
    // resume, and its frame must reach the sinks with the fact intact.
    if point.semantics.resume_kind() == ResumeSemantics::REEXECUTE {
        return frame;
    }
    STASH_NON_REEXECUTE_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if false {
        eprintln!(
            "[cratonvm-deopt] refusing a {} point into the ordinary stash: {} bci={} \
             reason={:?} -- stashing the re-run sentinel instead",
            point.semantics, frame.method_key, frame.bci, point.reason,
        );
    }
    rerun_sentinel_frame()
}

/// The identity-less `bci == u32::MAX` frame every resume sink refuses, so the
/// trap takes the whole-method re-run.
fn rerun_sentinel_frame() -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: String::new(),
        bci: u32::MAX,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        // Not load-bearing: `bci == u32::MAX` is what every sink refuses on,
        // and this frame names no bytecode to have semantics about. The
        // re-execute default is the one that cannot be mistaken for a licence
        // to skip an instruction if a future sink ever reads it first.
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

/// How many deopt points [`ordinary_stash_frame`] refused because they were
/// not re-execute points. Zero on every producer in the tree; non-zero names a
/// producer that broke the stash's invariant.
pub fn deopt_stash_non_reexecute_refused() -> u64 {
    STASH_NON_REEXECUTE_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    /// Frames reconstructed by [`ir_deopt_entry`] / [`x64_deopt_entry`],
    /// innermost LAST, consumed via [`take_last_deopt`].
    ///
    /// # Why a stack and not a slot
    ///
    /// Both trampoline entries used to **assign** this cell, discarding
    /// whatever was already there, and the take/restash discipline's own
    /// comment ("callers only restash what they just took, with no interleaving
    /// stash possible on the same thread") is a statement about the
    /// *consumers* — not about the trampolines, which can fire again at any
    /// point before a consumer claims the stash.
    ///
    /// The motivating shape was recorded as "`B` traps, the callee-deopt
    /// service declines, the sentinel propagates and `A` deopts on it too",
    /// with `A`'s trampoline overwriting `B`'s frame. `A` does not deopt on
    /// it: neither backend stashes a caller's frame when a callee returns the
    /// sentinel (see [`deopt_stash_nesting_counts`]). The slot's real hazard
    /// was the one a stack still answers: a frame no consumer took, overwritten
    /// by — or worse, left in place for — an unrelated later trap.
    ///
    /// Keeping the frames does two things a slot could not: the collector sees
    /// every stashed frame's oops for as long as it is stashed, and a leftover
    /// is counted ([`deopt_stash_nesting_counts`]) instead of silent. Only
    /// re-execute points may enter ([`ordinary_stash_frame`]), which is what
    /// makes every leftover a stale frame rather than a live caller chain.
    static LAST_DEOPT: std::cell::RefCell<Vec<StashEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Take (and clear) the frame most recently reconstructed by a deopt.
pub fn take_last_deopt() -> Option<ReconstructedFrame> {
    // HANDLER11-1: the push number rides beside the take, so a plain
    // `restash_last_deopt` of this frame keeps it.
    LAST_DEOPT
        .with(pop_stash_entry)
        .map(note_taken_seq)
        .map(|e| e.frame)
}

/// HANDLER11-1: remember the push number of the ordinary-stash entry a plain
/// take ([`take_last_deopt`]) just took, for its restash. The point, mark and
/// body of the last point-carrying take are left as they were.
fn note_taken_seq(e: StashEntry) -> StashEntry {
    TAKEN_FORCED_MARK.with(|c| c.borrow_mut().seq = e.seq);
    e
}

/// HANDLER11-4 (JIT round 19 wave 12, lane handler12): the consumer that
/// already completed the call of the innermost entry's trap and left it
/// ([`StashEntry::left_by`]), `None` when no completing consumer stamped it.
/// A call-site service never claims a stamped entry: it is nobody's live trap.
pub fn peek_last_deopt_left_by() -> Option<&'static str> {
    LAST_DEOPT.with(|c| c.borrow().last().and_then(|e| e.left_by))
}

/// HANDLER11-3 (JIT round 19 wave 12, lane handler12): the push number of the
/// innermost ordinary-stash entry, `None` when nothing is stashed. A consumer
/// of a callee's sentinel reads it on entry and hands it to
/// [`note_stash_left_by`] when it completes the call.
pub fn deopt_stash_top_seq() -> Option<u64> {
    LAST_DEOPT.with(|c| c.borrow().last().map(|e| e.seq))
}

/// HANDLER11-3: a consumer of a callee's sentinel just COMPLETED the call (it
/// hands its caller a value, or a throwable as a door does) and `entry_top`
/// is what [`deopt_stash_top_seq`] answered when it began. When that same
/// entry is still the innermost one, nothing will ever take it: no sentinel
/// travels on to a sink that could. That is an orphan's birth; it is stamped
/// with `label` ([`StashEntry::left_by`], named by every later discard) and
/// named once under `CRATONVM_DBG_DEOPT`. When `drop` answers `true` (asked
/// only for an orphan) the entry (and every
/// older one beneath it, as a take discards them) is discarded at once,
/// counted with the stash's unclaimed discards; nothing it names is released,
/// as with every discard ([`pop_stash_entry`]). Returns whether the entry was
/// left by this consumer.
pub fn note_stash_left_by(
    entry_top: Option<u64>,
    label: &'static str,
    drop: impl FnOnce() -> bool,
) -> bool {
    let Some(seq) = entry_top else {
        return false;
    };
    let left = LAST_DEOPT.with(|c| {
        let mut slot = c.borrow_mut();
        let Some(top) = slot.last_mut() else {
            return false;
        };
        if top.seq != seq {
            return false;
        }
        if top.left_by.is_none() {
            top.left_by = Some(label);
            if false {
                eprintln!(
                    "[cratonvm-deopt] orphan born: {label} completed its call and left the \
                     stashed frame {} bci={} (push {seq})",
                    top.frame.method_key, top.frame.bci,
                );
            }
        }
        true
    });
    if left && drop() {
        let dropped = LAST_DEOPT.with(|c| {
            let mut slot = c.borrow_mut();
            let n = slot.len();
            slot.clear();
            n
        });
        STASH_DROPPED_UNCLAIMED.fetch_add(dropped as u64, std::sync::atomic::Ordering::Relaxed);
    }
    left
}

/// HANDLER11-1 (JIT round 19 wave 11, lane handler11): this thread's deopt
/// stash push count now. A door that is about to enter a compiled body reads
/// it, and after the body returned the sentinel takes only an entry pushed
/// after it ([`take_last_deopt_with_point_pushed_after`]): an older one was on
/// the stash before the body ran, so it describes neither the body's
/// activation nor anything that activation called.
pub fn deopt_stash_push_mark() -> u64 {
    TAKEN_FORCED_MARK.with(|c| c.borrow().pushes)
}

/// [`take_last_deopt_with_point`] for a door that entered its body when
/// [`deopt_stash_push_mark`] answered `mark` (HANDLER11-1): the innermost entry
/// only when it was pushed after `mark` (a restash keeps the number of the
/// entry it puts back, so a frame a service inside the body took and put back
/// still counts as the body's). Otherwise no frame, and every entry numbered
/// at or below `mark` is DISCARDED, counted in [`deopt_stash_nesting_counts`]'s
/// second number, as [`pop_stash_entry`] discards the frames under the one it
/// takes: each such entry outlived the trap that pushed it with no consumer,
/// and left in place it is mistaken for its own trap by the next door or
/// call-site service on this thread (which match a stash by name and, at a
/// door, take whatever is on top), and it makes `jit_dispatch_threw` read a
/// genuine `Long.MIN_VALUE` as a sentinel. Like every discard, it releases
/// nothing the frame names (see [`pop_stash_entry`]).
///
/// The second element counts the discarded entries (0 when the take
/// succeeded or nothing was stashed).
pub fn take_last_deopt_with_point_pushed_after(
    mark: u64,
) -> (
    Option<(ReconstructedFrame, Option<DeoptCause>, usize)>,
    usize,
) {
    let fresh = LAST_DEOPT.with(|c| c.borrow().last().is_some_and(|e| e.seq > mark));
    if fresh {
        return (take_last_deopt_with_point(), 0);
    }
    let discarded = LAST_DEOPT.with(|c| {
        let mut slot = c.borrow_mut();
        let before = slot.len();
        if before == 0 {
            return 0;
        }
        if false {
            for stale in slot.iter().filter(|e| e.seq <= mark) {
                eprintln!(
                    "[cratonvm-deopt] a door discarded a stashed frame older than its body's \
                     entry: {} bci={} (push {} <= entry mark {mark}); its trap's consumer \
                     never took it (left by {})",
                    stale.frame.method_key,
                    stale.frame.bci,
                    stale.seq,
                    stale.left_by.unwrap_or("?"),
                );
            }
        }
        slot.retain(|e| e.seq > mark);
        before - slot.len()
    });
    if discarded > 0 {
        STASH_DROPPED_UNCLAIMED.fetch_add(discarded as u64, std::sync::atomic::Ordering::Relaxed);
    }
    (None, discarded)
}

/// [`take_last_deopt`], plus the [`DeoptCause`] of the point that stashed the
/// frame (`None` for the re-run sentinel of a null point, or for a frame put
/// back through the plain [`restash_last_deopt`]).
///
/// The exact reason and speculation id the trampoline was handed, so a sink
/// need not re-derive them from the bci — a derivation that answers
/// `Ambiguous` when two points share one (r11-tier-deopt-stash-drops-the-trap-reason).
///
/// Test-only since round 11 wave 10: every VM-side taker now takes the point
/// address too ([`take_last_deopt_with_point`]).
#[cfg(test)]
pub fn take_last_deopt_with_cause() -> Option<(ReconstructedFrame, Option<DeoptCause>)> {
    take_last_deopt_with_point().map(|(frame, cause, _)| (frame, cause))
}

/// [`take_last_deopt`], plus the [`DeoptCause`] of the point that stashed the
/// frame and the address of the point that fired
/// (0 when none did). A sink that looked the method's artifact up BY NAME asks
/// [`crate::CompiledMethod::owns_deopt_point`] with it to learn whether the
/// trap came out of that artifact or out of a body it superseded
/// (r11w6-lower-deopt-cause-artifact-identity-patch).
pub fn take_last_deopt_with_point() -> Option<(ReconstructedFrame, Option<DeoptCause>, usize)> {
    LAST_DEOPT
        .with(pop_stash_entry)
        .map(note_taken_forced_mark)
        .map(|e| (e.frame, e.cause, e.point_addr))
}

/// Round 16 lane chain (CH5W4-1): remember the point and forced mark of the
/// ordinary-stash entry this thread just took ([`TAKEN_FORCED_MARK`]). Every
/// take that hands out a point address records, a natural trap included (its
/// mark 0 replaces an older forced one at the same point).
fn note_taken_forced_mark(e: StashEntry) -> StashEntry {
    // Wave 60 (lane L2): and its trapping body ([`taken_trapping_body`]).
    TAKEN_FORCED_MARK.with(|c| {
        let mut taken = c.borrow_mut();
        taken.point = e.point_addr;
        taken.mark = e.forced;
        taken.body = e.body.clone();
        taken.seq = e.seq;
        // Interpreter round i6 wave 123, lane L2 ([`taken_frame_rbp`]).
        taken.frame_rbp = e.frame_rbp;
    });
    e
}

/// Put back a frame taken with [`take_last_deopt_with_point`], keeping its
/// cause and point (take-inspect-restash; not counted as nesting).
pub fn restash_last_deopt_with_point(
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    point_addr: usize,
) {
    LAST_DEOPT.with(|c| push_stash_with_cause(c, frame, cause, point_addr, true));
}

/// [`take_last_deopt_with_point`], plus the trapping body's own bytecode when
/// the trampoline named it ([`peek_last_deopt_trap_source`]).
///
/// Round 14 wave 3 (lane resume; proposal R14DP-3): the take-inspect-restash
/// pair of the call-site service (`helpers::try_resume_trapped_callee`) used
/// to drop it, so a frame the service declined reached the next sink without
/// the one fact that names its body exactly.
pub fn take_last_deopt_with_source() -> Option<(
    ReconstructedFrame,
    Option<DeoptCause>,
    usize,
    Option<TrapSource>,
)> {
    LAST_DEOPT
        .with(pop_stash_entry)
        .map(note_taken_forced_mark)
        .map(|e| (e.frame, e.cause, e.point_addr, e.source))
}

/// Put back a frame taken with [`take_last_deopt_with_source`], keeping its
/// cause, point and source (take-inspect-restash; not counted as nesting).
/// `source` must be the one the take answered for this very frame.
pub fn restash_last_deopt_with_source(
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    point_addr: usize,
    source: Option<TrapSource>,
) {
    LAST_DEOPT.with(|c| push_stash_with_source(c, frame, cause, point_addr, source, true));
}

/// One chain point's state under `CRATONVM_DEOPT_EAGER_CHAINS`
/// ([`crate::ir::ir_forced_chain_deopts_enabled`]): the arrival byte the
/// forced block `ir_lower::Lowerer::emit_forced_chain_deopt` bakes the address
/// of. Round 15 wave 4 (lane chain4; `r15w3-chain3-forced-chain-exit-bit-patch`).
///
/// Round 16 lane chain (CH5W4-1 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): the
/// forced trip no longer marks this cell (`in_flight`, `claimed` and
/// `body_counted` are retired, with `CompiledMethod::take_forced_chain_trip`).
/// It jumps to its own pad, which hands the stub the point pointer tagged
/// with [`FORCED_CHAIN_PAD_TAG`]; the trampoline stashes the frame exactly as
/// for a natural failure of the same point (so the soak still tests the
/// production resume path) plus a forced mark every sink reads from the trap
/// it took ([`claim_forced_trip_mark`]). Coverage is counted per PUBLISHED
/// body by the VM census ([`PublishedChainPoints`]), not here.
/// `#[repr(C)]` because the machine code addresses the field by offset.
#[repr(C)]
#[derive(Debug, Default)]
pub struct ForcedChainTripCell {
    /// Offset 0. `ADD BYTE [cell], 4` on every passing arrival: wraps to 0
    /// (ZF) on every 64th, which is the forced one (and on the first, from
    /// [`FORCED_CHAIN_FIRST_ARRIVAL_START`]: `Self::for_new_point`).
    pub arrivals: std::sync::atomic::AtomicU8,
}

// The forced block's machine code addresses this field by offset.
const _: () = assert!(std::mem::offset_of!(ForcedChainTripCell, arrivals) == 0);

/// `ForcedChainTripCell::arrivals`' start value when the first arrival is
/// forced too: the forced block's `ADD BYTE [cell], 4` wraps it to 0 (ZF) on
/// that arrival, then every 64th as before. Round 15 wave 4, lane chain5
/// (CH4W4-3).
pub const FORCED_CHAIN_FIRST_ARRIVAL_START: u8 = 0xFC;

impl ForcedChainTripCell {
    /// The cell the lowerer bakes for a new chain point
    /// (`ir_lower::Lowerer::emit_forced_chain_deopt`; only ever built under
    /// `CRATONVM_DEOPT_EAGER_CHAINS`). Round 15 wave 4, lane chain5 (CH4W4-3):
    /// under `CRATONVM_DEOPT_EAGER_CHAINS_FIRST` (**default ON**; inert
    /// without the eager switch, which is the only way a cell exists) the
    /// point's FIRST passing arrival is forced as well
    /// ([`FORCED_CHAIN_FIRST_ARRIVAL_START`]), so a chain point a body reaches
    /// once or twice (a cold error path's spliced helper, a `hashCode` reached
    /// once per resize) is soaked too; before, only a point reached 64 times by
    /// ONE body ever was. `=0` restores the round-13 every-64th-from-zero
    /// schedule, so that soak stays reproducible. Read once per chain point
    /// per compile, never on a hot path.
    ///
    /// j27 wave 4, lane DEOPTROWS (CH16-3): `CRATONVM_DEOPT_EAGER_CHAINS_SEED=<u64>`
    /// replaces both: the point's phase is [`seeded_forced_chain_start`] of
    /// the seed and `index` (the point's emission order in its body), so a
    /// soak run under several seeds forces different arrivals of the same
    /// points, each run reproducible.
    pub(crate) fn for_new_point(index: usize) -> Self {
        let start = if let Some(seed) = forced_chain_seed() {
            seeded_forced_chain_start(seed, index)
        } else if true {
            FORCED_CHAIN_FIRST_ARRIVAL_START
        } else {
            0
        };
        Self {
            arrivals: std::sync::atomic::AtomicU8::new(start),
        }
    }
}

/// `CRATONVM_DEOPT_EAGER_CHAINS_SEED` as a `u64` (surrounding blanks
/// ignored), `None` when unset or unparsable. j27 wave 4, lane DEOPTROWS
/// (CH16-3).
fn forced_chain_seed() -> Option<u64> {
    None::<String>
        .and_then(|v| v.trim().parse().ok())
}

/// The start of chain point `index`'s cell under `seed`: a multiple of 4, so
/// the forced block's `ADD BYTE [cell], 4` wraps to 0 on passing arrival
/// `64 - start / 4` (64 when the start is 0) and every 64th after it. The
/// splitmix64 finaliser of `seed` and `index`, so neighbouring points get
/// unrelated phases. j27 wave 4, lane DEOPTROWS (CH16-3).
pub(crate) fn seeded_forced_chain_start(seed: u64, index: usize) -> u8 {
    // Cast: an emission index, far below 2^64.
    let mut z = seed ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Cast: `z % 64` is below 64, so `* 4` stays below 256.
    ((z % 64) as u8) * 4
}

/// Round 16 lane chain (CH5W4-3 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): one
/// published body's chain points (its forced-block cells), described while
/// the body is certainly live -- at publication
/// ([`crate::JitCache::note_forced_chain_publication`]) -- so the VM's
/// eager-chains census counts `points=` for every body it published, reached
/// or not, and names each never-forced point at exit. Only ever built under
/// `CRATONVM_DEOPT_EAGER_CHAINS` (no cell exists otherwise).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedChainPoints {
    /// `(point address, description)` per chain point, in emission order. The
    /// address is the one a forced trap's stash carries
    /// ([`take_last_deopt_with_point`]); the description is the chain,
    /// innermost scope first (`method@bci <- caller@bci <- ... (Reason)`).
    pub points: Vec<(usize, String)>,
    /// How the body was published (`entry`, `osr`, `osr-optimizing`).
    pub kind: &'static str,
}

impl crate::CompiledMethod {
    /// This body's chain points as [`PublishedChainPoints`], or `None` when it
    /// has none (every body with `CRATONVM_DEOPT_EAGER_CHAINS` off). Round 16
    /// lane chain (CH5W4-3).
    pub fn forced_chain_points_for_census(
        &self,
        kind: &'static str,
    ) -> Option<PublishedChainPoints> {
        if self._forced_chain_deopt_points.is_empty() {
            return None;
        }
        let points = self
            ._forced_chain_deopt_points
            .iter()
            .map(|&p| {
                // u64 -> usize: a point address the code baked as imm64
                // (identity on the 64-bit JIT targets).
                let point = p as usize;
                (point, self.describe_chain_point(point))
            })
            .collect();
        Some(PublishedChainPoints { points, kind })
    }

    /// `method@bci <- caller@bci <- ... (reason)` for this body's deopt point
    /// at `point_addr`, innermost scope first. (Moved from the VM census by
    /// round 16 lane chain, so publication can describe a body it holds.)
    pub fn describe_chain_point(&self, point_addr: usize) -> String {
        let Some(point) = self
            ._deopt_point_boxes
            .iter()
            .find(|b| &***b as *const DeoptimizationPoint as usize == point_addr)
        else {
            return format!("point {point_addr:#x} (not among the body's deopt points)");
        };
        let mut scopes: Vec<String> = Vec::new();
        let mut fs = Some(&point.frame_state);
        while let Some(f) = fs {
            scopes.push(format!("{}@{}", f.method_key, f.bci));
            fs = f.caller.as_deref();
        }
        format!("{} ({:?})", scopes.join(" <- "), point.reason)
    }

    /// Is `addr` (the point address [`take_last_deopt_with_point`] hands out)
    /// one of THIS artifact's deopt points? 0 is never owned.
    ///
    /// Both homes are searched: an IR artifact's points are its
    /// `_deopt_point_boxes` (stable boxes), a single-pass artifact's are the
    /// elements of `deopt_points` (never reallocated after install, because
    /// its stubs bake the element addresses). Both live exactly as long as the
    /// artifact, so an address can name a DIFFERENT live artifact only after
    /// the trapping one was freed and the allocator reused its storage — and
    /// then the answer is `true`, i.e. the sink charges as it did before the
    /// address existed. The address only ever suppresses a charge.
    pub fn owns_deopt_point(&self, addr: usize) -> bool {
        if addr == 0 {
            return false;
        }
        self._deopt_point_boxes
            .iter()
            .any(|b| &**b as *const DeoptimizationPoint as usize == addr)
            || self
                .deopt_points
                .as_ptr_range()
                .contains(&(addr as *const DeoptimizationPoint))
    }
}

/// The [`DeoptCause`] of the innermost stashed frame, without clearing it —
/// the cause twin of [`peek_last_deopt_identity`]. `None` when nothing is
/// stashed or the top entry carries no cause.
pub fn peek_last_deopt_cause() -> Option<DeoptCause> {
    LAST_DEOPT.with(|c| c.borrow().last().and_then(|e| e.cause))
}

/// The address of the point that stashed the innermost frame, without
/// clearing it — the point twin of [`peek_last_deopt_cause`]; 0 when nothing
/// is stashed or no point described it. Only ever compared, never
/// dereferenced (see [`crate::CompiledMethod::owns_deopt_point`]).
pub fn peek_last_deopt_point() -> usize {
    LAST_DEOPT.with(|c| c.borrow().last().map_or(0, |e| e.point_addr))
}

/// The bytecode the body that stashed the innermost frame was compiled from,
/// and its constant-pool generation, without clearing the stash -- the source
/// twin of [`peek_last_deopt_point`]. `None` when nothing is stashed or the
/// trampoline could not name its body (every entry but a framed x64 trap of a
/// published, source-stamped body; see [`DeoptEpochGuard::compile_id`]).
///
/// Round 14 wave 2 (lane deopt; proposal R13RP6-1): read by the call-site
/// service (`helpers::try_resume_trapped_callee`), which holds no artifact and
/// so could not resume a trap out of a superseded body of a redefined class.
pub fn peek_last_deopt_trap_source() -> Option<TrapSource> {
    LAST_DEOPT.with(|c| c.borrow().last().and_then(|e| e.source.clone()))
}

/// The body that stashed the innermost frame, without clearing the stash --
/// the peek twin of [`taken_trapping_body`] for a sink that decides before it
/// takes (`helpers::try_resume_trapped_callee`). `Some` only when the
/// trampoline named the body ([`StashEntry::body`]), the body is still alive,
/// and it owns the entry's point. Interpreter round i2 wave 61, lane L2
/// (`i59-L2-proposal-every-stash-names-its-trapping-body`, the remaining
/// sinks).
pub fn peek_last_deopt_trapping_body() -> Option<std::sync::Arc<crate::CompiledMethod>> {
    LAST_DEOPT.with(|c| {
        let stash = c.borrow();
        let entry = stash.last()?;
        let point = entry.point_addr;
        if point == 0 {
            return None;
        }
        entry
            .body
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .filter(|body| body.owns_deopt_point(point))
    })
}

/// Put back a frame taken with [`take_last_deopt_with_cause`], keeping its
/// cause (the take-inspect-restash pattern; not counted as nesting).
pub fn restash_last_deopt_with_cause(frame: ReconstructedFrame, cause: Option<DeoptCause>) {
    restash_last_deopt_with_point(frame, cause, 0);
}

thread_local! {
    /// Frames published by a [`DeoptReason::PendingException`] stub — a
    /// method whose pending Java exception must be routed through its own
    /// exception table with precise locals. Same nesting discipline as
    /// [`LAST_DEOPT`], and for the same reason.
    ///
    /// Deliberately NOT `LAST_DEOPT`: every consumer of that stash treats it as
    /// "resume this method at `bci`", which for an exceptional frame executes
    /// past a call that never returned. Keeping it separate also keeps
    /// `has_last_deopt()` (which `jit_dispatch_threw` consults to disambiguate a
    /// legitimate `Long.MIN_VALUE` return) blind to it, so an exceptional frame
    /// nobody claims cannot make an unrelated later call site bail.
    static LAST_EXCEPTIONAL: std::cell::RefCell<Vec<StashEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Take (and clear) the pending-exception frame, if one was published by the
/// compiled method that just returned the deopt sentinel.
///
/// Test-only since interpreter round i1 wave 11: every VM-side taker now
/// takes the point too ([`take_exceptional_frame_with_point`]).
#[cfg(test)]
pub fn take_exceptional_frame() -> Option<ReconstructedFrame> {
    LAST_EXCEPTIONAL.with(pop_stash)
}

/// Put a taken pending-exception frame back (the take-inspect-restash pattern,
/// for a sink that discovers the frame is not its own to drop). Carries no
/// point; test-only since wave 11 for the reason [`take_exceptional_frame`] is
/// (VM sinks put back with [`restash_exceptional_frame_with_point`]).
#[cfg(test)]
pub fn restash_exceptional_frame(frame: ReconstructedFrame) {
    LAST_EXCEPTIONAL.with(|c| push_stash(c, frame, true));
}

/// [`take_exceptional_frame`], plus the [`DeoptCause`] and the address of the
/// point that published the frame (0 when none did: a frame put back through
/// the plain [`restash_exceptional_frame`]) — the exceptional twin of
/// [`take_last_deopt_with_point`].
///
/// A sink that holds the body it ran asks
/// [`crate::CompiledMethod::owns_deopt_point`] with the address, so a
/// same-named frame published by ANOTHER body (another loader's copy of the
/// class, a nested activation under a different artifact) is not routed as
/// its own (interpreter round i1 wave 11,
/// `interpreter-L7-deopt-sinks-match-stashed-frames-by-name-FIXED-20260925.md`).
pub fn take_exceptional_frame_with_point() -> Option<(ReconstructedFrame, Option<DeoptCause>, usize)>
{
    LAST_EXCEPTIONAL
        .with(pop_stash_entry)
        .map(|e| (e.frame, e.cause, e.point_addr))
}

/// Put back a frame taken with [`take_exceptional_frame_with_point`], keeping
/// its cause and point (take-inspect-restash; not counted as nesting).
pub fn restash_exceptional_frame_with_point(
    frame: ReconstructedFrame,
    cause: Option<DeoptCause>,
    point_addr: usize,
) {
    LAST_EXCEPTIONAL.with(|c| push_stash_with_cause(c, frame, cause, point_addr, true));
}

/// Is an exceptional frame stashed? The allocation-free "anything there" half
/// of [`peek_exceptional_identity`] (JIT round 19 wave 4, lane throws4,
/// THROWS3-3: the callee-deopt service's lean early out).
pub fn has_exceptional_frame() -> bool {
    LAST_EXCEPTIONAL.with(|c| !c.borrow().is_empty())
}

/// Peek the stashed exceptional frame's `(method_key, bci)` identity without
/// clearing it — the twin of [`peek_last_deopt_identity`].
///
/// `LAST_DEOPT` has had this since the groovy-regression fix, so a consumer can
/// verify a frame is its own *before* consuming it; `LAST_EXCEPTIONAL` had no
/// equivalent, so the only drop available was an unconditional one that took no
/// identity at all. Pair this with [`clear_exceptional_frame_of`], which is now
/// the ONLY way to drop an exceptional frame — see its doc for why the
/// unchecked form was deleted rather than kept beside it.
pub fn peek_exceptional_identity() -> Option<(String, u32)> {
    LAST_EXCEPTIONAL.with(|c| {
        c.borrow()
            .last()
            .map(|e| (e.frame.method_key.clone(), e.frame.bci))
    })
}

/// Drop the pending-exception frame **only if** it is the one named by
/// `(method_key, bci)`. Returns whether a frame was dropped.
///
/// **The only way to drop an exceptional frame — N6.** There used to be an
/// unchecked `clear_exceptional_frame()` beside this, whose own doc stated the
/// rule (*"sinks that merely pass an exception along must NOT call this"*) and
/// then dropped whatever was there, including a nested callee's frame that the
/// caller never published. A rule a function cannot check is documentation, not
/// enforcement, and that one could not check anything: it was handed no
/// identity at all. Its three VM-side callers now go through
/// `helpers::clear_own_exceptional_frame`, which peeks the identity first and
/// COUNTS a frame it declines to drop, so "was this ever happening" is a number.
///
/// A sink that abandons a compiled attempt and re-runs the callee in the
/// interpreter is entitled to drop the frame *that attempt* published — and
/// nothing else. An empty `method_key` never matches, which is the same
/// fail-closed rule `deopt_frame_matches_method` applies on the VM side.
pub fn clear_exceptional_frame_of(method_key: &str, bci: u32) -> bool {
    if method_key.is_empty() {
        return false;
    }
    LAST_EXCEPTIONAL.with(|c| {
        let mut slot = c.borrow_mut();
        match slot.last() {
            Some(e) if e.frame.method_key == method_key && e.frame.bci == bci => {
                slot.pop();
                true
            }
            _ => false,
        }
    })
}

/// How many pending-exception frames this thread has stashed.
///
/// gen r4w6/oomjit6 (2026-09-24): the ENTRY FLOOR a JIT door records
/// (`vm/src/jit/conservative_roots.rs`, `JitEntryGuard`), so a compiled catch
/// can tell the frames its own callees published during this throw from the
/// frames an outer sink already had stashed when the compiled activation was
/// entered. See [`truncate_exceptional_stash_to`].
pub fn exceptional_stash_depth() -> usize {
    LAST_EXCEPTIONAL.with(|c| c.borrow().len())
}

/// Drop every pending-exception frame stashed at index `floor` or above, and
/// return how many were dropped. A `floor` at or above the current depth
/// drops nothing.
///
/// gen r4w6/oomjit6 (2026-09-24), the entry-floor rule of
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// A frame leaves this stash only through a take (which also discards what is
/// beneath it) or through [`clear_exceptional_frame_of`] naming it. A compiled
/// method that CATCHES an exception locally is neither: the frames its own
/// callees published on their way out are left with no consumer, and every
/// object they name stays a GC root until the next take on the thread.
///
/// Why a floor is the right bound and not "every foreign frame": the caller
/// passes the stash depth recorded when the innermost JIT door of the catching
/// activation was entered. Between that door and the catching frame there is
/// only compiled code, and a compiled frame publishes only on its way OUT, so
/// every frame at index `floor` or above was published by a compiled frame
/// that has already unwound into the catch. A frame an outer sink stashed
/// before running Java is below the floor (the Java it runs enters through a
/// door of its own, which records the deeper floor), so it survives. A take or
/// a restash inside the span can only LOWER what sits above the floor, never
/// raise an older frame past it, so the rule can under-drop but never drop a
/// frame some consumer still owns.
pub fn truncate_exceptional_stash_to(floor: usize) -> usize {
    LAST_EXCEPTIONAL.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.len() <= floor {
            return 0;
        }
        let dropped = slot.len() - floor;
        slot.truncate(floor);
        STASH_DROPPED_UNCLAIMED.fetch_add(dropped as u64, std::sync::atomic::Ordering::Relaxed);
        dropped
    })
}

/// Pop the innermost pending-exception frame — keeping every frame beneath
/// it, unlike [`take_exceptional_frame_with_point`] — when the stash holds
/// more than `floor` frames and `claim` accepts the top one (asked with the
/// frame and the address of the point that published it). Returns the frame,
/// its cause and its point; `None` leaves the stash untouched.
///
/// Interpreter round i1 wave 25, lane L2: a door's no-exception sentinel arm
/// (`jit_bridge::run_jit_body_raw`, `try_osr`) claims the reason-9 frames its
/// own activation published on the way out — the pad of a protected call
/// whose callee returned the DEOPT sentinel — one at a time from the top,
/// down to the stash depth it recorded before entering the body, and stops at
/// the first frame it does not own. A take that discarded the frames beneath
/// would also destroy an outer sink's frame below the floor. `claim` runs
/// under the stash's borrow and must not touch the stash.
pub fn take_exceptional_frame_above_if(
    floor: usize,
    claim: impl FnOnce(&ReconstructedFrame, usize) -> bool,
) -> Option<(ReconstructedFrame, Option<DeoptCause>, usize)> {
    LAST_EXCEPTIONAL.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.len() <= floor {
            return None;
        }
        let top = slot.last()?;
        if !claim(&top.frame, top.point_addr) {
            return None;
        }
        slot.pop().map(|e| (e.frame, e.cause, e.point_addr))
    })
}

/// A pending-exception frame a take discarded from beneath the one it took,
/// with the address of the point that published it
/// ([`take_exceptional_frame_with_point_and_discards`]).
pub type DiscardedExceptionalFrame = (ReconstructedFrame, usize);

/// [`take_exceptional_frame_with_point`], but the frames the take discards
/// from beneath the top are HANDED BACK (outermost first, as they were
/// stashed) instead of dropped, so a VM-side taker can release what each one
/// names in the same Rust frame, right after its take. The stash ends exactly
/// as after [`take_exceptional_frame_with_point`] (empty), and the discards
/// are counted in the same `STASH_DROPPED_UNCLAIMED` (they were not claimed by
/// the consumer of their own trap). Returns `None`, and discards nothing, on
/// an empty stash.
///
/// JIT round 17 wave 4, lane stashdrop (site 1 of
/// `r17-syncdoor3-exceptional-frames-discarded-beneath-a-take-keep-their-holds-FIXED-20261005.md`):
/// [`pop_stash_entry`] refuses to release a discarded frame's holds at an
/// unrelated LATER take; the release here is at the very take that discards
/// them, which is the one place the objection does not reach. The VM decides
/// whether to release (`deopt_resume::CompiledLocksOfAStash`
/// `take_exceptional_frame_releasing_discards`, under the exceptional
/// hand-over's switches); this function only stops throwing the frames away.
#[allow(clippy::type_complexity)]
pub fn take_exceptional_frame_with_point_and_discards() -> Option<(
    ReconstructedFrame,
    Option<DeoptCause>,
    usize,
    Vec<DiscardedExceptionalFrame>,
)> {
    LAST_EXCEPTIONAL.with(|c| {
        let mut slot = c.borrow_mut();
        let entry = slot.pop()?;
        let discarded: Vec<DiscardedExceptionalFrame> =
            slot.drain(..).map(|e| (e.frame, e.point_addr)).collect();
        if !discarded.is_empty() {
            STASH_DROPPED_UNCLAIMED
                .fetch_add(discarded.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        Some((entry.frame, entry.cause, entry.point_addr, discarded))
    })
}

/// Peek (without clearing) whether a deopt frame is currently stashed.
///
/// Used by the VM's post-invoke sentinel-disambiguation helper
/// (`jit_dispatch_threw`): an IR-path deopt of a *dispatched callee* stashes a
/// frame here and returns the `i64::MIN` sentinel WITHOUT setting the VM-side
/// `JIT_DEOPT_PENDING` flag (this thread-local lives in the jit crate, which has
/// no access to the VM flag). The disambiguation helper must therefore treat a
/// stashed frame as a genuine deopt so a compiled caller bails instead of
/// mistaking the sentinel for a legitimate `Long.MIN_VALUE` return. Non-clearing
/// so the interpreter's outer `take_last_deopt` still consumes it.
pub fn has_last_deopt() -> bool {
    LAST_DEOPT.with(|c| !c.borrow().is_empty())
}

/// Peek the stashed frame's `(method_key, bci)` identity without clearing it.
///
/// Used by the dispatch-helper precise-resume arm
/// (`try_resume_trapped_callee`, vm/src/jit/helpers.rs) to decide — BEFORE
/// consuming the stash — whether the frame belongs to the compiled callee this
/// helper just invoked. A non-matching frame must stay stashed so it
/// propagates (with the sentinel) to the outer consumer that CAN attribute it.
pub fn peek_last_deopt_identity() -> Option<(String, u32)> {
    LAST_DEOPT.with(|c| {
        c.borrow()
            .last()
            .map(|e| (e.frame.method_key.clone(), e.frame.bci))
    })
}

/// Ask `f` about the innermost stashed frame without taking it -- the frame
/// twin of [`peek_last_deopt_identity`]; `None` when nothing is stashed.
/// Round 13 wave 9 (lane chain4): the call-site service asks it whether a
/// synchronized callee's frame carries the method monitor its self-locking
/// body handed over ([`handed_method_monitor_index`]) before it commits to
/// the take. `f` runs under the stash's borrow and must not touch the stash.
pub fn peek_last_deopt_frame<R>(f: impl FnOnce(&ReconstructedFrame) -> R) -> Option<R> {
    LAST_DEOPT.with(|c| c.borrow().last().map(|e| f(&e.frame)))
}

/// The stashed frame's OUTERMOST `(method_key, bci)` identity — N2.
///
/// [`peek_last_deopt_identity`] answers with the INNERMOST scope, which for an
/// ordinary frame is the whole story and for an inlined chain is a spliced
/// callee's name and a bci in that callee's space. The dispatch-helper resume
/// arm must ask a different question: *is this the method I just invoked?* —
/// and the method a compiled caller invoked is the OUTERMOST scope, the one the
/// artifact belongs to. Asking the innermost refuses every chain by
/// construction, because a spliced callee's name is not the call site's name.
///
/// The two answers are identical for a frame with no caller chain, which is
/// every frame in the tree today ([`ReconstructedFrame::caller_frames`] is
/// populated only by a splice that publishes deopt metadata, which
/// `jit/src/x64/inlining.rs`'s postcondition currently rolls back). This exists
/// so the consumer half is correct BEFORE that postcondition is relaxed — see
/// N2 and N5 in `NOTES-deopt.md`.
///
/// `caller_frames` runs innermost-first, so the outermost scope is its `last()`.
pub fn peek_last_deopt_outermost_identity() -> Option<(String, u32)> {
    LAST_DEOPT.with(|c| {
        c.borrow()
            .last()
            .map(|e| match e.frame.caller_frames.last() {
                Some(outer) => (outer.method_key.clone(), outer.bci),
                None => (e.frame.method_key.clone(), e.frame.bci),
            })
    })
}

/// Put a taken frame back (the take-inspect-restash pattern for consumers that
/// discover mid-flight they cannot handle it).
///
/// Pushes rather than assigns. The old form overwrote whatever was there on the
/// argument that "callers only restash what they just took, with no
/// interleaving stash possible on the same thread" — true of the consumers,
/// and not true of the trampolines, which is the whole of [`LAST_DEOPT`]'s
/// nesting note.
pub fn restash_last_deopt(frame: ReconstructedFrame) {
    LAST_DEOPT.with(|c| push_stash(c, frame, true));
}

#[cfg(test)]
mod stash_nesting_tests;

// ---------------------------------------------------------------------------
// GC visibility of the two stashes — see `docs/jit/deopt-thread-local-roots.md`
// ---------------------------------------------------------------------------
//
// [`LAST_DEOPT`] and [`LAST_EXCEPTIONAL`] hold [`ReconstructedFrame`]s whose
// slots carry **raw Java heap addresses**: [`FrameValue::Object`], produced at
// trap time by `resolve_value` from a `StackSlotRef` / `RegisterRef`. The read
// itself is current ("no GC has run since the guard captured it"), but nothing
// keeps it current afterwards.
//
// A collection CAN run inside the window, on this very thread. The shortest
// named one: a compiled method's post-invoke check
// (`jit/src/x64.rs::emit_post_invoke_exception_check`) routes **every**
// `i64::MIN` return at a protected bci into the reason-9 deopt stub, which
// publishes a `PendingException` frame here. The pending signal at that moment
// may be a bare NPE / AIOOBE / arithmetic flag rather than a throwable, and the
// VM sink then *allocates* the throwable — `throw_runtime_error` /
// `create_exception_object` in `vm/src/runtime/interpreter/invoke.rs` — BEFORE
// `route_jit_signal_exception` drains this stash and reads its locals. An
// allocation is a safepoint: threads stop "at their next safepoint (allocation
// site or backward branch)" (`vm/src/threading/gc_barrier.rs`). The same three
// arms sit *between* a stashed `LAST_DEOPT` frame and the `result == i64::MIN`
// drain, and they early-return without draining it at all.
//
// `jit/` cannot depend on `vm/`, so the storage cannot move onto `JvmThread` the
// way `jit_pending_exception` did. These two visitors are the alternative: they
// let the VM reach the stashes **from the owning thread**, which is the only
// place a thread-local is reachable at all — and is exactly where this VM
// already enumerates and rewrites per-thread roots
// (`memory/roots.rs::collect_roots`, `memory/gc.rs::update_all_roots`,
// `NativeContext::deposit_root_snapshot` / `check_post_block_gc`; all four run
// on the thread that owns the state, never on a peer's behalf).
//
// One exception (gci i9/ds, `gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`):
// a take-over pause that FREEZES a thread in compiled code reads that thread's
// stashes through the cell addresses it published ([`stash_cell_addrs`],
// [`for_each_stashed_deopt_object_of_frozen_peer`]) -- scan only, never remap:
// what it finds is pinned for the cycle, so the frozen owner has nothing to
// rewrite.
//
// **Both halves must be wired, in the same change.** A remap without a scan
// faithfully rewrites a reference to a reclaimed slot, which is worse than
// either failure alone. The debug assertion in [`remap_stashed_deopt_objects`]
// exists to catch exactly that half-wiring.

thread_local! {
    /// How many times this thread has offered its stashed frames to a root
    /// scan via [`for_each_stashed_deopt_object`]. Debug-only wiring check —
    /// see [`remap_stashed_deopt_objects`].
    static STASH_ROOT_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Visit every raw heap address in `values`, descending into scalar-replaced
/// object recipes.
///
/// A `VirtualObject`'s `field_values` are resolved to concrete values by
/// `resolve_value` at trap time (see its `FrameValue::VirtualObject` arm), so a
/// field can be an `Object` address exactly like a local or stack slot; missing
/// them would leave a scalar-replaced object's referents unrooted. A
/// `VirtualObjectRef` is an intra-frame id edge, not an address, and is also the
/// cycle terminator — not following it is what bounds this walk.
///
/// Address `0` is `null` and is never offered: callers are entitled to treat
/// every value they receive as a live heap reference.
fn visit_object_addrs(values: &[FrameValue], f: &mut dyn FnMut(u64)) {
    for v in values {
        match v {
            FrameValue::Object(addr) if *addr != 0 => f(*addr),
            FrameValue::VirtualObject(state) => visit_object_addrs(&state.field_values, f),
            FrameValue::ConditionalVirtualObject(pair) => {
                visit_object_addrs(&pair.1.field_values, f)
            }
            _ => {}
        }
    }
}

/// Mutable twin of [`visit_object_addrs`]: `f` returns the object's new address
/// when the collection moved it, `None` to leave the slot alone.
fn visit_object_addrs_mut(values: &mut [FrameValue], f: &mut dyn FnMut(u64) -> Option<u64>) {
    for v in values {
        match v {
            FrameValue::Object(addr) if *addr != 0 => {
                if let Some(moved) = f(*addr) {
                    *addr = moved;
                }
            }
            FrameValue::VirtualObject(state) => visit_object_addrs_mut(&mut state.field_values, f),
            FrameValue::ConditionalVirtualObject(pair) => {
                visit_object_addrs_mut(&mut pair.1.field_values, f)
            }
            _ => {}
        }
    }
}

impl ReconstructedFrame {
    /// Offer every raw heap address this frame holds to `f`, for a root scan.
    ///
    /// Covers all four containers that can hold one — `locals`, `stack`,
    /// `monitors[].object` and the inlined `caller_frames` chain — plus the
    /// field recipes of any scalar-replaced object nested in them. A slot family
    /// missed here is an unrooted reference, i.e. a use-after-free that surfaces
    /// days later somewhere else.
    pub fn for_each_object_address(&self, f: &mut dyn FnMut(u64)) {
        visit_object_addrs(&self.locals, f);
        visit_object_addrs(&self.stack, f);
        for m in &self.monitors {
            visit_object_addrs(std::slice::from_ref(&m.object), f);
        }
        for c in &self.caller_frames {
            c.for_each_object_address(f);
        }
    }

    /// Rewrite every raw heap address this frame holds through `f` (the GC
    /// pointer map). Same coverage as [`Self::for_each_object_address`] — the
    /// two must always agree, or a scanned slot goes un-remapped.
    pub fn for_each_object_address_mut(&mut self, f: &mut dyn FnMut(u64) -> Option<u64>) {
        visit_object_addrs_mut(&mut self.locals, f);
        visit_object_addrs_mut(&mut self.stack, f);
        for m in &mut self.monitors {
            visit_object_addrs_mut(std::slice::from_mut(&mut m.object), f);
        }
        for c in &mut self.caller_frames {
            c.for_each_object_address_mut(f);
        }
    }
}

/// Run `visit` against both stashes, tolerating a thread whose TLS is already
/// being torn down (such a thread cannot be mid-window).
fn with_both_stashes(mut visit: impl FnMut(&std::cell::RefCell<Vec<StashEntry>>)) {
    let _ = LAST_DEOPT.try_with(&mut visit);
    let _ = LAST_EXCEPTIONAL.try_with(&mut visit);
}

/// **Scan half.** Offer every raw heap address currently stashed in
/// [`LAST_DEOPT`] and [`LAST_EXCEPTIONAL`] to `f`, so the collector keeps those
/// objects alive.
///
/// Must be called from the thread that owns the stashes — a thread-local is
/// unreachable from anywhere else. In this VM that is not a restriction: root
/// enumeration is already per-thread and on-thread
/// (`memory/roots.rs::collect_roots(shared, thread)` for the collecting thread,
/// `NativeContext::deposit_root_snapshot` for a thread about to park).
///
/// Null (`0`) slots are skipped; every address handed to `f` is a live heap
/// reference.
pub fn for_each_stashed_deopt_object(mut f: impl FnMut(u64)) {
    let f: &mut dyn FnMut(u64) = &mut f;
    with_both_stashes(|cell| match cell.try_borrow() {
        Ok(slot) => {
            // EVERY stashed frame, not only the innermost: a nested callee's
            // frame stays stashed until something claims or discards it, and
            // its oops are just as live in the meantime.
            for entry in slot.iter() {
                entry.frame.for_each_object_address(&mut *f);
            }
        }
        Err(_) => debug_assert!(
            false,
            "deopt stash borrowed during a root scan: a stash accessor re-entered \
             the collector (see docs/jit/deopt-thread-local-roots.md)"
        ),
    });
    let _ = STASH_ROOT_SCANS.try_with(|c| c.set(c.get().saturating_add(1)));
}

/// **Remap half.** Rewrite every raw heap address stashed in [`LAST_DEOPT`] and
/// [`LAST_EXCEPTIONAL`] through the collection's pointer map: `f` returns the
/// new address for an object that moved, `None` for one that did not.
///
/// Same thread rule as [`for_each_stashed_deopt_object`], and the same pairing
/// rule: this is only sound when that scan ran for the same collection. Without
/// it the rewrite is applied to a reference the collector was free to reclaim —
/// a faithfully-updated pointer to a dead slot, which is worse than either
/// failure alone. Debug builds assert on that half-wiring rather than let it
/// ship quietly.
pub fn remap_stashed_deopt_objects(mut f: impl FnMut(u64) -> Option<u64>) {
    debug_assert!(
        STASH_ROOT_SCANS.try_with(|c| c.get()).unwrap_or(0) > 0
            || stashed_deopt_object_count() == 0,
        "remap_stashed_deopt_objects ran on a thread that never offered its deopt \
         stashes to a root scan — the fix is half-wired; see \
         docs/jit/deopt-thread-local-roots.md"
    );
    let f: &mut dyn FnMut(u64) -> Option<u64> = &mut f;
    with_both_stashes(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => {
            for entry in slot.iter_mut() {
                entry.frame.for_each_object_address_mut(&mut *f);
            }
        }
        Err(_) => debug_assert!(
            false,
            "deopt stash borrowed during a GC remap: a stash accessor re-entered \
             the collector (see docs/jit/deopt-thread-local-roots.md)"
        ),
    });
}

/// Whether both deoptimization stashes are empty and available to inspect.
/// A borrowed or torn-down stash cannot prove a precise native root deposit.
pub fn deopt_stashes_are_empty() -> bool {
    [&LAST_DEOPT, &LAST_EXCEPTIONAL].into_iter().all(|key| {
        key.try_with(|cell| cell.try_borrow().is_ok_and(|entries| entries.is_empty()))
            .unwrap_or(false)
    })
}

/// How many live heap references the two stashes currently hold on this thread.
///
/// Diagnostic / wiring check only — it is NOT a root provider. Does not count
/// as a scan.
pub fn stashed_deopt_object_count() -> usize {
    let mut n = 0usize;
    with_both_stashes(|cell| {
        if let Ok(slot) = cell.try_borrow() {
            for entry in slot.iter() {
                entry
                    .frame
                    .for_each_object_address(&mut |_addr: u64| n += 1);
            }
        }
    });
    n
}

/// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`): the
/// addresses of THIS thread's two stash cells ([`LAST_DEOPT`],
/// [`LAST_EXCEPTIONAL`]), for the VM to publish beside the thread's OS tid so
/// a take-over pause that freezes this thread in compiled code can root its
/// stashed frames ([`for_each_stashed_deopt_object_of_frozen_peer`]). `None`
/// during TLS teardown. Both cells are `const`-initialised thread-locals, so
/// the addresses are stable for the thread's whole life.
pub fn stash_cell_addrs() -> Option<(usize, usize)> {
    let deopt = LAST_DEOPT
        .try_with(|c| c as *const std::cell::RefCell<Vec<StashEntry>> as usize)
        .ok()?;
    let exceptional = LAST_EXCEPTIONAL
        .try_with(|c| c as *const std::cell::RefCell<Vec<StashEntry>> as usize)
        .ok()?;
    Some((deopt, exceptional))
}

/// t1 wave 5 lane E2 (stage 2 of
/// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`):
/// the calling thread's two stashes ([`LAST_DEOPT`], [`LAST_EXCEPTIONAL`]),
/// moved out for a continuation slice that runs on this OS thread while its
/// carrier stays blocked with compiled frames. Opaque: only
/// [`restore_deopt_stashes_aside`] reads it.
///
/// The stashes are per OS thread, but what they hold is the CARRIER's (its
/// compiled frames published them; its blocked deposit rooted them). Left in
/// place, the slice's own deposits would root them again and the slice's
/// collections remap them, and the carrier's wake would then remap the same
/// words a second time -- the double move a sliding compactor's `A -> B,
/// B -> C` map makes unsound; and an exceptional take in the slice could claim
/// a carrier frame (the point check refuses a foreign body, not a frame of the
/// same artifact at another depth). Moved out, the slice starts with empty
/// stashes, as on a runner.
pub struct DeoptStashesAside {
    deopt: Vec<StashEntry>,
    exceptional: Vec<StashEntry>,
}

impl DeoptStashesAside {
    /// How many frames are held aside (the carrier's).
    pub fn len(&self) -> usize {
        self.deopt.len() + self.exceptional.len()
    }

    /// No frame held aside.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Move the calling thread's two stashes out ([`DeoptStashesAside`]).
pub fn take_deopt_stashes_aside() -> DeoptStashesAside {
    let take = |key: &'static std::thread::LocalKey<std::cell::RefCell<Vec<StashEntry>>>| {
        key.try_with(|c| std::mem::take(&mut *c.borrow_mut()))
            .unwrap_or_default()
    };
    DeoptStashesAside {
        deopt: take(&LAST_DEOPT),
        exceptional: take(&LAST_EXCEPTIONAL),
    }
}

/// Put the stashes [`take_deopt_stashes_aside`] moved out back. Whatever the
/// slice left stashed is dropped: it belongs to frames of a continuation that
/// has yielded or ended (a slice cannot yield with a compiled frame), which is
/// what a runner's leftovers are too, and on the carrier's thread a carrier
/// compiled caller must not take it. Answers how many frames were dropped.
pub fn restore_deopt_stashes_aside(aside: DeoptStashesAside) -> usize {
    let DeoptStashesAside { deopt, exceptional } = aside;
    let put = |key: &'static std::thread::LocalKey<std::cell::RefCell<Vec<StashEntry>>>,
               back: Vec<StashEntry>| {
        key.try_with(|c| std::mem::replace(&mut *c.borrow_mut(), back).len())
            .unwrap_or(0)
    };
    put(&LAST_DEOPT, deopt) + put(&LAST_EXCEPTIONAL, exceptional)
}

#[cfg(test)]
mod t1e2_stash_aside_tests;

/// gci i9/ds: [`for_each_stashed_deopt_object`] for ANOTHER thread's stashes,
/// read through the cell addresses that thread published
/// ([`stash_cell_addrs`]). Returns `(stashed frames, addresses offered)`.
///
/// Read through `RefCell::try_borrow_unguarded`: nothing is written to the
/// owner's cells (no borrow count, no [`STASH_ROOT_SCANS`] bump -- that
/// counter is the owner's own scan/remap pairing, and a frozen peer's stashed
/// objects are pinned for the cycle, so the owner has nothing to remap), and a
/// cell the owner held mutably borrowed when it was frozen is skipped (only
/// Rust stash code borrows it mutably, and a take-over freezes a thread only
/// with its `Rip` in registered compiled code, so that does not happen;
/// skipping keeps the old root set).
///
/// # Safety
///
/// `deopt_cell` / `exceptional_cell` must be the pair [`stash_cell_addrs`]
/// returned on an OS thread that is alive and that the caller holds frozen
/// (it cannot run) for the whole call. A zero address is skipped.
pub unsafe fn for_each_stashed_deopt_object_of_frozen_peer(
    deopt_cell: usize,
    exceptional_cell: usize,
    mut f: impl FnMut(u64),
) -> (usize, usize) {
    let mut frames = 0usize;
    let mut offered = 0usize;
    for addr in [deopt_cell, exceptional_cell] {
        if addr == 0 {
            continue;
        }
        // SAFETY: the caller's contract: a live, frozen thread's stash cell.
        let cell = unsafe { &*(addr as *const std::cell::RefCell<Vec<StashEntry>>) };
        // SAFETY: the owner cannot run, so no borrow of the cell can begin or
        // end while this shared view lives; a borrow it held is refused.
        let Ok(slot) = (unsafe { cell.try_borrow_unguarded() }) else {
            continue;
        };
        for entry in slot.iter() {
            frames += 1;
            entry.frame.for_each_object_address(&mut |a: u64| {
                offered += 1;
                f(a)
            });
        }
    }
    (frames, offered)
}

#[cfg(test)]
mod deopt_stash_root_tests;

/// Stash the identity-less `bci == u32::MAX` re-run sentinel frame.
///
/// What a trampoline entry publishes when it cannot describe the trapping
/// frame at all (a null point or register-file pointer). The `i64::MIN` it
/// returns next must still read as a deopt rather than as a legitimate
/// `Long.MIN_VALUE` result — the interpreter's real-frame-deopt detection keys
/// on `result == i64::MIN && take_last_deopt().is_some()` — and the VM resume
/// path rejects `bci == u32::MAX` and re-runs the method in the interpreter.
///
/// Pushed, not assigned: the sentinel used to overwrite a real stashed frame
/// belonging to a nested callee with this identity-less one. See
/// [`LAST_DEOPT`].
fn stash_rerun_sentinel() {
    LAST_DEOPT.with(|c| push_stash(c, rerun_sentinel_frame(), false));
}

/// Deopt trampoline entry — called from JIT code when a guard fails.
///
/// The trampoline loads a pointer to the guard's `DeoptimizationPoint` into
/// the first argument register and the live `rbp` into the second, then calls
/// here. We reconstruct the interpreter frame from the live native frame and
/// stash it (see [`LAST_DEOPT`]). Returns the `i64::MIN` deopt sentinel so the
/// trampoline can return it as the method result, matching the existing JIT
/// deopt-signal convention.
///
/// # Safety
/// `point` must point at a live `DeoptimizationPoint` (owned by the running
/// `CompiledMethod`), and `rbp` must be the live frame base of the trapping
/// method. Both are guaranteed by the trampoline that calls this.
pub extern "C" fn ir_deopt_entry(
    point: *const DeoptimizationPoint,
    rbp: u64,
    regs: *const SavedRegisters,
) -> i64 {
    ir_deopt_entry_inner(point, rbp, regs, 0)
}

/// [`ir_deopt_entry`] with a 4th argument, the compile id the stub bakes for
/// its body: the entry names the trapping body by it and stashes it beside
/// the frame ([`StashEntry::body`]). Called by the shared IR deopt stub of a
/// compile that reserved an id, under [`stash_names_trapping_body_enabled`]
/// (interpreter round i2 wave 60, lane L2). Same contract as
/// [`ir_deopt_entry`].
pub extern "C" fn ir_deopt_entry_named(
    point: *const DeoptimizationPoint,
    rbp: u64,
    regs: *const SavedRegisters,
    compile_id: u64,
) -> i64 {
    // Cast: the stub bakes a `u32` id zero-extended into the register.
    ir_deopt_entry_inner(point, rbp, regs, compile_id as u32)
}

/// The weak reference of the body bound to `compile_id`, when that body owns
/// `point_addr` (so a stale or reissued id names nothing). Called only from a
/// trampoline entry, inside the trapping body's own frame: the artifact bound
/// to its id is then that body, live until its frame returns (the argument
/// [`trap_source_of_guard`] rests on). Cold: one table load, one point scan.
#[cold]
#[inline(never)]
fn trapping_body_of_compile_id(
    compile_id: u32,
    point_addr: usize,
) -> Option<std::sync::Weak<crate::CompiledMethod>> {
    let cm_ptr = crate::lookup_compile_id(compile_id)?;
    // SAFETY: a bound compile id names a live published artifact; the one
    // owning this point is executing the stub that called us (see above).
    let cm = unsafe { &*(cm_ptr as *const crate::CompiledMethod) };
    if !cm.owns_deopt_point(point_addr) {
        return None;
    }
    cm.bound_weak()
}

fn ir_deopt_entry_inner(
    point: *const DeoptimizationPoint,
    rbp: u64,
    regs: *const SavedRegisters,
    compile_id: u32,
) -> i64 {
    // Round 16 lane chain (CH5W4-1): a chain point's forced pad tags the
    // pointer ([`FORCED_CHAIN_PAD_TAG`]); strip it before anything reads the
    // point, and mark the stash entry forced below. No other pad sets bit 0.
    let tag = FORCED_CHAIN_PAD_TAG as usize;
    let forced = (point as usize & tag) != 0;
    let point = (point as usize & !tag) as *const DeoptimizationPoint;
    // Checked, not assumed. The contract above says `point` is non-null, but a
    // deopt trampoline is the worst place in the VM to find out that a
    // contract was broken: dereferencing null here is UB inside a stub with a
    // half-torn-down frame. A null pointer instead stashes the identity-less
    // `bci == u32::MAX` re-run sentinel — the VM resume path rejects that bci
    // and re-runs the method in the interpreter — so the `i64::MIN` return can
    // never be mistaken for a legitimate `Long.MIN_VALUE` result.
    if point.is_null() {
        stash_rerun_sentinel();
        return i64::MIN;
    }
    // SAFETY: contract documented above; non-null checked immediately above.
    let point = unsafe { &*point };
    // The register image, when this frame has one.
    //
    // It did not, until 2026-09-04: the comment here used to read "the IR
    // lowerer keeps every live value in a frame slot, so no register file is
    // needed", and that sentence was the reason a register-resident value in
    // that backend could never lose its home word — a deopt frame had nothing
    // but frame slots to name. `CRATONVM_JIT_IR_DEOPT_REGS=1` makes the stub
    // reserve a `SavedRegisters` region and spill the file into it; NULL is
    // still the answer with the flag off, and still means default zeros.
    let saved;
    let regs = if regs.is_null() {
        saved = SavedRegisters::default();
        &saved
    } else {
        // SAFETY: non-null here means the emitting stub reserved the region in
        // its own live frame and spilled 16 GPRs and 16 XMMs into it, and this
        // call happens before that frame's epilogue — the same lifetime
        // argument `x64_deopt_entry`'s `regs` rests on.
        unsafe { &*regs }
    };
    let frame = reconstruct_frame_from_machine_state(point, regs, rbp);
    let cause = Some(DeoptCause::of_point(point));
    let point_addr = point as *const DeoptimizationPoint as usize;
    // Route an exceptional frame to [`LAST_EXCEPTIONAL`], exactly as
    // [`x64_deopt_entry`] does and keyed on the same field. Round 10 wave 9
    // switched both routers from `reason == PendingException` to
    // `semantics.rethrow_exception`: the flag is the field whose documented job
    // is "this scope is not a resume point", and the two are equivalent on any
    // artifact that reached here (`DeoptMetadataError::ResumeSemanticsMismatch`
    // is checked on both backends' install paths, in release builds, and
    // refuses the artifact on disagreement). That is the last consumer half of
    // `docs/feature-designs/deopt-osr.md` §5.2 — the reason stays the
    // charging key, the semantics decide what the interpreter does with the bci.
    //
    // Reachable since round 11 wave 10 (lane irexc): `ir_lower`'s rethrow pads
    // (`Lowerer::push_rethrow_pad`, on the precise-frame route under
    // `CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=1`) are the IR-tier producer this
    // branch was waiting for — every call-exception exit inside a protected
    // range reaches here with a `PendingException` point and the callee's
    // exception still pending. (`emit_deopt_unless` still never stamps it: a
    // guard fires before its bytecode.) What was here before the branch was
    // `ordinary_stash_frame`, whose doc asserts the opposite premise — *"that one is routed to
    // `LAST_EXCEPTIONAL` before this is reached"* — which is true of
    // `x64_deopt_entry` and was NOT true of this entry. So the day the IR tier
    // grows a precise exceptional frame, that frame was destined to be replaced
    // by the identity-less re-run sentinel and counted in
    // [`deopt_stash_non_reexecute_refused`], a counter documented as "zero on
    // every producer in the tree; non-zero names a producer that broke the
    // stash's invariant" — blaming the new producer for this function's gap
    // while silently losing the precise handler locals. A trap whose only sink
    // discards it is not a slow path.
    if point.semantics.rethrow_exception {
        LAST_EXCEPTIONAL.with(|c| push_stash_with_cause(c, frame, cause, point_addr, false));
        return i64::MIN;
    }
    let frame = ordinary_stash_frame(point, frame);
    // Wave 60 (lane L2): the body, named while its frame is live. A stub only
    // bakes an id under the switch (`ir_deopt_entry_named`).
    let body = if compile_id != 0 && frame.bci != u32::MAX {
        trapping_body_of_compile_id(compile_id, point_addr)
    } else {
        None
    };
    if forced {
        LAST_DEOPT.with(|c| push_stash_forced_trap(c, frame, cause, point_addr, body));
    } else {
        LAST_DEOPT
            .with(|c| push_stash_with_mark(c, frame, cause, point_addr, None, false, 0, body));
    }
    // Interpreter round i6 wave 123, lane L2: the frame base beside the entry.
    note_pushed_frame_rbp(rbp);
    i64::MIN
}

/// real-frame-deopt x64 Step 2 — the 3-arg frame-deopt trampoline entry.
///
/// The x64 single-pass backend's frame-deopt stub spills all 16 GPRs into an
/// in-frame [`SavedRegisters`] region and calls here with a pointer to it, so —
/// unlike [`ir_deopt_entry`] (which passes a default-zero register file because
/// the IR lowerer keeps every live value in a frame slot) — a
/// `FrameValue::Register(r)` resolves against the **live** spilled GPR `r`.
///
/// Mirrors `ir_deopt_entry` otherwise: stashes the reconstructed frame in
/// `LAST_DEOPT` and returns the `i64::MIN` deopt sentinel. It deliberately does
/// **not** set the VM's out-of-band deopt-pending flag — the interpreter's
/// real-frame-deopt detection (`vm/src/runtime/interpreter.rs`) keys on
/// `result == i64::MIN && take_last_deopt().is_some()`, runs before the
/// `deopt_signaled` path, and clears the stash so it cannot leak to the next JIT
/// call. STASH ONLY — no resume yet (that is Step 4).
///
/// deopt-osr Step 9 follow-up (a): a 4th arg, `epoch_guard`, carries the
/// stable, retained [`DeoptEpochGuard`] baked alongside the box. The
/// before-deref short-circuit it fed existed only for the deleted
/// `CRATONVM_JIT_FREE_CODE=1` mode (see the body). Since round 14 wave 2
/// (R13RP6-1) its compile id names the trapping body, whose own bytecode is
/// stashed beside the frame ([`peek_last_deopt_trap_source`]). The epoch
/// fields are stamped only under `deopt_real_enabled()`.
///
/// # Safety
/// `point` and `regs` must be non-null and valid for the trapping frame, and
/// `rbp` its still-live base — guaranteed by the emitting stub, which calls this
/// after spilling and before the epilogue. `epoch_guard` is null or a valid,
/// retained [`DeoptEpochGuard`].
pub extern "C" fn x64_deopt_entry(
    point: *const DeoptimizationPoint,
    rbp: u64,
    regs: *const SavedRegisters,
    epoch_guard: *const DeoptEpochGuard,
) -> i64 {
    // No staleness short-circuit, even for a superseded guard. The trapping
    // frame is executing its artifact, and an executing artifact owns its deopt
    // boxes until it returns (the retirement queue reclaims a body only once no
    // thread can be inside it). So the box is valid, and a superseded
    // artifact's snapshot is still SELF-CONSISTENT with the machine state of the
    // code that trapped: the epochs version the SPECULATION, not the frame
    // layout. A short-circuit here stashed an identity-less `bci == u32::MAX`
    // sentinel that forced every post-supersession trap onto the imprecise
    // whole-method re-run, duplicating side effects
    // (jit-invokedynamic-groovy-regression fix). It survived only under a
    // `CRATONVM_JIT_FREE_CODE=1` mode that freed boxes under running frames, and
    // was deleted with that mode. `epoch_guard` stays in the stub ABI, and
    // since round 14 wave 2 names the trapping body (`trap_source_of_guard`,
    // below).
    if point.is_null() || regs.is_null() {
        // Stash the re-run sentinel, exactly as `ir_deopt_entry` does for a
        // null point. This used to return the bare `i64::MIN` with NOTHING
        // stashed, and the VM's real-frame-deopt detection
        // (`result == i64::MIN && take_last_deopt().is_some()`) then read the
        // sentinel as a legitimate `Long.MIN_VALUE` return — a silent wrong
        // result rather than a refusal. A null `regs` is refused rather than
        // defaulted to zeros (the IR entry's choice) because this stub's
        // points DO name registers, and a zeroed file would resume them as
        // plausible-looking `0`s.
        stash_rerun_sentinel();
        return i64::MIN;
    }
    // SAFETY: contract documented above.
    let point = unsafe { &*point };
    // SAFETY: contract documented above; `regs` was checked non-null.
    let regs = unsafe { &*regs };
    let frame = reconstruct_frame_from_machine_state(point, regs, rbp);
    let cause = Some(DeoptCause::of_point(point));
    let point_addr = point as *const DeoptimizationPoint as usize;
    // Keyed on `semantics.rethrow_exception`, not on `reason` — see the note on
    // the same branch in `ir_deopt_entry`.
    if point.semantics.rethrow_exception {
        // Exceptional frames get their own stash — see `LAST_EXCEPTIONAL`.
        if false {
            eprintln!(
                "[cratonvm-deopt] x64 exceptional frame at throw bci={} locals={:?}",
                point.bci, frame.locals,
            );
        }
        LAST_EXCEPTIONAL.with(|c| push_stash_with_cause(c, frame, cause, point_addr, false));
        return i64::MIN;
    }
    if false {
        // Resume-side trace: confirms the frame-deopt trampoline fired and at
        // which bci/reason (deopt-osr Step 8 OSR-exit shows reason=OsrExit), with
        // the RESOLVED locals/stack so a wrong reconstruction is visible.
        eprintln!(
            "[cratonvm-deopt] x64 frame-deopt entry reason={:?} at bci={} locals={:?} stack={:?}",
            point.reason, point.bci, frame.locals, frame.stack,
        );
    }
    let frame = ordinary_stash_frame(point, frame);
    // Round 14 wave 2 (lane deopt; R13RP6-1): name the body while it is
    // certainly live (its frame is executing this call). Not for the re-run
    // sentinel, which names no bytecode.
    let source = if frame.bci == u32::MAX {
        None
    } else {
        trap_source_of_guard(epoch_guard)
    };
    // Interpreter round i2 wave 60 (lane L2): and the body itself, under the
    // switch, through the same guard.
    let body = if frame.bci != u32::MAX && stash_names_trapping_body_enabled() {
        trapping_body_of_guard(epoch_guard)
    } else {
        None
    };
    LAST_DEOPT.with(|c| push_stash_with_mark(c, frame, cause, point_addr, source, false, 0, body));
    // Interpreter round i6 wave 123, lane L2: the frame base beside the entry
    // (`StashEntry::frame_rbp`), the key a debugger's pending write to this
    // activation is matched by when its post-call exit rebuilds it.
    note_pushed_frame_rbp(rbp);
    i64::MIN
}

/// The weak reference of the artifact whose stubs bake `guard`, through the
/// guard's compile id, only when the artifact bound to that id owns this very
/// guard -- [`trap_source_of_guard`]'s rule and lifetime argument. `None` for
/// a null guard, an unbound id, or a body bound without an `Arc`.
/// Interpreter round i2 wave 60, lane L2.
#[cold]
#[inline(never)]
fn trapping_body_of_guard(
    guard: *const DeoptEpochGuard,
) -> Option<std::sync::Weak<crate::CompiledMethod>> {
    if guard.is_null() {
        return None;
    }
    // SAFETY: a non-null guard is the retained (leaked) guard the emitting
    // stub baked; it is never freed.
    let guard_ref = unsafe { &*guard };
    let id = guard_ref
        .compile_id
        .load(std::sync::atomic::Ordering::Acquire);
    let cm_ptr = crate::lookup_compile_id(id)?;
    // SAFETY: as in `trap_source_of_guard`.
    let cm = unsafe { &*(cm_ptr as *const crate::CompiledMethod) };
    if !std::ptr::eq(cm.deopt_epoch_guard, guard) {
        return None;
    }
    cm.bound_weak()
}

/// The bytecode the artifact whose stubs bake `guard` was compiled from, and
/// its constant-pool generation: through the guard's compile id, and only when
/// the artifact bound to that id owns this very guard (a stale or reissued id,
/// an OSR body -- never bound -- or a body whose publish site stamped no
/// source answers `None`). Lock-free: one table load, one `Arc` clone.
///
/// Called only from [`x64_deopt_entry`], i.e. from inside the trapping body's
/// own frame: the artifact bound to the id is then that body, live until its
/// frame returns (the argument `release_compile_id`'s callers rest on: no
/// frame of a dropped artifact is live).
fn trap_source_of_guard(guard: *const DeoptEpochGuard) -> Option<TrapSource> {
    if guard.is_null() {
        return None;
    }
    // SAFETY: a non-null guard is the retained (leaked) guard the emitting
    // stub baked; it is never freed.
    let guard_ref = unsafe { &*guard };
    let id = guard_ref
        .compile_id
        .load(std::sync::atomic::Ordering::Acquire);
    let cm_ptr = crate::lookup_compile_id(id)?;
    // SAFETY: a bound compile id names a live published artifact; the one
    // owning this guard is executing the stub that called us (see above).
    // Cast: the address `bind_compile_id` recorded for the artifact.
    let cm = unsafe { &*(cm_ptr as *const crate::CompiledMethod) };
    if !std::ptr::eq(cm.deopt_epoch_guard, guard) {
        return None;
    }
    let source = cm.compiled_source()?;
    Some((std::sync::Arc::clone(source), cm.compile_cp_stamp()))
}

#[cfg(test)]
mod x64_deopt_entry_tests;

/// Count how many virtual objects need materialization across locals and
/// stack in a single frame (non-recursive).
pub fn count_virtual_objects(frame: &FrameState) -> usize {
    fn count_in(values: &[FrameValue]) -> usize {
        values
            .iter()
            .filter(|v| {
                matches!(
                    v,
                    FrameValue::VirtualObject(_) | FrameValue::ConditionalVirtualObject(_)
                )
            })
            .count()
    }

    count_in(&frame.locals) + count_in(&frame.stack)
}

// ---------------------------------------------------------------------------
// Resume semantics, and the bound every caller-chain walk shares
// ---------------------------------------------------------------------------
//
// RETIRED, 2026-09-21 (round 10, lane `interner`): an interned, structurally
// sharing frame-state representation used to live here —
// `FrameStateInterner`/`SharedFrameState`/`InternedDeoptPoint`/`InterningStats`,
// four handle newtypes, chunked value spines, a persistent-update family
// (`with_local`/`with_bci`/`with_caller`/…), a materialization layer, and a
// second verifier front door (`DeoptVerifier::violations_interned` /
// `::verify_interned`). It was landed as stage 1 of a staged change and stage 2
// never came: at deletion it had ZERO references anywhere outside its own
// `#[cfg(test)]` modules, in this file or any other.
//
// Why it went rather than getting its first caller — the argument is in
// `docs/feature-designs/deopt-osr.md` §7 and
// `docs/feature-designs/deopt-osr.md`,
// and the short form is:
//
//   * The saving it was built for is the PER-INLINE-LEVEL one. `FrameState::caller`
//     is a `Box<FrameState>`, so an inlined caller's slots are deep-copied into
//     every deopt point of the callee, and interning collapses that to one
//     handle. But no artifact this VM installs carries a caller chain at all:
//     on the single-pass side `x64/inlining.rs`'s splice postcondition rolls
//     back any splice that published a point (and `build_deopt_frame_inner`
//     refuses a non-empty chain outright, `DeoptFrameBail::InlinedChain`); on
//     the IR side `ir_lower::resolve_frame_state` sets `caller: None`
//     unconditionally (`ir::InlineScopeTable`, which was the staged populator
//     side, was retired in wave 9 for never having had one). So the multiplier
//     the interning exists
//     to divide is 1 today, and making it >1 is a different, larger piece of
//     work that will decide for itself what shape the metadata wants.
//   * The flat, per-method saving IS available today, but the numbers the design
//     doc quoted (88.0 % of slots shared, 87.2 % of slot bytes) were produced by
//     a test that CONSTRUCTED 100 snapshots differing in exactly one local per
//     bci and then derived them with `with_local`. Real producers do not derive:
//     `x64/deopt_stubs.rs::build_and_record_deopt_point` rebuilds the whole
//     frame from regalloc provenance at each site, and `ir_lower`'s
//     `build_deopt_points` rebuilds it from the IR. Content-hash dedup would
//     still have found some sharing — but nobody knows how much, because there
//     was no production interner, so `InterningStats` read zero forever. That is
//     round 10's own defect shape: an instrument whose number is indistinguishable
//     from "this never happened".
//   * The cost it targeted is still reported. `metrics.rs::frame_state_heap_bytes`
//     totals the owned representation per artifact, so the motivation for redoing
//     this work survives the deletion of the implementation.
//   * `violations_interned` was a second entry point into `DeoptVerifier`, which
//     since round 10 wave 6 is armed on BOTH backends' install paths. Its own
//     comment named the risk correctly — two checkers that were supposed to agree
//     and drifted — and the only thing keeping them in step was a test. One front
//     door is the honest state while there is one representation.
//
// What deliberately stayed: `ResumeSemantics` (a real `DeoptimizationPoint`
// field, stamped by every producer and checked by the verifier's
// exception-state-agreement lane), and `MAX_SCOPE_CHAIN` below.

/// Hard cap on how many scopes a caller chain may be walked for.
///
/// Owned chains are acyclic in every artifact this VM installs (nothing
/// publishes one at all — see the retirement note above), but every walk in
/// this module is bounded regardless: a metadata defect, or a chain built by a
/// producer that does not exist yet, must not turn into an unbounded loop
/// inside a deopt path. `DeoptVerifier::check_point` reports the overrun as
/// [`DeoptMetadataError::ScopeChainTooDeep`] rather than checking a prefix and
/// calling it clean, and `frame_state_is_resumable` answers `false` at the cap
/// rather than `true`.
///
/// Distinct from `ir::MAX_INLINE_SCOPE_DEPTH`, which bounds the IR tier's
/// inline-FRAME-site walks (stack traces, not deopt).
const MAX_SCOPE_CHAIN: usize = 256;

/// What the interpreter must do with the bytecode at a scope's `bci` — the
/// explicit form of what is otherwise a per-[`DeoptReason`] prose convention.
///
/// `docs/jit/deopt-metadata.md` records the convention as an outright gap:
///
/// > **Reexecute flag** — absent / absent. No field exists. Re-execute-vs-resume
/// > is encoded *implicitly* in `DeoptReason` […] A consumer that gets the
/// > convention wrong executes the instruction after a call that never returned.
///
/// The two flags are independent, exactly as in a real scope descriptor:
///
/// * `reexecute` — the bytecode at `bci` has **not** taken effect. The
///   interpreter must run it from the top. Every guard the backends emit is
///   like this: a bounds check fires *before* the `aaload` it protects, so the
///   snapshot's operand stack still holds the array and the index.
/// * `rethrow_exception` — the scope is not a resume point at all. A Java
///   exception is pending and `bci` names the *throwing* instruction, to be
///   routed through this method's own exception table
///   ([`DeoptReason::PendingException`]).
///
/// The distinction is load-bearing for inlining: the innermost (trapping) scope
/// of an inlined chain re-executes its bytecode, but every **caller** scope is
/// parked mid-`invoke` — its `bci` names a call that is already in progress, and
/// re-executing it would call the callee a second time.
/// [`Self::for_caller_scope`] is that answer.
///
/// Note what the owned representation still cannot say: [`FrameState`] has no
/// per-scope semantics field, so only the INNERMOST scope's semantics survive on
/// a [`DeoptimizationPoint`]. The retired interned form carried them per scope
/// and applied `for_caller_scope` automatically to every linked caller; whatever
/// eventually publishes caller chains has to solve that again, and it is the
/// half of `docs/feature-designs/deopt-osr.md` §5.2 that no deletion
/// closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResumeSemantics {
    /// Re-run the bytecode at `bci` instead of continuing after it.
    pub reexecute: bool,
    /// `bci` names a throwing instruction with a pending exception; route it
    /// through the exception table rather than resuming.
    pub rethrow_exception: bool,
    /// The compiled attempt that stopped here had NEVER passed `bci`: every
    /// arrival at `bci` in this body traps (round 12 wave 3, lane replay).
    ///
    /// Not a resume instruction: a precise resume ignores it. It is a fact for
    /// the whole-method REPLAY rule
    /// ([`replay_from_entry_commits_nothing_with_handlers`]): such an attempt can only have
    /// run what is reachable from the entry without passing `bci`, where an
    /// arbitrary (conditional) deopt inside a loop may have run the whole loop
    /// body, including code textually after `bci`, on earlier iterations.
    ///
    /// Set by exactly one producer: `ir_lower`'s `Op::Guard` for an IR SITE
    /// TRAP (`IrBuilder::plant_uncommon_trap`, the builder's list
    /// `Graph::site_trap_pcs`), whose whole translation of the bytecode at
    /// `bci` is a guard on the constant `0`. A guard whose condition merely
    /// FOLDED to `0` does not qualify: an unrolled or peeled copy of the same
    /// bytecode may have passed it. `false` everywhere else, which is the
    /// conservative answer. A wrong `true` is a silent double commit, so no
    /// other producer may set it.
    ///
    /// Compare resume kinds with [`Self::resume_kind`], which strips it: two
    /// points that differ only here resume identically.
    pub first_arrival: bool,
    /// JIT round 18 wave 9 (lane syncdoor6, SD17-3): `monitors[0]` of this
    /// point's frame is the METHOD monitor a self-locking body kept at this
    /// exit, and every further monitor is a `synchronized` block's hold.
    ///
    /// Not a resume instruction either: it tells the VM which monitor the
    /// resumed frame owns as its `monitor_on_exit`
    /// ([`handed_method_monitor_index`]), where the shape alone is ambiguous
    /// once the method has monitor bytecode (a wrapped body inside
    /// `synchronized (this)` names `[this]` too). Set by exactly one
    /// producer: `x64/deopt_stubs.rs` `build_deopt_point`, for a self-locking
    /// body with blocks (`CRATONVM_JIT_SELF_LOCKING_SYNC_BLOCKS`); `false`
    /// everywhere else. A wrong `true` would make a block hold the frame's
    /// method monitor, so no other producer may set it.
    ///
    /// [`Self::resume_kind`] strips it, like [`Self::first_arrival`].
    pub method_monitor_first: bool,
    /// JIT round 19 wave 3 (lane splice3, SPLICE2-2 = TF15-1): this point is
    /// a spliced callee's implicit-exception guard that a previous body kept
    /// failing on an EXPECTED exception path (the de-spec registry names it,
    /// [`expected_throw_speculation_id`]). Not a resume instruction: the
    /// point resumes exactly as without it (the interpreter re-executes the
    /// bytecode and throws). It tells the VM's sinks not to charge the trip
    /// (no eviction, no per-bci de-spec), so the body and its splice stay.
    /// Set by exactly one producer, `ir_lower::emit_deopt_unless`, for a chain
    /// point under `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW`; `false` everywhere
    /// else. A wrong `true` costs performance only (an uncharged trap repeats).
    ///
    /// [`Self::resume_kind`] strips it.
    pub expected_throw: bool,
}

impl ResumeSemantics {
    /// Continue *after* the bytecode at `bci`; it has already taken effect.
    pub const RESUME: Self = Self {
        reexecute: false,
        rethrow_exception: false,
        first_arrival: false,
        method_monitor_first: false,
        expected_throw: false,
    };

    /// Re-run the bytecode at `bci` from the top — the guard fired before it.
    pub const REEXECUTE: Self = Self {
        reexecute: true,
        rethrow_exception: false,
        first_arrival: false,
        method_monitor_first: false,
        expected_throw: false,
    };

    /// Not a resume point: route the pending exception through the method's
    /// exception table, starting at the throwing `bci`.
    pub const RETHROW: Self = Self {
        reexecute: false,
        rethrow_exception: true,
        first_arrival: false,
        method_monitor_first: false,
        expected_throw: false,
    };

    /// These semantics with [`Self::first_arrival`] set to `first_arrival`.
    /// Only `ir_lower`'s site-trap guard passes `true`; see the field.
    pub const fn with_first_arrival(self, first_arrival: bool) -> Self {
        Self {
            first_arrival,
            ..self
        }
    }

    /// What the interpreter does with the bytecode at `bci`, without the
    /// replay fact [`Self::first_arrival`]. Every comparison of two points'
    /// (or a point's and a constant's) RESUME behaviour goes through this.
    pub const fn resume_kind(self) -> Self {
        self.with_first_arrival(false)
            .with_method_monitor_first(false)
            .with_expected_throw(false)
    }

    /// These semantics with [`Self::expected_throw`] set to `expected`. Only
    /// `ir_lower::emit_deopt_unless` passes `true`; see the field.
    pub const fn with_expected_throw(self, expected: bool) -> Self {
        Self {
            expected_throw: expected,
            ..self
        }
    }

    /// These semantics with [`Self::method_monitor_first`] set to `first`.
    /// Only `x64/deopt_stubs.rs` `build_deopt_point` passes `true`; see the
    /// field.
    pub const fn with_method_monitor_first(self, first: bool) -> Self {
        Self {
            method_monitor_first: first,
            ..self
        }
    }

    /// The semantics a [`DeoptReason`] implies today, written down once.
    ///
    /// Every reason the two backends emit stamps a guard that fires *before*
    /// the bytecode it protects — a null check before the field access, a
    /// bounds check before the array access, a div-by-zero test before the
    /// `idiv` (`ir_lower.rs`: "deopt to the interpreter at this bci, which
    /// re-executes the `idiv`/`irem` and throws"), an uncommon trap on a
    /// branch that was never taken, an OSR-exit at a loop bci that has not run
    /// — so they all re-execute. The single exception is
    /// [`DeoptReason::PendingException`], whose own documentation states it "is
    /// **not** a resume point".
    ///
    /// This is a *derivation* from the current convention, not a licence to
    /// keep it: a producer that knows better (a caller scope, a resume point
    /// after a returning call) must pass explicit semantics instead.
    pub fn for_reason(reason: DeoptReason) -> Self {
        match reason {
            DeoptReason::PendingException => Self::RETHROW,
            _ => Self::REEXECUTE,
        }
    }

    /// The semantics of an inlined **caller** scope: the `invoke` at its `bci`
    /// is in progress, so the interpreter must neither re-execute it (that
    /// would call the callee twice) nor treat it as throwing.
    pub fn for_caller_scope() -> Self {
        Self::RESUME
    }
}

impl Default for ResumeSemantics {
    /// [`Self::REEXECUTE`] — the safe default for a guard-shaped deopt point,
    /// and what every point the backends emit today needs.
    fn default() -> Self {
        Self::REEXECUTE
    }
}

impl fmt::Display for ResumeSemantics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.reexecute, self.rethrow_exception) {
            (_, true) => f.write_str("rethrow")?,
            (true, false) => f.write_str("reexecute")?,
            (false, false) => f.write_str("resume")?,
        }
        if self.first_arrival {
            f.write_str(" (first arrival)")?;
        }
        if self.method_monitor_first {
            f.write_str(" (method monitor first)")?;
        }
        if self.expected_throw {
            f.write_str(" (expected throw)")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Whole-method replay: when re-running from entry is the same execution
// ---------------------------------------------------------------------------

/// Kill switch for the loop-closed replay extent. **Default ON**;
/// `CRATONVM_JIT_IR_TRAP_REPLAY_LOOP_EXTENT=0` restores the textual-prefix rule
/// at both ends (this is the switch the IR builder's half,
/// `ir::commits_before_first_arrival` in `IrBuilder::trap_replay_is_safe`,
/// already reads; `r12w2-irbuild-deopt-resume-replay-extent-patch`).
fn replay_loop_extent_enabled() -> bool {
    true
}

/// Kill switch for reading [`ResumeSemantics::first_arrival`] in
/// [`replay_from_entry_commits_nothing_with_handlers`]. **Default ON**;
/// `CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL=0` treats every point as a
/// conditional deopt (the loop-closed extent alone), which is stricter.
fn replay_first_arrival_enabled() -> bool {
    true
}

/// JIT r22 wave 8, lane inline (`CRATONVM_JIT_TRAP_REPLAY_OWN_SITE_IGNORES_SPLICES`,
/// correctness / HotSpot parity, **default ON**, `=0` restores the blanket veto;
/// page `docs/feature-designs/jit-ir-optimizations.md`):
/// may the replay question of a FIRST-ARRIVAL point at `resume_bci` ignore the
/// artifact's `spliced_bodies_side_effect_free`?
///
/// A spliced callee body runs only where the caller's own bytecode has an
/// `invoke` (`invokevirtual` / `invokespecial` / `invokestatic` /
/// `invokeinterface`, the only splice sites). A first-arrival point's attempt
/// ran exactly what is reachable from bci 0 without passing `resume_bci`
/// (`ir::commits_before_first_arrival`), and that walk counts every invoke as a
/// committed side effect, so a replay it admits ran no invoke and therefore no
/// spliced body. The one way a spliced body can be in progress at the point is
/// a deopt INSIDE the splice, which resumes at the outermost invoke: so the
/// purity is ignored only when the instruction at `resume_bci` is not one of
/// those four. The blanket veto it replaces refused every site trap of a method
/// with any impure splice ANYWHERE: `sortingAndDistinct`'s entry
/// `invokedynamic` (after `aload_1; iload_3` only) was declined, the method
/// never got an optimizing body, and HotSpot's behaviour (a compiled body with
/// an uncommon trap there) was out of reach.
///
/// Why `invokedynamic` (`0xba`) is NOT excluded although an own indy site
/// planned as a plain call of its constant target (`IrBuilder`'s
/// `indy_direct_sites`) can be a splice site: [`ResumeSemantics::first_arrival`]
/// has exactly one producer, an IR SITE TRAP (`plant_uncommon_trap`), whose
/// whole translation of the bytecode at its bci is the trap. A pc that carries
/// a trap carries no splice (the builder asks the same question with
/// `!inline_sites.contains_key(&pc) && !indy_direct_sites.contains_key(&pc)`),
/// a trap inside an OPEN splice raises the splice fence (the graph is refused
/// or the point resumes through its caller chain), and a deopt inside a splice
/// that resumes at its outermost invoke is a guard, never `first_arrival`. So
/// a `first_arrival` point at an indy bci is the unspliced site's own trap
/// (the r22 w7 entry-trap case), and the invoke-opcode test below is defence
/// in depth for the four opcodes a splice is planned at by name. An inlined
/// chain never reaches this question with its callee bci: the sinks ask the
/// whole-body form (`u32::MAX`) for a frame with caller frames
/// (`deopt_resume::replay_from_entry_is_observably_equivalent_for_stash`), and
/// the compile-time check passes `first_arrival && !chain`.
///
/// Only with the first-arrival walk itself on (`CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL`,
/// `CRATONVM_JIT_IR_TRAP_REPLAY_LOOP_EXTENT`), since that walk is the proof.
/// Asked by both ends: here and by `ir::IrBuilder::trap_replay_is_safe`.
pub fn replay_own_site_ignores_splice_purity(code: &[u8], code_len: usize, resume_bci: u32) -> bool {
    let Ok(bci) = usize::try_from(resume_bci) else {
        return false;
    };
    let len = code_len.min(code.len());
    bci < len
        && !matches!(code[bci], 0xb6..=0xb9)
        && replay_first_arrival_enabled()
        && replay_loop_extent_enabled()
        && true
}

/// [`replay_from_entry_commits_nothing_with_handlers`] with an empty
/// exception table: the question for a frame that takes no exception edge
/// inside itself. Test-only since round 13 wave 12 (lane replay6): every
/// production caller now passes the edges it knows of (the single-pass
/// finalizer its whole table, the VM sinks their body's local-handler sites,
/// an empty slice where there are none).
#[cfg(test)]
pub fn replay_from_entry_commits_nothing(
    code: &[u8],
    code_len: usize,
    spliced_bodies_pure: bool,
    resume_bci: u32,
    first_arrival: bool,
) -> bool {
    replay_from_entry_commits_nothing_with_handlers(
        code,
        code_len,
        spliced_bodies_pure,
        resume_bci,
        first_arrival,
        &[],
    )
}

/// Kill switch for following exception edges in the replay extent
/// ([`replay_from_entry_commits_nothing_with_handlers`]). **Default ON**;
/// `CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES=0` ignores the table, which is the
/// pre-round-13-wave-10 rule (unsound for a handler placed below its range's
/// end). Read only for a table that has such a row, which javac emits only as
/// `synchronized`'s self-covering `monitorexit` row.
fn replay_extent_handler_edges_enabled() -> bool {
    true
}

/// May a compiled attempt that deoptimized at `resume_bci` be replaced by a
/// re-run of the method from bci 0? `true` means the attempt cannot have
/// committed anything a re-run would commit a second time (round 12 wave 3,
/// lane replay).
///
/// The ONE rule, asked at both ends: by the VM's method-entry deopt sinks when
/// a precise resume is unavailable
/// (`deopt_resume::replay_from_entry_is_observably_equivalent*`), and by the
/// compiler for every point it knows a sink will NOT resume precisely
/// ([`first_point_needing_unsound_replay_of_method`],
/// [`single_pass_first_trap_needing_unsound_replay_with_handlers`]), so a body
/// whose only fallback would double-commit is never installed.
///
/// `code[..code_len]` is the method's own bytecode (trailing padding is `nop`s
/// and harmless). `spliced_bodies_pure` is the artifact's
/// `spliced_bodies_side_effect_free`: a relocated callee body the caller's
/// bytecode does not describe must commit nothing either.
///
/// # What the attempt can have run
///
/// Every point that reaches here re-executes the bytecode at `resume_bci`, so
/// that instruction had not completed. What else ran:
///
/// * a CONDITIONAL deopt (a bounds, null or divisor guard, a pruned branch)
///   may fire on any arrival, so inside a loop the earlier iterations ran the
///   whole body, including code textually AFTER `resume_bci`. The range is
///   `code[..ir::replay_committed_extent_with_handlers(..)]`, the prefix
///   closed over every loop containing `resume_bci`;
/// * a FIRST-ARRIVAL point ([`ResumeSemantics::first_arrival`], an IR site
///   trap) fires the first time control reaches `resume_bci`, so the attempt
///   ran only what is reachable from the entry without passing it
///   (`ir::commits_before_first_arrival`). Walked inside the loop-closed
///   extent, since nothing past it can reach `resume_bci` at all. This is what
///   lets `for (..) { <indy>; counter++; }` replay: the store is only
///   reachable through the trap.
///
/// # Exception edges (round 13 wave 10, lane exc4)
///
/// `handlers` is exception-table rows `(start_pc, end_pc, handler_pc)` in the
/// coordinates of `code`, the exception edges the frame may take INSIDE
/// itself (`r13w9-replay5-replay-extent-ignores-exception-edges`). A row whose
/// handler lies below its range's end is a way back to `resume_bci` the
/// branch-only extent does not see: a compiled frame with local handlers can
/// run the protected pcs up to `end`, throw, run the handler and arrive at
/// `resume_bci` again (`ir::replay_committed_extent_with_handlers`). An
/// optimizing-tier frame passes an empty slice here; the single-pass finalizer
/// passes the method's whole table; a VM sink holding the body that ran passes
/// that body's [`local_handler_replay_edges`] (round 13 wave 12, lane replay6).
/// The first-arrival walk here stays handler-blind, but optimizing bodies DO
/// enter their own handlers in compiled code (local throw edges, catch arms):
/// since JIT r22 wave 10 the trap PLANTER is the stricter end
/// (`ir::commits_before_first_arrival_with_handlers`,
/// `CRATONVM_JIT_IR_TRAP_REPLAY_FOLLOWS_HANDLERS`), and this sink is sound
/// only because a trap the planter refused is never planted.
///
/// # Conservative answers
///
/// A body that commits nothing anywhere answers `true` first (the historical
/// rule, and the only `true` for the identity-less `u32::MAX` re-run
/// sentinel). An impure spliced body, an out-of-range `resume_bci`, and a walk
/// that loses instruction sync answer `false`.
///
/// `CRATONVM_JIT_IR_TRAP_REPLAY_LOOP_EXTENT=0` answers with the textual prefix
/// `code[..resume_bci]` (the pre-round-12 rule, unsound inside a loop);
/// `CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL=0` ignores the first-arrival fact;
/// `CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES=0` ignores `handlers`.
pub fn replay_from_entry_commits_nothing_with_handlers(
    code: &[u8],
    code_len: usize,
    spliced_bodies_pure: bool,
    resume_bci: u32,
    first_arrival: bool,
    handlers: &[(usize, usize, usize)],
) -> bool {
    let len = code_len.min(code.len());
    if !crate::bytecode_commits_side_effect(code, len) {
        return true;
    }
    // JIT r22 wave 8 (lane inline): an impure spliced body vetoes the replay
    // only when one can have RUN, which a first-arrival point at an own
    // non-invoke bci rules out (`replay_own_site_ignores_splice_purity`).
    if !spliced_bodies_pure
        && !(first_arrival && replay_own_site_ignores_splice_purity(code, len, resume_bci))
    {
        return false;
    }
    let Ok(bci) = usize::try_from(resume_bci) else {
        return false;
    };
    if bci > len {
        return false;
    }
    if !replay_loop_extent_enabled() {
        return !crate::bytecode_commits_side_effect(code, bci);
    }
    let follow_handlers = handlers.iter().any(|&(_, end, handler)| handler < end)
        && replay_extent_handler_edges_enabled();
    let extent = if follow_handlers {
        crate::ir::replay_committed_extent_with_handlers(code, len, bci, handlers)
    } else {
        crate::ir::replay_committed_extent(code, len, bci)
    };
    if first_arrival && replay_first_arrival_enabled() {
        // `extent` bounds the walk: an instruction at or past it cannot reach
        // `bci`, so it cannot have run before the first arrival either.
        return !crate::ir::commits_before_first_arrival(code, extent, bci);
    }
    !crate::bytecode_commits_side_effect(code, extent)
}

/// The largest method [`rerun_window_closed_at`] analyses: it asks the replay
/// rule once per instruction, so its cost is quadratic in the code length.
const RERUN_WINDOW_MAX_CODE: usize = 4096;

/// gkr (OOM21-2, the callee half of `crate::service_copy_window_enabled`): for
/// every instruction start `p` of `code`, is NO point at which a re-run of the
/// method from entry would be exact reachable from `p` (`p` itself included)?
///
/// A trap the callee-deopt service cannot resume precisely is re-run from
/// entry only when the replay rule
/// ([`replay_from_entry_commits_nothing_with_handlers`]) calls that exact at
/// the trap's bci; otherwise the service raises `InternalError`
/// (`unsound_rerun_refused`) before it reads an argument. The rule is asked
/// here in its MOST permissive form for every instruction a trap could name
/// -- spliced bodies taken as pure, no exception edges in the extent, and the
/// first-arrival fact at each of `first_arrival_bcis` (the body's IR site
/// traps, the one producer of that fact) -- so a `true` here holds for
/// whatever the sink later asks. Control flow follows every normal successor
/// and every handler whose range covers a reachable instruction.
///
/// `None` (no claim anywhere) for an empty method, one over
/// [`RERUN_WINDOW_MAX_CODE`] bytes, and one with `jsr` / `ret` (whose
/// successors the lenient walk does not know). Pure.
pub fn rerun_window_closed_at(
    code: &[u8],
    code_len: usize,
    handlers: &[(usize, usize, usize)],
    first_arrival_bcis: &[u32],
) -> Option<Vec<bool>> {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    if len == 0 || len > RERUN_WINDOW_MAX_CODE {
        return None;
    }
    let starts = ba::instruction_starts(code, len);
    let insns: Vec<usize> = (0..len).filter(|&p| starts[p]).collect();
    if insns.iter().any(|&p| {
        matches!(code[p], 0xa8 | 0xa9 | 0xc9) || ba::is_wide_ret(&code[..len], p)
    }) {
        return None;
    }
    // Reverse edges: `preds[s]` lists every instruction that can step to `s`.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); len];
    for &p in &insns {
        ba::for_each_lenient_successor(code, len, p, |s| {
            if s < len && starts[s] {
                preds[s].push(p);
            }
        });
        for &(start, end, handler) in handlers {
            if start <= p && p < end && handler < len && starts[handler] {
                preds[handler].push(p);
            }
        }
    }
    if !crate::bytecode_commits_side_effect(code, len) {
        return Some(vec![false; len]);
    }
    let first_commit_pc = insns
        .iter()
        .copied()
        .find(|&p| crate::opcode_commits_side_effect(code[p]))
        .unwrap_or(len);
    let loop_extent = replay_loop_extent_enabled();
    let back_edges = if loop_extent {
        crate::ir::collect_replay_backward_edges(code, len, &[]).ok()
    } else {
        None
    };

    // Every instruction from which an exact point is reachable.
    let mut reaches_exact = vec![false; len];
    let mut work: Vec<usize> = Vec::new();
    // 1. Plain replay exactness: monotone in `t` (`extent(t)` grows with `t`),
    //    so stop as soon as `extent(t) > first_commit_pc`.
    for &t in &insns {
        if t > first_commit_pc {
            break;
        }
        let ok = if !loop_extent {
            true
        } else if let Some(ref back) = back_edges {
            crate::ir::replay_committed_extent_from_back_edges(len, t, back) <= first_commit_pc
        } else {
            false
        };
        if !ok {
            break;
        }
        reaches_exact[t] = true;
        work.push(t);
    }
    // 2. First-arrival IR site traps (usually empty or a handful of sites).
    for &bci in first_arrival_bcis {
        let t = bci as usize;
        if t < len
            && starts[t]
            && !reaches_exact[t]
            && replay_from_entry_commits_nothing_with_handlers(code, len, true, bci, true, &[])
        {
            reaches_exact[t] = true;
            work.push(t);
        }
    }
    let propagate = |reaches_exact: &mut [bool], work: &mut Vec<usize>| {
        while let Some(s) = work.pop() {
            for &p in &preds[s] {
                if !reaches_exact[p] {
                    reaches_exact[p] = true;
                    work.push(p);
                }
            }
        }
    };
    propagate(&mut reaches_exact, &mut work);
    // 3. Owned-arguments hand-off at invokes not already reaching an exact point.
    for &t in insns.iter().rev() {
        if !reaches_exact[t]
            && matches!(code[t], 0xb6..=0xb9)
            && owned_args_caller_replay_may_be_exact(code, len, t as u32, &[])
        {
            reaches_exact[t] = true;
            work.push(t);
            propagate(&mut reaches_exact, &mut work);
        }
    }
    Some((0..len).map(|p| starts[p] && !reaches_exact[p]).collect())
}

#[cfg(test)]
mod gkr_rerun_window_tests;



/// The owned-arguments hand-off's caller rule, both tiers (single-pass
/// `x64/op_invoke.rs` `caller_replay_to_call_is_exact`, optimizing
/// `ir_lower.rs` `ir_direct_call_args_owned_by_callee`): may a re-run of this
/// method from bci 0 replace an attempt that stopped at the call at `bci`?
///
/// [`replay_from_entry_commits_nothing_with_handlers`] (conditional semantics,
/// spliced bodies pure, the method's own table), or -- ARGWIN21-4 -- a
/// straight-line prefix whose only stores are array stores INTO ARRAYS THE
/// PREFIX ITSELF ALLOCATED ([`prefix_stores_only_into_its_own_arrays`]): the
/// varargs array and the array initializer filled right before the call. A
/// re-run allocates fresh arrays and fills them again; the abandoned ones are
/// reachable from nothing the re-run or anyone else reads (they could escape
/// only through an invoke or a store into an older object, which still
/// refuse), so nothing observable is committed twice.
pub fn owned_args_caller_replay_is_exact(
    code: &[u8],
    code_len: usize,
    bci: u32,
    handlers: &[(usize, usize, usize)],
) -> bool {
    owned_args_caller_replay_is_exact_with_statics(code, code_len, bci, handlers, &no_plain_statics)
}

/// `CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES=1` -- opt-in (j27 wave 5, lane
/// OOM21; page
/// `jr21-oom21-2-a-caller-whose-prefix-commits-keeps-its-outgoing-arguments-FIXED-20261101.md`).
/// See [`owned_args_caller_replay_is_exact_with_statics`]. Acts only while
/// `CRATONVM_JIT_OWNED_ARGS_FRESH_STORES` is on (it extends that walk). Until
/// j27 wave 9 it also stayed off while dev's service-copy window
/// (`CRATONVM_JIT_SERVICE_COPY_WINDOW`, gkr) was on; that window now counts
/// every invoke [`owned_args_caller_replay_may_be_exact`] admits as a re-run
/// point ([`rerun_window_closed_at`]), so it keeps the copies a hand-off at a
/// constant-store prefix re-reads, and the gate is gone
/// (`j27-service-copy-window-ignores-owned-args-rerun-points-FIXED-20261029.md`).
/// Default ON (j27 wave 9): a replay soundness rule verified on host. Read per admission,
/// no static.
pub fn owned_args_const_static_stores_enabled() -> bool {
    true
}

/// The `plain_static` answer of a tier that resolves no field: every
/// `putstatic` refuses.
fn no_plain_statics(_pc: usize) -> bool {
    false
}

/// [`owned_args_caller_replay_is_exact`], plus -- j27 OOM21, under
/// [`owned_args_const_static_stores_enabled`] -- a `putstatic` of a CONSTANT
/// (`aconst_null`, `iconst_*`, `lconst_*`, `fconst_*`, `dconst_*`, `bipush`,
/// `sipush`, the instruction right before it) into a static that
/// `plain_static(pc)` vouches is resolved and NOT volatile. A re-run stores
/// the same value into the same cell again. On this thread nothing between
/// the two writes is lost: the callee's abandoned attempt is replayed from
/// entry by the re-run too (the residue every hand-off already has), so it
/// sees the constant again before it reads. Another thread can tell only by
/// racing a plain write of its own against ours, and the Java memory model
/// lets that race resolve either way, so the outcome is one a single write
/// can also give. A volatile static refuses: synchronization order would
/// expose the second write. Resolution and `<clinit>` happened on the first
/// arrival, so the re-run's write cannot throw or initialise anything.
///
/// `plain_static` is asked only at a `putstatic` pc inside the prefix.
pub fn owned_args_caller_replay_is_exact_with_statics(
    code: &[u8],
    code_len: usize,
    bci: u32,
    handlers: &[(usize, usize, usize)],
    plain_static: &dyn Fn(usize) -> bool,
) -> bool {
    let statics: &dyn Fn(usize) -> bool = if owned_args_const_static_stores_enabled() {
        plain_static
    } else {
        &no_plain_statics
    };
    replay_from_entry_commits_nothing_with_handlers(code, code_len, true, bci, false, handlers)
        || (usize::try_from(bci).is_ok_and(|bci| {
                prefix_commits_only_rerunnable_stores(code, code_len, bci, handlers, statics)
            }))
}

/// The superset of every tier's [`owned_args_caller_replay_is_exact_with_statics`]
/// answer at `bci` (each tier's `plain_static` answers `true` at most where
/// this one does): may an owned-arguments hand-off at the call at `bci` make
/// the frame re-run from entry? For a rule that must keep what such a re-run
/// reads -- the service-copy window's "exact re-run point" (gkr,
/// `deopt::rerun_window_closed_at` on dev) must count every bci this answers
/// `true` for, or it clears the caller's copies of a frame the hand-off then
/// re-runs with them. Pure apart from the switches.
pub fn owned_args_caller_replay_may_be_exact(
    code: &[u8],
    code_len: usize,
    bci: u32,
    handlers: &[(usize, usize, usize)],
) -> bool {
    owned_args_caller_replay_is_exact_with_statics(code, code_len, bci, handlers, &|_| true)
}

/// The operand words an instruction [`owned_args_caller_replay_is_exact_with_statics`]
/// accepts as a `putstatic`'s constant pushes, `None` for every other opcode.
fn constant_push_words(op: u8) -> Option<usize> {
    match op {
        // aconst_null, iconst_*, fconst_*, bipush, sipush, ldc, ldc_w
        0x01..=0x08 | 0x0b..=0x0d | 0x10..=0x13 => Some(1),
        // lconst_*, dconst_*, ldc2_w
        0x09 | 0x0a | 0x0e | 0x0f | 0x14 => Some(2),
        _ => None,
    }
}

/// One operand-stack or local word of [`prefix_stores_only_into_its_own_arrays`]'s
/// walk: an array allocated by the walked prefix, or anything else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PrefixWord {
    FreshArray,
    Other,
}

/// ARGWIN21-4: is `code[..bci]` straight-line code -- no branch, switch,
/// return, throw, invoke, monitor, field or static access -- whose only
/// committing instructions are `*astore`s into an array a `newarray` /
/// `anewarray` / `multianewarray` of the same prefix produced? `false` (refuse)
/// for anything the walk does not model: an unlisted opcode, a stack
/// underflow, a lost instruction sync, a backward edge that brings later code
/// back before `bci` (the loop-closed extent is not `bci`), or an exception
/// row whose range starts before `bci` (a handler the attempt can have run).
/// Category-2 values are two `Other` words, as the JVM's `dup` forms count
/// them. Pure. Every `putstatic` refuses here; see
/// [`prefix_commits_only_rerunnable_stores`].
pub fn prefix_stores_only_into_its_own_arrays(
    code: &[u8],
    code_len: usize,
    bci: usize,
    handlers: &[(usize, usize, usize)],
) -> bool {
    prefix_commits_only_rerunnable_stores(code, code_len, bci, handlers, &no_plain_statics)
}

/// [`prefix_stores_only_into_its_own_arrays`]'s walk, which also accepts --
/// j27 OOM21 -- a `putstatic` (0xb3) at a pc `plain_static` vouches for, whose
/// value the instruction right before it pushed as a constant
/// ([`constant_push_words`]; the verifier makes that push's category the
/// field's) or an unmodified incoming parameter. Pure given `plain_static`.
fn prefix_commits_only_rerunnable_stores(
    code: &[u8],
    code_len: usize,
    bci: usize,
    handlers: &[(usize, usize, usize)],
    plain_static: &dyn Fn(usize) -> bool,
) -> bool {
    use PrefixWord::{FreshArray, Other};
    // The words the previous instruction pushed as a constant or unmodified parameter, if it did.
    let mut prev_rerunnable: Option<usize> = None;
    let mut modified_locals = std::collections::HashSet::new();
    let len = code_len.min(code.len());
    if bci == 0 || bci > len || handlers.iter().any(|&(start, _, _)| start < bci) {
        return false;
    }
    if crate::ir::replay_committed_extent(code, len, bci) != bci {
        return false;
    }
    let mut stack: Vec<PrefixWord> = Vec::new();
    let mut locals: std::collections::HashMap<usize, PrefixWord> = std::collections::HashMap::new();
    let mut pc = 0usize;
    // Pop `n` words; `None` on underflow.
    fn pop_n(stack: &mut Vec<PrefixWord>, n: usize) -> Option<Vec<PrefixWord>> {
        let at = stack.len().checked_sub(n)?;
        Some(stack.split_off(at))
    }
    while pc < bci {
        let op = code[pc];
        let step = crate::bytecode_analysis::step(code, pc);
        if step == 0 || pc + step > bci {
            return false;
        }
        let u8_at = |k: usize| usize::from(code[pc + k]);
        let ok = match op {
            0x00 => true,
            // aconst_null, iconst_*, fconst_*, bipush, sipush, ldc, ldc_w
            0x01..=0x08 | 0x0b..=0x0d | 0x10..=0x13 => {
                stack.push(Other);
                true
            }
            // lconst_*, dconst_*, ldc2_w
            0x09 | 0x0a | 0x0e | 0x0f | 0x14 => {
                stack.extend([Other, Other]);
                true
            }
            // iload, fload; iload_n, fload_n
            0x15 | 0x17 | 0x1a..=0x1d | 0x22..=0x25 => {
                stack.push(Other);
                true
            }
            // lload, dload; lload_n, dload_n
            0x16 | 0x18 | 0x1e..=0x21 | 0x26..=0x29 => {
                stack.extend([Other, Other]);
                true
            }
            // aload, aload_n
            0x19 | 0x2a..=0x2d => {
                let idx = if op == 0x19 { u8_at(1) } else { usize::from(op - 0x2a) };
                stack.push(locals.get(&idx).copied().unwrap_or(Other));
                true
            }
            // iaload, faload, aaload, baload, caload, saload
            0x2e | 0x30 | 0x32..=0x35 => pop_n(&mut stack, 2).map(|_| stack.push(Other)).is_some(),
            // laload, daload
            0x2f | 0x31 => pop_n(&mut stack, 2)
                .map(|_| stack.extend([Other, Other]))
                .is_some(),
            // istore, fstore, astore; *_n forms
            0x36 | 0x38 | 0x3a | 0x3b..=0x3e | 0x43..=0x46 | 0x4b..=0x4e => {
                let idx = match op {
                    0x36 | 0x38 | 0x3a => u8_at(1),
                    0x3b..=0x3e => usize::from(op - 0x3b),
                    0x43..=0x46 => usize::from(op - 0x43),
                    _ => usize::from(op - 0x4b),
                };
                modified_locals.insert(idx);
                match stack.pop() {
                    Some(w) => {
                        locals.insert(idx, if matches!(op, 0x3a | 0x4b..=0x4e) { w } else { Other });
                        true
                    }
                    None => false,
                }
            }
            // lstore, dstore; *_n forms
            0x37 | 0x39 | 0x3f..=0x42 | 0x47..=0x4a => {
                let idx = match op {
                    0x37 | 0x39 => u8_at(1),
                    0x3f..=0x42 => usize::from(op - 0x3f),
                    _ => usize::from(op - 0x47),
                };
                modified_locals.insert(idx);
                modified_locals.insert(idx + 1);
                locals.insert(idx, Other);
                locals.insert(idx + 1, Other);
                pop_n(&mut stack, 2).is_some()
            }
            // iastore, fastore, aastore, bastore, castore, sastore (3 words);
            // lastore, dastore (4 words): the array is the deepest word.
            0x4f | 0x51 | 0x53..=0x56 | 0x50 | 0x52 => {
                let n = if matches!(op, 0x50 | 0x52) { 4 } else { 3 };
                match pop_n(&mut stack, n) {
                    Some(ws) => ws[0] == FreshArray,
                    _ => false,
                }
            }
            0x57 => pop_n(&mut stack, 1).is_some(),
            0x58 => pop_n(&mut stack, 2).is_some(),
            // dup, dup_x1, dup_x2, dup2, dup2_x1, dup2_x2, swap
            0x59..=0x5f => {
                let (take, form): (usize, &[usize]) = match op {
                    0x59 => (1, &[0, 0]),
                    0x5a => (2, &[1, 0, 1]),
                    0x5b => (3, &[2, 0, 1, 2]),
                    0x5c => (2, &[0, 1, 0, 1]),
                    0x5d => (3, &[1, 2, 0, 1, 2]),
                    0x5e => (4, &[2, 3, 0, 1, 2, 3]),
                    _ => (2, &[1, 0]),
                };
                match pop_n(&mut stack, take) {
                    Some(ws) => {
                        stack.extend(form.iter().map(|&i| ws[i]));
                        true
                    }
                    None => false,
                }
            }
            // iadd, isub, imul, idiv, irem, ishl, ishr, iushr, iand, ior, ixor
            0x60 | 0x64 | 0x68 | 0x6c | 0x70 | 0x78 | 0x7a | 0x7c | 0x7e | 0x80 | 0x82 => {
                pop_n(&mut stack, 2).map(|_| stack.push(Other)).is_some()
            }
            // ineg, i2b, i2c, i2s
            0x74 | 0x91..=0x93 => pop_n(&mut stack, 1).map(|_| stack.push(Other)).is_some(),
            0x84 => {
                let idx = u8_at(1);
                modified_locals.insert(idx);
                locals.insert(idx, Other);
                true
            }
            // newarray, anewarray
            0xbc | 0xbd => pop_n(&mut stack, 1).map(|_| stack.push(FreshArray)).is_some(),
            // multianewarray
            0xc5 => pop_n(&mut stack, u8_at(3))
                .map(|_| stack.push(FreshArray))
                .is_some(),
            // arraylength
            0xbe => pop_n(&mut stack, 1).map(|_| stack.push(Other)).is_some(),
            // checkcast keeps the word
            0xc0 => !stack.is_empty(),
            // j27 OOM21: putstatic of the constant or unmodified parameter just pushed, into a
            // static the caller vouches is plain.
            0xb3 => prev_rerunnable.is_some_and(|n| plain_static(pc) && pop_n(&mut stack, n).is_some()),
            _ => false,
        };
        if !ok {
            return false;
        }
        prev_rerunnable = if let Some(n) = constant_push_words(op) {
            Some(n)
        } else {
            match op {
                0x15 | 0x17 => (!modified_locals.contains(&u8_at(1))).then_some(1),
                0x1a..=0x1d => (!modified_locals.contains(&usize::from(op - 0x1a))).then_some(1),
                0x22..=0x25 => (!modified_locals.contains(&usize::from(op - 0x22))).then_some(1),
                0x19 => (!modified_locals.contains(&u8_at(1))).then_some(1),
                0x2a..=0x2d => (!modified_locals.contains(&usize::from(op - 0x2a))).then_some(1),
                0x16 | 0x18 => {
                    let idx = u8_at(1);
                    (!modified_locals.contains(&idx) && !modified_locals.contains(&(idx + 1))).then_some(2)
                }
                0x1e..=0x21 => {
                    let idx = usize::from(op - 0x1e);
                    (!modified_locals.contains(&idx) && !modified_locals.contains(&(idx + 1))).then_some(2)
                }
                0x26..=0x29 => {
                    let idx = usize::from(op - 0x26);
                    (!modified_locals.contains(&idx) && !modified_locals.contains(&(idx + 1))).then_some(2)
                }
                _ => None,
            }
        };
        pc += step;
    }
    true
}

/// The exception edges a compiled frame can take INSIDE itself, as the
/// `handlers` rows [`replay_from_entry_commits_nothing_with_handlers`] reads:
/// one row `(throw_bci, end of the throwing instruction, handler_pc)` per
/// local-handler candidate placed at or before its throw (round 13 wave 12,
/// lane replay6, `r13w12-osrdoor-vm-sinks-replay-extent-ignores-handler-edges`).
///
/// `sites` is the body's own `CompiledMethod::_jit_local_handler_sites`: the
/// only way a frame enters its own `catch` block without leaving compiled
/// code is one of those stubs (the post-invoke sentinel edge and the implicit
/// probes both allocate their site through `local_handler_site_for`), and a
/// site's candidates are the table rows that cover its throw. So these rows
/// are exactly the edges the attempt can have taken: never more than the
/// whole table the single-pass finalizer asks with, and none at all for a
/// body without local handlers (every optimizing-tier body). A VM sink that
/// holds the body it ran asks with these, so it is never stricter than the
/// compile-time check that installed the body, and never laxer than the
/// edges its frame could take. Local handlers are armed only without the
/// loop rewrite, so `throw_bci` and the handler pcs index `code` directly.
///
/// Only backward candidates are kept (a forward one changes no extent), so
/// javac output yields rows only for `synchronized`'s self-covering
/// `monitorexit` row, and usually none.
pub fn local_handler_replay_edges(
    sites: &[Box<crate::JitLocalHandlerSite>],
    code: &[u8],
    code_len: usize,
) -> Vec<(usize, usize, usize)> {
    let len = code_len.min(code.len());
    let mut edges = Vec::new();
    for site in sites {
        let Ok(throw) = usize::try_from(site.throw_bci) else {
            continue;
        };
        if throw >= len {
            continue;
        }
        let end = throw
            .saturating_add(crate::bytecode_analysis::step(code, throw).max(1))
            .min(len);
        for &(_, handler) in &site.candidates {
            let Ok(handler) = usize::try_from(handler) else {
                continue;
            };
            if handler <= throw {
                edges.push((throw, end, handler));
            }
        }
    }
    edges
}

/// Kill switch for [`first_point_needing_unsound_replay_of_method`]. **Default ON**;
/// `CRATONVM_JIT_IR_REPLAY_FALLBACK_CHECK=0` installs such artifacts again (and
/// their unresumable deopts then double-commit or raise `InternalError` at the
/// sink, as before round 12 wave 3).
fn replay_fallback_check_enabled() -> bool {
    true
}

/// The bci of the first deopt point of an artifact whose ONLY fallback would
/// be an unsound whole-method replay, or `None` when every point either
/// resumes precisely or replays exactly (round 12 wave 3, lane replay).
///
/// A method-entry sink that cannot resume a point precisely re-runs the
/// method from entry: the interpreter's tier-up sink after asking
/// [`replay_from_entry_commits_nothing_with_handlers`] (and raising `InternalError` when it
/// answers no), the `jit_bridge` call doors after asking it only to count the
/// re-run and retire the method (round 12 wave 4). Neither is
/// a correct answer for a point whose replay would commit a side effect twice,
/// and at run time there is no correct answer left. The only place one exists
/// is here, before install: the compiler discards such an artifact and the
/// single-pass backend keeps the method.
///
/// `sink_resumes` is the artifact-wide half of the sinks' resume gate, as the
/// caller computes it (`can_deopt_resume` with `CRATONVM_DEOPT_REAL`, or the
/// sinks' additive arm for a body with no monitor bytecode that is not
/// `ACC_SYNCHRONIZED`). The per-point half is checked here: a single-scope
/// frame state inside the method's code with no slot
/// [`frame_state_is_resumable`] refuses.
///
/// Skipped, as `lib.rs`'s precise-frame post-check skips them: `RETHROW`
/// points (routed to the exceptional stash, never replayed) and `OsrExit`
/// points (taken only by the OSR door's planless transfer).
///
/// Production asks [`first_point_needing_unsound_replay_of_method`] since
/// round 12 wave 7; this monitor-less form stays for the unit tests.
#[cfg(test)]
pub(crate) fn first_point_needing_unsound_replay<'a>(
    points: impl IntoIterator<Item = &'a DeoptimizationPoint>,
    code: &[u8],
    code_len: usize,
    spliced_bodies_pure: bool,
    sink_resumes: bool,
) -> Option<u32> {
    first_point_needing_unsound_replay_of_method(
        points,
        code,
        code_len,
        spliced_bodies_pure,
        sink_resumes,
        SinkMethodMonitor::None,
    )
}

/// The first point of an artifact whose only fallback is an unsound whole-method
/// replay (see the monitor-less form above), for a method whose own monitor the
/// sinks must account for (round 12 wave 7, lane replay4): a point whose frame
/// the VM's frame builder refuses for that reason
/// ([`synchronized_frame_refuses_virtuals`]) is not resumed, so its replay is
/// asked, exactly as the sinks will ask it. `lib.rs`'s optimizing post-check
/// passes the compiled method's monitor; before this the check took such a
/// point as resumed while every door re-ran it from entry
/// (`r12w6-hunter4-synchronized-method-with-virtuals-reruns-from-entry`).
pub fn first_point_needing_unsound_replay_of_method<'a>(
    points: impl IntoIterator<Item = &'a DeoptimizationPoint>,
    code: &[u8],
    code_len: usize,
    spliced_bodies_pure: bool,
    sink_resumes: bool,
    monitor: SinkMethodMonitor,
) -> Option<u32> {
    if !replay_fallback_check_enabled() {
        return None;
    }
    points
        .into_iter()
        .find(|p| {
            point_needs_unsound_replay_of_method(
                p,
                code,
                code_len,
                spliced_bodies_pure,
                sink_resumes,
                monitor,
            )
        })
        // This method's bci: the outermost scope of an inlined chain.
        .map(|p| outermost_scope(&p.frame_state).bci)
}

/// The method monitor a deopt sink holds for the frame it resumes, as far as
/// the frame builder's refusals care (round 12 wave 7, lane replay4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkMethodMonitor {
    /// Not `ACC_SYNCHRONIZED`.
    None,
    /// `static synchronized`: the class mirror, which no compile allocates.
    Static,
    /// An instance `synchronized` method: the receiver (local 0 at entry).
    Instance,
}

impl SinkMethodMonitor {
    /// The monitor of a method with these access facts.
    pub fn of(is_synchronized: bool, is_static: bool) -> Self {
        match (is_synchronized, is_static) {
            (false, _) => SinkMethodMonitor::None,
            (true, true) => SinkMethodMonitor::Static,
            (true, false) => SinkMethodMonitor::Instance,
        }
    }
}



/// Does the VM's single-frame builder refuse to rebuild a frame of a method
/// with this `monitor` because the frame names a scalar-replaced object
/// (`names_virtual`)? The ONE answer both ends read: the VM's
/// `build_deopt_frame_or_refusal` (`DeoptFrameBail::SynchronizedWithVirtuals`)
/// and [`point_needs_unsound_replay_of_method`] at compile time (round 12
/// wave 7, lane replay4).
///
/// The refusal protects the method monitor from an elision under scalar
/// replacement "that would leave no trace". For a top-level compiled frame it
/// cannot happen, because the monitor object is either the class mirror or
/// the receiver, an argument this compile did not allocate, so escape
/// analysis can never elide it. That holds whoever holds the monitor: a
/// wrapped body entered through a door that holds it
/// (`CompiledMethod::requires_wrapped_entry`), or a SELF-LOCKING body
/// (`CompiledMethod::self_locks_monitor`: instance, and static by default
/// since round 14 wave 2) that takes it in its own prologue and hands it to
/// the resumed frame at a guard exit (`CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER`,
/// [`handed_method_monitor_index`]). (Round 14 wave 3, lane fixup: this
/// paragraph used to argue "no tier compiles the method monitor at all",
/// which self-locking bodies made false; the conclusion did not depend on
/// it.) So only an instance frame whose local 0 is itself virtual (a
/// method that overwrote `this` with an object it allocated, the one shape in
/// which "the receiver is scalar-replaced" can even be written down) is still
/// refused; everything else materialises and resumes under the door's hold.
/// With the kill switch off every such frame is refused, as before.
pub fn synchronized_frame_refuses_virtuals(
    monitor: SinkMethodMonitor,
    locals: &[FrameValue],
    names_virtual: bool,
) -> bool {
    if monitor == SinkMethodMonitor::None || !names_virtual {
        return false;
    }
    monitor == SinkMethodMonitor::Instance
        && locals.first().is_some_and(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
            )
        })
}

/// `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER` (round 13 wave 9, lane chain4;
/// **default ON** since round 14 wave 2, `0` turns it off): may a SELF-LOCKING synchronized body
/// (`CompiledMethod::self_locks_monitor`) keep its guard exits (stub reasons 2
/// and 6) by handing the method monitor it holds to the frame the VM resumes,
/// instead of the body check refusing it
/// (`r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md`)? Read by the
/// single-pass producer (the point names the hold, the stub keeps it) and by
/// the body check that admits such stubs
/// (`x64::Compiler::self_lock_deopt_exits_hand_over`). The VM sinks act on
/// what a stashed frame names ([`handed_method_monitor_index`]) and read no
/// switch. Not memoised (`process_global_statics_ratchet`): read per guard
/// point of a self-locking compile.
pub(crate) fn self_lock_deopt_handover_enabled() -> bool {
    // Default ON since JIT round 14 wave 2 (checklist on
    // `r13w6-sync3-self-lock-deopt-hand-over-design`: probes, battery census,
    // bench); `=0` restores the releasing stubs.
    true
}

/// `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER` (round 13 wave 11, lane
/// sync7; **default ON**, `0` restores wave 10): the hand-over of
/// [`self_lock_deopt_handover_enabled`] for a `static synchronized`
/// self-locking body too, whose monitor is its class mirror
/// (`r13w10-sync6-static-self-lock-deopt-hand-over-patch-FIXED-20260929.md`).
/// Effective only when that switch AND `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`
/// are on (no static self-locking body exists otherwise), so it changes
/// nothing by default. The VM sinks read no switch: they act on what a stashed
/// frame names ([`handed_method_monitor_index`]). Not memoised: read per guard
/// point of a static self-locking compile.
pub(crate) fn self_lock_static_deopt_handover_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER` (interpreter round i2 wave 56,
/// lane L2; shape 3's `self-lock` refusal of
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// may a single-pass SELF-LOCKING method-entry body's MODE exits -- its
/// back-edge polls, conditional back edges, post-call and constant-pool helper
/// exit sites, every `OsrExit` point -- hand the method monitor over to the
/// frame the VM resumes, exactly as its reason-2/6 guards do under
/// [`self_lock_deopt_handover_enabled`]? Off, such a body's polls stay
/// verdict-blind (`x64/safepoint.rs` `holds_self_lock_no_frame_names`): an
/// agent's or a debugger's request, or a redefinition that withdrew it, finds
/// `Hashtable.get`'s or `Vector.indexOf`'s loop finishing its activation
/// compiled. On, the `OsrExit` point names the hold as its one monitor, its
/// stub leaves it held, and the VM resumes the frame owning it as its
/// `monitor_on_exit` ([`handed_method_monitor_index`], which reads no
/// reason). Effective only with [`self_lock_deopt_handover_enabled`] (and, for
/// a static body, [`self_lock_static_deopt_handover_enabled`]). **Default
/// ON** since the wave-56 lane's switch-on commit (revert that commit to turn
/// it off by default); `=0` is the kill switch. Not memoised
/// (`process_global_statics_ratchet`): read per admission and per point of a
/// self-locking compile.
pub(crate) fn self_lock_mode_exit_handover_enabled() -> bool {
    true
}

/// Round 13 wave 9 (lane chain4): the index in `frame.monitors` of the METHOD
/// monitor a self-locking body handed over with this stashed frame, or `None`.
///
/// A self-locking body's guard stub leaves WITHOUT releasing the monitor its
/// prologue took, and its point names that hold as the frame's only monitor,
/// `{ local 0's frame home, lock_depth 1, relock false }`, with local 0
/// described from the same home (`x64/deopt_stubs.rs`
/// `build_and_record_deopt_point`). The resumed interpreter frame must own it
/// as its `monitor_on_exit` -- the implicit release of an `ACC_SYNCHRONIZED`
/// return or unwind -- and NOT as a block monitor (`held_monitors`), whose
/// return throws `IllegalMonitorStateException` (JVMS 2.11.10). Every sink
/// that abandons the frame instead releases it like any lock the compiled
/// code took (`relock == false`: `CompiledLocksOfAStash`), and every sink that
/// puts the frame back into the stash keeps it there.
///
/// Recognised by shape, and the shape is exclusive: an instance synchronized
/// method whose bytecode takes no monitor (`method_takes_monitors == false`)
/// has no `monitorenter` a frame could describe, and a re-execute point never
/// sits inside a call (a caller-held synchronized call's hold exists only
/// across its `CALL`), so a TAKEN lock such a frame names on its own `this` is
/// the method monitor. Also required: a single scope (the planner gives a
/// synchronized method no inlined chains), exactly one monitor, one level, not
/// an elided one, on the non-null object local 0 holds. A wrapped body of such
/// a method names no monitor at all (its door holds the method monitor), so
/// the answer there is `None` and nothing changes for it.
///
/// Round 13 wave 11 (lane sync7,
/// `r13w10-sync6-static-self-lock-deopt-hand-over-patch-FIXED-20260929.md`): a
/// `static synchronized` method too. Its self-locking body's guard names the
/// class mirror, loaded from the class's GC-maintained mirror slot by the
/// hand-over stub right before `x64_deopt_entry` reads the frame
/// (`x64/deopt_stubs.rs`). There is no `this` to match: the exclusivity
/// argument is the rest of the instance one (no monitor op in the method, one
/// scope, no re-execute point inside a caller-held call, no elided level), so
/// the one taken, non-null level such a frame names is the method monitor.
pub fn handed_method_monitor_index(
    monitor: SinkMethodMonitor,
    method_takes_monitors: bool,
    frame: &ReconstructedFrame,
) -> Option<usize> {
    if monitor == SinkMethodMonitor::None || !frame.caller_frames.is_empty() {
        return None;
    }
    // JIT round 18 wave 9 (lane syncdoor6, SD17-3): a self-locking body with
    // `synchronized` blocks MARKS the points that hand its hold over, naming
    // it first and the open blocks after it
    // (`ResumeSemantics::method_monitor_first`). The mark replaces the
    // exclusivity argument above, which monitor bytecode breaks; everything
    // else about the first monitor is checked as for the unmarked shape.
    let m = if frame.semantics.method_monitor_first {
        frame.monitors.first()?
    } else {
        if method_takes_monitors {
            return None;
        }
        let [m] = frame.monitors.as_slice() else {
            return None;
        };
        m
    };
    let held = match m.object {
        FrameValue::Object(addr) if addr != 0 && !m.relock && m.lock_depth == 1 => addr,
        _ => return None,
    };
    match monitor {
        SinkMethodMonitor::Instance => match frame.locals.first() {
            Some(FrameValue::Object(this)) if *this == held => Some(0),
            _ => None,
        },
        SinkMethodMonitor::Static => Some(0),
        SinkMethodMonitor::None => None,
    }
}

#[cfg(test)]
mod r13w9_chain4_handover_tests;



/// Does one scope of `fs` name a scalar-replaced object in a local, a stack
/// slot or a held monitor (the VM builder's `has_virtual` test)?
fn frame_scope_names_virtual(fs: &FrameState) -> bool {
    fs.locals
        .iter()
        .chain(fs.stack.iter())
        .chain(fs.monitors.iter().map(|m| &m.object))
        .any(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
            )
        })
}

/// `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS`, **default OFF** (round 14 wave 4,
/// lane resume2; proposal CH3W-4 of `jit-r14-chain3-proposals-RETIRED-20260930.md`, item 4 of
/// `r13w3-framestate-vm-chain-resume-gaps`): a census switch that lets an
/// `ACC_SYNCHRONIZED` compiling method publish inlined chains. Read here, by
/// the monitor clause of [`point_needs_unsound_replay_with_handlers`] (a
/// synchronized method's chain point is taken as resumed when the chain sinks
/// can materialise it, instead of asked the replay question), and by the IR
/// planner's three `!cached.is_synchronized` conditions (`ir_chain_fences`,
/// the `splice_scope_class_ids` / `splice_scope_sources` block,
/// `scope_method_key`; `r14w4-resume2-lib-sync-method-chains-patch-FIXED-20260929.md`).
/// Off, both are unchanged. On, the doors hand the method monitor to the
/// outermost pushed frame (chain4's reading); the census (`CRATONVM_DBG_DEOPT=1`,
/// the `callee-resume refused (stash method is ACC_SYNCHRONIZED)` line of the
/// call-site service) decides whether it can become a default. Not memoised
/// (`process_global_statics_ratchet`): read per synchronized compile / chain
/// point.
pub(crate) fn sync_method_chains_enabled() -> bool {
    false
}

/// j27 wave 3 (lane BACKLOG, CN16-3): the ONE spelling of the IR planner's
/// three sync-method chain conditions ([`sync_method_chains_enabled`]'s doc
/// names them), bound once per compile in `ir_tier_attempt`. A method may
/// plan deopt chains unless it is `ACC_SYNCHRONIZED`; a synchronized one only
/// under the census switch, and never as a self-locking body (which keeps
/// flat frames).
pub(crate) fn ir_method_admits_chains(is_synchronized: bool, self_lock_requested: bool) -> bool {
    method_admits_chains(is_synchronized, self_lock_requested, sync_method_chains_enabled)
}

/// [`ir_method_admits_chains`] with the switch read injected. The switch is
/// read only for a synchronized, non-self-locking method.
fn method_admits_chains(
    is_synchronized: bool,
    self_lock_requested: bool,
    chains_on: impl FnOnce() -> bool,
) -> bool {
    !is_synchronized || (!self_lock_requested && chains_on())
}

/// Does the VM refuse to resume this chain point of an `ACC_SYNCHRONIZED`
/// method because some scope names a scalar-replaced object? The compile-time
/// half of `deopt_resume::chain_virtuals_refusal`, which refuses every such
/// chain of a synchronized outermost method (round 13 wave 5) on the ground
/// that "the compile-time replay check does not take such a chain as
/// resumed". [`sync_method_chains_enabled`] made that ground false: under
/// it, with `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS=1` too, the replay check took
/// the chain as resumed, the body was installed without its replay fence, and
/// every trap at the point was refused at the VM and re-run from entry (an
/// `InternalError` under the doors' unsound-rerun rule). Round 16 wave 14
/// (lane framestate): such a point takes the replay question, as a flat
/// frame the builder refuses does ([`synchronized_frame_refuses_virtuals`]).
/// Kill switch `CRATONVM_JIT_IR_SYNC_CHAIN_VIRTUALS_REPLAY_CHECK` (default
/// ON), read only for a synchronized method's chain that names one.
fn sync_chain_virtuals_refused(fs: &FrameState) -> bool {
    let mut scope = Some(fs);
    let mut seen = 0usize;
    let mut names_virtual = false;
    while let Some(f) = scope {
        if frame_scope_names_virtual(f) {
            names_virtual = true;
            break;
        }
        seen += 1;
        if seen >= MAX_SCOPE_CHAIN {
            break;
        }
        scope = f.caller.as_deref();
    }
    names_virtual
}

/// The per-point half of [`first_point_needing_unsound_replay_of_method`], with no kill
/// switch: would a method-entry sink holding this point's frame have to fall
/// back to a replay from entry that commits a side effect again? (Round 12
/// wave 4, lane replay2: shared with the single-pass form below, so the two
/// tiers ask one question.)
///
/// `false` for a `RETHROW` point (routed to the exceptional stash, never
/// replayed) and an `OsrExit` point (taken only by the OSR door's transfer).
///
/// Test-only since round 13 wave 10 (lane exc4): the single-pass form asks
/// the handler-aware per-point question
/// (`point_needs_unsound_replay_with_handlers`) with the same arguments.
#[cfg(test)]
pub fn point_needs_unsound_replay(
    p: &DeoptimizationPoint,
    code: &[u8],
    code_len: usize,
    spliced_bodies_pure: bool,
    sink_resumes: bool,
) -> bool {
    point_needs_unsound_replay_of_method(
        p,
        code,
        code_len,
        spliced_bodies_pure,
        sink_resumes,
        SinkMethodMonitor::None,
    )
}

/// The per-point half of [`first_point_needing_unsound_replay_of_method`] for a
/// method with `monitor`: would a method-entry sink holding this point's frame
/// have to fall back to a replay from entry that commits a side effect again?
/// A frame the builder refuses for the monitor
/// ([`synchronized_frame_refuses_virtuals`]) is not resumed (round 12 wave 7,
/// lane replay4). `false` for a `RETHROW` point and an `OsrExit` point.
pub fn point_needs_unsound_replay_of_method(
    p: &DeoptimizationPoint,
    code: &[u8],
    code_len: usize,
    spliced_bodies_pure: bool,
    sink_resumes: bool,
    monitor: SinkMethodMonitor,
) -> bool {
    point_needs_unsound_replay_with_handlers(
        p,
        code,
        code_len,
        spliced_bodies_pure,
        sink_resumes,
        monitor,
        &[],
    )
}

/// [`point_needs_unsound_replay_of_method`] asking the replay question with
/// the method's exception table (`(start_pc, end_pc, handler_pc)`), for a
/// frame that runs its own handlers
/// ([`replay_from_entry_commits_nothing_with_handlers`]; round 13 wave 10,
/// lane exc4).
fn point_needs_unsound_replay_with_handlers(
    p: &DeoptimizationPoint,
    code: &[u8],
    code_len: usize,
    spliced_bodies_pure: bool,
    sink_resumes: bool,
    monitor: SinkMethodMonitor,
    handlers: &[(usize, usize, usize)],
) -> bool {
    if p.semantics.rethrow_exception || p.reason == DeoptReason::OsrExit {
        return false;
    }
    let fs = &p.frame_state;
    // Round 13 wave 3 (lane framestate): an inlined caller chain (a guard in a
    // spliced body, `ir_lower`'s `splice_chain_for_site`) names the CALLEE's
    // bci in its innermost scope. Everything asked here about THIS method's
    // bytecode is asked of the outermost scope, which is this method parked
    // mid-`invoke`: the replay question in particular, whose answer for the
    // flat frame the chain replaced was asked at that same `invoke`. A chain
    // resumes exactly when the chain sinks can materialise it
    // ([`inlined_chain_refusal`]); a synchronized method is not asked to
    // (the door hands its monitor to ONE resumed frame), so its chain point
    // takes the replay question.
    let outer = outermost_scope(fs);
    let chain = fs.caller.is_some();
    let frame_resumes = if chain {
        // Round 14 wave 4 (lane resume2, CH3W-4): the census switch takes a
        // synchronized method's chain as resumed too (read only for one).
        // Round 16 wave 14 (lane framestate): except a chain naming a
        // scalar-replaced object, which the VM's chain builder refuses for
        // a synchronized outermost method (`chain_virtuals_refusal`).
        (monitor == SinkMethodMonitor::None
            || (sync_method_chains_enabled() && !sync_chain_virtuals_refused(fs)))
            && inlined_chain_refusal(fs).is_none()
    } else {
        frame_state_is_resumable(fs)
            && (monitor == SinkMethodMonitor::None
                || !synchronized_frame_refuses_virtuals(
                    monitor,
                    &fs.locals,
                    frame_scope_names_virtual(fs),
                ))
    };
    let resumes = sink_resumes && (outer.bci as usize) < code_len && frame_resumes;
    !resumes
        && !replay_from_entry_commits_nothing_with_handlers(
            code,
            code_len,
            spliced_bodies_pure,
            outer.bci,
            // `first_arrival` is a fact about the TRAPPING scope's bci, which
            // for a chain is not this method's.
            p.semantics.first_arrival && !chain,
            handlers,
        )
}

/// The outermost scope of `fs`'s inlined caller chain -- the compiling method,
/// parked mid-`invoke` -- or `fs` itself when it has no caller. Bounded like
/// every other chain walk here ([`MAX_SCOPE_CHAIN`]).
pub fn outermost_scope(fs: &FrameState) -> &FrameState {
    let mut scope = fs;
    let mut seen = 0usize;
    while let Some(caller) = scope.caller.as_deref() {
        seen += 1;
        if seen >= MAX_SCOPE_CHAIN {
            break;
        }
        scope = caller;
    }
    scope
}



/// [`single_pass_first_trap_needing_unsound_replay_with_handlers`] with an
/// empty exception table, for the unit tests. Test-only since round 13 wave
/// 10 (lane exc4): the backend asks the handler-aware form, since its local
/// handlers run `catch` blocks inside the compiled frame.
#[cfg(test)]
pub fn single_pass_first_trap_needing_unsound_replay<'a>(
    baked_points: impl IntoIterator<Item = &'a DeoptimizationPoint>,
    frameless_trap_bcis: impl IntoIterator<Item = u32>,
    code: &[u8],
    code_len: usize,
    sink_resumes: bool,
) -> Option<u32> {
    single_pass_first_trap_needing_unsound_replay_with_handlers(
        baked_points,
        frameless_trap_bcis,
        code,
        code_len,
        sink_resumes,
        &[],
    )
}

/// The single-pass twin of [`first_point_needing_unsound_replay_of_method`] (round 12
/// wave 4, lane replay2; proposal W3-2 of `docs/feature-designs/jit-ir-optimizations.md`): the
/// interpreter bci of the first trap of a single-pass artifact whose only
/// fallback is an unsound replay from entry, or `None`.
///
/// The single-pass backend has two kinds of trap exit, and a method-entry door
/// (`jit_bridge`'s `jit-callsite-a` / `-b`, the synchronized door) re-runs the
/// method from bci 0 on both when it cannot resume:
///
/// * a FRAME stub bakes one `DeoptimizationPoint` (`baked_points`: only the
///   points some stub actually bakes, not every snapshot the emitter
///   recorded). Refused when the per-point question
///   ([`point_needs_unsound_replay_of_method`], here with the table) says so:
///   no sink resumes the frame (an `Unsupported` slot the width analysis could
///   not type, `sink_resumes` false) and the replay would commit again;
/// * a FRAMELESS stub (`jit_uncommon_trap`, the arm that sets
///   `has_frameless_trap_stub`) stashes nothing, so the door can only re-run.
///   Refused when the conditional-deopt replay at its bci
///   (`frameless_trap_bcis`, interpreter space) would commit again. A
///   frameless trap is never a first-arrival point: the emitter keeps no such
///   fact for it.
///
/// `code[..code_len]` is the method's ORIGINAL bytecode (a loop rewrite's
/// output is not what the bcis index). Spliced bodies are taken as pure: no
/// single-pass point or frameless trap is published from inside a splice
/// (`Compiler::may_file_by_bci`), and a trap at an outer bci after a splice
/// sees the splice's `invoke` in the caller's own prefix.
///
/// `handlers` is the method's ORIGINAL exception table (`(start_pc, end_pc,
/// handler_pc)`, interpreter bcis, as `code` is), so a handler placed below
/// its range's end counts as a way back to the trap (round 13 wave 10, lane
/// exc4, `r13w9-replay5-replay-extent-ignores-exception-edges`;
/// [`replay_from_entry_commits_nothing_with_handlers`]). The whole table is a
/// superset of the edges the VM sinks judge with
/// ([`local_handler_replay_edges`], the body's own local-handler sites), so a
/// body this check installs never meets a stricter verdict at a sink.
pub fn single_pass_first_trap_needing_unsound_replay_with_handlers<'a>(
    baked_points: impl IntoIterator<Item = &'a DeoptimizationPoint>,
    frameless_trap_bcis: impl IntoIterator<Item = u32>,
    code: &[u8],
    code_len: usize,
    sink_resumes: bool,
    handlers: &[(usize, usize, usize)],
) -> Option<u32> {
    let needs_replay = |p: &DeoptimizationPoint| {
        point_needs_unsound_replay_with_handlers(
            p,
            code,
            code_len,
            true,
            sink_resumes,
            SinkMethodMonitor::None,
            handlers,
        )
    };
    if let Some(p) = baked_points.into_iter().find(|&p| needs_replay(p)) {
        return Some(p.frame_state.bci);
    }
    let replays_exactly = |bci: u32| {
        replay_from_entry_commits_nothing_with_handlers(code, code_len, true, bci, false, handlers)
    };
    frameless_trap_bcis
        .into_iter()
        .find(|&bci| !replays_exactly(bci))
}

// ---------------------------------------------------------------------------
// Install-time deopt-metadata verification
// ---------------------------------------------------------------------------
//
// The P0 acceptance criteria this section serves:
//
//   "Any guard or dependency failure reconstructs byte-for-byte equivalent
//    interpreter state."
//   "Moving GC at every call, allocation, poll, and deopt site preserves all
//    objects and updates every reference."
//
// Neither can be proved by inspecting one slot at a time, because both are
// *agreement* properties: a scope must agree with its method's bytecode
// (`bci < code_len`, `locals.len() <= max_locals`), and a reference-typed deopt
// slot must agree with the oop map (if the deopt map will hand the interpreter
// the word at `[rbp-40]` as an object, the collector must know to rewrite that
// same word — otherwise a moving collection between the safepoint and the
// resume leaves the interpreter holding a pre-copy address).
//
// So the checks live here, run over the *emitted* metadata, and return a
// `CompileResult<()>`: a disagreement bails the compile instead of installing
// an artifact whose deopt cannot be reconstructed. That is strictly better than
// discovering it at deopt time, when the only remaining options are a
// whole-method re-run (duplicated side effects) or a wrong frame.

/// Which part of a frame a violation was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// A local-variable slot.
    Local,
    /// An operand-stack slot (index 0 = bottom of stack).
    Stack,
    /// A held monitor's object.
    Monitor,
    /// A field of a scalar-replaced object that a slot materializes.
    VirtualField {
        /// [`VirtualObjectState::id`] of the object owning the field.
        object_id: usize,
    },
}

impl fmt::Display for SlotKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Stack => f.write_str("stack"),
            Self::Monitor => f.write_str("monitor"),
            Self::VirtualField { object_id } => write!(f, "vobj#{object_id}.field"),
        }
    }
}

/// A fully-qualified address of one slot inside emitted deopt metadata:
/// which deopt point (native PC), which inlined scope (depth + method), and
/// which slot of that scope.
///
/// Every [`DeoptMetadataError`] that concerns a slot carries one, so a
/// violation message names the exact PC/BCI/slot rather than "some frame is
/// wrong" — the difference between an actionable bailout and a mystery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRef {
    /// Native code offset of the deopt point this slot belongs to.
    pub native_offset: u32,
    /// Bytecode index of the *scope* (not necessarily of the deopt point: an
    /// inlined caller resumes at its own call bci).
    pub bci: u32,
    /// Method key of the scope.
    pub method_key: String,
    /// 0 for the innermost (trapping) scope, 1 for its inlined caller, …
    pub scope_depth: usize,
    /// Which part of the frame.
    pub kind: SlotKind,
    /// Index within that part.
    pub index: usize,
}

impl SlotRef {
    /// A slot reference for a field of a virtual object defined at `self`.
    fn field(&self, object_id: usize, index: usize) -> SlotRef {
        SlotRef {
            native_offset: self.native_offset,
            bci: self.bci,
            method_key: self.method_key.clone(),
            scope_depth: self.scope_depth,
            kind: SlotKind::VirtualField { object_id },
            index,
        }
    }
}

impl fmt::Display for SlotRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pc+0x{:x} scope{}", self.native_offset, self.scope_depth)?;
        if !self.method_key.is_empty() {
            write!(f, " {}", self.method_key)?;
        }
        write!(f, " bci {} {}[{}]", self.bci, self.kind, self.index)
    }
}

/// A defect found in emitted deoptimization metadata.
///
/// Every variant names enough to locate the defect in the compiler output
/// without re-running the compile — that is the whole point of making these
/// structured rather than a `bool` or a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeoptMetadataError {
    /// A machine-location descriptor named a register outside the 16-entry
    /// GPR/XMM files the deopt stub spills. Produced by the *runtime* resolver
    /// (where it degrades to an unresumable slot) as well as the verifier.
    RegisterFileIndexOutOfRange {
        /// `"gpr"` or `"xmm"`.
        bank: &'static str,
        /// The out-of-range register number.
        index: u8,
    },
    /// A scope named a method the verifier has no limits for, so its bci and
    /// slot counts cannot be checked at all.
    UnknownMethod {
        native_offset: u32,
        bci: u32,
        method_key: String,
        scope_depth: usize,
    },
    /// A scope's resume bci is not a valid index into its method's code.
    BciOutOfRange {
        native_offset: u32,
        bci: u32,
        method_key: String,
        scope_depth: usize,
        code_len: u32,
    },
    /// A scope declares more locals than its method has slots for.
    LocalCountMismatch {
        native_offset: u32,
        bci: u32,
        method_key: String,
        scope_depth: usize,
        found: usize,
        max_locals: u16,
    },
    /// A scope declares a deeper operand stack than its method's `max_stack`.
    StackCountMismatch {
        native_offset: u32,
        bci: u32,
        method_key: String,
        scope_depth: usize,
        found: usize,
        max_stack: u16,
    },
    /// The deopt point's own bci disagrees with its innermost scope's bci.
    PointBciMismatch {
        native_offset: u32,
        point_bci: u32,
        frame_bci: u32,
    },
    /// A deopt point carries reference-typed slots but no oop map was
    /// registered for its native offset — the GC has no description of this
    /// program point, so a moving collection cannot update those references.
    MissingOopMap { native_offset: u32, bci: u32 },
    /// A reference-typed frame slot the deopt map will read as an object is not
    /// covered by the oop map. **This is the moving-GC correctness bug**: the
    /// collector will not rewrite that word, so the resume hands the
    /// interpreter a pre-copy address.
    ReferenceNotInOopMap {
        at: SlotRef,
        /// The `[rbp - off]` offset, as the oop map spells it (positive).
        frame_offset: i32,
    },
    /// A reference-typed *register* slot is not covered by the oop map.
    ReferenceRegisterNotInOopMap { at: SlotRef, reg: u8 },
    /// A reference slot's frame offset does not fit the `i16` the oop map
    /// encodes offsets in, so it is unrepresentable to the GC by construction.
    UnencodableRefOffset { at: SlotRef, frame_offset: i32 },
    /// A raw heap address was baked into the metadata as a constant. Nothing
    /// can update it when the object moves, so it is only ever valid as a
    /// *resolved* value, never as emitted metadata.
    BakedObjectAddress { at: SlotRef, address: u64 },
    /// A slot names an IR node the optimizer removed, without carrying the
    /// recipe needed to rebuild its value.
    SlotNamesRemovedNode { at: SlotRef, node: u32 },
    /// A `VirtualObjectRef(id)` edge with no `VirtualObject(id)` definition
    /// anywhere in the same scope — materialization would have nothing to point
    /// the slot at.
    UndefinedVirtualObjectRef { at: SlotRef, id: usize },
    /// The same virtual-object id is *defined* twice in one scope. Each object
    /// must be defined exactly once; later occurrences are `VirtualObjectRef`.
    DuplicateVirtualObjectDefinition { at: SlotRef, id: usize },
    /// A virtual object's `num_fields` and `field_values.len()` disagree.
    VirtualObjectFieldCountMismatch {
        at: SlotRef,
        id: usize,
        declared: usize,
        found: usize,
    },
    /// The monitor list of a scope cannot be replayed as balanced
    /// enter/exit pairs.
    UnbalancedMonitor { at: SlotRef, detail: String },
    /// `deopt_points` is not sorted by `native_offset`: a point at a LOWER
    /// offset follows one at a higher offset.
    ///
    /// The reason this is a finding is the producer, not a consumer. Both
    /// backends record a point at `buf.pos()` while the emitter walks forward,
    /// so a strictly-decreasing pair means either a producer pushed out of
    /// order or a loop rewrite re-based offsets wrongly
    /// (`x64::loop_rewrite::rewritten_deopt_points_are_publishable` checks the
    /// rewrite's own side of that). It used to be justified by
    /// `CompiledMethod::find_deopt_point`'s binary search; that search had no
    /// production caller and was deleted in round 10, and the justification
    /// never needed it — which is why this lane outlived it unchanged.
    ///
    /// **Deliberately `>`, not `>=`: two points at the SAME offset are legal
    /// metadata and are not reported.** `build_and_record_deopt_point` emits no
    /// machine code, and the three recorder families that call it key their
    /// idempotence in three different maps (`deopt_box_ptr_by_bci`,
    /// `osr_exit_box_ptr_by_bci`, `exc_frame_box_ptr_by_bci`) which do not see
    /// each other, so two adjacent metadata-only records at one pc land on one
    /// offset. Tightening to `>=` would refuse such an artifact to prove a
    /// hypothesis; see
    /// `docs/feature-designs/deopt-osr.md`,
    /// which is also where the three places that claim uniqueness are listed.
    DeoptPointsUnsorted { first: u32, second: u32 },
    /// The inlined caller chain is longer than [`MAX_SCOPE_CHAIN`], so it
    /// cannot be described in full. Reported rather than truncated: a chain
    /// silently cut at the cap reconstructs a *plausible* stack (the frames
    /// below the cut, with the outermost one claiming to be the bottom) instead
    /// of the correct one.
    ScopeChainTooDeep { native_offset: u32, cap: usize },
    /// A frame-slot descriptor's offset does not address a word inside the
    /// trapping frame. Every producer encodes a positive `[rbp - spill]` offset
    /// as its negation, so a non-negative offset is either `[rbp]` (the saved
    /// caller frame pointer) or a word in the *caller's* frame.
    FrameSlotOutsideFrame { at: SlotRef, offset: i32 },
    /// One frame word is described twice in the same scope with two different
    /// JVM value categories. A machine word holds one value at one program
    /// point, so at most one of the two descriptions can be right, and the
    /// resume has no way to tell which.
    SlotTypeConflict {
        /// The second (conflicting) description.
        at: SlotRef,
        /// The first description of the same word.
        first: SlotRef,
        /// The `*(rbp + off)` offset both descriptions name.
        offset: i32,
        /// Category the first description claims.
        first_kind: &'static str,
        /// Category this description claims.
        second_kind: &'static str,
    },
    /// The deopt map describes a location as a **primitive** that the oop map
    /// for the same safepoint lists as holding a **live reference**. The two
    /// disagree about the type of one word, so one of them is wrong: either the
    /// resume hands the interpreter an `int` where a reference belongs, or the
    /// collector relocates against a word that is not an object pointer.
    PrimitiveSlotCoveredByOopMap {
        at: SlotRef,
        /// `"gpr"` for a register location, `"frame"` for a frame slot.
        bank: &'static str,
        /// The GPR number, or the positive `[rbp - off]` frame offset.
        location: i32,
        /// What the deopt map claims the location holds.
        kind: &'static str,
    },
    /// A held monitor names an object the resume cannot lock: a value that is
    /// not reference-shaped at all (an `int` cannot be a monitor), or an
    /// explicit null. Either way the unwinding `monitorexit` count is wrong,
    /// which deadlocks the next acquirer rather than failing visibly.
    MonitorObjectNotAReference { at: SlotRef, detail: String },
    /// The point's [`ResumeSemantics`] and its [`DeoptReason`] disagree about
    /// whether a Java exception is pending. The two are read by *different*
    /// consumers — `x64_deopt_entry` routes on `reason`, a resume sink reads
    /// `semantics` — so a disagreement means one of them resumes a frame the
    /// other one knows is exceptional.
    ResumeSemanticsMismatch {
        native_offset: u32,
        reason: DeoptReason,
        rethrow: bool,
    },
}

impl fmt::Display for DeoptMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegisterFileIndexOutOfRange { bank, index } => write!(
                f,
                "{bank}[{index}] is outside the 16-entry register file the deopt stub spills"
            ),
            Self::UnknownMethod {
                native_offset,
                bci,
                method_key,
                scope_depth,
            } => write!(
                f,
                "pc+0x{native_offset:x} scope{scope_depth} bci {bci}: no frame limits registered \
                 for method {:?} — its bci and slot counts are unverifiable",
                method_key
            ),
            Self::BciOutOfRange {
                native_offset,
                bci,
                method_key,
                scope_depth,
                code_len,
            } => write!(
                f,
                "pc+0x{native_offset:x} scope{scope_depth} {method_key}: resume bci {bci} is \
                 outside the method's {code_len}-byte code"
            ),
            Self::LocalCountMismatch {
                native_offset,
                bci,
                method_key,
                scope_depth,
                found,
                max_locals,
            } => write!(
                f,
                "pc+0x{native_offset:x} scope{scope_depth} {method_key} bci {bci}: {found} local \
                 slot(s) recorded but max_locals is {max_locals}"
            ),
            Self::StackCountMismatch {
                native_offset,
                bci,
                method_key,
                scope_depth,
                found,
                max_stack,
            } => write!(
                f,
                "pc+0x{native_offset:x} scope{scope_depth} {method_key} bci {bci}: {found} operand \
                 stack slot(s) recorded but max_stack is {max_stack}"
            ),
            Self::PointBciMismatch {
                native_offset,
                point_bci,
                frame_bci,
            } => write!(
                f,
                "pc+0x{native_offset:x}: deopt point bci {point_bci} disagrees with its frame \
                 state bci {frame_bci}"
            ),
            Self::MissingOopMap { native_offset, bci } => write!(
                f,
                "pc+0x{native_offset:x} bci {bci}: reference-typed deopt slots but no oop map — a \
                 moving collection cannot update them"
            ),
            Self::ReferenceNotInOopMap { at, frame_offset } => write!(
                f,
                "{at}: reference at [rbp-{frame_offset}] is not in the oop map — the GC will not \
                 update it, so the resumed frame would hold a stale address"
            ),
            Self::ReferenceRegisterNotInOopMap { at, reg } => write!(
                f,
                "{at}: reference in gpr[{reg}] is not in the oop map — the GC will not update it"
            ),
            Self::UnencodableRefOffset { at, frame_offset } => write!(
                f,
                "{at}: reference offset {frame_offset} does not fit the i16 the oop map encodes"
            ),
            Self::BakedObjectAddress { at, address } => write!(
                f,
                "{at}: raw heap address {address:#x} baked into deopt metadata — nothing can \
                 update it when the object moves"
            ),
            Self::SlotNamesRemovedNode { at, node } => write!(
                f,
                "{at}: names removed node n{node} with no materialization recipe — the deopt \
                 frame would rebuild this slot from nothing"
            ),
            Self::UndefinedVirtualObjectRef { at, id } => write!(
                f,
                "{at}: VirtualObjectRef({id}) has no defining VirtualObject in this scope"
            ),
            Self::DuplicateVirtualObjectDefinition { at, id } => write!(
                f,
                "{at}: virtual object {id} is defined twice in one scope (later occurrences must \
                 be VirtualObjectRef)"
            ),
            Self::VirtualObjectFieldCountMismatch {
                at,
                id,
                declared,
                found,
            } => write!(
                f,
                "{at}: virtual object {id} declares {declared} field(s) but carries {found}"
            ),
            Self::UnbalancedMonitor { at, detail } => {
                write!(f, "{at}: unbalanced monitor state — {detail}")
            }
            Self::DeoptPointsUnsorted { first, second } => write!(
                f,
                "deopt points are not sorted by native offset (0x{first:x} precedes 0x{second:x}) \
                 — the emitter walks forward, so a producer recorded out of order"
            ),
            Self::ScopeChainTooDeep { native_offset, cap } => write!(
                f,
                "pc+0x{native_offset:x}: inlined caller chain is deeper than the {cap}-scope cap — \
                 the frames above the cut cannot be described, and a truncated chain resumes a \
                 stack that never existed"
            ),
            Self::FrameSlotOutsideFrame { at, offset } => write!(
                f,
                "{at}: frame-slot offset {offset} does not address this frame — *(rbp{offset:+}) \
                 is the saved frame pointer or the caller's frame, not a spill slot"
            ),
            Self::SlotTypeConflict {
                at,
                first,
                offset,
                first_kind,
                second_kind,
            } => write!(
                f,
                "{at}: describes *(rbp{offset:+}) as {second_kind}, but {first} already describes \
                 the same word as {first_kind} — one machine word holds one value"
            ),
            Self::PrimitiveSlotCoveredByOopMap {
                at,
                bank,
                location,
                kind,
            } => write!(
                f,
                "{at}: deopt map reads {bank}[{location}] as {kind}, but the oop map lists it as a \
                 live reference — the resume and the collector disagree about the type of one word"
            ),
            Self::MonitorObjectNotAReference { at, detail } => {
                write!(f, "{at}: monitor object is not lockable — {detail}")
            }
            Self::ResumeSemanticsMismatch {
                native_offset,
                reason,
                rethrow,
            } => write!(
                f,
                "pc+0x{native_offset:x}: reason {reason:?} and rethrow_exception={rethrow} \
                 disagree about whether an exception is pending — the stash routing and the \
                 resume sink would take opposite branches"
            ),
        }
    }
}

impl std::error::Error for DeoptMetadataError {}

/// The bytecode-level facts a scope is checked against.
///
/// Supplied by the compiler at install time from the same `MethodInfo` the
/// front end parsed, so "the frame agrees with the method" is checked against
/// the method itself and not against a second, drifting copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodFrameLimits {
    /// `"<class>.<method>:<descriptor>"`, matching [`FrameState::method_key`].
    pub method_key: String,
    /// Length of the method's `Code` attribute in bytes. A resume bci must be
    /// `< code_len`.
    pub code_len: u32,
    /// The method's `max_locals`.
    pub max_locals: u16,
    /// The method's `max_stack`.
    pub max_stack: u16,
}

impl MethodFrameLimits {
    pub fn new(
        method_key: impl Into<String>,
        code_len: u32,
        max_locals: u16,
        max_stack: u16,
    ) -> Self {
        Self {
            method_key: method_key.into(),
            code_len,
            max_locals,
            max_stack,
        }
    }
}

/// What the GC knows about one safepoint — the other half of the agreement the
/// verifier checks.
///
/// `frame_slot_offsets` uses the same encoding as `crate::OopMapEntry`:
/// **positive** offsets naming the word at `[rbp - off]`. Deopt metadata spells
/// the same word as a **negative** `StackSlotRef(off)` read as `*(rbp + off)`,
/// so the verifier compares `-off` against this list. Getting that sign
/// convention wrong in either direction is exactly the class of bug this type
/// exists to catch, so it is stated once, here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OopCoverage {
    /// Positive `[rbp - off]` frame-slot offsets holding live references.
    pub frame_slot_offsets: Vec<i16>,
    /// GPR numbers holding live references. Empty for the IR lowerer, which
    /// keeps every live value in a frame slot.
    pub registers: Vec<u8>,
    /// Mirror of `OopMapEntry::moving_young_coverage_complete`. `false` means
    /// the collector must fall back to a non-moving cycle for this frame
    /// (`mark_moving_young_coverage_incomplete_because`), so a reference the
    /// deopt map names but the oop map omits is *tolerated* — it cannot go
    /// stale if nothing moves.
    pub moving_young_coverage_complete: bool,
}

impl OopCoverage {
    /// Coverage claiming completeness over `offsets` (positive `[rbp - off]`).
    pub fn complete(offsets: impl IntoIterator<Item = i16>) -> Self {
        Self {
            frame_slot_offsets: offsets.into_iter().collect(),
            registers: Vec::new(),
            moving_young_coverage_complete: true,
        }
    }

    /// Does the map cover the word at `[rbp - off]`?
    pub fn covers_frame_slot(&self, off: i32) -> bool {
        i16::try_from(off).is_ok_and(|o| self.frame_slot_offsets.contains(&o))
    }

    /// Does the map cover GPR `reg`?
    pub fn covers_register(&self, reg: u8) -> bool {
        self.registers.contains(&reg)
    }
}

/// Cap on how many violations one report lists, mirroring
/// `ir_verify::MAX_REPORTED_VIOLATIONS`. The count is always exact; only the
/// rendered list is truncated.
const MAX_REPORTED_DEOPT_VIOLATIONS: usize = 20;

/// Install-time checker for emitted deopt metadata.
///
/// Built with what only the compiler knows (per-method bytecode limits, the oop
/// maps it just emitted, the nodes its optimizer retired) and then run over the
/// `DeoptimizationPoint`s about to be installed. Every lane is opt-in through
/// the presence of the corresponding data, so a caller that can supply only
/// some of it still gets the checks that data supports rather than nothing:
///
/// * **scope lane** — active once any [`MethodFrameLimits`] is registered.
///   Checks bci ranges and local/stack counts per scope, including inlined
///   caller scopes.
/// * **oop-map agreement lane** — active once any [`OopCoverage`] is
///   registered, or unconditionally with [`Self::requiring_oop_map`].
/// * **removed-node lane** — active once retired node ids are registered.
/// * **structural lane** — always on: virtual-object definition/reference
///   integrity, field counts, register-file bounds, monitor balance, and the
///   ascending-`native_offset` ordering an emitter that walks forward implies
///   (see [`DeoptMetadataError::DeoptPointsUnsorted`] for why that is a
///   producer check and not a search precondition).
///
/// There is exactly ONE entry point, [`Self::violations`], and that is now
/// deliberate. A second front door over an interned representation
/// (`violations_interned`) was retired with that representation on 2026-09-21:
/// two checkers that were supposed to agree, only one of which anything walked,
/// is a worse defect than the one representation it saved.
#[derive(Debug, Default)]
pub struct DeoptVerifier {
    methods: FxHashMap<String, MethodFrameLimits>,
    oop_coverage: FxHashMap<u32, OopCoverage>,
    removed_nodes: FxHashSet<u32>,
    materializable_nodes: FxHashSet<u32>,
    require_oop_map: bool,
}

include!("deopt/metadata_verifier.rs");

#[cfg(test)]
mod deopt_metadata_tests;

/// The soundness lanes added by the deopt-metadata audit
/// (`deopt-metadata-audit.md`). Each test fails against the verifier
/// as it stood before that audit: every construct below passed verification.
#[cfg(test)]
mod deopt_metadata_soundness_tests;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Tests: what a resumable owned frame state is
// ---------------------------------------------------------------------------
//
// This module used to be `frame_state_interning_tests` and was ~880 lines long.
// The interned representation it exercised was retired on 2026-09-21 (see the
// note above `MAX_SCOPE_CHAIN`); what is left is the part that was never about
// interning — the owned resumability predicates and the slot-naming refusals.

#[cfg(test)]
mod frame_state_resumability_tests;

#[cfg(test)]
mod despec_registry_shape_tests;

// ---------------------------------------------------------------------------
// OSR entries a handler can reach (round 11 wave 11, lane irexc)
// ---------------------------------------------------------------------------

/// The bytecode pcs control can reach, over NORMAL edges, once one of the
/// method's exception handlers has been entered: every handler entry in
/// `handler_pcs` plus everything reachable from one. Sorted, distinct.
/// `None` when `code` does not verify or a handler pc is not an instruction
/// start — a caller must then treat EVERY pc as reached (fail closed).
///
/// # Why an OSR door needs it
///
/// An optimizing body's graph does not contain the method's handlers: the
/// builder walks normally reachable code only (`ir.rs`, STUB-S8), and a throw
/// leaves the body. Every fact the optimizer derives at a loop header — a φ
/// collapsed to a constant, a range, a type, a null-ness — therefore holds on
/// the paths WITHOUT a handler. For a method-entry body that is exact: an exit
/// leaves the compiled frame for good. An OSR entry at header `H` is exact too,
/// as long as every interpreter state that can arrive at `H` came along such
/// paths. Once a handler of this frame has run in the interpreter, that is no
/// longer true, and re-entering the body at a header the handler reaches
/// makes the compiled code believe its header facts about values the handler
/// path produced
/// (`r11w10-orch-osr-exc-exits-loses-handler-updated-locals-FIXED-20260924.md`:
/// `caught` was `φ(0, itself)`, folded to `0`, and every re-entry after the
/// interpreter's `caught++` read 0 again).
///
/// The handler does not have to write a local for this to bite: a local the
/// TRY body wrote before the throwing call reaches the header with the
/// throw-time value, which no graph path produces either. So the set that
/// matters is "headers a handler can reach", not "locals a handler writes".
///
/// Every handler is a root, so an exception raised inside handler code (an
/// exception edge to another handler) is covered without modelling exception
/// edges. `jsr` is followed to its target and falls through to the next pc
/// (`VerifiedCode::successors`), which also stands for the `ret` back to it.
pub fn pcs_reachable_from_handlers(code: &[u8], handler_pcs: &[u32]) -> Option<Vec<u32>> {
    if handler_pcs.is_empty() {
        return Some(Vec::new());
    }
    let verified = cratonvm_reader::verified_code(code).ok()?;
    let mut seen: FxHashSet<usize> = FxHashSet::default();
    let mut work: Vec<usize> = Vec::new();
    for &handler in handler_pcs {
        // Widening: a bytecode pc is u16.
        let pc = handler as usize;
        if !verified.is_instruction_start(pc) {
            return None;
        }
        if seen.insert(pc) {
            work.push(pc);
        }
    }
    while let Some(pc) = work.pop() {
        for succ in verified.successors(pc) {
            if seen.insert(succ) {
                work.push(succ);
            }
        }
    }
    let mut out: Vec<u32> = seen
        .into_iter()
        .filter_map(|pc| u32::try_from(pc).ok())
        .collect();
    out.sort_unstable();
    Some(out)
}

/// Is `pc` in [`pcs_reachable_from_handlers`]' answer? `None` (unanalysable)
/// answers `true`: fail closed.
pub fn pc_reachable_from_handlers(reachable: Option<&[u32]>, pc: usize) -> bool {
    match (reachable, u32::try_from(pc)) {
        (Some(pcs), Ok(pc)) => pcs.binary_search(&pc).is_ok(),
        _ => true,
    }
}

#[cfg(test)]
mod r11w11_irexc_handler_reach_tests;



/// Round 12 wave 4 (lane replay2): the single-pass form of the replay
/// post-check, and the per-point predicate both tiers now share.
#[cfg(test)]
mod r12w4_replay2_tests;

/// Round 12 wave 7 (lane replay4): the synchronized-with-virtuals refusal is
/// one predicate read by the VM's frame builder and by the compile-time replay
/// check, so the two agree
/// (`r12w6-hunter4-synchronized-method-with-virtuals-reruns-from-entry`).
#[cfg(test)]
mod r12w7_replay4_tests;



/// Round 13 wave 5 (lane resume): an inlined caller chain that names a
/// scalar-replaced object (`r13w3-framestate-vm-chain-resume-gaps` item 1).
/// Virtual-object ids are CHAIN-wide: the producer resolves every scope with
/// one `emitted` set, so an object two scopes name is defined in one and
/// referenced from the other.
#[cfg(test)]
mod r13w5_resume_chain_virtual_tests;





#[cfg(test)]
mod r16_chain_forced_mark_tests;





/// The for-each residual round (2026-10-06): which frames the in-place OSR
/// exit transfer may re-allocate scalar-replaced objects for.
#[cfg(test)]
mod fer_osr_in_place_virtuals_tests;

/// JIT round 19 wave 3 (lane splice3, SPLICE2-2): the expected-throw ids, the
/// opcode rule and the registry's exact listing.
#[cfg(test)]
mod r19_splice3_expected_throw_tests;



/// `(frame base, trapping body, bci)` of the entry this thread last popped
/// from a deopt stash, by any take (interpreter round i6 wave 123, lane L2):
/// what the VM's frame build matches a debugger's pending write to a compiled
/// activation by, since not every sink takes with the point. The frame base
/// is 0 for an entry no framed trampoline pushed; the body is `None` when the
/// trampoline did not name it or it has been reclaimed. A caller compares
/// the bci with the frame it is building, so an older take cannot be taken
/// for this one.
///
/// Test-only since JIT round 22 wave 7 (lane review): the VM reads the record
/// through [`take_last_taken_frame`] (wave 124), which clears it.
#[cfg(test)]
pub fn last_taken_frame() -> (u64, Option<std::sync::Arc<crate::CompiledMethod>>, u32) {
    TAKEN_FORCED_MARK.with(|c| {
        let taken = c.borrow();
        let (rbp, body, bci) = &taken.last_frame;
        (*rbp, body.as_ref().and_then(std::sync::Weak::upgrade), *bci)
    })
}

/// The frame base ([`StashEntry::frame_rbp`]) of the innermost ordinary
/// stashed frame, without clearing the stash -- the base twin of
/// [`peek_last_deopt_point`]. 0 when nothing is stashed or no framed
/// trampoline pushed it. Only ever compared, never dereferenced: the frame it
/// named has been torn down by the time a sink reads it.
///
/// Interpreter round i6 wave 123, lane L2 (slice 4 of stage 3b of
/// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`): the
/// key a debugger's pending write to a compiled activation is matched by, with
/// the body and the exit bci, when the activation's post-call exit stashes it.
///
/// Test-only since JIT round 22 wave 7 (lane review), with [`taken_frame_rbp`]:
/// the VM's match reads [`take_last_taken_frame`] instead.
#[cfg(test)]
pub fn peek_last_deopt_frame_rbp() -> u64 {
    LAST_DEOPT.with(|c| c.borrow().last().map_or(0, |e| e.frame_rbp))
}

/// The frame base of the entry this thread last took from the ordinary stash
/// WITH its point ([`take_last_deopt_with_point`] and its family), when that
/// entry's point is `point_addr` -- the take twin of
/// [`peek_last_deopt_frame_rbp`], for a sink that matches after it took. 0
/// for point 0, another point, or an entry no framed trampoline pushed.
/// Interpreter round i6 wave 123, lane L2. Test-only since JIT round 22 wave 7.
#[cfg(test)]
pub fn taken_frame_rbp(point_addr: usize) -> u64 {
    taken_frame_rbp_inner(point_addr)
}

#[cfg(test)]
fn taken_frame_rbp_inner(point_addr: usize) -> u64 {
    if point_addr == 0 {
        return 0;
    }
    TAKEN_FORCED_MARK.with(|c| {
        let taken = c.borrow();
        if taken.point == point_addr {
            taken.frame_rbp
        } else {
            0
        }
    })
}

/// JIT round 25 (item 4 residual 1): the spliced compactness guard's chain id
/// is its own -- per scope and bci, distinct from the predicate and flat
/// ids -- and only it (or the wildcard) withdraws that guard at the call site.
#[cfg(test)]
mod r25_item4_chain_compact_id_tests;

/// ARGWIN21-4: the owned-arguments caller rule admits a straight-line prefix
/// whose only stores fill arrays it allocated, and nothing else new.
#[cfg(test)]
mod argwin21_4_fresh_store_tests;



/// JIT round 21 lane stress (FUZZ21-3): the exit-side detector of a poisoned
/// dead home. These build frames by hand; the abort path is not exercised.
#[cfg(test)]
mod r21_stress_dead_home_poison_tests;



/// j27 wave 3 (lane BACKLOG, CN16-3): the planner's sync-method chain gate.
#[cfg(test)]
mod j27_backlog_sync_chain_gate_tests;
