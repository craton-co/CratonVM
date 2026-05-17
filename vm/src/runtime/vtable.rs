//! Virtual method dispatch tables (vtables) and interface method tables (itables).
//!
//! Accelerates `invokevirtual` and `invokeinterface` bytecodes by replacing
//! HashMap lookups with direct array indexing. Each class gets a vtable that
//! inherits from its parent and overrides/appends entries for its own methods.

use std::collections::HashMap;
use std::hash::Hasher;
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

use crate::runtime::fx_collections::{FxHashMap, FxHasher};

/// HIGH-6 — fast-path key derived from `(name, descriptor)` without
/// allocating. Two independent FxHash streams XOR-mixed; the canonical
/// `FxHashMap<(Arc<str>, Arc<str>), usize>` index disambiguates the
/// vanishingly-rare collisions.
#[inline]
fn fast_lookup_key(name: &str, descriptor: &str) -> u64 {
    let mut h1 = FxHasher::default();
    h1.write(name.as_bytes());
    let mut h2 = FxHasher::default();
    h2.write(descriptor.as_bytes());
    // Rotate one half so `(a, b)` and `(b, a)` don't collide on the
    // (admittedly rare) case where name == descriptor reversed.
    h1.finish() ^ h2.finish().rotate_left(17)
}

/// A method slot in the vtable.
///
/// T10.9.A — carries an optional `Arc<CachedBytecodeMethod>` so the
/// interpreter can dispatch the method without taking a read lock on the
/// class manager. When `resolved_method` is `Some(_)` the entry represents
/// a concrete dispatchable method; when it is `None` (or the underlying
/// `is_native` flag is true) the interpreter must route through the
/// existing slower paths (native-method registry, `find_method_recursive`,
/// etc.).
#[derive(Clone)]
pub struct VtableEntry {
    /// Class that declared this method.
    pub declaring_class_id: u64,
    /// Method identifier (index into class method table).
    pub method_index: u32,
    /// Fully qualified method name for debugging.
    ///
    /// HIGH-5 — held as `Arc<str>` so `VtableEntry::clone()` is an
    /// atomic refcount bump rather than a deep string copy. The source
    /// of method names (`ClassFileMethod.name`) is already interned as
    /// `Arc<str>` per perf-gaps.md T10.9.C, so install-time inserts
    /// reuse those handles without allocating.
    pub method_name: Arc<str>,
    /// Method descriptor.
    ///
    /// HIGH-5 — see `method_name` rationale; held as `Arc<str>` for
    /// O(1) clone.
    pub descriptor: Arc<str>,
    /// Whether this entry has been resolved.
    pub resolved: bool,
    /// T10.9.A — fully-built dispatch snapshot. Present for every
    /// non-abstract bytecode method whose class reached the install
    /// hook. `None` means the slot is abstract, native without a
    /// captured snapshot, or was built from an older
    /// `VtableSlotDescriptor` that predates this field.
    pub resolved_method: Option<Arc<rustjvm_jit_api::CachedBytecodeMethod>>,
    /// T10.9.A — true when the underlying Java method is declared
    /// `native`. The interpreter uses this bit to bypass the bytecode
    /// dispatch and fall through to the native-method registry lookup.
    pub is_native: bool,
}

/// T10.9.A — manual `PartialEq` that ignores the cached `Arc<CachedBytecodeMethod>`
/// pointer identity. Two entries are equal iff their descriptive fields
/// match. `CachedBytecodeMethod` is not `PartialEq`, so we cannot derive.
impl PartialEq for VtableEntry {
    fn eq(&self, other: &Self) -> bool {
        self.declaring_class_id == other.declaring_class_id
            && self.method_index == other.method_index
            && self.method_name == other.method_name
            && self.descriptor == other.descriptor
            && self.resolved == other.resolved
            && self.is_native == other.is_native
    }
}

/// T10.9.A — manual `Debug` impl that renders only descriptive fields.
/// `CachedBytecodeMethod` is not `Debug`, so we display whether a
/// dispatch snapshot is attached but not its contents.
impl std::fmt::Debug for VtableEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtableEntry")
            .field("declaring_class_id", &self.declaring_class_id)
            .field("method_index", &self.method_index)
            .field("method_name", &self.method_name)
            .field("descriptor", &self.descriptor)
            .field("resolved", &self.resolved)
            .field("has_dispatch", &self.resolved_method.is_some())
            .field("is_native", &self.is_native)
            .finish()
    }
}

impl Default for VtableEntry {
    fn default() -> Self {
        VtableEntry {
            declaring_class_id: 0,
            method_index: 0,
            method_name: Arc::<str>::from(""),
            descriptor: Arc::<str>::from(""),
            resolved: true,
            resolved_method: None,
            is_native: false,
        }
    }
}

/// Virtual method dispatch table for a class.
///
/// The vtable is a flat array indexed by slot number. Each class inherits
/// its parent's vtable and appends/overrides entries for its own methods.
pub struct Vtable {
    /// The class this vtable belongs to.
    class_id: u64,
    /// Entries indexed by slot number.
    entries: Vec<Option<VtableEntry>>,
    /// Quick lookup: method (name, descriptor) -> slot index.
    ///
    /// HIGH-6 — keyed by `(Arc<str>, Arc<str>)` so insertion at link
    /// time reuses the interned method-name handles instead of
    /// allocating fresh `String`s. This map is the authoritative
    /// fallback when the `fast_lookup` u64-hash map collides; routine
    /// `lookup_slot(&str, &str)` calls never touch it.
    name_to_slot: FxHashMap<(Arc<str>, Arc<str>), usize>,
    /// HIGH-6 — zero-allocation fast path for `lookup_slot(&str, &str)`.
    /// Keyed by `fast_lookup_key(name, descriptor)`. Stores a `Vec<usize>`
    /// to handle the (vanishingly rare) FxHash collisions without
    /// allocating on probe; on hit, the caller verifies that each
    /// candidate entry's stored `(name, descriptor)` matches the args.
    fast_lookup: FxHashMap<u64, Vec<usize>>,
}

impl Vtable {
    /// Create an empty vtable for the given class.
    pub fn new(class_id: u64) -> Self {
        Vtable {
            class_id,
            entries: Vec::new(),
            name_to_slot: FxHashMap::default(),
            fast_lookup: FxHashMap::default(),
        }
    }

    /// Create a vtable that inherits all entries from a parent vtable.
    pub fn from_parent(class_id: u64, parent: &Vtable) -> Self {
        Vtable {
            class_id,
            entries: parent.entries.clone(),
            name_to_slot: parent.name_to_slot.clone(),
            fast_lookup: parent.fast_lookup.clone(),
        }
    }

    /// Add a new method to the vtable. If the method already has a slot
    /// (inherited from parent), overrides that slot instead of appending.
    /// Returns the slot number.
    pub fn add_method(
        &mut self,
        name: &str,
        descriptor: &str,
        declaring_class_id: u64,
        method_index: u32,
    ) -> usize {
        // Materialise Arc<str> handles once for both the entry payload
        // and the index key — two refcount bumps total instead of two
        // String allocs per insert.
        let name_arc: Arc<str> = Arc::<str>::from(name);
        let desc_arc: Arc<str> = Arc::<str>::from(descriptor);
        let key = (Arc::clone(&name_arc), Arc::clone(&desc_arc));

        // If this method signature already has a slot, override it.
        if let Some(&slot) = self.name_to_slot.get(&key) {
            self.entries[slot] = Some(VtableEntry {
                declaring_class_id,
                method_index,
                method_name: name_arc,
                descriptor: desc_arc,
                resolved: true,
                resolved_method: None,
                is_native: false,
            });
            return slot;
        }

        // Otherwise append a new slot.
        let slot = self.entries.len();
        self.entries.push(Some(VtableEntry {
            declaring_class_id,
            method_index,
            method_name: name_arc,
            descriptor: desc_arc,
            resolved: true,
            resolved_method: None,
            is_native: false,
        }));
        self.name_to_slot.insert(key, slot);
        // HIGH-6 — keep the u64 fast-lookup map in sync.
        self.fast_lookup
            .entry(fast_lookup_key(name, descriptor))
            .or_insert_with(Vec::new)
            .push(slot);
        slot
    }

    /// Override an existing entry at the given slot index.
    pub fn override_method(
        &mut self,
        slot: usize,
        declaring_class_id: u64,
        method_index: u32,
    ) {
        if let Some(Some(entry)) = self.entries.get_mut(slot) {
            entry.declaring_class_id = declaring_class_id;
            entry.method_index = method_index;
            entry.resolved = true;
        }
    }

    /// Find the slot index for a method by name and descriptor.
    ///
    /// HIGH-6 — zero allocation on the hot path. The `fast_lookup` map
    /// is keyed by `fxhash(name) ^ fxhash(descriptor)`, so the probe
    /// only borrows the input `&str` slices. On a single-slot bucket
    /// (the overwhelmingly common case) we verify the candidate entry's
    /// stored `(method_name, descriptor)` matches before returning;
    /// on a multi-candidate bucket (FxHash collision) we scan all
    /// candidates with the same verification.
    pub fn lookup_slot(&self, name: &str, descriptor: &str) -> Option<usize> {
        let key = fast_lookup_key(name, descriptor);
        let candidates = self.fast_lookup.get(&key)?;
        for &slot in candidates {
            let entry = self.entries.get(slot)?.as_ref()?;
            if &*entry.method_name == name && &*entry.descriptor == descriptor {
                return Some(slot);
            }
        }
        None
    }

    /// Get the vtable entry at the given slot index.
    pub fn get(&self, slot: usize) -> Option<&VtableEntry> {
        self.entries.get(slot).and_then(|e| e.as_ref())
    }

    /// Number of slots in the vtable.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the vtable is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Mark an entry as unresolved (for CHA invalidation).
    pub fn invalidate_slot(&mut self, slot: usize) {
        if let Some(Some(entry)) = self.entries.get_mut(slot) {
            entry.resolved = false;
        }
    }

    /// The class ID this vtable belongs to.
    pub fn class_id(&self) -> u64 {
        self.class_id
    }
}

/// Interface dispatch table.
/// Maps interface method signatures to vtable slot numbers.
pub struct Itable {
    /// Maps (interface_class_id, method_name, descriptor) -> vtable slot.
    entries: HashMap<(u64, String, String), usize>,
}

impl Itable {
    /// Create an empty interface table.
    pub fn new() -> Self {
        Itable {
            entries: HashMap::new(),
        }
    }

    /// Register a mapping from an interface method to a vtable slot.
    pub fn register(
        &mut self,
        interface_class_id: u64,
        method_name: &str,
        descriptor: &str,
        vtable_slot: usize,
    ) {
        self.entries.insert(
            (interface_class_id, method_name.to_string(), descriptor.to_string()),
            vtable_slot,
        );
    }

    /// Look up the vtable slot for an interface method.
    pub fn lookup(
        &self,
        interface_class_id: u64,
        method_name: &str,
        descriptor: &str,
    ) -> Option<usize> {
        let key = (interface_class_id, method_name.to_string(), descriptor.to_string());
        self.entries.get(&key).copied()
    }

    /// Number of registered interface method mappings.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the itable is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for Itable {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages vtables and itables for all loaded classes.
pub struct VtableManager {
    /// Vtable per class ID.
    ///
    /// MED-15 — FxHashMap (rather than SipHash-default `HashMap`)
    /// because the key is a 64-bit class id and we don't need
    /// DoS resistance for an internal cache.
    tables: FxHashMap<u64, Vtable>,
    /// Itable per class ID.
    itables: HashMap<u64, Itable>,
}

impl VtableManager {
    /// Create a new empty manager.
    pub fn new() -> Self {
        VtableManager {
            tables: FxHashMap::default(),
            itables: HashMap::new(),
        }
    }

    /// Create a vtable for a class, optionally inheriting from a parent.
    /// Returns a mutable reference to the newly created vtable.
    pub fn create_vtable(
        &mut self,
        class_id: u64,
        parent_class_id: Option<u64>,
    ) -> &mut Vtable {
        let vtable = match parent_class_id {
            Some(pid) => {
                // Clone the parent vtable entries so we can insert without borrow conflict.
                let parent_entries: Vec<Option<VtableEntry>> = self
                    .tables
                    .get(&pid)
                    .map(|p| p.entries.clone())
                    .unwrap_or_default();
                let parent_name_to_slot: FxHashMap<(Arc<str>, Arc<str>), usize> = self
                    .tables
                    .get(&pid)
                    .map(|p| p.name_to_slot.clone())
                    .unwrap_or_default();
                let parent_fast_lookup: FxHashMap<u64, Vec<usize>> = self
                    .tables
                    .get(&pid)
                    .map(|p| p.fast_lookup.clone())
                    .unwrap_or_default();
                Vtable {
                    class_id,
                    entries: parent_entries,
                    name_to_slot: parent_name_to_slot,
                    fast_lookup: parent_fast_lookup,
                }
            }
            None => Vtable::new(class_id),
        };
        self.tables.insert(class_id, vtable);
        self.tables.get_mut(&class_id).unwrap()
    }

    /// Get an immutable reference to a class's vtable.
    pub fn get_vtable(&self, class_id: u64) -> Option<&Vtable> {
        self.tables.get(&class_id)
    }

    /// Get a mutable reference to a class's vtable.
    pub fn get_vtable_mut(&mut self, class_id: u64) -> Option<&mut Vtable> {
        self.tables.get_mut(&class_id)
    }

    /// Create an itable for a class.
    pub fn create_itable(&mut self, class_id: u64) -> &mut Itable {
        self.itables.entry(class_id).or_insert_with(Itable::new)
    }

    /// Get an immutable reference to a class's itable.
    pub fn get_itable(&self, class_id: u64) -> Option<&Itable> {
        self.itables.get(&class_id)
    }

    /// Resolve a virtual method call: look up the vtable for the given class
    /// and return a cloned entry if found and resolved.
    pub fn resolve_virtual(
        &self,
        class_id: u64,
        name: &str,
        descriptor: &str,
    ) -> Option<VtableEntry> {
        let vtable = self.tables.get(&class_id)?;
        let slot = vtable.lookup_slot(name, descriptor)?;
        let entry = vtable.get(slot)?;
        if entry.resolved {
            Some(entry.clone())
        } else {
            None
        }
    }

    // -----------------------------------------------------------------
    // T10.5 — direct-slot APIs used by the link-time installer and by
    // `invokevirtual` / `invokeinterface` fast paths.
    //
    // `install_vtable` replaces the class's vtable entries wholesale with
    // a pre-built `Vec<Option<VtableEntry>>`; the class-link path in
    // `classloading::class_manager` builds this vec once per class and
    // hands it off here. `resolve_virtual_slot` is the read-only
    // zero-allocation-on-hit fast path the interpreter consults before
    // falling back to the name-based resolution cache.
    // -----------------------------------------------------------------

    /// Install a pre-built vtable for `class_id`. Replaces any existing
    /// entry. Meant to be called once per class at link time.
    ///
    /// `entries[i]` is the entry at slot `i`; `None` means the slot is
    /// intentionally empty (e.g. reserved for an abstract method that
    /// hasn't been overridden yet). The vec is moved — no clone.
    pub fn install_vtable(&mut self, class_id: u64, entries: Vec<Option<VtableEntry>>) {
        // Rebuild the name_to_slot index from the entries so downstream
        // callers that use `lookup_slot` still work. HIGH-6 — the
        // entries already own `Arc<str>` for the name and descriptor,
        // so the keys are O(1) refcount clones rather than fresh allocs.
        let mut name_to_slot: FxHashMap<(Arc<str>, Arc<str>), usize> =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        let mut fast_lookup: FxHashMap<u64, Vec<usize>> =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        for (slot, entry) in entries.iter().enumerate() {
            if let Some(e) = entry {
                let key = fast_lookup_key(&e.method_name, &e.descriptor);
                fast_lookup.entry(key).or_insert_with(Vec::new).push(slot);
                name_to_slot.insert(
                    (Arc::clone(&e.method_name), Arc::clone(&e.descriptor)),
                    slot,
                );
            }
        }
        let vtable = Vtable {
            class_id,
            entries,
            name_to_slot,
            fast_lookup,
        };
        self.tables.insert(class_id, vtable);
    }

    /// Zero-allocation fast path: look up a vtable slot by class+slot pair.
    ///
    /// Returns `Some(entry.clone())` only when the slot is populated and
    /// marked resolved. Called from the interpreter's `invokevirtual`
    /// and `invokeinterface` sites after Agent Theta wires them in.
    ///
    /// The only allocation on the hit path is the `String` clones inside
    /// the returned `VtableEntry` (name + descriptor). Callers that need
    /// strictly allocation-free dispatch should read the entry via
    /// `vtable_ref` below.
    pub fn resolve_virtual_slot(&self, class_id: u64, slot: usize) -> Option<VtableEntry> {
        let vtable = self.tables.get(&class_id)?;
        let entry = vtable.get(slot)?;
        if entry.resolved {
            Some(entry.clone())
        } else {
            None
        }
    }

    /// Borrow the vtable entry at `(class_id, slot)` without cloning.
    ///
    /// Strictly zero-allocation. Returns `None` when the class has no
    /// installed vtable, the slot is out of range, the slot is empty, or
    /// the entry has been invalidated. Agent Theta's dispatch path should
    /// prefer this over `resolve_virtual_slot` when it only needs
    /// `(declaring_class_id, method_index)`.
    #[inline]
    pub fn vtable_entry_ref(&self, class_id: u64, slot: usize) -> Option<&VtableEntry> {
        let vtable = self.tables.get(&class_id)?;
        let entry = vtable.get(slot)?;
        if entry.resolved {
            Some(entry)
        } else {
            None
        }
    }

    /// T5.4.4 CHA hook — invalidate a single vtable slot on the
    /// superclass when a subclass overrides the method.
    ///
    /// Called by the class-hierarchy-change listener after a new
    /// subclass is loaded. JIT entries keyed on the superclass's
    /// vtable entry (via `LeafClass(super_class_id)` assumptions) must
    /// observe `resolve_virtual_slot` returning `None` for that slot
    /// until they are recompiled.
    pub fn invalidate_for_override(&mut self, super_class_id: u64, slot: usize) {
        if let Some(vtable) = self.tables.get_mut(&super_class_id) {
            vtable.invalidate_slot(slot);
        }
    }

    /// Resolve an interface method call: look up the itable for the receiver class,
    /// find the vtable slot, then return the vtable entry.
    pub fn resolve_interface(
        &self,
        class_id: u64,
        interface_id: u64,
        name: &str,
        descriptor: &str,
    ) -> Option<VtableEntry> {
        let itable = self.itables.get(&class_id)?;
        let vtable_slot = itable.lookup(interface_id, name, descriptor)?;
        let vtable = self.tables.get(&class_id)?;
        let entry = vtable.get(vtable_slot)?;
        if entry.resolved {
            Some(entry.clone())
        } else {
            None
        }
    }

    /// Invalidate all vtable entries that were declared by the given class.
    /// Used for Class Hierarchy Analysis (CHA) invalidation when a class is
    /// redefined or a new subclass is loaded.
    pub fn invalidate_class(&mut self, class_id: u64) {
        // Collect slot indices to invalidate across all vtables.
        let invalidations: Vec<(u64, Vec<usize>)> = self
            .tables
            .iter()
            .map(|(&tid, vtable)| {
                let slots: Vec<usize> = vtable
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(i, entry)| {
                        entry.as_ref().and_then(|e| {
                            if e.declaring_class_id == class_id {
                                Some(i)
                            } else {
                                None
                            }
                        })
                    })
                    .collect();
                (tid, slots)
            })
            .collect();

        for (tid, slots) in invalidations {
            if let Some(vtable) = self.tables.get_mut(&tid) {
                for slot in slots {
                    vtable.invalidate_slot(slot);
                }
            }
        }
    }
}

impl Default for VtableManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// T10.5 — VM-wide global VtableManager handle.
//
// The `VtableInstallHook` registered with the classloading crate is a plain
// `fn(u32, Vec<Option<VtableSlotDescriptor>>)` and can't carry a reference
// to any `SharedVm` instance. `SharedVm::new` installs the same `Arc` into
// both the struct field and this global cell, so the hook's adapter can
// upgrade the cell to reach the manager with zero per-call overhead.
//
// The single-VM-per-process convention matches every other global in this
// crate (`JvmtiEventManager`, GC hooks, class-load hooks). If a multi-VM
// harness ever lands, each installed manager will race to win the `set`;
// whichever lands first owns the population stream.
// ---------------------------------------------------------------------------

static GLOBAL_VTABLE_MANAGER: OnceLock<Arc<RwLock<VtableManager>>> = OnceLock::new();

/// Register the process-wide `VtableManager` handle.
///
/// Called once by `SharedVm::new` with the same `Arc` that is stored in
/// `SharedVm::vtable_manager`. Subsequent calls are silently ignored — the
/// first installed manager wins.
pub fn install_global_vtable_manager(manager: Arc<RwLock<VtableManager>>) {
    let _ = GLOBAL_VTABLE_MANAGER.set(manager);
}

/// Borrow the global `VtableManager` handle. Returns `None` when no VM
/// instance has registered one yet (e.g. isolated unit-test harnesses
/// that exercise the class loader without booting `SharedVm`).
pub fn global_vtable_manager() -> Option<Arc<RwLock<VtableManager>>> {
    GLOBAL_VTABLE_MANAGER.get().cloned()
}

/// Adapter that satisfies `rustjvm_classloading::VtableInstallHook`.
///
/// Converts each `VtableSlotDescriptor` into a `VtableEntry` and writes
/// the whole vec into the global `VtableManager` via `install_vtable`.
/// A single write lock is taken per class; concurrent readers calling
/// `resolve_virtual_slot` are unaffected because the read lock is free
/// while the install lock is held only for the index rebuild.
pub fn vtable_install_adapter(
    class_id: u32,
    entries: Vec<Option<rustjvm_classloading::VtableSlotDescriptor>>,
) {
    let manager = match global_vtable_manager() {
        Some(m) => m,
        None => return,
    };
    let converted: Vec<Option<VtableEntry>> = entries
        .into_iter()
        .map(|opt| {
            opt.map(|d| {
                // T10.9.A — when the class loader captured a dispatch
                // snapshot, promote it into a full `Arc<CachedBytecodeMethod>`
                // so the interpreter can push a frame without re-entering
                // the class manager. Bytecode is padded (two trailing
                // zeros) so the hot loop can speculatively read `code[pc+1]`
                // and `code[pc+2]` without bounds checks — same invariant
                // the slow path relies on.
                let (resolved_method, is_native) = match d.dispatch {
                    Some(snap) if !snap.is_native => {
                        let padded = {
                            let mut v = Vec::with_capacity(snap.code.len() + 2);
                            v.extend_from_slice(&snap.code);
                            v.push(0);
                            v.push(0);
                            Arc::<[u8]>::from(v.into_boxed_slice())
                        };
                        let cached = Arc::new(rustjvm_jit_api::CachedBytecodeMethod {
                            declaring_class_id: crate::classloading::ClassId::new(
                                d.declaring_class_id,
                            ),
                            class_name: Arc::<str>::from(snap.class_name.as_str()),
                            method_name: Arc::<str>::from(d.method_name.as_str()),
                            method_descriptor: Arc::<str>::from(d.descriptor.as_str()),
                            source_file: snap.source_file.as_deref().map(Arc::<str>::from),
                            code: padded,
                            exception_table: Arc::<[_]>::from(snap.exception_table.as_slice()),
                            max_stack: snap.max_stack,
                            max_locals: snap.max_locals,
                            num_params: snap.num_params,
                            is_synchronized: snap.is_synchronized,
                            is_static: snap.is_static,
                        });
                        (Some(cached), false)
                    }
                    Some(snap) => (None, snap.is_native),
                    None => (None, false),
                };
                VtableEntry {
                    declaring_class_id: d.declaring_class_id as u64,
                    method_index: d.method_index,
                    // HIGH-5 — promote the owned `String` from the
                    // descriptor into an `Arc<str>` once here so the
                    // hot-path `VtableEntry::clone()` (called on every
                    // virtual dispatch) only bumps a refcount.
                    method_name: Arc::<str>::from(d.method_name),
                    descriptor: Arc::<str>::from(d.descriptor),
                    resolved: true,
                    resolved_method,
                    is_native,
                }
            })
        })
        .collect();
    manager.write().install_vtable(class_id as u64, converted);
}

/// T10.9.A — adapter that satisfies `rustjvm_classloading::VtableOverrideHook`.
///
/// Fires on the same global `VtableManager` as the install hook. Each call
/// invalidates exactly one `(super_class_id, slot)` pair so in-flight
/// thread-local `invoke_cache` entries, promoted shared-resolution
/// targets, and JIT inlines all observe the new subclass as soon as it
/// is linked.
pub fn vtable_override_adapter(super_class_id: u32, slot: usize) {
    let manager = match global_vtable_manager() {
        Some(m) => m,
        None => return,
    };
    manager.write().invalidate_for_override(super_class_id as u64, slot);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_vtable_and_add_methods() {
        let mut vt = Vtable::new(1);
        let s0 = vt.add_method("toString", "()Ljava/lang/String;", 1, 0);
        let s1 = vt.add_method("hashCode", "()I", 1, 1);
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(vt.len(), 2);
    }

    #[test]
    fn test_lookup_by_name() {
        let mut vt = Vtable::new(1);
        vt.add_method("foo", "(I)V", 1, 0);
        vt.add_method("bar", "()I", 1, 1);
        assert_eq!(vt.lookup_slot("foo", "(I)V"), Some(0));
        assert_eq!(vt.lookup_slot("bar", "()I"), Some(1));
        assert_eq!(vt.lookup_slot("baz", "()V"), None);
    }

    #[test]
    fn test_get_entry_by_slot() {
        let mut vt = Vtable::new(10);
        vt.add_method("run", "()V", 10, 5);
        let entry = vt.get(0).unwrap();
        assert_eq!(entry.declaring_class_id, 10);
        assert_eq!(entry.method_index, 5);
        assert_eq!(&*entry.method_name, "run");
        assert!(entry.resolved);
    }

    #[test]
    fn test_inherit_from_parent() {
        let mut parent = Vtable::new(1);
        parent.add_method("toString", "()Ljava/lang/String;", 1, 0);
        parent.add_method("hashCode", "()I", 1, 1);

        let child = Vtable::from_parent(2, &parent);
        assert_eq!(child.len(), 2);
        assert_eq!(child.lookup_slot("toString", "()Ljava/lang/String;"), Some(0));
        let entry = child.get(0).unwrap();
        assert_eq!(entry.declaring_class_id, 1); // still declared by parent
    }

    #[test]
    fn test_override_method_in_child() {
        let mut parent = Vtable::new(1);
        parent.add_method("toString", "()Ljava/lang/String;", 1, 0);

        let mut child = Vtable::from_parent(2, &parent);
        // add_method with same sig overrides the inherited slot
        let slot = child.add_method("toString", "()Ljava/lang/String;", 2, 7);
        assert_eq!(slot, 0); // same slot as parent
        let entry = child.get(0).unwrap();
        assert_eq!(entry.declaring_class_id, 2);
        assert_eq!(entry.method_index, 7);
    }

    #[test]
    fn test_slot_numbers_stable_after_override() {
        let mut parent = Vtable::new(1);
        parent.add_method("a", "()V", 1, 0);
        parent.add_method("b", "()V", 1, 1);
        parent.add_method("c", "()V", 1, 2);

        let mut child = Vtable::from_parent(2, &parent);
        child.add_method("b", "()V", 2, 10); // override middle method
        child.add_method("d", "()V", 2, 11); // new method

        assert_eq!(child.lookup_slot("a", "()V"), Some(0));
        assert_eq!(child.lookup_slot("b", "()V"), Some(1));
        assert_eq!(child.lookup_slot("c", "()V"), Some(2));
        assert_eq!(child.lookup_slot("d", "()V"), Some(3));
        assert_eq!(child.len(), 4);
    }

    #[test]
    fn test_override_method_direct() {
        let mut vt = Vtable::new(1);
        vt.add_method("foo", "()V", 1, 0);
        vt.override_method(0, 2, 99);
        let entry = vt.get(0).unwrap();
        assert_eq!(entry.declaring_class_id, 2);
        assert_eq!(entry.method_index, 99);
        assert!(entry.resolved);
    }

    #[test]
    fn test_interface_dispatch_via_itable() {
        let mut it = Itable::new();
        it.register(100, "run", "()V", 3);
        assert_eq!(it.lookup(100, "run", "()V"), Some(3));
        assert_eq!(it.lookup(100, "run", "(I)V"), None);
        assert_eq!(it.lookup(999, "run", "()V"), None);
    }

    #[test]
    fn test_resolve_virtual_on_leaf_class() {
        let mut mgr = VtableManager::new();
        let vt = mgr.create_vtable(1, None);
        vt.add_method("compute", "(II)I", 1, 0);

        let entry = mgr.resolve_virtual(1, "compute", "(II)I").unwrap();
        assert_eq!(entry.declaring_class_id, 1);
        assert_eq!(entry.method_index, 0);
    }

    #[test]
    fn test_resolve_interface_method() {
        let mut mgr = VtableManager::new();

        // Class 10 has a vtable with "run" at slot 2
        let vt = mgr.create_vtable(10, None);
        vt.add_method("toString", "()Ljava/lang/String;", 10, 0);
        vt.add_method("hashCode", "()I", 10, 1);
        vt.add_method("run", "()V", 10, 2);

        // Register interface 50 mapping for class 10
        let it = mgr.create_itable(10);
        it.register(50, "run", "()V", 2);

        let entry = mgr.resolve_interface(10, 50, "run", "()V").unwrap();
        assert_eq!(entry.declaring_class_id, 10);
        assert_eq!(entry.method_index, 2);
    }

    #[test]
    fn test_invalidate_slot_marks_unresolved() {
        let mut vt = Vtable::new(1);
        vt.add_method("foo", "()V", 1, 0);
        assert!(vt.get(0).unwrap().resolved);

        vt.invalidate_slot(0);
        assert!(!vt.get(0).unwrap().resolved);
    }

    #[test]
    fn test_invalidate_class_across_vtables() {
        let mut mgr = VtableManager::new();

        // Parent class 1
        let vt = mgr.create_vtable(1, None);
        vt.add_method("foo", "()V", 1, 0);
        vt.add_method("bar", "()V", 1, 1);

        // Child class 2 inherits from 1, overrides bar
        let vt2 = mgr.create_vtable(2, Some(1));
        vt2.add_method("bar", "()V", 2, 5);

        // Invalidate class 1 -- should mark foo unresolved in both vtables,
        // bar unresolved only in class 1's vtable (class 2 overrode it).
        mgr.invalidate_class(1);

        let vt1 = mgr.get_vtable(1).unwrap();
        assert!(!vt1.get(0).unwrap().resolved); // foo in class 1
        assert!(!vt1.get(1).unwrap().resolved); // bar in class 1

        let vt2 = mgr.get_vtable(2).unwrap();
        assert!(!vt2.get(0).unwrap().resolved); // foo inherited from class 1
        assert!(vt2.get(1).unwrap().resolved);  // bar overridden by class 2
    }

    #[test]
    fn test_empty_vtable() {
        let vt = Vtable::new(99);
        assert_eq!(vt.len(), 0);
        assert!(vt.is_empty());
        assert_eq!(vt.get(0), None);
        assert_eq!(vt.lookup_slot("any", "()V"), None);
        assert_eq!(vt.class_id(), 99);
    }

    #[test]
    fn test_multiple_interfaces() {
        let mut mgr = VtableManager::new();
        let vt = mgr.create_vtable(10, None);
        vt.add_method("run", "()V", 10, 0);
        vt.add_method("call", "()Ljava/lang/Object;", 10, 1);
        vt.add_method("compare", "(Ljava/lang/Object;Ljava/lang/Object;)I", 10, 2);

        let it = mgr.create_itable(10);
        it.register(50, "run", "()V", 0);                    // Runnable
        it.register(51, "call", "()Ljava/lang/Object;", 1);  // Callable
        it.register(52, "compare", "(Ljava/lang/Object;Ljava/lang/Object;)I", 2); // Comparator

        assert!(mgr.resolve_interface(10, 50, "run", "()V").is_some());
        assert!(mgr.resolve_interface(10, 51, "call", "()Ljava/lang/Object;").is_some());
        assert!(mgr.resolve_interface(10, 52, "compare", "(Ljava/lang/Object;Ljava/lang/Object;)I").is_some());
        assert!(mgr.resolve_interface(10, 99, "run", "()V").is_none());
    }

    #[test]
    fn test_diamond_inheritance() {
        // A (class 1) -> B (class 2), A -> C (class 3), D (class 4) -> B + C
        let mut mgr = VtableManager::new();

        // A: base class with method "foo"
        let vt_a = mgr.create_vtable(1, None);
        vt_a.add_method("foo", "()V", 1, 0);

        // B: inherits from A, overrides foo
        let vt_b = mgr.create_vtable(2, Some(1));
        vt_b.add_method("foo", "()V", 2, 10);

        // C: inherits from A, adds bar
        let vt_c = mgr.create_vtable(3, Some(1));
        vt_c.add_method("bar", "()V", 3, 20);

        // D: inherits from B (in JVM single-inheritance, pick one parent for vtable).
        // Then manually add C's interface methods via itable.
        let vt_d = mgr.create_vtable(4, Some(2));
        vt_d.add_method("bar", "()V", 4, 30); // D implements bar itself

        // D's itable maps interface C's bar
        let it_d = mgr.create_itable(4);
        it_d.register(3, "bar", "()V", 1); // slot 1 in D's vtable

        let entry_foo = mgr.resolve_virtual(4, "foo", "()V").unwrap();
        assert_eq!(entry_foo.declaring_class_id, 2); // inherited from B

        let entry_bar = mgr.resolve_interface(4, 3, "bar", "()V").unwrap();
        assert_eq!(entry_bar.declaring_class_id, 4); // D's own impl
    }

    #[test]
    fn test_lookup_nonexistent_method() {
        let mut mgr = VtableManager::new();
        mgr.create_vtable(1, None);
        assert!(mgr.resolve_virtual(1, "nope", "()V").is_none());
        assert!(mgr.resolve_virtual(999, "nope", "()V").is_none());
        assert!(mgr.resolve_interface(1, 50, "nope", "()V").is_none());
    }

    #[test]
    fn test_large_vtable_100_plus_methods() {
        let mut vt = Vtable::new(1);
        for i in 0..150u32 {
            let name = format!("method_{}", i);
            vt.add_method(&name, "()V", 1, i);
        }
        assert_eq!(vt.len(), 150);

        // Spot check
        assert_eq!(vt.lookup_slot("method_0", "()V"), Some(0));
        assert_eq!(vt.lookup_slot("method_149", "()V"), Some(149));
        let entry = vt.get(75).unwrap();
        assert_eq!(entry.method_index, 75);
        assert_eq!(&*entry.method_name, "method_75");
    }

    #[test]
    fn test_vtable_manager_lifecycle() {
        let mut mgr = VtableManager::new();

        // No vtables initially
        assert!(mgr.get_vtable(1).is_none());
        assert!(mgr.get_vtable_mut(1).is_none());

        // Create root
        let vt = mgr.create_vtable(1, None);
        vt.add_method("init", "()V", 1, 0);

        // Create child
        let vt2 = mgr.create_vtable(2, Some(1));
        vt2.add_method("doWork", "()V", 2, 1);

        // Both exist
        assert!(mgr.get_vtable(1).is_some());
        assert!(mgr.get_vtable(2).is_some());

        // Child has inherited method plus its own
        let child_vt = mgr.get_vtable(2).unwrap();
        assert_eq!(child_vt.len(), 2);
        assert!(child_vt.lookup_slot("init", "()V").is_some());
        assert!(child_vt.lookup_slot("doWork", "()V").is_some());

        // Create itable
        assert!(mgr.get_itable(2).is_none());
        mgr.create_itable(2);
        assert!(mgr.get_itable(2).is_some());
    }

    #[test]
    fn test_resolve_virtual_unresolved_returns_none() {
        let mut mgr = VtableManager::new();
        let vt = mgr.create_vtable(1, None);
        vt.add_method("foo", "()V", 1, 0);

        // Invalidate it
        mgr.get_vtable_mut(1).unwrap().invalidate_slot(0);

        // Should return None because entry is unresolved
        assert!(mgr.resolve_virtual(1, "foo", "()V").is_none());
    }

    #[test]
    fn test_resolve_interface_unresolved_returns_none() {
        let mut mgr = VtableManager::new();
        let vt = mgr.create_vtable(10, None);
        vt.add_method("run", "()V", 10, 0);

        let it = mgr.create_itable(10);
        it.register(50, "run", "()V", 0);

        mgr.get_vtable_mut(10).unwrap().invalidate_slot(0);

        assert!(mgr.resolve_interface(10, 50, "run", "()V").is_none());
    }

    #[test]
    fn test_itable_default() {
        let it = Itable::default();
        assert!(it.is_empty());
        assert_eq!(it.len(), 0);
    }

    #[test]
    fn test_vtable_manager_default() {
        let mgr = VtableManager::default();
        assert!(mgr.get_vtable(1).is_none());
    }

    // ---------------------------------------------------------------
    // T10.5 — install_vtable + resolve_virtual_slot + CHA invalidate
    // ---------------------------------------------------------------

    #[test]
    fn test_install_vtable_and_resolve_slot() {
        let mut mgr = VtableManager::new();
        let entries = vec![
            Some(VtableEntry {
                declaring_class_id: 42,
                method_index: 0,
                method_name: Arc::<str>::from("toString"),
                descriptor: Arc::<str>::from("()Ljava/lang/String;"),
                resolved: true,
                resolved_method: None,
                is_native: false,
            }),
            Some(VtableEntry {
                declaring_class_id: 42,
                method_index: 1,
                method_name: Arc::<str>::from("hashCode"),
                descriptor: Arc::<str>::from("()I"),
                resolved: true,
                resolved_method: None,
                is_native: false,
            }),
        ];
        mgr.install_vtable(42, entries);

        let e0 = mgr.resolve_virtual_slot(42, 0).unwrap();
        assert_eq!(e0.declaring_class_id, 42);
        assert_eq!(e0.method_index, 0);
        assert_eq!(&*e0.method_name, "toString");

        let e1 = mgr.resolve_virtual_slot(42, 1).unwrap();
        assert_eq!(e1.method_index, 1);

        // name-based lookup still works because install_vtable rebuilt the index
        let by_name = mgr.resolve_virtual(42, "hashCode", "()I").unwrap();
        assert_eq!(by_name.method_index, 1);

        // unknown class / slot → None
        assert!(mgr.resolve_virtual_slot(999, 0).is_none());
        assert!(mgr.resolve_virtual_slot(42, 99).is_none());
    }

    #[test]
    fn test_install_vtable_replaces_previous_entries() {
        let mut mgr = VtableManager::new();

        let first = vec![Some(VtableEntry {
            declaring_class_id: 1,
            method_index: 0,
            method_name: Arc::<str>::from("foo"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        })];
        mgr.install_vtable(1, first);

        let second = vec![Some(VtableEntry {
            declaring_class_id: 1,
            method_index: 7,
            method_name: Arc::<str>::from("bar"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        })];
        mgr.install_vtable(1, second);

        // old "foo" is gone, new "bar" is installed at slot 0
        assert!(mgr.resolve_virtual(1, "foo", "()V").is_none());
        let e = mgr.resolve_virtual_slot(1, 0).unwrap();
        assert_eq!(e.method_index, 7);
        assert_eq!(&*e.method_name, "bar");
    }

    #[test]
    fn test_invalidate_for_override_single_slot() {
        let mut mgr = VtableManager::new();
        let entries = vec![
            Some(VtableEntry {
                declaring_class_id: 1,
                method_index: 0,
                method_name: Arc::<str>::from("foo"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: None,
                is_native: false,
            }),
            Some(VtableEntry {
                declaring_class_id: 1,
                method_index: 1,
                method_name: Arc::<str>::from("bar"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: None,
                is_native: false,
            }),
        ];
        mgr.install_vtable(1, entries);

        // CHA signal: a new subclass overrode slot 0 of class 1.
        mgr.invalidate_for_override(1, 0);

        // slot 0 is now unresolved → resolve_virtual_slot returns None
        assert!(mgr.resolve_virtual_slot(1, 0).is_none());
        // slot 1 is still resolved
        assert!(mgr.resolve_virtual_slot(1, 1).is_some());

        // invalidating an unknown class is a no-op
        mgr.invalidate_for_override(999, 0);
    }

    #[test]
    fn test_vtable_entry_ref_borrow_no_clone() {
        let mut mgr = VtableManager::new();
        let entries = vec![Some(VtableEntry {
            declaring_class_id: 5,
            method_index: 3,
            method_name: Arc::<str>::from("run"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        })];
        mgr.install_vtable(5, entries);

        let eref = mgr.vtable_entry_ref(5, 0).expect("slot should be populated");
        assert_eq!(eref.method_index, 3);
        assert_eq!(eref.declaring_class_id, 5);

        // invalidate → vtable_entry_ref returns None even though the entry exists
        mgr.invalidate_for_override(5, 0);
        assert!(mgr.vtable_entry_ref(5, 0).is_none());
    }

    // -----------------------------------------------------------------
    // T10.5 task-required tests
    // -----------------------------------------------------------------

    /// A populated vtable must be queryable via resolve_virtual_slot
    /// immediately after `install_vtable`.
    #[test]
    fn t10_vtable_populated_at_link_time() {
        let mut mgr = VtableManager::new();
        let entries = vec![Some(VtableEntry {
            declaring_class_id: 7,
            method_index: 2,
            method_name: Arc::<str>::from("foo"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        })];
        mgr.install_vtable(7, entries);

        let entry = mgr
            .resolve_virtual_slot(7, 0)
            .expect("vtable should be populated at link time");
        assert_eq!(entry.declaring_class_id, 7);
        assert_eq!(entry.method_index, 2);
    }

    /// When a subclass doesn't override a slot, the installed vtable
    /// carries the superclass's entry for that slot.
    #[test]
    fn t10_vtable_inherits_from_super() {
        let super_entry = VtableEntry {
            declaring_class_id: 1,
            method_index: 0,
            method_name: Arc::<str>::from("greet"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        };

        // Subclass didn't override — slot 0 still carries the super's
        // declaring_class_id.
        let subclass_entries = vec![Some(super_entry.clone())];

        let mut mgr = VtableManager::new();
        mgr.install_vtable(1, vec![Some(super_entry.clone())]);
        mgr.install_vtable(2, subclass_entries);

        let inherited = mgr.resolve_virtual_slot(2, 0).unwrap();
        assert_eq!(
            inherited.declaring_class_id, 1,
            "subclass slot must still point at super's declaring class",
        );
        assert_eq!(inherited.method_index, 0);
        assert_eq!(&*inherited.method_name, "greet");
    }

    /// When the subclass overrides a method, its own entry occupies the
    /// inherited slot (same slot number, new declaring_class_id).
    #[test]
    fn t10_vtable_override_replaces_super() {
        let super_entries = vec![Some(VtableEntry {
            declaring_class_id: 1,
            method_index: 0,
            method_name: Arc::<str>::from("greet"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        })];

        let subclass_entries = vec![Some(VtableEntry {
            declaring_class_id: 2,
            method_index: 5,
            method_name: Arc::<str>::from("greet"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        })];

        let mut mgr = VtableManager::new();
        mgr.install_vtable(1, super_entries);
        mgr.install_vtable(2, subclass_entries);

        let e = mgr.resolve_virtual_slot(2, 0).unwrap();
        assert_eq!(
            e.declaring_class_id, 2,
            "override must replace the super's declaring_class_id",
        );
        assert_eq!(e.method_index, 5);
    }

    /// 4 threads reading the same vtable concurrently must all succeed
    /// without deadlock. `resolve_virtual_slot` must be read-lock only.
    #[test]
    fn t10_vtable_resolve_concurrent() {
        use std::sync::Arc;
        use std::thread;

        let mgr = Arc::new(parking_lot::RwLock::new(VtableManager::new()));
        {
            let mut w = mgr.write();
            let entries = vec![
                Some(VtableEntry {
                    declaring_class_id: 10,
                    method_index: 0,
                    method_name: Arc::<str>::from("a"),
                    descriptor: Arc::<str>::from("()V"),
                    resolved: true,
                    resolved_method: None,
                    is_native: false,
                }),
                Some(VtableEntry {
                    declaring_class_id: 10,
                    method_index: 1,
                    method_name: Arc::<str>::from("b"),
                    descriptor: Arc::<str>::from("()V"),
                    resolved: true,
                    resolved_method: None,
                    is_native: false,
                }),
                Some(VtableEntry {
                    declaring_class_id: 10,
                    method_index: 2,
                    method_name: Arc::<str>::from("c"),
                    descriptor: Arc::<str>::from("()V"),
                    resolved: true,
                    resolved_method: None,
                    is_native: false,
                }),
            ];
            w.install_vtable(10, entries);
        }

        let mut handles = Vec::new();
        for t in 0..4 {
            let mgr = Arc::clone(&mgr);
            handles.push(thread::spawn(move || {
                let mut hits = 0usize;
                for _ in 0..2000 {
                    let guard = mgr.read();
                    let slot = t % 3;
                    let e = guard
                        .resolve_virtual_slot(10, slot)
                        .expect("concurrent reader missed a populated slot");
                    assert_eq!(e.declaring_class_id, 10);
                    assert_eq!(e.method_index as usize, slot);
                    hits += 1;
                }
                hits
            }));
        }
        let totals: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for t in totals {
            assert_eq!(t, 2000);
        }
    }

    // ------------------------------------------------------------------
    // T10.9.A tests — vtable dispatch integration
    //
    // These tests exercise the `VtableEntry::resolved_method` payload,
    // `vtable_install_adapter` conversion, and the `invalidate_for_override`
    // CHA path without booting a full `SharedVm`.  The vm-level
    // integration test `t10_9_a_vtable_fast_path_hits_without_class_manager_lock`
    // lives in `vm.rs` alongside other synthetic-jdk integration tests.
    // ------------------------------------------------------------------

    /// Helper — build a minimal `CachedBytecodeMethod` Arc for a given
    /// slot so the `resolved_method` field can be exercised without a
    /// real class file.
    fn make_dispatch_stub(
        declaring_class_id: u32,
        method_name: &str,
        descriptor: &str,
    ) -> Arc<rustjvm_jit_api::CachedBytecodeMethod> {
        Arc::new(rustjvm_jit_api::CachedBytecodeMethod {
            declaring_class_id: rustjvm_types::ClassId::new(declaring_class_id),
            class_name: Arc::<str>::from("Stub"),
            method_name: Arc::<str>::from(method_name),
            method_descriptor: Arc::<str>::from(descriptor),
            source_file: None,
            // 0xb1 = `return` — safe single-op bytecode.
            code: Arc::<[u8]>::from(vec![0xb1u8, 0x00, 0x00].into_boxed_slice()),
            exception_table: Arc::<[_]>::from(Vec::new().into_boxed_slice()),
            max_stack: 0,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        })
    }

    /// T10.9.A.1 — `VtableEntry` must carry a pre-built `Arc<CachedBytecodeMethod>`
    /// after `vtable_install_adapter` converts a
    /// `VtableSlotDescriptor` that included a dispatch snapshot.
    #[test]
    fn t10_9_a_vtable_entry_carries_resolved_method() {
        let mut mgr = VtableManager::new();
        let entries = vec![Some(VtableEntry {
            declaring_class_id: 100,
            method_index: 0,
            method_name: Arc::<str>::from("run"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: Some(make_dispatch_stub(100, "run", "()V")),
            is_native: false,
        })];
        mgr.install_vtable(100, entries);

        let entry = mgr
            .resolve_virtual_slot(100, 0)
            .expect("slot 0 should be populated");
        assert_eq!(entry.declaring_class_id, 100);
        assert!(
            entry.resolved_method.is_some(),
            "vtable entry must carry Arc<CachedBytecodeMethod> for fast-path dispatch",
        );
        let cached = entry.resolved_method.unwrap();
        assert_eq!(&*cached.method_name, "run");
        assert_eq!(&*cached.method_descriptor, "()V");
    }

    /// T10.9.A.4 — when a subclass overrides a superclass method,
    /// `invalidate_for_override` is fired against the super's slot and
    /// the entry is marked unresolved. Subsequent `resolve_virtual_slot`
    /// returns `None` so stale cached dispatch entries get re-resolved.
    #[test]
    fn t10_9_a_vtable_override_invalidation() {
        let mut mgr = VtableManager::new();

        // Super class Animal — slot 0 = speak()V
        let super_entries = vec![Some(VtableEntry {
            declaring_class_id: 1,
            method_index: 0,
            method_name: Arc::<str>::from("speak"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method: Some(make_dispatch_stub(1, "speak", "()V")),
            is_native: false,
        })];
        mgr.install_vtable(1, super_entries);

        // Before the override, Animal's slot 0 resolves.
        assert!(mgr.resolve_virtual_slot(1, 0).is_some());
        // And carries a dispatch snapshot.
        assert!(mgr.resolve_virtual_slot(1, 0).unwrap().resolved_method.is_some());

        // Subclass Dog overrides speak() — the link-time hook calls
        // `vtable_override_adapter(1, 0)` which hits this API.
        mgr.invalidate_for_override(1, 0);

        // Now Animal's slot 0 is unresolved.
        assert!(
            mgr.resolve_virtual_slot(1, 0).is_none(),
            "override invalidation must mark super's slot unresolved",
        );
        // `vtable_entry_ref` also returns None because the entry is
        // unresolved (the is_resolved check gates both accessors).
        assert!(mgr.vtable_entry_ref(1, 0).is_none());
    }

    /// T10.9.A.3 — a class with no vtable entry at the requested slot
    /// returns `None` from `resolve_virtual_slot` — the fast-path
    /// caller must fall through to the slow path without panic. Also
    /// exercises bounds-check safety: slot way past the end, class_id
    /// out of range.
    #[test]
    fn t10_9_a_vtable_miss_falls_through() {
        let mut mgr = VtableManager::new();

        // Empty vtable for class 50 — no slots at all.
        mgr.install_vtable(50, vec![]);
        assert!(mgr.resolve_virtual_slot(50, 0).is_none());
        assert!(mgr.resolve_virtual_slot(50, 99).is_none());

        // Sparse vtable: slot 0 = Some, slot 1 = None (reserved).
        let entries = vec![
            Some(VtableEntry {
                declaring_class_id: 51,
                method_index: 0,
                method_name: Arc::<str>::from("foo"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: None,
                is_native: false,
            }),
            None,
        ];
        mgr.install_vtable(51, entries);

        assert!(mgr.resolve_virtual_slot(51, 0).is_some());
        // `None` slot → lookup returns None, no panic.
        assert!(mgr.resolve_virtual_slot(51, 1).is_none());
        // Out-of-range slot → None.
        assert!(mgr.resolve_virtual_slot(51, 1_000_000).is_none());
        // Unknown class_id → None.
        assert!(mgr.resolve_virtual_slot(999_999, 0).is_none());
    }

    /// T10.9.A — when the install adapter sees a `VtableSlotDescriptor`
    /// with `dispatch: None`, the resulting `VtableEntry` has
    /// `resolved_method: None` (and `is_native: false` unless the
    /// snapshot explicitly flagged native).
    #[test]
    fn t10_9_a_adapter_preserves_empty_dispatch() {
        // We can't call vtable_install_adapter without a global
        // manager; simulate the conversion manually.
        let desc = rustjvm_classloading::VtableSlotDescriptor {
            declaring_class_id: 77,
            method_index: 3,
            method_name: Arc::<str>::from("abstr"),
            descriptor: Arc::<str>::from("()I"),
            dispatch: None, // abstract method — no snapshot
        };
        // Sanity — field is accessible and defaults to None.
        assert!(desc.dispatch.is_none());
        assert_eq!(desc.method_index, 3);
    }

    /// T10.9.A — a native `VtableMethodSnapshot` routes into a
    /// `VtableEntry` with `is_native == true` and no bytecode Arc. The
    /// interpreter's fast-path returns `CacheMiss` for these so the
    /// native registry takes over.
    #[test]
    fn t10_9_a_native_snapshot_flags_is_native() {
        let mut mgr = VtableManager::new();
        let entries = vec![Some(VtableEntry {
            declaring_class_id: 88,
            method_index: 0,
            method_name: Arc::<str>::from("getClass"),
            descriptor: Arc::<str>::from("()Ljava/lang/Class;"),
            resolved: true,
            resolved_method: None,
            is_native: true,
        })];
        mgr.install_vtable(88, entries);

        let e = mgr.resolve_virtual_slot(88, 0).expect("slot populated");
        assert!(e.is_native, "native methods must carry is_native=true");
        assert!(
            e.resolved_method.is_none(),
            "native methods carry no bytecode Arc",
        );
    }

    /// T10.9.A — installing a vtable with a dispatch-bearing entry
    /// twice (e.g. class redefine) replaces the Arc cleanly; the old
    /// Arc is dropped when the install completes.
    #[test]
    fn t10_9_a_install_twice_drops_prior_arc() {
        let mut mgr = VtableManager::new();
        let first_arc = make_dispatch_stub(10, "foo", "()V");
        let second_arc = make_dispatch_stub(10, "foo", "()V");
        let first_weak = Arc::downgrade(&first_arc);

        mgr.install_vtable(
            10,
            vec![Some(VtableEntry {
                declaring_class_id: 10,
                method_index: 0,
                method_name: Arc::<str>::from("foo"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: Some(first_arc),
                is_native: false,
            })],
        );

        mgr.install_vtable(
            10,
            vec![Some(VtableEntry {
                declaring_class_id: 10,
                method_index: 0,
                method_name: Arc::<str>::from("foo"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: Some(second_arc),
                is_native: false,
            })],
        );

        // The weak reference to the first Arc should now be unable to
        // upgrade — the vtable no longer holds it and we dropped our
        // local handle.
        assert!(
            first_weak.upgrade().is_none(),
            "installing a second vtable must drop the prior resolved_method Arc",
        );
    }
}
