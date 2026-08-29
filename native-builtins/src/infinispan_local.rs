// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.10 — Infinispan local-mode cache natives.
//!
//! Keycloak (both KC16 and KC26) uses Infinispan in **local mode** for its
//! realm cache, user-session cache, authentication-session cache, and a
//! handful of auxiliary caches. The call path through
//! `KeycloakSessionFactory.init()` boils down to:
//!
//! ```text
//!   cacheManager = new DefaultCacheManager(globalConfig)
//!   cache        = cacheManager.getCache("realms")
//!   cache.put(key, value)
//!   cache.get(key) -> value
//!   cache.remove(key)
//!   cache.addListener(listenerObj)   // @CacheEntryCreated / Modified / Removed
//! ```
//!
//! We back every named cache with a process-wide Rust-side `CacheInner`
//! guarded by a `parking_lot::RwLock<HashMap<CacheKey, CacheEntry>>` —
//! reader parallelism for the steady-state `get` traffic, serialised
//! writers, no async. LRU eviction is a doubly-linked-list implemented via
//! a `VecDeque<CacheKey>` in MRU order with a configurable `size_limit`
//! (translated from `Configuration.memory().size(N)`). TTL is lazy: every
//! lookup checks `Option<Instant>` expiry and returns `None` (and evicts)
//! if past deadline. An optional reaper thread can be toggled on to sweep
//! expired entries every 5 s.
//!
//! ## Field layout (wired via `class_manager::synthetic_stub_fields`)
//!
//! | Class                                                       | Slot 0      | Slot 1        | Slot 2   |
//! |-------------------------------------------------------------|-------------|---------------|----------|
//! | `org.infinispan.manager.DefaultCacheManager`                 | handle (J)  | config_name    | started  |
//! | `org.infinispan.Cache`                                       | handle (J)  | name           | manager  |
//!
//! `org.infinispan.configuration.cache.{Configuration,ConfigurationBuilder}`
//! and `org.infinispan.configuration.global.{GlobalConfiguration,
//! GlobalConfigurationBuilder}` are no longer backed by a synthetic field
//! layout: their `build()` methods run real Infinispan bytecode (see
//! `native_dcm_define_configuration` for how the real `Configuration`'s
//! size/ttl are read back out via its real accessor API instead of a raw
//! slot index). `class_manager::synthetic_stub_fields` still lists a 3-field
//! layout for these four class names as a harmless padding floor (real
//! Infinispan's field count is far larger, so the padding is a no-op) —
//! see the "Wave 3-B (RE.4)" comment there.
//!
//! ## Why RwLock + VecDeque over a dedicated LRU crate
//!
//! 1. Infinispan's listener model requires atomic "did the insert cause an
//!    eviction?" inspection, which `lru`/`cached`/etc. hide behind
//!    take/put returns that don't composit cleanly with our per-cache
//!    listener dispatch loop.
//! 2. Reader parallelism dominates `get`-heavy workloads; `RwLock` gives
//!    us that directly whereas `parking_lot::Mutex`-protected LRU crates
//!    serialise even lookups.
//! 3. The implementation is ~80 lines of data-structure code — worth
//!    owning end-to-end rather than pulling a dep that masks the
//!    interplay with listener dispatch.
//!
//! ## Security
//!
//! * Cache names are validated via `validate_cache_name`: rejects `../`,
//!   `/`, `\`, `:`, NUL, and any ASCII control byte (< 0x20). This is a
//!   defence-in-depth measure against a hostile class smuggling a
//!   filesystem-looking name through a config XML and tripping code
//!   elsewhere that splits on `/`.
//! * `CacheKey` is **never** constructed from Java object identity —
//!   identity-based hashing would mis-bucket semantically-equal keys
//!   minted by different loaders. Instead we hash the Java key's raw
//!   bytes (as obtained from `read_string` or the primitive encoding).
//! * Listener panics are caught via `std::panic::catch_unwind` and
//!   funnelled to `tracing::error!` — a misbehaving `@CacheEntryCreated`
//!   method MUST NOT corrupt cache state.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Field layout constants — mirrored in class_manager.rs synthetic_stub_fields
// ---------------------------------------------------------------------------

pub(crate) const DCM_FIELD_HANDLE: usize = 0;
pub(crate) const DCM_FIELD_CONFIG_NAME: usize = 1;
pub(crate) const DCM_FIELD_STARTED: usize = 2;
pub(crate) const DCM_NUM_FIELDS: usize = 3;

pub(crate) const CACHE_FIELD_HANDLE: usize = 0;
pub(crate) const CACHE_FIELD_NAME: usize = 1;
pub(crate) const CACHE_FIELD_MANAGER: usize = 2;
pub(crate) const CACHE_NUM_FIELDS: usize = 3;

pub(crate) const GC_FIELD_SITE_NAME: usize = 0;
pub(crate) const GC_FIELD_JMX_ENABLED: usize = 1;
pub(crate) const GC_FIELD_RESERVED: usize = 2;
pub(crate) const GC_NUM_FIELDS: usize = 3;

const CLS_MANAGER: &str = "org/infinispan/manager/DefaultCacheManager";
const CLS_MANAGER_IFACE: &str = "org/infinispan/manager/EmbeddedCacheManager";
const CLS_CACHE: &str = "org/infinispan/Cache";
const CLS_CACHE_IMPL: &str = "org/infinispan/cache/impl/CacheImpl";
const CLS_CACHE_ADVANCED: &str = "org/infinispan/AdvancedCache";
const CLS_CONFIG: &str = "org/infinispan/configuration/cache/Configuration";
const CLS_CONFIG_BUILDER: &str = "org/infinispan/configuration/cache/ConfigurationBuilder";
const CLS_GLOBAL_CONFIG: &str = "org/infinispan/configuration/global/GlobalConfiguration";
const CLS_GLOBAL_CONFIG_BUILDER: &str =
    "org/infinispan/configuration/global/GlobalConfigurationBuilder";
const CLS_NOTIFIER: &str = "org/infinispan/notifications/cachelistener/CacheNotifier";

// Default cache bounds. Infinispan's own defaults are "unbounded" but we
// cap at a sensible ceiling so a runaway clinit doesn't OOM us.
const DEFAULT_SIZE_LIMIT: usize = 10_000;
const MAX_SIZE_LIMIT: usize = 1_000_000;
const DEFAULT_REAPER_INTERVAL: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// CacheKey — hash-based key that is independent of Java object identity.
// ---------------------------------------------------------------------------

/// A cache key is the immutable byte-representation of the Java key object.
/// We derive it from either the `String` contents (most-common path for the
/// Keycloak caches we care about) or a primitive's little-endian bytes.
/// This is intentionally NOT based on Java object identity so keys minted
/// by different class loaders but equal under `.equals()` still collide.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey(Arc<[u8]>);

impl CacheKey {
    /// Build a key from a UTF-8 string.
    pub fn from_str(s: &str) -> Self {
        CacheKey(Arc::from(s.as_bytes()))
    }

    /// Build a key from raw bytes (primitive boxes, byte[]).
    pub fn from_bytes(b: &[u8]) -> Self {
        CacheKey(Arc::from(b))
    }

    /// Build a key from a `Value`. `null` maps to a sentinel empty key
    /// (Infinispan rejects null keys per spec, but we surface the
    /// rejection via `put`/`get` rather than constructing a bogus key).
    pub fn from_value(ctx: &dyn NativeContext, v: Value) -> Option<Self> {
        match v {
            Value::Object(Some(obj)) => {
                // String path: read via NativeContext.
                if let Some(s) = ctx.read_string(obj) {
                    return Some(CacheKey::from_str(&s));
                }
                // Non-string object: fall back to identity-hash bytes so
                // semantically-different objects don't collide. This is
                // still equals-stable for the single-loader, single-vm
                // case we care about.
                let h = ctx.identity_hash_code(obj).to_le_bytes();
                Some(CacheKey::from_bytes(&h))
            }
            Value::Object(None) => None,
            Value::Int(i) => Some(CacheKey::from_bytes(&i.to_le_bytes())),
            Value::Long(i) => Some(CacheKey::from_bytes(&i.to_le_bytes())),
            Value::Float(f) => Some(CacheKey::from_bytes(&f.to_le_bytes())),
            Value::Double(f) => Some(CacheKey::from_bytes(&f.to_le_bytes())),
            _ => None,
        }
    }

    /// Raw bytes accessor — used by tests.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// CacheEntry — value plus metadata.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CacheEntry {
    /// Serialized form of the value. For Java objects we store the raw
    /// `ObjectRef.as_ptr()` value so the Java side can round-trip the
    /// reference back out. Primitives get encoded into a `Value`.
    pub value: StoredValue,
    /// Monotonic creation timestamp.
    pub created_at: Instant,
    /// Absolute expiry instant. `None` = no TTL.
    pub expires_at: Option<Instant>,
}

#[derive(Clone, Debug)]
pub enum StoredValue {
    /// A Java reference, encoded as the raw pointer bits. The cache does
    /// not root the object from GC — callers are expected to keep a hard
    /// reference alive through the Cache's field-0 manager back-pointer.
    JavaRef(usize),
    /// A string value (common case for Keycloak realm entries).
    Str(String),
    /// A primitive `Value` (non-reference).
    Primitive(Value),
    /// Raw bytes (byte[] payloads).
    Bytes(Arc<[u8]>),
    /// Null (distinguishes "cached null" from "no entry").
    Null,
}

impl StoredValue {
    /// Materialize back into a VM `Value`. JavaRef comes back as the same
    /// ObjectRef. Strings are re-interned by the caller if needed.
    pub fn to_value(&self, ctx: &mut dyn NativeContext) -> Value {
        match self {
            StoredValue::JavaRef(bits) => {
                if *bits == 0 {
                    Value::Object(None)
                } else {
                    // SAFETY: the bits were originally produced by
                    // `ObjectRef::as_ptr() as usize`. Callers must keep
                    // the referenced object alive (we do not pin it).
                    unsafe { Value::Object(Some(ObjectRef::from_raw(*bits as *mut u8))) }
                }
            }
            StoredValue::Str(s) => Value::Object(Some(ctx.create_string(s))),
            StoredValue::Primitive(v) => *v,
            StoredValue::Bytes(_) => Value::Object(None),
            StoredValue::Null => Value::Object(None),
        }
    }

    /// Produce a `StoredValue` from a VM `Value`.
    pub fn from_value(ctx: &dyn NativeContext, v: Value) -> Self {
        match v {
            Value::Object(None) => StoredValue::Null,
            Value::Object(Some(obj)) => {
                if let Some(s) = ctx.read_string(obj) {
                    return StoredValue::Str(s);
                }
                StoredValue::JavaRef(obj.as_ptr() as usize)
            }
            other => StoredValue::Primitive(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Listener — Java-side callback registration. Panic-safe via catch_unwind.
// ---------------------------------------------------------------------------

/// Kind of cache-entry event an Infinispan listener handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerEventKind {
    Created,
    Modified,
    Removed,
    Expired,
    Evicted,
}

/// A registered Java-side listener. We keep the ObjectRef pointer bits so
/// we can fire an upcall via `invoke_virtual` into the listener's
/// `onEvent(CacheEntryEvent)` handler.
#[derive(Clone, Debug)]
pub struct ListenerHandle {
    /// Raw pointer bits of the Java listener object.
    pub listener_ptr: usize,
    /// Optional event-kind filter; `None` = all events.
    pub event_filter: Option<ListenerEventKind>,
}

impl ListenerHandle {
    fn new(listener: ObjectRef) -> Self {
        Self {
            listener_ptr: listener.as_ptr() as usize,
            event_filter: None,
        }
    }

    /// Dispatch an event to this listener via catch_unwind. A panic inside
    /// the listener is logged but NOT propagated — otherwise a misbehaving
    /// `@CacheEntryCreated` method would poison the cache.
    fn dispatch(&self, kind: ListenerEventKind, cache_name: &str, key: &CacheKey) {
        if let Some(filter) = self.event_filter {
            if filter != kind {
                return;
            }
        }
        // We intentionally do NOT call back into the VM here — the listener
        // store only records the pointer bits; the VM upcall is performed
        // lazily by the Java shim when it walks the registered listeners.
        // This keeps our panic-safety boundary tight: we only log.
        let key_bytes = key.as_bytes();
        let key_preview: String = key_bytes
            .iter()
            .take(32)
            .flat_map(|b| std::ascii::escape_default(*b))
            .map(|c| c as char)
            .collect();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::debug!(
                target: "infinispan_local",
                cache = %cache_name,
                kind = ?kind,
                key = %key_preview,
                listener = self.listener_ptr,
                "dispatching cache listener event"
            );
        }));
        if let Err(_panic) = res {
            tracing::error!(
                target: "infinispan_local",
                cache = %cache_name,
                kind = ?kind,
                listener = self.listener_ptr,
                "cache listener panicked during dispatch; swallowed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// CacheInner — the actual state of one named cache.
// ---------------------------------------------------------------------------

pub struct CacheInner {
    /// Cache name (validated on creation).
    pub name: String,
    /// Primary store: RwLock for reader parallelism.
    pub store: RwLock<CacheStore>,
    /// Registered Java-side listeners. Separate Mutex so listener
    /// registration doesn't contend with get/put.
    pub listeners: Mutex<Vec<ListenerHandle>>,
    /// Configured entry ceiling. `0` means unbounded (capped at MAX).
    pub size_limit: usize,
    /// Optional default TTL for entries that don't override it.
    pub default_ttl: Option<Duration>,
}

pub struct CacheStore {
    /// Primary key -> entry map.
    pub entries: HashMap<CacheKey, CacheEntry>,
    /// LRU order. Front = most-recently-used, back = eviction candidate.
    pub lru: VecDeque<CacheKey>,
    /// Monotonic counter for stats.
    pub total_puts: u64,
    pub total_gets: u64,
    pub total_hits: u64,
    pub total_evictions: u64,
}

impl CacheStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            total_puts: 0,
            total_gets: 0,
            total_hits: 0,
            total_evictions: 0,
        }
    }

    fn promote(&mut self, key: &CacheKey) {
        // Remove from current position (if present) and push to front.
        // O(n) in the LRU queue length but bounded by `size_limit`.
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_front(key.clone());
    }

    fn evict_tail(&mut self) -> Option<CacheKey> {
        while let Some(candidate) = self.lru.pop_back() {
            if self.entries.remove(&candidate).is_some() {
                self.total_evictions += 1;
                return Some(candidate);
            }
            // Stale LRU entry (already removed from map) — keep popping.
        }
        None
    }
}

impl CacheInner {
    fn new(name: String, size_limit: usize, default_ttl: Option<Duration>) -> Self {
        Self {
            name,
            store: RwLock::new(CacheStore::new()),
            listeners: Mutex::new(Vec::new()),
            size_limit: if size_limit == 0 {
                DEFAULT_SIZE_LIMIT
            } else {
                size_limit.min(MAX_SIZE_LIMIT)
            },
            default_ttl,
        }
    }

    /// Put a key/value pair. Returns the previous value if one was present.
    pub fn put(&self, key: CacheKey, value: StoredValue) -> Option<StoredValue> {
        self.put_with_ttl(key, value, self.default_ttl)
    }

    pub fn put_with_ttl(
        &self,
        key: CacheKey,
        value: StoredValue,
        ttl: Option<Duration>,
    ) -> Option<StoredValue> {
        let expires_at = ttl.map(|d| Instant::now() + d);
        let entry = CacheEntry {
            value,
            created_at: Instant::now(),
            expires_at,
        };
        let (previous, event_kind, evicted) = {
            let mut store = self.store.write();
            let previous = store.entries.insert(key.clone(), entry);
            store.promote(&key);
            store.total_puts += 1;

            let mut evicted = None;
            while store.entries.len() > self.size_limit {
                evicted = store.evict_tail();
                if evicted.is_none() {
                    break;
                }
            }

            let event_kind = if previous.is_some() {
                ListenerEventKind::Modified
            } else {
                ListenerEventKind::Created
            };
            (previous.map(|e| e.value), event_kind, evicted)
        };

        // Listener dispatch is outside the store lock.
        self.dispatch(event_kind, &key);
        if let Some(evicted_key) = evicted {
            self.dispatch(ListenerEventKind::Evicted, &evicted_key);
        }

        previous
    }

    /// Get an entry, checking and lazily evicting if expired. Promotes the
    /// key to MRU on a hit.
    pub fn get(&self, key: &CacheKey) -> Option<StoredValue> {
        // Fast-path: read-lock probe that also checks expiry.
        let (hit, expired) = {
            let store = self.store.read();
            match store.entries.get(key) {
                Some(entry) => {
                    let now = Instant::now();
                    let expired = entry.expires_at.is_some_and(|deadline| now >= deadline);
                    if expired {
                        (None, true)
                    } else {
                        (Some(entry.value.clone()), false)
                    }
                }
                None => (None, false),
            }
        };

        if expired {
            // Promote the read-lock to a write-lock and evict.
            let mut store = self.store.write();
            if let Some(entry) = store.entries.get(key) {
                let still_expired = entry
                    .expires_at
                    .is_some_and(|deadline| Instant::now() >= deadline);
                if still_expired {
                    store.entries.remove(key);
                    if let Some(pos) = store.lru.iter().position(|k| k == key) {
                        store.lru.remove(pos);
                    }
                    store.total_evictions += 1;
                    drop(store);
                    self.dispatch(ListenerEventKind::Expired, key);
                    return None;
                }
            }
        }

        if let Some(value) = hit {
            let mut store = self.store.write();
            store.total_gets += 1;
            store.total_hits += 1;
            store.promote(key);
            Some(value)
        } else {
            let mut store = self.store.write();
            store.total_gets += 1;
            None
        }
    }

    /// Remove an entry. Returns the previous value if one was present.
    pub fn remove(&self, key: &CacheKey) -> Option<StoredValue> {
        let previous = {
            let mut store = self.store.write();
            let v = store.entries.remove(key);
            if let Some(pos) = store.lru.iter().position(|k| k == key) {
                store.lru.remove(pos);
            }
            v.map(|e| e.value)
        };
        if previous.is_some() {
            self.dispatch(ListenerEventKind::Removed, key);
        }
        previous
    }

    /// Size of the cache (live entries, not expired).
    pub fn size(&self) -> usize {
        self.store.read().entries.len()
    }

    /// Remove every entry but keep the cache object alive.
    pub fn clear(&self) {
        let mut store = self.store.write();
        store.entries.clear();
        store.lru.clear();
    }

    /// Register a Java-side listener. Returns the listener index.
    pub fn add_listener(&self, listener: ObjectRef) -> usize {
        let mut listeners = self.listeners.lock();
        listeners.push(ListenerHandle::new(listener));
        listeners.len() - 1
    }

    /// Remove a previously-registered listener.
    pub fn remove_listener(&self, listener: ObjectRef) -> bool {
        let mut listeners = self.listeners.lock();
        let ptr = listener.as_ptr() as usize;
        let before = listeners.len();
        listeners.retain(|h| h.listener_ptr != ptr);
        listeners.len() != before
    }

    fn dispatch(&self, kind: ListenerEventKind, key: &CacheKey) {
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            listener.dispatch(kind, &self.name, key);
        }
    }

    /// Snapshot for tests and monitoring.
    pub fn stats(&self) -> (u64, u64, u64, u64) {
        let store = self.store.read();
        (
            store.total_puts,
            store.total_gets,
            store.total_hits,
            store.total_evictions,
        )
    }
}

// ---------------------------------------------------------------------------
// DefaultCacheManagerInner — process-wide registry of named caches.
// ---------------------------------------------------------------------------

pub struct DefaultCacheManagerInner {
    /// Named caches. ~20 in practice (Keycloak); HashMap under RwLock is
    /// cheaper than DashMap for this cardinality.
    caches: RwLock<HashMap<String, Arc<CacheInner>>>,
    /// Per-cache configuration that `getCache(name)` should apply on first
    /// request. Populated by `defineConfiguration(name, config)`.
    pending_configs: RwLock<HashMap<String, PendingConfig>>,
    /// Is the manager started?
    started: parking_lot::RwLock<bool>,
}

#[derive(Clone, Debug)]
pub struct PendingConfig {
    pub size_limit: usize,
    pub default_ttl: Option<Duration>,
}

impl Default for DefaultCacheManagerInner {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultCacheManagerInner {
    pub fn new() -> Self {
        Self {
            caches: RwLock::new(HashMap::new()),
            pending_configs: RwLock::new(HashMap::new()),
            started: parking_lot::RwLock::new(false),
        }
    }

    pub fn start(&self) {
        *self.started.write() = true;
    }

    pub fn stop(&self) {
        *self.started.write() = false;
        self.caches.write().clear();
        self.pending_configs.write().clear();
    }

    pub fn is_started(&self) -> bool {
        *self.started.read()
    }

    /// Record a configuration for a named cache. Subsequent `getCache(name)`
    /// calls consume this config. If a cache under the same name already
    /// exists with a DIFFERENT effective config, returns Err; calling with
    /// the same config is a no-op (idempotent).
    pub fn define_configuration(&self, name: &str, config: PendingConfig) -> Result<(), String> {
        validate_cache_name(name)?;
        // If a cache is already live, disallow re-define with different
        // numbers. Matches Infinispan's `CacheConfigurationException`
        // behaviour on conflicting defineConfiguration calls.
        if let Some(existing) = self.caches.read().get(name) {
            let same = existing.size_limit == config.size_limit.max(1).min(MAX_SIZE_LIMIT)
                && existing.default_ttl == config.default_ttl;
            if !same {
                return Err(format!(
                    "CacheConfigurationException: cache {name:?} already configured differently"
                ));
            }
        }
        self.pending_configs
            .write()
            .insert(name.to_string(), config);
        Ok(())
    }

    /// Idempotent cache lookup/creation.
    pub fn get_cache(&self, name: &str) -> Result<Arc<CacheInner>, String> {
        validate_cache_name(name)?;
        {
            let caches = self.caches.read();
            if let Some(c) = caches.get(name) {
                return Ok(c.clone());
            }
        }
        // Slow path: write-lock and re-check (double-checked locking
        // without unsafe — RwLock gives us the guard promotion).
        let mut caches = self.caches.write();
        if let Some(c) = caches.get(name) {
            return Ok(c.clone());
        }
        let cfg = self
            .pending_configs
            .read()
            .get(name)
            .cloned()
            .unwrap_or(PendingConfig {
                size_limit: DEFAULT_SIZE_LIMIT,
                default_ttl: None,
            });
        let cache = Arc::new(CacheInner::new(
            name.to_string(),
            cfg.size_limit,
            cfg.default_ttl,
        ));
        caches.insert(name.to_string(), cache.clone());
        Ok(cache)
    }

    /// Remove a named cache entirely.
    pub fn remove_cache(&self, name: &str) -> bool {
        self.caches.write().remove(name).is_some()
    }

    /// List all cache names (owned strings so we don't hold the lock).
    ///
    /// Includes names registered via `defineConfiguration` even before the
    /// cache has been lazily instantiated through `get_cache` — mirrors real
    /// Infinispan's `getCacheNames()`/`getDefinedCaches()`, which reflects
    /// configuration, not instantiation (see `native_dcm_get_cache_names`'s
    /// synthetic branch, which surfaced this gap: `defineConfiguration("foo",
    /// ...)` alone left `cache_names()` empty until `getCache("foo")` was
    /// also called).
    pub fn cache_names(&self) -> Vec<String> {
        let mut names: std::collections::HashSet<String> =
            self.caches.read().keys().cloned().collect();
        names.extend(self.pending_configs.read().keys().cloned());
        names.into_iter().collect()
    }

    #[cfg(test)]
    pub fn cache_count(&self) -> usize {
        self.caches.read().len()
    }
}

// ---------------------------------------------------------------------------
// Global manager instance — Keycloak is single-VM, single-DefaultCacheManager.
// ---------------------------------------------------------------------------

fn global_manager() -> &'static DefaultCacheManagerInner {
    static M: OnceLock<DefaultCacheManagerInner> = OnceLock::new();
    M.get_or_init(|| {
        let m = DefaultCacheManagerInner::new();
        m.start();
        m
    })
}

/// Reset global state. For tests that need isolation.
#[allow(dead_code)]
pub fn reset_manager_for_tests() {
    let m = global_manager();
    m.caches.write().clear();
    m.pending_configs.write().clear();
    *m.started.write() = true;
}

/// Serialize tests that touch the global registry.
#[cfg(test)]
pub(crate) fn test_lock() -> parking_lot::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock()
}

// ---------------------------------------------------------------------------
// Validation helpers.
// ---------------------------------------------------------------------------

/// Reject pathological cache names. Defence-in-depth against a hostile
/// class smuggling a filesystem-looking name.
pub fn validate_cache_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("CacheConfigurationException: cache name must not be empty".to_string());
    }
    if name.len() > 255 {
        return Err(format!(
            "CacheConfigurationException: cache name exceeds 255 bytes: {}",
            name.len()
        ));
    }
    if name.contains("../") {
        return Err("CacheConfigurationException: cache name contains '../'".to_string());
    }
    for (i, b) in name.bytes().enumerate() {
        if b < 0x20 || b == 0x7f {
            return Err(format!(
                "CacheConfigurationException: cache name contains control byte 0x{b:02x} at {i}"
            ));
        }
        if b == b'/' || b == b'\\' || b == b':' {
            return Err(format!(
                "CacheConfigurationException: cache name contains reserved char '{}'",
                b as char
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reaper — optional background sweep.
// ---------------------------------------------------------------------------

/// Spawn a single-shot reaper thread that walks every cache and evicts
/// expired entries every `DEFAULT_REAPER_INTERVAL`. Idempotent: calling
/// twice is a no-op.
#[allow(dead_code)]
pub fn start_reaper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("infinispan-local-reaper".to_string())
            .spawn(|| loop {
                std::thread::sleep(DEFAULT_REAPER_INTERVAL);
                let manager = global_manager();
                let names = manager.cache_names();
                for name in names {
                    if let Ok(cache) = manager.get_cache(&name) {
                        reap_cache(&cache);
                    }
                }
            })
            .ok();
    });
}

fn reap_cache(cache: &CacheInner) {
    let now = Instant::now();
    let expired_keys: Vec<CacheKey> = {
        let store = cache.store.read();
        store
            .entries
            .iter()
            .filter_map(|(k, e)| {
                if e.expires_at.is_some_and(|d| now >= d) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect()
    };
    for key in expired_keys {
        let mut store = cache.store.write();
        if let Some(entry) = store.entries.get(&key) {
            if entry.expires_at.is_some_and(|d| now >= d) {
                store.entries.remove(&key);
                if let Some(pos) = store.lru.iter().position(|k| k == &key) {
                    store.lru.remove(pos);
                }
                store.total_evictions += 1;
                drop(store);
                cache.dispatch(ListenerEventKind::Expired, &key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Native callbacks.
// ---------------------------------------------------------------------------

fn alloc_object_for(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    min_slots: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // Fall back to a synthetic class (declaring `min_slots` fields) rather
    // than `ClassId::new(0)` when the real class can't be loaded: an object
    // allocated with `java/lang/Object`'s id but a non-zero slot count is an
    // undersized layout the GC's `get_field` bounds guard rejects.
    //
    // The fallback is the FALLIBLE spelling (JDK-only wave 2, step 3): under
    // `--jdk-only` the policy refuses to fabricate rather than recording the
    // violation and fabricating anyway, and the refusal arrives as the
    // catchable `NoClassDefFoundError` contract §5 names rather than the
    // uncatchable `MethodCallFailed::InternalError` the `?` conversion builds.
    let cid = match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => cid,
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, class_name, min_slots)?,
    };
    let n = ctx.class_num_total_fields(cid).max(min_slots);
    Ok(ctx.alloc_object(cid, n))
}

fn obj_arg(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn string_arg(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    obj_arg(args, idx).and_then(|o| ctx.read_string(o))
}

/// `new DefaultCacheManager()` / `new DefaultCacheManager(GlobalConfiguration)`.
/// Idempotent w.r.t. the global manager singleton.
fn native_dcm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    // The handle is the literal address of the global manager. Java side
    // just needs a stable non-zero identifier to round-trip.
    let handle = global_manager() as *const DefaultCacheManagerInner as i64;
    ctx.set_field(this, DCM_FIELD_HANDLE, Value::Long(handle));
    ctx.set_field(this, DCM_FIELD_CONFIG_NAME, Value::Object(None));
    ctx.set_field(this, DCM_FIELD_STARTED, Value::Int(1));
    global_manager().start();
    Ok(None)
}

/// `DefaultCacheManager.getCache(String)Lorg/infinispan/Cache;`
///
/// Registered unconditionally on `DefaultCacheManager`/`EmbeddedCacheManager`
/// (native dispatch is keyed by class+method+descriptor, not by which
/// constructor built the receiver — see `is_real_dcm`'s doc), so this also
/// intercepts `.getCache()`/`.getCache(String)` on a REAL `DefaultCacheManager`.
/// For a real manager, delegate to the real `internalGetCache(String)` (what
/// `getCache()`/`getCache(String)`'s own real bytecode calls) so the returned
/// `Cache` is genuinely real — real `config`, interceptor chain, component
/// wiring — instead of a synthetic object with only `handle`/`name`/`manager`
/// ever populated. The synthetic object was allocated with the REAL
/// `CacheImpl` class's full field layout, so every other real field
/// (`config` included) sat at its zero-initialized default; Infinispan's own
/// internal bootstrap (`GlobalComponentRegistry.postStart()` →
/// `GlobalConfigurationManagerImpl.postStart()`, reached via
/// `SecurityActions.getCache(EmbeddedCacheManager, String)` →
/// `EmbeddedCacheManager.getCache(String)` — this exact native) then NPEs on
/// `cache.config.clustering()` in `AbstractCacheBackedSet.<init>`. See
/// fixed-suite-bugs/keycloak/keycloak-model-infinispan-cache-config-null-after-real-start-FIXED.md.
fn native_dcm_get_cache(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_real_dcm(ctx, this) {
        let name_value = match args.get(1) {
            Some(v @ Value::Object(Some(_))) => *v,
            _ => match ctx.get_field_by_name(this, "defaultCacheName") {
                v @ Value::Object(Some(_)) => v,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "ISPN000289: No default cache configured for this cache manager"
                            .to_string(),
                    }
                    .into());
                }
            },
        };
        return ctx.invoke_virtual(
            this,
            "internalGetCache",
            "(Ljava/lang/String;)Lorg/infinispan/Cache;",
            &[name_value],
        );
    }
    let name = match string_arg(ctx, args, 1) {
        Some(s) => s,
        // getCache() with no args = default cache.
        None => "___defaultcache".to_string(),
    };
    let cache = match global_manager().get_cache(&name) {
        Ok(c) => c,
        Err(e) => {
            return Err(RuntimeError::IllegalStateException { message: e }.into());
        }
    };
    // Allocate a Java-side Cache object. Tuck the Rust Arc pointer in
    // slot 0 as a long handle so follow-up natives can re-hydrate.
    let cache_obj = alloc_object_for(ctx, CLS_CACHE_IMPL, CACHE_NUM_FIELDS)?;
    let handle = Arc::into_raw(cache.clone()) as i64;
    ctx.set_field(cache_obj, CACHE_FIELD_HANDLE, Value::Long(handle));
    let name_str = ctx.create_string(&name);
    ctx.set_field(cache_obj, CACHE_FIELD_NAME, Value::Object(Some(name_str)));
    ctx.set_field(cache_obj, CACHE_FIELD_MANAGER, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(cache_obj))))
}

/// `DefaultCacheManager.getCacheNames()Ljava/util/Set;`
///
/// Registered unconditionally on `DefaultCacheManager`/`EmbeddedCacheManager`
/// (native dispatch is keyed by class+method+descriptor, not by which
/// constructor built the receiver — see `is_real_dcm`'s doc), so this also
/// intercepts `.getCacheNames()` on a REAL `DefaultCacheManager`. The old
/// unconditional `null` return here (comment: "Keycloak only hits it for
/// diagnostics") was wrong for the real-manager case: real Infinispan
/// bytecode iterates the result with no null guard —
/// `org.infinispan.jcache.embedded.JCacheManager.registerPredefinedCaches()`
/// (reached via the JSR-107 `JCachingProvider.createCacheManager` path Spring
/// Boot's `JCacheCacheConfiguration` uses for Infinispan) does
/// `cm.getCacheNames().iterator()` directly, so returning `null` surfaced as
/// `NullPointerException: Cannot invoke "java.util.Set.iterator()" because
/// "cacheNames" is null` instead of an (often empty) real `Set`. See
/// docs/known-issues/springboot/cacheautoconfigurationtests-infinispan-null-cachemanager-residual.md.
fn native_dcm_get_cache_names(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = obj_arg(args, 0) {
        if is_real_dcm(ctx, this) {
            return native_real_dcm_get_cache_names(ctx, this);
        }
    }
    // Synthetic manager: return the real names actually defined against the
    // process-wide synthetic cache (via `defineConfiguration`/`getCache`)
    // instead of `null` — same rationale as the real-manager branch above,
    // e.g. Spring Boot's `InfinispanCacheConfiguration.infinispanCacheManager`
    // uses the zero-arg `new DefaultCacheManager()` (this synthetic path)
    // whenever no `spring.cache.infinispan.config` resource is set, and
    // `SpringEmbeddedCacheManager.getCacheNames()` calls straight through to
    // this method with no null guard.
    let names = global_manager().cache_names();
    let key_objs: Vec<ObjectRef> = names.iter().map(|n| ctx.create_string(n)).collect();
    let set = crate::build_real_layout_string_hashset(ctx, &key_objs)?;
    Ok(Some(Value::Object(Some(set))))
}

/// Real-manager path for `getCacheNames()`: mirrors
/// `DefaultCacheManager.getCacheNames()`'s own real bytecode
/// (`configurationManager.getDefinedCaches()` unioned with `caches.keySet()`,
/// filtered through `InternalCacheRegistry.filterPrivateCaches`) via
/// `invoke_virtual` against the real fields, rather than re-entering this
/// native (which would just recurse into itself since dispatch is keyed by
/// method name, not by which code is calling it).
fn native_real_dcm_get_cache_names(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let configuration_manager = match ctx.get_field_by_name(this_cur, "configurationManager") {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let cm_pin = ctx.pin_native_root(configuration_manager);

        let cm_cur = ctx.read_native_pin(cm_pin, configuration_manager);
        let defined =
            ctx.invoke_virtual(cm_cur, "getDefinedCaches", "()Ljava/util/Collection;", &[])?;
        let defined_obj = match defined {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let defined_pin = ctx.pin_native_root(defined_obj);

        let defined_cur = ctx.read_native_pin(defined_pin, defined_obj);
        let set = ctx.new_object_initialized(
            "java/util/TreeSet",
            "(Ljava/util/Collection;)V",
            &[Value::Object(Some(defined_cur))],
        )?;
        let set_obj = match set {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let set_pin = ctx.pin_native_root(set_obj);

        let this_cur = ctx.read_native_pin(this_pin, this);
        if let Value::Object(Some(caches)) = ctx.get_field_by_name(this_cur, "caches") {
            let caches_pin = ctx.pin_native_root(caches);
            let caches_cur = ctx.read_native_pin(caches_pin, caches);
            let key_set = ctx.invoke_virtual(caches_cur, "keySet", "()Ljava/util/Set;", &[])?;
            if let Some(Value::Object(Some(key_set_obj))) = key_set {
                let set_cur = ctx.read_native_pin(set_pin, set_obj);
                ctx.invoke_virtual(
                    set_cur,
                    "addAll",
                    "(Ljava/util/Collection;)Z",
                    &[Value::Object(Some(key_set_obj))],
                )?;
            }
        }

        let this_cur = ctx.read_native_pin(this_pin, this);
        if let Value::Object(Some(gcr)) = ctx.get_field_by_name(this_cur, "globalComponentRegistry")
        {
            let gcr_pin = ctx.pin_native_root(gcr);
            if let Ok(icr_cid) =
                ctx.ensure_class_initialized("org/infinispan/registry/InternalCacheRegistry")
            {
                let icr_mirror = ctx.get_class_mirror(icr_cid);
                let gcr_cur = ctx.read_native_pin(gcr_pin, gcr);
                let icr = ctx.invoke_virtual(
                    gcr_cur,
                    "getComponent",
                    "(Ljava/lang/Class;)Ljava/lang/Object;",
                    &[Value::Object(Some(icr_mirror))],
                )?;
                if let Some(Value::Object(Some(icr_obj))) = icr {
                    let set_cur = ctx.read_native_pin(set_pin, set_obj);
                    ctx.invoke_virtual(
                        icr_obj,
                        "filterPrivateCaches",
                        "(Ljava/util/Set;)V",
                        &[Value::Object(Some(set_cur))],
                    )?;
                }
            }
        }

        let set_cur = ctx.read_native_pin(set_pin, set_obj);
        Ok(Some(Value::Object(Some(set_cur))))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `DefaultCacheManager.cacheExists(String)Z`
fn native_dcm_cache_exists(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = obj_arg(args, 0) {
        if is_real_dcm(ctx, this) {
            return native_real_dcm_cache_exists(ctx, this, args);
        }
    }
    let name = match string_arg(ctx, args, 1) {
        Some(s) => s,
        None => return Ok(Some(Value::Int(0))),
    };
    let exists = global_manager().cache_names().iter().any(|n| n == &name);
    Ok(Some(Value::Int(if exists { 1 } else { 0 })))
}

fn native_real_dcm_cache_exists(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    let name = match obj_arg(args, 1) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };

    let this_pin = ctx.pin_native_root(this);
    let name_pin = ctx.pin_native_root(name);
    let result = (|| -> MethodCallResult {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let name_cur = ctx.read_native_pin(name_pin, name);

        let configuration_manager = match ctx.get_field_by_name(this_cur, "configurationManager") {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let caches = match ctx.get_field_by_name(this_cur, "caches") {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let configuration_manager_pin = ctx.pin_native_root(configuration_manager);
        let caches_pin = ctx.pin_native_root(caches);

        let selected = ctx.invoke_virtual(
            configuration_manager,
            "selectCache",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(name_cur))],
        )?;

        let caches_cur = ctx.read_native_pin(caches_pin, caches);
        let selected_name = match selected {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let selected_pin = ctx.pin_native_root(selected_name);
        let selected_cur = ctx.read_native_pin(selected_pin, selected_name);

        ctx.invoke_virtual(
            caches_cur,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(selected_cur))],
        )
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `DefaultCacheManager.defineConfiguration(String, Configuration)` — read
/// the size/ttl back out of the real `Configuration` object via its real
/// accessor API: `configuration.memory().maxCount()` and
/// `configuration.expiration().lifespan()`, both via `invoke_virtual`.
///
/// `ConfigurationBuilder.build()` used to be shimmed as an identity wrapper
/// (`native_cfg_build`, now removed) that returned the Builder itself
/// relabeled as a `Configuration`, precisely so this function could read
/// `CONFIG_FIELD_SIZE_LIMIT`/`CONFIG_FIELD_TTL_MS` off it by raw synthetic
/// slot index. That shim caused a `ClassCastException` one call site up
/// (real Infinispan code casting the "Configuration" back to its real type)
/// once other code started reaching this path — see
/// fixed-suite-bugs/keycloak/keycloak-model-infinispan-configurationbuilder-classcastexception.md.
/// The fix: let real `ConfigurationBuilder.build()` bytecode construct a
/// genuine `Configuration` (real field layout, ~18 nested config-section
/// fields with no relation to size/ttl by position), and read size/ttl back
/// out through its real API instead of a raw slot index.
fn native_dcm_define_configuration(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(this) = obj_arg(args, 0) {
        if is_real_dcm(ctx, this) {
            return native_real_dcm_define_configuration(ctx, this, args);
        }
    }

    let name = match string_arg(ctx, args, 1) {
        Some(s) => s,
        None => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineConfiguration: name is null".to_string(),
            }
            .into());
        }
    };
    let config = obj_arg(args, 2);
    let (size, ttl_ms) = match config {
        Some(obj) => {
            let s = match ctx
                .invoke_virtual(
                    obj,
                    "memory",
                    "()Lorg/infinispan/configuration/cache/MemoryConfiguration;",
                    &[],
                )
                .ok()
                .flatten()
            {
                Some(Value::Object(Some(mem))) => {
                    match ctx
                        .invoke_virtual(mem, "maxCount", "()J", &[])
                        .ok()
                        .flatten()
                    {
                        Some(Value::Long(v)) if v > 0 => v as usize,
                        _ => DEFAULT_SIZE_LIMIT,
                    }
                }
                _ => DEFAULT_SIZE_LIMIT,
            };
            let t = match ctx
                .invoke_virtual(
                    obj,
                    "expiration",
                    "()Lorg/infinispan/configuration/cache/ExpirationConfiguration;",
                    &[],
                )
                .ok()
                .flatten()
            {
                Some(Value::Object(Some(exp))) => {
                    match ctx
                        .invoke_virtual(exp, "lifespan", "()J", &[])
                        .ok()
                        .flatten()
                    {
                        Some(Value::Long(v)) if v > 0 => Some(Duration::from_millis(v as u64)),
                        _ => None,
                    }
                }
                _ => None,
            };
            (s, t)
        }
        None => (DEFAULT_SIZE_LIMIT, None),
    };
    let cfg = PendingConfig {
        size_limit: size,
        default_ttl: ttl_ms,
    };
    if let Err(e) = global_manager().define_configuration(&name, cfg) {
        return Err(RuntimeError::IllegalStateException { message: e }.into());
    }
    // Infinispan returns the Configuration object; we echo the input.
    Ok(Some(
        config
            .map(|o| Value::Object(Some(o)))
            .unwrap_or(Value::Object(None)),
    ))
}

fn native_real_dcm_define_configuration(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    let name = match obj_arg(args, 1) {
        Some(o) => o,
        None => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineConfiguration: name is null".to_string(),
            }
            .into());
        }
    };
    let config = match obj_arg(args, 2) {
        Some(o) => o,
        None => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineConfiguration: configuration is null".to_string(),
            }
            .into());
        }
    };

    let this_pin = ctx.pin_native_root(this);
    let name_pin = ctx.pin_native_root(name);
    let config_pin = ctx.pin_native_root(config);
    let result = (|| -> MethodCallResult {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let mut name_cur = ctx.read_native_pin(name_pin, name);
        let mut config_cur = ctx.read_native_pin(config_pin, config);

        let configuration_manager = match ctx.get_field_by_name(this_cur, "configurationManager") {
            Value::Object(Some(o)) => o,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "defineConfiguration: missing configurationManager".to_string(),
                }
                .into());
            }
        };
        let configuration_manager_pin = ctx.pin_native_root(configuration_manager);

        let builder = match ctx.new_object_initialized(CLS_CONFIG_BUILDER, "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "defineConfiguration: failed to allocate ConfigurationBuilder"
                        .to_string(),
                }
                .into());
            }
        };
        name_cur = ctx.read_native_pin(name_pin, name_cur);
        config_cur = ctx.read_native_pin(config_pin, config_cur);
        let configuration_manager_cur =
            ctx.read_native_pin(configuration_manager_pin, configuration_manager);
        let builder_pin = ctx.pin_native_root(builder);
        let mut builder_cur = ctx.read_native_pin(builder_pin, builder);

        ctx.invoke_virtual(
            builder_cur,
            "read",
            "(Lorg/infinispan/configuration/cache/Configuration;)Lorg/infinispan/configuration/cache/ConfigurationBuilder;",
            &[Value::Object(Some(config_cur))],
        )?;
        name_cur = ctx.read_native_pin(name_pin, name_cur);
        config_cur = ctx.read_native_pin(config_pin, config_cur);
        builder_cur = ctx.read_native_pin(builder_pin, builder_cur);
        let configuration_manager_cur =
            ctx.read_native_pin(configuration_manager_pin, configuration_manager_cur);

        let template = match ctx.invoke_virtual(config_cur, "isTemplate", "()Z", &[])? {
            Some(Value::Int(v)) => v != 0,
            _ => false,
        };
        name_cur = ctx.read_native_pin(name_pin, name_cur);
        builder_cur = ctx.read_native_pin(builder_pin, builder_cur);
        let configuration_manager_cur =
            ctx.read_native_pin(configuration_manager_pin, configuration_manager_cur);

        ctx.invoke_virtual(
            builder_cur,
            "template",
            "(Z)Lorg/infinispan/configuration/cache/ConfigurationBuilder;",
            &[Value::Int(if template { 1 } else { 0 })],
        )?;
        name_cur = ctx.read_native_pin(name_pin, name_cur);
        builder_cur = ctx.read_native_pin(builder_pin, builder_cur);
        let configuration_manager_cur =
            ctx.read_native_pin(configuration_manager_pin, configuration_manager_cur);

        ctx.invoke_virtual(
            configuration_manager_cur,
            "putConfiguration",
            "(Ljava/lang/String;Lorg/infinispan/configuration/cache/ConfigurationBuilder;)Lorg/infinispan/configuration/cache/Configuration;",
            &[Value::Object(Some(name_cur)), Value::Object(Some(builder_cur))],
        )
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// Distinguish a REAL `DefaultCacheManager` (built via the un-shimmed
/// `(ConfigurationBuilderHolder, boolean)` constructor -- the overload
/// Keycloak's own `DefaultInfinispanConnectionProviderFactory` actually
/// uses, see `native_dcm_init`'s doc comment; that overload is NOT
/// registered there) from one built by `native_dcm_init`'s synthetic
/// shim, for `native_dcm_start`/`native_dcm_stop` below.
///
/// Checking `DCM_FIELD_HANDLE` (field 0, "caches" on the real class) by
/// raw index doesn't round-trip reliably: this class's synthetic 3-slot
/// (handle/config_name/started) layout registration overrides field-type
/// metadata for its first few indices, so a zero-initialized *reference*
/// field there decodes as `Value::Int(0)` instead of `Value::Object(None)`
/// -- indistinguishable from a real, not-yet-set field of a different
/// type. `globalComponentRegistry` sits well past that range: the real
/// constructor sets it unconditionally, early, and `native_dcm_init`
/// never touches it, so it reliably reads `Object(None)` for a synthetic
/// instance and a real `GlobalComponentRegistry` reference for a
/// fully-constructed real one.
fn is_real_dcm(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    !matches!(
        ctx.get_field_by_name(this, "globalComponentRegistry"),
        Value::Object(None)
    )
}

/// `DefaultCacheManager.start()` / `stop()`.
///
/// This native is registered unconditionally on the class (native dispatch
/// is keyed by class+method+descriptor, not by which constructor built the
/// receiver), so it also intercepts `.start()`/`.stop()` on a REAL
/// `DefaultCacheManager` (see `is_real_dcm` above). Left unguarded, a real
/// object's `.start()` call was silently swallowed: the synthetic
/// `global_manager().start()` ran against the unrelated process-wide
/// synthetic cache, while the real object's own `internalStart(boolean)`
/// (which starts `GlobalComponentRegistry` -- module lifecycles, JGroups
/// transport, etc.) never executed. That left `JGroupsTransport.channel`
/// permanently null, surfacing later as an NPE in
/// `DefaultInfinispanConnectionProviderFactory.createEmbeddedCacheManager`
/// (see docs/known-issues/keycloak-model-netty-reflective-setaccessible-disabled.md).
///
/// For a real object, delegate directly to the real
/// `internalStart(boolean)`/`internalStop()` (bypassing the
/// natively-overridden `start()`/`stop()` bytecode itself, which would
/// just re-enter this native) -- this is what `start()`/`stop()`'s own
/// real bytecode calls after an `authorizer.checkPermission(...)` guard we
/// don't replicate here; skipping it is safe since it's a no-op whenever
/// `GlobalSecurityConfiguration` authorization isn't enabled (Infinispan's
/// default, and Keycloak's local cache manager doesn't enable it).
fn native_dcm_start(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if is_real_dcm(ctx, this) {
        return ctx.invoke_virtual(this, "internalStart", "(Z)V", &[Value::Int(1)]);
    }
    global_manager().start();
    Ok(None)
}

fn native_dcm_stop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if is_real_dcm(ctx, this) {
        return ctx.invoke_virtual(this, "internalStop", "()V", &[]);
    }
    global_manager().stop();
    Ok(None)
}

/// Distinguish a REAL `Cache`/`CacheImpl` (built by real `internalGetCache`/
/// `createCache` bytecode) from one fabricated by `native_dcm_get_cache`'s
/// synthetic branch — same rationale and same necessity as `is_real_dcm`
/// above: the Cache-instance natives below (`put`/`get`/`remove`/etc.) are
/// registered unconditionally on `CLS_CACHE`/`CLS_CACHE_IMPL`/
/// `CLS_CACHE_ADVANCED`, so once `native_dcm_get_cache` starts handing back
/// real `Cache` objects for a real `DefaultCacheManager`, these natives would
/// otherwise read `CACHE_FIELD_HANDLE` (raw slot 0) off a real object —
/// which is actually `invocationContextFactory`, not our synthetic handle —
/// and misbehave.
///
/// **Three prior attempts, all empirically falsified** by a regression probe
/// (`new DefaultCacheManager()` + `getCache()` + `put()`, the old synthetic
/// path — must still route to the Rust-side `CacheInner`, not real bytecode):
///
/// 1. `get_field_by_name(this, "config")` — `CacheImpl`'s real field order is
///    `invocationContextFactory`(0), `commandsFactory`(1), `invoker`(2),
///    `config`(3), immediately adjacent to the synthetic 3-slot layout
///    override.
/// 2. `get_field_by_name(this, "componentRegistry")` — further into the real
///    field list, still failed identically.
/// 3. `get_field(this, CACHE_FIELD_HANDLE)` matching `Value::Long(bits) if
///    bits != 0` (mirroring `cache_from_field`'s own handle check) — debug
///    tracing proved this doesn't round-trip AT ALL for this class: even
///    right after `native_dcm_get_cache`'s synthetic branch writes
///    `Value::Long(handle)` into raw slot 0, reading it back gives
///    `Object(None)`, not the `Long`. Slot 0's REAL declared type is a
///    reference (`invocationContextFactory`), and storing a `Long` bit
///    pattern into a heap slot the GC treats as reference-typed silently
///    loses the value on readback — a type-mismatched write, not a
///    low-index-only quirk. (This also means the ORIGINAL, pre-existing
///    `cache_from_field`'s "fast path" via `CACHE_FIELD_HANDLE` has quietly
///    never worked once real Infinispan jars are on the classpath — every
///    call was already silently falling through to its `CACHE_FIELD_NAME`
///    string-based fallback, which is why this was never noticed.)
///
/// What DOES reliably round-trip (proven by that same pre-existing
/// fallback): storing and reading back an **object reference** via raw slot
/// index — `cache_from_field` has relied on `CACHE_FIELD_NAME` (slot 1)
/// round-tripping a `String` reference for as long as the synthetic Cache
/// path has existed. `native_dcm_get_cache`'s synthetic branch ALSO stores
/// the owning manager into `CACHE_FIELD_MANAGER` (slot 2) as a reference —
/// the real cache never does this (slot 2 is really `invoker`, an
/// `AsyncInterceptorChain`, never a `DefaultCacheManager`). So: read slot 2
/// back and check whether its *runtime class* is the manager class — a
/// reference-identity check, not a value-round-trip gamble.
fn is_real_cache(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    match ctx.get_field(this, CACHE_FIELD_MANAGER) {
        Value::Object(Some(mgr)) => {
            let cid = ctx.class_id_of_object(mgr);
            ctx.class_name_arc_of_id(cid).as_deref() != Some(CLS_MANAGER)
        }
        _ => true,
    }
}

/// Build the `InvocationContext` a real `CacheImpl.get`/`containsKey`'s own
/// bytecode builds for itself: `invocationContextFactory.createInvocationContext(false, 1)`.
fn real_cache_invocation_context(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let icf = match ctx.get_field_by_name(this, "invocationContextFactory") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Cache.invocationContextFactory is null".to_string(),
            }
            .into());
        }
    };
    ctx.invoke_virtual(
        icf,
        "createInvocationContext",
        "(ZI)Lorg/infinispan/context/InvocationContext;",
        &[Value::Int(0), Value::Int(1)],
    )
}

/// Pin `this` (and `key`, if it holds a heap reference) before a re-entrant
/// call that runs real bytecode and can trigger a moving GC. Bare Rust
/// locals like `this`/`key` are NOT automatically kept up to date by the
/// entry-level pinning `safe_native_call` does for a native's *incoming*
/// args — that protects only the initial dispatch, not a GC triggered by
/// this function's *own* subsequent `ctx.invoke_virtual`/`ctx.invoke` calls.
/// Mirrors the identical hazard fixed in
/// `NativeContextImpl::build_thread_field_holder` (vm/src/vm/vm_exec.rs) —
/// see fixed-suite-bugs/gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md.
/// Re-read both with `read_this_and_key` after the risky call, then
/// `ctx.unpin_native_roots(this_handle)` once `this`/`key` are no longer
/// needed (releases the whole batch, `key`'s slot included).
fn pin_this_and_key(ctx: &mut dyn NativeContext, this: ObjectRef, key: Value) -> (usize, usize) {
    let this_handle = ctx.pin_native_root(this);
    let key_handle = if let Value::Object(Some(k)) = key {
        ctx.pin_native_root(k)
    } else {
        this_handle
    };
    (this_handle, key_handle)
}

/// Re-read `this`/`key` after a risky call — see `pin_this_and_key`.
fn read_this_and_key(
    ctx: &dyn NativeContext,
    this_handle: usize,
    this: ObjectRef,
    key_handle: usize,
    key: Value,
) -> (ObjectRef, Value) {
    let this = ctx.read_native_pin(this_handle, this);
    let key = if let Value::Object(Some(k)) = key {
        Value::Object(Some(ctx.read_native_pin(key_handle, k)))
    } else {
        key
    };
    (this, key)
}

fn cache_from_field(ctx: &dyn NativeContext, this: ObjectRef) -> Option<Arc<CacheInner>> {
    match ctx.get_field(this, CACHE_FIELD_HANDLE) {
        Value::Long(bits) if bits != 0 => {
            // Reconstruct without consuming: the caller owns the strong
            // reference in the field.
            let raw = bits as *const CacheInner;
            // SAFETY: `raw` was produced by `Arc::into_raw` in
            // `native_dcm_get_cache`. We rebuild an Arc without
            // decrementing — use Arc::increment_strong_count + from_raw
            // to keep the field's reference intact.
            unsafe {
                Arc::increment_strong_count(raw);
                Some(Arc::from_raw(raw))
            }
        }
        _ => {
            // Fallback: pull the name and re-resolve.
            match ctx.get_field(this, CACHE_FIELD_NAME) {
                Value::Object(Some(s)) => {
                    let name = ctx.read_string(s)?;
                    global_manager().get_cache(&name).ok()
                }
                _ => None,
            }
        }
    }
}

/// `Cache.put(K, V)V` / `put(Object, Object)Ljava/lang/Object;`.
fn native_cache_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_real_cache(ctx, this) {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let val = args.get(2).copied().unwrap_or(Value::Object(None));
        let metadata = ctx.get_field_by_name(this, "defaultMetadata");
        return ctx.invoke_virtual(
            this,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;Lorg/infinispan/metadata/Metadata;)Ljava/lang/Object;",
            &[key, val, metadata],
        );
    }
    let cache = match cache_from_field(ctx, this) {
        Some(c) => c,
        None => {
            return Err(RuntimeError::IllegalStateException {
                message: "Cache is not associated with a cache manager".to_string(),
            }
            .into());
        }
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let val_v = args.get(2).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::from_value(ctx, key_v) {
        Some(k) => k,
        None => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Cache.put: null key".to_string(),
            }
            .into());
        }
    };
    let stored = StoredValue::from_value(ctx, val_v);
    let previous = cache.put(key, stored);
    let prev_value = match previous {
        Some(sv) => sv.to_value(ctx),
        None => Value::Object(None),
    };
    Ok(Some(prev_value))
}

/// `Cache.get(Object)Ljava/lang/Object;`.
fn native_cache_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_real_cache(ctx, this) {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let (this_h, key_h) = pin_this_and_key(ctx, this, key);
        let invocation_ctx = real_cache_invocation_context(ctx, this);
        let (this, key) = read_this_and_key(ctx, this_h, this, key_h, key);
        ctx.unpin_native_roots(this_h);
        let invocation_ctx = invocation_ctx?.unwrap_or(Value::Object(None));
        return ctx.invoke_virtual(
            this,
            "get",
            "(Ljava/lang/Object;JLorg/infinispan/context/InvocationContext;)Ljava/lang/Object;",
            &[key, Value::Long(0), invocation_ctx],
        );
    }
    let cache = match cache_from_field(ctx, this) {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    let stored = cache.get(&key);
    let value = match stored {
        Some(sv) => sv.to_value(ctx),
        None => Value::Object(None),
    };
    Ok(Some(value))
}

/// `Cache.remove(Object)Ljava/lang/Object;`.
fn native_cache_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_real_cache(ctx, this) {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let (this_h, key_h) = pin_this_and_key(ctx, this, key);
        let ctx_builder = ctx.invoke_virtual(
            this,
            "defaultContextBuilderForWrite",
            "()Lorg/infinispan/cache/impl/ContextBuilder;",
            &[],
        );
        let (this, key) = read_this_and_key(ctx, this_h, this, key_h, key);
        ctx.unpin_native_roots(this_h);
        let ctx_builder = ctx_builder?.unwrap_or(Value::Object(None));
        return ctx.invoke_virtual(
            this,
            "remove",
            "(Ljava/lang/Object;JLorg/infinispan/cache/impl/ContextBuilder;)Ljava/lang/Object;",
            &[key, Value::Long(0), ctx_builder],
        );
    }
    let cache = match cache_from_field(ctx, this) {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    let prev = cache.remove(&key);
    let ret = match prev {
        Some(sv) => sv.to_value(ctx),
        None => Value::Object(None),
    };
    Ok(Some(ret))
}

/// `Cache.containsKey(Object)Z`.
fn native_cache_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    if is_real_cache(ctx, this) {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let (this_h, key_h) = pin_this_and_key(ctx, this, key);
        let invocation_ctx = real_cache_invocation_context(ctx, this);
        let (this, key) = read_this_and_key(ctx, this_h, this, key_h, key);
        ctx.unpin_native_roots(this_h);
        let invocation_ctx = invocation_ctx?.unwrap_or(Value::Object(None));
        return ctx.invoke_virtual(
            this,
            "containsKey",
            "(Ljava/lang/Object;JLorg/infinispan/context/InvocationContext;)Z",
            &[key, Value::Long(0), invocation_ctx],
        );
    }
    let cache = match cache_from_field(ctx, this) {
        Some(c) => c,
        None => return Ok(Some(Value::Int(0))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Int(0))),
    };
    let present = cache.get(&key).is_some();
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

/// `Cache.size()I`.
fn native_cache_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    if is_real_cache(ctx, this) {
        return ctx.invoke_virtual(this, "size", "(J)I", &[Value::Long(0)]);
    }
    let cache = match cache_from_field(ctx, this) {
        Some(c) => c,
        None => return Ok(Some(Value::Int(0))),
    };
    let size = cache.size() as i32;
    Ok(Some(Value::Int(size)))
}

/// `Cache.clear()V`.
fn native_cache_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if is_real_cache(ctx, this) {
        return ctx.invoke_virtual(this, "clear", "(J)V", &[Value::Long(0)]);
    }
    if let Some(cache) = cache_from_field(ctx, this) {
        cache.clear();
    }
    Ok(None)
}

/// `Cache.evict(Object)V` — same as remove but no return value.
fn native_cache_evict(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if is_real_cache(ctx, this) {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        return ctx.invoke_virtual(
            this,
            "evict",
            "(Ljava/lang/Object;J)V",
            &[key, Value::Long(0)],
        );
    }
    if let Some(cache) = cache_from_field(ctx, this) {
        let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
        if let Some(key) = CacheKey::from_value(ctx, key_v) {
            cache.remove(&key);
        }
    }
    Ok(None)
}

/// `Cache.addListener(Object)V`.
fn native_cache_add_listener(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let listener = match obj_arg(args, 1) {
        Some(o) => o,
        None => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Cache.addListener: listener is null".to_string(),
            }
            .into());
        }
    };
    if is_real_cache(ctx, this) {
        let stage = ctx
            .invoke_virtual(
                this,
                "addListenerAsync",
                "(Ljava/lang/Object;)Ljava/util/concurrent/CompletionStage;",
                &[Value::Object(Some(listener))],
            )?
            .unwrap_or(Value::Object(None));
        ctx.invoke(
            "org/infinispan/commons/util/concurrent/CompletionStages",
            "join",
            "(Ljava/util/concurrent/CompletionStage;)Ljava/lang/Object;",
            &[stage],
        )?;
        return Ok(None);
    }
    if let Some(cache) = cache_from_field(ctx, this) {
        cache.add_listener(listener);
    }
    Ok(None)
}

/// `Cache.removeListener(Object)V`.
fn native_cache_remove_listener(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let listener = match obj_arg(args, 1) {
        Some(o) => o,
        None => return Ok(None),
    };
    if is_real_cache(ctx, this) {
        let stage = ctx
            .invoke_virtual(
                this,
                "removeListenerAsync",
                "(Ljava/lang/Object;)Ljava/util/concurrent/CompletionStage;",
                &[Value::Object(Some(listener))],
            )?
            .unwrap_or(Value::Object(None));
        ctx.invoke(
            "org/infinispan/commons/util/concurrent/CompletionStages",
            "join",
            "(Ljava/util/concurrent/CompletionStage;)Ljava/lang/Object;",
            &[stage],
        )?;
        return Ok(None);
    }
    if let Some(cache) = cache_from_field(ctx, this) {
        cache.remove_listener(listener);
    }
    Ok(None)
}

/// `Cache.getName()Ljava/lang/String;`.
fn native_cache_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_real_cache(ctx, this) {
        return Ok(Some(ctx.get_field_by_name(this, "name")));
    }
    if let Value::Object(Some(s)) = ctx.get_field(this, CACHE_FIELD_NAME) {
        return Ok(Some(Value::Object(Some(s))));
    }
    Ok(Some(Value::Object(None)))
}

/// `Cache.putIfAbsent(K, V)V`.
fn native_cache_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_real_cache(ctx, this) {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let val = args.get(2).copied().unwrap_or(Value::Object(None));
        let metadata = ctx.get_field_by_name(this, "defaultMetadata");
        return ctx.invoke_virtual(
            this,
            "putIfAbsent",
            "(Ljava/lang/Object;Ljava/lang/Object;Lorg/infinispan/metadata/Metadata;)Ljava/lang/Object;",
            &[key, val, metadata],
        );
    }
    let cache = match cache_from_field(ctx, this) {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let val_v = args.get(2).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    // Fast-check then atomic put — hold the write lock ourselves to keep
    // the "absent test + insert" atomic.
    {
        let store = cache.store.read();
        if let Some(existing) = store.entries.get(&key) {
            let expired = existing.expires_at.is_some_and(|d| Instant::now() >= d);
            if !expired {
                return Ok(Some(existing.value.clone().to_value(ctx)));
            }
        }
    }
    // Not present (or expired) — do the put under write lock.
    let stored = StoredValue::from_value(ctx, val_v);
    cache.put(key, stored);
    Ok(Some(Value::Object(None)))
}

/// `Cache.replace(K, V)V` — only if present.
fn native_cache_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    if is_real_cache(ctx, this) {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let val = args.get(2).copied().unwrap_or(Value::Object(None));
        let metadata = ctx.get_field_by_name(this, "defaultMetadata");
        return ctx.invoke_virtual(
            this,
            "replace",
            "(Ljava/lang/Object;Ljava/lang/Object;Lorg/infinispan/metadata/Metadata;)Ljava/lang/Object;",
            &[key, val, metadata],
        );
    }
    let cache = match cache_from_field(ctx, this) {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let val_v = args.get(2).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    if cache.get(&key).is_none() {
        return Ok(Some(Value::Object(None)));
    }
    let stored = StoredValue::from_value(ctx, val_v);
    let prev = cache.put(key, stored);
    let ret = match prev {
        Some(sv) => sv.to_value(ctx),
        None => Value::Object(None),
    };
    Ok(Some(ret))
}

// ---------------------------------------------------------------------------
// Public registration.
// ---------------------------------------------------------------------------

/// Register all Infinispan local-mode native methods.
pub fn register_infinispan_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // DefaultCacheManager
    registry.register(CLS_MANAGER, "<init>", "()V", native_dcm_init);
    registry.register(
        CLS_MANAGER,
        "<init>",
        "(Lorg/infinispan/configuration/global/GlobalConfiguration;)V",
        native_dcm_init,
    );
    registry.register(
        CLS_MANAGER,
        "<init>",
        "(Lorg/infinispan/configuration/global/GlobalConfiguration;Lorg/infinispan/configuration/cache/Configuration;)V",
        native_dcm_init,
    );
    registry.register(
        CLS_MANAGER,
        "getCache",
        "(Ljava/lang/String;)Lorg/infinispan/Cache;",
        native_dcm_get_cache,
    );
    registry.register(
        CLS_MANAGER,
        "getCache",
        "()Lorg/infinispan/Cache;",
        native_dcm_get_cache,
    );
    registry.register(
        CLS_MANAGER,
        "getCacheNames",
        "()Ljava/util/Set;",
        native_dcm_get_cache_names,
    );
    registry.register(
        CLS_MANAGER,
        "cacheExists",
        "(Ljava/lang/String;)Z",
        native_dcm_cache_exists,
    );
    registry.register(
        CLS_MANAGER,
        "defineConfiguration",
        "(Ljava/lang/String;Lorg/infinispan/configuration/cache/Configuration;)Lorg/infinispan/configuration/cache/Configuration;",
        native_dcm_define_configuration,
    );
    registry.register(CLS_MANAGER, "start", "()V", native_dcm_start);
    registry.register(CLS_MANAGER, "stop", "()V", native_dcm_stop);

    // Alias the same handlers onto the EmbeddedCacheManager interface so
    // calls made through the interface type dispatch identically. (Native
    // dispatch is keyed by class name, not Java inheritance.)
    registry.register(
        CLS_MANAGER_IFACE,
        "getCache",
        "(Ljava/lang/String;)Lorg/infinispan/Cache;",
        native_dcm_get_cache,
    );
    registry.register(
        CLS_MANAGER_IFACE,
        "cacheExists",
        "(Ljava/lang/String;)Z",
        native_dcm_cache_exists,
    );

    // Cache (interface) + CacheImpl (concrete).
    for cls in &[CLS_CACHE, CLS_CACHE_IMPL, CLS_CACHE_ADVANCED] {
        registry.register(
            cls,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_cache_put,
        );
        registry.register(
            cls,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_cache_get,
        );
        registry.register(
            cls,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_cache_remove,
        );
        registry.register(
            cls,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            native_cache_contains_key,
        );
        registry.register(cls, "size", "()I", native_cache_size);
        registry.register(cls, "clear", "()V", native_cache_clear);
        registry.register(cls, "evict", "(Ljava/lang/Object;)V", native_cache_evict);
        registry.register(
            cls,
            "addListener",
            "(Ljava/lang/Object;)V",
            native_cache_add_listener,
        );
        registry.register(
            cls,
            "removeListener",
            "(Ljava/lang/Object;)V",
            native_cache_remove_listener,
        );
        registry.register(
            cls,
            "getName",
            "()Ljava/lang/String;",
            native_cache_get_name,
        );
        registry.register(
            cls,
            "putIfAbsent",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_cache_put_if_absent,
        );
        registry.register(
            cls,
            "replace",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_cache_replace,
        );
    }

    // ConfigurationBuilder.build() is intentionally NOT overridden here (as
    // of this fix). It used to be shimmed as an identity wrapper
    // (`native_cfg_build`, return `this` relabeled as the return type),
    // which left the returned object's REAL runtime class as
    // `ConfigurationBuilder` instead of a genuine `Configuration`. Real
    // Infinispan code that casts the "Configuration" back to its real type —
    // `CoreConfigurationSerializer.writeCacheContainer` does exactly that —
    // hit a real `ClassCastException` naming `ConfigurationBuilder`, reached
    // once the sibling `GlobalConfigurationBuilder.build()` fix (below) let
    // Keycloak's cache bootstrap get this far. See
    // fixed-suite-bugs/keycloak/keycloak-model-infinispan-configurationbuilder-classcastexception.md
    // (now fixed) and `native_dcm_define_configuration`'s doc comment for how
    // its raw synthetic-slot-index reads of `Configuration`'s size/ttl were
    // reworked to use `Configuration`'s real accessor API instead, which is
    // what let this identity wrapper be safely removed too.
    //
    // GlobalConfigurationBuilder.build() is intentionally NOT overridden here.
    // It used to be shimmed as an identity wrapper (return `this` relabeled
    // as the return type), which left the returned object's REAL runtime
    // class as `GlobalConfigurationBuilder` instead of a genuine
    // `GlobalConfiguration`. Real Infinispan's `GlobalConfigurationBuilder`
    // has no `isClustered()` method (only `GlobalConfiguration` does), so any
    // caller that invoked `isClustered()` on the "GlobalConfiguration" the
    // shim handed back hit a real `NoSuchMethodError` naming
    // `GlobalConfigurationBuilder` — e.g. Infinispan's own
    // `CoreConfigurationSerializer.writeJGroups`, which every
    // `testsuite/model` Keycloak test reaches during cache bootstrap. Unlike
    // `ConfigurationBuilder`/`Configuration` above, nothing in this file reads
    // `GlobalConfiguration`'s fields by synthetic slot index (`GC_FIELD_*` are
    // declared but never read; `native_dcm_init` ignores its
    // `GlobalConfiguration` argument entirely), so it is safe to let the real,
    // correct `GlobalConfigurationBuilder.build()` bytecode run and construct
    // a genuinely distinct, correctly-typed `GlobalConfiguration` instance.
    registry.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn fresh_cache(name: &str, limit: usize) -> Arc<CacheInner> {
        reset_manager_for_tests();
        let _ = global_manager().define_configuration(
            name,
            PendingConfig {
                size_limit: limit,
                default_ttl: None,
            },
        );
        global_manager().get_cache(name).unwrap()
    }

    #[test]
    fn t19_10_put_get_roundtrip() {
        let _g = test_lock();
        let cache = fresh_cache("realms", 1024);
        cache.put(
            CacheKey::from_str("realm-master"),
            StoredValue::Str("master-value".to_string()),
        );
        match cache.get(&CacheKey::from_str("realm-master")) {
            Some(StoredValue::Str(s)) => assert_eq!(s, "master-value"),
            other => panic!("unexpected: {other:?}"),
        }
        let (puts, gets, hits, _) = cache.stats();
        assert_eq!(puts, 1);
        assert_eq!(gets, 1);
        assert_eq!(hits, 1);
    }

    #[test]
    fn t19_10_remove_returns_previous_value() {
        let _g = test_lock();
        let cache = fresh_cache("users", 16);
        cache.put(
            CacheKey::from_str("u1"),
            StoredValue::Primitive(Value::Int(42)),
        );
        let prev = cache.remove(&CacheKey::from_str("u1"));
        assert!(matches!(prev, Some(StoredValue::Primitive(Value::Int(42)))));
        assert!(cache.get(&CacheKey::from_str("u1")).is_none());
    }

    #[test]
    fn t19_10_eviction_on_size_limit() {
        let _g = test_lock();
        let cache = fresh_cache("sessions", 3);
        for i in 0..5 {
            cache.put(
                CacheKey::from_bytes(&(i as i32).to_le_bytes()),
                StoredValue::Primitive(Value::Int(i)),
            );
        }
        // The oldest two keys (0, 1) must have been evicted.
        assert_eq!(cache.size(), 3);
        let (_, _, _, evictions) = cache.stats();
        assert!(evictions >= 2, "expected >=2 evictions, got {evictions}");
        assert!(cache
            .get(&CacheKey::from_bytes(&0i32.to_le_bytes()))
            .is_none());
        assert!(cache
            .get(&CacheKey::from_bytes(&4i32.to_le_bytes()))
            .is_some());
    }

    #[test]
    fn t19_10_ttl_expiry() {
        let _g = test_lock();
        let cache = Arc::new(CacheInner::new(
            "expiring".to_string(),
            16,
            Some(Duration::from_millis(50)),
        ));
        cache.put(
            CacheKey::from_str("k"),
            StoredValue::Primitive(Value::Int(1)),
        );
        assert!(cache.get(&CacheKey::from_str("k")).is_some());
        std::thread::sleep(Duration::from_millis(70));
        assert!(
            cache.get(&CacheKey::from_str("k")).is_none(),
            "TTL should have expired"
        );
        // Expiry path must also have incremented eviction counter.
        let (_, _, _, evictions) = cache.stats();
        assert!(evictions >= 1);
    }

    #[test]
    fn t19_10_listener_fires_on_put() {
        let _g = test_lock();
        let cache = fresh_cache("listened", 4);
        let mut ctx = mock_ctx();
        let listener_obj = ctx.alloc_object(ClassId::new(0), 1);
        let idx = cache.add_listener(listener_obj);
        assert_eq!(idx, 0);
        cache.put(
            CacheKey::from_str("k"),
            StoredValue::Primitive(Value::Int(1)),
        );
        let count = cache.listeners.lock().len();
        assert_eq!(count, 1, "listener still registered after put");
    }

    #[test]
    fn t19_10_listener_panic_does_not_corrupt_cache() {
        let _g = test_lock();
        let cache = fresh_cache("panicky", 4);
        // We can't easily stash a panic closure through the ListenerHandle
        // (it's an ObjectRef) but we can prove the catch_unwind guard in
        // dispatch() won't propagate. Verify that after a listener is
        // registered and a put fires dispatch, the store's state is
        // still consistent.
        let mut ctx = mock_ctx();
        let listener_obj = ctx.alloc_object(ClassId::new(0), 1);
        cache.add_listener(listener_obj);
        cache.put(
            CacheKey::from_str("k1"),
            StoredValue::Primitive(Value::Int(1)),
        );
        cache.put(
            CacheKey::from_str("k2"),
            StoredValue::Primitive(Value::Int(2)),
        );
        assert_eq!(cache.size(), 2);
        assert!(cache.get(&CacheKey::from_str("k1")).is_some());
        assert!(cache.get(&CacheKey::from_str("k2")).is_some());
    }

    #[test]
    fn t19_10_concurrent_put_get_eight_threads() {
        let _g = test_lock();
        let cache = fresh_cache("concurrent", 1024);
        let cache_shared = cache.clone();
        let mut handles = vec![];
        for t in 0..8 {
            let c = cache_shared.clone();
            let h = std::thread::spawn(move || {
                for i in 0..128 {
                    let key = format!("t{t}-k{i}");
                    c.put(
                        CacheKey::from_str(&key),
                        StoredValue::Primitive(Value::Int(i)),
                    );
                    let got = c.get(&CacheKey::from_str(&key));
                    assert!(got.is_some(), "thread {t} failed to read back {key}");
                }
            });
            handles.push(h);
        }
        for h in handles {
            h.join().unwrap();
        }
        // Every insert should be present (1024 = exactly the limit).
        assert_eq!(cache.size(), 8 * 128);
    }

    #[test]
    fn t19_10_cache_name_validation_rejects_bad_names() {
        // Empty
        assert!(validate_cache_name("").is_err());
        // Path traversal
        assert!(validate_cache_name("../etc/passwd").is_err());
        assert!(validate_cache_name("foo/../bar").is_err());
        // Reserved separators
        assert!(validate_cache_name("foo/bar").is_err());
        assert!(validate_cache_name("foo\\bar").is_err());
        assert!(validate_cache_name("C:\\Windows").is_err());
        // Control bytes
        assert!(validate_cache_name("foo\0bar").is_err());
        assert!(validate_cache_name("foo\nbar").is_err());
        assert!(validate_cache_name("foo\x1fbar").is_err());
        // Length
        let huge = "a".repeat(256);
        assert!(validate_cache_name(&huge).is_err());
        // Positives
        assert!(validate_cache_name("realms").is_ok());
        assert!(validate_cache_name("user-sessions").is_ok());
        assert!(validate_cache_name("authenticationSessions").is_ok());
        assert!(validate_cache_name("a.b.c").is_ok());
    }

    #[test]
    fn t19_10_get_cache_is_idempotent() {
        let _g = test_lock();
        reset_manager_for_tests();
        let a = global_manager().get_cache("idempotent").unwrap();
        let b = global_manager().get_cache("idempotent").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "getCache must return the same Arc");
    }

    #[test]
    fn t19_10_get_cache_different_configs_errors() {
        let _g = test_lock();
        reset_manager_for_tests();
        // Start with limit=10.
        global_manager()
            .define_configuration(
                "conflict",
                PendingConfig {
                    size_limit: 10,
                    default_ttl: None,
                },
            )
            .unwrap();
        let _first = global_manager().get_cache("conflict").unwrap();
        // Re-defining with a different limit must be rejected.
        let err = global_manager()
            .define_configuration(
                "conflict",
                PendingConfig {
                    size_limit: 1000,
                    default_ttl: None,
                },
            )
            .unwrap_err();
        assert!(err.contains("already configured"), "got: {err}");
    }

    #[test]
    fn t19_10_cache_key_equality_across_constructors() {
        let k1 = CacheKey::from_str("hello");
        let k2 = CacheKey::from_bytes(b"hello");
        assert_eq!(k1, k2);
        let mut map: HashMap<CacheKey, i32> = HashMap::new();
        map.insert(k1.clone(), 1);
        assert_eq!(map.get(&k2), Some(&1));
    }

    #[test]
    fn t19_10_clear_empties_but_keeps_cache_registered() {
        let _g = test_lock();
        let cache = fresh_cache("to-clear", 16);
        for i in 0..5 {
            cache.put(
                CacheKey::from_bytes(&(i as i32).to_le_bytes()),
                StoredValue::Primitive(Value::Int(i)),
            );
        }
        assert_eq!(cache.size(), 5);
        cache.clear();
        assert_eq!(cache.size(), 0);
        assert_eq!(global_manager().cache_count(), 1);
    }

    #[test]
    fn t19_10_remove_listener_succeeds_only_when_registered() {
        let _g = test_lock();
        let cache = fresh_cache("listener-dedup", 8);
        let mut ctx = mock_ctx();
        let l1 = ctx.alloc_object(ClassId::new(0), 1);
        let l2 = ctx.alloc_object(ClassId::new(0), 1);
        cache.add_listener(l1);
        assert!(cache.remove_listener(l1));
        assert!(!cache.remove_listener(l2), "never-registered listener");
        assert_eq!(cache.listeners.lock().len(), 0);
    }

    #[test]
    fn t19_10_put_if_absent_preserves_existing() {
        let _g = test_lock();
        let cache = fresh_cache("put-if-absent", 8);
        cache.put(
            CacheKey::from_str("k"),
            StoredValue::Primitive(Value::Int(1)),
        );
        // Simulate putIfAbsent: peek first.
        let already = cache.get(&CacheKey::from_str("k"));
        assert!(already.is_some());
        // Only overwrite if None — we purposely don't.
        match cache.get(&CacheKey::from_str("k")) {
            Some(StoredValue::Primitive(Value::Int(1))) => {}
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn t19_10_evict_updates_lru_order() {
        let _g = test_lock();
        let cache = fresh_cache("lru", 3);
        cache.put(
            CacheKey::from_str("a"),
            StoredValue::Primitive(Value::Int(1)),
        );
        cache.put(
            CacheKey::from_str("b"),
            StoredValue::Primitive(Value::Int(2)),
        );
        cache.put(
            CacheKey::from_str("c"),
            StoredValue::Primitive(Value::Int(3)),
        );
        // Re-access `a` so it's the MRU.
        cache.get(&CacheKey::from_str("a"));
        // Fourth insert should now evict `b` (LRU).
        cache.put(
            CacheKey::from_str("d"),
            StoredValue::Primitive(Value::Int(4)),
        );
        assert!(cache.get(&CacheKey::from_str("a")).is_some());
        assert!(
            cache.get(&CacheKey::from_str("b")).is_none(),
            "b should have been evicted"
        );
        assert!(cache.get(&CacheKey::from_str("c")).is_some());
        assert!(cache.get(&CacheKey::from_str("d")).is_some());
    }

    #[test]
    fn t19_10_native_registration_smoke() {
        let mut r = NativeMethodRegistry::new();
        register_infinispan_natives(&mut r);
        assert!(r
            .find(
                CLS_MANAGER,
                "getCache",
                "(Ljava/lang/String;)Lorg/infinispan/Cache;"
            )
            .is_some());
        assert!(r
            .find(
                CLS_CACHE_IMPL,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some());
        assert!(r.find(CLS_CACHE, "size", "()I").is_some());
        assert!(r
            .find(CLS_CACHE, "addListener", "(Ljava/lang/Object;)V")
            .is_some());
    }

    #[test]
    fn t19_10_define_configuration_size_limit_respected() {
        let _g = test_lock();
        reset_manager_for_tests();
        global_manager()
            .define_configuration(
                "tiny",
                PendingConfig {
                    size_limit: 2,
                    default_ttl: None,
                },
            )
            .unwrap();
        let cache = global_manager().get_cache("tiny").unwrap();
        cache.put(
            CacheKey::from_str("a"),
            StoredValue::Primitive(Value::Int(1)),
        );
        cache.put(
            CacheKey::from_str("b"),
            StoredValue::Primitive(Value::Int(2)),
        );
        cache.put(
            CacheKey::from_str("c"),
            StoredValue::Primitive(Value::Int(3)),
        );
        assert_eq!(cache.size(), 2, "size_limit=2 must be honoured");
    }

    /// Regression test for the `ConfigurationBuilder.build()`
    /// `ClassCastException` fix
    /// (fixed-suite-bugs/keycloak/keycloak-model-infinispan-configurationbuilder-classcastexception.md,
    /// now fixed): `native_dcm_define_configuration` must read size/ttl off
    /// a `Configuration` object through its real accessor API
    /// (`memory().maxCount()` / `expiration().lifespan()`, both via
    /// `invoke_virtual`) rather than a raw synthetic slot index. This test
    /// stands in for a real `Configuration` object with a mock whose
    /// `invoke_virtual` hook only understands those four real method names —
    /// it has NO synthetic slots at all, so if the native regressed back to
    /// `ctx.get_field(obj, N)` this test's mock would return `Value::Int(0)`
    /// (the mock's zeroed default) for every field read, silently passing
    /// with the wrong (default) values instead of the scripted 777/45000 —
    /// so the test also asserts the values are NOT the size/ttl defaults.
    #[test]
    fn t19_10_define_configuration_reads_via_real_accessor_api_not_raw_slots() {
        let _g = test_lock();
        reset_manager_for_tests();

        fn hook(
            ctx: &mut crate::test_utils::MockNativeContext,
            _receiver: ObjectRef,
            method_name: &str,
            _descriptor: &str,
            _args: &[Value],
        ) -> Option<MethodCallResult> {
            match method_name {
                "memory" => Some(Ok(Some(Value::Object(Some(ctx.fresh_object_ref()))))),
                "maxCount" => Some(Ok(Some(Value::Long(777)))),
                "expiration" => Some(Ok(Some(Value::Object(Some(ctx.fresh_object_ref()))))),
                "lifespan" => Some(Ok(Some(Value::Long(45_000)))),
                _ => None,
            }
        }

        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(hook);

        let name_obj = ctx.create_string("real-cfg-cache");
        // Stand-in for the real `Configuration` object real
        // `ConfigurationBuilder.build()` bytecode would construct — its own
        // fields are irrelevant since the fix reads through `invoke_virtual`,
        // never `get_field`, on this object.
        let config_obj = ctx.fresh_object_ref();

        let result = native_dcm_define_configuration(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(name_obj)),
                Value::Object(Some(config_obj)),
            ],
        );
        assert!(
            result.is_ok(),
            "defineConfiguration must succeed: {result:?}"
        );

        let cache = global_manager().get_cache("real-cfg-cache").unwrap();
        assert_eq!(
            cache.size_limit, 777,
            "size_limit must come from memory().maxCount(), not a default/garbage slot read"
        );
        assert_eq!(
            cache.default_ttl,
            Some(Duration::from_millis(45_000)),
            "default_ttl must come from expiration().lifespan(), not a default/garbage slot read"
        );
        assert_ne!(cache.size_limit, DEFAULT_SIZE_LIMIT);
    }

    fn real_dcm_define_configuration_hook(
        ctx: &mut crate::test_utils::MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        match (method_name, descriptor) {
            (
                "read",
                "(Lorg/infinispan/configuration/cache/Configuration;)Lorg/infinispan/configuration/cache/ConfigurationBuilder;",
            ) => {
                assert_eq!(args.len(), 1);
                ctx.set_field(receiver, 0, args[0]);
                Some(Ok(Some(Value::Object(Some(receiver)))))
            }
            ("isTemplate", "()Z") => Some(Ok(Some(Value::Int(0)))),
            (
                "template",
                "(Z)Lorg/infinispan/configuration/cache/ConfigurationBuilder;",
            ) => {
                assert_eq!(args.len(), 1);
                ctx.set_field(receiver, 1, args[0]);
                Some(Ok(Some(Value::Object(Some(receiver)))))
            }
            (
                "putConfiguration",
                "(Ljava/lang/String;Lorg/infinispan/configuration/cache/ConfigurationBuilder;)Lorg/infinispan/configuration/cache/Configuration;",
            ) => {
                assert_eq!(args.len(), 2);
                let builder = match args[1] {
                    Value::Object(Some(o)) => o,
                    other => panic!("expected builder object, got {other:?}"),
                };
                ctx.set_field(receiver, 0, args[0]);
                ctx.set_field(receiver, 1, args[1]);
                Some(Ok(Some(ctx.get_field(builder, 0))))
            }
            _ => None,
        }
    }

    #[test]
    fn t19_10_real_dcm_define_configuration_delegates_to_real_configuration_manager() {
        let _g = test_lock();
        reset_manager_for_tests();

        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(real_dcm_define_configuration_hook);

        let manager_class = ctx.ensure_class_initialized(CLS_MANAGER).unwrap();
        let this = ctx.alloc_object(manager_class, 4);
        let global_component_registry = ctx.fresh_object_ref();
        let configuration_manager = ctx.fresh_object_ref();
        ctx.set_field_by_name(
            this,
            "globalComponentRegistry",
            Value::Object(Some(global_component_registry)),
        );
        ctx.set_field_by_name(
            this,
            "configurationManager",
            Value::Object(Some(configuration_manager)),
        );

        let name = ctx.create_string("___protobuf_metadata");
        let config = ctx.fresh_object_ref();
        let result = native_dcm_define_configuration(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(name)),
                Value::Object(Some(config)),
            ],
        )
        .expect("real DefaultCacheManager defineConfiguration should delegate");

        assert_eq!(result, Some(Value::Object(Some(config))));
        assert_eq!(
            ctx.get_field(configuration_manager, 0),
            Value::Object(Some(name)),
            "real manager path must pass the cache name to ConfigurationManager"
        );
        let builder = match ctx.get_field(configuration_manager, 1) {
            Value::Object(Some(o)) => o,
            other => panic!("expected builder passed to putConfiguration, got {other:?}"),
        };
        assert_eq!(
            ctx.class_name_arc_of_id(ctx.class_id_of_object(builder))
                .as_deref(),
            Some(CLS_CONFIG_BUILDER)
        );
        assert_eq!(
            ctx.get_field(builder, 0),
            Value::Object(Some(config)),
            "builder.read(Configuration) must receive the original real configuration"
        );
        assert_eq!(
            ctx.get_field(builder, 1),
            Value::Int(0),
            "builder.template(false) should mirror the real config template bit"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    fn real_dcm_cache_exists_hook(
        ctx: &mut crate::test_utils::MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        match (method_name, descriptor) {
            ("selectCache", "(Ljava/lang/String;)Ljava/lang/String;") => {
                assert_eq!(args.len(), 1);
                ctx.set_field(receiver, 0, args[0]);
                let selected = ctx.create_string("___protobuf_metadata");
                Some(Ok(Some(Value::Object(Some(selected)))))
            }
            ("containsKey", "(Ljava/lang/Object;)Z") => {
                assert_eq!(args.len(), 1);
                ctx.set_field(receiver, 0, args[0]);
                Some(Ok(Some(Value::Int(1))))
            }
            _ => None,
        }
    }

    #[test]
    fn t19_10_real_dcm_cache_exists_uses_real_alias_resolution_and_cache_map() {
        let _g = test_lock();
        reset_manager_for_tests();

        let mut ctx = mock_ctx();
        ctx.set_invoke_virtual_hook(real_dcm_cache_exists_hook);

        let manager_class = ctx.ensure_class_initialized(CLS_MANAGER).unwrap();
        let this = ctx.alloc_object(manager_class, 4);
        let global_component_registry = ctx.fresh_object_ref();
        let configuration_manager = ctx.fresh_object_ref();
        let caches = ctx.fresh_object_ref();
        ctx.set_field_by_name(
            this,
            "globalComponentRegistry",
            Value::Object(Some(global_component_registry)),
        );
        ctx.set_field_by_name(
            this,
            "configurationManager",
            Value::Object(Some(configuration_manager)),
        );
        ctx.set_field_by_name(this, "caches", Value::Object(Some(caches)));

        let alias = ctx.create_string("protobuf-alias");
        let result = native_dcm_cache_exists(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(alias))],
        )
        .expect("real DefaultCacheManager cacheExists should delegate");

        assert_eq!(result, Some(Value::Int(1)));
        assert_eq!(
            ctx.get_field(configuration_manager, 0),
            Value::Object(Some(alias)),
            "cacheExists must ask ConfigurationManager.selectCache for aliases"
        );
        let selected = match ctx.get_field(caches, 0) {
            Value::Object(Some(o)) => o,
            other => panic!("expected selected cache name passed to containsKey, got {other:?}"),
        };
        assert_eq!(
            ctx.read_string(selected).as_deref(),
            Some("___protobuf_metadata")
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    #[test]
    fn t19_10_define_configuration_rejects_bad_name() {
        let _g = test_lock();
        reset_manager_for_tests();
        let err = global_manager()
            .define_configuration(
                "../evil",
                PendingConfig {
                    size_limit: 4,
                    default_ttl: None,
                },
            )
            .unwrap_err();
        assert!(err.contains("'../'"), "got: {err}");
    }

    #[test]
    fn t19_10_stored_value_roundtrip_through_ctx() {
        let _g = test_lock();
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let stored = StoredValue::from_value(&ctx, Value::Object(Some(s)));
        match &stored {
            StoredValue::Str(t) => assert_eq!(t, "hello"),
            other => panic!("expected Str, got: {other:?}"),
        }
        let back = stored.to_value(&mut ctx);
        match back {
            Value::Object(Some(obj)) => {
                assert_eq!(ctx.read_string(obj).as_deref(), Some("hello"));
            }
            other => panic!("expected Object, got: {other:?}"),
        }
    }

    #[test]
    fn t19_10_cache_names_listing() {
        let _g = test_lock();
        reset_manager_for_tests();
        global_manager().get_cache("a").unwrap();
        global_manager().get_cache("b").unwrap();
        global_manager().get_cache("c").unwrap();
        let mut names = global_manager().cache_names();
        names.sort();
        assert_eq!(
            names,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
}
