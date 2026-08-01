// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Tiered compilation subsystem.
//!
//! Implements a HotSpot-style tiered compilation pipeline:
//!
//! | Level | Name              | Description                                    |
//! |-------|-------------------|------------------------------------------------|
//! |   0   | Interpreter       | Bytecode interpretation with profiling          |
//! |   1   | C1                | Quick compile, no profiling                     |
//! |   2   | C1WithProfiling   | Quick compile with profiling (transition to C2) |
//! |   3   | FullProfile       | Profile collection only (not compiled)          |
//! |   4   | C2                | Full profile-guided optimization                |
//!
//! Transition graph:
//!
//! ```text
//! Interpreter ──► C1 ──► C2
//!       │                  │
//!       └──► C2 (if has profile + enough invocations)
//!                          │
//!       ◄──────────────────┘  (on deoptimization)
//!       │
//!       └──► C1 (after 3+ deopts → c2_bailout, stays C1)
//! ```

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex, RwLock};
use rustc_hash::FxHashMap;

// `CompilationBroker` consumes the compiler's own structured failure type
// rather than defining a second one — see the broker section below.
use crate::bailout::Bailout;

/// Consecutive background-compile attempts allowed to fail (run but not
/// publish a body) at a given tier before `should_compile` gives up on that
/// method entirely. See `CompilerCore::complete_task` / `should_compile`.
const MAX_TIER_FAIL_RETRIES: u32 = 3;

fn osr_deny_list() -> &'static RwLock<HashSet<MethodKey>> {
    static LIST: std::sync::OnceLock<RwLock<HashSet<MethodKey>>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Process-start timestamp, seeded from [`TieredCompilationManager::new`]
/// (constructed very early during VM init, well before any method can reach
/// a compile threshold). Backs the `elapsed_ms` field of the
/// `CRATONVM_DBG_TIER_ENQUEUE` diagnostic below -- see
/// `docs/known-issues/hibernate/hib-misc-residuals-20260716.md` for why
/// "how far into the process's life did this compile trigger" was the key
/// diagnostic needed to confirm the compile-time-tax mechanism.
fn process_start() -> &'static std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now)
}

/// Milliseconds elapsed since [`process_start`] was first seeded.
fn process_uptime_ms() -> u64 {
    process_start().elapsed().as_millis() as u64
}

/// Returns true when OSR is permanently disabled for this method, without
/// affecting normal invocation-counted JIT compilation.
///
/// `java/util/DualPivotQuicksort.sort` was statically denied here for one
/// day (commit 05f6930e, "Deny OSR for DualPivotQuicksort.sort") as a
/// scoped mitigation for the ES `SortingDigestTests` garbage-index
/// corruption. A parallel, independent investigation the same day
/// (a8c5825d, "Fix IR-lowerer unallocated-slot miscompile corrupting
/// Arrays.sort(double[]) under JIT") found and fixed the actual root
/// cause -- the IR/C2 backend's `slot_of()` returned a bogus `0` default
/// for SSA nodes the scheduler never placed in an emitted block, so an
/// emitted use read `[rbp - 0]` (the saved caller RBP) as data; plus a
/// phantom-return-value push on the raw self-recursive-CALL path for VOID
/// methods, an OSR dead-local mask gap for XMM-resident locals, and a
/// precise-maps RBP-mirror restore gap around Rust-side compiled-entry
/// calls. With those fixed, OSR-entering `DualPivotQuicksort.sort` is safe
/// (`ES SortingDigestTests -Jit on` verified 19/20 with the static deny
/// REMOVED, same as with it present -- the deny was never load-bearing for
/// correctness once a8c5825d landed).
///
/// The static deny stayed in place afterward purely as a perf/complexity
/// artifact of the parallel-fix landing, but it has a real cost: routing
/// every self-recursive call of a denied method through the
/// `self_call_stack_guard` helper path (instead of the fast inlined
/// stack-floor check) on every OSR-entered invocation is itself the
/// documented dominant per-level cost for recursion-heavy code
/// (`perf/throughput-20260710`), and CratonVM's `on_backedge`/`request_osr`
/// dispatch is a per-loop-back-edge hot path -- so a static string-keyed
/// deny check sitting there is pure overhead once nothing needs denying.
/// Removed: OSR is now available for `DualPivotQuicksort.sort` again, and
/// `is_osr_denied` only consults the *dynamic* `osr_deny_list`.
pub fn is_osr_denied(key: &MethodKey) -> bool {
    osr_deny_list().read().contains(key)
}

/// Permanently disable OSR for this method in the current VM process.
pub fn mark_osr_denied(key: MethodKey) {
    osr_deny_list().write().insert(key);
}

/// Test helper: reset process-global OSR deny state.
pub fn clear_osr_deny_list_for_test() {
    osr_deny_list().write().clear();
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationTier
// ───────────────────────────────────────────────────────────────────────────────

/// The compilation level a method is currently executing at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompilationTier {
    /// Level 0: Interpreter with profiling.
    Interpreter,
    /// Level 1: Quick compile, no profiling (client-compiler style).
    C1,
    /// Level 2: Quick compile with profiling (for transition to C2).
    C1WithProfiling,
    /// Level 3: Full profile-guided optimization (profile collection only).
    FullProfile,
    /// Level 4: Full optimization (server-compiler style, current JIT).
    C2,
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationPolicy
// ───────────────────────────────────────────────────────────────────────────────

/// Policy that decides when and how to compile methods.
pub struct CompilationPolicy {
    /// Invocation count threshold for C1 compilation.
    pub c1_threshold: u32,
    /// Invocation count threshold for C2 compilation.
    pub c2_threshold: u32,
    /// Back-edge count threshold for OSR compilation.
    pub osr_threshold: u32,
    /// Whether tiered compilation is enabled.
    pub tiered_enabled: bool,
    /// Minimum number of invocations before considering C2.
    pub c2_min_invocations: u32,
    /// Whether to use profiling in the C1 tier.
    pub c1_profiling: bool,
}

impl Default for CompilationPolicy {
    fn default() -> Self {
        Self {
            // The invocation dispatch gate starts consulting this policy at
            // `CRATONVM_JIT_THRESHOLD` (500 by default).  Keep the first
            // background-tier admission aligned with that gate: delaying C1
            // until 1500 left reflection-heavy, short-lived bootstraps fully
            // interpreted for an additional thousand hot calls.  Those calls
            // dominate Hibernate/JAXB model construction, while the worker is
            // otherwise idle.  C2 remains deliberately conservative so a
            // one-shot process still avoids expensive optimizing recompiles.
            c1_threshold: 500,
            c2_threshold: 20_000,
            osr_threshold: 10_000,
            tiered_enabled: true,
            c2_min_invocations: 1_000,
            c1_profiling: true,
        }
    }
}

impl CompilationPolicy {
    /// wire-tiered-manager Step 6: a [`Default`] policy with `CRATONVM_TIER_*`
    /// environment overrides applied. HotSpot's defaults are the reference, but
    /// CratonVM's compile cost differs, so these knobs let the tiered pipeline
    /// be re-tuned on the app gauntlet without a recompile. Read once at VM init
    /// (cold path), so it consults the process environment directly.
    ///
    /// | env var                            | field                | default |
    /// |------------------------------------|----------------------|---------|
    /// | `CRATONVM_TIER_C1_THRESHOLD`       | `c1_threshold`       | 500     |
    /// | `CRATONVM_TIER_C2_THRESHOLD`       | `c2_threshold`       | 20000   |
    /// | `CRATONVM_TIER_OSR_THRESHOLD`      | `osr_threshold`      | 10000   |
    /// | `CRATONVM_TIER_C2_MIN_INVOCATIONS` | `c2_min_invocations` | 1000    |
    /// | `CRATONVM_TIER_ENABLED=0`          | `tiered_enabled`     | true    |
    ///
    /// (The per-frame back-edge OSR trigger — `Frame::should_try_osr` — is a
    /// separate live knob, `CRATONVM_TIER_OSR_BACKEDGE`, read VM-side because it
    /// is consulted on the default path too, not only under the tiered manager.)
    pub fn from_env() -> Self {
        Self::with_overrides(|name| cratonvm_types::flags::runtime_var(name).ok())
    }

    /// Testable core of [`from_env`]: apply the `CRATONVM_TIER_*` overrides
    /// resolved through `get` (production passes `cratonvm_types::flags::runtime_var`). Each numeric
    /// knob is parsed as `u32` and clamped to `>= 1` — a `0` threshold would
    /// compile/OSR on the first observation, defeating warmup. An absent or
    /// unparseable value leaves the [`Default`].
    pub fn with_overrides(get: impl Fn(&str) -> Option<String>) -> Self {
        let num =
            |name: &str| -> Option<u32> { get(name)?.trim().parse::<u32>().ok().map(|v| v.max(1)) };
        let mut p = Self::default();
        if let Some(v) = num("CRATONVM_TIER_C1_THRESHOLD") {
            p.c1_threshold = v;
        }
        if let Some(v) = num("CRATONVM_TIER_C2_THRESHOLD") {
            p.c2_threshold = v;
        }
        if let Some(v) = num("CRATONVM_TIER_OSR_THRESHOLD") {
            p.osr_threshold = v;
        }
        if let Some(v) = num("CRATONVM_TIER_C2_MIN_INVOCATIONS") {
            p.c2_min_invocations = v;
        }
        if let Some(s) = get("CRATONVM_TIER_ENABLED") {
            p.tiered_enabled = s != "0" && !s.eq_ignore_ascii_case("false");
        }
        p
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// MethodKey
// ───────────────────────────────────────────────────────────────────────────────

/// Uniquely identifies a method.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodKey {
    pub class_name: String,
    pub method_name: String,
    pub descriptor: String,
}

impl MethodKey {
    pub fn new(
        class_name: impl Into<String>,
        method_name: impl Into<String>,
        descriptor: impl Into<String>,
    ) -> Self {
        Self {
            class_name: class_name.into(),
            method_name: method_name.into(),
            descriptor: descriptor.into(),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// Profile types
// ───────────────────────────────────────────────────────────────────────────────

/// Branch taken/not-taken counts at a single bytecode offset.
#[derive(Debug, Clone, Default)]
pub struct BranchProfile {
    pub taken: u64,
    pub not_taken: u64,
}

/// Receiver type profile at a virtual call site.
#[derive(Debug, Clone, Default)]
pub struct ReceiverProfile {
    /// Top receivers (class_id, count), limited to 3.
    pub receivers: Vec<(u32, u64)>,
    pub total_calls: u64,
}

/// Type profile for checkcast/instanceof.
#[derive(Debug, Clone, Default)]
pub struct TypeProfile {
    /// Observed types (class_id, count).
    pub types: Vec<(u32, u64)>,
    pub total_checks: u64,
}

/// Null-check profile.
#[derive(Debug, Clone, Default)]
pub struct NullProfile {
    pub null_seen: u64,
    pub non_null_seen: u64,
}

/// Aggregated profile data collected during interpreted / C1 execution.
/// T10.9.B: FxHashMap — bytecode-offset keys, hot path per-branch.
#[derive(Debug, Clone, Default)]
pub struct MethodProfile {
    /// Branch taken/not-taken counts per bytecode offset.
    pub branch_counts: FxHashMap<u32, BranchProfile>,
    /// Receiver type profiles per call site.
    pub receiver_profiles: FxHashMap<u32, ReceiverProfile>,
    /// Type profiles for checkcast/instanceof.
    pub type_profiles: FxHashMap<u32, TypeProfile>,
    /// Null check profiles.
    pub null_profiles: FxHashMap<u32, NullProfile>,
    /// Total profiled invocations.
    pub profiled_invocations: u64,
}

// ───────────────────────────────────────────────────────────────────────────────
// MethodState
// ───────────────────────────────────────────────────────────────────────────────

/// Tracks per-method compilation state.
#[derive(Debug, Clone)]
pub struct MethodState {
    /// Unique method identifier.
    pub method_key: MethodKey,
    /// Current compilation tier.
    pub current_tier: CompilationTier,
    /// Number of interpreter invocations.
    pub invocation_count: u64,
    /// Number of back-edge executions (loop iterations).
    pub backedge_count: u64,
    /// Whether currently queued for compilation.
    pub queued_for_compilation: bool,
    /// Tier at which it is queued.
    pub queued_tier: Option<CompilationTier>,
    /// Number of deoptimizations.
    pub deopt_count: u32,
    /// Last compilation time in milliseconds.
    pub last_compile_time_ms: u64,
    /// Whether this method is too complex for C2.
    pub c2_bailout: bool,
    /// Profile data collected during interpreted/C1 execution.
    pub profile: MethodProfile,
    /// Consecutive background-compile attempts that ran but did not
    /// publish a compiled body (`complete_task(success=false)`). Distinct
    /// from `c2_bailout` (which tracks *runtime* deopt-driven demotion from
    /// an already-published C2 body) — this tracks the compile STEP itself
    /// never producing/publishing code, so `current_tier` never advances
    /// past whatever tier last actually succeeded. `should_compile` stops
    /// recommending further attempts once this saturates, mirroring the
    /// existing "3+ deopts" convention for `c2_bailout` above.
    ///
    /// Counts **compile attempts that ran and failed** only. A task the VM
    /// declined on policy grounds never lands here — see [`Self::ineligible`].
    pub tier_fail_count: u32,
    /// The VM declined to compile this method for a reason that cannot change
    /// for the life of the process — the JIT skip list rejected it, or a
    /// permanent OSR denial applies.
    ///
    /// This exists because the two outcomes used to be conflated. A policy
    /// decline reported `success=false` exactly like a failed compile, so a
    /// permanently-ineligible method was enqueued and declined **three times**
    /// before `tier_fail_count` saturated and the tier gates gave up. Two of
    /// those three round-trips were pure waste (queue traffic, a worker
    /// wake-up and a skip-list evaluation each), and the hot interpreter path
    /// kept paying tier-up bookkeeping until the ban finally landed.
    ///
    /// The worse cost was diagnostic. `hot_but_stuck_in_interpreter` reported
    /// `tier_fail_count=3` identically for methods banned *by design*
    /// (`org/hibernate/` wholesale, `org/h2/` via HIB-LONGTAIL.1, the
    /// `AbstractQueuedSynchronizer` state family) and for methods whose
    /// codegen genuinely broke. That is what made "1531 of 1642 hot methods
    /// never compile" impossible to act on without re-deriving every entry by
    /// hand — see
    /// `docs/known-issues/hibernate/smoketests-concurrent-println-timeout-20260723.md`.
    ///
    /// Recording the decline once, under its own flag, ends the churn on the
    /// first attempt and leaves `tier_fail_count` meaning only what its name
    /// says.
    pub ineligible: bool,
}

impl MethodState {
    fn new(key: MethodKey) -> Self {
        Self {
            method_key: key,
            current_tier: CompilationTier::Interpreter,
            invocation_count: 0,
            backedge_count: 0,
            queued_for_compilation: false,
            queued_tier: None,
            deopt_count: 0,
            last_compile_time_ms: 0,
            c2_bailout: false,
            profile: MethodProfile::default(),
            tier_fail_count: 0,
            ineligible: false,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationQueue
// ───────────────────────────────────────────────────────────────────────────────

/// Priority of a compilation task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilationPriority {
    High,
    Normal,
    Low,
}

/// A single compilation task.
///
/// `PartialEq`/`Eq` were added for [`CompilationBroker`]'s tests, which assert
/// on the exact request a queue shed or handed back. Every field was already
/// comparable; nothing about the task's meaning changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationTask {
    pub method_key: MethodKey,
    pub target_tier: CompilationTier,
    pub priority: CompilationPriority,
    pub enqueue_time_ms: u64,
    /// If this is an OSR compilation, the bytecode index.
    pub osr_bci: Option<u32>,
}

/// Priority queue for compilation tasks.
pub struct CompilationQueue {
    /// High priority: C2 recompilations.
    high: VecDeque<CompilationTask>,
    /// Normal priority: C1 compilations.
    normal: VecDeque<CompilationTask>,
    /// Low priority: speculative compilations.
    low: VecDeque<CompilationTask>,
    /// Total tasks processed.
    total_processed: u64,
}

impl CompilationQueue {
    fn new() -> Self {
        Self {
            high: VecDeque::new(),
            normal: VecDeque::new(),
            low: VecDeque::new(),
            total_processed: 0,
        }
    }

    fn enqueue(&mut self, task: CompilationTask) {
        match task.priority {
            CompilationPriority::High => self.high.push_back(task),
            CompilationPriority::Normal => self.normal.push_back(task),
            CompilationPriority::Low => self.low.push_back(task),
        }
    }

    fn dequeue(&mut self) -> Option<CompilationTask> {
        let task = self
            .high
            .pop_front()
            .or_else(|| self.normal.pop_front())
            .or_else(|| self.low.pop_front());
        if task.is_some() {
            self.total_processed += 1;
        }
        task
    }

    fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.low.len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilerCore — shared between the manager and the background compile thread
// ───────────────────────────────────────────────────────────────────────────────

/// State the background compile thread shares with the manager.
///
/// The compilation queue, its wake-up condvar, the "compiler is running" flag,
/// and the shutdown flag all live here behind an [`Arc`] so the spawned worker
/// can drain the queue off the mutator thread without a back-reference to the
/// whole [`TieredCompilationManager`] (which is owned by `SharedVm` by value).
struct CompilerCore {
    /// Per-method compilation state.
    /// T10.9.B: FxHashMap — MethodKey (internal class/name/desc) is trusted.
    ///
    /// Lives here (rather than on the manager) so the background compile thread,
    /// which only holds an `Arc<CompilerCore>`, can update a method's tier /
    /// queued flag on completion under the same lock the mutator-side API uses.
    methods: Mutex<FxHashMap<MethodKey, MethodState>>,
    /// Pending compilation tasks (three-priority).
    queue: Mutex<CompilationQueue>,
    /// Signalled whenever a task is enqueued or shutdown is requested.
    wake: Condvar,
    /// Whether a background worker is currently running.
    active: AtomicBool,
    /// Requests the worker to stop: it finishes any in-flight compile, then
    /// exits at the next queue check (remaining queued tasks are abandoned —
    /// acceptable at teardown).
    shutdown: AtomicBool,
    /// Number of tasks the worker has finished compiling (for tests/diagnostics).
    completed: AtomicU64,
    /// Aggregate compilation statistics (shared so the worker can update them).
    stats: CompilationStats,
}

impl CompilerCore {
    fn new() -> Self {
        Self {
            methods: Mutex::new(FxHashMap::default()),
            queue: Mutex::new(CompilationQueue::new()),
            wake: Condvar::new(),
            active: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            completed: AtomicU64::new(0),
            stats: CompilationStats::default(),
        }
    }

    /// Push a task and wake the worker (if any).
    fn enqueue(&self, task: CompilationTask) {
        self.queue.lock().enqueue(task);
        self.wake.notify_one();
    }

    /// C1→C2 supersede: enqueue a Low-priority C2 recompile for a method
    /// whose C1 body just published. Idempotent: skipped when the method is
    /// already queued, already at C2, has bailed out of C2, or has exhausted
    /// its compile retries. Called by the worker loop AFTER
    /// [`Self::complete_task`] cleared the C1 task's queued flag.
    fn request_c2_upgrade(&self, key: &MethodKey) {
        {
            let mut methods = self.methods.lock();
            let Some(state) = methods.get_mut(key) else {
                return;
            };
            if state.queued_for_compilation
                || state.current_tier >= CompilationTier::C2
                || state.c2_bailout
                || state.ineligible
                || state.tier_fail_count >= MAX_TIER_FAIL_RETRIES
            {
                return;
            }
            state.queued_for_compilation = true;
            state.queued_tier = Some(CompilationTier::C2);
        }
        self.enqueue(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::Low,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
    }

    /// Pop the highest-priority task, or `None` if the queue is empty.
    fn dequeue(&self) -> Option<CompilationTask> {
        self.queue.lock().dequeue()
    }

    /// Record that `key` finished a compile attempt at `tier`. Mirrors
    /// [`TieredCompilationManager::compilation_complete`] but operates purely on
    /// the shared core so the background worker needs no back-reference to the
    /// (by-value, non-`Arc`) manager.
    ///
    /// `success` reports whether the attempt actually produced and published a
    /// compiled body (vs. `compile_fn` running but bailing internally — a
    /// skip-listed construct, a resolver miss, the code-cache cap, a
    /// concurrent class redefine, etc.). Only on success does `current_tier`
    /// advance to `tier` and the per-tier compilation stat increment — a
    /// failed attempt must NOT be recorded as "compiled", or the method is
    /// silently stuck interpreting forever: `current_tier` would already read
    /// as "done" for that tier, so `should_compile` would never recommend it
    /// again, and nothing was ever inserted into `jit_cache` for the
    /// interpreter's fast-path lookup to find.
    ///
    /// `osr` marks a back-edge OSR task. A successful OSR publish goes into
    /// the SEPARATE OSR artifact cache — the method-entry cache is still
    /// empty — so it must NOT advance `current_tier`: `should_compile` reads
    /// `current_tier` as "method-entry compiled through this tier" and
    /// returns `None` at C2, which starved the method-entry compile of any
    /// loop-heavy method whose OSR body published first (the invocation
    /// counter kept firing but every recommendation was refused, so each
    /// fresh call re-entered the interpreter and re-OSR'd forever — observed
    /// as QuickBench `sieve` never retiring its per-call interpreter warmup
    /// across 20,000 invocations). This is the mirror image of the
    /// `request_osr` decoupling introduced with the independent OSR cache:
    /// OSR requests are not suppressed by method-entry C2, and method-entry
    /// tiering must not be suppressed by an OSR artifact. Queue flags and
    /// fail counters still clear/advance normally so both pipelines share
    /// the single in-flight slot.
    fn complete_task(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        compile_time_ms: u64,
        success: bool,
        osr: bool,
        declined_permanently: bool,
    ) {
        {
            let mut methods = self.methods.lock();
            if let Some(state) = methods.get_mut(key) {
                if success {
                    if !osr {
                        state.current_tier = tier;
                    }
                    state.tier_fail_count = 0;
                } else if declined_permanently {
                    // Policy verdict, not a compile failure: the skip list and
                    // the OSR-denial set are pure functions of inputs that are
                    // fixed once the method is loaded, so re-asking can only
                    // produce the same answer. Record it once and stop; do NOT
                    // spend `tier_fail_count`, which exists to bound genuinely
                    // failing codegen. See `MethodState::ineligible`.
                    state.ineligible = true;
                } else {
                    state.tier_fail_count = state.tier_fail_count.saturating_add(1);
                }
                state.queued_for_compilation = false;
                state.queued_tier = None;
                state.last_compile_time_ms = compile_time_ms;
                // jit-inlining-and-ir-calls — tier-4 compile-time guard.
                //
                // The optimizing IR pipeline's admission rule was widened
                // substantially on 2026-07-26: the invoke / field / static-field
                // caps went from 5 to 64 and the bytecode-size cap from 200 to
                // HotSpot's 8000-byte HugeMethodLimit, so the population of
                // methods reaching C2 is now ordinary application code rather
                // than a handful of small arithmetic kernels. `ir_compatible`
                // and `ir::IR_MAX_GRAPH_NODES` bound the *static* inputs to a
                // compile, but nothing bounded the OBSERVED cost.
                //
                // This closes that: a C2 compile that actually took longer than
                // `MAX_C2_COMPILE_TIME_MS` is not re-attempted at C2 — the
                // method degrades to C1 exactly as if it had deopted three
                // times. Reusing `c2_bailout` rather than adding new state is
                // deliberate: every existing degradation path already consults
                // it (`should_compile`, `on_backedge`, `request_osr`,
                // `request_c2_upgrade`), so the demotion is coherent with the
                // deopt-driven one by construction, and the `c2_bailouts`
                // statistic keeps counting the whole "stopped trying C2"
                // population.
                //
                // Note this is measured on a compile that SUCCEEDED — a slow
                // failure is already handled by `tier_fail_count`. A one-off
                // slow compile on a contended host will demote a method that
                // might have been fine; that is the intended conservative
                // direction (C1 code is correct code, just less optimized).
                if tier == CompilationTier::C2
                    && compile_time_ms > MAX_C2_COMPILE_TIME_MS
                    && !state.c2_bailout
                {
                    state.c2_bailout = true;
                    self.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if success {
            match tier {
                CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                    self.stats.c1_compilations.fetch_add(1, Ordering::Relaxed);
                }
                CompilationTier::C2 => {
                    self.stats.c2_compilations.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        self.stats
            .total_compile_time_ms
            .fetch_add(compile_time_ms, Ordering::Relaxed);
        self.completed.fetch_add(1, Ordering::Release);
    }
}

/// Result of one background compile attempt, reported by the VM's
/// [`CompileFn`] callback.
#[derive(Debug, Clone, Copy)]
pub struct CompileOutcome {
    /// Wall-clock compile time in milliseconds.
    pub compile_time_ms: u64,
    /// `true` only if the attempt actually published a compiled body (see
    /// [`CompilerCore::complete_task`] for why this must not be conflated
    /// with "the task was processed").
    pub published: bool,
    /// C1→C2 supersede: the VM judged this method would take the optimizing
    /// IR pipeline at C2 AND is expected to benefit (see the VM-side
    /// `c2_upgrade_would_engage` predicate). After a successful C1-family
    /// publish the worker loop enqueues a Low-priority C2 recompile whose
    /// publish REPLACES the C1 body in the jit cache.
    pub c2_upgrade_candidate: bool,
    /// The attempt did not publish because the VM *declined* the method on
    /// grounds that are fixed for the life of the process (skip list, OSR
    /// denial) — as opposed to a compile that ran and failed.
    ///
    /// Set this and the tier manager records the decision once
    /// ([`MethodState::ineligible`]) instead of spending the method's
    /// `tier_fail_count` retry budget on a verdict that cannot change. Ignored
    /// when `published` is true.
    pub declined_permanently: bool,
}

impl CompileOutcome {
    /// A compile that ran and failed — spends one retry.
    pub fn failed(compile_time_ms: u64) -> Self {
        Self {
            compile_time_ms,
            published: false,
            c2_upgrade_candidate: false,
            declined_permanently: false,
        }
    }

    /// The VM refused the method on policy grounds — recorded once, never
    /// retried, and not counted as a compile failure.
    pub fn declined(compile_time_ms: u64) -> Self {
        Self {
            compile_time_ms,
            published: false,
            c2_upgrade_candidate: false,
            declined_permanently: true,
        }
    }
}

/// A compile callback invoked on the background thread for each drained task.
/// The actual codegen is supplied by the VM at startup; `tiered.rs` only owns
/// the scheduling.
pub type CompileFn = Box<dyn Fn(&CompilationTask) -> CompileOutcome + Send + 'static>;

/// Whether a target tier should use the **optimized** (C2-equivalent) backend.
///
/// wire-tiered-manager increment 2 / Step 3: this is the policy half of the
/// C1/C2 backend split. `C1`/`C1WithProfiling` map to the fast single-pass
/// (no-opt) backend; `C2` (and the `FullProfile` collection tier, which only
/// reaches codegen as a C2 promotion) map to the optimizing pipeline.
///
/// The *codegen* half lives VM-side (the `cratonvm-jit` crate cannot reference
/// `SharedVm` or the interpreter's compile entry points), so the VM-supplied
/// [`CompileFn`] consumes this to pick its compile strategy. As of
/// wire-tiered-manager Step 3 this drives **real backend routing**, not an
/// advisory hint: `background_compile_task` threads the returned boolean into
/// `jit::try_compile`'s trailing `optimize` flag — `true` runs the optimizing
/// IR pipeline, `false` skips it and routes to the single-pass `x64::compile`
/// (C1) backend.
#[inline]
pub fn tier_uses_optimized_backend(tier: CompilationTier) -> bool {
    matches!(tier, CompilationTier::C2 | CompilationTier::FullProfile)
}

/// Handle to the spawned background compile thread.
///
/// Dropping the handle (or calling [`BackgroundCompiler::shutdown`]) signals the
/// worker to finish and joins it, so no compile thread outlives the VM.
pub struct BackgroundCompiler {
    core: Arc<CompilerCore>,
    handle: Option<JoinHandle<()>>,
}

impl BackgroundCompiler {
    /// Request shutdown and join the worker thread.
    pub fn shutdown(&mut self) {
        // Set the shutdown flag while holding the queue lock the worker parks
        // on. Without this, a lost-wakeup race deadlocks `join()`: the worker
        // can load `shutdown == false` (under the queue lock), then this thread
        // could store `true` + `notify_all` before the worker reaches
        // `wake.wait()`, so the worker parks AFTER the notify and never wakes.
        // Holding the lock here forces our store to land either while the worker
        // is already parked (so `notify_all` wakes it) or before it re-checks
        // the flag — `parking_lot::Condvar::wait` registers the waiter before
        // releasing the lock, so no notification can be missed.
        {
            let _q = self.core.queue.lock();
            self.core.shutdown.store(true, Ordering::Release);
        }
        self.core.wake.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.core.active.store(false, Ordering::Release);
    }
}

impl Drop for BackgroundCompiler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// Process-global background compiler
// ───────────────────────────────────────────────────────────────────────────────

/// Holds the single process-wide [`BackgroundCompiler`] handle for its lifetime.
///
/// The interpreter's invocation hook runs on the mutator and only has a `&self`
/// borrow of the by-value `SharedVm::tiered_manager`; it can't own the worker
/// handle. We park the handle here so the worker keeps running (the handle is
/// not dropped) and a single worker is started exactly once via [`Once`].
static BACKGROUND_COMPILER: Mutex<Option<BackgroundCompiler>> = Mutex::new(None);
static BACKGROUND_COMPILER_INIT: std::sync::Once = std::sync::Once::new();

/// Diagnostic-only handle to the (singular, per-process) [`CompilerCore`] —
/// see [`TieredCompilationManager::new`] and [`dump_method_stats_to_stderr`].
static DIAG_CORE: std::sync::OnceLock<Arc<CompilerCore>> = std::sync::OnceLock::new();
/// Diagnostic-only snapshot of the active policy's `c1_threshold`, captured
/// at [`TieredCompilationManager::new`] — used by
/// [`dump_method_stats_to_stderr`] to flag methods that crossed the
/// promotion threshold but never actually got promoted.
static DIAG_C1_THRESHOLD: AtomicU64 = AtomicU64::new(0);

/// Per-method invocation-vs-promotion counts, aggregated across every method
/// this process has ever tracked. See [`dump_method_stats_to_stderr`].
#[derive(Default)]
struct MethodPromotionSnapshot {
    distinct_methods: u64,
    methods_ever_invoked: u64,
    total_invocations: u64,
    methods_still_interpreted: u64,
    methods_at_c1: u64,
    methods_at_full_profile: u64,
    methods_at_c2: u64,
}

/// `CRATONVM_DBG_JIT_METHOD_STATS=1` diagnostic: dump, to stderr, how many
/// distinct methods this process ever tracked, how many were actually
/// invoked, and how many reached each compilation tier — plus the aggregate
/// compile counts/time already tracked in [`CompilationStats`]. Written to
/// characterize whether a slow run is dominated by code that genuinely never
/// gets hot enough to promote past the interpreter (as opposed to a stuck
/// lock, a cache-thrashing hot path, or some other fixable inefficiency) —
/// see `docs/known-issues/elasticsearch-suite/ES-PERF-20260719-testSlicesDense-interpreter-throughput.md`.
/// No-op if no [`TieredCompilationManager`] was ever constructed this process
/// (should not happen in the normal VM binary, but keeps this safe to call
/// unconditionally from an exit hook).
pub fn dump_method_stats_to_stderr() {
    let Some(core) = DIAG_CORE.get() else {
        return;
    };
    let c1_threshold = DIAG_C1_THRESHOLD.load(Ordering::Relaxed);
    let mut snap = MethodPromotionSnapshot::default();
    // (invocation_count, queued_for_compilation, tier_fail_count, ineligible,
    // name) for every Interpreter-tier method whose invocation_count already
    // crossed c1_threshold. Splitting `ineligible` out matters: a method the
    // skip list refuses is stuck BY DESIGN and is not evidence of anything,
    // whereas one with a non-zero `tier_fail_count` is a compiler failure.
    // Reporting both as `tier_fail_count=3` is what made an earlier
    // "1531 of 1642 hot methods never compile" reading unactionable.
    let mut hot_but_stuck: Vec<(u64, bool, u32, bool, String)> = Vec::new();
    let mut ineligible_by_policy: u64 = 0;
    {
        let methods = core.methods.lock();
        for state in methods.values() {
            snap.distinct_methods += 1;
            snap.total_invocations += state.invocation_count;
            if state.invocation_count > 0 {
                snap.methods_ever_invoked += 1;
            }
            match state.current_tier {
                CompilationTier::Interpreter => {
                    snap.methods_still_interpreted += 1;
                    if state.invocation_count >= c1_threshold {
                        if state.ineligible {
                            ineligible_by_policy += 1;
                        }
                        hot_but_stuck.push((
                            state.invocation_count,
                            state.queued_for_compilation,
                            state.tier_fail_count,
                            state.ineligible,
                            format!(
                                "{}.{}{}",
                                state.method_key.class_name,
                                state.method_key.method_name,
                                state.method_key.descriptor
                            ),
                        ));
                    }
                }
                CompilationTier::C1 | CompilationTier::C1WithProfiling => snap.methods_at_c1 += 1,
                CompilationTier::FullProfile => snap.methods_at_full_profile += 1,
                CompilationTier::C2 => snap.methods_at_c2 += 1,
            }
        }
    }
    let stats = &core.stats;
    eprintln!(
        "[cratonvm] JIT method stats: {} distinct methods tracked, {} ever invoked, {} total invocations \
         | still-interpreted={} c1={} full-profile={} c2={} \
         | compiles: c1={} c2={} osr={} deopts={} c2_bailouts={} total_compile_time_ms={} \
         | c1_threshold={} hot_but_stuck_in_interpreter={} (of which ineligible-by-policy={}, compile-failures={})",
        snap.distinct_methods,
        snap.methods_ever_invoked,
        snap.total_invocations,
        snap.methods_still_interpreted,
        snap.methods_at_c1,
        snap.methods_at_full_profile,
        snap.methods_at_c2,
        stats.c1_compilations.load(Ordering::Relaxed),
        stats.c2_compilations.load(Ordering::Relaxed),
        stats.osr_compilations.load(Ordering::Relaxed),
        stats.deoptimizations.load(Ordering::Relaxed),
        stats.c2_bailouts.load(Ordering::Relaxed),
        stats.total_compile_time_ms.load(Ordering::Relaxed),
        c1_threshold,
        hot_but_stuck.len(),
        ineligible_by_policy,
        hot_but_stuck
            .iter()
            .filter(|(_, _, fail, inelig, _)| *fail > 0 && !*inelig)
            .count(),
    );
    if !hot_but_stuck.is_empty() {
        hot_but_stuck.sort_by(|a, b| b.0.cmp(&a.0));
        eprintln!(
            "[cratonvm] JIT method stats: top {} hot-but-stuck methods (invocations, queued, tier_fail_count, why, name):",
            hot_but_stuck.len().min(30)
        );
        for (count, queued, fail, inelig, name) in hot_but_stuck.iter().take(30) {
            let why = if *inelig {
                "ineligible-by-policy"
            } else if *fail > 0 {
                "compile-failed"
            } else {
                "not-yet-attempted"
            };
            eprintln!(
                "[cratonvm]   {count:>10} queued={queued:<5} tier_fail_count={fail:<3} {why:<20} {name}"
            );
        }
        // The compile failures are the only actionable entries here — a
        // policy decline is stuck by design — but they are usually a tiny
        // minority and get buried under the policy ones when the list is
        // ranked by invocation count (measured on the Hibernate concurrency
        // workload: 1513 policy declines vs 6 real failures, none of which
        // appeared in the top 30). List them separately so the actionable set
        // is never hidden by the expected one.
        let failures: Vec<_> = hot_but_stuck
            .iter()
            .filter(|(_, _, fail, inelig, _)| *fail > 0 && !*inelig)
            .collect();
        if !failures.is_empty() {
            eprintln!(
                "[cratonvm] JIT method stats: {} hot method(s) whose COMPILE FAILED (not policy — these are bugs):",
                failures.len()
            );
            for (count, queued, fail, _, name) in failures.iter().take(30) {
                eprintln!(
                    "[cratonvm]   {count:>10} queued={queued:<5} tier_fail_count={fail:<3} {name}"
                );
            }
        }
    }
}

/// Idempotently start the background compile thread for `mgr`.
///
/// Safe to call on every interpreter invocation hook — the spawn happens at most
/// once (guarded by [`Once`]). `compile_fn` performs the actual codegen for a
/// drained task off the mutator thread; for increment 1 the VM passes a
/// drain-only closure (real codegen wiring lands when the VM-init path can be
/// touched). The worker handle is parked in a process-global so it lives for the
/// VM's lifetime; the OS reclaims the thread at process exit.
pub fn ensure_background_compiler<F>(mgr: &TieredCompilationManager, make_compile_fn: F)
where
    F: FnOnce() -> CompileFn,
{
    BACKGROUND_COMPILER_INIT.call_once(|| {
        if let Some(handle) = mgr.start_background_compiler(make_compile_fn()) {
            *BACKGROUND_COMPILER.lock() = Some(handle);
        }
    });
}

/// Stop the process-global background compiler (drains+joins). Mainly for tests
/// and orderly shutdown; production relies on process exit.
pub fn shutdown_background_compiler() {
    if let Some(mut handle) = BACKGROUND_COMPILER.lock().take() {
        handle.shutdown();
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationStats
// ───────────────────────────────────────────────────────────────────────────────

/// Aggregate statistics for the compilation subsystem.
#[derive(Debug, Default)]
pub struct CompilationStats {
    pub c1_compilations: AtomicU64,
    pub c2_compilations: AtomicU64,
    pub osr_compilations: AtomicU64,
    pub deoptimizations: AtomicU64,
    pub c2_bailouts: AtomicU64,
    pub total_compile_time_ms: AtomicU64,
}

// ───────────────────────────────────────────────────────────────────────────────
// TieredCompilationManager
// ───────────────────────────────────────────────────────────────────────────────

/// Central coordinator for tiered compilation decisions.
pub struct TieredCompilationManager {
    /// Per-method state + queue + stats + wake/shutdown flags, shared with the
    /// background compile thread (which holds an `Arc<CompilerCore>`).
    core: Arc<CompilerCore>,
    /// Compilation policy.
    policy: Mutex<CompilationPolicy>,
}

/// Maximum number of deoptimizations before bailing out of C2.
const MAX_DEOPTS_BEFORE_BAILOUT: u32 = 3;

/// Wall-clock budget for a single C2 (optimizing IR) compile, in milliseconds.
///
/// A successful C2 compile that exceeds this demotes the method to C1 for the
/// rest of the process, via the same `c2_bailout` flag the 3-deopt rule uses.
/// See the guard in `CompilerCore::complete_task` for the full rationale.
///
/// 250 ms is roughly two orders of magnitude above a typical C2 compile in this
/// JIT (single-digit milliseconds for the arithmetic kernels that historically
/// reached tier 4), so it never fires on healthy input — it exists to catch a
/// pathological method that slips past `ir_compatible`'s static budgets and
/// `ir::IR_MAX_GRAPH_NODES`, not to tune throughput.
pub const MAX_C2_COMPILE_TIME_MS: u64 = 250;

/// Maximum number of receiver types tracked per call site.
const MAX_RECEIVER_TYPES: usize = 3;

impl TieredCompilationManager {
    /// Create a new manager with the given policy.
    pub fn new(policy: CompilationPolicy) -> Self {
        // Seed the process-start timestamp as early as possible (this
        // manager is constructed during VM init) so `process_uptime_ms`
        // reports genuine process age, not "time since first compile".
        process_start();
        let core = Arc::new(CompilerCore::new());
        // Diagnostic-only: the VM constructs exactly one manager per process
        // (this is an embedded single-JVM-per-process binary, not a
        // multi-tenant host), so a "last one registered" global handle is
        // safe here purely for `dump_method_stats_to_stderr`'s exit-time
        // introspection — see that function's doc comment.
        let _ = DIAG_CORE.set(core.clone());
        DIAG_C1_THRESHOLD.store(policy.c1_threshold as u64, Ordering::Relaxed);
        Self {
            core,
            policy: Mutex::new(policy),
        }
    }

    /// Remove queued and historical tiering state for an unloaded class.
    pub fn invalidate_class(&self, class_name: &str) {
        self.core
            .methods
            .lock()
            .retain(|key, _| key.class_name != class_name);
        let mut queue = self.core.queue.lock();
        queue
            .high
            .retain(|task| task.method_key.class_name != class_name);
        queue
            .normal
            .retain(|task| task.method_key.class_name != class_name);
        queue
            .low
            .retain(|task| task.method_key.class_name != class_name);
    }

    /// Create a new manager with the default policy.
    pub fn with_default_policy() -> Self {
        Self::new(CompilationPolicy::default())
    }

    /// wire-tiered-manager Step 6: create a manager whose policy honors the
    /// `CRATONVM_TIER_*` environment overrides (see [`CompilationPolicy::from_env`]).
    /// Used by `SharedVm::new`; an unset environment is identical to
    /// [`with_default_policy`].
    pub fn with_env_policy() -> Self {
        Self::new(CompilationPolicy::from_env())
    }

    // ── Invocation / back-edge hooks ─────────────────────────────────────

    /// Called on each method invocation from the interpreter.
    /// Increments the counter and checks if compilation should be triggered.
    /// Returns the target tier if compilation was enqueued.
    pub fn on_method_invocation(&self, key: &MethodKey) -> Option<CompilationTier> {
        self.on_method_invocation_observed(key, 0)
    }

    /// [`Self::on_method_invocation`], but fast-forwarded to an externally
    /// observed invocation count.
    ///
    /// The interpreter counts every invocation in its own profile store but
    /// consults the tiered manager only at stride boundaries (the
    /// `JIT_RETRY_STRIDE = 64` schedule past the warmup threshold). With the
    /// plain `+= 1` counting, the manager's view of "hotness" was therefore
    /// 64x DEFLATED: a method needed `c1_threshold (200) × 64 ≈ 12,800` real
    /// invocations past warmup before the manager recommended its first C1
    /// compile (observed live: `tiered-enqueue … invoc_count=13236` for a
    /// `CRATONVM_JIT_THRESHOLD=500` run). Passing the interpreter's real
    /// per-method count lets the recommendation fire at the intended
    /// thresholds. `observed_count == 0` (or a stale/smaller value) degrades
    /// to the historical `+= 1` behaviour.
    pub fn on_method_invocation_observed(
        &self,
        key: &MethodKey,
        observed_count: u64,
    ) -> Option<CompilationTier> {
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.invocation_count += 1;
        state.profile.profiled_invocations += 1;
        if observed_count > state.invocation_count {
            state.invocation_count = observed_count;
        }

        if state.queued_for_compilation {
            return None;
        }

        let policy = self.policy.lock();
        if !policy.tiered_enabled {
            return None;
        }

        self.should_compile_inner(state, &policy)
    }

    /// Called on each back-edge (loop iteration) from the interpreter.
    /// May trigger OSR compilation.
    pub fn on_backedge(&self, key: &MethodKey, bci: u32) -> Option<CompilationTask> {
        if is_osr_denied(key) {
            return None;
        }
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.backedge_count += 1;

        let policy = self.policy.lock();
        if !policy.tiered_enabled {
            return None;
        }

        if state.backedge_count >= policy.osr_threshold as u64
            && !state.queued_for_compilation
            && state.current_tier < CompilationTier::C2
            && !state.c2_bailout
        {
            let target_tier = if state.c2_bailout {
                CompilationTier::C1
            } else {
                CompilationTier::C2
            };

            state.queued_for_compilation = true;
            state.queued_tier = Some(target_tier);

            let task = CompilationTask {
                method_key: key.clone(),
                target_tier,
                priority: CompilationPriority::High,
                enqueue_time_ms: 0,
                osr_bci: Some(bci),
            };

            self.core.enqueue(task.clone());
            self.core
                .stats
                .osr_compilations
                .fetch_add(1, Ordering::Relaxed);
            return Some(task);
        }
        None
    }

    /// wire-tiered-manager Step 5 (precise background OSR): request an OSR
    /// compilation for a method whose loop the *caller* has already judged hot.
    ///
    /// Unlike [`on_backedge`], which counts every back-edge and only enqueues
    /// once its `osr_threshold` is crossed, this enqueues **immediately** — the
    /// interpreter's per-frame back-edge schedule (`Frame::should_try_osr`) is
    /// the throttle, so calling `on_backedge` per iteration just to reach the
    /// count threshold would both pay a lock per back-edge and double-count.
    /// It is idempotent: a no-op (returns `None`) if the method is already
    /// queued or has bailed out of C2. Method-entry C2 does not suppress this
    /// request because OSR bodies live in an independent cache. The enqueued task
    /// carries `osr_bci` so the background worker compiles an OSR-enterable
    /// artifact; the mutator enters it once published. (Threshold tuning of
    /// when a loop counts as "hot enough" is Step 6.)
    pub fn request_osr(&self, key: &MethodKey, bci: u32) -> Option<CompilationTask> {
        if is_osr_denied(key) {
            return None;
        }
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.backedge_count = state.backedge_count.saturating_add(1);

        let policy = self.policy.lock();
        if !policy.tiered_enabled {
            return None;
        }
        if state.queued_for_compilation
            || state.c2_bailout
            // Same "give up after repeated failures" convention as
            // `should_compile`/`request_c2_upgrade`: without this, a method
            // whose OSR artifact compile keeps returning `published=false`
            // (e.g. an uninlinable callee) has `current_tier` permanently
            // stuck below C2 and `queued_for_compilation` cleared by
            // `complete_task` after each failure — so the very next hot
            // back-edge re-enqueues an OSR task, forever, with no
            // diagnostic. This is the OSR-request twin of the plain
            // background-compile bail-listing gap (see the `try_compile`
            // ldc/ldc2_w fix): observed as a silent hang where the same
            // method (`TestResponsePerformance.doHomebrew`) kept getting
            // "bg-compile ... osr_bci=..." re-attempted every stride while
            // never completing even one 1M-iteration measurement pass.
            || state.ineligible
            || state.tier_fail_count >= MAX_TIER_FAIL_RETRIES
        {
            return None;
        }

        let target_tier = CompilationTier::C2;
        state.queued_for_compilation = true;
        state.queued_tier = Some(target_tier);
        let task = CompilationTask {
            method_key: key.clone(),
            target_tier,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: Some(bci),
        };
        self.core.enqueue(task.clone());
        self.core
            .stats
            .osr_compilations
            .fetch_add(1, Ordering::Relaxed);
        Some(task)
    }

    // ── Profile recording ────────────────────────────────────────────────

    /// Record a branch outcome for profiling.
    pub fn record_branch(&self, key: &MethodKey, bci: u32, taken: bool) {
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        let bp = state.profile.branch_counts.entry(bci).or_default();
        if taken {
            bp.taken += 1;
        } else {
            bp.not_taken += 1;
        }
    }

    /// Record a receiver type at a call site.
    pub fn record_receiver(&self, key: &MethodKey, bci: u32, class_id: u32) {
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        let rp = state.profile.receiver_profiles.entry(bci).or_default();
        rp.total_calls += 1;

        // Update or insert the class_id, keeping at most MAX_RECEIVER_TYPES.
        if let Some(entry) = rp.receivers.iter_mut().find(|(id, _)| *id == class_id) {
            entry.1 += 1;
        } else if rp.receivers.len() < MAX_RECEIVER_TYPES {
            rp.receivers.push((class_id, 1));
        } else {
            // Replace the least-frequent entry if the new class_id is hotter.
            if let Some(min_entry) = rp.receivers.iter_mut().min_by_key(|(_, c)| *c) {
                if min_entry.1 < 1 {
                    *min_entry = (class_id, 1);
                }
            }
        }
    }

    /// Record a type check result (checkcast/instanceof).
    pub fn record_type_check(&self, key: &MethodKey, bci: u32, class_id: u32) {
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        let tp = state.profile.type_profiles.entry(bci).or_default();
        tp.total_checks += 1;

        if let Some(entry) = tp.types.iter_mut().find(|(id, _)| *id == class_id) {
            entry.1 += 1;
        } else {
            tp.types.push((class_id, 1));
        }
    }

    /// Record a null check result.
    pub fn record_null_check(&self, key: &MethodKey, bci: u32, was_null: bool) {
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        let np = state.profile.null_profiles.entry(bci).or_default();
        if was_null {
            np.null_seen += 1;
        } else {
            np.non_null_seen += 1;
        }
    }

    // ── Compilation queue ────────────────────────────────────────────────

    /// Enqueue a compilation task.
    pub fn enqueue_compilation(&self, task: CompilationTask) {
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(task.method_key.clone())
            .or_insert_with(|| MethodState::new(task.method_key.clone()));
        state.queued_for_compilation = true;
        state.queued_tier = Some(task.target_tier);
        drop(methods);
        self.core.enqueue(task);
    }

    /// Dequeue the next compilation task (highest priority first).
    pub fn dequeue_compilation(&self) -> Option<CompilationTask> {
        self.core.dequeue()
    }

    /// Notify that compilation completed successfully at `tier`. A thin
    /// synchronous wrapper over [`CompilerCore::complete_task`] (always
    /// reports `success = true` — this API has no failure-reporting caller
    /// today; production code goes through the background worker's
    /// `compiler_loop`, which threads a real success/failure bool through).
    pub fn compilation_complete(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        compile_time_ms: u64,
    ) {
        self.core
            .complete_task(key, tier, compile_time_ms, true, false, false);
    }

    // ── Deoptimization ───────────────────────────────────────────────────

    /// Notify that deoptimization occurred for the given method.
    pub fn on_deoptimization(&self, key: &MethodKey) {
        let mut methods = self.core.methods.lock();
        if let Some(state) = methods.get_mut(key) {
            state.deopt_count += 1;
            state.current_tier = CompilationTier::Interpreter;
            state.queued_for_compilation = false;
            state.queued_tier = None;

            if state.deopt_count >= MAX_DEOPTS_BEFORE_BAILOUT {
                state.c2_bailout = true;
                self.core.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.core
            .stats
            .deoptimizations
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Notify that C2 compilation bailed out (method too complex, etc.).
    pub fn on_c2_bailout(&self, key: &MethodKey) {
        let mut methods = self.core.methods.lock();
        if let Some(state) = methods.get_mut(key) {
            state.c2_bailout = true;
            state.queued_for_compilation = false;
            state.queued_tier = None;
        }
        self.core.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// Get the current tier for a method.
    pub fn current_tier(&self, key: &MethodKey) -> CompilationTier {
        self.core
            .methods
            .lock()
            .get(key)
            .map(|s| s.current_tier)
            .unwrap_or(CompilationTier::Interpreter)
    }

    /// Get a clone of the profile data for a method.
    pub fn get_profile(&self, key: &MethodKey) -> Option<MethodProfile> {
        self.core.methods.lock().get(key).map(|s| s.profile.clone())
    }

    /// Get compilation statistics.
    pub fn stats(&self) -> &CompilationStats {
        &self.core.stats
    }

    /// Get a snapshot of the compilation policy.
    pub fn policy(&self) -> CompilationPolicy {
        let p = self.policy.lock();
        CompilationPolicy {
            c1_threshold: p.c1_threshold,
            c2_threshold: p.c2_threshold,
            osr_threshold: p.osr_threshold,
            tiered_enabled: p.tiered_enabled,
            c2_min_invocations: p.c2_min_invocations,
            c1_profiling: p.c1_profiling,
        }
    }

    /// Update the compilation policy.
    pub fn set_policy(&self, policy: CompilationPolicy) {
        *self.policy.lock() = policy;
    }

    /// Check if the compilation queue is empty.
    pub fn queue_empty(&self) -> bool {
        self.core.queue.lock().is_empty()
    }

    /// Get the number of tasks in the compilation queue.
    pub fn queue_size(&self) -> usize {
        self.core.queue.lock().len()
    }

    /// Get all method states: (key, current_tier, invocation_count).
    pub fn method_states(&self) -> Vec<(MethodKey, CompilationTier, u64)> {
        self.core
            .methods
            .lock()
            .values()
            .map(|s| (s.method_key.clone(), s.current_tier, s.invocation_count))
            .collect()
    }

    /// Whether the background compiler is active.
    pub fn compiler_active(&self) -> bool {
        self.core.active.load(Ordering::Relaxed)
    }

    /// Set whether the background compiler is active.
    ///
    /// Retained for compatibility / tests that only assert the flag. The real
    /// worker is started via [`start_background_compiler`] and flips this flag
    /// itself.
    pub fn set_compiler_active(&self, active: bool) {
        self.core.active.store(active, Ordering::Relaxed);
    }

    /// Number of tasks the background worker has finished compiling.
    pub fn completed_compilations(&self) -> u64 {
        self.core.completed.load(Ordering::Relaxed)
    }

    /// Spawn the background compile thread (idempotent).
    ///
    /// The worker loops: block on the queue's condvar until a task is available
    /// (or shutdown is requested), drain the highest-priority task, run
    /// `compile_fn` for it **off the mutator thread**, then record completion on
    /// the shared core. The returned [`BackgroundCompiler`] owns the join handle;
    /// dropping it (e.g. at VM teardown) signals shutdown and joins, so no
    /// compile thread outlives the VM.
    ///
    /// Takes `&self` (not `Arc<Self>`): the worker only needs the
    /// `Arc<CompilerCore>`, so this works even though `SharedVm` owns the
    /// manager by value.
    ///
    /// Returns `None` if a worker is already active.
    pub fn start_background_compiler(&self, compile_fn: CompileFn) -> Option<BackgroundCompiler> {
        // Atomically claim the single-worker slot.
        if self
            .core
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        self.core.shutdown.store(false, Ordering::Release);

        let core = Arc::clone(&self.core);
        // The default Rust thread stack (~2 MiB) has no margin for compiling
        // methods with deep IR (heavy inlining, long expression chains from
        // generated/framework code such as Quarkus/JUnit5 test-framework
        // classes) — unlike the main-vm interpreter thread, which was bumped
        // to 128 MiB after binaryTrees(18)-style recursion overflowed 64 MiB
        // (see vm-cli/src/main.rs). Give the compiler thread the same
        // 16 MiB headroom already used for other native-recursion-heavy
        // worker threads (see libcratonvm's foreign-attach threads); the
        // cost is virtual-address-space only (no commit until touched).
        let handle = std::thread::Builder::new()
            .name("cratonvm-jit-compiler".to_string())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                Self::compiler_loop(&core, compile_fn);
            })
            .ok();

        match handle {
            Some(handle) => Some(BackgroundCompiler {
                core: Arc::clone(&self.core),
                handle: Some(handle),
            }),
            None => {
                // Spawn failed — release the slot so a retry can succeed.
                self.core.active.store(false, Ordering::Release);
                None
            }
        }
    }

    /// Body of the background compile thread.
    ///
    /// ## GC-STW-safety invariant (wire-tiered-manager increment 3)
    ///
    /// This loop runs on the unregistered, GC-neutral `cratonvm-jit-compiler`
    /// daemon (see [`start_background_compiler`]). The queue wait MUST block on
    /// `CompilerCore`'s OWN [`Condvar`] (`core.wake`) while holding ONLY this
    /// crate's `core.queue` mutex — never any VM lock. `tiered.rs` is in the
    /// `cratonvm-jit` crate and cannot even name `SharedVm`'s locks, so the wait
    /// here is structurally VM-lock-free: `core.queue`/`core.wake`/`core.methods`
    /// are jit-crate-private. `parking_lot::Condvar::wait` releases `q` while the
    /// worker is parked and re-acquires it on wake.
    ///
    /// The VM-side codegen + publish runs in `compile_fn` with NO lock of any
    /// kind held by this frame (`q` is dropped at the end of the inner scope
    /// before `compile_fn` is called). The VM closure
    /// ([`crate::...background_compile_task`]) is responsible for bounding its
    /// own VM-lock scopes; this loop guarantees it is entered lock-free. The net
    /// effect: while the worker idles on `core.wake`, it holds no lock a mutator
    /// could need, so a mutator never stalls behind it and a concurrent STW
    /// completes promptly.
    fn compiler_loop(core: &Arc<CompilerCore>, compile_fn: CompileFn) {
        loop {
            // Pop one task while holding ONLY the jit-crate queue lock; block on
            // the core's own condvar when empty so the worker idles instead of
            // spinning. No VM lock is — or can be — held across this wait.
            let task = {
                let mut q = core.queue.lock();
                loop {
                    if core.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(task) = q.dequeue() {
                        break task;
                    }
                    // `parking_lot::Condvar::wait` releases `q` while parked and
                    // re-acquires on wake; spurious wakeups re-check the loop.
                    core.wake.wait(&mut q);
                }
            };

            // Compile off the mutator thread with NO lock held by this frame
            // (`q` was dropped above), then publish completion. `compile_fn`
            // bounds its own VM-lock scopes internally.
            let outcome = compile_fn(&task);
            core.complete_task(
                &task.method_key,
                task.target_tier,
                outcome.compile_time_ms,
                outcome.published,
                task.osr_bci.is_some(),
                outcome.declined_permanently,
            );
            // C1→C2 supersede: a freshly-published C1-family body whose
            // method the VM judged IR-eligible gets a Low-priority C2
            // recompile. Enqueued AFTER complete_task so the C1 task's
            // `queued_for_compilation` flag has been cleared (otherwise the
            // idempotence gate would drop the upgrade). OSR tasks are
            // excluded (their artifacts serve loop entry; the invocation
            // path re-tiers separately), as are tasks already at an
            // optimized tier.
            if outcome.published
                && outcome.c2_upgrade_candidate
                && task.osr_bci.is_none()
                && !tier_uses_optimized_backend(task.target_tier)
            {
                core.request_c2_upgrade(&task.method_key);
            }
        }
    }

    // ── Internal ─────────────────────────────────────────────────────────

    /// Core compilation-decision logic. Must be called while `methods` is locked.
    fn should_compile_inner(
        &self,
        state: &mut MethodState,
        policy: &CompilationPolicy,
    ) -> Option<CompilationTier> {
        let target = self.should_compile(state, policy)?;

        state.queued_for_compilation = true;
        state.queued_tier = Some(target);

        let priority = match target {
            CompilationTier::C2 => CompilationPriority::High,
            CompilationTier::C1 | CompilationTier::C1WithProfiling => CompilationPriority::Normal,
            _ => CompilationPriority::Low,
        };

        let task = CompilationTask {
            method_key: state.method_key.clone(),
            target_tier: target,
            priority,
            enqueue_time_ms: 0,
            osr_bci: None,
        };

        // `self.core.enqueue` takes the queue lock (distinct from `methods`,
        // which the caller still holds) and wakes the background worker.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_TIER_ENQUEUE").is_some() {
            eprintln!(
                "[cratonvm-tier] enqueue {}.{}{} tier={:?} invocations={} elapsed_ms={}",
                state.method_key.class_name,
                state.method_key.method_name,
                state.method_key.descriptor,
                target,
                state.invocation_count,
                process_uptime_ms(),
            );
        }
        self.core.enqueue(task);
        Some(target)
    }

    /// Pure policy check: determine if the method should be compiled (and at what tier).
    fn should_compile(
        &self,
        state: &MethodState,
        policy: &CompilationPolicy,
    ) -> Option<CompilationTier> {
        // Keep the background compiler from queueing the known-corrupting
        // BigInteger implementation. `try_compile` carries the same final
        // guard for direct/manual queue paths that bypass this policy method.
        // Give up after repeated compile-attempt failures (the attempt ran
        // but never published a body — see `complete_task`), matching the
        // "3+ deopts" convention `c2_bailout` already uses below. Without
        // this, a method whose compile step keeps failing for a reason
        // outside the (fast) permanent bail-list — e.g. a transient
        // code-cache-cap or in-flight class redefine — would be
        // re-recommended and re-enqueued on every single invocation
        // forever, since a failed attempt no longer advances `current_tier`.
        // A policy decline is permanent by construction, so one is enough —
        // unlike `tier_fail_count`, which deliberately allows retries.
        if state.ineligible {
            return None;
        }
        if state.tier_fail_count >= MAX_TIER_FAIL_RETRIES {
            return None;
        }
        match state.current_tier {
            CompilationTier::Interpreter => {
                // Can we skip straight to C2?
                if !state.c2_bailout
                    && state.invocation_count >= policy.c2_threshold as u64
                    && state.invocation_count >= policy.c2_min_invocations as u64
                    && state.profile.profiled_invocations > 0
                {
                    return Some(CompilationTier::C2);
                }
                // Otherwise go to C1
                if state.invocation_count >= policy.c1_threshold as u64 {
                    return Some(CompilationTier::C1);
                }
                None
            }
            CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                if state.c2_bailout {
                    return None;
                }
                if state.invocation_count >= policy.c2_threshold as u64
                    && state.invocation_count >= policy.c2_min_invocations as u64
                {
                    return Some(CompilationTier::C2);
                }
                None
            }
            // Already at C2 or FullProfile — nothing to do.
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// CompilationBroker — compilation POLICY, separated from the compiler
// ═══════════════════════════════════════════════════════════════════════════════
//
// C2 review P1, "Separate compilation policy from compiler implementation":
// *"Policy can be tested without emitting code and backend failures return
// structured reasons."* Backlog step 22 adds: *"Tier decisions are
// deterministic, observable, and independently testable."*
//
// ## What was wrong
//
// The decisions that make up compilation policy are currently spread across
// three places that cannot be exercised without a backend:
//
//   * `TieredCompilationManager::should_compile` (this file) owns tier
//     selection, but every path into it goes through a `Mutex<FxHashMap>` and
//     a process-global background worker.
//   * `jit::try_compile_with_invokespecial_resolver` (`lib.rs`) owns the
//     *final* admission gate — the deny lists, `CRATONVM_JIT_DENY`, and the
//     code-cache cap — interleaved with the constant-pool resolvers and the
//     backend call itself. Asking "would this method be admitted?" means
//     running a compile.
//   * `try_compile_inner` (`lib.rs`) owns the optimizing-tier admission
//     conjunction, which is evaluated *after* the bytecode scan has already
//     run.
//
// Nothing there is reachable from a unit test with no `CachedBytecodeMethod`,
// no resolvers, and no executable memory. That is the defect this section
// fixes: the *decisions* move to a value-in / decision-out object that has no
// idea a compiler exists.
//
// ## What this is not
//
// It is **not** a retune. Every threshold, retry limit and priority mapping
// below is copied from the code above it, and
// `threshold_policy_reproduces_todays_tier_decisions` proves the copy is exact
// by running a table of synthetic counter values through BOTH
// [`ThresholdPolicy`] and the live [`TieredCompilationManager::should_compile`]
// and asserting the answers agree. A policy refactor that also moved a
// threshold would be untestable, because there would be no oracle left.
//
// It is also **not yet wired in**. `TieredCompilationManager` and
// `try_compile` are untouched; the broker is additive. The extraction plan —
// which `lib.rs` line becomes which broker call — is
// `docs/jit/compilation-broker.md`.
//
// ## Vocabularies it reuses rather than reinvents
//
//   * Backend failure = [`crate::bailout::Bailout`]. The broker consumes it,
//     keys its per-category tally on [`crate::bailout::Bailout::category`], and
//     defines no failure enum of its own.
//   * Observability = [`crate::metrics`]. [`AdmissionDecision`]'s `Display` is
//     the string `metrics::CompileRecorder::set_admission` already stores in
//     `CompilationReport::admission`, so the broker's verdict and the
//     per-compilation record say the same thing.
//   * Requests = [`CompilationTask`], the type the existing queue and the
//     VM-side `CompileFn` already speak.

/// Default depth of a [`BoundedCompileQueue`].
///
/// The in-tree [`CompilationQueue`] is unbounded, which is safe only because
/// `should_compile` refuses to re-enqueue a method that is already queued. A
/// broker that also accepts externally-originated requests (OSR, C1→C2
/// upgrades, VM-driven recompiles) needs a real bound, and a bound needs a
/// shed rule — see [`BoundedCompileQueue::enqueue`]. 512 is far above any
/// steady-state depth observed on the app gauntlet, so it behaves as
/// "unbounded" until something goes wrong, which is the point.
pub const DEFAULT_COMPILE_QUEUE_CAPACITY: usize = 512;

impl CompilationPriority {
    /// Ordering key: `High` outranks `Normal` outranks `Low`.
    ///
    /// Deliberately a method rather than a `PartialOrd` derive — the variant
    /// declaration order is `High, Normal, Low`, so a derive would rank them
    /// backwards, and a silently inverted shed rule is exactly the bug this
    /// avoids.
    pub fn rank(self) -> u8 {
        match self {
            CompilationPriority::Low => 0,
            CompilationPriority::Normal => 1,
            CompilationPriority::High => 2,
        }
    }

    /// Every priority, lowest rank first — the order [`BoundedCompileQueue`]
    /// searches when it needs something to shed.
    pub const BY_RANK: [CompilationPriority; 3] = [
        CompilationPriority::Low,
        CompilationPriority::Normal,
        CompilationPriority::High,
    ];
}

impl CompilationTask {
    /// Whether this request is for an OSR (loop-entry) artifact rather than a
    /// method-entry body. The two publish to different caches — see
    /// [`CompilerCore::complete_task`]'s `osr` parameter — so nothing that
    /// consumes a task may treat them alike.
    pub fn is_osr(&self) -> bool {
        self.osr_bci.is_some()
    }
}

// ── Signals ──────────────────────────────────────────────────────────

/// Everything a [`TierPolicy`] is allowed to look at, as a plain value.
///
/// This is the whole testability argument in one type: a policy that reads
/// only a `TierSignals` can be driven from a test with synthetic invocation
/// counts, back-edge counts and failure counts, and no VM, no method body and
/// no code buffer anywhere.
///
/// Constructors are split by purity on purpose. [`TierSignals::new`] and
/// [`TierSignals::from_state`] touch no global state, so a test that builds
/// them is hermetic. [`TierSignals::with_process_rules`] is the one place that
/// consults the process-wide deny sets, and it is opt-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSignals {
    /// Which method these signals describe.
    pub key: MethodKey,
    /// Tier whose body is currently published for this method.
    pub current_tier: CompilationTier,
    /// Method-entry invocations observed.
    pub invocation_count: u64,
    /// Loop back-edges observed.
    pub backedge_count: u64,
    /// Invocations that carried profile collection — the C2-from-interpreter
    /// rule requires this to be non-zero.
    pub profiled_invocations: u64,
    /// A request for this method is already in flight.
    pub queued: bool,
    /// Consecutive compile attempts that ran and published nothing.
    pub tier_fail_count: u32,
    /// The method has been demoted out of C2 (3 deopts, or a C2 compile over
    /// [`MAX_C2_COMPILE_TIME_MS`]).
    pub c2_bailout: bool,
    /// The VM declined this method for a reason fixed for the process's life.
    pub ineligible: bool,
    /// Always `false` since 2026-07-31: the static class-level deny sets were
    /// removed along with `vm/src/jit/skip_list.rs`. Retained so tier-policy
    /// call sites and their tests keep their shape; `CRATONVM_JIT_DENY` is the
    /// remaining force-interpret lever and is applied in `try_compile`.
    pub class_denied: bool,
    /// OSR is permanently disabled for this method (see [`is_osr_denied`]).
    pub osr_denied: bool,
}

impl TierSignals {
    /// A cold method: no counts, no flags, no deny. Pure — reads nothing
    /// outside its argument.
    pub fn new(key: MethodKey) -> Self {
        TierSignals {
            key,
            current_tier: CompilationTier::Interpreter,
            invocation_count: 0,
            backedge_count: 0,
            profiled_invocations: 0,
            queued: false,
            tier_fail_count: 0,
            c2_bailout: false,
            ineligible: false,
            class_denied: false,
            osr_denied: false,
        }
    }

    /// Project a live [`MethodState`] onto its signals. Pure: the two deny
    /// flags stay `false` — apply [`Self::with_process_rules`] to fill them.
    pub fn from_state(state: &MethodState) -> Self {
        TierSignals {
            key: state.method_key.clone(),
            current_tier: state.current_tier,
            invocation_count: state.invocation_count,
            backedge_count: state.backedge_count,
            profiled_invocations: state.profile.profiled_invocations,
            queued: state.queued_for_compilation,
            tier_fail_count: state.tier_fail_count,
            c2_bailout: state.c2_bailout,
            ineligible: state.ineligible,
            class_denied: false,
            osr_denied: false,
        }
    }

    /// Fill `class_denied` / `osr_denied` from the process-wide deny sets.
    ///
    /// The only impure step in building signals, kept separate so a policy
    /// test never has to reason about what some other test wrote into
    /// `osr_deny_list()`.
    pub fn with_process_rules(mut self) -> Self {
        self.class_denied = false;
        self.osr_denied = is_osr_denied(&self.key);
        self
    }
}

/// What made a caller ask for an OSR artifact.
///
/// The two live OSR entry points apply *different* rules, and collapsing them
/// would change behaviour: [`TieredCompilationManager::on_backedge`] counts
/// back-edges and fires at `osr_threshold`, while
/// [`TieredCompilationManager::request_osr`] enqueues immediately because the
/// interpreter's per-frame `Frame::should_try_osr` schedule is the throttle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsrTrigger {
    /// The broker's own back-edge counter crossed `osr_threshold`
    /// (mirrors [`TieredCompilationManager::on_backedge`]).
    BackedgeCounter,
    /// The caller already judged the loop hot; no counter gate applies
    /// (mirrors [`TieredCompilationManager::request_osr`]).
    CallerJudged,
}

// ── Decisions ────────────────────────────────────────────────────────

/// Why a request was admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitReason {
    /// The invocation counter crossed the tier's threshold.
    InvocationThreshold { invocations: u64, threshold: u32 },
    /// The back-edge counter crossed `osr_threshold`.
    OsrBackedgeThreshold { backedges: u64, threshold: u32 },
    /// The caller judged the loop hot and asked directly.
    OsrCallerJudged { backedges: u64 },
    /// A C1-family body just published and the VM judged C2 worthwhile.
    C2Upgrade,
}

impl AdmitReason {
    /// Short stable category name, for metrics keys and log greps. Same
    /// contract as [`crate::bailout::BailoutReason::category`]: these strings
    /// are external, so they must not be renamed with the variants.
    pub fn category(&self) -> &'static str {
        match self {
            AdmitReason::InvocationThreshold { .. } => "invocation_threshold",
            AdmitReason::OsrBackedgeThreshold { .. } => "osr_backedge_threshold",
            AdmitReason::OsrCallerJudged { .. } => "osr_caller_judged",
            AdmitReason::C2Upgrade => "c2_upgrade",
        }
    }
}

impl std::fmt::Display for AdmitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmitReason::InvocationThreshold {
                invocations,
                threshold,
            } => write!(f, "{invocations} invocations >= threshold {threshold}"),
            AdmitReason::OsrBackedgeThreshold {
                backedges,
                threshold,
            } => write!(f, "{backedges} back-edges >= osr_threshold {threshold}"),
            AdmitReason::OsrCallerJudged { backedges } => {
                write!(f, "caller-judged hot loop at {backedges} back-edges")
            }
            AdmitReason::C2Upgrade => write!(f, "C1 body published; C2 upgrade requested"),
        }
    }
}

/// Why a request was declined.
///
/// Every variant carries the operands that produced the verdict, so a
/// diagnostic can print an actionable line without re-deriving the decision —
/// the same rule [`crate::bailout::BailoutReason`] follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclineReason {
    /// `CompilationPolicy::tiered_enabled` is off.
    TieringDisabled,
    /// A request for this method is already in flight.
    AlreadyQueued,
    /// A static class-level deny applies.
    ClassDenied,
    /// OSR is permanently disabled for this method.
    OsrDenied,
    /// The VM declined this method permanently (skip list, OSR denial).
    PermanentlyIneligible,
    /// Consecutive failed compile attempts saturated the retry budget.
    RetriesExhausted { failures: u32, limit: u32 },
    /// The method has been demoted out of C2 and C1 is already published.
    C2BailedOut,
    /// Nothing left to promote to.
    AlreadyAtTier { tier: CompilationTier },
    /// A C1→C2 supersede was requested for a method with no compiled body to
    /// supersede. [`CompilerCore::request_c2_upgrade`] expresses the same
    /// precondition as an early return on a missing `MethodState`.
    NoPublishedBody,
    /// The invocation counter has not reached the next tier's threshold.
    BelowInvocationThreshold { invocations: u64, threshold: u32 },
    /// The back-edge counter has not reached `osr_threshold`.
    BelowBackedgeThreshold { backedges: u64, threshold: u32 },
    /// Live code-cache occupancy is at the configured cap.
    CodeCacheFull { used_bytes: u64, cap_bytes: u64 },
    /// The queue is full and nothing in it ranks below this request.
    QueueFull { depth: usize, capacity: usize },
}

impl DeclineReason {
    /// Short stable category name — see [`AdmitReason::category`].
    pub fn category(&self) -> &'static str {
        match self {
            DeclineReason::TieringDisabled => "tiering_disabled",
            DeclineReason::AlreadyQueued => "already_queued",
            DeclineReason::ClassDenied => "class_denied",
            DeclineReason::OsrDenied => "osr_denied",
            DeclineReason::PermanentlyIneligible => "permanently_ineligible",
            DeclineReason::RetriesExhausted { .. } => "retries_exhausted",
            DeclineReason::C2BailedOut => "c2_bailed_out",
            DeclineReason::AlreadyAtTier { .. } => "already_at_tier",
            DeclineReason::NoPublishedBody => "no_published_body",
            DeclineReason::BelowInvocationThreshold { .. } => "below_invocation_threshold",
            DeclineReason::BelowBackedgeThreshold { .. } => "below_backedge_threshold",
            DeclineReason::CodeCacheFull { .. } => "code_cache_full",
            DeclineReason::QueueFull { .. } => "queue_full",
        }
    }

    /// Whether re-asking with a larger counter could ever produce a different
    /// answer. `false` means the method is done for the life of the process,
    /// which is what `MethodState::ineligible` exists to record once instead
    /// of rediscovering three times.
    pub fn is_transient(&self) -> bool {
        match self {
            DeclineReason::ClassDenied
            | DeclineReason::OsrDenied
            | DeclineReason::PermanentlyIneligible
            | DeclineReason::RetriesExhausted { .. } => false,
            DeclineReason::TieringDisabled
            | DeclineReason::AlreadyQueued
            | DeclineReason::C2BailedOut
            | DeclineReason::AlreadyAtTier { .. }
            | DeclineReason::NoPublishedBody
            | DeclineReason::BelowInvocationThreshold { .. }
            | DeclineReason::BelowBackedgeThreshold { .. }
            | DeclineReason::CodeCacheFull { .. }
            | DeclineReason::QueueFull { .. } => true,
        }
    }
}

impl std::fmt::Display for DeclineReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeclineReason::TieringDisabled => write!(f, "tiered compilation is disabled"),
            DeclineReason::AlreadyQueued => write!(f, "a request is already in flight"),
            DeclineReason::ClassDenied => write!(f, "the class is on a static JIT deny list"),
            DeclineReason::OsrDenied => write!(f, "OSR is permanently denied for this method"),
            DeclineReason::PermanentlyIneligible => {
                write!(f, "the VM declined this method permanently")
            }
            DeclineReason::RetriesExhausted { failures, limit } => {
                write!(f, "{failures} failed compile attempts >= limit {limit}")
            }
            DeclineReason::C2BailedOut => write!(f, "the method has bailed out of C2"),
            DeclineReason::AlreadyAtTier { tier } => write!(f, "already compiled at {tier:?}"),
            DeclineReason::NoPublishedBody => write!(f, "no compiled body to supersede"),
            DeclineReason::BelowInvocationThreshold {
                invocations,
                threshold,
            } => write!(f, "{invocations} invocations < threshold {threshold}"),
            DeclineReason::BelowBackedgeThreshold {
                backedges,
                threshold,
            } => write!(f, "{backedges} back-edges < osr_threshold {threshold}"),
            DeclineReason::CodeCacheFull {
                used_bytes,
                cap_bytes,
            } => write!(f, "code cache full: {used_bytes} bytes >= cap {cap_bytes}"),
            DeclineReason::QueueFull { depth, capacity } => {
                write!(f, "compile queue full: depth {depth} of {capacity}")
            }
        }
    }
}

/// An admitted request: what to compile, how urgently, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitTicket {
    /// Tier the backend is being asked for.
    pub tier: CompilationTier,
    /// Queue band.
    pub priority: CompilationPriority,
    /// `Some(bci)` for an OSR artifact, `None` for a method-entry body.
    pub osr_bci: Option<u32>,
    /// Why this was admitted.
    pub reason: AdmitReason,
}

/// The broker's answer to "should this be compiled, and why (not)?".
///
/// Both arms carry their reason. That is the acceptance criterion for the
/// admission half of P1: today a decline is `None` from `should_compile` or a
/// bare `return None` in `try_compile`, and the caller cannot tell "still
/// warming up" from "banned forever".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// Compile it.
    Admit(AdmitTicket),
    /// Do not compile it now.
    Decline(DeclineReason),
}

impl AdmissionDecision {
    /// The admitted tier, or `None` — the shape
    /// [`TieredCompilationManager::should_compile`] returns today, so the two
    /// can be compared directly.
    pub fn admitted_tier(&self) -> Option<CompilationTier> {
        match self {
            AdmissionDecision::Admit(t) => Some(t.tier),
            AdmissionDecision::Decline(_) => None,
        }
    }

    /// Whether this decision admits anything.
    pub fn is_admit(&self) -> bool {
        matches!(self, AdmissionDecision::Admit(_))
    }

    /// Whether this decision admits an OSR artifact specifically.
    pub fn is_osr(&self) -> bool {
        matches!(self, AdmissionDecision::Admit(t) if t.osr_bci.is_some())
    }

    /// Short stable category name of whichever reason applies.
    pub fn category(&self) -> &'static str {
        match self {
            AdmissionDecision::Admit(t) => t.reason.category(),
            AdmissionDecision::Decline(r) => r.category(),
        }
    }
}

impl std::fmt::Display for AdmissionDecision {
    /// The string `metrics::CompileRecorder::set_admission` stores in
    /// [`crate::metrics::CompilationReport::admission`]. Deliberately shaped
    /// like the verdicts `try_compile_inner` already builds
    /// ("admitted to the optimizing pipeline", "optimize=false — …") so the
    /// broker's decision and the per-compilation record read as one vocabulary.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionDecision::Admit(t) => {
                write!(f, "admitted to {:?} [{}]: {}", t.tier, t.reason.category(), t.reason)?;
                if let Some(bci) = t.osr_bci {
                    write!(f, " (OSR at bci {bci})")?;
                }
                Ok(())
            }
            AdmissionDecision::Decline(r) => {
                write!(f, "declined [{}]: {r}", r.category())
            }
        }
    }
}

// ── TierPolicy ───────────────────────────────────────────────────────

/// The tier-selection rules, as an interface.
///
/// Implementations must be pure functions of their arguments: given the same
/// [`TierSignals`] they must return the same [`AdmissionDecision`], with no
/// clock, no environment read and no global state. `decisions_are_deterministic_given_identical_inputs`
/// holds the in-tree implementation to that.
pub trait TierPolicy: Send + Sync {
    /// Name for diagnostics.
    fn name(&self) -> &'static str;

    /// Method-entry tier selection. The extraction target for
    /// [`TieredCompilationManager::should_compile`].
    fn select_tier(&self, signals: &TierSignals) -> AdmissionDecision;

    /// OSR (loop-entry) selection. See [`OsrTrigger`] for why the trigger is
    /// part of the question.
    fn select_osr(
        &self,
        signals: &TierSignals,
        bci: u32,
        trigger: OsrTrigger,
    ) -> AdmissionDecision;

    /// C1→C2 supersede after a C1-family body publishes.
    ///
    /// Provided, because the rule reads only state flags — no threshold — so
    /// every policy can share it. Mirrors [`CompilerCore::request_c2_upgrade`],
    /// including its `Low` priority: an upgrade must never displace a method
    /// that is still interpreting.
    fn select_c2_upgrade(&self, signals: &TierSignals) -> AdmissionDecision {
        if signals.queued {
            return AdmissionDecision::Decline(DeclineReason::AlreadyQueued);
        }
        if signals.current_tier >= CompilationTier::C2 {
            return AdmissionDecision::Decline(DeclineReason::AlreadyAtTier {
                tier: signals.current_tier,
            });
        }
        if signals.c2_bailout {
            return AdmissionDecision::Decline(DeclineReason::C2BailedOut);
        }
        if signals.ineligible {
            return AdmissionDecision::Decline(DeclineReason::PermanentlyIneligible);
        }
        if signals.tier_fail_count >= self.max_tier_failures() {
            return AdmissionDecision::Decline(DeclineReason::RetriesExhausted {
                failures: signals.tier_fail_count,
                limit: self.max_tier_failures(),
            });
        }
        AdmissionDecision::Admit(AdmitTicket {
            tier: CompilationTier::C2,
            priority: CompilationPriority::Low,
            osr_bci: None,
            reason: AdmitReason::C2Upgrade,
        })
    }

    /// Queue band for a target tier. Mirrors
    /// [`TieredCompilationManager::should_compile_inner`]'s mapping, plus
    /// "every OSR request is `High`" from `on_backedge` / `request_osr`.
    fn priority_for(&self, tier: CompilationTier, osr: bool) -> CompilationPriority {
        if osr {
            return CompilationPriority::High;
        }
        match tier {
            CompilationTier::C2 => CompilationPriority::High,
            CompilationTier::C1 | CompilationTier::C1WithProfiling => CompilationPriority::Normal,
            _ => CompilationPriority::Low,
        }
    }

    /// Consecutive failed compile attempts tolerated before the method is
    /// abandoned. Defaults to [`MAX_TIER_FAIL_RETRIES`].
    fn max_tier_failures(&self) -> u32 {
        MAX_TIER_FAIL_RETRIES
    }

    /// Wall-clock budget for one C2 compile, in milliseconds. Exceeding it
    /// demotes the method exactly as three deopts would. Defaults to
    /// [`MAX_C2_COMPILE_TIME_MS`].
    fn max_c2_compile_time_ms(&self) -> u64 {
        MAX_C2_COMPILE_TIME_MS
    }
}

/// The tier policy CratonVM ships: HotSpot-style invocation and back-edge
/// counters against the thresholds in [`CompilationPolicy`].
///
/// Every rule below is transcribed from
/// [`TieredCompilationManager::should_compile`], `on_backedge` and
/// `request_osr`. The transcription is checked, not asserted, by
/// `threshold_policy_reproduces_todays_tier_decisions` and
/// `osr_backedge_trigger_matches_the_live_manager`.
pub struct ThresholdPolicy {
    policy: CompilationPolicy,
}

impl ThresholdPolicy {
    /// Wrap a [`CompilationPolicy`].
    pub fn new(policy: CompilationPolicy) -> Self {
        ThresholdPolicy { policy }
    }

    /// The shipped defaults.
    pub fn with_default_thresholds() -> Self {
        ThresholdPolicy::new(CompilationPolicy::default())
    }

    /// The thresholds in force.
    pub fn thresholds(&self) -> &CompilationPolicy {
        &self.policy
    }

    /// The guards `should_compile` applies before it looks at any counter, in
    /// the same order, plus the two the *callers* of `should_compile` apply
    /// (`tiered_enabled`, `queued_for_compilation`).
    fn common_guards(&self, signals: &TierSignals) -> Option<DeclineReason> {
        if !self.policy.tiered_enabled {
            return Some(DeclineReason::TieringDisabled);
        }
        if signals.queued {
            return Some(DeclineReason::AlreadyQueued);
        }
        if signals.class_denied {
            return Some(DeclineReason::ClassDenied);
        }
        if signals.ineligible {
            return Some(DeclineReason::PermanentlyIneligible);
        }
        if signals.tier_fail_count >= MAX_TIER_FAIL_RETRIES {
            return Some(DeclineReason::RetriesExhausted {
                failures: signals.tier_fail_count,
                limit: MAX_TIER_FAIL_RETRIES,
            });
        }
        None
    }
}

impl TierPolicy for ThresholdPolicy {
    fn name(&self) -> &'static str {
        "threshold"
    }

    fn select_tier(&self, signals: &TierSignals) -> AdmissionDecision {
        if let Some(reason) = self.common_guards(signals) {
            return AdmissionDecision::Decline(reason);
        }
        let p = &self.policy;
        let admit = |tier: CompilationTier, threshold: u32| {
            AdmissionDecision::Admit(AdmitTicket {
                tier,
                priority: self.priority_for(tier, false),
                osr_bci: None,
                reason: AdmitReason::InvocationThreshold {
                    invocations: signals.invocation_count,
                    threshold,
                },
            })
        };
        match signals.current_tier {
            CompilationTier::Interpreter => {
                // Straight to C2 when the method is already far past the C2
                // threshold and has a profile to compile against.
                if !signals.c2_bailout
                    && signals.invocation_count >= p.c2_threshold as u64
                    && signals.invocation_count >= p.c2_min_invocations as u64
                    && signals.profiled_invocations > 0
                {
                    return admit(CompilationTier::C2, p.c2_threshold);
                }
                if signals.invocation_count >= p.c1_threshold as u64 {
                    return admit(CompilationTier::C1, p.c1_threshold);
                }
                AdmissionDecision::Decline(DeclineReason::BelowInvocationThreshold {
                    invocations: signals.invocation_count,
                    threshold: p.c1_threshold,
                })
            }
            CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                if signals.c2_bailout {
                    return AdmissionDecision::Decline(DeclineReason::C2BailedOut);
                }
                if signals.invocation_count >= p.c2_threshold as u64
                    && signals.invocation_count >= p.c2_min_invocations as u64
                {
                    return admit(CompilationTier::C2, p.c2_threshold);
                }
                AdmissionDecision::Decline(DeclineReason::BelowInvocationThreshold {
                    invocations: signals.invocation_count,
                    threshold: p.c2_threshold,
                })
            }
            CompilationTier::FullProfile | CompilationTier::C2 => {
                AdmissionDecision::Decline(DeclineReason::AlreadyAtTier {
                    tier: signals.current_tier,
                })
            }
        }
    }

    fn select_osr(
        &self,
        signals: &TierSignals,
        bci: u32,
        trigger: OsrTrigger,
    ) -> AdmissionDecision {
        if signals.osr_denied {
            return AdmissionDecision::Decline(DeclineReason::OsrDenied);
        }
        if !self.policy.tiered_enabled {
            return AdmissionDecision::Decline(DeclineReason::TieringDisabled);
        }
        let admit = |reason: AdmitReason| {
            AdmissionDecision::Admit(AdmitTicket {
                tier: CompilationTier::C2,
                priority: self.priority_for(CompilationTier::C2, true),
                osr_bci: Some(bci),
                reason,
            })
        };
        match trigger {
            // `on_backedge`: counter gate, and the method must not already be
            // at C2. Note it does NOT consult `ineligible` / `tier_fail_count`
            // — transcribed as-is; making the two triggers agree would be a
            // behaviour change, which this pass does not make.
            OsrTrigger::BackedgeCounter => {
                if signals.queued {
                    return AdmissionDecision::Decline(DeclineReason::AlreadyQueued);
                }
                if signals.c2_bailout {
                    return AdmissionDecision::Decline(DeclineReason::C2BailedOut);
                }
                if signals.current_tier >= CompilationTier::C2 {
                    return AdmissionDecision::Decline(DeclineReason::AlreadyAtTier {
                        tier: signals.current_tier,
                    });
                }
                if signals.backedge_count < self.policy.osr_threshold as u64 {
                    return AdmissionDecision::Decline(DeclineReason::BelowBackedgeThreshold {
                        backedges: signals.backedge_count,
                        threshold: self.policy.osr_threshold,
                    });
                }
                admit(AdmitReason::OsrBackedgeThreshold {
                    backedges: signals.backedge_count,
                    threshold: self.policy.osr_threshold,
                })
            }
            // `request_osr`: no counter gate (the caller is the throttle), but
            // the full failure-budget guard set. Method-entry C2 deliberately
            // does NOT suppress this — OSR artifacts live in their own cache.
            OsrTrigger::CallerJudged => {
                if signals.queued {
                    return AdmissionDecision::Decline(DeclineReason::AlreadyQueued);
                }
                if signals.c2_bailout {
                    return AdmissionDecision::Decline(DeclineReason::C2BailedOut);
                }
                if signals.ineligible {
                    return AdmissionDecision::Decline(DeclineReason::PermanentlyIneligible);
                }
                if signals.tier_fail_count >= MAX_TIER_FAIL_RETRIES {
                    return AdmissionDecision::Decline(DeclineReason::RetriesExhausted {
                        failures: signals.tier_fail_count,
                        limit: MAX_TIER_FAIL_RETRIES,
                    });
                }
                admit(AdmitReason::OsrCallerJudged {
                    backedges: signals.backedge_count,
                })
            }
        }
    }
}

// ── Code-cache pressure ──────────────────────────────────────────────

/// Live code-cache occupancy against its cap — the broker's only view of the
/// resource `jit::try_compile` guards with `jit_code_cache_at_capacity`.
///
/// A value, not a reader: the production wiring feeds
/// [`crate::COMMITTED_JIT_CODE_BYTES`] in through
/// [`CompilationBroker::note_code_cache_used`], and a test feeds a number.
/// That is what makes "pressure changes admission" assertable without mapping
/// a single executable page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeCachePressure {
    /// Bytes of executable code currently committed.
    pub used_bytes: u64,
    /// Configured cap. [`u64::MAX`] means "cap disabled", matching
    /// `CRATONVM_JIT_CODE_CACHE_MAX_MB=0`.
    pub cap_bytes: u64,
}

impl CodeCachePressure {
    /// No cap at all.
    pub const UNBOUNDED: CodeCachePressure = CodeCachePressure {
        used_bytes: 0,
        cap_bytes: u64::MAX,
    };

    /// Occupancy against a cap.
    pub fn new(used_bytes: u64, cap_bytes: u64) -> Self {
        CodeCachePressure {
            used_bytes,
            cap_bytes,
        }
    }

    /// Whether new compilation must be refused. Same predicate as
    /// `jit::jit_code_cache_at_capacity`: at or over the cap, unless disabled.
    pub fn at_capacity(&self) -> bool {
        self.cap_bytes != u64::MAX && self.used_bytes >= self.cap_bytes
    }

    /// Headroom remaining, saturating at zero.
    pub fn free_bytes(&self) -> u64 {
        self.cap_bytes.saturating_sub(self.used_bytes)
    }
}

// ── Bounded queue ────────────────────────────────────────────────────

/// What a [`BoundedCompileQueue::enqueue`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// Queued with room to spare.
    Accepted,
    /// Queued after shedding a strictly-lower-priority request, which is
    /// returned so the caller can clear that method's in-flight flag. A queue
    /// that dropped work silently would leave `queued_for_compilation` set
    /// forever, and the method would never be recommended again.
    AcceptedAfterShedding(CompilationTask),
    /// Not queued: full, and nothing in it ranks below the incoming request.
    Rejected { depth: usize, capacity: usize },
}

/// A three-band priority queue with a hard depth bound and an explicit shed
/// rule.
///
/// The existing [`CompilationQueue`] is the same three bands without the
/// bound; this is its policy-owned counterpart. Both drain highest band
/// first, FIFO within a band.
pub struct BoundedCompileQueue {
    high: VecDeque<CompilationTask>,
    normal: VecDeque<CompilationTask>,
    low: VecDeque<CompilationTask>,
    capacity: usize,
    total_processed: u64,
    total_shed: u64,
}

impl BoundedCompileQueue {
    /// An empty queue of the given depth. A capacity of 0 is clamped to 1 —
    /// a queue that can hold nothing would shed every request and report a
    /// permanently starved compiler.
    pub fn new(capacity: usize) -> Self {
        BoundedCompileQueue {
            high: VecDeque::new(),
            normal: VecDeque::new(),
            low: VecDeque::new(),
            capacity: capacity.max(1),
            total_processed: 0,
            total_shed: 0,
        }
    }

    fn band(&self, priority: CompilationPriority) -> &VecDeque<CompilationTask> {
        match priority {
            CompilationPriority::High => &self.high,
            CompilationPriority::Normal => &self.normal,
            CompilationPriority::Low => &self.low,
        }
    }

    fn band_mut(&mut self, priority: CompilationPriority) -> &mut VecDeque<CompilationTask> {
        match priority {
            CompilationPriority::High => &mut self.high,
            CompilationPriority::Normal => &mut self.normal,
            CompilationPriority::Low => &mut self.low,
        }
    }

    /// The lowest non-empty band that ranks strictly below `priority`, i.e.
    /// the band a request at `priority` may displace.
    fn sheddable_band(&self, priority: CompilationPriority) -> Option<CompilationPriority> {
        CompilationPriority::BY_RANK
            .into_iter()
            .find(|band| band.rank() < priority.rank() && !self.band(*band).is_empty())
    }

    /// Total queued requests across all bands.
    pub fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.low.len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Configured depth bound.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Requests shed to make room, cumulative.
    pub fn total_shed(&self) -> u64 {
        self.total_shed
    }

    /// Requests drained, cumulative.
    pub fn total_processed(&self) -> u64 {
        self.total_processed
    }

    /// Whether a request at `priority` would be rejected right now. Pure —
    /// this is what lets [`CompilationBroker::decide`] answer "would this be
    /// admitted?" without mutating anything.
    pub fn would_reject(&self, priority: CompilationPriority) -> bool {
        self.len() >= self.capacity && self.sheddable_band(priority).is_none()
    }

    /// Queue a request, shedding one strictly-lower-priority request if the
    /// queue is full.
    ///
    /// The shed victim is the **newest** entry of the lowest occupied band
    /// below the incoming request: entries that have already waited keep their
    /// place, so a saturated queue still makes progress in arrival order
    /// instead of starving its oldest work. When no band ranks below the
    /// incoming request, the incoming one is rejected rather than displacing
    /// an equal — otherwise a burst of same-priority requests would churn the
    /// queue without ever compiling anything.
    pub fn enqueue(&mut self, task: CompilationTask) -> EnqueueOutcome {
        if self.len() < self.capacity {
            self.band_mut(task.priority).push_back(task);
            return EnqueueOutcome::Accepted;
        }
        match self.sheddable_band(task.priority) {
            Some(band) => {
                let evicted = self
                    .band_mut(band)
                    .pop_back()
                    .expect("sheddable_band only returns non-empty bands");
                self.total_shed += 1;
                self.band_mut(task.priority).push_back(task);
                EnqueueOutcome::AcceptedAfterShedding(evicted)
            }
            None => EnqueueOutcome::Rejected {
                depth: self.len(),
                capacity: self.capacity,
            },
        }
    }

    /// Drain the highest-priority request.
    pub fn dequeue(&mut self) -> Option<CompilationTask> {
        let task = self
            .high
            .pop_front()
            .or_else(|| self.normal.pop_front())
            .or_else(|| self.low.pop_front());
        if task.is_some() {
            self.total_processed += 1;
        }
        task
    }

    /// Drop every queued request for `class_name`, returning them. Mirrors the
    /// queue half of [`TieredCompilationManager::invalidate_class`].
    pub fn drain_class(&mut self, class_name: &str) -> Vec<CompilationTask> {
        let mut dropped = Vec::new();
        for band in CompilationPriority::BY_RANK {
            let queue = self.band_mut(band);
            let mut kept = VecDeque::with_capacity(queue.len());
            while let Some(task) = queue.pop_front() {
                if task.method_key.class_name == class_name {
                    dropped.push(task);
                } else {
                    kept.push_back(task);
                }
            }
            *queue = kept;
        }
        dropped
    }
}

// ── Dependencies and invalidation ────────────────────────────────────

/// An assumption a compiled body made about the rest of the program.
///
/// A body is only valid while every dependency it recorded still holds; when
/// one breaks, the body must be retired before it can execute again.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Dependency {
    /// The body assumed `class` is loaded and its bytecode unchanged.
    ClassUnchanged(String),
    /// The body devirtualized or inlined on the assumption that no loaded
    /// subclass overrides `method_name` in `class_name`.
    NoSubclassOverrides {
        class_name: String,
        method_name: String,
    },
    /// The body baked a direct call to another compiled method — the shape
    /// `jit::lib.rs` describes as "invalidation … transitively withdraws
    /// direct callers".
    DirectCall(MethodKey),
}

/// Something happened that may have broken a [`Dependency`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidationEvent {
    /// A class was redefined (retransform / instrumentation).
    ClassRedefined(String),
    /// A class was unloaded.
    ClassUnloaded(String),
    /// A subclass overriding `class_name.method_name` was loaded.
    OverrideLoaded {
        class_name: String,
        method_name: String,
    },
    /// A compiled body was retired, so anything that baked a direct call into
    /// it is now stale. Emitted by the broker itself while it walks the
    /// transitive closure; callers rarely construct it directly.
    BodyRetired(MethodKey),
}

impl InvalidationEvent {
    /// Whether this event falsifies `dep`.
    pub fn breaks(&self, dep: &Dependency) -> bool {
        match (self, dep) {
            (
                InvalidationEvent::ClassRedefined(class)
                | InvalidationEvent::ClassUnloaded(class),
                Dependency::ClassUnchanged(dep_class),
            ) => class == dep_class,
            (
                InvalidationEvent::ClassRedefined(class)
                | InvalidationEvent::ClassUnloaded(class),
                Dependency::NoSubclassOverrides { class_name, .. },
            ) => class == class_name,
            (
                InvalidationEvent::ClassRedefined(class)
                | InvalidationEvent::ClassUnloaded(class),
                Dependency::DirectCall(callee),
            ) => *class == callee.class_name,
            (
                InvalidationEvent::OverrideLoaded {
                    class_name,
                    method_name,
                },
                Dependency::NoSubclassOverrides {
                    class_name: dep_class,
                    method_name: dep_method,
                },
            ) => class_name == dep_class && method_name == dep_method,
            (InvalidationEvent::BodyRetired(retired), Dependency::DirectCall(callee)) => {
                retired == callee
            }
            _ => false,
        }
    }

    /// The class this event is about, when it has one.
    pub fn class_name(&self) -> Option<&str> {
        match self {
            InvalidationEvent::ClassRedefined(c) | InvalidationEvent::ClassUnloaded(c) => Some(c),
            InvalidationEvent::OverrideLoaded { class_name, .. } => Some(class_name),
            InvalidationEvent::BodyRetired(key) => Some(&key.class_name),
        }
    }
}

/// Identifies one compiled artifact.
///
/// A method-entry body and the OSR body for one of the same method's
/// back-edges are **different artifacts in different caches** — see
/// [`CompilerCore::complete_task`], where a successful OSR publish
/// deliberately does not advance `current_tier`. Keying compiled records by
/// method alone would let an OSR publish silently displace the entry body's
/// record, and an invalidation would then retire the wrong thing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactId {
    /// The method.
    pub key: MethodKey,
    /// `None` for the method-entry body; `Some(bci)` for the OSR body at that
    /// back-edge.
    pub osr_bci: Option<u32>,
}

impl ArtifactId {
    /// The method-entry body of `key`.
    pub fn entry(key: MethodKey) -> Self {
        ArtifactId { key, osr_bci: None }
    }

    /// The OSR body of `key` at `bci`.
    pub fn osr(key: MethodKey, bci: u32) -> Self {
        ArtifactId {
            key,
            osr_bci: Some(bci),
        }
    }

    /// Whether this is an OSR artifact.
    pub fn is_osr(&self) -> bool {
        self.osr_bci.is_some()
    }
}

/// A compiled body the broker knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRecord {
    /// Which method.
    pub key: MethodKey,
    /// Tier that produced it.
    pub tier: CompilationTier,
    /// `Some(bci)` for an OSR artifact.
    pub osr_bci: Option<u32>,
    /// Emitted bytes — the broker's contribution to code-cache occupancy.
    pub code_bytes: u64,
    /// Assumptions this body made; any one breaking retires it.
    pub dependencies: Vec<Dependency>,
}

// ── Compile verdicts ─────────────────────────────────────────────────

/// What the backend did with a request the broker handed it.
///
/// The failure arm is [`crate::bailout::Bailout`] verbatim. There is
/// deliberately no broker-local failure enum: the compiler already produces
/// structured reasons and a second vocabulary would immediately drift from
/// the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileVerdict {
    /// A body was produced and published.
    Installed {
        /// Wall-clock compile time; over [`TierPolicy::max_c2_compile_time_ms`]
        /// at C2 this demotes the method.
        compile_time_ms: u64,
        /// Emitted bytes.
        code_bytes: u64,
        /// Assumptions the body made.
        dependencies: Vec<Dependency>,
    },
    /// The backend ran and declined, with a structured reason. Spends one
    /// retry, exactly as `CompilerCore::complete_task(success = false)` does.
    Bailed(Bailout),
    /// The VM refused the method on grounds fixed for the life of the process
    /// (skip list, OSR denial). Recorded once; spends no retry. Mirrors
    /// [`CompileOutcome::declined`].
    Declined {
        /// Which rule refused it, for the report.
        rule: &'static str,
    },
}

impl CompileVerdict {
    /// A published body with no recorded dependencies.
    pub fn installed(compile_time_ms: u64, code_bytes: u64) -> Self {
        CompileVerdict::Installed {
            compile_time_ms,
            code_bytes,
            dependencies: Vec::new(),
        }
    }
}

// ── Counters ─────────────────────────────────────────────────────────

/// Everything the broker counted, as a plain comparable value.
///
/// Plain `u64`s rather than atomics: the broker is `&mut self`-driven, so a
/// test reads exact numbers instead of a racy sample. The per-category maps
/// are `BTreeMap` so iteration order is stable — a report whose row order
/// depends on hashing is not observable in the sense step 22 asks for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BrokerCounters {
    /// Requests admitted and queued.
    pub admitted: u64,
    /// Of those, OSR artifacts.
    pub osr_admitted: u64,
    /// Requests declined, for any reason.
    pub declined: u64,
    /// Requests shed to make room for a higher-priority one.
    pub shed: u64,
    /// Requests drained by [`CompilationBroker::next_request`].
    pub dispatched: u64,
    /// Bodies installed.
    pub installed: u64,
    /// Backend bailouts consumed.
    pub bailed: u64,
    /// Permanent VM declines recorded.
    pub declined_permanently: u64,
    /// Bodies retired by invalidation.
    pub retired: u64,
    /// Queued requests dropped because their class went away.
    pub requests_dropped: u64,
    /// Declines by [`DeclineReason::category`].
    pub decline_reasons: BTreeMap<&'static str, u64>,
    /// Bailouts by [`crate::bailout::Bailout::category`] — the same keys
    /// [`crate::bailout::bailout_counts`] reports.
    pub bailout_categories: BTreeMap<&'static str, u64>,
}

// ── The broker ───────────────────────────────────────────────────────

/// Owns compilation policy: counters, the request queue, admission, tier
/// selection, OSR requests, code-cache pressure, and dependency invalidation.
///
/// The whole object is `&mut self`-driven with no locks, no threads and no
/// backend reference. Construct one, feed it synthetic numbers, and assert on
/// what it decided — that is the P1 acceptance criterion, and every test in
/// `broker_tests` is written that way.
///
/// Concurrency is the integration's problem, not the policy's: the VM wiring
/// wraps a broker in the mutex `CompilerCore` already holds. Keeping the
/// policy object lock-free is what makes its decisions reproducible.
pub struct CompilationBroker {
    policy: Box<dyn TierPolicy>,
    queue: BoundedCompileQueue,
    states: HashMap<MethodKey, MethodState>,
    installed: HashMap<ArtifactId, CompiledRecord>,
    cache_cap_bytes: u64,
    cache_used_bytes: u64,
    counters: BrokerCounters,
}

impl CompilationBroker {
    /// A broker driven by `policy`, with the default queue depth and no
    /// code-cache cap.
    pub fn new(policy: Box<dyn TierPolicy>) -> Self {
        CompilationBroker {
            policy,
            queue: BoundedCompileQueue::new(DEFAULT_COMPILE_QUEUE_CAPACITY),
            states: HashMap::new(),
            installed: HashMap::new(),
            cache_cap_bytes: u64::MAX,
            cache_used_bytes: 0,
            counters: BrokerCounters::default(),
        }
    }

    /// A broker running [`ThresholdPolicy`] over `thresholds`.
    pub fn with_thresholds(thresholds: CompilationPolicy) -> Self {
        CompilationBroker::new(Box::new(ThresholdPolicy::new(thresholds)))
    }

    /// A broker running the shipped thresholds.
    pub fn with_default_policy() -> Self {
        CompilationBroker::with_thresholds(CompilationPolicy::default())
    }

    /// Name of the policy in force.
    pub fn policy_name(&self) -> &'static str {
        self.policy.name()
    }

    // ── Configuration ────────────────────────────────────────────────

    /// Bound the request queue. Requests already queued are kept even if the
    /// new bound is smaller; the bound applies to subsequent admissions.
    pub fn set_queue_capacity(&mut self, capacity: usize) {
        let mut replacement = BoundedCompileQueue::new(capacity);
        replacement.high = std::mem::take(&mut self.queue.high);
        replacement.normal = std::mem::take(&mut self.queue.normal);
        replacement.low = std::mem::take(&mut self.queue.low);
        replacement.total_processed = self.queue.total_processed;
        replacement.total_shed = self.queue.total_shed;
        self.queue = replacement;
    }

    /// Cap live code-cache occupancy. [`u64::MAX`] disables the cap, matching
    /// `CRATONVM_JIT_CODE_CACHE_MAX_MB=0`.
    pub fn set_code_cache_cap_bytes(&mut self, cap_bytes: u64) {
        self.cache_cap_bytes = cap_bytes;
    }

    /// Overwrite occupancy with an authoritative external reading — the VM
    /// wiring passes [`crate::COMMITTED_JIT_CODE_BYTES`], which counts deopt
    /// stubs and OSR trampolines the broker never sees.
    pub fn note_code_cache_used(&mut self, used_bytes: u64) {
        self.cache_used_bytes = used_bytes;
    }

    /// Current occupancy against the cap.
    pub fn pressure(&self) -> CodeCachePressure {
        CodeCachePressure::new(self.cache_used_bytes, self.cache_cap_bytes)
    }

    // ── Queries ──────────────────────────────────────────────────────

    /// Signals for `key`, including the process-wide deny rules.
    pub fn signals(&self, key: &MethodKey) -> TierSignals {
        match self.states.get(key) {
            Some(state) => TierSignals::from_state(state).with_process_rules(),
            None => TierSignals::new(key.clone()).with_process_rules(),
        }
    }

    /// Tracked state for `key`, if the broker has seen it.
    pub fn state(&self, key: &MethodKey) -> Option<&MethodState> {
        self.states.get(key)
    }

    /// The method-entry body recorded for `key`, if any.
    pub fn installed_body(&self, key: &MethodKey) -> Option<&CompiledRecord> {
        self.installed.get(&ArtifactId::entry(key.clone()))
    }

    /// The OSR body recorded for `key` at `bci`, if any.
    pub fn installed_osr_body(&self, key: &MethodKey, bci: u32) -> Option<&CompiledRecord> {
        self.installed.get(&ArtifactId::osr(key.clone(), bci))
    }

    /// How many compiled artifacts (entry bodies + OSR bodies) are tracked.
    pub fn installed_count(&self) -> usize {
        self.installed.len()
    }

    /// Requests waiting.
    pub fn queue_depth(&self) -> usize {
        self.queue.len()
    }

    /// The request queue, for inspection.
    pub fn queue(&self) -> &BoundedCompileQueue {
        &self.queue
    }

    /// Everything counted so far.
    pub fn counters(&self) -> &BrokerCounters {
        &self.counters
    }

    // ── Decisions (pure) ─────────────────────────────────────────────

    /// Would this method-entry request be admitted?
    ///
    /// Pure: takes `&self`, mutates nothing, reads no clock and no
    /// environment. Calling it twice with the same signals on the same broker
    /// returns the same decision.
    pub fn decide(&self, signals: &TierSignals) -> AdmissionDecision {
        self.gate(self.policy.select_tier(signals))
    }

    /// Would this OSR request be admitted? Pure — see [`Self::decide`].
    pub fn decide_osr(
        &self,
        signals: &TierSignals,
        bci: u32,
        trigger: OsrTrigger,
    ) -> AdmissionDecision {
        self.gate(self.policy.select_osr(signals, bci, trigger))
    }

    /// Would a C1→C2 upgrade be admitted? Pure — see [`Self::decide`].
    pub fn decide_c2_upgrade(&self, signals: &TierSignals) -> AdmissionDecision {
        self.gate(self.policy.select_c2_upgrade(signals))
    }

    /// The resource gates the *policy* does not own: code-cache pressure and
    /// queue depth. Applied after tier selection so a decline names the tier
    /// rule when one applies and the resource only when the tier rule passed.
    fn gate(&self, decision: AdmissionDecision) -> AdmissionDecision {
        let priority = match &decision {
            AdmissionDecision::Admit(ticket) => ticket.priority,
            AdmissionDecision::Decline(_) => return decision,
        };
        let pressure = self.pressure();
        if pressure.at_capacity() {
            return AdmissionDecision::Decline(DeclineReason::CodeCacheFull {
                used_bytes: pressure.used_bytes,
                cap_bytes: pressure.cap_bytes,
            });
        }
        if self.queue.would_reject(priority) {
            return AdmissionDecision::Decline(DeclineReason::QueueFull {
                depth: self.queue.len(),
                capacity: self.queue.capacity(),
            });
        }
        decision
    }

    // ── Inputs (mutating) ────────────────────────────────────────────

    fn state_mut(&mut self, key: &MethodKey) -> &mut MethodState {
        self.states
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()))
    }

    /// One method-entry invocation. `observed_count` fast-forwards to the
    /// interpreter's real per-method count (see
    /// [`TieredCompilationManager::on_method_invocation_observed`]); pass `0`
    /// for plain `+= 1` counting.
    pub fn on_invocation(&mut self, key: &MethodKey, observed_count: u64) -> AdmissionDecision {
        {
            let state = self.state_mut(key);
            state.invocation_count += 1;
            state.profile.profiled_invocations += 1;
            if observed_count > state.invocation_count {
                state.invocation_count = observed_count;
            }
        }
        let signals = self.signals(key);
        let decision = self.decide(&signals);
        self.apply(key, decision)
    }

    /// One loop back-edge at `bci`, counted against `osr_threshold`.
    pub fn on_backedge(&mut self, key: &MethodKey, bci: u32) -> AdmissionDecision {
        self.osr(key, bci, OsrTrigger::BackedgeCounter)
    }

    /// An OSR request from a caller that has already judged the loop hot.
    pub fn request_osr(&mut self, key: &MethodKey, bci: u32) -> AdmissionDecision {
        self.osr(key, bci, OsrTrigger::CallerJudged)
    }

    fn osr(&mut self, key: &MethodKey, bci: u32, trigger: OsrTrigger) -> AdmissionDecision {
        self.state_mut(key).backedge_count += 1;
        let signals = self.signals(key);
        let decision = self.decide_osr(&signals, bci, trigger);
        self.apply(key, decision)
    }

    /// Request a C1→C2 upgrade for a method whose C1 body just published.
    pub fn request_c2_upgrade(&mut self, key: &MethodKey) -> AdmissionDecision {
        let signals = self.signals(key);
        let decision = self.decide_c2_upgrade(&signals);
        self.apply(key, decision)
    }

    /// Record a decision and, when it admits, queue the request.
    fn apply(&mut self, key: &MethodKey, decision: AdmissionDecision) -> AdmissionDecision {
        let ticket = match decision {
            AdmissionDecision::Decline(reason) => {
                self.count_decline(&reason);
                return AdmissionDecision::Decline(reason);
            }
            AdmissionDecision::Admit(ticket) => ticket,
        };
        let task = CompilationTask {
            method_key: key.clone(),
            target_tier: ticket.tier,
            priority: ticket.priority,
            enqueue_time_ms: 0,
            osr_bci: ticket.osr_bci,
        };
        let outcome = self.queue.enqueue(task);
        match outcome {
            EnqueueOutcome::Accepted => {}
            EnqueueOutcome::AcceptedAfterShedding(evicted) => {
                self.counters.shed += 1;
                // The shed method is no longer in flight. Leaving its flag set
                // would make every later `select_tier` answer `AlreadyQueued`
                // for a request that no longer exists.
                if let Some(state) = self.states.get_mut(&evicted.method_key) {
                    state.queued_for_compilation = false;
                    state.queued_tier = None;
                }
            }
            EnqueueOutcome::Rejected { depth, capacity } => {
                // `gate` already refuses this case, so reaching it means a
                // custom policy handed back a priority the queue cannot take.
                // Report it as the decline it is rather than dropping work.
                let reason = DeclineReason::QueueFull { depth, capacity };
                self.count_decline(&reason);
                return AdmissionDecision::Decline(reason);
            }
        }
        {
            let tier = ticket.tier;
            let state = self.state_mut(key);
            state.queued_for_compilation = true;
            state.queued_tier = Some(tier);
        }
        self.counters.admitted += 1;
        if ticket.osr_bci.is_some() {
            self.counters.osr_admitted += 1;
        }
        AdmissionDecision::Admit(ticket)
    }

    fn count_decline(&mut self, reason: &DeclineReason) {
        self.counters.declined += 1;
        *self
            .counters
            .decline_reasons
            .entry(reason.category())
            .or_insert(0) += 1;
    }

    /// Drain the highest-priority request. The backend-facing half of the
    /// interface: everything above decided *what* to compile, this hands it
    /// over.
    pub fn next_request(&mut self) -> Option<CompilationTask> {
        let task = self.queue.dequeue();
        if task.is_some() {
            self.counters.dispatched += 1;
        }
        task
    }

    /// Record what the backend did with `task`.
    ///
    /// Transcribed from [`CompilerCore::complete_task`], including the parts
    /// that are easy to get wrong: an OSR publish must not advance
    /// `current_tier` (its artifact lives in a separate cache), a permanent
    /// decline must not spend a retry, and a C2 compile over the time budget
    /// demotes the method through the same `c2_bailout` flag three deopts use.
    pub fn complete(&mut self, task: &CompilationTask, verdict: CompileVerdict) {
        let key = task.method_key.clone();
        let tier = task.target_tier;
        let osr = task.is_osr();
        let time_budget = self.policy.max_c2_compile_time_ms();

        match verdict {
            CompileVerdict::Installed {
                compile_time_ms,
                code_bytes,
                dependencies,
            } => {
                {
                    let state = self.state_mut(&key);
                    if !osr {
                        state.current_tier = tier;
                    }
                    state.tier_fail_count = 0;
                    state.queued_for_compilation = false;
                    state.queued_tier = None;
                    state.last_compile_time_ms = compile_time_ms;
                    if tier == CompilationTier::C2
                        && compile_time_ms > time_budget
                        && !state.c2_bailout
                    {
                        state.c2_bailout = true;
                    }
                }
                self.cache_used_bytes = self.cache_used_bytes.saturating_add(code_bytes);
                let id = ArtifactId {
                    key: key.clone(),
                    osr_bci: task.osr_bci,
                };
                // A re-publish replaces the previous artifact rather than
                // double-counting its bytes.
                if let Some(previous) = self.installed.insert(
                    id,
                    CompiledRecord {
                        key,
                        tier,
                        osr_bci: task.osr_bci,
                        code_bytes,
                        dependencies,
                    },
                ) {
                    self.cache_used_bytes =
                        self.cache_used_bytes.saturating_sub(previous.code_bytes);
                }
                self.counters.installed += 1;
            }
            CompileVerdict::Bailed(bailout) => {
                {
                    let state = self.state_mut(&key);
                    state.tier_fail_count = state.tier_fail_count.saturating_add(1);
                    state.queued_for_compilation = false;
                    state.queued_tier = None;
                }
                self.counters.bailed += 1;
                *self
                    .counters
                    .bailout_categories
                    .entry(bailout.category())
                    .or_insert(0) += 1;
            }
            CompileVerdict::Declined { .. } => {
                let state = self.state_mut(&key);
                state.ineligible = true;
                state.queued_for_compilation = false;
                state.queued_tier = None;
                self.counters.declined_permanently += 1;
            }
        }
    }

    /// Record a deoptimization. Three demote the method out of C2, the rule
    /// [`TieredCompilationManager::on_deoptimization`] already applies.
    pub fn on_deoptimization(&mut self, key: &MethodKey) {
        {
            let state = self.state_mut(key);
            state.deopt_count += 1;
            state.current_tier = CompilationTier::Interpreter;
            state.queued_for_compilation = false;
            state.queued_tier = None;
            if state.deopt_count >= MAX_DEOPTS_BEFORE_BAILOUT {
                state.c2_bailout = true;
            }
        }
        for id in self.artifact_ids_of(key) {
            self.drop_artifact(&id);
        }
    }

    /// Every artifact currently tracked for `key`, sorted for determinism.
    fn artifact_ids_of(&self, key: &MethodKey) -> Vec<ArtifactId> {
        let mut ids: Vec<ArtifactId> = self
            .installed
            .keys()
            .filter(|id| id.key == *key)
            .cloned()
            .collect();
        sort_artifact_ids(&mut ids);
        ids
    }

    /// Remove one artifact and return its bytes to the code cache.
    fn drop_artifact(&mut self, id: &ArtifactId) -> Option<CompiledRecord> {
        let record = self.installed.remove(id)?;
        self.cache_used_bytes = self.cache_used_bytes.saturating_sub(record.code_bytes);
        Some(record)
    }

    // ── Invalidation ─────────────────────────────────────────────────

    /// Retire every compiled body whose dependencies `event` falsifies, and
    /// then everything that baked a direct call into a body just retired,
    /// until the closure is empty.
    ///
    /// Returns the retired artifacts. The result is deterministic: each wave
    /// is sorted by artifact identity before it is walked, so the answer does
    /// not depend on hash order.
    ///
    /// Retiring a body returns its bytes to the code cache, which is what
    /// makes invalidation a real input to admission rather than bookkeeping —
    /// a method refused for [`DeclineReason::CodeCacheFull`] becomes
    /// admissible again once something is retired.
    ///
    /// Only a retired **method-entry** body cascades: a direct call is baked
    /// against a method's entry point, so retiring an OSR artifact leaves
    /// every caller valid.
    pub fn invalidate(&mut self, event: &InvalidationEvent) -> Vec<ArtifactId> {
        let mut retired: Vec<ArtifactId> = Vec::new();
        let mut pending: Vec<InvalidationEvent> = vec![event.clone()];
        while let Some(current) = pending.pop() {
            let mut hit: Vec<ArtifactId> = self
                .installed
                .iter()
                .filter(|(_, record)| record.dependencies.iter().any(|dep| current.breaks(dep)))
                .map(|(id, _)| id.clone())
                .collect();
            sort_artifact_ids(&mut hit);
            for id in hit {
                if self.drop_artifact(&id).is_none() {
                    continue;
                }
                if !id.is_osr() {
                    if let Some(state) = self.states.get_mut(&id.key) {
                        // The body is gone, so the method is interpreting
                        // again. Counters and failure budgets are untouched:
                        // nothing about the method changed, only the world it
                        // assumed.
                        state.current_tier = CompilationTier::Interpreter;
                    }
                    pending.push(InvalidationEvent::BodyRetired(id.key.clone()));
                }
                self.counters.retired += 1;
                retired.push(id);
            }
        }
        retired
    }

    /// Forget everything about `class_name`: retire its bodies, drop its
    /// queued requests, and discard its tracked state. The broker-side
    /// counterpart of [`TieredCompilationManager::invalidate_class`].
    pub fn purge_class(&mut self, class_name: &str) -> Vec<ArtifactId> {
        let mut retired = self.invalidate(&InvalidationEvent::ClassUnloaded(class_name.to_string()));
        // A body of this class that recorded no `ClassUnchanged` dependency
        // is still unreachable once the class is gone; drop it too, and give
        // its bytes back.
        let mut leftovers: Vec<ArtifactId> = self
            .installed
            .keys()
            .filter(|id| id.key.class_name == class_name)
            .cloned()
            .collect();
        sort_artifact_ids(&mut leftovers);
        for id in leftovers {
            if self.drop_artifact(&id).is_some() {
                self.counters.retired += 1;
                retired.push(id);
            }
        }
        let dropped = self.queue.drain_class(class_name);
        self.counters.requests_dropped += dropped.len() as u64;
        for task in &dropped {
            if let Some(state) = self.states.get_mut(&task.method_key) {
                state.queued_for_compilation = false;
                state.queued_tier = None;
            }
        }
        self.states.retain(|key, _| key.class_name != class_name);
        retired
    }
}

/// Total order on [`ArtifactId`] by (class, name, descriptor, osr bci).
///
/// A free function rather than an `Ord` derive on the public types: the derive
/// would be a new public trait impl on types the VM stores in hash maps, and
/// nothing outside this ordering needs it. It exists so
/// [`CompilationBroker::invalidate`] walks each wave in a fixed order instead
/// of hash order — a retirement list whose contents depend on hashing is not
/// "deterministic and observable" in the sense the backlog asks for.
fn sort_artifact_ids(ids: &mut [ArtifactId]) {
    ids.sort_by(|a, b| {
        a.key
            .class_name
            .cmp(&b.key.class_name)
            .then_with(|| a.key.method_name.cmp(&b.key.method_name))
            .then_with(|| a.key.descriptor.cmp(&b.key.descriptor))
            .then_with(|| a.osr_bci.cmp(&b.osr_bci))
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> MethodKey {
        MethodKey::new("java/lang/String", "hashCode", "()I")
    }

    /// A key used ONLY by the deny-list test.
    ///
    /// `mark_osr_denied` writes to a PROCESS-GLOBAL set. Marking the shared
    /// `test_key()` therefore denies OSR for every other test in the binary
    /// that uses it, for as long as the mark stands — which is exactly how
    /// `stats_osr_compilations` intermittently observed 0 OSR compilations
    /// instead of 1. Keep the poison on a key nobody else touches.
    fn osr_deny_only_key() -> MethodKey {
        MethodKey::new("craton/test/OsrDenyOnly", "denied", "()V")
    }

    fn test_key2() -> MethodKey {
        MethodKey::new(
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
    }

    #[test]
    #[ignore = "BigInteger JIT deny removed 2026-07-31 with the last static ban mirrors (docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md); the assertion is kept as the record of what the ban covered"]
    fn hibernate_biginteger_divide_cluster_is_never_background_enqueued() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = MethodKey::new(
            "java/math/MutableBigInteger",
            "divideMagnitude",
            "(Ljava/math/MutableBigInteger;Ljava/math/MutableBigInteger;)Ljava/math/MutableBigInteger;",
        );

        assert_eq!(mgr.on_method_invocation(&key), None);
        assert!(
            mgr.dequeue_compilation().is_none(),
            "quarantined MutableBigInteger must not enter the background queue"
        );
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    }

    // ── wire-tiered-manager Step 6: CRATONVM_TIER_* policy overrides ─────

    #[test]
    fn step6_policy_overrides_apply_and_clamp() {
        use std::collections::HashMap;
        let env: HashMap<&str, &str> = [
            ("CRATONVM_TIER_C1_THRESHOLD", "50"),
            ("CRATONVM_TIER_C2_THRESHOLD", "9000"),
            ("CRATONVM_TIER_OSR_THRESHOLD", "0"), // clamps to 1
            ("CRATONVM_TIER_C2_MIN_INVOCATIONS", "garbage"), // ignored → default
            ("CRATONVM_TIER_ENABLED", "0"),
        ]
        .into_iter()
        .collect();
        let p = CompilationPolicy::with_overrides(|k| env.get(k).map(|s| s.to_string()));
        assert_eq!(p.c1_threshold, 50);
        assert_eq!(p.c2_threshold, 9000);
        assert_eq!(p.osr_threshold, 1, "0 must clamp to 1, not disable warmup");
        assert_eq!(
            p.c2_min_invocations, 1_000,
            "unparseable value keeps the default"
        );
        assert!(
            !p.tiered_enabled,
            "CRATONVM_TIER_ENABLED=0 disables tiering"
        );
    }

    #[test]
    fn step6_policy_overrides_empty_env_is_default() {
        let p = CompilationPolicy::with_overrides(|_| None);
        let d = CompilationPolicy::default();
        assert_eq!(p.c1_threshold, d.c1_threshold);
        assert_eq!(p.c2_threshold, d.c2_threshold);
        assert_eq!(p.osr_threshold, d.osr_threshold);
        assert_eq!(p.c2_min_invocations, d.c2_min_invocations);
        assert_eq!(p.tiered_enabled, d.tiered_enabled);
    }

    // ── observed-count fast-forward (stride-boundary deflation fix) ──────

    #[test]
    fn observed_count_fast_forwards_hotness() {
        let mgr = TieredCompilationManager::with_default_policy();

        // Plain counting: a single visit is far below c1_threshold → None.
        let cold = test_key();
        assert_eq!(mgr.on_method_invocation(&cold), None);

        // Observed-count fast-forward: the interpreter has REALLY seen 5,000
        // invocations of this method but only consults the manager at stride
        // boundaries — the recommendation must fire on this single visit
        // instead of after another c1_threshold visits (64x deflation).
        let hot = test_key2();
        let rec = mgr.on_method_invocation_observed(&hot, 5_000);
        assert!(
            rec.is_some(),
            "observed=5000 must produce a tier recommendation on the first visit"
        );

        // A stale/smaller observed value never rewinds the counter.
        let state_count = mgr
            .method_states()
            .into_iter()
            .find(|(k, _, _)| *k == hot)
            .map(|(_, _, c)| c)
            .expect("state for hot key");
        assert!(state_count >= 5_000);
        let _ = mgr.on_method_invocation_observed(&hot, 3);
        let state_count_after = mgr
            .method_states()
            .into_iter()
            .find(|(k, _, _)| *k == hot)
            .map(|(_, _, c)| c)
            .expect("state for hot key");
        assert!(
            state_count_after > state_count.saturating_sub(1),
            "smaller observed count must not rewind the counter"
        );
    }

    // ── wire-tiered-manager Step 5: request_osr ──────────────────────────

    /// Serialises the tests that mutate the process-global OSR deny list.
    ///
    /// `osr_deny_list()` is one `RwLock<HashSet<MethodKey>>` for the whole
    /// process, and `clear_osr_deny_list_for_test` wipes it outright. Run in
    /// parallel, one test's clear lands between another's `mark_osr_denied`
    /// and its assertion. There is no per-test identity to key off here -- the
    /// deny list IS the shared state under test -- so unlike the code-cache
    /// tests these serialise rather than retry.
    fn osr_deny_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn step5_request_osr_enqueues_osr_task_immediately() {
        let _osr_deny_guard = osr_deny_test_guard();
        clear_osr_deny_list_for_test();
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        // Unlike on_backedge, the first call enqueues immediately (no 10k count).
        let task = mgr
            .request_osr(&key, 42)
            .expect("first request should enqueue");
        assert_eq!(task.osr_bci, Some(42));
        assert_eq!(task.priority, CompilationPriority::High);
        assert_eq!(task.target_tier, CompilationTier::C2);
        // Idempotent while queued: a second request is a no-op (no double compile).
        assert!(mgr.request_osr(&key, 42).is_none());
        // The task really is on the queue, and the OSR stat counted exactly once.
        let dq = mgr
            .dequeue_compilation()
            .expect("an OSR task should be queued");
        assert_eq!(dq.osr_bci, Some(42));
        assert_eq!(
            mgr.stats()
                .osr_compilations
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        clear_osr_deny_list_for_test();
    }

    #[test]
    fn step5_request_osr_honors_osr_deny_list() {
        let _osr_deny_guard = osr_deny_test_guard();
        clear_osr_deny_list_for_test();
        let mgr = TieredCompilationManager::with_default_policy();
        let key = osr_deny_only_key();
        mark_osr_denied(key.clone());
        assert!(
            mgr.request_osr(&key, 42).is_none(),
            "OSR-denied method must not enqueue an OSR task"
        );
        assert!(
            mgr.on_backedge(&key, 42).is_none(),
            "OSR-denied method must not enqueue through backedge accounting"
        );
        assert!(mgr.dequeue_compilation().is_none());
        clear_osr_deny_list_for_test();
    }

    #[test]
    fn step5_request_osr_is_independent_of_method_entry_c2_but_honors_bailout() {
        let _osr_deny_guard = osr_deny_test_guard();
        clear_osr_deny_list_for_test();
        // A method-entry C2 body does not provide an OSR entry and therefore
        // must not suppress the separately cached OSR artifact.
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        let task = mgr
            .request_osr(&key, 7)
            .expect("method-entry C2 must still allow an OSR artifact");
        assert_eq!(task.osr_bci, Some(7));

        // C2-bailed method → no OSR enqueue.
        let mgr2 = TieredCompilationManager::with_default_policy();
        let key2 = test_key2();
        mgr2.on_method_invocation(&key2);
        mgr2.on_c2_bailout(&key2);
        assert!(
            mgr2.request_osr(&key2, 7).is_none(),
            "bailed method: no OSR enqueue"
        );
    }

    // ── Policy defaults ──────────────────────────────────────────────────

    #[test]
    fn default_policy_values() {
        let p = CompilationPolicy::default();
        assert_eq!(p.c1_threshold, 500);
        assert_eq!(p.c2_threshold, 20_000);
        assert_eq!(p.osr_threshold, 10_000);
        assert!(p.tiered_enabled);
        assert_eq!(p.c2_min_invocations, 1_000);
        assert!(p.c1_profiling);
    }

    // ── Initial state ────────────────────────────────────────────────────

    #[test]
    fn method_starts_at_interpreter() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert_eq!(mgr.current_tier(&test_key()), CompilationTier::Interpreter);
    }

    // ── Invocation counting ──────────────────────────────────────────────

    #[test]
    fn invocation_count_tracking() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        for _ in 0..50 {
            mgr.on_method_invocation(&key);
        }
        let states = mgr.method_states();
        let (_, _, count) = states.iter().find(|(k, _, _)| k == &key).unwrap();
        assert_eq!(*count, 50);
    }

    // ── C1 trigger ───────────────────────────────────────────────────────

    #[test]
    fn c1_triggered_after_threshold() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        // Drive exactly `c1_threshold` invocations rather than a hardcoded
        // literal, so this test stays correct regardless of the default
        // policy's threshold value (currently 500; see
        // the rationale comment on `CompilationPolicy::default`).
        let c1_threshold = mgr.policy().c1_threshold;
        let mut triggered = None;
        for _ in 0..c1_threshold {
            if let Some(tier) = mgr.on_method_invocation(&key) {
                triggered = Some(tier);
            }
        }
        assert_eq!(triggered, Some(CompilationTier::C1));
    }

    // ── C2 trigger ───────────────────────────────────────────────────────

    #[test]
    fn c2_triggered_after_threshold_with_profile() {
        let policy = CompilationPolicy {
            c1_threshold: 10,
            c2_threshold: 100,
            c2_min_invocations: 50,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // Run up to C1 compilation.
        for _ in 0..10 {
            mgr.on_method_invocation(&key);
        }
        // Drain the C1 task.
        let task = mgr.dequeue_compilation().unwrap();
        assert_eq!(task.target_tier, CompilationTier::C1);
        mgr.compilation_complete(&key, CompilationTier::C1, 5);

        // Keep invoking until C2 threshold.
        let mut c2_triggered = false;
        for _ in 10..100 {
            if let Some(tier) = mgr.on_method_invocation(&key) {
                if tier == CompilationTier::C2 {
                    c2_triggered = true;
                }
            }
        }
        assert!(c2_triggered);
    }

    // ── OSR trigger ──────────────────────────────────────────────────────

    #[test]
    fn osr_triggered_after_backedge_threshold() {
        let policy = CompilationPolicy {
            osr_threshold: 50,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        let mut osr_task = None;
        for _i in 0..50 {
            if let Some(t) = mgr.on_backedge(&key, 42) {
                osr_task = Some(t);
            }
        }
        let task = osr_task.unwrap();
        assert_eq!(task.osr_bci, Some(42));
        assert_eq!(task.target_tier, CompilationTier::C2);
    }

    // ── Branch profile ───────────────────────────────────────────────────

    #[test]
    fn branch_profile_recording() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.record_branch(&key, 10, true);
        mgr.record_branch(&key, 10, true);
        mgr.record_branch(&key, 10, false);

        let profile = mgr.get_profile(&key).unwrap();
        let bp = &profile.branch_counts[&10];
        assert_eq!(bp.taken, 2);
        assert_eq!(bp.not_taken, 1);
    }

    // ── Receiver profile (top 3) ─────────────────────────────────────────

    #[test]
    fn receiver_profile_recording_top3() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        // Record 4 different class_ids — only 3 should be retained.
        for _ in 0..10 {
            mgr.record_receiver(&key, 5, 100);
        }
        for _ in 0..5 {
            mgr.record_receiver(&key, 5, 200);
        }
        for _ in 0..3 {
            mgr.record_receiver(&key, 5, 300);
        }
        // This fourth type should not displace existing ones (they all have count >= 1).
        mgr.record_receiver(&key, 5, 400);

        let profile = mgr.get_profile(&key).unwrap();
        let rp = &profile.receiver_profiles[&5];
        assert_eq!(rp.receivers.len(), 3);
        assert_eq!(rp.total_calls, 19);
    }

    // ── Type profile ─────────────────────────────────────────────────────

    #[test]
    fn type_profile_recording() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.record_type_check(&key, 20, 42);
        mgr.record_type_check(&key, 20, 42);
        mgr.record_type_check(&key, 20, 99);

        let profile = mgr.get_profile(&key).unwrap();
        let tp = &profile.type_profiles[&20];
        assert_eq!(tp.total_checks, 3);
        assert_eq!(tp.types.len(), 2);
    }

    // ── Null profile ─────────────────────────────────────────────────────

    #[test]
    fn null_profile_recording() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.record_null_check(&key, 15, true);
        mgr.record_null_check(&key, 15, false);
        mgr.record_null_check(&key, 15, false);

        let profile = mgr.get_profile(&key).unwrap();
        let np = &profile.null_profiles[&15];
        assert_eq!(np.null_seen, 1);
        assert_eq!(np.non_null_seen, 2);
    }

    // ── Queue priority ordering ──────────────────────────────────────────

    #[test]
    fn queue_priority_ordering() {
        let mgr = TieredCompilationManager::with_default_policy();
        let k1 = MethodKey::new("A", "a", "()V");
        let k2 = MethodKey::new("B", "b", "()V");
        let k3 = MethodKey::new("C", "c", "()V");

        mgr.enqueue_compilation(CompilationTask {
            method_key: k2.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: k3.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Low,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: k1.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });

        let t1 = mgr.dequeue_compilation().unwrap();
        assert_eq!(t1.priority, CompilationPriority::High);
        let t2 = mgr.dequeue_compilation().unwrap();
        assert_eq!(t2.priority, CompilationPriority::Normal);
        let t3 = mgr.dequeue_compilation().unwrap();
        assert_eq!(t3.priority, CompilationPriority::Low);
    }

    // ── Enqueue / dequeue roundtrip ──────────────────────────────────────

    #[test]
    fn enqueue_dequeue_roundtrip() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 42,
            osr_bci: None,
        });
        let task = mgr.dequeue_compilation().unwrap();
        assert_eq!(task.method_key, key);
        assert_eq!(task.enqueue_time_ms, 42);
    }

    // ── Compilation complete updates tier ────────────────────────────────

    #[test]
    fn compilation_complete_updates_tier() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key); // create state
        mgr.compilation_complete(&key, CompilationTier::C1, 10);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    }

    // ── Failed compile attempts do not fake "compiled" and DO retry ──────
    //
    // Regression coverage for the bg-compile-no-publish bug: a
    // `compile_fn` that runs but bails (skip-listed method, resolver miss,
    // code-cache cap, ...) must not be recorded as having reached `tier` —
    // `current_tier` has to stay put so a later invocation gets another
    // shot, and the per-tier compilation stat must not count a body that
    // was never published.

    #[test]
    fn failed_compile_does_not_advance_tier_or_stats() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key); // create state
        mgr.core
            .complete_task(&key, CompilationTier::C1, 10, false, false, false);
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::Interpreter,
            "a failed attempt must not advance current_tier"
        );
        assert_eq!(
            mgr.stats().c1_compilations.load(Ordering::Relaxed),
            0,
            "a failed attempt must not count as a C1 compilation"
        );
    }

    // ── Policy declines are recorded once, not charged to the retry budget ──
    //
    // A method the VM refuses on policy grounds (skip list, OSR denial) used
    // to report `success=false` exactly like a failed compile, so it was
    // enqueued and declined three times before `tier_fail_count` saturated.
    // Two of those round-trips were waste, and the resulting
    // `tier_fail_count=3` was indistinguishable from genuinely broken codegen
    // in `hot_but_stuck_in_interpreter`.

    #[test]
    fn permanent_decline_bans_on_first_attempt_without_spending_retries() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        mgr.core
            .complete_task(&key, CompilationTier::C1, 0, false, false, true);

        {
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(state.ineligible, "a policy decline must be recorded");
            assert_eq!(
                state.tier_fail_count, 0,
                "a policy decline must NOT spend the compile-failure retry budget"
            );
        }

        // ONE decline is enough: no further invocation may re-enqueue it.
        for _ in 0..10 {
            assert_eq!(
                mgr.on_method_invocation(&key),
                None,
                "an ineligible method must never be re-enqueued"
            );
        }
    }

    #[test]
    fn permanent_decline_does_not_ban_unrelated_methods() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let declined = test_key();
        let healthy = MethodKey::new("Other", "m", "()V");

        assert_eq!(mgr.on_method_invocation(&declined), Some(CompilationTier::C1));
        mgr.core
            .complete_task(&declined, CompilationTier::C1, 0, false, false, true);

        // The other method is untouched and still compiles normally.
        assert_eq!(mgr.on_method_invocation(&healthy), Some(CompilationTier::C1));
        mgr.core
            .complete_task(&healthy, CompilationTier::C1, 1, true, false, false);
        assert_eq!(mgr.current_tier(&healthy), CompilationTier::C1);
    }

    #[test]
    fn failed_compile_is_retried_up_to_the_fail_limit() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // 1st invocation crosses c1_threshold=1 and enqueues C1.
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        // Fail it MAX_TIER_FAIL_RETRIES - 1 times; each failure must still
        // leave the method eligible for another attempt (queued_for_compilation
        // reset, current_tier untouched).
        for i in 0..(MAX_TIER_FAIL_RETRIES - 1) {
            mgr.core
                .complete_task(&key, CompilationTier::C1, 1, false, false, false);
            assert_eq!(
                mgr.on_method_invocation(&key),
                Some(CompilationTier::C1),
                "attempt {i}: should still be retried below the fail limit"
            );
        }
        // One more failure reaches MAX_TIER_FAIL_RETRIES — should_compile
        // must now give up permanently.
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, false, false, false);
        assert_eq!(
            mgr.on_method_invocation(&key),
            None,
            "should stop recommending compilation after the fail limit"
        );
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    }

    #[test]
    fn successful_compile_after_a_failure_resets_the_fail_count() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, false, false, false);
        mgr.core
            .complete_task(&key, CompilationTier::C1, 5, true, false, false);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
        let methods = mgr.core.methods.lock();
        assert_eq!(
            methods[&key].tier_fail_count, 0,
            "a later success should reset the fail streak"
        );
    }

    // Regression coverage for the OSR-starves-method-entry bug: a successful
    // OSR compile publishes into the SEPARATE OSR artifact cache, so it must
    // not stamp `current_tier = C2` — that made `should_compile` refuse every
    // later method-entry recommendation while the method-entry cache was
    // still empty, so each fresh invocation of a loop-heavy method (e.g.
    // QuickBench sieve) re-entered the interpreter and re-OSR'd forever.
    #[test]
    fn osr_completion_does_not_suppress_method_entry_tiering() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // A hot back-edge enqueues an OSR task (target tier C2) before the
        // invocation counter has recommended anything.
        let task = mgr.request_osr(&key, 42).expect("OSR task should enqueue");
        assert_eq!(task.osr_bci, Some(42));
        // The worker completes it successfully — artifact goes to the OSR
        // cache, `osr = true`.
        mgr.core
            .complete_task(&key, task.target_tier, 3, true, true, false);
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::Interpreter,
            "an OSR publish must not advance the method-entry tier"
        );
        // The invocation counter must still be able to recommend the
        // method-entry compile.
        assert_eq!(
            mgr.on_method_invocation(&key),
            Some(CompilationTier::C1),
            "method-entry compilation must still be recommended after an OSR publish"
        );
        // And a successful method-entry completion advances the tier as usual.
        mgr.core
            .complete_task(&key, CompilationTier::C1, 2, true, false, false);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    }

    // ── Deoptimization ───────────────────────────────────────────────────

    #[test]
    fn deoptimization_drops_to_interpreter() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 50);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);

        mgr.on_deoptimization(&key);
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    }

    #[test]
    fn multiple_deopts_trigger_c2_bailout() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        for _ in 0..3 {
            mgr.compilation_complete(&key, CompilationTier::C2, 10);
            mgr.on_deoptimization(&key);
        }
        // After 3 deopts, c2_bailout should be set.
        let methods = mgr.core.methods.lock();
        assert!(methods[&key].c2_bailout);
    }

    // ── Tier-4 compile-time guard (jit-inlining-and-ir-calls) ────────────

    /// A C2 compile that stays inside `MAX_C2_COMPILE_TIME_MS` must leave the
    /// method eligible for C2; one that exceeds it must demote the method to
    /// C1 for the rest of the process, through the SAME `c2_bailout` flag the
    /// 3-deopt rule uses (so every existing degradation path honours it with
    /// no additional wiring).
    ///
    /// This is the coherence guarantee for the widened `ir_compatible` gate:
    /// the population reaching tier 4 grew from small arithmetic kernels to
    /// ordinary call-bearing application methods, and nothing previously
    /// bounded the OBSERVED cost of an optimizing compile.
    #[test]
    fn slow_c2_compile_demotes_to_c1() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);

        // Inside the budget → still a C2 method.
        mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS);
        assert!(
            !mgr.core.methods.lock()[&key].c2_bailout,
            "a C2 compile at exactly the budget must not demote"
        );

        // Over the budget → permanent C2 bailout, and the statistic counts it
        // alongside deopt-driven bailouts.
        let before = mgr.stats().c2_bailouts.load(Ordering::Relaxed);
        mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS + 1);
        assert!(
            mgr.core.methods.lock()[&key].c2_bailout,
            "a C2 compile over MAX_C2_COMPILE_TIME_MS must demote the method to C1"
        );
        assert_eq!(
            mgr.stats().c2_bailouts.load(Ordering::Relaxed),
            before + 1,
            "the compile-time demotion must be counted as a c2 bailout"
        );
    }

    /// The compile-time guard is C2-only: a slow C1 compile is not a reason to
    /// refuse the optimizing tier (C1 and C2 use different backends, and the
    /// single-pass backend is the fallback the bailout demotes *to*).
    #[test]
    fn slow_c1_compile_does_not_demote() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, MAX_C2_COMPILE_TIME_MS * 10);
        assert!(!mgr.core.methods.lock()[&key].c2_bailout);
    }

    // ── C2 bailout stays at C1 ───────────────────────────────────────────

    #[test]
    fn c2_bailout_stays_at_c1() {
        let policy = CompilationPolicy {
            c1_threshold: 5,
            c2_threshold: 20,
            c2_min_invocations: 10,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // Trigger C1.
        for _ in 0..5 {
            mgr.on_method_invocation(&key);
        }
        mgr.dequeue_compilation();
        mgr.compilation_complete(&key, CompilationTier::C1, 5);

        // Force bailout.
        mgr.on_c2_bailout(&key);

        // Keep invoking past C2 threshold — should NOT trigger C2.
        for _ in 5..30 {
            let tier = mgr.on_method_invocation(&key);
            assert_ne!(tier, Some(CompilationTier::C2));
        }
    }

    // ── Queue size ───────────────────────────────────────────────────────

    #[test]
    fn queue_size_tracking() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert_eq!(mgr.queue_size(), 0);

        let key = test_key();
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        assert_eq!(mgr.queue_size(), 1);

        mgr.dequeue_compilation();
        assert_eq!(mgr.queue_size(), 0);
    }

    // ── Stats counting ───────────────────────────────────────────────────

    #[test]
    fn stats_c1_compilations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, 10);
        assert_eq!(mgr.stats().c1_compilations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stats_c2_compilations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 50);
        assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stats_osr_compilations() {
        let policy = CompilationPolicy {
            osr_threshold: 5,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        for _ in 0..5 {
            mgr.on_backedge(&key, 0);
        }
        assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stats_deoptimizations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 10);
        mgr.on_deoptimization(&key);
        assert_eq!(mgr.stats().deoptimizations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stats_total_compile_time() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, 15);
        mgr.compilation_complete(&key, CompilationTier::C2, 85);
        assert_eq!(
            mgr.stats().total_compile_time_ms.load(Ordering::Relaxed),
            100
        );
    }

    // ── Method states listing ────────────────────────────────────────────

    #[test]
    fn method_states_listing() {
        let mgr = TieredCompilationManager::with_default_policy();
        let k1 = test_key();
        let k2 = test_key2();
        mgr.on_method_invocation(&k1);
        mgr.on_method_invocation(&k2);
        mgr.on_method_invocation(&k2);

        let states = mgr.method_states();
        assert_eq!(states.len(), 2);
    }

    // ── Policy update ────────────────────────────────────────────────────

    #[test]
    fn policy_update() {
        let mgr = TieredCompilationManager::with_default_policy();
        let mut p = mgr.policy();
        assert_eq!(p.c1_threshold, 500);
        p.c1_threshold = 500;
        mgr.set_policy(p);
        assert_eq!(mgr.policy().c1_threshold, 500);
    }

    // ── Empty queue returns None ─────────────────────────────────────────

    #[test]
    fn empty_queue_returns_none() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert!(mgr.dequeue_compilation().is_none());
        assert!(mgr.queue_empty());
    }

    // ── High priority first ──────────────────────────────────────────────

    #[test]
    fn queue_processes_high_priority_first() {
        let mgr = TieredCompilationManager::with_default_policy();
        mgr.enqueue_compilation(CompilationTask {
            method_key: test_key(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Low,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: test_key2(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });

        let first = mgr.dequeue_compilation().unwrap();
        assert_eq!(first.method_key, test_key2());
        assert_eq!(first.priority, CompilationPriority::High);
    }

    // ── OSR compilation has bci set ──────────────────────────────────────

    #[test]
    fn osr_compilation_has_bci() {
        let policy = CompilationPolicy {
            osr_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        let task = mgr.on_backedge(&key, 77).unwrap();
        assert_eq!(task.osr_bci, Some(77));
    }

    // ── Tiered disabled ──────────────────────────────────────────────────

    #[test]
    fn no_compilation_when_tiered_disabled() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            tiered_enabled: false,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        for _ in 0..100 {
            assert!(mgr.on_method_invocation(&key).is_none());
        }
    }

    // ── Current tier query ───────────────────────────────────────────────

    #[test]
    fn current_tier_returns_correct_value() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1WithProfiling, 5);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1WithProfiling);
    }

    // ── Profile data accessible ──────────────────────────────────────────

    #[test]
    fn profile_data_accessible_after_recording() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.record_branch(&key, 0, true);
        mgr.record_null_check(&key, 1, false);

        let profile = mgr.get_profile(&key).unwrap();
        assert!(profile.branch_counts.contains_key(&0));
        assert!(profile.null_profiles.contains_key(&1));
    }

    // ── Profiled invocations counted ─────────────────────────────────────

    #[test]
    fn profiled_invocations_counted() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        for _ in 0..10 {
            mgr.on_method_invocation(&key);
        }
        let profile = mgr.get_profile(&key).unwrap();
        assert_eq!(profile.profiled_invocations, 10);
    }

    // ── Compilation task priorities ──────────────────────────────────────

    #[test]
    fn compilation_task_priorities() {
        assert_eq!(CompilationPriority::High, CompilationPriority::High);
        assert_ne!(CompilationPriority::High, CompilationPriority::Low);
    }

    // ── Queue total_processed ────────────────────────────────────────────

    #[test]
    fn queue_total_processed_counter() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.dequeue_compilation();
        mgr.dequeue_compilation();

        let q = mgr.core.queue.lock();
        assert_eq!(q.total_processed, 2);
    }

    // ── Custom policy thresholds ─────────────────────────────────────────

    #[test]
    fn custom_policy_thresholds() {
        let policy = CompilationPolicy {
            c1_threshold: 10,
            c2_threshold: 50,
            osr_threshold: 100,
            tiered_enabled: true,
            c2_min_invocations: 25,
            c1_profiling: false,
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // Should trigger C1 at invocation 10.
        let mut triggered_at = None;
        for i in 1..=15 {
            if let Some(CompilationTier::C1) = mgr.on_method_invocation(&key) {
                triggered_at = Some(i);
                break;
            }
        }
        assert_eq!(triggered_at, Some(10));
    }

    // ── No profile returns None ──────────────────────────────────────────

    #[test]
    fn no_profile_returns_none() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert!(mgr.get_profile(&test_key()).is_none());
    }

    // ── Compiler active flag ─────────────────────────────────────────────

    #[test]
    fn compiler_active_flag() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert!(!mgr.compiler_active());
        mgr.set_compiler_active(true);
        assert!(mgr.compiler_active());
    }

    // ── Tier ordering ────────────────────────────────────────────────────

    #[test]
    fn tier_ordering() {
        assert!(CompilationTier::Interpreter < CompilationTier::C1);
        assert!(CompilationTier::C1 < CompilationTier::C1WithProfiling);
        assert!(CompilationTier::C1WithProfiling < CompilationTier::FullProfile);
        assert!(CompilationTier::FullProfile < CompilationTier::C2);
    }

    // ── Deopt resets queued state ────────────────────────────────────────

    #[test]
    fn deopt_resets_queued_state() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.on_deoptimization(&key);

        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(!state.queued_for_compilation);
        assert!(state.queued_tier.is_none());
    }

    // ── C2 bailout stat incremented ──────────────────────────────────────

    #[test]
    fn c2_bailout_stat() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.on_c2_bailout(&key);
        assert_eq!(mgr.stats().c2_bailouts.load(Ordering::Relaxed), 1);
    }

    // ── Backedge disabled when tiered off ────────────────────────────────

    #[test]
    fn backedge_disabled_when_tiered_off() {
        let policy = CompilationPolicy {
            osr_threshold: 1,
            tiered_enabled: false,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        assert!(mgr.on_backedge(&key, 0).is_none());
    }

    // ── Background worker drains an enqueued task off-thread ──────────────

    /// wire-tiered-manager increment 1: crossing the C1 threshold via
    /// `on_method_invocation` enqueues a task, and the background compile thread
    /// dequeues + "compiles" it on a *different* thread, then publishes the tier.
    ///
    /// Deterministic: the test blocks on an `mpsc` recv (a synchronization
    /// handle) rather than sleeping, so it never races on timing.
    #[test]
    fn background_worker_drains_enqueued_task_off_thread() {
        use std::sync::mpsc;

        // Low C1 threshold so a couple of invocations cross it. Disable the
        // straight-to-C2 path by keeping c2 thresholds high.
        let policy = CompilationPolicy {
            c1_threshold: 2,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // The compile closure reports (task tier, the thread it ran on) back to
        // the test thread, proving the work happened off the "mutator".
        let (tx, rx) = mpsc::channel::<(CompilationTier, std::thread::ThreadId)>();
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                tx.send((task.target_tier, std::thread::current().id()))
                    .unwrap();
                // pretend the compile took 7ms and published
                CompileOutcome {
                    compile_time_ms: 7,
                    published: true,
                    c2_upgrade_candidate: false,
                    declined_permanently: false,
                }
            }))
            .expect("worker should start");

        let mutator_thread = std::thread::current().id();

        // Drive invocations on *this* (mutator) thread until the threshold
        // crossing enqueues a C1 task.
        assert!(
            mgr.on_method_invocation(&key).is_none(),
            "1st invocation: below threshold"
        );
        let rec = mgr.on_method_invocation(&key);
        assert_eq!(
            rec,
            Some(CompilationTier::C1),
            "threshold crossing enqueues C1"
        );

        // The worker should pick it up off-thread. Block on the channel (no sleep).
        let (compiled_tier, worker_thread) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("worker must drain the task");
        assert_eq!(compiled_tier, CompilationTier::C1);
        assert_ne!(
            worker_thread, mutator_thread,
            "compilation must run OFF the mutator thread"
        );

        // After completion the worker must publish the tier and clear the queue.
        // Spin briefly on the completion counter (bounded, no fixed sleep).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 1, "one task completed");
        assert!(mgr.queue_empty(), "queue drained");
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::C1,
            "tier published by worker"
        );
        {
            let methods = mgr.core.methods.lock();
            assert!(
                !methods[&key].queued_for_compilation,
                "queued flag cleared on completion"
            );
            assert_eq!(methods[&key].last_compile_time_ms, 7);
        }

        // Clean shutdown joins the worker thread.
        drop(bg);
        assert!(!mgr.compiler_active(), "worker stopped after shutdown");
    }

    // ── Tier → backend routing (Step 3) ──────────────────────────────────

    #[test]
    fn tier_routing_selects_optimized_backend_for_c2() {
        // C1 tiers route to the fast single-pass (no-opt) backend; C2 (and the
        // FullProfile collection tier, which only reaches codegen as a C2
        // promotion) route to the optimizing pipeline.
        assert!(!tier_uses_optimized_backend(CompilationTier::Interpreter));
        assert!(!tier_uses_optimized_backend(CompilationTier::C1));
        assert!(!tier_uses_optimized_backend(
            CompilationTier::C1WithProfiling
        ));
        assert!(tier_uses_optimized_backend(CompilationTier::FullProfile));
        assert!(tier_uses_optimized_backend(CompilationTier::C2));
    }

    // ── C1→C2 supersede: candidate C1 publish auto-enqueues a C2 recompile ──

    #[test]
    fn c1_publish_with_upgrade_candidate_enqueues_c2_supersede() {
        use std::sync::mpsc;
        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        let (tx, rx) = mpsc::channel::<CompilationTier>();
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                tx.send(task.target_tier).unwrap();
                CompileOutcome {
                    compile_time_ms: 1,
                    published: true,
                    // Models the VM-side predicate: judged IR-eligible on the
                    // C1 pass; a C2 task never re-seeds an upgrade.
                    c2_upgrade_candidate: !tier_uses_optimized_backend(task.target_tier),
                    declined_permanently: false,
                }
            }))
            .expect("worker should start");

        // Cross c1_threshold=1 → C1 task enqueued.
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));

        // Worker compiles C1, then the loop auto-enqueues + compiles the C2
        // supersede (Low priority). Deterministic via the channel.
        let first = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("C1 compile must run");
        assert_eq!(first, CompilationTier::C1);
        let second = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("C2 supersede compile must follow a candidate C1 publish");
        assert_eq!(second, CompilationTier::C2);

        // Both completions recorded; tier settles at C2; nothing re-queued
        // (request_c2_upgrade is idempotent and gated on current_tier < C2).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while mgr.completed_compilations() < 2 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 2);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
        assert!(mgr.queue_empty(), "no repeat upgrade churn");
        drop(bg);
    }

    // ── Increment 2: flag-gated off-thread compile publishes the Jit target ──

    /// wire-tiered-manager increment 2: with background compilation enabled, a
    /// crossed threshold enqueues a task that the worker compiles OFF the
    /// mutator thread; the worker then "publishes" the compiled entry (here a
    /// shared `Jit`-target map standing in for `SharedVm::jit_cache`, which is
    /// VM-crate-only) and the manager's tier is flipped to the compiled tier —
    /// the jit-crate analogue of the invoke cache being updated to the `Jit`
    /// target. Deterministic: blocks on an `mpsc` recv, never sleeps.
    #[test]
    fn flag_on_threshold_compiles_off_thread_and_publishes_jit_target() {
        use std::sync::mpsc;

        // A stand-in for the VM's `jit_cache`: the compile closure inserts the
        // method key here to model "the Jit target is now installed/published".
        let published: Arc<Mutex<Vec<MethodKey>>> = Arc::new(Mutex::new(Vec::new()));

        // Low C2 threshold so a couple of invocations route STRAIGHT to C2
        // (the optimized backend), exercising the Step-3 routing decision.
        let policy = CompilationPolicy {
            c1_threshold: u32::MAX, // skip the C1 step
            c2_threshold: 2,
            c2_min_invocations: 2,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        let mutator_thread = std::thread::current().id();
        let (tx, rx) = mpsc::channel::<(CompilationTier, bool, std::thread::ThreadId)>();
        let published_w = Arc::clone(&published);
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                // Real compile_fn shape: pick the backend by tier (Step 3),
                // "publish" the Jit target, and report back off-thread.
                let optimized = tier_uses_optimized_backend(task.target_tier);
                published_w.lock().push(task.method_key.clone());
                tx.send((task.target_tier, optimized, std::thread::current().id()))
                    .unwrap();
                CompileOutcome {
                    compile_time_ms: 3,
                    published: true,
                    c2_upgrade_candidate: false,
                    declined_permanently: false,
                }
            }))
            .expect("worker should start");

        // Drive invocations on the mutator thread until C2 is recommended.
        assert!(
            mgr.on_method_invocation(&key).is_none(),
            "1st invocation: below threshold"
        );
        let rec = mgr.on_method_invocation(&key);
        assert_eq!(
            rec,
            Some(CompilationTier::C2),
            "threshold crossing enqueues a C2 task (straight-to-C2 path)"
        );

        // Worker drains + compiles off-thread; block on the channel (no sleep).
        let (compiled_tier, optimized, worker_thread) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("worker must drain the task");
        assert_eq!(compiled_tier, CompilationTier::C2);
        assert!(
            optimized,
            "C2 must route to the optimized backend (Step 3 routing)"
        );
        assert_ne!(
            worker_thread, mutator_thread,
            "compilation must run OFF the mutator thread"
        );

        // After completion the worker publishes the tier (invoke-cache analogue)
        // and clears the queue. Bounded spin on the completion counter.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 1, "one task completed");
        assert!(mgr.queue_empty(), "queue drained");
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::C2,
            "Jit target (tier) published by the worker"
        );
        assert_eq!(
            *published.lock(),
            vec![key.clone()],
            "the compiled method's Jit target was published off-thread"
        );
        assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 1);

        drop(bg);
        assert!(!mgr.compiler_active(), "worker stopped after shutdown");
    }

    // ── Increment 3: GC-STW-safety — no VM-equivalent lock held across waits ──

    /// wire-tiered-manager increment 3 (GC-STW-safety): the background worker
    /// must hold NO VM lock across (a) its queue wait and (b) an in-flight
    /// compile's own blocking. This is the load-bearing invariant that keeps a
    /// STW prompt: a mutator wanting an exclusive VM lock (modelled here by
    /// `vm_lock`, standing in for `SharedVm::class_manager.write()`) must be
    /// able to acquire it WHILE a compile task is in-flight, because the worker
    /// only ever takes that lock for a bounded scope and drops it before doing
    /// anything blocking.
    ///
    /// The test drives a compile task whose `compile_fn` mirrors the real
    /// `background_compile_task` lock shape: briefly take `vm_lock` (read out
    /// what it needs), DROP it, then block (here on a barrier standing in for
    /// the long codegen / a `load_class_concurrent` condvar wait). While the
    /// worker is blocked mid-compile, a competing "STW initiator" thread must
    /// acquire `vm_lock` PROMPTLY. If the worker wrongly held a VM lock across
    /// its blocking wait, this acquisition would deadlock and the bounded
    /// `recv_timeout` would fire. Deterministic: every rendezvous is a channel
    /// recv or a barrier, never a sleep.
    #[test]
    fn worker_holds_no_vm_lock_across_blocking_compile() {
        use std::sync::mpsc;
        use std::sync::{Arc as StdArc, Barrier};

        // Stand-in for `SharedVm::class_manager` (the lock a class-defining
        // mutator / STW path contends for). The worker takes it only briefly.
        let vm_lock: StdArc<Mutex<u64>> = StdArc::new(Mutex::new(0));

        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // Rendezvous: worker -> test when it has ENTERED the compile and is
        // about to block; a two-party barrier the worker waits on to model the
        // long in-flight compile; and a channel the worker uses to report the
        // VM-lock value it read during its bounded critical section.
        let (entered_tx, entered_rx) = mpsc::channel::<u64>();
        let release = StdArc::new(Barrier::new(2));

        let vm_lock_w = StdArc::clone(&vm_lock);
        let release_w = StdArc::clone(&release);
        let bg = mgr
            .start_background_compiler(Box::new(move |_task: &CompilationTask| -> CompileOutcome {
                // (1) Bounded VM-lock scope: acquire, read, DROP — exactly the
                // shape `try_jit_compile_callee_slow` uses for class_manager /
                // jit_cache. The guard must NOT survive into the blocking wait.
                let seen = {
                    let g = vm_lock_w.lock();
                    *g
                }; // <-- guard dropped here, BEFORE blocking below.
                entered_tx.send(seen).unwrap();
                // (2) Blocking wait with NO VM lock held — models long codegen
                // or a `load_class_concurrent` condvar wait. If a VM lock were
                // still held here, the STW thread below would deadlock.
                release_w.wait();
                CompileOutcome {
                    compile_time_ms: 4,
                    published: true,
                    c2_upgrade_candidate: false,
                    declined_permanently: false,
                }
            }))
            .expect("worker should start");

        // Enqueue one task (crossing c1_threshold=1 on the 1st invocation) ->
        // worker picks it up.
        assert_eq!(
            mgr.on_method_invocation(&key),
            Some(CompilationTier::C1),
            "1st invocation crosses c1_threshold=1 and enqueues a C1 task"
        );
        // The worker has entered the compile and finished its bounded VM-lock
        // critical section; block on the channel (no sleep).
        let seen = entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("worker must enter compile and release the VM lock");
        assert_eq!(seen, 0, "worker read the VM-lock-protected state");

        // The worker is now blocked mid-compile (on `release`). A competing STW
        // initiator MUST be able to grab the VM lock promptly — proving the
        // worker holds no VM lock across its blocking wait. Do it on a separate
        // thread with a bounded join so a regression deadlocks the test thread's
        // timeout rather than hanging forever.
        let vm_lock_stw = StdArc::clone(&vm_lock);
        let (stw_tx, stw_rx) = mpsc::channel::<()>();
        let stw = std::thread::spawn(move || {
            let mut g = vm_lock_stw.lock();
            *g += 1; // mutate while the worker is mid-compile
            stw_tx.send(()).unwrap();
        });
        stw_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("STW initiator must acquire the VM lock while a compile is in-flight");
        stw.join().unwrap();
        assert_eq!(*vm_lock.lock(), 1, "STW path mutated the VM-locked state");

        // The worker is also not holding its OWN queue lock while mid-compile:
        // `queue_size()` takes `core.queue.lock()` and returns without blocking,
        // confirming the worker dropped the queue lock before running compile_fn
        // (the queue was drained when the task was dequeued).
        assert_eq!(mgr.queue_size(), 0, "queue drained while compile in-flight");

        // Let the in-flight compile finish.
        release.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 1, "compile completed");

        drop(bg);
        assert!(!mgr.compiler_active(), "worker stopped after shutdown");
    }
}

