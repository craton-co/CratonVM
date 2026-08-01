// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-compilation compiler metrics for the JIT.
//!
//! ## Why this exists
//!
//! The C2 review (`docs/known-issues/deep-research-vm-c2.md`) has a P0 lane
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
//! Several quantities the review names are not obtainable without changing
//! modules this task does not own (`regalloc.rs`). Reporting them as `0` would
//! be worse than useless — a reader cannot tell "this method spilled nothing"
//! from "nobody counts spills". Every numeric field is therefore a
//! [`Measured<T>`], which renders as JSON `null` when no call site has supplied
//! it. The uninstrumented fields today are [`CompilationReport::spills`] and
//! [`CompilationReport::reloads`]; their setters exist and are wired to
//! nothing, so a future `regalloc` change is a one-line addition rather than a
//! schema change. [`CompilationReport::peak_live_values`] left that list once
//! `ir_lower` gained liveness-based frame-slot colouring: its slot planner
//! computes the peak, and [`note_current_peak_live_values`] carries it here.
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
    /// Values spilled to the frame. **Not measured** — `regalloc` computes live
    /// ranges but publishes no spill/reload counts, and this task does not own
    /// that file.
    pub spills: Measured<u32>,
    /// Reloads from the frame. **Not measured** — see
    /// [`spills`](Self::spills).
    pub reloads: Measured<u32>,
    /// `CompiledMethod::frame_layout.frame_size` — bytes subtracted from RSP.
    pub frame_bytes: Measured<u32>,
    /// Emitted machine-code bytes (`CompiledMethod::code_bytes().len()`), i.e.
    /// the buffer's write position, not its mapped capacity.
    pub code_bytes: Measured<u32>,
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

    /// Spill count. No call site yet — see
    /// [`CompilationReport::spills`].
    pub fn set_spills(&self, n: usize) {
        self.with_report(|r| r.spills = Measured::Value(n.min(u32::MAX as usize) as u32));
    }

    /// Reload count. No call site yet — see
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
        s.push('}');
        s
    }
}

// ── Test helpers ─────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) fn set_enabled_for_test(on: bool) {
    ENABLED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
}

/// Serializes every test that touches the process-wide enable flag or the
/// report ring.
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
    fn json_escape_covers_control_characters() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("x\u{1}y"), "x\\u0001y");
        assert_eq!(json_escape("plain/name;()I"), "plain/name;()I");
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
