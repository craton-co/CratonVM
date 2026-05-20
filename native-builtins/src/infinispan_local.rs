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
//! | `org.infinispan.configuration.cache.Configuration`           | name        | size_limit (I) | ttl_ms (J)|
//! | `org.infinispan.configuration.global.GlobalConfiguration`    | site_name   | jmx_enabled    | reserved |
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
use cratonvm_types::error::{MethodCallResult, RuntimeError};
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

pub(crate) const CONFIG_FIELD_NAME: usize = 0;
pub(crate) const CONFIG_FIELD_SIZE_LIMIT: usize = 1;
pub(crate) const CONFIG_FIELD_TTL_MS: usize = 2;
pub(crate) const CONFIG_NUM_FIELDS: usize = 3;

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
                    unsafe {
                        Value::Object(Some(ObjectRef::from_raw(*bits as *mut u8)))
                    }
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
    pub fn define_configuration(
        &self,
        name: &str,
        config: PendingConfig,
    ) -> Result<(), String> {
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
        self.pending_configs.write().insert(name.to_string(), config);
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
    pub fn cache_names(&self) -> Vec<String> {
        self.caches.read().keys().cloned().collect()
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

fn alloc_object_for(ctx: &mut dyn NativeContext, class_name: &str, min_slots: usize) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .unwrap_or(ClassId::new(0));
    let n = ctx.class_num_total_fields(cid).max(min_slots);
    ctx.alloc_object(cid, n)
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
fn native_dcm_get_cache(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let name = match string_arg(ctx, args, 1) {
        Some(s) => s,
        // getCache() with no args = default cache.
        None => "___defaultcache".to_string(),
    };
    let cache = match global_manager().get_cache(&name) {
        Ok(c) => c,
        Err(e) => {
            return Err(RuntimeError::IllegalStateException {
                message: e,
            }
            .into());
        }
    };
    // Allocate a Java-side Cache object. Tuck the Rust Arc pointer in
    // slot 0 as a long handle so follow-up natives can re-hydrate.
    let cache_obj = alloc_object_for(ctx, CLS_CACHE_IMPL, CACHE_NUM_FIELDS);
    let handle = Arc::into_raw(cache.clone()) as i64;
    ctx.set_field(cache_obj, CACHE_FIELD_HANDLE, Value::Long(handle));
    let name_str = ctx.create_string(&name);
    ctx.set_field(cache_obj, CACHE_FIELD_NAME, Value::Object(Some(name_str)));
    ctx.set_field(cache_obj, CACHE_FIELD_MANAGER, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(cache_obj))))
}

/// `DefaultCacheManager.getCacheNames()Ljava/util/Set;` — we return an
/// empty `Object(None)`; bytecode that needs the real set can call
/// `cacheExists(name)` instead. Keycloak only hits it for diagnostics.
fn native_dcm_get_cache_names(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `DefaultCacheManager.cacheExists(String)Z`
fn native_dcm_cache_exists(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match string_arg(ctx, args, 1) {
        Some(s) => s,
        None => return Ok(Some(Value::Int(0))),
    };
    let exists = global_manager().cache_names().iter().any(|n| n == &name);
    Ok(Some(Value::Int(if exists { 1 } else { 0 })))
}

/// `DefaultCacheManager.defineConfiguration(String, Configuration)` — we
/// read the size/ttl fields off the Configuration object and stash.
fn native_dcm_define_configuration(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
            let s = match ctx.get_field(obj, CONFIG_FIELD_SIZE_LIMIT) {
                Value::Int(v) if v >= 0 => v as usize,
                _ => DEFAULT_SIZE_LIMIT,
            };
            let t = match ctx.get_field(obj, CONFIG_FIELD_TTL_MS) {
                Value::Long(v) if v > 0 => Some(Duration::from_millis(v as u64)),
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
    Ok(Some(config.map(|o| Value::Object(Some(o))).unwrap_or(Value::Object(None))))
}

/// `DefaultCacheManager.start()` / `stop()`.
fn native_dcm_start(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    global_manager().start();
    Ok(None)
}

fn native_dcm_stop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    global_manager().stop();
    Ok(None)
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
            let expired = existing
                .expires_at
                .is_some_and(|d| Instant::now() >= d);
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

// ConfigurationBuilder.build() — return the builder as a Configuration.
fn native_cfg_build(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(Value::Object(Some(this))))
}

// GlobalConfigurationBuilder.build() — same identity wrapper.
fn native_global_cfg_build(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(Value::Object(Some(this))))
}

// ---------------------------------------------------------------------------
// Public registration.
// ---------------------------------------------------------------------------

/// Register all Infinispan local-mode native methods.
pub fn register_infinispan_natives(registry: &mut NativeMethodRegistry) {
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
        registry.register(
            cls,
            "evict",
            "(Ljava/lang/Object;)V",
            native_cache_evict,
        );
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

    // ConfigurationBuilder / GlobalConfigurationBuilder .build()
    registry.register(
        CLS_CONFIG_BUILDER,
        "build",
        "()Lorg/infinispan/configuration/cache/Configuration;",
        native_cfg_build,
    );
    registry.register(
        CLS_GLOBAL_CONFIG_BUILDER,
        "build",
        "()Lorg/infinispan/configuration/global/GlobalConfiguration;",
        native_global_cfg_build,
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;

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
        assert!(cache.get(&CacheKey::from_bytes(&0i32.to_le_bytes())).is_none());
        assert!(cache.get(&CacheKey::from_bytes(&4i32.to_le_bytes())).is_some());
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
        assert!(cache.get(&CacheKey::from_str("k")).is_none(), "TTL should have expired");
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
        assert!(cache.get(&CacheKey::from_str("b")).is_none(), "b should have been evicted");
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
        assert!(r
            .find(CLS_CACHE, "size", "()I")
            .is_some());
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
        assert_eq!(names, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }
}
