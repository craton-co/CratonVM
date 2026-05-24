// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
use cratonvm_types::{ClassId, ObjectRef, Value};
use std::sync::Arc;

/// Which GC backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcBackend {
    /// Generational semi-space + old-gen mark-sweep (default).
    Generational,
    /// G1 (Garbage-First) region-based collector.
    G1,
}

// ─── GPU-offload coordination (Phase 6 item 1) ───────────────────────────
//
// Process-wide counter tracking the number of live SafepointTokens
// (see `crate::safepoint`). While non-zero, every collect_garbage
// entry point on `GenerationalHeap` / `G1Collector` spin-yields
// instead of running a collection.
//
// Why process-wide rather than per-heap: there is always exactly one
// VmHeap per CratonVM process, so the distinction is academic. A
// `static` keeps the safepoint API zero-cost (no `&Heap` plumbed
// through the `enter_gpu_critical` API).
//
// The legacy `Heap` struct in `heap.rs` keeps its own per-instance
// counter for backwards compatibility with the Phase 1 tests — the
// two paths are independent.

/// Process-wide GPU-critical-section counter. Public so the VM
/// crate can manage the count manually from `runtime::offload` for
/// the Phase 7 deferred-finalize path (which crosses thread
/// boundaries and therefore can't use the `!Send` `SafepointToken`).
#[cfg(feature = "gpu-offload")]
pub static GPU_CRITICAL_COUNT: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Spin-yield until every live `SafepointToken` has been dropped.
///
/// Called from the GC entry points on `GenerationalHeap` and
/// `G1Collector` before starting a collection cycle, so a kernel
/// running under a token never observes its inputs being moved.
///
/// Emits a tracing warning after [`crate::safepoint::GPU_CRITICAL_DEADLINE_SECS`]
/// seconds so an indefinitely-blocked collector still surfaces in logs.
#[cfg(feature = "gpu-offload")]
pub fn wait_for_gpu_critical_drain() {
    use std::sync::atomic::Ordering;
    if GPU_CRITICAL_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(
        crate::safepoint::GPU_CRITICAL_DEADLINE_SECS,
    );
    let mut warned = false;
    loop {
        std::thread::yield_now();
        let now = GPU_CRITICAL_COUNT.load(Ordering::Acquire);
        if now == 0 {
            return;
        }
        if !warned && start.elapsed() >= deadline {
            tracing::warn!(
                "GC delayed >{}s by GPU critical section — {} active tokens",
                crate::safepoint::GPU_CRITICAL_DEADLINE_SECS,
                now,
            );
            warned = true;
        }
    }
}

/// No-op when the gpu-offload feature is disabled. Callers in the
/// GC entry points can use this unconditionally.
#[cfg(not(feature = "gpu-offload"))]
#[inline(always)]
pub fn wait_for_gpu_critical_drain() {}

// ─── Task #25: SATB triad-ordering debug assertion ───────────────────────
//
// In debug builds, every `write_barrier_pre` call arms a per-thread
// sentinel that the next `write_barrier` call clears. If `write_barrier`
// observes the sentinel still armed at the start of the *next*
// `write_barrier_pre` (i.e. two pre-barriers with no intervening post),
// it means a store path called `write_barrier_pre` and then forgot to
// follow up with the matching post-store `write_barrier` — the exact
// shape of the SATB-without-post bug. The sentinel is `thread_local`
// and entirely compiled out in release builds, so the production hot
// path is unchanged.

#[cfg(debug_assertions)]
thread_local! {
    static PENDING_PRE_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(debug_assertions)]
#[inline]
fn arm_pending_pre_barrier() {
    PENDING_PRE_BARRIER.with(|f| {
        // If we are arming AGAIN without an intervening `write_barrier`,
        // a previous (pre, store, post) triad was left incomplete. The
        // assertion catches missing post-store calls without crashing
        // release builds.
        debug_assert!(
            !f.get(),
            "write_barrier_pre called twice with no intervening write_barrier — \
             SATB triad order violated; missing post-store barrier somewhere on \
             the prior reference store"
        );
        f.set(true);
    });
}

#[cfg(debug_assertions)]
#[inline]
fn clear_pending_pre_barrier() {
    PENDING_PRE_BARRIER.with(|f| f.set(false));
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

    /// Loose validity check: alignment + heap-region containment.
    ///
    /// Unlike [`Self::is_object_address`] this does NOT read the object
    /// header. Used by operand-stack root scanning to root ambiguous
    /// JVM-long-vs-jobject slots without rejecting legitimate references
    /// whose header is transiently unreadable (interior pointers, mid-
    /// initialisation slots, or stale-bit-pattern Long slots that the GC
    /// is well-equipped to ignore via its own size-sanity guard).
    pub fn is_heap_addr(&self, addr: usize) -> Option<ObjectRef> {
        match self {
            VmHeap::Generational(h) => h.is_heap_addr(addr),
            VmHeap::G1(h) => h.is_heap_addr(addr),
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
    ///
    /// # Backend compatibility
    ///
    /// All current `VmHeap` backends (`Heap`, `GenerationalHeap`,
    /// `G1Collector`) allocate objects with the **full** 32-byte
    /// `ObjectHeader` layout — none of them use the compact 64-bit
    /// header format. This barrier therefore reads
    /// `ObjectHeader.forwarding_ptr` directly. Decoding the first 8
    /// bytes of a full header as a `CompactHeader` would alias the
    /// `forwarding_ptr` bit-pattern onto unrelated header fields
    /// (`_padding`, `identity_hash_code`, `array_length`) and could
    /// cause `is_forwarded` to spuriously fire. A backend that adopts
    /// compact headers in the future MUST update this method (and
    /// gate the alternate decode path on the appropriate cfg).
    #[inline]
    pub fn load_and_forward(&self, obj: ObjectRef) -> ObjectRef {
        // SAFETY: the caller guarantees `obj` is a live root. Every
        // current backend lays out `ObjectHeader` at offset 0 of the
        // ObjectRef pointer with `forwarding_ptr` at the documented
        // structural offset; reading the field is well-formed.
        let header = unsafe { &*(obj.as_ptr() as *const crate::heap::ObjectHeader) };
        if !header.is_forwarded() {
            return obj;
        }
        let addr = header.forwarding_address();
        if addr.is_null() {
            // Defensive fallback (shouldn't happen — is_forwarded()
            // already checks for a non-null pointer — but kept for
            // belt-and-braces against concurrent-GC races we did not
            // anticipate). Returning the original pointer is always
            // safe because the original object still exists in memory
            // until the evacuation epoch ends.
            return obj;
        }
        // SAFETY: the forwarding pointer was installed by the GC and
        // points at a valid object header within this heap.
        unsafe { ObjectRef::from_raw(addr) }
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
        // Task #25: this path fires the inherent write_barrier inside
        // `set_field`, which doesn't go through `VmHeap::write_barrier`
        // and would leave the triad sentinel armed. Clear it here so
        // the (pre, store-via-set_field) sequence closes cleanly.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        dispatch!(self, set_field(obj, index, value))
    }

    pub fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        dispatch!(self, get_field_volatile(obj, index))
    }

    pub fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        // Task #25: see `set_field` comment — also closes the triad
        // since `set_field_volatile` likewise routes through the
        // inherent write_barrier, bypassing `VmHeap::write_barrier`.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
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
    // GPU offload — safepoint coordination
    // =====================================================================

    /// Enter a GPU-critical section. Returns a `SafepointToken` whose
    /// lifetime brackets a no-GC window for the calling thread.
    ///
    /// The counter is process-wide (a module-level `AtomicU32`), so
    /// every `VmHeap` instance in the process shares the same gate.
    /// In practice every CratonVM process has exactly one `VmHeap`,
    /// so this is equivalent to per-heap.
    ///
    /// The GC entry points on both `GenerationalHeap` and
    /// `G1Collector` call [`wait_for_gpu_critical_drain`] before
    /// collecting, so a kernel running under this token will not
    /// observe its inputs being moved.
    #[cfg(feature = "gpu-offload")]
    pub fn enter_gpu_critical(&self) -> crate::safepoint::SafepointToken<'static> {
        crate::safepoint::SafepointToken::new(&GPU_CRITICAL_COUNT)
    }

    /// Current number of live `SafepointToken`s. Useful for tests.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_critical_count(&self) -> u32 {
        GPU_CRITICAL_COUNT.load(std::sync::atomic::Ordering::Acquire)
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
        // Task #25: closes the triad sentinel (same rationale as
        // `set_field`); ref-array stores also route through the
        // inherent write_barrier.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
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
        // Task #25 debug-triad assertion: if this thread recently fired
        // a `write_barrier_pre` (post-store sequence), the matching
        // `write_barrier` MUST follow within the same logical store —
        // the SATB / card-marking invariants depend on (pre, store,
        // post) being co-located. We clear the flag here so the next
        // pre-barrier starts a fresh triad. The flag is per-thread and
        // updated only under `debug_assertions`, so release builds pay
        // nothing.
        #[cfg(debug_assertions)]
        clear_pending_pre_barrier();
        match self {
            VmHeap::Generational(h) => h.write_barrier(obj, stored_value),
            VmHeap::G1(h) => h.write_barrier(obj, stored_value),
        }
    }

    /// Task #25: SATB pre-store barrier dispatched through the
    /// `GarbageCollector::write_barrier_pre` trait method. Call BEFORE
    /// overwriting a heap-managed reference slot. `old` is the value
    /// that will be lost; concurrent marking treats it as a root for
    /// the remainder of the cycle.
    ///
    /// `slot` is reserved for future debug triad-assertions and may be
    /// `null_mut()` if the caller only has the value (e.g. SATB log
    /// drain rebroadcasts).
    #[inline]
    pub fn write_barrier_pre(&self, slot: *mut ObjectRef, old: ObjectRef) {
        // Task #25: arm the per-thread triad sentinel. Cleared on the
        // matching `write_barrier`. Debug-only — release builds elide.
        #[cfg(debug_assertions)]
        arm_pending_pre_barrier();
        match self {
            VmHeap::Generational(h) => {
                <GenerationalHeap as GarbageCollector>::write_barrier_pre(h, slot, old)
            }
            VmHeap::G1(h) => {
                <G1Collector as GarbageCollector>::write_barrier_pre(h, slot, old)
            }
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

    /// Round-5 fix (CRIT — UAF): drain THIS thread's per-thread SATB
    /// buffer into the global SATB queue.
    ///
    /// Must be called on every mutator thread immediately before it
    /// blocks at a GC safepoint, and on the GC-initiating thread before
    /// it runs initial-mark / remark. Without this drain, up to
    /// `DEFAULT_SATB_CAPACITY` (256) overwritten references per thread
    /// remain in the thread-local buffer and never reach the marker —
    /// causing the classic SATB lost-object scenario (A→B replaced by
    /// A→null after A is scanned but before B is scanned), which the
    /// next evacuation turns into a use-after-free on B.
    ///
    /// The `thread_local!` storage means each thread must call this
    /// itself; the GC cannot reach into another thread's buffer.
    pub fn flush_thread_satb(&self) {
        match self {
            VmHeap::G1(h) => {
                if h.satb_queue().is_active() {
                    crate::satb::flush_thread_satb_buffer(h.satb_queue());
                }
            }
            VmHeap::Generational(h) => {
                if let Some(q) = h.satb_queue_handle() {
                    if q.is_active() {
                        crate::satb::flush_thread_satb_buffer(&q);
                    }
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
    pub fn g1_mark_roots(&self, roots: &[cratonvm_types::ObjectRef]) {
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

// ─── Phase 6 #1: GPU/GC coordination tests ───────────────────────────
//
// Verify that `enter_gpu_critical` increments/decrements the global
// `GPU_CRITICAL_COUNT` and that `wait_for_gpu_critical_drain` blocks
// while any token is alive.

#[cfg(all(test, feature = "gpu-offload"))]
mod gpu_coordination_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    #[test]
    fn token_lifecycle_drives_global_counter() {
        let heap = VmHeap::new(GcBackend::Generational, 1 << 20);
        let before = GPU_CRITICAL_COUNT.load(Ordering::Acquire);
        {
            let _t = heap.enter_gpu_critical();
            assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before + 1);
            assert_eq!(heap.gpu_critical_count(), before + 1);
            {
                let _t2 = heap.enter_gpu_critical();
                assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before + 2);
            }
            assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before + 1);
        }
        assert_eq!(GPU_CRITICAL_COUNT.load(Ordering::Acquire), before);
    }

    /// Hold a token on a worker thread for 200 ms; assert the
    /// drain on the main thread blocks at least 150 ms.
    #[test]
    fn drain_blocks_until_every_token_dropped() {
        let heap = std::sync::Arc::new(VmHeap::new(GcBackend::Generational, 1 << 20));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let heap2 = heap.clone();
        let holder = std::thread::spawn(move || {
            let _t = heap2.enter_gpu_critical();
            started_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        started_rx.recv().unwrap();
        let start = Instant::now();
        wait_for_gpu_critical_drain();
        let elapsed = start.elapsed();
        holder.join().unwrap();
        assert!(
            elapsed >= Duration::from_millis(150),
            "drain returned in {elapsed:?} but the token was held for 200ms",
        );
    }
}
