// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class unloading support for the garbage collector.
//!
//! When a `ClassLoader` object becomes unreachable during GC, all classes it
//! loaded can be unloaded: their metadata (constant pool, bytecode, method
//! descriptors) is reclaimed and any JIT-compiled code is invalidated.
//!
//! System/bootstrap loaders are never unloaded.
//!
//! # THIS MODULE IS NOT THE PRODUCTION CLASS UNLOADER
//!
//! Audit note, 2026-07-26 (`arch-2026-07-26/refs-metaspace-unloading`).
//! [`ClassUnloader`] and [`ClassLoaderHierarchy`] have **no caller anywhere in
//! the workspace** — a repo-wide search for `ClassUnloader`,
//! `ClassLoaderHierarchy` and `class_unloading::` matches only this file and
//! the `pub mod class_unloading;` line in `gc/src/lib.rs`. Nothing ever calls
//! `register_loader` / `register_class` / `register_jit_code`, so every table
//! below is permanently empty in a running VM.
//!
//! The real unload transaction lives outside the `gc` crate and does not touch
//! this type:
//!
//! * driver: `vm/src/runtime/interpreter.rs` — `process_references_after_gc`
//!   (post-GC path) and `g1_remark_process_references` (G1 final remark), both
//!   under STW;
//! * loader reachability: `native-builtins/src/classloader.rs` —
//!   `gc_reconcile_defining_loaders`, returning dead class-id hints;
//! * the transaction itself: `vm/src/memory/gc.rs` —
//!   `unload_dead_class_metadata`, which prunes statics, class locks, field
//!   descriptors, lambda proxies, mirrors, vtables, JIT profiles/tier
//!   state/deopt log/code cache and the initiating-resolution cache, then
//!   `classloading/src/class_manager.rs` — `unload_user_loader`.
//!
//! See `docs/architecture/class-loader-unloading.md` for the
//! invariants that transaction upholds, and
//! `refs-metaspace-unloading.md` for why this
//! scaffolding is being kept (correct, tested, and the natural home should the
//! GC ever need to own loader bookkeeping) rather than deleted.
//!
//! **Do not wire this in without re-reading that doc.** Two of the tables here
//! previously had no bound at all and would have violated the "unload
//! invalidation or hard bound" rule the moment they saw traffic; both are fixed
//! below and pinned by tests.

use parking_lot::Mutex;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Metadata tracked per class loader.
#[derive(Debug, Clone)]
pub struct ClassLoaderData {
    /// Address of the ClassLoader object on the heap.
    pub loader_addr: usize,
    /// ClassLoader name (for logging).
    pub name: String,
    /// Classes loaded by this loader.
    pub loaded_classes: Vec<ClassInfo>,
    /// Total metadata bytes used by this loader's classes.
    pub metadata_bytes: usize,
    /// Whether this is a system/bootstrap loader (never unloaded).
    pub is_system_loader: bool,
    /// Whether this loader has been marked as alive this GC cycle.
    pub alive: bool,
}

/// Information about a single loaded class.
#[derive(Debug, Clone)]
pub struct ClassInfo {
    /// Fully qualified class name (e.g., "java/lang/String").
    pub name: String,
    /// Class ID in the VM's class table.
    pub class_id: u32,
    /// Estimated metadata size: constant pool + bytecode + method structs.
    pub metadata_bytes: usize,
    /// Whether this class has JIT-compiled methods.
    pub has_jit_code: bool,
    /// Number of instances currently alive (tracked if enabled).
    pub instance_count: u64,
}

/// Result of a single class-unloading cycle.
#[derive(Debug, Default, Clone)]
pub struct ClassUnloadingResult {
    /// Number of class loaders unloaded.
    pub loaders_unloaded: usize,
    /// Number of classes unloaded.
    pub classes_unloaded: usize,
    /// Metadata bytes reclaimed.
    pub metadata_bytes_reclaimed: usize,
    /// JIT code cache entries invalidated.
    pub jit_entries_invalidated: usize,
    /// Names of unloaded classes (for logging/debugging).
    pub unloaded_class_names: Vec<String>,
    /// Names of unloaded loaders.
    pub unloaded_loader_names: Vec<String>,
    /// Heap addresses of the unloaded loaders.
    ///
    /// BOUNDED-METADATA FIX (2026-07-26): [`ClassLoaderHierarchy`] is a
    /// separate, unlinked type whose `parents` map is only ever pruned through
    /// the manual [`ClassLoaderHierarchy::remove`] / [`ClassLoaderHierarchy::remove_all`].
    /// Without the unloaded addresses in the result, a caller holding both
    /// types had no way to keep the hierarchy in step, so `parents` grew
    /// without bound. `unloaded_loader_names` is not usable for this — loader
    /// names are not unique.
    pub unloaded_loader_addrs: Vec<usize>,
}

/// A JIT code cache entry associated with a compiled method.
#[derive(Debug, Clone)]
pub struct CodeCacheEntry {
    /// Address/identity of the ClassLoader that defined the owning class.
    ///
    /// Class IDs are only unique *within* a loader and can be reused across
    /// loaders, so invalidation must key on the `(loader_addr, class_id)` pair
    /// rather than `class_id` alone (bug gc-classunload). `None` means the
    /// owning loader was not known at registration time (e.g. the class was not
    /// yet registered); such entries fall back to matching on `class_id` alone.
    pub loader_addr: Option<usize>,
    /// Class that this JIT code belongs to.
    pub class_id: u32,
    /// Method name.
    pub method_name: String,
    /// Size of compiled code in bytes.
    pub code_size: usize,
    /// Whether this entry is still valid.
    pub valid: bool,
}

/// Cumulative statistics across all unloading cycles.
#[derive(Debug, Default, Clone)]
pub struct ClassUnloadingStats {
    pub total_unload_cycles: u64,
    pub total_loaders_unloaded: u64,
    pub total_classes_unloaded: u64,
    pub total_metadata_reclaimed: u64,
    pub total_jit_invalidated: u64,
}

// ---------------------------------------------------------------------------
// ClassUnloader
// ---------------------------------------------------------------------------

/// Orchestrates class unloading during GC.
///
/// Thread safety: All mutable state is behind a `Mutex`, allowing the
/// `ClassUnloader` to be safely shared across threads (e.g., between the
/// GC coordinator and marker threads). The lock is held for the duration of
/// each public method call. During STW GC pauses, contention is minimal
/// since application threads are suspended.
pub struct ClassUnloader {
    /// All mutable state behind a Mutex for thread safety.
    inner: Mutex<ClassUnloaderInner>,
}

/// Inner mutable state for [`ClassUnloader`], protected by a `Mutex`.
struct ClassUnloaderInner {
    /// All known class loaders.
    loaders: Vec<ClassLoaderData>,
    /// JIT code cache entries (for invalidation).
    code_cache: Vec<CodeCacheEntry>,
    /// Total metadata bytes across all loaders.
    total_metadata_bytes: usize,
    /// Total classes loaded.
    total_classes: usize,
    /// Unloading enabled flag.
    enabled: bool,
    /// Statistics.
    stats: ClassUnloadingStats,
}

impl ClassUnloader {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ClassUnloaderInner {
                loaders: Vec::new(),
                code_cache: Vec::new(),
                total_metadata_bytes: 0,
                total_classes: 0,
                enabled: true,
                stats: ClassUnloadingStats::default(),
            }),
        }
    }

    /// Register a new class loader.
    pub fn register_loader(&self, loader_addr: usize, name: &str, is_system: bool) {
        self.inner.lock().loaders.push(ClassLoaderData {
            loader_addr,
            name: name.to_string(),
            loaded_classes: Vec::new(),
            metadata_bytes: 0,
            is_system_loader: is_system,
            alive: false,
        });
    }

    /// Register a class loaded by a specific loader.
    pub fn register_class(&self, loader_addr: usize, class_info: ClassInfo) {
        let mut inner = self.inner.lock();
        if let Some(loader) = inner
            .loaders
            .iter_mut()
            .find(|l| l.loader_addr == loader_addr)
        {
            loader.metadata_bytes += class_info.metadata_bytes;
            let meta_bytes = class_info.metadata_bytes;
            loader.loaded_classes.push(class_info);
            inner.total_metadata_bytes += meta_bytes;
            inner.total_classes += 1;
        }
    }

    /// Register JIT compiled code for a class.
    ///
    /// The owning loader is resolved from the registered classes so that the
    /// resulting code-cache entry is keyed on the `(loader_addr, class_id)`
    /// pair: class IDs may be reused across loaders, so invalidation must not
    /// match on `class_id` alone (bug gc-classunload). If no registered class
    /// currently owns this `class_id` the entry stores `loader_addr = None` and
    /// later falls back to matching on `class_id` alone.
    pub fn register_jit_code(&self, class_id: u32, method_name: &str, code_size: usize) {
        let mut inner = self.inner.lock();
        // Resolve the owning loader and mark the class as having JIT code.
        let mut owner_loader: Option<usize> = None;
        for loader in &mut inner.loaders {
            for cls in &mut loader.loaded_classes {
                if cls.class_id == class_id {
                    cls.has_jit_code = true;
                    owner_loader = Some(loader.loader_addr);
                }
            }
        }
        inner.code_cache.push(CodeCacheEntry {
            loader_addr: owner_loader,
            class_id,
            method_name: method_name.to_string(),
            code_size,
            valid: true,
        });
    }

    /// Mark a class loader as alive (called during GC marking phase).
    pub fn mark_loader_alive(&self, loader_addr: usize) {
        let mut inner = self.inner.lock();
        if let Some(loader) = inner
            .loaders
            .iter_mut()
            .find(|l| l.loader_addr == loader_addr)
        {
            loader.alive = true;
        }
    }

    /// Perform class unloading after GC marking.
    ///
    /// `is_loader_reachable` checks if the ClassLoader object is still reachable
    /// on the heap. Returns which classes/loaders were unloaded.
    pub fn unload_classes(
        &self,
        is_loader_reachable: &dyn Fn(usize) -> bool,
    ) -> ClassUnloadingResult {
        let mut inner = self.inner.lock();

        if !inner.enabled {
            return ClassUnloadingResult::default();
        }

        // Phase 1 -- find unreachable loaders.
        //
        // Perf (gc-classunload): collect the unreachable loader addresses into a
        // `HashSet` once so that every later membership test
        // (`unreachable.contains(&addr)`) in Phases 2/4, the name collections and
        // the final `retain` is O(1) instead of an O(loaders) linear scan over a
        // `Vec`. The previous code re-scanned a `Vec<usize>` multiple times,
        // giving O(loaders * unreachable) total work per phase. The membership
        // set unloaded is exactly the same as before.
        let unreachable: std::collections::HashSet<usize> = inner
            .loaders
            .iter()
            .filter(|l| !l.is_system_loader && !l.alive && !is_loader_reachable(l.loader_addr))
            .map(|l| l.loader_addr)
            .collect();

        if unreachable.is_empty() {
            inner.stats.total_unload_cycles += 1;
            return ClassUnloadingResult::default();
        }

        // Phase 2 -- collect classes to unload.
        let classes: Vec<ClassInfo> = inner
            .loaders
            .iter()
            .filter(|l| unreachable.contains(&l.loader_addr))
            .flat_map(|l| l.loaded_classes.clone())
            .collect();

        // Phase 3 -- invalidate JIT code.
        //
        // Key invalidation on the `(loader_addr, class_id)` pair rather than on
        // `class_id` alone (bug gc-classunload): class IDs are only unique
        // within a loader and can be reused across loaders, so a reused id under
        // a *different* (still-reachable) loader must not be invalidated when an
        // unreachable loader happens to share that id. Entries whose owning
        // loader was unknown at registration time (`loader_addr == None`) fall
        // back to matching on `class_id` alone, scoped to the unloaded classes.
        let unloaded_pairs: std::collections::HashSet<(usize, u32)> = inner
            .loaders
            .iter()
            .filter(|l| unreachable.contains(&l.loader_addr))
            .flat_map(|l| {
                let loader_addr = l.loader_addr;
                l.loaded_classes
                    .iter()
                    .map(move |c| (loader_addr, c.class_id))
            })
            .collect();
        let unloaded_class_ids: std::collections::HashSet<u32> =
            unloaded_pairs.iter().map(|&(_, id)| id).collect();
        // BOUNDED-METADATA FIX (2026-07-26): this loop used to only flip
        // `entry.valid = false` and leave the `CodeCacheEntry` — including its
        // owned `method_name: String` — in `code_cache` forever. `code_cache`
        // is otherwise append-only (`register_jit_code` only ever pushes), so
        // the vector grew without bound across unload cycles: exactly the
        // "long-running app with dynamic proxies dies after hours" failure
        // mode, and a direct violation of the "unload invalidation or hard
        // bound" rule in
        // `docs/architecture/class-loader-unloading.md`.
        //
        // Retiring an entry is now a REMOVAL, not a flag flip. The `valid`
        // field is kept because a caller may hold an entry snapshot, but no
        // retired entry survives the transaction that retired it. This also
        // keeps the O(code_cache) scan here — and the O(loaders * classes) scan
        // in `register_jit_code` — proportional to *live* compiled methods
        // rather than to every method ever compiled.
        let mut jit_invalidated = 0;
        inner.code_cache.retain_mut(|entry| {
            if !entry.valid {
                // Already retired by an earlier path; drop it now.
                return false;
            }
            let should_invalidate = match entry.loader_addr {
                // Owning loader known: only invalidate the exact pair, so a
                // reused class_id under a different loader is left untouched.
                Some(addr) => unloaded_pairs.contains(&(addr, entry.class_id)),
                // Owning loader unknown: fall back to class_id matching, scoped
                // to the classes actually being unloaded this cycle.
                None => unloaded_class_ids.contains(&entry.class_id),
            };
            if should_invalidate {
                entry.valid = false;
                jit_invalidated += 1;
                false // drop: the owning class is gone
            } else {
                true
            }
        });

        // Phase 4 -- reclaim metadata.
        let mut metadata_reclaimed = 0;
        for loader in &inner.loaders {
            if unreachable.contains(&loader.loader_addr) {
                metadata_reclaimed += loader.metadata_bytes;
            }
        }
        // `saturating_sub`: a plain `-=` here underflows (and panics in debug)
        // if the accounting ever drifts — e.g. a class registered against an
        // unknown loader bumps neither total, but a later re-registration
        // could. Accounting drift must not take down the collector.
        inner.total_metadata_bytes = inner
            .total_metadata_bytes
            .saturating_sub(metadata_reclaimed);

        // Build result.
        let unloaded_class_names: Vec<String> = classes.iter().map(|c| c.name.clone()).collect();
        let unloaded_loader_names: Vec<String> = inner
            .loaders
            .iter()
            .filter(|l| unreachable.contains(&l.loader_addr))
            .map(|l| l.name.clone())
            .collect();
        let unloaded_loader_addrs: Vec<usize> = inner
            .loaders
            .iter()
            .map(|l| l.loader_addr)
            .filter(|a| unreachable.contains(a))
            .collect();
        let loaders_unloaded = unreachable.len();
        let classes_unloaded = classes.len();

        // Remove the unloaded loaders.
        inner
            .loaders
            .retain(|l| !unreachable.contains(&l.loader_addr));

        // Update totals. `saturating_sub` for the same reason as
        // `total_metadata_bytes` above.
        inner.total_classes = inner.total_classes.saturating_sub(classes_unloaded);

        // Update stats.
        inner.stats.total_unload_cycles += 1;
        inner.stats.total_loaders_unloaded += loaders_unloaded as u64;
        inner.stats.total_classes_unloaded += classes_unloaded as u64;
        inner.stats.total_metadata_reclaimed += metadata_reclaimed as u64;
        inner.stats.total_jit_invalidated += jit_invalidated as u64;

        ClassUnloadingResult {
            loaders_unloaded,
            classes_unloaded,
            metadata_bytes_reclaimed: metadata_reclaimed,
            jit_entries_invalidated: jit_invalidated,
            unloaded_class_names,
            unloaded_loader_names,
            unloaded_loader_addrs,
        }
    }

    /// Update loader addresses after GC relocation.
    pub fn update_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        let mut inner = self.inner.lock();
        for loader in &mut inner.loaders {
            if let Some(&new_addr) = pointer_map.get(&loader.loader_addr) {
                loader.loader_addr = new_addr;
            }
        }
        // Remap the owning-loader key stored on each code-cache entry in lockstep
        // (bug gc-classunload): the `(loader_addr, class_id)` pair is the
        // invalidation key, so a stale loader address here would make a later
        // unload silently fail to invalidate the entry (a false negative).
        for entry in &mut inner.code_cache {
            if let Some(addr) = entry.loader_addr {
                if let Some(&new_addr) = pointer_map.get(&addr) {
                    entry.loader_addr = Some(new_addr);
                }
            }
        }
    }

    /// Reset alive marks for next GC cycle.
    pub fn reset_alive_marks(&self) {
        let mut inner = self.inner.lock();
        for loader in &mut inner.loaders {
            loader.alive = false;
        }
    }

    /// Get a snapshot of a loader's data by address.
    pub fn get_loader(&self, addr: usize) -> Option<ClassLoaderData> {
        let inner = self.inner.lock();
        inner
            .loaders
            .iter()
            .find(|l| l.loader_addr == addr)
            .cloned()
    }

    /// Get all classes loaded by a specific loader.
    pub fn classes_for_loader(&self, loader_addr: usize) -> Vec<ClassInfo> {
        let inner = self.inner.lock();
        inner
            .loaders
            .iter()
            .find(|l| l.loader_addr == loader_addr)
            .map(|l| l.loaded_classes.clone())
            .unwrap_or_default()
    }

    /// Total metadata bytes in use.
    pub fn total_metadata_bytes(&self) -> usize {
        self.inner.lock().total_metadata_bytes
    }

    /// Total number of loaded classes.
    pub fn total_classes(&self) -> usize {
        self.inner.lock().total_classes
    }

    /// Number of class loaders.
    pub fn loader_count(&self) -> usize {
        self.inner.lock().loaders.len()
    }

    /// Enable/disable class unloading.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.lock().enabled = enabled;
    }

    /// Check if enabled.
    pub fn is_enabled(&self) -> bool {
        self.inner.lock().enabled
    }

    /// Get cumulative statistics (returns a clone).
    pub fn stats(&self) -> ClassUnloadingStats {
        self.inner.lock().stats.clone()
    }

    /// Reset statistics.
    pub fn reset_stats(&self) {
        self.inner.lock().stats = ClassUnloadingStats::default();
    }

    /// Access to the code cache for assertions in tests.
    #[cfg(test)]
    fn code_cache_snapshot(&self) -> Vec<CodeCacheEntry> {
        self.inner.lock().code_cache.clone()
    }
}

impl Default for ClassUnloader {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ClassLoaderHierarchy
// ---------------------------------------------------------------------------

/// Tracks parent-delegation relationships among class loaders.
pub struct ClassLoaderHierarchy {
    /// Parent loader address for each loader (`None` = bootstrap).
    parents: HashMap<usize, Option<usize>>,
}

impl ClassLoaderHierarchy {
    pub fn new() -> Self {
        Self {
            parents: HashMap::new(),
        }
    }

    /// Register a loader with its parent.
    pub fn register(&mut self, loader_addr: usize, parent_addr: Option<usize>) {
        self.parents.insert(loader_addr, parent_addr);
    }

    /// Get the parent of a loader.
    pub fn parent_of(&self, loader_addr: usize) -> Option<Option<usize>> {
        self.parents.get(&loader_addr).copied()
    }

    /// Check if `ancestor` is an ancestor of `descendant`.
    ///
    /// Cycle protection (bug gc-classunload): a malformed parent chain
    /// (e.g. `A -> B -> A`) previously spun this loop forever and hung the GC
    /// thread. We now bound the walk by the number of registered loaders and
    /// track visited nodes; on a revisit (or once we exceed the loader count)
    /// we bail out to `false` rather than looping. A correct, acyclic chain can
    /// never be longer than `parents.len()`, so this never rejects a valid
    /// ancestor relationship.
    pub fn is_ancestor(&self, ancestor: usize, descendant: usize) -> bool {
        let mut current = descendant;
        let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
        // Hard upper bound on iterations as a second line of defence in case a
        // future change drops the visited-set guard.
        let max_iterations = self.parents.len() + 1;
        for _ in 0..max_iterations {
            match self.parents.get(&current) {
                Some(Some(parent)) => {
                    if *parent == ancestor {
                        return true;
                    }
                    // Revisiting a node we've already seen means the chain is
                    // cyclic; stop instead of spinning forever.
                    if !visited.insert(current) {
                        return false;
                    }
                    current = *parent;
                }
                _ => return false,
            }
        }
        // Exceeded the loader-count bound -> chain is cyclic/malformed.
        false
    }

    /// Get all loaders in the hierarchy.
    pub fn all_loaders(&self) -> Vec<usize> {
        self.parents.keys().copied().collect()
    }

    /// Remove a loader from the hierarchy.
    pub fn remove(&mut self, loader_addr: usize) {
        self.parents.remove(&loader_addr);
    }

    /// Remove every loader in `loader_addrs` from the hierarchy.
    ///
    /// BOUNDED-METADATA FIX (2026-07-26): pair this with
    /// [`ClassUnloadingResult::unloaded_loader_addrs`] after every
    /// [`ClassUnloader::unload_classes`]. `parents` is otherwise append-only
    /// (`register` is the only writer besides `update_after_gc`, which
    /// re-inserts what it drained), so a caller that unloads loaders without
    /// pruning here leaks one map entry per loader for the process lifetime —
    /// and every leaked entry also lengthens the `is_ancestor` iteration bound.
    ///
    /// Note this deliberately does NOT re-parent orphaned children: a child of
    /// an unloaded loader is itself unreachable (a loader strongly references
    /// its parent, never the reverse), so it is unloaded in the same
    /// transaction and appears in the same address list.
    pub fn remove_all(&mut self, loader_addrs: &[usize]) {
        for addr in loader_addrs {
            self.parents.remove(addr);
        }
    }

    /// Number of registered loaders. Exposed so callers (and tests) can assert
    /// the map actually shrinks on unload.
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    /// Whether the hierarchy is empty.
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// Update addresses after GC.
    ///
    /// Collision rejection (bug gc-classunload): the loader address and its
    /// parent address are remapped independently. A pathological/contended
    /// `pointer_map` can map two distinct old loader addresses onto the same
    /// new address, which would collapse two nodes into one and could make the
    /// hierarchy cyclic (e.g. `A -> B` and `B -> A` both landing on the same
    /// key), which in turn would hang `is_ancestor`. To guarantee the hierarchy
    /// stays acyclic, we drop (do not re-insert) any entry whose remapped
    /// loader address would collide with an entry already inserted this pass,
    /// and we reject any self-referential parent (`new_addr == new_parent`)
    /// produced by remapping.
    pub fn update_after_gc(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        let old: Vec<(usize, Option<usize>)> = self.parents.drain().collect();
        for (addr, parent) in old {
            let new_addr = pointer_map.get(&addr).copied().unwrap_or(addr);
            let new_parent = parent.map(|p| pointer_map.get(&p).copied().unwrap_or(p));

            // Skip an entry whose remapped loader address collides with one we
            // already re-inserted: re-inserting would silently overwrite the
            // earlier node and risk fusing two chains into a cycle.
            if self.parents.contains_key(&new_addr) {
                continue;
            }

            // Reject a parent edge that became self-referential after remapping
            // (a one-node cycle), which would hang `is_ancestor`. Treat such a
            // loader as having no parent rather than pointing at itself.
            let new_parent = match new_parent {
                Some(p) if p == new_addr => None,
                other => other,
            };

            self.parents.insert(new_addr, new_parent);
        }
    }
}

impl Default for ClassLoaderHierarchy {
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

    fn make_class(name: &str, id: u32, bytes: usize) -> ClassInfo {
        ClassInfo {
            name: name.to_string(),
            class_id: id,
            metadata_bytes: bytes,
            has_jit_code: false,
            instance_count: 0,
        }
    }

    // -- basic registration ------------------------------------------------

    #[test]
    fn register_loader_and_verify() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "AppLoader", false);
        let loader = u
            .get_loader(0x100)
            .expect("class_unloading: expected loader at 0x100");
        assert_eq!(loader.name, "AppLoader");
        assert!(!loader.is_system_loader);
        assert_eq!(u.loader_count(), 1);
    }

    #[test]
    fn register_class_and_verify_metadata() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "AppLoader", false);
        u.register_class(0x100, make_class("com/example/Foo", 1, 512));
        assert_eq!(u.total_classes(), 1);
        assert_eq!(u.total_metadata_bytes(), 512);
        let loader = u
            .get_loader(0x100)
            .expect("class_unloading: expected loader at 0x100");
        assert_eq!(loader.metadata_bytes, 512);
        assert_eq!(loader.loaded_classes.len(), 1);
    }

    #[test]
    fn multiple_classes_per_loader() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "Loader", false);
        u.register_class(0x100, make_class("A", 1, 100));
        u.register_class(0x100, make_class("B", 2, 200));
        u.register_class(0x100, make_class("C", 3, 300));
        assert_eq!(u.total_classes(), 3);
        assert_eq!(u.total_metadata_bytes(), 600);
        assert_eq!(u.classes_for_loader(0x100).len(), 3);
    }

    #[test]
    fn class_info_metadata_estimation() {
        let ci = make_class("com/example/Big", 42, 8192);
        assert_eq!(ci.metadata_bytes, 8192);
        assert_eq!(ci.class_id, 42);
        assert!(!ci.has_jit_code);
    }

    // -- system loader protection ------------------------------------------

    #[test]
    fn system_loader_never_unloaded() {
        let u = ClassUnloader::new();
        u.register_loader(0x10, "bootstrap", true);
        u.register_class(0x10, make_class("java/lang/Object", 1, 1024));
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.loaders_unloaded, 0);
        assert_eq!(result.classes_unloaded, 0);
        assert_eq!(u.loader_count(), 1);
    }

    // -- basic unloading ---------------------------------------------------

    #[test]
    fn unreachable_non_system_loader_is_unloaded() {
        let u = ClassUnloader::new();
        u.register_loader(0x200, "PluginLoader", false);
        u.register_class(0x200, make_class("plugin/Main", 10, 256));
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.loaders_unloaded, 1);
        assert_eq!(result.classes_unloaded, 1);
        assert_eq!(result.metadata_bytes_reclaimed, 256);
        assert_eq!(u.loader_count(), 0);
    }

    #[test]
    fn classes_of_unreachable_loader_are_unloaded() {
        let u = ClassUnloader::new();
        u.register_loader(0x300, "DynLoader", false);
        u.register_class(0x300, make_class("dyn/A", 1, 100));
        u.register_class(0x300, make_class("dyn/B", 2, 200));
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.classes_unloaded, 2);
        assert!(result.unloaded_class_names.contains(&"dyn/A".to_string()));
        assert!(result.unloaded_class_names.contains(&"dyn/B".to_string()));
    }

    #[test]
    fn metadata_bytes_reclaimed_correctly() {
        let u = ClassUnloader::new();
        u.register_loader(0x400, "L", false);
        u.register_class(0x400, make_class("X", 1, 1000));
        u.register_class(0x400, make_class("Y", 2, 2000));
        assert_eq!(u.total_metadata_bytes(), 3000);
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.metadata_bytes_reclaimed, 3000);
        assert_eq!(u.total_metadata_bytes(), 0);
    }

    // -- JIT invalidation --------------------------------------------------

    #[test]
    fn jit_code_invalidated_for_unloaded_classes() {
        let u = ClassUnloader::new();
        u.register_loader(0x500, "JitLoader", false);
        u.register_class(0x500, make_class("jit/Hot", 50, 128));
        u.register_jit_code(50, "run", 4096);
        u.register_jit_code(50, "init", 1024);
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.jit_entries_invalidated, 2);
        // BOUNDED-METADATA FIX (2026-07-26): retired entries are REMOVED, not
        // just flagged. Previously they stayed in `code_cache` forever.
        assert!(
            u.code_cache_snapshot().is_empty(),
            "retired code-cache entries must be released, not merely flagged"
        );
    }

    #[test]
    fn register_jit_code_and_verify() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "L", false);
        u.register_class(0x100, make_class("C", 7, 64));
        u.register_jit_code(7, "compute", 2048);
        let cache = u.code_cache_snapshot();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].code_size, 2048);
        assert!(cache[0].valid);
        // Class should now be flagged as having JIT code.
        let loader = u
            .get_loader(0x100)
            .expect("class_unloading: expected loader at 0x100");
        assert!(loader.loaded_classes[0].has_jit_code);
    }

    #[test]
    fn code_cache_invalidation_only_targets_specific_class() {
        let u = ClassUnloader::new();
        u.register_loader(0x10, "sys", true);
        u.register_class(0x10, make_class("java/lang/String", 1, 500));
        u.register_jit_code(1, "hashCode", 256);

        u.register_loader(0x600, "plugin", false);
        u.register_class(0x600, make_class("plugin/X", 2, 100));
        u.register_jit_code(2, "exec", 512);

        let result = u.unload_classes(&|_| false);
        // Only the plugin class JIT should be invalidated.
        assert_eq!(result.jit_entries_invalidated, 1);
        let cache = u.code_cache_snapshot();
        let string_entry = cache
            .iter()
            .find(|e| e.class_id == 1)
            .expect("class_unloading: expected code cache entry for class_id 1");
        assert!(string_entry.valid);
    }

    #[test]
    fn jit_invalidation_keyed_on_loader_class_pair() {
        // bug gc-classunload: two loaders reuse the same class_id (7). When the
        // first loader is unloaded, ONLY its JIT code must be invalidated; the
        // still-reachable second loader's code (same class_id) must survive.
        let u = ClassUnloader::new();

        u.register_loader(0xA00, "DeadLoader", false);
        u.register_class(0xA00, make_class("dead/Same", 7, 128));
        u.register_jit_code(7, "run", 4096);

        u.register_loader(0xB00, "LiveLoader", false);
        u.register_class(0xB00, make_class("live/Same", 7, 128));
        u.register_jit_code(7, "run", 4096);

        // 0xB00 stays reachable, 0xA00 does not.
        let result = u.unload_classes(&|addr| addr == 0xB00);
        assert_eq!(result.loaders_unloaded, 1);

        // Exactly one JIT entry (the dead loader's) is invalidated despite the
        // shared class_id.
        assert_eq!(result.jit_entries_invalidated, 1);
        let cache = u.code_cache_snapshot();
        // BOUNDED-METADATA FIX (2026-07-26): the dead loader's entry is
        // released outright rather than left behind with `valid == false`.
        assert!(
            !cache.iter().any(|e| e.loader_addr == Some(0xA00)),
            "dead loader's JIT code must be invalidated AND released"
        );
        let live = cache
            .iter()
            .find(|e| e.loader_addr == Some(0xB00))
            .expect("class_unloading: expected code cache entry for live loader 0xB00");
        assert!(
            live.valid,
            "live loader's JIT code must survive (reused class_id)"
        );
        assert_eq!(cache.len(), 1, "exactly the live entry remains");
    }

    #[test]
    fn jit_invalidation_remaps_loader_key_after_gc() {
        // bug gc-classunload: after relocation the code-cache loader key must be
        // remapped in lockstep with the loader, or a later unload silently fails
        // to invalidate the entry.
        let u = ClassUnloader::new();
        u.register_loader(0xA00, "Moved", false);
        u.register_class(0xA00, make_class("m/Hot", 3, 64));
        u.register_jit_code(3, "hot", 2048);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xA00, 0xC00);
        u.update_after_gc(&map);

        // The code-cache entry's owning-loader key followed the relocation.
        let cache = u.code_cache_snapshot();
        assert_eq!(cache[0].loader_addr, Some(0xC00));

        // Unloading the relocated loader still invalidates its JIT code.
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.jit_entries_invalidated, 1);
        assert!(u.code_cache_snapshot().iter().all(|e| !e.valid));
    }

    #[test]
    fn jit_invalidation_unknown_owner_falls_back_to_class_id() {
        // An entry registered before its class was registered has no known owner
        // (loader_addr == None) and must fall back to class_id matching, scoped
        // to the classes actually being unloaded.
        let u = ClassUnloader::new();
        // Register JIT code first -> owner unknown.
        u.register_jit_code(99, "orphan", 256);
        let cache = u.code_cache_snapshot();
        assert_eq!(cache[0].loader_addr, None);

        // A loader that owns class_id 99 becomes unreachable.
        u.register_loader(0xA00, "Owner", false);
        u.register_class(0xA00, make_class("own/C", 99, 64));
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.jit_entries_invalidated, 1);
    }

    // -- mixed reachable/unreachable loaders --------------------------------

    #[test]
    fn mixed_reachable_unreachable_loaders() {
        let u = ClassUnloader::new();
        u.register_loader(0x10, "bootstrap", true);
        u.register_class(0x10, make_class("java/lang/Object", 1, 1024));

        u.register_loader(0xA00, "ReachablePlugin", false);
        u.register_class(0xA00, make_class("rp/Main", 10, 128));

        u.register_loader(0xB00, "DeadPlugin", false);
        u.register_class(0xB00, make_class("dp/Main", 20, 256));

        // 0xA00 is reachable, 0xB00 is not.
        let result = u.unload_classes(&|addr| addr == 0xA00);
        assert_eq!(result.loaders_unloaded, 1);
        assert_eq!(result.classes_unloaded, 1);
        assert!(result
            .unloaded_loader_names
            .contains(&"DeadPlugin".to_string()));
        assert_eq!(u.loader_count(), 2); // bootstrap + ReachablePlugin
    }

    #[test]
    fn alive_mark_prevents_unloading() {
        let u = ClassUnloader::new();
        u.register_loader(0x700, "MarkedAlive", false);
        u.register_class(0x700, make_class("alive/C", 1, 64));
        u.mark_loader_alive(0x700);
        // Even though the reachability callback says false, alive mark keeps it.
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.loaders_unloaded, 0);
    }

    // -- empty / edge cases -------------------------------------------------

    #[test]
    fn empty_unloader_returns_empty_result() {
        let u = ClassUnloader::new();
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.loaders_unloaded, 0);
        assert_eq!(result.classes_unloaded, 0);
        assert_eq!(result.metadata_bytes_reclaimed, 0);
    }

    // -- enable/disable -----------------------------------------------------

    #[test]
    fn enable_disable_flag_respected() {
        let u = ClassUnloader::new();
        u.register_loader(0x800, "Dead", false);
        u.register_class(0x800, make_class("dead/C", 1, 128));
        u.set_enabled(false);
        assert!(!u.is_enabled());
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.loaders_unloaded, 0);
        // Re-enable and unload.
        u.set_enabled(true);
        assert!(u.is_enabled());
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.loaders_unloaded, 1);
    }

    // -- update_after_gc ---------------------------------------------------

    #[test]
    fn update_after_gc_remaps_addresses() {
        let u = ClassUnloader::new();
        u.register_loader(0x1000, "Moved", false);
        u.register_class(0x1000, make_class("m/C", 1, 64));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x2000);
        u.update_after_gc(&map);

        assert!(u.get_loader(0x1000).is_none());
        assert!(u.get_loader(0x2000).is_some());
        assert_eq!(
            u.get_loader(0x2000)
                .expect("class_unloading: expected loader at 0x2000")
                .name,
            "Moved"
        );
    }

    // -- reset_alive_marks -------------------------------------------------

    #[test]
    fn reset_alive_marks_clears_all() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "A", false);
        u.register_loader(0x200, "B", false);
        u.mark_loader_alive(0x100);
        u.mark_loader_alive(0x200);
        assert!(
            u.get_loader(0x100)
                .expect("class_unloading: expected loader at 0x100")
                .alive
        );
        assert!(
            u.get_loader(0x200)
                .expect("class_unloading: expected loader at 0x200")
                .alive
        );
        u.reset_alive_marks();
        assert!(
            !u.get_loader(0x100)
                .expect("class_unloading: expected loader at 0x100 after reset")
                .alive
        );
        assert!(
            !u.get_loader(0x200)
                .expect("class_unloading: expected loader at 0x200 after reset")
                .alive
        );
    }

    // -- statistics ---------------------------------------------------------

    #[test]
    fn stats_tracking_across_multiple_cycles() {
        let u = ClassUnloader::new();

        // Cycle 1: unload one loader with 1 class.
        u.register_loader(0x100, "L1", false);
        u.register_class(0x100, make_class("c1", 1, 100));
        u.unload_classes(&|_| false);

        // Cycle 2: unload another loader with 2 classes.
        u.register_loader(0x200, "L2", false);
        u.register_class(0x200, make_class("c2", 2, 200));
        u.register_class(0x200, make_class("c3", 3, 300));
        u.unload_classes(&|_| false);

        let s = u.stats();
        assert_eq!(s.total_unload_cycles, 2);
        assert_eq!(s.total_loaders_unloaded, 2);
        assert_eq!(s.total_classes_unloaded, 3);
        assert_eq!(s.total_metadata_reclaimed, 600);
    }

    #[test]
    fn reset_stats() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "L", false);
        u.register_class(0x100, make_class("c", 1, 64));
        u.unload_classes(&|_| false);
        assert_eq!(u.stats().total_unload_cycles, 1);
        u.reset_stats();
        assert_eq!(u.stats().total_unload_cycles, 0);
        assert_eq!(u.stats().total_loaders_unloaded, 0);
    }

    // -- loader/class count updates -----------------------------------------

    #[test]
    fn loader_count_updates_after_unloading() {
        let u = ClassUnloader::new();
        u.register_loader(0x10, "sys", true);
        u.register_loader(0x100, "dead", false);
        assert_eq!(u.loader_count(), 2);
        u.unload_classes(&|_| false);
        assert_eq!(u.loader_count(), 1);
    }

    #[test]
    fn total_classes_updates_after_unloading() {
        let u = ClassUnloader::new();
        u.register_loader(0x10, "sys", true);
        u.register_class(0x10, make_class("java/lang/Object", 1, 1024));
        u.register_loader(0x100, "dead", false);
        u.register_class(0x100, make_class("dead/C", 2, 128));
        assert_eq!(u.total_classes(), 2);
        u.unload_classes(&|_| false);
        assert_eq!(u.total_classes(), 1);
    }

    #[test]
    fn total_metadata_updates_after_unloading() {
        let u = ClassUnloader::new();
        u.register_loader(0x10, "sys", true);
        u.register_class(0x10, make_class("java/lang/Object", 1, 1000));
        u.register_loader(0x100, "dead", false);
        u.register_class(0x100, make_class("dead/C", 2, 500));
        assert_eq!(u.total_metadata_bytes(), 1500);
        u.unload_classes(&|_| false);
        assert_eq!(u.total_metadata_bytes(), 1000);
    }

    // -- unloaded names tracked --------------------------------------------

    #[test]
    fn unloaded_class_names_tracked_in_result() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "MyLoader", false);
        u.register_class(0x100, make_class("pkg/Alpha", 1, 64));
        u.register_class(0x100, make_class("pkg/Beta", 2, 64));
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.unloaded_class_names.len(), 2);
        assert!(result
            .unloaded_class_names
            .contains(&"pkg/Alpha".to_string()));
        assert!(result
            .unloaded_class_names
            .contains(&"pkg/Beta".to_string()));
        assert!(result
            .unloaded_loader_names
            .contains(&"MyLoader".to_string()));
    }

    // -- classes_for_loader -------------------------------------------------

    #[test]
    fn classes_for_loader_returns_correct_set() {
        let u = ClassUnloader::new();
        u.register_loader(0x100, "L1", false);
        u.register_loader(0x200, "L2", false);
        u.register_class(0x100, make_class("a/X", 1, 10));
        u.register_class(0x200, make_class("b/Y", 2, 20));
        let classes = u.classes_for_loader(0x100);
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "a/X");
    }

    #[test]
    fn classes_for_unknown_loader_returns_empty() {
        let u = ClassUnloader::new();
        assert!(u.classes_for_loader(0xDEAD).is_empty());
    }

    // -- ClassLoaderHierarchy tests ----------------------------------------

    #[test]
    fn hierarchy_parent_tracking() {
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x100, None); // bootstrap child
        h.register(0x200, Some(0x100));
        assert_eq!(h.parent_of(0x100), Some(None));
        assert_eq!(h.parent_of(0x200), Some(Some(0x100)));
        assert_eq!(h.parent_of(0x999), None);
    }

    #[test]
    fn hierarchy_is_ancestor() {
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x10, None); // root
        h.register(0x20, Some(0x10));
        h.register(0x30, Some(0x20));
        assert!(h.is_ancestor(0x10, 0x30));
        assert!(h.is_ancestor(0x20, 0x30));
        assert!(!h.is_ancestor(0x30, 0x10));
        assert!(!h.is_ancestor(0x10, 0x10)); // not ancestor of itself
    }

    #[test]
    fn hierarchy_is_ancestor_returns_false_for_unrelated() {
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x10, None);
        h.register(0x20, None);
        assert!(!h.is_ancestor(0x10, 0x20));
    }

    #[test]
    fn hierarchy_all_loaders() {
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x10, None);
        h.register(0x20, Some(0x10));
        let all = h.all_loaders();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&0x10));
        assert!(all.contains(&0x20));
    }

    #[test]
    fn hierarchy_removal_on_unload() {
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x10, None);
        h.register(0x20, Some(0x10));
        h.remove(0x20);
        assert_eq!(h.all_loaders().len(), 1);
        assert!(h.parent_of(0x20).is_none());
    }

    #[test]
    fn hierarchy_update_after_gc() {
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x100, None);
        h.register(0x200, Some(0x100));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x100, 0xA00);
        map.insert(0x200, 0xB00);
        h.update_after_gc(&map);

        assert!(h.parent_of(0xB00).is_some());
        assert_eq!(h.parent_of(0xB00), Some(Some(0xA00)));
        assert!(h.parent_of(0x100).is_none()); // old address gone
    }

    // -- cycle protection (bug gc-classunload) -----------------------------

    #[test]
    fn hierarchy_is_ancestor_terminates_on_direct_cycle() {
        // A -> B -> A is a 2-node cycle; is_ancestor must return without
        // spinning forever and must not falsely report an ancestor.
        let mut h = ClassLoaderHierarchy::new();
        h.register(0xA, Some(0xB));
        h.register(0xB, Some(0xA));
        assert!(!h.is_ancestor(0xC, 0xA));
        assert!(!h.is_ancestor(0xC, 0xB));
        // The two nodes are mutual parents; querying their real relationship
        // still terminates.
        assert!(h.is_ancestor(0xB, 0xA));
        assert!(h.is_ancestor(0xA, 0xB));
    }

    #[test]
    fn hierarchy_is_ancestor_terminates_on_self_cycle() {
        // A node that is its own parent must not hang the walk.
        let mut h = ClassLoaderHierarchy::new();
        h.register(0xA, Some(0xA));
        assert!(h.is_ancestor(0xA, 0xA)); // immediate parent match
        assert!(!h.is_ancestor(0xB, 0xA)); // unrelated, must terminate
    }

    #[test]
    fn hierarchy_is_ancestor_terminates_on_longer_cycle() {
        // A -> B -> C -> A: a 3-node cycle with an unrelated query target.
        let mut h = ClassLoaderHierarchy::new();
        h.register(0xA, Some(0xB));
        h.register(0xB, Some(0xC));
        h.register(0xC, Some(0xA));
        assert!(!h.is_ancestor(0xD, 0xA));
    }

    #[test]
    fn update_after_gc_rejects_colliding_remap() {
        // Two distinct loaders remap onto the same new address. The collision
        // must be dropped (not fused) so the hierarchy cannot become cyclic.
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x100, Some(0x200));
        h.register(0x200, Some(0x100));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x100, 0x900);
        map.insert(0x200, 0x900); // collision onto the same new address

        h.update_after_gc(&map);

        // Exactly one node survives at the collided address; no infinite loop.
        assert_eq!(h.all_loaders().len(), 1);
        assert!(h.all_loaders().contains(&0x900));
        // Whatever survived, is_ancestor must terminate.
        assert!(!h.is_ancestor(0xDEAD, 0x900));
    }

    #[test]
    fn update_after_gc_rejects_self_referential_remap() {
        // A loader whose parent remaps onto its own new address would become a
        // one-node cycle; the parent edge must be cleared to None.
        let mut h = ClassLoaderHierarchy::new();
        h.register(0x100, Some(0x200));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x100, 0x500);
        map.insert(0x200, 0x500); // parent now points at the loader itself

        h.update_after_gc(&map);

        assert_eq!(h.parent_of(0x500), Some(None));
        assert!(!h.is_ancestor(0x500, 0x500));
    }

    // -- Default impls -----------------------------------------------------

    #[test]
    fn default_impls() {
        let u = ClassUnloader::default();
        assert!(u.is_enabled());
        assert_eq!(u.loader_count(), 0);

        let h = ClassLoaderHierarchy::default();
        assert!(h.all_loaders().is_empty());
    }

    // ======================================================================
    // BOUNDED-METADATA FIX (2026-07-26): "unload releases everything".
    //
    // These pin the invariant stated in
    // `docs/architecture/class-loader-unloading.md`: every
    // process-lifetime cache must have unload invalidation or a hard bound.
    // Two tables here previously had neither.
    // ======================================================================

    // Repeated define-heat-drop cycles — the shape of the checked-in
    // `class_loader_unload` probe, and of any app using CGLIB/ByteBuddy
    // proxies — must not grow ANY table. Before the fix `code_cache` grew by
    // one `CodeCacheEntry` (with an owned `String`) per compiled method per
    // cycle, forever.
    #[test]
    fn repeated_unload_cycles_leave_no_residue() {
        let u = ClassUnloader::new();
        let mut hierarchy = ClassLoaderHierarchy::new();
        u.register_loader(0x10, "bootstrap", true);
        u.register_class(0x10, make_class("java/lang/Object", 1, 1024));

        for cycle in 0..64usize {
            let addr = 0x1000 + cycle * 0x100;
            u.register_loader(addr, "ProxyLoader", false);
            hierarchy.register(addr, Some(0x10));
            u.register_class(
                addr,
                make_class("proxy/Generated", 1000 + cycle as u32, 512),
            );
            u.register_jit_code(1000 + cycle as u32, "invoke", 2048);
            u.register_jit_code(1000 + cycle as u32, "<init>", 512);

            // Only the bootstrap loader stays reachable.
            let result = u.unload_classes(&|a| a == 0x10);
            assert_eq!(result.loaders_unloaded, 1, "cycle {cycle}");
            assert_eq!(result.jit_entries_invalidated, 2, "cycle {cycle}");
            hierarchy.remove_all(&result.unloaded_loader_addrs);
        }

        // Steady state: only the bootstrap loader and its one class remain.
        assert_eq!(u.loader_count(), 1);
        assert_eq!(u.total_classes(), 1);
        assert_eq!(u.total_metadata_bytes(), 1024);
        assert!(
            u.code_cache_snapshot().is_empty(),
            "code_cache grew across unload cycles — the unbounded-metadata leak is back"
        );
        assert!(
            hierarchy.is_empty(),
            "hierarchy grew across unload cycles: {} entries left",
            hierarchy.len()
        );
    }

    // The unloaded addresses must be reported, or a caller holding a
    // `ClassLoaderHierarchy` cannot keep it in step (loader NAMES are not
    // unique, so `unloaded_loader_names` is not a substitute).
    #[test]
    fn unload_result_reports_loader_addresses() {
        let u = ClassUnloader::new();
        u.register_loader(0xA00, "Plugin", false);
        u.register_loader(0xB00, "Plugin", false); // same name, different loader
        u.register_class(0xA00, make_class("a/C", 1, 64));
        u.register_class(0xB00, make_class("b/C", 2, 64));

        let result = u.unload_classes(&|a| a == 0xB00);
        assert_eq!(result.unloaded_loader_addrs, vec![0xA00]);
        assert_eq!(result.unloaded_loader_names, vec!["Plugin".to_string()]);
    }

    // Retiring one loader's code must leave the surviving loader's entry
    // untouched, and a second cycle with nothing new to unload must neither
    // resurrect nor re-count anything.
    #[test]
    fn retired_entries_do_not_linger_or_recount() {
        let u = ClassUnloader::new();
        u.register_loader(0xA00, "Dead", false);
        u.register_class(0xA00, make_class("d/C", 1, 64));
        u.register_jit_code(1, "m", 128);
        u.register_loader(0xB00, "Live", false);
        u.register_class(0xB00, make_class("l/C", 2, 64));
        u.register_jit_code(2, "m", 128);

        let first = u.unload_classes(&|a| a == 0xB00);
        assert_eq!(first.jit_entries_invalidated, 1);
        assert_eq!(u.code_cache_snapshot().len(), 1);

        // A second cycle with nothing new to unload must not resurrect or
        // re-count anything.
        let second = u.unload_classes(&|a| a == 0xB00);
        assert_eq!(second.jit_entries_invalidated, 0);
        assert_eq!(u.code_cache_snapshot().len(), 1);
        assert!(u.code_cache_snapshot()[0].valid);
    }

    // Accounting must not underflow when a class is registered against an
    // unknown loader (the registration is dropped, so the totals never rose).
    #[test]
    fn accounting_does_not_underflow_on_unknown_loader() {
        let u = ClassUnloader::new();
        u.register_class(0xDEAD, make_class("ghost/C", 1, 4096)); // no such loader
        assert_eq!(u.total_classes(), 0);
        assert_eq!(u.total_metadata_bytes(), 0);
        u.register_loader(0xA00, "L", false);
        u.register_class(0xA00, make_class("a/C", 2, 64));
        let result = u.unload_classes(&|_| false);
        assert_eq!(result.classes_unloaded, 1);
        assert_eq!(u.total_classes(), 0);
        assert_eq!(u.total_metadata_bytes(), 0);
    }
}
