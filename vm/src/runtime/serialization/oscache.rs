// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    ///
    /// Hands `visit` every cached `(class, descriptor)`; the caller decides
    /// whether to root the descriptor or pin it to the class's defining
    /// loader (`native_roots::defer_or_root`). The decision used to be made
    /// here, from the process-global `metadata_weak_mode()` and the VM-less
    /// `loader_pin_addr`, with no `metadata_pin_deferrable` check (gc-common
    /// w5-b).
    pub fn scan_roots(&self, visit: &mut dyn FnMut(ClassId, ObjectRef)) {
        for (&class_id, desc) in self.inner.read().iter() {
            visit(class_id, *desc);
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
mod tests;
