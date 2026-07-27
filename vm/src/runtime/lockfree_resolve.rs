// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared method-resolution caching for the interpreter's hot path.
//!
//! ## What is actually wired (audited 2026-07-26, `stackwalk-and-vtable`)
//!
//! This module's historical header described a three-level flow:
//!
//! ```text
//! 1. Check thread-local cache (no lock) -> if hit, return immediately
//! 2. Check shared global cache (read lock) -> if hit, populate thread-local, return
//! 3. Do full resolution (class manager lock) -> populate shared + thread-local, return
//! ```
//!
//! **Level 1 does not exist in this module.** [`ThreadLocalResolveCache`] has
//! zero production instantiations — every reference to it outside this file is
//! a doc comment or one of this file's own unit tests. The real thread-local
//! tier is `JvmThread::invoke_cache`, which lives in the threading/interpreter
//! code and has nothing to do with the type below. Likewise
//! [`SharedResolutionState::resolve_method`] / `cache_method` /
//! `resolve_field` / `cache_field` and their `global_methods` /
//! `global_fields` maps have **no production callers**: `ResolutionKey`,
//! `ResolvedTarget` and `ResolvedField` are exercised only by tests. The one
//! live consumer of this module is `promoted_invokes`, reached via
//! [`SharedResolutionState::get_promoted_invoke`] /
//! [`SharedResolutionState::insert_promoted_invoke`] from the interpreter's
//! invoke paths.
//!
//! ## "Lock-free" is a misnomer — keep it in mind before relying on it
//!
//! Nothing here is lock-free in the technical sense, and the live path in
//! particular is not. `get_promoted_invoke` takes a `parking_lot::RwLock`
//! **read** guard on a single process-wide map: acquiring it is an atomic
//! read-modify-write on one shared word, so every dispatching thread writes
//! the same cache line on every consult. That is much cheaper than the
//! `ClassManager` write lock it replaces — which is the real and worthwhile
//! win, and it is genuine — but it is contention, not its absence. The claim
//! that this "eliminates lock contention on the common case" was overstated;
//! it *relocates* it off the class-manager lock.
//!
//! (This is the same class of overstatement a sibling pass found on
//! `class_manager.rs::class_loading_locks`, which claimed to "allow concurrent
//! loading of different classes to proceed without contention" while
//! `load_class_concurrent` still ran under the L10 write lock. Verify against
//! the code, not the comment.)

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared-cache capacity bound
// ---------------------------------------------------------------------------

/// HIGH (security) — default upper bound on the number of entries held in each
/// of the cross-thread shared maps (`global_methods`, `global_fields`,
/// `promoted_invokes`).
///
/// The per-thread [`ThreadLocalResolveCache`] has always been bounded
/// (`max_entries`, default 4096), but the shared maps grew without limit: an
/// adversarial or dynamic-codegen workload that mints an unbounded number of
/// distinct `(class, member, descriptor, loader)` keys — e.g. a class that
/// emits fresh lambda / proxy / hidden-class names per call — would grow these
/// maps until the process exhausts memory, a denial-of-service.
///
/// These maps are *pure caches*: every entry can be reconstructed by
/// re-resolving against the class manager, so dropping an entry is always
/// correctness-preserving (a miss simply re-resolves and re-promotes). That
/// lets us pick the lowest-risk effective bound — a hard cap with approximate
/// eviction — without touching the read path.
///
/// The default (65536 per map) is generous: a real application's live working
/// set of resolved members is far smaller, so steady-state programs never hit
/// the cap, while a key-minting adversary is held to a bounded footprint.
const DEFAULT_SHARED_CACHE_CAP: usize = 65_536;

/// Uncached form of [`shared_cache_cap`]. Kept separate so the unit tests can
/// exercise the parsing rules without depending on which test happened to run
/// first (the public entry point memoizes for process lifetime).
fn parse_shared_cache_cap(value: Option<&str>) -> usize {
    match value {
        Some(s) => match s.trim().parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => DEFAULT_SHARED_CACHE_CAP,
        },
        None => DEFAULT_SHARED_CACHE_CAP,
    }
}

fn read_shared_cache_cap() -> usize {
    let value = cratonvm_types::flags::runtime_var("CRATONVM_RESOLVE_CACHE_CAP").ok();
    parse_shared_cache_cap(value.as_deref())
}

/// Resolve the shared-cache capacity, honouring the `CRATONVM_RESOLVE_CACHE_CAP`
/// environment override (mirrors the project's `CRATONVM_*` configuration
/// convention). A value of `0`, an empty string, or an unparseable value falls
/// back to [`DEFAULT_SHARED_CACHE_CAP`]; the cap can never be set below 1 so a
/// freshly-inserted entry always survives.
///
/// PERF (2026-07-26 arch pass). This used to call `cratonvm_types::flags::runtime_var` — which
/// allocates a `String` and, on Windows, is a `GetEnvironmentVariableW`
/// syscall — on **every first promotion of a call site**, and did so *while
/// holding the `promoted_invokes` write guard*, so every thread promoting a
/// target serialised behind an environment lookup. Real applications mint one
/// such promotion per distinct `(caller class, cp_index, receiver class)`
/// triple, i.e. hundreds of thousands of them during warm-up.
///
/// Memoized in a `OnceLock`, matching the process-lifetime memo contract every
/// flag in `runtime::env_cache` already has (and the one a sibling pass just
/// applied to the seven throw-path flags in `exceptions.rs`): setting the
/// variable after the first promotion no longer takes effect. This is not a
/// new gate — the variable is an existing, optional tuning override with a
/// working default, so nothing lands off by default.
#[inline]
fn shared_cache_cap() -> usize {
    // Test-only escape hatch. The memo below is process-lifetime, so the
    // eviction tests can no longer steer it with the environment variable;
    // they set this instead. Compiled out entirely in non-test builds, so the
    // shipped fast path is a single relaxed `OnceLock` read.
    #[cfg(test)]
    {
        let forced = tests_cap_override::get();
        if forced != 0 {
            return forced;
        }
    }
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(read_shared_cache_cap)
}

#[cfg(test)]
mod tests_cap_override {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static OVERRIDE: AtomicUsize = AtomicUsize::new(0);
    /// `0` means "no override — use the memoized value".
    pub(super) fn get() -> usize {
        OVERRIDE.load(Ordering::Relaxed)
    }
    pub(super) fn set(v: usize) {
        OVERRIDE.store(v, Ordering::Relaxed);
    }
}

/// Evict entries from a write-locked shared cache map until it can accept one
/// more insert without exceeding `cap`, i.e. until `len < cap`.
///
/// This runs **only** while the caller already holds the map's write guard, so
/// no reader or other writer can be touching the map concurrently — the
/// eviction can never deadlock against the read path. Because the maps are pure
/// caches, the eviction policy only needs to be *approximate*: we drop an
/// arbitrary entry surfaced by the hash map's iteration order (cheap — no
/// auxiliary ordering structure to maintain, unlike the thread-local FIFO). A
/// wrongly-evicted entry costs at most one re-resolution.
///
/// Generic over the key/value so the same routine bounds all three shared maps.
#[inline]
fn evict_to_fit<K, V>(map: &mut FxHashMap<K, V>, cap: usize)
where
    K: std::hash::Hash + Eq + Clone,
{
    // Leave room for the imminent insert: shrink until `len < cap`. Guard on
    // emptiness so a pathological `cap == 0` (already excluded by
    // `shared_cache_cap`, but defended here for direct callers/tests) cannot
    // spin forever.
    while map.len() >= cap {
        let victim = match map.keys().next() {
            Some(k) => k.clone(),
            None => break,
        };
        map.remove(&victim);
    }
}

// Round-9 MED-1: doc cleanup. `SharedResolutionState` below holds three
// `parking_lot::RwLock` maps (`global_methods`, `global_fields`,
// `promoted_invokes`); the struct definition at the bottom of this file is the
// source of truth. parking_lot drops the pthread_rwlock + poisoning overhead
// that the std lock would impose — every other workspace lock is already
// parking_lot.
use parking_lot::RwLock;

#[allow(unused_imports)]
use super::fx_collections::{fx_hashmap, FxBuildHasher, FxHashMap, FxHasher};
use crate::classloading::resolution::CachedInvokeTarget as GenericCachedInvokeTarget;
use crate::classloading::ClassId;
use std::hash::{Hash, Hasher};

type CachedInvokeTarget = GenericCachedInvokeTarget<Arc<crate::jit::CompiledMethod>>;

// ---------------------------------------------------------------------------
// ResolutionKey
// ---------------------------------------------------------------------------

/// Resolution cache key.
///
/// HIGH (security) — Previously the key was a 3-tuple of `FxHasher` digests of
/// `(class_name, member_name, descriptor)`. `FxHasher` is a non-cryptographic
/// hash with poor diffusion; an adversary that controls bytecode can forge a
/// triple whose digests collide with an unrelated triple already in the cache,
/// causing the cache to return the WRONG `ResolvedTarget` / `ResolvedField`
/// on lookup. That is type-confusion / privilege bypass.
///
/// The fix has two parts:
///
/// 1. **Identity validation (option *a* from the harden brief).** The key
///    stores the full interned strings as `Arc<str>` so the `Eq` impl can
///    verify byte-for-byte that the looked-up entry actually matches; hash
///    collisions only ever land in the same hashmap bucket and the linear
///    probe rejects collisions via `Eq`. This is correctness-not-probabilistic.
///
///    `Hash` keeps the precomputed digests so the fast path still hashes the
///    key by writing three `u64`s, not by re-walking each string.
///
/// 2. **Loader isolation.** A `loader_epoch` field is included so a cache
///    entry promoted by loader L1 can never satisfy a lookup from loader L2
///    sharing the same FQN (JVMS §5.3.4 — initiating + defining loader pair
///    is part of class identity).
///
/// PERF NOTE: the `Eq` impl on a hit pays one short-circuited `Arc::ptr_eq`
/// per field followed by a byte compare only on the (rare) interning miss.
/// The vast majority of hot-path lookups go through the JVM's existing
/// `Arc<str>` interner, so `Arc::ptr_eq` returns true in O(1) without
/// touching the underlying bytes. The cost relative to the broken `u64`-only
/// `Eq` is a handful of pointer compares per resolution — correctness wins.
#[derive(Clone, Debug)]
pub struct ResolutionKey {
    pub class_hash: u64,
    pub name_hash: u64,
    pub desc_hash: u64,
    /// Defining classloader epoch / handle hash. Two loaders that have
    /// independently loaded a class of the same FQN MUST get distinct cache
    /// entries — JVMS §5.3.4 makes the loader pair part of class identity.
    pub loader_epoch: u64,
    /// Full class name. Preserved on the key so `Eq` can byte-compare on
    /// hash collision and refuse to return a poisoned entry.
    pub class_name: Arc<str>,
    /// Full member (method / field) name. See `class_name`.
    pub member_name: Arc<str>,
    /// Full type descriptor. See `class_name`.
    pub descriptor: Arc<str>,
}

impl ResolutionKey {
    /// Build a key from the raw JVM strings using loader epoch `0`. Suitable
    /// for tests and for call sites that have not yet plumbed the
    /// defining-loader handle through. Production code paths SHOULD use
    /// [`ResolutionKey::with_loader`] so that two loaders with overlapping
    /// FQNs do not cross-contaminate the cache.
    pub fn new(class_name: &str, member_name: &str, descriptor: &str) -> Self {
        Self::with_loader(class_name, member_name, descriptor, 0)
    }

    /// Build a key from the raw JVM strings plus a defining-classloader epoch.
    ///
    /// `loader_epoch` is any value that uniquely identifies the defining
    /// classloader — a monotonically-bumped counter, an `Arc::as_ptr` cast,
    /// or a stable handle hash. Two loaders MUST yield different epochs.
    pub fn with_loader(
        class_name: &str,
        member_name: &str,
        descriptor: &str,
        loader_epoch: u64,
    ) -> Self {
        Self {
            class_hash: fx_hash_str(class_name),
            name_hash: fx_hash_str(member_name),
            desc_hash: fx_hash_str(descriptor),
            loader_epoch,
            class_name: Arc::from(class_name),
            member_name: Arc::from(member_name),
            descriptor: Arc::from(descriptor),
        }
    }

    /// Build a key directly from already-interned `Arc<str>` handles. Hot-path
    /// callers (`invokevirtual`, `getfield`, etc.) that obtain their strings
    /// from the JVM's interner SHOULD use this constructor: the `Arc<str>`
    /// is cloned by bumping a refcount, never allocated, and `Eq` short-
    /// circuits via `Arc::ptr_eq`.
    pub fn from_interned(
        class_name: Arc<str>,
        member_name: Arc<str>,
        descriptor: Arc<str>,
        loader_epoch: u64,
    ) -> Self {
        Self {
            class_hash: fx_hash_str(&class_name),
            name_hash: fx_hash_str(&member_name),
            desc_hash: fx_hash_str(&descriptor),
            loader_epoch,
            class_name,
            member_name,
            descriptor,
        }
    }
}

/// Equality is *full identity*, not just hash equality. This is the security-
/// critical bit: an attacker who forges a triple whose `FxHasher` digests
/// collide with a cached entry still gets a cache miss because the byte
/// comparison rejects the forged strings.
impl PartialEq for ResolutionKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // Cheap rejects first — the precomputed digests + loader epoch are
        // 4 × u64 and on a real miss they almost always differ. Only on a
        // hash match do we fall through to the string compares.
        if self.class_hash != other.class_hash
            || self.name_hash != other.name_hash
            || self.desc_hash != other.desc_hash
            || self.loader_epoch != other.loader_epoch
        {
            return false;
        }
        // Same-interner hot path: pointer-equal `Arc<str>` => equal in O(1).
        // Fallback to byte compare for the rare cross-interner case (e.g.
        // tests, defensively-decoded strings).
        arc_str_eq(&self.class_name, &other.class_name)
            && arc_str_eq(&self.member_name, &other.member_name)
            && arc_str_eq(&self.descriptor, &other.descriptor)
    }
}

impl Eq for ResolutionKey {}

/// `Hash` writes only the precomputed digests + loader epoch — O(1), no
/// re-walk of the underlying string bytes. The full strings are kept *only*
/// for `Eq` to defeat collisions; they MUST NOT participate in the hash, or
/// the hashmap bucket lookup would re-hash multi-byte strings on every probe
/// and the perf justification for `FxHasher` digests disappears.
impl Hash for ResolutionKey {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.class_hash);
        state.write_u64(self.name_hash);
        state.write_u64(self.desc_hash);
        state.write_u64(self.loader_epoch);
    }
}

/// Fast string equality: pointer-equal `Arc<str>` short-circuits, otherwise
/// fall back to byte compare. Hot-path callers using the JVM's interner hit
/// the fast path on every lookup.
#[inline]
fn arc_str_eq(a: &Arc<str>, b: &Arc<str>) -> bool {
    Arc::ptr_eq(a, b) || a.as_ref() == b.as_ref()
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
    ///
    /// HIGH (security) — `ResolutionKey` now embeds the full interned
    /// strings + loader epoch so the bucket linear-probe rejects forged
    /// collisions via byte compare. See the `ResolutionKey` doc for the
    /// full rationale.
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
            self.method_order.push_back(key.clone());
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
    ///
    /// HIGH (security) — see `put_method`.
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
            self.field_order.push_back(key.clone());
        }
        self.fields.insert(key, field);
    }

    // -- housekeeping -----------------------------------------------------

    /// Remove all entries whose `class_id` matches (supports class
    /// redefinition / hot-swap).
    pub fn invalidate_class(&mut self, class_id: u64) {
        self.methods.retain(|_, v| v.class_id != class_id);
        self.method_order.retain(|k| self.methods.contains_key(k));

        self.fields.retain(|_, v| v.class_id != class_id);
        self.field_order.retain(|k| self.fields.contains_key(k));
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
    ///
    /// HIGH (security) — the map is bounded at [`shared_cache_cap`] entries; if
    /// inserting a *new* key would exceed the cap, an approximate eviction
    /// drops an existing entry first so a key-minting workload cannot exhaust
    /// memory. Re-caching an already-present key never evicts.
    pub fn cache_method(&self, key: ResolutionKey, target: ResolvedTarget) {
        let mut guard = self.global_methods.write();
        if !guard.contains_key(&key) {
            evict_to_fit(&mut guard, shared_cache_cap());
        }
        guard.insert(key, target);
    }

    // -- fields -----------------------------------------------------------

    /// Attempt to resolve a field from the shared cache.  Acquires a read lock.
    pub fn resolve_field(&self, key: &ResolutionKey) -> Option<ResolvedField> {
        let guard = self.global_fields.read();
        guard.get(key).cloned()
    }

    /// Store a field resolution in the shared cache.  Acquires a write lock.
    ///
    /// HIGH (security) — bounded at [`shared_cache_cap`] entries; see
    /// [`SharedResolutionState::cache_method`] for the rationale.
    pub fn cache_field(&self, key: ResolutionKey, field: ResolvedField) {
        let mut guard = self.global_fields.write();
        if !guard.contains_key(&key) {
            evict_to_fit(&mut guard, shared_cache_cap());
        }
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
    pub fn get_promoted_invoke(&self, key: &PromotedInvokeKey) -> Option<CachedInvokeTarget> {
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
    ///
    /// HIGH (security) — bounded at [`shared_cache_cap`] entries; a call site
    /// minting unbounded distinct `(caller, cp_index, receiver)` keys cannot
    /// grow this map without limit. Re-promoting an already-present call site
    /// never evicts. See [`SharedResolutionState::cache_method`].
    pub fn insert_promoted_invoke(&self, key: PromotedInvokeKey, target: CachedInvokeTarget) {
        let mut guard = self.promoted_invokes.write();
        if !guard.contains_key(&key) {
            evict_to_fit(&mut guard, shared_cache_cap());
        }
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

    /// Clear every cache this type owns: the promoted-invoke map **and** the
    /// `global_methods` / `global_fields` maps.
    ///
    /// Note that the latter two have no production writers (see the module
    /// header), so in a running VM this acquires two write locks to clear two
    /// permanently-empty maps. Callers that only need the live cache gone
    /// should say so with [`invalidate_promoted`](Self::invalidate_promoted) —
    /// it is not a micro-optimisation but a statement of intent, so a future
    /// reader does not conclude from a call site that the other two maps are
    /// live.
    pub fn invalidate_all(&self) {
        self.global_methods.write().clear();
        self.global_fields.write().clear();
        self.promoted_invokes.write().clear();
    }

    /// Clear the promoted-invoke cache — the only cache in this module with
    /// production writers (`insert_promoted_invoke`, reached from the
    /// interpreter's invoke paths).
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-LR-1 of
    /// `docs/internal/arch-2026-07-26/stackwalk-and-vtable.md`). This is the
    /// entry point the class-loader unload sweep in `vm/src/memory/gc.rs`
    /// wants: a conservative wholesale clear of the live cache, without
    /// asserting by implication that `global_methods` / `global_fields` are
    /// live too.
    pub fn invalidate_promoted(&self) {
        self.promoted_invokes.write().clear();
    }

    /// Clear promoted invoke entries whose caller class or receiver class
    /// matches `class_id` — used by CHA invalidation / class redefinition.
    pub fn invalidate_promoted_for_class(&self, class_id: ClassId) {
        let mut guard = self.promoted_invokes.write();
        guard.retain(|(caller, _, _, rcv), _| *caller != class_id && *rcv != Some(class_id));
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
    use std::sync::{Arc, Mutex};

    static RESOLVE_CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());
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

    /// Helper: build a `ResolutionKey` directly from arbitrary numeric
    /// digests for eviction / collision tests. Strings are formatted from
    /// the same digests so the `Eq` impl behaves consistently with the
    /// hashes.
    fn key_from_parts(class_hash: u64, name_hash: u64, desc_hash: u64) -> ResolutionKey {
        ResolutionKey {
            class_hash,
            name_hash,
            desc_hash,
            loader_epoch: 0,
            class_name: std::sync::Arc::from(format!("C{class_hash}")),
            member_name: std::sync::Arc::from(format!("N{name_hash}")),
            descriptor: std::sync::Arc::from(format!("D{desc_hash}")),
        }
    }

    #[test]
    fn thread_local_cache_put_get_method() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("A", "m", "()V");
        let target = sample_target(1);
        cache.put_method(key.clone(), target.clone());
        assert_eq!(cache.get_method(&key), Some(&target));
    }

    #[test]
    fn thread_local_cache_put_get_field() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let key = ResolutionKey::new("A", "x", "I");
        let field = sample_field(1);
        cache.put_field(key.clone(), field.clone());
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
        cache.put_method(key.clone(), sample_target(1));

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
            let key = key_from_parts(i, 0, 0);
            cache.put_method(key, sample_target(i));
        }
        assert_eq!(cache.len(), 3);

        // Inserting a 4th should evict the oldest (class_hash=0).
        let new_key = key_from_parts(99, 0, 0);
        cache.put_method(new_key.clone(), sample_target(99));
        assert_eq!(cache.len(), 3);

        // The oldest entry should be gone.
        let evicted_key = key_from_parts(0, 0, 0);
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

        cache.put_method(k1.clone(), sample_target(10));
        cache.put_method(k2, sample_target(10));
        cache.put_method(k3.clone(), sample_target(20));
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
        cache.put_method(key.clone(), sample_target(1));
        cache.put_field(key.clone(), sample_field(1));
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
        cache.put_method(key.clone(), sample_target(1));
        cache.put_field(key.clone(), sample_field(1));
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
        state.cache_method(key.clone(), target.clone());
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
        state.cache_method(key.clone(), target.clone());
        let resolved = state.resolve_method(&key).unwrap();
        assert_eq!(resolved, target);
        assert_eq!(state.method_count(), 1);
    }

    #[test]
    fn shared_state_concurrent_reads() {
        let state = Arc::new(SharedResolutionState::new());
        let key = ResolutionKey::new("A", "m", "()V");
        let target = sample_target(1);
        state.cache_method(key.clone(), target.clone());

        let mut handles = Vec::new();
        for _ in 0..4 {
            let state = Arc::clone(&state);
            let expected = target.clone();
            let key = key.clone();
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
        state.cache_method(key.clone(), sample_target(1));
        state.cache_field(key.clone(), sample_field(1));
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

    fn sample_bytecode_method(
        class_id: u32,
    ) -> std::sync::Arc<cratonvm_jit_api::CachedBytecodeMethod> {
        std::sync::Arc::new(cratonvm_jit_api::CachedBytecodeMethod {
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
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
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
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id, ..
            } => {
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
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id, ..
            } => {
                assert_eq!(receiver_class_id, ClassId::new(20));
            }
            _ => panic!("wrong variant"),
        }
        match state.get_promoted_invoke(&k2).unwrap() {
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id, ..
            } => {
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
        local.put_method(key_a.clone(), target_a.clone());
        // Pre-populate shared with key_b.
        shared.cache_method(key_b.clone(), target_b.clone());

        // Step 1: thread-local hit for key_a.
        assert_eq!(local.get_method(&key_a), Some(&target_a));

        // Step 2: thread-local miss for key_b, shared hit.
        assert_eq!(local.get_method(&key_b), None);
        let from_shared = shared.resolve_method(&key_b).unwrap();
        assert_eq!(from_shared, target_b);
        // Populate thread-local from shared.
        local.put_method(key_b.clone(), from_shared);
        // Now thread-local has it.
        assert_eq!(local.get_method(&key_b), Some(&target_b));

        // Step 3: full miss for key_c (neither local nor shared).
        assert_eq!(local.get_method(&key_c), None);
        assert_eq!(shared.resolve_method(&key_c), None);
        // Simulate full resolution: populate both.
        shared.cache_method(key_c.clone(), target_c.clone());
        local.put_method(key_c.clone(), target_c.clone());
        // Now both have it.
        assert_eq!(local.get_method(&key_c), Some(&target_c));
        assert_eq!(shared.resolve_method(&key_c), Some(target_c));
    }

    // -- HIGH security harden tests ---------------------------------------
    //
    // These tests prove the cache rejects (1) forged hash collisions
    // (an adversary crafts a triple whose `FxHasher` digests match a
    // cached entry's despite the strings differing — would return the
    // wrong target on the vulnerable key) and (2) cross-classloader
    // contamination (loader L1 and L2 both load the same FQN — JVMS
    // §5.3.4 makes the defining loader part of class identity).

    /// Forge a `ResolutionKey` whose precomputed digests match `real`'s
    /// digests but whose underlying strings differ. We construct the
    /// forgery directly rather than brute-forcing a real `FxHasher`
    /// collision — the security property is "keys that hash the same
    /// but whose Eq-relevant data differs MUST NOT alias", which the
    /// directly-forged key exercises identically.
    fn forge_collision(real: &ResolutionKey) -> ResolutionKey {
        ResolutionKey {
            class_hash: real.class_hash,
            name_hash: real.name_hash,
            desc_hash: real.desc_hash,
            loader_epoch: real.loader_epoch,
            class_name: Arc::from("AttackerClass"),
            member_name: Arc::from("AttackerMethod"),
            descriptor: Arc::from("(Lattacker/Payload;)V"),
        }
    }

    #[test]
    fn forged_hash_collision_method_returns_miss() {
        let mut cache = ThreadLocalResolveCache::new(128);
        let real = ResolutionKey::new("java/lang/Object", "hashCode", "()I");
        let real_target = sample_target(0xDEADBEEF);
        cache.put_method(real.clone(), real_target.clone());
        assert_eq!(cache.get_method(&real), Some(&real_target));

        // Forged key — same hash digests, different strings. Lookup MUST
        // miss; otherwise the cache would return `real_target` for an
        // attacker-controlled triple (type confusion / privilege bypass).
        let forged = forge_collision(&real);
        assert_eq!(forged.class_hash, real.class_hash);
        assert_eq!(forged.name_hash, real.name_hash);
        assert_eq!(forged.desc_hash, real.desc_hash);
        assert_ne!(forged, real);
        assert!(
            cache.get_method(&forged).is_none(),
            "SECURITY: forged collision returned cached entry — type confusion!"
        );
        // Same property for the shared cross-thread cache.
        let state = SharedResolutionState::new();
        state.cache_method(real.clone(), real_target.clone());
        assert!(state.resolve_method(&forged).is_none());
        // And the field cache.
        let field_key = ResolutionKey::new("java/lang/String", "value", "[B");
        let real_field = sample_field(0xCAFEBABE);
        cache.put_field(field_key.clone(), real_field.clone());
        let forged_field = forge_collision(&field_key);
        assert!(cache.get_field(&forged_field).is_none());
    }

    #[test]
    fn two_classloaders_same_fqn_do_not_cross_contaminate() {
        // L1 and L2 both load `org/x/Foo::bar()V` independently. The
        // string hashes are identical; only the loader epoch separates
        // them. L1's resolution MUST NOT satisfy L2's lookup.
        let mut cache = ThreadLocalResolveCache::new(128);
        let k_l1 = ResolutionKey::with_loader("org/x/Foo", "bar", "()V", 0xA1A1);
        let k_l2 = ResolutionKey::with_loader("org/x/Foo", "bar", "()V", 0xB2B2);
        assert_eq!(k_l1.class_hash, k_l2.class_hash);
        assert_eq!(k_l1.name_hash, k_l2.name_hash);
        assert_eq!(k_l1.desc_hash, k_l2.desc_hash);
        assert_ne!(k_l1, k_l2);

        let l1_target = sample_target(100);
        cache.put_method(k_l1.clone(), l1_target.clone());
        assert_eq!(cache.get_method(&k_l1), Some(&l1_target));
        assert!(
            cache.get_method(&k_l2).is_none(),
            "SECURITY: L2 saw L1's resolution — loader isolation broken"
        );

        let l2_target = sample_target(200);
        cache.put_method(k_l2.clone(), l2_target.clone());
        // Both loaders' resolutions coexist; lookups don't cross over.
        assert_eq!(cache.get_method(&k_l1), Some(&l1_target));
        assert_eq!(cache.get_method(&k_l2), Some(&l2_target));
    }

    // -- HIGH security: shared-cache capacity bound -----------------------
    //
    // The cross-thread shared maps were previously unbounded — a workload
    // minting unbounded distinct keys could exhaust memory (DoS). These
    // tests prove the cap holds and that an evicted entry simply misses
    // (re-resolution is correctness-preserving), never panics.

    #[test]
    fn evict_to_fit_keeps_map_under_cap() {
        // Direct, env-independent test of the eviction core. Insert far more
        // than `cap` entries, evicting before each insert, and assert the map
        // never exceeds `cap`.
        const CAP: usize = 8;
        let mut map: FxHashMap<u64, u64> = fx_hashmap();
        for i in 0..1000u64 {
            evict_to_fit(&mut map, CAP);
            map.insert(i, i);
            assert!(
                map.len() <= CAP,
                "map exceeded cap: len={} cap={CAP}",
                map.len()
            );
        }
        // After the run the map is full but bounded.
        assert_eq!(map.len(), CAP);
    }

    #[test]
    fn evict_to_fit_zero_cap_does_not_spin_on_empty() {
        // Defensive: a `cap == 0` against an empty map must terminate (the
        // emptiness guard breaks the loop) rather than spin forever.
        let mut map: FxHashMap<u64, u64> = fx_hashmap();
        evict_to_fit(&mut map, 0);
        assert!(map.is_empty());
    }

    /// RAII helper for the test-only capacity override, so a failing
    /// assertion cannot leak a forced cap into the rest of the suite.
    struct CapOverride;
    impl CapOverride {
        fn set(v: usize) -> Self {
            tests_cap_override::set(v);
            CapOverride
        }
    }
    impl Drop for CapOverride {
        fn drop(&mut self) {
            tests_cap_override::set(0);
        }
    }

    #[test]
    fn shared_cache_cap_parses_env_override() {
        assert_eq!(parse_shared_cache_cap(Some("10")), 10);
        assert_eq!(parse_shared_cache_cap(Some("0")), DEFAULT_SHARED_CACHE_CAP);
        assert_eq!(parse_shared_cache_cap(Some("")), DEFAULT_SHARED_CACHE_CAP);
        assert_eq!(
            parse_shared_cache_cap(Some("not-a-number")),
            DEFAULT_SHARED_CACHE_CAP
        );
        assert_eq!(parse_shared_cache_cap(None), DEFAULT_SHARED_CACHE_CAP);
    }

    #[test]
    fn shared_cache_cap_is_memoized_for_process_lifetime() {
        // The public path is a stable process-lifetime value.
        let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
        let first = shared_cache_cap();
        assert_eq!(
            shared_cache_cap(),
            first,
            "shared_cache_cap must not re-read the environment"
        );
    }

    #[test]
    fn shared_cache_method_field_promoted_stay_bounded_via_env_cap() {
        // Drive the *public* insert paths through a small cap and prove all
        // three shared maps stay bounded while a key-minting workload floods
        // distinct keys. The lock is still taken so this test does not
        // interleave with the env-var tests that share the same knob.
        let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
        let _cap = CapOverride::set(16);

        let state = SharedResolutionState::new();
        for i in 0..500u64 {
            // Distinct method, field, and promoted-invoke keys per iteration.
            let mkey = key_from_parts(i, i, i);
            state.cache_method(mkey, sample_target(i));

            let fkey = key_from_parts(i ^ 0xFFFF, i, i);
            state.cache_field(fkey, sample_field(i));

            let pkey: PromotedInvokeKey = (
                ClassId::new(i as u32),
                (i % 64) as u16,
                false,
                Some(ClassId::new((i + 1) as u32)),
            );
            state.insert_promoted_invoke(
                pkey,
                CachedInvokeTarget::VirtualBytecode {
                    receiver_class_id: ClassId::new((i + 1) as u32),
                    cached: sample_bytecode_method((i + 1) as u32),
                    gate: crate::classloading::resolution::RedefineGate::never_stale(),
                },
            );

            assert!(state.method_count() <= 16, "methods exceeded cap");
            assert!(state.field_count() <= 16, "fields exceeded cap");
            assert!(state.promoted_invoke_count() <= 16, "promoted exceeded cap");
        }

        // An early, long-since-evicted key now simply misses (no panic); a
        // miss is correctness-preserving — the caller re-resolves.
        let evicted = key_from_parts(0, 0, 0);
        assert!(state.resolve_method(&evicted).is_none());

        // Re-inserting that key works and is observable (still bounded).
        state.cache_method(evicted.clone(), sample_target(0));
        assert_eq!(state.resolve_method(&evicted), Some(sample_target(0)));
        assert!(state.method_count() <= 16);
    }

    #[test]
    fn shared_cache_recache_existing_key_does_not_evict() {
        // Re-caching an already-present key must not trigger eviction (the
        // `contains_key` guard) — capacity is for *new* keys only.
        let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
        let _cap = CapOverride::set(4);

        let state = SharedResolutionState::new();
        // Fill to exactly the cap with 4 distinct keys.
        let keys: Vec<ResolutionKey> = (0..4u64).map(|i| key_from_parts(i, 7, 7)).collect();
        for (i, k) in keys.iter().enumerate() {
            state.cache_method(k.clone(), sample_target(i as u64));
        }
        assert_eq!(state.method_count(), 4);

        // Re-cache an existing key with a new value: must update in place,
        // not evict, and not grow.
        state.cache_method(keys[1].clone(), sample_target(999));
        assert_eq!(state.method_count(), 4);
        assert_eq!(
            state.resolve_method(&keys[1]),
            Some(sample_target(999)),
            "re-cache must update the value in place"
        );
    }

    // -- 2026-07-26 arch pass: wiring assertions -------------------------

    #[test]
    fn promoted_invoke_round_trip_is_the_only_live_path() {
        // Guards the module doc's claim about what is actually wired: the
        // promoted-invoke map is the live tier and its read path must not
        // require the class manager. If a future pass wires `global_methods`
        // / `global_fields` / `ThreadLocalResolveCache` into production, this
        // test's comment (and the module header) must be updated with it.
        let state = SharedResolutionState::new();
        let key: PromotedInvokeKey = (ClassId::new(1), 4, false, Some(ClassId::new(2)));
        assert!(state.get_promoted_invoke(&key).is_none());
        assert_eq!(state.promoted_hit_count(), 0);

        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert!(state.get_promoted_invoke(&key).is_some());
        assert_eq!(state.promoted_hit_count(), 1);
        assert_eq!(state.promoted_insert_count(), 1);

        // A miss must not be memoized as a negative anywhere: after
        // invalidation the same key simply re-misses and can be re-promoted.
        state.invalidate_promoted_for_class(ClassId::new(1));
        assert!(state.get_promoted_invoke(&key).is_none());
        state.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert!(state.get_promoted_invoke(&key).is_some());
    }

    #[test]
    fn invalidate_promoted_matches_invalidate_all_for_the_live_cache() {
        // CR-LR-1: `gc.rs`'s loader-unload sweep needs a wholesale clear of the
        // live cache. `invalidate_all` also clears two maps that have no
        // production writers; `invalidate_promoted` says what the call site
        // means. On the live cache the two must be indistinguishable.
        let make = || {
            let state = SharedResolutionState::new();
            for cp in 0..4u16 {
                let key: PromotedInvokeKey = (ClassId::new(1), cp, false, Some(ClassId::new(2)));
                state.insert_promoted_invoke(
                    key,
                    CachedInvokeTarget::VirtualBytecode {
                        receiver_class_id: ClassId::new(2),
                        cached: sample_bytecode_method(2),
                        gate: crate::classloading::resolution::RedefineGate::never_stale(),
                    },
                );
            }
            state
        };

        let via_all = make();
        let via_promoted = make();
        assert_eq!(via_all.promoted_invoke_count(), 4);
        assert_eq!(via_promoted.promoted_invoke_count(), 4);

        via_all.invalidate_all();
        via_promoted.invalidate_promoted();
        assert_eq!(via_all.promoted_invoke_count(), 0);
        assert_eq!(
            via_promoted.promoted_invoke_count(),
            via_all.promoted_invoke_count()
        );

        // ...and the two maps `invalidate_all` additionally clears are empty
        // either way, because nothing in production ever writes them.
        assert_eq!(via_promoted.method_count(), 0);
        assert_eq!(via_promoted.field_count(), 0);
        assert_eq!(via_all.method_count(), 0);
        assert_eq!(via_all.field_count(), 0);

        // A cleared key is a miss, not a memoized negative.
        let key: PromotedInvokeKey = (ClassId::new(1), 0, false, Some(ClassId::new(2)));
        assert!(via_promoted.get_promoted_invoke(&key).is_none());
        via_promoted.insert_promoted_invoke(
            key,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
        assert!(via_promoted.get_promoted_invoke(&key).is_some());
    }
}
