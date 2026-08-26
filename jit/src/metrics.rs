// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-compilation compiler metrics for the JIT.
//!
//! ## Why this exists
//!
//! The C2 review (`feature-designs/c2/deep-research-vm-c2.md`) has a P0 lane
//! "Measure compilation quality", whose acceptance criterion is: *"Per-method
//! compiler report is available without parsing debug logs."* Before this
//! module the only way to learn what the compiler did to a method was to set
//! one of a dozen `CRATONVM_DBG_*` flags and scrape stderr — and most of the
//! quantities the review asks for (phase wall time, node counts across passes,
//! frame bytes, code bytes, deopt metadata size) were never printed at all.
//!
//! [`CompilationReport`] is the structured answer: one record per
//! `try_compile_inner` invocation, published to a bounded in-process ring and
//! (optionally) to a JSON-lines file. Nothing here parses or produces log
//! text; [`compilation_reports`] and [`summary`] are the interface.
//!
//! ## Relationship to [`crate::bailout`]
//!
//! This module **consumes** the bailout machinery, it does not duplicate it.
//! [`crate::bailout::record_bailout`] owns the process-wide counters keyed by
//! [`crate::bailout::Bailout::category`]; [`summary`] reads them back through
//! [`crate::bailout::bailout_counts`]. What this module adds is *attribution*:
//! [`CompileRecorder::note_bailout`] attaches the same bailout to the specific
//! method and pipeline phase that produced it, which the counters cannot do.
//! `note_bailout` deliberately does **not** bump the process counters — the
//! call site that already calls `record_bailout` (`lib.rs::ir_verify_reject`)
//! would otherwise double-count.
//!
//! ## Measured vs. not measured
//!
//! Reporting an unmeasured quantity as `0` would be worse than useless — a
//! reader cannot tell "this method spilled nothing" from "nobody counts
//! spills". Every numeric field is therefore a [`Measured<T>`], which renders
//! as JSON `null` when no call site has supplied it, and that distinction is
//! load-bearing for the fields whose *producer* exists but whose *call site*
//! does not yet:
//!
//! * [`CompilationReport::peak_live_values`] is fed: `ir_lower`'s
//!   liveness-based frame-slot colouring computes the peak and
//!   [`note_current_peak_live_values`] carries it here.
//! * [`CompilationReport::spills`] / [`CompilationReport::reloads`] are fed on
//!   exactly one path: `ir_lower::lower_inner_with_scopes` with
//!   `CRATONVM_JIT_IR_LINEAR_SCAN` on, which runs
//!   `regalloc::allocate_linear_scan` and reports the register↔memory
//!   transitions it EMITTED through [`note_current_spills`] /
//!   [`note_current_reloads`] (the ambient form, because that function's
//!   signature is pinned and cannot take a recorder). The numbers describe
//!   generated code, not `regalloc::Allocation`'s plan: that wiring executes
//!   only the subset of the plan it can prove, so reporting
//!   `Allocation::spills` would describe instructions nobody emitted.
//!
//!   With the flag off — the default, and every compile today — the lowerer
//!   allocates no registers at all: every value lives in a frame slot and
//!   there is no register↔memory transition to count. Both stay
//!   `NotMeasured`, which is the honest answer. A reported `0` would be
//!   indistinguishable from "an allocator ran and spilled nothing", and that
//!   distinction is the entire reason [`Measured`] exists.
//!   `regalloc::record_allocation_metrics` remains the recorder-holding
//!   sibling of the two hooks, for a caller that has one.
//!
//!   See `docs/jit/linear-scan-wiring.md` for what that path does and does
//!   not do.
//! * the inlining tallies ([`CompilationReport::inline_candidates`] and
//!   friends) are harvested from `CompiledMethod::inline_tally` in
//!   [`CompileRecorder::installed`], so they are measured on every installed
//!   body and absent on every bailout — which is the truth, not a gap.
//!
//! `docs/jit/compiler-metrics.md` keeps the current inventory.
//!
//! Likewise [`Phase::Encode`] and [`Phase::Install`] always report
//! `not measured`: in this pipeline `ir_lower::lower_inner` and
//! `x64::compile_with_param_slots` lower, encode and install inside a single
//! call, so there is no boundary to time. The phases exist so that splitting
//! them later does not change the schema.
//!
//! ## Cost when disabled
//!
//! Collection is off unless `CRATONVM_JIT_METRICS=1`. When off,
//! [`CompileRecorder::begin`] performs one relaxed atomic load and returns a
//! handle whose `state` is `None`: no allocation, no clock read, no
//! thread-local touch, and every method on it is an `Option` test that returns
//! immediately. [`CompileRecorder::phase`] does not even call
//! [`std::time::Instant::now`].
//!
//! ## Flags
//!
//! | Flag | Meaning |
//! |---|---|
//! | `CRATONVM_JIT_METRICS=1` | enable collection (default off) |
//! | `CRATONVM_JIT_METRICS_RING=<n>` | retained report count (default [`DEFAULT_RING_CAPACITY`]) |
//! | `CRATONVM_JIT_METRICS_OUT=<path>` | append one JSON object per line per compilation |
//!
//! These are read through [`cratonvm_types::flags::runtime_var_os`] rather than
//! `std::env`, matching [`crate::ir_verify`]: they are not declared `VmFlags`
//! names, so the live-environment path is the one that sees them.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::Mutex;

use crate::bailout::{Bailout, BailoutReason};

// ── Enablement ───────────────────────────────────────────────────────

/// Default number of [`CompilationReport`]s retained in the ring.
///
/// 256 keeps the whole structure well under a megabyte for realistic methods
/// while covering the entire compile history of a small benchmark. Raise it
/// with `CRATONVM_JIT_METRICS_RING` when profiling a long run.
pub const DEFAULT_RING_CAPACITY: usize = 256;

/// Tri-state cache of the enable flag: `0` unresolved, `1` off, `2` on.
///
/// An `AtomicU8` rather than a `OnceLock<bool>` so the test helper can flip it;
/// the read is a single relaxed load either way.
static ENABLED: AtomicU8 = AtomicU8::new(0);

/// Parse a boolean-ish environment flag. `None` = unset / unparseable.
///
/// Deliberately a copy of `ir_verify`'s private helper rather than a shared
/// one: the two modules must be able to disagree about defaults without one
/// edit silently retuning the other.
fn env_flag(name: &str) -> Option<bool> {
    let raw = cratonvm_types::flags::runtime_var_os(name)?;
    let s = raw.to_str()?.trim().to_ascii_lowercase();
    match s.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Whether per-compilation metrics are collected.
///
/// Latched on first read. Off unless `CRATONVM_JIT_METRICS` is truthy —
/// including in debug builds, because unlike the IR verifier this is a
/// *measurement*, not a correctness gate, and a clock read per phase would
/// perturb the very compile times it reports.
#[inline]
pub fn enabled() -> bool {
    match ENABLED.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = env_flag("CRATONVM_JIT_METRICS").unwrap_or(false);
            ENABLED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Number of reports the ring retains. Latched on first read.
///
/// `CRATONVM_JIT_METRICS_RING=0` (or an unparseable value) falls back to
/// [`DEFAULT_RING_CAPACITY`]; a ring of zero would make
/// [`last_compilation_report`] permanently `None`, which is never what an
/// operator means.
pub fn ring_capacity() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_METRICS_RING")
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_RING_CAPACITY)
    })
}

// ── Measured<T> ──────────────────────────────────────────────────────

/// A quantity that is either measured or explicitly *not* measured.
///
/// The whole point of this type is that `NotMeasured` and `Value(0)` are
/// different answers. "This method spilled nothing" and "nothing in this build
/// counts spills" have to be distinguishable in the report, or every zero in
/// it becomes unfalsifiable. Renders as JSON `null` when not measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measured<T> {
    /// No call site supplied this quantity for this compilation.
    NotMeasured,
    /// The measured value.
    Value(T),
}

impl<T> Default for Measured<T> {
    fn default() -> Self {
        Measured::NotMeasured
    }
}

impl<T> Measured<T> {
    /// Wrap a measured value. (A `From<T>` impl would collide with core's
    /// blanket `impl<T> From<T> for T`, so this is a named constructor.)
    pub fn of(value: T) -> Self {
        Measured::Value(value)
    }

    /// Whether a value was supplied.
    pub fn is_measured(&self) -> bool {
        matches!(self, Measured::Value(_))
    }

    /// Borrow the value, if any.
    pub fn as_option(&self) -> Option<&T> {
        match self {
            Measured::Value(v) => Some(v),
            Measured::NotMeasured => None,
        }
    }
}

impl<T: Copy> Measured<T> {
    /// Copy the value out, if any.
    pub fn get(&self) -> Option<T> {
        match self {
            Measured::Value(v) => Some(*v),
            Measured::NotMeasured => None,
        }
    }

    /// The value, or `fallback` when not measured. Use only where the caller
    /// has already decided that conflating the two is acceptable.
    pub fn or(&self, fallback: T) -> T {
        self.get().unwrap_or(fallback)
    }
}

impl<T: std::fmt::Display> Measured<T> {
    /// JSON fragment: the value, or `null`.
    fn json(&self) -> String {
        match self {
            Measured::Value(v) => v.to_string(),
            Measured::NotMeasured => "null".to_string(),
        }
    }
}

// ── Phases ───────────────────────────────────────────────────────────

/// A timed stage of one compilation.
///
/// The set is fixed and ordered so a report always carries a row per phase —
/// including phases nothing currently times, which then read `not measured`.
/// Names are an external contract (they key the JSON and the summary), so they
/// must not be renamed with the variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// `x64::jit_scan` — bytecode walk and opcode admission.
    Scan,
    /// `ir::IrBuilder::build` — sea-of-nodes graph construction.
    Build,
    /// `ir_optimize::optimize` — the whole optimizer, as one pass. It does not
    /// expose its constituent passes (GVN, DCE, reassociation, LICM, unroll),
    /// so this is the finest grain reachable without editing that module.
    Optimize,
    /// `escape_analysis::analyze_escapes` + `apply_ea_to_ir`.
    EscapeAnalysis,
    /// `ir_verify::verify_graph`, summed over every lane it runs
    /// (post-optimize, post-escape-analysis, pre-lower).
    Verify,
    /// `ir_schedule::schedule`.
    Schedule,
    /// `ir_lower::lower_inner` — instruction selection **and** encoding **and**
    /// buffer install, which is why [`Phase::Encode`] and [`Phase::Install`]
    /// stay unmeasured on this path.
    Lower,
    /// Machine-code emission, if it is ever split out of lowering. Always
    /// `not measured` today.
    Encode,
    /// Executable-buffer publication, if it is ever split out of lowering.
    /// Always `not measured` today.
    Install,
    /// `x64::compile_with_param_slots` — the single-pass (C1) backend, whole.
    SinglePass,
}

impl Phase {
    /// Every phase, in pipeline order. The index of a phase here is its index
    /// into [`CompilationReport::phases`].
    pub const ALL: [Phase; 10] = [
        Phase::Scan,
        Phase::Build,
        Phase::Optimize,
        Phase::EscapeAnalysis,
        Phase::Verify,
        Phase::Schedule,
        Phase::Lower,
        Phase::Encode,
        Phase::Install,
        Phase::SinglePass,
    ];

    /// Stable metrics key.
    pub fn name(self) -> &'static str {
        match self {
            Phase::Scan => "scan",
            Phase::Build => "build",
            Phase::Optimize => "optimize",
            Phase::EscapeAnalysis => "escape_analysis",
            Phase::Verify => "verify",
            Phase::Schedule => "schedule",
            Phase::Lower => "lower",
            Phase::Encode => "encode",
            Phase::Install => "install",
            Phase::SinglePass => "single_pass",
        }
    }

    /// Index into [`Phase::ALL`] / [`CompilationReport::phases`].
    pub fn index(self) -> usize {
        match self {
            Phase::Scan => 0,
            Phase::Build => 1,
            Phase::Optimize => 2,
            Phase::EscapeAnalysis => 3,
            Phase::Verify => 4,
            Phase::Schedule => 5,
            Phase::Lower => 6,
            Phase::Encode => 7,
            Phase::Install => 8,
            Phase::SinglePass => 9,
        }
    }

    /// Resolve a phase from its stable key.
    pub fn from_name(name: &str) -> Option<Phase> {
        Phase::ALL.iter().copied().find(|p| p.name() == name)
    }
}

/// One phase's contribution to one compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseRecord {
    /// Which phase.
    pub phase: Phase,
    /// How many times it ran. `0` means "not instrumented, or never reached" —
    /// and is exactly the condition under which `wall_ns` is `NotMeasured`.
    pub runs: u32,
    /// Total wall time across `runs`, in nanoseconds. `NotMeasured` iff
    /// `runs == 0`, so a genuinely instantaneous phase still reports `0`.
    pub wall_ns: Measured<u64>,
    /// IR node count (`graph.nodes.len()`, including `Op::Dead`) entering the
    /// phase. Only the graph-mutating phases supply it.
    pub nodes_before: Measured<u32>,
    /// IR node count leaving the phase.
    pub nodes_after: Measured<u32>,
}

impl PhaseRecord {
    fn new(phase: Phase) -> Self {
        PhaseRecord {
            phase,
            runs: 0,
            wall_ns: Measured::NotMeasured,
            nodes_before: Measured::NotMeasured,
            nodes_after: Measured::NotMeasured,
        }
    }

    fn add_run(&mut self, ns: u64) {
        self.runs = self.runs.saturating_add(1);
        let prev = self.wall_ns.or(0);
        self.wall_ns = Measured::Value(prev.saturating_add(ns));
    }

    fn json(&self) -> String {
        format!(
            "{{\"phase\":\"{}\",\"runs\":{},\"wall_ns\":{},\"nodes_before\":{},\"nodes_after\":{}}}",
            self.phase.name(),
            self.runs,
            self.wall_ns.json(),
            self.nodes_before.json(),
            self.nodes_after.json(),
        )
    }
}

// ── Outcome / path ───────────────────────────────────────────────────

/// How a compilation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Still running. A published report never carries this — the recorder's
    /// `Drop` resolves it to [`Outcome::BailedOut`] or [`Outcome::Abandoned`].
    InProgress,
    /// A `CompiledMethod` was produced.
    Installed,
    /// Abandoned with at least one structured [`Bailout`] recorded.
    BailedOut,
    /// Abandoned with no structured reason — a resolver miss, an admission
    /// gate, or a backend that returned bare `None`. The size of this bucket
    /// is itself the finding: it counts the failure paths that still have no
    /// [`BailoutReason`].
    Abandoned,
}

impl Outcome {
    /// Stable metrics key.
    pub fn name(self) -> &'static str {
        match self {
            Outcome::InProgress => "in_progress",
            Outcome::Installed => "installed",
            Outcome::BailedOut => "bailed_out",
            Outcome::Abandoned => "abandoned",
        }
    }

    /// Every outcome, in summary order.
    pub const ALL: [Outcome; 4] = [
        Outcome::Installed,
        Outcome::BailedOut,
        Outcome::Abandoned,
        Outcome::InProgress,
    ];
}

/// Which backend the compilation actually reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilerPath {
    /// Neither backend was entered — the method died on an admission gate or a
    /// constant-pool resolver miss.
    NotEntered,
    /// The optimizing (sea-of-nodes, "C2") pipeline.
    Optimizing,
    /// The single-pass `x64` emitter ("C1"). Reached either directly
    /// (`optimize == false`) or by falling out of the optimizing pipeline —
    /// [`CompilationReport::fell_through_to_single_pass`] distinguishes them.
    SinglePass,
}

impl CompilerPath {
    /// Stable metrics key.
    pub fn name(self) -> &'static str {
        match self {
            CompilerPath::NotEntered => "not_entered",
            CompilerPath::Optimizing => "optimizing",
            CompilerPath::SinglePass => "single_pass",
        }
    }

    /// Every path, in summary order.
    pub const ALL: [CompilerPath; 3] = [
        CompilerPath::Optimizing,
        CompilerPath::SinglePass,
        CompilerPath::NotEntered,
    ];
}

/// A bailout, attributed to the phase and method that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BailoutRecord {
    /// [`Bailout::category`] — the same string [`crate::bailout::bailout_counts`] keys on.
    pub category: &'static str,
    /// Pipeline phase, as passed by the call site (e.g. `"pre-lower"`).
    pub phase: String,
    /// [`BailoutReason`] rendered through its `Display`.
    pub detail: String,
}

// ── The report ───────────────────────────────────────────────────────

/// Everything one compilation is known to have done.
///
/// Fields are public so a consumer can aggregate without going through
/// [`to_json`](Self::to_json). Every numeric field is a [`Measured`]: `null`
/// in the JSON means nothing measured it, never zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationReport {
    /// Monotonic sequence number, assigned at publication. Gaps mean the ring
    /// wrapped.
    pub seq: u64,
    /// Internal (slash-separated) class name.
    pub class_name: String,
    /// Method name.
    pub method_name: String,
    /// Method descriptor.
    pub descriptor: String,
    /// `true` when the caller asked for the optimizing tier (`optimize`), i.e.
    /// a C2 request; `false` is a C1 request.
    pub optimizing_requested: bool,
    /// Which backend was actually reached.
    pub path: CompilerPath,
    /// `true` when the optimizing pipeline was entered and then declined, so
    /// the single-pass backend produced (or failed to produce) the body.
    pub fell_through_to_single_pass: bool,
    /// Why the optimizing pipeline was or was not admitted — the same verdict
    /// string `CRATONVM_DBG_IR_COMPILES` prints, captured structurally.
    pub admission: Option<String>,
    /// How the compilation ended.
    pub outcome: Outcome,
    /// Every structured bailout observed, in the order they were recorded.
    pub bailouts: Vec<BailoutRecord>,
    /// One entry per [`Phase::ALL`], same order.
    pub phases: Vec<PhaseRecord>,
    /// `graph.nodes.len()` immediately after `IrBuilder::build`.
    pub nodes_built: Measured<u32>,
    /// `graph.nodes.len()` immediately before lowering.
    pub nodes_at_lower: Measured<u32>,
    /// Nodes at lowering that are not `Op::Dead`. The gap against
    /// `nodes_at_lower` is the arena the optimizer left behind. It is no longer
    /// also a frame-size figure: `ir_lower::estimate_frame_bytes` budgets the
    /// liveness-**coloured** slot count, so a dead arena node costs no frame
    /// bytes. Compare [`peak_live_values`](Self::peak_live_values) against this
    /// to see how much of the graph is simultaneously live.
    pub live_nodes_at_lower: Measured<u32>,
    /// Peak simultaneously-live values — the maximum number of live ranges
    /// covering any single program point, i.e. the floor a perfect frame-slot
    /// colouring would reach. Supplied by `ir_lower`'s slot planner
    /// (`SlotPlan::peak_live`) through
    /// [`note_current_peak_live_values`]. Optimizing path only; the single-pass
    /// backend computes no live ranges and leaves this unmeasured.
    pub peak_live_values: Measured<u32>,
    /// Register → memory transitions the backend **emitted**.
    ///
    /// Supplied by [`note_current_spills`] from
    /// `ir_lower::lower_inner_with_scopes` when `CRATONVM_JIT_IR_LINEAR_SCAN`
    /// is on, or by `regalloc::record_allocation_metrics` when a caller holds
    /// a recorder. `NotMeasured` on every path that allocates no registers,
    /// which is the default build: absence of an allocator is not a spill
    /// count of zero.
    pub spills: Measured<u32>,
    /// Memory → register transitions the backend **emitted**. See
    /// [`spills`](Self::spills) for who supplies it.
    pub reloads: Measured<u32>,
    /// `CompiledMethod::frame_layout.frame_size` — bytes subtracted from RSP.
    pub frame_bytes: Measured<u32>,
    /// Emitted machine-code bytes (`CompiledMethod::code_bytes().len()`), i.e.
    /// the buffer's write position, not its mapped capacity.
    pub code_bytes: Measured<u32>,
    /// Call sites the inliner considered — `crate::InlineDecisionTally::
    /// candidates`. Harvested from the artifact in
    /// [`CompileRecorder::installed`], so it is `NotMeasured` on every report
    /// that produced no body. Note the tally's own caveat: a site the invoke
    /// loop short-circuits once the expansion budget is exhausted is never
    /// counted, so this is "sites the resolver was asked about".
    pub inline_candidates: Measured<u32>,
    /// Sites actually inlined (`InlineDecisionTally::inlined_sites`).
    pub inlined_sites: Measured<u32>,
    /// Of [`inlined_sites`](Self::inlined_sites), those resting on a receiver-type
    /// speculation and its guard (`InlineDecisionTally::speculative_sites`).
    pub speculative_inlined_sites: Measured<u32>,
    /// Callee bytecodes spliced into this body — the review's "inlined
    /// bytecodes" (`InlineDecisionTally::inlined_bytecodes`).
    pub inlined_bytecodes: Measured<u32>,
    /// Summed profile-observed executions of the inlined sites — the review's
    /// "call count" (`InlineDecisionTally::observed_calls_inlined`). A measured
    /// `0` here means "inlined, but the compile was unprofiled", which is a
    /// different claim from `null` ("nothing harvested a tally").
    pub inlined_call_count: Measured<u64>,
    /// Refusals by `crate::InlineRefusal::category`, in the tally's first-seen
    /// order. Empty is *not* the same as absent: check
    /// [`inline_candidates`](Self::inline_candidates) to tell "no site was
    /// refused" from "no tally was harvested".
    pub inline_refusals: Vec<(String, u32)>,
    /// `graph.safepoints.len()` at lowering — the builder's snapshot count.
    /// Optimizing path only.
    pub ir_safepoints: Measured<u32>,
    /// `CompiledMethod::oop_maps.len()`. Zero without
    /// `CRATONVM_PRECISE_JIT_MAPS`, which is a real fact about the artifact
    /// (the GC falls back to the conservative scan), not a measurement gap.
    pub oop_maps: Measured<u32>,
    /// `CompiledMethod::deopt_points.len()`.
    pub deopt_points: Measured<u32>,
    /// Estimated resident bytes of deopt metadata: the `DeoptimizationPoint`
    /// vector and the boxed points, plus the heap owned by each `FrameState`
    /// (method key, locals, stack, monitors, inlined caller chain). An
    /// estimate because a `FrameValue::VirtualObject` owns further heap this
    /// walk does not follow.
    pub deopt_metadata_bytes: Measured<u64>,
    /// [`crate::COMMITTED_JIT_CODE_BYTES`] read at install — live code-cache
    /// occupancy including this method.
    pub code_cache_bytes_at_install: Measured<u64>,
    /// Wall time from [`CompileRecorder::begin`] to publication. Measured on
    /// every published report, including bailouts.
    pub total_wall_ns: Measured<u64>,
}

impl CompilationReport {
    /// A report with identity filled in and every measurement absent.
    pub fn new(class_name: &str, method_name: &str, descriptor: &str, optimizing: bool) -> Self {
        CompilationReport {
            seq: 0,
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
            optimizing_requested: optimizing,
            path: CompilerPath::NotEntered,
            fell_through_to_single_pass: false,
            admission: None,
            outcome: Outcome::InProgress,
            bailouts: Vec::new(),
            phases: Phase::ALL.iter().map(|p| PhaseRecord::new(*p)).collect(),
            nodes_built: Measured::NotMeasured,
            nodes_at_lower: Measured::NotMeasured,
            live_nodes_at_lower: Measured::NotMeasured,
            peak_live_values: Measured::NotMeasured,
            spills: Measured::NotMeasured,
            reloads: Measured::NotMeasured,
            frame_bytes: Measured::NotMeasured,
            code_bytes: Measured::NotMeasured,
            inline_candidates: Measured::NotMeasured,
            inlined_sites: Measured::NotMeasured,
            speculative_inlined_sites: Measured::NotMeasured,
            inlined_bytecodes: Measured::NotMeasured,
            inlined_call_count: Measured::NotMeasured,
            inline_refusals: Vec::new(),
            ir_safepoints: Measured::NotMeasured,
            oop_maps: Measured::NotMeasured,
            deopt_points: Measured::NotMeasured,
            deopt_metadata_bytes: Measured::NotMeasured,
            code_cache_bytes_at_install: Measured::NotMeasured,
            total_wall_ns: Measured::NotMeasured,
        }
    }

    /// `Class.name descriptor` as one grep-able key.
    pub fn method_key(&self) -> String {
        format!("{}.{}{}", self.class_name, self.method_name, self.descriptor)
    }

    /// This compilation's record for `phase`.
    pub fn phase(&self, phase: Phase) -> Option<&PhaseRecord> {
        self.phases.get(phase.index())
    }

    /// One JSON object, on one line — the unit of the
    /// `CRATONVM_JIT_METRICS_OUT` file. Hand-rolled because the `jit` crate has
    /// no serialization dependency and adding one for a diagnostic would be a
    /// poor trade.
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(1024);
        s.push('{');
        let _ = write!(
            s,
            "\"seq\":{},\"class\":\"{}\",\"method\":\"{}\",\"descriptor\":\"{}\"",
            self.seq,
            json_escape(&self.class_name),
            json_escape(&self.method_name),
            json_escape(&self.descriptor),
        );
        let _ = write!(
            s,
            ",\"tier_requested\":\"{}\",\"path\":\"{}\",\"fell_through_to_single_pass\":{}",
            if self.optimizing_requested { "c2" } else { "c1" },
            self.path.name(),
            self.fell_through_to_single_pass,
        );
        match &self.admission {
            Some(a) => {
                let _ = write!(s, ",\"admission\":\"{}\"", json_escape(a));
            }
            None => s.push_str(",\"admission\":null"),
        }
        let _ = write!(s, ",\"outcome\":\"{}\"", self.outcome.name());
        s.push_str(",\"bailouts\":[");
        for (i, b) in self.bailouts.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "{{\"category\":\"{}\",\"phase\":\"{}\",\"detail\":\"{}\"}}",
                json_escape(b.category),
                json_escape(&b.phase),
                json_escape(&b.detail),
            );
        }
        s.push_str("],\"phases\":[");
        for (i, p) in self.phases.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&p.json());
        }
        s.push(']');
        let _ = write!(
            s,
            ",\"nodes_built\":{},\"nodes_at_lower\":{},\"live_nodes_at_lower\":{}",
            self.nodes_built.json(),
            self.nodes_at_lower.json(),
            self.live_nodes_at_lower.json(),
        );
        let _ = write!(
            s,
            ",\"peak_live_values\":{},\"spills\":{},\"reloads\":{}",
            self.peak_live_values.json(),
            self.spills.json(),
            self.reloads.json(),
        );
        let _ = write!(
            s,
            ",\"frame_bytes\":{},\"code_bytes\":{},\"ir_safepoints\":{},\"oop_maps\":{}",
            self.frame_bytes.json(),
            self.code_bytes.json(),
            self.ir_safepoints.json(),
            self.oop_maps.json(),
        );
        let _ = write!(
            s,
            ",\"deopt_points\":{},\"deopt_metadata_bytes\":{},\"code_cache_bytes_at_install\":{}",
            self.deopt_points.json(),
            self.deopt_metadata_bytes.json(),
            self.code_cache_bytes_at_install.json(),
        );
        let _ = write!(
            s,
            ",\"inline_candidates\":{},\"inlined_sites\":{},\"speculative_inlined_sites\":{}",
            self.inline_candidates.json(),
            self.inlined_sites.json(),
            self.speculative_inlined_sites.json(),
        );
        let _ = write!(
            s,
            ",\"inlined_bytecodes\":{},\"inlined_call_count\":{}",
            self.inlined_bytecodes.json(),
            self.inlined_call_count.json(),
        );
        // A nested object rather than an array of pairs: the categories are a
        // fixed vocabulary (`InlineRefusal::category`), so a consumer keys on
        // them directly. Emitted even when empty, so `inline_refusals` is never
        // a missing key.
        s.push_str(",\"inline_refusals\":{");
        for (i, (category, count)) in self.inline_refusals.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "\"{}\":{}", json_escape(category), count);
        }
        s.push('}');
        let _ = write!(s, ",\"total_wall_ns\":{}", self.total_wall_ns.json());
        s.push('}');
        s
    }
}

/// Escape a string for a JSON double-quoted scalar.
///
/// Class names, descriptors and bailout detail strings are all attacker-free
/// but not character-free (a `Bailout` detail can carry any byte a verifier
/// message contains), and one unescaped control character would make the whole
/// JSON-lines file unparseable.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

// ── Recorder ─────────────────────────────────────────────────────────

/// Shared per-compilation state. `RefCell` (not a lock): a compilation runs
/// synchronously on one thread, and every borrow here is a `try_borrow_mut`
/// so even a re-entrant call degrades to "this datum is not recorded" rather
/// than a panic in the compiler.
struct RecorderState {
    report: RefCell<CompilationReport>,
    start: Instant,
}

thread_local! {
    /// Stack of in-flight compilations on this thread. A stack, not a slot:
    /// `callee_compiler` re-enters `try_compile` for an inlining candidate, so
    /// compilations nest. The innermost one owns
    /// [`current_phase`] / [`note_current_bailout`].
    static ACTIVE: RefCell<Vec<Rc<RecorderState>>> = const { RefCell::new(Vec::new()) };
}

/// The innermost in-flight compilation on this thread, if any.
///
/// Returns `None` immediately when metrics are off, so a TLS-based hook costs
/// one relaxed atomic load in the default configuration. `try_with` rather
/// than `with` because these hooks can run during thread teardown.
fn current() -> Option<Rc<RecorderState>> {
    if !enabled() {
        return None;
    }
    ACTIVE
        .try_with(|s| s.try_borrow().ok().and_then(|stack| stack.last().cloned()))
        .ok()
        .flatten()
}

/// A scope guard that charges its lifetime to one [`Phase`].
///
/// Constructed by [`CompileRecorder::phase`] or [`current_phase`], so
/// instrumenting a call site is one line:
///
/// ```ignore
/// let t = metrics.phase(metrics::Phase::Schedule);
/// let schedule = ir_schedule::schedule(&graph);
/// drop(t);
/// ```
///
/// When metrics are off it holds no state and never reads the clock.
pub struct PhaseTimer {
    state: Option<Rc<RecorderState>>,
    phase: Phase,
    start: Option<Instant>,
}

impl PhaseTimer {
    fn new(state: Option<Rc<RecorderState>>, phase: Phase) -> Self {
        // `Instant::now` only when something will consume the result.
        let start = state.as_ref().map(|_| Instant::now());
        PhaseTimer {
            state,
            phase,
            start,
        }
    }

    /// A timer that measures nothing. Cheap enough to construct
    /// unconditionally.
    pub fn disabled() -> Self {
        PhaseTimer {
            state: None,
            phase: Phase::Scan,
            start: None,
        }
    }

    /// Which phase this timer charges.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether this timer is actually measuring.
    pub fn is_measuring(&self) -> bool {
        self.start.is_some()
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let Some(start) = self.start else {
            return;
        };
        let ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        if let Ok(mut report) = state.report.try_borrow_mut() {
            if let Some(rec) = report.phases.get_mut(self.phase.index()) {
                rec.add_run(ns);
            }
        }
    }
}

/// Start timing `phase` against the innermost in-flight compilation on this
/// thread.
///
/// This is the hook for helpers that do not have the recorder in scope —
/// `lib.rs::ir_verify_reject` is the motivating one: it is called from three
/// places inside `try_compile_inner` and taking a recorder parameter would
/// change its signature.
pub fn current_phase(phase: Phase) -> PhaseTimer {
    PhaseTimer::new(current(), phase)
}

/// Record the peak simultaneously-live value count against the innermost
/// in-flight compilation on this thread.
///
/// The hook for `ir_lower::lower_inner`, which is the only place in the compiler
/// that computes this number ([`crate::ir_lower`]'s liveness-based frame-slot
/// colouring produces it as a by-product) and whose signature is pinned, so it
/// cannot take a recorder parameter. Same shape and same reason as
/// [`note_current_bailout`].
pub fn note_current_peak_live_values(n: usize) {
    let Some(state) = current() else {
        return;
    };
    // The `Result<RefMut<..>, _>` scrutinee is a temporary whose drop would
    // otherwise run after `state`; the trailing semicolon ends its scope first.
    if let Ok(mut report) = state.report.try_borrow_mut() {
        report.peak_live_values = Measured::Value(n.min(u32::MAX as usize) as u32);
    };
}

/// Record the emitted spill count against the innermost in-flight compilation
/// on this thread.
///
/// Two ways to get a spill count here, for two different callers:
///
/// * `regalloc::record_allocation_metrics(&recorder, &alloc)` — for a caller
///   that holds a [`CompileRecorder`], reporting what the *allocation* plans;
/// * this function — for a caller that does not, because its signature is
///   pinned. `ir_lower::lower_inner_with_scopes` is that caller: it takes no
///   recorder and already reports [`note_current_peak_live_values`] the same
///   way, so wiring the allocator into it must not widen its parameter list.
///   It passes what it **emitted**, which is the smaller number — its wiring
///   executes only the subset of the allocation it can prove.
///
/// Only call this from a path that actually allocated registers. Leaving the
/// field `NotMeasured` is the correct report for a backend that keeps every
/// value in a frame slot; a `0` there would claim a measurement nobody made.
///
/// Same shape and same reason as [`note_current_bailout`]. A no-op when metrics
/// are off, and a no-op when no compilation is in flight — so an allocator run
/// from a unit test records nothing.
pub fn note_current_spills(n: usize) {
    let Some(state) = current() else {
        return;
    };
    // The `Result<RefMut<..>, _>` scrutinee is a temporary whose drop would
    // otherwise run after `state`; the trailing semicolon ends its scope first.
    if let Ok(mut report) = state.report.try_borrow_mut() {
        report.spills = Measured::Value(n.min(u32::MAX as usize) as u32);
    };
}

/// Record the allocator's reload count against the innermost in-flight
/// compilation on this thread. See [`note_current_spills`].
pub fn note_current_reloads(n: usize) {
    let Some(state) = current() else {
        return;
    };
    // The `Result<RefMut<..>, _>` scrutinee is a temporary whose drop would
    // otherwise run after `state`; the trailing semicolon ends its scope first.
    if let Ok(mut report) = state.report.try_borrow_mut() {
        report.reloads = Measured::Value(n.min(u32::MAX as usize) as u32);
    };
}

/// Attribute `bailout` to the innermost in-flight compilation on this thread.
///
/// Does **not** touch [`crate::bailout`]'s process-wide counters — the call
/// site that produced the bailout owns that (see the module docs).
pub fn note_current_bailout(bailout: &Bailout, phase: &str) {
    let Some(state) = current() else {
        return;
    };
    // The `Result<RefMut<..>, _>` scrutinee is a temporary whose drop would
    // otherwise run after `state`; the trailing semicolon ends its scope first.
    if let Ok(mut report) = state.report.try_borrow_mut() {
        push_bailout(&mut report, bailout, phase);
    };
}

fn push_bailout(report: &mut CompilationReport, bailout: &Bailout, phase: &str) {
    // A shredded graph can produce a bailout per pass; the report is a
    // diagnostic, not a transcript, so bound it.
    const MAX_BAILOUTS_PER_REPORT: usize = 16;
    if report.bailouts.len() >= MAX_BAILOUTS_PER_REPORT {
        return;
    }
    report.bailouts.push(BailoutRecord {
        category: bailout.category(),
        phase: phase.to_string(),
        detail: bailout.to_string(),
    });
}

/// The handle a compilation holds for its whole duration.
///
/// Create one with [`CompileRecorder::begin`] at the top of the compile entry
/// point and let it drop: publication happens in `Drop`, so *every* exit path
/// — including the `?` on the single-pass backend and each of the ~40
/// resolver-miss `return None`s — produces a report without any control-flow
/// change at the call site.
pub struct CompileRecorder {
    state: Option<Rc<RecorderState>>,
}

impl CompileRecorder {
    /// Begin recording a compilation of `class.method descriptor`.
    ///
    /// `optimizing` is the caller's `optimize` flag (C2 requested vs C1).
    /// Returns a no-op handle when metrics are off — one relaxed atomic load,
    /// no allocation.
    pub fn begin(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        optimizing: bool,
    ) -> CompileRecorder {
        if !enabled() {
            return CompileRecorder { state: None };
        }
        let state = Rc::new(RecorderState {
            report: RefCell::new(CompilationReport::new(
                class_name,
                method_name,
                descriptor,
                optimizing,
            )),
            start: Instant::now(),
        });
        // If the TLS is already being torn down we still return a live
        // recorder; it simply will not be visible to `current_phase`.
        let _ = ACTIVE.try_with(|s| {
            if let Ok(mut stack) = s.try_borrow_mut() {
                stack.push(Rc::clone(&state));
            }
        });
        CompileRecorder { state: Some(state) }
    }

    /// A handle that records nothing, regardless of the enable flag. Used by
    /// tests and by any caller that wants an explicit opt-out.
    pub fn disabled() -> CompileRecorder {
        CompileRecorder { state: None }
    }

    /// Whether this handle is recording.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.state.is_some()
    }

    #[inline]
    fn with_report(&self, f: impl FnOnce(&mut CompilationReport)) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        if let Ok(mut report) = state.report.try_borrow_mut() {
            f(&mut report);
        }
    }

    /// Start timing `phase`. The returned guard charges its lifetime on drop.
    #[inline]
    pub fn phase(&self, phase: Phase) -> PhaseTimer {
        PhaseTimer::new(self.state.clone(), phase)
    }

    /// Record the IR node count entering and leaving `phase`.
    pub fn phase_nodes(&self, phase: Phase, before: usize, after: usize) {
        self.with_report(|r| {
            if let Some(rec) = r.phases.get_mut(phase.index()) {
                rec.nodes_before = Measured::Value(before.min(u32::MAX as usize) as u32);
                rec.nodes_after = Measured::Value(after.min(u32::MAX as usize) as u32);
            }
        });
    }

    /// Record why the optimizing pipeline was (or was not) admitted.
    ///
    /// Takes `&str` and copies only when recording, so a caller that builds
    /// the string lazily pays nothing when metrics are off.
    pub fn set_admission(&self, verdict: &str) {
        self.with_report(|r| r.admission = Some(verdict.to_string()));
    }

    /// The optimizing (sea-of-nodes) pipeline was entered.
    pub fn enter_optimizing_pipeline(&self) {
        self.with_report(|r| r.path = CompilerPath::Optimizing);
    }

    /// The single-pass backend path was entered. If the optimizing pipeline
    /// had already been entered, this is a fall-through and is flagged as one.
    pub fn enter_single_pass(&self) {
        self.with_report(|r| {
            if r.path == CompilerPath::Optimizing {
                r.fell_through_to_single_pass = true;
            }
            r.path = CompilerPath::SinglePass;
        });
    }

    /// Node count immediately after `IrBuilder::build`.
    pub fn set_nodes_built(&self, nodes: usize) {
        self.with_report(|r| r.nodes_built = Measured::Value(nodes.min(u32::MAX as usize) as u32));
    }

    /// Graph shape immediately before lowering.
    pub fn set_graph_at_lower(&self, nodes: usize, live_nodes: usize, safepoints: usize) {
        self.with_report(|r| {
            r.nodes_at_lower = Measured::Value(nodes.min(u32::MAX as usize) as u32);
            r.live_nodes_at_lower = Measured::Value(live_nodes.min(u32::MAX as usize) as u32);
            r.ir_safepoints = Measured::Value(safepoints.min(u32::MAX as usize) as u32);
        });
    }

    /// Peak simultaneously-live values, recorded against *this* recorder. The
    /// compiler itself records through [`note_current_peak_live_values`]
    /// (`ir_lower::lower_inner` has no recorder in scope); this form exists for
    /// a caller that holds one. See [`CompilationReport::peak_live_values`].
    pub fn set_peak_live_values(&self, n: usize) {
        self.with_report(|r| {
            r.peak_live_values = Measured::Value(n.min(u32::MAX as usize) as u32)
        });
    }

    /// Spill count, recorded against *this* recorder — the form
    /// `regalloc::record_allocation_metrics` uses. A caller whose signature
    /// cannot carry a recorder uses [`note_current_spills`] instead. See
    /// [`CompilationReport::spills`].
    pub fn set_spills(&self, n: usize) {
        self.with_report(|r| r.spills = Measured::Value(n.min(u32::MAX as usize) as u32));
    }

    /// Reload count. Same two forms as [`set_spills`](Self::set_spills); see
    /// [`CompilationReport::spills`].
    pub fn set_reloads(&self, n: usize) {
        self.with_report(|r| r.reloads = Measured::Value(n.min(u32::MAX as usize) as u32));
    }

    /// Attribute a structured bailout to this compilation. Does not touch
    /// [`crate::bailout`]'s counters.
    pub fn note_bailout(&self, bailout: &Bailout, phase: &str) {
        self.with_report(|r| push_bailout(r, bailout, phase));
    }

    /// Attribute a [`BailoutReason`] that has no [`Bailout`] value at the call
    /// site (the pipeline still has `Option`-returning steps).
    pub fn note_bailout_reason(&self, reason: BailoutReason, phase: &str) {
        let bailout = Bailout::new(reason);
        self.with_report(|r| push_bailout(r, &bailout, phase));
    }

    /// A body was produced: harvest everything the artifact knows about
    /// itself.
    ///
    /// Reads only public accessors of [`crate::CompiledMethod`], so it cannot
    /// perturb the artifact. `used_ir_backend` is the artifact's own answer to
    /// "which backend made me", so it overrides whatever the path tracking
    /// believed.
    pub fn installed(&self, cm: &crate::CompiledMethod) {
        let code_bytes = cm.code_bytes().len();
        let frame_bytes = cm.frame_layout.frame_size.max(0) as u64;
        let oop_maps = cm.oop_maps.len();
        let deopt_points = cm.deopt_points.len();
        let deopt_bytes = deopt_metadata_bytes(cm);
        let cache_bytes =
            crate::COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed) as u64;
        let used_ir = cm.used_ir_backend;
        // The inlining tallies ride on the artifact for exactly this reason:
        // harvesting them here costs nothing on the ~40 `return None` paths
        // through `try_compile_inner` and cannot go stale, because it is the
        // installed body's own record of what was spliced into it. Borrowed
        // rather than cloned up front: the refusal histogram is the only
        // allocation in this method, and `with_report` never runs the closure
        // when metrics are off.
        let tally = &cm.inline_tally;
        self.with_report(|r| {
            r.outcome = Outcome::Installed;
            r.path = if used_ir {
                CompilerPath::Optimizing
            } else {
                CompilerPath::SinglePass
            };
            r.code_bytes = Measured::Value(code_bytes.min(u32::MAX as usize) as u32);
            r.frame_bytes = Measured::Value(frame_bytes.min(u32::MAX as u64) as u32);
            r.oop_maps = Measured::Value(oop_maps.min(u32::MAX as usize) as u32);
            r.deopt_points = Measured::Value(deopt_points.min(u32::MAX as usize) as u32);
            r.deopt_metadata_bytes = Measured::Value(deopt_bytes);
            r.code_cache_bytes_at_install = Measured::Value(cache_bytes);
            r.inline_candidates = Measured::Value(tally.candidates);
            r.inlined_sites = Measured::Value(tally.inlined_sites);
            r.speculative_inlined_sites = Measured::Value(tally.speculative_sites);
            r.inlined_bytecodes = Measured::Value(tally.inlined_bytecodes);
            r.inlined_call_count = Measured::Value(tally.observed_calls_inlined);
            r.inline_refusals = tally
                .refusals
                .iter()
                .map(|(c, n)| ((*c).to_string(), *n))
                .collect();
        });
    }

    /// A snapshot of the in-flight report. Test/introspection helper; the
    /// authoritative copy is the one `Drop` publishes.
    pub fn snapshot(&self) -> Option<CompilationReport> {
        let state = self.state.as_ref()?;
        state.report.try_borrow().ok().map(|r| (*r).clone())
    }
}

impl Drop for CompileRecorder {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let _ = ACTIVE.try_with(|s| {
            if let Ok(mut stack) = s.try_borrow_mut() {
                if let Some(pos) = stack.iter().rposition(|e| Rc::ptr_eq(e, &state)) {
                    stack.remove(pos);
                }
            }
        });
        let snapshot = {
            let Ok(mut report) = state.report.try_borrow_mut() else {
                // A live borrow at drop time means a PhaseTimer outlived its
                // recorder. Publishing nothing is strictly better than
                // panicking inside the compiler.
                return;
            };
            report.total_wall_ns = Measured::Value(
                state.start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            );
            if report.outcome == Outcome::InProgress {
                report.outcome = if report.bailouts.is_empty() {
                    Outcome::Abandoned
                } else {
                    Outcome::BailedOut
                };
            }
            report.clone()
        };
        record_report(snapshot);
    }
}

/// Estimated resident bytes of an artifact's deopt metadata.
fn deopt_metadata_bytes(cm: &crate::CompiledMethod) -> u64 {
    use crate::deopt::DeoptimizationPoint;
    let point = std::mem::size_of::<DeoptimizationPoint>();
    let mut total: usize = 0;
    for p in &cm.deopt_points {
        total = total.saturating_add(point);
        total = total.saturating_add(frame_state_heap_bytes(&p.frame_state));
    }
    for p in &cm._deopt_point_boxes {
        total = total.saturating_add(point);
        total = total.saturating_add(frame_state_heap_bytes(&p.frame_state));
    }
    total as u64
}

/// Heap owned by a `FrameState`, excluding the struct itself.
fn frame_state_heap_bytes(fs: &crate::deopt::FrameState) -> usize {
    use crate::deopt::{FrameState, FrameValue, MonitorInfo};
    let value = std::mem::size_of::<FrameValue>();
    let mut n = fs.method_key.len();
    n = n.saturating_add(fs.locals.len().saturating_mul(value));
    n = n.saturating_add(fs.stack.len().saturating_mul(value));
    n = n.saturating_add(
        fs.monitors
            .len()
            .saturating_mul(std::mem::size_of::<MonitorInfo>()),
    );
    if let Some(caller) = &fs.caller {
        n = n.saturating_add(std::mem::size_of::<FrameState>());
        n = n.saturating_add(frame_state_heap_bytes(caller));
    }
    n
}

// ── Publication ──────────────────────────────────────────────────────

/// Reports published so far, including any the ring has since evicted.
static TOTAL_RECORDED: AtomicU64 = AtomicU64::new(0);

fn ring() -> &'static Mutex<VecDeque<CompilationReport>> {
    static RING: OnceLock<Mutex<VecDeque<CompilationReport>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(ring_capacity().min(4096))))
}

/// The JSON-lines sink, or `None` when `CRATONVM_JIT_METRICS_OUT` is unset or
/// the path could not be opened.
///
/// An unopenable path is silently ignored: a metrics sink that aborts a JVM
/// because a directory is read-only would be a worse defect than the missing
/// measurement.
fn json_sink() -> Option<&'static Mutex<std::fs::File>> {
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| -> Option<Mutex<std::fs::File>> {
        let path = cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_METRICS_OUT")?;
        if path.is_empty() {
            return None;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// Publish a report: stamp its sequence number, push it into the bounded ring,
/// and append it to the JSON-lines sink when one is configured.
///
/// Public so a test (or an out-of-crate harness) can seed the ring without
/// running a compile.
pub fn record_report(mut report: CompilationReport) {
    let seq = TOTAL_RECORDED.fetch_add(1, Ordering::Relaxed) + 1;
    report.seq = seq;
    if let Some(sink) = json_sink() {
        use std::io::Write as _;
        let line = report.to_json();
        let mut file = sink.lock();
        let _ = writeln!(file, "{line}");
    }
    let cap = ring_capacity();
    let mut ring = ring().lock();
    while ring.len() >= cap {
        let _ = ring.pop_front();
    }
    ring.push_back(report);
}

/// Every retained report, oldest first.
pub fn compilation_reports() -> Vec<CompilationReport> {
    ring().lock().iter().cloned().collect()
}

/// The most recent retained report, optionally restricted to methods whose
/// `Class.name descriptor` key contains `filter`.
///
/// `last_compilation_report(None)` answers "what did the compiler just do?";
/// `last_compilation_report(Some("HashMap.hash"))` answers "what did it do to
/// *this* method?" without the caller scanning the ring.
pub fn last_compilation_report(filter: Option<&str>) -> Option<CompilationReport> {
    let ring = ring().lock();
    match filter {
        None => ring.back().cloned(),
        Some(needle) => ring
            .iter()
            .rev()
            .find(|r| r.method_key().contains(needle))
            .cloned(),
    }
}

/// Drop every retained report. Does not reset [`MetricsSummary::total_recorded`],
/// which is what makes "the ring wrapped" detectable.
pub fn clear_reports() {
    ring().lock().clear();
}

// ── Scheduling counters ──────────────────────────────────────────────
//
// Everything above this line describes a compilation that *ran*. These count
// the compilations that never ran because the scheduler threw the request
// away, which is the one class of event a per-compilation report structurally
// cannot carry: there is no report, because there was no compile.
//
// That gap is why they exist. A request the tier manager drops silently is a
// method that stays interpreted forever, and nothing downstream can tell it
// apart from a method that was never hot — no bailout, no `Outcome`, no ring
// entry, no log line. `docs/jit/broker-install-epoch.md` is the write-up.
//
// The design is a deliberate copy of [`crate::bailout`]'s: a fixed array of
// `&'static str` names and a parallel array of relaxed counters, read back as
// `Vec<(&'static str, u64)>` in a stable order including the zeroes. Two
// consequences are load-bearing and match that module:
//
//   * The names are an **external contract** — a test or a dashboard keys on
//     them — so they must not be renamed along with any field.
//   * The counters are **not gated on [`enabled`]**. A dropped request is a
//     correctness-adjacent event, not a measurement, and it has to be visible
//     in a default production run where `CRATONVM_JIT_METRICS` is unset.

/// Every OSR lifecycle event, in the fixed order [`osr_counts`] reports.
///
/// `docs/feature-designs/jit-osr-exit-and-recompile.md`. The reason these are
/// ungated and always on: **a silent OSR exit is indistinguishable from never
/// having entered.** Both leave the method running in the interpreter with a
/// correct answer and no diagnostic, so an OSR pipeline that enters and
/// immediately bails on every iteration looks exactly like one that is simply
/// not triggering — and the second is a tuning question while the first is a
/// livelock. Nothing in a default run could tell them apart before this.
///
/// Same shape as [`SCHEDULING_EVENTS`]: a closed set, a fixed array of relaxed
/// counters, no allocation and no initialization order.
pub const OSR_EVENTS: [&str; 13] = [
    // An OSR entry was actually taken: the trampoline ran and control reached
    // compiled code at a back edge. The denominator for everything below.
    "osr_entered",
    // An entered OSR frame bailed back to the interpreter — the compiled body
    // returned the `i64::MIN` sentinel or signalled a deopt. `osr_exited`
    // approaching `osr_entered` is the shape the livelock takes: every entry
    // paying the trampoline and the seed, then leaving immediately.
    "osr_exited",
    // A back edge asked to enter and was refused, either because the artifact
    // publishes no enterable offset for that bci or because
    // `validate_osr_entry` rejected the live state. Expected to be non-zero;
    // interesting only next to `osr_entered`.
    "osr_refused_entry",
    // The OSR compile produced no artifact at all. Distinct from a refusal:
    // nothing was built, so no `osr_pc_to_native` verdict exists to memo, and
    // the per-pc reject memo cannot suppress the next request.
    "osr_compile_declined",
    // ── Where the exit landed ────────────────────────────────────────────
    //
    // `osr_exit_points` "exists and is populated … but nothing cross-checks it
    // against where exits are actually taken" — the lane's step 4. These four
    // partition `osr_exited`'s stashed-frame half exactly
    // (`crate::osr_exit::OsrExitSite`), so their sum is the number of exits
    // that arrived carrying a reconstructed frame.
    //
    // An exit at a true loop-boundary map — in `osr_exit_points` AND recorded
    // with reason `OsrExit`. A whole number of iterations completed and the
    // header has not been re-entered: the shape the in-place transfer was
    // designed for.
    "osr_exit_at_loop_boundary",
    // An exit at a recorded deopt point that is NOT a loop boundary — an
    // `invokedynamic` uncommon trap (which shares the exit-map machinery, so
    // set membership alone could never have told the two apart), or a
    // speculative-BCE guard. Legitimate. Read it against the row above: an OSR
    // population that leaves predominantly off the loop boundary is entering
    // bodies that trap, not bodies that run.
    "osr_exit_off_loop_boundary",
    // The two sets disagree: an `OsrExit`-reason point whose bci is missing
    // from `osr_exit_points`. One function writes both, and the loop transform
    // moves both between coordinate spaces, so **this must read zero** — it is
    // the cross-check itself, not a classification.
    "osr_exit_map_missing",
    // Neither: the artifact records no deopt point at the bci the frame names.
    // **Expected to stay zero** — non-zero means a stash reached an artifact
    // that cannot describe it, and the transfer refuses.
    "osr_exit_bci_unrecorded",
    // The admission-time form of the lane's "what to refuse": a bci naming two
    // resume images whose `ResumeSemantics` disagree, so the resume bci itself
    // is arbitrary. Refused at ENTRY, where nothing has run — refusing at exit
    // would force the safe reject, which after a committed body re-runs every
    // iteration since entry. Expected to read zero.
    "osr_entry_refused_ambiguous_image",
    // The benign neighbour, counted rather than refused: images that agree on
    // `semantics` and differ on `reason`. The ordinary shape of a compiled
    // counted loop (the loop-boundary exit map and the speculative-BCE range
    // guard on one header bci), and NOT expected to be zero — on CratonBench it
    // is the majority of admitted OSR artifacts. It has a row because the
    // de-speculation lookup at the OSR-exit *reject* sink does pick one of the
    // two arbitrarily; admission makes that sink unreachable for an admitted
    // entry, and a tolerated shape should be visible rather than assumed.
    "osr_entry_reason_ambiguous_image",
    // A PUBLISHED OSR artifact exists but reports no enterable native offset
    // for this back edge's pc, so the loop keeps interpreting. Distinct from
    // `osr_refused_entry`, which is recorded inside `try_osr` — this arm returns
    // before reaching it. Without this row the lifecycle line reads
    // `osr_entered=0 osr_refused_entry=0` next to a non-zero compile count,
    // which looks like "OSR was never tried" and is not.
    "osr_published_but_unenterable",
    // The method is on the OSR-denied list, so no back edge in it will enter.
    "osr_method_denied",
    // An exception raised inside an OSR'd body was routed through that method's
    // OWN exception table and the live frame was parked at the handler — the
    // thing RBC.6b refused to allow at all until 2026-08-17.
    //
    // This is the engagement counter for the lift, and it is the row to read
    // when a `try`/`catch` loop is "still slow". `osr_entered` climbing with
    // this at zero means the loop's `catch` never fires (so the lift is not
    // what is costing you); this climbing means it fires, and each one is an
    // OSR exit plus a re-entry at the next back edge.
    //
    // It also names the shape that ate the lift once already: each of these
    // used to be charged to the per-pc rejection budget
    // (`Frame::record_osr_rejection`), which retires OSR after
    // `OSR_MAX_ATTEMPTS = 5`. A loop with netty's 7.7% throw rate therefore ran
    // compiled for about sixty-five of four billion iterations while passing
    // every correctness probe.
    "osr_exception_handler_entered",
];

/// One relaxed counter per [`OSR_EVENTS`] entry.
static OSR_COUNTERS: [AtomicU64; OSR_EVENTS.len()] = [
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
    AtomicU64::new(0),
];

/// Count one occurrence of `event`.
///
/// Infallible, non-blocking, and independent of [`enabled`] — like
/// [`record_scheduling_event`], and for a stronger reason: this is called from
/// the interpreter's OSR path on the hot back-edge, where a lock or a panic
/// would be far worse than a lost count. An unknown name is ignored.
pub fn record_osr_event(event: &str) {
    if let Some(idx) = OSR_EVENTS.iter().position(|e| *e == event) {
        OSR_COUNTERS[idx].fetch_add(1, Ordering::Relaxed);
    }
}

/// Bytecode loop-rewriter admission, counted per compile that reaches
/// `x64::loop_rewrite::plan_bytecode_loop_xform`.
///
/// ## Why the four conditions are counted INDEPENDENTLY
///
/// They were once four whole-compile REFUSALS evaluated in a fixed order,
/// returning on the first — so a "which refusal fired" tally answered a
/// question nobody asked: `deopt_real` is default-ON and process-wide, so it
/// would have accounted for **100%** of refusals and hidden the other three
/// permanently. Counting them independently is what retired three of them:
/// `DeoptimizationPoint::bci` is now published through the rewrite's own
/// provenance map (`Compiler::orig_bci`), which removed `deopt_real`, precise
/// exception frames and `invokedynamic` as refusals in one move. Only
/// `inline_sites` still refuses.
///
/// The four rows stay, still recorded on every compile that reaches the
/// planner whether or not something else refuses, because they now answer a
/// different question: how much of the compile population each construct
/// covers — i.e. how much the translation bought. They **overlap by
/// construction** — a method with an `invokedynamic` compiled under
/// `deopt_real` bumps both — and must not be summed. `loop_xform_eligible` is
/// the count of compiles no whole-compile refusal held for, which today is
/// exactly `loop_xform_compiles - loop_xform_inline_sites`.
///
/// ## They are properties of the METHOD, not of the rewriter
///
/// which is why they are meaningful on a default (unarmed) run: "how often does
/// an `invokedynamic` cost this transform a method" needs nothing armed. The
/// four loop-level rows below do need it — on an unarmed run
/// `loop_xform_not_armed` equals `loop_xform_compiles` and the rest are zero,
/// which is the honest answer rather than a gap.
///
/// See `feature-designs/c2/loop-02-planner-admission-gates.md`.
pub const LOOP_XFORM_EVENTS: [&str; 12] = [
    // Denominator: compiles that reached the planner at all.
    "loop_xform_compiles",
    // The four conditions, each counted on every compile it holds for.
    // Overlapping; do not sum. Only the last of them still REFUSES.
    "loop_xform_deopt_real",
    "loop_xform_precise_exception_frames",
    "loop_xform_invokedynamic",
    "loop_xform_inline_sites",
    // No whole-compile refusal held. `compiles - inline_sites` today.
    "loop_xform_eligible",
    // …and of those, the ones that got no further because nothing armed the
    // rewriter. On a default run this equals `loop_xform_compiles`.
    "loop_xform_not_armed",
    // Armed and eligible, but no loop passed the profitability band and the
    // structural admission test.
    "loop_xform_no_candidate_loop",
    // Armed and eligible, a loop was selected, and the rewriter refused it for
    // a structural reason (`LoopXformRefusal`).
    "loop_xform_planner_refused",
    // A transform was produced and the emitter compiled rewritten bytecode.
    "loop_xform_applied",
    // …and was then DISCARDED, because its recorded deopt points could not be
    // published as interpreter resume points. Fail-closed: the method stays
    // interpreted. Any non-zero value here is a defect in the coordinate
    // change, not a tuning signal — see
    // `x64::loop_rewrite::rewritten_deopt_points_are_publishable`.
    "loop_xform_deopt_bci_unpublishable",
    // Two images of one bytecode published deopt points whose frame SHAPES
    // differ (a slot's kind, the operand-stack depth, a monitor). Reported, not
    // refused: the only bci-keyed reader of those fields is the OSR entry
    // contract, which re-verifies every one of them against the live
    // interpreter frame. Non-zero is normal — see `PointDifference`.
    "loop_xform_deopt_frames_diverge",
];

/// One relaxed counter per [`LOOP_XFORM_EVENTS`] entry.
static LOOP_XFORM_COUNTERS: [AtomicU64; LOOP_XFORM_EVENTS.len()] = [
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

/// Count one occurrence of `event`.
///
/// Infallible, non-blocking and independent of [`enabled`], like
/// [`record_osr_event`]: the point of this tally is to be readable from a
/// default run, and a counter that only works when the metrics ring is on
/// would answer for a configuration nobody runs. An unknown name is ignored.
pub fn record_loop_xform_event(event: &str) {
    if let Some(idx) = LOOP_XFORM_EVENTS.iter().position(|e| *e == event) {
        LOOP_XFORM_COUNTERS[idx].fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        LoopXformCapture::note(idx, 1);
    }
}

/// Define a per-thread capture of one of this module's process-global event
/// tables, for tests that assert exact counts.
///
/// **Why these tables need one.** Every counter here is process-wide and
/// monotone, which is right for a production run and useless for a test
/// assertion: Rust runs a crate's tests on a thread pool, so a test asserting
/// `count == 1` is asserting against every other test's work as well. Two ways
/// of papering over that have already been tried in this crate and neither
/// holds:
///
///  * **zero the table first.** A sibling can bump it between the reset and the
///    read, and does.
///  * **hold a lock while asserting.** That only serialises the tests that
///    ASSERT. The tests that PRODUCE are elsewhere — `x64::tests` compiles
///    thousands of methods, `tiered::tests` drops compilation requests — and
///    none of them takes the lock. This is what made
///    `drops_reach_the_process_wide_scheduling_counters` fail about 1 run in 20
///    at `--test-threads=32`, reporting `Some(5)` for a count of 1.
///
/// Both failure modes are rare, which is worse than frequent: the symptom is
/// one unrelated number off by a few, on a test that looks unconnected to
/// whatever change is being reviewed.
///
/// Every producer this matters for runs on its caller's thread, so counting
/// there is exact and needs no lock at all. The generated struct is a guard:
/// `start()` installs a fresh per-thread vector, `Drop` removes it, and the
/// global counters are untouched throughout — a sink reading the real table
/// still sees every event.
///
/// Nothing it generates is compiled outside `#[cfg(test)]` — the macro itself
/// is unconditional so the invocations below need no `cfg` of their own. The
/// recorder pays one thread-local check per event in test builds and nothing
/// in release.
macro_rules! per_thread_event_capture {
    (
        $(#[$meta:meta])*
        struct $name:ident, events $events:ident, tls $tls:ident
    ) => {
        #[cfg(test)]
        thread_local! {
            /// Installed by the capture guard; `None` on every thread that has
            /// not asked to count.
            static $tls: std::cell::RefCell<Option<Vec<u64>>> =
                const { std::cell::RefCell::new(None) };
        }

        $(#[$meta])*
        #[cfg(test)]
        pub(crate) struct $name(());

        #[cfg(test)]
        impl $name {
            /// Start counting THIS thread's events from zero. Dropping the
            /// guard stops counting.
            pub(crate) fn start() -> Self {
                $tls.with(|c| *c.borrow_mut() = Some(vec![0; $events.len()]));
                $name(())
            }

            /// This thread's count for `event`. Panics on an unknown name
            /// rather than answering zero, which is how a renamed row would
            /// otherwise pass.
            pub(crate) fn count(&self, event: &str) -> u64 {
                let idx = $events
                    .iter()
                    .position(|e| *e == event)
                    .unwrap_or_else(|| panic!("no such counter: {event}"));
                $tls.with(|c| c.borrow().as_ref().map_or(0, |v| v[idx]))
            }

            /// Forget everything counted so far and keep counting.
            #[allow(dead_code)]
            pub(crate) fn reset(&self) {
                $tls.with(|c| {
                    if let Some(v) = c.borrow_mut().as_mut() {
                        v.iter_mut().for_each(|x| *x = 0);
                    }
                });
            }

            /// Add `n` to this thread's capture of row `idx`, if capturing.
            /// Called from the recorder, which already resolved the index.
            fn note(idx: usize, n: u64) {
                $tls.with(|c| {
                    if let Some(v) = c.borrow_mut().as_mut() {
                        v[idx] += n;
                    }
                });
            }
        }

        #[cfg(test)]
        impl Drop for $name {
            fn drop(&mut self) {
                $tls.with(|c| *c.borrow_mut() = None);
            }
        }
    };
}

per_thread_event_capture! {
    /// A per-thread view of [`record_loop_xform_event`]. See
    /// [`per_thread_event_capture`] for why the global table cannot be
    /// asserted on directly; the planner runs on its caller's thread, so this
    /// is exact.
    struct LoopXformCapture, events LOOP_XFORM_EVENTS, tls LOOP_XFORM_CAPTURE
}

/// Read every loop-rewriter event's count, including zero-valued ones, in
/// [`LOOP_XFORM_EVENTS`] order. Relaxed loads: a sample, not an atomic
/// snapshot.
pub fn loop_xform_counts() -> Vec<(&'static str, u64)> {
    LOOP_XFORM_EVENTS
        .iter()
        .enumerate()
        .map(|(i, name)| (*name, LOOP_XFORM_COUNTERS[i].load(Ordering::Relaxed)))
        .collect()
}

// There is deliberately no `reset_loop_xform_counts_for_test`. Zeroing a
// process-wide counter that every concurrent test is incrementing does not make
// a count assertable — see [`LoopXformCapture`], which is what to use instead.

/// Read every OSR event's count, including zero-valued ones, in
/// [`OSR_EVENTS`] order. Relaxed loads: a sample, not an atomic snapshot.
pub fn osr_counts() -> Vec<(&'static str, u64)> {
    OSR_EVENTS
        .iter()
        .zip(OSR_COUNTERS.iter())
        .map(|(name, c)| (*name, c.load(Ordering::Relaxed)))
        .collect()
}

/// Test support: zero the OSR counters.
#[cfg(test)]
pub fn reset_osr_counts() {
    for c in OSR_COUNTERS.iter() {
        c.store(0, Ordering::Relaxed);
    }
}

/// Every scheduling event that discards a compilation request, in the fixed
/// order [`scheduling_counts`] reports.
pub const SCHEDULING_EVENTS: [&str; 4] = [
    // A queued request was discarded at dispatch because the process-wide JIT
    // install epoch (`crate::jit_install_epoch`) moved after it was queued —
    // a JVMTI redefinition or a code-cache flush replaced the world the
    // request was formed against. The method's in-flight slot is released, so
    // the next invocation re-admits it against the bytecode that is loaded
    // now. Non-zero is expected under an instrumenting agent and is not by
    // itself a fault.
    "queue_dropped_stale_install_epoch",
    // A queued request was discarded because its class was invalidated —
    // `TieredCompilationManager::invalidate_class`, which the VM's class-unload
    // path calls. Unlike a stale-epoch drop this one is final: the class is
    // gone, so there is no next invocation to re-admit the method. Counted
    // separately for exactly that reason.
    "queue_dropped_class_invalidated",
    // The install epoch moved *while* a compile was running. The artifact is
    // not lost here: `JitCache::put`/`put_osr` compare it against the owning
    // cache's flush barrier and refuse it there (counted separately by
    // `crate::stale_install_epoch_refusals`). This counts the wasted-work
    // window that the dispatch-time drop cannot close, because the epoch had
    // not moved yet when the request was dispatched.
    "inflight_epoch_moved",
    // Requests still queued when the background compiler was shut down. Only
    // ever non-zero at VM teardown, where abandoning them is correct — but a
    // non-zero value in the middle of a run means the worker was stopped with
    // work outstanding, which is not.
    "queue_shutdown_abandoned",
];

/// One relaxed counter per [`SCHEDULING_EVENTS`] entry. Fixed array, same
/// reasoning as [`crate::bailout`]'s: the event set is closed, so this needs
/// no allocation, no lock and no initialization order.
static SCHEDULING_COUNTERS: [AtomicU64; SCHEDULING_EVENTS.len()] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Index of `event` in [`SCHEDULING_EVENTS`], compared by content so a
/// hand-built name also resolves.
fn scheduling_index(event: &str) -> Option<usize> {
    SCHEDULING_EVENTS.iter().position(|e| *e == event)
}

/// Count `n` occurrences of `event`.
///
/// Infallible, non-blocking, and independent of [`enabled`]. An unknown event
/// name is ignored rather than panicking: this is called from the compile
/// worker's drain loop, where a panic would take the only compiler thread in
/// the process down.
///
/// Callers should prefer the `SCHEDULING_EVENTS` constants over string
/// literals at the call site; see `crate::tiered`, which does.
pub fn record_scheduling_events(event: &str, n: u64) {
    if n == 0 {
        return;
    }
    if let Some(idx) = scheduling_index(event) {
        SCHEDULING_COUNTERS[idx].fetch_add(n, Ordering::Relaxed);
        #[cfg(test)]
        SchedulingCapture::note(idx, n);
    }
}

per_thread_event_capture! {
    /// A per-thread view of [`record_scheduling_event`]. See
    /// [`per_thread_event_capture`].
    ///
    /// The producer that matters here is `TieredCompilationManager`'s own
    /// drain, which runs on the thread that called `next_fresh_task` — so a
    /// test driving a manager it constructed counts exactly its own drops.
    /// The compile WORKER thread also records, and a test that means to
    /// observe a worker's drops must therefore still read the global table.
    struct SchedulingCapture, events SCHEDULING_EVENTS, tls SCHEDULING_CAPTURE
}

/// Count one occurrence of `event`.
pub fn record_scheduling_event(event: &str) {
    record_scheduling_events(event, 1);
}

/// Read every scheduling event's count.
///
/// Returns **all** events, including zero-valued ones, in the fixed
/// [`SCHEDULING_EVENTS`] order — for the same reason
/// [`crate::bailout::bailout_counts`] does: a sink wants a stable row set, and
/// "this drop never happened" is itself information. Counts are relaxed loads
/// and are therefore a sample, not an atomic snapshot.
pub fn scheduling_counts() -> Vec<(&'static str, u64)> {
    SCHEDULING_EVENTS
        .iter()
        .zip(SCHEDULING_COUNTERS.iter())
        .map(|(name, counter)| (*name, counter.load(Ordering::Relaxed)))
        .collect()
}

/// Read one event's count, or `None` if the name is not a known event.
pub fn scheduling_count(event: &str) -> Option<u64> {
    scheduling_index(event).map(|idx| SCHEDULING_COUNTERS[idx].load(Ordering::Relaxed))
}

/// Total requests discarded by the scheduler without a compile ever running.
///
/// The in-flight-epoch counter is deliberately excluded: that compile *ran*,
/// and whether its artifact survived is the code cache's question, not the
/// scheduler's.
pub fn scheduling_dropped_total() -> u64 {
    scheduling_count(SCHEDULING_EVENTS[0]).unwrap_or(0)
        + scheduling_count(SCHEDULING_EVENTS[1]).unwrap_or(0)
        + scheduling_count(SCHEDULING_EVENTS[3]).unwrap_or(0)
}

/// Zero every scheduling counter.
///
/// Only for a test that must read the GLOBAL table — `summary()` carries that
/// table, so the summary-plumbing test cannot use [`SchedulingCapture`]. For
/// "did my own thread's event get counted", use the capture: resetting a
/// counter every other test is incrementing does not make it assertable, which
/// is the whole argument in [`per_thread_event_capture`].
///
/// `pub(crate)` and test-only: these counters are monotone by contract in a
/// real run, and a production reset would make "how many requests has this
/// process thrown away" unanswerable.
#[cfg(test)]
pub(crate) fn reset_scheduling_counts_for_test() {
    for counter in SCHEDULING_COUNTERS.iter() {
        counter.store(0, Ordering::Relaxed);
    }
}

// ── Summary ──────────────────────────────────────────────────────────

/// Aggregate view over the retained reports plus the process-wide bailout
/// table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSummary {
    /// Whether collection is on. `false` with a non-empty ring means the
    /// reports predate a flag change, or were seeded by a test.
    pub enabled: bool,
    /// Reports published since process start, including evicted ones.
    pub total_recorded: u64,
    /// Reports currently in the ring.
    pub retained: usize,
    /// Ring size.
    pub ring_capacity: usize,
    /// Retained-report counts by [`Outcome`], in [`Outcome::ALL`] order.
    pub by_outcome: Vec<(&'static str, u64)>,
    /// Retained-report counts by [`CompilerPath`], in [`CompilerPath::ALL`] order.
    pub by_path: Vec<(&'static str, u64)>,
    /// Retained-report count that fell out of the optimizing pipeline into the
    /// single-pass backend.
    pub fell_through_to_single_pass: u64,
    /// Total nanoseconds per phase across retained reports, in [`Phase::ALL`]
    /// order. A phase no report measured contributes `0` runs — check
    /// `phase_runs` before reading a zero as "instant".
    pub phase_totals_ns: Vec<(&'static str, u64)>,
    /// Run count per phase across retained reports.
    pub phase_runs: Vec<(&'static str, u64)>,
    /// [`crate::bailout::bailout_counts`] verbatim: **process-wide**, not
    /// restricted to the retained reports, and fed by every `record_bailout`
    /// call site whether or not metrics are enabled.
    pub bailout_categories: Vec<(&'static str, u64)>,
    /// [`scheduling_counts`] verbatim: compilation requests the *scheduler*
    /// discarded, so they never became a report at all.
    ///
    /// Read this before concluding from `by_outcome` that a method was never
    /// hot: a request dropped at dispatch produces no row anywhere else in
    /// this summary. Same process-wide, metrics-flag-independent semantics as
    /// `bailout_categories`.
    pub scheduling: Vec<(&'static str, u64)>,
    /// [`osr_counts`] verbatim: the OSR lifecycle, process-wide and
    /// metrics-flag-independent like the two above.
    ///
    /// Read `osr_exited` against `osr_entered`, not on its own. The two being
    /// close together is the livelock: every entry paying for a trampoline and
    /// a local seed, then leaving immediately. `osr_entered` at zero with a
    /// hot loop means the requests are being refused or declined, and the
    /// other two rows say which.
    pub osr: Vec<(&'static str, u64)>,
    /// [`loop_xform_counts`] verbatim: bytecode loop-rewriter admission,
    /// process-wide and metrics-flag-independent like the three above.
    ///
    /// The four refusal-condition rows **overlap** — read each against
    /// `loop_xform_compiles`, never as a partition, and never by summing them.
    /// See [`LOOP_XFORM_EVENTS`].
    pub loop_xform: Vec<(&'static str, u64)>,
}

/// Aggregate the retained reports.
///
/// Derived from the ring rather than from independent counters, so
/// `summary().by_outcome` and `compilation_reports()` can never disagree. The
/// cost is that eviction is lossy: compare `total_recorded` against `retained`
/// to see whether the ring wrapped.
pub fn summary() -> MetricsSummary {
    let reports = compilation_reports();
    let mut by_outcome: Vec<(&'static str, u64)> =
        Outcome::ALL.iter().map(|o| (o.name(), 0u64)).collect();
    let mut by_path: Vec<(&'static str, u64)> =
        CompilerPath::ALL.iter().map(|p| (p.name(), 0u64)).collect();
    let mut phase_totals_ns: Vec<(&'static str, u64)> =
        Phase::ALL.iter().map(|p| (p.name(), 0u64)).collect();
    let mut phase_runs: Vec<(&'static str, u64)> =
        Phase::ALL.iter().map(|p| (p.name(), 0u64)).collect();
    let mut fell_through = 0u64;

    for report in &reports {
        if let Some(slot) = by_outcome
            .iter_mut()
            .find(|(name, _)| *name == report.outcome.name())
        {
            slot.1 += 1;
        }
        if let Some(slot) = by_path
            .iter_mut()
            .find(|(name, _)| *name == report.path.name())
        {
            slot.1 += 1;
        }
        if report.fell_through_to_single_pass {
            fell_through += 1;
        }
        for rec in &report.phases {
            let idx = rec.phase.index();
            if let Some(slot) = phase_totals_ns.get_mut(idx) {
                slot.1 = slot.1.saturating_add(rec.wall_ns.or(0));
            }
            if let Some(slot) = phase_runs.get_mut(idx) {
                slot.1 = slot.1.saturating_add(rec.runs as u64);
            }
        }
    }

    MetricsSummary {
        enabled: enabled(),
        total_recorded: TOTAL_RECORDED.load(Ordering::Relaxed),
        retained: reports.len(),
        ring_capacity: ring_capacity(),
        by_outcome,
        by_path,
        fell_through_to_single_pass: fell_through,
        phase_totals_ns,
        phase_runs,
        bailout_categories: crate::bailout::bailout_counts(),
        scheduling: scheduling_counts(),
        osr: osr_counts(),
        loop_xform: loop_xform_counts(),
    }
}

impl MetricsSummary {
    /// One JSON object. Same hand-rolled encoder as
    /// [`CompilationReport::to_json`].
    pub fn to_json(&self) -> String {
        fn pairs(list: &[(&'static str, u64)]) -> String {
            let mut s = String::from("{");
            for (i, (name, count)) in list.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let _ = write!(s, "\"{}\":{}", json_escape(name), count);
            }
            s.push('}');
            s
        }
        let mut s = String::with_capacity(512);
        s.push('{');
        let _ = write!(
            s,
            "\"enabled\":{},\"total_recorded\":{},\"retained\":{},\"ring_capacity\":{}",
            self.enabled, self.total_recorded, self.retained, self.ring_capacity,
        );
        let _ = write!(
            s,
            ",\"fell_through_to_single_pass\":{}",
            self.fell_through_to_single_pass
        );
        let _ = write!(s, ",\"by_outcome\":{}", pairs(&self.by_outcome));
        let _ = write!(s, ",\"by_path\":{}", pairs(&self.by_path));
        let _ = write!(s, ",\"phase_totals_ns\":{}", pairs(&self.phase_totals_ns));
        let _ = write!(s, ",\"phase_runs\":{}", pairs(&self.phase_runs));
        let _ = write!(
            s,
            ",\"bailout_categories\":{}",
            pairs(&self.bailout_categories)
        );
        let _ = write!(s, ",\"scheduling\":{}", pairs(&self.scheduling));
        let _ = write!(s, ",\"osr\":{}", pairs(&self.osr));
        let _ = write!(s, ",\"loop_xform\":{}", pairs(&self.loop_xform));
        s.push('}');
        s
    }
}

// ── Test helpers ─────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) fn set_enabled_for_test(on: bool) {
    ENABLED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
}

/// Serializes every test that touches the process-wide enable flag, the
/// report ring, or the [`SCHEDULING_COUNTERS`] table.
///
/// Module-level and `pub(crate)` rather than private to `mod tests`, because the
/// enable flag is one process-wide `AtomicU8`: `ir_lower`'s end-to-end wiring
/// test (`peak_live_values_reaches_the_compilation_report`) flips the same flag
/// from another module, and a lock the tests here cannot share with it would
/// serialize nothing — the `set_enabled_for_test(false)` that ends each test
/// below would disable collection in the middle of that one.
#[cfg(test)]
pub(crate) static METRICS_TEST_LOCK: Mutex<()> = Mutex::new(());

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // The enable flag and the ring are process-wide, and the cargo harness
    // runs `#[test]`s in parallel threads of one process. Every test that
    // touches either takes this first, so they serialize against each other
    // (they cannot serialize against an unrelated `lib.rs` compile test, which
    // is why the assertions below are written to tolerate foreign reports
    // wherever they cannot exclude them). Defined one level up as
    // `METRICS_TEST_LOCK` so `ir_lower`'s wiring test shares the same lock —
    // it flips the same process-wide enable flag from another module.
    use super::METRICS_TEST_LOCK as TEST_LOCK;

    fn sample(class: &str, outcome: Outcome, path: CompilerPath) -> CompilationReport {
        let mut r = CompilationReport::new(class, "m", "()I", true);
        r.outcome = outcome;
        r.path = path;
        r
    }

    #[test]
    fn report_round_trips_through_to_json() {
        let mut r = CompilationReport::new("java/lang/String", "hashCode", "()I", true);
        r.seq = 7;
        r.path = CompilerPath::Optimizing;
        r.outcome = Outcome::Installed;
        r.admission = Some("admitted to the optimizing pipeline".to_string());
        r.nodes_built = Measured::Value(41);
        r.nodes_at_lower = Measured::Value(33);
        r.live_nodes_at_lower = Measured::Value(29);
        r.frame_bytes = Measured::Value(392);
        r.code_bytes = Measured::Value(188);
        r.ir_safepoints = Measured::Value(3);
        r.oop_maps = Measured::Value(0);
        r.deopt_points = Measured::Value(2);
        r.deopt_metadata_bytes = Measured::Value(768);
        r.code_cache_bytes_at_install = Measured::Value(65_536);
        r.total_wall_ns = Measured::Value(123_456);
        r.phases[Phase::Build.index()].add_run(1_000);
        r.phases[Phase::Build.index()].nodes_before = Measured::Value(0);
        r.phases[Phase::Build.index()].nodes_after = Measured::Value(41);
        r.bailouts.push(BailoutRecord {
            category: "ir_verification",
            phase: "pre-lower".to_string(),
            detail: "n3 \"input\"\tout of range\n".to_string(),
        });

        let json = r.to_json();
        // Structure: one line, balanced, and every field the consumer keys on.
        assert!(!json.contains('\n'), "JSON lines must be one line: {json}");
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"seq\":7"));
        assert!(json.contains("\"class\":\"java/lang/String\""));
        assert!(json.contains("\"method\":\"hashCode\""));
        assert!(json.contains("\"descriptor\":\"()I\""));
        assert!(json.contains("\"tier_requested\":\"c2\""));
        assert!(json.contains("\"path\":\"optimizing\""));
        assert!(json.contains("\"outcome\":\"installed\""));
        assert!(json.contains("\"nodes_built\":41"));
        assert!(json.contains("\"frame_bytes\":392"));
        assert!(json.contains("\"code_bytes\":188"));
        assert!(json.contains("\"deopt_metadata_bytes\":768"));
        assert!(json.contains("\"total_wall_ns\":123456"));
        assert!(json.contains("\"category\":\"ir_verification\""));
        // Escaping: the raw quote/tab/newline must not appear unescaped.
        assert!(json.contains("\\\"input\\\""), "{json}");
        assert!(json.contains("\\t"), "{json}");
        assert!(json.contains("\\n"), "{json}");
        // Every phase is present by name, so a consumer never has to guess
        // whether a missing key means zero.
        for phase in Phase::ALL {
            assert!(
                json.contains(&format!("\"phase\":\"{}\"", phase.name())),
                "missing {} in {json}",
                phase.name()
            );
        }
        assert_eq!(
            json.matches("{\"phase\":").count(),
            Phase::ALL.len(),
            "one row per phase"
        );
    }

    #[test]
    fn unmeasured_is_distinguishable_from_zero() {
        let r = CompilationReport::new("C", "m", "()V", false);
        // Nothing supplied yet.
        assert_eq!(r.spills, Measured::NotMeasured);
        assert!(!r.spills.is_measured());
        assert_eq!(r.spills.get(), None);
        assert!(r.to_json().contains("\"spills\":null"));

        let mut measured = r.clone();
        measured.spills = Measured::Value(0);
        assert!(measured.spills.is_measured());
        assert_eq!(measured.spills.get(), Some(0));
        assert!(measured.to_json().contains("\"spills\":0"));

        // The two JSON encodings differ — which is the entire point.
        assert_ne!(r.to_json(), measured.to_json());

        // Same distinction for a phase that never ran vs one that ran fast.
        let never = &r.phases[Phase::Encode.index()];
        assert_eq!(never.runs, 0);
        assert_eq!(never.wall_ns, Measured::NotMeasured);
        assert!(never.json().contains("\"wall_ns\":null"));

        let mut instant = PhaseRecord::new(Phase::Encode);
        instant.add_run(0);
        assert_eq!(instant.runs, 1);
        assert_eq!(instant.wall_ns, Measured::Value(0));
        assert!(instant.json().contains("\"wall_ns\":0"));
    }

    #[test]
    fn phase_names_and_indices_agree() {
        for (i, phase) in Phase::ALL.iter().enumerate() {
            assert_eq!(phase.index(), i, "{} index", phase.name());
            assert_eq!(Phase::from_name(phase.name()), Some(*phase));
        }
        assert_eq!(Phase::from_name("no_such_phase"), None);
        let r = CompilationReport::new("C", "m", "()V", false);
        assert_eq!(r.phases.len(), Phase::ALL.len());
        for phase in Phase::ALL {
            assert_eq!(r.phase(phase).map(|p| p.phase), Some(phase));
        }
    }

    #[test]
    fn ring_buffer_is_bounded_and_evicts_oldest() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(false); // no concurrent compile can publish
        clear_reports();
        let cap = ring_capacity();
        let overshoot = cap + 5;
        for i in 0..overshoot {
            record_report(sample(
                &format!("ring/Probe{i}"),
                Outcome::Installed,
                CompilerPath::Optimizing,
            ));
        }
        let retained = compilation_reports();
        assert_eq!(retained.len(), cap, "ring must be bounded by its capacity");
        // Oldest evicted, newest kept.
        assert_eq!(retained.first().unwrap().class_name, "ring/Probe5");
        assert_eq!(
            retained.last().unwrap().class_name,
            format!("ring/Probe{}", overshoot - 1)
        );
        assert!(last_compilation_report(None).is_some());
        // The trailing `.` anchors the filter to the method-key separator, so
        // `Probe7` cannot be satisfied by `Probe70`. Guarded on the capacity
        // because `CRATONVM_JIT_METRICS_RING` can shrink the retained window.
        if cap >= 8 {
            assert_eq!(
                last_compilation_report(Some("ring/Probe7."))
                    .map(|r| r.class_name)
                    .as_deref(),
                Some("ring/Probe7")
            );
        }
        assert!(last_compilation_report(Some("ring/Probe0.")).is_none());
        // Sequence numbers are monotonic and gap-free within a retained window.
        for pair in retained.windows(2) {
            assert_eq!(pair[1].seq, pair[0].seq + 1);
        }
        clear_reports();
        set_enabled_for_test(false);
    }

    #[test]
    fn summary_counts_match_recorded_reports() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(false);
        clear_reports();
        record_report(sample("s/A", Outcome::Installed, CompilerPath::Optimizing));
        record_report(sample("s/B", Outcome::Installed, CompilerPath::SinglePass));
        record_report(sample("s/C", Outcome::BailedOut, CompilerPath::Optimizing));
        record_report(sample("s/D", Outcome::Abandoned, CompilerPath::NotEntered));
        let mut fell = sample("s/E", Outcome::Installed, CompilerPath::SinglePass);
        fell.fell_through_to_single_pass = true;
        fell.phases[Phase::Optimize.index()].add_run(500);
        fell.phases[Phase::SinglePass.index()].add_run(1_500);
        record_report(fell);

        let s = summary();
        assert_eq!(s.retained, 5);
        assert_eq!(s.retained, compilation_reports().len());
        fn count(list: &[(&'static str, u64)], key: &str) -> Option<u64> {
            list.iter().find(|(n, _)| *n == key).map(|(_, c)| *c)
        }
        assert_eq!(count(&s.by_outcome, "installed"), Some(3));
        assert_eq!(count(&s.by_outcome, "bailed_out"), Some(1));
        assert_eq!(count(&s.by_outcome, "abandoned"), Some(1));
        assert_eq!(count(&s.by_outcome, "in_progress"), Some(0));
        assert_eq!(count(&s.by_path, "optimizing"), Some(2));
        assert_eq!(count(&s.by_path, "single_pass"), Some(2));
        assert_eq!(count(&s.by_path, "not_entered"), Some(1));
        assert_eq!(s.fell_through_to_single_pass, 1);
        // Every outcome/path bucket is present even at zero, and the buckets
        // sum to the retained count.
        assert_eq!(s.by_outcome.len(), Outcome::ALL.len());
        assert_eq!(
            s.by_outcome.iter().map(|(_, c)| *c).sum::<u64>(),
            s.retained as u64
        );
        assert_eq!(
            s.by_path.iter().map(|(_, c)| *c).sum::<u64>(),
            s.retained as u64
        );
        // Phase totals come from the reports, and a phase nothing ran reports
        // zero runs (not a bogus time).
        assert_eq!(count(&s.phase_totals_ns, "optimize"), Some(500));
        assert_eq!(count(&s.phase_totals_ns, "single_pass"), Some(1_500));
        assert_eq!(count(&s.phase_runs, "optimize"), Some(1));
        assert_eq!(count(&s.phase_runs, "encode"), Some(0));
        assert_eq!(count(&s.phase_totals_ns, "encode"), Some(0));
        // Bailout categories are the process-wide table, verbatim. Compared by
        // NAME rather than by value: the counters are shared with every other
        // test in this process, so a count can advance between the two reads —
        // which is exactly the "sample, not snapshot" contract `bailout.rs`
        // documents.
        let summary_names: Vec<&str> = s.bailout_categories.iter().map(|(n, _)| *n).collect();
        let table_names: Vec<&str> = crate::bailout::bailout_counts()
            .iter()
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(summary_names, table_names);
        assert!(!summary_names.is_empty());
        assert!(s.total_recorded >= 5);
        // JSON is well-formed enough for a consumer to key on.
        let json = s.to_json();
        assert!(json.contains("\"by_outcome\":{\"installed\":3"));
        assert!(json.contains("\"retained\":5"));
        assert!(!json.contains('\n'));

        clear_reports();
        set_enabled_for_test(false);
    }

    #[test]
    fn disabled_recorder_records_and_publishes_nothing() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(false);
        clear_reports();
        let before_total = TOTAL_RECORDED.load(Ordering::Relaxed);

        // Both the explicit opt-out and the flag-driven constructor.
        for rec in [
            CompileRecorder::disabled(),
            CompileRecorder::begin("C", "m", "()V", true),
        ] {
            assert!(!rec.is_enabled());
            assert!(rec.snapshot().is_none());
            // Every hook is reachable and does nothing observable.
            let timer = rec.phase(Phase::Build);
            assert!(!timer.is_measuring(), "no clock is read when disabled");
            drop(timer);
            rec.phase_nodes(Phase::Optimize, 10, 8);
            rec.set_admission("ignored");
            rec.enter_optimizing_pipeline();
            rec.enter_single_pass();
            rec.set_nodes_built(10);
            rec.set_graph_at_lower(10, 9, 2);
            rec.set_peak_live_values(4);
            rec.set_spills(1);
            rec.set_reloads(1);
            rec.note_bailout_reason(BailoutReason::RegisterPressure, "lower");
            // The TLS hooks find nothing to record against.
            let t = current_phase(Phase::Verify);
            assert!(!t.is_measuring());
            drop(t);
            note_current_bailout(&Bailout::new(BailoutReason::RegisterPressure), "verify");
            note_current_peak_live_values(4);
            note_current_spills(3);
            note_current_reloads(2);
            drop(rec); // Drop must not publish.
        }

        assert!(
            compilation_reports().is_empty(),
            "a disabled recorder must publish nothing"
        );
        assert_eq!(
            TOTAL_RECORDED.load(Ordering::Relaxed),
            before_total,
            "a disabled recorder must not advance the publication counter"
        );
        set_enabled_for_test(false);
    }

    #[test]
    fn enabled_recorder_publishes_on_drop_with_phase_and_bailout() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(true);
        clear_reports();
        {
            let rec = CompileRecorder::begin("metrics/Probe", "run", "(I)I", true);
            assert!(rec.is_enabled());
            rec.set_admission("admitted to the optimizing pipeline");
            rec.enter_optimizing_pipeline();
            let t = rec.phase(Phase::Build);
            assert!(t.is_measuring());
            drop(t);
            rec.set_nodes_built(12);
            rec.phase_nodes(Phase::Optimize, 12, 9);
            // The TLS hook reaches the innermost in-flight compilation, which
            // is what lets `ir_verify_reject` record without a signature change.
            {
                let t = current_phase(Phase::Verify);
                assert!(t.is_measuring(), "the TLS hook must find this recorder");
            }
            note_current_bailout(
                &Bailout::with_context(BailoutReason::RegisterPressure, "n7"),
                "pre-lower",
            );
            rec.enter_single_pass();
            let snap = rec.snapshot().expect("in-flight snapshot");
            assert_eq!(snap.outcome, Outcome::InProgress);
            assert!(snap.fell_through_to_single_pass);
        }
        let published =
            last_compilation_report(Some("metrics/Probe.run")).expect("recorder must publish");
        assert_eq!(published.outcome, Outcome::BailedOut, "bailout was recorded");
        assert_eq!(published.path, CompilerPath::SinglePass);
        assert!(published.fell_through_to_single_pass);
        assert_eq!(published.nodes_built, Measured::Value(12));
        assert_eq!(
            published.phases[Phase::Optimize.index()].nodes_after,
            Measured::Value(9)
        );
        assert_eq!(published.phases[Phase::Build.index()].runs, 1);
        assert!(published.phases[Phase::Build.index()].wall_ns.is_measured());
        assert_eq!(published.phases[Phase::Verify.index()].runs, 1);
        assert!(published.total_wall_ns.is_measured());
        assert_eq!(published.bailouts.len(), 1);
        assert_eq!(published.bailouts[0].category, "register_pressure");
        assert_eq!(published.bailouts[0].phase, "pre-lower");
        assert!(published.bailouts[0].detail.contains("n7"));
        // Uninstrumented quantities stay explicitly unmeasured on a real record.
        assert_eq!(published.peak_live_values, Measured::NotMeasured);
        assert_eq!(published.spills, Measured::NotMeasured);
        assert_eq!(published.reloads, Measured::NotMeasured);
        assert!(published.to_json().contains("\"peak_live_values\":null"));

        clear_reports();
        set_enabled_for_test(false);
    }

    #[test]
    fn abandoned_is_the_outcome_when_no_reason_was_recorded() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(true);
        clear_reports();
        drop(CompileRecorder::begin("metrics/Silent", "m", "()V", false));
        let r = last_compilation_report(Some("metrics/Silent")).expect("published");
        assert_eq!(r.outcome, Outcome::Abandoned);
        assert_eq!(r.path, CompilerPath::NotEntered);
        assert!(!r.optimizing_requested);
        assert!(r.to_json().contains("\"tier_requested\":\"c1\""));
        clear_reports();
        set_enabled_for_test(false);
    }

    #[test]
    fn nested_compilations_record_against_the_innermost() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(true);
        clear_reports();
        {
            let outer = CompileRecorder::begin("metrics/Outer", "m", "()V", true);
            {
                let _inner = CompileRecorder::begin("metrics/Inner", "m", "()V", true);
                // `callee_compiler` re-enters the compiler; the TLS hook must
                // charge the callee, not the caller.
                note_current_bailout(&Bailout::new(BailoutReason::RegisterPressure), "build");
            }
            // The inner recorder has popped, so the hook now finds the outer.
            note_current_bailout(
                &Bailout::new(BailoutReason::UnsupportedShape("multianewarray")),
                "build",
            );
            drop(outer);
        }
        let inner = last_compilation_report(Some("metrics/Inner")).expect("inner published");
        let outer = last_compilation_report(Some("metrics/Outer")).expect("outer published");
        assert_eq!(inner.bailouts.len(), 1);
        assert_eq!(inner.bailouts[0].category, "register_pressure");
        assert_eq!(outer.bailouts.len(), 1);
        assert_eq!(outer.bailouts[0].category, "unsupported_shape");
        assert!(inner.seq < outer.seq, "inner publishes first");
        clear_reports();
        set_enabled_for_test(false);
    }

    #[test]
    fn bailouts_per_report_are_bounded() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(true);
        clear_reports();
        {
            let rec = CompileRecorder::begin("metrics/Flood", "m", "()V", true);
            for _ in 0..100 {
                rec.note_bailout_reason(BailoutReason::RegisterPressure, "build");
            }
        }
        let r = last_compilation_report(Some("metrics/Flood")).expect("published");
        assert!(r.bailouts.len() <= 16, "got {}", r.bailouts.len());
        assert!(!r.bailouts.is_empty());
        clear_reports();
        set_enabled_for_test(false);
    }

    #[test]
    fn spill_reload_and_inline_fields_round_trip_through_to_json() {
        let mut r = CompilationReport::new("inl/Probe", "hot", "(I)I", true);
        r.spills = Measured::Value(7);
        r.reloads = Measured::Value(4);
        r.peak_live_values = Measured::Value(11);
        r.inline_candidates = Measured::Value(9);
        r.inlined_sites = Measured::Value(3);
        r.speculative_inlined_sites = Measured::Value(1);
        r.inlined_bytecodes = Measured::Value(214);
        r.inlined_call_count = Measured::Value(4_000_000_000);
        r.inline_refusals = vec![
            ("guard-not-emittable".to_string(), 5),
            ("budget-exhausted".to_string(), 1),
        ];

        let json = r.to_json();
        assert!(!json.contains('\n'), "JSON lines must be one line: {json}");
        assert!(json.contains("\"spills\":7"), "{json}");
        assert!(json.contains("\"reloads\":4"), "{json}");
        assert!(json.contains("\"peak_live_values\":11"), "{json}");
        assert!(json.contains("\"inline_candidates\":9"), "{json}");
        assert!(json.contains("\"inlined_sites\":3"), "{json}");
        assert!(json.contains("\"speculative_inlined_sites\":1"), "{json}");
        assert!(json.contains("\"inlined_bytecodes\":214"), "{json}");
        // A u64 that does not fit an i32/u32, so a narrowing regression shows.
        assert!(json.contains("\"inlined_call_count\":4000000000"), "{json}");
        // The refusal histogram nests as an object keyed by category, in the
        // tally's first-seen order.
        assert!(
            json.contains("\"inline_refusals\":{\"guard-not-emittable\":5,\"budget-exhausted\":1}"),
            "{json}"
        );
    }

    #[test]
    fn inline_and_allocation_fields_stay_distinguishable_from_zero() {
        let fresh = CompilationReport::new("inl/Fresh", "m", "()V", true);
        for (name, m) in [
            ("inline_candidates", fresh.inline_candidates),
            ("inlined_sites", fresh.inlined_sites),
            ("speculative_inlined_sites", fresh.speculative_inlined_sites),
            ("inlined_bytecodes", fresh.inlined_bytecodes),
            ("spills", fresh.spills),
            ("reloads", fresh.reloads),
        ] {
            assert_eq!(m, Measured::NotMeasured, "{name}");
            assert!(
                fresh.to_json().contains(&format!("\"{name}\":null")),
                "{name} must render null: {}",
                fresh.to_json()
            );
        }
        assert_eq!(fresh.inlined_call_count, Measured::NotMeasured);
        assert!(fresh.to_json().contains("\"inlined_call_count\":null"));
        // An empty histogram is an empty object, never a missing key — and it
        // is a different claim from "no tally was harvested", which the
        // `inline_candidates: null` above carries.
        assert!(fresh.inline_refusals.is_empty());
        assert!(fresh.to_json().contains("\"inline_refusals\":{}"));

        // A method the inliner looked at and declined everywhere reports
        // measured zeros, and the two encodings differ.
        let mut none_inlined = fresh.clone();
        none_inlined.inline_candidates = Measured::Value(4);
        none_inlined.inlined_sites = Measured::Value(0);
        none_inlined.inlined_bytecodes = Measured::Value(0);
        none_inlined.inlined_call_count = Measured::Value(0);
        none_inlined.spills = Measured::Value(0);
        none_inlined.reloads = Measured::Value(0);
        let json = none_inlined.to_json();
        assert!(json.contains("\"inlined_sites\":0"), "{json}");
        assert!(json.contains("\"inlined_call_count\":0"), "{json}");
        assert!(json.contains("\"spills\":0"), "{json}");
        assert!(json.contains("\"reloads\":0"), "{json}");
        assert_ne!(fresh.to_json(), json);
    }

    #[test]
    fn ambient_spill_and_reload_hooks_reach_the_innermost_compilation() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(true);
        clear_reports();
        {
            let outer = CompileRecorder::begin("metrics/AllocOuter", "m", "()V", true);
            {
                let _inner = CompileRecorder::begin("metrics/AllocInner", "m", "()V", true);
                // `callee_compiler` nests compilations; an allocator run for the
                // callee must not be billed to the caller.
                note_current_spills(6);
                note_current_reloads(2);
            }
            note_current_spills(0);
            drop(outer);
        }
        let inner = last_compilation_report(Some("metrics/AllocInner")).expect("inner published");
        let outer = last_compilation_report(Some("metrics/AllocOuter")).expect("outer published");
        assert_eq!(inner.spills, Measured::Value(6));
        assert_eq!(inner.reloads, Measured::Value(2));
        // The outer compilation measured zero spills and never measured
        // reloads at all — two different facts, and the report keeps them apart.
        assert_eq!(outer.spills, Measured::Value(0));
        assert_eq!(outer.reloads, Measured::NotMeasured);
        let json = outer.to_json();
        assert!(json.contains("\"spills\":0"), "{json}");
        assert!(json.contains("\"reloads\":null"), "{json}");
        clear_reports();
        set_enabled_for_test(false);
    }

    #[test]
    fn json_escape_covers_control_characters() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("x\u{1}y"), "x\\u0001y");
        assert_eq!(json_escape("plain/name;()I"), "plain/name;()I");
    }

    // ── Scheduling counters ──────────────────────────────────────────

    /// Every OSR event is reported, in a fixed order, including zeros.
    ///
    /// The zeros are the point: "this never happened" is information, and a
    /// summary whose row set changes with the run is one no sink can diff.
    /// The exact edit that trips it: add a name to `OSR_EVENTS` without
    /// widening `OSR_COUNTERS`, which is a compile error, or reorder them,
    /// which this catches.
    #[test]
    fn osr_counts_report_every_event_in_a_fixed_order() {
        let names: Vec<&str> = osr_counts().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, OSR_EVENTS.to_vec());
        assert_eq!(osr_counts().len(), OSR_COUNTERS.len());
    }

    /// `record_osr_event` increments its own row and nothing else, and an
    /// unknown name is ignored rather than panicking — it is called from the
    /// interpreter's hot back-edge path.
    #[test]
    fn record_osr_event_increments_its_row_only() {
        let _guard = METRICS_TEST_LOCK.lock();
        reset_osr_counts();
        record_osr_event("osr_entered");
        record_osr_event("osr_entered");
        record_osr_event("osr_exited");
        record_osr_event("not_an_osr_event");
        let counts: Vec<(&str, u64)> = osr_counts();
        for (name, n) in &counts {
            let want = match *name {
                "osr_entered" => 2,
                "osr_exited" => 1,
                _ => 0,
            };
            assert_eq!(*n, want, "{name}");
        }
        // And it reaches the summary, which is the surface a run actually
        // shows — a counter nothing reports is a counter nobody reads.
        assert!(summary().osr.contains(&("osr_entered", 2)));
        reset_osr_counts();
    }

    #[test]
    fn scheduling_counts_report_every_event_in_a_fixed_order() {
        let names: Vec<&str> = scheduling_counts().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, SCHEDULING_EVENTS.to_vec());
        // The whole point of a fixed row set: an event that never fired still
        // has a row, so "zero drops" and "nobody counts drops" are different
        // readings.
        assert_eq!(names.len(), SCHEDULING_EVENTS.len());
    }

    #[test]
    fn scheduling_events_are_distinct_names() {
        let mut sorted = SCHEDULING_EVENTS.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "two events share a name: {sorted:?}");
    }

    #[test]
    fn recording_a_scheduling_event_is_visible_without_metrics_enabled() {
        let _guard = TEST_LOCK.lock();
        // Explicitly OFF. A dropped compilation request must be countable in a
        // default production run, where `CRATONVM_JIT_METRICS` is unset — this
        // is the property that distinguishes these counters from the ring.
        set_enabled_for_test(false);
        reset_scheduling_counts_for_test();

        record_scheduling_event(SCHEDULING_EVENTS[0]);
        record_scheduling_events(SCHEDULING_EVENTS[0], 4);
        record_scheduling_events(SCHEDULING_EVENTS[3], 2);
        // A zero count must not advance anything.
        record_scheduling_events(SCHEDULING_EVENTS[1], 0);
        // An unknown name is ignored rather than panicking.
        record_scheduling_event("not_an_event");

        assert_eq!(scheduling_count(SCHEDULING_EVENTS[0]), Some(5));
        assert_eq!(scheduling_count(SCHEDULING_EVENTS[1]), Some(0));
        assert_eq!(scheduling_count(SCHEDULING_EVENTS[3]), Some(2));
        assert_eq!(scheduling_count("not_an_event"), None);
        // `inflight_epoch_moved` describes a compile that RAN, so it is not a
        // drop and must stay out of the total.
        record_scheduling_events(SCHEDULING_EVENTS[2], 9);
        assert_eq!(scheduling_dropped_total(), 7);

        reset_scheduling_counts_for_test();
    }

    #[test]
    fn summary_carries_the_scheduling_table_and_json() {
        let _guard = TEST_LOCK.lock();
        set_enabled_for_test(false);
        reset_scheduling_counts_for_test();
        record_scheduling_events(SCHEDULING_EVENTS[0], 3);

        let s = summary();
        let names: Vec<&str> = s.scheduling.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, SCHEDULING_EVENTS.to_vec());
        assert_eq!(
            s.scheduling.iter().find(|(n, _)| *n == SCHEDULING_EVENTS[0]),
            Some(&(SCHEDULING_EVENTS[0], 3))
        );
        let json = s.to_json();
        assert!(
            json.contains("\"scheduling\":{\"queue_dropped_stale_install_epoch\":3"),
            "{json}"
        );

        reset_scheduling_counts_for_test();
    }

    #[test]
    fn measured_helpers_behave() {
        let m: Measured<u32> = Measured::of(5);
        assert_eq!(m.get(), Some(5));
        assert_eq!(m.or(9), 5);
        assert_eq!(m.as_option(), Some(&5));
        assert_eq!(m.json(), "5");
        let n: Measured<u32> = Measured::NotMeasured;
        assert_eq!(n.get(), None);
        assert_eq!(n.or(9), 9);
        assert_eq!(n.as_option(), None);
        assert_eq!(n.json(), "null");
        assert_eq!(Measured::<u32>::default(), Measured::NotMeasured);
    }
}

/// Which emission arm produced each `jit_getfield` CALL.
///
/// The runtime helper counter says compiled code called the helper 49 M times;
/// the collector A/B and the compile-time layout census both came back negative,
/// and a receiver dump showed the failing receivers were INSIDE the published
/// bounds on Generational — i.e. they should have passed the inline guard. That
/// leaves only "a different arm emitted the CALL", and there are six of them.
/// This names the one, at COMPILE time, instead of another round of inference.
///
/// Index: 0 = single-pass inlined-callee, 1 = single-pass compact-inline slow
/// path, 2 = single-pass legacy-inline slow path, 3 = single-pass
/// resolved-but-inline-disabled, 4 = single-pass unresolved, 5 = IR tier
/// helper fallback.
pub static GETFIELD_ARM_EMITS: [std::sync::atomic::AtomicU64; 6] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// What the spliced-call emitter actually EMITTED, per arm.
///
/// A feature that reports itself on while emitting nothing is the failure mode
/// this whole line of work keeps running into: `nest` and `devirt` measured
/// within noise of each other, and without these there is no way to tell "the
/// devirtualised splice did not help" from "the devirtualised splice never
/// fired". Index-parallel with [`INLINE_CALL_ARM_NAMES`].
pub static INLINE_CALL_ARM_EMITS: [std::sync::atomic::AtomicU64; 7] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Names for [`INLINE_CALL_ARM_EMITS`], index-parallel.
pub const INLINE_CALL_ARM_NAMES: [&str; 7] = [
    // A call inside a spliced body, emitted as a raw CALL to a compiled entry.
    "spliced-call-direct",
    // The same, emitted through the blind `jit_invoke_dispatch` helper. Should
    // be 0 unless `CRATONVM_JIT_INLINE_CALL_DISPATCH` is on.
    "spliced-call-dispatch",
    // A statically bound call replaced by the callee's own body.
    "nested-splice",
    // A virtual/interface call replaced by a body behind a receiver class-id
    // guard. This is the devirtualisation counter.
    "nested-splice-guarded",
    // A guarded splice that was PLANNED and then refused at emission — the
    // number that distinguishes "did not help" from "could not be emitted".
    "nested-splice-guarded-refused",
    // A statically bound nested splice that was PLANNED and bailed during
    // emission. The counter set shipped without this, which is why a run
    // showing `nested-splice=0` could not distinguish "no nested site was ever
    // planned" from "every one of them was planned and then rolled back".
    "nested-splice-refused",
    // An OUTER splice that the planner admitted and the emitter rolled back.
    // Everything nested inside it dies with it, so a zero in every nested arm
    // means nothing until this number is known.
    "outer-splice-rolled-back",
];

/// Record that the spliced-call emitter took arm `arm`.
#[inline]
pub fn note_inline_call_arm(arm: usize) {
    if let Some(slot) = INLINE_CALL_ARM_EMITS.get(arm) {
        slot.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Compiled-local-handler census, index-parallel with `LOCAL_HANDLER_COUNTS`.
///
/// Four numbers, because most single readings of "the feature is on" are wrong
/// on their own. `sites-emitted` is a COMPILE-time count and says only that
/// stubs exist. `entered` is the win — a `catch` that ran in compiled code.
/// `propagated` is the miss edge, which is correct behaviour rather than a
/// failure. `methods-armed` separates "no method qualified" from "methods
/// qualified and nothing ever threw".
pub const LOCAL_HANDLER_NAMES: [&str; 4] =
    ["methods-armed", "sites-emitted", "entered", "propagated"];

static LOCAL_HANDLER_COUNTS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Index into [`LOCAL_HANDLER_NAMES`]: a method compiled with local handlers
/// armed.
pub const LOCAL_HANDLER_METHOD_ARMED: usize = 0;
/// Index: one local-handler dispatch stub emitted.
pub const LOCAL_HANDLER_SITE_EMITTED: usize = 1;
/// Index: a `catch` block entered without leaving compiled code.
pub const LOCAL_HANDLER_ENTERED: usize = 2;
/// Index: a throwable this frame does not catch, sent down the old route.
pub const LOCAL_HANDLER_PROPAGATED: usize = 3;

/// Direct-call safepoint blind-spill census, index-parallel with
/// `CALL_SPILL_COUNTS`. Compile-time counts, from the one predicate every
/// direct call to a compiled callee crosses.
///
/// Three numbers because `elided=0` alone has three readings that a timing
/// table cannot tell apart: the elision is off, no direct call was compiled at
/// all, or every one of them had a reason to refuse. The two refusal counters
/// say which reason.
pub const CALL_SPILL_NAMES: [&str; 7] = [
    "elided",
    "oop-arg",
    "no-precise-maps",
    "ref-local-in-reg",
    "marks-inexact",
    "survivor-in-scratch",
    "moving-unpublishable",
];

static CALL_SPILL_COUNTS: [std::sync::atomic::AtomicU64; 7] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Index into [`CALL_SPILL_NAMES`]: the 14-store blind GPR spill was replaced
/// by the 2-instruction safepoint-id publication. The engagement counter.
pub const CALL_SPILL_ELIDED: usize = 0;
/// Index: refused because an ARGUMENT of this call is a reference and is not
/// frame-resident at the `CALL`.
pub const CALL_SPILL_OOP_ARG: usize = 1;
/// Index: refused because this compile has no precise oop maps, so there is no
/// map to publish INSTEAD of the spill.
pub const CALL_SPILL_NO_PRECISE_MAPS: usize = 2;
/// Index: refused because a register-homed local can hold an object reference.
/// On real reference-manipulating code this is expected to dominate, and it is
/// the clause a future narrowing of the spill (rather than an elision of it)
/// would have to attack.
pub const CALL_SPILL_REF_LOCAL_IN_REG: usize = 3;
/// Index: refused because the operand-stack oop marks are absent or inexact.
pub const CALL_SPILL_MARKS_INEXACT: usize = 4;
/// Index: refused because an operand-stack survivor lives in a `Scratch`/`Xmm`
/// register, which the `CALL` clobbers — eliding here would elide the flush
/// that keeps the value alive, not just the root publication.
pub const CALL_SPILL_SURVIVOR_IN_SCRATCH: usize = 5;
/// Index: refused because moving-young cannot publish an empty precise map
/// here (analysis incomplete, or the live-oop home set is non-empty).
pub const CALL_SPILL_MOVING_UNPUBLISHABLE: usize = 6;

/// Bump one [`CALL_SPILL_NAMES`] counter.
#[inline]
pub fn note_call_spill(index: usize) {
    if let Some(slot) = CALL_SPILL_COUNTS.get(index) {
        slot.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `(name, count)` for the direct-call blind-spill census, INCLUDING zeros --
/// see [`CALL_SPILL_NAMES`] for why each zero is a different answer.
pub fn call_spill_counts() -> Vec<(&'static str, u64)> {
    CALL_SPILL_NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| {
            (
                *n,
                CALL_SPILL_COUNTS[i].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .collect()
}

/// Receiver-type-speculation census, index-parallel with
/// `RECEIVER_DESPEC_COUNTS`. Compile-time counts, from the one filter every
/// receiver-guarded call-site intrinsic crosses in
/// `x64::bytecode_walk`'s invoke ladder.
///
/// Three numbers, because a lone `sites-declined=0` has two readings that a
/// timing table cannot tell apart: "the de-spec consult is wired and nothing
/// needed it" and "no guarded site was compiled at all". `guards-emitted`
/// separates them, and `unguarded` says how much of the direct-call traffic
/// the question does not apply to.
pub const RECEIVER_DESPEC_NAMES: [&str; 4] = [
    "unguarded",
    "guards-emitted",
    "sites-declined",
    "profile-declined",
];

static RECEIVER_DESPEC_COUNTS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Index into [`RECEIVER_DESPEC_NAMES`]: a direct-bound call site with no
/// receiver guard (`guard_class_id == 0`), which the de-spec consult never
/// applies to.
pub const RECEIVER_DESPEC_UNGUARDED: usize = 0;
/// Index: a receiver class-id guard emitted -- the site speculates.
pub const RECEIVER_DESPEC_GUARD_EMITTED: usize = 1;
/// Index: a receiver-guarded intrinsic DECLINED because this bci is in the
/// per-bci de-spec registry. Non-zero is the only proof the consult engaged.
pub const RECEIVER_DESPEC_DECLINED: usize = 2;
/// Index: a receiver-guarded intrinsic REFUSED AT RESOLUTION because the
/// method's own receiver profile at that bci says the guarded class is under
/// `MIN_GUARDED_RECEIVER_PCT` of the receivers. This is the cheap half -- it
/// costs no deopts at all, where `sites-declined` costs
/// `PER_BCI_DESPEC_LIMIT` of them plus a recompile.
pub const RECEIVER_DESPEC_PROFILE_DECLINED: usize = 3;

/// Bump one [`RECEIVER_DESPEC_NAMES`] counter.
#[inline]
pub fn note_receiver_despec(index: usize) {
    if let Some(slot) = RECEIVER_DESPEC_COUNTS.get(index) {
        slot.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `(name, count)` for the receiver-type-speculation census, INCLUDING zeros --
/// see [`RECEIVER_DESPEC_NAMES`] for why each zero is a different answer.
pub fn receiver_despec_counts() -> Vec<(&'static str, u64)> {
    RECEIVER_DESPEC_NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| {
            (
                *n,
                RECEIVER_DESPEC_COUNTS[i].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .collect()
}

/// Bump the "escalation spared" counter: a `MakeNotCompilable` withheld by
/// `DeoptimizationLog::recommend_action_at_bci` because the failing bci had
/// already been de-spec'd. Counted separately from the compile-side census
/// because it is a POLICY event, not an emission.
static RECEIVER_DESPEC_ESCALATIONS_SPARED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// See [`RECEIVER_DESPEC_ESCALATIONS_SPARED`].
#[inline]
pub fn note_despec_escalation_spared() {
    RECEIVER_DESPEC_ESCALATIONS_SPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Number of whole-method blacklists withheld because the failing speculation
/// site had already been de-spec'd.
pub fn despec_escalations_spared() -> u64 {
    RECEIVER_DESPEC_ESCALATIONS_SPARED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Bump one [`LOCAL_HANDLER_NAMES`] counter.
#[inline]
pub fn note_local_handler(index: usize) {
    if let Some(slot) = LOCAL_HANDLER_COUNTS.get(index) {
        slot.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `(name, count)` for the compiled-local-handler census, INCLUDING zeros —
/// see [`LOCAL_HANDLER_NAMES`] for why each zero is a different answer.
pub fn local_handler_counts() -> Vec<(&'static str, u64)> {
    LOCAL_HANDLER_NAMES
        .iter()
        .zip(LOCAL_HANDLER_COUNTS.iter())
        .map(|(n, c)| (*n, c.load(std::sync::atomic::Ordering::Relaxed)))
        .collect()
}

/// Names for [`GETFIELD_ARM_EMITS`], index-parallel.
pub const GETFIELD_ARM_NAMES: [&str; 6] = [
    "sp-inlined-callee",
    "sp-compact-inline-slowpath",
    "sp-legacy-inline-slowpath",
    "sp-resolved-inline-disabled",
    "sp-unresolved",
    "ir-helper-fallback",
];

/// Record that emission arm `arm` emitted a `jit_getfield` CALL site.
#[inline]
pub fn note_getfield_arm(arm: usize) {
    if let Some(c) = GETFIELD_ARM_EMITS.get(arm) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `(name, count)` for every spliced-call arm, INCLUDING the zeros.
///
/// Unfiltered on purpose, unlike `getfield_arm_emits`: a zero is the answer
/// here. "devirt measured the same as nest" and "devirt never fired" are
/// different findings, and only an explicit `nested-splice-guarded=0` tells
/// them apart.
pub fn inline_call_arm_emits() -> Vec<(&'static str, u64)> {
    INLINE_CALL_ARM_NAMES
        .iter()
        .zip(INLINE_CALL_ARM_EMITS.iter())
        .map(|(n, c)| (*n, c.load(std::sync::atomic::Ordering::Relaxed)))
        .collect()
}

/// `(name, count)` for every arm that emitted at least one CALL site.
pub fn getfield_arm_emits() -> Vec<(&'static str, u64)> {
    GETFIELD_ARM_NAMES
        .iter()
        .zip(GETFIELD_ARM_EMITS.iter())
        .map(|(n, c)| (*n, c.load(std::sync::atomic::Ordering::Relaxed)))
        .filter(|(_, v)| *v > 0)
        .collect()
}

/// Why the IR tier declined to inline a `getfield`, by early-out.
///
/// The tier A/B (C2 threshold raised out of reach) moved helper calls
/// 48.9 M -> 5.7 M, so ~88% of them are this function's `return false`
/// fallback, which emits an UNGUARDED `CALL jit_getfield`. It has seven
/// early-outs; this says which.
pub static IR_GETFIELD_DECLINE: [std::sync::atomic::AtomicU64; 7] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Names for [`IR_GETFIELD_DECLINE`], index-parallel.
pub const IR_GETFIELD_DECLINE_NAMES: [&str; 7] = [
    "no-helper-addr",
    "no-bytecode-pc",
    "no-compact-slot-for-pc",
    "narrow-oops-or-zgc-barrier",
    "inline-gates-off",
    "type-tag-disagrees",
    "width-not-int-category",
];

/// Record an IR-tier inline-`getfield` refusal.
#[inline]
pub fn note_ir_getfield_decline(reason: usize) {
    if let Some(c) = IR_GETFIELD_DECLINE.get(reason) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `(name, count)` for every refusal reason that fired.
pub fn ir_getfield_declines() -> Vec<(&'static str, u64)> {
    IR_GETFIELD_DECLINE_NAMES
        .iter()
        .zip(IR_GETFIELD_DECLINE.iter())
        .map(|(n, c)| (*n, c.load(std::sync::atomic::Ordering::Relaxed)))
        .filter(|(_, v)| *v > 0)
        .collect()
}
