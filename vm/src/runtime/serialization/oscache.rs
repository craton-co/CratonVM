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
//!
//! # GC rooting (per-VM, NOT process-global)
//!
//! The cache holds `ObjectRef`s that may be the *only* reference to a
//! descriptor, so it must be both scanned (else the descriptor is
//! reclaimed) and remapped (else the entry points into vacated
//! from-space). Both halves live on [`OscCache::scan_roots`] /
//! [`OscCache::remap_roots`] and are driven from
//! `memory::native_roots::VM_ROOT_SOURCES` with the **owning**
//! `SharedVm`, so a collection in VM B never reports or rewrites VM A's
//! descriptors.
//!
//! This replaces a process-global `Mutex<Vec<*const OscMap>>` registry
//! that every `OscCache` published a raw pointer into, hooked into the
//! VM-agnostic `register_native_root_source` fan-out. That was wrong
//! twice over:
//!
//!   * **Isolation / memory safety.** The fan-out has no VM parameter,
//!     so *every* VM's collection walked *every* live cache: VM B's root
//!     scan handed its collector addresses belonging to VM A's heap, and
//!     VM B's post-move fixup rewrote VM A's entries through VM B's
//!     relocation map. Both are the "pointer into a heap this collector
//!     does not own" hazard that scoped the logmanager and
//!     security-manager tables.
//!   * **Lifetime.** Keeping the registry sound required a `SendPtr`
//!     raw-pointer wrapper, a `Drop` impl to deregister, and holding the
//!     registry lock across every walk — machinery that had already
//!     produced SIGSEGVs and permanent hangs in this module's own tests.
//!     Owning the scan through `SharedVm` removes the raw pointer, the
//!     `unsafe`, and the teardown ordering problem outright.

use std::collections::HashMap;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use cratonvm_types::{ClassId, ObjectRef};

/// The backing map of one VM's cache. Reachable only through the owning
/// [`OscCache`], which is a field of that VM's `ClassRealm` — there is no
/// process-global handle on it, by design (see the module docs).
type OscMap = RwLock<FxHashMap<ClassId, ObjectRef>>;

/// A per-VM cache of `ObjectStreamClass` mirror objects keyed by
/// `ClassId`.
///
/// Constructed once during `SharedVm::new` and reused across every
/// native call to `ObjectStreamClass.lookup`. Thread-safe; cheap to
/// `get`, slightly more expensive to `get_or_insert_with` only on the
/// very first lookup of a class.
#[derive(Default)]
pub struct OscCache {
    inner: OscMap,
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
        // No registration step: this cache is a field of the owning VM's
        // `ClassRealm`, and `memory::native_roots` scans it through that
        // `SharedVm` on every collection — so a descriptor is rooted from
        // the moment the VM exists, not from the first insert.
        let mut guard = self.inner.write();
        *guard.entry(class_id).or_insert(fresh)
    }

    /// Insert `desc` for `class_id` unconditionally, returning the
    /// descriptor that ends up in the cache (either `desc` or the
    /// pre-existing entry if one was present).
    pub fn insert_if_absent(&self, class_id: ClassId, desc: ObjectRef) -> ObjectRef {
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

    /// Scan this VM's descriptor cache from its owning `SharedVm`.
    ///
    /// This is the ONLY scan path (`memory::native_roots`' `"osc-cache"`
    /// source drives it); there is no process-global backstop, so a
    /// collection in another VM can neither see nor mark these refs.
    /// Direct ownership also lets loader metadata remain a conditional
    /// edge during a full class-unloading mark.
    pub fn scan_roots(&self, vm_identity: usize, roots: &mut Vec<ObjectRef>) {
        for (&class_id, desc) in self.inner.read().iter() {
            if cratonvm_types::metadata_pin::metadata_weak_mode() {
                if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr(class_id.as_u32())
                {
                    cratonvm_types::metadata_pin::add_metadata_pin(
                        vm_identity,
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
    pub fn remap_roots(&self, pointer_map: &cratonvm_types::PointerMap) {
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

// NOTE: there is deliberately no `Drop` impl. The previous one existed only
// to deregister this cache's raw backing-map pointer from a process-global
// registry before the map was freed — a use-after-free the registry itself
// created. With the cache owned by its `SharedVm` and scanned through it,
// dropping the cache drops its refs and nothing outside can still name them.

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

    #[test]
    fn scan_yields_cached_descriptor() {
        let seeded = fake_ref(0x5_0001);
        let cache = OscCache::new();
        cache.insert_if_absent(ClassId::new(5001), seeded);

        let mut roots = Vec::new();
        cache.scan_roots(0, &mut roots);
        assert!(
            roots.contains(&seeded),
            "scan must surface the cached descriptor as a GC root"
        );
    }

    /// The isolation property the process-global registry did not have:
    /// scanning ONE cache must not report another cache's descriptors.
    ///
    /// Under the old `Mutex<Vec<*const OscMap>>` fan-out, every live
    /// cache in the process contributed to every root scan, so a second
    /// VM's collector received addresses belonging to a heap it does not
    /// own. This asserts the scan is bounded by the receiver.
    #[test]
    fn scan_does_not_leak_another_caches_descriptors() {
        let mine = fake_ref(0x5_1001);
        let theirs = fake_ref(0x5_1002);
        let vm_a = OscCache::new();
        let vm_b = OscCache::new();
        vm_a.insert_if_absent(ClassId::new(5101), mine);
        vm_b.insert_if_absent(ClassId::new(5101), theirs);

        let mut roots = Vec::new();
        vm_a.scan_roots(0, &mut roots);
        assert!(roots.contains(&mine), "own descriptor must be rooted");
        assert!(
            !roots.contains(&theirs),
            "scanning VM A's cache must not surface VM B's descriptor \
             (that is a pointer into a heap A's collector does not own)"
        );
    }

    #[test]
    fn remap_rewrites_cached_descriptor() {
        let old_ref = fake_ref(0x6_0001);
        let new_ref = fake_ref(0x6_0002);
        let cache = OscCache::new();
        let cid = ClassId::new(6001);
        cache.insert_if_absent(cid, old_ref);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_ref.as_ptr() as usize, new_ref.as_ptr() as usize);
        cache.remap_roots(&map);

        assert_eq!(
            cache.get(cid),
            Some(new_ref),
            "remap must rewrite the cached ref to its relocated address"
        );
    }

    /// The remap counterpart of the scan-isolation test above: VM B's
    /// relocation map must not rewrite VM A's entries. Two heaps can
    /// hand out the same address, so a shared fan-out silently repointed
    /// the other VM's descriptor at an unrelated object.
    #[test]
    fn remap_does_not_rewrite_another_caches_descriptors() {
        let shared_addr = fake_ref(0x6_1001);
        let relocated = fake_ref(0x6_1002);
        let vm_a = OscCache::new();
        let vm_b = OscCache::new();
        let cid = ClassId::new(6101);
        vm_a.insert_if_absent(cid, shared_addr);
        vm_b.insert_if_absent(cid, shared_addr);

        // VM B collects and moves the object at `shared_addr`.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(shared_addr.as_ptr() as usize, relocated.as_ptr() as usize);
        vm_b.remap_roots(&map);

        assert_eq!(vm_b.get(cid), Some(relocated), "B's own entry moves");
        assert_eq!(
            vm_a.get(cid),
            Some(shared_addr),
            "B's relocation map must not rewrite A's entry"
        );
    }

    #[test]
    fn remap_leaves_unmoved_refs_untouched() {
        let kept = fake_ref(0x7_0001);
        let cache = OscCache::new();
        let cid = ClassId::new(7001);
        cache.insert_if_absent(cid, kept);

        // A remap table that mentions a *different* address must not
        // disturb our entry.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xDEAD_0000usize, 0xBEEF_0000usize);
        cache.remap_roots(&map);

        assert_eq!(cache.get(cid), Some(kept));
    }
}
