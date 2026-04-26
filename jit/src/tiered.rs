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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

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
    pub fn new(class_name: impl Into<String>, method_name: impl Into<String>, descriptor: impl Into<String>) -> Self {
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
    /// Per-method compilation state.
    /// T10.9.B: FxHashMap — MethodKey (internal class/name/desc) is trusted.
    methods: Mutex<FxHashMap<MethodKey, MethodState>>,
    /// Compilation queue.
    queue: Mutex<CompilationQueue>,
    /// Compilation policy.
    policy: Mutex<CompilationPolicy>,
    /// Whether the compilation thread is running.
    compiler_active: AtomicBool,
    /// Statistics.
    stats: CompilationStats,
}

/// Maximum number of deoptimizations before bailing out of C2.
const MAX_DEOPTS_BEFORE_BAILOUT: u32 = 3;

/// Maximum number of receiver types tracked per call site.
const MAX_RECEIVER_TYPES: usize = 3;

impl TieredCompilationManager {
    /// Create a new manager with the given policy.
    pub fn new(policy: CompilationPolicy) -> Self {
        Self {
            methods: Mutex::new(FxHashMap::default()),
            queue: Mutex::new(CompilationQueue::new()),
            policy: Mutex::new(policy),
            compiler_active: AtomicBool::new(false),
            stats: CompilationStats::default(),
        }
    }

    /// Create a new manager with the default policy.
    pub fn with_default_policy() -> Self {
        Self::new(CompilationPolicy::default())
    }

    // ── Invocation / back-edge hooks ─────────────────────────────────────

    /// Called on each method invocation from the interpreter.
    /// Increments the counter and checks if compilation should be triggered.
    /// Returns the target tier if compilation was enqueued.
    pub fn on_method_invocation(&self, key: &MethodKey) -> Option<CompilationTier> {
        let mut methods = self.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.invocation_count += 1;
        state.profile.profiled_invocations += 1;

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
        let mut methods = self.methods.lock();
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

            self.queue.lock().enqueue(task.clone());
            self.stats.osr_compilations.fetch_add(1, Ordering::Relaxed);
            return Some(task);
        }
        None
    }

    // ── Profile recording ────────────────────────────────────────────────

    /// Record a branch outcome for profiling.
    pub fn record_branch(&self, key: &MethodKey, bci: u32, taken: bool) {
        let mut methods = self.methods.lock();
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
        let mut methods = self.methods.lock();
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
        let mut methods = self.methods.lock();
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
        let mut methods = self.methods.lock();
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
        let mut methods = self.methods.lock();
        let state = methods
            .entry(task.method_key.clone())
            .or_insert_with(|| MethodState::new(task.method_key.clone()));
        state.queued_for_compilation = true;
        state.queued_tier = Some(task.target_tier);
        self.queue.lock().enqueue(task);
    }

    /// Dequeue the next compilation task (highest priority first).
    pub fn dequeue_compilation(&self) -> Option<CompilationTask> {
        self.queue.lock().dequeue()
    }

    /// Notify that compilation completed.
    pub fn compilation_complete(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        compile_time_ms: u64,
    ) {
        let mut methods = self.methods.lock();
        if let Some(state) = methods.get_mut(key) {
            state.current_tier = tier;
            state.queued_for_compilation = false;
            state.queued_tier = None;
            state.last_compile_time_ms = compile_time_ms;
        }

        match tier {
            CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                self.stats.c1_compilations.fetch_add(1, Ordering::Relaxed);
            }
            CompilationTier::C2 => {
                self.stats.c2_compilations.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        self.stats
            .total_compile_time_ms
            .fetch_add(compile_time_ms, Ordering::Relaxed);
    }

    // ── Deoptimization ───────────────────────────────────────────────────

    /// Notify that deoptimization occurred for the given method.
    pub fn on_deoptimization(&self, key: &MethodKey) {
        let mut methods = self.methods.lock();
        if let Some(state) = methods.get_mut(key) {
            state.deopt_count += 1;
            state.current_tier = CompilationTier::Interpreter;
            state.queued_for_compilation = false;
            state.queued_tier = None;

            if state.deopt_count >= MAX_DEOPTS_BEFORE_BAILOUT {
                state.c2_bailout = true;
                self.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.stats.deoptimizations.fetch_add(1, Ordering::Relaxed);
    }

    /// Notify that C2 compilation bailed out (method too complex, etc.).
    pub fn on_c2_bailout(&self, key: &MethodKey) {
        let mut methods = self.methods.lock();
        if let Some(state) = methods.get_mut(key) {
            state.c2_bailout = true;
            state.queued_for_compilation = false;
            state.queued_tier = None;
        }
        self.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// Get the current tier for a method.
    pub fn current_tier(&self, key: &MethodKey) -> CompilationTier {
        self.methods
            .lock()
            .get(key)
            .map(|s| s.current_tier)
            .unwrap_or(CompilationTier::Interpreter)
    }

    /// Get a clone of the profile data for a method.
    pub fn get_profile(&self, key: &MethodKey) -> Option<MethodProfile> {
        self.methods.lock().get(key).map(|s| s.profile.clone())
    }

    /// Get compilation statistics.
    pub fn stats(&self) -> &CompilationStats {
        &self.stats
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
        self.queue.lock().is_empty()
    }

    /// Get the number of tasks in the compilation queue.
    pub fn queue_size(&self) -> usize {
        self.queue.lock().len()
    }

    /// Get all method states: (key, current_tier, invocation_count).
    pub fn method_states(&self) -> Vec<(MethodKey, CompilationTier, u64)> {
        self.methods
            .lock()
            .values()
            .map(|s| (s.method_key.clone(), s.current_tier, s.invocation_count))
            .collect()
    }

    /// Whether the background compiler is active.
    pub fn compiler_active(&self) -> bool {
        self.compiler_active.load(Ordering::Relaxed)
    }

    /// Set whether the background compiler is active.
    pub fn set_compiler_active(&self, active: bool) {
        self.compiler_active.store(active, Ordering::Relaxed);
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

        self.queue.lock().enqueue(task);
        Some(target)
    }

    /// Pure policy check: determine if the method should be compiled (and at what tier).
    fn should_compile(
        &self,
        state: &MethodState,
        policy: &CompilationPolicy,
    ) -> Option<CompilationTier> {
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
        MethodKey::new("java/util/HashMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
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
        let methods = mgr.methods.lock();
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
        assert_eq!(mgr.stats().total_compile_time_ms.load(Ordering::Relaxed), 100);
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

        let q = mgr.queue.lock();
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

        let methods = mgr.methods.lock();
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
}
