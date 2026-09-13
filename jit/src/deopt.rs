// SPDX-License-Identifier: Apache-2.0
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

use std::{
    collections::hash_map::DefaultHasher,
    fmt,
    hash::{Hash, Hasher},
    mem,
    sync::Arc,
};

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
/// (The VM's *resume* sinks are a separate, stricter gate: they refuse any
/// `ReconstructedFrame` with a non-empty `caller_frames` outright —
/// `vm/src/runtime/interpreter.rs:13393`, `:13616`, `:13959` — so an inlined
/// chain currently never resumes at all. This predicate is what decides
/// whether such a chain gets compiled in the first place.)
///
/// Behaviour-preserving today: every producer sets `caller: None`, and a
/// one-scope chain is exactly the old predicate. The interned counterparts are
/// [`FrameStateInterner::is_resumable`] (deliberately scope-local — it answers
/// "is *this* scope clean") and [`FrameStateInterner::chain_is_resumable`],
/// which is the handle-side equivalent of this function.
///
/// The walk is bounded by [`MAX_SCOPE_CHAIN`]: a chain longer than that is
/// treated as unresumable rather than walked further, because a chain that deep
/// is a metadata defect and refusing costs only a whole-method re-run.
/// Which slot makes [`frame_state_is_resumable`] answer `false` — `"local 3
/// (Unsupported)"`, `"stack 1 (MaterializationRequired)"`, `depth`-prefixed
/// when the offender is in an inlined caller scope. `None` iff the state is
/// resumable.
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
/// 9 is HotSpot's own inlining depth limit. The IR-side
/// `ir::MAX_INLINE_SCOPE_DEPTH` (64) and `MAX_SCOPE_CHAIN` (256) bound
/// different things — what may be recorded, and what may be walked.
pub const MAX_OSR_INLINE_RESUME_DEPTH: usize = 9;

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

/// Every machine register `fs` names as the home of a value — GPRs as
/// `(n, false)`, XMMs as `(n, true)` — across the whole caller chain.
///
/// The reason-9 exceptional-frame stub reconstructs those homes by spilling the
/// live register file AT THE STUB and indexing it, so anything that runs
/// between the trapping instruction and that stub has to preserve every
/// register listed here. Compiled local handlers put a helper `CALL` on exactly
/// that edge, and this is how that emitter proves the call cannot disturb a
/// frame it may still have to fall through to.
pub fn frame_state_register_homes(fs: &FrameState) -> Vec<(u8, bool)> {
    let mut out = Vec::new();
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        for v in f.locals.iter().chain(f.stack.iter()) {
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
    let mut scope = Some(fs);
    let mut depth = 0usize;
    while let Some(f) = scope {
        for (i, v) in f.locals.iter().enumerate() {
            if value_blocks_resume(v) {
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

pub fn first_unresumable_slot(fs: &FrameState) -> Option<String> {
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
            if value_blocks_resume(v) {
                return Some(at(depth, "local", i, v));
            }
        }
        for (i, v) in f.stack.iter().enumerate() {
            if value_blocks_resume(v) {
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
            state.field_values.iter().any(contains_materialization_required)
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

/// Error returned when scalar-replaced objects cannot be materialized safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualObjectMaterializationError {
    /// The frame contains virtual objects, but this crate does not have the
    /// live VM heap/allocator required to turn them into real object refs.
    GcMaterializerUnavailable { virtual_objects: usize },
}

impl fmt::Display for VirtualObjectMaterializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GcMaterializerUnavailable { virtual_objects } => write!(
                f,
                "GC-backed virtual object materializer unavailable for {virtual_objects} object(s)"
            ),
        }
    }
}

impl std::error::Error for VirtualObjectMaterializationError {}

pub type VirtualObjectMaterializationResult =
    Result<Vec<(usize, u64)>, VirtualObjectMaterializationError>;

// ---------------------------------------------------------------------------
// Per-bci de-speculation registry (deopt-osr Step 9 follow-up c)
// ---------------------------------------------------------------------------

/// One VM's set of `(method_key, bci)` speculation sites that have deopted past
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
/// The data structure and its locking discipline are unchanged: one
/// `std::sync::RwLock` over an `FxHashSet`, a poisoned lock reads as "not
/// de-spec'd". Keyed by the same `"<class>.<method>:<descriptor>"` string the
/// deopt log / `method_epochs` use.
#[derive(Debug, Default)]
pub struct DespecRegistry {
    set: std::sync::RwLock<FxHashSet<(String, u32)>>,
}

impl DespecRegistry {
    /// An empty registry: nothing is de-spec'd.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `(method_key, bci)` as a failed speculation site that must not be
    /// re-speculated. Idempotent.
    pub fn insert(&self, method_key: &str, bci: u32) {
        if let Ok(mut s) = self.set.write() {
            s.insert((method_key.to_string(), bci));
        }
    }

    /// `true` if `(method_key, bci)` was recorded as a failed speculation site.
    /// Consulted by the compiler at speculative-guard emission. An empty
    /// `method_key` never matches (the `x64::compile()` legacy/test wrapper
    /// passes `""`).
    ///
    /// Fast path: when the registry is empty (nothing ever de-spec'd) this
    /// returns `false` after a cheap `is_empty` check, WITHOUT the
    /// `method_key.to_string()` lookup allocation, so consulting it per
    /// speculative guard during normal compilation is allocation-free.
    pub fn contains(&self, method_key: &str, bci: u32) -> bool {
        if method_key.is_empty() {
            return false;
        }
        let set = match self.set.read() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if set.is_empty() {
            return false;
        }
        set.contains(&(method_key.to_string(), bci))
    }

    /// Number of recorded de-spec sites for `method_key` (diagnostics / tests).
    pub fn count_for(&self, method_key: &str) -> usize {
        self.set
            .read()
            .map(|s| s.iter().filter(|(m, _)| m == method_key).count())
            .unwrap_or(0)
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
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_DESPEC_SPARE_FACTOR")
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
}

impl DeoptEpochGuard {
    /// A fresh, unstamped guard (null cell, epoch 0) — the in-entry check is a
    /// no-op until the VM stamps it.
    pub fn new() -> Self {
        Self {
            creation_epoch: std::sync::atomic::AtomicU64::new(0),
            live_epoch_cell: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
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
    /// that did return — can now say so, and that
    /// [`FrameStateInterner::materialize_point`] can round-trip it instead of
    /// dropping it.
    pub semantics: ResumeSemantics,
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

/// Tracks deopt history per method and drives adaptive recompilation.
/// T10.9.B: FxHashMap — keyed on internal method names from loaded class files.
pub struct DeoptimizationLog {
    history: FxHashMap<String, Vec<DeoptEvent>>,
    total_deopts: u64,
    max_deopts_per_method: u32,
}

impl DeoptimizationLog {
    /// Create a new log with default threshold (20 deopts before giving up).
    pub fn new() -> Self {
        Self {
            history: FxHashMap::default(),
            total_deopts: 0,
            max_deopts_per_method: 20,
        }
    }

    /// Create a new log with a custom threshold.
    pub fn new_with_threshold(max_deopts: u32) -> Self {
        Self {
            history: FxHashMap::default(),
            total_deopts: 0,
            max_deopts_per_method: max_deopts,
        }
    }

    /// Record a deoptimization event for a method.
    ///
    /// PERF-P5 (T10.9.C): avoid the per-call `String::from(method)` that
    /// `HashMap::entry` requires by trying a `get_mut` first. The owned-key
    /// allocation only happens on the first deopt for a given method name
    /// (the miss path). For hot methods that deopt repeatedly this turns
    /// every call after the first into a single hash + push.
    ///
    /// TODO(PERF-P5): take `&Arc<str>` once upstream call sites in
    /// `vm/src/runtime/jit_integration.rs` and `vm/src/vm/vm_init.rs`
    /// thread the standard `Arc<str>` method-name carrier through.
    pub fn record_deopt(&mut self, method: &str, event: DeoptEvent) {
        self.total_deopts += 1;
        if let Some(events) = self.history.get_mut(method) {
            events.push(event);
            return;
        }
        self.history.insert(method.to_string(), vec![event]);
    }

    /// Number of deopts recorded for `method`.
    pub fn deopt_count(&self, method: &str) -> usize {
        self.history.get(method).map_or(0, |v| v.len())
    }

    /// Number of deopts recorded for `method` at the specific bytecode index
    /// `bci`. Drives per-bci de-speculation (Step 9 follow-up c): a single
    /// speculation site that fails repeatedly is de-spec'd on its own (its guard
    /// suppressed on the next compile) rather than escalating to a whole-method
    /// blacklist once the *aggregate* per-method count crosses the threshold.
    pub fn deopt_count_at_bci(&self, method: &str, bci: u32) -> usize {
        self.history
            .get(method)
            .map_or(0, |v| v.iter().filter(|e| e.bci == bci).count())
    }

    /// Returns `true` when the method has exceeded the deopt threshold.
    pub fn should_give_up(&self, method: &str) -> bool {
        self.deopt_count(method) >= self.max_deopts_per_method as usize
    }

    /// The deopt reason that has occurred most often for `method`.
    pub fn most_common_reason(&self, method: &str) -> Option<DeoptReason> {
        let events = self.history.get(method)?;
        let mut counts: FxHashMap<DeoptReason, usize> = FxHashMap::default();
        for e in events {
            *counts.entry(e.reason).or_default() += 1;
        }
        counts.into_iter().max_by_key(|&(_, c)| c).map(|(r, _)| r)
    }

    /// Get the event history for a method (empty slice if none).
    pub fn history(&self, method: &str) -> &[DeoptEvent] {
        self.history.get(method).map_or(&[], |v| v.as_slice())
    }

    /// Total deopts across all methods.
    pub fn total_deopts(&self) -> u64 {
        self.total_deopts
    }

    /// Clear history for a method (e.g., after successful recompilation).
    pub fn clear_history(&mut self, method: &str) {
        if let Some(events) = self.history.remove(method) {
            // Do not decrement total_deopts — it is a lifetime counter.
            let _ = events;
        }
    }

    /// Release deoptimization history for methods owned by an unloaded class.
    pub fn clear_class(&mut self, class_name: &str) {
        let prefix = format!("{class_name}.");
        self.history
            .retain(|method, _| !method.starts_with(&prefix));
    }

    /// Recommend a deopt action based on current history and the triggering reason.
    ///
    /// The `reason` parameter influences the recommended action:
    /// - `ReceiverTypeChanged`, `ClassCheck` → aggressive recompile (the type profile changed)
    /// - `NotCompiled`, `UnreachedCode` → give up immediately
    /// - `SpeculationFailed`, `ClassLoading` → recompile with updated assumptions
    /// - Other reasons use the count-based policy:
    ///   - First occurrence              → Reinterpret
    ///   - 2..threshold/2                → RecompileAndReinterpret
    ///   - threshold/2..threshold        → MakeNotEntrant
    ///   - >= threshold                  → MakeNotCompilable
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
        {
            return action;
        }
        crate::metrics::note_despec_escalation_spared();
        DeoptAction::RecompileAndReinterpret
    }

    pub fn recommend_action(&self, method: &str, reason: DeoptReason) -> DeoptAction {
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
            DeoptReason::SpeculationFailed | DeoptReason::ClassLoading => {
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
            // `Response.toAbsolute()`, docs/known-issues/tomcat-08-07/silent-
            // hang-no-signature-cluster.md) get evicted and eagerly recompiled
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
#[derive(Debug, Clone)]
pub struct ReconstructedFrame {
    pub method_key: String,
    pub bci: u32,
    pub locals: Vec<FrameValue>,
    pub stack: Vec<FrameValue>,
    pub monitors: Vec<MonitorInfo>,
    /// Outer frames when the deopt point was inside inlined code.
    pub caller_frames: Vec<ReconstructedFrame>,
}

/// Reconstruct an interpreter frame from a `DeoptimizationPoint`.
///
/// PERF-P5 (T10.9.C): every clone here is necessary today — the
/// `DeoptimizationPoint` is embedded in compiled code metadata and may be
/// triggered again by another thread or another deopt at the same site, so
/// we cannot `mem::take` out of it. The slow-path nature of deopt
/// (interpreter resume + recompile decision dominates) makes these clones
/// acceptable for now.
///
/// TODO(PERF-P5): to truly eliminate these allocations the upstream type
/// `FrameState` would need `locals: Arc<[FrameValue]>`,
/// `stack: Arc<[FrameValue]>`, `monitors: Arc<[MonitorInfo]>`, and
/// `method_key: Arc<str>`. Then reconstruction degenerates to a refcount
/// bump per slot. That requires coordinated changes to
/// `vm/src/runtime/jit_integration.rs` and the IR emitter that builds
/// `FrameState`, which is outside the scope of this patch.
pub fn reconstruct_frame(deopt: &DeoptimizationPoint) -> ReconstructedFrame {
    fn unwind(state: &FrameState) -> (ReconstructedFrame, Vec<ReconstructedFrame>) {
        let frame = ReconstructedFrame {
            method_key: state.method_key.clone(),
            bci: state.bci,
            locals: state.locals.clone(),
            stack: state.stack.clone(),
            monitors: state.monitors.clone(),
            caller_frames: Vec::new(),
        };

        // Pre-size the caller chain in one pass so the inlining-depth Vec
        // grows once instead of doubling.
        let mut depth = 0usize;
        {
            let mut probe = state.caller.as_deref();
            while let Some(c) = probe {
                depth += 1;
                probe = c.caller.as_deref();
            }
        }
        let mut callers = Vec::with_capacity(depth);
        let mut next = state.caller.as_deref();
        while let Some(caller) = next {
            callers.push(ReconstructedFrame {
                method_key: caller.method_key.clone(),
                bci: caller.bci,
                locals: caller.locals.clone(),
                stack: caller.stack.clone(),
                monitors: caller.monitors.clone(),
                caller_frames: Vec::new(),
            });
            next = caller.caller.as_deref();
        }

        (frame, callers)
    }

    let (mut frame, callers) = unwind(&deopt.frame_state);
    frame.caller_frames = callers;
    frame
}

/// Reconstruct an interpreter frame by consuming a `DeoptimizationPoint`.
///
/// PERF-P5 (T10.9.C): when the caller owns the `DeoptimizationPoint` and
/// doesn't need it again (e.g. one-shot deopt where the compiled code is
/// being invalidated and the metadata can be dropped), use this variant
/// to `mem::take` the Vec fields instead of cloning them. The
/// reconstruction logic and shape are identical to `reconstruct_frame`.
pub fn reconstruct_frame_owned(mut deopt: DeoptimizationPoint) -> ReconstructedFrame {
    fn unwind(state: &mut FrameState) -> (ReconstructedFrame, Vec<ReconstructedFrame>) {
        let frame = ReconstructedFrame {
            method_key: mem::take(&mut state.method_key),
            bci: state.bci,
            locals: mem::take(&mut state.locals),
            stack: mem::take(&mut state.stack),
            monitors: mem::take(&mut state.monitors),
            caller_frames: Vec::new(),
        };

        // Count depth without holding a mutable borrow into the chain.
        let mut depth = 0usize;
        {
            let mut probe = state.caller.as_deref();
            while let Some(c) = probe {
                depth += 1;
                probe = c.caller.as_deref();
            }
        }
        let mut callers = Vec::with_capacity(depth);
        let mut next = state.caller.take();
        while let Some(mut caller) = next {
            let following = caller.caller.take();
            callers.push(ReconstructedFrame {
                method_key: mem::take(&mut caller.method_key),
                bci: caller.bci,
                locals: mem::take(&mut caller.locals),
                stack: mem::take(&mut caller.stack),
                monitors: mem::take(&mut caller.monitors),
                caller_frames: Vec::new(),
            });
            next = following;
        }

        (frame, callers)
    }

    let (mut frame, callers) = unwind(&mut deopt.frame_state);
    frame.caller_frames = callers;
    frame
}

/// Identify which local/stack slots hold scalar-replaced (virtual) objects
/// that would need to be re-materialized on the heap during a deopt, and
/// return either placeholder `(index, heap_address)` pairs in tests or a
/// structured error in production builds.
///
/// # ⚠ NOT WIRED TO A LIVE DEOPT PATH — placeholder addresses are NOT real objects
///
/// This function is **diagnostic/skeleton only**. It is currently called
/// exclusively from the unit test `materialize_virtual_objects_count`,
/// which validates slot-index extraction and address distinctness — it does
/// NOT exercise a real deopt. No VM code path invokes it (verified: no
/// references outside this module's tests). In particular the in-progress
/// live deopt machinery (the `FrameState`/`DeoptimizationPoint` plumbing that
/// `reconstruct_frame{,_owned}` feed) does **not** call this.
///
/// In test builds, returned addresses are FAKE: monotonically increasing
/// placeholders starting at `0x1000_0000`, stepping by `0x100`.
/// They do **not** point at GC-allocated, header-initialized,
/// field-populated heap objects. Treating
/// a returned address as a live object reference is **memory-unsafe** — it
/// would hand the interpreter (and then the GC, on its next root scan) a
/// dangling pointer into an unmapped/foreign region, almost certainly
/// crashing or corrupting the heap.
///
/// ## What the real fix requires (GC-backed materialization)
///
/// A correct implementation cannot run against a borrowed `&FrameState`
/// alone — re-materialization is a heap-mutating, GC-coordinated operation.
/// It must, for each `FrameValue::VirtualObject(state)`:
///   1. Allocate an object of `state.class_id` via the live allocator/TLAB
///      (which may trigger a GC; the surrounding deopt frame must already be
///      a valid GC root set so the half-built object survives).
///   2. Write the real object header (class id, mark word, etc.).
///   3. Recursively materialize/store each `state.field_values[i]`, resolving
///      nested `VirtualObject`s and patching any back-references (cyclic
///      scalar-replaced graphs).
///   4. Return the *real* heap address from the allocator.
/// This needs a handle to the VM heap/allocator threaded in from the deopt
/// path, so it belongs with the live-deopt feature work in the VM crate, not
/// here. Until then this stays a placeholder.
///
/// # Guard
///
/// To make sure the fake addresses can never be silently consumed by real
/// VM execution, the placeholder implementation is hard-gated to test builds.
/// In a non-test build, frames that contain virtual objects return
/// [`VirtualObjectMaterializationError::GcMaterializerUnavailable`] rather than
/// minting bogus object references.
pub fn materialize_virtual_objects(frame: &FrameState) -> VirtualObjectMaterializationResult {
    let virtual_objects = count_virtual_objects(frame);
    if virtual_objects == 0 {
        return Ok(Vec::new());
    }
    materialize_virtual_objects_impl(frame, virtual_objects)
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
fn materialize_virtual_objects_impl(
    frame: &FrameState,
    _virtual_objects: usize,
) -> VirtualObjectMaterializationResult {
    let mut result = Vec::new();
    let mut next_addr: u64 = 0x1000_0000;

    fn collect(values: &[FrameValue], result: &mut Vec<(usize, u64)>, next_addr: &mut u64) {
        for (i, v) in values.iter().enumerate() {
            if let FrameValue::VirtualObject(_) = v {
                result.push((i, *next_addr));
                *next_addr += 0x100;
            }
        }
    }

    collect(&frame.locals, &mut result, &mut next_addr);
    collect(&frame.stack, &mut result, &mut next_addr);

    Ok(result)
}

/// Production helper for [`materialize_virtual_objects`].
///
/// The JIT crate does not own a live heap/allocator, so it cannot safely turn
/// scalar-replaced values into real object references. Return an explicit error
/// and let the VM-side `runtime::deopt_materialize` path handle real frames.
#[cfg(not(test))]
fn materialize_virtual_objects_impl(
    _frame: &FrameState,
    virtual_objects: usize,
) -> VirtualObjectMaterializationResult {
    Err(VirtualObjectMaterializationError::GcMaterializerUnavailable { virtual_objects })
}

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
        caller_frames: Vec::new(),
    }
}

/// Reconstruct a precise interpreter frame from a `DeoptimizationPoint` and
/// the live machine state captured at the trapping site.
///
/// This is the machine-state-aware sibling of [`reconstruct_frame`]: where
/// that one assumes every `FrameValue` is already a resolved constant, this
/// one reads `Register`/`StackSlot` values out of the spilled register file
/// and the native stack. The inlined caller chain (`FrameState.caller`) is
/// flattened into `caller_frames`, each resolved against the same machine
/// state (inlined frames share the physical frame).
///
/// Virtual (scalar-replaced) objects are NOT materialized here — that needs
/// GC-backed allocation and is Phase B (`materialize_virtual_objects`); they
/// pass through as `VirtualObject` for a later pass to realize.
pub fn reconstruct_frame_from_machine_state(
    deopt: &DeoptimizationPoint,
    regs: &SavedRegisters,
    rbp: u64,
) -> ReconstructedFrame {
    let mut frame = resolve_frame_state_machine(&deopt.frame_state, regs, rbp);
    let mut callers = Vec::new();
    let mut next = deopt.frame_state.caller.as_deref();
    while let Some(c) = next {
        callers.push(resolve_frame_state_machine(c, regs, rbp));
        next = c.caller.as_deref();
    }
    frame.caller_frames = callers;
    frame
}

thread_local! {
    /// The most recent frame reconstructed by [`ir_deopt_entry`]. The IR-path
    /// deopt trampoline is not yet wired into VM interpreter dispatch, so the
    /// reconstructed frame is stashed here for the test harness / caller to
    /// consume via [`take_last_deopt`] rather than being resumed directly.
    static LAST_DEOPT: std::cell::RefCell<Option<ReconstructedFrame>> =
        const { std::cell::RefCell::new(None) };
}

/// Take (and clear) the frame most recently reconstructed by a deopt.
pub fn take_last_deopt() -> Option<ReconstructedFrame> {
    LAST_DEOPT.with(|c| c.borrow_mut().take())
}

thread_local! {
    /// The frame published by a [`DeoptReason::PendingException`] stub — a
    /// method whose pending Java exception must be routed through its own
    /// exception table with precise locals.
    ///
    /// Deliberately NOT `LAST_DEOPT`: every consumer of that stash treats it as
    /// "resume this method at `bci`", which for an exceptional frame executes
    /// past a call that never returned. Keeping it separate also keeps
    /// `has_last_deopt()` (which `jit_dispatch_threw` consults to disambiguate a
    /// legitimate `Long.MIN_VALUE` return) blind to it, so an exceptional frame
    /// nobody claims cannot make an unrelated later call site bail.
    static LAST_EXCEPTIONAL: std::cell::RefCell<Option<ReconstructedFrame>> =
        const { std::cell::RefCell::new(None) };
}

/// Take (and clear) the pending-exception frame, if one was published by the
/// compiled method that just returned the deopt sentinel.
pub fn take_exceptional_frame() -> Option<ReconstructedFrame> {
    LAST_EXCEPTIONAL.with(|c| c.borrow_mut().take())
}

/// Put a taken pending-exception frame back (the take-inspect-restash pattern,
/// for a sink that discovers the frame is not its own to drop).
pub fn restash_exceptional_frame(frame: ReconstructedFrame) {
    LAST_EXCEPTIONAL.with(|c| *c.borrow_mut() = Some(frame));
}

/// Drop any pending-exception frame.
///
/// Used by the dispatch helper when it decides to re-execute a callee in the
/// interpreter: the re-run regenerates and routes the exception itself, so the
/// frame the abandoned compiled attempt published is stale. Sinks that merely
/// pass an exception along must NOT call this — see
/// `interpreter::drop_own_exceptional_frame`.
pub fn clear_exceptional_frame() {
    LAST_EXCEPTIONAL.with(|c| {
        let _ = c.borrow_mut().take();
    });
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
    LAST_DEOPT.with(|c| c.borrow().is_some())
}

/// Peek the stashed frame's `(method_key, bci)` identity without clearing it.
///
/// Used by the dispatch-helper precise-resume arm
/// (`try_resume_trapped_callee`, vm/src/jit/helpers.rs) to decide — BEFORE
/// consuming the stash — whether the frame belongs to the compiled callee this
/// helper just invoked. A non-matching frame must stay stashed so it
/// propagates (with the sentinel) to the outer consumer that CAN attribute it.
pub fn peek_last_deopt_identity() -> Option<(String, u32)> {
    LAST_DEOPT.with(|c| c.borrow().as_ref().map(|f| (f.method_key.clone(), f.bci)))
}

/// Put a taken frame back (the take-inspect-restash pattern for consumers that
/// discover mid-flight they cannot handle it). Overwrites any newer stash —
/// callers only restash what they just took, with no interleaving stash
/// possible on the same thread.
pub fn restash_last_deopt(frame: ReconstructedFrame) {
    LAST_DEOPT.with(|c| *c.borrow_mut() = Some(frame));
}

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
fn with_both_stashes(mut visit: impl FnMut(&std::cell::RefCell<Option<ReconstructedFrame>>)) {
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
            if let Some(frame) = slot.as_ref() {
                frame.for_each_object_address(&mut *f);
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
            if let Some(frame) = slot.as_mut() {
                frame.for_each_object_address_mut(&mut *f);
            }
        }
        Err(_) => debug_assert!(
            false,
            "deopt stash borrowed during a GC remap: a stash accessor re-entered \
             the collector (see docs/jit/deopt-thread-local-roots.md)"
        ),
    });
}

/// How many live heap references the two stashes currently hold on this thread.
///
/// Diagnostic / wiring check only — it is NOT a root provider. Does not count
/// as a scan.
pub fn stashed_deopt_object_count() -> usize {
    let mut n = 0usize;
    with_both_stashes(|cell| {
        if let Ok(slot) = cell.try_borrow() {
            if let Some(frame) = slot.as_ref() {
                frame.for_each_object_address(&mut |_addr: u64| n += 1);
            }
        }
    });
    n
}

#[cfg(test)]
mod deopt_stash_root_tests {
    use super::*;

    /// Both stashes empty, whatever a previous test on this thread did.
    fn clear_stashes() {
        let _ = take_last_deopt();
        let _ = take_exceptional_frame();
    }

    fn scanned_addresses() -> Vec<u64> {
        let mut out = Vec::new();
        for_each_stashed_deopt_object(|a| out.push(a));
        out.sort_unstable();
        out
    }

    /// A frame that puts a distinct object address in EVERY container that can
    /// hold one, so a walk that forgets a family fails loudly rather than
    /// silently under-rooting:
    ///
    /// * `locals`                                    → `0x1000`
    /// * `stack`                                     → `0x2000`
    /// * `monitors[].object`                         → `0x3000`
    /// * a scalar-replaced object's `field_values`   → `0x4000`
    /// * the inlined `caller_frames` chain (locals)  → `0x5000`
    /// * a nested virtual object inside that field   → `0x6000`
    ///
    /// Interleaved with non-reference slots of every width, and with a null
    /// (`Object(0)`) slot, both of which must be left strictly alone.
    fn frame_with_one_object_per_container(base: u64) -> ReconstructedFrame {
        let nested = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 2,
            class_id: 9,
            num_fields: 1,
            field_values: vec![FrameValue::Object(base + 0x6000)],
        });
        ReconstructedFrame {
            method_key: "craton/probe/Stash.m:()V".to_string(),
            bci: 4,
            locals: vec![
                FrameValue::Object(base + 0x1000),
                FrameValue::Int(-7),
                FrameValue::Object(0),                  // null — never a root
                FrameValue::Long(base as i64 + 0x1000), // an int-typed lookalike
                FrameValue::Undefined,
            ],
            stack: vec![
                FrameValue::Double(0x4059_0000_0000_0000),
                FrameValue::Object(base + 0x2000),
                FrameValue::VirtualObject(VirtualObjectState {
                    array_element_type: None,
                    id: 1,
                    class_id: 8,
                    num_fields: 2,
                    field_values: vec![FrameValue::Object(base + 0x4000), nested],
                }),
                FrameValue::VirtualObjectRef(1),
            ],
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(base + 0x3000),
                lock_depth: 1,
                relock: true,
            }],
            caller_frames: vec![ReconstructedFrame {
                method_key: "craton/probe/Stash.caller:()V".to_string(),
                bci: 0,
                locals: vec![FrameValue::Object(base + 0x5000)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller_frames: Vec::new(),
            }],
        }
    }

    fn expected_addresses(base: u64) -> Vec<u64> {
        let mut v = vec![
            base + 0x1000,
            base + 0x2000,
            base + 0x3000,
            base + 0x4000,
            base + 0x5000,
            base + 0x6000,
        ];
        v.sort_unstable();
        v
    }

    /// Every container that can hold a raw heap address is reachable from the
    /// scan half. This is the test that fails if a new `FrameValue` variant or
    /// a new frame field starts carrying an address.
    #[test]
    fn every_object_slot_family_is_offered_to_a_root_scan() {
        clear_stashes();
        restash_last_deopt(frame_with_one_object_per_container(0));
        assert_eq!(scanned_addresses(), expected_addresses(0));
        clear_stashes();
    }

    /// The exceptional stash is a second, independent thread-local. It is the
    /// one with the shortest proven window (an allocating `sig.npe` /
    /// `sig.aioobe` / `sig.arithmetic` arm sits between its publication and its
    /// drain), so a scan that covered only `LAST_DEOPT` would close the wrong
    /// half.
    #[test]
    fn the_exceptional_stash_is_scanned_too() {
        clear_stashes();
        restash_exceptional_frame(frame_with_one_object_per_container(0));
        assert_eq!(scanned_addresses(), expected_addresses(0));
        clear_stashes();
    }

    /// Both stashes can be occupied at once — a deopt frame belonging to a
    /// callee standing while this method publishes an exceptional frame. The
    /// scan must yield the union, not whichever it happens to look at first.
    #[test]
    fn both_stashes_are_scanned_when_both_are_occupied() {
        clear_stashes();
        restash_last_deopt(frame_with_one_object_per_container(0));
        restash_exceptional_frame(frame_with_one_object_per_container(0x10_0000));
        let mut expected = expected_addresses(0);
        expected.extend(expected_addresses(0x10_0000));
        expected.sort_unstable();
        assert_eq!(scanned_addresses(), expected);
        assert_eq!(stashed_deopt_object_count(), 12);
        clear_stashes();
    }

    /// `Object(0)` is `null`. Offering it as a root would hand the collector a
    /// zero address to resolve; callers are entitled to assume every value they
    /// receive is a live reference.
    #[test]
    fn null_object_slots_are_not_offered_as_roots() {
        clear_stashes();
        restash_last_deopt(ReconstructedFrame {
            method_key: String::new(),
            bci: 0,
            locals: vec![FrameValue::Object(0), FrameValue::Object(0)],
            stack: vec![FrameValue::Object(0)],
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(0),
                lock_depth: 1,
                relock: true,
            }],
            caller_frames: Vec::new(),
        });
        assert!(scanned_addresses().is_empty());
        assert_eq!(stashed_deopt_object_count(), 0);
        clear_stashes();
    }

    #[test]
    fn nothing_is_offered_when_no_frame_is_stashed() {
        clear_stashes();
        assert!(scanned_addresses().is_empty());
        assert_eq!(stashed_deopt_object_count(), 0);
    }

    /// The real shape: a moving collection relocates every object the frame
    /// names, and the drained frame must read the POST-move addresses. A frame
    /// that kept its pre-move addresses hands the interpreter from-space
    /// pointers — the failure this whole mechanism exists to prevent.
    #[test]
    fn a_moving_collection_rewrites_every_stashed_object_slot() {
        clear_stashes();
        restash_last_deopt(frame_with_one_object_per_container(0));
        restash_exceptional_frame(frame_with_one_object_per_container(0x10_0000));

        // The scan half runs first, exactly as it does in a real collection.
        let scanned = scanned_addresses();
        assert_eq!(scanned.len(), 12);

        // Every scanned object moved by +0x8000_0000.
        const DELTA: u64 = 0x8000_0000;
        remap_stashed_deopt_objects(|a| Some(a + DELTA));

        let deopt = take_last_deopt().expect("still stashed");
        let exceptional = take_exceptional_frame().expect("still stashed");

        let mut after: Vec<u64> = Vec::new();
        deopt.for_each_object_address(&mut |a: u64| after.push(a));
        exceptional.for_each_object_address(&mut |a: u64| after.push(a));
        after.sort_unstable();
        let expected: Vec<u64> = scanned.iter().map(|a| a + DELTA).collect();
        assert_eq!(after, expected, "every slot family must follow the move");

        // Spot-check the two hardest containers by hand, so a walk that
        // "visits" them without writing back cannot pass.
        assert_eq!(deopt.monitors[0].object, FrameValue::Object(0x3000 + DELTA));
        assert_eq!(
            deopt.caller_frames[0].locals[0],
            FrameValue::Object(0x5000 + DELTA)
        );
        clear_stashes();
    }

    /// A pointer map that does not mention an object means it did not move.
    /// Perturbing such a slot would be a corruption of its own.
    #[test]
    fn remap_leaves_addresses_the_map_does_not_mention_alone() {
        clear_stashes();
        restash_last_deopt(frame_with_one_object_per_container(0));
        let _ = scanned_addresses();
        // Only the local at 0x1000 moved.
        remap_stashed_deopt_objects(|a| if a == 0x1000 { Some(0xABCD) } else { None });
        let frame = take_last_deopt().expect("still stashed");
        assert_eq!(frame.locals[0], FrameValue::Object(0xABCD));
        assert_eq!(frame.stack[1], FrameValue::Object(0x2000));
        assert_eq!(frame.monitors[0].object, FrameValue::Object(0x3000));
        assert_eq!(frame.caller_frames[0].locals[0], FrameValue::Object(0x5000));
        clear_stashes();
    }

    /// Nothing but an `Object` slot is an address. An `Int`/`Long`/`Float`/
    /// `Double` whose bits happen to look like a pointer, an unresolved
    /// `StackSlotRef` (a frame OFFSET, not an address), a `VirtualObjectRef` (an
    /// intra-frame id) and `Undefined` must all survive a remap untouched — a
    /// visitor that widened to "anything 64-bit" would silently rewrite live
    /// primitive data.
    #[test]
    fn non_reference_slots_are_never_offered_or_rewritten() {
        clear_stashes();
        let originals = vec![
            FrameValue::Int(0x1000),
            FrameValue::Long(0x1000),
            FrameValue::Float(0x1000),
            FrameValue::Double(0x1000),
            FrameValue::StackSlotRef(-16),
            FrameValue::RegisterRef(3),
            FrameValue::StackSlot(-8),
            FrameValue::VirtualObjectRef(1),
            FrameValue::Undefined,
            FrameValue::Unsupported,
            FrameValue::MaterializationRequired(EliminatedValue::unknown(
                EliminationCause::Unclassified,
            )),
        ];
        restash_last_deopt(ReconstructedFrame {
            method_key: String::new(),
            bci: 0,
            locals: originals.clone(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller_frames: Vec::new(),
        });
        assert!(scanned_addresses().is_empty());
        remap_stashed_deopt_objects(|_| Some(0xDEAD_BEEF));
        let frame = take_last_deopt().expect("still stashed");
        assert_eq!(frame.locals, originals);
        clear_stashes();
    }

    /// The visitors must not disturb the stash itself: a scan is a peek, and a
    /// remap rewrites in place. A consumer that runs after a collection must
    /// still find its frame, with its identity intact.
    #[test]
    fn scanning_and_remapping_leave_the_stash_in_place() {
        clear_stashes();
        restash_last_deopt(frame_with_one_object_per_container(0));
        assert!(has_last_deopt());
        let _ = scanned_addresses();
        assert!(has_last_deopt(), "a root scan must not consume the stash");
        remap_stashed_deopt_objects(|_| None);
        assert!(has_last_deopt(), "a remap must not consume the stash");
        assert_eq!(
            peek_last_deopt_identity(),
            Some(("craton/probe/Stash.m:()V".to_string(), 4))
        );
        clear_stashes();
    }

    /// A remap on a thread that never offered its stashes to a root scan is the
    /// half-wired state: the reference gets faithfully rewritten to a slot the
    /// collector was free to reclaim. Debug builds must refuse it. (An EMPTY
    /// stash is not half-wiring — there is nothing to keep alive — so the
    /// unscanned no-op below must stay quiet.)
    #[test]
    fn remapping_an_empty_stash_without_a_scan_is_not_an_error() {
        clear_stashes();
        remap_stashed_deopt_objects(|a| Some(a));
        assert_eq!(stashed_deopt_object_count(), 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn remapping_a_populated_stash_without_a_scan_trips_the_wiring_check() {
        // Deliberately on a FRESH thread rather than via `#[should_panic]`:
        // both the stashes and the scan counter are thread-local, and
        // `--test-threads=1` runs every test on one thread, where an earlier
        // test's scan would satisfy the check and make this one vacuous.
        let outcome = std::thread::spawn(|| {
            restash_last_deopt(frame_with_one_object_per_container(0));
            // No `for_each_stashed_deopt_object` — this is the half-wiring.
            remap_stashed_deopt_objects(|a| Some(a + 1));
        })
        .join();
        let payload = outcome.expect_err("a remap without a scan must be refused");
        let msg = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("half-wired"),
            "the panic must name the half-wiring, got: {msg}"
        );
    }
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
    // Checked, not assumed. The contract above says `point` is non-null, but a
    // deopt trampoline is the worst place in the VM to find out that a
    // contract was broken: dereferencing null here is UB inside a stub with a
    // half-torn-down frame. A null pointer instead stashes the identity-less
    // `bci == u32::MAX` re-run sentinel — the VM resume path rejects that bci
    // and re-runs the method in the interpreter — so the `i64::MIN` return can
    // never be mistaken for a legitimate `Long.MIN_VALUE` result.
    if point.is_null() {
        LAST_DEOPT.with(|c| {
            *c.borrow_mut() = Some(ReconstructedFrame {
                method_key: String::new(),
                bci: u32::MAX,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller_frames: Vec::new(),
            })
        });
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
    LAST_DEOPT.with(|c| *c.borrow_mut() = Some(frame));
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
/// stable, retained [`DeoptEpochGuard`] baked alongside the box. It is accepted
/// and ignored: the before-deref short-circuit it fed existed only for the
/// deleted `CRATONVM_JIT_FREE_CODE=1` mode (see the body). `epoch_guard` is null
/// on every production artifact anyway (the VM stamps it only under
/// `deopt_real_enabled()`).
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
    // was deleted with that mode. `epoch_guard` stays in the stub ABI.
    let _ = epoch_guard;
    if point.is_null() || regs.is_null() {
        return i64::MIN;
    }
    // SAFETY: contract documented above.
    let point = unsafe { &*point };
    // SAFETY: contract documented above; `regs` was checked non-null.
    let regs = unsafe { &*regs };
    let frame = reconstruct_frame_from_machine_state(point, regs, rbp);
    if point.reason == DeoptReason::PendingException {
        // Exceptional frames get their own stash — see `LAST_EXCEPTIONAL`.
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
            eprintln!(
                "[cratonvm-deopt] x64 exceptional frame at throw bci={} locals={:?}",
                point.bci, frame.locals,
            );
        }
        LAST_EXCEPTIONAL.with(|c| *c.borrow_mut() = Some(frame));
        return i64::MIN;
    }
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
        // Resume-side trace: confirms the frame-deopt trampoline fired and at
        // which bci/reason (deopt-osr Step 8 OSR-exit shows reason=OsrExit), with
        // the RESOLVED locals/stack so a wrong reconstruction is visible.
        eprintln!(
            "[cratonvm-deopt] x64 frame-deopt entry reason={:?} at bci={} locals={:?} stack={:?}",
            point.reason, point.bci, frame.locals, frame.stack,
        );
    }
    LAST_DEOPT.with(|c| *c.borrow_mut() = Some(frame));
    i64::MIN
}

#[cfg(test)]
mod x64_deopt_entry_tests {
    use super::*;

    /// The 3-arg entry resolves `FrameValue::Register(r)` against the PASSED
    /// register file (proving the in-stub spill + 3-arg wiring), where
    /// `ir_deopt_entry`'s default-zeros path would yield 0; constants pass
    /// through; and the frame is stashed for `take_last_deopt`.
    #[test]
    fn resolves_registers_against_passed_regfile_and_stashes() {
        let mut regs = SavedRegisters::default();
        regs.gpr[3] = 0xDEAD_BEEF; // architectural reg 3 (RBX) holds a live value
        let point = DeoptimizationPoint {
            native_offset: 0,
            bci: 7,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: String::new(),
                bci: 7,
                locals: vec![FrameValue::Register(3), FrameValue::Int(5)],
                stack: vec![FrameValue::Register(3)],
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        let _ = take_last_deopt(); // clear any prior stash
                                   // Null guard ⇒ no staleness check (the production / unstamped path).
        let r = x64_deopt_entry(&point, 0, &regs as *const SavedRegisters, std::ptr::null());
        assert_eq!(r, i64::MIN, "entry returns the deopt sentinel");

        let frame = take_last_deopt().expect("entry stashes a reconstructed frame");
        assert_eq!(frame.bci, 7);
        // Register(3) resolved to gpr[3]; Int passes through.
        assert_eq!(frame.locals[0], FrameValue::Int(0xDEAD_BEEF));
        assert_eq!(frame.locals[1], FrameValue::Int(5));
        assert_eq!(frame.stack[0], FrameValue::Int(0xDEAD_BEEF));
    }

    /// Null args are tolerated (return the sentinel, stash nothing).
    #[test]
    fn null_args_return_sentinel_without_stash() {
        let _ = take_last_deopt();
        let r = x64_deopt_entry(std::ptr::null(), 0, std::ptr::null(), std::ptr::null());
        assert_eq!(r, i64::MIN);
        assert!(take_last_deopt().is_none());
    }

    /// deopt-osr Step 9 follow-up (a), REVISED by the
    /// jit-invokedynamic-groovy-regression identity fix: a SUPERSEDED guard
    /// does not short-circuit. The trapping frame owns its artifact, so the
    /// box is valid and its snapshot is self-consistent with the (stale,
    /// still-executing) code that trapped. The entry must proceed to a normal
    /// reconstruction; short-circuiting here stashed an identity-less
    /// `bci == u32::MAX` sentinel that forced every post-supersession trap
    /// onto the corrupting imprecise re-run. (The short-circuit survived under
    /// a `CRATONVM_JIT_FREE_CODE=1` mode, since deleted.)
    #[test]
    fn superseded_guard_still_reconstructs_in_retain_mode() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let live = Box::new(AtomicU64::new(3)); // live epoch = 3
        let guard = DeoptEpochGuard::new();
        guard.creation_epoch.store(1, Ordering::Relaxed); // artifact made at epoch 1 < 3
        guard.live_epoch_cell.store(
            live.as_ref() as *const AtomicU64 as *mut AtomicU64,
            Ordering::Release,
        );
        assert!(guard.is_superseded());

        let regs = SavedRegisters::default();
        let point = DeoptimizationPoint {
            native_offset: 0,
            bci: 21,
            reason: DeoptReason::UnreachedCode,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: "T.m:()V".to_string(),
                bci: 21,
                locals: vec![FrameValue::Int(7)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        let _ = take_last_deopt();
        let r = x64_deopt_entry(&point, 0, &regs as *const SavedRegisters, &guard);
        assert_eq!(r, i64::MIN);
        let frame = take_last_deopt().expect("retain-mode superseded path reconstructs normally");
        assert_eq!(frame.bci, 21, "real bci, not the u32::MAX re-run sentinel");
        assert_eq!(
            frame.method_key, "T.m:()V",
            "identity preserved for the resume sinks"
        );
        assert_eq!(frame.locals[0], FrameValue::Int(7));
    }

    /// A FRESH guard (creation epoch == live epoch) does NOT short-circuit: the
    /// entry proceeds to reconstruct from the box as normal.
    #[test]
    fn fresh_guard_proceeds_to_reconstruct() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let live = Box::new(AtomicU64::new(2));
        let guard = DeoptEpochGuard::new();
        guard.creation_epoch.store(2, Ordering::Relaxed); // == live ⇒ fresh
        guard.live_epoch_cell.store(
            live.as_ref() as *const AtomicU64 as *mut AtomicU64,
            Ordering::Release,
        );
        assert!(!guard.is_superseded());

        let regs = SavedRegisters::default();
        let point = DeoptimizationPoint {
            native_offset: 0,
            bci: 11,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: String::new(),
                bci: 11,
                locals: vec![FrameValue::Int(99)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        let _ = take_last_deopt();
        let r = x64_deopt_entry(&point, 0, &regs as *const SavedRegisters, &guard);
        assert_eq!(r, i64::MIN);
        let frame = take_last_deopt().expect("fresh path reconstructs from the box");
        assert_eq!(frame.bci, 11);
        assert_eq!(frame.locals[0], FrameValue::Int(99));
    }
}

/// Count how many virtual objects need materialization across locals and
/// stack in a single frame (non-recursive).
pub fn count_virtual_objects(frame: &FrameState) -> usize {
    fn count_in(values: &[FrameValue]) -> usize {
        values
            .iter()
            .filter(|v| matches!(v, FrameValue::VirtualObject(_)))
            .count()
    }

    count_in(&frame.locals) + count_in(&frame.stack)
}

// ---------------------------------------------------------------------------
// Interned, immutable frame states (structural sharing)
// ---------------------------------------------------------------------------
//
// The P0 acceptance criterion this section serves:
//
//   "Safepoint metadata structurally shares states and supports inlining."
//
// A [`FrameState`] owns its slots: `Vec<FrameValue>` locals, `Vec<FrameValue>`
// stack, `Vec<MonitorInfo>` monitors, a `String` method key and a
// `Box<FrameState>` caller. One snapshot per safepoint is therefore one full
// copy per safepoint, and a hot method has a safepoint at every call, poll and
// guard. Consecutive safepoints differ in *one or two* slots — the JVM operand
// stack pushes one value per bytecode — so the copies are almost entirely
// identical, and the cost is paid again for every inlined caller scope once
// producers start building them. That is why interning has to land *before*
// inlined scope chains do: a depth-8 chain that re-copies the caller's locals
// at every callee safepoint multiplies the metadata by the inline depth.
//
// The representation here is persistent and parent-linked:
//
//   * a scope is a [`SharedFrameState`] — seven `Copy` handles, no owned heap;
//   * `locals`/`stack` are [`ValuesId`] handles to an interned *spine* of
//     interned fixed-size chunks ([`FRAME_VALUE_CHUNK`] slots each), so two
//     snapshots differing in one slot share every chunk but one;
//   * `caller` is an `Option<FrameStateId>`, i.e. the inlined caller scope is
//     shared by every deopt point inside the same inlined callee rather than
//     deep-copied into each;
//   * the resume semantics that used to be a per-[`DeoptReason`] prose
//     convention are an explicit [`ResumeSemantics`] field.
//
// Everything is content-addressed: interning the same state twice returns the
// same [`FrameStateId`], so `==` on handles is structural equality and a
// `FxHashMap` keyed by handle is a map keyed by frame state.
//
// **Compatibility.** No producer is edited by this change (`ir_lower.rs` and
// `x64.rs` are owned elsewhere), so the owned [`FrameState`] stays exactly as
// it is and the two directions are explicit:
// [`FrameStateInterner::intern`] takes what producers already build, and
// [`FrameStateInterner::materialize`] hands consumers (the resume sinks,
// [`reconstruct_frame`], [`DeoptVerifier`]) the owned form they already
// consume. See `docs/jit/deopt-frame-state-interning.md` for the producer
// edits that would let the interned form be the *only* form.

/// How many [`FrameValue`]s one interned chunk holds.
///
/// The unit of structural sharing: two value arrays that differ in one slot
/// share every chunk except the one containing it, so the storage cost of a
/// derived snapshot is one chunk rather than one array. Smaller chunks share
/// more and cost more spine entries (`u32` each); 8 is the point where the
/// spine is ~5% of the slot bytes it indexes for the array lengths real
/// methods produce (`max_locals` + `max_stack` in the tens).
pub const FRAME_VALUE_CHUNK: usize = 8;

/// Hard cap on how many scopes a caller chain may be walked for.
///
/// Chains are acyclic by construction (a scope can only name a caller that was
/// interned *before* it, so caller handles are strictly smaller), but every
/// walk in this module is bounded anyway: a metadata defect must not turn into
/// an unbounded loop inside a deopt path.
const MAX_SCOPE_CHAIN: usize = 256;

/// Handle to an interned scope in a [`FrameStateInterner`].
///
/// Handles are only meaningful in the interner that issued them. Two handles
/// from the same interner are equal exactly when the states they name are
/// structurally equal, including their whole caller chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameStateId(u32);

/// Handle to an interned array of [`FrameValue`]s (a scope's locals or stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValuesId(u32);

/// Handle to an interned array of [`MonitorInfo`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonitorsId(u32);

/// Handle to an interned method key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MethodKeyId(u32);

/// Handle to one interned chunk of [`FRAME_VALUE_CHUNK`] (or fewer, for the
/// last chunk of an array) values. Private: chunking is an implementation
/// detail of the sharing, not part of the frame-state model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ChunkId(u32);

impl FrameStateId {
    /// Raw index, for diagnostics and stable ordering only.
    pub fn index(self) -> u32 {
        self.0
    }
}

impl ValuesId {
    /// Raw index, for diagnostics and stable ordering only.
    pub fn index(self) -> u32 {
        self.0
    }
}

impl MonitorsId {
    /// Raw index, for diagnostics and stable ordering only.
    pub fn index(self) -> u32 {
        self.0
    }
}

impl MethodKeyId {
    /// Raw index, for diagnostics and stable ordering only.
    pub fn index(self) -> u32 {
        self.0
    }
}

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
/// The distinction is load-bearing for inlining, which is why it lands with
/// the interning: the innermost (trapping) scope of an inlined chain
/// re-executes its bytecode, but every **caller** scope is parked mid-`invoke`
/// — its `bci` names a call that is already in progress, and re-executing it
/// would call the callee a second time. [`Self::for_caller_scope`] is that
/// answer, and [`FrameStateInterner::intern`] applies it automatically to
/// every scope it links as a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResumeSemantics {
    /// Re-run the bytecode at `bci` instead of continuing after it.
    pub reexecute: bool,
    /// `bci` names a throwing instruction with a pending exception; route it
    /// through the exception table rather than resuming.
    pub rethrow_exception: bool,
}

impl ResumeSemantics {
    /// Continue *after* the bytecode at `bci`; it has already taken effect.
    pub const RESUME: Self = Self {
        reexecute: false,
        rethrow_exception: false,
    };

    /// Re-run the bytecode at `bci` from the top — the guard fired before it.
    pub const REEXECUTE: Self = Self {
        reexecute: true,
        rethrow_exception: false,
    };

    /// Not a resume point: route the pending exception through the method's
    /// exception table, starting at the throwing `bci`.
    pub const RETHROW: Self = Self {
        reexecute: false,
        rethrow_exception: true,
    };

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
            (_, true) => f.write_str("rethrow"),
            (true, false) => f.write_str("reexecute"),
            (false, false) => f.write_str("resume"),
        }
    }
}

/// One interned scope: a [`FrameState`] with every owned field replaced by a
/// handle, plus the explicit [`ResumeSemantics`] the owned form has no room
/// for.
///
/// `Copy` and 32 bytes, so a snapshot sequence is a `Vec` of these rather than
/// a `Vec` of nested heap graphs. Handles are only valid in the
/// [`FrameStateInterner`] that issued them; read them through that interner's
/// accessors rather than indexing anything yourself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SharedFrameState {
    /// Interned `"<class>.<method>:<descriptor>"` key.
    pub method_key: MethodKeyId,
    /// Bytecode index this scope names (resume point, re-execution point or
    /// throwing instruction — see `semantics`).
    pub bci: u32,
    /// Interned local-variable array.
    pub locals: ValuesId,
    /// Interned operand-stack array.
    pub stack: ValuesId,
    /// Interned held-monitor array.
    pub monitors: MonitorsId,
    /// The inlined caller scope, shared with every other deopt point inside
    /// the same inlined callee.
    pub caller: Option<FrameStateId>,
    /// Re-execute / rethrow, explicitly.
    pub semantics: ResumeSemantics,
}

/// A [`DeoptimizationPoint`] whose frame state is an interned handle.
///
/// `Copy` and pointer-free, so — unlike `DeoptimizationPoint`, which owns a
/// whole `FrameState` graph — it is cheap to clone into a sorted table: every
/// point at the same bci with the same live values names the *same*
/// `frame_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternedDeoptPoint {
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
    /// Handle to the interned frame state.
    pub frame_state: FrameStateId,
}

/// What one interner is holding, and how much of it is shared.
///
/// The measurement that makes the sharing claim checkable rather than
/// asserted: `logical_slots` is what today's owned `Vec<FrameValue>`s would
/// store (one full copy per snapshot), `stored_slots` is what the interner
/// actually holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterningStats {
    /// How many times a scope was offered for interning (hits included).
    pub intern_requests: u64,
    /// Distinct scopes held.
    pub states: usize,
    /// Distinct value arrays (locals/stack) held.
    pub value_arrays: usize,
    /// Distinct value chunks held.
    pub chunks: usize,
    /// Slots actually stored, i.e. the sum of every distinct chunk's length.
    pub stored_slots: usize,
    /// Slots the owned representation would store: the sum over distinct
    /// scopes of `locals.len() + stack.len()`.
    pub logical_slots: u64,
    /// Chunk-handle entries across every distinct spine (the sharing overhead).
    pub spine_entries: usize,
    /// Distinct monitor arrays held.
    pub monitor_arrays: usize,
    /// Monitor entries actually stored.
    pub stored_monitors: usize,
    /// Monitor entries the owned representation would store.
    pub logical_monitors: u64,
    /// Distinct method keys held.
    pub method_keys: usize,
}

impl InterningStats {
    /// Fraction of logical slots that cost no storage because an identical
    /// chunk was already held. `0.0` when nothing has been interned.
    pub fn slot_sharing_ratio(&self) -> f64 {
        if self.logical_slots == 0 {
            return 0.0;
        }
        1.0 - (self.stored_slots as f64 / self.logical_slots as f64)
    }

    /// Bytes of slot storage the owned `Vec<FrameValue>` representation would
    /// need (excluding the `Vec` headers, which favour the owned form).
    pub fn owned_slot_bytes(&self) -> usize {
        (self.logical_slots as usize).saturating_mul(mem::size_of::<FrameValue>())
    }

    /// Bytes of slot storage the interned representation needs: the distinct
    /// chunks plus the spines that index them.
    pub fn interned_slot_bytes(&self) -> usize {
        self.stored_slots
            .saturating_mul(mem::size_of::<FrameValue>())
            .saturating_add(self.spine_entries.saturating_mul(mem::size_of::<ChunkId>()))
    }

    /// Fraction of slot *bytes* saved against the owned representation,
    /// counting the spine overhead against the interned side.
    pub fn slot_byte_saving(&self) -> f64 {
        let owned = self.owned_slot_bytes();
        if owned == 0 {
            return 0.0;
        }
        1.0 - (self.interned_slot_bytes() as f64 / owned as f64)
    }

    /// Fraction of intern requests answered by an existing scope.
    pub fn state_dedup_ratio(&self) -> f64 {
        if self.intern_requests == 0 {
            return 0.0;
        }
        1.0 - (self.states as f64 / self.intern_requests as f64)
    }
}

/// Hash one [`FrameValue`] structurally, consistently with its `PartialEq`.
///
/// Written by hand rather than derived because [`FrameValue`] carries only
/// `PartialEq` today and widening its derives would change a type four other
/// files construct. Every field `PartialEq` compares is hashed, and nothing
/// else, which is the property the interner's hash buckets need.
fn hash_frame_value<H: Hasher>(v: &FrameValue, state: &mut H) {
    mem::discriminant(v).hash(state);
    match v {
        FrameValue::Int(i) | FrameValue::Long(i) => i.hash(state),
        FrameValue::Float(b) | FrameValue::Double(b) | FrameValue::Object(b) => b.hash(state),
        FrameValue::Register(r)
        | FrameValue::RegisterLong(r)
        | FrameValue::RegisterRef(r)
        | FrameValue::XmmFloat(r)
        | FrameValue::XmmDouble(r) => r.hash(state),
        FrameValue::StackSlot(o)
        | FrameValue::StackSlotRef(o)
        | FrameValue::StackSlotLong(o)
        | FrameValue::StackSlotFloat(o)
        | FrameValue::StackSlotDouble(o) => o.hash(state),
        FrameValue::VirtualObject(vo) => {
            vo.id.hash(state);
            vo.class_id.hash(state);
            vo.num_fields.hash(state);
            for f in &vo.field_values {
                hash_frame_value(f, state);
            }
        }
        FrameValue::VirtualObjectRef(id) => id.hash(state),
        FrameValue::MaterializationRequired(ev) => {
            ev.producer.hash(state);
            ev.class_id.hash(state);
            mem::discriminant(&ev.cause).hash(state);
        }
        FrameValue::Undefined | FrameValue::Unsupported => {}
    }
}

/// Structural hash of a value slice (its length included, so `[a]` and `[a, a]`
/// do not collide by construction).
fn hash_frame_values(values: &[FrameValue]) -> u64 {
    let mut h = DefaultHasher::new();
    values.len().hash(&mut h);
    for v in values {
        hash_frame_value(v, &mut h);
    }
    h.finish()
}

/// Structural hash of a monitor slice. [`MonitorInfo`] carries no `PartialEq`,
/// so both the hash and the equality used against it are spelled out here.
fn hash_monitors(monitors: &[MonitorInfo]) -> u64 {
    let mut h = DefaultHasher::new();
    monitors.len().hash(&mut h);
    for m in monitors {
        hash_frame_value(&m.object, &mut h);
        m.lock_depth.hash(&mut h);
        m.relock.hash(&mut h);
    }
    h.finish()
}

/// How many [`FrameValue::MaterializationRequired`] markers `v` is or
/// transitively contains as a virtual-object field.
///
/// Identical to the counting arm inside [`count_materialization_required`],
/// factored out so the interned predicate cannot drift from the owned one.
fn count_materialization_required_in(v: &FrameValue) -> usize {
    match v {
        FrameValue::MaterializationRequired(_) => 1,
        FrameValue::VirtualObject(state) => state
            .field_values
            .iter()
            .map(count_materialization_required_in)
            .sum(),
        _ => 0,
    }
}

/// Structural equality of two monitor slices — the counterpart of
/// [`hash_monitors`].
fn monitors_eq(a: &[MonitorInfo], b: &[MonitorInfo]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| {
                x.lock_depth == y.lock_depth && x.relock == y.relock && x.object == y.object
            })
}

/// Content-addressed store for immutable frame states.
///
/// One interner per compilation. Nothing is ever removed: an artifact's
/// metadata is built once and the interner is dropped with it, so there is no
/// reclamation policy to get wrong, and handles can never dangle.
///
/// Every accessor tolerates a handle it did not issue (returning `None`, an
/// empty slice or a conservative `false`) rather than panicking — this type is
/// reachable from deopt paths, where a panic is strictly worse than a refusal.
#[derive(Debug)]
pub struct FrameStateInterner {
    /// Distinct value chunks, indexed by [`ChunkId`].
    chunks: Vec<Arc<[FrameValue]>>,
    /// Structural hash → chunks with that hash (equality decides).
    chunk_index: FxHashMap<u64, Vec<ChunkId>>,
    /// Distinct spines (chunk-handle arrays), indexed by [`ValuesId`].
    arrays: Vec<Arc<[ChunkId]>>,
    /// Spine → its handle. `ChunkId` is `Hash + Eq`, so this needs no custom
    /// hashing and no allocation to probe.
    array_index: FxHashMap<Arc<[ChunkId]>, ValuesId>,
    /// Distinct monitor arrays, indexed by [`MonitorsId`].
    monitors: Vec<Arc<[MonitorInfo]>>,
    /// Structural hash → monitor arrays with that hash.
    monitor_index: FxHashMap<u64, Vec<MonitorsId>>,
    /// Distinct method keys, indexed by [`MethodKeyId`].
    keys: Vec<Arc<str>>,
    /// Method key → its handle.
    key_index: FxHashMap<Arc<str>, MethodKeyId>,
    /// Distinct scopes, indexed by [`FrameStateId`].
    states: Vec<SharedFrameState>,
    /// Scope → its handle. This is what makes identical states share one id.
    state_index: FxHashMap<SharedFrameState, FrameStateId>,
    /// Scopes offered for interning, hits included.
    intern_requests: u64,
    /// Slots the owned representation would have stored for the distinct
    /// scopes held (the denominator of the sharing ratio).
    logical_slots: u64,
    /// Monitor entries the owned representation would have stored.
    logical_monitors: u64,
}

impl Default for FrameStateInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameStateInterner {
    /// An empty interner.
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            chunk_index: FxHashMap::default(),
            arrays: Vec::new(),
            array_index: FxHashMap::default(),
            monitors: Vec::new(),
            monitor_index: FxHashMap::default(),
            keys: Vec::new(),
            key_index: FxHashMap::default(),
            states: Vec::new(),
            state_index: FxHashMap::default(),
            intern_requests: 0,
            logical_slots: 0,
            logical_monitors: 0,
        }
    }

    // ── interning ────────────────────────────────────────────────────

    /// Intern an owned [`FrameState`] and its whole caller chain.
    ///
    /// The compatibility direction producers use unchanged: they keep building
    /// the owned form, and this turns it into the shared one. The innermost
    /// scope gets [`ResumeSemantics::default`] (re-execute — what every guard
    /// the backends emit needs); every scope reached through `caller` gets
    /// [`ResumeSemantics::for_caller_scope`], because an inlined caller is
    /// parked mid-`invoke`.
    pub fn intern(&mut self, fs: &FrameState) -> FrameStateId {
        self.intern_with(fs, ResumeSemantics::default())
    }

    /// [`Self::intern`] with the innermost scope's semantics taken from the
    /// deopt reason (see [`ResumeSemantics::for_reason`]).
    pub fn intern_for_reason(&mut self, fs: &FrameState, reason: DeoptReason) -> FrameStateId {
        self.intern_with(fs, ResumeSemantics::for_reason(reason))
    }

    /// [`Self::intern`] with explicit semantics for the innermost scope.
    ///
    /// Iterative, not recursive: a caller chain is walked into a small vector
    /// and interned outermost-first, so a chain long enough to overflow a
    /// stack cannot — and it is capped at [`MAX_SCOPE_CHAIN`] regardless.
    pub fn intern_with(&mut self, fs: &FrameState, semantics: ResumeSemantics) -> FrameStateId {
        let mut chain: Vec<&FrameState> = Vec::new();
        let mut cursor = Some(fs);
        // Was the walk cut short with frames still above it?
        let mut truncated = false;
        while let Some(scope) = cursor {
            chain.push(scope);
            if chain.len() >= MAX_SCOPE_CHAIN {
                truncated = scope.caller.is_some();
                break;
            }
            cursor = scope.caller.as_deref();
        }

        // A cut chain must not be interned as if the last scope we kept were
        // the bottom of the stack. That is the silent-truncation shape: every
        // scope below the cut is well-formed, `chain_is_resumable` says clean,
        // and the resume rebuilds a call stack the program never had — a
        // *plausible* frame instead of the correct one, which is strictly worse
        // than a refusal. Terminating the chain with an explicitly
        // unreconstructable scope makes the cut visible to every consumer that
        // already knows how to refuse one: `chain_is_resumable`,
        // `frame_state_is_resumable` after materialization, and the
        // `DeoptVerifier` scope lane (an out-of-range bci under a method key it
        // has no limits for).
        let mut caller: Option<FrameStateId> = if truncated {
            Some(self.intern_scope(
                "",
                u32::MAX,
                &[FrameValue::Unsupported],
                &[],
                &[],
                None,
                ResumeSemantics::for_caller_scope(),
            ))
        } else {
            None
        };
        for (depth, scope) in chain.iter().enumerate().rev() {
            let sem = if depth == 0 {
                semantics
            } else {
                ResumeSemantics::for_caller_scope()
            };
            caller = Some(self.intern_scope(
                &scope.method_key,
                scope.bci,
                &scope.locals,
                &scope.stack,
                &scope.monitors,
                caller,
                sem,
            ));
        }
        // `chain` always holds at least `fs`, so this is `Some`; the fallback
        // keeps the function total rather than relying on that.
        caller.unwrap_or_else(|| {
            self.intern_scope(&fs.method_key, fs.bci, &[], &[], &[], None, semantics)
        })
    }

    /// Intern one scope from its parts. The building block the producers would
    /// call directly once they are allowed to change.
    #[allow(clippy::too_many_arguments)]
    pub fn intern_scope(
        &mut self,
        method_key: &str,
        bci: u32,
        locals: &[FrameValue],
        stack: &[FrameValue],
        monitors: &[MonitorInfo],
        caller: Option<FrameStateId>,
        semantics: ResumeSemantics,
    ) -> FrameStateId {
        let node = SharedFrameState {
            method_key: self.intern_key(method_key),
            bci,
            locals: self.intern_values(locals),
            stack: self.intern_values(stack),
            monitors: self.intern_monitors(monitors),
            caller,
            semantics,
        };
        self.intern_node(node)
    }

    /// Intern a [`DeoptimizationPoint`], taking the innermost scope's semantics
    /// from the point itself.
    ///
    /// It used to re-derive them with [`ResumeSemantics::for_reason`], because
    /// the owned point had no field to read. It does now, and every producer
    /// fills that field with `for_reason(reason)` — so this is the same answer
    /// today, and the *producer's* answer once one of them knows better.
    pub fn intern_point(&mut self, point: &DeoptimizationPoint) -> InternedDeoptPoint {
        InternedDeoptPoint {
            native_offset: point.native_offset,
            bci: point.bci,
            reason: point.reason,
            action: point.action,
            speculation_id: point.speculation_id,
            frame_state: self.intern_with(&point.frame_state, point.semantics),
        }
    }

    /// Intern an already-assembled scope node, deduplicating against every
    /// scope held.
    fn intern_node(&mut self, node: SharedFrameState) -> FrameStateId {
        self.intern_requests = self.intern_requests.saturating_add(1);
        if let Some(&id) = self.state_index.get(&node) {
            return id;
        }
        let id = FrameStateId(self.states.len() as u32);
        let slots = (self.values_len(node.locals) + self.values_len(node.stack)) as u64;
        let mons = self.monitor_slice(node.monitors).len() as u64;
        self.states.push(node);
        self.state_index.insert(node, id);
        self.logical_slots = self.logical_slots.saturating_add(slots);
        self.logical_monitors = self.logical_monitors.saturating_add(mons);
        id
    }

    fn intern_key(&mut self, key: &str) -> MethodKeyId {
        if let Some(&id) = self.key_index.get(key) {
            return id;
        }
        let id = MethodKeyId(self.keys.len() as u32);
        let arc: Arc<str> = Arc::from(key);
        self.keys.push(Arc::clone(&arc));
        self.key_index.insert(arc, id);
        id
    }

    /// Intern one chunk (at most [`FRAME_VALUE_CHUNK`] values).
    fn intern_chunk(&mut self, slice: &[FrameValue]) -> ChunkId {
        let h = hash_frame_values(slice);
        if let Some(bucket) = self.chunk_index.get(&h) {
            for &cid in bucket {
                if self
                    .chunks
                    .get(cid.0 as usize)
                    .is_some_and(|held| &held[..] == slice)
                {
                    return cid;
                }
            }
        }
        let cid = ChunkId(self.chunks.len() as u32);
        let arc: Arc<[FrameValue]> = Arc::from(slice.to_vec());
        self.chunks.push(arc);
        self.chunk_index.entry(h).or_default().push(cid);
        cid
    }

    /// Intern a spine of chunk handles.
    fn intern_spine(&mut self, spine: Vec<ChunkId>) -> ValuesId {
        if let Some(&id) = self.array_index.get(spine.as_slice()) {
            return id;
        }
        let id = ValuesId(self.arrays.len() as u32);
        let arc: Arc<[ChunkId]> = Arc::from(spine);
        self.arrays.push(Arc::clone(&arc));
        self.array_index.insert(arc, id);
        id
    }

    /// Intern a value array by chunking it. Chunk `i` always covers slots
    /// `[i * FRAME_VALUE_CHUNK, …)`, so only the last chunk may be short and a
    /// slot index maps to a chunk by division — the invariant
    /// [`Self::replace_slot`] depends on.
    fn intern_values(&mut self, values: &[FrameValue]) -> ValuesId {
        let mut spine: Vec<ChunkId> = Vec::with_capacity(values.len().div_ceil(FRAME_VALUE_CHUNK));
        for chunk in values.chunks(FRAME_VALUE_CHUNK) {
            let cid = self.intern_chunk(chunk);
            spine.push(cid);
        }
        self.intern_spine(spine)
    }

    fn intern_monitors(&mut self, monitors: &[MonitorInfo]) -> MonitorsId {
        let h = hash_monitors(monitors);
        if let Some(bucket) = self.monitor_index.get(&h) {
            for &mid in bucket {
                if self
                    .monitors
                    .get(mid.0 as usize)
                    .is_some_and(|held| monitors_eq(held, monitors))
                {
                    return mid;
                }
            }
        }
        let mid = MonitorsId(self.monitors.len() as u32);
        let arc: Arc<[MonitorInfo]> = Arc::from(monitors.to_vec());
        self.monitors.push(arc);
        self.monitor_index.entry(h).or_default().push(mid);
        mid
    }

    // ── reading ──────────────────────────────────────────────────────

    /// The scope a handle names, or `None` for a handle this interner never
    /// issued.
    pub fn scope(&self, id: FrameStateId) -> Option<&SharedFrameState> {
        self.states.get(id.0 as usize)
    }

    /// The method key of a scope (`""` for an unknown handle).
    pub fn method_key(&self, id: FrameStateId) -> &str {
        match self
            .scope(id)
            .and_then(|s| self.keys.get(s.method_key.0 as usize))
        {
            Some(key) => key,
            None => "",
        }
    }

    /// The bci of a scope, or `None` for an unknown handle.
    pub fn bci(&self, id: FrameStateId) -> Option<u32> {
        self.scope(id).map(|s| s.bci)
    }

    /// The resume semantics of a scope, or `None` for an unknown handle.
    pub fn semantics(&self, id: FrameStateId) -> Option<ResumeSemantics> {
        self.scope(id).map(|s| s.semantics)
    }

    /// The inlined caller of a scope, if any.
    pub fn caller(&self, id: FrameStateId) -> Option<FrameStateId> {
        self.scope(id).and_then(|s| s.caller)
    }

    /// How many inlined caller scopes sit above `id` (`0` for a scope that was
    /// not inlined into anything).
    pub fn depth(&self, id: FrameStateId) -> usize {
        let mut n = 0;
        let mut cursor = self.caller(id);
        while let Some(c) = cursor {
            n += 1;
            if n >= MAX_SCOPE_CHAIN {
                break;
            }
            cursor = self.caller(c);
        }
        n
    }

    /// Number of local slots in a scope.
    pub fn locals_len(&self, id: FrameStateId) -> usize {
        self.scope(id).map_or(0, |s| self.values_len(s.locals))
    }

    /// Number of operand-stack slots in a scope.
    pub fn stack_len(&self, id: FrameStateId) -> usize {
        self.scope(id).map_or(0, |s| self.values_len(s.stack))
    }

    /// One local slot, without materializing the array.
    pub fn local(&self, id: FrameStateId, index: usize) -> Option<&FrameValue> {
        self.slot(self.scope(id)?.locals, index)
    }

    /// One operand-stack slot, without materializing the array.
    pub fn stack_slot(&self, id: FrameStateId, index: usize) -> Option<&FrameValue> {
        self.slot(self.scope(id)?.stack, index)
    }

    /// The held monitors of a scope (empty for an unknown handle).
    pub fn monitors(&self, id: FrameStateId) -> &[MonitorInfo] {
        match self.scope(id) {
            Some(node) => self.monitor_slice(node.monitors),
            None => &[],
        }
    }

    /// Logical length of an interned value array.
    fn values_len(&self, id: ValuesId) -> usize {
        let Some(spine) = self.arrays.get(id.0 as usize) else {
            return 0;
        };
        match spine.last() {
            None => 0,
            Some(last) => {
                let tail = self
                    .chunks
                    .get(last.0 as usize)
                    .map_or(0, |chunk| chunk.len());
                (spine.len() - 1) * FRAME_VALUE_CHUNK + tail
            }
        }
    }

    /// One slot of an interned value array.
    fn slot(&self, id: ValuesId, index: usize) -> Option<&FrameValue> {
        let spine = self.arrays.get(id.0 as usize)?;
        let cid = spine.get(index / FRAME_VALUE_CHUNK)?;
        self.chunks
            .get(cid.0 as usize)?
            .get(index % FRAME_VALUE_CHUNK)
    }

    /// Every value of an interned array, in order.
    fn values_iter(&self, id: ValuesId) -> impl Iterator<Item = &FrameValue> + '_ {
        let spine: &[ChunkId] = match self.arrays.get(id.0 as usize) {
            Some(held) => held,
            None => &[],
        };
        spine.iter().flat_map(move |cid| {
            let chunk: &[FrameValue] = match self.chunks.get(cid.0 as usize) {
                Some(held) => held,
                None => &[],
            };
            chunk.iter()
        })
    }

    fn values_vec(&self, id: ValuesId) -> Vec<FrameValue> {
        let mut out = Vec::with_capacity(self.values_len(id));
        out.extend(self.values_iter(id).cloned());
        out
    }

    fn monitor_slice(&self, id: MonitorsId) -> &[MonitorInfo] {
        match self.monitors.get(id.0 as usize) {
            Some(held) => held,
            None => &[],
        }
    }

    // ── persistent derivation ────────────────────────────────────────

    /// The scope `id` with local `index` replaced — the persistent update.
    ///
    /// Costs one chunk and one spine, not one copy of the locals array. An
    /// out-of-range index or an unknown handle returns `id` unchanged (there
    /// is nothing to derive and nothing worth panicking over), and so does a
    /// write of the value already there.
    pub fn with_local(
        &mut self,
        id: FrameStateId,
        index: usize,
        value: FrameValue,
    ) -> FrameStateId {
        let Some(node) = self.scope(id).copied() else {
            return id;
        };
        let Some(locals) = self.replace_slot(node.locals, index, value) else {
            return id;
        };
        if locals == node.locals {
            return id;
        }
        self.intern_node(SharedFrameState { locals, ..node })
    }

    /// The scope `id` with operand-stack slot `index` replaced. See
    /// [`Self::with_local`].
    pub fn with_stack_slot(
        &mut self,
        id: FrameStateId,
        index: usize,
        value: FrameValue,
    ) -> FrameStateId {
        let Some(node) = self.scope(id).copied() else {
            return id;
        };
        let Some(stack) = self.replace_slot(node.stack, index, value) else {
            return id;
        };
        if stack == node.stack {
            return id;
        }
        self.intern_node(SharedFrameState { stack, ..node })
    }

    /// The scope `id` resuming at a different bci — every slot shared.
    pub fn with_bci(&mut self, id: FrameStateId, bci: u32) -> FrameStateId {
        let Some(node) = self.scope(id).copied() else {
            return id;
        };
        if node.bci == bci {
            return id;
        }
        self.intern_node(SharedFrameState { bci, ..node })
    }

    /// The scope `id` linked under an inlined caller — every slot of both
    /// scopes shared.
    ///
    /// This is the operation inlined scope chains need: one interned caller
    /// scope is linked from every deopt point the inlined callee emits, so a
    /// callee with 20 safepoints costs 20 handles, not 20 copies of the
    /// caller's frame.
    pub fn with_caller(&mut self, id: FrameStateId, caller: Option<FrameStateId>) -> FrameStateId {
        let Some(node) = self.scope(id).copied() else {
            return id;
        };
        if node.caller == caller {
            return id;
        }
        self.intern_node(SharedFrameState { caller, ..node })
    }

    /// The scope `id` with different resume semantics — every slot shared.
    pub fn with_semantics(&mut self, id: FrameStateId, semantics: ResumeSemantics) -> FrameStateId {
        let Some(node) = self.scope(id).copied() else {
            return id;
        };
        if node.semantics == semantics {
            return id;
        }
        self.intern_node(SharedFrameState { semantics, ..node })
    }

    /// Replace one slot of an interned array, re-interning only the chunk that
    /// contains it. `None` when the array or the index does not exist;
    /// `Some(values)` unchanged when the slot already holds `value`.
    fn replace_slot(
        &mut self,
        values: ValuesId,
        index: usize,
        value: FrameValue,
    ) -> Option<ValuesId> {
        let spine = Arc::clone(self.arrays.get(values.0 as usize)?);
        let chunk_index = index / FRAME_VALUE_CHUNK;
        let cid = *spine.get(chunk_index)?;
        let mut chunk = self.chunks.get(cid.0 as usize)?.to_vec();
        let pos = index % FRAME_VALUE_CHUNK;
        if pos >= chunk.len() {
            return None;
        }
        if chunk[pos] == value {
            return Some(values);
        }
        chunk[pos] = value;
        let new_cid = self.intern_chunk(&chunk);
        let mut new_spine = spine.to_vec();
        new_spine[chunk_index] = new_cid;
        Some(self.intern_spine(new_spine))
    }

    // ── materialization (the other half of the compatibility layer) ──

    /// Rebuild the owned [`FrameState`] a handle names, caller chain included.
    ///
    /// `None` for a handle this interner never issued. The owned form has no
    /// room for [`ResumeSemantics`], so materializing **drops** it — which is
    /// exactly the producer/consumer gap `docs/jit/deopt-metadata.md` §5.3
    /// records, and why the flag has to reach `DeoptimizationPoint` before the
    /// owned form can be retired.
    pub fn materialize(&self, id: FrameStateId) -> Option<FrameState> {
        let mut chain: Vec<&SharedFrameState> = Vec::new();
        let mut cursor = Some(id);
        while let Some(handle) = cursor {
            let node = self.scope(handle)?;
            chain.push(node);
            if chain.len() >= MAX_SCOPE_CHAIN {
                // Refuse, do not truncate. Returning the first
                // `MAX_SCOPE_CHAIN` scopes with the last one's `caller` set to
                // `None` would hand the caller a well-formed `FrameState` that
                // describes a *different* call stack from the one that
                // trapped, with nothing anywhere marking it short. `None`
                // routes to the safe whole-method re-run instead; the
                // `DeoptVerifier` turns it into
                // `DeoptMetadataError::ScopeChainTooDeep`.
                if node.caller.is_some() {
                    return None;
                }
                break;
            }
            cursor = node.caller;
        }

        let mut built: Option<Box<FrameState>> = None;
        for node in chain.iter().rev() {
            built = Some(Box::new(FrameState {
                method_key: self
                    .keys
                    .get(node.method_key.0 as usize)
                    .map_or_else(String::new, |k| k.to_string()),
                bci: node.bci,
                locals: self.values_vec(node.locals),
                stack: self.values_vec(node.stack),
                monitors: self.monitor_slice(node.monitors).to_vec(),
                caller: built.take(),
            }));
        }
        built.map(|boxed| *boxed)
    }

    /// Rebuild the owned [`DeoptimizationPoint`] an interned point names.
    ///
    /// The semantics survive the round trip: they are read back from the
    /// interned innermost scope rather than re-derived from `reason`. (The
    /// caller *scopes'* semantics are still dropped — the owned `FrameState`
    /// has no field for them — which is the remaining half of the gap
    /// `docs/jit/deopt-frame-state-interning.md` records.)
    pub fn materialize_point(&self, point: &InternedDeoptPoint) -> Option<DeoptimizationPoint> {
        Some(DeoptimizationPoint {
            native_offset: point.native_offset,
            bci: point.bci,
            reason: point.reason,
            action: point.action,
            speculation_id: point.speculation_id,
            frame_state: self.materialize(point.frame_state)?,
            semantics: self
                .semantics(point.frame_state)
                .unwrap_or_else(|| ResumeSemantics::for_reason(point.reason)),
        })
    }

    // ── predicates over the interned form ────────────────────────────

    /// Is *this* scope's own frame reconstructable?
    ///
    /// Scope-local by design: only this scope's locals and stack are
    /// inspected, so it answers "is this one frame clean" for a caller walking
    /// a chain itself. [`Self::chain_is_resumable`] is the whole-chain answer,
    /// and it is the one that corresponds to the owned
    /// [`frame_state_is_resumable`] — which follows `caller` for exactly the
    /// reason spelled out there. The two agree on a one-scope chain, which is
    /// every chain a producer builds today.
    ///
    /// An unknown handle is *not* resumable: refusing costs a whole-method
    /// re-run, accepting would resume a frame nobody can describe.
    pub fn is_resumable(&self, id: FrameStateId) -> bool {
        let Some(node) = self.scope(id) else {
            return false;
        };
        !self
            .values_iter(node.locals)
            .chain(self.values_iter(node.stack))
            .any(value_blocks_resume)
    }

    /// [`Self::is_resumable`] for a scope **and** every inlined caller above
    /// it: a caller scope holding a value no one can rebuild is exactly as
    /// unresumable as the trapping scope holding one.
    ///
    /// Bounded by [`MAX_SCOPE_CHAIN`], and — like the owned
    /// [`frame_state_is_resumable`] this mirrors — a chain that is still going
    /// at the cap answers **`false`**, not `true`. Both callers are compile-time
    /// admission gates deciding "may this artifact ever be entered", so
    /// stopping the walk early and reporting `true` would admit an artifact on
    /// the strength of scopes nobody looked at. Refusing costs a whole-method
    /// re-run; admitting costs a wrong frame.
    pub fn chain_is_resumable(&self, id: FrameStateId) -> bool {
        let mut cursor = Some(id);
        let mut seen = 0;
        while let Some(handle) = cursor {
            if !self.is_resumable(handle) {
                return false;
            }
            seen += 1;
            if seen >= MAX_SCOPE_CHAIN {
                return self.caller(handle).is_none();
            }
            cursor = self.caller(handle);
        }
        true
    }

    /// [`count_materialization_required`] without materializing (scope-local,
    /// same as the owned function, down to the recursion into virtual-object
    /// field graphs).
    pub fn count_materialization_required(&self, id: FrameStateId) -> usize {
        let Some(node) = self.scope(id) else {
            return 0;
        };
        self.values_iter(node.locals)
            .chain(self.values_iter(node.stack))
            .map(count_materialization_required_in)
            .sum()
    }

    // ── measurement ──────────────────────────────────────────────────

    /// Slots the **owned** representation would store for `roots`: every scope
    /// of every root's caller chain, counted once per root.
    ///
    /// This is the honest denominator once inlining lands.
    /// [`InterningStats::logical_slots`] counts each distinct *scope* once,
    /// which is right for a flat snapshot sequence but understates what
    /// `DeoptimizationPoint` actually costs today: its caller is a
    /// `Box<FrameState>`, so an inlined caller's locals are re-copied into
    /// *every* deopt point of the inlined callee.
    pub fn owned_chain_slots(&self, roots: &[FrameStateId]) -> u64 {
        let mut total: u64 = 0;
        for &root in roots {
            let mut cursor = Some(root);
            let mut seen = 0;
            while let Some(handle) = cursor {
                let Some(node) = self.scope(handle) else {
                    break;
                };
                total = total.saturating_add(
                    (self.values_len(node.locals) + self.values_len(node.stack)) as u64,
                );
                seen += 1;
                if seen >= MAX_SCOPE_CHAIN {
                    break;
                }
                cursor = node.caller;
            }
        }
        total
    }

    /// What is held and how much of it is shared. See [`InterningStats`].
    pub fn stats(&self) -> InterningStats {
        InterningStats {
            intern_requests: self.intern_requests,
            states: self.states.len(),
            value_arrays: self.arrays.len(),
            chunks: self.chunks.len(),
            stored_slots: self.chunks.iter().map(|c| c.len()).sum(),
            logical_slots: self.logical_slots,
            spine_entries: self.arrays.iter().map(|a| a.len()).sum(),
            monitor_arrays: self.monitors.len(),
            stored_monitors: self.monitors.iter().map(|m| m.len()).sum(),
            logical_monitors: self.logical_monitors,
            method_keys: self.keys.len(),
        }
    }
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
    /// `deopt_points` is not sorted by `native_offset`, so
    /// `CompiledMethod::find_deopt_point`'s binary search can miss an entry —
    /// which reads as "no deopt metadata here" and forces the imprecise path.
    DeoptPointsUnsorted { first: u32, second: u32 },
    /// An [`InternedDeoptPoint`] names a [`FrameStateId`] the interner it was
    /// checked against never issued — a handle from a different interner, or
    /// from one that has since been rebuilt. Its scope chain cannot be read at
    /// all, so nothing about the point is verifiable.
    UnknownFrameStateHandle { native_offset: u32, handle: u32 },
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
                 — find_deopt_point's binary search can miss an entry"
            ),
            Self::UnknownFrameStateHandle {
                native_offset,
                handle,
            } => write!(
                f,
                "pc+0x{native_offset:x}: frame-state handle #{handle} was not issued by this \
                 interner — the deopt point's scope chain is unreadable"
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
///   sortedness `find_deopt_point` depends on.
#[derive(Debug, Default)]
pub struct DeoptVerifier {
    methods: FxHashMap<String, MethodFrameLimits>,
    oop_coverage: FxHashMap<u32, OopCoverage>,
    removed_nodes: FxHashSet<u32>,
    materializable_nodes: FxHashSet<u32>,
    require_oop_map: bool,
}

impl DeoptVerifier {
    /// A verifier with no data registered: structural lane only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the bytecode limits of one method (the root method or any
    /// inlined callee whose scopes appear in the metadata).
    pub fn with_method(mut self, limits: MethodFrameLimits) -> Self {
        self.methods.insert(limits.method_key.clone(), limits);
        self
    }

    /// Register the oop map covering the safepoint at `native_offset`.
    pub fn with_oop_map(mut self, native_offset: u32, coverage: OopCoverage) -> Self {
        self.oop_coverage.insert(native_offset, coverage);
        self
    }

    /// Register IR node ids the optimizer removed. A slot that names one of
    /// these without carrying a rebuild recipe is rejected.
    pub fn with_removed_nodes(mut self, nodes: impl IntoIterator<Item = u32>) -> Self {
        self.removed_nodes.extend(nodes);
        self
    }

    /// Register removed node ids the emitter *can* rebuild (the scalar
    /// replacement map's keys). These are the removed nodes a
    /// `VirtualObject` slot is allowed to name.
    pub fn with_materializable_nodes(mut self, nodes: impl IntoIterator<Item = u32>) -> Self {
        self.materializable_nodes.extend(nodes);
        self
    }

    /// Require every deopt point carrying reference slots to have a registered
    /// oop map. Off by default, because a backend that has not wired precise
    /// maps yet publishes none at all and its frames are handled
    /// conservatively — flagging every point there would be noise, not a
    /// finding.
    pub fn requiring_oop_map(mut self, require: bool) -> Self {
        self.require_oop_map = require;
        self
    }

    /// Verify `points`, returning every violation found (never short-circuits:
    /// the second violation is usually the one that explains the first).
    ///
    /// Never panics and never mutates — same contract as
    /// `ir_verify::verify_graph`, and for the same reason: this is the
    /// component that must survive input it was not designed for.
    pub fn violations(&self, points: &[DeoptimizationPoint]) -> Vec<DeoptMetadataError> {
        let mut out = Vec::new();

        for w in points.windows(2) {
            if w[0].native_offset > w[1].native_offset {
                out.push(DeoptMetadataError::DeoptPointsUnsorted {
                    first: w[0].native_offset,
                    second: w[1].native_offset,
                });
            }
        }

        for point in points {
            self.check_point(point, &mut out);
        }
        out
    }

    /// [`Self::violations`], as a compile step: `Ok(())` or a [`Bailout`]
    /// listing what was wrong.
    ///
    /// A failure means the artifact must NOT be installed. Bailing is always
    /// semantically valid — the method takes the interpreter or the
    /// single-pass backend — whereas installing code whose deopt cannot be
    /// reconstructed is not.
    pub fn verify(&self, points: &[DeoptimizationPoint]) -> CompileResult<()> {
        let errors = self.violations(points);
        if errors.is_empty() {
            return Ok(());
        }
        Err(deopt_metadata_bailout(&errors))
    }

    // ── per-point ────────────────────────────────────────────────────

    fn check_point(&self, point: &DeoptimizationPoint, out: &mut Vec<DeoptMetadataError>) {
        // A `PendingException` frame deliberately carries the *throwing* bci and
        // a post-pop operand stack — it is not a resume point (see
        // `DeoptReason::PendingException`), so the point-vs-frame bci identity
        // still holds and is checked, but nothing below assumes resumability.
        if point.bci != point.frame_state.bci {
            out.push(DeoptMetadataError::PointBciMismatch {
                native_offset: point.native_offset,
                point_bci: point.bci,
                frame_bci: point.frame_state.bci,
            });
        }

        // ── exception-state agreement ────────────────────────────────
        //
        // `reason` and `semantics.rethrow_exception` answer the same question
        // for two different consumers: `x64_deopt_entry` routes the frame to
        // `LAST_EXCEPTIONAL` on `reason == PendingException`, while a resume
        // sink that has learned to read `semantics` decides re-execute /
        // resume / rethrow from the flag. If they disagree, one of them treats
        // an exceptional frame as a resume point and executes past a call that
        // never returned — which is the `finally`-not-run / leaked-`athrow`-bci
        // defect shape, restated in metadata. Both directions are checked: a
        // `PendingException` point that forgot the flag, and a flagged point
        // whose reason will route it into `LAST_DEOPT`.
        let pending = point.reason == DeoptReason::PendingException;
        if pending != point.semantics.rethrow_exception {
            out.push(DeoptMetadataError::ResumeSemanticsMismatch {
                native_offset: point.native_offset,
                reason: point.reason,
                rethrow: point.semantics.rethrow_exception,
            });
        }

        let coverage = self.oop_coverage.get(&point.native_offset);
        let mut scope: Option<&FrameState> = Some(&point.frame_state);
        let mut depth = 0usize;
        while let Some(state) = scope {
            // Bounded exactly like every other walk in this module
            // (`frame_state_is_resumable`, `FrameStateInterner::materialize`):
            // a chain this deep is a metadata defect, and checking only its
            // first `MAX_SCOPE_CHAIN` scopes would report "clean" for a frame
            // whose outer scopes were never looked at.
            if depth >= MAX_SCOPE_CHAIN {
                out.push(DeoptMetadataError::ScopeChainTooDeep {
                    native_offset: point.native_offset,
                    cap: MAX_SCOPE_CHAIN,
                });
                break;
            }
            self.check_scope(point, state, depth, coverage, out);
            scope = state.caller.as_deref();
            depth += 1;
        }
    }

    /// The JVM value category a machine-location descriptor claims for the word
    /// it names, or `None` for a descriptor that names no machine word.
    ///
    /// Two descriptors of the *same* word must agree on this, which is what
    /// [`DeoptMetadataError::SlotTypeConflict`] enforces. `"ref"` is also what
    /// the oop-map agreement lanes join on.
    fn slot_category(v: &FrameValue) -> Option<(i32, &'static str)> {
        match v {
            FrameValue::StackSlot(off) => Some((*off, "int")),
            FrameValue::StackSlotRef(off) => Some((*off, "ref")),
            FrameValue::StackSlotLong(off) => Some((*off, "long")),
            FrameValue::StackSlotFloat(off) => Some((*off, "float")),
            FrameValue::StackSlotDouble(off) => Some((*off, "double")),
            _ => None,
        }
    }

    fn check_scope(
        &self,
        point: &DeoptimizationPoint,
        state: &FrameState,
        depth: usize,
        coverage: Option<&OopCoverage>,
        out: &mut Vec<DeoptMetadataError>,
    ) {
        // ── scope lane ───────────────────────────────────────────────
        if !self.methods.is_empty() {
            match self.methods.get(&state.method_key) {
                None => out.push(DeoptMetadataError::UnknownMethod {
                    native_offset: point.native_offset,
                    bci: state.bci,
                    method_key: state.method_key.clone(),
                    scope_depth: depth,
                }),
                Some(limits) => {
                    if state.bci >= limits.code_len {
                        out.push(DeoptMetadataError::BciOutOfRange {
                            native_offset: point.native_offset,
                            bci: state.bci,
                            method_key: state.method_key.clone(),
                            scope_depth: depth,
                            code_len: limits.code_len,
                        });
                    }
                    if state.locals.len() > limits.max_locals as usize {
                        out.push(DeoptMetadataError::LocalCountMismatch {
                            native_offset: point.native_offset,
                            bci: state.bci,
                            method_key: state.method_key.clone(),
                            scope_depth: depth,
                            found: state.locals.len(),
                            max_locals: limits.max_locals,
                        });
                    }
                    if state.stack.len() > limits.max_stack as usize {
                        out.push(DeoptMetadataError::StackCountMismatch {
                            native_offset: point.native_offset,
                            bci: state.bci,
                            method_key: state.method_key.clone(),
                            scope_depth: depth,
                            found: state.stack.len(),
                            max_stack: limits.max_stack,
                        });
                    }
                }
            }
        }

        // ── slot lanes ───────────────────────────────────────────────
        //
        // Virtual-object ids are scope-local (the materializer resolves shells
        // per reconstructed frame), so definitions and references are collected
        // per scope. References are validated at the end because a
        // `VirtualObjectRef` may legally precede its definition — the
        // materializer allocates all shells before wiring any field.
        let mut ctx = ScopeCheck::default();

        let base = |kind: SlotKind, index: usize| SlotRef {
            native_offset: point.native_offset,
            bci: state.bci,
            method_key: state.method_key.clone(),
            scope_depth: depth,
            kind,
            index,
        };

        for (i, v) in state.locals.iter().enumerate() {
            self.check_value(v, &base(SlotKind::Local, i), coverage, &mut ctx, out);
        }
        for (i, v) in state.stack.iter().enumerate() {
            self.check_value(v, &base(SlotKind::Stack, i), coverage, &mut ctx, out);
        }

        // ── monitor lane ─────────────────────────────────────────────
        //
        // "Balanced" here means the list can be replayed as `monitorenter`
        // pairs on resume: every entry names a re-lockable object, carries a
        // depth of at least one (a depth-0 entry is a lock nobody holds), and
        // no object appears twice (a re-entrant lock is ONE entry with depth 2 —
        // two entries would make the resume enter it twice and leave the
        // interpreter one `monitorexit` short at method end).
        for (i, m) in state.monitors.iter().enumerate() {
            let at = base(SlotKind::Monitor, i);
            if m.lock_depth == 0 {
                out.push(DeoptMetadataError::UnbalancedMonitor {
                    at: at.clone(),
                    detail: "lock_depth is 0, so the resume would record a lock nobody holds"
                        .to_string(),
                });
            }
            if state.monitors[..i].iter().any(|p| p.object == m.object) {
                out.push(DeoptMetadataError::UnbalancedMonitor {
                    at: at.clone(),
                    detail: format!(
                        "object {:?} is already held by an earlier entry — re-entrancy must be \
                         one entry with lock_depth > 1",
                        m.object
                    ),
                });
            }
            if value_blocks_resume(&m.object) || matches!(m.object, FrameValue::Undefined) {
                out.push(DeoptMetadataError::UnbalancedMonitor {
                    at: at.clone(),
                    detail: format!(
                        "monitor object {:?} cannot be reconstructed, so the resume cannot \
                         unlock it",
                        m.object
                    ),
                });
            }
            // A monitor entry replays as `monitorenter` on the object it names,
            // and the interpreter's method-exit path emits one `monitorexit`
            // per entry. So the entry has to name something lockable. A
            // primitive descriptor here is not a near-miss: the resume would
            // build a `Value::Int`, the exit would try to unlock it, and the
            // real monitor stays held — a hang in whatever thread asks for it
            // next, arbitrarily far from the deopt that caused it. An explicit
            // null is the same story with an NPE at the enter instead.
            if let Some(detail) = monitor_object_defect(&m.object) {
                out.push(DeoptMetadataError::MonitorObjectNotAReference {
                    at: at.clone(),
                    detail,
                });
            }
            self.check_value(&m.object, &at, coverage, &mut ctx, out);
        }

        for (at, id) in std::mem::take(&mut ctx.referenced) {
            if !ctx.defined.contains(&id) {
                out.push(DeoptMetadataError::UndefinedVirtualObjectRef { at, id });
            }
        }

        if ctx.needs_oop_map && coverage.is_none() && self.require_oop_map {
            out.push(DeoptMetadataError::MissingOopMap {
                native_offset: point.native_offset,
                bci: state.bci,
            });
        }
    }

    /// Check one `FrameValue`, recursing through virtual-object fields.
    fn check_value(
        &self,
        v: &FrameValue,
        at: &SlotRef,
        coverage: Option<&OopCoverage>,
        ctx: &mut ScopeCheck,
        out: &mut Vec<DeoptMetadataError>,
    ) {
        // ── frame-slot well-formedness, for every `StackSlot*` variant ──
        //
        // Two properties that hold for *all five* typed frame-slot variants
        // and that nothing checked before, so they are done once here rather
        // than five times in the match below.
        if let Some((off, kind)) = Self::slot_category(v) {
            // (1) The offset must address this frame. Every producer builds
            // these as `-spill_off` from a strictly positive `[rbp - spill]`
            // offset (`x64::frame_value_for_slot`, `x64::sr_field_values`,
            // `ir_lower::typed_stack_slot`), so `off >= 0` cannot come from a
            // correct emitter: `0` is `[rbp]`, the saved caller frame pointer
            // — the one word `ir_lower::poison_slot` exists to keep the
            // compiler from ever naming — and anything above it is the
            // caller's frame or the return address. Reconstructing a local
            // from there is a plausible-looking wrong value, which is worse
            // than a refusal.
            if off >= 0 {
                out.push(DeoptMetadataError::FrameSlotOutsideFrame {
                    at: at.clone(),
                    offset: off,
                });
            }
            // (2) One machine word, one value. Two slots may legitimately
            // *share* a word (`aload_0` leaves local 0 and stack[0] naming the
            // same spill), but then they agree on its type. Disagreeing is an
            // internal contradiction that needs no external data to spot, and
            // it is precisely how an `int` ends up reconstructed into a slot
            // the interpreter reads as a reference.
            // Cloned out of the map before the match so the lookup borrow ends
            // here rather than spanning the `insert` in the `None` arm.
            let prior = ctx.word_types.get(&off).cloned();
            match prior {
                Some((first, first_kind)) if first_kind != kind => {
                    out.push(DeoptMetadataError::SlotTypeConflict {
                        at: at.clone(),
                        first,
                        offset: off,
                        first_kind,
                        second_kind: kind,
                    });
                }
                Some(_) => {}
                None => {
                    ctx.word_types.insert(off, (at.clone(), kind));
                }
            }
        }

        match v {
            // ── register-file bounds ─────────────────────────────────
            FrameValue::Register(r) | FrameValue::RegisterLong(r) => {
                self.check_gpr_index(*r, out);
                self.check_primitive_not_in_oop_map(
                    at,
                    coverage,
                    "gpr",
                    *r as i32,
                    if matches!(v, FrameValue::Register(_)) {
                        "int"
                    } else {
                        "long"
                    },
                    |cov| cov.covers_register(*r),
                    out,
                );
            }
            FrameValue::XmmFloat(n) | FrameValue::XmmDouble(n) => {
                if *n as usize >= 16 {
                    out.push(DeoptMetadataError::RegisterFileIndexOutOfRange {
                        bank: "xmm",
                        index: *n,
                    });
                }
            }

            // ── oop-map agreement ────────────────────────────────────
            FrameValue::RegisterRef(r) => {
                ctx.needs_oop_map = true;
                self.check_gpr_index(*r, out);
                if let Some(cov) = coverage {
                    if cov.moving_young_coverage_complete && !cov.covers_register(*r) {
                        out.push(DeoptMetadataError::ReferenceRegisterNotInOopMap {
                            at: at.clone(),
                            reg: *r,
                        });
                    }
                }
            }
            FrameValue::StackSlotRef(off) => {
                ctx.needs_oop_map = true;
                // Deopt spells the word as `*(rbp + off)` with `off < 0`; the
                // oop map spells the same word as the positive `[rbp - off]`.
                let positive = -*off;
                if i16::try_from(positive).is_err() {
                    out.push(DeoptMetadataError::UnencodableRefOffset {
                        at: at.clone(),
                        frame_offset: positive,
                    });
                } else if let Some(cov) = coverage {
                    if cov.moving_young_coverage_complete && !cov.covers_frame_slot(positive) {
                        out.push(DeoptMetadataError::ReferenceNotInOopMap {
                            at: at.clone(),
                            frame_offset: positive,
                        });
                    }
                }
            }
            // The converse oop-map direction, for the four *primitive* frame
            // slots. See `check_primitive_not_in_oop_map`.
            FrameValue::StackSlot(off)
            | FrameValue::StackSlotLong(off)
            | FrameValue::StackSlotFloat(off)
            | FrameValue::StackSlotDouble(off) => {
                let positive = -*off;
                let kind = Self::slot_category(v).map_or("primitive", |(_, k)| k);
                self.check_primitive_not_in_oop_map(
                    at,
                    coverage,
                    "frame",
                    positive,
                    kind,
                    |cov| cov.covers_frame_slot(positive),
                    out,
                );
            }
            FrameValue::Object(addr) if *addr != 0 => {
                out.push(DeoptMetadataError::BakedObjectAddress {
                    at: at.clone(),
                    address: *addr,
                });
            }

            // ── virtual objects ──────────────────────────────────────
            FrameValue::VirtualObject(state) => {
                if !ctx.defined.insert(state.id) {
                    out.push(DeoptMetadataError::DuplicateVirtualObjectDefinition {
                        at: at.clone(),
                        id: state.id,
                    });
                }
                if state.num_fields != state.field_values.len() {
                    out.push(DeoptMetadataError::VirtualObjectFieldCountMismatch {
                        at: at.clone(),
                        id: state.id,
                        declared: state.num_fields,
                        found: state.field_values.len(),
                    });
                }
                // The id IS the IR node of the eliminated allocation. Naming a
                // removed node is only legal when that node is also registered
                // as materializable — otherwise the recipe was built from a
                // stale graph and rebuilds a slot from nothing.
                let node = state.id as u32;
                if self.removed_nodes.contains(&node) && !self.materializable_nodes.contains(&node)
                {
                    out.push(DeoptMetadataError::SlotNamesRemovedNode {
                        at: at.clone(),
                        node,
                    });
                }
                for (i, fv) in state.field_values.iter().enumerate() {
                    let field_at = at.field(state.id, i);
                    self.check_value(fv, &field_at, coverage, ctx, out);
                }
            }
            FrameValue::VirtualObjectRef(id) => ctx.referenced.push((at.clone(), *id)),

            // ── eliminated vs. undefined ─────────────────────────────
            //
            // `MaterializationRequired` is NOT a violation: it is the honest
            // marker for "this value was deleted and I cannot rebuild it", and
            // it already makes the frame unresumable, so the method takes the
            // safe re-run. What WOULD be a violation is the same situation
            // spelled `Undefined` — and that one is undetectable here by
            // construction, which is exactly why producers must be fixed to
            // emit this variant. See `docs/jit/deopt-metadata.md`.
            FrameValue::MaterializationRequired(_) => {}

            _ => {}
        }
    }

    fn check_gpr_index(&self, r: u8, out: &mut Vec<DeoptMetadataError>) {
        if r as usize >= 16 {
            out.push(DeoptMetadataError::RegisterFileIndexOutOfRange {
                bank: "gpr",
                index: r,
            });
        }
    }

    /// The **converse** of the oop-map agreement rule: a location the deopt map
    /// reads as a primitive must not be one the oop map lists as holding a live
    /// reference.
    ///
    /// `docs/jit/deopt-metadata.md` §3 states the forward direction (a
    /// reference the deopt map names must be one the GC updates) and then says
    /// the converse — an oop-map slot the deopt map does not name — is *not* an
    /// error, because the GC may legitimately track a spilled temporary the
    /// interpreter frame does not resume from. That remains true, and this is
    /// not that case: here the deopt map **does** name the word, and names it
    /// as an `int`/`long`/`float`/`double`. One machine word at one safepoint
    /// holds one value, so the two maps cannot both be right, and either
    /// reading is unsound — the resume hands the interpreter a truncated `int`
    /// where a live reference belongs (and drops the oop from the frame's root
    /// set), or the collector relocates against a word that is not a pointer.
    ///
    /// Sound on the IR backend by construction rather than by luck: `plan_slots`
    /// keeps disjoint `Ref` and `Prim` free lists, so "a colour is `Ref` or
    /// `Prim` from its first assignment and never changes class"
    /// (`jit/src/ir_lower.rs`, *Reference / primitive separation*). This lane is
    /// the tripwire on that invariant, and a real check for any backend wired in
    /// later whose deopt map and oop map are built from different sources — which
    /// `jit/src/x64.rs` is (`local_oop_masks`/`stack_oop_marks` vs. the register
    /// allocator's live sets).
    ///
    /// Gated on `moving_young_coverage_complete` for the same reason the forward
    /// lane is: a map that does not claim completeness is a conservative
    /// over-approximation the collector already refuses to move against, so a
    /// disagreement there is not evidence of a defect.
    #[allow(clippy::too_many_arguments)]
    fn check_primitive_not_in_oop_map(
        &self,
        at: &SlotRef,
        coverage: Option<&OopCoverage>,
        bank: &'static str,
        location: i32,
        kind: &'static str,
        covered: impl Fn(&OopCoverage) -> bool,
        out: &mut Vec<DeoptMetadataError>,
    ) {
        let Some(cov) = coverage else { return };
        if !cov.moving_young_coverage_complete {
            return;
        }
        if covered(cov) {
            out.push(DeoptMetadataError::PrimitiveSlotCoveredByOopMap {
                at: at.clone(),
                bank,
                location,
                kind,
            });
        }
    }
}

/// Per-scope accumulators shared by every [`DeoptVerifier::check_value`] call
/// for one scope.
///
/// Grouped into a struct rather than passed as five `&mut` parameters because
/// the set grew past what a reader can keep straight at a call site, and every
/// member has the same lifetime: one scope of one deopt point.
#[derive(Default)]
struct ScopeCheck {
    /// Virtual-object ids *defined* (by a `VirtualObject`) in this scope.
    defined: FxHashSet<usize>,
    /// `VirtualObjectRef` edges, validated after the whole scope is walked (a
    /// reference may legally precede its definition).
    referenced: Vec<(SlotRef, usize)>,
    /// Any reference-typed slot was seen, so this scope needs an oop map.
    needs_oop_map: bool,
    /// Frame word (`*(rbp + off)`) → the first descriptor that named it and the
    /// JVM category that descriptor claimed. Drives
    /// [`DeoptMetadataError::SlotTypeConflict`].
    word_types: FxHashMap<i32, (SlotRef, &'static str)>,
}

/// Why `object` cannot serve as a held monitor, or `None` if it can.
///
/// A monitor entry is replayed as a `monitorenter` on resume and balanced by
/// one `monitorexit` at method exit, so the recorded value has to be something
/// the interpreter can lock. Two shapes cannot be:
///
/// * a **primitive** descriptor — the resume builds a `Value::Int`/`Long`/…,
///   the exit tries to unlock it, and the real monitor is never released. The
///   symptom is a hang in an unrelated thread, arbitrarily later.
/// * an **explicit null** (`Object(0)`) — `monitorenter` on null throws, so no
///   correct emitter records one; a null here means the emitter lost the
///   object, not that the program locked nothing.
///
/// `Undefined`, `Unsupported` and `MaterializationRequired` are already
/// reported by the surrounding [`DeoptMetadataError::UnbalancedMonitor`] arm,
/// so they are deliberately not repeated here.
fn monitor_object_defect(object: &FrameValue) -> Option<String> {
    match object {
        FrameValue::Object(0) => Some(
            "the recorded object is null, and `monitorenter` on null throws rather than locking"
                .to_string(),
        ),
        FrameValue::Object(_)
        | FrameValue::StackSlotRef(_)
        | FrameValue::RegisterRef(_)
        | FrameValue::VirtualObject(_)
        | FrameValue::VirtualObjectRef(_)
        | FrameValue::Undefined
        | FrameValue::Unsupported
        | FrameValue::MaterializationRequired(_) => None,
        other => Some(format!(
            "{other:?} is a primitive descriptor, so the resume would `monitorenter` a \
             non-reference and the matching `monitorexit` would never release the real lock"
        )),
    }
}

/// The verifier over the interned representation.
///
/// Deliberately *not* a second implementation of the rules: each interned
/// point is materialized through the compatibility layer and handed to the
/// exact same [`DeoptVerifier::violations`]. Two checkers that were supposed
/// to agree and drifted would be a worse defect than the materialization
/// these methods pay for — this runs once per compile, on the install path.
impl DeoptVerifier {
    /// [`Self::violations`] for interned points.
    ///
    /// A handle the interner never issued is itself a violation
    /// ([`DeoptMetadataError::UnknownFrameStateHandle`]) rather than a silently
    /// skipped point: an unreadable scope chain means nothing about that deopt
    /// point was checked, which must not read as "clean".
    pub fn violations_interned(
        &self,
        interner: &FrameStateInterner,
        points: &[InternedDeoptPoint],
    ) -> Vec<DeoptMetadataError> {
        let mut owned = Vec::with_capacity(points.len());
        let mut out = Vec::new();
        for point in points {
            match interner.materialize_point(point) {
                Some(p) => owned.push(p),
                // `materialize` refuses for two different reasons and they are
                // different findings. A handle the interner never issued means
                // nothing about the point is readable; a chain past the cap
                // means the point is readable but *undescribable*, and saying
                // "unknown handle" would send a reader looking for the wrong
                // bug. Distinguished by whether the root scope resolves.
                None if interner.scope(point.frame_state).is_some() => {
                    out.push(DeoptMetadataError::ScopeChainTooDeep {
                        native_offset: point.native_offset,
                        cap: MAX_SCOPE_CHAIN,
                    })
                }
                None => out.push(DeoptMetadataError::UnknownFrameStateHandle {
                    native_offset: point.native_offset,
                    handle: point.frame_state.index(),
                }),
            }
        }
        out.extend(self.violations(&owned));
        out
    }

    /// [`Self::verify`] for interned points: `Ok(())` or a [`Bailout`].
    pub fn verify_interned(
        &self,
        interner: &FrameStateInterner,
        points: &[InternedDeoptPoint],
    ) -> CompileResult<()> {
        let errors = self.violations_interned(interner, points);
        if errors.is_empty() {
            return Ok(());
        }
        Err(deopt_metadata_bailout(&errors))
    }
}

/// Render a violation list as a compilation [`Bailout`].
///
/// Uses [`BailoutReason::DeoptMetadata`] (its own category in `bailout.rs`,
/// so these are countable separately from IR-verification rejections) with the
/// context `phase=install`, because that is the only point at which this runs:
/// over emitted metadata, before the artifact becomes a `CompiledMethod`.
fn deopt_metadata_bailout(errors: &[DeoptMetadataError]) -> Bailout {
    let shown = errors.len().min(MAX_REPORTED_DEOPT_VIOLATIONS);
    let mut msg = format!("{} deopt-metadata violation(s): ", errors.len());
    let rendered: Vec<String> = errors[..shown].iter().map(|e| e.to_string()).collect();
    msg.push_str(&rendered.join("; "));
    if errors.len() > shown {
        msg.push_str(&format!(" … and {} more", errors.len() - shown));
    }
    Bailout::with_context(
        BailoutReason::DeoptMetadata(msg),
        "phase=install".to_string(),
    )
}

/// Convenience wrapper: verify `points` with a verifier built from `methods`
/// and `oop_maps`.
///
/// The shape an install site wants — one call, `CompileResult<()>`, bail on
/// failure — without having to know the builder API.
pub fn verify_deopt_metadata(
    points: &[DeoptimizationPoint],
    methods: impl IntoIterator<Item = MethodFrameLimits>,
    oop_maps: impl IntoIterator<Item = (u32, OopCoverage)>,
) -> CompileResult<()> {
    let mut verifier = DeoptVerifier::new();
    for m in methods {
        verifier = verifier.with_method(m);
    }
    for (off, cov) in oop_maps {
        verifier = verifier.with_oop_map(off, cov);
    }
    verifier.verify(points)
}

#[cfg(test)]
mod deopt_metadata_tests {
    use super::*;

    const M: &str = "T.m:(I)I";

    fn limits() -> MethodFrameLimits {
        // 32 bytes of code, 3 locals, 2 stack.
        MethodFrameLimits::new(M, 32, 3, 2)
    }

    /// A well-formed point: bci in range, counts within limits, the one
    /// reference local ([rbp-40]) covered by the oop map.
    fn good_point() -> DeoptimizationPoint {
        DeoptimizationPoint {
            native_offset: 0x40,
            bci: 12,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: M.to_string(),
                bci: 12,
                locals: vec![FrameValue::StackSlotRef(-40), FrameValue::Int(7)],
                stack: vec![FrameValue::StackSlot(-48)],
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        }
    }

    fn verifier() -> DeoptVerifier {
        DeoptVerifier::new()
            .with_method(limits())
            .with_oop_map(0x40, OopCoverage::complete([40]))
    }

    fn rendered(errors: &[DeoptMetadataError]) -> String {
        errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn well_formed_metadata_passes() {
        let v = verifier();
        let points = [good_point()];
        assert!(
            v.violations(&points).is_empty(),
            "{}",
            rendered(&v.violations(&points))
        );
        assert!(v.verify(&points).is_ok());
    }

    #[test]
    fn bci_past_the_end_of_the_method_is_rejected() {
        let mut p = good_point();
        p.bci = 99;
        p.frame_state.bci = 99;
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::BciOutOfRange {
                    bci: 99,
                    code_len: 32,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
        let msg = rendered(&errs);
        assert!(msg.contains("resume bci 99"), "{msg}");
        assert!(msg.contains("32-byte code"), "{msg}");
    }

    #[test]
    fn more_locals_than_max_locals_is_rejected() {
        let mut p = good_point();
        p.frame_state.locals = vec![FrameValue::Int(0); 4]; // max_locals is 3
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::LocalCountMismatch {
                    found: 4,
                    max_locals: 3,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("max_locals is 3"));
    }

    #[test]
    fn deeper_stack_than_max_stack_is_rejected() {
        let mut p = good_point();
        p.frame_state.stack = vec![FrameValue::Int(0); 3]; // max_stack is 2
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::StackCountMismatch {
                    found: 3,
                    max_stack: 2,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
    }

    /// The moving-GC agreement rule: a reference the deopt map will hand the
    /// interpreter must be a reference the collector knows to update. A
    /// `StackSlotRef` whose word the oop map does not name is a stale-pointer
    /// bug waiting for the first relocating young collection.
    #[test]
    fn reference_slot_absent_from_the_oop_map_is_rejected() {
        let mut p = good_point();
        // The oop map covers [rbp-40]; move the reference to [rbp-56].
        p.frame_state.locals[0] = FrameValue::StackSlotRef(-56);
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::ReferenceNotInOopMap {
                    frame_offset: 56,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
        let msg = rendered(&errs);
        assert!(msg.contains("[rbp-56]"), "{msg}");
        assert!(msg.contains("local[0]"), "must name the slot: {msg}");
        assert!(msg.contains("stale address"), "{msg}");
    }

    /// A register-resident reference is checked against the same map.
    #[test]
    fn reference_register_absent_from_the_oop_map_is_rejected() {
        let mut p = good_point();
        p.frame_state.locals[0] = FrameValue::RegisterRef(9);
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::ReferenceRegisterNotInOopMap { reg: 9, .. })
            ),
            "{}",
            rendered(&errs)
        );
    }

    /// An oop map that does NOT claim moving-young completeness forces a
    /// non-moving cycle for this frame, so an uncovered reference cannot go
    /// stale — the lane must not report it (that would be a false positive on
    /// every conservatively-handled frame).
    #[test]
    fn incomplete_coverage_does_not_flag_uncovered_references() {
        let mut p = good_point();
        p.frame_state.locals[0] = FrameValue::StackSlotRef(-56);
        let v = DeoptVerifier::new().with_method(limits()).with_oop_map(
            0x40,
            OopCoverage {
                frame_slot_offsets: vec![40],
                registers: Vec::new(),
                moving_young_coverage_complete: false,
            },
        );
        assert!(v.violations(&[p]).is_empty());
    }

    #[test]
    fn reference_slots_without_any_oop_map_are_rejected_when_required() {
        let p = good_point();
        let v = DeoptVerifier::new()
            .with_method(limits())
            .requiring_oop_map(true);
        let errs = v.violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::MissingOopMap {
                    native_offset: 0x40,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
    }

    /// A slot that names an IR node the optimizer removed, without that node
    /// being registered as materializable, rebuilds the slot from nothing.
    #[test]
    fn slot_naming_a_removed_node_is_rejected() {
        let mut p = good_point();
        p.frame_state.locals[0] = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 17, // IR node 17
            class_id: 5,
            num_fields: 1,
            field_values: vec![FrameValue::Int(3)],
        });
        let v = verifier().with_removed_nodes([17u32]);
        let errs = v.violations(&[p.clone()]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::SlotNamesRemovedNode { node: 17, .. })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("rebuild this slot from nothing"));

        // The same node, registered as materializable (it has a recipe), is fine.
        let ok = verifier()
            .with_removed_nodes([17u32])
            .with_materializable_nodes([17u32]);
        assert!(ok.violations(&[p]).is_empty());
    }

    #[test]
    fn unbalanced_lock_state_is_rejected() {
        // depth 0 — a lock nobody holds.
        let mut p = good_point();
        p.frame_state.monitors = vec![MonitorInfo {
            object: FrameValue::StackSlotRef(-40),
            lock_depth: 0,
            relock: true,
        }];
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::UnbalancedMonitor { .. })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("lock_depth is 0"));

        // The same object recorded twice instead of once at depth 2.
        let mut p2 = good_point();
        p2.frame_state.monitors = vec![
            MonitorInfo {
                object: FrameValue::StackSlotRef(-40),
                lock_depth: 1,
                relock: true,
            },
            MonitorInfo {
                object: FrameValue::StackSlotRef(-40),
                lock_depth: 1,
                relock: true,
            },
        ];
        let errs2 = verifier().violations(&[p2]);
        assert!(
            rendered(&errs2).contains("already held by an earlier entry"),
            "{}",
            rendered(&errs2)
        );

        // A monitor whose object cannot be reconstructed cannot be unlocked.
        let mut p3 = good_point();
        p3.frame_state.monitors = vec![MonitorInfo {
            object: FrameValue::MaterializationRequired(EliminatedValue::new(
                4,
                EliminationCause::ElidedLock,
            )),
            lock_depth: 1,
            relock: true,
        }];
        let errs3 = verifier().violations(&[p3]);
        assert!(
            rendered(&errs3).contains("cannot be reconstructed"),
            "{}",
            rendered(&errs3)
        );
    }

    #[test]
    fn virtual_object_graph_integrity_is_checked() {
        // Field count disagreement.
        let mut p = good_point();
        p.frame_state.locals[0] = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 3,
            class_id: 1,
            num_fields: 2,
            field_values: vec![FrameValue::Int(1)],
        });
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::VirtualObjectFieldCountMismatch {
                    declared: 2,
                    found: 1,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );

        // A dangling reference edge.
        let mut p2 = good_point();
        p2.frame_state.locals[0] = FrameValue::VirtualObjectRef(9);
        let errs2 = verifier().violations(&[p2]);
        assert!(
            matches!(
                errs2.first(),
                Some(DeoptMetadataError::UndefinedVirtualObjectRef { id: 9, .. })
            ),
            "{}",
            rendered(&errs2)
        );

        // A forward reference (ref in a local, definition later on the stack)
        // is legal — the materializer allocates every shell before wiring.
        let mut p3 = good_point();
        p3.frame_state.locals[0] = FrameValue::VirtualObjectRef(4);
        p3.frame_state.stack[0] = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 4,
            class_id: 1,
            num_fields: 0,
            field_values: Vec::new(),
        });
        assert!(verifier().violations(&[p3]).is_empty());

        // Defining the same object twice in one scope is not.
        let mut p4 = good_point();
        let vo = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 4,
            class_id: 1,
            num_fields: 0,
            field_values: Vec::new(),
        });
        p4.frame_state.locals[0] = vo.clone();
        p4.frame_state.stack[0] = vo;
        let errs4 = verifier().violations(&[p4]);
        assert!(
            matches!(
                errs4.first(),
                Some(DeoptMetadataError::DuplicateVirtualObjectDefinition { id: 4, .. })
            ),
            "{}",
            rendered(&errs4)
        );
    }

    #[test]
    fn inlined_caller_scopes_are_checked_too() {
        let mut p = good_point();
        p.frame_state.caller = Some(Box::new(FrameState {
            method_key: "T.outer:()V".to_string(),
            bci: 500, // past the caller's code length
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        }));
        let v = verifier().with_method(MethodFrameLimits::new("T.outer:()V", 20, 1, 1));
        let errs = v.violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::BciOutOfRange { scope_depth: 1, .. })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("T.outer:()V"));
    }

    #[test]
    fn a_scope_naming_an_unregistered_method_is_rejected() {
        let mut p = good_point();
        p.frame_state.method_key = "Other.x:()V".to_string();
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(errs.first(), Some(DeoptMetadataError::UnknownMethod { .. })),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("unverifiable"));
    }

    #[test]
    fn out_of_range_register_descriptors_are_rejected() {
        let mut p = good_point();
        p.frame_state.locals[1] = FrameValue::Register(31);
        p.frame_state.stack[0] = FrameValue::XmmDouble(20);
        let errs = verifier().violations(&[p]);
        assert_eq!(errs.len(), 2, "{}", rendered(&errs));
        assert!(rendered(&errs).contains("gpr[31]"));
        assert!(rendered(&errs).contains("xmm[20]"));
    }

    #[test]
    fn a_baked_heap_address_is_rejected() {
        let mut p = good_point();
        p.frame_state.locals[1] = FrameValue::Object(0x7f00_1234);
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::BakedObjectAddress { .. })
            ),
            "{}",
            rendered(&errs)
        );
        // A null constant is fine — nothing to update.
        let mut p2 = good_point();
        p2.frame_state.locals[1] = FrameValue::Object(0);
        assert!(verifier().violations(&[p2]).is_empty());
    }

    #[test]
    fn unsorted_points_break_the_binary_search_and_are_rejected() {
        let mut a = good_point();
        a.native_offset = 0x80;
        let mut b = good_point();
        b.native_offset = 0x40;
        let v = DeoptVerifier::new();
        let errs = v.violations(&[a, b]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::DeoptPointsUnsorted {
                    first: 0x80,
                    second: 0x40
                })
            ),
            "{}",
            rendered(&errs)
        );
    }

    #[test]
    fn point_bci_must_match_its_frame_state_bci() {
        let mut p = good_point();
        p.bci = 13; // frame_state.bci is 12
        let errs = verifier().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::PointBciMismatch { .. })
            ),
            "{}",
            rendered(&errs)
        );
    }

    #[test]
    fn verify_returns_a_bailout_naming_every_violation() {
        let mut p = good_point();
        p.bci = 99;
        p.frame_state.bci = 99;
        p.frame_state.locals = vec![FrameValue::Int(0); 4];
        let err = verifier().verify(&[p]).unwrap_err();
        assert_eq!(err.category(), "deopt_metadata");
        assert_eq!(err.context.as_deref(), Some("phase=install"));
        let s = err.to_string();
        assert!(s.contains("2 deopt-metadata violation(s)"), "{s}");
        assert!(s.contains("resume bci 99"), "{s}");
        assert!(s.contains("max_locals is 3"), "{s}");
    }

    #[test]
    fn free_function_wrapper_matches_the_builder() {
        let points = [good_point()];
        assert!(verify_deopt_metadata(
            &points,
            [limits()],
            [(0x40u32, OopCoverage::complete([40]))],
        )
        .is_ok());
    }

    // ── eliminated vs. undefined ─────────────────────────────────────

    /// The distinction that keeps a scalar-replaced object from silently
    /// reconstructing as `null`: `Undefined` is resumable (the sink maps it to
    /// `Int(0)` because nothing reads the slot), `MaterializationRequired` is
    /// not (the sink refuses, forcing a safe re-run), and the two are never
    /// equal — so a producer that emits one can never be mistaken for the other.
    #[test]
    fn eliminated_and_undefined_are_distinct_states() {
        let eliminated = FrameValue::MaterializationRequired(EliminatedValue::allocation(
            12,
            77,
            EliminationCause::ScalarReplacedObject,
        ));
        assert_ne!(eliminated, FrameValue::Undefined);
        assert_ne!(eliminated, FrameValue::Unsupported);

        let undefined_frame = FrameState {
            method_key: M.to_string(),
            bci: 0,
            locals: vec![FrameValue::Undefined],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let eliminated_frame = FrameState {
            method_key: M.to_string(),
            bci: 0,
            locals: vec![eliminated.clone()],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        assert!(
            frame_state_is_resumable(&undefined_frame),
            "a genuinely undefined slot must stay resumable"
        );
        assert!(
            !frame_state_is_resumable(&eliminated_frame),
            "an eliminated-but-unrebuildable slot must NOT resume as a value"
        );
        assert_eq!(count_materialization_required(&undefined_frame), 0);
        assert_eq!(count_materialization_required(&eliminated_frame), 1);
    }

    /// The marker survives machine-state resolution unchanged — softening it to
    /// `Undefined` (or to a machine read) anywhere in the pipeline restores the
    /// silent-null reconstruction.
    #[test]
    fn eliminated_marker_round_trips_through_reconstruction() {
        let eliminated = FrameValue::MaterializationRequired(EliminatedValue::new(
            3,
            EliminationCause::NestedVirtualObject,
        ));
        let dp = DeoptimizationPoint {
            native_offset: 0,
            bci: 4,
            reason: DeoptReason::UncommonTrap,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: M.to_string(),
                bci: 4,
                locals: vec![eliminated.clone(), FrameValue::Undefined],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        let regs = SavedRegisters::default();
        let rf = reconstruct_frame_from_machine_state(&dp, &regs, 0);
        assert_eq!(rf.locals[0], eliminated);
        assert_eq!(rf.locals[1], FrameValue::Undefined);
        // …and through the constant-only reconstruction path as well.
        let rf2 = reconstruct_frame(&dp);
        assert_eq!(rf2.locals[0], eliminated);
    }

    /// A `MaterializationRequired` field inside an otherwise-complete virtual
    /// object poisons the whole object: materializing it would store a null
    /// into a field that held a live reference.
    #[test]
    fn an_unrebuildable_field_makes_the_virtual_object_unresumable() {
        let fs = FrameState {
            method_key: M.to_string(),
            bci: 0,
            locals: vec![FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: None,
                id: 1,
                class_id: 2,
                num_fields: 2,
                field_values: vec![
                    FrameValue::Int(4),
                    FrameValue::MaterializationRequired(EliminatedValue::unknown(
                        EliminationCause::EliminatedStore,
                    )),
                ],
            })],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        assert!(!frame_state_is_resumable(&fs));
        assert_eq!(count_materialization_required(&fs), 1);
        // The object itself is still a well-formed *description* — the verifier
        // reports no violation, because refusing to resume is the correct,
        // already-safe outcome. Only the resumability predicate rejects it.
        assert!(DeoptVerifier::new()
            .violations(&[DeoptimizationPoint {
                native_offset: 0,
                bci: 0,
                reason: DeoptReason::UncommonTrap,
                action: DeoptAction::Reinterpret,
                speculation_id: 0,
                frame_state: fs,
                semantics: ResumeSemantics::REEXECUTE,
            }])
            .is_empty());
    }

    /// `EliminationCause` reaches the report: the rendered marker names the
    /// pass that deleted the value and the node it deleted.
    #[test]
    fn eliminated_value_renders_its_cause_and_producer() {
        let ev = EliminatedValue::allocation(21, 9, EliminationCause::ScalarReplacedObject);
        let s = ev.to_string();
        assert!(s.contains("scalar-replaced object"), "{s}");
        assert!(s.contains("n21"), "{s}");
        assert!(s.contains("class_id=9"), "{s}");
        assert_eq!(
            EliminatedValue::unknown(EliminationCause::Unclassified).to_string(),
            "unclassified elimination"
        );
    }

    /// A null `DeoptimizationPoint` must not be dereferenced inside the
    /// trampoline: the entry stashes the `u32::MAX` re-run sentinel so the VM
    /// takes the safe whole-method path and never reads the `i64::MIN` return
    /// as a legitimate `Long.MIN_VALUE`.
    #[test]
    fn ir_deopt_entry_survives_a_null_point() {
        let _ = take_last_deopt();
        assert_eq!(
            ir_deopt_entry(std::ptr::null(), 0, std::ptr::null()),
            i64::MIN
        );
        let frame = take_last_deopt().expect("null point stashes the re-run sentinel");
        assert_eq!(frame.bci, u32::MAX);
        assert!(frame.method_key.is_empty());
    }

    /// The runtime resolver must not panic on a malformed register descriptor:
    /// it degrades to `Unsupported` (refuse + safe re-run) and the structured
    /// error is available to the checked path.
    #[test]
    fn out_of_range_register_resolves_to_unsupported_instead_of_panicking() {
        let regs = SavedRegisters::default();
        assert_eq!(
            resolve_value(&FrameValue::Register(200), &regs, 0),
            FrameValue::Unsupported
        );
        assert_eq!(
            try_resolve_value(&FrameValue::XmmFloat(99), &regs, 0),
            Err(DeoptMetadataError::RegisterFileIndexOutOfRange {
                bank: "xmm",
                index: 99,
            })
        );
    }
}

/// The soundness lanes added by the deopt-metadata audit
/// (`deopt-metadata-audit.md`). Each test fails against the verifier
/// as it stood before that audit: every construct below passed verification.
#[cfg(test)]
mod deopt_metadata_soundness_tests {
    use super::*;

    const M: &str = "T.m:(I)I";

    fn limits() -> MethodFrameLimits {
        MethodFrameLimits::new(M, 32, 3, 2)
    }

    /// A point with no reference slots and no oop-map dependency, so each test
    /// below introduces exactly one defect.
    fn plain_point() -> DeoptimizationPoint {
        DeoptimizationPoint {
            native_offset: 0x40,
            bci: 12,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: M.to_string(),
                bci: 12,
                locals: vec![FrameValue::Int(7)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        }
    }

    fn scoped() -> DeoptVerifier {
        DeoptVerifier::new().with_method(limits())
    }

    fn rendered(errors: &[DeoptMetadataError]) -> String {
        errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    // ── frame-slot addressing ────────────────────────────────────────

    /// Every producer encodes a frame slot as the negation of a strictly
    /// positive `[rbp - spill]` offset, so a non-negative offset addresses the
    /// saved frame pointer (`0`) or the caller's frame. Reading a local from
    /// there reconstructs a value that looks entirely plausible and is not the
    /// program's.
    #[test]
    fn a_frame_slot_at_or_above_rbp_is_rejected() {
        for bad in [
            FrameValue::StackSlot(0),
            FrameValue::StackSlot(16),
            FrameValue::StackSlotLong(8),
            FrameValue::StackSlotFloat(0),
            FrameValue::StackSlotDouble(24),
            FrameValue::StackSlotRef(0),
        ] {
            let mut p = plain_point();
            p.frame_state.locals[0] = bad.clone();
            let errs = scoped().violations(&[p]);
            assert!(
                errs.iter()
                    .any(|e| matches!(e, DeoptMetadataError::FrameSlotOutsideFrame { .. })),
                "{bad:?} must be refused: {}",
                rendered(&errs)
            );
        }
    }

    #[test]
    fn an_ordinary_negative_frame_slot_is_accepted() {
        let mut p = plain_point();
        p.frame_state.locals[0] = FrameValue::StackSlot(-40);
        assert!(scoped().violations(&[p]).is_empty());
    }

    /// The message names the word, not just "some slot is wrong".
    #[test]
    fn the_outside_frame_message_names_the_slot_and_the_offset() {
        let mut p = plain_point();
        p.frame_state.stack = vec![FrameValue::StackSlotRef(8)];
        let msg = rendered(&scoped().violations(&[p]));
        assert!(msg.contains("stack[0]"), "{msg}");
        assert!(msg.contains("rbp+8"), "{msg}");
    }

    // ── one word, one type ───────────────────────────────────────────

    /// A machine word holds one value at one program point. Describing it as a
    /// reference in one slot and an `int` in another is an internal
    /// contradiction, and it is exactly the shape that reconstructs an `int`
    /// into a slot the interpreter reads as a reference.
    #[test]
    fn one_word_described_as_two_types_is_rejected() {
        let mut p = plain_point();
        p.frame_state.locals = vec![FrameValue::StackSlotRef(-40)];
        p.frame_state.stack = vec![FrameValue::StackSlot(-40)];
        let errs = DeoptVerifier::new().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::SlotTypeConflict {
                    offset: -40,
                    first_kind: "ref",
                    second_kind: "int",
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
        let msg = rendered(&errs);
        assert!(msg.contains("local[0]"), "must name the first slot: {msg}");
        assert!(msg.contains("stack[0]"), "must name the second slot: {msg}");
    }

    /// Sharing a word is legal — `aload_0` leaves local 0 and stack[0] naming
    /// the same spill — as long as the two agree on its type.
    #[test]
    fn the_same_word_named_twice_with_the_same_type_is_fine() {
        let mut p = plain_point();
        p.frame_state.locals = vec![FrameValue::StackSlotRef(-40)];
        p.frame_state.stack = vec![FrameValue::StackSlotRef(-40)];
        assert!(DeoptVerifier::new().violations(&[p]).is_empty());
    }

    /// Cat-1 and cat-2 are different types even though both are "not a
    /// reference": a `long` read as an `int` loses the upper 32 bits and the
    /// second local slot.
    #[test]
    fn an_int_and_a_long_at_one_word_are_a_conflict() {
        let mut p = plain_point();
        p.frame_state.locals = vec![FrameValue::StackSlot(-40)];
        p.frame_state.stack = vec![FrameValue::StackSlotLong(-40)];
        let errs = DeoptVerifier::new().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::SlotTypeConflict {
                    first_kind: "int",
                    second_kind: "long",
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
    }

    // ── the converse oop-map direction ───────────────────────────────

    /// The forward rule (a reference the deopt map names must be one the GC
    /// updates) had a hole in the other direction: the deopt map could read a
    /// word as an `int` that the oop map lists as a live reference. One of the
    /// two is wrong and both readings are unsound.
    #[test]
    fn a_primitive_frame_slot_the_oop_map_calls_a_reference_is_rejected() {
        let mut p = plain_point();
        p.frame_state.locals[0] = FrameValue::StackSlot(-40);
        let v = scoped().with_oop_map(0x40, OopCoverage::complete([40]));
        let errs = v.violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::PrimitiveSlotCoveredByOopMap {
                    bank: "frame",
                    location: 40,
                    kind: "int",
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("disagree about the type of one word"));
    }

    #[test]
    fn a_primitive_register_the_oop_map_calls_a_reference_is_rejected() {
        let mut p = plain_point();
        p.frame_state.locals[0] = FrameValue::Register(3);
        let v = scoped().with_oop_map(
            0x40,
            OopCoverage {
                frame_slot_offsets: Vec::new(),
                registers: vec![3],
                moving_young_coverage_complete: true,
            },
        );
        let errs = v.violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::PrimitiveSlotCoveredByOopMap {
                    bank: "gpr",
                    location: 3,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
    }

    /// Same exemption as the forward lane: a map that does not claim
    /// moving-young completeness is a conservative over-approximation the
    /// collector already refuses to relocate against, so a disagreement there
    /// is not evidence of a defect.
    #[test]
    fn an_incomplete_oop_map_does_not_flag_a_primitive_slot() {
        let mut p = plain_point();
        p.frame_state.locals[0] = FrameValue::StackSlot(-40);
        let v = scoped().with_oop_map(
            0x40,
            OopCoverage {
                frame_slot_offsets: vec![40],
                registers: Vec::new(),
                moving_young_coverage_complete: false,
            },
        );
        assert!(v.violations(&[p]).is_empty());
    }

    /// The direction that is *not* an error, restated as a test so it stays
    /// that way: the GC may track a spilled temporary the interpreter frame
    /// does not resume from.
    #[test]
    fn an_oop_map_slot_the_deopt_map_never_names_is_not_an_error() {
        let p = plain_point();
        let v = scoped().with_oop_map(0x40, OopCoverage::complete([40, 48, 56]));
        assert!(v.violations(&[p]).is_empty());
    }

    // ── monitors ─────────────────────────────────────────────────────

    /// A monitor entry replays as `monitorenter` and is balanced by one
    /// `monitorexit` at method exit. An entry naming a primitive descriptor
    /// leaves the real lock held: a hang in an unrelated thread, arbitrarily
    /// later, with nothing pointing back at the deopt.
    #[test]
    fn a_monitor_on_a_primitive_is_rejected() {
        for bad in [
            FrameValue::Int(5),
            FrameValue::Long(5),
            FrameValue::StackSlot(-40),
            FrameValue::StackSlotLong(-40),
            FrameValue::Register(2),
            FrameValue::XmmDouble(1),
        ] {
            let mut p = plain_point();
            p.frame_state.monitors = vec![MonitorInfo {
                object: bad.clone(),
                lock_depth: 1,
                relock: true,
            }];
            let errs = scoped().violations(&[p]);
            assert!(
                errs.iter()
                    .any(|e| matches!(e, DeoptMetadataError::MonitorObjectNotAReference { .. })),
                "monitor on {bad:?} must be refused: {}",
                rendered(&errs)
            );
        }
    }

    /// `monitorenter` on null throws rather than locking, so a null monitor
    /// object means the emitter lost the object — not that the program locked
    /// nothing.
    #[test]
    fn a_monitor_on_null_is_rejected() {
        let mut p = plain_point();
        p.frame_state.monitors = vec![MonitorInfo {
            object: FrameValue::Object(0),
            lock_depth: 1,
            relock: true,
        }];
        let errs = scoped().violations(&[p]);
        assert!(
            errs.iter()
                .any(|e| matches!(e, DeoptMetadataError::MonitorObjectNotAReference { .. })),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("null"));
    }

    /// The reference-shaped forms stay accepted, including a monitor on a
    /// scalar-replaced object that will be materialized before the re-lock.
    #[test]
    fn reference_shaped_monitor_objects_are_accepted() {
        for good in [
            FrameValue::StackSlotRef(-40),
            FrameValue::RegisterRef(4),
            FrameValue::VirtualObjectRef(9),
        ] {
            let mut p = plain_point();
            // A defining occurrence for the `VirtualObjectRef` case.
            p.frame_state.locals[0] = FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: None,
                id: 9,
                class_id: 3,
                num_fields: 0,
                field_values: Vec::new(),
            });
            p.frame_state.monitors = vec![MonitorInfo {
                object: good.clone(),
                lock_depth: 2,
                relock: true,
            }];
            let errs = scoped().violations(&[p]);
            assert!(
                !errs
                    .iter()
                    .any(|e| matches!(e, DeoptMetadataError::MonitorObjectNotAReference { .. })),
                "monitor on {good:?} must be accepted: {}",
                rendered(&errs)
            );
        }
    }

    /// The monitor object goes through the same oop-map agreement lane as a
    /// local: a lock the collector cannot see is re-acquired on a stale address
    /// after a relocating young collection.
    #[test]
    fn a_monitor_object_is_checked_against_the_oop_map() {
        let mut p = plain_point();
        p.frame_state.monitors = vec![MonitorInfo {
            object: FrameValue::StackSlotRef(-64),
            lock_depth: 1,
            relock: true,
        }];
        let v = scoped().with_oop_map(0x40, OopCoverage::complete([40]));
        let errs = v.violations(&[p]);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                DeoptMetadataError::ReferenceNotInOopMap {
                    frame_offset: 64,
                    ..
                }
            )),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("monitor[0]"));
    }

    // ── exception state ──────────────────────────────────────────────

    /// `x64_deopt_entry` routes on `reason`; a resume sink reads `semantics`.
    /// If the two disagree, one of them resumes a frame the other knows is
    /// exceptional — the metadata restatement of "the `finally` block was not
    /// run" and "the `athrow` bci leaked".
    #[test]
    fn a_pending_exception_point_without_rethrow_semantics_is_rejected() {
        let mut p = plain_point();
        p.reason = DeoptReason::PendingException;
        p.semantics = ResumeSemantics::REEXECUTE;
        let errs = scoped().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::ResumeSemanticsMismatch {
                    reason: DeoptReason::PendingException,
                    rethrow: false,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
    }

    #[test]
    fn a_rethrow_point_whose_reason_routes_it_to_the_resume_stash_is_rejected() {
        let mut p = plain_point();
        p.reason = DeoptReason::NullCheck;
        p.semantics = ResumeSemantics::RETHROW;
        let errs = scoped().violations(&[p]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::ResumeSemanticsMismatch { rethrow: true, .. })
            ),
            "{}",
            rendered(&errs)
        );
    }

    /// What every producer stamps today (`ResumeSemantics::for_reason`) passes
    /// for every reason, so the lane is a tripwire on a future producer rather
    /// than a tax on the current ones.
    #[test]
    fn for_reason_agrees_with_the_lane_for_every_reason() {
        for reason in [
            DeoptReason::NullCheck,
            DeoptReason::ClassCheck,
            DeoptReason::BoundsCheck,
            DeoptReason::DivByZero,
            DeoptReason::ReceiverTypeChanged,
            DeoptReason::ClassLoading,
            DeoptReason::UninitializedAccess,
            DeoptReason::TransferToInterpreter,
            DeoptReason::UncommonTrap,
            DeoptReason::SpeculationFailed,
            DeoptReason::NotCompiled,
            DeoptReason::UnreachedCode,
            DeoptReason::OsrExit,
            DeoptReason::PendingException,
        ] {
            let mut p = plain_point();
            p.reason = reason;
            p.semantics = ResumeSemantics::for_reason(reason);
            let errs = scoped().violations(&[p]);
            assert!(
                !errs
                    .iter()
                    .any(|e| matches!(e, DeoptMetadataError::ResumeSemanticsMismatch { .. })),
                "{reason:?}: {}",
                rendered(&errs)
            );
        }
    }

    // ── caller-chain depth ───────────────────────────────────────────

    fn owned_chain(depth: usize) -> FrameState {
        let mut fs = FrameState {
            method_key: String::new(),
            bci: 0,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        for i in 0..depth {
            fs = FrameState {
                method_key: String::new(),
                bci: i as u32 + 1,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: Some(Box::new(fs)),
            };
        }
        fs
    }

    /// The verifier used to walk a caller chain to its end with no bound and
    /// report nothing about the fact that it was absurdly deep. Now the cap is
    /// a finding: stopping at it and saying "clean" would report a verdict for
    /// scopes nobody looked at.
    #[test]
    fn an_over_deep_owned_chain_is_reported_not_silently_accepted() {
        let mut p = plain_point();
        p.frame_state = owned_chain(MAX_SCOPE_CHAIN + 2);
        p.frame_state.bci = p.bci;
        let errs = DeoptVerifier::new().violations(&[p]);
        assert!(
            errs.iter()
                .any(|e| matches!(e, DeoptMetadataError::ScopeChainTooDeep { .. })),
            "{}",
            rendered(&errs)
        );
    }

    #[test]
    fn a_chain_within_the_cap_is_walked_without_complaint() {
        let mut p = plain_point();
        p.frame_state = owned_chain(4);
        p.frame_state.bci = p.bci;
        assert!(DeoptVerifier::new().violations(&[p]).is_empty());
    }

    /// Interning used to cut a chain at the cap and hand back a state whose
    /// outermost kept scope claimed to be the bottom of the stack: every scope
    /// well-formed, `chain_is_resumable` true, and a resume that rebuilds a
    /// call stack the program never had. The cut is now represented.
    #[test]
    fn interning_an_over_deep_chain_marks_the_cut_instead_of_dropping_the_outer_frames() {
        let mut it = FrameStateInterner::new();
        let id = it.intern(&owned_chain(MAX_SCOPE_CHAIN + 3));
        assert!(
            !it.chain_is_resumable(id),
            "a chain that was cut must not read as resumable"
        );
        assert!(
            it.materialize(id).is_none(),
            "materializing a cut chain must refuse rather than return a short stack"
        );
    }

    /// …and a chain that fits is unaffected: it round-trips whole.
    #[test]
    fn a_chain_within_the_cap_still_round_trips_through_interning() {
        let mut it = FrameStateInterner::new();
        let id = it.intern(&owned_chain(6));
        assert!(it.chain_is_resumable(id));
        let back = it
            .materialize(id)
            .expect("a chain within the cap materializes");
        let mut depth = 0;
        let mut cursor = Some(&back);
        while let Some(fs) = cursor {
            depth += 1;
            cursor = fs.caller.as_deref();
        }
        assert_eq!(depth, 7, "6 callers above the innermost scope");
    }

    /// A cut chain reaching the interned verifier is reported as what it is,
    /// not as an unknown handle — the two send a reader looking for different
    /// bugs.
    #[test]
    fn the_interned_verifier_names_an_over_deep_chain_rather_than_an_unknown_handle() {
        let mut it = FrameStateInterner::new();
        let mut p = plain_point();
        p.frame_state = owned_chain(MAX_SCOPE_CHAIN + 3);
        p.frame_state.bci = p.bci;
        let interned = it.intern_point(&p);
        let errs = DeoptVerifier::new().violations_interned(&it, &[interned]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::ScopeChainTooDeep { .. })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("truncated chain"));
    }

    /// The owned and handle-side resumability predicates must agree at the cap.
    /// `frame_state_is_resumable` has always refused there; its handle-side
    /// twin used to stop walking and answer `true`, which is a fail-open in a
    /// compile-time admission gate.
    #[test]
    fn the_two_resumability_predicates_agree_at_the_cap() {
        let deep = owned_chain(MAX_SCOPE_CHAIN + 1);
        assert!(!frame_state_is_resumable(&deep));
        let mut it = FrameStateInterner::new();
        let id = it.intern(&deep);
        assert_eq!(frame_state_is_resumable(&deep), it.chain_is_resumable(id));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> FrameState {
        FrameState {
            method_key: "T.m:()V".to_string(),
            bci: 0,
            locals,
            stack,
            monitors: Vec::new(),
            caller: None,
        }
    }

    /// Regression witness for the 2026-07-31 `unresumable-indy-trap` compile
    /// bail (`x64.rs`, opcode `0xba`). `Unsupported` in ANY live slot -- locals
    /// or operand stack -- makes the frame unresumable; everything the resume
    /// sink can map (including `Undefined`, which becomes `Int(0)`) does not.
    /// Inverting this predicate re-arms the `InternalError: precise
    /// deoptimization unavailable ... refusing side-effecting replay` failure
    /// that took out javac `ClassReader.readInnerClasses` and the whole Spring
    /// AOT in-process-compilation cluster.
    #[test]
    fn frame_state_resumability_tracks_unsupported_slots() {
        assert!(frame_state_is_resumable(&fs(vec![], vec![])));
        assert!(frame_state_is_resumable(&fs(
            vec![
                FrameValue::Object(0),
                FrameValue::Int(3),
                FrameValue::Undefined
            ],
            vec![FrameValue::Object(0x1000), FrameValue::Long(7)],
        )));
        assert!(
            !frame_state_is_resumable(&fs(vec![FrameValue::Unsupported], vec![])),
            "an Unsupported LOCAL must make the frame unresumable"
        );
        assert!(
            !frame_state_is_resumable(&fs(
                vec![FrameValue::Object(0)],
                vec![
                    FrameValue::Object(0x1000),
                    FrameValue::Unsupported,
                    FrameValue::Object(0x2000),
                ],
            )),
            "an Unsupported STACK slot under an indy argument -- the javac \
             ClassReader.readInnerClasses shape -- must make the frame unresumable"
        );
    }

    /// The predicate follows `caller`.
    ///
    /// The predicate used to inspect the innermost scope alone, which was
    /// correct only because no producer built a chain. Its callers are the two
    /// compile-time admission gates (`x64.rs`'s unresumable-indy-trap bail and
    /// `CompiledMethod::osr_exit_policy`), and both must answer "no" for an
    /// artifact whose trap sits under a caller frame nobody can describe —
    /// otherwise a clean innermost scope admits it. `chain_is_resumable`
    /// already existed on the interned side; this is its owned counterpart.
    #[test]
    fn resumability_follows_the_whole_caller_chain() {
        let clean = || fs(vec![FrameValue::Int(1)], vec![]);
        let mut chain = clean();
        chain.caller = Some(Box::new(clean()));
        assert!(
            frame_state_is_resumable(&chain),
            "a clean chain stays resumable"
        );

        for dirty in [
            FrameValue::Unsupported,
            FrameValue::MaterializationRequired(EliminatedValue::unknown(
                EliminationCause::ElidedLock,
            )),
        ] {
            let mut bad_caller = fs(vec![dirty.clone()], vec![]);
            bad_caller.caller = None;
            let mut inner = clean();
            inner.caller = Some(Box::new(bad_caller));
            assert!(
                frame_state_is_resumable(&clean()),
                "precondition: the innermost scope alone is clean"
            );
            assert!(
                !frame_state_is_resumable(&inner),
                "an unrebuildable CALLER slot ({dirty:?}) must sink the whole chain"
            );

            // …at depth 3 as well, not just as an immediate caller.
            let mut deep = fs(vec![dirty.clone()], vec![]);
            for _ in 0..3 {
                let mut next = clean();
                next.caller = Some(Box::new(deep));
                deep = next;
            }
            assert!(!frame_state_is_resumable(&deep));
        }
    }

    /// A caller chain deeper than `MAX_SCOPE_CHAIN` is refused rather than
    /// walked further: that deep is a metadata defect, and refusing costs only
    /// a whole-method re-run.
    #[test]
    fn an_absurdly_deep_chain_is_refused_not_walked() {
        let mut deep = fs(vec![FrameValue::Int(0)], vec![]);
        for _ in 0..(MAX_SCOPE_CHAIN + 4) {
            let mut next = fs(vec![FrameValue::Int(0)], vec![]);
            next.caller = Some(Box::new(deep));
            deep = next;
        }
        assert!(!frame_state_is_resumable(&deep));
    }

    // -- per-bci de-spec registry (Step 9 follow-up c) ---------------------

    /// `DespecRegistry::{insert, contains, count_for}` are per-(method, bci):
    /// a recorded site matches only its own key+bci, an empty key never matches,
    /// and inserts are idempotent.
    #[test]
    fn despec_registry_is_per_method_bci() {
        let registry = DespecRegistry::new();
        let m = "DespecTest$Unique.loop:(I)I";
        assert!(!registry.contains(m, 7));
        registry.insert(m, 7);
        registry.insert(m, 7); // idempotent
        registry.insert(m, 12);
        assert!(registry.contains(m, 7));
        assert!(registry.contains(m, 12));
        assert!(!registry.contains(m, 8), "a different bci must not match");
        assert!(
            !registry.contains("OtherClass.m:()V", 7),
            "a different method must not match"
        );
        assert!(!registry.contains("", 7), "an empty key never matches");
        assert_eq!(registry.count_for(m), 2);
    }

    /// Two VMs, two registries: a verdict recorded by one is invisible to the
    /// other. This is the property the process-global set did not have.
    #[test]
    fn despec_registries_do_not_share_verdicts() {
        let first_vm = DespecRegistry::new();
        let second_vm = DespecRegistry::new();
        first_vm.insert("Shared.m:()V", 3);
        assert!(first_vm.contains("Shared.m:()V", 3));
        assert!(
            !second_vm.contains("Shared.m:()V", 3),
            "a second VM must not inherit the first VM's despeculation"
        );
        assert_eq!(second_vm.count_for("Shared.m:()V"), 0);
    }

    // -- helpers -----------------------------------------------------------

    fn make_event(reason: DeoptReason, bci: u32) -> DeoptEvent {
        DeoptEvent {
            reason,
            action: DeoptAction::Reinterpret,
            bci,
            timestamp_ms: 1000,
            speculation_id: 0,
        }
    }

    fn simple_frame_state() -> FrameState {
        FrameState {
            method_key: "Foo.bar:()V".to_string(),
            bci: 10,
            locals: vec![FrameValue::Int(42), FrameValue::Object(0)],
            stack: vec![FrameValue::Int(7)],
            monitors: Vec::new(),
            caller: None,
        }
    }

    fn simple_deopt_point() -> DeoptimizationPoint {
        DeoptimizationPoint {
            native_offset: 0x100,
            bci: 10,
            reason: DeoptReason::NullCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: simple_frame_state(),
            semantics: ResumeSemantics::REEXECUTE,
        }
    }

    // -- real-frame-deopt type source -------------------------------------

    /// `resolve_value` (via `reconstruct_frame_from_machine_state`) reads a
    /// `StackSlotRef` slot as an `Object` (the raw word IS the heap pointer),
    /// a `StackSlot` slot as an `Int`, a `StackSlotLong` slot as a cat-2 `Long`
    /// (the full 64-bit word), and passes `Unsupported` through — so the resume
    /// can build a real `Value::Object`/`Value::Long` for ref/long slots instead
    /// of a truncated `Value::Int`.
    #[test]
    fn reconstruct_resolves_typed_slots() {
        // A fake native frame. `resolve_value` reads `*(rbp + off)`; the IR
        // convention stores spills below rbp, so we point `rbp` past the buffer
        // and use negative offsets.
        //   buf[0] @ rbp-24 (ref), buf[1] @ rbp-16 (int), buf[2] @ rbp-8 (long).
        let buf: [u64; 4] = [
            0x1111_2222_3333_4444,
            0x0000_0000_DEAD_BEEF,
            0xFEDC_BA98_7654_3210, // a full 64-bit long (high bits set)
            0,
        ];
        let rbp = (&buf[3] as *const u64) as u64;
        let regs = SavedRegisters::default();
        let fs = FrameState {
            method_key: "T.m:()V".to_string(),
            bci: 3,
            locals: vec![
                FrameValue::StackSlotRef(-24),
                FrameValue::StackSlot(-16),
                FrameValue::StackSlotLong(-8),
                FrameValue::Unsupported,
            ],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let dp = DeoptimizationPoint {
            native_offset: 0,
            bci: 3,
            reason: DeoptReason::DivByZero,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: fs,
            semantics: ResumeSemantics::REEXECUTE,
        };
        let rf = reconstruct_frame_from_machine_state(&dp, &regs, rbp);
        assert_eq!(rf.locals[0], FrameValue::Object(0x1111_2222_3333_4444));
        assert_eq!(rf.locals[1], FrameValue::Int(0xDEAD_BEEF));
        // StackSlotLong reads the full 64-bit word (NOT truncated to i32).
        assert_eq!(
            rf.locals[2],
            FrameValue::Long(0xFEDC_BA98_7654_3210u64 as i64)
        );
        assert_eq!(rf.locals[3], FrameValue::Unsupported);
    }

    /// `resolve_value` reads FP slots and XMM-resident FP values precisely: a
    /// `StackSlotFloat`/`XmmFloat` as the low 32 bits (a cat-1 `Float`), a
    /// `StackSlotDouble`/`XmmDouble` as the full 64 bits (a cat-2 `Double`). The
    /// high garbage in the float sources proves the low-32 mask/read — so the
    /// resume builds a real `Value::Float`/`Value::Double`, never a truncated int.
    #[test]
    fn reconstruct_resolves_fp_slots_and_xmm_registers() {
        let float_bits: u32 = 1.5f32.to_bits(); // 0x3FC0_0000
        let double_bits: u64 = std::f64::consts::PI.to_bits();
        // buf[0] @ rbp-16 (float bits in low 32, high garbage), buf[1] @ rbp-8 (double).
        let buf: [u64; 3] = [0xDEAD_BEEF_0000_0000 | float_bits as u64, double_bits, 0];
        let rbp = (&buf[2] as *const u64) as u64;

        let mut regs = SavedRegisters::default();
        regs.xmm[5] = 0xCAFE_F00D_0000_0000 | float_bits as u64; // high garbage masked off
        regs.xmm[9] = double_bits;
        // A cat-2 `long` in GPR 7 — full 64 bits (high bits set, would truncate
        // if mistyped as a cat-1 `Register`).
        regs.gpr[7] = 0xFEDC_BA98_7654_3210;
        // An object reference in GPR 3 — the full pointer word.
        regs.gpr[3] = 0x0000_7F12_3456_7890;

        let fs = FrameState {
            method_key: "T.m:()V".to_string(),
            bci: 0,
            locals: vec![
                FrameValue::StackSlotFloat(-16),
                FrameValue::StackSlotDouble(-8),
                FrameValue::XmmFloat(5),
                FrameValue::XmmDouble(9),
                FrameValue::RegisterLong(7),
                FrameValue::RegisterRef(3),
            ],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let dp = DeoptimizationPoint {
            native_offset: 0,
            bci: 0,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: fs,
            semantics: ResumeSemantics::REEXECUTE,
        };
        let rf = reconstruct_frame_from_machine_state(&dp, &regs, rbp);
        assert_eq!(rf.locals[0], FrameValue::Float(float_bits as u64));
        assert_eq!(rf.locals[1], FrameValue::Double(double_bits));
        assert_eq!(rf.locals[2], FrameValue::Float(float_bits as u64));
        assert_eq!(rf.locals[3], FrameValue::Double(double_bits));
        // RegisterLong keeps all 64 bits (NOT truncated like Register -> Int).
        assert_eq!(
            rf.locals[4],
            FrameValue::Long(0xFEDC_BA98_7654_3210u64 as i64)
        );
        // RegisterRef carries the full heap pointer as an Object (NOT a truncated
        // Int that would also drop it from the GC scan).
        assert_eq!(rf.locals[5], FrameValue::Object(0x0000_7F12_3456_7890));
    }

    // -- register-homed locals vs. a stale frame slot ----------------------
    //
    // These pin the invariant the register allocator's cross-call publication
    // policy depends on: once a local is described by a REGISTER `FrameValue`,
    // reconstruction never consults that local's canonical frame slot. The
    // slot may therefore hold a stale copy — which is exactly what happens if
    // a call site is allowed to skip publishing a primitive register-homed
    // local. See `regalloc::SafepointPublishPlan` and
    // `jit-regalloc-and-deopt.md`.

    /// Every register-homed local reconstructs from `SavedRegisters`, and the
    /// frame slot that would be its canonical home is filled with a *wrong*
    /// value to prove it is never read.
    #[test]
    fn register_homed_locals_ignore_a_stale_canonical_frame_slot() {
        // A fake native frame whose local slots hold DELIBERATELY STALE data:
        // slot for local 0 @ rbp-8, local 1 @ rbp-16, local 2 @ rbp-24 — the
        // `[rbp - (idx+1)*8]` layout the x64 emitter uses.
        let stale: [u64; 4] = [
            0xBAAD_F00D_BAAD_F00D, // rbp-32 (unused)
            0xDEAD_DEAD_DEAD_DEAD, // rbp-24  <- local 2's canonical slot
            0xBEEF_BEEF_BEEF_BEEF, // rbp-16  <- local 1's canonical slot
            0xFACE_FACE_FACE_FACE, // rbp-8   <- local 0's canonical slot
        ];
        let rbp = (stale.as_ptr() as u64) + 32;

        let mut regs = SavedRegisters::default();
        regs.gpr[12] = 41; // local 0: a live int in R12
        regs.gpr[13] = 0x0000_0001_0000_002A; // local 1: a live long in R13
        regs.gpr[14] = 0x0000_7F00_1234_5678; // local 2: a live ref in R14

        let fs = FrameState {
            method_key: "T.fib:(I)I".to_string(),
            bci: 10,
            locals: vec![
                FrameValue::Register(12),
                FrameValue::RegisterLong(13),
                FrameValue::RegisterRef(14),
            ],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let dp = DeoptimizationPoint {
            native_offset: 0,
            bci: 10,
            reason: DeoptReason::UncommonTrap,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: fs,
            semantics: ResumeSemantics::REEXECUTE,
        };
        let rf = reconstruct_frame_from_machine_state(&dp, &regs, rbp);
        assert_eq!(
            rf.locals[0],
            FrameValue::Int(41),
            "a register-homed int must come from the GPR file, not the frame slot"
        );
        assert_eq!(
            rf.locals[1],
            FrameValue::Long(0x0000_0001_0000_002A),
            "a register-homed long must keep all 64 bits from the GPR file"
        );
        assert_eq!(
            rf.locals[2],
            FrameValue::Object(0x0000_7F00_1234_5678),
            "a register-homed ref must resolve as a GC-tracked Object from the GPR file"
        );
        // None of the stale words leaked through.
        for v in &rf.locals {
            match v {
                FrameValue::Int(i) | FrameValue::Long(i) => {
                    assert!(!stale.contains(&(*i as u64)), "stale slot leaked: {v:?}")
                }
                FrameValue::Object(o) => {
                    assert!(!stale.contains(o), "stale slot leaked: {v:?}")
                }
                other => panic!("unexpected reconstruction {other:?}"),
            }
        }
    }

    /// The same value in the same GPR reconstructs to a *different* category
    /// depending on the descriptor variant the snapshot emitter chose. This is
    /// what makes `Register` vs `RegisterLong` vs `RegisterRef` load-bearing:
    /// they select cat-1/cat-2 local placement and GC tracking on resume, not
    /// just a numeric read.
    #[test]
    fn register_descriptor_variant_selects_the_resumed_value_category() {
        let mut regs = SavedRegisters::default();
        regs.gpr[15] = 0x0000_7F55_0000_0007;
        let rbp = 0u64; // never dereferenced — no slot descriptors here
        assert_eq!(
            resolve_value(&FrameValue::Register(15), &regs, rbp),
            FrameValue::Int(0x0000_7F55_0000_0007)
        );
        assert_eq!(
            resolve_value(&FrameValue::RegisterLong(15), &regs, rbp),
            FrameValue::Long(0x0000_7F55_0000_0007)
        );
        assert_eq!(
            resolve_value(&FrameValue::RegisterRef(15), &regs, rbp),
            FrameValue::Object(0x0000_7F55_0000_0007)
        );
    }

    /// Inlined caller frames share the physical frame AND the register file, so
    /// a register-homed local in an inlined *caller* must resolve from the same
    /// `SavedRegisters` — an inliner that raises its budget (the concurrent
    /// work on `jit/src/lib.rs`) deepens exactly this chain.
    #[test]
    fn inlined_caller_frames_resolve_register_locals_from_the_same_regfile() {
        let mut regs = SavedRegisters::default();
        regs.gpr[3] = 7; // RBX — callee-saved, so it survives the inlined call
        regs.gpr[12] = 9;

        let caller = FrameState {
            method_key: "T.outer:()V".to_string(),
            bci: 4,
            locals: vec![FrameValue::Register(3)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let callee = FrameState {
            method_key: "T.inner:()V".to_string(),
            bci: 1,
            locals: vec![FrameValue::Register(12)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: Some(Box::new(caller)),
        };
        let dp = DeoptimizationPoint {
            native_offset: 0,
            bci: 1,
            reason: DeoptReason::SpeculationFailed,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: callee,
            semantics: ResumeSemantics::REEXECUTE,
        };
        let rf = reconstruct_frame_from_machine_state(&dp, &regs, 0);
        assert_eq!(rf.locals[0], FrameValue::Int(9));
        assert_eq!(rf.caller_frames.len(), 1);
        assert_eq!(rf.caller_frames[0].method_key, "T.outer:()V");
        assert_eq!(rf.caller_frames[0].locals[0], FrameValue::Int(7));
    }

    /// A monitor whose object is register-homed must also resolve from the
    /// register file — otherwise a deopt inside a synchronized region rebuilds
    /// the interpreter frame holding the *wrong* monitor and the unlock on
    /// resume targets a different object.
    #[test]
    fn monitors_resolve_register_homed_objects() {
        let mut regs = SavedRegisters::default();
        regs.gpr[14] = 0x0000_7F99_0000_1000;
        let fs = FrameState {
            method_key: "T.sync:()V".to_string(),
            bci: 2,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: vec![MonitorInfo {
                object: FrameValue::RegisterRef(14),
                lock_depth: 1,
                relock: true,
            }],
            caller: None,
        };
        let dp = DeoptimizationPoint {
            native_offset: 0,
            bci: 2,
            reason: DeoptReason::UncommonTrap,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: fs,
            semantics: ResumeSemantics::REEXECUTE,
        };
        let rf = reconstruct_frame_from_machine_state(&dp, &regs, 0);
        assert_eq!(rf.monitors.len(), 1);
        assert_eq!(
            rf.monitors[0].object,
            FrameValue::Object(0x0000_7F99_0000_1000)
        );
        assert_eq!(rf.monitors[0].lock_depth, 1);
    }

    /// `SavedRegisters` is the ABI contract between the x64 deopt stub (which
    /// spills 16 GPRs then 16 XMMs into a contiguous 256-byte region and passes
    /// `&gpr[0]`) and this resolver. A layout change on either side silently
    /// mis-resolves every register-homed value, so pin it here.
    #[test]
    fn saved_registers_layout_matches_the_stub_spill_region() {
        assert_eq!(std::mem::size_of::<SavedRegisters>(), 256);
        let sr = SavedRegisters::default();
        let base = &sr as *const SavedRegisters as usize;
        assert_eq!(
            &sr.gpr[0] as *const u64 as usize, base,
            "gpr[0] must be at the struct base — the stub passes &gpr[0] as the pointer"
        );
        assert_eq!(
            &sr.xmm[0] as *const u64 as usize - base,
            128,
            "the XMM half must start 128 bytes in (16 GPRs x 8 bytes)"
        );
        // Highest indices addressable by a FrameValue register descriptor.
        assert_eq!(sr.gpr.len(), 16);
        assert_eq!(sr.xmm.len(), 16);
    }

    // -- DeoptReason -------------------------------------------------------

    #[test]
    fn deopt_reason_null_check() {
        assert_eq!(DeoptReason::NullCheck, DeoptReason::NullCheck);
    }

    #[test]
    fn deopt_reason_class_check() {
        assert_ne!(DeoptReason::ClassCheck, DeoptReason::NullCheck);
    }

    #[test]
    fn deopt_reason_all_variants_distinct() {
        let variants = [
            DeoptReason::NullCheck,
            DeoptReason::ClassCheck,
            DeoptReason::BoundsCheck,
            DeoptReason::DivByZero,
            DeoptReason::ReceiverTypeChanged,
            DeoptReason::ClassLoading,
            DeoptReason::UninitializedAccess,
            DeoptReason::TransferToInterpreter,
            DeoptReason::UncommonTrap,
            DeoptReason::SpeculationFailed,
            DeoptReason::NotCompiled,
            DeoptReason::UnreachedCode,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn deopt_reason_is_hashable() {
        let mut set = std::collections::HashSet::new();
        set.insert(DeoptReason::NullCheck);
        set.insert(DeoptReason::NullCheck);
        assert_eq!(set.len(), 1);
    }

    // -- DeoptAction -------------------------------------------------------

    #[test]
    fn deopt_action_all_variants() {
        let actions = [
            DeoptAction::Reinterpret,
            DeoptAction::RecompileAndReinterpret,
            DeoptAction::MakeNotEntrant,
            DeoptAction::MakeNotCompilable,
        ];
        assert_eq!(actions.len(), 4);
        assert_ne!(actions[0], actions[1]);
    }

    // -- DeoptimizationPoint -----------------------------------------------

    #[test]
    fn deopt_point_creation() {
        let dp = simple_deopt_point();
        assert_eq!(dp.native_offset, 0x100);
        assert_eq!(dp.bci, 10);
        assert_eq!(dp.reason, DeoptReason::NullCheck);
        assert_eq!(dp.action, DeoptAction::Reinterpret);
        assert_eq!(dp.speculation_id, 0);
    }

    // -- FrameState --------------------------------------------------------

    #[test]
    fn frame_state_locals_and_stack() {
        let fs = simple_frame_state();
        assert_eq!(fs.locals.len(), 2);
        assert_eq!(fs.stack.len(), 1);
        assert_eq!(fs.bci, 10);
    }

    #[test]
    fn frame_state_nested_caller() {
        let outer = FrameState {
            method_key: "Outer.run:()V".to_string(),
            bci: 5,
            locals: vec![FrameValue::Int(1)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let inner = FrameState {
            method_key: "Inner.go:()V".to_string(),
            bci: 20,
            locals: vec![FrameValue::Int(2)],
            stack: vec![FrameValue::Int(3)],
            monitors: Vec::new(),
            caller: Some(Box::new(outer)),
        };
        assert!(inner.caller.is_some());
        assert_eq!(inner.caller.as_ref().unwrap().method_key, "Outer.run:()V");
    }

    // -- FrameValue --------------------------------------------------------

    #[test]
    fn frame_value_int() {
        assert_eq!(FrameValue::Int(42), FrameValue::Int(42));
    }

    #[test]
    fn frame_value_float() {
        let bits = f64::to_bits(3.14);
        assert_eq!(FrameValue::Float(bits), FrameValue::Float(bits));
    }

    #[test]
    fn frame_value_object_null() {
        assert_eq!(FrameValue::Object(0), FrameValue::Object(0));
    }

    #[test]
    fn frame_value_register() {
        assert_eq!(FrameValue::Register(7), FrameValue::Register(7));
        assert_ne!(FrameValue::Register(0), FrameValue::Register(1));
    }

    #[test]
    fn frame_value_stack_slot() {
        assert_eq!(FrameValue::StackSlot(-8), FrameValue::StackSlot(-8));
    }

    #[test]
    fn frame_value_undefined() {
        assert_eq!(FrameValue::Undefined, FrameValue::Undefined);
    }

    // -- VirtualObjectState ------------------------------------------------

    #[test]
    fn virtual_object_state_fields() {
        let vo = VirtualObjectState {
            array_element_type: None,
            id: 0,
            class_id: 42,
            num_fields: 2,
            field_values: vec![FrameValue::Int(1), FrameValue::Object(0)],
        };
        assert_eq!(vo.class_id, 42);
        assert_eq!(vo.num_fields, 2);
        assert_eq!(vo.field_values.len(), 2);
    }

    #[test]
    fn resolve_value_recurses_virtual_object_fields() {
        // Guard-surviving SR: the producer emits a `VirtualObject` whose fields
        // are still machine forms (`StackSlot*`/`Register*`). `resolve_value`
        // must resolve each field from the live machine state so the VM
        // materializer receives concrete values; a `VirtualObjectRef` field (an
        // intra-frame id edge) passes through unchanged.
        let mut regs = SavedRegisters::default();
        regs.gpr[3] = 0x1234; // a RegisterRef field reads this GPR as an object ptr
                              // Stand-in native frame: a StackSlot(off) reads *(rbp + off) as i64.
        let buf: [i64; 4] = [0, 111, 0xBEEFi64, 0];
        let rbp = buf.as_ptr() as u64;
        let vo = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 9,
            class_id: 7,
            num_fields: 4,
            field_values: vec![
                FrameValue::StackSlot(8),        // buf[1] = 111 → Int(111)
                FrameValue::StackSlotRef(16),    // buf[2] = 0xBEEF → Object(0xBEEF)
                FrameValue::RegisterRef(3),      // gpr[3] = 0x1234 → Object(0x1234)
                FrameValue::VirtualObjectRef(5), // intra-frame edge → unchanged
            ],
        });
        match resolve_value(&vo, &regs, rbp) {
            FrameValue::VirtualObject(s) => {
                assert_eq!(s.id, 9);
                assert_eq!(
                    s.field_values,
                    vec![
                        FrameValue::Int(111),
                        FrameValue::Object(0xBEEF),
                        FrameValue::Object(0x1234),
                        FrameValue::VirtualObjectRef(5),
                    ]
                );
            }
            other => panic!("expected VirtualObject, got {other:?}"),
        }
        // A bare ref edge resolves to itself (no machine location).
        assert_eq!(
            resolve_value(&FrameValue::VirtualObjectRef(5), &regs, rbp),
            FrameValue::VirtualObjectRef(5)
        );
    }

    // -- MonitorInfo -------------------------------------------------------

    #[test]
    fn monitor_info_tracking() {
        let mi = MonitorInfo {
            object: FrameValue::Object(0xDEAD),
            lock_depth: 2,
            relock: true,
        };
        assert_eq!(mi.lock_depth, 2);
        assert_eq!(mi.object, FrameValue::Object(0xDEAD));
    }

    // -- DeoptimizationLog -------------------------------------------------

    #[test]
    fn log_new_empty() {
        let log = DeoptimizationLog::new();
        assert_eq!(log.total_deopts(), 0);
        assert_eq!(log.deopt_count("any"), 0);
    }

    #[test]
    fn log_empty_history() {
        let log = DeoptimizationLog::new();
        assert!(log.history("nonexistent").is_empty());
    }

    #[test]
    fn log_record_and_count() {
        let mut log = DeoptimizationLog::new();
        log.record_deopt("Foo.bar", make_event(DeoptReason::NullCheck, 0));
        log.record_deopt("Foo.bar", make_event(DeoptReason::BoundsCheck, 5));
        assert_eq!(log.deopt_count("Foo.bar"), 2);
        assert_eq!(log.total_deopts(), 2);
    }

    #[test]
    fn log_total_deopts_counter() {
        let mut log = DeoptimizationLog::new();
        log.record_deopt("A", make_event(DeoptReason::NullCheck, 0));
        log.record_deopt("B", make_event(DeoptReason::DivByZero, 0));
        log.record_deopt("A", make_event(DeoptReason::NullCheck, 1));
        assert_eq!(log.total_deopts(), 3);
    }

    #[test]
    fn log_should_give_up_after_threshold() {
        let mut log = DeoptimizationLog::new_with_threshold(3);
        assert!(!log.should_give_up("m"));
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 1));
        assert!(!log.should_give_up("m"));
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 2));
        assert!(log.should_give_up("m"));
    }

    #[test]
    fn log_most_common_reason() {
        let mut log = DeoptimizationLog::new();
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
        log.record_deopt("m", make_event(DeoptReason::BoundsCheck, 1));
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 2));
        assert_eq!(log.most_common_reason("m"), Some(DeoptReason::NullCheck));
    }

    #[test]
    fn log_most_common_reason_empty() {
        let log = DeoptimizationLog::new();
        assert_eq!(log.most_common_reason("m"), None);
    }

    #[test]
    fn log_clear_history() {
        let mut log = DeoptimizationLog::new();
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 1));
        assert_eq!(log.deopt_count("m"), 2);
        log.clear_history("m");
        assert_eq!(log.deopt_count("m"), 0);
        // total_deopts is a lifetime counter — not decremented.
        assert_eq!(log.total_deopts(), 2);
    }

    #[test]
    fn log_recommend_action_first_deopt() {
        let log = DeoptimizationLog::new_with_threshold(10);
        assert_eq!(
            log.recommend_action("m", DeoptReason::NullCheck),
            DeoptAction::Reinterpret
        );
    }

    #[test]
    fn log_recommend_action_few_deopts() {
        let mut log = DeoptimizationLog::new_with_threshold(10);
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 1));
        assert_eq!(
            log.recommend_action("m", DeoptReason::NullCheck),
            DeoptAction::RecompileAndReinterpret
        );
    }

    #[test]
    fn log_recommend_action_many_deopts() {
        let mut log = DeoptimizationLog::new_with_threshold(10);
        for i in 0..6 {
            log.record_deopt("m", make_event(DeoptReason::NullCheck, i));
        }
        assert_eq!(
            log.recommend_action("m", DeoptReason::NullCheck),
            DeoptAction::MakeNotEntrant
        );
    }

    #[test]
    fn log_recommend_action_too_many() {
        let mut log = DeoptimizationLog::new_with_threshold(10);
        for i in 0..10 {
            log.record_deopt("m", make_event(DeoptReason::NullCheck, i));
        }
        assert_eq!(
            log.recommend_action("m", DeoptReason::NullCheck),
            DeoptAction::MakeNotCompilable
        );
    }

    #[test]
    fn log_multiple_reasons() {
        let mut log = DeoptimizationLog::new();
        log.record_deopt("m", make_event(DeoptReason::NullCheck, 0));
        log.record_deopt("m", make_event(DeoptReason::BoundsCheck, 1));
        log.record_deopt("m", make_event(DeoptReason::DivByZero, 2));
        let hist = log.history("m");
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].reason, DeoptReason::NullCheck);
        assert_eq!(hist[1].reason, DeoptReason::BoundsCheck);
        assert_eq!(hist[2].reason, DeoptReason::DivByZero);
    }

    #[test]
    fn log_event_timestamp() {
        let event = DeoptEvent {
            reason: DeoptReason::NullCheck,
            action: DeoptAction::Reinterpret,
            bci: 0,
            timestamp_ms: 123456789,
            speculation_id: 7,
        };
        assert_eq!(event.timestamp_ms, 123456789);
        assert_eq!(event.speculation_id, 7);
    }

    // -- reconstruct_frame -------------------------------------------------

    #[test]
    fn reconstruct_frame_basic() {
        let dp = simple_deopt_point();
        let rf = reconstruct_frame(&dp);
        assert_eq!(rf.method_key, "Foo.bar:()V");
        assert_eq!(rf.bci, 10);
        assert_eq!(rf.locals.len(), 2);
        assert_eq!(rf.stack.len(), 1);
        assert!(rf.caller_frames.is_empty());
    }

    #[test]
    fn reconstruct_frame_with_inlined_caller() {
        let outer = FrameState {
            method_key: "Outer.run:()V".to_string(),
            bci: 5,
            locals: vec![FrameValue::Int(1)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let inner = FrameState {
            method_key: "Inner.go:()V".to_string(),
            bci: 20,
            locals: vec![FrameValue::Int(2)],
            stack: vec![FrameValue::Int(3)],
            monitors: Vec::new(),
            caller: Some(Box::new(outer)),
        };
        let dp = DeoptimizationPoint {
            native_offset: 0x200,
            bci: 20,
            reason: DeoptReason::ClassCheck,
            action: DeoptAction::RecompileAndReinterpret,
            speculation_id: 1,
            frame_state: inner,
            semantics: ResumeSemantics::REEXECUTE,
        };
        let rf = reconstruct_frame(&dp);
        assert_eq!(rf.method_key, "Inner.go:()V");
        assert_eq!(rf.caller_frames.len(), 1);
        assert_eq!(rf.caller_frames[0].method_key, "Outer.run:()V");
        assert_eq!(rf.caller_frames[0].bci, 5);
    }

    // -- materialize_virtual_objects / count --------------------------------

    #[test]
    fn materialize_virtual_objects_count() {
        let frame = FrameState {
            method_key: "M".to_string(),
            bci: 0,
            locals: vec![
                FrameValue::VirtualObject(VirtualObjectState {
                    array_element_type: None,
                    id: 0,
                    class_id: 1,
                    num_fields: 1,
                    field_values: vec![FrameValue::Int(10)],
                }),
                FrameValue::Int(5),
            ],
            stack: vec![FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: None,
                id: 1,
                class_id: 2,
                num_fields: 0,
                field_values: Vec::new(),
            })],
            monitors: Vec::new(),
            caller: None,
        };
        assert_eq!(count_virtual_objects(&frame), 2);
        let materialized = materialize_virtual_objects(&frame).expect("test materialization");
        assert_eq!(materialized.len(), 2);
        // First virtual object is at locals index 0
        assert_eq!(materialized[0].0, 0);
        // Second virtual object is at stack index 0
        assert_eq!(materialized[1].0, 0);
        // Addresses are distinct
        assert_ne!(materialized[0].1, materialized[1].1);
    }

    #[test]
    fn count_virtual_objects_none() {
        let frame = FrameState {
            method_key: "M".to_string(),
            bci: 0,
            locals: vec![FrameValue::Int(1)],
            stack: vec![FrameValue::Int(2)],
            monitors: Vec::new(),
            caller: None,
        };
        assert_eq!(count_virtual_objects(&frame), 0);
        assert_eq!(
            materialize_virtual_objects(&frame).expect("empty frame is safe"),
            Vec::<(usize, u64)>::new()
        );
    }

    #[test]
    fn log_recommend_action_receiver_type_changed() {
        let mut log = DeoptimizationLog::new_with_threshold(10);
        log.record_deopt("m", make_event(DeoptReason::ReceiverTypeChanged, 0));
        // ReceiverTypeChanged should aggressively recompile regardless of count.
        assert_eq!(
            log.recommend_action("m", DeoptReason::ReceiverTypeChanged),
            DeoptAction::RecompileAndReinterpret,
        );
    }

    #[test]
    fn log_recommend_action_not_compiled() {
        let log = DeoptimizationLog::new_with_threshold(10);
        // NotCompiled should immediately give up.
        assert_eq!(
            log.recommend_action("m", DeoptReason::NotCompiled),
            DeoptAction::MakeNotCompilable,
        );
    }

    #[test]
    fn log_recommend_action_unreached_code() {
        let log = DeoptimizationLog::new_with_threshold(10);
        assert_eq!(
            log.recommend_action("m", DeoptReason::UnreachedCode),
            DeoptAction::MakeNotCompilable,
        );
    }

    #[test]
    fn log_recommend_action_transfer_to_interpreter() {
        let mut log = DeoptimizationLog::new_with_threshold(10);
        for i in 0..5 {
            log.record_deopt("m", make_event(DeoptReason::TransferToInterpreter, i));
        }
        // TransferToInterpreter always reinterprets regardless of count.
        assert_eq!(
            log.recommend_action("m", DeoptReason::TransferToInterpreter),
            DeoptAction::Reinterpret,
        );
    }

    #[test]
    fn log_recommend_action_speculation_failed_recompiles() {
        let mut log = DeoptimizationLog::new_with_threshold(10);
        log.record_deopt("m", make_event(DeoptReason::SpeculationFailed, 0));
        assert_eq!(
            log.recommend_action("m", DeoptReason::SpeculationFailed),
            DeoptAction::RecompileAndReinterpret,
        );
    }

    #[test]
    fn log_recommend_action_class_check_gives_up_at_threshold() {
        let mut log = DeoptimizationLog::new_with_threshold(3);
        for i in 0..3 {
            log.record_deopt("m", make_event(DeoptReason::ClassCheck, i));
        }
        assert_eq!(
            log.recommend_action("m", DeoptReason::ClassCheck),
            DeoptAction::MakeNotCompilable,
        );
    }

    #[test]
    fn reconstruct_frame_monitors() {
        let fs = FrameState {
            method_key: "Sync.lock:()V".to_string(),
            bci: 3,
            locals: vec![FrameValue::Object(0x1000)],
            stack: Vec::new(),
            monitors: vec![MonitorInfo {
                object: FrameValue::Object(0x1000),
                lock_depth: 1,
                relock: true,
            }],
            caller: None,
        };
        let dp = DeoptimizationPoint {
            native_offset: 0x50,
            bci: 3,
            reason: DeoptReason::TransferToInterpreter,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: fs,
            semantics: ResumeSemantics::REEXECUTE,
        };
        let rf = reconstruct_frame(&dp);
        assert_eq!(rf.monitors.len(), 1);
        assert_eq!(rf.monitors[0].lock_depth, 1);
    }
}

// ---------------------------------------------------------------------------
// Tests: interned, immutable frame states
// ---------------------------------------------------------------------------

#[cfg(test)]
mod frame_state_interning_tests {
    use super::*;

    const M: &str = "T.m:(I)I";

    fn ints(n: usize) -> Vec<FrameValue> {
        (0..n).map(|i| FrameValue::Int(i as i64)).collect()
    }

    fn owned(bci: u32, locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> FrameState {
        FrameState {
            method_key: M.to_string(),
            bci,
            locals,
            stack,
            monitors: Vec::new(),
            caller: None,
        }
    }

    /// An owned caller chain `depth` levels deep above the innermost scope.
    /// Level 0 is the trapping scope; level `depth` is the outermost caller.
    fn owned_chain(depth: usize) -> FrameState {
        let mut built: Option<Box<FrameState>> = None;
        for level in (0..=depth).rev() {
            built = Some(Box::new(FrameState {
                method_key: format!("T.level{level}:()V"),
                bci: level as u32 + 1,
                locals: vec![FrameValue::Int(level as i64), FrameValue::Undefined],
                stack: vec![FrameValue::Int(100 + level as i64)],
                monitors: vec![MonitorInfo {
                    object: FrameValue::StackSlotRef(-8 * (level as i32 + 1)),
                    lock_depth: 1,
                    relock: true,
                }],
                caller: built.take(),
            }));
        }
        *built.expect("at least the innermost scope")
    }

    // ── identity ─────────────────────────────────────────────────────

    /// The interning contract: structurally identical states are one state.
    /// Everything else in this module rests on it — `==` on handles is only a
    /// stand-in for structural equality if this holds.
    #[test]
    fn identical_states_intern_to_the_same_handle() {
        let mut it = FrameStateInterner::new();
        let a = owned(7, ints(20), vec![FrameValue::StackSlot(-8)]);
        let b = a.clone();
        let ia = it.intern(&a);
        let ib = it.intern(&b);
        assert_eq!(ia, ib, "identical states must share one handle");

        let st = it.stats();
        assert_eq!(st.states, 1, "the second intern allocated nothing");
        assert_eq!(st.intern_requests, 2);
        assert!(st.state_dedup_ratio() > 0.0);

        // Different in ONE field ⇒ a different state, but the untouched value
        // arrays are still the same handles.
        let ic = it.with_bci(ia, 8);
        assert_ne!(ia, ic);
        let sa = *it.scope(ia).expect("interned");
        let sc = *it.scope(ic).expect("interned");
        assert_eq!(sa.locals, sc.locals, "a bci change copies no slots");
        assert_eq!(sa.stack, sc.stack);
        assert_eq!(sa.method_key, sc.method_key);
    }

    /// The sharing claim at its smallest: two 32-slot snapshots differing in
    /// one slot store 40 slots, not 64.
    #[test]
    fn states_differing_in_one_slot_share_every_other_chunk() {
        let mut it = FrameStateInterner::new();
        let base = owned(0, ints(32), Vec::new());
        let mut changed = base.clone();
        changed.locals[5] = FrameValue::Int(999);

        let ia = it.intern(&base);
        let ib = it.intern(&changed);
        assert_ne!(ia, ib);

        let sa = *it.scope(ia).expect("interned");
        let sb = *it.scope(ib).expect("interned");
        assert_ne!(sa.locals, sb.locals);
        assert_eq!(sa.stack, sb.stack, "the empty stack is one shared array");
        assert_eq!(sa.method_key, sb.method_key, "the key is interned once");

        // 32 slots = 4 chunks; only the chunk holding slot 5 is re-interned.
        let st = it.stats();
        assert_eq!(st.chunks, 5, "4 shared + 1 re-interned");
        assert_eq!(st.stored_slots, 32 + FRAME_VALUE_CHUNK);
        assert_eq!(st.logical_slots, 64, "the owned form would store 2 x 32");
        assert!(
            (st.slot_sharing_ratio() - 0.375).abs() < 1e-9,
            "sharing {}",
            st.slot_sharing_ratio()
        );

        // Reading a shared slot needs no materialization.
        assert_eq!(it.local(ia, 5), Some(&FrameValue::Int(5)));
        assert_eq!(it.local(ib, 5), Some(&FrameValue::Int(999)));
        assert_eq!(it.local(ib, 6), Some(&FrameValue::Int(6)));
        assert_eq!(it.locals_len(ib), 32);
        assert_eq!(it.stack_len(ib), 0);
    }

    /// The persistent update and the compatibility path must land on the same
    /// state — otherwise a producer that switches from rebuilding owned
    /// snapshots to deriving them changes the metadata.
    #[test]
    fn persistent_derivation_agrees_with_interning_the_owned_state() {
        let mut it = FrameStateInterner::new();
        let base = owned(0, ints(24), vec![FrameValue::Int(7)]);
        let id = it.intern(&base);

        let mut expected = base.clone();
        expected.locals[9] = FrameValue::Object(0);
        expected.stack[0] = FrameValue::Undefined;
        expected.bci = 42;

        let derived = it.with_local(id, 9, FrameValue::Object(0));
        let derived = it.with_stack_slot(derived, 0, FrameValue::Undefined);
        let derived = it.with_bci(derived, 42);

        assert_eq!(
            derived,
            it.intern(&expected),
            "derivation and re-interning must agree"
        );

        // Writing the value already there derives nothing.
        assert_eq!(it.with_local(id, 9, FrameValue::Int(9)), id);
        // An out-of-range index is inert, not a panic.
        assert_eq!(it.with_local(id, 99, FrameValue::Int(1)), id);
        assert_eq!(it.with_stack_slot(id, 5, FrameValue::Int(1)), id);
    }

    // ── inlining ─────────────────────────────────────────────────────

    /// A caller chain of every depth 0..=8 survives interning byte-for-byte:
    /// method keys, bcis, locals, stack and monitors of every scope.
    #[test]
    fn caller_chains_of_depth_zero_to_eight_round_trip() {
        for depth in 0..=8usize {
            let before = owned_chain(depth);
            let mut it = FrameStateInterner::new();
            let id = it.intern(&before);

            assert_eq!(it.depth(id), depth, "depth {depth}");
            assert_eq!(it.method_key(id), "T.level0:()V");
            assert_eq!(it.bci(id), Some(1));

            let after = it.materialize(id).expect("round-trip");
            assert_eq!(
                format!("{after:?}"),
                format!("{before:?}"),
                "chain of depth {depth} must round-trip unchanged"
            );

            // Only the innermost scope re-executes; every inlined caller is
            // parked mid-invoke.
            assert_eq!(it.semantics(id), Some(ResumeSemantics::REEXECUTE));
            let mut cursor = it.caller(id);
            let mut seen = 0;
            while let Some(c) = cursor {
                assert_eq!(
                    it.semantics(c),
                    Some(ResumeSemantics::RESUME),
                    "caller scope at depth {seen} must not re-execute its invoke"
                );
                seen += 1;
                cursor = it.caller(c);
            }
            assert_eq!(seen, depth);
        }
    }

    /// What makes inlined scopes affordable: every deopt point inside one
    /// inlined callee names the *same* caller scope handle, so the caller's
    /// frame is stored once regardless of how many safepoints the callee has.
    #[test]
    fn one_inlined_caller_scope_is_shared_by_every_point_in_the_callee() {
        const POINTS: usize = 40;
        let mut it = FrameStateInterner::new();

        let caller = it.intern_scope(
            "T.outer:()V",
            17,
            &ints(16),
            &[FrameValue::Int(3)],
            &[],
            None,
            ResumeSemantics::for_caller_scope(),
        );

        let mut callee_locals = ints(8);
        let mut ids = Vec::with_capacity(POINTS);
        for bci in 0..POINTS {
            callee_locals[bci % 8] = FrameValue::Int(500 + bci as i64);
            let id = it.intern_scope(
                "T.inner:()V",
                bci as u32,
                &callee_locals,
                &[],
                &[],
                Some(caller),
                ResumeSemantics::REEXECUTE,
            );
            ids.push(id);
        }

        for id in &ids {
            assert_eq!(it.caller(*id), Some(caller));
            assert_eq!(it.depth(*id), 1);
        }

        // The caller's 17 slots are stored once, not 40 times: the owned form
        // boxes a full copy of the caller frame into every deopt point.
        let st = it.stats();
        assert_eq!(st.states, POINTS + 1);
        let owned_slots = it.owned_chain_slots(&ids);
        assert_eq!(
            owned_slots,
            (POINTS * (8 + 16 + 1)) as u64,
            "the owned form re-copies the caller into every point"
        );
        assert_eq!(
            st.stored_slots,
            16 + 1 + POINTS * FRAME_VALUE_CHUNK,
            "the caller's chunks are stored once for all {POINTS} points"
        );
        let chain_sharing = 1.0 - (st.stored_slots as f64 / owned_slots as f64);
        assert!(
            chain_sharing > 0.60,
            "expected >60% of chain slots shared at inline depth 1, got {chain_sharing:.4}"
        );
        eprintln!(
            "[frame-state interning] depth-1 inline, {POINTS} points: \
             chain slots {owned_slots} -> {} ({:.1}% shared)",
            st.stored_slots,
            chain_sharing * 100.0
        );

        // Relinking a callee scope under a *different* caller shares the
        // callee's slots too.
        let other_caller = it.with_bci(caller, 21);
        let relinked = it.with_caller(ids[0], Some(other_caller));
        assert_ne!(relinked, ids[0]);
        assert_eq!(
            it.scope(relinked).expect("interned").locals,
            it.scope(ids[0]).expect("interned").locals
        );
        assert_eq!(it.with_caller(ids[0], Some(caller)), ids[0]);
    }

    // ── the reexecute flag ───────────────────────────────────────────

    /// The flag that replaces the per-`DeoptReason` prose convention: it is
    /// part of a state's identity, it survives interning and derivation, and
    /// it is derived from the reason in exactly one place.
    #[test]
    fn the_reexecute_flag_survives_interning() {
        let mut it = FrameStateInterner::new();
        let fs = owned(4, ints(12), vec![FrameValue::Int(1)]);

        let re = it.intern_with(&fs, ResumeSemantics::REEXECUTE);
        let resume = it.intern_with(&fs, ResumeSemantics::RESUME);
        let rethrow = it.intern_with(&fs, ResumeSemantics::RETHROW);

        assert_ne!(re, resume, "semantics are part of the state's identity");
        assert_ne!(re, rethrow);
        assert!(it.semantics(re).expect("interned").reexecute);
        assert!(!it.semantics(resume).expect("interned").reexecute);
        assert!(it.semantics(rethrow).expect("interned").rethrow_exception);
        assert!(!it.semantics(rethrow).expect("interned").reexecute);

        // Three semantics, one copy of the slots.
        let (a, b, c) = (
            *it.scope(re).expect("interned"),
            *it.scope(resume).expect("interned"),
            *it.scope(rethrow).expect("interned"),
        );
        assert_eq!(a.locals, b.locals);
        assert_eq!(b.locals, c.locals);
        assert_eq!(it.stats().logical_slots, 3 * 13);
        assert_eq!(it.stats().stored_slots, 12 + 1);

        // The convention, written down once.
        assert_eq!(
            ResumeSemantics::for_reason(DeoptReason::BoundsCheck),
            ResumeSemantics::REEXECUTE
        );
        assert_eq!(
            ResumeSemantics::for_reason(DeoptReason::DivByZero),
            ResumeSemantics::REEXECUTE
        );
        assert_eq!(
            ResumeSemantics::for_reason(DeoptReason::OsrExit),
            ResumeSemantics::REEXECUTE
        );
        assert_eq!(
            ResumeSemantics::for_reason(DeoptReason::PendingException),
            ResumeSemantics::RETHROW
        );
        assert_eq!(ResumeSemantics::for_caller_scope(), ResumeSemantics::RESUME);
        assert_eq!(ResumeSemantics::default(), ResumeSemantics::REEXECUTE);
        assert_eq!(ResumeSemantics::REEXECUTE.to_string(), "reexecute");
        assert_eq!(ResumeSemantics::RESUME.to_string(), "resume");
        assert_eq!(ResumeSemantics::RETHROW.to_string(), "rethrow");

        // …and it reaches a state interned from a whole deopt point.
        let point = DeoptimizationPoint {
            native_offset: 0x10,
            bci: 4,
            reason: DeoptReason::PendingException,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: fs.clone(),
            semantics: ResumeSemantics::for_reason(DeoptReason::PendingException),
        };
        let interned = it.intern_point(&point);
        assert_eq!(
            it.semantics(interned.frame_state),
            Some(ResumeSemantics::RETHROW)
        );
        assert_eq!(interned.frame_state, rethrow, "same state, same handle");

        let flipped = it.with_semantics(interned.frame_state, ResumeSemantics::REEXECUTE);
        assert_eq!(flipped, re);
        assert_eq!(
            it.with_semantics(re, ResumeSemantics::REEXECUTE),
            re,
            "a no-op derivation allocates nothing"
        );
    }

    // ── the existing verifier over interned states ───────────────────

    fn limits() -> MethodFrameLimits {
        MethodFrameLimits::new(M, 32, 3, 2)
    }

    fn good_point() -> DeoptimizationPoint {
        DeoptimizationPoint {
            native_offset: 0x40,
            bci: 12,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: M.to_string(),
                bci: 12,
                locals: vec![FrameValue::StackSlotRef(-40), FrameValue::Int(7)],
                stack: vec![FrameValue::StackSlot(-48)],
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        }
    }

    fn verifier() -> DeoptVerifier {
        DeoptVerifier::new()
            .with_method(limits())
            .with_oop_map(0x40, OopCoverage::complete([40]))
    }

    fn rendered(errors: &[DeoptMetadataError]) -> String {
        errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// The interned form must not weaken the install-time check: a well-formed
    /// point still passes, and every violation the owned lane rejects today is
    /// still rejected through the interned lane, with the same error.
    #[test]
    fn the_verifier_accepts_interned_states_and_rejects_the_same_violations() {
        let mut it = FrameStateInterner::new();

        let ok = it.intern_point(&good_point());
        assert!(
            verifier().violations_interned(&it, &[ok]).is_empty(),
            "{}",
            rendered(&verifier().violations_interned(&it, &[ok]))
        );
        assert!(verifier().verify_interned(&it, &[ok]).is_ok());

        // bci past the end of the method
        let mut p = good_point();
        p.bci = 99;
        p.frame_state.bci = 99;
        let bad_bci = it.intern_point(&p);
        assert!(
            matches!(
                verifier().violations_interned(&it, &[bad_bci]).first(),
                Some(DeoptMetadataError::BciOutOfRange {
                    bci: 99,
                    code_len: 32,
                    ..
                })
            ),
            "{}",
            rendered(&verifier().violations_interned(&it, &[bad_bci]))
        );

        // more locals than max_locals
        let mut p = good_point();
        p.frame_state.locals = vec![FrameValue::Int(0); 4];
        let too_many = it.intern_point(&p);
        assert!(matches!(
            verifier().violations_interned(&it, &[too_many]).first(),
            Some(DeoptMetadataError::LocalCountMismatch {
                found: 4,
                max_locals: 3,
                ..
            })
        ));

        // the moving-GC agreement rule
        let mut p = good_point();
        p.frame_state.locals[0] = FrameValue::StackSlotRef(-56);
        let uncovered = it.intern_point(&p);
        let errs = verifier().violations_interned(&it, &[uncovered]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::ReferenceNotInOopMap {
                    frame_offset: 56,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(rendered(&errs).contains("local[0]"), "{}", rendered(&errs));

        // a baked heap address
        let mut p = good_point();
        p.frame_state.locals[1] = FrameValue::Object(0x7f00_1234);
        let baked = it.intern_point(&p);
        assert!(matches!(
            verifier().violations_interned(&it, &[baked]).first(),
            Some(DeoptMetadataError::BakedObjectAddress { .. })
        ));

        // a duplicate virtual-object definition
        let mut p = good_point();
        let vo = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 4,
            class_id: 1,
            num_fields: 0,
            field_values: Vec::new(),
        });
        p.frame_state.locals[0] = vo.clone();
        p.frame_state.stack[0] = vo;
        let dup = it.intern_point(&p);
        assert!(matches!(
            verifier().violations_interned(&it, &[dup]).first(),
            Some(DeoptMetadataError::DuplicateVirtualObjectDefinition { id: 4, .. })
        ));

        // an inlined caller scope is still checked at depth 1
        let mut p = good_point();
        p.frame_state.caller = Some(Box::new(FrameState {
            method_key: "T.outer:()V".to_string(),
            bci: 500,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        }));
        let inlined = it.intern_point(&p);
        let v = verifier().with_method(MethodFrameLimits::new("T.outer:()V", 20, 1, 1));
        assert!(
            matches!(
                v.violations_interned(&it, &[inlined]).first(),
                Some(DeoptMetadataError::BciOutOfRange { scope_depth: 1, .. })
            ),
            "{}",
            rendered(&v.violations_interned(&it, &[inlined]))
        );

        // sortedness still guards find_deopt_point's binary search
        let mut a = good_point();
        a.native_offset = 0x80;
        let mut b = good_point();
        b.native_offset = 0x40;
        let (ia, ib) = (it.intern_point(&a), it.intern_point(&b));
        assert!(matches!(
            DeoptVerifier::new()
                .violations_interned(&it, &[ia, ib])
                .first(),
            Some(DeoptMetadataError::DeoptPointsUnsorted {
                first: 0x80,
                second: 0x40
            })
        ));

        // the bailout is the same shape as the owned lane's
        let err = verifier()
            .verify_interned(&it, &[bad_bci])
            .expect_err("a bad bci must bail");
        assert_eq!(err.category(), "deopt_metadata");
        assert!(err.to_string().contains("resume bci 99"), "{err}");
    }

    /// Parity, point by point: the interned lane reports exactly what the
    /// owned lane reports. Two checkers that drift apart would be worse than
    /// the materialization this parity costs.
    #[test]
    fn interned_and_owned_verification_agree() {
        let mut it = FrameStateInterner::new();
        let mut points = Vec::new();

        points.push(good_point());
        let mut p = good_point();
        p.frame_state.locals[0] = FrameValue::RegisterRef(9);
        points.push(p);
        let mut p = good_point();
        p.bci = 13; // disagrees with frame_state.bci
        points.push(p);

        for p in &points {
            let interned = it.intern_point(p);
            let owned_errs = verifier().violations(std::slice::from_ref(p));
            let interned_errs = verifier().violations_interned(&it, &[interned]);
            assert_eq!(
                rendered(&owned_errs),
                rendered(&interned_errs),
                "lane disagreement on pc+0x{:x}",
                p.native_offset
            );
        }
    }

    /// A handle from a different interner is a violation, not a silently
    /// skipped point: an unreadable scope chain must never read as "clean".
    #[test]
    fn a_foreign_frame_state_handle_is_reported_not_skipped() {
        let mut it = FrameStateInterner::new();
        let point = it.intern_point(&good_point());
        let empty = FrameStateInterner::new();
        let errs = verifier().violations_interned(&empty, &[point]);
        assert!(
            matches!(
                errs.first(),
                Some(DeoptMetadataError::UnknownFrameStateHandle {
                    native_offset: 0x40,
                    ..
                })
            ),
            "{}",
            rendered(&errs)
        );
        assert!(
            rendered(&errs).contains("unreadable"),
            "{}",
            rendered(&errs)
        );
        assert!(verifier().verify_interned(&empty, &[point]).is_err());
    }

    /// `semantics` survives the interning round trip.
    ///
    /// It used to be re-derived from `DeoptReason` on the way *in* and dropped
    /// on the way *out* (`materialize` has nowhere to put it), so a producer
    /// that knew better than the reason-based convention could not say so.
    /// Now `intern_point` reads the field and `materialize_point` restores it —
    /// including the case where they disagree, which is the only case that
    /// proves the value is carried rather than recomputed.
    #[test]
    fn point_semantics_survive_the_interning_round_trip() {
        let mut it = FrameStateInterner::new();

        // The convention case: what every producer stamps today.
        let p = good_point();
        assert_eq!(p.semantics, ResumeSemantics::for_reason(p.reason));
        let conventional = it.intern_point(&p);
        let back = it.materialize_point(&conventional).expect("round-trip");
        assert_eq!(back.semantics, p.semantics);

        // The case the field exists for: semantics that the reason does NOT
        // imply. A resume point after a call that returned is `RESUME`, while
        // `for_reason` would say `REEXECUTE` and call the callee twice.
        let mut explicit = good_point();
        explicit.semantics = ResumeSemantics::RESUME;
        assert_ne!(
            explicit.semantics,
            ResumeSemantics::for_reason(explicit.reason),
            "precondition: the reason must not already imply these semantics"
        );
        let interned = it.intern_point(&explicit);
        assert_eq!(
            it.semantics(interned.frame_state),
            Some(ResumeSemantics::RESUME)
        );
        let back = it.materialize_point(&interned).expect("round-trip");
        assert_eq!(
            back.semantics,
            ResumeSemantics::RESUME,
            "materialize_point must read the interned semantics, not re-derive them"
        );
        // …and the rest of the point is untouched.
        assert_eq!(back.native_offset, explicit.native_offset);
        assert_eq!(back.bci, explicit.bci);
        assert_eq!(back.reason, explicit.reason);
    }

    // ── unresumable states stay unresumable ──────────────────────────

    /// The eliminated-vs-undefined distinction must survive the new
    /// representation intact. Interning a `MaterializationRequired` slot that
    /// softened it to `Undefined` (or dropped it in a shared chunk) would
    /// restore the silent-null reconstruction the variant exists to stop.
    #[test]
    fn materialization_required_remains_unresumable_through_interning() {
        let eliminated = FrameValue::MaterializationRequired(EliminatedValue::allocation(
            12,
            77,
            EliminationCause::ScalarReplacedObject,
        ));
        let mut it = FrameStateInterner::new();

        let fs = owned(
            3,
            vec![
                FrameValue::Int(1),
                eliminated.clone(),
                FrameValue::Undefined,
            ],
            Vec::new(),
        );
        let id = it.intern(&fs);
        assert!(!it.is_resumable(id), "an eliminated slot must refuse");
        assert!(!it.chain_is_resumable(id));
        assert_eq!(it.count_materialization_required(id), 1);
        assert_eq!(it.local(id, 1), Some(&eliminated));

        // …and it is still there after a round-trip through the owned form.
        let back = it.materialize(id).expect("round-trip");
        assert_eq!(back.locals[1], eliminated);
        assert!(!frame_state_is_resumable(&back));
        assert_eq!(count_materialization_required(&back), 1);

        // Parity with the owned predicates on a resumable frame too.
        let plain = owned(
            3,
            vec![FrameValue::Int(1), FrameValue::Undefined],
            Vec::new(),
        );
        let plain_id = it.intern(&plain);
        assert!(it.is_resumable(plain_id));
        assert_eq!(
            it.is_resumable(plain_id),
            frame_state_is_resumable(&it.materialize(plain_id).expect("round-trip"))
        );
        assert_eq!(it.count_materialization_required(plain_id), 0);

        // A poisoned virtual-object field poisons the slot, chunked or not.
        let poisoned = owned(
            0,
            vec![FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: None,
                id: 1,
                class_id: 2,
                num_fields: 2,
                field_values: vec![
                    FrameValue::Int(4),
                    FrameValue::MaterializationRequired(EliminatedValue::unknown(
                        EliminationCause::EliminatedStore,
                    )),
                ],
            })],
            Vec::new(),
        );
        let poisoned_id = it.intern(&poisoned);
        assert!(!it.is_resumable(poisoned_id));
        assert_eq!(it.count_materialization_required(poisoned_id), 1);

        // `is_resumable` is scope-local — it answers "is *this* frame clean".
        // `chain_is_resumable` is the whole-chain answer an inlined deopt
        // needs, and it is the one the owned `frame_state_is_resumable` now
        // matches: the owned predicate used to stop at the innermost scope, so
        // this exact state round-tripped back as "resumable" while holding a
        // caller slot nobody can rebuild.
        let mut inner = owned(1, vec![FrameValue::Int(0)], Vec::new());
        inner.caller = Some(Box::new(owned(2, vec![eliminated.clone()], Vec::new())));
        let inner_id = it.intern(&inner);
        assert!(
            it.is_resumable(inner_id),
            "the trapping scope itself is clean"
        );
        assert!(
            !it.chain_is_resumable(inner_id),
            "an unrebuildable caller slot makes the whole chain unresumable"
        );
        assert!(
            !frame_state_is_resumable(&it.materialize(inner_id).expect("round-trip")),
            "the owned predicate must agree with `chain_is_resumable`, not with \
             `is_resumable` — this is the resume sinks' only guard"
        );
    }

    // ── measurement ──────────────────────────────────────────────────

    /// The headline number: 100 consecutive safepoint snapshots of a 72-slot
    /// frame that differ by one slot each. The owned representation stores
    /// 7 200 slots; the interned one stores 864.
    #[test]
    fn sharing_ratio_on_a_hundred_snapshots_differing_by_one_slot() {
        const POINTS: usize = 100;
        const LOCALS: usize = 64;
        const STACK: usize = 8;

        let mut it = FrameStateInterner::new();
        let mut locals = ints(LOCALS);
        let stack: Vec<FrameValue> = (0..STACK)
            .map(|i| FrameValue::StackSlot(-8 * (i as i32 + 1)))
            .collect();

        let mut ids = Vec::with_capacity(POINTS);
        for bci in 0..POINTS {
            if bci > 0 {
                // Exactly one slot differs from the previous snapshot.
                locals[bci % LOCALS] = FrameValue::Int(1000 + bci as i64);
            }
            let fs = FrameState {
                method_key: "T.hot:()V".to_string(),
                bci: bci as u32,
                locals: locals.clone(),
                stack: stack.clone(),
                monitors: Vec::new(),
                caller: None,
            };
            ids.push(it.intern(&fs));
        }

        let distinct: FxHashSet<FrameStateId> = ids.iter().copied().collect();
        assert_eq!(distinct.len(), POINTS, "every bci is its own state");

        let st = it.stats();
        assert_eq!(st.states, POINTS);
        assert_eq!(st.method_keys, 1, "one key for the whole sequence");
        assert_eq!(st.logical_slots, (POINTS * (LOCALS + STACK)) as u64);
        // First snapshot: 8 local chunks + 1 stack chunk. Every later one
        // re-interns exactly the one local chunk it touched.
        assert_eq!(st.chunks, 9 + (POINTS - 1));
        assert_eq!(
            st.stored_slots,
            LOCALS + STACK + (POINTS - 1) * FRAME_VALUE_CHUNK
        );
        assert_eq!(st.value_arrays, POINTS + 1, "one shared stack array");

        let ratio = st.slot_sharing_ratio();
        assert!(
            ratio > 0.85,
            "expected >85% of slots shared, got {:.4}",
            ratio
        );
        assert!(
            st.slot_byte_saving() > 0.80,
            "expected >80% of slot bytes saved, got {:.4}",
            st.slot_byte_saving()
        );

        eprintln!(
            "[frame-state interning] {POINTS} snapshots x {} slots: \
             slots {} -> {} ({:.1}% shared); bytes {} -> {} ({:.1}% saved); \
             states {} value-arrays {} chunks {} spine-entries {}",
            LOCALS + STACK,
            st.logical_slots,
            st.stored_slots,
            ratio * 100.0,
            st.owned_slot_bytes(),
            st.interned_slot_bytes(),
            st.slot_byte_saving() * 100.0,
            st.states,
            st.value_arrays,
            st.chunks,
            st.spine_entries,
        );

        // Every snapshot still reads back exactly, shared chunks and all.
        let last = *ids.last().expect("100 points");
        assert_eq!(it.bci(last), Some((POINTS - 1) as u32));
        assert_eq!(it.locals_len(last), LOCALS);
        assert_eq!(it.stack_len(last), STACK);
        assert_eq!(
            it.local(last, (POINTS - 1) % LOCALS),
            Some(&FrameValue::Int(1000 + (POINTS - 1) as i64))
        );
        let materialized = it.materialize(last).expect("round-trip");
        assert_eq!(materialized.locals, locals);
        assert_eq!(materialized.stack, stack);
    }

    /// Monitors and method keys are interned too: a synchronized region open
    /// across many safepoints stores its monitor list once.
    #[test]
    fn monitor_lists_and_method_keys_are_shared() {
        let mut it = FrameStateInterner::new();
        let monitors = vec![MonitorInfo {
            object: FrameValue::StackSlotRef(-40),
            lock_depth: 2,
            relock: true,
        }];
        let mut ids = Vec::new();
        for bci in 0..16u32 {
            let fs = FrameState {
                method_key: "T.sync:()V".to_string(),
                bci,
                locals: vec![FrameValue::Int(bci as i64)],
                stack: Vec::new(),
                monitors: monitors.clone(),
                caller: None,
            };
            ids.push(it.intern(&fs));
        }
        let st = it.stats();
        assert_eq!(st.method_keys, 1);
        assert_eq!(st.monitor_arrays, 1, "one shared monitor list");
        assert_eq!(st.stored_monitors, 1);
        assert_eq!(st.logical_monitors, 16);
        for id in &ids {
            assert_eq!(it.monitors(*id).len(), 1);
            assert_eq!(it.monitors(*id)[0].lock_depth, 2);
        }
        // An empty monitor list is also interned once, and is a different one.
        let bare = it.intern(&owned(0, Vec::new(), Vec::new()));
        assert!(it.monitors(bare).is_empty());
        assert_eq!(it.stats().monitor_arrays, 2);
    }

    /// Nothing in this module panics on a handle it did not issue — these
    /// accessors are reachable from deopt paths, where a panic is strictly
    /// worse than a refusal.
    #[test]
    fn foreign_handles_are_inert() {
        let mut it = FrameStateInterner::new();
        let real = it.intern(&owned(1, ints(4), Vec::new()));
        let foreign = FrameStateId(9_999);

        assert!(it.scope(foreign).is_none());
        assert!(it.materialize(foreign).is_none());
        assert_eq!(it.method_key(foreign), "");
        assert_eq!(it.bci(foreign), None);
        assert_eq!(it.semantics(foreign), None);
        assert_eq!(it.caller(foreign), None);
        assert_eq!(it.depth(foreign), 0);
        assert_eq!(it.locals_len(foreign), 0);
        assert_eq!(it.stack_len(foreign), 0);
        assert_eq!(it.local(foreign, 0), None);
        assert_eq!(it.stack_slot(foreign, 0), None);
        assert!(it.monitors(foreign).is_empty());
        assert_eq!(it.count_materialization_required(foreign), 0);
        assert!(
            !it.is_resumable(foreign),
            "an undescribable frame must refuse, not resume"
        );
        assert!(!it.chain_is_resumable(foreign));

        // Derivations on a foreign handle return it unchanged.
        assert_eq!(it.with_local(foreign, 0, FrameValue::Int(1)), foreign);
        assert_eq!(it.with_stack_slot(foreign, 0, FrameValue::Int(1)), foreign);
        assert_eq!(it.with_bci(foreign, 3), foreign);
        assert_eq!(it.with_caller(foreign, Some(real)), foreign);
        assert_eq!(it.with_semantics(foreign, ResumeSemantics::RESUME), foreign);

        // …and the real handle is untouched by any of it.
        assert_eq!(it.locals_len(real), 4);
        assert_eq!(it.stats().states, 1);
    }

    /// A register number past the 16-entry files the deopt stub spills cannot
    /// be read at exit (`try_resolve_value` reports it and the value resumes as
    /// `Unsupported`), so resumability refuses it up front (review #88).
    #[test]
    fn a_register_past_the_spilled_files_blocks_the_resume() {
        assert_eq!(SavedRegisters::default().gpr.len(), SPILLED_REGISTER_FILE_LEN);
        assert_eq!(SavedRegisters::default().xmm.len(), SPILLED_REGISTER_FILE_LEN);
        let fs = |v: FrameValue| FrameState {
            method_key: String::from("T.m()V"),
            bci: 0,
            locals: vec![FrameValue::Int(1), v],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        for ok in [
            FrameValue::Register(15),
            FrameValue::RegisterLong(0),
            FrameValue::RegisterRef(3),
            FrameValue::XmmFloat(15),
            FrameValue::XmmDouble(7),
        ] {
            assert_eq!(first_unresumable_slot(&fs(ok.clone())), None, "{ok:?}");
        }
        for bad in [
            FrameValue::Register(16),
            FrameValue::RegisterLong(17),
            FrameValue::RegisterRef(255),
            FrameValue::XmmFloat(16),
            FrameValue::XmmDouble(32),
        ] {
            let msg = first_unresumable_slot(&fs(bad.clone()))
                .expect("an unspilled register blocks the resume");
            assert!(msg.contains("local 1"), "{bad:?}: {msg}");
        }
    }

    /// The unresumable-frame refusal has to name the slot: an `Unsupported`
    /// STACK entry means the operand stack had no width source at that bci, an
    /// `Unsupported` LOCAL means the local's kind or liveness was unknown, and
    /// those are different defects. Reading "reconstructs an unresumable frame"
    /// alone, the first cost a full instrumented rebuild to tell apart.
    #[test]
    fn first_unresumable_slot_names_the_offender() {
        let fs = FrameState {
            method_key: String::from("T.m()V"),
            bci: 0,
            locals: vec![FrameValue::Int(1), FrameValue::Int(2)],
            stack: vec![FrameValue::Int(3)],
            monitors: Vec::new(),
            caller: None,
        };
        assert_eq!(first_unresumable_slot(&fs), None);
        assert!(frame_state_is_resumable(&fs));

        let mut with_stack = fs.clone();
        with_stack.stack = vec![FrameValue::Int(3), FrameValue::Unsupported];
        let msg = first_unresumable_slot(&with_stack).expect("stack 1 blocks the resume");
        assert!(msg.starts_with("stack 1 "), "{msg}");
        assert!(
            msg.ends_with(" of 2"),
            "depth belongs in the message: {msg}"
        );

        let mut with_local = fs.clone();
        with_local.locals = vec![FrameValue::Int(1), FrameValue::Unsupported];
        let msg = first_unresumable_slot(&with_local).expect("local 1 blocks the resume");
        assert!(msg.starts_with("local 1 "), "{msg}");

        // Locals are scanned before the stack, so a frame with both names the
        // local — the same order `frame_state_is_resumable` walks.
        let mut both = fs;
        both.locals = vec![FrameValue::Unsupported];
        both.stack = vec![FrameValue::Unsupported];
        assert!(first_unresumable_slot(&both)
            .expect("blocked")
            .starts_with("local 0 "));
    }

    /// A RETHROW point's operand stack is never reconstructed, so it must not
    /// veto — and its LOCALS must still veto, because the handler reads them.
    ///
    /// Both halves are asserted here, and both matter. Dropping the first
    /// costs every method whose `catch` block contains a call its OSR entry
    /// (the veto is artifact-wide). Dropping the second enters a handler on
    /// zeroed non-parameter locals, which is the silent miscompile
    /// `route_jit_signal_exception` fails closed against.
    #[test]
    fn a_rethrow_frame_is_judged_on_its_locals_only() {
        let base = FrameState {
            method_key: String::from("T.m()V"),
            bci: 105,
            locals: vec![FrameValue::Int(1)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };

        // The shape this exists for: `catch (E e) { g(-1, x); }` in a method
        // that also touches a `long`, so the `-1` under the call's argument
        // has no width source and comes out `Unsupported`.
        let mut untypeable_stack = base.clone();
        untypeable_stack.stack = vec![FrameValue::Unsupported, FrameValue::Object(0)];
        assert!(
            first_unresumable_slot(&untypeable_stack).is_some(),
            "the general rule still sees it — this is the one a RESUME point is judged by"
        );
        assert_eq!(
            first_unresumable_local(&untypeable_stack),
            None,
            "a RETHROW frame's stack is replaced by [exception]; it cannot block anything"
        );

        let mut untypeable_local = base;
        untypeable_local.locals = vec![FrameValue::Int(1), FrameValue::Unsupported];
        let msg = first_unresumable_local(&untypeable_local).expect("a handler reads its locals");
        assert!(msg.starts_with("local 1 "), "{msg}");
    }

    /// The narrow rule walks the WHOLE caller chain, exactly as the general one
    /// does — an inlined caller's local is as much a handler input as the
    /// innermost scope's.
    #[test]
    fn the_rethrow_rule_walks_caller_scopes_too() {
        let caller = FrameState {
            method_key: String::from("T.outer()V"),
            bci: 4,
            locals: vec![FrameValue::Unsupported],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        let fs = FrameState {
            method_key: String::from("T.inner()V"),
            bci: 0,
            locals: vec![FrameValue::Int(1)],
            stack: vec![FrameValue::Unsupported],
            monitors: Vec::new(),
            caller: Some(Box::new(caller)),
        };
        let msg = first_unresumable_local(&fs).expect("the caller's local blocks it");
        assert!(msg.starts_with("caller-scope-1 local 0 "), "{msg}");
    }
}
