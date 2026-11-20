// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Structured compilation bailouts for the optimizing (sea-of-nodes) JIT.
//!
//! ## Why this exists
//!
//! The P0 "JIT correctness" lane of the C2 review
//! (`deep-research-vm-c2.md`) requires that *"invalid IR or
//! ABI state causes a deterministic compilation bailout, never silent wrong
//! code, panic, or native crash"*. Today the compiler signals "I cannot
//! compile this" in three incompatible ways:
//!
//! * `Option::None` (`IrBuilder::build`, `ir_lower::lower_inner`) — carries no
//!   reason at all, so the per-method compiler report the review asks for
//!   ("admitted/bailout counts by reason") cannot be produced.
//! * a `bool` sticky flag (`ir_lower`'s `unallocated_slot_use`,
//!   `ExecutableBuffer`'s overflow flag) — one bit per failure class.
//! * `assert!` / `panic!` (`Lowerer::alloc_slot`'s frame-capacity assertion) —
//!   which takes the whole VM down for what is a *compiler* resource limit.
//!
//! [`Bailout`] is the one structured value all three should converge on. It is
//! deliberately additive: nothing in this module changes an existing signature.
//! A caller that still returns `Option` adapts at its own call site
//! (`match verify(..) { Err(b) => { record_bailout(&b); None } … }`), which is
//! exactly how `lib.rs` wires the IR verifier in.
//!
//! ## What a bailout is *not*
//!
//! A bailout is **not** an error the Java program can observe. Every bailout
//! means "this method takes the interpreter / single-pass backend instead",
//! which is always semantically valid — the optimizing tier is an optimization,
//! never a requirement. That is why [`record_bailout`] only bumps a counter and
//! why nothing here logs by default: a bailout is a *quality* signal, not a
//! fault.
//!
//! ## Metrics
//!
//! [`record_bailout`] increments a process-wide relaxed atomic counter keyed by
//! [`Bailout::category`], and [`bailout_counts`] reads the whole table back.
//! Relaxed ordering is correct here: the counters are pure statistics with no
//! happens-before relationship to anything else, and a torn read across
//! categories is not a defect (the review asks for counts by reason, not for a
//! consistent snapshot). The table is process-wide rather than per-`SharedVm`
//! on purpose — it counts *compiler* events, which are a property of the
//! process's code generator, not of a VM instance's Java state.

use std::sync::atomic::{AtomicU64, Ordering};

// ── Default compiler limits ──────────────────────────────────────────

/// Default node-count ceiling for a graph entering the optimizing pipeline.
///
/// Aliased to [`crate::ir::IR_MAX_GRAPH_NODES`] rather than duplicated, so the
/// bailout reason and the check that produces it can never drift apart. See
/// that constant for the rationale (`ir_optimize`'s passes are super-linear in
/// node count, so a bytecode-length cap alone does not bound compile time).
pub const DEFAULT_MAX_NODES: usize = crate::ir::IR_MAX_GRAPH_NODES;

/// Default frame-size ceiling, in bytes, for a lowered method.
///
/// `ir_lower.rs` encodes safepoint-map slot offsets — the reference parameter
/// homes in `Lowerer::new` and every entry `emit_safepoint_map` publishes — as
/// `i16`, guarded by `off <= i16::MAX`. A frame larger than `i16::MAX + 1`
/// bytes therefore has slots whose offset is *unrepresentable* in the oop map,
/// which means the GC would not see the reference living there. That makes
/// 32 KiB the hard correctness bound on frame size, independent of any
/// stack-consumption policy, so it is the default the
/// [`BailoutReason::FrameTooLarge`] check should carry.
///
/// Note how this interacts with [`DEFAULT_MAX_NODES`]. `ir_lower` used to
/// reserve one 8-byte spill slot per *arena* node, so a graph at the node limit
/// implied a ~160 KiB frame — five times this bound — and a method could only
/// approach the limit with a far smaller graph. The liveness-based slot-reuse
/// item from the same review ("the current lowerer reserves `max_nodes * 8`
/// bytes for spills, regardless of live-range overlap") has since landed:
/// `ir_lower::plan_slots` colours values whose live ranges do not overlap into
/// one slot, and `estimate_frame_bytes` budgets that coloured slot count. The
/// spill term is therefore bounded by peak simultaneous liveness, not by graph
/// size, so the two limits are now independent — a large graph with a narrow
/// live set fits comfortably, and a graph whose colouring still overflows is
/// declined here rather than by the node cap.
pub const DEFAULT_MAX_FRAME_BYTES: usize = i16::MAX as usize + 1;

// ── Reasons ──────────────────────────────────────────────────────────

/// Why the optimizing compiler declined to produce code for a method.
///
/// Every variant carries the operands needed to reproduce the decision, so a
/// compiler report can print an actionable line without re-running the
/// compile. Variants are ordered from "policy limit" through "unsupported
/// input" to "internal invariant broken".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BailoutReason {
    /// The IR graph exceeded the node budget (compile-time guard).
    GraphTooLarge { nodes: usize, limit: usize },
    /// The lowered frame exceeded the frame-size budget. See
    /// [`DEFAULT_MAX_FRAME_BYTES`] for why this is a correctness bound and not
    /// merely a stack-consumption policy.
    FrameTooLarge { bytes: usize, limit: usize },
    /// A bytecode the front end does not model.
    UnsupportedOpcode { opcode: u8 },
    /// A shape the back end does not model (a *static* description, e.g.
    /// `"multianewarray"` or `"phi of Ref at a Region"`).
    UnsupportedShape(&'static str),
    /// Not enough machine registers for the live values at some program point.
    RegisterPressure,
    /// A memory displacement did not fit its instruction encoding.
    DisplacementOutOfRange { disp: i64 },
    /// A branch / call relocation did not fit its instruction encoding.
    RelocationOutOfRange { delta: i64 },
    /// The executable buffer ran out of room mid-emission.
    CodeBufferExhausted { needed: usize, capacity: usize },
    /// A value was read before a machine location was assigned to it — the
    /// structured form of `ir_lower`'s `unallocated_slot_use` sticky flag.
    UnallocatedValue { node: u32 },
    /// The IR verifier rejected the graph. The `String` is the accumulated
    /// violation list produced by [`crate::ir_verify::verify_graph`].
    IrVerification(String),
    /// The install-time deopt-metadata verifier rejected the emitted safepoint
    /// descriptors. The `String` is the accumulated violation list produced by
    /// `crate::deopt`'s verifier.
    ///
    /// Deliberately *not* folded into [`BailoutReason::IrVerification`]: the two
    /// verify different artifacts at different times (a graph before lowering
    /// vs. the metadata a finished code blob is about to be installed with), and
    /// a metrics consumer that cannot tell them apart cannot tell "the front end
    /// built a bad graph" from "the back end described a good graph badly".
    /// `deopt.rs` used `IrVerification` with a `phase=deopt-metadata` context
    /// only because this variant did not exist yet.
    DeoptMetadata(String),
    /// A compiler invariant was broken. This is the variant that replaces a
    /// `panic!`: the method loses its optimized body, the VM does not die.
    Internal(&'static str),
}

impl BailoutReason {
    /// Short stable category name, for metrics keys and log greps.
    ///
    /// These strings are an external contract: dashboards and test assertions
    /// key on them, so they must not be renamed with the variants.
    pub fn category(&self) -> &'static str {
        CATEGORIES[self.category_index()]
    }

    /// Index of this reason's category in [`CATEGORIES`] (and [`COUNTERS`]).
    ///
    /// The one place a variant is mapped to a category, so [`record_bailout`]
    /// can bump its counter directly instead of string-searching the category
    /// table for the name this function just produced.
    fn category_index(&self) -> usize {
        match self {
            BailoutReason::GraphTooLarge { .. } => 0,
            BailoutReason::FrameTooLarge { .. } => 1,
            BailoutReason::UnsupportedOpcode { .. } => 2,
            BailoutReason::UnsupportedShape(_) => 3,
            BailoutReason::RegisterPressure => 4,
            BailoutReason::DisplacementOutOfRange { .. } => 5,
            BailoutReason::RelocationOutOfRange { .. } => 6,
            BailoutReason::CodeBufferExhausted { .. } => 7,
            BailoutReason::UnallocatedValue { .. } => 8,
            BailoutReason::IrVerification(_) => 9,
            BailoutReason::DeoptMetadata(_) => 10,
            BailoutReason::Internal(_) => 11,
        }
    }
}

impl std::fmt::Display for BailoutReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BailoutReason::GraphTooLarge { nodes, limit } => {
                write!(f, "IR graph too large: {nodes} nodes > limit {limit}")
            }
            BailoutReason::FrameTooLarge { bytes, limit } => {
                write!(f, "frame too large: {bytes} bytes > limit {limit}")
            }
            BailoutReason::UnsupportedOpcode { opcode } => {
                write!(f, "unsupported opcode 0x{opcode:02x}")
            }
            BailoutReason::UnsupportedShape(what) => write!(f, "unsupported shape: {what}"),
            BailoutReason::RegisterPressure => write!(f, "register pressure"),
            BailoutReason::DisplacementOutOfRange { disp } => {
                write!(f, "displacement out of range: {disp}")
            }
            BailoutReason::RelocationOutOfRange { delta } => {
                write!(f, "relocation out of range: {delta}")
            }
            BailoutReason::CodeBufferExhausted { needed, capacity } => {
                write!(
                    f,
                    "code buffer exhausted: needed {needed} bytes, capacity {capacity}"
                )
            }
            BailoutReason::UnallocatedValue { node } => {
                write!(f, "value n{node} read before a location was assigned")
            }
            BailoutReason::IrVerification(msg) => write!(f, "IR verification failed: {msg}"),
            BailoutReason::DeoptMetadata(msg) => {
                write!(f, "deopt metadata verification failed: {msg}")
            }
            BailoutReason::Internal(what) => write!(f, "internal compiler invariant: {what}"),
        }
    }
}

// ── Bailout ──────────────────────────────────────────────────────────

/// A structured compilation bailout: a [`BailoutReason`] plus optional
/// call-site context (usually the method identity and/or the pass name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bailout {
    /// Why the compile was abandoned.
    pub reason: BailoutReason,
    /// Free-form context — method name, pass name, bytecode pc. Owned rather
    /// than borrowed so a bailout can outlive the compiler state it describes.
    pub context: Option<String>,
}

impl Bailout {
    /// A bailout with no extra context.
    pub fn new(reason: BailoutReason) -> Self {
        Bailout {
            reason,
            context: None,
        }
    }

    /// A bailout carrying call-site context.
    pub fn with_context(reason: BailoutReason, context: impl Into<String>) -> Self {
        Bailout {
            reason,
            context: Some(context.into()),
        }
    }

    /// Short stable category name for metrics/logging, e.g. `"graph_too_large"`.
    pub fn category(&self) -> &'static str {
        self.reason.category()
    }
}

impl std::fmt::Display for Bailout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JIT bailout [{}]: {}", self.category(), self.reason)?;
        if let Some(ctx) = &self.context {
            write!(f, " ({ctx})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Bailout {}

/// Result of any fallible compiler step.
pub type CompileResult<T> = Result<T, Bailout>;

/// `return Err(Bailout::…)` from the enclosing function.
///
/// Two forms:
///
/// ```ignore
/// bail_compile!(BailoutReason::RegisterPressure);
/// bail_compile!(BailoutReason::UnsupportedOpcode { opcode }, "at pc {}", pc);
/// ```
///
/// The second form formats its context eagerly; that is fine because a bailout
/// is by construction off the hot path (it abandons a compile).
#[macro_export]
macro_rules! bail_compile {
    ($reason:expr $(,)?) => {
        return ::core::result::Result::Err($crate::bailout::Bailout::new($reason))
    };
    ($reason:expr, $($arg:tt)+) => {
        return ::core::result::Result::Err($crate::bailout::Bailout::with_context(
            $reason,
            ::std::format!($($arg)+),
        ))
    };
}

// ── Process-wide counters ────────────────────────────────────────────

/// Every category name, in the order [`bailout_counts`] reports them. The index
/// of a name here is its index into [`COUNTERS`]; `BailoutReason::category`
/// returns elements of this array so the two can never disagree.
const CATEGORIES: [&str; 12] = [
    "graph_too_large",
    "frame_too_large",
    "unsupported_opcode",
    "unsupported_shape",
    "register_pressure",
    "displacement_out_of_range",
    "relocation_out_of_range",
    "code_buffer_exhausted",
    "unallocated_value",
    "ir_verification",
    "deopt_metadata",
    "internal",
];

/// One relaxed counter per category. A fixed array rather than a map: the
/// category set is closed, so this needs no allocation, no lock, and no
/// initialization order.
static COUNTERS: [AtomicU64; CATEGORIES.len()] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Index of `category` in [`CATEGORIES`]. Pointer-equality is not relied on —
/// the comparison is by content, so a hand-built category name also resolves.
fn category_index(category: &str) -> Option<usize> {
    CATEGORIES.iter().position(|c| *c == category)
}

/// Count one bailout against its category.
///
/// Infallible and non-blocking: safe to call from any compiler phase,
/// including one already unwinding a failed compile.
pub fn record_bailout(bailout: &Bailout) {
    COUNTERS[bailout.reason.category_index()].fetch_add(1, Ordering::Relaxed);
}

/// Read every category's count.
///
/// Returns **all** categories, including zero-valued ones, in the fixed
/// [`CATEGORIES`] order — a metrics sink wants a stable row set, and "this
/// failure class never fired" is itself information. Counts are read with
/// relaxed ordering and are therefore a *sample*, not an atomic snapshot.
pub fn bailout_counts() -> Vec<(&'static str, u64)> {
    CATEGORIES
        .iter()
        .zip(COUNTERS.iter())
        .map(|(name, counter)| (*name, counter.load(Ordering::Relaxed)))
        .collect()
}

/// Read one category's count, or `None` if the name is not a known category.
pub fn bailout_count(category: &str) -> Option<u64> {
    category_index(category).map(|idx| COUNTERS[idx].load(Ordering::Relaxed))
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
