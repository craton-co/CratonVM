//! Unified heap abstraction for the VM.
//!
//! `VmHeap` wraps the available GC implementations (GenerationalHeap, G1Collector)
//! behind a single type, so the VM code doesn't need to be generic or use trait objects.

use crate::collector::{GarbageCollector, MonitorCleanup};
use crate::concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState};
use crate::g1::{G1Collector, G1CollectorConfig};
use crate::gc::GcResult;
use crate::gen_heap::GenerationalHeap;
use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
use crate::old_gen::OldGen;
use crate::satb::SatbQueue;
use rustjvm_types::{ClassId, ObjectRef, Value};
use std::sync::Arc;

/// Which GC backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcBackend {
    /// Generational semi-space + old-gen mark-sweep (default).
    Generational,
    /// G1 (Garbage-First) region-based collector.
    G1,
}

/// Unified heap wrapping either GenerationalHeap or G1Collector.
///
/// Provides the same API surface as `GenerationalHeap` so existing call sites
/// work unchanged. For G1-specific methods that don't apply, reasonable defaults
/// are returned.
pub enum VmHeap {
    Generational(GenerationalHeap),
    G1(G1Collector),
}

// Safety: both inner types are already Send + Sync.
unsafe impl Send for VmHeap {}
unsafe impl Sync for VmHeap {}

/// Macro to dispatch a method call to the inner heap implementation.
macro_rules! dispatch {
    ($self:expr, $method:ident ( $($arg:expr),* ) ) => {
        match $self {
            VmHeap::Generational(h) => h.$method($($arg),*),
            VmHeap::G1(h) => h.$method($($arg),*),
        }
    };
}

impl VmHeap {
    /// Create a new VmHeap with the specified backend and total capacity.
    pub fn new(backend: GcBackend, total_bytes: usize) -> Self {
        match backend {
            GcBackend::Generational => {
                VmHeap::Generational(GenerationalHeap::with_capacity(total_bytes))
            }
            GcBackend::G1 => {
                let mut config = G1CollectorConfig::default();
                config.heap_size = total_bytes;
                // Scale region size: 1 MB for heaps < 4 GB, 2 MB for larger
                if total_bytes > 4 * 1024 * 1024 * 1024 {
                    config.region_size = 2 * 1024 * 1024;
                }
                VmHeap::G1(G1Collector::new(config))
            }
        }
    }

    // =====================================================================
    // Core allocation (both backends implement GarbageCollector trait)
    // =====================================================================

    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        dispatch!(self, alloc_object(class_id, num_fields))
    }

    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        dispatch!(self, alloc_array(class_id, element_type, length))
    }

    /// Try to allocate an object. Returns `None` on OOM (caller should trigger GC and retry).
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_object(class_id, num_fields),
            VmHeap::G1(h) => h.try_alloc_object(class_id, num_fields),
        }
    }

    /// Allocate a new Java object and initialize primitive-typed slots to
    /// their spec-mandated typed zero based on JVM field descriptor bytes.
    ///
    /// This is the safe allocation entry point — it fixes the
    /// ConcurrentHashMap.initTable CAS livelock by guaranteeing that an
    /// unwritten `int` slot reads back as `Value::Int(0)` instead of
    /// `Value::Object(None)`. See
    /// [`crate::heap::default_value_for_descriptor`] for the mapping.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        match self {
            VmHeap::Generational(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            VmHeap::G1(h) => {
                h.alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
        }
    }

    /// Fallible variant: returns `None` when the heap is out of space.
    pub fn try_alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
            VmHeap::G1(h) => {
                h.try_alloc_object_with_descriptors(class_id, num_fields, descriptor_bytes)
            }
        }
    }

    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_array(class_id, element_type, length),
            VmHeap::G1(h) => h.try_alloc_array(class_id, element_type, length),
        }
    }

    // =====================================================================
    // Header access
    // =====================================================================

    pub fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        dispatch!(self, get_header(obj))
    }

    pub fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        dispatch!(self, class_id_of(obj))
    }

    /// NEW-1.5 conservative validity check for a *raw stack-spill address*.
    ///
    /// Used by JIT frame root scanning to filter spurious values: returns
    /// `Some(ObjectRef)` only if `addr` lands on a live object header in
    /// this heap. See [`crate::gen_heap::GenerationalHeap::is_object_address`]
    /// for the full contract.
    pub fn is_object_address(&self, addr: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.is_object_address(addr),
            VmHeap::G1(h) => h.is_object_address(addr),
        }
    }

    /// T1.7.1 — Brooks-pointer read barrier.
    ///
    /// Consults the object's compact header: if the `LockState` is
    /// `Forwarded`, the object has been evacuated by a concurrent
    /// compaction cycle and the real object lives at
    /// `header.forwarding_ptr()`. The barrier self-heals stale
    /// pointers by returning the forwarded address.
    ///
    /// Under stop-the-world GC this is **always a no-op fast path**
    /// because the header's lock state is only set to `Forwarded`
    /// during evacuation, and the collector updates all root
    /// references before resuming mutators. The barrier becomes
    /// meaningful when concurrent compaction is enabled and a
    /// mutator loads a reference *while* the collector is relocating
    /// the target object.
    ///
    /// The fast path is a single load + mask + branch: an empty
    /// inline-able sequence on both x86-64 and AArch64. The slow
    /// path (forwarded) is one additional mask operation on the
    /// 30-bit shifted address.
    ///
    /// Idempotent: calling `load_and_forward` on an already-forwarded
    /// object returns the *terminal* forwarding target in one hop
    /// because Brooks pointers only ever point "forward" (the
    /// evacuation pass never builds chains longer than one link).
    ///
    /// # Safety
    ///
    /// The caller must hold a live root to `obj` (or the GC must be
    /// stopped) so the header read is against a valid object. The
    /// returned `ObjectRef` points into the same heap region.
    #[inline]
    pub fn load_and_forward(&self, obj: ObjectRef) -> ObjectRef {
        // Fast path: read the header once, check the forwarded bit,
        // and return the original pointer when unforwarded.
        let header = self.get_compact_header(obj);
        if !header.is_forwarded() {
            return obj;
        }
        // Slow path: extract the forwarding target and return the
        // corresponding ObjectRef. The forwarded address is always
        // 8-byte aligned and within the same heap arena as the
        // original object.
        let addr = header.forwarding_ptr();
        if addr == 0 {
            // Defensive fallback: a zero forwarding pointer means
            // either an uninitialized header or a concurrent-GC
            // race we didn't anticipate. Returning the original
            // pointer is always safe because the original object
            // still exists in memory until the evacuation epoch
            // ends; the barrier just loses the optimization.
            return obj;
        }
        // SAFETY: the forwarding pointer was installed by the GC
        // and points at a valid object header within this heap.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Read the compact header for an object. Thin wrapper over the
    /// backend-specific path. Used by [`Self::load_and_forward`].
    ///
    /// The return is a *copy* of the 64-bit header word wrapped in a
    /// `CompactHeader` so the caller can inspect it without holding
    /// a borrow into the heap.
    #[inline]
    pub fn get_compact_header(&self, obj: ObjectRef) -> crate::compact_header::CompactHeader {
        // Every heap backend lays out the object header as a 64-bit
        // word at offset 0 from the ObjectRef pointer. Read the
        // word directly and reinterpret as a CompactHeader.
        //
        // SAFETY: ObjectRef is a validated heap address pointing at
        // a live object header. The load is aligned (headers are
        // 8-byte aligned by construction).
        let ptr = obj.as_ptr() as *const u64;
        let raw = unsafe { std::ptr::read(ptr) };
        crate::compact_header::CompactHeader::from_raw(raw)
    }

    pub fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        dispatch!(self, kind_of(obj))
    }

    pub fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        dispatch!(self, element_type_of(obj))
    }

    pub fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        dispatch!(self, identity_hash_code(obj))
    }

    /// H1: mint a fresh non-zero identity hash code for use at object
    /// construction time. Matches the value the slow-path allocators
    /// (`alloc_object` / `alloc_array`) write into the header. The TLAB
    /// fast path in `interpreter::init_object_header` calls this so all
    /// freshly allocated objects have a non-zero `identity_hash_code`
    /// header field, eliminating false-positive "Stale pointer detected"
    /// warnings when a fresh `new Object()` (cid=0, fields=0, hash=0)
    /// otherwise produces an all-zero first 16 bytes that the detector
    /// can't distinguish from genuine stale memory.
    pub fn next_identity_hash(&self) -> i32 {
        dispatch!(self, next_hash())
    }

    // =====================================================================
    // Field access
    // =====================================================================

    pub fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        dispatch!(self, get_field(obj, index))
    }

    pub fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        dispatch!(self, set_field(obj, index, value))
    }

    pub fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        dispatch!(self, get_field_volatile(obj, index))
    }

    pub fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        dispatch!(self, set_field_volatile(obj, index, value))
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Descriptor-aware get — dispatches to the underlying heap's
    /// [`get_field_as`](crate::collector::GarbageCollector::get_field_as),
    /// which normalizes the returned `Value` against the declared field
    /// type. See the trait-level documentation for the coercion rules.
    pub fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
        dispatch!(self, get_field_as(obj, index, desc_byte))
    }

    /// Volatile descriptor-aware get.
    pub fn get_field_volatile_as(
        &self,
        obj: ObjectRef,
        index: usize,
        desc_byte: u8,
    ) -> Value {
        dispatch!(self, get_field_volatile_as(obj, index, desc_byte))
    }

    /// Descriptor-aware set.
    pub fn set_field_as(
        &self,
        obj: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        dispatch!(self, set_field_as(obj, index, value, desc_byte))
    }

    /// Volatile descriptor-aware set.
    pub fn set_field_volatile_as(
        &self,
        obj: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        dispatch!(self, set_field_volatile_as(obj, index, value, desc_byte))
    }

    // =====================================================================
    // Array access
    // =====================================================================

    pub fn array_length(&self, obj: ObjectRef) -> usize {
        dispatch!(self, array_length(obj))
    }

    /// Returns the element type of an array object.
    /// For non-array objects the returned value is meaningless.
    pub fn array_element_type(&self, obj: ObjectRef) -> Option<ArrayElementType> {
        let header = self.get_header(obj);
        if header.kind == ObjectKind::Array {
            Some(header.element_type)
        } else {
            None
        }
    }

    pub fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        dispatch!(self, get_array_element(obj, index))
    }

    pub fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        dispatch!(self, set_array_element(obj, index, value))
    }

    /// Read an array element with auto-unboxing of wrapper types.
    /// G1 falls back to plain get_array_element (no unboxing support yet).
    pub fn get_array_element_unboxing(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        match self {
            VmHeap::Generational(h) => h.get_array_element_unboxing(obj, index),
            VmHeap::G1(h) => h.get_array_element(obj, index),
        }
    }

    /// Bulk-read a char[] array into a `Vec<u16>`.
    pub fn read_char_array_bulk(&self, obj: ObjectRef) -> Vec<u16> {
        match self {
            VmHeap::Generational(h) => h.read_char_array_bulk(obj),
            VmHeap::G1(_h) => {
                // G1 uses the same object layout, so we can read directly
                let header = self.get_header(obj);
                let len = header.array_length as usize;
                let mut out = vec![0u16; len];
                // SAFETY: `obj` is a live `ObjectRef` whose header we
                // just read. `HEADER_SIZE` offset lands exactly at
                // the start of the array payload, and `len * 2` is
                // the payload byte length (char = 2 bytes). The
                // destination slice was just allocated with `len`
                // u16 elements, so writing `len * 2` bytes is in
                // bounds. Non-overlapping because source is in the
                // heap arena and destination is the freshly-
                // allocated `out` Vec on the caller's stack.
                unsafe {
                    let src = obj.as_ptr().add(HEADER_SIZE);
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 2);
                }
                out
            }
        }
    }

    /// Raw pointer to array data region (after header).
    pub fn array_data_ptr(&self, obj: ObjectRef) -> *mut u8 {
        // SAFETY: `obj` is a live `ObjectRef` in the heap arena;
        // `HEADER_SIZE` offset is the layout-documented start of
        // the array payload region. The returned pointer is valid
        // for the lifetime of the object (which the caller must
        // not drop while holding the pointer).
        unsafe { obj.as_ptr().add(HEADER_SIZE) }
    }

    // =====================================================================
    // GC operations
    // =====================================================================

    pub fn needs_gc(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.needs_gc(),
            VmHeap::G1(h) => h.needs_gc(),
        }
    }

    pub fn collect_garbage(&self, roots: &mut [ObjectRef], monitors: &dyn MonitorCleanup) -> GcResult {
        match self {
            VmHeap::Generational(h) => h.collect_garbage(roots, monitors),
            VmHeap::G1(h) => h.collect_garbage(roots, monitors),
        }
    }

    /// GC with finalizer-aware resurrection (semispace only; G1 falls back
    /// to normal collect since `is_addr_live` handles non-collected regions).
    pub fn collect_garbage_with_finalizers(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        match self {
            VmHeap::Generational(h) => {
                h.collect_garbage_with_finalizers(roots, finalizer_addrs, monitors)
            }
            VmHeap::G1(h) => {
                // G1's is_addr_live handles this correctly — dead finalizable
                // objects in non-collected regions are still accessible.
                let result = h.collect_garbage(roots, monitors);
                (result, Vec::new())
            }
        }
    }

    pub fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        match self {
            VmHeap::Generational(h) => h.write_barrier(obj, stored_value),
            VmHeap::G1(h) => h.write_barrier(obj, stored_value),
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        match self {
            VmHeap::Generational(h) => h.allocated_bytes(),
            VmHeap::G1(h) => h.allocated_bytes(),
        }
    }

    /// Return the total number of GC collections performed so far.
    pub fn collection_count(&self) -> u64 {
        match self {
            VmHeap::Generational(_) => 0, // generational heap doesn't track this
            VmHeap::G1(h) => h.collection_count(),
        }
    }

    /// Return the total heap capacity in bytes.
    pub fn heap_capacity(&self) -> usize {
        match self {
            VmHeap::Generational(h) => {
                // 2 semi-spaces (only one active) + old gen
                h.young_semi_capacity() + h.old_gen_capacity()
            }
            VmHeap::G1(h) => h.heap_capacity(),
        }
    }

    /// Return young generation (used, capacity) in bytes.
    pub fn young_gen_stats(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => {
                let from = h.young_from_used();
                let cap = h.young_semi_capacity();
                (from, cap)
            }
            VmHeap::G1(h) => h.eden_stats(),
        }
    }

    /// Return old generation (used, capacity) in bytes.
    pub fn old_gen_stats(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => {
                let cap = h.old_gen_capacity();
                let used = h.old_gen_used();
                (used, cap)
            }
            VmHeap::G1(h) => h.old_gen_stats(),
        }
    }

    // =====================================================================
    // GenerationalHeap-specific methods (no-ops for G1)
    // =====================================================================

    /// SATB barrier for concurrent marking.
    /// For G1, logs the old reference to the global SATB queue when marking is active.
    pub fn satb_barrier(&self, old_value: Value) {
        match self {
            VmHeap::Generational(h) => h.satb_barrier(old_value),
            VmHeap::G1(h) => {
                if let Value::Object(Some(obj_ref)) = old_value {
                    h.satb_pre_barrier(obj_ref.as_ptr() as usize);
                }
            }
        }
    }

    /// Check if old generation needs GC (generational only).
    pub fn old_gen_needs_gc(&self) -> bool {
        match self {
            VmHeap::Generational(h) => h.old_gen_needs_gc(),
            VmHeap::G1(_) => false, // G1 manages its own concurrent marking
        }
    }

    /// Get old generation base pointer and capacity (generational only).
    pub fn old_gen_info(&self) -> (usize, usize) {
        match self {
            VmHeap::Generational(h) => h.old_gen_info(),
            VmHeap::G1(_) => (0, 0),
        }
    }

    /// Lock the old generation for direct access (generational only).
    /// Returns None for G1.
    pub fn old_gen_lock(&self) -> Option<parking_lot::MutexGuard<'_, OldGen>> {
        match self {
            VmHeap::Generational(h) => Some(h.old_gen_lock()),
            VmHeap::G1(_) => None,
        }
    }

    /// Enable concurrent GC support (generational only).
    pub fn enable_concurrent_gc(
        &mut self,
        satb_queue: Arc<SatbQueue>,
        gc_state: Arc<ConcurrentGcState>,
    ) {
        match self {
            VmHeap::Generational(h) => h.enable_concurrent_gc(satb_queue, gc_state),
            VmHeap::G1(_) => {} // G1 has built-in concurrent marking
        }
    }

    /// Probe whether a young-gen allocation would succeed (generational only).
    /// G1 always returns Some(()) since it allocates from regions.
    pub fn try_alloc_young_probe(&self, size: usize) -> Option<()> {
        match self {
            VmHeap::Generational(h) => h.try_alloc_young_probe(size),
            VmHeap::G1(_) => Some(()),
        }
    }

    /// Returns whether this heap is using the G1 collector.
    pub fn is_g1(&self) -> bool {
        matches!(self, VmHeap::G1(_))
    }

    /// Check if G1 should start concurrent marking (IHOP threshold crossed).
    pub fn g1_should_start_marking(&self) -> bool {
        match self {
            VmHeap::G1(g1) => {
                // Check if old gen bytes exceed the G1 marking threshold (IHOP)
                g1.old_gen_bytes() > g1.marking_threshold_bytes() && g1.marking_threshold_bytes() > 0
            }
            VmHeap::Generational(_) => false,
        }
    }

    /// Check if G1 concurrent marking is currently active.
    pub fn g1_is_marking_active(&self) -> bool {
        match self {
            VmHeap::G1(g1) => g1.gc_state.is_marking_active(),
            VmHeap::Generational(_) => false,
        }
    }

    /// Start G1 concurrent mark cycle: activate SATB, clear bitmap.
    pub fn g1_start_concurrent_mark(&self) {
        if let VmHeap::G1(g1) = self {
            g1.start_concurrent_mark();
        }
    }

    /// Mark roots into G1's mark bitmap.
    pub fn g1_mark_roots(&self, roots: &[rustjvm_types::ObjectRef]) {
        if let VmHeap::G1(g1) = self {
            g1.remark(roots); // remark marks roots + drains SATB
        }
    }

    /// Perform one step of G1 concurrent marking. Returns true when done.
    pub fn g1_concurrent_mark_step(&self, work_amount: usize) -> bool {
        match self {
            VmHeap::G1(g1) => g1.concurrent_mark_step(work_amount),
            VmHeap::Generational(_) => true,
        }
    }

    /// Signal that G1 concurrent marking is complete.
    /// Sets phase to ConcurrentSweep and runs cleanup.
    pub fn g1_signal_marking_complete(&self) {
        if let VmHeap::G1(g1) = self {
            g1.gc_state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
            g1.cleanup();
            g1.gc_state.set_phase(ConcurrentGcPhase::Idle);
        }
    }

    /// Enable GC logging (verbose:gc).
    pub fn enable_gc_logging(&self) {
        match self {
            VmHeap::G1(g1) => g1.enable_gc_logging(),
            VmHeap::Generational(_) => {
                // Generational heap doesn't have a logging toggle yet,
                // but log that GC logging was requested.
                tracing::info!("[GC] Verbose GC logging enabled (generational collector)");
            }
        }
    }

    /// Get the number of fields (slots) in an object.
    pub fn num_fields(&self, obj: ObjectRef) -> usize {
        dispatch!(self, get_header(obj)).num_slots as usize
    }

    /// Carve out a TLAB from the young generation (generational) or Eden region (G1).
    /// Returns `Some((ptr, size))` on success.
    pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
        match self {
            VmHeap::Generational(h) => h.refill_tlab(requested_size),
            VmHeap::G1(h) => h.refill_tlab(requested_size),
        }
    }

    /// Check if a raw address is within a live (non-Free) region of the heap.
    /// For generational GC, always returns false (not applicable).
    /// For G1, checks whether the address falls within an allocated portion
    /// of a non-Free region — used by reference processing to distinguish
    /// live objects in non-collected regions from dead objects.
    pub fn is_addr_live(&self, addr: usize) -> bool {
        match self {
            VmHeap::Generational(_) => false,
            VmHeap::G1(h) => h.is_addr_in_live_region(addr),
        }
    }

    /// Walk all live objects in the heap (both generations / all regions).
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        dispatch!(self, walk_objects())
    }
}
