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
    *COMPACT_ENABLED.get_or_init(|| match std::env::var("CRATONVM_COMPACT_REF_FIELDS") {
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
static CLASS_LAYOUT_VERSIONS: LazyLock<RwLock<HashMap<(u32, u32), Arc<CompactLayout>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

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
        let mut versions = CLASS_LAYOUT_VERSIONS.write().unwrap();
        versions.insert((class_id, field_count), Arc::clone(&layout));
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

/// Look up the compact layout for a class, if registered.
#[inline]
pub fn class_layout(class_id: u32) -> Option<Arc<CompactLayout>> {
    let v = CLASS_LAYOUTS.read().unwrap();
    v.get(class_id as usize).and_then(|o| o.clone())
}

/// Resolve the immutable layout version used by an object with this exact
/// hierarchy-wide field count. Retaining versions makes object size and oop
/// maps stable across append-only synthetic-class upgrades.
#[inline]
pub fn class_layout_for_fields(
    class_id: u32,
    field_count: u32,
) -> Option<Arc<CompactLayout>> {
    CLASS_LAYOUT_VERSIONS
        .read()
        .unwrap()
        .get(&(class_id, field_count))
        .cloned()
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
    struct SlotCache {
        entries: [Option<(u32, u64, Arc<CompactLayout>)>; 8],
        next: usize,
    }
    impl SlotCache {
        const fn new() -> Self {
            Self {
                entries: [None, None, None, None, None, None, None, None],
                next: 0,
            }
        }
    }
    thread_local! {
        static SLOT_CACHE: std::cell::RefCell<SlotCache> =
            const { std::cell::RefCell::new(SlotCache::new()) };
    }
    let generation = layout_generation();
    SLOT_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        for entry in &cache.entries {
            if let Some((cached_cid, cached_gen, layout)) = entry {
                if *cached_cid == class_id && *cached_gen == generation {
                    return Some((
                        layout.field_offset(index)? as usize,
                        layout.field_storage(index)?,
                    ));
                }
            }
        }
        let layout = class_layout(class_id)?;
        let offset = layout.field_offset(index)? as usize;
        let storage = layout.field_storage(index)?;
        let slot = cache.next;
        cache.entries[slot] = Some((class_id, generation, layout));
        cache.next = (slot + 1) % cache.entries.len();
        Some((offset, storage))
    })
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
    class_layout(class_id)
        .filter(|layout| layout.field_count() == field_count)
        .map(|layout| layout.body_size as usize)
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
            let raw = unsafe { (&*(ptr as *const AtomicU64)).load(ordering) };
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
            unsafe { (&*(ptr as *const AtomicU64)).store(raw, ordering) };
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
            let raw = match value { Value::Int(v) => v as u32, _ => 0 };
            unsafe { (&*(ptr as *const AtomicU32)).store(raw, ordering) };
        }
        FieldStorageKind::Float => {
            let raw = match value { Value::Float(v) => v.to_bits(), _ => 0 };
            unsafe { (&*(ptr as *const AtomicU32)).store(raw, ordering) };
        }
        FieldStorageKind::Long => {
            let raw = match value {
                Value::Long(v) => v as u64,
                Value::Int(v) => v as i64 as u64,
                _ => 0,
            };
            unsafe { (&*(ptr as *const AtomicU64)).store(raw, ordering) };
        }
        FieldStorageKind::Double => {
            let raw = match value { Value::Double(v) => v.to_bits(), _ => 0 };
            unsafe { (&*(ptr as *const AtomicU64)).store(raw, ordering) };
        }
    }
}

/// Clear the registry (test/debug-only).
#[cfg(any(test, debug_assertions))]
pub fn clear_class_layouts() {
    CLASS_LAYOUTS.write().unwrap().clear();
    CLASS_LAYOUT_VERSIONS.write().unwrap().clear();
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
        class_layout_for_fields(header.class_id.as_u32(), header.num_slots())
            .map(|layout| layout.body_size as usize)
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
