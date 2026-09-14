// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Virtual method dispatch tables (vtables) and interface method tables (itables).
//!
//! Each class gets a vtable that inherits from its parent and
//! overrides/appends entries for its own methods.
//!
//! ## What this actually costs today (audited 2026-07-26, `stackwalk-and-vtable`)
//!
//! The historical module doc claimed this module "replaces HashMap lookups
//! with direct array indexing". That is **half true and worth stating
//! precisely**, because callers have repeatedly assumed the O(1) half:
//!
//! * `Vtable::get(slot)` *is* direct array indexing — O(1), no hashing.
//! * But no production caller has a slot number. The interpreter's only
//!   consumer (`interpreter.rs`, the "fast path 0" `vtable_fast` helper)
//!   calls [`Vtable::lookup_slot`]`(name, descriptor)`, which is an
//!   `FxHashMap` probe (two `FxHasher` streams over the name and the
//!   descriptor) followed by a verifying `&str` comparison of both. There is
//!   no per-call-site slot cache, so **every** dispatch that reaches this
//!   module re-derives the slot from strings. See the cross-owner request in
//!   `stackwalk-and-vtable.md` for the
//!   quickened-CP slot cache that would make it genuinely O(1).
//! * The lookup also runs under a read guard on the process-wide
//!   `RwLock<VtableManager>`, and the interpreter re-opens
//!   `class_manager.read()` around it for the redefine guard.
//!
//! ## Interface dispatch
//!
//! [`Itable`] is **not on any production path**. `VtableManager::create_itable`,
//! `get_itable` and `resolve_interface` have no callers outside this file's own
//! tests; `invokeinterface` resolves through the receiver's *vtable* by
//! `(name, descriptor)` exactly like `invokevirtual`, because the receiver's
//! vtable already contains its interface implementations. The `CRIT` note on
//! `Itable::lookup` saying "`invokeinterface` is on every interface dispatch"
//! describes an intent, not the wiring. The type is kept (it is correct, and
//! wiring it is the natural next step) but must not be read as a description
//! of current behaviour.

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

/// Candidate slots sharing one [`fast_lookup_key`] bucket.
///
/// PERF (2026-07-26 arch pass). The bucket used to be a `Vec<usize>`, i.e. a
/// **heap allocation per distinct method signature per class**. `install_vtable`
/// builds one bucket for every slot of every class it links, so a Spring-shaped
/// application with tens of thousands of classes averaging dozens of methods
/// paid a `Vec` header + allocation for each — and `Vtable::from_parent` /
/// `create_vtable` then deep-cloned every one of them again per subclass.
///
/// FxHash collisions between two `(name, descriptor)` pairs are vanishingly
/// rare, so `One` covers essentially every bucket and allocates nothing;
/// `Many` preserves the exact collision behaviour (all candidates are probed
/// and verified) for the rare case.
#[derive(Clone, Debug, PartialEq)]
enum SlotBucket {
    One(u32),
    Many(Vec<u32>),
}

impl SlotBucket {
    #[inline]
    fn push(&mut self, slot: u32) {
        match self {
            SlotBucket::One(first) => {
                if *first != slot {
                    *self = SlotBucket::Many(vec![*first, slot]);
                }
            }
            SlotBucket::Many(v) => {
                if !v.contains(&slot) {
                    v.push(slot);
                }
            }
        }
    }

    #[inline]
    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        // `std::slice::from_ref` keeps both arms one type without allocating.
        let slice: &[u32] = match self {
            SlotBucket::One(s) => std::slice::from_ref(s),
            SlotBucket::Many(v) => v.as_slice(),
        };
        slice.iter().copied()
    }
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
    pub resolved_method: Option<Arc<cratonvm_jit_api::CachedBytecodeMethod>>,
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
    /// HIGH-6 — zero-allocation fast path for `lookup_slot(&str, &str)`.
    /// Keyed by `fast_lookup_key(name, descriptor)`; the bucket handles the
    /// (vanishingly rare) FxHash collisions without allocating on probe. On
    /// hit, the caller verifies that each candidate entry's stored
    /// `(name, descriptor)` matches the args.
    ///
    /// This is the **only** signature index. A second
    /// `name_to_slot: FxHashMap<(Arc<str>, Arc<str>), usize>` used to be built
    /// alongside it — by `install_vtable`, for every slot of every class the
    /// class loader links — and was documented as "the authoritative fallback
    /// when the `fast_lookup` u64-hash map collides". It was not: `lookup_slot`
    /// never consulted it (it resolves collisions by verifying each candidate
    /// in the bucket), and its only reader was the test-only `add_method`.
    /// Every production class link therefore paid a full second hash map —
    /// hashing both strings again and bumping two `Arc` refcounts per method —
    /// for a map nothing read. Removed 2026-07-26; `add_method` now asks
    /// `lookup_slot`, which returns the identical answer.
    fast_lookup: FxHashMap<u64, SlotBucket>,
}

impl Vtable {
    /// Create an empty vtable for the given class.
    pub fn new(class_id: u64) -> Self {
        Vtable {
            class_id,
            entries: Vec::new(),
            fast_lookup: FxHashMap::default(),
        }
    }

    /// Create a vtable that inherits all entries from a parent vtable.
    pub fn from_parent(class_id: u64, parent: &Vtable) -> Self {
        Vtable {
            class_id,
            entries: parent.entries.clone(),
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
        let name_arc: Arc<str> = Arc::<str>::from(name);
        let desc_arc: Arc<str> = Arc::<str>::from(descriptor);

        // If this method signature already has a slot, override it. This used
        // to probe a separate `name_to_slot` map; `lookup_slot` gives the
        // identical answer (it verifies name+descriptor byte-for-byte on every
        // candidate) from the one index this vtable now keeps.
        if let Some(slot) = self.lookup_slot(name, descriptor) {
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
        // HIGH-6 — keep the u64 fast-lookup map in sync.
        self.fast_lookup
            .entry(fast_lookup_key(name, descriptor))
            .and_modify(|b| b.push(slot as u32))
            .or_insert(SlotBucket::One(slot as u32));
        slot
    }

    /// Override an existing entry at the given slot index.
    pub fn override_method(&mut self, slot: usize, declaring_class_id: u64, method_index: u32) {
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
    ///
    /// Round-7 Fix 6: `#[inline]` so the vtable lookup folds into the
    /// invokevirtual / invokeinterface dispatch path under LTO.
    #[inline]
    pub fn lookup_slot(&self, name: &str, descriptor: &str) -> Option<usize> {
        let key = fast_lookup_key(name, descriptor);
        let candidates = self.fast_lookup.get(&key)?;
        for slot in candidates.iter() {
            let slot = slot as usize;
            // HIGH-6 — verify each candidate; on bucket collision we
            // continue rather than short-circuit. A `None` or
            // out-of-range slot here would indicate index/entries
            // drift, which shouldn't happen under the current insert
            // paths, but skipping silently keeps the lookup correct.
            let entry = match self.entries.get(slot).and_then(|e| e.as_ref()) {
                Some(e) => e,
                None => continue,
            };
            if &*entry.method_name == name && &*entry.descriptor == descriptor {
                return Some(slot);
            }
        }
        None
    }

    /// Get the vtable entry at the given slot index.
    #[inline]
    pub fn get(&self, slot: usize) -> Option<&VtableEntry> {
        self.entries.get(slot).and_then(|e| e.as_ref())
    }

    /// Number of slots in the vtable.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the vtable is empty.
    #[inline]
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
    #[inline]
    pub fn class_id(&self) -> u64 {
        self.class_id
    }
}

/// One concrete itable entry — keeps the `Arc<str>` name + descriptor so
/// hash collisions can be disambiguated without allocation.
struct ItableEntry {
    method_name: Arc<str>,
    descriptor: Arc<str>,
    vtable_slot: usize,
}

/// Interface dispatch table.
/// Maps interface method signatures to vtable slot numbers.
///
/// CRIT — `invokeinterface` is on every interface dispatch. The lookup
/// must not allocate. The map is keyed by a precomputed
/// `(interface_class_id, fxhash(name) ^ fxhash(descriptor))` u128, and
/// the slot bucket carries `Arc<str>` name+descriptor for the (rare)
/// FxHash-collision disambiguation. Both `register` and `lookup` take
/// `&str` and never `to_string()`.
pub struct Itable {
    /// FxHashMap from (interface_class_id, fast_lookup_key) -> candidate
    /// slot entries. Multiple entries per bucket are only present on
    /// hash collisions; the common case is a single entry per bucket.
    entries: FxHashMap<(u64, u64), Vec<ItableEntry>>,
}

impl Itable {
    /// Create an empty interface table.
    pub fn new() -> Self {
        Itable {
            entries: FxHashMap::default(),
        }
    }

    /// Register a mapping from an interface method to a vtable slot.
    ///
    /// Takes `&str` for the name/descriptor and interns them as
    /// `Arc<str>` inside the bucket entry. Callers that already hold
    /// `Arc<str>` pay a single refcount bump worth of work via
    /// `Arc::from(name)` rather than a heap copy proportional to the
    /// method-name length.
    pub fn register(
        &mut self,
        interface_class_id: u64,
        method_name: &str,
        descriptor: &str,
        vtable_slot: usize,
    ) {
        let key = (interface_class_id, fast_lookup_key(method_name, descriptor));
        let bucket = self.entries.entry(key).or_default();
        // Replace if the same name/descriptor already lives in this
        // bucket (re-register), otherwise append.
        if let Some(existing) = bucket
            .iter_mut()
            .find(|e| &*e.method_name == method_name && &*e.descriptor == descriptor)
        {
            existing.vtable_slot = vtable_slot;
        } else {
            bucket.push(ItableEntry {
                method_name: Arc::from(method_name),
                descriptor: Arc::from(descriptor),
                vtable_slot,
            });
        }
    }

    /// Look up the vtable slot for an interface method.
    ///
    /// CRIT — zero allocation. The fast-path key is a 128-bit
    /// `(class_id, fxhash(name)^fxhash(descriptor))` derived directly
    /// from the input `&str` slices.
    #[inline]
    pub fn lookup(
        &self,
        interface_class_id: u64,
        method_name: &str,
        descriptor: &str,
    ) -> Option<usize> {
        let key = (interface_class_id, fast_lookup_key(method_name, descriptor));
        let bucket = self.entries.get(&key)?;
        for entry in bucket {
            if &*entry.method_name == method_name && &*entry.descriptor == descriptor {
                return Some(entry.vtable_slot);
            }
        }
        None
    }

    /// Number of registered interface method mappings.
    ///
    /// Counts all bucket entries, not just buckets — a multi-candidate
    /// bucket (FxHash collision) is rare but still has multiple
    /// distinct registrations.
    pub fn len(&self) -> usize {
        self.entries.values().map(|b| b.len()).sum()
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
    pub fn create_vtable(&mut self, class_id: u64, parent_class_id: Option<u64>) -> &mut Vtable {
        let vtable = match parent_class_id {
            Some(pid) => {
                // Clone the parent vtable entries so we can insert without borrow conflict.
                let parent_entries: Vec<Option<VtableEntry>> = self
                    .tables
                    .get(&pid)
                    .map(|p| p.entries.clone())
                    .unwrap_or_default();
                let parent_fast_lookup: FxHashMap<u64, SlotBucket> = self
                    .tables
                    .get(&pid)
                    .map(|p| p.fast_lookup.clone())
                    .unwrap_or_default();
                Vtable {
                    class_id,
                    entries: parent_entries,
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
    ///
    /// Round-2 VM §8 HIGH — cloning a `VtableEntry` triggers three
    /// `Arc` refcount bumps (`method_name`, `descriptor`,
    /// `resolved_method`). Hot-path callers that only need the
    /// dispatch snapshot should prefer
    /// [`VtableManager::resolve_virtual_method`] which clones the
    /// single `Arc<CachedBytecodeMethod>` and avoids the other two.
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

    /// Hot-path counterpart to [`VtableManager::resolve_virtual`]:
    /// returns only the cached `Arc<CachedBytecodeMethod>` for the
    /// resolved entry.
    ///
    /// Round-2 VM §8 HIGH — cloning a full `VtableEntry` performs
    /// three `Arc` bumps (`method_name`, `descriptor`,
    /// `resolved_method`) even though the interpreter's
    /// `invokevirtual` fast path only needs the third one. This
    /// helper clones the dispatch `Arc` alone, eliminating the other
    /// two refcount round-trips per invoke. Returns `None` when the
    /// class has no vtable, the method isn't present, the slot is
    /// unresolved, or the entry is a native/abstract method without
    /// a cached bytecode snapshot.
    #[inline]
    pub fn resolve_virtual_method(
        &self,
        class_id: u64,
        name: &str,
        descriptor: &str,
    ) -> Option<Arc<cratonvm_jit_api::CachedBytecodeMethod>> {
        let vtable = self.tables.get(&class_id)?;
        let slot = vtable.lookup_slot(name, descriptor)?;
        let entry = vtable.get(slot)?;
        if !entry.resolved {
            return None;
        }
        entry.resolved_method.clone()
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
        // Redefinition keeps a ClassId stable and calls this installer again
        // with fresh bytecode snapshots. Subclass vtables copy inherited
        // entries when the subclass is linked, so merely replacing the
        // redefined class's own table leaves every already-linked subclass
        // pointing at the old Arc<CachedBytecodeMethod>. Refresh precisely
        // those inherited entries before replacing the owner's table.
        //
        // This is intentionally O(total vtable slots) on the cold JVMTI
        // redefine path. Dispatch remains O(1), and an unrelated override is
        // protected by the declaring_class_id check. Prior code avoided this
        // cold scan by permanently rejecting every inherited vtable hit after
        // redefine, which forced method resolution and bytecode quickening
        // back onto the per-call path.
        if self.tables.contains_key(&class_id) {
            let mut replacements: FxHashMap<u64, Vec<VtableEntry>> =
                FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
            for entry in entries.iter().flatten() {
                if entry.declaring_class_id == class_id {
                    replacements
                        .entry(fast_lookup_key(&entry.method_name, &entry.descriptor))
                        .or_default()
                        .push(entry.clone());
                }
            }
            if !replacements.is_empty() {
                for table in self.tables.values_mut() {
                    if table.class_id == class_id {
                        continue;
                    }
                    for slot in &mut table.entries {
                        let Some(inherited) = slot.as_ref() else {
                            continue;
                        };
                        if inherited.declaring_class_id != class_id {
                            continue;
                        }
                        let key = fast_lookup_key(&inherited.method_name, &inherited.descriptor);
                        let replacement = replacements.get(&key).and_then(|candidates| {
                            candidates.iter().find(|candidate| {
                                candidate.method_name == inherited.method_name
                                    && candidate.descriptor == inherited.descriptor
                            })
                        });
                        if let Some(replacement) = replacement {
                            *slot = Some(replacement.clone());
                        } else {
                            // The JVM structural-redefinition checks normally
                            // make this impossible. Fail closed if a malformed
                            // internal caller violates that invariant.
                            *slot = None;
                        }
                    }
                }
            }
        }

        // Rebuild the signature index from the entries so `lookup_slot`
        // works. One map, one bucket per signature, no allocation per bucket
        // in the (overwhelmingly common) collision-free case — see the
        // `fast_lookup` field doc for the second map this used to build and
        // why it was removed.
        let mut fast_lookup: FxHashMap<u64, SlotBucket> =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        for (slot, entry) in entries.iter().enumerate() {
            if let Some(e) = entry {
                let key = fast_lookup_key(&e.method_name, &e.descriptor);
                fast_lookup
                    .entry(key)
                    .and_modify(|b| b.push(slot as u32))
                    .or_insert(SlotBucket::One(slot as u32));
            }
        }
        let vtable = Vtable {
            class_id,
            entries,
            fast_lookup,
        };
        self.tables.insert(class_id, vtable);
    }

    /// Slot-indexed lookup that returns a fully cloned `VtableEntry`.
    ///
    /// Returns `Some(entry.clone())` only when the slot is populated and
    /// marked resolved. Called from the interpreter's `invokevirtual`
    /// and `invokeinterface` sites after Agent Theta wires them in.
    ///
    /// HIGH-5 — the returned clone is now O(1): `method_name` and
    /// `descriptor` are `Arc<str>` so cloning the entry is a pair of
    /// refcount bumps rather than two heap copies.
    ///
    /// Round-2 VM §8 HIGH — the full entry clone is still three Arc
    /// refcount bumps (`method_name`, `descriptor`, `resolved_method`).
    /// Hot-path callers (`invokevirtual` fast path) that only need to
    /// push a frame should prefer
    /// [`VtableManager::resolve_virtual_slot_method`], which clones a
    /// single `Arc<CachedBytecodeMethod>` and skips the other two
    /// refcount round-trips. Use this variant only when the
    /// `(declaring_class_id, method_index, method_name, descriptor,
    /// is_native)` descriptive fields are all needed by the caller —
    /// or use [`VtableManager::vtable_entry_ref`] to borrow without
    /// any refcount traffic at all.
    pub fn resolve_virtual_slot(&self, class_id: u64, slot: usize) -> Option<VtableEntry> {
        let vtable = self.tables.get(&class_id)?;
        let entry = vtable.get(slot)?;
        if entry.resolved {
            Some(entry.clone())
        } else {
            None
        }
    }

    /// Hot-path slot-indexed lookup that returns only the cached
    /// `Arc<CachedBytecodeMethod>` for the slot.
    ///
    /// Round-2 VM §8 HIGH — `resolve_virtual_slot` clones the full
    /// `VtableEntry` (three `Arc` refcount bumps). The interpreter's
    /// `invokevirtual` fast-path miss only needs the `resolved_method`
    /// Arc to push a frame; this helper returns just that single
    /// `Arc<CachedBytecodeMethod>` so each invoke pays one refcount
    /// bump instead of three.
    ///
    /// Returns `None` when:
    /// - the class has no installed vtable,
    /// - the slot is out of range or empty,
    /// - the entry is marked unresolved (CHA invalidation), or
    /// - the entry has no cached bytecode snapshot (abstract / native).
    ///
    /// Callers that need the descriptive fields too should still use
    /// [`VtableManager::resolve_virtual_slot`] or
    /// [`VtableManager::vtable_entry_ref`].
    #[inline]
    pub fn resolve_virtual_slot_method(
        &self,
        class_id: u64,
        slot: usize,
    ) -> Option<Arc<cratonvm_jit_api::CachedBytecodeMethod>> {
        self.vtable_entry_ref(class_id, slot)
            .and_then(|e| e.resolved_method.clone())
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
    ///
    /// ## Known cost: this is a permanent, never-retried negative
    ///
    /// Audited 2026-07-26 (`stackwalk-and-vtable`). `resolved = false` is a
    /// one-way door: nothing in this module ever sets it back to `true` except
    /// a wholesale `install_vtable` / `add_method` / `override_method` for that
    /// same class, which only happens when *that* class is (re)linked. So the
    /// first subclass to override `Foo.bar()` disables the interpreter's
    /// vtable fast path for `(Foo, bar)` **for the life of the process** — and
    /// since essentially every class overrides `toString`/`equals`/`hashCode`,
    /// those slots are disabled on their declaring classes almost immediately
    /// after boot.
    ///
    /// It is not a correctness bug: the interpreter looks the vtable up by the
    /// *receiver object's own* class id, so `Foo`'s entry is only ever
    /// consulted for a receiver that really is a `Foo`, for which `Foo.bar` is
    /// the correct target. The invalidation is a CHA/`LeafClass`-assumption
    /// signal that costs the interpreter its fast path — and the JIT, which is
    /// the assumption's intended consumer, does not read `VtableManager` at
    /// all (no `jit/**` reference to any of these APIs exists today).
    ///
    /// Deliberately **not** changed in this pass: separating "CHA assumption
    /// broken" from "entry undispatchable" changes dispatch-tier semantics,
    /// and this pass cannot build or run the suites. Written up as a scoped
    /// proposal in
    /// `stackwalk-and-vtable.md`.
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

    /// Release the dispatch tables owned by an unloaded class after first
    /// invalidating inherited copies of entries it declared.
    ///
    /// **Prefer [`VtableManager::unload_classes`] when unloading a batch.**
    /// `invalidate_class` sweeps every slot of every vtable in the VM, so
    /// calling this in a loop over a loader's classes is
    /// `O(unloaded × all_classes × slots_per_class)` — for a class-loader
    /// unload of a few hundred classes in a VM holding tens of thousands, that
    /// is hundreds of millions of slot visits under the manager write lock.
    pub fn unload_class(&mut self, class_id: u64) {
        self.invalidate_class(class_id);
        self.tables.remove(&class_id);
        self.itables.remove(&class_id);
    }

    /// Batch counterpart to [`VtableManager::unload_class`]: one sweep over all
    /// vtables for the whole set, instead of one sweep per class.
    ///
    /// Semantics are identical to calling `unload_class` for each id — every
    /// inherited copy of an entry declared by any of `class_ids` is
    /// invalidated, then the unloaded classes' own tables are dropped.
    pub fn unload_classes(&mut self, class_ids: &[u64]) {
        if class_ids.is_empty() {
            return;
        }
        let dead: std::collections::HashSet<u64> = class_ids.iter().copied().collect();
        for vtable in self.tables.values_mut() {
            for entry in vtable.entries.iter_mut().flatten() {
                if dead.contains(&entry.declaring_class_id) {
                    entry.resolved = false;
                }
            }
        }
        for id in &dead {
            self.tables.remove(id);
            self.itables.remove(id);
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
// KNOWN MULTI-VM BLOCKER (P0 — `docs/architecture/per-vm-state.md`, item V1).
//
// An earlier version of this comment said a second VM would merely "lose the
// race" for the `set`. That understates it. `ClassId`s are allocated PER VM
// (`ClassStore::next_id` returns `self.classes.len()`, and each `SharedVm` owns
// its own `ClassStore`), so with two VMs live:
//
//   * the second VM's `install_global_vtable_manager` is silently ignored, and
//   * `vtable_install_adapter` — a captureless `fn(u32, Vec<…>)` handed to the
//     classloading crate, with no VM parameter — writes the SECOND VM's vtables
//     into the FIRST VM's manager under colliding numeric ids.
//
// The result is not "one VM owns the stream": it is two VMs' virtual-dispatch
// tables interleaved in one index, so `resolve_virtual_slot` can hand a caller
// in VM B a `CachedBytecodeMethod` compiled from VM A's class. That is silent
// wrong dispatch, and it is why "100 concurrent VMs" cannot be claimed today.
//
// Unlike the redefine/JIT-invalidation hooks (see
// `vm_init::live_hook_vms`), this one CANNOT be fixed by fanning out to every
// live VM: invalidation is idempotent and safe to over-apply, whereas vtable
// INSTALLATION is a write of authoritative state and fanning it out would
// corrupt every other VM rather than merely over-invalidate them.
//
// The fix requires a VM parameter on `cratonvm_classloading::VtableInstallHook`
// (or a current-VM thread-local established around class loading), both of
// which live outside this crate. Until then this global is correct for exactly
// one VM per process and unsound for two.
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

/// Adapter that satisfies `cratonvm_classloading::VtableInstallHook`.
///
/// Converts each `VtableSlotDescriptor` into a `VtableEntry` and writes
/// the whole vec into the global `VtableManager` via `install_vtable`.
/// A single write lock is taken per class; concurrent readers calling
/// `resolve_virtual_slot` are unaffected because the read lock is free
/// while the install lock is held only for the index rebuild.
pub fn vtable_install_adapter(
    class_id: u32,
    entries: Vec<Option<cratonvm_classloading::VtableSlotDescriptor>>,
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
                        // `snap.code: Arc<[u8]>` (round 4 — was `Vec<u8>`).
                        // We still need a +2-byte zero-padded copy for the
                        // interpreter's speculative `code[pc+1/+2]` reads,
                        // so build one fresh `Arc<[u8]>` here. The
                        // previous implementation also did a fresh alloc,
                        // and `snap.code` itself was a Vec built from a
                        // Vec; now `snap.code` is the reader's zero-copy
                        // shared-buffer slice, so the producer side
                        // (`build_vtable_descriptors_with_overrides`)
                        // dropped one Vec alloc + memcpy.
                        let padded = {
                            let mut v = Vec::with_capacity(snap.code.len() + 2);
                            v.extend_from_slice(&snap.code);
                            v.push(0);
                            v.push(0);
                            Arc::<[u8]>::from(v.into_boxed_slice())
                        };
                        let cached = Arc::new(cratonvm_jit_api::CachedBytecodeMethod {
                            declaring_class_id: crate::classloading::ClassId::new(
                                d.declaring_class_id,
                            ),
                            class_name: Arc::<str>::from(snap.class_name.as_str()),
                            method_name: Arc::clone(&d.method_name),
                            method_descriptor: Arc::clone(&d.descriptor),
                            source_file: snap.source_file.as_deref().map(Arc::<str>::from),
                            code: padded,
                            exception_table: Arc::<[_]>::from(snap.exception_table.as_slice()),
                            max_stack: snap.max_stack,
                            max_locals: snap.max_locals,
                            num_params: snap.num_params,
                            is_synchronized: snap.is_synchronized,
                            is_static: snap.is_static,
                            force_native_cache: std::sync::OnceLock::new(),
                            descriptor_facts_cache: std::sync::OnceLock::new(),
                            intercept_shape_cache: std::sync::OnceLock::new(),
                            interp_invocations: std::sync::atomic::AtomicU32::new(0),
                            tiering_settled: std::sync::atomic::AtomicU32::new(0),
                            native_callback_cache: std::sync::OnceLock::new(),
                            invoc_key: std::sync::OnceLock::new(),
                            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
                            quickened: std::sync::OnceLock::new(),
                        });
                        (Some(cached), false)
                    }
                    Some(snap) => (None, snap.is_native),
                    None => (None, false),
                };
                VtableEntry {
                    declaring_class_id: d.declaring_class_id as u64,
                    method_index: d.method_index,
                    // HIGH-5 — descriptor's `method_name`/`descriptor` are
                    // already `Arc<str>` (T10.9.E classloading conversion);
                    // moving them in here is a single refcount transfer.
                    method_name: d.method_name,
                    descriptor: d.descriptor,
                    resolved: true,
                    resolved_method,
                    is_native,
                }
            })
        })
        .collect();
    manager.write().install_vtable(class_id as u64, converted);
}

/// T10.9.A — adapter that satisfies `cratonvm_classloading::VtableOverrideHook`.
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
    manager
        .write()
        .invalidate_for_override(super_class_id as u64, slot);
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
        assert_eq!(
            child.lookup_slot("toString", "()Ljava/lang/String;"),
            Some(0)
        );
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

    // -- 2026-07-26 arch pass: single-index + inline bucket ---------------

    fn probe_entry(name: &str, descriptor: &str, declaring: u64, idx: u32) -> VtableEntry {
        VtableEntry {
            declaring_class_id: declaring,
            method_index: idx,
            method_name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            resolved: true,
            resolved_method: None,
            is_native: false,
        }
    }

    #[test]
    fn slot_bucket_promotes_to_many_only_on_collision() {
        let mut b = SlotBucket::One(3);
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![3]);
        b.push(3); // idempotent — must not allocate a Vec
        assert_eq!(b, SlotBucket::One(3));
        b.push(9);
        assert_eq!(b, SlotBucket::Many(vec![3, 9]));
        b.push(9);
        assert_eq!(b, SlotBucket::Many(vec![3, 9]));
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![3, 9]);
    }

    #[test]
    fn add_method_override_still_reuses_the_slot_without_name_to_slot() {
        // The `name_to_slot` map that used to answer this question is gone;
        // `lookup_slot` must give the identical answer.
        let mut vt = Vtable::new(1);
        assert_eq!(vt.add_method("m", "(I)V", 1, 0), 0);
        assert_eq!(vt.add_method("m", "(J)V", 1, 1), 1); // overload: new slot
        assert_eq!(vt.add_method("m", "(I)V", 2, 7), 0); // same sig: override
        assert_eq!(vt.len(), 2);
        let e = vt.get(0).unwrap();
        assert_eq!(e.declaring_class_id, 2);
        assert_eq!(e.method_index, 7);
        assert_eq!(vt.lookup_slot("m", "(I)V"), Some(0));
        assert_eq!(vt.lookup_slot("m", "(J)V"), Some(1));
    }

    #[test]
    fn install_vtable_indexes_every_slot_and_lookups_verify_exactly() {
        let mut mgr = VtableManager::new();
        mgr.install_vtable(
            7,
            vec![
                Some(probe_entry("run", "()V", 7, 0)),
                None, // an intentionally empty slot must not break the index
                Some(probe_entry("run", "(I)V", 7, 1)),
                Some(probe_entry("stop", "()V", 7, 2)),
            ],
        );
        let vt = mgr.get_vtable(7).unwrap();
        assert_eq!(vt.lookup_slot("run", "()V"), Some(0));
        assert_eq!(vt.lookup_slot("run", "(I)V"), Some(2));
        assert_eq!(vt.lookup_slot("stop", "()V"), Some(3));
        assert_eq!(vt.lookup_slot("stop", "(I)V"), None);
        assert_eq!(vt.lookup_slot("nope", "()V"), None);
    }

    #[test]
    fn reinstall_refreshes_inherited_entries_without_touching_overrides() {
        let mut mgr = VtableManager::new();
        mgr.install_vtable(
            1,
            vec![
                Some(probe_entry("inherited", "()I", 1, 0)),
                Some(probe_entry("overridden", "()I", 1, 1)),
            ],
        );
        mgr.install_vtable(
            2,
            vec![
                Some(probe_entry("inherited", "()I", 1, 0)),
                Some(probe_entry("overridden", "()I", 2, 9)),
            ],
        );

        // Reinstalling an existing class id is the vtable side of JVMTI
        // redefine. The child's inherited snapshot must move to the new
        // method metadata while its own override remains untouched.
        mgr.install_vtable(
            1,
            vec![
                Some(probe_entry("inherited", "()I", 1, 7)),
                Some(probe_entry("overridden", "()I", 1, 8)),
            ],
        );

        let inherited = mgr.resolve_virtual(2, "inherited", "()I").unwrap();
        assert_eq!(inherited.declaring_class_id, 1);
        assert_eq!(inherited.method_index, 7);

        let overridden = mgr.resolve_virtual(2, "overridden", "()I").unwrap();
        assert_eq!(overridden.declaring_class_id, 2);
        assert_eq!(overridden.method_index, 9);
    }

    #[test]
    fn from_parent_inherits_the_signature_index() {
        let mut parent = Vtable::new(1);
        parent.add_method("a", "()V", 1, 0);
        parent.add_method("b", "()V", 1, 1);
        let mut child = Vtable::from_parent(2, &parent);
        assert_eq!(child.lookup_slot("b", "()V"), Some(1));
        // Overriding through the inherited index must land on the same slot.
        assert_eq!(child.add_method("b", "()V", 2, 42), 1);
        assert_eq!(child.get(1).unwrap().declaring_class_id, 2);
        // The parent's own table is untouched.
        assert_eq!(parent.get(1).unwrap().declaring_class_id, 1);
    }

    #[test]
    fn unload_classes_matches_a_loop_of_unload_class() {
        let build = || {
            let mut mgr = VtableManager::new();
            // Class 1 declares `a`; classes 2 and 3 inherit that entry.
            for cid in [1u64, 2, 3] {
                mgr.install_vtable(
                    cid,
                    vec![
                        Some(probe_entry("a", "()V", 1, 0)),
                        Some(probe_entry("b", "()V", cid, 1)),
                    ],
                );
            }
            mgr
        };

        let mut looped = build();
        looped.unload_class(1);
        let mut batched = build();
        batched.unload_classes(&[1]);

        assert!(looped.get_vtable(1).is_none());
        assert!(batched.get_vtable(1).is_none());
        for cid in [2u64, 3] {
            // The inherited `a` entry (declared by the unloaded class 1) is
            // invalidated in both; the class's own `b` stays resolved.
            assert!(looped.vtable_entry_ref(cid, 0).is_none());
            assert!(batched.vtable_entry_ref(cid, 0).is_none());
            assert!(looped.vtable_entry_ref(cid, 1).is_some());
            assert!(batched.vtable_entry_ref(cid, 1).is_some());
        }
    }

    #[test]
    fn unload_classes_is_a_noop_on_an_empty_batch() {
        let mut mgr = VtableManager::new();
        mgr.install_vtable(1, vec![Some(probe_entry("a", "()V", 1, 0))]);
        mgr.unload_classes(&[]);
        assert!(mgr.vtable_entry_ref(1, 0).is_some());
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
        assert!(vt2.get(1).unwrap().resolved); // bar overridden by class 2
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
        it.register(50, "run", "()V", 0); // Runnable
        it.register(51, "call", "()Ljava/lang/Object;", 1); // Callable
        it.register(52, "compare", "(Ljava/lang/Object;Ljava/lang/Object;)I", 2); // Comparator

        assert!(mgr.resolve_interface(10, 50, "run", "()V").is_some());
        assert!(mgr
            .resolve_interface(10, 51, "call", "()Ljava/lang/Object;")
            .is_some());
        assert!(mgr
            .resolve_interface(10, 52, "compare", "(Ljava/lang/Object;Ljava/lang/Object;)I")
            .is_some());
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

        let eref = mgr
            .vtable_entry_ref(5, 0)
            .expect("slot should be populated");
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
    ) -> Arc<cratonvm_jit_api::CachedBytecodeMethod> {
        Arc::new(cratonvm_jit_api::CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(declaring_class_id),
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
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            tiering_settled: std::sync::atomic::AtomicU32::new(0),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
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
        assert!(mgr
            .resolve_virtual_slot(1, 0)
            .unwrap()
            .resolved_method
            .is_some());

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

    /// **THIS TEST CANNOT FAIL, and its name overstates what it covers.**
    ///
    /// The property the name claims — "when `vtable_install_adapter` sees a
    /// `VtableSlotDescriptor` with `dispatch: None`, the resulting
    /// `VtableEntry` has `resolved_method: None`" — is NOT asserted here.
    /// The body constructs a descriptor and then asserts `dispatch.is_none()`
    /// and `method_index == 3`, which are the two fields the four lines above
    /// just set. `vtable_install_adapter` is never called. Make the adapter
    /// hand a `dispatch: None` descriptor a `resolved_method: Some(..)` —
    /// i.e. dispatch an abstract method to a body — and this test does not
    /// move.
    ///
    /// It is left in place rather than deleted because what it constructs is
    /// still a live compile-time check that the `VtableSlotDescriptor` shape
    /// the adapter consumes has not drifted. The behavioural half needs a
    /// production change and is recorded as a residual:
    ///
    /// * The conversion lives inside a closure in `vtable_install_adapter`,
    ///   with no separately callable entry point, so the only route to it is
    ///   the adapter itself.
    /// * The adapter reaches its manager through `GLOBAL_VTABLE_MANAGER`, a
    ///   `OnceLock` that is documented as "one VM per process" and that
    ///   `MemberResolver::vtable_manager` uses (via `Arc::ptr_eq`) to decide
    ///   whether a manager is foreign. A unit test that installed its own
    ///   manager would latch that cell for the whole `vm` lib-test binary and
    ///   make every `SharedVm` in it report `ForeignVtableManager`.
    ///
    /// The repair is to lift the descriptor -> entry conversion out of the
    /// closure into a standalone function that `vtable_install_adapter` calls,
    /// so it can be tested with no manager and no global at all.
    #[test]
    fn t10_9_a_adapter_preserves_empty_dispatch() {
        // Shape-only: this is a compile-time check on `VtableSlotDescriptor`,
        // NOT a check on the adapter. See the doc comment above.
        let desc = cratonvm_classloading::VtableSlotDescriptor {
            declaring_class_id: 77,
            method_index: 3,
            method_name: Arc::<str>::from("abstr"),
            descriptor: Arc::<str>::from("()I"),
            dispatch: None, // abstract method — no snapshot
        };
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

    /// Round-2 VM §8 HIGH — `resolve_virtual_slot_method` returns the
    /// same `Arc<CachedBytecodeMethod>` payload the full
    /// `resolve_virtual_slot` would carry, but as a bare Arc clone so
    /// the hot path skips two needless refcount bumps on
    /// `method_name` / `descriptor`.
    #[test]
    fn resolve_virtual_slot_method_returns_cached_arc() {
        let mut mgr = VtableManager::new();
        let stub = make_dispatch_stub(42, "tick", "()V");
        let stub_weak = Arc::downgrade(&stub);
        mgr.install_vtable(
            42,
            vec![Some(VtableEntry {
                declaring_class_id: 42,
                method_index: 0,
                method_name: Arc::<str>::from("tick"),
                descriptor: Arc::<str>::from("()V"),
                resolved: true,
                resolved_method: Some(stub),
                is_native: false,
            })],
        );

        // Slot-indexed helper returns just the bytecode Arc.
        let got = mgr
            .resolve_virtual_slot_method(42, 0)
            .expect("slot populated and resolved");
        assert!(
            stub_weak.upgrade().is_some(),
            "vtable should still own the installed Arc",
        );
        assert_eq!(&*got.method_name, "tick");
        assert_eq!(&*got.method_descriptor, "()V");

        // Name-based helper does the same for the slow path.
        let got_named = mgr
            .resolve_virtual_method(42, "tick", "()V")
            .expect("named lookup hits");
        assert!(Arc::ptr_eq(&got, &got_named));

        // Unresolved → None for both variants.
        mgr.invalidate_for_override(42, 0);
        assert!(mgr.resolve_virtual_slot_method(42, 0).is_none());
        assert!(mgr.resolve_virtual_method(42, "tick", "()V").is_none());
    }

    /// Round-2 VM §8 HIGH — a vtable entry with no cached bytecode
    /// (abstract / native) returns `None` from the `_method` helpers
    /// even when the slot itself is resolved.
    #[test]
    fn resolve_virtual_method_none_for_native_slot() {
        let mut mgr = VtableManager::new();
        mgr.install_vtable(
            88,
            vec![Some(VtableEntry {
                declaring_class_id: 88,
                method_index: 0,
                method_name: Arc::<str>::from("getClass"),
                descriptor: Arc::<str>::from("()Ljava/lang/Class;"),
                resolved: true,
                resolved_method: None, // native — no bytecode
                is_native: true,
            })],
        );
        assert!(mgr.resolve_virtual_slot_method(88, 0).is_none());
        assert!(mgr
            .resolve_virtual_method(88, "getClass", "()Ljava/lang/Class;")
            .is_none(),);
        // The full-entry variant still returns Some(..) because the
        // slot is resolved — callers that need to detect the native
        // case rely on `is_native` from the full entry.
        let full = mgr.resolve_virtual_slot(88, 0).unwrap();
        assert!(full.is_native);
        assert!(full.resolved_method.is_none());
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
