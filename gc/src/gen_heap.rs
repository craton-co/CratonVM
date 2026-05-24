//! Generational garbage-collected heap.
//!
//! Combines a young generation (semi-space copying) with an old generation
//! (non-moving free-list mark-sweep) and a card table for efficient
//! old→young reference tracking.
//!
//! ## Allocation
//! All new objects are allocated in the young generation's from-space via
//! bump-pointer allocation. Objects are promoted to the old generation
//! after surviving [`PROMOTION_AGE`] minor GC cycles.
//!
//! ## Minor GC
//! Triggered when young from-space exceeds [`YOUNG_GC_THRESHOLD_PERCENT`]%.
//! Copies live young objects to young to-space (incrementing age), or
//! promotes them to old gen if their age reaches the threshold.
//! Dirty card table entries are scanned for old→young references as
//! additional roots.
//!
//! ## Major GC
//! Triggered when old gen runs out of space during promotion. Performs a
//! mark-sweep of the old generation, freeing unreachable objects back to
//! the free list.

use std::backtrace::Backtrace;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use std::sync::Arc;

use crate::arena::Arena;
use crate::card_table::CardTable;
use crate::collector::{GarbageCollector, MonitorCleanup};
use crate::concurrent_mark::ConcurrentGcState;
use crate::gc::GcResult;
use crate::heap::{
    array_data_size, read_prim_element, write_prim_element, ArrayElementType, ObjectHeader,
    ObjectKind, AUTOBOX_CLASS_ID, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE, REF_ELEMENT_SIZE,
    SLOT_SIZE,
};
use crate::old_gen::OldGen;
use crate::satb::SatbQueue;
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of each young-generation semi-space.
///
/// Bumped from 16 MB to 64 MB to reduce minor-GC frequency under
/// allocation-heavy workloads (the RealWorldBench "GC stress" phase
/// was 26× slower than HotSpot because it triggered full GC after
/// every ~16 MB of allocation). HotSpot defaults to ~25% of max heap
/// for the young gen; 64 MB for a 256 MB max heap is the same ratio.
const DEFAULT_YOUNG_SEMI_SIZE: usize = 64 * 1024 * 1024;

/// Size of the old generation (128 MB).
const DEFAULT_OLD_GEN_SIZE: usize = 128 * 1024 * 1024;

/// Number of minor GC survivals before an object is promoted to old gen.
const PROMOTION_AGE: u8 = 3;

/// GC threshold: trigger minor GC when young from-space usage exceeds this %.
const YOUNG_GC_THRESHOLD_PERCENT: usize = 75;

/// Maximum allowed heap expansion factor (4x the initial size).
const MAX_HEAP_EXPANSION_FACTOR: usize = 4;

/// If GC reclaims less than this fraction of young gen, expand the heap.
const GC_EXPANSION_THRESHOLD_PERCENT: usize = 25;

// ---------------------------------------------------------------------------
// GenerationalHeap
// ---------------------------------------------------------------------------

/// Statistics for the generational heap.  All counters are
/// monotonically non-decreasing; use [`HeapStats::snapshot`] to capture a
/// consistent view at one point in time.  This is Phase H (RH.1) — a
/// hardening-only addition so tests can assert that allocation pressure
/// does not corrupt promotion bookkeeping (every object is accounted
/// for exactly once).
#[derive(Default, Debug)]
pub struct HeapStats {
    /// Number of minor GC cycles completed.
    minor_gc_count: AtomicU64,
    /// Number of major (old-gen mark-sweep) GC cycles completed.
    major_gc_count: AtomicU64,
    /// Total bytes copied in the young gen across all minor GCs.
    bytes_copied_young: AtomicU64,
    /// Total objects copied in the young gen across all minor GCs.
    objects_copied_young: AtomicU64,
    /// Total bytes promoted from young→old across all minor GCs.
    bytes_promoted: AtomicU64,
    /// Total objects promoted from young→old across all minor GCs.
    objects_promoted: AtomicU64,
    /// Total bytes freed by major GC sweep phases.
    bytes_freed_old: AtomicU64,
    /// Number of young allocations since startup.
    young_allocations: AtomicU64,
    /// Number of old-gen direct allocations (e.g. humongous) since
    /// startup.  Currently all allocations go through young, but the
    /// counter is exposed for future humongous handling.
    old_allocations: AtomicU64,
}

/// Immutable snapshot of [`HeapStats`] at a moment in time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStatsSnapshot {
    pub minor_gc_count: u64,
    pub major_gc_count: u64,
    pub bytes_copied_young: u64,
    pub objects_copied_young: u64,
    pub bytes_promoted: u64,
    pub objects_promoted: u64,
    pub bytes_freed_old: u64,
    pub young_allocations: u64,
    pub old_allocations: u64,
}

impl HeapStats {
    pub fn snapshot(&self) -> HeapStatsSnapshot {
        // `Relaxed` is fine for monotonic counters sampled from outside
        // a GC pause — snapshot consistency is not guaranteed and
        // callers only use this for observability / regression tests.
        HeapStatsSnapshot {
            minor_gc_count: self.minor_gc_count.load(Ordering::Relaxed),
            major_gc_count: self.major_gc_count.load(Ordering::Relaxed),
            bytes_copied_young: self.bytes_copied_young.load(Ordering::Relaxed),
            objects_copied_young: self.objects_copied_young.load(Ordering::Relaxed),
            bytes_promoted: self.bytes_promoted.load(Ordering::Relaxed),
            objects_promoted: self.objects_promoted.load(Ordering::Relaxed),
            bytes_freed_old: self.bytes_freed_old.load(Ordering::Relaxed),
            young_allocations: self.young_allocations.load(Ordering::Relaxed),
            old_allocations: self.old_allocations.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters to zero.  Useful in tests that want to
    /// observe a single GC cycle in isolation.
    pub fn reset(&self) {
        self.minor_gc_count.store(0, Ordering::Relaxed);
        self.major_gc_count.store(0, Ordering::Relaxed);
        self.bytes_copied_young.store(0, Ordering::Relaxed);
        self.objects_copied_young.store(0, Ordering::Relaxed);
        self.bytes_promoted.store(0, Ordering::Relaxed);
        self.objects_promoted.store(0, Ordering::Relaxed);
        self.bytes_freed_old.store(0, Ordering::Relaxed);
        self.young_allocations.store(0, Ordering::Relaxed);
        self.old_allocations.store(0, Ordering::Relaxed);
    }
}

/// A generational garbage-collected heap.
///
/// Young generation: two semi-spaces (from/to) using Cheney copying.
/// Old generation: non-moving free-list allocator with mark-sweep.
/// Card table: tracks old→young cross-generation references.
pub struct GenerationalHeap {
    /// Young generation from-space (allocation target).
    young_from: Mutex<Arena>,
    /// Young generation to-space (GC copy target).
    young_to: Mutex<Arena>,
    /// Old generation (promoted objects).
    old_gen: Mutex<OldGen>,
    /// Card table covering the old generation's address space.
    ///
    /// T5.5.2 (HIGH-1 fix): the table now uses interior mutability for
    /// all state, so no outer `Mutex` is needed. The mutator write
    /// barrier calls [`CardTable::thread_local_dirty_addr`] on the
    /// shared reference (per-thread buffer, no shared lock); the
    /// collector calls [`CardTable::flush_all`] +
    /// [`CardTable::drain_pending`] at GC start to fold queued offsets
    /// into the authoritative bitmap before the dirty-card scan.
    card_table: CardTable,
    /// Next identity hash code.
    next_hash_code: AtomicI32,
    /// Young GC threshold in bytes.
    young_gc_threshold: Mutex<usize>,
    // Volatile field access uses `SeqCst` fences inside
    // `get_field_volatile`/`set_field_volatile`; no global lock is
    // needed (and the previous `Mutex<()>` here serialised every
    // volatile access across the entire heap, mirroring nothing in
    // the JMM). Removed to match the fence-only approach used by
    // `heap::Heap`.
    /// Global SATB queue for concurrent GC write barrier logging.
    /// Shared with the concurrent marker; `None` if concurrent GC is not enabled.
    satb_queue: Option<Arc<SatbQueue>>,
    /// Concurrent GC phase tracker. Shared with the concurrent marker.
    concurrent_gc_state: Option<Arc<ConcurrentGcState>>,
    /// Maximum young semi-space size (limits growth).
    max_young_semi_size: usize,
    /// Primary NUMA node hint for the young-gen slow path.
    ///
    /// Records the topology's preferred home node for this heap (defaults
    /// to 0 on single-node hosts, which covers every current test target).
    /// On a multi-node host this is the node whose arena the slow path
    /// would prefer if/when [`try_alloc_young`]/[`refill_tlab`] grow into a
    /// per-node arena layout.
    ///
    /// TODO(numa-multi-arena): replace the single `young_from`/`young_to`
    /// pair with `Arc<Vec<Mutex<Arena>>>` keyed by node id, and route the
    /// slow path through `numa::current_thread_node()` for the local arena.
    /// The fast path (TLAB) is per-thread so already NUMA-local by
    /// construction; only the refill/spill paths need plumbing. Today this
    /// field is consumed only by the debug/tracing stub in
    /// [`numa_slow_path_hint`] so the wiring lands without churning the
    /// arena layout — see numa_node_hint accessor below.
    numa_node_hint: usize,
    /// Total number of NUMA nodes seen at construction time (>=1).
    ///
    /// Cached so the slow path doesn't have to re-query
    /// [`crate::numa::global_topology`] on every allocation. Used by the
    /// stubbed hint logic to short-circuit on single-node hosts (preserving
    /// existing behavior exactly).
    numa_num_nodes: usize,
    /// Phase H (RH.1) statistics — updated during every minor/major GC.
    stats: HeapStats,
}

// Safety: Same reasoning as Heap — raw pointers are to internally owned memory.
// The Mutex on each sub-allocator serializes access.
unsafe impl Send for GenerationalHeap {}
unsafe impl Sync for GenerationalHeap {}

impl GenerationalHeap {
    /// Create a new generational heap with default sizes.
    pub fn new() -> Self {
        Self::with_sizes(DEFAULT_YOUNG_SEMI_SIZE, DEFAULT_OLD_GEN_SIZE)
    }

    /// Create a generational heap with custom young semi-space and old gen sizes.
    pub fn with_sizes(young_semi_size: usize, old_gen_size: usize) -> Self {
        let young_semi_size = young_semi_size.max(1024);
        let old_gen_size = old_gen_size.max(1024);
        let threshold = young_semi_size * YOUNG_GC_THRESHOLD_PERCENT / 100;
        let max_young = young_semi_size * MAX_HEAP_EXPANSION_FACTOR;

        let old_gen = OldGen::new(old_gen_size);
        let card_table = CardTable::new(old_gen.base_ptr() as usize, old_gen_size);

        // Cache the host NUMA shape at construction. The slow-path
        // allocator consults this through `numa_slow_path_hint`. On the
        // ~all-current-targets single-node case the hint is just 0 and
        // we behave exactly as before (no per-node arena yet — see TODO
        // on `numa_node_hint`).
        let topology = crate::numa::global_topology();
        let numa_num_nodes = topology.num_nodes.max(1);
        let numa_node_hint = if numa_num_nodes == 1 {
            0
        } else {
            // Multi-node: prefer the node of the constructing thread so
            // long-lived heap metadata lands near whoever booted the VM.
            // Falls back to 0 if the platform's current-thread probe is
            // unavailable, which keeps single-arena behavior stable.
            let n = crate::numa::NumaTopology::current_thread_node();
            if n < numa_num_nodes { n } else { 0 }
        };

        Self {
            young_from: Mutex::new(Arena::new(young_semi_size)),
            young_to: Mutex::new(Arena::new(young_semi_size)),
            old_gen: Mutex::new(old_gen),
            card_table,
            next_hash_code: AtomicI32::new(1),
            young_gc_threshold: Mutex::new(threshold),
            satb_queue: None,
            concurrent_gc_state: None,
            max_young_semi_size: max_young,
            numa_node_hint,
            numa_num_nodes,
            stats: HeapStats::default(),
        }
    }

    /// Return the primary NUMA node hint recorded at construction.
    ///
    /// Public observer so tests and profiling code can confirm the heap
    /// picked up the expected node. The field itself remains private to
    /// keep room for the multi-arena refactor.
    pub fn numa_node_hint(&self) -> usize {
        self.numa_node_hint
    }

    /// Number of NUMA nodes the heap was constructed for (>=1).
    pub fn numa_num_nodes(&self) -> usize {
        self.numa_num_nodes
    }

    /// Compute the node the *calling* thread would prefer for a young-gen
    /// slow-path allocation, falling back to the heap's primary hint.
    ///
    /// This is the seam where the multi-arena refactor will plug in: it
    /// returns the index that `try_alloc_young`/`refill_tlab` would use to
    /// pick a per-node arena. Today there is only one arena pair, so the
    /// return value is consumed only by the tracing stub below — but the
    /// query path is exercised on every slow-path allocation, which means
    /// the platform probe and topology cache are validated in production
    /// long before we flip the multi-arena switch.
    #[inline]
    fn numa_slow_path_hint(&self) -> usize {
        if self.numa_num_nodes <= 1 {
            return self.numa_node_hint;
        }
        let n = crate::numa::NumaTopology::current_thread_node();
        if n < self.numa_num_nodes {
            n
        } else {
            self.numa_node_hint
        }
    }

    /// Return a handle to the GC statistics counters.  Counters are
    /// updated during GC cycles and allocation fast-paths.
    pub fn stats(&self) -> &HeapStats {
        &self.stats
    }

    /// Create a generational heap with a total capacity split proportionally.
    ///
    /// HotSpot-equivalent sizing: young gen takes ~50% of the total heap
    /// (the from+to semi-space pair together), split across two
    /// semi-spaces. So each semi-space is ~25% of -Xmx. HotSpot's default
    /// is closer to NewRatio=2 (young = 1/3 of heap) but our copying
    /// collector trades old-gen room for young-gen room more aggressively
    /// because (a) the non-moving sweep that runs while JIT frames are
    /// active cannot promote, and so depends on the young semi being big
    /// enough to hold the entire transient working set; and (b) Cheney
    /// copy cost scales with *survivors*, not capacity, so a larger
    /// young semi is essentially free unless the working set actually
    /// grows to fill it.
    ///
    /// For the common ranges:
    ///   - 64 KiB heap (test):   young_semi = 16 KiB
    ///   - 256 MiB heap (def.):  young_semi = 64 MiB
    ///   - 1 GiB heap:           young_semi = 256 MiB
    ///   - 4 GiB heap (-Xmx4g):  young_semi = 1 GiB
    ///   - 16 GiB heap:          young_semi = 4 GiB
    ///
    /// The young semi-space can still *grow* up to
    /// `young_semi * MAX_HEAP_EXPANSION_FACTOR` after a low-reclamation
    /// minor GC (see the expansion logic in `collect_garbage_inner`).
    pub fn with_capacity(total_bytes: usize) -> Self {
        let total = total_bytes.max(4096);
        // Young takes 1/2 of total, split across from+to semi-spaces (so
        // each semi gets 1/4 of total). The other half goes to old gen.
        let young_semi_raw = total / 4;
        // Clamp: floor at 512 bytes (the smallest test heap) so very
        // tiny test heaps still produce a non-trivial arena. No ceiling —
        // the user passed -Xmx N to get N bytes of heap, not "N capped
        // at some arbitrary internal constant".
        const YOUNG_SEMI_MIN: usize = 512;
        let young_semi = young_semi_raw.max(YOUNG_SEMI_MIN);
        // Old gen gets whatever's left after the young (from+to). Use
        // `saturating_sub` so a tiny `total` (`max(4096)`) doesn't
        // underflow when the clamped young is larger than half of it.
        let young_pair = young_semi.saturating_mul(2);
        let old_size = total.saturating_sub(young_pair).max(512);
        Self::with_sizes(young_semi, old_size)
    }

    // ----- Allocation --------------------------------------------------------

    /// Allocate a new Java object in the young generation.
    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        // Checked arithmetic — an overflowed `total_size` would size the
        // allocation incorrectly. Mirrors `try_alloc_object`'s checked path.
        let total_size = num_fields
            .checked_mul(SLOT_SIZE)
            .and_then(|fields_size| HEADER_SIZE.checked_add(fields_size))
            .unwrap_or_else(|| {
                eprintln!(
                    "FATAL: object size overflow in gen_heap alloc_object \
                     (num_fields={})",
                    num_fields
                );
                std::process::abort();
            });
        // The slot count MUST fit the `u32` header field. Clamping to
        // `u32::MAX` would write a count that disagrees with `total_size`,
        // causing later GC scans to walk off the end of the object.
        let num_slots_u32 = u32::try_from(num_fields).unwrap_or_else(|_| {
            eprintln!(
                "FATAL: object field count exceeds u32 in gen_heap alloc_object \
                 (num_fields={})",
                num_fields
            );
            std::process::abort();
        });
        let ptr = self.alloc_young(total_size);

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            num_slots_u32,
        );

        // SAFETY: `ptr` was just bump-allocated from the young arena with sufficient
        // size (`HEADER_SIZE + num_fields * SLOT_SIZE`) and 8-byte alignment, so
        // writing an `ObjectHeader` at its start is valid. The pointer is non-null
        // and exclusively owned by this allocation; wrapping it in `ObjectRef` is
        // sound because the header has been fully initialized.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    /// Allocate a new Java object and initialize primitive-typed slots to
    /// their spec-mandated typed zero based on `descriptor_bytes`.
    ///
    /// See [`crate::heap::default_value_for_descriptor`] for the
    /// descriptor-byte-to-default-Value mapping. This method is the
    /// canonical allocation entry point for the VM's `new` bytecode path —
    /// it fixes a subtle correctness bug where primitive fields on a
    /// freshly-allocated object decoded as `Value::Object(None)` instead
    /// of the correctly-tagged zero, breaking `Unsafe.compareAndSetInt`
    /// comparisons against `Int(0)`.
    ///
    /// Fields beyond `descriptor_bytes.len()` keep the zeroed default
    /// (`Object(None)`), matching the reference-slot spec default.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        let obj = self.alloc_object(class_id, num_fields);
        let n = num_fields.min(descriptor_bytes.len());
        for i in 0..n {
            if let Some(default) = crate::heap::default_value_for_descriptor(descriptor_bytes[i]) {
                self.set_field(obj, i, default);
            }
        }
        obj
    }

    /// Fallible variant of [`Self::alloc_object_with_descriptors`] that
    /// returns `None` on young-gen exhaustion (caller should trigger GC
    /// and retry).
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        let obj = self.try_alloc_object(class_id, num_fields)?;
        let n = num_fields.min(descriptor_bytes.len());
        for i in 0..n {
            if let Some(default) = crate::heap::default_value_for_descriptor(descriptor_bytes[i]) {
                self.set_field(obj, i, default);
            }
        }
        Some(obj)
    }

    /// Allocate a new Java array in the young generation.
    ///
    /// Arrays use compact element sizes: 1 byte for boolean/byte, 2 for char/short,
    /// 4 for int/float, 8 for long/double/reference.
    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        let data_size = array_data_size(length, element_type)
            .unwrap_or_else(|_| { eprintln!("FATAL: array data size overflow in gen_heap alloc_array (length={}, element_type={:?})", length, element_type); std::process::abort(); });
        let total_size = HEADER_SIZE.checked_add(data_size).unwrap_or_else(|| {
            eprintln!(
                "FATAL: array size overflow in gen_heap alloc_array (length={}, element_type={:?})",
                length, element_type
            );
            std::process::abort();
        });
        // The length MUST fit the `u32` header field. Clamping to `u32::MAX`
        // would record a length that disagrees with the allocated `data_size`,
        // causing later GC scans to walk off the end of the array.
        let length_u32 = u32::try_from(length).unwrap_or_else(|_| {
            eprintln!(
                "FATAL: array length exceeds u32 in gen_heap alloc_array (length={}, element_type={:?})",
                length, element_type
            );
            std::process::abort();
        });
        let ptr = self.alloc_young(total_size);

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            length_u32,
            length_u32,
        );

        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // (`HEADER_SIZE + data_size`) and 8-byte alignment. Writing the header is valid
        // because the region is exclusively owned and properly sized. The data region
        // is already zeroed by `try_alloc_young()`. Wrapping in `ObjectRef` is sound
        // because the header is fully initialized.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            // Data region already zeroed by try_alloc_young() — no redundant memset needed.
            ObjectRef::from_raw(ptr)
        }
    }

    /// Try to allocate a Java object. Returns `None` if young gen is exhausted.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let total_size = HEADER_SIZE + num_fields.checked_mul(SLOT_SIZE)?;
        let ptr = self.try_alloc_young(total_size)?;
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference,
            self.next_hash(),
            0,
            u32::try_from(num_fields).ok()?,
        );
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // and 8-byte alignment via `try_alloc_young`. The pointer is exclusively owned,
        // so writing the header and creating an `ObjectRef` are sound.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Try to allocate a Java array. Returns `None` if young gen is exhausted.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let data_size = array_data_size(length, element_type).ok()?;
        let total_size = HEADER_SIZE.checked_add(data_size)?;
        let ptr = self.try_alloc_young(total_size)?;
        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type,
            self.next_hash(),
            u32::try_from(length).ok()?,
            u32::try_from(length).ok()?,
        );
        // SAFETY: `ptr` was bump-allocated from the young arena with sufficient size
        // for the array header + data and 8-byte alignment. The pointer is exclusively
        // owned, so writing the header and creating an `ObjectRef` are sound.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    // ----- Header access -----------------------------------------------------

    /// Read the object header from a heap reference.
    pub fn get_header(&self, obj_ref: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj_ref` was created by one of this heap's `alloc_*` methods (or
        // forwarded during GC), so its pointer targets a valid, fully initialized
        // `ObjectHeader` within a heap-owned arena. The reference lifetime is bounded
        // by `&self`, ensuring the arena stays alive.
        unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) }
    }

    /// Get the class id of a heap object.
    pub fn class_id_of(&self, obj_ref: ObjectRef) -> ClassId {
        self.get_header(obj_ref).class_id
    }

    /// Get the kind (Object or Array) of a heap allocation.
    pub fn kind_of(&self, obj_ref: ObjectRef) -> ObjectKind {
        self.get_header(obj_ref).kind
    }

    /// Get the element type of an array object.
    pub fn element_type_of(&self, obj_ref: ObjectRef) -> ArrayElementType {
        self.get_header(obj_ref).element_type
    }

    /// Get the identity hash code of a heap object.
    pub fn identity_hash_code(&self, obj_ref: ObjectRef) -> i32 {
        self.get_header(obj_ref).identity_hash_code
    }

    /// Conservative validity check for a *raw address* — used by NEW-1.5
    /// JIT frame root scanning to filter spurious stack values.
    ///
    /// Returns `Some(ObjectRef)` if `addr` lands on a live object header in
    /// any of this heap's three storage regions (young from-space, young
    /// to-space, or old-gen). Returns `None` for any address that is null,
    /// outside every region, or fails the header sanity checks (alignment,
    /// kind tag, plausible num_slots).
    ///
    /// This is intentionally a structural check only — we do not consult
    /// the live-object marking bitmap or the class manager. False positives
    /// are acceptable because the caller treats every returned `ObjectRef`
    /// as a *root* (which only inflates retention; it cannot cause incorrect
    /// behavior). False negatives (missing a real object) would be wrong, so
    /// we err on the inclusive side.
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        // Reject obvious garbage.
        if addr == 0 {
            return None;
        }
        // Object headers are 8-byte aligned (and HEADER_SIZE itself is a
        // multiple of 8). A pointer to a real object never has its low 3
        // bits set.
        if addr & 0x7 != 0 {
            return None;
        }
        let raw = addr as *const u8;

        // Region check: must fall inside one of the three arenas. Holding
        // the locks for the duration of the validation is fine — this is
        // only called during stop-the-world root scanning.
        let in_region = self.young_from.lock().contains(raw)
            || self.young_to.lock().contains(raw)
            || self.old_gen.lock().contains(raw);
        if !in_region {
            return None;
        }

        // SAFETY: The region check above confirmed `raw` is inside one of the
        // three arenas, so reading `HEADER_SIZE` bytes from it is valid memory.
        // The reference is short-lived and the arena locks are held.
        let header = unsafe { &*(raw as *const ObjectHeader) };

        // Validate the discriminated-union tag. Object/Array are the only
        // valid kinds; anything else means we landed in the middle of a
        // field or in stale memory. HumongousFiller is a synthetic
        // walker-sentinel (round-9 gc CRIT-1) and never represents a
        // real object reachable from a root.
        match header.kind {
            ObjectKind::Object | ObjectKind::Array => {}
            ObjectKind::HumongousFiller => return None,
        }
        // Cap num_slots at a sanity limit so a stale word can't fool us
        // into "validating" a slot count that would exceed the arena.
        // Multi-array reloc fix (2026-05-22): for arrays, `num_slots` is a
        // mirror of `array_length` (see `alloc_array` and `try_alloc_array`),
        // so a legitimate 256 MB int[] has num_slots = 2^26 > 1<<24 and
        // would be falsely rejected here. Bound num_slots only for non-arrays.
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24; // 16M slots → 256 MB obj
        let is_array = matches!(header.kind, ObjectKind::Array);
        if !is_array && header.num_slots > MAX_PLAUSIBLE_SLOTS {
            return None;
        }
        // Array length, if it's an array, must also be plausible. The JVM
        // spec caps arrays at `Integer.MAX_VALUE` elements, so use that as
        // the upper bound (matches `MAX_REASONABLE_ARRAY_LEN` in
        // `array_length()`).
        if is_array && header.array_length > i32::MAX as u32 {
            return None;
        }

        // SAFETY: `raw` passed the region containment and header sanity checks above,
        // so it points to a valid object header within a heap arena.
        Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
    }

    /// Loose validity check: alignment + region containment only.
    ///
    /// Unlike [`Self::is_object_address`] this does **not** read the object
    /// header. It is intended for GC root scanning of ambiguous slots (JVM
    /// long vs jobject smuggled as jlong) where the header may not yet be
    /// initialised, the pointer may target an interior offset, or the slot
    /// may transiently look like a pointer mid-construction.
    ///
    /// False positives (passing a non-object aligned heap-range address) are
    /// safe: the generational collector's `forward_object` already drops
    /// "suspected false roots" whose computed extent runs off the end of the
    /// from-space arena, so the worst case is over-retention rather than a
    /// deref of garbage.
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        if addr == 0 || addr & 0x7 != 0 {
            return None;
        }
        let raw = addr as *const u8;
        let in_region = self.young_from.lock().contains(raw)
            || self.young_to.lock().contains(raw)
            || self.old_gen.lock().contains(raw);
        if !in_region {
            return None;
        }
        // SAFETY: alignment + region containment guarantee a valid heap
        // pointer; constructing an ObjectRef from it is safe — only deref
        // through it is gated by downstream header sanity checks.
        Some(unsafe { ObjectRef::from_raw(raw as *mut u8) })
    }

    // ----- Field access ------------------------------------------------------

    /// Get the value of a field at the given index.
    pub fn get_field(&self, obj_ref: ObjectRef, index: usize) -> Value {
        // KC16 SIGSEGV audit: runtime (not debug-only) bounds check. A
        // corrupted ObjectRef whose fake header has a huge num_slots would
        // happily pass the old debug_assert in release and then read
        // arbitrary memory.  For the suspect-header case we still panic
        // (that's a real bug).  For in-bounds-of-reality but out-of-bounds
        // of this object's layout (the common case when synthetic and
        // real-JDK class layouts disagree), return Object(None) — the
        // interpreter's primitive-field coercion will further normalize
        // it.  This converts what would be a silent SIGSEGV / panic into
        // a benign null read, matching HotSpot's behavior when an object
        // is accessed through a Reflection path that resolved a
        // larger-than-actual layout.
        let header = self.get_header(obj_ref);
        let num_slots = header.num_slots as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                kind_byte = header.kind as u8,
                gc_flags = format!("{:x}", header.gc_flags),
                "gen_heap::get_field: suspect header (returning null)",
            );
            // Rate-limited warn — do NOT panic. Returning null lets the
            // caller raise a normal Java exception / continue in a degraded
            // mode, matching how `array_length` is handled above. Without
            // this, Keycloak / Kafka tests abort the JVM on a corrupt
            // synthetic-receiver dispatch instead of surfacing a real
            // error to the Java side.
            return Value::Object(None);
        }
        if index >= num_slots {
            // Layout mismatch — return null/zero instead of reading past
            // the object. Resolve the class name + real declared field
            // count so the orchestrator can spot undersized-layout bugs
            // (see the matching `set_field` diagnostic below).
            let (class_name, real_fields) =
                match crate::gc::resolve_class_info(header.class_id.as_u32()) {
                    Some((name, n)) => (name, Some(n)),
                    None => ("<unresolved>".to_string(), None),
                };
            tracing::error!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                class_name = %class_name,
                real_field_count = ?real_fields,
                "gen_heap::get_field: out-of-bounds field read dropped \
                 (undersized object layout — class declares more fields \
                 than the object was allocated with)",
            );
            if std::env::var("CRATONVM_DBG_OOBFIELD").is_ok() {
                eprintln!(
                    "[OOBFIELD-READ] class={class_name} index={index} num_slots={num_slots}\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
            }
            return Value::Object(None);
        }
        // SAFETY: `obj_ref` points to a valid heap object and `index` is within
        // `num_slots` (checked above). `slot_ptr` computes
        // `obj_ref + HEADER_SIZE + index * SLOT_SIZE`, which is within the
        // object's allocated region. `read_slot` reads a `Value` from that address.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            read_slot(ptr)
        }
    }

    /// Set the value of a field at the given index.
    ///
    /// Automatically fires the write barrier for generational GC correctness.
    /// Callers do NOT need to call `write_barrier` separately.
    pub fn set_field(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        // KC16 SIGSEGV audit: runtime bounds check.  Silently drop writes
        // that fall past the object's declared layout (layout mismatch
        // between synthetic and real JDK class shapes) rather than
        // overflowing into the neighboring object.  Still panic on
        // clearly-corrupt headers.
        let header = self.get_header(obj_ref);
        let num_slots = header.num_slots as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                kind_byte = header.kind as u8,
                gc_flags = format!("{:x}", header.gc_flags),
                value = ?value,
                "gen_heap::set_field: suspect header (returning silently)",
            );
            // Rate-limited warn — do NOT panic. See matching comment in
            // `get_field` above.  Returning without writing keeps the
            // JVM alive so the Java side can recover or surface a real
            // exception.
            return;
        }
        if index >= num_slots {
            // Out-of-bounds writes are dropped rather than corrupting the
            // neighboring object, but log first — silently swallowing this
            // masks real layout-mismatch bugs in the caller.
            //
            // Triage diagnostic: resolve the raw `ClassId` to the class
            // NAME and its REAL declared field count via the VM-installed
            // hook. `real_field_count > num_slots` is the smoking gun for
            // an undersized synthetic-stub allocation: the object was
            // allocated with `num_slots` slots but the class actually
            // declares more — the same bug class as the `PrintStream`
            // 1-field-stub fix (commit cf1b478).
            let (class_name, real_fields) =
                match crate::gc::resolve_class_info(header.class_id.as_u32()) {
                    Some((name, n)) => (name, Some(n)),
                    None => ("<unresolved>".to_string(), None),
                };
            tracing::error!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                class_name = %class_name,
                real_field_count = ?real_fields,
                value = ?value,
                "gen_heap::set_field: out-of-bounds field write dropped \
                 (undersized object layout — class declares more fields \
                 than the object was allocated with)",
            );
            return;
        }
        debug_assert!(index < self.get_header(obj_ref).num_slots as usize);
        // SAFETY: Same as `get_field` — `index` is within `num_slots` so the
        // computed slot pointer is within the object's allocated region.
        // `write_slot` writes a `Value` at the computed address.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            write_slot(ptr, value);
        }
        self.write_barrier(obj_ref, value);
    }

    /// Get the value of a volatile field.
    ///
    /// JLS §17.7 requires reads and writes of volatile long / double
    /// (and any volatile-declared field) to be atomic. The on-heap
    /// `Value` slot is 16 bytes (8-byte tag + 8-byte payload), wider
    /// than any stable Rust atomic primitive on x86-64, so a SeqCst
    /// fence pair gives the required happens-before ordering but does
    /// **not** by itself guarantee atomicity of the slot's read —
    /// a concurrent writer could leave the tag and payload words
    /// momentarily out of sync, surfacing as a torn long/double
    /// (e.g. high 32 bits from the old write, low 32 from the new).
    ///
    /// We close the atomicity hole with the striped-mutex pool in
    /// [`crate::collector::volatile_stripe_lock`]: paired
    /// `set_field_volatile` writers acquire the same stripe, so the
    /// 16-byte read here either observes a fully old or fully new
    /// `Value`. Striping keeps unrelated volatile fields from
    /// serializing across the heap (the previous design used a single
    /// heap-wide mutex which became a global bottleneck).
    pub fn get_field_volatile(&self, obj_ref: ObjectRef, index: usize) -> Value {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let val = self.get_field(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        val
    }

    /// Set the value of a volatile field.
    ///
    /// Acquires the per-slot stripe lock from
    /// [`crate::collector::volatile_stripe_lock`] to make the 16-byte
    /// `Value` write appear atomic to a concurrent volatile reader.
    /// SeqCst fences provide the JMM happens-before edge. The write
    /// barrier still fires inside `set_field`. See
    /// [`Self::get_field_volatile`] for the full rationale.
    pub fn set_field_volatile(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.set_field(obj_ref, index, value); // barrier fires inside set_field
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Descriptor-aware get — normalizes the returned `Value` to the declared
    /// field type. See [`crate::heap::coerce_field_value_by_descriptor`].
    pub fn get_field_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj_ref, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Volatile descriptor-aware get.
    pub fn get_field_volatile_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        desc_byte: u8,
    ) -> Value {
        let raw = self.get_field_volatile(obj_ref, index);
        crate::heap::coerce_field_value_by_descriptor(raw, desc_byte)
    }

    /// Descriptor-aware set — normalizes the written `Value` to the declared
    /// field type before the underlying slot write.
    pub fn set_field_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field(obj_ref, index, coerced);
    }

    /// Volatile descriptor-aware set.
    pub fn set_field_volatile_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = crate::heap::coerce_field_value_by_descriptor(value, desc_byte);
        self.set_field_volatile(obj_ref, index, coerced);
    }

    // ----- Array access ------------------------------------------------------

    /// Get the length of an array.
    pub fn array_length(&self, obj_ref: ObjectRef) -> usize {
        let header = self.get_header(obj_ref);
        if header.kind != ObjectKind::Array {
            // Defensive hardening: native/bootstrap code can occasionally pass a
            // plain object into array helpers. Returning 0 keeps startup alive,
            // while diagnostics below pinpoint the exact caller and object shape.
            //
            // Rate-limit the diagnostic: long-running boots (e.g. Keycloak)
            // can hit this path tens of thousands of times. Emitting a full
            // `Backtrace::force_capture` each time produces hundreds of MB
            // of stderr and exhausts disk. Keep the first few one-line
            // warnings (with backtrace gated by `CRATONVM_GC_ARRAY_GUARD_BT`)
            // and silently return 0 thereafter.
            static GUARD_COUNT: AtomicUsize = AtomicUsize::new(0);
            const GUARD_LIMIT: usize = 5;
            let n = GUARD_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < GUARD_LIMIT {
                // Format the entire message into one String and emit via a
                // single locked stderr write. Multi-segment `eprintln!`
                // formatting can interleave with other threads' stderr
                // writes on Windows and trigger the rare
                // "Имеющийся буфер не подходит" (os error 1784) panic on the
                // intermediate write of large {:?} pretty-printed values.
                //
                // CRITICAL: we are formatting fields of a header that we
                // already know is suspect (kind != Array). The raw bytes
                // backing `kind` and `element_type` may be garbage — using
                // `{:?}` on `#[repr(u8)]` enums whose byte value is outside
                // the declared variants is undefined behavior and has been
                // observed to provoke a `capacity overflow` panic deep in
                // `core::fmt` (the Display jump table writing an arbitrary
                // huge slice into the format buffer).
                //
                // Read the raw bytes directly via the object pointer rather
                // than via `&ObjectHeader` field access, so we never go
                // through the unsound enum-value path. The header layout is
                // `#[repr(C)]`: class_id(u32) @0, kind(u8) @4, elem(u8) @5.
                let obj_ptr = obj_ref.as_ptr();
                // SAFETY: `obj_ref` was previously dereferenced via
                // `get_header` above without faulting, so `obj_ptr` plus
                // `HEADER_SIZE` bytes is readable. We read individual bytes
                // and a 4-byte u32 within that range, which is well-defined
                // even when the byte values do not correspond to valid enum
                // discriminants.
                let (kind_byte, elem_byte, class_id_raw, stored_len) = unsafe {
                    let class_id_raw = (obj_ptr as *const u32).read_unaligned();
                    let kind_byte = *obj_ptr.add(4);
                    let elem_byte = *obj_ptr.add(5);
                    let stored_len = (obj_ptr.add(12) as *const u32).read_unaligned();
                    (kind_byte, elem_byte, class_id_raw, stored_len)
                };
                let msg = if std::env::var_os("CRATONVM_GC_ARRAY_GUARD_BT").is_some() {
                    let bt = Backtrace::force_capture();
                    format!(
                        "[GC-ARRAY-GUARD] array_length(non-array): kind_byte={} class_id={} elem_byte={} stored_len={} obj={:p} (#{}/{})\nbacktrace:\n{}\n",
                        kind_byte,
                        class_id_raw,
                        elem_byte,
                        stored_len,
                        obj_ptr,
                        n + 1,
                        GUARD_LIMIT,
                        bt,
                    )
                } else {
                    format!(
                        "[GC-ARRAY-GUARD] array_length(non-array): kind_byte={} class_id={} elem_byte={} stored_len={} obj={:p} (#{}/{}; set CRATONVM_GC_ARRAY_GUARD_BT=1 for backtrace)\n",
                        kind_byte,
                        class_id_raw,
                        elem_byte,
                        stored_len,
                        obj_ptr,
                        n + 1,
                        GUARD_LIMIT,
                    )
                };
                // Atomic write via locked stderr; ignore errors (best-effort
                // diagnostic). Avoids the eprintln panic on Windows when the
                // writer's internal buffer is unhappy.
                use std::io::Write;
                let stderr = std::io::stderr();
                let mut handle = stderr.lock();
                let _ = handle.write_all(msg.as_bytes());
                if n + 1 == GUARD_LIMIT {
                    let _ = handle.write_all(
                        b"[GC-ARRAY-GUARD] suppressing further array_length(non-array) warnings\n",
                    );
                }
            }
            return 0;
        }
        // Guard against a corrupt synthetic header whose length field is
        // garbage. The only sound ceiling is the JVM's own limit: a Java
        // array cannot exceed `Integer.MAX_VALUE` elements, so any larger
        // value is provably a corrupt header. An earlier `1 << 24` cap also
        // rejected *legitimate* arrays larger than 16M elements (e.g. a
        // 2^26-int vector), clobbering their length to 0 and raising a
        // spurious ArrayIndexOutOfBoundsException on every access.
        const MAX_REASONABLE_ARRAY_LEN: u32 = i32::MAX as u32; // JVM array ceiling
        let raw_len = header.array_length;
        if raw_len > MAX_REASONABLE_ARRAY_LEN {
            static SUSPECT_LEN_WARN_COUNT: AtomicU64 = AtomicU64::new(0);
            let n = SUSPECT_LEN_WARN_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < 16 {
                // Format with raw byte values to avoid UB on Debug-formatting
                // a potentially-garbage `ArrayElementType` byte (same hazard
                // as the GC-ARRAY-GUARD path above).
                eprintln!(
                    "[GC-ARRAY-LEN-GUARD] suspect array_length={} (>1<<24) class_id={} elem_byte={} obj={:p} (n={})",
                    raw_len,
                    header.class_id.as_u32(),
                    header.element_type as u8,
                    obj_ref.as_ptr(),
                    n,
                );
            }
            return 0;
        }
        raw_len as usize
    }

    /// Bulk-read a char[] array into a `Vec<u16>`.
    ///
    /// Much faster than per-element `get_array_element` for reading Java
    /// String backing arrays — copies the raw 2-byte-per-element data directly.
    pub fn read_char_array_bulk(&self, obj_ref: ObjectRef) -> Vec<u16> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        debug_assert_eq!(header.element_type, ArrayElementType::Char);
        let len = header.array_length as usize;
        let mut out = vec![0u16; len];
        // SAFETY: `obj_ref` is a valid Char array with `len` elements (verified by
        // header assertions above). Each char element is 2 bytes, so `HEADER_SIZE`
        // to `HEADER_SIZE + len * 2` is within the allocation. The destination
        // buffer `out` is freshly allocated with the same length. The regions do
        // not overlap because `out` is on the Rust heap, not in the GC arena.
        unsafe {
            let src = obj_ref.as_ptr().add(HEADER_SIZE);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 2);
        }
        out
    }

    /// Get a raw pointer to the start of the array data region.
    ///
    /// This is the address immediately after the object header. The caller is
    /// responsible for knowing the element type and bounds.
    pub fn array_data_ptr(&self, obj_ref: ObjectRef) -> *mut u8 {
        // SAFETY: `obj_ref` is a valid heap object whose allocation includes
        // `HEADER_SIZE` plus the data region, so advancing by `HEADER_SIZE`
        // yields a pointer within the allocation. Caller is responsible for
        // bounds and element-type correctness.
        unsafe { obj_ref.as_ptr().add(HEADER_SIZE) }
    }

    /// Get an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn get_array_element(&self, obj_ref: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        if index >= header.array_length as usize {
            return Err(index as i32);
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // was allocated with sufficient data space for all elements after the header.
        // `read_prim_element` reads the correctly typed element at the given index.
        unsafe {
            let base = obj_ref.as_ptr().add(HEADER_SIZE);
            Ok(read_prim_element(base, index, header.element_type))
        }
    }

    /// Like `get_array_element`, but auto-unboxes values stored by native
    /// collections. See `Heap::get_array_element_unboxing` for details.
    pub fn get_array_element_unboxing(
        &self,
        obj_ref: ObjectRef,
        index: usize,
    ) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind, ObjectKind::Array);
        if index >= header.array_length as usize {
            return Err(index as i32);
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // data region is within the allocation.
        let value = unsafe {
            let base = obj_ref.as_ptr().add(HEADER_SIZE);
            read_prim_element(base, index, header.element_type)
        };
        if header.element_type == ArrayElementType::Reference {
            if let Value::Object(Some(obj)) = value {
                // The stored word is treated as an `ObjectRef`, but a stale or
                // garbage non-zero element could point anywhere. Validate it
                // against the heap arenas (region + alignment + header sanity)
                // before dereferencing it as an `ObjectHeader`. If it does not
                // look like a live heap object, skip the unboxing and return
                // the value as-is rather than performing a wild read.
                if self.is_object_address(obj.as_ptr() as usize).is_some() {
                    // SAFETY: `is_object_address` confirmed `obj` points to a
                    // valid object header inside one of this heap's arenas.
                    let obj_header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
                    if obj_header.class_id == AUTOBOX_CLASS_ID {
                        return Ok(self.get_field(obj, 0));
                    }
                }
            }
        }
        Ok(value)
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
        debug_assert_eq!(header.kind, ObjectKind::Array);
        if index >= header.array_length as usize {
            return Err(index as i32);
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // data region is within the allocation. `write_prim_element` writes at the
        // correct element offset. For reference arrays, autobox wrappers are
        // allocated on this heap and thus valid.
        unsafe {
            let base = obj_ref.as_ptr().add(HEADER_SIZE);
            // Compact ref arrays only store 8-byte pointers. If a non-Object
            // value is written (e.g. Value::Int from a native collection),
            // auto-box it into a 1-field wrapper object.
            if header.element_type == ArrayElementType::Reference {
                match value {
                    Value::Object(_) => {
                        write_prim_element(base, index, header.element_type, value);
                        // Write barrier: old-gen array storing young-gen ref
                        self.write_barrier(obj_ref, value);
                    }
                    _ => {
                        let wrapper = self.alloc_object(AUTOBOX_CLASS_ID, 1);
                        self.set_field(wrapper, 0, value);
                        let wrapper_val = Value::Object(Some(wrapper));
                        write_prim_element(base, index, header.element_type, wrapper_val);
                        // Write barrier: track the wrapper reference for gen GC
                        self.write_barrier(obj_ref, Value::Object(Some(wrapper)));
                    }
                }
            } else {
                write_prim_element(base, index, header.element_type, value);
            }
        }
        Ok(())
    }

    // ----- Write barrier -----------------------------------------------------

    /// Write barrier — called after every reference store into a heap object.
    ///
    /// Performs two functions:
    /// 1. **Card marking:** If source is in old gen and target is in young gen,
    ///    marks the card table entry dirty (for minor GC).
    /// 2. **SATB logging:** If concurrent marking is active, logs the *old*
    ///    reference value to the SATB queue (for concurrent GC correctness).
    ///
    /// Round-5 #11 (perf) — the previous implementation dereferenced the
    /// object header twice on the hot path: once to inspect
    /// `header.gc_flags` on the *source*, then a second time on the
    /// *target*. Each dereference is a cache-line load on a separate
    /// object and both are pure waste — the same information is already
    /// encoded in the address ranges of the old-gen arena. The card
    /// table caches those bounds (it must, in order to compute card
    /// indices), so we route the gen-check through it and skip both
    /// header reads entirely. The cost on the common path drops from
    /// "two L1/L2 misses + two flag tests" to "two integer range
    /// comparisons" — the same pattern that round-10 used for
    /// `post_write_barrier_rset` (per-thread region cache).
    #[inline]
    pub fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        // FAST PATH: only reference stores can produce a cross-gen edge.
        // Bail out BEFORE touching either object header for primitive/null
        // stores so the barrier cost on the common (primitive) path is
        // a single tag check.
        let target_ref = match stored_value {
            Value::Object(Some(r)) => r,
            _ => return,
        };

        let src_addr = obj.as_ptr() as usize;
        let dst_addr = target_ref.as_ptr() as usize;

        // Round-5 #11 fix: replace the two header dereferences with two
        // pure address-range comparisons against the cached old-gen
        // bounds. The card table already stores `base_addr` and
        // `region_size` for the old-gen arena (it has to — every dirty
        // entry is indexed by `(addr - base) / CARD_SIZE`). Reading
        // those two `usize` fields touches one already-hot cache line
        // (`self.card_table`) instead of two cold object headers.
        let old_base = self.card_table.base_addr();
        let old_end = old_base.wrapping_add(self.card_table.region_size());

        // Source must live in the old-gen arena. If it doesn't, this is
        // either a young→young store (no card needed) or a write into
        // GC-internal scratch space; either way, nothing to record.
        if src_addr < old_base || src_addr >= old_end {
            return;
        }

        // Target must live OUTSIDE the old-gen arena (i.e. in young) for
        // the edge to be cross-generational. Old→old refs are followed
        // by full-heap marking, not the card table.
        if dst_addr >= old_base && dst_addr < old_end {
            return;
        }

        // T5.5.2 — route through the thread-local batched dirty path
        // instead of taking the global card_table mutex on every
        // reference store. The offset is queued in this mutator's
        // per-thread buffer (no shared lock) and flushed into the shared
        // `pending_offsets` either when the buffer hits
        // THREAD_BUFFER_FLUSH_THRESHOLD entries or when the collector
        // drains at GC start. The shared `cards` bitmap is updated by
        // `drain_pending` while the GC holds the card_table mutex
        // exclusively.
        self.card_table.thread_local_dirty_addr(src_addr);
    }

    /// SATB write barrier — called BEFORE a reference field is overwritten.
    ///
    /// If concurrent marking is active, logs the old reference value to the
    /// global SATB queue so the concurrent marker won't miss live objects.
    ///
    /// This should be called by the interpreter/JIT before every putfield/putstatic
    /// that overwrites a reference-typed field.
    #[inline]
    pub fn satb_barrier(&self, old_value: Value) {
        // Fast path: check if concurrent marking is active
        if let Some(ref state) = self.concurrent_gc_state {
            if !state.is_marking_active() {
                return;
            }
        } else {
            return;
        }

        // Only log reference values
        let old_ref = match old_value {
            Value::Object(Some(r)) => r,
            _ => return,
        };

        // Log the old reference to the per-thread SATB buffer. The buffer
        // auto-flushes into the global queue every ~256 entries, so the
        // hot write-barrier path takes no shared lock in the common case.
        if let Some(ref satb) = self.satb_queue {
            crate::satb::satb_thread_local_log(satb, old_ref.as_ptr() as usize);
        }
    }

    /// Enable concurrent GC support by attaching shared SATB and state.
    pub fn enable_concurrent_gc(
        &mut self,
        satb_queue: Arc<SatbQueue>,
        gc_state: Arc<ConcurrentGcState>,
    ) {
        self.satb_queue = Some(satb_queue);
        self.concurrent_gc_state = Some(gc_state);
    }

    /// Round-5 fix (CRIT — UAF): expose the SATB queue handle so the
    /// VM can drain per-thread SATB buffers at safepoint entry. Returns
    /// `None` until `enable_concurrent_gc` has been called (i.e. until
    /// the concurrent old-gen collector is wired up).
    pub fn satb_queue_handle(&self) -> Option<Arc<SatbQueue>> {
        self.satb_queue.clone()
    }

    /// Get the old generation's base pointer and capacity (for creating a ConcurrentMarker).
    pub fn old_gen_info(&self) -> (usize, usize) {
        let og = self.old_gen.lock();
        (og.base_ptr() as usize, og.capacity())
    }

    /// Access the old generation directly (for concurrent sweep).
    /// Returns a lock guard.
    pub fn old_gen_lock(&self) -> parking_lot::MutexGuard<'_, OldGen> {
        self.old_gen.lock()
    }

    // ----- GC ----------------------------------------------------------------

    /// Returns true when the young generation should be collected.
    pub fn needs_gc(&self) -> bool {
        self.young_from.lock().used() >= *self.young_gc_threshold.lock()
    }

    /// Total bytes currently allocated across young and old generations.
    pub fn allocated_bytes(&self) -> usize {
        self.young_from.lock().used() + self.old_gen.lock().used()
    }

    /// Check if the old generation is above 75% capacity.
    pub fn old_gen_needs_gc(&self) -> bool {
        let og = self.old_gen.lock();
        og.used() >= og.capacity() * 75 / 100
    }

    /// Run a minor garbage collection cycle.
    ///
    /// Copies live young-gen objects to young to-space (or promotes to old gen
    /// if they have survived enough cycles). Scans dirty card table entries
    /// for old→young references.
    ///
    /// Returns GC statistics and a pointer remapping table.
    /// Like [`collect_garbage`] but keeps dead finalizable objects alive so
    /// their `finalize()` method can be invoked.  Returns the GC result and
    /// the *new* (post-GC) addresses of dead finalizable objects.
    pub fn collect_garbage_with_finalizers(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        self.collect_garbage_inner(roots, finalizer_addrs, monitors)
    }

    pub fn collect_garbage(&self, roots: &mut [ObjectRef], monitors: &dyn MonitorCleanup) -> GcResult {
        self.collect_garbage_inner(roots, &[], monitors).0
    }

    fn collect_garbage_inner(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        // Phase 6 #1: spin-yield until every live `SafepointToken` has
        // dropped. While a kernel is reading a JVM array on the GPU, we
        // must not move that array — a single token held anywhere on
        // any thread defers this collection until it is released.
        // No-op when the `gpu-offload` feature is off.
        crate::vm_heap::wait_for_gpu_critical_drain();

        // SAFETY: If any thread is currently inside a JIT call, we MUST NOT
        // run a *moving* collection. JIT frames hold raw object pointers in
        // their spill slots / registers which are NOT precisely described by
        // an oop map — the GC sees them only through the *conservative*
        // stack scan (`conservative_roots::scan_active_jit_frames`). A Cheney
        // copy would relocate those objects but it cannot safely rewrite a
        // conservatively-discovered slot: a stack word that merely *looks*
        // like a heap pointer might actually be an `i64`, and rewriting it
        // would corrupt the mutator's data.
        //
        // Previously the collector simply *skipped* the cycle entirely while
        // any JIT frame was live. Because a busy program always has a JIT
        // frame on the stack, the young gen would fill and the process would
        // OOM (the user-visible "young gen exhausted" abort).
        //
        // The fix: when JIT frames are active, run a **non-moving**
        // mark-sweep of the young generation instead of skipping. A
        // non-moving collection never relocates an object, so every JIT
        // spill slot stays valid no matter what it points at. Marking is
        // precise enough — it starts from the full root set (interpreter
        // frames, statics, JNI handles, *and* the conservative JIT-frame
        // roots that the caller already folded into `roots`) plus the
        // old→young dirty-card references — and dead young objects are
        // reclaimed into the from-space free list. Conservative false
        // positives only over-retain; they can never free a live object.
        //
        // This reclaims memory while JIT frames are live and eliminates the
        // OOM. A precise compacting collection still runs once every JIT
        // call has returned (quiescence ends), so fragmentation introduced
        // by the non-moving sweep is transient.
        if crate::gc_quiescence::is_active() {
            tracing::debug!(
                "JIT frames are active (depth={}) — running non-moving \
                 young-gen mark-sweep (compaction deferred until quiescence \
                 ends).",
                crate::gc_quiescence::depth(),
            );
            let result = self.sweep_young_non_moving(roots, finalizer_addrs);
            return result;
        }

        let mut young_from = self.young_from.lock();
        let mut young_to = self.young_to.lock();
        let mut old_gen = self.old_gen.lock();

        // T5.5.2 (HIGH-1 fix): drain every mutator's thread-local card
        // buffer into the authoritative bitmap BEFORE the dirty-card
        // scan. We're inside the STW collector — the safepoint sync
        // barrier (which the caller holds before invoking
        // `collect_garbage`) guarantees every mutator either parked at
        // a safepoint (flushing its buffer on the way in) or completed
        // its barrier call before we got here. `flush_all` drains the
        // calling (collector) thread's buffer for completeness, then
        // `drain_pending` folds every queued offset — including those
        // submitted by mutators via auto-flush or safepoint-entry
        // flush — into the `cards`/`dirty_cards` bitmap.
        self.card_table.flush_all();
        self.card_table.drain_pending();
        let card_table: &CardTable = &self.card_table;

        let bytes_before = young_from.used();
        let mut objects_copied: usize = 0;
        // CRIT-P2 fix: use FxHashMap to avoid SipHash overhead on every
        // forwarded pointer (N hash ops per GC for N live objects).
        // Converted back to std HashMap at the end for public-API
        // compatibility (`GcResult.pointer_map` and `MonitorCleanup`).
        let mut pointer_map: FxHashMap<usize, usize> = FxHashMap::default();
        // CRIT-P2 fix: explicit worklist of promoted (old-gen) objects awaiting
        // a scan. Replaces the O(promoted^2) filter loop that previously
        // rebuilt `Vec<unscanned>` from `pointer_map.values()` per iteration.
        // Populated by `forward_object` whenever an object is promoted to
        // old gen; popped by the alternating Cheney scan below.
        let mut promoted_worklist: Vec<*mut u8> = Vec::new();

        // Collect additional roots from dirty cards in old gen
        let mut extra_roots: Vec<(ObjectRef, usize, usize)> = Vec::new();
        // (old_gen_obj, slot_index, _) for each old→young reference slot
        Self::scan_dirty_cards(&card_table, &old_gen, &young_from, &mut extra_roots);

        // Phase 1: Forward all root objects
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if !young_from.contains(old_ptr) {
                continue; // Skip roots not in young gen (e.g., old gen objects)
            }
            let new_ptr = Self::forward_object(
                &young_from,
                &mut young_to,
                &mut old_gen,
                old_ptr,
                &mut objects_copied,
                &mut pointer_map,
                &mut promoted_worklist,
            );
            // SAFETY: `new_ptr` was returned by `forward_object`, which allocated
            // space in young_to or old_gen and copied a valid object there.
            *root = unsafe { ObjectRef::from_raw(new_ptr) };
        }

        // Phase 1b: Forward old→young references from dirty cards
        // Ref arrays use compact 8-byte pointers; object fields use 16-byte Value.
        for &(old_obj, slot_idx, _) in &extra_roots {
            // SAFETY: `old_obj` is a live old-gen ObjectRef collected from dirty card
            // scanning, so its pointer targets a valid ObjectHeader.
            let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
            let is_ref_array = header.kind == ObjectKind::Array
                && header.element_type == ArrayElementType::Reference;

            if is_ref_array {
                // SAFETY: `old_obj` is a valid old-gen object and `slot_idx` was collected
                // from dirty card scanning (within array bounds). Pointer arithmetic stays
                // within the object's allocation.
                let slot_ptr = unsafe {
                    old_obj
                        .as_ptr()
                        .add(HEADER_SIZE + slot_idx * REF_ELEMENT_SIZE)
                };
                // SAFETY: `slot_ptr` points to a valid 8-byte ref element within the array.
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    let ref_ptr = raw as usize as *mut u8;
                    if young_from.contains(ref_ptr) {
                        let new_ptr = Self::forward_object(
                            &young_from,
                            &mut young_to,
                            &mut old_gen,
                            ref_ptr,
                            &mut objects_copied,
                            &mut pointer_map,
                            &mut promoted_worklist,
                        );
                        // SAFETY: Writing the forwarded pointer back to the same valid slot.
                        unsafe { std::ptr::write(slot_ptr as *mut u64, new_ptr as u64) };
                    }
                }
            } else {
                // SAFETY: `old_obj` is a valid old-gen object, `slot_idx` is within
                // `num_slots` (from dirty card scanning). Arithmetic stays in bounds.
                let slot_ptr = unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: `slot_ptr` points to a valid `Value`-sized slot in the object.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if young_from.contains(ref_ptr) {
                        let new_ptr = Self::forward_object(
                            &young_from,
                            &mut young_to,
                            &mut old_gen,
                            ref_ptr,
                            &mut objects_copied,
                            &mut pointer_map,
                            &mut promoted_worklist,
                        );
                        // SAFETY: `new_ptr` is a valid forwarded allocation.
                        let new_value =
                            Value::Object(Some(unsafe { ObjectRef::from_raw(new_ptr) }));
                        // SAFETY: Writing updated Value back to the same valid slot.
                        unsafe { std::ptr::write(slot_ptr as *mut Value, new_value) };
                    }
                }
            }
        }

        // Phase 2 + 2b: Combined Cheney scan and promoted object scan.
        //
        // We alternate between scanning young_to (standard Cheney) and
        // scanning newly promoted old-gen objects until both are fully
        // processed. This is necessary because:
        //   - Scanning a young_to object may forward a reference that gets
        //     promoted to old gen (needs promoted scan).
        //   - Scanning a promoted old-gen object may forward a reference
        //     that lands in young_to (needs Cheney scan) or gets promoted
        //     itself (needs another promoted scan iteration).
        let mut scan_cursor: usize = 0;
        // CRIT-P2 fix: FxHashSet (replaces std HashSet/SipHash) for cheap
        // dedup of promoted-object scans.
        let mut scanned_promoted: FxHashSet<usize> = FxHashSet::default();
        // Accumulate old-gen addresses needing card dirty marks after GC.
        // These arise when a promoted object contains a reference that was
        // forwarded to young to-space (old→young cross-gen reference).
        let mut deferred_dirty_cards: Vec<usize> = Vec::new();

        loop {
            let mut made_progress = false;

            // Cheney scan: process any unscanned objects in young_to
            while scan_cursor < young_to.used() {
                made_progress = true;
                // SAFETY: `scan_cursor` is within `young_to.used()` and advances by
                // `total_size` per object, so this points to a valid object header
                // in the young to-space arena.
                let obj_ptr = unsafe { young_to.base_ptr_mut().add(scan_cursor) };
                // SAFETY: `obj_ptr` points to a copied/promoted object with a valid header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let total_size = gen_object_total_size(header);

                // Scan ref slots: ref arrays use compact 8-byte pointers,
                // object fields use 16-byte Value.
                if header.kind == ObjectKind::Array {
                    if header.element_type == ArrayElementType::Reference {
                        for i in 0..header.array_length as usize {
                            // SAFETY: `i` is within `array_length`, so the offset is
                            // within the array's data region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                            // SAFETY: `s_ptr` points to a valid 8-byte ref element.
                            let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                            if raw != 0 {
                                let ref_ptr = raw as usize as *mut u8;
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from,
                                        &mut young_to,
                                        &mut old_gen,
                                        ref_ptr,
                                        &mut objects_copied,
                                        &mut pointer_map,
                                        &mut promoted_worklist,
                                    );
                                    // SAFETY: Writing forwarded pointer back to the same valid slot.
                                    unsafe {
                                        std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    for slot_idx in 0..header.num_slots as usize {
                        // SAFETY: `slot_idx` is within `num_slots`, so the offset is
                        // within the object's field region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                        // SAFETY: `s_ptr` points to a valid `Value`-sized slot.
                        let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                        if let Value::Object(Some(ref_obj)) = value {
                            let ref_ptr = ref_obj.as_ptr();
                            if young_from.contains(ref_ptr) {
                                let new_ref_ptr = Self::forward_object(
                                    &young_from,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                );
                                // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                let new_value = Value::Object(Some(unsafe {
                                    ObjectRef::from_raw(new_ref_ptr)
                                }));
                                // SAFETY: Writing updated Value back to the same valid slot.
                                unsafe { std::ptr::write(s_ptr as *mut Value, new_value) };
                            }
                        }
                    }
                }

                scan_cursor += total_size;
            }

            // Promoted object scan: process any unscanned promoted objects.
            //
            // CRIT-P2 fix: drain the explicit `promoted_worklist` instead of
            // rebuilding a Vec from `pointer_map.values()` on every outer
            // iteration (which was O(promoted^2) until fixpoint). Every
            // `forward_object` call that promotes an object to old gen
            // pushes its new address onto `promoted_worklist`, so popping
            // here is true Cheney-style O(promoted) scanning.
            //
            // `forward_object` only pushes on a fresh promotion (not on the
            // already-forwarded re-encounter path), so the worklist holds
            // each promoted address at most once. The `scanned_promoted`
            // check below is kept as a defensive idempotency guard.
            while let Some(obj_ptr) = promoted_worklist.pop() {
                if !scanned_promoted.insert(obj_ptr as usize) {
                    // already scanned this cycle
                    continue;
                }
                made_progress = true;
                // SAFETY: `obj_ptr` is a promoted object in old gen (it was
                // pushed only when `forward_object` confirmed the allocation
                // landed in `old_gen`), with a valid copied header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

                // Scan ref slots: ref arrays use compact 8-byte pointers,
                // object fields use 16-byte Value.
                if header.kind == ObjectKind::Array {
                    if header.element_type == ArrayElementType::Reference {
                        for i in 0..header.array_length as usize {
                            // SAFETY: `i` < `array_length`; offset within array data region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                            // SAFETY: `s_ptr` points to a valid 8-byte ref element.
                            let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                            if raw != 0 {
                                let ref_ptr = raw as usize as *mut u8;
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from,
                                        &mut young_to,
                                        &mut old_gen,
                                        ref_ptr,
                                        &mut objects_copied,
                                        &mut pointer_map,
                                        &mut promoted_worklist,
                                    );
                                    // SAFETY: Writing forwarded pointer back to the same valid slot.
                                    unsafe {
                                        std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64);
                                    }
                                    // Mark card dirty if the forwarded ref landed in young
                                    // to-space — this old→young cross-gen reference must be
                                    // visible to the NEXT minor GC's dirty card scan.
                                    if !old_gen.contains(new_ref_ptr) {
                                        deferred_dirty_cards.push(obj_ptr as usize);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    for slot_idx in 0..header.num_slots as usize {
                        // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                        let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                        if let Value::Object(Some(ref_obj)) = value {
                            let ref_ptr = ref_obj.as_ptr();
                            if young_from.contains(ref_ptr) {
                                let new_ref_ptr = Self::forward_object(
                                    &young_from,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                );
                                // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                let new_value = Value::Object(Some(unsafe {
                                    ObjectRef::from_raw(new_ref_ptr)
                                }));
                                // SAFETY: Writing updated Value back to the same valid slot.
                                unsafe { std::ptr::write(s_ptr as *mut Value, new_value) };
                                // Mark card dirty if the forwarded ref landed in young
                                // to-space — this old→young cross-gen reference must be
                                // visible to the NEXT minor GC's dirty card scan.
                                if !old_gen.contains(new_ref_ptr) {
                                    card_table.mark_dirty(obj_ptr as usize);
                                }
                            }
                        }
                    }
                }
            }

            if !made_progress {
                break;
            }
        }

        // Phase 2.5: Resurrect dead finalizable objects — forward any
        // unreachable finalizable objects so finalize() can access them.
        let mut dead_finalizers = Vec::new();
        for &old_addr in finalizer_addrs {
            if pointer_map.contains_key(&old_addr) {
                continue; // already reachable — skip
            }
            let old_ptr = old_addr as *mut u8;
            if !young_from.contains(old_ptr) {
                continue; // not in young gen (e.g. old gen or invalid)
            }
            let new_ptr = Self::forward_object(
                &young_from,
                &mut young_to,
                &mut old_gen,
                old_ptr,
                &mut objects_copied,
                &mut pointer_map,
                &mut promoted_worklist,
            );
            dead_finalizers.push(new_ptr as usize);
        }

        // Phase 2.5b: Continue Cheney scan for resurrected objects and their refs
        if !dead_finalizers.is_empty() {
            loop {
                let mut made_progress = false;
                while scan_cursor < young_to.used() {
                    made_progress = true;
                    // SAFETY: `scan_cursor` is within `young_to.used()`; points to a valid copied object header.
                    let obj_ptr = unsafe { young_to.base_ptr_mut().add(scan_cursor) };
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    let total_size = gen_object_total_size(header);
                    if header.kind == ObjectKind::Array {
                        if header.element_type == ArrayElementType::Reference {
                            for i in 0..header.array_length as usize {
                                // SAFETY: `i` < `array_length`; offset within array data region.
                                let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                                let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                                if raw != 0 {
                                    let ref_ptr = raw as usize as *mut u8;
                                    if young_from.contains(ref_ptr) {
                                        let new_ref_ptr = Self::forward_object(
                                            &young_from, &mut young_to, &mut old_gen,
                                            ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                        );
                                        // SAFETY: Writing forwarded pointer back to the same valid ref-array slot.
                                        unsafe { std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64); }
                                    }
                                }
                            }
                        }
                    } else {
                        for slot_idx in 0..header.num_slots as usize {
                            // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                            let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                            if let Value::Object(Some(ref_obj)) = value {
                                let ref_ptr = ref_obj.as_ptr();
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from, &mut young_to, &mut old_gen,
                                        ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                    );
                                    // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                    let new_value = Value::Object(Some(unsafe {
                                        ObjectRef::from_raw(new_ref_ptr)
                                    }));
                                    // SAFETY: Writing updated Value back to the same valid slot.
                                    unsafe { std::ptr::write(s_ptr as *mut Value, new_value); }
                                }
                            }
                        }
                    }
                    scan_cursor += total_size;
                }
                // Also scan promoted objects from resurrection — drain the
                // shared worklist (see CRIT-P2 note above on the main scan).
                while let Some(obj_ptr) = promoted_worklist.pop() {
                    if !scanned_promoted.insert(obj_ptr as usize) {
                        continue;
                    }
                    made_progress = true;
                    // SAFETY: `obj_ptr` is a promoted old-gen object with a valid copied header.
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    if header.kind == ObjectKind::Array {
                        if header.element_type == ArrayElementType::Reference {
                            for i in 0..header.array_length as usize {
                                // SAFETY: `i` < `array_length`; offset within array data region.
                                let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                                let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                                if raw != 0 {
                                    let ref_ptr = raw as usize as *mut u8;
                                    if young_from.contains(ref_ptr) {
                                        let new_ref_ptr = Self::forward_object(
                                            &young_from, &mut young_to, &mut old_gen,
                                            ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                        );
                                        // SAFETY: Writing forwarded pointer back to the same valid ref-array slot.
                                        unsafe { std::ptr::write(s_ptr as *mut u64, new_ref_ptr as u64); }
                                        if !old_gen.contains(new_ref_ptr) {
                                            card_table.mark_dirty(obj_ptr as usize);
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        for slot_idx in 0..header.num_slots as usize {
                            // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                            let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                            let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                            if let Value::Object(Some(ref_obj)) = value {
                                let ref_ptr = ref_obj.as_ptr();
                                if young_from.contains(ref_ptr) {
                                    let new_ref_ptr = Self::forward_object(
                                        &young_from, &mut young_to, &mut old_gen,
                                        ref_ptr, &mut objects_copied, &mut pointer_map, &mut promoted_worklist,
                                    );
                                    // SAFETY: `new_ref_ptr` is a valid forwarded allocation.
                                    let new_value = Value::Object(Some(unsafe {
                                        ObjectRef::from_raw(new_ref_ptr)
                                    }));
                                    // SAFETY: Writing updated Value back to the same valid slot.
                                    unsafe { std::ptr::write(s_ptr as *mut Value, new_value); }
                                    if !old_gen.contains(new_ref_ptr) {
                                        deferred_dirty_cards.push(obj_ptr as usize);
                                    }
                                }
                            }
                        }
                    }
                }
                if !made_progress {
                    break;
                }
            }
        }

        let bytes_copied = young_to.used();

        // Phase H (RH.1): compute promotion / young-copy stats from the
        // pointer_map BEFORE major_gc appends its own entries below.
        // Every entry at this point is a minor-GC forward: new_addr is
        // either in old_gen (promoted) or in young_to-space (copied).
        // Walking the map once avoids an expensive counter plumbed
        // through `forward_object`'s 12 call sites.
        let mut bytes_promoted_cycle: u64 = 0;
        let mut objects_promoted_cycle: u64 = 0;
        let mut bytes_copied_young_cycle: u64 = 0;
        let mut objects_copied_young_cycle: u64 = 0;
        for &new_addr in pointer_map.values() {
            let new_ptr = new_addr as *const u8;
            // SAFETY: `new_addr` is a pointer returned by forward_object,
            // which either allocated in young_to or in old_gen. Both
            // allocations begin with a valid ObjectHeader.
            let header = unsafe { &*(new_addr as *const ObjectHeader) };
            // `gen_object_total_size` is the sum of HEADER_SIZE and the
            // variable-length object body, computed from the header
            // exactly as the copy path does.
            let sz = gen_object_total_size(header) as u64;
            if old_gen.contains(new_ptr) {
                bytes_promoted_cycle += sz;
                objects_promoted_cycle += 1;
            } else {
                bytes_copied_young_cycle += sz;
                objects_copied_young_cycle += 1;
            }
        }

        // Phase 3: Clear card table and reset young from-space
        card_table.clear_all();
        // Re-mark cards for promoted objects that still reference young gen.
        // These old→young cross-gen references were established during the
        // Phase 2 promoted-object scan and must be visible to the next GC.
        // Use the bulk API so the card-table lock is acquired once for the
        // entire batch rather than once per deferred address.
        card_table.mark_dirty_bulk(&deferred_dirty_cards);
        young_from.reset();

        // CRIT-P2 fix: convert the internal FxHashMap to the std HashMap
        // expected by `MonitorCleanup::remap_after_gc` (defined in
        // `collector.rs`) and `GcResult.pointer_map` (the public-API field
        // in `gc.rs`). The conversion is a single O(N) walk — cheap
        // compared to N SipHash operations across the Cheney scan.
        let mut pointer_map: HashMap<usize, usize> = pointer_map.into_iter().collect();

        // Phase 4: Remap monitors and swap young spaces
        monitors.remap_after_gc(&pointer_map);
        std::mem::swap(&mut *young_from, &mut *young_to);

        // Phase 5: Check if old gen is getting full — trigger major GC (mark-compact)
        let major_ran = if old_gen.used() >= old_gen.capacity() * 75 / 100 {
            tracing::debug!(
                "Old gen at {}% — running major GC (mark-compact)",
                old_gen.used() * 100 / old_gen.capacity(),
            );
            let old_used_before = old_gen.used();
            let compact_map = Self::major_gc(roots, &young_from, &mut old_gen);
            // Merge old-gen compaction relocations into the overall pointer map
            // so the VM can update external roots (statics, JNI, string pool, etc.)
            pointer_map.extend(compact_map);
            let old_used_after = old_gen.used();
            // `used` can rise after compaction if the compactor's metadata
            // overhead exceeds reclaimed garbage; clamp with saturating_sub.
            self.stats
                .bytes_freed_old
                .fetch_add(old_used_before.saturating_sub(old_used_after) as u64, Ordering::Relaxed);
            true
        } else {
            false
        };

        let bytes_freed = bytes_before.saturating_sub(bytes_copied);

        // Phase 6: Adaptive heap expansion.
        // If GC didn't reclaim enough space (less than 25% of capacity freed),
        // expand the young gen to avoid GC thrashing.
        //
        // After swap: young_from has live data, young_to is empty (was reset).
        // We can only safely grow young_to (empty arena). This means the
        // expansion takes effect on the NEXT GC cycle — when live objects are
        // copied into the now-larger to-space, which then becomes from-space.
        let freed_percent = if bytes_before > 0 {
            bytes_freed * 100 / bytes_before
        } else {
            100
        };
        if freed_percent < GC_EXPANSION_THRESHOLD_PERCENT {
            let current_cap = young_to.capacity();
            let new_cap = (current_cap * 2).min(self.max_young_semi_size);
            if new_cap > current_cap {
                tracing::debug!(
                    "GC: low reclamation ({}% freed) — expanding young to-space {} → {} bytes",
                    freed_percent,
                    current_cap,
                    new_cap,
                );
                // young_to is empty after reset+swap, safe to grow
                young_to.grow(new_cap);
                // Update threshold based on the upcoming larger from-space
                *self.young_gc_threshold.lock() = new_cap * YOUNG_GC_THRESHOLD_PERCENT / 100;
            }
        }

        // Phase H (RH.1): commit per-cycle counters to the lifetime
        // accumulator. Do this at the end so tests can observe GC
        // statistics after the call returns.
        self.stats.minor_gc_count.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_promoted
            .fetch_add(bytes_promoted_cycle, Ordering::Relaxed);
        self.stats
            .objects_promoted
            .fetch_add(objects_promoted_cycle, Ordering::Relaxed);
        self.stats
            .bytes_copied_young
            .fetch_add(bytes_copied_young_cycle, Ordering::Relaxed);
        self.stats
            .objects_copied_young
            .fetch_add(objects_copied_young_cycle, Ordering::Relaxed);
        if major_ran {
            self.stats.major_gc_count.fetch_add(1, Ordering::Relaxed);
        }

        (
            GcResult {
                stats: crate::gc::GcStats {
                    objects_copied,
                    bytes_copied,
                    bytes_freed,
                },
                pointer_map,
            },
            dead_finalizers,
        )
    }

    /// Non-moving young-generation mark-sweep, used when JIT frames are
    /// active and a moving (Cheney) collection would be unsafe.
    ///
    /// Unlike [`Self::collect_garbage_inner`]'s copying collector, this
    /// **never relocates an object**. Survivors keep their exact
    /// addresses, so every raw pointer held in a JIT spill slot or
    /// register stays valid regardless of whether the GC could describe
    /// it precisely. Dead young objects are reclaimed into the
    /// from-space arena's free list (see [`Arena::add_free_block`]).
    ///
    /// ## Correctness
    ///
    /// Marking must be **complete** — a non-moving sweep frees every
    /// young object that is not marked, so a missed root would free a
    /// live object. The root set is:
    ///
    ///   * `roots` — every precise VM root the caller gathered
    ///     (interpreter frame locals/stacks, statics, JNI handles, class
    ///     locks, autobox caches, …) **plus** the conservative
    ///     JIT-frame roots that `collect_roots` already appended via
    ///     `conservative_roots::scan_active_jit_frames`.
    ///   * old→young references discovered by scanning dirty cards.
    ///   * `finalizer_addrs` — finalizable objects kept alive so their
    ///     `finalize()` can run.
    ///
    /// A conservative false-positive root only over-retains an object;
    /// it can never cause a live object to be freed. Because nothing
    /// moves, a stack word that merely *looks* like a pointer is never
    /// rewritten, so the mutator's non-pointer data is never corrupted.
    ///
    /// Returns an empty pointer map (no object moved) and the list of
    /// resurrected dead-finalizer addresses (unchanged, since they too
    /// stay in place).
    fn sweep_young_non_moving(
        &self,
        roots: &[ObjectRef],
        finalizer_addrs: &[usize],
    ) -> (GcResult, Vec<usize>) {
        let mut young_from = self.young_from.lock();
        let old_gen = self.old_gen.lock();

        // Fold every mutator's thread-local card buffer into the bitmap
        // before scanning dirty cards (same protocol as the moving path).
        self.card_table.flush_all();
        self.card_table.drain_pending();

        let bytes_before = young_from.used();
        let from_base = young_from.base_ptr() as usize;
        let from_end = from_base + young_from.used();

        // Helper: is `addr` the start of a young from-space object?
        let in_young = |addr: usize| -> bool {
            addr >= from_base && addr < from_end && (addr & 0x7) == 0
        };

        // ----- Mark phase -------------------------------------------------
        //
        // BFS over young-gen objects. The worklist holds young object
        // pointers that have been marked but not yet scanned. Marking an
        // old-gen object is unnecessary for a young collection, but we
        // still traverse *through* an old-gen object if a dirty card says
        // it may reference young gen (handled below via the dirty-card
        // seed). Marking uses `GC_FLAG_MARKED` in the object header.
        let mut worklist: Vec<*mut u8> = Vec::new();

        // mark_if_young: mark a candidate young pointer and enqueue it.
        // SAFETY contract: `ptr` is only dereferenced after `in_young`
        // confirms it lands inside the live from-space region.
        let mut mark_young = |ptr: *mut u8, worklist: &mut Vec<*mut u8>| {
            let addr = ptr as usize;
            if !in_young(addr) {
                return;
            }
            // SAFETY: `in_young` confirmed `addr` is an 8-byte-aligned
            // address inside the live from-space region, so reading an
            // ObjectHeader there is valid.
            let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
            // Reject implausible headers — a conservative root may point
            // at a non-object word. `is_object_address`-style sanity.
            // Multi-array reloc fix (2026-05-22): `num_slots` mirrors
            // `array_length` for arrays, so a legitimate 256 MB int[] has
            // num_slots = 2^26 > 1<<24 and would be skipped (then swept as
            // garbage, despite being a live root). Gate num_slots on
            // non-arrays; bound array_length at the JVM ceiling.
            let kind_byte = header.kind as u8;
            let is_array = header.kind == ObjectKind::Array;
            if kind_byte > 1
                || (!is_array && header.num_slots > (1 << 24))
                || (is_array && header.array_length > i32::MAX as u32)
            {
                return;
            }
            if header.gc_flags & GC_FLAG_MARKED == 0 {
                header.gc_flags |= GC_FLAG_MARKED;
                worklist.push(ptr);
            }
        };

        // Seed: precise + conservative roots gathered by the caller.
        for root in roots.iter() {
            mark_young(root.as_ptr(), &mut worklist);
        }

        // Seed: finalizable objects — keep them alive so finalize() runs.
        for &addr in finalizer_addrs {
            mark_young(addr as *mut u8, &mut worklist);
        }

        // Seed: old→young references from dirty cards. Reuse the existing
        // dirty-card scanner, which yields (old_obj, slot_idx, _) tuples;
        // we read the referenced young object out of each slot.
        let mut extra_roots: Vec<(ObjectRef, usize, usize)> = Vec::new();
        Self::scan_dirty_cards(&self.card_table, &old_gen, &young_from, &mut extra_roots);
        // `scan_dirty_cards` *consumes* the dirty-card tracking list. Since
        // this collection does not move young objects, every old→young
        // reference it found is still valid and the card covering it must
        // stay dirty for the NEXT collection. Re-dirty each old object's
        // card after we have read its slots below.
        let mut redirty_cards: Vec<usize> = Vec::with_capacity(extra_roots.len());
        for &(old_obj, slot_idx, _) in &extra_roots {
            // Card covering this old object must remain dirty for the
            // next GC (the old→young edge survives an in-place sweep).
            redirty_cards.push(old_obj.as_ptr() as usize);
            // SAFETY: `old_obj` is a live old-gen object from dirty-card
            // scanning; its header is valid.
            let header = unsafe { &*(old_obj.as_ptr() as *const ObjectHeader) };
            if header.kind == ObjectKind::Array
                && header.element_type == ArrayElementType::Reference
            {
                // SAFETY: `slot_idx` is within array bounds (from card scan).
                let slot_ptr = unsafe {
                    old_obj
                        .as_ptr()
                        .add(HEADER_SIZE + slot_idx * REF_ELEMENT_SIZE)
                };
                // SAFETY: `slot_ptr` is a valid 8-byte ref element.
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    mark_young(raw as usize as *mut u8, &mut worklist);
                }
            } else {
                // SAFETY: `slot_idx` is within `num_slots` (from card scan).
                let slot_ptr =
                    unsafe { old_obj.as_ptr().add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // SAFETY: `slot_ptr` is a valid Value-sized slot.
                let value = unsafe { std::ptr::read(slot_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    mark_young(ref_obj.as_ptr(), &mut worklist);
                }
            }
        }

        // Restore the dirty cards consumed by `scan_dirty_cards` so the
        // next collection still sees these old→young references.
        self.card_table.mark_dirty_bulk(&redirty_cards);

        // BFS: transitively mark every young object reachable from a root.
        while let Some(obj_ptr) = worklist.pop() {
            // SAFETY: `obj_ptr` was validated by `mark_young` before being
            // pushed — it is a sane young-gen object header.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data.
                        let s_ptr =
                            unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        // SAFETY: `s_ptr` is a valid 8-byte ref element.
                        let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                        if raw != 0 {
                            mark_young(raw as usize as *mut u8, &mut worklist);
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` < `num_slots`; offset within field region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    // SAFETY: `s_ptr` is a valid Value-sized slot.
                    let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        mark_young(ref_obj.as_ptr(), &mut worklist);
                    }
                }
            }
        }

        // ----- Sweep phase ------------------------------------------------
        //
        // Walk the from-space linearly, skipping holes already on the free
        // list (exactly as `OldGen::walk_objects` does — a hole's stale
        // bytes must never be parsed as an object). Every *unmarked*
        // object is dead: zero it and return its span to the free list.
        // Every marked object is a survivor: clear the mark and leave it
        // exactly where it is.
        let existing_free = young_from.free_blocks_sorted();
        let mut dead_regions: Vec<(usize, usize)> = Vec::new();
        let mut bytes_swept: usize = 0;
        let mut objects_swept: usize = 0;
        let mut objects_live: usize = 0;

        let mut cursor: usize = 0;
        let used = young_from.used();
        let mut free_iter = existing_free.iter().peekable();
        while cursor < used {
            // If `cursor` is the start of a known free block, skip it.
            if let Some(&&(off, sz)) = free_iter.peek() {
                if cursor == off {
                    cursor += sz;
                    free_iter.next();
                    continue;
                }
            }
            // SAFETY: `cursor` is within `used`; the from-space region
            // `[base, base+used)` is backed by mapped, allocated memory.
            let obj_ptr = unsafe { (from_base + cursor) as *mut u8 };
            let header = unsafe { &mut *(obj_ptr as *const ObjectHeader as *mut ObjectHeader) };
            let total_size = gen_object_total_size(header);
            // Defensive: a corrupt / zero-size header would desynchronise
            // the linear walk. Stop rather than risk freeing live data.
            if total_size < HEADER_SIZE || cursor + total_size > used {
                tracing::warn!(
                    "non-moving sweep: stopping walk at offset {} — implausible \
                     object size {} (kind={:?}, num_slots={}, array_len={})",
                    cursor,
                    total_size,
                    header.kind,
                    header.num_slots,
                    header.array_length,
                );
                break;
            }

            if header.gc_flags & GC_FLAG_MARKED != 0 {
                // Survivor: clear the mark, keep in place.
                header.gc_flags &= !GC_FLAG_MARKED;
                objects_live += 1;
            } else {
                // Dead: zero the whole object span so a later conservative
                // root scan cannot resurrect a stale header inside the
                // reclaimed hole, then record it for the free list.
                // SAFETY: `[obj_ptr, obj_ptr+total_size)` lies within the
                // live from-space region (checked above).
                unsafe { std::ptr::write_bytes(obj_ptr, 0, total_size) };
                dead_regions.push((cursor, total_size));
                bytes_swept += total_size;
                objects_swept += 1;
            }
            cursor += total_size;
        }

        // Defence-in-depth: unconditionally clear `GC_FLAG_MARKED` on every
        // object header in the from-space. The survivor branch above already
        // clears the bit for objects it visited, but if the walk broke out
        // early (corrupt / zero-size header) every later survivor would keep
        // a stale mark. The next non-moving sweep treats any object with the
        // mark bit set as live regardless of root reachability, which would
        // retain garbage indefinitely (bug C5). Re-walk and clear all marks
        // — cheap (one byte per header) and idempotent on this path. Runs on
        // both the normal-completion and the `break` arm because it sits
        // after the `while` loop.
        clear_all_mark_bits_in_arena(&mut young_from);

        // Publish reclaimed regions to the arena's free list. Subsequent
        // `try_alloc_young` calls will satisfy allocations from these
        // holes before bumping the cursor — reclaiming memory without
        // moving a single survivor.
        for (off, sz) in dead_regions {
            young_from.add_free_block(off, sz);
        }

        let live_bytes = bytes_before.saturating_sub(bytes_swept);
        tracing::debug!(
            "non-moving young sweep: {} live objects ({} bytes), {} dead \
             objects reclaimed ({} bytes) into free list",
            objects_live,
            live_bytes,
            objects_swept,
            bytes_swept,
        );

        self.stats.minor_gc_count.fetch_add(1, Ordering::Relaxed);

        (
            GcResult {
                stats: crate::gc::GcStats {
                    objects_copied: 0,
                    bytes_copied: live_bytes,
                    bytes_freed: bytes_swept,
                },
                // Nothing moved — no roots need rewriting.
                pointer_map: HashMap::new(),
            },
            // Resurrected finalizers keep their addresses (non-moving).
            finalizer_addrs.to_vec(),
        )
    }

    /// Run a major garbage collection on the old generation using mark-compact.
    ///
    /// 1. **Mark phase:** starting from `roots` + all young-gen objects, traverse
    ///    the heap marking live old-gen objects (set `GC_FLAG_MARKED`).
    /// 2. **Compact phase:** slide all live objects toward the start of the heap,
    ///    eliminating fragmentation. Update all internal references.
    /// 3. **Cross-gen fixup:** update young-gen references that pointed into
    ///    old gen to use the new compacted addresses.
    /// 4. **Root fixup:** update external root references pointing into old gen.
    ///
    /// Returns a pointer map (old_addr → new_addr) for relocated old-gen objects.
    /// The caller merges this into the overall `GcResult` for VM-level root updates.
    fn major_gc(
        roots: &mut [ObjectRef],
        young_from: &Arena,
        old_gen: &mut OldGen,
    ) -> HashMap<usize, usize> {
        // ---- Mark phase ---- BFS from roots + young-gen cross-references ----

        let mut worklist: Vec<*mut u8> = Vec::new();

        // Seed: root ObjectRefs that point into old gen
        for root in roots.iter() {
            let ptr = root.as_ptr();
            if old_gen.contains(ptr) {
                // SAFETY: `ptr` is a root ObjectRef in old gen (verified by `contains` above); its header is valid.
                let header = unsafe { &mut *(ptr as *mut ObjectHeader) };
                if header.gc_flags & GC_FLAG_MARKED == 0 {
                    header.gc_flags |= GC_FLAG_MARKED;
                    worklist.push(ptr);
                }
            }
        }

        // Seed: young from-space references into old gen
        Self::mark_young_to_old_refs(young_from, old_gen, &mut worklist);

        // BFS: transitively mark all reachable old-gen objects
        while let Some(obj_ptr) = worklist.pop() {
            Self::scan_object_for_old_refs(obj_ptr, old_gen, &mut worklist);
        }

        // ---- Compact phase ---- sliding compaction of old gen ----

        let compact_map = old_gen.compact();

        // ---- Cross-gen fixup ---- update young-gen refs into old gen ----

        if !compact_map.is_empty() {
            Self::fixup_young_old_refs(young_from, &compact_map);

            // ---- Root fixup ---- update roots pointing into old gen ----
            for root in roots.iter_mut() {
                let old_addr = root.as_ptr() as usize;
                if let Some(&new_addr) = compact_map.get(&old_addr) {
                    // SAFETY: `new_addr` comes from the compaction map, pointing to a valid relocated object.
                    *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }

        compact_map
    }

    /// Scan young from-space for references into old gen and mark them.
    fn mark_young_to_old_refs(
        young_from: &Arena,
        old_gen: &OldGen,
        worklist: &mut Vec<*mut u8>,
    ) {
        let mut cursor: usize = 0;
        while cursor < young_from.used() {
            // SAFETY: `cursor` is within `young_from.used()`; pointer arithmetic stays in the arena.
            let obj_ptr = unsafe { young_from.base_ptr().add(cursor) as *mut u8 };
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let total_size = gen_object_total_size(header);
            if total_size < HEADER_SIZE {
                break;
            }

            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                        if raw != 0 {
                            let ref_ptr = raw as usize as *mut u8;
                            if old_gen.contains(ref_ptr) {
                                // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                                let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                                if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                                    ref_header.gc_flags |= GC_FLAG_MARKED;
                                    worklist.push(ref_ptr);
                                }
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        let ref_ptr = ref_obj.as_ptr();
                        if old_gen.contains(ref_ptr) {
                            // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                            let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                            if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                                ref_header.gc_flags |= GC_FLAG_MARKED;
                                worklist.push(ref_ptr);
                            }
                        }
                    }
                }
            }

            cursor += total_size;
        }
    }

    /// Scan a single object's reference slots for old-gen pointers and mark them.
    fn scan_object_for_old_refs(
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        worklist: &mut Vec<*mut u8>,
    ) {
        // SAFETY: `obj_ptr` is a live old-gen object from the mark worklist; its header is valid.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

        if header.kind == ObjectKind::Array {
            if header.element_type == ArrayElementType::Reference {
                for i in 0..header.array_length as usize {
                    // SAFETY: `i` < `array_length`; offset within array data region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                    let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if old_gen.contains(ref_ptr) {
                            // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                            let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                            if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                                ref_header.gc_flags |= GC_FLAG_MARKED;
                                worklist.push(ref_ptr);
                            }
                        }
                    }
                }
            }
        } else {
            for slot_idx in 0..header.num_slots as usize {
                // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if old_gen.contains(ref_ptr) {
                        // SAFETY: `ref_ptr` is in old gen (verified by `contains`); its header is valid and mutable for marking.
                        let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                        if ref_header.gc_flags & GC_FLAG_MARKED == 0 {
                            ref_header.gc_flags |= GC_FLAG_MARKED;
                            worklist.push(ref_ptr);
                        }
                    }
                }
            }
        }
    }

    /// After old-gen compaction, update references in young from-space that
    /// pointed to old-gen objects which have been relocated.
    fn fixup_young_old_refs(young_from: &Arena, compact_map: &HashMap<usize, usize>) {
        let mut cursor: usize = 0;
        while cursor < young_from.used() {
            // SAFETY: `cursor` is within `young_from.used()`; pointer arithmetic stays in the arena.
            let obj_ptr = unsafe { young_from.base_ptr().add(cursor) as *mut u8 };
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let total_size = gen_object_total_size(header);
            if total_size < HEADER_SIZE {
                break;
            }

            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data region.
                        let slot = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw: u64 = unsafe { std::ptr::read(slot as *const u64) };
                        if raw != 0 {
                            if let Some(&new_addr) = compact_map.get(&(raw as usize)) {
                                // SAFETY: Writing the compacted address back to the same valid ref-array slot.
                                unsafe { std::ptr::write(slot as *mut u64, new_addr as u64) };
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                    let slot = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe { std::ptr::read(slot as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        if let Some(&new_addr) = compact_map.get(&(ref_obj.as_ptr() as usize)) {
                            // SAFETY: `new_addr` comes from the compaction map, pointing to a valid relocated object.
                            let new_value = Value::Object(Some(unsafe {
                                ObjectRef::from_raw(new_addr as *mut u8)
                            }));
                            // SAFETY: Writing updated Value back to the same valid slot.
                            unsafe { std::ptr::write(slot as *mut Value, new_value) };
                        }
                    }
                }
            }

            cursor += total_size;
        }
    }

    // ----- Internal ----------------------------------------------------------

    /// Allocate bytes in the young from-space.
    ///
    /// Returns `None` if the young generation is exhausted and cannot satisfy
    /// the allocation. The caller should trigger a GC cycle and retry, or
    /// throw `OutOfMemoryError`.
    fn try_alloc_young(&self, size: usize) -> Option<*mut u8> {
        // NUMA stub: query the preferred node for the calling thread so
        // the topology probe gets exercised in production. On multi-node
        // hosts this records when the heap's primary node disagrees with
        // the caller's node — that delta is the win the multi-arena
        // upgrade will eventually capture. On single-node hosts (every
        // current test target) this is two integer compares and a return.
        let node = self.numa_slow_path_hint();
        if self.numa_num_nodes > 1 && node != self.numa_node_hint {
            tracing::trace!(
                target: "cratonvm::gc::numa",
                node, primary = self.numa_node_hint, size,
                "try_alloc_young: cross-node slow path (single-arena fallback)",
            );
        }

        let ptr = {
            let mut from = self.young_from.lock();
            from.alloc(size, 8)
            // Lock released here — zeroing happens outside the lock
        };
        if let Some(ptr) = ptr {
            // Zero the block after releasing the lock (O(size) memset)
            // SAFETY: `ptr` was just allocated from the arena with `size` bytes; zeroing is within bounds.
            unsafe { std::ptr::write_bytes(ptr, 0, size) };
            self.stats.young_allocations.fetch_add(1, Ordering::Relaxed);
            return Some(ptr);
        }
        None
    }

    /// Check if an allocation of `size` bytes would succeed in the young from-space.
    /// Does NOT allocate — just probes available space.
    pub fn try_alloc_young_probe(&self, size: usize) -> Option<()> {
        let from = self.young_from.lock();
        let aligned = from.used().checked_add(7).map(|v| v & !7)?;
        let end = aligned.checked_add(size)?;
        if end <= from.capacity() { Some(()) } else { None }
    }

    /// Carve out a TLAB-sized chunk from the young from-space.
    ///
    /// Returns `Some((ptr, size))` on success, where `ptr` is the start of
    /// the zeroed region and `size` is the actual TLAB size (may be smaller
    /// than requested if the arena is nearly full).
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        // NUMA stub: same shape as try_alloc_young. The TLAB itself is
        // per-thread so its fast path is already NUMA-local; this records
        // the *refill* node so that once we land per-node arenas we can
        // size each node's young-from to match its TLAB refill pressure.
        let node = self.numa_slow_path_hint();
        if self.numa_num_nodes > 1 && node != self.numa_node_hint {
            tracing::trace!(
                target: "cratonvm::gc::numa",
                node, primary = self.numa_node_hint, requested_size,
                "refill_tlab: cross-node refill (single-arena fallback)",
            );
        }

        let mut from = self.young_from.lock();
        let available = from.remaining();
        if available < 256 {
            return None; // Not enough for a useful TLAB
        }
        let actual_size = requested_size.min(available);
        let ptr = from.alloc(actual_size, 8)?;
        // Zero the TLAB region
        // SAFETY: `ptr` was just allocated from the arena with `actual_size` bytes; zeroing is within bounds.
        unsafe { std::ptr::write_bytes(ptr, 0, actual_size) };
        Some((ptr, actual_size))
    }

    /// Allocate bytes in the young from-space.
    ///
    /// If the from-space is full, logs a fatal error and aborts. The
    /// interpreter's `gc_alloc_*` functions use `try_alloc_young` with
    /// GC-and-retry; this method is only called by the panicking
    /// `alloc_object`/`alloc_array` convenience wrappers.
    fn alloc_young(&self, size: usize) -> *mut u8 {
        self.try_alloc_young(size).unwrap_or_else(|| {
            let from = self.young_from.lock();
            eprintln!(
                "FATAL: OutOfMemoryError: young gen exhausted — tried to allocate {} bytes, \
                 from-space has {}/{} used",
                size,
                from.used(),
                from.capacity(),
            );
            std::process::abort();
        })
    }

    /// Generate the next identity hash code.
    ///
    /// H1: exposed publicly so `interpreter::init_object_header` (TLAB
    /// fast path) can mint a unique hash at allocation time, matching the
    /// non-TLAB allocators. Without this, freshly TLAB-allocated objects
    /// of `ClassId(0)` (java/lang/Object) with no fields produce an
    /// all-zero header that the stale-pointer detector mis-flags as
    /// stale.
    pub fn next_hash(&self) -> i32 {
        self.next_hash_code.fetch_add(1, Ordering::Relaxed)
    }

    /// Forward (copy or promote) a single object from young from-space.
    ///
    /// If the object has been forwarded already, returns the existing address.
    /// If the object has survived enough GCs (age >= PROMOTION_AGE), promotes
    /// it to old gen. Otherwise copies to young to-space with incremented age.
    fn forward_object(
        young_from: &Arena,
        young_to: &mut Arena,
        old_gen: &mut OldGen,
        old_ptr: *mut u8,
        objects_copied: &mut usize,
        pointer_map: &mut FxHashMap<usize, usize>,
        promoted_worklist: &mut Vec<*mut u8>,
    ) -> *mut u8 {
        // SAFETY: `old_ptr` points to a live young-gen object; its header is
        // valid. Build an owned *copy* of the header rather than holding a
        // shared `&ObjectHeader`: later in this function we install the
        // forwarding pointer through a `&mut`/raw write to the same address,
        // and a live `&` aliasing that write would be undefined behavior.
        //
        // ATOMIC-UB fix: do NOT `std::ptr::read` the whole `ObjectHeader` by
        // value. `ObjectHeader` embeds `mark_word: AtomicU64`; a `ptr::read`
        // of a struct containing an atomic performs a *non-atomic* read of an
        // atomic location, which is undefined behavior. Instead read each
        // scalar field individually through field-projected raw pointers, and
        // read `mark_word` via an explicit `AtomicU64::load`, then reconstruct
        // an owned header from those values.
        let header: ObjectHeader = unsafe {
            let h = old_ptr as *const ObjectHeader;
            let mut owned = ObjectHeader::new(
                std::ptr::addr_of!((*h).class_id).read(),
                std::ptr::addr_of!((*h).kind).read(),
                std::ptr::addr_of!((*h).element_type).read(),
                std::ptr::addr_of!((*h).identity_hash_code).read(),
                std::ptr::addr_of!((*h).array_length).read(),
                std::ptr::addr_of!((*h).num_slots).read(),
            );
            owned.gc_age = std::ptr::addr_of!((*h).gc_age).read();
            owned.gc_flags = std::ptr::addr_of!((*h).gc_flags).read();
            owned.forwarding_ptr = std::ptr::addr_of!((*h).forwarding_ptr).read();
            // `mark_word` is an `AtomicU64`: read it through an atomic load so
            // the access is well-defined under the memory model.
            owned.mark_word.store(
                (*h).mark_word.load(std::sync::atomic::Ordering::Relaxed),
                std::sync::atomic::Ordering::Relaxed,
            );
            owned
        };
        let header = &header;

        // KC16 SIGSEGV audit: sanity-check header before using it. A corrupted
        // header (e.g., slot tag mis-identified a non-pointer bit-pattern as an
        // ObjectRef) would cause forward_object to walk into arbitrary memory.
        // Emitting forensic output here converts a silent SIGSEGV into a visible
        // diagnostic.
        //
        // Multi-array reloc fix (2026-05-22): the `num_slots > 1<<24` clause
        // used to trip for ANY array whose length exceeds 2^24 elements,
        // because `alloc_array` stores the array length in BOTH
        // `array_length` and `num_slots`. With three back-to-back 2^26-int
        // arrays the third allocation triggers a minor GC; `forward_object`
        // would then refuse to relocate the first two large arrays, return
        // their old pointers without inserting into the pointer map, and
        // young_from.reset() would zero the original headers. After the
        // semispace swap, the still-unremapped roots dereferenced the now-
        // zeroed memory, giving the all-zero `kind/class_id/element_type/
        // array_length` symptom and a `.length` of 0. Guard num_slots only
        // for non-arrays (where it really is a field count). For arrays,
        // gate on `array_length` against the JVM's i32::MAX ceiling — the
        // same cap as `MAX_REASONABLE_ARRAY_LEN` in `array_length()`.
        let kind_byte = header.kind as u8;
        let is_array = header.kind == ObjectKind::Array;
        let array_length_too_large = is_array && header.array_length > i32::MAX as u32;
        let num_slots_too_large = !is_array && header.num_slots > (1 << 24);
        if kind_byte > 1 || num_slots_too_large || array_length_too_large {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                old_ptr = ?old_ptr,
                kind_byte,
                num_slots = header.num_slots,
                array_length = header.array_length,
                class_id = ?header.class_id,
                gc_flags = format!("{:x}", header.gc_flags),
                backtrace = ?std::backtrace::Backtrace::capture(),
                "gen_heap::forward_object: suspect header",
            );
            // Leave the object unmoved; this is a suspected false root or
            // corrupted slot. Returning old_ptr preserves progress while the
            // eprintln above gives us the evidence needed to diagnose.
            return old_ptr;
        }

        // Already forwarded?
        if header.is_forwarded() {
            let fwd = header.forwarding_address();
            // KC16 SIGSEGV audit: verify the forwarding address is sane before
            // returning it. A stale forwarding pointer left over from a prior
            // GC cycle that wasn't cleared by session 73's fix would send
            // callers to freed memory.
            if fwd.is_null() || (fwd as usize) % 8 != 0 {
                tracing::debug!(
                    target: "cratonvm::gc::guard",
                    fwd = ?fwd,
                    old_ptr = ?old_ptr,
                    class_id = ?header.class_id,
                    gc_flags = format!("{:x}", header.gc_flags),
                    gc_age = header.gc_age,
                    backtrace = ?std::backtrace::Backtrace::capture(),
                    "gen_heap::forward_object: bad forwarding_ptr",
                );
                return old_ptr;
            }
            // Ensure pointer_map has this entry so update_all_roots can update
            // all references to this old address, even if this is a second
            // encounter of the same object (e.g., root + dirty card + Cheney scan).
            pointer_map.entry(old_ptr as usize).or_insert(fwd as usize);
            return fwd;
        }

        let total_size = gen_object_total_size(header);

        // Sanity: skip objects whose computed extent does not fit entirely
        // within the young from-space. This guards against false conservative
        // roots from JIT spill slots that bit-match a heap address but don't
        // point at a real object — a bogus header yields a size that runs off
        // the end of the arena. A genuine object, however large, fits within
        // the arena it was allocated from by construction, so this never
        // rejects a real live array (a fixed byte cap did: a 64 MB int[] is
        // perfectly valid at a multi-GB heap).
        let from_base = young_from.base_ptr() as usize;
        let from_end = from_base + young_from.capacity();
        let obj_addr = old_ptr as usize;
        let fits_in_arena = obj_addr >= from_base
            && obj_addr
                .checked_add(total_size)
                .is_some_and(|obj_end| obj_end <= from_end);
        if total_size < HEADER_SIZE || !fits_in_arena {
            tracing::warn!(
                "GC: skipping suspected false root at {:p} (computed size {} bytes, \
                 kind={:?}, num_slots={}, array_len={})",
                old_ptr,
                total_size,
                header.kind,
                header.num_slots,
                header.array_length,
            );
            return old_ptr; // Leave unmoved — likely not a real object
        }
        // Promote if this GC survival would reach or exceed the promotion age.
        // E.g., with PROMOTION_AGE=3: an object at age 2, surviving this GC,
        // would become age 3 → promote instead.
        let should_promote = header.gc_age + 1 >= PROMOTION_AGE;

        let new_ptr = if should_promote {
            // Promote to old gen
            match old_gen.alloc(total_size, 8) {
                Some(ptr) => ptr,
                None => {
                    // Old gen full — fall back to young to-space
                    // (Major GC will be needed later)
                    match young_to.alloc(total_size, 8) {
                        Some(ptr) => ptr,
                        None => {
                            // UAF fix: both old gen and to-space are full.
                            // Previously this returned `old_ptr`, leaving the
                            // object in from-space. But the caller resets
                            // young_from immediately after this collection,
                            // wiping that memory — every still-live reference
                            // to `old_ptr` would then dangle (use-after-free).
                            // A copying collector cannot safely "skip" an
                            // object: there is no valid address to hand back.
                            // This is an unrecoverable OOM; abort hard, the
                            // same way `alloc_young` handles young-gen
                            // exhaustion.
                            eprintln!(
                                "FATAL: OutOfMemoryError: GC could not relocate a live object — \
                                 both old gen and young to-space are full during promotion \
                                 (tried {} bytes, to-space {}/{} used). Cannot leave the object \
                                 unmoved without dangling references after from-space reset.",
                                total_size,
                                young_to.used(),
                                young_to.capacity(),
                            );
                            std::process::abort();
                        }
                    }
                }
            }
        } else {
            // Copy to young to-space
            match young_to.alloc(total_size, 8) {
                Some(ptr) => ptr,
                None => {
                    // UAF fix: young to-space is full. Previously this
                    // returned `old_ptr`, leaving the object in from-space —
                    // but the caller resets young_from right after this
                    // collection, so every live reference to `old_ptr` would
                    // dangle (use-after-free). A copying collector has no
                    // valid address to return for an un-relocated object.
                    // This is an unrecoverable OOM; abort hard, consistent
                    // with `alloc_young`'s handling of young-gen exhaustion.
                    eprintln!(
                        "FATAL: OutOfMemoryError: GC could not relocate a live object — \
                         young to-space is full (tried {} bytes, to-space has {}/{} used). \
                         Cannot leave the object unmoved without dangling references after \
                         from-space reset.",
                        total_size,
                        young_to.used(),
                        young_to.capacity(),
                    );
                    std::process::abort();
                }
            }
        };

        // Copy the entire object
        // SAFETY: `old_ptr` and `new_ptr` are valid, non-overlapping regions of `total_size` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(old_ptr, new_ptr, total_size);
        }

        // Round-2 fix (T2-4): explicit atomic load+store for the mark_word
        // field. The bulk memcpy above is technically UB for `AtomicU64`:
        // even under STW (no mutator is racing), the memory model still
        // requires that reads/writes of atomic locations go through atomic
        // ops. Replicating the mark word atomically materializes the
        // correct happens-before edge for any observer that may later
        // perform a CAS on the new copy (monitor inflation, etc.).
        //
        // NOTE for future concurrent-GC support: this STW-only protocol is
        // not sufficient for a concurrent collector. A concurrent
        // forwarding protocol must instead CAS-install the forwarding
        // pointer and re-read the mark word if a mutator raced.
        // SAFETY: both `old_ptr` and `new_ptr` point at a fully written
        // ObjectHeader whose `mark_word` field lives at MARK_WORD_OFFSET.
        unsafe {
            let old_header_ptr = old_ptr as *const ObjectHeader;
            let new_header_ptr = new_ptr as *mut ObjectHeader;
            let mark = (*old_header_ptr)
                .mark_word
                .load(std::sync::atomic::Ordering::Relaxed);
            (*new_header_ptr)
                .mark_word
                .store(mark, std::sync::atomic::Ordering::Relaxed);
        }

        // Update the new header
        // SAFETY: `new_ptr` was just allocated and the object was copied there; its header is valid and mutable.
        let new_header = unsafe { &mut *(new_ptr as *mut ObjectHeader) };
        new_header.forwarding_ptr = std::ptr::null_mut();

        let landed_in_old_gen = should_promote && old_gen.contains(new_ptr);
        if landed_in_old_gen {
            // Mark as old gen
            new_header.gc_flags |= GC_FLAG_OLD_GEN;
        } else {
            // Increment age for young gen survivors
            new_header.gc_age = new_header.gc_age.saturating_add(1);
        }

        // Install forwarding pointer in the old header.
        // SAFETY: `old_ptr` is a valid young-gen object; writing the forwarding
        // pointer into its header is safe. Written through a raw pointer so no
        // `&mut ObjectHeader` is ever live alongside another reference to this
        // header (we earlier read an owned copy rather than borrowing it).
        unsafe {
            std::ptr::addr_of_mut!((*(old_ptr as *mut ObjectHeader)).forwarding_ptr).write(new_ptr);
        }

        pointer_map.insert(old_ptr as usize, new_ptr as usize);
        *objects_copied += 1;
        // CRIT-P2 fix: enqueue promoted objects so the alternating Cheney
        // loop can scan them in O(1) per object instead of re-filtering
        // `pointer_map.values()` per iteration. Young to-space copies are
        // already handled by the bump-cursor Cheney scan in the caller, so
        // we only enqueue when the object actually landed in old gen.
        if landed_in_old_gen {
            promoted_worklist.push(new_ptr);
        }

        debug_assert!(young_from.contains(old_ptr));

        new_ptr
    }

    /// Scan dirty cards in the card table for old→young references.
    ///
    /// For each dirty card, walks the objects in that card region and collects
    /// slots containing references into young from-space.
    fn scan_dirty_cards(
        card_table: &CardTable,
        old_gen: &OldGen,
        young_from: &Arena,
        extra_roots: &mut Vec<(ObjectRef, usize, usize)>,
    ) {
        // Drain the O(dirty) tracking list rather than scanning the whole
        // bitmap with `dirty_card_indices()` — this is O(dirty) instead of
        // O(total cards). `take_dirty_cards` also clears the tracking list,
        // which is fine because `card_table.clear_all()` is called by the
        // caller right after the dirty-card scan completes.
        let mut dirty_indices = card_table.take_dirty_cards();
        if dirty_indices.is_empty() {
            return;
        }

        let card_base = card_table.base_addr();
        let card_size = crate::card_table::CARD_SIZE;
        let old_base = old_gen.base_ptr() as usize;

        // Round-11 perf: instead of `walk_objects()` (which allocates a Vec
        // of *every* old-gen object and then filters per-object), translate
        // the dirty card indices into byte-offset ranges relative to the
        // old-gen data buffer and let `walk_objects_in_card_ranges` collect
        // only the objects that actually start inside a dirty card.
        //
        // The card table covers a region starting at `card_base`; old-gen
        // object pointers may sit at `old_base >= card_base`. Convert each
        // dirty card's `[card_start, card_end)` address window into an
        // offset window relative to `old_base`, clamped to `[0, capacity)`.
        // Cards entirely below the old gen contribute nothing.
        dirty_indices.sort_unstable();
        let old_cap = old_gen.capacity();
        let mut dirty_ranges: Vec<(usize, usize)> = Vec::with_capacity(dirty_indices.len());
        for &card_idx in &dirty_indices {
            let card_start = card_base.saturating_add(card_idx * card_size);
            let card_end = card_start.saturating_add(card_size);
            // Skip cards that end at or before the old-gen base.
            if card_end <= old_base {
                continue;
            }
            let lo = card_start.saturating_sub(old_base).min(old_cap);
            let hi = card_end.saturating_sub(old_base).min(old_cap);
            if lo < hi {
                // Coalesce with the previous range if the cards are
                // contiguous (adjacent dirty cards are common).
                if let Some(last) = dirty_ranges.last_mut() {
                    if last.1 >= lo {
                        last.1 = last.1.max(hi);
                        continue;
                    }
                }
                dirty_ranges.push((lo, hi));
            }
        }
        if dirty_ranges.is_empty() {
            return;
        }

        // Walk old gen, collecting only objects whose start offset lands in
        // a dirty card range. Preserves the original semantics (an object
        // is a root iff the card containing its *header* is dirty).
        let objects = old_gen.walk_objects_in_card_ranges(&dirty_ranges);
        for (obj_ptr, _total_size) in objects {
            // SAFETY: `obj_ptr` is from `old_gen.walk_objects_in_card_ranges()`, pointing to a valid old-gen object header.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };

            // Scan ref slots: ref arrays use compact 8-byte pointers,
            // object fields use 16-byte Value.
            if header.kind == ObjectKind::Array {
                if header.element_type == ArrayElementType::Reference {
                    for i in 0..header.array_length as usize {
                        // SAFETY: `i` < `array_length`; offset within array data region.
                        let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * REF_ELEMENT_SIZE) };
                        let raw: u64 = unsafe { std::ptr::read(s_ptr as *const u64) };
                        if raw != 0 {
                            let ref_ptr = raw as usize as *mut u8;
                            if young_from.contains(ref_ptr) {
                                // SAFETY: `obj_ptr` is a valid old-gen object pointer; wrapping in ObjectRef is sound.
                                let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                                extra_roots.push((obj_ref, i, 0));
                            }
                        }
                    }
                }
            } else {
                for slot_idx in 0..header.num_slots as usize {
                    // SAFETY: `slot_idx` is within `num_slots`; offset within the object's field region.
                    let s_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                    let value = unsafe { std::ptr::read(s_ptr as *const Value) };
                    if let Value::Object(Some(ref_obj)) = value {
                        if young_from.contains(ref_obj.as_ptr()) {
                            // SAFETY: `obj_ptr` is a valid old-gen object pointer; wrapping in ObjectRef is sound.
                            let obj_ref = unsafe { ObjectRef::from_raw(obj_ptr) };
                            extra_roots.push((obj_ref, slot_idx, 0));
                        }
                    }
                }
            }
        }
    }

    /// The capacity of each young semi-space.
    pub fn young_semi_capacity(&self) -> usize {
        self.young_from.lock().capacity()
    }

    /// The capacity of the old generation.
    pub fn old_gen_capacity(&self) -> usize {
        self.old_gen.lock().capacity()
    }

    /// The number of bytes currently used in the young from-space.
    pub fn young_from_used(&self) -> usize {
        self.young_from.lock().used()
    }

    /// The number of bytes currently used in the old generation.
    pub fn old_gen_used(&self) -> usize {
        self.old_gen.lock().used()
    }

    /// Check if a pointer is in the young from-space.
    pub fn is_in_young(&self, ptr: *const u8) -> bool {
        self.young_from.lock().contains(ptr)
    }

    /// Check if a pointer is in the old generation.
    pub fn is_in_old(&self, ptr: *const u8) -> bool {
        self.old_gen.lock().contains(ptr)
    }

    /// Walk all live objects in both young and old generations.
    /// Returns a Vec of (raw pointer, total byte size) for each object.
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        let mut result = Vec::new();

        // Walk young generation (from-space only — to-space is GC scratch)
        {
            let young = self.young_from.lock();
            let base = young.base_ptr() as usize;
            let used = young.used();
            let mut offset = 0;
            while offset < used {
                let ptr = (base + offset) as *mut u8;
                // SAFETY: `ptr` is within `young_from.used()` region; reading the header is valid.
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                // Round-9 gc CRIT-1: HumongousFiller is a walker sentinel
                // installed by the regional GC. The generational heap
                // never allocates humongous, but defensively stop the
                // scan if the kind byte is the sentinel value (treating
                // it as a real object would mis-parse the rest of the
                // stripe).
                if header.kind == ObjectKind::HumongousFiller {
                    break;
                }
                let total_size = if header.kind == ObjectKind::Array {
                    // A malformed `array_length` makes `array_data_size`
                    // overflow/fail. Do NOT silently treat it as a 0-byte
                    // payload — that would advance the cursor by only
                    // HEADER_SIZE and mis-parse the rest of the arena as
                    // bogus objects. Treat it as heap corruption and stop
                    // the walk cleanly, matching the `size < HEADER_SIZE`
                    // handling below.
                    match array_data_size(header.array_length as usize, header.element_type) {
                        Ok(data) => HEADER_SIZE + data,
                        Err(_) => {
                            tracing::warn!(
                                "GC: stopping young-gen heap walk at {:p} — implausible \
                                 array_length {} (element_type={:?}); suspected corrupt header",
                                ptr,
                                header.array_length,
                                header.element_type,
                            );
                            break;
                        }
                    }
                } else {
                    HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
                };
                if total_size < HEADER_SIZE || offset + total_size > used {
                    break;
                }
                result.push((ptr, total_size));
                offset += total_size;
            }
        }

        // Walk old generation
        {
            let old = self.old_gen.lock();
            result.extend(old.walk_objects());
        }

        result
    }
}

impl Default for GenerationalHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for GenerationalHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let yf = self.young_from.lock();
        let yt = self.young_to.lock();
        let og = self.old_gen.lock();
        f.debug_struct("GenerationalHeap")
            .field("young_from_used", &yf.used())
            .field("young_from_capacity", &yf.capacity())
            .field("young_to_used", &yt.used())
            .field("young_to_capacity", &yt.capacity())
            .field("old_gen_used", &og.used())
            .field("old_gen_capacity", &og.capacity())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Size / slot access helpers
// ---------------------------------------------------------------------------

/// Compute total size of a heap object (for GC cursor advancement).
/// Objects use SLOT_SIZE per field. Arrays use compact element sizes.
///
/// A malformed `array_length` makes `array_data_size` overflow/fail. Rather
/// than silently returning a 0-byte payload (`HEADER_SIZE`, which advances a
/// scan cursor by the wrong amount and mis-parses subsequent memory), this
/// returns `0` — a value below `HEADER_SIZE`. Every caller already either
/// breaks the walk on `total_size < HEADER_SIZE` or rejects it via the
/// `MAX_SANE_OBJECT_SIZE` corrupt-header check, so a bad array length is
/// surfaced as corruption instead of corrupting the cursor.
#[inline]
fn gen_object_total_size(header: &ObjectHeader) -> usize {
    if header.kind == ObjectKind::Array {
        match array_data_size(header.array_length as usize, header.element_type) {
            Ok(data) => HEADER_SIZE + data,
            Err(_) => {
                tracing::warn!(
                    "GC: implausible array_length {} (element_type={:?}) in heap object \
                     header — treating as corrupt; caller will stop/skip the walk",
                    header.array_length,
                    header.element_type,
                );
                0
            }
        }
    } else {
        HEADER_SIZE + header.num_slots as usize * SLOT_SIZE
    }
}

/// Compute a pointer to the slot at `index` within an object/array.
#[inline]
fn slot_ptr(obj_ref: ObjectRef, index: usize) -> *mut u8 {
    // SAFETY: Caller guarantees `index` is within the object's slot count; pointer arithmetic stays within the allocation.
    unsafe { obj_ref.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE) }
}

/// Walk every object header in `arena` and clear `GC_FLAG_MARKED`.
///
/// Used after `sweep_young_non_moving` to guarantee no stale mark bits
/// survive into the next collection (bug C5). The non-moving sweep's main
/// loop only clears marks on objects it visits as survivors; if the loop
/// breaks early on a corrupt / zero-size header, every still-marked object
/// past the breakout point would be treated as live by the *next* sweep
/// (`header.gc_flags & GC_FLAG_MARKED != 0`), retaining unreachable
/// garbage indefinitely.
///
/// Walks the same layout as the sweep — skipping known free blocks, parsing
/// headers in place — and is equally defensive about corruption: a
/// zero-size or out-of-range header stops the walk (we cannot safely
/// continue past unknown structure), but every header we *did* reach gets
/// its mark cleared. Cheap (one byte per header) and idempotent.
fn clear_all_mark_bits_in_arena(arena: &mut Arena) {
    let base = arena.base_ptr() as usize;
    let used = arena.used();
    let free_blocks = arena.free_blocks_sorted();
    let mut free_iter = free_blocks.iter().peekable();
    let mut cursor: usize = 0;
    while cursor < used {
        // Skip known free blocks — their bytes are stale and must not be
        // parsed as object headers.
        if let Some(&&(off, sz)) = free_iter.peek() {
            if cursor == off {
                cursor += sz;
                free_iter.next();
                continue;
            }
        }
        // SAFETY: `cursor` is within `used`; the arena's `[base, base+used)`
        // region is backed by mapped, allocated memory.
        let obj_ptr = unsafe { (base + cursor) as *mut u8 };
        // SAFETY: `obj_ptr` is 8-byte-aligned (bump arena) and points at the
        // start of an object header within the live region.
        let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
        let total_size = gen_object_total_size(header);
        if total_size < HEADER_SIZE || cursor + total_size > used {
            // Corruption — same defence as the sweep loop. Stop rather than
            // risk parsing arbitrary bytes as a header. Any marks past this
            // point remain, but on the normal-completion path of the sweep
            // this branch is unreachable; on the break path the sweep
            // already abandoned freeing past this offset for the same
            // reason, so any retained mark is no worse than the sweep's own
            // pre-existing conservativism.
            break;
        }
        header.gc_flags &= !GC_FLAG_MARKED;
        cursor += total_size;
    }
}

/// Read a `Value` from a slot pointer.
///
/// # Safety
///
/// `ptr` must point to a valid, initialized `Value`-sized region within a
/// heap-allocated object. The caller must ensure no concurrent writes to
/// the same slot.
// SAFETY: caller guarantees `ptr` points to a valid Value within a heap object.
#[inline]
unsafe fn read_slot(ptr: *mut u8) -> Value {
    std::ptr::read(ptr as *const Value)
}

/// Write a `Value` to a slot pointer.
///
/// # Safety
///
/// `ptr` must point to a `Value`-sized region within a heap-allocated object.
/// The caller must ensure exclusive access to the slot.
// SAFETY: caller guarantees `ptr` points to a valid Value slot within a heap object.
#[inline]
unsafe fn write_slot(ptr: *mut u8, value: Value) {
    std::ptr::write(ptr as *mut Value, value);
}

// ---------------------------------------------------------------------------
// GarbageCollector trait impl
// ---------------------------------------------------------------------------

impl GarbageCollector for GenerationalHeap {
    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.alloc_object(class_id, num_fields)
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        self.alloc_array(class_id, element_type, length)
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        self.get_header(obj)
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.class_id_of(obj)
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.kind_of(obj)
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.element_type_of(obj)
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        self.identity_hash_code(obj)
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field(obj, index)
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field(obj, index, value)
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field_volatile(obj, index)
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field_volatile(obj, index, value)
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        self.array_length(obj)
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        self.get_array_element(obj, index)
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        self.set_array_element(obj, index, value)
    }

    fn needs_gc(&self) -> bool {
        self.needs_gc()
    }

    fn collect_garbage(&self, roots: &mut [ObjectRef], monitors: &dyn MonitorCleanup) -> GcResult {
        self.collect_garbage(roots, monitors)
    }

    fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        self.write_barrier(obj, stored_value)
    }

    fn allocated_bytes(&self) -> usize {
        self.allocated_bytes()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// No-op monitor cleanup for tests in the gc crate.
    struct NoOpMonitors;
    impl crate::collector::MonitorCleanup for NoOpMonitors {
        fn remap_after_gc(&self, _pointer_map: &std::collections::HashMap<usize, usize>) {}
    }

    /// Create a small generational heap for testing.
    fn small_gen_heap() -> GenerationalHeap {
        // 4KB young semi-space, 8KB old gen
        GenerationalHeap::with_sizes(4 * 1024, 8 * 1024)
    }

    #[test]
    fn alloc_object_in_young() {
        let heap = small_gen_heap();
        let obj = heap.alloc_object(ClassId::new(1), 2);
        assert!(heap.is_in_young(obj.as_ptr()));
        assert!(!heap.is_in_old(obj.as_ptr()));
        assert_eq!(heap.class_id_of(obj), ClassId::new(1));
        assert_eq!(heap.get_header(obj).num_slots, 2);
        assert_eq!(heap.get_header(obj).gc_age, 0);
        assert_eq!(heap.get_header(obj).gc_flags, 0);
    }

    #[test]
    fn alloc_array_in_young() {
        let heap = small_gen_heap();
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
        assert!(heap.is_in_young(arr.as_ptr()));
        assert_eq!(heap.array_length(arr), 5);
        assert_eq!(heap.kind_of(arr), ObjectKind::Array);
    }

    #[test]
    fn field_set_and_get() {
        let heap = small_gen_heap();
        let obj = heap.alloc_object(ClassId::new(0), 3);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(100));
        heap.set_field(obj, 2, Value::Float(2.71));

        assert_eq!(heap.get_field(obj, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(obj, 1).as_long(), Some(100));
        assert_eq!(heap.get_field(obj, 2).as_float(), Some(2.71));
    }

    #[test]
    fn array_set_and_get() {
        let heap = small_gen_heap();
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        heap.set_array_element(arr, 0, Value::Int(10)).unwrap();
        heap.set_array_element(arr, 1, Value::Int(20)).unwrap();
        heap.set_array_element(arr, 2, Value::Int(30)).unwrap();

        assert_eq!(heap.get_array_element(arr, 0), Ok(Value::Int(10)));
        assert_eq!(heap.get_array_element(arr, 1), Ok(Value::Int(20)));
        assert_eq!(heap.get_array_element(arr, 2), Ok(Value::Int(30)));
        assert!(heap.get_array_element(arr, 3).is_err());
    }

    #[test]
    fn minor_gc_basic() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(100));

        let mut roots = vec![obj];
        let result = heap.collect_garbage(&mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 1);

        // Root should be updated
        let new_obj = roots[0];
        assert_ne!(new_obj.as_ptr(), obj.as_ptr());

        // Fields intact
        assert_eq!(heap.get_field(new_obj, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(new_obj, 1).as_long(), Some(100));

        // Age should be incremented
        assert_eq!(heap.get_header(new_obj).gc_age, 1);
    }

    #[test]
    fn large_array_survives_gc() {
        // Regression: `forward_object`'s false-root guard rejected any
        // object whose computed size exceeded a hardcoded 64 MiB cap.
        // A genuine `int[16777216]` is 67_108_904 bytes (16M*4 + 40-byte
        // header) — 40 bytes over the cap — so the collector skipped it
        // as a "suspected false root" and freed a still-live array.
        // The guard now bounds-checks against the from-space arena, so a
        // real object of any size that fits the arena survives.
        let heap = GenerationalHeap::with_sizes(80 * 1024 * 1024, 8 * 1024 * 1024);
        let monitors = NoOpMonitors;

        let n: usize = 16_777_216; // 67_108_904-byte int[] — over the old 64 MiB cap
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(arr, 0, Value::Int(7)).unwrap();
        heap.set_array_element(arr, n - 1, Value::Int(12345)).unwrap();

        let mut roots = vec![arr];
        let result = heap.collect_garbage(&mut roots, &monitors);

        // The array must be forwarded, not skipped as a false root.
        assert_eq!(result.stats.objects_copied, 1, "large array must survive GC");

        let new_arr = roots[0];
        assert_ne!(new_arr.as_ptr(), arr.as_ptr(), "array should have been moved");
        assert_eq!(heap.array_length(new_arr), n);
        assert_eq!(heap.get_array_element(new_arr, 0), Ok(Value::Int(7)));
        assert_eq!(heap.get_array_element(new_arr, n - 1), Ok(Value::Int(12345)));
    }

    #[test]
    fn multiple_large_arrays_survive_gc() {
        // Multi-array reloc regression (2026-05-22): when three back-to-back
        // 2^26-element int[] (256 MiB each) survive a minor GC, every array's
        // length must remain intact after the swap. The previous
        // `forward_object` guard rejected any array whose `num_slots > 2^24`
        // (because `alloc_array` mirrors the length into `num_slots`),
        // leaving the roots pointing at original young-from memory that
        // `reset()` then zeroed — every header looked all-zero
        // (kind=Object, class_id=0, element_type=Reference, array_length=0)
        // and `.length` reported 0. Use a smaller scale (2^20-elt) here to
        // keep the unit test cheap; the failing condition is `num_slots`
        // exceeding the old 2^24 ceiling.
        //
        // We use three arrays so the third allocation forces the same
        // copying-collection codepath the real workload triggers.
        let n: usize = 17 * 1024 * 1024; // 17M elements; num_slots = 17M > 1<<24
        let payload = (n * 4 + 7) & !7usize;
        let arr_bytes = HEADER_SIZE + payload;
        let young_semi = (arr_bytes * 4).max(80 * 1024 * 1024);
        let heap = GenerationalHeap::with_sizes(young_semi, 64 * 1024 * 1024);
        let monitors = NoOpMonitors;

        let a = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(a, n - 1, Value::Int(0xAAAA_AAAAu32 as i32)).unwrap();
        let b = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(b, n - 1, Value::Int(0xBBBB_BBBBu32 as i32)).unwrap();
        let c = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        heap.set_array_element(c, n - 1, Value::Int(0xCCCC_CCCCu32 as i32)).unwrap();

        let mut roots = vec![a, b, c];
        let result = heap.collect_garbage(&mut roots, &monitors);
        assert_eq!(result.stats.objects_copied, 3, "all three large arrays must be forwarded");

        let (na, nb, nc) = (roots[0], roots[1], roots[2]);
        assert_eq!(heap.array_length(na), n, "a.length corrupted after GC");
        assert_eq!(heap.array_length(nb), n, "b.length corrupted after GC");
        assert_eq!(heap.array_length(nc), n, "c.length corrupted after GC");
        assert_eq!(heap.get_array_element(na, n - 1), Ok(Value::Int(0xAAAA_AAAAu32 as i32)));
        assert_eq!(heap.get_array_element(nb, n - 1), Ok(Value::Int(0xBBBB_BBBBu32 as i32)));
        assert_eq!(heap.get_array_element(nc, n - 1), Ok(Value::Int(0xCCCC_CCCCu32 as i32)));
    }

    #[test]
    fn minor_gc_unreachable_freed() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let _dead = heap.alloc_object(ClassId::new(0), 2);
        let live = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(live, 0, Value::Int(999));

        let mut roots = vec![live];
        let result = heap.collect_garbage(&mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 1);
        assert!(result.stats.bytes_freed > 0);

        let new_live = roots[0];
        assert_eq!(heap.get_field(new_live, 0).as_int(), Some(999));
    }

    #[test]
    fn minor_gc_preserves_references() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        let obj_b = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
        heap.set_field(obj_b, 0, Value::Int(77));

        let mut roots = vec![obj_a];
        let result = heap.collect_garbage(&mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 2);

        let new_a = roots[0];
        match heap.get_field(new_a, 0) {
            Value::Object(Some(new_b)) => {
                assert_ne!(new_b.as_ptr(), obj_b.as_ptr());
                assert_eq!(heap.get_field(new_b, 0).as_int(), Some(77));
            }
            _ => panic!("Expected A's field to reference B"),
        }
    }

    /// Non-moving young-gen sweep (the JIT-frames-active path).
    ///
    /// With `gc_quiescence` active the collector must run a non-moving
    /// mark-sweep: survivors keep their exact addresses, dead objects
    /// are reclaimed into the free list, and a subsequent allocation
    /// reuses a reclaimed hole. This is the fix for the
    /// "GC skipped: JIT frames are active → OOM" blocker.
    #[test]
    fn non_moving_sweep_when_jit_active() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        // Build a live chain a→b plus a dead object in between.
        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        let dead = heap.alloc_object(ClassId::new(9), 1);
        let obj_b = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
        heap.set_field(obj_b, 0, Value::Int(4242));
        heap.set_field(dead, 0, Value::Int(-1));

        let a_ptr = obj_a.as_ptr();
        let b_ptr = obj_b.as_ptr();
        let dead_ptr = dead.as_ptr();

        // Simulate "a JIT frame is active" so the collector takes the
        // non-moving path. The guard is paired with a `leave()` below.
        crate::gc_quiescence::enter();
        assert!(crate::gc_quiescence::is_active());

        // `roots` carries only `obj_a`; `obj_b` is reached transitively,
        // `dead` is unreachable.
        let mut roots = vec![obj_a];
        let result = heap.collect_garbage(&mut roots, &monitors);

        crate::gc_quiescence::leave();

        // Non-moving: nothing was copied, the pointer map is empty, and
        // the root's address is UNCHANGED.
        assert_eq!(result.stats.objects_copied, 0);
        assert!(result.pointer_map.is_empty());
        assert_eq!(roots[0].as_ptr(), a_ptr, "survivor must not move");

        // Live chain intact at its original addresses.
        assert_eq!(obj_a.as_ptr(), a_ptr);
        match heap.get_field(obj_a, 0) {
            Value::Object(Some(b)) => {
                assert_eq!(b.as_ptr(), b_ptr, "B must not move");
                assert_eq!(heap.get_field(b, 0).as_int(), Some(4242));
            }
            other => panic!("A's field should still reference B, got {other:?}"),
        }

        // Dead object's memory was reclaimed (zeroed + on the free list).
        assert!(result.stats.bytes_freed > 0, "dead object must be reclaimed");
        // The reclaimed region was zeroed by the sweep.
        // SAFETY: `dead_ptr` is inside the young arena; reading its
        // (now-freed, zeroed) header is a valid in-bounds read.
        let dead_header = unsafe { &*(dead_ptr as *const ObjectHeader) };
        assert_eq!(dead_header.class_id, ClassId::new(0), "freed hole must be zeroed");

        // A fresh allocation must succeed and reuse the reclaimed hole
        // (it lands at the dead object's old address since that's the
        // first free block).
        let reused = heap.alloc_object(ClassId::new(7), 1);
        assert_eq!(
            reused.as_ptr(),
            dead_ptr,
            "new allocation should reuse the swept hole",
        );
        heap.set_field(reused, 0, Value::Int(555));
        assert_eq!(heap.get_field(reused, 0).as_int(), Some(555));

        // And the live chain is STILL intact after reusing the hole.
        match heap.get_field(obj_a, 0) {
            Value::Object(Some(b)) => {
                assert_eq!(heap.get_field(b, 0).as_int(), Some(4242));
            }
            other => panic!("live chain corrupted after hole reuse: {other:?}"),
        }
    }

    #[test]
    fn promotion_after_enough_gcs() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj, 0, Value::Int(42));

        let mut roots = vec![obj];

        // Run PROMOTION_AGE minor GCs — object should be promoted on the last one
        for i in 0..PROMOTION_AGE {
            let result = heap.collect_garbage(&mut roots, &monitors);
            assert_eq!(result.stats.objects_copied, 1);
            let cur = roots[0];

            if i < PROMOTION_AGE - 1 {
                // Still in young gen
                assert!(
                    heap.is_in_young(cur.as_ptr()),
                    "Expected in young gen at iteration {i}"
                );
                assert_eq!(heap.get_header(cur).gc_age, i + 1);
            } else {
                // Should be promoted to old gen
                assert!(
                    heap.is_in_old(cur.as_ptr()),
                    "Expected in old gen after {PROMOTION_AGE} GCs"
                );
                assert!(heap.get_header(cur).is_old_gen());
            }
        }

        // Field value should still be intact
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(42));
    }

    #[test]
    fn write_barrier_marks_card_dirty() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        // First, promote an object to old gen
        let old_obj = heap.alloc_object(ClassId::new(0), 1);
        let mut roots = vec![old_obj];

        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }

        let promoted = roots[0];
        assert!(heap.is_in_old(promoted.as_ptr()));

        // Allocate a young object
        let young_obj = heap.alloc_object(ClassId::new(0), 0);
        assert!(heap.is_in_young(young_obj.as_ptr()));

        // Store young ref into old object
        heap.set_field(promoted, 0, Value::Object(Some(young_obj)));
        heap.write_barrier(promoted, Value::Object(Some(young_obj)));

        // Card should be dirty.
        //
        // T5.5.2 (HIGH-1 fix): the write barrier now batches into a
        // per-thread buffer instead of touching the global card-table
        // mutex on every store. Drain the buffer manually here so the
        // bitmap reflects the dirtying — the collector performs the
        // same flush + drain at GC entry.
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        let card_idx = (promoted.as_ptr() as usize - heap.card_table.base_addr())
            / crate::card_table::CARD_SIZE;
        assert!(heap.card_table.is_dirty(card_idx));
    }

    #[test]
    fn card_table_preserves_old_to_young_ref() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        // Promote an object to old gen
        let old_obj = heap.alloc_object(ClassId::new(0), 1);
        let mut roots = vec![old_obj];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }
        let promoted = roots[0];
        assert!(heap.is_in_old(promoted.as_ptr()));

        // Allocate a young object and store ref from old → young
        let young_obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(young_obj, 0, Value::Int(999));
        heap.set_field(promoted, 0, Value::Object(Some(young_obj)));
        heap.write_barrier(promoted, Value::Object(Some(young_obj)));

        // Minor GC: only the promoted object is a root (young_obj is NOT a root)
        // But the write barrier should have marked the card dirty, so the GC
        // should discover young_obj via the dirty card scan.
        let mut gc_roots = vec![promoted];
        let result = heap.collect_garbage(&mut gc_roots, &monitors);

        // young_obj should have been copied (reachable via dirty card)
        assert!(result.stats.objects_copied >= 1);

        // Verify the old→young reference was updated
        let promoted_after = gc_roots[0];
        match heap.get_field(promoted_after, 0) {
            Value::Object(Some(new_young)) => {
                assert_eq!(heap.get_field(new_young, 0).as_int(), Some(999));
            }
            _ => panic!("Expected promoted object to still reference young object"),
        }
    }

    #[test]
    fn needs_gc_threshold() {
        // Create heap with very small young gen
        let heap = GenerationalHeap::with_sizes(1024, 4096);
        assert!(!heap.needs_gc());

        // Allocate objects until threshold is exceeded
        let mut count = 0;
        while !heap.needs_gc() {
            heap.alloc_object(ClassId::new(0), 1);
            count += 1;
            if count > 100 {
                panic!("needs_gc never triggered");
            }
        }
        assert!(heap.needs_gc());
    }

    #[test]
    fn multiple_gc_cycles() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj1 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj1, 0, Value::Int(1));
        let mut roots = vec![obj1];

        // Cycle 1
        let r1 = heap.collect_garbage(&mut roots, &monitors);
        assert_eq!(r1.stats.objects_copied, 1);
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(1));

        // Cycle 2: add another object
        let obj2 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj2, 0, Value::Int(2));
        heap.set_field(roots[0], 0, Value::Object(Some(obj2)));
        roots = vec![roots[0]];

        let r2 = heap.collect_garbage(&mut roots, &monitors);
        assert_eq!(r2.stats.objects_copied, 2);
    }

    #[test]
    fn identity_hash_codes_unique() {
        let heap = small_gen_heap();
        let obj1 = heap.alloc_object(ClassId::new(0), 0);
        let obj2 = heap.alloc_object(ClassId::new(0), 0);
        assert_ne!(heap.identity_hash_code(obj1), heap.identity_hash_code(obj2),);
    }

    #[test]
    fn major_gc_frees_old_gen_garbage() {
        // Create a heap with small old gen to force major GC
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        // Promote several objects to old gen, then drop references to some
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..5 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        // Run enough minor GCs to promote all objects
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }

        // All should be in old gen now
        for root in &roots {
            assert!(
                heap.is_in_old(root.as_ptr()),
                "Expected object to be in old gen after promotion"
            );
        }

        // Verify values intact
        for (i, root) in roots.iter().enumerate() {
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32));
        }

        // Drop references to some objects (keep indices 0 and 2)
        let kept_root0 = roots[0];
        let kept_root2 = roots[2];
        roots = vec![kept_root0, kept_root2];

        // Manually trigger major GC (mark-compact)
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let old_used_before = old_gen.used();
            let _compact_map = GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);
            let old_used_after = old_gen.used();
            assert!(
                old_used_after < old_used_before,
                "Major GC should have freed memory: before={}, after={}",
                old_used_before,
                old_used_after,
            );
        }

        // Roots may have been relocated by compaction — use updated refs
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(0));
        assert_eq!(heap.get_field(roots[1], 0).as_int(), Some(2));
    }

    #[test]
    fn major_gc_preserves_old_gen_references() {
        let heap = GenerationalHeap::with_sizes(2 * 1024, 8 * 1024);
        let monitors = NoOpMonitors;

        // Create a chain: A -> B -> C (all will be promoted to old gen)
        let obj_c = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj_c, 0, Value::Int(333));

        let obj_b = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj_b, 0, Value::Object(Some(obj_c)));

        let obj_a = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));

        let mut roots = vec![obj_a];

        // Promote to old gen
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }

        let promoted_a = roots[0];
        assert!(heap.is_in_old(promoted_a.as_ptr()));

        // Also create some garbage in old gen
        let garbage = heap.alloc_object(ClassId::new(0), 0);
        let mut garbage_roots = vec![garbage];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut garbage_roots, &monitors);
        }
        // Drop reference to garbage

        // Run major GC (mark-compact) with only A as a root
        let mut major_roots = vec![promoted_a];
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut major_roots, &young_from, &mut old_gen);
        }

        // Walk the chain from the (possibly relocated) root: A -> B -> C
        let compacted_a = major_roots[0];
        match heap.get_field(compacted_a, 0) {
            Value::Object(Some(new_b)) => match heap.get_field(new_b, 0) {
                Value::Object(Some(new_c)) => {
                    assert_eq!(heap.get_field(new_c, 0).as_int(), Some(333));
                }
                _ => panic!("Expected B -> C reference"),
            },
            _ => panic!("Expected A -> B reference"),
        }
    }

    #[test]
    fn stress_alloc_gc_cycles() {
        let heap = GenerationalHeap::with_sizes(2 * 1024, 8 * 1024);
        let monitors = NoOpMonitors;

        let mut live_refs: Vec<ObjectRef> = Vec::new();

        // Rapid alloc/dealloc cycles
        for cycle in 0..20 {
            // Allocate some objects
            for j in 0..5 {
                let obj = heap.alloc_object(ClassId::new(0), 1);
                heap.set_field(obj, 0, Value::Int(cycle * 100 + j));
                live_refs.push(obj);
            }

            // Drop half the references
            if live_refs.len() > 5 {
                live_refs = live_refs.split_off(live_refs.len() / 2);
            }

            // Trigger GC if needed
            if heap.needs_gc() {
                heap.collect_garbage(&mut live_refs, &monitors);
            }
        }

        // Final GC
        heap.collect_garbage(&mut live_refs, &monitors);

        // All surviving objects should be readable without panicking
        for obj_ref in &live_refs {
            let _ = heap.get_field(*obj_ref, 0);
        }
    }

    #[test]
    fn stress_fill_young_and_promote() {
        let heap = GenerationalHeap::with_sizes(2 * 1024, 16 * 1024);
        let monitors = NoOpMonitors;

        let mut roots = Vec::new();

        // Fill young gen repeatedly, causing GC and promotions
        let obj_size = HEADER_SIZE + SLOT_SIZE; // 1 field object
        let approx_objects_per_young = 2 * 1024 / obj_size;

        for wave in 0..6 {
            // Allocate until GC triggers or young gen is full
            let mut wave_objs = Vec::new();
            for i in 0..approx_objects_per_young {
                let obj = match heap.try_alloc_object(ClassId::new(0), 1) {
                    Some(obj) => obj,
                    None => {
                        // Young gen full — trigger GC and retry
                        roots.append(&mut wave_objs);
                        heap.collect_garbage(&mut roots, &monitors);
                        match heap.try_alloc_object(ClassId::new(0), 1) {
                            Some(obj) => obj,
                            None => break, // Can't allocate even after GC
                        }
                    }
                };
                heap.set_field(obj, 0, Value::Int((wave * 1000 + i) as i32));
                wave_objs.push(obj);

                if heap.needs_gc() {
                    // Combine with existing roots
                    roots.append(&mut wave_objs);
                    heap.collect_garbage(&mut roots, &monitors);
                    break;
                }
            }
            roots.extend(wave_objs);

            // Keep only last 10 objects each wave to manage memory
            if roots.len() > 10 {
                roots = roots.split_off(roots.len() - 10);
            }
        }

        // Everything should be accessible
        for root in &roots {
            let _ = heap.get_field(*root, 0);
        }
    }

    #[test]
    fn gc_handles_cycles_in_young_gen() {
        let heap = small_gen_heap();
        let monitors = NoOpMonitors;

        let obj_a = heap.alloc_object(ClassId::new(0), 1);
        let obj_b = heap.alloc_object(ClassId::new(0), 1);

        // A -> B -> A (cycle)
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));
        heap.set_field(obj_b, 0, Value::Object(Some(obj_a)));

        let mut roots = vec![obj_a];
        let result = heap.collect_garbage(&mut roots, &monitors);

        assert_eq!(result.stats.objects_copied, 2); // both survive

        // Verify the cycle is intact
        let new_a = roots[0];
        match heap.get_field(new_a, 0) {
            Value::Object(Some(new_b)) => match heap.get_field(new_b, 0) {
                Value::Object(Some(back_to_a)) => {
                    assert_eq!(back_to_a.as_ptr(), new_a.as_ptr());
                }
                _ => panic!("Expected B->A cycle"),
            },
            _ => panic!("Expected A->B reference"),
        }
    }

    #[test]
    fn heap_expansion_on_gc_pressure() {
        // Small heap under allocation pressure: GC runs, and if reclamation
        // is low, the to-space expands for the next cycle.
        let heap = GenerationalHeap::with_capacity(64 * 1024); // 64 KB
        let monitors = NoOpMonitors;
        let _initial_cap = heap.young_semi_capacity();

        // Allocate objects, keeping half alive to create GC pressure.
        let mut live: Vec<ObjectRef> = Vec::new();
        for i in 0..200 {
            match heap.try_alloc_object(ClassId::new(0), 2) {
                Some(obj) => {
                    if i % 2 == 0 {
                        live.push(obj);
                    }
                }
                None => {
                    heap.collect_garbage(&mut live, &monitors);
                    let obj = heap.try_alloc_object(ClassId::new(0), 2)
                        .expect("alloc should succeed after GC");
                    if i % 2 == 0 {
                        live.push(obj);
                    }
                }
            }
        }
        // The heap should have detected low reclamation and expanded to-space
    }

    #[test]
    fn tlab_refill() {
        let heap = GenerationalHeap::with_capacity(1024 * 1024); // 1 MB
        let result = heap.refill_tlab(64 * 1024);
        assert!(result.is_some(), "refill_tlab should succeed");
        let (ptr, size) = result.unwrap();
        assert!(!ptr.is_null());
        assert!(size > 0);
    }

    #[test]
    fn gc_reclaims_dead_objects() {
        // Allocate objects, don't keep roots to them, GC should reclaim.
        let heap = GenerationalHeap::with_capacity(256 * 1024); // 256 KB
        // Fill up young gen — use try_alloc with manual GC on failure
        let mut dummy_roots: Vec<ObjectRef> = Vec::new();
        for _ in 0..100 {
            if heap.try_alloc_object(ClassId::new(0), 4).is_none() {
                heap.collect_garbage(&mut dummy_roots, &NoOpMonitors);
                let _obj = heap.try_alloc_object(ClassId::new(0), 4);
            }
        }
        // GC with empty roots — all objects are dead
        let mut roots = vec![];
        let result = heap.collect_garbage(&mut roots, &NoOpMonitors);
        assert!(result.stats.bytes_freed > 0, "GC should free dead objects");
    }

    #[test]
    fn gc_preserves_live_objects() {
        let heap = GenerationalHeap::with_capacity(256 * 1024);
        let live = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(live, 0, Value::Int(42));
        // Allocate dead objects
        for _ in 0..50 {
            if heap.try_alloc_object(ClassId::new(0), 2).is_none() {
                break; // Don't overflow
            }
        }
        let mut roots = vec![live];
        let _result = heap.collect_garbage(&mut roots, &NoOpMonitors);
        // The root should still be valid after GC
        let survived = roots[0];
        assert_eq!(heap.get_field(survived, 0).as_int(), Some(42));
    }

    #[test]
    fn heavy_allocation_with_gc_cycles() {
        // Simulates the workload that caused M4 heap exhaustion.
        let heap = GenerationalHeap::with_capacity(256 * 1024); // 256 KB
        let monitors = NoOpMonitors;
        let mut live_objs: Vec<ObjectRef> = Vec::new();

        for i in 0..1000 {
            let obj = heap.alloc_object(ClassId::new(0), 2);
            heap.set_field(obj, 0, Value::Int(i as i32));
            // Keep every 20th object alive
            if i % 20 == 0 {
                live_objs.push(obj);
            }
            // Trigger GC periodically
            if heap.needs_gc() {
                let mut roots: Vec<ObjectRef> = live_objs.clone();
                let _result = heap.collect_garbage(&mut roots, &monitors);
                // Update live_objs to new addresses
                live_objs = roots;
            }
        }

        // Verify all surviving objects
        for obj in &live_objs {
            let header = heap.get_header(*obj);
            assert_eq!(header.class_id, ClassId::new(0));
        }
    }

    // -----------------------------------------------------------------------
    // Session 26: GC Compaction tests
    // -----------------------------------------------------------------------

    #[test]
    fn s26_compact_eliminates_fragmentation() {
        // Create a heap with small old gen, promote objects, free some, compact
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        // Allocate 6 objects and promote them all to old gen
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..6 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i * 100));
            roots.push(obj);
        }
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }
        for root in &roots {
            assert!(heap.is_in_old(root.as_ptr()), "Object should be in old gen");
        }

        // Drop every other object (indices 1, 3, 5) — creates fragmentation
        let kept = vec![roots[0], roots[2], roots[4]];
        roots = kept;

        // Before compaction: multiple free blocks (fragmented)
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let free_blocks_before = old_gen.free_block_count();

            let compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // After compaction: exactly one free block (defragmented)
            assert_eq!(
                old_gen.free_block_count(),
                1,
                "After compaction, there should be exactly one contiguous free block"
            );

            // The largest free block should equal total free space
            let total_free = old_gen.capacity() - old_gen.used();
            assert_eq!(
                old_gen.largest_free_block(),
                total_free,
                "Largest free block should be all free space after compaction"
            );

            // Some objects should have moved
            assert!(
                !compact_map.is_empty() || free_blocks_before <= 1,
                "Objects should have been relocated (or heap was already compacted)"
            );
        }

        // Values should be preserved after compaction
        assert_eq!(heap.get_field(roots[0], 0).as_int(), Some(0));
        assert_eq!(heap.get_field(roots[1], 0).as_int(), Some(200));
        assert_eq!(heap.get_field(roots[2], 0).as_int(), Some(400));
    }

    #[test]
    fn s26_compact_updates_internal_references() {
        // Object A -> B -> C, all in old gen. Compact should update A->B and B->C.
        let heap = GenerationalHeap::with_sizes(2 * 1024, 8 * 1024);
        let monitors = NoOpMonitors;

        let obj_c = heap.alloc_object(ClassId::new(3), 1);
        heap.set_field(obj_c, 0, Value::Int(777));

        let obj_b = heap.alloc_object(ClassId::new(2), 1);
        heap.set_field(obj_b, 0, Value::Object(Some(obj_c)));

        let obj_a = heap.alloc_object(ClassId::new(1), 1);
        heap.set_field(obj_a, 0, Value::Object(Some(obj_b)));

        // Also allocate garbage between chain objects
        let garbage1 = heap.alloc_object(ClassId::new(0), 2);
        let garbage2 = heap.alloc_object(ClassId::new(0), 2);

        let mut roots = vec![obj_a, obj_b, obj_c, garbage1, garbage2];

        // Promote all to old gen
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }

        // Drop garbage references, keep only A (B and C reachable via A)
        roots = vec![roots[0]];

        // Run major GC (mark-compact)
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // Only 3 live objects (A, B, C) — garbage should be freed
            assert_eq!(
                old_gen.free_block_count(),
                1,
                "After compaction, old gen should have one free block"
            );
        }

        // Walk the chain from compacted A -> B -> C
        let a = roots[0];
        match heap.get_field(a, 0) {
            Value::Object(Some(b)) => {
                assert_eq!(heap.class_id_of(b), ClassId::new(2), "B should have class 2");
                match heap.get_field(b, 0) {
                    Value::Object(Some(c)) => {
                        assert_eq!(
                            heap.class_id_of(c),
                            ClassId::new(3),
                            "C should have class 3"
                        );
                        assert_eq!(heap.get_field(c, 0).as_int(), Some(777));
                    }
                    _ => panic!("Expected B -> C reference after compaction"),
                }
            }
            _ => panic!("Expected A -> B reference after compaction"),
        }
    }

    #[test]
    fn s26_compact_recovers_fragmented_space() {
        // Allocate objects in a pattern that fragments the heap, then verify
        // compaction recovers enough space for a large allocation that would
        // have failed without compaction.
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        // Allocate many small objects and promote them
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..8 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }

        // Record old gen usage before freeing
        let used_before_free = heap.old_gen.lock().used();

        // Free every other object — creates interleaved free blocks
        roots = roots
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % 2 == 0)
            .map(|(_, o)| o)
            .collect();

        // Run mark-compact via major GC
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // Used space should have decreased (half the objects freed)
            assert!(
                old_gen.used() < used_before_free,
                "Old gen used should decrease after freeing half the objects"
            );

            // Should be exactly 1 free block (fully compacted)
            assert_eq!(old_gen.free_block_count(), 1);

            // The single free block should be large enough for a new large alloc
            assert!(
                old_gen.largest_free_block() >= HEADER_SIZE + 4 * SLOT_SIZE,
                "Compacted free block should be large enough for a 4-field object"
            );
        }

        // Verify surviving objects have correct values (0, 2, 4, 6)
        for (i, root) in roots.iter().enumerate() {
            let expected = (i * 2) as i32;
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(expected));
        }
    }

    #[test]
    fn s26_compact_pointer_map_in_gc_result() {
        // Verify that compaction pointer_map flows through collect_garbage()
        // so the VM can update external roots.
        let heap = GenerationalHeap::with_sizes(2 * 1024, 2 * 1024);
        let monitors = NoOpMonitors;

        // Fill old gen to >75% to trigger major GC during minor GC
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..10 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        // Promote to old gen
        for _ in 0..PROMOTION_AGE {
            let result = heap.collect_garbage(&mut roots, &monitors);
            // Update roots from minor GC pointer_map
            for root in &mut roots {
                if let Some(&new_addr) = result.pointer_map.get(&(root.as_ptr() as usize)) {
                    // SAFETY: `new_addr` comes from the GC pointer map, pointing to a valid relocated object.
                    *root = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }

        // Drop most references — keep only 2 objects
        roots = vec![roots[0], roots[5]];

        // Allocate more to trigger minor GC which should cascade into major GC
        for _ in 0..20 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            // Don't keep reference — these are garbage
            let _ = obj;
        }

        let result = heap.collect_garbage(&mut roots, &monitors);

        // The pointer_map should contain entries (from minor GC and/or major GC compaction)
        // Surviving objects should still be accessible
        for root in &roots {
            let val = heap.get_field(*root, 0);
            assert!(
                val.as_int().is_some(),
                "Root should have valid Int field after compaction"
            );
        }
    }

    #[test]
    fn s26_compact_no_move_when_contiguous() {
        // If live objects are already contiguous, compaction should be a no-op
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..3 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        // Promote all to old gen
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }

        // All objects are contiguous (no gaps) — compaction should not move anything
        let ptrs_before: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();

        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);
            assert!(
                compact_map.is_empty(),
                "No objects should move when they are already contiguous"
            );
        }

        // Pointers should be unchanged
        let ptrs_after: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(ptrs_before, ptrs_after);
    }

    #[test]
    fn s26_compact_forwarding_ptrs_cleared() {
        // After compaction, no objects should have forwarding pointers set
        let heap = GenerationalHeap::with_sizes(2 * 1024, 4 * 1024);
        let monitors = NoOpMonitors;

        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..4 {
            let obj = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(obj, 0, Value::Int(i));
            roots.push(obj);
        }

        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }

        // Free some, then compact
        roots = vec![roots[0], roots[2]];
        {
            let young_from = heap.young_from.lock();
            let mut old_gen = heap.old_gen.lock();
            let _compact_map =
                GenerationalHeap::major_gc(&mut roots, &young_from, &mut old_gen);

            // Verify no forwarding pointers remain set
            for (obj_ptr, _) in old_gen.walk_objects() {
                // SAFETY: `obj_ptr` is from `old_gen.walk_objects()`, pointing to a valid old-gen object header.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                assert!(
                    header.forwarding_ptr.is_null(),
                    "Forwarding pointer should be cleared after compaction"
                );
                assert_eq!(
                    header.gc_flags & GC_FLAG_MARKED,
                    0,
                    "Mark bit should be cleared after compaction"
                );
            }
        }
    }

    // =========================================================================
    // S29: GC Write Barrier Verification
    // =========================================================================

    /// Helper: promote an object to old gen by surviving PROMOTION_AGE minor GCs.
    fn promote_to_old(heap: &GenerationalHeap, obj: ObjectRef) -> ObjectRef {
        let monitors = NoOpMonitors;
        let mut roots = vec![obj];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }
        assert!(heap.is_in_old(roots[0].as_ptr()), "object should be promoted to old gen");
        roots[0]
    }

    #[test]
    fn s29_random_graph_gc_never_collects_reachable() {
        // Property-based: build random object graph, GC, verify all reachable objects survive
        // with correct field values.
        let heap = GenerationalHeap::with_sizes(32 * 1024, 64 * 1024);
        let monitors = NoOpMonitors;

        // Allocate 50 objects, each tagged with its index
        let n = 50;
        let mut objs: Vec<ObjectRef> = Vec::new();
        for i in 0..n {
            let obj = heap.alloc_object(ClassId::new(0), 2); // field 0 = tag, field 1 = link
            heap.set_field(obj, 0, Value::Int(i as i32));
            heap.set_field(obj, 1, Value::Object(None));
            objs.push(obj);
        }

        // Build random links: obj[i].field[1] = obj[(i*7+3) % n] (deterministic pseudo-random)
        for i in 0..n {
            let target_idx = (i * 7 + 3) % n;
            heap.set_field(objs[i], 1, Value::Object(Some(objs[target_idx])));
        }

        // Use first 10 as roots
        let mut roots: Vec<ObjectRef> = objs[..10].to_vec();

        // Compute reachable set from roots via BFS
        let mut reachable: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        for i in 0..10 {
            reachable.insert(i);
            queue.push_back(i);
        }
        while let Some(idx) = queue.pop_front() {
            let target_idx = (idx * 7 + 3) % n;
            if reachable.insert(target_idx) {
                queue.push_back(target_idx);
            }
        }

        // Run GC
        heap.collect_garbage(&mut roots, &monitors);

        // Verify all roots survived and their tags are correct
        for (i, root) in roots.iter().enumerate() {
            let tag = heap.get_field(*root, 0);
            assert_eq!(tag.as_int(), Some(i as i32),
                "S29: root {} tag should be {} after GC", i, i);
        }

        // Walk reachable graph from roots and verify all tags
        let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut walk_queue: std::collections::VecDeque<ObjectRef> = std::collections::VecDeque::new();
        for r in &roots {
            walk_queue.push_back(*r);
        }
        while let Some(obj) = walk_queue.pop_front() {
            let tag = heap.get_field(obj, 0).as_int().unwrap() as usize;
            if !visited.insert(tag) { continue; }
            // Verify tag is in our expected reachable set
            assert!(reachable.contains(&tag),
                "S29: object with tag {} should be reachable", tag);
            // Follow link
            if let Value::Object(Some(next)) = heap.get_field(obj, 1) {
                walk_queue.push_back(next);
            }
        }
        // All reachable objects should have been visited
        assert_eq!(visited.len(), reachable.len(),
            "S29: all {} reachable objects should survive GC, found {}", reachable.len(), visited.len());
    }

    #[test]
    fn s29_cross_gen_old_to_young_chain() {
        // Test old→young reference chain: old object points to young, GC must preserve young.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Create and promote root to old gen
        let root = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(root, 0, Value::Int(100));
        let promoted = promote_to_old(&heap, root);

        // Create chain of young objects: y1 → y2 → y3
        let y1 = heap.alloc_object(ClassId::new(0), 2);
        let y2 = heap.alloc_object(ClassId::new(0), 2);
        let y3 = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(y1, 0, Value::Int(1));
        heap.set_field(y2, 0, Value::Int(2));
        heap.set_field(y3, 0, Value::Int(3));
        heap.set_field(y1, 1, Value::Object(Some(y2)));
        heap.set_field(y2, 1, Value::Object(Some(y3)));

        // Link old → y1
        heap.set_field(promoted, 1, Value::Object(Some(y1)));
        heap.write_barrier(promoted, Value::Object(Some(y1)));

        // GC with only old object as root — young chain must survive via dirty card
        let mut roots = vec![promoted];
        heap.collect_garbage(&mut roots, &monitors);

        // Walk chain and verify all tags
        let old_after = roots[0];
        assert_eq!(heap.get_field(old_after, 0).as_int(), Some(100));
        let y1_after = match heap.get_field(old_after, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: old→young link broken after GC"),
        };
        assert_eq!(heap.get_field(y1_after, 0).as_int(), Some(1));
        let y2_after = match heap.get_field(y1_after, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: y1→y2 link broken after GC"),
        };
        assert_eq!(heap.get_field(y2_after, 0).as_int(), Some(2));
        let y3_after = match heap.get_field(y2_after, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: y2→y3 link broken after GC"),
        };
        assert_eq!(heap.get_field(y3_after, 0).as_int(), Some(3));
    }

    #[test]
    fn s29_cross_gen_young_to_old_survives() {
        // Young object references old object — young must be collected if unreachable,
        // old must survive independently.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Promote an object to old gen
        let old_obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(old_obj, 0, Value::Int(42));
        let promoted = promote_to_old(&heap, old_obj);

        // Create young object pointing to old
        let young = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(young, 0, Value::Int(7));
        heap.set_field(young, 1, Value::Object(Some(promoted)));

        // Both as roots — both should survive
        let mut roots = vec![promoted, young];
        heap.collect_garbage(&mut roots, &monitors);

        let old_after = roots[0];
        let young_after = roots[1];
        assert_eq!(heap.get_field(old_after, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(young_after, 0).as_int(), Some(7));
        // Young's reference to old should be updated
        match heap.get_field(young_after, 1) {
            Value::Object(Some(r)) => assert_eq!(
                heap.get_field(r, 0).as_int(), Some(42),
                "S29: young→old reference should point to correct old object"
            ),
            _ => panic!("S29: young→old link broken after GC"),
        }
    }

    #[test]
    fn s29_card_table_multiple_dirty_cards() {
        // Multiple old objects on different cards all pointing to young — all must be preserved.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 64 * 1024);
        let monitors = NoOpMonitors;

        // Promote 5 objects — spread across old gen (different cards if possible)
        let mut old_objs = Vec::new();
        for i in 0..5 {
            let obj = heap.alloc_object(ClassId::new(0), 2);
            heap.set_field(obj, 0, Value::Int(i * 100));
            old_objs.push(obj);
        }
        let mut roots: Vec<ObjectRef> = old_objs.clone();
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }
        // All should be in old gen now
        for r in &roots {
            assert!(heap.is_in_old(r.as_ptr()), "S29: object should be promoted");
        }

        // Create 5 young objects, each referenced by one old object
        let mut young_objs = Vec::new();
        for i in 0..5 {
            let y = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(y, 0, Value::Int(i * 10 + 1));
            young_objs.push(y);
            heap.set_field(roots[i as usize], 1, Value::Object(Some(y)));
            heap.write_barrier(roots[i as usize], Value::Object(Some(y)));
        }

        // GC with only old objects as roots
        heap.collect_garbage(&mut roots, &monitors);

        // Verify all old→young links preserved
        for (i, old) in roots.iter().enumerate() {
            match heap.get_field(*old, 1) {
                Value::Object(Some(y)) => {
                    assert_eq!(heap.get_field(y, 0).as_int(), Some(i as i32 * 10 + 1),
                        "S29: old[{}]→young tag should be {}", i, i * 10 + 1);
                }
                _ => panic!("S29: old[{}]→young link broken after GC", i),
            }
        }
    }

    #[test]
    fn s29_card_table_overwrite_still_marks_new_target() {
        // Overwrite an old→young ref with a different young ref, verify new target preserved.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(1));
        let promoted = promote_to_old(&heap, obj);

        // First young target
        let y1 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(y1, 0, Value::Int(10));
        heap.set_field(promoted, 1, Value::Object(Some(y1)));
        heap.write_barrier(promoted, Value::Object(Some(y1)));

        // Overwrite with different young target
        let y2 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(y2, 0, Value::Int(20));
        heap.set_field(promoted, 1, Value::Object(Some(y2)));
        heap.write_barrier(promoted, Value::Object(Some(y2)));

        // GC: y1 is unreachable, y2 reachable via old
        let mut roots = vec![promoted];
        heap.collect_garbage(&mut roots, &monitors);

        match heap.get_field(roots[0], 1) {
            Value::Object(Some(y)) => {
                assert_eq!(heap.get_field(y, 0).as_int(), Some(20),
                    "S29: overwritten ref should point to y2 (tag=20)");
            }
            _ => panic!("S29: old→young link broken after overwrite + GC"),
        }
    }

    #[test]
    fn s29_stress_random_link_unlink_gc_cycles() {
        // Stress test: all objects are roots so GC updates all refs. Random link/unlink per cycle.
        let heap = GenerationalHeap::with_sizes(64 * 1024, 128 * 1024);
        let monitors = NoOpMonitors;

        let num_objects = 80;
        let num_cycles = 20;

        // All objects are roots — GC will update all of them
        let mut roots: Vec<ObjectRef> = Vec::new();
        for i in 0..num_objects {
            let obj = heap.alloc_object(ClassId::new(0), 2); // field 0=tag, field 1=link
            heap.set_field(obj, 0, Value::Int(i as i32));
            roots.push(obj);
        }

        for cycle in 0..num_cycles {
            // Deterministic pseudo-random linking among roots
            let n = roots.len();
            for i in 0..n {
                let target_idx = (i * 13 + cycle * 7 + 5) % n;
                heap.set_field(roots[i], 1, Value::Object(Some(roots[target_idx])));
            }

            // Unlink every 3rd
            for i in (0..n).step_by(3) {
                heap.set_field(roots[i], 1, Value::Object(None));
            }

            // Run GC — all objects are roots, so all survive and get updated
            heap.collect_garbage(&mut roots, &monitors);

            // Verify tags
            for (i, root) in roots.iter().enumerate() {
                let tag = heap.get_field(*root, 0).as_int().unwrap();
                assert_eq!(tag, i as i32,
                    "S29 cycle {}: root {} tag corrupted (got {})", cycle, i, tag);
            }

            // Verify linked objects are reachable with correct tags
            for i in 0..n {
                if i % 3 == 0 { continue; } // unlinked
                let target_idx = (i * 13 + cycle * 7 + 5) % n;
                match heap.get_field(roots[i], 1) {
                    Value::Object(Some(linked)) => {
                        let linked_tag = heap.get_field(linked, 0).as_int().unwrap();
                        assert_eq!(linked_tag, target_idx as i32,
                            "S29 cycle {}: obj[{}] link tag should be {}, got {}",
                            cycle, i, target_idx, linked_tag);
                    }
                    _ => panic!("S29 cycle {}: obj[{}] link broken", cycle, i),
                }
            }
        }
    }

    #[test]
    fn s29_cross_gen_old_to_young_ref_array() {
        // Old object has a reference array pointing to young objects — verify write barrier
        // preserves all young objects through GC.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Promote a reference array to old gen
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 4);
        let promoted_arr = promote_to_old(&heap, arr);

        // Create young objects and store in the old array
        let mut young_tags = Vec::new();
        for i in 0..4 {
            let y = heap.alloc_object(ClassId::new(0), 1);
            heap.set_field(y, 0, Value::Int(i * 11));
            young_tags.push(i * 11);
            heap.set_array_element(promoted_arr, i as usize, Value::Object(Some(y))).unwrap();
            heap.write_barrier(promoted_arr, Value::Object(Some(y)));
        }

        // GC with only old array as root
        let mut roots = vec![promoted_arr];
        heap.collect_garbage(&mut roots, &monitors);

        let arr_after = roots[0];
        for i in 0..4 {
            match heap.get_array_element(arr_after, i as usize).unwrap() {
                Value::Object(Some(y)) => {
                    assert_eq!(heap.get_field(y, 0).as_int(), Some(young_tags[i as usize]),
                        "S29: ref array[{}] young tag should be {}", i, young_tags[i as usize]);
                }
                _ => panic!("S29: ref array[{}] lost after GC", i),
            }
        }
    }

    #[test]
    fn s29_write_barrier_no_false_positives() {
        // Write barrier should NOT mark card for: young→young, old→old, non-reference stores.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        // Promote two objects to old gen
        let o1 = heap.alloc_object(ClassId::new(0), 2);
        let o2 = heap.alloc_object(ClassId::new(0), 1);
        let mut roots = vec![o1, o2];
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }
        let old1 = roots[0];
        let old2 = roots[1];
        assert!(heap.is_in_old(old1.as_ptr()));
        assert!(heap.is_in_old(old2.as_ptr()));

        // Clear card table.
        //
        // T5.5.2 (HIGH-1 fix): the write barrier batches into a
        // per-thread buffer; observing the bitmap requires draining
        // the buffer first. Each scenario below flushes + drains
        // before checking `take_dirty_cards()` so the assertion sees
        // the post-barrier state, exactly as the collector would at
        // GC entry.
        heap.card_table.clear_all();

        // Old → Old: should NOT dirty card
        heap.set_field(old1, 1, Value::Object(Some(old2)));
        heap.write_barrier(old1, Value::Object(Some(old2)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(heap.card_table.take_dirty_cards().is_empty(),
            "S29: old→old should not dirty card");

        // Young → Young: should NOT dirty card
        let y1 = heap.alloc_object(ClassId::new(0), 2);
        let y2 = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(y1, 1, Value::Object(Some(y2)));
        heap.write_barrier(y1, Value::Object(Some(y2)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(heap.card_table.take_dirty_cards().is_empty(),
            "S29: young→young should not dirty card");

        // Non-reference store: should NOT dirty card
        heap.set_field(old1, 0, Value::Int(999));
        heap.write_barrier(old1, Value::Int(999));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(heap.card_table.take_dirty_cards().is_empty(),
            "S29: non-ref store should not dirty card");

        // Old → Young: SHOULD dirty card
        heap.set_field(old1, 1, Value::Object(Some(y1)));
        heap.write_barrier(old1, Value::Object(Some(y1)));
        heap.card_table.flush_all();
        heap.card_table.drain_pending();
        assert!(!heap.card_table.take_dirty_cards().is_empty(),
            "S29: old→young SHOULD dirty card");
    }

    #[test]
    fn s29_gc_cycle_with_promotion_and_cross_gen_refs() {
        // Complex scenario: objects promoted over multiple GC cycles while maintaining
        // cross-generational references.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 64 * 1024);
        let monitors = NoOpMonitors;

        // Create chain: A → B → C → D
        let a = heap.alloc_object(ClassId::new(0), 2);
        let b = heap.alloc_object(ClassId::new(0), 2);
        let c = heap.alloc_object(ClassId::new(0), 2);
        let d = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(a, 0, Value::Int(1));
        heap.set_field(b, 0, Value::Int(2));
        heap.set_field(c, 0, Value::Int(3));
        heap.set_field(d, 0, Value::Int(4));
        heap.set_field(a, 1, Value::Object(Some(b)));
        heap.set_field(b, 1, Value::Object(Some(c)));
        heap.set_field(c, 1, Value::Object(Some(d)));

        let mut roots = vec![a];

        // Run PROMOTION_AGE GCs — all should be promoted
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }
        let a_old = roots[0];
        assert!(heap.is_in_old(a_old.as_ptr()), "S29: A should be in old gen");

        // Verify entire chain survives promotion
        let b_old = match heap.get_field(a_old, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: A→B link lost during promotion"),
        };
        assert_eq!(heap.get_field(b_old, 0).as_int(), Some(2));

        let c_old = match heap.get_field(b_old, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: B→C link lost during promotion"),
        };
        assert_eq!(heap.get_field(c_old, 0).as_int(), Some(3));

        let d_old = match heap.get_field(c_old, 1) {
            Value::Object(Some(r)) => r,
            _ => panic!("S29: C→D link lost during promotion"),
        };
        assert_eq!(heap.get_field(d_old, 0).as_int(), Some(4));

        // Now add a new young object hanging off the old chain
        let e = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(e, 0, Value::Int(5));
        heap.set_field(d_old, 1, Value::Object(Some(e)));
        heap.write_barrier(d_old, Value::Object(Some(e)));

        // GC again — e should survive via dirty card
        heap.collect_garbage(&mut roots, &monitors);
        let a_final = roots[0];
        // Walk full chain: A→B→C→D→E
        let mut current = a_final;
        for expected_tag in [1, 2, 3, 4] {
            assert_eq!(heap.get_field(current, 0).as_int(), Some(expected_tag));
            current = match heap.get_field(current, 1) {
                Value::Object(Some(r)) => r,
                _ => panic!("S29: chain broken at tag={}", expected_tag),
            };
        }
        assert_eq!(heap.get_field(current, 0).as_int(), Some(5),
            "S29: E (young, linked from old D) should survive");
    }

    #[test]
    fn s29_stress_1000_objects_random_graph_gc() {
        // Large stress test: 1000 objects, random links, multiple GC cycles.
        let heap = GenerationalHeap::with_sizes(256 * 1024, 512 * 1024);
        let monitors = NoOpMonitors;

        let n = 1000;
        let mut objs: Vec<ObjectRef> = Vec::new();
        for i in 0..n {
            let obj = heap.alloc_object(ClassId::new(0), 2);
            heap.set_field(obj, 0, Value::Int(i as i32));
            objs.push(obj);
        }

        // Build deterministic random graph
        for i in 0..n {
            let target = (i * 37 + 17) % n;
            heap.set_field(objs[i], 1, Value::Object(Some(objs[target])));
        }

        // All objects are roots initially
        let mut roots = objs.clone();

        // GC cycle 1: everything should survive
        heap.collect_garbage(&mut roots, &monitors);
        for (i, root) in roots.iter().enumerate() {
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32),
                "S29 stress: object {} tag corrupted after GC1", i);
        }

        // Drop half the roots — only first 500
        roots.truncate(500);

        // GC cycle 2
        heap.collect_garbage(&mut roots, &monitors);
        for (i, root) in roots.iter().enumerate() {
            assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32),
                "S29 stress: root {} tag corrupted after GC2", i);
        }

        // GC cycles 3-5: repeatedly compact
        for cycle in 3..=5 {
            heap.collect_garbage(&mut roots, &monitors);
            for (i, root) in roots.iter().enumerate() {
                assert_eq!(heap.get_field(*root, 0).as_int(), Some(i as i32),
                    "S29 stress: root {} tag corrupted after GC{}", i, cycle);
            }
        }
    }

    #[test]
    fn s29_barrier_correctness_promoted_then_linked() {
        // Edge case: object promoted during GC, then immediately linked to a young object.
        // Next GC must see the cross-gen ref via write barrier.
        let heap = GenerationalHeap::with_sizes(16 * 1024, 32 * 1024);
        let monitors = NoOpMonitors;

        let obj = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(obj, 0, Value::Int(42));
        let mut roots = vec![obj];

        // Promote object
        for _ in 0..PROMOTION_AGE {
            heap.collect_garbage(&mut roots, &monitors);
        }
        let promoted = roots[0];
        assert!(heap.is_in_old(promoted.as_ptr()));

        // Allocate young, link from old, and immediately GC
        let young = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(young, 0, Value::Int(99));
        heap.set_field(promoted, 1, Value::Object(Some(young)));
        heap.write_barrier(promoted, Value::Object(Some(young)));

        // GC: young object's only root is via old object + write barrier
        heap.collect_garbage(&mut roots, &monitors);

        let after = roots[0];
        match heap.get_field(after, 1) {
            Value::Object(Some(y)) => {
                assert_eq!(heap.get_field(y, 0).as_int(), Some(99),
                    "S29: young object linked from just-promoted old should survive");
            }
            _ => panic!("S29: old→young link lost after promotion+barrier+GC"),
        }
    }
}
