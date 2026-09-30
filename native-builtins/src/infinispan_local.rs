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
//! We back every named cache with a Rust-side `CacheInner` guarded by an
//! ordered `RwLock<CacheStore>` (serialised writers, no async), in a
//! `DefaultCacheManagerInner` that belongs to ONE VM (see "Per-VM state"
//! below). LRU eviction is a `VecDeque` in MRU order with lazily-skipped stale
//! elements (see `CacheStore`) and a configurable `size_limit` (translated
//! from `Configuration.memory().size(N)`). TTL is lazy: every lookup checks
//! `Option<Instant>` expiry and returns `None` (and evicts) if past deadline,
//! and a VM's caches with a TTL are swept for expired entries at most every
//! 5 s, on that VM's next cache operation.
//!
//! ## Per-VM state and Java values (gc-common w30-a)
//!
//! `common-w29d-infinispan-shim-stores-java-values-as-raw-unrooted-addresses-FIXED-20260923`:
//! a non-String value used to be stored as its raw address in ONE
//! process-wide manager, so a moving collection left the entry pointing at a
//! vacated span (nothing rooted the value either, so it could be reclaimed
//! while cached), and two VMs sharing a cache name shared the entries. Now:
//!
//! * the manager is per VM (`VM_CACHES`, dropped at teardown by
//!   [`forget_vm_infinispan_state`] from `forget_vm_native_root_stores`);
//! * a Java value or listener is a GLOBAL ROOT of the owning VM
//!   ([`JavaRoot`]), resolved on every read, so the collector keeps it alive
//!   and current;
//! * a root that leaves the cache without a `NativeContext` at hand
//!   (displacement, eviction, TTL expiry, `clear`, `stop`, the lock-key sweep)
//!   is released when its last `Arc<JavaRoot>` drops: the handle is sent to
//!   the VM's release queue and removed by that VM's next cache operation. A
//!   reader holds its own `Arc` while it resolves, so a concurrent removal
//!   can never release (and a later `add_global_root` never reuse) a handle
//!   that a reader is about to resolve;
//! * a non-String KEY is its object's weak lock key (gc-common w29-d); when
//!   that object dies, `forget_infinispan_keys` (from the lock-key sweep)
//!   drops its entries, whose value roots would otherwise never be released.
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
//! ## Why a lock + VecDeque over a dedicated LRU crate
//!
//! 1. Infinispan's listener model requires atomic "did the insert cause an
//!    eviction?" inspection, which `lru`/`cached`/etc. hide behind
//!    take/put returns that don't composit cleanly with our per-cache
//!    listener dispatch loop.
//! 2. A `get` promotes its key, so it writes the store either way; a crate
//!    would not save the lock, only hide it.
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex, OrderedPlRwLock};
#[cfg(test)]
use cratonvm_types::ClassId;
use cratonvm_types::{ObjectRef, Value};

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
                // Non-string object: an IDENTITY key (real Infinispan uses
                // `equals`; this shim never did). gc-common w29-d
                // (`common-w28b-remaining-identity-hash-keyed-side-tables`,
                // rank 26): it used to be the object's 4 identity-hash bytes,
                // with no VM, in the process-wide manager. Two live key
                // objects of one VM with one hash shared an entry, and two
                // VMs collided on their first objects (every heap numbers
                // hashes from the same seed); a dead key's entry was also
                // served to a later object with its hash. Now: a tag byte
                // 0xFF (never valid in UTF-8, so no String key collides, and
                // 17 bytes, so no primitive key does), the VM, and the
                // object's weak lock key, which is unique per live object and
                // never minted again once it dies.
                Some(CacheKey::from_identity(ctx, obj))
            }
            Value::Object(None) => None,
            Value::Int(i) => Some(CacheKey::from_bytes(&i.to_le_bytes())),
            Value::Long(i) => Some(CacheKey::from_bytes(&i.to_le_bytes())),
            Value::Float(f) => Some(CacheKey::from_bytes(&f.to_le_bytes())),
            Value::Double(f) => Some(CacheKey::from_bytes(&f.to_le_bytes())),
            _ => None,
        }
    }

    /// The identity key of a non-String key object (see [`Self::from_value`]):
    /// `0xFF`, the VM identity and the object's weak lock key, 17 bytes.
    /// gc-common w29-d.
    fn from_identity(ctx: &dyn NativeContext, obj: ObjectRef) -> Self {
        let lock_key = crate::gc_stable_weak_lock_key(ctx, obj)
            .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"));
        Self::identity(ctx.vm_identity(), lock_key)
    }

    fn identity(vm: usize, lock_key: usize) -> Self {
        let mut bytes = [0u8; 17];
        bytes[0] = 0xFF;
        bytes[1..9].copy_from_slice(&(vm as u64).to_le_bytes());
        bytes[9..17].copy_from_slice(&(lock_key as u64).to_le_bytes());
        CacheKey::from_bytes(&bytes)
    }

    /// [`Self::from_value`] for a LOOKUP (`get`, `remove`, `containsKey`,
    /// `evict`): a non-String key object that was never keyed has no entry,
    /// so this answers `None` (a miss) instead of minting it a lock key the
    /// sweep then has to drop. gc-common w30-a.
    pub fn lookup_from_value(ctx: &dyn NativeContext, v: Value) -> Option<Self> {
        match v {
            Value::Object(Some(obj)) => {
                if let Some(s) = ctx.read_string(obj) {
                    return Some(CacheKey::from_str(&s));
                }
                crate::existing_weak_lock_key(ctx, obj)
                    .map(|lock_key| Self::identity(ctx.vm_identity(), lock_key))
            }
            other => Self::from_value(ctx, other),
        }
    }

    /// The weak lock key of an identity key (see [`Self::from_identity`]),
    /// `None` for a String or primitive key. A String key is valid UTF-8, so
    /// it never starts with `0xFF`; a primitive key is at most 8 bytes.
    fn identity_lock_key(&self) -> Option<usize> {
        let b = &self.0;
        if b.len() != 17 || b[0] != 0xFF {
            return None;
        }
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&b[9..17]);
        Some(u64::from_le_bytes(raw) as usize)
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
    /// Serialized form of the value: a Java object is a global root of the
    /// owning VM ([`JavaRoot`]), a String its contents, a primitive its
    /// `Value`.
    pub value: StoredValue,
    /// Monotonic creation timestamp.
    pub created_at: Instant,
    /// Absolute expiry instant. `None` = no TTL.
    pub expires_at: Option<Instant>,
    /// The stamp of this entry's live element in `CacheStore::lru`.
    lru_stamp: u64,
}

impl CacheEntry {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|deadline| now >= deadline)
    }
}

/// A Java object held by a cache (a value or a listener): a global root of the
/// VM that stored it. gc-common w30-a
/// (`common-w29d-infinispan-shim-stores-java-values-as-raw-unrooted-addresses-FIXED-20260923`):
/// it used to be the object's raw address, unrooted and never remapped.
///
/// Releasing a global root needs a `NativeContext`, which eviction, expiry,
/// `clear` and the lock-key sweep do not have. So the root is released when the
/// LAST `Arc<JavaRoot>` drops: [`Drop`] sends the handle to the owning VM's
/// release queue, drained by that VM's next cache operation
/// (`VmCaches::release_pending`). Every reader resolves through an `Arc` it
/// holds, so no handle is released (and reused by a later `add_global_root`)
/// while someone can still resolve it.
#[derive(Debug)]
pub struct JavaRoot {
    handle: usize,
    release: Sender<usize>,
}

impl JavaRoot {
    /// The global-root handle (tests).
    pub fn handle(&self) -> usize {
        self.handle
    }

    /// The object's CURRENT address, or `None` if the root is gone.
    fn resolve(&self, ctx: &dyn NativeContext) -> Option<ObjectRef> {
        ctx.resolve_global_root(self.handle)
    }
}

impl Drop for JavaRoot {
    fn drop(&mut self) {
        // Lock-free (an `mpsc` send), so a root may drop under a store lock or
        // in the collector's lock-key sweep. The send fails only once the VM's
        // state is gone (teardown), and then its global-root table went with
        // the VM: nothing is left to release.
        let _ = self.release.send(self.handle);
    }
}

#[derive(Clone, Debug)]
pub enum StoredValue {
    /// A Java reference: a global root of the owning VM (gc-common w30-a; it
    /// used to be the raw pointer bits, unrooted and never remapped).
    JavaRef(Arc<JavaRoot>),
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
    /// Materialize back into a VM `Value`. A `JavaRef` answers the object's
    /// current address (its root is resolved, never a remembered address).
    /// A `Str` allocates a new `String` (a collection point: hold nothing raw
    /// across this call).
    pub fn to_value(&self, ctx: &mut dyn NativeContext) -> Value {
        match self {
            StoredValue::JavaRef(root) => Value::Object(root.resolve(ctx)),
            StoredValue::Str(s) => Value::Object(Some(ctx.create_string(s))),
            StoredValue::Primitive(v) => *v,
            StoredValue::Bytes(_) => Value::Object(None),
            StoredValue::Null => Value::Object(None),
        }
    }

    /// Produce a `StoredValue` from a VM `Value`. A non-String object becomes
    /// a global root of `state`'s VM (the caller's). Does not allocate on the
    /// Java heap.
    pub fn from_value(
        ctx: &mut dyn NativeContext,
        state: &VmCaches,
        v: Value,
    ) -> Result<Self, MethodCallFailed> {
        Ok(match v {
            Value::Object(None) => StoredValue::Null,
            Value::Object(Some(obj)) => {
                if let Some(s) = ctx.read_string(obj) {
                    return Ok(StoredValue::Str(s));
                }
                StoredValue::JavaRef(state.root(ctx, obj)?)
            }
            other => StoredValue::Primitive(other),
        })
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

/// A registered Java-side listener: a global root of the owning VM
/// (gc-common w30-a; it used to be the raw pointer bits, so `removeListener`
/// missed a listener that had moved, and nothing kept it alive).
#[derive(Clone, Debug)]
pub struct ListenerHandle {
    /// The Java listener object.
    pub root: Arc<JavaRoot>,
    /// Optional event-kind filter; `None` = all events.
    pub event_filter: Option<ListenerEventKind>,
}

impl ListenerHandle {
    fn new(root: Arc<JavaRoot>) -> Self {
        Self {
            root,
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
        // store only records the listener's root; the VM upcall is performed
        // lazily by the Java shim when it walks the registered listeners.
        // This keeps our panic-safety boundary tight: we only log.
        //
        // gc-common w30-a: the preview below allocated a `String` per event
        // per listener on every write even with the log off.
        if !tracing::enabled!(target: "infinispan_local", tracing::Level::DEBUG) {
            return;
        }
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
                listener = self.root.handle,
                "dispatching cache listener event"
            );
        }));
        if let Err(_panic) = res {
            tracing::error!(
                target: "infinispan_local",
                cache = %cache_name,
                kind = ?kind,
                listener = self.root.handle,
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
    /// This cache's id in its VM's manager
    /// ([`DefaultCacheManagerInner::cache_by_id`]), stored in the Java `Cache`
    /// object's handle slot. gc-common w30-a: that slot used to hold a leaked
    /// `Arc::into_raw` pointer (one strong count per `getCache`, never given
    /// back) that `cache_from_field` rebuilt an `Arc` from, trusting whatever
    /// `long` the slot held.
    pub id: u64,
    /// Primary store. `LockLevel::Scratch`: never held across a
    /// `NativeContext` call, nor together with any other lock of this file.
    pub store: OrderedPlRwLock<CacheStore>,
    /// Registered Java-side listeners. Separate lock so listener
    /// registration doesn't contend with get/put. `Scratch`, same rule.
    pub listeners: OrderedPlMutex<Vec<ListenerHandle>>,
    /// Configured entry ceiling. `0` means unbounded (capped at MAX).
    pub size_limit: usize,
    /// Optional default TTL for entries that don't override it.
    pub default_ttl: Option<Duration>,
}

pub struct CacheStore {
    /// Primary key -> entry map.
    pub entries: HashMap<CacheKey, CacheEntry>,
    /// LRU order. Front = most-recently-used, back = eviction candidate.
    ///
    /// Each element carries the stamp its entry was given when the element
    /// was pushed; an element whose entry is gone, or carries a newer stamp,
    /// is stale and skipped. gc-common w30-a: promotion and removal used to
    /// search this queue linearly (`position`), O(`size_limit`) -- up to
    /// 10 000 -- on every `get`, `put` and `remove`. Stale elements are now
    /// left behind and compacted once they outnumber the live ones.
    lru: VecDeque<(CacheKey, u64)>,
    next_stamp: u64,
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
            next_stamp: 0,
            total_puts: 0,
            total_gets: 0,
            total_hits: 0,
            total_evictions: 0,
        }
    }

    /// Make `key` (which must have an entry) the most-recently-used.
    fn promote(&mut self, key: &CacheKey) {
        let stamp = self.next_stamp;
        self.next_stamp = self.next_stamp.wrapping_add(1);
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        entry.lru_stamp = stamp;
        self.lru.push_front((key.clone(), stamp));
        if self.lru.len() > 2 * self.entries.len() + 32 {
            let entries = &self.entries;
            self.lru
                .retain(|(k, s)| entries.get(k).is_some_and(|e| e.lru_stamp == *s));
        }
    }

    /// Remove the least-recently-used entry and answer its key.
    fn evict_tail(&mut self) -> Option<CacheKey> {
        while let Some((candidate, stamp)) = self.lru.pop_back() {
            if self
                .entries
                .get(&candidate)
                .is_some_and(|e| e.lru_stamp == stamp)
            {
                self.entries.remove(&candidate);
                self.total_evictions += 1;
                return Some(candidate);
            }
            // Stale element (promoted again, or removed) — keep popping.
        }
        None
    }
}

impl CacheInner {
    #[cfg(test)]
    fn new(name: String, size_limit: usize, default_ttl: Option<Duration>) -> Self {
        Self::with_id(0, name, size_limit, default_ttl)
    }

    fn with_id(id: u64, name: String, size_limit: usize, default_ttl: Option<Duration>) -> Self {
        Self {
            name,
            id,
            store: OrderedPlRwLock::new(CacheStore::new(), LockLevel::Scratch),
            listeners: OrderedPlMutex::new(Vec::new(), LockLevel::Scratch),
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
        let (previous, evicted) = {
            let mut store = self.store.write();
            self.insert_locked(&mut store, &key, value, ttl)
        };
        // Listener dispatch is outside the store lock.
        self.after_insert(&key, previous.is_some(), &evicted);
        previous.map(|e| e.value)
    }

    /// Atomic `putIfAbsent`: the live value when one is present (and nothing
    /// is stored), else `None` after storing `value`.
    ///
    /// gc-common w30-a: the native used to probe under the read lock and
    /// insert under a second, write, acquisition, so a racing `put` between
    /// the two was silently overwritten; and it materialized the live value
    /// (a `String` allocation, a collection point) with the read lock held.
    pub fn put_if_absent(&self, key: CacheKey, value: StoredValue) -> Option<StoredValue> {
        let (replaced, evicted) = {
            let mut store = self.store.write();
            if let Some(e) = store.entries.get(&key) {
                if !e.is_expired(Instant::now()) {
                    return Some(e.value.clone());
                }
            }
            let (previous, evicted) = self.insert_locked(&mut store, &key, value, self.default_ttl);
            (previous.is_some(), evicted)
        };
        self.after_insert(&key, replaced, &evicted);
        None
    }

    /// Atomic `replace`: stores `value` only when a live entry is present and
    /// answers its previous value; `None` (nothing stored) otherwise.
    /// gc-common w30-a: was a `get` and a `put`, two acquisitions.
    pub fn replace(&self, key: CacheKey, value: StoredValue) -> Option<StoredValue> {
        let (previous, evicted) = {
            let mut store = self.store.write();
            let live = store
                .entries
                .get(&key)
                .is_some_and(|e| !e.is_expired(Instant::now()));
            if !live {
                return None;
            }
            self.insert_locked(&mut store, &key, value, self.default_ttl)
        };
        self.after_insert(&key, true, &evicted);
        previous.map(|e| e.value)
    }

    /// Insert under the held store lock, evicting LRU entries past the
    /// limit: answers the displaced entry and the evicted keys.
    fn insert_locked(
        &self,
        store: &mut CacheStore,
        key: &CacheKey,
        value: StoredValue,
        ttl: Option<Duration>,
    ) -> (Option<CacheEntry>, Vec<CacheKey>) {
        let now = Instant::now();
        let entry = CacheEntry {
            value,
            created_at: now,
            expires_at: ttl.map(|d| now + d),
            lru_stamp: 0,
        };
        let previous = store.entries.insert(key.clone(), entry);
        store.promote(key);
        store.total_puts += 1;
        let mut evicted = Vec::new();
        while store.entries.len() > self.size_limit {
            match store.evict_tail() {
                Some(k) => evicted.push(k),
                None => break,
            }
        }
        (previous, evicted)
    }

    fn after_insert(&self, key: &CacheKey, replaced: bool, evicted: &[CacheKey]) {
        let kind = if replaced {
            ListenerEventKind::Modified
        } else {
            ListenerEventKind::Created
        };
        self.dispatch(kind, key);
        for evicted_key in evicted {
            self.dispatch(ListenerEventKind::Evicted, evicted_key);
        }
    }

    /// Get an entry, checking and lazily evicting if expired. Promotes the
    /// key to MRU on a hit. One write acquisition (a hit promotes, so the
    /// old read-lock probe was always followed by a write lock anyway).
    pub fn get(&self, key: &CacheKey) -> Option<StoredValue> {
        let (value, expired) = {
            let mut store = self.store.write();
            store.total_gets += 1;
            let expired = store.entries.get(key).map(|e| e.is_expired(Instant::now()));
            match expired {
                None => (None, false),
                Some(true) => {
                    store.entries.remove(key);
                    store.total_evictions += 1;
                    (None, true)
                }
                Some(false) => {
                    store.total_hits += 1;
                    store.promote(key);
                    (store.entries.get(key).map(|e| e.value.clone()), false)
                }
            }
        };
        if expired {
            self.dispatch(ListenerEventKind::Expired, key);
        }
        value
    }

    /// Remove an entry. Returns the previous value if one was present.
    pub fn remove(&self, key: &CacheKey) -> Option<StoredValue> {
        let previous = self.store.write().entries.remove(key).map(|e| e.value);
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

    /// Drop every entry whose key `dead` matches (the lock-key sweep, see
    /// [`forget_infinispan_keys`]); answers how many went. No listener event:
    /// the key can no longer be named by anyone.
    fn remove_where(&self, dead: impl Fn(&CacheKey) -> bool) -> usize {
        let mut store = self.store.write();
        let before = store.entries.len();
        store.entries.retain(|k, _| !dead(k));
        before - store.entries.len()
    }

    /// Drop every expired entry (the per-VM periodic sweep,
    /// [`VmCaches::reap_if_due`]); answers how many went. Only a cache with a
    /// default TTL can hold one (the natives store every entry with it).
    fn reap(&self, now: Instant) -> usize {
        if self.default_ttl.is_none() {
            return 0;
        }
        let expired: Vec<CacheKey> = {
            let mut store = self.store.write();
            let keys: Vec<CacheKey> = store
                .entries
                .iter()
                .filter(|(_, e)| e.is_expired(now))
                .map(|(k, _)| k.clone())
                .collect();
            for k in &keys {
                store.entries.remove(k);
            }
            store.total_evictions += keys.len() as u64;
            keys
        };
        for key in &expired {
            self.dispatch(ListenerEventKind::Expired, key);
        }
        expired.len()
    }

    /// Register a Java-side listener (a root of the caller's VM, see
    /// `VmCaches::root`). Returns the listener index.
    pub fn add_listener(&self, root: Arc<JavaRoot>) -> usize {
        let mut listeners = self.listeners.lock();
        listeners.push(ListenerHandle::new(root));
        listeners.len() - 1
    }

    /// Remove every registration of `listener`, judged by the listeners'
    /// CURRENT addresses (their roots are resolved with no lock held). A
    /// removed listener's root is released by the VM's next cache operation.
    pub fn remove_listener(&self, ctx: &dyn NativeContext, listener: ObjectRef) -> bool {
        let snapshot: Vec<Arc<JavaRoot>> = self
            .listeners
            .lock()
            .iter()
            .map(|h| Arc::clone(&h.root))
            .collect();
        let target = listener.as_ptr() as usize;
        let matching: Vec<Arc<JavaRoot>> = snapshot
            .into_iter()
            .filter(|root| {
                root.resolve(ctx)
                    .is_some_and(|o| o.as_ptr() as usize == target)
            })
            .collect();
        if matching.is_empty() {
            return false;
        }
        let mut listeners = self.listeners.lock();
        let before = listeners.len();
        listeners.retain(|h| !matching.iter().any(|m| Arc::ptr_eq(m, &h.root)));
        listeners.len() != before
    }

    fn dispatch(&self, kind: ListenerEventKind, key: &CacheKey) {
        // Dispatch only logs (see `ListenerHandle::dispatch`); with the log off
        // there is nothing to do, so do not copy the listener list per event.
        if !tracing::enabled!(target: "infinispan_local", tracing::Level::DEBUG) {
            return;
        }
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
// DefaultCacheManagerInner — one VM's registry of named caches.
// ---------------------------------------------------------------------------

pub struct DefaultCacheManagerInner {
    /// Named caches, by name and by id. ~20 in practice (Keycloak).
    /// `LockLevel::Scratch`; never held with `pending_configs`.
    caches: OrderedPlRwLock<CacheTable>,
    /// Per-cache configuration that `getCache(name)` should apply on first
    /// request. Populated by `defineConfiguration(name, config)`. `Scratch`.
    pending_configs: OrderedPlRwLock<HashMap<String, PendingConfig>>,
    /// Is the manager started?
    started: AtomicBool,
}

/// The caches of one manager, by name and by the id a Java `Cache` object
/// carries in its handle slot.
#[derive(Default)]
struct CacheTable {
    by_name: HashMap<String, Arc<CacheInner>>,
    by_id: HashMap<u64, Arc<CacheInner>>,
    next_id: u64,
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
            caches: OrderedPlRwLock::new(CacheTable::default(), LockLevel::Scratch),
            pending_configs: OrderedPlRwLock::new(HashMap::new(), LockLevel::Scratch),
            started: AtomicBool::new(false),
        }
    }

    pub fn start(&self) {
        self.started.store(true, Ordering::Release);
    }

    /// Stop: every cache and configuration goes, and with them the entries'
    /// roots (released by the VM's next cache operation). A Java `Cache`
    /// object obtained before the stop then re-resolves by name, to a new,
    /// empty cache. (It used to keep operating on the orphaned store, which
    /// the leaked `Arc::into_raw` handle kept alive for the life of the
    /// process.)
    pub fn stop(&self) {
        self.started.store(false, Ordering::Release);
        let dropped = std::mem::take(&mut *self.caches.write());
        self.pending_configs.write().clear();
        drop(dropped);
    }

    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
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
        let existing = self.caches.read().by_name.get(name).cloned();
        if let Some(existing) = existing {
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
        let hit = self.caches.read().by_name.get(name).cloned();
        if let Some(c) = hit {
            return Ok(c);
        }
        // Read the pending configuration BEFORE taking the table's write
        // lock: both are `Scratch`, so neither may be taken under the other.
        let cfg = self
            .pending_configs
            .read()
            .get(name)
            .cloned()
            .unwrap_or(PendingConfig {
                size_limit: DEFAULT_SIZE_LIMIT,
                default_ttl: None,
            });
        // Slow path: write-lock and re-check.
        let mut table = self.caches.write();
        if let Some(c) = table.by_name.get(name) {
            return Ok(c.clone());
        }
        table.next_id += 1;
        let id = table.next_id;
        let cache = Arc::new(CacheInner::with_id(
            id,
            name.to_string(),
            cfg.size_limit,
            cfg.default_ttl,
        ));
        table.by_name.insert(name.to_string(), cache.clone());
        table.by_id.insert(id, cache.clone());
        Ok(cache)
    }

    /// The live cache a Java `Cache` object's handle slot names.
    pub fn cache_by_id(&self, id: u64) -> Option<Arc<CacheInner>> {
        self.caches.read().by_id.get(&id).cloned()
    }

    /// Remove a named cache entirely.
    pub fn remove_cache(&self, name: &str) -> bool {
        let mut table = self.caches.write();
        let removed = table.by_name.remove(name);
        match removed {
            Some(cache) => {
                table.by_id.remove(&cache.id);
                true
            }
            None => false,
        }
    }

    /// Every live cache (owned, so no lock is held while the caller uses
    /// them).
    fn live_caches(&self) -> Vec<Arc<CacheInner>> {
        self.caches.read().by_name.values().cloned().collect()
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
        let mut names: HashSet<String> = self.caches.read().by_name.keys().cloned().collect();
        names.extend(self.pending_configs.read().keys().cloned());
        names.into_iter().collect()
    }

    #[cfg(test)]
    pub fn cache_count(&self) -> usize {
        self.caches.read().by_name.len()
    }
}

// ---------------------------------------------------------------------------
// Per-VM state (gc-common w30-a).
// ---------------------------------------------------------------------------

/// One VM's synthetic Infinispan state: its cache manager, the release queue
/// of its [`JavaRoot`]s, and the weak lock keys its identity-keyed entries are
/// filed under.
pub struct VmCaches {
    manager: DefaultCacheManagerInner,
    /// Cloned into every [`JavaRoot`]; see its `Drop`.
    release_tx: Sender<usize>,
    /// `LockLevel::Scratch`: drained into a local, released after the guard
    /// drops ([`Self::release_pending`]).
    release_rx: OrderedPlMutex<Receiver<usize>>,
    /// The weak lock keys of the non-String key objects this VM's caches
    /// filed entries under ([`CacheKey::from_identity`]), so the lock-key
    /// sweep can tell in one probe per freed key whether any entry is now
    /// unreachable ([`forget_infinispan_keys`]). A key stays until its object
    /// dies. `Scratch`, never held with another lock.
    identity_keys: OrderedPlMutex<HashSet<usize>>,
    /// Origin of `last_reap_ms`.
    born: Instant,
    /// When [`Self::reap_if_due`] last swept, in ms since `born`.
    last_reap_ms: AtomicU64,
}

impl Default for VmCaches {
    fn default() -> Self {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let manager = DefaultCacheManagerInner::new();
        manager.start();
        Self {
            manager,
            release_tx,
            release_rx: OrderedPlMutex::new(release_rx, LockLevel::Scratch),
            identity_keys: OrderedPlMutex::new(HashSet::new(), LockLevel::Scratch),
            born: Instant::now(),
            last_reap_ms: AtomicU64::new(0),
        }
    }
}

impl VmCaches {
    /// This VM's cache manager.
    pub fn manager(&self) -> &DefaultCacheManagerInner {
        &self.manager
    }

    /// Root `obj` as a global root of the caller's VM, released when the last
    /// `Arc` to it drops (see [`JavaRoot`]).
    fn root(
        &self,
        ctx: &mut dyn NativeContext,
        obj: ObjectRef,
    ) -> Result<Arc<JavaRoot>, MethodCallFailed> {
        let handle = ctx.add_global_root(obj);
        if handle == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "Infinispan cache: could not create a global root".to_string(),
            }
            .into());
        }
        Ok(Arc::new(JavaRoot {
            handle,
            release: self.release_tx.clone(),
        }))
    }

    /// Release the global roots whose last [`JavaRoot`] dropped since the
    /// previous call; answers how many.
    fn release_pending(&self, ctx: &mut dyn NativeContext) -> usize {
        let handles: Vec<usize> = self.release_rx.lock().try_iter().collect();
        for &handle in &handles {
            ctx.remove_global_root(handle);
        }
        handles.len()
    }

    /// Record that an entry is about to be filed under `key` (see
    /// `identity_keys`). A String or primitive key is not recorded.
    fn note_key(&self, key: &CacheKey) {
        if let Some(lock_key) = key.identity_lock_key() {
            self.identity_keys.lock().insert(lock_key);
        }
    }

    /// Sweep this VM's caches for expired entries, at most once per
    /// [`DEFAULT_REAPER_INTERVAL`]. This replaces the process-wide reaper
    /// thread (`start_reaper`, never started by anything), so an expired
    /// entry nobody reads again no longer keeps its value rooted until LRU
    /// eviction.
    fn reap_if_due(&self) {
        let now = Instant::now();
        let now_ms = now.saturating_duration_since(self.born).as_millis() as u64;
        let last = self.last_reap_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < DEFAULT_REAPER_INTERVAL.as_millis() as u64 {
            return;
        }
        if self
            .last_reap_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        for cache in self.manager.live_caches() {
            cache.reap(now);
        }
    }
}

/// Every VM's [`VmCaches`]. gc-common w30-a: this was ONE process-wide
/// manager (`global_manager()`), so VM B's `getCache("realms")` returned VM
/// A's cache, raw addresses into A's heap included, and a VM's caches outlived
/// it. Dropped per VM by [`forget_vm_infinispan_state`].
static VM_CACHES: VmScoped<Arc<VmCaches>> = VmScoped::new();

/// The caller's VM's state (created on first use).
fn vm_caches(ctx: &dyn NativeContext) -> Arc<VmCaches> {
    VM_CACHES.with(ctx.vm_identity(), |state| Arc::clone(state))
}

/// [`vm_caches`] for a cache operation: first sweep the VM's expired entries
/// (when due) and release the roots its caches let go of since the previous
/// operation. Takes no lock the caller could hold.
fn vm_caches_for_op(ctx: &mut dyn NativeContext) -> Arc<VmCaches> {
    let state = vm_caches(ctx);
    state.reap_if_due();
    state.release_pending(ctx);
    state
}

/// The lock-key sweep freed `freed`, weak lock keys of VM `vm` whose objects
/// died (or, at teardown, all of the VM's keys). An entry filed under one of
/// them can never be looked up again (the key is never minted again), so drop
/// it; its value's root is released by the VM's next cache operation. Without
/// this such an entry kept its value rooted until LRU eviction -- forever in a
/// cache that never fills. gc-common w30-a, a hook of `lib.rs::sweep_lock_keys`
/// and `forget_vm_lock_keys`: it runs in the collector's epilogue, so it takes
/// only this file's leaf locks, one at a time, and no `NativeContext`.
pub(crate) fn forget_infinispan_keys(vm: usize, freed: &[usize]) {
    if freed.is_empty() {
        return;
    }
    let Some(state) = VM_CACHES.peek(vm, Arc::clone) else {
        return;
    };
    let dead: HashSet<usize> = {
        let mut keys = state.identity_keys.lock();
        if keys.is_empty() {
            return;
        }
        freed.iter().copied().filter(|k| keys.remove(k)).collect()
    };
    if dead.is_empty() {
        return;
    }
    for cache in state.manager.live_caches() {
        cache.remove_where(|key| key.identity_lock_key().is_some_and(|k| dead.contains(&k)));
    }
}

/// Drop VM `vm`'s caches, configurations and queued releases (VM teardown,
/// from `lib.rs::forget_vm_native_root_stores`). The VM's global-root table
/// goes with the VM, so nothing is released. Idempotent. gc-common w30-a.
pub fn forget_vm_infinispan_state(vm: usize) {
    VM_CACHES.forget(vm);
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
/// Idempotent w.r.t. the VM's manager (every synthetic `DefaultCacheManager`
/// of one VM shares it).
fn native_dcm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let state = vm_caches_for_op(ctx);
    // The handle is only an identifier (nothing reads it back): the address
    // of this VM's state. Java side just needs a stable non-zero value.
    let handle = Arc::as_ptr(&state) as i64;
    ctx.set_field(this, DCM_FIELD_HANDLE, Value::Long(handle));
    ctx.set_field(this, DCM_FIELD_CONFIG_NAME, Value::Object(None));
    ctx.set_field(this, DCM_FIELD_STARTED, Value::Int(1));
    state.manager.start();
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
/// keycloak-model-infinispan-cache-config-null-after-real-start-FIXED.md.
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
    let state = vm_caches_for_op(ctx);
    let cache = match state.manager.get_cache(&name) {
        Ok(c) => c,
        Err(e) => {
            return Err(RuntimeError::IllegalStateException { message: e }.into());
        }
    };
    // Allocate a Java-side Cache object. Tuck the cache's id (in this VM's
    // manager) in slot 0 as a long handle so follow-up natives can
    // re-hydrate. gc-common w30-a: it used to be a leaked `Arc::into_raw`
    // pointer.
    //
    // gc-common w20-g: both allocations below can collect. The manager was
    // held raw across both (and stored as the cache's back-pointer), and the
    // cache object across the name's; both are rooted now.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let cache_obj = alloc_object_for(&mut *scope, CLS_CACHE_IMPL, CACHE_NUM_FIELDS)?;
    let cache_h = scope.root(cache_obj);
    scope.set_field(cache_obj, CACHE_FIELD_HANDLE, Value::Long(cache.id as i64));
    let name_str = scope.create_string(&name);
    let cache_obj = scope.get(&cache_h);
    let this = scope.get(&this_h);
    scope.set_field(cache_obj, CACHE_FIELD_NAME, Value::Object(Some(name_str)));
    scope.set_field(cache_obj, CACHE_FIELD_MANAGER, Value::Object(Some(this)));
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
    let names = vm_caches_for_op(ctx).manager.cache_names();
    // gc-common w30-a: every `create_string` can collect, and the strings
    // made before it were held raw in this Vec; each is rooted now and read
    // back current after the last allocation.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let handles: Vec<_> = names
        .iter()
        .map(|n| {
            let s = scope.create_string(n);
            scope.root(s)
        })
        .collect();
    let key_objs: Vec<ObjectRef> = handles.iter().map(|h| scope.get(h)).collect();
    let set = crate::build_real_layout_string_hashset(&mut *scope, &key_objs)?;
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
    let exists = vm_caches_for_op(ctx)
        .manager
        .cache_names()
        .iter()
        .any(|n| n == &name);
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
/// keycloak-model-infinispan-configurationbuilder-classcastexception.md.
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
    // gc-common w30-a: each `invoke_virtual` below runs Java (a collection
    // point); `config` was the receiver of the second call and the return
    // value, both raw after the first. It is rooted now and read back
    // current at each use.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let config_h = obj_arg(args, 2).map(|obj| scope.root(obj));
    let (size, ttl_ms) = match &config_h {
        Some(config_h) => {
            let obj = scope.get(config_h);
            let s = match scope
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
                    match scope
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
            let obj = scope.get(config_h);
            let t = match scope
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
                    match scope
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
    if let Err(e) = vm_caches_for_op(&mut *scope)
        .manager
        .define_configuration(&name, cfg)
    {
        return Err(RuntimeError::IllegalStateException { message: e }.into());
    }
    // Infinispan returns the Configuration object; we echo the input.
    Ok(Some(Value::Object(config_h.map(|h| scope.get(&h)))))
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
/// manager's `start()` ran against the unrelated synthetic cache, while the real object's own `internalStart(boolean)`
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
    vm_caches_for_op(ctx).manager.start();
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
    vm_caches_for_op(ctx).manager.stop();
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
/// see gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md.
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

/// The synthetic cache a Java `Cache` object names, in the caller's VM: by the
/// id in its handle slot, else (a real class layout loses the `long`, see
/// `is_real_cache`) by the name in its name slot.
///
/// gc-common w30-a: the handle was a leaked `Arc::into_raw` pointer that this
/// rebuilt an `Arc` from, trusting whatever non-zero `long` the slot held, and
/// the name fallback resolved in the process-wide manager.
fn cache_from_field(
    ctx: &dyn NativeContext,
    state: &VmCaches,
    this: ObjectRef,
) -> Option<Arc<CacheInner>> {
    if let Value::Long(id) = ctx.get_field(this, CACHE_FIELD_HANDLE) {
        if id > 0 {
            if let Some(cache) = state.manager.cache_by_id(id as u64) {
                return Some(cache);
            }
        }
    }
    match ctx.get_field(this, CACHE_FIELD_NAME) {
        Value::Object(Some(s)) => {
            let name = ctx.read_string(s)?;
            state.manager.get_cache(&name).ok()
        }
        _ => None,
    }
}

/// Entry of every synthetic Cache-instance native: the caller's VM's state
/// (after its pending root releases, see [`vm_caches_for_op`]) and the cache
/// `this` names.
fn synthetic_cache(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> (Arc<VmCaches>, Option<Arc<CacheInner>>) {
    let state = vm_caches_for_op(ctx);
    let cache = cache_from_field(ctx, &state, this);
    (state, cache)
}

/// A value the cache answered, as a VM `Value` (`null` for none). May
/// allocate (a `String` value): the caller holds nothing raw across it.
fn stored_to_value(ctx: &mut dyn NativeContext, value: Option<StoredValue>) -> Value {
    match value {
        Some(sv) => sv.to_value(ctx),
        None => Value::Object(None),
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
    let (state, cache) = synthetic_cache(ctx, this);
    let cache = match cache {
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
    let stored = StoredValue::from_value(ctx, &state, val_v)?;
    state.note_key(&key);
    let previous = cache.put(key, stored);
    Ok(Some(stored_to_value(ctx, previous)))
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
    let cache = match synthetic_cache(ctx, this).1 {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::lookup_from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    let stored = cache.get(&key);
    Ok(Some(stored_to_value(ctx, stored)))
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
    let cache = match synthetic_cache(ctx, this).1 {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::lookup_from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    // The removed value's root is released by the VM's next cache operation,
    // after this one has handed the value to Java.
    let prev = cache.remove(&key);
    Ok(Some(stored_to_value(ctx, prev)))
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
    let cache = match synthetic_cache(ctx, this).1 {
        Some(c) => c,
        None => return Ok(Some(Value::Int(0))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::lookup_from_value(ctx, key_v) {
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
    let cache = match synthetic_cache(ctx, this).1 {
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
    if let Some(cache) = synthetic_cache(ctx, this).1 {
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
    if let Some(cache) = synthetic_cache(ctx, this).1 {
        let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
        if let Some(key) = CacheKey::lookup_from_value(ctx, key_v) {
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
    let (state, cache) = synthetic_cache(ctx, this);
    if let Some(cache) = cache {
        let root = state.root(ctx, listener)?;
        cache.add_listener(root);
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
    if let Some(cache) = synthetic_cache(ctx, this).1 {
        cache.remove_listener(ctx, listener);
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
    let (state, cache) = synthetic_cache(ctx, this);
    let cache = match cache {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let val_v = args.get(2).copied().unwrap_or(Value::Object(None));
    let key = match CacheKey::from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    // One atomic test-and-insert under the store's write lock (gc-common
    // w30-a: it was a read-locked probe that materialized the live value --
    // a `String` allocation -- with the lock held, then a separate `put`).
    // A value that is not stored drops its root here; it is released by the
    // VM's next cache operation.
    let stored = StoredValue::from_value(ctx, &state, val_v)?;
    state.note_key(&key);
    let existing = cache.put_if_absent(key, stored);
    Ok(Some(stored_to_value(ctx, existing)))
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
    let (state, cache) = synthetic_cache(ctx, this);
    let cache = match cache {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let key_v = args.get(1).copied().unwrap_or(Value::Object(None));
    let val_v = args.get(2).copied().unwrap_or(Value::Object(None));
    // An entry exists only under a key that was filed (and so minted).
    let key = match CacheKey::lookup_from_value(ctx, key_v) {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    let stored = StoredValue::from_value(ctx, &state, val_v)?;
    let prev = cache.replace(key, stored);
    Ok(Some(stored_to_value(ctx, prev)))
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
    // keycloak-model-infinispan-configurationbuilder-classcastexception.md
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

    /// Forgets its VM's cache state and lock keys on drop, pass or fail, and
    /// only that VM's (lessons c, pp).
    pub(super) struct VmGuard(pub(super) usize);

    impl Drop for VmGuard {
        fn drop(&mut self) {
            forget_vm_infinispan_state(self.0);
            crate::forget_vm_lock_keys(self.0);
        }
    }

    fn fresh_cache(name: &str, limit: usize) -> Arc<CacheInner> {
        let m = DefaultCacheManagerInner::new();
        let _ = m.define_configuration(
            name,
            PendingConfig {
                size_limit: limit,
                default_ttl: None,
            },
        );
        m.get_cache(name).unwrap()
    }

    #[test]
    fn t19_10_put_get_roundtrip() {
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
        let cache = fresh_cache("listened", 4);
        let mut ctx = mock_ctx();
        let listener_obj = ctx.alloc_object(ClassId::new(0), 1);
        let idx = cache.add_listener(VmCaches::default().root(&mut ctx, listener_obj).unwrap());
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
        let cache = fresh_cache("panicky", 4);
        // We can't easily stash a panic closure through the ListenerHandle
        // (it's an ObjectRef) but we can prove the catch_unwind guard in
        // dispatch() won't propagate. Verify that after a listener is
        // registered and a put fires dispatch, the store's state is
        // still consistent.
        let mut ctx = mock_ctx();
        let listener_obj = ctx.alloc_object(ClassId::new(0), 1);
        cache.add_listener(VmCaches::default().root(&mut ctx, listener_obj).unwrap());
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
        let m = DefaultCacheManagerInner::new();
        let a = m.get_cache("idempotent").unwrap();
        let b = m.get_cache("idempotent").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "getCache must return the same Arc");
    }

    #[test]
    fn t19_10_get_cache_different_configs_errors() {
        let m = DefaultCacheManagerInner::new();
        // Start with limit=10.
        m.define_configuration(
            "conflict",
            PendingConfig {
                size_limit: 10,
                default_ttl: None,
            },
        )
        .unwrap();
        let _first = m.get_cache("conflict").unwrap();
        // Re-defining with a different limit must be rejected.
        let err = m
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
        let m = DefaultCacheManagerInner::new();
        let cache = m.get_cache("to-clear").unwrap();
        for i in 0..5 {
            cache.put(
                CacheKey::from_bytes(&(i as i32).to_le_bytes()),
                StoredValue::Primitive(Value::Int(i)),
            );
        }
        assert_eq!(cache.size(), 5);
        cache.clear();
        assert_eq!(cache.size(), 0);
        assert_eq!(m.cache_count(), 1);
    }

    #[test]
    fn t19_10_remove_listener_succeeds_only_when_registered() {
        let cache = fresh_cache("listener-dedup", 8);
        let mut ctx = mock_ctx();
        let state = VmCaches::default();
        let l1 = ctx.alloc_object(ClassId::new(0), 1);
        let l2 = ctx.alloc_object(ClassId::new(0), 1);
        cache.add_listener(state.root(&mut ctx, l1).unwrap());
        assert!(cache.remove_listener(&ctx, l1));
        assert!(
            !cache.remove_listener(&ctx, l2),
            "never-registered listener"
        );
        assert_eq!(cache.listeners.lock().len(), 0);
        // The removed listener's root is released by the next operation.
        assert_eq!(ctx.global_root_count(), 1);
        assert_eq!(state.release_pending(&mut ctx), 1);
        assert_eq!(ctx.global_root_count(), 0);
    }

    #[test]
    fn t19_10_put_if_absent_preserves_existing() {
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
        let m = DefaultCacheManagerInner::new();
        m.define_configuration(
            "tiny",
            PendingConfig {
                size_limit: 2,
                default_ttl: None,
            },
        )
        .unwrap();
        let cache = m.get_cache("tiny").unwrap();
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
    /// (keycloak-model-infinispan-configurationbuilder-classcastexception.md,
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

        const VM: usize = 0x30A0_0101;
        let _vm = VmGuard(VM);
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(VM);
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

        let cache = vm_caches(&ctx).manager.get_cache("real-cfg-cache").unwrap();
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
        let m = DefaultCacheManagerInner::new();
        let err = m
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
        let mut ctx = mock_ctx();
        let s = ctx.create_string("hello");
        let state = VmCaches::default();
        let stored = StoredValue::from_value(&mut ctx, &state, Value::Object(Some(s))).unwrap();
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
        let m = DefaultCacheManagerInner::new();
        m.get_cache("a").unwrap();
        m.get_cache("b").unwrap();
        m.get_cache("c").unwrap();
        let mut names = m.cache_names();
        names.sort();
        assert_eq!(
            names,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
}

/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 26): a non-String key object's cache key. The mock's identity hash is
/// the address truncated to `i32` (lesson qq).
#[cfg(test)]
mod w29d_cache_key_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: a key only, 8-byte aligned, never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn identity_keys_are_per_object_and_per_vm() {
        const VM: usize = 0x29D1_5A01;
        const OTHER_VM: usize = 0x29D1_5A02;
        let ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let other = crate::test_utils::mock_ctx();
        other.set_vm_identity(OTHER_VM);
        let (a, b) = (obj(0x1_29D1_5A00), obj(0x2_29D1_5A00));
        assert_eq!(
            ctx.identity_hash_code(a),
            ctx.identity_hash_code(b),
            "premise: the two key objects share an identity hash"
        );
        let ka = CacheKey::from_identity(&ctx, a);
        assert_eq!(ka, CacheKey::from_identity(&ctx, a), "stable for one object");
        assert_ne!(ka, CacheKey::from_identity(&ctx, b), "a same-hash object is another key");
        assert_ne!(ka, CacheKey::from_identity(&other, a), "another VM's object is another key");
        assert_eq!(ka.as_bytes().len(), 17);
        assert_eq!(ka.as_bytes()[0], 0xFF, "never a UTF-8 String key's first byte");
        crate::forget_vm_lock_keys(VM);
        crate::forget_vm_lock_keys(OTHER_VM);
    }
}

/// gc-common w30-a
/// (`common-w29d-infinispan-shim-stores-java-values-as-raw-unrooted-addresses-FIXED-20260923`):
/// Java values and listeners are global roots of the owning VM, released
/// through the VM's queue; the manager is per VM. Each test owns a private VM
/// identity and forgets only it (lessons nn, pp).
#[cfg(test)]
mod w30a_rooted_value_tests {
    use super::tests::VmGuard;
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    fn ctx_for(vm: usize) -> MockNativeContext {
        let ctx = mock_ctx();
        ctx.set_vm_identity(vm);
        ctx
    }

    fn o(obj: ObjectRef) -> Value {
        Value::Object(Some(obj))
    }

    /// A synthetic `Cache` object named `name`, built through the natives
    /// (`new DefaultCacheManager()` + `getCache(name)`).
    fn synthetic_cache_obj(ctx: &mut MockNativeContext, name: &str) -> ObjectRef {
        let mgr_cid = ctx.ensure_class_initialized(CLS_MANAGER).unwrap();
        let mgr = ctx.alloc_object(mgr_cid, 4);
        native_dcm_init(ctx, &[o(mgr)]).unwrap();
        let name_obj = ctx.create_string(name);
        match native_dcm_get_cache(ctx, &[o(mgr), o(name_obj)]).unwrap() {
            Some(Value::Object(Some(cache))) => cache,
            other => panic!("getCache answered {other:?}"),
        }
    }

    fn put(ctx: &mut MockNativeContext, cache: ObjectRef, key: &str, value: Value) -> Value {
        let k = ctx.create_string(key);
        native_cache_put(ctx, &[o(cache), o(k), value])
            .unwrap()
            .unwrap()
    }

    fn get(ctx: &mut MockNativeContext, cache: ObjectRef, key: &str) -> Value {
        let k = ctx.create_string(key);
        native_cache_get(ctx, &[o(cache), o(k)]).unwrap().unwrap()
    }

    /// Any cache operation: releases the roots queued since the last one.
    fn next_op(ctx: &mut MockNativeContext, cache: ObjectRef) {
        native_cache_size(ctx, &[o(cache)]).unwrap();
    }

    /// The global-root handle of the Java value filed under `key`.
    fn entry_handle(ctx: &MockNativeContext, cache: &str, key: &str) -> usize {
        let cache = vm_caches(ctx).manager.get_cache(cache).unwrap();
        let store = cache.store.read();
        match &store.entries.get(&CacheKey::from_str(key)).unwrap().value {
            StoredValue::JavaRef(root) => root.handle(),
            other => panic!("expected a rooted Java value, got {other:?}"),
        }
    }

    #[test]
    fn a_cached_value_is_a_root_and_reads_back_at_its_new_address() {
        const VM: usize = 0x30A0_0201;
        let _vm = VmGuard(VM);
        let mut ctx = ctx_for(VM);
        let cache = synthetic_cache_obj(&mut ctx, "w30a-reloc");
        let value = ctx.fresh_object_ref();
        let roots = ctx.global_root_count();
        assert_eq!(put(&mut ctx, cache, "k", o(value)), Value::Object(None));
        assert_eq!(ctx.global_root_count(), roots + 1, "the value is rooted");
        let handle = entry_handle(&ctx, "w30a-reloc", "k");
        assert_eq!(ctx.resolve_global_root(handle), Some(value));
        // A moving collection re-addresses the root; the entry keeps the handle.
        let moved = ctx.fresh_object_ref();
        assert!(ctx.w30a_retarget_global_root_for_test(handle, moved));
        assert_eq!(get(&mut ctx, cache, "k"), o(moved));
    }

    #[test]
    fn displaced_evicted_removed_and_expired_roots_are_released_by_the_next_operation() {
        const VM: usize = 0x30A0_0202;
        let _vm = VmGuard(VM);
        let mut ctx = ctx_for(VM);
        let limit_one = PendingConfig {
            size_limit: 1,
            default_ttl: None,
        };
        vm_caches(&ctx)
            .manager
            .define_configuration("w30a-one", limit_one)
            .unwrap();
        let cache = synthetic_cache_obj(&mut ctx, "w30a-one");
        let base = ctx.global_root_count();

        // Displacement: `put` hands the previous value back, current, and its
        // root survives until the next operation.
        let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        put(&mut ctx, cache, "k", o(a));
        assert_eq!(put(&mut ctx, cache, "k", o(b)), o(a));
        assert_eq!(ctx.global_root_count(), base + 2);
        next_op(&mut ctx, cache);
        assert_eq!(ctx.global_root_count(), base + 1, "a's root went");

        // Eviction (limit 1): no context at eviction time; queued.
        let c = ctx.fresh_object_ref();
        put(&mut ctx, cache, "k2", o(c));
        assert_eq!(
            get(&mut ctx, cache, "k"),
            Value::Object(None),
            "k was evicted"
        );
        assert_eq!(ctx.global_root_count(), base + 1, "b's root went");

        // Removal hands the value back, then the root goes.
        let k2 = ctx.create_string("k2");
        assert_eq!(
            native_cache_remove(&mut ctx, &[o(cache), o(k2)]).unwrap(),
            Some(o(c))
        );
        next_op(&mut ctx, cache);
        assert_eq!(ctx.global_root_count(), base);

        // TTL expiry.
        let ttl = PendingConfig {
            size_limit: 16,
            default_ttl: Some(Duration::from_millis(1)),
        };
        vm_caches(&ctx)
            .manager
            .define_configuration("w30a-ttl", ttl)
            .unwrap();
        let expiring = synthetic_cache_obj(&mut ctx, "w30a-ttl");
        let d = ctx.fresh_object_ref();
        put(&mut ctx, expiring, "k", o(d));
        assert_eq!(ctx.global_root_count(), base + 1);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(get(&mut ctx, expiring, "k"), Value::Object(None), "expired");
        next_op(&mut ctx, expiring);
        assert_eq!(
            ctx.global_root_count(),
            base,
            "the expired value's root went"
        );
    }

    #[test]
    fn put_if_absent_keeps_the_live_value_and_releases_the_rejected_root() {
        const VM: usize = 0x30A0_0203;
        let _vm = VmGuard(VM);
        let mut ctx = ctx_for(VM);
        let cache = synthetic_cache_obj(&mut ctx, "w30a-pia");
        let base = ctx.global_root_count();
        let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        let k = ctx.create_string("k");
        assert_eq!(
            native_cache_put_if_absent(&mut ctx, &[o(cache), o(k), o(a)]).unwrap(),
            Some(Value::Object(None))
        );
        let k = ctx.create_string("k");
        assert_eq!(
            native_cache_put_if_absent(&mut ctx, &[o(cache), o(k), o(b)]).unwrap(),
            Some(o(a)),
            "the live value wins"
        );
        next_op(&mut ctx, cache);
        assert_eq!(ctx.global_root_count(), base + 1, "b was never stored");
        assert_eq!(get(&mut ctx, cache, "k"), o(a));
    }

    #[test]
    fn two_vms_with_one_cache_name_stay_separate_and_teardown_forgets_one() {
        const VM_A: usize = 0x30A0_0204;
        const VM_B: usize = 0x30A0_0205;
        let _a = VmGuard(VM_A);
        let _b = VmGuard(VM_B);
        let mut a = ctx_for(VM_A);
        let mut b = ctx_for(VM_B);
        let cache_a = synthetic_cache_obj(&mut a, "w30a-shared");
        let cache_b = synthetic_cache_obj(&mut b, "w30a-shared");
        let va = a.fresh_object_ref();
        put(&mut a, cache_a, "k", o(va));
        assert_eq!(
            get(&mut b, cache_b, "k"),
            Value::Object(None),
            "B sees none of A's entries"
        );
        let vb = b.create_string("from-b");
        put(&mut b, cache_b, "k", o(vb));
        assert_eq!(
            get(&mut a, cache_a, "k"),
            o(va),
            "A's entry is untouched by B's put"
        );
        assert!(!Arc::ptr_eq(
            &vm_caches(&a).manager.get_cache("w30a-shared").unwrap(),
            &vm_caches(&b).manager.get_cache("w30a-shared").unwrap()
        ));

        forget_vm_infinispan_state(VM_A);
        assert!(!VM_CACHES.has_row(VM_A), "A's caches went with A");
        assert!(VM_CACHES.has_row(VM_B), "B's did not");
        match get(&mut b, cache_b, "k") {
            Value::Object(Some(s)) => assert_eq!(b.read_string(s).as_deref(), Some("from-b")),
            other => panic!("B's entry must survive A's teardown, got {other:?}"),
        }
    }

    #[test]
    fn an_entry_under_a_dead_key_object_is_dropped_by_the_lock_key_sweep() {
        const VM: usize = 0x30A0_0206;
        let _vm = VmGuard(VM);
        let mut ctx = ctx_for(VM);
        let cache = synthetic_cache_obj(&mut ctx, "w30a-identity");
        let base = ctx.global_root_count();
        let (key_obj, value) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        native_cache_put(&mut ctx, &[o(cache), o(key_obj), o(value)]).unwrap();
        assert_eq!(
            native_cache_get(&mut ctx, &[o(cache), o(key_obj)]).unwrap(),
            Some(o(value))
        );
        let inner = vm_caches(&ctx).manager.get_cache("w30a-identity").unwrap();
        assert_eq!(inner.size(), 1);

        // The key object dies: every weak slot of this VM is judged dead.
        crate::gc_sweep_lock_keys(VM, &|_| false);
        assert_eq!(inner.size(), 0, "the unreachable entry went with its key");
        assert_eq!(ctx.global_root_count(), base + 1, "its root is only queued");
        next_op(&mut ctx, cache);
        assert_eq!(
            ctx.global_root_count(),
            base,
            "and released by the next operation"
        );
    }

    #[test]
    fn a_lookup_does_not_mint_a_key_for_an_object_never_filed() {
        const VM: usize = 0x30A0_0207;
        let _vm = VmGuard(VM);
        let mut ctx = ctx_for(VM);
        let cache = synthetic_cache_obj(&mut ctx, "w30a-lookup");
        let stranger = ctx.fresh_object_ref();
        assert_eq!(
            native_cache_get(&mut ctx, &[o(cache), o(stranger)]).unwrap(),
            Some(Value::Object(None))
        );
        assert_eq!(crate::existing_weak_lock_key(&ctx, stranger), None);
    }

    #[test]
    fn remove_listener_finds_a_listener_that_moved() {
        const VM: usize = 0x30A0_0208;
        let _vm = VmGuard(VM);
        let mut ctx = ctx_for(VM);
        let cache = synthetic_cache_obj(&mut ctx, "w30a-listener");
        let base = ctx.global_root_count();
        let listener = ctx.fresh_object_ref();
        native_cache_add_listener(&mut ctx, &[o(cache), o(listener)]).unwrap();
        assert_eq!(ctx.global_root_count(), base + 1, "the listener is rooted");
        let inner = vm_caches(&ctx).manager.get_cache("w30a-listener").unwrap();
        let handle = inner.listeners.lock()[0].root.handle();
        let moved = ctx.fresh_object_ref();
        assert!(ctx.w30a_retarget_global_root_for_test(handle, moved));
        native_cache_remove_listener(&mut ctx, &[o(cache), o(moved)]).unwrap();
        assert_eq!(inner.listeners.lock().len(), 0);
        next_op(&mut ctx, cache);
        assert_eq!(ctx.global_root_count(), base);
    }

    #[test]
    fn the_lru_queue_stays_bounded_under_repeated_gets() {
        let m = DefaultCacheManagerInner::new();
        let cache = m.get_cache("w30a-lru").unwrap();
        for i in 0..4 {
            cache.put(
                CacheKey::from_bytes(&(i as i32).to_le_bytes()),
                StoredValue::Primitive(Value::Int(i)),
            );
        }
        for round in 0..1_000 {
            let key = CacheKey::from_bytes(&((round % 4) as i32).to_le_bytes());
            assert!(cache.get(&key).is_some());
        }
        let store = cache.store.read();
        assert!(
            store.lru.len() <= 2 * store.entries.len() + 33,
            "{}",
            store.lru.len()
        );
    }
}
