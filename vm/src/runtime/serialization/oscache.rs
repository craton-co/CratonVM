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
//! reference — in rustjvm's current GC we don't have softref semantics,
//! and clearing the cache would re-open the NPE this module exists to
//! fix.
//!
//! # Reset semantics
//!
//! `clear()` is only called in tests (see `tests.rs`). Production code
//! should treat entries as permanent — removing one and then reading it
//! back would produce two `ObjectStreamClass` instances for the same
//! class, violating the identity contract.

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use rustjvm_types::{ClassId, ObjectRef};

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

    /// Drop every cached descriptor. Test-only; production code should
    /// not call this (see module docs).
    #[doc(hidden)]
    pub fn clear(&self) {
        self.inner.write().clear();
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
}
