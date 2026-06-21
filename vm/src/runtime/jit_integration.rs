// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT-to-interpreter integration layer.
//!
//! Provides invocation counters, code cache, OSR support,
//! deoptimization management, and inline caching to bridge
//! compiled native code with the bytecode interpreter.

use std::collections::{HashMap, HashSet, VecDeque};

use rustc_hash::{FxHashMap, FxHashSet};

// ---------------------------------------------------------------------------
// Compilation Trigger / Tier
// ---------------------------------------------------------------------------

/// Result of incrementing an invocation or backedge counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilationTrigger {
    None,
    CompileC1,
    CompileC2,
    OsrCompile,
    Recompile,
}

/// Compilation tier for a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompilationTier {
    Interpreter,
    C1,
    C1WithProfiling,
    C2,
}

// ---------------------------------------------------------------------------
// Invocation Counters
// ---------------------------------------------------------------------------

/// Per-method invocation and backedge counters that drive tiered compilation.
#[derive(Debug, Clone)]
pub struct InvocationCounter {
    pub count: u32,
    pub backedge_count: u32,
    pub c1_threshold: u32,
    pub c2_threshold: u32,
    pub osr_threshold: u32,
}

impl Default for InvocationCounter {
    fn default() -> Self {
        Self {
            count: 0,
            backedge_count: 0,
            c1_threshold: 2000,
            c2_threshold: 15000,
            osr_threshold: 10000,
        }
    }
}

impl InvocationCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the invocation count and return any compilation trigger.
    pub fn increment(&mut self) -> CompilationTrigger {
        self.count = self.count.saturating_add(1);
        let total = self.total();
        if total == self.c2_threshold {
            CompilationTrigger::CompileC2
        } else if total == self.c1_threshold {
            CompilationTrigger::CompileC1
        } else {
            CompilationTrigger::None
        }
    }

    /// Increment the backedge count and return any compilation trigger.
    pub fn increment_backedge(&mut self) -> CompilationTrigger {
        self.backedge_count = self.backedge_count.saturating_add(1);
        let total = self.total();
        if self.backedge_count == self.osr_threshold {
            CompilationTrigger::OsrCompile
        } else if total == self.c2_threshold {
            CompilationTrigger::CompileC2
        } else if total == self.c1_threshold {
            CompilationTrigger::CompileC1
        } else {
            CompilationTrigger::None
        }
    }

    /// Reset all counters.
    pub fn reset(&mut self) {
        self.count = 0;
        self.backedge_count = 0;
    }

    /// Combined invocation + backedge total.
    pub fn total(&self) -> u32 {
        self.count.saturating_add(self.backedge_count)
    }
}

// ---------------------------------------------------------------------------
// Method Compilation State
// ---------------------------------------------------------------------------

/// Represents a compiled form of a Java method.
#[derive(Debug, Clone)]
pub struct CompiledMethodState {
    pub method_id: u64,
    pub class_name: String,
    pub method_name: String,
    pub descriptor: String,
    pub tier: CompilationTier,
    pub code_size: usize,
    pub entry_point: usize,
    pub deopt_count: u32,
    pub is_valid: bool,
}

// ---------------------------------------------------------------------------
// Code Cache
// ---------------------------------------------------------------------------

/// An entry in the code cache, augmented with access metadata for eviction.
#[derive(Debug, Clone)]
pub struct CodeCacheEntry {
    pub method: CompiledMethodState,
    pub last_access: u64,
    pub access_count: u64,
}

/// Errors returned from code cache operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeCacheError {
    Full { available: usize, needed: usize },
    DuplicateMethod(u64),
}

/// Summary statistics for the code cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeCacheStats {
    pub total_size: usize,
    pub used_size: usize,
    pub entry_count: usize,
    pub invalidated_count: usize,
    pub c1_count: usize,
    pub c2_count: usize,
}

/// Fixed-size code cache holding compiled method entries.
pub struct CodeCache {
    pub entries: Vec<CodeCacheEntry>,
    pub total_size: usize,
    pub max_size: usize,
    pub used_size: usize,
    access_clock: u64,
}

impl CodeCache {
    /// 240 MB default max size.
    const DEFAULT_MAX: usize = 240 * 1024 * 1024;

    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            total_size: Self::DEFAULT_MAX,
            max_size: Self::DEFAULT_MAX,
            used_size: 0,
            access_clock: 0,
        }
    }

    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            entries: Vec::new(),
            total_size: max_size,
            max_size,
            used_size: 0,
            access_clock: 0,
        }
    }

    fn next_clock(&mut self) -> u64 {
        self.access_clock += 1;
        self.access_clock
    }

    /// Install a compiled method into the cache. Returns the entry index.
    pub fn install(&mut self, method: CompiledMethodState) -> Result<usize, CodeCacheError> {
        // Check for duplicate
        if self
            .entries
            .iter()
            .any(|e| e.method.method_id == method.method_id)
        {
            return Err(CodeCacheError::DuplicateMethod(method.method_id));
        }
        let needed = method.code_size;
        if self.used_size + needed > self.max_size {
            return Err(CodeCacheError::Full {
                available: self.max_size.saturating_sub(self.used_size),
                needed,
            });
        }
        let clock = self.next_clock();
        self.used_size += needed;
        self.entries.push(CodeCacheEntry {
            method,
            last_access: clock,
            access_count: 1,
        });
        Ok(self.entries.len() - 1)
    }

    /// Invalidate a compiled method by id. Returns true if found.
    pub fn invalidate(&mut self, method_id: u64) -> bool {
        for entry in &mut self.entries {
            if entry.method.method_id == method_id {
                entry.method.is_valid = false;
                return true;
            }
        }
        false
    }

    /// Look up a compiled method by id.
    pub fn lookup(&self, method_id: u64) -> Option<&CompiledMethodState> {
        self.entries
            .iter()
            .find(|e| e.method.method_id == method_id && e.method.is_valid)
            .map(|e| &e.method)
    }

    /// Look up a compiled method by class and method name.
    pub fn lookup_by_name(&self, class: &str, method: &str) -> Option<&CompiledMethodState> {
        self.entries
            .iter()
            .find(|e| {
                e.method.is_valid && e.method.class_name == class && e.method.method_name == method
            })
            .map(|e| &e.method)
    }

    /// Evict the least-used entry (lowest access_count, break ties by oldest last_access).
    pub fn evict_least_used(&mut self) -> Option<CompiledMethodState> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self
            .entries
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.access_count
                    .cmp(&b.access_count)
                    .then(a.last_access.cmp(&b.last_access))
            })
            .map(|(i, _)| i)
            .unwrap();
        let entry = self.entries.remove(idx);
        self.used_size = self.used_size.saturating_sub(entry.method.code_size);
        Some(entry.method)
    }

    /// Return aggregate statistics.
    pub fn get_stats(&self) -> CodeCacheStats {
        let mut stats = CodeCacheStats {
            total_size: self.max_size,
            used_size: self.used_size,
            entry_count: self.entries.len(),
            invalidated_count: 0,
            c1_count: 0,
            c2_count: 0,
        };
        for e in &self.entries {
            if !e.method.is_valid {
                stats.invalidated_count += 1;
            }
            match e.method.tier {
                CompilationTier::C1 | CompilationTier::C1WithProfiling => stats.c1_count += 1,
                CompilationTier::C2 => stats.c2_count += 1,
                _ => {}
            }
        }
        stats
    }

    /// True when the cache cannot accept a single additional byte.
    pub fn is_full(&self) -> bool {
        self.used_size >= self.max_size
    }

    /// Sweep invalidated entries and return total bytes freed.
    pub fn sweep(&mut self) -> usize {
        let mut freed = 0usize;
        self.entries.retain(|e| {
            if !e.method.is_valid {
                freed += e.method.code_size;
                false
            } else {
                true
            }
        });
        self.used_size = self.used_size.saturating_sub(freed);
        freed
    }
}

impl Default for CodeCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// OSR (On-Stack Replacement)
// ---------------------------------------------------------------------------

/// Describes how a single interpreter local maps into compiled code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsrLocation {
    Register(u8),
    StackSlot(i32),
    Constant(i64),
}

/// Mapping for one local variable during OSR transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsrLocalMapping {
    pub local_index: u16,
    pub register_or_stack: OsrLocation,
}

/// A single OSR compilation entry for a (method, bci) pair.
#[derive(Debug, Clone)]
pub struct OsrEntry {
    pub compiled_entry_point: usize,
    pub bci: u32,
    pub local_mapping: Vec<OsrLocalMapping>,
    pub tier: CompilationTier,
}

/// Manages OSR entries keyed by (method_id, bci).
/// T10.9.B: FxHashMap — keys are internal (method_id, bci) tuples.
pub struct OsrManager {
    pub osr_entries: FxHashMap<(u64, u32), OsrEntry>,
}

impl OsrManager {
    pub fn new() -> Self {
        Self {
            osr_entries: FxHashMap::default(),
        }
    }

    pub fn register_osr(&mut self, method_id: u64, bci: u32, entry: OsrEntry) {
        self.osr_entries.insert((method_id, bci), entry);
    }

    pub fn lookup_osr(&self, method_id: u64, bci: u32) -> Option<&OsrEntry> {
        self.osr_entries.get(&(method_id, bci))
    }

    /// Remove all OSR entries for a method. Returns the number removed.
    pub fn remove_osr(&mut self, method_id: u64) -> usize {
        let before = self.osr_entries.len();
        self.osr_entries.retain(|&(mid, _), _| mid != method_id);
        before - self.osr_entries.len()
    }

    pub fn osr_count(&self) -> usize {
        self.osr_entries.len()
    }
}

impl Default for OsrManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Deoptimization
// ---------------------------------------------------------------------------

/// Reason a compiled method was deoptimized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeoptReason {
    NullCheck,
    ClassCheck,
    BoundsCheck,
    DivByZero,
    UnreachedCode,
    Uninitialized,
    ConstraintViolation,
    TransferToInterpreter,
}

impl DeoptReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::NullCheck => "NullCheck",
            Self::ClassCheck => "ClassCheck",
            Self::BoundsCheck => "BoundsCheck",
            Self::DivByZero => "DivByZero",
            Self::UnreachedCode => "UnreachedCode",
            Self::Uninitialized => "Uninitialized",
            Self::ConstraintViolation => "ConstraintViolation",
            Self::TransferToInterpreter => "TransferToInterpreter",
        }
    }
}

/// Action to take after a deoptimization event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptAction {
    Recompile,
    RecompileWithProfile,
    InterpretForever,
    None,
}

/// Record of a single deoptimization event.
#[derive(Debug, Clone)]
pub struct DeoptEvent {
    pub method_id: u64,
    pub reason: DeoptReason,
    pub bci: u32,
    pub timestamp: u64,
}

/// A queued request to recompile a deoptimized method.
#[derive(Debug, Clone)]
pub struct RecompilationRequest {
    pub method_id: u64,
    pub reason: DeoptReason,
    pub target_tier: CompilationTier,
    pub priority: u8,
}

/// Summary statistics for the deoptimization subsystem.
#[derive(Debug, Clone)]
pub struct DeoptStats {
    pub total_deopts: u64,
    pub recompilations: u64,
    pub blacklisted: usize,
    /// T10.9.B: FxHashMap — reason strings are internal.
    pub by_reason: FxHashMap<String, u64>,
}

/// Tracks deoptimization events and decides when to blacklist methods.
/// T10.9.B: FxHashSet — method_id is internal packed class+method.
pub struct DeoptimizationManager {
    pub deopt_count: u64,
    pub recompilation_queue: Vec<RecompilationRequest>,
    /// Bounded ring of recent events (oldest trimmed once it exceeds
    /// `MAX_DEOPT_HISTORY`). Purely diagnostic — no decision reads its length,
    /// so trimming never affects observable deopt behavior.
    pub deopt_history: VecDeque<DeoptEvent>,
    /// O(1) per-method deopt counter driving blacklist/recompile decisions.
    /// Never trimmed, so decisions are independent of `deopt_history` bounding.
    deopt_count_by_method: FxHashMap<u64, u32>,
    /// Incrementally maintained per-reason tally for `get_stats` (avoids a full
    /// `deopt_history` rescan and survives history trimming).
    deopt_count_by_reason: FxHashMap<String, u64>,
    pub max_deopts_before_blacklist: u32,
    pub blacklisted_methods: FxHashSet<u64>,
}

/// Upper bound on retained deopt events. Older events are dropped from the
/// front once this is exceeded; decision counters are kept separately and are
/// unaffected by trimming.
const MAX_DEOPT_HISTORY: usize = 4096;

impl DeoptimizationManager {
    pub fn new() -> Self {
        Self {
            deopt_count: 0,
            recompilation_queue: Vec::new(),
            deopt_history: VecDeque::new(),
            deopt_count_by_method: FxHashMap::default(),
            deopt_count_by_reason: FxHashMap::default(),
            max_deopts_before_blacklist: 10,
            blacklisted_methods: FxHashSet::default(),
        }
    }

    /// Record a deoptimization event and return the recommended action.
    pub fn record_deopt(&mut self, event: DeoptEvent) -> DeoptAction {
        let method_id = event.method_id;
        self.deopt_count += 1;

        // Maintain the per-reason tally incrementally so get_stats() is O(1)
        // per reason rather than an O(history) rescan, and so it stays accurate
        // after the bounded history trims old events below.
        *self
            .deopt_count_by_reason
            .entry(event.reason.as_str().to_string())
            .or_insert(0) += 1;

        // Bounded ring: drop the oldest event once the cap is exceeded. Only
        // the diagnostic history is trimmed — the decision counters below are
        // kept separately and never trimmed, so behavior is unchanged.
        self.deopt_history.push_back(event.clone());
        if self.deopt_history.len() > MAX_DEOPT_HISTORY {
            self.deopt_history.pop_front();
        }

        // O(1) per-method counter replaces the former O(n) history rescan.
        // Counted whether or not the method is already blacklisted, matching
        // the prior behavior where the just-pushed event was always included
        // in the linear count.
        let method_deopts = {
            let c = self.deopt_count_by_method.entry(method_id).or_insert(0);
            *c += 1;
            *c
        };

        if self.blacklisted_methods.contains(&method_id) {
            return DeoptAction::InterpretForever;
        }

        if method_deopts >= self.max_deopts_before_blacklist {
            self.blacklisted_methods.insert(method_id);
            return DeoptAction::InterpretForever;
        }

        if method_deopts > 3 {
            self.recompilation_queue.push(RecompilationRequest {
                method_id,
                reason: event.reason,
                target_tier: CompilationTier::C1WithProfiling,
                priority: 1,
            });
            DeoptAction::RecompileWithProfile
        } else {
            self.recompilation_queue.push(RecompilationRequest {
                method_id,
                reason: event.reason,
                target_tier: CompilationTier::C2,
                priority: 2,
            });
            DeoptAction::Recompile
        }
    }

    /// Whether the method should be recompiled (i.e. is not blacklisted and has pending deopts).
    pub fn should_recompile(&self, method_id: u64) -> bool {
        if self.blacklisted_methods.contains(&method_id) {
            return false;
        }
        self.recompilation_queue
            .iter()
            .any(|r| r.method_id == method_id)
    }

    pub fn is_blacklisted(&self, method_id: u64) -> bool {
        self.blacklisted_methods.contains(&method_id)
    }

    pub fn get_stats(&self) -> DeoptStats {
        DeoptStats {
            total_deopts: self.deopt_count,
            recompilations: self.recompilation_queue.len() as u64,
            blacklisted: self.blacklisted_methods.len(),
            // Incrementally maintained, so this reflects every recorded deopt
            // even after the bounded history has trimmed old events.
            by_reason: self.deopt_count_by_reason.clone(),
        }
    }
}

impl Default for DeoptimizationManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Inline Cache
// ---------------------------------------------------------------------------

/// State of an inline cache site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineCacheState {
    Uninitialized,
    Monomorphic,
    Bimorphic,
    Polymorphic,
    Megamorphic,
}

/// Transition result after updating an inline cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTransition {
    NoChange,
    MonomorphicHit,
    Transition(InlineCacheState, InlineCacheState),
}

/// Per-call-site inline cache.
#[derive(Debug, Clone)]
pub struct InlineCache {
    pub state: InlineCacheState,
    pub receiver_types: Vec<u32>,
    pub target_methods: Vec<u64>,
    pub miss_count: u32,
}

impl InlineCache {
    pub fn new() -> Self {
        Self {
            state: InlineCacheState::Uninitialized,
            receiver_types: Vec::new(),
            target_methods: Vec::new(),
            miss_count: 0,
        }
    }
}

impl Default for InlineCache {
    fn default() -> Self {
        Self::new()
    }
}

fn state_for_count(n: usize) -> InlineCacheState {
    match n {
        0 => InlineCacheState::Uninitialized,
        1 => InlineCacheState::Monomorphic,
        2 => InlineCacheState::Bimorphic,
        3 | 4 => InlineCacheState::Polymorphic,
        _ => InlineCacheState::Megamorphic,
    }
}

/// Manages inline caches for virtual/interface call sites.
/// T10.9.B: FxHashMap — call-site ID is internal.
pub struct InlineCacheManager {
    pub caches: FxHashMap<u64, InlineCache>,
}

impl InlineCacheManager {
    pub fn new() -> Self {
        Self {
            caches: FxHashMap::default(),
        }
    }

    /// Register (or get) the inline cache for a call site.
    pub fn register_call_site(&mut self, site_id: u64) -> &mut InlineCache {
        self.caches.entry(site_id).or_insert_with(InlineCache::new)
    }

    /// Update a call site with a new receiver type. Returns the cache transition.
    pub fn update(&mut self, site_id: u64, receiver_type: u32) -> CacheTransition {
        let cache = self.caches.entry(site_id).or_insert_with(InlineCache::new);

        // Already seen this type?
        if cache.receiver_types.contains(&receiver_type) {
            if cache.state == InlineCacheState::Monomorphic {
                return CacheTransition::MonomorphicHit;
            }
            return CacheTransition::NoChange;
        }

        // Megamorphic caches stop recording new types.
        if cache.state == InlineCacheState::Megamorphic {
            cache.miss_count += 1;
            return CacheTransition::NoChange;
        }

        let old_state = cache.state;
        cache.receiver_types.push(receiver_type);
        let new_state = state_for_count(cache.receiver_types.len());
        cache.state = new_state;

        if old_state == new_state {
            CacheTransition::NoChange
        } else {
            CacheTransition::Transition(old_state, new_state)
        }
    }

    pub fn lookup(&self, site_id: u64) -> Option<&InlineCache> {
        self.caches.get(&site_id)
    }

    pub fn total_sites(&self) -> usize {
        self.caches.len()
    }

    pub fn megamorphic_count(&self) -> usize {
        self.caches
            .values()
            .filter(|c| c.state == InlineCacheState::Megamorphic)
            .count()
    }
}

impl Default for InlineCacheManager {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers ----------------------------------------------------------

    fn make_method(id: u64, size: usize) -> CompiledMethodState {
        CompiledMethodState {
            method_id: id,
            class_name: format!("Class{}", id),
            method_name: format!("method{}", id),
            descriptor: "()V".to_string(),
            tier: CompilationTier::C1,
            code_size: size,
            entry_point: 0x1000 + id as usize,
            deopt_count: 0,
            is_valid: true,
        }
    }

    fn make_c2_method(id: u64, size: usize) -> CompiledMethodState {
        CompiledMethodState {
            tier: CompilationTier::C2,
            ..make_method(id, size)
        }
    }

    // =====================================================================
    // Invocation Counters
    // =====================================================================

    #[test]
    fn counter_default_thresholds() {
        let c = InvocationCounter::new();
        assert_eq!(c.c1_threshold, 2000);
        assert_eq!(c.c2_threshold, 15000);
        assert_eq!(c.osr_threshold, 10000);
    }

    #[test]
    fn counter_increment_to_c1() {
        let mut c = InvocationCounter::new();
        for _ in 0..1999 {
            assert_eq!(c.increment(), CompilationTrigger::None);
        }
        assert_eq!(c.increment(), CompilationTrigger::CompileC1);
    }

    #[test]
    fn counter_increment_to_c2() {
        let mut c = InvocationCounter::new();
        for _ in 0..14999 {
            c.increment();
        }
        assert_eq!(c.increment(), CompilationTrigger::CompileC2);
    }

    #[test]
    fn counter_backedge_osr() {
        let mut c = InvocationCounter::new();
        for _ in 0..9999 {
            assert_ne!(c.increment_backedge(), CompilationTrigger::OsrCompile);
        }
        assert_eq!(c.increment_backedge(), CompilationTrigger::OsrCompile);
    }

    #[test]
    fn counter_reset() {
        let mut c = InvocationCounter::new();
        for _ in 0..500 {
            c.increment();
        }
        c.reset();
        assert_eq!(c.count, 0);
        assert_eq!(c.backedge_count, 0);
        assert_eq!(c.total(), 0);
    }

    #[test]
    fn counter_total_combines() {
        let mut c = InvocationCounter::new();
        for _ in 0..100 {
            c.increment();
        }
        for _ in 0..50 {
            c.increment_backedge();
        }
        assert_eq!(c.total(), 150);
    }

    #[test]
    fn counter_backedge_c1_trigger() {
        let mut c = InvocationCounter::new();
        // Drive count to 1999 via invocations, then one backedge hits c1
        for _ in 0..1999 {
            c.increment();
        }
        assert_eq!(c.increment_backedge(), CompilationTrigger::CompileC1);
    }

    // =====================================================================
    // Code Cache
    // =====================================================================

    #[test]
    fn cache_install_and_lookup() {
        let mut cache = CodeCache::with_max_size(1024);
        let m = make_method(1, 100);
        let idx = cache.install(m).unwrap();
        assert_eq!(idx, 0);
        assert!(cache.lookup(1).is_some());
        assert_eq!(cache.lookup(1).unwrap().method_id, 1);
    }

    #[test]
    fn cache_duplicate_rejected() {
        let mut cache = CodeCache::with_max_size(1024);
        cache.install(make_method(1, 100)).unwrap();
        let err = cache.install(make_method(1, 100)).unwrap_err();
        assert_eq!(err, CodeCacheError::DuplicateMethod(1));
    }

    #[test]
    fn cache_full_rejected() {
        let mut cache = CodeCache::with_max_size(200);
        cache.install(make_method(1, 150)).unwrap();
        let err = cache.install(make_method(2, 100)).unwrap_err();
        assert_eq!(
            err,
            CodeCacheError::Full {
                available: 50,
                needed: 100
            }
        );
    }

    #[test]
    fn cache_invalidate() {
        let mut cache = CodeCache::with_max_size(1024);
        cache.install(make_method(1, 100)).unwrap();
        assert!(cache.invalidate(1));
        assert!(cache.lookup(1).is_none()); // invalid entries hidden from lookup
    }

    #[test]
    fn cache_invalidate_missing() {
        let mut cache = CodeCache::with_max_size(1024);
        assert!(!cache.invalidate(999));
    }

    #[test]
    fn cache_lookup_by_name() {
        let mut cache = CodeCache::with_max_size(1024);
        cache.install(make_method(1, 100)).unwrap();
        let found = cache.lookup_by_name("Class1", "method1");
        assert!(found.is_some());
        assert!(cache.lookup_by_name("ClassX", "methodX").is_none());
    }

    #[test]
    fn cache_evict_least_used() {
        let mut cache = CodeCache::with_max_size(4096);
        cache.install(make_method(1, 100)).unwrap();
        cache.install(make_method(2, 200)).unwrap();
        // Method 1 has access_count = 1 and lowest last_access → evicted first
        let evicted = cache.evict_least_used().unwrap();
        assert_eq!(evicted.method_id, 1);
        assert_eq!(cache.used_size, 200);
    }

    #[test]
    fn cache_evict_empty() {
        let mut cache = CodeCache::with_max_size(1024);
        assert!(cache.evict_least_used().is_none());
    }

    #[test]
    fn cache_sweep() {
        let mut cache = CodeCache::with_max_size(4096);
        cache.install(make_method(1, 100)).unwrap();
        cache.install(make_method(2, 200)).unwrap();
        cache.invalidate(1);
        let freed = cache.sweep();
        assert_eq!(freed, 100);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.used_size, 200);
    }

    #[test]
    fn cache_sweep_nothing() {
        let mut cache = CodeCache::with_max_size(4096);
        cache.install(make_method(1, 100)).unwrap();
        let freed = cache.sweep();
        assert_eq!(freed, 0);
    }

    #[test]
    fn cache_is_full() {
        let mut cache = CodeCache::with_max_size(100);
        assert!(!cache.is_full());
        cache.install(make_method(1, 100)).unwrap();
        assert!(cache.is_full());
    }

    #[test]
    fn cache_stats() {
        let mut cache = CodeCache::with_max_size(4096);
        cache.install(make_method(1, 100)).unwrap();
        cache.install(make_c2_method(2, 200)).unwrap();
        cache.invalidate(1);
        let stats = cache.get_stats();
        assert_eq!(stats.entry_count, 2);
        assert_eq!(stats.invalidated_count, 1);
        assert_eq!(stats.c1_count, 1);
        assert_eq!(stats.c2_count, 1);
        assert_eq!(stats.used_size, 300);
    }

    #[test]
    fn cache_default_max_size() {
        let cache = CodeCache::new();
        assert_eq!(cache.max_size, 240 * 1024 * 1024);
    }

    // =====================================================================
    // OSR
    // =====================================================================

    #[test]
    fn osr_register_and_lookup() {
        let mut mgr = OsrManager::new();
        let entry = OsrEntry {
            compiled_entry_point: 0x5000,
            bci: 42,
            local_mapping: vec![OsrLocalMapping {
                local_index: 0,
                register_or_stack: OsrLocation::Register(0),
            }],
            tier: CompilationTier::C2,
        };
        mgr.register_osr(1, 42, entry);
        assert!(mgr.lookup_osr(1, 42).is_some());
        assert_eq!(mgr.lookup_osr(1, 42).unwrap().compiled_entry_point, 0x5000);
    }

    #[test]
    fn osr_lookup_missing() {
        let mgr = OsrManager::new();
        assert!(mgr.lookup_osr(1, 0).is_none());
    }

    #[test]
    fn osr_remove() {
        let mut mgr = OsrManager::new();
        let entry = OsrEntry {
            compiled_entry_point: 0x5000,
            bci: 10,
            local_mapping: vec![],
            tier: CompilationTier::C1,
        };
        mgr.register_osr(1, 10, entry.clone());
        mgr.register_osr(
            1,
            20,
            OsrEntry {
                bci: 20,
                ..entry.clone()
            },
        );
        mgr.register_osr(2, 10, OsrEntry { bci: 10, ..entry });
        let removed = mgr.remove_osr(1);
        assert_eq!(removed, 2);
        assert_eq!(mgr.osr_count(), 1);
    }

    #[test]
    fn osr_count() {
        let mut mgr = OsrManager::new();
        assert_eq!(mgr.osr_count(), 0);
        let entry = OsrEntry {
            compiled_entry_point: 0,
            bci: 0,
            local_mapping: vec![],
            tier: CompilationTier::C1,
        };
        mgr.register_osr(1, 0, entry.clone());
        mgr.register_osr(2, 5, entry);
        assert_eq!(mgr.osr_count(), 2);
    }

    #[test]
    fn osr_local_mapping_variants() {
        let mappings = vec![
            OsrLocalMapping {
                local_index: 0,
                register_or_stack: OsrLocation::Register(3),
            },
            OsrLocalMapping {
                local_index: 1,
                register_or_stack: OsrLocation::StackSlot(-8),
            },
            OsrLocalMapping {
                local_index: 2,
                register_or_stack: OsrLocation::Constant(42),
            },
        ];
        assert_eq!(mappings[0].register_or_stack, OsrLocation::Register(3));
        assert_eq!(mappings[1].register_or_stack, OsrLocation::StackSlot(-8));
        assert_eq!(mappings[2].register_or_stack, OsrLocation::Constant(42));
    }

    // =====================================================================
    // Deoptimization
    // =====================================================================

    #[test]
    fn deopt_record_recompile() {
        let mut mgr = DeoptimizationManager::new();
        let event = DeoptEvent {
            method_id: 1,
            reason: DeoptReason::NullCheck,
            bci: 10,
            timestamp: 1,
        };
        let action = mgr.record_deopt(event);
        assert_eq!(action, DeoptAction::Recompile);
        assert_eq!(mgr.deopt_count, 1);
    }

    #[test]
    fn deopt_recompile_with_profile_after_many() {
        let mut mgr = DeoptimizationManager::new();
        for i in 0..4 {
            mgr.record_deopt(DeoptEvent {
                method_id: 1,
                reason: DeoptReason::ClassCheck,
                bci: 0,
                timestamp: i,
            });
        }
        // 4th deopt for the method (count > 3) → RecompileWithProfile
        assert_eq!(
            mgr.record_deopt(DeoptEvent {
                method_id: 1,
                reason: DeoptReason::ClassCheck,
                bci: 0,
                timestamp: 5,
            }),
            DeoptAction::RecompileWithProfile
        );
    }

    #[test]
    fn deopt_blacklist() {
        let mut mgr = DeoptimizationManager::new();
        for i in 0..10 {
            mgr.record_deopt(DeoptEvent {
                method_id: 1,
                reason: DeoptReason::BoundsCheck,
                bci: 0,
                timestamp: i,
            });
        }
        assert!(mgr.is_blacklisted(1));
        let action = mgr.record_deopt(DeoptEvent {
            method_id: 1,
            reason: DeoptReason::BoundsCheck,
            bci: 0,
            timestamp: 100,
        });
        assert_eq!(action, DeoptAction::InterpretForever);
    }

    #[test]
    fn deopt_not_blacklisted() {
        let mgr = DeoptimizationManager::new();
        assert!(!mgr.is_blacklisted(42));
    }

    #[test]
    fn deopt_should_recompile() {
        let mut mgr = DeoptimizationManager::new();
        mgr.record_deopt(DeoptEvent {
            method_id: 5,
            reason: DeoptReason::DivByZero,
            bci: 0,
            timestamp: 0,
        });
        assert!(mgr.should_recompile(5));
        assert!(!mgr.should_recompile(99));
    }

    #[test]
    fn deopt_should_not_recompile_blacklisted() {
        let mut mgr = DeoptimizationManager::new();
        for i in 0..10 {
            mgr.record_deopt(DeoptEvent {
                method_id: 1,
                reason: DeoptReason::NullCheck,
                bci: 0,
                timestamp: i,
            });
        }
        assert!(!mgr.should_recompile(1));
    }

    #[test]
    fn deopt_stats() {
        let mut mgr = DeoptimizationManager::new();
        mgr.record_deopt(DeoptEvent {
            method_id: 1,
            reason: DeoptReason::NullCheck,
            bci: 0,
            timestamp: 0,
        });
        mgr.record_deopt(DeoptEvent {
            method_id: 2,
            reason: DeoptReason::NullCheck,
            bci: 0,
            timestamp: 1,
        });
        let stats = mgr.get_stats();
        assert_eq!(stats.total_deopts, 2);
        assert_eq!(*stats.by_reason.get("NullCheck").unwrap(), 2);
    }

    #[test]
    fn deopt_reason_variants() {
        // Make sure all variants are constructible
        let reasons = [
            DeoptReason::NullCheck,
            DeoptReason::ClassCheck,
            DeoptReason::BoundsCheck,
            DeoptReason::DivByZero,
            DeoptReason::UnreachedCode,
            DeoptReason::Uninitialized,
            DeoptReason::ConstraintViolation,
            DeoptReason::TransferToInterpreter,
        ];
        assert_eq!(reasons.len(), 8);
    }

    // =====================================================================
    // Inline Cache
    // =====================================================================

    #[test]
    fn ic_register_call_site() {
        let mut mgr = InlineCacheManager::new();
        let cache = mgr.register_call_site(1);
        assert_eq!(cache.state, InlineCacheState::Uninitialized);
        assert_eq!(mgr.total_sites(), 1);
    }

    #[test]
    fn ic_monomorphic() {
        let mut mgr = InlineCacheManager::new();
        let t = mgr.update(1, 100);
        assert_eq!(
            t,
            CacheTransition::Transition(
                InlineCacheState::Uninitialized,
                InlineCacheState::Monomorphic
            )
        );
    }

    #[test]
    fn ic_monomorphic_hit() {
        let mut mgr = InlineCacheManager::new();
        mgr.update(1, 100);
        let t = mgr.update(1, 100);
        assert_eq!(t, CacheTransition::MonomorphicHit);
    }

    #[test]
    fn ic_bimorphic() {
        let mut mgr = InlineCacheManager::new();
        mgr.update(1, 100);
        let t = mgr.update(1, 200);
        assert_eq!(
            t,
            CacheTransition::Transition(InlineCacheState::Monomorphic, InlineCacheState::Bimorphic)
        );
    }

    #[test]
    fn ic_polymorphic() {
        let mut mgr = InlineCacheManager::new();
        mgr.update(1, 100);
        mgr.update(1, 200);
        let t = mgr.update(1, 300);
        assert_eq!(
            t,
            CacheTransition::Transition(InlineCacheState::Bimorphic, InlineCacheState::Polymorphic)
        );
    }

    #[test]
    fn ic_polymorphic_4_types() {
        let mut mgr = InlineCacheManager::new();
        mgr.update(1, 100);
        mgr.update(1, 200);
        mgr.update(1, 300);
        let t = mgr.update(1, 400);
        // 4 types is still Polymorphic
        assert_eq!(t, CacheTransition::NoChange);
        assert_eq!(mgr.lookup(1).unwrap().state, InlineCacheState::Polymorphic);
    }

    #[test]
    fn ic_megamorphic() {
        let mut mgr = InlineCacheManager::new();
        for i in 0..5 {
            mgr.update(1, i);
        }
        assert_eq!(mgr.lookup(1).unwrap().state, InlineCacheState::Megamorphic);
    }

    #[test]
    fn ic_megamorphic_stops_recording() {
        let mut mgr = InlineCacheManager::new();
        for i in 0..5 {
            mgr.update(1, i);
        }
        let t = mgr.update(1, 999);
        assert_eq!(t, CacheTransition::NoChange);
        // Should NOT have added the 6th type
        assert_eq!(mgr.lookup(1).unwrap().receiver_types.len(), 5);
    }

    #[test]
    fn ic_megamorphic_count() {
        let mut mgr = InlineCacheManager::new();
        for i in 0..5 {
            mgr.update(1, i);
        }
        mgr.update(2, 100); // monomorphic site
        assert_eq!(mgr.megamorphic_count(), 1);
    }

    #[test]
    fn ic_lookup_missing() {
        let mgr = InlineCacheManager::new();
        assert!(mgr.lookup(42).is_none());
    }

    #[test]
    fn ic_total_sites() {
        let mut mgr = InlineCacheManager::new();
        mgr.update(1, 10);
        mgr.update(2, 20);
        mgr.update(3, 30);
        assert_eq!(mgr.total_sites(), 3);
    }

    // =====================================================================
    // Additional edge case / integration tests
    // =====================================================================

    #[test]
    fn compiled_method_state_fields() {
        let m = make_method(42, 1024);
        assert_eq!(m.method_id, 42);
        assert_eq!(m.class_name, "Class42");
        assert_eq!(m.code_size, 1024);
        assert!(m.is_valid);
        assert_eq!(m.deopt_count, 0);
    }

    #[test]
    fn compilation_tier_equality() {
        assert_ne!(CompilationTier::C1, CompilationTier::C2);
        assert_eq!(CompilationTier::Interpreter, CompilationTier::Interpreter);
    }

    #[test]
    fn cache_multiple_install_lookup() {
        let mut cache = CodeCache::with_max_size(10000);
        for i in 0..10 {
            cache.install(make_method(i, 100)).unwrap();
        }
        assert_eq!(cache.entries.len(), 10);
        assert_eq!(cache.used_size, 1000);
        for i in 0..10 {
            assert!(cache.lookup(i).is_some());
        }
    }

    #[test]
    fn cache_sweep_frees_space_for_new_install() {
        let mut cache = CodeCache::with_max_size(300);
        cache.install(make_method(1, 200)).unwrap();
        cache.install(make_method(2, 100)).unwrap();
        // Cache is full
        assert!(cache.is_full());
        cache.invalidate(1);
        cache.sweep();
        assert_eq!(cache.used_size, 100);
        // Now we can install another
        cache.install(make_method(3, 150)).unwrap();
        assert_eq!(cache.used_size, 250);
    }

    #[test]
    fn osr_overwrite_entry() {
        let mut mgr = OsrManager::new();
        let e1 = OsrEntry {
            compiled_entry_point: 0x1000,
            bci: 10,
            local_mapping: vec![],
            tier: CompilationTier::C1,
        };
        let e2 = OsrEntry {
            compiled_entry_point: 0x2000,
            bci: 10,
            local_mapping: vec![],
            tier: CompilationTier::C2,
        };
        mgr.register_osr(1, 10, e1);
        mgr.register_osr(1, 10, e2);
        assert_eq!(mgr.osr_count(), 1);
        assert_eq!(mgr.lookup_osr(1, 10).unwrap().compiled_entry_point, 0x2000);
    }

    #[test]
    fn deopt_history_is_bounded() {
        let mut mgr = DeoptimizationManager::new();
        let n = (MAX_DEOPT_HISTORY as u64) + 100;
        for i in 0..n {
            mgr.record_deopt(DeoptEvent {
                method_id: i, // distinct ids so none is blacklisted
                reason: DeoptReason::NullCheck,
                bci: 0,
                timestamp: i,
            });
        }
        // History is capped, but the running total still counts every event.
        assert_eq!(mgr.deopt_history.len(), MAX_DEOPT_HISTORY);
        assert_eq!(mgr.deopt_count, n);
        // Per-reason stats survive history trimming (not a rescan of history).
        assert_eq!(*mgr.get_stats().by_reason.get("NullCheck").unwrap(), n);
    }

    #[test]
    fn deopt_decision_independent_of_history_trim() {
        // Even after the bounded history has churned past its cap on other
        // methods, the per-method counter still blacklists at the threshold.
        let mut mgr = DeoptimizationManager::new();
        for i in 0..(MAX_DEOPT_HISTORY as u64 + 50) {
            mgr.record_deopt(DeoptEvent {
                method_id: 7, // same id every time → must blacklist at 10
                reason: DeoptReason::ClassCheck,
                bci: 0,
                timestamp: i,
            });
        }
        assert!(mgr.is_blacklisted(7));
    }

    #[test]
    fn deopt_multiple_methods_independent() {
        let mut mgr = DeoptimizationManager::new();
        for i in 0..5 {
            mgr.record_deopt(DeoptEvent {
                method_id: 1,
                reason: DeoptReason::NullCheck,
                bci: 0,
                timestamp: i,
            });
        }
        // Method 2 should still get Recompile (only 1 deopt)
        let action = mgr.record_deopt(DeoptEvent {
            method_id: 2,
            reason: DeoptReason::NullCheck,
            bci: 0,
            timestamp: 100,
        });
        assert_eq!(action, DeoptAction::Recompile);
    }
}
