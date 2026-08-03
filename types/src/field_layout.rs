// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-class compact object-field layout + GC oop-map registry.
//!
//! Under the compact reference-field layout (`CRATONVM_COMPACT_REF_FIELDS=0`
//! opts out),
//! reference instance fields are stored as bare 8-byte pointers instead of the
//! 16-byte tagged [`crate::Value`] cell. The GC can no longer detect a reference
//! slot by its cell tag, so it consults a per-class **oop-map**: the byte offsets
//! (within the object body, after `HEADER_SIZE`) of the reference fields.
//!
//! This registry lives in `cratonvm-types` because both producers and consumers
//! depend on it: the **classloading** crate builds a [`CompactLayout`] from a
//! class's field descriptors and registers it at class-define time; the **gc**
//! crate (collector scan/remap) and the heap field accessors read it back by
//! `class_id`. Keeping it here avoids a classloading→gc dependency edge and any
//! GC→class-manager callback (the GC only ever *reads* this registry, never
//! calls back into class metadata — important for stop-the-world safety).
//!
//! See `docs/feature-designs/compact-ref-field-layout.md`.

use crate::heap_types::{ObjectHeader, GC_FLAG_COMPACT, SLOT_SIZE};
use crate::{ObjectRef, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, RwLock};

/// Tagless storage representation of a compact instance field.
///
/// The verifier and class loader determine this once from the field descriptor;
/// mutators and compiled code then load/store only the Java payload width. This
/// removes the 16-byte Rust `Value` discriminant cell from production object
/// bodies while retaining `Value` at VM API boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FieldStorageKind {
    Reference,
    Boolean,
    Byte,
    Char,
    Short,
    Int,
    Float,
    Long,
    Double,
}

impl FieldStorageKind {
    #[inline]
    pub const fn from_descriptor_byte(descriptor: u8) -> Option<Self> {
        Some(match descriptor {
            b'L' | b'[' => Self::Reference,
            b'Z' => Self::Boolean,
            b'B' => Self::Byte,
            b'C' => Self::Char,
            b'S' => Self::Short,
            b'I' => Self::Int,
            b'F' => Self::Float,
            b'J' => Self::Long,
            b'D' => Self::Double,
            _ => return None,
        })
    }

    #[inline]
    pub const fn size(self) -> u32 {
        match self {
            Self::Boolean | Self::Byte => 1,
            Self::Char | Self::Short => 2,
            Self::Int | Self::Float => 4,
            Self::Reference | Self::Long | Self::Double => 8,
        }
    }

    #[inline]
    pub const fn alignment(self) -> u32 {
        self.size()
    }

    #[inline]
    pub const fn is_reference(self) -> bool {
        matches!(self, Self::Reference)
    }

    /// Storage width honouring the process-wide narrow-oop setting.
    ///
    /// Identical to [`Self::size`] for every primitive kind and for references
    /// while compressed oops are off (the default). With compressed oops on, a
    /// reference occupies [`crate::narrow_oop::NARROW_REF_SIZE`] bytes instead
    /// of 8. This is the width the layout builder must use — [`Self::size`]
    /// stays `const` for the compile-time layout assertions.
    #[inline]
    pub fn size_runtime(self) -> u32 {
        match self {
            Self::Reference => crate::narrow_oop::ref_field_size() as u32,
            other => other.size(),
        }
    }

    /// Alignment honouring the process-wide narrow-oop setting. A narrow
    /// reference is 4-byte aligned, matching its width.
    #[inline]
    pub fn alignment_runtime(self) -> u32 {
        self.size_runtime()
    }
}

/// Per-class instance-field layout for the compact reference-field model.
///
/// Indexed by **absolute field index** (`declaring.first_field_index +
/// instance_idx`), the hierarchy-wide stable index used by `get_field` /
/// `set_field`. A parent's layout is an exact prefix of every child's layout
/// (declaration order, supers first), so inherited indices map to identical
/// offsets across the hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactLayout {
    /// Byte offset within the object body (relative to `HEADER_SIZE`) of each
    /// field index. `field_offsets.len()` == instance-field count.
    pub field_offsets: Vec<u32>,
    /// Whether each field index holds a reference (descriptor `L`/`[`, or an
    /// unknown/uncovered descriptor — treated as a reference, matching the heap
    /// default-init rule).
    pub is_ref: Vec<bool>,
    /// Descriptor-derived tagless storage representation for each field.
    pub field_kinds: Vec<FieldStorageKind>,
    /// Byte offsets (relative to `HEADER_SIZE`) of every reference field, in
    /// ascending order. This is the GC oop-map: the only slots the collector
    /// scans/remaps for an instance of this class.
    pub ref_offsets: Vec<u32>,
    /// Total object body size in bytes, rounded to 8-byte object alignment.
    /// Fields use their natural Java payload width (1/2/4/8 bytes).
    pub body_size: u32,
}

impl CompactLayout {
    /// Byte offset of field `index` within the body, or `None` if out of range.
    #[inline]
    pub fn field_offset(&self, index: usize) -> Option<u32> {
        self.field_offsets.get(index).copied()
    }

    /// Whether field `index` is a reference, or `None` if out of range.
    #[inline]
    pub fn field_is_ref(&self, index: usize) -> Option<bool> {
        self.is_ref.get(index).copied()
    }

    /// Tagless storage representation of field `index`.
    #[inline]
    pub fn field_storage(&self, index: usize) -> Option<FieldStorageKind> {
        self.field_kinds.get(index).copied()
    }

    /// Instance-field count (bounds for `get_field`/`set_field`).
    #[inline]
    pub fn field_count(&self) -> usize {
        self.field_offsets.len()
    }
}

// --- Process-wide enable flag -------------------------------------------------

static COMPACT_ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether the compact reference-field layout is enabled for this process.
///
/// Read once from `CRATONVM_COMPACT_REF_FIELDS` and cached for the lifetime of
/// the process — the layout must be fixed so objects are never read under a
/// different layout than they were written. On by default; set
/// `CRATONVM_COMPACT_REF_FIELDS=0` (or `false`/`off`/`no`) to use the legacy
/// uniform 16-byte-cell layout for A/B runs.
#[inline]
pub fn compact_ref_fields_enabled() -> bool {
    *COMPACT_ENABLED.get_or_init(|| match crate::flags::runtime_var("CRATONVM_COMPACT_REF_FIELDS") {
        Ok(value) => {
            let value = value.trim();
            !matches!(
                value,
                "0" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" | "no" | "No" | "NO"
            )
        }
        Err(_) => true,
    })
}

/// Force the enable flag to a specific value (test-only / explicit VM config).
/// Idempotent — only the first set (whether via this fn or the env probe) wins.
pub fn set_compact_ref_fields_enabled(enabled: bool) {
    let _ = COMPACT_ENABLED.set(enabled);
}

// --- Per-class layout registry ------------------------------------------------

/// Dense `class_id -> layout` registry. `class_id`s are assigned densely from 0,
/// so a `Vec` indexed by id is compact. Only populated when the flag is on.
static CLASS_LAYOUTS: RwLock<Vec<Option<Arc<CompactLayout>>>> = RwLock::new(Vec::new());
/// Version registry: `(class_id, field_count) -> layout`.
///
/// `parking_lot::RwLock<FxHashMap<..>>`, not `std::sync::RwLock<HashMap<..>>`.
/// The std pairing cost real time: `class_layout_for_fields` is called once per
/// scanned object by the Cheney collector's scan loops (`gc/src/gc.rs`), and
/// `std::collections::HashMap`'s default hasher is SipHash — a cryptographic
/// hash over an 8-byte `(u32, u32)` key. A `perf record` of
/// `BinTreesClassic d=18` attributed **37.8%** of all samples to
/// `class_layout_for_fields` and a further **4.8%** to
/// `<sip::Hasher as Hasher>::write`. FxHash over two words is a couple of
/// multiplies, and `parking_lot`'s reader acquire is cheaper than std's.
static CLASS_LAYOUT_VERSIONS: LazyLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<(u32, u32), Arc<CompactLayout>>>,
> = LazyLock::new(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));

/// Reverse index over [`CLASS_LAYOUT_VERSIONS`]: `class_id -> its field
/// counts`. Exists solely so [`unregister_class_layout`] can drop one class's
/// versions without a full-map `retain`.
///
/// A `retain` is `O(all registered versions)` and class unloading walks every
/// class of a dying loader, so the sweep was quadratic in the number of loaded
/// classes — the same "linear scan where an index belongs" shape the registry
/// above was rewritten to remove. Held under the *same* lock discipline: both
/// maps are only ever mutated inside [`register_class_layout`] /
/// [`unregister_class_layout`], and the versions lock is taken first in both,
/// so the pair cannot deadlock or diverge.
static CLASS_LAYOUT_VERSION_KEYS: LazyLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<u32, Vec<u32>>>,
> = LazyLock::new(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));

/// Maximum slot count for the dense layout registry.
///
/// Class IDs are expected to be allocator-issued and dense. A sparse or corrupt
/// ID near `u32::MAX` would otherwise make `register_class_layout` try to resize
/// the Vec to billions of entries. One million classes is already far above the
/// current VM scale; if that ever becomes a real workload, replace this registry
/// with a sparse map instead of raising the limit blindly.
const MAX_DENSE_CLASS_LAYOUTS: usize = 1 << 20;

/// Monotonic generation, bumped on every (re)registration. Lets hot-path
/// consumers (the GC scan) cache a `(class_id -> Arc)` lookup and cheaply
/// validate it with a single atomic load instead of re-taking the registry
/// `RwLock` per scanned object. A redefine bumps this, invalidating caches.
static LAYOUT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-class layout REPLACEMENT counters (perf/halfgap-20260717).
///
/// `LAYOUT_GENERATION` bumps on every registration — including brand-new
/// classes — so JIT code that baked a layout at compile time cannot use it
/// as a staleness guard without degrading on every subsequent class load.
/// This table counts only REPLACEMENTS (an existing `class_id`'s layout
/// swapped for a different one — the synthetic-stub→real-bytecode upgrade
/// that made baked compact sizes corrupt the heap, see the
/// GROOVY-CLUSTER-20260717 note in `jit/src/x64.rs`). JIT code bakes the
/// slot's ADDRESS and its value at compile time and emits a 2-instruction
/// guard (load + compare); a mismatch routes to the always-correct helper.
///
/// Fixed-capacity so the slot addresses baked into machine code can never
/// dangle: 4 bytes × `MAX_DENSE_CLASS_LAYOUTS` = 4 MiB, virtually allocated
/// once and touched lazily per 1024-class page.
static LAYOUT_REPLACE_COUNTS: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU32]>> =
    std::sync::OnceLock::new();

fn layout_replace_counts() -> &'static [std::sync::atomic::AtomicU32] {
    LAYOUT_REPLACE_COUNTS.get_or_init(|| {
        let mut v = Vec::with_capacity(MAX_DENSE_CLASS_LAYOUTS);
        v.resize_with(MAX_DENSE_CLASS_LAYOUTS, || {
            std::sync::atomic::AtomicU32::new(0)
        });
        v.into_boxed_slice()
    })
}

/// Stable address of `class_id`'s replace counter (for baking into JIT
/// code) plus its current value. The address is valid for the process
/// lifetime — the table is fixed-capacity and never reallocates.
pub fn layout_replace_guard(class_id: u32) -> (*const u32, u32) {
    let counts = layout_replace_counts();
    let idx = (class_id as usize).min(counts.len() - 1);
    let slot = &counts[idx];
    (
        slot as *const std::sync::atomic::AtomicU32 as *const u32,
        slot.load(std::sync::atomic::Ordering::Acquire),
    )
}

/// Current layout-registry generation (see [`LAYOUT_GENERATION`]).
#[inline]
pub fn layout_generation() -> u64 {
    LAYOUT_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Register (or replace, on redefine) the compact layout for a class.
pub fn register_class_layout(class_id: u32, layout: Arc<CompactLayout>) {
    let idx = class_id as usize;
    assert!(
        idx < MAX_DENSE_CLASS_LAYOUTS,
        "compact field-layout class_id {class_id} exceeds dense registry limit \
         ({MAX_DENSE_CLASS_LAYOUTS}); refusing sparse allocation"
    );

    let field_count = u32::try_from(layout.field_count())
        .expect("compact field count exceeds u32");
    {
        let mut versions = CLASS_LAYOUT_VERSIONS.write();
        if versions
            .insert((class_id, field_count), Arc::clone(&layout))
            .is_none()
        {
            // First time this class has been registered at this field count —
            // record the key so `unregister_class_layout` can find it without
            // scanning the whole version map.
            let mut keys = CLASS_LAYOUT_VERSION_KEYS.write();
            keys.entry(class_id).or_default().push(field_count);
        }
    }
    let mut v = CLASS_LAYOUTS.write().unwrap();
    if idx >= v.len() {
        v.resize(idx + 1, None);
    }
    // REPLACEMENT (not first registration) invalidates any JIT code that
    // baked this class's layout — bump the per-class replace counter BEFORE
    // publishing the new layout, still under the write lock, so a baked
    // guard that passes (sees the old count) can only have raced an
    // already-correct old layout, never observe new-count + old-layout.
    if v[idx].is_some() {
        layout_replace_counts()[idx].fetch_add(1, std::sync::atomic::Ordering::Release);
    }
    v[idx] = Some(layout);
    // Bump after the store so a reader that observes the new generation also
    // observes the new entry (Release pairs with the Acquire in
    // `layout_generation`). Done under the write lock, so no torn updates.
    LAYOUT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Drop the compact field layout owned by an unloaded class.
///
/// The registry is slot-indexed and ClassIds are monotonic, so clearing the
/// slot releases the layout allocation without allowing a future class to
/// inherit it.
pub fn unregister_class_layout(class_id: u32) {
    let Ok(index) = usize::try_from(class_id) else {
        return;
    };
    {
        // O(this class's versions), not O(every registered version): see
        // `CLASS_LAYOUT_VERSION_KEYS`. Versions lock first, matching
        // `register_class_layout`.
        let mut versions = CLASS_LAYOUT_VERSIONS.write();
        if let Some(field_counts) = CLASS_LAYOUT_VERSION_KEYS.write().remove(&class_id) {
            for fc in field_counts {
                versions.remove(&(class_id, fc));
            }
        }
    }
    let mut layouts = CLASS_LAYOUTS.write().unwrap();
    if let Some(slot) = layouts.get_mut(index) {
        if slot.take().is_some() {
            if index < MAX_DENSE_CLASS_LAYOUTS {
                layout_replace_counts()[index]
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
            }
            LAYOUT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Look up the compact layout for a class, if registered.
#[inline]
pub fn class_layout(class_id: u32) -> Option<Arc<CompactLayout>> {
    let v = CLASS_LAYOUTS.read().unwrap();
    v.get(class_id as usize).and_then(|o| o.clone())
}

const CURRENT_CACHE_LEN: usize = 8;

struct CurrentLayoutCache {
    entries: [Option<(u32, u64, Option<Arc<CompactLayout>>)>; CURRENT_CACHE_LEN],
    next: std::cell::Cell<usize>,
    mru: std::cell::Cell<usize>,
}

impl CurrentLayoutCache {
    const fn new() -> Self {
        Self {
            entries: [None, None, None, None, None, None, None, None],
            next: std::cell::Cell::new(0),
            mru: std::cell::Cell::new(0),
        }
    }

    #[inline]
    fn find(&self, class_id: u32, generation: u64) -> Option<usize> {
        let matches = |entry: &Option<(u32, u64, Option<Arc<CompactLayout>>)>| {
            matches!(entry, Some((cid, gen, _)) if *cid == class_id && *gen == generation)
        };
        let mru = self.mru.get();
        if self.entries.get(mru).is_some_and(matches) {
            return Some(mru);
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if matches(entry) {
                self.mru.set(index);
                return Some(index);
            }
        }
        None
    }

    #[inline]
    fn install(
        &mut self,
        class_id: u32,
        generation: u64,
        resolved: Option<Arc<CompactLayout>>,
    ) -> usize {
        let slot = self.next.get() % CURRENT_CACHE_LEN;
        self.next.set(self.next.get().wrapping_add(1));
        self.mru.set(slot);
        self.entries[slot] = Some((class_id, generation, resolved));
        slot
    }
}

thread_local! {
    static CURRENT_LAYOUT_CACHE: std::cell::RefCell<CurrentLayoutCache> =
        const { std::cell::RefCell::new(CurrentLayoutCache::new()) };
}

/// Run `f` against the currently published layout for `class_id`.
///
/// Allocation and compiled/interpreted field-resolution paths ask for the
/// current class recipe millions of times. Returning an owned `Arc` from
/// [`class_layout`] paid an `RwLock` acquisition plus an atomic clone/drop on
/// every object allocation. This generation-validated per-thread working set
/// retains the handles and borrows them in place. Negative lookups are cached
/// as well, which keeps legacy/padded synthetic classes off the registry lock.
///
/// Unlike [`with_class_layout`], this deliberately does not address historical
/// `(class_id, field_count)` versions: allocation must use only the current
/// class recipe. A stale field-count request therefore continues to fall back
/// to a legacy self-describing object rather than minting a compact object from
/// an obsolete layout.
#[inline]
pub fn with_current_class_layout<R>(
    class_id: u32,
    f: impl FnOnce(&CompactLayout) -> R,
) -> Option<R> {
    let generation = layout_generation();
    CURRENT_LAYOUT_CACHE.with(|cell| {
        match cell.try_borrow() {
            Ok(cache) => {
                if let Some(index) = cache.find(class_id, generation) {
                    return cache.entries[index]
                        .as_ref()
                        .and_then(|entry| entry.2.as_deref())
                        .map(f);
                }
            }
            Err(_) => return class_layout(class_id).map(|layout| f(&layout)),
        }

        let resolved = class_layout(class_id);
        match cell.try_borrow_mut() {
            Ok(mut cache) => {
                let index = cache.install(class_id, generation, resolved);
                drop(cache);
                match cell.try_borrow() {
                    Ok(cache) => cache.entries[index]
                        .as_ref()
                        .and_then(|entry| entry.2.as_deref())
                        .map(f),
                    Err(_) => class_layout(class_id).map(|layout| f(&layout)),
                }
            }
            Err(_) => resolved.map(|layout| f(&layout)),
        }
    })
}

/// Small per-thread working set for the `(class_id, field_count) -> layout`
/// lookup. The GC scans long runs of same-class objects, so a handful of entries
/// captures essentially all traffic; 8 matches the sibling cache in
/// [`compact_field_storage`].
const VERSION_CACHE_LEN: usize = 8;

struct VersionCache {
    /// `(class_id, field_count, generation, resolved)`. `resolved` is `None` for
    /// a cached negative lookup — see [`class_layout_for_fields`] for why
    /// negatives are cached too.
    entries: [Option<(u32, u32, u64, Option<Arc<CompactLayout>>)>; VERSION_CACHE_LEN],
    /// Round-robin insertion cursor. A [`Cell`](std::cell::Cell) so that the
    /// lookup path needs only a *shared* `RefCell` borrow — see [`Self::find`].
    next: std::cell::Cell<usize>,
    /// Index of the entry that satisfied the previous lookup, checked before the
    /// linear scan.
    ///
    /// The GC scans long *runs* of same-class objects (a binary tree of one node
    /// type; a young gen full of `HashMap$Node`s), so consecutive calls almost
    /// always want the same entry. Probing it first turns the common case from a
    /// scan of up to `VERSION_CACHE_LEN` tuples into one comparison.
    mru: std::cell::Cell<usize>,
}

impl VersionCache {
    const fn new() -> Self {
        Self {
            entries: [None, None, None, None, None, None, None, None],
            next: std::cell::Cell::new(0),
            mru: std::cell::Cell::new(0),
        }
    }

    /// Index of the live entry matching `(class_id, field_count)` at
    /// `generation`, or `None` for a miss.
    ///
    /// Takes `&self`, not `&mut self` — the MRU bookkeeping goes through
    /// [`std::cell::Cell`] specifically so the *whole lookup path* runs under a
    /// shared `RefCell` borrow. That is what makes the cache re-entrant: see
    /// [`with_class_layout`], which holds a shared borrow across a user closure
    /// that calls back into this cache. Only insertion needs `&mut self`.
    #[inline]
    fn find(&self, class_id: u32, field_count: u32, generation: u64) -> Option<usize> {
        let matches = |entry: &Option<(u32, u32, u64, Option<Arc<CompactLayout>>)>| {
            matches!(entry, Some((cid, fc, gen, _))
                if *cid == class_id && *fc == field_count && *gen == generation)
        };
        // MRU probe: the GC scans runs of same-class objects, so this hits on
        // nearly every call and skips the scan below entirely.
        let mru = self.mru.get();
        if self.entries.get(mru).is_some_and(matches) {
            return Some(mru);
        }
        for (idx, entry) in self.entries.iter().enumerate() {
            if matches(entry) {
                // Self-tune the probe to the run the caller is currently in.
                self.mru.set(idx);
                return Some(idx);
            }
        }
        None
    }

    /// Install `resolved` (which may be a cached negative) and return its slot.
    #[inline]
    fn install(
        &mut self,
        class_id: u32,
        field_count: u32,
        generation: u64,
        resolved: Option<Arc<CompactLayout>>,
    ) -> usize {
        let slot = self.next.get() % VERSION_CACHE_LEN;
        self.next.set(self.next.get().wrapping_add(1));
        self.mru.set(slot);
        self.entries[slot] = Some((class_id, field_count, generation, resolved));
        slot
    }
}

thread_local! {
    static VERSION_CACHE: std::cell::RefCell<VersionCache> =
        const { std::cell::RefCell::new(VersionCache::new()) };
}

/// The uncached registry probe both accessors fall back to: an `RwLock` reader
/// acquire plus a SipHash probe. Everything above exists to avoid this.
#[inline]
fn registry_layout_for_fields(class_id: u32, field_count: u32) -> Option<Arc<CompactLayout>> {
    CLASS_LAYOUT_VERSIONS
        .read()
        .get(&(class_id, field_count))
        .cloned()
}

/// Resolve the immutable layout version used by an object with this exact
/// hierarchy-wide field count. Retaining versions makes object size and oop
/// maps stable across append-only synthetic-class upgrades.
///
/// Prefer [`with_class_layout`] on hot read-only paths: it serves the same cache
/// without the `Arc` clone/drop pair of atomics.
///
/// PERF: this is called **once per scanned object** by the collectors' scan
/// loops, and the naked registry lookup underneath — an `RwLock` reader
/// acquire, a hash probe, and an `Arc` clone/drop pair of atomics — dominated
/// allocation-heavy workloads. A `perf record` of `BinTreesClassic d=18` (whose
/// young gen is entirely `Node` objects, so the collector re-resolves the same
/// one or two layouts for every object it scans, every collection) attributed
/// **37.8%** of all samples to this function and another **4.8%** to SipHash
/// inside it.
///
/// The scan sites themselves have since moved to [`with_class_layout`]; what
/// still calls *this* form is code that genuinely needs to own the handle —
/// notably `compact_oop_scan` in `gc/src/heap.rs`, which returns it.
///
/// This is a recurrence of a fix already applied elsewhere: `compact_field_slot`
/// below, and `compact_oop_scan` in `gc/src/heap.rs`, each grew their own
/// generation-validated thread-local cache for exactly this reason — but they
/// are per-caller band-aids around one uncached primitive, and the `gc.rs` scan
/// loops called the primitive directly and so were never covered. Caching *here*
/// covers every call site at once. (`compact_field_slot`'s private copy has been
/// deleted as redundant; `compact_oop_scan`'s remains.)
///
/// [`LAYOUT_GENERATION`] was introduced for precisely this purpose (see its
/// doc: "lets hot-path consumers (the GC scan) cache a lookup and cheaply
/// validate it with a single atomic load instead of re-taking the registry
/// `RwLock` per scanned object"). A hit costs one relaxed atomic load, one
/// comparison against the MRU entry (the GC scans runs of same-class objects, so
/// that hits on nearly every call; a miss falls back to scanning the 8-entry
/// array), and an `Arc` clone; a registration or redefine bumps the generation
/// and invalidates every thread's cache at once.
///
/// The `Arc` clone/drop is two atomic RMWs paid per call. Callers that only
/// *read* the layout and drop the handle immediately — which is every GC scan
/// site — should use [`with_class_layout`] instead, which serves the same cache
/// entry by reference and pays neither.
///
/// Negative results are cached too (`None`): callers that do not pre-filter
/// with [`is_compact_object`] would otherwise re-probe the registry for every
/// legacy object, which is the same cost this exists to avoid.
#[inline]
pub fn class_layout_for_fields(class_id: u32, field_count: u32) -> Option<Arc<CompactLayout>> {
    let generation = layout_generation();
    VERSION_CACHE.with(|cell| {
        // `try_borrow`/`try_borrow_mut` rather than the panicking forms: this
        // function is reachable from GC scan paths (and from inside a
        // `with_class_layout` closure), and a panic-on-reentrancy here would
        // abort a collection. A failed borrow just skips the cache.
        //
        // Taking the *shared* borrow for the lookup is what makes re-entrancy
        // cheap rather than merely survivable: a nested call still hits the
        // cache, because shared borrows nest. Only a nested call that also
        // *misses* falls through to the registry — installing needs the
        // exclusive borrow, which the enclosing `with_class_layout` frame's
        // shared borrow rules out.
        match cell.try_borrow() {
            Ok(cache) => {
                if let Some(idx) = cache.find(class_id, field_count, generation) {
                    return cache.entries[idx].as_ref().and_then(|e| e.3.clone());
                }
            }
            Err(_) => return registry_layout_for_fields(class_id, field_count),
        }
        let resolved = registry_layout_for_fields(class_id, field_count);
        if let Ok(mut cache) = cell.try_borrow_mut() {
            cache.install(class_id, field_count, generation, resolved.clone());
        }
        resolved
    })
}

/// Borrowing form of [`class_layout_for_fields`]: runs `f` against the cached
/// layout in place and returns its result, or `None` if no layout is registered
/// for `(class_id, field_count)`.
///
/// PERF: this exists for the GC scan loops, which call the lookup **once per
/// scanned object**, read only `ref_offsets` / `field_offsets` / `body_size`,
/// and drop the handle immediately. [`class_layout_for_fields`] hands them an
/// `Arc`, so each of those calls pays a clone/drop pair of atomic RMWs for a
/// handle that never outlives the statement. Serving the same cache entry by
/// reference removes both.
///
/// # `f` runs with the cache borrowed
///
/// `f` executes while this thread's cache is borrowed *shared*, which has two
/// consequences callers must know about:
///
///   * Re-entrancy is safe and still fast. A call back into
///     [`class_layout_for_fields`] or [`with_class_layout`] from inside `f`
///     takes another shared borrow and hits normally. This is not hypothetical:
///     the Cheney scan loops call `forward_object` from inside `f`, which
///     reaches [`object_body_size`] and back into this cache for the *referent's*
///     class on every object it copies.
///   * A re-entrant call that *misses* cannot install its result — installation
///     needs the exclusive borrow — so it degrades to one uncached registry
///     probe. That is a correctness-preserving slow path, not a stale answer,
///     and it self-heals as soon as a non-nested call caches the class.
///
/// Borrowing rather than cloning is sound across `f` for the same reason: the
/// shared borrow blocks any `install` that could overwrite the entry, and the
/// entry owns an `Arc`, so the layout stays alive even if a concurrent redefine
/// drops the registry's own handle.
#[inline]
pub fn with_class_layout<R>(
    class_id: u32,
    field_count: u32,
    f: impl FnOnce(&CompactLayout) -> R,
) -> Option<R> {
    let generation = layout_generation();
    VERSION_CACHE.with(|cell| {
        match cell.try_borrow() {
            Ok(cache) => {
                if let Some(idx) = cache.find(class_id, field_count, generation) {
                    // Cache hit. `f` runs with the shared borrow live.
                    return cache.entries[idx]
                        .as_ref()
                        .and_then(|e| e.3.as_deref())
                        .map(f);
                }
            }
            // Cache unusable (an `install` is on this thread's stack). Fall back
            // to an owning registry read; the `Arc` here is unavoidable but this
            // path is not reachable from steady-state GC scanning.
            Err(_) => return registry_layout_for_fields(class_id, field_count).map(|a| f(&a)),
        }
        // Miss: resolve and install under a *short* exclusive borrow that never
        // spans user code, then re-borrow shared to run `f` against the entry.
        let resolved = registry_layout_for_fields(class_id, field_count);
        match cell.try_borrow_mut() {
            Ok(mut cache) => {
                let idx = cache.install(class_id, field_count, generation, resolved);
                drop(cache);
                // `try_borrow`, not `borrow`: nothing can be holding a borrow
                // here (the exclusive one was just dropped and no user code has
                // run since), but this is a GC path where a panic aborts the
                // collection, so re-resolve rather than rely on that reasoning.
                // Falling through to `None` would be worse than slow — it would
                // claim the class has no layout.
                match cell.try_borrow() {
                    Ok(cache) => cache.entries[idx]
                        .as_ref()
                        .and_then(|e| e.3.as_deref())
                        .map(f),
                    Err(_) => registry_layout_for_fields(class_id, field_count).map(|a| f(&a)),
                }
            }
            Err(_) => resolved.map(|a| f(&a)),
        }
    })
}

/// `(byte_offset, is_ref)` for field `index` of `class_id`, if a compact layout
/// is registered (and the index is in range); else `None`. Used by the JIT
/// field helpers, which only have the raw object pointer (`class_id` from the
/// header) and the resolved field index — not a heap handle.
///
/// PERF (perf/halfgap-20260717): this is on the per-field-access hot path of
/// every JIT `putfield`/`getfield` helper and several interpreter compact
/// arms, and the naked `class_layout` lookup underneath it (registry RwLock
/// read + `Arc` clone/drop per call) measured **41% of all CPU** in a
/// BinTreesClassic d=18 profile (every `Node.left/right` store resolving the
/// same layout through the lock). Mirror the generation-validated
/// small-working-set thread-local cache that `gc/src/gen_heap.rs`'s own
/// `compact_field_slot` has used since 2026-07-11 (same key, same
/// invalidation rule: a class redefine bumps `layout_generation`, which
/// makes every cached entry miss). Layout *content* per `class_id` is
/// immutable once registered — redefines replace the Arc and bump the
/// generation — so a generation-validated hit can never serve a stale
/// layout.
#[inline]
pub fn compact_field_slot(class_id: u32, index: usize) -> Option<(usize, bool)> {
    compact_field_storage(class_id, index)
        .map(|(offset, storage)| (offset, storage.is_reference()))
}

/// `(byte_offset, storage_kind)` for a field in a registered compact layout.
#[inline]
pub fn compact_field_storage(
    class_id: u32,
    index: usize,
) -> Option<(usize, FieldStorageKind)> {
    with_current_class_layout(class_id, |layout| {
        Some((
            layout.field_offset(index)? as usize,
            layout.field_storage(index)?,
        ))
    })
    .flatten()
}

/// Return the compact body size when `class_id` has a complete layout matching
/// the requested instance-field count. Allocation paths in every collector use
/// this one predicate, so an object can never be marked compact with a partial
/// or stale class recipe.
#[inline]
pub fn compact_object_body_size(class_id: u32, field_count: usize) -> Option<usize> {
    if !compact_ref_fields_enabled() {
        return None;
    }
    with_current_class_layout(class_id, |layout| {
        (layout.field_count() == field_count).then_some(layout.body_size as usize)
    })
    .flatten()
}

/// Resolve a field of an object that is actually marked compact.
#[inline]
pub fn compact_object_field_storage(
    header: &ObjectHeader,
    index: usize,
) -> Option<(usize, FieldStorageKind)> {
    if !is_compact_object(header) {
        return None;
    }
    let layout = class_layout_for_fields(header.class_id.as_u32(), header.num_slots())?;
    Some((
        layout.field_offset(index)? as usize,
        layout.field_storage(index)?,
    ))
}

/// Atomically read a tagless compact field and reconstruct its VM `Value`.
///
/// # Safety
/// `ptr` must point to a naturally aligned field of the supplied storage kind.
#[inline]
pub unsafe fn read_compact_field(
    ptr: *const u8,
    storage: FieldStorageKind,
    ordering: Ordering,
) -> Value {
    match storage {
        FieldStorageKind::Reference => {
            // Narrow oops: the slot is a 4-byte `(addr - base) >> shift`. The
            // branch reads a process-wide flag written once at VM init, so it
            // predicts perfectly and folds away in the common case.
            let raw = if crate::narrow_oop::narrow_oops_enabled() {
                let n = unsafe { (&*(ptr as *const AtomicU32)).load(ordering) };
                crate::narrow_oop::decode(n)
            } else {
                unsafe { (&*(ptr as *const AtomicU64)).load(ordering) }
            };
            if raw == 0 {
                Value::Object(None)
            } else {
                Value::Object(Some(unsafe { ObjectRef::from_raw(raw as usize as *mut u8) }))
            }
        }
        FieldStorageKind::Boolean => {
            Value::Int((unsafe { (&*(ptr as *const AtomicU8)).load(ordering) } != 0) as i32)
        }
        FieldStorageKind::Byte => Value::Int(
            unsafe { (&*(ptr as *const AtomicU8)).load(ordering) } as i8 as i32,
        ),
        FieldStorageKind::Char => Value::Int(
            unsafe { (&*(ptr as *const AtomicU16)).load(ordering) } as i32,
        ),
        FieldStorageKind::Short => Value::Int(
            unsafe { (&*(ptr as *const AtomicU16)).load(ordering) } as i16 as i32,
        ),
        FieldStorageKind::Int => Value::Int(
            unsafe { (&*(ptr as *const AtomicU32)).load(ordering) } as i32,
        ),
        FieldStorageKind::Float => Value::Float(f32::from_bits(
            unsafe { (&*(ptr as *const AtomicU32)).load(ordering) },
        )),
        FieldStorageKind::Long => Value::Long(
            unsafe { (&*(ptr as *const AtomicU64)).load(ordering) } as i64,
        ),
        FieldStorageKind::Double => Value::Double(f64::from_bits(
            unsafe { (&*(ptr as *const AtomicU64)).load(ordering) },
        )),
    }
}

/// Atomically store a VM `Value` into tagless compact field storage.
///
/// Descriptor-compatible values are expected on verified bytecode paths.
/// Integer-family writes are narrowed according to JVMS field semantics.
///
/// # Safety
/// `ptr` must point to a naturally aligned field of the supplied storage kind.
#[inline]
pub unsafe fn write_compact_field(
    ptr: *mut u8,
    storage: FieldStorageKind,
    value: Value,
    ordering: Ordering,
) {
    match storage {
        FieldStorageKind::Reference => {
            let raw = match value {
                Value::Object(Some(r)) => r.as_ptr() as u64,
                Value::Object(None) => 0,
                _ => 0,
            };
            crate::narrow_oop::probe(raw);
            if crate::narrow_oop::narrow_oops_enabled() {
                let n = crate::narrow_oop::encode(raw);
                unsafe { (&*(ptr as *const AtomicU32)).store(n, ordering) };
            } else {
                unsafe { (&*(ptr as *const AtomicU64)).store(raw, ordering) };
            }
        }
        FieldStorageKind::Boolean => {
            let raw = matches!(value, Value::Int(v) if v != 0) as u8;
            unsafe { (&*(ptr as *const AtomicU8)).store(raw, ordering) };
        }
        FieldStorageKind::Byte => {
            let raw = match value { Value::Int(v) => v as u8, _ => 0 };
            unsafe { (&*(ptr as *const AtomicU8)).store(raw, ordering) };
        }
        FieldStorageKind::Char | FieldStorageKind::Short => {
            let raw = match value { Value::Int(v) => v as u16, _ => 0 };
            unsafe { (&*(ptr as *const AtomicU16)).store(raw, ordering) };
        }
        FieldStorageKind::Int => {
            let raw = match value {
                Value::Int(v) => v as u32,
                Value::Float(v) => v.to_bits(),
                _ => 0,
            };
            unsafe { (&*(ptr as *const AtomicU32)).store(raw, ordering) };
        }
        FieldStorageKind::Float => {
            let raw = match value {
                Value::Float(v) => v.to_bits(),
                Value::Int(v) => v as u32,
                _ => 0,
            };
            unsafe { (&*(ptr as *const AtomicU32)).store(raw, ordering) };
        }
        FieldStorageKind::Long => {
            let raw = match value {
                Value::Long(v) => v as u64,
                Value::Int(v) => v as i64 as u64,
                Value::Double(v) => v.to_bits(),
                _ => 0,
            };
            unsafe { (&*(ptr as *const AtomicU64)).store(raw, ordering) };
        }
        FieldStorageKind::Double => {
            let raw = match value {
                Value::Double(v) => v.to_bits(),
                Value::Long(v) => v as u64,
                Value::Int(v) => v as i64 as u64,
                _ => 0,
            };
            unsafe { (&*(ptr as *const AtomicU64)).store(raw, ordering) };
        }
    }
}

/// Clear the registry (test/debug-only).
#[cfg(any(test, debug_assertions))]
pub fn clear_class_layouts() {
    CLASS_LAYOUTS.write().unwrap().clear();
    CLASS_LAYOUT_VERSIONS.write().clear();
    CLASS_LAYOUT_VERSION_KEYS.write().clear();
    // The per-thread `class_layout_for_fields` cache is validated against
    // `layout_generation()`, so bump it here: without this, a test that clears
    // the registry and re-registers could be served a pre-clear entry from
    // whichever thread had already warmed its cache.
    LAYOUT_GENERATION.fetch_add(1, Ordering::AcqRel);
}

// --- Object body size --------------------------------------------------------

/// Whether this object instance uses the compact reference-field layout.
/// Decided per-object via the [`GC_FLAG_COMPACT`] header bit (set at alloc).
#[inline]
pub fn is_compact_object(header: &ObjectHeader) -> bool {
    header.gc_flags & GC_FLAG_COMPACT != 0
}

/// Total instance-field body size in bytes (excludes `HEADER_SIZE`), honouring
/// this object's layout. Object total size = `HEADER_SIZE + object_body_size`.
///
/// For a compact object the body size is stored in the header's `array_length`
/// field — unused for `kind == Object` (arrays keep it for their length). For a
/// legacy object it is `num_slots * SLOT_SIZE`. Callers must only invoke this
/// for `kind == Object` (arrays size via `array_data_size`).
#[inline]
pub fn object_body_size(header: &ObjectHeader) -> usize {
    if is_compact_object(header) {
        // `with_class_layout`, not `class_layout_for_fields`: this reads one
        // `u32` and drops the handle. It is also the hottest *nested* consumer
        // of the layout cache — the Cheney collector reaches it once per copied
        // object, from `object_total_size` inside `forward_object`, which the
        // scan loops call from inside their own `with_class_layout` closure.
        with_class_layout(header.class_id.as_u32(), header.num_slots(), |layout| {
            layout.body_size as usize
        })
        .unwrap_or(0)
    } else {
        header.num_slots() as usize * SLOT_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn layout_offsets_and_oopmap() {
        // Class with fields: ref, int, ref  (descriptors L, I, L)
        // natural-width layout: ref@0, int@8, align the next ref to ref@16.
        let layout = CompactLayout {
            field_offsets: vec![0, 8, 16],
            is_ref: vec![true, false, true],
            field_kinds: vec![
                FieldStorageKind::Reference,
                FieldStorageKind::Int,
                FieldStorageKind::Reference,
            ],
            ref_offsets: vec![0, 16],
            body_size: 24,
        };
        assert_eq!(layout.field_offset(0), Some(0));
        assert_eq!(layout.field_offset(1), Some(8));
        assert_eq!(layout.field_offset(2), Some(16));
        assert_eq!(layout.field_offset(3), None);
        assert_eq!(layout.field_is_ref(1), Some(false));
        assert_eq!(layout.field_count(), 3);
        assert_eq!(layout.ref_offsets, vec![0, 16]);
    }

    /// Regression: mixed reference + `double`/`long`/`float` fields.
    ///
    /// The compact layout stores a reference field as a bare 8-byte pointer but
    /// keeps every primitive field (including `double`/`long`) as a 16-byte
    /// tagged `Value` cell. A primitive's byte offset therefore shifts by only
    /// **8** bytes per preceding reference field — NOT the uniform
    /// `index * SLOT_SIZE` the non-compact layout assumes. Reading a `double`
    /// with the non-compact assumption lands it on (or overlapping) a reference
    /// slot and pushes a pointer where a `double` is expected — the
    /// "expected double on stack, got ref(..)" class of failure this test
    /// guards against (compact-ref SIGSEGV follow-up, `docs/feature-designs/
    /// compact-ref-field-layout.md`).
    #[test]
    fn mixed_ref_double_long_float_offsets() {
        // Fields (declaration order): Object a; double x; Object b; double y;
        //                             long z;   Object c; float f
        // descriptors:                L        D         L        D
        //                             J        L         F
        // prefix-sum (ref=8, prim=16):
        //   a  L  off 0  (ref, +8)  -> 8
        //   x  D  off 8  (prim,+16) -> 24
        //   b  L  off 24 (ref, +8)  -> 32
        //   y  D  off 32 (prim,+16) -> 48
        //   z  J  off 48 (prim,+16) -> 64
        //   c  L  off 64 (ref, +8)  -> 72
        //   f  F  off 72 (prim,+16) -> 88
        let layout = CompactLayout {
            field_offsets: vec![0, 8, 16, 24, 32, 40, 48],
            is_ref: vec![true, false, true, false, false, true, false],
            field_kinds: vec![
                FieldStorageKind::Reference,
                FieldStorageKind::Double,
                FieldStorageKind::Reference,
                FieldStorageKind::Double,
                FieldStorageKind::Long,
                FieldStorageKind::Reference,
                FieldStorageKind::Float,
            ],
            ref_offsets: vec![0, 16, 40],
            body_size: 56,
        };

        // Cross-check the literal layout against the prefix-sum rule it encodes,
        // so a change to REF_FIELD_SIZE / SLOT_SIZE (or a mis-sized field) is
        // caught here rather than corrupting objects at runtime.
        let kinds = [
            FieldStorageKind::Reference,
            FieldStorageKind::Double,
            FieldStorageKind::Reference,
            FieldStorageKind::Double,
            FieldStorageKind::Long,
            FieldStorageKind::Reference,
            FieldStorageKind::Float,
        ];
        let mut expect_off = 0u32;
        let mut expect_refs = Vec::new();
        for (i, &kind) in kinds.iter().enumerate() {
            let align = kind.alignment();
            expect_off = (expect_off + align - 1) & !(align - 1);
            assert_eq!(
                layout.field_offset(i),
                Some(expect_off),
                "field {i} must be naturally aligned",
            );
            assert_eq!(
                layout.field_is_ref(i),
                Some(kind.is_reference()),
                "field {i} ref-ness"
            );
            if kind.is_reference() {
                expect_refs.push(expect_off);
            }
            expect_off += kind.size();
        }
        expect_off = (expect_off + 7) & !7;
        assert_eq!(
            layout.body_size, expect_off,
            "body size = sum of field sizes"
        );
        assert_eq!(
            layout.ref_offsets, expect_refs,
            "oop-map = reference offsets"
        );
        assert_eq!(layout.field_count(), 7);
        assert_eq!(layout.field_offset(7), None, "out-of-range index");

        // The crux of the bug: each `double` sits at its ref-shifted offset, and
        // its 16-byte cell never overlaps a reference slot. If the double were
        // read at the non-compact `index * SLOT_SIZE` offset it would land on a
        // ref slot and decode a pointer as a double.
        assert_eq!(layout.field_offset(1), Some(8)); // double x, one ref before
        assert_ne!(
            8,
            1 * SLOT_SIZE as u32,
            "non-compact assumption would use 16"
        );
        assert_eq!(layout.field_offset(3), Some(24)); // double y, two refs before
        assert_ne!(
            24,
            3 * SLOT_SIZE as u32,
            "non-compact assumption would use 48"
        );

        // No primitive field's [off, off+SLOT_SIZE) byte range may overlap any
        // reference field's [off, off+REF_FIELD_SIZE) range — that overlap is
        // exactly how a double read yields a ref's bytes.
        for (i, &kind) in kinds.iter().enumerate() {
            if kind.is_reference() {
                continue;
            }
            let p0 = layout.field_offset(i).unwrap();
            let p1 = p0 + kind.size();
            for &ro in &layout.ref_offsets {
                let r1 = ro + FieldStorageKind::Reference.size();
                assert!(
                    p1 <= ro || r1 <= p0,
                    "primitive field {i} at [{p0},{p1}) overlaps ref slot [{ro},{r1})",
                );
            }
        }
    }

    #[test]
    fn tagless_field_round_trip_all_storage_kinds() {
        let mut words = [0u64; 8];
        let ptr = words.as_mut_ptr() as *mut u8;
        let cases = [
            (FieldStorageKind::Boolean, Value::Int(1)),
            (FieldStorageKind::Byte, Value::Int(-17)),
            (FieldStorageKind::Char, Value::Int(0x20ac)),
            (FieldStorageKind::Short, Value::Int(-1234)),
            (FieldStorageKind::Int, Value::Int(-123_456_789)),
            (FieldStorageKind::Float, Value::Float(3.25)),
            (FieldStorageKind::Long, Value::Long(-9_876_543_210)),
            (FieldStorageKind::Double, Value::Double(-17.5)),
        ];
        for (kind, value) in cases {
            unsafe {
                write_compact_field(ptr, kind, value, Ordering::Release);
                assert_eq!(read_compact_field(ptr, kind, Ordering::Acquire), value);
            }
        }
        unsafe {
            write_compact_field(
                ptr,
                FieldStorageKind::Reference,
                Value::Object(None),
                Ordering::Release,
            );
            assert_eq!(
                read_compact_field(ptr, FieldStorageKind::Reference, Ordering::Acquire),
                Value::Object(None)
            );
        }
    }

    #[test]
    fn registry_round_trip() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        let layout = Arc::new(CompactLayout {
            field_offsets: vec![0],
            is_ref: vec![true],
            field_kinds: vec![FieldStorageKind::Reference],
            ref_offsets: vec![0],
            body_size: 8,
        });
        register_class_layout(7, layout.clone());
        assert!(class_layout(0).is_none());
        let got = class_layout(7).expect("registered");
        assert_eq!(got.body_size, 8);
        let layout2 = Arc::new(CompactLayout {
            field_offsets: vec![0, 8],
            is_ref: vec![true, true],
            field_kinds: vec![
                FieldStorageKind::Reference,
                FieldStorageKind::Reference,
            ],
            ref_offsets: vec![0, 8],
            body_size: 16,
        });
        register_class_layout(7, layout2);
        assert_eq!(class_layout(7).unwrap().body_size, 16);
        assert_eq!(class_layout_for_fields(7, 1).unwrap().body_size, 8);
        assert_eq!(class_layout_for_fields(7, 2).unwrap().body_size, 16);
        clear_class_layouts();
    }

    // --- class_layout_for_fields thread-local cache invalidation -------------
    //
    // The cache is validated against `layout_generation()`. Every registry
    // mutation must bump it, or a warmed thread would keep serving a stale
    // layout — which for the GC scan means walking an object with the wrong
    // oop-map (missed or fabricated reference slots => heap corruption). These
    // tests pin each mutation path.

    fn one_ref_layout(body_size: u32) -> Arc<CompactLayout> {
        Arc::new(CompactLayout {
            field_offsets: vec![0],
            is_ref: vec![true],
            field_kinds: vec![FieldStorageKind::Reference],
            ref_offsets: vec![0],
            body_size,
        })
    }

    /// Replacing a layout for the SAME (class_id, field_count) must be visible
    /// to a thread that already cached the old one.
    #[test]
    fn version_cache_sees_replacement() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        register_class_layout(21, one_ref_layout(8));
        // Warm the cache.
        assert_eq!(class_layout_for_fields(21, 1).unwrap().body_size, 8);
        // Replace with a different layout at the same field count.
        register_class_layout(21, one_ref_layout(24));
        assert_eq!(
            class_layout_for_fields(21, 1).unwrap().body_size,
            24,
            "cached entry survived a replacement — generation bump missing"
        );
        clear_class_layouts();
    }

    /// A cached NEGATIVE lookup must not hide a subsequent registration.
    #[test]
    fn version_cache_sees_registration_after_negative_lookup() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        // Warm a negative entry.
        assert!(class_layout_for_fields(22, 1).is_none());
        register_class_layout(22, one_ref_layout(8));
        assert!(
            class_layout_for_fields(22, 1).is_some(),
            "cached negative lookup hid a later registration"
        );
        clear_class_layouts();
    }

    /// `unregister_class_layout` must invalidate a warmed positive entry —
    /// otherwise an unloaded class's layout could be applied to a recycled id.
    #[test]
    fn version_cache_sees_unregister() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        register_class_layout(23, one_ref_layout(8));
        assert!(class_layout_for_fields(23, 1).is_some());
        unregister_class_layout(23);
        assert!(
            class_layout_for_fields(23, 1).is_none(),
            "cached entry survived unregister_class_layout"
        );
        clear_class_layouts();
    }

    /// `clear_class_layouts` must invalidate too (it is used between tests, so
    /// a surviving entry would cross-contaminate them).
    #[test]
    fn version_cache_sees_clear() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        register_class_layout(24, one_ref_layout(8));
        assert!(class_layout_for_fields(24, 1).is_some());
        clear_class_layouts();
        assert!(
            class_layout_for_fields(24, 1).is_none(),
            "cached entry survived clear_class_layouts"
        );
    }

    /// Every thread must agree with the registry, and repeated lookups (which
    /// exercise hits) must agree with the first (which was a miss). Also
    /// exercises eviction: more distinct keys than the 8-entry cache holds.
    #[test]
    fn version_cache_consistent_across_threads_and_evictions() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        // 20 distinct classes > VERSION_CACHE_LEN (8), so entries get evicted
        // and re-resolved; each body_size encodes its class id.
        for cid in 30..50u32 {
            register_class_layout(cid, one_ref_layout(cid * 8));
        }
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(std::thread::spawn(|| {
                // Two passes: the second pass hits entries the first warmed.
                for _ in 0..2 {
                    for cid in 30..50u32 {
                        let got = class_layout_for_fields(cid, 1)
                            .expect("registered layout must resolve");
                        assert_eq!(
                            got.body_size,
                            cid * 8,
                            "cache returned another class's layout for {cid}"
                        );
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("lookup worker should not panic");
        }
        clear_class_layouts();
    }

    /// `with_class_layout` must agree with `class_layout_for_fields` on both
    /// the miss (first call) and hit (second call) paths, and must report a
    /// missing layout as `None` rather than running `f`.
    #[test]
    fn with_class_layout_matches_owning_accessor() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        register_class_layout(60, one_ref_layout(40));
        // First call is a cache miss (installs), second is a hit.
        for pass in 0..2 {
            assert_eq!(
                with_class_layout(60, 1, |l| l.body_size),
                Some(40),
                "pass {pass}"
            );
            assert_eq!(
                with_class_layout(60, 1, |l| l.ref_offsets.clone()),
                Some(vec![0]),
                "pass {pass}"
            );
        }
        // Unregistered: `f` must not run.
        let mut ran = false;
        let got = with_class_layout(61, 1, |_| {
            ran = true;
        });
        assert!(got.is_none());
        assert!(!ran, "`f` ran for a class with no registered layout");
        clear_class_layouts();
    }

    /// Generation invalidation must apply to the borrowing accessor too — it
    /// shares one cache with `class_layout_for_fields`, so a replacement that
    /// the owning accessor sees must not be hidden from this one.
    #[test]
    fn with_class_layout_sees_replacement_and_unregister() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        register_class_layout(62, one_ref_layout(8));
        assert_eq!(with_class_layout(62, 1, |l| l.body_size), Some(8));
        register_class_layout(62, one_ref_layout(24));
        assert_eq!(
            with_class_layout(62, 1, |l| l.body_size),
            Some(24),
            "borrowed entry survived a replacement — generation bump missing"
        );
        unregister_class_layout(62);
        assert_eq!(
            with_class_layout(62, 1, |l| l.body_size),
            None,
            "borrowed entry survived unregister_class_layout"
        );
        clear_class_layouts();
    }

    #[test]
    fn current_layout_cache_invalidates_without_reusing_historical_recipe() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        register_class_layout(70, one_ref_layout(8));
        assert_eq!(compact_object_body_size(70, 1), Some(8));
        assert_eq!(
            with_current_class_layout(70, |layout| layout.body_size),
            Some(8)
        );

        let grown = Arc::new(CompactLayout {
            field_offsets: vec![0, 8],
            is_ref: vec![true, true],
            field_kinds: vec![
                FieldStorageKind::Reference,
                FieldStorageKind::Reference,
            ],
            ref_offsets: vec![0, 8],
            body_size: 16,
        });
        register_class_layout(70, grown);

        assert_eq!(
            compact_object_body_size(70, 1),
            None,
            "allocation must not resurrect the historical one-field layout"
        );
        assert_eq!(compact_object_body_size(70, 2), Some(16));
        assert_eq!(
            with_current_class_layout(70, |layout| layout.body_size),
            Some(16)
        );
        clear_class_layouts();
    }

    #[test]
    fn current_layout_cache_invalidates_negative_and_unregister_entries() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        assert!(with_current_class_layout(71, |_| ()).is_none());
        register_class_layout(71, one_ref_layout(8));
        assert_eq!(
            with_current_class_layout(71, |layout| layout.body_size),
            Some(8)
        );
        unregister_class_layout(71);
        assert!(with_current_class_layout(71, |_| ()).is_none());
        clear_class_layouts();
    }

    /// `f` runs while this thread's cache is borrowed, and the GC calls back
    /// into the cache from inside `f` (`forward_object` -> `object_body_size`)
    /// on every object it copies. Nested lookups must therefore return correct
    /// layouts — for the same class and for a different one — and must not
    /// panic on the `RefCell`.
    ///
    /// This is the property that makes the borrowing accessor usable inside the
    /// Cheney scan loops at all; without it every nested lookup would fall
    /// through to the registry and the change would be a net slowdown.
    #[test]
    fn with_class_layout_is_reentrant() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        register_class_layout(63, one_ref_layout(8));
        register_class_layout(64, one_ref_layout(16));
        // Warm both so the nested lookups exercise the hit path.
        assert_eq!(class_layout_for_fields(63, 1).unwrap().body_size, 8);
        assert_eq!(class_layout_for_fields(64, 1).unwrap().body_size, 16);

        let observed = with_class_layout(63, 1, |outer| {
            // Same class, owning accessor (what `object_body_size` used to do).
            let same = class_layout_for_fields(63, 1).map(|l| l.body_size);
            // Different class, borrowing accessor (what it does now) — this is
            // the referent-sizing call the scan loops make per copied object.
            let other = with_class_layout(64, 1, |inner| inner.body_size);
            // Three levels deep, to confirm nesting is not merely depth-1.
            let deep = with_class_layout(64, 1, |_| with_class_layout(63, 1, |l| l.body_size));
            (outer.body_size, same, other, deep)
        });
        assert_eq!(observed, Some((8, Some(8), Some(16), Some(Some(8)))));

        // A nested lookup for a class that is NOT cached cannot install (the
        // outer shared borrow blocks it), so it falls through to the registry.
        // It must still return the right answer.
        register_class_layout(65, one_ref_layout(32));
        let uncached = with_class_layout(63, 1, |_| with_class_layout(65, 1, |l| l.body_size));
        assert_eq!(
            uncached,
            Some(Some(32)),
            "a nested miss must fall through to the registry, not fail"
        );
        clear_class_layouts();
    }

    #[test]
    fn registry_rejects_sparse_class_id_without_poisoning_lock() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap();
        clear_class_layouts();
        let layout = Arc::new(CompactLayout {
            field_offsets: vec![0],
            is_ref: vec![true],
            field_kinds: vec![FieldStorageKind::Reference],
            ref_offsets: vec![0],
            body_size: 8,
        });

        let rejected = std::panic::catch_unwind({
            let layout = Arc::clone(&layout);
            move || register_class_layout(u32::MAX, layout)
        });
        assert!(
            rejected.is_err(),
            "corrupt high class_id must be rejected before dense allocation"
        );

        // The guard fires before taking CLASS_LAYOUTS, so a later valid
        // registration must still succeed instead of observing a poisoned lock.
        register_class_layout(0, Arc::clone(&layout));
        assert!(Arc::ptr_eq(&class_layout(0).unwrap(), &layout));
        assert!(class_layout(u32::MAX).is_none());
        clear_class_layouts();
    }

}
