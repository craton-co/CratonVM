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
use std::sync::{Arc, OnceLock, RwLock};

/// Per-class instance-field layout for the compact reference-field model.
///
/// Indexed by **absolute field index** (`declaring.first_field_index +
/// instance_idx`), the hierarchy-wide stable index used by `get_field` /
/// `set_field`. A parent's layout is an exact prefix of every child's layout
/// (declaration order, supers first), so inherited indices map to identical
/// offsets across the hierarchy.
#[derive(Debug, Clone)]
pub struct CompactLayout {
    /// Byte offset within the object body (relative to `HEADER_SIZE`) of each
    /// field index. `field_offsets.len()` == instance-field count.
    pub field_offsets: Vec<u32>,
    /// Whether each field index holds a reference (descriptor `L`/`[`, or an
    /// unknown/uncovered descriptor — treated as a reference, matching the heap
    /// default-init rule).
    pub is_ref: Vec<bool>,
    /// Byte offsets (relative to `HEADER_SIZE`) of every reference field, in
    /// ascending order. This is the GC oop-map: the only slots the collector
    /// scans/remaps for an instance of this class.
    pub ref_offsets: Vec<u32>,
    /// Total object body size in bytes (sum of per-field sizes; ref = 8,
    /// primitive = 16). Object total size = `HEADER_SIZE + body_size`.
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

    let mut v = CLASS_LAYOUTS.write().unwrap();
    if idx >= v.len() {
        v.resize(idx + 1, None);
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
                        layout.field_is_ref(index)?,
                    ));
                }
            }
        }
        let layout = class_layout(class_id)?;
        let offset = layout.field_offset(index)? as usize;
        let is_ref = layout.field_is_ref(index)?;
        let slot = cache.next;
        cache.entries[slot] = Some((class_id, generation, layout));
        cache.next = (slot + 1) % cache.entries.len();
        Some((offset, is_ref))
    })
}

/// Clear the registry (test/debug-only).
#[cfg(any(test, debug_assertions))]
pub fn clear_class_layouts() {
    CLASS_LAYOUTS.write().unwrap().clear();
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
        header.array_length as usize
    } else {
        header.num_slots as usize * SLOT_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_offsets_and_oopmap() {
        // Class with fields: ref, int, ref  (descriptors L, I, L)
        // prefix-sum: off0=0(ref,8) off1=8(int,16) off2=24(ref,8) body=32
        let layout = CompactLayout {
            field_offsets: vec![0, 8, 24],
            is_ref: vec![true, false, true],
            ref_offsets: vec![0, 24],
            body_size: 32,
        };
        assert_eq!(layout.field_offset(0), Some(0));
        assert_eq!(layout.field_offset(1), Some(8));
        assert_eq!(layout.field_offset(2), Some(24));
        assert_eq!(layout.field_offset(3), None);
        assert_eq!(layout.field_is_ref(1), Some(false));
        assert_eq!(layout.field_count(), 3);
        assert_eq!(layout.ref_offsets, vec![0, 24]);
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
        use crate::heap_types::REF_FIELD_SIZE;

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
            field_offsets: vec![0, 8, 24, 32, 48, 64, 72],
            is_ref: vec![true, false, true, false, false, true, false],
            ref_offsets: vec![0, 24, 64],
            body_size: 88,
        };

        // Cross-check the literal layout against the prefix-sum rule it encodes,
        // so a change to REF_FIELD_SIZE / SLOT_SIZE (or a mis-sized field) is
        // caught here rather than corrupting objects at runtime.
        let is_ref = [true, false, true, false, false, true, false];
        let mut expect_off = 0u32;
        let mut expect_refs = Vec::new();
        for (i, &r) in is_ref.iter().enumerate() {
            assert_eq!(
                layout.field_offset(i),
                Some(expect_off),
                "field {i} offset must follow the 8-byte-per-ref prefix sum",
            );
            assert_eq!(layout.field_is_ref(i), Some(r), "field {i} ref-ness");
            if r {
                expect_refs.push(expect_off);
                expect_off += REF_FIELD_SIZE as u32;
            } else {
                expect_off += SLOT_SIZE as u32;
            }
        }
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
        assert_eq!(layout.field_offset(3), Some(32)); // double y, two refs before
        assert_ne!(
            32,
            3 * SLOT_SIZE as u32,
            "non-compact assumption would use 48"
        );

        // No primitive field's [off, off+SLOT_SIZE) byte range may overlap any
        // reference field's [off, off+REF_FIELD_SIZE) range — that overlap is
        // exactly how a double read yields a ref's bytes.
        for (i, &r) in is_ref.iter().enumerate() {
            if r {
                continue;
            }
            let p0 = layout.field_offset(i).unwrap();
            let p1 = p0 + SLOT_SIZE as u32;
            for &ro in &layout.ref_offsets {
                let r1 = ro + REF_FIELD_SIZE as u32;
                assert!(
                    p1 <= ro || r1 <= p0,
                    "primitive field {i} at [{p0},{p1}) overlaps ref slot [{ro},{r1})",
                );
            }
        }
    }

    #[test]
    fn registry_round_trip() {
        clear_class_layouts();
        let layout = Arc::new(CompactLayout {
            field_offsets: vec![0],
            is_ref: vec![true],
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
            ref_offsets: vec![0, 8],
            body_size: 16,
        });
        register_class_layout(7, layout2);
        assert_eq!(class_layout(7).unwrap().body_size, 16);
        clear_class_layouts();
    }

    #[test]
    fn registry_rejects_sparse_class_id_without_poisoning_lock() {
        clear_class_layouts();
        let layout = Arc::new(CompactLayout {
            field_offsets: vec![0],
            is_ref: vec![true],
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
