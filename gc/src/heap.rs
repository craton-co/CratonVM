// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Heap — semi-space copying GC object and array storage.
//!
//! Objects are laid out as contiguous blocks:
//! ```text
//! [ObjectHeader (HEADER_SIZE bytes)] [field0] [field1] ... [fieldN]
//! ```
//!
//! [`HEADER_SIZE`] is **16** today — the authority is
//! `cratonvm_types::HEADER_SIZE` (`types/src/heap_types.rs`), never a literal
//! written here. It is used symbolically throughout this file on purpose: the
//! header has already shrunk twice, 32 -> 24 (2026-08-06) and 24 -> 16
//! (completed 2026-08-07), and until 2026-08-07 this very paragraph still read
//! "32 today, a shrink to 24 is mapped out in `arch-2026-07-26/header-shrink.md`"
//! — i.e. it was itself an instance of the staleness that document's §6.9
//! catalogues, wrong about both the current value and the pending one. Anything
//! below that needs the number must read the constant.
//!
//! Field cell width depends on the field's type:
//!
//! * **primitive** fields occupy `SLOT_SIZE` (16 bytes) — the full tagged
//!   [`cratonvm_types::Value`] enum (4-byte discriminant, then payload);
//! * **reference** fields occupy `REF_FIELD_SIZE` (8 bytes) — a bare pointer,
//!   `0` meaning null — under the compact reference-field layout, which is the
//!   default (`CRATONVM_COMPACT_REF_FIELDS=0` opts back out to a 16-byte tagged
//!   cell). Such objects are flagged [`GC_FLAG_COMPACT`] and the collector
//!   scans them via the per-class oop-map ([`CompactLayout::ref_offsets`])
//!   instead of tag-scanning uniform cells — see [`compact_oop_scan`].
//!
//! Arrays use **compact, natural element widths**, not `SLOT_SIZE` — 1 byte for
//! `boolean`/`byte`, 2 for `char`/`short`, 4 for `int`/`float`, 8 for
//! `long`/`double`, and `REF_ELEMENT_SIZE` (8) for reference elements. See
//! [`element_byte_size`] and [`array_data_size`], which are the authority:
//! ```text
//! [ObjectHeader (HEADER_SIZE bytes)] [elem0] [elem1] ... [elemN]   // element_byte_size(elem_type) each,
//!                                                                  // data area rounded up to 8 bytes
//! ```
//!
//! The heap uses two arenas (from-space and to-space) for a semi-space
//! copying garbage collector. Allocation is linear in from-space. During
//! collection, live objects are copied to to-space, then spaces are swapped.

use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::arena::Arena;
use crate::numa;
use cratonvm_types::{ClassId, ObjectRef, Value};

// Re-export heap types from the shared types crate.
use crate::gc_flags;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, write_ref_slot};
pub use cratonvm_types::{
    array_data_size, array_data_size_checked, array_element_type_from_tag, element_byte_size,
    object_kind_from_tag, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    ARRAY_LENGTH_OFFSET, AUTOBOX_CLASS_ID, GC_FLAG_COMPACT, GC_FLAG_MARKED, GC_FLAG_OLD_GEN,
    HEADER_SIZE, REF_ELEMENT_SIZE, REF_FIELD_SIZE, SLOT_SIZE,
};
use cratonvm_types::{class_layout_for_fields, is_compact_object, CompactLayout};
use std::sync::Arc;

/// For a compact object (one allocated under the compact reference-field
/// layout, [`GC_FLAG_COMPACT`]), return its class oop-map together with the
/// object's own allocated body size in bytes. The GC scans/remaps **only** the
/// reference slots listed in [`CompactLayout::ref_offsets`], each an 8-byte
/// pointer at `HEADER_SIZE + offset`.
///
/// Returns `None` for a legacy object — the caller must fall back to
/// tag-scanning its uniform 16-byte cells.
///
/// The `body_size` is the object's *own* body (from its header), which may be
/// smaller than the class's current `body_size` if the (synthetic-stub) class
/// grew after this object was allocated. Callers iterate `ref_offsets` in
/// ascending order and stop at the first offset that would read past
/// `body_size`, so a grown class never makes the GC read out of bounds.
/// H2-CID0 (2026-08-05) — compact objects whose class oop map the layout
/// registry could not produce, so the GC fell back to scanning their packed
/// body as legacy 16-byte `Value` cells and saw none of their reference slots.
///
/// A marking FAIL-OPEN: non-zero means some object's outgoing edges were
/// invisible to the collector, which is licence to reclaim its referents while
/// it is live. Expected to be ZERO. Reported by `VmHeap::print_gc_summary`.
pub static COMPACT_OOP_MAP_MISSING: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[inline]
pub(crate) fn compact_oop_scan(header: &ObjectHeader) -> Option<(Arc<CompactLayout>, usize)> {
    if !is_compact_object(header) {
        return None;
    }
    let cid = header.class_id.as_u32();
    // Per-thread single-entry cache. The GC scans long runs of same-class
    // objects (e.g. a tree of one node type), so this avoids re-taking the
    // registry `RwLock` on every scanned object — a hit is just a generation
    // load + an `Arc` refcount bump. Validated against `layout_generation()`
    // so a redefine (which bumps the generation) cannot serve a stale layout.
    thread_local! {
        static OOP_CACHE: std::cell::RefCell<Option<(u32, u32, u64, Arc<CompactLayout>)>> =
            const { std::cell::RefCell::new(None) };
    }
    let gen = cratonvm_types::layout_generation();
    let layout = OOP_CACHE.with(|c| {
        {
            let cache = c.borrow();
            if let Some((cached_cid, cached_fields, cached_gen, arc)) = &*cache {
                if *cached_cid == cid && *cached_fields == header.num_slots() && *cached_gen == gen
                {
                    return Some(arc.clone());
                }
            }
        }
        let arc = class_layout_for_fields(cid, header.num_slots())?;
        *c.borrow_mut() = Some((cid, header.num_slots(), gen, arc.clone()));
        Some(arc)
    });
    let Some(layout) = layout else {
        // See `COMPACT_OOP_MAP_MISSING`. Returning `None` here is not a
        // "this is a legacy object" answer — the header already said it is
        // not — it is the GC agreeing to scan a packed body as `Value` cells
        // and miss every reference in it.
        let n = COMPACT_OOP_MAP_MISSING.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                class_id = cid,
                num_slots = header.num_slots(),
                layout_generation = gen,
                "object carries GC_FLAG_COMPACT but the layout registry has no oop map for \
                 (class_id, num_slots) — the GC is about to scan its packed body as legacy \
                 16-byte `Value` cells and will miss every reference slot it holds.",
            );
        }
        return None;
    };
    let body_size = layout.body_size as usize;
    Some((layout, body_size))
}

// ---------------------------------------------------------------------------
// Heap — semi-space copying GC heap
// ---------------------------------------------------------------------------

/// Default semi-space size: 32 MB per semi-space (64 MB total).
const DEFAULT_SEMI_SPACE_SIZE: usize = 32 * 1024 * 1024;

/// Maximum array length — prevents excessive allocation from malformed bytecode.
/// Matches HotSpot's practical limit.
const MAX_ARRAY_LENGTH: usize = i32::MAX as usize; // 2^31 - 1

/// GC threshold: trigger collection when from-space usage exceeds 75% capacity.
const GC_THRESHOLD_PERCENT: usize = 75;

/// Semi-space heap for Java objects and arrays.
///
/// Uses two arenas (from-space and to-space) for a copying garbage collector.
/// Allocation happens linearly in from-space. During GC, live objects are
/// copied to to-space, then the spaces are swapped.
pub struct Heap {
    /// Compact-layout domain of the VM that owns this heap.
    ///
    /// `class_id` is a per-`ClassStore` index, so it does not identify a class
    /// in the process-global layout registry. Allocating against another
    /// domain's entry gives the object a foreign shape and corrupts every field
    /// access it will ever see. Defaults to the FIRST domain, so a heap nobody
    /// tells behaves exactly as it did before domains existed — and a heap told
    /// the wrong value merely loses compact layouts (tagged slots carry their
    /// own type), it does not corrupt.
    layout_domain: std::sync::atomic::AtomicU32,

    from_space: Mutex<Arena>,
    to_space: Mutex<Arena>,
    next_hash_code: AtomicI32,
    gc_threshold: usize,

    // ---- NUMA hint (stub: see TODO below) ---------------------------------
    //
    // Snapshot of the host NUMA topology's node count, captured at
    // `Heap::with_capacity`. Stored so [`Heap::preferred_numa_node`] can
    // bound `numa::current_thread_node()` against it without re-reading the
    // `OnceLock` on every alloc.
    //
    // The current allocator still funnels every request through the single
    // `from_space` semi-space — this field is a "design intent" marker, not
    // an actual per-node partition.
    //
    // TODO(NUMA, multi-arena): replace `from_space`/`to_space` with
    //   `arenas_from: Vec<Mutex<Arena>>` and `arenas_to: Vec<Mutex<Arena>>`,
    //   each indexed by NUMA node. The fast-path alloc would then dispatch
    //   to `arenas_from[numa::current_thread_node()]`, falling back to
    //   round-robin on local OOM. The GC's `collect_garbage` would walk
    //   every (from, to) pair and swap them in lockstep. This is deferred
    //   because the existing GC entry points (`crate::gc::collect`,
    //   `crate::gc::collect_with_finalizers`) take a single `&mut Arena`
    //   pair, so a full multi-arena rewrite touches the collector crate
    //   too — out of scope for this single-file change. See `gc/src/numa.rs`
    //   for `NumaTopology` / `current_thread_node`.
    num_numa_nodes: usize,
    /// Last observed NUMA node hint. Updated on the alloc fast-path purely
    /// for observability / debugging; allocation itself still hits the
    /// single shared `from_space`.
    numa_node_hint: AtomicUsize,

    // ---- GPU-offload coordination (Part F) --------------------------------
    //
    // These fields exist ONLY when the `gpu-offload` Cargo feature is on. They
    // hold the live-token counter and the pinned-ObjectRef set described in
    // [crate::safepoint]. With the feature off the struct has the same shape
    // as before the GPU work.
    /// Number of [`crate::safepoint::SafepointToken`]s currently alive on
    /// this heap. While non-zero, `collect_garbage` and
    /// `collect_garbage_with_finalizers` spin-yield instead of running a
    /// collection. Tokens increment on construction and decrement on
    /// drop; the counter is the sole gate.
    #[cfg(feature = "gpu-offload")]
    pub(crate) gpu_critical_count: std::sync::atomic::AtomicU32,

    /// Object refs currently being marshalled to or from the GPU. These
    /// are walked as additional GC roots so that even between kernel
    /// launches (when no token is alive) the underlying JVM arrays stay
    /// reachable for any pending re-use.
    ///
    /// Uses `parking_lot::Mutex` to match every other mutex in this struct.
    /// `HashSet` so a pin/unpin pair is idempotent and order-independent.
    /// Test-only counter for tests that observe "did the GC actually skip
    /// because of the token?" without instrumenting the call path.
    #[cfg(feature = "gpu-offload")]
    pub(crate) gpu_pinned_refs: parking_lot::Mutex<std::collections::HashSet<ObjectRef>>,

    /// Number of times `collect_garbage` (or its finalizer variant) was
    /// asked to run but found `gpu_critical_count != 0` and bailed out.
    /// Exposed via [`Heap::gpu_blocked_gc_count`] so tests can assert
    /// "the GC saw the token and stepped aside" without timing tricks.
    #[cfg(feature = "gpu-offload")]
    pub(crate) gpu_blocked_gc_count: std::sync::atomic::AtomicU64,
}

// SAFETY: `Heap` contains only `Mutex<Arena>` (which is Send+Sync), an
// `AtomicI32` (Send+Sync), and a `usize` (Send+Sync). The raw pointers
// live *inside* the `Arena` behind the `Mutex`, so they are never directly
// exposed as struct fields. All allocation goes through `self.from_space.lock()`,
// which serializes mutations. After allocation, `ObjectRef` handles are plain
// `*mut u8` pointers into arena-owned memory — reads and writes to disjoint
// objects are safe from different threads because:
//   1. Object fields are at fixed offsets from the allocation base.
//   2. The GC stops the world before relocating objects.
//   3. Volatile field access uses SeqCst fences.
//
// HIGH-soundness audit: invariant (2) is now backed at the *type* level by
// [`crate::collector::StopTheWorldToken`]: every mutating-by-`&self` entry
// point on this struct (`collect_garbage`, `collect_garbage_with_finalizers`)
// requires `&StopTheWorldToken` as its first argument, so cross-thread
// callers cannot invoke a moving collection without first parking every
// other mutator. The blanket `unsafe impl` is retained because the heap's
// internal raw `*mut u8` arena pointers are still `!Send` / `!Sync` on
// their own — the impl asserts that the combination of (token-gated
// mutation) + (Mutex-serialised allocation) + (SeqCst volatile fences)
// makes shared `&Heap` usage sound across threads.
unsafe impl Send for Heap {}
unsafe impl Sync for Heap {}

impl Heap {
    /// Bind this heap to its VM's compact-layout domain. Call once, as early as
    /// possible in VM construction: allocations made before it land on tagged
    /// slots, which is safe but larger.
    pub fn set_layout_domain(&self, domain: u32) {
        self.layout_domain
            .store(domain, std::sync::atomic::Ordering::Release);
    }

    /// This heap's compact-layout domain. See [`Self::set_layout_domain`].
    pub fn layout_domain(&self) -> u32 {
        self.layout_domain
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Create a new heap with default capacity (4 MB per semi-space).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_SEMI_SPACE_SIZE * 2)
    }

    /// Create a new heap with the given total capacity.
    /// Each semi-space gets half the capacity.
    pub fn with_capacity(total_bytes: usize) -> Self {
        let half = total_bytes.max(1024) / 2;
        let threshold = half * GC_THRESHOLD_PERCENT / 100;
        // Snapshot the host NUMA topology once. On single-node hosts
        // (the common case for non-Linux platforms) `num_nodes == 1`, so
        // the alloc fast-path collapses to its original single-arena
        // behaviour — no regression. See the field-level TODO on `Heap`
        // for the planned multi-arena partitioning.
        let topo = numa::global_topology();
        let num_numa_nodes = topo.num_nodes.max(1);
        Self {
            layout_domain: std::sync::atomic::AtomicU32::new(
                cratonvm_types::FIRST_LAYOUT_DOMAIN,
            ),
            from_space: Mutex::new(Arena::new(half)),
            to_space: Mutex::new(Arena::new(half)),
            next_hash_code: AtomicI32::new(1),
            gc_threshold: threshold,

            num_numa_nodes,
            numa_node_hint: AtomicUsize::new(0),

            #[cfg(feature = "gpu-offload")]
            gpu_critical_count: std::sync::atomic::AtomicU32::new(0),
            #[cfg(feature = "gpu-offload")]
            gpu_pinned_refs: parking_lot::Mutex::new(std::collections::HashSet::new()),
            #[cfg(feature = "gpu-offload")]
            gpu_blocked_gc_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Return the NUMA node this thread should *prefer* for allocation,
    /// clamped to the topology size captured at heap construction.
    ///
    /// On single-node hosts this is always `0`. On multi-node hosts the
    /// value is derived from [`numa::current_thread_node()`].
    ///
    /// Currently informational: the alloc fast-path records this on every
    /// call (so observers can confirm per-thread node affinity) but the
    /// underlying arena is still shared. See the field-level TODO on
    /// [`Heap`] for the full multi-arena plan.
    #[inline]
    pub fn preferred_numa_node(&self) -> usize {
        if self.num_numa_nodes <= 1 {
            return 0;
        }
        let raw = numa::NumaTopology::current_thread_node();
        if raw < self.num_numa_nodes {
            raw
        } else {
            // current_thread_node returned a node beyond what we
            // snapshotted (topology changed mid-flight, or platform
            // returned a stale CPU id). Fold back to node 0 rather
            // than indexing OOB.
            0
        }
    }

    /// Last NUMA node hint observed by the alloc fast-path. Mainly useful
    /// for tests / observability — see [`Heap::preferred_numa_node`] for
    /// the live value.
    pub fn last_numa_node_hint(&self) -> usize {
        self.numa_node_hint.load(Ordering::Relaxed)
    }

    /// Number of NUMA nodes the heap is aware of (>= 1).
    pub fn num_numa_nodes(&self) -> usize {
        self.num_numa_nodes
    }

    /// Allocation fast-path hook: refresh the cached NUMA hint from the
    /// current OS thread. Cheap on single-node hosts (early return).
    ///
    /// TODO(NUMA, multi-arena): once `from_space` is replaced with a
    /// per-node `Vec<Mutex<Arena>>`, this hook should return the chosen
    /// arena index and the caller should lock that arena instead of the
    /// shared `from_space`. Fallback policy on local OOM: round-robin
    /// over the remaining nodes before giving up (so the heap behaves
    /// like a unified pool only when every node is exhausted).
    #[inline]
    fn refresh_numa_hint(&self) -> usize {
        if self.num_numa_nodes <= 1 {
            // Single-node host: avoid the syscall / sysfs read entirely.
            return 0;
        }
        let node = self.preferred_numa_node();
        // Relaxed: this is purely observational; allocations don't depend
        // on its value yet, and racing writes between threads are fine.
        self.numa_node_hint.store(node, Ordering::Relaxed);
        node
    }

    /// Allocate a new Java object with `num_fields` field slots, all zeroed.
    ///
    /// # Panics
    /// Panics if `num_fields * SLOT_SIZE` overflows.
    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        let compact_body = cratonvm_types::compact_object_body_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let fields_size = compact_body.unwrap_or_else(|| {
            num_fields
                .checked_mul(SLOT_SIZE)
                .expect("object field size overflow")
        });
        let total_size = HEADER_SIZE
            .checked_add(fields_size)
            .expect("object total size overflow");
        let ptr = self.alloc_zeroed(total_size);

        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference, // hash installed lazily in the mark word on first request
            0,
            u32::try_from(num_fields).expect("field count exceeds u32::MAX"),
        );
        if let Some(body) = compact_body {
            header.set_compact_shape(num_fields as u32, body);
        }

        // SAFETY: `ptr` was just returned by `alloc_zeroed`, which guarantees it
        // is valid, non-null, properly aligned (8-byte), and has at least
        // `total_size` bytes available. Writing the header is within bounds
        // (HEADER_SIZE <= total_size). `ObjectRef::from_raw` wraps the raw
        // pointer in a typed handle; the pointed-to memory remains valid for
        // the lifetime of the current semi-space (until the next GC swap).
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    /// Allocate a new Java object with `num_fields` field slots and
    /// initialize each slot to the JVM-spec-mandated default value based on
    /// its descriptor byte.
    ///
    /// `descriptor_bytes[i]` must be the first byte of the JVM field
    /// descriptor for field index `i` (e.g. `b'I'` for int, `b'J'` for long,
    /// `b'L'`/`b'['` for reference types). Fields not covered by
    /// `descriptor_bytes` (when `descriptor_bytes.len() < num_fields`) are
    /// initialized to an explicit `Value::Object(None)` — the correct
    /// default for reference slots.
    ///
    /// This fixes a subtle correctness bug. `read_slot` does a raw
    /// `ptr::read::<Value>`; after `Value::Object` became
    /// `Option<ObjectRef>` with a `NonNull` niche, the all-zero bit pattern
    /// left by `alloc_zeroed` decodes as `Value::Int(0)` (the
    /// discriminant-0 variant of [`Value`]), NOT `Value::Object(None)`.
    /// So neither a primitive nor a reference default can be obtained "for
    /// free" from zeroed memory any more — both must be written explicitly.
    /// For primitive fields the JVM spec §2.3 mandates a typed zero —
    /// `Value::Int(0)` for I/S/B/C/Z, `Value::Long(0)` for J,
    /// `Value::Float(0.0)` for F, `Value::Double(0.0)` for D. For reference
    /// fields the default is `null` (`Value::Object(None)`). Without this
    /// explicit default-init an unwritten reference field reads as
    /// `Int(0)`, violating the documented slot contract, and an unwritten
    /// `int` field would (under the OLD zeroed==Object(None) decode) read as
    /// `Object(None)`, breaking `Unsafe.compareAndSetInt` comparisons
    /// against `Int(0)` (e.g. `ConcurrentHashMap.initTable`) and livelocking
    /// the caller in a CAS retry loop.
    ///
    /// Unknown/malformed descriptor bytes initialize to the reference
    /// default (`Object(None)`), matching the legacy intent so this cannot
    /// regress non-primitive paths.
    ///
    /// # Panics
    /// Panics if `num_fields * SLOT_SIZE` overflows.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        let obj = self.alloc_object(class_id, num_fields);
        // Initialize EVERY slot with an explicitly-tagged default `Value`.
        //
        // R-niche fix: `read_slot` does a raw `ptr::read::<Value>`, and after
        // `Value::Object` became `Option<ObjectRef>` with a `NonNull` niche,
        // the all-zero bit pattern that `alloc_zeroed` leaves now decodes as
        // `Value::Int(0)` (the discriminant-0 variant), NOT `Value::Object(None)`.
        // A zeroed slot and a slot written with `Value::Int(0)` are therefore
        // bit-indistinguishable, so the decode rule cannot be recovered in
        // `read_slot` alone. We instead make the reference/uninitialized default
        // EXPLICIT here: every primitive slot gets its spec-mandated typed zero,
        // and every reference-typed (`L`/`[`), unknown-descriptor, or
        // descriptor-uncovered slot gets an explicit `Value::Object(None)`
        // (discriminant 4) so a later `get_field` reads back the documented
        // `null` rather than a bogus `Int(0)`.
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            // SAFETY: `i < num_fields == header.num_slots()`, so `slot_ptr`
            // lands within the freshly-allocated object's field region.
            unsafe {
                let ptr = slot_ptr(obj, i);
                write_slot(ptr, default);
            }
        }
        obj
    }

    /// Like `alloc_object`, but returns `None` instead of panicking on overflow.
    pub fn alloc_object_checked(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let compact_body = cratonvm_types::compact_object_body_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let fields_size = compact_body.or_else(|| num_fields.checked_mul(SLOT_SIZE))?;
        let total_size = HEADER_SIZE.checked_add(fields_size)?;
        // See `alloc_zeroed` for the NUMA hint rationale.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(total_size, 8)?;
        // SAFETY: same as `alloc_object` — ptr is valid, aligned, with sufficient capacity.
        unsafe {
            std::ptr::write_bytes(ptr, 0, total_size);
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference, // hash installed lazily in the mark word on first request
                0,
                u32::try_from(num_fields).ok()?,
            );
            if let Some(body) = compact_body {
                header.set_compact_shape(num_fields as u32, body);
            }
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Allocate a new Java array with the given element type and length, all zeroed.
    ///
    /// Arrays use compact element sizes: 1 byte for boolean/byte, 2 for char/short,
    /// 4 for int/float, 8 for long/double/reference.
    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        assert!(
            length <= MAX_ARRAY_LENGTH,
            "array length {} exceeds maximum {}",
            length,
            MAX_ARRAY_LENGTH
        );
        let data_size =
            array_data_size(length, element_type).expect("array data size overflow in alloc_array");
        // M6 (round-12 gc): make the `+ HEADER_SIZE` add checked too, matching
        // `alloc_array_checked` (which uses `HEADER_SIZE.checked_add(data_size)?`).
        let total_size = HEADER_SIZE
            .checked_add(data_size)
            .expect("array total size overflow in alloc_array");
        let ptr = self.alloc_zeroed(total_size);

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type, // hash installed lazily in the mark word on first request
            u32::try_from(length).expect("array length exceeds u32::MAX"),
            u32::try_from(length).expect("array length exceeds u32::MAX"),
        );

        // SAFETY: `ptr` was returned by `alloc_zeroed` — valid, non-null, 8-byte
        // aligned, and has at least `total_size` bytes. The header write is in
        // bounds. The data area (elements) is already zero-initialized by
        // `alloc_zeroed`, which is the correct default for all primitive types
        // (0) and reference types (null pointer = 0).
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    /// Like `alloc_array`, but returns `None` instead of panicking on overflow
    /// or out-of-memory.
    pub fn alloc_array_checked(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if length > MAX_ARRAY_LENGTH {
            return None;
        }
        let data_size = array_data_size_checked(length, element_type)?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        // See `alloc_zeroed` for the NUMA hint rationale.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(total_size, 8)?;
        // SAFETY: same as `alloc_array` — ptr is valid, aligned, with sufficient capacity.
        unsafe {
            std::ptr::write_bytes(ptr, 0, total_size);
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type, // hash installed lazily in the mark word on first request
                u32::try_from(length).ok()?,
                u32::try_from(length).ok()?,
            );
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    // ----- Header access ---------------------------------------------------

    /// Read the object header from a heap reference.
    ///
    /// # Safety
    /// `obj_ref` must point to a valid heap allocation.
    pub fn get_header(&self, obj_ref: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj_ref` points to a live heap allocation whose first
        // HEADER_SIZE bytes are a valid `ObjectHeader` written during allocation.
        // The reference is valid for the lifetime of the current semi-space.
        unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) }
    }

    /// Get the class id of a heap object.
    pub fn class_id_of(&self, obj_ref: ObjectRef) -> ClassId {
        self.get_header(obj_ref).class_id
    }

    /// Get the kind (Object or Array) of a heap allocation.
    pub fn kind_of(&self, obj_ref: ObjectRef) -> ObjectKind {
        self.get_header(obj_ref).kind()
    }

    /// Get the element type of an array object.
    pub fn element_type_of(&self, obj_ref: ObjectRef) -> ArrayElementType {
        self.get_header(obj_ref).element_type()
    }

    /// Get the identity hash code of a heap object.
    pub fn identity_hash_code(&self, obj_ref: ObjectRef) -> i32 {
        let header = self.get_header(obj_ref);
        match header.mark_word_identity_hash(|| match self.next_hash() {
            0 => i32::MAX,
            h => h,
        }) {
            Ok(hash) => hash,
            Err(()) => {
                // Not NEUTRAL: the object inflated, and the hash went with it
                // into its Monitor. Never mint here -- see
                // `collector::displaced_identity_hash`.
                crate::collector::displaced_identity_hash(
                    header.mark_word.load(Ordering::Relaxed),
                )
            }
        }
    }

    // ----- Field access (for Objects) --------------------------------------

    /// Get the value of a field at the given index.
    ///
    /// # Panics
    /// Panics if `index >= num_slots`.
    pub fn get_field(&self, obj_ref: ObjectRef, index: usize) -> Value {
        assert!(
            index < self.get_header(obj_ref).num_slots() as usize,
            "field index {} out of bounds (num_slots={})",
            index,
            self.get_header(obj_ref).num_slots()
        );
        if let Some((offset, storage)) =
            cratonvm_types::compact_object_field_storage(self.get_header(obj_ref), index)
        {
            let ptr = unsafe { obj_ref.as_ptr().add(HEADER_SIZE + offset) };
            let v = unsafe {
                cratonvm_types::read_compact_field(
                    ptr,
                    storage,
                    std::sync::atomic::Ordering::Relaxed,
                )
            };
            if !storage.is_reference() {
                return v;
            }
            // Un-box the wrapper `set_field` installs for a non-reference value
            // stored into a declared-REFERENCE slot — the field half of what
            // `get_array_element_unboxing` already does for elements. This
            // accessor used to hand such a value to `write_compact_field`,
            // whose `FieldStorageKind::Reference` arm maps every non-`Object`
            // value to raw 0, so the write was silently dropped to null
            // (W7-84-primitive-in-reference-store.md).
            return crate::autobox::unbox_reference_slot(
                v,
                |r| {
                    if !self.is_valid_heap_object(r) {
                        return None;
                    }
                    // SAFETY: `is_valid_heap_object` confirmed `r` points to an
                    // 8-byte-aligned address inside one of this heap's arenas,
                    // so reading its `ObjectHeader` is valid memory.
                    Some(unsafe { (*(r.as_ptr() as *const ObjectHeader)).class_id })
                },
                |r| self.get_field(r, 0),
            );
        }
        // HIB-DCAST-LATEPHASE.1 (mutator side), the fourth accessor family.
        // `compact_object_field_storage` answers `None` for TWO reasons and
        // only the first licenses the fall-through below: (1) "this is a
        // legacy object" — its contract, and the uniform 16-byte `Value` cell
        // is the right read; (2) "this IS a compact object (`GC_FLAG_COMPACT`,
        // set at allocation by `alloc_object`, which sized the body with
        // `compact_object_body_size`) whose `(class_id, num_slots)` no longer
        // resolves to a registered layout" — a redefinition that changed the
        // field count, or a foreign `layout_domain`.
        //
        // In case (2) `num_slots()` is the FIELD COUNT, not a count of 16-byte
        // cells, so the `index < num_slots` assert above does NOT bound
        // `HEADER_SIZE + index * SLOT_SIZE` and an entirely in-range index
        // still reads past the allocation.
        //
        // `g1::get_field`, `gen_heap::get_field` and `ZgcRealHeap::get_field`
        // all carry this guard; this accessor was the one that did not. Degrade
        // exactly as they do (benign null, loud `cratonvm::gc::guard` record,
        // no panic — a racing redefinition must not abort the JVM).
        if cratonvm_types::is_compact_object(self.get_header(obj_ref)) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                class_id = ?self.get_header(obj_ref).class_id,
                "heap::get_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — returning null rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return Value::Object(None);
        }
        // SAFETY: `obj_ref` is a live heap object. The assert above
        // confirms `index < num_slots`, and the guard above confirms the
        // object is NOT compact, so its body really is `num_slots` uniform
        // 16-byte cells. `slot_ptr` computes
        // `obj_ref + HEADER_SIZE + index * SLOT_SIZE`, which is within the
        // allocated block. `read_slot` reads a `Value` from that pointer.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            read_slot(ptr)
        }
    }

    /// Set the value of a field at the given index.
    ///
    /// # Panics
    /// Panics if `index >= num_slots`.
    pub fn set_field(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        assert!(
            index < self.get_header(obj_ref).num_slots() as usize,
            "field index {} out of bounds (num_slots={})",
            index,
            self.get_header(obj_ref).num_slots()
        );
        if let Some((offset, storage)) =
            cratonvm_types::compact_object_field_storage(self.get_header(obj_ref), index)
        {
            // A non-reference value into a declared-REFERENCE slot: box it,
            // rather than let `write_compact_field`'s `Reference` arm map it to
            // raw 0 and drop the write to null. This is the field half of what
            // `set_array_element` below has always done for elements
            // (W7-84-primitive-in-reference-store.md).
            let value = if storage.is_reference() {
                let class_id = self.get_header(obj_ref).class_id;
                crate::autobox::box_for_reference_slot(value, class_id, index, |v| {
                    let wrapper = self.alloc_object(AUTOBOX_CLASS_ID, 1);
                    self.set_field(wrapper, 0, v);
                    wrapper
                })
            } else {
                value
            };
            // Recomputed AFTER the boxing closure: it may have allocated, and
            // this is a semi-space copying heap.
            let ptr = unsafe { obj_ref.as_ptr().add(HEADER_SIZE + offset) };
            unsafe {
                cratonvm_types::write_compact_field(
                    ptr,
                    storage,
                    value,
                    std::sync::atomic::Ordering::Relaxed,
                )
            };
            return;
        }
        // HIB-DCAST-LATEPHASE.1, write half — see the long note on the matching
        // guard in `get_field`. This half is the more damaging of the two: the
        // legacy stride does not merely read past a compact-sized body, it
        // *writes* a 16-byte `Value` cell over whatever follows the object.
        if cratonvm_types::is_compact_object(self.get_header(obj_ref)) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                class_id = ?self.get_header(obj_ref).class_id,
                value = ?value,
                "heap::set_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — dropping the write rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return;
        }
        // SAFETY: same invariant as `get_field` — index is within bounds, the
        // object is not compact, and the slot pointer is within the allocated
        // object block.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            write_slot(ptr, value);
        }
    }

    // ----- Volatile field access (for ACC_VOLATILE fields) -----------------

    /// Get the value of a volatile field at the given index.
    ///
    /// JLS §17.7 requires volatile long/double (and any volatile-declared
    /// field) reads to be atomic. The on-heap `Value` slot is 16 bytes,
    /// wider than any stable Rust atomic on x86-64, so SeqCst fences alone
    /// give the JMM ordering edge but not 16-byte slot atomicity — a
    /// concurrent writer mid-store would expose a torn (tag, payload)
    /// pair to this read.
    ///
    /// We acquire the per-slot stripe lock from
    /// [`crate::collector::volatile_stripe_lock`] so paired
    /// `set_field_volatile` writers serialize against this read; the
    /// 16-byte `Value` therefore appears either fully old or fully new.
    pub fn get_field_volatile(&self, obj_ref: ObjectRef, index: usize) -> Value {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let val = self.get_field(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        val
    }

    /// Set the value of a volatile field at the given index.
    ///
    /// Acquires the per-slot stripe lock so concurrent volatile readers
    /// observe a fully-old or fully-new 16-byte `Value`. SeqCst fences
    /// provide the JMM happens-before edge. See [`Self::get_field_volatile`]
    /// for the full rationale.
    pub fn set_field_volatile(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.set_field(obj_ref, index, value);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Get the value of a field at the given index, **normalized to the
    /// declared field type** from its descriptor byte.
    ///
    /// This is the T10.9.E defense-in-depth layer: even if an upstream
    /// putfield wrote a drifted `Value` variant (e.g. a `Double` whose
    /// bit pattern happens to match a long, or an `Object(None)` in a
    /// long slot that slipped past the `alloc_object_with_descriptors`
    /// zero-init), reading through this API returns a `Value` whose
    /// variant matches the class-file descriptor for the field.
    ///
    /// Descriptor byte semantics (matches CompactValue::decode_by_descriptor):
    /// - `b'J'` → `Value::Long` (bit reinterpret if needed)
    /// - `b'D'` → `Value::Double`
    /// - `b'F'` → `Value::Float`
    /// - `b'I' | b'B' | b'C' | b'S' | b'Z'` → `Value::Int`
    /// - `b'L' | b'['` → `Value::Object(Some/None)` (unchanged from raw read)
    /// - other → raw `get_field` return (legacy behavior)
    ///
    /// Out-of-bounds behavior matches `get_field` (panics on bad index).
    pub fn get_field_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj_ref, index);
        coerce_field_value_for_slot(
            raw,
            desc_byte,
            FieldCoercionSite::read(Some(self.class_id_of(obj_ref)), index),
        )
    }

    /// Volatile variant of [`get_field_as`].
    pub fn get_field_volatile_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field_volatile(obj_ref, index);
        coerce_field_value_for_slot(
            raw,
            desc_byte,
            FieldCoercionSite::read(Some(self.class_id_of(obj_ref)), index),
        )
    }

    /// Set a field value, coercing to the declared type from the descriptor.
    ///
    /// Symmetric to [`get_field_as`] — if a `Value::Double` lands where the
    /// class file declares `J`, it is reinterpreted as `Value::Long` before
    /// the slot write so downstream readers always see a consistent tag.
    ///
    /// G30: the coercion is the provenance-carrying
    /// [`coerce_field_value_for_slot`], so a value-destroying store made
    /// through THIS heap names its class and slot in the warning. The three
    /// collectors on the live `VmHeap` dispatch path do not yet; that is
    /// NOMINATION 1 and it is a one-line change per call site.
    pub fn set_field_as(&self, obj_ref: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = coerce_field_value_for_slot(
            value,
            desc_byte,
            FieldCoercionSite::store(Some(self.class_id_of(obj_ref)), index),
        );
        self.set_field(obj_ref, index, coerced);
    }

    /// Volatile variant of [`set_field_as`].
    pub fn set_field_volatile_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = coerce_field_value_for_slot(
            value,
            desc_byte,
            FieldCoercionSite::store(Some(self.class_id_of(obj_ref)), index),
        );
        self.set_field_volatile(obj_ref, index, coerced);
    }

    // ----- Array access ----------------------------------------------------

    /// Get the length of an array.
    ///
    /// # Panics
    /// Panics if the object is not an array.
    pub fn array_length(&self, obj_ref: ObjectRef) -> usize {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        header.array_length() as usize
    }

    /// Get an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn get_array_element(&self, obj_ref: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        if index >= header.array_length() as usize {
            return Err(index as i32);
        }
        // SAFETY: bounds check passed above (`index < array_length`).
        // `obj_ref + HEADER_SIZE` is the start of the data area.
        // `read_prim_element` reads `element_byte_size(et)` bytes at
        // `base + index * element_byte_size(et)`, which is within the
        // allocated data area of size `array_data_size(length, et)`.
        unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            Ok(read_prim_element(base, index, header.element_type()))
        }
    }

    /// Like `get_array_element`, but auto-unboxes values stored by native
    /// collections. When a non-Object value (Int, Long, etc.) was auto-boxed
    /// into a wrapper object during `set_array_element`, this method
    /// transparently returns the original primitive value.
    ///
    /// Use this from NativeContext (for collection code). The interpreter's
    /// bytecode aaload should use plain `get_array_element` — Java-level
    /// Object[] arrays never contain auto-boxed wrappers.
    pub fn get_array_element_unboxing(
        &self,
        obj_ref: ObjectRef,
        index: usize,
    ) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        if index >= header.array_length() as usize {
            return Err(index as i32);
        }
        // SAFETY: bounds check passed above. Same invariant as `get_array_element`.
        let value = unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            read_prim_element(base, index, header.element_type())
        };
        if header.element_type() == ArrayElementType::Reference {
            if let Value::Object(Some(obj)) = value {
                // The stored word is treated as an `ObjectRef`, but a stale or
                // garbage non-zero element could point anywhere. Validate it
                // against this heap's semi-space arenas before dereferencing
                // it as an `ObjectHeader`); otherwise a wild read can crash or
                // mis-classify garbage. If the pointer does not look like a
                // live heap object, skip the unboxing and return the value.
                if self.is_valid_heap_object(obj) {
                    // SAFETY: `is_valid_heap_object` confirmed `obj` points to
                    // an 8-byte-aligned address inside one of this heap's
                    // arenas, so reading its `ObjectHeader` is valid memory.
                    let obj_header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
                    if obj_header.class_id == AUTOBOX_CLASS_ID {
                        return Ok(self.get_field(obj, 0));
                    }
                }
            }
        }
        Ok(value)
    }

    /// Conservative validity check for a heap object pointer.
    ///
    /// Returns `true` only if `obj` is 8-byte aligned and falls within one of
    /// this heap's semi-space arenas (`from_space` or `to_space`). Mirrors the
    /// `GenerationalHeap::is_object_address` check used by `gen_heap.rs`.
    ///
    /// This is a structural guard: it lets callers reject a stale or garbage
    /// stored pointer before dereferencing it as an `ObjectHeader`.
    ///
    /// LOCK-ORDER: this method touches both `from_space` and `to_space` and
    /// must follow the global heap convention — `from_space` is acquired
    /// before `to_space` everywhere in this crate (see `collect_garbage`,
    /// `lock_spaces`, `swap_spaces`). To stay safe against a concurrent
    /// collector that has already taken one or both locks, both acquisitions
    /// use `try_lock`: on contention the check returns `false` ("not provably
    /// valid"), which is sound because callers treat a `false` answer as
    /// "skip the unboxing fast path and just return the raw value". This
    /// avoids the latent deadlock that would otherwise occur if a mutator
    /// invoked `get_array_element_unboxing` against a stale autobox wrapper
    /// while the collector held `from_space.lock()`.
    fn is_valid_heap_object(&self, obj: ObjectRef) -> bool {
        let addr = obj.as_ptr() as usize;
        // Object headers are always 8-byte aligned; a real object pointer
        // never has its low 3 bits set.
        if addr == 0 || addr & 0x7 != 0 {
            return false;
        }
        let raw = obj.as_ptr() as *const u8;
        // Region check: must land inside one of the two semi-spaces. Use
        // `try_lock` (not `lock`) so a mutator that races a collector that
        // already holds either arena mutex degrades to "not provably valid"
        // rather than deadlocking.
        if let Some(from) = self.from_space.try_lock() {
            if from.contains(raw) {
                return true;
            }
        } else {
            return false;
        }
        if let Some(to) = self.to_space.try_lock() {
            to.contains(raw)
        } else {
            false
        }
    }

    /// Set an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn set_array_element(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
    ) -> Result<(), i32> {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        if index >= header.array_length() as usize {
            return Err(index as i32);
        }
        // SAFETY: bounds check passed above. Same invariant as `get_array_element`.
        // For reference arrays, non-Object values are auto-boxed into wrapper
        // objects allocated on this heap before being stored.
        unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            // Compact ref arrays only store 8-byte pointers. If a non-Object
            // value is written (e.g. Value::Int from a native collection),
            // auto-box it into a 1-field wrapper object.
            if header.element_type() == ArrayElementType::Reference {
                match value {
                    Value::Object(_) => {
                        write_prim_element(base, index, header.element_type(), value);
                    }
                    _ => {
                        let wrapper = self.alloc_object(AUTOBOX_CLASS_ID, 1);
                        self.set_field(wrapper, 0, value);
                        // Arm the process-wide wrapper latch — see the matching
                        // note in `GenerationalHeap::set_array_element` and
                        // `crate::autobox`.
                        crate::autobox::note_wrapper_created();
                        write_prim_element(
                            base,
                            index,
                            header.element_type(),
                            Value::Object(Some(wrapper)),
                        );
                    }
                }
            } else {
                write_prim_element(base, index, header.element_type(), value);
            }
        }
        Ok(())
    }

    // ----- Write barrier (no-op for non-generational heap) --------------------

    /// Write barrier — called after every reference store into a heap object.
    ///
    /// For the simple semi-space heap this is a no-op. The generational heap
    /// uses this to mark card table entries dirty.
    #[inline]
    pub fn write_barrier(&self, _obj: ObjectRef, _stored_value: Value) {
        // No-op for non-generational heap.
    }

    // ----- GC-related queries -----------------------------------------------

    /// Returns true when from-space usage exceeds the GC threshold.
    pub fn needs_gc(&self) -> bool {
        self.from_space.lock().used() >= self.gc_threshold
    }

    /// Lock and return mutable references to both semi-spaces.
    /// Used by the GC collector.
    pub fn lock_spaces(
        &self,
    ) -> (
        parking_lot::MutexGuard<'_, Arena>,
        parking_lot::MutexGuard<'_, Arena>,
    ) {
        let from = self.from_space.lock();
        let to = self.to_space.lock();
        (from, to)
    }

    /// Swap from-space and to-space after GC collection.
    pub fn swap_spaces(&self) {
        let mut from = self.from_space.lock();
        let mut to = self.to_space.lock();
        std::mem::swap(&mut *from, &mut *to);
    }

    /// Run a full garbage collection cycle.
    ///
    /// 1. Copies all live objects (reachable from roots) to to-space
    /// 2. Remaps monitor table keys
    /// 3. Swaps from-space and to-space
    ///
    /// After this call, `roots` contains updated ObjectRefs pointing to the
    /// new object locations, and the returned `GcResult` contains the pointer
    /// mapping for updating external references.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage(
        &self,
        _stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn crate::collector::MonitorCleanup,
    ) -> crate::gc::GcResult {
        // Part F: if a GPU critical section is active, spin-yield until it
        // ends before grabbing the arena locks. Gated to keep the default
        // feature build byte-identical to before.
        #[cfg(feature = "gpu-offload")]
        {
            self.wait_for_gpu_critical();
        }

        let mut from = self.from_space.lock();
        let mut to = self.to_space.lock();

        // JNI critical-section pins (vm-jni-roots #2): an array whose elements
        // are checked out by GetPrimitiveArrayCritical / Get<Type>ArrayElements
        // is registered in the process-global `crate::pinned` set for the
        // duration. Splice those addresses in as additional roots so a pinned
        // array reachable ONLY through native code is not reclaimed here and is
        // remapped to its post-GC address. Always compiled in (unlike the
        // gpu-gated set below).
        //
        // This is KEEP-ALIVE only — it intentionally does NOT keep the object IN
        // PLACE, and it does not need to: the JNI layer hands native code a
        // detached COPY of the array body (is_copy=JNI_TRUE), never a direct
        // heap pointer, so a relocation of the source array here is harmless (the
        // copy is independent and the root is remapped). No per-object
        // no-relocation enforcement in the collector is required. See the
        // `crate::pinned` module doc.
        let jni_pins: Vec<ObjectRef> = crate::pinned::pinned_addrs()
            .into_iter()
            // SAFETY: addresses come from the JNI pin set — live, pinned object
            // addresses registered by Get*Critical and not yet released.
            .map(|addr| unsafe { ObjectRef::from_raw(addr as *mut u8) })
            .collect();

        // Part F: walk GPU pinned refs as additional roots. We splice them onto
        // a combined buffer, run the collector, then copy the updated
        // ObjectRefs back into both the caller's slice and the pinned-refs
        // set so the next pin/unpin sees post-GC addresses.
        #[cfg(feature = "gpu-offload")]
        let result = {
            let pinned_snapshot: Vec<ObjectRef> = {
                let guard = self.gpu_pinned_refs.lock();
                guard.iter().copied().collect()
            };

            if pinned_snapshot.is_empty() && jni_pins.is_empty() {
                crate::gc::collect(&mut from, &mut to, roots)
            } else {
                let caller_len = roots.len();
                let gpu_len = pinned_snapshot.len();
                let mut combined: Vec<ObjectRef> =
                    Vec::with_capacity(caller_len + gpu_len + jni_pins.len());
                combined.extend_from_slice(roots);
                combined.extend_from_slice(&pinned_snapshot);
                combined.extend_from_slice(&jni_pins);

                let result = crate::gc::collect(&mut from, &mut to, &mut combined);

                // Copy the (possibly-updated) caller roots back.
                roots.copy_from_slice(&combined[..caller_len]);

                // Rewrite the GPU pinned-refs set with their new addresses.
                let new_pinned = &combined[caller_len..caller_len + gpu_len];
                let mut guard = self.gpu_pinned_refs.lock();
                guard.clear();
                for r in new_pinned {
                    guard.insert(*r);
                }

                result
            }
        };

        #[cfg(not(feature = "gpu-offload"))]
        let result = if jni_pins.is_empty() {
            crate::gc::collect(&mut from, &mut to, roots)
        } else {
            let caller_len = roots.len();
            let mut combined: Vec<ObjectRef> = Vec::with_capacity(caller_len + jni_pins.len());
            combined.extend_from_slice(roots);
            combined.extend_from_slice(&jni_pins);
            let result = crate::gc::collect(&mut from, &mut to, &mut combined);
            // Copy the (possibly-relocated) caller roots back.
            roots.copy_from_slice(&combined[..caller_len]);
            result
        };

        // Remap monitor table keys using the pointer mapping
        monitors.remap_after_gc(&result.pointer_map);

        // Swap spaces: to-space (now containing live data) becomes from-space
        std::mem::swap(&mut *from, &mut *to);

        result
    }

    /// Like [`collect_garbage`] but keeps dead finalizable objects alive so
    /// their `finalize()` method can be invoked.  Returns the GC result and
    /// the *new* (post-GC) addresses of dead finalizable objects.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage_with_finalizers(
        &self,
        _stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn crate::collector::MonitorCleanup,
    ) -> (crate::gc::GcResult, Vec<usize>) {
        // Part F: see `collect_garbage` for the rationale on both gated blocks.
        #[cfg(feature = "gpu-offload")]
        {
            self.wait_for_gpu_critical();
        }

        let mut from = self.from_space.lock();
        let mut to = self.to_space.lock();

        // JNI critical-section pins (vm-jni-roots #2): see `collect_garbage`.
        // Splice the process-global pin set in as additional roots so a pinned
        // array is not reclaimed and is remapped to its post-GC address.
        let jni_pins: Vec<ObjectRef> = crate::pinned::pinned_addrs()
            .into_iter()
            // SAFETY: addresses come from the JNI pin set — live, pinned object
            // addresses registered by Get*Critical and not yet released.
            .map(|addr| unsafe { ObjectRef::from_raw(addr as *mut u8) })
            .collect();

        #[cfg(feature = "gpu-offload")]
        let (result, dead_finalizers) = {
            let pinned_snapshot: Vec<ObjectRef> = {
                let guard = self.gpu_pinned_refs.lock();
                guard.iter().copied().collect()
            };

            if pinned_snapshot.is_empty() && jni_pins.is_empty() {
                crate::gc::collect_with_finalizers(&mut from, &mut to, roots, finalizer_addrs)
            } else {
                let caller_len = roots.len();
                let gpu_len = pinned_snapshot.len();
                let mut combined: Vec<ObjectRef> =
                    Vec::with_capacity(caller_len + gpu_len + jni_pins.len());
                combined.extend_from_slice(roots);
                combined.extend_from_slice(&pinned_snapshot);
                combined.extend_from_slice(&jni_pins);

                let (result, dead_finalizers) = crate::gc::collect_with_finalizers(
                    &mut from,
                    &mut to,
                    &mut combined,
                    finalizer_addrs,
                );

                roots.copy_from_slice(&combined[..caller_len]);

                let new_pinned = &combined[caller_len..caller_len + gpu_len];
                let mut guard = self.gpu_pinned_refs.lock();
                guard.clear();
                for r in new_pinned {
                    guard.insert(*r);
                }

                (result, dead_finalizers)
            }
        };

        #[cfg(not(feature = "gpu-offload"))]
        let (result, dead_finalizers) = if jni_pins.is_empty() {
            crate::gc::collect_with_finalizers(&mut from, &mut to, roots, finalizer_addrs)
        } else {
            let caller_len = roots.len();
            let mut combined: Vec<ObjectRef> = Vec::with_capacity(caller_len + jni_pins.len());
            combined.extend_from_slice(roots);
            combined.extend_from_slice(&jni_pins);
            let (result, dead_finalizers) = crate::gc::collect_with_finalizers(
                &mut from,
                &mut to,
                &mut combined,
                finalizer_addrs,
            );
            roots.copy_from_slice(&combined[..caller_len]);
            (result, dead_finalizers)
        };

        monitors.remap_after_gc(&result.pointer_map);
        std::mem::swap(&mut *from, &mut *to);

        (result, dead_finalizers)
    }

    // ----- Internal --------------------------------------------------------

    /// Allocate `size` bytes, 8-byte aligned, zero-initialized.
    /// Locks the from-space mutex for the duration of the allocation.
    ///
    /// # Panics
    /// Panics on OOM as a last resort. In normal operation the interpreter
    /// calls [`try_alloc_zeroed`](Self::try_alloc_zeroed) first, triggers GC
    /// on failure, and only falls back to this method when the object *must*
    /// be allocated (e.g. internal bookkeeping). If this panic fires it means
    /// the heap is genuinely exhausted after GC has already been attempted.
    fn alloc_zeroed(&self, size: usize) -> *mut u8 {
        // NUMA hint refresh — stub for the planned per-node arena
        // dispatch. On single-node hosts this is a no-op; on multi-node
        // hosts it records the calling thread's preferred node so
        // observers/tests can confirm dispatch intent. See the
        // `TODO(NUMA, multi-arena)` on `Heap` for the full plan.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(size, 8).unwrap_or_else(|| {
            // Use process::abort() instead of panic!() to avoid unwinding
            // through unsafe code. In production, GenerationalHeap's try_alloc
            // path handles OOM gracefully; this code path is only reached by
            // the legacy Heap (used in tests).
            eprintln!(
                "heap out of memory: tried to allocate {} bytes, from-space has {}/{} used",
                size,
                from.used(),
                from.capacity(),
            );
            std::process::abort();
        });
        // SAFETY: `ptr` was returned by `Arena::alloc` and points to `size` bytes
        // of uninitialized (or stale) memory within the arena. Writing zeros is
        // safe because the pointer and size are guaranteed valid by the arena.
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }
        ptr
    }

    /// Try to allocate `size` bytes, returning `None` if from-space is full.
    fn try_alloc_zeroed(&self, size: usize) -> Option<*mut u8> {
        // See `alloc_zeroed` for the NUMA hint rationale.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(size, 8)?;
        // SAFETY: `ptr` was returned by `Arena::alloc` and points to `size` bytes
        // of uninitialized memory within the arena. Writing zeros is safe because
        // the pointer and size are guaranteed valid by the arena.
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }
        Some(ptr)
    }

    /// Try to allocate a new Java object, returning `None` if the heap is full.
    /// Callers should trigger GC and retry, or throw `OutOfMemoryError`.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let fields_size = num_fields.checked_mul(SLOT_SIZE)?;
        let total_size = HEADER_SIZE.checked_add(fields_size)?;
        let ptr = self.try_alloc_zeroed(total_size)?;

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference, // hash installed lazily in the mark word on first request
            0,
            u32::try_from(num_fields).ok()?,
        );

        // SAFETY: `ptr` was just returned by `try_alloc_zeroed`, which guarantees
        // it is valid, non-null, properly aligned (8-byte), and has at least
        // `total_size` bytes available.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Try to allocate a new Java array, returning `None` if the heap is full.
    /// Callers should trigger GC and retry, or throw `OutOfMemoryError`.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if length > MAX_ARRAY_LENGTH {
            return None;
        }
        let data_size = array_data_size_checked(length, element_type)?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        let ptr = self.try_alloc_zeroed(total_size)?;

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type, // hash installed lazily in the mark word on first request
            u32::try_from(length).ok()?,
            u32::try_from(length).ok()?,
        );

        // SAFETY: `ptr` was returned by `try_alloc_zeroed` — valid, non-null,
        // 8-byte aligned, and has at least `total_size` bytes.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Generate the next identity hash code.
    fn next_hash(&self) -> i32 {
        self.next_hash_code.fetch_add(1, Ordering::Relaxed)
    }

    /// Total bytes currently allocated in from-space.
    pub fn allocated_bytes(&self) -> usize {
        self.from_space.lock().used()
    }

    /// The capacity of each semi-space.
    pub fn semi_space_capacity(&self) -> usize {
        self.from_space.lock().capacity()
    }

    // ----- GPU-offload coordination (Part F) ---------------------------------

    /// Enter a "GPU critical section": acquire a
    /// [`SafepointToken`](crate::safepoint::SafepointToken). While at least
    /// one token is alive on this heap, calls to [`Heap::collect_garbage`]
    /// and [`Heap::collect_garbage_with_finalizers`] spin-yield instead of
    /// running a GC cycle. Drop the token to release the GC.
    ///
    /// Available only with the `gpu-offload` Cargo feature. See
    /// [`crate::safepoint`] for the design rationale.
    #[cfg(feature = "gpu-offload")]
    pub fn enter_gpu_critical(&self) -> crate::safepoint::SafepointToken<'_> {
        crate::safepoint::SafepointToken::new(&self.gpu_critical_count)
    }

    /// Number of [`SafepointToken`](crate::safepoint::SafepointToken)s
    /// currently alive on this heap. Useful for tests.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_critical_count(&self) -> u32 {
        self.gpu_critical_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Pin `obj` so the GC walks it as an additional root for as long as
    /// the pin is in effect. Idempotent.
    ///
    /// Callers must balance every `pin_ref` with an `unpin_ref`. The
    /// canonical pattern is "pin around marshalling, unpin after the
    /// kernel result is read back into the heap".
    #[cfg(feature = "gpu-offload")]
    pub fn pin_ref(&self, obj: ObjectRef) {
        self.gpu_pinned_refs.lock().insert(obj);
    }

    /// Forget a previously pinned `obj`. No-op if the ref was not pinned.
    #[cfg(feature = "gpu-offload")]
    pub fn unpin_ref(&self, obj: ObjectRef) {
        self.gpu_pinned_refs.lock().remove(&obj);
    }

    /// Snapshot the current pinned-ref set. Mainly for tests; the GC path
    /// reads through the mutex directly.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_pinned_refs_snapshot(&self) -> Vec<ObjectRef> {
        self.gpu_pinned_refs.lock().iter().copied().collect()
    }

    /// Number of times the GC saw a non-zero `gpu_critical_count` and
    /// stepped aside. Exposed for tests.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_blocked_gc_count(&self) -> u64 {
        self.gpu_blocked_gc_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Block until no GPU critical section is in flight, using the
    /// crate-standard yield-spin pattern (see `reference.rs::remove_timeout`).
    ///
    /// Returns immediately if no tokens are alive. Otherwise spin-yields,
    /// re-checking the counter each iteration. After
    /// [`crate::safepoint::GPU_CRITICAL_DEADLINE_SECS`] seconds we log a
    /// single `tracing::warn!` and keep waiting — we NEVER force a
    /// collection while a token is alive.
    ///
    /// Each call also increments [`Heap::gpu_blocked_gc_count`] iff the
    /// counter was non-zero on entry, so callers can attribute blocked
    /// cycles in tests.
    #[cfg(feature = "gpu-offload")]
    fn wait_for_gpu_critical(&self) {
        use std::sync::atomic::Ordering;

        let count = self.gpu_critical_count.load(Ordering::Acquire);
        if count == 0 {
            return;
        }
        // Record one "GC asked, GPU said no" event per call. We do this once
        // (not once per iteration) so the test-visible counter matches the
        // number of attempted collections, not the number of yield loops.
        self.gpu_blocked_gc_count.fetch_add(1, Ordering::AcqRel);

        let start = std::time::Instant::now();
        let deadline = std::time::Duration::from_secs(crate::safepoint::GPU_CRITICAL_DEADLINE_SECS);
        let mut warned = false;
        loop {
            std::thread::yield_now();
            let now = self.gpu_critical_count.load(Ordering::Acquire);
            if now == 0 {
                return;
            }
            if !warned && start.elapsed() >= deadline {
                tracing::warn!(
                    "GC delayed >5s by GPU critical section — {} active tokens",
                    now
                );
                warned = true;
            }
        }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Heap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = self.from_space.lock();
        let to = self.to_space.lock();
        f.debug_struct("Heap")
            .field("from_space_used", &from.used())
            .field("from_space_capacity", &from.capacity())
            .field("to_space_used", &to.used())
            .field("to_space_capacity", &to.capacity())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Slot read/write helpers
// ---------------------------------------------------------------------------

/// Map a JVM field descriptor's first byte to its default [`Value`] per the
/// JVM spec §2.3 (primitive default values).
///
/// | Descriptor | Java type           | Default `Value` variant |
/// |------------|---------------------|-------------------------|
/// | `B`        | byte                | `Int(0)` (sign-extended) |
/// | `C`        | char                | `Int(0)` (zero-extended) |
/// | `I`        | int                 | `Int(0)`                |
/// | `S`        | short               | `Int(0)` (sign-extended) |
/// | `Z`        | boolean             | `Int(0)` (0 = false)    |
/// | `J`        | long                | `Long(0)`               |
/// | `F`        | float               | `Float(0.0)`            |
/// | `D`        | double              | `Double(0.0)`           |
/// | `L…;` / `[`| reference / array   | `None` → keep zero bits |
/// | *other*    | malformed           | `None` → keep zero bits |
///
/// For reference-typed fields the caller should rely on the zeroed memory
/// (which decodes as `Value::Object(None)`) — returning `None` here signals
/// "no override needed". This also fail-opens for unknown descriptor bytes,
/// preserving the pre-fix behavior so malformed class metadata cannot
/// regress non-primitive paths.
#[inline]
pub fn default_value_for_descriptor(desc_byte: u8) -> Option<Value> {
    match desc_byte {
        // Integer-family primitives all live in Value::Int per JVM spec.
        b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(0)),
        b'J' => Some(Value::Long(0)),
        b'F' => Some(Value::Float(0.0)),
        b'D' => Some(Value::Double(0.0)),
        // References (L...;) and arrays ([) default to null, which matches
        // the zero-bits decoding of Value::Object(None).
        _ => None,
    }
}

// ----- G30: the descriptor-coercion loss instrument -----------------------
//
// Why this exists, in one paragraph, because the next reader will otherwise
// re-derive it from a symptom the way three records already had to.
//
// `coerce_field_value_by_descriptor` has two jobs that look alike and are not.
// Most of its arms NORMALISE: a `Double` bit pattern that was meant as a long
// is reinterpreted, an `Int` is widened to a `Long`, and no information is
// lost. Three arms DESTROY: a primitive handed to an `L`/`[` slot becomes
// `null`, a `null` handed to a primitive slot becomes the typed zero, and an
// object POINTER handed to a primitive slot becomes its own address as a
// number. Until 2026-08-17 all three were silent in release, and the only
// detector that named the species — `overlay_check_access` in
// `vm/src/vm/vm_exec.rs` — is gated behind `CRATONVM_DBG_OVERLAY` AND
// requires the class to carry a fabricated shadow layout, so it cannot see a
// class this VM never modelled.
//
// This is NOT the `cratonvm::gc::guard` W7-84 warning and must not be counted
// with it. MEASURED 2026-08-17, `RJdkHello` `--jdk-only`: all 12 W7-84
// warnings in a run are `class_id=ClassId(12) index=0`, one class and one
// slot, the VM's own class-mirror populator over
// `java.lang.Class.cachedConstructor` (`vm/src/vm/vm_object.rs`). That path is
// `autobox::box_for_reference_slot` reached from the DESCRIPTOR-LESS
// `set_field`; it BOXES. This path is descriptor-driven; it NULLS. Different
// function, different answer, disjoint populations.
//
// The instrument deliberately does not change any answer. See
// `docs/known-issues/jdk-only/G30-1-the-silent-reference-slot-coercion-20260817.md`.

/// Which kind of value-destroying coercion fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldCoercionLoss {
    /// A primitive was handed to a slot the class declares `L`/`[`, and was
    /// replaced by `null`. The G25/G30 headline species.
    PrimitiveIntoReference,
    /// A primitive was handed to a slot the class declares `L`/`[` and was
    /// **stored as-is** (the `Float` / `ReturnAddress` fall-through). Reported
    /// separately because the value survives — the reader of that slot gets a
    /// primitive where a reference is declared, rather than a null.
    PrimitiveIntoReferenceUncoerced,
    /// `Value::Object(None)` was handed to a primitive slot and became the
    /// typed JVMS zero — i.e. `null` silently means `0` / `false`.
    NullIntoPrimitive,
    /// A live object POINTER was handed to a primitive slot and was published
    /// as its own address. The worst of the three: the answer is not merely
    /// wrong, it is non-deterministic and leaks a heap address.
    PointerIntoPrimitive,
}

impl FieldCoercionLoss {
    /// Number of variants; the row count of the counter matrix.
    pub const COUNT: usize = 4;

    #[inline]
    fn index(self) -> usize {
        match self {
            FieldCoercionLoss::PrimitiveIntoReference => 0,
            FieldCoercionLoss::PrimitiveIntoReferenceUncoerced => 1,
            FieldCoercionLoss::NullIntoPrimitive => 2,
            FieldCoercionLoss::PointerIntoPrimitive => 3,
        }
    }

    /// Stable, greppable name for logs and dumps.
    pub fn name(self) -> &'static str {
        match self {
            FieldCoercionLoss::PrimitiveIntoReference => "primitive-into-reference",
            FieldCoercionLoss::PrimitiveIntoReferenceUncoerced => {
                "primitive-into-reference-uncoerced"
            }
            FieldCoercionLoss::NullIntoPrimitive => "null-into-primitive",
            FieldCoercionLoss::PointerIntoPrimitive => "pointer-into-primitive",
        }
    }
}

/// Which side of the field access the coercion ran on.
///
/// A READ that coerces is usually benign — the slot was never
/// descriptor-initialised and the read is *repairing* it (MEASURED: 336 of
/// the 352 coercions in one `RJdkNet` run are reads of
/// `java.lang.ref.ReferenceQueue.head`, which correctly answer `null`). A
/// STORE that coerces is the defect. Reporting them in one bucket is how the
/// signal gets lost, so they are counted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldAccessKind {
    Read,
    Store,
    /// The caller did not say. Every collector on the live `VmHeap` dispatch
    /// path is currently here; see [`coerce_field_value_for_slot`].
    Unattributed,
}

impl FieldAccessKind {
    /// Number of variants; the column count of the counter matrix.
    pub const COUNT: usize = 3;

    #[inline]
    fn index(self) -> usize {
        match self {
            FieldAccessKind::Read => 0,
            FieldAccessKind::Store => 1,
            FieldAccessKind::Unattributed => 2,
        }
    }

    /// Stable, greppable name for logs and dumps.
    pub fn name(self) -> &'static str {
        match self {
            FieldAccessKind::Read => "read",
            FieldAccessKind::Store => "store",
            FieldAccessKind::Unattributed => "unattributed",
        }
    }
}

/// Everything the instrument knows about where a coercion came from.
///
/// `Copy` and three words wide so passing it costs nothing on the arms that
/// never look at it.
#[derive(Debug, Clone, Copy)]
pub struct FieldCoercionSite {
    pub kind: FieldAccessKind,
    pub class_id: Option<ClassId>,
    pub index: Option<usize>,
}

impl FieldCoercionSite {
    /// A caller that carries no provenance at all.
    pub const UNATTRIBUTED: Self = Self {
        kind: FieldAccessKind::Unattributed,
        class_id: None,
        index: None,
    };

    /// A descriptor-aware READ of `index` on an object of `class_id`.
    #[inline]
    pub fn read(class_id: Option<ClassId>, index: usize) -> Self {
        Self {
            kind: FieldAccessKind::Read,
            class_id,
            index: Some(index),
        }
    }

    /// A descriptor-aware STORE to `index` on an object of `class_id`.
    #[inline]
    pub fn store(class_id: Option<ClassId>, index: usize) -> Self {
        Self {
            kind: FieldAccessKind::Store,
            class_id,
            index: Some(index),
        }
    }
}

/// One counter per (species, access kind). Flat so the increment is a single
/// relaxed `fetch_add` on a cold path.
static COERCION_LOSSES: [[AtomicU64; FieldAccessKind::COUNT]; FieldCoercionLoss::COUNT] = [
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
];

/// How many times each (species, access kind) pair has fired in this process.
///
/// Rows are indexed by [`FieldCoercionLoss`] in declaration order, columns by
/// [`FieldAccessKind`]. Intended for a shutdown summary or the native-registry
/// dump; see [`field_coercion_loss_report`] for a rendered form.
pub fn field_coercion_loss_counts() -> [[u64; FieldAccessKind::COUNT]; FieldCoercionLoss::COUNT] {
    let mut out = [[0u64; FieldAccessKind::COUNT]; FieldCoercionLoss::COUNT];
    for (r, row) in COERCION_LOSSES.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            out[r][c] = cell.load(Ordering::Relaxed);
        }
    }
    out
}

/// Total number of value-destroying descriptor coercions in this process.
pub fn field_coercion_loss_total() -> u64 {
    COERCION_LOSSES
        .iter()
        .flatten()
        .map(|c| c.load(Ordering::Relaxed))
        .sum()
}

/// A one-line-per-species rendering of [`field_coercion_loss_counts`], or
/// `None` when nothing fired.
///
/// `None` rather than an empty string so a caller can print a section header
/// only when there is something under it.
pub fn field_coercion_loss_report() -> Option<String> {
    let counts = field_coercion_loss_counts();
    if counts.iter().flatten().all(|&n| n == 0) {
        return None;
    }
    let species = [
        FieldCoercionLoss::PrimitiveIntoReference,
        FieldCoercionLoss::PrimitiveIntoReferenceUncoerced,
        FieldCoercionLoss::NullIntoPrimitive,
        FieldCoercionLoss::PointerIntoPrimitive,
    ];
    let kinds = [
        FieldAccessKind::Read,
        FieldAccessKind::Store,
        FieldAccessKind::Unattributed,
    ];
    let mut s = String::new();
    for sp in species {
        let row = counts[sp.index()];
        if row.iter().all(|&n| n == 0) {
            continue;
        }
        s.push_str(sp.name());
        for k in kinds {
            s.push_str(&format!(" {}={}", k.name(), row[k.index()]));
        }
        s.push('\n');
    }
    Some(s)
}

/// `CRATONVM_DBG_COERCION=1` — log EVERY loss with a backtrace instead of the
/// rate-limited sample.
///
/// Read once. This is the flag a lane repairing individual sites wants; the
/// default sample is for a reader who did not know the defect existed.
fn coercion_loss_verbose() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        // `flags::runtime_var`, not `std::env::var`. A DECLARED name read
        // raw is served by a live `getenv` instead of the latched snapshot,
        // so `CRATONVM_DBG=coercion` would silently do nothing here and a
        // test could not arrange it through `with_thread_overrides` — which
        // matters for a diagnostic the guard's own WARN text tells operators
        // to set. Check 4 of `tools/flag-census/check-surface.sh` names this
        // call site as a core-crate bypass.
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_COERCION")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// The instrument. Counts always; warns on a rate-limited sample.
///
/// Rate limit is `n < 4 || n.is_power_of_two()`, PER SPECIES — the same shape
/// as `autobox::observe_primitive_into_reference_field` and the sibling
/// `cratonvm::gc::guard` records, chosen so a boot that coerces thousands of
/// times costs ~a dozen lines rather than minutes of stderr. Per species
/// rather than globally so a high-frequency benign read population (see
/// [`FieldAccessKind`]) cannot bury a rare store.
///
/// Target `cratonvm::gc::guard`, deliberately: that target already emits on
/// this VM's default stderr configuration, so anyone who has ever looked at a
/// CratonVM boot log has the filter for it, and a new target would have been
/// one more thing to know about.
///
/// **No `debug_assert!` and no refusal.** `autobox.rs`'s module note settles
/// why, and the reasoning transfers verbatim: the population reaching this is
/// live on shipped paths (400 measured sites), so a hard error converts a
/// wrong answer into a crash, and an assert reds the synthetic-JDK tests where
/// a fabricated class's slot genuinely IS the primitive it is handed. Take the
/// diagnostic half without the behaviour half.
#[cold]
fn note_field_coercion_loss(
    loss: FieldCoercionLoss,
    site: FieldCoercionSite,
    value: Value,
    desc_byte: u8,
) {
    let n = COERCION_LOSSES[loss.index()][site.kind.index()].fetch_add(1, Ordering::Relaxed);
    let verbose = coercion_loss_verbose();
    if !verbose && !(n < 4 || n.is_power_of_two()) {
        return;
    }
    let descriptor = desc_byte as char;
    let class = site.class_id.map(|c| c.as_u32() as i64).unwrap_or(-1);
    let index = site.index.map(|i| i as i64).unwrap_or(-1);
    tracing::warn!(
        target: "cratonvm::gc::guard",
        species = loss.name(),
        access = site.kind.name(),
        descriptor = %descriptor,
        value = ?value,
        class_id = class,
        index,
        occurrence = n,
        "a descriptor-aware field access DESTROYED the value it was handed \
         (G30-1-the-silent-reference-slot-coercion-20260817.md). This is NOT \
         the W7-84 autobox guard: that one boxes and fires only for the class \
         mirror; this one nulls (or zeroes) and is the shape behind the null \
         `java.net.ServerSocket.impl`. class_id=-1/index=-1 means the caller \
         is one of the collectors that has not yet been given provenance \
         (G30 NOMINATION 1). Run with CRATONVM_DBG_LAYOUT=1 to resolve a \
         class_id to a name, or CRATONVM_DBG_COERCION=1 for every occurrence \
         with a backtrace. THIS GUARD SEES DESCRIPTOR MISMATCHES ONLY: a \
         wrong slot whose value happens to fit the field's own descriptor is \
         invisible here, so a quiet log is not a clean one. G59-1 measured \
         both halves of one defect at once -- two writes warned, and two \
         more from the same line landed silently on an int and set a 1ms \
         connect timeout.",
    );
    if verbose {
        tracing::warn!(
            target: "cratonvm::gc::guard",
            "  coercion-loss backtrace:\n{}",
            std::backtrace::Backtrace::force_capture(),
        );
    }
}

/// T10.9.E — Coerce a loaded-from-slot `Value` to match its declared field
/// type.
///
/// This is the runtime complement of [`default_value_for_descriptor`]: that
/// helper sets the **initial** slot contents so zero-init has the correct
/// tag; this helper **normalises** any drifted Value on read/write so
/// downstream code always sees the declared variant.
///
/// Behaviour is a superset of `CompactValue::decode_by_descriptor`:
/// - `Value::Object(None)` on a primitive descriptor → the typed JVMS zero.
/// - `Value::Long(x)` on a J descriptor → unchanged.
/// - `Value::Double(d)` on a J descriptor → `Value::Long(d.to_bits() as i64)`.
///   This is the Session 93 fix: a long bit pattern that happened to be
///   written as Double (via CompactValue::long + round-trip through
///   `to_value`) is reinterpreted as the intended long.
/// - symmetric for D/F/I families.
/// - references (L/[): unchanged from raw read.
/// - unknown descriptor: unchanged (`_ => value`).
///
/// Defensive: never panics; always returns a `Value`.
///
/// # G30: the lossy arms are now instrumented, and NOTHING ELSE CHANGED
///
/// Three of the arms below do not normalise a value, they DESTROY it, and
/// until 2026-08-17 every one of them was silent in a release build
/// (`G30-1-the-silent-reference-slot-coercion-20260817.md`). They are now
/// routed through [`note_field_coercion_loss`], which counts and (rate-limited)
/// warns. **The returned `Value` is byte-identical to what this function
/// returned before**, deliberately: 400 measured sites in this tree rely on
/// the current answers, `java.util.HashMap.table` load-bearingly so (see the
/// `b'L'` arm), and a VM that started refusing them all at once would fail
/// catastrophically and prove nothing. Visibility first; the per-site repairs
/// are nominated individually.
///
/// See [`coerce_field_value_for_slot`] for the variant that carries the
/// class and slot into the report.
#[inline]
pub fn coerce_field_value_by_descriptor(value: Value, desc_byte: u8) -> Value {
    coerce_field_value_for_slot(value, desc_byte, FieldCoercionSite::UNATTRIBUTED)
}

/// [`coerce_field_value_by_descriptor`] with provenance for the instrument.
///
/// The coercion itself is identical; `site` only decides how good the warning
/// is. `Heap`'s own four descriptor-aware accessors pass a real site. The
/// three collectors on the live `VmHeap` dispatch path (`gen_heap.rs`,
/// `g1.rs`, `zgc.rs` via `collector.rs`) still call the descriptor-only
/// entry point above and therefore report `UNATTRIBUTED`; upgrading them is
/// a one-line change per call site and is NOMINATION 1 of the G30 record —
/// it is not taken here because those files belong to other lanes.
#[inline]
pub fn coerce_field_value_for_slot(value: Value, desc_byte: u8, site: FieldCoercionSite) -> Value {
    match desc_byte {
        b'J' => match value {
            Value::Long(_) => value,
            Value::Double(d) => Value::Long(d.to_bits() as i64),
            Value::Float(f) => Value::Long(f.to_bits() as i64),
            Value::Int(i) => Value::Long(i as i64),
            // `Uninitialized` is the allocator's "no value yet" tag, not a
            // claim by any writer about what the slot holds, so it is the one
            // arm here that is NOT a loss and is left silent. Splitting it out
            // of the shared `Object(None) | Uninitialized` pattern is the only
            // structural change in this function; both still yield `Long(0)`.
            Value::Uninitialized => Value::Long(0),
            Value::Object(None) => {
                note_field_coercion_loss(FieldCoercionLoss::NullIntoPrimitive, site, value, b'J');
                Value::Long(0)
            }
            // An object pointer landing in a long slot is upstream drift;
            // surface the raw pointer as a long so downstream unsafe ops
            // can decode it (matches HotSpot behavior of treating the slot
            // as raw bits).
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    b'J',
                );
                Value::Long(o.as_ptr() as usize as i64)
            }
            Value::ReturnAddress(pc) => Value::Long(pc as i64),
        },
        b'D' => match value {
            Value::Double(_) => value,
            Value::Long(l) => Value::Double(f64::from_bits(l as u64)),
            Value::Float(f) => Value::Double(f as f64),
            Value::Int(i) => Value::Double(i as f64),
            Value::Uninitialized => Value::Double(0.0),
            Value::Object(None) => {
                note_field_coercion_loss(FieldCoercionLoss::NullIntoPrimitive, site, value, b'D');
                Value::Double(0.0)
            }
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    b'D',
                );
                Value::Double(f64::from_bits(o.as_ptr() as usize as u64))
            }
            Value::ReturnAddress(pc) => Value::Double(pc as f64),
        },
        b'F' => match value {
            Value::Float(_) => value,
            Value::Int(i) => Value::Float(f32::from_bits(i as u32)),
            Value::Long(l) => Value::Float(f32::from_bits(l as u32)),
            // G52, THE ONE CELL THAT DISAGREES WITH ITSELF — reported, not
            // moved. The `Long` arm one line up reads the slot as a BIT
            // pattern; this arm reads it as a NUMBER. They are the same
            // untagged compact slot, so they contradict each other:
            // `Long(5)` at `F` is `Float(7e-45)` and
            // `Double(f64::from_bits(5))` at `F` is `Float(0.0)`. Every other
            // cross-variant pair in this function agrees (see the `b'I'`
            // `Double` arm for the argument and the invariant it is pinned
            // by); this is the only pair that does not.
            //
            // NOT changed here, deliberately. Either answer is defensible in
            // isolation — `d as f32` is the correct JVMS `d2f` for a genuine
            // double, `f32::from_bits(d.to_bits() as u32)` is the correct
            // decode for an untagged slot — and MEASURED 2026-08-17, zero
            // `Value::Double` reached ANY `b'F'` slot in a 26-vector-run
            // sweep, so there is no live population to decide it against and
            // no run that could falsify a change. Whoever gets a
            // non-zero count here first should decide it; until then the
            // disagreement is pinned by
            // `the_double_and_long_arms_disagree_only_at_a_float_slot` so it
            // cannot drift in silence.
            Value::Double(d) => Value::Float(d as f32),
            Value::Uninitialized => Value::Float(0.0),
            Value::Object(None) => {
                note_field_coercion_loss(FieldCoercionLoss::NullIntoPrimitive, site, value, b'F');
                Value::Float(0.0)
            }
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    b'F',
                );
                Value::Float(f32::from_bits(o.as_ptr() as usize as u32))
            }
            Value::ReturnAddress(pc) => Value::Float(f32::from_bits(pc)),
        },
        b'I' | b'B' | b'C' | b'S' | b'Z' => match value {
            Value::Int(_) => value,
            Value::Long(l) => Value::Int(l as i32),
            Value::Float(f) => Value::Int(f.to_bits() as i32),
            // G52, DELIBERATE — this is a BIT projection and not `d as i32`,
            // and the reason is the same one the `b'J'` arm above states.
            //
            // `G43-1` NOMINATION 5 asked for this to become the numeric
            // narrowing `d as i32`, on the ground that a bit-cast makes the
            // blast radius of a mis-slotted `Double` depend on its VALUE. The
            // complaint is right; the proposed repair is wrong, three times
            // over, and this comment exists so nobody has to re-derive that.
            //
            // 1. IT IS THE UNTAGGED-COMPACT-SLOT DECODE, NOT A DOUBLE
            //    CONVERSION. This function declares itself a superset of
            //    `CompactValue::decode_by_descriptor`, and this is the arm
            //    that makes the claim true: `types/src/compact_value.rs:1675`
            //    answers an integral descriptor on an UNTAGGED slot with
            //    `Value::Int(self.0 as u32 as i32)` — the low 32 raw bits.
            //    An untagged slot decoded through `to_value()` surfaces as
            //    `Value::Double(f64::from_bits(raw))`, so a `Value::Double`
            //    arriving here is, on the shipped compact-layout path,
            //    OVERWHELMINGLY A LONG/INT BIT PATTERN rather than a number.
            //    Taking its low half is the correct `l2i`.
            //
            // 2. IT IS FORCED BY THE `b'J'` ARM. `Value::Long(l)` and
            //    `Value::Double(f64::from_bits(l as u64))` are the same
            //    untagged slot read two ways; they MUST agree about the
            //    slot's low half, or a long read at `I` and the same long
            //    read at `J` contradict each other. `d.to_bits() as i32`
            //    makes them agree for every `l`; `d as i32` makes them
            //    disagree for every `l` outside the subnormal window —
            //    `CompactValue::long(5)` would read back as `0`, which is
            //    Session 93 resurrected at 32 bits. Pinned by
            //    `a_double_at_an_integral_slot_decodes_like_the_untagged_long_it_usually_is`.
            //
            // 3. ON `G43-1`'s OWN CASE THE PROPOSED REPAIR IS STRICTLY WORSE.
            //    `G43-1` §5.2 shows a synthetic `Provider` version landing on
            //    the real `java.util.Hashtable.count`, surviving only because
            //    `25.0f64.to_bits() as i32 == 0` and `getEnumeration`
            //    early-returns on `count == 0`. Under `d as i32` that same
            //    write yields `count == 25`, the early return does NOT fire,
            //    and `keys()` walks the `String` sitting in `table` as an
            //    `Entry[]` — i.e. the numeric rule converts that record's
            //    LATENT corruption into a LIVE one. (The producer is gated
            //    off at HEAD by `provider_has_named_layout`,
            //    `native-builtins/src/jca/provider_chain.rs:285`.)
            //
            // What IS wrong is that the ambiguity is unresolvable here: a
            // genuine `double` mis-slotted into an `int` field and an
            // untagged long that round-tripped through `to_value()` arrive as
            // the same `Value::Double`, and this arm resolves it in favour of
            // the one that happens on a shipped path. That is a property of
            // the boxed-`Value` representation, not of this line, and it
            // cannot be fixed by choosing the other answer.
            Value::Double(d) => Value::Int(d.to_bits() as i32),
            Value::Uninitialized => Value::Int(0),
            // MEASURED, G25-1 §1 consequence 1: this arm is how
            // `create_ssl_server_socket`'s fourth write — an explicit
            // `Value::Object(None)` aimed at what its model called slot 3 —
            // became `java.net.ServerSocket.closed = false`. It is not a
            // "dropped" store, it is a store of the typed zero.
            Value::Object(None) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::NullIntoPrimitive,
                    site,
                    value,
                    desc_byte,
                );
                Value::Int(0)
            }
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    desc_byte,
                );
                Value::Int(o.as_ptr() as usize as i32)
            }
            Value::ReturnAddress(pc) => Value::Int(pc as i32),
        },
        b'L' | b'[' => match value {
            Value::Object(_) => value,
            // A primitive landing where a reference is declared is a verifier
            // violation; degrade to null rather than surface a bogus Value
            // variant to native code. S111r29: extend this to ALL Int/Long
            // values (not just zero) — synthetic init paths historically wrote
            // `Int(capacity)` to slots that the real-JDK class layout declares
            // as references (e.g. `HashMap.table: [Ljava/util/HashMap$Node;`),
            // which then aborted JDK bytecode `arraylength` with
            //   `expected object reference, got int(N)`. Coercing the bogus
            // primitive to null lets `HashMap.resize()`'s
            //   `(oldTab == null) ? 0 : oldTab.length` branch handle the
            // never-initialized case correctly.
            //
            // G30: this rule is DELIBERATE AND LOAD-BEARING and must not be
            // deleted — `the_hashmap_table_degrade_to_null_is_pinned` fails if
            // it is. What was wrong was never the rule; it was that the rule
            // fired in silence, so the 374 callers writing an `Int` at a
            // reference slot could not tell they were writing null. The
            // instrument below is the whole of the change.
            Value::Int(_) | Value::Long(_) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PrimitiveIntoReference,
                    site,
                    value,
                    desc_byte,
                );
                Value::Object(None)
            }
            // A `Value::Double` landing in a reference-typed (`L`/`[`) field is
            // a genuine type error — the bytecode wrote a primitive where the
            // class layout declares a reference. Degrade it to null, exactly
            // like the `Int`/`Long` case above.
            //
            // SAFETY/UAF NOTE: a previous revision reinterpreted an 8-aligned,
            // sub-2^48 double bit-pattern as a live `ObjectRef` via
            // `ObjectRef::from_raw`. That fabricates a wild heap pointer out of
            // arbitrary numeric data: any double that happens to bit-match an
            // aligned address would be handed to the GC and field accessors as
            // a real object, causing a use-after-free or worse. Numeric data
            // is NOT a pointer — never manufacture one. Coerce to null.
            Value::Double(_) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PrimitiveIntoReference,
                    site,
                    value,
                    desc_byte,
                );
                Value::Object(None)
            }
            // G30, SOURCE-VERIFIED and left alone on purpose: `Float` and
            // `ReturnAddress` fall through the old `_ => value` arm, so a
            // `Value::Float` written at a reference slot is neither refused
            // nor nulled — it is STORED, and a later reader sees a `Float`
            // where the class declares an object. That asymmetry with the
            // `Double` arm two lines up is almost certainly an oversight, but
            // closing it is a behaviour change at an unknown number of sites,
            // so it is reported and NOT repaired here (G30 NOMINATION 6).
            // The value is passed through byte-identically to before.
            //
            // G52 — "an unknown number of sites" is no longer unknown, and
            // the answer is ZERO. This species now has a denominator from
            // three independent directions:
            //
            // * RUNTIME, `primitive-into-reference-uncoerced`: 0 events in
            //   1,339 (G45-1 §3, 19 vectors) and 0 in a further 638 (G52-1,
            //   10 vectors). 0 / 1,977.
            // * RUNTIME, the other instrument: 0 `Float`-at-a-reference-slot
            //   rows in a 4-vector `CRATONVM_DBG_OVERLAY=1` sweep, whose
            //   211 `[cross-type]` rows are ALL `Int` at `L`/`[`.
            // * STATIC: every `Value::Float` reaching a `set_field`-family
            //   call in the native crates (25 sites) targets a slot whose
            //   REAL JDK-25 descriptor is `F` — `HashMap`/`Hashtable`
            //   `loadFactor`, `java.lang.Float.value`, `CharsetEncoder`
            //   slots 1/2 (`averageBytesPerChar`/`maxBytesPerChar`), and
            //   `Float` wrapper boxes. Resolved against `javap -p` on
            //   HotSpot 25.0.3+9; see the G52-1 record for the table.
            //
            // `ReturnAddress` is stronger than empty, it is UNREACHABLE: the
            // only producer in the tree is `jsr`/`jsr_w`
            // (`vm/src/runtime/interpreter/opcodes.rs:4009`/`:4016`), those
            // opcodes are illegal in class files of version >= 51, and even
            // a hypothetical one could only reach a slot through the
            // interpreter's `putfield`, which does not coerce.
            //
            // So closing the hole (nulling `Float` like `Double`) is now a
            // safe one-line change — and also an UNVERIFIABLE one, because
            // an empty population means no run can tell the two versions
            // apart. It is still not taken here for that reason. What WOULD
            // justify taking it: a non-zero
            // `primitive-into-reference-uncoerced` count from any vector.
            Value::Float(_) | Value::ReturnAddress(_) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PrimitiveIntoReferenceUncoerced,
                    site,
                    value,
                    desc_byte,
                );
                value
            }
            Value::Uninitialized => value,
        },
        _ => value,
    }
}

/// Get a pointer to the slot at `index` (field or array element).
#[inline]
unsafe fn slot_ptr(obj_ref: ObjectRef, index: usize) -> *mut u8 {
    obj_ref.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE)
}

/// Read a `Value` from a slot.
///
/// We store the raw 16-byte `Value` enum directly in the slot. The first
/// byte is the variant discriminant (`Int=0`, `Long=1`, `Float=2`,
/// `Double=3`, `Object=4`, `ReturnAddress=5`, `Uninitialized=6`); the niche
/// `Object(None)` is discriminant 4 with an all-zero pointer payload.
///
/// DECODE RULE (R-niche): there is no "zero bytes decode to null" shortcut.
/// Since `Value::Object` gained a `NonNull` niche, the all-zero bit pattern
/// has discriminant byte 0 and decodes as `Value::Int(0)` — bit-identical to
/// a slot that was explicitly written `Value::Int(0)`. A read therefore
/// returns exactly the variant whose discriminant was last *written* into the
/// slot. Reference/uninitialized slots are NOT left zeroed: callers that need
/// the `null` default (e.g. `alloc_object_with_descriptors`) write an explicit
/// `Value::Object(None)` so the discriminant byte is 4 and this read yields
/// `null`, not `Int(0)`.
///
/// PLAIN-SLOT TEARING FIX (2026-07-06): this used to be a bare
/// `std::ptr::read::<Value>(ptr)` — a non-atomic 16-byte copy that can race
/// a concurrent plain `set_field` from another mutator thread. Commit
/// `4e6b560f` ("atomic-per-word object-slot access for concurrent marking")
/// already closed this gap for the GC-marker-vs-JIT-store race using
/// `cratonvm_types::read_value_atomic`/`write_value_atomic`, but left the
/// interpreter's own plain `get_field`/`set_field` (this function) using the
/// raw, non-atomic path — so two ordinary mutator threads doing plain
/// `getfield`/`putfield` on the SAME field slot could still tear each
/// other's writes. Real JDK library code legally relies on this being
/// tear-free (e.g. `ReentrantReadWriteLock$Sync`'s plain `firstReader`/
/// `firstReaderHoldCount`, published via a nearby `volatile`/CAS write to
/// `state` — see
/// fixed-suite-bugs/elasticsearch-suite/elasticsearch-lucene-binary-docvalues-range-hangs.md
/// #3). Delegates to the same already-proven `read_value_atomic` helper.
///
/// # Safety
/// The pointer must be valid and 8-byte aligned.
unsafe fn read_slot(ptr: *mut u8) -> Value {
    read_value_cell_checked(ptr as *const Value, "heap::read_slot")
}

/// Read a legacy 16-byte `Value` cell, screening the discriminant first.
///
/// # why this is not `read_value_atomic`
///
/// `gen_heap::read_slot` has validated the discriminant since `HIB-CV-32`; the
/// other three legacy-cell readers did not — `heap::read_slot` and
/// `g1::get_field` used the unchecked `read_value_atomic`, and `zgc::get_field`
/// used a bare non-atomic `std::ptr::read`. That asymmetry is not a style
/// difference, it decides whether a heap reference-integrity defect is
/// *reported* or *fatal*, and it is why one defect presented as two unrelated
/// outcomes:
///
/// `JsonMarshallerTests` reads a `String.value` field through a stale receiver
/// whose memory has been swept and handed to something else, so the cell holds
/// two heap pointers (`raw0=0x…3c50a5d8 raw1=0x…3c7218e0`) rather than a
/// `(tag, payload)` pair. Generational screened it, returned null, logged, and
/// the class passed 17/17. ZGC and G1 transmuted it and handed the result to a
/// `match`, whose jump-table load is `[table + disc*4]` with no bounds check
/// because Rust guarantees an in-range discriminant — so the low word of a heap
/// pointer became the index. The faulting address is exactly that arithmetic:
/// `r10=0x00007FF7EEFFB914` (table) `+ rax=0x440A7D38` (the bogus discriminant)
/// `* 4 = 0x00007FF8FF29ADF4`, the address in the SIGSEGV report.
///
/// Screening here does NOT fix the producer — the stale receiver is a separate,
/// still-open defect that is present under Generational too, where this guard is
/// the only reason its green looks clean. What it fixes is that the same corrupt
/// cell must not be a localizable diagnostic on one collector and an
/// unrecoverable crash on the others.
///
/// # Safety
/// The pointer must be valid, readable for 16 bytes, and 8-byte aligned.
/// Corrupt-`Value`-cell telemetry, published for the VM.
///
/// The guard below lives in the collector crate, so it can name the CELL - the
/// address and the two words it holds - and nothing else. The question the
/// record it belongs to actually asks is the other half: which reference points
/// at a block that was swept and re-served, and who is holding it. Only the VM
/// can answer that, because only the VM has the receiver and the owning thread's
/// frames. Publishing the hit count lets a VM-side field read notice "that read
/// tripped the guard" and report the producer at the moment it happens; the
/// three coordinates let it report the same cell the guard did.
///
/// Relaxed throughout: this is a diagnostic on an already-failed path, and the
/// only ordering that matters - the counter moving before the reader re-reads it
/// - comes from the read being sequenced between the two loads on one thread.
pub(crate) static CORRUPT_CELL_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static CORRUPT_CELL_SLOT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static CORRUPT_CELL_RAW0: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CORRUPT_CELL_RAW1: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many corrupt `Value` cells this process has decoded. See
/// [`CORRUPT_CELL_HITS`].
pub fn corrupt_cell_hits() -> u64 {
    CORRUPT_CELL_HITS.load(Ordering::Relaxed)
}

/// `(slot address, raw word 0, raw word 1)` of the last corrupt cell decoded.
/// Meaningful only when [`corrupt_cell_hits`] is non-zero.
pub fn corrupt_cell_last() -> (usize, u64, u64) {
    (
        CORRUPT_CELL_SLOT.load(Ordering::Relaxed),
        CORRUPT_CELL_RAW0.load(Ordering::Relaxed),
        CORRUPT_CELL_RAW1.load(Ordering::Relaxed),
    )
}

pub(crate) unsafe fn read_value_cell_checked(ptr: *const Value, site: &'static str) -> Value {
    match cratonvm_types::read_value_checked_atomic(ptr) {
        Some(v) => v,
        None => {
            let n = CORRUPT_CELL_HITS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: caller contract - 16 readable, 8-byte-aligned bytes.
            CORRUPT_CELL_SLOT.store(ptr as usize, Ordering::Relaxed);
            CORRUPT_CELL_RAW0.store(
                (*(ptr as *const std::sync::atomic::AtomicU64)).load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            CORRUPT_CELL_RAW1.store(
                (*((ptr as *const u8).add(8) as *const std::sync::atomic::AtomicU64))
                    .load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            if n < 32 || gc_flags().diag_hib32 {
                // SAFETY: caller contract — 16 readable, 8-byte-aligned bytes.
                // Read atomically per word so the diagnostic itself cannot tear
                // against a concurrent plain writer (PLAIN-SLOT TEARING FIX).
                let raw0 = (*(ptr as *const std::sync::atomic::AtomicU64)).load(Ordering::Relaxed);
                let raw1 = (*((ptr as *const u8).add(8) as *const std::sync::atomic::AtomicU64))
                    .load(Ordering::Relaxed);
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    slot = ?ptr,
                    raw0 = format!("{raw0:#018x}"),
                    raw1 = format!("{raw1:#018x}"),
                    "{site}: corrupt Value cell (out-of-range discriminant) — \
                     returning null instead of a UB-on-match Value. Heap \
                     reference-integrity defect (see HIB-CV-32).",
                );
            }
            Value::Object(None)
        }
    }
}

/// Write a `Value` into a slot.
///
/// See [`read_slot`]'s PLAIN-SLOT TEARING FIX note — delegates to the same
/// already-proven `write_value_atomic` helper so a concurrent plain reader
/// on another mutator thread can never observe a torn (tag, payload) pair.
///
/// # Safety
/// The pointer must be valid and 8-byte aligned.
unsafe fn write_slot(ptr: *mut u8, value: Value) {
    cratonvm_types::write_value_atomic(ptr as *mut Value, value);
}

// ---------------------------------------------------------------------------
// Compact primitive array element access
// ---------------------------------------------------------------------------

// --- Reference-element decode chokepoint for `read_prim_element` ------------
//
// WHY THIS EXISTS. `read_prim_element`'s `Reference` arm ended commit
// `6a04b0e3c1` (2026-06-29, "degrade stale references to null at every decode
// boundary") applying `cratonvm_types::plausible_heap_pointer` to the loaded
// word and, on failure, returning `Value::Object(None)` — handing Java a `null`
// where the slot held bits. That commit's own message records what the filter
// is: defense-in-depth for an *unfixed* GC defect (live blocked-thread frame
// objects swept by the non-moving young sweep; observed as 8000+ all-zero-header
// stale `Thread` receivers, then `0x77..` / `": contex"` buffer bytes read back
// through this arm), not an integrity guard on trusted data. Two failure modes
// hid behind the bare `else`:
//
// (1) THE DEGRADE WAS SILENT — and its interpreter twin is not. The same commit
//     put the same filter on `cratonvm_types`' own decode paths (`decode_value`
//     `VTAG_OBJECT`, `CompactValue::to_value` SUB_OBJECT), where it feeds a
//     process-wide counter and a one-shot stderr line (`note_object_degradation`,
//     types/src/compact_value.rs:330, read back via the `pub`
//     `cratonvm_types::compact_value::object_degradation_count`). THIS arm fed
//     nothing at all. `read_prim_element` is the array-element read for EVERY
//     collector — `Heap::get_array_element` / `get_array_element_unboxing` above,
//     `GenerationalHeap`'s twins (gc/src/gen_heap.rs:4072, :4092, :3518) and
//     `zgc.rs:2601` all funnel through it — so on the DEFAULT (Generational)
//     collector a live `Object[]` element could be nulled with no trace
//     anywhere: no counter, no log, no assert, and `object_degradation_count()`
//     reading a reassuring 0. That is an observability bug today, independent of
//     ZGC, and [`ref_element_degradation_count`] closes it.
//
//     NULL IS NOT A DEGRADATION. `plausible_heap_pointer(0)` is `false`, so an
//     ordinary null element — by far the common case for `Object[]` — reaches
//     the cold arm too. Counting it would put the counter in the millions on a
//     clean run and make "non-zero means a live object was nulled" false on
//     first use. That test is load-bearing, not a micro-optimisation, and it
//     now lives in the shared sink
//     (`cratonvm_types::compact_value::note_ref_word_degradation`) rather than
//     in this file — see [`ref_element_degradation_count`].
//
// (2) A ZGC COLORED WORD IS NOT CORRUPTION, and must never take the degrade.
//     `gc/src/zgc/vaddr.rs` sets bit 63 (`Z_COLORED_TAG`) on every non-null
//     colored word *precisely so* it fails `plausible_heap_pointer`'s 47-bit
//     test and is caught loudly rather than dereferenced as a wild pointer. The
//     bare `else` did the exact opposite of loud: it converted "the load barrier
//     has not run on this word" into a null handed to Java, i.e. an NPE or a
//     silently dropped store at an arbitrary point far from the cause. Under a
//     relocating collector that is silent heap corruption, which
//     `docs/feature-designs/zgc-jit-load-barrier.md` (risk J1) rates worse than
//     a clean SIGSEGV; `docs/feature-designs/zgc-reference-slot-representation.md`
//     names this arm as the most dangerous unmigrated read in the tree, and
//     `gc/src/zgc/census.rs` reads raw words rather than call it for exactly
//     this reason.
//
//     The fix is NOT to weaken `plausible_heap_pointer` — both studies say so
//     explicitly — it is to make the caller barrier the word first. Until that
//     lands, a *structurally well-formed* colored word is an invariant violation
//     and fails loudly instead of fabricating a null. Genuine garbage keeps the
//     old degrade: `0x8D8D8D8D8D8D8D8D` has bit 63 set but fails
//     `vaddr::is_well_formed` (which additionally demands bits 62-46 clear and
//     exactly one metadata bit), so a `--features zgc` build running
//     Generational or G1 behaves as before.

/// Read the process-wide count of **non-null** array reference elements
/// [`read_prim_element`] degraded to `null` because they failed
/// [`cratonvm_types::plausible_heap_pointer`].
///
/// The read-side companion to [`COMPACT_OOP_MAP_MISSING`]: that one is a
/// marking FAIL-OPEN, this one is a read FAIL-SILENT. Non-zero means Java was
/// handed `null` for a slot that held bits — i.e. the GC root-coverage gap
/// recorded in commit `6a04b0e3c1` is live in this run. Expected to be ZERO;
/// zero is the only good value.
///
/// # This is one slot of the shared counter, not a private one
///
/// It used to be a `pub static REF_ELEMENT_DEGRADATIONS: AtomicU64` in this
/// file, because the sink it belonged in
/// (`cratonvm_types::compact_value::note_object_degradation`) was `pub(crate)`
/// and this crate could not reach it. That produced the exact failure the
/// counter exists to prevent: a triager reading
/// `cratonvm_types::compact_value::object_degradation_count()` saw a reassuring
/// `0` while this path was nulling live elements, because the total did not
/// include them. The sink is now `pub` and source-tagged, so this reads
/// [`DegradationSource::ArrayElement`]'s slot of the one process-wide table and
/// the total genuinely totals. `vm::jit::helpers::jit_ref_degradation_count`
/// (the `Jit` slot) was merged the same way.
///
/// Advisory and `Relaxed`: it carries no happens-before relationship with the
/// slot it counts.
#[inline]
pub fn ref_element_degradation_count() -> u64 {
    cratonvm_types::compact_value::object_degradation_count_from(
        cratonvm_types::compact_value::DegradationSource::ArrayElement,
    )
}

/// Decode a raw reference word read out of an array element slot into the
/// `Value` the interpreter expects, degrading a provably-impossible pointer to
/// `Value::Object(None)`.
///
/// The fast path is byte-for-byte the predicate the `Reference` arm used before
/// this chokepoint existed: `plausible_heap_pointer(raw)` and nothing else.
/// Everything new lives in the `#[cold]`, `#[inline(never)]` callee, which is
/// only reached once that predicate has *already* failed — so this costs
/// nothing per element read on any build or any collector. See the block
/// comment above for why the callee exists.
///
/// # Safety
/// Nothing beyond `ObjectRef::from_raw`'s contract, and `raw` has passed
/// [`cratonvm_types::plausible_heap_pointer`] before it is wrapped.
#[inline(always)]
unsafe fn decode_ref_element_word(raw: u64) -> Value {
    if cratonvm_types::plausible_heap_pointer(raw) {
        Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
    } else {
        ref_element_word_implausible(raw)
    }
}

/// Cold arm of [`decode_ref_element_word`]: the word cannot be a live heap
/// pointer. Separates the three reasons a word lands here, which the previous
/// bare `else { Value::Object(None) }` conflated into one silent answer:
///
/// * **`raw == 0`** — an ordinary null element. Not a degradation, not counted;
///   the slot said null and the caller gets null. See the block comment above:
///   counting this would destroy the counter's meaning on the first clean run.
///   The test is no longer written here: it lives inside
///   [`cratonvm_types::compact_value::note_ref_word_degradation`], which exists
///   because this arm and the JIT's twin each had to discover the rule
///   separately. Stated once, where the next raw-word path cannot miss it.
/// * **A structurally well-formed ZGC colored word** — legitimate data that has
///   simply not been through the load barrier. Nulling it is the silent-null
///   corruption of `zgc-jit-load-barrier.md` J1; returning the colored word is a
///   wild-pointer deref. Neither is acceptable, so this is a hard failure that
///   names the missing barrier.
/// * **Stale/garbage bits** (the `0x8D8D..` class from commit `6a04b0e3c1`) —
///   genuinely not a pointer. Keeps the existing degrade-to-null contract,
///   now counted.
///
/// The ZGC arm is `#[cfg(feature = "zgc")]`, so a default build does not merely
/// behave identically — the branch is not compiled. `crate::zgc` is itself
/// `#[cfg(feature = "zgc")]` in `gc/src/lib.rs`, so the path is only nameable
/// under that cfg.
#[cold]
#[inline(never)]
fn ref_element_word_implausible(raw: u64) -> Value {
    #[cfg(feature = "zgc")]
    {
        // TODO(zgc): once the load barrier runs AHEAD of this decode (see the
        // TODO in `read_prim_element`'s `Reference` arm) this branch becomes
        // unreachable, because the word arriving here will already be a plain
        // address. The barrier entry point is
        // `crate::zgc::barrier::z_load(slot: &std::sync::atomic::AtomicU64,
        // ctx: &C) -> u64` where `C: crate::zgc::barrier::ZBarrierContext +
        // ?Sized` (gc/src/zgc/barrier.rs, `pub fn z_load`, line 1233 as of
        // 2026-08-07). It must be applied to the element SLOT before any
        // plausibility test, and the test must then ask about the barrier's
        // UNMASKED address, never about the colored word. Do not weaken
        // `plausible_heap_pointer` to admit colored words. Keep this panic as
        // the tripwire for a caller that was missed.
        if crate::zgc::vaddr::is_colored_word(raw) && crate::zgc::vaddr::is_well_formed(raw) {
            panic!(
                "ZGC colored word {raw:#018x} reached `read_prim_element`'s \
                 Reference arm with no load barrier: bit 63 (Z_COLORED_TAG) is \
                 set by gc/src/zgc/vaddr.rs, so this word is a legitimate \
                 reference that has not been unmasked, not corruption. Barrier \
                 it via crate::zgc::barrier::z_load and plausibility-check the \
                 UNMASKED address; do not degrade it to null (that is the silent \
                 heap corruption of zgc-jit-load-barrier.md J1) and do not weaken \
                 plausible_heap_pointer to admit it."
            );
        }
    }
    // Counts the event and emits the one-shot "array-element" diagnostic, or
    // returns `false` and does neither for `raw == 0`. See
    // `ref_element_degradation_count` for why this is the shared table's
    // `ArrayElement` slot rather than a static in this file.
    cratonvm_types::compact_value::note_ref_word_degradation(
        raw,
        cratonvm_types::compact_value::DegradationSource::ArrayElement,
        "gc::heap::read_prim_element",
    );
    Value::Object(None)
}

/// Read an array element from compact storage.
///
/// # Safety
/// `base` must point to the start of the array data area (header + HEADER_SIZE).
/// `index` must have been validated against the array length by the caller.
#[inline]
pub unsafe fn read_prim_element(base: *mut u8, index: usize, et: ArrayElementType) -> Value {
    // Use a helper to compute byte offset with overflow checking.
    // Panics in debug builds, wraps in release — the caller MUST bounds-check
    // `index` against the array length before calling this function.
    macro_rules! elem_ptr {
        ($base:expr, $index:expr, $stride:expr, $ty:ty) => {{
            let offset = $index
                .checked_mul($stride)
                .expect("array element offset overflow");
            std::ptr::read_unaligned($base.add(offset) as *const $ty)
        }};
    }

    match et {
        ArrayElementType::Int => Value::Int(elem_ptr!(base, index, 4, i32)),
        ArrayElementType::Long => Value::Long(elem_ptr!(base, index, 8, i64)),
        ArrayElementType::Float => Value::Float(elem_ptr!(base, index, 4, f32)),
        ArrayElementType::Double => Value::Double(elem_ptr!(base, index, 8, f64)),
        ArrayElementType::Byte => {
            let v = std::ptr::read(base.add(index));
            Value::Int(v as i8 as i32) // sign-extend
        }
        ArrayElementType::Boolean => {
            let v = std::ptr::read(base.add(index));
            Value::Int(v as i32) // zero-extend (0 or 1)
        }
        ArrayElementType::Char => Value::Int(elem_ptr!(base, index, 2, u16) as i32),
        ArrayElementType::Short => Value::Int(elem_ptr!(base, index, 2, i16) as i32),
        ArrayElementType::Reference => {
            let offset = index
                .checked_mul(ref_element_size())
                .expect("array ref element offset overflow");
            // TODO(zgc): this is a raw reference-array element load — Category A
            // in `docs/feature-designs/zgc-jit-load-barrier.md` §2.3, and entry
            // 13 of `zgc-reference-slot-representation.md`'s migration table.
            // Under `VmHeap::Zgc` the word must go through
            // `crate::zgc::barrier::z_load(slot: &std::sync::atomic::AtomicU64,
            // ctx: &C)` (gc/src/zgc/barrier.rs, `pub fn z_load`, line 1233 as of
            // 2026-08-07) HERE — between `read_ref_slot` and the decode below —
            // and the plausibility test inside `decode_ref_element_word` must
            // then be applied to the barrier's unmasked address rather than to
            // the colored word. Until that lands,
            // `ref_element_word_implausible` fails loudly on a well-formed
            // colored word instead of nulling it.
            let raw: u64 = read_ref_slot(base.add(offset));
            // Defense-in-depth reference-slot decode. Mirrors the VTAG_OBJECT
            // degrade in `cratonvm_types::decode_value` (operand/local SoA path)
            // and `CompactValue::to_value`'s SUB_OBJECT plausibility gate, which
            // this hot heap-read path previously bypassed by wrapping ANY
            // non-zero bits verbatim. A non-null reference slot whose bits are
            // unaligned, inside the null-guard page, or outside the 47-bit
            // user-address range is provably NOT a live object pointer.
            // Fabricating an `ObjectRef` from such bits SIGSEGVs on its next
            // deref, or panics in `CompactValue::object` for >47-bit bits.
            // Degrade to null instead — the field/array read callers already
            // normalise `Object(None)`, matching HotSpot's "you get a null, not
            // a VM crash". Pure bit ops (no heap probe), so it is safe on this
            // hot path and never rejects a valid pointer (every real object is
            // 8-aligned, above the guard page, <=47-bit). The degrade is now
            // COUNTED rather than silent — see the chokepoint block comment
            // above `ref_element_degradation_count` for the failure that closes.
            decode_ref_element_word(raw)
        }
    }
}

/// gcstress residual face-1 hunt (`CRATONVM_DBG_WATCH_CELL=<hex addr>`) — a
/// software write-watch over the heap write primitives. Every instrumented
/// writer calls [`cell_watch_check`] with its destination range; a range that
/// covers the watched address prints the site, the range, and a backtrace.
/// Unlike a DR hardware watchpoint this covers BULK copies (range overlap)
/// and needs no per-thread arming. `0` = disabled (single cached load + cmp
/// on the hot paths).
#[inline]
pub fn cell_watch_addr() -> usize {
    crate::gc_flags().dbg_watch_cell
}

/// ES-FAIL-FAMILY-20260710 hunt: a RUNTIME-settable companion to
/// [`cell_watch_addr`] (which only reads a fixed address from the
/// `CRATONVM_DBG_WATCH_CELL` env var at process start). Some hunts don't know
/// the address to watch until the program has already run for a while (e.g.
/// "watch the `cause` slot of the next `BufferUnderflowException` this
/// constructs" — the address is only known once that object is allocated).
/// `set_dynamic_watch` lets native code (via a `NativeContext` hook) arm the
/// watch mid-run; [`cell_watch_check`] checks both addresses. `0` = disabled.
static DYNAMIC_WATCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// See [`DYNAMIC_WATCH`].
#[inline]
pub fn set_dynamic_watch(addr: usize) {
    DYNAMIC_WATCH.store(addr, std::sync::atomic::Ordering::SeqCst);
}

/// `CRATONVM_DBG_MARK_WHY_CLASS=<internal/class/Name>` — the address whose
/// young-mark REASON should be reported. Armed at runtime.
///
/// "Why is this object still alive after a collection that should have
/// reclaimed it" is not answerable from outside the marker. Every root source
/// can be eliminated one at a time — the `TestDefaultInstanceManager` chain has
/// now done that four times — and still leave the question open, because the
/// retaining edge may be a SIDE TABLE (`loader_pin`, `mirror_pin`,
/// `metadata_pin`, an overlay owner edge). Those are invisible to a referrer
/// walk, absent from the root vector, and followed only inside the marker.
///
/// The old-gen BFS already labels each such edge (`mark_and_push_old_gen`'s
/// `reason`). The young precise marker did not — so on the `System.gc()` path,
/// which is exactly the non-moving young sweep, nothing recorded WHICH edge did
/// the marking. That asymmetry is why the question kept being answered by
/// elimination instead of by evidence.
///
/// Armed from `vm_object`'s `add_mirror_pin` hook: the interesting address (a
/// JSP `ClassLoader`) is not known until its class is defined. Snapshotted into
/// `YoungMarkCtx` once per collection, so the per-edge cost is a compare
/// against a struct field, not an atomic load. `0` = disabled.
static YOUNG_MARK_WATCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// See [`YOUNG_MARK_WATCH`].
#[inline]
pub fn set_young_mark_watch(addr: usize) {
    YOUNG_MARK_WATCH.store(addr, std::sync::atomic::Ordering::SeqCst);
}

/// See [`YOUNG_MARK_WATCH`].
#[inline]
pub fn young_mark_watch() -> usize {
    YOUNG_MARK_WATCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// See [`DYNAMIC_WATCH`].
#[inline]
pub fn dynamic_watch_addr() -> usize {
    DYNAMIC_WATCH.load(std::sync::atomic::Ordering::SeqCst)
}

/// See [`cell_watch_addr`]. `extra` carries the value/context being written.
#[inline]
pub fn cell_watch_check(dst: usize, len: usize, site: &str, extra: &dyn std::fmt::Debug) {
    let env_w = cell_watch_addr();
    let dyn_w = dynamic_watch_addr();
    for w in [env_w, dyn_w] {
        if w != 0 && dst <= w && w.wrapping_sub(dst) < len {
            eprintln!(
                "[CELLWATCH] {site}: write [{dst:#x} +{len}) covers watch {w:#x} value={extra:?}\n{}",
                std::backtrace::Backtrace::force_capture(),
            );
        }
    }
}

/// Write an array element to compact storage.
///
/// # Safety
/// `base` must point to the start of the array data area (header + HEADER_SIZE).
#[inline]
pub unsafe fn write_prim_element(base: *mut u8, index: usize, et: ArrayElementType, value: Value) {
    // gcstress face-1 hunt (no-op unless CRATONVM_DBG_WATCH_CELL is set).
    if cell_watch_addr() != 0 {
        let width: usize = match et {
            ArrayElementType::Byte | ArrayElementType::Boolean => 1,
            ArrayElementType::Char | ArrayElementType::Short => 2,
            ArrayElementType::Int | ArrayElementType::Float => 4,
            _ => 8,
        };
        cell_watch_check(
            base as usize + index * width,
            width,
            "write_prim_element",
            &value,
        );
    }
    match et {
        ArrayElementType::Int => {
            let v = match value {
                Value::Int(i) => i,
                _ => 0,
            };
            std::ptr::write_unaligned(base.add(index * 4) as *mut i32, v);
        }
        ArrayElementType::Long => {
            let v = match value {
                Value::Long(l) => l,
                _ => 0,
            };
            std::ptr::write_unaligned(base.add(index * 8) as *mut i64, v);
        }
        ArrayElementType::Float => {
            let v = match value {
                Value::Float(f) => f,
                _ => 0.0,
            };
            std::ptr::write_unaligned(base.add(index * 4) as *mut f32, v);
        }
        ArrayElementType::Double => {
            let v = match value {
                Value::Double(d) => d,
                _ => 0.0,
            };
            std::ptr::write_unaligned(base.add(index * 8) as *mut f64, v);
        }
        ArrayElementType::Byte => {
            let v = match value {
                Value::Int(i) => i as u8,
                _ => 0,
            };
            std::ptr::write(base.add(index), v);
        }
        // JVMS `bastore`: a store into a *boolean* array narrows to one bit
        // (`value & 1`), not to a byte — the opcode serves `byte[]` and
        // `boolean[]` both, and the verifier permits either. `read_prim_element`
        // zero-extends whatever is here, so a truncated 2 reads back as 2 and
        // tests `true`, where HotSpot stores 0 and tests `false`. Reachable via
        // hand-written `bastore` on a `boolean[]` and via
        // `Unsafe.putByte`/`putBoolean`; not via javac output.
        ArrayElementType::Boolean => {
            let v = match value {
                Value::Int(i) => (i & 1) as u8,
                _ => 0,
            };
            std::ptr::write(base.add(index), v);
        }
        ArrayElementType::Char => {
            let v = match value {
                Value::Int(i) => i as u16,
                _ => 0,
            };
            std::ptr::write_unaligned(base.add(index * 2) as *mut u16, v);
        }
        ArrayElementType::Short => {
            let v = match value {
                Value::Int(i) => i as i16,
                _ => 0,
            };
            std::ptr::write_unaligned(base.add(index * 2) as *mut i16, v);
        }
        ArrayElementType::Reference => {
            let raw: u64 = match value {
                Value::Object(Some(r)) => r.as_ptr() as u64,
                Value::Object(None) => 0,
                _ => 0,
            };
            write_ref_slot(base.add(index * ref_element_size()), raw);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_size_check() {
        assert_eq!(std::mem::size_of::<ObjectHeader>(), HEADER_SIZE);
    }

    /// `HIB-DCAST-LATEPHASE.1`, mutator side — the fourth accessor family.
    ///
    /// `compact_object_field_storage` answers `None` both for a legacy object
    /// and for a genuinely compact one whose `(class_id, field_count)` no
    /// longer resolves to a registered layout, and `Heap::get_field` /
    /// `Heap::set_field` fell through to the uniform `index * SLOT_SIZE` stride
    /// in *both* cases. On a real instance of the second state `alloc_object`
    /// sized the body with `compact_object_body_size` and `num_slots()` is the
    /// FIELD COUNT, so the `index < num_slots` assert does not bound that
    /// stride: the read escapes the allocation and the write puts a 16-byte
    /// `Value` cell past it. `g1.rs`, `gen_heap.rs` and `zgc.rs` all grew the
    /// `is_compact_object` guard; this one did not.
    #[test]
    fn field_accessors_refuse_a_compact_object_with_no_registered_layout() {
        let heap = Heap::new();
        // No layout is registered for this class id, so the allocation is
        // LEGACY and the cells written below are real, readable cells. Setting
        // the header bit afterwards reproduces the racing state (header says
        // compact, registry cannot serve it) without a live redefinition.
        let obj = heap.alloc_object(ClassId::new(999_997), 4);
        heap.set_field(obj, 0, Value::Int(1));
        heap.set_field(obj, 1, Value::Int(2));

        let header = heap.get_header(obj);
        header.add_gc_flags(cratonvm_types::GC_FLAG_COMPACT);

        assert!(
            matches!(heap.get_field(obj, 0), Value::Object(None)),
            "an unresolvable compact receiver must read as null, not as the \
             legacy 16-byte cell at index * SLOT_SIZE"
        );

        // The write must be dropped, not striped over the legacy cell.
        heap.set_field(obj, 1, Value::Int(77));

        header.clear_gc_flags(cratonvm_types::GC_FLAG_COMPACT);
        assert!(
            matches!(heap.get_field(obj, 1), Value::Int(2)),
            "the dropped write must not have reached the object at all"
        );
    }

    #[test]
    fn value_size_fits_slot() {
        assert!(
            std::mem::size_of::<Value>() <= SLOT_SIZE,
            "Value ({} bytes) exceeds SLOT_SIZE ({SLOT_SIZE} bytes)!",
            std::mem::size_of::<Value>()
        );
    }

    /// Serialises every test that reads `ref_element_degradation_count`
    /// against every test that *increments* it.
    ///
    /// Still sufficient after the merge into `cratonvm_types`' shared
    /// per-source table: that accessor reads only the
    /// `DegradationSource::ArrayElement` slot, and
    /// `ref_element_word_implausible` in this file is that slot's only writer
    /// in the whole tree. A `cratonvm_types` test bumping `Interpreter`, or a
    /// `vm` test bumping `Jit`, is both in a different test binary AND in a
    /// different slot.
    ///
    /// The counter is process-wide and `cargo test` runs this crate's tests in
    /// one process, in parallel — so a test that merely triggers a degradation
    /// perturbs a concurrently-running test that measures one. Same reasoning
    /// (and same mistake, already paid for once) as
    /// `cratonvm_types::compact_value::degrade_counter_test_lock`. Deltas, never
    /// absolute counts, and never a reset: a reset would destroy whatever window
    /// a concurrent observer had already opened.
    fn ref_degrade_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A null reference element is NOT a degradation.
    ///
    /// `plausible_heap_pointer(0)` is `false`, so an ordinary null `Object[]`
    /// element takes the same cold arm as genuine garbage. If that arm counted
    /// it, [`ref_element_degradation_count`] would read in the millions on a
    /// clean run and "non-zero means a live object was nulled" would be false on
    /// first use — the counter would be worse than none at all.
    #[test]
    fn null_ref_element_is_not_counted_as_a_degradation() {
        let _guard = ref_degrade_test_lock();
        let before = ref_element_degradation_count();
        for _ in 0..1000 {
            assert_eq!(
                ref_element_word_implausible(0),
                Value::Object(None),
                "a null element must still decode to null"
            );
        }
        assert_eq!(
            ref_element_degradation_count() - before,
            0,
            "reading null elements must not move the degradation counter"
        );
    }

    /// The degrade stopped being silent.
    ///
    /// Before this counter existed, `read_prim_element` handing Java a `null`
    /// for a slot that held bits left no trace anywhere — no counter, no log, no
    /// assert — while `cratonvm_types`' `object_degradation_count()` reported a
    /// reassuring 0 because it only sees the interpreter's SoA/compact decode
    /// paths, never this one.
    #[test]
    fn implausible_ref_element_degrades_to_null_and_is_counted() {
        let _guard = ref_degrade_test_lock();
        // The `0x8D8D..` class from commit `6a04b0e3c1`, plus an unaligned word
        // and a >47-bit word. None can be a live object pointer.
        let garbage: [u64; 3] = [0x8D8D_8D8D_8D8D_8D8D, 0x1001, 0x0001_0000_0000_0000];
        let before = ref_element_degradation_count();
        for raw in garbage {
            assert!(
                !cratonvm_types::plausible_heap_pointer(raw),
                "{raw:#018x} must fail the plausibility test for this test to mean anything"
            );
            assert_eq!(
                ref_element_word_implausible(raw),
                Value::Object(None),
                "garbage bits must still degrade to null, not fabricate an ObjectRef"
            );
        }
        assert_eq!(
            ref_element_degradation_count() - before,
            garbage.len() as u64,
            "every non-null degrade must be counted exactly once"
        );
    }

    /// An array-element degrade must move the SHARED total, not just this
    /// path's own slot.
    ///
    /// This is the assertion the other tests in this file cannot make. They all
    /// measure deltas on `ref_element_degradation_count`, so they passed just as
    /// happily when that read a `static REF_ELEMENT_DEGRADATIONS` private to
    /// this file — and that arrangement was itself the bug: a triager reading
    /// `cratonvm_types::compact_value::object_degradation_count()` (the name the
    /// interpreter's twin publishes, and the one a crash report reaches for) saw
    /// `0` while this path was nulling live elements. Re-privatising the counter
    /// must fail a test, not merely go unnoticed.
    ///
    /// Deltas on the total are asserted as a LOWER bound, deliberately. The
    /// total sums all three sources and `ref_degrade_test_lock` only serialises
    /// this one, so a concurrently-running test in this binary that trips a
    /// `cratonvm_types` `Interpreter` decode may add to it. `>= n` still fails
    /// closed for the regression this guards (a private counter moves the total
    /// by 0 while the slot moves by `n`), and does not flake.
    #[test]
    fn an_array_element_degrade_is_visible_in_the_shared_process_wide_total() {
        use cratonvm_types::compact_value::{
            object_degradation_breakdown, object_degradation_count, DegradationSource,
        };
        let _guard = ref_degrade_test_lock();
        let garbage: [u64; 3] = [0x8D8D_8D8D_8D8D_8D8D, 0x1001, 0x0001_0000_0000_0000];
        let n = garbage.len() as u64;

        let before_slot = ref_element_degradation_count();
        let before_total = object_degradation_count();
        for raw in garbage {
            assert_eq!(ref_element_word_implausible(raw), Value::Object(None));
        }

        assert_eq!(
            ref_element_degradation_count() - before_slot,
            n,
            "the ArrayElement slot must count every non-null degrade exactly once",
        );
        assert!(
            object_degradation_count() - before_total >= n,
            "an array-element degrade was invisible to object_degradation_count() — \
             this path is back on a counter of its own",
        );
        assert_eq!(
            object_degradation_breakdown()[DegradationSource::ArrayElement.index()],
            ref_element_degradation_count(),
            "ref_element_degradation_count must BE the ArrayElement slot, not a \
             parallel tally that happens to agree",
        );
    }

    /// The fast path is unchanged: a plausible word still decodes verbatim and
    /// costs nothing (it never reaches the cold arm, so it never touches the
    /// counter).
    #[test]
    fn plausible_ref_element_decodes_verbatim_without_counting() {
        let _guard = ref_degrade_test_lock();
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(9), 1);
        let raw: u64 = obj.as_ptr() as usize as u64;
        assert!(cratonvm_types::plausible_heap_pointer(raw));
        let before = ref_element_degradation_count();
        // SAFETY: `raw` is the address of a live object just allocated above.
        let decoded = unsafe { decode_ref_element_word(raw) };
        match decoded {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr(), obj.as_ptr()),
            other => panic!("a live object pointer must decode verbatim, got {other:?}"),
        }
        assert_eq!(
            ref_element_degradation_count() - before,
            0,
            "the fast path must not touch the counter"
        );
    }

    /// A well-formed ZGC colored word is legitimate data that has not been
    /// through the load barrier — nulling it is silent heap corruption
    /// (`zgc-jit-load-barrier.md` J1), so the cold arm must fail loudly instead.
    #[cfg(feature = "zgc")]
    #[test]
    #[should_panic(expected = "with no load barrier")]
    fn well_formed_colored_word_panics_instead_of_degrading() {
        use crate::zgc::vaddr;
        // Tagged (bit 63), reserved bits 62-46 clear, exactly one metadata bit.
        let colored: u64 = vaddr::Z_COLORED_TAG | vaddr::Z_MARKED0 | 0x40;
        assert!(vaddr::is_well_formed(colored));
        assert!(!cratonvm_types::plausible_heap_pointer(colored));
        let _ = ref_element_word_implausible(colored);
    }

    /// ...but garbage that merely happens to have bit 63 set is NOT a colored
    /// word, and must keep the old degrade even in a `--features zgc` build
    /// running Generational or G1. `0x8D8D..` sets bit 63 and fails
    /// `is_well_formed` (reserved bits set, several metadata bits set).
    #[cfg(feature = "zgc")]
    #[test]
    fn stale_garbage_with_bit63_still_degrades_under_the_zgc_feature() {
        let _guard = ref_degrade_test_lock();
        let raw: u64 = 0x8D8D_8D8D_8D8D_8D8D;
        assert!(crate::zgc::vaddr::is_colored_word(raw));
        assert!(
            !crate::zgc::vaddr::is_well_formed(raw),
            "if this ever becomes well-formed the panic arm would fire on real garbage"
        );
        let before = ref_element_degradation_count();
        assert_eq!(ref_element_word_implausible(raw), Value::Object(None));
        assert_eq!(ref_element_degradation_count() - before, 1);
    }

    #[test]
    fn alloc_object_and_read_header() {
        let heap = Heap::new();
        let class_id = ClassId::new(42);
        let obj = heap.alloc_object(class_id, 3);

        let header = heap.get_header(obj);
        assert_eq!(header.class_id, class_id);
        assert_eq!(header.kind(), ObjectKind::Object);
        assert_eq!(header.num_slots(), 3);

        // The identity hash is installed LAZILY, on first request, into the
        // mark word -- not eagerly at allocation as it used to be.
        //
        // That change is not cosmetic. Minting at allocation would leave every
        // object's mark word non-zero, and `try_thin_lock` CASes from the
        // literal `MARK_NEUTRAL` -- so every `synchronized` block in the
        // program would lose that CAS and inflate a `Monitor`. Eager hashing
        // and a mark-word hash cannot coexist.
        assert_eq!(
            ObjectHeader::neutral_hash(header.mark_word.load(Ordering::Relaxed)),
            0,
            "a freshly allocated object must not carry a hash yet, or nothing              can ever thin-lock"
        );
        let hash = heap.identity_hash_code(obj);
        assert_ne!(hash, 0);
        assert_eq!(heap.identity_hash_code(obj), hash, "must be stable");
    }

    /// `alloc_object` allocates through `alloc_zeroed`, and an all-zero slot
    /// decodes as `Value::Int(0)` (the discriminant-0 variant of `Value`; see
    /// `alloc_object_with_descriptors`, which exists precisely because that is
    /// NOT `Object(None)`). Every field of a fresh object must read back as
    /// that zero, including one carved out of arena bytes a previous object
    /// has already written to.
    ///
    /// Was vacuous: three `let _ = heap.get_field(obj, n);` and "just verify no
    /// crash". Swapping `alloc_zeroed` for a non-zeroing bump in
    /// `alloc_object`, so a recycled/dirty slot is handed back as-is, stayed
    /// green.
    #[test]
    fn alloc_object_fields_zero_initialized() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 3);

        for i in 0..3 {
            assert_eq!(
                heap.get_field(obj, i),
                Value::Int(0),
                "field {i} of a fresh object must read as the zeroed slot"
            );
        }

        // Dirty this object, then allocate another: the new one's slots must
        // still be zero rather than whatever the arena last held.
        heap.set_field(obj, 0, Value::Long(-1));
        heap.set_field(obj, 1, Value::Double(1.5));
        heap.set_field(obj, 2, Value::Object(Some(obj)));
        let obj2 = heap.alloc_object(ClassId::new(0), 3);
        for i in 0..3 {
            assert_eq!(
                heap.get_field(obj2, i),
                Value::Int(0),
                "field {i} of a later object must be zeroed too"
            );
        }
    }

    #[test]
    fn object_field_set_and_get() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 3);

        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(123_456_789));
        heap.set_field(obj, 2, Value::Float(3.125));

        assert_eq!(heap.get_field(obj, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(obj, 1).as_long(), Some(123_456_789));
        assert_eq!(heap.get_field(obj, 2).as_float(), Some(3.125));
    }

    #[test]
    fn object_field_overwrite() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 1);

        heap.set_field(obj, 0, Value::Int(1));
        assert_eq!(heap.get_field(obj, 0).as_int(), Some(1));

        heap.set_field(obj, 0, Value::Int(999));
        assert_eq!(heap.get_field(obj, 0).as_int(), Some(999));
    }

    #[test]
    fn object_reference_field() {
        let heap = Heap::new();
        let obj1 = heap.alloc_object(ClassId::new(0), 1);
        let obj2 = heap.alloc_object(ClassId::new(1), 0);

        // Store a reference to obj2 in obj1's field
        heap.set_field(obj1, 0, Value::Object(Some(obj2)));

        match heap.get_field(obj1, 0) {
            Value::Object(Some(r)) => assert_eq!(r, obj2),
            other => panic!("Expected Object(Some(...)), got {other:?}"),
        }

        // Null reference
        heap.set_field(obj1, 0, Value::Object(None));
        assert!(heap.get_field(obj1, 0).is_null());
    }

    #[test]
    fn alloc_array_and_read_header() {
        let heap = Heap::new();
        let class_id = ClassId::new(10);
        let arr = heap.alloc_array(class_id, ArrayElementType::Int, 5);

        let header = heap.get_header(arr);
        assert_eq!(header.class_id, class_id);
        assert_eq!(header.kind(), ObjectKind::Array);
        assert_eq!(header.element_type(), ArrayElementType::Int);
        assert_eq!(header.array_length(), 5);
        assert_eq!(heap.array_length(arr), 5);
    }

    #[test]
    fn array_set_and_get() {
        let heap = Heap::new();
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);

        heap.set_array_element(arr, 0, Value::Int(10)).unwrap();
        heap.set_array_element(arr, 1, Value::Int(20)).unwrap();
        heap.set_array_element(arr, 2, Value::Int(30)).unwrap();

        assert_eq!(heap.get_array_element(arr, 0).unwrap().as_int(), Some(10));
        assert_eq!(heap.get_array_element(arr, 1).unwrap().as_int(), Some(20));
        assert_eq!(heap.get_array_element(arr, 2).unwrap().as_int(), Some(30));
    }

    #[test]
    fn array_bounds_check() {
        let heap = Heap::new();
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);

        // In-bounds
        assert!(heap.set_array_element(arr, 0, Value::Int(1)).is_ok());
        assert!(heap.set_array_element(arr, 1, Value::Int(2)).is_ok());

        // Out of bounds
        assert_eq!(heap.set_array_element(arr, 2, Value::Int(3)), Err(2));
        assert_eq!(heap.get_array_element(arr, 5), Err(5));
    }

    #[test]
    fn reference_array() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let arr = heap.alloc_array(ClassId::new(1), ArrayElementType::Reference, 2);

        heap.set_array_element(arr, 0, Value::Object(Some(obj)))
            .unwrap();
        heap.set_array_element(arr, 1, Value::Object(None)).unwrap();

        match heap.get_array_element(arr, 0).unwrap() {
            Value::Object(Some(r)) => assert_eq!(r, obj),
            other => panic!("Expected Object(Some), got {other:?}"),
        }
        assert!(heap.get_array_element(arr, 1).unwrap().is_null());
    }

    #[test]
    fn multiple_objects_independent() {
        let heap = Heap::new();
        let obj1 = heap.alloc_object(ClassId::new(0), 1);
        let obj2 = heap.alloc_object(ClassId::new(1), 1);

        heap.set_field(obj1, 0, Value::Int(111));
        heap.set_field(obj2, 0, Value::Int(222));

        // They don't interfere with each other
        assert_eq!(heap.get_field(obj1, 0).as_int(), Some(111));
        assert_eq!(heap.get_field(obj2, 0).as_int(), Some(222));
    }

    #[test]
    fn identity_hash_codes_unique() {
        let heap = Heap::new();
        let obj1 = heap.alloc_object(ClassId::new(0), 0);
        let obj2 = heap.alloc_object(ClassId::new(0), 0);

        assert_ne!(heap.identity_hash_code(obj1), heap.identity_hash_code(obj2),);
    }

    #[test]
    fn allocated_bytes_grows() {
        let heap = Heap::new();
        let before = heap.allocated_bytes();
        let _ = heap.alloc_object(ClassId::new(0), 100);
        assert!(heap.allocated_bytes() > before);
    }

    #[test]
    fn zero_length_array() {
        let heap = Heap::new();
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
        assert_eq!(heap.array_length(arr), 0);
        // Out of bounds on any index
        assert!(heap.get_array_element(arr, 0).is_err());
    }

    // -- Tests for integer overflow protection --

    #[test]
    fn array_data_size_checked_small_values() {
        assert_eq!(
            array_data_size_checked(10, ArrayElementType::Int),
            Some(40) // 10 * 4 = 40, already 8-aligned
        );
        assert_eq!(
            array_data_size_checked(3, ArrayElementType::Long),
            Some(24) // 3 * 8 = 24, already 8-aligned
        );
        assert_eq!(
            array_data_size_checked(5, ArrayElementType::Byte),
            Some(8) // 5 * 1 = 5, rounded up to 8
        );
    }

    #[test]
    fn array_data_size_checked_overflow_returns_none() {
        // usize::MAX / 8 + 1 elements * 8 bytes each would overflow
        let huge = usize::MAX / 8 + 1;
        assert_eq!(array_data_size_checked(huge, ArrayElementType::Long), None);
    }

    #[test]
    fn array_data_size_returns_err_on_overflow() {
        let huge = usize::MAX / 8 + 1;
        assert!(array_data_size(huge, ArrayElementType::Long).is_err());
    }

    #[test]
    #[should_panic(expected = "overflow")]
    fn alloc_object_panics_on_field_count_overflow() {
        let heap = Heap::new();
        // This should trigger checked_mul overflow
        let _ = heap.alloc_object(ClassId::new(0), usize::MAX);
    }

    #[test]
    fn array_data_size_zero_length() {
        assert_eq!(array_data_size(0, ArrayElementType::Int).unwrap(), 0);
        assert_eq!(array_data_size(0, ArrayElementType::Reference).unwrap(), 0);
    }

    #[test]
    fn element_byte_sizes() {
        assert_eq!(element_byte_size(ArrayElementType::Boolean), 1);
        assert_eq!(element_byte_size(ArrayElementType::Byte), 1);
        assert_eq!(element_byte_size(ArrayElementType::Char), 2);
        assert_eq!(element_byte_size(ArrayElementType::Short), 2);
        assert_eq!(element_byte_size(ArrayElementType::Int), 4);
        assert_eq!(element_byte_size(ArrayElementType::Float), 4);
        assert_eq!(element_byte_size(ArrayElementType::Long), 8);
        assert_eq!(element_byte_size(ArrayElementType::Double), 8);
        assert_eq!(
            element_byte_size(ArrayElementType::Reference),
            REF_ELEMENT_SIZE
        );
    }

    #[test]
    fn alloc_object_checked_success() {
        let heap = Heap::new();
        let obj = heap.alloc_object_checked(ClassId::new(1), 2);
        assert!(obj.is_some());
        let obj = obj.unwrap();
        let header = heap.get_header(obj);
        assert_eq!(header.class_id, ClassId::new(1));
        assert_eq!(header.num_slots(), 2);
    }

    #[test]
    fn alloc_object_checked_overflow_returns_none() {
        let heap = Heap::new();
        // usize::MAX fields will overflow the size calculation
        assert!(heap
            .alloc_object_checked(ClassId::new(0), usize::MAX)
            .is_none());
    }

    #[test]
    fn alloc_array_checked_success() {
        let heap = Heap::new();
        let arr = heap.alloc_array_checked(ClassId::new(1), ArrayElementType::Int, 10);
        assert!(arr.is_some());
        let arr = arr.unwrap();
        assert_eq!(heap.array_length(arr), 10);
    }

    #[test]
    fn alloc_array_checked_overflow_returns_none() {
        let heap = Heap::new();
        assert!(heap
            .alloc_array_checked(ClassId::new(0), ArrayElementType::Int, usize::MAX)
            .is_none());
    }

    #[test]
    fn alloc_array_checked_rejects_oversized() {
        // Arrays larger than MAX_ARRAY_LENGTH (i32::MAX) should be rejected
        let heap = Heap::with_capacity(8 * 1024);
        let result = heap.alloc_array_checked(
            ClassId::new(0),
            ArrayElementType::Int,
            (i32::MAX as usize) + 1,
        );
        assert!(result.is_none());
    }

    #[test]
    fn alloc_array_checked_oom_returns_none() {
        // Create a tiny heap (1 KB total = 512 bytes per semi-space)
        let heap = Heap::with_capacity(1024);
        // Try to allocate an array larger than the semi-space
        let result = heap.alloc_array_checked(ClassId::new(0), ArrayElementType::Long, 1_000_000);
        assert!(result.is_none());
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn test_get_field_out_of_bounds_panics() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        let _ = heap.get_field(obj, 2); // index 2 is out of bounds for 2 fields
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn test_set_field_out_of_bounds_panics() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 5, Value::Int(42));
    }

    #[test]
    #[should_panic(expected = "not an array")]
    fn test_array_length_on_non_array_panics() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 1);
        let _ = heap.array_length(obj);
    }

    #[test]
    #[should_panic(expected = "not an array")]
    fn test_get_array_element_on_non_array_panics() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 1);
        let _ = heap.get_array_element(obj, 0);
    }

    #[test]
    fn test_try_alloc_object_returns_some() {
        let heap = Heap::new();
        let result = heap.try_alloc_object(ClassId::new(7), 3);
        assert!(result.is_some());
        let obj = result.unwrap();
        let header = heap.get_header(obj);
        assert_eq!(header.class_id, ClassId::new(7));
        assert_eq!(header.kind(), ObjectKind::Object);
        assert_eq!(header.num_slots(), 3);
    }

    #[test]
    fn test_try_alloc_object_returns_none_on_oom() {
        let heap = Heap::with_capacity(1024); // 512 bytes per semi-space
                                              // Keep allocating until we get None
        let mut count = 0;
        loop {
            match heap.try_alloc_object(ClassId::new(0), 4) {
                Some(_) => count += 1,
                None => break,
            }
            // Safety limit to avoid infinite loop in case of bug
            assert!(count < 1000, "expected OOM but allocated {count} objects");
        }
        assert!(count > 0, "should have allocated at least one object");
    }

    #[test]
    fn test_try_alloc_array_returns_some() {
        let heap = Heap::new();
        let result = heap.try_alloc_array(ClassId::new(5), ArrayElementType::Int, 10);
        assert!(result.is_some());
        let arr = result.unwrap();
        assert_eq!(heap.array_length(arr), 10);
        let header = heap.get_header(arr);
        assert_eq!(header.class_id, ClassId::new(5));
        assert_eq!(header.kind(), ObjectKind::Array);
        assert_eq!(header.element_type(), ArrayElementType::Int);
    }

    #[test]
    fn test_try_alloc_array_returns_none_on_oom() {
        let heap = Heap::with_capacity(1024); // 512 bytes per semi-space
        let mut count = 0;
        loop {
            match heap.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 8) {
                Some(_) => count += 1,
                None => break,
            }
            assert!(count < 1000, "expected OOM but allocated {count} arrays");
        }
        assert!(count > 0, "should have allocated at least one array");
    }

    #[test]
    fn test_try_alloc_array_rejects_oversized() {
        let heap = Heap::new();
        let result = heap.try_alloc_array(
            ClassId::new(0),
            ArrayElementType::Int,
            (i32::MAX as usize) + 1, // MAX_ARRAY_LENGTH + 1
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_volatile_field_access() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 2);

        heap.set_field_volatile(obj, 0, Value::Int(42));
        heap.set_field_volatile(obj, 1, Value::Long(999_999));

        assert_eq!(heap.get_field_volatile(obj, 0).as_int(), Some(42));
        assert_eq!(heap.get_field_volatile(obj, 1).as_long(), Some(999_999));

        // Overwrite via volatile and read back
        heap.set_field_volatile(obj, 0, Value::Int(100));
        assert_eq!(heap.get_field_volatile(obj, 0).as_int(), Some(100));
    }

    // -------------------------------------------------------------------
    // R1 — Primitive-field default initialization tests.
    //
    // Regression coverage for the ConcurrentHashMap.initTable CAS livelock
    // observed on Keycloak 16/26 boot. Zero-initialized heap memory
    // decodes as `Value::Object(None)` (the zero-discriminant `Value`
    // variant) when read via `std::ptr::read::<Value>()`. For a primitive
    // `int` field that has never been explicitly written, this causes
    // `Unsafe.compareAndSetInt(obj, off, Int(0), new)` to permanently
    // fail because `values_equal_for_cas(Object(None), Int(0))` returns
    // `false` — the Java caller spins forever in its retry loop.
    //
    // The fix is `alloc_object_with_descriptors`: at allocation time we
    // write the correctly-tagged zero `Value` variant into every
    // primitive-typed slot based on the class's field descriptors. For
    // reference slots we leave the zeroed memory alone because it
    // already decodes as `Object(None)` (i.e. `null`), which is the
    // spec-mandated default.
    // -------------------------------------------------------------------

    /// CAS semantics that mirror `vm::vm_exec::values_equal_for_cas`,
    /// reproduced locally so the tests below can exercise the full
    /// alloc→read→CAS→write cycle without pulling in the `vm` crate.
    /// Keep in sync with that definition.
    fn values_equal_for_cas_stub(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Long(x), Value::Long(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
            (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
            (Value::Object(None), Value::Object(None)) => true,
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                std::ptr::eq(a.as_ptr(), b.as_ptr())
            }
            _ => false,
        }
    }

    #[test]
    fn r1_alloc_object_int_field_default_is_int_zero() {
        let heap = Heap::new();
        // Single int field — descriptor "I".
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"I");
        // Without the fix this would read Value::Object(None).
        assert_eq!(
            heap.get_field(obj, 0),
            Value::Int(0),
            "primitive int field must default to Value::Int(0), not Object(None)"
        );
    }

    #[test]
    fn r1_alloc_object_long_field_default_is_long_zero() {
        let heap = Heap::new();
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"J");
        assert_eq!(
            heap.get_field(obj, 0),
            Value::Long(0),
            "primitive long field must default to Value::Long(0)"
        );
    }

    #[test]
    fn r1_alloc_object_float_field_default_is_float_zero() {
        let heap = Heap::new();
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"F");
        match heap.get_field(obj, 0) {
            Value::Float(f) => assert_eq!(
                f.to_bits(),
                0.0_f32.to_bits(),
                "primitive float field must default to 0.0 bits"
            ),
            other => panic!("expected Value::Float(0.0), got {other:?}"),
        }
    }

    #[test]
    fn r1_alloc_object_double_field_default_is_double_zero() {
        let heap = Heap::new();
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"D");
        match heap.get_field(obj, 0) {
            Value::Double(d) => assert_eq!(
                d.to_bits(),
                0.0_f64.to_bits(),
                "primitive double field must default to 0.0 bits"
            ),
            other => panic!("expected Value::Double(0.0), got {other:?}"),
        }
    }

    #[test]
    fn r1_alloc_object_boolean_field_default_is_int_zero() {
        let heap = Heap::new();
        // Z descriptor is stored as Int per JVM spec (boolean lives in
        // Value::Int at the interpreter stack-slot level).
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"Z");
        assert_eq!(
            heap.get_field(obj, 0),
            Value::Int(0),
            "boolean (Z) must default to Value::Int(0), not Object(None)"
        );
    }

    #[test]
    fn r1_alloc_object_reference_field_default_is_object_none() {
        let heap = Heap::new();
        // L-descriptor: reference to java/lang/Object. Descriptor bytes
        // start with 'L' (full form is "Ljava/lang/Object;") — only the
        // first byte matters for default-init dispatch.
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"L");
        assert!(
            heap.get_field(obj, 0).is_null(),
            "reference field must retain the Object(None) zero-bits default"
        );
        // Array descriptor '[' must also default to null.
        let arr_field = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"[");
        assert!(
            heap.get_field(arr_field, 0).is_null(),
            "array-ref field must retain the Object(None) default"
        );
    }

    #[test]
    fn r1_unsafe_compare_and_set_int_on_uninit_slot_succeeds() {
        // This is the concrete KC16/KC26 livelock repro. Before the fix,
        // a freshly-allocated ConcurrentHashMap had its `sizeCtl` slot
        // reading back as Object(None), so CAS(Int(0), Int(N)) never
        // matched and the VM spun at ~5,740 iterations/s.
        let heap = Heap::new();
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"I");

        // Read the slot as the Java caller's `Getfield` would.
        let current = heap.get_field(obj, 0);
        assert_eq!(
            current,
            Value::Int(0),
            "uninit int slot must read as Int(0) for CAS against Int(0)"
        );

        // Simulate Unsafe.compareAndSetInt(obj, off, expected=Int(0), new=Int(42)).
        let expected = Value::Int(0);
        let new_val = Value::Int(42);
        assert!(
            values_equal_for_cas_stub(&current, &expected),
            "CAS compare must succeed on the uninit primitive slot"
        );
        heap.set_field(obj, 0, new_val);

        // Post-CAS, the slot must hold the new value.
        assert_eq!(
            heap.get_field(obj, 0),
            Value::Int(42),
            "post-CAS int slot must read as Int(42)"
        );
    }

    #[test]
    fn r1_unsafe_compare_and_set_long_on_uninit_slot_succeeds() {
        let heap = Heap::new();
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, b"J");

        let current = heap.get_field(obj, 0);
        assert_eq!(
            current,
            Value::Long(0),
            "uninit long slot must read as Long(0)"
        );

        let expected = Value::Long(0);
        let new_val = Value::Long(0xDEAD_BEEF_CAFE_F00D_u64 as i64);
        assert!(
            values_equal_for_cas_stub(&current, &expected),
            "CAS compare must succeed on the uninit primitive long slot"
        );
        heap.set_field(obj, 0, new_val);

        assert_eq!(
            heap.get_field(obj, 0),
            new_val,
            "post-CAS long slot must read the new value"
        );
    }

    // --- Robustness: unknown / malformed descriptor bytes fail open. ---

    #[test]
    fn r1_alloc_object_unknown_descriptor_falls_back_to_object_none() {
        let heap = Heap::new();
        // 0xFF is not a valid JVM field descriptor byte — must fall back
        // to the zeroed default (Object(None)) rather than panic.
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 1, &[0xFF]);
        assert!(
            heap.get_field(obj, 0).is_null(),
            "unknown descriptor must fail-open to Object(None)"
        );
    }

    #[test]
    fn r1_alloc_object_short_descriptor_slice_leaves_extra_slots_zeroed() {
        // num_fields > descriptor_bytes.len() — extra slots keep the
        // zeroed default (Object(None)). This is important because
        // inherited fields may not all have their descriptors passed in
        // a single call.
        let heap = Heap::new();
        let obj = heap.alloc_object_with_descriptors(ClassId::new(0), 3, b"I");
        assert_eq!(heap.get_field(obj, 0), Value::Int(0));
        // Slots 1 and 2 never had their descriptor — they remain as the
        // zero-bits Object(None).
        assert!(heap.get_field(obj, 1).is_null());
        assert!(heap.get_field(obj, 2).is_null());
    }

    #[test]
    fn r1_default_value_for_descriptor_mapping_is_exhaustive() {
        // Every JVM primitive descriptor byte → expected Value variant.
        assert_eq!(default_value_for_descriptor(b'I'), Some(Value::Int(0)));
        assert_eq!(default_value_for_descriptor(b'B'), Some(Value::Int(0)));
        assert_eq!(default_value_for_descriptor(b'C'), Some(Value::Int(0)));
        assert_eq!(default_value_for_descriptor(b'S'), Some(Value::Int(0)));
        assert_eq!(default_value_for_descriptor(b'Z'), Some(Value::Int(0)));
        assert_eq!(default_value_for_descriptor(b'J'), Some(Value::Long(0)));
        match default_value_for_descriptor(b'F') {
            Some(Value::Float(f)) => assert_eq!(f.to_bits(), 0.0_f32.to_bits()),
            other => panic!("F must map to Float(0.0), got {other:?}"),
        }
        match default_value_for_descriptor(b'D') {
            Some(Value::Double(d)) => assert_eq!(d.to_bits(), 0.0_f64.to_bits()),
            other => panic!("D must map to Double(0.0), got {other:?}"),
        }
        // Reference types and unknown bytes map to None → keep zero.
        assert_eq!(default_value_for_descriptor(b'L'), None);
        assert_eq!(default_value_for_descriptor(b'['), None);
        assert_eq!(default_value_for_descriptor(0x00), None);
        assert_eq!(default_value_for_descriptor(0xFF), None);
    }

    // ── T10.9.E descriptor-aware heap-field tests ────────────────────────

    #[test]
    fn t10_9_e_coerce_value_j_from_double_is_long() {
        // Exact Session 93 drift: a long whose bit pattern round-tripped
        // through CompactValue::to_value landed as Value::Double. The
        // helper reinterprets and returns Value::Long.
        let v_double = Value::Double(f64::from_bits(5));
        let coerced = coerce_field_value_by_descriptor(v_double, b'J');
        assert_eq!(coerced, Value::Long(5));
    }

    #[test]
    fn t10_9_e_coerce_value_j_preserves_existing_long() {
        let v = Value::Long(0xDEAD_BEEF_CAFE_BABEu64 as i64);
        let coerced = coerce_field_value_by_descriptor(v, b'J');
        assert_eq!(coerced, Value::Long(0xDEAD_BEEF_CAFE_BABEu64 as i64));
    }

    #[test]
    fn t10_9_e_coerce_value_j_widens_int() {
        let coerced = coerce_field_value_by_descriptor(Value::Int(-7), b'J');
        assert_eq!(coerced, Value::Long(-7));
    }

    #[test]
    fn t10_9_e_coerce_value_j_from_null_is_zero() {
        let coerced = coerce_field_value_by_descriptor(Value::Object(None), b'J');
        assert_eq!(coerced, Value::Long(0));
    }

    #[test]
    fn t10_9_e_coerce_value_d_from_long_is_double() {
        let v_long = Value::Long(f64::to_bits(std::f64::consts::PI) as i64);
        let coerced = coerce_field_value_by_descriptor(v_long, b'D');
        match coerced {
            // Bit equality, not a tolerance. This asserts a REINTERPRETATION:
            // the whole claim is that the 64 bits survive, and a tolerance of
            // 1e-12 admits ~5,100 ulps of drift in a value that cannot legally
            // drift at all — it would pass a slot that silently narrowed the
            // double to f32 and back.
            Value::Double(d) => assert_eq!(d.to_bits(), std::f64::consts::PI.to_bits()),
            other => panic!("expected Double, got {other:?}"),
        }
    }

    #[test]
    fn t10_9_e_coerce_value_f_from_int_reinterprets() {
        let f_bits = 1.5_f32.to_bits();
        let coerced = coerce_field_value_by_descriptor(Value::Int(f_bits as i32), b'F');
        match coerced {
            Value::Float(f) => assert_eq!(f.to_bits(), f_bits),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn t10_9_e_coerce_value_reference_rejects_nonzero_primitive_unchanged() {
        // A reference descriptor leaves most primitives as-is (they will
        // have been rejected by the verifier); only zero-valued primitives
        // normalize to null.
        let coerced = coerce_field_value_by_descriptor(Value::Int(0), b'L');
        assert!(matches!(coerced, Value::Object(None)));
        let coerced = coerce_field_value_by_descriptor(Value::Long(0), b'L');
        assert!(matches!(coerced, Value::Object(None)));
    }

    #[test]
    fn t10_9_e_coerce_value_unknown_desc_preserves_value() {
        let coerced = coerce_field_value_by_descriptor(Value::Int(99), b'V');
        assert_eq!(coerced, Value::Int(99));
    }

    #[test]
    fn t10_9_e_heap_get_field_as_normalizes_j() {
        // Allocate an object and directly write a Value::Double that
        // represents a long bit pattern — simulating upstream drift.
        // `get_field_as(b'J')` must surface Value::Long.
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field(obj, 0, Value::Double(f64::from_bits(12345)));
        match heap.get_field_as(obj, 0, b'J') {
            Value::Long(l) => assert_eq!(l, 12345),
            other => panic!("expected Long(12345), got {other:?}"),
        }
    }

    #[test]
    fn t10_9_e_heap_set_field_as_normalizes_on_write() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 1);
        // Write a Double through the descriptor-aware setter with J descriptor.
        heap.set_field_as(obj, 0, Value::Double(f64::from_bits(42)), b'J');
        // Direct read should already yield Long (normalized on write).
        match heap.get_field(obj, 0) {
            Value::Long(l) => assert_eq!(l, 42),
            other => panic!("expected Long(42), got {other:?}"),
        }
    }

    #[test]
    fn t10_9_e_heap_get_field_volatile_as_normalizes_j() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field_volatile(obj, 0, Value::Double(f64::from_bits(999)));
        match heap.get_field_volatile_as(obj, 0, b'J') {
            Value::Long(l) => assert_eq!(l, 999),
            other => panic!("expected Long(999), got {other:?}"),
        }
    }

    #[test]
    fn t10_9_e_heap_get_field_as_reference_passthrough() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 1);
        let other = heap.alloc_object(ClassId::new(2), 0);
        heap.set_field(obj, 0, Value::Object(Some(other)));
        match heap.get_field_as(obj, 0, b'L') {
            Value::Object(Some(o)) => assert_eq!(o.as_ptr(), other.as_ptr()),
            other => panic!("expected Object(Some), got {other:?}"),
        }
    }

    // ----- G30: the descriptor-coercion loss instrument ---------------------
    //
    // These pin two separate things, and the second matters more than the
    // first: (a) that the instrument exists and classifies correctly, and
    // (b) that it changed NO ANSWER. 400 measured sites in this tree write a
    // primitive at a reference slot; `java.util.HashMap` cannot survive that
    // store being refused, and `RJdkHello` cannot survive it being boxed.
    //
    // Counter isolation: the counters are process-global and `cargo test`
    // runs this module in parallel, so every test that reads them takes
    // `G30_COUNTERS` first. The pre-G30 `t10_9_e_*` tests deliberately do NOT
    // need the lock — they call `coerce_field_value_by_descriptor`, which
    // lands in the `Unattributed` COLUMN, while every assertion here is on
    // the `Read`/`Store` columns.

    static G30_COUNTERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn g30_lock() -> std::sync::MutexGuard<'static, ()> {
        // A panicking test must not poison the rest of the suite into
        // failing for an unrelated reason.
        G30_COUNTERS.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn g30_count(loss: FieldCoercionLoss, kind: FieldAccessKind) -> u64 {
        field_coercion_loss_counts()[loss.index()][kind.index()]
    }

    /// THE PIN. `java.util.HashMap.table` is declared
    /// `[Ljava/util/HashMap$Node;`, and CratonVM's synthetic init paths write
    /// `Value::Int(capacity)` there. The `b'L' | b'['` arm turning that into
    /// `null` is what lets `HashMap.resize()`'s
    /// `(oldTab == null) ? 0 : oldTab.length` handle the never-initialised
    /// case; without it the JDK's own `arraylength` aborts with
    /// "expected object reference, got int(N)". This is rule `S111r29` and it
    /// is LOAD-BEARING.
    ///
    /// The obvious reading of the G30 defect — "stop nulling, refuse instead"
    /// — breaks exactly this, which is why the instrument counts and does not
    /// intervene. If a later lane deletes the degrade, this test is the thing
    /// that says so.
    #[test]
    fn the_hashmap_table_degrade_to_null_is_pinned() {
        let _g = g30_lock();
        let heap = Heap::new();
        // Slot 2 stands for `HashMap.table`; `[` is its real descriptor byte.
        let map = heap.alloc_object(ClassId::new(7), 3);

        for capacity in [Value::Int(16), Value::Int(1), Value::Long(64)] {
            heap.set_field_as(map, 2, capacity, b'[');
            assert_eq!(
                heap.get_field(map, 2),
                Value::Object(None),
                "a capacity written at HashMap.table must degrade to null so \
                 `(oldTab == null) ? 0 : oldTab.length` takes the null branch; \
                 refusing or boxing the store breaks HashMap.resize()",
            );
            assert_eq!(
                heap.get_field_as(map, 2, b'['),
                Value::Object(None),
                "and it must still read back as null through the \
                 descriptor-aware getter",
            );
        }

        // A genuine table array is untouched — the degrade must not fire on
        // the case it exists to make possible.
        let table = heap.alloc_object(ClassId::new(8), 0);
        heap.set_field_as(map, 2, Value::Object(Some(table)), b'[');
        match heap.get_field_as(map, 2, b'[') {
            Value::Object(Some(o)) => assert_eq!(o.as_ptr(), table.as_ptr()),
            other => panic!("a real table array must survive, got {other:?}"),
        }
    }

    /// The whole point of G30: the instrument observes, it does not repair.
    /// Every answer below is the answer this function gave BEFORE the
    /// instrument existed. If any of these move, 400 call sites move with
    /// them.
    #[test]
    fn the_g30_instrument_changes_no_answer() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 1);
        let ptr = obj.as_ptr() as usize;
        let site = FieldCoercionSite::store(None, 0);
        let c = |v: Value, d: u8| coerce_field_value_for_slot(v, d, site);

        // Lossless normalisation — untouched arms, listed so a refactor that
        // "tidies" them fails here.
        assert_eq!(c(Value::Int(-7), b'J'), Value::Long(-7));
        assert_eq!(c(Value::Double(f64::from_bits(42)), b'J'), Value::Long(42));
        assert_eq!(c(Value::Long(5), b'I'), Value::Int(5));

        // `Uninitialized` is the allocator's tag, not a claim about the slot:
        // still the typed zero, and (see the next test) still silent.
        assert_eq!(c(Value::Uninitialized, b'J'), Value::Long(0));
        assert_eq!(c(Value::Uninitialized, b'I'), Value::Int(0));
        assert_eq!(c(Value::Uninitialized, b'L'), Value::Uninitialized);

        // null -> primitive: the typed zero. G25-1 §1: this is the arm that
        // turned an `Object(None)` aimed at a model slot into
        // `java.net.ServerSocket.closed = false`.
        assert_eq!(c(Value::Object(None), b'J'), Value::Long(0));
        assert_eq!(c(Value::Object(None), b'I'), Value::Int(0));
        assert_eq!(c(Value::Object(None), b'Z'), Value::Int(0));
        assert_eq!(c(Value::Object(None), b'F'), Value::Float(0.0));
        assert_eq!(c(Value::Object(None), b'D'), Value::Double(0.0));

        // pointer -> primitive: the address, as a number.
        assert_eq!(c(Value::Object(Some(obj)), b'J'), Value::Long(ptr as i64));
        assert_eq!(c(Value::Object(Some(obj)), b'I'), Value::Int(ptr as i32));

        // primitive -> reference: null, for Int/Long/Double...
        assert_eq!(c(Value::Int(99), b'L'), Value::Object(None));
        assert_eq!(c(Value::Long(99), b'['), Value::Object(None));
        assert_eq!(c(Value::Double(1.5), b'L'), Value::Object(None));
        // ...but NOT for Float, which is stored as-is. That asymmetry is
        // pre-existing and disclosed, not introduced here.
        assert_eq!(c(Value::Float(1.5), b'L'), Value::Float(1.5));

        // Unknown descriptor still fails open.
        assert_eq!(c(Value::Int(99), b'V'), Value::Int(99));

        // And the descriptor-only entry point agrees with the sited one.
        for (v, d) in [
            (Value::Int(3), b'L'),
            (Value::Object(None), b'I'),
            (Value::Float(1.5), b'L'),
            (Value::Uninitialized, b'J'),
        ] {
            assert_eq!(
                coerce_field_value_by_descriptor(v, d),
                coerce_field_value_for_slot(v, d, site),
                "provenance must not change the answer for {v:?} at {}",
                d as char,
            );
        }
    }

    /// Each destroyed value lands in its own (species, direction) cell.
    ///
    /// Direction is the part that earns its keep: MEASURED on the pre-G30
    /// binary, 336 of the 352 cross-type accesses in one `RJdkNet` run are
    /// READS of `java.lang.ref.ReferenceQueue.head` on a slot that was never
    /// descriptor-initialised — benign, because the read answers `null`,
    /// which is what the field means. Counting those in the same bucket as a
    /// store is how the 16 real stores become invisible.
    #[test]
    fn each_destroyed_value_is_counted_under_its_own_species_and_direction() {
        let _g = g30_lock();
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 2);
        let store = FieldCoercionSite::store(Some(ClassId::new(1)), 0);
        let read = FieldCoercionSite::read(Some(ClassId::new(1)), 0);

        let before = field_coercion_loss_counts();
        let total_before = field_coercion_loss_total();

        coerce_field_value_for_slot(Value::Int(42), b'L', store);
        coerce_field_value_for_slot(Value::Double(1.0), b'[', store);
        coerce_field_value_for_slot(Value::Float(1.0), b'L', store);
        coerce_field_value_for_slot(Value::Object(None), b'I', store);
        coerce_field_value_for_slot(Value::Object(Some(obj)), b'J', store);
        coerce_field_value_for_slot(Value::Int(42), b'L', read);

        let after = field_coercion_loss_counts();
        let d = |l: FieldCoercionLoss, k: FieldAccessKind| {
            after[l.index()][k.index()] - before[l.index()][k.index()]
        };
        // Fully qualified rather than glob-imported: `Read` and `Store` are
        // names a future `use` in this module could easily shadow, and the
        // failure mode would be a type error a long way from here.
        assert_eq!(
            d(
                FieldCoercionLoss::PrimitiveIntoReference,
                FieldAccessKind::Store
            ),
            2,
            "Int and Double",
        );
        assert_eq!(
            d(
                FieldCoercionLoss::PrimitiveIntoReference,
                FieldAccessKind::Read
            ),
            1,
        );
        assert_eq!(
            d(
                FieldCoercionLoss::PrimitiveIntoReferenceUncoerced,
                FieldAccessKind::Store
            ),
            1,
            "Float",
        );
        assert_eq!(
            d(FieldCoercionLoss::NullIntoPrimitive, FieldAccessKind::Store),
            1,
        );
        assert_eq!(
            d(
                FieldCoercionLoss::PointerIntoPrimitive,
                FieldAccessKind::Store
            ),
            1,
        );
        assert_eq!(field_coercion_loss_total() - total_before, 6);
    }

    /// The zero-init contract must stay silent.
    ///
    /// `alloc_object_with_descriptors` leaves slots tagged `Uninitialized`,
    /// and normalising those to the typed zero is the FIX from R1, not a
    /// defect. If they were counted, every allocation would look like a
    /// violation and the instrument would be worthless.
    #[test]
    fn an_uninitialized_slot_is_not_reported_as_a_loss() {
        let _g = g30_lock();
        let site = FieldCoercionSite::store(None, 0);
        let before = field_coercion_loss_total();
        for d in [b'J', b'D', b'F', b'I', b'B', b'C', b'S', b'Z', b'L', b'['] {
            coerce_field_value_for_slot(Value::Uninitialized, d, site);
        }
        assert_eq!(
            field_coercion_loss_total(),
            before,
            "Uninitialized is the allocator's 'no value yet' tag, not a \
             writer's claim about the slot — it must never be counted",
        );
    }

    /// The report renders only what fired, and names the species it renders.
    #[test]
    fn the_loss_report_names_every_species_that_fired() {
        let _g = g30_lock();
        let site = FieldCoercionSite::store(None, 0);
        coerce_field_value_for_slot(Value::Int(1), b'L', site);
        let report =
            field_coercion_loss_report().expect("a loss has fired, so the report must not be None");
        assert!(
            report.contains("primitive-into-reference"),
            "report should name the species that fired: {report}",
        );
        assert!(
            report.contains("store="),
            "report should break the count down by access direction: {report}",
        );
    }

    /// The heap's own descriptor-aware setter reports as a STORE and carries
    /// the class and slot. This is the shape NOMINATION 1 asks the three
    /// live collectors to adopt; pinning it here means the nomination can be
    /// applied mechanically.
    #[test]
    fn the_descriptor_aware_setter_reports_a_store_with_provenance() {
        let _g = g30_lock();
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(3), 1);
        let before = g30_count(
            FieldCoercionLoss::PrimitiveIntoReference,
            FieldAccessKind::Store,
        );
        heap.set_field_as(obj, 0, Value::Int(1234), b'L');
        assert_eq!(
            g30_count(
                FieldCoercionLoss::PrimitiveIntoReference,
                FieldAccessKind::Store,
            ),
            before + 1,
            "Heap::set_field_as must attribute its loss to the STORE column",
        );
        assert_eq!(heap.get_field(obj, 0), Value::Object(None));
    }

    // ----- G52: the Double arm, and the theorem clone rests on -------------

    /// Bit-exact `Value` comparison. `PartialEq` on `f32`/`f64` says a NaN is
    /// not itself, and several arms below legitimately produce one (a long
    /// bit pattern reinterpreted as a float usually IS a NaN), so a plain
    /// `assert_eq!` would fail on values that are in fact identical.
    fn same(a: Value, b: Value) -> bool {
        match (a, b) {
            (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
            (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
            _ => a == b,
        }
    }

    /// THE `b'I'` DOUBLE ARM, PINNED. `G43-1` NOMINATION 5 asked for
    /// `d as i32` in place of `d.to_bits() as i32`. This test is why that
    /// must not happen.
    ///
    /// `Value::Long(l)` and `Value::Double(f64::from_bits(l as u64))` are the
    /// SAME untagged compact slot read two ways —
    /// `CompactValue::to_value()` surfaces an untagged slot as a `Double`,
    /// which is the whole reason the `b'J'` arm's Session-93 repair exists.
    /// They must therefore agree about the slot's low 32 bits. The bit
    /// projection makes them agree for every `l`; the numeric narrowing would
    /// make `CompactValue::long(5)` read back as `0` at an `int` field.
    #[test]
    fn a_double_at_an_integral_slot_decodes_like_the_untagged_long_it_usually_is() {
        let _g = g30_lock();
        let site = FieldCoercionSite::store(None, 0);
        let c = |v: Value, d: u8| coerce_field_value_for_slot(v, d, site);

        // NaN payloads: every pattern below is either not a NaN at all or is
        // a QUIET NaN (mantissa MSB set), which `f64::from_bits`/`to_bits`
        // round-trip. Signalling patterns are deliberately not used.
        for l in [0i64, 1, 5, -1, 0x0123_4567_89AB_CDEF, i64::MIN] {
            let as_double = Value::Double(f64::from_bits(l as u64));
            for d in [b'I', b'B', b'C', b'S', b'Z'] {
                assert!(
                    same(c(Value::Long(l), d), c(as_double, d)),
                    "a long and the untagged slot it decodes from must agree \
                     at descriptor {}: long {l} gave {:?}, double gave {:?}",
                    d as char,
                    c(Value::Long(l), d),
                    c(as_double, d),
                );
            }
            // ...and the same slot at `J`, which is where the rule the `I`
            // arm mirrors is already documented.
            assert!(same(c(as_double, b'J'), Value::Long(l)));
        }

        // `G43-1` §5.2's arithmetic, spelled out so the record and the code
        // cannot drift apart. A synthetic `Provider` version landing on the
        // real `java.util.Hashtable.count`:
        //   25.0 -> 0x4039_0000_0000_0000, low half 0 -> count == 0, and
        //           `Hashtable.getEnumeration` early-returns. LATENT.
        //   1.8  -> 0x3FFC_CCCC_CCCC_CCCD, low half 0xCCCC_CCCD.  LIVE.
        // Under `d as i32` the first line would read `count == 25` and walk
        // 25 buckets of a table holding a `String` — i.e. the proposed repair
        // makes that record's own case WORSE, not safer.
        assert_eq!(c(Value::Double(25.0), b'I'), Value::Int(0));
        assert_eq!(c(Value::Double(1.8), b'I'), Value::Int(-858_993_459));
        assert_ne!(
            c(Value::Double(25.0), b'I'),
            Value::Int(25),
            "this arm is a bit projection, not a numeric narrowing; see the \
             comment at the arm before changing it",
        );
    }

    /// The `Double`/`Long` pair agrees at every integral descriptor and at
    /// `J`/`D` — and disagrees at exactly one place, `b'F'`, because that
    /// arm's `Long` case is a bit decode and its `Double` case is a numeric
    /// one. Pinned so the known asymmetry cannot drift in silence, and so
    /// whoever decides it has a test to change rather than a surprise.
    #[test]
    fn the_double_and_long_arms_disagree_only_at_a_float_slot() {
        let _g = g30_lock();
        let site = FieldCoercionSite::store(None, 0);
        let c = |v: Value, d: u8| coerce_field_value_for_slot(v, d, site);
        let l = 5i64;
        let as_double = Value::Double(f64::from_bits(l as u64));

        for d in [b'I', b'J', b'D'] {
            assert!(
                same(c(Value::Long(l), d), c(as_double, d)),
                "the pair must agree at {}",
                d as char,
            );
        }
        assert!(
            !same(c(Value::Long(l), b'F'), c(as_double, b'F')),
            "b'F' is the one cell where the pair disagrees; if this now \
             passes, someone unified them — good, but update the comment at \
             the arm and the G52-1 record",
        );
        assert!(same(
            c(Value::Long(l), b'F'),
            Value::Float(f32::from_bits(5))
        ));
        assert!(same(c(as_double, b'F'), Value::Float(0.0)));
    }

    /// IDEMPOTENCE, and it is not an academic property.
    ///
    /// `native_object_clone` (`native-builtins/src/lib.rs`) copies a field
    /// with `ctx.get_field` then `ctx.set_field`, and BOTH of those resolve
    /// the slot's descriptor and land here (`vm_exec.rs`'s
    /// `NativeContextImpl`). So every cloned field is coerced TWICE, and the
    /// clone's contents equal the original's coerced contents only if this
    /// function is idempotent. Nothing asserted that before G52.
    ///
    /// It also bounds any future caller that composes two coercions — a
    /// read-modify-write through the descriptor-aware pair, a CAS, a
    /// re-decode after a slot move.
    #[test]
    fn the_descriptor_coercion_is_idempotent() {
        let _g = g30_lock();
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(1), 1);
        let site = FieldCoercionSite::store(None, 0);

        let values = [
            Value::Int(7),
            Value::Long(-3),
            Value::Float(1.5),
            Value::Double(2.5),
            Value::Double(f64::from_bits(5)),
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Uninitialized,
            Value::ReturnAddress(9),
        ];
        for v in values {
            for d in [
                b'J', b'D', b'F', b'I', b'B', b'C', b'S', b'Z', b'L', b'[', b'V',
            ] {
                let once = coerce_field_value_for_slot(v, d, site);
                let twice = coerce_field_value_for_slot(once, d, site);
                assert!(
                    same(once, twice),
                    "coercing {v:?} at {} twice must equal coercing it once, \
                     got {once:?} then {twice:?} — Object.clone() copies \
                     every field through two of these",
                    d as char,
                );
            }
        }
    }

    // ----- Part F: GPU/GC coordination tests --------------------------------
    //
    // The whole block is doubly-gated (#[cfg(test)] from the surrounding
    // module + #[cfg(feature = "gpu-offload")]) so that the default
    // `cargo test -p cratonvm-gc` invocation does not even compile this code.

    #[cfg(feature = "gpu-offload")]
    mod gpu_offload_tests {
        use super::*;
        use crate::collector::MonitorCleanup;
        use std::collections::HashMap;

        /// MonitorCleanup stub for tests — matches the pattern in
        /// `tests/phase_h_integration.rs` and `gen_heap.rs::NoOpMonitors`.
        struct NoMonitors;
        impl MonitorCleanup for NoMonitors {
            fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
        }

        /// Test-only `StopTheWorldToken`. These tests are mostly single-
        /// threaded; the worker-thread test in
        /// `safepoint_check_skips_gc_while_token_held` deliberately races a
        /// GC against a pin/unpin sequence and the harness itself ensures
        /// no other mutator is touching the heap concurrently.
        #[inline]
        fn stw() -> crate::collector::StopTheWorldToken {
            // SAFETY: these unit tests run the heap single-threaded.
            unsafe { crate::collector::StopTheWorldToken::new() }
        }

        #[test]
        fn enter_gpu_critical_increments_counter() {
            let heap = Heap::new();
            assert_eq!(heap.gpu_critical_count(), 0, "fresh heap has zero tokens");

            let _t = heap.enter_gpu_critical();
            assert_eq!(
                heap.gpu_critical_count(),
                1,
                "one token alive after enter_gpu_critical"
            );
        }

        #[test]
        fn drop_token_decrements_counter() {
            let heap = Heap::new();
            {
                let _t = heap.enter_gpu_critical();
                assert_eq!(heap.gpu_critical_count(), 1);
            }
            assert_eq!(
                heap.gpu_critical_count(),
                0,
                "counter returns to zero on token drop"
            );
        }

        #[test]
        fn nested_tokens_count_independently() {
            let heap = Heap::new();
            let t1 = heap.enter_gpu_critical();
            let t2 = heap.enter_gpu_critical();
            assert_eq!(heap.gpu_critical_count(), 2, "two tokens alive");

            drop(t1);
            assert_eq!(
                heap.gpu_critical_count(),
                1,
                "dropping one token leaves one alive"
            );

            drop(t2);
            assert_eq!(heap.gpu_critical_count(), 0, "all tokens released");
        }

        #[test]
        fn safepoint_check_skips_gc_while_token_held() {
            // Observable side-effect: gpu_blocked_gc_count increments each
            // time collect_garbage is called while a token is alive.
            //
            // We do the GC call on a worker thread so we can drop the token
            // from the main thread and join cleanly. The worker thread first
            // races to bump gpu_blocked_gc_count, then proceeds with a
            // (now no-op) collection.
            use std::sync::Arc;
            use std::thread;

            let heap = Arc::new(Heap::new());
            // Allocate an unrooted object so collect_garbage has something
            // (or nothing — that's fine; we only care about the gate).
            let _scratch = heap.alloc_object(ClassId::new(0), 1);

            let token = heap.enter_gpu_critical();
            assert_eq!(heap.gpu_blocked_gc_count(), 0);

            let heap_clone = Arc::clone(&heap);
            let gc_thread = thread::spawn(move || {
                let mut roots: Vec<ObjectRef> = Vec::new();
                heap_clone.collect_garbage(&stw(), &mut roots, &NoMonitors);
            });

            // Spin until the worker has noticed the token. The blocked-count
            // increment happens before the worker enters the yield loop, so
            // observing it tells us the gate fired. A 5s ceiling matches
            // GPU_CRITICAL_DEADLINE_SECS and is plenty for a wakeup.
            let start = std::time::Instant::now();
            while heap.gpu_blocked_gc_count() == 0 {
                if start.elapsed() > std::time::Duration::from_secs(5) {
                    panic!("GC worker never observed the token");
                }
                std::thread::yield_now();
            }
            assert!(
                heap.gpu_blocked_gc_count() >= 1,
                "GC saw the token and stepped aside",
            );

            // Release the token so the worker can finish.
            drop(token);
            gc_thread.join().expect("GC thread joined cleanly");
            assert_eq!(heap.gpu_critical_count(), 0);
        }

        #[test]
        fn pinned_ref_survives_gc_with_real_heap_alloc() {
            // Sequence:
            // 1. Allocate a real array via the Heap API — no synthesised
            //    ObjectRefs.
            // 2. Pin it.
            // 3. Run a GC with an empty `roots` slice. Without the pin,
            //    the array would be collected (the snapshot we kept would
            //    become a dangling-ish address). With the pin, the array
            //    is walked as a root and survives.
            // 4. After the GC, the (updated) pinned ref still has the
            //    correct header.
            let heap = Heap::new();
            let arr = heap.alloc_array(ClassId::new(7), ArrayElementType::Int, 4);
            heap.set_array_element(arr, 0, Value::Int(11)).unwrap();
            heap.set_array_element(arr, 3, Value::Int(44)).unwrap();

            heap.pin_ref(arr);
            assert_eq!(
                heap.gpu_pinned_refs_snapshot().len(),
                1,
                "exactly one pinned ref before GC"
            );

            // Empty caller-roots: only the pin keeps `arr` alive.
            let mut roots: Vec<ObjectRef> = Vec::new();
            heap.collect_garbage(&stw(), &mut roots, &NoMonitors);

            // After GC the address may have moved. The pinned-refs set
            // contains the post-GC address.
            let pinned_after: Vec<ObjectRef> = heap.gpu_pinned_refs_snapshot();
            assert_eq!(pinned_after.len(), 1, "pin preserved across GC");
            let arr_new = pinned_after[0];

            // The header decodes correctly via the new address.
            let h = heap.get_header(arr_new);
            assert_eq!(h.kind, ObjectKind::Array);
            assert_eq!(h.array_length(), 4);
            assert_eq!(h.class_id, ClassId::new(7));
            // And the element values survived.
            assert_eq!(
                heap.get_array_element(arr_new, 0).unwrap().as_int(),
                Some(11),
            );
            assert_eq!(
                heap.get_array_element(arr_new, 3).unwrap().as_int(),
                Some(44),
            );

            heap.unpin_ref(arr_new);
            assert_eq!(
                heap.gpu_pinned_refs_snapshot().len(),
                0,
                "unpin_ref removes the entry",
            );
        }

        #[test]
        fn root_walker_includes_pinned_refs() {
            // Variant of the test above that asserts the GC's pointer_map
            // contains an entry for the pinned ref — i.e. that the root
            // walker actually visited it. With the feature off this code
            // doesn't compile, so we can rely on the gated combined-roots
            // path running.
            let heap = Heap::new();
            let arr = heap.alloc_array(ClassId::new(3), ArrayElementType::Int, 2);
            let old_addr = arr.as_ptr() as usize;

            heap.pin_ref(arr);

            let mut roots: Vec<ObjectRef> = Vec::new();
            let result = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);

            assert!(
                result.pointer_map.contains_key(&old_addr),
                "pinned ref must appear in the GC pointer map (root walker saw it)",
            );

            heap.unpin_ref(arr);
        }
    }
}
