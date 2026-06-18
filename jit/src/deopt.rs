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

use std::mem;

use rustc_hash::FxHashMap;

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
    /// Integer constant.
    Int(i64),
    /// Float constant (stored as raw bits).
    Float(u64),
    /// Object reference (heap address, 0 for null).
    Object(u64),
    /// Value currently in a machine register.
    Register(u8),
    /// Value at a native stack slot offset.
    StackSlot(i32),
    /// Scalar-replaced object that must be re-materialized.
    VirtualObject(VirtualObjectState),
    /// Undefined / uninitialized.
    Undefined,
}

/// State of a scalar-replaced object that needs heap materialization.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualObjectState {
    pub class_id: u32,
    pub num_fields: usize,
    pub field_values: Vec<FrameValue>,
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
            // All other reasons use count-based policy.
            _ => {}
        }

        let half = (self.max_deopts_per_method as usize) / 2;
        let full = self.max_deopts_per_method as usize;

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
pub struct InvalidationManager {
    assumptions: FxHashMap<String, Vec<CompilationAssumption>>,
    class_dependencies: FxHashMap<u32, Vec<String>>,
}

impl InvalidationManager {
    pub fn new() -> Self {
        Self {
            assumptions: FxHashMap::default(),
            class_dependencies: FxHashMap::default(),
        }
    }

    /// Register an assumption made while compiling `method`.
    ///
    /// PERF-P5 (T10.9.C): same get_mut/insert pattern as `record_deopt`
    /// — assumptions accumulate over many calls for the same method, so
    /// skipping `method.to_string()` on the hit path is a real win.
    ///
    /// TODO(PERF-P5): take `&Arc<str>` once upstream call sites in
    /// `vm/src/vm.rs` thread the standard `Arc<str>` method-name carrier.
    pub fn register_assumption(&mut self, method: &str, assumption: CompilationAssumption) {
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

        // Check LeafClass assumptions across all methods.
        for (method, assumptions) in &self.assumptions {
            for a in assumptions {
                if let CompilationAssumption::LeafClass(cid) = a {
                    if *cid == class_id {
                        invalidated.push(method.clone());
                        break;
                    }
                }
            }
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
        let mut invalidated = Vec::new();

        for (method, assumptions) in &self.assumptions {
            for a in assumptions {
                if let CompilationAssumption::UniqueConcreteMethod {
                    class_id: cid,
                    method_name: mn,
                } = a
                {
                    if *cid == class_id && mn == method_name {
                        invalidated.push(method.clone());
                        break;
                    }
                }
            }
        }

        invalidated
    }

    /// Get all assumptions recorded for `method`.
    pub fn assumptions_for(&self, method: &str) -> &[CompilationAssumption] {
        self.assumptions.get(method).map_or(&[], |v| v.as_slice())
    }

    /// Clear assumptions for a method (on recompilation).
    pub fn clear_assumptions(&mut self, method: &str) {
        self.assumptions.remove(method);
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
/// return placeholder `(index, heap_address)` pairs.
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
/// The returned addresses are FAKE: monotonically increasing placeholders
/// starting at `0x1000_0000`, stepping by `0x100`. They do **not** point at
/// GC-allocated, header-initialized, field-populated heap objects. Treating
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
/// VM execution, this function is hard-gated to test builds. In a non-test
/// build it is unreachable: any accidental wiring onto a live path will fail
/// to compile / panic loudly rather than mint bogus object references.
#[cfg(test)]
pub fn materialize_virtual_objects(frame: &FrameState) -> Vec<(usize, u64)> {
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

    result
}

/// Non-test stub for [`materialize_virtual_objects`].
///
/// The real (test-only) implementation hands back FAKE placeholder heap
/// addresses (see that function's docs). To guarantee those cannot leak into
/// a live deopt and be mistaken for real objects, the placeholder body is
/// compiled only under `cfg(test)`. If a future caller wires this onto a real
/// deopt path before the GC-backed materialization above is implemented, this
/// stub makes the mistake impossible to miss: it never returns a fabricated
/// address — it panics. Replace it with the GC-allocating implementation
/// described above when the live-deopt heap plumbing lands.
#[cfg(not(test))]
pub fn materialize_virtual_objects(frame: &FrameState) -> Vec<(usize, u64)> {
    // Hard guard: virtual-object re-materialization needs a live heap/allocator
    // (see doc above). There is intentionally no placeholder-address path in a
    // real build — fail loud instead of producing fake object references.
    debug_assert!(
        false,
        "materialize_virtual_objects is a placeholder not wired to a live deopt \
         path; it needs GC-backed allocation before it can run for real"
    );
    let _ = frame;
    panic!(
        "materialize_virtual_objects called on a live path without GC-backed \
         materialization — refusing to mint fake heap addresses (see deopt.rs)"
    );
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
            class_id: 42,
            num_fields: 2,
            field_values: vec![FrameValue::Int(1), FrameValue::Object(0)],
        };
        assert_eq!(vo.class_id, 42);
        assert_eq!(vo.num_fields, 2);
        assert_eq!(vo.field_values.len(), 2);
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
                    class_id: 1,
                    num_fields: 1,
                    field_values: vec![FrameValue::Int(10)],
                }),
                FrameValue::Int(5),
            ],
            stack: vec![FrameValue::VirtualObject(VirtualObjectState {
                class_id: 2,
                num_fields: 0,
                field_values: Vec::new(),
            })],
            monitors: Vec::new(),
            caller: None,
        };
        assert_eq!(count_virtual_objects(&frame), 2);
        let materialized = materialize_virtual_objects(&frame);
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
