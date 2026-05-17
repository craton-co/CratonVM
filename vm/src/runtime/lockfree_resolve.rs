//! Lock-free method resolution caching for the interpreter's hot path.
//!
//! This module provides a two-level resolution cache that eliminates lock
//! contention on the common case (cache hit):
//!
//! ```text
//! Resolution flow:
//! 1. Check thread-local cache (no lock) -> if hit, return immediately
//! 2. Check shared global cache (read lock) -> if hit, populate thread-local, return
//! 3. Do full resolution (class manager lock) -> populate shared + thread-local, return
//! ```
//!
//! The **ThreadLocalResolveCache** lives per-thread and requires zero
//! synchronisation.  The **SharedResolutionState** is a read-optimised global
//! cache protected by `RwLock`s so concurrent readers never block each other.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

// Round-9 MED-1: doc cleanup. `SharedResolutionState` below holds three
// `parking_lot::RwLock` maps (`global_methods`, `global_fields`,
// `promoted_invokes`); the struct definition at the bottom of this file is the
// source of truth. parking_lot drops the pthread_rwlock + poisoning overhead
// that the std lock would impose — every other workspace lock is already
// parking_lot.
use parking_lot::RwLock;

use super::fx_collections::{fx_hashmap, FxBuildHasher, FxHashMap, FxHasher};
use crate::classloading::resolution::CachedInvokeTarget;
use crate::classloading::ClassId;
use std::hash::Hasher;

// ---------------------------------------------------------------------------
// ResolutionKey
// ---------------------------------------------------------------------------

/// Resolution key -- (class_name_hash, method_name_hash, descriptor_hash).
///
/// Using pre-computed hashes avoids string comparisons in the hot path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResolutionKey {
    pub class_hash: u64,
    pub name_hash: u64,
    pub desc_hash: u64,
}

impl ResolutionKey {
    /// Build a key from the raw JVM strings.
    pub fn new(class_name: &str, method_name: &str, descriptor: &str) -> Self {
        Self {
            class_hash: fx_hash_str(class_name),
            name_hash: fx_hash_str(method_name),
            desc_hash: fx_hash_str(descriptor),
        }
    }
}

/// Hash a string slice with `FxHasher`.
#[inline]
fn fx_hash_str(s: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    h.finish()
}

// ---------------------------------------------------------------------------
// ResolvedTarget / ResolvedField
// ---------------------------------------------------------------------------

/// A resolved method target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub class_id: u64,
    pub method_index: u32,
    pub is_static: bool,
    pub is_native: bool,
    pub vtable_slot: Option<u32>,
}

/// A resolved field target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedField {
    pub class_id: u64,
    pub field_index: u32,
    pub field_offset: u32,
    pub is_static: bool,
}

// ---------------------------------------------------------------------------
// CacheStats
// ---------------------------------------------------------------------------

/// Snapshot of cache statistics.
#[derive(Clone, Debug)]
pub struct CacheStats {
    pub method_entries: usize,
    pub field_entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
}

// ---------------------------------------------------------------------------
// ThreadLocalResolveCache
// ---------------------------------------------------------------------------

/// Per-thread resolution cache.  All operations are lock-free because the
/// cache is owned exclusively by one thread.
pub struct ThreadLocalResolveCache {
    methods: FxHashMap<ResolutionKey, ResolvedTarget>,
    fields: FxHashMap<ResolutionKey, ResolvedField>,
    /// HIGH — FIFO insertion-order tracking for eviction. Previously
    /// `Vec<ResolutionKey>` with `Vec::remove(0)` on eviction, which is
    /// O(n) per eviction (full memmove of every remaining element).
    /// At a steady-state hot cache that re-evicts on every miss, this
    /// degrades to O(n) per cache miss = O(n^2) total churn. `VecDeque`
    /// gives O(1) front-pop while preserving FIFO eviction order, which
    /// is the documented intent.
    method_order: VecDeque<ResolutionKey>,
    field_order: VecDeque<ResolutionKey>,
    max_entries: usize,
    hits: u64,
    misses: u64,
}

impl ThreadLocalResolveCache {
    /// Create a new per-thread cache.  `max_entries` bounds the combined
    /// number of method **and** field entries to prevent unbounded growth.
    /// The default is 4096 entries.
    pub fn new(max_entries: usize) -> Self {
        Self {
            methods: fx_hashmap(),
            fields: fx_hashmap(),
            method_order: VecDeque::new(),
            field_order: VecDeque::new(),
            max_entries,
            hits: 0,
            misses: 0,
        }
    }

    // -- methods ----------------------------------------------------------

    /// Look up a cached method.  Returns `Some` on hit, `None` on miss.
    /// Updates hit/miss counters.
    ///
    /// PERF (round-5 vm #5): collapse the previous `contains_key` + `get`
    /// double-probe into a single `match self.methods.get(...)`. SipHash on
    /// a `ResolutionKey` is not free; doing it twice on every hit doubled
    /// the cost of the resolution fast path.
    ///
    /// Round-7 Fix 6: `#[inline]` so the resolution fast path (called on
    /// every Invoke* opcode) inlines into the dispatch loop under LTO.
    #[inline]
    pub fn get_method(&mut self, key: &ResolutionKey) -> Option<&ResolvedTarget> {
        match self.methods.get(key) {
            Some(t) => {
                self.hits += 1;
                Some(t)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Insert (or update) a method resolution result.  If the cache has
    /// reached `max_entries`, the oldest method entry is evicted first.
    pub fn put_method(&mut self, key: ResolutionKey, target: ResolvedTarget) {
        if !self.methods.contains_key(&key) {
            // Evict oldest method entry if we are at capacity. O(1)
            // per eviction via `VecDeque::pop_front` (was O(n)
            // `Vec::remove(0)`).
            while self.methods.len() + self.fields.len() >= self.max_entries
                && !self.method_order.is_empty()
            {
                if let Some(oldest) = self.method_order.pop_front() {
                    self.methods.remove(&oldest);
                }
            }
            self.method_order.push_back(key);
        }
        self.methods.insert(key, target);
    }

    // -- fields -----------------------------------------------------------

    /// Look up a cached field.  Returns `Some` on hit, `None` on miss.
    /// Updates hit/miss counters.
    ///
    /// PERF (round-5 vm #5): single-probe lookup, see `get_method`.
    ///
    /// Round-7 Fix 6: `#[inline]` for the Get/Putfield/static fast path.
    #[inline]
    pub fn get_field(&mut self, key: &ResolutionKey) -> Option<&ResolvedField> {
        match self.fields.get(key) {
            Some(t) => {
                self.hits += 1;
                Some(t)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Insert (or update) a field resolution result, evicting the oldest
    /// field entry if at capacity.
    pub fn put_field(&mut self, key: ResolutionKey, field: ResolvedField) {
        if !self.fields.contains_key(&key) {
            // O(1) front-pop eviction via `VecDeque`.
            while self.methods.len() + self.fields.len() >= self.max_entries
                && !self.field_order.is_empty()
            {
                if let Some(oldest) = self.field_order.pop_front() {
                    self.fields.remove(&oldest);
                }
            }
            self.field_order.push_back(key);
        }
        self.fields.insert(key, field);
    }

    // -- housekeeping -----------------------------------------------------

    /// Remove all entries whose `class_id` matches (supports class
    /// redefinition / hot-swap).
    pub fn invalidate_class(&mut self, class_id: u64) {
        self.methods.retain(|_, v| v.class_id != class_id);
        self.method_order
            .retain(|k| self.methods.contains_key(k));

        self.fields.retain(|_, v| v.class_id != class_id);
        self.field_order
            .retain(|k| self.fields.contains_key(k));
    }

    /// Remove every cached entry.
    pub fn clear(&mut self) {
        self.methods.clear();
        self.fields.clear();
        self.method_order.clear();
        self.field_order.clear();
        self.hits = 0;
        self.misses = 0;
    }

    /// Hit rate as a fraction in `[0.0, 1.0]`.  Returns 0.0 when there
    /// have been no accesses.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// Total number of cached entries (methods + fields).
    pub fn len(&self) -> usize {
        self.methods.len() + self.fields.len()
    }

    /// Return a snapshot of current statistics.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            method_entries: self.methods.len(),
            field_entries: self.fields.len(),
            hits: self.hits,
            misses: self.misses,
            hit_rate: self.hit_rate(),
        }
    }
}

impl Default for ThreadLocalResolveCache {
    fn default() -> Self {
        Self::new(4096)
    }
}

// ---------------------------------------------------------------------------
// SharedResolutionState
// ---------------------------------------------------------------------------

/// Cross-thread promoted invoke target key:
/// `(caller_class, cp_index, is_special, receiver_class)`.
///
/// `receiver_class` is `None` for invokestatic / invokespecial and the concrete
/// receiver `ClassId` for monomorphic invokevirtual/invokeinterface entries so
/// that two receiver classes sharing the same call site do not collide.
pub type PromotedInvokeKey = (ClassId, u16, bool, Option<ClassId>);

/// Read-optimised shared resolution state accessible from any thread.
///
/// Reads take a `RwLock` read-guard (concurrent readers never block each
/// other).  Writes take the write-guard and are expected to be infrequent
/// (only on first resolution of a given target).
pub struct SharedResolutionState {
    global_methods: RwLock<FxHashMap<ResolutionKey, ResolvedTarget>>,
    global_fields: RwLock<FxHashMap<ResolutionKey, ResolvedField>>,
    /// T10.4 — cross-thread promoted cache of fully-built invoke targets.
    ///
    /// Thread-local `invoke_cache` misses consult this map under a read-lock
    /// before falling through to the slow `class_manager` walk.  When a thread
    /// completes the slow path it promotes the resulting `CachedInvokeTarget`
    /// here so sibling threads skip the walk on their first call.
    promoted_invokes: RwLock<FxHashMap<PromotedInvokeKey, CachedInvokeTarget>>,
    /// T10.4 observability — number of successful read-lock hits on
    /// `promoted_invokes` since VM start.  Tests assert this counter to prove
    /// the shared read path bypassed the class-manager write lock.
    promoted_hits: AtomicU64,
    /// T10.4 observability — number of write-lock inserts into
    /// `promoted_invokes`.
    promoted_inserts: AtomicU64,
}

impl SharedResolutionState {
    /// Create an empty shared state.
    pub fn new() -> Self {
        Self {
            global_methods: RwLock::new(fx_hashmap()),
            global_fields: RwLock::new(fx_hashmap()),
            promoted_invokes: RwLock::new(fx_hashmap()),
            promoted_hits: AtomicU64::new(0),
            promoted_inserts: AtomicU64::new(0),
        }
    }

    // -- methods ----------------------------------------------------------

    /// Attempt to resolve a method from the shared cache.  Acquires a read
    /// lock; concurrent callers do not block each other.
    pub fn resolve_method(&self, key: &ResolutionKey) -> Option<ResolvedTarget> {
        let guard = self.global_methods.read();
        guard.get(key).cloned()
    }

    /// Store a method resolution in the shared cache.  Acquires a write lock.
    pub fn cache_method(&self, key: ResolutionKey, target: ResolvedTarget) {
        let mut guard = self.global_methods.write();
        guard.insert(key, target);
    }

    // -- fields -----------------------------------------------------------

    /// Attempt to resolve a field from the shared cache.  Acquires a read lock.
    pub fn resolve_field(&self, key: &ResolutionKey) -> Option<ResolvedField> {
        let guard = self.global_fields.read();
        guard.get(key).cloned()
    }

    /// Store a field resolution in the shared cache.  Acquires a write lock.
    pub fn cache_field(&self, key: ResolutionKey, field: ResolvedField) {
        let mut guard = self.global_fields.write();
        guard.insert(key, field);
    }

    // -- T10.4 promoted invoke targets -------------------------------------

    /// Attempt to fetch a fully-built `CachedInvokeTarget` for the given
    /// call site.  A hit avoids walking the class manager and rebuilding the
    /// `Arc<CachedBytecodeMethod>`.  Acquires only a read-lock.
    ///
    /// WP2.4-F1 — stale entries (whose declaring class has been redefined
    /// since the entry was promoted) are *not* returned. The hot path stays
    /// read-locked: we observe a stale entry under the read guard, drop the
    /// guard, then upgrade to a write guard to remove it. Callers see
    /// `None` and fall through to the slow re-resolution which will
    /// promote a fresh entry.
    pub fn get_promoted_invoke(
        &self,
        key: &PromotedInvokeKey,
    ) -> Option<CachedInvokeTarget> {
        let guard = self.promoted_invokes.read();
        let hit = guard.get(key).cloned();
        drop(guard);
        match hit {
            Some(t) if !t.is_stale() => {
                self.promoted_hits.fetch_add(1, Ordering::Relaxed);
                Some(t)
            }
            Some(_stale) => {
                // Evict on detection — sibling threads would otherwise keep
                // re-promoting the same stale entry until somebody noticed.
                let mut guard = self.promoted_invokes.write();
                if let Some(existing) = guard.get(key) {
                    if existing.is_stale() {
                        guard.remove(key);
                    }
                }
                None
            }
            None => None,
        }
    }

    /// Promote a fully-built `CachedInvokeTarget` so sibling threads can
    /// populate their local invoke cache without repeating the slow
    /// resolution walk.  Acquires a write-lock.
    pub fn insert_promoted_invoke(
        &self,
        key: PromotedInvokeKey,
        target: CachedInvokeTarget,
    ) {
        let mut guard = self.promoted_invokes.write();
        guard.insert(key, target);
        self.promoted_inserts.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of the promoted-invoke hit counter (tests / diagnostics).
    pub fn promoted_hit_count(&self) -> u64 {
        self.promoted_hits.load(Ordering::Relaxed)
    }

    /// Snapshot of the promoted-invoke insert counter (tests / diagnostics).
    pub fn promoted_insert_count(&self) -> u64 {
        self.promoted_inserts.load(Ordering::Relaxed)
    }

    /// Number of distinct call sites currently cached in the promoted-invoke
    /// map (tests / diagnostics).  Acquires a read-lock.
    pub fn promoted_invoke_count(&self) -> usize {
        self.promoted_invokes.read().len()
    }

    // -- housekeeping -----------------------------------------------------

    /// Clear both method and field caches (e.g. after class redefinition).
    pub fn invalidate_all(&self) {
        self.global_methods.write().clear();
        self.global_fields.write().clear();
        self.promoted_invokes.write().clear();
    }

    /// Clear promoted invoke entries whose caller class or receiver class
    /// matches `class_id` — used by CHA invalidation / class redefinition.
    pub fn invalidate_promoted_for_class(&self, class_id: ClassId) {
        let mut guard = self.promoted_invokes.write();
        guard.retain(|(caller, _, _, rcv), _| {
            *caller != class_id && *rcv != Some(class_id)
        });
    }

    /// Number of cached method resolutions.
    pub fn method_count(&self) -> usize {
        self.global_methods.read().len()
    }

    /// Number of cached field resolutions.
    pub fn field_count(&self) -> usize {
        self.global_fields.read().len()
    }
}

impl Default for SharedResolutionState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn sample_target(class_id: u64) -> ResolvedTarget {
        ResolvedTarget {
            class_id,
            method_index: 0,
            is_static: false,
            is_native: false,
            vtable_slot: None,
        }
    }

    fn sample_field(class_id: u64) -> ResolvedField {
        ResolvedField {
            class_id,
            field_index: 0,
            field_offset: 8,
            is_static: false,
        }
    }

    // -- ResolutionKey tests ----------------------------------------------

    #[test]
    fn resolution_key_hashing_deterministic() {
        let k1 = ResolutionKey::new("java/lang/Object", "hashCode", "()I");
        let k2 = ResolutionKey::new("java/lang/Object", "hashCode", "()I");
        assert_eq!(k1, k2);
        assert_eq!(k1.class_hash, k2.class_hash);
        assert_eq!(k1.name_hash, k2.name_hash);
        assert_eq!(k1.desc_hash, k2.desc_hash);
    }

    #[test]
    fn resolution_key_different_strings_different_hashes() {
        let k1 = ResolutionKey::new("java/lang/Object", "hashCode", "()I");
        let k2 = ResolutionKey::new("java/lang/String", "length", "()I");
        assert_ne!(k1.class_hash, k2.class_hash);
        assert_ne!(k1.name_hash, k2.name_hash);
        // descriptors are the same here, so only class+name differ
    }

    // -- ThreadLocalResolveCache: method put/get --------------------------

    #[test]
    fn thread_local_cache_put_get_method() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("A", "m", "()V");
        let target = sample_target(1);
        cache.put_method(key, target.clone());
        assert_eq!(cache.get_method(&key), Some(&target));
    }

    #[test]
    fn thread_local_cache_put_get_field() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("A", "x", "I");
        let field = sample_field(1);
        cache.put_field(key, field.clone());
        assert_eq!(cache.get_field(&key), Some(&field));
    }

    #[test]
    fn thread_local_cache_miss_returns_none() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("X", "y", "()V");
        assert_eq!(cache.get_method(&key), None);
        assert_eq!(cache.get_field(&key), None);
    }

    #[test]
    fn thread_local_cache_hit_rate_tracking() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("A", "m", "()V");
        cache.put_method(key, sample_target(1));

        // 1 hit
        let _ = cache.get_method(&key);
        // 1 miss
        let miss_key = ResolutionKey::new("B", "n", "()V");
        let _ = cache.get_method(&miss_key);

        assert_eq!(cache.hits, 1);
        assert_eq!(cache.misses, 1);
        assert!((cache.hit_rate() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn thread_local_cache_max_entries_eviction() {
        let mut cache = ThreadLocalResolveCache::new(3);

        // Fill to capacity with 3 method entries.
        for i in 0..3u64 {
            let key = ResolutionKey {
                class_hash: i,
                name_hash: 0,
                desc_hash: 0,
            };
            cache.put_method(key, sample_target(i));
        }
        assert_eq!(cache.len(), 3);

        // Inserting a 4th should evict the oldest (class_hash=0).
        let new_key = ResolutionKey {
            class_hash: 99,
            name_hash: 0,
            desc_hash: 0,
        };
        cache.put_method(new_key, sample_target(99));
        assert_eq!(cache.len(), 3);

        // The oldest entry should be gone.
        let evicted_key = ResolutionKey {
            class_hash: 0,
            name_hash: 0,
            desc_hash: 0,
        };
        // Use contains_key directly to avoid affecting hit/miss stats.
        assert!(!cache.methods.contains_key(&evicted_key));
        assert!(cache.methods.contains_key(&new_key));
    }

    #[test]
    fn thread_local_cache_invalidate_class() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let k1 = ResolutionKey::new("A", "m1", "()V");
        let k2 = ResolutionKey::new("A", "m2", "()V");
        let k3 = ResolutionKey::new("B", "m1", "()V");

        cache.put_method(k1, sample_target(10));
        cache.put_method(k2, sample_target(10));
        cache.put_method(k3, sample_target(20));
        cache.put_field(k1, sample_field(10));

        assert_eq!(cache.len(), 4);
        cache.invalidate_class(10);
        // Only k3 (class_id=20) should survive.
        assert_eq!(cache.len(), 1);
        assert!(cache.methods.contains_key(&k3));
    }

    #[test]
    fn thread_local_cache_clear() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("A", "m", "()V");
        cache.put_method(key, sample_target(1));
        cache.put_field(key, sample_field(1));
        let _ = cache.get_method(&key); // hit=1
        assert!(cache.len() > 0);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.hits, 0);
        assert_eq!(cache.misses, 0);
    }

    #[test]
    fn cache_stats_computation() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("A", "m", "()V");
        cache.put_method(key, sample_target(1));
        cache.put_field(key, sample_field(1));
        // Generate hits and misses.
        let _ = cache.get_method(&key); // hit
        let _ = cache.get_method(&key); // hit
        let miss_key = ResolutionKey::new("X", "y", "()V");
        let _ = cache.get_method(&miss_key); // miss

        let stats = cache.stats();
        assert_eq!(stats.method_entries, 1);
        assert_eq!(stats.field_entries, 1);
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        let expected_rate = 2.0 / 3.0;
        assert!((stats.hit_rate - expected_rate).abs() < 1e-9);
    }

    // -- SharedResolutionState tests --------------------------------------

    #[test]
    fn shared_state_put_get_method() {
        let state = SharedResolutionState::new();
        let key = ResolutionKey::new("A", "m", "()V");
        let target = sample_target(1);
        state.cache_method(key, target.clone());
        assert_eq!(state.resolve_method(&key), Some(target));
    }

    #[test]
    fn shared_state_cache_method_resolve_method_roundtrip() {
        let state = SharedResolutionState::new();
        let key = ResolutionKey::new("java/lang/Math", "max", "(II)I");
        let target = ResolvedTarget {
            class_id: 42,
            method_index: 7,
            is_static: true,
            is_native: false,
            vtable_slot: Some(3),
        };
        state.cache_method(key, target.clone());
        let resolved = state.resolve_method(&key).unwrap();
        assert_eq!(resolved, target);
        assert_eq!(state.method_count(), 1);
    }

    #[test]
    fn shared_state_concurrent_reads() {
        let state = Arc::new(SharedResolutionState::new());
        let key = ResolutionKey::new("A", "m", "()V");
        let target = sample_target(1);
        state.cache_method(key, target.clone());

        let mut handles = Vec::new();
        for _ in 0..4 {
            let state = Arc::clone(&state);
            let expected = target.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let got = state.resolve_method(&key);
                    assert_eq!(got, Some(expected.clone()));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn shared_state_invalidate_all() {
        let state = SharedResolutionState::new();
        let key = ResolutionKey::new("A", "m", "()V");
        state.cache_method(key, sample_target(1));
        state.cache_field(key, sample_field(1));
        assert_eq!(state.method_count(), 1);
        assert_eq!(state.field_count(), 1);

        state.invalidate_all();
        assert_eq!(state.method_count(), 0);
        assert_eq!(state.field_count(), 0);
        assert_eq!(state.resolve_method(&key), None);
    }

    // -- FxHasher tests ---------------------------------------------------

    #[test]
    fn fxhasher_consistency() {
        let a = fx_hash_str("java/lang/Object");
        let b = fx_hash_str("java/lang/Object");
        assert_eq!(a, b);
    }

    #[test]
    fn fxhasher_different_inputs_different_outputs() {
        let a = fx_hash_str("java/lang/Object");
        let b = fx_hash_str("java/lang/String");
        let c = fx_hash_str("java/util/HashMap");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    // -- T10.4 promoted invoke cache --------------------------------------

    fn sample_bytecode_method(class_id: u32) -> std::sync::Arc<rustjvm_jit_api::CachedBytecodeMethod> {
        std::sync::Arc::new(rustjvm_jit_api::CachedBytecodeMethod {
            declaring_class_id: ClassId::new(class_id),
            class_name: std::sync::Arc::from("A"),
            method_name: std::sync::Arc::from("m"),
            method_descriptor: std::sync::Arc::from("()V"),
            source_file: None,
            code: std::sync::Arc::from(vec![0xB1u8].as_slice()),
            exception_table: std::sync::Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        })
    }

    #[test]
    fn t10_shared_resolution_read_hit_bypasses_write_lock() {
        let state = SharedResolutionState::new();
        let key: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(20)));
        let target = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(20),
            cached: sample_bytecode_method(20),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        };
        // Miss first — returns None, does not bump hit counter.
        assert!(state.get_promoted_invoke(&key).is_none());
        assert_eq!(state.promoted_hit_count(), 0);
        // Insert once (takes the only write-lock for this call site).
        state.insert_promoted_invoke(key, target.clone());
        assert_eq!(state.promoted_insert_count(), 1);
        // Hit — advances the hit counter, returns an equivalent target,
        // and takes only a read-lock internally.
        let got = state.get_promoted_invoke(&key).expect("hit expected");
        match got {
            CachedInvokeTarget::VirtualBytecode { receiver_class_id, .. } => {
                assert_eq!(receiver_class_id, ClassId::new(20));
            }
            _ => panic!("wrong variant cached"),
        }
        assert_eq!(state.promoted_hit_count(), 1);
        assert_eq!(state.promoted_invoke_count(), 1);
    }

    #[test]
    fn t10_shared_resolution_distinguishes_receiver_classes() {
        // Same call site, two different receiver classes must not collide.
        let state = SharedResolutionState::new();
        let k1: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(20)));
        let k2: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(21)));
        let t1 = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(20),
            cached: sample_bytecode_method(20),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        };
        let t2 = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(21),
            cached: sample_bytecode_method(21),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        };
        state.insert_promoted_invoke(k1, t1);
        state.insert_promoted_invoke(k2, t2);
        assert_eq!(state.promoted_invoke_count(), 2);
        match state.get_promoted_invoke(&k1).unwrap() {
            CachedInvokeTarget::VirtualBytecode { receiver_class_id, .. } => {
                assert_eq!(receiver_class_id, ClassId::new(20));
            }
            _ => panic!("wrong variant"),
        }
        match state.get_promoted_invoke(&k2).unwrap() {
            CachedInvokeTarget::VirtualBytecode { receiver_class_id, .. } => {
                assert_eq!(receiver_class_id, ClassId::new(21));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn t10_shared_resolution_invalidate_for_class_drops_both_ends() {
        let state = SharedResolutionState::new();
        // Caller = class 10, receiver = class 30.
        let k1: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(30)));
        // Caller = class 40, receiver = class 30 (so invalidating 30 drops it).
        let k2: PromotedInvokeKey = (ClassId::new(40), 7, false, Some(ClassId::new(30)));
        // Caller = class 50, receiver = class 60 (untouched by invalidation).
        let k3: PromotedInvokeKey = (ClassId::new(50), 7, false, Some(ClassId::new(60)));
        state.insert_promoted_invoke(
            k1,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(30),
                cached: sample_bytecode_method(30),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        state.insert_promoted_invoke(
            k2,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(30),
                cached: sample_bytecode_method(30),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        state.insert_promoted_invoke(
            k3,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(60),
                cached: sample_bytecode_method(60),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        state.invalidate_promoted_for_class(ClassId::new(30));
        assert!(state.get_promoted_invoke(&k1).is_none());
        assert!(state.get_promoted_invoke(&k2).is_none());
        // k3 (class 60 / caller 50) must survive.
        assert!(state.get_promoted_invoke(&k3).is_some());
    }

    #[test]
    fn t10_invalidate_all_clears_promoted_cache() {
        let state = SharedResolutionState::new();
        let key: PromotedInvokeKey = (ClassId::new(1), 0, false, None);
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert_eq!(state.promoted_invoke_count(), 1);
        state.invalidate_all();
        assert_eq!(state.promoted_invoke_count(), 0);
        assert!(state.get_promoted_invoke(&key).is_none());
    }

    #[test]
    fn t10_shared_resolution_concurrent_reads_do_not_serialize() {
        // Ensure the read path uses a read-lock: multiple reader threads
        // should all observe the same hit without serialising.  We detect
        // serialisation indirectly — all N*M reads must complete and the
        // hit counter must end at exactly N*M.
        let state = std::sync::Arc::new(SharedResolutionState::new());
        let key: PromotedInvokeKey = (ClassId::new(5), 3, false, None);
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(7),
                cached: sample_bytecode_method(7),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = std::sync::Arc::clone(&state);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    assert!(s.get_promoted_invoke(&key).is_some());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(state.promoted_hit_count(), 4 * 50);
        // Only one insert (the setup) — no write contention occurred.
        assert_eq!(state.promoted_insert_count(), 1);
    }

    // -- Resolution flow integration test ---------------------------------

    #[test]
    fn resolution_flow_thread_local_hit_shared_hit_full_miss() {
        let shared = Arc::new(SharedResolutionState::new());
        let mut local = ThreadLocalResolveCache::new(128);

        let key_a = ResolutionKey::new("A", "m", "()V");
        let key_b = ResolutionKey::new("B", "n", "()V");
        let key_c = ResolutionKey::new("C", "o", "()V");

        let target_a = sample_target(1);
        let target_b = sample_target(2);
        let target_c = sample_target(3);

        // Pre-populate thread-local with key_a.
        local.put_method(key_a, target_a.clone());
        // Pre-populate shared with key_b.
        shared.cache_method(key_b, target_b.clone());

        // Step 1: thread-local hit for key_a.
        assert_eq!(local.get_method(&key_a), Some(&target_a));

        // Step 2: thread-local miss for key_b, shared hit.
        assert_eq!(local.get_method(&key_b), None);
        let from_shared = shared.resolve_method(&key_b).unwrap();
        assert_eq!(from_shared, target_b);
        // Populate thread-local from shared.
        local.put_method(key_b, from_shared);
        // Now thread-local has it.
        assert_eq!(local.get_method(&key_b), Some(&target_b));

        // Step 3: full miss for key_c (neither local nor shared).
        assert_eq!(local.get_method(&key_c), None);
        assert_eq!(shared.resolve_method(&key_c), None);
        // Simulate full resolution: populate both.
        shared.cache_method(key_c, target_c.clone());
        local.put_method(key_c, target_c.clone());
        // Now both have it.
        assert_eq!(local.get_method(&key_c), Some(&target_c));
        assert_eq!(shared.resolve_method(&key_c), Some(target_c));
    }
}
