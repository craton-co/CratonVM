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

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::{Condvar, Mutex, RwLock};
use rustc_hash::FxHashMap;

/// Consecutive background-compile attempts allowed to fail (run but not
/// publish a body) at a given tier before `should_compile` gives up on that
/// method entirely. See `CompilerCore::complete_task` / `should_compile`.
const MAX_TIER_FAIL_RETRIES: u32 = 3;

fn osr_deny_list() -> &'static RwLock<HashSet<MethodKey>> {
    static LIST: std::sync::OnceLock<RwLock<HashSet<MethodKey>>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Methods statically known to corrupt state when OSR-entered, pending a
/// full root-cause fix. Denying OSR for a method does not stop it from
/// tiering up to normal (non-OSR) JIT compilation from a fresh call -- it
/// only forces an already-interpreting invocation to keep interpreting
/// rather than jumping into compiled code mid-loop.
///
/// `java/util/DualPivotQuicksort.sort` (both the `([DIII)V` entry point and
/// the `(Ldk$Sorter;[DIII)V` worker it delegates to) is denied here:
/// ES's `libs/tdigest` `SortingDigestTests` (`testSorted`, `testMonotonicity`,
/// `testMidPointRule`, `testSingletonAtEnd`, `testFewRepeatedValues`) reads a
/// garbage `ArrayIndexOutOfBoundsException` index -- always a plausible heap
/// pointer (e.g. `0x20048466300`), never a plausible array index -- out of
/// `SortingDigest.compress()`'s `values.sort()` call, which bottoms out in
/// `Arrays.sort(double[])` -> `DualPivotQuicksort.sort`. `CRATONVM_DBG_OSR=1`
/// on the failing repro shows these are the ONLY two methods ever OSR-entered
/// during the run; `CRATONVM_JIT_OSR=0` (disabling OSR VM-wide) makes all 5
/// failures disappear with no other behavior change. Both overloads are
/// self-recursive (standard dual-pivot partitioning), and an OSR-entered
/// frame's `stack_floor_slot_off` is seeded to the OSR trampoline's
/// usize::MAX sentinel (see `emit_osr_trampoline`), which makes every
/// self-recursive call site's inline fast-path check
/// (`RSP > floor`) always false -- so an OSR-entered instance of either
/// method ALWAYS routes its recursive calls through the `self_call_stack_guard`
/// helper path (jit/src/x64.rs, the `guard_skip_patch` block), a
/// combination (OSR entry + self-recursion) that gets far less exercise than
/// either feature alone. That is the leading suspect, not a pinpointed
/// single instruction -- this is a scoped mitigation, not a root-cause fix.
fn statically_osr_denied(key: &MethodKey) -> bool {
    key.class_name == "java/util/DualPivotQuicksort"
        && key.method_name == "sort"
        && (key.descriptor == "([DIII)V"
            || key.descriptor == "(Ljava/util/DualPivotQuicksort$Sorter;[DIII)V")
}

/// Returns true when OSR is permanently disabled for this method, without
/// affecting normal invocation-counted JIT compilation.
pub fn is_osr_denied(key: &MethodKey) -> bool {
    statically_osr_denied(key) || osr_deny_list().read().contains(key)
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
            c1_threshold: 200,
            c2_threshold: 5_000,
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
    /// | `CRATONVM_TIER_C1_THRESHOLD`       | `c1_threshold`       | 200     |
    /// | `CRATONVM_TIER_C2_THRESHOLD`       | `c2_threshold`       | 5000    |
    /// | `CRATONVM_TIER_OSR_THRESHOLD`      | `osr_threshold`      | 10000   |
    /// | `CRATONVM_TIER_C2_MIN_INVOCATIONS` | `c2_min_invocations` | 1000    |
    /// | `CRATONVM_TIER_ENABLED=0`          | `tiered_enabled`     | true    |
    ///
    /// (The per-frame back-edge OSR trigger — `Frame::should_try_osr` — is a
    /// separate live knob, `CRATONVM_TIER_OSR_BACKEDGE`, read VM-side because it
    /// is consulted on the default path too, not only under the tiered manager.)
    pub fn from_env() -> Self {
        Self::with_overrides(|name| std::env::var(name).ok())
    }

    /// Testable core of [`from_env`]: apply the `CRATONVM_TIER_*` overrides
    /// resolved through `get` (production passes `std::env::var`). Each numeric
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
    pub tier_fail_count: u32,
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
#[derive(Debug, Clone)]
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
    fn complete_task(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        compile_time_ms: u64,
        success: bool,
    ) {
        {
            let mut methods = self.methods.lock();
            if let Some(state) = methods.get_mut(key) {
                if success {
                    state.current_tier = tier;
                    state.tier_fail_count = 0;
                } else {
                    state.tier_fail_count = state.tier_fail_count.saturating_add(1);
                }
                state.queued_for_compilation = false;
                state.queued_tier = None;
                state.last_compile_time_ms = compile_time_ms;
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

/// Maximum number of receiver types tracked per call site.
const MAX_RECEIVER_TYPES: usize = 3;

impl TieredCompilationManager {
    /// Create a new manager with the given policy.
    pub fn new(policy: CompilationPolicy) -> Self {
        Self {
            core: Arc::new(CompilerCore::new()),
            policy: Mutex::new(policy),
        }
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
    /// queued, already at/above C2, or has bailed out of C2. The enqueued task
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
            || state.current_tier >= CompilationTier::C2
            || state.c2_bailout
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
        self.core.complete_task(key, tier, compile_time_ms, true);
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
        self.core.enqueue(task);
        Some(target)
    }

    /// Pure policy check: determine if the method should be compiled (and at what tier).
    fn should_compile(
        &self,
        state: &MethodState,
        policy: &CompilationPolicy,
    ) -> Option<CompilationTier> {
        // Give up after repeated compile-attempt failures (the attempt ran
        // but never published a body — see `complete_task`), matching the
        // "3+ deopts" convention `c2_bailout` already uses below. Without
        // this, a method whose compile step keeps failing for a reason
        // outside the (fast) permanent bail-list — e.g. a transient
        // code-cache-cap or in-flight class redefine — would be
        // re-recommended and re-enqueued on every single invocation
        // forever, since a failed attempt no longer advances `current_tier`.
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
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> MethodKey {
        MethodKey::new("java/lang/String", "hashCode", "()I")
    }

    fn test_key2() -> MethodKey {
        MethodKey::new(
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
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

    #[test]
    fn step5_request_osr_enqueues_osr_task_immediately() {
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
        clear_osr_deny_list_for_test();
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
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
    fn step5_request_osr_skips_when_already_c2_or_bailed() {
        clear_osr_deny_list_for_test();
        // Already at C2 → nothing to OSR-compile. `compilation_complete` /
        // `on_c2_bailout` use `get_mut` (no-op on an unseen method), so the
        // method must first be registered via `on_method_invocation`.
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        assert!(
            mgr.request_osr(&key, 7).is_none(),
            "C2 method: no OSR enqueue"
        );

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
        assert_eq!(p.c1_threshold, 200);
        assert_eq!(p.c2_threshold, 5_000);
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
        let mut triggered = None;
        for _ in 0..200 {
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
        mgr.core.complete_task(&key, CompilationTier::C1, 10, false);
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
            mgr.core.complete_task(&key, CompilationTier::C1, 1, false);
            assert_eq!(
                mgr.on_method_invocation(&key),
                Some(CompilationTier::C1),
                "attempt {i}: should still be retried below the fail limit"
            );
        }
        // One more failure reaches MAX_TIER_FAIL_RETRIES — should_compile
        // must now give up permanently.
        mgr.core.complete_task(&key, CompilationTier::C1, 1, false);
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
        mgr.core.complete_task(&key, CompilationTier::C1, 1, false);
        mgr.core.complete_task(&key, CompilationTier::C1, 5, true);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
        let methods = mgr.core.methods.lock();
        assert_eq!(
            methods[&key].tier_fail_count, 0,
            "a later success should reset the fail streak"
        );
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
        assert_eq!(p.c1_threshold, 200);
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
