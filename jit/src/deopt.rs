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
//! - An `InvalidationManager` that tracks compilation assumptions and
//!   determines which methods must be invalidated when the class hierarchy
//!   changes.

use std::{fmt, mem};

use rustc_hash::{FxHashMap, FxHashSet};

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
    /// Undefined / uninitialized.
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

/// State of a scalar-replaced object that needs heap materialization.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualObjectState {
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
/// cannot determine a slot JVM width/type. The VM resume sink
/// (`build_deopt_frame_inner`) maps such a slot to `None` and refuses the
/// resume, so a deopt point carrying one is unresumable by construction.
///
/// Callers that emit an UNCONDITIONAL trap (the `invokedynamic` uncommon trap)
/// use this to bail the whole compile: an artifact that always traps at a point
/// it can never resume from fails on its first compiled call with
/// `InternalError: precise deoptimization unavailable ... refusing
/// side-effecting replay`, which is strictly worse than staying interpreted.
pub fn frame_state_is_resumable(fs: &FrameState) -> bool {
    !fs.locals
        .iter()
        .chain(fs.stack.iter())
        .any(|v| matches!(v, FrameValue::Unsupported))
}

/// Lock/monitor state for a single object.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub object: FrameValue,
    pub lock_depth: u32,
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

/// Process-global set of `(method_key, bci)` speculation sites that have
/// deopted past the per-bci give-up threshold and must NOT be re-speculated on
/// the next compilation — "only de-spec the speculation that failed instead of
/// whole-method eviction." The VM's real-frame-deopt de-spec path
/// (`vm/.../interpreter.rs`) inserts into it (only under `deopt_real_enabled()`);
/// the optimizing compiler reads it when deciding whether to emit a speculative
/// guard at a loop header (see `compile_with_param_slots`).
///
/// Empty on every production VM (nothing inserts unless the deopt-resume feature
/// is on), so `despec_contains` always returns `false` there and codegen is
/// byte-identical. Keyed by the same `"<class>.<method>:<descriptor>"` string
/// the deopt log / `method_epochs` use.
static DESPEC_SET: std::sync::OnceLock<std::sync::RwLock<FxHashSet<(String, u32)>>> =
    std::sync::OnceLock::new();

fn despec_set() -> &'static std::sync::RwLock<FxHashSet<(String, u32)>> {
    DESPEC_SET.get_or_init(|| std::sync::RwLock::new(FxHashSet::default()))
}

/// Record `(method_key, bci)` as a failed speculation site that must not be
/// re-speculated. Idempotent. See [`DESPEC_SET`].
pub fn despec_insert(method_key: &str, bci: u32) {
    if let Ok(mut s) = despec_set().write() {
        s.insert((method_key.to_string(), bci));
    }
}

/// `true` if `(method_key, bci)` was recorded as a failed speculation site.
/// Consulted by the optimizing compiler at speculative-guard emission. An empty
/// `method_key` never matches (the `compile()` legacy/test wrapper passes `""`).
///
/// Production fast path: when the registry is empty (nothing ever de-spec'd —
/// the case unless the deopt-resume feature is on) this returns `false` after a
/// cheap `is_empty` check, WITHOUT the `method_key.to_string()` lookup
/// allocation, so consulting it per speculative guard during normal compilation
/// is allocation-free.
pub fn despec_contains(method_key: &str, bci: u32) -> bool {
    if method_key.is_empty() {
        return false;
    }
    let set = match despec_set().read() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if set.is_empty() {
        return false;
    }
    set.contains(&(method_key.to_string(), bci))
}

/// Number of recorded de-spec sites for `method_key` (diagnostics / tests).
pub fn despec_count_for(method_key: &str) -> usize {
    despec_set()
        .read()
        .map(|s| s.iter().filter(|(m, _)| m == method_key).count())
        .unwrap_or(0)
}

/// Clear the entire de-spec registry. Test-only (process-global state leaks
/// across in-process tests otherwise).
pub fn despec_clear_for_test() {
    if let Ok(mut s) = despec_set().write() {
        s.clear();
    }
}

// ---------------------------------------------------------------------------
// Epoch staleness guard (deopt-osr Step 9 follow-up a)
// ---------------------------------------------------------------------------

/// A small, **process-lifetime-retained** cell baked (by raw pointer) into the
/// x64 frame-deopt stub *alongside* the `DeoptimizationPoint` box, so the deopt
/// trampoline can decide — **before dereferencing the box** — whether the
/// speculation it bakes has been superseded by a later invalidation.
///
/// Why a separate cell rather than a field on the box: the box describes the
/// speculation and must be dereferenced to reconstruct the interpreter frame.
/// Under the `CRATONVM_JIT_FREE_CODE=1` A/B mode an evicted artifact's
/// `DeoptimizationPoint` boxes can be freed; reading the epoch *from* the box
/// would itself be the use-after-free we are trying to avoid. This guard is
/// retained independently of the artifact (see [`crate::CompiledMethod`]'s
/// `Drop`), so `x64_deopt_entry` reads the live epoch and the artifact's
/// creation epoch from here without touching the box at all when the artifact is
/// stale. ("bake a stable live-epoch cell pointer alongside the box.")
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
        self.history.retain(|method, _| !method.starts_with(&prefix));
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
// Compilation assumptions & invalidation
// ---------------------------------------------------------------------------

/// An assumption the JIT made while compiling a method.
#[derive(Debug, Clone)]
pub enum CompilationAssumption {
    /// Class has no subclasses (enables devirtualization).
    LeafClass(u32),
    /// A concrete method is the only implementation.
    UniqueConcreteMethod { class_id: u32, method_name: String },
    /// A field is always non-null.
    NonNullField { class_id: u32, field_index: usize },
    /// A branch is never taken.
    UncommonBranch { bci: u32 },
    /// A type check always succeeds with a specific type.
    StableType { bci: u32, expected_class: u32 },
}

/// Tracks assumptions and class dependencies so compiled code can be
/// invalidated when the class hierarchy changes.
/// T10.9.B: FxHashMap — internal method names and class_id keys.
///
/// PERF (jit-deopt-perf): `on_class_loaded` / `on_method_override` used to
/// scan EVERY method's full assumption list on every class-load /
/// method-override event — O(methods × assumptions) per event, i.e. a linear
/// sweep over the entire assumption table on every single class load. We now
/// maintain two reverse indices (`leaf_class_index`, `unique_method_index`)
/// keyed by the class / (class, method) an assumption depends on, so an event
/// visits only the assumptions that actually reference it. The indices are
/// kept in lock-step with `assumptions` in `register_assumption` /
/// `clear_assumptions`; invalidation correctness is preserved exactly (every
/// dependent assumption that fired before still fires, with the same dedup).
pub struct InvalidationManager {
    assumptions: FxHashMap<String, Vec<CompilationAssumption>>,
    class_dependencies: FxHashMap<u32, Vec<String>>,
    /// Reverse index: class_id → method keys that hold a `LeafClass(class_id)`
    /// assumption. Each method key appears at most once per class_id (matching
    /// the old per-method `break` dedup). Mirrors the `LeafClass` entries in
    /// `assumptions`.
    leaf_class_index: FxHashMap<u32, Vec<String>>,
    /// Reverse index: (class_id, method_name) → method keys that hold a
    /// `UniqueConcreteMethod { class_id, method_name }` assumption. Each method
    /// key appears at most once per (class_id, method_name). Mirrors the
    /// `UniqueConcreteMethod` entries in `assumptions`.
    unique_method_index: FxHashMap<(u32, String), Vec<String>>,
}

impl InvalidationManager {
    pub fn new() -> Self {
        Self {
            assumptions: FxHashMap::default(),
            class_dependencies: FxHashMap::default(),
            leaf_class_index: FxHashMap::default(),
            unique_method_index: FxHashMap::default(),
        }
    }

    /// Drop all hierarchy assumptions after class unloading. Unloading is rare
    /// and invalidates both owners and dependants, so a conservative reset is
    /// smaller and safer than retaining strings that may name dead metadata.
    pub fn clear_all(&mut self) {
        self.assumptions.clear();
        self.class_dependencies.clear();
        self.leaf_class_index.clear();
        self.unique_method_index.clear();
    }

    /// Register an assumption made while compiling `method`.
    ///
    /// PERF-P5 (T10.9.C): same get_mut/insert pattern as `record_deopt`
    /// — assumptions accumulate over many calls for the same method, so
    /// skipping `method.to_string()` on the hit path is a real win.
    ///
    /// TODO(PERF-P5): take `&Arc<str>` once upstream call sites in
    /// `vm/src/vm.rs` thread the standard `Arc<str>` method-name carrier.
    ///
    /// PERF (jit-deopt-perf): also feed the reverse indices used by
    /// `on_class_loaded` / `on_method_override` so those events no longer scan
    /// the whole assumption table. The index update mirrors exactly which
    /// `(method, assumption)` pairs the old linear scan would have matched.
    pub fn register_assumption(&mut self, method: &str, assumption: CompilationAssumption) {
        // Maintain the reverse index for the assumption kinds queried by the
        // invalidation events. We dedup the method key per index entry so a
        // method appears at most once in the result — matching the old
        // per-method `break` after the first match. (A method may register the
        // same assumption more than once; the index must not list it twice.)
        match &assumption {
            CompilationAssumption::LeafClass(cid) => {
                let entry = self.leaf_class_index.entry(*cid).or_default();
                if !entry.iter().any(|m| m == method) {
                    entry.push(method.to_string());
                }
            }
            CompilationAssumption::UniqueConcreteMethod {
                class_id,
                method_name,
            } => {
                let key = (*class_id, method_name.clone());
                let entry = self.unique_method_index.entry(key).or_default();
                if !entry.iter().any(|m| m == method) {
                    entry.push(method.to_string());
                }
            }
            // Other assumption kinds are not consulted by class-load /
            // method-override events, so they need no reverse index.
            _ => {}
        }

        if let Some(assumptions) = self.assumptions.get_mut(method) {
            assumptions.push(assumption);
            return;
        }
        self.assumptions
            .insert(method.to_string(), vec![assumption]);
    }

    /// Called when a new class is loaded. Returns the set of compiled methods
    /// whose `LeafClass` assumption on `class_id` is now invalid, plus any
    /// methods listed in `class_dependencies`.
    pub fn on_class_loaded(&self, class_id: u32) -> Vec<String> {
        let mut invalidated = Vec::new();

        // PERF (jit-deopt-perf): O(matches) reverse-index lookup instead of an
        // O(methods × assumptions) sweep of the whole table. `leaf_class_index`
        // already holds exactly the method keys whose `LeafClass(class_id)`
        // assumption is now invalid, deduped per class_id.
        if let Some(methods) = self.leaf_class_index.get(&class_id) {
            invalidated.extend(methods.iter().cloned());
        }

        // Also include direct class dependencies.
        if let Some(deps) = self.class_dependencies.get(&class_id) {
            for m in deps {
                if !invalidated.contains(m) {
                    invalidated.push(m.clone());
                }
            }
        }

        invalidated
    }

    /// Called when a method is overridden in `class_id`. Returns methods whose
    /// `UniqueConcreteMethod` assumption is now invalid.
    pub fn on_method_override(&self, class_id: u32, method_name: &str) -> Vec<String> {
        // PERF (jit-deopt-perf): O(matches) reverse-index lookup instead of an
        // O(methods × assumptions) sweep. `unique_method_index` already holds
        // exactly the method keys whose `UniqueConcreteMethod { class_id,
        // method_name }` assumption is now invalid, deduped per key.
        //
        // We allocate one `String` to build the probe key — method-override is
        // a rare, cold event, so this is dwarfed by the eliminated full-table
        // scan (and by the work the caller does to actually invalidate code).
        self.unique_method_index
            .get(&(class_id, method_name.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    /// Get all assumptions recorded for `method`.
    pub fn assumptions_for(&self, method: &str) -> &[CompilationAssumption] {
        self.assumptions.get(method).map_or(&[], |v| v.as_slice())
    }

    /// Clear assumptions for a method (on recompilation).
    pub fn clear_assumptions(&mut self, method: &str) {
        let removed = match self.assumptions.remove(method) {
            Some(a) => a,
            // Nothing recorded for this method — indices already consistent.
            None => return,
        };

        // PERF (jit-deopt-perf): keep the reverse indices in lock-step. For
        // each removed assumption, drop this method key from the matching
        // index entry so `on_class_loaded` / `on_method_override` no longer
        // report it (preserving exact invalidation correctness). We dedup the
        // index-key work so a method that registered the same assumption twice
        // is removed once. Empty buckets are pruned to keep lookups tight.
        let mut leaf_seen: Vec<u32> = Vec::new();
        let mut unique_seen: Vec<(u32, &str)> = Vec::new();
        for a in &removed {
            match a {
                CompilationAssumption::LeafClass(cid) => {
                    if leaf_seen.contains(cid) {
                        continue;
                    }
                    leaf_seen.push(*cid);
                    if let Some(entry) = self.leaf_class_index.get_mut(cid) {
                        entry.retain(|m| m != method);
                        if entry.is_empty() {
                            self.leaf_class_index.remove(cid);
                        }
                    }
                }
                CompilationAssumption::UniqueConcreteMethod {
                    class_id,
                    method_name,
                } => {
                    let probe = (*class_id, method_name.as_str());
                    if unique_seen.contains(&probe) {
                        continue;
                    }
                    unique_seen.push(probe);
                    let key = (*class_id, method_name.clone());
                    if let Some(entry) = self.unique_method_index.get_mut(&key) {
                        entry.retain(|m| m != method);
                        if entry.is_empty() {
                            self.unique_method_index.remove(&key);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Register that `method` depends on `class_id`.
    ///
    /// PERF-P5 (T10.9.C): the inner `Vec<String>` still owns its method
    /// names — but at least skip the empty-vec allocation by using
    /// `get_mut` first. (We still pay one `String::from(method)` per
    /// call because the dependency lists may legitimately contain the
    /// same method multiple times; we are not deduping.)
    ///
    /// TODO(PERF-P5): switch `class_dependencies` values to
    /// `Vec<Arc<str>>` once upstream call sites carry `Arc<str>` keys.
    pub fn add_class_dependency(&mut self, class_id: u32, method: &str) {
        if let Some(deps) = self.class_dependencies.get_mut(&class_id) {
            deps.push(method.to_string());
            return;
        }
        self.class_dependencies
            .insert(class_id, vec![method.to_string()]);
    }

    /// Get the list of methods that depend on `class_id`.
    pub fn methods_depending_on(&self, class_id: u32) -> &[String] {
        self.class_dependencies
            .get(&class_id)
            .map_or(&[], |v| v.as_slice())
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
    match v {
        FrameValue::Register(r) => FrameValue::Int(regs.gpr[*r as usize] as i64),
        FrameValue::RegisterLong(r) => FrameValue::Long(regs.gpr[*r as usize] as i64),
        // The register holds the raw heap pointer (0 == null) captured in-stub at
        // the guard — same as `StackSlotRef` but read from the spilled GPR file.
        FrameValue::RegisterRef(r) => FrameValue::Object(regs.gpr[*r as usize]),
        FrameValue::XmmFloat(n) => {
            // Low 32 bits of the spilled XMM ARE the IEEE-754 float pattern.
            FrameValue::Float(regs.xmm[*n as usize] & 0xFFFF_FFFF)
        }
        FrameValue::XmmDouble(n) => {
            // Full 64 bits of the spilled XMM ARE the IEEE-754 double pattern.
            FrameValue::Double(regs.xmm[*n as usize])
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
                *fv = resolve_value(fv, regs, rbp);
            }
            FrameValue::VirtualObject(resolved)
        }
        other => other.clone(),
    }
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

/// Legacy compatibility gate. Code reclamation is now ownership-safe in every
/// configuration: an executing artifact owns its deopt metadata until return,
/// so a superseded frame remains reconstructable and must not be forced into a
/// side-effect-replaying whole-method fallback.
fn jit_free_code_enabled() -> bool {
    false
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
pub extern "C" fn ir_deopt_entry(point: *const DeoptimizationPoint, rbp: u64) -> i64 {
    // SAFETY: contract documented above.
    let point = unsafe { &*point };
    // The IR lowerer keeps every live value in a frame slot, so no register
    // file is needed; a register-allocating backend would spill GPRs/XMMs in
    // the trampoline and pass them here instead.
    let regs = SavedRegisters::default();
    let frame = reconstruct_frame_from_machine_state(point, &regs, rbp);
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
/// stable, retained [`DeoptEpochGuard`] baked alongside the box. Before
/// dereferencing `point`, the entry consults the guard: if the artifact has been
/// superseded (its creation epoch is older than the method's live epoch), the
/// baked speculation is stale, so it stashes a sentinel "re-run" frame
/// (out-of-range bci ⇒ the VM resume path rejects it and re-runs the method)
/// **without touching `point` at all** — the before-deref check the
/// `CRATONVM_JIT_FREE_CODE=1` mode needs (where the box may have been freed).
/// `epoch_guard` is null on every production artifact (the VM stamps it only
/// under `deopt_real_enabled()`), so the check is inert there.
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
    // Before-deref staleness short-circuit — ONLY when `CRATONVM_JIT_FREE_CODE`
    // is set (the A/B mode that actually frees artifacts and their deopt-point
    // boxes on eviction). In the default retain-everything mode the box is
    // leaked for the process lifetime (see `CompiledMethod`'s Drop), and a
    // superseded artifact's snapshot is still SELF-CONSISTENT with the machine
    // state of the (retained, still-executing) code that trapped — the epochs
    // version the SPECULATION, not the frame layout. Short-circuiting here for
    // retained code stashed an identity-less `bci == u32::MAX` sentinel that
    // forced every post-supersession trap onto the imprecise whole-method
    // re-run — re-introducing the side-effect duplication for exactly the
    // methods that keep getting dispatched via stale cached entries after
    // their first de-speculation (jit-invokedynamic-groovy-regression fix).
    if !epoch_guard.is_null() && jit_free_code_enabled() {
        // SAFETY: a non-null `epoch_guard` is a retained `DeoptEpochGuard`
        // (process-lifetime, see `CompiledMethod`'s Drop) — valid to read.
        let guard = unsafe { &*epoch_guard };
        if guard.is_superseded() {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
                eprintln!(
                    "[cratonvm-deopt] x64 frame-deopt SUPERSEDED (creation_epoch={} < live) — \
                     skipping reconstruction, routing to safe re-run",
                    guard
                        .creation_epoch
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
            }
            // Stash a sentinel so the VM treats this as deopt-and-re-run
            // (take_last_deopt is Some), never as a real i64::MIN return. The
            // out-of-range bci makes the resume path fail → safe whole-method
            // re-run. No `point` deref.
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
    }
    if point.is_null() || regs.is_null() {
        return i64::MIN;
    }
    // SAFETY: contract documented above.
    let point = unsafe { &*point };
    let regs = unsafe { &*regs };
    let frame = reconstruct_frame_from_machine_state(point, regs, rbp);
    if point.reason == DeoptReason::PendingException {
        // Exceptional frames get their own stash — see `LAST_EXCEPTIONAL`.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
            eprintln!(
                "[cratonvm-deopt] x64 exceptional frame at throw bci={} locals={:?}",
                point.bci, frame.locals,
            );
        }
        LAST_EXCEPTIONAL.with(|c| *c.borrow_mut() = Some(frame));
        return i64::MIN;
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
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
    /// jit-invokedynamic-groovy-regression identity fix: in the default
    /// retain-everything mode (`CRATONVM_JIT_FREE_CODE` unset — which unit
    /// tests must assume, since mutating a process-global env var races
    /// parallel tests) a SUPERSEDED guard NO LONGER short-circuits — the
    /// artifact's code and deopt boxes are leaked for the process lifetime,
    /// so the box is valid and its snapshot is self-consistent with the
    /// (stale, still-executing) code that trapped. The entry must proceed to
    /// a normal reconstruction; short-circuiting here stashed an
    /// identity-less `bci == u32::MAX` sentinel that forced every
    /// post-supersession trap onto the corrupting imprecise re-run. (The
    /// before-deref short-circuit still exists under `CRATONVM_JIT_FREE_CODE`
    /// — not unit-covered, by the env-race constraint above.)
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
            vec![FrameValue::Object(0), FrameValue::Int(3), FrameValue::Undefined],
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

    // -- per-bci de-spec registry (Step 9 follow-up c) ---------------------

    /// `despec_insert`/`despec_contains`/`despec_count_for` are per-(method, bci):
    /// a recorded site matches only its own key+bci, an empty key never matches,
    /// and inserts are idempotent. Uses a test-unique method key so it does not
    /// race the shared process-global set with other parallel tests.
    #[test]
    fn despec_registry_is_per_method_bci() {
        let m = "DespecTest$Unique.loop:(I)I";
        assert!(!despec_contains(m, 7));
        despec_insert(m, 7);
        despec_insert(m, 7); // idempotent
        despec_insert(m, 12);
        assert!(despec_contains(m, 7));
        assert!(despec_contains(m, 12));
        assert!(!despec_contains(m, 8), "a different bci must not match");
        assert!(
            !despec_contains("OtherClass.m:()V", 7),
            "a different method must not match"
        );
        assert!(!despec_contains("", 7), "an empty key never matches");
        assert_eq!(despec_count_for(m), 2);
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
    // `docs/internal/arch-2026-07-26/jit-regalloc-and-deopt.md`.

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

    // -- InvalidationManager -----------------------------------------------

    #[test]
    fn invalidation_register_assumption() {
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption("m", CompilationAssumption::LeafClass(1));
        assert_eq!(mgr.assumptions_for("m").len(), 1);
    }

    #[test]
    fn invalidation_on_class_loaded_leaf() {
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption("m1", CompilationAssumption::LeafClass(10));
        mgr.register_assumption("m2", CompilationAssumption::LeafClass(20));
        let inv = mgr.on_class_loaded(10);
        assert!(inv.contains(&"m1".to_string()));
        assert!(!inv.contains(&"m2".to_string()));
    }

    #[test]
    fn invalidation_on_method_override() {
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption(
            "caller",
            CompilationAssumption::UniqueConcreteMethod {
                class_id: 5,
                method_name: "run".to_string(),
            },
        );
        let inv = mgr.on_method_override(5, "run");
        assert_eq!(inv, vec!["caller".to_string()]);
    }

    #[test]
    fn invalidation_on_method_override_no_match() {
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption(
            "caller",
            CompilationAssumption::UniqueConcreteMethod {
                class_id: 5,
                method_name: "run".to_string(),
            },
        );
        let inv = mgr.on_method_override(5, "stop");
        assert!(inv.is_empty());
    }

    #[test]
    fn invalidation_class_dependency() {
        let mut mgr = InvalidationManager::new();
        mgr.add_class_dependency(10, "dep_method");
        let deps = mgr.methods_depending_on(10);
        assert_eq!(deps, &["dep_method".to_string()]);
    }

    #[test]
    fn invalidation_class_loaded_includes_dependencies() {
        let mut mgr = InvalidationManager::new();
        mgr.add_class_dependency(10, "dep_method");
        let inv = mgr.on_class_loaded(10);
        assert!(inv.contains(&"dep_method".to_string()));
    }

    #[test]
    fn invalidation_clear_assumptions() {
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption("m", CompilationAssumption::LeafClass(1));
        mgr.register_assumption("m", CompilationAssumption::UncommonBranch { bci: 5 });
        assert_eq!(mgr.assumptions_for("m").len(), 2);
        mgr.clear_assumptions("m");
        assert_eq!(mgr.assumptions_for("m").len(), 0);
    }

    #[test]
    fn invalidation_empty_dependencies() {
        let mgr = InvalidationManager::new();
        assert!(mgr.methods_depending_on(999).is_empty());
    }

    // -- reverse-index correctness (jit-deopt-perf) ------------------------

    #[test]
    fn invalidation_clear_removes_from_leaf_index() {
        // After clearing, on_class_loaded must no longer report the method —
        // this guards the reverse-index teardown in clear_assumptions.
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption("m", CompilationAssumption::LeafClass(42));
        assert_eq!(mgr.on_class_loaded(42), vec!["m".to_string()]);
        mgr.clear_assumptions("m");
        assert!(mgr.on_class_loaded(42).is_empty());
    }

    #[test]
    fn invalidation_clear_removes_from_unique_method_index() {
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption(
            "caller",
            CompilationAssumption::UniqueConcreteMethod {
                class_id: 7,
                method_name: "go".to_string(),
            },
        );
        assert_eq!(mgr.on_method_override(7, "go"), vec!["caller".to_string()]);
        mgr.clear_assumptions("caller");
        assert!(mgr.on_method_override(7, "go").is_empty());
    }

    #[test]
    fn invalidation_clear_only_affects_cleared_method() {
        // Two methods share LeafClass(9); clearing one must leave the other.
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption("a", CompilationAssumption::LeafClass(9));
        mgr.register_assumption("b", CompilationAssumption::LeafClass(9));
        mgr.clear_assumptions("a");
        let inv = mgr.on_class_loaded(9);
        assert!(!inv.contains(&"a".to_string()));
        assert!(inv.contains(&"b".to_string()));
    }

    #[test]
    fn invalidation_duplicate_assumption_listed_once() {
        // Registering the same LeafClass twice for one method must still yield
        // the method exactly once (matches the old per-method `break` dedup).
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption("m", CompilationAssumption::LeafClass(3));
        mgr.register_assumption("m", CompilationAssumption::LeafClass(3));
        let inv = mgr.on_class_loaded(3);
        assert_eq!(inv, vec!["m".to_string()]);
        // And clearing once removes it fully despite the double registration.
        mgr.clear_assumptions("m");
        assert!(mgr.on_class_loaded(3).is_empty());
    }

    #[test]
    fn invalidation_class_loaded_unions_leaf_and_dependencies() {
        // A LeafClass match and a class dependency on the same class_id both
        // appear, with no duplicate when a method is in both.
        let mut mgr = InvalidationManager::new();
        mgr.register_assumption("leaf_m", CompilationAssumption::LeafClass(5));
        mgr.add_class_dependency(5, "leaf_m"); // also a dependency
        mgr.add_class_dependency(5, "dep_only");
        let inv = mgr.on_class_loaded(5);
        assert!(inv.contains(&"leaf_m".to_string()));
        assert!(inv.contains(&"dep_only".to_string()));
        // leaf_m must not be duplicated.
        assert_eq!(inv.iter().filter(|m| *m == "leaf_m").count(), 1);
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
                    id: 0,
                    class_id: 1,
                    num_fields: 1,
                    field_values: vec![FrameValue::Int(10)],
                }),
                FrameValue::Int(5),
            ],
            stack: vec![FrameValue::VirtualObject(VirtualObjectState {
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
        };
        let rf = reconstruct_frame(&dp);
        assert_eq!(rf.monitors.len(), 1);
        assert_eq!(rf.monitors[0].lock_depth, 1);
    }
}
