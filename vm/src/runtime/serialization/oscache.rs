// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ObjectStreamClass` per-class cache.
//!
//! Each `Class` has (at most) one `ObjectStreamClass` descriptor. Once a
//! call to `ObjectStreamClass.lookup(cls)` has built a descriptor for a
//! given `ClassId`, every subsequent `lookup(cls)` of the same class
//! must return the *same* object (reference-identity equality, not just
//! structural equality).
//!
//! Rationale — the real JDK caches in `ObjectStreamClass.Caches.localDescs`,
//! a `ConcurrentMap<WeakClassKey, SoftReference<ObjectStreamClass>>`.
//! User code frequently relies on identity: e.g. `osc1 == osc2` is a
//! cheap test in the real JDK that this cache makes true for us as
//! well. More importantly, populating the descriptor is *expensive*
//! (walks the whole super-class chain, reflects every field and the 4
//! serialization hook methods) — doing that once per class is an
//! important boot-time win for apps like Keycloak that call `lookup`
//! thousands of times during CHM / AtomicInteger / AtomicLong clinit.
//!
//! # Thread safety
//!
//! Backed by `parking_lot::RwLock<FxHashMap>`. Reads are non-exclusive;
//! inserts briefly take the write lock. `ObjectRef` is `Copy + Send +
//! Sync`, so the cache value is trivially sharable across threads.
//!
//! # Lifetime
//!
//! Entries live as long as the `SharedVm`. Because `ObjectStreamClass`
//! descriptors themselves hold references back into the VM heap (the
//! constructor, method, and field mirrors), we do *not* use a soft
//! reference — in cratonvm's current GC we don't have softref semantics,
//! and clearing the cache would re-open the NPE this module exists to
//! fix.
//!
//! # Reset semantics
//!
//! `clear()` is only called in tests (see `tests.rs`). Production code
//! should treat entries as permanent — removing one and then reading it
//! back would produce two `ObjectStreamClass` instances for the same
//! class, violating the identity contract.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use cratonvm_types::{ClassId, ObjectRef};

/// Process-global registry of every live [`OscCache`]'s backing map.
///
/// The GC-root registry (`crate::memory::native_roots`) drives scanning
/// and remapping through bare `fn` pointers, which cannot capture a
/// specific `OscCache` instance. To bridge that, each cache publishes a
/// raw pointer to its `inner` map here on first insert; the global
/// [`scan_osc_cache_roots`] / [`remap_osc_cache_refs`] functions walk
/// this list.
///
/// # Safety / lifetime
///
/// Entries are raw pointers into `OscCache::inner`, which lives as long
/// as the owning `SharedVm` (see module docs — `OscCache` is a permanent
/// per-VM field, never moved or dropped during normal execution). The
/// VM does not tear down `SharedVm` mid-run, so these pointers stay
/// valid for the process lifetime. We never form a `&` reference that
/// outlives a lock acquisition: each scan/remap re-locks the target map
/// freshly through its `RwLock`, so there is no aliasing with the
/// owning cache's own `read()`/`write()` guards.
///
/// Lifetime is enforced by the registry lock itself, not by the "caches
/// outlive the process" assumption above (which the unit tests break, and
/// which any teardown path would break): [`scan_osc_cache_roots`] and
/// [`remap_osc_cache_refs`] hold the registry guard for the whole walk, and
/// [`OscCache::drop`] must take that same guard to deregister, so a
/// registered map cannot be freed while a walk is dereferencing it.
type OscMap = RwLock<FxHashMap<ClassId, ObjectRef>>;

/// Lazily-initialised list of live cache backing maps. `Mutex` (not
/// `RwLock`) because registration is rare (once per VM) and the GC scan
/// only needs a short read of the pointer list before re-locking each
/// map individually.
fn cache_registry() -> &'static Mutex<Vec<SendPtr>> {
    static REGISTRY: OnceLock<Mutex<Vec<SendPtr>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

// SAFETY: the registry stores raw `*const OscMap` pointers that are only
// ever dereferenced under the target map's own `RwLock`. `OscMap`
// (`RwLock<FxHashMap<ClassId, ObjectRef>>`) is itself `Send + Sync`
// (`ObjectRef` is `Send + Sync`), so sharing these pointers across
// threads for GC scanning is sound under the VM's safepoint model — the
// GC observes them only at a stop-the-world safepoint, when no other
// thread mutates the maps.
#[derive(Clone, Copy)]
struct SendPtr(*const OscMap);
// SAFETY: see the comment above `SendPtr`.
unsafe impl Send for SendPtr {}

/// Push every cached descriptor `ObjectRef` from every live cache onto
/// `roots`, so the GC marks them as live. Null/dangling refs are never
/// stored (the cache only holds descriptors returned from a successful
/// build), but we still skip the null bit-pattern defensively.
///
/// Registered with the native-root registry via
/// [`register_with_gc`]; must be a bare `fn` for that API.
pub fn scan_osc_cache_roots(roots: &mut Vec<ObjectRef>) {
    // The registry lock is held for the WHOLE walk, not just long enough to
    // clone the pointer list.
    //
    // It used to snapshot-then-release, "to keep lock ordering simple (registry
    // -> map, never the reverse)" — but holding the registry across `map.read()`
    // *is* registry -> map, the same order, so the early release bought nothing
    // and opened a use-after-free: `OscCache::drop` deregisters under this very
    // lock and then frees the map, so a cache dropped between the snapshot and
    // the `&*ptr` below left a dangling pointer in the snapshot. Reading freed
    // memory either faults outright or, if the freed bytes happen to look like a
    // locked `RwLock`, parks `map.read()` forever.
    //
    // That is not theoretical: `runtime::serialization::oscache`'s own tests
    // create and drop short-lived stack `OscCache`s while other threads run GC,
    // and 1500 filtered runs of that module produced 5 SIGSEGVs and 2 permanent
    // hangs. Holding the guard closes the window — `drop` cannot make progress
    // while we walk, and no path takes the registry while holding a map lock
    // (`register_with_gc` releases the registry before its caller touches
    // `inner`), so the order stays total.
    let guard = cache_registry().lock();
    for &SendPtr(ptr) in guard.iter() {
        // SAFETY: `ptr` names a map whose owning `OscCache` is still registered,
        // and `OscCache::drop` must take the registry lock we are holding in
        // order to deregister and be freed — so the map cannot go away while
        // this reference lives. We re-lock through its own `RwLock`, so there is
        // no aliasing with the owner's guards either.
        let map = unsafe { &*ptr };
        for (&class_id, desc) in map.read().iter() {
            // `ObjectRef` is non-null by construction; the guard is
            // belt-and-suspenders against a future nullable value type.
            if !desc.as_ptr().is_null() {
                if cratonvm_types::metadata_pin::metadata_weak_mode() {
                    if let Some(loader) =
                        cratonvm_types::loader_pin::loader_pin_addr(class_id.as_u32())
                    {
                        cratonvm_types::metadata_pin::add_metadata_pin(
                            loader,
                            desc.as_ptr() as usize,
                        );
                        continue;
                    }
                }
                roots.push(*desc);
            }
        }
    }
}

/// Rewrite every cached descriptor `ObjectRef` through `map`
/// (old_addr -> new_addr) after a moving GC relocates the heap. Entries
/// whose address is absent from `map` are left untouched (a non-moved
/// object, or one already updated).
///
/// Registered with the native-root registry via [`register_with_gc`];
/// must be a bare `fn` for that API.
pub fn remap_osc_cache_refs(map: &HashMap<usize, usize>) {
    // Registry guard held across the walk — see `scan_osc_cache_roots` for why
    // the snapshot-then-release this replaces was a use-after-free.
    let guard = cache_registry().lock();
    for &SendPtr(ptr) in guard.iter() {
        // SAFETY: see `scan_osc_cache_roots`. We take the *write* lock
        // because we mutate the stored refs in place.
        let osc_map = unsafe { &*ptr };
        let mut guard = osc_map.write();
        for desc in guard.values_mut() {
            if desc.as_ptr().is_null() {
                continue;
            }
            let old = desc.as_ptr() as usize;
            if let Some(&new) = map.get(&old) {
                // SAFETY: `new` is a live, 8-byte-aligned heap address
                // produced by the moving collector's forwarding table
                // (the same invariant every other root-remapper relies
                // on). `from_raw` only debug-asserts non-null/alignment.
                *desc = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }
}

/// Register this cache's backing map with the process-global registry
/// **and** (once per process) hook [`scan_osc_cache_roots`] /
/// [`remap_osc_cache_refs`] into the GC's native-root registry.
///
/// Idempotent per cache: the same `inner` pointer is added at most once.
/// The GC-registry hook is guarded by a `OnceLock` so it fires exactly
/// once for the whole process even though every `OscCache` calls this on
/// its first insert.
fn register_with_gc(inner: &OscMap) {
    let ptr = inner as *const OscMap;
    {
        let mut guard = cache_registry().lock();
        if guard.iter().any(|s| s.0 == ptr) {
            return;
        }
        guard.push(SendPtr(ptr));
    }

    // Wire the scan/remap pair into the VM-wide native-root registry
    // exactly once. `register_native_root_source` is idempotent, but we
    // still gate on a `OnceLock` to avoid taking its lock on every cache
    // creation.
    static GC_HOOK: OnceLock<()> = OnceLock::new();
    GC_HOOK.get_or_init(|| {
        crate::memory::native_roots::register_native_root_source(
            scan_osc_cache_roots,
            remap_osc_cache_refs,
        );
    });
}

/// A per-VM cache of `ObjectStreamClass` mirror objects keyed by
/// `ClassId`.
///
/// Constructed once during `SharedVm::new` and reused across every
/// native call to `ObjectStreamClass.lookup`. Thread-safe; cheap to
/// `get`, slightly more expensive to `get_or_insert_with` only on the
/// very first lookup of a class.
#[derive(Default)]
pub struct OscCache {
    inner: RwLock<FxHashMap<ClassId, ObjectRef>>,
}

impl OscCache {
    /// Create a new, empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch the cached descriptor for `class_id`, or `None` if none
    /// has been built yet.
    pub fn get(&self, class_id: ClassId) -> Option<ObjectRef> {
        self.inner.read().get(&class_id).copied()
    }

    /// Fetch the cached descriptor for `class_id`, building it via
    /// `build` if this is the first call for this class.
    ///
    /// `build` is called *without* the cache's write lock held (to
    /// avoid deadlocks when the builder itself needs to do further
    /// class loading or reflection). If two threads race to build the
    /// same descriptor, both calls to `build` will run, but only one
    /// result wins the insertion — the loser's `ObjectRef` is dropped
    /// on the floor (still reachable from Java-side roots via the
    /// caller's stack, so no leak; GC will collect it at next cycle).
    ///
    /// The roadmap's identity acceptance ("two `lookup(cls)` calls
    /// return the same instance") is satisfied because every call
    /// *after* the first returns the winning insertion.
    pub fn get_or_insert_with<F>(&self, class_id: ClassId, build: F) -> ObjectRef
    where
        F: FnOnce() -> ObjectRef,
    {
        // Fast path: already cached.
        if let Some(existing) = self.get(class_id) {
            return existing;
        }
        let fresh = build();
        // Ensure this cache's descriptors are GC-rooted/remapped before
        // the first ref is parked in the map (the only reference to a
        // descriptor may now be the cache entry).
        register_with_gc(&self.inner);
        let mut guard = self.inner.write();
        *guard.entry(class_id).or_insert(fresh)
    }

    /// Insert `desc` for `class_id` unconditionally, returning the
    /// descriptor that ends up in the cache (either `desc` or the
    /// pre-existing entry if one was present).
    pub fn insert_if_absent(&self, class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        // See `get_or_insert_with`: register for GC root-scanning before
        // we become the sole holder of this descriptor.
        register_with_gc(&self.inner);
        let mut guard = self.inner.write();
        *guard.entry(class_id).or_insert(desc)
    }

    /// Number of cached descriptors. Primarily for tests / JMX.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Scan this VM's descriptor cache directly from its owning `SharedVm`.
    ///
    /// The process-global callback remains as a compatibility backstop, but a
    /// VM-owned cache must not depend on lazy raw-pointer registration for
    /// correctness or unloading. Direct ownership also lets loader metadata
    /// remain a conditional edge during a full class-unloading mark.
    pub fn scan_roots(&self, roots: &mut Vec<ObjectRef>) {
        for (&class_id, desc) in self.inner.read().iter() {
            if cratonvm_types::metadata_pin::metadata_weak_mode() {
                if let Some(loader) =
                    cratonvm_types::loader_pin::loader_pin_addr(class_id.as_u32())
                {
                    cratonvm_types::metadata_pin::add_metadata_pin(
                        loader,
                        desc.as_ptr() as usize,
                    );
                    continue;
                }
            }
            roots.push(*desc);
        }
    }

    /// Rewrite cached descriptors after a moving collection.
    pub fn remap_roots(&self, pointer_map: &HashMap<usize, usize>) {
        if pointer_map.is_empty() {
            return;
        }
        for desc in self.inner.write().values_mut() {
            if let Some(&new) = pointer_map.get(&(desc.as_ptr() as usize)) {
                // SAFETY: collector forwarding maps contain live aligned
                // object starts.
                *desc = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }

    /// Release descriptor mirrors owned by unloaded classes.
    pub fn remove_classes(&self, class_ids: &rustc_hash::FxHashSet<ClassId>) -> usize {
        let mut map = self.inner.write();
        let before = map.len();
        map.retain(|id, _| !class_ids.contains(id));
        before - map.len()
    }

    /// Drop every cached descriptor. Test-only; production code should
    /// not call this (see module docs).
    #[doc(hidden)]
    pub fn clear(&self) {
        self.inner.write().clear();
    }
}

impl Drop for OscCache {
    /// Remove this cache's backing map from the process-global registry on
    /// teardown, so the GC never dereferences a pointer to a freed `OscMap`.
    ///
    /// `insert_if_absent` / `get_or_insert_with` register `&self.inner` via
    /// [`register_with_gc`] on first use, but nothing un-registered it: a
    /// production cache outlives the VM, so this never bit real code, but it
    /// is a latent use-after-free for any teardown path. It *did* bite the
    /// unit tests, which create and drop many short-lived caches sharing the
    /// one global registry — a dropped cache left a dangling pointer that a
    /// later `scan_osc_cache_roots` / `remap_osc_cache_refs` would read.
    /// Deregistering on `Drop` keeps the registry tracking only live maps.
    fn drop(&mut self) {
        let ptr = &self.inner as *const OscMap;
        cache_registry().lock().retain(|s| s.0 != ptr);
    }
}

/// A borrow handle into the cache. Currently an alias, but exposed
/// separately so future work can narrow the API surface (e.g. restrict
/// clear() to a privileged handle).
pub type OscCacheHandle<'a> = &'a OscCache;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    /// Build a fake `ObjectRef` with a distinct pointer value. Because
    /// the cache only stores and returns the ref (never dereferences
    /// it), we can fabricate one pointing at an integer for unit-test
    /// purposes. 8-byte alignment is required by `ObjectRef::from_raw`.
    fn fake_ref(n: usize) -> ObjectRef {
        // SAFETY: we never dereference this pointer in this test; the
        // cache only stores and returns it by copy.
        unsafe { ObjectRef::from_raw((n * 8 + 8) as *mut u8) }
    }

    #[test]
    fn empty_cache_returns_none() {
        let cache = OscCache::new();
        assert!(cache.is_empty());
        assert!(cache.get(ClassId::new(1)).is_none());
    }

    #[test]
    fn insert_and_retrieve() {
        let cache = OscCache::new();
        let r = fake_ref(1);
        cache.insert_if_absent(ClassId::new(7), r);
        assert_eq!(cache.get(ClassId::new(7)), Some(r));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn get_or_insert_with_builds_once() {
        let cache = OscCache::new();
        let cid = ClassId::new(42);
        let r1 = fake_ref(1);
        let r2 = fake_ref(2);

        let a = cache.get_or_insert_with(cid, || r1);
        let b = cache.get_or_insert_with(cid, || r2); // should NOT rebuild
        assert_eq!(a, r1);
        assert_eq!(b, r1, "second lookup must return the first insert");
    }

    #[test]
    fn different_classes_have_different_entries() {
        let cache = OscCache::new();
        let r1 = fake_ref(1);
        let r2 = fake_ref(2);
        cache.insert_if_absent(ClassId::new(1), r1);
        cache.insert_if_absent(ClassId::new(2), r2);
        assert_eq!(cache.get(ClassId::new(1)), Some(r1));
        assert_eq!(cache.get(ClassId::new(2)), Some(r2));
        assert_ne!(
            cache.get(ClassId::new(1)),
            cache.get(ClassId::new(2)),
            "distinct classes must yield distinct descriptors"
        );
    }

    #[test]
    fn concurrent_inserts_are_idempotent() {
        // Two threads racing on the same class must land on the same
        // descriptor. The winner is whichever thread's insert runs
        // first under the write lock; whichever loses drops its
        // ObjectRef on the floor but both observers see the same value.
        let cache = Arc::new(OscCache::new());
        let cid = ClassId::new(99);

        let cache1 = Arc::clone(&cache);
        let cache2 = Arc::clone(&cache);
        let t1 = thread::spawn(move || cache1.get_or_insert_with(cid, || fake_ref(1)));
        let t2 = thread::spawn(move || cache2.get_or_insert_with(cid, || fake_ref(2)));
        let v1 = t1.join().unwrap();
        let v2 = t2.join().unwrap();
        assert_eq!(v1, v2, "concurrent inserts must agree on the winner");
    }

    #[test]
    fn clear_resets_the_cache() {
        let cache = OscCache::new();
        cache.insert_if_absent(ClassId::new(1), fake_ref(1));
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.get(ClassId::new(1)).is_none());
    }

    /// Register a cache's backing map with the global registry *without*
    /// going through `register_with_gc` — the latter also hooks the VM's
    /// native-root registry, which we don't want to touch from a unit
    /// test. This exercises the scan/remap logic in isolation.
    fn register_for_test(cache: &OscCache) {
        let ptr = &cache.inner as *const OscMap;
        let mut guard = cache_registry().lock();
        // `SendPtr` is `Copy + Send` but deliberately not `PartialEq`, so
        // dedup by comparing the wrapped raw pointer, mirroring the production
        // `register_with_gc` idiom above.
        if !guard.iter().any(|s| s.0 == ptr) {
            guard.push(SendPtr(ptr));
        }
    }

    #[test]
    fn scan_yields_cached_descriptor() {
        // Use a high, distinctive pointer value to avoid colliding with
        // other tests sharing the process-global registry.
        let seeded = fake_ref(0x5_0001);
        let cache = OscCache::new();
        cache.insert_if_absent(ClassId::new(5001), seeded);
        register_for_test(&cache);

        let mut roots = Vec::new();
        scan_osc_cache_roots(&mut roots);
        assert!(
            roots.contains(&seeded),
            "scan must surface the cached descriptor as a GC root"
        );
    }

    #[test]
    fn remap_rewrites_cached_descriptor() {
        let old_ref = fake_ref(0x6_0001);
        let new_ref = fake_ref(0x6_0002);
        let cache = OscCache::new();
        let cid = ClassId::new(6001);
        cache.insert_if_absent(cid, old_ref);
        register_for_test(&cache);

        let mut map = HashMap::new();
        map.insert(old_ref.as_ptr() as usize, new_ref.as_ptr() as usize);
        remap_osc_cache_refs(&map);

        assert_eq!(
            cache.get(cid),
            Some(new_ref),
            "remap must rewrite the cached ref to its relocated address"
        );
    }

    #[test]
    fn remap_leaves_unmoved_refs_untouched() {
        let kept = fake_ref(0x7_0001);
        let cache = OscCache::new();
        let cid = ClassId::new(7001);
        cache.insert_if_absent(cid, kept);
        register_for_test(&cache);

        // A remap table that mentions a *different* address must not
        // disturb our entry.
        let mut map = HashMap::new();
        map.insert(0xDEAD_0000usize, 0xBEEF_0000usize);
        remap_osc_cache_refs(&map);

        assert_eq!(cache.get(cid), Some(kept));
    }
}
