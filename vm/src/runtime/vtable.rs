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
use std::sync::Arc;

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

/// The signature index of one vtable: [`fast_lookup_key`] -> candidate slots.
///
/// PERF (2026-07-26 arch pass). The bucket used to be a `Vec<usize>`, i.e. a
/// **heap allocation per distinct method signature per class**; FxHash
/// collisions between two `(name, descriptor)` pairs are vanishingly rare, so
/// a bucket holds one slot inline and only a collision allocates.
///
/// zs s3/fp3 (2026-10-14, footprint): the inline bucket was a 24-byte enum
/// (`One(u32)` / `Many(Vec<u32>)`) under a `u64` key, so a hash slot cost 32
/// bytes. A slot is now 8: the key is the low half of [`fast_lookup_key`]
/// ([`sig_key`]), and the value is the slot number itself, or [`SIG_MANY`]
/// plus an index into `many`, the per-index list of colliding buckets. A
/// shorter key only adds candidates, which the caller verifies one by one in
/// slot order as before; the candidates with the right signature are the same
/// slots in the same order, so the first verified one, the answer, is too.
/// The index is immutable once built and shared (`Arc`) by every table with
/// the same signature layout: see [`VtableManager::install_vtable_shared`].
#[derive(Clone, Debug, Default, PartialEq)]
struct SigIndex {
    map: FxHashMap<u32, u32>,
    many: Vec<Vec<u32>>,
}

/// zs s3/fp3: the [`SigIndex`] key of a [`fast_lookup_key`] (its low half).
#[inline]
fn sig_key(key: u64) -> u32 {
    key as u32
}

/// Tag bit of a [`SigIndex`] value naming a collision list, not a slot.
const SIG_MANY: u32 = 1 << 31;

impl SigIndex {
    fn with_capacity(n: usize) -> Self {
        SigIndex {
            map: FxHashMap::with_capacity_and_hasher(n, Default::default()),
            many: Vec::new(),
        }
    }

    /// Add `slot` to `key`'s bucket (idempotent, keeps insertion order).
    fn push(&mut self, key: u64, slot: u32) {
        debug_assert!(slot < SIG_MANY, "vtable slot {slot} overflows the index");
        match self.map.entry(sig_key(key)) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(slot);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let cur = *o.get();
                if cur & SIG_MANY != 0 {
                    let list = &mut self.many[(cur & !SIG_MANY) as usize];
                    if !list.contains(&slot) {
                        list.push(slot);
                    }
                } else if cur != slot {
                    let idx = self.many.len() as u32;
                    self.many.push(vec![cur, slot]);
                    *o.get_mut() = SIG_MANY | idx;
                }
            }
        }
    }

    /// The candidate slots of `key`, in insertion order.
    #[inline]
    fn get(&self, key: u64) -> Option<&[u32]> {
        let v = self.map.get(&sig_key(key))?;
        Some(if v & SIG_MANY != 0 {
            self.many[(v & !SIG_MANY) as usize].as_slice()
        } else {
            std::slice::from_ref(v)
        })
    }

    /// The index of `entries`: every populated slot under its signature.
    fn build(entries: &[Option<Arc<VtableEntry>>]) -> Self {
        let mut index = SigIndex::with_capacity(entries.len());
        for (slot, entry) in entries.iter().enumerate() {
            if let Some(e) = entry {
                index.push(fast_lookup_key(&e.method_name, &e.descriptor), slot as u32);
            }
        }
        index
    }

    /// Estimated heap bytes (footprint census only).
    fn heap_bytes(&self) -> usize {
        hash_table_bytes(self.map.capacity(), std::mem::size_of::<(u32, u32)>())
            + self.many.capacity() * std::mem::size_of::<Vec<u32>>()
            + self.many.iter().map(|v| v.capacity() * 4).sum::<usize>()
    }
}

/// Estimated bytes of a hashbrown table with `capacity` usable slots of
/// `entry` bytes (the classloading census's rule,
/// `cratonvm_classloading::footprint::hash_table_bytes`): next power of two at
/// or above 8/7 of the capacity, entry + one control byte each, plus a
/// 16-byte trailing group. Diagnostics only.
fn hash_table_bytes(capacity: usize, entry: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = capacity
        .saturating_mul(8)
        .div_ceil(7)
        .max(4)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    buckets
        .saturating_mul(entry.saturating_add(1))
        .saturating_add(16)
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
    ///
    /// zs s3/fp3 (2026-10-14, footprint): each slot is a shared handle. An
    /// inherited slot whose entry equals its declaring class's own entry
    /// shares that class's block (`install_vtable_descriptors`), so a slot
    /// costs 8 bytes here and 72 once per declared method, where it cost a
    /// 56-byte copy per class. The blocks are immutable while shared: the
    /// one mutable field (`resolved`) is written through `Arc::make_mut`,
    /// which gives the written table its own copy, exactly as when every
    /// table held a private one.
    entries: Vec<Option<Arc<VtableEntry>>>,
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
    ///
    /// zs s3/fp3: shared (`Arc`) between tables with the same signature
    /// layout, and copied on write by the test-only `add_method`.
    fast_lookup: Arc<SigIndex>,
}

impl Vtable {
    /// Create an empty vtable for the given class.
    pub fn new(class_id: u64) -> Self {
        Vtable {
            class_id,
            entries: Vec::new(),
            fast_lookup: Arc::default(),
        }
    }

    /// Create a vtable that inherits all entries from a parent vtable.
    pub fn from_parent(class_id: u64, parent: &Vtable) -> Self {
        Vtable {
            class_id,
            entries: parent.entries.clone(),
            fast_lookup: Arc::clone(&parent.fast_lookup),
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
            self.entries[slot] = Some(Arc::new(VtableEntry {
                declaring_class_id,
                method_index,
                method_name: name_arc,
                descriptor: desc_arc,
                resolved: true,
                resolved_method: None,
                is_native: false,
            }));
            return slot;
        }

        // Otherwise append a new slot.
        let slot = self.entries.len();
        self.entries.push(Some(Arc::new(VtableEntry {
            declaring_class_id,
            method_index,
            method_name: name_arc,
            descriptor: desc_arc,
            resolved: true,
            resolved_method: None,
            is_native: false,
        })));
        // HIGH-6 — keep the u64 fast-lookup map in sync (copy on write: the
        // index may be shared with the parent's table).
        Arc::make_mut(&mut self.fast_lookup).push(fast_lookup_key(name, descriptor), slot as u32);
        slot
    }

    /// Override an existing entry at the given slot index.
    pub fn override_method(&mut self, slot: usize, declaring_class_id: u64, method_index: u32) {
        if let Some(Some(entry)) = self.entries.get_mut(slot) {
            let entry = Arc::make_mut(entry);
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
        let candidates = self.fast_lookup.get(key)?;
        for &slot in candidates {
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
        self.entries.get(slot).and_then(|e| e.as_deref())
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
            // zs s3/fp3: skip the copy when the bit is already clear; the
            // written value is the same either way.
            if entry.resolved {
                Arc::make_mut(entry).resolved = false;
            }
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
    /// `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE` (default OFF), read once per
    /// manager (one per VM): a class whose every slot is inherited takes a
    /// copy of the table of the class that declared its last slot, when that
    /// table holds exactly the entries the per-slot conversion would build,
    /// instead of converting slot by slot and rebuilding the signature index
    /// ([`Self::inherited_template`]). Interpreter round i4 wave 95, lane L5
    /// (`i87-L4-proposal-define-and-first-instance-cost`, the
    /// `register-vtable` row; census rows `install-convert` /
    /// `install-table`).
    inherit_template: bool,
    /// zt t2/fp (`CRATONVM_VTABLE_LAZY_DISPATCH`, default OFF), read once per
    /// manager (one per VM): a declared bytecode method's dispatch snapshot
    /// (`CachedBytecodeMethod` + padded bytecode) is built on the first
    /// dispatch through one of its slots instead of at link. See
    /// [`LazyDispatch`].
    lazy_dispatch: bool,
    /// The deferred snapshots, one per declared bytecode method, keyed by
    /// [`lazy_key`]`(declaring class, method index)`. Empty unless
    /// `lazy_dispatch`.
    lazy: FxHashMap<u64, LazyDispatch>,
    /// Snapshots this manager deferred at link and how many of those a
    /// dispatch built since (diagnostic only: [`lazy_dispatch_line`]). Per
    /// manager, so per VM (zt t6/rv3: they were process statics summed over
    /// every VM).
    lazy_deferred: std::sync::atomic::AtomicU64,
    lazy_built: std::sync::atomic::AtomicU64,
}

impl VtableManager {
    /// Create a new empty manager.
    pub fn new() -> Self {
        Self::with_lazy_dispatch(cratonvm_types::flags::runtime_flag_on(
            "CRATONVM_VTABLE_LAZY_DISPATCH",
        ))
    }

    /// [`Self::new`] with the `CRATONVM_VTABLE_LAZY_DISPATCH` decision made by
    /// the caller (a test builds both arms in one process).
    pub fn with_lazy_dispatch(lazy_dispatch: bool) -> Self {
        VtableManager {
            tables: FxHashMap::default(),
            itables: HashMap::new(),
            inherit_template: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE",
            ),
            lazy_dispatch,
            lazy: FxHashMap::default(),
            lazy_deferred: std::sync::atomic::AtomicU64::new(0),
            lazy_built: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The dispatch snapshot of `entry` (a slot of one of this manager's
    /// tables): the one built at link, or, under
    /// `CRATONVM_VTABLE_LAZY_DISPATCH`, the declaring method's deferred one,
    /// built here on first use. `None` for an abstract or native slot, as
    /// `entry.resolved_method` is.
    ///
    /// Readers of a dispatch target go through this, never through
    /// `resolved_method` alone: under the switch a bytecode slot's
    /// `resolved_method` is `None` until then.
    #[inline]
    pub fn entry_dispatch<'a>(
        &'a self,
        entry: &'a VtableEntry,
    ) -> Option<&'a Arc<cratonvm_jit_api::CachedBytecodeMethod>> {
        if let Some(built) = entry.resolved_method.as_ref() {
            return Some(built);
        }
        if !self.lazy_dispatch || entry.is_native {
            return None;
        }
        self.lazy_dispatch_for(entry.declaring_class_id, entry.method_index)
    }

    /// The deferred snapshot of `(declaring_class_id, method_index)`, built
    /// on its first request. Under the vtable manager's read guard, as every
    /// caller holds it: the build reads only the loader's snapshot (no class
    /// manager lock) and the padded-bytecode memo (a leaf lock), and
    /// `OnceLock` publishes exactly one result however many threads race.
    #[cold]
    fn lazy_dispatch_for(
        &self,
        declaring_class_id: u64,
        method_index: u32,
    ) -> Option<&Arc<cratonvm_jit_api::CachedBytecodeMethod>> {
        let lazy = self.lazy.get(&lazy_key(declaring_class_id, method_index))?;
        let snap = lazy.desc.dispatch.as_ref()?;
        Some(lazy.built.get_or_init(|| {
            self.lazy_built.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            build_dispatch_snapshot(&lazy.desc, snap)
        }))
    }

    /// zt t2/fp: under `CRATONVM_VTABLE_LAZY_DISPATCH`, defer the snapshot of
    /// slot `d` (a non-native bytecode method, `snap`) of the class `class_id`
    /// being installed. Returns whether it was deferred; the caller then
    /// installs the entry with no `resolved_method`.
    ///
    /// - A slot the class declares records the method's [`LazyDispatch`]. A
    ///   re-install that carries the very snapshot already recorded (a
    ///   catch-up replay) keeps the cell, and with it a snapshot already
    ///   built; a new snapshot (link, redefinition) gets a fresh cell.
    /// - An inherited slot is deferred only when the declaring method's
    ///   recorded cell holds the same snapshot (`Arc` identity), so every slot
    ///   of one method resolves to one snapshot and one set of interpreter
    ///   counters, as the eager path's sharing does. Anything else is built
    ///   at link exactly as before.
    fn defer_dispatch(
        &mut self,
        class_id: u32,
        d: &cratonvm_classloading::VtableSlotDescriptor,
        snap: &Arc<cratonvm_classloading::VtableMethodSnapshot>,
    ) -> bool {
        if !self.lazy_dispatch {
            return false;
        }
        let key = lazy_key(d.declaring_class_id as u64, d.method_index);
        let recorded = self
            .lazy
            .get(&key)
            .and_then(|lazy| lazy.desc.dispatch.as_ref())
            .is_some_and(|have| Arc::ptr_eq(have, snap));
        if d.declaring_class_id != class_id {
            return recorded;
        }
        if !recorded {
            self.lazy.insert(
                key,
                LazyDispatch {
                    desc: d.clone(),
                    built: std::sync::OnceLock::new(),
                },
            );
            self.lazy_deferred.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        true
    }

    /// zt t2/fp: drop the deferred snapshots of methods declared by `dead`
    /// classes (an unload): their ids may be reused, and the cells pin the
    /// class-file buffers their snapshots view.
    fn forget_lazy_dispatch(&mut self, dead: impl Fn(u64) -> bool) {
        if self.lazy.is_empty() {
            return;
        }
        self.lazy.retain(|&key, _| !dead(key >> 32));
    }

    /// zt t6/rv3 (`zgc-zt3x2-*` item 3): a re-install of `class_id` (a
    /// redefinition, or a replay) keeps only the cells of the methods its new
    /// descriptor vector still declares at the same index (`declared`). A
    /// redefinition that renumbers a method records a new key in
    /// [`Self::defer_dispatch`]; the old key's cell, its built snapshot and
    /// the class-file buffer it views stayed until the class unloaded. A
    /// replay declares the same indices, so its cells (and their built
    /// snapshots) stay.
    fn forget_undeclared_lazy_dispatch(&mut self, class_id: u64, declared: &[u32]) {
        if self.lazy.is_empty() {
            return;
        }
        self.lazy.retain(|&key, _| key >> 32 != class_id || declared.contains(&(key as u32)));
    }

    /// The table a class whose descriptor vector is `entries` would get from
    /// [`install_vtable_descriptors`]'s per-slot conversion, as a copy of an
    /// installed table, with its signature index, when one provably equals it
    /// (`CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE`): every slot of `entries` is
    /// inherited (no slot declared by `class_id`, none empty), the template is
    /// the table of the class that declared the LAST slot (the shortest
    /// ancestor table that can be as long), it has the same length, and slot
    /// by slot it holds the same declaring class, method index, name,
    /// descriptor and nativeness, resolved, with the very dispatch snapshot
    /// the conversion would share ([`Self::inherited_dispatch`], compared by
    /// identity). `None` sends the caller down the per-slot conversion
    /// unchanged: the switch off, a redefinition (`class_id` already has a
    /// table), any slot the conversion would build a fresh snapshot for, or
    /// any difference.
    fn inherited_template(
        &self,
        class_id: u64,
        entries: &[Option<cratonvm_classloading::VtableSlotDescriptor>],
    ) -> Option<(Vec<Option<Arc<VtableEntry>>>, Arc<SigIndex>)> {
        if !self.inherit_template || self.tables.contains_key(&class_id) {
            return None;
        }
        let last = entries.last()?.as_ref()?;
        let template = self.tables.get(&(last.declaring_class_id as u64))?;
        if template.class_id == class_id || template.entries.len() != entries.len() {
            return None;
        }
        for (slot, (wanted, have)) in entries.iter().zip(template.entries.iter()).enumerate() {
            let (Some(wanted), Some(have)) = (wanted, have) else {
                return None;
            };
            let declaring = wanted.declaring_class_id as u64;
            if declaring == class_id
                || !have.resolved
                || have.declaring_class_id != declaring
                || have.method_index != wanted.method_index
                || have.method_name != wanted.method_name
                || have.descriptor != wanted.descriptor
            {
                return None;
            }
            match wanted.dispatch.as_ref() {
                // The conversion's `(None, false)`.
                None => {
                    if have.resolved_method.is_some() || have.is_native {
                        return None;
                    }
                }
                // `(None, true)`.
                Some(snap) if snap.is_native => {
                    if have.resolved_method.is_some() || !have.is_native {
                        return None;
                    }
                }
                // The declaring class's own snapshot, shared; the conversion
                // builds a fresh one when there is none (not shared here).
                Some(snap) => {
                    let shared = self.inherited_dispatch(
                        declaring,
                        slot,
                        &snap.class_name,
                        &wanted.method_name,
                        &wanted.descriptor,
                    );
                    let same = match shared {
                        Some(shared) => have
                            .resolved_method
                            .as_ref()
                            .is_some_and(|m| Arc::ptr_eq(m, &shared)),
                        // zt t6/rv3 (`zgc-zt3x2-*` item 4): under
                        // `CRATONVM_VTABLE_LAZY_DISPATCH` the declaring slot
                        // is deferred (no `resolved_method`), and the
                        // conversion defers this slot too exactly when the
                        // method's recorded cell holds this very snapshot
                        // (`defer_dispatch`'s inherited arm). Accept the
                        // same case, so the two switches compose.
                        None => {
                            self.lazy_dispatch
                                && have.resolved_method.is_none()
                                && self
                                    .lazy
                                    .get(&lazy_key(declaring, wanted.method_index))
                                    .and_then(|lazy| lazy.desc.dispatch.as_ref())
                                    .is_some_and(|recorded| Arc::ptr_eq(recorded, snap))
                        }
                    };
                    if !same || have.is_native {
                        return None;
                    }
                }
            }
        }
        // zs s3/fp3: the copy shares the template's entry blocks and its
        // signature index (`Arc` handles, as `install_vtable_shared` does).
        Some((template.entries.clone(), Arc::clone(&template.fast_lookup)))
    }

    /// [`Self::install_vtable`] for a NEW class (no table yet) whose signature
    /// index is already built ([`Self::inherited_template`]).
    fn install_new_vtable_with_index(
        &mut self,
        class_id: u64,
        entries: Vec<Option<Arc<VtableEntry>>>,
        fast_lookup: Arc<SigIndex>,
    ) {
        debug_assert!(!self.tables.contains_key(&class_id));
        self.tables.insert(
            class_id,
            Vtable {
                class_id,
                entries,
                fast_lookup,
            },
        );
    }

    /// Create a vtable for a class, optionally inheriting from a parent.
    /// Returns a mutable reference to the newly created vtable.
    pub fn create_vtable(&mut self, class_id: u64, parent_class_id: Option<u64>) -> &mut Vtable {
        let vtable = match parent_class_id {
            Some(pid) => {
                // Clone the parent vtable entries so we can insert without borrow conflict.
                let parent_entries: Vec<Option<Arc<VtableEntry>>> = self
                    .tables
                    .get(&pid)
                    .map(|p| p.entries.clone())
                    .unwrap_or_default();
                let parent_fast_lookup: Arc<SigIndex> = self
                    .tables
                    .get(&pid)
                    .map(|p| Arc::clone(&p.fast_lookup))
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
        self.entry_dispatch(entry).cloned()
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
        self.install_vtable_shared(
            class_id,
            entries.into_iter().map(|e| e.map(Arc::new)).collect(),
        );
    }

    /// [`VtableManager::install_vtable`] over shared entry blocks: the link
    /// path's installer (`install_vtable_descriptors`), which hands an
    /// inherited slot its declaring class's own block.
    ///
    /// zs s3/fp3 (2026-10-14, footprint): the signature index is shared too.
    /// A table whose slots carry exactly the signatures of an ancestor's table
    /// (same length, same `(name, descriptor)` or the same empty slot at every
    /// index: a class that only overrides) holds that table's index instead of
    /// building its own. `lookup_slot` reads only the index and each slot's
    /// signature, so its answers are the same either way.
    pub fn install_vtable_shared(&mut self, class_id: u64, entries: Vec<Option<Arc<VtableEntry>>>) {
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
            let mut replacements: FxHashMap<u64, Vec<Arc<VtableEntry>>> =
                FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
            for entry in entries.iter().flatten() {
                if entry.declaring_class_id == class_id {
                    replacements
                        .entry(fast_lookup_key(&entry.method_name, &entry.descriptor))
                        .or_default()
                        .push(Arc::clone(entry));
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
                            // zs s3/fp3: the subclass shares the redefined
                            // class's new block (was a copy of it).
                            *slot = Some(Arc::clone(replacement));
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

        // The signature index, so `lookup_slot` works: an ancestor's when the
        // layout is the same (zs s3/fp3), else built from the entries. One
        // map, one bucket per signature, no allocation per bucket in the
        // (overwhelmingly common) collision-free case — see the `fast_lookup`
        // field doc for the second map this used to build and why it was
        // removed.
        let fast_lookup = self
            .same_layout_index(class_id, &entries)
            .unwrap_or_else(|| Arc::new(SigIndex::build(&entries)));
        let vtable = Vtable {
            class_id,
            entries,
            fast_lookup,
        };
        self.tables.insert(class_id, vtable);
    }

    /// zs s3/fp3: the signature index of an installed ancestor table whose
    /// slots carry exactly `entries`' signatures, if one is found among the
    /// declaring classes of `entries`' inherited slots (the first few distinct
    /// ones, latest slot first: the class that appended the last slot has a
    /// table of the same length). `None` sends the caller to build its own.
    fn same_layout_index(
        &self,
        class_id: u64,
        entries: &[Option<Arc<VtableEntry>>],
    ) -> Option<Arc<SigIndex>> {
        const MAX_CANDIDATES: usize = 4;
        let mut tried = [u64::MAX; MAX_CANDIDATES];
        let mut n_tried = 0usize;
        for entry in entries.iter().rev().flatten() {
            let owner = entry.declaring_class_id;
            if owner == class_id || tried[..n_tried].contains(&owner) {
                continue;
            }
            if n_tried == MAX_CANDIDATES {
                return None;
            }
            tried[n_tried] = owner;
            n_tried += 1;
            let Some(candidate) = self.tables.get(&owner) else {
                continue;
            };
            if candidate.entries.len() != entries.len() {
                continue;
            }
            let same = candidate
                .entries
                .iter()
                .zip(entries)
                .all(|(a, b)| match (a, b) {
                    (None, None) => true,
                    (Some(a), Some(b)) => {
                        Arc::ptr_eq(a, b)
                            || (a.method_name == b.method_name && a.descriptor == b.descriptor)
                    }
                    _ => false,
                });
            if same {
                return Some(Arc::clone(&candidate.fast_lookup));
            }
        }
        None
    }

    /// zs s3/fp3 (2026-10-14): estimated heap bytes of this manager's vtables,
    /// by owner, for the footprint census (`zs3fp3-vm` lines). Shared blocks
    /// (entries, signature indices, dispatch snapshots, padded bytecode) are
    /// counted once each. Diagnostics only: nothing on a dispatch path reads
    /// it. `("tables", n)` is the number of installed tables, not bytes.
    pub fn footprint_census(&self) -> Vec<(&'static str, usize)> {
        use std::collections::HashSet;
        use std::mem::size_of;
        // addr-keyed sets below: every block is held alive by `self`, which
        // is borrowed for the whole walk.
        let mut entry_blocks: HashSet<usize> = HashSet::new(); // addr-keyed: Rust heap blocks owned by `self` (borrowed for the walk), never Java heap.
        let mut index_blocks: HashSet<usize> = HashSet::new(); // addr-keyed: Rust heap blocks owned by `self` (borrowed for the walk), never Java heap.
        let mut method_blocks: HashSet<usize> = HashSet::new(); // addr-keyed: Rust heap blocks owned by `self` (borrowed for the walk), never Java heap.
        let mut code_blocks: HashSet<usize> = HashSet::new(); // addr-keyed: Rust heap blocks owned by `self` (borrowed for the walk), never Java heap.
        let mut table_blocks: HashSet<usize> = HashSet::new(); // addr-keyed: Rust heap blocks owned by `self` (borrowed for the walk), never Java heap.
        let (mut slot_vecs, mut entries_b, mut index_b, mut methods_b, mut code_b) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        for table in self.tables.values() {
            slot_vecs += table.entries.capacity() * size_of::<Option<Arc<VtableEntry>>>();
            if index_blocks.insert(Arc::as_ptr(&table.fast_lookup) as usize) {
                index_b += 16 + size_of::<SigIndex>() + table.fast_lookup.heap_bytes();
            }
            for entry in table.entries.iter().flatten() {
                if !entry_blocks.insert(Arc::as_ptr(entry) as usize) {
                    continue;
                }
                entries_b += 16 + size_of::<VtableEntry>();
                let Some(m) = &entry.resolved_method else {
                    continue;
                };
                if method_blocks.insert(Arc::as_ptr(m) as usize) {
                    methods_b += 16 + size_of::<cratonvm_jit_api::CachedBytecodeMethod>();
                    if table_blocks.insert(Arc::as_ptr(&m.exception_table) as *const u8 as usize) {
                        methods_b += 16 + std::mem::size_of_val(&*m.exception_table);
                    }
                    if code_blocks.insert(m.code.as_ptr() as usize) {
                        code_b += (16 + m.code.len()).div_ceil(8) * 8;
                    }
                }
            }
        }
        // zt t2/fp: the deferred snapshots (`CRATONVM_VTABLE_LAZY_DISPATCH`):
        // the cells' table, and the snapshots a dispatch built since, counted
        // with the link-time ones (each block once).
        let lazy_cells = hash_table_bytes(self.lazy.capacity(), size_of::<(u64, LazyDispatch)>());
        for lazy in self.lazy.values() {
            let Some(m) = lazy.built.get() else {
                continue;
            };
            if method_blocks.insert(Arc::as_ptr(m) as usize) {
                methods_b += 16 + size_of::<cratonvm_jit_api::CachedBytecodeMethod>();
                if table_blocks.insert(Arc::as_ptr(&m.exception_table) as *const u8 as usize) {
                    methods_b += 16 + std::mem::size_of_val(&*m.exception_table);
                }
                if code_blocks.insert(m.code.as_ptr() as usize) {
                    code_b += (16 + m.code.len()).div_ceil(8) * 8;
                }
            }
        }
        vec![
            ("tables", self.tables.len()),
            (
                "table_map",
                hash_table_bytes(self.tables.capacity(), size_of::<(u64, Vtable)>()),
            ),
            ("slot_vecs", slot_vecs),
            ("entry_blocks", entries_b),
            ("sig_index", index_b),
            ("dispatch_snapshots", methods_b),
            ("padded_code", code_b),
            ("lazy_cells", lazy_cells),
        ]
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
            .and_then(|e| self.entry_dispatch(e))
            .cloned()
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

    /// The dispatch snapshot the DECLARING class already installed for an
    /// inherited slot, for a subclass's install to share instead of rebuilding.
    ///
    /// Every subclass's descriptor vector carries the inherited slots'
    /// snapshots verbatim, and `vtable_install_adapter` used to turn each one
    /// into a fresh `Arc<CachedBytecodeMethod>` — including a padded copy of
    /// the bytecode — per slot, per class. `Object`'s bytecode methods alone
    /// were rebuilt once for every class linked, and each copy carried its own
    /// invocation counter, quickened code and JIT-probe memo, so one Java
    /// method's interpreter state was split by receiver class.
    ///
    /// Inherited slots keep their index down the hierarchy, so the declaring
    /// class's own table holds the entry at `slot`. It is used only when it
    /// provably describes the same method; anything else returns `None` and
    /// the caller builds a snapshot exactly as before.
    pub fn inherited_dispatch(
        &self,
        declaring_class_id: u64,
        slot: usize,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<Arc<cratonvm_jit_api::CachedBytecodeMethod>> {
        let entry = self.tables.get(&declaring_class_id)?.get(slot)?;
        if entry.declaring_class_id != declaring_class_id
            || entry.is_native
            || &*entry.method_name != method_name
            || &*entry.descriptor != descriptor
        {
            return None;
        }
        let cached = entry.resolved_method.as_ref()?;
        if &*cached.class_name != class_name
            || cached.declaring_class_id.as_u32() as u64 != declaring_class_id
        {
            return None;
        }
        Some(Arc::clone(cached))
    }

    /// zs s3/fp3: the declaring class's own block at `slot`, when its table
    /// is installed and the slot is populated (whatever it holds: the caller
    /// compares it field for field before sharing it).
    fn declared_block(&self, declaring_class_id: u64, slot: usize) -> Option<&Arc<VtableEntry>> {
        self.tables
            .get(&declaring_class_id)?
            .entries
            .get(slot)?
            .as_ref()
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
    /// the assumption's intended consumer, does not read `resolved` at all (no
    /// `jit/**` reference to any of these APIs exists; the VM-side JIT helper
    /// reads only `get_vtable` / `lookup_slot` / `len`, as an index hint for the
    /// shared megamorphic table's per-class cells, round 12 wave 4, and never a
    /// target -- see `vm/src/jit/helpers.rs::mega_class_slot_plan`).
    ///
    /// Deliberately **not** changed in this pass: separating "CHA assumption
    /// broken" from "entry undispatchable" changes dispatch-tier semantics,
    /// and this pass cannot build or run the suites. Written up as a scoped
    /// proposal in
    /// `stackwalk-and-vtable.md`.
    ///
    /// Since interpreter round i1 wave 22 the class-definition signal no
    /// longer calls this ([`OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY`]); it is kept
    /// for that switch. A class-hierarchy consumer that needs a "has an
    /// override" fact should get its own bit, not reuse `resolved`.
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
        self.forget_lazy_dispatch(|declaring| declaring == class_id);
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
                // zs s3/fp3: copy on write (the block may be shared), and only
                // when the bit is set; the written value is the same.
                if dead.contains(&entry.declaring_class_id) && entry.resolved {
                    Arc::make_mut(entry).resolved = false;
                }
            }
        }
        for id in &dead {
            self.tables.remove(id);
            self.itables.remove(id);
        }
        self.forget_lazy_dispatch(|declaring| dead.contains(&declaring));
    }
}

impl Default for VtableManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// T10.5 — the process-wide `GLOBAL_VTABLE_MANAGER` and the legacy
// `fn(u32, ..)` install/override hooks are GONE (interpreter round i1 wave 21,
// lane L5, `i18-L5-proposal-one-owner-token-for-every-classloading-hook`).
// They were correct for exactly one VM per process: `ClassId`s are allocated
// per VM, and a captureless `fn(u32, ..)` hook wrote every VM's vtables into
// the first VM's manager under colliding ids (per-vm-state V1), so VM B's
// `resolve_virtual_slot` could hand back a body built from VM A's class. No VM
// had installed them since wave 19; the scoped hooks below replaced them.

// ---------------------------------------------------------------------------
// Per-VM routing of the VM-scoped vtable hooks (interpreter round i1 wave 19,
// stage 3 of `i18-L5-proposal-one-owner-token-for-every-classloading-hook`).
//
// `SharedVm::new` binds its class manager's metadata realm to the VM's
// `vm_identity` right after constructing it, registers its `VtableManager`
// here under that identity, and installs the scoped hooks
// (`install_vtable_install_hook_scoped`), which hand the adapter a
// `ClassHandle` naming the firing VM. The adapter installs into that VM's
// manager and nowhere else. The legacy `fn(u32, ..)` hooks and
// `GLOBAL_VTABLE_MANAGER` were deleted in wave 21.
//
// A handle whose VM has no manager registered yet (a VM still inside
// `SharedVm::new`, whose classes the catch-up replay installs once it has
// registered) or never will (an unbound realm: a class manager built outside
// a VM) is dropped: a missing vtable only sends dispatch down the slow path,
// whereas installing into another VM's manager was wrong dispatch there.
// ---------------------------------------------------------------------------

type VmVtableManagers = parking_lot::Mutex<Vec<(usize, std::sync::Weak<RwLock<VtableManager>>)>>;

/// `vm_identity -> VtableManager`, weakly, for the scoped adapters. A
/// dropped VM's entry is pruned by the next registration.
static VM_VTABLE_MANAGERS: VmVtableManagers = parking_lot::Mutex::new(Vec::new());

/// Register `manager` as the vtable manager of the VM `vm_identity`, the
/// target of every scoped install and override hook fired for that VM.
/// Called by `SharedVm::new` before its catch-up replay. Re-registering an
/// identity replaces its entry.
pub fn register_vm_vtable_manager(vm_identity: usize, manager: &Arc<RwLock<VtableManager>>) {
    let mut managers = VM_VTABLE_MANAGERS.lock();
    managers.retain(|(id, weak)| *id != vm_identity && weak.strong_count() > 0);
    managers.push((vm_identity, Arc::downgrade(manager)));
}

/// The live vtable manager registered for `vm_identity`, if any. The
/// registry lock is released before the caller takes the manager's lock.
pub fn vtable_manager_of_vm(vm_identity: usize) -> Option<Arc<RwLock<VtableManager>>> {
    VM_VTABLE_MANAGERS
        .lock()
        .iter()
        .find(|(id, _)| *id == vm_identity)
        .and_then(|(_, weak)| weak.upgrade())
}

/// Adapter that satisfies `cratonvm_classloading::VtableInstallHookScoped`:
/// installs into the manager of the VM that minted `class`.
pub fn vtable_install_adapter_scoped(
    class: cratonvm_classloading::metadata_handle::ClassHandle,
    entries: Vec<Option<cratonvm_classloading::VtableSlotDescriptor>>,
) {
    // `CRATONVM_DBG_DEFINE_CENSUS`: the `install-lookup` row, for a charged
    // define (interpreter round i4 wave 95, lane L5).
    let lookup_clock = cratonvm_classloading::define_census::phase_start_if(
        cratonvm_classloading::define_census::charged_scope(),
    );
    let manager = vtable_manager_of_vm(class.vm().as_usize());
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::InstallLookup,
        lookup_clock,
    );
    if let Some(manager) = manager {
        install_vtable_descriptors(&manager, class.class_id_unchecked().as_u32(), entries);
    }
}

/// Kill switch for the subclass-override signal (interpreter round i1 wave
/// 22, lane L4; the proposal
/// `docs/internal/fixed-bugs/interpreter-L4-proposal-vtable-override-signal-only-disables-the-supers-own-fast-path-FIXED-20260926.md`).
/// `false` (the default): [`vtable_override_adapter_scoped`] does nothing.
/// `true`: it clears `resolved` on the SUPER's own entry for the overridden
/// slot, as it did until wave 22.
///
/// # Why the signal is dropped
///
/// Every reader of a `VtableManager` entry (`resolve_virtual*`,
/// `vtable_entry_ref`, `resolve_interface`, and the interpreter's
/// `execute_invokevirtual_vtable_fast`) indexes the table of the RECEIVER's
/// own class. The super's table is therefore only consulted for a receiver
/// whose class IS the super, and for that receiver the super's own method is
/// the JVMS 5.4.6 selection whatever its subclasses declare: the subclass's
/// override lives in the subclass's own table (installed just before the
/// signal fires). Clearing the super's entry never made an answer correct; it
/// only sent every later vtable-path call on a plain-super receiver
/// (`Object.hashCode` / `toString` / `equals` on a bare `Object`, any
/// overridden method on its declaring class's own instances) down the slow
/// resolution path for the life of the process, because nothing ever set the
/// bit back. The consumer the comment at the fire site names — "stale cached
/// dispatch entries (thread-local invoke_cache or promoted shared-resolution
/// entries)" — is not touched by it either: an invoke-cache entry is guarded
/// by its receiver class, and the JIT reads no `VtableManager` entry (its
/// helper's `lookup_slot` column hint, `helpers.rs::mega_class_slot_plan`,
/// never reads `resolved` and never selects a target).
/// `inherited_dispatch` (a later subclass sharing the super's snapshot)
/// never looked at `resolved`. `invalidate_class` and unloading still clear
/// entries: those make an entry undispatchable.
pub const OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY: bool = false;

/// Adapter that satisfies `cratonvm_classloading::VtableOverrideHookScoped`,
/// fired once per `(super_class, slot)` a newly linked class overrides. Since
/// interpreter round i1 wave 22 it does nothing (see
/// [`OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY`], which also skips the registry lock
/// and the manager's write lock it took per overridden slot); under the
/// switch it clears the super's own entry in the manager of the VM that
/// minted `super_class`. It replaced the process-global
/// `vtable_override_adapter` in interpreter round i1 wave 19.
pub fn vtable_override_adapter_scoped(
    super_class: cratonvm_classloading::metadata_handle::ClassHandle,
    slot: usize,
) {
    if !OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY {
        return;
    }
    if let Some(manager) = vtable_manager_of_vm(super_class.vm().as_usize()) {
        manager
            .write()
            .invalidate_for_override(super_class.class_id_unchecked().as_u32() as u64, slot);
    }
}

/// zs s3/fp3 (2026-10-14, footprint): a dispatch snapshot's exception table.
/// Most methods have none, and `Arc::<[_]>::from(&[])` still allocates a
/// 16-byte block (the two counts) per snapshot; an empty table shares one
/// block per process instead. Immutable and empty, so no answer can differ;
/// nothing compares these tables by address.
fn shared_exception_table(
    table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> Arc<[cratonvm_reader::attribute::ExceptionTableEntry]> {
    static EMPTY: std::sync::OnceLock<Arc<[cratonvm_reader::attribute::ExceptionTableEntry]>> =
        std::sync::OnceLock::new();
    if table.is_empty() {
        Arc::clone(EMPTY.get_or_init(|| Arc::from(Vec::new())))
    } else {
        Arc::from(table)
    }
}

/// zt t2/fp (`CRATONVM_VTABLE_LAZY_DISPATCH`): one declared bytecode
/// method's dispatch snapshot, deferred from link to its first dispatch.
///
/// The link-time install built a `CachedBytecodeMethod` (~300 bytes) and a
/// padded copy of the bytecode for every declared virtual bytecode method,
/// called or not: ~1.9 KB a java.base class by the zs s3/fp3 local census
/// (`zs3fp3-vm-part dispatch_snapshots` + `padded_code`), the largest
/// per-class owner the VM keeps itself. HotSpot builds a method's
/// `MethodCounters` / `MethodData` only once it runs. A cell is the loader's
/// descriptor handle (8 bytes; the loader keeps the same block for the
/// class's life) and an empty `OnceLock`.
struct LazyDispatch {
    /// The declaring class's own slot descriptor (its snapshot is `Some` and
    /// not native: [`VtableManager::defer_dispatch`] records no other).
    desc: cratonvm_classloading::VtableSlotDescriptor,
    /// The snapshot, once a dispatch asked for it.
    built: std::sync::OnceLock<Arc<cratonvm_jit_api::CachedBytecodeMethod>>,
}

/// The [`VtableManager::lazy`] key of a method: its declaring class id (a
/// `u32` `ClassId` widened) in the high half, its method index in the low.
#[inline]
fn lazy_key(declaring_class_id: u64, method_index: u32) -> u64 {
    (declaring_class_id << 32) | u64::from(method_index)
}

/// The `[vm] vtable-lazy-dispatch:` line of one VM's `manager`: whether it
/// latched `CRATONVM_VTABLE_LAZY_DISPATCH`, how many declared bytecode
/// methods' snapshots it deferred at link and how many of those a dispatch
/// built since (`built / deferred` is the share a lazy arm still pays for).
/// Both 0 with the switch off. zt t6/rv3 (`zgc-zt3x2-*` item 5): `on=` is the
/// manager's own latch, not a fresh read of the environment, and the counters
/// are the manager's, not process statics. `None` (no owning VM) prints
/// `on=0 deferred=0 built=0`.
pub fn lazy_dispatch_line(manager: Option<&VtableManager>) -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(m) = manager else {
        return "[vm] vtable-lazy-dispatch: on=0 deferred=0 built=0".to_string();
    };
    format!(
        "[vm] vtable-lazy-dispatch: on={} deferred={} built={}",
        u8::from(m.lazy_dispatch),
        m.lazy_deferred.load(Relaxed),
        m.lazy_built.load(Relaxed)
    )
}

/// The dispatch snapshot of the bytecode method `snap` occupying slot `d`:
/// what the link-time install builds, and what a deferred cell builds on
/// first use ([`LazyDispatch`]), the one recipe for both.
///
/// T10.9.A — a full `Arc<CachedBytecodeMethod>` so the interpreter can push
/// a frame without re-entering the class manager. Bytecode is padded (two
/// trailing zeros) so the hot loop can speculatively read `code[pc+1]` and
/// `code[pc+2]` without bounds checks — same invariant the slow path relies
/// on.
fn build_dispatch_snapshot(
    d: &cratonvm_classloading::VtableSlotData,
    snap: &cratonvm_classloading::VtableMethodSnapshot,
) -> Arc<cratonvm_jit_api::CachedBytecodeMethod> {
    // `snap.code` is the reader's zero-copy view of the class-file buffer; the
    // interpreter needs a +2-byte zero-padded copy for its speculative
    // `code[pc+1/+2]` reads, built through the per-method memo
    // (`padded_bytecode_for_method`, interpreter round i1 wave 5): the
    // interpreter's cache fills and `execute` pad the same method through it,
    // and the quickened stream and local-liveness table are keyed on the
    // padded `Arc`'s address — a private copy here gave every
    // vtable-dispatched method a second set of both.
    let padded = crate::runtime::frame::padded_bytecode_for_method(
        crate::classloading::ClassId::new(d.declaring_class_id),
        &d.method_name,
        &d.descriptor,
        &snap.code,
        &snap.exception_table,
    );
    Arc::new(cratonvm_jit_api::CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: crate::classloading::ClassId::new(d.declaring_class_id),
            class_name: Arc::clone(&snap.class_name),
            method_name: Arc::clone(&d.method_name),
            method_descriptor: Arc::clone(&d.descriptor),
            source_file: snap.source_file.clone(),
            code: padded,
            exception_table: shared_exception_table(&snap.exception_table),
            max_stack: snap.max_stack,
            max_locals: snap.max_locals,
            num_params: snap.num_params,
            is_synchronized: snap.is_synchronized,
            is_static: snap.is_static,
        },
    ))
}

/// Convert `class_id`'s descriptor vec into `VtableEntry`s and install it
/// into `manager` (one write lock per class): the body of the scoped install
/// adapter and of `SharedVm::new`'s catch-up replay into its own manager.
pub fn install_vtable_descriptors(
    manager: &RwLock<VtableManager>,
    class_id: u32,
    entries: Vec<Option<cratonvm_classloading::VtableSlotDescriptor>>,
) {
    // Taken BEFORE the conversion, not after it: an inherited slot reuses the
    // declaring class's already-installed snapshot (see
    // `VtableManager::inherited_dispatch`), and reading it under the same
    // guard that installs this table keeps a concurrent redefinition's
    // refresh sweep from landing between the two.
    // `CRATONVM_DBG_DEFINE_CENSUS`: `install-convert` (the lock and the
    // conversion) and `install-table`, for a charged define (wave 95, L5).
    let census = cratonvm_classloading::define_census::charged_scope();
    let convert_clock = cratonvm_classloading::define_census::phase_start_if(census);
    let mut manager = manager.write();
    // zt t6/rv3: a re-install drops the cells of methods this class no longer
    // declares at that index (an index-moving redefinition). Sound because
    // `install_vtable_shared`'s refresh sweep then points every subclass slot
    // declared by this class at the new blocks, so no table keeps an old
    // index. With nothing declared the sweep does not run, so neither does
    // this.
    if manager.lazy_dispatch && manager.tables.contains_key(&(class_id as u64)) {
        let declared: Vec<u32> = entries
            .iter()
            .flatten()
            .filter(|d| d.declaring_class_id == class_id)
            .map(|d| d.method_index)
            .collect();
        if !declared.is_empty() {
            manager.forget_undeclared_lazy_dispatch(class_id as u64, &declared);
        }
    }
    // `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE` (default off): an all-inherited
    // table copied from the installed one it provably equals.
    if let Some((table, fast_lookup)) = manager.inherited_template(class_id as u64, &entries) {
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::InstallConvert,
            convert_clock,
        );
        let table_clock = cratonvm_classloading::define_census::phase_start_if(census);
        manager.install_new_vtable_with_index(class_id as u64, table, fast_lookup);
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::InstallTable,
            table_clock,
        );
        return;
    }
    let converted: Vec<Option<Arc<VtableEntry>>> = entries
        .into_iter()
        .enumerate()
        .map(|(slot, opt)| {
            opt.map(|d| {
                // T10.9.A — when the class loader captured a dispatch
                // snapshot, promote it into a full `Arc<CachedBytecodeMethod>`
                // so the interpreter can push a frame without re-entering
                // the class manager. Bytecode is padded (two trailing
                // zeros) so the hot loop can speculatively read `code[pc+1]`
                // and `code[pc+2]` without bounds checks — same invariant
                // the slow path relies on.
                let inherited = if d.declaring_class_id != class_id {
                    d.dispatch.as_ref().and_then(|snap| {
                        manager.inherited_dispatch(
                            d.declaring_class_id as u64,
                            slot,
                            &snap.class_name,
                            &d.method_name,
                            &d.descriptor,
                        )
                    })
                } else {
                    None
                };
                // zs s2/fp2: `d` is a shared handle (an inherited slot shares
                // its declaring class's block), so the snapshot and names are
                // borrowed and their `Arc`s cloned, not moved out.
                let (resolved_method, is_native) = match &d.dispatch {
                    Some(snap) if !snap.is_native && inherited.is_some() => (inherited, false),
                    Some(snap) if !snap.is_native => {
                        // zt t2/fp: deferred to the first dispatch under
                        // `CRATONVM_VTABLE_LAZY_DISPATCH` (never by default).
                        if manager.defer_dispatch(class_id, &d, snap) {
                            (None, false)
                        } else {
                            (Some(build_dispatch_snapshot(&d, snap)), false)
                        }
                    }
                    Some(snap) => (None, snap.is_native),
                    None => (None, false),
                };
                let entry = VtableEntry {
                    declaring_class_id: d.declaring_class_id as u64,
                    method_index: d.method_index,
                    // HIGH-5 — descriptor's `method_name`/`descriptor` are
                    // already `Arc<str>` (T10.9.E classloading conversion);
                    // a refcount bump each.
                    method_name: Arc::clone(&d.method_name),
                    descriptor: Arc::clone(&d.descriptor),
                    resolved: true,
                    resolved_method,
                    is_native,
                };
                // zs s3/fp3: an inherited slot whose entry is field for field
                // the declaring class's own (the same snapshot `Arc`, which
                // `inherited_dispatch` handed out above) shares that block.
                // Anything else (an unresolved, redefined or rebuilt entry)
                // keeps a block of its own, with the same values as before.
                if d.declaring_class_id != class_id {
                    if let Some(shared) = manager.declared_block(d.declaring_class_id as u64, slot)
                    {
                        if **shared == entry
                            && match (&shared.resolved_method, &entry.resolved_method) {
                                (None, None) => true,
                                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                                _ => false,
                            }
                        {
                            return Arc::clone(shared);
                        }
                    }
                }
                Arc::new(entry)
            })
        })
        .collect();
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::InstallConvert,
        convert_clock,
    );
    let table_clock = cratonvm_classloading::define_census::phase_start_if(census);
    manager.install_vtable_shared(class_id as u64, converted);
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::InstallTable,
        table_clock,
    );
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
        // zs s3/fp3: the bucket is a tagged `u32` in the index.
        let mut ix = SigIndex::default();
        ix.push(77, 3);
        assert_eq!(ix.get(77), Some(&[3u32][..]));
        ix.push(77, 3); // idempotent — must not allocate a Vec
        assert!(ix.many.is_empty());
        assert_eq!(ix.get(77), Some(&[3u32][..]));
        ix.push(77, 9);
        assert_eq!(ix.get(77), Some(&[3u32, 9][..]));
        ix.push(77, 9);
        assert_eq!(ix.get(77), Some(&[3u32, 9][..]));
        ix.push(77, 4);
        assert_eq!(ix.get(77), Some(&[3u32, 9, 4][..]));
        assert_eq!(ix.many.len(), 1);
        // A second colliding key gets its own list; other keys stay inline.
        ix.push(5, 1);
        ix.push(5, 2);
        ix.push(6, 0);
        assert_eq!(ix.get(5), Some(&[1u32, 2][..]));
        assert_eq!(ix.get(6), Some(&[0u32][..]));
        assert_eq!(ix.get(77), Some(&[3u32, 9, 4][..]));
        assert_eq!(ix.get(8), None);
        assert_eq!(std::mem::size_of::<(u32, u32)>(), 8);
        // Two keys that share the low half share a bucket; both stay
        // candidates in insertion order (the caller verifies each).
        let mut shared = SigIndex::default();
        shared.push(0x1_0000_0042, 4);
        shared.push(0x2_0000_0042, 1);
        assert_eq!(shared.get(0x1_0000_0042), Some(&[4u32, 1][..]));
        assert_eq!(shared.get(0x3_0000_0042), Some(&[4u32, 1][..]));
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
        Arc::new(cratonvm_jit_api::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ))
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
    /// the adapter consumes has not drifted. The behavioural half is
    /// `an_abstract_slot_installs_without_a_body` below, which calls the
    /// conversion (`install_vtable_descriptors`, lifted out of the adapter in
    /// wave 19; the global manager it once needed is gone since wave 21).
    #[test]
    fn t10_9_a_adapter_preserves_empty_dispatch() {
        // Shape-only: this is a compile-time check on `VtableSlotDescriptor`,
        // NOT a check on the adapter. See the doc comment above.
        let desc = cratonvm_classloading::VtableSlotDescriptor::new(
            cratonvm_classloading::VtableSlotData {
                declaring_class_id: 77,
                method_index: 3,
                method_name: Arc::<str>::from("abstr"),
                descriptor: Arc::<str>::from("()I"),
                dispatch: None, // abstract method — no snapshot
            },
        );
        assert!(desc.dispatch.is_none());
        assert_eq!(desc.method_index, 3);
    }

    /// The behavioural half of the test above, now that the conversion is a
    /// callable function (`install_vtable_descriptors`, interpreter round i1
    /// wave 19): a `dispatch: None` descriptor (an abstract method) installs
    /// a resolved entry with no bytecode body, into the manager it is given.
    #[test]
    fn an_abstract_slot_installs_without_a_body() {
        let manager = RwLock::new(VtableManager::new());
        let desc = cratonvm_classloading::VtableSlotDescriptor::new(
            cratonvm_classloading::VtableSlotData {
                declaring_class_id: 77,
                method_index: 3,
                method_name: Arc::<str>::from("abstr"),
                descriptor: Arc::<str>::from("()I"),
                dispatch: None,
            },
        );
        install_vtable_descriptors(&manager, 77, vec![Some(desc)]);
        let entry = manager
            .read()
            .resolve_virtual_slot(77, 0)
            .expect("slot installed");
        assert_eq!(entry.method_index, 3);
        assert!(
            entry.resolved_method.is_none(),
            "no body for an abstract method"
        );
        assert!(!entry.is_native);
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

    /// An inherited slot shares the declaring class's snapshot, and only when
    /// every identifying field agrees; every mismatch answers `None` so the
    /// install builds a fresh snapshot exactly as it did before.
    #[test]
    fn inherited_dispatch_shares_only_a_provably_identical_snapshot() {
        let mut mgr = VtableManager::new();
        let declared = make_dispatch_stub(40, "run", "()V");
        let entry = |resolved_method, is_native| VtableEntry {
            declaring_class_id: 40,
            method_index: 0,
            method_name: Arc::<str>::from("run"),
            descriptor: Arc::<str>::from("()V"),
            resolved: true,
            resolved_method,
            is_native,
        };
        mgr.install_vtable(40, vec![Some(entry(Some(Arc::clone(&declared)), false))]);

        let shared = mgr
            .inherited_dispatch(40, 0, "Stub", "run", "()V")
            .expect("identical inherited slot");
        assert!(Arc::ptr_eq(&shared, &declared), "must be the same Arc");

        // A CHA-invalidated slot still describes the same method body.
        mgr.invalidate_for_override(40, 0);
        assert!(mgr
            .inherited_dispatch(40, 0, "Stub", "run", "()V")
            .is_some());

        assert!(mgr
            .inherited_dispatch(40, 1, "Stub", "run", "()V")
            .is_none());
        assert!(mgr
            .inherited_dispatch(41, 0, "Stub", "run", "()V")
            .is_none());
        assert!(mgr
            .inherited_dispatch(40, 0, "Other", "run", "()V")
            .is_none());
        assert!(mgr
            .inherited_dispatch(40, 0, "Stub", "walk", "()V")
            .is_none());
        assert!(mgr
            .inherited_dispatch(40, 0, "Stub", "run", "(I)V")
            .is_none());

        mgr.install_vtable(40, vec![Some(entry(None, true))]);
        assert!(
            mgr.inherited_dispatch(40, 0, "Stub", "run", "()V")
                .is_none(),
            "a native slot has no snapshot to share"
        );
    }

    /// Interpreter round i1 wave 22, lane L4
    /// (`OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY`): a subclass that overrides a
    /// slot leaves the super's own entry dispatchable — a receiver of the
    /// super's class still selects the super's method — and the subclass's
    /// table answers the override.
    #[test]
    fn a_subclass_override_leaves_the_supers_own_entry_dispatchable() {
        use cratonvm_classloading::metadata_handle::ClassHandle;
        use cratonvm_native_api::VmId;
        if OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY {
            return;
        }
        // A VM identity no real VM in the test process holds.
        let vm_identity = 0x4C34_0022_usize;
        let manager = Arc::new(RwLock::new(VtableManager::new()));
        register_vm_vtable_manager(vm_identity, &manager);
        let entry = |declaring: u64| VtableEntry {
            declaring_class_id: declaring,
            method_index: 0,
            method_name: Arc::<str>::from("hashCode"),
            descriptor: Arc::<str>::from("()I"),
            resolved: true,
            resolved_method: None,
            is_native: false,
        };
        let (sup, sub) = (0x0022_0001_u32, 0x0022_0002_u32);
        manager
            .write()
            .install_vtable(sup as u64, vec![Some(entry(sup as u64))]);
        manager
            .write()
            .install_vtable(sub as u64, vec![Some(entry(sub as u64))]);

        vtable_override_adapter_scoped(
            ClassHandle::from_parts(
                VmId::from_raw(vm_identity),
                0,
                crate::classloading::ClassId::new(sup),
            ),
            0,
        );

        let mgr = manager.read();
        assert_eq!(
            mgr.resolve_virtual_slot(sup as u64, 0)
                .map(|e| e.declaring_class_id),
            Some(sup as u64),
            "a receiver of the super's own class still selects its method"
        );
        assert_eq!(
            mgr.resolve_virtual_slot(sub as u64, 0)
                .map(|e| e.declaring_class_id),
            Some(sub as u64)
        );
    }

    // -- zs s3/fp3 (2026-10-14): shared entry blocks and signature indices --

    fn zs3fp3_slot(
        declaring: u32,
        index: u32,
        name: &str,
        desc: &str,
        body: Option<(&str, bool)>,
    ) -> cratonvm_classloading::VtableSlotDescriptor {
        cratonvm_classloading::VtableSlotDescriptor::new(cratonvm_classloading::VtableSlotData {
            declaring_class_id: declaring,
            method_index: index,
            method_name: Arc::<str>::from(name),
            descriptor: Arc::<str>::from(desc),
            dispatch: body.map(|(class_name, is_native)| {
                Arc::new(cratonvm_classloading::VtableMethodSnapshot {
                    class_name: Arc::from(class_name),
                    source_file: None,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xb1_u8, index as u8]),
                    exception_table: vec![],
                    max_stack: 1,
                    max_locals: 1,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: false,
                    is_native,
                })
            }),
        })
    }

    fn zs3fp3_block(mgr: &VtableManager, class_id: u32, slot: usize) -> Arc<VtableEntry> {
        Arc::clone(
            mgr.tables[&(class_id as u64)].entries[slot]
                .as_ref()
                .expect("slot populated"),
        )
    }

    /// An inherited slot shares its declaring class's entry block, an
    /// override-only class shares its ancestor's signature index, and every
    /// answer (lookups, entries, invalidation, redefinition, unloading) is
    /// the one private copies gave.
    #[test]
    fn zs3fp3_inherited_vtable_entries_share_the_declaring_block() {
        const P: u32 = 73_101; // declares run (bytecode), nat (native), abs (abstract)
        const C: u32 = 73_102; // inherits all three, appends extra
        const O: u32 = 73_103; // overrides run only: same layout as P
        const D: u32 = 73_104; // linked after P's invalidation
        // The eager arm explicitly (zt t2/fp): this test reads `resolved_method`.
        let manager = RwLock::new(VtableManager::with_lazy_dispatch(false));
        let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
        let nat = zs3fp3_slot(P, 1, "nat", "()V", Some(("pkg/P", true)));
        let abs = zs3fp3_slot(P, 2, "abs", "()I", None);
        install_vtable_descriptors(
            &manager,
            P,
            vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
        );
        install_vtable_descriptors(
            &manager,
            C,
            vec![
                Some(run.clone()),
                Some(nat.clone()),
                Some(abs.clone()),
                Some(zs3fp3_slot(C, 0, "extra", "()V", Some(("pkg/C", false)))),
            ],
        );
        install_vtable_descriptors(
            &manager,
            O,
            vec![
                Some(zs3fp3_slot(O, 0, "run", "()V", Some(("pkg/O", false)))),
                Some(nat.clone()),
                Some(abs.clone()),
            ],
        );
        {
            let mgr = manager.read();
            for slot in 0..3 {
                assert!(
                    Arc::ptr_eq(&zs3fp3_block(&mgr, P, slot), &zs3fp3_block(&mgr, C, slot)),
                    "C slot {slot} shares P's block"
                );
            }
            assert!(!Arc::ptr_eq(
                &zs3fp3_block(&mgr, P, 0),
                &zs3fp3_block(&mgr, O, 0)
            ));
            assert!(Arc::ptr_eq(
                &zs3fp3_block(&mgr, P, 1),
                &zs3fp3_block(&mgr, O, 1)
            ));
            // The index: O (override only) shares P's; C (one more slot) has its own.
            let ix = |c: u32| Arc::as_ptr(&mgr.tables[&(c as u64)].fast_lookup);
            assert_eq!(ix(O), ix(P));
            assert_ne!(ix(C), ix(P));
            // Same answers as a private build of each table.
            let three: &[(&str, &str)] = &[("run", "()V"), ("nat", "()V"), ("abs", "()I")];
            let four: &[(&str, &str)] = &[
                ("run", "()V"),
                ("nat", "()V"),
                ("abs", "()I"),
                ("extra", "()V"),
            ];
            for (c, sigs) in [(P, three), (C, four), (O, three)] {
                let vt = mgr.get_vtable(c as u64).unwrap();
                let own = SigIndex::build(&vt.entries);
                for (slot, (n, d)) in sigs.iter().enumerate() {
                    assert_eq!(vt.lookup_slot(n, d), Some(slot), "{c} {n}{d}");
                    assert_eq!(own.get(fast_lookup_key(n, d)), Some(&[slot as u32][..]));
                }
                assert_eq!(vt.lookup_slot("extra", "()I"), None);
            }
            assert_eq!(
                mgr.get_vtable(O as u64)
                    .unwrap()
                    .lookup_slot("extra", "()V"),
                None
            );
            let c0 = mgr.resolve_virtual_slot(C as u64, 0).unwrap();
            assert_eq!(c0.declaring_class_id, P as u64);
            assert!(c0.resolved_method.is_some());
            // Two snapshots with no handlers share one empty exception table.
            let c3 = mgr.resolve_virtual_slot(C as u64, 3).unwrap();
            assert!(Arc::ptr_eq(
                &c0.resolved_method.as_ref().unwrap().exception_table,
                &c3.resolved_method.as_ref().unwrap().exception_table
            ));
            assert!(c3
                .resolved_method
                .as_ref()
                .unwrap()
                .exception_table
                .is_empty());
            assert!(mgr.resolve_virtual_slot(C as u64, 1).unwrap().is_native);
            assert!(mgr
                .resolve_virtual_slot(C as u64, 2)
                .unwrap()
                .resolved_method
                .is_none());
            assert_eq!(
                mgr.resolve_virtual_slot(O as u64, 0)
                    .unwrap()
                    .declaring_class_id,
                O as u64
            );
        }

        // A write to one table's shared block copies it: only that table sees it.
        manager
            .write()
            .get_vtable_mut(C as u64)
            .unwrap()
            .invalidate_slot(1);
        {
            let mgr = manager.read();
            assert!(mgr.vtable_entry_ref(C as u64, 1).is_none());
            assert!(mgr.vtable_entry_ref(P as u64, 1).is_some());
            assert!(mgr.vtable_entry_ref(O as u64, 1).is_some());
            assert!(!Arc::ptr_eq(
                &zs3fp3_block(&mgr, P, 1),
                &zs3fp3_block(&mgr, C, 1)
            ));
        }

        // Redefinition: the subclasses' inherited slots follow P's new block.
        let run2 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
        install_vtable_descriptors(
            &manager,
            P,
            vec![Some(run2), Some(nat.clone()), Some(abs.clone())],
        );
        {
            let mgr = manager.read();
            assert_eq!(
                mgr.resolve_virtual_slot(C as u64, 0).unwrap().method_index,
                5
            );
            assert!(Arc::ptr_eq(
                &zs3fp3_block(&mgr, P, 0),
                &zs3fp3_block(&mgr, C, 0)
            ));
            assert_eq!(
                mgr.resolve_virtual_slot(O as u64, 0)
                    .unwrap()
                    .declaring_class_id,
                O as u64
            );
        }

        // invalidate_class clears every table's copy; a class linked after it
        // gets a resolved block of its own, not the cleared shared one.
        manager.write().invalidate_class(P as u64);
        install_vtable_descriptors(
            &manager,
            D,
            vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
        );
        {
            let mgr = manager.read();
            for c in [P, C, O] {
                assert!(
                    mgr.vtable_entry_ref(c as u64, 2).is_none(),
                    "{c} abs cleared"
                );
            }
            assert!(
                mgr.vtable_entry_ref(O as u64, 0).is_some(),
                "O's own override stays"
            );
            for slot in 0..3 {
                assert!(
                    mgr.vtable_entry_ref(D as u64, slot).is_some(),
                    "D slot {slot}"
                );
            }
        }

        // Unloading P clears the inherited slots of C and D alike.
        manager.write().unload_classes(&[P as u64]);
        let mgr = manager.read();
        assert!(mgr.get_vtable(P as u64).is_none());
        for c in [C, D] {
            for slot in 0..3 {
                assert!(mgr.vtable_entry_ref(c as u64, slot).is_none(), "{c} {slot}");
            }
        }
        assert!(mgr.vtable_entry_ref(C as u64, 3).is_some());
        assert!(mgr.vtable_entry_ref(O as u64, 0).is_some());
        let census: std::collections::BTreeMap<_, _> = mgr.footprint_census().into_iter().collect();
        assert_eq!(census["tables"], 3);
        assert!(census["entry_blocks"] > 0 && census["sig_index"] > 0);
    }

    /// zt t2/fp (`CRATONVM_VTABLE_LAZY_DISPATCH`): no snapshot is built at
    /// link; the first request builds one per METHOD (an inherited slot gets
    /// its declaring method's), with the eager arm's contents; a redefinition
    /// gets a fresh cell, a replay of the same snapshot keeps the built one,
    /// an inherited slot carrying another snapshot is built at link as
    /// before, and an unload forgets the class's cells.
    #[test]
    fn zt2fp_lazy_dispatch_builds_one_snapshot_per_method_on_first_use() {
        const P: u32 = 74_201; // declares run (bytecode), nat (native), abs (abstract)
        const C: u32 = 74_202; // inherits all three, appends extra
        const O: u32 = 74_203; // overrides run only
        const Q: u32 = 74_204; // inherits run through another snapshot block
        let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
        let nat = zs3fp3_slot(P, 1, "nat", "()V", Some(("pkg/P", true)));
        let abs = zs3fp3_slot(P, 2, "abs", "()I", None);
        let extra = zs3fp3_slot(C, 0, "extra", "()V", Some(("pkg/C", false)));
        let o_run = zs3fp3_slot(O, 0, "run", "()V", Some(("pkg/O", false)));
        let install_all = |manager: &RwLock<VtableManager>| {
            install_vtable_descriptors(
                manager,
                P,
                vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
            );
            install_vtable_descriptors(
                manager,
                C,
                vec![
                    Some(run.clone()),
                    Some(nat.clone()),
                    Some(abs.clone()),
                    Some(extra.clone()),
                ],
            );
            install_vtable_descriptors(
                manager,
                O,
                vec![Some(o_run.clone()), Some(nat.clone()), Some(abs.clone())],
            );
        };
        let lazy = RwLock::new(VtableManager::with_lazy_dispatch(true));
        let eager = RwLock::new(VtableManager::with_lazy_dispatch(false));
        install_all(&lazy);
        install_all(&eager);

        {
            let mgr = lazy.read();
            // Nothing built at link: every bytecode slot is deferred.
            for (c, slot) in [(P, 0), (C, 0), (C, 3), (O, 0)] {
                assert!(
                    zs3fp3_block(&mgr, c, slot).resolved_method.is_none(),
                    "{c} slot {slot} deferred"
                );
            }
            assert_eq!(mgr.lazy.len(), 3, "run, extra and O.run");
            let census: std::collections::BTreeMap<_, _> =
                mgr.footprint_census().into_iter().collect();
            assert_eq!(census["dispatch_snapshots"], 0);
            assert_eq!(census["padded_code"], 0);
            assert!(census["lazy_cells"] > 0);
            // The inherited slot still shares the declaring block.
            assert!(Arc::ptr_eq(
                &zs3fp3_block(&mgr, P, 0),
                &zs3fp3_block(&mgr, C, 0)
            ));
            // First use: one snapshot for the method, whichever slot asks.
            let via_c = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
            let via_p = mgr.resolve_virtual_slot_method(P as u64, 0).unwrap();
            let by_name = mgr.resolve_virtual_method(P as u64, "run", "()V").unwrap();
            assert!(Arc::ptr_eq(&via_c, &via_p) && Arc::ptr_eq(&via_p, &by_name));
            let via_o = mgr.resolve_virtual_slot_method(O as u64, 0).unwrap();
            assert!(!Arc::ptr_eq(&via_o, &via_p), "O.run is its own method");
            // Abstract and native slots have none, as on the eager arm.
            assert!(mgr.resolve_virtual_slot_method(C as u64, 1).is_none());
            assert!(mgr.resolve_virtual_slot_method(C as u64, 2).is_none());
            // Same contents as the eager arm's link-time snapshot.
            let em = eager.read();
            for (c, slot) in [(P, 0), (C, 0), (C, 3), (O, 0)] {
                let l = mgr.resolve_virtual_slot_method(c as u64, slot).unwrap();
                let e = em.resolve_virtual_slot_method(c as u64, slot).unwrap();
                assert_eq!(l.declaring_class_id, e.declaring_class_id, "{c} {slot}");
                assert_eq!(&*l.class_name, &*e.class_name);
                assert_eq!(&*l.method_name, &*e.method_name);
                assert_eq!(&*l.method_descriptor, &*e.method_descriptor);
                assert_eq!(&*l.code, &*e.code);
                assert_eq!(l.max_stack, e.max_stack);
                assert_eq!(l.max_locals, e.max_locals);
            }
            let census: std::collections::BTreeMap<_, _> =
                mgr.footprint_census().into_iter().collect();
            assert!(census["dispatch_snapshots"] > 0, "built ones are counted");
        }

        // A replay of the same descriptors keeps the built snapshot.
        let before = lazy.read().resolve_virtual_slot_method(P as u64, 0).unwrap();
        install_vtable_descriptors(
            &lazy,
            P,
            vec![Some(run.clone()), Some(nat.clone()), Some(abs.clone())],
        );
        let after = lazy.read().resolve_virtual_slot_method(C as u64, 0).unwrap();
        assert!(Arc::ptr_eq(&before, &after), "replay keeps the cell");

        // A redefinition (a new snapshot) gets a fresh cell: the subclass
        // follows the new body.
        let run2 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
        install_vtable_descriptors(
            &lazy,
            P,
            vec![Some(run2), Some(nat.clone()), Some(abs.clone())],
        );
        {
            let mgr = lazy.read();
            let redefined = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
            assert!(!Arc::ptr_eq(&redefined, &before));
            assert_eq!(redefined.code[1], 5, "the new body");
            assert!(Arc::ptr_eq(
                &redefined,
                &mgr.resolve_virtual_slot_method(P as u64, 0).unwrap()
            ));
        }

        // zt t3/x2: a redefinition that KEEPS the method index replaces the
        // recorded cell itself. (The arm above moved the index from 0 to 5,
        // so it recorded a second key and never reached the replacement: the
        // subclass followed the refresh sweep's new block, whatever
        // `defer_dispatch` did with the old cell.)
        let built = lazy.read().resolve_virtual_slot_method(P as u64, 0).unwrap();
        let keys = lazy.read().lazy.len();
        let run3 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
        install_vtable_descriptors(
            &lazy,
            P,
            vec![Some(run3), Some(nat.clone()), Some(abs.clone())],
        );
        {
            let mgr = lazy.read();
            assert_eq!(mgr.lazy.len(), keys, "same key: replaced, not added");
            let renewed = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
            assert!(!Arc::ptr_eq(&renewed, &built), "a fresh cell builds anew");
            assert!(Arc::ptr_eq(
                &renewed,
                &mgr.resolve_virtual_slot_method(P as u64, 0).unwrap()
            ));
        }

        // An inherited slot carrying a snapshot block other than the one
        // recorded for its method is built at link, as on the eager arm.
        let other_run = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
        install_vtable_descriptors(&lazy, Q, vec![Some(other_run)]);
        assert!(zs3fp3_block(&lazy.read(), Q, 0).resolved_method.is_some());

        // Unloading P forgets its cells, and only its.
        lazy.write().unload_classes(&[P as u64]);
        {
            let mgr = lazy.read();
            assert!(mgr.lazy.keys().all(|key| key >> 32 != P as u64));
            assert_eq!(mgr.lazy.len(), 2, "extra and O.run stay");
            assert!(mgr.resolve_virtual_slot_method(C as u64, 0).is_none());
            assert!(mgr.resolve_virtual_slot_method(C as u64, 3).is_some());
        }
        lazy.write().unload_class(O as u64);
        assert_eq!(lazy.read().lazy.len(), 1, "extra stays");
    }

    /// zt t6/rv3 (`zgc-zt3x2-*` item 3): a redefinition that moves a
    /// method's index drops the old index's cell instead of keeping it, with
    /// its snapshot, until the class unloads; a method still declared at its
    /// index keeps its cell and built snapshot, and another class's cells are
    /// untouched.
    #[test]
    fn zt6rv3_an_index_moving_redefinition_drops_the_old_cell() {
        const P: u32 = 74_401;
        const C: u32 = 74_402;
        let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
        let keep = zs3fp3_slot(P, 1, "keep", "()V", Some(("pkg/P", false)));
        let extra = zs3fp3_slot(C, 0, "extra", "()V", Some(("pkg/C", false)));
        let lazy = RwLock::new(VtableManager::with_lazy_dispatch(true));
        install_vtable_descriptors(&lazy, P, vec![Some(run.clone()), Some(keep.clone())]);
        install_vtable_descriptors(
            &lazy,
            C,
            vec![Some(run.clone()), Some(keep.clone()), Some(extra)],
        );
        assert_eq!(lazy.read().lazy.len(), 3, "P.run, P.keep, C.extra");
        let kept = lazy.read().resolve_virtual_slot_method(P as u64, 1).unwrap();
        let run5 = zs3fp3_slot(P, 5, "run", "()V", Some(("pkg/P", false)));
        install_vtable_descriptors(&lazy, P, vec![Some(run5), Some(keep.clone())]);
        let mgr = lazy.read();
        assert_eq!(mgr.lazy.len(), 3, "the index-0 cell is dropped, not kept beside 5");
        assert!(!mgr.lazy.contains_key(&lazy_key(P as u64, 0)));
        assert!(mgr.lazy.contains_key(&lazy_key(P as u64, 5)));
        assert!(mgr.lazy.contains_key(&lazy_key(C as u64, 0)), "C's cell stays");
        assert!(
            Arc::ptr_eq(&kept, &mgr.resolve_virtual_slot_method(P as u64, 1).unwrap()),
            "a method still declared at its index keeps its built snapshot"
        );
        assert_eq!(
            mgr.resolve_virtual_slot_method(P as u64, 0).unwrap().code[1],
            5,
            "slot 0 dispatches to the redefined body"
        );
    }

    /// zt t6/rv3 (`zgc-zt3x2-*` item 4): with both
    /// `CRATONVM_VTABLE_LAZY_DISPATCH` and
    /// `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE`, an all-inherited class whose
    /// declaring slot is deferred takes the template (it never matched: the
    /// bytecode arm needed a built snapshot). A snapshot block other than the
    /// recorded one still refuses (negative control).
    #[test]
    fn zt6rv3_the_inherit_template_accepts_a_deferred_declaring_slot() {
        const P: u32 = 74_601;
        const C: u32 = 74_602;
        let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
        let nat = zs3fp3_slot(P, 1, "nat", "()V", Some(("pkg/P", true)));
        let lazy = RwLock::new(VtableManager::with_lazy_dispatch(true));
        lazy.write().inherit_template = true;
        install_vtable_descriptors(&lazy, P, vec![Some(run.clone()), Some(nat.clone())]);
        {
            let mgr = lazy.read();
            assert!(zs3fp3_block(&mgr, P, 0).resolved_method.is_none(), "P.run deferred");
            let (table, _) = mgr
                .inherited_template(C as u64, &[Some(run.clone()), Some(nat.clone())])
                .expect("the template matches a deferred declaring slot");
            assert!(Arc::ptr_eq(table[0].as_ref().unwrap(), &zs3fp3_block(&mgr, P, 0)));
            let other = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
            assert!(
                mgr.inherited_template(C as u64, &[Some(other), Some(nat.clone())]).is_none(),
                "another snapshot block is not the recorded one"
            );
        }
        install_vtable_descriptors(&lazy, C, vec![Some(run), Some(nat)]);
        let mgr = lazy.read();
        let via_c = mgr.resolve_virtual_slot_method(C as u64, 0).unwrap();
        let via_p = mgr.resolve_virtual_slot_method(P as u64, 0).unwrap();
        assert!(Arc::ptr_eq(&via_c, &via_p), "one snapshot for the method");
    }

    /// zt t6/rv3 (`zgc-zt3x2-*` item 5): the line reports the manager it is
    /// given: its own latch and its own counters, so two VMs in one process
    /// do not sum.
    #[test]
    fn zt6rv3_the_lazy_dispatch_line_reads_its_manager() {
        const P: u32 = 74_501;
        let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
        let on = RwLock::new(VtableManager::with_lazy_dispatch(true));
        let off = RwLock::new(VtableManager::with_lazy_dispatch(false));
        install_vtable_descriptors(&on, P, vec![Some(run.clone())]);
        install_vtable_descriptors(&off, P, vec![Some(run)]);
        assert_eq!(
            lazy_dispatch_line(Some(&on.read())),
            "[vm] vtable-lazy-dispatch: on=1 deferred=1 built=0"
        );
        on.read().resolve_virtual_slot_method(P as u64, 0).unwrap();
        assert_eq!(
            lazy_dispatch_line(Some(&on.read())),
            "[vm] vtable-lazy-dispatch: on=1 deferred=1 built=1"
        );
        assert_eq!(
            lazy_dispatch_line(Some(&off.read())),
            "[vm] vtable-lazy-dispatch: on=0 deferred=0 built=0"
        );
        assert_eq!(
            lazy_dispatch_line(None),
            "[vm] vtable-lazy-dispatch: on=0 deferred=0 built=0"
        );
    }

    /// zt t2/fp: the switch off defers nothing and keeps no cell.
    #[test]
    fn zt2fp_lazy_dispatch_off_builds_at_link() {
        const P: u32 = 74_301;
        let manager = RwLock::new(VtableManager::with_lazy_dispatch(false));
        let run = zs3fp3_slot(P, 0, "run", "()V", Some(("pkg/P", false)));
        install_vtable_descriptors(&manager, P, vec![Some(run)]);
        let mgr = manager.read();
        assert!(mgr.lazy.is_empty());
        assert!(zs3fp3_block(&mgr, P, 0).resolved_method.is_some());
        let census: std::collections::BTreeMap<_, _> = mgr.footprint_census().into_iter().collect();
        assert_eq!(census["lazy_cells"], 0);
    }
}
